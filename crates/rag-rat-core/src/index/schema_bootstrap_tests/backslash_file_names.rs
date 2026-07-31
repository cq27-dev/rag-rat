//! A literal `\` in a Unix file name is part of the NAME, not a separator (#1032).
//!
//! `src/foo\bar.rs` and `src/foo/bar.rs` are two different files on Unix. The index rendered both
//! through a blanket backslash rewrite, so both landed on the `files.path` `src/foo/bar.rs` — one
//! row, one set of chunks, one symbol `qualified_name` namespace — and the second file indexed
//! silently overwrote the first. Everything keyed on `files.path` inherited that collision.
//!
//! The whole module is Unix-only rather than each test: Windows forbids `\` in a file name, so the
//! fixture cannot be created there at all, and gating per-item would leave the helpers below as
//! dead code on the Windows leg.

use super::*;

/// The nested file and the backslash-named file, as they must be spelled in `files.path`.
const NESTED_PATH: &str = "src/foo/bar.rs";
const BACKSLASH_PATH: &str = "src/foo\\bar.rs";

/// Write the two files under `root`: a genuinely nested `src/foo/bar.rs` and a sibling whose NAME
/// is `foo\bar.rs`. Their symbol names differ so a collision is visible as a missing symbol rather
/// than as identical content.
fn write_colliding_pair(root: &Path, nested_body: &str, backslash_body: &str) {
    fs::create_dir_all(root.join("src/foo")).unwrap();
    fs::write(root.join("src/foo/bar.rs"), nested_body).unwrap();
    fs::write(root.join("src").join("foo\\bar.rs"), backslash_body).unwrap();
}

/// Every `files.path` in the active scope, sorted.
fn scoped_paths(db: &IndexDatabase) -> Vec<String> {
    let conn = db.storage.connection();
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path").unwrap();
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
    rows.filter_map(Result::ok).collect()
}

/// Symbol `qualified_name`s in the active scope, sorted. Read through the `name_strings` intern
/// table, which is where the column lives post-V028.
fn scoped_qualified_names(db: &IndexDatabase) -> Vec<String> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT qn.value FROM symbols s
             JOIN files f ON f.id = s.file_id
             JOIN name_strings qn ON qn.id = s.qualified_name_id
             ORDER BY qn.value",
        )
        .unwrap();
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
    rows.filter_map(Result::ok).collect()
}

/// Chunk ids whose file row has `path` in the active scope.
fn scoped_chunk_ids(db: &IndexDatabase, path: &str) -> Vec<i64> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
             WHERE f.path = ?1 ORDER BY c.id",
        )
        .unwrap();
    let rows = stmt.query_map([path], |row| row.get::<_, i64>(0)).unwrap();
    rows.filter_map(Result::ok).collect()
}

#[test]
fn a_backslash_named_file_gets_its_own_row_chunks_and_qualified_names() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    write_colliding_pair(
        &root,
        "pub fn nested_fn() -> i32 { 1 }\n",
        "pub fn backslash_fn() -> i32 { 2 }\n",
    );
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // One row EACH. Before the fix the backslash file rendered as `src/foo/bar.rs` too, so the
    // second file to be walked replaced the first and only one row survived.
    assert_eq!(
        scoped_paths(&db),
        vec![NESTED_PATH.to_string(), BACKSLASH_PATH.to_string()],
        "the nested file and the backslash-named file are two rows"
    );

    // Chunks are keyed to the file row, so a collision would leave one of the two files with none.
    let nested_chunks = scoped_chunk_ids(&db, NESTED_PATH);
    let backslash_chunks = scoped_chunk_ids(&db, BACKSLASH_PATH);
    assert!(!nested_chunks.is_empty(), "the nested file has chunks");
    assert!(!backslash_chunks.is_empty(), "the backslash-named file has chunks");
    assert!(
        nested_chunks.iter().all(|id| !backslash_chunks.contains(id)),
        "the two files must not share chunks: {nested_chunks:?} vs {backslash_chunks:?}"
    );

    // `qualified_name` is `path::name` — the human-readable symbol identity every graph and MCP
    // surface round-trips — so the collision corrupted symbol identity, not just a row.
    assert_eq!(
        scoped_qualified_names(&db),
        vec![format!("{NESTED_PATH}::nested_fn"), format!("{BACKSLASH_PATH}::backslash_fn")],
        "each file names its own symbol"
    );
}

/// A memory's stored binding path is the CALLER'S OWN string — `resolve_binding` copies
/// `bind.path` verbatim, and no writer ever put a walked path through the rendering seam on its way
/// into `repo_memory_bindings` — so the rendering change does not move it and there is nothing for
/// the conversion to rekey. What the change does is make the binding RESOLVE: pre-fix the two files
/// shared one `files.path`, so a memory bound to the backslash-named one answered for whichever row
/// survived the collision.
///
/// Pinned because the alternative — quarantining every path-bearing binding on upgrade — would
/// discard correct anchors wholesale, and because the old rendering was lossy: a stored
/// `src/foo/bar.rs` cannot be told apart from a collapsed `src/foo\bar.rs`, so no rekey is
/// derivable even in principle.
#[test]
fn a_path_bound_memory_resolves_to_the_file_it_names_not_its_slash_sibling() {
    use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    write_colliding_pair(
        &root,
        "pub fn nested_fn() -> i32 { 1 }\n",
        "pub fn backslash_fn() -> i32 { 2 }\n",
    );
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let bind_to = |path: &str, title: &str| {
        crate::memory_write::create_memory(db.storage.connection(), RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: RepoMemoryBindTarget {
                path: Some(path.to_string()),
                ..RepoMemoryBindTarget::default()
            },
        })
        .unwrap()
    };
    let backslash = bind_to(BACKSLASH_PATH, "about the backslash-named file");
    let nested = bind_to(NESTED_PATH, "about the nested file");
    assert_eq!(
        backslash.memory.bindings[0].anchor_status, "current",
        "the binding resolves against a real row, not a collapsed sibling's"
    );

    let titles = |path: &str| {
        let mut found: Vec<String> =
            rag_rat_query::memory::memories_for_path(db.storage.connection(), path, 10)
                .unwrap()
                .into_iter()
                .map(|memory| memory.title)
                .collect();
        found.sort();
        found
    };
    assert_eq!(
        titles(BACKSLASH_PATH),
        vec![backslash.memory.title.clone()],
        "the backslash-named file surfaces only its own memory"
    );
    assert_eq!(
        titles(NESTED_PATH),
        vec![nested.memory.title.clone()],
        "and the nested sibling surfaces only its own — neither file inherits the other's"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_worktree_overlay_shadows_only_the_backslash_named_sibling() {
    // The worktree-correctness leg: main checkout and linked worktree share ONE database, and the
    // branch edits ONLY the backslash-named file. Under the collision the two files were one row,
    // so an overlay of either shadowed both — the sibling could not be preserved.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    write_colliding_pair(
        &main,
        "pub fn nested_base() -> i32 { 1 }\n",
        "pub fn backslash_base() -> i32 { 2 }\n",
    );
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(names_in_scope(&db, NESTED_PATH), vec!["nested_base".to_string()]);
    assert_eq!(names_in_scope(&db, BACKSLASH_PATH), vec!["backslash_base".to_string()]);

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src").join("foo\\bar.rs"), "pub fn backslash_branch() -> i32 { 3 }\n")
        .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch edits the backslash-named file"]);

    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the branch's edited file is indexed as an overlay row");

    // ACTIVE-CHECKOUT SCOPE: the overlay shadows the backslash-named file...
    assert_eq!(
        names_in_scope(&db, BACKSLASH_PATH),
        vec!["backslash_branch".to_string()],
        "the overlay scope sees the branch's backslash-named file"
    );
    // ...and leaves its untouched nested SIBLING alone.
    assert_eq!(
        names_in_scope(&db, NESTED_PATH),
        vec!["nested_base".to_string()],
        "the nested sibling is not shadowed by an edit to the backslash-named file"
    );

    // SIBLING-CHECKOUT PRESERVATION: back in the main checkout's scope, both files still read as
    // the base committed them.
    set_base_scope(&mut db, &main);
    assert_eq!(
        names_in_scope(&db, BACKSLASH_PATH),
        vec!["backslash_base".to_string()],
        "the base scope keeps its own backslash-named file, unshadowed by the branch"
    );
    assert_eq!(names_in_scope(&db, NESTED_PATH), vec!["nested_base".to_string()]);

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
