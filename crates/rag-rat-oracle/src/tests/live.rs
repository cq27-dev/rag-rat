//! End-to-end tests for the LIVE oracle pass (#534): a fake in-process LSP server (the slice-1
//! test seam) answers `textDocument/definition`; the real `live_oracle_pass` write path maps the
//! definitions to symbols + batch monikers and persists `ra-lsp` verdicts + a backing run row.
//! No real rust-analyzer, no network.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::live::{LiveOracleSession, LivePassAbort, LivePassInput, live_oracle_pass};
use crate::lsp::client::test_support::client_with_server;

const LIVE_VERSION: &str = "ra-test-1";

/// The callsite file: `caller` calls `target`; the callee identifier `target` sits at byte 14.
const CALLER_SRC: &str = "fn caller() { target(); }\n";
const CALLER_CALLEE_START: usize = 14;
const CALLER_CALLEE_END: usize = 20;
/// The definition file: `target` declared at the top; its identifier span is (0,3)–(0,9).
const DEFS_SRC: &str = "fn target() {}\n";

/// A canned definition the fake server returns: `(target_uri, start (line, char), end)`.
type FakeDefTarget = (String, (u32, u32), (u32, u32));

/// A fake LSP session whose server answers every definition request with `def_target`
/// `(uri, (start_line, start_char), (end_line, end_char))` — or `None` for an unresolved `null`.
fn fake_session(root_uri: &str, def_target: Option<FakeDefTarget>) -> LiveOracleSession {
    let client = client_with_server(move |msg: &Value| {
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => Some(vec![json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"capabilities": {"positionEncoding": "utf-16"}}
            })]),
            Some("textDocument/definition") => {
                let result = match &def_target {
                    Some((uri, (sl, sc), (el, ec))) => json!({
                        "uri": uri,
                        "range": {"start": {"line": sl, "character": sc},
                                  "end": {"line": el, "character": ec}}
                    }),
                    None => Value::Null,
                };
                Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": result})])
            },
            // Notifications (initialized / didOpen / didClose) need no reply.
            _ if id.is_none() => Some(vec![]),
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    });
    // `from_client` runs the `initialize` handshake, exactly like a spawned session.
    LiveOracleSession::from_client(client, LIVE_VERSION, root_uri)
}

/// Seed the standard two-file corpus: `defs.rs` with `target`, `src.rs` with a `NameOnly` call
/// edge onto `target`. Returns `(src_file_id, target_symbol_id, edge_id)`.
fn seed_corpus(h: &Harness) -> (i64, i64, i64) {
    let defs = h.add_file("defs.rs", DEFS_SRC);
    let target = h.add_symbol(defs, "target", 0, 13);
    let src = h.add_file("src.rs", CALLER_SRC);
    let edge = h.add_edge(src, "target", CALLER_CALLEE_START, CALLER_CALLEE_END, "NameOnly", None);
    (src, target, edge)
}

fn pass_input<'a>(h: &'a Harness, worklist: &'a [String], max_requests: u64) -> LivePassInput<'a> {
    LivePassInput {
        commit_sha: COMMIT,
        worktree_id: WORKTREE,
        checkout_root: h.root(),
        worklist,
        max_requests,
        started_at_ms: 1_000,
    }
}

fn root_uri(h: &Harness) -> String {
    format!("file://{}", h.root().display())
}

fn def_uri(h: &Harness, path: &str) -> String {
    format!("{}/{path}", root_uri(h))
}

fn live_run_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM oracle_runs WHERE tool = 'ra-lsp'", [], |row| row.get(0))
        .unwrap()
}

fn scip_symbol_of(h: &Harness, edge_id: i64) -> String {
    h.verdict(edge_id).map(|(_, _, symbol)| symbol).expect("a persisted verdict")
}

/// A work-done progress notification a fake server can interleave — the readiness signal of every
/// backend whose policy is [`crate::lsp::readiness::ReadinessPolicy::WorkDoneProgress`].
fn progress(token: &str, kind: &str) -> Value {
    json!({
        "jsonrpc": "2.0", "method": "$/progress",
        "params": {"token": token, "value": {"kind": kind}}
    })
}

#[test]
fn live_pass_upgrades_a_name_only_edge_and_records_the_run() {
    let h = Harness::new();
    let (_src, target, edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(report.rows_written, 1);
    assert_eq!(report.upgraded, 1);
    assert_eq!(report.requests_used, 1);
    assert_eq!(report.status, "Completed");
    assert!(report.run_recorded);
    assert!(!report.refinements_invalidated);
    let (kind, resolved, symbol) = h.verdict(edge).expect("verdict persisted");
    assert_eq!(kind, "upgrade");
    assert_eq!(resolved, Some(target));
    assert!(
        symbol.starts_with("local ra-lsp-"),
        "no batch baseline ⇒ the content-stable sentinel: {symbol}"
    );
    assert_eq!(live_run_count(&h.conn), 1, "one backing run row for the writing pass");
}

#[test]
fn live_pass_defers_the_whole_worklist_while_the_server_is_warming() {
    let h = Harness::new();
    let (_src, _target, _edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let requested_methods = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requested_methods);
    let client = client_with_server(move |msg: &Value| {
        let id = msg.get("id").cloned();
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            captured.lock().unwrap().push(method.to_string());
        }
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => Some(vec![
                json!({
                    "jsonrpc": "2.0", "method": "experimental/serverStatus",
                    "params": {"health": "ok", "quiescent": false}
                }),
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"capabilities": {"positionEncoding": "utf-16"}}
                }),
            ]),
            _ if id.is_none() => Some(vec![]),
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    });
    let mut session = LiveOracleSession::from_warming_client(client, LIVE_VERSION, &uri);
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(report.status, "Warming");
    assert_eq!(report.requests_used, 0);
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.unfinished_paths, worklist);
    assert_eq!(live_run_count(&h.conn), 0);
    assert!(
        !requested_methods.lock().unwrap().iter().any(|method| method == "textDocument/definition"),
        "warm-up must not turn temporary null definitions into completed work"
    );
}

#[test]
fn live_pass_discards_a_definition_batch_when_readiness_regresses() {
    let h = Harness::new();
    let (_src, _target, edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let definition_uri = def_uri(&h, "defs.rs");
    let client = client_with_server(move |msg: &Value| {
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => Some(vec![json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"capabilities": {"positionEncoding": "utf-16"}}
            })]),
            Some("textDocument/definition") => Some(vec![
                json!({
                    "jsonrpc": "2.0", "method": "experimental/serverStatus",
                    "params": {"health": "ok", "quiescent": false}
                }),
                json!({
                    "jsonrpc": "2.0", "method": "experimental/serverStatus",
                    "params": {"health": "ok", "quiescent": true}
                }),
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "uri": definition_uri,
                        "range": {
                            "start": {"line": 0, "character": 3},
                            "end": {"line": 0, "character": 9}
                        }
                    }
                }),
            ]),
            _ if id.is_none() => Some(vec![]),
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    });
    let mut session = LiveOracleSession::from_client(client, LIVE_VERSION, &uri);
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(report.status, "Warming");
    assert_eq!(report.requests_used, 1);
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.unresolved, 0);
    assert_eq!(report.unfinished_paths, worklist);
    assert!(h.verdict(edge).is_none(), "the in-flight batch must be discarded");
    assert_eq!(live_run_count(&h.conn), 0);
}

#[test]
fn live_verdict_copies_the_batch_moniker_verbatim() {
    let h = Harness::new();
    let (_src, target, edge) = seed_corpus(&h);
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", target);
    crate::store::write_logical_symbol_moniker(
        &h.conn,
        OracleTool::RustAnalyzer,
        "batch-v",
        1001,
        TARGET_MONIKER,
    )
    .unwrap();
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert!(report.refinements_invalidated, "new non-local evidence invalidates cached refinement");
    assert_eq!(
        scip_symbol_of(&h, edge),
        TARGET_MONIKER,
        "the live row carries the batch moniker byte-identically — clone-collapse and memory \
         anchoring treat live and batch evidence as one set"
    );
}

#[test]
fn live_sentinel_is_stable_then_a_content_change_upgrades_to_the_batch_moniker() {
    let h = Harness::new();
    let (src_file, target, edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let worklist = vec!["src.rs".to_string()];

    // Pass 1 (no batch baseline): the sentinel.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert!(!report.refinements_invalidated, "no prior row — nothing to invalidate");
    let sentinel = scip_symbol_of(&h, edge);
    assert!(sentinel.starts_with("local ra-lsp-"));

    // Pass 2, same content: the callee is COVERED (same tool_version + file_sha) and the
    // covered-skip never re-resolves it — zero requests, the row untouched.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert_eq!(report.requests_used, 0, "a fully-covered file costs no requests");
    assert_eq!(report.rows_written, 0);
    assert_eq!(scip_symbol_of(&h, edge), sentinel, "sentinel is stable by construction");
    assert!(!report.refinements_invalidated);

    // A batch run then lands a moniker. Same content → the covered-skip still declines to
    // re-resolve (the batch row carries the real evidence for the span meanwhile).
    h.add_logical_symbol(1001, "defs.rs", "target", "defs.rs::target", target);
    crate::store::write_logical_symbol_moniker(
        &h.conn,
        OracleTool::RustAnalyzer,
        "batch-v",
        1001,
        TARGET_MONIKER,
    )
    .unwrap();
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert_eq!(scip_symbol_of(&h, edge), sentinel, "unchanged content stays covered");

    // A content change un-covers the callee (new file_sha), so the pass re-resolves it — now
    // copying the batch moniker. No refine invalidation: the content change already re-keys
    // every refinement that consulted the file.
    let edited = "fn callXr() { target(); }\n";
    std::fs::write(h.root().join("src.rs"), edited).unwrap();
    h.set_file_sha(src_file, &sha256_hex(edited.as_bytes()));
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert_eq!(scip_symbol_of(&h, edge), TARGET_MONIKER, "content change re-resolves");
    assert!(!report.refinements_invalidated, "the sha changed — refinements re-key anyway");
}

#[test]
fn live_pass_confirms_and_contradicts_exact_edges() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", DEFS_SRC);
    let target = h.add_symbol(defs, "target", 0, 13);
    let other = h.add_symbol(defs, "other", 0, 13);
    // Two DISTINCT call sites (the verdict's content key spans the callee token): one the
    // heuristic already resolved to `target` (the oracle agrees → confirm), one resolved to
    // `other` (the oracle disagrees → contradict).
    let src = h.add_file("src.rs", "fn caller() { target(); target(); }\n");
    let agree = h.add_edge(src, "target", 14, 20, "Exact", Some(target));
    let disagree = h.add_edge(src, "target", 24, 30, "Exact", Some(other));
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(report.confirmed, 1);
    assert_eq!(report.contradicted, 1);
    assert_eq!(h.verdict(agree).unwrap().0, "confirm");
    assert_eq!(h.verdict(disagree).unwrap().0, "contradict");
    // The heuristic edge row is NEVER touched (the side-table invariant).
    assert_eq!(h.heuristic_resolution(disagree), ("exact".to_string(), Some(other)));
}

#[test]
fn live_pass_skips_a_drifted_callsite_and_records_no_run() {
    let h = Harness::new();
    let (src, _target, _edge) = seed_corpus(&h);
    // Model a mid-pass edit: the indexed sha no longer matches the disk bytes.
    h.set_file_sha(src, "not-the-disk-sha");
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(report.skipped_drifted, 1);
    assert_eq!(report.rows_written, 0);
    assert!(!report.run_recorded);
    assert_eq!(live_run_count(&h.conn), 0, "a pass that wrote nothing records no run row");
    assert_eq!(report.unfinished_paths, ["src.rs"], "drifted caller is retried after reindex");
}

#[test]
fn live_pass_revalidates_the_caller_after_lsp_requests() {
    let h = Harness::new();
    let (_src, _target, _edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let root = h.root().to_path_buf();
    let client = client_with_server(move |msg: &Value| {
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => Some(vec![json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"capabilities": {"positionEncoding": "utf-16"}}
            })]),
            Some("textDocument/definition") => {
                std::fs::write(root.join("src.rs"), "fn changed() { target(); }\n").unwrap();
                Some(vec![json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"uri": def,
                               "range": {"start": {"line": 0, "character": 3},
                                         "end": {"line": 0, "character": 9}}}
                })])
            },
            _ if id.is_none() => Some(vec![]),
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    });
    let mut session = LiveOracleSession::from_client(client, LIVE_VERSION, &uri);
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.skipped_drifted, 1);
    assert_eq!(report.unfinished_paths, ["src.rs"]);
}

#[test]
fn live_pass_requeues_a_caller_when_its_definition_drifts() {
    let h = Harness::new();
    let (_src, _target, _edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let root = h.root().to_path_buf();
    let client = client_with_server(move |msg: &Value| {
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => Some(vec![json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"capabilities": {"positionEncoding": "utf-16"}}
            })]),
            Some("textDocument/definition") => {
                std::fs::write(root.join("defs.rs"), "fn changed_target() {}\n").unwrap();
                Some(vec![json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"uri": def,
                               "range": {"start": {"line": 0, "character": 3},
                                         "end": {"line": 0, "character": 9}}}
                })])
            },
            _ if id.is_none() => Some(vec![]),
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    });
    let mut session = LiveOracleSession::from_client(client, LIVE_VERSION, &uri);
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.skipped_drifted, 1);
    assert_eq!(report.unfinished_paths, ["src.rs"]);
}

#[test]
fn live_pass_propagates_definition_metadata_query_failures() {
    // SQLite's dynamic typing models a malformed symbol metadata row. The cache fill must return
    // the DB error, not reinterpret it as "no symbols" and record a deceptively successful run.
    let h = Harness::new();
    let (_src, _target, _edge) = seed_corpus(&h);
    h.conn
        .execute(
            "UPDATE symbols SET start_byte = 'bad' WHERE file_id =
                 (SELECT id FROM files WHERE path = 'defs.rs')",
            [],
        )
        .unwrap();
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let mut session = fake_session(&uri, Some((def, (0, 3), (0, 9))));
    let worklist = vec!["src.rs".to_string()];

    assert!(live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).is_err());
    assert_eq!(live_run_count(&h.conn), 0);
}

#[test]
fn live_pass_skips_an_external_definition_without_a_row() {
    let h = Harness::new();
    let (_src, _target, _edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    // The definition lands OUTSIDE the checkout root — a dependency source live can't write
    // about (no indexed symbol, no batch-interchangeable moniker).
    let mut session = fake_session(
        &uri,
        Some(("file:///cargo/registry/src/serde/lib.rs".to_string(), (0, 3), (0, 9))),
    );
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(report.skipped_external, 1);
    assert_eq!(report.rows_written, 0);
    assert_eq!(live_run_count(&h.conn), 0);
}

#[test]
fn live_pass_records_no_run_when_the_server_resolves_nothing() {
    let h = Harness::new();
    let (_src, _target, _edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let mut session = fake_session(&uri, None); // every definition is `null`
    let worklist = vec!["src.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(report.unresolved, 1);
    assert_eq!(report.rows_written, 0);
    assert_eq!(report.status, "NoVerdicts");
    assert_eq!(live_run_count(&h.conn), 0);
}

#[test]
fn live_pass_budget_defers_whole_files_to_the_next_pass() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", DEFS_SRC);
    h.add_symbol(defs, "target", 0, 13);
    let a = h.add_file("a.rs", CALLER_SRC);
    let b = h.add_file("b.rs", CALLER_SRC);
    h.add_edge(a, "target", 14, 20, "NameOnly", None);
    h.add_edge(b, "target", 14, 20, "NameOnly", None);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    // One request of budget: the first file resolves, the second rides.
    let worklist = vec!["a.rs".to_string(), "b.rs".to_string()];

    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 1)).unwrap();

    assert_eq!(report.files_resolved, 1);
    assert_eq!(report.rows_written, 1);
    assert_eq!(report.unfinished_paths, vec!["b.rs".to_string()]);
    assert_eq!(report.status, "BudgetExhausted");
}

/// The budget binds WITHIN a file too: a file with more callees than the budget is truncated,
/// rides the backlog, and the NEXT pass resumes at the unverdicted callees (the covered-skip
/// continuation) instead of re-requesting from the top (#534 review).
#[test]
fn live_pass_budget_continuation_resumes_within_a_file() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", DEFS_SRC);
    h.add_symbol(defs, "target", 0, 13);
    // Three calls in one file: callees at 14, 24, 34.
    let src = h.add_file("src.rs", "fn caller() { target(); target(); target(); }\n");
    h.add_edge(src, "target", 14, 20, "NameOnly", None);
    h.add_edge(src, "target", 24, 30, "NameOnly", None);
    h.add_edge(src, "target", 34, 40, "NameOnly", None);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");

    // Pass 1, budget 2: two callees resolve, the file rides with one deferred.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let worklist = vec!["src.rs".to_string()];
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 2)).unwrap();
    assert_eq!(report.rows_written, 2);
    assert_eq!(report.unfinished_paths, vec!["src.rs".to_string()]);

    // Pass 2 (same session + version): the two covered callees are skipped, so the whole
    // remaining budget goes to the ONE unverdicted callee — exactly one request.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 2)).unwrap();
    assert_eq!(report.requests_used, 1, "covered callees don't spend the budget");
    assert_eq!(report.rows_written, 1);
    assert!(report.unfinished_paths.is_empty(), "the file drains this pass");
    assert_eq!(report.status, "Completed");
}

/// A `rust-analyzer` upgrade between sessions must not strand prior verdicts: the first pass
/// under the new `tool_version` copies them (content-addressed, still `file_sha`-gated) and
/// records the run that makes the new version current — otherwise the currency gate collapses
/// live coverage to the few files the new session revisited (#534 review).
#[test]
fn live_pass_migrates_prior_verdicts_across_a_version_change() {
    let h = Harness::new();
    let (_src, _target, edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let worklist = vec!["src.rs".to_string()];

    // Pass 1 under version LIVE_VERSION.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    let sentinel = scip_symbol_of(&h, edge);
    let version_of = |v: &str| {
        h.conn
            .query_row(
                "SELECT COUNT(*) FROM edge_oracle WHERE tool = 'ra-lsp' AND tool_version = ?1",
                params![v],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(version_of(LIVE_VERSION), 1);

    // A NEW session probing a NEW rust-analyzer version; an EMPTY worklist (a quiet pass).
    let client = client_with_server(move |msg: &Value| {
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => Some(vec![json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"capabilities": {"positionEncoding": "utf-16"}}
            })]),
            _ if id.is_none() => Some(vec![]),
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    });
    let mut session = LiveOracleSession::from_client(client, "ra-test-2", &uri);
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &[], 100)).unwrap();

    assert!(report.version_migrated);
    assert!(report.refinements_invalidated, "the evidence changed hands");
    assert_eq!(report.status, "VersionMigrated");
    assert_eq!(version_of(LIVE_VERSION), 1, "old-version rows remain for sibling currency");
    assert_eq!(version_of("ra-test-2"), 1, "rows are copied under the new version");
    // The migrated verdict survives with its content + symbol intact.
    let (kind, resolved, symbol) = h.verdict(edge).expect("migrated verdict persists");
    assert_eq!(kind, "upgrade");
    assert!(resolved.is_some());
    assert_eq!(symbol, sentinel);
    // And the new run row makes the new version the currency gate's latest.
    let latest: String = h
        .conn
        .query_row(
            "SELECT tool_version FROM oracle_runs WHERE tool = 'ra-lsp' ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(latest, "ra-test-2");
}

/// Two edges sharing ONE callee token (`calls_name` + `references_type`) must not starve each
/// other: coverage is keyed by the FULL content key, so a budget split between them resumes the
/// deferred edge instead of treating the shared start byte as covered (#534 review).
#[test]
fn live_pass_continuation_keys_coverage_by_the_full_edge_identity() {
    let h = Harness::new();
    let defs = h.add_file("defs.rs", DEFS_SRC);
    h.add_symbol(defs, "target", 0, 13);
    let src = h.add_file("src.rs", CALLER_SRC);
    // Two edge kinds on the SAME token (14..20).
    let call = h.add_edge(src, "target", 14, 20, "NameOnly", None);
    let refty = h.add_edge_with_kind(src, "target", 14, 20, "references_type", "NameOnly", None);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");

    // Pass 1, budget 1: ONE of the two edges resolves; the other is deferred.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let worklist = vec!["src.rs".to_string()];
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 1)).unwrap();
    assert_eq!(report.rows_written, 1);
    assert_eq!(report.unfinished_paths, vec!["src.rs".to_string()]);

    // Pass 2: the deferred edge must NOT read as covered by the shared start byte — it resolves
    // (one more request) and both verdicts persist.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 1)).unwrap();
    assert_eq!(report.requests_used, 1);
    assert_eq!(report.rows_written, 1);
    assert!(report.unfinished_paths.is_empty());
    assert!(h.verdict(call).is_some() || h.verdict(refty).is_some());
    let both = h
        .conn
        .query_row("SELECT COUNT(*) FROM edge_oracle WHERE tool = 'ra-lsp'", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(both, 2, "both token-sharing edges have verdicts");
}

/// The version migration is SCOPED to the active checkout (#534 review): a sibling worktree's
/// `ra-lsp` rows must not be relabeled — only the migrating checkout's currency advances.
#[test]
fn live_version_migration_leaves_a_sibling_checkouts_rows_alone() {
    let h = Harness::new();
    let (_src, _target, edge) = seed_corpus(&h);
    // A sibling checkout with its own file + edge + ra-lsp verdict under the OLD version.
    let sib_file = h.add_file_in_scope("sibling.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let sib_edge = h.add_edge(sib_file, "t", 9, 10, "NameOnly", None);
    let sib_key = h.edge_content_key(sib_edge);
    crate::store::write_edge_oracle(
        &h.conn,
        OracleTool::RaLsp,
        "old-v",
        &crate::store::EdgeOracleRow {
            source_path: &sib_key.source_path,
            source_start_byte: sib_key.source_start_byte,
            source_end_byte: sib_key.source_end_byte,
            callee_start_byte: sib_key.callee_start_byte,
            callee_end_byte: sib_key.callee_end_byte,
            edge_kind: &sib_key.edge_kind,
            file_sha: "sib-sha",
            resolved_symbol_id: None,
            scip_symbol: "local ra-lsp-sib",
            kind: OracleResolutionKind::Upgrade,
        },
    )
    .unwrap();
    // The active checkout's ra-lsp verdict under the OLD version (from a prior pass).
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let worklist = vec!["src.rs".to_string()];
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    // Relabel the active checkout's row to the old version too (both start at old-v).
    h.conn
        .execute("UPDATE edge_oracle SET tool_version = 'old-v' WHERE tool = 'ra-lsp'", [])
        .unwrap();

    let moved = crate::store::migrate_live_verdicts_to_version(
        &h.conn,
        OracleTool::RaLsp,
        "old-v",
        "new-v",
        COMMIT,
        WORKTREE,
    )
    .unwrap();

    assert_eq!(
        moved,
        crate::store::LiveVersionMigration::Copied(1),
        "only the active checkout's row copies"
    );
    let sibling_version: String = h
        .conn
        .query_row(
            "SELECT tool_version FROM edge_oracle WHERE source_path = 'sibling.rs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sibling_version, "old-v", "the sibling checkout's row keeps its version");
    let _ = edge;
}

/// Identical-content sibling checkouts SHARE the same content-keyed verdict row. A version
/// transition must copy that row, not relabel it: the active checkout selects the new version while
/// the sibling's still-old currency needs the old row to remain visible.
#[test]
fn live_version_migration_preserves_a_shared_old_version_row() {
    let h = Harness::new();
    let (_src, _target, edge) = seed_corpus(&h);
    let key = h.edge_content_key(edge);
    let active_sha = h.file_sha("src.rs");
    crate::store::write_edge_oracle(
        &h.conn,
        OracleTool::RaLsp,
        "old-v",
        &crate::store::EdgeOracleRow {
            source_path: &key.source_path,
            source_start_byte: key.source_start_byte,
            source_end_byte: key.source_end_byte,
            callee_start_byte: key.callee_start_byte,
            callee_end_byte: key.callee_end_byte,
            edge_kind: &key.edge_kind,
            file_sha: &active_sha,
            resolved_symbol_id: None,
            scip_symbol: "local ra-lsp-shared",
            kind: OracleResolutionKind::Upgrade,
        },
    )
    .unwrap();

    // Model another checkout with the SAME path, bytes, and edge spans: both scopes join to the
    // one old-version edge_oracle row.
    let sibling_file = h.add_file_in_scope("src.rs", OTHER_COMMIT, OTHER_WORKTREE);
    h.set_file_sha(sibling_file, &active_sha);
    h.add_edge(sibling_file, "target", 14, 20, "NameOnly", None);

    let copied = crate::store::migrate_live_verdicts_to_version(
        &h.conn,
        OracleTool::RaLsp,
        "old-v",
        "new-v",
        COMMIT,
        WORKTREE,
    )
    .unwrap();
    assert_eq!(copied, crate::store::LiveVersionMigration::Copied(1));

    let mut versions = h
        .conn
        .prepare("SELECT tool_version FROM edge_oracle WHERE tool = 'ra-lsp' ORDER BY tool_version")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    versions.dedup();
    assert_eq!(versions, ["new-v", "old-v"], "both checkout currencies retain coverage");
}

/// The content-key PK excludes `file_sha`, so two different file versions cannot both occupy the
/// same edge key + tool version. The transition must remain on the old currency instead of letting
/// a sibling's destination row make active evidence disappear.
#[test]
fn live_version_migration_blocks_a_different_content_destination_collision() {
    let h = Harness::new();
    let (_src, _target, edge) = seed_corpus(&h);
    let key = h.edge_content_key(edge);
    let active_sha = h.file_sha("src.rs");
    crate::store::write_edge_oracle(
        &h.conn,
        OracleTool::RaLsp,
        LIVE_VERSION,
        &crate::store::EdgeOracleRow {
            source_path: &key.source_path,
            source_start_byte: key.source_start_byte,
            source_end_byte: key.source_end_byte,
            callee_start_byte: key.callee_start_byte,
            callee_end_byte: key.callee_end_byte,
            edge_kind: &key.edge_kind,
            file_sha: &active_sha,
            resolved_symbol_id: None,
            scip_symbol: "local ra-lsp-active",
            kind: OracleResolutionKind::Upgrade,
        },
    )
    .unwrap();
    crate::store::record_oracle_run(
        &h.conn,
        OracleTool::RaLsp,
        LIVE_VERSION,
        COMMIT,
        WORKTREE,
        "Completed",
        "{}",
    )
    .unwrap();

    let sibling_file = h.add_file_in_scope("src.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let sibling_sha = h.file_sha_for_commit("src.rs", OTHER_COMMIT);
    h.add_edge(sibling_file, "target", 14, 20, "NameOnly", None);
    crate::store::write_edge_oracle(
        &h.conn,
        OracleTool::RaLsp,
        "ra-test-2",
        &crate::store::EdgeOracleRow {
            source_path: &key.source_path,
            source_start_byte: key.source_start_byte,
            source_end_byte: key.source_end_byte,
            callee_start_byte: key.callee_start_byte,
            callee_end_byte: key.callee_end_byte,
            edge_kind: &key.edge_kind,
            file_sha: &sibling_sha,
            resolved_symbol_id: None,
            scip_symbol: "local ra-lsp-sibling",
            kind: OracleResolutionKind::Upgrade,
        },
    )
    .unwrap();

    let client = client_with_server(move |msg: &Value| {
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => Some(vec![json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"capabilities": {"positionEncoding": "utf-16"}}
            })]),
            _ if id.is_none() => Some(vec![]),
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    });
    let uri = root_uri(&h);
    let mut session = LiveOracleSession::from_client(client, "ra-test-2", &uri);
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &[], 100)).unwrap();

    assert_eq!(report.status, "VersionMigrationBlocked");
    assert!(!report.version_migrated);
    assert_eq!(
        crate::store::latest_run_tool_version(&h.conn, OracleTool::RaLsp, COMMIT, WORKTREE)
            .unwrap()
            .as_deref(),
        Some(LIVE_VERSION),
        "active currency stays on its still-valid old row"
    );
    assert_eq!(scip_symbol_of(&h, edge), "local ra-lsp-active");
}

/// A destination row for bytes no live checkout still owns is retained history, not a collision.
/// Version migration must replace it with the active checkout's current evidence.
#[test]
fn live_version_migration_replaces_a_stale_destination_collision() {
    let h = Harness::new();
    let (_src, _target, edge) = seed_corpus(&h);
    let key = h.edge_content_key(edge);
    let active_sha = h.file_sha("src.rs");
    crate::store::write_edge_oracle(
        &h.conn,
        OracleTool::RaLsp,
        "old-v",
        &crate::store::EdgeOracleRow {
            source_path: &key.source_path,
            source_start_byte: key.source_start_byte,
            source_end_byte: key.source_end_byte,
            callee_start_byte: key.callee_start_byte,
            callee_end_byte: key.callee_end_byte,
            edge_kind: &key.edge_kind,
            file_sha: &active_sha,
            resolved_symbol_id: None,
            scip_symbol: "local ra-lsp-current",
            kind: OracleResolutionKind::Upgrade,
        },
    )
    .unwrap();
    crate::store::write_edge_oracle(
        &h.conn,
        OracleTool::RaLsp,
        "new-v",
        &crate::store::EdgeOracleRow {
            source_path: &key.source_path,
            source_start_byte: key.source_start_byte,
            source_end_byte: key.source_end_byte,
            callee_start_byte: key.callee_start_byte,
            callee_end_byte: key.callee_end_byte,
            edge_kind: &key.edge_kind,
            file_sha: "stale-unowned-sha",
            resolved_symbol_id: None,
            scip_symbol: "local ra-lsp-stale",
            kind: OracleResolutionKind::Upgrade,
        },
    )
    .unwrap();

    let copied = crate::store::migrate_live_verdicts_to_version(
        &h.conn,
        OracleTool::RaLsp,
        "old-v",
        "new-v",
        COMMIT,
        WORKTREE,
    )
    .unwrap();
    assert_eq!(copied, crate::store::LiveVersionMigration::Copied(1));
    let (sha, symbol): (String, String) = h
        .conn
        .query_row(
            "SELECT file_sha, scip_symbol FROM edge_oracle
             WHERE tool = 'ra-lsp' AND tool_version = 'new-v'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((sha, symbol), (active_sha, "local ra-lsp-current".into()));
}

#[test]
fn live_same_version_write_preserves_a_current_sibling_content_row() {
    let h = Harness::new();
    let (_src, _target, edge) = seed_corpus(&h);
    let key = h.edge_content_key(edge);
    let sibling_file = h.add_file_in_scope("src.rs", OTHER_COMMIT, OTHER_WORKTREE);
    let sibling_sha = h.file_sha_for_commit("src.rs", OTHER_COMMIT);
    h.add_edge(sibling_file, "target", 14, 20, "NameOnly", None);
    crate::store::write_edge_oracle(
        &h.conn,
        OracleTool::RaLsp,
        LIVE_VERSION,
        &crate::store::EdgeOracleRow {
            source_path: &key.source_path,
            source_start_byte: key.source_start_byte,
            source_end_byte: key.source_end_byte,
            callee_start_byte: key.callee_start_byte,
            callee_end_byte: key.callee_end_byte,
            edge_kind: &key.edge_kind,
            file_sha: &sibling_sha,
            resolved_symbol_id: None,
            scip_symbol: "local ra-lsp-sibling",
            kind: OracleResolutionKind::Upgrade,
        },
    )
    .unwrap();

    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let mut session = fake_session(&uri, Some((def, (0, 3), (0, 9))));
    let worklist = vec!["src.rs".to_string()];
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(report.rows_written, 0);
    assert_eq!(report.skipped_content_collisions, 1);
    let (stored_sha, stored_symbol): (String, String) = h
        .conn
        .query_row(
            "SELECT file_sha, scip_symbol FROM edge_oracle
             WHERE tool = 'ra-lsp' AND tool_version = ?1",
            [LIVE_VERSION],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((stored_sha, stored_symbol), (sibling_sha, "local ra-lsp-sibling".into()));
}

#[test]
fn live_pass_aborts_best_effort_when_the_server_dies_mid_pass() {
    let h = Harness::new();
    let (_src, _target, _edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    // The server answers initialize, then CLOSES on the first definition request (a crash).
    let client = client_with_server(move |msg: &Value| {
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => Some(vec![json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"capabilities": {"positionEncoding": "utf-16"}}
            })]),
            Some("textDocument/definition") => None,
            _ if id.is_none() => Some(vec![]),
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    });
    let mut session = LiveOracleSession::from_client(client, LIVE_VERSION, &uri);
    let worklist = vec!["src.rs".to_string()];
    // Never an `Err` — the maintenance pass must survive a dead server (#535 hardens further).
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert!(report.status.starts_with("Aborted:"), "{}", report.status);
    // A dead server is reported as the SERVER's abort, not the checkout's: the layout is exactly
    // what it was, so the paths a session skipped as unconfigurable are as unresolvable as before
    // and the watcher must not requeue them off the back of this.
    assert_eq!(report.abort, Some(LivePassAbort::Server));
    assert_eq!(report.rows_written, 0);
    // The failed file is REQUEUED (not dropped): the watcher rides it into the next pass with a
    // freshly spawned session.
    assert_eq!(report.unfinished_paths, vec!["src.rs".to_string()]);
    assert_eq!(live_run_count(&h.conn), 0);
}

/// The TypeScript live backend (#536). These lock the behaviours that differ from `ra-lsp` — the
/// readiness signal, the warm-up that makes that signal reachable, the `languageId` documents are
/// opened under, and the sentinel namespace — against the same fake-server seam.
mod typescript {
    use super::*;
    use crate::OracleTool;
    use crate::lsp::client::test_support::client_with_server_policy;
    use crate::lsp::readiness::ReadinessPolicy;

    const TS_VERSION: &str = "ts-test-1";
    /// `run` calls `greet`; the callee identifier sits at byte 20.
    const TS_CALLER_SRC: &str = "export function run() { greet(); }\n";
    const TS_CALLEE_START: usize = 24;
    const TS_CALLEE_END: usize = 29;
    const TS_DEFS_SRC: &str = "export function greet() {}\n";

    /// The two-file TypeScript corpus: `lib.ts` defines `greet`, `main.ts` calls it unresolved.
    /// A `tsconfig.json` makes them a real project, which is what the server needs before it will
    /// report a project load at all — and what the backend's prerequisite gate requires.
    fn seed_ts_corpus(h: &Harness) -> (i64, i64) {
        std::fs::write(h.root().join("tsconfig.json"), "{}").unwrap();
        let defs = h.add_file("lib.ts", TS_DEFS_SRC);
        let target = h.add_symbol(defs, "greet", 0, 25);
        let src = h.add_file("main.ts", TS_CALLER_SRC);
        let edge = h.add_edge(src, "greet", TS_CALLEE_START, TS_CALLEE_END, "NameOnly", None);
        (target, edge)
    }

    fn ts_run_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oracle_runs WHERE tool = 'ts-lsp'", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    /// A TypeScript-backed session over a fake server, recording every method the client sent.
    /// `emit_on_initialize` rides alongside the `initialize` response (the seam for handing the
    /// session a completed progress cycle), and `emit_on_definition` before a definition result.
    fn ts_session(
        h: &Harness,
        emit_on_initialize: Vec<Value>,
        emit_on_definition: Vec<Value>,
        resolve_to: Option<String>,
    ) -> (LiveOracleSession, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&sent);
        let uri = root_uri(h);
        let client = client_with_server_policy(
            move |msg: &Value| {
                let id = msg.get("id").cloned();
                let method = msg.get("method").and_then(Value::as_str);
                if let Some(method) = method {
                    // Record the opened URI too: the warm-up asserts WHICH document was chosen.
                    let entry = match method {
                        "textDocument/didOpen" => format!(
                            "didOpen:{}",
                            msg["params"]["textDocument"]["uri"].as_str().unwrap_or_default()
                        ),
                        other => other.to_string(),
                    };
                    captured.lock().unwrap().push(entry);
                }
                match method {
                    Some("initialize") => {
                        let mut out = emit_on_initialize.clone();
                        out.push(json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"capabilities": {}}
                        }));
                        Some(out)
                    },
                    Some("textDocument/definition") => {
                        let mut out = emit_on_definition.clone();
                        let result = match &resolve_to {
                            Some(target) => json!({
                                "targetUri": target,
                                "targetSelectionRange": {
                                    "start": {"line": 0, "character": 16},
                                    "end": {"line": 0, "character": 21}
                                }
                            }),
                            None => Value::Null,
                        };
                        out.push(json!({"jsonrpc": "2.0", "id": id, "result": result}));
                        Some(out)
                    },
                    _ if id.is_none() => Some(vec![]),
                    _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
                }
            },
            ReadinessPolicy::WorkDoneProgress,
        );
        let session =
            LiveOracleSession::from_warming_client_for(OracleTool::TsLsp, client, TS_VERSION, &uri);
        (session, sent)
    }

    #[test]
    fn an_unwarmed_session_opens_a_document_to_trigger_the_signal_it_is_waiting_for() {
        // typescript-language-server starts its project load on the first didOpen, not at
        // initialize, so a pass that waits for readiness without opening anything waits forever.
        // The warm-up open breaks that deadlock — WITHOUT asking a single definition, because the
        // answers during a load are wrong rather than null.
        let h = Harness::new();
        seed_ts_corpus(&h);
        let (mut session, sent) = ts_session(&h, Vec::new(), Vec::new(), None);
        let worklist = vec!["main.ts".to_string()];

        let report =
            live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

        assert_eq!(report.status, "Warming");
        assert_eq!(report.unfinished_paths, worklist, "the work rides to the next pass");
        assert_eq!(report.requests_used, 0);
        // The warm-up is notification-only, so a synchronous round trip is what proves the
        // server has seen it before the assertions read the recorded traffic.
        session.barrier();
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter().any(|entry| entry.starts_with("didOpen:")),
            "the warm-up must open a document: {sent:?}"
        );
        assert!(
            !sent.iter().any(|method| method == "textDocument/definition"),
            "a warming server must never be asked for a definition: {sent:?}"
        );
        assert_eq!(ts_run_count(&h.conn), 0);
    }

    #[test]
    fn a_completed_progress_cycle_lets_the_next_pass_write_ts_lsp_verdicts() {
        // The pass after warm-up finds the latched ready state and resolves normally, writing
        // under the ts-lsp tool id with its own sentinel namespace.
        let h = Harness::new();
        let (target, edge) = seed_ts_corpus(&h);
        let definition = def_uri(&h, "lib.ts");
        let (mut session, sent) = ts_session(
            &h,
            vec![progress("load", "begin"), progress("load", "end")],
            Vec::new(),
            Some(definition),
        );
        let worklist = vec!["main.ts".to_string()];

        let report =
            live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

        assert_eq!(report.status, "Completed");
        assert_eq!(report.rows_written, 1);
        assert_eq!(report.upgraded, 1);
        let (kind, resolved, symbol) = h.verdict(edge).expect("verdict persisted");
        assert_eq!(kind, "upgrade");
        assert_eq!(resolved, Some(target));
        assert!(
            symbol.starts_with("local ts-lsp-"),
            "a live TS verdict must not borrow another backend's sentinel namespace: {symbol}"
        );
        assert_eq!(ts_run_count(&h.conn), 1, "the run row is written under the live TS tool id");
        assert!(sent.lock().unwrap().iter().any(|method| method == "textDocument/definition"));
    }

    #[test]
    fn a_project_load_starting_mid_batch_discards_the_whole_batch() {
        // The empirical failure this backend exists to survive: asked during a project load,
        // typescript-language-server answers an imported callee with the IMPORT STATEMENT in the
        // calling file — a plausible non-null that would persist as a real verdict and never be
        // revisited until the file's bytes change. A load that begins while the batch is in
        // flight must therefore invalidate the batch, not merely be noted.
        let h = Harness::new();
        let (_target, edge) = seed_ts_corpus(&h);
        let definition = def_uri(&h, "lib.ts");
        let (mut session, _sent) = ts_session(
            &h,
            vec![progress("load", "begin"), progress("load", "end")],
            vec![progress("reload", "begin")],
            Some(definition),
        );
        let worklist = vec!["main.ts".to_string()];

        let report =
            live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

        assert_eq!(report.status, "Warming");
        assert_eq!(report.rows_written, 0, "a batch that straddled a load must write nothing");
        assert!(h.verdict(edge).is_none(), "no verdict may survive the discarded batch");
        assert_eq!(report.unfinished_paths, worklist, "the file retries once the load settles");
        assert_eq!(ts_run_count(&h.conn), 0);
    }

    // A TypeScript spawn declining on its unmet prerequisite rather than on its binary is pinned
    // by `live::tests::a_checkout_with_neither_a_server_nor_a_project_reports_the_missing_server`,
    // which supplies availability instead of probing for it. Asserting that through the real
    // `spawn` made the outcome depend on whether the machine happened to have the server
    // installed — it read as a prerequisite block on a developer box and as a missing binary
    // anywhere else.

    #[test]
    fn a_ts_pass_leaves_a_sibling_checkout_and_the_other_live_tool_untouched() {
        // This backend is the SECOND writer into `edge_oracle`/`oracle_runs`, whose rows are keyed
        // by (tool, tool_version, content, commit, worktree). Two dimensions therefore have to
        // hold at once: a sibling checkout's rows must survive a pass in this one, and the two
        // live tools must not read or overwrite each other's verdicts on their own edges.
        let h = Harness::new();
        let (target, edge) = seed_ts_corpus(&h);

        // A SIBLING checkout with its own edge and its own ts-lsp verdict.
        let sibling_file = h.add_file_in_scope("other.ts", OTHER_COMMIT, OTHER_WORKTREE);
        let sibling_edge = h.add_edge(sibling_file, "greet", 9, 14, "NameOnly", None);
        let sibling_key = h.edge_content_key(sibling_edge);
        crate::store::write_edge_oracle(&h.conn, OracleTool::TsLsp, TS_VERSION, &{
            crate::store::EdgeOracleRow {
                source_path: &sibling_key.source_path,
                source_start_byte: sibling_key.source_start_byte,
                source_end_byte: sibling_key.source_end_byte,
                callee_start_byte: sibling_key.callee_start_byte,
                callee_end_byte: sibling_key.callee_end_byte,
                edge_kind: &sibling_key.edge_kind,
                file_sha: "sibling-sha",
                resolved_symbol_id: None,
                scip_symbol: "local ts-lsp-sibling",
                kind: OracleResolutionKind::Upgrade,
            }
        })
        .unwrap();

        // A RUST edge in THIS checkout, already carrying an ra-lsp verdict from its own backend.
        let rust_defs = h.add_file("defs.rs", DEFS_SRC);
        h.add_symbol(rust_defs, "target", 0, 13);
        let rust_src = h.add_file("caller.rs", CALLER_SRC);
        let rust_edge = h.add_edge(
            rust_src,
            "target",
            CALLER_CALLEE_START,
            CALLER_CALLEE_END,
            "NameOnly",
            None,
        );
        let rust_key = h.edge_content_key(rust_edge);
        crate::store::write_edge_oracle(&h.conn, OracleTool::RaLsp, LIVE_VERSION, &{
            crate::store::EdgeOracleRow {
                source_path: &rust_key.source_path,
                source_start_byte: rust_key.source_start_byte,
                source_end_byte: rust_key.source_end_byte,
                callee_start_byte: rust_key.callee_start_byte,
                callee_end_byte: rust_key.callee_end_byte,
                edge_kind: &rust_key.edge_kind,
                file_sha: "rust-sha",
                resolved_symbol_id: None,
                scip_symbol: "local ra-lsp-rust",
                kind: OracleResolutionKind::Upgrade,
            }
        })
        .unwrap();

        let definition = def_uri(&h, "lib.ts");
        let (mut session, _sent) = ts_session(
            &h,
            vec![progress("load", "begin"), progress("load", "end")],
            Vec::new(),
            Some(definition),
        );
        let worklist = vec!["main.ts".to_string()];
        let report =
            live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
        assert_eq!(report.rows_written, 1, "{report:?}");

        // This checkout's TypeScript edge got its verdict under ts-lsp…
        assert_eq!(h.verdict(edge).expect("a ts verdict").1, Some(target));
        // …the sibling checkout's ts-lsp row is untouched (a pass here is not a whole-tool clear)…
        // `edge_oracle` is CONTENT-keyed — checkout scope comes from the join to edges/files, not
        // from a column — so the sibling's row is identified by its own source path.
        let sibling_symbol: String = h
            .conn
            .query_row(
                "SELECT scip_symbol FROM edge_oracle WHERE tool = 'ts-lsp' AND source_path = ?1",
                rusqlite::params![sibling_key.source_path],
                |row| row.get(0),
            )
            .expect("the sibling checkout's row survives");
        assert_eq!(sibling_symbol, "local ts-lsp-sibling");
        // …and the OTHER live tool's verdict on its own Rust edge is neither read nor rewritten.
        let rust_symbol: String = h
            .conn
            .query_row("SELECT scip_symbol FROM edge_oracle WHERE tool = 'ra-lsp'", [], |row| {
                row.get(0)
            })
            .expect("the ra-lsp row survives");
        assert_eq!(rust_symbol, "local ra-lsp-rust");
        // The run row is scoped to this checkout and this tool only.
        assert_eq!(ts_run_count(&h.conn), 1);
    }

    #[test]
    fn a_checkout_with_no_project_is_not_re_opened_every_pass() {
        // Opening a project-less document emits no progress cycle, so it cannot warm anything.
        // Doing it anyway would burn a notification per pass forever. (The manifest gate normally
        // stops such a checkout before a session is even spawned; this pins the pass-level
        // behaviour so the two layers agree.)
        let h = Harness::new();
        let src = h.add_file("main.ts", TS_CALLER_SRC);
        h.add_edge(src, "greet", TS_CALLEE_START, TS_CALLEE_END, "NameOnly", None);
        let (mut session, sent) = ts_session(&h, Vec::new(), Vec::new(), None);
        let worklist = vec!["main.ts".to_string()];

        let report =
            live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

        assert_eq!(report.status, "Warming");
        session.barrier();
        let sent = sent.lock().unwrap();
        assert!(
            !sent.iter().any(|entry| entry.starts_with("didOpen:")),
            "nothing in this checkout can signal readiness, so nothing is worth opening: {sent:?}",
        );
    }

    #[test]
    fn the_warmup_open_picks_a_file_that_can_actually_produce_the_signal() {
        // typescript-language-server brackets a TSCONFIG PROJECT's load in a progress cycle;
        // opening a file that belongs to no project creates an inferred project silently, with no
        // cycle at all. Warming on such a file leaves the session exactly as un-warmed as before,
        // so the worklist's first entry is the wrong choice when a sibling IS inside a project.
        let h = Harness::new();
        std::fs::create_dir_all(h.root().join("pkg/src")).unwrap();
        std::fs::create_dir_all(h.root().join("scripts")).unwrap();
        std::fs::write(h.root().join("pkg/tsconfig.json"), "{}").unwrap();
        let defs = h.add_file("pkg/src/lib.ts", TS_DEFS_SRC);
        h.add_symbol(defs, "greet", 0, 25);
        let loose = h.add_file("scripts/tool.ts", TS_CALLER_SRC);
        h.add_edge(loose, "greet", TS_CALLEE_START, TS_CALLEE_END, "NameOnly", None);
        let inside = h.add_file("pkg/src/main.ts", TS_CALLER_SRC);
        h.add_edge(inside, "greet", TS_CALLEE_START, TS_CALLEE_END, "NameOnly", None);

        let (mut session, sent) = ts_session(&h, Vec::new(), Vec::new(), None);
        // The project-less file comes FIRST in the worklist; the warm-up must skip past it.
        let worklist = vec!["scripts/tool.ts".to_string(), "pkg/src/main.ts".to_string()];

        let report =
            live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

        assert_eq!(report.status, "Warming");
        session.barrier();
        let opened: Vec<String> = sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|entry| entry.strip_prefix("didOpen:").map(str::to_string))
            .collect();
        assert!(
            opened.iter().all(|uri| uri.ends_with("pkg/src/main.ts")),
            "the warm-up must open the file inside the tsconfig project, not {opened:?}",
        );
    }

    #[test]
    fn documents_are_opened_under_the_extension_s_language_id() {
        // tsserver rejects (and other servers mis-parse) a document declared under the wrong
        // languageId, so `.tsx` must open as typescriptreact even though it shares a backend.
        let h = Harness::new();
        let defs = h.add_file("lib.ts", TS_DEFS_SRC);
        h.add_symbol(defs, "greet", 0, 25);
        let src = h.add_file("view.tsx", TS_CALLER_SRC);
        h.add_edge(src, "greet", TS_CALLEE_START, TS_CALLEE_END, "NameOnly", None);
        let opened = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&opened);
        let uri = root_uri(&h);
        let client = client_with_server_policy(
            move |msg: &Value| {
                let id = msg.get("id").cloned();
                if msg.get("method").and_then(Value::as_str) == Some("textDocument/didOpen") {
                    captured.lock().unwrap().push(
                        msg["params"]["textDocument"]["languageId"].as_str().unwrap().to_string(),
                    );
                }
                match msg.get("method").and_then(Value::as_str) {
                    Some("initialize") => Some(vec![
                        progress("load", "begin"),
                        progress("load", "end"),
                        json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}}),
                    ]),
                    _ if id.is_none() => Some(vec![]),
                    _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
                }
            },
            ReadinessPolicy::WorkDoneProgress,
        );
        let mut session =
            LiveOracleSession::from_warming_client_for(OracleTool::TsLsp, client, TS_VERSION, &uri);
        let worklist = vec!["view.tsx".to_string()];

        live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

        assert_eq!(opened.lock().unwrap().as_slice(), ["typescriptreact"]);
    }
}

/// The live clangd backend (#536). Its project marker is a compilation database the SERVER
/// discovers, not a config that encloses its own sources, which makes two pass branches reachable
/// that no other backend has: the session ends when the checkout stops pointing at the database it
/// was spawned against, and a file whose database the server cannot find on its own is skipped
/// rather than resolved. Both depend on a session carrying a REAL layout, so these resolve one with
/// [`LiveBackend::resolve_layout`] over a fixture checkout rather than building one by hand.
mod clangd {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::OracleTool;
    use crate::backend::{self, LiveBackend, ProjectLayout};
    use crate::live::InjectedSession;
    use crate::lsp::client::test_support::client_with_server_policy;
    use crate::lsp::readiness::ReadinessPolicy;

    const CLANGD_VERSION: &str = "clangd-test-1";
    /// `caller` calls `target`; the callee identifier sits at byte 20.
    const C_CALLER_SRC: &str = "void caller(void) { target(); }\n";
    const C_CALLEE_START: usize = 20;
    const C_CALLEE_END: usize = 26;
    /// The same caller with TWO calls: callee identifiers at bytes 20 and 30.
    const C_CALLER_TWICE_SRC: &str = "void caller(void) { target(); target(); }\n";
    /// The definition file: `target`'s identifier span is (0,5)–(0,11), inside the symbol's 0–20.
    const C_DEFS_SRC: &str = "void target(void) {}\n";
    /// A compilation database with one complete entry. `[]` parses but names no translation unit,
    /// and the layout resolver classes such a file UNUSABLE — a fixture written that way would put
    /// the checkout in a different state than the one under test.
    const COMPDB: &str = r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"}]"#;

    fn clangd_run_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oracle_runs WHERE tool = 'clangd-lsp'", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    /// Write a usable compilation database in `relative_dir`, creating it if needed.
    fn write_compdb(h: &Harness, relative_dir: &str) {
        let dir = h.root().join(relative_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    }

    /// [`Harness::add_file`] for a path in a subdirectory, which it does not create itself.
    fn add_c_file(h: &Harness, path: &str, contents: &str) -> i64 {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(h.root().join(parent)).unwrap();
        }
        h.add_file(path, contents)
    }

    /// The layout the real resolver finds in the fixture checkout as it stands right now.
    fn clangd_layout(h: &Harness) -> ProjectLayout {
        LiveBackend::for_tool(OracleTool::ClangdLsp)
            .expect("a live backend")
            .resolve_layout(h.root())
    }

    /// A clangd-backed session over a fake server that reports a completed index cycle alongside
    /// `initialize` and answers every definition with `definition` (or a `null`). The session
    /// behaves as though it resolved `layout` `resolved_ago` ago. Also returns every document URI
    /// the client opened or asked about.
    fn clangd_session(
        h: &Harness,
        layout: ProjectLayout,
        resolved_ago: Duration,
        definition: Option<FakeDefTarget>,
    ) -> (LiveOracleSession, Arc<Mutex<Vec<String>>>) {
        let touched = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&touched);
        let client = client_with_server_policy(
            move |msg: &Value| {
                let id = msg.get("id").cloned();
                let method = msg.get("method").and_then(Value::as_str);
                if matches!(method, Some("textDocument/didOpen" | "textDocument/definition")) {
                    let uri = msg["params"]["textDocument"]["uri"].as_str().unwrap_or_default();
                    captured.lock().unwrap().push(uri.to_string());
                }
                match method {
                    Some("initialize") => Some(vec![
                        progress("index", "begin"),
                        progress("index", "end"),
                        json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}}),
                    ]),
                    Some("textDocument/definition") => {
                        let result = match &definition {
                            Some((uri, (sl, sc), (el, ec))) => json!({
                                "uri": uri,
                                "range": {"start": {"line": sl, "character": sc},
                                          "end": {"line": el, "character": ec}}
                            }),
                            None => Value::Null,
                        };
                        Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": result})])
                    },
                    _ if id.is_none() => Some(vec![]),
                    _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
                }
            },
            ReadinessPolicy::WorkDoneProgress,
        );
        let session = LiveOracleSession::from_injected(InjectedSession {
            tool: OracleTool::ClangdLsp,
            client,
            tool_version: CLANGD_VERSION,
            root_uri: &root_uri(h),
            layout,
            // Subtracted, never `Instant - Duration`: that panics when the monotonic clock is
            // younger than the age being modelled.
            layout_resolved_at: Instant::now()
                .checked_sub(resolved_ago)
                .expect("the monotonic clock predates the modelled layout age"),
        });
        (session, touched)
    }

    #[test]
    fn an_aged_out_layout_pointing_elsewhere_aborts_the_pass_and_requeues_the_worklist() {
        let h = Harness::new();
        let defs = add_c_file(&h, "src/lib.c", C_DEFS_SRC);
        h.add_symbol(defs, "target", 0, 20);
        let src = add_c_file(&h, "src/main.c", C_CALLER_SRC);
        let edge = h.add_edge(src, "target", C_CALLEE_START, C_CALLEE_END, "NameOnly", None);
        write_compdb(&h, "build");
        // The session pins `build/`; the database then moves. The running server already holds an
        // argv derived from the old layout, so this cannot be corrected in place — resolving the
        // new project's files under the old project's flags selects a different preprocessor
        // branch, and the wrong definition would be persisted as a real verdict.
        let pinned = clangd_layout(&h);
        std::fs::create_dir_all(h.root().join("out")).unwrap();
        std::fs::rename(
            h.root().join("build/compile_commands.json"),
            h.root().join("out/compile_commands.json"),
        )
        .unwrap();
        // Aged past the cache's lifetime, so the pass re-resolves the layout before anything else.
        let (mut session, touched) = clangd_session(
            &h,
            pinned,
            backend::LAYOUT_MAX_AGE + Duration::from_secs(1),
            Some((def_uri(&h, "src/lib.c"), (0, 5), (0, 11))),
        );
        // A path carrying NO candidates rides too: the abort requeues the WORKLIST, not the
        // candidate-bearing part of it.
        let worklist = vec!["src/main.c".to_string(), "src/untouched.c".to_string()];

        let report =
            live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

        // The typed abort is the contract, not the wording: the watcher drops the session for any
        // abort but requeues the paths it could not configure for THIS one only, and a status
        // string is operator-facing text nobody should be parsing to tell those apart.
        assert_eq!(report.abort, Some(LivePassAbort::LayoutChanged));
        assert!(report.status.starts_with("Aborted:"), "{}", report.status);
        assert_eq!(report.unfinished_paths, worklist, "the whole worklist rides the next pass");
        assert_eq!(report.rows_written, 0);
        assert_eq!(report.requests_used, 0);
        assert!(h.verdict(edge).is_none(), "a session whose database moved must write nothing");
        assert_eq!(clangd_run_count(&h.conn), 0);
        assert!(
            touched.lock().unwrap().is_empty(),
            "the layout check precedes every document request, so a session that can never be \
             correct asks nothing",
        );
    }

    #[test]
    fn a_file_whose_database_the_server_cannot_find_is_skipped_and_never_deferred() {
        let h = Harness::new();
        // TWO compilation databases, so no `--compile-commands-dir` is passed (it is global) and
        // each file's database has to be discoverable from its own ancestors or one of their
        // `build/` subdirectories. `a/` holds one; nothing at or above `b/` does.
        write_compdb(&h, "a");
        write_compdb(&h, "z");
        let defs = add_c_file(&h, "a/lib.c", C_DEFS_SRC);
        let target = h.add_symbol(defs, "target", 0, 20);
        let configured = add_c_file(&h, "a/main.c", C_CALLER_SRC);
        let resolvable =
            h.add_edge(configured, "target", C_CALLEE_START, C_CALLEE_END, "NameOnly", None);
        let unconfigurable = add_c_file(&h, "b/main.c", C_CALLER_TWICE_SRC);
        // TWO callees in the skipped file, so the counter is unmistakably candidates, not files.
        let first_skipped = h.add_edge(unconfigurable, "target", 20, 26, "NameOnly", None);
        let second_skipped = h.add_edge(unconfigurable, "target", 30, 36, "NameOnly", None);
        let layout = clangd_layout(&h);
        let (mut session, touched) = clangd_session(
            &h,
            layout,
            Duration::ZERO,
            Some((def_uri(&h, "a/lib.c"), (0, 5), (0, 11))),
        );
        // The unconfigurable file comes FIRST: skipping it must not end the pass.
        let worklist = vec!["b/main.c".to_string(), "a/main.c".to_string()];

        let report =
            live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

        assert_eq!(report.skipped_unconfigured, 2, "both candidates of the unconfigurable file");
        assert_eq!(report.rows_written, 1);
        assert_eq!(report.upgraded, 1);
        assert_eq!(report.requests_used, 1, "only the configured file's callee is asked about");
        assert_eq!(report.status, "Completed");
        assert_eq!(clangd_run_count(&h.conn), 1);
        assert_eq!(
            h.verdict(resolvable).expect("the configured file still resolves").1,
            Some(target),
        );
        assert!(h.verdict(first_skipped).is_none());
        assert!(h.verdict(second_skipped).is_none());
        // NOT deferred: retrying cannot help until the checkout's layout changes, so a backlog
        // entry here would be re-skipped on every pass forever.
        assert!(
            report.unfinished_paths.is_empty(),
            "an unconfigurable path must not ride the backlog: {:?}",
            report.unfinished_paths,
        );
        // Retained for the caller instead — the FILE, once, not its two candidates. Without this
        // the path is simply lost, and the layout change that makes it resolvable has nothing to
        // bring back: it would stay without live evidence until someone edited it again.
        assert_eq!(report.skipped_unconfigured_paths, vec!["b/main.c".to_string()]);
        assert_eq!(report.abort, None, "skipping a file is not an abort");
        // The server never even sees the document it cannot be configured for: with fallback flags
        // its answer would be wrong rather than absent, and a wrong verdict is what this skip
        // exists to prevent.
        let touched = touched.lock().unwrap();
        assert!(!touched.is_empty(), "the configured document did reach the server");
        assert!(
            touched.iter().all(|uri| uri.ends_with("a/main.c")),
            "the unconfigurable document must never reach the server: {touched:?}",
        );
    }
}
