//! Repo-memory query surface on `IndexDatabase`: create/update/obsolete, search, anchor resolution
//! (by symbol / path / call-path), rebind, and the validate/doctor anchor-health passes.

use rag_rat_query::memory::{
    self, EdgeRelation, EdgeTarget, MemoryDoctorEntry, MemorySummary, NodeEdge, RepoMemory,
    RepoMemoryBindTarget, RepoMemoryCreate, RepoMemoryCreateResult, RepoMemoryEvidence,
    RepoMemoryUpdate, RepoMemoryValidationReport,
};
use rag_rat_query::symbol::SymbolHit;

use super::*;

/// Near-duplicates reported per create.
const SIMILAR_MEMORY_LIMIT: usize = 3;

impl IndexDatabase {
    pub fn memory_create(
        &self,
        request: RepoMemoryCreate,
    ) -> anyhow::Result<RepoMemoryCreateResult> {
        let mut created = crate::memory_write::create_memory(self.storage.connection(), request)?;
        self.embed_written_memory(&created.memory.memory_id);
        // An exact duplicate is already flagged `duplicate`; the neighbour list is for new notes.
        if !created.duplicate {
            created.similar_memories = self.similar_memories(&created.memory.memory_id);
        }
        Ok(created)
    }

    /// The near-duplicates to warn about for a just-created memory (#1445), or `None` when the
    /// check could not run. Best effort, like the embed before it: a failure is logged and reported
    /// as "not checked", never as a failed write.
    fn similar_memories(
        &self,
        memory_id: &str,
    ) -> Option<Vec<rag_rat_query::memory::SimilarMemory>> {
        let conn = self.storage.connection();
        let lookup = || -> anyhow::Result<Option<Vec<rag_rat_query::memory::SimilarMemory>>> {
            let Some(similar) =
                crate::index::ai::similar_memories(conn, memory_id, SIMILAR_MEMORY_LIMIT)?
            else {
                return Ok(None);
            };
            let mut hydrated = Vec::new();
            for (id, similarity) in similar {
                if let Some(memory) = memory::memory_by_id(conn, &id)? {
                    hydrated.push(rag_rat_query::memory::SimilarMemory {
                        memory_id: memory.memory_id,
                        kind: memory.kind,
                        title: memory.title,
                        similarity,
                    });
                }
            }
            Ok(Some(hydrated))
        };
        lookup().unwrap_or_else(|err| {
            tracing::warn!(
                target: "rag_rat_core::index::ai::reconcile",
                error = %err,
                memory_id,
                "near-duplicate lookup failed"
            );
            None
        })
    }

    pub fn memory_update(&self, update: RepoMemoryUpdate) -> anyhow::Result<RepoMemory> {
        let updated = crate::memory_write::update_memory(self.storage.connection(), update)?;
        self.embed_written_memory(&updated.memory_id);
        Ok(updated)
    }

    /// Embed a memory this connection just wrote, so it ranks by meaning on the next search
    /// (#1443). Best effort after the write committed: a failure is logged and left to the
    /// reconcile backfill, never surfaced as a failed write.
    fn embed_written_memory(&self, memory_id: &str) {
        if let Err(err) =
            crate::index::ai::embed_written_memory(self.storage.connection(), memory_id)
        {
            tracing::warn!(
                target: "rag_rat_core::index::ai::reconcile",
                error = %err,
                memory_id,
                "memory embed after write failed; the next reconcile retries it"
            );
        }
    }

    pub fn memory_mark_obsolete(&self, memory_id: &str) -> anyhow::Result<RepoMemory> {
        crate::memory_write::mark_obsolete(self.storage.connection(), memory_id)
    }

    /// Add a typed graph edge from a source node to another node or a GitHub issue (#464).
    pub fn memory_edge_add(
        &self,
        source_node_id: &str,
        relation: &str,
        target: EdgeTarget,
    ) -> anyhow::Result<NodeEdge> {
        let relation = EdgeRelation::from_db_str(relation)?;
        crate::memory_write::add_edge(self.storage.connection(), source_node_id, relation, &target)
    }

    /// Remove a graph edge by its stable `edge_key` (#464). `false` when the key is unknown.
    pub fn memory_edge_remove(&self, edge_key: &str) -> anyhow::Result<bool> {
        crate::memory_write::remove_edge(self.storage.connection(), edge_key)
    }

    /// Every edge OUT of a node — its outgoing graph (deps / mind-map links / tracks) (#464).
    pub fn memory_edges_from(&self, source_node_id: &str) -> anyhow::Result<Vec<NodeEdge>> {
        memory::edges_from(self.storage.connection(), source_node_id)
    }

    /// Every edge INTO a target — the reverse traversal (e.g. tasks tracking a github issue)
    /// (#464).
    pub fn memory_edges_into(&self, target: EdgeTarget) -> anyhow::Result<Vec<NodeEdge>> {
        memory::edges_into(self.storage.connection(), &target)
    }

    pub fn memory_search(
        &self,
        query: &str,
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Vec<RepoMemory>> {
        let conn = self.storage.connection();
        // Embed the query once, outside the corruption retry, so a heal-and-retry does not pay
        // for it twice.
        let query_embedding = crate::index::ai::embed_query(conn, query)?;
        // #582: both the MATCH and the surface hydration (whose Summary path runs a RANKED
        // chunk_fts query) can hit FTS shadow corruption; heal-and-retry rather than surfacing
        // a bare "database disk image is malformed" forever.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = crate::search::lexical::memory_search(
                    conn,
                    query,
                    limit,
                    query_embedding.as_ref(),
                )?;
                memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_for_symbol(
        &self,
        symbol: &SymbolHit,
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Vec<RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = memory::memories_for_symbol(conn, symbol, limit)?;
                memory::apply_memory_surface(conn, &mut memories, surface)?;
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
        symbol: &SymbolHit,
        limit: usize,
    ) -> anyhow::Result<Vec<rag_rat_papertrail::DriveByRecord>> {
        self.drive_by_records_for_logical_id(symbol.logical_symbol_id, limit)
    }

    /// Distilled decision records for the symbol a chunk defines (#705 drive-by on `read_chunk`).
    /// Resolves the chunk's PRECISE defining symbol by the direct `chunks.symbol_id` link
    /// (#855/#860 — position matching is ambiguous across same-simple-name methods that nest or
    /// share a line), then the same facet-gated lane as [`records_for_symbol`]. Empty when the
    /// chunk defines no resolvable logical symbol. `pub(crate)`: only the `read_chunk` reader calls
    /// it (unlike the sibling `records_for_symbol`, which the MCP handler invokes directly).
    pub(crate) fn records_for_chunk_symbol(
        &self,
        chunk_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<rag_rat_papertrail::DriveByRecord>> {
        let conn = self.storage.connection();
        let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
        let logical_symbol_id = memory::logical_symbol_id_for_chunk(conn, chunk_id)?;
        Self::drive_by_records_scoped(conn, &repo_id, logical_symbol_id, limit)
    }

    /// Attach distilled decision records (#705 drive-by) to each search hit's symbol — the same
    /// facet-gated, capped lane as `read_chunk`, resolved precisely from each hit's `chunk_id`
    /// (#855). Skips a result set with no symbol-bearing hits so a doc/config-only result pays
    /// nothing.
    /// `pub`: the `semantic_search` MCP handler calls it directly (the shared
    /// `search_with_graph_meta` deliberately does NOT, so records stay off docs_for_symbol and
    /// other search consumers).
    pub fn attach_distilled_records_to_search_hits(
        &self,
        hits: &mut [rag_rat_query::SearchHit],
    ) -> anyhow::Result<()> {
        if hits.iter().all(|hit| hit.symbol_path.is_none()) {
            return Ok(());
        }
        let conn = self.storage.connection();
        // The distill store is optional (V077). When it is absent — a repo that never distilled —
        // skip the WHOLE batch: `records_for_symbol` would bail at the same guard, but only after
        // per-hit symbol resolution, so checking once here avoids that resolution work entirely on
        // the search hot path.
        if !rag_rat_db::schema::table_exists(conn, "papertrail_distill")? {
            return Ok(());
        }
        // Resolve the repo scope ONCE, and memoize the fetched records by the RESOLVED
        // logical-symbol id — so the many chunks of one symbol (including its continuation
        // parts, which each carry that symbol's `symbol_id` and so resolve to the same logical
        // id) fetch a single time.
        let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
        let mut by_logical: std::collections::HashMap<i64, Vec<rag_rat_papertrail::DriveByRecord>> =
            std::collections::HashMap::new();
        for hit in hits.iter_mut() {
            let Some(logical_symbol_id) = memory::logical_symbol_id_for_chunk(conn, hit.chunk_id)?
            else {
                continue;
            };
            if let Some(cached) = by_logical.get(&logical_symbol_id) {
                hit.distilled_records = cached.clone();
                continue;
            }
            let records =
                Self::drive_by_records_scoped(conn, &repo_id, Some(logical_symbol_id), 2)?;
            by_logical.insert(logical_symbol_id, records.clone());
            hit.distilled_records = records;
        }
        Ok(())
    }

    /// Shared drive-by fetch: the repo-scoped, facet-gated `records_for_symbol` lane over a
    /// resolved logical-symbol handle. `None`/unresolved id surfaces nothing (no anchor to bind
    /// a record to).
    fn drive_by_records_for_logical_id(
        &self,
        logical_symbol_id: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<rag_rat_papertrail::DriveByRecord>> {
        let conn = self.storage.connection();
        let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
        Self::drive_by_records_scoped(conn, &repo_id, logical_symbol_id, limit)
    }

    /// The facet-gated `records_for_symbol` fetch over a resolved logical id and an
    /// ALREADY-resolved repo scope — the batch-friendly core so a caller enriching many hits
    /// resolves `active_repo_id` once. `None`/unresolved id surfaces nothing (no anchor to bind
    /// a record to).
    fn drive_by_records_scoped(
        conn: &rusqlite::Connection,
        repo_id: &str,
        logical_symbol_id: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<rag_rat_papertrail::DriveByRecord>> {
        let Some(logical_symbol_id) = logical_symbol_id else {
            return Ok(Vec::new());
        };
        let records =
            rag_rat_papertrail::records_for_symbol(conn, repo_id, logical_symbol_id, limit)?;
        Ok(records.into_iter().map(rag_rat_papertrail::DriveByRecord::new).collect())
    }

    pub fn memory_for_path(
        &self,
        path: &str,
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<Vec<RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = memory::memories_for_path(conn, path, limit)?;
                memory::apply_memory_surface(conn, &mut memories, surface)?;
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
    ) -> anyhow::Result<Vec<RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories = memory::memories_for_edges(conn, edge_ids, limit)?;
                memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_evidence_for_symbol_and_edges(
        &self,
        symbol: &SymbolHit,
        caller_edge_ids: &[i64],
        callee_edge_ids: &[i64],
        limit: u32,
        surface: rag_rat_base::config::MemorySurface,
    ) -> anyhow::Result<RepoMemoryEvidence> {
        // This wrapper exposes only the evidence; the impact builder consumes the truncation flag
        // directly from the core fn. `find_callers` / `trace_callees` emit the evidence FULL (not
        // compact), so honor `[memory] surface` here by deferring each lane's bodies under
        // `Summary`.
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut evidence = memory::memory_evidence_for_symbol_and_edges(
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
    ) -> anyhow::Result<Vec<RepoMemory>> {
        let conn = self.storage.connection();
        // #582: the Summary surface hydration runs a RANKED chunk_fts query — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                let mut memories =
                    memory::memories_for_call_path_hash(conn, edge_sequence_hash, limit)?;
                memory::apply_memory_surface(conn, &mut memories, surface)?;
                Ok(memories)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn memory_rebind(
        &self,
        memory_id: &str,
        bind: RepoMemoryBindTarget,
    ) -> anyhow::Result<RepoMemory> {
        crate::memory_write::rebind_memory(self.storage.connection(), memory_id, bind)
    }

    pub fn memory_validate(&self) -> anyhow::Result<RepoMemoryValidationReport> {
        memory::validate_memories(self.storage.connection(), self.storage.source_root())
    }

    pub fn memory_doctor(&self) -> anyhow::Result<Vec<MemoryDoctorEntry>> {
        memory::doctor_report(self.storage.connection())
    }

    /// Read-only list of active+stale memories, optionally filtered by binding_kind.
    /// `kind` filters by binding kind (e.g. `Some("dir")`); `None` returns all.
    pub fn memory_list(&self, kind: Option<&str>) -> anyhow::Result<Vec<MemorySummary>> {
        memory::list_memories(self.storage.connection(), kind)
    }

    /// Fetch a single memory by id, returning `None` when not found.
    pub fn memory_get(&self, memory_id: &str) -> anyhow::Result<Option<RepoMemory>> {
        memory::memory_by_id(self.storage.connection(), memory_id)
    }
}
