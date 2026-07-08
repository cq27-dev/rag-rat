use super::*;

impl IndexDatabase {
    pub fn index_changed(config: &Config) -> anyhow::Result<Self> {
        Self::index_changed_with_progress(config, |_| {})
    }

    pub fn index_changed_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        Self::index_incremental_with_progress(config, IndexMode::Changed, &mut progress)
            .map(|(db, _)| db)
    }

    pub fn index_discover(config: &Config) -> anyhow::Result<Self> {
        Self::index_discover_with_progress(config, |_| {})
    }

    pub fn index_discover_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        Self::index_incremental_with_progress(config, IndexMode::Discover, &mut progress)
            .map(|(db, _)| db)
    }

    /// Like [`Self::index_discover`], but also reports whether the pass changed index *content*
    /// (a file was added / edited / removed). The watch loop uses this to skip the
    /// reconcile / memory-validate tail on an idle no-change sweep (issue #63).
    pub fn index_discover_reporting(config: &Config) -> anyhow::Result<(Self, bool)> {
        Self::index_incremental_with_progress(config, IndexMode::Discover, &mut |_| {})
    }

    fn index_incremental_with_progress<F>(
        config: &Config,
        mode: IndexMode,
        progress: &mut F,
    ) -> anyhow::Result<(Self, bool)>
    where
        F: FnMut(IndexProgress),
    {
        if !config.database.exists() {
            return Self::rebuild_with_progress(config, progress).map(|db| (db, true));
        }
        if Self::migration_check(&config.database)?.state == schema::SchemaState::Missing {
            return Self::rebuild_with_progress(config, progress).map(|db| (db, true));
        }

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
            return Self::rebuild_with_progress(config, progress).map(|db| (db, true));
        }
        // A commit advances HEAD before committed-scope rows have been stamped for it. Likewise, a
        // target-set change can make the active scope marker stale even when the old rows still
        // make the counts match. Neither is a complete changed-file pass: discover the tree
        // and restamp the generation for the active HEAD/target fingerprint (#459 review).
        let active_scope_incomplete =
            !active_base_scope_discovered || scoped_file_count < repo_generation_file_count;
        let effective_mode = if active_scope_incomplete && mode == IndexMode::Changed {
            IndexMode::Discover
        } else {
            mode
        };
        progress(IndexProgress::Started {
            database: config.database.clone(),
            mode: effective_mode,
        });
        // Gate the git-history reload. Unchanged HEAD/root/shallow skips it entirely; a
        // fast-forward HEAD prepares only the new range (`old..new`) and appends rows; uncertainty,
        // shallow history, root drift, and non-fast-forward rewrites prepare the full history. The
        // append plan is revalidated after BEGIN IMMEDIATE because preparation happens before this
        // writer owns the lock.
        let mut git_history = if db.git_history_is_current(&config.root) {
            None
        } else {
            progress(IndexProgress::IndexingGitHistory);
            let plan = git_history::prepare_plan(db.storage.connection(), &config.root);
            Some(spawn_git_history_prepare_with_plan(&config.root, plan))
        };
        let result = (|| -> anyhow::Result<bool> {
            // BEGIN IMMEDIATE: acquire the write lock up front so a racing writer waits out
            // busy_timeout instead of failing the deferred read→write upgrade with SQLITE_BUSY.
            db.storage.execute_batch("BEGIN IMMEDIATE")?;
            // Write meta only when it actually changed, and track whether this pass mutated
            // anything at all. A periodic sweep or a spurious event over an unchanged tree must
            // NOT churn the WAL with a timestamp-only write + COMMIT (issue #63) — that idle write
            // is also exactly the false signal the watcher-loop diagnostic keys on
            // (indexed_at_ms advancing while content is unchanged).
            let source_root_changed =
                db.set_repo_meta_if_changed("source_root", &config.root.display().to_string())?;
            db.storage.set_source_root(config.root.clone());
            let git_meta_changed = db.write_git_meta(&config.root)?;
            // Heal the active embedding model from config INSIDE the txn: the incremental /
            // maintenance / watch path opens config-blind via `Self::open` and can't seed on its
            // own, so a pre-#394 index (active unset or still provisional) would
            // otherwise keep reconciling via the hash fallback here. Doing it in-txn
            // means a failed pass rolls the reseed back with everything else, rather
            // than stranding the active model on a possibly-uninstalled configured
            // model (#394 review). A no-op unless a seed is owed, preserving the idle-pass
            // no-write invariant (#63); when owed it counts as a mutation so the COMMIT persists
            // it.
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
            let (indexed, manifest_in_change_set) = match effective_mode {
                IndexMode::Changed => db.index_changed_files_with_progress(config, progress)?,
                IndexMode::Discover => db.index_discovered_files_with_progress(config, progress)?,
                IndexMode::Full => unreachable!("full mode is handled by rebuild_with_progress"),
            };
            let base_scope_discovery_marked = if effective_mode == IndexMode::Discover {
                db.mark_active_base_scope_discovered(&config.targets)?
            } else {
                false
            };
            // Self-heal stale worktree-overlay rows (#87): a dirty-then-committed file's overlay
            // row otherwise lingers forever, shadowing its (correct) committed row in every
            // scoped read and colliding with a later full rebuild. Runs AFTER the indexing step
            // so a freshly discovered committed row can take over. Read-only when there is
            // nothing to heal, preserving the idle-pass no-write invariant (#63).
            let healed = match git_changed_paths(&config.root) {
                Ok(changes) => db.heal_stale_overlay_rows(&changes)?,
                // Non-git roots have no commit scope; overlays are their canonical rows.
                Err(_) => 0,
            };
            let mut mutated = indexed > 0
                || healed > 0
                || source_root_changed
                || git_meta_changed
                || embedding_model_seeded
                || base_scope_discovery_marked;
            // None when the gate above found git history already current — skip the reload.
            if let Some(handle) = git_history.take() {
                db.apply_prepared_git_history(&config.root, handle)?;
                mutated = true;
            }
            // Per-package import scope (#61, salvaging #95's ordering): rewrite `packages` +
            // refresh the global `local_crate_roots` union BEFORE the resolve pass, so
            // the resolver sees the current package map (the file→package mapping is
            // then computed at resolve LOAD time from those rows — there is no
            // persisted `files.package_id` to reassign). Run it when a Cargo.toml is in
            // the change set (the crate set may have changed) OR a file was (re)indexed
            // — OUTSIDE the `indexed>0 || healed>0` gate, so a manifest-only change (no
            // Rust file touched, indexed==0) still refreshes (#95 bug: the refresh was nested
            // inside that gate and was skipped, leaving the crate set stale until the
            // next full rebuild). `refresh_packages` returns whether the package map
            // changed, which forces a re-resolve even when no file was indexed.
            let roots_changed = if indexed > 0 || healed > 0 || manifest_in_change_set {
                db.refresh_packages(&config.root)?
            } else {
                false
            };
            if roots_changed {
                mutated = true;
            }
            // Healing can delete overlay symbols (NULLing their in-edges via
            // `remove_file_in_scope`), so it needs the same re-derive tail as real file changes.
            // Also re-resolve when the crate set changed but no file was indexed (a manifest-only
            // change) so `use new_crate::X` resolves correctly (#95).
            if indexed > 0 || healed > 0 || roots_changed {
                progress(IndexProgress::RebuildingLogicalSymbols);
                db.rebuild_logical_symbols()?;
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
                // writing, so an idle server does not touch the DB.
                db.storage.execute_batch("ROLLBACK")?;
            }
            progress(IndexProgress::Finished { files: indexed });
            // Report whether index *content* changed (files added / edited / removed, or stale
            // overlays healed — symbols move scope), so the watch loop can skip the
            // reconcile / memory-validate tail on an idle sweep.
            Ok(indexed > 0 || healed > 0)
        })();
        if result.is_err() {
            if let Some(handle) = git_history.take() {
                let _ = join_git_history_prepare(handle);
            }
            let _ = db.storage.execute_batch("ROLLBACK");
        }
        let content_changed = result?;
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
            self.rebuild_logical_symbols()?;
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

    /// Returns `(file_count, manifest_in_change_set)`. The manifest flag is true when any changed
    /// or deleted path is a `Cargo.toml`, signalling the workspace crate set may have changed
    /// and the `packages` map should be refreshed before the resolve pass (#61, salvaging #95).
    fn index_changed_files_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<(usize, bool)>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        let changes = git_changed_paths(&config.root)?;
        let manifest_in_change_set =
            paths_include_cargo_toml(changes.changed.iter().map(PathBuf::as_path))
                || paths_include_cargo_toml(changes.deleted.iter().map(PathBuf::as_path));
        let files = collect_changed_index_files(config, &changes)?;
        let files = self.assign_file_scopes(files, &changes);
        let count = self.apply_incremental_file_plan(files, changes.deleted, progress)?;
        Ok((count, manifest_in_change_set))
    }

    /// Returns `(file_count, manifest_in_change_set)`. The manifest flag also consults the
    /// discovery plan's file list, so a NEW (untracked, not-yet-committed) `Cargo.toml` is
    /// caught even though git status would not list it as changed (#61, salvaging #95).
    fn index_discovered_files_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<(usize, bool)>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        let plan = discovery_plan(self.storage.connection(), config)?;
        let changes = git_changed_paths(&config.root).unwrap_or_default();
        let manifest_in_change_set =
            paths_include_cargo_toml(changes.changed.iter().map(PathBuf::as_path))
                || paths_include_cargo_toml(changes.deleted.iter().map(PathBuf::as_path))
                || paths_include_cargo_toml(
                    plan.files.iter().map(|file| file.relative_path.as_path()),
                );
        let files = self.assign_file_scopes(plan.files, &changes);
        let count = self.apply_incremental_file_plan(files, plan.deleted, progress)?;
        Ok((count, manifest_in_change_set))
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
    pub(super) fn heal_stale_overlay_rows(
        &self,
        changes: &GitChangedPaths,
    ) -> anyhow::Result<usize> {
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
        let mut healed = 0usize;
        for (file_id, path, sha) in overlays {
            if changes.changed.contains(Path::new(&path)) {
                continue; // genuinely dirty — the overlay is the canonical row
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

    pub(super) fn apply_incremental_file_plan<F>(
        &self,
        files: Vec<IndexFile>,
        deleted: BTreeSet<PathBuf>,
        progress: &mut F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovered { files: files.len() });

        let deleted_count = deleted.len();
        for path in deleted {
            self.mark_file_deleted(&path)?;
        }

        let prepared = prepare_files_with_progress(&files, progress, 0, files.len())?;
        for (index, prepared_file) in prepared.iter().enumerate() {
            let current = index + 1;
            if should_report_file_progress(current, files.len()) {
                progress(IndexProgress::IndexingFile {
                    current,
                    total: files.len(),
                    path: prepared_file.file.relative_path.clone(),
                    language: prepared_file.file.language,
                    kind: prepared_file.file.kind,
                });
            }
            self.remove_file_in_scope(
                &prepared_file.file.relative_path,
                &prepared_file.file.commit_sha,
                &prepared_file.file.worktree_id,
            )?;
            // Incremental: per-file replace; chunk_fts is kept synced in place by the inline write
            // in insert_chunks (no full rebuild_fts). No accumulator — edges are inserted
            // unresolved here and resolved by resolve_edges.
            self.insert_prepared_file(prepared_file, None)?;
        }

        Ok(files.len() + deleted_count)
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
