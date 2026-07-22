//! Repo-memory query surface on `IndexDatabase`: create/update/obsolete, search, anchor resolution
//! (by symbol / path / call-path), rebind, and the validate/doctor anchor-health passes.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncCatchUpReport {
    pub target: rag_rat_oplog::DeviceFingerprint,
    pub required: u64,
    pub already_covered: u64,
    pub authored: u64,
}

impl IndexDatabase {
    /// Permanently enable sealed local memory authoring for this repo. Existing suite-0 history is
    /// retained; subsequent live and reconcile entries use suite 1.
    pub fn sync_enable(&self) -> anyhow::Result<bool> {
        crate::memory_write::enable_sealed_authoring(
            self.storage.connection(),
            rag_rat_base::time::now_ms(),
        )
    }

    /// Re-wrap the active repo stream's existing live keys to an already-effective enrolled device.
    /// This authors same-key siblings only; it does not enroll, pair, transport, or rotate keys.
    pub fn sync_catch_up(
        &self,
        target: rag_rat_oplog::DeviceFingerprint,
    ) -> anyhow::Result<SyncCatchUpReport> {
        let report = crate::memory_write::catch_up_enrolled_device_keys(
            self.storage.connection(),
            target,
            rag_rat_base::time::now_ms(),
        )?;
        Ok(SyncCatchUpReport {
            target: report.target,
            required: report.authored.len() as u64,
            already_covered: report.already_covered.len() as u64,
            authored: report.authored.len() as u64,
        })
    }

    pub fn memory_create(
        &self,
        request: rag_rat_query::memory::RepoMemoryCreate,
    ) -> anyhow::Result<rag_rat_query::memory::RepoMemoryCreateResult> {
        crate::memory_write::create_memory(self.storage.connection(), request)
    }

    pub fn memory_update(
        &self,
        update: rag_rat_query::memory::RepoMemoryUpdate,
    ) -> anyhow::Result<rag_rat_query::memory::RepoMemory> {
        crate::memory_write::update_memory(self.storage.connection(), update)
    }

    pub fn memory_mark_obsolete(
        &self,
        memory_id: &str,
    ) -> anyhow::Result<rag_rat_query::memory::RepoMemory> {
        crate::memory_write::mark_obsolete(self.storage.connection(), memory_id)
    }

    /// Add a typed graph edge from a source node to another node or a GitHub issue (#464).
    pub fn memory_edge_add(
        &self,
        source_node_id: &str,
        relation: &str,
        target: rag_rat_query::memory::EdgeTarget,
    ) -> anyhow::Result<rag_rat_query::memory::NodeEdge> {
        let relation = rag_rat_query::memory::EdgeRelation::from_db_str(relation)?;
        crate::memory_write::add_edge(self.storage.connection(), source_node_id, relation, &target)
    }

    /// Remove a graph edge by its stable `edge_key` (#464). `false` when the key is unknown.
    pub fn memory_edge_remove(&self, edge_key: &str) -> anyhow::Result<bool> {
        crate::memory_write::remove_edge(self.storage.connection(), edge_key)
    }

    /// Every edge OUT of a node — its outgoing graph (deps / mind-map links / tracks) (#464).
    pub fn memory_edges_from(
        &self,
        source_node_id: &str,
    ) -> anyhow::Result<Vec<rag_rat_query::memory::NodeEdge>> {
        rag_rat_query::memory::edges_from(self.storage.connection(), source_node_id)
    }

    /// Every edge INTO a target — the reverse traversal (e.g. tasks tracking a github issue)
    /// (#464).
    pub fn memory_edges_into(
        &self,
        target: rag_rat_query::memory::EdgeTarget,
    ) -> anyhow::Result<Vec<rag_rat_query::memory::NodeEdge>> {
        rag_rat_query::memory::edges_into(self.storage.connection(), &target)
    }

    pub fn memory_search(
        &self,
        query: &str,
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Vec<rag_rat_query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: both the MATCH and the surface hydration (whose Summary path runs a RANKED
        // chunk_fts query) can hit FTS shadow corruption; heal-and-retry rather than surfacing
        // a bare "database disk image is malformed" forever.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = rag_rat_query::memory::memory_search(conn, query, limit)?;
                rag_rat_query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_for_symbol(
        &self,
        symbol: &rag_rat_query::symbol::SymbolHit,
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Vec<rag_rat_query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = rag_rat_query::memory::memories_for_symbol(conn, symbol, limit)?;
                rag_rat_query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    /// Distilled decision records worth surfacing on a symbol (#705 drive-by), labeled unreviewed.
    /// Empty for a symbol with no resolved logical id (nothing to anchor a record to). Repo-scoped;
    /// the facet gate + cap live in `rag_rat_papertrail::records_for_symbol`.
    pub fn records_for_symbol(
        &self,
        symbol: &rag_rat_query::symbol::SymbolHit,
        limit: usize,
    ) -> anyhow::Result<Vec<rag_rat_papertrail::DriveByRecord>> {
        self.drive_by_records_for_logical_id(symbol.logical_symbol_id, limit)
    }

    /// Distilled decision records for the symbol a chunk defines (#705 drive-by on `read_chunk`).
    /// Resolves the chunk's file `path` + qualified `symbol_path` to a logical symbol, then the
    /// same facet-gated lane as [`records_for_symbol`]. Empty when the chunk defines no resolvable
    /// logical symbol. `pub(crate)`: only the `read_chunk` reader calls it (unlike the sibling
    /// `records_for_symbol`, which the MCP handler invokes directly).
    pub(crate) fn records_for_chunk_symbol(
        &self,
        path: &str,
        symbol_path: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<rag_rat_papertrail::DriveByRecord>> {
        let logical_symbol_id = rag_rat_query::memory::logical_symbol_id_for_chunk_symbol(
            self.storage.connection(),
            path,
            symbol_path,
        )?;
        self.drive_by_records_for_logical_id(logical_symbol_id, limit)
    }

    /// Shared drive-by fetch: the repo-scoped, facet-gated `records_for_symbol` lane over a
    /// resolved logical-symbol handle. `None`/unresolved id surfaces nothing (no anchor to bind
    /// a record to).
    fn drive_by_records_for_logical_id(
        &self,
        logical_symbol_id: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<rag_rat_papertrail::DriveByRecord>> {
        let Some(logical_symbol_id) = logical_symbol_id else {
            return Ok(Vec::new());
        };
        let conn = self.storage.connection();
        let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
        let records =
            rag_rat_papertrail::records_for_symbol(conn, &repo_id, logical_symbol_id, limit)?;
        Ok(records.into_iter().map(rag_rat_papertrail::DriveByRecord::new).collect())
    }

    pub fn memory_for_path(
        &self,
        path: &str,
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Vec<rag_rat_query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = rag_rat_query::memory::memories_for_path(conn, path, limit)?;
                rag_rat_query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_for_edges(
        &self,
        edge_ids: &[i64],
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Vec<rag_rat_query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories =
                    rag_rat_query::memory::memories_for_edges(conn, edge_ids, limit)?;
                rag_rat_query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_evidence_for_symbol_and_edges(
        &self,
        symbol: &rag_rat_query::symbol::SymbolHit,
        caller_edge_ids: &[i64],
        callee_edge_ids: &[i64],
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<rag_rat_query::memory::RepoMemoryEvidence> {
        // This wrapper exposes only the evidence; the impact builder consumes the truncation flag
        // directly from the core fn. `find_callers` / `trace_callees` emit the evidence FULL (not
        // compact), so honor `[memory] surface` here by deferring each lane's bodies under
        // `Summary`.
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut evidence = rag_rat_query::memory::memory_evidence_for_symbol_and_edges(
                    conn,
                    symbol,
                    caller_edge_ids,
                    callee_edge_ids,
                    limit,
                )
                .map(|(evidence, _truncated)| evidence)?;
                evidence.apply_surface(conn, surface)?;
                Ok(evidence)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_for_call_path_hash(
        &self,
        edge_sequence_hash: &str,
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Vec<rag_rat_query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = rag_rat_query::memory::memories_for_call_path_hash(
                    conn,
                    edge_sequence_hash,
                    limit,
                )?;
                rag_rat_query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_rebind(
        &self,
        memory_id: &str,
        bind: rag_rat_query::memory::RepoMemoryBindTarget,
    ) -> anyhow::Result<rag_rat_query::memory::RepoMemory> {
        crate::memory_write::rebind_memory(self.storage.connection(), memory_id, bind)
    }

    pub fn memory_validate(
        &self,
    ) -> anyhow::Result<rag_rat_query::memory::RepoMemoryValidationReport> {
        rag_rat_query::memory::validate_memories(
            self.storage.connection(),
            self.storage.source_root(),
        )
    }

    pub fn memory_doctor(&self) -> anyhow::Result<Vec<rag_rat_query::memory::MemoryDoctorEntry>> {
        rag_rat_query::memory::doctor_report(self.storage.connection())
    }

    /// Read-only list of active+stale memories, optionally filtered by binding_kind.
    /// `kind` filters by binding kind (e.g. `Some("dir")`); `None` returns all.
    pub fn memory_list(
        &self,
        kind: Option<&str>,
    ) -> anyhow::Result<Vec<rag_rat_query::memory::MemorySummary>> {
        rag_rat_query::memory::list_memories(self.storage.connection(), kind)
    }

    /// Fetch a single memory by id, returning `None` when not found.
    pub fn memory_get(
        &self,
        memory_id: &str,
    ) -> anyhow::Result<Option<rag_rat_query::memory::RepoMemory>> {
        rag_rat_query::memory::memory_by_id(self.storage.connection(), memory_id)
    }
}
