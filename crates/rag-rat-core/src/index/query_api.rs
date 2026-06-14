use super::*;
use crate::index::oracle::{
    self, OracleEvalMetrics, OracleReport, OracleStatus, OracleTool, RecallCalls,
};

impl IndexDatabase {
    /// Run a SCIP-oracle pass from a pre-built `.scip` over the current (active commit/worktree)
    /// edge candidates, writing `edge_oracle` verdicts. The heuristic resolution on the `edges`
    /// row is never touched. Phase 1 (#68): eval-only, no CLI/MCP surface. Requires a `source_root`
    /// (the checkout whose bytes back the SCIP document position-encoding conversion).
    /// `production_sha` is the per-document disk-hash snapshot a tool-driven run captured the
    /// instant its `.scip` was produced (`Some`), arming the scip-vs-disk content gate (#82
    /// TOCTOU); a pre-built `--scip` has no production moment and passes `None`.
    /// `pre_spawn_sha` is the indexed-sha snapshot taken before the tool subprocess was spawned
    /// (see [`Self::oracle_pre_spawn_snapshot`]), arming the pre-spawn gate that covers the
    /// subprocess interior (#83); a pre-built `--scip` has no spawn and passes `None`.
    pub fn run_oracle(
        &self,
        tool: OracleTool,
        tool_version: &str,
        scip_bytes: &[u8],
        production_sha: Option<&std::collections::HashMap<String, String>>,
        pre_spawn_sha: Option<&std::collections::HashMap<String, String>>,
    ) -> anyhow::Result<OracleReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!(
                "index has no source_root metadata; rebuild required for the oracle pass"
            );
        };
        let root = root.to_path_buf();
        oracle::run_oracle(
            self.storage.connection(),
            tool,
            tool_version,
            &self.active_commit_sha,
            &self.active_worktree_id,
            scip_bytes,
            &root,
            production_sha,
            pre_spawn_sha,
        )
    }

    /// The active checkout's indexed `(path -> files.sha256)` map — the pre-spawn snapshot the
    /// CLI takes BEFORE spawning the oracle tool (and before acquiring the index write lock; this
    /// is a cheap read-only query), so the join can reject any document the watcher reindexed
    /// across the entire spawn → join window (#83).
    pub fn oracle_pre_spawn_snapshot(
        &self,
    ) -> anyhow::Result<std::collections::HashMap<String, String>> {
        oracle::pre_spawn_snapshot(
            self.storage.connection(),
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }

    /// Heuristic-vs-oracle eval metrics (precision/recall/recovery) for a tool/version, diffing the
    /// persisted `edge_oracle` rows against the `edges` heuristic. [`RecallCalls`] is the
    /// `(covered_calls, oracle_only_calls)` pair reported by the most recent [`run_oracle`] — both
    /// occurrence-counted over the call population, so recall compares like with like.
    pub fn oracle_eval_metrics(
        &self,
        tool: OracleTool,
        tool_version: &str,
        recall_calls: RecallCalls,
    ) -> anyhow::Result<OracleEvalMetrics> {
        oracle::oracle_eval_metrics(
            self.storage.connection(),
            tool,
            tool_version,
            &self.active_commit_sha,
            &self.active_worktree_id,
            recall_calls,
        )
    }

    /// Persisted oracle status (verdict counts + last run) for a tool/version, scoped to this
    /// database's active `(commit_sha, worktree_id)` checkout.
    pub fn oracle_status(
        &self,
        tool: OracleTool,
        tool_version: &str,
    ) -> anyhow::Result<OracleStatus> {
        oracle::oracle_status(
            self.storage.connection(),
            tool,
            tool_version,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }

    /// `rag-rat oracle run [--tool <id>]` without a pre-built `--scip`: invoke the indexer to
    /// produce a `.scip` (to a caller-owned temp path), then run the phase-1 join over it. A
    /// missing or unrunnable tool returns [`oracle::OracleRunOutcome::Blocked`] with an install
    /// hint (the CLI prints it and exits 0) — never an error. Records an `oracle_runs` row on
    /// success.
    pub fn run_oracle_with_tool(
        &self,
        tool: OracleTool,
        scip_output: &Path,
    ) -> anyhow::Result<oracle::OracleRunOutcome> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!(
                "index has no source_root metadata; rebuild required for the oracle pass"
            );
        };
        let root = root.to_path_buf();
        oracle::run_oracle_with_tool(
            self.storage.connection(),
            tool,
            &root,
            scip_output,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }

    /// Run the oracle pass over a PRE-BUILT `.scip` for a tool, recording an `oracle_runs` row. The
    /// `--scip <path>` consumption path of `oracle run`; deterministic (no subprocess), so it's the
    /// tested end-to-end seam. `tool_version` labels the run (content-addressed staleness key).
    pub fn run_oracle_from_scip(
        &self,
        tool: OracleTool,
        tool_version: &str,
        scip_bytes: &[u8],
    ) -> anyhow::Result<oracle::OracleReport> {
        // A pre-built `--scip` carries no production moment or spawn we control, so neither the
        // scip-vs-disk nor the pre-spawn gate can arm — only the index-vs-disk gate applies.
        self.run_oracle(tool, tool_version, scip_bytes, None, None)
    }

    /// Probe whether an oracle tool is installed, for `oracle status`. A `Blocked` probe is
    /// informational (the tool isn't installed), never an error.
    pub fn probe_oracle_tool(&self, tool: OracleTool) -> oracle::ToolAvailability {
        oracle::probe_oracle_tool(tool)
    }

    /// The `tool_version` of the most recent oracle run for `tool` in this checkout, or `None` when
    /// no run exists. The version `oracle status` reports verdict counts against.
    pub fn latest_oracle_run_version(&self, tool: OracleTool) -> anyhow::Result<Option<String>> {
        oracle::latest_run_tool_version(
            self.storage.connection(),
            tool,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )
    }

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
        Ok(hits)
    }

    pub fn symbols(
        &self,
        name: &str,
        language: Option<Language>,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::symbol::SymbolHit>> {
        crate::query::symbol::lookup(self.storage.connection(), name, language, limit)
    }

    pub fn symbol_candidates(
        &self,
        selector: &crate::query::symbol::SymbolSelector,
    ) -> anyhow::Result<crate::query::symbol::SymbolLookup> {
        crate::query::symbol::lookup_candidates(self.storage.connection(), selector)
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

    pub fn commit_search(&self, query: &str, limit: u32) -> anyhow::Result<Vec<CommitSearchHit>> {
        git_history::commit_search(self.storage.connection(), query, limit)
    }

    pub fn git_history_for_path(
        &self,
        path: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<PathHistoryItem>> {
        git_history::history_for_path(self.storage.connection(), path, limit)
    }

    pub fn git_history_for_symbol(
        &self,
        symbol: &str,
        language: Option<Language>,
        limit: u32,
    ) -> anyhow::Result<Vec<SymbolHistoryItem>> {
        let symbols = self.symbols(symbol, language, limit)?;
        let per_symbol_limit = limit.max(1);
        let mut out = Vec::new();
        for symbol_hit in symbols {
            for commit in self.git_history_for_path(&symbol_hit.path, per_symbol_limit)? {
                out.push(SymbolHistoryItem {
                    symbol: symbol_hit.name.clone(),
                    qualified_name: symbol_hit.qualified_name.clone(),
                    path: symbol_hit.path.clone(),
                    start_byte: symbol_hit.start_byte,
                    end_byte: symbol_hit.end_byte,
                    commit,
                    evidence_kind: "historical",
                });
                if out.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    pub fn commits_touching_query(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<QueryCommitHit>> {
        let current_hits = self.search(query, limit, true)?;
        git_history::commits_touching_query(self.storage.connection(), query, limit, &current_hits)
    }

    pub fn git_blame_chunk(&self, chunk_id: i64) -> anyhow::Result<Option<ChunkBlameSummary>> {
        let Some(chunk) = self.read_chunk(chunk_id)? else {
            return Ok(None);
        };
        let source_text_hash = git_history::source_text_hash(&chunk.text);
        if let Some(cached) =
            git_history::cached_blame(self.storage.connection(), chunk_id, &source_text_hash)?
        {
            return Ok(Some(cached));
        }
        let Some(root) = self.storage.source_root() else {
            return Ok(Some(ChunkBlameSummary {
                chunk_id,
                path: chunk.path,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                source_text_hash,
                line_count: 0,
                dominant_commit: None,
                dominant_commit_lines: 0,
                newest_commit: None,
                newest_commit_time_s: None,
                oldest_commit: None,
                oldest_commit_time_s: None,
                commit_counts: BTreeMap::new(),
                evidence_kind: "historical",
            }));
        };
        let blame_lines =
            git_history::blame_lines(root, &chunk.path, chunk.start_line, chunk.end_line);
        let mut counts = BTreeMap::<String, i64>::new();
        let mut newest = None::<(String, i64)>;
        let mut oldest = None::<(String, i64)>;
        for line in &blame_lines {
            *counts.entry(line.commit.clone()).or_default() += 1;
            if let Some(time) = line.author_time_s {
                if newest.as_ref().is_none_or(|(_, newest_time)| time > *newest_time) {
                    newest = Some((line.commit.clone(), time));
                }
                if oldest.as_ref().is_none_or(|(_, oldest_time)| time < *oldest_time) {
                    oldest = Some((line.commit.clone(), time));
                }
            }
        }
        let dominant = counts
            .iter()
            .max_by_key(|(commit, count)| (*count, *commit))
            .map(|(commit, count)| (commit.clone(), *count));
        let summary = ChunkBlameSummary {
            chunk_id,
            path: chunk.path,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            source_text_hash,
            line_count: i64::try_from(blame_lines.len()).unwrap_or(i64::MAX),
            dominant_commit: dominant.as_ref().map(|(commit, _)| commit.clone()),
            dominant_commit_lines: dominant.map(|(_, count)| count).unwrap_or(0),
            newest_commit: newest.as_ref().map(|(commit, _)| commit.clone()),
            newest_commit_time_s: newest.as_ref().map(|(_, time)| *time),
            oldest_commit: oldest.as_ref().map(|(commit, _)| commit.clone()),
            oldest_commit_time_s: oldest.as_ref().map(|(_, time)| *time),
            commit_counts: counts,
            evidence_kind: "historical",
        };
        git_history::store_blame(self.storage.connection(), &summary)?;
        Ok(Some(summary))
    }

    pub fn github_sync_from_refs(&self, offline: bool) -> anyhow::Result<GitHubSyncReport> {
        self.github_sync_from_refs_with_progress(offline, |_| {})
    }

    pub fn github_sync_from_refs_with_progress(
        &self,
        offline: bool,
        progress: impl FnMut(github::GitHubSyncProgress),
    ) -> anyhow::Result<GitHubSyncReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("index has no source_root metadata; rebuild required");
        };
        if offline {
            github::sync_from_refs::<github::GhCliGitHubClient>(
                self.storage.connection(),
                root,
                None,
                true,
                &self.github,
            )
        } else {
            let client = github::GhCliGitHubClient;
            github::sync_from_refs_with_progress(
                self.storage.connection(),
                root,
                Some(&client),
                false,
                &self.github,
                progress,
            )
        }
    }

    pub fn github_sync_issue(
        &self,
        issue_ref: &str,
        offline: bool,
    ) -> anyhow::Result<GitHubSyncReport> {
        if offline {
            github::sync_issue::<github::GhCliGitHubClient>(
                self.storage.connection(),
                issue_ref,
                None,
                true,
                &self.github,
            )
        } else {
            let client = github::GhCliGitHubClient;
            github::sync_issue(
                self.storage.connection(),
                issue_ref,
                Some(&client),
                false,
                &self.github,
            )
        }
    }

    pub fn github_issue_search(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<GitHubEvidence>> {
        github::issue_search(self.storage.connection(), query, limit)
    }

    pub fn rationale_search(&self, query: &str, limit: u32) -> anyhow::Result<Vec<GitHubEvidence>> {
        github::rationale_search(self.storage.connection(), query, limit, &self.github)
    }

    pub fn github_refs_for_path(
        &self,
        path: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<github::GitHubRef>> {
        github::refs_for_path(self.storage.connection(), path, limit)
    }

    pub fn github_sync_status(&self) -> anyhow::Result<GitHubStatus> {
        self.github_status()
    }

    pub fn papertrail_for_chunk(
        &self,
        chunk_id: i64,
        limit: u32,
    ) -> anyhow::Result<Option<Papertrail>> {
        let Some(chunk) = self.read_chunk(chunk_id)? else {
            return Ok(None);
        };
        Ok(Some(github::papertrail_for_chunk(
            self.storage.connection(),
            &chunk,
            limit,
            &self.github,
        )?))
    }

    pub fn papertrail_for_symbol(
        &self,
        symbol: &str,
        language: Option<Language>,
        limit: u32,
    ) -> anyhow::Result<Option<Papertrail>> {
        let Some(symbol) = self.symbols(symbol, language, limit)?.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(github::papertrail_for_symbol(
            self.storage.connection(),
            &symbol,
            limit,
            &self.github,
        )?))
    }

    pub fn papertrail_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Papertrail> {
        github::papertrail_for_symbol(self.storage.connection(), symbol, limit, &self.github)
    }

    pub fn papertrail_for_commit(
        &self,
        commit_hash: &str,
        limit: u32,
    ) -> anyhow::Result<Papertrail> {
        github::papertrail_for_commit(self.storage.connection(), commit_hash, limit, &self.github)
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

    pub fn ffi_surface(&self, limit: u32) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::ffi_surface(self.storage.connection(), limit)
    }

    pub fn find_callers(
        &self,
        symbol: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        crate::query::graph::traverse(self.storage.connection(), symbol, true, limit)
    }

    pub fn find_callers_with_options(
        &self,
        symbol: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        let options = self.graph_options_with_logical_group(options)?;
        self.traverse_with_oracle(symbol, true, limit, &options)
    }

    /// Upgrade graph hops to the `Compiler` confidence tier where a CURRENT, in-scope `edge_oracle`
    /// verdict covers the edge — the read-side surfacing for `trace_callees` / `find_callers` /
    /// `impact_surface`. The heuristic `edges` row is NEVER mutated (side-table invariant); this
    /// JOINs `edge_oracle` (scoped to the active checkout AND filtered to current content via
    /// `file_sha == files.sha256`) at read time and rewrites only the in-memory hop's display
    /// fields:
    /// - `Upgrade`/`Confirm` (in-corpus compiler resolution) → `confidence = "compiler"` with
    ///   `resolution_reason = "scip:<tool>@<version>"`. A drifted file's verdict is excluded by the
    ///   current-content filter, so the hop reverts to heuristic display (never `compiler`).
    /// - `ResolvedExternal` → `resolved_external = "resolved-external(<package>)"` + the reason;
    ///   the confidence stays heuristic (the callee is outside the corpus, not an in-corpus
    ///   upgrade).
    /// - `Contradict` is NOT surfaced as `compiler`: the oracle disagrees with the heuristic
    ///   target, so promoting it would assert a resolution we don't stand behind — it stays
    ///   heuristic (`compare_graph_to_scip` is where contradictions surface).
    ///
    /// Verdicts bind to whichever files are the ACTIVE version of the checkout, not to
    /// commit-scoped rows specifically: on a clean tree that's the committed `(sha,'')` rows;
    /// on a dirty tree the worktree-dirty `('',wt)` overlay rows shadow them. The shared
    /// active-checkout predicate (the #82 P0 fix) selects exactly one version per file, so a
    /// verdict surfaces only for the version in play — a verdict written against a file that
    /// has since been overlaid (or that belongs to a non-active checkout) drops out of scope
    /// and the hop reverts to heuristic, with no special-casing.
    /// Returns whether any hop was PROMOTED to the `compiler` tier — i.e. whether enrichment
    /// changed a hop's `effective_confidence_rank`. Only an `Upgrade`/`Confirm` that became
    /// `compiler` changes ranking; a `ResolvedExternal` sets a label but leaves the confidence
    /// heuristic, so it does NOT count. The caller uses this to decide whether the
    /// overfetch+re-sort is needed: with no promotion the heuristic order + the caller's
    /// original `limit` are already correct and must be left untouched (#82 P2 — the
    /// unconditional re-sort changed truncation membership on EVERY query, including repos with
    /// no oracle run).
    fn enrich_hops_with_oracle(
        &self,
        hops: &mut [crate::query::graph::GraphHop],
    ) -> anyhow::Result<bool> {
        if hops.is_empty() {
            return Ok(false);
        }
        let tool = oracle::OracleTool::RustAnalyzer;
        let Some(tool_version) = oracle::latest_run_tool_version(
            self.storage.connection(),
            tool,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )?
        else {
            // No oracle run for this checkout — nothing to surface, all hops stay heuristic.
            return Ok(false);
        };
        let edge_ids = hops.iter().map(|hop| hop.edge_id).collect::<Vec<_>>();
        let verdicts = oracle::current_oracle_verdicts_for_edges(
            self.storage.connection(),
            tool,
            &tool_version,
            &self.active_commit_sha,
            &self.active_worktree_id,
            &edge_ids,
        )?;
        if verdicts.is_empty() {
            return Ok(false);
        }
        let mut promoted_any = false;
        for hop in hops.iter_mut() {
            let Some(verdict) = verdicts.get(&hop.edge_id) else {
                continue;
            };
            match verdict.kind {
                oracle::OracleResolutionKind::Upgrade | oracle::OracleResolutionKind::Confirm => {
                    // Hydrate the hop's target from the compiler's `resolved_symbol_id` BEFORE
                    // promoting to `compiler` — for an `Upgrade` on a `NameOnly`/`Ambiguous` edge
                    // the heuristic target is missing or wrong, so promoting confidence without
                    // moving the target would attach the compiler tier to a heuristic/absent
                    // target (#82 finding 2). If the resolved symbol can't be surfaced (it was
                    // deleted/reinserted, so its qualified name is gone — though the def-drift gate
                    // in `edge_oracle_current_predicate` already filters that case), do NOT
                    // promote.
                    let Some(resolved_name) = verdict.resolved_qualified_name.clone() else {
                        continue;
                    };
                    hop.to_symbol = Some(resolved_name.clone());
                    hop.target_qualified_name = Some(resolved_name);
                    hop.verified_target_symbol = true;
                    hop.confidence = "compiler".to_string();
                    hop.resolution_reason = Some(verdict.resolution_reason());
                    promoted_any = true;
                },
                oracle::OracleResolutionKind::ResolvedExternal => {
                    hop.resolved_external = verdict.resolved_external_label();
                    hop.resolution_reason = Some(verdict.resolution_reason());
                },
                // The oracle disagrees with the heuristic target — do not promote to `compiler`.
                oracle::OracleResolutionKind::Contradict => {},
            }
        }
        Ok(promoted_any)
    }

    /// Traverse, surface the `Compiler` tier, then rank-and-truncate so a compiler-upgraded edge is
    /// never dropped by the heuristic limit (#82 finding 4).
    ///
    /// The heuristic `traverse_with_options` orders by heuristic confidence and applies `LIMIT`
    /// BEFORE oracle enrichment runs — so a low-confidence edge the compiler would upgrade to
    /// `compiler` (the tier ABOVE `exact`) can fall below the cutoff and never be fetched, even
    /// though it should outrank the `exact`/`syntactic` neighbors that displaced it. To fix the
    /// ordering we OVERFETCH (traverse with an inflated cap), enrich the larger candidate set,
    /// RE-SORT by EFFECTIVE confidence (`compiler` > `exact` > `syntactic` > `name_only` >
    /// `ambiguous`) with a stable tiebreak on the heuristic order, and only THEN truncate to
    /// `limit`. The overfetch cap is bounded so a huge `limit` can't blow up the candidate set; an
    /// edge upgraded beyond the overfetch window is the residual we accept (the heuristic already
    /// ranked it far down, and the window is generous).
    fn traverse_with_oracle(
        &self,
        symbol: &str,
        reverse: bool,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        let overfetch = crate::query::graph::oracle_overfetch_limit(limit);
        let mut hops = crate::query::graph::traverse_with_options(
            self.storage.connection(),
            symbol,
            reverse,
            overfetch,
            options,
        )?;
        let promoted = self.enrich_hops_with_oracle(&mut hops)?;
        // Only re-rank when a hop was actually PROMOTED to `compiler` (#82 P2). With no promotion
        // the heuristic SQL order already ranks the candidates correctly, so re-sorting would only
        // perturb truncation membership for free (it demotes `match_tier` to a within-confidence
        // tiebreak) — including on every query in a repo with no oracle run. When nothing was
        // promoted, keep the heuristic order and the caller's original `limit`: the overfetched set
        // is in heuristic order, so its first `limit` rows ARE the original top-`limit`.
        if promoted {
            // Stable sort by effective (post-enrichment) confidence so a `compiler` upgrade rises
            // above the heuristic `exact`/`syntactic` edges that out-ranked it in the SQL ORDER BY.
            // Stable keeps the heuristic order (the `match_tier` primary key) within a tier.
            hops.sort_by_key(|hop| crate::query::graph::effective_confidence_rank(&hop.confidence));
        }
        hops.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(hops)
    }

    pub fn trace_callees(
        &self,
        symbol: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        crate::query::graph::traverse(self.storage.connection(), symbol, false, limit)
    }

    pub fn trace_callees_with_options(
        &self,
        symbol: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        let options = self.graph_options_with_logical_group(options)?;
        self.traverse_with_oracle(symbol, false, limit, &options)
    }

    pub fn graph_traversal_report(
        &self,
        tool: &str,
        symbol: &crate::query::symbol::SymbolHit,
        reverse: bool,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<crate::query::graph::GraphTraversalReport> {
        let options = self.graph_options_with_logical_group(options)?;
        // Overfetch + enrich + re-rank + truncate so a compiler-upgraded edge survives the limit
        // (#82 finding 4). `traversal_summary` below still describes the FULL matching population
        // (its own COUNT query, independent of the returned window), so passing the truncated
        // `results.len()` as the returned count stays correct.
        let results =
            self.traverse_with_oracle(&symbol.qualified_name, reverse, limit, &options)?;
        let mut summary = crate::query::graph::traversal_summary(
            self.storage.connection(),
            &symbol.qualified_name,
            reverse,
            limit,
            &options,
            results.len(),
        )?;
        // Make `completeness_risk` quantitative where the oracle covers the unresolved neighbors:
        // append a clause like "2 of 7 unresolved are resolved-external: libc, tokio". This is a
        // read-time annotation derived from the surfaced `resolved-external` verdicts — the risk
        // *level* string is unchanged; the clause just tells the caller how much of the gap is a
        // known external dependency rather than a resolver miss.
        annotate_completeness_with_externals(&mut summary, &results);
        let (logical_symbol, variants) = self.graph_logical_symbol(options.logical_symbol_id)?;
        let mut paths = BTreeSet::new();
        paths.insert(symbol.path.clone());
        for result in &results {
            if let Some(callsite) = &result.callsite {
                paths.insert(callsite.path.clone());
            }
        }
        let mut coverage = self.graph_coverage(paths)?;
        if summary.unresolved > 0 {
            coverage.known_index_gaps.push(format!(
                "{} unresolved qualified callsites match the requested final segment but are not \
                 verified to this symbol",
                summary.unresolved
            ));
        }
        Ok(crate::query::graph::GraphTraversalReport {
            query: crate::query::graph::GraphTraversalQuery {
                tool: tool.to_string(),
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: options.logical_symbol_id,
                symbol_path: symbol.qualified_name.clone(),
                resolution: options.resolution_mode.as_str().to_string(),
            },
            logical_symbol,
            variants,
            summary,
            coverage,
            results,
        })
    }

    pub fn compare_graph_to_text(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        pattern: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
        include_tests: bool,
    ) -> anyhow::Result<crate::query::graph::CompareGraphTextReport> {
        let regex = Regex::new(pattern)?;
        let options = self.graph_options_with_logical_group(options)?;
        let mut graph_edges = crate::query::graph::traverse_with_options(
            self.storage.connection(),
            &symbol.qualified_name,
            true,
            limit,
            &options,
        )?;
        if !include_tests {
            graph_edges.retain(|edge| {
                edge.callsite.as_ref().is_none_or(|callsite| !is_test_like_path(&callsite.path))
            });
        }
        let (logical_symbol, variants) = self.graph_logical_symbol(options.logical_symbol_id)?;
        let text_hits = self.regex_hits(pattern, &regex, include_tests)?;
        let text_by_location = text_hits
            .iter()
            .map(|hit| ((hit.path.clone(), hit.line), hit))
            .collect::<BTreeMap<_, _>>();
        let graph_by_location = graph_edges
            .iter()
            .filter_map(|edge| {
                edge.callsite
                    .as_ref()
                    .map(|callsite| ((callsite.path.clone(), callsite.line), edge))
            })
            .collect::<BTreeMap<_, _>>();

        let mut paths = BTreeSet::new();
        paths.insert(symbol.path.clone());
        for hit in &text_hits {
            paths.insert(hit.path.clone());
        }
        for edge in &graph_edges {
            if let Some(callsite) = &edge.callsite {
                paths.insert(callsite.path.clone());
            }
        }

        let parser_failure_paths = self
            .parser_failure_paths()?
            .into_iter()
            .map(|failure| failure.path)
            .collect::<BTreeSet<_>>();
        let mut matched_hits = Vec::new();
        let mut text_only_hits = Vec::new();
        let mut likely_parser_gaps = Vec::new();
        for hit in &text_hits {
            if let Some(edge) = graph_by_location.get(&(hit.path.clone(), hit.line)) {
                matched_hits.push(crate::query::graph::MatchedGraphTextHit {
                    path: hit.path.clone(),
                    line: hit.line,
                    text: hit.text.clone(),
                    target: edge.target.clone(),
                    edge_kind: edge.edge_kind.clone(),
                    confidence: edge.confidence.clone(),
                    resolution: edge.resolution.clone(),
                });
            } else {
                let gap_kind = classify_text_only_hit(&hit.path, &hit.text, &parser_failure_paths);
                let text_only_hit = crate::query::graph::TextOnlyHit {
                    path: hit.path.clone(),
                    line: hit.line,
                    text: hit.text.clone(),
                    reason: if gap_kind == "parser_call_extraction" || gap_kind == "parser_failure"
                    {
                        "no graph edge extracted"
                    } else {
                        "text mention outside graph-call evidence"
                    }
                    .to_string(),
                    likely_gap: gap_kind.to_string(),
                };
                if is_likely_parser_gap_kind(gap_kind) {
                    likely_parser_gaps.push(text_only_hit.clone());
                }
                text_only_hits.push(text_only_hit);
            }
        }

        let mut graph_only_edges = Vec::new();
        let mut likely_false_positives = Vec::new();
        for edge in &graph_edges {
            let Some(callsite) = &edge.callsite else {
                continue;
            };
            if text_by_location.contains_key(&(callsite.path.clone(), callsite.line)) {
                continue;
            }
            let current_line = self.current_line_text(&callsite.path, callsite.line)?;
            let graph_only = crate::query::graph::GraphOnlyEdge {
                path: callsite.path.clone(),
                line: callsite.line,
                target: edge.target.clone(),
                edge_kind: edge.edge_kind.clone(),
                confidence: edge.confidence.clone(),
                resolution: edge.resolution.clone(),
                evidence: edge.evidence.clone(),
                reason: "graph edge exists but pattern did not match text".to_string(),
                likely_reason: graph_only_reason(edge, current_line.as_deref()),
            };
            if is_likely_false_positive_graph_only(edge, &graph_only) {
                likely_false_positives.push(graph_only.clone());
            }
            graph_only_edges.push(graph_only);
        }
        let complete = likely_parser_gaps.is_empty() && likely_false_positives.is_empty();
        let recommended_fallback =
            recommended_graph_text_fallback(&likely_parser_gaps, &graph_only_edges);
        let pattern_match_mode = compare_pattern_match_mode(pattern, &symbol.name);
        let mut warnings = Vec::new();
        if pattern_match_mode == "substring_identifier" {
            warnings.push(format!(
                "pattern may match identifiers that merely contain `{}`; use an identifier \
                 boundary or escaped call suffix for exact text auditing",
                symbol.name
            ));
        }

        Ok(crate::query::graph::CompareGraphTextReport {
            query: crate::query::graph::CompareGraphTextQuery {
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: options.logical_symbol_id,
                symbol_path: symbol.qualified_name.clone(),
                pattern: pattern.to_string(),
                resolution: options.resolution_mode.as_str().to_string(),
                include_tests,
            },
            logical_symbol,
            variants,
            summary: crate::query::graph::CompareGraphTextSummary {
                graph_hits: u64::try_from(graph_edges.len()).unwrap_or(u64::MAX),
                graph_edges: u64::try_from(graph_edges.len()).unwrap_or(u64::MAX),
                text_hits: u64::try_from(text_hits.len()).unwrap_or(u64::MAX),
                matched: u64::try_from(matched_hits.len()).unwrap_or(u64::MAX),
                graph_only: u64::try_from(graph_only_edges.len()).unwrap_or(u64::MAX),
                text_only: u64::try_from(text_only_hits.len()).unwrap_or(u64::MAX),
                text_mentions: u64::try_from(text_only_hits.len() - likely_parser_gaps.len())
                    .unwrap_or(u64::MAX),
                likely_parser_gaps: u64::try_from(likely_parser_gaps.len()).unwrap_or(u64::MAX),
                likely_false_positives: u64::try_from(likely_false_positives.len())
                    .unwrap_or(u64::MAX),
                likely_index_gaps: u64::try_from(likely_parser_gaps.len()).unwrap_or(u64::MAX),
                complete,
                recommended_fallback,
                pattern_match_mode,
                warnings,
            },
            coverage: self.graph_coverage(paths)?,
            matched_hits,
            text_only_hits,
            graph_only_edges,
            likely_parser_gaps,
            likely_false_positives,
        })
    }

    /// `compare_graph_to_scip` — report where tree-sitter and the compiler (SCIP) DISAGREE on edge
    /// resolution: the `Contradict` verdicts in `edge_oracle` (the heuristic resolved an edge to an
    /// in-corpus target the compiler says is wrong, OR resolved in-corpus while the compiler placed
    /// the callee in a dependency). A user diagnostic + our own resolver-debugging instrument,
    /// sibling of `compare_graph_to_text`.
    ///
    /// Scoped + current ONLY: reads through the store's scope+current join, so a sibling worktree's
    /// or a drifted/dirty file's verdict is never reported. When no oracle run has populated this
    /// checkout, `no_oracle_data` is set and the contradiction list is empty (the graph isn't
    /// "verified to agree" — there's just nothing to compare). The heuristic `edges` row is never
    /// mutated; this is pure read-time diffing.
    pub fn compare_graph_to_scip(
        &self,
    ) -> anyhow::Result<crate::query::graph::CompareGraphScipReport> {
        let tool = oracle::OracleTool::RustAnalyzer;
        let conn = self.storage.connection();
        let tool_version = oracle::latest_run_tool_version(
            conn,
            tool,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )?;
        let mut summary = crate::query::graph::CompareGraphScipSummary::default();
        let mut contradictions = Vec::new();
        let Some(version) = tool_version.clone() else {
            summary.no_oracle_data = true;
            summary.warnings.push(
                "no oracle run for this checkout; run `rag-rat oracle run` to populate compiler \
                 verdicts before comparing"
                    .to_string(),
            );
            return Ok(crate::query::graph::CompareGraphScipReport {
                query: crate::query::graph::CompareGraphScipQuery {
                    tool: tool.as_db_str().to_string(),
                    tool_version: None,
                    commit_sha: self.active_commit_sha.clone(),
                    worktree_id: self.active_worktree_id.clone(),
                },
                summary,
                contradictions,
            });
        };
        let comparisons = oracle::current_oracle_comparisons(
            conn,
            tool,
            &version,
            &self.active_commit_sha,
            &self.active_worktree_id,
        )?;
        summary.verdicts_examined = u64::try_from(comparisons.len()).unwrap_or(u64::MAX);
        // A run exists for this checkout but produced ZERO in-scope verdicts to compare. This is
        // NOT "the compiler agrees with the graph" — it is "the run found nothing in this
        // checkout's scope," which is exactly the silent-no-op symptom the #82 P0 scope bug
        // produced (the active-checkout predicate matched no file rows). Surface it so a
        // run-but-empty result is distinguishable from a genuine all-agree.
        if summary.verdicts_examined == 0 {
            summary.warnings.push(
                "oracle run exists for this checkout but examined 0 in-scope verdicts — nothing \
                 to compare (this is run-but-empty, not compiler-agrees); re-run `rag-rat oracle \
                 run` if you expected verdicts"
                    .to_string(),
            );
        }
        for comparison in comparisons {
            if comparison.kind != oracle::OracleResolutionKind::Contradict {
                continue;
            }
            contradictions.push(crate::query::graph::GraphScipContradiction {
                edge_id: comparison.edge_id,
                edge_kind: comparison.edge_kind,
                heuristic_confidence: crate::query::graph::normalize_confidence(
                    &comparison.heuristic_confidence,
                )
                .to_string(),
                heuristic_target: comparison.heuristic_target,
                callee_name: comparison.callee_name,
                // Label `resolved-external` ONLY for a contradiction the compiler resolved OUTSIDE
                // the corpus (`resolved_symbol_id IS NULL`). A Rust SCIP symbol carries a
                // crate/package component even for the LOCAL crate (`scip-rust crate held-mini …`),
                // so deriving the label from `scip_symbol` alone would mislabel an IN-CORPUS
                // contradiction (the compiler resolved to a *different* in-corpus symbol) as
                // `resolved-external(<local-crate>)` (#82 finding 1). An in-corpus contradiction is
                // a same-corpus disagreement, not an external placement.
                resolved_external: comparison
                    .resolved_symbol_id
                    .is_none()
                    .then(|| resolved_external_label(&comparison.scip_symbol))
                    .flatten(),
                scip_symbol: comparison.scip_symbol,
                callsite: Some(crate::query::graph::Callsite {
                    path: comparison.callsite_path,
                    line: comparison.callsite_line,
                    span: [comparison.callsite_line, comparison.callsite_line],
                }),
            });
        }
        summary.contradictions = u64::try_from(contradictions.len()).unwrap_or(u64::MAX);
        Ok(crate::query::graph::CompareGraphScipReport {
            query: crate::query::graph::CompareGraphScipQuery {
                tool: tool.as_db_str().to_string(),
                tool_version,
                commit_sha: self.active_commit_sha.clone(),
                worktree_id: self.active_worktree_id.clone(),
            },
            summary,
            contradictions,
        })
    }

    fn graph_logical_symbol(
        &self,
        logical_symbol_id: Option<i64>,
    ) -> anyhow::Result<(
        Option<crate::query::graph::LogicalSymbol>,
        Vec<crate::query::graph::LogicalSymbolVariant>,
    )> {
        let Some(logical_symbol_id) = logical_symbol_id else {
            return Ok((None, Vec::new()));
        };
        let Some(logical) = crate::query::symbol::lookup_logical_by_id(
            self.storage.connection(),
            logical_symbol_id,
        )?
        else {
            return Ok((None, Vec::new()));
        };
        let variants = crate::query::symbol::logical_members(
            self.storage.connection(),
            logical.logical_symbol_id,
        )?
        .into_iter()
        .map(|member| crate::query::graph::LogicalSymbolVariant {
            symbol_id: member.symbol_id,
            cfg_expr: member.cfg_expr,
            signature_hash: member.signature_hash,
            start_line: member.start_line,
            end_line: member.end_line,
        })
        .collect::<Vec<_>>();
        Ok((
            Some(crate::query::graph::LogicalSymbol {
                logical_symbol_id: logical.logical_symbol_id,
                qualified_name: logical.qualified_name,
                variant_count: logical.variant_count,
                group_reason: logical.group_reason,
            }),
            variants,
        ))
    }

    fn graph_options_with_logical_group(
        &self,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<crate::query::graph::GraphTraversalOptions> {
        if options.logical_symbol_id.is_some() {
            return Ok(options.clone());
        }
        let Some(symbol_id) = options.symbol_id else {
            return Ok(options.clone());
        };
        let Some(logical) =
            crate::query::symbol::logical_for_symbol_id(self.storage.connection(), symbol_id)?
        else {
            return Ok(options.clone());
        };
        let mut options = options.clone();
        options.logical_symbol_id = Some(logical.logical_symbol_id);
        Ok(options)
    }

    fn local_symbol_context_hits(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let mut stmt = self.storage.connection().prepare(
            "
            SELECT chunks.id, files.path, files.language, files.kind,
                   chunks.start_line, chunks.end_line, chunks.symbol_path, chunks.text
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE files.path = ?1
              AND (
                chunks.symbol_path = ?2
                OR chunks.symbol_path LIKE ?3
                OR chunks.text LIKE ?4
              )
            ORDER BY
              CASE
                WHEN chunks.symbol_path = ?2 THEN 0
                WHEN chunks.symbol_path LIKE ?3 THEN 1
                ELSE 2
              END,
              chunks.start_line
            LIMIT ?5
            ",
        )?;
        let rows = stmt.query_map(
            params![
                symbol.path,
                symbol.qualified_name,
                format!("%{}%", symbol.name),
                format!("%{}%", symbol.name),
                i64::from(limit.max(1)),
            ],
            |row| {
                let text: String = row.get(7)?;
                Ok(SearchHit {
                    chunk_id: row.get(0)?,
                    path: row.get(1)?,
                    language: row.get(2)?,
                    kind: row.get(3)?,
                    start_line: row.get(4)?,
                    end_line: row.get(5)?,
                    symbol_path: row.get(6)?,
                    score: 1.0,
                    retrieval_mode: "lexical".to_string(),
                    summary: bounded_summary(&text),
                    graph: None,
                    score_components: None,
                })
            },
        )?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    pub fn impact_surface(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::impact_surface(self.storage.connection(), query, limit)
    }

    pub fn impact_surface_with_options(
        &self,
        query: &str,
        limit: u32,
        resolution_mode: crate::query::graph::GraphResolutionMode,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::impact_surface_with_options(
            self.storage.connection(),
            query,
            limit,
            resolution_mode,
        )
    }

    pub fn impact_surface_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
        resolution_mode: crate::query::graph::GraphResolutionMode,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::impact_surface_for_symbol(
            self.storage.connection(),
            symbol,
            limit,
            resolution_mode,
        )
    }

    pub fn impact_surface_report_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
        options: &crate::query::impact::ImpactSurfaceOptions,
    ) -> anyhow::Result<crate::query::impact::ImpactSurfaceReport> {
        // Surface the `Compiler` tier on impact's direct graph neighbors too (same read-side JOIN
        // as trace_callees/find_callers). The enrichment is now injected INTO the builder so it
        // runs over the OVERFETCHED candidate set before the re-rank + limit truncation (#82
        // finding
        // 4) and before the memory-evidence edge-id collection — so a compiler-upgraded neighbor
        // can't be dropped by the heuristic limit, and downstream counts see the final window.
        let report = crate::query::impact::impact_surface_report_for_symbol(
            self.storage.connection(),
            symbol,
            limit,
            options,
            |hops| self.enrich_hops_with_oracle(hops),
        )?;
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

    /// Top load-bearing symbols by weighted PageRank over the active checkout's edge graph (#108).
    /// `personalize_to` biases importance toward those symbol ids (changed/query symbols); empty =
    /// global.
    pub fn important_symbols(
        &self,
        limit: usize,
        personalize_to: &[i64],
    ) -> anyhow::Result<Vec<crate::query::pagerank::SymbolImportance>> {
        crate::query::pagerank::important_symbols(
            self.storage.connection(),
            crate::query::pagerank::ImportanceOptions { limit, personalize_to },
        )
    }

    pub fn memory_create(
        &self,
        request: crate::query::memory::RepoMemoryCreate,
    ) -> anyhow::Result<crate::query::memory::RepoMemoryCreateResult> {
        crate::query::memory::create_memory(self.storage.connection(), request)
    }

    pub fn memory_update(
        &self,
        update: crate::query::memory::RepoMemoryUpdate,
    ) -> anyhow::Result<crate::query::memory::RepoMemory> {
        crate::query::memory::update_memory(self.storage.connection(), update)
    }

    pub fn memory_mark_obsolete(
        &self,
        memory_id: &str,
    ) -> anyhow::Result<crate::query::memory::RepoMemory> {
        crate::query::memory::mark_obsolete(self.storage.connection(), memory_id)
    }

    pub fn memory_search(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memory_search(self.storage.connection(), query, limit)
    }

    pub fn memory_for_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memories_for_symbol(self.storage.connection(), symbol, limit)
    }

    pub fn memory_for_path(
        &self,
        path: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memories_for_path(self.storage.connection(), path, limit)
    }

    pub fn memory_for_edges(
        &self,
        edge_ids: &[i64],
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memories_for_edges(self.storage.connection(), edge_ids, limit)
    }

    pub fn memory_evidence_for_symbol_and_edges(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        caller_edge_ids: &[i64],
        callee_edge_ids: &[i64],
        limit: u32,
    ) -> anyhow::Result<crate::query::memory::RepoMemoryEvidence> {
        crate::query::memory::memory_evidence_for_symbol_and_edges(
            self.storage.connection(),
            symbol,
            caller_edge_ids,
            callee_edge_ids,
            limit,
        )
    }

    pub fn memory_for_call_path_hash(
        &self,
        edge_sequence_hash: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        crate::query::memory::memories_for_call_path_hash(
            self.storage.connection(),
            edge_sequence_hash,
            limit,
        )
    }

    pub fn memory_rebind(
        &self,
        memory_id: &str,
        bind: crate::query::memory::RepoMemoryBindTarget,
    ) -> anyhow::Result<crate::query::memory::RepoMemory> {
        crate::query::memory::rebind_memory(self.storage.connection(), memory_id, bind)
    }

    pub fn memory_validate(
        &self,
    ) -> anyhow::Result<crate::query::memory::RepoMemoryValidationReport> {
        crate::query::memory::validate_memories(
            self.storage.connection(),
            self.storage.source_root(),
        )
    }

    pub fn memory_doctor(&self) -> anyhow::Result<Vec<crate::query::memory::MemoryDoctorEntry>> {
        crate::query::memory::doctor_report(self.storage.connection())
    }

    /// Read-only list of active+stale memories, optionally filtered by binding_kind.
    /// `kind` filters by binding kind (e.g. `Some("dir")`); `None` returns all.
    pub fn memory_list(
        &self,
        kind: Option<&str>,
    ) -> anyhow::Result<Vec<crate::query::memory::MemorySummary>> {
        crate::query::memory::list_memories(self.storage.connection(), kind)
    }

    /// Fetch a single memory by id, returning `None` when not found.
    pub fn memory_get(
        &self,
        memory_id: &str,
    ) -> anyhow::Result<Option<crate::query::memory::RepoMemory>> {
        crate::query::memory::memory_by_id(self.storage.connection(), memory_id)
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
mod oracle_surfacing_tests {
    //! End-to-end integration tests for the phase-2 (#69) read-side surfacing through the public
    //! `IndexDatabase` API: build a real temp Rust checkout with an intra-file call, rebuild, run
    //! the oracle over a programmatically-built `.scip` (the deterministic `--scip` consumption
    //! path — no rust-analyzer), and assert the `Compiler` tier / `resolved-external` /
    //! `compare_graph_to_scip` / gc behaviours. Models `eval::tests::eval_suite_runs_oracle_*`.

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ::protobuf::{EnumOrUnknown, Message};
    use ::scip::types::{Document, Index, Occurrence, PositionEncoding, SymbolRole};

    use super::*;
    use crate::config::ResolvedTarget;
    use crate::index::oracle::OracleTool;

    static N: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-q-oracle-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        root
    }

    fn rust_config(root: PathBuf) -> Config {
        Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            local_ai: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
        }
    }

    /// The callee identifier byte range of the (single) `calls_name` edge, read back from the DB so
    /// the `.scip` occurrence aligns exactly with what the indexer recorded.
    fn call_edge(db: &IndexDatabase) -> (i64, i64, i64, String) {
        db.storage
            .connection()
            .query_row(
                "SELECT edges.id, edges.callee_start_byte, edges.callee_end_byte, files.path
                 FROM edges JOIN files ON files.id = edges.source_file_id
                 WHERE edges.edge_kind = 'calls_name' AND edges.callee_start_byte IS NOT NULL
                 LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap()
    }

    /// Build a `.scip` over a single-line source file: a reference occurrence at the callee byte
    /// range (single-line → char == byte for ASCII) plus a definition occurrence for the same
    /// symbol at `def_range`. `def_path` is where the definition lives (in-corpus → Upgrade;
    /// elsewhere/external symbol → resolved-external).
    fn scip_with(
        path: &str,
        callee_start: i64,
        callee_end: i64,
        symbol: &str,
        def_path: Option<&str>,
        def_range: Option<(i64, i64)>,
    ) -> Vec<u8> {
        let occ = |start: i64, end: i64, role: SymbolRole| Occurrence {
            range: vec![0, start as i32, end as i32],
            symbol: symbol.to_string(),
            symbol_roles: role as i32,
            ..Default::default()
        };
        let mut documents = vec![Document {
            relative_path: path.to_string(),
            occurrences: vec![occ(callee_start, callee_end, SymbolRole::UnspecifiedSymbolRole)],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }];
        if let (Some(def_path), Some((ds, de))) = (def_path, def_range) {
            // A def in the SAME file must be appended to that document's occurrence list, not
            // pushed as a SECOND document with the same `relative_path` —
            // `ScipIndex::from_index` keys `occurrences_by_path` by path, so a
            // duplicate-path document overwrites the ref.
            if def_path == path {
                documents[0].occurrences.push(occ(ds, de, SymbolRole::Definition));
            } else {
                documents.push(Document {
                    relative_path: def_path.to_string(),
                    occurrences: vec![occ(ds, de, SymbolRole::Definition)],
                    position_encoding: EnumOrUnknown::new(
                        PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                    ),
                    ..Default::default()
                });
            }
        }
        Index { documents, ..Default::default() }.write_to_bytes().unwrap()
    }

    /// The full path: rebuild a checkout where `caller` calls `target`, run the oracle from a
    /// pre-built `.scip` that resolves the call in-corpus, and assert `find_callers` /
    /// `trace_callees` surface the `compiler` tier with the `scip:<tool>@<version>` reason — while
    /// the heuristic `edges` row is untouched. Also asserts `compare_graph_to_scip` reports no
    /// contradiction (an Upgrade is agreement-shaped, not a disagreement).
    #[test]
    fn oracle_run_from_scip_surfaces_compiler_tier() {
        let root = temp_root();
        // Single line so byte offsets == char offsets (ASCII): `target` is the callee.
        fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let (edge_id, callee_start, callee_end, path) = call_edge(&db);
        // `target` definition: `fn target() {}` → identifier `target` at bytes 29..35.
        let symbol = "scip-rust crate held-mini `target`().";
        let scip = scip_with(&path, callee_start, callee_end, symbol, Some(&path), Some((29, 35)));

        let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
        // `target` is defined in-corpus, so the heuristic resolves it; the oracle CONFIRMS it. Both
        // Confirm and Upgrade surface the `compiler` tier.
        assert!(
            report.confirmed >= 1 || report.upgraded >= 1,
            "expected in-corpus confirm/upgrade, got {report:?}"
        );

        // find_callers (reverse) surfaces the compiler tier on the matching edge.
        let callers = db
            .find_callers_with_options("target", 50, &crate::query::graph::GraphTraversalOptions {
                include_unresolved: true,
                ..Default::default()
            })
            .unwrap();
        let hop = callers.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
        assert_eq!(hop.confidence, "compiler", "expected Compiler tier surfaced");
        assert_eq!(hop.resolution_reason.as_deref(), Some("scip:rust-analyzer@v-test"));
        // The heuristic edge confidence is preserved (not overwritten).
        assert_ne!(hop.edge_confidence, "compiler");

        // The heuristic `edges` row was never mutated by the oracle pass.
        let edge_confidence: String = db
            .storage
            .connection()
            .query_row("SELECT confidence FROM edges WHERE id = ?1", params![edge_id], |r| r.get(0))
            .unwrap();
        assert_ne!(edge_confidence, "compiler");

        // An Upgrade is agreement-shaped → compare_graph_to_scip reports no contradiction.
        let compare = db.compare_graph_to_scip().unwrap();
        assert!(!compare.summary.no_oracle_data);
        assert_eq!(compare.summary.contradictions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    /// `oracle_pre_spawn_snapshot` returns the active checkout's indexed `(path -> sha256)` map;
    /// a run pinned to a MATCHING snapshot verdicts normally, while a snapshot disagreeing with
    /// the join-time indexed sha (the mid-subprocess reindex, #83) skips the candidate.
    #[test]
    fn pre_spawn_snapshot_round_trips_through_run_oracle() {
        let root = temp_root();
        fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let (_edge_id, cs, ce, path) = call_edge(&db);
        let snapshot = db.oracle_pre_spawn_snapshot().unwrap();
        let indexed_sha: String = db
            .storage
            .connection()
            .query_row("SELECT sha256 FROM files WHERE path = ?1", params![path], |r| r.get(0))
            .unwrap();
        assert_eq!(
            snapshot.get(&path).map(String::as_str),
            Some(indexed_sha.as_str()),
            "snapshot must carry the indexed sha for every active-checkout file"
        );

        let symbol = "scip-rust crate held-mini `target`().";
        let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((29, 35)));
        let report = db
            .run_oracle(OracleTool::RustAnalyzer, "v-test", &scip, None, Some(&snapshot))
            .unwrap();
        assert!(report.confirmed >= 1 || report.upgraded >= 1, "matching pin verdicts normally");
        assert_eq!(report.skipped_drifted, 0);

        let mut stale = snapshot.clone();
        stale.insert(path.clone(), "pre-spawn-old".to_string());
        let report =
            db.run_oracle(OracleTool::RustAnalyzer, "v-test2", &scip, None, Some(&stale)).unwrap();
        assert_eq!(report.rows_written, 0, "a mid-subprocess reindex must skip the candidate");
        assert!(report.skipped_drifted >= 1);

        let _ = fs::remove_dir_all(&root);
    }

    /// Staleness revert: after a current run surfaces `compiler`, drifting the source file (so its
    /// `files.sha256` no longer matches the verdict's `file_sha`) reverts the edge to heuristic
    /// display — never `compiler`.
    #[test]
    fn drifted_file_reverts_to_heuristic_display() {
        let root = temp_root();
        fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let (edge_id, cs, ce, path) = call_edge(&db);
        let symbol = "scip-rust crate held-mini `target`().";
        let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((29, 35)));
        db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

        // Drift the recorded sha so the verdict's file_sha no longer matches (file changed). The
        // active connection exposes `files` as a scoped TEMP VIEW (read-only), so the UPDATE must
        // target the underlying `main.files` table.
        db.storage
            .connection()
            .execute("UPDATE main.files SET sha256 = 'drifted' WHERE path = ?1", params![path])
            .unwrap();

        let callers = db
            .find_callers_with_options("target", 50, &crate::query::graph::GraphTraversalOptions {
                include_unresolved: true,
                ..Default::default()
            })
            .unwrap();
        let hop = callers.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
        assert_ne!(hop.confidence, "compiler", "drifted file must revert to heuristic display");
        assert!(hop.resolution_reason.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    /// `resolved-external`: a `.scip` resolving the callee to a packaged dependency symbol with no
    /// in-corpus definition surfaces `resolved_external = resolved-external(<package>)` on the hop,
    /// and feeds the quantitative completeness clause in the graph report.
    #[test]
    fn external_resolution_surfaces_resolved_external_label() {
        let root = temp_root();
        // `external_fn` is NOT defined in-corpus → the heuristic can't resolve it (NameOnly /
        // unresolved), so SCIP's external resolution is a clean `resolved-external`, not a
        // contradiction of an in-corpus claim.
        fs::write(root.join("src/lib.rs"), "fn caller() { external_fn(); }\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let (edge_id, cs, ce, path) = call_edge(&db);
        // A packaged SCIP symbol with NO in-corpus definition occurrence →
        // resolved-external(tokio).
        let symbol = "scip-rust cargo tokio 1.0 `external_fn`().";
        let scip = scip_with(&path, cs, ce, symbol, None, None);
        let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
        assert!(report.resolved_external >= 1, "expected resolved-external, got {report:?}");

        let callees = db
            .trace_callees_with_options("caller", 50, &crate::query::graph::GraphTraversalOptions {
                include_unresolved: true,
                ..Default::default()
            })
            .unwrap();
        let hop = callees.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
        assert_eq!(hop.resolved_external.as_deref(), Some("resolved-external(tokio)"));
        // External placement is not an in-corpus upgrade → confidence stays heuristic.
        assert_ne!(hop.confidence, "compiler");

        let _ = fs::remove_dir_all(&root);
    }

    /// `compare_graph_to_scip` reports a contradiction when the heuristic resolved an edge
    /// in-corpus but the compiler resolves the callee to a DIFFERENT (external) target.
    #[test]
    fn compare_graph_to_scip_reports_contradiction() {
        let root = temp_root();
        fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let (edge_id, cs, ce, path) = call_edge(&db);
        // Force the heuristic edge to look in-corpus-resolved (Exact + to_symbol_id), so the
        // compiler's external resolution is a contradiction, not a plain resolved-external.
        let target_sym: i64 = db
            .storage
            .connection()
            .query_row("SELECT id FROM symbols WHERE name = 'target' LIMIT 1", [], |r| r.get(0))
            .unwrap();
        db.storage
            .connection()
            .execute(
                "UPDATE edges SET confidence = 'Exact', resolution = 'exact', to_symbol_id = ?2 \
                 WHERE id = ?1",
                params![edge_id, target_sym],
            )
            .unwrap();

        // The compiler says the callee is actually `other::target` in a dependency → contradiction.
        let symbol = "scip-rust cargo other 1.0 `target`().";
        let scip = scip_with(&path, cs, ce, symbol, None, None);
        db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

        let compare = db.compare_graph_to_scip().unwrap();
        assert_eq!(compare.summary.contradictions, 1, "{compare:?}");
        let c = &compare.contradictions[0];
        assert_eq!(c.edge_id, edge_id);
        assert_eq!(c.heuristic_confidence, "exact");
        assert_eq!(c.resolved_external.as_deref(), Some("resolved-external(other)"));

        let _ = fs::remove_dir_all(&root);
    }

    /// #82 P0: when a run EXISTS but examined 0 in-scope verdicts, `compare_graph_to_scip` must WARN
    /// — that is "run-but-empty" (the silent symptom of the scope bug), not "compiler agrees". Here
    /// a run writes a verdict, then the callsite file drifts so the current-content gate
    /// filters every verdict out → `verdicts_examined == 0` despite the run row existing.
    #[test]
    fn compare_warns_when_run_exists_but_no_verdicts_in_scope() {
        let root = temp_root();
        fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let (_edge_id, cs, ce, path) = call_edge(&db);
        let symbol = "scip-rust crate held-mini `target`().";
        let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((29, 35)));
        db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

        // Drift the callsite file so every verdict's `file_sha` gate fails → 0 in-scope verdicts,
        // even though the `oracle_runs` row still exists.
        db.storage
            .connection()
            .execute("UPDATE main.files SET sha256 = 'drifted' WHERE path = ?1", params![path])
            .unwrap();

        let compare = db.compare_graph_to_scip().unwrap();
        assert!(!compare.summary.no_oracle_data, "a run DOES exist for this checkout");
        assert_eq!(compare.summary.verdicts_examined, 0, "all verdicts filtered by the drift gate");
        assert!(
            compare.summary.warnings.iter().any(|w| w.contains("0 in-scope verdicts")),
            "run-but-empty must warn, not silently read as compiler-agrees: {:?}",
            compare.summary.warnings
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// #82 finding 1: an IN-CORPUS contradiction (the compiler resolved the callee to a DIFFERENT
    /// in-corpus symbol than the heuristic) must NOT be labeled `resolved-external`. A Rust SCIP
    /// symbol carries a crate/package component even for the LOCAL crate, so deriving the label
    /// from `scip_symbol` alone would mislabel it as `resolved-external(held-mini)`.
    #[test]
    fn in_corpus_contradiction_is_not_labeled_resolved_external() {
        let root = temp_root();
        // Two in-corpus defs. The heuristic resolves `target()` to `target`; the compiler resolves
        // the same callsite to the OTHER in-corpus def `other` → in-corpus Contradict.
        fs::write(
            root.join("src/lib.rs"),
            "fn caller() { target(); } fn target() {} fn other() {}\n",
        )
        .unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let (edge_id, cs, ce, path) = call_edge(&db);
        // Force the heuristic edge to look in-corpus-resolved to `target` (Exact + to_symbol_id).
        let target_sym: i64 = db
            .storage
            .connection()
            .query_row("SELECT id FROM symbols WHERE name = 'target' LIMIT 1", [], |r| r.get(0))
            .unwrap();
        db.storage
            .connection()
            .execute(
                "UPDATE edges SET confidence = 'Exact', resolution = 'exact', to_symbol_id = ?2 \
                 WHERE id = ?1",
                params![edge_id, target_sym],
            )
            .unwrap();
        // The compiler resolves the callee to the in-corpus def `other` (a LOCAL-crate SCIP
        // symbol), whose definition occurrence sits at `other`'s recorded byte span.
        let (other_start, other_end): (i64, i64) = db
            .storage
            .connection()
            .query_row(
                "SELECT start_byte, end_byte FROM symbols WHERE name = 'other' LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let symbol = "scip-rust crate held-mini `other`().";
        let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((other_start, other_end)));
        db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();

        let compare = db.compare_graph_to_scip().unwrap();
        assert_eq!(compare.summary.contradictions, 1, "{compare:?}");
        let c = &compare.contradictions[0];
        assert_eq!(c.edge_id, edge_id);
        // The disagreement is in-corpus → no external label, even though the SCIP symbol names the
        // local crate.
        assert_eq!(
            c.resolved_external, None,
            "an in-corpus contradiction must not be labeled resolved-external"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// #82 finding 2: an `Upgrade` on a heuristic-unresolved edge must surface the SCIP-RESOLVED
    /// symbol as the hop's target, not the heuristic's missing/heuristic one. We strip the
    /// heuristic resolution (NameOnly, no `to_symbol_id`) and let the compiler resolve in-corpus,
    /// then read FORWARD from `caller` (robust for an unresolved edge) and assert the hop's target
    /// moved to the compiler-resolved symbol.
    #[test]
    fn upgrade_hydrates_target_from_compiler_resolution() {
        let root = temp_root();
        fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        let (edge_id, cs, ce, path) = call_edge(&db);
        let target_qualified: String = db
            .storage
            .connection()
            .query_row(
                "SELECT qualified_name FROM symbols WHERE name = 'target' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let (target_start, target_end): (i64, i64) = db
            .storage
            .connection()
            .query_row(
                "SELECT start_byte, end_byte FROM symbols WHERE name = 'target' LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // Demote the heuristic edge to a genuine miss: NameOnly, no resolved target / qualified
        // name. A promotion that didn't MOVE the target would surface this absent one.
        db.storage
            .connection()
            .execute(
                "UPDATE edges SET confidence = 'NameOnly', resolution = 'name_only', to_symbol_id \
                 = NULL, target_qualified_name = NULL WHERE id = ?1",
                params![edge_id],
            )
            .unwrap();
        let symbol = "scip-rust crate held-mini `target`().";
        let scip = scip_with(&path, cs, ce, symbol, Some(&path), Some((target_start, target_end)));
        let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
        assert!(report.upgraded >= 1, "expected an Upgrade, got {report:?}");

        let callees = db
            .trace_callees_with_options("caller", 50, &crate::query::graph::GraphTraversalOptions {
                include_unresolved: true,
                ..Default::default()
            })
            .unwrap();
        let hop = callees.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
        assert_eq!(hop.confidence, "compiler", "an Upgrade surfaces the compiler tier");
        // The target moved to the SCIP-resolved symbol, not the heuristic's absent target.
        assert_eq!(hop.to_symbol.as_deref(), Some(target_qualified.as_str()));
        assert_eq!(hop.target_qualified_name.as_deref(), Some(target_qualified.as_str()));
        assert!(hop.verified_target_symbol, "the compiler-resolved target is verified");

        let _ = fs::remove_dir_all(&root);
    }

    /// #82 finding 4: a compiler-`Upgrade`d low-confidence neighbor must appear within `limit` even
    /// when more `Exact` neighbors than `limit` outrank it heuristically. Oracle enrichment runs
    /// AFTER the heuristic-ordered limit, so without overfetch+re-rank the upgraded edge is
    /// dropped.
    #[test]
    fn compiler_upgrade_survives_heuristic_limit() {
        let root = temp_root();
        // Single line so byte offsets == char offsets (ASCII) for the `.scip` occurrence. `pull` is
        // the upgrade target; two of its three callers are Exact (high heuristic rank), the third
        // is a name-only miss the compiler upgrades.
        fs::write(
            root.join("src/lib.rs"),
            "fn pull() {} fn a() { pull(); } fn b() { pull(); } fn c() { pull(); }\n",
        )
        .unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        // Find the three call edges to `pull`. Make two of them Exact (high heuristic rank) and the
        // third a NameOnly miss the compiler will upgrade.
        let edges: Vec<(i64, i64, i64, String)> = {
            let conn = db.storage.connection();
            let mut stmt = conn
                .prepare(
                    "SELECT edges.id, edges.callee_start_byte, edges.callee_end_byte, files.path \
                     FROM edges JOIN files ON files.id = edges.source_file_id WHERE \
                     edges.edge_kind = 'calls_name' AND edges.callee_start_byte IS NOT NULL ORDER \
                     BY edges.id",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)))
                .unwrap();
            rows.map(Result::unwrap).collect()
        };
        assert_eq!(edges.len(), 3, "three calls to pull");
        let pull_sym: i64 = db
            .storage
            .connection()
            .query_row("SELECT id FROM symbols WHERE name = 'pull' LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let (pull_start, pull_end): (i64, i64) = db
            .storage
            .connection()
            .query_row(
                "SELECT start_byte, end_byte FROM symbols WHERE name = 'pull' LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // Edges 0,1: Exact resolved to `pull`. Edge 2: NameOnly miss (the upgrade candidate).
        let conn = db.storage.connection();
        for (edge_id, ..) in &edges[..2] {
            conn.execute(
                "UPDATE edges SET confidence = 'Exact', resolution = 'exact', to_symbol_id = ?2 \
                 WHERE id = ?1",
                params![edge_id, pull_sym],
            )
            .unwrap();
        }
        let (upgrade_edge, ucs, uce, path) = edges[2].clone();
        // NameOnly (heuristically low rank, below the Exact callers) but still target-resolved to
        // `pull` so the reverse traversal includes it as a candidate. The oracle still classifies
        // it an Upgrade (the heuristic didn't resolve it Exact/Syntactic-in-corpus), and
        // the re-rank must lift it above the Exact callers within the limit.
        conn.execute(
            "UPDATE edges SET confidence = 'NameOnly', resolution = 'name_only', to_symbol_id = \
             ?2 WHERE id = ?1",
            params![upgrade_edge, pull_sym],
        )
        .unwrap();

        // The compiler upgrades ONLY the name-only edge to the in-corpus `pull` def.
        let symbol = "scip-rust crate held-mini `pull`().";
        let scip = scip_with(&path, ucs, uce, symbol, Some(&path), Some((pull_start, pull_end)));
        let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
        assert!(report.upgraded >= 1, "expected an Upgrade, got {report:?}");

        // limit = 1: heuristically the two Exact callers outrank the name-only one, so without the
        // overfetch+re-rank the compiler upgrade is dropped. With it, the compiler tier wins.
        let callers = db
            .find_callers_with_options("pull", 1, &crate::query::graph::GraphTraversalOptions {
                include_unresolved: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(callers.len(), 1, "limit honored");
        assert_eq!(
            callers[0].edge_id, upgrade_edge,
            "the compiler-upgraded neighbor must rank into the limit ahead of Exact neighbors"
        );
        assert_eq!(callers[0].confidence, "compiler");

        let _ = fs::remove_dir_all(&root);
    }

    /// gc prunes `oracle_runs` for dead checkout contexts: an oracle run recorded under a sibling
    /// `(commit, worktree)` is dropped by `prune_to_live` when that context is not live.
    #[test]
    fn gc_prunes_oracle_runs_for_dead_contexts() {
        let root = temp_root();
        fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        // Record a run for THIS (active) checkout + a run for a dead sibling context.
        crate::index::oracle::run_oracle(
            db.storage.connection(),
            OracleTool::RustAnalyzer,
            "v-test",
            &db.active_commit_sha,
            &db.active_worktree_id,
            &Index::default().write_to_bytes().unwrap(),
            &root,
            None,
            None,
        )
        .unwrap();
        db.storage
            .connection()
            .execute(
                "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, \
                 status, stats_json) VALUES ('rust-analyzer', 'v-test', 'dead-commit', \
                 'dead-worktree', 0, 'Completed', '{}')",
                [],
            )
            .unwrap();
        let before: i64 = db
            .storage
            .connection()
            .query_row("SELECT COUNT(*) FROM oracle_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 2);

        // gc keeps the active context and prunes the dead one.
        db.prune_to_live(
            std::slice::from_ref(&db.active_commit_sha),
            std::slice::from_ref(&db.active_worktree_id),
        )
        .unwrap();

        let remaining: Vec<String> = {
            let conn = db.storage.connection();
            let mut stmt = conn.prepare("SELECT commit_sha FROM oracle_runs").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(
            !remaining.iter().any(|c| c == "dead-commit"),
            "dead-context oracle_runs row must be pruned: {remaining:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolved_external_label_extracts_package() {
        assert_eq!(
            resolved_external_label("scip-rust cargo tokio 1.0 `spawn`()."),
            Some("resolved-external(tokio)".to_string())
        );
        // A symbol with no package component yields no label.
        assert_eq!(resolved_external_label("local 0"), None);
    }

    #[test]
    fn completeness_annotation_counts_externals_and_lists_packages() {
        let mut summary = crate::query::graph::GraphTraversalSummary {
            unresolved: 5,
            completeness_risk: "medium".to_string(),
            ..Default::default()
        };
        let hop = |external: Option<&str>| crate::query::graph::GraphHop {
            edge_id: 0,
            from_symbol: None,
            to_symbol: None,
            edge_kind: "calls_name".to_string(),
            confidence: "name_only".to_string(),
            edge_confidence: "name_only".to_string(),
            target: None,
            target_qualified_name: None,
            evidence: None,
            receiver_hint: None,
            resolution: "unresolved".to_string(),
            resolution_reason: None,
            resolved_external: external.map(str::to_string),
            verified_target_symbol: false,
            shown_by_default: true,
            callsite: None,
        };
        let hops = vec![
            hop(Some("resolved-external(tokio)")),
            hop(Some("resolved-external(libc)")),
            hop(None),
        ];
        annotate_completeness_with_externals(&mut summary, &hops);
        // The count is over the SHOWN window (2 of the passed hops), not divided by the
        // population-wide unresolved gap (#82 P3) — the clause speaks of "shown neighbors".
        assert_eq!(
            summary.completeness_risk,
            "medium (2 shown neighbors are resolved-external: libc, tokio)"
        );

        // No externals → the qualitative string is left untouched.
        let mut bare = crate::query::graph::GraphTraversalSummary {
            unresolved: 3,
            completeness_risk: "high".to_string(),
            ..Default::default()
        };
        annotate_completeness_with_externals(&mut bare, &[hop(None)]);
        assert_eq!(bare.completeness_risk, "high");
    }

    /// `run_oracle_with_tool` degrades to `Blocked` (never an error) when the indexer isn't
    /// installed — the missing-embedding-model UX. Skipped when rust-analyzer happens to be on PATH
    /// (then the subprocess path runs, which this test doesn't assert).
    #[test]
    fn oracle_run_without_tool_is_blocked_not_error() {
        if std::process::Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return; // rust-analyzer present; the Blocked path isn't exercised here.
        }
        let root = temp_root();
        fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();
        let outcome =
            db.run_oracle_with_tool(OracleTool::RustAnalyzer, &root.join("o.scip")).unwrap();
        assert!(matches!(outcome, crate::index::oracle::OracleRunOutcome::Blocked { .. }));
        let _ = fs::remove_dir_all(&root);
    }

    /// Run a git command in `root`, panicking on failure — used to make a real committed checkout
    /// so `resolve_git_context` returns a non-empty `commit_sha` AND `worktree_id` (the active
    /// context every other e2e test misses by running in a non-git temp dir).
    fn git(root: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// #82 P2 regression: `find_callers` with NO oracle data must return the IDENTICAL membership
    /// and order as the plain heuristic traversal. The unconditional re-sort in
    /// `traverse_with_oracle` demoted `match_tier` to a within-confidence tiebreak and changed
    /// truncation membership on EVERY query — including repos with no oracle run, where
    /// enrichment is a no-op. The fix only re-sorts when a hop was actually promoted, so with
    /// no oracle data the overfetched heuristic order + the caller's `limit` are returned
    /// untouched.
    #[test]
    fn find_callers_without_oracle_matches_heuristic_order() {
        let root = temp_root();
        // Several callers of `target` with differing heuristic confidence, more than `limit` so
        // truncation membership is observable.
        let mut src = String::new();
        for i in 0..8 {
            src.push_str(&format!("fn caller{i}() {{ target(); }} "));
        }
        src.push_str("fn target() {}\n");
        fs::write(root.join("src/lib.rs"), src).unwrap();
        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        // No oracle run at all: enrichment early-returns false, so no re-sort fires.
        let opts = crate::query::graph::GraphTraversalOptions {
            include_unresolved: true,
            ..Default::default()
        };
        let limit = 3;
        let via_oracle_path = db.find_callers_with_options("target", limit, &opts).unwrap();

        // The pre-oracle path: plain heuristic traversal at the SAME limit (what the oracle-aware
        // entry point must collapse to when there's nothing to enrich).
        let grouped = db.graph_options_with_logical_group(&opts).unwrap();
        let heuristic = crate::query::graph::traverse_with_options(
            db.storage.connection(),
            "target",
            true,
            limit,
            &grouped,
        )
        .unwrap();

        let ids = |hops: &[crate::query::graph::GraphHop]| {
            hops.iter().map(|h| h.edge_id).collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&via_oracle_path),
            ids(&heuristic),
            "with no oracle data, find_callers must match the plain heuristic membership + order"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// #82 P0 regression: on a REAL committed git checkout the active context is
    /// `(commit_sha = HEAD, worktree_id = root)` — BOTH non-empty — and the indexed files are
    /// `FileScope::commit` rows `(HEAD, '')`. The old oracle scope predicate
    /// `files.commit_sha = ?sha AND files.worktree_id = ?wt` matched ZERO such rows, so `oracle
    /// run` silently wrote 0 verdicts and the `Compiler` tier never surfaced. This test commits
    /// the checkout, runs the oracle, and asserts verdicts are written AND `trace_callees`
    /// surfaces `compiler` — the exact case the unit harness (`commit=''`) and the non-git e2e
    /// tests both degenerate past.
    #[test]
    fn oracle_surfaces_compiler_tier_on_a_real_git_checkout() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // no git on PATH — skip rather than fail.
        }
        let root = temp_root();
        fs::write(root.join("src/lib.rs"), "fn caller() { target(); } fn target() {}\n").unwrap();
        // A real committed checkout: clean tree → files index as `FileScope::commit` (HEAD, '').
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let config = rust_config(root.clone());
        let db = IndexDatabase::rebuild(&config).unwrap();

        // Sanity: the active context carries BOTH a real commit_sha and a worktree_id, and the file
        // rows are committed-scoped (commit set, worktree empty) — the shape that broke the AND.
        let (active_commit, active_worktree) = crate::index::resolve_git_context(&root);
        assert!(!active_commit.is_empty(), "real checkout has a HEAD commit");
        assert!(!active_worktree.is_empty(), "worktree id is the root path");
        let (file_commit, file_worktree): (String, String) = db
            .storage
            .connection()
            .query_row(
                "SELECT commit_sha, worktree_id FROM files WHERE path = 'src/lib.rs'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(!file_commit.is_empty() && file_worktree.is_empty(), "committed-scoped file row");

        let (edge_id, callee_start, callee_end, path) = call_edge(&db);
        let symbol = "scip-rust crate held-mini `target`().";
        let scip = scip_with(&path, callee_start, callee_end, symbol, Some(&path), Some((29, 35)));

        let report = db.run_oracle_from_scip(OracleTool::RustAnalyzer, "v-test", &scip).unwrap();
        assert!(
            report.rows_written >= 1,
            "verdicts must be written on a real git checkout (the #82 P0 wrote 0); got {report:?}"
        );

        let callees = db
            .trace_callees_with_options("caller", 50, &crate::query::graph::GraphTraversalOptions {
                include_unresolved: true,
                ..Default::default()
            })
            .unwrap();
        let hop = callees.iter().find(|h| h.edge_id == edge_id).expect("call edge present");
        assert_eq!(hop.confidence, "compiler", "Compiler tier must surface on a real git checkout");
        assert_eq!(hop.resolution_reason.as_deref(), Some("scip:rust-analyzer@v-test"));

        let _ = fs::remove_dir_all(&root);
    }
}
