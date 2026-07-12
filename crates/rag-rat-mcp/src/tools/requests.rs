//! MCP tool request/argument types: the per-tool `*Args` structs, the `*Include` and `Mcp*` mode
//! enums (with their tolerant `Deserialize` impls and `as_str`/`parse`/`core` helpers), and the
//! conversions from arg structs into the `rag_rat_core` request types. The dispatch layer in
//! `mod.rs` deserializes into these; `handlers.rs` consumes them. Shared imports (serde, the
//! `rag_rat_core` query types, the `default_*` fns) come through the parent via `use super::*`.

use super::*;

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpGraphMode {
    None,
    Compact,
    Full,
}

impl McpGraphMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }
}

impl<'de> Deserialize<'de> for McpGraphMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = McpGraphMode;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("none, compact, or full")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(if value { McpGraphMode::Compact } else { McpGraphMode::None })
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "none" | "false" => Ok(McpGraphMode::None),
                    "compact" | "true" => Ok(McpGraphMode::Compact),
                    "full" => Ok(McpGraphMode::Full),
                    other => Err(E::custom(format!(
                        "unknown graph metadata mode `{other}`; expected none, compact, or full"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpGraphResolutionMode {
    Exact,
    Syntactic,
    Fuzzy,
}

impl McpGraphResolutionMode {
    pub(super) fn core(self) -> GraphResolutionMode {
        match self {
            Self::Exact => GraphResolutionMode::Exact,
            Self::Syntactic => GraphResolutionMode::Syntactic,
            Self::Fuzzy => GraphResolutionMode::Fuzzy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpGraphEdgeKind {
    CallsName,
    Constructs,
    Dispatches,
    UsesMacro,
    ReferencesType,
    Imports,
    Exports,
    Contains,
    Implements,
}

impl McpGraphEdgeKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::CallsName => "calls_name",
            Self::Constructs => "constructs",
            Self::Dispatches => "dispatches",
            Self::UsesMacro => "uses_macro",
            Self::ReferencesType => "references_type",
            Self::Imports => "imports",
            Self::Exports => "exports",
            Self::Contains => "contains",
            Self::Implements => "implements",
        }
    }
}

/// Deserialize an optional `Vec<T>` that may arrive EITHER as a real JSON array (the schema form)
/// OR as a JSON-string-encoded array (`"[\"git\"]"`). Some MCP clients serialize non-string args as
/// strings — Claude Code does this for array/object params (anthropics/claude-code#24599) — so the
/// array `include` surface is unusable from them unless the server also accepts the stringified
/// form. We keep advertising a real array (schema unchanged) and rescue the stringified case here;
/// `null`/absent -> `None`, `[]` -> `Some(vec![])` (the explicit empty on-set). Serialize stays the
/// default (a real array), which round-trips back through this deserializer fine — so no
/// `serialize_with` is needed (contrast the scalar `sym_handle` fields, which DO need symmetry).
fn de_seq_or_json_string<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    use serde::de::Error as _;
    match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => serde_json::from_str(&raw).map(Some).map_err(D::Error::custom),
        Some(other) => serde_json::from_value(other).map(Some).map_err(D::Error::custom),
    }
}

/// Non-`Option` sibling of [`de_seq_or_json_string`] for a plain `Vec<T>` field (e.g.
/// `personalize`): same array-or-JSON-string tolerance, with `null`/absent collapsing to an empty
/// `Vec`.
fn de_vec_or_json_string<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(de_seq_or_json_string(deserializer)?.unwrap_or_default())
}

/// `semantic_search` `include` flags. `git`/`papertrail` are on by default (omit `include` to keep
/// them); `generated`/`fallback` are off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchInclude {
    Generated,
    Git,
    Papertrail,
    Fallback,
}

/// `repo_brief` / `repo_clusters` `include` flags. `memories` on by default; `generated` off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrientationInclude {
    Generated,
    Memories,
}

/// `read_chunk` `include` flags. `memories` on by default (pass `include: []` to suppress).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoriesInclude {
    Memories,
}

/// `symbol_lookup` `include` flags. `memories` on by default (pass `include: []` to suppress);
/// `generated` opts generated bindings (ubrn FFI output, codegen) back into the results — they're
/// excluded by default because they bury the source symbol a search is after (#202).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolInclude {
    Memories,
    Generated,
}

/// `find_callers` / `trace_callees` `include` flags. `memories` on by default; the rest off (they
/// add unresolved/macro/common-method/reference noise or the coverage block on demand).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GraphInclude {
    References,
    Unresolved,
    Macros,
    CommonMethods,
    Coverage,
    Memories,
}

/// `compare_graph_to_text` `include` flags. `tests` on by default; the rest off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompareInclude {
    Tests,
    References,
    Unresolved,
    Macros,
    CommonMethods,
}

/// `impact_surface` `include` flags — ALL on by default (impact's value is the bundled evidence).
/// Pass an explicit `include` to narrow it, e.g. `["git"]` for git history only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImpactInclude {
    Tests,
    Docs,
    Git,
    Papertrail,
    TextFallback,
    Memories,
}

/// `papertrail_for_commit` `include` flags. `fallback` off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PapertrailCommitInclude {
    Fallback,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    #[serde(default)]
    pub explain: bool,
    /// What to include: `git`, `papertrail` (both on by default), `generated`, `fallback` (off by
    /// default). Omit to keep defaults; an explicit list is the exact on-set.
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<SearchInclude>>,
    #[serde(default = "default_search_graph_mode")]
    pub include_graph: McpGraphMode,
    #[serde(default = "default_search_graph_limit")]
    pub graph_limit: u32,
    /// Absolute path to a linked git worktree you're working in. When set, results are served from
    /// that worktree's branch overlay (its committed + uncommitted changes) on top of the indexed
    /// checkout; omit to query the indexed checkout. An unrelated/invalid path falls back to it.
    #[serde(default)]
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpRepoBriefMode {
    Spine,
    Churn,
    GodModules,
    RefactorCandidates,
}

impl McpRepoBriefMode {
    pub(super) fn core(self) -> RepoBriefMode {
        match self {
            Self::Spine => RepoBriefMode::Spine,
            Self::Churn => RepoBriefMode::Churn,
            Self::GodModules => RepoBriefMode::GodModules,
            Self::RefactorCandidates => RepoBriefMode::RefactorCandidates,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RepoBriefArgs {
    #[serde(default = "default_repo_brief_mode")]
    pub mode: McpRepoBriefMode,
    #[serde(default = "default_repo_brief_limit")]
    pub limit: u32,
    /// What to include: `memories` (on by default), `generated` (off). Omit to keep defaults.
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<OrientationInclude>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RepoClustersArgs {
    #[serde(default = "default_repo_brief_limit")]
    pub limit: u32,
    /// What to include: `memories` (on by default), `generated` (off). Omit to keep defaults.
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<OrientationInclude>>,
    #[serde(default = "default_min_cluster_size")]
    pub min_cluster_size: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ImportantSymbolsArgs {
    /// Max load-bearing symbols to return.
    #[serde(default = "default_repo_brief_limit")]
    pub limit: u32,
    /// Symbols to bias importance toward (the symbols you're editing/querying) — names, refs
    /// (`path::name`), or `sym_<hex>` handles; the random surfer teleports back to these, lifting
    /// the spine *they* depend on. A `sym_<hex>` handle resolves to its logical symbol's members;
    /// otherwise the entry is resolved by ref then name (ambiguous/missing entries are skipped,
    /// never fatal). LEAVE EMPTY to auto-seed from
    /// your current git diff (the default — "importance relative to your current changes").
    /// Pass a single `"global"` to force whole-repo PageRank instead.
    #[serde(default, deserialize_with = "de_vec_or_json_string")]
    pub personalize: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolArgs {
    pub symbol: Option<String>,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(
        rename = "id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    #[serde(rename = "lang")]
    pub language: Option<String>,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default = "default_symbol_limit")]
    pub limit: u32,
    /// What to include: `memories` (on by default) and/or `generated` (off by default — opts
    /// generated bindings back into the results). Pass `include: []` to suppress memories.
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<SymbolInclude>>,
    /// Absolute path to a linked git worktree you're working in; serves that worktree's branch
    /// overlay over the indexed checkout. Omit (or pass an unrelated path) for the indexed
    /// checkout.
    #[serde(default)]
    pub worktree: Option<String>,
}

/// Pure symbol-selector args (symbol/ref/id/lang) with no `include` — for tools that resolve ONE
/// symbol and never opt generated rows back in (`git_history_for_symbol`, `papertrail_for_symbol`).
/// These go through `select_symbol`, which always excludes generated; advertising a `generated`
/// include they silently ignore would be a lie, so they don't carry one (#202 review).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolRefArgs {
    pub symbol: Option<String>,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(
        rename = "id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    #[serde(rename = "lang")]
    pub language: Option<String>,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default = "default_symbol_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolGraphArgs {
    pub symbol: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(
        rename = "id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    pub resolution: Option<McpGraphResolutionMode>,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
    #[serde(default)]
    pub allow_ambiguous: bool,
    /// What to include: `memories` (on by default); `references`, `unresolved`, `macros`,
    /// `common_methods`, `coverage` (all off by default). Omit to keep defaults; an explicit list
    /// is the exact on-set (so listing `macros` alone also drops the default `memories`).
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<GraphInclude>>,
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub edge_kinds: Option<Vec<McpGraphEdgeKind>>,
    /// Absolute path to a linked git worktree you're working in; serves that worktree's branch
    /// overlay over the indexed checkout. Omit (or pass an unrelated path) for the indexed
    /// checkout.
    #[serde(default)]
    pub worktree: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CompareGraphTextArgs {
    pub pattern: String,
    pub symbol: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(
        rename = "id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    pub resolution: Option<McpGraphResolutionMode>,
    #[serde(default = "default_compare_limit")]
    pub limit: u32,
    #[serde(default)]
    pub allow_ambiguous: bool,
    /// What to include: `tests` (on by default); `references`, `unresolved`, `macros`,
    /// `common_methods` (off by default). Omit to keep defaults; an explicit list is the exact
    /// on-set.
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<CompareInclude>>,
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub edge_kinds: Option<Vec<McpGraphEdgeKind>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ImpactArgs {
    pub query: Option<String>,
    pub symbol: Option<String>,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(
        rename = "id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    pub resolution: Option<McpGraphResolutionMode>,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
    /// What to include — `tests`, `docs`, `git`, `papertrail`, `text_fallback`, `memories`, ALL on
    /// by default (impact's value is the bundled evidence). Omit to keep them; pass an explicit
    /// list to narrow, e.g. `["git"]` for git history only. `git` bundles both the recent commits
    /// touching the symbol's file and the files that historically co-changed with it (the windowed
    /// change-coupling section).
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<ImpactInclude>>,
    /// Return full memory bodies + every binding + call paths instead of the default compact,
    /// scannable per-memory headers (#37). To expand ONE memory by id (e.g. the `memory_id` from a
    /// `surface="summary"` compact attachment), call `memory_show`; full detail for a symbol/path
    /// is also reachable via `memory_for_symbol` / `memory_for_path` / `memory_for_call_path`.
    #[serde(default)]
    pub full_memories: bool,
    /// Absolute path to a linked git worktree you're working in; serves that worktree's branch
    /// overlay over the indexed checkout. Omit (or pass an unrelated path) for the indexed
    /// checkout.
    #[serde(default)]
    pub worktree: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct LimitArgs {
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CheckLibraryUsageArgs {
    /// Restrict to external call sites in this exact file or under this directory prefix (e.g.
    /// `src/net`). Omit for the whole checkout.
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict to one dependency package — the moniker's package component, e.g. `ky` / `tokio`.
    #[serde(default)]
    pub package: Option<String>,
    /// Only surface contracts flagged deprecated (the asserted verdict).
    #[serde(default)]
    pub deprecated_only: bool,
    /// Max dependency-symbol entries returned; summary counts always cover the full set.
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
    /// Absolute path to a linked git worktree you're working in; serves that worktree's branch
    /// overlay over the indexed checkout. Omit for the indexed checkout.
    #[serde(default)]
    pub worktree: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadChunkArgs {
    pub chunk_id: i64,
    #[serde(default = "default_read_chunk_graph_mode")]
    pub include_graph: McpGraphMode,
    #[serde(default = "default_read_chunk_graph_limit")]
    pub graph_limit: u32,
    /// What to include: `memories` (on by default). Pass `include: []` to suppress.
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<MemoriesInclude>>,
    /// Absolute path to a linked git worktree you're working in; serves that worktree's branch
    /// overlay over the indexed checkout. Omit (or pass an unrelated path) for the indexed
    /// checkout.
    #[serde(default)]
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
pub enum McpMemoryKind {
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
    Task,
    Concept,
}

impl McpMemoryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Invariant => "Invariant",
            Self::Decision => "Decision",
            Self::RejectedAlternative => "RejectedAlternative",
            Self::Risk => "Risk",
            Self::BugPattern => "BugPattern",
            Self::TestExpectation => "TestExpectation",
            Self::PerformanceNote => "PerformanceNote",
            Self::SecurityNote => "SecurityNote",
            Self::FFIBoundary => "FFIBoundary",
            Self::PlatformQuirk => "PlatformQuirk",
            Self::FollowUp => "FollowUp",
            Self::OpenQuestion => "OpenQuestion",
            Self::Obsolete => "Obsolete",
            Self::Task => "Task",
            Self::Concept => "Concept",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpMemoryConfidence {
    High,
    Medium,
    Low,
}

impl McpMemoryConfidence {
    fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpMemoryStatus {
    Active,
    Stale,
    Obsolete,
    Rejected,
}

impl McpMemoryStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Obsolete => "obsolete",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpMemorySource {
    Agent,
    Human,
    Imported,
    Generated,
}

impl McpMemorySource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Human => "human",
            Self::Imported => "imported",
            Self::Generated => "generated",
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryBindArgs {
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(
        rename = "id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub edge_id: Option<i64>,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub commit_hash: Option<String>,
    /// Tracker-item binding: all three of `tracker` (e.g. `github`), `project`
    /// (e.g. `owner/repo`), and `item_key` (e.g. `588`) together.
    pub tracker: Option<String>,
    pub project: Option<String>,
    pub item_key: Option<String>,
    #[serde(
        rename = "start_id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub start_logical_symbol_id: Option<i64>,
    #[serde(
        rename = "end_id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub end_logical_symbol_id: Option<i64>,
    pub edge_sequence_hash: Option<String>,
    pub path_summary: Option<String>,
    /// Ordered edge ids (e.g. from `find_callers`/`trace_callees`) for a server-derived call-path
    /// binding. The server computes the authoritative `edge_sequence_hash` from these edges —
    /// preferred over passing `edge_sequence_hash` directly.
    pub edge_path: Option<Vec<i64>>,
    /// Directory path relative to the repo root (e.g. `"src/actors"`); empty string anchors to the
    /// repo root.
    pub dir: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryCreateArgs {
    pub kind: McpMemoryKind,
    /// One-line summary, max 160 characters.
    pub title: String,
    /// The memory text (the *why* + *how to apply*), max 8000 characters.
    pub body: String,
    pub confidence: McpMemoryConfidence,
    pub created_by: Option<String>,
    pub source: Option<McpMemorySource>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional structured payload (#465) for a polymorphic node — a `Task`/`Concept`'s
    /// kind-specific JSON object (e.g. a task's priority/estimate). Must be a JSON object.
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    /// Optional (#463): omit to create an UNANCHORED node (a `Concept` or standalone `Task` that
    /// lives only as a graph node). When present, names exactly one code/anchor binding.
    #[serde(default)]
    pub bind: MemoryBindArgs,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryRebindArgs {
    pub memory_id: String,
    pub bind: MemoryBindArgs,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryUpdateArgs {
    pub memory_id: String,
    pub kind: Option<McpMemoryKind>,
    /// One-line summary, max 160 characters.
    pub title: Option<String>,
    /// The memory text (the *why* + *how to apply*), max 8000 characters.
    pub body: Option<String>,
    pub confidence: Option<McpMemoryConfidence>,
    pub status: Option<McpMemoryStatus>,
    pub tags: Option<Vec<String>>,
    /// Set the node's structured payload (#465). Omit to leave the stored payload unchanged; a
    /// JSON object replaces it.
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemorySearchArgs {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryForSymbolArgs {
    pub symbol: Option<String>,
    #[serde(rename = "ref")]
    pub symbol_path: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(
        rename = "id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryForPathArgs {
    pub path: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryForCallPathArgs {
    pub edge_sequence_hash: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryIdArgs {
    pub memory_id: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PathHistoryArgs {
    pub path: String,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BlameChunkArgs {
    pub chunk_id: i64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PapertrailChunkArgs {
    pub chunk_id: i64,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PapertrailCommitArgs {
    pub commit_hash: String,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
    /// What to include: `fallback` (off by default).
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<PapertrailCommitInclude>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct HealIndexArgs {
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Default)]
pub struct EmptyArgs {}

impl MemoryCreateArgs {
    pub(super) fn core(self) -> RepoMemoryCreate {
        RepoMemoryCreate {
            kind: self.kind.as_str().to_string(),
            title: self.title,
            body: self.body,
            confidence: self.confidence.as_str().to_string(),
            created_by: self.created_by,
            source: self.source.map(|source| source.as_str().to_string()),
            tags: self.tags,
            payload_json: self.payload.map(|payload| payload.to_string()),
            bind: self.bind.into(),
        }
    }
}

impl MemoryUpdateArgs {
    pub(super) fn core(self) -> RepoMemoryUpdate {
        RepoMemoryUpdate {
            memory_id: self.memory_id,
            kind: self.kind.map(|kind| kind.as_str().to_string()),
            title: self.title,
            body: self.body,
            confidence: self.confidence.map(|confidence| confidence.as_str().to_string()),
            status: self.status.map(|status| status.as_str().to_string()),
            tags: self.tags,
            payload_json: self.payload.map(|payload| payload.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpEdgeRelation {
    DependsOn,
    RelatesTo,
    Supersedes,
    DerivedFrom,
    Tracks,
}

impl McpEdgeRelation {
    fn as_str(self) -> &'static str {
        match self {
            Self::DependsOn => "depends_on",
            Self::RelatesTo => "relates_to",
            Self::Supersedes => "supersedes",
            Self::DerivedFrom => "derived_from",
            Self::Tracks => "tracks",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpEdgeDirection {
    /// Edges OUT of the node (its dependencies / mind-map links / tracks).
    From,
    /// Edges INTO the node — the reverse traversal (e.g. tasks that track an issue).
    Into,
}

/// Add a typed graph edge (#464). Give EXACTLY ONE target: another node (`target_node_id`, with an
/// optional `target_repo_id` for a cross-repo edge) OR a GitHub issue (all three `github_*`).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryEdgeAddArgs {
    pub source_node_id: String,
    pub relation: McpEdgeRelation,
    pub target_node_id: Option<String>,
    pub target_repo_id: Option<String>,
    pub github_owner: Option<String>,
    pub github_repo: Option<String>,
    pub github_number: Option<i64>,
}

/// Build an `EdgeTarget` from the flat wire fields shared by the edge tools: EXACTLY ONE of a node
/// (`node_id`, with optional `node_repo_id`) or a full github ref. `node_field` names the node arg
/// in the error so each tool reports its own field name.
fn edge_target_from_parts(
    node_id: Option<&str>,
    node_repo_id: Option<&str>,
    github: (Option<&str>, Option<&str>, Option<i64>),
    node_field: &str,
) -> anyhow::Result<rag_rat_core::query::memory::EdgeTarget> {
    use rag_rat_core::query::memory::EdgeTarget;
    match (node_id, github) {
        (Some(node_id), (None, None, None)) => Ok(EdgeTarget::Node {
            repo_id: node_repo_id.map(str::to_string),
            node_id: node_id.to_string(),
        }),
        (None, (Some(owner), Some(repo), Some(number))) =>
            Ok(EdgeTarget::Github { owner: owner.to_string(), repo: repo.to_string(), number }),
        _ => anyhow::bail!(
            "needs exactly one target: a {node_field}, OR a full github ref (github_owner + \
             github_repo + github_number)"
        ),
    }
}

impl MemoryEdgeAddArgs {
    pub(super) fn relation_str(&self) -> &'static str {
        self.relation.as_str()
    }

    pub(super) fn target(&self) -> anyhow::Result<rag_rat_core::query::memory::EdgeTarget> {
        edge_target_from_parts(
            self.target_node_id.as_deref(),
            self.target_repo_id.as_deref(),
            (self.github_owner.as_deref(), self.github_repo.as_deref(), self.github_number),
            "target_node_id",
        )
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryEdgeRemoveArgs {
    pub edge_key: String,
}

/// List a node's typed edges. `direction=from` lists a SOURCE node's outgoing edges (`node_id`
/// required — a github issue has no outgoing edges). `direction=into` is the reverse traversal INTO
/// a target: give EITHER `node_id` (nodes that edge into it) OR a full github ref (e.g. the tasks
/// that `tracks` an issue).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MemoryEdgesArgs {
    pub direction: McpEdgeDirection,
    pub node_id: Option<String>,
    pub github_owner: Option<String>,
    pub github_repo: Option<String>,
    pub github_number: Option<i64>,
}

impl MemoryEdgesArgs {
    /// The reverse-traversal target for `direction=into` — a node id ignores its repo (the match is
    /// on the globally-unique anchor), or a full github ref.
    pub(super) fn reverse_target(&self) -> anyhow::Result<rag_rat_core::query::memory::EdgeTarget> {
        edge_target_from_parts(
            self.node_id.as_deref(),
            None,
            (self.github_owner.as_deref(), self.github_repo.as_deref(), self.github_number),
            "node_id",
        )
    }
}

/// `dream`: recompute and return the deterministic memory-maintenance worklist (coverage gaps +
/// stale references). Mirrors the SHAPE of the read tools (returns a ranked worklist) but is
/// classified as a write tool — like the CLI `rag-rat dream`, it syncs `dream_findings`. It does
/// NOT run the opt-in model verdict/compaction passes (those stay on the CLI/cron `--verify` /
/// `--compact`); findings a prior model run persisted still surface here.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DreamArgs {
    /// Max `coverage_gap` findings to compute (the load-bearing-symbol budget); defaults to 20.
    #[serde(default = "default_dream_limit")]
    pub limit: u32,
    /// Also surface the human-reviewed (`accepted` / `dismissed`) findings, not just the open
    /// worklist — the `rag-rat dream --all` listing, so a reviewer can see and `reset` them.
    #[serde(default)]
    pub all: bool,
}

/// The human verdict `dream_review` applies to a dream finding — mirrors the CLI
/// `rag-rat dream <id> --accept|--dismiss|--reset`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpReviewVerdict {
    /// Confirm the finding as a real gap to act on; the verdict survives future dream runs.
    Accept,
    /// Reject the finding as noise; the verdict survives future dream runs.
    Dismiss,
    /// Clear a prior accept/dismiss, returning the finding to the open worklist.
    Reset,
}

impl McpReviewVerdict {
    pub(super) fn core(self) -> rag_rat_core::dream::ReviewVerdict {
        use rag_rat_core::dream::ReviewVerdict;
        match self {
            Self::Accept => ReviewVerdict::Accept,
            Self::Dismiss => ReviewVerdict::Dismiss,
            Self::Reset => ReviewVerdict::Reset,
        }
    }
}

/// `dream_review`: apply a human verdict to ONE dream finding by id or prefix — the pull-based
/// counterpart to the CLI review surface, so a strong agent burns down the worklist over MCP.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DreamReviewArgs {
    /// The finding id from the `dream` worklist — a full id or an unambiguous git-style PREFIX.
    pub finding: String,
    /// `accept` / `dismiss` / `reset`.
    pub verdict: McpReviewVerdict,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindClonesArgs {
    /// Minimum pairwise overlap/max_len similarity. Must be in the range [0.5, 1.0]; defaults to
    /// 0.7 (the θ threshold) when omitted. Out-of-range values are rejected.
    pub min_similarity: Option<f64>,
    /// Minimum number of copies for a class to be returned (defaults to 2).
    pub min_copies: Option<usize>,
    /// Maximum number of clone classes to return, sorted by ROI descending. A supplied limit is
    /// capped at the refine budget (currently 50): `limit: N` returns at most 50 classes, all
    /// refined. Omit (null) to retrieve all classes (only the top 50 refined, the rest unrefined).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ClonesForSymbolArgs {
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(
        rename = "id",
        default,
        serialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::serialize",
        deserialize_with = "rag_rat_core::serde_big_id::sym_handle_opt::deserialize"
    )]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    #[serde(rename = "ref")]
    pub symbol_ref: Option<String>,
    pub path: Option<String>,
    pub line: Option<i64>,
}

impl ClonesForSymbolArgs {
    pub(super) fn into_selector(self) -> anyhow::Result<rag_rat_core::index::CloneSymbolSelector> {
        use rag_rat_core::index::CloneSymbolSelector;

        // Enforce EXACTLY ONE selector form: `id`, `ref`, or `path`+`line` (a path and a line
        // together count as a SINGLE form). Counting populated forms — not first-wins precedence —
        // is what makes a conflicting `{id, ref}` (or `{id, path, line}`, …) an error instead of a
        // silently-resolved ambiguity.
        let has_id = self.logical_symbol_id.is_some();
        let has_ref = self.symbol_ref.is_some();
        let has_path_line = match (&self.path, &self.line) {
            (Some(_), Some(_)) => true,
            (Some(_), None) => anyhow::bail!("clones_for_symbol: `path` requires `line`"),
            (None, Some(_)) => anyhow::bail!("clones_for_symbol: `line` requires `path`"),
            (None, None) => false,
        };

        let forms = usize::from(has_id) + usize::from(has_ref) + usize::from(has_path_line);
        if forms != 1 {
            anyhow::bail!(
                "clones_for_symbol: provide exactly one of `id`, `ref`, or `path`+`line`"
            );
        }

        if let Some(id) = self.logical_symbol_id {
            return Ok(CloneSymbolSelector::Id(rag_rat_core::serde_big_id::format_sym_handle(id)));
        }
        if let Some(r) = self.symbol_ref {
            return Ok(CloneSymbolSelector::Ref(r));
        }
        // The exactly-one count guarantees path+line are both present here.
        match (self.path, self.line) {
            (Some(path), Some(line)) => Ok(CloneSymbolSelector::PathLine { path, line }),
            _ => unreachable!("forms == 1 with neither id nor ref implies path+line"),
        }
    }
}

impl From<MemoryBindArgs> for RepoMemoryBindTarget {
    fn from(args: MemoryBindArgs) -> Self {
        Self {
            logical_symbol_id: args.logical_symbol_id,
            symbol_id: None,
            chunk_id: args.chunk_id,
            edge_id: args.edge_id,
            path: args.path,
            start_line: args.start_line,
            end_line: args.end_line,
            commit_hash: args.commit_hash,
            tracker: args.tracker,
            project: args.project,
            item_key: args.item_key,
            start_logical_symbol_id: args.start_logical_symbol_id,
            end_logical_symbol_id: args.end_logical_symbol_id,
            edge_sequence_hash: args.edge_sequence_hash,
            path_summary: args.path_summary,
            edge_path: args.edge_path,
            dir: args.dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use rag_rat_core::index::CloneSymbolSelector;

    use super::ClonesForSymbolArgs;

    #[test]
    fn memory_edges_into_target_routes_node_and_github() {
        use rag_rat_core::query::memory::EdgeTarget;

        use super::{McpEdgeDirection, MemoryEdgesArgs};
        let into = |node_id, gh: Option<(&str, &str, i64)>| MemoryEdgesArgs {
            direction: McpEdgeDirection::Into,
            node_id,
            github_owner: gh.map(|g| g.0.to_string()),
            github_repo: gh.map(|g| g.1.to_string()),
            github_number: gh.map(|g| g.2),
        };
        // #464 review fix: `direction=into` must reach a GITHUB target, not always build a node.
        assert!(matches!(
            into(None, Some(("o", "r", 7))).reverse_target().unwrap(),
            EdgeTarget::Github { number: 7, .. }
        ));
        assert!(matches!(
            into(Some("mem_x".to_string()), None).reverse_target().unwrap(),
            EdgeTarget::Node { .. }
        ));
        // Exactly one target: neither (or both) is an error.
        assert!(into(None, None).reverse_target().is_err());
        assert!(into(Some("mem_x".to_string()), Some(("o", "r", 7))).reverse_target().is_err());
    }

    #[test]
    fn memory_edge_add_relation_and_target_mappings() {
        use rag_rat_core::query::memory::EdgeTarget;

        use super::{McpEdgeRelation, MemoryEdgeAddArgs};
        let add = |relation, node: Option<&str>, gh: Option<(&str, &str, i64)>| MemoryEdgeAddArgs {
            source_node_id: "s".to_string(),
            relation,
            target_node_id: node.map(str::to_string),
            target_repo_id: None,
            github_owner: gh.map(|g| g.0.to_string()),
            github_repo: gh.map(|g| g.1.to_string()),
            github_number: gh.map(|g| g.2),
        };
        // Every relation maps to its db token.
        for (relation, token) in [
            (McpEdgeRelation::DependsOn, "depends_on"),
            (McpEdgeRelation::RelatesTo, "relates_to"),
            (McpEdgeRelation::Supersedes, "supersedes"),
            (McpEdgeRelation::DerivedFrom, "derived_from"),
            (McpEdgeRelation::Tracks, "tracks"),
        ] {
            assert_eq!(add(relation, Some("t"), None).relation_str(), token);
        }
        // Target: exactly one of a node or a github ref.
        assert!(matches!(
            add(McpEdgeRelation::RelatesTo, Some("t"), None).target().unwrap(),
            EdgeTarget::Node { .. }
        ));
        assert!(matches!(
            add(McpEdgeRelation::Tracks, None, Some(("o", "r", 1))).target().unwrap(),
            EdgeTarget::Github { .. }
        ));
        assert!(add(McpEdgeRelation::Tracks, None, None).target().is_err());
        assert!(add(McpEdgeRelation::Tracks, Some("t"), Some(("o", "r", 1))).target().is_err());
    }

    fn args(
        id: Option<i64>,
        symbol_ref: Option<&str>,
        path: Option<&str>,
        line: Option<i64>,
    ) -> ClonesForSymbolArgs {
        ClonesForSymbolArgs {
            logical_symbol_id: id,
            symbol_ref: symbol_ref.map(str::to_string),
            path: path.map(str::to_string),
            line,
        }
    }

    #[test]
    fn into_selector_accepts_exactly_one_form() {
        // id alone.
        assert!(matches!(
            args(Some(7), None, None, None).into_selector().unwrap(),
            CloneSymbolSelector::Id(_)
        ));
        // ref alone.
        assert!(matches!(
            args(None, Some("src/a.rs::f"), None, None).into_selector().unwrap(),
            CloneSymbolSelector::Ref(_)
        ));
        // path+line together (one form).
        assert!(matches!(
            args(None, None, Some("src/a.rs"), Some(1)).into_selector().unwrap(),
            CloneSymbolSelector::PathLine { .. }
        ));
    }

    #[test]
    fn into_selector_rejects_conflicting_id_and_ref() {
        let err = args(Some(7), Some("src/a.rs::f"), None, None).into_selector().unwrap_err();
        assert!(
            err.to_string().contains("exactly one"),
            "both id and ref must be rejected as conflicting selectors: {err}"
        );
    }

    #[test]
    fn into_selector_rejects_conflicting_id_and_path_line() {
        let err = args(Some(7), None, Some("src/a.rs"), Some(1)).into_selector().unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err}");
    }

    #[test]
    fn into_selector_rejects_none() {
        let err = args(None, None, None, None).into_selector().unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err}");
    }

    #[test]
    fn into_selector_rejects_path_without_line() {
        let err = args(None, None, Some("src/a.rs"), None).into_selector().unwrap_err();
        assert!(err.to_string().contains("`path` requires `line`"), "{err}");
    }

    #[test]
    fn into_selector_rejects_line_without_path() {
        let err = args(None, None, None, Some(1)).into_selector().unwrap_err();
        assert!(err.to_string().contains("`line` requires `path`"), "{err}");
    }
}
