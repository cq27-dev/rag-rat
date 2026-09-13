use super::*;
use crate::index::IndexDatabase;
use crate::index::schema_bootstrap_tests::poison_test_config;

/// The harness self-check: after a normal rebuild (poison ON by default), an INTENTIONALLY
/// unscoped probe of a scoped table MUST see the sentinel — proving the tripwires are live. If
/// this fails, the harness is asleep and every "sibling intact" downstream assertion is
/// meaningless.
#[test]
fn poison_tripwires_are_live_after_a_default_rebuild() {
    let (_root, config) = poison_test_config("poison_live");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // Unscoped total over a direct-scoped table sees BOTH the fixture repo and the sibling.
    let sibling_files: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.files WHERE repo_id = ?1", [POISON_REPO_ID], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(sibling_files >= 1, "the poison file must be seeded by the default rebuild");

    // And every tripwire is intact right after seeding.
    assert_sibling_intact(conn);
}

#[test]
fn poison_sibling_seeds_memory_model_failure_tripwire() {
    let (_root, config) = poison_test_config("poison_failure");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    let (pass, reason): (String, String) = conn
        .query_row(
            "SELECT pass, reason FROM memory_model_failures WHERE repo_id = ?1 AND memory_id = ?2",
            [POISON_REPO_ID, POISON_MEMORY_ID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(pass, "compact");
    assert_eq!(reason, "summary_guard_rejected");
}

/// SAME-PATH tripwire liveness: the harness must seed a sibling row whose join key COLLIDES
/// with a real primary-repo path, and an intentionally path-keyed UNSCOPED aggregate must see
/// it — proving the harness can trip a join-by-path leak (the class the distinct-path rows
/// cannot). The paired assertion pins the FIX: the SCOPED production `repo_brief` attributes
/// ZERO of the sibling's refs to the primary path. If the unscoped side stops seeing the
/// sentinel, the same-path harness is asleep and every join-by-path scoping test is toothless.
#[test]
fn same_path_tripwires_expose_a_join_by_path_leak() {
    let (_root, config) = poison_test_config("poison_samepath");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // The real primary path the harness collided onto — re-resolved the same deterministic way
    // `primary_collision_path` picks it (src/lib.rs for this fixture).
    let collision_path: String = conn
        .query_row(
            "SELECT path FROM main.files WHERE repo_id != ?1 ORDER BY path LIMIT 1",
            [POISON_REPO_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !collision_path.starts_with(POISON_PREFIX),
        "the collision path must be a REAL primary path, got `{collision_path}`"
    );

    // The UNSCOPED join-by-path shape (the pre-fix `papertrail_ref_counts` CTE) sees the
    // sibling's colliding ref at the primary path — the tripwire is live.
    let unscoped_refs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM papertrail_refs WHERE source_path = ?1",
            [&collision_path],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        unscoped_refs >= 1,
        "same-path papertrail_refs tripwire is asleep: an unscoped path aggregate saw no sibling \
         ref at the primary path `{collision_path}`",
    );

    // The SCOPED production surface must NOT leak it: the fixture repo has no
    // papertrail_refs of its own, so its `src/lib.rs` candidate reports zero refs.
    let brief = db
        .repo_brief(rag_rat_query::repo_brief::RepoBriefOptions {
            mode: rag_rat_query::repo_brief::RepoBriefMode::Spine,
            limit: 50,
            include_generated: true,
            include_memories: false,
        })
        .unwrap();
    let primary = brief
        .candidates
        .iter()
        .find(|candidate| candidate.path == collision_path)
        .expect("the primary path must appear in the brief");
    assert_eq!(
        primary.metrics.papertrail_ref_count, 0,
        "repo_brief leaked a sibling repo's papertrail_refs across the shared path \
         `{collision_path}` — scope the papertrail_ref_counts CTE by repo_id",
    );

    // The same-path rows are counted by the intact check too.
    assert_sibling_intact(conn);
}

/// A full mirror rebuild (the full re-walk / recovery path) DELETEs the whole
/// `papertrail_fts` mirror and re-derives it from the base tables — every poisoned base row
/// becomes a derived mirror row. The harness must reconverge: the seeded mirror rows are
/// derivation-faithful copies (same doc_kind / comment_id / url / title / body slots), so the
/// rebuilt mirror carries the SAME tripwire set and `assert_sibling_intact` stays meaningful
/// after a mid-test rebuild. This pins `seed_sibling`'s fts seeding to BOTH derivations (the
/// incremental writers and `rebuild_fts` share the slot mapping) — if a column mapping in
/// either drifts, this fails locally instead of surfacing as a phantom sibling leak in
/// whichever papertrail test rebuilds first.
#[test]
fn papertrail_fts_tripwires_survive_a_mirror_rebuild() {
    let (_root, config) = poison_test_config("poison_resync");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();

    // Intact on the seeded mirror…
    assert_sibling_intact(conn);
    // …and byte-equivalently intact on the re-derived mirror.
    rag_rat_papertrail::rebuild_fts(conn).unwrap();
    assert_sibling_intact(conn);
}

/// Opt-out honored: with the guard held, a rebuild seeds NO tripwire rows.
#[test]
fn disabling_the_harness_seeds_no_tripwires() {
    let _guard = disable_poison_sibling();
    let (_root, config) = poison_test_config("poison_optout");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let sibling_files: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE repo_id = ?1", [POISON_REPO_ID], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(sibling_files, 0, "opt-out must seed no poison rows");
}

/// A7: on a git fixture (a REAL registered repo), the sibling now becomes a SECOND real repo —
/// `repos` + `repo_roots` + `repo_meta` rows — so the DB is a genuine multi-repo shape and an
/// unscoped registry read/count/delete trips a tripwire. The fixture resolves by identity /
/// recorded root (never `sole_repo_id`), so the second real repo does not hijack it.
#[test]
fn the_sibling_is_a_real_repo_on_a_git_fixture() {
    let (_root, config) = poison_test_config("poison_registry");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let conn = db.storage.connection();
    let poison_registered: i64 = conn
        .query_row("SELECT COUNT(*) FROM repos WHERE repo_id = ?1", [POISON_REPO_ID], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(poison_registered, 1, "the sibling is registered as a real repo on a git fixture");
    assert!(
        rag_rat_db::schema::multiple_real_repos(conn).unwrap(),
        "the fixture + the sibling make the DB genuinely multi-repo",
    );
    // The sibling's registry rows are counted by the intact check (the registry tripwires are
    // appended because `primary_is_real`).
    assert_sibling_intact(conn);
}
