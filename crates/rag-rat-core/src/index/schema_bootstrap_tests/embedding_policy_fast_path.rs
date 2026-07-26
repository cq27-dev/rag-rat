//! Coverage for the version-stamped fast-path certification gates (#530): the rebuild stamp and its
//! `carried_rows` gate, the fast-path cap gate, the self-heal cap gate, and the plan's cap
//! threading. The end-to-end fast-path/self-heal behaviour lives in `reconcile_embeddings.rs`;
//! these target the individual GATES that decide whether the column may be trusted.

use super::*;
use crate::index::ai;

fn policy_version(db: &IndexDatabase) -> Option<String> {
    let conn = db.storage.connection();
    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    rag_rat_db::meta::repo_meta(conn, &repo_id, ai::EMBEDDING_POLICY_VERSION_KEY).unwrap()
}

fn stale_the_stamp(db: &IndexDatabase) {
    let conn = db.storage.connection();
    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    rag_rat_db::meta::set_repo_meta(
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    assert_eq!(policy_version(&db).as_deref(), Some(ai::EMBEDDING_POLICY_VERSION));
    assert_eq!(
        rag_rat_db::meta::repo_meta(conn, &repo_id, ai::EMBEDDING_POLICY_CAP_KEY)
            .unwrap()
            .as_deref(),
        Some(ai::DEFAULT_MAX_EMBEDDING_CHARS.to_string().as_str()),
        "the stamp records the DEFAULT cap the column was derived at"
    );
    let _ = fs::remove_dir_all(&root);
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
    let _ = fs::remove_dir_all(&root);
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
    let _ = fs::remove_dir_all(&root);
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
    let _ = fs::remove_dir_all(&root);
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
    let _ = fs::remove_dir_all(&root);
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
    let _ = fs::remove_dir_all(&root);
}

/// Shared setup for the embed-path policy-source tests (#725): a rebuild whose fn chunk classifies
/// Embed, the deterministic hash embedder installed, and every Embed row poisoned to
/// `SkipLowSignal` in the column. Whether a reconcile then embeds is exactly the question of which
/// policy source it read: the column (obeys the poison → nothing embedded) or a FromText recompute
/// (ignores it → the fn chunk is embedded).
fn poisoned_embed_fixture(root: &std::path::Path) -> IndexDatabase {
    rust_fixture(root);
    let mut config = source_config(root.to_path_buf(), Language::Rust);
    // Select the hash embedder explicitly: a fresh index adopts the CONFIGURED model (#394), and
    // the default all-MiniLM would leave the reconcile Blocked before it reads any policy.
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    let poisoned = db
        .storage
        .connection()
        .execute(
            "UPDATE main.chunks SET embedding_policy = 'SkipLowSignal'
             WHERE embedding_policy = 'Embed'",
            [],
        )
        .unwrap();
    assert!(poisoned >= 1, "the fixture must stamp at least one Embed chunk to poison");
    db
}

#[test]
fn reconcile_embed_path_reads_the_stamped_policy_column() {
    // The embed path takes each candidate's policy from the CERTIFIED stamped column instead of
    // re-deriving it FromText — the per-candidate tree-sitter re-parse that dominated large
    // reconciles (#725). Under a current stamp at the default cap, the poisoned column must be
    // OBEYED: zero embeddings written. Only the column read produces that outcome — a recompute
    // would classify the fn chunk Embed and write it.
    let root = unique_temp_root();
    let db = poisoned_embed_fixture(&root);
    ai::reset_policy_fromtext_calls();
    let report = db.reconcile(None, Some(8)).unwrap();
    assert_eq!(
        report.embeddings_written, 0,
        "a certified-stamp reconcile must serve the poisoned column; an embedding written means \
         the embed path re-derived policy from text"
    );
    assert_eq!(db.current_embedding_count(HASH_MODEL_ID).unwrap(), 0);
    // Directly: the embed path took the stamped column, never the FromText re-parse — the cost
    // #725 removes. This is the counter half of the fast-path/fallback disagreement.
    assert_eq!(
        ai::policy_fromtext_calls(),
        0,
        "a certified-stamp embed path must not re-classify any candidate FromText"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn stale_stamp_reconcile_heals_then_takes_the_fast_path() {
    // The first reconcile after a classifier/version bump: the stamp is stale, so the self-heal
    // recomputes + re-certifies the column ONCE up front. The embed loop must then re-derive its
    // certification from the HEALED stamp and read the column — NOT re-parse every candidate
    // FromText for the whole run (the regression Codex caught: `stamped_policy` captured before the
    // heal stayed false). Prove it with the counter: zero FromText calls despite the stale start,
    // and the stamp ends certified. (The poison is erased by the heal — the fn chunk classifies
    // Embed from source — so it still embeds; the counter, not the output, is the fast-path proof.)
    let root = unique_temp_root();
    let db = poisoned_embed_fixture(&root);
    stale_the_stamp(&db);
    ai::reset_policy_fromtext_calls();
    let report = db.reconcile(None, Some(8)).unwrap();
    assert_eq!(
        ai::policy_fromtext_calls(),
        0,
        "after the heal re-certifies the stamp, the embed loop must read the column, not \
         re-parse: {report:?}"
    );
    assert_eq!(
        policy_version(&db).as_deref(),
        Some(ai::EMBEDDING_POLICY_VERSION),
        "the stale-stamp reconcile heals and re-certifies"
    );
    assert!(report.embeddings_written >= 1, "the healed Embed chunk still embeds: {report:?}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reconcile_plan_classifies_from_the_stamped_column_like_the_embed_path() {
    // `reconcile --plan` must preview exactly what `reconcile` will do — so under a certified stamp
    // it classifies candidates from the stamped column, not FromText (which can legitimately
    // disagree on a chunk slicing a long comment/string). Poison every Embed chunk to SkipLowSignal
    // under a current stamp: the plan must report zero eligible work (it read the column) and take
    // no FromText re-parse, matching the embed path on the same index. A FromText recompute would
    // classify the fn chunk Embed and count it missing.
    let root = unique_temp_root();
    let db = poisoned_embed_fixture(&root);
    ai::reset_policy_fromtext_calls();
    let plan = db.reconcile_plan().unwrap();
    assert_eq!(
        ai::policy_fromtext_calls(),
        0,
        "a certified-stamp plan must read the column, not re-classify FromText"
    );
    assert_eq!(
        plan.embeddings.missing, 0,
        "the plan reads the poisoned (SkipLowSignal) column, so nothing is eligible — matching \
         the embed path: {:?}",
        plan.embeddings
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn self_heal_refreshes_stale_priorities_under_an_unchanged_policy() {
    // The stamp certifies policy AND priority (the embed path trusts both, #725), and a classifier
    // change can move priority while the policy name stays the same. A heal that rewrote only
    // `embedding_policy` would re-certify stale priorities — so the heal must stage on either
    // column differing and write both back. Poison the fn chunk's priority (policy untouched),
    // stale the stamp, and let the default-cap reconcile self-heal: the priority must be restored
    // and the stamp current again.
    let root = unique_temp_root();
    rust_fixture(&root);
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();
    let poisoned = conn
        .execute(
            "UPDATE main.chunks SET embedding_priority = 7 WHERE embedding_policy = 'Embed'",
            [],
        )
        .unwrap();
    assert!(poisoned >= 1, "the fixture must have an Embed chunk whose priority can be poisoned");
    stale_the_stamp(&db);

    db.reconcile_with_options_progress(
        ai::ReconcileOptions { batch_size: Some(8), ..Default::default() },
        |_| {},
    )
    .unwrap();

    let still_poisoned: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks WHERE embedding_priority = 7", [], |r| r.get(0))
        .unwrap();
    assert_eq!(still_poisoned, 0, "the heal must recompute priorities, not just policy names");
    assert_eq!(
        policy_version(&db).as_deref(),
        Some(ai::EMBEDDING_POLICY_VERSION),
        "the heal re-certifies after writing BOTH columns"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reconcile_embed_path_recomputes_at_a_non_default_cap() {
    // A CURRENT stamp at a NON-DEFAULT cap also fails certification (the column is stamped at the
    // DEFAULT cap, and a different cap re-buckets SkipTooLarge), and the self-heal is skipped at a
    // non-default cap too — so the embed path genuinely re-derives FromText and ignores the poison.
    // This is the fallback that must DISAGREE with the certified fast path: the FromText counter is
    // positive here, zero in `reconcile_embed_path_reads_the_stamped_policy_column`.
    let root = unique_temp_root();
    let db = poisoned_embed_fixture(&root);
    ai::reset_policy_fromtext_calls();
    let report = db
        .reconcile_with_options_progress(
            ai::ReconcileOptions {
                batch_size: Some(8),
                max_embedding_chars: ai::DEFAULT_MAX_EMBEDDING_CHARS + 1_000,
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
    assert!(
        report.embeddings_written >= 1,
        "a non-default-cap reconcile must not trust the DEFAULT-cap column: {report:?}"
    );
    assert!(
        ai::policy_fromtext_calls() > 0,
        "the uncertified embed path must re-derive policy FromText (the fallback the fast path \
         avoids)"
    );
    let _ = fs::remove_dir_all(&root);
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn).unwrap();
    conn.execute(
        "UPDATE main.chunks SET embedding_policy = 'SkipGenerated'
         WHERE id = (SELECT MIN(id) FROM main.chunks WHERE embedding_policy = 'Embed')",
        [],
    )
    .unwrap();
    rag_rat_db::meta::delete_repo_meta(conn, &repo_id, ai::EMBEDDING_POLICY_VERSION_KEY).unwrap();

    let summary = ai::embedding_policy_skip_summary(conn, ai::DEFAULT_MAX_EMBEDDING_CHARS).unwrap();
    assert_eq!(
        summary.get("SkipGenerated").copied().unwrap_or(0),
        0,
        "an absent stamp forces a recompute that ignores the poisoned column: {summary:?}"
    );
    let _ = fs::remove_dir_all(&root);
}
