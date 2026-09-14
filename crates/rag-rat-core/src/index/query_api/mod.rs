use rag_rat_base::paths::path_string;
use rag_rat_db::schema;
use rag_rat_query::memory::AnchorHealth;
use rag_rat_query::text_compare::*;
use rusqlite::OptionalExtension;

use super::*;
use crate::index::staleness::Heal;
use crate::search::lexical::SearchOptions;

mod ai_lifecycle;
mod chunk_read;
mod clones;
mod db_file_health;
mod dream;
mod gc;
mod global_status;
mod graph;
mod heal;
mod history;
mod importance;
mod lens;
mod memory;
mod oracle_runs;
mod search;
mod sync;

// Crate-internal: member-cap constants, re-exported so the schema-bootstrap tests can assert
// the capped-class semantics by name (instead of hardcoding 50). Keeps the `clones` module
// private so `build_class`'s reachability stays narrow (no `private_interfaces` widening of
// `SymbolBag`).
pub use chunk_read::ReadChunkRequest;
pub(crate) use clones::delta::CloneDeltaHint;
pub use clones::delta::{CLONE_DELTA_MAX_FILES, CloneDeltaReport, CloneDeltaStatus};
pub use clones::of_text::{CloneCheckInput, CloneFingerprintHealth, TextCloneMatch};
pub use clones::precompute::{CloneEdgeReport, CloneEdgeStatus};
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
pub use search::SearchRequest;
pub use sync::{PublishSeedReport, SyncCatchUpReport};

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
            .read_chunk_with(crate::index::ReadChunkRequest {
                chunk_id,
                graph_mode: GraphMetaMode::Full,
                graph_limit: 20,
                include_memories: true,
                surface: rag_rat_base::config::MemorySurface::Full,
            })
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
            .read_chunk_with(crate::index::ReadChunkRequest {
                chunk_id,
                graph_mode: GraphMetaMode::Full,
                graph_limit: 20,
                include_memories: true,
                surface: rag_rat_base::config::MemorySurface::Full,
            })
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
