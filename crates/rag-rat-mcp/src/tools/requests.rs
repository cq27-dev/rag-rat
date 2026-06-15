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

/// `symbol_lookup` / `read_chunk` `include` flags. `memories` on by default (pass `include: []` to
/// suppress).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoriesInclude {
    Memories,
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
    /// What to include: `memories` (on by default). Pass `include: []` to suppress.
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<MemoriesInclude>>,
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
    /// list to narrow, e.g. `["git"]` for git history only.
    #[serde(default, deserialize_with = "de_seq_or_json_string")]
    pub include: Option<Vec<ImpactInclude>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct LimitArgs {
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
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

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
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
    pub github_owner: Option<String>,
    pub github_repo: Option<String>,
    pub github_number: Option<i64>,
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
    pub title: String,
    pub body: String,
    pub confidence: McpMemoryConfidence,
    pub created_by: Option<String>,
    pub source: Option<McpMemorySource>,
    #[serde(default)]
    pub tags: Vec<String>,
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
    pub title: Option<String>,
    pub body: Option<String>,
    pub confidence: Option<McpMemoryConfidence>,
    pub status: Option<McpMemoryStatus>,
    pub tags: Option<Vec<String>>,
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
            github_owner: args.github_owner,
            github_repo: args.github_repo,
            github_number: args.github_number,
            start_logical_symbol_id: args.start_logical_symbol_id,
            end_logical_symbol_id: args.end_logical_symbol_id,
            edge_sequence_hash: args.edge_sequence_hash,
            path_summary: args.path_summary,
            edge_path: args.edge_path,
            dir: args.dir,
        }
    }
}
