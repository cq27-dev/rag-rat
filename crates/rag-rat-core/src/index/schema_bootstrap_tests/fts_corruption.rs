//! #582: FTS5 shadow-table corruption is invisible to `PRAGMA integrity_check` (and, for
//! contentless docsize damage, even to FTS5's own `'integrity-check'`) — only a query that
//! actually EXECUTES rank/bm25 fails, with a bare "database disk image is malformed". These pin
//! the query-layer self-heal (rebuild the FTS mirrors from durable sources through the SAME
//! connection, retry once) and `heal_index`'s ranked-probe detection.

use super::*;

/// Mirror of the real incident's corruption class (#582): drop the mirror's docsize rows, so a
/// RANKED match fails SQLITE_CORRUPT (bm25 requires a docsize row per matched rowid) while an
/// unranked COUNT over the same MATCH passes (the ORDER BY is optimized away). This is the ONLY
/// deterministic corruption shape for a small index: garbaging or zeroing `_data` pages degrades
/// to a silent empty index (no match, no error) — the exact trap that burned the incident's
/// wrong theories.
fn corrupt_fts_docsize(db: &IndexDatabase, fts_table: &str) {
    db.storage.connection().execute(&format!("DELETE FROM {fts_table}_docsize"), []).unwrap();
}

fn ranked_chunk_probe(db: &IndexDatabase, term: &str) -> anyhow::Result<Option<i64>> {
    let out = db
        .storage
        .connection()
        .query_row(
            "SELECT rowid FROM chunk_fts WHERE chunk_fts MATCH ?1 ORDER BY rank LIMIT 1",
            [term],
            |row| row.get::<_, i64>(0),
        )
        .map(Some);
    match out {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

#[test]
fn ranked_chunk_search_self_heals_docsize_corruption() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn corruption_witness_alpha() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    corrupt_fts_docsize(&db, "chunk_fts");
    // Sanity, pinning the #582 diagnosis: the RANKED probe fails "malformed"; the same MATCH
    // without rank passes (so integrity-check-style probes would miss this entirely).
    let ranked = ranked_chunk_probe(&db, "corruption_witness_alpha");
    assert!(
        ranked.is_err_and(|e| e.to_string().contains("malformed")),
        "docsize corruption must fail the ranked probe"
    );
    let unranked: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM chunk_fts WHERE chunk_fts MATCH 'corruption_witness_alpha'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unranked, 1, "the unranked MATCH still passes — corruption is rank-only");

    // The public ranked search path self-heals and answers.
    let hits = db.search("corruption_witness_alpha", 10, false).expect("search self-heals");
    assert!(!hits.is_empty(), "the healed index serves the hit");
    assert!(
        ranked_chunk_probe(&db, "corruption_witness_alpha").is_ok(),
        "the ranked probe passes after the self-heal"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn memory_search_self_heals_fts_corruption() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn memoried_fn() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "corruption witness memory".to_string(),
        body: "Body of the corruption witness memory.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: dir_bind_target(Some("src".to_string())),
    })
    .unwrap();

    corrupt_fts_docsize(&db, "repo_memory_fts");
    // Sanity: the ranked memory MATCH is broken before the heal.
    let direct = db.storage.connection().query_row(
        "SELECT rowid FROM repo_memory_fts WHERE repo_memory_fts MATCH 'witness' ORDER BY rank \
         LIMIT 1",
        [],
        |row| row.get::<_, i64>(0),
    );
    assert!(
        matches!(&direct, Err(e) if e.to_string().contains("malformed")),
        "the corrupted memory FTS must fail before the heal: {direct:?}"
    );

    let hits = db
        .memory_search("witness", 10, crate::config::MemorySurface::Full)
        .expect("memory_search self-heals");
    assert_eq!(hits.len(), 1, "the healed FTS serves the memory");
    assert_eq!(hits[0].title, "corruption witness memory");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn heal_index_probes_and_repairs_corrupt_fts() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn heal_probe_witness() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: "heal probe memory".to_string(),
        body: "Body of the heal probe memory.".to_string(),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: dir_bind_target(Some("src".to_string())),
    })
    .unwrap();

    corrupt_fts_docsize(&db, "chunk_fts");
    corrupt_fts_docsize(&db, "repo_memory_fts");

    let report = db.heal_index(None).unwrap();
    assert!(
        report.fts_healed.iter().any(|t| t == "chunk_fts"),
        "the ranked probe catches the docsize corruption (an integrity-check-style probe \
         wouldn't): {report:?}"
    );
    assert!(
        report.fts_healed.iter().any(|t| t == "repo_memory_fts"),
        "the memory FTS corruption is caught and healed: {report:?}"
    );
    assert!(
        ranked_chunk_probe(&db, "heal_probe_witness").is_ok(),
        "ranked chunk queries pass after heal_index"
    );

    // A second heal on the now-clean index rebuilds nothing.
    let clean = db.heal_index(None).unwrap();
    assert!(clean.fts_healed.is_empty(), "no false-positive probe on a clean index: {clean:?}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_search_self_heals_commit_fts_corruption() {
    // #582 review: commit_fts is an independent ranked path (it was one of the initially
    // uncovered seams) — same corruption shape, same heal-and-retry contract.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn committed_fn() {}\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "corruption witness commit subject"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    corrupt_fts_docsize(&db, "commit_fts");
    let direct = db.storage.connection().query_row(
        "SELECT rowid FROM commit_fts WHERE commit_fts MATCH 'witness' ORDER BY rank LIMIT 1",
        [],
        |row| row.get::<_, i64>(0),
    );
    assert!(
        matches!(&direct, Err(e) if e.to_string().contains("malformed")),
        "the corrupted commit FTS must fail before the heal: {direct:?}"
    );

    let hits = db.commit_search("witness", 10).expect("commit_search self-heals");
    assert!(!hits.is_empty(), "the healed commit index serves the hit");

    let _ = fs::remove_dir_all(&root);
}
