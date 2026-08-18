use rag_rat_base::hash::hex_sha256;
use rag_rat_base::paths::path_string;
use rag_rat_db::schema;
use rag_rat_query::memory::AnchorHealth;
use rag_rat_query::text_compare::*;
use rusqlite::OptionalExtension;

use super::*;
use crate::index::staleness::Heal;
use crate::search::lexical::SearchOptions;

mod ai_lifecycle;
mod clones;
mod db_file_health;
mod dream;
mod gc;
mod global_status;
mod graph;
mod history;
mod importance;
mod lens;
mod memory;
mod oracle_runs;
mod search;

// Crate-internal: member-cap constants, re-exported so the schema-bootstrap tests can assert
// the capped-class semantics by name (instead of hardcoding 50). Keeps the `clones` module
// private so `build_class`'s reachability stays narrow (no `private_interfaces` widening of
// `SymbolBag`).
pub(crate) use clones::delta::CloneDeltaHint;
pub use clones::delta::{CLONE_DELTA_MAX_FILES, CloneDeltaReport};
pub use clones::of_text::{CloneCheckInput, CloneFingerprintHealth, TextCloneMatch};
pub use clones::precompute::CloneEdgeReport;
pub use clones::{
    CandidateCloneClass, CloneCompleteness, CloneEligibility, CloneIneligibilityReason,
    CloneMember, CloneSymbolSelector, ClonesForSymbolResult, FindClonesOptions, FindClonesResult,
    RoiFactors,
};
#[cfg(test)]
pub(crate) use clones::{MAX_MEMBERS, MEMBER_VALUE_CAP};
pub use db_file_health::{
    DatabaseFileHealth, FreelistReclaim, FreelistReclaimReport, WAL_CHECKPOINT_MIN_BYTES,
    WalCheckpointReport, reclaim_freelist_at,
};
pub use gc::GcReport;
pub use global_status::{
    GlobalFtsStatus, GlobalStatus, MemoryCounts, MemoryKindCounts, PapertrailCursor, RepoContent,
    RepoFreshness, RepoPapertrail, RepoStatus, WorktreeOverlay,
};
pub use importance::ImportantSymbolsRequest;
pub use lens::clones::{
    LensCloneGraphMeta, LensClonePartner, LensCloneRefine, LensCloneRegion, LensFileClones,
};
pub use lens::{
    LensCallees, LensCallers, LensChunkText, LensCloneGraphCache, LensCouplingPartner,
    LensDecisionRecord, LensDispatchDetail, LensFileAnswer, LensFileCoupling, LensFileGraph,
    LensFileMemories, LensFileMemory, LensFilePapertrail, LensFileSymbolGraph, LensFileSymbols,
    LensGraphCallerCounts, LensHopResolvedBy, LensHopSelector, LensLaneVersions, LensPapertrailRef,
    LensStatus, LensSymbol, LensSymbolHop, LensTreemap, LensTreemapFile, LensVersion,
};
pub use memory::SyncCatchUpReport;
pub use oracle_runs::OracleShaSnapshots;
pub use search::SearchRequest;

/// Volume cap on the memories `read_chunk` attaches as drive-by context. The binding is
/// structural, so every hit is relevant; the cap is purely about how much of a reader's attention
/// one chunk read may spend (the grep-augment hook lanes budget 4).
///
/// A cap this tight makes the ordering load-bearing: `memories_for_chunk` returns chunk-bound
/// memories ahead of the file's path-bound ones, so a recently-touched file-level note cannot
/// spend the last slot the chunk's own memory needed.
const DRIVE_BY_CHUNK_MEMORY_LIMIT: u32 = 6;

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
        // GLOBAL keys (V040 reclassification): `chunk_fts` is one global FTS5 index and
        // `content_revision()` digests the whole `main.files`, so their freshness lives in
        // `index_meta` (`self.meta`), never per-repo `repo_meta`.
        let fts_source_revision = self.meta("fts_source_revision")?;
        let fts_dirty = self.fts_dirty()?;

        Ok(IndexStatus {
            database: database.display().to_string(),
            exists: database.exists(),
            schema: schema::status(self.storage.connection())?,
            git_commit: self.repo_meta("git_commit")?,
            git_dirty: self.repo_meta("git_dirty")?.map(|value| value == "true"),
            indexed_at_ms: self
                .repo_meta("indexed_at_ms")?
                .and_then(|value| value.parse::<i64>().ok()),
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
            watch_placement_failures: self.watch_placement_failures()?,
            git_history: self.git_history_status()?,
            papertrail: self.papertrail_status()?,
            llm: self.llm_status()?,
            anchor_health: rag_rat_query::memory::anchor_health_counts(self.storage.connection())
                .unwrap_or_default(),
        })
    }

    /// Read-only count of active repo-memory bindings grouped by anchor_status.
    /// Does not run `memory_validate`; reads persisted anchor_status values only.
    pub fn memory_anchor_health(&self) -> anyhow::Result<AnchorHealth> {
        rag_rat_query::memory::anchor_health_counts(self.storage.connection())
    }

    pub fn storage_status(&self) -> anyhow::Result<StorageStatus> {
        self.storage.status()
    }

    pub fn discovery_status(&self, config: &Config) -> anyhow::Result<DiscoveryStatus> {
        // The plan's carry filter needs the working tree's status (dirty/untracked paths are
        // never carried), so status computes it exactly like the discover pass does — keeping
        // the reported counts identical to what that pass would do.
        let changes = git_changed_paths(&config.root).unwrap_or_default();
        let plan = discovery_plan(self.storage.connection(), config, &changes)?;
        let unindexed_source_files =
            plan.unindexed.iter().filter(|file| file.kind == TargetKind::Source).count();
        let unindexed_sample =
            plan.unindexed.iter().take(10).map(|file| path_string(&file.relative_path)).collect();
        // A pending carry (#502) is pending work too: the retained rows are not in the active
        // scope until a discover pass re-stamps them, so a carry-only HEAD move must not read as
        // a clean index (a watcher-less install would otherwise report clean while queries at
        // the new HEAD miss those files). The unindexed warning wins when both apply — its
        // remedy covers the carry as well.
        let warning = if unindexed_source_files > 0 {
            Some(format!(
                "{unindexed_source_files} unindexed source files detected. Run `rag-rat index \
                 --full` or `rag-rat index --discover`."
            ))
        } else if !plan.carried.is_empty() {
            Some(format!(
                "{} indexed files await adoption onto the current HEAD (it moved since the last \
                 pass). Run `rag-rat index --discover`.",
                plan.carried.len()
            ))
        } else {
            None
        };
        Ok(DiscoveryStatus {
            discovered_files: plan.discovered_files,
            indexed_files: plan.indexed_files,
            unindexed_files: plan.unindexed.len(),
            unindexed_source_files,
            carryable_files: plan.carried.len(),
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
    ) -> anyhow::Result<Vec<rag_rat_query::symbol::SymbolHit>> {
        let mut hits =
            rag_rat_query::symbol::lookup(self.storage.connection(), name, language, limit)?;
        self.enrich_symbol_hits_with_load_bearing(&mut hits)?;
        Ok(hits)
    }

    pub fn symbol_candidates(
        &self,
        selector: &rag_rat_query::symbol::SymbolSelector,
        include_generated: bool,
    ) -> anyhow::Result<rag_rat_query::symbol::SymbolLookup> {
        let mut lookup = rag_rat_query::symbol::lookup_candidates(
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
            lookup = rag_rat_query::symbol::lookup_candidates(
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
            let healed = rag_rat_query::symbol::lookup_candidates(
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
            // Defer: this heal re-parsed only the STALE files, so it must not stamp the
            // logical-key version — untouched files' drift is still in the future (#493).
            self.rebuild_logical_symbols(graph_index::KeyVersionStamp::Defer)?;
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
        selector: &rag_rat_query::symbol::SymbolSelector,
    ) -> anyhow::Result<
        Result<
            Option<rag_rat_query::symbol::SymbolHit>,
            rag_rat_query::symbol::SymbolDisambiguation,
        >,
    > {
        rag_rat_query::symbol::select_one(self.storage.connection(), selector)
    }

    /// Resolve a selector to a single symbol for `memory rebind`, collapsing a cfg-split / overload
    /// group (all candidates sharing one logical symbol) to one member instead of disambiguating.
    pub fn select_symbol_for_bind(
        &self,
        selector: &rag_rat_query::symbol::SymbolSelector,
    ) -> anyhow::Result<
        Result<
            Option<rag_rat_query::symbol::SymbolHit>,
            rag_rat_query::symbol::SymbolDisambiguation,
        >,
    > {
        rag_rat_query::symbol::select_one_for_bind(self.storage.connection(), selector)
    }

    pub fn read_chunk(&self, chunk_id: i64) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        // Internal/CLI/test entry — always the FULL memory bodies; the surface-aware path is the
        // MCP `read_chunk` tool, which calls `read_chunk_with_graph_and_memories` with the
        // config surface.
        self.read_chunk_with_graph_and_memories(
            chunk_id,
            GraphMetaMode::Full,
            20,
            true,
            rag_rat_base::config::MemorySurface::Full,
        )
    }

    pub fn read_chunk_with_graph(
        &self,
        chunk_id: i64,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
    ) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        // `include_memories = false`, so the surface never applies — pass `Full`.
        self.read_chunk_with_graph_and_memories(
            chunk_id,
            graph_mode,
            graph_limit,
            false,
            rag_rat_base::config::MemorySurface::Full,
        )
    }

    pub fn read_chunk_with_graph_and_memories(
        &self,
        chunk_id: i64,
        graph_mode: GraphMetaMode,
        graph_limit: u32,
        include_memories: bool,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
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
            let conn = self.storage.connection();
            // Drive-by chunk attachments honor `[memory] surface`: under `Summary` each memory's
            // body is deferred to `memory show`, leaving the summary + verdict marker
            // (title-only fallback). #582: the Summary hydration runs a RANKED chunk_fts query —
            // heal-and-retry.
            chunk.memories = crate::index::retry_once_on_fts_corruption(
                || {
                    let mut memories = rag_rat_query::memory::memories_for_chunk(
                        conn,
                        chunk_id,
                        DRIVE_BY_CHUNK_MEMORY_LIMIT,
                    )?;
                    rag_rat_query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                    Ok(memories)
                },
                || self.heal_corrupt_fts(),
            )?;
            // Distilled decision records (#705 drive-by) for the symbol this chunk defines, capped
            // ≤2 and labeled unreviewed. Rides the memories flag; facet-gated, so empty for almost
            // every chunk — matches the symbol_lookup convention (a lightweight per-item surface).
            chunk.distilled_records = self.records_for_chunk_symbol(chunk_id, 2)?;
        }
        Ok(Some(chunk))
    }

    pub(crate) fn read_chunk_current(
        &self,
        chunk_id: i64,
    ) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        let dicts = rag_rat_query::chunk_text_dicts(self.storage.connection())?;
        let mut decoder = rag_rat_db::text_compression::ChunkTextDecoder::new(&dicts);
        self.read_chunk_current_with(chunk_id, &mut decoder)
    }

    /// Live-revalidating chunk read that resolves text through a caller-owned dict decoder (reused
    /// across a batch) rather than reloading the dict versions per call.
    pub(crate) fn read_chunk_current_with(
        &self,
        chunk_id: i64,
        decoder: &mut rag_rat_db::text_compression::ChunkTextDecoder,
    ) -> anyhow::Result<Option<rag_rat_query::ReadChunk>> {
        let Some(mut chunk) =
            rag_rat_query::read_chunk_with(self.storage.connection(), chunk_id, decoder)?
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
                // #767 review: the gated variant — a stale-scope read path must not stamp a
                // `kind='deleted'` row for a repo `rag-rat rm` already purged.
                self.mark_file_deleted_if_not_removed(Path::new(&path))?;
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
                let healed = rag_rat_query::read_chunk(self.storage.connection(), chunk_id)?;
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
                fts_healed: Vec::new(),
                fts_deferred: Vec::new(),
                message: Some(
                    "skipped: heal does not run under a linked-worktree overlay scope".to_string(),
                ),
            });
        }
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("heal_index requires source_root metadata; run `rag-rat index` first");
        };
        // #767 review: fail closed when the active repo was `rag-rat rm`-removed after this
        // connection resolved its scope (a stale MCP `heal_index` writer). This is the PREFLIGHT
        // that stops the batch before any per-file work; the AUTHORITATIVE checks live at each
        // per-file write boundary — `heal_file` and `mark_file_deleted_if_not_removed` re-check
        // the tombstone inside their IMMEDIATE mutation transactions, which serialize with rm's
        // purge on the SQLite write lock (the heal path deliberately stays flock-free so it can
        // run alongside a mid-flight rebuild).
        let conn = self.storage.connection();
        let active_repo_id = rag_rat_db::schema::active_repo_id(conn)?;
        crate::index::remove::assert_repo_not_removed(conn, &active_repo_id)?;
        let indexed_files = self.indexed_files()?;
        let max_repairs = limit.map(usize::try_from).transpose()?.unwrap_or(usize::MAX);
        let mut report = HealIndexReport {
            checked_files: 0,
            healed_files: 0,
            removed_files: 0,
            skipped_files: 0,
            fts_fresh: false,
            fts_healed: Vec::new(),
            fts_deferred: Vec::new(),
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
                self.mark_file_deleted_if_not_removed(path)?;
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

        // #828 §9.1: heal is the explicit operator remedy — verify the content-digest parity and
        // reseed `content_digest_state` on drift BEFORE the FTS freshness pass below, so
        // `ensure_fts_fresh` compares against a healed digest rather than a drifted one.
        self.verify_content_digest_parity()?;

        // Probe for FTS shadow corruption BEFORE the freshness pass (#582): a dirty-flagged
        // index would otherwise be incidentally repaired by `ensure_fts_fresh`'s rebuild and the
        // report would under-attribute what was actually corrupt.
        let fts_outcome = self.heal_fts_if_corrupt()?;
        report.fts_healed = fts_outcome.healed;
        report.fts_deferred = fts_outcome.deferred;
        if report.healed_files > 0 || report.removed_files > 0 {
            self.sync_fts()?;
        } else {
            self.ensure_fts_fresh()?;
        }
        // A deferred corrupt mirror is NOT fresh, whatever the dirty flag says — operators key
        // health off this field.
        report.fts_fresh = !self.fts_dirty()? && report.fts_deferred.is_empty();
        Ok(report)
    }

    pub fn repo_brief(
        &self,
        options: rag_rat_query::repo_brief::RepoBriefOptions,
    ) -> anyhow::Result<rag_rat_query::repo_brief::RepoBrief> {
        rag_rat_query::repo_brief::repo_brief(self.storage.connection(), options)
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
fn selector_is_name_based(selector: &rag_rat_query::symbol::SymbolSelector) -> bool {
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
    rag_rat_oracle::package_of(scip_symbol).map(|package| format!("resolved-external({package})"))
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
    summary: &mut rag_rat_query::graph::GraphTraversalSummary,
    hops: &[rag_rat_query::graph::GraphHop],
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
mod drive_by_memory_cap_tests {
    use rag_rat_query::graph_meta::GraphMetaMode;
    use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
    use rusqlite::{Connection, params};

    use crate::index::IndexDatabase;

    /// A store holding one chunk of `src/a.rs` and the bare connection to seed memories through —
    /// the shape `read_chunk` attaches drive-by context to. Seeding happens before the
    /// [`IndexDatabase`] opens so it scopes to the seeded repo.
    fn store_with_one_chunk() -> (tempfile::TempDir, Connection, i64) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("index.sqlite")).unwrap();
        rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               commit_sha, worktree_id, repo_id, generation)
             VALUES ('src/a.rs', 'rust', 'source', 'h', 0, 0, '', '', 'r', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text_hash)
             VALUES (1, 'symbol', 'a::foo', 0, 10, 1, 2, 'h1')",
            [],
        )
        .unwrap();
        let chunk_id = conn.last_insert_rowid();
        rag_rat_db::chunk_text_store::seed_chunk_text(&conn, chunk_id, "fn foo() {}").unwrap();
        (dir, conn, chunk_id)
    }

    /// Opens the seeded store, releasing the seeding connection first.
    fn open_seeded(dir: &tempfile::TempDir, conn: Connection) -> IndexDatabase {
        drop(conn);
        IndexDatabase::open(&dir.path().join("index.sqlite")).unwrap()
    }

    fn create_memory_bound_to(
        conn: &Connection,
        title: &str,
        bind: RepoMemoryBindTarget,
    ) -> String {
        crate::memory_write::create_memory(conn, RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: title.to_string(),
            body: format!("Body of {title}."),
            confidence: "high".to_string(),
            created_by: Some("test".to_string()),
            source: None,
            tags: vec![],
            payload_json: None,
            bind,
        })
        .unwrap()
        .memory
        .memory_id
    }

    /// `create_memory` stamps the wall clock, so a test that ranks on recency sets the timestamps
    /// itself rather than relying on creation order landing in distinct milliseconds.
    fn stamp_updated_at(conn: &Connection, memory_id: &str, updated_at_ms: i64) {
        conn.execute("UPDATE repo_memories SET updated_at_ms = ?2 WHERE id = ?1", params![
            memory_id,
            updated_at_ms
        ])
        .unwrap();
    }

    fn db_with_chunk_memories(n: usize) -> (tempfile::TempDir, IndexDatabase, i64) {
        let (dir, conn, chunk_id) = store_with_one_chunk();
        for i in 0..n {
            create_memory_bound_to(&conn, &format!("Drive-by memory {i}"), RepoMemoryBindTarget {
                chunk_id: Some(chunk_id),
                ..RepoMemoryBindTarget::default()
            });
        }
        let db = open_seeded(&dir, conn);
        (dir, db, chunk_id)
    }

    /// #1200: `read_chunk` used to attach up to 20 memories per chunk — enough to bury the chunk
    /// itself. The bindings are structural, so this is a volume cap, not a relevance gate, and it
    /// belongs to the read path: the test drives the real `read_chunk` call so raising the literal
    /// back at the call site fails it.
    #[test]
    fn read_chunk_attaches_at_most_the_drive_by_cap() {
        let (_dir, db, chunk_id) = db_with_chunk_memories(9);
        let chunk = db
            .read_chunk_with_graph_and_memories(
                chunk_id,
                GraphMetaMode::Full,
                20,
                true,
                rag_rat_base::config::MemorySurface::Full,
            )
            .unwrap()
            .expect("chunk");
        assert_eq!(
            chunk.memories.len(),
            usize::try_from(super::DRIVE_BY_CHUNK_MEMORY_LIMIT).unwrap(),
            "9 bound memories, capped to the drive-by limit"
        );
    }

    /// The cap makes the ranking load-bearing. A chunk binding names THIS code; a path binding
    /// names the whole file, and a file accrues far more of them. Ranked on recency alone, file
    /// notes touched after the chunk's own memory take every slot, and the read that exists to
    /// surface the specific anchor never shows it.
    #[test]
    fn a_chunk_bound_memory_outranks_the_files_newer_path_bound_ones() {
        let (dir, conn, chunk_id) = store_with_one_chunk();
        let chunk_bound =
            create_memory_bound_to(&conn, "The chunk's own invariant", RepoMemoryBindTarget {
                chunk_id: Some(chunk_id),
                ..RepoMemoryBindTarget::default()
            });
        stamp_updated_at(&conn, &chunk_bound, 0);
        // More than the cap, every one of them newer than the chunk-bound memory.
        for i in 0..8i64 {
            let path_bound = create_memory_bound_to(
                &conn,
                &format!("File-level note {i}"),
                RepoMemoryBindTarget {
                    path: Some("src/a.rs".to_string()),
                    ..RepoMemoryBindTarget::default()
                },
            );
            stamp_updated_at(&conn, &path_bound, 1_000 + i);
        }
        let db = open_seeded(&dir, conn);

        let chunk = db
            .read_chunk_with_graph_and_memories(
                chunk_id,
                GraphMetaMode::Full,
                20,
                true,
                rag_rat_base::config::MemorySurface::Full,
            )
            .unwrap()
            .expect("chunk");

        let ids: Vec<&str> = chunk.memories.iter().map(|m| m.memory_id.as_str()).collect();
        assert_eq!(ids.len(), usize::try_from(super::DRIVE_BY_CHUNK_MEMORY_LIMIT).unwrap());
        assert_eq!(
            ids.first().copied(),
            Some(chunk_bound.as_str()),
            "the chunk's own binding leads eight newer path-bound notes: {ids:?}"
        );
    }
}

#[cfg(test)]
mod name_based_tests {
    use rag_rat_query::symbol::SymbolSelector;

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
        let token = rag_rat_base::serde_big_id::format_sym_handle(0x688b_7144_3793_b726_u64 as i64);
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
