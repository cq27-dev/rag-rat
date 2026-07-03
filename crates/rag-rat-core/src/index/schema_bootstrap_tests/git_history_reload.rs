use super::*;

#[test]
fn git_history_reloads_after_a_new_commit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    insert_sentinel_commit(&db);
    drop(db);

    // A real new commit moves HEAD → the gate must reload, wiping the sentinel.
    fs::write(root.join("docs/search.md"), "# Title\ngamma token\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add gamma docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a new commit must force a reload");
    assert_eq!(db.commit_search("gamma", 10).unwrap().len(), 1, "new commit is indexed");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_reloads_after_a_history_rewrite() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    let before = db.status(&config.database).unwrap().git_history.commit_count;
    insert_sentinel_commit(&db);
    drop(db);

    // Amend rewrites the tip to a new sha WITHOUT adding a commit — a non-fast-forward rewrite,
    // like the squash that motivated the gate. HEAD's content-addressed sha changes → reload.
    run_git(&root, &["commit", "--amend", "-m", "Refresh delta docs"]);

    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a history rewrite must force a reload");
    let status = db.status(&config.database).unwrap();
    assert_eq!(status.git_history.commit_count, before, "amend does not change the commit count");
    assert_eq!(db.commit_search("delta", 10).unwrap().len(), 1, "rewritten subject is indexed");
    assert_eq!(db.commit_search("beta", 10).unwrap().len(), 0, "old subject is gone after rewrite");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn git_history_reload_is_not_skipped_on_a_shallow_clone() {
    let origin = unique_temp_root();
    let _ = fs::remove_dir_all(&origin);
    let _ = git_history_test_config(&origin); // origin repo with two commits

    let shallow = unique_temp_root();
    let _ = fs::remove_dir_all(&shallow);
    // Local clones ignore --depth unless the source is a file:// URL; use one so the clone is
    // genuinely shallow.
    run_git(&std::env::temp_dir(), &[
        "clone",
        "--depth",
        "1",
        &format!("file://{}", origin.display()),
        shallow.to_str().unwrap(),
    ]);
    let config = rag_rat_config(&shallow);
    // A depth-cut shallow clone cannot derive a PORTABLE identity (its root is unreachable), but it
    // no longer fails: `resolve_repo_identity` derives a deterministic `local:`-prefixed LocalOnly
    // id from the shallow boundary and proceeds. So rebuild/open/incremental adopt it WITHOUT a
    // pin — exactly the path CI's shallow fixtures exercise — and this test keeps its subject
    // (the history reload gate) while also covering LocalOnly adoption end-to-end.

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert!(
        db.active_repo_id.starts_with("local:"),
        "a cut shallow clone adopts under a LocalOnly id, got {}",
        db.active_repo_id
    );
    insert_sentinel_commit(&db);
    drop(db);

    // HEAD is unchanged, but a shallow clone can be deepened without moving HEAD, so its history
    // is not pinned by the HEAD sha — the gate must NOT skip. It reloads and wipes the sentinel.
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(sentinel_commit_count(&db), 0, "a shallow clone must never skip the reload");

    let _ = fs::remove_dir_all(origin);
    let _ = fs::remove_dir_all(shallow);
}

#[test]
fn idle_discover_sweep_does_not_rewrite_indexed_at_ms() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);

    let db = IndexDatabase::rebuild(&config).unwrap();
    // Stamp a non-numeric sentinel so any spurious timestamp write is unmistakable. Under the
    // ACTIVE repo id (a real git fixture is adopted, so the `__unassigned__` placeholder repos row
    // is gone and `repo_meta` under it would trip the FK).
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES(?1, 'indexed_at_ms', 'SENTINEL')
             ON CONFLICT(repo_id, key) DO UPDATE SET value = 'SENTINEL'",
            [&db.active_repo_id],
        )
        .unwrap();
    drop(db);

    // A discover sweep over an unchanged tree must not mutate the DB — the sentinel survives
    // (no timestamp-only write + COMMIT). See issue #63.
    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_eq!(
        read_meta(&db, "indexed_at_ms").as_deref(),
        Some("SENTINEL"),
        "an unchanged discover sweep must not rewrite indexed_at_ms"
    );
    drop(db);

    // A real change must persist — the sweep writes a fresh timestamp, clearing the sentinel.
    fs::write(root.join("docs/added.md"), "# Added\nfresh content\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "Add a doc"]);
    let db = IndexDatabase::index_discover(&config).unwrap();
    assert_ne!(
        read_meta(&db, "indexed_at_ms").as_deref(),
        Some("SENTINEL"),
        "a sweep that indexes a new file must update indexed_at_ms"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn index_discover_reporting_flags_content_changes() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let config = git_history_test_config(&root);
    IndexDatabase::rebuild(&config).unwrap();

    // No change → reports false, so the watch loop skips the reconcile / memory-validate tail.
    let (_db, changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(!changed, "an unchanged discover sweep must report no content change");

    // A new file on disk → reports true.
    fs::write(root.join("docs/extra.md"), "# Extra\nbody text\n").unwrap();
    let (_db, changed) = IndexDatabase::index_discover_reporting(&config).unwrap();
    assert!(changed, "a discover sweep that indexes a new file must report a content change");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn discover_relanguages_h_when_binding_changes_c_to_cpp() {
    // A `.h` indexed under a `c` binding, then re-discovered under a `cpp` binding with IDENTICAL
    // content, must be reindexed as C++ — discovery treats (language, kind) drift as a change, not
    // just sha drift. Without this the `.h`→C++ upgrade would never take effect on an existing
    // index (the sha is unchanged) until `--full`.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.h"), "class X { public: void f(); };\n").unwrap();

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::C)).unwrap();
    let lang: String = db
        .storage
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/lib.h'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lang, "c", "indexed as C under the c binding");
    drop(db);

    let (db, changed) =
        IndexDatabase::index_discover_reporting(&source_config(root.clone(), Language::Cpp))
            .unwrap();
    assert!(changed, "re-languaging a .h with unchanged content must report a change");
    let lang: String = db
        .storage
        .connection()
        .query_row("SELECT language FROM files WHERE path = 'src/lib.h'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lang, "cpp", "the .h must be reindexed as C++ after the binding change");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_rust_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
use crate::worker::Worker;
mod worker;

trait Service {
    fn serve(&self);
}

struct Worker;

impl Service for Worker {
    fn serve(&self) {
        helper();
    }
}

fn helper() {}

fn caller() {
    helper();
    Worker.serve();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "caller", "helper", "calls_name", "Syntactic");
    assert_edge(&db, "Worker", "Service", "implements", "Syntactic");
    assert_edge(&db, "src/lib.rs", "worker", "imports", "Syntactic");
    let callers = db.find_callers("helper", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("caller"))
                && edge.edge_kind == "calls_name"
        }),
        "helper callers: {callers:?}"
    );

    let _ = fs::remove_dir_all(root);
}
