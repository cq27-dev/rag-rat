//! Repo-memory query surface on `IndexDatabase`: create/update/obsolete, search, anchor resolution
//! (by symbol / path / call-path), rebind, and the validate/doctor anchor-health passes.

use super::*;

impl IndexDatabase {
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

    /// Add a typed graph edge from a source node to another node or a GitHub issue (#464).
    pub fn memory_edge_add(
        &self,
        source_node_id: &str,
        relation: &str,
        target: crate::query::memory::EdgeTarget,
    ) -> anyhow::Result<crate::query::memory::NodeEdge> {
        let relation = crate::query::memory::EdgeRelation::from_db_str(relation)?;
        crate::query::memory::add_edge(self.storage.connection(), source_node_id, relation, &target)
    }

    /// Remove a graph edge by its stable `edge_key` (#464). `false` when the key is unknown.
    pub fn memory_edge_remove(&self, edge_key: &str) -> anyhow::Result<bool> {
        crate::query::memory::remove_edge(self.storage.connection(), edge_key)
    }

    /// Every edge OUT of a node — its outgoing graph (deps / mind-map links / tracks) (#464).
    pub fn memory_edges_from(
        &self,
        source_node_id: &str,
    ) -> anyhow::Result<Vec<crate::query::memory::NodeEdge>> {
        crate::query::memory::edges_from(self.storage.connection(), source_node_id)
    }

    /// Every edge INTO a target — the reverse traversal (e.g. tasks tracking a github issue)
    /// (#464).
    pub fn memory_edges_into(
        &self,
        target: crate::query::memory::EdgeTarget,
    ) -> anyhow::Result<Vec<crate::query::memory::NodeEdge>> {
        crate::query::memory::edges_into(self.storage.connection(), &target)
    }

    pub fn memory_search(
        &self,
        query: &str,
        limit: u32,
        surface: crate::config::MemorySurface,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: both the MATCH and the surface hydration (whose Summary path runs a RANKED
        // chunk_fts query) can hit FTS shadow corruption; heal-and-retry rather than surfacing
        // a bare "database disk image is malformed" forever.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = crate::query::memory::memory_search(conn, query, limit)?;
                crate::query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_for_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
        surface: crate::config::MemorySurface,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = crate::query::memory::memories_for_symbol(conn, symbol, limit)?;
                crate::query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_for_path(
        &self,
        path: &str,
        limit: u32,
        surface: crate::config::MemorySurface,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = crate::query::memory::memories_for_path(conn, path, limit)?;
                crate::query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_for_edges(
        &self,
        edge_ids: &[i64],
        limit: u32,
        surface: crate::config::MemorySurface,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = crate::query::memory::memories_for_edges(conn, edge_ids, limit)?;
                crate::query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_evidence_for_symbol_and_edges(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        caller_edge_ids: &[i64],
        callee_edge_ids: &[i64],
        limit: u32,
        surface: crate::config::MemorySurface,
    ) -> anyhow::Result<crate::query::memory::RepoMemoryEvidence> {
        // This wrapper exposes only the evidence; the impact builder consumes the truncation flag
        // directly from the core fn. `find_callers` / `trace_callees` emit the evidence FULL (not
        // compact), so honor `[memory] surface` here by deferring each lane's bodies under
        // `Summary`.
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut evidence = crate::query::memory::memory_evidence_for_symbol_and_edges(
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
        surface: crate::config::MemorySurface,
    ) -> anyhow::Result<Vec<crate::query::memory::RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = crate::query::memory::memories_for_call_path_hash(
                    conn,
                    edge_sequence_hash,
                    limit,
                )?;
                crate::query::memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
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
