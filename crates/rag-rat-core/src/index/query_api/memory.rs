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
