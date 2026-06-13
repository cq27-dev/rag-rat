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

        let mut db = Self::open(&config.database)?;
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        if db.indexed_file_count()? == 0 {
            return Self::rebuild_with_progress(config, progress).map(|db| (db, true));
        }
        progress(IndexProgress::Started { database: config.database.clone(), mode });
        // Gate the git-history reload: `apply_prepared` is a full `git log` re-read (O(total
        // history) — the `--numstat` pass diffs every commit) + 4-table wipe, and it runs on EVERY
        // incremental/watcher pass. Skip it when HEAD/root/shallow are unchanged (the common case:
        // a file edit or idle sweep leaves history untouched). Reloading only on a real change
        // still catches every rewrite, since HEAD's sha is content-addressed. Decide BEFORE
        // spawning `prepare` so an unchanged HEAD never pays the `git log` cost at all.
        let mut git_history = if db.git_history_is_current(&config.root) {
            None
        } else {
            progress(IndexProgress::IndexingGitHistory);
            Some(spawn_git_history_prepare(&config.root))
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
                db.set_meta_if_changed("source_root", &config.root.display().to_string())?;
            db.storage.set_source_root(config.root.clone());
            let git_meta_changed = db.write_git_meta(&config.root)?;
            let (indexed, manifest_in_change_set) = match mode {
                IndexMode::Changed => db.index_changed_files_with_progress(config, progress)?,
                IndexMode::Discover => db.index_discovered_files_with_progress(config, progress)?,
                IndexMode::Full => unreachable!("full mode is handled by rebuild_with_progress"),
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
            let mut mutated = indexed > 0 || healed > 0 || source_root_changed || git_meta_changed;
            // None when the gate above found git history already current — skip the reload.
            if let Some(handle) = git_history.take() {
                db.apply_prepared_git_history(&config.root, handle)?;
                mutated = true;
            }
            // Per-package import scope (#61, salvaging #95's ordering): rewrite `packages` +
            // reassign `files.package_id` + refresh the global `local_crate_roots` union BEFORE the
            // resolve pass, so the resolver sees the current package map. Run it when a file was
            // (re)indexed (a new/edited .rs needs its package_id) OR a Cargo.toml is in the change
            // set (the crate set may have changed) — OUTSIDE the `indexed>0 || healed>0` gate, so a
            // manifest-only change (no Rust file touched, indexed==0) still refreshes (#95 bug:
            // the refresh was nested inside that gate and was skipped, leaving the crate set stale
            // until the next full rebuild). `refresh_packages` returns whether the global union
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
                db.set_meta("indexed_at_ms", &now_ms().to_string())?;
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

    pub fn index_targets(&self, config: &Config) -> anyhow::Result<()> {
        self.index_targets_with_progress(config, &mut |_| {})?;
        Ok(())
    }

    pub(super) fn index_targets_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<usize>
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
        for wave in files.chunks(wave_size) {
            let prepared = prepare_files_with_progress(wave, progress, done, total)?;
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
                // Full rebuild: skip per-row chunk_fts writes; rebuild_fts repopulates it at the
                // end.
                self.insert_prepared_file(prepared_file, false, Some(&mut graph))?;
            }
            // `prepared` (this wave's chunk texts / symbols / edge candidates) drops here.
        }
        // Per-package import scope (#61): write `packages` + `files.package_id` + the global
        // `local_crate_roots` union now — files are inserted, but the resolve below has not run, so
        // it reads the fresh package assignment. (`set_context` installs the `files` scope view at
        // open, so the scoped reads in the resolve see these rows.)
        self.refresh_packages(&config.root)?;
        edges::resolve_and_insert_edges(self.storage.connection(), graph)?;

        Ok(total)
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

    fn assign_file_scopes(
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
            let mut stmt = conn.prepare(
                "SELECT id, path, sha256 FROM main.files
                 WHERE worktree_id = ?1 AND worktree_id != '' AND kind != 'deleted'",
            )?;
            let rows = stmt.query_map([&self.active_worktree_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut healed = 0usize;
        for (file_id, path, sha) in overlays {
            if changes.changed.contains(Path::new(&path)) {
                continue; // genuinely dirty — the overlay is the canonical row
            }
            let committed_exists: bool = self.storage.connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM main.files
                 WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = '')",
                params![path, self.active_commit_sha],
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

    fn apply_incremental_file_plan<F>(
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
            // Incremental: per-file replace, so keep chunk_fts synced in place (no full
            // rebuild_fts). No accumulator — edges are inserted unresolved here and
            // resolved by resolve_edges.
            self.insert_prepared_file(prepared_file, true, None)?;
        }

        Ok(files.len() + deleted_count)
    }
}

/// Whether any path in an iterator is a `Cargo.toml` — the gate for the per-package refresh on an
/// incremental pass (#61, salvaging #95). Checked against both changed and deleted paths so a crate
/// removal also triggers the refresh.
fn paths_include_cargo_toml<'a>(mut paths: impl Iterator<Item = &'a Path>) -> bool {
    paths.any(|path| path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml"))
}
