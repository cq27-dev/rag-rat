//! `rag-rat rm` driven from a LINKED WORKTREE, against a database shared with the main checkout and
//! with an unrelated sibling repo.
//!
//! The table-sync stream directory is keyed by `(repo_id, account_id, scope_id)`, and the entry log
//! it scopes carries no `repo_id` at all — the purge reaches that log only through the captured
//! directory rows (#1004). Neither key has a checkout dimension, and this pins that rather than
//! assuming it: both checkouts of a repo must register under ONE `repo_id`, a removal resolved from
//! the linked worktree must take the whole repo's log, and a sibling repo in the same database must
//! keep its own — which no `repo_id` predicate could have protected, since the log has none.

use super::*;

/// A git checkout with one committed Rust file.
fn checkout(marker: &str) -> ScratchRoot {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), format!("pub fn {marker}() {{}}\n")).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    root
}

/// Index `root` into the SHARED `database`, returning the repo id it registered under.
fn index_at(root: &Path, database: &Path) -> String {
    let mut config = source_config(root.to_path_buf(), Language::Rust);
    config.database = database.to_path_buf();
    IndexDatabase::rebuild(&config).unwrap().active_repo_id.clone()
}

/// One stream directory row for `repo_id` plus one entry on its stream; returns the stream id.
fn seed_stream(conn: &rusqlite::Connection, repo_id: &str, seed: u8) -> Vec<u8> {
    let stream_id = vec![seed; 32];
    conn.execute(
        "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, scope_id) VALUES (?1, ?2, \
         ?3, 'demo/1')",
        rusqlite::params![stream_id, repo_id, vec![seed; 32]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO table_sync_entries(entry_hash, stream_id, device_fingerprint, lamport, \
         prev_hash, signed_bytes, received_at_ms) VALUES (?1, ?2, ?3, 1, NULL, ?4, 0)",
        rusqlite::params![vec![seed; 32], stream_id, vec![seed; 32], vec![seed; 8]],
    )
    .unwrap();
    stream_id
}

fn entries_on(conn: &rusqlite::Connection, stream_id: &[u8]) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM table_sync_entries WHERE stream_id = ?1",
        rusqlite::params![stream_id],
        |row| row.get(0),
    )
    .unwrap()
}

fn directory_rows(conn: &rusqlite::Connection, repo_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM table_sync_streams WHERE repo_id = ?1",
        rusqlite::params![repo_id],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn rm_from_a_linked_worktree_purges_the_shared_stream_log_and_spares_the_sibling_repo() {
    let main = checkout("main_anchor");
    let store = unique_temp_root();
    let database = store.join("shared.sqlite");
    let repo_id = index_at(&main, &database);

    // A linked worktree of the SAME repo, indexed into the SAME database.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    let linked_repo_id = index_at(&linked, &database);
    assert_eq!(
        linked_repo_id, repo_id,
        "a linked worktree indexes under the SAME repo id — the property that makes a purge keyed \
         by `repo_id` alone reach every checkout",
    );

    // An unrelated repo in the same database: the isolation half.
    let sibling = checkout("sibling_anchor");
    let sibling_repo_id = index_at(&sibling, &database);
    assert_ne!(sibling_repo_id, repo_id, "the sibling is a distinct repo in the shared store");

    // A BARE connection, exactly as the production purge uses: a scoped `IndexDatabase` connection
    // installs a `temp.files` view that shadows the base tables.
    let storage = IndexConnection::open(&database).unwrap();
    let conn = storage.connection();
    let shared_stream = seed_stream(conn, &repo_id, 0xaa);
    let sibling_stream = seed_stream(conn, &sibling_repo_id, 0xbb);

    let roots: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_roots WHERE repo_id = ?1",
            rusqlite::params![&repo_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(roots >= 2, "both checkouts registered a root under the one repo id, saw {roots}");

    // ACTIVE-CHECKOUT SCOPE: the removal is resolved from the LINKED worktree, not the main one.
    let resolved = crate::index::remove::resolve_removable_repo(conn, &linked)
        .unwrap()
        .expect("the linked worktree resolves to a registered repo");
    assert_eq!(resolved.repo_id, repo_id, "and it resolves to the shared repo, not a new one");

    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    schema::purge_repo_rows(&tx, &resolved.repo_id).unwrap();
    tx.commit().unwrap();

    assert_eq!(directory_rows(conn, &repo_id), 0, "the shared repo's stream directory is swept");
    assert_eq!(
        entries_on(conn, &shared_stream),
        0,
        "and the stream-keyed entry log goes with the directory that placed it, whichever \
         checkout drove the removal",
    );

    // SIBLING PRESERVATION: the other repo in the shared store keeps both halves. Its entries carry
    // no `repo_id`, so only the captured stream ids could have spared them.
    assert_eq!(directory_rows(conn, &sibling_repo_id), 1, "the sibling's directory row survives");
    assert_eq!(entries_on(conn, &sibling_stream), 1, "and so does its entry log");

    let _ = fs::remove_dir_all(&linked);
}
