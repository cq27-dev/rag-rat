mod predicates;
#[cfg(test)]
mod tests;
mod traverse;
use std::collections::BTreeSet;

pub use predicates::*;
use rusqlite::{Connection, params_from_iter};
use serde::Serialize;
pub use traverse::*;

// `dispatches` (#200) and `uses_operator` ride with the call kinds so graph traversal surfaces the
// synthesized handler hop and an operator declaration alongside its callable implementation. The
// internal `dispatch_construct`/`dispatch_handle` FACT kinds are deliberately absent from every
// set.
const CALL_EDGE_KINDS: &[&str] = &["calls_name", "constructs", "dispatches", "uses_operator"];
const MACRO_EDGE_KINDS: &[&str] = &["uses_macro"];
const REFERENCE_EDGE_KINDS: &[&str] =
    &["references_type", "uses_precedence_group", "imports", "exports", "contains", "implements"];
const OPTIONAL_EDGE_KINDS: &[&str] = &[
    "calls_name",
    "constructs",
    "dispatches",
    "uses_operator",
    "uses_macro",
    "references_type",
    "uses_precedence_group",
    "imports",
    "exports",
    "contains",
    "implements",
];

#[derive(Debug, Clone, Default)]
pub struct GraphTraversalOptions {
    pub include_references: bool,
    pub include_unresolved: bool,
    pub include_macros: bool,
    pub include_common_methods: bool,
    pub edge_kinds: Option<Vec<String>>,
    pub resolution_mode: GraphResolutionMode,
    pub symbol_id: Option<i64>,
    pub logical_symbol_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct GraphTraversalReport {
    pub query: GraphTraversalQuery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_symbol: Option<LogicalSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<LogicalSymbolVariant>,
    pub summary: GraphTraversalSummary,
    pub coverage: GraphCoverage,
    pub results: Vec<GraphHop>,
}

#[derive(Debug, Serialize)]
pub struct GraphTraversalQuery {
    pub tool: String,
    // Internal rowid — never serialized (reindex-churned, #149); the handle is logical_symbol_id.
    #[serde(skip_serializing)]
    pub symbol_id: Option<i64>,
    // Opaque `sym_<hex>` symbol handle (stable, JSON-safe — #130/#149).
    #[serde(
        rename = "id",
        serialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::serialize"
    )]
    pub logical_symbol_id: Option<i64>,
    #[serde(rename = "ref")]
    pub symbol_path: String,
    pub resolution: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogicalSymbol {
    #[serde(rename = "id", serialize_with = "rag_rat_base::serde_big_id::sym_handle::serialize")]
    pub logical_symbol_id: i64,
    pub qualified_name: String,
    pub variant_count: u64,
    pub group_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogicalSymbolVariant {
    // Internal rowid only — a variant is identified on the wire by cfg/signature/lines (#149).
    #[serde(skip_serializing)]
    pub symbol_id: i64,
    pub cfg_expr: Option<String>,
    pub signature_hash: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
}

#[derive(Debug, Default, Serialize)]
pub struct GraphTraversalSummary {
    pub returned_count: u64,
    pub total_matching_edges: u64,
    pub truncated: bool,
    pub exact_verified: u64,
    pub syntactic: u64,
    pub name_only: u64,
    pub ambiguous: u64,
    /// Matching edges the COMPILER bound to this symbol while the heuristic left `to_symbol_id`
    /// NULL — the oracle-seeded reverse rows, which the read-side enrichment promotes to the
    /// `compiler` tier. Counted here rather than under `unresolved` / the confidence buckets,
    /// which report what tree-sitter alone concluded. Always 0 for a forward traversal, which has
    /// no oracle seed.
    pub compiler_verified: u64,
    pub unresolved: u64,
    pub false_positive_risk: String,
    pub completeness_risk: String,
    /// Why completeness may be worse than the counts suggest — set for a `find_callers` that found
    /// ZERO callers (#200): a static call graph can't see callers reached via message/enum
    /// dispatch, dynamic dispatch, trait objects, FFI, or reflection, nor entry points, so "0
    /// callers" is not proof of none. `None` in the common case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completeness_note: Option<String>,
}

/// The `find_callers`-found-zero completeness note (#200). A const so the MCP output layer can
/// recognize this always-identical string and throttle the repeat per agent (#752).
pub const NO_STATIC_CALLERS_NOTE: &str = "no static callers found; a static call graph can't see \
                                          callers reached via message/enum dispatch, dynamic \
                                          dispatch, trait objects, FFI, or reflection, nor \
                                          external/entry-point callers — 0 may be incomplete";

#[derive(Debug, Default, Serialize)]
pub struct GraphCoverage {
    pub indexed_files: u64,
    pub parser_failures: u64,
    pub stale_files: u64,
    pub known_index_gaps: Vec<String>,
    pub parser_coverage_for_paths: Vec<GraphPathCoverage>,
}

#[derive(Debug, Serialize)]
pub struct GraphPathCoverage {
    pub path: String,
    #[serde(rename = "lang")]
    pub language: String,
    pub parser_status: String,
    pub graph_status: String,
    pub last_indexed_revision: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GraphResolutionMode {
    Exact,
    #[default]
    Syntactic,
    Fuzzy,
}

impl GraphResolutionMode {
    pub fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value.unwrap_or("syntactic") {
            "exact" => Ok(Self::Exact),
            "syntactic" => Ok(Self::Syntactic),
            "fuzzy" => Ok(Self::Fuzzy),
            other => anyhow::bail!(
                "unknown graph resolution mode `{other}`; expected exact, syntactic, or fuzzy"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Syntactic => "syntactic",
            Self::Fuzzy => "fuzzy",
        }
    }
}

impl GraphTraversalOptions {
    pub fn callee_edge_kinds(&self) -> anyhow::Result<Vec<String>> {
        if let Some(edge_kinds) = &self.edge_kinds {
            validate_edge_kinds(edge_kinds)?;
            return Ok(edge_kinds.clone());
        }
        let mut edge_kinds =
            CALL_EDGE_KINDS.iter().map(|value| (*value).to_string()).collect::<Vec<_>>();
        if self.include_macros {
            edge_kinds.extend(MACRO_EDGE_KINDS.iter().map(|value| (*value).to_string()));
        }
        if self.include_references {
            edge_kinds.extend(REFERENCE_EDGE_KINDS.iter().map(|value| (*value).to_string()));
        }
        Ok(edge_kinds)
    }

    pub fn caller_edge_kinds(&self) -> anyhow::Result<Vec<String>> {
        self.callee_edge_kinds()
    }
}

#[derive(Debug, Serialize)]
pub struct CompareGraphTextReport {
    pub query: CompareGraphTextQuery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_symbol: Option<LogicalSymbol>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<LogicalSymbolVariant>,
    pub summary: CompareGraphTextSummary,
    pub coverage: GraphCoverage,
    pub matched_hits: Vec<MatchedGraphTextHit>,
    pub text_only_hits: Vec<TextOnlyHit>,
    pub graph_only_edges: Vec<GraphOnlyEdge>,
    pub likely_parser_gaps: Vec<TextOnlyHit>,
    pub likely_false_positives: Vec<GraphOnlyEdge>,
}

#[derive(Debug, Serialize)]
pub struct CompareGraphTextQuery {
    // Internal rowid — never serialized (reindex-churned, #149); the handle is logical_symbol_id.
    #[serde(skip_serializing)]
    pub symbol_id: Option<i64>,
    // Opaque `sym_<hex>` symbol handle (stable, JSON-safe — #130/#149).
    #[serde(
        rename = "id",
        serialize_with = "rag_rat_base::serde_big_id::sym_handle_opt::serialize"
    )]
    pub logical_symbol_id: Option<i64>,
    #[serde(rename = "ref")]
    pub symbol_path: String,
    pub pattern: String,
    pub resolution: String,
}

#[derive(Debug, Default, Serialize)]
pub struct CompareGraphTextSummary {
    pub graph_hits: u64,
    pub graph_edges: u64,
    pub text_hits: u64,
    pub matched: u64,
    pub graph_only: u64,
    pub text_only: u64,
    pub text_mentions: u64,
    pub likely_parser_gaps: u64,
    pub likely_false_positives: u64,
    pub likely_index_gaps: u64,
    pub complete: bool,
    pub recommended_fallback: String,
    pub pattern_match_mode: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MatchedGraphTextHit {
    pub path: String,
    pub line: i64,
    pub text: String,
    pub target: Option<String>,
    pub edge_kind: String,
    pub confidence: String,
    pub resolution: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextOnlyHit {
    pub path: String,
    pub line: i64,
    pub text: String,
    pub reason: String,
    pub likely_gap: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphOnlyEdge {
    pub path: String,
    pub line: i64,
    pub target: Option<String>,
    pub edge_kind: String,
    pub confidence: String,
    pub resolution: String,
    pub evidence: Option<String>,
    pub reason: String,
    pub likely_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphHop {
    pub edge_id: i64,
    pub from_symbol: Option<String>,
    pub to_symbol: Option<String>,
    pub edge_kind: String,
    /// The DISPLAYED confidence tier. Normally the heuristic tier (`exact`/`syntactic`/…);
    /// upgraded to `compiler` when a current, in-scope `edge_oracle` verdict covers this edge
    /// (the new tier ABOVE `exact`). `edge_confidence` always keeps the underlying heuristic
    /// tier so the upgrade is legible.
    pub confidence: String,
    /// The heuristic edge confidence as stored on the `edges` row, regardless of any oracle
    /// upgrade — so a `compiler`-tier hop still shows what tree-sitter alone concluded.
    pub edge_confidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_qualified_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver_hint: Option<String>,
    pub resolution: String,
    /// `scip:<tool>@<version>` when this hop carries a current oracle verdict — the provenance of
    /// the `compiler` tier (and the `resolved-external` placement). `None` for heuristic-only
    /// hops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_reason: Option<String>,
    /// `resolved-external(<package>)` when the oracle resolved this callee to a dependency outside
    /// the corpus. Present only on `resolved-external` verdicts; `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_external: Option<String>,
    pub verified_target_symbol: bool,
    pub shown_by_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Callsite>,
    /// LOCAL structural-load signal (scoped weighted fan-in) for this neighbor — the THIRD
    /// importance scale, NOT PageRank. Attached by the `impact_surface` enrichment pass over the
    /// neighbors a result already holds. `None` when the neighbor has no in-edges in the active
    /// scope or wasn't enriched. See `crate::load_bearing`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<crate::load_bearing::ImportanceEnrichment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Callsite {
    pub path: String,
    pub line: i64,
    pub span: [i64; 2],
}

/// Report for `compare_graph_to_scip`: where tree-sitter and the compiler (SCIP) DISAGREE on an
/// edge's resolution. Sibling of [`CompareGraphTextReport`]; a user diagnostic + a resolver-
/// debugging instrument. Built from the `Contradict` verdicts in `edge_oracle` (the heuristic
/// resolved an edge to an in-corpus target the compiler says is wrong), scoped to the active
/// checkout and gated to current content — never from drifted/dirty rows.
#[derive(Debug, Serialize)]
pub struct CompareGraphScipReport {
    pub query: CompareGraphScipQuery,
    pub summary: CompareGraphScipSummary,
    /// Edges the compiler contradicts: tree-sitter resolved them one way, SCIP another.
    pub contradictions: Vec<GraphScipContradiction>,
}

#[derive(Debug, Serialize)]
pub struct CompareGraphScipQuery {
    pub tool: String,
    pub tool_version: Option<String>,
    pub commit_sha: String,
    pub worktree_id: String,
}

#[derive(Debug, Default, Serialize)]
pub struct CompareGraphScipSummary {
    /// `edge_oracle` rows examined in scope (current content only).
    pub verdicts_examined: u64,
    /// How many were `Contradict` (the heuristic and compiler disagree).
    pub contradictions: u64,
    /// True when no oracle run has populated this checkout — `contradictions` is then trivially 0
    /// because there's nothing to compare, not because the graph agrees with the compiler.
    pub no_oracle_data: bool,
    pub warnings: Vec<String>,
}

/// One edge where the heuristic and the compiler disagree on the callee's resolution.
#[derive(Debug, Serialize)]
pub struct GraphScipContradiction {
    pub edge_id: i64,
    pub edge_kind: String,
    /// The heuristic's confidence (`exact`/`syntactic`/…) — what tree-sitter concluded.
    pub heuristic_confidence: String,
    /// The heuristic's resolved target symbol (qualified name), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heuristic_target: Option<String>,
    /// The callee name the edge points at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee_name: Option<String>,
    /// The raw SCIP symbol the compiler resolved the callee to — the disagreement's other side.
    pub scip_symbol: String,
    /// `resolved-external(<package>)` when the compiler placed the callee in a dependency; `None`
    /// when it resolved to a different in-corpus symbol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_external: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Callsite>,
}
