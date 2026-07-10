//! Coverage for the version-stamped fast-path certification gates (#530): the rebuild stamp and its
//! `carried_rows` gate, the fast-path cap gate, the self-heal cap gate, and the plan's cap
//! threading. The end-to-end fast-path/self-heal behaviour lives in `reconcile_embeddings.rs`;
//! these target the individual GATES that decide whether the column may be trusted.

use super::*;
use crate::index::ai;

fn policy_version(db: &IndexDatabase) -> Option<String> {
    let conn = db.storage.connection();
    let repo_id = crate::index::schema::active_repo_id(conn).unwrap();
    crate::index::meta::repo_meta(conn, &repo_id, ai::EMBEDDING_POLICY_VERSION_KEY).unwrap()
}

fn stale_the_stamp(db: &IndexDatabase) {
    let conn = db.storage.connection();
    let repo_id = crate::index::schema::active_repo_id(conn).unwrap();
    crate::index::meta::set_repo_meta(
        conn,
        &repo_id,
        ai::EMBEDDING_POLICY_VERSION_KEY,
        "pre-upgrade",
    )
    .unwrap();
}

fn rust_fixture(root: &std::path::Path) {
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/code.rs"),
        "pub fn compute(x: i64, y: i64) -> i64 {\n    let sum = x * 2 + y - 1;\n    \
         sum.wrapping_mul(3)\n}\n",
    )
    .unwrap();
}

#[test]
fn rebuild_stamps_the_policy_version_current() {
    // A full rebuild (re)derives every chunk's policy with the current classifier, so it certifies
    // the column: the version + the DEFAULT cap the fast path reads at.
    let root = unique_temp_root();
    rust_fixture(&root);
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();
    let repo_id = crate::index::schema::active_repo_id(conn).unwrap();
    assert_eq!(policy_version(&db).as_deref(), Some(ai::EMBEDDING_POLICY_VERSION));
    assert_eq!(
        crate::index::meta::repo_meta(conn, &repo_id, ai::EMBEDDING_POLICY_CAP_KEY)
            .unwrap()
            .as_deref(),
        Some(ai::DEFAULT_MAX_EMBEDDING_CHARS.to_string().as_str()),
        "the stamp records the DEFAULT cap the column was derived at"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn rebuild_carrying_overlay_rows_does_not_certify() {
    // A rebuild that CARRIES un-reparsed rows forward (a second linked-worktree overlay /
    // other-commit leftover) must NOT (re)stamp the version — those rows kept their
    // old-classifier column, so a repo-wide stamp would let a fast summary trust them after an
    // upgrade. Same gate as the logical-key version's `carried_rows == 0`.
    let (root, config) = git_fixture_for_overlay_tests();
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(policy_version(&db).as_deref(), Some(ai::EMBEDDING_POLICY_VERSION));

    // Seed a live overlay row so the NEXT rebuild carries it, and simulate a classifier-version
    // bump since that clean stamp.
    insert_stale_overlay_row(&db, "src/lib.rs", "some-linked-worktree");
    stale_the_stamp(&db);

    // The carry-ful rebuild must leave the (stale) stamp untouched — never certify current.
    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_ne!(
        policy_version(&db).as_deref(),
        Some(ai::EMBEDDING_POLICY_VERSION),
        "a rebuild that carried un-reparsed overlay rows must not certify the version current"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn fast_path_refused_at_non_default_cap() {
    // The fast path may only read the column at the cap it was stamped at (DEFAULT). Poison a
    // chunk's column: at DEFAULT the fast path reads the poison; at a NON-DEFAULT cap the
    // cap-gate forces a recompute from source that ignores it.
    let root = unique_temp_root();
    rust_fixture(&root);
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();
    conn.execute(
        "UPDATE main.chunks SET embedding_policy = 'SkipGenerated'
         WHERE id = (SELECT MIN(id) FROM main.chunks WHERE embedding_policy = 'Embed')",
        [],
    )
    .unwrap();

    let fast = ai::embedding_policy_skip_summary(conn, ai::DEFAULT_MAX_EMBEDDING_CHARS).unwrap();
    let recomputed =
        ai::embedding_policy_skip_summary(conn, ai::DEFAULT_MAX_EMBEDDING_CHARS + 1_000).unwrap();
    assert!(
        fast.get("SkipGenerated").copied().unwrap_or(0)
            > recomputed.get("SkipGenerated").copied().unwrap_or(0),
        "default-cap fast path reads the poisoned column; a non-default cap recomputes from \
         source: {fast:?} vs {recomputed:?}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn self_heal_skipped_at_non_default_cap() {
    // A stale-stamp reconcile at a NON-DEFAULT cap must NOT self-heal: the heal reclassifies +
    // stamps at DEFAULT (which only a DEFAULT-cap summary can read), so healing here would just
    // double the parse pass. The stamp stays stale.
    let root = unique_temp_root();
    rust_fixture(&root);
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    stale_the_stamp(&db);
    db.reconcile_with_options_progress(
        ai::ReconcileOptions {
            batch_size: Some(8),
            max_embedding_chars: ai::DEFAULT_MAX_EMBEDDING_CHARS + 1_000,
            ..Default::default()
        },
        |_| {},
    )
    .unwrap();
    assert_eq!(
        policy_version(&db).as_deref(),
        Some("pre-upgrade"),
        "a non-default-cap reconcile must not certify the DEFAULT-cap column"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn reconcile_plan_classifies_at_the_requested_cap() {
    // The `--plan` preview must bucket by the cap it is GIVEN (matching the reconcile it previews):
    // a ~5 KB generated chunk is SkipGenerated at the default cap but SkipTooLarge at cap=1000.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("gen")).unwrap();
    let big =
        "// generated data line with sufficient length to matter for the cap xxxxx\n".repeat(80);
    fs::write(root.join("gen/bindings.rs"), &big).unwrap();
    let mut config = source_config(root.clone(), Language::Rust);
    config.targets = vec![ResolvedTarget {
        name: "generated".to_string(),
        language: Language::Rust,
        directories: vec![PathBuf::from("gen")],
        include: vec!["gen/".to_string()],
        exclude: Vec::new(),
        kind: TargetKind::Generated,
    }];
    let db = IndexDatabase::rebuild(&config).unwrap();

    let default_plan = db.reconcile_plan().unwrap();
    assert!(
        default_plan.embeddings.skipped_by_policy.contains_key("SkipGenerated"),
        "default cap: the generated chunk is SkipGenerated: {:?}",
        default_plan.embeddings.skipped_by_policy
    );
    let small_plan = db.reconcile_plan_with_cap(1_000).unwrap();
    assert!(
        small_plan.embeddings.skipped_by_policy.contains_key("SkipTooLarge"),
        "cap=1000: the same chunk is now SkipTooLarge: {:?}",
        small_plan.embeddings.skipped_by_policy
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn incremental_edit_keeps_the_certified_column_fresh() {
    // The fast path reads the PERSISTED `chunks.embedding_policy` column, so it stays correct after
    // EDITS only if incremental indexing keeps that column fresh — it routes through the same
    // single chunk-writer and never restamps. Flip a chunk's class via an edit + heal and read
    // the column DIRECTLY: this proves the incremental writer updated it in place, regardless
    // of which summary path would run (a summary assertion alone can't distinguish the column
    // from a recompute — the recompute would see the edited import-only file as low-signal
    // too).
    let root = unique_temp_root();
    // A >= MIN_EMBEDDING_CHARS real fn, so it classifies Embed (a shorter one would be
    // SkipTooSmall).
    rust_fixture(&root);
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();

    // Count a policy in the PERSISTED column, read via the SAME FROM/JOIN the fast path
    // (`policy_skip_summary_from_column`) uses — `chunks` scoped by the `files` view AND joined to
    // `chunk_text` — so it counts exactly the row set the fast path would, not a superset. (Without
    // the `chunk_text` join, a heal that wrote the policy but dropped the chunk_text row would
    // count here yet be excluded by the real summary.) No recompute, no summary.
    let policy_in_column = |value: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM chunks
             JOIN files ON files.id = chunks.file_id
             JOIN chunk_text ON chunk_text.chunk_id = chunks.id
             WHERE chunks.embedding_policy = ?1",
            [value],
            |r| r.get(0),
        )
        .unwrap()
    };

    // Fresh rebuild: the fn chunk is stamped Embed in the column; the fast path is live.
    assert_eq!(policy_version(&db).as_deref(), Some(ai::EMBEDDING_POLICY_VERSION));
    assert!(policy_in_column("Embed") >= 1, "the fn chunk is Embed in the column");
    assert_eq!(policy_in_column("SkipLowSignal"), 0, "no low-signal chunk yet");

    // Edit the file to pure imports (low-signal) and heal it (the incremental single-writer path).
    fs::write(
        root.join("src/code.rs"),
        "use std::collections::HashMap;\nuse std::fmt::Debug;\nuse std::io::Read;\nuse \
         std::sync::Arc;\n",
    )
    .unwrap();
    db.heal_file(std::path::Path::new("src/code.rs")).unwrap();

    // The incremental writer updated the PERSISTED column IN PLACE — the old Embed value is gone
    // and the flipped SkipLowSignal value is written — while leaving the version stamp valid.
    // So the fast path serves the fresh column with no full rebuild.
    assert_eq!(policy_in_column("Embed"), 0, "the old Embed value is gone from the column");
    assert!(
        policy_in_column("SkipLowSignal") >= 1,
        "the heal wrote the flipped policy into the certified column"
    );
    assert_eq!(
        policy_version(&db).as_deref(),
        Some(ai::EMBEDDING_POLICY_VERSION),
        "incremental indexing must not invalidate the stamp"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn absent_stamp_takes_the_recompute_path() {
    // A never-stamped index (a pre-#530 DB, or one never fully rebuilt with this binary) has NO
    // version key — the fast path must fall back to recompute exactly like a stale stamp. Poison
    // the column and DELETE the stamp: the summary must IGNORE the poison and recompute from
    // source.
    let root = unique_temp_root();
    rust_fixture(&root);
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();
    let repo_id = crate::index::schema::active_repo_id(conn).unwrap();
    conn.execute(
        "UPDATE main.chunks SET embedding_policy = 'SkipGenerated'
         WHERE id = (SELECT MIN(id) FROM main.chunks WHERE embedding_policy = 'Embed')",
        [],
    )
    .unwrap();
    crate::index::meta::delete_repo_meta(conn, &repo_id, ai::EMBEDDING_POLICY_VERSION_KEY).unwrap();

    let summary = ai::embedding_policy_skip_summary(conn, ai::DEFAULT_MAX_EMBEDDING_CHARS).unwrap();
    assert_eq!(
        summary.get("SkipGenerated").copied().unwrap_or(0),
        0,
        "an absent stamp forces a recompute that ignores the poisoned column: {summary:?}"
    );
    let _ = fs::remove_dir_all(root);
}
