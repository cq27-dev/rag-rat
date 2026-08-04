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
/// an auth tier
/// + retention class + flood budget.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TableSpec {
    pub name: &'static str,
    pub scope_id: &'static str,
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
];

const MEMORY_BINDINGS: TableSpec = TableSpec {
    name: "repo_memory_bindings",
    scope_id: "anchors/1",
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
    scope_id: "overlay/1",
    spec_version: 1,
    pk: MEMORY_REALITY_PK,
    columns: MEMORY_REALITY_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

// A compacted memory summary keyed WITH `content_hash`, so a title/body edit is a new row rather
// than an in-place overwrite. Every non-pk column is regenerable model output; nothing is local.
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
    scope_id: "overlay/1",
    spec_version: 1,
    pk: MEMORY_SUMMARIES_PK,
    columns: MEMORY_SUMMARIES_COLUMNS,
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
    scope_id: "distill/1",
    spec_version: 1,
    pk: DISTILL_RECORD_PK,
    columns: DISTILL_RECORD_COLUMNS,
    local_columns: &[],
    repo_column: Some("repo_id"),
};

/// The production table registry. `anchors/1` retains durable memory-binding history in full;
/// `overlay/1` carries regenerable dream output (verdicts, summaries) and `distill/1` the distilled
/// papertrail records — both under a bounded retention budget.
pub(crate) const SYNCABLE_TABLES: &[TableSpec] =
    &[MEMORY_BINDINGS, MEMORY_REALITY, MEMORY_SUMMARIES, DISTILL_RECORD];

/// The per-repo Lens lane metas a scope's applied rows advance — the aggregate enrichment clock
/// plus the scope's specific lane. Returned to the apply-side and refold bump sites, which cannot
/// always name the applied table but always know the stream/entry's `scope_id`. An unknown scope
/// advances nothing. Keep in sync with the scopes in [`SYNCABLE_TABLES`].
pub(crate) fn scope_lens_metas(scope_id: &str) -> &'static [&'static str] {
    match scope_id {
        // anchors/1 and overlay/1 are memory-facing scopes.
        "anchors/1" | "overlay/1" => &[
            rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
            rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
        ],
        "distill/1" => &[
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
    pub scope_id: &'static str,
    pub spec_version: u32,
    pub repo_column: Option<&'static str>,
    /// `(column, value_type)` per identity column, in registry order.
    pub pk: &'static [(&'static str, ValueType)],
    /// `(column, value_type, added)` per synced column, in registry order. `added` is `None` for a
    /// column present since the table's first version.
    pub columns: &'static [(&'static str, ValueType, Option<AddedColumn>)],
}

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
    &[TableGeneration {
        table: "repo_memory_bindings",
        scope_id: "anchors/1",
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
    }],
    // v3: regenerable dream output (verdicts, summaries) replicates on the bounded overlay/1
    // stream. A generation is a whole-registry snapshot, so this repeats the anchors table and
    // adds the two overlay tables, in `SYNCABLE_TABLES` order.
    &[
        TableGeneration {
            table: "repo_memory_bindings",
            scope_id: "anchors/1",
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
        },
        TableGeneration {
            table: "memory_reality",
            scope_id: "overlay/1",
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
        },
        TableGeneration {
            table: "memory_summaries",
            scope_id: "overlay/1",
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
        },
    ],
    // v4: distilled papertrail records replicate on the bounded distill/1 stream. A generation is
    // a whole-registry snapshot, so this repeats v3's three tables and adds the distill
    // parent, in `SYNCABLE_TABLES` order.
    &[
        TableGeneration {
            table: "repo_memory_bindings",
            scope_id: "anchors/1",
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
        },
        TableGeneration {
            table: "memory_reality",
            scope_id: "overlay/1",
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
        },
        TableGeneration {
            table: "memory_summaries",
            scope_id: "overlay/1",
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
        },
        TableGeneration {
            table: "papertrail_distill",
            scope_id: "distill/1",
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
        },
    ],
];

/// Assert a spec classifies EVERY physical column of its table exactly once — as pk, a synced
/// column, or a local column — and names no column the table doesn't have. This is the invariant
/// that stops a new column being silently unclassified: it is either replicated or deliberately
/// local, never neither. Returns a human-readable diff on mismatch.
pub(crate) fn assert_spec_covers_schema(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    let columns = schema_facts::physical_column_info(conn, spec.name)
        .map_err(|err| format!("cannot read columns of `{}`: {err}", spec.name))?;
    let physical: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();

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

    let physical_set: BTreeSet<&str> = physical.iter().copied().collect();
    let unclassified: Vec<&str> = physical_set.difference(&classified).copied().collect();
    let absent: Vec<&str> = classified.difference(&physical_set).copied().collect();
    if !unclassified.is_empty() || !absent.is_empty() {
        return Err(format!(
            "`{}` registry/schema mismatch: physical columns not classified {unclassified:?}; \
             classified columns absent from the table {absent:?}",
            spec.name
        ));
    }

    // A repo scope must be a primary-key column: only then does the applier's repo-identity gate
    // fire on every incoming op. A non-pk repo column would filter the producer but leave ingest
    // unguarded, so a peer could write another repo's row into the shared table.
    if let Some(repo_column) = spec.repo_column {
        match spec.repo_pk_index() {
            None => {
                return Err(format!(
                    "`{}`: repo_column `{repo_column}` must be a primary-key column so the ingest \
                     repo gate applies",
                    spec.name
                ));
            },
            // The applier's repo gate compares the repo pk value to `TypedValue::Text(repo_id)`, so
            // a non-Text scope key never matches — every locally-produced row self-quarantines and
            // the whole table can never sync.
            Some(idx) if spec.pk[idx].value_type != ValueType::Text => {
                return Err(format!(
                    "`{}`: repo_column `{repo_column}` must be ValueType::Text (the repo gate \
                     compares it to the text repo_id)",
                    spec.name
                ));
            },
            Some(_) => {},
        }
    }

    // The whole-row apply/produce SQL builds a `SELECT`/`SET` over the synced columns; a spec with
    // no synced non-key column would emit empty-column SQL (`SELECT  FROM …`) at apply time. A
    // key-only (pure set-membership) table would need a deliberately designed empty-row path
    // (existence-only hash, no-op update) that whole-row LWW does not have — reject it here rather
    // than emit invalid SQL when its first row is applied.
    if spec.columns.is_empty() {
        return Err(format!(
            "`{}`: a syncable table must declare at least one synced non-key column; a key-only \
             table is not supported by the whole-row apply path",
            spec.name
        ));
    }

    // The declared `pk` must be EXACTLY the table's real primary key, in order. The classification
    // check above only matched column NAMES, so a spec could name a non-key column as `pk` (or bury
    // a real key column in `columns`/`local_columns`): row identity would then be non-unique — one
    // op could update or delete several physical rows through `pk_where`, and the per-row clock /
    // tombstone key would not identify a single row. Compare against `PRAGMA table_info`'s pk
    // order.
    let mut pk_cols: Vec<&PhysicalColumn> = columns.iter().filter(|c| c.pk_position > 0).collect();
    pk_cols.sort_by_key(|c| c.pk_position);
    let actual_pk: Vec<&str> = pk_cols.iter().map(|c| c.name.as_str()).collect();
    let declared_pk: Vec<&str> = spec.pk.iter().map(|c| c.name).collect();
    if actual_pk != declared_pk {
        return Err(format!(
            "`{}`: declared pk {declared_pk:?} does not match the table's primary key \
             {actual_pk:?} (identical columns, identical order required)",
            spec.name
        ));
    }

    // Every pk column must be NOT NULL. A rowid table's bare `id TEXT PRIMARY KEY` is NULLABLE
    // (SQLite's historic quirk) — a NULL pk is unaddressable, so `read_all_rows` emits a Null pk
    // that self-apply quarantines, and `produce_and_author` then re-signs that ghost row on
    // every pass. A STRICT table makes its pk NOT NULL (table_info reports it), which is the
    // intended shape.
    for col in &pk_cols {
        if !col.not_null {
            return Err(format!(
                "`{}`: primary-key column `{}` is nullable — declare it NOT NULL (a STRICT table \
                 does this implicitly); a NULL pk is unaddressable and self-quarantines",
                spec.name, col.name
            ));
        }
    }

    // Every pk column must use BINARY equality. A non-binary collation (e.g. `COLLATE NOCASE`)
    // makes SQLite treat values differing only by collation as ONE row in the `WHERE`
    // predicates, but `row_op::row_pk_string` encodes them as DIFFERENT bookkeeping identities
    // — so one physical row would carry two write clocks / published hashes and diverge (or
    // suppress the wrong update).
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

    // Whole-row LWW converges per row INDEPENDENTLY — each row's fate is decided solely by its own
    // write clock. A CROSS-ROW constraint breaks that: the same op set can fold to different states
    // under different arrival orders (two rows racing for one UNIQUE value: whichever loses is
    // quarantined, and WHICH loses depends on order), so peers diverge with no dirty-local edit. A
    // foreign key is the same class (a delete/insert can fail against another row). Reject both
    // until a deterministic cross-row conflict rule exists.
    if schema_facts::table_has_foreign_key(conn, spec.name)
        .map_err(|err| format!("cannot read foreign keys of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: a foreign key makes whole-row LWW order-dependent (an op can fail against \
             another row) — not supported",
            spec.name
        ));
    }
    // The inbound direction is the same hazard: a table REFERENCED by another's FK can have a
    // `Remove` blocked (FK RESTRICT) on a peer that holds a child row but not on one that doesn't →
    // the delete quarantines on one side, applies on the other, and the replicas diverge.
    if schema_facts::table_is_referenced_by_foreign_key(conn, spec.name)
        .map_err(|err| format!("cannot scan foreign keys referencing `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: another table has a foreign key referencing it — a delete can be blocked on \
             one peer but not another, so whole-row LWW diverges — not supported",
            spec.name
        ));
    }
    // A trigger breaks the whole-row fold's assumption that a row write is independent and
    // deterministic: an INSERT/UPDATE/DELETE trigger can adjust the row (or others) from local
    // derived state, so the SAME received op folds to different physical results on two devices,
    // and apply_upsert then publishes each divergent result — the replicas stay divergent.
    if let Some(trigger) = schema_facts::table_trigger(conn, spec.name)
        .map_err(|err| format!("cannot read triggers of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: trigger `{trigger}` can mutate a row from local/derived state, so the same op \
             folds differently across devices — not supported",
            spec.name
        ));
    }
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

    // A synced column's DECLARED default must equal its physical SQL default, exactly (#1002).
    //
    // This is a CONVERGENCE check for the upgrade path, not hygiene. `ALTER TABLE ADD COLUMN`
    // backfills existing rows with the SQL default, while the applier fills a column an older op
    // omits with the DECLARED one. If the two disagree, a device that applied an op BEFORE
    // upgrading and one that applied the same op AFTER hold different rows AT THE SAME CLOCK —
    // silent divergence with no local edit to signal it, and nothing to repair it while the
    // authoring entry is unavailable.
    //
    // KNOW ITS LIMIT. The real invariant is "the migration that introduces the column backfills
    // existing rows with the DECLARED default", and this reads `PRAGMA table_info.dflt_value` — the
    // DEFAULT CLAUSE. Those coincide only for `ALTER TABLE ADD COLUMN … DEFAULT x`. They do NOT
    // coincide for a table REBUILD (`CREATE new; INSERT INTO new SELECT …, <expr> FROM old; DROP;
    // RENAME`), which is a routine migration idiom in this repo: the `SELECT` expression is
    // invisible here, so a rebuild that backfills anything other than the declared default passes
    // this check while violating the invariant. INTRODUCE A SYNCED COLUMN WITH `ADD COLUMN …
    // DEFAULT x`, which satisfies it structurally. A rebuild that computes per-row values is not
    // wrong, but it is new content peers have not seen, and it re-authors the whole table once on
    // every device — budget for that deliberately rather than discovering it.
    //
    // A declared default must also match its column's `ValueType`: the fill goes straight into the
    // row, so a mistyped default would write a value the applier would have quarantined on the
    // wire.
    // An IDENTITY column can never be `added`. The declared default is unreachable for it: an op
    // authored before the key grew carries fewer pk values, and `apply_row_op`'s arity check
    // quarantines it TERMINALLY before projection ever runs — so the evolution the `added` shape
    // promises simply does not exist here, and declaring it would advertise a redemption path that
    // silently drops every older op instead. A changed primary key is a new table identity.
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

    // Every local (never-replicated) column must be nullable or carry a DB default: a remote upsert
    // INSERTs only the pk + synced columns (a local column is re-derived here, not sent), so a NOT
    // NULL local column with no default makes that insert fail and the applier quarantine the op —
    // the row would then be absent on every new peer that never authored it locally.
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

    // The table MUST be STRICT. STRICT enforces the declared column type at write time, so a value
    // the producer read (by its `ValueType`) can never be affinity-coerced to a different stored
    // type. It pins the storage CLASS only, not the value's domain within it — a `Bool` can still
    // hold 2 and a `Text` can still hold invalid UTF-8 — which is why `read_typed` carries those as
    // unreadable rather than relying on the schema. It also makes pk columns NOT NULL, and is the
    // schema convention for every new table regardless.
    // A GENERATED column is invisible to the rest of this lint and to the applier alike:
    // `PRAGMA table_info` omits it, so the exhaustiveness diff never classifies it, and the
    // applier never supplies it. It is not inert, though — its expression can read a synced column,
    // and its own NOT NULL and CHECK constraints then apply to a value derived from whatever the
    // applier filled in. That makes a constraint reachable through it depend on this column
    // transitively, which the probe models by name and therefore cannot see.
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

    if !schema_facts::table_is_strict(conn, spec.name)
        .map_err(|err| format!("cannot read the schema of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: a syncable table must be STRICT (enforced column types keep an applied value \
             from being coerced to a type the producer did not read)",
            spec.name
        ));
    }

    // Each replicated column's declared `ValueType` must match its physical STRICT type, so the
    // value the producer reads round-trips through SQLite unchanged and a peer's op passes the
    // applier's type check. (Local columns are re-derived, never sent, so they are exempt.)
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
                    && hex.eq_ignore_ascii_case(
                        &bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                    )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO_PK: &[ColumnSpec] = &[ColumnSpec::required("id", ValueType::Text)];
    const DEMO_COLUMNS: &[ColumnSpec] = &[
        ColumnSpec::required("title", ValueType::Text),
        ColumnSpec::required("count", ValueType::I64),
    ];
    const DEMO_LOCAL: &[&str] = &["resolved_rowid"];
    const DEMO_SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        spec_version: 1,
        pk: DEMO_PK,
        columns: DEMO_COLUMNS,
        local_columns: DEMO_LOCAL,
        repo_column: None,
    };

    fn demo_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 resolved_rowid INTEGER
             ) STRICT;",
        )
        .unwrap();
        conn
    }

    /// Every production spec must classify its live physical schema exactly — the invariant that
    /// stops a column being added to a registered table without a matching spec edit. Runs against
    /// the real migration ladder, not a synthetic fixture, so a drift between the CREATE TABLE and
    /// the `TableSpec` fails here.
    #[test]
    fn the_production_specs_cover_their_live_schema() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        for spec in SYNCABLE_TABLES {
            assert_spec_covers_schema(&conn, spec).unwrap_or_else(|err| {
                panic!("spec for `{}` does not cover its schema: {err}", spec.name)
            });
        }
        assert_registry_consistent(SYNCABLE_TABLES).unwrap();
    }

    /// Every registered scope must map to a non-empty Lens-lane set. `scope_lens_metas` is a
    /// hand-maintained match on scope-id literals; without this a new scope silently drops Lens
    /// invalidation (the apply/refold bump sites short-circuit cleanly on an empty set, so nothing
    /// errors) — this test forces the match to be extended alongside `SYNCABLE_TABLES`.
    #[test]
    fn every_registered_scope_maps_to_a_lens_lane() {
        for spec in SYNCABLE_TABLES {
            assert!(
                !scope_lens_metas(spec.scope_id).is_empty(),
                "scope `{}` (table `{}`) has no scope_lens_metas entry",
                spec.scope_id,
                spec.name,
            );
        }
        assert!(
            scope_lens_metas("nonexistent/1").is_empty(),
            "an unregistered scope bumps nothing"
        );
    }

    /// The comparable shape of one generation: `(table, spec_version, [(column, in_version,
    /// default)])`, table order preserved.
    /// One table's replicated contract, in a shape both a recorded generation and the live registry
    /// can be reduced to.
    type TableShape = (
        &'static str,                                        // table
        &'static str,                                        // scope_id
        u32,                                                 // spec_version
        Option<&'static str>,                                // repo_column
        Vec<(&'static str, ValueType)>,                      // pk
        Vec<(&'static str, ValueType, Option<AddedColumn>)>, // synced columns
    );
    type Snapshot = Vec<TableShape>;

    fn snapshot_of_generation(generation: &[TableGeneration]) -> Snapshot {
        generation
            .iter()
            .map(|t| {
                (
                    t.table,
                    t.scope_id,
                    t.spec_version,
                    t.repo_column,
                    t.pk.to_vec(),
                    t.columns.to_vec(),
                )
            })
            .collect()
    }

    fn snapshot_of_live_registry() -> Snapshot {
        SYNCABLE_TABLES
            .iter()
            .map(|spec| {
                (
                    spec.name,
                    spec.scope_id,
                    spec.spec_version,
                    spec.repo_column,
                    spec.pk.iter().map(|c| (c.name, c.value_type)).collect(),
                    spec.columns.iter().map(|c| (c.name, c.value_type, c.added)).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_registry_change_cannot_land_without_a_projector_generation() {
        // The coupling that makes a widened registry actually reach parked entries. A refold is
        // owed only when the store's stamp is behind or an entry was parked by an OLDER projector,
        // so registering a table (or widening a spec) without moving the version leaves a store
        // already stamped at that version with entries it will never retry — and redelivery
        // short-circuits on `entry_exists`, so the payload is gone.
        //
        // A pin of the CURRENT registry cannot enforce this: editing the pin is exactly as easy as
        // making the change it is supposed to guard. Requiring the live registry to equal the LAST
        // recorded generation does, because the only way to satisfy it after a change is to append
        // — and the version IS the number of generations.
        assert_eq!(
            usize::try_from(super::super::refold::TABLE_SYNC_PROJECTOR_VERSION).unwrap(),
            PROJECTOR_GENERATIONS.len(),
            "the projector version is the count of recorded generations — append one, do not \
             renumber"
        );
        assert_eq!(
            snapshot_of_generation(PROJECTOR_GENERATIONS.last().expect("at least one generation")),
            snapshot_of_live_registry(),
            "the live registry differs from the newest recorded generation — APPEND a generation \
             (which bumps the projector version), rather than editing the last one"
        );
    }

    #[test]
    fn each_generation_only_extends_the_one_before_it() {
        // EVOLUTION IS ADDITIVE ONLY, enforced across history rather than asserted in prose. A
        // single binary cannot check this — it has no past registry to compare against — which is
        // why the rule is documented as un-lintable. The generation list IS that past, so every
        // consecutive pair can be checked.
        //
        // Both directions of "additive" matter, and they fail differently:
        //
        // - A column that DISAPPEARS (dropped or renamed) strands every stored or received op
        //   naming it on `project_cells`' `UnknownColumn` path — parked forever, since no future
        //   binary reintroduces the name, and redelivery short-circuits on `entry_exists`.
        // - A column that CHANGES its type or introduction tuple diverges silently. A declared
        //   default is the value every receiver synthesizes for an op predating the column, so
        //   changing it — even in step with the table's SQL default, which keeps the schema lint
        //   happy because that lint only ever sees the CURRENT schema — makes a device that folded
        //   an op before the change and one that folded it after hold different rows AT THE SAME
        //   CLOCK, with no local edit to signal it. `in_version` decides WHICH ops a column is
        //   filled for, so moving it retroactively rewrites what every stored op means.
        //
        // An accumulate-and-compare map catches only the second: a key that never reappears is
        // never revisited. Comparing each generation against its predecessor catches both.
        for (index, pair) in PROJECTOR_GENERATIONS.windows(2).enumerate() {
            let (previous, next) = (pair[0], pair[1]);
            let version = index + 2;
            for old in previous {
                let Some(new) = next.iter().find(|t| t.table == old.table) else {
                    panic!(
                        "`{}` disappeared from the registry by generation {version} — ops already \
                         stored for it would park as `TableNotInScope` with nothing to redeem \
                         them. Retiring a table is a deliberate act, not a registry edit.",
                        old.table
                    );
                };
                assert_eq!(
                    old.pk, new.pk,
                    "`{}` changed its primary key by generation {version} — the identity is what \
                     every clock, tombstone and published record is keyed on; a changed identity \
                     means a NEW TABLE, not a new spec version",
                    old.table
                );
                // The scope selects the STREAM, and a projector bump cannot repair a move. Three
                // separate things break, none of them recoverable by replay:
                //   - a retained entry resolves its spec by the scope RECORDED ON THE ENTRY
                //     (`refold::replay_pending_entry`), so every stored op for this table reparks
                //     as `TableNotInScope` forever, whatever the projector version becomes;
                //   - `sync_row_clocks` / tombstones / `sync_published_rows` are keyed `(repo_id,
                //     table_name, row_pk)` with NO stream component, so they carry across silently
                //     and now hold lamports from a stream nobody writes — a locally-authored op
                //     starts from the NEW stream's max and loses its own self-apply;
                //   - the winner lookup keys on `(stream, device, lamport)`, so it lands on some
                //     sibling table's entry (guarded, but only down to "cannot resolve").
                // Moving a table between scopes is a data migration, not a registry edit.
                assert_eq!(
                    old.scope_id, new.scope_id,
                    "`{}` moved from scope `{}` to `{}` by generation {version} — its stream, and \
                     with it every retained entry and row clock, is derived from that scope; a \
                     scope change means a NEW TABLE",
                    old.table, old.scope_id, new.scope_id
                );
                // The repo dimension decides WHICH rows this table replicates and which incoming
                // ops are accepted, while the bookkeeping it writes stays keyed by the caller's
                // repo either way. Dropping it to `None` is the sharp case: `read_all_rows` stops
                // filtering, so every physical row is emitted into EVERY repo's stream, and the
                // applier's repo-identity gate stops rejecting foreign ops — while one physical row
                // now collects an independent clock per repo. That is cross-repo leakage and
                // divergence at once, and no replay repairs it.
                assert_eq!(
                    old.repo_column, new.repo_column,
                    "`{}` changed its repo column from {:?} to {:?} by generation {version} — the \
                     repo dimension selects what replicates and what is accepted, but the clocks \
                     and published records it writes are keyed the same either way; changing it \
                     means a NEW TABLE",
                    old.table, old.repo_column, new.repo_column
                );
                assert!(
                    new.spec_version >= old.spec_version,
                    "`{}`'s spec version went backwards by generation {version}",
                    old.table
                );
                for (column, value_type, added) in old.columns {
                    let carried = new.columns.iter().find(|(name, ..)| name == column);
                    let Some((_, new_type, new_added)) = carried else {
                        panic!(
                            "`{}`.`{column}` disappeared by generation {version} — every op that \
                             names it would park as `UnknownColumn` forever. Removing or renaming \
                             a synced column means a NEW TABLE.",
                            old.table
                        );
                    };
                    assert_eq!(
                        (new_type, new_added),
                        (value_type, added),
                        "`{}`.`{column}` changed its type or introduction tuple by generation \
                         {version} — both are history the projection of older ops depends on",
                        old.table
                    );
                }
                // A column that APPEARS must be fillable for every op the previous generation could
                // have authored, or older→newer replication stops for this table. The schema lint
                // cannot see this: it bounds `in_version` against the CURRENT spec version and
                // skips `required` columns entirely, both of which are judgements about one
                // generation in isolation. Only the predecessor says which versions are still out
                // there.
                for (column, _, added) in new.columns {
                    if old.columns.iter().any(|(name, ..)| name == column) {
                        continue;
                    }
                    let Some(added) = added else {
                        panic!(
                            "`{}`.`{column}` was added in generation {version} as a REQUIRED \
                             column — an op from spec version {} omits it and parks as \
                             `PartialAfterImage` forever. A column added to a live table needs \
                             `ColumnSpec::added` with a declared default.",
                            old.table, old.spec_version
                        );
                    };
                    assert!(
                        added.in_version > old.spec_version,
                        "`{}`.`{column}` was added in generation {version} claiming to exist \
                         since spec version {}, but the previous generation shipped spec version \
                         {} — an op stamped {} omits the column yet is not old enough to have it \
                         filled, so it parks forever. Its introduction version must FOLLOW the \
                         previous spec.",
                        old.table,
                        added.in_version,
                        old.spec_version,
                        old.spec_version
                    );
                    assert!(
                        added.in_version <= new.spec_version,
                        "`{}`.`{column}` claims an introduction version above the spec version \
                         that introduces it ({} > {})",
                        old.table,
                        added.in_version,
                        new.spec_version
                    );
                }
            }
        }
    }

    #[test]
    fn a_spec_that_classifies_every_column_passes() {
        assert!(assert_spec_covers_schema(&demo_conn(), &DEMO_SPEC).is_ok());
    }

    /// A table with a later column, matching what an `ALTER TABLE ADD COLUMN` leaves behind.
    fn widened_conn(default_clause: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later TEXT{default_clause},
                 resolved_rowid INTEGER
             ) STRICT;"
        ))
        .unwrap();
        conn
    }

    macro_rules! widened_spec {
        ($later:expr) => {
            TableSpec {
                name: "t_demo",
                scope_id: "demo/1",
                spec_version: 2,
                pk: DEMO_PK,
                columns: &[
                    ColumnSpec::required("title", ValueType::Text),
                    ColumnSpec::required("count", ValueType::I64),
                    $later,
                ],
                local_columns: DEMO_LOCAL,
                repo_column: None,
            }
        };
    }

    #[test]
    fn a_declared_default_must_equal_the_physical_default() {
        // THE CONVERGENCE GUARANTEE, not hygiene. Adding a column backfills existing rows with the
        // SQL default, while the applier fills a column an older op omits with the DECLARED one. If
        // the two disagree, a device that applied an op before upgrading and one that applied the
        // same op after hold DIFFERENT ROWS AT THE SAME CLOCK — silent divergence, with no local
        // edit to signal it.
        const MATCHING: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("x")));
        assert!(assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &MATCHING).is_ok());

        const DISAGREES: TableSpec = widened_spec!(ColumnSpec::added(
            "later",
            ValueType::Text,
            2,
            DefaultValue::Text("other")
        ));
        let err = assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &DISAGREES)
            .expect_err("a declared default that disagrees with the schema is refused");
        assert!(err.contains("later"), "the error names the column: {err}");

        // An absent DEFAULT clause is SQLite's own NULL, and matches a declared Null.
        const NULL_DEFAULT: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Null));
        assert!(assert_spec_covers_schema(&widened_conn(""), &NULL_DEFAULT).is_ok());
        assert!(assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &NULL_DEFAULT).is_err());
    }

    #[test]
    fn a_non_literal_default_is_refused() {
        // A per-device non-deterministic default would have two receivers fill the same op with
        // different values — divergence by construction. The lint cannot match it, so it refuses.
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("x")));
        assert!(
            assert_spec_covers_schema(&widened_conn(" DEFAULT (unixepoch())"), &SPEC).is_err(),
            "a non-literal default cannot be honored deterministically"
        );
    }

    #[test]
    fn a_declared_default_must_match_the_columns_type() {
        // The fill goes straight into the row, so a mistyped default would write a value the
        // applier would have quarantined had it arrived on the wire.
        const MISTYPED: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::I64(7)));
        assert!(assert_spec_covers_schema(&widened_conn(" DEFAULT 7"), &MISTYPED).is_err());
    }

    #[test]
    fn each_default_type_is_matched_against_its_own_sql_spelling() {
        // One case per `DefaultValue` variant, each asserted BOTH ways. Testing only `Text` left
        // every other arm of `default_matches_sql` free to return `true` unconditionally — i.e. a
        // declared default could disagree with the migration's backfill for any non-text column and
        // the lint that exists to catch exactly that would pass.
        for (declared, value_type, agrees, disagrees) in [
            (DefaultValue::Bool(true), ValueType::Bool, " DEFAULT 1", " DEFAULT 0"),
            (DefaultValue::I64(7), ValueType::I64, " DEFAULT 7", " DEFAULT 8"),
            (DefaultValue::Text("x"), ValueType::Text, " DEFAULT 'x'", " DEFAULT 'y'"),
            (DefaultValue::Blob(&[0xab]), ValueType::Blob, " DEFAULT X'ab'", " DEFAULT X'cd'"),
        ] {
            let sql_type = match value_type {
                ValueType::Bool | ValueType::I64 => "INTEGER",
                ValueType::Text => "TEXT",
                ValueType::Blob => "BLOB",
            };
            let conn = |clause: &str| {
                let conn = Connection::open_in_memory().unwrap();
                conn.execute_batch(&format!(
                    "CREATE TABLE t_demo(
                         id TEXT PRIMARY KEY,
                         title TEXT NOT NULL,
                         count INTEGER NOT NULL,
                         later {sql_type}{clause},
                         resolved_rowid INTEGER
                     ) STRICT;"
                ))
                .unwrap();
                conn
            };
            let spec = TableSpec {
                name: "t_demo",
                scope_id: "demo/1",
                spec_version: 2,
                pk: DEMO_PK,
                // `columns` is `&'static`, and these vary per iteration — leaking a test-sized
                // array is simpler than a const per type.
                columns: Box::leak(Box::new([
                    ColumnSpec::required("title", ValueType::Text),
                    ColumnSpec::required("count", ValueType::I64),
                    ColumnSpec::added("later", value_type, 2, declared),
                ])),
                local_columns: DEMO_LOCAL,
                repo_column: None,
            };
            assert!(
                assert_spec_covers_schema(&conn(agrees), &spec).is_ok(),
                "{declared:?} must match the SQL default `{agrees}`"
            );
            assert!(
                assert_spec_covers_schema(&conn(disagrees), &spec).is_err(),
                "{declared:?} must NOT match the SQL default `{disagrees}`"
            );
        }
    }

    #[test]
    fn a_text_default_must_be_one_literal_not_an_expression() {
        // SQLite strips the outer parentheses from a parenthesized default, so `DEFAULT ('x'||'y')`
        // comes back as `'x'||'y'` — quoted at both ends, but a CONCATENATION. Matching on the
        // outer quotes alone would accept it while SQLite backfills `xy` and the applier
        // synthesizes the raw text, which is the exact divergence the check exists to prevent.
        assert_eq!(schema_facts::single_quoted_literal("'x'"), Some("x".to_string()));
        assert_eq!(schema_facts::single_quoted_literal("'it''s'"), Some("it's".to_string()));
        assert_eq!(schema_facts::single_quoted_literal("''"), Some(String::new()));
        assert_eq!(
            schema_facts::single_quoted_literal("'x'||'y'"),
            None,
            "a concatenation is not a literal"
        );
        assert_eq!(schema_facts::single_quoted_literal("'a'||b"), None);
        assert_eq!(schema_facts::single_quoted_literal("unixepoch()"), None);

        // End to end: the spec declaring exactly what SQLite would evaluate is still refused,
        // because the physical default is not a literal at all.
        const SPEC: TableSpec = widened_spec!(ColumnSpec::added(
            "later",
            ValueType::Text,
            2,
            DefaultValue::Text("x'||'y")
        ));
        assert!(
            assert_spec_covers_schema(&widened_conn(" DEFAULT ('x'||'y')"), &SPEC).is_err(),
            "an expression default cannot be honored deterministically"
        );

        // Blobs need no equivalent case: a concatenation carries quotes and pipes, which cannot
        // compare equal to fixed-length pure hex, so the expression form is excluded already.
        const BLOB: TableSpec = widened_spec!(ColumnSpec::added(
            "later",
            ValueType::Blob,
            2,
            DefaultValue::Blob(&[0xab])
        ));
        assert!(assert_spec_covers_schema(&blob_conn(" DEFAULT X'ab'"), &BLOB).is_ok());
        assert!(
            assert_spec_covers_schema(&blob_conn(" DEFAULT (X'ab'||X'cd')"), &BLOB).is_err(),
            "an expression default is refused whatever it would evaluate to"
        );
    }

    /// `widened_conn`, but the later column is a BLOB.
    fn blob_conn(default_clause: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later BLOB{default_clause},
                 resolved_rowid INTEGER
             ) STRICT;"
        ))
        .unwrap();
        conn
    }

    #[test]
    fn a_default_that_violates_its_own_check_is_refused() {
        // The sharp case, and the one a static reading of the constraint cannot decide: the CHECK
        // names ONLY this column, so it looks self-contained, and every other lint passes — the
        // declared default matches the SQL default, matches the ValueType, and is not Null on a
        // NOT NULL column. It is simply a value the table rejects. Every op older than the column
        // would be filled with it, fail the constraint at INSERT, and be quarantined TERMINALLY.
        //
        // `ALTER TABLE ... ADD COLUMN later INTEGER NOT NULL DEFAULT 0 CHECK(later > 0)` is
        // accepted by SQLite on an empty table, so this schema is reachable, not hypothetical.
        let violates = Connection::open_in_memory().unwrap();
        violates
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0 CHECK(later > 0),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&violates, &SPEC)
            .expect_err("a default its own CHECK rejects cannot be a default");
        assert!(err.contains("REJECTS"), "the error says what is wrong: {err}");

        // The same shape with a default the constraint accepts is fine — the lint DECIDES the
        // question rather than refusing the shape.
        let satisfied = Connection::open_in_memory().unwrap();
        satisfied
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 1 CHECK(later > 0),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        const OK_SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(1)));
        assert!(assert_spec_covers_schema(&satisfied, &OK_SPEC).is_ok());
    }

    #[test]
    fn the_probe_honors_the_columns_collation_and_sqlites_truth_semantics() {
        // Both cases are ones an EVALUATED expression gets wrong, silently and in opposite
        // directions — which is why the probe re-declares the column instead.

        // COLLATION. A bare expression compares BINARY, so `'x' <> 'X'` reads true and the default
        // looks fine; the real column is NOCASE, where it is FALSE and the insert is refused. Under
        // an expression-based probe this schema would be accepted and then quarantine every older
        // op.
        let nocase = Connection::open_in_memory().unwrap();
        nocase
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT COLLATE NOCASE NOT NULL DEFAULT 'x' CHECK(later <> 'X'),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        const COLLATED: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("x")));
        assert!(
            assert_spec_covers_schema(&nocase, &COLLATED).is_err(),
            "the column's own collation decides, not BINARY"
        );

        // TRUTH SEMANTICS. SQLite violates a CHECK only when it evaluates to ZERO, so a REAL 0.5 is
        // satisfied. Reading the expression's result as an integer would fail to decode and refuse
        // a schema that works.
        let real = Connection::open_in_memory().unwrap();
        real.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0
                     CHECK(CASE WHEN later = 0 THEN 0.5 ELSE 1 END),
                 resolved_rowid INTEGER
             ) STRICT;",
        )
        .unwrap();
        const REAL_TRUTHY: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        assert!(
            assert_spec_covers_schema(&real, &REAL_TRUTHY).is_ok(),
            "a non-zero REAL satisfies a CHECK, so the default is fine"
        );
    }

    #[test]
    fn a_check_keywords_case_does_not_hide_it() {
        // The keyword's case is not normalised in `sqlite_master.sql`, and locating the body by
        // trimming the literal text `CHECK` drops a mixed-case constraint SILENTLY — the
        // false-negative direction, where an unsafe default sails through.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 ChEcK(later > 0)
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("a mixed-case CHECK is still a CHECK");
        assert!(err.contains("REJECTS"), "the default is caught violating it: {err}");
    }

    #[test]
    fn a_column_may_be_named_like_a_keyword_or_the_rowid() {
        // A QUOTED head is always a column name, never the keyword; and a table that DECLARES a
        // column called `rowid` shadows the implicit alias, so the name is an ordinary reference
        // rather than per-device state. Both were refused as impossible.
        let quoted = Connection::open_in_memory().unwrap();
        quoted
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     \"check\" INTEGER NOT NULL DEFAULT 0 CHECK(\"check\" >= 0),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        const QUOTED: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: DEMO_PK,
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
                ColumnSpec::added("check", ValueType::I64, 2, DefaultValue::I64(0)),
            ],
            local_columns: DEMO_LOCAL,
            repo_column: None,
        };
        assert!(
            assert_spec_covers_schema(&quoted, &QUOTED).is_ok(),
            "`\"check\"` is a column, not a constraint"
        );

        let shadowing = Connection::open_in_memory().unwrap();
        shadowing
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     rowid INTEGER NOT NULL DEFAULT 0 CHECK(rowid >= 0),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        const SHADOWING: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: DEMO_PK,
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
                ColumnSpec::added("rowid", ValueType::I64, 2, DefaultValue::I64(0)),
            ],
            local_columns: DEMO_LOCAL,
            repo_column: None,
        };
        assert!(
            assert_spec_covers_schema(&shadowing, &SHADOWING).is_ok(),
            "a declared `rowid` column shadows the implicit alias"
        );
    }

    #[test]
    fn a_named_constraint_is_still_a_check() {
        // `CONSTRAINT c CHECK(...)` is the same constraint as a bare `CHECK(...)`. Missing the
        // named form drops it silently — the false-negative direction, where the unsafe default is
        // simply accepted.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CONSTRAINT later_is_positive CHECK(later > 0)
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("a named constraint is still a constraint");
        assert!(err.contains("REJECTS"), "the default is caught violating it: {err}");
    }

    #[test]
    fn a_double_quoted_column_reference_is_not_read_as_a_string() {
        // SQLite's double-quoted-string misfeature: `"later"` falls back to a STRING LITERAL when
        // no such column is in scope. Asking "does this resolve WITHOUT the column?" first would
        // therefore see it resolve and call the constraint irrelevant — accepting an unsafe
        // default. Asking "does it resolve with ONLY the column?" first settles it correctly.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CHECK(\"later\" > 10)
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("a quoted reference to the column is a reference, not a string");
        assert!(err.contains("REJECTS"), "the default is caught violating it: {err}");
    }

    #[test]
    fn a_check_whose_result_can_differ_between_devices_is_refused() {
        // Self-contained and satisfiable, yet worthless as a guarantee: the identical replicated op
        // can be accepted on one peer and quarantined on another, which is divergence with no local
        // edit to signal it.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CHECK(later >= 0 AND random() <> 0)
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("a non-deterministic constraint cannot be relied on");
        assert!(err.contains("random"), "the error names the function: {err}");

        // A QUOTED function name is still a call: matching only bare words would let it past.
        let quoted_fn = Connection::open_in_memory().unwrap();
        quoted_fn
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0,
                     resolved_rowid INTEGER,
                     CHECK(later >= 0 AND \"random\"() <> 0)
                 ) STRICT;",
            )
            .unwrap();
        let err = assert_spec_covers_schema(&quoted_fn, &SPEC)
            .expect_err("a quoted function name is still a call");
        assert!(err.contains("random"), "the error names the function: {err}");

        // A date/time call is deterministic in its INPUTS; only an environment-dependent argument
        // reads the device. Refusing the whole family would block a legitimate schema.
        let dated = Connection::open_in_memory().unwrap();
        dated
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01'
                         CHECK(date(later) >= date('2000-01-01')),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        const DATED: TableSpec = widened_spec!(ColumnSpec::added(
            "later",
            ValueType::Text,
            2,
            DefaultValue::Text("2020-01-01")
        ));
        assert!(
            assert_spec_covers_schema(&dated, &DATED).is_ok(),
            "date() over the column's own value is deterministic"
        );

        // ...but the same function reading the clock is refused.
        let clock = Connection::open_in_memory().unwrap();
        clock
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01'
                         CHECK(date(later) <= date('now')),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        assert!(
            assert_spec_covers_schema(&clock, &DATED).is_err(),
            "date('now') reads the device clock"
        );

        // The environment literal must be an ARGUMENT of the date/time call, not merely present
        // somewhere in the constraint: here `'now'` is a value the column is compared against.
        let unrelated = Connection::open_in_memory().unwrap();
        unrelated
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01'
                         CHECK(date(later) IS NOT NULL AND later <> 'now'),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        assert!(
            assert_spec_covers_schema(&unrelated, &DATED).is_ok(),
            "a `'now'` outside the call's arguments does not make it clock-reading"
        );

        // A comment inside the call's arguments is not an argument. Scanning the raw text would
        // read it as one.
        let commented = Connection::open_in_memory().unwrap();
        commented
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01'
                         CHECK(date(later /* not 'now' */) IS NOT NULL),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        assert!(
            assert_spec_covers_schema(&commented, &DATED).is_ok(),
            "a commented-out `'now'` is not an argument"
        );

        // A build-varying function is refused even though SQLite flags it DETERMINISTIC: that flag
        // means "same answer for the same arguments within this build", which is not the question.
        // `fts5_source_id()` satisfies it and still returns a different string on a peer compiled
        // against another SQLite — which is why the rule is an allowlist rather than a denylist.
        let build_varying = Connection::open_in_memory().unwrap();
        build_varying
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0
                         CHECK(later = 0 AND fts5_source_id() IS NOT NULL),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        const ZERO: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&build_varying, &ZERO)
            .expect_err("a build-varying function is not a shared guarantee");
        assert!(err.contains("fts5_source_id"), "the error names it: {err}");

        // A DETERMINISTIC builtin is fine — the rule is about per-device variation, not calls.
        let ok = Connection::open_in_memory().unwrap();
        ok.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later TEXT NOT NULL DEFAULT 'ab' CHECK(length(later) = 2),
                 resolved_rowid INTEGER
             ) STRICT;",
        )
        .unwrap();
        const DETERMINISTIC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("ab")));
        assert!(assert_spec_covers_schema(&ok, &DETERMINISTIC).is_ok());
    }

    #[test]
    fn a_sibling_named_after_a_keyword_does_not_break_the_probe() {
        // The probe's "does this resolve without the column" world lists the OTHER columns by name.
        // A column may legally be named after a keyword, and splicing such a name in bare fails
        // that CREATE on syntax — refusing a valid schema for a reason unrelated to the
        // constraint.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 \"order\" INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CHECK(title <> '')
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: DEMO_PK,
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("order", ValueType::I64),
                ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)),
            ],
            local_columns: DEMO_LOCAL,
            repo_column: None,
        };
        assert!(
            assert_spec_covers_schema(&conn, &SPEC).is_ok(),
            "a sibling's name is quoted into the probe, so a keyword name is harmless"
        );
    }

    #[test]
    fn a_check_sharing_a_segment_with_another_constraint_is_still_found() {
        // SQLite does not require a comma between table constraints, so a CHECK can share a segment
        // with a PRIMARY KEY. Dispatching on the segment's HEAD dropped that check entirely — the
        // verdict turned on where the author happened to put a comma.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT NOT NULL,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 PRIMARY KEY(id) CHECK(later >= count)
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("a CHECK is a CHECK wherever it is written");
        assert!(
            err.contains("read something other than that column"),
            "refused as cross-column: {err}"
        );
    }

    #[test]
    fn a_trailing_line_comment_does_not_break_the_probe() {
        // A declaration is spliced VERBATIM into the probe's DDL, so one ending in a `--` comment
        // would comment out everything after it on that line and the CREATE would fail as
        // incomplete input — refusing a perfectly ordinary table.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 resolved_rowid INTEGER,
                 later INTEGER NOT NULL DEFAULT 0 CHECK(later >= 0) -- how many
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        assert!(
            assert_spec_covers_schema(&conn, &SPEC).is_ok(),
            "a comment on the declaration is not a defect in the table"
        );
    }

    #[test]
    fn a_generated_column_is_refused() {
        // A generated column is absent from `PRAGMA table_info`, so the exhaustiveness diff cannot
        // classify it as synced or local, and the applier never supplies it — yet it is not inert.
        // Its expression reads synced columns, so its own NOT NULL and CHECK constraints apply to a
        // value derived from whatever the applier filled in. Here `CHECK(derived > count)` fails
        // for a valid older op carrying `count = 5`, and the probe — which models the other columns
        // by NAME — cannot see the dependency that runs through `derived`. Refuse the shape.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 derived INTEGER GENERATED ALWAYS AS (later) VIRTUAL CHECK(derived > count)
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: DEMO_PK,
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
                ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)),
            ],
            local_columns: DEMO_LOCAL,
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("a generated column cannot be classified or modelled");
        assert!(
            err.contains("GENERATED") && err.contains("derived"),
            "refused for being generated, not incidentally: {err}"
        );
    }

    #[test]
    fn an_inline_check_on_a_sibling_column_is_still_a_constraint() {
        // An inline CHECK constrains the ROW, not the column it happens to be attached to. Reading
        // only table-level constraints plus the added column's own declaration therefore misses one
        // written on a SIBLING — and a valid older op carrying `count = 5` would be filled with
        // `later = 0`, rejected by SQLite, and quarantined terminally.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL CHECK(later >= count),
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("where the constraint is WRITTEN does not change what it constrains");
        assert!(
            err.contains("read something other than that column"),
            "refused as cross-column: {err}"
        );

        // A sibling's inline CHECK that does NOT read this column is still irrelevant to it.
        let unrelated = Connection::open_in_memory().unwrap();
        unrelated
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL CHECK(count >= 0),
                     later INTEGER NOT NULL DEFAULT 0,
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        assert!(
            assert_spec_covers_schema(&unrelated, &SPEC).is_ok(),
            "a sibling constraint that never reads this column does not concern it"
        );
    }

    #[test]
    fn a_clock_reading_date_call_is_refused_by_the_probe() {
        // Omitting the time value IS `'now'`: `date()` means `date('now')`, and `strftime('%s')` —
        // one argument, the format — reads the clock too.
        //
        // Nothing in this lint decides that. SQLite refuses a clock-reading date/time call inside a
        // CHECK at INSERT, which is exactly what the probe performs, so the verdict comes back as
        // not-self-contained on its own. That is why the date/time family sits on the allowlist:
        // SQLite draws the line more precisely than this lint could — it also catches a
        // `'localtime'` modifier, and it correctly permits `date(column, 'now')`, which is not a
        // clock read. This test pins that reliance so a future change cannot quietly lose it.
        let table = |constraint: &str| {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01' CHECK({constraint}),
                     resolved_rowid INTEGER
                 ) STRICT;"
            ))
            .unwrap();
            conn
        };
        const SPEC: TableSpec = widened_spec!(ColumnSpec::added(
            "later",
            ValueType::Text,
            2,
            DefaultValue::Text("2020-01-01")
        ));

        // The WHOLE boundary is pinned, not just the two forms that prompted it: this lint now
        // depends on where SQLite draws the line, so a change in that line must fail here rather
        // than silently widen or narrow what the lint accepts.
        for constraint in [
            "later <= date()",                   // time value omitted
            "later <= strftime('%s')",           // format only, time value omitted
            "later <= date('now')",              // the clock, explicitly
            "later <= date(later, 'localtime')", // the device's timezone
        ] {
            let err = assert_spec_covers_schema(&table(constraint), &SPEC)
                .expect_err("a clock-reading call cannot be a shared guarantee");
            assert!(
                err.contains("non-deterministic use"),
                "refused because SQLite itself will not evaluate it: {err}"
            );
        }

        // ...and the forms SQLite permits stay permitted. `date(column, 'now')` is the sharp one:
        // `'now'` in a MODIFIER position is not a clock read, and a hand-rolled rule that scanned
        // for the literal would wrongly refuse it.
        for constraint in [
            "date(later) >= date('2000-01-01')",
            "strftime('%Y', later) >= '2000'",
            "date(later, '+1 day') > date('2000-01-01')",
            "date(later, 'now') > date('2000-01-01')",
        ] {
            assert!(
                assert_spec_covers_schema(&table(constraint), &SPEC).is_ok(),
                "deterministic in its inputs, so it is a shared guarantee: {constraint}"
            );
        }
    }

    #[test]
    fn a_connection_configurable_operator_is_refused() {
        // `PRAGMA case_sensitive_like` changes both `a LIKE b` and `like(a, b)`, so the constraint
        // means different things on two peers. The OPERATOR form is the one a call-shaped scan
        // cannot see.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later TEXT NOT NULL DEFAULT 'ab' CHECK(later NOT LIKE 'Z%'),
                 resolved_rowid INTEGER
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("ab")));
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("LIKE is connection state, not a property of its operands");
        assert!(err.contains("like"), "the error names it: {err}");
    }

    #[test]
    fn per_device_references_are_refused_in_every_form_they_take() {
        // The call-shaped scan and the bare-word rowid scan each missed a form. Neither gap is
        // visible from inside the other, and both are silent: the probe passes at ITS values, then
        // the same op is quarantined on a peer whose clock or insertion order differs.
        let table = |constraint: &str| {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 2 CHECK({constraint}),
                     resolved_rowid INTEGER
                 ) STRICT;"
            ))
            .unwrap();
            conn
        };
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(2)));

        // A QUOTED rowid alias resolves to the implicit rowid exactly as a bare one does. The
        // default passes at the probe's rowid 1 and fails on a peer whose row lands at rowid 2.
        let quoted = table("later <> \"rowid\"");
        let err = assert_spec_covers_schema(&quoted, &SPEC)
            .expect_err("a quoted rowid alias is still the rowid");
        assert!(err.contains("assigned per device"), "refused for the rowid rule: {err}");

        // A clock KEYWORD takes no parentheses, so a scan shaped around calls never reaches it.
        let clock = table("later > 0 AND CURRENT_TIMESTAMP IS NOT NULL");
        let err =
            assert_spec_covers_schema(&clock, &SPEC).expect_err("a clock keyword reads the device");
        assert!(err.contains("current_timestamp"), "the error names what reads the device: {err}");
    }

    #[test]
    fn a_check_reading_the_implicit_rowid_is_refused() {
        // The rowid resolves in ANY rowid table — including the probe — so a constraint reading it
        // looks self-contained and passes. It is also per-device, assigned by insertion order, so
        // the same op lands at a different rowid on every peer.
        //
        // The body is deliberately one the default SATISFIES at the probe's rowid (1): a body that
        // merely fails there would be reported as `Violated` and the test would pass off SQLite's
        // own error text without ever exercising the rowid rule — which is how the first version of
        // this test was vacuous.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0 CHECK(later = 0 AND rowid < 10),
                 resolved_rowid INTEGER
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&conn, &SPEC)
            .expect_err("a rowid-dependent constraint cannot be proven safe");
        assert!(
            err.contains("assigned per device"),
            "refused for the rowid rule, not incidentally: {err}"
        );
    }

    #[test]
    fn an_unrelated_constraint_is_not_dragged_into_the_probe() {
        // Which constraints involve the column is SQLite's answer, not a token match. `text` here
        // is a TYPE NAME inside a CAST, and the constraint does not read the `text` column at all —
        // a token match would import it into the probe, where `other` fails to resolve and the
        // schema is refused for a constraint that never concerned it.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 text TEXT NOT NULL DEFAULT '',
                 CHECK(CAST(title AS text) <> 'zz')
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: DEMO_PK,
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
                ColumnSpec::added("text", ValueType::Text, 2, DefaultValue::Text("")),
            ],
            local_columns: &[],
            repo_column: None,
        };
        assert!(
            assert_spec_covers_schema(&conn, &SPEC).is_ok(),
            "a constraint that does not read the column must not decide its fate"
        );
    }

    #[test]
    fn a_table_qualified_self_reference_is_self_contained() {
        // `t_demo.later` names this very column. The probe rebuilds the column under the real
        // table's NAME so the qualification still resolves; otherwise a legal, genuinely
        // self-contained constraint would read as cross-column.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CHECK(t_demo.later >= 0)
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        assert!(
            assert_spec_covers_schema(&conn, &SPEC).is_ok(),
            "a qualified reference to the column itself is self-contained"
        );
    }

    #[test]
    fn a_function_call_is_not_a_cross_column_reference() {
        // A built-in whose name matches a column of the table is not a reference to that column.
        // Token comparison alone cannot tell them apart; evaluation can, because SQLite resolves
        // `length(...)` as a function regardless of what the table's columns are called.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 length INTEGER NOT NULL DEFAULT 0,
                 later INTEGER NOT NULL DEFAULT 0 CHECK(later <= length('abc')),
                 resolved_rowid INTEGER
             ) STRICT;",
        )
        .unwrap();
        const SPEC: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: DEMO_PK,
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
                ColumnSpec::required("length", ValueType::I64),
                ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)),
            ],
            local_columns: DEMO_LOCAL,
            repo_column: None,
        };
        assert!(
            assert_spec_covers_schema(&conn, &SPEC).is_ok(),
            "`length(...)` is a call, not a reference to the `length` column"
        );
    }

    #[test]
    fn an_added_column_may_not_participate_in_a_cross_column_check() {
        // The default and the other columns come from different places — the applier synthesizes
        // this one and takes the rest from the OP — so a constraint relating them can fail for a
        // perfectly valid older op and quarantine it TERMINALLY, ending older→newer replication.
        let cross = Connection::open_in_memory().unwrap();
        cross
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0 CHECK(later >= count),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        const CROSS: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
        let err = assert_spec_covers_schema(&cross, &CROSS)
            .expect_err("a default cannot satisfy a constraint that depends on the op's values");
        assert!(err.contains("later") && err.contains("count"), "names both sides: {err}");

        // A SELF-CONTAINED check is fine: it constrains only the synthesized value, statically.
        let alone = Connection::open_in_memory().unwrap();
        alone
            .execute_batch(
                "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0 CHECK(later IN (0, 1)),
                     resolved_rowid INTEGER
                 ) STRICT;",
            )
            .unwrap();
        assert!(
            assert_spec_covers_schema(&alone, &CROSS).is_ok(),
            "a check on the added column alone is decidable and allowed"
        );
    }

    #[test]
    fn an_identity_column_may_not_declare_an_introduction_version() {
        // `added` promises a redemption path that does not exist for a key: an op authored before
        // the key grew carries fewer pk values, so `apply_row_op`'s arity check quarantines it
        // TERMINALLY before any default could apply. Declaring it would advertise older→newer
        // replication while silently dropping every older op.
        const GROWN_KEY: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: &[
                ColumnSpec::required("id", ValueType::Text),
                ColumnSpec::added("shard", ValueType::Text, 2, DefaultValue::Text("d")),
            ],
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
            ],
            local_columns: DEMO_LOCAL,
            repo_column: None,
        };
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT NOT NULL,
                 shard TEXT NOT NULL DEFAULT 'd',
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 resolved_rowid INTEGER,
                 PRIMARY KEY(id, shard)
             ) STRICT;",
        )
        .unwrap();
        let err = assert_spec_covers_schema(&conn, &GROWN_KEY)
            .expect_err("a primary key cannot grow within a table's life");
        assert!(err.contains("shard") && err.contains("NEW TABLE"), "names the remedy: {err}");
    }

    #[test]
    fn a_not_null_column_may_not_declare_a_null_default() {
        // Filling an older op from a Null default on a NOT NULL column fails the constraint at
        // INSERT, and the applier quarantines the op TERMINALLY — older→newer replication for the
        // table would be dead with nothing to redeem it. The SQL-default check cannot catch this:
        // a NOT NULL column with no DEFAULT clause reads as an absent physical default, which a
        // declared Null matches.
        const NULL_ON_NOT_NULL: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Null));
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later TEXT NOT NULL,
                 resolved_rowid INTEGER
             ) STRICT;",
        )
        .unwrap();
        let err = assert_spec_covers_schema(&conn, &NULL_ON_NOT_NULL)
            .expect_err("a Null default on a NOT NULL column is refused");
        assert!(err.contains("later") && err.contains("NOT NULL"), "the error is specific: {err}");

        // The same declaration is fine once the column is nullable — the fill can succeed.
        assert!(assert_spec_covers_schema(&widened_conn(""), &NULL_ON_NOT_NULL).is_ok());
    }

    #[test]
    fn an_added_columns_version_must_sit_inside_the_specs_history() {
        // `1` is the first version, so a column "added" there was present from the start and is
        // `required`; a version above the spec's own names a column this binary carries but does
        // not announce — a forgotten bump, which puts the fill window wrong in both directions.
        const AT_VERSION_1: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 1, DefaultValue::Text("x")));
        let err = assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &AT_VERSION_1)
            .expect_err("a column added in version 1 is `required`, not `added`");
        assert!(err.contains("later"), "the error names the column: {err}");

        // `widened_spec!` is spec_version 2, so 3 is beyond this spec's own history.
        const BEYOND_THE_SPEC: TableSpec =
            widened_spec!(ColumnSpec::added("later", ValueType::Text, 3, DefaultValue::Text("x")));
        assert!(
            assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &BEYOND_THE_SPEC).is_err(),
            "a column introduced beyond the spec's own version is a forgotten bump"
        );
    }

    #[test]
    fn a_physical_column_missing_from_the_spec_fails() {
        // The table has an extra `note` column the spec forgot to classify.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY, title TEXT NOT NULL, count INTEGER NOT NULL,
                 resolved_rowid INTEGER, note TEXT
             ) STRICT;",
        )
        .unwrap();
        let err = assert_spec_covers_schema(&conn, &DEMO_SPEC).unwrap_err();
        assert!(err.contains("note"), "the unclassified column is named: {err}");
    }

    #[test]
    fn a_spec_column_absent_from_the_table_fails() {
        // The spec names `count` but the table doesn't have it.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT NOT NULL, resolved_rowid \
             INTEGER) STRICT;",
        )
        .unwrap();
        let err = assert_spec_covers_schema(&conn, &DEMO_SPEC).unwrap_err();
        assert!(err.contains("count"), "the phantom column is named: {err}");
    }

    #[test]
    fn a_column_classified_twice_fails() {
        const DOUBLED: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 1,
            pk: DEMO_PK,
            // `title` is both a synced column and (wrongly) a local column.
            columns: DEMO_COLUMNS,
            local_columns: &["resolved_rowid", "title"],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&demo_conn(), &DOUBLED).unwrap_err();
        assert!(err.contains("title"), "the doubly-classified column is named: {err}");
    }

    #[test]
    fn a_repo_column_that_is_not_a_primary_key_fails() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t_x(id TEXT PRIMARY KEY, repo_id TEXT NOT NULL) STRICT;")
            .unwrap();
        const BAD: TableSpec = TableSpec {
            name: "t_x",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("repo_id", ValueType::Text)],
            local_columns: &[],
            repo_column: Some("repo_id"), /* a synced column, not a pk — the ingest gate would
                                           * miss it */
        };
        let err = assert_spec_covers_schema(&conn, &BAD).unwrap_err();
        assert!(err.contains("primary-key"), "a non-pk repo column is rejected: {err}");
    }

    #[test]
    fn a_key_only_table_with_no_synced_columns_fails() {
        // A table whose entire row is its composite pk has no synced non-key column; the whole-row
        // apply path would build empty-column SQL, so the lint rejects the shape up front.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_members(group_id TEXT NOT NULL, member_id TEXT NOT NULL, PRIMARY \
             KEY(group_id, member_id)) STRICT;",
        )
        .unwrap();
        const KEY_ONLY: TableSpec = TableSpec {
            name: "t_members",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[
                ColumnSpec::required("group_id", ValueType::Text),
                ColumnSpec::required("member_id", ValueType::Text),
            ],
            columns: &[],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &KEY_ONLY).unwrap_err();
        assert!(
            err.contains("at least one synced non-key column"),
            "a key-only table is rejected: {err}"
        );
    }

    #[test]
    fn a_declared_pk_that_does_not_match_the_schema_fails() {
        // The table's real primary key is (a, b), but the spec declares only `a` as pk and buries
        // the real key column `b` in `columns` — every name is still classified once, so only the
        // pk-vs-schema check catches the non-unique identity.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_pk(a TEXT NOT NULL, b TEXT NOT NULL, v TEXT, PRIMARY KEY(a, b)) \
             STRICT;",
        )
        .unwrap();
        const WRONG_PK: TableSpec = TableSpec {
            name: "t_pk",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("a", ValueType::Text)],
            columns: &[
                ColumnSpec::required("b", ValueType::Text),
                ColumnSpec::required("v", ValueType::Text),
            ],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &WRONG_PK).unwrap_err();
        assert!(
            err.contains("does not match the table's primary key"),
            "the pk mismatch is named: {err}"
        );
    }

    #[test]
    fn a_not_null_local_column_without_a_default_fails() {
        // A remote insert supplies only pk + synced columns, so a NOT NULL local column with no
        // default would make that insert fail — the lint rejects the shape up front.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_nn_local(id TEXT PRIMARY KEY, syn TEXT, loc TEXT NOT NULL) STRICT;",
        )
        .unwrap();
        const NN_LOCAL: TableSpec = TableSpec {
            name: "t_nn_local",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("syn", ValueType::Text)],
            local_columns: &["loc"],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &NN_LOCAL).unwrap_err();
        assert!(
            err.contains("NOT NULL without a default"),
            "the required local column is named: {err}"
        );
    }

    #[test]
    fn a_not_null_local_column_with_a_default_passes() {
        // The same shape but with a DB default on the local column is fine — a remote insert leaves
        // it to the default, and the local index re-derives it.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_def_local(id TEXT PRIMARY KEY, syn TEXT, loc INTEGER NOT NULL DEFAULT \
             0) STRICT;",
        )
        .unwrap();
        const DEF_LOCAL: TableSpec = TableSpec {
            name: "t_def_local",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("syn", ValueType::Text)],
            local_columns: &["loc"],
            repo_column: None,
        };
        assert!(assert_spec_covers_schema(&conn, &DEF_LOCAL).is_ok());
    }

    #[test]
    fn a_nullable_primary_key_fails() {
        // A non-STRICT rowid table's bare `id TEXT PRIMARY KEY` is NULLABLE (SQLite quirk); a NULL
        // pk self-quarantines and gets re-signed every producer pass.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t_np(id TEXT PRIMARY KEY, v TEXT);").unwrap();
        const NP: TableSpec = TableSpec {
            name: "t_np",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &NP).unwrap_err();
        assert!(err.contains("nullable"), "a nullable pk is rejected: {err}");
    }

    #[test]
    fn a_foreign_key_fails() {
        // A foreign key is a cross-row constraint: a delete/insert can fail against another row, so
        // whole-row LWW becomes order-dependent (peers diverge).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE parent(id TEXT NOT NULL PRIMARY KEY, v TEXT) STRICT;
             CREATE TABLE t_fk(id TEXT NOT NULL PRIMARY KEY, v TEXT, p TEXT REFERENCES parent(id)) \
             STRICT;",
        )
        .unwrap();
        const FK: TableSpec = TableSpec {
            name: "t_fk",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[
                ColumnSpec::required("v", ValueType::Text),
                ColumnSpec::required("p", ValueType::Text),
            ],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &FK).unwrap_err();
        assert!(err.contains("foreign key"), "an FK table is rejected: {err}");
    }

    #[test]
    fn a_non_pk_unique_index_fails() {
        // A UNIQUE constraint on a non-pk column is a cross-row constraint: two rows racing for one
        // value fold differently by arrival order, so peers diverge.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_u(id TEXT NOT NULL PRIMARY KEY, email TEXT UNIQUE) STRICT;",
        )
        .unwrap();
        const U: TableSpec = TableSpec {
            name: "t_u",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("email", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &U).unwrap_err();
        assert!(err.contains("UNIQUE index"), "a non-pk UNIQUE table is rejected: {err}");
    }

    #[test]
    fn a_non_strict_table_fails() {
        // Otherwise valid (explicit NOT NULL pk, no FK/UNIQUE, matching types) but NOT STRICT — an
        // applied value could be affinity-coerced and wedge the post-write hash read-back.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t_ns(id TEXT NOT NULL PRIMARY KEY, v TEXT);").unwrap();
        const NS: TableSpec = TableSpec {
            name: "t_ns",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &NS).unwrap_err();
        assert!(err.contains("must be STRICT"), "a non-STRICT table is rejected: {err}");
    }

    #[test]
    fn a_declared_value_type_that_disagrees_with_the_physical_type_fails() {
        // `n` is physically INTEGER but the spec declares it Text.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t_tm(id TEXT NOT NULL PRIMARY KEY, n INTEGER) STRICT;")
            .unwrap();
        const TM: TableSpec = TableSpec {
            name: "t_tm",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("n", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &TM).unwrap_err();
        assert!(
            err.contains("physical type"),
            "a ValueType/physical-type mismatch is rejected: {err}"
        );
    }

    #[test]
    fn a_table_registered_under_two_scopes_fails() {
        const A: TableSpec = TableSpec {
            name: "t_dup",
            scope_id: "scope-a/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        const B: TableSpec = TableSpec {
            name: "t_dup",
            scope_id: "scope-b/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_registry_consistent(&[A, B]).unwrap_err();
        assert!(err.contains("more than one spec"), "a table under two scopes is rejected: {err}");
        assert!(assert_registry_consistent(&[A]).is_ok(), "a single registration is fine");
    }

    #[test]
    fn a_non_text_repo_column_fails() {
        // The applier's repo gate compares the repo pk value to a TEXT repo_id, so a non-Text repo
        // scope key never matches → every row self-quarantines.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_ri(rid INTEGER NOT NULL, id TEXT NOT NULL, v TEXT, PRIMARY KEY(rid, \
             id)) STRICT;",
        )
        .unwrap();
        const RI: TableSpec = TableSpec {
            name: "t_ri",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[
                ColumnSpec::required("rid", ValueType::I64),
                ColumnSpec::required("id", ValueType::Text),
            ],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: Some("rid"),
        };
        let err = assert_spec_covers_schema(&conn, &RI).unwrap_err();
        assert!(
            err.contains("must be ValueType::Text"),
            "a non-Text repo column is rejected: {err}"
        );
    }

    #[test]
    fn a_table_referenced_by_a_foreign_key_fails() {
        // No outbound FK, but a child table references it — the same cross-row delete hazard.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_ref(id TEXT NOT NULL PRIMARY KEY, v TEXT) STRICT;
             CREATE TABLE kid(id TEXT NOT NULL PRIMARY KEY, r TEXT REFERENCES t_ref(id)) STRICT;",
        )
        .unwrap();
        const REF: TableSpec = TableSpec {
            name: "t_ref",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &REF).unwrap_err();
        assert!(err.contains("referencing it"), "an inbound-FK table is rejected: {err}");
    }

    #[test]
    fn a_non_binary_pk_collation_fails() {
        // A `COLLATE NOCASE` pk: SQLite treats "a"/"A" as one row, but the row-clock encoding is
        // byte-exact → split bookkeeping for one physical row.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_ci(id TEXT COLLATE NOCASE NOT NULL PRIMARY KEY, v TEXT) STRICT;",
        )
        .unwrap();
        const CI: TableSpec = TableSpec {
            name: "t_ci",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &CI).unwrap_err();
        assert!(err.contains("non-BINARY collation"), "a NOCASE pk is rejected: {err}");
    }

    #[test]
    fn a_table_with_a_trigger_fails() {
        // An AFTER INSERT trigger that mutates a synced cell makes the same received op fold to a
        // different physical row across devices.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t_trig(id TEXT NOT NULL PRIMARY KEY, v TEXT, n INTEGER NOT NULL DEFAULT \
             0) STRICT;
             CREATE TRIGGER t_trig_ai AFTER INSERT ON t_trig
                 BEGIN UPDATE t_trig SET n = n + 1 WHERE id = NEW.id; END;",
        )
        .unwrap();
        const TRIG: TableSpec = TableSpec {
            name: "t_trig",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[
                ColumnSpec::required("v", ValueType::Text),
                ColumnSpec::required("n", ValueType::I64),
            ],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &TRIG).unwrap_err();
        assert!(err.contains("trigger"), "a table with a trigger is rejected: {err}");
    }

    #[test]
    fn every_registered_table_is_covered_by_the_live_schema() {
        // Empty today; the moment a per-scope milestone registers a real table, this pins that its
        // spec classifies every physical column of the migrated schema.
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        assert_registry_consistent(SYNCABLE_TABLES).expect("the registry has a duplicate table");
        for spec in SYNCABLE_TABLES {
            assert_spec_covers_schema(&conn, spec).unwrap_or_else(|err| {
                panic!("registered table `{}` is not covered: {err}", spec.name)
            });
        }
    }
}
