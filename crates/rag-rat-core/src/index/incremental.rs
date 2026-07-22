use rag_rat_base::hash::hex_sha256;
use rag_rat_base::time::now_ms;
use rag_rat_db::schema;

use super::*;

/// What an incremental file pass did, split by what each count gates downstream: `indexed`
/// (derived or deleted rows — real content change), `manifest_in_change_set` (a `Cargo.toml`
/// moved — refresh the package map), and `carried` (#502 — retained rows re-stamped into the
/// active scope; mutates the DB and re-keys the package scope, but is NOT a content change:
/// chunks, embeddings, and FTS entries all survive the re-stamp untouched).
struct IncrementalFilesOutcome {
    indexed: usize,
    manifest_in_change_set: bool,
    carried: usize,
    /// The batch's logical-grouping verdict (#820): whether the pass's tail may re-link members
    /// instead of paying the whole-repo `rebuild_logical_symbols`.
    logical: graph_index::LogicalGroupingUpkeep,
}

/// What the shared prepared-file WRITE step did: the rows that actually landed (per-file writes
/// plus tombstones — the caller's `mutated`/finalize gate input, unchanged), and the batch's
/// logical-grouping verdict (#820) accumulated while replacing files.
pub(super) struct IncrementalWriteOutcome {
    pub(super) written: usize,
    pub(super) logical: graph_index::LogicalGroupingUpkeep,
}

/// Everything the incremental/discover WRITE phase needs, computed entirely OUTSIDE the write
/// transaction (#560): the filesystem walk, `git_changed_paths`, the discovery snapshot, and the
/// tree-sitter parse all happen while producing this, so `BEGIN IMMEDIATE` then covers only the
/// SQLite writes. The read→write gap is made safe by holding the per-repo write flock across both
/// phases (see [`IndexDatabase::index_incremental_with_progress`]).
struct PreparedIncrementalPass {
    manifest_in_change_set: bool,
    /// `files.id`s to re-stamp into the active commit scope (#502). Empty in Changed mode.
    carried_ids: Vec<i64>,
    /// Parsed files ready to insert — the tree-sitter work is already done here, off the lock.
    prepared_files: Vec<PreparedIndexFile>,
    deleted: BTreeSet<PathBuf>,
}

impl IndexDatabase {
    pub fn index_changed(config: &Config) -> anyhow::Result<Self> {
        Self::index_changed_with_progress(config, |_| {})
    }

    pub fn index_changed_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        Self::index_incremental_with_progress(config, IndexMode::Changed, None, &mut progress)
            .map(|(db, _)| db)
    }

    /// Reconcile exactly the supplied candidate `paths` (#659) — the edit-driven-reindex substrate.
    /// Like [`Self::index_changed`], but the change set is the explicit list rather than a
    /// git-status walk (so it also sees committed changes), filtered through the same
    /// ignore/target rules and content-hash staleness; ignored / out-of-target / unchanged
    /// paths are no-ops, and a supplied path that no longer exists is tombstoned. Same per-repo
    /// `WriteLock` and #427 first-time-empty deferral as the other modes. Paths must be under
    /// `config.root`; a linked-worktree edit is routed to the overlay path by the caller (see
    /// the CLI `index --paths`).
    pub fn index_paths(config: &Config, paths: &[PathBuf]) -> anyhow::Result<Self> {
        Self::index_paths_with_progress(config, paths, |_| {})
    }

    pub fn index_paths_with_progress<F>(
        config: &Config,
        paths: &[PathBuf],
        mut progress: F,
    ) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        Self::index_incremental_with_progress(config, IndexMode::Paths, Some(paths), &mut progress)
            .map(|(db, _)| db)
    }

    pub fn index_discover(config: &Config) -> anyhow::Result<Self> {
        Self::index_discover_with_progress(config, |_| {})
    }

    pub fn index_discover_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        Self::index_incremental_with_progress(config, IndexMode::Discover, None, &mut progress)
            .map(|(db, _)| db)
    }

    /// Like [`Self::index_discover`], but also reports whether the pass changed index *content*
    /// (a file was added / edited / removed). The watch loop uses this to skip the
    /// reconcile / memory-validate tail on an idle no-change sweep (issue #63).
    pub fn index_discover_reporting(config: &Config) -> anyhow::Result<(Self, bool)> {
        Self::index_incremental_with_progress(config, IndexMode::Discover, None, &mut |_| {})
    }

    fn index_incremental_with_progress<F>(
        config: &Config,
        mode: IndexMode,
        explicit_paths: Option<&[PathBuf]>,
        progress: &mut F,
    ) -> anyhow::Result<(Self, bool)>
    where
        F: FnMut(IndexProgress),
    {
        // An absent / schemaless DB is bootstrapped by a FULL rebuild for the sweep-style modes
        // (Changed / Discover) — but NOT for Paths (#659). `Paths` is a scoped reconcile whose
        // whole contract is "touch exactly the supplied paths"; a full rebuild would index
        // the entire repository instead, so a scoped pass on a not-yet-initialized index
        // DEFERS (its caller — the CLI / an edit hook — surfaces `EmptyIndexRefused` as
        // "run `rag-rat index` first"), exactly like the #427 first-time-empty deferral.
        let uninitialized = !config.database.exists()
            || Self::migration_check(&config.database)?.state == schema::SchemaState::Missing;
        if uninitialized {
            if mode == IndexMode::Paths {
                return Err(crate::index::EmptyIndexRefused {
                    root: config.root.display().to_string(),
                }
                .into());
            }
            return Self::rebuild_with_progress(config, progress).map(|db| (db, true));
        }

        // Acquire the per-repo write flock BEFORE opening + scoping the connection (#560/#561). It
        // MUST precede `set_context` and every live-generation read below: if a sibling
        // `rebuild_with_progress` holds this flock, blocking for it AFTER `set_context` pinned
        // `active_generation` would resume this pass on the pre-flip (now-dead) generation and land
        // every delete/carry/insert there — a silent no-op that still returns success. Acquiring
        // first means `open_bare` / `set_context` / the count reads all observe the post-rebuild
        // live generation, and no flock-respecting writer can flip it out from under this pass.
        // It is also the exclusion the hoisted reads rely on: the filesystem walk / git status /
        // parse / git-history join below moved OUT of `BEGIN IMMEDIATE` (freeing the SQLite writer
        // slot so a cross-repo writer on a consolidated DB can slip a short write in), so this
        // flock — not the SQLite lock — is now what keeps a sibling rebuild/discover from
        // racing this pass's generation. The same flock every rebuild ENTRY holds
        // (`rebuild.rs`); reentrant, so watcher/maintenance callers that already hold it
        // just depth-increment and a CLI one-shot `index` acquires it fresh. GLOBAL-LOCK
        // ORDERING RULE holds: per-repo taken before the global schema lock `open_bare` may
        // take (per-repo → global), same as every rebuild entry.
        let lock_repo = rag_rat_base::locks::write_lock_repo_id(config);
        let _write_lock =
            rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;

        // Open in ADOPTION-PENDING mode: the multi-repo fail-fast must not fire (the config below
        // supplies the scope, unlike a genuinely config-less `Self::open`), and the graph check is
        // DEFERRED — its `ensure_graph_index_current` is a repo-scoped edge DELETE+rebuild keyed
        // on `active_repo_id`, which pre-adoption would resolve the config-blind SOLE-repo pick
        // and refresh/wipe the WRONG repo's graph (or skip the config repo's own stale graph).
        // Adopt + install the scope context FIRST, then run the graph/generated-flags heal for the
        // correct repo — the exact sequence `open_config` uses.
        let mut db =
            Self::open_bare(&config.database, super::lifecycle::BareOpenMode::AdoptionPending)?;
        // #427 — enforce the empty-index invariant HERE, BEFORE `adopt_repo_from_config` records
        // this checkout's root. Enforcing after adoption would defeat the check: adoption registers
        // the repo + records the root, so a post-adoption "was this already indexed?" test would
        // see the just-recorded root and wave an empty first-time registration through (the
        // exact seam the earlier rebuild-only guard missed on the incremental/discover
        // path). `open_bare` already migrated the schema, so this reads current
        // `source_root` on the live connection: an ESTABLISHED repo whose files were just
        // deleted still counts as indexed (its persisted `source_root` matches) and is
        // allowed through to prune, while a genuinely first-time-empty checkout is refused
        // before anything is registered. Callers react to the error: the watcher / git-hook
        // `maintenance` `let _ =`-discard it and wait for content; the one-shot
        // `index` surfaces it. `--allow-empty` (`config.allow_empty`) opts in.
        if !config.allow_empty
            && crate::index::is_first_time_empty_conn(db.storage.connection(), config)?
        {
            return Err(crate::index::EmptyIndexRefused {
                root: config.root.display().to_string(),
            }
            .into());
        }
        // Register/adopt the config's repo BEFORE `set_context` stamps `active_repo_id` into the
        // scope view (A3). `Self::open` scoped to the SOLE repo via the config-blind fallback; on a
        // consolidated DB that would pick the lexicographically-first repo, so this incremental
        // pass could delete/stamp rows under the WRONG repo. Adopting here — the same step
        // `open_config` and `rebuild` run — resolves the config's own identity and points
        // every repo-scoped write below at it. Idempotent on an already-adopted single-repo
        // DB (the common case). INDEXING intent: an incremental/discover pass records this
        // checkout's root (#427).
        db.adopt_repo_from_config(config, super::lifecycle::AdoptIntent::Indexing)?;
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        // ADOPTION RESETS ALL CONNECTION-CARRIED REPO-DERIVED STATE BEFORE ANY DEFERRED HEAL (the
        // rule that closes the pre-adoption-pick family). `open_bare` derived per-repo state from
        // the config-less SOLE pick — on a consolidated DB, a first-sorting SIBLING — and each
        // piece must be re-derived for the adopted repo before a heal consumes it:
        //  * `active_repo_id` / scope view / `active_generation` — re-derived by
        //    `adopt_repo_from_config` + `set_context` above;
        //  * `source_root` — reset HERE from the config: `ensure_graph_index_current` re-reads
        //    changed files from `source_root.join(path)` while stamping the ADOPTED repo, so a
        //    stale sibling root would refresh the target's graph from the WRONG CHECKOUT;
        //  * the model-manifest heal — deferred below, so its `repo_meta` reads/writes resolve the
        //    adopted repo, not the pick.
        // (`active_commit_sha` / `active_worktree_id` were set by `set_context`; the GitHub
        // context and `_identity_lock` are not repo-pick-derived.)
        db.storage.set_source_root(config.root.clone());
        // The DEFERRED model-manifest heal (open_bare skips it in AdoptionPending mode): now that
        // the scope context names the CONFIG's repo, a heal-owed pass reads/clears the RIGHT
        // repo's `repo_meta` active-model keys under the lock this command actually holds —
        // pre-adoption it resolved the sole-repo pick, mutating a first-sorting SIBLING's meta on
        // a consolidated DB. The `open_config` ordering, mirrored.
        ai::ensure_model_manifest(db.storage.connection())?;
        // Now that the config's repo is adopted and its scope is installed, run the deferred graph
        // + generated-flags heal against the RIGHT repo (the graph check's edge DELETE is
        // scoped by `active_repo_id`, so it must not run under the pre-adoption sole-repo
        // fallback).
        db.ensure_graph_index_current()?;
        db.ensure_generated_flags_current()?;
        let scoped_file_count = db.indexed_file_count()?;
        let active_base_scope_discovered = db.active_base_scope_discovered(&config.targets)?;
        let repo_generation_file_count =
            db.repo_generation_file_count(!active_base_scope_discovered)?;
        if repo_generation_file_count == 0 && !active_base_scope_discovered {
            // A repo registered in a SHARED/global DB but not yet indexed reaches here with zero
            // rows: another repo created the DB file (so the missing-DB/schema guard above passed)
            // and this repo has files on disk (so the #427 first-time-empty check let it through).
            // For the sweep modes that means "bootstrap it" — a full rebuild. But `Paths` must
            // NEVER rebuild the WHOLE repository for a command like `index --paths
            // src/a.rs`; it DEFERS exactly like the missing-DB case above, so the
            // caller surfaces "run `rag-rat index` first" (#659 review).
            if mode == IndexMode::Paths {
                return Err(crate::index::EmptyIndexRefused {
                    root: config.root.display().to_string(),
                }
                .into());
            }
            return Self::rebuild_with_progress(config, progress).map(|db| (db, true));
        }
        // A commit advances HEAD before committed-scope rows have been stamped for it. Likewise, a
        // target-set change can make the active scope marker stale even when the old rows still
        // make the counts match. Neither is a complete changed-file pass: discover the tree
        // and restamp the generation for the active HEAD/target fingerprint (#459 review).
        let active_scope_incomplete =
            !active_base_scope_discovered || scoped_file_count < repo_generation_file_count;
        // Both the git-status `Changed` mode AND the explicit-path `Paths` mode (#659) index only a
        // subset; when the active base scope is INCOMPLETE — a commit advanced HEAD but the
        // existing committed rows are still keyed to the old sha, or a target-set change
        // staled the marker — that subset alone would leave every UNCHANGED file absent
        // from the new scope, so queries would suddenly lose most of the repository.
        // Promote to `Discover` (which restamps/carries the committed rows onto the current
        // HEAD/target fingerprint, #459) to complete the scope; the named paths are a
        // subset discovery already covers. On a COMPLETE scope, `Paths` keeps its scoped
        // behavior.
        let effective_mode =
            if active_scope_incomplete && matches!(mode, IndexMode::Changed | IndexMode::Paths) {
                IndexMode::Discover
            } else {
                mode
            };
        progress(IndexProgress::Started {
            database: config.database.clone(),
            mode: effective_mode,
        });
        // Gate + spawn the git-history reload BEFORE `BEGIN IMMEDIATE` (unchanged gate). Unchanged
        // HEAD/root/shallow skips it entirely; a fast-forward HEAD prepares only the new range
        // (`old..new`); uncertainty, shallow history, root drift, and non-fast-forward rewrites
        // prepare the full history. The prepared append plan is revalidated at APPLY time (inside
        // the terminal txn, via `apply_prepared`), so preparing — and now joining — off the SQLite
        // lock is safe.
        let git_history_handle = if db.git_history_is_current(&config.root) {
            None
        } else {
            progress(IndexProgress::IndexingGitHistory);
            let plan = git_history::prepare_plan(db.storage.connection(), &config.root);
            Some(spawn_git_history_prepare_with_plan(&config.root, plan))
        };

        // PREPARE — OUTSIDE the SQLite write transaction (#560): filesystem walk,
        // `git_changed_paths`, discovery snapshot (a pure DB read), tree-sitter parse. This
        // is the work the audit found holding the writer lock for time proportional to repo
        // size (unbounded in Discover mode); hoisting it is the entire point of the patch.
        let prepare_started = std::time::Instant::now();
        let prepared =
            match db.prepare_incremental_pass(config, effective_mode, explicit_paths, progress) {
                Ok(prepared) => prepared,
                Err(err) => {
                    // Prepare failed off the lock (e.g. an unreadable changed file). JOIN the
                    // pending git-history worker before bailing, so its
                    // (possibly full `git log`) scan does not detach and keep
                    // burning CPU/IO after the failed pass — matching the pre-hoist
                    // error path, which joined the handle before returning.
                    if let Some(handle) = git_history_handle {
                        let _ = join_git_history_prepare(handle);
                    }
                    return Err(err);
                },
            };
        // Join the git-history prepare thread OUTSIDE the write lock too — a thread `join()` must
        // not sit inside `BEGIN IMMEDIATE`. The apply below still revalidates the append plan under
        // the lock, preserving the git-history freshness invariant.
        let prepared_git_history = match git_history_handle {
            Some(handle) => Some(join_git_history_prepare(handle)?),
            None => None,
        };
        let prepare_ms = prepare_started.elapsed().as_millis() as u64;

        // WRITE — ONE `BEGIN IMMEDIATE` .. COMMIT (#560). The incremental path writes the LIVE
        // generation directly (no staging, no pointer flip — unlike the full rebuild), so every
        // write must land ATOMICALLY: a reader sees the pre-pass or the post-pass state, never a
        // half-applied change set. That is why these writes are NOT split into per-wave commits the
        // way the staged rebuild's are — a rebuild wave is invisible until the flip, but a
        // live-generation wave commit would publish a partially-updated generation. Every effect
        // here is publication authority and stays in this one transaction; the expensive reads are
        // already hoisted above, so the writer lock now covers only DB mutation.
        let write_started = std::time::Instant::now();
        let result = (|| -> anyhow::Result<(bool, usize, usize, usize)> {
            // BEGIN IMMEDIATE: take the write lock up front so a racing writer waits out
            // busy_timeout instead of failing a deferred read→write upgrade with SQLITE_BUSY.
            db.storage.execute_batch("BEGIN IMMEDIATE")?;
            // Write meta only when it actually changed, and track whether this pass mutated
            // anything at all. A periodic sweep or a spurious event over an unchanged
            // tree must NOT churn the WAL with a timestamp-only write + COMMIT (issue
            // #63) — that idle write is also the false signal the watcher-loop
            // diagnostic keys on (indexed_at_ms advancing while content is unchanged).
            let source_root_changed =
                db.set_repo_meta_if_changed("source_root", &config.root.display().to_string())?;
            db.storage.set_source_root(config.root.clone());
            let git_meta_changed = db.write_git_meta(&config.root)?;
            // Heal the active embedding model from config INSIDE the txn (#394): a failed pass
            // rolls the reseed back with everything else rather than stranding the
            // active model on a possibly-uninstalled configured model. A no-op unless a
            // seed is owed (preserving the #63 idle-pass no-write invariant); when owed
            // it counts as a mutation so COMMIT persists it.
            let embedding_model_seeded = ai::active_embedding_model_seed_owed(
                db.storage.connection(),
                config.llm.embedding.backend.model_id(),
            )?;
            if embedding_model_seeded {
                ai::seed_active_embedding_model(
                    db.storage.connection(),
                    config.llm.embedding.backend.model_id(),
                )?;
            }
            // Apply the prepared plan — WRITES ONLY (the parse already happened during prepare).
            // Freshness is revalidated cheaply here at apply time, not re-derived: the #502 carry
            // re-stamp pins `repo_id`/`generation`/scope in its UPDATE `WHERE`, so a candidate that
            // went stale between prepare and now updates nothing rather than the wrong row; the
            // per-file writes are keyed by (path, commit_sha, worktree_id) scope, so they are
            // idempotent against any intervening lockless overlay heal; and the flock has excluded
            // the only writer that could flip `active_generation`.
            let IncrementalFilesOutcome { indexed, manifest_in_change_set, carried, logical } =
                db.apply_prepared_incremental_pass(&prepared, effective_mode, progress)?;
            let base_scope_discovery_marked = if effective_mode == IndexMode::Discover {
                db.mark_active_base_scope_discovered(&config.targets)?
            } else {
                false
            };
            // Self-heal stale worktree-overlay rows (#87). The heal walks git status IN-TXN before
            // any destructive decision, so a path dirtied/restored/deleted during the off-lock
            // window or the BEGIN wait cannot be mis-healed (#561). Read-only +
            // walk-free when there are no overlay candidates (#63).
            let healed = db.heal_stale_overlay_rows(&config.root)?;
            let mut mutated = indexed > 0
                || healed > 0
                || carried > 0
                || source_root_changed
                || git_meta_changed
                || embedding_model_seeded
                || base_scope_discovery_marked;
            // None when the gate found git history already current. The handle was JOINED outside
            // the lock; `apply_prepared` revalidates the append plan here, under the lock, so a
            // history rewrite between prepare and now is caught (git-history freshness invariant).
            if let Some(prepared_history) = prepared_git_history {
                db.apply_joined_git_history(&config.root, prepared_history)?;
                mutated = true;
            }
            // Per-package import scope (#61, salvaging #95): rewrite `packages` + refresh the
            // global `local_crate_roots` union BEFORE the resolve pass. Runs OUTSIDE
            // the `indexed>0 || healed>0` gate so a manifest-only change (indexed==0)
            // still refreshes; `carried > 0` also refreshes (#502) since the package
            // map is keyed by `(commit_sha, worktree_id)`. `refresh_packages` returns
            // whether the map changed, forcing a re-resolve.
            let roots_changed =
                if indexed > 0 || healed > 0 || carried > 0 || manifest_in_change_set {
                    db.refresh_packages(&config.root)?
                } else {
                    false
                };
            if roots_changed {
                mutated = true;
            }
            // Healing can delete overlay symbols (NULLing in-edges via `remove_file_in_scope`), so
            // it needs the same re-derive tail as real file changes; a manifest-only
            // change re-resolves so `use new_crate::X` binds (#95); a carried scope
            // re-resolves so a carried caller's edge re-points at re-derived rowids
            // (#502).
            if indexed > 0 || healed > 0 || carried > 0 || roots_changed {
                // #820: a batch whose EVERY change was a key-stable file replacement keeps the
                // grouped table correct by re-linking members inside this same transaction —
                // the wholesale rebuild is owed only when a key set changed, or when the pass
                // mutated grouping-relevant state OUTSIDE the per-file plan (an overlay heal
                // moves symbols across scopes; a carry re-stamps scope rows; a package-map
                // change keeps today's rebuild coupling). A pre-existing #819 obligation is
                // untouched either way — the pass's tail settle below still consumes it.
                let key_stable_relinks = match logical {
                    graph_index::LogicalGroupingUpkeep::RelinkMembers(relinks)
                        if healed == 0 && carried == 0 && !roots_changed =>
                        Some(relinks),
                    _ => None,
                };
                match key_stable_relinks {
                    Some(relinks) => db.apply_logical_member_relinks(&relinks)?,
                    None => {
                        progress(IndexProgress::RebuildingLogicalSymbols);
                        // Defer: this pass re-parsed only the CHANGED files, so it must not
                        // stamp the logical-key version — untouched files' drift is still in
                        // the future (#493).
                        db.rebuild_logical_symbols(graph_index::KeyVersionStamp::Defer)?;
                    },
                }
                progress(IndexProgress::ResolvingGraph);
                db.resolve_edges()?;
                db.mark_graph_index_current()?;
                progress(IndexProgress::SyncingFts);
                db.sync_fts()?;
            }
            if mutated {
                db.set_repo_meta("indexed_at_ms", &now_ms().to_string())?;
                db.storage.execute_batch("COMMIT")?;
            } else {
                // Nothing changed since the last pass — close the (empty) transaction without
                // writing, so an idle server does not touch the DB (#63).
                db.storage.execute_batch("ROLLBACK")?;
            }
            progress(IndexProgress::Finished { files: indexed });
            // Report whether index *content* changed (files added / edited / removed, or stale
            // overlays healed — symbols move scope), so the watch loop can skip the reconcile /
            // memory-validate tail on an idle sweep.
            Ok((indexed > 0 || healed > 0, indexed, carried, healed))
        })();
        if result.is_err() {
            let _ = db.storage.execute_batch("ROLLBACK");
        }
        let (content_changed, indexed, carried, healed) = result?;
        // #560 measurement: prepare (reads/parse) vs write (BEGIN IMMEDIATE..COMMIT) durations +
        // row counts, so the write-lock hold time is observable independently of the hoisted reads.
        tracing::debug!(
            target: "rag_rat::index::incremental",
            mode = ?effective_mode,
            prepare_ms,
            write_ms = write_started.elapsed().as_millis() as u64,
            files = indexed,
            carried,
            healed,
            "incremental pass (reads hoisted out of the write transaction)"
        );
        // Settle any pending overlay-batch logical rebuild before returning (#819 review): an
        // interrupted Deferred overlay batch leaves its obligation committed, and this pass's
        // own rebuild is gated on its row changes — an IDLE pass closes its empty transaction
        // above and would otherwise exit past the marker, leaving branch-only symbols
        // unresolvable until an unrelated pass rebuilt. Its own `BEGIN IMMEDIATE` (the pass's
        // transaction is closed by now); write-free when nothing is pending (one meta read),
        // so the #63 idle-pass posture holds. On failure the marker survives for the next pass.
        db.apply_pending_logical_rebuild()?;
        Ok((db, content_changed))
    }

    /// Standalone full-corpus indexing into the CURRENT context — no generation staging, no
    /// pointer flip: the connection's `active_generation` is written directly, so this is only
    /// correct on a scope+generation with no pre-existing file rows (a fresh index). The wave
    /// loop shares the full rebuild's temp-table staging (`graph.is_some()` routing), so this
    /// finalize must publish everything the rebuild's terminal transaction would (batch 7 P2:
    /// staged parser failures were silently dropped here — a standalone pass's newly failing
    /// files vanished from `parser_failures`, and coverage/repo-brief underreported, until some
    /// later full rebuild happened to publish the staging). Unlike the rebuild there is no flip
    /// to defer to: this path operates on the LIVE generation, so its failure state is not
    /// staged-generation authority — it publishes atomically with this pass's own edges.
    pub fn index_targets(&self, config: &Config) -> anyhow::Result<()> {
        let (_, graph) = self.index_targets_with_progress(config, &mut |_| {})?;
        // The standalone twin of the rebuild's Phase-2 + terminal tail, in ONE short transaction
        // (batch 6 moved base edges + package roots out of the wave loop; batch 7 completed the
        // twin), mirroring the rebuild's order. Step by step against `rebuild_with_progress`:
        // - build_chunk_text_store: first-index dict training — `insert_chunks` staged the text
        //   into `temp.rebuild_chunk_text` when no dict existed; a no-op once a dict exists.
        // - finalize_base_edges: base package roots + accumulated-edge resolution (batch 6).
        // - rebuild_logical_symbols: the open-time graph heal re-derives EDGES only, so without
        //   this fold the standalone pass's symbols stay invisible to symbol_lookup/graph nav (the
        //   `finalize_overlay_refresh` precedent — every finalize that writes symbols folds).
        // - apply_staged_parser_failures: THE batch-7 finding — the wave loop stages failures
        //   (`graph.is_some()` routing), so the finalize must publish them; at this connection's
        //   own (live) generation, atomic with its edges (no flip exists to defer to).
        // - refresh_clone_token_df: recompute the LIVE df exactly (the wave ran with
        //   `BumpDf(false)` — this pass, like the full rebuild, recomputes at finalize instead of
        //   paying per-token upserts). Restored to the pre-#473 unconditional refresh by #479: the
        //   persisted clone-graph postings are ordered by their own generation's `clone_df_epoch`,
        //   so a live refresh no longer desyncs (or invalidates) anything.
        // - sync_fts + the graph/flags marks: chunk_fts was written inline and the edges/flags were
        //   just derived in full, so record freshness like the rebuild does — otherwise the very
        //   next open pays a full (safe but wasted) edge re-derive heal and an FTS rebuild.
        // NOT here, deliberately: the generation carry-forwards, overlay re-resolution, git
        // history/meta/cursors, source_root, the live-generation pointer, and the model seed are
        // PUBLISH authority — a standalone pass writes the live generation in place and owns no
        // flip, no git authority, and no checkout move.
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<()> {
            self.build_chunk_text_store()?;
            self.finalize_base_edges(config, graph)?;
            // FullRederive: the standalone pass indexed the whole corpus (a fresh index — no
            // pre-existing rows, so the drift heal is empty), so it may stamp the logical-key
            // version (#493).
            self.rebuild_logical_symbols(graph_index::KeyVersionStamp::FullRederive)?;
            self.apply_staged_parser_failures(self.active_generation)?;
            self.refresh_clone_token_df()?;
            self.sync_fts()?;
            self.mark_graph_index_current()?;
            self.mark_generated_flags_current()
        })();
        if result.is_err() {
            let _ = self.storage.execute_batch("ROLLBACK");
            return result;
        }
        self.storage.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Write the ACTIVE (base) scope's `packages` rows + the global `local_crate_roots` union, then
    /// resolve and insert every accumulated base edge against those roots (the full-rebuild fast
    /// path — [`edges::resolve_and_insert_edges`] computes each file's package from the
    /// just-written rows at load time, so the rows MUST land first). NO transaction of its own
    /// — the caller wraps it. `rebuild` runs this INSIDE its terminal publish transaction so
    /// the generation-less `packages`/`local_crate_roots` writes are invisible to a concurrent
    /// reader/heal until the pointer flips and roll back with a failed tail (batch 6 P2, #4);
    /// the standalone `index_targets` runs it in a short transaction of its own.
    pub(super) fn finalize_base_edges(
        &self,
        config: &Config,
        graph: edges::FullRebuildGraph,
    ) -> anyhow::Result<()> {
        self.refresh_packages(&config.root)?;
        edges::resolve_and_insert_edges(self.storage.connection(), graph)
    }

    pub(super) fn index_targets_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<(usize, edges::FullRebuildGraph)>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        let files = collect_index_files(config)?;
        let changes = git_changed_paths(&config.root).unwrap_or_default();
        let files = self.assign_file_scopes(files, &changes);
        progress(IndexProgress::Discovered { files: files.len() });

        // Process files in WAVES: prepare a wave in parallel, insert it, then drop the wave's
        // prepared output before preparing the next. The previous fan-in barrier materialized the
        // prepared form (every chunk/symbol/edge-candidate/anchor) of ALL files at once — ~19 GB
        // for the Linux kernel. Waves cap peak memory at one wave of prepared files + the
        // accumulating symbol/edge graph (which is needed until resolution but is compact).
        // Files stay in path order, so symbol/chunk/edge ids are assigned in exactly the
        // same order — byte-identical output, no id remapping. Wave size is tunable via
        // RAG_RAT_INDEX_WAVE.
        let total = files.len();
        let wave_size = index_wave_size();
        let mut graph = edges::FullRebuildGraph::default();
        let mut done = 0usize;
        // Per-connection staging for the generation-less `parser_failures` mutations this pass
        // produces (A6, P2 review): the waves below commit BEFORE the flip, so upserting/clearing
        // the real table here would expose an unpublished generation's failure state. Insert
        // routes stage into this table; `apply_staged_parser_failures` publishes it — inside the
        // rebuild's terminal flip transaction, or the standalone `index_targets`' own finalize
        // transaction (batch 7). `message` NULL = clean parse (clear at publish).
        self.storage.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS rebuild_parser_failures(
                 path TEXT PRIMARY KEY,
                 language TEXT NOT NULL,
                 message TEXT
             );
             DELETE FROM temp.rebuild_parser_failures;",
        )?;
        // The first-index chunk-text staging table lives with the wave loop that writes it
        // (batch 7): BOTH entries — the full rebuild and the standalone `index_targets` — run
        // waves whose `insert_chunks` stages text here when no dict exists yet, so the creation
        // belongs to the shared loop, not the rebuild alone (the standalone path used to error
        // "no such table" on a fresh, dict-less index).
        self.prepare_rebuild_scratch_tables()?;
        for wave in files.chunks(wave_size) {
            let prepared = prepare_files_with_progress(wave, progress, done, total)?;
            // A6: commit each wave in its OWN short transaction. A full rebuild stages a fresh file
            // GENERATION (`self.active_generation`, set to N+1 by the rebuild's `set_context`)
            // ALONGSIDE the still-live one, so it must NOT hold the shared DB's write lock across
            // the whole file set — a sibling repo's short write (a memory create) has
            // to be able to slip in between waves within the busy-timeout slice (spec
            // §3.3). Readers stay on the live generation until the flip; a wave that
            // fails rolls back just its own batch, and a torn
            // rebuild's committed-but-never-flipped generation is swept lazily by gc.
            //
            // INTERLEAVED SAME-REPO WRITERS ARE SAFE HERE (A6, P2 review — the proof, not a
            // hand-wave). The per-repo advisory flock covers the CLI/watcher writers, but NOT
            // every write path (MCP `memory_create`/`memory_update` and the read-path heals go
            // through `open_config` locklessly), so a same-repo write CAN land between wave
            // commits. The proof splits by what the writer touches:
            // - DISJOINT-TABLE writers — this proof's own scope (regression test
            //   `a_memory_written_mid_rebuild_survives_the_flip_intact`): MEMORY writes touch only
            //   `repo_memories`/bindings/tags/fts, none of the rebuild's tables. A binding captured
            //   against the LIVE generation's symbol/chunk ROWIDS goes stale when the flip + gc
            //   retire those rows — the ORDINARY reindex lifecycle those bindings already live with
            //   (rowids re-mint on every incremental pass too): `memory_validate` re-anchors by
            //   content hash / the generation-stable `logical_symbol_id`. (The pre-A6
            //   mega-transaction did not make such a write safe — it made it FAIL with SQLITE_BUSY
            //   after the busy_timeout; landing it and re-anchoring is the designed behavior, not a
            //   regression.)
            // - FILES-ADJACENT writers (heals, incremental replacement, gc) are NOT covered by
            //   disjointness; they get the same GENERATION DISCIPLINE the rebuild uses. Their
            //   deletes carry the writer's own generation (`remove_file_in_scope` — a
            //   live-generation heal can never remove a staged row for the same scope key); gc's
            //   deadness predicate is `generation != live` UNDER THE PER-REPO WRITE FLOCK (batch 5:
            //   a collector holding the flock knows no rebuild is mid-flight, so an above-live
            //   staging is abandoned, not in-progress — the flockless-safe `< live` form is the
            //   fallback; see `gc.rs`, plus the CLI `gc` flock belt). Every rebuild ENTRY acquires
            //   that flock (`rebuild_with_progress`, batch 6), so the precondition holds by
            //   construction. And every reader resolves `files` through a repo+generation view
            //   (`set_context` on config opens, `write_repo_generation_view` on bare opens), so the
            //   staged generation is invisible until the flip. Writes they DO make land at the live
            //   generation and die with it at gc after the flip (wasted work, not corruption);
            //   `target` is allocated strictly above both the row-MAX and the live pointer, so
            //   staged keys never collide.
            self.storage.execute_batch("BEGIN IMMEDIATE")?;
            let wave_result: anyhow::Result<()> = (|| {
                for prepared_file in &prepared {
                    done += 1;
                    if should_report_file_progress(done, total) {
                        progress(IndexProgress::IndexingFile {
                            current: done,
                            total,
                            path: prepared_file.file.relative_path.clone(),
                            language: prepared_file.file.language,
                            kind: prepared_file.file.kind,
                        });
                    }
                    // chunk_fts is written inline from the in-memory chunk text (#77 Phase 2): it's
                    // contentless now, so there is no content table for a closing 'rebuild' to
                    // re-read, and the in-memory text is available regardless of whether the
                    // chunks.text column still exists. `finalize_full_rebuild_fts` then only
                    // rebuilds commit_fts.
                    self.insert_prepared_file(prepared_file, Some(&mut graph))?;
                }
                Ok(())
            })();
            if let Err(err) = wave_result {
                let _ = self.storage.execute_batch("ROLLBACK");
                return Err(err);
            }
            self.storage.execute_batch("COMMIT")?;
            // Test seam: a barrier between committed waves lets a concurrent reader observe the
            // staged (but not-yet-live) generation. Keyed by THIS connection's database path so
            // parallel same-process tests (libtest / coverage) never trip each other's barrier.
            // No-op in production.
            #[cfg(test)]
            crate::index::rebuild::run_after_wave_commit(self.database_path());
            // `prepared` (this wave's chunk texts / symbols / edge candidates) drops here.
        }
        // Base-edge resolution + package roots NO LONGER run here (batch 6 P2, #4). The full
        // rebuild carries the accumulated `graph` out to its TERMINAL publish transaction
        // (`finalize_base_edges`), where `refresh_packages` writes the generation-less
        // `packages`/`local_crate_roots` and the resolve inserts the base edges — all atomic with
        // the pointer flip, so a concurrent reader/heal on the old generation never sees the new
        // package map, and a failed tail rolls the whole lot back. The waves above committed only
        // the staged-generation file/chunk/symbol rows (inert until the flip).
        Ok((total, graph))
    }

    /// PREPARE phase (#560): everything an incremental/discover pass can compute WITHOUT the SQLite
    /// write lock — the git status walk, the discovery snapshot (a pure DB read), scope assignment,
    /// and the tree-sitter parse. Returns a [`PreparedIncrementalPass`] the caller applies inside
    /// its terminal `BEGIN IMMEDIATE`. NO writes happen here: the #502 carry re-stamp is a write,
    /// so it is only SELECTED here (`plan.carried`) and applied later. Runs under the per-repo
    /// write flock, so the snapshot it reads cannot be flipped by a sibling rebuild/discover
    /// before it is applied.
    fn prepare_incremental_pass<F>(
        &self,
        config: &Config,
        mode: IndexMode,
        explicit_paths: Option<&[PathBuf]>,
        progress: &mut F,
    ) -> anyhow::Result<PreparedIncrementalPass>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        match mode {
            // Changed mode never carries: it only sees working-tree dirt, and a stale base-scope
            // marker promotes a HEAD move to Discover (#459), where the carry lives. Its change set
            // is the git-status walk; `Paths` mode reuses the SAME preparation over an explicit
            // caller-supplied list instead (#659), and likewise never carries.
            IndexMode::Changed => {
                let changes = git_changed_paths(&config.root)?;
                // `changes.changed` IS the full dirty set (git status), so a dirty `Cargo.toml` is
                // already in it; `files` ⊆ `changes.changed`, so this is complete for Changed.
                let manifest_in_change_set =
                    paths_include_cargo_toml(changes.changed.iter().map(PathBuf::as_path))
                        || paths_include_cargo_toml(changes.deleted.iter().map(PathBuf::as_path));
                let files = collect_changed_index_files(config, &changes)?;
                self.prepare_from_files_and_changes(
                    files,
                    changes,
                    manifest_in_change_set,
                    progress,
                )
            },
            IndexMode::Paths => {
                // The builder reports whether a supplied path is a `Cargo.toml` — a non-target file
                // dropped from `files`, but still a package-map refresh signal even when CLEAN /
                // committed (this mode reconciles committed changes) (#659 review).
                let (files, changes, manifest_in_change_set) =
                    explicit_index_files_and_changes(self, config, explicit_paths.unwrap_or(&[]))?;
                self.prepare_from_files_and_changes(
                    files,
                    changes,
                    manifest_in_change_set,
                    progress,
                )
            },
            // The manifest flag also consults the discovery plan's file list, so a NEW (untracked,
            // not-yet-committed) `Cargo.toml` is caught even though git status would not list it as
            // changed (#61, salvaging #95).
            IndexMode::Discover => {
                // The status walk feeds BOTH the plan's carry filter (a dirty/untracked path must
                // never be carried into the committed scope) and the scope assignment below.
                // Discover tolerates a status error (falls back to an empty set): the carry's real
                // guard is its exact-sha match, not this belt. The overlay heal uses its own fresh
                // walk (taken later, in the caller) so it is not affected by this snapshot.
                let changes = git_changed_paths(&config.root).unwrap_or_default();
                let plan = discovery_plan(self.storage.connection(), config, &changes)?;
                let manifest_in_change_set =
                    paths_include_cargo_toml(changes.changed.iter().map(PathBuf::as_path))
                        || paths_include_cargo_toml(changes.deleted.iter().map(PathBuf::as_path))
                        || paths_include_cargo_toml(
                            plan.files.iter().map(|file| file.relative_path.as_path()),
                        );
                let carried_ids = plan.carried;
                let deleted = plan.deleted;
                let files = self.assign_file_scopes(plan.files, &changes);
                progress(IndexProgress::Discovered { files: files.len() });
                let prepared_files = prepare_files_with_progress(&files, progress, 0, files.len())?;
                Ok(PreparedIncrementalPass {
                    manifest_in_change_set,
                    carried_ids,
                    prepared_files,
                    deleted,
                })
            },
            IndexMode::Full => unreachable!("full mode is handled by rebuild_with_progress"),
        }
    }

    /// Prepare a no-carry incremental pass from an already-collected `files` list and its
    /// dirty/deleted `changes` — shared by `Changed` (git-status walk → collect) and `Paths`
    /// (explicit list) (#659). `changes.changed` classifies which of `files` are working-tree DIRT
    /// for [`Self::assign_file_scopes`]; a file present in `files` but ABSENT from
    /// `changes.changed` is committed-scoped (how `Paths` keeps a clean/committed supplied file
    /// out of an overlay row). Content-hash staleness is decided later (an unchanged file
    /// prepares but writes nothing); `deleted` paths are tombstoned at apply time.
    fn prepare_from_files_and_changes<F>(
        &self,
        files: Vec<IndexFile>,
        changes: GitChangedPaths,
        manifest_in_change_set: bool,
        progress: &mut F,
    ) -> anyhow::Result<PreparedIncrementalPass>
    where
        F: FnMut(IndexProgress),
    {
        let files = self.assign_file_scopes(files, &changes);
        let deleted = changes.deleted.clone();
        progress(IndexProgress::Discovered { files: files.len() });
        let prepared_files = prepare_files_with_progress(&files, progress, 0, files.len())?;
        Ok(PreparedIncrementalPass {
            manifest_in_change_set,
            carried_ids: Vec::new(),
            prepared_files,
            deleted,
        })
    }

    /// APPLY phase (#560): the WRITE half of an incremental/discover pass, run inside the caller's
    /// terminal `BEGIN IMMEDIATE`. Applies the #502 carry re-stamp (Discover only — before the
    /// diff-sized remainder, preserving the old ordering), then the deletions and per-file
    /// remove+insert. All keyed writes, so applying a plan prepared off the lock is idempotent
    /// under the per-repo write flock. No filesystem/parse work happens here.
    fn apply_prepared_incremental_pass<F>(
        &self,
        prepared: &PreparedIncrementalPass,
        mode: IndexMode,
        progress: &mut F,
    ) -> anyhow::Result<IncrementalFilesOutcome>
    where
        F: FnMut(IndexProgress),
    {
        // The #502 carry re-stamps a retained committed row into the active scope. Its dirty-path
        // belt is the discovery status set, but its PRIMARY guard is the exact (sha256, lang, kind)
        // match in `discovery_plan` — a dirty file's disk content does not match a retained
        // committed row's sha — so it is safe to run whether or not that status walk succeeded, and
        // it always runs in Discover mode (unconditionally marking the base scope discovered, as
        // before). This is the pre-#560 behavior; only the destructive, sha-less overlay HEAL gates
        // on a reliable status snapshot.
        let carried = if mode == IndexMode::Discover {
            self.carry_retained_files_into_active_scope(&prepared.carried_ids)?
        } else {
            0
        };
        let written = self.write_prepared_incremental_files(
            &prepared.prepared_files,
            &prepared.deleted,
            Some(mode),
            progress,
        )?;
        Ok(IncrementalFilesOutcome {
            indexed: written.written,
            manifest_in_change_set: prepared.manifest_in_change_set,
            carried,
            logical: written.logical,
        })
    }

    pub(super) fn assign_file_scopes(
        &self,
        files: Vec<IndexFile>,
        changes: &GitChangedPaths,
    ) -> Vec<IndexFile> {
        let has_base_commit = !self.active_commit_sha.is_empty();
        files
            .into_iter()
            .map(|mut file| {
                if !has_base_commit || changes.changed.contains(&file.relative_path) {
                    file.commit_sha.clear();
                    file.worktree_id.clone_from(&self.active_worktree_id);
                } else {
                    file.commit_sha.clone_from(&self.active_commit_sha);
                    file.worktree_id.clear();
                }
                file
            })
            .collect()
    }

    /// Drop or re-stamp stale worktree-overlay rows: overlay rows of the active worktree whose
    /// path is NOT currently dirty (#87). They arise when a dirty file is committed (or its edit
    /// reverted) and the cleanup pass never ran — e.g. a binary upgrade or schema migration cut
    /// the old watcher off mid-session. Left alone they shadow the committed row in every scoped
    /// read (queries see stale content) and collide with a later full rebuild's insert.
    ///
    /// Per stale overlay row:
    /// - a row exists at `(path, active_commit, '')` → DELETE the overlay (cascade via
    ///   `remove_file_in_scope`); the committed row takes over.
    /// - no committed row, and the overlay's `sha256` matches the disk bytes (the path is clean, so
    ///   disk == HEAD) → RE-STAMP the row to the commit scope in place. The row id — and every
    ///   chunk/symbol/embedding/oracle row and memory binding hanging off it — survives.
    /// - no committed row and the sha differs (checkout moved under a stale overlay) → leave it;
    ///   the next discover pass reindexes the path at the commit scope (sha mismatch), and the pass
    ///   after that takes the first branch.
    ///
    /// Returns the number of rows healed. Purely a read when nothing is stale, so an idle pass
    /// stays write-free (#63). Non-git contexts (`active_commit_sha` empty) are untouched —
    /// overlay rows ARE their canonical scope.
    pub(super) fn heal_stale_overlay_rows(&self, root: &Path) -> anyhow::Result<usize> {
        if self.active_commit_sha.is_empty() {
            return Ok(0);
        }
        let overlays: Vec<(i64, String, String)> = {
            let conn = self.storage.connection();
            // Direct `main.files` probes bypass the repo-scoped `files` view, so they carry the
            // `repo_id` predicate explicitly (A3): in a consolidated DB two forks at the same
            // commit share `commit_sha`/`worktree_id`, and without `repo_id` the
            // committed-row probe below would see a SIBLING repo's committed row and
            // delete THIS repo's overlay as if its own base row existed.
            // Also generation-qualified (A6): the heal operates on THIS connection's live
            // generation only — a staged or superseded generation's overlay rows are the
            // rebuild's / gc's business.
            let mut stmt = conn.prepare(
                "SELECT id, path, sha256 FROM main.files
                 WHERE repo_id = ?1 AND worktree_id = ?2 AND worktree_id != '' AND kind != \
                 'deleted' AND generation = ?3",
            )?;
            let rows = stmt.query_map(
                params![self.active_repo_id, self.active_worktree_id, self.active_generation],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        // No overlay candidates → nothing to heal and NO walk under the lock (the common clean-tree
        // / idle pass pays nothing).
        if overlays.is_empty() {
            return Ok(0);
        }
        // Authoritative dirty set, walked INSIDE the write txn (#561). The off-lock discovery
        // snapshot CANNOT soundly pre-filter these: the flock excludes rag-rat writers but NOT the
        // user's editor, so between prepare and now a file dirty-at-prepare may have been restored
        // to clean (its overlay is now stale and due for a delete) and a file
        // clean-at-prepare may have been dirtied (its overlay is now canonical). Every
        // candidate is therefore rechecked against a FRESH status walk here. `Err` => skip
        // the heal entirely (the pre-#560 `Err(_) => 0`). This is the original pre-hoist
        // behavior: the git-status walk (cheap next to the hoisted parse) stays in-txn for
        // correctness, and it only runs when overlay candidates exist.
        let changes = match git_changed_paths(root) {
            Ok(changes) => changes,
            Err(_) => return Ok(0),
        };
        let mut healed = 0usize;
        for (file_id, path, sha) in overlays {
            let p = Path::new(&path);
            if changes.changed.contains(p) || changes.deleted.contains(p) {
                // Dirtied OR deleted since the snapshot — the overlay is NOT stale-clean. Skipping
                // a DELETED path is essential: its delete branch would remove the
                // overlay and expose the base committed file the worktree just
                // deleted (#561).
                continue;
            }
            let committed_exists: bool = self.storage.connection().query_row(
                // Generation-qualified (A6): only a committed row at THIS connection's live
                // generation can take over from the overlay — a staged or superseded generation's
                // committed row must not trigger deleting a live overlay.
                "SELECT EXISTS(SELECT 1 FROM main.files
                 WHERE repo_id = ?1 AND path = ?2 AND commit_sha = ?3 AND worktree_id = ''
                   AND generation = ?4)",
                params![self.active_repo_id, path, self.active_commit_sha, self.active_generation],
                |row| row.get(0),
            )?;
            if committed_exists {
                self.remove_file_in_scope(Path::new(&path), "", &self.active_worktree_id)?;
                healed += 1;
                continue;
            }
            let disk_matches = self
                .storage
                .source_root()
                .map(|root| root.join(&path))
                .and_then(|full| std::fs::read(full).ok())
                .is_some_and(|bytes| hex_sha256(&bytes) == sha);
            if disk_matches {
                self.storage.connection().execute(
                    "UPDATE main.files SET commit_sha = ?2, worktree_id = '' WHERE id = ?1",
                    params![file_id, self.active_commit_sha],
                )?;
                healed += 1;
            }
        }
        Ok(healed)
    }

    /// Prepare + apply a file plan in one shot, parsing INSIDE the caller's transaction. Retained
    /// for the bounded read-path zero-hit heal (`query_api`, ≤4 files) and the worktree-overlay
    /// pass, which are not part of the #560 incremental hoist. The main incremental/discover pass
    /// instead prepares OFF the lock (`prepare_incremental_pass`) and calls
    /// [`Self::write_prepared_incremental_files`] directly.
    pub(super) fn apply_incremental_file_plan<F>(
        &self,
        files: Vec<IndexFile>,
        deleted: BTreeSet<PathBuf>,
        progress: &mut F,
    ) -> anyhow::Result<IncrementalWriteOutcome>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovered { files: files.len() });
        let prepared = prepare_files_with_progress(&files, progress, 0, files.len())?;
        // In-txn caller (zero-hit heal / worktree-overlay pass): the parse+write share one
        // transaction, so no concurrent commit can interleave — no guards needed (#561).
        self.write_prepared_incremental_files(&prepared, &deleted, None, progress)
    }

    /// Write ALREADY-PREPARED files into the live generation: tombstone the deletions, then
    /// per-file remove+insert. Purely SQL writes — no parse — so this is what a terminal `BEGIN
    /// IMMEDIATE` covers. `deleted` and each prepared file are keyed by (path, scope), so
    /// replaying a plan that was prepared off the lock is idempotent under the per-repo write
    /// flock (#560).
    pub(super) fn write_prepared_incremental_files<F>(
        &self,
        prepared: &[PreparedIndexFile],
        deleted: &BTreeSet<PathBuf>,
        mode_guard: Option<IndexMode>,
        progress: &mut F,
    ) -> anyhow::Result<IncrementalWriteOutcome>
    where
        F: FnMut(IndexProgress),
    {
        // #820: the batch's logical-grouping verdict, opened only when this call can actually
        // mutate rows — an idle pass (nothing prepared, nothing deleted) stays at zero reads.
        let mut logical = if prepared.is_empty() && deleted.is_empty() {
            graph_index::LogicalGroupingUpkeep::RelinkMembers(Vec::new())
        } else {
            self.begin_logical_grouping_upkeep()?
        };
        // `mode_guard` is `Some` only for the #560 hoisted path, whose plan was prepared OFF the
        // write lock; the in-txn callers (zero-hit heal, worktree-overlay pass) prepare inside
        // their own transaction — no concurrent commit is possible — so they pass `None`
        // and skip both guards below. The mtime guard (changed-file overwrites) applies in
        // either hoisted mode.
        let guard_concurrent_writes = mode_guard.is_some();
        // The fs-deletion restore recheck applies only to the fs-deletion modes (Changed and
        // Paths). There, `deleted` is purely file-deletions and the target/ignore set is
        // stable within the pass, so "exists on disk" means "restored AND still in scope".
        // Discover's `deleted` also carries SEMANTIC deletions (a path that left the target
        // set or became gitignored but still exists on disk) which MUST be tombstoned
        // regardless of disk existence — and a genuine fs-restore in Discover self-heals on
        // the next discover pass anyway.
        let revalidate_fs_deletions = mode_guard.is_some_and(IndexMode::revalidates_fs_deletions);
        let mut deleted_count = 0usize;
        for path in deleted {
            // A git-deleted path may have been RESTORED on disk during the off-lock window (#561).
            // Tombstoning it would HIDE it — and a file restored to HEAD-clean content is in no
            // later CHANGED set and the stale-overlay heal skips `kind='deleted'` rows,
            // so it would not self-heal until a discover pass. Skip the tombstone only when the
            // path is a regular FILE on disk again — via `symlink_metadata` (does NOT follow), so a
            // SYMLINK or a directory recreated at that path is NOT treated as a restored source
            // file (the walker skips both and would never re-index them), and its stale
            // row still tombstones (#659 review).
            if revalidate_fs_deletions
                && self
                    .storage
                    .source_root()
                    .map(|root| root.join(path))
                    .and_then(|full| full.symlink_metadata().ok())
                    .is_some_and(|meta| meta.is_file())
            {
                continue;
            }
            self.mark_file_deleted(path)?;
            deleted_count += 1;
        }
        // A tombstoned path removes a whole file's keys from the grouped corpus — never
        // key-stable (#820).
        if deleted_count > 0 {
            logical.require_rebuild();
        }

        let total = prepared.len();
        let mut written = 0usize;
        for (index, prepared_file) in prepared.iter().enumerate() {
            let current = index + 1;
            if should_report_file_progress(current, total) {
                progress(IndexProgress::IndexingFile {
                    current,
                    total,
                    path: prepared_file.file.relative_path.clone(),
                    language: prepared_file.file.language,
                    kind: prepared_file.file.kind,
                });
            }
            // A lockless heal could have indexed a NEWER on-disk version of THIS exact scope key in
            // the off-lock prepare window (#561). Compare the CURRENT row's disk mtime to the one
            // we prepared: if the row already reflects a newer disk state, skip the
            // remove+insert so a concurrent heal's row is not rolled back to our stale
            // content. Disk mtime — not the indexing clock — is the signal, so our OWN
            // prior row (an OLDER-or-equal disk mtime, since edits only advance it)
            // never trips this; only a genuinely newer index does. A skip leaves the
            // path dirty for the next changed pass, so it can only DEFER, never strand.
            if guard_concurrent_writes
                && let Ok(content) = &prepared_file.prepared
                && let Some(row_modified_at_ms) = self.scope_row_modified_at_ms(
                    &prepared_file.file.relative_path,
                    &prepared_file.file.commit_sha,
                    &prepared_file.file.worktree_id,
                )?
                && row_modified_at_ms > content.modified_at_ms
            {
                continue;
            }
            // True no-op skip for the explicit-path flow ONLY (#659 review): `index --paths`
            // prepares EVERY supplied file — including clean/reverted ones — to scope
            // them, so without this an unchanged file would be needlessly
            // removed+reinserted, churning its id and cascade-dropping its chunk
            // embeddings. Compares the FULL `(sha256, language, kind)` identity, not sha alone: a
            // TARGET-identity drift with unchanged bytes (an extension-precedence change
            // re-languages the path) must still reindex, exactly as discovery's
            // staleness does — a sha-only skip would strand the old parse. Gated to
            // `Paths`: the heal / worktree-overlay callers (`mode_guard = None`)
            // deliberately re-index UNCHANGED content to upgrade a stale anchor/schema
            // format the sha doesn't capture, and Changed/Discover never prepare an
            // unchanged file — so neither wants this skip.
            if mode_guard == Some(IndexMode::Paths)
                && let Ok(content) = &prepared_file.prepared
                && self.scope_row_identity(
                    &prepared_file.file.relative_path,
                    &prepared_file.file.commit_sha,
                    &prepared_file.file.worktree_id,
                )? == Some((
                    content.sha256.clone(),
                    prepared_file.file.language.as_str().to_string(),
                    prepared_file.file.kind.as_str().to_string(),
                ))
            {
                continue;
            }
            // #820 key-stability capture, BEFORE the removal cascades the old member rows away
            // with the symbols. Skipped once the batch already owes a rebuild — the capture
            // would be dead weight then.
            let replaced_grouping = if logical.is_relinkable() {
                self.load_grouped_key_claims(
                    &prepared_file.file.relative_path,
                    &prepared_file.file.commit_sha,
                    &prepared_file.file.worktree_id,
                )?
            } else {
                None
            };
            self.remove_file_in_scope(
                &prepared_file.file.relative_path,
                &prepared_file.file.commit_sha,
                &prepared_file.file.worktree_id,
            )?;
            // Incremental per-file replace; chunk_fts is kept synced in place by the inline write
            // in insert_chunks (no full rebuild_fts). No accumulator — edges are
            // inserted unresolved here and resolved by resolve_edges in the caller's
            // terminal tail.
            self.insert_prepared_file(prepared_file, None)?;
            written += 1;
            // Compare the replacement's key multiset against the captured claims: identical →
            // the file's owed member rows accumulate; anything else (including a first-time
            // scope row, whose captured claims are empty) downgrades the batch to the rebuild.
            let owed_relinks = match &replaced_grouping {
                Some(replaced) => self.derive_key_stable_relinks(
                    &prepared_file.file.relative_path,
                    &prepared_file.file.commit_sha,
                    &prepared_file.file.worktree_id,
                    replaced,
                )?,
                None => None,
            };
            logical.absorb_replaced_file(owed_relinks);
        }

        // Count only what actually landed (skips excluded), so an all-skipped pass keeps the
        // caller's `mutated` flag honest and preserves the #63 idle no-write invariant.
        Ok(IncrementalWriteOutcome { written: written + deleted_count, logical })
    }
}

/// Whether any path in an iterator is a `Cargo.toml` — the gate for the per-package refresh on an
/// incremental pass (#61, salvaging #95). Checked against both changed and deleted paths so a crate
/// removal also triggers the refresh.
fn paths_include_cargo_toml<'a>(mut paths: impl Iterator<Item = &'a Path>) -> bool {
    paths.any(is_cargo_toml)
}

fn is_cargo_toml(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml")
}
