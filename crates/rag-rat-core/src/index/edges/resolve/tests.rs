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
    // #224: qualified_name is interned into name_strings; intern then store the id.
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line)
         VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?3), 'function', 0, \
         10, 1, 1)",
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
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line)
         VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?3), ?4, 0, 10, 1, 1)",
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

/// A Rust `references_type` to a generic parameter (`T`) or an associated-type projection
/// (`Self::Value`, `V::Value`) must NOT bind to a same-named concrete type — name-based resolution
/// can't know the concrete type, and an arbitrary confident bind is a pure oracle contradiction. A
/// real type reference (`Gadget`) and a module-qualified path (lowercase root) still resolve.
#[test]
fn references_type_does_not_bind_generic_params_or_projections() {
    let conn = seeded_conn();
    let user = add_file(&conn, "a.rs", NEW);
    let defs = add_file(&conn, "b.rs", NEW);
    add_symbol(&conn, user, "user_fn", "crate::a::user_fn");
    // Same-named concrete types exist in-corpus — the pre-fix resolver would bind to these.
    add_symbol_kind(&conn, defs, "T", "crate::b::T", "struct");
    add_symbol_kind(&conn, defs, "Value", "crate::b::Value", "struct");
    let gadget = add_symbol_kind(&conn, defs, "Gadget", "crate::b::Gadget", "struct");

    let generic = add_type_ref_edge(&conn, user, "T"); // generic parameter
    let projection = add_type_ref_edge(&conn, user, "Self::Value"); // associated-type projection
    let v_projection = add_type_ref_edge(&conn, user, "V::Value"); // type-param projection
    let real = add_type_ref_edge(&conn, user, "Gadget"); // genuine type reference

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    for (edge, what) in [(generic, "T"), (projection, "Self::Value"), (v_projection, "V::Value")] {
        let (to, _, resolution) = edge_state(&conn, edge);
        assert_eq!(to, None, "{what} must stay unresolved, not bind a same-named concrete type");
        assert_eq!(resolution, "unresolved", "{what}");
    }
    let (to, _, _) = edge_state(&conn, real);
    assert_eq!(to, Some(gadget), "a genuine type reference still resolves");
}

/// The PRODUCTION projection path: the Rust extractor emits `Self::Value` / `T::Output` as a
/// `references_type` to the bare LAST segment (`Value` / `Output`), no `::`. When several
/// distinct same-named type definitions exist in DIFFERENT files (different `qualified_name` and
/// `scope_path`), they're not one logical symbol, so a bare reference from a third file stays
/// unresolved rather than guessing. A UNIQUE same-named type still resolves.
#[test]
fn references_type_multi_candidate_across_files_does_not_guess() {
    let conn = seeded_conn();
    let user = add_file(&conn, "a.rs", NEW);
    let f1 = add_file(&conn, "b.rs", NEW);
    let f2 = add_file(&conn, "c.rs", NEW);
    add_symbol(&conn, user, "user_fn", "crate::a::user_fn");
    // Two distinct `Value` types in different files — a bare `Value` ref can't disambiguate.
    add_symbol_kind(&conn, f1, "Value", "b.rs::Value", "type");
    add_symbol_kind(&conn, f2, "Value", "c.rs::Value", "type");
    // A uniquely-named type (positive control).
    let only = add_symbol_kind(&conn, f1, "Config", "b.rs::Config", "struct");

    let ambiguous = add_type_ref_edge(&conn, user, "Value");
    let unique = add_type_ref_edge(&conn, user, "Config");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    assert_eq!(
        edge_state(&conn, ambiguous).0,
        None,
        "ambiguous cross-file type ref stays unresolved"
    );
    assert_eq!(edge_state(&conn, unique).0, Some(only), "a uniquely-named type still resolves");
}

/// `#[cfg]`-split twin types (`#[cfg(unix)] struct Thing` / `#[cfg(windows)] struct Thing`) share
/// `qualified_name` AND `scope_path` — they ARE one logical symbol. A `references_type` to `Thing`
/// must still resolve via `logical_variant` (multi-candidate suppression must not block true
/// logical variants).
#[test]
fn references_type_resolves_cfg_split_twin_types() {
    let conn = seeded_conn();
    let home = add_file(&conn, "a.rs", NEW);
    // Two cfg-gated `Thing` structs in one file: same qualified_name, same (empty) scope_path.
    let first = add_symbol_kind(&conn, home, "Thing", "a.rs::Thing", "struct");
    add_symbol_kind(&conn, home, "Thing", "a.rs::Thing", "struct");
    let edge = add_type_ref_edge(&conn, home, "Thing");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, Some(first), "cfg-twin type variants resolve via logical_variant");
    assert_eq!(resolution, "logical_variant");
}

/// The multi-candidate `references_type` suppression must NOT drop a type defined AND used in its
/// OWN file just because the name recurs elsewhere — `same_file_name` still resolves it locally.
#[test]
fn references_type_resolves_same_file_definition_despite_name_collision() {
    let conn = seeded_conn();
    let home = add_file(&conn, "a.rs", NEW);
    let other = add_file(&conn, "b.rs", NEW);
    // `Error` defined in BOTH files; a reference inside a.rs should bind to a.rs's own `Error`.
    let local = add_symbol_kind(&conn, home, "Error", "a.rs::Error", "struct");
    add_symbol_kind(&conn, other, "Error", "b.rs::Error", "struct");
    let edge = add_type_ref_edge(&conn, home, "Error");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, Some(local), "a same-file type definition still resolves locally");
    assert_eq!(resolution, "same_file_name");
}

fn add_symbol_scope(
    conn: &Connection,
    file_id: i64,
    name: &str,
    qualified: &str,
    scope_path: &str,
) -> i64 {
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, scope_path, kind, \
         start_byte, end_byte, start_line, end_line)
         VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?3), ?4, 'function', \
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

/// Distinct same-(file, name, kind) items that differ only by `scope_path` (e.g. two `impl` blocks
/// each defining `build`, or serde's many `impl Visitor { type Value }`) are NOT one logical
/// symbol. `same_logical_symbol` must split them so the resolver falls through to unresolved
/// instead of picking one arbitrarily at `Syntactic` (`logical_variant`) — that overconfidence made
/// the SCIP oracle count the wrong pick as a contradiction, tanking Rust precision on trait-heavy
/// crates.
#[test]
fn same_file_distinct_scopes_do_not_collapse_to_logical_variant() {
    let conn = seeded_conn();
    let defs = add_file(&conn, "a.rs", NEW);
    let caller = add_file(&conn, "c.rs", NEW);
    // Two distinct `build`s in ONE file (so same qualified_name a.rs::build), different impls.
    add_symbol_scope(&conn, defs, "build", "a.rs::build", "A::build");
    add_symbol_scope(&conn, defs, "build", "a.rs::build", "B::build");
    let edge = add_edge(&conn, caller, "build", ""); // bare name — nothing disambiguates

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, None, "distinct same-file scopes must not collapse to an arbitrary pick");
    assert_eq!(resolution, "unresolved");
}

/// Positive control: GENUINE variants — same file, name, kind AND scope_path (e.g. `#[cfg]`-gated
/// copies) — still group, so the resolver binds the first at `Syntactic` via `logical_variant`.
#[test]
fn same_file_same_scope_variants_still_bind_via_logical_variant() {
    let conn = seeded_conn();
    let defs = add_file(&conn, "a.rs", NEW);
    let caller = add_file(&conn, "c.rs", NEW);
    let first = add_symbol_scope(&conn, defs, "build", "a.rs::build", "A::build");
    add_symbol_scope(&conn, defs, "build", "a.rs::build", "A::build");
    let edge = add_edge(&conn, caller, "build", "");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, Some(first), "true variants (shared scope_path) still bind the first");
    assert_eq!(resolution, "logical_variant");
}

fn set_local_crate_roots(conn: &Connection, roots: &str) {
    conn.execute(
        "INSERT OR REPLACE INTO repo_meta(repo_id, key, value)
         VALUES ('__unassigned__', 'local_crate_roots', ?1)",
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
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line)
         VALUES (?1, 'python', ?2, (SELECT id FROM name_strings WHERE value = ?3), ?4, 0, 10, 1, 1)",
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
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('w.ts::Base')", []).unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line)
         VALUES (?1, 'typescript', 'Base', (SELECT id FROM name_strings WHERE value = \
         'w.ts::Base'), 'class', 0, 10, 1, 1)",
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
/// #174: a Python `from <module> import <T> as <alias>` makes a later reference to `alias` resolve
/// to the imported `T`, NOT an unrelated local symbol named `alias`. The aliased import's Imports
/// edge carries `to_name = T` (target), `evidence = alias`, and a whole-file import scope; the
/// resolver registers `alias → T` and rebinds the alias use before name resolution.
#[test]
fn python_from_import_alias_rebinds_to_the_imported_target() {
    let conn = seeded_conn();
    let app = add_py_file(&conn, "app.py");
    let models = add_py_file(&conn, "models.py");
    let other = add_py_file(&conn, "other.py");
    // The import target, and a DECOY local symbol that shares the ALIAS name.
    let user = add_py_symbol(&conn, models, "User", "models.py::User");
    let decoy = add_py_symbol(&conn, other, "Account", "other.py::Account");
    // `from models import User as Account` → alias carrier: target `User`, alias `Account`, whole-
    // file scope [0, 200).
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'User', 'imports', 'NameOnly', 'unresolved', 'Account', 0, 0, 200, -1)",
        params![app],
    )
    .unwrap();
    // `Account()` at byte 50 (inside the import scope).
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         source_start_byte) VALUES (?1, 'Account', 'calls_name', 'NameOnly', 'unresolved', 50)",
        params![app],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, confidence, _resolution) = edge_state(&conn, call);
    assert_eq!(
        to,
        Some(user),
        "alias `Account` must rebind to the imported `User`, not the local decoy {decoy}"
    );
    assert_ne!(confidence, "NameOnly", "the rebound alias reference resolves");
}

/// #174 review: the alias's `scope_end` bounds the rebind — a reference PAST it (where extraction
/// found the name rebound at module scope) is not covered, so it falls through to normal resolution
/// and binds the local `class Account`. The order/shadow computation itself lives in extraction
/// (`python_next_module_binding`); here we check the resolver honors the resulting `scope_end`.
#[test]
fn python_alias_rebind_respects_the_scope_end_shadow() {
    let conn = seeded_conn();
    let app = add_py_file(&conn, "app.py");
    let models = add_py_file(&conn, "models.py");
    add_py_symbol(&conn, models, "User", "models.py::User");
    // A LOCAL `class Account` at byte 30 — extraction would set the alias `scope_end` here.
    let local_account = add_py_symbol_at(&conn, app, "Account", "app.py::Account", 30);
    // Alias scope is [0, 30): the rebind applies before the redefinition, not after.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'User', 'imports', 'NameOnly', 'unresolved', 'Account', 0, 0, 30, -1)",
        params![app],
    )
    .unwrap();
    // `Account()` at byte 50 — PAST the scope_end, so the alias no longer applies.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         source_start_byte) VALUES (?1, 'Account', 'calls_name', 'NameOnly', 'unresolved', 50)",
        params![app],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _confidence, _resolution) = edge_state(&conn, call);
    assert_eq!(
        to,
        Some(local_account),
        "a reference past scope_end must fall through to the local `class Account`, not the import"
    );
}

/// #174 review: a QUALIFIED reference whose RECEIVER root is an alias is rebound at the receiver —
/// `from models import User as Account; Account.from_id()` resolves the method on the imported
/// `User`, not left unresolved. The alias rewrite rewrites the receiver + the qualified-name root.
#[test]
fn python_alias_rebind_rebinds_a_qualified_receiver() {
    let conn = seeded_conn();
    let app = add_py_file(&conn, "app.py");
    let models = add_py_file(&conn, "models.py");
    // The imported class `User` and its method `from_id` (scope_path `User::from_id`).
    add_py_symbol(&conn, models, "User", "models.py::User");
    let from_id =
        add_py_symbol_scope(&conn, models, "from_id", "models.py::from_id", "User::from_id");
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'User', 'imports', 'NameOnly', 'unresolved', 'Account', 0, 0, 200, -1)",
        params![app],
    )
    .unwrap();
    // `Account.from_id()` at byte 50: to_name=from_id, receiver=Account, qn=Account::from_id.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, target_qualified_name, edge_kind, confidence, \
         resolution, receiver_hint, source_start_byte) VALUES (?1, 'from_id', 'Account::from_id', \
         'calls_name', 'NameOnly', 'unresolved', 'Account', 50)",
        params![app],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _confidence, _resolution) = edge_state(&conn, call);
    assert_eq!(
        to,
        Some(from_id),
        "a qualified receiver alias must rebind so `Account.from_id` resolves to `User.from_id`"
    );
}

/// #174 review: a sequential re-import reassigns the alias; the later binding wins. Real extraction
/// shrinks the first binding's `scope_end` to the second's start, so the scopes ABUT (non-
/// overlapping): `Account -> User` is [0, 20), `Account -> Customer` is [20, 200). `Account()` at
/// byte 50 falls in the second, resolving to `Customer`.
#[test]
fn python_alias_rebind_picks_the_latest_reimport() {
    let conn = seeded_conn();
    let app = add_py_file(&conn, "app.py");
    let models = add_py_file(&conn, "models.py");
    let _user = add_py_symbol(&conn, models, "User", "models.py::User");
    let customer = add_py_symbol(&conn, models, "Customer", "models.py::Customer");
    // First binding `Account -> User` — scope ends where the second import rebinds the name (byte
    // 20).
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'User', 'imports', 'NameOnly', 'unresolved', 'Account', 0, 0, 20, -1)",
        params![app],
    )
    .unwrap();
    // Second binding `Account -> Customer` at byte 20 — reassigns the alias.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'Customer', 'imports', 'NameOnly', 'unresolved', 'Account', 20, 20, 200, -1)",
        params![app],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         source_start_byte) VALUES (?1, 'Account', 'calls_name', 'NameOnly', 'unresolved', 50)",
        params![app],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _confidence, _resolution) = edge_state(&conn, call);
    assert_eq!(to, Some(customer), "the latest re-import of the alias must win");
}

/// #174 review: mutually-exclusive branch imports (`try: import … as DB except: import … as DB`)
/// produce two OVERLAPPING alias bindings to DIFFERENT targets — neither shrinks the other's scope
/// (extraction only shrinks at unconditional rebindings). The alias is genuinely ambiguous, so the
/// reference must stay unresolved rather than picking one by byte order.
#[test]
fn python_alias_rebind_is_ambiguous_across_exclusive_branches() {
    let conn = seeded_conn();
    let app = add_py_file(&conn, "app.py");
    let models = add_py_file(&conn, "models.py");
    add_py_symbol(&conn, models, "Fast", "models.py::Fast");
    add_py_symbol(&conn, models, "Slow", "models.py::Slow");
    // Two covering bindings of `DB`, both spanning the file (the try/except branches don't shrink
    // each other), to different targets.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'Fast', 'imports', 'NameOnly', 'unresolved', 'DB', 0, 0, 200, -1)",
        params![app],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'Slow', 'imports', 'NameOnly', 'unresolved', 'DB', 20, 0, 200, -1)",
        params![app],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         source_start_byte) VALUES (?1, 'DB', 'calls_name', 'NameOnly', 'unresolved', 50)",
        params![app],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, confidence, _resolution) = edge_state(&conn, call);
    assert_eq!(to, None, "an alias bound to different targets in exclusive branches is ambiguous");
    assert_eq!(confidence, "NameOnly");
}

/// #174 review: two branch imports of the SAME target (`try: from a import Engine as DB except: from
/// a import Engine as DB`) overlap but agree, so the alias still resolves.
#[test]
fn python_alias_rebind_resolves_when_branches_agree() {
    let conn = seeded_conn();
    let app = add_py_file(&conn, "app.py");
    let models = add_py_file(&conn, "models.py");
    let engine = add_py_symbol(&conn, models, "Engine", "models.py::Engine");
    for start in [0, 20] {
        conn.execute(
            "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
             evidence, source_start_byte, import_scope_start_byte, import_scope_end_byte, \
             import_mod_id) VALUES (?1, 'Engine', 'imports', 'NameOnly', 'unresolved', 'DB', ?2, \
             0, 200, -1)",
            params![app, start],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         source_start_byte) VALUES (?1, 'DB', 'calls_name', 'NameOnly', 'unresolved', 50)",
        params![app],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, _confidence, _resolution) = edge_state(&conn, call);
    assert_eq!(to, Some(engine), "agreeing branch imports still resolve");
}

/// #174 review: a QUALIFIED reference (`other.Account()`) is not the local alias `Account`, so it
/// must NOT be rebound to the import. The receiver hint marks the reference qualified; the rebind
/// bails on it. With no local `Account` symbol, a rebound reference would resolve to `User` — so a
/// NameOnly (unresolved) outcome proves the rebind was correctly skipped.
#[test]
fn python_alias_rebind_skips_a_qualified_reference() {
    let conn = seeded_conn();
    let app = add_py_file(&conn, "app.py");
    let models = add_py_file(&conn, "models.py");
    add_py_symbol(&conn, models, "User", "models.py::User");
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'User', 'imports', 'NameOnly', 'unresolved', 'Account', 0, 0, 200, -1)",
        params![app],
    )
    .unwrap();
    // `other.Account()` at byte 50 — a member access on `other`, recorded with a receiver hint.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         receiver_hint, source_start_byte) VALUES (?1, 'Account', 'calls_name', 'NameOnly', \
         'unresolved', 'other', 50)",
        params![app],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, confidence, _resolution) = edge_state(&conn, call);
    assert_eq!(to, None, "a qualified `other.Account` must not rebind to the imported `User`");
    assert_eq!(confidence, "NameOnly", "the qualified reference stays unresolved");
}

/// #174 review: a reference BEFORE the import is outside the alias's scope, so it is not rebound.
/// `Account()` (byte 5); `from m import User as Account` (byte 20) — the call precedes the binding,
/// so it must stay unresolved rather than rebinding to `User`.
#[test]
fn python_alias_rebind_skips_a_use_before_the_import() {
    let conn = seeded_conn();
    let app = add_py_file(&conn, "app.py");
    let models = add_py_file(&conn, "models.py");
    add_py_symbol(&conn, models, "User", "models.py::User");
    // The aliased import's scope starts at byte 20.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence, \
         source_start_byte, import_scope_start_byte, import_scope_end_byte, import_mod_id) VALUES \
         (?1, 'User', 'imports', 'NameOnly', 'unresolved', 'Account', 20, 20, 200, -1)",
        params![app],
    )
    .unwrap();
    // `Account()` at byte 5 — BEFORE the import scope opens.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         source_start_byte) VALUES (?1, 'Account', 'calls_name', 'NameOnly', 'unresolved', 5)",
        params![app],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |r| r.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, confidence, _resolution) = edge_state(&conn, call);
    assert_eq!(to, None, "a use before the import is out of scope and must not rebind");
    assert_eq!(confidence, "NameOnly", "the pre-import reference stays unresolved");
}

/// A Python symbol carrying a semantic `scope_path` (e.g. a method `User::from_id`), so the
/// resolver's scope-suffix matching can bind a qualified reference.
fn add_py_symbol_scope(
    conn: &Connection,
    file_id: i64,
    name: &str,
    qualified: &str,
    scope_path: &str,
) -> i64 {
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, scope_path, kind, \
         start_byte, end_byte, start_line, end_line)
         VALUES (?1, 'python', ?2, (SELECT id FROM name_strings WHERE value = ?3), ?4, 'function', \
         0, 10, 1, 1)",
        params![file_id, name, qualified, scope_path],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn add_py_file(conn: &Connection, path: &str) -> i64 {
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES (?1, 'python', 'source', ?2, 0, 0, ?3, '')",
        params![path, format!("sha-{path}"), NEW],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn add_py_symbol(conn: &Connection, file_id: i64, name: &str, qualified: &str) -> i64 {
    add_py_symbol_at(conn, file_id, name, qualified, 0)
}

/// Like [`add_py_symbol`] but with an explicit `start_byte`, so a test can position a local
/// definition before or after the alias import — the order-aware shadow check (#174 review) depends
/// on which comes first.
fn add_py_symbol_at(
    conn: &Connection,
    file_id: i64,
    name: &str,
    qualified: &str,
    start_byte: i64,
) -> i64 {
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line)
         VALUES (?1, 'python', ?2, (SELECT id FROM name_strings WHERE value = ?3), 'class', ?4, ?4 \
         + 10, 1, 1)",
        params![file_id, name, qualified, start_byte],
    )
    .unwrap();
    conn.last_insert_rowid()
}
