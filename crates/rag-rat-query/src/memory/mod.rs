mod api;
pub mod edges;
pub mod evidence;
mod hydrate;
mod moniker;
mod resolve;
mod validate;
use std::collections::BTreeSet;

pub(crate) use api::memory_ids_with_broken_anchors;
pub use api::{
    MAX_EDGE_ANCHOR_LEN, MAX_MEMORY_BODY_LEN, MAX_MEMORY_PAYLOAD_LEN, MAX_MEMORY_TITLE_LEN,
    MemoryDoctorEntry, MemorySummary, anchor_health_counts, doctor_attention_count, doctor_report,
    list_memories, memories_for_call_path_hash, memories_for_chunk, memories_for_edges,
    memories_for_path, memories_for_symbol, memory_by_id, memory_evidence_for_symbol,
    memory_evidence_for_symbol_and_edges, memory_search, memory_search_scored, validate_memories,
};
pub use edges::{
    EDGE_SELECT, edge_by_key, edge_key, edge_row, edges_from, edges_into,
    periphery_edge_scope_clause, repo_is_registered, reresolve_on_read, resolve_node_target,
    source_node_owner_repo,
};
// The typed-edge public surface (#464): the boundary types cross the FFI/MCP/CLI edge, so they
// are `pub`; the query fns stay crate-internal.
pub use edges::{EdgeRelation, EdgeTarget, NodeEdge};
pub use hydrate::{
    CurrentDreamState, current_dream_state, current_summary_and_verdict, duplicate_memory_id,
    heal_repo_memory_fts, mark_drive_by_drift, normalize_tags, replace_tags, split_active_stale,
    tags_for_memory, upsert_memory_fts,
};
pub(crate) use hydrate::{
    attach_memory_children, binding_row, drive_by_memory, ids_to_memories, memory_row,
};
pub use moniker::{MonikerResolution, insert_auto_moniker_binding, resolve_moniker};
pub(crate) use moniker::{relocate_binding_by_moniker, validate_moniker_binding};
use rag_rat_base::hash::hex_sha256;
use rag_rat_base::time::now_ms;
pub(crate) use resolve::{
    RelocateMatch, binding_leaf_name, call_path_edge_by_id, chunk_by_id, chunk_for_logical_symbol,
    chunk_for_symbol, chunk_ids_for_symbol, compute_edge_sequence_hash, dir_has_files,
    edge_by_fingerprint, edge_by_id, edge_id_matches_fingerprint_in_linked_worktree,
    logical_symbol_id_for_symbol, relocate_chunk_by_hash, relocate_symbol_by_name, symbol_signal,
};
pub use resolve::{
    insert_binding, logical_symbol_id_for_chunk, remap_call_path_callee_logical_symbol_ids,
    resolve_binding, stamp_bindings_from_parent_repo,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
pub use validate::{
    AppliedTarget, AppliedTargets, decode_applied_targets, encode_applied_targets,
    is_polymorphic_node_kind, memory_id, memory_input_hash, validate_confidence, validate_edge_len,
    validate_kind, validate_len, validate_payload, validate_source, validate_status,
};
pub(crate) use validate::{effective_fs_root, fts_query, validate_binding};

/// The active `repo_id` scope for the memory tables, or `None` on the pre-A5 schema (the memory
/// tables are still repo-global until the periphery-scoping migration lands). Every memory
/// read/write gates its `repo_id` predicate on this: `Some(repo_id)` scopes to the active repo,
/// `None` runs the original unscoped SQL. See `schema::periphery_repo_scope` for the deferral.
pub fn memory_repo_scope(conn: &Connection) -> anyhow::Result<Option<String>> {
    Ok(rag_rat_db::schema::periphery_repo_scope(conn, "repo_memories")?)
}

/// The ` AND repo_memories.repo_id = '…'` predicate for a memory read, or `""` when unscoped.
pub(crate) fn memory_repo_scope_clause(scope: &Option<String>) -> String {
    rag_rat_db::schema::periphery_repo_scope_clause(scope, "repo_memories")
}

/// The predicate selecting the memories recall still surfaces, on the `repo_memories` row named
/// `alias` (the table name itself where the statement does not alias it): a `stale` memory is live
/// (its anchor drifted, not its memory), only `obsolete`/`rejected` are dead. Every memory read
/// that attaches, lists or searches filters through this, as do the typed-edge reads and the dream
/// queues, so reclassifying a status is one edit here.
pub fn live_memory_status_sql(alias: &str) -> String {
    let live = <MemoryStatus as strum::VariantArray>::VARIANTS
        .iter()
        .filter(|status| status.is_live())
        .map(|status| format!("'{}'", status.as_db_str()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{alias}.status IN ({live})")
}

/// Escape a string for use as a SQLite `LIKE` pattern under `ESCAPE '\'`: the three special
/// characters `\`, `%`, `_` are backslash-escaped so a bound path containing one matches literally.
pub(crate) fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Serialize `synced_anchor_drifted` only when it is set, so the common case adds no field and a
/// drifted memory carries a visible one. A reader that never demotes still sees the divergence.
fn is_not_drifted(drifted: &bool) -> bool {
    !*drifted
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoMemory {
    pub memory_id: String,
    pub kind: String,
    pub title: String,
    // Under `[memory] surface = "summary"` this carries the elision marker instead of the prose
    // (see `apply_memory_surface`); a note inside the summary envelope keeps its full body, since
    // nothing will ever summarize it. Skipped when EMPTY, which stored bodies never are, so `full`
    // surface and `memory_show` always serialize it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// The dream-compacted summary of the CURRENT body, populated ONLY by the summary-first
    /// renderers under `[memory] surface = "summary"` when a `memory_note_summaries` row exists
    /// for the current content_hash. `None` under `full` (and for every non-surfacing tool),
    /// and for a note short enough that compaction skips it — that note surfaces whole in
    /// `body` instead.
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
    /// A synced memory whose author-stamped `source_text_hash` no longer matches the text this
    /// checkout holds at any of its anchors (#1236). Set only on the drive-by surfaces, where it
    /// demotes the memory into the stale lane — it never hides one. Exact-text hashing cannot tell
    /// "the peer is ahead of my checkout" from "I edited after pulling", so a divergence is a
    /// reason to mark, never to withhold. Always `false` for a locally authored memory, and for a
    /// memory carrying no stamp (every pre-carrier row is NULL, and an absent stamp is not
    /// evidence of drift).
    #[serde(skip_serializing_if = "is_not_drifted")]
    pub synced_anchor_drifted: bool,
    pub bindings: Vec<RepoMemoryBinding>,
    pub call_paths: Vec<RepoMemoryCallPath>,
    pub tags: Vec<String>,
}

/// One anchor of a memory, as this store sees it.
///
/// Two things live in a binding row (#1297). The AUTHORED anchor — `binding_id`, and the path,
/// span, kind, signature and moniker version the author bound — replicates on `anchors/1` and in
/// the `/3` anchor set, and changes only when the author rebinds. This store's RESOLUTION of it
/// is local: where validation last found the target here and what it landed on. The struct is the
/// store's view: `binding_id` is the authored identity; `resolved_binding_id` the name the target
/// carries here when it differs; `path`, the span, `symbol_kind`, `signature_hash` and
/// `moniker_tool_version` are the resolution where the store has one (`resolved` set — then they
/// are its view, NULL included), else the authored value. Readers that need the authored value
/// itself — publication, the drain's identity match — read the columns.
#[derive(Debug, Clone, Serialize)]
pub struct RepoMemoryBinding {
    pub memory_id: String,
    pub binding_kind: String,
    pub binding_id: String,
    /// The qualified name, fingerprint or hash the target carries on this store when relocation
    /// moved it off the authored one; `None` while it is the authored one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_binding_id: Option<String>,
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
    /// The stored [`AnchorStatus`] token. Like `binding_kind`, it stays a string: this struct is
    /// the row as read and as serialized.
    pub anchor_status: String,
    pub created_at_ms: i64,
}

/// The SET fragment a writer that moves ONE component of a binding's resolution (its name, a
/// call-path hash) pairs with its own assignment: it marks the row resolved on this store and
/// carries every other shadow forward — what the row had already resolved, else the authored
/// value. Once `resolved` is set the seven shadows are this store's view, NULL included, so a
/// partial writer must never leave one unset. The caller's own column must not appear here.
pub const BINDING_RESOLUTION_CARRY_SQL: &str = "resolved = 1,
     resolved_path = IIF(resolved, resolved_path, path),
     resolved_start_line = IIF(resolved, resolved_start_line, start_line),
     resolved_end_line = IIF(resolved, resolved_end_line, end_line),
     resolved_symbol_kind = IIF(resolved, resolved_symbol_kind, symbol_kind),
     resolved_signature_hash = IIF(resolved, resolved_signature_hash, signature_hash),
     resolved_moniker_tool_version = IIF(resolved, resolved_moniker_tool_version, \
                                                moniker_tool_version)";

/// The read side of [`BINDING_RESOLUTION_CARRY_SQL`]: a shadowed column's value on the
/// `repo_memory_bindings` row named `alias` — this store's resolution when `resolved` is set (then
/// the shadow is its view, NULL included), else the authored value. The shadowed columns are the
/// authored identity `binding_id`, which the writer moving it assigns itself, and the six the carry
/// names; read them only through this in SQL, or `binding_row` in Rust. A read that names the
/// authored column directly returns the AUTHORED location of a relocated binding.
pub(crate) fn binding_current(alias: &str, column: &str) -> String {
    format!("IIF({alias}.resolved, {alias}.resolved_{column}, {alias}.{column})")
}

/// [`binding_current`] of `path` on the unaliased `repo_memory_bindings` table.
pub(crate) const BINDING_CURRENT_PATH: &str = "IIF(repo_memory_bindings.resolved, \
                                               repo_memory_bindings.resolved_path, \
                                               repo_memory_bindings.path)";

/// [`binding_current`] of `binding_id` on the unaliased `repo_memory_bindings` table.
pub(crate) const BINDING_CURRENT_BINDING_ID: &str = "IIF(repo_memory_bindings.resolved, \
                                                     repo_memory_bindings.resolved_binding_id, \
                                                     repo_memory_bindings.binding_id)";

impl RepoMemoryBinding {
    /// The name, fingerprint or hash the target carries on this store: the resolution where
    /// relocation moved it, the authored identity otherwise. What lookups against the index and
    /// the local call-path tables key on; never what identifies the row or crosses the wire.
    pub fn current_binding_id(&self) -> &str {
        self.resolved_binding_id.as_deref().unwrap_or(&self.binding_id)
    }

    /// Record where relocation landed. Landing back on the authored identity clears the
    /// resolution rather than restating it, so "resolved" always means "moved".
    pub(crate) fn set_resolved_binding_id(&mut self, id: String) {
        self.resolved_binding_id = (id != self.binding_id).then_some(id);
    }
}

/// The closed set of `repo_memory_bindings.binding_kind` tokens this build resolves and validates.
///
/// [`RepoMemoryBinding::binding_kind`] stays a string because a binding row replicates with its
/// memory: a peer on a newer build can deliver a kind this one does not know, and that row must
/// still load. It validates as `unverified` (see `validate_binding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum BindingKind {
    LogicalSymbol,
    Symbol,
    Chunk,
    Edge,
    CallPath,
    ScipMoniker,
    Path,
    Dir,
    Commit,
    Tracker,
}

impl BindingKind {
    /// The exact persisted token.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown binding kind `{value}`"))
    }
}

/// The closed set of `repo_memory_bindings.anchor_status` tokens: what the last validation of a
/// binding concluded in this checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum AnchorStatus {
    Current,
    Relocated,
    Stale,
    Gone,
    /// Absent in this checkout but alive in another indexed scope (#492).
    Pending,
    Unverified,
}

impl AnchorStatus {
    /// The exact persisted token.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown anchor status `{value}`"))
    }
}

/// The closed set of `repo_memories.kind` tokens: the variant names verbatim (PascalCase).
///
/// [`RepoMemory::kind`] — like its `status`, `confidence` and `source`, and a binding's
/// `relocation_reason` — stays a string for the reason [`BindingKind`] gives: a memory replicates,
/// so a peer on a newer build can deliver a token this one does not know, and that row must still
/// load. These enums name what this build writes and compares; the `validate_*` gates reject any
/// other token on the write path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
pub enum MemoryKind {
    Invariant,
    Decision,
    RejectedAlternative,
    Risk,
    BugPattern,
    TestExpectation,
    PerformanceNote,
    SecurityNote,
    FFIBoundary,
    PlatformQuirk,
    FollowUp,
    OpenQuestion,
    Obsolete,
    // Polymorphic graph-node kinds (#465): legitimately unanchored (a Concept / standalone Task
    // lives as a graph node with no code binding — see resolve_binding / #463).
    Task,
    Concept,
}

impl MemoryKind {
    /// The exact persisted token.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("invalid memory kind `{value}`"))
    }

    /// The polymorphic graph-node kinds — `Task` and `Concept` (#463/#465). They ALONE may be
    /// created UNANCHORED (no code binding) AND may carry a structured `payload_json`; every other
    /// kind is a plain note (anchors to code, no payload). The SINGLE source of truth for the
    /// unanchored-create gate (`create`/`update_memory`), the payload-kind gate
    /// (`validate_payload`), and the dream verifier's `memory_unverifiable` exemption — they must
    /// never drift, or a create the gate allows becomes self-inflicted dream noise, or an
    /// off-contract payload/anchor slips through.
    pub fn is_polymorphic_node(self) -> bool {
        matches!(self, Self::Task | Self::Concept)
    }
}

/// The closed set of `repo_memories.status` tokens (lowercase). A string on [`RepoMemory`] for the
/// reason [`MemoryKind`] gives.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Stale,
    Obsolete,
    Rejected,
}

impl MemoryStatus {
    /// The exact persisted token.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("invalid memory status `{value}`"))
    }

    /// Whether recall still surfaces a memory in this status — the set
    /// [`live_memory_status_sql`] filters on. A `stale` memory is live (its anchor drifted, not its
    /// memory), only `obsolete`/`rejected` are dead.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Active | Self::Stale)
    }
}

/// The closed set of `repo_memories.confidence` tokens (lowercase). A string on [`RepoMemory`] for
/// the reason [`MemoryKind`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum MemoryConfidence {
    High,
    Medium,
    Low,
}

impl MemoryConfidence {
    /// The exact persisted token.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("invalid memory confidence `{value}`"))
    }
}

/// The closed set of `repo_memories.source` tokens (lowercase). A string on [`RepoMemory`] for the
/// reason [`MemoryKind`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum MemorySource {
    Agent,
    Human,
    Imported,
    Generated,
}

impl MemorySource {
    /// The exact persisted token.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("invalid memory source `{value}`"))
    }
}

/// The closed set of `repo_memory_bindings.relocation_reason` tokens (kebab-case) this build
/// stamps. [`RepoMemoryBinding::relocation_reason`] stays a string for the reason [`MemoryKind`]
/// gives; `NULL` means the anchor never relocated or relocated via the default
/// qualified-name/content paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum RelocationReason {
    /// Why a moniker relocation succeeded — persisted on `repo_memory_bindings.relocation_reason`
    /// so `doctor`/MCP output can distinguish a semantic-identity relocate from the default
    /// qualified-name/content paths.
    MonikerMatch,
    /// Why a `scip_moniker` binding's own anchor string was rewritten: its live logical symbol got
    /// a NEW moniker from the latest run (rust-analyzer monikers embed the Cargo package
    /// version, so a routine version bump changes every string without changing any symbol
    /// identity). The rebind is keyed off our own content-derived logical id, not fuzzy
    /// matching.
    MonikerRefresh,
    /// The `relocation_reason` the synced-memory drain stamps on a symbol binding it moved IN PLACE
    /// to a target another store published, when the published kind or signature differs from
    /// the row's. Its cached ids may still name the target it left: a rebind between two impls
    /// of one type keeps the binding's identity and kind, so only the signature says it moved.
    ///
    /// The validator weighs a recorded kind or signature against a live handle ONLY on a row so
    /// marked. Any other row can disagree with its handle for reasons that name no other target
    /// — a sibling device's `anchors/1` update carrying its own checkout's view, or values
    /// recorded before the target's kind or signature changed here — and following them there
    /// would hand the memory to any same-named sibling that has the old kind or signature.
    ///
    /// The mark stands until a validation lands on a target agreeing with the recorded kind and
    /// signature. The row is shared by every checkout of the repo, and the one validating first may
    /// not hold the author's target: on the raw-id arm, whose candidates are the validating
    /// checkout's own, a linked worktree that edited the target leaves the mark for the
    /// checkout that has it (the logical arm's candidates are repo-wide, so any checkout can
    /// answer there). That works because relocation does not refresh a marked row's recorded
    /// kind or signature until the mark is answered. An identity match answers it outright: the
    /// content-hash fallback clears it and restates the kind and signature, and a moniker
    /// relocation replaces the reason with its own. (An `anchors/1` row update also moves a
    /// binding in place, but marks nothing.)
    Retargeted,
}

impl RelocationReason {
    /// The exact persisted token.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown relocation reason `{value}`"))
    }
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

#[derive(Debug, Clone, Default, Serialize)]
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
    /// `memory_note_summaries` / `memory_reality` siblings (repo-scoped, keyed on the current
    /// content_hash). A memory compaction SKIPS for already fitting the summary envelope
    /// ([`evidence::note_is_shown_whole`]) has no summary row and never will, so its body stands in
    /// as its own summary — it is inside the envelope by construction, so the header costs no more
    /// than a generated summary would, and without it `impact_surface` would render that memory
    /// title-only permanently. A LONGER body with no summary row still falls back to the mechanical
    /// title-only header; `memory show` remains the expand path.
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
                    compact.summary = summary.or_else(|| shown_whole_body(&memory.body));
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
    pub fn apply_surface(
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
/// counterpart to [`RepoMemoryEvidence::compact_summary_first`]). Under `Summary` a memory's body
/// is DEFERRED behind [`body_elision_marker`] — the reader gets a signal that prose was withheld
/// and the one call that expands it — while the summary + verdict marker are hydrated from the
/// derived `memory_note_summaries` / `memory_reality` siblings (repo-scoped, keyed on the current
/// content_hash, prompt-version-gated). The ONE exception is an UNSUMMARIZED body compaction skips
/// for already fitting the summary envelope ([`evidence::note_is_shown_whole`]): nothing will ever
/// summarize it, so deferring would leave a bare title, and showing it whole costs no more than the
/// summary it stands in for. An over-envelope memory with no summary row — dream disabled (the
/// default), never run, or waiting behind a [`evidence::COMPACT_PROMPT_VERSION`] bump — still
/// defers; dumping full bodies is the outcome this surface exists to prevent. Unlike the compact
/// projection this KEEPS the full binding/call-path structure a direct `memory_for_symbol` /
/// `memory_for_path` query relies on — only the prose is compacted. Under `Full` the list is
/// untouched (byte-identical output).
pub fn apply_memory_surface(
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
        if summary.is_some() || !evidence::note_is_shown_whole(&memory.body) {
            memory.body = body_elision_marker(&memory.memory_id);
        }
        memory.summary = summary;
        memory.verdict = verdict;
    }
    Ok(())
}

/// The one-line stand-in for a body the summary surface withheld. The stored body is NEVER deleted
/// (`memory_note_summaries` is a separate derived table), so the marker names the expand path — the
/// reader would otherwise have no signal that prose is missing, or that a `summary` beside it is a
/// lossy rewrite. `rag-rat memory get <id>` is the CLI equivalent; naming one path keeps the marker
/// cheap, since it is paid per memory on every attachment under the default surface.
fn body_elision_marker(memory_id: &str) -> String {
    format!("{BODY_ELISION_PREFIX} — full text: memory_show {memory_id}]")
}

/// The opening of every [`body_elision_marker`].
const BODY_ELISION_PREFIX: &str = "[body elided";

/// Whether a surfaced body is the elision marker rather than prose. A renderer with ONE prose slot
/// (the grep/read hook digest, which shows a memory as a single line) has to tell the two apart:
/// the marker is a pointer, and rendering it as the memory's gist spends that line's whole prose
/// budget saying nothing.
///
/// Matching on the marker's opening text alone would misread a memory that documents the marker —
/// prose is free to quote it, and under `Full` no marker is ever applied. So the test is equality
/// with the marker [`apply_memory_surface`] would have written for THIS memory. The marker is
/// written in exactly one place and carries the memory's own id, so prose describing the marker no
/// longer reads as one: an author writing about it cannot be writing the id it would carry here.
pub fn body_is_elided(memory: &RepoMemory) -> bool {
    memory.body == body_elision_marker(&memory.memory_id)
}

/// The body of a note compaction skips as already inside the summary envelope, to stand in for the
/// `memory_note_summaries` row it will never have — the body-less compact projection has nowhere
/// else to put prose. `None` for a longer body (that one defers) or an empty one (nothing to show).
fn shown_whole_body(body: &str) -> Option<String> {
    (evidence::note_is_shown_whole(body) && !body.trim().is_empty()).then(|| body.to_string())
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
/// (`bindings.first()`) is anchored — without the remaining bindings or call paths, and with prose
/// bounded by the summary envelope (a compacted summary, or the note's own body when it already
/// fits — see [`Self::summary`]). Full detail stays available via `memory_for_symbol` /
/// `memory_for_path` / `memory_for_call_path`, or `impact_surface` full mode (`include` unaffected;
/// `full_memories: true`).
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
    /// surface = "summary"` when a `memory_note_summaries` row exists for the current content_hash
    /// (dream v2 pass 2) — or, for a note compaction skips as already inside the summary envelope,
    /// that note's verbatim body. `None` under the `full` surface, and for a long body no summary
    /// has been generated for yet; the title then stands alone (the title-only fallback). The full
    /// body is always one lookup away via `memory show` / `memory_show`.
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
            .find(|binding| binding.binding_kind != BindingKind::ScipMoniker.as_db_str())
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
pub struct ResolvedBinding {
    pub binding_kind: BindingKind,
    pub binding_id: String,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub edge_id: Option<i64>,
    pub commit_hash: Option<String>,
    pub tracker: Option<String>,
    pub project: Option<String>,
    pub item_key: Option<String>,
    pub symbol_kind: Option<String>,
    pub signature_hash: Option<String>,
    pub call_path: Option<ResolvedCallPath>,
    pub source_text_hash: Option<String>,
    pub anchor_status: AnchorStatus,
}

impl ResolvedBinding {
    /// A binding of `binding_kind` with every location and identity field unset — the base the
    /// resolvers fill in with struct-update syntax, so each names only the fields it knows.
    pub(crate) fn new(
        binding_kind: BindingKind,
        binding_id: String,
        anchor_status: AnchorStatus,
    ) -> Self {
        Self {
            binding_kind,
            binding_id,
            path: None,
            start_line: None,
            end_line: None,
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            commit_hash: None,
            tracker: None,
            project: None,
            item_key: None,
            symbol_kind: None,
            signature_hash: None,
            call_path: None,
            source_text_hash: None,
            anchor_status,
        }
    }
}

#[derive(Debug)]
pub struct ResolvedCallPath {
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
    pub(crate) callee_logical_symbol_id: Option<i64>,
}

/// Row-independent fields that may re-find an edge after its source lines move. Callee identity is
/// part of this loose match so relocation never changes which symbol the edge reaches.
pub(crate) struct EdgeLooseIdentity {
    pub(crate) from_name: Option<String>,
    pub(crate) to_name: String,
    pub(crate) edge_kind: String,
    pub(crate) target_qualified_name: Option<String>,
    pub(crate) callee_logical_symbol_id: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct ChunkAnchor {
    chunk_id: i64,
    path: String,
    start_line: i64,
    end_line: i64,
    text_hash: String,
    symbol_id: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct EdgeAnchor {
    edge_id: i64,
    fingerprint: String,
    /// The pre-#567 8-field compatibility fingerprint. Present for every live edge because a
    /// legacy binding cannot encode whether receiver inference would produce a hint today.
    legacy_fingerprint: Option<String>,
    /// The lookup matched the compatibility fingerprint rather than the current versioned
    /// identity. Validation must demote this to relocated: legacy identity cannot prove the
    /// receiver owner stayed unchanged.
    matched_legacy_fingerprint: bool,
    path: String,
    start_line: i64,
    end_line: i64,
    source_hash: String,
    callee_logical_symbol_id: Option<i64>,
}

#[derive(Clone, Copy)]
pub(crate) struct EdgeFingerprintParts<'a> {
    path: &'a str,
    start_line: i64,
    end_line: i64,
    from_name: Option<&'a str>,
    to_name: Option<&'a str>,
    edge_kind: &'a str,
    target_qualified_name: Option<&'a str>,
    receiver_hint: Option<&'a str>,
    /// Rust-only conservative receiver type inference (`recv.run()` → `Alpha`/`Beta`, #567):
    /// deliberately excluded from the loose call-site identity. The resolved callee below is the
    /// authority for whether that loose match may relocate: changing Alpha::run to Beta::run must
    /// demote the binding even when this hint remains `Worker`.
    receiver_type_hint: Option<&'a str>,
    /// Stable content-derived identity of the resolved callee. `None` is also identity: an
    /// unresolved edge must fingerprint differently from the same call once it resolves.
    callee_logical_symbol_id: Option<i64>,
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

/// READ-only counts of active repo-memory bindings grouped by `anchor_status`.
/// Computed by a single GROUP BY query; does not run `memory_validate` or write anything.
#[derive(Debug, Default, Serialize)]
pub struct AnchorHealth {
    pub current: u64,
    pub relocated: u64,
    pub stale: u64,
    pub gone: u64,
}
