// Re-export the test-only wave barrier (defined at the end of the file so the `#[cfg(test)]`
// module stays last — clippy::items_after_test_module). `incremental.rs`'s wave loop calls
// `run_after_wave_commit`; the reader-consistency tests register a database-keyed hook via
// `set_after_wave_commit` and hold the returned guard.
#[cfg(test)]
pub(crate) use wave_barrier::{WaveBarrierGuard, run_after_wave_commit, set_after_wave_commit};

use super::*;

/// Which liveness AXIS staged the rows a [`IndexDatabase::delete_staged_files_cascade`] call is
/// sweeping (A6). The cascade's id-keyed children behave identically under both, but the
/// GENERATION-LESS, path-keyed tables must be classified per axis — see the `parser_failures`
/// block inside the cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StagedSweep {
    /// The staged rows' `(commit_sha, worktree_id)` context is dead (outside every live set): the
    /// path's last owner in this repo is going away, so path-keyed satellite state goes with it.
    DeadContext,
    /// The staged rows belong to a superseded `files.generation` (a completed rebuild's old
    /// generation, or a torn rebuild's never-flipped staging). The SAME paths live on in the
    /// current generation, so path-keyed satellite state must survive untouched.
    DeadGeneration,
}

impl IndexDatabase {
    pub fn rebuild(config: &Config) -> anyhow::Result<Self> {
        Self::rebuild_with_progress(config, |_| {})
    }

    pub fn rebuild_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Started {
            database: config.database.clone(),
            mode: IndexMode::Full,
        });
        // Every rebuild ENTRY holds the per-repo write flock (batch 6, concurrency HIGH). The
        // library `rebuild` was a FLOCK-LESS production writer reachable from `rag-rat init`'s
        // `setup_index` (through `index_discover`'s missing-DB / empty-index fallback to
        // `rebuild_with_progress`) — which FALSIFIED the precondition gc's `generation != live`
        // deadness predicate documents ("every production rebuild entry runs under the flock"): a
        // flock-holding collector (watcher/git-hook maintenance, `rag-rat gc`) racing this staging
        // would read live=N, classify the mid-flight staged N+1 as dead, and cascade it — the
        // rebuild's flip then publishes an EMPTY generation and returns Ok. Acquiring here makes
        // the precondition hold by CONSTRUCTION, not convention. `WriteLock` is reentrant
        // within a thread, so the CLI/watcher wrappers that already hold it just bump the
        // depth; a flock-less caller (init) acquires it fresh. `write_lock_repo_id`
        // resolves from git WITHOUT opening the DB, so it precedes `create_or_migrate`
        // (which opens it). Blocking (non-interactive writer); a racing collector holds the
        // flock only for its brief sweep.
        let lock_repo = crate::locks::write_lock_repo_id(config);
        let _write_lock = crate::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
        // #427 — enforce the empty-index invariant on the FULL-rebuild entry. Split around
        // `create_or_migrate` so it mirrors the incremental path (open+migrate, THEN check) while
        // still leaving NO stray DB file behind on a fresh refusal:
        //  * PHASE 1 — a FRESH database (the file does not exist yet) that would discover nothing
        //    is refused BEFORE `create_or_migrate` materializes the file (the `maintenance`
        //    `!db.exists()` contract). A fresh DB is trivially not-already-indexed, so the check is
        //    just the discovery walk.
        //  * PHASE 2 — an EXISTING database is refused only AFTER `create_or_migrate` has brought
        //    its schema current, because a pre-registry / older index can't be read (no `repos`
        //    table) until it migrates. Checking post-migration lets a legacy index whose files were
        //    all deleted be RECOGNIZED (via the persisted `source_root`, incl. the sole-placeholder
        //    fallback for a not-yet-adopted legacy DB) and PRUNED, instead of wrongly refused as
        //    first-time-empty (#427 review — the read-before-migrate hole the incremental path
        //    never had). A genuine first-time-empty registration into an existing shared DB is
        //    still refused, before `adopt` records it. `is_first_time_empty_conn` short-circuits on
        //    an already-indexed root, so an established repo pruning to empty never pays the walk.
        // Read-only `open_config` opens deliberately do NOT enforce (a read must not error); they
        // can't smuggle an empty scope in because recognition keys on `source_root`, which only
        // these indexing passes write. Callers react to the error: the one-shot `index`
        // surfaces it; the watcher / maintenance `let _ =`-discard it and wait for content.
        // `--allow-empty` opts in.
        let db_existed = config.database.exists();
        if !config.allow_empty && !db_existed && !crate::index::would_discover_any_file(config)? {
            return Err(crate::index::EmptyIndexRefused {
                root: config.root.display().to_string(),
            }
            .into());
        }
        let mut db = Self::create_or_migrate(&config.database)?;
        if !config.allow_empty
            && db_existed
            && crate::index::is_first_time_empty_conn(db.storage.connection(), config)?
        {
            return Err(crate::index::EmptyIndexRefused {
                root: config.root.display().to_string(),
            }
            .into());
        }
        // Register/adopt the repo BEFORE the scope view is installed and BEFORE any repo-scoped
        // write, so `active_repo_id` is a real (registered) id — every direct-scoped insert stamps
        // it, and `repo_meta` writes below satisfy the `repo_meta → repos` FK (an
        // empty/unregistered id would violate it). `create_or_migrate` no longer resolves
        // the repo or heals the model manifest, so both move here, ordered after
        // registration. INDEXING intent: a full rebuild records this checkout's root (#427).
        db.adopt_repo_from_config(config, super::lifecycle::AdoptIntent::Indexing)?;
        // A6: stage a FRESH file generation instead of clearing-then-reinserting in one long
        // write-locked transaction. `old_live` is the generation readers currently see; `target` is
        // higher than any the repo holds, so it never collides with a torn prior rebuild's
        // committed-but-never-flipped generation (which `old_live + 1` could). The rebuild writes
        // `target`; the flip publishes it.
        let old_live = schema::live_files_generation(db.storage.connection(), &db.active_repo_id)?;
        let target = db.next_files_generation(old_live)?;
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        // Install the scope view + writer stamp at the WRITE generation, so every insert lands on
        // `target` and the rebuild's own edge-resolution / logical-symbol reads see only it.
        db.set_context_at_generation(&commit_sha, &worktree_id, target)?;
        ai::ensure_model_manifest(db.storage.connection())?;
        progress(IndexProgress::IndexingGitHistory);
        let mut git_history = Some(spawn_git_history_prepare(&config.root));
        // RAM-first bulk build: give SQLite a large per-connection page cache. `synchronous` stays
        // NORMAL — the shared global DB must NEVER run `synchronous = OFF` (spec §3.3, Global
        // Constraint). A full rebuild is no longer one mega-transaction, so a crash mid-rebuild
        // leaves a STAGED (never-flipped) generation gc reclaims, not a corrupt file — durability
        // no longer trades against a giant write. `cache_size`/`soft_heap_limit` are per-connection
        // and safe under concurrency; do NOT touch `temp_store` (it would drop the
        // connection_context overlay temp table `set_context_at_generation` created above).
        db.storage.execute_batch("PRAGMA cache_size = -262144;")?;
        maybe_set_sqlite_soft_heap_limit();
        // Diagnostic: override wal_autocheckpoint for this rebuild. No-op unless
        // RAG_RAT_WAL_AUTOCHECKPOINT is set.
        if let Ok(raw) = std::env::var("RAG_RAT_WAL_AUTOCHECKPOINT")
            && let Ok(pages) = raw.trim().parse::<i64>()
        {
            db.storage.execute_batch(&format!("PRAGMA wal_autocheckpoint = {pages};"))?;
        }
        let result = (|| -> anyhow::Result<usize> {
            mem_trace("before rebuild (staging a fresh generation)");
            // NO clear of the live rows anywhere below: the old generation stays intact and
            // complete for concurrent readers until the flip; the fresh generation lands alongside
            // it and gc sweeps the dead one afterward (A6). The wave-staging scratch temp tables
            // are created by `index_targets_with_progress` itself (batch 7 — the standalone
            // `index_targets` entry shares the same wave loop and needs them too).
            // Only the IN-MEMORY source root is set here (this connection reads file bytes from
            // the new checkout for the whole staging run); the PERSISTED
            // `repo_meta[source_root]` is an AUTHORITY key other readers resolve fs-fallback
            // paths against (memory validation, heals), so it joins the cursors-last set and is
            // written inside the terminal flip transaction (batch-5 P2): a rebuild from a NEW
            // checkout root that fails pre-publish must leave old-generation readers resolving
            // against the OLD root. Audit of the other early meta writes found no further
            // authority keys: the FTS freshness trio is global infrastructure over main.* (not
            // generation authority), and everything else already rides the flip.
            db.storage.set_source_root(config.root.clone());

            // Phase 1: the file/chunk/symbol bulk, written at the WRITE generation in CHUNKED
            // transactions (one per wave) so the rebuild never holds the shared DB's write lock
            // across the whole file set. Base EDGES accumulate in the returned `graph` and are
            // resolved LATER, inside the terminal transaction (batch 6 #4) — NOT here — so the
            // generation-less `packages`/`local_crate_roots` they resolve against never precede the
            // flip. `refresh_packages` moved out with them.
            let (indexed, graph) = db.index_targets_with_progress(config, &mut progress)?;
            mem_trace("after index_targets (staged generation written; base edges pending)");

            // Phase 2: the ONLY pre-flip derived write left is the compressed chunk_text store. It
            // is keyed by STAGED chunk ids (a live-generation reader never joins them — inert) and
            // gc sweeps it with the dead generation, so committing it before the flip misleads no
            // reader. Everything a reader treats as AUTHORITY moved into the terminal flip
            // transaction below (batch 6): base edges + package roots (#4), the git-history ROWS +
            // `commit_fts` (#1), and the clone-token df (#2). The pre-A6 mega-transaction's
            // atomicity for those domains is restored WITHOUT re-inflating Phase 1's chunked waves.
            db.storage.execute_batch("BEGIN IMMEDIATE")?;
            // Derive the compressed chunk_text store (#77 Phase 2) from the staged chunk text.
            db.build_chunk_text_store()?;
            db.storage.execute_batch("COMMIT")?;
            mem_trace("after phase 2 (chunk_text store)");

            // Phase 3: the TERMINAL transaction. The pointer flip is the LAST fallible step of a
            // rebuild — everything a reader treats as AUTHORITY or that must be CONSISTENT with the
            // published generation completes INSIDE this one transaction, with
            // `live_files_generation` written last. A failure anywhere here rolls the WHOLE tail
            // back — the pointer never moves, readers stay on the complete old generation, and a
            // retry stages a fresh target. Its writes are uncommitted until the flip, so a
            // concurrent reader/heal never observes any of the GENERATION-LESS ones (base package
            // roots, re-keyed overlay packages, git-history rows, clone df) in front of the
            // still-live OLD generation. TXN-SIZE TRADEOFF (batch 6): the terminal txn now also
            // carries base-edge resolution (O(edges), formerly its own Phase-1 tail txn) and the
            // git-history rows + external-content `commit_fts` rebuild (O(commits) on a
            // deep-history repo). Both are the price of atomicity and stay proportional
            // to symbol/edge/history counts, NOT the file bytes — the whole-file
            // indexing bulk stays in Phase 1's waves.
            db.storage.execute_batch("BEGIN IMMEDIATE")?;
            // Carry every live row OUTSIDE the base scope (linked-worktree overlays, other-commit
            // leftovers) forward onto `target` — the rebuild only re-emits the base scope, so
            // they must ride along to stay visible after the flip.
            db.carry_forward_live_overlays(target, old_live)?;
            let carried_overlays = db.carried_overlay_worktrees(target)?;
            // Carry each carried overlay's PACKAGE ROOTS onto the new base scope (batch 6 #3): the
            // overlay FILE rows carried above are matched into the scope view by `worktree_id`
            // alone, but `load_package_roots_into_scope` reads `packages` by `(commit_sha,
            // worktree_id)`, so an overlay keyed to the OLD base HEAD finds NO package map under
            // the re-resolution view (installed at the NEW base commit) and resolves
            // its imports fall-open. Re-key their `commit_sha` to the rebuilt HEAD
            // BEFORE the re-resolution.
            db.carry_forward_overlay_packages(&carried_overlays)?;
            // Base package roots + base-edge resolution, folded into the flip (batch 6 #4):
            // `finalize_base_edges` writes the generation-less `packages`/`local_crate_roots` and
            // resolves every accumulated base edge against them. Runs INSIDE the terminal txn so
            // those authority writes are invisible to a concurrent reader/heal until the pointer
            // moves and roll back with a failed tail; the resolve reads the package rows back from
            // this same uncommitted transaction.
            db.finalize_base_edges(config, graph)?;
            // Re-resolve each carried overlay's OWN edges against the freshly staged base (P2
            // review): the base re-emit re-minted every base `symbols.id`, so a carried overlay
            // edge's `to_symbol_id` still points at the OLD generation's symbol row — dead the
            // moment gc sweeps it. `resolve_overlay_edges` writes only the overlay's own rows
            // (targets span the overlay view), exactly as `finalize_overlay_refresh` uses it; the
            // view is swapped per overlay and restored to the base scope after.
            for worktree_id in &carried_overlays {
                db.install_view_for_scope(&db.active_commit_sha, worktree_id, target)?;
                db.resolve_overlay_edges(worktree_id)?;
            }
            if !carried_overlays.is_empty() {
                db.install_view_for_scope(&db.active_commit_sha, &db.active_worktree_id, target)?;
            }
            // Logical symbols fold the generation being published (base scope + carried
            // overlays), scoped to `self.active_generation == target` — the carried overlays are
            // already at `target` within this transaction, so the fold sees them (the A6 handoff
            // note, now satisfied PRE-flip inside the same atomic write).
            progress(IndexProgress::RebuildingLogicalSymbols);
            db.rebuild_logical_symbols()?;
            // Publish the STAGED `parser_failures` state (upserts for paths that failed this
            // pass, clears for clean re-parses, an orphan sweep for paths removed from the tree)
            // atomically with the flip: the waves staged these mutations in a temp table instead
            // of writing the generation-less table mid-pass, so readers see the OLD failure state
            // until the pointer moves and a tail failure rolls the whole reconciliation back with
            // it. Generation-dead gc deliberately never touches this table.
            db.apply_staged_parser_failures(target)?;
            // Recompute clone-token df over the FINAL published set (batch 6 #2): it reads
            // `symbol_fingerprints` at `active_generation == target`, which AFTER the overlay
            // carry-forward above is exactly the generation about to go live (base + carried
            // overlays). Recomputing in Phase 2 (before carry-forward) either omitted carried
            // overlay fingerprints from the df on success, or on a tail failure left the OLD
            // generation's clone queries reading a df computed from the never-published target —
            // and the df drives sub-block-postings selection + persisted-postings
            // invalidation, so it is consumed as authority w.r.t. the published set,
            // not merely a drift-tolerated hint.
            db.refresh_clone_token_df()?;
            // Git-history ROWS + external-content `commit_fts` fold into the flip too (batch 6 #1):
            // `git_commits`/`git_file_changes` are read DIRECTLY by
            // `query::orientation::recent_commit_subjects`, lexical churn, and commit search — not
            // only through the deferred `git_history_indexed_*` cursors — so landing them in Phase
            // 2 let a rebuild that observed changed/cleared history then failed
            // pre-flip strand the NEW history rows in front of the still-live OLD file
            // generation. The reload-gate CURSORS still write cursors-last below; the
            // ROWS + `commit_fts` now ride the same atomic transaction as the file
            // generation, so orientation/search can never mix new history with the old
            // files.
            let history_cursors = db.apply_prepared_git_history_deferring_cursors(
                &config.root,
                git_history
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("git history preparation was already used"))?,
            )?;
            progress(IndexProgress::RebuildingFts);
            // chunk_fts was written inline during chunk insert; only the external-content
            // commit_fts needs the bulk 'rebuild' here (#77 Phase 2), now atomic with
            // the git rows it indexes.
            db.finalize_full_rebuild_fts()?;
            progress(IndexProgress::ResolvingGraph);
            db.mark_graph_index_current()?;
            // Full rebuild writes correct `files.generated`, so stamp the flags version current and
            // skip a redundant re-derive on next open (#202).
            db.mark_generated_flags_current()?;
            // The git AUTHORITY writes ride the flip (batch-4 P2): `git_commit`/`git_dirty` meta
            // and the history reload-gate cursors say "this index reflects commit H" — true only
            // once the generation built at H is published. Deferring them here means a tail
            // failure leaves status()/`is_history_current` honestly reporting the OLD state (a
            // reload stays owed), and the retry publishes files + git authority together.
            db.set_repo_meta("source_root", &config.root.display().to_string())?;
            db.write_git_meta(&config.root)?;
            db.mark_active_base_scope_discovered(&config.targets)?;
            if let Some(cursors) = &history_cursors {
                db.record_git_history_cursors(cursors)?;
            }
            // Publish: `live_files_generation` LAST, so a concurrent reader sees either the whole
            // old generation or the whole new one, never a mix. The active-model seed rides the
            // flip (#394): it is advanced only when the fresh generation actually goes live.
            db.set_repo_meta(schema::LIVE_FILES_GENERATION_META_KEY, &target.to_string())?;
            db.set_repo_meta("indexed_at_ms", &now_ms().to_string())?;
            ai::seed_active_embedding_model(
                db.storage.connection(),
                config.llm.embedding.backend.model_id(),
            )?;
            db.storage.execute_batch("COMMIT")?;
            mem_trace("after terminal flip (overlay edges + logical symbols + pointer)");
            progress(IndexProgress::Finished { files: indexed });
            Ok(indexed)
        })();
        if result.is_err() {
            if let Some(handle) = git_history.take() {
                let _ = join_git_history_prepare(handle);
            }
            // Roll back whichever phase transaction was left open by the failing `?` (Phase 1's
            // per-wave commits already landed a staged generation that never flips — dead,
            // gc-swept).
            let _ = db.storage.execute_batch("ROLLBACK");
        }
        // cache_size is left bumped — harmless for the short remaining lifetime of the connection.
        result?;
        // Poison-sibling test harness (compiled out of production): after the rebuild flips,
        // register a second `poison-sibling` repo with tripwire rows in every repo-scoped
        // table, so any unscoped read/count/delete downstream trips an EXISTING test.
        // Default-ON per test thread; a test needing a virgin single-repo DB opts out via
        // `poison_sibling::disable_poison_sibling`.
        #[cfg(test)]
        crate::index::poison_sibling::seed_if_enabled(db.storage.connection())?;
        Ok(db)
    }

    /// The generation a full rebuild stages into (A6): STRICTLY ABOVE both every generation the
    /// ACTIVE repo's rows currently carry AND the live pointer itself. The row-MAX keeps it above
    /// a torn prior rebuild's committed-but-never-flipped staging (which `live + 1` could collide
    /// with); folding `old_live` in keeps it above the pointer even when the live generation has
    /// ZERO rows left (every file incrementally removed) — without that, `MAX(rows) + 1` could
    /// allocate BELOW live, and gc's LOCKLESS-SAFE `generation < live` fallback predicate would
    /// classify the fresh staging as superseded and cascade it mid-rebuild. (Under gc's primary
    /// `generation != live` form the per-repo write flock is the protection — every rebuild entry
    /// holds it, batch 6 — but the allocator stays above live so the lockless fallback is safe too,
    /// and so the `< live`/`!= live` forms agree on an in-flight staging.) Per-repo — the `files`
    /// view scopes `(repo_id, generation)`, so generations need only be unique WITHIN a repo.
    fn next_files_generation(&self, old_live: i64) -> anyhow::Result<i64> {
        let max_row_generation = self.storage.connection().query_row(
            "SELECT COALESCE(MAX(generation), 0) FROM main.files WHERE repo_id = ?1",
            params![self.active_repo_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(max_row_generation.max(old_live) + 1)
    }

    /// The DISTINCT linked-worktree ids among the rows just carried forward onto `target` — the
    /// overlay scopes whose edges the terminal transaction must re-resolve against the freshly
    /// staged base (their old `to_symbol_id` targets die with the superseded generation). Excludes
    /// the empty (committed) scope and the base checkout's own worktree id; other-commit leftovers
    /// carry no worktree id, so they never appear here (their edges are self-contained per scope
    /// and die with their context at gc).
    fn carried_overlay_worktrees(&self, target: i64) -> anyhow::Result<Vec<String>> {
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT worktree_id FROM main.files
             WHERE repo_id = ?1 AND generation = ?2
               AND worktree_id != '' AND worktree_id != ?3
             ORDER BY worktree_id",
        )?;
        let rows = stmt
            .query_map(params![self.active_repo_id, target, self.active_worktree_id], |row| {
                row.get::<_, String>(0)
            })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Carry every LIVE row OUTSIDE the base rebuild scope — linked-worktree overlays and
    /// other-commit leftovers — forward from the previous live generation `old_live` onto the
    /// freshly-staged `target`, so they stay visible after the flip. A full rebuild only re-emits
    /// the BASE scope (`commit_sha` = the rebuilt HEAD, or `worktree_id` = the base checkout),
    /// so those non-base rows would otherwise vanish under the new live generation; the base
    /// scope it DID re-emit stays at `old_live` → dead → swept by gc. A no-op on a non-git repo
    /// (base commit + base worktree both empty ⇒ every row IS in the base scope) and when there
    /// are no overlays. Runs INSIDE the terminal flip transaction so a reader sees overlays at
    /// exactly one generation.
    fn carry_forward_live_overlays(&self, target: i64, old_live: i64) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "UPDATE main.files SET generation = ?1
             WHERE repo_id = ?2 AND generation = ?3
               AND commit_sha != ?4 AND worktree_id != ?5",
            params![
                target,
                self.active_repo_id,
                old_live,
                self.active_commit_sha,
                self.active_worktree_id
            ],
        )?;
        Ok(())
    }

    /// Re-key each carried linked-worktree overlay's generation-less `packages` rows onto the
    /// rebuilt base commit so its terminal-txn edge re-resolution finds them (batch 6 #3). An
    /// overlay's FILE rows are matched into the scope view by `worktree_id` ALONE
    /// (commit-agnostic), but `load_package_roots_into_scope` (resolve.rs) reads `packages` by
    /// BOTH `(commit_sha, worktree_id)`, and the re-resolution installs the view at
    /// `active_commit_sha` (the NEW base HEAD). An overlay indexed at the OLD base HEAD keyed
    /// its package rows to that old commit, so without this the read finds no overlay package
    /// map and resolves the branch's imports against the wrong / fall-open roots — until a
    /// separate overlay refresh runs. Re-keying `commit_sha` to the rebuilt HEAD aligns them
    /// with BOTH the re-resolution context AND a post-flip overlay query's `(base_sha,
    /// worktree_id)` scope.
    ///
    /// SUPERSEDED OLD ROWS ARE DELETED, NOT RE-KEYED (batch 7 P2): an overlay that already
    /// REFRESHED after the base HEAD moved wrote fresh rows at the NEW `(commit_sha,
    /// worktree_id)` while its old-commit rows lingered (`refresh_packages` deletes only its own
    /// scope). A blind re-key of those old rows collides with the fresh ones under
    /// `UNIQUE(repo_id, manifest_dir, commit_sha, worktree_id)` and ABORTS the whole rebuild —
    /// and the fresh refresh is also more current than any re-key could make the stale row. So:
    /// (1) DELETE old-commit rows whose `manifest_dir` already has a row at the new key (the
    /// fresh refresh wins); (2) DELETE all-but-the-newest duplicates among the REMAINING
    /// old-commit rows — rows at TWO different stale commits (two refreshes, two HEAD moves ago)
    /// would otherwise both re-key to the same new key and collide with each other; highest
    /// `id` wins (`refresh_packages` reinserts per pass, so rowid order is recency); (3) re-key
    /// the survivors. `commit_sha != ?1` throughout keeps a same-HEAD rebuild (the overlay
    /// already at this commit) a pure no-op — no self-UPDATE, no collision, nothing deleted.
    /// Scoped to exactly the `worktree_ids` `carried_overlay_worktrees` re-resolves (non-empty,
    /// != the base checkout), so other-commit leftovers (`worktree_id = ''`) are untouched. Runs
    /// INSIDE the terminal flip transaction, so the re-key is invisible until the pointer moves.
    fn carry_forward_overlay_packages(&self, worktree_ids: &[String]) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let mut delete_shadowed = conn.prepare(
            "DELETE FROM packages
             WHERE repo_id = ?2 AND worktree_id = ?3 AND commit_sha != ?1
               AND manifest_dir IN (
                   SELECT manifest_dir FROM packages
                   WHERE repo_id = ?2 AND worktree_id = ?3 AND commit_sha = ?1
               )",
        )?;
        let mut delete_stale_duplicates = conn.prepare(
            "DELETE FROM packages
             WHERE repo_id = ?2 AND worktree_id = ?3 AND commit_sha != ?1
               AND id NOT IN (
                   SELECT MAX(id) FROM packages
                   WHERE repo_id = ?2 AND worktree_id = ?3 AND commit_sha != ?1
                   GROUP BY manifest_dir
               )",
        )?;
        let mut rekey = conn.prepare(
            "UPDATE packages SET commit_sha = ?1
             WHERE repo_id = ?2 AND worktree_id = ?3 AND commit_sha != ?1",
        )?;
        for worktree_id in worktree_ids {
            delete_shadowed.execute(params![
                self.active_commit_sha,
                self.active_repo_id,
                worktree_id
            ])?;
            delete_stale_duplicates.execute(params![
                self.active_commit_sha,
                self.active_repo_id,
                worktree_id
            ])?;
            rekey.execute(params![self.active_commit_sha, self.active_repo_id, worktree_id])?;
        }
        Ok(())
    }

    /// Recompute `clone_token_df` exactly from the `symbol_fingerprints.token_bag` BLOBs (#231).
    /// A Rust aggregate over the decoded bags replaces the former `GROUP BY symbol_token_postings`
    /// (that table is dropped in V032) with the same authoritative document frequency: the count of
    /// distinct symbols whose bag contains each token, per `(normalizer_kind, token_hash)`. Runs
    /// inside the rebuild transaction so the df and the fingerprints it summarizes commit
    /// atomically.
    ///
    /// R6 — SCOPE PARITY: this reads EVERY `symbol_fingerprints` row (no `files.generated` filter),
    /// matching the original GROUP BY's scope over whatever rows exist. (Since #232, generated
    /// files are no longer fingerprinted at index time, so there are no generated-file fp rows to
    /// count — the df scope now lines up with the `generated = 0` candidate read rather than being
    /// deliberately wider; df is selectivity-only and drift-tolerated, so this is not load-bearing
    /// for recall either way.) df feeds candidate GENERATION (the `sub_block_tokens` ordering), not
    /// just ranking. Each symbol's decoded bag has no duplicate `token_hash` (the codec invariant),
    /// so counting one increment per (symbol, token) pair equals `COUNT(DISTINCT symbol_id)`.
    /// Recompute the LIVE `clone_token_df` exactly from the active repo's token-bag BLOBs — the
    /// authoritative refresh the full-rebuild / standalone-index finalizes run (the waves skip
    /// per-token bumps via `BumpDf(false)` and settle here instead). #479: this refreshes the
    /// LIVE selectivity table only; the persisted clone-graph postings are ordered by their own
    /// generation's `clone_df_epoch` snapshot, so a refresh no longer invalidates them.
    pub(crate) fn refresh_clone_token_df(&self) -> anyhow::Result<()> {
        // Resolve the active-repo scope ONCE: it gates BOTH the fingerprint READ below and the df
        // wipe/reinsert (Phase 2). `symbol_fingerprints` is deliberately `repo_id`-free (keyed by
        // `symbol_id`, FK CASCADE — see the CLONE_FINGERPRINT_DDL note), so the read is scoped
        // TRANSITIVELY by joining `symbols` → `main.files` and filtering `files.repo_id`, exactly
        // like the clone candidate reads. Without it, a consolidated DB's SIBLING repo's
        // fingerprints pool their token frequencies into THIS repo's df — inflating
        // document frequencies and reordering `sub_block_tokens`, which changes SourcererCC
        // candidate selection (the review finding). `probe = "clone_token_df"` is
        // authoritative for the periphery set.
        let df_scope = {
            let conn = self.storage.connection();
            crate::index::schema::periphery_repo_scope(conn, "clone_token_df")?
        };

        // Phase 1 (read): decode every (active-repo) fingerprint's bag and accumulate df in memory,
        // off the connection borrow. NULL `token_bag` rows (un-reindexed after the V032 migration)
        // and any stale/corrupt blob (decode → None) contribute nothing, exactly as a missing
        // postings row would have.
        // Outer key: normalizer_kind (cloned once per kind, not per (symbol, token) pair).
        // Inner key: token_hash → df count.
        let mut df: BTreeMap<String, BTreeMap<i64, i64>> = BTreeMap::new();
        {
            let conn = self.storage.connection();
            // A6: the join also filters the ACTIVE generation. During a full rebuild this runs
            // AFTER the fresh generation's fingerprints are staged but BEFORE gc sweeps the
            // superseded one, so a repo-only join would fold BOTH generations' bags and
            // systematically double every df — not drift, a 2x skew of the "exact" recompute this
            // function exists to produce. `active_generation` is the WRITE generation on the
            // rebuild connection (exactly the fingerprints just written) and the live generation
            // elsewhere. The pre-V042 `None` branch has no files join to filter (that schema also
            // predates V043).
            let read_sql = match &df_scope {
                Some(repo_id) => format!(
                    "SELECT symbol_fingerprints.normalizer_kind, symbol_fingerprints.token_bag
                     FROM symbol_fingerprints
                     JOIN symbols ON symbols.id = symbol_fingerprints.symbol_id
                     JOIN main.files ON main.files.id = symbols.file_id
                     WHERE main.files.repo_id = '{}' AND main.files.generation = {}",
                    repo_id.replace('\'', "''"),
                    self.active_generation
                ),
                None => "SELECT normalizer_kind, token_bag FROM symbol_fingerprints".to_string(),
            };
            let mut stmt = conn.prepare(&read_sql)?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let normalizer_kind: String = row.get(0)?;
                let Some(blob) = row.get::<_, Option<Vec<u8>>>(1)? else {
                    continue;
                };
                let Some(bag) = clones::bag_blob::decode_token_bag(&blob) else {
                    continue;
                };
                let inner = df.entry(normalizer_kind).or_default();
                for (token_hash, _freq) in bag {
                    *inner.entry(token_hash).or_insert(0) += 1;
                }
            }
        }

        // Phase 2 (write): replace the ACTIVE REPO's df with the recomputed df. Post-A5
        // `clone_token_df` carries `repo_id` in its PK (df must not pool across repos), so both the
        // wipe and the reinsert scope to the active repo — the repo id is embedded as a per-call
        // literal so the bound params stay unchanged; pre-A5 uses the original global SQL.
        let conn = self.storage.connection();
        let df_delete_sql = match &df_scope {
            Some(repo_id) => format!(
                "DELETE FROM clone_token_df WHERE repo_id = '{}';",
                repo_id.replace('\'', "''")
            ),
            None => "DELETE FROM clone_token_df;".to_string(),
        };
        let df_insert_sql = match &df_scope {
            Some(repo_id) => format!(
                "INSERT INTO clone_token_df(repo_id, normalizer_kind, token_hash, df)
                 VALUES ('{}', ?1, ?2, ?3)",
                repo_id.replace('\'', "''")
            ),
            None => "INSERT INTO clone_token_df(normalizer_kind, token_hash, df) VALUES (?1, ?2, \
                     ?3)"
            .to_string(),
        };
        conn.execute_batch(&df_delete_sql)?;
        for (normalizer_kind, inner) in df {
            for (token_hash, count) in inner {
                conn.prepare_cached(&df_insert_sql)?.execute(params![
                    normalizer_kind,
                    token_hash,
                    count
                ])?;
            }
        }
        Ok(())
    }

    /// Set up the per-connection scratch temp tables the wave loop needs, WITHOUT clearing any
    /// live rows (A6). The rebuild stages a FRESH generation alongside the live one — the old
    /// generation must stay intact for concurrent readers until the flip, and gc sweeps it
    /// afterward — so the former staged-cascade DELETE of the active scope is GONE (its
    /// collision-avoidance job is obsolete: the widened `UNIQUE(repo_id, path, commit_sha,
    /// worktree_id, generation)` lets a stale lingering overlay coexist with the fresh insert
    /// at a different generation).
    ///
    /// Only the first-index chunk-text staging table is (re)created here: `insert_chunks` writes
    /// the in-memory text into it and `build_chunk_text_store` reads + clears it (there is no
    /// chunks.text column). `chunk_text_dict` is never cleared — dicts are IMMUTABLE decode
    /// keys (#77 Phase 2), and other generations'/scopes' blobs reference existing versions.
    /// Called by `index_targets_with_progress` (batch 7): BOTH entries — the full rebuild and the
    /// standalone `index_targets` — run the wave loop whose inserts stage into this table.
    pub(super) fn prepare_rebuild_scratch_tables(&self) -> anyhow::Result<()> {
        self.storage.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS rebuild_chunk_text(
                 chunk_id INTEGER PRIMARY KEY,
                 text TEXT NOT NULL
             );
             DELETE FROM temp.rebuild_chunk_text;",
        )?;
        Ok(())
    }

    /// Cascade-delete every derived row (edges, symbols, chunks, embeddings, FTS, blame, docs —
    /// and, for a context-death sweep only, parser failures) for the file ids staged in
    /// `temp.staged_file_ids`, then the files themselves. The caller is responsible for populating
    /// and clearing the temp table. GC's two sweeps share it, distinguished by [`StagedSweep`].
    pub(super) fn delete_staged_files_cascade(&self, sweep: StagedSweep) -> anyhow::Result<()> {
        // GENERATION-LESS TABLE CLASSIFICATION (A6, P2 review): `parser_failures` is keyed by
        // `(repo_id, path)` with NO generation — path-keyed indexer state OWNED at (re)parse time
        // (upsert on failure, clear on clean parse, orphan-path sweep in the rebuild tail). A
        // DEAD-GENERATION sweep must NOT delete it: the staged dead rows share their paths with the
        // LIVE generation, so the path-keyed delete would drop a still-failing live path's only
        // record. Only true CONTEXT death (a dead commit/worktree — the path's last owner in this
        // repo going away) may clear it. The row-value join matches BOTH key columns so a sibling
        // repo's failure at the same path is never clobbered. Runs BEFORE the batch below (it joins
        // the staged `main.files` rows the batch deletes). Everything else in the cascade is keyed
        // by staged file/symbol/chunk ids (per-generation rows) or, for the `logical_symbols`
        // orphan cleanup, by membership — correct under both sweep axes.
        if sweep == StagedSweep::DeadContext {
            self.storage.connection().execute(
                "DELETE FROM main.parser_failures
                 WHERE (repo_id, path) IN (
                     SELECT files.repo_id, files.path
                     FROM main.files
                     JOIN temp.staged_file_ids ON staged_file_ids.id = files.id
                 )",
                [],
            )?;
        }
        self.storage.execute_batch(
            "
            INSERT OR IGNORE INTO main.name_strings(value) VALUES ('unresolved');
            UPDATE main.edges_data
            SET to_symbol_id = NULL,
                target_start_line = NULL,
                target_end_line = NULL,
                resolution_id =
                    (SELECT id FROM main.name_strings WHERE value = 'unresolved')
            WHERE to_symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.edges_data
            WHERE source_file_id IN (SELECT id FROM temp.staged_file_ids)
               OR from_symbol_id IN (
                    SELECT symbols.id
                    FROM main.symbols
                    JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
               );

            DELETE FROM main.logical_symbol_members
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.logical_symbols
            WHERE id NOT IN (
                SELECT logical_symbol_id FROM main.logical_symbol_members
            );
            DELETE FROM main.symbol_facts
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.chunk_fts
            WHERE rowid IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.chunk_summaries
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.chunk_embeddings
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.git_chunk_blame
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.docs
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            -- chunk_text cascades from chunks via FK, but enumerate it explicitly like the other
            -- chunk-child tables (#77): the migration runner toggles foreign_keys = OFF, so a \
             delete
            -- that ran while FK was off would orphan compressed blobs.
            DELETE FROM main.chunk_text
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.symbol_fingerprints
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            -- The token bag rides the symbol_fingerprints row deleted above as the `token_bag`
            -- BLOB column (#231); symbol_token_postings was dropped in V032, so there is no
            -- separate per-token table to cascade here (R1: a DELETE on it would error
            -- `no such table` on every reindex).
            DELETE FROM main.symbols
            WHERE file_id IN (SELECT id FROM temp.staged_file_ids);
            DELETE FROM main.chunks
            WHERE file_id IN (SELECT id FROM temp.staged_file_ids);
            DELETE FROM main.files
            WHERE id IN (SELECT id FROM temp.staged_file_ids);
            ",
        )?;
        Ok(())
    }
}

/// Test seam for the generation-staged rebuild: a barrier the full rebuild runs after each
/// committed file wave, so a concurrent reader can observe the STAGED (committed but not-yet-live)
/// generation mid-rebuild. Compiled out of production entirely; kept at the end of the file so it
/// does not read as a `#[cfg(test)]` module with production items after it
/// (clippy::items_after_test_module).
///
/// KEYED BY DATABASE PATH, never a single process-global slot: the coverage job runs plain
/// `cargo test` under llvm-cov — every test shares ONE process with parallel libtest threads
/// (unlike nextest's process-per-test), so two barrier tests racing a global slot drop each
/// other's hook (and its channel sender → `RecvError`), and a leaked hook could fire inside an
/// UNRELATED test's rebuild. The same class as the #409 flock/fork coverage incident: same-process
/// global state that is safe under nextest breaks under libtest. Keying by the rebuild's database
/// path makes concurrent tests inherently isolated (each uses its own temp DB), and registration
/// returns an RAII [`WaveBarrierGuard`] so the entry is removed even when the test panics.
#[cfg(test)]
mod wave_barrier {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// `Arc`, not `Box`: [`run_after_wave_commit`] clones the hook out of the registry and RELEASES
    /// the map lock before calling it — the hook blocks on its test barrier, and holding the map
    /// lock across that block would serialize (or deadlock) every other test's registration.
    /// `Sync` is required by the shared `Arc`; hook captures that are `!Sync` (channel endpoints)
    /// ride in `Mutex`es.
    pub(crate) type WaveHook = Arc<dyn Fn() + Send + Sync + 'static>;

    static AFTER_WAVE_COMMIT: Mutex<BTreeMap<PathBuf, WaveHook>> = Mutex::new(BTreeMap::new());

    /// Unregisters its database's hook on drop — panic-safe cleanup, so a failing barrier test
    /// can never leak a hook into a stranger's rebuild.
    pub(crate) struct WaveBarrierGuard {
        database: PathBuf,
    }

    impl Drop for WaveBarrierGuard {
        fn drop(&mut self) {
            if let Ok(mut hooks) = AFTER_WAVE_COMMIT.lock() {
                hooks.remove(&self.database);
            }
        }
    }

    /// Register the after-wave-commit hook for the rebuild whose `config.database` is `database`.
    /// Hold the returned guard for the duration of the observed rebuild.
    #[must_use = "dropping the guard unregisters the hook"]
    pub(crate) fn set_after_wave_commit(database: &Path, hook: WaveHook) -> WaveBarrierGuard {
        AFTER_WAVE_COMMIT
            .lock()
            .expect("wave barrier registry poisoned")
            .insert(database.to_path_buf(), hook);
        WaveBarrierGuard { database: database.to_path_buf() }
    }

    /// Invoked by the full-rebuild wave loop after each wave commits, with the rebuilding
    /// connection's database path; fires only a hook registered for THAT database.
    pub(crate) fn run_after_wave_commit(database: &Path) {
        let hook = AFTER_WAVE_COMMIT
            .lock()
            .expect("wave barrier registry poisoned")
            .get(database)
            .cloned();
        if let Some(hook) = hook {
            hook();
        }
    }
}
