use rusqlite::OptionalExtension;

use super::*;
use crate::index::oracle;
use crate::index::staleness::Heal;
use crate::query::text_compare::*;
use crate::search::lexical::SearchOptions;

mod ai_lifecycle;
mod clones;
mod gc;
mod graph;
mod history;
mod importance;
mod memory;
mod oracle_runs;
mod search;

#[allow(unused_imports)] // consumed by MCP/CLI layers (Plan-2 T5/T6)
pub use clones::{
    CandidateCloneClass, CloneCompleteness, CloneMember, CloneSymbolSelector, FindClonesOptions,
    FindClonesResult, RoiFactors,
};
pub use gc::GcReport;
pub use importance::ImportantSymbolsRequest;
pub use oracle_runs::OracleShaSnapshots;
pub use search::SearchRequest;

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
        include_generated: bool,
    ) -> anyhow::Result<crate::query::symbol::SymbolLookup> {
        let mut lookup = crate::query::symbol::lookup_candidates(
            self.storage.connection(),
            selector,
            include_generated,
        )?;
        // #152: a name/symbol_path lookup that found NOTHING may be a just-added symbol the watcher
        // hasn't indexed yet. Index the working-tree change set (bounded) and re-resolve once.
        // Name-based selectors only — a miss on a churning id isn't "newly added".
        if lookup.candidates.is_empty()
            && selector_is_name_based(selector)
            && self.heal_changed_for_zero_hit()?
        {
            lookup = crate::query::symbol::lookup_candidates(
                self.storage.connection(),
                selector,
                include_generated,
            )?;
        }
        // #147: symbol rows aren't anchor-relocated like chunks, so a file edited since indexing
        // returns stale line numbers. Heal the matched files inline (bounded, like
        // search_with_heal) and re-resolve so positions/ids are current; #148: report any
        // file still dirty after.
        let paths: Vec<String> = lookup.candidates.iter().map(|c| c.path.clone()).collect();
        let stale = self.stale_source_paths(&paths)?;
        if !stale.is_empty() {
            self.heal_stale_paths(&stale)?; // NeedsReindex beyond the cap
            let healed = crate::query::symbol::lookup_candidates(
                self.storage.connection(),
                selector,
                include_generated,
            )?;
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
                 WHERE symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
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
        // Under a LINKED-WORKTREE OVERLAY scope, `source_root` is the MAIN checkout — NOT the
        // branch these rows came from. Hashing the overlay rows against main's copy reports every
        // branch-changed file as stale even though the overlay is current, which makes
        // symbol_lookup's matched-file heal trip `NeedsReindex` (and `heal_file` no-ops under the
        // overlay anyway) and impact's `stale_files` caveat lie. The overlay rows are authoritative
        // (maintained by `index_worktree_overlay`), so report nothing stale — same rationale as the
        // read_chunk overlay skip (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(Vec::new());
        }
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

    /// #152: a name lookup that found NOTHING may be a symbol just added (or renamed into
    /// existence) the watcher hasn't indexed yet. Index the working-tree change set — changed
    /// source files that are dirty-vs-index OR not-yet-indexed — bounded by
    /// `MAX_AUTO_HEAL_FILES_PER_CALL` (over the cap → do nothing; never raise `NeedsReindex`, since
    /// the common zero-hit is a plain typo'd miss that must stay cheap), then re-derive logical
    /// symbols + edges so the re-resolve sees the new symbol. Returns whether anything was indexed.
    /// No-op without a stored `Config` (rebuild/open/tests) or a git root — a genuine miss on a
    /// clean tree costs at most one `git status` and no write.
    fn heal_changed_for_zero_hit(&self) -> anyhow::Result<bool> {
        // Under a LINKED-WORKTREE OVERLAY scope this would scan `config.root` (the MAIN checkout)
        // for working-tree changes and index them into the overlay scope — main's edits, not the
        // branch's. The overlay is maintained by `index_worktree_overlay`; leave the heal to it
        // (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(false);
        }
        let Some(config) = self.config.as_ref() else {
            return Ok(false);
        };
        let Ok(changes) = crate::index::git_changed_paths(&config.root) else {
            return Ok(false);
        };
        if changes.changed.is_empty() {
            return Ok(false);
        }
        // Classify changed paths against the indexed targets (include/exclude/language), keeping
        // only those dirty-vs-index OR not-yet-indexed — a file already current adds nothing.
        let mut healable = Vec::new();
        for file in collect_changed_index_files(config, &changes)? {
            let needs_index = match self.indexed_sha_for_path(&path_string(&file.relative_path))? {
                None => true,
                Some(indexed) => match fs::read(&file.full_path) {
                    Ok(bytes) => hex_sha256(&bytes) != indexed,
                    Err(_) => false,
                },
            };
            if needs_index {
                healable.push(file);
            }
            if healable.len() > MAX_AUTO_HEAL_FILES_PER_CALL {
                // Too many newly-changed files to heal inline on a lookup miss — leave it to the
                // watcher rather than turn a read into a large rebuild.
                return Ok(false);
            }
        }
        if healable.is_empty() {
            return Ok(false);
        }
        let files = self.assign_file_scopes(healable, &changes);
        // Apply with the SAME write discipline as the incremental indexer
        // (`index_incremental_with_progress`), not a corner-cut version (PR #158 review):
        //  - one `BEGIN IMMEDIATE` txn so a concurrent reader never observes the index between the
        //    logical-symbol DELETE-all and its rebuild (which would return empty logical handles);
        //  - apply `changes.deleted` so a removed file's stale rows don't survive the re-resolve
        //    and let the new file's edges bind to a deleted definition;
        //  - `refresh_packages` BEFORE `resolve_edges`, since per-package import scope (#61) is
        //    read at resolve time — a change set that adds/edits a Cargo.toml must resolve against
        //    the fresh package map, not the stale one.
        // On the read-only MCP connection the BEGIN trips SQLITE_READONLY and the dispatch retries
        // read-write (like the #147 heal), so the txn always runs on a writable connection.
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<()> {
            self.apply_incremental_file_plan(files, changes.deleted.clone(), &mut |_| {})?;
            self.refresh_packages(&config.root)?;
            self.rebuild_logical_symbols()?;
            self.resolve_edges()?;
            self.sync_fts()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.storage.execute_batch("COMMIT")?;
                Ok(true)
            },
            Err(error) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                Err(error)
            },
        }
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

    pub(crate) fn read_chunk_current(
        &self,
        chunk_id: i64,
    ) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        let dicts = crate::query::chunk_text_dicts(self.storage.connection())?;
        let mut decoder = crate::index::text_compression::ChunkTextDecoder::new(&dicts);
        self.read_chunk_current_with(chunk_id, &mut decoder)
    }

    /// Live-revalidating chunk read that resolves text through a caller-owned dict decoder (reused
    /// across a batch) rather than reloading the dict versions per call.
    pub(crate) fn read_chunk_current_with(
        &self,
        chunk_id: i64,
        decoder: &mut crate::index::text_compression::ChunkTextDecoder,
    ) -> anyhow::Result<Option<crate::query::ReadChunk>> {
        let Some(mut chunk) =
            crate::query::read_chunk_with(self.storage.connection(), chunk_id, decoder)?
        else {
            return Ok(None);
        };
        // Under a LINKED-WORKTREE OVERLAY scope, `source_root` is the MAIN checkout — NOT the
        // branch the chunk came from. Live-revalidating against main would slice the chunk
        // text out of main's copy of the file (returning BASE text for a branch chunk
        // whenever the anchor still matches), or call the overlay-guarded `heal_file`
        // no-op. The overlay rows are maintained by `index_worktree_overlay` (read from the
        // linked checkout), so the STORED text is already the branch's — return it as-is
        // and skip live revalidation (#219 review). The base/main scope keeps full live
        // revalidation below.
        if self.active_scope_is_linked_overlay() {
            return Ok(Some(chunk));
        }
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

    pub fn heal_index(&self, limit: Option<u32>) -> anyhow::Result<HealIndexReport> {
        // `heal_index` reads file bytes from `source_root` (the MAIN checkout) and would write into
        // the active scope. Under a linked-worktree overlay scope that reindexes the overlay with
        // MAIN's contents or tombstones branch-only files, so refuse — the overlay is owned by
        // `index_worktree_overlay`. Callers scope writes to the base (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(HealIndexReport {
                checked_files: 0,
                healed_files: 0,
                removed_files: 0,
                skipped_files: 0,
                fts_fresh: !self.fts_dirty()?,
                message: Some(
                    "skipped: heal does not run under a linked-worktree overlay scope".to_string(),
                ),
            });
        }
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

/// Whether a selector resolves by NAME (`symbol` / `symbol_path`) rather than by a reindex-churning
/// id. Only name lookups get the #152 zero-hit heal: a miss on a `symbol_id`/`logical_symbol_id`
/// isn't a "just added" symbol, just a stale or wrong id, so re-indexing the change set wouldn't
/// recover it and would put a `git status` on every such miss.
fn selector_is_name_based(selector: &crate::query::symbol::SymbolSelector) -> bool {
    // A `sym_<hex>` handle in the ref/symbol_path slot is id-based (#201), not a name — exclude
    // both a handle that RESOLVES and one that's merely handle-SHAPED but malformed (typo/bad
    // hex). Either way it must fail cheaply like `id`, never be misread as a name/path miss
    // that trips the #152 zero-hit heal + reindex (which can't recover a handle anyway) on
    // every bad handle.
    selector.symbol_id.is_none()
        && selector.effective_logical_symbol_id().is_none()
        && !selector.ref_is_handle_shaped()
        && (selector.symbol.is_some() || selector.symbol_path.is_some())
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

#[cfg(test)]
mod name_based_tests {
    use crate::query::symbol::SymbolSelector;

    fn selector(symbol: Option<&str>, symbol_path: Option<&str>) -> SymbolSelector {
        SymbolSelector {
            logical_symbol_id: None,
            symbol_id: None,
            symbol_path: symbol_path.map(str::to_string),
            symbol: symbol.map(str::to_string),
            language: None,
            allow_ambiguous: false,
            limit: 10,
        }
    }

    #[test]
    fn ref_slot_handle_is_not_name_based() {
        // #201 review (P2): a `sym_<hex>` handle in the ref/symbol_path slot resolves as a logical
        // id, so it must be treated as id-based — a stale handle then fails cheaply instead of
        // tripping the #152 zero-hit heal/reindex meant for genuinely-new name/path lookups.
        let token = crate::serde_big_id::format_sym_handle(0x688b_7144_3793_b726_u64 as i64);
        assert!(!super::selector_is_name_based(&selector(None, Some(&token))));
        // A MALFORMED handle (typo/bad hex) is still handle-SHAPED → id-based, so it also cannot
        // trip the heal even though it doesn't parse (#201 review follow-up).
        assert!(!super::selector_is_name_based(&selector(None, Some("sym_zzzz"))));
        // A real qualified name in the same slot stays name-based (heal-eligible).
        assert!(super::selector_is_name_based(&selector(None, Some("crates/x/src/a.rs::foo"))));
        // A qualified name that merely STARTS with `sym_` (path-qualified, or a `sym_…/` path) is a
        // name, not a handle — it keeps the #152 heal (P3 review follow-up).
        assert!(super::selector_is_name_based(&selector(None, Some("sym_helpers.rs::build"))));
        assert!(super::selector_is_name_based(&selector(None, Some("sym_dir/mod.rs::build"))));
        // A bare name is name-based.
        assert!(super::selector_is_name_based(&selector(Some("foo"), None)));
    }
}
