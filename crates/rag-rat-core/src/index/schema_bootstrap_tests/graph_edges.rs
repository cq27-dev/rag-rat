use super::*;

#[test]
fn calls_name_edge_stores_callee_identifier_byte_range_not_whole_call() {
    // #67: SCIP occurrences key on the callee identifier token, but source_start_byte covers the
    // whole call_expression. The new callee_*_byte columns must span exactly the identifier:
    //   `foo`   in `foo(a, b)`     (plain call)
    //   `method` in `obj.method(x)` (final segment of a method call)
    //   `c`     in `a::b::c()`     (final segment of a path call)
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let source = r#"
fn foo(a: u32, b: u32) -> u32 {
    a + b
}

mod nested {
    pub mod inner {
        pub fn c() {}
    }
}

struct Obj;

impl Obj {
    fn method(&self, _x: u32) {}
}

fn driver() {
    let obj = Obj;
    foo(1, 2);
    obj.method(3);
    nested::inner::c();
}
"#;
    fs::write(root.join("src/lib.rs"), source).unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let assert_callee = |to: &str, expected: &str| {
        let (start, end) = callee_byte_range(&db, "driver", to, "calls_name")
            .unwrap_or_else(|| panic!("no callee range for driver -> {to}"));
        let (start, end) = (start as usize, end as usize);
        assert_eq!(
            &source[start..end],
            expected,
            "callee range for driver -> {to} should be exactly `{expected}`, got `{}`",
            &source[start..end]
        );
        // It must NOT be the whole call expression (that would include the `(`).
        assert!(
            !source[start..end].contains('('),
            "callee range for driver -> {to} must not span the whole call: `{}`",
            &source[start..end]
        );
    };

    assert_callee("foo", "foo");
    assert_callee("method", "method");
    assert_callee("c", "c");

    // A `contains` edge (parent symbol -> child symbol) has no callee identifier → NULL.
    let contains = callee_byte_range(&db, "Obj", "method", "contains");
    assert_eq!(contains, None, "contains edges must have a NULL callee range");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn imports_edge_has_null_callee_byte_range() {
    // #67: file-level edges (imports / exports) carry no callee identifier range.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod worker;\n\nfn touch() {\n    worker::run();\n}\n")
        .unwrap();
    fs::write(root.join("src/worker.rs"), "pub fn run() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let import = callee_byte_range(&db, "src/lib.rs", "worker", "imports");
    assert_eq!(import, None, "imports edges must have a NULL callee range");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn c_calls_name_edge_stores_callee_identifier_byte_range() {
    // #67: at least one non-Rust language. A C call `helper(runtime)` stores the range of `helper`,
    // not the whole `helper(runtime)` call expression.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let source = r#"
typedef struct Runtime Runtime;

struct Runtime {
  int state;
};

int helper(Runtime *runtime) {
  return runtime->state;
}

int runtime_open(Runtime *runtime) {
  return helper(runtime);
}
"#;
    fs::write(root.join("src/runtime.c"), source).unwrap();
    let config = source_config(root.clone(), Language::C);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (start, end) = callee_byte_range(&db, "runtime_open", "helper", "calls_name")
        .expect("no callee range for runtime_open -> helper");
    let (start, end) = (start as usize, end as usize);
    assert_eq!(&source[start..end], "helper", "C callee range must span exactly `helper`");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ffi_surface_labels_exported_impl_members_separately() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct PhraseRepo;

#[uniffi::export]
impl PhraseRepo {
    pub fn children(&self) {}
    pub fn journal(&self) {}
}

#[cfg_attr(not(target_arch = "wasm32"), uniffi::export(async_runtime = "tokio"))]
impl Runtime {
    pub fn route_search_query(&self) {}
}

pub struct Runtime;

/// Not #[uniffi::export]: this is an internal helper.
pub fn internal_helper() {}

#[cfg_attr(target_arch = "wasm32", ::uniffi::export)]
pub fn exported_fn() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let surface = db.ffi_surface(20).unwrap();
    assert!(
        surface.iter().any(|item| {
            item.reason == "rust_uniffi_export"
                && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("exported_fn"))
        }),
        "direct export should remain direct: {surface:?}"
    );
    assert!(
        surface.iter().any(|item| item.reason == "rust_uniffi_exported_impl"),
        "exported impl/type surface should be explicit: {surface:?}"
    );
    assert!(
        surface.iter().any(|item| {
            item.reason == "rust_uniffi_impl_member"
                && item
                    .symbol
                    .as_deref()
                    .is_some_and(|symbol| symbol.ends_with("route_search_query"))
        }),
        "cfg_attr exported impl member should be labeled separately: {surface:?}"
    );
    assert!(
        surface.iter().any(|item| {
            item.reason == "rust_uniffi_impl_member"
                && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("children"))
        }),
        "impl member should be labeled separately: {surface:?}"
    );
    assert!(
        !surface.iter().any(|item| {
            item.reason == "rust_uniffi_export"
                && item.symbol.as_deref().is_some_and(|symbol| {
                    symbol.ends_with("children") || symbol.ends_with("journal")
                })
        }),
        "impl members must not be reported as direct exports: {surface:?}"
    );
    assert!(
        !surface.iter().any(|item| {
            item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("internal_helper"))
        }),
        "comment-only UniFFI mentions must not create FFI surface rows: {surface:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn find_callers_sees_calls_in_let_bindings() {
    // Regression for issue #47: calls in `let x = f();` and `let-else` initializers produced
    // no caller edge, so find_callers reported 0 callers for a function that is called.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn target() -> Option<i32> {\n    Some(1)\n}\n\npub fn via_statement() {\n    \
         target();\n}\n\npub fn via_let() {\n    let _x = target();\n}\n\npub fn via_let_else() \
         {\n    let Some(_x) = target() else {\n        return;\n    };\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let callers = db.find_callers("target", 50).unwrap();
    let names: Vec<String> = callers.iter().filter_map(|hop| hop.from_symbol.clone()).collect();
    let has = |suffix: &str| names.iter().any(|name| name.ends_with(suffix));

    assert!(has("via_statement"), "missing plain-statement caller; got {names:?}");
    assert!(has("via_let"), "missing `let x = target()` caller; got {names:?}");
    assert!(has("via_let_else"), "missing `let-else` caller; got {names:?}");

    let _ = fs::remove_dir_all(&root);
}

/// #827: an incremental content-changed pass narrows the edge re-resolve to the changed files plus
/// the source files of the in-edges its removals touch — but must preserve `find_callers` parity
/// for BOTH the touched file's own out-edges AND untouched files' in-edges into it. Editing `a.rs`
/// (which defines `target`, called from the UNCHANGED `b.rs`) must NOT drop `b_caller` from
/// `find_callers(target)`: the scoped pass re-points that in-edge onto the changed file's new
/// symbol id even though the first dirty edit only shadows (never removes) the committed `target`.
/// The reverse leg — the touched file's own call into `b.rs` — must resolve too. This is the exact
/// verification the issue calls for: find_callers parity for touched and untouched files after a
/// scoped pass.
#[test]
fn scoped_incremental_pass_preserves_find_callers_both_directions() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod a;\nmod b;\n").unwrap();
    fs::write(
        root.join("src/a.rs"),
        "use crate::b::b_helper;\npub fn target() {}\npub fn a_caller() {\n    b_helper();\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "use crate::a::target;\npub fn b_helper() {}\npub fn b_caller() {\n    target();\n}\n",
    )
    .unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);

    let caller_names = |db: &IndexDatabase, of: &str| -> Vec<String> {
        db.find_callers(of, 50).unwrap().iter().filter_map(|hop| hop.from_symbol.clone()).collect()
    };
    let calls = |db: &IndexDatabase, callee: &str, caller: &str| {
        caller_names(db, callee).iter().any(|name| name.ends_with(caller))
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    // Baseline: both cross-file callers resolve after the full rebuild.
    assert!(calls(&db, "target", "b_caller"), "baseline: b_caller must call target");
    assert!(calls(&db, "b_helper", "a_caller"), "baseline: a_caller must call b_helper");
    drop(db);

    // Edit a.rs (dirty, uncommitted) — prepend a comment so its `target` / `a_caller` symbols get
    // fresh ids while b.rs is untouched. This is the pure per-file change the scoped resolve
    // narrows to (indexed > 0, nothing healed / carried / manifest-changed).
    fs::write(
        root.join("src/a.rs"),
        "// touched\nuse crate::b::b_helper;\npub fn target() {}\npub fn a_caller() {\n    \
         b_helper();\n}\n",
    )
    .unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();

    // The in-edge from the UNCHANGED b.rs must survive the scoped pass — the `find_callers` recall
    // floor. A naive source-file-only write scope would leave it pointing at the shadowed committed
    // `target` and drop b_caller here.
    assert!(
        calls(&db, "target", "b_caller"),
        "after a scoped incremental edit of a.rs, b_caller must still call target (in-edge \
         re-pointed onto the new symbol), got {:?}",
        caller_names(&db, "target"),
    );
    // The touched file's own out-edge into b.rs must resolve too (set (a)).
    assert!(
        calls(&db, "b_helper", "a_caller"),
        "after a scoped incremental edit of a.rs, a_caller must still call b_helper, got {:?}",
        caller_names(&db, "b_helper"),
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn incremental_pass_refreshes_receiver_type_and_target() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let alpha_source = "struct Alpha;\nstruct Beta;\nimpl Alpha { fn run(&self) {} }\nimpl Beta { \
                        fn run(&self) {} }\nfn call(receiver: Alpha) { receiver.run(); }\n";
    fs::write(root.join("src/lib.rs"), alpha_source).unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);

    let edge_state = |db: &IndexDatabase| -> (String, String, String, String) {
        db.storage
            .connection()
            .query_row(
                "SELECT e.receiver_type_hint, e.confidence, e.resolution, s.scope_path
                 FROM edges e
                 JOIN files f ON f.id = e.source_file_id
                 JOIN symbols s ON s.id = e.to_symbol_id
                 WHERE COALESCE(e.from_name, '') LIKE '%call%' AND e.to_name = 'run'
                   AND e.edge_kind = 'calls_name'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
    };

    let db = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        edge_state(&db),
        (
            "Alpha".to_string(),
            "Syntactic".to_string(),
            "receiver_type".to_string(),
            "Alpha::run".to_string(),
        )
    );
    drop(db);

    let beta_source =
        format!("// changed\n{}", alpha_source.replace("receiver: Alpha", "receiver: Beta"));
    fs::write(root.join("src/lib.rs"), beta_source).unwrap();
    let db = IndexDatabase::index_paths(&config, &[root.join("src/lib.rs")]).unwrap();
    assert_eq!(
        edge_state(&db),
        (
            "Beta".to_string(),
            "Syntactic".to_string(),
            "receiver_type".to_string(),
            "Beta::run".to_string(),
        )
    );

    let _ = fs::remove_dir_all(&root);
}

/// A receiver hint is persisted per FILE ROW, and a linked worktree holds its own row for a path
/// the base checkout also has. So two checkouts can disagree about what type a call is made on, and
/// each scope's resolved target has to be its own: re-resolving one must not stamp its answer onto
/// the other, which would make every graph answer served from that worktree describe the wrong
/// method.
#[test]
fn a_linked_worktree_keeps_its_own_receiver_hint_and_target() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    let base_source = "struct Alpha;\nstruct Beta;\nimpl Alpha { fn run<T>(&self) {} }\nimpl Beta \
                       { fn run<T>(&self) {} }\nfn call(receiver: Alpha) { receiver.run::<u8>(); \
                       }\n";
    fs::write(main.join("src/lib.rs"), base_source).unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // Per-scope read: the hint and the resolved owner for THIS file row alone.
    let state_for = |db: &IndexDatabase, worktree_id: &str| -> (String, String) {
        db.storage
            .connection()
            .query_row(
                "SELECT e.receiver_type_hint, s.scope_path
                 FROM edges e
                 JOIN main.files f ON f.id = e.source_file_id
                 JOIN symbols s ON s.id = e.to_symbol_id
                 WHERE e.to_name = 'run' AND e.edge_kind = 'calls_name'
                   AND f.path LIKE '%lib.rs' AND COALESCE(f.worktree_id, '') = ?1",
                [worktree_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };

    assert_eq!(
        state_for(&db, ""),
        ("Alpha".to_string(), "Alpha::run".to_string()),
        "the base checkout calls it on Alpha"
    );

    // The branch changes only the receiver's type, so the two rows differ in exactly the field
    // this change added.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/lib.rs"), base_source.replace("receiver: Alpha", "receiver: Beta"))
        .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch receiver"]);
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "lib.rs indexed as an overlay row");

    let overlay_worktree = crate::index::git_context::worktree_id_of(&linked);
    assert_eq!(
        state_for(&db, &overlay_worktree),
        ("Beta".to_string(), "Beta::run".to_string()),
        "the overlay row resolves against the branch's receiver"
    );

    set_base_scope(&mut db, &main);
    assert_eq!(
        state_for(&db, ""),
        ("Alpha".to_string(), "Alpha::run".to_string()),
        "and indexing the overlay left the base row's hint and target alone"
    );

    let _ = fs::remove_dir_all(&linked);
    let _ = fs::remove_dir_all(&main);
}

/// A renaming import gives a type a name the index never stored: the method lives at `Worker::run`
/// while the only spelling this file has is `Alias`. Probing `Alias::run` finds nothing, and a
/// receiver type that is present but fails also closes the bare-name fallback, so the call loses
/// its last chance. Asserted through the real indexer, because the rewrite depends on the import
/// scope actually recording the rename — a version that recorded nothing looked identical here.
#[test]
fn a_renaming_import_resolves_to_the_type_it_renames() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/worker.rs"),
        "pub struct Worker;\nimpl Worker { pub fn run(&self) {} }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub mod worker;\nuse crate::worker::Worker as Alias;\n\npub fn drive(w: Alias) { \
         w.run(); }\n",
    )
    .unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let resolved: Option<String> = db
        .storage
        .connection()
        .query_row(
            "SELECT s.scope_path
               FROM edges e
               JOIN symbols s ON s.id = e.to_symbol_id
              WHERE e.to_name = 'run' AND e.edge_kind = 'calls_name'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("Worker::run"),
        "the alias resolves to the type it renames, not to a scope named for the alias"
    );

    let _ = fs::remove_dir_all(&root);
}
