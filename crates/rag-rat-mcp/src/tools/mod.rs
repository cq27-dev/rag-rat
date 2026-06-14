mod defaults;
mod handlers;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

pub(crate) use defaults::*;
pub(crate) use handlers::*;
use rag_rat_core::language::Language;
use rag_rat_core::query::clusters::RepoClustersOptions;
use rag_rat_core::query::graph::{GraphResolutionMode, GraphTraversalOptions};
use rag_rat_core::query::graph_meta::GraphMetaMode;
use rag_rat_core::query::impact::ImpactSurfaceOptions;
use rag_rat_core::query::memory::{RepoMemoryBindTarget, RepoMemoryCreate, RepoMemoryUpdate};
use rag_rat_core::query::repo_brief::{RepoBriefMode, RepoBriefOptions};
use rag_rat_core::query::symbol::SymbolSelector;
use rag_rat_core::search::lexical::SearchOptions;
use rag_rat_core::{Config, IndexDatabase};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpGraphMode {
    None,
    Compact,
    Full,
}

impl McpGraphMode {
    fn as_str(self) -> &'static str {
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
    fn core(self) -> GraphResolutionMode {
        match self {
            Self::Exact => GraphResolutionMode::Exact,
            Self::Syntactic => GraphResolutionMode::Syntactic,
            Self::Fuzzy => GraphResolutionMode::Fuzzy,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
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
    fn as_str(self) -> &'static str {
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

pub const TOOL_NAMES: &[&str] = &[
    "semantic_search",
    "symbol_lookup",
    "find_callers",
    "trace_callees",
    "compare_graph_to_text",
    "compare_graph_to_scip",
    "impact_surface",
    "repo_brief",
    "repo_clusters",
    "ffi_surface",
    "docs_for_symbol",
    "read_chunk",
    "commit_search",
    "git_history_for_path",
    "git_history_for_symbol",
    "commits_touching_query",
    "git_blame_chunk",
    "papertrail_for_chunk",
    "papertrail_for_symbol",
    "papertrail_for_commit",
    "github_issue_search",
    "github_refs_for_path",
    "rationale_search",
    "local_ai_status",
    "heal_index",
    "github_sync_status",
    "index_status",
    "memory_create",
    "memory_rebind",
    "memory_update",
    "memory_search",
    "memory_for_symbol",
    "memory_for_path",
    "memory_for_call_path",
    "memory_validate",
    "memory_mark_obsolete",
];

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    #[serde(default)]
    pub include_generated: bool,
    #[serde(default)]
    pub explain: bool,
    #[serde(default = "default_true")]
    pub include_git: bool,
    #[serde(default = "default_true")]
    pub include_papertrail: bool,
    #[serde(default = "default_search_graph_mode")]
    pub include_graph: McpGraphMode,
    #[serde(default = "default_search_graph_limit")]
    pub graph_limit: u32,
    #[serde(default)]
    pub include_fallback: bool,
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
    fn core(self) -> RepoBriefMode {
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
    #[serde(default)]
    pub include_generated: bool,
    #[serde(default = "default_true")]
    pub include_memories: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RepoClustersArgs {
    #[serde(default = "default_repo_brief_limit")]
    pub limit: u32,
    #[serde(default)]
    pub include_generated: bool,
    #[serde(default = "default_true")]
    pub include_memories: bool,
    #[serde(default = "default_min_cluster_size")]
    pub min_cluster_size: u32,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolArgs {
    pub symbol: Option<String>,
    pub symbol_path: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(default, deserialize_with = "rag_rat_core::serde_big_id::big_id_opt::deserialize")]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub language: Option<String>,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default = "default_symbol_limit")]
    pub limit: u32,
    #[serde(default = "default_true")]
    pub include_memories: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SymbolGraphArgs {
    pub symbol: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(default, deserialize_with = "rag_rat_core::serde_big_id::big_id_opt::deserialize")]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub symbol_path: Option<String>,
    pub resolution: Option<McpGraphResolutionMode>,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default)]
    pub include_references: bool,
    #[serde(default)]
    pub include_unresolved: bool,
    #[serde(default)]
    pub include_macros: bool,
    #[serde(default)]
    pub include_common_methods: bool,
    #[serde(default)]
    pub include_coverage: bool,
    #[serde(default = "default_true")]
    pub include_memories: bool,
    pub edge_kinds: Option<Vec<McpGraphEdgeKind>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CompareGraphTextArgs {
    pub pattern: String,
    pub symbol: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(default, deserialize_with = "rag_rat_core::serde_big_id::big_id_opt::deserialize")]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub symbol_path: Option<String>,
    pub resolution: Option<McpGraphResolutionMode>,
    #[serde(default = "default_compare_limit")]
    pub limit: u32,
    #[serde(default = "default_true")]
    pub include_tests: bool,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default)]
    pub include_references: bool,
    #[serde(default)]
    pub include_unresolved: bool,
    #[serde(default)]
    pub include_macros: bool,
    #[serde(default)]
    pub include_common_methods: bool,
    pub edge_kinds: Option<Vec<McpGraphEdgeKind>>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ImpactArgs {
    pub query: Option<String>,
    pub symbol: Option<String>,
    pub symbol_path: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(default, deserialize_with = "rag_rat_core::serde_big_id::big_id_opt::deserialize")]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub resolution: Option<McpGraphResolutionMode>,
    #[serde(default)]
    pub allow_ambiguous: bool,
    #[serde(default = "default_graph_limit")]
    pub limit: u32,
    #[serde(default = "default_true")]
    pub include_tests: bool,
    #[serde(default = "default_true")]
    pub include_docs: bool,
    #[serde(default = "default_true")]
    pub include_git: bool,
    #[serde(default = "default_true")]
    pub include_papertrail: bool,
    #[serde(default = "default_true")]
    pub include_text_fallback: bool,
    #[serde(default = "default_true")]
    pub include_memories: bool,
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
    #[serde(default = "default_true")]
    pub include_memories: bool,
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
    #[serde(default, deserialize_with = "rag_rat_core::serde_big_id::big_id_opt::deserialize")]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub edge_id: Option<i64>,
    pub path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub commit_hash: Option<String>,
    pub github_owner: Option<String>,
    pub github_repo: Option<String>,
    pub github_number: Option<i64>,
    #[serde(default, deserialize_with = "rag_rat_core::serde_big_id::big_id_opt::deserialize")]
    #[schemars(with = "Option<String>")]
    pub start_logical_symbol_id: Option<i64>,
    #[serde(default, deserialize_with = "rag_rat_core::serde_big_id::big_id_opt::deserialize")]
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
    pub symbol_path: Option<String>,
    // 64-bit content hash > 2^53: take it as a string so a JSON client doesn't round it (#130).
    #[serde(default, deserialize_with = "rag_rat_core::serde_big_id::big_id_opt::deserialize")]
    #[schemars(with = "Option<String>")]
    pub logical_symbol_id: Option<i64>,
    pub symbol_id: Option<i64>,
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
    #[serde(default)]
    pub include_fallback: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct HealIndexArgs {
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Default)]
pub struct EmptyArgs {}

pub fn list_tools() -> Value {
    json!(
        TOOL_NAMES
            .iter()
            .map(|name| json!({
                "name": name,
                "description": description(name),
                "inputSchema": schema(name)
            }))
            .collect::<Vec<_>>()
    )
}

pub fn call_tool(database: &Path, name: &str, arguments: Value) -> anyhow::Result<Value> {
    let db = IndexDatabase::open(database)?;
    call_tool_with_db(&db, name, arguments)
}

/// Tools that only read the index. They open a read-only connection so a concurrent writer can
/// never lock them out (#143). This is a writer DENY-list, not a reader allow-list, on purpose: a
/// newly added read tool is read-only by default (worst case: it falls back to a read-write open).
/// Every tool listed here mutates the index — keep it in sync with the write handlers in
/// `handlers.rs` (`tool_classification_covers_every_tool` guards that no tool is missed).
fn is_read_only_tool(name: &str) -> bool {
    !matches!(
        name,
        "heal_index"
            | "memory_create"
            | "memory_rebind"
            | "memory_update"
            | "memory_mark_obsolete"
            | "memory_validate"
    )
}

pub fn call_tool_for_config(
    config: &Config,
    name: &str,
    arguments: Value,
) -> anyhow::Result<Value> {
    // Read tools run on a lock-free read-only connection (#143). Two fall-backs to the read-write
    // open: (1) `try_open_config_read_only` returns `None` when the index still owes a heal/migrate
    // write; (2) a handful of read tools lazily WRITE on a cold path (`semantic_search` heals stale
    // FTS, `read_chunk` heals a stale/deleted file, `git_blame_chunk` fills the blame cache), which
    // fails on the read-only connection with `SQLITE_READONLY` — we detect that and retry the whole
    // call read-write (which performs the heal). The warm path never writes, so it stays lock-free.
    if is_read_only_tool(name)
        && let Some(db) = IndexDatabase::try_open_config_read_only(config)?
    {
        match call_tool_with_db(&db, name, arguments.clone()) {
            Ok(result) => return finalize_tool_result(config, name, result),
            Err(err) if rag_rat_core::storage::is_readonly_violation(&err) => {
                // A lazy write hit the read-only connection — fall through to the read-write open.
            },
            Err(err) => return Err(err),
        }
    }
    let db = IndexDatabase::open_config(config)?;
    let result = call_tool_with_db(&db, name, arguments)?;
    finalize_tool_result(config, name, result)
}

/// Post-process a tool result before returning it. Currently only `index_status`: surface the
/// crates.io version status so an agent can see (and relay) when a newer rag-rat is published.
/// Read-only — it reads the cached check (refreshed out of band), never the network, so the tool
/// stays fast; omitted entirely when version checking is disabled.
fn finalize_tool_result(config: &Config, name: &str, mut result: Value) -> anyhow::Result<Value> {
    if name == "index_status"
        && let Some(version) = rag_rat_core::version_check::cached_status(
            config.version_check.enabled,
            &config.database,
        )
        && let Some(obj) = result.as_object_mut()
    {
        obj.insert("version".to_string(), serde_json::json!(version));
    }
    Ok(result)
}

impl MemoryCreateArgs {
    fn core(self) -> RepoMemoryCreate {
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
    fn core(self) -> RepoMemoryUpdate {
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
            symbol_id: args.symbol_id,
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

pub fn description(name: &str) -> &'static str {
    match name {
        "semantic_search" =>
            "Search indexed source and docs. `score` is a blended relevance score combining BM25 \
             lexical rank and (when an embedding model is installed) vector cosine similarity; \
             pass explain=true for the per-component breakdown. Each hit carries `retrieval_mode` \
             ('lexical', 'vector', or 'hybrid') so you can tell whether embeddings contributed \
             without explain. Hits are validated against current source. Falls back to BM25-only \
             (every hit 'lexical') when no embedding model is present.",
        "symbol_lookup" =>
            "Resolve a symbol name (or symbol_path/id) to its definition(s) in Rust, TypeScript, \
             Kotlin, C, or C++ — exact or fuzzy. Returns candidates with signatures, locations, \
             logical-symbol grouping (cfg variants), and any bound repo memories. Use to \
             disambiguate before a graph or read call.",
        "find_callers" =>
            "Find what calls a symbol (reverse call graph), instead of grepping for call sites. \
             Returns call sites with confidence + target verification, a completeness / \
             false-positive risk summary, and repo memories crossing the call path. Resolve the \
             symbol with symbol_lookup first when a name is ambiguous.",
        "trace_callees" =>
            "Find what a symbol calls (forward call graph). Same evidence shape as find_callers; \
             unresolved std/common-method noise is filtered out by default (toggle with \
             include_common_methods / include_unresolved).",
        "compare_graph_to_text" =>
            "Cross-check a symbol's graph caller edges against a regex text search of indexed \
             source — surfaces call sites the tree-sitter graph missed and flags likely false \
             edges. Use when you suspect graph coverage gaps.",
        "compare_graph_to_scip" =>
            "Cross-check the tree-sitter graph against the SCIP compiler oracle — report the edges \
             where they DISAGREE on a callee's resolution (the compiler contradicts tree-sitter). \
             A resolver-debugging diagnostic; requires `rag-rat oracle run` first to populate \
             compiler verdicts. Reports nothing when no oracle data exists for this checkout.",
        "impact_surface" =>
            "Pre-edit blast radius for a symbol or path: graph callers/callees, tests, docs, git \
             history, GitHub papertrail, and the repo memories crossing it, with a completeness / \
             risk summary. Run this before changing anything non-trivial.",
        "repo_brief" =>
            "Orientation for an unfamiliar repo: ranked files by mode — spine (central coupling), \
             churn, god_modules, or refactor_candidates — with size/coupling/churn/memory signals \
             and suggested next tools. Start here when you don't know the codebase.",
        "repo_clusters" =>
            "Map the repo into ownership clusters from path proximity, graph edges, and git \
             co-touch — a cheap overview of subsystems and their representative files.",
        "ffi_surface" =>
            "Find the FFI surface: #[uniffi::export] items, exported impl members, and generated \
             binding artifacts (detected by path). Empty in repos without FFI.",
        "docs_for_symbol" =>
            "Find documentation related to a symbol — markdown chunks and doc comments, preferring \
             local context before broad docs.",
        "read_chunk" =>
            "Read the current source text for one chunk id, validated against HEAD (relocates or \
             flags stale/gone), with compact call-graph context and bound repo memories. Use to \
             read exact text after a search returns a chunk_id.",
        "commit_search" =>
            "Full-text search over historical commit subjects and bodies — find when/why something \
             changed by keyword.",
        "git_history_for_path" =>
            "List commits that touched a current path, newest first, with additions/deletions and \
             subjects.",
        "git_history_for_symbol" =>
            "Resolve a symbol, then list commits touching its file — symbol-scoped history without \
             needing the path.",
        "commits_touching_query" =>
            "Combine commit-message matches with current file-change evidence for a query — \"what \
             work relates to X?\" across both messages and the files that changed.",
        "git_blame_chunk" =>
            "Hash-bound git blame for one chunk: who last touched its lines, computed lazily and \
             cached against the chunk hash.",
        "papertrail_for_chunk" =>
            "The 'why' behind a chunk: its current text plus the cached GitHub issues/PRs/reviews \
             that reference it.",
        "papertrail_for_symbol" =>
            "Resolve a symbol, then return its current context plus the cached GitHub rationale \
             (issues/PRs/reviews) referencing it.",
        "papertrail_for_commit" =>
            "Cached GitHub issues/PRs/reviews related to a historical commit.",
        "github_issue_search" =>
            "Full-text search across cached GitHub issue and PR titles and bodies.",
        "github_refs_for_path" =>
            "List cached GitHub issues/PRs discovered to reference a current path.",
        "rationale_search" =>
            "Search cached GitHub rationale snippets (review comments, PR/issue discussion) by \
             keyword.",
        "local_ai_status" =>
            "On-device embedding status: model, install state, and how many chunks are embedded / \
             missing / skipped.",
        "heal_index" =>
            "Re-index stale already-indexed files and refresh FTS — repair when reads report \
             drift. Writes only to the index, never to source.",
        "github_sync_status" =>
            "GitHub papertrail cache status: counts of issues, PRs, comments, and refs, plus last \
             sync time.",
        "index_status" =>
            "Index freshness vs HEAD: git/indexed head, per-language file counts, parser failures, \
             FTS sync state, and schema version.",
        "memory_create" =>
            "Record a durable, source-anchored repo memory (Invariant / Decision / Risk / \
             BugPattern / …) bound to a symbol, chunk, path, edge/call-path, commit, or GitHub ref \
             — so the rationale resurfaces for the next agent editing that code. Capture \
             non-obvious invariants and decisions as you discover them.",
        "memory_rebind" =>
            "Re-anchor an existing repo memory to a different symbol, chunk, path, or other source \
             location — use this after a symbol moves or is renamed rather than obsoleting and \
             recreating the memory. Replaces the binding and refreshes the source_text_hash so the \
             memory stays current.",
        "memory_update" => "Update a repo memory's text, status, confidence, kind, or tags by id.",
        "memory_search" => "Full-text search across active (or stale) repo memories by keyword.",
        "memory_for_symbol" =>
            "Return repo memories bound to a symbol (or its logical-symbol group).",
        "memory_for_path" => "Return repo memories bound to a path.",
        "memory_for_call_path" =>
            "Return repo memories bound to a specific call-path edge sequence.",
        "memory_validate" =>
            "Re-anchor every repo memory against current source and mark each current / relocated \
             / stale / gone. Runs automatically after indexing.",
        "memory_mark_obsolete" =>
            "Mark a repo memory obsolete — kept for audit, hidden from active recall.",
        _ => "Unknown tool.",
    }
}

pub fn schema(name: &str) -> Value {
    match name {
        "semantic_search"
        | "commit_search"
        | "commits_touching_query"
        | "github_issue_search"
        | "rationale_search" => schema_for::<SearchArgs>(),
        "symbol_lookup" | "git_history_for_symbol" | "papertrail_for_symbol" =>
            schema_for::<SymbolArgs>(),
        "find_callers" | "trace_callees" | "docs_for_symbol" => schema_for::<SymbolGraphArgs>(),
        "compare_graph_to_text" => schema_for::<CompareGraphTextArgs>(),
        "compare_graph_to_scip" => schema_for::<EmptyArgs>(),
        "impact_surface" => schema_for::<ImpactArgs>(),
        "repo_brief" => schema_for::<RepoBriefArgs>(),
        "repo_clusters" => schema_for::<RepoClustersArgs>(),
        "ffi_surface" => schema_for::<LimitArgs>(),
        "read_chunk" => schema_for::<ReadChunkArgs>(),
        "git_history_for_path" | "github_refs_for_path" => schema_for::<PathHistoryArgs>(),
        "git_blame_chunk" => schema_for::<BlameChunkArgs>(),
        "papertrail_for_chunk" => schema_for::<PapertrailChunkArgs>(),
        "papertrail_for_commit" => schema_for::<PapertrailCommitArgs>(),
        "heal_index" => schema_for::<HealIndexArgs>(),
        "memory_create" => schema_for::<MemoryCreateArgs>(),
        "memory_rebind" => schema_for::<MemoryRebindArgs>(),
        "memory_update" => schema_for::<MemoryUpdateArgs>(),
        "memory_search" => schema_for::<MemorySearchArgs>(),
        "memory_for_symbol" => schema_for::<MemoryForSymbolArgs>(),
        "memory_for_path" => schema_for::<MemoryForPathArgs>(),
        "memory_for_call_path" => schema_for::<MemoryForCallPathArgs>(),
        "memory_mark_obsolete" => schema_for::<MemoryIdArgs>(),
        "local_ai_status" | "github_sync_status" | "index_status" | "memory_validate" =>
            schema_for::<EmptyArgs>(),
        _ => json!({"type": "object"}),
    }
}

#[cfg(test)]
mod tests;
