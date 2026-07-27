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
    // The failed file is REQUEUED (not dropped): the watcher rides it into the next pass with a
    // freshly spawned session.
    assert_eq!(report.unfinished_paths, vec!["src.rs".to_string()]);
    assert_eq!(live_run_count(&h.conn), 0);
}
