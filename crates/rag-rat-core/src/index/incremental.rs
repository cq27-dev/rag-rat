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
            let indexed = match mode {
                IndexMode::Changed => db.index_changed_files_with_progress(config, progress)?,
                IndexMode::Discover => db.index_discovered_files_with_progress(config, progress)?,
                IndexMode::Full => unreachable!("full mode is handled by rebuild_with_progress"),
            };
            let mut mutated = indexed > 0 || source_root_changed || git_meta_changed;
            // None when the gate above found git history already current — skip the reload.
            if let Some(handle) = git_history.take() {
                db.apply_prepared_git_history(&config.root, handle)?;
                mutated = true;
            }
            if indexed > 0 {
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
            // Report whether index *content* changed (files added / edited / removed), so the
            // watch loop can skip the reconcile / memory-validate tail on an idle sweep.
            Ok(indexed > 0)
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
        edges::resolve_and_insert_edges(self.storage.connection(), graph)?;

        Ok(total)
    }

    fn index_changed_files_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        let changes = git_changed_paths(&config.root)?;
        let files = collect_changed_index_files(config, &changes)?;
        let files = self.assign_file_scopes(files, &changes);
        self.apply_incremental_file_plan(files, changes.deleted, progress)
    }

    fn index_discovered_files_with_progress<F>(
        &self,
        config: &Config,
        progress: &mut F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Discovering);
        let plan = discovery_plan(self.storage.connection(), config)?;
        let changes = git_changed_paths(&config.root).unwrap_or_default();
        let files = self.assign_file_scopes(plan.files, &changes);
        self.apply_incremental_file_plan(files, plan.deleted, progress)
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
