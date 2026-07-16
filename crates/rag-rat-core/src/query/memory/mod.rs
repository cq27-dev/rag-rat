mod api;
// The op-log authoring seam: row→op translation, the one-time full backfill, and the live
// `author_*` helpers the memory mutations call in-transaction (#532).
mod authoring;
mod edges;
mod hydrate;
mod moniker;
mod resolve;
mod validate;
use std::collections::BTreeSet;

pub use api::memory_evidence_for_symbol;
pub(crate) use api::*;
// The scope-READING reconcile entry (#541): reconciles the active repo's owner stream, reading
// the repo id from the connection scope and no-oping under an absent/unstable scope.
// Re-exported so the index reconcile path (the idle-repo ghost backstop, #583) can name it
// across the private module.
pub(crate) use authoring::backfill_memory_oplog;
// The scope-explicit reconcile entry (#541): `authoring` is a PRIVATE module, so
// `index::consolidate` names this through this re-export (Task 5 of #541).
pub(crate) use authoring::reconcile_owner_stream_for_repo;
// The typed-edge public surface (#464): the boundary types cross the FFI/MCP/CLI edge, so they
// are `pub`; the query fns stay crate-internal.
pub use edges::{EdgeRelation, EdgeTarget, NodeEdge};
pub(crate) use edges::{add_edge, edge_key, edges_from, edges_into, remove_edge};
pub(crate) use hydrate::*;
pub(crate) use moniker::*;
pub(crate) use resolve::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub(crate) use validate::*;

/// The active `repo_id` scope for the memory tables, or `None` on the pre-A5 schema (the memory
/// tables are still repo-global until the periphery-scoping migration lands). Every memory
/// read/write gates its `repo_id` predicate on this: `Some(repo_id)` scopes to the active repo,
/// `None` runs the original unscoped SQL. See `schema::periphery_repo_scope` for the deferral.
pub(crate) fn memory_repo_scope(conn: &Connection) -> anyhow::Result<Option<String>> {
    Ok(crate::index::schema::periphery_repo_scope(conn, "repo_memories")?)
}

/// The ` AND repo_memories.repo_id = '…'` predicate for a memory read, or `""` when unscoped.
pub(crate) fn memory_repo_scope_clause(scope: &Option<String>) -> String {
    crate::index::schema::periphery_repo_scope_clause(scope, "repo_memories")
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemory {
    pub memory_id: String,
    pub kind: String,
    pub title: String,
    // Skipped when EMPTY so the `[memory] surface = "summary"` view can defer the full prose to
    // `memory show` (the summary-first renderers empty this and populate `summary` instead).
    // Stored bodies are never empty, so `full` surface and `memory_show` always serialize it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// The dream-compacted summary of the CURRENT body, populated ONLY by the summary-first
    /// renderers under `[memory] surface = "summary"` when a `memory_summaries` row exists for the
    /// current content_hash. `None` under `full` (and for every non-surfacing tool). Title-only
    /// fallback when absent — the full body is one `memory show` away.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Plain-text verdict marker from the memory's `memory_reality` row (e.g. `[verdict:
    /// diverged]`), populated alongside `summary` under `surface = "summary"`. `None`
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    pub confidence: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub source: String,
    /// Opaque, `schema_version`-tagged JSON payload for polymorphic nodes (#465) — the
    /// kind-specific structured data a `Task` / `Concept` carries (e.g. a task's
    /// priority/estimate). `None` for a plain note. Stored verbatim; the core does not type
    /// it. (content_hash folds it in phase B.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_json: Option<String>,
    // Internal anchoring/dedup mechanics — never actionable for a reader, so kept off the wire.
    #[serde(skip_serializing)]
    pub source_text_hash: Option<String>,
    #[serde(skip_serializing)]
    pub input_hash: Option<String>,
    #[serde(skip_serializing)]
    pub memory_version: String,
    pub bindings: Vec<RepoMemoryBinding>,
    pub call_paths: Vec<RepoMemoryCallPath>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryBinding {
    pub memory_id: String,
    pub binding_kind: String,
    pub binding_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<i64>,
    // Opaque `sym_<hex>` symbol handle (stable, JSON-safe — #130/#149).
    #[serde(
        rename = "id",
        skip_serializing_if = "Option::is_none",
        serialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::serialize"
    )]
    pub logical_symbol_id: Option<i64>,
    // Internal rowid — never serialized (reindex-churned, #149); the handle is logical_symbol_id.
    #[serde(skip_serializing)]
    pub symbol_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_hash: Option<String>,
    /// SCIP moniker provenance, set on `scip_moniker`-kind bindings: the oracle tool + version
    /// whose data supplied `binding_id` (the moniker) at bind time. A relocation match against a
    /// different current `tool_version` is lower confidence (#70).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moniker_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moniker_tool_version: Option<String>,
    /// How the last validation relocated this binding (e.g. `moniker-match`), `None` when the
    /// anchor never relocated or relocated via the default qualified-name/content paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relocation_reason: Option<String>,
    pub anchor_status: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryCallPath {
    pub memory_id: String,
    // Opaque `sym_<hex>` symbol handles (stable, JSON-safe — #130/#149).
    #[serde(
        rename = "start_id",
        skip_serializing_if = "Option::is_none",
        serialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::serialize"
    )]
    pub start_logical_symbol_id: Option<i64>,
    #[serde(
        rename = "end_id",
        skip_serializing_if = "Option::is_none",
        serialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::serialize"
    )]
    pub end_logical_symbol_id: Option<i64>,
    pub edge_sequence_hash: String,
    pub path_summary: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryCreateResult {
    pub memory: RepoMemory,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoMemoryCreate {
    pub kind: String,
    pub title: String,
    pub body: String,
    pub confidence: String,
    pub created_by: Option<String>,
    pub source: Option<String>,
    /// Opaque JSON payload for a polymorphic node (#465) — a `Task`/`Concept`'s kind-specific
    /// data. Validated as a JSON object with no node/edge references (payload-closure); stored
    /// verbatim.
    #[serde(default)]
    pub payload_json: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub bind: RepoMemoryBindTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct RepoMemoryBindTarget {
    // Accept the opaque `sym_<hex>` handle (#149); `default` keeps it optional under the custom
    // deserializer.
    #[serde(
        rename = "id",
        default,
        deserialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::deserialize"
    )]
    pub logical_symbol_id: Option<i64>,
    // Internal rowid — NOT accepted from the wire (reindex-churned, #149); bind by handle/path.
    // CLI sets it programmatically. `skip_deserializing` keeps it off the input schema.
    #[serde(skip_deserializing)]
    pub symbol_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub edge_id: Option<i64>,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub commit_hash: Option<String>,
    pub tracker: Option<String>,
    pub project: Option<String>,
    pub item_key: Option<String>,
    #[serde(
        rename = "start_id",
        default,
        deserialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::deserialize"
    )]
    pub start_logical_symbol_id: Option<i64>,
    #[serde(
        rename = "end_id",
        default,
        deserialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::deserialize"
    )]
    pub end_logical_symbol_id: Option<i64>,
    pub edge_sequence_hash: Option<String>,
    pub path_summary: Option<String>,
    /// Ordered edge ids for a server-derived call-path binding (#38). When set, the server
    /// computes the authoritative `edge_sequence_hash` from these edges' fingerprints and stores
    /// them for validation — preferred over a client-supplied `edge_sequence_hash`.
    pub edge_path: Option<Vec<i64>>,
    /// Directory anchor: a repo-root-relative directory path, or `""` for the repo root.
    /// Normalized on resolve (trim, drop leading `./`, strip trailing `/`).
    pub dir: Option<String>,
}

impl RepoMemoryBindTarget {
    /// True iff NO field is set — a truly empty target, i.e. an UNANCHORED node (#463). Derived
    /// structurally (`== default`) so it is drift-proof: any field added to this struct is covered
    /// automatically. A PARTIALLY populated target (some field set, but not a complete binding —
    /// e.g. a tracker+project without an item_key, or a span without a path) is NOT empty; it is a
    /// malformed anchor that `resolve_binding` rejects rather than silently treating as unanchored.
    pub(crate) fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoMemoryUpdate {
    pub memory_id: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub confidence: Option<String>,
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
    /// Set the node's payload (#465). `None` leaves the stored payload unchanged (like the other
    /// fields); a `Some` value replaces it. Clearing a payload to null is not supported yet.
    pub payload_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryValidationReport {
    pub checked: u64,
    pub current: u64,
    pub relocated: u64,
    pub stale: u64,
    pub gone: u64,
    /// Absent at THIS context's HEAD but alive in another indexed scope (a linked-worktree
    /// overlay — in-flight branch work, #492). Not broken: it re-anchors when the branch lands,
    /// and it must never draw `gone`-style remediation (rebind / mark-obsolete).
    pub pending: u64,
    pub unverified: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryEvidence {
    pub direct: Vec<RepoMemory>,
    pub path_crossed: Vec<RepoMemory>,
    /// Memories bound to a server-derived call path whose computed hash this traversal crossed —
    /// i.e. a `caller -> symbol -> callee` (or single-edge) path through the focus symbol (#38).
    #[serde(default)]
    pub call_path_crossed: Vec<RepoMemory>,
    pub stale: Vec<RepoMemory>,
}

impl RepoMemoryEvidence {
    /// Project to the scannable compact view (#37): drops bodies, extra bindings, and call paths,
    /// keeping the per-lane header an agent skims during preflight. Field names match the full
    /// evidence so the wire shape is identical apart from per-memory detail.
    pub fn compact(&self) -> CompactRepoMemoryEvidence {
        let project =
            |memories: &[RepoMemory]| memories.iter().map(CompactRepoMemory::from).collect();
        CompactRepoMemoryEvidence {
            direct: project(&self.direct),
            path_crossed: project(&self.path_crossed),
            call_path_crossed: project(&self.call_path_crossed),
            stale: project(&self.stale),
        }
    }

    /// [`Self::compact`] plus the dream summary + verdict marker for each memory's CURRENT body
    /// (the `[memory] surface = "summary"` view). Each header is hydrated from the derived
    /// `memory_summaries` / `memory_reality` siblings (repo-scoped, keyed on the current
    /// content_hash), so a memory with no summary row falls back to the mechanical header
    /// (title-only in practice). The full body is never included — `memory show` remains the
    /// expand path.
    pub fn compact_summary_first(
        &self,
        conn: &Connection,
    ) -> rusqlite::Result<CompactRepoMemoryEvidence> {
        let project = |memories: &[RepoMemory]| -> rusqlite::Result<Vec<CompactRepoMemory>> {
            memories
                .iter()
                .map(|memory| {
                    let mut compact = CompactRepoMemory::from(memory);
                    let (summary, verdict) = hydrate::current_summary_and_verdict(
                        conn,
                        &memory.memory_id,
                        &memory.title,
                        &memory.body,
                    )?;
                    compact.summary = summary;
                    compact.verdict = verdict;
                    Ok(compact)
                })
                .collect()
        };
        Ok(CompactRepoMemoryEvidence {
            direct: project(&self.direct)?,
            path_crossed: project(&self.path_crossed)?,
            call_path_crossed: project(&self.call_path_crossed)?,
            stale: project(&self.stale)?,
        })
    }

    /// Apply `[memory] surface` to every lane's FULL memories IN PLACE (Option A: defer bodies,
    /// keep structure) — the graph-traversal (`find_callers` / `trace_callees`) counterpart to
    /// [`Self::compact_summary_first`], used where the evidence is emitted full rather than
    /// compact. Under `Full` every lane is untouched.
    pub(crate) fn apply_surface(
        &mut self,
        conn: &Connection,
        surface: rag_rat_base::config::MemorySurface,
    ) -> rusqlite::Result<()> {
        for lane in
            [&mut self.direct, &mut self.path_crossed, &mut self.call_path_crossed, &mut self.stale]
        {
            apply_memory_surface(conn, lane, surface)?;
        }
        Ok(())
    }
}

/// Apply the `[memory] surface` view to a hydrated FULL memory list IN PLACE (the direct-query
/// counterpart to [`RepoMemoryEvidence::compact_summary_first`]). Under `Summary` each memory's
/// full body is DEFERRED to `memory show`: the current-body summary + verdict marker are hydrated
/// from the derived `memory_summaries` / `memory_reality` siblings (repo-scoped, keyed on the
/// current content_hash, prompt-version-gated), the body is emptied, and a memory with no summary
/// falls back to title-only. Unlike the compact projection this KEEPS the full binding/call-path
/// structure a direct `memory_for_symbol` / `memory_for_path` query relies on — only the prose is
/// compacted. Under `Full` the list is untouched (byte-identical output).
pub(crate) fn apply_memory_surface(
    conn: &Connection,
    memories: &mut [RepoMemory],
    surface: rag_rat_base::config::MemorySurface,
) -> rusqlite::Result<()> {
    if !matches!(surface, rag_rat_base::config::MemorySurface::Summary) {
        return Ok(());
    }
    for memory in memories.iter_mut() {
        let (summary, verdict) = hydrate::current_summary_and_verdict(
            conn,
            &memory.memory_id,
            &memory.title,
            &memory.body,
        )?;
        memory.summary = summary;
        memory.verdict = verdict;
        memory.body = String::new();
    }
    Ok(())
}

/// Compact (default) view of `RepoMemoryEvidence` for `impact_surface` (#37) — same lane layout,
/// each memory summarized to its high-signal header by [`CompactRepoMemory`].
#[derive(Debug, Clone, Serialize)]
pub struct CompactRepoMemoryEvidence {
    pub direct: Vec<CompactRepoMemory>,
    pub path_crossed: Vec<CompactRepoMemory>,
    #[serde(default)]
    pub call_path_crossed: Vec<CompactRepoMemory>,
    pub stale: Vec<CompactRepoMemory>,
}

/// A one-line-scannable projection of a [`RepoMemory`] for `impact_surface`'s default output (#37):
/// what the memory says (kind/title/confidence/status) and where its *primary* binding
/// (`bindings.first()`) is anchored — without the full body, the remaining bindings, or call paths.
/// Full detail stays available via `memory_for_symbol` / `memory_for_path` /
/// `memory_for_call_path`, or `impact_surface` full mode (`include` unaffected; `full_memories:
/// true`).
///
/// Future direction: this header could carry a short LLM-generated `summary` of the full body,
/// produced by an out-of-process local model (Ollama) rather than truncating the title — see the
/// local-AI memory-maintenance spike (#122), which already commits to keeping Ollama out of the
/// binary. Until that lands, the projection stays purely mechanical (no model dependency here).
#[derive(Debug, Clone, Serialize)]
pub struct CompactRepoMemory {
    pub memory_id: String,
    pub kind: String,
    pub title: String,
    pub confidence: String,
    pub status: String,
    /// Anchor status of the primary binding (`current` / `stale` / …); `None` when unbound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `[start_line, end_line]` of the primary binding when both are known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<[i64; 2]>,
    // Opaque `sym_<hex>` handle of the primary binding when it's a symbol binding — the actionable
    // key for a follow-up `memory_for_symbol` / `impact_surface` full lookup (#149).
    #[serde(
        rename = "id",
        skip_serializing_if = "Option::is_none",
        serialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::serialize"
    )]
    pub logical_symbol_id: Option<i64>,
    /// The dream-compacted summary of the memory's CURRENT body, populated ONLY under `[memory]
    /// surface = "summary"` when a `memory_summaries` row exists for the current content_hash
    /// (dream v2 pass 2). `None` under the `full` surface, or when no summary has been generated —
    /// the title then stands alone (the title-only fallback). The full body is always one lookup
    /// away via `memory show` / `memory_show`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// A plain-text verdict marker from the memory's `memory_reality` row (dream v2 pass 1), e.g.
    /// `[verdict: diverged]` / `[verdict: current @<short-commit>]`. Populated alongside `summary`
    /// under `surface = "summary"`; `None` under `full` or when the memory has no stored verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl From<&RepoMemory> for CompactRepoMemory {
    fn from(memory: &RepoMemory) -> Self {
        // Pick the first NON-moniker binding for the header. The auxiliary `scip_moniker` binding
        // is an identity anchor that lags between (opt-in) oracle runs and is NOT the
        // memory's real content anchor — `split_active_stale` excludes it from staleness
        // for exactly this reason. But `attach_memory_children` orders bindings by
        // `binding_kind`, and `"scip_moniker"` sorts before `"symbol"`, so a naive
        // `bindings.first()` could surface a lagging `unverified`/`gone` moniker and make
        // an ACTIVE memory read as stale in the compact view (Codex on #194). Fall back to
        // the first binding only if every binding is a moniker.
        let primary = memory
            .bindings
            .iter()
            .find(|binding| binding.binding_kind != SCIP_MONIKER_BINDING_KIND)
            .or_else(|| memory.bindings.first());
        Self {
            memory_id: memory.memory_id.clone(),
            kind: memory.kind.clone(),
            title: memory.title.clone(),
            confidence: memory.confidence.clone(),
            status: memory.status.clone(),
            anchor_status: primary.map(|binding| binding.anchor_status.clone()),
            binding_kind: primary.map(|binding| binding.binding_kind.clone()),
            path: primary.and_then(|binding| binding.path.clone()),
            span: primary.and_then(|binding| match (binding.start_line, binding.end_line) {
                (Some(start), Some(end)) => Some([start, end]),
                _ => None,
            }),
            logical_symbol_id: primary.and_then(|binding| binding.logical_symbol_id),
            // The mechanical projection carries no summary/verdict; the summary surface hydrates
            // them from the sibling tables (see `RepoMemoryEvidence::compact_summary_first`).
            summary: None,
            verdict: None,
            tags: memory.tags.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedBinding {
    binding_kind: String,
    binding_id: String,
    path: Option<String>,
    start_line: Option<i64>,
    end_line: Option<i64>,
    logical_symbol_id: Option<i64>,
    symbol_id: Option<i64>,
    chunk_id: Option<i64>,
    edge_id: Option<i64>,
    commit_hash: Option<String>,
    tracker: Option<String>,
    project: Option<String>,
    item_key: Option<String>,
    symbol_kind: Option<String>,
    signature_hash: Option<String>,
    call_path: Option<ResolvedCallPath>,
    source_text_hash: Option<String>,
    anchor_status: String,
}

#[derive(Debug)]
struct ResolvedCallPath {
    start_logical_symbol_id: Option<i64>,
    end_logical_symbol_id: Option<i64>,
    edge_sequence_hash: String,
    path_summary: String,
    /// Ordered edges behind a server-derived hash (#38). Empty for a legacy client-supplied
    /// `edge_sequence_hash` (which stays `unverified` — no edges to re-check).
    edges: Vec<CallPathEdge>,
}

/// One edge in a server-derived call path: its exact `edge_fingerprint` plus the looser
/// identity (names/kind/target) that lets validation re-find it after a line move (#38).
#[derive(Debug, Clone)]
pub(crate) struct CallPathEdge {
    pub(crate) fingerprint: String,
    pub(crate) from_name: Option<String>,
    pub(crate) to_name: Option<String>,
    pub(crate) edge_kind: String,
    pub(crate) target_qualified_name: Option<String>,
    pub(crate) receiver_hint: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ChunkAnchor {
    chunk_id: i64,
    path: String,
    start_line: i64,
    end_line: i64,
    symbol_path: Option<String>,
    text_hash: String,
    symbol_id: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct EdgeAnchor {
    edge_id: i64,
    fingerprint: String,
    path: String,
    start_line: i64,
    end_line: i64,
    source_hash: String,
}

pub(crate) struct EdgeFingerprintParts<'a> {
    path: &'a str,
    start_line: i64,
    end_line: i64,
    from_name: Option<&'a str>,
    to_name: Option<&'a str>,
    edge_kind: &'a str,
    target_qualified_name: Option<&'a str>,
    receiver_hint: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(kind: &str, anchor_status: &str, path: Option<&str>) -> RepoMemoryBinding {
        RepoMemoryBinding {
            memory_id: "mem_x".to_string(),
            binding_kind: kind.to_string(),
            binding_id: format!("{kind}-id"),
            path: path.map(str::to_string),
            start_line: path.map(|_| 10),
            end_line: path.map(|_| 20),
            logical_symbol_id: Some(42),
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            commit_hash: None,
            tracker: None,
            project: None,
            item_key: None,
            symbol_kind: None,
            signature_hash: None,
            moniker_tool: None,
            moniker_tool_version: None,
            relocation_reason: None,
            anchor_status: anchor_status.to_string(),
            created_at_ms: 0,
        }
    }

    fn memory(bindings: Vec<RepoMemoryBinding>) -> RepoMemory {
        RepoMemory {
            memory_id: "mem_x".to_string(),
            kind: "Invariant".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            summary: None,
            verdict: None,
            confidence: "high".to_string(),
            status: "active".to_string(),
            created_by: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            source: "agent".to_string(),
            payload_json: None,
            source_text_hash: None,
            input_hash: None,
            memory_version: String::new(),
            bindings,
            call_paths: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn compact_header_skips_a_lagging_moniker_binding_for_the_real_anchor() {
        // `attach_memory_children` orders bindings by `binding_kind`, so a `scip_moniker` companion
        // (which can be `unverified`/`gone` between oracle runs, and which `split_active_stale`
        // deliberately ignores) sorts BEFORE the real `symbol` anchor. The compact header must skip
        // it, or an ACTIVE memory reads as stale (Codex on #194).
        let compact = CompactRepoMemory::from(&memory(vec![
            binding(SCIP_MONIKER_BINDING_KIND, "unverified", None),
            binding("symbol", "current", Some("src/lib.rs")),
        ]));
        assert_eq!(compact.binding_kind.as_deref(), Some("symbol"));
        assert_eq!(compact.anchor_status.as_deref(), Some("current"));
        assert_eq!(compact.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(compact.span, Some([10, 20]));
    }

    #[test]
    fn compact_header_falls_back_to_a_moniker_only_binding_set() {
        // A memory anchored ONLY by a moniker still gets a header (no non-moniker binding to
        // prefer).
        let compact = CompactRepoMemory::from(&memory(vec![binding(
            SCIP_MONIKER_BINDING_KIND,
            "current",
            None,
        )]));
        assert_eq!(compact.binding_kind.as_deref(), Some(SCIP_MONIKER_BINDING_KIND));
    }

    // ── dream-summary surfacing (`[memory] surface = "summary"`) ─────────────────

    /// A fresh in-memory index scoped to repo `r` — the fixture for the summary-surfacing tests.
    fn summary_conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&c).unwrap();
        c.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        c.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id','r')",
            [],
        )
        .unwrap();
        c
    }

    /// A minimal `RepoMemory` with a controlled id + body (no bindings).
    fn memory_with_body(id: &str, body: &str) -> RepoMemory {
        RepoMemory {
            memory_id: id.to_string(),
            kind: "Invariant".to_string(),
            title: "t".to_string(),
            body: body.to_string(),
            summary: None,
            verdict: None,
            confidence: "high".to_string(),
            status: "active".to_string(),
            created_by: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            source: "agent".to_string(),
            payload_json: None,
            source_text_hash: None,
            input_hash: None,
            memory_version: String::new(),
            bindings: Vec::new(),
            call_paths: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn seed_summary(c: &Connection, id: &str, body: &str, summary: &str) {
        // Stamp the current content_hash (title `"t"`, matching `memory_with_body`) + the current
        // COMPACT_PROMPT_VERSION — the hydrator gates the summary read on both (like the compaction
        // queue's coverage check), so a mismatch drops the summary.
        c.execute(
            "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, \
             prompt_version, generated_at_ms) VALUES (?1,'r',?2,?3,?4,0)",
            params![
                id,
                crate::dream::note_content_hash("t", body),
                summary,
                crate::dream::COMPACT_PROMPT_VERSION
            ],
        )
        .unwrap();
    }

    fn seed_reality(c: &Connection, id: &str, body: &str, verdict: &str, commit: Option<&str>) {
        // Key the reality row on the memory's TRUE content_hash (title `"t"`, matching
        // `memory_with_body`), its current evidence hash, AND the current verdict PROMPT_VERSION —
        // the hydrator gates the marker on all three (like the queue/divergence finder), so a
        // mismatch on any silently drops it. These test memories have no bindings/identifiers, so
        // the evidence hash is the stable empty value `checked_inputs_hash` computes.
        let inputs = crate::dream::checked_inputs_hash(c, id, &Some("r".to_string())).unwrap();
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
             checked_against_commit, checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
             (?1,'r',?2,?3,?4,?5,?6,0)",
            params![
                id,
                crate::dream::note_content_hash("t", body),
                verdict,
                commit,
                inputs,
                crate::dream::VERDICT_PROMPT_VERSION
            ],
        )
        .unwrap();
    }

    fn evidence(memories: Vec<RepoMemory>) -> RepoMemoryEvidence {
        RepoMemoryEvidence {
            direct: memories,
            path_crossed: Vec::new(),
            call_path_crossed: Vec::new(),
            stale: Vec::new(),
        }
    }

    #[test]
    fn summary_surface_renders_summary_and_verdict_marker() {
        let c = summary_conn();
        let body = "the full body worth compacting";
        seed_summary(
            &c,
            "m1",
            body,
            "A compacted three-sentence summary. It preserves polarity. Done.",
        );
        seed_reality(&c, "m1", body, "diverged", None);

        let compact =
            evidence(vec![memory_with_body("m1", body)]).compact_summary_first(&c).unwrap();
        let header = &compact.direct[0];
        assert_eq!(
            header.summary.as_deref(),
            Some("A compacted three-sentence summary. It preserves polarity. Done."),
            "the compacted summary is hydrated under the summary surface"
        );
        assert_eq!(
            header.verdict.as_deref(),
            Some("[verdict: diverged]"),
            "the verdict marker renders"
        );
    }

    #[test]
    fn summary_surface_falls_back_to_title_only_without_a_summary_row() {
        let c = summary_conn();
        // No memory_summaries / memory_reality rows → summary + verdict stay None (title-only).
        let compact =
            evidence(vec![memory_with_body("m1", "body")]).compact_summary_first(&c).unwrap();
        let header = &compact.direct[0];
        assert_eq!(header.summary, None, "no summary row → the title stands alone");
        assert_eq!(header.verdict, None, "no reality row → no verdict marker");
        assert_eq!(header.title, "t", "the title is still present");
    }

    #[test]
    fn summary_surface_misses_a_stale_summary_after_a_body_edit() {
        let c = summary_conn();
        // A summary exists, but for the OLD body — the current content_hash differs, so the LEFT
        // JOIN misses and the header falls back to title-only (the summary
        // self-invalidated).
        seed_summary(
            &c,
            "m1",
            "old body",
            "A stale summary from before. It no longer applies. Ignore.",
        );
        let compact =
            evidence(vec![memory_with_body("m1", "new body")]).compact_summary_first(&c).unwrap();
        assert_eq!(
            compact.direct[0].summary, None,
            "a summary keyed on a stale content_hash is not surfaced"
        );
    }

    #[test]
    fn verdict_marker_misses_a_stale_verdict_after_a_body_edit() {
        let c = summary_conn();
        // A verdict exists, but for the OLD body — the current content_hash differs, so the verdict
        // read misses and the header carries no marker. Symmetric to the stale-summary case: a body
        // edit self-invalidates the verdict just like the summary, so a just-edited memory never
        // renders the PRIOR body's verdict.
        seed_reality(&c, "m1", "old body", "diverged", None);
        let compact =
            evidence(vec![memory_with_body("m1", "new body")]).compact_summary_first(&c).unwrap();
        assert_eq!(
            compact.direct[0].verdict, None,
            "a verdict keyed on a stale content_hash is not surfaced after a body edit"
        );
    }

    #[test]
    fn verdict_marker_misses_a_stale_verdict_after_a_bound_input_change() {
        // Regression (PR #428 Codex P2): the marker is gated on `checked_inputs_hash`, not only
        // `content_hash`. A stored verdict whose inputs hash no longer matches the memory's current
        // bound-file inputs (a bound file changed since the check) must drop, like the divergence
        // finder and queue treat an inputs mismatch. Seed a row with a deliberately-mismatched
        // inputs hash — the current inputs hash for this binding-less memory is the
        // empty-set value, which this arbitrary value is not.
        let c = summary_conn();
        let body = "a note whose stored verdict predates a bound-file change";
        // Stamp the CURRENT content hash + prompt version so the only mismatch is the inputs hash —
        // otherwise the marker would drop for the wrong reason and not exercise the inputs gate.
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
             checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
             ('m1','r',?1,'diverged','stale-inputs',?2,0)",
            params![
                crate::dream::note_content_hash("t", body),
                crate::dream::VERDICT_PROMPT_VERSION
            ],
        )
        .unwrap();
        let compact =
            evidence(vec![memory_with_body("m1", body)]).compact_summary_first(&c).unwrap();
        assert_eq!(
            compact.direct[0].verdict, None,
            "a verdict whose checked_inputs_hash no longer matches is not surfaced"
        );
    }

    #[test]
    fn summary_and_marker_drop_under_an_obsolete_prompt_version() {
        // Regression (PR #428 Codex P2): the surfacing hydrator must apply the SAME prompt-version
        // gate the compaction queue / verification queue use. A summary from an obsolete compact
        // prompt or a verdict from an obsolete verdict prompt must not keep showing while the
        // memory waits behind the budget (or a model failure) for a fresh one.
        let c = summary_conn();
        let body = "b";
        let content_hash = crate::dream::note_content_hash("t", body);
        c.execute(
            "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, \
             prompt_version, generated_at_ms) VALUES ('m1','r',?1,'A three sentence summary. It \
             holds. Done.','compact-OLD',0)",
            params![content_hash],
        )
        .unwrap();
        let inputs = crate::dream::checked_inputs_hash(&c, "m1", &Some("r".to_string())).unwrap();
        c.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, verdict, \
             checked_inputs_hash, prompt_version, checked_at_ms) VALUES \
             ('m1','r',?1,'diverged',?2,'verify-OLD',0)",
            params![content_hash, inputs],
        )
        .unwrap();
        let compact =
            evidence(vec![memory_with_body("m1", body)]).compact_summary_first(&c).unwrap();
        assert_eq!(
            compact.direct[0].summary, None,
            "a summary from an obsolete compact prompt is not surfaced"
        );
        assert_eq!(
            compact.direct[0].verdict, None,
            "a verdict from an obsolete verdict prompt is not surfaced"
        );
    }

    #[test]
    fn verdict_marker_current_carries_the_short_commit() {
        let c = summary_conn();
        let body = "b";
        seed_summary(&c, "m1", body, "One sentence summary here. Two now. Three done.");
        seed_reality(&c, "m1", body, "current", Some("abcdef0123456789"));
        let compact =
            evidence(vec![memory_with_body("m1", body)]).compact_summary_first(&c).unwrap();
        assert_eq!(
            compact.direct[0].verdict.as_deref(),
            Some("[verdict: current @abcdef0]"),
            "a current verdict carries the 7-hex short commit"
        );
    }

    #[test]
    fn full_surface_projection_carries_no_summary_or_verdict() {
        // The `full` compact projection is purely mechanical — no summary/verdict, even
        // when sibling rows exist (they are only read by `compact_summary_first`).
        let compact = CompactRepoMemory::from(&memory_with_body("m1", "body"));
        assert_eq!(compact.summary, None);
        assert_eq!(compact.verdict, None);
    }

    #[test]
    fn memory_get_returns_the_full_body_even_when_a_summary_exists() {
        // `memory show` / `memory_show` is surface-independent: the expand path always carries the
        // full body regardless of any compacted summary.
        let c = summary_conn();
        let body = "the full body that memory show must always return";
        c.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id) VALUES \
             ('m1','Invariant','t',?1,'high','active','agent',1,1,'agent','v1','r')",
            [body],
        )
        .unwrap();
        seed_summary(&c, "m1", body, "A short summary stands in for surfacing. Not for show. Ok.");
        let fetched = memory_by_id(&c, "m1").unwrap().expect("memory present");
        assert_eq!(
            fetched.body, body,
            "memory_get returns the full body regardless of the summary"
        );
    }

    #[test]
    fn apply_memory_surface_summary_defers_the_body_and_hydrates_summary_and_verdict() {
        // The direct-query / read_chunk / grep counterpart to `compact_summary_first`: under
        // `Summary` the full body is emptied (deferred to `memory show`) and the current-body
        // summary + verdict marker are hydrated in its place.
        let c = summary_conn();
        let body = "the full body worth compacting";
        seed_summary(&c, "m1", body, "A compacted summary in place of the body.");
        seed_reality(&c, "m1", body, "diverged", None);
        let mut memories = vec![memory_with_body("m1", body)];
        apply_memory_surface(&c, &mut memories, rag_rat_base::config::MemorySurface::Summary)
            .unwrap();
        assert_eq!(memories[0].body, "", "the full body is deferred under summary");
        assert_eq!(
            memories[0].summary.as_deref(),
            Some("A compacted summary in place of the body.")
        );
        assert!(
            memories[0].verdict.as_deref().unwrap_or_default().contains("diverged"),
            "the verdict marker is set: {:?}",
            memories[0].verdict
        );
    }

    #[test]
    fn apply_memory_surface_summary_falls_back_to_title_only_without_a_summary_row() {
        // No `memory_summaries` / `memory_reality` rows → summary + verdict stay None, but the body
        // is still deferred: the reader sees the title (+ structure) and expands via `memory show`.
        let c = summary_conn();
        let mut memories = vec![memory_with_body("m1", "some body")];
        apply_memory_surface(&c, &mut memories, rag_rat_base::config::MemorySurface::Summary)
            .unwrap();
        assert_eq!(memories[0].body, "", "body deferred even without a summary (title-only)");
        assert_eq!(memories[0].summary, None);
        assert_eq!(memories[0].verdict, None);
    }

    #[test]
    fn apply_memory_surface_full_is_a_noop() {
        // `Full` keeps the body byte-identical and never hydrates a summary, even when one exists.
        let c = summary_conn();
        let body = "kept verbatim in full mode";
        seed_summary(&c, "m1", body, "would-be summary");
        let mut memories = vec![memory_with_body("m1", body)];
        apply_memory_surface(&c, &mut memories, rag_rat_base::config::MemorySurface::Full).unwrap();
        assert_eq!(memories[0].body, body, "full surface keeps the body");
        assert_eq!(memories[0].summary, None, "full surface never hydrates a summary");
        assert_eq!(memories[0].verdict, None);
    }
}
