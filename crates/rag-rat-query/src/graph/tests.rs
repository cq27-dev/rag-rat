//! Reverse-traversal (`find_callers`) recall and completeness-honesty coverage.
//!
//! The fixtures here all reproduce the same real shape: a method call the resolver could not bind,
//! whose recorded target name is RECEIVER-qualified (`h::target`) and therefore matches no
//! definition's file-path qualified name. That is the population `summary.unresolved` has to
//! confess to (#1198), the population a compiler verdict can recover (#1197), and the population
//! `resolution: "fuzzy"` is supposed to reach by short name (#1199).

use rag_rat_core::index::install_scope_view;
use rag_rat_db::schema;
use rusqlite::{Connection, params};

use super::*;

const COMMIT: &str = "c0ffee";
const TOOL: &str = "rust-analyzer";
const TOOL_VERSION: &str = "ra 1.0";

fn scoped_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &rag_rat_core::index::migration_hooks()).unwrap();
    conn
}

/// A committed file row (`(commit_sha, '')` — the clean-checkout scope).
fn add_file(conn: &Connection, path: &str, sha: &str) -> i64 {
    add_scoped_file(conn, path, sha, COMMIT, "")
}

/// A file row in an explicit scope. Production rows carry EITHER `(commit_sha, '')` OR
/// `('', worktree_id)`, never both; the scope view lets the worktree row SHADOW the committed row
/// of the same path, which is how a re-indexed definition leaves its pre-edit `symbols` rows
/// present-but-invisible.
fn add_scoped_file(
    conn: &Connection,
    path: &str,
    sha: &str,
    commit_sha: &str,
    worktree_id: &str,
) -> i64 {
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                           commit_sha, worktree_id)
         VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, ?4)",
        params![path, sha, commit_sha, worktree_id],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn add_symbol(conn: &Connection, file_id: i64, name: &str, qualified: &str) -> i64 {
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                             end_byte, signature, docs)
         VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?3),
                 'function', 0, 10, NULL, NULL)",
        params![file_id, name, qualified],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// One logical symbol over `members` — the grouping that makes a `#[cfg]`-split pair, or an
/// overload set, ONE callable with several `symbols` rows.
fn add_logical_symbol(conn: &Connection, qualified: &str, members: &[i64]) -> i64 {
    conn.execute(
        "INSERT INTO logical_symbols(language, path, logical_name, qualified_name_id, kind,
                                     variant_count, group_reason)
         VALUES ('rust', 'a.rs', ?1, (SELECT id FROM name_strings WHERE value = ?2),
                 'function', ?3, 'test')",
        params![short_name(qualified), qualified, i64::try_from(members.len()).unwrap()],
    )
    .unwrap();
    let logical_symbol_id = conn.last_insert_rowid();
    for symbol_id in members {
        conn.execute(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, \
             end_line)
             VALUES (?1, ?2, 1, 2)",
            params![logical_symbol_id, symbol_id],
        )
        .unwrap();
    }
    logical_symbol_id
}

/// A `calls_name` edge with a distinct call-site byte span (the oracle content key). `to_symbol_id`
/// NULL is the unresolved case; `target_qualified_name` is what the extractor WROTE at the call
/// site, which for a method call is receiver-qualified.
fn add_call(
    conn: &Connection,
    file_id: i64,
    from: i64,
    to: Option<i64>,
    to_name: &str,
    target_qualified_name: &str,
    span: i64,
) -> i64 {
    conn.execute(
        "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                           target_qualified_name, edge_kind, confidence,
                           source_start_byte, source_end_byte,
                           callee_start_byte, callee_end_byte)
         VALUES (?1, ?2, ?3, ?4, ?5, 'calls_name', ?6, ?7, ?8, ?7, ?8)",
        params![
            file_id,
            from,
            to,
            to_name,
            target_qualified_name,
            if to.is_some() { "Exact" } else { "NameOnly" },
            span,
            span + 5,
        ],
    )
    .unwrap();
    conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
}

/// One completed oracle run in a checkout. Insert order is significant: the latest-run gate keys on
/// `MAX(oracle_runs.id)`, so a superseded run must be inserted BEFORE the run that supersedes it.
fn add_oracle_run(conn: &Connection, tool_version: &str, worktree_id: &str) {
    conn.execute(
        "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, status)
         VALUES (?1, ?2, ?3, ?4, 0, 'ok')",
        params![TOOL, tool_version, COMMIT, worktree_id],
    )
    .unwrap();
}

/// An `upgrade` verdict binding the edge at `span` in `path`/`sha` to `resolved_symbol_id`.
fn add_upgrade_verdict(
    conn: &Connection,
    path: &str,
    sha: &str,
    span: i64,
    resolved_symbol_id: i64,
    tool_version: &str,
) {
    conn.execute(
        "INSERT INTO edge_oracle(source_path, source_start_byte, source_end_byte,
                                 callee_start_byte, callee_end_byte, edge_kind, file_sha,
                                 tool, tool_version, resolved_symbol_id, scip_symbol, kind,
                                 computed_at)
         VALUES (?1, ?2, ?3, ?2, ?3, 'calls_name', ?4, ?5, ?6, ?7, 'scip x', 'upgrade', 0)",
        params![path, span, span + 5, sha, TOOL, tool_version, resolved_symbol_id],
    )
    .unwrap();
}

fn syntactic(symbol_id: i64) -> GraphTraversalOptions {
    GraphTraversalOptions { symbol_id: Some(symbol_id), ..GraphTraversalOptions::default() }
}

fn callers(conn: &Connection, symbol: &str, options: &GraphTraversalOptions) -> Vec<GraphHop> {
    traverse_with_options(conn, symbol, true, 100, options).unwrap()
}

/// One resolved caller plus three the resolver left unbound: the summary must confess to the three.
///
/// Before the null-safe negation, `hidden_unresolved_candidate_count` negated a predicate that is
/// NULL (never false) on unresolved rows, so it counted 0 and `find_callers` answered
/// `unresolved: 0, completeness_risk: "low"` — an affirmative claim of completeness on a symbol
/// with three missed call sites.
#[test]
fn summary_counts_unresolved_call_sites_the_seed_could_not_reach() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let target = add_symbol(&conn, file, "target", "a.rs::target");
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    add_call(&conn, file, caller, Some(target), "target", "a.rs::target", 10);
    for i in 0..3 {
        add_call(&conn, file, caller, None, "target", "h::target", 100 + i * 10);
    }
    install_scope_view(&conn, COMMIT, "").unwrap();

    let options = syntactic(target);
    let hops = callers(&conn, "a.rs::target", &options);
    let summary =
        traversal_summary(&conn, "a.rs::target", true, 100, &options, hops.len()).unwrap();

    assert_eq!(summary.unresolved, 3, "three receiver-qualified call sites are unreachable");
    assert_eq!(summary.total_matching_edges, 4);
    assert_ne!(summary.completeness_risk, "low", "a missed majority cannot read as low risk");
}

/// The mirror-image dishonesty: an unresolved call site is a candidate of THIS symbol by short name
/// only while that short name is unique in the checkout.
///
/// `target` here names two different definitions, so a bare `.target()` the resolver could not bind
/// says nothing about which one it meant. Counting those as this symbol's hidden candidates pins
/// `completeness_risk` at `high` and reports `truncated: true` for every symbol whose short name is
/// common — `get`, `new`, `execute` run to thousands of rows apiece in a real index — on a
/// traversal that returned every row it admitted.
#[test]
fn an_ambiguous_short_name_does_not_absorb_unrelated_unresolved_calls() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let other_file = add_file(&conn, "b.rs", "sha-b");
    let target = add_symbol(&conn, file, "target", "a.rs::target");
    // A second, unrelated definition sharing the short name: `?7` is now false.
    add_symbol(&conn, other_file, "target", "b.rs::target");
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    add_call(&conn, file, caller, Some(target), "target", "a.rs::target", 10);
    for i in 0..3 {
        add_call(&conn, file, caller, None, "target", "other::target", 100 + i * 10);
    }
    install_scope_view(&conn, COMMIT, "").unwrap();

    let options = syntactic(target);
    let hops = callers(&conn, "a.rs::target", &options);
    let summary =
        traversal_summary(&conn, "a.rs::target", true, 100, &options, hops.len()).unwrap();

    assert_eq!(hops.len(), 1, "the one bound call site is still the only caller");
    assert_eq!(summary.unresolved, 0, "an ambiguous short name claims no unresolved candidate");
    assert_eq!(summary.total_matching_edges, 1);
    assert!(!summary.truncated, "nothing was withheld, so nothing may claim to be");
}

/// The seed's OWN definitions are not rivals for its short name: a `#[cfg]`-split pair (and every
/// overload group) puts two `symbols` rows under one name while naming one callable.
///
/// Gating the candidate population on the name being unique in the CHECKOUT suppressed the count
/// for exactly those symbols — `find_callers` answered `unresolved: 0, completeness_risk: "low"` on
/// a symbol whose receiver-qualified call sites it had all missed, with no genuine ambiguity to
/// excuse it. The gate asks about symbols OUTSIDE the seed instead.
#[test]
fn a_cfg_split_seed_still_counts_the_call_sites_it_could_not_reach() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let native = add_symbol(&conn, file, "target", "a.rs::target");
    let wasm = add_symbol(&conn, file, "target", "a.rs::target");
    let logical = add_logical_symbol(&conn, "a.rs::target", &[native, wasm]);
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    for i in 0..3 {
        add_call(&conn, file, caller, None, "target", "h::target", 100 + i * 10);
    }
    install_scope_view(&conn, COMMIT, "").unwrap();

    let options = GraphTraversalOptions {
        symbol_id: Some(native),
        logical_symbol_id: Some(logical),
        ..GraphTraversalOptions::default()
    };
    let hops = callers(&conn, "a.rs::target", &options);
    let summary =
        traversal_summary(&conn, "a.rs::target", true, 100, &options, hops.len()).unwrap();

    assert!(hops.is_empty(), "no heuristic arm reaches a receiver-qualified name");
    assert_eq!(summary.unresolved, 3, "the cfg twin is the seed, not a rival for its name");
    assert_ne!(summary.completeness_risk, "low", "three missed call sites are not low risk");
}

/// The recall repro: 1 of 4 call sites resolves heuristically, the compiler names all four.
///
/// Enrichment runs after the fetch and only relabels rows already returned, so without the
/// oracle-seeded arm the three upgraded edges are never fetched and `find_callers` answers 1.
#[test]
fn oracle_verdicts_seed_the_call_sites_the_resolver_left_unbound() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let target = add_symbol(&conn, file, "target", "a.rs::target");
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    add_call(&conn, file, caller, Some(target), "target", "a.rs::target", 10);
    let mut spans = Vec::new();
    for i in 0..3 {
        let span = 100 + i * 10;
        add_call(&conn, file, caller, None, "target", "h::target", span);
        spans.push(span);
    }
    add_oracle_run(&conn, TOOL_VERSION, "");
    for span in &spans {
        add_upgrade_verdict(&conn, "a.rs", "sha-a", *span, target, TOOL_VERSION);
    }
    install_scope_view(&conn, COMMIT, "").unwrap();

    let options = syntactic(target);
    assert_eq!(callers(&conn, "a.rs::target", &options).len(), 4, "all four call sites come back");

    // …and the summary stops calling them hidden, so they are counted once, not twice.
    let summary = traversal_summary(&conn, "a.rs::target", true, 100, &options, 4).unwrap();
    assert_eq!(summary.total_matching_edges, 4);
    // The compiler resolved the three; only tree-sitter left them unbound. Counting them as
    // unresolved reports a fully recalled, compiler-verified answer as the least complete one.
    assert_eq!(summary.compiler_verified, 3);
    assert_eq!(summary.unresolved, 0, "a compiler-bound call site is not an unresolved one");
    assert_eq!(summary.name_only, 0, "nor a name guess");
    assert_ne!(summary.completeness_risk, "high", "every call site came back");
    assert_eq!(summary.false_positive_risk, "low");
}

/// The symbol-selected `impact_surface` report traverses through `traverse_with_options`, so it
/// answers with the same seeded callers `find_callers` does. Its flat `Vec<ImpactItem>` sibling
/// has its own reverse SQL and does not (#1214).
#[test]
fn the_symbol_selected_impact_report_carries_an_oracle_seeded_caller() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let target = add_symbol(&conn, file, "target", "a.rs::target");
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    add_call(&conn, file, caller, None, "target", "h::target", 100);
    add_oracle_run(&conn, TOOL_VERSION, "");
    add_upgrade_verdict(&conn, "a.rs", "sha-a", 100, target, TOOL_VERSION);
    install_scope_view(&conn, COMMIT, "").unwrap();

    let hit = crate::symbol::lookup_by_id(&conn, target).unwrap().unwrap();
    // Graph lanes only: the evidence sections are irrelevant here and several need a populated FTS.
    let options = crate::impact::ImpactSurfaceOptions {
        include_tests: false,
        include_docs: false,
        include_git: false,
        include_papertrail: false,
        include_text_fallback: false,
        include_memories: false,
        ..Default::default()
    };
    // No promotion reported: the hop is in the report because the traversal SEEDED it, not because
    // enrichment relabelled one it already had.
    let report =
        crate::impact::impact_surface_report_for_symbol(&conn, &hit, 100, &options, |_| Ok(false))
            .unwrap();

    assert_eq!(report.direct_semantic_callers.len(), 1);
    assert_eq!(
        report.direct_semantic_callers[0].target_qualified_name.as_deref(),
        Some("h::target")
    );
}

/// `match_tier` is the traversal's primary sort key, so it decides what survives the caller's
/// `LIMIT`. A compiler-attributed caller must outrank a bare name guess — scored in the `ELSE`
/// bucket it would rank below one, and the very rows the seeding recovers would be the first
/// truncated away.
#[test]
fn an_oracle_seeded_caller_outranks_a_name_guess_under_a_tight_limit() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let target = add_symbol(&conn, file, "target", "a.rs::target");
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    add_call(&conn, file, caller, None, "target", "h::target", 100);
    // A call site the qualified-name arm admits on its written name alone — the tier-1 guess.
    add_call(&conn, file, caller, None, "target", "a.rs::target", 200);
    add_oracle_run(&conn, TOOL_VERSION, "");
    add_upgrade_verdict(&conn, "a.rs", "sha-a", 100, target, TOOL_VERSION);
    install_scope_view(&conn, COMMIT, "").unwrap();

    let hops = traverse_with_options(&conn, "a.rs::target", true, 1, &syntactic(target)).unwrap();
    assert_eq!(hops.len(), 1);
    assert_eq!(hops[0].target_qualified_name.as_deref(), Some("h::target"));
}

/// A verdict from a superseded `tool_version` is not evidence: the display path keys on the latest
/// run, so seeding on a stale one would surface a caller the enrichment then refuses to stand
/// behind.
///
/// The fixture keeps BOTH runs, which is the shape supersession actually takes — `oracle_runs` is
/// append-only and the old `ra 0.9` row stays beside the new one. With only the newer run present
/// the plain `oracle_runs.tool_version = edge_oracle.tool_version` join already rejects the stale
/// verdict and the `MAX(id)` gate is untested; with both, the stale verdict has a run of its own to
/// join and only the `MAX(id)` gate keeps it out.
#[test]
fn a_superseded_oracle_run_does_not_seed_callers() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let target = add_symbol(&conn, file, "target", "a.rs::target");
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    add_call(&conn, file, caller, None, "target", "h::target", 100);
    add_oracle_run(&conn, "ra 0.9", "");
    add_oracle_run(&conn, TOOL_VERSION, "");
    add_upgrade_verdict(&conn, "a.rs", "sha-a", 100, target, "ra 0.9");
    install_scope_view(&conn, COMMIT, "").unwrap();

    assert!(callers(&conn, "a.rs::target", &syntactic(target)).is_empty());
}

/// A verdict resolving to a SHADOWED definition is not evidence either: the display path's
/// def-drift gate requires `resolved_symbol_id` to still be live in the active checkout, so seeding
/// on a stale one produces a hop enrichment then declines to promote — a caller with no visible
/// evidence, and one that outranks and displaces genuine matches under the caller's `LIMIT`.
///
/// The fixture is the ordinary agent loop: edit the file that DEFINES the seed (`def.rs` gets a
/// dirty worktree row shadowing its committed row), leave the CALLER file alone (so the callsite
/// sha gate still passes), and run `find_callers` on the seed. The pre-edit `symbols` row survives
/// in the raw table carrying the same qualified name, and the verdict still points at it.
#[test]
fn a_verdict_resolving_to_a_shadowed_definition_does_not_seed_callers() {
    const WORKTREE: &str = "wt-1";
    let conn = scoped_conn();
    let caller_file = add_file(&conn, "a.rs", "sha-a");
    let caller = add_symbol(&conn, caller_file, "caller", "a.rs::caller");
    let shadowed_file = add_scoped_file(&conn, "def.rs", "sha-old", COMMIT, "");
    let shadowed = add_symbol(&conn, shadowed_file, "target", "def.rs::target");
    let live_file = add_scoped_file(&conn, "def.rs", "sha-new", "", WORKTREE);
    let live = add_symbol(&conn, live_file, "target", "def.rs::target");
    add_call(&conn, caller_file, caller, None, "target", "h::target", 100);
    add_oracle_run(&conn, TOOL_VERSION, WORKTREE);
    add_upgrade_verdict(&conn, "a.rs", "sha-a", 100, shadowed, TOOL_VERSION);
    install_scope_view(&conn, COMMIT, WORKTREE).unwrap();

    assert!(
        callers(&conn, "def.rs::target", &syntactic(live)).is_empty(),
        "a verdict bound to the pre-edit definition must not seed a caller"
    );
}

/// Two symbols share the qualified name `a.rs::target`; the compiler bound the call site to the
/// FIRST. The oracle-seeded hop must land only under the symbol the verdict names — the sibling
/// must not steal it the way it can steal a name-attributed unresolved edge.
#[test]
fn an_oracle_seeded_hop_lands_on_the_symbol_the_verdict_names() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let named = add_symbol(&conn, file, "target", "a.rs::target");
    let sibling = add_symbol(&conn, file, "target", "a.rs::target");
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    // Receiver-qualified target name: no heuristic arm reaches it, for either symbol.
    add_call(&conn, file, caller, None, "target", "h::target", 100);
    add_oracle_run(&conn, TOOL_VERSION, "");
    add_upgrade_verdict(&conn, "a.rs", "sha-a", 100, named, TOOL_VERSION);

    // Overloads are attributed by LOGICAL membership, so give each symbol its own logical group.
    let logical =
        [named, sibling].map(|symbol_id| add_logical_symbol(&conn, "a.rs::target", &[symbol_id]));
    install_scope_view(&conn, COMMIT, "").unwrap();

    let for_logical = |logical_symbol_id: i64, symbol_id: i64| GraphTraversalOptions {
        symbol_id: Some(symbol_id),
        logical_symbol_id: Some(logical_symbol_id),
        ..GraphTraversalOptions::default()
    };
    assert_eq!(callers(&conn, "a.rs::target", &for_logical(logical[0], named)).len(), 1);
    assert!(
        callers(&conn, "a.rs::target", &for_logical(logical[1], sibling)).is_empty(),
        "the same-qualified-name sibling must not inherit the compiler's hop"
    );
}

/// `fuzzy` asked for the loosest match and got the syntactic answer: the by-short-name arm was
/// gated on the seed NOT being path-qualified, and every tool-selected seed is path-qualified.
/// Syntactic must not grow in the same fixture — fuzzy is opt-in.
#[test]
fn fuzzy_reaches_a_short_name_caller_that_syntactic_cannot() {
    let conn = scoped_conn();
    let file = add_file(&conn, "a.rs", "sha-a");
    let target = add_symbol(&conn, file, "target", "a.rs::target");
    let caller = add_symbol(&conn, file, "caller", "a.rs::caller");
    add_call(&conn, file, caller, Some(target), "target", "a.rs::target", 10);
    // Unbound, and its written target name belongs to no arm but the short-name one.
    add_call(&conn, file, caller, None, "target", "other::target", 100);
    install_scope_view(&conn, COMMIT, "").unwrap();

    let options = syntactic(target);
    assert_eq!(callers(&conn, "a.rs::target", &options).len(), 1, "syntactic must not grow");

    let fuzzy =
        GraphTraversalOptions { resolution_mode: GraphResolutionMode::Fuzzy, ..syntactic(target) };
    let hops = callers(&conn, "a.rs::target", &fuzzy);
    assert_eq!(hops.len(), 2, "fuzzy reaches the short-name call site");
    let name_only = hops.iter().find(|hop| !hop.verified_target_symbol).unwrap();
    assert_eq!(
        name_only.resolution, "target_name_fallback",
        "a name-only caller must be labelled as one"
    );
    assert_eq!(name_only.confidence, "name_only");
}
