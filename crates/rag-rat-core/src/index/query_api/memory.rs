//! Repo-memory query surface on `IndexDatabase`: create/update/obsolete, search, anchor resolution
//! (by symbol / path / call-path), rebind, and the validate/doctor anchor-health passes.

use anyhow::Context as _;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncCatchUpReport {
    pub target: rag_rat_oplog::DeviceFingerprint,
    pub required: u64,
    pub already_covered: u64,
    pub authored: u64,
}

/// Outcome of `sync publish --seed`: whether the publish ratchet flipped this run, and how many
/// memories were imported from the seed source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishSeedReport {
    pub published: bool,
    pub imported_memories: u64,
}

/// Decode a 64-hex account id (as `sync whoami` prints) into an [`rag_rat_oplog::AccountId`].
fn parse_account_id_hex(value: &str) -> anyhow::Result<rag_rat_oplog::AccountId> {
    anyhow::ensure!(
        value.len() == 64,
        "account id must be 64 hex characters (got {})",
        value.len()
    );
    let mut bytes = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = rag_rat_base::hash::hex_nibble(pair[0])
            .with_context(|| format!("account id has invalid hex at position {}", index * 2))?;
        let low = rag_rat_base::hash::hex_nibble(pair[1])
            .with_context(|| format!("account id has invalid hex at position {}", index * 2 + 1))?;
        bytes[index] = high << 4 | low;
    }
    Ok(rag_rat_oplog::AccountId::from_bytes(bytes))
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

    /// Mark this repo's account as a public knowledge base: persist the one-way `public`
    /// access-mode intent and ensure its `PublicRead` `/2` owner stream, so subsequent memory
    /// authoring is public and the account is servable to anonymous readers. Refuses if the
    /// account already holds a private stream (publishing an existing private repo is not
    /// supported) or authors sealed.
    pub fn sync_publish(&self) -> anyhow::Result<bool> {
        crate::memory_write::enable_public_authoring(
            self.storage.connection(),
            rag_rat_base::time::now_ms(),
        )
    }

    /// Publish this repo's account as a public knowledge base AND seed it from `source`: refuse a
    /// sealed source, flip the one-way publish ratchet, then import this repo's locally-authored
    /// memories out of `source` (a separate rag-rat index) and author them onto the PublicRead
    /// owner stream. The sealed-source refusal runs BEFORE publish, so a bad source never
    /// leaves a half-published node; a failure between the publish and the import is
    /// recoverable by re-running.
    pub fn sync_publish_seed(&self, source: &std::path::Path) -> anyhow::Result<PublishSeedReport> {
        let conn = self.storage.connection();
        let repo_id = rag_rat_query::memory::memory_repo_scope(conn)?
            .context("sync publish requires an active repo scope")?;
        crate::index::consolidate::ensure_source_unsealed(source, &repo_id)?;
        let published =
            crate::memory_write::enable_public_authoring(conn, rag_rat_base::time::now_ms())?;
        let imported_memories = crate::index::consolidate::seed_from_index(
            conn,
            source,
            &repo_id,
            rag_rat_base::time::now_ms(),
        )
        .context(
            "the public node is published, but seeding failed; re-run `sync publish --seed \
             <path>` to complete",
        )?;
        Ok(PublishSeedReport { published, imported_memories })
    }

    /// This store's local account id as lowercase hex — the identity another owner grants with
    /// `sync grant <id>`. Mints the account if absent so a fresh contributor can report its id.
    pub fn sync_whoami(&self) -> anyhow::Result<String> {
        let account =
            rag_rat_oplog::local_account(self.storage.connection(), rag_rat_base::time::now_ms())?;
        Ok(rag_rat_base::hash::hex_lower(&account.to_bytes()))
    }

    /// Grant `grantee_account_hex` (a 64-hex account id from its `sync whoami`) Writer authority on
    /// the active repo's owner stream (#1164), so that identity can author memories into this
    /// repo's shared set. Owner-only; requires a published repo. Returns the grant id as hex.
    pub fn sync_grant(&self, grantee_account_hex: &str) -> anyhow::Result<String> {
        let grantee = parse_account_id_hex(grantee_account_hex)?;
        let grant_id = crate::memory_write::grant_repo_writer(
            self.storage.connection(),
            grantee,
            rag_rat_base::time::now_ms(),
        )?;
        Ok(rag_rat_base::hash::hex_lower(&grant_id))
    }

    /// Configure the active repo to contribute memories to `owner_account_hex` (paste flow, #1164):
    /// subsequent memory authoring targets that owner's stream via this account's Writer grant. The
    /// owner must `sync grant` this account (its id from `sync whoami`) and this store must sync
    /// the owner's log before authoring succeeds.
    pub fn sync_contribute(&self, owner_account_hex: &str) -> anyhow::Result<()> {
        crate::memory_write::set_contribution_owner(
            self.storage.connection(),
            owner_account_hex,
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
        let logical_symbol_id = rag_rat_query::memory::logical_symbol_id_for_chunk(conn, chunk_id)?;
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
            let Some(logical_symbol_id) =
                rag_rat_query::memory::logical_symbol_id_for_chunk(conn, hit.chunk_id)?
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

    /// Materialize any accepted SYNCED `/3` content into the local memory tables — the reverse of
    /// the memory reconcile (#691 A1). Store-global (one pass per registered real repo); a repo
    /// with no minted account or no synced content is a cheap no-op. Called from the watcher's
    /// maintenance pass so a long-running process picks up content pulled AFTER open without a
    /// reopen (open and consolidate drain at their own seams). The eventual live sync-session
    /// driver should call `drain_synced_stream_for_repo` per session for immediacy; this pass
    /// is the backstop.
    pub fn drain_synced_memory(&self) -> anyhow::Result<()> {
        crate::memory_write::drain_synced_streams_for_all_repos(
            self.storage.connection(),
            rag_rat_base::time::now_ms(),
        )?;
        Ok(())
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
