use rusqlite::{Connection, params};

use super::*;
use crate::index::schema;

const NEW: &str = "newcommitsha";
const OLD: &str = "oldcommitsha";

fn seeded_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn
}

fn add_file(conn: &Connection, path: &str, commit: &str) -> i64 {
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, '')",
        params![path, format!("sha-{commit}-{path}"), commit],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn add_symbol(conn: &Connection, file_id: i64, name: &str, qualified: &str) -> i64 {
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, end_byte, \
         start_line, end_line) VALUES (?1, 'rust', ?2, ?3, 'function', 0, 10, 1, 1)",
        params![file_id, name, qualified],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn add_edge(
    conn: &Connection,
    source_file_id: i64,
    to_name: &str,
    target_qualified_name: &str,
) -> i64 {
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, target_qualified_name, edge_kind, confidence, \
         resolution) VALUES (?1, ?2, ?3, 'calls_name', 'NameOnly', 'unresolved')",
        params![source_file_id, to_name, target_qualified_name],
    )
    .unwrap();
    // `edges` is a view; `last_insert_rowid` does not survive its INSTEAD OF trigger (#79).
    conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
}

fn edge_state(conn: &Connection, edge_id: i64) -> (Option<i64>, String, String) {
    conn.query_row(
        "SELECT to_symbol_id, confidence, resolution FROM edges WHERE id = ?1",
        params![edge_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

/// The #89 regression: with a DEAD scope's rows still in the DB (post-HEAD-move before gc, or
/// a sibling worktree's live scope), resolution must behave exactly as in a single-scope DB —
/// unique qualified-suffix matches stay `qualified_suffix` (not demoted to `logical_variant`
/// picking an arbitrary scope's copy), and the target id belongs to the ACTIVE scope.
#[test]
fn resolution_is_scoped_to_the_active_checkout() {
    let conn = seeded_conn();
    // Active scope NEW + dead scope OLD, same corpus shape in both.
    let caller_new = add_file(&conn, "a.rs", NEW);
    let defs_new = add_file(&conn, "b.rs", NEW);
    let caller_old = add_file(&conn, "a.rs", OLD);
    let defs_old = add_file(&conn, "b.rs", OLD);
    let target_new = add_symbol(&conn, defs_new, "target", "crate::b::target");
    let target_old = add_symbol(&conn, defs_old, "target", "crate::b::target");
    add_symbol(&conn, caller_new, "caller", "crate::a::caller");
    add_symbol(&conn, caller_old, "caller", "crate::a::caller");

    // The suffix-shaped qualified target exercises the by_qn_tail arm (the one duplicates
    // demote): `b::target` matches `crate::b::target` by suffix.
    let edge_new = add_edge(&conn, caller_new, "b::target", "b::target");
    // The dead scope's own edge: pre-resolved to its own scope's symbol; must stay untouched.
    let edge_old = add_edge(&conn, caller_old, "b::target", "b::target");
    conn.execute(
        "UPDATE edges SET to_symbol_id = ?2, confidence = 'Syntactic', resolution = \
         'qualified_suffix' WHERE id = ?1",
        params![edge_old, target_old],
    )
    .unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, confidence, resolution) = edge_state(&conn, edge_new);
    assert_eq!(
        to,
        Some(target_new),
        "the active edge must resolve to the ACTIVE scope's symbol, not an arbitrary copy"
    );
    assert_eq!(confidence, "Syntactic");
    assert_eq!(
        resolution, "qualified_suffix",
        "a unique in-scope suffix match must not demote to logical_variant"
    );

    let (to, _, resolution) = edge_state(&conn, edge_old);
    assert_eq!(to, Some(target_old), "the dead scope's edge is left untouched");
    assert_eq!(resolution, "qualified_suffix");
}

/// A dirty-worktree overlay shadows the committed row: resolution must target the OVERLAY's
/// symbols (the active content), not the shadowed committed copy.
#[test]
fn resolution_prefers_overlay_over_shadowed_committed_rows() {
    let conn = seeded_conn();
    let caller = add_file(&conn, "a.rs", NEW);
    let defs_committed = add_file(&conn, "b.rs", NEW);
    // Overlay row for b.rs (dirty file): commit_sha empty, worktree id set.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES ('b.rs', 'rust', 'source', 'sha-overlay', 0, 0, '', \
         '/wt')",
        [],
    )
    .unwrap();
    let defs_overlay = conn.last_insert_rowid();
    add_symbol(&conn, defs_committed, "target", "crate::b::target");
    let target_overlay = add_symbol(&conn, defs_overlay, "target", "crate::b::target");
    add_symbol(&conn, caller, "caller", "crate::a::caller");
    let edge = add_edge(&conn, caller, "b::target", "b::target");

    crate::index::install_scope_view(&conn, NEW, "/wt").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, Some(target_overlay), "overlay symbols win over shadowed committed rows");
    assert_eq!(resolution, "qualified_suffix");
}

fn add_symbol_kind(
    conn: &Connection,
    file_id: i64,
    name: &str,
    qualified: &str,
    kind: &str,
) -> i64 {
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, end_byte, \
         start_line, end_line) VALUES (?1, 'rust', ?2, ?3, ?4, 0, 10, 1, 1)",
        params![file_id, name, qualified, kind],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn add_type_ref_edge(conn: &Connection, source_file_id: i64, to_name: &str) -> i64 {
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution) VALUES \
         (?1, ?2, 'references_type', 'NameOnly', 'unresolved')",
        params![source_file_id, to_name],
    )
    .unwrap();
    conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
}

/// #61: a `references_type` reference resolves only to a type DEFINITION. When the sole
/// same-named in-corpus symbol is a non-type (an `impl` block — the type's real definition is
/// external / in another crate), the edge stays UNRESOLVED rather than binding to the non-type.
/// A real type definition still resolves.
#[test]
fn references_type_does_not_resolve_to_a_non_type_symbol() {
    let conn = seeded_conn();
    let user = add_file(&conn, "a.rs", NEW);
    let defs = add_file(&conn, "b.rs", NEW);
    // A symbol in the source file so the index knows it's Rust (source_language drives the
    // type/value-namespace strictness; a real source file always has at least the caller).
    add_symbol(&conn, user, "user_fn", "crate::a::user_fn");
    // Only same-named candidate for `Widget` is an impl block (no struct/enum/trait in-corpus).
    add_symbol_kind(&conn, defs, "Widget", "crate::b::Widget", "impl");
    // A genuine type definition under a different name (the positive control).
    let gadget = add_symbol_kind(&conn, defs, "Gadget", "crate::b::Gadget", "struct");
    let ref_impl = add_type_ref_edge(&conn, user, "Widget");
    let ref_struct = add_type_ref_edge(&conn, user, "Gadget");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, ref_impl);
    assert_eq!(to, None, "a type reference must not bind to an impl block");
    assert_eq!(resolution, "unresolved");

    let (to, _, _) = edge_state(&conn, ref_struct);
    assert_eq!(to, Some(gadget), "a type reference still resolves to a struct definition");
}

fn add_symbol_scope(
    conn: &Connection,
    file_id: i64,
    name: &str,
    qualified: &str,
    scope_path: &str,
) -> i64 {
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name, scope_path, kind, \
         start_byte, end_byte, start_line, end_line) VALUES (?1, 'rust', ?2, ?3, ?4, 'function', \
         0, 10, 1, 1)",
        params![file_id, name, qualified, scope_path],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// #61: `scope_path` is NOT file-unique. When two symbols in different files share a scope_path,
/// an exact scope match is AMBIGUOUS and must NOT bind one as `Exact` — it falls through.
#[test]
fn scope_exact_does_not_bind_an_ambiguous_scope_path() {
    let conn = seeded_conn();
    let f1 = add_file(&conn, "a.rs", NEW);
    let f2 = add_file(&conn, "b.rs", NEW);
    let caller = add_file(&conn, "c.rs", NEW);
    // Two distinct symbols sharing the SAME scope_path (a multi-crate same-name collision).
    add_symbol_scope(&conn, f1, "build", "a.rs::build", "core::Builder::build");
    add_symbol_scope(&conn, f2, "build", "b.rs::build", "core::Builder::build");
    let edge = add_edge(&conn, caller, "build", "core::Builder::build");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, None, "an ambiguous scope_path must not silently bind one at Exact");
    assert_eq!(resolution, "unresolved");
}

/// The positive control: a UNIQUE scope_path binds `Exact` via `scope_exact`.
#[test]
fn scope_exact_binds_a_unique_scope_path() {
    let conn = seeded_conn();
    let defs = add_file(&conn, "b.rs", NEW);
    let caller = add_file(&conn, "c.rs", NEW);
    let target = add_symbol_scope(&conn, defs, "build", "b.rs::build", "core::Builder::build");
    let edge = add_edge(&conn, caller, "build", "core::Builder::build");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, Some(target));
    assert_eq!(resolution, "scope_exact");
}

fn set_local_crate_roots(conn: &Connection, roots: &str) {
    conn.execute(
        "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('local_crate_roots', ?1)",
        params![roots],
    )
    .unwrap();
}

fn add_import_edge(conn: &Connection, source_file_id: i64, to_name: &str, evidence: &str) {
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, target_qualified_name, edge_kind, confidence, \
         resolution, evidence) VALUES (?1, ?2, '', 'imports', 'NameOnly', 'unresolved', ?3)",
        params![source_file_id, to_name, evidence],
    )
    .unwrap();
}

/// #61 Project B: a bare reference to a name `use`d from an EXTERNAL crate (`url::Url`) must not
/// bind to a local same-named symbol — but an explicitly LOCAL-qualified `crate::Url` reference
/// in the same file still must (the qualifier overrides the import; Codex review
/// resolve.rs:334).
#[test]
fn external_import_suppresses_bare_but_not_locally_qualified() {
    let conn = seeded_conn();
    set_local_crate_roots(&conn, "mycrate");
    let user = add_file(&conn, "a.rs", NEW);
    let defs = add_file(&conn, "b.rs", NEW);
    let local = add_symbol(&conn, defs, "Url", "crate::b::Url");
    add_import_edge(&conn, user, "Url", "use url::Url;");
    let bare = add_edge(&conn, user, "Url", "");
    let qualified = add_edge(&conn, user, "Url", "crate::Url");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, bare);
    assert_eq!(to, None, "a bare `Url` from the external `url` crate must not bind locally");
    assert_eq!(resolution, "unresolved");

    let (to, _, _) = edge_state(&conn, qualified);
    assert_eq!(to, Some(local), "explicit `crate::Url` names the local item despite the import");
}

/// #61 Project B (Codex review imports.rs:87 / resolve.rs:41): the imports edge stream emits the
/// path PREFIX of a braced `use` (`std::path`) as well as the real bindings, so the scope must
/// be built from parsed bindings — a local `path` must stay resolvable next to `use
/// std::path::{…}`.
#[test]
fn use_path_prefix_does_not_suppress_a_local_name() {
    let conn = seeded_conn();
    set_local_crate_roots(&conn, "mycrate");
    let user = add_file(&conn, "a.rs", NEW);
    let local = add_symbol(&conn, user, "path", "crate::a::path");
    add_import_edge(&conn, user, "Path", "use std::path::{Path, PathBuf};");
    let call = add_edge(&conn, user, "path", "");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, _) = edge_state(&conn, call);
    assert_eq!(to, Some(local), "`path` is the use PREFIX, not a binding — local `path` resolves");
}

/// #61 Project B (Codex review resolve.rs:89): a path-qualified call whose RECEIVER is an
/// external import (`Url::parse`, with `use url::Url`) must not bind to an in-repo `Url::parse`
/// via the scope-path lookup — the leaf `parse` isn't itself imported, so the receiver root has
/// to be checked. A call through a LOCAL receiver (`Widget::parse`) still resolves.
#[test]
fn qualified_call_through_an_external_receiver_is_suppressed() {
    let conn = seeded_conn();
    set_local_crate_roots(&conn, "mycrate");
    let user = add_file(&conn, "a.rs", NEW);
    let defs = add_file(&conn, "b.rs", NEW);
    add_import_edge(&conn, user, "Url", "use url::Url;");
    // A lowercase external import — a value-receiver method call must NOT be suppressed.
    add_import_edge(&conn, user, "config", "use external_dep::config;");
    add_symbol_scope(&conn, defs, "parse", "b.rs::parse_url", "Url::parse");
    let widget = add_symbol_scope(&conn, defs, "parse", "b.rs::parse_widget", "Widget::parse");
    // `config.build()` extracts as tqn `config::build` (helpers rewrites `.`→`::`); the head is
    // a local value receiver, not an external type path.
    let build = add_symbol_scope(&conn, defs, "build", "b.rs::cfg_build", "config::build");
    let external = add_edge(&conn, user, "parse", "Url::parse");
    let local = add_edge(&conn, user, "parse", "Widget::parse");
    let value_recv = add_edge(&conn, user, "build", "config::build");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, external);
    assert_eq!(to, None, "`Url::parse` (external receiver) must not bind a local `Url::parse`");
    assert_eq!(resolution, "unresolved");

    let (to, _, _) = edge_state(&conn, local);
    assert_eq!(to, Some(widget), "`Widget::parse` (local receiver) resolves normally");

    let (to, _, _) = edge_state(&conn, value_recv);
    assert_eq!(
        to,
        Some(build),
        "`config.build()` (lowercase value receiver) must NOT be suppressed by the import"
    );
}

/// Insert an Imports edge carrying the dedicated module-aware scope columns (the V022 shape):
/// `[scope_start, scope_end)` + `mod_id`. For an inline `mod`, pass `mod_id == scope_start`.
fn add_import_edge_scoped(
    conn: &Connection,
    source_file_id: i64,
    to_name: &str,
    evidence: &str,
    scope_start: i64,
    scope_end: i64,
    mod_id: i64,
) {
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, target_qualified_name, edge_kind, confidence, \
         resolution, evidence, import_scope_start_byte, import_scope_end_byte, import_mod_id) \
         VALUES (?1, ?2, '', 'imports', 'NameOnly', 'unresolved', ?3, ?4, ?5, ?6)",
        params![source_file_id, to_name, evidence, scope_start, scope_end, mod_id],
    )
    .unwrap();
}

/// A `calls_name`/reference edge whose call site sits at `source_start_byte` (drives the
/// module-aware covering test).
fn add_edge_at_byte(
    conn: &Connection,
    source_file_id: i64,
    to_name: &str,
    target_qualified_name: &str,
    source_start_byte: i64,
) -> i64 {
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, target_qualified_name, edge_kind, confidence, \
         resolution, source_start_byte) VALUES (?1, ?2, ?3, 'calls_name', 'NameOnly', \
         'unresolved', ?4)",
        params![source_file_id, to_name, target_qualified_name, source_start_byte],
    )
    .unwrap();
    conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
}

/// #61 (#4 via the DB driver): a `use url::Url` in a parent module must NOT suppress a `Url`
/// reference inside a CHILD module — the module-aware scope columns + inline-`mod` ranges flow
/// through `resolve_all_edges`, not just the unit-level `ImportScope`. A reference in the
/// parent module IS suppressed.
#[test]
fn module_aware_suppression_through_db_driver() {
    let conn = seeded_conn();
    set_local_crate_roots(&conn, "mycrate");
    let user = add_file(&conn, "a.rs", NEW);
    let defs = add_file(&conn, "b.rs", NEW);
    // A local `Url` definition the bare references could (wrongly) bind to.
    let local = add_symbol(&conn, defs, "Url", "crate::b::Url");
    add_symbol(&conn, user, "user_fn", "crate::a::user_fn");
    // Inline modules: parent a body [0,200), child b body [80,160) nested inside.
    add_import_edge_scoped(&conn, user, "a", "mod a", 0, 200, 0);
    add_import_edge_scoped(&conn, user, "b", "mod b", 80, 160, 80);
    // `use url::Url;` lives directly in mod a (enclosing mod_id 0).
    add_import_edge_scoped(&conn, user, "Url", "use url::Url;", 0, 200, 0);
    // A `Url` reference inside child mod b (byte 100) and one in mod a itself (byte 40).
    let in_child = add_edge_at_byte(&conn, user, "Url", "", 100);
    let in_parent = add_edge_at_byte(&conn, user, "Url", "", 40);

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, _) = edge_state(&conn, in_child);
    assert_eq!(
        to,
        Some(local),
        "the parent module's `use url::Url` must not reach a reference in child mod b"
    );
    let (to, _, resolution) = edge_state(&conn, in_parent);
    assert_eq!(to, None, "a `Url` reference in mod a itself IS suppressed by a's `use url::Url`");
    assert_eq!(resolution, "unresolved");
}

/// Insert a `packages` row for the active test scope `(NEW, worktree_id)`.
fn add_package_in(conn: &Connection, manifest_dir: &str, worktree_id: &str, roots_json: &str) {
    conn.execute(
        "INSERT INTO packages(manifest_dir, commit_sha, worktree_id, local_roots_json) VALUES \
         (?1, ?2, ?3, ?4)",
        params![manifest_dir, NEW, worktree_id, roots_json],
    )
    .unwrap();
}

fn add_package(conn: &Connection, manifest_dir: &str, roots_json: &str) {
    add_package_in(conn, manifest_dir, "", roots_json);
}

/// Insert a file row in an explicit `(commit_sha, worktree_id)` scope (the default `add_file`
/// pins `worktree_id=''`). Used by the multi-worktree regression test, which needs two files
/// living in two different worktree scopes at the same commit.
fn add_file_in(conn: &Connection, path: &str, commit: &str, worktree_id: &str) -> i64 {
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, ?4)",
        params![path, format!("sha-{commit}-{worktree_id}-{path}"), commit, worktree_id],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// #61 (#1 via the DB driver): a `path`-dep alias is local for the package that declares it and
/// EXTERNAL for a package that does not. The file→package mapping is computed at LOAD time from
/// the `packages` rows (longest `manifest_dir` prefix) — there is no persisted
/// `files.package_id` — and `packages.local_roots_json` then flows through
/// `resolve_all_edges`.
#[test]
fn per_package_alias_suppression_through_db_driver() {
    let conn = seeded_conn();
    // Global fallback union has both crates; per-package sets differ.
    set_local_crate_roots(&conn, "myws\nlocal");
    add_package(&conn, "a", "[\"myws\",\"local\"]");
    add_package(&conn, "b", "[\"myws\"]");
    // Files live under their package dirs; the loader assigns each by longest-prefix match.
    let file_a = add_file(&conn, "a/src/lib.rs", NEW);
    let file_b = add_file(&conn, "b/src/lib.rs", NEW);
    // A local `Thing` definition both files' bare refs could bind to.
    let local = add_symbol(&conn, file_a, "Thing", "crate::a::Thing");
    add_import_edge_scoped(&conn, file_a, "Thing", "use local::Thing;", 0, 9999, MOD_FILE_ROOT);
    add_import_edge_scoped(&conn, file_b, "Thing", "use local::Thing;", 0, 9999, MOD_FILE_ROOT);
    let ref_a = add_edge_at_byte(&conn, file_a, "Thing", "", 100);
    let ref_b = add_edge_at_byte(&conn, file_b, "Thing", "", 100);

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, _) = edge_state(&conn, ref_a);
    assert_eq!(to, Some(local), "in package A, `local` is its own alias — a LOCAL crate");
    let (to, _, resolution) = edge_state(&conn, ref_b);
    assert_eq!(to, None, "in package B, `local` is an EXTERNAL crate — the bare ref is suppressed");
    assert_eq!(resolution, "unresolved");
}

/// #106 multi-worktree regression: two worktree scopes at the SAME commit whose `packages` carry
/// DIFFERENT path-dep aliases for the same key. Each scope must resolve `use alias::X` against
/// ITS OWN package roots — worktree B must NOT see worktree A's alias as local. This is the
/// leak the dropped persisted `files.package_id` caused: a clean file is a shared
/// commit-scope row, so stamping it with one worktree's package id let the sibling follow
/// the wrong map. Computing the mapping at load from the ACTIVE scope's own `packages` rows
/// makes the leak impossible.
#[test]
fn worktree_package_roots_do_not_leak_across_scopes() {
    let conn = seeded_conn();
    // Both worktrees share the commit `NEW`; their `packages` rows differ on whether `local` is
    // a declared (local) alias. `wt_a` has it; `wt_b` does not.
    let wt_a = "/wt-a";
    let wt_b = "/wt-b";
    set_local_crate_roots(&conn, "myws\nlocal");
    add_package_in(&conn, "", wt_a, "[\"myws\",\"local\"]");
    add_package_in(&conn, "", wt_b, "[\"myws\"]");
    // Each worktree's own overlay row for the same file path (commit_sha empty, worktree set —
    // the dirty-overlay shape `install_scope_view` selects on for the active worktree).
    let file_a = add_file_in(&conn, "src/lib.rs", "", wt_a);
    let file_b = add_file_in(&conn, "src/lib.rs", "", wt_b);
    let local_a = add_symbol(&conn, file_a, "Thing", "crate::Thing");
    // A same-named local symbol in B's scope, the temptation the suppression must resist.
    let _local_b = add_symbol(&conn, file_b, "Thing", "crate::Thing");
    add_import_edge_scoped(&conn, file_a, "Thing", "use local::Thing;", 0, 9999, MOD_FILE_ROOT);
    add_import_edge_scoped(&conn, file_b, "Thing", "use local::Thing;", 0, 9999, MOD_FILE_ROOT);
    let ref_a = add_edge_at_byte(&conn, file_a, "Thing", "", 100);
    let ref_b = add_edge_at_byte(&conn, file_b, "Thing", "", 100);

    // Resolve worktree A's scope: `local` is A's own alias → LOCAL, binds to A's `Thing`.
    crate::index::install_scope_view(&conn, NEW, wt_a).unwrap();
    resolve_all_edges(&conn).unwrap();
    let (to, _, _) = edge_state(&conn, ref_a);
    assert_eq!(to, Some(local_a), "worktree A declares `local` — its bare ref binds local");

    // Resolve worktree B's scope: `local` is NOT B's alias → EXTERNAL, the bare ref is
    // suppressed. If B were following A's package map (the #106 leak), this would bind local.
    crate::index::install_scope_view(&conn, NEW, wt_b).unwrap();
    resolve_all_edges(&conn).unwrap();
    let (to, _, resolution) = edge_state(&conn, ref_b);
    assert_eq!(
        to, None,
        "worktree B does NOT declare `local` — it must not see worktree A's alias as local"
    );
    assert_eq!(resolution, "unresolved");
}

/// The dedicated import-scope columns must NOT perturb the SCIP-oracle candidate set: import
/// edges leave `callee_start_byte` NULL, so `edge_join_candidates` (whose filter is
/// `callee_start_byte IS NOT NULL`) never sees them — this is why the columns are DEDICATED and
/// the `ORACLE_JUDGED_EDGE_KINDS` band-aid (#100) is unnecessary.
#[test]
fn oracle_unaffected_by_import_scope_columns() {
    let conn = seeded_conn();
    let user = add_file(&conn, "a.rs", NEW);
    // An import edge with scope columns set but callee_* NULL.
    add_import_edge_scoped(&conn, user, "Url", "use url::Url;", 0, 200, 0);
    // A call edge that DOES carry a callee range (the oracle's real candidate).
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         callee_start_byte, callee_end_byte) VALUES (?1, 'parse', 'calls_name', 'NameOnly', \
         'unresolved', 10, 15)",
        params![user],
    )
    .unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    // The oracle's candidate filter is exactly `callee_start_byte IS NOT NULL` (store.rs
    // `edge_join_candidates`). Mirror it here: only the call edge qualifies; the import edge —
    // despite its populated import_scope_* columns — leaves callee_* NULL and is excluded.
    let candidate_kinds: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT edge_kind FROM edges WHERE callee_start_byte IS NOT NULL AND \
                 callee_end_byte IS NOT NULL ORDER BY edge_kind",
            )
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        candidate_kinds,
        vec!["calls_name".to_string()],
        "only the call edge (non-NULL callee range) is an oracle candidate; the import edge's \
         scope columns must not pull it in"
    );
}

/// #172: a Python `class Sub(Base)` emits an `implements` edge to `Base`, which is a CLASS (Python
/// has no traits/interfaces). The resolver must prefer the base CLASS over a same-named non-class
/// (e.g. a function) — language-scoped, so Kotlin/TS `implements` still prefers an interface.
#[test]
fn python_implements_prefers_a_base_class_over_a_non_class() {
    let conn = seeded_conn();
    let sub = py_source(&conn, "sub.py");
    let base = py_source(&conn, "base.py");
    let other = py_source(&conn, "other.py");
    // A symbol in sub.py so the resolver knows the reference's source language is Python (a file's
    // language is inferred from its symbols, and the language-scoped `implements` preference is
    // what this exercises).
    py_sym(&conn, sub, "Sub", "sub.py::Sub", "class");
    // The real base class, and a DECOY same-named non-class (a module-level function `Base`).
    let base_class = py_sym(&conn, base, "Base", "base.py::Base", "class");
    let _decoy_fn = py_sym(&conn, other, "Base", "other.py::Base", "function");
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         source_start_byte) VALUES (?1, 'Base', 'implements', 'NameOnly', 'unresolved', 10)",
        params![sub],
    )
    .unwrap();
    let edge: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _confidence, _resolution) = edge_state(&conn, edge);
    assert_eq!(
        to,
        Some(base_class),
        "implements must prefer the base CLASS, not the decoy function"
    );
}

fn py_source(conn: &Connection, path: &str) -> i64 {
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES (?1, 'python', 'source', ?2, 0, 0, ?3, '')",
        params![path, format!("sha-{path}"), NEW],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn py_sym(conn: &Connection, file_id: i64, name: &str, qualified: &str, kind: &str) -> i64 {
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, end_byte, \
         start_line, end_line) VALUES (?1, 'python', ?2, ?3, ?4, 0, 10, 1, 1)",
        params![file_id, name, qualified, kind],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// #172 review: a Python `implements` (base class) must NOT bind to a same-named class in another
/// language. With only a TypeScript `class Base` in the index (the Python base is external), the
/// edge stays unresolved rather than wrongly binding cross-language.
#[test]
fn python_implements_ignores_a_foreign_language_class() {
    let conn = seeded_conn();
    let sub = py_source(&conn, "sub.py");
    py_sym(&conn, sub, "Sub", "sub.py::Sub", "class");
    // The only same-named `Base` is a TYPESCRIPT class.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES ('w.ts', 'typescript', 'source', 'sha-w', 0, 0, ?1, '')",
        params![NEW],
    )
    .unwrap();
    let ts = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, end_byte, \
         start_line, end_line) VALUES (?1, 'typescript', 'Base', 'w.ts::Base', 'class', 0, 10, 1, \
         1)",
        params![ts],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         source_start_byte) VALUES (?1, 'Base', 'implements', 'NameOnly', 'unresolved', 10)",
        params![sub],
    )
    .unwrap();
    let edge: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _confidence, _resolution) = edge_state(&conn, edge);
    assert_eq!(to, None, "a Python base must not bind to a foreign-language class");
}
