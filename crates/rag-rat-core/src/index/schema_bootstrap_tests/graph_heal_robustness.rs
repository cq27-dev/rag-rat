//! `ensure_graph_index_current` runs on the OPEN path, so anything it treats as fatal wedges the
//! database for every later open too — the heal is retried and re-fails forever, and no CLI or MCP
//! call can get in to repair it. These tests pin the states that must NOT be fatal.
//!
//! All three arise on ordinary indexes: a deletion tombstone (every repo that ever removed a
//! file), a row whose path is absent from this checkout (the heal repopulates the repo's whole
//! generation, including sibling commits/worktrees), and a Rust file above the parse limit.

use super::*;

/// Stage an index that owes BOTH heals: the graph-version bump (which drives the reindex loop)
/// and the logical-key bump (which turns on the Rust scope refresh).
fn owe_both_heals(db: &IndexDatabase) {
    db.set_repo_meta("graph_index_version", "0").unwrap();
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();
}

fn indexed_root(files: &[(&str, &str)]) -> (ScratchRoot, rag_rat_base::config::Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    for (name, body) in files {
        fs::write(root.join("src").join(name), body).unwrap();
    }
    let config = source_config(root.to_path_buf(), Language::Rust);
    (root, config)
}

/// A `kind='deleted'` tombstone carries `language='unknown'` (`mark_file_deleted`). Neither token
/// parses into `Language`/`TargetKind`, so a heal that walks raw `main.files` rows without
/// excluding tombstones fails at the FIRST one — on every open, permanently. Any repo that has
/// ever deleted an indexed file holds these rows.
#[test]
fn a_deletion_tombstone_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Delete an indexed file and let the incremental pass write its tombstone, exactly as a
    // normal edit-and-reindex cycle does.
    fs::write(root.join("src/gone.rs"), "pub fn vanishing() {}\n").unwrap();
    let db = {
        drop(db);
        IndexDatabase::index_discover(&config).unwrap()
    };
    fs::remove_file(root.join("src/gone.rs")).unwrap();
    drop(db);
    let db = IndexDatabase::index_discover(&config).unwrap();
    let tombstones: i64 = db
        .storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE kind = 'deleted'", [], |row| row.get(0))
        .unwrap();
    assert!(tombstones > 0, "the delete-and-reindex cycle must leave a tombstone to test against");

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("a tombstone row must not fail the heal");
    assert_eq!(
        db.repo_meta("graph_index_version").unwrap().as_deref(),
        Some(GRAPH_INDEX_VERSION),
        "the heal ran to completion and stamped its version"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The heal repopulates the repo's whole generation, so it walks rows for paths that need not
/// exist under THIS root (a sibling commit/worktree scope) — and a row can simply outlive its file
/// between an edit and the next index pass. An unreadable path is an expected outcome of that
/// repopulation, not a failure.
#[test]
fn a_file_row_without_a_file_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A live row (not a tombstone) whose path has no file behind it.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated,
                 indexed_at_ms, indexed_revision, commit_sha, worktree_id, repo_id, generation,
                 has_test_code)
             SELECT 'src/absent.rs', 'rust', 'source', '', 0, 0, 0, '', commit_sha, worktree_id,
                    repo_id, generation, 0
             FROM main.files LIMIT 1",
            [],
        )
        .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("an unreadable path must not fail the heal");

    let _ = fs::remove_dir_all(&root);
}

/// Above `MAX_GRAPH_PARSE_BYTES` the indexer persists no symbols at all (the chunker declines at
/// the same bound), so there is nothing for the scope refresh to certify. Treating the file as an
/// un-certifiable heal blocker would fail the open over a file the index never parsed.
#[test]
fn an_oversized_rust_file_does_not_wedge_the_graph_heal() {
    let filler = "pub fn pad() {}\n".repeat(40_000);
    assert!(filler.len() > edges::MAX_GRAPH_PARSE_BYTES, "the fixture must exceed the parse limit");
    let (root, config) = indexed_root(&[
        ("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n"),
        ("huge.rs", filler.as_str()),
    ]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("an oversized Rust file must not fail the heal");

    let _ = fs::remove_dir_all(&root);
}

/// A file edited since it was indexed shifts every byte offset after the edit, so the span-matched
/// scope refresh reaches only part of its symbols. That is the ordinary state between an edit and
/// the next watcher pass — the heal reports the shortfall and moves on, and the file re-lands its
/// scope when it is next indexed.
#[test]
fn a_file_edited_since_indexing_does_not_wedge_the_graph_heal() {
    let (root, config) =
        indexed_root(&[("lib.rs", "pub struct Alpha;\nimpl Alpha { pub fn run(&self) {} }\n")]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Prepend a symbol WITHOUT reindexing: every stored span is now stale by the same offset.
    fs::write(
        root.join("src/lib.rs"),
        "pub fn inserted_ahead_of_everything() {}\npub struct Alpha;\nimpl Alpha { pub fn \
         run(&self) {} }\n",
    )
    .unwrap();

    owe_both_heals(&db);
    db.ensure_graph_index_current().expect("a file edited since indexing must not fail the heal");

    let _ = fs::remove_dir_all(&root);
}
