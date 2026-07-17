//! Git- and GitHub-history query surface on `IndexDatabase`: commit/rationale search, per-path and
//! per-symbol history, blame, and GitHub ref/issue sync + lookup.

use rag_rat_papertrail as papertrail;

use super::*;

impl IndexDatabase {
    pub fn commit_search(&self, query: &str, limit: u32) -> anyhow::Result<Vec<CommitSearchHit>> {
        // #582: ranked commit_fts read — heal-and-retry on shadow corruption.
        crate::index::retry_once_on_fts_corruption(
            || git_history::commit_search(self.storage.connection(), query, limit),
            || self.heal_corrupt_fts(),
        )
    }

    /// Commit-replay eval cases (#120) from the indexed git history — commit message as query, the
    /// diff's changed paths as recall gold. See [`git_history::replay_commit_cases`].
    pub fn replay_commit_cases(
        &self,
        limit: u32,
        max_files: u32,
    ) -> anyhow::Result<Vec<git_history::ReplayCase>> {
        git_history::replay_commit_cases(self.storage.connection(), limit, max_files)
    }

    /// The indexed-path set the commit-replay eval filters its gold against, so recall counts only
    /// retrievable paths (#315). See [`git_history::indexed_path_set`].
    pub fn indexed_path_set(&self) -> anyhow::Result<std::collections::BTreeSet<String>> {
        git_history::indexed_path_set(self.storage.connection())
    }

    /// Symbol-level replay gold (#120): distinct chunk `symbol_path`s overlapping `ranges` in
    /// `path`. See [`git_history::chunk_symbol_paths_in_ranges`].
    pub fn chunk_symbol_paths_in_ranges(
        &self,
        path: &str,
        ranges: &[(i64, i64)],
    ) -> anyhow::Result<Vec<String>> {
        git_history::chunk_symbol_paths_in_ranges(self.storage.connection(), path, ranges)
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
        // #582: an INDEPENDENT ranked commit_fts path — it calls git_history::commit_search
        // internally, not the wrapped commit_search above, so it needs its own heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                git_history::commits_touching_query(
                    self.storage.connection(),
                    query,
                    limit,
                    &current_hits,
                )
            },
            || self.heal_corrupt_fts(),
        )
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

    /// Mirror every resolved tracker binding. References remain an annotation layer and do not
    /// select the network fetch set.
    pub fn papertrail_sync(&self, full: bool) -> anyhow::Result<PapertrailSyncReport> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("index has no source_root metadata; rebuild required");
        };
        papertrail::block_on(papertrail::sync_mirror(
            self.storage.connection(),
            root,
            full,
            &self.papertrail,
        ))
    }

    /// Mirror only the bindings the scheduling policy says are due (the automatic watcher / hook
    /// path); [`Self::papertrail_sync`] is the unconditional manual pass.
    pub fn papertrail_sync_scheduled(
        &self,
        request: papertrail::AutosyncRequest,
    ) -> anyhow::Result<PapertrailSyncReport> {
        // The commit-closer rederive inspects the checkout's remotes; use the PROCESS-resolved
        // active root (`storage.source_root()`, the reanchored `config.root` that memory
        // validation also resolves against) — NOT the raw persisted `repo_meta("source_root")`,
        // which goes stale under a shared DB across linked worktrees and would point the git
        // inspection at another checkout. Bail when absent, matching the manual entry.
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("index has no source_root metadata; rebuild required");
        };
        papertrail::block_on(papertrail::sync_mirror_scheduled(
            self.storage.connection(),
            root,
            &self.papertrail,
            request,
        ))
    }

    pub fn papertrail_issue_search(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<PapertrailEvidence>> {
        crate::index::retry_once_on_fts_corruption(
            || papertrail::issue_search(self.storage.connection(), query, limit),
            || self.heal_corrupt_fts(),
        )
    }

    pub fn rationale_search(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<PapertrailEvidence>> {
        crate::index::retry_once_on_fts_corruption(
            || {
                papertrail::rationale_search(
                    self.storage.connection(),
                    query,
                    limit,
                    &self.papertrail,
                )
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn papertrail_refs_for_path(
        &self,
        path: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<papertrail::PapertrailRef>> {
        papertrail::refs_for_path(self.storage.connection(), path, limit)
    }

    pub fn papertrail_sync_status(&self) -> anyhow::Result<PapertrailStatus> {
        self.papertrail_status()
    }

    pub fn papertrail_for_chunk(
        &self,
        chunk_id: i64,
        limit: u32,
    ) -> anyhow::Result<Option<Papertrail>> {
        let Some(chunk) = self.read_chunk(chunk_id)? else {
            return Ok(None);
        };
        let chunk_ref = papertrail::ChunkRef {
            path: &chunk.path,
            chunk_id: chunk.chunk_id,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            symbol_path: chunk.symbol_path.as_deref(),
        };
        Ok(Some(papertrail::papertrail_for_chunk(
            self.storage.connection(),
            &chunk_ref,
            limit,
            &self.papertrail,
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
        Ok(Some(papertrail::papertrail_for_symbol(
            self.storage.connection(),
            &papertrail::SymbolRef {
                path: &symbol.path,
                qualified_name: &symbol.qualified_name,
                symbol_path: &symbol.symbol_path,
            },
            limit,
            &self.papertrail,
        )?))
    }

    pub fn papertrail_for_selected_symbol(
        &self,
        symbol: &rag_rat_query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Papertrail> {
        papertrail::papertrail_for_symbol(
            self.storage.connection(),
            &papertrail::SymbolRef {
                path: &symbol.path,
                qualified_name: &symbol.qualified_name,
                symbol_path: &symbol.symbol_path,
            },
            limit,
            &self.papertrail,
        )
    }

    pub fn papertrail_for_commit(
        &self,
        commit_hash: &str,
        limit: u32,
    ) -> anyhow::Result<Papertrail> {
        papertrail::papertrail_for_commit(
            self.storage.connection(),
            commit_hash,
            limit,
            &self.papertrail,
        )
    }
}
