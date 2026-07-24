use super::build::{build_sub_block_index, resolve_symbol_anchors};
use super::storage::open_building_generation;
use super::*;

/// The config for a fixed two-file fixture with two renamed-clone groups
/// (load_user/load_order and compute_totals/tally_amounts). Identical file CONTENT across tags
/// → identical content-key edges, so two builds are directly comparable. Split out so a
/// test can `rebuild` the SAME config twice (identical content) to exercise the
/// full-rebuild df refresh.
pub(in super::super) fn clone_fixture_config(tag: &str) -> rag_rat_base::config::Config {
    let root = std::env::temp_dir().join(format!(
        "rag-rat-precompute-{tag}-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
         compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s += it * 2; } \
         s + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\npub \
         fn tally_amounts(values: Vec<i64>) -> i64 { let mut t = 0; for v in values { t += v * 2; \
         } t + 1 }\n",
    )
    .unwrap();
    rag_rat_base::config::Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![rag_rat_base::config::ResolvedTarget {
            name: "rust".to_string(),
            language: rag_rat_base::language::Language::Rust,
            directories: vec![std::path::PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: rag_rat_base::config::TargetKind::Source,
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
    }
}

/// A fixed two-file fixture (see [`clone_fixture_config`]), rebuilt fresh.
fn build_clone_fixture(tag: &str) -> crate::IndexDatabase {
    crate::IndexDatabase::rebuild(&clone_fixture_config(tag)).unwrap()
}

/// The content-key set of the live-or-only generation's edges, sorted — the build-stable
/// identity of the persisted graph (symbol_id-independent).
pub(in super::super) fn edge_keys(db: &crate::IndexDatabase) -> Vec<(String, i64, String, i64)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT a_path, a_start_byte, b_path, b_start_byte FROM clone_edges
             ORDER BY a_path, a_start_byte, b_path, b_start_byte",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[test]
fn precompute_writes_graph_and_skips_when_current() {
    // Asserts a whole-DB `clone_graph_generations` count; opt out of the poison harness whose
    // sibling seeds another repo's generation.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let db = build_clone_fixture("write");
    let report = db.precompute_clone_graph(None).unwrap();
    assert_eq!(report.status, "Complete", "fresh precompute completes");
    assert!(!edge_keys(&db).is_empty(), "renamed-clone fixture writes edges");

    let conn = db.storage.connection();
    let live: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM repo_meta WHERE key = \
             'clone_graph_live_generation'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM clone_graph_generations WHERE generation = ?1",
            [live],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "Complete", "the live generation is Complete");

    // Re-running on unchanged content is a skip-when-current no-op (no new generation).
    let again = db.precompute_clone_graph(None).unwrap();
    assert_eq!(again.status, "Current");
    let generations: i64 =
        conn.query_row("SELECT COUNT(*) FROM clone_graph_generations", [], |r| r.get(0)).unwrap();
    assert_eq!(generations, 1, "skip-when-current adds no generation");
}

#[test]
fn precompute_resume_matches_single_pass() {
    // Reference: one uninterrupted pass.
    let single = build_clone_fixture("single");
    single.precompute_clone_graph(None).unwrap();
    let expected = edge_keys(&single);
    assert!(!expected.is_empty());

    // Resumed: a per-symbol budget that trips after every batch forces many resumable passes.
    let resumed = build_clone_fixture("resume");
    let mut passes = 0;
    loop {
        let report = resumed
            .reconcile_clone_edges_pass(&CloneEdgeOptions {
                max_seconds: Some(0),
                batch_size: 1,
                force: false,
            })
            .unwrap();
        passes += 1;
        assert!(passes < 10_000, "must converge");
        if report.status != "Partial" {
            assert_eq!(report.status, "Complete");
            break;
        }
    }
    assert!(passes >= 2, "a tiny budget forces multiple resumable passes, got {passes}");
    assert_eq!(
        edge_keys(&resumed),
        expected,
        "the resumed (checkpointed) graph equals the single-pass graph — the smaller-endpoint \
         partition is correct across checkpoints"
    );
}

/// A stable, symbol-id-independent projection of a `find_clones` result: each class as its
/// sorted member refs, classes sorted. Equal projections ⇒ the same clone classes.
fn class_projection(result: &crate::index::FindClonesResult) -> Vec<Vec<String>> {
    let mut classes: Vec<Vec<String>> = result
        .classes
        .iter()
        .map(|c| {
            let mut refs: Vec<String> = c.members.iter().map(|m| m.r#ref.clone()).collect();
            refs.sort();
            refs
        })
        .collect();
    classes.sort();
    classes
}

/// THE CORNERSTONE (#286 Phase C): `find_clones` served from the persisted graph is IDENTICAL
/// to `find_clones` recomputed live, at θ = 0.7 (the precompute floor) and above (where the
/// stored edges are θ-filtered). Proven on the same index: capture live first (no graph →
/// live path), precompute, capture again (graph present → fast path), assert equal. This is
/// what makes the fast path a pure optimization rather than a behavior change.
#[test]
fn find_clones_precomputed_matches_live() {
    use crate::index::FindClonesOptions;

    for theta in [0.7_f64, 0.8, 0.9] {
        let db = build_clone_fixture(&format!("parity-{}", (theta * 100.0) as i64));
        let opts =
            || FindClonesOptions { min_similarity: Some(theta), min_copies: None, limit: None };

        // No graph yet → live path.
        let live = class_projection(&db.find_clones(opts()).unwrap());
        assert!(!live.is_empty(), "renamed-clone fixture has classes at θ={theta}");

        // Build the graph → subsequent find_clones takes the fast path.
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        let fast = class_projection(&db.find_clones(opts()).unwrap());

        assert_eq!(fast, live, "precomputed find_clones must equal live at θ={theta}");
    }
}

/// The content-key set of the live generation's postings, sorted — the build-stable identity of
/// the persisted postings (symbol-id-independent, the postings analogue of [`edge_keys`]).
fn posting_keys(db: &crate::IndexDatabase) -> Vec<(i64, String, i64, String)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT token_hash, path, start_byte, file_sha FROM clone_subblock_postings
             ORDER BY token_hash, path, start_byte, file_sha",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// PARITY (design §Invariants #2): every persisted posting resolves to a symbol that
/// `build_sub_block_index` — the RAM index the live candidate-gen uses — places under the SAME
/// token, and vice versa. Pinning the persisted set as a byte-for-byte mirror of the RAM index
/// is what will make the Phase-C postings fast path return the same candidates as the fallback.
#[test]
fn precompute_postings_match_sub_block_index() {
    let db = build_clone_fixture("postings-parity");
    assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");

    let conn = db.storage.connection();
    // Expected: token_hash -> {symbol_id} from the in-RAM sub-block index over the scoped bags.
    let bags = load_scoped_baseline_bags(conn).unwrap();
    assert!(!bags.is_empty(), "the fixture has scoped bags");
    let expected: BTreeMap<i64, BTreeSet<i64>> =
        build_sub_block_index(&bags, CLONE_PRECOMPUTE_THETA)
            .into_iter()
            .map(|(token, ids)| (token, ids.into_iter().collect()))
            .collect();

    // Actual: token_hash -> {symbol_id} from the persisted postings, resolving each content
    // anchor (path, start_byte) back to its live symbol id (the read-path resolution shape).
    let by_anchor: HashMap<(String, i64), i64> = resolve_symbol_anchors(conn)
        .unwrap()
        .into_iter()
        .map(|(id, (path, start_byte, _sha))| ((path, start_byte), id))
        .collect();
    let mut actual: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    let mut stmt =
        conn.prepare("SELECT token_hash, path, start_byte FROM clone_subblock_postings").unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))
        .unwrap();
    for row in rows {
        let (token, path, start_byte) = row.unwrap();
        actual.entry(token).or_default().insert(by_anchor[&(path, start_byte)]);
    }

    assert_eq!(actual, expected, "persisted postings mirror the RAM sub-block index exactly");
}

/// RESUME IDEMPOTENCY (review R6): a budget-split precompute (one symbol per pass) yields the
/// SAME postings as a single uninterrupted pass. Guards the "postings staged before the cursor
/// advances" contract — a split between "postings written" and "cursor advanced" would drop a
/// symbol's postings on resume. Mirrors [`precompute_resume_matches_single_pass`] for edges.
#[test]
fn precompute_postings_resume_matches_single_pass() {
    let single = build_clone_fixture("postings-single");
    single.precompute_clone_graph(None).unwrap();
    let expected = posting_keys(&single);
    assert!(!expected.is_empty(), "the fixture writes postings");

    let resumed = build_clone_fixture("postings-resume");
    let mut passes = 0;
    loop {
        let report = resumed
            .reconcile_clone_edges_pass(&CloneEdgeOptions {
                max_seconds: Some(0),
                batch_size: 1,
                force: false,
            })
            .unwrap();
        passes += 1;
        assert!(passes < 10_000, "must converge");
        if report.status != "Partial" {
            assert_eq!(report.status, "Complete");
            break;
        }
    }
    assert!(passes >= 2, "a tiny budget forces multiple resumable passes, got {passes}");
    assert_eq!(
        posting_keys(&resumed),
        expected,
        "the resumed (checkpointed) postings equal the single-pass postings"
    );
}

/// UPGRADE REPOPULATION (review R2): a DB whose clone graph was already `Complete` BEFORE
/// postings existed (`postings_written = 0`, empty `clone_subblock_postings`) is treated as
/// pending and rebuilt ONCE to fill the postings — instead of skip-when-current leaving the
/// table empty forever. Self-correcting: no `content_revision` change or manual rebuild needed.
#[test]
fn precompute_repopulates_postings_on_upgrade() {
    let db = build_clone_fixture("postings-upgrade");
    assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");

    // Simulate the pre-feature on-disk state: a Complete live generation that predates
    // postings.
    db.storage
        .connection()
        .execute_batch(
            "UPDATE clone_graph_generations SET postings_written = 0;
             DELETE FROM clone_subblock_postings;",
        )
        .unwrap();
    assert!(db.pending_clone_graph().unwrap(), "a postings-less live generation is pending");

    // One reconcile pass rebuilds a postings-full generation and clears the pending state.
    assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
    let postings: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM clone_subblock_postings", [], |r| r.get(0))
        .unwrap();
    assert!(postings > 0, "the upgrade rebuild fills clone_subblock_postings");
    assert!(!db.pending_clone_graph().unwrap(), "no longer pending after the rebuild");
}

/// The background quiet-window gate (#472): a pending clone graph does NOT fire a rebuild on
/// first observation — the probe ARMS the window by recording the stale revision — and fires
/// only once that revision has stayed stable past the window. This is what stops sustained
/// editing from treadmilling full-generation rebuilds on every watcher/maintenance pass.
#[test]
fn clone_graph_quiet_gate_arms_then_fires_after_the_window() {
    let db = build_clone_fixture("quiet-gate-arms");
    assert!(db.pending_clone_graph().unwrap(), "fresh fixture has no generation yet");
    assert!(
        !db.clone_graph_rebuild_due_at(1_000, 300_000, true).unwrap(),
        "first observation arms the window instead of firing"
    );
    assert!(
        !db.clone_graph_rebuild_due_at(1_000 + 299_999, 300_000, true).unwrap(),
        "still inside the window"
    );
    assert!(
        db.clone_graph_rebuild_due_at(1_000 + 300_000, 300_000, true).unwrap(),
        "a stable revision past the window fires"
    );
}

/// Content moving while armed re-arms the window for the NEW revision: sustained editing keeps
/// deferring (the treadmill fix), and only a revision that stays put for the full window fires.
#[test]
fn clone_graph_quiet_gate_rearms_when_the_revision_moves() {
    let config = clone_fixture_config("quiet-gate-rearm");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert!(!db.clone_graph_rebuild_due_at(1_000, 300_000, true).unwrap(), "arm");
    drop(db);

    // Edit a fixture file and re-index so `content_revision()` moves past the armed candidate
    // (the armed `clone_graph_quiet_*` repo_meta survives the rebuild, like the live-generation
    // pointer does).
    let a = config.root.join("src/a.rs");
    let mut text = std::fs::read_to_string(&a).unwrap();
    text.push_str("pub fn freshly_added(x: i32) -> i32 { x + 41 }\n");
    std::fs::write(&a, text).unwrap();
    let db = crate::IndexDatabase::rebuild(&config).unwrap();

    assert!(
        !db.clone_graph_rebuild_due_at(10_000_000, 300_000, true).unwrap(),
        "a moved revision re-arms instead of firing, however long the old candidate sat"
    );
    assert!(
        db.clone_graph_rebuild_due_at(10_000_000 + 300_000, 300_000, true).unwrap(),
        "the new revision fires once it has been stable for the window"
    );
}

/// `probe_without_candidate = false` (an idle watcher pass with no deferred rebuild owed)
/// skips the probe entirely — nothing is armed, so an idle server never pays the
/// content-revision digest for the gate.
#[test]
fn clone_graph_quiet_gate_skips_the_probe_without_a_candidate() {
    let db = build_clone_fixture("quiet-gate-cold");
    assert!(
        !db.clone_graph_rebuild_due_at(1_000, 300_000, false).unwrap(),
        "no candidate + no probe permission means not due"
    );
    // The cold call did no bookkeeping: a later probing call still only ARMS.
    assert!(
        !db.clone_graph_rebuild_due_at(50_000_000, 300_000, true).unwrap(),
        "the probing call after a cold one arms fresh"
    );
    assert!(db.clone_graph_rebuild_due_at(50_000_000 + 300_000, 300_000, true).unwrap());
}

/// An ARMED candidate bypasses `probe_without_candidate = false`: an overlay-only or idle
/// pass — which carries no probe permission since #817 — still fires a quiet-elapsed owed
/// rebuild. Arming is what needs permission (a content/gc/backlog pass); firing is not.
#[test]
fn clone_graph_quiet_gate_fires_an_armed_candidate_without_probe_permission() {
    let db = build_clone_fixture("quiet-gate-armed-fires");
    assert!(!db.clone_graph_rebuild_due_at(1_000, 300_000, true).unwrap(), "arm");
    assert!(
        !db.clone_graph_rebuild_due_at(2_000, 300_000, false).unwrap(),
        "armed but not quiet-elapsed: not due"
    );
    assert!(
        db.clone_graph_rebuild_due_at(1_000 + 300_000, 300_000, false).unwrap(),
        "a quiet-elapsed armed candidate fires on a pass with no probe permission"
    );
}

/// Once the graph is current the gate reports not-due and drops the armed candidate, so idle
/// passes go back to the cheap no-candidate path.
#[test]
fn clone_graph_quiet_gate_clears_once_current() {
    let db = build_clone_fixture("quiet-gate-clear");
    assert!(!db.clone_graph_rebuild_due_at(1_000, 300_000, true).unwrap(), "arm");
    assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
    assert!(
        !db.clone_graph_rebuild_due_at(90_000_000, 300_000, true).unwrap(),
        "a current graph is never due, regardless of the armed candidate's age"
    );
    let leftover: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM repo_meta WHERE key LIKE 'clone_graph_quiet_%'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(leftover, 0, "the armed candidate is dropped once the graph is current");
}

/// #479: a FRESH Building generation pins its build-time df order durably in
/// `clone_df_epoch`, so the persisted postings survive later movement of the live
/// `clone_token_df`. A RESUMED partial must not re-snapshot — its postings are ordered by the
/// epoch its build opened under, and a mid-build df movement must not leak in.
#[test]
fn a_fresh_build_snapshots_the_df_epoch_and_a_resume_preserves_it() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let epoch_rows = |db: &crate::IndexDatabase, generation: i64| -> Vec<(i64, i64)> {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT token_hash, df FROM clone_df_epoch WHERE build_generation = ?1
                 ORDER BY token_hash",
            )
            .unwrap();
        stmt.query_map([generation], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let df_rows = |db: &crate::IndexDatabase| -> Vec<(i64, i64)> {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT token_hash, df FROM clone_token_df WHERE normalizer_kind = 'baseline'
                 ORDER BY token_hash",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };

    let db = build_clone_fixture("df-epoch-fresh");
    let report = db.precompute_clone_graph(None).unwrap();
    assert_eq!(report.status, "Complete");
    let fresh_epoch = epoch_rows(&db, report.generation);
    assert!(!fresh_epoch.is_empty(), "a fresh build snapshots its df epoch");
    assert_eq!(fresh_epoch, df_rows(&db), "the snapshot equals the df the build ran under");

    // Resume: trip the budget so a Building generation persists, move the live df between
    // passes (what an interleaved incremental bump does), and finish the build.
    let resumed = build_clone_fixture("df-epoch-resume");
    let first = resumed
        .reconcile_clone_edges_pass(&CloneEdgeOptions {
            max_seconds: Some(0),
            batch_size: 1,
            force: false,
        })
        .unwrap();
    assert_eq!(first.status, "Partial", "a zero-second budget trips after one batch");
    let open_epoch = epoch_rows(&resumed, first.generation);
    assert!(!open_epoch.is_empty(), "the epoch is pinned when the generation opens");
    // Adversarial mid-build live-df movement: invert the whole table between the paused
    // passes (an incremental bump storm's worst case).
    resumed
        .storage
        .connection()
        .execute("UPDATE clone_token_df SET df = 1000000 - df", [])
        .unwrap();
    let mut passes = 0;
    let completed = loop {
        let report = resumed
            .reconcile_clone_edges_pass(&CloneEdgeOptions {
                max_seconds: Some(0),
                batch_size: 1,
                force: false,
            })
            .unwrap();
        passes += 1;
        assert!(passes < 10_000, "must converge");
        if report.status != "Partial" {
            assert_eq!(report.status, "Complete");
            break report;
        }
    };
    assert_eq!(
        epoch_rows(&resumed, completed.generation),
        open_epoch,
        "a resume preserves the open-time epoch — the mid-build df movement must not leak in"
    );
    // And the resumed passes must EMIT under that epoch too (Codex review): every persisted
    // posting — including the symbols walked after the inversion — matches the sub-block
    // selection under the pinned epoch, not the moved live table.
    let conn = resumed.storage.connection();
    let epoch_df = load_clone_df_epoch(conn, completed.generation).unwrap();
    let live_df = super::super::substrate::load_current_clone_df(conn).unwrap();
    let union_under = |df: &std::collections::HashMap<i64, i64>| -> BTreeSet<i64> {
        load_scoped_baseline_bags_with_df(conn, df)
            .unwrap()
            .iter()
            .flat_map(|bag| sub_block_tokens(bag, CLONE_PRECOMPUTE_THETA))
            .collect()
    };
    let under_epoch = union_under(&epoch_df);
    assert_ne!(
        under_epoch,
        union_under(&live_df),
        "precondition: the inversion must actually change the prefix selection"
    );
    let persisted: BTreeSet<i64> = conn
        .prepare(
            "SELECT DISTINCT token_hash FROM clone_subblock_postings
             WHERE build_generation = ?1",
        )
        .unwrap()
        .query_map(params![completed.generation], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        persisted, under_epoch,
        "resumed passes emit postings under the pinned epoch, not the moved live df"
    );
}

/// #479 empty-graph sentinel (Codex review): a repo with no baseline fingerprints (docs-only,
/// data-only) builds a Complete generation with ZERO postings and therefore ZERO epoch rows —
/// a legitimately empty order, not a lost one. It must read as current, or
/// `pending_clone_graph` would schedule a rebuild of the already-current empty graph on every
/// maintenance pass, forever.
#[test]
fn an_empty_generation_without_epoch_rows_stays_current() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let mut config = clone_fixture_config("df-epoch-empty");
    // Replace the clone fixture's sources with a data-only file: no functions, so nothing is
    // fingerprinted and the built graph is empty.
    std::fs::remove_file(config.root.join("src/a.rs")).unwrap();
    std::fs::remove_file(config.root.join("src/b.rs")).unwrap();
    std::fs::write(config.root.join("src/data.rs"), "pub struct OnlyData { pub x: i64 }\n")
        .unwrap();
    config.allow_empty = true;
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
    assert!(
        !db.pending_clone_graph().unwrap(),
        "an empty generation with no epoch rows is current, not perpetually pending"
    );
    assert_eq!(
        db.precompute_clone_graph(None).unwrap().status,
        "Current",
        "and the next pass skips instead of rebuilding the empty graph forever"
    );
    drop(db);

    // The FIRST fingerprinted content arrives: the delta must REFUSE (it would otherwise
    // write the generation's first postings under an empty epoch map — postings no reader
    // could order; Codex review) and the full path builds a fresh, epoch-pinned generation.
    std::fs::write(
        config.root.join("src/first.rs"),
        "pub fn first_function(q: i64) -> i64 { q * 13 + 1 }\n",
    )
    .unwrap();
    let (db, _changed) = crate::IndexDatabase::index_discover_reporting(&config).unwrap();
    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(
        report.status, "NotEligible",
        "the delta must not create first postings on an epoch-less generation: {report:?}"
    );
    assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
    assert!(
        db.clone_check_indexed_generation().unwrap().is_some(),
        "the full rebuild pins an epoch and the fast path serves"
    );
}

/// #479 upgrade defense: postings without their generation's epoch rows cannot be ordered
/// correctly (the reader would fall to DF_FALLBACK for every token — a silently different
/// order than the postings were built under). A missing epoch must behave like
/// `postings_written = 0`: the fast path falls back and the delta refuses, so one full
/// rebuild self-heals instead of silently losing recall.
#[test]
fn a_generation_without_epoch_rows_is_not_servable() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let db = build_clone_fixture("df-epoch-eligibility");
    assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
    assert!(
        db.clone_check_indexed_generation().unwrap().is_some(),
        "a fresh build with its epoch serves the fast path"
    );
    db.storage.connection().execute("DELETE FROM clone_df_epoch", []).unwrap();
    assert!(
        db.clone_check_indexed_generation().unwrap().is_none(),
        "an epoch-less generation must not serve the postings fast path"
    );
    let report = db.apply_clone_graph_delta(64).unwrap();
    assert_eq!(
        report.status, "NotEligible",
        "the delta must not patch postings whose build order is unknown: {report:?}"
    );
    // The self-heal loop must CLOSE (Codex review of this change): the unservable state has
    // to read as pending and rebuild on the next pass — not skip as "Current" forever, which
    // would strand the fast path and the delta on the fallback.
    assert!(
        db.pending_clone_graph().unwrap(),
        "an epoch-less generation reads as pending, scheduling the healing rebuild"
    );
    let heal = db.precompute_clone_graph(None).unwrap();
    assert_eq!(heal.status, "Complete", "the pass rebuilds instead of skipping as current");
    assert!(
        db.clone_check_indexed_generation().unwrap().is_some(),
        "the rebuilt generation pins a fresh epoch and serves again"
    );
}

/// `quiet_ms = 0` disables the gate — a pending graph is immediately due (the pre-#472
/// immediate-rebuild behavior).
#[test]
fn clone_graph_quiet_gate_zero_window_fires_immediately() {
    let db = build_clone_fixture("quiet-gate-zero");
    assert!(db.clone_graph_rebuild_due_at(1_000, 0, true).unwrap());
}

/// #479: incremental passes bump the LIVE `clone_token_df` (a new file's tokens get real df
/// instead of riding `DF_FALLBACK` until the next full build — the live-fallback recall fix),
/// while the published generation's PINNED epoch stays byte-identical and its postings stay
/// servable. This inverts the #473 whole-table freeze: the freeze now lives per generation in
/// `clone_df_epoch`, not on the live table.
#[test]
fn incremental_index_bumps_live_df_and_keeps_the_generation_epoch_frozen() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("df-live-bump");
    let db = crate::IndexDatabase::rebuild(&config).unwrap();
    let report = db.precompute_clone_graph(None).unwrap();
    assert_eq!(report.status, "Complete");
    let generation = report.generation;
    let df_rows = |db: &crate::IndexDatabase| -> Vec<(i64, i64)> {
        let conn = db.storage.connection();
        let mut stmt =
            conn.prepare("SELECT token_hash, df FROM clone_token_df ORDER BY token_hash").unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let epoch_rows = |db: &crate::IndexDatabase| -> Vec<(i64, i64)> {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT token_hash, df FROM clone_df_epoch WHERE build_generation = ?1
                 ORDER BY token_hash",
            )
            .unwrap();
        stmt.query_map([generation], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let live_df = df_rows(&db);
    let pinned_epoch = epoch_rows(&db);
    assert!(!live_df.is_empty(), "the full rebuild computed the df");
    assert_eq!(live_df, pinned_epoch, "at build time the live df IS the epoch");
    drop(db);

    // A new file with brand-new tokens through the watcher/maintenance incremental path.
    std::fs::write(
        config.root.join("src/live_probe.rs"),
        "pub fn live_bump_probe(zx: u64) -> u64 { zx.rotate_left(9) ^ 0xfeed_beef }\n",
    )
    .unwrap();
    crate::watch::maintenance_pass(&config, false).unwrap();

    let db = crate::IndexDatabase::open_config(&config).unwrap();
    assert!(
        df_rows(&db).len() > live_df.len(),
        "the incremental pass bumps the live df with the new file's tokens"
    );
    assert_eq!(
        epoch_rows(&db),
        pinned_epoch,
        "the generation's pinned epoch never moves after its build"
    );
    assert!(
        db.clone_check_indexed_generation().unwrap().is_some(),
        "the live df movement must not invalidate the generation's postings (they are \
         epoch-pinned; the maintenance pass's delta keeps them fresh)"
    );
}

/// End-to-end through the watcher pass (#472): a content-changing maintenance pass ARMS the
/// gate and DEFERS the clone rebuild; once the armed candidate has sat past the quiet window,
/// an otherwise-idle pass picks the owed rebuild up (the gate is also a tail trigger) and
/// completes it.
#[test]
fn maintenance_pass_defers_the_clone_rebuild_until_the_quiet_window() {
    // Asserts whole-DB `clone_graph_generations` counts; opt out of the poison harness whose
    // sibling seeds another repo's generation.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let config = clone_fixture_config("quiet-gate-pass");
    drop(crate::IndexDatabase::rebuild(&config).unwrap());

    // A content change lands, then a maintenance pass runs while the window is still open: it
    // must arm and defer (no generation built), not discard-and-rebuild.
    let a = config.root.join("src/a.rs");
    let mut text = std::fs::read_to_string(&a).unwrap();
    text.push_str("pub fn freshly_edited(x: i32) -> i32 { x * 3 }\n");
    std::fs::write(&a, text).unwrap();
    crate::watch::maintenance_pass(&config, false).unwrap();

    let db = crate::IndexDatabase::open_config(&config).unwrap();
    let generations: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM clone_graph_generations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(generations, 0, "a pass inside the quiet window defers the clone rebuild");

    // Backdate the armed candidate past the window, then run an IDLE pass: the owed rebuild
    // is now due, forces the otherwise-skipped tail, and completes.
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
    let complete: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM clone_graph_generations WHERE status = 'Complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(complete, 1, "the quiet-elapsed pass builds the graph to completion");
    assert!(!edge_keys(&db).is_empty(), "and the fixture's clone edges are persisted");
}

/// A second FULL index rebuild over identical content leaves the clone graph transiently
/// pending — `content_revision()` digests raw `main.files`, and the freshly staged file
/// generation coexists with the superseded one until gc, so the digest moves — and one
/// precompute settles it. #479 note: the df refresh the rebuild runs no longer invalidates
/// anything (`invalidate_clone_graph_postings` is gone — postings are pinned to their own
/// `clone_df_epoch`); the pending window here is purely the revision-key movement.
#[test]
fn a_second_full_rebuild_leaves_the_graph_pending_until_one_precompute() {
    let config = clone_fixture_config("df-refresh");
    let db1 = crate::IndexDatabase::rebuild(&config).unwrap();
    db1.precompute_clone_graph(None).unwrap();
    assert!(!db1.pending_clone_graph().unwrap(), "a fresh precompute is current");
    assert!(
        db1.clone_check_indexed_generation().unwrap().is_some(),
        "and the write-time fast path is eligible"
    );
    drop(db1); // release the DB file before the second rebuild takes the write lock

    // Second FULL rebuild over IDENTICAL content: the clone graph survives (it is content-
    // anchored), but the staged file generation moves the revision key until gc.
    let db2 = crate::IndexDatabase::rebuild(&config).unwrap();
    assert!(
        db2.pending_clone_graph().unwrap(),
        "the staged-generation revision drift leaves the graph pending"
    );
    assert!(
        db2.clone_check_indexed_generation().unwrap().is_none(),
        "and the write-time fast path falls back to RAM until the graph settles"
    );

    // Settles on the next precompute (a maintenance pass's delta would re-pin it likewise).
    assert_eq!(db2.precompute_clone_graph(None).unwrap().status, "Complete");
    assert!(!db2.pending_clone_graph().unwrap(), "current again after the rebuild");
    assert!(db2.clone_check_indexed_generation().unwrap().is_some(), "eligible again");
}

// --- #413 finding #6: the global clone-generation cleanups are guarded on a multi-repo DB
// (`clone_graph_generations` has no `repo_id` until the V042 seam). ---

/// Register two REAL repos so `schema::multiple_real_repos` reports the consolidated shape. The
/// fixture DB is non-git (unadopted), so its registry holds only the placeholder; inserting the
/// rows directly is the white-box multi-repo construction (`register_repo` refuses a second
/// real repo until A7).
fn make_multi_repo(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-x', 'x', 0);
         INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-y', 'y', 0);",
    )
    .unwrap();
}

/// Insert a `clone_graph_generations` row in `status` toward `source_revision`.
fn seed_generation(
    conn: &rusqlite::Connection,
    generation: i64,
    status: &str,
    revision: &str,
    repo_id: &str,
) {
    // Stamp `repo_id` explicitly: since V042 `complete_generation` / `open_building_generation`
    // scope by `clone_graph_generations.repo_id` (superseding the old `multiple_real_repos`
    // guard), a "sibling" generation must carry a DIFFERENT id than the active repo for these
    // tests to exercise the per-repo predicate.
    conn.execute(
        "INSERT INTO clone_graph_generations
            (generation, status, theta_floor, normalizer_kind, normalizer_version,
             source_revision, cursor_symbol_id, edges_written, postings_written, started_at_ms,
             repo_id)
         VALUES (?1, ?2, 0.0, 'baseline', ?3, ?4, 0, 0, 1, 0, ?5)",
        params![generation, status, NORM_VERSION, revision, repo_id],
    )
    .unwrap();
}

fn generation_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM clone_graph_generations", [], |r| r.get(0)).unwrap()
}

/// `complete_generation` GCs every OTHER generation of the ACTIVE repo; the V042 `repo_id`
/// predicate scopes that delete, so a sibling repo's live generation is spared (superseding the
/// old `multiple_real_repos` guard).
#[test]
fn complete_generation_spares_sibling_generations_on_a_multi_repo_db() {
    // Whole-DB `generation_count` — opt out of the poison harness (whose sibling seeds another
    // generation) so this test controls the exact generation set it asserts on.
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let db = build_clone_fixture("mr-complete");
    {
        let conn = db.storage.connection();
        make_multi_repo(conn);
        // this repo's, to complete
        seed_generation(conn, 1, "Building", "rev-a", &db.active_repo_id);
        // a sibling repo's live generation
        seed_generation(conn, 2, "Complete", "rev-sibling", "repo-x");
    }
    db.complete_generation(1, 0).unwrap();

    let conn = db.storage.connection();
    assert_eq!(
        generation_count(conn),
        2,
        "the per-repo predicate spared the sibling repo's generation"
    );
    let g1_status: String = conn
        .query_row("SELECT status FROM clone_graph_generations WHERE generation = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(g1_status, "Complete", "this repo's generation still completes + publishes");
}

/// The complement: with only the active repo's own generations present, `complete_generation`
/// GCs every OTHER one of them.
#[test]
fn complete_generation_prunes_other_generations_on_a_single_repo_db() {
    // Whole-DB `generation_count` — opt out of the poison harness (its sibling generation is a
    // different repo's and is correctly spared, but would inflate this whole-DB count).
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let db = build_clone_fixture("sr-complete");
    {
        let conn = db.storage.connection();
        seed_generation(conn, 1, "Building", "rev-a", &db.active_repo_id);
        seed_generation(conn, 2, "Complete", "rev-old", &db.active_repo_id);
    }
    db.complete_generation(1, 0).unwrap();

    let conn = db.storage.connection();
    assert_eq!(generation_count(conn), 1, "GC drops every other generation of the active repo");
}

/// `open_building_generation` discards a stale (different-revision) Building row before
/// starting fresh; on a multi-repo DB that discard is global, so the guard must skip it and
/// leave the sibling's in-progress row intact while still allocating a fresh generation.
#[test]
fn open_building_generation_spares_a_sibling_building_row_on_a_multi_repo_db() {
    let db = build_clone_fixture("mr-open");
    let conn = db.storage.connection();
    make_multi_repo(conn);
    // a sibling repo's in-progress build
    seed_generation(conn, 7, "Building", "sibling-rev", "repo-x");

    let opened = open_building_generation(conn, "my-new-rev").unwrap();
    assert_ne!(opened.generation, 7, "a fresh generation is allocated, not the sibling's");
    let sibling: i64 = conn
        .query_row("SELECT COUNT(*) FROM clone_graph_generations WHERE generation = 7", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sibling, 1, "the sibling repo's Building row is not globally discarded");
}

/// #413 round-5: the multi-repo guard must not just skip the DISCARD — it must also skip the
/// RESUME. `source_revision` is `content_revision()`, GLOBAL over `main.files`, so a sibling's
/// Building row at the SAME revision (+ postings-aware) would otherwise be RESUMED and then
/// published under this repo's live pointer. On a multi-repo DB `open_building_generation` must
/// allocate a FRESH generation and leave the sibling's Building row untouched, even on a match.
#[test]
fn open_building_generation_does_not_resume_a_sibling_building_row_on_a_multi_repo_db() {
    let db = build_clone_fixture("mr-resume");
    let conn = db.storage.connection();
    make_multi_repo(conn);
    // A SIBLING repo's in-progress build at the SAME revision this repo is about to open (a
    // matching, postings-aware row — `seed_generation` writes `postings_written = 1`). The
    // pre-predicate code would RESUME generation 9 and hand it to this repo.
    seed_generation(conn, 9, "Building", "shared-rev", "repo-x");

    let opened = open_building_generation(conn, "shared-rev").unwrap();
    assert_ne!(
        opened.generation, 9,
        "a matching sibling Building row is NOT resumed on a multi-repo DB — fresh generation",
    );
    let sibling_status: String = conn
        .query_row("SELECT status FROM clone_graph_generations WHERE generation = 9", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sibling_status, "Building", "the sibling's Building row is left intact");
}

/// The complement: a single-repo DB discards its own stale Building row as before.
#[test]
fn open_building_generation_discards_the_stale_building_row_on_a_single_repo_db() {
    let db = build_clone_fixture("sr-open");
    let conn = db.storage.connection();
    seed_generation(conn, 7, "Building", "stale-rev", &db.active_repo_id);

    open_building_generation(conn, "new-rev").unwrap();
    let stale: i64 = conn
        .query_row("SELECT COUNT(*) FROM clone_graph_generations WHERE generation = 7", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stale, 0, "single-repo discards the stale Building row as before");
}
