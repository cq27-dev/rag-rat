//! The declarative registry of syncable tables.
//!
//! A [`TableSpec`] declares, for one physical table, which columns replicate (the `pk` identity
//! plus the synced `columns`) and which are re-derived locally and must NEVER travel
//! (`local_columns`). The engine is generic over `&[TableSpec]`, so the mechanism is exercised
//! against synthetic specs in tests and the production registry starts with durable memory anchors.
//!
//! [`assert_spec_covers_schema`] is the load-bearing invariant: every physical column must be
//! classified exactly once — as pk, synced, or local. A newly-added physical column can never be
//! silently unclassified (neither replicated nor deliberately local), which would be an invisible
//! correctness gap.

use std::collections::BTreeSet;

use rusqlite::Connection;

use super::schema_facts::{self, CheckVerdict, PhysicalColumn};
use super::scope_stream::ScopeId;

/// The storage/wire type of a synced column. A cell whose runtime value disagrees with its column's
/// declared type is quarantined by the applier rather than silently coerced.
///
/// `Bool` stores as a STRICT `INTEGER` and must hold only 0 or 1. SQLite does not enforce that
/// domain without a `CHECK (col IN (0, 1))`, which no pragma exposes for the lint to require — so a
/// `Bool` column SHOULD carry that CHECK, and the runtime backstop is that `read_typed` refuses any
/// other integer rather than coercing it. `Text` has the same shape: STRICT pins the storage class,
/// not that the bytes are valid UTF-8. Both are refused as a VALUE, not an error: the reader runs
/// under the refold at store open, where an error would fail every subsequent open, so a row with
/// such a cell is carried as unreadable — never published, never deleted — until it is repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueType {
    Text,
    I64,
    Bool,
    Blob,
}

/// A column's declared default — the value an op authored BEFORE this column existed contributes
/// when it is projected here (#1002). Literal forms only: a non-literal SQL default
/// (`CURRENT_TIMESTAMP`, `unixepoch()`) is per-device non-deterministic, so two receivers filling
/// the same op would produce different rows. That is the determinism requirement, not a style rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DefaultValue {
    Null,
    Bool(bool),
    I64(i64),
    Text(&'static str),
    Blob(&'static [u8]),
}

/// When a column entered the spec, and what an op authored before that contributes for it. The
/// version is what makes the default SAFE to apply: without it, an op merely older than the CURRENT
/// spec would have every added column defaulted, including ones that already existed in the op's
/// own version — so a broken producer that dropped a column it was obliged to send would have that
/// column silently reset to its default on every receiver instead of parking as the partial it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AddedColumn {
    /// The spec version that introduced the column. An op stamped BELOW this predates the column
    /// and legitimately omits it; an op stamped at or above it was obliged to send it.
    pub in_version: u32,
    pub default: DefaultValue,
}

/// One synced, non-pk column: its name, wire type, and — for a column added after the table's first
/// spec version — when it arrived plus the value an older producer's op contributes for it. Merge
/// is whole-row (all synced columns move together under the row's write clock), so a column carries
/// no per-column merge policy.
///
/// `added` is `None` for a column that has existed since the table's first version: no op can ever
/// legitimately omit it, so there is nothing to fill, and demanding a default would force a
/// meaningless one onto an original `NOT NULL` column. It also keeps failure contained — an op
/// missing such a column is a genuinely broken partial, and parks rather than being silently
/// filled.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ColumnSpec {
    pub name: &'static str,
    pub value_type: ValueType,
    pub added: Option<AddedColumn>,
}

impl ColumnSpec {
    /// A column every op must carry — the shape for pk columns and for any column present since the
    /// table's first spec version.
    pub const fn required(name: &'static str, value_type: ValueType) -> Self {
        Self { name, value_type, added: None }
    }

    /// A column introduced in `in_version`, carrying the value an op older than that contributes.
    pub const fn added(
        name: &'static str,
        value_type: ValueType,
        in_version: u32,
        default: DefaultValue,
    ) -> Self {
        Self { name, value_type, added: Some(AddedColumn { in_version, default }) }
    }
}

/// One syncable table. `pk` names the identity columns (encoded as the row op's `pk`); `columns`
/// are the non-pk synced columns (encoded as the op's cells; the whole row is folded as a unit
/// under its write clock); `local_columns` are re-derived from the local index and never
/// replicated. `scope_id` names the `/5` stream this table rides — the routing key that binds it to
/// an auth tier, a retention class and a flood budget.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TableSpec {
    pub name: &'static str,
    pub scope_id: ScopeId,
    /// Which synced column set this binary authors against — stamped into every op it produces, so
    /// a receiver can tell an OLDER producer's complete row from a NEWER producer's partial
    /// one (#1002). BUMP whenever `columns` changes; `local_columns` never cross the wire, so
    /// they do not count.
    ///
    /// EVOLUTION IS ADDITIVE ONLY. A column may be ADDED (with a bump and a declared default);
    /// removing, renaming, or retyping one means a NEW TABLE. Default-fill closes older→newer for
    /// additions alone — an older op naming a column the current spec dropped parks forever, and
    /// no future binary redeems it. This cannot be linted (it needs registry history), so it
    /// is an invariant on whoever edits a spec.
    ///
    /// The version is largely ADVISORY, and the asymmetry matters. A receiver CAN reject an
    /// OVER-stamp (a version above its own → park) and a *partial* under-stamp (a cell for a
    /// column introduced after the claimed version is self-contradictory → park, since
    /// `in_version` is a fixed historical fact under additive-only evolution). What it CANNOT
    /// detect is a WHOLE-CLOTH under-stamp: an op carrying only the columns its claimed
    /// version had, stamped lower than the producer actually authored against. That case is
    /// indistinguishable from an honest un-upgraded peer, and it is the DESTRUCTIVE direction
    /// — every column added since the claimed version is reset to its default on every
    /// receiver, at a winning lamport, silently.
    ///
    /// That is not privilege escalation (an authorized writer can already write any value into
    /// those columns under whole-row LWW, and the reset is the deliberate meaning of a whole-row
    /// write from a device that does not know the column), but it does mean a buggy producer
    /// degrades data fleet-wide rather than failing loudly. STAMP CORRECTLY; the rest of the rule
    /// assumes it. A mis-stamp PARKS rather than quarantining because a forgotten bump is the
    /// likeliest cause and parking is what lets the next binary redeem it.
    pub spec_version: u32,
    /// The identity columns, with types — the applier validates each incoming pk value against its
    /// declared type so SQLite affinity can't coerce a mismatched pk (e.g. `I64(1)` onto a `TEXT`
    /// key `'1'`) and split a row's bookkeeping.
    pub pk: &'static [ColumnSpec],
    pub columns: &'static [ColumnSpec],
    pub local_columns: &'static [&'static str],
    /// The column that scopes rows to a project, if the table is repo-scoped. It MUST be a
    /// primary-key column (the exhaustiveness lint enforces this): the producer emits only rows
    /// whose value here matches the repo being synced (so foreign-repo rows are never signed into
    /// the wrong repo's stream), and the applier rejects an incoming op naming a different repo.
    /// `None` for a table with no repo dimension.
    pub repo_column: Option<&'static str>,
}

impl TableSpec {
    /// The position of `repo_column` within `pk`, if the repo scope is a primary-key column — the
    /// index the applier checks against the repo being synced.
    pub fn repo_pk_index(&self) -> Option<usize> {
        let repo_column = self.repo_column?;
        self.pk.iter().position(|c| c.name == repo_column)
    }
}

const MEMORY_BINDING_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("memory_id", ValueType::Text),
    ColumnSpec::required("binding_kind", ValueType::Text),
    ColumnSpec::required("binding_id", ValueType::Text),
];

const MEMORY_BINDING_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("path", ValueType::Text),
    ColumnSpec::required("start_line", ValueType::I64),
    ColumnSpec::required("end_line", ValueType::I64),
    ColumnSpec::required("commit_hash", ValueType::Text),
    ColumnSpec::required("tracker", ValueType::Text),
    ColumnSpec::required("project", ValueType::Text),
    ColumnSpec::required("item_key", ValueType::Text),
    ColumnSpec::required("created_at_ms", ValueType::I64),
    ColumnSpec::required("symbol_kind", ValueType::Text),
    ColumnSpec::required("signature_hash", ValueType::Text),
    ColumnSpec::required("moniker_tool", ValueType::Text),
    ColumnSpec::required("moniker_tool_version", ValueType::Text),
];

const MEMORY_BINDING_LOCAL_COLUMNS: &[&str] = &[
    "logical_symbol_id",
    "symbol_id",
    "chunk_id",
    "edge_id",
    "anchor_status",
    "relocation_reason",
    "downgrade_pending_at_ms",
    // This store's resolution of the authored anchor (#1297): where validation found the target
    // last, and the discriminators it landed on. `resolved` set means the seven shadows are this
    // store's view, NULL included. The authored columns they shadow never change except by
    // authoring; relocation writes these, so a checkout that differs from its siblings publishes
    // nothing.
    "resolved",
    "resolved_binding_id",
    "resolved_path",
    "resolved_start_line",
    "resolved_end_line",
    "resolved_symbol_kind",
    "resolved_signature_hash",
    "resolved_moniker_tool_version",
];

/// The local columns a winning remote upsert that CHANGES a `repo_memory_bindings` row's authored
/// columns resets to NULL: a new authored statement invalidates whatever this store had resolved
/// for the old one — the location and discriminators relocation recorded describe the anchor as
/// it was, and would otherwise outlive the author's change as this store's view and evidence.
/// What the reset does NOT decide is a same-key rebind between twins (struct → impl under one
/// qualified name): an `anchors/1` rebind is unmarked, so validation keeps trusting the retained
/// handle; only the drain's `refresh_binding` marks a row `retargeted` and makes the author's kind
/// and signature outrank it. The drain resets the same columns; an upsert restating the row this
/// store already holds resets nothing. The handles
/// (`logical_symbol_id`, `symbol_id`, `chunk_id`, `edge_id`) stay: they are what validation
/// re-derives the target from, and a chunk handle in particular has no other way back (a missing
/// one validates `unverified` without the hash fallback). A handle a restatement made stale is
/// what the next validate pass exists to find out.
const MEMORY_BINDING_RESET_ON_UPSERT: &[&str] = &[
    "resolved",
    "resolved_binding_id",
    "resolved_path",
    "resolved_start_line",
    "resolved_end_line",
    "resolved_symbol_kind",
    "resolved_signature_hash",
    "resolved_moniker_tool_version",
];

/// The local columns the applier nulls when a winning upsert changes a held row's synced
/// columns (see [`MEMORY_BINDING_RESET_ON_UPSERT`]); empty for every other table. Every name
/// must be one of the table's `local_columns` and nullable, which the registry tests pin.
pub(crate) fn reset_on_upsert(spec: &TableSpec) -> &'static [&'static str] {
    match spec.name {
        "repo_memory_bindings" => MEMORY_BINDING_RESET_ON_UPSERT,
        _ => &[],
    }
}

/// A row predicate under which [`reset_on_upsert`] does NOT apply. A call-path binding's
/// resolution is not evidence but the KEY of its local `repo_memory_call_paths`/`_edges` rows
/// (they follow the hash this store derived); clearing it on an authored restatement would leave
/// those rows unreachable and the anchor `gone`. Validation re-derives the path against the live
/// graph every pass regardless.
pub(crate) fn reset_on_upsert_keeps(spec: &TableSpec) -> Option<&'static str> {
    match spec.name {
        "repo_memory_bindings" => Some("binding_kind = 'call_path'"),
        _ => None,
    }
}

const MEMORY_BINDINGS: TableSpec = TableSpec {
    name: "repo_memory_bindings",
    scope_id: ScopeId::ANCHORS,
    spec_version: 1,
    pk: MEMORY_BINDING_PK,
    columns: MEMORY_BINDING_COLUMNS,
    local_columns: MEMORY_BINDING_LOCAL_COLUMNS,
    repo_column: Some("repo_id"),
};

// A memory verdict is regenerable model output; every non-pk column is a portable fact about the
// verdict or the check that produced it (`checked_against_commit`/`checked_inputs_hash` are the
// churn-skip comparators — replicating them lets a receiver skip re-verification, which is the
// point of the scope). Nothing here is checkout-local, so `local_columns` is empty; a NULL value
// (an uncitable row's verdict/direction/model_id) is wire-legal under whole-row LWW.
const MEMORY_REALITY_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("memory_id", ValueType::Text),
];

const MEMORY_REALITY_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("content_hash", ValueType::Text),
    ColumnSpec::required("verdict", ValueType::Text),
    ColumnSpec::required("direction", ValueType::Text),
    ColumnSpec::required("checked_against_commit", ValueType::Text),
    ColumnSpec::required("checked_inputs_hash", ValueType::Text),
    ColumnSpec::required("evidence_json", ValueType::Text),
    ColumnSpec::required("model_id", ValueType::Text),
    ColumnSpec::required("prompt_version", ValueType::Text),
    ColumnSpec::required("checked_at_ms", ValueType::I64),
];

const MEMORY_REALITY: TableSpec = TableSpec {
    name: "memory_reality",
    scope_id: ScopeId::OVERLAY,
    spec_version: 1,
    pk: MEMORY_REALITY_PK,
    columns: MEMORY_REALITY_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// RETIRED (#1319): the per-content-hash summary table. Keyed WITH `content_hash`, so every
// regeneration was a `Remove` of the old row plus an `Upsert` of the new one — a tombstone on every
// device and a pinned entry on the regenerating chain that nothing collects (#1295). It stays
// registered because retained entries name it (a table that leaves the registry strands them as
// `TableNotInScope`) and older binaries still author into it; nothing on this binary reads or
// writes it. The live table is `memory_note_summaries` below.
const MEMORY_SUMMARIES_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("memory_id", ValueType::Text),
    ColumnSpec::required("content_hash", ValueType::Text),
];

const MEMORY_SUMMARIES_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("summary", ValueType::Text),
    ColumnSpec::required("model_id", ValueType::Text),
    ColumnSpec::required("prompt_version", ValueType::Text),
    ColumnSpec::required("generated_at_ms", ValueType::I64),
];

const MEMORY_SUMMARIES: TableSpec = TableSpec {
    name: "memory_summaries",
    scope_id: ScopeId::OVERLAY,
    spec_version: 1,
    pk: MEMORY_SUMMARIES_PK,
    columns: MEMORY_SUMMARIES_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// The summary of a memory's CURRENT note, one row per memory. `content_hash` is a synced column,
// not part of the key: every reader already selects on it (and on `prompt_version`) against the
// memory's current note, so a stale row is rejected without the key having to change — and a
// regeneration is one `Upsert` of the same row, never a delete. Every non-pk column is regenerable
// model output; nothing is local.
const MEMORY_NOTE_SUMMARIES_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("memory_id", ValueType::Text),
];

const MEMORY_NOTE_SUMMARIES_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("content_hash", ValueType::Text),
    ColumnSpec::required("summary", ValueType::Text),
    ColumnSpec::required("model_id", ValueType::Text),
    ColumnSpec::required("prompt_version", ValueType::Text),
    ColumnSpec::required("generated_at_ms", ValueType::I64),
];

const MEMORY_NOTE_SUMMARIES: TableSpec = TableSpec {
    name: "memory_note_summaries",
    scope_id: ScopeId::OVERLAY,
    spec_version: 1,
    pk: MEMORY_NOTE_SUMMARIES_PK,
    columns: MEMORY_NOTE_SUMMARIES_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// A distilled papertrail record — costly LLM output keyed by the thread natural key. Every non-pk
// column is portable derived output or a mechanical facet; the parent carries no checkout-local
// resolution state (that lives on `papertrail_distill_anchors`, a later scope stage). The 0/1
// verified/override facets declare `Bool` (wire-validated to 0/1); `quotes_materialized` and
// `anchors_qualified_count` are COUNTS, so `I64`, as are timestamps and versions.
const DISTILL_RECORD_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("tracker", ValueType::Text),
    ColumnSpec::required("project", ValueType::Text),
    ColumnSpec::required("item_kind", ValueType::Text),
    ColumnSpec::required("item_key", ValueType::Text),
];

const DISTILL_RECORD_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("distill_input_hash", ValueType::Text),
    ColumnSpec::required("pipeline_version", ValueType::I64),
    ColumnSpec::required("root_issue", ValueType::Text),
    ColumnSpec::required("root_cause", ValueType::Text),
    ColumnSpec::required("root_cause_class", ValueType::Text),
    ColumnSpec::required("decision_chosen", ValueType::Text),
    ColumnSpec::required("outcome_summary", ValueType::Text),
    ColumnSpec::required("outcome_status_model", ValueType::Text),
    ColumnSpec::required("epistemic_status_decision", ValueType::Text),
    ColumnSpec::required("epistemic_status_outcome", ValueType::Text),
    ColumnSpec::required("fix_edge_source", ValueType::Text),
    ColumnSpec::required("quotes_materialized", ValueType::I64),
    ColumnSpec::required("anchors_qualified_count", ValueType::I64),
    ColumnSpec::required("thread_shape", ValueType::Text),
    ColumnSpec::required("outcome_claim_verified", ValueType::Bool),
    ColumnSpec::required("decision_provenance_verified", ValueType::Bool),
    ColumnSpec::required("revert_override", ValueType::Bool),
    ColumnSpec::required("closing_keyword_floor", ValueType::Text),
    ColumnSpec::required("distilled_at_ms", ValueType::I64),
    ColumnSpec::required("prompt_version", ValueType::I64),
    ColumnSpec::required("model_input_hash", ValueType::Text),
];

const DISTILL_RECORD: TableSpec = TableSpec {
    name: "papertrail_distill",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    pk: DISTILL_RECORD_PK,
    columns: DISTILL_RECORD_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// A distill-cluster edge (coalesced / supersedes / promoted), keyed by the full edge tuple — its
// own natural key, distinct from the thread key its parent uses. `created_at_ms` is the only synced
// non-key column.
const DISTILL_EDGE_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("tracker", ValueType::Text),
    ColumnSpec::required("project", ValueType::Text),
    ColumnSpec::required("src_item_kind", ValueType::Text),
    ColumnSpec::required("src_item_key", ValueType::Text),
    ColumnSpec::required("dst_item_kind", ValueType::Text),
    ColumnSpec::required("dst_item_key", ValueType::Text),
    ColumnSpec::required("edge_kind", ValueType::Text),
];

const DISTILL_EDGE_COLUMNS: &[ColumnSpec] =
    &[ColumnSpec::required("created_at_ms", ValueType::I64)];

const DISTILL_EDGES: TableSpec = TableSpec {
    name: "papertrail_distill_edges",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    pk: DISTILL_EDGE_PK,
    columns: DISTILL_EDGE_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// A rejected alternative, an ordinal junction keyed by the thread + its stable ordinal.
const DISTILL_ALTERNATIVE_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("tracker", ValueType::Text),
    ColumnSpec::required("project", ValueType::Text),
    ColumnSpec::required("item_kind", ValueType::Text),
    ColumnSpec::required("item_key", ValueType::Text),
    ColumnSpec::required("ordinal", ValueType::I64),
];

const DISTILL_ALTERNATIVE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("alternative", ValueType::Text),
    ColumnSpec::required("reason", ValueType::Text),
];

const DISTILL_ALTERNATIVES: TableSpec = TableSpec {
    name: "papertrail_distill_alternatives",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    pk: DISTILL_ALTERNATIVE_PK,
    columns: DISTILL_ALTERNATIVE_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// A mechanical fixing-commit link, keyed by the thread + commit SHA. `created_at_ms` (when the link
// was recorded) is the sole synced non-key column — it exists so the otherwise key-only table has a
// value for the whole-row apply path to carry.
const DISTILL_RECORD_COMMIT_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("tracker", ValueType::Text),
    ColumnSpec::required("project", ValueType::Text),
    ColumnSpec::required("item_kind", ValueType::Text),
    ColumnSpec::required("item_key", ValueType::Text),
    ColumnSpec::required("commit_sha", ValueType::Text),
];

const DISTILL_RECORD_COMMIT_COLUMNS: &[ColumnSpec] =
    &[ColumnSpec::required("created_at_ms", ValueType::I64)];

const DISTILL_RECORD_COMMITS: TableSpec = TableSpec {
    name: "papertrail_distill_record_commits",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    pk: DISTILL_RECORD_COMMIT_PK,
    columns: DISTILL_RECORD_COMMIT_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// An evidence unit: a byte-span citation with a materialized quote. No natural unique key
// (duplicate citations are possible), so the row is keyed by a per-thread `ordinal` the drain
// assigns in citation order. Every non-pk column is portable evidence/provenance; nothing is
// checkout-local.
const DISTILL_EVIDENCE_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("tracker", ValueType::Text),
    ColumnSpec::required("project", ValueType::Text),
    ColumnSpec::required("item_kind", ValueType::Text),
    ColumnSpec::required("item_key", ValueType::Text),
    ColumnSpec::required("ordinal", ValueType::I64),
];

const DISTILL_EVIDENCE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("field", ValueType::Text),
    ColumnSpec::required("source_kind", ValueType::Text),
    ColumnSpec::required("source_part", ValueType::Text),
    ColumnSpec::required("source_id", ValueType::Text),
    ColumnSpec::required("byte_start", ValueType::I64),
    ColumnSpec::required("byte_end", ValueType::I64),
    ColumnSpec::required("quote", ValueType::Text),
    ColumnSpec::required("author", ValueType::Text),
    ColumnSpec::required("author_kind", ValueType::Text),
    ColumnSpec::required("author_association", ValueType::Text),
    ColumnSpec::required("unit_created_at_ms", ValueType::I64),
];

const DISTILL_EVIDENCE: TableSpec = TableSpec {
    name: "papertrail_distill_evidence",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    pk: DISTILL_EVIDENCE_PK,
    columns: DISTILL_EVIDENCE_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// An anchor candidate, keyed by the thread + its stable candidate ordinal. Portable facts (kind,
// exact file path, name, model selection) replicate; `logical_symbol_id`/`resolved` are
// checkout-local resolution state — a `sym_<hex>` handle valid only in the local index, remapped
// per checkout by the relocation engine — so they are `local_columns` and never cross the wire. (A
// synced symbol anchor therefore surfaces as drive-by only after a local re-resolution path exists;
// a file anchor surfaces immediately via `records_for_path`.)
const DISTILL_ANCHOR_PK: &[ColumnSpec] = &[
    ColumnSpec::required("repo_id", ValueType::Text),
    ColumnSpec::required("tracker", ValueType::Text),
    ColumnSpec::required("project", ValueType::Text),
    ColumnSpec::required("item_kind", ValueType::Text),
    ColumnSpec::required("item_key", ValueType::Text),
    ColumnSpec::required("candidate_ordinal", ValueType::I64),
];

const DISTILL_ANCHOR_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("anchor_kind", ValueType::Text),
    ColumnSpec::required("file_path", ValueType::Text),
    ColumnSpec::required("name", ValueType::Text),
    ColumnSpec::required("selected", ValueType::Bool),
];

const DISTILL_ANCHOR_LOCAL_COLUMNS: &[&str] = &["logical_symbol_id", "resolved"];

const DISTILL_ANCHORS: TableSpec = TableSpec {
    name: "papertrail_distill_anchors",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    pk: DISTILL_ANCHOR_PK,
    columns: DISTILL_ANCHOR_COLUMNS,
    local_columns: DISTILL_ANCHOR_LOCAL_COLUMNS,
    repo_column: Some("repo_id"),
};

/// The production table registry. `anchors/1` retains durable memory-binding history in full;
/// `overlay/1` carries regenerable dream output (verdicts, summaries) and `distill/1` the distilled
/// papertrail record plus its enrichment children — both under a bounded retention budget.
pub(crate) const SYNCABLE_TABLES: &[TableSpec] = &[
    MEMORY_BINDINGS,
    MEMORY_REALITY,
    MEMORY_SUMMARIES,
    DISTILL_RECORD,
    DISTILL_EDGES,
    DISTILL_ALTERNATIVES,
    DISTILL_RECORD_COMMITS,
    DISTILL_EVIDENCE,
    DISTILL_ANCHORS,
    MEMORY_NOTE_SUMMARIES,
];

/// The per-repo Lens lane metas a scope's applied rows advance — the aggregate enrichment clock
/// plus the scope's specific lane. Returned to the apply-side and refold bump sites, which cannot
/// always name the applied table but always know the stream/entry's `scope_id`. An unknown scope
/// advances nothing. Keep in sync with the scopes in [`SYNCABLE_TABLES`].
/// Advance the Lens lanes `scope_id` feeds after an applied entry changed `repo_id`'s derived
/// state. Gated on repo registration: `repo_meta` has a foreign key to `repos`, so an ungated
/// bump for a placeholder id would fail rather than no-op.
pub(crate) fn bump_scope_lanes(
    tx: &rusqlite::Transaction<'_>,
    scope_id: &str,
    repo_id: &str,
) -> anyhow::Result<()> {
    let lens_metas = scope_lens_metas(scope_id);
    if !lens_metas.is_empty() && rag_rat_db::schema::repo_id_is_registered(tx, repo_id)? {
        rag_rat_db::meta::bump_lens_revisions(tx, repo_id, lens_metas)?;
    }
    Ok(())
}

pub(crate) fn scope_lens_metas(scope_id: &str) -> &'static [&'static str] {
    match ScopeId::from_db_str(scope_id) {
        // anchors/1 and overlay/1 are memory-facing scopes.
        Some(ScopeId::ANCHORS | ScopeId::OVERLAY) => &[
            rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
            rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
        ],
        Some(ScopeId::DISTILL) => &[
            rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
            rag_rat_db::meta::LENS_PAPERTRAIL_REVISION_META,
        ],
        _ => &[],
    }
}

/// One table's REPLICATED CONTRACT within a projector generation — everything that decides what
/// this binary can project from the wire, and nothing that does not.
///
/// `scope_id` is part of it, not decoration: it selects the stream a table rides, so moving a table
/// between scopes turns entries the old registry parked as `TableNotInScope` into entries the new
/// one understands. Omitting it would let that edit land without a projector bump, and those
/// entries would never be retried. `pk` and each column's `ValueType` are recorded for a different
/// reason — changing either means a NEW TABLE under the additive-only rule, which is stated as an
/// un-lintable invariant precisely because a single binary has no history to check against. A
/// generation list IS that history, so the cross-generation test can enforce part of it.
///
/// `local_columns` is deliberately absent: it never crosses the wire, so changing it widens
/// nothing and must not force a generation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TableGeneration {
    pub table: &'static str,
    pub scope_id: ScopeId,
    pub spec_version: u32,
    pub repo_column: Option<&'static str>,
    /// `(column, value_type)` per identity column, in registry order.
    pub pk: &'static [(&'static str, ValueType)],
    /// `(column, value_type, added)` per synced column, in registry order. `added` is `None` for a
    /// column present since the table's first version.
    pub columns: &'static [(&'static str, ValueType, Option<AddedColumn>)],
}

// Each distinct table snapshot is written ONCE, named by its table and spec version; a generation
// lists the snapshots it contained. A table whose replicated contract changes gets a NEW const
// (`_V2`, ...) in the generation that changed it — a landed const is history and is never edited,
// exactly like the generation list itself.
const REPO_MEMORY_BINDINGS_V1: TableGeneration = TableGeneration {
    table: "repo_memory_bindings",
    scope_id: ScopeId::ANCHORS,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[
        ("repo_id", ValueType::Text),
        ("memory_id", ValueType::Text),
        ("binding_kind", ValueType::Text),
        ("binding_id", ValueType::Text),
    ],
    columns: &[
        ("path", ValueType::Text, None),
        ("start_line", ValueType::I64, None),
        ("end_line", ValueType::I64, None),
        ("commit_hash", ValueType::Text, None),
        ("tracker", ValueType::Text, None),
        ("project", ValueType::Text, None),
        ("item_key", ValueType::Text, None),
        ("created_at_ms", ValueType::I64, None),
        ("symbol_kind", ValueType::Text, None),
        ("signature_hash", ValueType::Text, None),
        ("moniker_tool", ValueType::Text, None),
        ("moniker_tool_version", ValueType::Text, None),
    ],
};

const MEMORY_REALITY_V1: TableGeneration = TableGeneration {
    table: "memory_reality",
    scope_id: ScopeId::OVERLAY,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[("repo_id", ValueType::Text), ("memory_id", ValueType::Text)],
    columns: &[
        ("content_hash", ValueType::Text, None),
        ("verdict", ValueType::Text, None),
        ("direction", ValueType::Text, None),
        ("checked_against_commit", ValueType::Text, None),
        ("checked_inputs_hash", ValueType::Text, None),
        ("evidence_json", ValueType::Text, None),
        ("model_id", ValueType::Text, None),
        ("prompt_version", ValueType::Text, None),
        ("checked_at_ms", ValueType::I64, None),
    ],
};

const MEMORY_SUMMARIES_V1: TableGeneration = TableGeneration {
    table: "memory_summaries",
    scope_id: ScopeId::OVERLAY,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[
        ("repo_id", ValueType::Text),
        ("memory_id", ValueType::Text),
        ("content_hash", ValueType::Text),
    ],
    columns: &[
        ("summary", ValueType::Text, None),
        ("model_id", ValueType::Text, None),
        ("prompt_version", ValueType::Text, None),
        ("generated_at_ms", ValueType::I64, None),
    ],
};

const MEMORY_NOTE_SUMMARIES_V1: TableGeneration = TableGeneration {
    table: "memory_note_summaries",
    scope_id: ScopeId::OVERLAY,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[("repo_id", ValueType::Text), ("memory_id", ValueType::Text)],
    columns: &[
        ("content_hash", ValueType::Text, None),
        ("summary", ValueType::Text, None),
        ("model_id", ValueType::Text, None),
        ("prompt_version", ValueType::Text, None),
        ("generated_at_ms", ValueType::I64, None),
    ],
};

const PAPERTRAIL_DISTILL_V1: TableGeneration = TableGeneration {
    table: "papertrail_distill",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[
        ("repo_id", ValueType::Text),
        ("tracker", ValueType::Text),
        ("project", ValueType::Text),
        ("item_kind", ValueType::Text),
        ("item_key", ValueType::Text),
    ],
    columns: &[
        ("distill_input_hash", ValueType::Text, None),
        ("pipeline_version", ValueType::I64, None),
        ("root_issue", ValueType::Text, None),
        ("root_cause", ValueType::Text, None),
        ("root_cause_class", ValueType::Text, None),
        ("decision_chosen", ValueType::Text, None),
        ("outcome_summary", ValueType::Text, None),
        ("outcome_status_model", ValueType::Text, None),
        ("epistemic_status_decision", ValueType::Text, None),
        ("epistemic_status_outcome", ValueType::Text, None),
        ("fix_edge_source", ValueType::Text, None),
        ("quotes_materialized", ValueType::I64, None),
        ("anchors_qualified_count", ValueType::I64, None),
        ("thread_shape", ValueType::Text, None),
        ("outcome_claim_verified", ValueType::Bool, None),
        ("decision_provenance_verified", ValueType::Bool, None),
        ("revert_override", ValueType::Bool, None),
        ("closing_keyword_floor", ValueType::Text, None),
        ("distilled_at_ms", ValueType::I64, None),
        ("prompt_version", ValueType::I64, None),
        ("model_input_hash", ValueType::Text, None),
    ],
};

const PAPERTRAIL_DISTILL_EDGES_V1: TableGeneration = TableGeneration {
    table: "papertrail_distill_edges",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[
        ("repo_id", ValueType::Text),
        ("tracker", ValueType::Text),
        ("project", ValueType::Text),
        ("src_item_kind", ValueType::Text),
        ("src_item_key", ValueType::Text),
        ("dst_item_kind", ValueType::Text),
        ("dst_item_key", ValueType::Text),
        ("edge_kind", ValueType::Text),
    ],
    columns: &[("created_at_ms", ValueType::I64, None)],
};

const PAPERTRAIL_DISTILL_ALTERNATIVES_V1: TableGeneration = TableGeneration {
    table: "papertrail_distill_alternatives",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[
        ("repo_id", ValueType::Text),
        ("tracker", ValueType::Text),
        ("project", ValueType::Text),
        ("item_kind", ValueType::Text),
        ("item_key", ValueType::Text),
        ("ordinal", ValueType::I64),
    ],
    columns: &[("alternative", ValueType::Text, None), ("reason", ValueType::Text, None)],
};

const PAPERTRAIL_DISTILL_RECORD_COMMITS_V1: TableGeneration = TableGeneration {
    table: "papertrail_distill_record_commits",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[
        ("repo_id", ValueType::Text),
        ("tracker", ValueType::Text),
        ("project", ValueType::Text),
        ("item_kind", ValueType::Text),
        ("item_key", ValueType::Text),
        ("commit_sha", ValueType::Text),
    ],
    columns: &[("created_at_ms", ValueType::I64, None)],
};

const PAPERTRAIL_DISTILL_EVIDENCE_V1: TableGeneration = TableGeneration {
    table: "papertrail_distill_evidence",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[
        ("repo_id", ValueType::Text),
        ("tracker", ValueType::Text),
        ("project", ValueType::Text),
        ("item_kind", ValueType::Text),
        ("item_key", ValueType::Text),
        ("ordinal", ValueType::I64),
    ],
    columns: &[
        ("field", ValueType::Text, None),
        ("source_kind", ValueType::Text, None),
        ("source_part", ValueType::Text, None),
        ("source_id", ValueType::Text, None),
        ("byte_start", ValueType::I64, None),
        ("byte_end", ValueType::I64, None),
        ("quote", ValueType::Text, None),
        ("author", ValueType::Text, None),
        ("author_kind", ValueType::Text, None),
        ("author_association", ValueType::Text, None),
        ("unit_created_at_ms", ValueType::I64, None),
    ],
};

const PAPERTRAIL_DISTILL_ANCHORS_V1: TableGeneration = TableGeneration {
    table: "papertrail_distill_anchors",
    scope_id: ScopeId::DISTILL,
    spec_version: 1,
    repo_column: Some("repo_id"),
    pk: &[
        ("repo_id", ValueType::Text),
        ("tracker", ValueType::Text),
        ("project", ValueType::Text),
        ("item_kind", ValueType::Text),
        ("item_key", ValueType::Text),
        ("candidate_ordinal", ValueType::I64),
    ],
    columns: &[
        ("anchor_kind", ValueType::Text, None),
        ("file_path", ValueType::Text, None),
        ("name", ValueType::Text, None),
        ("selected", ValueType::Bool, None),
    ],
};

/// The registry as of EACH projector generation, oldest first: a generation's index + 1 is the
/// [`TABLE_SYNC_PROJECTOR_VERSION`] it describes, and the LAST entry must equal the live registry.
///
/// This is the mechanical coupling between a registry change and a projector bump, and it is
/// load-bearing rather than documentation. A refold is owed only when the store's stamp is behind
/// the current projector version, or some entry was parked by an older one — so if the registry
/// widens (a table registered, a column added) WITHOUT the version moving, a store already stamped
/// at that version keeps entries parked as `TableNotInScope` / `NewerSpecVersion` with a
/// `pending_projector_version` equal to the current one. Neither trigger fires, they are never
/// replayed, and redelivery cannot rescue them because it short-circuits on `entry_exists`. The
/// payload is simply lost.
///
/// A pinned copy of the current registry cannot enforce this: updating the pin to match a change is
/// exactly as easy as making the change. Recording a generation PER VERSION can, because the live
/// registry must equal the last entry — so widening the registry forces an APPEND, and appending
/// moves `len()`, which is the version. Widenings that are not registry changes (a new op-kind)
/// append a generation that repeats the previous snapshot.
///
/// Entries here are HISTORY. Append only; never edit a landed generation.
pub(crate) const PROJECTOR_GENERATIONS: &[&[TableGeneration]] = &[
    // v1: the engine exists; no table is registered yet.
    &[],
    // v2: durable memory anchors replicate on the retained anchors/1 stream.
    &[REPO_MEMORY_BINDINGS_V1],
    // v3: regenerable dream output (verdicts, summaries) replicates on the bounded overlay/1
    // stream. A generation is a whole-registry snapshot, so this repeats the anchors table and
    // adds the two overlay tables, in `SYNCABLE_TABLES` order.
    &[REPO_MEMORY_BINDINGS_V1, MEMORY_REALITY_V1, MEMORY_SUMMARIES_V1],
    // v4: distilled papertrail records replicate on the bounded distill/1 stream. A generation is
    // a whole-registry snapshot, so this repeats v3's three tables and adds the distill
    // parent, in `SYNCABLE_TABLES` order.
    &[REPO_MEMORY_BINDINGS_V1, MEMORY_REALITY_V1, MEMORY_SUMMARIES_V1, PAPERTRAIL_DISTILL_V1],
    // v5: the distill edges + alternatives enrichment children join distill/1. Whole-registry
    // snapshot: v4's four tables plus the two children, in `SYNCABLE_TABLES` order.
    &[
        REPO_MEMORY_BINDINGS_V1,
        MEMORY_REALITY_V1,
        MEMORY_SUMMARIES_V1,
        PAPERTRAIL_DISTILL_V1,
        PAPERTRAIL_DISTILL_EDGES_V1,
        PAPERTRAIL_DISTILL_ALTERNATIVES_V1,
    ],
    // v6: the distill record_commits child joins distill/1. Whole-registry snapshot: v5's six
    // tables plus record_commits, in `SYNCABLE_TABLES` order.
    &[
        REPO_MEMORY_BINDINGS_V1,
        MEMORY_REALITY_V1,
        MEMORY_SUMMARIES_V1,
        PAPERTRAIL_DISTILL_V1,
        PAPERTRAIL_DISTILL_EDGES_V1,
        PAPERTRAIL_DISTILL_ALTERNATIVES_V1,
        PAPERTRAIL_DISTILL_RECORD_COMMITS_V1,
    ],
    // v7: the distill evidence child joins distill/1. Whole-registry snapshot: v6's seven tables
    // plus evidence, in `SYNCABLE_TABLES` order.
    &[
        REPO_MEMORY_BINDINGS_V1,
        MEMORY_REALITY_V1,
        MEMORY_SUMMARIES_V1,
        PAPERTRAIL_DISTILL_V1,
        PAPERTRAIL_DISTILL_EDGES_V1,
        PAPERTRAIL_DISTILL_ALTERNATIVES_V1,
        PAPERTRAIL_DISTILL_RECORD_COMMITS_V1,
        PAPERTRAIL_DISTILL_EVIDENCE_V1,
    ],
    // v8: the distill anchors child joins distill/1. Whole-registry snapshot: v7's eight tables
    // plus anchors, in `SYNCABLE_TABLES` order. Anchors' local columns (logical_symbol_id,
    // resolved) are excluded from the generation by design.
    &[
        REPO_MEMORY_BINDINGS_V1,
        MEMORY_REALITY_V1,
        MEMORY_SUMMARIES_V1,
        PAPERTRAIL_DISTILL_V1,
        PAPERTRAIL_DISTILL_EDGES_V1,
        PAPERTRAIL_DISTILL_ALTERNATIVES_V1,
        PAPERTRAIL_DISTILL_RECORD_COMMITS_V1,
        PAPERTRAIL_DISTILL_EVIDENCE_V1,
        PAPERTRAIL_DISTILL_ANCHORS_V1,
    ],
    // v9: the per-memory summary table joins overlay/1 (#1319); `memory_summaries` stays, retired.
    // Whole-registry snapshot: v8's nine tables plus the new one, in `SYNCABLE_TABLES` order.
    &[
        REPO_MEMORY_BINDINGS_V1,
        MEMORY_REALITY_V1,
        MEMORY_SUMMARIES_V1,
        PAPERTRAIL_DISTILL_V1,
        PAPERTRAIL_DISTILL_EDGES_V1,
        PAPERTRAIL_DISTILL_ALTERNATIVES_V1,
        PAPERTRAIL_DISTILL_RECORD_COMMITS_V1,
        PAPERTRAIL_DISTILL_EVIDENCE_V1,
        PAPERTRAIL_DISTILL_ANCHORS_V1,
        MEMORY_NOTE_SUMMARIES_V1,
    ],
    // v10: the `restate` row-op kind (#1295) — a widening that is not a registry change, so the
    // snapshot repeats v9. The bump is what schedules the replay of entries an older binary parked
    // as `UnknownOpKind`, which is how a row deleted below a floor that binary adopted goes once
    // it upgrades.
    &[
        REPO_MEMORY_BINDINGS_V1,
        MEMORY_REALITY_V1,
        MEMORY_SUMMARIES_V1,
        PAPERTRAIL_DISTILL_V1,
        PAPERTRAIL_DISTILL_EDGES_V1,
        PAPERTRAIL_DISTILL_ALTERNATIVES_V1,
        PAPERTRAIL_DISTILL_RECORD_COMMITS_V1,
        PAPERTRAIL_DISTILL_EVIDENCE_V1,
        PAPERTRAIL_DISTILL_ANCHORS_V1,
        MEMORY_NOTE_SUMMARIES_V1,
    ],
];

/// Assert a spec classifies EVERY physical column of its table exactly once — as pk, a synced
/// column, or a local column — and names no column the table doesn't have. This is the invariant
/// that stops a new column being silently unclassified: it is either replicated or deliberately
/// local, never neither. Returns a human-readable diff on mismatch.
pub(crate) fn assert_spec_covers_schema(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    let columns = schema_facts::physical_column_info(conn, spec.name)
        .map_err(|err| format!("cannot read columns of `{}`: {err}", spec.name))?;
    // Order is part of the contract: the first failing rule is the one reported. The later rules
    // read their own schema facts, so a failed read surfaces only after every earlier rule passed.
    rule_every_column_classified_once(spec, &columns)?;
    rule_repo_scope_is_a_text_pk(spec)?;
    rule_declares_a_synced_column(spec)?;
    rule_declared_pk_is_the_table_pk(spec, &columns)?;
    rule_pk_columns_are_not_null(spec, &columns)?;
    rule_pk_uses_binary_collation(conn, spec)?;
    rule_no_outbound_foreign_key(conn, spec)?;
    rule_no_inbound_foreign_key(conn, spec)?;
    rule_no_trigger(conn, spec)?;
    rule_no_non_pk_unique_index(conn, spec)?;
    rule_identity_columns_are_never_added(spec)?;
    rule_added_column_defaults_converge(conn, spec, &columns)?;
    rule_local_columns_are_materializable(spec, &columns)?;
    rule_no_generated_column(conn, spec)?;
    rule_table_is_strict(conn, spec)?;
    rule_value_types_match_physical(spec, &columns)
}

fn rule_every_column_classified_once(
    spec: &TableSpec,
    columns: &[PhysicalColumn],
) -> Result<(), String> {
    let mut classified: BTreeSet<&str> = BTreeSet::new();
    let mut duplicated: Vec<&str> = Vec::new();
    let declared = spec
        .pk
        .iter()
        .map(|c| c.name)
        .chain(spec.columns.iter().map(|c| c.name))
        .chain(spec.local_columns.iter().copied());
    for name in declared {
        if !classified.insert(name) {
            duplicated.push(name);
        }
    }
    if !duplicated.is_empty() {
        return Err(format!(
            "`{}`: column(s) classified more than once: {duplicated:?}",
            spec.name
        ));
    }

    let physical_set: BTreeSet<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    let unclassified: Vec<&str> = physical_set.difference(&classified).copied().collect();
    let absent: Vec<&str> = classified.difference(&physical_set).copied().collect();
    if !unclassified.is_empty() || !absent.is_empty() {
        return Err(format!(
            "`{}` registry/schema mismatch: physical columns not classified {unclassified:?}; \
             classified columns absent from the table {absent:?}",
            spec.name
        ));
    }
    Ok(())
}

/// A repo scope must be a primary-key column: only then does the applier's repo-identity gate fire
/// on every incoming op. A non-pk repo column would filter the producer but leave ingest unguarded,
/// so a peer could write another repo's row into the shared table.
fn rule_repo_scope_is_a_text_pk(spec: &TableSpec) -> Result<(), String> {
    let Some(repo_column) = spec.repo_column else {
        return Ok(());
    };
    match spec.repo_pk_index() {
        None => Err(format!(
            "`{}`: repo_column `{repo_column}` must be a primary-key column so the ingest repo \
             gate applies",
            spec.name
        )),
        // The applier's repo gate compares the repo pk value to `TypedValue::Text(repo_id)`, so a
        // non-Text scope key never matches — every locally-produced row self-quarantines and the
        // whole table can never sync.
        Some(idx) if spec.pk[idx].value_type != ValueType::Text => Err(format!(
            "`{}`: repo_column `{repo_column}` must be ValueType::Text (the repo gate compares it \
             to the text repo_id)",
            spec.name
        )),
        Some(_) => Ok(()),
    }
}

/// The whole-row apply/produce SQL builds a `SELECT`/`SET` over the synced columns; a spec with no
/// synced non-key column would emit empty-column SQL (`SELECT  FROM …`) at apply time. A key-only
/// (pure set-membership) table would need a deliberately designed empty-row path (existence-only
/// hash, no-op update) that whole-row LWW does not have — reject it here rather than emit invalid
/// SQL when its first row is applied.
fn rule_declares_a_synced_column(spec: &TableSpec) -> Result<(), String> {
    if spec.columns.is_empty() {
        return Err(format!(
            "`{}`: a syncable table must declare at least one synced non-key column; a key-only \
             table is not supported by the whole-row apply path",
            spec.name
        ));
    }
    Ok(())
}

/// The table's primary-key columns in `PRAGMA table_info` pk order.
fn physical_pk_columns(columns: &[PhysicalColumn]) -> Vec<&PhysicalColumn> {
    let mut pk_cols: Vec<&PhysicalColumn> = columns.iter().filter(|c| c.pk_position > 0).collect();
    pk_cols.sort_by_key(|c| c.pk_position);
    pk_cols
}

/// The declared `pk` must be EXACTLY the table's real primary key, in order. The classification
/// check only matched column NAMES, so a spec could name a non-key column as `pk` (or bury a real
/// key column in `columns`/`local_columns`): row identity would then be non-unique — one op could
/// update or delete several physical rows through `pk_where`, and the per-row clock / tombstone key
/// would not identify a single row. Compare against `PRAGMA table_info`'s pk order.
fn rule_declared_pk_is_the_table_pk(
    spec: &TableSpec,
    columns: &[PhysicalColumn],
) -> Result<(), String> {
    let actual_pk: Vec<&str> =
        physical_pk_columns(columns).iter().map(|c| c.name.as_str()).collect();
    let declared_pk: Vec<&str> = spec.pk.iter().map(|c| c.name).collect();
    if actual_pk != declared_pk {
        return Err(format!(
            "`{}`: declared pk {declared_pk:?} does not match the table's primary key \
             {actual_pk:?} (identical columns, identical order required)",
            spec.name
        ));
    }
    Ok(())
}

/// Every pk column must be NOT NULL. A rowid table's bare `id TEXT PRIMARY KEY` is NULLABLE
/// (SQLite's historic quirk) — a NULL pk is unaddressable, so `read_all_rows` emits a Null pk that
/// self-apply quarantines, and `produce_and_author` then re-signs that ghost row on every pass. A
/// STRICT table makes its pk NOT NULL (table_info reports it), which is the intended shape.
fn rule_pk_columns_are_not_null(
    spec: &TableSpec,
    columns: &[PhysicalColumn],
) -> Result<(), String> {
    for col in physical_pk_columns(columns) {
        if !col.not_null {
            return Err(format!(
                "`{}`: primary-key column `{}` is nullable — declare it NOT NULL (a STRICT table \
                 does this implicitly); a NULL pk is unaddressable and self-quarantines",
                spec.name, col.name
            ));
        }
    }
    Ok(())
}

/// Every pk column must use BINARY equality. A non-binary collation (e.g. `COLLATE NOCASE`) makes
/// SQLite treat values differing only by collation as ONE row in the `WHERE` predicates, but
/// `row_op::row_pk_string` encodes them as DIFFERENT bookkeeping identities — so one physical row
/// would carry two write clocks / published hashes and diverge (or suppress the wrong update).
fn rule_pk_uses_binary_collation(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    if let Some(col) = schema_facts::pk_column_with_non_binary_collation(conn, spec.name)
        .map_err(|err| format!("cannot read the pk collation of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: primary-key column `{col}` uses a non-BINARY collation — the row-clock \
             encoding is byte-exact, so collation-equal keys would split one row's bookkeeping; \
             use BINARY",
            spec.name
        ));
    }
    Ok(())
}

/// Whole-row LWW converges per row INDEPENDENTLY — each row's fate is decided solely by its own
/// write clock. A CROSS-ROW constraint breaks that: the same op set can fold to different states
/// under different arrival orders (two rows racing for one UNIQUE value: whichever loses is
/// quarantined, and WHICH loses depends on order), so peers diverge with no dirty-local edit. A
/// foreign key is the same class (a delete/insert can fail against another row). Reject both until
/// a deterministic cross-row conflict rule exists.
fn rule_no_outbound_foreign_key(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    if schema_facts::table_has_foreign_key(conn, spec.name)
        .map_err(|err| format!("cannot read foreign keys of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: a foreign key makes whole-row LWW order-dependent (an op can fail against \
             another row) — not supported",
            spec.name
        ));
    }
    Ok(())
}

/// The inbound direction is the same hazard: a table REFERENCED by another's FK can have a `Remove`
/// blocked (FK RESTRICT) on a peer that holds a child row but not on one that doesn't → the delete
/// quarantines on one side, applies on the other, and the replicas diverge.
fn rule_no_inbound_foreign_key(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    if schema_facts::table_is_referenced_by_foreign_key(conn, spec.name)
        .map_err(|err| format!("cannot scan foreign keys referencing `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: another table has a foreign key referencing it — a delete can be blocked on \
             one peer but not another, so whole-row LWW diverges — not supported",
            spec.name
        ));
    }
    Ok(())
}

/// A trigger breaks the whole-row fold's assumption that a row write is independent and
/// deterministic: an INSERT/UPDATE/DELETE trigger can adjust the row (or others) from local derived
/// state, so the SAME received op folds to different physical results on two devices, and
/// apply_upsert then publishes each divergent result — the replicas stay divergent.
fn rule_no_trigger(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    if let Some(trigger) = schema_facts::table_trigger(conn, spec.name)
        .map_err(|err| format!("cannot read triggers of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: trigger `{trigger}` can mutate a row from local/derived state, so the same op \
             folds differently across devices — not supported",
            spec.name
        ));
    }
    Ok(())
}

/// A non-pk UNIQUE index is the cross-row constraint class [`rule_no_outbound_foreign_key`]
/// describes: two rows racing for one value diverge by arrival order.
fn rule_no_non_pk_unique_index(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    if let Some(index) = schema_facts::non_pk_unique_index(conn, spec.name)
        .map_err(|err| format!("cannot read indexes of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: UNIQUE index `{index}` is a cross-row constraint that makes whole-row LWW \
             order-dependent (two rows racing for one value diverge by arrival order) — not \
             supported",
            spec.name
        ));
    }
    Ok(())
}

/// An IDENTITY column can never be `added`. The declared default is unreachable for it: an op
/// authored before the key grew carries fewer pk values, and `apply_row_op_on_stream`'s arity check
/// quarantines it TERMINALLY before projection ever runs — so the evolution the `added` shape
/// promises simply does not exist here, and declaring it would advertise a redemption path that
/// silently drops every older op instead. A changed primary key is a new table identity.
fn rule_identity_columns_are_never_added(spec: &TableSpec) -> Result<(), String> {
    for key in spec.pk {
        if key.added.is_some() {
            return Err(format!(
                "`{}`: identity column `{}` declares an introduction version — a primary key \
                 cannot grow (an older op carries fewer pk values and is quarantined on arity \
                 before its default could apply); a changed key means a NEW TABLE",
                spec.name, key.name
            ));
        }
    }
    Ok(())
}

/// A synced column's DECLARED default must equal its physical SQL default, exactly (#1002).
///
/// This is a CONVERGENCE check for the upgrade path, not hygiene. `ALTER TABLE ADD COLUMN`
/// backfills existing rows with the SQL default, while the applier fills a column an older op
/// omits with the DECLARED one. If the two disagree, a device that applied an op BEFORE upgrading
/// and one that applied the same op AFTER hold different rows AT THE SAME CLOCK — silent
/// divergence with no local edit to signal it, and nothing to repair it while the authoring entry
/// is unavailable.
///
/// KNOW ITS LIMIT. The real invariant is "the migration that introduces the column backfills
/// existing rows with the DECLARED default", and this reads `PRAGMA table_info.dflt_value` — the
/// DEFAULT CLAUSE. Those coincide only for `ALTER TABLE ADD COLUMN … DEFAULT x`. They do NOT
/// coincide for a table REBUILD (`CREATE new; INSERT INTO new SELECT …, <expr> FROM old; DROP;
/// RENAME`), which is a routine migration idiom in this repo: the `SELECT` expression is invisible
/// here, so a rebuild that backfills anything other than the declared default passes this check
/// while violating the invariant. INTRODUCE A SYNCED COLUMN WITH `ADD COLUMN … DEFAULT x`, which
/// satisfies it structurally. A rebuild that computes per-row values is not wrong, but it is new
/// content peers have not seen, and it re-authors the whole table once on every device — budget
/// for that deliberately rather than discovering it.
///
/// A declared default must also match its column's `ValueType`: the fill goes straight into the
/// row, so a mistyped default would write a value the applier would have quarantined on the wire.
fn rule_added_column_defaults_converge(
    conn: &Connection,
    spec: &TableSpec,
    columns: &[PhysicalColumn],
) -> Result<(), String> {
    // Read and lex the table's DDL ONCE: its declarations and constraints are a fact about the
    // table, not about each column.
    let ddl = schema_facts::read_table_ddl(conn, spec.name)
        .map_err(|err| format!("cannot read the DDL of `{}`: {err}", spec.name))?;
    for column in spec.columns {
        let Some(added) = column.added else {
            continue;
        };
        let declared = added.default;
        let Some(physical) = columns.iter().find(|c| c.name == column.name) else {
            continue; // an unknown column name is already reported by the exhaustiveness diff above.
        };
        // The introducing version must sit inside this spec's history. `1` is the first version, so
        // a column "added" there was present from the start and is `required`; a version above the
        // spec's own names a column this binary carries but does not announce — a forgotten bump,
        // which would make the fill window wrong in both directions.
        if added.in_version < 2 || added.in_version > spec.spec_version {
            return Err(format!(
                "`{}`: column `{}` claims to be added in spec version {} — it must be between 2 \
                 and the spec's own version {} (a column present since version 1 is `required`)",
                spec.name, column.name, added.in_version, spec.spec_version
            ));
        }
        if !default_matches_value_type(declared, column.value_type) {
            return Err(format!(
                "`{}`: column `{}` declares a {declared:?} default, which is not a {:?} value",
                spec.name, column.name, column.value_type
            ));
        }
        // A NOT NULL column cannot be filled with NULL. The declared default goes straight into the
        // row, so this would fail the constraint at INSERT and quarantine the op TERMINALLY —
        // older→newer replication for the table would be dead, with nothing to redeem it. The
        // SQL-default check below does not catch it: a NOT NULL column with no DEFAULT clause reads
        // as an absent physical default, which a declared `Null` matches.
        if physical.not_null && matches!(declared, DefaultValue::Null) {
            return Err(format!(
                "`{}`: column `{}` is NOT NULL but declares a Null default — filling an older op \
                 from it would fail the constraint and quarantine the op permanently",
                spec.name, column.name
            ));
        }
        // The column must ACCEPT its own declared default, and its constraints must depend on
        // nothing but this column. Both are decided by rebuilding the column alone and attempting
        // the insert the applier would perform — see `schema_facts::default_satisfies_check`.
        //
        // Both failures end the same way and are equally silent: the applier fills this column from
        // the default while every other column comes from the OP, so a constraint the default
        // violates (or one whose other inputs the op supplies) fails at INSERT and the op is
        // QUARANTINED — terminally, so older→newer replication for the table simply stops.
        match schema_facts::default_satisfies_check(&ddl, spec.name, column.name, declared) {
            CheckVerdict::Satisfied => {},
            CheckVerdict::Violated(why) => {
                return Err(format!(
                    "`{}`: column `{}` declares default {declared:?}, which the column itself \
                     REJECTS ({why}) — every op older than the column would be filled with a \
                     value the table refuses, and quarantined terminally",
                    spec.name, column.name
                ));
            },
            CheckVerdict::NotSelfContained(why) => {
                return Err(format!(
                    "`{}`: column `{}` has a declared default but its constraints read something \
                     other than that column ({why}) — the default supplies this column while the \
                     OP supplies the rest, so a constraint can fail for a valid older op and \
                     quarantine it terminally. Keep a synced column's CHECK self-contained.",
                    spec.name, column.name
                ));
            },
        }
        let physical_default = physical.default_sql.as_deref();
        if !default_matches_sql(declared, physical_default) {
            return Err(format!(
                "`{}`: column `{}` declares default {declared:?} but the table's DEFAULT is {} — \
                 they must agree exactly, or a row backfilled by the migration and a row rebuilt \
                 from an older op differ at the same write clock",
                spec.name,
                column.name,
                physical_default.unwrap_or("absent")
            ));
        }
    }
    Ok(())
}

/// Every local (never-replicated) column must be nullable or carry a DB default: a remote upsert
/// INSERTs only the pk + synced columns (a local column is re-derived here, not sent), so a NOT
/// NULL local column with no default makes that insert fail and the applier quarantine the op — the
/// row would then be absent on every new peer that never authored it locally.
fn rule_local_columns_are_materializable(
    spec: &TableSpec,
    columns: &[PhysicalColumn],
) -> Result<(), String> {
    for local in spec.local_columns {
        if let Some(col) = columns.iter().find(|c| c.name == *local)
            && col.not_null
            && col.default_sql.is_none()
        {
            return Err(format!(
                "`{}`: local column `{local}` is NOT NULL without a default, so a remote insert \
                 (pk + synced columns only) cannot materialize the row",
                spec.name
            ));
        }
    }
    Ok(())
}

/// A GENERATED column is invisible to the rest of this lint and to the applier alike: `PRAGMA
/// table_info` omits it, so the exhaustiveness diff never classifies it, and the applier never
/// supplies it. It is not inert, though — its expression can read a synced column, and its own NOT
/// NULL and CHECK constraints then apply to a value derived from whatever the applier filled in.
/// That makes a constraint reachable through it depend on this column transitively, which the probe
/// models by name and therefore cannot see.
fn rule_no_generated_column(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    if let Some(generated) = schema_facts::generated_column(conn, spec.name)
        .map_err(|err| format!("cannot read the columns of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: column `{generated}` is GENERATED — it is absent from `PRAGMA table_info`, so \
             it can be neither replicated nor classified as local, and a constraint on it depends \
             on the synced columns its expression reads. Derive it outside the table.",
            spec.name
        ));
    }
    Ok(())
}

/// The table MUST be STRICT. STRICT enforces the declared column type at write time, so a value the
/// producer read (by its `ValueType`) can never be affinity-coerced to a different stored type. It
/// pins the storage CLASS only, not the value's domain within it — a `Bool` can still hold 2 and a
/// `Text` can still hold invalid UTF-8 — which is why `read_typed` carries those as unreadable
/// rather than relying on the schema. It also makes pk columns NOT NULL, and is the schema
/// convention for every new table regardless.
fn rule_table_is_strict(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    if !schema_facts::table_is_strict(conn, spec.name)
        .map_err(|err| format!("cannot read the schema of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: a syncable table must be STRICT (enforced column types keep an applied value \
             from being coerced to a type the producer did not read)",
            spec.name
        ));
    }
    Ok(())
}

/// Each replicated column's declared `ValueType` must match its physical STRICT type, so the value
/// the producer reads round-trips through SQLite unchanged and a peer's op passes the applier's
/// type check. (Local columns are re-derived, never sent, so they are exempt.)
fn rule_value_types_match_physical(
    spec: &TableSpec,
    columns: &[PhysicalColumn],
) -> Result<(), String> {
    for spec_col in spec.pk.iter().chain(spec.columns.iter()) {
        let Some(phys) = columns.iter().find(|c| c.name == spec_col.name) else {
            continue; // classified/absent already checked above
        };
        if !value_type_matches_declared(spec_col.value_type, &phys.decl_type) {
            return Err(format!(
                "`{}`: column `{}` is declared {:?} in the spec but has physical type `{}` — the \
                 two must agree so values round-trip unchanged",
                spec.name, spec_col.name, spec_col.value_type, phys.decl_type
            ));
        }
    }
    Ok(())
}

/// Whether a declared `ValueType` matches a physical STRICT column type. `Bool` and `I64` both
/// store as INTEGER; `Text`/`Blob` map to their obvious types. The permissive `ANY` is rejected — a
/// typed column must pin its type so a stored value can never be a different type than the producer
/// reads.
fn value_type_matches_declared(vt: ValueType, decl_type: &str) -> bool {
    match vt {
        ValueType::Text => decl_type == "TEXT",
        ValueType::I64 | ValueType::Bool => decl_type == "INTEGER" || decl_type == "INT",
        ValueType::Blob => decl_type == "BLOB",
    }
}

/// Assert the registry is internally consistent: no physical table name registered under more than
/// one spec. Two specs for one table would share its `(repo_id, table, row_pk)` clock / tombstone /
/// published rows across two streams — cross-scope LWW interference (the first stream to publish a
/// row silences the second, and received writes compete through one clock). Called over the whole
/// [`SYNCABLE_TABLES`] set, complementing the per-spec [`assert_spec_covers_schema`].
pub(crate) fn assert_registry_consistent(registry: &[TableSpec]) -> Result<(), String> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for spec in registry {
        if !seen.insert(spec.name) {
            return Err(format!(
                "table `{}` is registered under more than one spec/scope; its per-row bookkeeping \
                 keys on (repo_id, table, row_pk) and would be shared across streams",
                spec.name
            ));
        }
    }
    Ok(())
}

/// Whether a declared default is a value of the column's declared wire type.
fn default_matches_value_type(default: DefaultValue, value_type: ValueType) -> bool {
    matches!(
        (default, value_type),
        (DefaultValue::Null, _)
            | (DefaultValue::Bool(_), ValueType::Bool)
            | (DefaultValue::I64(_), ValueType::I64)
            | (DefaultValue::Text(_), ValueType::Text)
            | (DefaultValue::Blob(_), ValueType::Blob)
    )
}

/// Whether a declared default equals the column's physical `DEFAULT` clause.
///
/// SQLite reports `dflt_value` as the literal AS WRITTEN, so this compares against the canonical
/// spelling of each literal form. Anything else — an expression, a function call, a differently
/// spelled literal — does NOT match and is reported: a non-literal default is per-device
/// non-deterministic, and two receivers filling the same op from it would produce different rows.
/// `DefaultValue::Null` corresponds to an absent DEFAULT clause (SQLite's own default) as well as
/// an explicit `DEFAULT NULL`.
fn default_matches_sql(declared: DefaultValue, physical: Option<&str>) -> bool {
    let physical = physical.map(str::trim);
    match declared {
        DefaultValue::Null => matches!(physical, None | Some("NULL") | Some("null")),
        DefaultValue::Bool(b) => physical == Some(if b { "1" } else { "0" }),
        DefaultValue::I64(n) => physical.is_some_and(|sql| sql.parse::<i64>() == Ok(n)),
        // SQLite reports a text default with its quotes; compare the unquoted content so an
        // embedded quote (doubled in SQL) still round-trips. It must be exactly ONE literal — see
        // `single_quoted_literal`.
        DefaultValue::Text(text) => physical
            .and_then(schema_facts::single_quoted_literal)
            .is_some_and(|inner| inner == text),
        // A blob default is written as X'..' — compare the hex, case-insensitively. Unlike text,
        // the comparison already cannot admit an expression: a concatenation carries quotes and
        // pipes, and the target is pure hex of a fixed length, so it can never compare equal. The
        // digit check states that rather than leaving it to be re-derived.
        DefaultValue::Blob(bytes) => physical
            .and_then(|sql| {
                let hex = sql.strip_prefix("X'").or_else(|| sql.strip_prefix("x'"))?;
                hex.strip_suffix('\'')
            })
            .is_some_and(|hex| {
                hex.len() == bytes.len() * 2
                    && hex.bytes().all(|b| b.is_ascii_hexdigit())
                    && hex.eq_ignore_ascii_case(&rag_rat_base::hash::hex_lower(bytes))
            }),
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
