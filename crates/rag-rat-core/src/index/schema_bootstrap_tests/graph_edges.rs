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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
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

    let _ = fs::remove_dir_all(root);
}
