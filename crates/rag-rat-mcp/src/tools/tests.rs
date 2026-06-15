use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rag_rat_core::language::Language;
use rag_rat_core::{Config, IndexDatabase, ResolvedTarget, TargetKind};
use serde_json::json;

use super::*;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn degraded_coverage_escalates_low_completeness_risk() {
    // issue #47: a stale/partial index can hide caller edges, so a 0-result must not read
    // as confident. `low` is escalated to `medium` when coverage is degraded.
    let mut stale = json!({
        "summary": { "completeness_risk": "low", "returned_count": 0 },
        "coverage": { "stale_files": 1, "parser_failures": 0, "known_index_gaps": [] },
    });
    compact_graph_coverage(&mut stale, true);
    assert_eq!(stale["summary"]["completeness_risk"], "medium");

    // Clean coverage leaves an honest `low` untouched.
    let mut clean = json!({
        "summary": { "completeness_risk": "low" },
        "coverage": { "stale_files": 0, "parser_failures": 0, "known_index_gaps": [] },
    });
    compact_graph_coverage(&mut clean, true);
    assert_eq!(clean["summary"]["completeness_risk"], "low");

    // A medium/high risk is never downgraded by this path.
    let mut high = json!({
        "summary": { "completeness_risk": "high" },
        "coverage": { "stale_files": 3, "parser_failures": 0, "known_index_gaps": [] },
    });
    compact_graph_coverage(&mut high, true);
    assert_eq!(high["summary"]["completeness_risk"], "high");
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
    assert_handle_round_trips!(SymbolGraphArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(CompareGraphTextArgs, "id", json!({ "pattern": "x", "id": HANDLE }));
    assert_handle_round_trips!(ImpactArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(MemoryForSymbolArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(MemoryBindArgs, "id", json!({ "id": HANDLE }));
    assert_handle_round_trips!(MemoryBindArgs, "start_id", json!({ "start_id": HANDLE }));
    assert_handle_round_trips!(MemoryBindArgs, "end_id", json!({ "end_id": HANDLE }));
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
}

#[test]
fn list_tools_exposes_complete_typed_schemas() {
    let tools = list_tools();
    let tools = tools.as_array().expect("tools/list shape");
    let names =
        tools.iter().map(|tool| tool["name"].as_str().expect("tool name")).collect::<Vec<_>>();

    for expected in [
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
        "index_status",
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
        "memory_create",
        "memory_update",
        "memory_search",
        "memory_for_symbol",
        "memory_for_path",
        "memory_for_call_path",
        "memory_validate",
        "memory_mark_obsolete",
    ] {
        assert!(names.contains(&expected), "missing MCP tool {expected}");
    }

    assert_schema_requires(tools, "semantic_search", "query");
    assert_schema_has_property(tools, "semantic_search", "include_graph");
    assert_schema_property_enum(tools, "semantic_search", "include_graph", &[
        "none", "compact", "full",
    ]);
    assert_schema_has_property(tools, "semantic_search", "graph_limit");
    assert_schema_array_item_enum(tools, "semantic_search", "include", &[
        "generated",
        "git",
        "papertrail",
        "fallback",
    ]);
    assert_schema_has_property(tools, "semantic_search", "explain");
    assert_symbol_selector_schema(tools, "symbol_lookup");
    assert_schema_array_item_enum(tools, "symbol_lookup", "include", &["memories"]);
    assert_schema_array_item_enum(tools, "find_callers", "include", &[
        "references",
        "unresolved",
        "macros",
        "common_methods",
        "coverage",
        "memories",
    ]);
    assert_schema_has_property(tools, "find_callers", "edge_kinds");
    assert_schema_has_property(tools, "find_callers", "resolution");
    assert_schema_property_enum(tools, "find_callers", "resolution", &[
        "exact",
        "syntactic",
        "fuzzy",
    ]);
    assert_schema_array_item_enum(tools, "find_callers", "edge_kinds", &[
        "calls_name",
        "constructs",
        "uses_macro",
        "references_type",
        "imports",
        "exports",
        "contains",
        "implements",
    ]);
    assert_schema_has_property(tools, "find_callers", "id");
    assert_symbol_selector_schema(tools, "find_callers");
    assert_schema_array_item_enum(tools, "trace_callees", "include", &[
        "references",
        "unresolved",
        "macros",
        "common_methods",
        "coverage",
        "memories",
    ]);
    assert_schema_has_property(tools, "trace_callees", "edge_kinds");
    assert_schema_has_property(tools, "trace_callees", "resolution");
    assert_schema_has_property(tools, "trace_callees", "id");
    assert_symbol_selector_schema(tools, "trace_callees");
    assert_schema_requires(tools, "compare_graph_to_text", "pattern");
    assert_schema_array_item_enum(tools, "compare_graph_to_text", "include", &[
        "tests",
        "references",
        "unresolved",
        "macros",
        "common_methods",
    ]);
    assert_schema_has_property(tools, "compare_graph_to_text", "edge_kinds");
    assert_schema_has_property(tools, "compare_graph_to_text", "resolution");
    assert_schema_has_property(tools, "compare_graph_to_text", "id");
    assert_symbol_selector_schema(tools, "compare_graph_to_text");
    assert_schema_has_property(tools, "impact_surface", "resolution");
    assert_schema_array_item_enum(tools, "impact_surface", "include", &[
        "tests",
        "docs",
        "git",
        "papertrail",
        "text_fallback",
        "memories",
    ]);
    assert_schema_has_property(tools, "impact_surface", "id");
    assert_symbol_selector_schema(tools, "impact_surface");
    assert_schema_has_property(tools, "repo_brief", "mode");
    assert_schema_property_enum(tools, "repo_brief", "mode", &[
        "spine",
        "churn",
        "god_modules",
        "refactor_candidates",
    ]);
    assert_schema_has_property(tools, "repo_brief", "limit");
    assert_schema_array_item_enum(tools, "repo_brief", "include", &["generated", "memories"]);
    assert_schema_has_property(tools, "repo_clusters", "limit");
    assert_schema_array_item_enum(tools, "repo_clusters", "include", &["generated", "memories"]);
    assert_schema_has_property(tools, "repo_clusters", "min_cluster_size");
    assert_symbol_selector_schema(tools, "docs_for_symbol");
    assert_symbol_selector_schema(tools, "git_history_for_symbol");
    assert_symbol_selector_schema(tools, "papertrail_for_symbol");
    assert_schema_requires(tools, "read_chunk", "chunk_id");
    assert_schema_has_property(tools, "read_chunk", "include_graph");
    assert_schema_property_enum(tools, "read_chunk", "include_graph", &["none", "compact", "full"]);
    assert_schema_has_property(tools, "read_chunk", "graph_limit");
    assert_schema_array_item_enum(tools, "read_chunk", "include", &["memories"]);
    assert_schema_requires(tools, "papertrail_for_commit", "commit_hash");
    assert_schema_array_item_enum(tools, "papertrail_for_commit", "include", &["fallback"]);
    assert_schema_array_item_enum(tools, "rationale_search", "include", &[
        "generated",
        "git",
        "papertrail",
        "fallback",
    ]);
    assert_schema_has_property(tools, "heal_index", "limit");
    assert_schema_requires(tools, "memory_create", "kind");
    assert_schema_requires(tools, "memory_create", "bind");
    assert_schema_has_property(tools, "memory_create", "confidence");
    assert_schema_nested_property(tools, "memory_create", "bind", "edge_id");
    assert_schema_nested_property(tools, "memory_create", "bind", "edge_sequence_hash");
    assert_schema_nested_property(tools, "memory_create", "bind", "path_summary");
    assert_schema_requires(tools, "memory_update", "memory_id");
    assert_schema_requires(tools, "memory_search", "query");
    assert_schema_has_property(tools, "memory_for_symbol", "id");
    assert_schema_requires(tools, "memory_for_path", "path");
    assert_schema_requires(tools, "memory_for_call_path", "edge_sequence_hash");
    assert_schema_requires(tools, "memory_mark_obsolete", "memory_id");
    assert_eq!(tool_schema(tools, "memory_validate")["type"], "object");
    assert_eq!(tool_schema(tools, "local_ai_status")["type"], "object");
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
    let (root, config) = mixed_config();
    IndexDatabase::rebuild(&config).unwrap();
    // Seed a clearly-newer cached crates.io result next to the index (cache_path is public; the
    // network refresh that normally writes this is out of band).
    let cache = rag_rat_core::version_check::cache_path(&config.database);
    std::fs::write(&cache, r#"{"latest_version":"99.0.0","checked_at_ms":1}"#).unwrap();

    let status = call_tool_for_config(&config, "index_status", json!({})).unwrap();
    let version = status.get("version").expect("index_status surfaces a version field");
    assert_eq!(version["current_version"], rag_rat_core::version_check::current_version());
    assert_eq!(version["latest_version"], "99.0.0");
    assert_eq!(version["update_available"], true);
    assert_eq!(version["update_command"], "cargo install rag-rat --force");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn mcp_tool_calls_preserve_compatibility_shapes() {
    let (root, config) = mixed_config();
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
    // index_status trims the embedded local_ai block (use local_ai_status) and the static
    // migration ledger (use the CLI doctor/migrate).
    assert!(status.get("local_ai").is_none(), "local_ai should not be embedded in index_status");
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
    assert!(papertrail["github_evidence"].is_array());

    let github_status = call_tool(&config.database, "github_sync_status", json!({})).unwrap();
    assert!(github_status["capability"].is_string());

    let local_ai = call_tool(&config.database, "local_ai_status", json!({})).unwrap();
    assert_eq!(local_ai["embedding"]["state"], "MissingModel");

    fs::remove_dir_all(root).unwrap();
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
    let config = rust_config(root.clone());
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

    fs::remove_dir_all(root).unwrap();
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

    fs::remove_dir_all(root).unwrap();
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
    let config = rust_config(root.clone());
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

    fs::remove_dir_all(root).unwrap();
}

fn assert_schema_requires(tools: &[Value], name: &str, field: &str) {
    let schema = tool_schema(tools, name);
    let required = schema["required"].as_array().expect("required array");
    assert!(required.iter().any(|value| value == field), "{name} should require {field}");
}

fn assert_schema_has_property(tools: &[Value], name: &str, field: &str) {
    let schema = tool_schema(tools, name);
    assert!(schema["properties"].get(field).is_some(), "{name} should define {field}");
}

fn assert_schema_lacks_property(tools: &[Value], name: &str, field: &str) {
    let schema = tool_schema(tools, name);
    assert!(schema["properties"].get(field).is_none(), "{name} must not define {field}");
}

fn assert_schema_nested_property(tools: &[Value], name: &str, parent: &str, field: &str) {
    let schema = tool_schema(tools, name);
    let property = schema["properties"].get(parent).expect("schema property");
    let resolved = resolve_schema_ref(schema, property);
    assert!(resolved["properties"].get(field).is_some(), "{name}.{parent} should define {field}");
}

fn assert_schema_property_enum(tools: &[Value], name: &str, field: &str, expected: &[&str]) {
    let schema = tool_schema(tools, name);
    let property = schema["properties"].get(field).expect("schema property");
    let resolved = resolve_schema_ref(schema, property);
    let enum_schema = enum_schema(schema, resolved);
    assert_enum_values(enum_schema, expected, &format!("{name}.{field}"));
}

fn assert_schema_array_item_enum(tools: &[Value], name: &str, field: &str, expected: &[&str]) {
    let schema = tool_schema(tools, name);
    let property = schema["properties"].get(field).expect("schema property");
    let resolved = resolve_schema_ref(schema, property);
    let items = resolved
        .get("items")
        .or_else(|| {
            resolved.get("anyOf").and_then(|any| {
                any.as_array()?
                    .iter()
                    .find(|schema| schema.get("type").and_then(Value::as_str) == Some("array"))?
                    .get("items")
            })
        })
        .expect("array items schema");
    let items = resolve_schema_ref(schema, items);
    assert_enum_values(items, expected, &format!("{name}.{field}[]"));
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

fn enum_schema<'a>(root: &'a Value, value: &'a Value) -> &'a Value {
    if value.get("enum").is_some() {
        return value;
    }
    if let Some(any_of) = value.get("anyOf").and_then(Value::as_array) {
        for candidate in any_of {
            if candidate.get("type").and_then(Value::as_str) == Some("null") {
                continue;
            }
            let resolved = resolve_schema_ref(root, candidate);
            if resolved.get("enum").is_some() {
                return resolved;
            }
        }
    }
    value
}

fn assert_enum_values(schema: &Value, expected: &[&str], label: &str) {
    let values = schema["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("{label} should expose enum values: {schema:?}"))
        .iter()
        .map(|value| value.as_str().expect("string enum value"))
        .collect::<Vec<_>>();
    assert_eq!(values, expected, "{label} enum mismatch");
}

fn assert_symbol_selector_schema(tools: &[Value], name: &str) {
    // #149: the wire selector is symbol/ref/id (the opaque handle); the
    // ephemeral numeric symbol_id is no longer an accepted input.
    for field in ["symbol", "ref", "id", "allow_ambiguous"] {
        assert_schema_has_property(tools, name, field);
    }
    assert_schema_lacks_property(tools, name, "symbol_id");
}

#[test]
fn mcp_rewrites_ranking_hint_when_auto_run_enabled() {
    // #142 review: with `[oracle] auto_run` on and no oracle data yet, the important_symbols nudge
    // must say compiler ranking refreshes in the background — not tell the agent to run `oracle
    // run` by hand. The core query is config-unaware, so call_tool_for_config applies the
    // rewrite.
    let (root, mut config) = mixed_config();
    IndexDatabase::rebuild(&config).unwrap();

    config.oracle.auto_run = true;
    let auto = call_tool_for_config(&config, "important_symbols", json!({"limit": 5})).unwrap();
    assert_eq!(
        auto["ranking_hint"].as_str(),
        Some(rag_rat_core::query::pagerank::RANKING_HINT_AUTO_RUN),
        "auto_run must rewrite the heuristic nudge: {auto:?}"
    );

    // With auto_run OFF the default manual-run nudge is preserved.
    config.oracle.auto_run = false;
    let manual = call_tool_for_config(&config, "important_symbols", json!({"limit": 5})).unwrap();
    assert_eq!(
        manual["ranking_hint"].as_str(),
        Some(rag_rat_core::query::pagerank::RANKING_HINT_RUN_ORACLE),
    );

    std::fs::remove_dir_all(root).unwrap();
}

fn tool_schema<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .map(|tool| &tool["inputSchema"])
        .expect("tool schema")
}

fn mixed_config() -> (PathBuf, Config) {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("docs/search.md"), "# Title\nalpha token\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn alpha_symbol() {}\n").unwrap();
    (root.clone(), Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
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
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    })
}

fn markdown_config(text: &str) -> (PathBuf, Config) {
    let root = unique_temp_root();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/search.md"), text).unwrap();
    (root.clone(), Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "markdown".to_string(),
            language: Language::Markdown,
            directories: vec![PathBuf::from("docs")],
            include: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Docs,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    })
}

fn rust_config(root: PathBuf) -> Config {
    Config {
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        local_ai: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
    }
}

fn unique_temp_root() -> PathBuf {
    let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rag-rat-mcp-test-{}-{id}", std::process::id()))
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
        !rag_rat_core::storage::is_readonly_violation(&err),
        "the lazy write must be retried read-write, never surfaced as SQLITE_READONLY: {err:?}"
    );

    std::fs::remove_dir_all(root).unwrap();
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
        "memory_mark_obsolete",
        "memory_validate",
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
