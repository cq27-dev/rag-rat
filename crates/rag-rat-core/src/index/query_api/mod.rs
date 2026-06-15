use rusqlite::OptionalExtension;

use super::*;
use crate::index::oracle;

mod graph;
mod history;
mod importance;
mod memory;
mod oracle_runs;

pub use importance::ImportantSymbolsRequest;

impl IndexDatabase {
    pub fn status(&self, database: &Path) -> anyhow::Result<IndexStatus> {
        let mut counts = BTreeMap::new();
        let mut stmt = self
            .storage
            .connection()
            .prepare("SELECT language, COUNT(*) FROM files GROUP BY language ORDER BY language")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        for row in rows {
            let (language, count) = row?;
            counts.insert(language, u64::try_from(count).unwrap_or(0));
        }

        let content_revision = self.content_revision()?;
        let fts_source_revision = self.meta("fts_source_revision")?;
        let fts_dirty = self.fts_dirty()?;

        Ok(IndexStatus {
            database: database.display().to_string(),
            exists: database.exists(),
            schema: schema::status(self.storage.connection())?,
            git_commit: self.meta("git_commit")?,
            git_dirty: self.meta("git_dirty")?.map(|value| value == "true"),
            indexed_at_ms: self.meta("indexed_at_ms")?.and_then(|value| value.parse::<i64>().ok()),
            content_revision: content_revision.clone(),
            fts_synced_at_ms: self
                .meta("fts_synced_at_ms")?
                .and_then(|value| value.parse::<i64>().ok()),
            fts_dirty,
            fts_fresh: !fts_dirty
                && fts_source_revision.as_deref() == Some(content_revision.as_str()),
            fts_source_revision,
            file_count_by_language: counts,
            parser_failures: self.parser_failure_count()?,
            parser_failure_paths: self.parser_failure_paths()?,
            git_history: self.git_history_status()?,
            github: self.github_status()?,
            local_ai: self.local_ai_status()?,
            anchor_health: crate::query::memory::anchor_health_counts(self.storage.connection())
                .unwrap_or_default(),
        })
    }

    /// Read-only count of active repo-memory bindings grouped by anchor_status.
    /// Does not run `memory_validate`; reads persisted anchor_status values only.
    pub fn memory_anchor_health(&self) -> anyhow::Result<AnchorHealth> {
        crate::query::memory::anchor_health_counts(self.storage.connection())
    }

    pub fn storage_status(&self) -> anyhow::Result<StorageStatus> {
        self.storage.status()
    }

    pub fn discovery_status(&self, config: &Config) -> anyhow::Result<DiscoveryStatus> {
        let plan = discovery_plan(self.storage.connection(), config)?;
        let unindexed_source_files =
            plan.unindexed.iter().filter(|file| file.kind == TargetKind::Source).count();
        let unindexed_sample =
            plan.unindexed.iter().take(10).map(|file| path_string(&file.relative_path)).collect();
        let warning = (unindexed_source_files > 0).then(|| {
            format!(
                "{unindexed_source_files} unindexed source files detected. Run `rag-rat index \
                 --full` or `rag-rat index --discover`."
            )
        });
        Ok(DiscoveryStatus {
            discovered_files: plan.discovered_files,
            indexed_files: plan.indexed_files,
            unindexed_files: plan.unindexed.len(),
            unindexed_source_files,
            changed_indexed_files: plan.changed.len(),
            removed_indexed_files: plan.deleted.len(),
            unindexed_sample,
            warning,
        })
    }

    pub fn search(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_with_graph_meta(query, limit, include_generated, GraphMetaMode::Compact, 3)
    }

    pub fn search_explain(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_explain_with_graph_meta(
            query,
            limit,
            include_generated,
            GraphMetaMode::Compact,
            3,
        )
    }

    pub fn search_with_graph_meta(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_with_graph_meta_options(
            query,
            limit,
            include_generated,
            graph_mode,
            graph_limit,
            SearchOptions::default(),
        )
    }

    pub fn search_with_graph_meta_options(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
        options: SearchOptions,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.ensure_fts_fresh()?;
        let mut hits =
            self.search_with_heal(query, limit, include_generated, true, false, options)?;
        graph_meta::attach_to_search_hits(
            self.storage.connection(),
            &mut hits,
            graph_mode,
            graph_limit,
        )?;
        self.enrich_search_hits_with_load_bearing(&mut hits)?;
        Ok(hits)
    }

    pub fn search_explain_with_graph_meta(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_explain_with_graph_meta_options(
            query,
            limit,
            include_generated,
            graph_mode,
            graph_limit,
            SearchOptions::default(),
        )
    }

    pub fn search_explain_with_graph_meta_options(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
        options: SearchOptions,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.ensure_fts_fresh()?;
        let mut hits =
            self.search_with_heal(query, limit, include_generated, true, true, options)?;
        graph_meta::attach_to_search_hits(
            self.storage.connection(),
            &mut hits,
            graph_mode,
            graph_limit,
        )?;
        self.enrich_search_hits_with_load_bearing(&mut hits)?;
        Ok(hits)
    }

    pub fn symbols(
        &self,
        name: &str,
        language: Option<Language>,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::symbol::SymbolHit>> {
        let mut hits =
            crate::query::symbol::lookup(self.storage.connection(), name, language, limit)?;
        self.enrich_symbol_hits_with_load_bearing(&mut hits)?;
        Ok(hits)
    }

    pub fn symbol_candidates(
        &self,
        selector: &crate::query::symbol::SymbolSelector,
    ) -> anyhow::Result<crate::query::symbol::SymbolLookup> {
        let mut lookup =
            crate::query::symbol::lookup_candidates(self.storage.connection(), selector)?;
        // #147: symbol rows aren't anchor-relocated like chunks, so a file edited since indexing
        // returns stale line numbers. Heal the matched files inline (bounded, like
        // search_with_heal) and re-resolve so positions/ids are current; #148: report any
        // file still dirty after.
        let paths: Vec<String> = lookup.candidates.iter().map(|c| c.path.clone()).collect();
        let stale = self.stale_source_paths(&paths)?;
        if !stale.is_empty() {
            self.heal_stale_paths(&stale)?; // NeedsReindex beyond the cap
            let healed =
                crate::query::symbol::lookup_candidates(self.storage.connection(), selector)?;
            // A `symbol_id` selector can't survive a reindex (ids are reassigned per #149), so a
            // re-resolve by the OLD id finds nothing even though the symbol still exists — keep the
            // pre-heal candidates, flagged stale. For a name/symbol_path/logical selector an empty
            // re-resolve means the symbol was genuinely deleted/renamed by the edit, so we must NOT
            // resurrect a ghost with dead ids and old offsets — return the (empty) healed result.
            if healed.candidates.is_empty()
                && !lookup.candidates.is_empty()
                && selector.symbol_id.is_some()
            {
                lookup.stale_files = stale;
            } else {
                lookup = healed;
                let healed_paths: Vec<String> =
                    lookup.candidates.iter().map(|c| c.path.clone()).collect();
                lookup.stale_files = self.stale_source_paths(&healed_paths)?;
            }
        }
        self.enrich_symbol_hits_with_load_bearing(&mut lookup.candidates)?;
        Ok(lookup)
    }

    /// The active-scope file path that defines `qualified_name` (lowest symbol id on a tie), or
    /// `None` if unresolved — used to fold a direct callee's DEFINITION file into impact staleness.
    /// Scoped through the per-connection `files` view, like `active_symbol_id_for_qualified_name`.
    fn file_for_qualified_name(&self, qualified_name: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .storage
            .connection()
            .query_row(
                "SELECT files.path FROM symbols
                 JOIN files ON files.id = symbols.file_id
                 WHERE symbols.qualified_name = ?1
                 ORDER BY symbols.id
                 LIMIT 1",
                [qualified_name],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    }

    /// The indexed `files.sha256` for `path` in the ACTIVE checkout (via the per-connection `files`
    /// scope view), or `None` when the path isn't indexed in this scope.
    fn indexed_sha_for_path(&self, path: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .storage
            .connection()
            .query_row("SELECT sha256 FROM files WHERE path = ?1 LIMIT 1", [path], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }

    /// Of `paths`, those whose on-disk content differs from the indexed sha (or are unreadable) —
    /// results drawn from them may be stale relative to the working tree. Deduped; paths not
    /// indexed in scope are skipped (nothing to be stale against); no source root → empty
    /// (bare/copied index). One file read + hash per distinct path — callers pass the small
    /// result set, not the whole corpus.
    fn stale_source_paths(&self, paths: &[String]) -> anyhow::Result<Vec<String>> {
        let Some(root) = self.storage.source_root().map(Path::to_path_buf) else {
            return Ok(Vec::new());
        };
        let mut stale = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for path in paths {
            if !seen.insert(path.as_str()) {
                continue;
            }
            let Some(indexed) = self.indexed_sha_for_path(path)? else {
                continue;
            };
            match fs::read(root.join(path)) {
                Ok(bytes) if hex_sha256(&bytes) == indexed => {},
                _ => stale.push(path.clone()),
            }
        }
        Ok(stale)
    }

    /// Reindex stale files inline so the next read sees current positions (#147), mirroring
    /// `search_with_heal`: bounded by `MAX_AUTO_HEAL_FILES_PER_CALL` (raises `NeedsReindex` beyond
    /// it so a huge dirty set can't turn a read into an unbounded rebuild), then sync FTS.
    fn heal_stale_paths(&self, stale: &[String]) -> anyhow::Result<()> {
        if stale.is_empty() {
            return Ok(());
        }
        if stale.len() > MAX_AUTO_HEAL_FILES_PER_CALL {
            anyhow::bail!(IndexError::NeedsReindex {
                stale_files: stale.len(),
                cap: MAX_AUTO_HEAL_FILES_PER_CALL,
            });
        }
        for path in stale {
            self.heal_file(Path::new(path))?;
        }
        self.sync_fts()?;
        Ok(())
    }

    pub fn select_symbol(
        &self,
        selector: &crate::query::symbol::SymbolSelector,
    ) -> anyhow::Result<
        Result<Option<crate::query::symbol::SymbolHit>, crate::query::symbol::SymbolDisambiguation>,
    > {
        crate::query::symbol::select_one(self.storage.connection(), selector)
    }

    /// Resolve a selector to a single symbol for `memory rebind`, collapsing a cfg-split / overload
    /// group (all candidates sharing one logical symbol) to one member instead of disambiguating.
    pub fn select_symbol_for_bind(
        &self,
        selector: &crate::query::symbol::SymbolSelector,
    ) -> anyhow::Result<
        Result<Option<crate::query::symbol::SymbolHit>, crate::query::symbol::SymbolDisambiguation>,
    > {
        crate::query::symbol::select_one_for_bind(self.storage.connection(), selector)
    }

    pub fn read_chunk(&self, chunk_id: i64) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        self.read_chunk_with_graph_and_memories(chunk_id, GraphMetaMode::Full, 20, true)
    }

    pub fn read_chunk_with_graph(
        &self,
        chunk_id: i64,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
    ) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        self.read_chunk_with_graph_and_memories(chunk_id, graph_mode, graph_limit, false)
    }

    pub fn read_chunk_with_graph_and_memories(
        &self,
        chunk_id: i64,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
        include_memories: bool,
    ) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        let Some(mut chunk) = self.read_chunk_current(chunk_id)? else {
            return Ok(None);
        };
        graph_meta::attach_to_read_chunk(
            self.storage.connection(),
            &mut chunk,
            graph_mode,
            graph_limit,
        )?;
        if include_memories {
            chunk.memories =
                crate::query::memory::memories_for_chunk(self.storage.connection(), chunk_id, 20)?;
        }
        Ok(Some(chunk))
    }

    fn read_chunk_current(&self, chunk_id: i64) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        let Some(mut chunk) = crate::query::read_chunk(self.storage.connection(), chunk_id)? else {
            return Ok(None);
        };
        let Some(root) = self.storage.source_root() else {
            return Ok(Some(chunk));
        };
        let source_path = root.join(&chunk.path);
        let current_text = match fs::read_to_string(&source_path) {
            Ok(text) => text,
            Err(_) => {
                let path = chunk.path.clone();
                self.mark_file_deleted(Path::new(&path))?;
                self.sync_fts()?;
                anyhow::bail!(IndexError::Gone { chunk_id });
            },
        };
        let anchor = self.chunk_anchor(chunk_id)?;
        let status = anchors::validate(
            &chunk.text,
            usize::try_from(chunk.start_line).unwrap_or(1),
            usize::try_from(chunk.end_line).unwrap_or(1),
            &anchor,
            &current_text,
        );
        match status {
            AnchorStatus::Exact => {
                if let Some(text) = anchors::slice_lines(
                    &current_text,
                    usize::try_from(chunk.start_line).unwrap_or(1),
                    usize::try_from(chunk.end_line).unwrap_or(1),
                ) {
                    chunk.text = text;
                }
                Ok(Some(chunk))
            },
            AnchorStatus::Relocated { start_line, end_line, text } => {
                chunk.start_line = i64::try_from(start_line)?;
                chunk.end_line = i64::try_from(end_line)?;
                chunk.text = text;
                Ok(Some(chunk))
            },
            AnchorStatus::Stale => {
                self.heal_file(Path::new(&chunk.path))?;
                self.sync_fts()?;
                let healed = crate::query::read_chunk(self.storage.connection(), chunk_id)?;
                match healed {
                    Some(chunk) => Ok(Some(chunk)),
                    None => anyhow::bail!(IndexError::StaleChunk { chunk_id, path: chunk.path }),
                }
            },
        }
    }

    pub fn search_hash_baseline(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.ensure_fts_fresh()?;
        crate::search::lexical::search_hash_baseline(
            self.storage.connection(),
            query,
            limit,
            include_generated,
        )
    }

    pub fn docs_for_symbol(&self, symbol: &str, limit: u32) -> anyhow::Result<Vec<SearchHit>> {
        self.search(symbol, limit, true)
    }

    pub fn docs_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let mut hits = self.local_symbol_context_hits(symbol, limit)?;
        hits.extend(self.search(&symbol.name, limit.saturating_mul(4).max(limit), true)?);
        rank_docs_for_symbol(symbol, &mut hits);
        dedupe_search_hits(&mut hits);
        hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(hits)
    }

    pub fn local_ai_status(&self) -> anyhow::Result<LocalAiStatus> {
        ai::status(self.storage.connection())
    }

    pub fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        ai::models(self.storage.connection())
    }

    pub fn install_model(&self, model_id: &str) -> anyhow::Result<ModelInfo> {
        ai::install_model(self.storage.connection(), model_id)
    }

    pub fn reconcile(
        &self,
        limit: Option<u32>,
        batch_size: Option<u32>,
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile(self.storage.connection(), limit, batch_size)
    }

    pub fn reconcile_plan(&self) -> anyhow::Result<ReconcilePlan> {
        ai::reconcile_plan(self.storage.connection())
    }

    pub fn reconcile_with_progress(
        &self,
        limit: Option<u32>,
        batch_size: Option<u32>,
        force: bool,
        progress: impl FnMut(ai::ReconcileProgress),
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile_with_progress(self.storage.connection(), limit, batch_size, force, progress)
    }

    pub fn reconcile_with_options_progress(
        &self,
        options: ai::ReconcileOptions,
        progress: impl FnMut(ai::ReconcileProgress),
    ) -> anyhow::Result<ReconcileReport> {
        ai::reconcile_with_options_progress(self.storage.connection(), options, progress)
    }

    /// Garbage-collect index rows for git contexts that are no longer live. Keeps the active
    /// commit and overlay of every worktree reported by `git worktree list` (plus this
    /// connection's active context) and prunes file/chunk/embedding/symbol/edge rows for any
    /// other commit. Never prunes when no live context can be determined (non-git, git error).
    pub fn gc(&self) -> anyhow::Result<GcReport> {
        let mut live_commits = Vec::new();
        let mut live_worktrees = Vec::new();
        if let Some(root) = self.storage.source_root() {
            let (commits, worktrees) = live_worktree_contexts(root);
            live_commits.extend(commits);
            live_worktrees.extend(worktrees);
        }
        // Always keep this connection's active context, even if git enumeration missed it.
        if !self.active_commit_sha.is_empty() {
            live_commits.push(self.active_commit_sha.clone());
        }
        if !self.active_worktree_id.is_empty() {
            live_worktrees.push(self.active_worktree_id.clone());
        }
        live_commits.sort();
        live_commits.dedup();
        live_worktrees.sort();
        live_worktrees.dedup();
        self.prune_to_live(&live_commits, &live_worktrees)
    }

    /// Prune file rows (and their derived rows) whose `commit_sha` and `worktree_id` are both
    /// outside the live sets. Refuses to prune when both live sets are empty, so a missing
    /// live set never wipes the index. `parser_failures` are keyed by path (shared across
    /// commits) and are regenerated on the next index, so they are not preserved per-commit.
    pub fn prune_to_live(
        &self,
        live_commits: &[String],
        live_worktrees: &[String],
    ) -> anyhow::Result<GcReport> {
        let conn = self.storage.connection();
        let files_before = table_row_count(conn, "files")?;
        let chunks_before = table_row_count(conn, "chunks")?;
        if live_commits.is_empty() && live_worktrees.is_empty() {
            return Ok(GcReport {
                files_pruned: 0,
                chunks_pruned: 0,
                files_remaining: files_before,
                chunks_remaining: chunks_before,
                skipped: true,
            });
        }
        conn.execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS gc_live_commits(sha TEXT PRIMARY KEY);
            DELETE FROM temp.gc_live_commits;
            CREATE TEMP TABLE IF NOT EXISTS gc_live_worktrees(id TEXT PRIMARY KEY);
            DELETE FROM temp.gc_live_worktrees;
            CREATE TEMP TABLE IF NOT EXISTS staged_file_ids(id INTEGER PRIMARY KEY);
            DELETE FROM temp.staged_file_ids;
            ",
        )?;
        {
            let mut stmt =
                conn.prepare("INSERT OR IGNORE INTO temp.gc_live_commits(sha) VALUES (?1)")?;
            for sha in live_commits {
                stmt.execute([sha])?;
            }
        }
        {
            let mut stmt =
                conn.prepare("INSERT OR IGNORE INTO temp.gc_live_worktrees(id) VALUES (?1)")?;
            for id in live_worktrees {
                stmt.execute([id])?;
            }
        }
        // A file survives if its commit is live OR its worktree overlay is live. Empty-string
        // keys never appear in the live sets, so unkeyed rows are pruned.
        conn.execute(
            "
            INSERT OR IGNORE INTO temp.staged_file_ids(id)
            SELECT id FROM main.files
            WHERE commit_sha NOT IN (SELECT sha FROM temp.gc_live_commits)
              AND worktree_id NOT IN (SELECT id FROM temp.gc_live_worktrees)
            ",
            [],
        )?;
        self.delete_staged_files_cascade()?;
        conn.execute_batch("DELETE FROM temp.staged_file_ids;")?;
        // `edge_oracle` verdicts cascade away with their edges via the FK ON DELETE CASCADE (fired
        // by the cascade above, with `PRAGMA foreign_keys=ON`). `oracle_runs`, however, is keyed by
        // `(commit_sha, worktree_id)` directly — nothing cascades it — so a dead checkout's run
        // rows would survive the file pruning. Prune them with the SAME live sets, so a run and the
        // edges it produced are dropped together.
        oracle::prune_oracle_runs_outside_scope(conn, live_commits, live_worktrees)?;
        // Dictionary hygiene (#79): drop `edge_strings` values no edge references any more. The
        // dictionary has NO FKs by design (see the schema comment), so orphans accumulate as
        // edges are pruned; the vocabulary is small, but gc is the natural rate-limited home for
        // the sweep. Every referencing column must appear here — a missed column would null its
        // strings out from under live edges.
        conn.execute(
            "
            DELETE FROM main.edge_strings
            WHERE id NOT IN (
                SELECT from_name_id FROM main.edges_data WHERE from_name_id IS NOT NULL
                UNION SELECT to_name_id FROM main.edges_data
                UNION SELECT target_qualified_name_id FROM main.edges_data
                    WHERE target_qualified_name_id IS NOT NULL
                UNION SELECT receiver_hint_id FROM main.edges_data
                    WHERE receiver_hint_id IS NOT NULL
                UNION SELECT resolution_id FROM main.edges_data
                UNION SELECT edge_kind_id FROM main.edges_data
                UNION SELECT confidence_id FROM main.edges_data
            )
            ",
            [],
        )?;
        let files_remaining = table_row_count(conn, "files")?;
        let chunks_remaining = table_row_count(conn, "chunks")?;
        Ok(GcReport {
            files_pruned: files_before.saturating_sub(files_remaining),
            chunks_pruned: chunks_before.saturating_sub(chunks_remaining),
            files_remaining,
            chunks_remaining,
            skipped: false,
        })
    }

    pub fn current_embedding_count(&self, model_id: &str) -> anyhow::Result<u64> {
        ai::current_embedding_count(self.storage.connection(), model_id)
    }

    pub fn heal_index(&self, limit: Option<u32>) -> anyhow::Result<HealIndexReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("heal_index requires source_root metadata; run `rag-rat index` first");
        };
        let indexed_files = self.indexed_files()?;
        let max_repairs = limit.map(usize::try_from).transpose()?.unwrap_or(usize::MAX);
        let mut report = HealIndexReport {
            checked_files: 0,
            healed_files: 0,
            removed_files: 0,
            skipped_files: 0,
            fts_fresh: false,
            message: None,
        };

        for file in indexed_files {
            report.checked_files += 1;
            let path = Path::new(&file.path);
            let full_path = root.join(path);
            let Ok(text) = fs::read_to_string(&full_path) else {
                if usize::try_from(report.healed_files + report.removed_files).unwrap_or(usize::MAX)
                    >= max_repairs
                {
                    report.message =
                        Some("limit reached; rerun heal_index to continue".to_string());
                    break;
                }
                self.mark_file_deleted(path)?;
                report.removed_files += 1;
                continue;
            };
            let sha256 = hex_sha256(text.as_bytes());
            if sha256 == file.sha256 {
                report.skipped_files += 1;
                continue;
            }
            if usize::try_from(report.healed_files + report.removed_files).unwrap_or(usize::MAX)
                >= max_repairs
            {
                report.message = Some("limit reached; rerun heal_index to continue".to_string());
                break;
            }
            self.heal_file(path)?;
            report.healed_files += 1;
        }

        if report.healed_files > 0 || report.removed_files > 0 {
            self.sync_fts()?;
        } else {
            self.ensure_fts_fresh()?;
        }
        report.fts_fresh = !self.fts_dirty()?;
        Ok(report)
    }

    pub fn repo_brief(
        &self,
        options: crate::query::repo_brief::RepoBriefOptions,
    ) -> anyhow::Result<crate::query::repo_brief::RepoBrief> {
        crate::query::repo_brief::repo_brief(self.storage.connection(), options)
    }

    pub fn repo_clusters(
        &self,
        options: crate::query::clusters::RepoClustersOptions,
    ) -> anyhow::Result<crate::query::clusters::RepoClustersReport> {
        crate::query::clusters::repo_clusters(self.storage.connection(), options)
    }
}

/// `resolved-external(<package>)` for a SCIP symbol that names a dependency outside the corpus, or
/// `None` when it has no package component. Shared by `compare_graph_to_scip` to label a
/// contradiction whose compiler resolution is external.
fn resolved_external_label(scip_symbol: &str) -> Option<String> {
    oracle::package_of(scip_symbol).map(|package| format!("resolved-external({package})"))
}

/// Append a quantitative clause to `summary.completeness_risk` describing how many of the SHOWN
/// neighbors the oracle placed in an external dependency, e.g. " (2 shown neighbors are
/// resolved-external: libc, tokio)". Quantitative completeness is the #69 ask: turn the qualitative
/// risk into a count when the oracle has data. No-op when no hop carries a `resolved-external`
/// verdict (then the risk string stays purely qualitative).
///
/// COUNTING SCOPE (#82 P3): `external_count` is counted over the TRUNCATED `hops` window (the
/// neighbors actually returned), so the clause speaks of "shown neighbors" — it does NOT divide by
/// the population-wide `summary.unresolved + name_only + ambiguous`. Mixing a shown-window
/// numerator with a population-wide denominator produced a misleading ratio (`5 of 7` where 5
/// counts only the displayed window and 7 the whole graph). The honest statement is the count over
/// what was shown.
fn annotate_completeness_with_externals(
    summary: &mut crate::query::graph::GraphTraversalSummary,
    hops: &[crate::query::graph::GraphHop],
) {
    let mut packages: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut external_count = 0u64;
    for hop in hops {
        if let Some(label) = &hop.resolved_external {
            external_count += 1;
            // `resolved-external(<package>)` → `<package>` for the readable list.
            if let Some(package) =
                label.strip_prefix("resolved-external(").and_then(|rest| rest.strip_suffix(')'))
            {
                packages.insert(package.to_string());
            }
        }
    }
    if external_count == 0 {
        return;
    }
    let neighbor_word = if external_count == 1 { "neighbor is" } else { "neighbors are" };
    let package_list = packages.into_iter().collect::<Vec<_>>().join(", ");
    let clause = if package_list.is_empty() {
        format!(" ({external_count} shown {neighbor_word} resolved-external)")
    } else {
        format!(" ({external_count} shown {neighbor_word} resolved-external: {package_list})")
    };
    summary.completeness_risk.push_str(&clause);
}

#[cfg(test)]
mod oracle_surfacing_tests;
