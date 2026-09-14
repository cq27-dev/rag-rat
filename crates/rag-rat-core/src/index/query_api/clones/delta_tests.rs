#[test]
fn clone_delta_status_tokens_are_stable() {
    for (status, token) in [
        (CloneDeltaStatus::Applied, "Applied"),
        (CloneDeltaStatus::Noop, "Noop"),
        (CloneDeltaStatus::NotEligible, "NotEligible"),
        (CloneDeltaStatus::Escalate, "Escalate"),
    ] {
        assert_eq!(status.as_db_str(), token);
        assert_eq!(CloneDeltaStatus::from_db_str(token), Some(status));
        assert_eq!(serde_json::to_value(status).unwrap(), token);
    }
    assert_eq!(CloneDeltaStatus::from_db_str("future-token"), None);
}

use super::super::precompute::tests::{clone_fixture_config, edge_keys};
use super::{
    BTreeSet, CLONE_PRECOMPUTE_THETA, CloneDeltaHint, CloneDeltaStatus,
    load_scoped_baseline_bags_for_paths, params, sub_block_tokens,
};
use crate::index::query_api::clones::precompute::CloneEdgeOptions;

/// One incremental index over the fixture root (the watcher's discover path — works without
/// git), returning a fresh handle.
fn reindex(config: &rag_rat_base::config::Config) -> crate::IndexDatabase {
    let (db, _changed) = crate::IndexDatabase::index_discover_reporting(config).unwrap();
    db
}

fn force_rebuild_edges(db: &crate::IndexDatabase) -> Vec<(String, i64, String, i64)> {
    let report = db
        .reconcile_clone_edges_pass(&CloneEdgeOptions { force: true, ..Default::default() })
        .unwrap();
    assert_eq!(
        report.status,
        crate::index::CloneEdgeStatus::Complete,
        "forced rebuild runs to completion"
    );
    edge_keys(db)
}

/// THE differential pin: after every delta, the maintained edge set equals what a from-scratch
/// generation build produces at the same content (same frozen df epoch). Exercises add, edit,
/// delete, and COMPOUND deltas (two deltas with no intermediate rebuild).
#[test]
fn clone_graph_delta_matches_a_full_rebuild_over_an_edit_sequence() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-differential");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    drop(db);

    let steps: &[(&str, Option<&str>)] = &[
        // Add a near-clone pair member + a unique function.
        (
            "src/c.rs",
            Some(
                "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 \
                 }\npub fn distinct_worker(n: u64) -> u64 { n.rotate_left(3) ^ 0x0defaced }\n",
            ),
        ),
        // Edit an existing file: append another member of the tally family.
        (
            "src/a.rs",
            Some(
                "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub \
                 fn compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s \
                 += it * 2; } s + 1 }\npub fn sum_figures(rows: Vec<i64>) -> i64 { let mut f = 0; \
                 for r in rows { f += r * 2; } f + 1 }\n",
            ),
        ),
        // Delete a file that carries clone-family members.
        ("src/b.rs", None),
    ];
    for (path, content) in steps {
        let target = config.root.join(path);
        match content {
            Some(text) => std::fs::write(&target, text).unwrap(),
            None => std::fs::remove_file(&target).unwrap(),
        }
        let db = reindex(&config);
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(
            report.status,
            CloneDeltaStatus::Applied,
            "delta applies for {path}: {report:?}"
        );
        let delta_edges = edge_keys(&db);
        let rebuilt_edges = force_rebuild_edges(&db);
        assert_eq!(
            delta_edges, rebuilt_edges,
            "delta-maintained edges equal a from-scratch rebuild after touching {path}"
        );
    }

    // COMPOUND: two deltas back-to-back with no intermediate rebuild, compared once at the
    // end — pins that parity is maintained inductively, not just from a fresh baseline.
    std::fs::write(
        config.root.join("src/d.rs"),
        "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\n",
    )
    .unwrap();
    let db = reindex(&config);
    assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, CloneDeltaStatus::Applied);
    drop(db);
    std::fs::write(
        config.root.join("src/c.rs"),
        "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\npub fn \
         distinct_worker(n: u64) -> u64 { n.rotate_left(4) ^ 0x0badf00d }\n",
    )
    .unwrap();
    let db = reindex(&config);
    assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, CloneDeltaStatus::Applied);
    let delta_edges = edge_keys(&db);
    let rebuilt_edges = force_rebuild_edges(&db);
    assert_eq!(delta_edges, rebuilt_edges, "compound deltas stay parity-equal");
    drop(db);

    // INTERLEAVED LIVE-INDEX STEP (#479): every reindex above already bumps the LIVE df; here
    // an adversarial whole-table inversion (the most live drift could ever diverge from the
    // pinned epoch) precedes one more edit + delta. Parity with a from-scratch rebuild must
    // still hold — the delta orders by the generation's epoch, not the live table. (The
    // byte-level discriminator lives in
    // `delta_postings_are_ordered_by_the_epoch_not_the_live_df`; this step pins the
    // end-to-end soundness claim on the edge set.)
    {
        let db = crate::IndexDatabase::open_config(&config).unwrap();
        db.storage.connection().execute("UPDATE clone_token_df SET df = 1000000 - df", []).unwrap();
    }
    std::fs::write(
        config.root.join("src/e.rs"),
        "pub fn load_shipment(db: Db) -> i32 { let s = db.get(50); validate(s); s + 1 }\n",
    )
    .unwrap();
    let db = reindex(&config);
    assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, CloneDeltaStatus::Applied);
    let delta_edges = edge_keys(&db);
    let rebuilt_edges = force_rebuild_edges(&db);
    assert_eq!(
        delta_edges, rebuilt_edges,
        "parity holds through an adversarial live-table inversion between deltas"
    );
}

/// The byte-level pin for the #479 df split: the delta's persisted postings are ordered by
/// the generation's PINNED epoch, not the live `clone_token_df`. The live table is inverted
/// before the delta, so the two orders provably select DIFFERENT sub-block prefixes for the
/// touched file — and the postings must match the EPOCH's selection.
#[test]
fn delta_postings_are_ordered_by_the_epoch_not_the_live_df() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-epoch-postings");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    let built = db.precompute_clone_graph(None).unwrap();
    assert_eq!(built.status, crate::index::CloneEdgeStatus::Complete);
    // Adversarial live drift: invert the whole live table.
    db.storage.connection().execute("UPDATE clone_token_df SET df = 1000000 - df", []).unwrap();
    drop(db);

    // A near-clone family member: enough shared (mid-df) and unique (df=1) tokens that the
    // epoch and inverted-live orders pick different prefixes.
    let touched = "src/shipment.rs".to_string();
    std::fs::write(
        config.root.join(&touched),
        "pub fn load_shipment(db: Db) -> i32 { let s = db.get(50); validate(s); s + 1 }\n",
    )
    .unwrap();
    let db = reindex(&config);
    assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, CloneDeltaStatus::Applied);

    let conn = db.storage.connection();
    let paths = vec![touched.clone()];
    let sub_block_union = |df: &std::collections::HashMap<i64, i64>| -> BTreeSet<i64> {
        load_scoped_baseline_bags_for_paths(conn, &paths, df)
            .unwrap()
            .iter()
            .flat_map(|bag| sub_block_tokens(bag, CLONE_PRECOMPUTE_THETA))
            .collect()
    };
    let epoch_df = super::super::substrate::load_clone_df_epoch(conn, built.generation).unwrap();
    let live_df = super::super::substrate::load_current_clone_df(conn).unwrap();
    let under_epoch = sub_block_union(&epoch_df);
    let under_live = sub_block_union(&live_df);
    assert_ne!(
        under_epoch, under_live,
        "precondition: the inversion must actually change the prefix selection — if this fails \
         the fixture has degenerated and the test is vacuous"
    );

    let persisted: BTreeSet<i64> = conn
        .prepare(
            "SELECT DISTINCT token_hash FROM clone_subblock_postings
                 WHERE build_generation = ?1 AND path = ?2",
        )
        .unwrap()
        .query_map(params![built.generation, touched], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        persisted, under_epoch,
        "delta-emitted postings are selected under the generation's pinned epoch"
    );
}

/// A current generation is a cheap no-op — and a SECOND delta right after an applied one must
/// also be a no-op (idempotence at the write level).
#[test]
fn clone_graph_delta_is_noop_when_current() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-noop");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, CloneDeltaStatus::Noop);
    drop(db);

    std::fs::write(
        config.root.join("src/e.rs"),
        "pub fn delta_probe(q: i64) -> i64 { q * 7 + 5 }\n",
    )
    .unwrap();
    let db = reindex(&config);
    let applied = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(applied.status, CloneDeltaStatus::Applied);
    assert_eq!(applied.files_changed, 1, "exactly the touched file: {applied:?}");
    let again = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(again.status, CloneDeltaStatus::Noop, "an applied delta leaves nothing owed");
    assert_eq!(again.edges_added + again.edges_removed, 0);
}

/// Without a live Complete generation there is nothing to patch — the caller must use the
/// full-rebuild path.
#[test]
fn clone_graph_delta_is_not_eligible_without_a_live_generation() {
    let config = clone_fixture_config("delta-no-gen");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(report.status, CloneDeltaStatus::NotEligible, "{report:?}");
}

/// The remaining eligibility gates: a postings-stale live generation (pre-upgrade or
/// df-refresh-invalidated) and an in-flight Building generation both refuse the in-place
/// patch — the full-rebuild path owns those states.
#[test]
fn clone_graph_delta_is_not_eligible_for_stale_postings_or_inflight_builds() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-ineligible");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    let conn = db.storage.connection();

    conn.execute("UPDATE clone_graph_generations SET postings_written = 0", []).unwrap();
    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(
        report.status,
        CloneDeltaStatus::NotEligible,
        "postings-stale generation: {report:?}"
    );
    conn.execute("UPDATE clone_graph_generations SET postings_written = 1", []).unwrap();

    // An in-flight (Building) generation means a full rebuild is owed — patching the live
    // generation now would race its eventual publish.
    conn.execute(
        "INSERT INTO clone_graph_generations
                (generation, status, theta_floor, normalizer_kind, normalizer_version,
                 source_revision, started_at_ms, postings_written, repo_id)
             VALUES (9999, 'Building', 0.7, 'baseline', ?1, 'inflight-rev', 0, 1, ?2)",
        rusqlite::params![rag_rat_clones::NORM_VERSION, db.active_repo_id],
    )
    .unwrap();
    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(report.status, CloneDeltaStatus::NotEligible, "in-flight full rebuild: {report:?}");
}

/// A content-revision move with NO clone-relevant file change (a new file with no
/// fingerprintable functions) re-pins `source_revision` without touching the graph —
/// `Applied` with zero files, zero edge churn.
#[test]
fn clone_graph_delta_repins_freshness_for_clone_irrelevant_changes() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-irrelevant");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    let edges_before = edge_keys(&db);
    drop(db);

    // A type-only file: indexed (revision moves) but no function fingerprints.
    std::fs::write(config.root.join("src/j.rs"), "pub struct MarkerOnly;\n").unwrap();
    let db = reindex(&config);
    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(report.status, CloneDeltaStatus::Applied, "{report:?}");
    assert_eq!(report.files_changed, 0, "no clone-relevant file changed");
    assert_eq!(report.edges_added + report.edges_removed, 0);
    assert_eq!(edge_keys(&db), edges_before, "the graph itself is untouched");
    assert!(!db.pending_clone_graph().unwrap(), "but the freshness key is re-pinned");
}

/// A delta larger than `max_files` escalates without writing anything — a huge delta (branch
/// switch) is cheaper to rebuild than to patch file-by-file.
#[test]
fn clone_graph_delta_escalates_when_too_many_files_changed() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-escalate");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    let edges_before = edge_keys(&db);
    drop(db);

    std::fs::write(
        config.root.join("src/f.rs"),
        "pub fn escalate_probe(q: i64) -> i64 { q * 9 + 2 }\n",
    )
    .unwrap();
    let db = reindex(&config);
    let report = db.apply_clone_graph_delta(0).unwrap();
    assert_eq!(report.status, CloneDeltaStatus::Escalate, "{report:?}");
    assert_eq!(edge_keys(&db), edges_before, "an escalated delta writes nothing");
}

/// The PERSISTED graph does not apply the live path's #271 hot-token cap (the build walks
/// every sub-block token; the cap belongs to `sub_block_candidate_pairs` / the RAM fallback
/// only) — so neither may the delta. A stable-hot shared token (postings above the cap before
/// AND after the delta) must still re-emit the changed file's verified edges, or the delta
/// silently under-populates the graph a full rebuild would keep.
#[test]
fn delta_keeps_hot_token_edges_the_build_would_emit() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-hot-token");
    // A sub-block-only near-clone pair: same token bag family, DIFFERENT structure (the extra
    // trailing statement), so no struct-hash edge can mask a dropped sub-block edge.
    std::fs::write(
        config.root.join("src/a.rs"),
        "pub fn alpha_total(vals: Vec<i64>) -> i64 { let mut s = 0; for v in vals { s += v * 3; } \
         s - 2 }\n",
    )
    .unwrap();
    std::fs::write(
        config.root.join("src/b.rs"),
        "pub fn omega_total(vals: Vec<i64>) -> i64 { let mut s = 0; for v in vals { s += v * 3; } \
         let z = s; z - 2 }\n",
    )
    .unwrap();
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    let conn = db.storage.connection();
    let sub_block_edges: i64 = conn
        .query_row("SELECT COUNT(*) FROM clone_edges WHERE edge_source = 'sub_block'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(sub_block_edges > 0, "the pair must be sub-block-discovered, not struct-exact");

    // Make EVERY shared discovery token stable-hot: inflate its postings past the cap with
    // rows anchored at the UNTOUCHED file's current sha (so they are neither stale nor part
    // of the delta set) at start_bytes that resolve to no symbol (hydration drops them).
    let b_sha: String = conn
        .query_row("SELECT sha256 FROM files WHERE path = 'src/b.rs'", [], |r| r.get(0))
        .unwrap();
    let generation: i64 = conn
        .query_row(
            "SELECT MAX(generation) FROM clone_graph_generations WHERE status = 'Complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let shared_tokens: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT p1.token_hash FROM clone_subblock_postings p1
                      WHERE p1.path = 'src/a.rs'
                        AND EXISTS (SELECT 1 FROM clone_subblock_postings p2
                                     WHERE p2.token_hash = p1.token_hash AND p2.path = 'src/b.rs')",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert!(!shared_tokens.is_empty(), "the near-clone pair shares discovery tokens");
    for token in &shared_tokens {
        for i in 0..(super::super::substrate::HOT_TOKEN_POSTINGS_CAP as i64 + 8) {
            conn.execute(
                "INSERT OR IGNORE INTO clone_subblock_postings
                        (build_generation, token_hash, path, start_byte, file_sha)
                     VALUES (?1, ?2, 'src/b.rs', ?3, ?4)",
                rusqlite::params![generation, token, 1_000_000 + i, b_sha],
            )
            .unwrap();
        }
    }
    drop(db);

    // Edit the OTHER file trivially (append an unrelated fn): its symbols recompute through
    // the delta, and their edges to b.rs must survive despite every shared token being hot.
    let mut text = std::fs::read_to_string(config.root.join("src/a.rs")).unwrap();
    text.push_str("pub fn unrelated_probe(q: i64) -> i64 { q ^ 3 }\n");
    std::fs::write(config.root.join("src/a.rs"), text).unwrap();
    let db = reindex(&config);
    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(report.status, CloneDeltaStatus::Applied, "{report:?}");
    let delta_edges = edge_keys(&db);
    let rebuilt_edges = force_rebuild_edges(&db);
    assert_eq!(
        delta_edges, rebuilt_edges,
        "stable-hot shared tokens must not drop edges the build would emit"
    );
}

/// The watcher tail applies the delta IN PLACE on the same pass that indexed the edit — no
/// quiet window, no generation churn. The #472 gate now guards only the full-rebuild path.
#[test]
fn maintenance_pass_applies_the_delta_in_place() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-tail-inplace");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    let built = db.precompute_clone_graph(None).unwrap();
    assert_eq!(built.status, crate::index::CloneEdgeStatus::Complete);
    drop(db);

    std::fs::write(
        config.root.join("src/h.rs"),
        "pub fn load_receipt(db: Db) -> i32 { let r = db.get(40); validate(r); r + 1 }\n",
    )
    .unwrap();
    crate::watch::maintenance_pass(&config, false).unwrap();

    let db = crate::IndexDatabase::open_config(&config).unwrap();
    assert!(
        !db.pending_clone_graph().unwrap(),
        "the SAME pass that indexed the edit settled the graph via the in-place delta"
    );
    let (generations, live_generation, absorbed): (i64, i64, i64) = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*), MAX(generation), MAX(delta_files_applied)
                   FROM clone_graph_generations WHERE status = 'Complete'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(generations, 1);
    assert_eq!(
        live_generation, built.generation,
        "patched in place — no generation was discarded or rebuilt"
    );
    assert_eq!(absorbed, 1, "the drift counter absorbed the edited file");
}

/// Accumulated delta drift past [`CLONE_GRAPH_DRIFT_REBUILD_FILES`] owes a df-epoch refresh:
/// the graph keeps serving (fresh via deltas), and the full rebuild rides the #472 quiet
/// window — deferred while edits land, executed on the first quiet-elapsed pass (the
/// reconcile pass must NOT skip-as-current a drifted generation).
#[test]
fn drift_past_the_limit_schedules_a_quiet_gated_full_rebuild() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-drift-rebuild");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    let built = db.precompute_clone_graph(None).unwrap();
    assert_eq!(built.status, crate::index::CloneEdgeStatus::Complete);
    db.storage
        .connection()
        .execute("UPDATE clone_graph_generations SET delta_files_applied = ?1", rusqlite::params![
            super::CLONE_GRAPH_DRIFT_REBUILD_FILES
        ])
        .unwrap();
    drop(db);

    // A content pass inside the quiet window: the delta settles freshness in place; the
    // drift-owed FULL rebuild stays deferred. #479: the new file's tokens DO enter the LIVE
    // df immediately (the incremental bump), but the generation's PINNED epoch — what its
    // postings are ordered by — still predates them; that gap is exactly the drift the
    // counter measures.
    std::fs::write(
        config.root.join("src/i.rs"),
        "pub fn drift_probe(q: i64) -> i64 { q * 11 - 6 }\n",
    )
    .unwrap();
    crate::watch::maintenance_pass(&config, false).unwrap();
    let db = crate::IndexDatabase::open_config(&config).unwrap();
    let epoch_count = |db: &crate::IndexDatabase, generation: i64| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM clone_df_epoch WHERE build_generation = ?1",
                [generation],
                |r| r.get(0),
            )
            .unwrap()
    };
    let pinned_epoch = epoch_count(&db, built.generation);
    let live: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT MAX(generation) FROM clone_graph_generations WHERE status = 'Complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, built.generation, "drift rebuild deferred inside the quiet window");

    // Quiet elapses (backdate the armed candidate): the next idle pass runs the full rebuild
    // — a NEW generation with the drift counter reset.
    db.storage
        .connection()
        .execute(
            "UPDATE repo_meta SET value = '1' WHERE key = 'clone_graph_quiet_candidate_since_ms'",
            [],
        )
        .unwrap();
    drop(db);
    crate::watch::maintenance_pass(&config, false).unwrap();
    let db = crate::IndexDatabase::open_config(&config).unwrap();
    let (live, absorbed): (i64, i64) = db
        .storage
        .connection()
        .query_row(
            "SELECT MAX(generation), MAX(delta_files_applied)
                   FROM clone_graph_generations WHERE status = 'Complete'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(live > built.generation, "the quiet-elapsed pass ran the drift full rebuild");
    assert_eq!(absorbed, 0, "the fresh generation starts with zero absorbed deltas");
    // The whole POINT of the drift rebuild (PR #477 review): it must move the PINNED epoch,
    // not just reset the counter — the fresh generation's postings are ordered by a df that
    // includes the delta-added file's tokens (#479: the live table already had them from the
    // incremental bump; the rebuild is what folds them into the served order).
    assert!(
        epoch_count(&db, live) > pinned_epoch,
        "the drift full rebuild re-pins the epoch with the delta-added tokens"
    );
}

/// The generation bookkeeping: an applied delta bumps `source_revision` to current (making
/// the write-time postings fast path eligible again) and counts the absorbed files in
/// `delta_files_applied` (the df-drift signal for scheduling the next full rebuild).
#[test]
fn clone_graph_delta_updates_generation_bookkeeping() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-bookkeeping");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    drop(db);

    std::fs::write(
        config.root.join("src/g.rs"),
        "pub fn bookkeeping_probe(q: i64) -> i64 { q * 3 - 4 }\n",
    )
    .unwrap();
    let db = reindex(&config);
    assert!(
        db.clone_check_indexed_generation().unwrap().is_none(),
        "stale revision → write-time fast path ineligible before the delta"
    );
    assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, CloneDeltaStatus::Applied);
    assert!(
        db.clone_check_indexed_generation().unwrap().is_some(),
        "the applied delta restores exact freshness (source_revision == content_revision)"
    );
    let applied: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT delta_files_applied FROM clone_graph_generations WHERE status = 'Complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(applied, 1, "the drift counter absorbed one file");
    assert!(!db.pending_clone_graph().unwrap(), "the graph reads as current after the delta");
}
/// #598: the file-count Escalate bail can't see posting fan-out — a ≤64-file delta whose bags
/// hit hot tokens hydrates candidate posting lists uncapped (build parity forbids filtering)
/// and was observed pinning a core for 38+ minutes under the write lock. The delta now also
/// escalates when hydration REQUESTS more posting rows than its work budget — same escape as
/// the file-count bail: nothing written, the caller schedules the (budgeted, resumable) full
/// rebuild.
#[test]
fn delta_escalates_when_posting_hydration_exceeds_the_work_budget() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-work-budget");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    drop(db);

    // A near-clone family member: its bag's tokens hit the corpus postings, so hydration
    // requests a non-zero number of posting rows — more than a zero budget allows.
    std::fs::write(
        config.root.join("src/c.rs"),
        "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\n",
    )
    .unwrap();
    let db = reindex(&config);
    let before = edge_keys(&db);

    let report = db.apply_clone_graph_delta_with_budget(64, 0).unwrap();
    assert_eq!(
        report.status,
        CloneDeltaStatus::Escalate,
        "budget exhaustion escalates: {report:?}"
    );
    assert!(
        report.reason.as_deref().is_some_and(|r| r.contains("work budget")),
        "the reason names the work budget: {report:?}"
    );
    assert!(report.posting_rows_requested > 0, "the bail happened because rows were owed");
    assert_eq!(edge_keys(&db), before, "an escalated delta writes nothing");

    // The default budget applies the same delta — the bail is about pathology, not size 1.
    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(report.status, CloneDeltaStatus::Applied, "{report:?}");
}

/// #598: posting lists are hydrated ONCE per token per delta application — bags sharing hot
/// tokens re-use the cached rows instead of re-walking the same b-tree lists (the observed
/// pathology re-walked ~3k-row lists once per bag). `posting_rows_requested` deliberately
/// still counts cache hits (it is the combinatorics proxy the work budget meters), so
/// memoization shows as fetched < requested.
#[test]
fn posting_hydration_memoizes_shared_tokens_across_bags() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-hydration-memo");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    drop(db);

    // TWO near-identical family members in the delta: their sub-block prefixes share tokens,
    // so the second bag's hydration must hit the first's cached posting lists.
    std::fs::write(
        config.root.join("src/c.rs"),
        "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\npub fn \
         load_receipt(db: Db) -> i32 { let r = db.get(40); validate(r); r + 1 }\n",
    )
    .unwrap();
    let db = reindex(&config);

    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(report.status, CloneDeltaStatus::Applied, "{report:?}");
    assert!(report.posting_rows_fetched > 0, "hydration touched the postings: {report:?}");
    assert!(
        report.posting_rows_fetched < report.posting_rows_requested,
        "shared tokens are served from the per-application cache: {report:?}"
    );
}

/// #830: the hinted changed-set derivation must NEVER full-scan the (large) postings table —
/// that scan is exactly what the hint exists to avoid. Pin the query plans: both the STALE
/// point-lookup and the FRESH `NOT EXISTS` reach `clone_subblock_postings` through
/// `idx_clone_subblock_postings_path` (a SEARCH), and neither plan contains a full
/// `SCAN clone_subblock_postings`.
#[test]
fn hinted_changed_set_lookups_never_full_scan_the_postings_table() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-hint-qplan");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    let conn = db.storage.connection();

    let plan = |sql: &str, binds: &[super::Value]| -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map(super::params_from_iter(binds.iter().cloned()), |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let generation = super::Value::Integer(1);
    let path = super::Value::Text("src/a.rs".to_string());
    let norm = super::Value::Integer(rag_rat_clones::NORM_VERSION);

    let stale_plan = plan(
        "SELECT EXISTS(SELECT 1 FROM clone_subblock_postings p WHERE p.build_generation = ?1 AND \
         p.path = ?2 AND NOT EXISTS (SELECT 1 FROM files f WHERE f.path = p.path AND f.sha256 = \
         p.file_sha AND f.generated = 0))",
        &[generation.clone(), path.clone()],
    );
    assert!(
        stale_plan.contains("idx_clone_subblock_postings_path"),
        "STALE lookup must seek the postings path index: {stale_plan}"
    );
    assert!(
        !stale_plan.contains("SCAN clone_subblock_postings"),
        "STALE lookup must not full-scan the postings table: {stale_plan}"
    );

    let fresh_plan = plan(
        "SELECT EXISTS(SELECT 1 FROM files f WHERE f.path = ?2 AND f.generated = 0 AND EXISTS \
         (SELECT 1 FROM symbols s JOIN symbol_fingerprints sf ON sf.symbol_id = s.id WHERE \
         s.file_id = f.id AND sf.normalizer_kind = 'baseline' AND sf.normalizer_version = ?3 AND \
         sf.token_bag IS NOT NULL) AND NOT EXISTS (SELECT 1 FROM clone_subblock_postings p WHERE \
         p.build_generation = ?1 AND p.path = f.path))",
        &[generation, path, norm],
    );
    assert!(
        !fresh_plan.contains("SCAN clone_subblock_postings"),
        "FRESH NOT EXISTS must not full-scan the postings table: {fresh_plan}"
    );
    assert!(
        fresh_plan.contains("idx_clone_subblock_postings_path"),
        "FRESH NOT EXISTS must seek the postings path index: {fresh_plan}"
    );
    // The FRESH check must drive from the `files.path` seek (via the scoped view's composite
    // indexes), NOT scan every baseline fingerprint — otherwise the per-path cost grows with
    // the corpus and the hint stops being a win.
    assert!(
        !fresh_plan.contains("SCAN sf") && !fresh_plan.contains("SCAN symbol_fingerprints"),
        "FRESH check must not scan the fingerprint table: {fresh_plan}"
    );
}

/// #830: the cached `clone_graph_generations.postings_row_count` is maintained transactionally
/// by each delta write-back (net inserted − deleted), so it stays EQUAL to a live `COUNT(*)` of
/// the generation's postings across an edit sequence — that equality is what makes the cheap
/// column read a sound substitute for the per-pass table scan the work budget used to pay.
#[test]
fn delta_maintains_the_cached_postings_row_count() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-postings-count");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    let built = db.precompute_clone_graph(None).unwrap();
    assert_eq!(built.status, crate::index::CloneEdgeStatus::Complete);
    let generation = built.generation;

    // The build's COUNT seed already matches the postings it just wrote.
    let cached_matches_scan = |db: &crate::IndexDatabase| {
        let conn = db.storage.connection();
        let cached: i64 = conn
            .query_row(
                "SELECT postings_row_count FROM clone_graph_generations WHERE generation = ?1",
                [generation],
                |r| r.get(0),
            )
            .unwrap();
        let scanned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clone_subblock_postings WHERE build_generation = ?1",
                [generation],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cached, scanned, "cached postings_row_count must equal COUNT(*)");
    };
    cached_matches_scan(&db);
    drop(db);

    // An edit sequence that both REMOVES postings (edited/deleted files) and ADDS them (a new
    // near-clone member), each applied in place — the net-delta maintenance must track both.
    let steps: &[(&str, Option<&str>)] = &[
        (
            "src/c.rs",
            Some("pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\n"),
        ),
        (
            "src/a.rs",
            Some(
                "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub \
                 fn compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s \
                 += it * 2; } s + 1 }\npub fn sum_figures(rows: Vec<i64>) -> i64 { let mut f = 0; \
                 for r in rows { f += r * 2; } f + 1 }\n",
            ),
        ),
        ("src/b.rs", None),
    ];
    for (path, content) in steps {
        let target = config.root.join(path);
        match content {
            Some(text) => std::fs::write(&target, text).unwrap(),
            None => std::fs::remove_file(&target).unwrap(),
        }
        let db = reindex(&config);
        assert_eq!(
            db.apply_clone_graph_delta(64).unwrap().status,
            CloneDeltaStatus::Applied,
            "delta for {path}"
        );
        cached_matches_scan(&db);
    }
}

/// THE #830 correctness pin: a delta driven by a `Paths` hint produces the IDENTICAL result as
/// one driven by the `FullScan` DB derivation — same `CloneDeltaReport` (files_changed,
/// edges_added/removed, full_rebuild_owed) AND the same resulting edge set. The hint is
/// exercised with a set that COULD disagree: it names an UNCHANGED base file alongside the
/// changed/new ones, so `delta_paths_from_hint` must filter the unchanged one back out to match
/// the scan (a hint that blindly trusted its paths would emit different edges). Run on two
/// identical DBs — the delta mutates, so the two derivations can't share one.
#[test]
fn a_hinted_delta_equals_the_full_scan_delta() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();

    // Two byte-identical fixtures, taken through the SAME rebuild + precompute + edit +
    // reindex, so only the delta's changed-set derivation differs between them.
    let prepare = |tag: &str| {
        let config = clone_fixture_config(tag);
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(
            db.precompute_clone_graph(None).unwrap().status,
            crate::index::CloneEdgeStatus::Complete
        );
        drop(db);
        // Edit an existing clone-family file AND add a new near-clone member; src/b.rs is left
        // untouched (the "could disagree" element the hint names but the scan excludes).
        std::fs::write(
            config.root.join("src/a.rs"),
            "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
             compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s += it * \
             2; } s + 1 }\npub fn sum_figures(rows: Vec<i64>) -> i64 { let mut f = 0; for r in \
             rows { f += r * 2; } f + 1 }\n",
        )
        .unwrap();
        std::fs::write(
            config.root.join("src/c.rs"),
            "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\n",
        )
        .unwrap();
        let db = reindex(&config);
        config.retain_for(db)
    };

    let scan_db = prepare("delta-hint-scan");
    let scan_report = scan_db.apply_clone_graph_delta_hinted(64, CloneDeltaHint::FullScan).unwrap();
    let scan_edges = edge_keys(&scan_db);

    let hint_db = prepare("delta-hint-paths");
    // The hint the reconcile would supply: the reindexed/new paths PLUS an unchanged base file.
    let touched: BTreeSet<String> =
        ["src/a.rs", "src/b.rs", "src/c.rs"].iter().map(|s| s.to_string()).collect();
    let hint_report =
        hint_db.apply_clone_graph_delta_hinted(64, CloneDeltaHint::Paths(&touched)).unwrap();
    let hint_edges = edge_keys(&hint_db);

    assert_eq!(scan_report.status, CloneDeltaStatus::Applied, "scan applied: {scan_report:?}");
    assert_eq!(hint_report.status, CloneDeltaStatus::Applied, "hint applied: {hint_report:?}");
    assert_eq!(
        (hint_report.files_changed, hint_report.edges_added, hint_report.edges_removed),
        (scan_report.files_changed, scan_report.edges_added, scan_report.edges_removed),
        "hinted counts must equal the scan's: hint={hint_report:?} scan={scan_report:?}"
    );
    assert_eq!(
        hint_report.full_rebuild_owed, scan_report.full_rebuild_owed,
        "hinted drift bookkeeping must equal the scan's"
    );
    assert_eq!(
        hint_edges, scan_edges,
        "the hinted delta's resulting edge set must equal the full-scan delta's"
    );
}

/// #830: a `Paths` hint whose every path is clone-IRRELEVANT (a docs / fingerprint-less edit)
/// makes `delta_paths_from_hint` return empty, so the delta takes the same re-pin branch as an
/// empty `delta_paths` — `Applied` with zero files, no edge churn, freshness key re-pinned —
/// and never touches the postings corpus.
#[test]
fn a_clone_irrelevant_hint_repins_without_scanning() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-hint-irrelevant");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );
    let edges_before = edge_keys(&db);
    drop(db);

    // A type-only file: indexed (revision moves) but no function fingerprints →
    // clone-irrelevant.
    std::fs::write(config.root.join("src/j.rs"), "pub struct MarkerOnly;\n").unwrap();
    let db = reindex(&config);
    let touched: BTreeSet<String> = ["src/j.rs"].iter().map(|s| s.to_string()).collect();
    let report = db.apply_clone_graph_delta_hinted(64, CloneDeltaHint::Paths(&touched)).unwrap();
    assert_eq!(report.status, CloneDeltaStatus::Applied, "{report:?}");
    assert_eq!(report.files_changed, 0, "no clone-relevant path in the hint");
    assert_eq!(report.edges_added + report.edges_removed, 0);
    assert_eq!(edge_keys(&db), edges_before, "the graph itself is untouched");
    assert!(!db.pending_clone_graph().unwrap(), "the freshness key is re-pinned");
}

/// #830 SelfHeal: a `generated`-flag flip changes `files.generated`, not `(path, sha256)`, so
/// `content_revision()` does NOT move and a `FullScan`/`Paths` delta returns `Noop` before the
/// changed-set derivation — the flipped file's now-ineligible postings linger. `SelfHeal`
/// bypasses that revision-equality early return, scans, and removes them. This pins the exact
/// gap the gc-cadence self-heal closes: a plain `FullScan` on the SAME drift `Noop`s past it,
/// so the two derivations must DISAGREE here or the self-heal would be dead code.
#[test]
fn self_heal_repairs_generated_flip_drift_a_full_scan_noops_past() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-selfheal-genflip");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );

    let flipped: String = db
        .storage
        .connection()
        .query_row(
            "SELECT DISTINCT path FROM clone_subblock_postings ORDER BY path LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let postings_for = |db: &crate::IndexDatabase| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM clone_subblock_postings WHERE path = ?1",
                [&flipped],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert!(postings_for(&db) > 0, "the flipped file starts with postings");

    // Flip it to generated = 1 directly (the `rederive_generated_flags` mechanism); the #828
    // content-digest trigger keys on path/sha256/kind, so the revision is unmoved.
    let revision_before = db.content_revision().unwrap();
    db.storage
        .connection()
        .execute("UPDATE main.files SET generated = 1 WHERE path = ?1", [&flipped])
        .unwrap();
    assert_eq!(
        db.content_revision().unwrap(),
        revision_before,
        "a generated-flag flip does not move content_revision"
    );

    // A plain FullScan honors the revision-equality fast path: Noop, drift left in place.
    let full = db.apply_clone_graph_delta_hinted(64, CloneDeltaHint::FullScan).unwrap();
    assert_eq!(
        full.status,
        CloneDeltaStatus::Noop,
        "FullScan Noops when the revision is unchanged: {full:?}"
    );
    assert!(postings_for(&db) > 0, "FullScan left the stale postings — the gap SelfHeal closes");

    // SelfHeal scans past the early return and drops the now-ineligible file's postings.
    let heal = db.apply_clone_graph_delta_hinted(64, CloneDeltaHint::SelfHeal).unwrap();
    assert_eq!(
        heal.status,
        CloneDeltaStatus::Applied,
        "SelfHeal applies the drift repair: {heal:?}"
    );
    assert_eq!(postings_for(&db), 0, "SelfHeal removed the generated file's stale postings");
}

/// #830: when `SelfHeal` finds MORE revision-neutral drift than the delta cap it `Escalate`s
/// WHILE `clone_graph_stale_against` still reports the graph fresh (the revision never moved).
/// That is the exact `(Escalate, !stale)` state the watcher keys its forced full rebuild on —
/// the quiet gate keys on revision movement and would otherwise suppress the rebuild forever.
/// Uses a tiny `max_files` so two generated-flipped files exceed the cap.
#[test]
fn self_heal_escalates_while_fresh_when_revision_neutral_drift_exceeds_the_cap() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("delta-selfheal-escalate");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        crate::index::CloneEdgeStatus::Complete
    );

    let conn = db.storage.connection();
    let flip: Vec<String> = conn
        .prepare("SELECT DISTINCT path FROM clone_subblock_postings ORDER BY path LIMIT 2")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(flip.len(), 2, "fixture has at least two files with postings");
    let revision_before = db.content_revision().unwrap();
    for path in &flip {
        conn.execute("UPDATE main.files SET generated = 1 WHERE path = ?1", [path]).unwrap();
    }
    assert_eq!(
        db.content_revision().unwrap(),
        revision_before,
        "flipping generated flags does not move the revision"
    );

    // Cap of 1 < 2 drifted paths → Escalate; and the graph still reads fresh against the
    // (unmoved) revision — the watcher's `force_revision_neutral_rebuild` condition.
    let report = db.apply_clone_graph_delta_hinted(1, CloneDeltaHint::SelfHeal).unwrap();
    assert_eq!(
        report.status,
        CloneDeltaStatus::Escalate,
        "oversized revision-neutral drift escalates: {report:?}"
    );
    assert!(
        !db.clone_graph_stale().unwrap(),
        "the graph is fresh against the revision, so the quiet gate would suppress the rebuild"
    );
}
