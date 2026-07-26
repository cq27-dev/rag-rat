//! End-to-end tests for the LIVE oracle pass (#534): a fake in-process LSP server (the slice-1
//! test seam) answers `textDocument/definition`; the real `live_oracle_pass` write path maps the
//! definitions to symbols + batch monikers and persists `ra-lsp` verdicts + a backing run row.
//! No real rust-analyzer, no network.

use serde_json::{Value, json};

use super::*;
use crate::live::{LiveOracleSession, LivePassInput, live_oracle_pass};
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

    live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();

    assert_eq!(
        scip_symbol_of(&h, edge),
        TARGET_MONIKER,
        "the live row carries the batch moniker byte-identically — clone-collapse and memory \
         anchoring treat live and batch evidence as one set"
    );
}

#[test]
fn live_sentinel_is_stable_then_a_batch_moniker_arrival_invalidates_refinements() {
    let h = Harness::new();
    let (_src, target, edge) = seed_corpus(&h);
    let uri = root_uri(&h);
    let def = def_uri(&h, "defs.rs");
    let worklist = vec!["src.rs".to_string()];

    // Pass 1 (no batch baseline): the sentinel.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert!(!report.refinements_invalidated, "no prior row — nothing to invalidate");
    let sentinel = scip_symbol_of(&h, edge);
    assert!(sentinel.starts_with("local ra-lsp-"));

    // Pass 2, same content: the SAME sentinel → a same-value upsert, no refine churn.
    let mut session = fake_session(&uri, Some((def.clone(), (0, 3), (0, 9))));
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert_eq!(scip_symbol_of(&h, edge), sentinel, "sentinel is content-stable");
    assert!(!report.refinements_invalidated, "same-value upsert skips the invalidation");

    // A batch run then lands a moniker; the next live pass copies it — the scip_symbol CHANGES
    // under an unchanged file_sha, so the scip-mode refinements that consulted the sentinel must
    // be invalidated.
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
    let report = live_oracle_pass(&h.conn, &mut session, &pass_input(&h, &worklist, 100)).unwrap();
    assert_eq!(scip_symbol_of(&h, edge), TARGET_MONIKER);
    assert!(report.refinements_invalidated, "changed evidence under an unchanged file_sha");
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
    assert_eq!(report.rows_written, 0);
    assert_eq!(live_run_count(&h.conn), 0);
}
