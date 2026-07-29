use std::path::PathBuf;
use std::time::{Duration, Instant};

use rag_rat_base::config::{Config, ResolvedTarget, TargetKind};
use rag_rat_base::language::Language;
use rag_rat_core::{IndexDatabase, OutputFormat};
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData, ServerHandler};
use serde_json::{Value, json};

use super::*;
use crate::blocking::{self, ToolTimeoutPolicy};

fn config_over_temp_repo() -> (rag_rat_base::test_scratch::ScratchDir, Config) {
    let root = rag_rat_base::test_scratch::ScratchDir::new("server-test");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn open_database() {}\n").unwrap();
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
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    IndexDatabase::rebuild(&config).unwrap();
    (root, config)
}

fn service_over_temp_repo() -> (rag_rat_base::test_scratch::ScratchDir, RagRatService) {
    let (root, config) = config_over_temp_repo();
    (root, RagRatService::new(config, OutputFormat::Toon))
}

fn ok_result() -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text("ok")])
}

fn test_tool_workers(permits: usize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(permits))
}

#[test]
fn timeout_and_worker_env_ignore_blank_zero_and_invalid_values() {
    assert_eq!(blocking::parse_tool_timeout("5"), Some(Duration::from_secs(5)));
    assert_eq!(blocking::parse_tool_timeout(" 9 "), Some(Duration::from_secs(9)));
    assert_eq!(blocking::parse_tool_timeout(""), None);
    assert_eq!(blocking::parse_tool_timeout("0"), None);
    assert_eq!(blocking::parse_tool_timeout("nope"), None);
    assert_eq!(blocking::parse_tool_workers("3"), Some(3));
    assert_eq!(blocking::parse_tool_workers(" 4 "), Some(4));
    assert_eq!(blocking::parse_tool_workers(""), None);
    assert_eq!(blocking::parse_tool_workers("0"), None);
    assert_eq!(blocking::parse_tool_workers("nope"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn call_async_dispatches_through_blocking_chokepoint() {
    let (_root, svc) = service_over_temp_repo();
    let result = svc.call_async("index_status".to_string(), json!({})).await.unwrap();

    assert!(!result.content.is_empty(), "async tool dispatch returned no content");
    let err = svc
        .call_async("definitely_not_a_tool".to_string(), json!({}))
        .await
        .expect_err("unknown tool must surface as an MCP error");
    assert!(
        err.message.contains("unknown tool"),
        "unknown-tool error should preserve the dispatcher message, got: {}",
        err.message
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn call_async_counts_queued_blocking_work_as_inflight() {
    let (_root, mut svc) = service_over_temp_repo();
    svc.tool_workers = test_tool_workers(1);
    let held_worker = Arc::clone(&svc.tool_workers).acquire_owned().await.unwrap();
    let inflight = svc.inflight();

    let pending = tokio::spawn({
        let svc = svc.clone();
        async move { svc.call_async("index_status".to_string(), json!({})).await }
    });
    for _ in 0..20 {
        if inflight.count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(inflight.count(), 1, "queued blocking work must count as in-flight");

    drop(held_worker);
    pending.await.unwrap().unwrap();
    assert_eq!(inflight.count(), 0, "in-flight guard must drop after the worker exits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocking_tool_work_does_not_starve_the_runtime() {
    let workers = test_tool_workers(2);
    let slow = tokio::spawn(blocking::run_blocking_tool(
        "slow_test_tool".to_string(),
        Duration::from_secs(1),
        ToolTimeoutPolicy::ReturnTimeout,
        Arc::clone(&workers),
        || {
            std::thread::sleep(Duration::from_millis(200));
            Ok(ok_result())
        },
    ));

    tokio::time::sleep(Duration::from_millis(10)).await;
    let quick = tokio::time::timeout(
        Duration::from_millis(100),
        blocking::run_blocking_tool(
            "quick_test_tool".to_string(),
            Duration::from_secs(1),
            ToolTimeoutPolicy::ReturnTimeout,
            workers,
            || Ok(ok_result()),
        ),
    )
    .await;
    assert!(quick.is_ok(), "quick tool call should not wait behind blocking work");
    quick.unwrap().unwrap();
    slow.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocking_tool_work_returns_timeout_error() {
    let err = blocking::run_blocking_tool(
        "timeout_test_tool".to_string(),
        Duration::from_millis(10),
        ToolTimeoutPolicy::ReturnTimeout,
        test_tool_workers(1),
        || {
            std::thread::sleep(Duration::from_millis(100));
            Ok(ok_result())
        },
    )
    .await
    .expect_err("slow blocking work must time out");

    assert!(
        err.message.contains("timed out"),
        "timeout error should be actionable, got: {}",
        err.message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocking_tool_work_applies_worker_limit() {
    let workers = test_tool_workers(1);
    let slow = tokio::spawn(blocking::run_blocking_tool(
        "slow_test_tool".to_string(),
        Duration::from_secs(1),
        ToolTimeoutPolicy::ReturnTimeout,
        Arc::clone(&workers),
        || {
            std::thread::sleep(Duration::from_millis(100));
            Ok(ok_result())
        },
    ));
    tokio::time::sleep(Duration::from_millis(10)).await;

    let err = blocking::run_blocking_tool(
        "queued_test_tool".to_string(),
        Duration::from_millis(20),
        ToolTimeoutPolicy::ReturnTimeout,
        workers,
        || Ok(ok_result()),
    )
    .await
    .expect_err("a queued tool must respect the shared worker limit and timeout");
    assert!(
        err.message.contains("timed out"),
        "queued timeout error should be actionable, got: {}",
        err.message
    );
    slow.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn write_tool_deadline_waits_for_blocking_work_to_finish() {
    let started = Instant::now();
    let result = blocking::run_blocking_tool(
        "memory_create".to_string(),
        Duration::from_millis(10),
        ToolTimeoutPolicy::WaitForCompletion,
        test_tool_workers(1),
        || {
            std::thread::sleep(Duration::from_millis(60));
            Ok(ok_result())
        },
    )
    .await
    .expect("write-classified tools must not detach and return timeout");

    assert!(!result.content.is_empty());
    assert!(
        started.elapsed() >= Duration::from_millis(50),
        "write tool returned before the blocking worker actually stopped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocking_tool_work_propagates_tool_errors() {
    let err = blocking::run_blocking_tool(
        "error_test_tool".to_string(),
        Duration::from_secs(1),
        ToolTimeoutPolicy::ReturnTimeout,
        test_tool_workers(1),
        || Err(ErrorData::internal_error("intentional tool failure".to_string(), None)),
    )
    .await
    .expect_err("tool errors must not be converted into successes");

    assert!(
        err.message.contains("intentional tool failure"),
        "tool error should keep its original message, got: {}",
        err.message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocking_tool_work_reports_panics_as_mcp_errors() {
    let err = blocking::run_blocking_tool(
        "panic_test_tool".to_string(),
        // Generous budget: the tool panics immediately, but a 1 s timeout races cold
        // Windows-CI worker-pool startup (measured >1 s), turning the expected
        // panic error into a spurious timeout error. This test exercises panic
        // REPORTING, not the timeout.
        Duration::from_secs(30),
        ToolTimeoutPolicy::ReturnTimeout,
        test_tool_workers(1),
        || panic!("intentional panic from blocking tool"),
    )
    .await
    .expect_err("blocking worker panic must be converted into an MCP error");

    assert!(
        err.message.contains("panic_test_tool") && err.message.contains("panicked"),
        "panic error should name the tool and failure mode, got: {}",
        err.message
    );
}

/// The staleness nudge (#160) rides a TOON tool result as a second content block, SUPPRESSED in
/// `--json` mode (Codex #160 review), and THROTTLED (#752) to at most once per window across
/// the fleet — except a `memory_create`/`memory_update` forces it (and resets the window).
#[test]
fn stale_memory_nudge_throttles_forces_and_suppresses_json() {
    let (_root, config) = config_over_temp_repo();
    // A memory bound to an unindexed/absent path resolves `gone` → drift the nudge reports.
    let toon = RagRatService::new(config.clone(), OutputFormat::Toon);
    toon.call(
        "memory_create",
        json!({
            "kind": "Invariant",
            "title": "drift",
            "body": "b",
            "confidence": "high",
            "bind": {"path": "does/not/exist.rs"}
        }),
    )
    .unwrap();
    // A path binding is created `current`; validation flips the absent path to `gone` —
    // after TWO passes, per the #492 downgrade hysteresis (the first only arms the marker).
    toon.call("memory_validate", json!({})).unwrap();
    toon.call("memory_validate", json!({})).unwrap();

    // JSON mode NEVER shows the prose nudge (would break JSON-parsing clients).
    let json_svc = RagRatService::new(config, OutputFormat::Json);
    assert!(json_svc.stale_memory_nudge("index_status").is_none(), "JSON suppresses the nudge");

    // A memory_create FORCES the nudge regardless of the throttle window, and proves it rides a
    // real TOON tool result as a second content block. (n stays > 0 — this new binding is
    // `current`, the original one is still `gone`.)
    let forced = toon
        .call(
            "memory_create",
            json!({
                "kind": "Decision", "title": "another", "body": "b", "confidence": "high",
                "bind": {"path": "also/absent.rs"}
            }),
        )
        .unwrap();
    assert_eq!(forced.content.len(), 2, "a forced nudge rides the tool result as a 2nd block");

    // Immediately after — within the window — a plain read tool is THROTTLED (nudge
    // suppressed).
    assert!(toon.stale_memory_nudge("semantic_search").is_none(), "throttled inside the window",);
}

/// #752 end-to-end: in TOON (LLM-facing) mode a repo memory riding a DRIVE-BY result
/// (symbol_lookup) is full on first surface and STUBBED on re-show within the session, while an
/// EXPLICIT `memory_*` tool is never trimmed. JSON mode is UNTRIMMED even on repeat, so
/// programmatic clients keep stable, complete shapes (Codex #752 review).
#[test]
fn drive_by_memories_dedup_in_toon_but_json_mode_is_untrimmed() {
    fn text_of(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }
    fn call_json(svc: &RagRatService, tool: &str, args: Value) -> Value {
        serde_json::from_str(&text_of(&svc.call(tool, args).unwrap())).unwrap()
    }

    let (_root, config) = config_over_temp_repo(); // indexes `open_database`
    let lookup_args = json!({"symbol": "open_database", "allow_ambiguous": true});
    const ELIDED: &str = "already surfaced this session";

    // Setup on a JSON service (results parse cleanly): resolve the symbol, bind a memory to it
    // so it rides drive-by results. The anchor is `current` (real indexed symbol) → no nudge.
    let json_svc = RagRatService::new(config.clone(), OutputFormat::Json);
    let lookup = call_json(&json_svc, "symbol_lookup", lookup_args.clone());
    let sym_id = lookup["candidates"][0]["id"].as_str().unwrap().to_string();
    let created = call_json(
        &json_svc,
        "memory_create",
        json!({
            "kind": "Invariant", "title": "db open invariant",
            "body": "a distinctive memory body", "confidence": "high",
            "bind": {"id": sym_id.clone()}
        }),
    );
    let mem_id = created["memory"]["memory_id"].as_str().unwrap().to_string();

    // TOON: first surface carries the full memory (no stub marker); the re-show is stubbed —
    // the `elided` marker appears, id + title stay. (A fresh service = a fresh per-agent
    // seen-set.)
    let toon = RagRatService::new(config.clone(), OutputFormat::Toon);
    let first = text_of(&toon.call("symbol_lookup", lookup_args.clone()).unwrap());
    assert!(first.contains(&mem_id), "the memory rides the first drive-by result");
    assert!(!first.contains(ELIDED), "first TOON surface is the full memory, not a stub");
    let second = text_of(&toon.call("symbol_lookup", lookup_args.clone()).unwrap());
    assert!(second.contains(ELIDED), "re-shown drive-by memory is stubbed with a fetch hint");
    assert!(
        second.contains(&mem_id) && second.contains("db open invariant"),
        "the stub keeps the id and title",
    );

    // An EXPLICIT memory tool is NEVER trimmed — even though this agent already saw the memory
    // via the drive-by surfaces above, `memory_for_symbol` returns it in full, not a stub.
    let explicit = text_of(&toon.call("memory_for_symbol", json!({"id": sym_id.clone()})).unwrap());
    assert!(explicit.contains(&mem_id), "the explicit memory tool returns the memory");
    assert!(!explicit.contains(ELIDED), "the explicit memory tool is not stubbed");

    // JSON mode is UNTRIMMED even on repeat: the drive-by memory keeps its full shape, no stub.
    for _ in 0..2 {
        let v = call_json(&json_svc, "symbol_lookup", lookup_args.clone());
        let m = &v["candidates"][0]["memories"][0];
        assert_eq!(m["memory_id"].as_str(), Some(mem_id.as_str()));
        assert!(m["elided"].is_null(), "JSON mode never stubs a drive-by memory");
    }
}

#[test]
fn get_info_advertises_tool_capability() {
    let (root, svc) = service_over_temp_repo();
    let info = svc.get_info();
    assert!(info.capabilities.tools.is_some(), "server must advertise tools");
    assert_eq!(info.server_info.name, "rag-rat");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn call_dispatches_every_read_tool_and_rejects_unknown() {
    let (root, svc) = service_over_temp_repo();
    // The chokepoint `call()` funnels every tool: success path (render to TOON text) across a
    // representative read-tool set, plus the error mapping for an unknown tool.
    let calls = [
        ("semantic_search", json!({ "query": "open_database" })),
        ("symbol_lookup", json!({ "symbol": "open_database" })),
        ("find_callers", json!({ "symbol": "open_database" })),
        ("trace_callees", json!({ "symbol": "open_database" })),
        ("impact_surface", json!({ "symbol": "open_database" })),
        ("docs_for_symbol", json!({ "symbol": "open_database" })),
        ("git_history_for_symbol", json!({ "symbol": "open_database" })),
        ("repo_brief", json!({})),
        ("repo_clusters", json!({})),
        ("important_symbols", json!({})),
        ("ffi_surface", json!({})),
        ("compare_graph_to_scip", json!({})),
        ("index_status", json!({})),
        ("llm_status", json!({})),
        ("papertrail_sync_status", json!({})),
        ("memory_validate", json!({})),
    ];
    for (name, args) in calls {
        let result = svc.call(name, args).unwrap_or_else(|e| panic!("{name} failed: {e:?}"));
        assert!(!result.content.is_empty(), "{name} returned no content");
    }
    assert!(svc.call("definitely_not_a_tool", json!({})).is_err(), "unknown tool must error");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn memory_show_expands_a_memory_to_its_full_body_by_id() {
    // The expand path: given the `memory_id` from a compact summary (surface="summary"),
    // `memory_show` returns the COMPLETE original body — no shelling out to the CLI.
    let (root, config) = config_over_temp_repo();
    let db_path = &config.database;
    let created = crate::tools::call_tool(
        db_path,
        "memory_create",
        json!({
            "kind": "Invariant",
            "title": "t",
            "body": "FULL-BODY-MARKER expand text",
            "confidence": "high",
            "bind": { "path": "src/lib.rs" }
        }),
    )
    .unwrap();
    // memory_create returns RepoMemoryCreateResult { memory, duplicate } — the id is nested.
    let id = created["memory"]["memory_id"]
        .as_str()
        .expect("created memory carries a memory_id")
        .to_string();

    let shown =
        crate::tools::call_tool(db_path, "memory_show", json!({ "memory_id": id })).unwrap();
    assert_eq!(
        shown["body"].as_str(),
        Some("FULL-BODY-MARKER expand text"),
        "returns the full body"
    );
    assert_eq!(shown["title"].as_str(), Some("t"));

    // An unknown id errors, not a silent null.
    assert!(
        crate::tools::call_tool(db_path, "memory_show", json!({ "memory_id": "mem_nope" }))
            .is_err(),
        "unknown memory id must error",
    );

    let _ = std::fs::remove_dir_all(&root);
}
