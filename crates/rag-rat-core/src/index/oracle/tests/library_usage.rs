//! `check_library_usage` (#114): the end-to-end read — run the oracle over a `.scip` that carries a
//! `resolved-external` call site AND its `external_symbols` contract, then assert the join surfaces
//! the current signature/docs and the deprecation verdict.
use super::*;
use crate::index::oracle::{
    LibraryUsageOptions, LibraryUsageStatus, OracleResolutionKind, check_library_usage,
};

/// Build a `.scip` for a single external call site in `caller.rs` plus the external contracts.
fn scip_external_call(
    call_moniker: &str,
    externals: Vec<::scip::types::SymbolInformation>,
) -> Vec<u8> {
    use ::protobuf::{EnumOrUnknown, Message};
    use ::scip::types::{Document, Index};
    Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            // The callee token `fetch` sits at bytes 14..19 of "fn caller() { fetch(); }".
            occurrences: vec![occurrence(
                0,
                14,
                19,
                call_moniker,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        external_symbols: externals,
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap()
}

fn external(
    moniker: &str,
    display_name: &str,
    docs: Vec<String>,
    sig: &str,
) -> ::scip::types::SymbolInformation {
    use ::protobuf::{EnumOrUnknown, MessageField};
    use ::scip::types::symbol_information::Kind;
    use ::scip::types::{Signature, SymbolInformation};
    SymbolInformation {
        symbol: moniker.to_string(),
        display_name: display_name.to_string(),
        kind: EnumOrUnknown::new(Kind::Function),
        documentation: docs,
        signature_documentation: MessageField::some(Signature {
            language: "typescript".to_string(),
            text: sig.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The core join: a `resolved-external` call site + its contract → one entry carrying the current
/// signature (context) and the asserted deprecation verdict, with the call-site location.
#[test]
fn check_library_usage_surfaces_signature_and_asserts_deprecation() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    // NameOnly + no in-corpus target, so the oracle bins the call `resolved-external`.
    let edge = h.add_edge(caller, "fetch", 14, 19, "NameOnly", None);

    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    let bytes = scip_external_call(moniker, vec![external(
        moniker,
        "get",
        vec!["@deprecated use fetch2 instead.".to_string()],
        "get(url: string): ResponsePromise",
    )]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.resolved_external, 1, "the call resolved external");
    assert_eq!(report.external_symbols_written, 1, "the contract persisted");
    let (kind, resolved, _) = h.verdict(edge).expect("verdict written");
    assert_eq!(kind, OracleResolutionKind::ResolvedExternal.as_db_str());
    assert_eq!(resolved, None);

    let out =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(out.status, LibraryUsageStatus::Ok);
    assert_eq!(out.total_external_call_sites, 1);
    assert_eq!(out.deprecated_call_sites, 1);
    assert_eq!(out.call_sites_without_signature_info, 0);
    assert_eq!(out.distinct_monikers, 1);
    assert_eq!(out.entries.len(), 1);

    let entry = &out.entries[0];
    assert_eq!(entry.moniker, moniker);
    assert_eq!(entry.package.as_deref(), Some("ky"));
    assert_eq!(entry.kind, "Function");
    assert!(entry.deprecated, "the @deprecated doc is the asserted verdict");
    assert!(entry.signature_text.contains("ResponsePromise"), "signature surfaced as context");
    assert_eq!(entry.call_count, 1);
    assert_eq!(entry.call_sites[0].path, "caller.rs");
    assert_eq!(entry.call_sites[0].start_line, 0);
}

/// `deprecated_only` keeps a deprecated contract; a non-matching `package` filter excludes it (and
/// zeroes the counts, since the filter runs before tallying).
#[test]
fn check_library_usage_filters_by_deprecation_and_package() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    h.add_edge(caller, "fetch", 14, 19, "NameOnly", None);

    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    let bytes = scip_external_call(moniker, vec![external(
        moniker,
        "get",
        vec!["@deprecated".to_string()],
        "get(url: string): ResponsePromise",
    )]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let deprecated_only = check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions {
        deprecated_only: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(deprecated_only.entries.len(), 1, "deprecated contract kept");

    let other_pkg = check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions {
        package: Some("tokio".to_string()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(other_pkg.entries.len(), 0, "a non-matching package filter excludes the ky call");
    assert_eq!(other_pkg.total_external_call_sites, 0);
    assert_eq!(other_pkg.status, LibraryUsageStatus::Ok);
}

/// Multi-worktree isolation (the #114 review finding): an oracle run in a SIBLING checkout must NOT
/// clobber this checkout's contracts. Checkout A writes its contract; checkout B (a different
/// commit) runs the same tool with a DIFFERENT contract set; A's `check_library_usage` still
/// returns A's contract and never B's.
#[test]
fn check_library_usage_is_isolated_across_checkouts() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    h.add_edge(caller, "fetch", 14, 19, "NameOnly", None);

    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    let bytes = scip_external_call(moniker, vec![external(moniker, "get", vec![], "get(): X")]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    // Checkout B (a DIFFERENT commit) runs the SAME tool with a DIFFERENT contract, and its
    // authoritative per-checkout clear runs against B's scope only.
    let other = "scip-typescript npm dep 2.0.0 index.ts/run().";
    let bytes_b = scip_external_call(other, vec![external(other, "run", vec![], "run(): Y")]);
    run_oracle(
        &h.conn,
        TOOL,
        VERSION,
        OTHER_COMMIT,
        OTHER_WORKTREE,
        &bytes_b,
        h.root(),
        None,
        None,
    )
    .unwrap();

    // A's contract survived B's run (per-checkout scope), and A never sees B's contract.
    let a =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(a.entries.len(), 1, "A keeps exactly its own contract");
    assert_eq!(a.entries[0].moniker, moniker);
    let total: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM external_symbols", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 2, "both checkouts' contracts coexist in the table");
}

/// A dependency call the heuristic MIS-binds to an in-corpus symbol becomes a `contradict` verdict
/// (not `resolved-external`) but still carries the external moniker with a NULL resolved id — it
/// must be counted (#114 review). The `resolved_symbol_id IS NULL` filter captures it.
#[test]
fn check_library_usage_includes_external_contradictions() {
    let h = Harness::new();
    // An in-corpus `fetch` the heuristic (wrongly) binds the call to.
    let defs = h.add_file("defs.rs", "fn fetch() {}\n");
    let sym = h.add_symbol_qualified(defs, "fetch", "defs.rs::fetch", "function", 0, 13);
    h.add_chunk(defs, "defs.rs::fetch", "fn fetch() {}\n");
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    // Exact + a concrete in-corpus target → SCIP resolves external → the oracle records
    // `contradict`.
    h.add_edge(caller, "fetch", 14, 19, "Exact", Some(sym));

    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    let bytes = scip_external_call(moniker, vec![external(moniker, "get", vec![], "get(): X")]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.contradicted, 1, "the mis-bound dependency call is a contradiction");
    assert_eq!(report.resolved_external, 0, "not recorded as resolved-external");

    let out =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(out.total_external_call_sites, 1, "the external contradiction is counted");
    assert_eq!(out.entries.len(), 1);
    assert_eq!(out.entries[0].moniker, moniker);
    assert!(out.entries[0].signature_text.contains('X'), "its contract is surfaced");
}

/// With no oracle run, the report is `NoOracleRun`; a run that emits NO external symbols is
/// `NoExternalSymbols` — the two diagnostics that tell an agent WHY there is nothing to show.
#[test]
fn check_library_usage_reports_missing_oracle_and_missing_external_symbols() {
    let h = Harness::new();
    h.add_file("caller.rs", "fn caller() {}\n");

    // No oracle run yet.
    let no_run =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(no_run.status, LibraryUsageStatus::NoOracleRun);

    // A run whose `.scip` carries NO external_symbols.
    let bytes = {
        use ::protobuf::{EnumOrUnknown, Message};
        use ::scip::types::{Document, Index};
        Index {
            documents: vec![Document {
                relative_path: "caller.rs".to_string(),
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap()
    };
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    let no_ext =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(no_ext.status, LibraryUsageStatus::NoExternalSymbols);
}

/// When external calls EXIST but the indexer emitted no `index.external_symbols`, the report is
/// still `NoExternalSymbols` yet the coverage counts reflect the real gap (#114 review) — not a
/// bare zero that hides how many external calls lack a contract.
#[test]
fn check_library_usage_no_contracts_still_reports_coverage_counts() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    h.add_edge(caller, "fetch", 14, 19, "NameOnly", None);
    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    // The external call occurrence is present, but the `.scip` carries NO external_symbols
    // contract.
    let bytes = scip_external_call(moniker, vec![]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.resolved_external, 1);
    assert_eq!(report.external_symbols_written, 0);

    let out =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(out.status, LibraryUsageStatus::NoExternalSymbols);
    assert_eq!(out.total_external_call_sites, 1, "the external call is still counted");
    assert_eq!(out.call_sites_without_signature_info, 1, "and reported as an uncovered gap");
    assert_eq!(out.distinct_monikers, 0);
    assert!(out.entries.is_empty());
    assert!(
        out.note.contains("oracle run"),
        "the note nudges a rerun (pre-V057 runs lack the data)"
    );
}

fn count_external_symbols(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM external_symbols", [], |r| r.get(0)).unwrap()
}

/// `limit` is a hard cap honored exactly: `limit: 0` returns NO entries, but the summary counts
/// still cover the full pre-limit set (the #114-review contract fix — 0 is not "unlimited").
#[test]
fn check_library_usage_limit_is_a_hard_cap() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    h.add_edge(caller, "fetch", 14, 19, "NameOnly", None);
    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    let bytes = scip_external_call(moniker, vec![external(moniker, "get", vec![], "get(): X")]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let zero = check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions {
        limit: 0,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(zero.entries.len(), 0, "limit 0 returns no entries");
    assert_eq!(zero.total_external_call_sites, 1, "summary counts still cover the full set");
    assert_eq!(zero.distinct_monikers, 1);
}

/// gc (#114): `prune_external_symbols_outside_scope` drops a dead checkout's contracts (both
/// columns dead), keeps a live-worktree-overlay contract (the OR rule, matching `oracle_runs`), and
/// is a no-op on empty live sets so a missing live set never wipes everything.
#[test]
fn prune_external_symbols_drops_dead_checkouts_only() {
    let h = Harness::new();
    let row = |moniker: &'static str| store::ExternalSymbolRow {
        moniker,
        kind: "Function",
        display_name: "f",
        signature_text: "f()",
        signature_language: "rust",
        documentation: "",
        deprecated: false,
    };
    store::write_external_symbol(
        &h.conn,
        TOOL,
        "v1",
        "live-commit",
        "live-wt",
        &row("pkg a 1 m/l()."),
    )
    .unwrap();
    store::write_external_symbol(
        &h.conn,
        TOOL,
        "v1",
        "dead-commit",
        "dead-wt",
        &row("pkg a 1 m/d()."),
    )
    .unwrap();
    // Dead commit but LIVE worktree overlay → survives (OR rule).
    store::write_external_symbol(
        &h.conn,
        TOOL,
        "v1",
        "dead-commit",
        "live-wt",
        &row("pkg a 1 m/o()."),
    )
    .unwrap();
    assert_eq!(count_external_symbols(&h.conn), 3);

    // Empty live sets are a no-op (never wipe everything).
    assert_eq!(store::prune_external_symbols_outside_scope(&h.conn, &[], &[]).unwrap(), 0);
    assert_eq!(count_external_symbols(&h.conn), 3);

    let deleted =
        store::prune_external_symbols_outside_scope(&h.conn, &["live-commit".to_string()], &[
            "live-wt".to_string(),
        ])
        .unwrap();
    assert_eq!(deleted, 1, "only the (dead-commit, dead-wt) contract is pruned");
    assert_eq!(count_external_symbols(&h.conn), 2);
}

/// A non-call external edge (`references_type`) must NOT count as a call site (#114 review): its
/// verdict also has a NULL resolved id, so the read filters on `edge_kind = 'calls_name'`.
#[test]
fn check_library_usage_ignores_non_call_external_edges() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    // A TYPE reference (not a call) to an external symbol.
    h.add_edge_with_kind(caller, "fetch", 14, 19, "references_type", "NameOnly", None);
    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    let bytes = scip_external_call(moniker, vec![external(moniker, "get", vec![], "get(): X")]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.resolved_external, 1, "the oracle still resolves the type ref external");

    let out =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(out.total_external_call_sites, 0, "but a references_type edge is not a call site");
    assert!(out.entries.is_empty());
}

/// A CONSTRUCTOR invocation of an external symbol (`edge_kind = 'constructs'`) IS library usage and
/// must be counted (#114 review): the read includes the full invocation set (`calls_name`,
/// `constructs`, `dispatches`), not just `calls_name`.
#[test]
fn check_library_usage_counts_constructor_calls() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    // A constructor call to an external symbol (e.g. `new Ky(...)`).
    h.add_edge_with_kind(caller, "fetch", 14, 19, "constructs", "NameOnly", None);
    let moniker = "scip-typescript npm ky 1.7.2 index.ts/Ky#constructor().";
    let bytes = scip_external_call(moniker, vec![external(moniker, "Ky", vec![], "constructor()")]);
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    let out =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(out.total_external_call_sites, 1, "a constructor call counts as library usage");
    assert_eq!(out.entries.len(), 1);
    assert_eq!(out.entries[0].moniker, moniker);
}

/// One source call that emits MULTIPLE invocation edge kinds at the SAME callee span (e.g. a Kotlin
/// constructor → both `calls_name` and `constructs`) counts as ONE call site, not two (#114
/// review).
#[test]
fn check_library_usage_deduplicates_multi_kind_edges_at_one_call_site() {
    let h = Harness::new();
    let caller = h.add_file("caller.rs", "fn caller() { fetch(); }\n");
    // Both edge kinds at the SAME callee span (14..19) — two `edge_oracle` rows, one physical call.
    h.add_edge_with_kind(caller, "fetch", 14, 19, "calls_name", "NameOnly", None);
    h.add_edge_with_kind(caller, "fetch", 14, 19, "constructs", "NameOnly", None);
    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    let bytes = scip_external_call(moniker, vec![external(moniker, "get", vec![], "get()")]);
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.resolved_external, 2, "both edge kinds get a verdict");

    let out =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(
        out.total_external_call_sites, 1,
        "the two edge kinds at one span count as ONE call"
    );
    assert_eq!(out.entries.len(), 1);
    assert_eq!(out.entries[0].call_count, 1);
    assert_eq!(out.entries[0].call_sites.len(), 1);
}

/// A `path` filter with a trailing slash (`src/`) matches files under that directory (#114 review):
/// trailing separators are trimmed before the boundary predicate is built.
#[test]
fn check_library_usage_path_filter_tolerates_trailing_slash() {
    use ::protobuf::{EnumOrUnknown, Message};
    use ::scip::types::{Document, Index};
    let h = Harness::new();
    std::fs::create_dir_all(h.root().join("src")).unwrap();
    let caller = h.add_file("src/caller.rs", "fn caller() { fetch(); }\n");
    h.add_edge(caller, "fetch", 14, 19, "NameOnly", None);
    let moniker = "scip-typescript npm ky 1.7.2 index.ts/get().";
    let bytes = Index {
        documents: vec![Document {
            relative_path: "src/caller.rs".to_string(),
            occurrences: vec![occurrence(
                0,
                14,
                19,
                moniker,
                SymbolRole::UnspecifiedSymbolRole as i32,
            )],
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        external_symbols: vec![external(moniker, "get", vec![], "get(): X")],
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();

    for filter in ["src", "src/"] {
        let out = check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions {
            path: Some(filter.to_string()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(out.total_external_call_sites, 1, "path {filter:?} matches src/caller.rs");
    }
    let none = check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions {
        path: Some("other".to_string()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(none.total_external_call_sites, 0, "a non-matching directory yields nothing");
}

/// Per-entry call sites are capped (#114 review): a symbol called far more than the cap reports the
/// TRUE `call_count` but a bounded `call_sites` list, so the response can't balloon.
#[test]
fn check_library_usage_caps_call_sites_per_entry() {
    use ::protobuf::{EnumOrUnknown, Message};
    use ::scip::types::{Document, Index};
    let h = Harness::new();
    let n = 30usize;
    // "f();f();…" — each `f` token is 4 bytes apart (f ( ) ;).
    let src = format!("{}\n", "f();".repeat(n));
    let caller = h.add_file("caller.rs", &src);
    let moniker = "scip-typescript npm ky 1.7.2 index.ts/f().";
    let mut occs = Vec::new();
    for i in 0..n {
        let start = (i * 4) as i32;
        h.add_edge(caller, "f", start as usize, start as usize + 1, "NameOnly", None);
        occs.push(occurrence(
            0,
            start,
            start + 1,
            moniker,
            SymbolRole::UnspecifiedSymbolRole as i32,
        ));
    }
    let bytes = Index {
        documents: vec![Document {
            relative_path: "caller.rs".to_string(),
            occurrences: occs,
            position_encoding: EnumOrUnknown::new(
                PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            ),
            ..Default::default()
        }],
        external_symbols: vec![external(moniker, "f", vec![], "f()")],
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    let report =
        run_oracle(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, &bytes, h.root(), None, None).unwrap();
    assert_eq!(report.resolved_external, n as u64, "all N calls resolved external");

    let out =
        check_library_usage(&h.conn, COMMIT, WORKTREE, &LibraryUsageOptions::default()).unwrap();
    assert_eq!(out.entries.len(), 1);
    assert_eq!(out.entries[0].call_count, n, "call_count is the TRUE total");
    assert_eq!(
        out.entries[0].call_sites.len(),
        25,
        "call_sites capped at MAX_CALL_SITES_PER_ENTRY"
    );
    assert_eq!(out.total_external_call_sites, n);
}
