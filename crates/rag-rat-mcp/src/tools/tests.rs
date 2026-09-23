use std::fs;
use std::path::{Path, PathBuf};

use rag_rat_base::config::{Config, ResolvedTarget, TargetKind};
use rag_rat_base::language::Language;
use rag_rat_core::IndexDatabase;
use serde_json::json;

use super::*;

fn graph_report(completeness_risk: &str, coverage: GraphCoverage) -> GraphTraversalReport {
    use rag_rat_query::graph::{GraphTraversalQuery, GraphTraversalSummary};
    GraphTraversalReport {
        query: GraphTraversalQuery {
            tool: "find_callers".to_string(),
            symbol_id: None,
            logical_symbol_id: None,
            symbol_path: "src/lib.rs::target".to_string(),
            resolution: "syntactic".to_string(),
        },
        logical_symbol: None,
        variants: Vec::new(),
        summary: GraphTraversalSummary {
            completeness_risk: completeness_risk.to_string(),
            ..GraphTraversalSummary::default()
        },
        coverage,
        results: Vec::new(),
    }
}

#[test]
fn degraded_coverage_escalates_low_completeness_risk() {
    // issue #47: a stale/partial index can hide caller edges, so a 0-result must not read
    // as confident. `low` is escalated to `medium` when coverage is degraded.
    let stale = || GraphCoverage { stale_files: 1, ..GraphCoverage::default() };
    let mut report = graph_report("low", stale());
    escalate_risk_when_coverage_degraded(&mut report);
    assert_eq!(report.summary.completeness_risk, "medium");
    for degraded in
        [GraphCoverage { parser_failures: 1, ..GraphCoverage::default() }, GraphCoverage {
            known_index_gaps: vec!["gap".to_string()],
            ..GraphCoverage::default()
        }]
    {
        let mut report = graph_report("low", degraded);
        escalate_risk_when_coverage_degraded(&mut report);
        assert_eq!(report.summary.completeness_risk, "medium");
    }

    // Clean coverage leaves an honest `low` untouched.
    let mut clean = graph_report("low", GraphCoverage::default());
    escalate_risk_when_coverage_degraded(&mut clean);
    assert_eq!(clean.summary.completeness_risk, "low");

    // A medium/high risk is never downgraded by this path.
    let mut high = graph_report("high", GraphCoverage { stale_files: 3, ..stale() });
    escalate_risk_when_coverage_degraded(&mut high);
    assert_eq!(high.summary.completeness_risk, "high");
}

#[test]
fn compact_coverage_swaps_the_block_for_one_line_warnings() {
    let report = graph_report("medium", GraphCoverage {
        parser_failures: 2,
        stale_files: 1,
        known_index_gaps: vec!["gap".to_string()],
        ..GraphCoverage::default()
    });
    let mut value = json!(report);
    compact_graph_coverage(&mut value, &report.coverage);
    assert!(value.get("coverage").is_none(), "the full block is dropped: {value}");
    assert_eq!(
        value["coverage_warnings"],
        json!([
            "2 parser failures may affect graph coverage",
            "1 stale files may affect graph coverage",
            "1 known graph index gaps",
        ])
    );

    // Clean coverage drops the block and adds no warnings key at all.
    let clean = graph_report("low", GraphCoverage::default());
    let mut value = json!(clean);
    compact_graph_coverage(&mut value, &clean.coverage);
    assert!(value.get("coverage").is_none() && value.get("coverage_warnings").is_none());
}

#[test]
fn rationale_search_narrows_to_literal_tracker_refs_unless_fallback_is_included() {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Two cached issues whose text both match the query's words; only #42 is NAMED by it.
    for (key, title) in [("42", "The referenced issue"), ("7", "A thread that only mentions it")] {
        let item = rag_rat_papertrail::PapertrailItem {
            project: "octo/repo".to_string(),
            item_kind: rag_rat_papertrail::ItemKind::Issue,
            item_key: key.to_string(),
            url: format!("https://github.com/octo/repo/issues/{key}"),
            state: "open".to_string(),
            title: title.to_string(),
            body: "octo repo rationale".to_string(),
            author: None,
            created_at: None,
            updated_at: None,
            merged_at: None,
            closed_at: None,
            resolution: None,
            merge_commit_sha: None,
            author_kind: None,
            author_association: None,
            tags: Vec::new(),
        };
        rag_rat_papertrail::store_item(db.connection(), rag_rat_papertrail::Tracker::Github, &item)
            .unwrap();
    }
    drop(db);

    let hits = |arguments: Value| -> Vec<(String, String)> {
        let value = call_tool_for_config(&config, "rationale_search", arguments).unwrap();
        value
            .as_array()
            .expect("rationale_search answers a list")
            .iter()
            .map(|hit| {
                let field = |name: &str| hit[name].as_str().unwrap_or_default().to_string();
                (field("item_key"), field("evidence_kind"))
            })
            .collect()
    };

    // Default: the literal reference wins and the keyword-only thread is dropped.
    let narrowed = hits(json!({"query": "octo/repo#42"}));
    assert!(!narrowed.is_empty(), "the named issue is found");
    assert!(
        narrowed.iter().all(|(key, kind)| key == "42" && kind == "literal_tracker_ref"),
        "only literal tracker refs survive the default filter: {narrowed:?}"
    );

    // `fallback` keeps the keyword matches alongside it.
    let with_fallback = hits(json!({"query": "octo/repo#42", "include": ["fallback"]}));
    assert!(with_fallback.iter().any(|(key, kind)| key == "42" && kind == "literal_tracker_ref"));
    assert!(
        with_fallback.iter().any(|(key, kind)| key == "7" && kind != "literal_tracker_ref"),
        "fallback keeps the keyword-only thread: {with_fallback:?}"
    );
}

#[test]
fn arg_struct_handles_survive_an_rmcp_style_serde_round_trip() {
    // rmcp's `Parameters` extractor round-trips tool args through serialize -> deserialize, so any
    // custom serde on an arg field MUST be symmetric. A `deserialize_with` (sym_handle) without a
    // matching `serialize_with` re-emits a bare i64 on the round-trip, which the second deserialize
    // then rejects ("invalid type: integer, expected a symbol handle string") — breaking handle
    // input on the LIVE MCP server while unit tests that call `call_tool` directly (bypassing rmcp)
    // pass. This guards every arg struct that carries a `sym_<hex>` handle (#153 review).
    const HANDLE: &str = "sym_23bad57dfb79ad5f";

    macro_rules! assert_handle_round_trips {
        ($ty:ty, $field:literal, $value:expr) => {{
            let first: $ty = serde_json::from_value($value).expect("initial deserialize");
            let reserialized = serde_json::to_value(&first).expect("serialize");
            assert_eq!(
                reserialized[$field],
                HANDLE,
                "{} must re-serialize {} as the sym_<hex> token, not a bare integer",
                stringify!($ty),
                $field
            );
            // The round-trip (what rmcp does) must deserialize again without error.
            serde_json::from_value::<$ty>(reserialized).expect("round-trip deserialize");
        }};
    }

    assert_handle_round_trips!(SymbolArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(SymbolRefArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(SymbolGraphArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(CompareGraphTextArgs, "id", json!({ "pattern": "x", "id": HANDLE }));
    assert_handle_round_trips!(ImpactArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(MemoryForSymbolArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(MemoryBindArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(MemoryBindArgs, "start_id", json!({ "start_id": HANDLE }));
    assert_handle_round_trips!(MemoryBindArgs, "end_id", json!({ "end_id": HANDLE }));
    assert_handle_round_trips!(ClonesForSymbolArgs, "id", json!({ "id": HANDLE }));
}

#[test]
fn memory_bind_rejects_removed_github_fields_instead_of_dropping_the_anchor() {
    let error = serde_json::from_value::<MemoryBindArgs>(json!({
        "github_owner": "o",
        "github_repo": "r",
        "github_number": 588
    }))
    .expect_err("legacy GitHub bind fields must not deserialize as an empty bind");
    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[test]
fn include_accepts_a_json_string_encoded_array_from_buggy_clients() {
    // Some MCP clients serialize array args as JSON strings (Claude Code does this for array/object
    // params — anthropics/claude-code#24599), so `include` arrives as `"[\"git\"]"` not `["git"]`.
    // The server accepts both forms so the array surface stays usable; the schema still advertises
    // a real array (#153 review).
    let from_array: ImpactArgs = serde_json::from_value(json!({ "include": ["git"] })).unwrap();
    let from_string: ImpactArgs =
        serde_json::from_value(json!({ "include": "[\"git\"]" })).unwrap();
    assert_eq!(from_string.include, Some(vec![ImpactInclude::Git]));
    assert_eq!(from_array.include, from_string.include, "array and stringified array must agree");

    // Omitted -> None (tool defaults apply); explicit empty (either form) -> Some(empty on-set).
    assert_eq!(serde_json::from_value::<ImpactArgs>(json!({})).unwrap().include, None);
    assert_eq!(
        serde_json::from_value::<ImpactArgs>(json!({ "include": "[]" })).unwrap().include,
        Some(vec![])
    );

    // The other array params get the same tolerance: edge_kinds (Option<Vec>) and personalize
    // (Vec).
    let edges_arr: SymbolGraphArgs =
        serde_json::from_value(json!({ "edge_kinds": ["calls_name"] })).unwrap();
    let edges_str: SymbolGraphArgs =
        serde_json::from_value(json!({ "edge_kinds": "[\"calls_name\"]" })).unwrap();
    assert_eq!(edges_str.edge_kinds, Some(vec![McpGraphEdgeKind::CallsName]));
    assert_eq!(edges_arr.edge_kinds, edges_str.edge_kinds);

    let seeds_arr: ImportantSymbolsArgs =
        serde_json::from_value(json!({ "personalize": ["a", "b"] })).unwrap();
    let seeds_str: ImportantSymbolsArgs =
        serde_json::from_value(json!({ "personalize": "[\"a\",\"b\"]" })).unwrap();
    assert_eq!(seeds_str.personalize, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(seeds_arr.personalize, seeds_str.personalize);
    // personalize is non-Option: absent collapses to an empty Vec (global ranking).
    assert!(
        serde_json::from_value::<ImportantSymbolsArgs>(json!({})).unwrap().personalize.is_empty()
    );
}

/// One advertised tool's schema contract. `properties` is the exact argument set, sorted (the
/// catalog's `worktree` scoping parameter aside — its own test pins it); `include` is the exact
/// `include` vocabulary (`None` = the tool takes no `include`); `enums` pins every other
/// enum-valued argument. Keyed by tool so a tool added to `TOOL_NAMES` without a row fails.
struct SchemaRow {
    tool: &'static str,
    required: &'static [&'static str],
    properties: &'static [&'static str],
    include: Option<&'static [&'static str]>,
    enums: &'static [(&'static str, &'static [&'static str])],
}

const fn row(
    tool: &'static str,
    required: &'static [&'static str],
    properties: &'static [&'static str],
) -> SchemaRow {
    SchemaRow { tool, required, properties, include: None, enums: &[] }
}

const GRAPH_MODE: &[&str] = &["none", "compact", "full"];
const RESOLUTION: &[&str] = &["exact", "syntactic", "fuzzy"];
const EDGE_KINDS: &[&str] = &[
    "calls_name",
    "constructs",
    "uses_operator",
    "uses_precedence_group",
    "dispatches",
    "uses_macro",
    "references_type",
    "imports",
    "exports",
    "contains",
    "implements",
];
const GRAPH_INCLUDE: &[&str] =
    &["references", "unresolved", "macros", "common_methods", "coverage", "memories"];
const GRAPH_ENUMS: &[(&str, &[&str])] = &[("resolution", RESOLUTION), ("edge_kinds", EDGE_KINDS)];
const MEMORY_KIND: &[&str] = &[
    "Invariant",
    "Decision",
    "RejectedAlternative",
    "Risk",
    "BugPattern",
    "TestExpectation",
    "PerformanceNote",
    "SecurityNote",
    "FFIBoundary",
    "PlatformQuirk",
    "FollowUp",
    "OpenQuestion",
    "Obsolete",
    "Task",
    "Concept",
];
const CONFIDENCE: &[&str] = &["high", "medium", "low"];
// #149: the wire selector is symbol/ref/id (the opaque handle); the ephemeral numeric symbol_id is
// not an accepted input, which the exact property sets below pin.
const GRAPH_ARGS: &[&str] =
    &["allow_ambiguous", "edge_kinds", "id", "include", "limit", "ref", "resolution", "symbol"];
// #202 review: tools that resolve via select_symbol (always source-only) carry no `include` —
// advertising a `generated` opt-in they would silently ignore would be a lie.
const SYMBOL_REF_ARGS: &[&str] = &["allow_ambiguous", "id", "lang", "limit", "ref", "symbol"];
// The plain full-text tools honor only `query` + `limit`, never the semantic_search knobs.
const QUERY_ARGS: &[&str] = &["limit", "query"];
const PATH_ARGS: &[&str] = &["limit", "path"];

const SCHEMA_ROWS: &[SchemaRow] = &[
    SchemaRow {
        include: Some(&["generated", "git", "papertrail", "fallback"]),
        enums: &[("include_graph", GRAPH_MODE)],
        ..row("semantic_search", &["query"], &[
            "explain",
            "graph_limit",
            "include",
            "include_graph",
            "limit",
            "query",
        ])
    },
    SchemaRow {
        include: Some(&["memories", "generated"]),
        ..row("symbol_lookup", &[], &[
            "allow_ambiguous",
            "id",
            "include",
            "lang",
            "limit",
            "ref",
            "symbol",
        ])
    },
    SchemaRow {
        include: Some(GRAPH_INCLUDE),
        enums: GRAPH_ENUMS,
        ..row("find_callers", &[], GRAPH_ARGS)
    },
    SchemaRow {
        include: Some(GRAPH_INCLUDE),
        enums: GRAPH_ENUMS,
        ..row("trace_callees", &[], GRAPH_ARGS)
    },
    SchemaRow {
        include: Some(&["tests", "references", "unresolved", "macros", "common_methods"]),
        enums: GRAPH_ENUMS,
        ..row("compare_graph_to_text", &["pattern"], &[
            "allow_ambiguous",
            "edge_kinds",
            "id",
            "include",
            "limit",
            "pattern",
            "ref",
            "resolution",
            "symbol",
        ])
    },
    row("compare_graph_to_scip", &[], &[]),
    SchemaRow {
        include: Some(&["tests", "docs", "git", "papertrail", "text_fallback", "memories"]),
        enums: &[("resolution", RESOLUTION)],
        ..row("impact_surface", &[], &[
            "allow_ambiguous",
            "full_memories",
            "id",
            "include",
            "limit",
            "query",
            "ref",
            "resolution",
            "symbol",
        ])
    },
    row("check_library_usage", &[], &["deprecated_only", "limit", "package", "path"]),
    SchemaRow {
        include: Some(&["generated", "memories"]),
        enums: &[("mode", &["spine", "churn", "god_modules", "refactor_candidates"])],
        ..row("repo_brief", &[], &["include", "limit", "mode"])
    },
    SchemaRow {
        include: Some(&["generated", "memories"]),
        ..row("repo_clusters", &[], &["include", "limit", "min_cluster_size"])
    },
    row("important_symbols", &[], &["limit", "personalize"]),
    row("find_clones", &[], &[
        "id",
        "limit",
        "line",
        "min_copies",
        "min_similarity",
        "path",
        "ref",
    ]),
    row("clones_for_symbol", &[], &["id", "line", "path", "ref"]),
    row("ffi_surface", &[], &["limit"]),
    SchemaRow {
        include: Some(&["commits", "blame", "tracker", "fallback"]),
        ..row("history_for", &[], &[
            "allow_ambiguous",
            "chunk_id",
            "commit",
            "id",
            "include",
            "lang",
            "limit",
            "path",
            "ref",
            "symbol",
        ])
    },
    SchemaRow {
        include: Some(&["fallback"]),
        enums: &[("source", &["commits", "changes", "issues", "rationale"])],
        ..row("history_search", &["query", "source"], &["include", "limit", "query", "source"])
    },
    // Symbol selector only: the handler never read the graph knobs it used to advertise.
    row("docs_for_symbol", &[], SYMBOL_REF_ARGS),
    SchemaRow {
        include: Some(&["memories"]),
        enums: &[("include_graph", GRAPH_MODE)],
        ..row("read_chunk", &["chunk_id"], &["chunk_id", "graph_limit", "include", "include_graph"])
    },
    row("commit_search", &["query"], QUERY_ARGS),
    row("git_history_for_path", &["path"], PATH_ARGS),
    row("git_history_for_symbol", &[], SYMBOL_REF_ARGS),
    row("commits_touching_query", &["query"], QUERY_ARGS),
    row("git_blame_chunk", &["chunk_id"], &["chunk_id"]),
    row("papertrail_for_chunk", &["chunk_id"], &["chunk_id", "limit"]),
    row("papertrail_for_symbol", &[], SYMBOL_REF_ARGS),
    SchemaRow {
        include: Some(&["fallback"]),
        ..row("papertrail_for_commit", &["commit_hash"], &["commit_hash", "include", "limit"])
    },
    row("papertrail_issue_search", &["query"], QUERY_ARGS),
    row("papertrail_refs_for_path", &["path"], PATH_ARGS),
    SchemaRow {
        include: Some(&["fallback"]),
        ..row("rationale_search", &["query"], &["include", "limit", "query"])
    },
    row("llm_status", &[], &[]),
    row("heal_index", &[], &["limit"]),
    row("papertrail_sync_status", &[], &[]),
    SchemaRow { include: Some(&["embeddings"]), ..row("index_status", &[], &["include"]) },
    // `bind` is OPTIONAL (#463): omitting it creates an unanchored Concept/Task node.
    SchemaRow {
        enums: &[
            ("kind", MEMORY_KIND),
            ("confidence", CONFIDENCE),
            ("source", &["agent", "human", "imported", "generated"]),
        ],
        ..row("memory_create", &["kind", "title", "body", "confidence"], &[
            "bind",
            "body",
            "confidence",
            "created_by",
            "kind",
            "payload",
            "source",
            "tags",
            "title",
        ])
    },
    row("memory_rebind", &["memory_id", "bind"], &["bind", "memory_id"]),
    SchemaRow {
        enums: &[
            ("kind", MEMORY_KIND),
            ("confidence", CONFIDENCE),
            ("status", &["active", "stale", "obsolete", "rejected"]),
        ],
        ..row("memory_update", &["memory_id"], &[
            "bind",
            "body",
            "confidence",
            "kind",
            "memory_id",
            "payload",
            "status",
            "tags",
            "title",
        ])
    },
    row("memory_search", &["query"], QUERY_ARGS),
    row("memory_get", &[], &[
        "allow_ambiguous",
        "edge_sequence_hash",
        "id",
        "limit",
        "memory_id",
        "path",
        "ref",
        "symbol",
    ]),
    row("memory_for_symbol", &[], &["allow_ambiguous", "id", "limit", "ref", "symbol"]),
    row("memory_for_path", &["path"], PATH_ARGS),
    row("memory_for_call_path", &["edge_sequence_hash"], &["edge_sequence_hash", "limit"]),
    row("memory_show", &["memory_id"], &["memory_id"]),
    row("memory_validate", &[], &[]),
    row("memory_doctor", &[], &[]),
    row("memory_mark_obsolete", &["memory_id"], &["memory_id"]),
    SchemaRow {
        enums: &[("relation", &[
            "depends_on",
            "relates_to",
            "supersedes",
            "derived_from",
            "tracks",
        ])],
        ..row("memory_edge_add", &["source_node_id", "relation"], &[
            "github_number",
            "github_owner",
            "github_repo",
            "relation",
            "source_node_id",
            "target_node_id",
            "target_repo_id",
        ])
    },
    row("memory_edge_remove", &["edge_key"], &["edge_key"]),
    SchemaRow {
        enums: &[("direction", &["from", "into"])],
        ..row("memory_edges", &["direction"], &[
            "direction",
            "github_number",
            "github_owner",
            "github_repo",
            "node_id",
        ])
    },
    row("dream", &[], &["all", "limit"]),
    SchemaRow {
        enums: &[("verdict", &["accept", "dismiss", "reset"])],
        ..row("dream_review", &["finding", "verdict"], &["finding", "verdict"])
    },
];

#[test]
fn list_tools_exposes_complete_typed_schemas() {
    let tools = list_tools();
    let tools = tools.as_array().expect("tools/list shape");
    for row in SCHEMA_ROWS {
        assert!(TOOL_NAMES.contains(&row.tool), "row for unlisted tool {}", row.tool);
    }
    for name in TOOL_NAMES {
        let row = SCHEMA_ROWS
            .iter()
            .find(|row| row.tool == *name)
            .unwrap_or_else(|| panic!("{name}: no schema row — add one to SCHEMA_ROWS"));
        let schema = tool_schema(tools, name);
        assert_eq!(schema["type"], "object", "{name}: arguments are an object");

        let mut properties: Vec<&str> = schema["properties"]
            .as_object()
            .map(|properties| {
                properties.keys().map(String::as_str).filter(|key| *key != "worktree").collect()
            })
            .unwrap_or_default();
        properties.sort_unstable();
        assert_eq!(properties, row.properties, "{name}: advertised arguments");

        let mut required = schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|field| field.as_str().expect("required property name"))
            .collect::<Vec<_>>();
        required.sort_unstable();
        let mut expected_required = row.required.to_vec();
        expected_required.sort_unstable();
        assert_eq!(required, expected_required, "{name}: required arguments");
        for field in &required {
            assert!(properties.contains(field), "{name} requires `{field}` but does not define it");
        }

        for field in ["include", "edge_kinds"] {
            if schema["properties"].get(field).is_some() {
                assert_schema_array_property(schema, field);
            }
        }

        let owned = |values: &[&str]| values.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(enum_values(schema, "include"), row.include.map(owned), "{name}.include");
        for property in properties.iter().filter(|property| **property != "include") {
            let pinned = row.enums.iter().find(|(pinned, _)| pinned == property);
            assert_eq!(
                enum_values(schema, property),
                pinned.map(|(_, values)| owned(values)),
                "{name}.{property}: every enum-valued argument is pinned, and only those"
            );
        }
    }
    for field in ["edge_id", "edge_sequence_hash", "path_summary"] {
        assert_schema_nested_property(tools, "memory_create", "bind", field);
    }
}

fn assert_schema_array_property(root: &Value, field: &str) {
    let property = resolve_schema_ref(root, &root["properties"][field]);
    property
        .get("items")
        .or_else(|| {
            property
                .get("anyOf")?
                .as_array()?
                .iter()
                .map(|candidate| resolve_schema_ref(root, candidate))
                .find(|candidate| candidate["type"] == "array")?
                .get("items")
        })
        .expect("array items schema");
}

#[test]
#[should_panic(expected = "array items schema")]
fn schema_array_guard_rejects_a_scalar_with_the_same_vocabulary() {
    assert_schema_array_property(
        &json!({
            "properties": {"include": {"type": "string", "enum": ["memories", "generated"]}}
        }),
        "include",
    );
}

/// The value vocabulary `property` advertises — a string enum, directly or as an array's items,
/// through `$ref`, a nullable `anyOf`, or a documented-variant `oneOf` — or `None` when the
/// property is absent or not enum-valued.
fn enum_values(root: &Value, property: &str) -> Option<Vec<String>> {
    enum_vocabulary(root, root["properties"].get(property)?)
}

fn enum_vocabulary(root: &Value, schema: &Value) -> Option<Vec<String>> {
    let schema = resolve_schema_ref(root, schema);
    let schema = schema.get("items").map_or(schema, |items| resolve_schema_ref(root, items));
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return Some(
            values.iter().map(|value| value.as_str().expect("string enum value").into()).collect(),
        );
    }
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array) {
        return variants
            .iter()
            .map(|variant| variant.get("const").and_then(Value::as_str).map(str::to_string))
            .collect();
    }
    schema
        .get("anyOf")?
        .as_array()?
        .iter()
        .filter(|candidate| candidate.get("type").and_then(Value::as_str) != Some("null"))
        .find_map(|candidate| enum_vocabulary(root, candidate))
}

#[test]
fn enum_like_tool_args_reject_unknown_values_during_decoding() {
    let err = serde_json::from_value::<SearchArgs>(json!({
        "query": "alpha",
        "include_graph": "auto"
    }))
    .unwrap_err()
    .to_string();
    assert!(err.contains("expected none, compact, or full"), "{err}");

    let err = serde_json::from_value::<SymbolGraphArgs>(json!({
        "symbol": "alpha",
        "resolution": "maybe"
    }))
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown variant"), "{err}");

    let err = serde_json::from_value::<SymbolGraphArgs>(json!({
        "symbol": "alpha",
        "edge_kinds": ["calls_name", "bogus"]
    }))
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown variant"), "{err}");
}

#[test]
fn index_status_surfaces_version_when_cached_and_enabled() {
    let (_root, config) = mixed_config();
    IndexDatabase::rebuild(&config).unwrap();
    // Seed a clearly-newer cached crates.io result into the combined sidecar store (the network
    // refresh that normally writes this is out of band).
    rag_rat_core::sidecar_state::write_version_cache(
        &config.database,
        &rag_rat_core::version_check::CachedVersion {
            latest_version: "99.0.0".into(),
            checked_at_ms: 1,
        },
    );

    let status = call_tool_for_config(&config, "index_status", json!({})).unwrap();
    let version = status.get("version").expect("index_status surfaces a version field");
    assert_eq!(version["current_version"], rag_rat_core::version_check::current_version());
    assert_eq!(version["latest_version"], "99.0.0");
    assert_eq!(version["update_available"], true);
    assert_eq!(version["update_command"], "cargo install rag-rat --force");
}

#[test]
fn mcp_tool_calls_preserve_compatibility_shapes() {
    let (_root, config) = mixed_config();
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    let search =
        call_tool_for_config(&config, "semantic_search", json!({"query": "alpha"})).unwrap();
    let hit = search.as_array().unwrap().first().expect("semantic hit");
    for field in ["chunk_id", "path", "start_line", "end_line", "summary", "score"] {
        assert!(hit.get(field).is_some(), "semantic_search missing {field}");
    }
    let chunk_id = hit["chunk_id"].as_i64().unwrap();

    let chunk = call_tool_for_config(&config, "read_chunk", json!({"chunk_id": chunk_id})).unwrap();
    for field in ["chunk_id", "path", "start_line", "end_line", "text"] {
        assert!(chunk.get(field).is_some(), "read_chunk missing {field}");
    }

    let status = call_tool(&config.database, "index_status", json!({})).unwrap();
    assert!(status["database"].as_str().unwrap().ends_with("index.sqlite"));
    assert_eq!(status["fts_fresh"], true);
    // index_status trims the embedded llm block (use llm_status) and the static
    // migration ledger (use the CLI doctor/migrate).
    assert!(status.get("llm").is_none(), "llm should not be embedded in index_status");
    assert!(
        status["schema"].get("migrations").is_none(),
        "migration ledger should be trimmed from index_status"
    );

    let papertrail = call_tool(
        &config.database,
        "papertrail_for_symbol",
        json!({"symbol": "alpha_symbol", "language": "rust"}),
    )
    .unwrap();
    assert!(papertrail["current_source"].is_object());
    assert!(papertrail["evidence"].is_array());

    let sync_status = call_tool(&config.database, "papertrail_sync_status", json!({})).unwrap();
    assert!(sync_status["capabilities"].is_array());

    let llm = call_tool(&config.database, "llm_status", json!({})).unwrap();
    assert_eq!(llm["embedding"]["state"], "MissingModel");
}

#[test]
fn mcp_edge_tools_add_traverse_and_remove() {
    let (_root, config) = mixed_config();
    IndexDatabase::rebuild(&config).unwrap();

    let create = |title: &str| -> String {
        let v = call_tool_for_config(
            &config,
            "memory_create",
            json!({"kind": "Task", "title": title, "body": "b", "confidence": "low", "bind": {}}),
        )
        .unwrap();
        v["memory"]["memory_id"].as_str().unwrap().to_string()
    };
    let a = create("task a");
    let b = create("task b");

    // add a --depends_on--> b via the MCP surface; forward + reverse traversal see it.
    let edge = call_tool_for_config(
        &config,
        "memory_edge_add",
        json!({"source_node_id": a, "relation": "depends_on", "target_node_id": b}),
    )
    .unwrap();
    assert_eq!(edge["relation"], "depends_on");
    let key = edge["edge_key"].as_str().unwrap().to_string();
    let from =
        call_tool_for_config(&config, "memory_edges", json!({"direction": "from", "node_id": a}))
            .unwrap();
    assert_eq!(from.as_array().unwrap().len(), 1);
    let into =
        call_tool_for_config(&config, "memory_edges", json!({"direction": "into", "node_id": b}))
            .unwrap();
    assert_eq!(into.as_array().unwrap().len(), 1);

    // a `tracks` github edge, reachable by the REVERSE traversal on the github ref.
    call_tool_for_config(
        &config,
        "memory_edge_add",
        json!({"source_node_id": a, "relation": "tracks",
               "github_owner": "o", "github_repo": "r", "github_number": 5}),
    )
    .unwrap();
    let tracking = call_tool_for_config(
        &config,
        "memory_edges",
        json!({"direction": "into", "github_owner": "o", "github_repo": "r", "github_number": 5}),
    )
    .unwrap();
    assert_eq!(tracking.as_array().unwrap().len(), 1);
    assert_eq!(tracking[0]["source_node_id"], a);

    // remove by edge_key.
    assert_eq!(
        call_tool_for_config(&config, "memory_edge_remove", json!({"edge_key": key})).unwrap(),
        json!(true)
    );
}

#[test]
fn mcp_memory_tools_create_surface_validate_and_obsolete_symbol_memory() {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "#[cfg(unix)]\npub fn cfg_helper() {}\n#[cfg(windows)]\npub fn cfg_helper() {}\n",
    )
    .unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    let lookup = call_tool(
        &config.database,
        "symbol_lookup",
        json!({"symbol": "cfg_helper", "allow_ambiguous": true}),
    )
    .unwrap();
    // logical_symbol_id crosses the MCP boundary as a STRING (#130: a 64-bit hash > 2^53 can't be a
    // JSON number without rounding). Read it as a string and pass it straight back to the other
    // tools — the round-trip the fix guarantees.
    let logical_symbol_id = lookup["candidates"].as_array().unwrap()[0]["id"].as_str().unwrap();
    let memory = call_tool(
            &config.database,
            "memory_create",
            json!({
                "kind": "Invariant",
                "title": "Treat cfg helper variants as one logical helper",
                "body": "Caller and impact analysis should use the logical symbol, not one cfg body variant.",
                "confidence": "high",
                "created_by": "mcp-test",
                "tags": ["cfg", "graph"],
                "bind": {"id": logical_symbol_id}
            }),
        )
        .unwrap();
    assert_eq!(memory["duplicate"], false);
    let memory_id = memory["memory"]["memory_id"].as_str().unwrap();

    let for_symbol =
        call_tool(&config.database, "memory_for_symbol", json!({"id": logical_symbol_id})).unwrap();
    assert_eq!(for_symbol.as_array().unwrap()[0]["memory_id"], memory_id);
    let search =
        call_tool(&config.database, "memory_search", json!({"query": "logical helper"})).unwrap();
    assert_eq!(search.as_array().unwrap()[0]["memory_id"], memory_id);
    let path_like_search =
        call_tool(&config.database, "memory_search", json!({"query": "follow-up/src/lib.rs"}))
            .unwrap();
    assert!(path_like_search.is_array());

    let chunk_id =
        memory["memory"]["bindings"].as_array().unwrap()[0]["chunk_id"].as_i64().unwrap();
    let chunk = call_tool(
        &config.database,
        "read_chunk",
        json!({"chunk_id": chunk_id, "include": ["memories"]}),
    )
    .unwrap();
    assert_eq!(chunk["memories"].as_array().unwrap()[0]["memory_id"], memory_id);

    let impact = call_tool(
        &config.database,
        "impact_surface",
        json!({"id": logical_symbol_id, "include": ["memories"]}),
    )
    .unwrap();
    assert_eq!(impact["repo_memories"]["direct"].as_array().unwrap()[0]["memory_id"], memory_id);
    assert_eq!(impact["completeness_and_caveats"]["memory_status"]["active"], 1);

    // symbol_lookup must still attach bound memories to each candidate. The enrichment resolves
    // them from the in-memory hit's internal symbol_id, which no longer crosses the wire (#149/#153
    // review) — reading it back off the serialized candidate would find nothing and drop them all.
    let enriched = call_tool(
        &config.database,
        "symbol_lookup",
        json!({"symbol": "cfg_helper", "allow_ambiguous": true}),
    )
    .unwrap();
    let candidate_memories = enriched["candidates"].as_array().unwrap()[0]["memories"]
        .as_array()
        .expect("symbol_lookup candidate should carry its bound memory");
    assert_eq!(candidate_memories[0]["memory_id"], memory_id);

    let validation = call_tool(&config.database, "memory_validate", json!({})).unwrap();
    assert_eq!(validation["current"], 1);
    let obsolete =
        call_tool(&config.database, "memory_mark_obsolete", json!({"memory_id": memory_id}))
            .unwrap();
    assert_eq!(obsolete["status"], "obsolete");
}

#[test]
fn mcp_dream_surfaces_a_finding_and_review_applies_a_verdict() {
    // #263: the pull-based dream surface over MCP. `dream` recomputes the deterministic worklist
    // (a WRITE tool — it syncs `dream_findings`, like `rag-rat dream`); `dream_review` applies a
    // human verdict. Seed a `stale_reference` finding by binding a memory whose body cites a `.rs`
    // path that does NOT resolve against the index — a deterministic, single-finding trigger that
    // needs no model.
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    let memory = call_tool(
        &config.database,
        "memory_create",
        json!({
            "kind": "Risk",
            "title": "Cites a path that has since moved",
            "body": "The old logic lived in crates/ghost/src/vanished.rs before the refactor.",
            "confidence": "medium",
            "created_by": "dream-test",
            "bind": {"path": "src/lib.rs"}
        }),
    )
    .unwrap();
    let memory_id = memory["memory"]["memory_id"].as_str().unwrap().to_string();

    // The default worklist surfaces the OPEN stale_reference finding (its subject is the memory
    // id).
    let report = call_tool(&config.database, "dream", json!({})).unwrap();
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "stale_reference" && f["subject"] == memory_id.as_str())
        .expect("dream should surface the stale_reference finding")
        .clone();
    assert_eq!(finding["status"], "open");
    let finding_id = finding["id"].as_str().unwrap().to_string();

    // Accept it: the verdict is echoed back, and the finding leaves the default (open) worklist.
    let reviewed = call_tool(
        &config.database,
        "dream_review",
        json!({"finding": finding_id, "verdict": "accept"}),
    )
    .unwrap();
    assert_eq!(reviewed["status"], "accepted");
    assert_eq!(reviewed["id"], finding_id.as_str());

    // Re-running `dream` recomputes stale_reference (the bad path is still cited) — the refresh
    // branch PRESERVES the accepted verdict, so the finding stays out of the open worklist.
    let after = call_tool(&config.database, "dream", json!({})).unwrap();
    assert!(
        !after["findings"].as_array().unwrap().iter().any(|f| f["id"] == finding_id.as_str()),
        "an accepted finding must drop out of the default (open) worklist"
    );
    // `all: true` brings the reviewed finding back, now marked accepted.
    let all = call_tool(&config.database, "dream", json!({"all": true})).unwrap();
    let seen = all["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == finding_id.as_str())
        .expect("`all: true` lists the accepted finding");
    assert_eq!(seen["status"], "accepted");

    // reset clears the verdict, returning the finding to the open worklist.
    let reset = call_tool(
        &config.database,
        "dream_review",
        json!({"finding": finding_id, "verdict": "reset"}),
    )
    .unwrap();
    assert_eq!(reset["status"], "open");
}

#[test]
fn mcp_dream_defaults_when_arguments_are_omitted() {
    // #514: a JSON-RPC tools/call with the `arguments` object OMITTED reaches the dispatcher as
    // Value::Null (server maps None -> Null). A bare `dream` call — all schema fields optional —
    // must still apply its `limit`/`all` defaults, not error deserializing Null into DreamArgs.
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn anchor() {}\n").unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    let report = call_tool(&config.database, "dream", json!(null)).unwrap();
    assert!(
        report["findings"].is_array(),
        "a bare `dream` call (omitted arguments) returns a worklist: {report}"
    );
}

#[test]
fn mcp_read_chunk_and_heal_index_do_not_return_stale_text() {
    let (root, config) = markdown_config("# Title\nalpha token\n");
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    let search = call_tool(&config.database, "semantic_search", json!({"query": "alpha"})).unwrap();
    let chunk_id = search.as_array().unwrap()[0]["chunk_id"].as_i64().unwrap();
    fs::write(root.join("docs/search.md"), "inserted\n# Title\nalpha token\n").unwrap();

    let chunk = call_tool(&config.database, "read_chunk", json!({"chunk_id": chunk_id})).unwrap();
    assert_eq!(chunk["start_line"], 2);
    assert_eq!(chunk["text"], "# Title\nalpha token\n");

    fs::write(root.join("docs/search.md"), "# Changed\nbeta token\n").unwrap();
    let report = call_tool_for_config(&config, "heal_index", json!({"limit": 10})).unwrap();
    assert_eq!(report["healed_files"], 1);
    assert_eq!(report["fts_fresh"], true);

    let stale =
        call_tool_for_config(&config, "semantic_search", json!({"query": "alpha"})).unwrap();
    assert!(stale.as_array().unwrap().is_empty());
    let fresh = call_tool_for_config(&config, "semantic_search", json!({"query": "beta"})).unwrap();
    assert_eq!(fresh.as_array().unwrap().len(), 1);
}

#[test]
fn mcp_handle_selection_disambiguates_graph_tools() {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub mod one;\npub mod two;\n").unwrap();
    fs::write(
        root.join("src/one.rs"),
        "pub fn shared() {}\n//     shared(\npub fn caller_one() {\n    shared();\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/two.rs"),
        "pub fn shared() {}\npub fn caller_two() {\n    shared();\n}\n",
    )
    .unwrap();
    let config = rust_config(root.to_path_buf());
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);

    let lookup = call_tool(&config.database, "symbol_lookup", json!({"symbol": "shared"})).unwrap();
    assert_eq!(lookup["disambiguation_required"], true);
    let candidates = lookup["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    // #149: candidates carry the opaque `sym_<hex>` handle (not the ephemeral numeric symbol_id),
    // plus the human-readable ref (symbol_path).
    assert!(candidates.iter().all(|candidate| {
        candidate.get("symbol_id").is_none()
            && candidate["id"].as_str().is_some_and(|h| h.starts_with("sym_"))
            && candidate["ref"].as_str().is_some()
    }));

    let ambiguous =
        call_tool(&config.database, "find_callers", json!({"symbol": "shared"})).unwrap();
    assert_eq!(ambiguous["disambiguation_required"], true);
    assert_eq!(ambiguous["candidates"].as_array().unwrap().len(), 2);

    let one = candidates
        .iter()
        .find(|candidate| candidate["ref"].as_str().unwrap().contains("one.rs"))
        .unwrap();
    let exact = call_tool(
        &config.database,
        "find_callers",
        json!({
            "id": one["id"].as_str().unwrap(),
            "resolution": "exact",
            "edge_kinds": ["calls_name"]
        }),
    )
    .unwrap();
    assert_eq!(exact["query"]["tool"], "find_callers");
    assert_eq!(exact["query"]["id"], one["id"]);
    assert_eq!(exact["query"]["resolution"], "exact");
    assert_eq!(exact["summary"]["returned_count"], 1);
    assert_eq!(exact["summary"]["total_matching_edges"], 1);
    assert_eq!(exact["summary"]["truncated"], false);
    assert_eq!(exact["summary"]["exact_verified"], 1);
    assert_eq!(exact["summary"]["false_positive_risk"], "low");
    assert_eq!(exact["summary"]["completeness_risk"], "low");
    assert!(exact.get("coverage").is_none());
    assert!(exact.get("coverage_warnings").is_none());
    let exact_with_coverage = call_tool(
        &config.database,
        "find_callers",
        json!({
            "id": one["id"].as_str().unwrap(),
            "resolution": "exact",
            "edge_kinds": ["calls_name"],
            "include": ["coverage"]
        }),
    )
    .unwrap();
    assert!(
        !exact_with_coverage["coverage"]["parser_coverage_for_paths"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let exact_results = exact["results"].as_array().unwrap();
    assert_eq!(exact_results.len(), 1, "exact callers: {exact:?}");
    assert_eq!(exact_results[0]["verified_target_symbol"], true);
    assert!(exact_results[0]["from_symbol"].as_str().unwrap().contains("caller"));

    let comparison = call_tool(
        &config.database,
        "compare_graph_to_text",
        json!({
            "id": one["id"].as_str().unwrap(),
            "pattern": "    shared\\(",
            "resolution": "exact",
            "edge_kinds": ["calls_name"]
        }),
    )
    .unwrap();
    assert_eq!(comparison["query"]["id"], one["id"]);
    assert_eq!(comparison["summary"]["graph_edges"], 1);
    assert_eq!(comparison["summary"]["graph_hits"], 1);
    assert_eq!(comparison["summary"]["text_hits"], 3);
    assert_eq!(comparison["summary"]["matched"], 1);
    assert_eq!(comparison["summary"]["text_only"], 2);
    assert_eq!(comparison["summary"]["text_mentions"], 1);
    assert_eq!(comparison["summary"]["likely_parser_gaps"], 1);
    assert_eq!(comparison["summary"]["likely_index_gaps"], 1);
    assert_eq!(comparison["summary"]["graph_only"], 0);
    assert_eq!(comparison["summary"]["complete"], false);
    assert_eq!(comparison["summary"]["recommended_fallback"], "text");
    assert_eq!(comparison["summary"]["pattern_match_mode"], "identifier_or_call");
    assert!(comparison["summary"]["warnings"].as_array().unwrap().is_empty());
    assert_eq!(comparison["matched_hits"].as_array().unwrap().len(), 1);
    assert_eq!(comparison["text_only_hits"].as_array().unwrap().len(), 2);
    assert_eq!(comparison["likely_parser_gaps"].as_array().unwrap().len(), 1);
    assert!(
        comparison["text_only_hits"].as_array().unwrap().iter().any(|hit| {
            hit["likely_gap"].as_str() == Some("comment_text_mention")
                && hit["reason"].as_str() == Some("text mention outside graph-call evidence")
        }),
        "comment text hits should not be promoted to parser gaps: {comparison:?}"
    );

    let substring_comparison = call_tool(
        &config.database,
        "compare_graph_to_text",
        json!({
            "id": one["id"].as_str().unwrap(),
            "pattern": "shared",
            "resolution": "exact",
            "edge_kinds": ["calls_name"]
        }),
    )
    .unwrap();
    assert_eq!(substring_comparison["summary"]["pattern_match_mode"], "substring_identifier");
    assert!(
        !substring_comparison["summary"]["warnings"].as_array().unwrap().is_empty(),
        "substring comparison should warn: {substring_comparison:?}"
    );
    assert_eq!(comparison["likely_parser_gaps"].as_array().unwrap().len(), 1);

    let impact = call_tool(
        &config.database,
        "impact_surface",
        json!({
            "id": one["id"].as_str().unwrap(),
            "resolution": "exact",
            "include": ["tests", "docs", "git", "papertrail", "text_fallback"]
        }),
    )
    .unwrap();
    assert_eq!(impact["query"]["ref"], one["ref"]);
    assert_eq!(impact["query"]["resolution"], "exact");
    assert!(impact["direct_semantic_callers"].as_array().unwrap().len() == 1);
    assert!(impact["direct_semantic_callees"].as_array().unwrap().is_empty());
    assert!(impact["text_fallback_hits"].is_array());
    assert!(
        impact["completeness_and_caveats"]["caveats"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().is_some_and(|value| value.contains("tree-sitter/syntactic")))
    );

    let papertrail = call_tool(
        &config.database,
        "papertrail_for_symbol",
        json!({"id": one["id"].as_str().unwrap()}),
    )
    .unwrap();
    assert!(papertrail["current_source"]["symbol"].as_str().unwrap().contains("shared"));
}

#[test]
fn find_callers_zero_callers_is_not_low_completeness() {
    // #200: a symbol with no static callers can't be reported `low` completeness — a static graph
    // can't see callers reached via message/enum dispatch, dynamic dispatch, trait objects, FFI, or
    // reflection. find_callers on a call-less fn must escalate to >= medium and attach a note.
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    // `orphan` is never called; `caller` calls `callee` so the graph is otherwise healthy.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn orphan() {}\npub fn callee() {}\npub fn caller() {\n    callee();\n}\n",
    )
    .unwrap();
    let config = rust_config(root.to_path_buf());
    IndexDatabase::rebuild(&config).unwrap();

    let orphan = call_tool(&config.database, "find_callers", json!({"symbol": "orphan"})).unwrap();
    assert_eq!(orphan["summary"]["returned_count"], 0);
    assert_ne!(
        orphan["summary"]["completeness_risk"], "low",
        "0 callers must not read low: {orphan:?}"
    );
    assert!(
        orphan["summary"]["completeness_note"].as_str().is_some_and(|n| n.contains("dispatch")),
        "0 callers must carry a hidden-edge note: {orphan:?}"
    );

    // A symbol WITH a resolved caller keeps its honest low risk and no note.
    let callee = call_tool(&config.database, "find_callers", json!({"symbol": "callee"})).unwrap();
    assert_eq!(callee["summary"]["returned_count"], 1);
    assert_eq!(callee["summary"]["completeness_risk"], "low");
    assert!(callee["summary"]["completeness_note"].is_null());
}

fn assert_schema_nested_property(tools: &[Value], name: &str, parent: &str, field: &str) {
    let schema = tool_schema(tools, name);
    let property = schema["properties"].get(parent).expect("schema property");
    let resolved = resolve_schema_ref(schema, property);
    assert!(resolved["properties"].get(field).is_some(), "{name}.{parent} should define {field}");
}

fn resolve_schema_ref<'a>(root: &'a Value, value: &'a Value) -> &'a Value {
    let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
        return value;
    };
    let Some(definition) = reference.strip_prefix("#/$defs/") else {
        return value;
    };
    &root["$defs"][definition]
}

#[test]
fn mcp_rewrites_ranking_hint_when_auto_run_enabled() {
    // #142 review: with `[oracle] auto_run` on and no oracle data yet, the important_symbols nudge
    // must say compiler ranking refreshes in the background — not tell the agent to run `oracle
    // run` by hand. The core query is config-unaware, so call_tool_for_config applies the
    // rewrite.
    let (_root, mut config) = mixed_config();
    IndexDatabase::rebuild(&config).unwrap();

    config.oracle.auto_run = true;
    let auto = call_tool_for_config(&config, "important_symbols", json!({"limit": 5})).unwrap();
    assert_eq!(
        auto["ranking_hint"].as_str(),
        Some(rag_rat_query::pagerank::RANKING_HINT_AUTO_RUN),
        "auto_run must rewrite the heuristic nudge: {auto:?}"
    );

    // With auto_run OFF the default manual-run nudge is preserved.
    config.oracle.auto_run = false;
    let manual = call_tool_for_config(&config, "important_symbols", json!({"limit": 5})).unwrap();
    assert_eq!(
        manual["ranking_hint"].as_str(),
        Some(rag_rat_query::pagerank::RANKING_HINT_RUN_ORACLE),
    );
}

fn tool_schema<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .map(|tool| &tool["inputSchema"])
        .expect("tool schema")
}

fn mixed_config() -> (rag_rat_base::test_scratch::ScratchDir, Config) {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("docs/search.md"), "# Title\nalpha token\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn alpha_symbol() {}\n").unwrap();
    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![
            ResolvedTarget {
                name: "markdown".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            },
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
        ],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        mcp: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    (root, config)
}

fn markdown_config(text: &str) -> (rag_rat_base::test_scratch::ScratchDir, Config) {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/search.md"), text).unwrap();
    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![ResolvedTarget {
            name: "markdown".to_string(),
            language: Language::Markdown,
            directories: vec![PathBuf::from("docs")],
            include: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Docs,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        mcp: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    (root, config)
}

fn rust_config(root: PathBuf) -> Config {
    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        mcp: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}

fn unique_temp_root() -> rag_rat_base::test_scratch::ScratchDir {
    rag_rat_base::test_scratch::ScratchDir::new("mcp-test")
}

fn git(root: &Path, args: &[&str]) {
    rag_rat_base::test_git::run(root, args);
}

fn candidate_count(value: &Value) -> usize {
    value.get("candidates").and_then(Value::as_array).map_or(0, Vec::len)
}

#[test]
fn worktree_arg_prefers_request_then_falls_back_to_cwd() {
    let cwd = Some(PathBuf::from("/server/cwd"));
    // Explicit request field wins.
    assert_eq!(
        worktree_arg_or_cwd(&json!({"worktree": "/explicit"}), cwd.clone()),
        Some(PathBuf::from("/explicit"))
    );
    // Absent / blank request field → fall back to the server cwd (validated downstream).
    assert_eq!(worktree_arg_or_cwd(&json!({}), cwd.clone()), Some(PathBuf::from("/server/cwd")));
    assert_eq!(worktree_arg_or_cwd(&json!({"worktree": "  "}), cwd.clone()), cwd);
    // No request field and no cwd → None (base scope).
    assert_eq!(worktree_arg_or_cwd(&json!({}), None), None);
}

#[test]
fn worktree_param_routes_query_to_branch_overlay() {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let config = rust_config(root.to_path_buf());
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    git(&root, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    git(&linked, &["add", "."]);
    git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    drop(db);

    let linked_str = linked.to_str().unwrap();
    // With the `worktree` param the query resolves against that worktree's branch overlay.
    let hit = call_tool_for_config(
        &config,
        "symbol_lookup",
        json!({"symbol": "linked_fn", "worktree": linked_str}),
    )
    .unwrap();
    assert!(candidate_count(&hit) > 0, "worktree-scoped lookup finds the branch symbol");

    // Without it the base scope is queried — the branch symbol isn't there.
    let base =
        call_tool_for_config(&config, "symbol_lookup", json!({"symbol": "linked_fn"})).unwrap();
    assert_eq!(candidate_count(&base), 0, "base scope does not see the branch symbol");

    // And within the worktree scope the overlay shadows the base symbol.
    let shadowed = call_tool_for_config(
        &config,
        "symbol_lookup",
        json!({"symbol": "base_fn", "worktree": linked_str}),
    )
    .unwrap();
    assert_eq!(
        candidate_count(&shadowed),
        0,
        "the overlay shadows the base symbol in the worktree scope"
    );
}

/// Add a linked worktree on `branch` whose `src/a.rs` defines `symbol`, and index its overlay.
fn linked_worktree_with(
    config: &Config,
    root: &Path,
    branch: &str,
    symbol: &str,
) -> rag_rat_base::test_scratch::ScratchDir {
    let linked = unique_temp_root();
    git(root, &["worktree", "add", "-q", "-b", branch, linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), format!("pub fn {symbol}() {{}}\n")).unwrap();
    git(&linked, &["add", "."]);
    git(&linked, &["commit", "-q", "-m", branch]);
    let mut db = IndexDatabase::open_config(config).unwrap();
    db.index_worktree_overlay(config, &linked, &mut |_| {}).unwrap();
    linked
}

/// The scoping field `name` ADVERTISES, read back out of the generated schema so the calls below
/// drive the declared name. A client can only pass what the catalog declares, so an undeclared
/// parameter is an unusable one however faithfully the dispatcher honors it (#1201).
fn advertised_worktree_param(name: &str) -> String {
    let schema = schema(name);
    let (param, _) = schema["properties"]
        .as_object()
        .and_then(|properties| properties.get_key_value("worktree"))
        .unwrap_or_else(|| panic!("{name} advertises the worktree parameter"));
    param.clone()
}

#[test]
fn the_advertised_worktree_param_serves_that_checkout_and_not_its_siblings() {
    let param = advertised_worktree_param("symbol_lookup");
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let config = rust_config(root.to_path_buf());
    IndexDatabase::rebuild(&config).unwrap();

    let first = linked_worktree_with(&config, &root, "feat-a", "first_fn");
    let second = linked_worktree_with(&config, &root, "feat-b", "second_fn");
    let (first, second) = (first.to_str().unwrap(), second.to_str().unwrap());
    let call = |tool: &str, mut args: Value, worktree: Option<&str>| {
        if let Some(worktree) = worktree {
            args[param.as_str()] = json!(worktree);
        }
        call_tool_for_config(&config, tool, args).unwrap()
    };
    let found = |symbol: &str, worktree: Option<&str>| {
        candidate_count(&call("symbol_lookup", json!({"symbol": symbol}), worktree)) > 0
    };

    // Active-checkout scope: the requested worktree's overlay shadows the base file.
    assert!(found("first_fn", Some(first)), "the scoped checkout's own symbol");
    assert!(!found("base_fn", Some(first)), "the overlay shadows the base file");
    // Sibling isolation: the OTHER linked checkout's overlay must not leak into this scope.
    assert!(!found("second_fn", Some(first)), "a sibling checkout's symbol must not");
    assert!(found("second_fn", Some(second)), "each checkout serves its own overlay");

    // Unscoped: the base scope, with neither overlay visible.
    assert!(found("base_fn", None), "unscoped reads serve the base checkout");
    assert!(!found("first_fn", None));
    assert!(!found("second_fn", None));

    // A tool a client could NOT scope until the catalog declared the parameter for it:
    // `SymbolRefArgs` has no `worktree` field of its own. It resolves its symbol through the
    // scoped views, answering `null` for a symbol absent from the requested checkout.
    assert_eq!(advertised_worktree_param("git_history_for_symbol"), param);
    let history = |symbol: &str, worktree: &str| {
        call("git_history_for_symbol", json!({"symbol": symbol}), Some(worktree))
    };
    assert!(!history("first_fn", first).is_null(), "the scoped checkout resolves its symbol");
    assert!(history("first_fn", second).is_null(), "a sibling checkout does not");
}

#[test]
fn compare_graph_to_text_stays_base_scoped_under_a_worktree_param() {
    // #219 review (3440746678): `compare_graph_to_text` reads LIVE source text through
    // `source_root` (the MAIN checkout). Under an overlay scope its GRAPH side would be the branch
    // overlay while its TEXT side stayed main — mismatched. It must stay BASE-scoped even with a
    // `worktree` arg, so a `worktree`-passed call matches the base call exactly.
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    // Base: `caller` calls `target`.
    fs::write(root.join("src/a.rs"), "pub fn target() {}\npub fn caller() {\n    target();\n}\n")
        .unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let config = rust_config(root.to_path_buf());
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // Branch: `caller` no longer calls `target` (the callsite is gone on the branch). An
    // overlay-scoped compare would see 0 graph edges; the base sees 1.
    let linked = unique_temp_root();
    git(&root, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn target() {}\npub fn caller() {\n}\n").unwrap();
    git(&linked, &["add", "."]);
    git(&linked, &["commit", "-q", "-m", "branch drops call"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    drop(db);

    let lookup =
        call_tool_for_config(&config, "symbol_lookup", json!({"symbol": "target"})).unwrap();
    let id = lookup["candidates"][0]["id"].as_str().unwrap().to_string();
    let args = |worktree: Option<&str>| {
        let mut a = json!({"id": id, "pattern": "target\\(", "resolution": "exact",
            "edge_kinds": ["calls_name"]});
        if let Some(w) = worktree {
            a["worktree"] = json!(w);
        }
        a
    };

    let base = call_tool_for_config(&config, "compare_graph_to_text", args(None)).unwrap();
    let with_worktree = call_tool_for_config(
        &config,
        "compare_graph_to_text",
        args(Some(linked.to_str().unwrap())),
    )
    .unwrap();
    // The `worktree` call is identical to the base call: graph + text both from main, never the
    // overlay (which would have dropped the graph edge).
    assert_eq!(
        base["summary"], with_worktree["summary"],
        "compare_graph_to_text must stay base-scoped regardless of the worktree param",
    );
    assert!(
        base["summary"]["graph_edges"].as_u64().unwrap() >= 1,
        "the base call sees the committed callsite edge: {base:?}",
    );
}

#[test]
fn heal_index_from_a_linked_worktree_does_not_corrupt_the_overlay() {
    // #219 review: `heal_index` (a WRITE tool) reads file bytes from the stored `source_root` (the
    // MAIN checkout). If the read-write connection were scoped to the linked worktree, the heal
    // would reindex the overlay with MAIN's contents or — for a BRANCH-ONLY file, absent from main
    // — tombstone it in the overlay scope. The fix keeps write tools in the BASE scope and
    // makes the heal paths refuse to write under a linked overlay scope, so the overlay
    // survives a `heal_index` invoked from the worktree cwd.
    let root = unique_temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let config = rust_config(root.to_path_buf());
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    git(&root, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Branch modifies one file AND adds a branch-only file (the file absent from main is the strong
    // corruption vector: a worktree-scoped heal can't read it from `source_root`, so it
    // tombstones).
    fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
    fs::write(linked.join("src/only.rs"), "pub fn branch_only_fn() {}\n").unwrap();
    git(&linked, &["add", "."]);
    git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    drop(db);

    let linked_str = linked.to_str().unwrap();
    // Run `heal_index` from the worktree cwd (the `worktree` param the dispatcher would honor).
    call_tool_for_config(&config, "heal_index", json!({"worktree": linked_str})).unwrap();

    // The overlay is intact: the worktree scope still serves the modified BRANCH version, not
    // main's.
    let modified = call_tool_for_config(
        &config,
        "symbol_lookup",
        json!({"symbol": "linked_fn", "worktree": linked_str}),
    )
    .unwrap();
    assert!(
        candidate_count(&modified) > 0,
        "heal_index from the worktree must NOT overwrite the modified branch overlay",
    );
    // The branch-only file is still served — NOT tombstoned by a heal that couldn't read it in
    // main.
    let only = call_tool_for_config(
        &config,
        "symbol_lookup",
        json!({"symbol": "branch_only_fn", "worktree": linked_str}),
    )
    .unwrap();
    assert!(
        candidate_count(&only) > 0,
        "heal_index must NOT tombstone the branch-only overlay file",
    );
}

#[test]
fn read_tool_lazy_write_retries_read_write_not_readonly_error() {
    // #143 review: read tools open read-only, but a few lazily WRITE on a cold path — here
    // `read_chunk` calls `mark_file_deleted` when its source file is gone on disk. That write fails
    // on the read-only connection with SQLITE_READONLY; the dispatcher must transparently retry the
    // call on a read-write connection, so the caller gets the domain error (chunk gone), NEVER a
    // raw read-only violation.
    let (root, config) = mixed_config();
    IndexDatabase::rebuild(&config).unwrap();

    let search =
        call_tool_for_config(&config, "semantic_search", json!({"query": "alpha"})).unwrap();
    let hit = search.as_array().unwrap().first().expect("a semantic hit").clone();
    let chunk_id = hit["chunk_id"].as_i64().unwrap();
    let path = hit["path"].as_str().unwrap().to_string();

    // Remove the source file → `read_chunk` takes the mark_file_deleted (write) branch.
    std::fs::remove_file(root.join(&path)).unwrap();

    let result = call_tool_for_config(&config, "read_chunk", json!({"chunk_id": chunk_id}));
    let err = result.expect_err("a deleted-source chunk reports gone");
    assert!(
        !rag_rat_db::storage::is_readonly_violation(&err),
        "the lazy write must be retried read-write, never surfaced as SQLITE_READONLY: {err:?}"
    );
}

#[test]
fn read_only_classification_covers_every_tool_and_denies_writers() {
    // #143: the read-only fast path must classify EVERY tool, and exactly the mutating tools must
    // be denied read-only access. Drift either lock-contends a read tool (slow) or hands a write
    // tool a read-only connection (runtime failure).
    const WRITERS: &[&str] = &[
        "heal_index",
        "memory_create",
        "memory_rebind",
        "memory_update",
        "memory_edge_add",
        "memory_edge_remove",
        "memory_mark_obsolete",
        "memory_validate",
        "dream",
        "dream_review",
    ];
    for writer in WRITERS {
        assert!(TOOL_NAMES.contains(writer), "writer {writer} is not a registered tool");
        assert!(!is_read_only_tool(writer), "{writer} mutates the index — must not be read-only");
    }
    for name in TOOL_NAMES {
        let expected_read_only = !WRITERS.contains(name);
        assert_eq!(
            is_read_only_tool(name),
            expected_read_only,
            "tool {name} is misclassified for the read-only open path"
        );
    }
}

#[test]
fn busy_retry_rides_out_a_short_writer_but_is_bounded() {
    use std::cell::Cell;

    fn busy() -> anyhow::Error {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_string()),
        ))
    }

    // Busy twice, then success — the read-write fallback rides out a brief writer instead of
    // surfacing -32603 to the agent (#220).
    let calls = Cell::new(0u32);
    let ok: anyhow::Result<&str> = with_busy_retry(|| {
        calls.set(calls.get() + 1);
        if calls.get() < 3 { Err(busy()) } else { Ok("ok") }
    });
    assert_eq!(ok.unwrap(), "ok");
    assert_eq!(calls.get(), 3, "retries the busy attempts");

    // A non-busy error returns immediately — never mask a real error behind a backoff.
    let calls = Cell::new(0u32);
    let err: anyhow::Result<()> = with_busy_retry(|| {
        calls.set(calls.get() + 1);
        Err(anyhow::anyhow!("syntax error"))
    });
    assert!(err.is_err());
    assert_eq!(calls.get(), 1, "non-busy errors are not retried");

    // A sustained writer gives up after the bound, returning the busy error (not a hang).
    let calls = Cell::new(0u32);
    let exhausted: anyhow::Result<()> = with_busy_retry(|| {
        calls.set(calls.get() + 1);
        Err(busy())
    });
    assert!(rag_rat_db::storage::is_busy(&exhausted.unwrap_err()));
    assert_eq!(calls.get(), 3, "bounded to MAX_ATTEMPTS");
}

/// The default list is the coding surface: maintenance, diagnostics and the memory task graph come
/// off it, and each comes back when its toolset is enabled. Listing only — every tool stays
/// routable (`mcp_stdio_every_advertised_tool_is_routable` covers `TOOL_NAMES`).
#[test]
fn toolsets_decide_what_is_listed_and_nothing_else() {
    use rag_rat_base::config::McpToolset::{Admin, Graph};
    let listed = |enabled: &[rag_rat_base::config::McpToolset]| {
        TOOL_NAMES
            .iter()
            .copied()
            .filter(|name| super::is_listed(name, enabled))
            .collect::<Vec<_>>()
    };
    let default = listed(&[]);
    for name in ["semantic_search", "impact_surface", "memory_search", "index_status"] {
        assert!(default.contains(&name), "{name} is on the default list");
    }
    for name in ["heal_index", "dream", "llm_status", "memory_edges", "ffi_surface"] {
        assert!(!default.contains(&name), "{name} is off the default list");
    }
    assert!(
        listed(&[Admin]).contains(&"heal_index") && !listed(&[Admin]).contains(&"memory_edges")
    );
    assert!(
        listed(&[Graph]).contains(&"memory_edges") && !listed(&[Graph]).contains(&"heal_index")
    );
    let not_deprecated = TOOL_NAMES
        .iter()
        .copied()
        .filter(|name| super::replacement(name).is_none())
        .collect::<Vec<_>>();
    assert_eq!(listed(&[Admin, Graph]), not_deprecated, "both toolsets list all but deprecated");
}

#[test]
fn toolsets_merge_config_with_the_environment_and_skip_unknown_names() {
    use rag_rat_base::config::McpToolset::{Admin, Graph};
    assert_eq!(super::merge_toolsets(&[], ""), vec![]);
    assert_eq!(super::merge_toolsets(&[Graph], " admin , nope ,graph"), vec![Admin, Graph]);
}
