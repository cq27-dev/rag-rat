use rag_rat_db::schema;
use rusqlite::{Connection, params};

use super::*;

const NEW: &str = "newcommitsha";
const OLD: &str = "oldcommitsha";

fn seeded_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
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
    add_symbol_kind_language(conn, file_id, name, qualified, kind, Language::Rust)
}

fn add_symbol_kind_language(
    conn: &Connection,
    file_id: i64,
    name: &str,
    qualified: &str,
    kind: &str,
    language: Language,
) -> i64 {
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line)
         VALUES (?1, ?2, ?3, (SELECT id FROM name_strings WHERE value = ?4), ?5, 0, 10, 1, 1)",
        params![file_id, language.as_str(), name, qualified, kind],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn add_type_ref_edge(conn: &Connection, source_file_id: i64, to_name: &str) -> i64 {
    add_named_edge(conn, source_file_id, to_name, EdgeKind::ReferencesType)
}

fn add_named_edge(
    conn: &Connection,
    source_file_id: i64,
    to_name: &str,
    edge_kind: EdgeKind,
) -> i64 {
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution) VALUES \
         (?1, ?2, ?3, 'NameOnly', 'unresolved')",
        params![source_file_id, to_name, edge_kind.as_str()],
    )
    .unwrap();
    conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
}

fn preferred_candidate(id: i64, language: Language, kind: &str) -> IndexedSymbol {
    IndexedSymbol {
        id,
        file_id: id,
        language: language.as_str().to_string(),
        name: "Target".to_string(),
        qualified_name: format!("target-{id}::Target"),
        scope_path: "Target".to_string(),
        kind: kind.to_string(),
        start_byte: 0,
        end_byte: 10,
        start_line: 1,
        end_line: 1,
    }
}

#[test]
fn swift_type_edges_prefer_swift_protocols_and_actors_over_foreign_types() {
    let protocol = preferred_candidate(1, Language::Swift, "protocol");
    let foreign_trait = preferred_candidate(2, Language::Rust, "trait");
    let actor = preferred_candidate(3, Language::Swift, "actor");
    let foreign_struct = preferred_candidate(4, Language::Rust, "struct");
    let swift_class = preferred_candidate(5, Language::Swift, "class");
    let swift_macro = preferred_candidate(6, Language::Swift, "macro");
    let foreign_macro = preferred_candidate(7, Language::Rust, "macro");
    let swift_enum = preferred_candidate(8, Language::Swift, "enum");
    let swift_method = preferred_candidate(9, Language::Swift, "method");
    let foreign_function = preferred_candidate(10, Language::Rust, "function");
    let swift_constructor = preferred_candidate(11, Language::Swift, "constructor");

    let implements = preferred_matches(EdgeKind::Implements, Some(Language::Swift.as_str()), &[
        &foreign_trait,
        &protocol,
    ]);
    assert_eq!(implements.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![protocol.id]);

    let inheritance = preferred_matches(EdgeKind::Implements, Some(Language::Swift.as_str()), &[
        &foreign_trait,
        &swift_class,
    ]);
    assert_eq!(inheritance.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![
        swift_class.id
    ]);

    let protocol_reference =
        preferred_matches(EdgeKind::ReferencesType, Some(Language::Swift.as_str()), &[
            &foreign_trait,
            &protocol,
        ]);
    assert_eq!(protocol_reference.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![
        protocol.id
    ]);

    for edge_kind in [EdgeKind::Constructs, EdgeKind::ReferencesType] {
        let preferred = preferred_matches(edge_kind, Some(Language::Swift.as_str()), &[
            &foreign_struct,
            &actor,
        ]);
        assert_eq!(
            preferred.iter().map(|symbol| symbol.id).collect::<Vec<_>>(),
            vec![actor.id],
            "{edge_kind:?} must prefer the Swift actor"
        );
    }

    let macro_use = preferred_matches(EdgeKind::UsesMacro, Some(Language::Swift.as_str()), &[
        &foreign_macro,
        &swift_macro,
    ]);
    assert_eq!(macro_use.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![swift_macro.id]);

    let enum_construction =
        preferred_matches(EdgeKind::Constructs, Some(Language::Swift.as_str()), &[
            &foreign_struct,
            &swift_enum,
        ]);
    assert_eq!(enum_construction.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![
        swift_enum.id
    ]);

    let call = preferred_matches(EdgeKind::CallsName, Some(Language::Swift.as_str()), &[
        &foreign_function,
        &swift_method,
    ]);
    assert_eq!(call.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![swift_method.id]);

    let initializer_call =
        preferred_matches(EdgeKind::CallsName, Some(Language::Swift.as_str()), &[
            &foreign_function,
            &swift_constructor,
        ]);
    assert_eq!(initializer_call.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![
        swift_constructor.id
    ]);

    assert!(
        preferred_matches(EdgeKind::ReferencesType, Some(Language::Swift.as_str()), &[
            &foreign_struct
        ],)
        .is_empty(),
        "Swift type names without a Swift target must not prefer a foreign symbol"
    );
    assert!(
        preferred_matches(EdgeKind::UsesMacro, Some(Language::Swift.as_str()), &[&foreign_macro],)
            .is_empty(),
        "Swift macro names without a Swift target must not prefer a foreign macro"
    );
    assert!(
        preferred_matches(EdgeKind::CallsName, Some(Language::Swift.as_str()), &[
            &foreign_function
        ],)
        .is_empty(),
        "Swift calls without a Swift target must not prefer a foreign function"
    );
}

#[test]
fn generic_resolution_does_not_prefer_swift_only_symbol_kinds() {
    let swift_protocol = preferred_candidate(1, Language::Swift, "protocol");
    let rust_trait = preferred_candidate(2, Language::Rust, "trait");
    let swift_actor = preferred_candidate(3, Language::Swift, "actor");
    let rust_struct = preferred_candidate(4, Language::Rust, "struct");

    let implements =
        preferred_matches(EdgeKind::Implements, Some(Language::TypeScript.as_str()), &[
            &swift_protocol,
            &rust_trait,
        ]);
    assert_eq!(implements.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![rust_trait.id]);

    for edge_kind in [EdgeKind::Constructs, EdgeKind::ReferencesType] {
        let preferred = preferred_matches(edge_kind, Some(Language::TypeScript.as_str()), &[
            &swift_actor,
            &rust_struct,
        ]);
        assert_eq!(preferred.iter().map(|symbol| symbol.id).collect::<Vec<_>>(), vec![
            rust_struct.id
        ]);
    }
}

#[test]
fn swift_name_only_edges_do_not_fall_back_to_foreign_symbols() {
    let conn = seeded_conn();
    let source = add_file(&conn, "Caller.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [source]).unwrap();
    // Top-level Swift scripts can contain calls without declaring any indexed symbol. Resolution
    // must use the source file's language rather than inferring it from the symbol table.
    let foreign = add_file(&conn, "foreign.rs", NEW);
    add_symbol_kind(&conn, foreign, "URL", "foreign.rs::URL", "struct");
    add_symbol_kind(&conn, foreign, "stringify", "foreign.rs::stringify", "macro");
    add_symbol_kind(&conn, foreign, "parse", "foreign.rs::parse", "function");
    add_symbol_kind_language(
        &conn,
        source,
        "Foundation",
        "Caller.swift::Foundation",
        "struct",
        Language::Swift,
    );
    let type_edge = add_type_ref_edge(&conn, source, "URL");
    let macro_edge = add_named_edge(&conn, source, "stringify", EdgeKind::UsesMacro);
    let call_edge = add_named_edge(&conn, source, "parse", EdgeKind::CallsName);
    let import_edge = add_named_edge(&conn, source, "Foundation", EdgeKind::Imports);

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    for (edge, description) in [
        (type_edge, "Swift SDK/external type"),
        (macro_edge, "Swift external macro"),
        (call_edge, "Swift external function"),
        (import_edge, "Swift module import"),
    ] {
        let (to, _, resolution) = edge_state(&conn, edge);
        assert_eq!(to, None, "a {description} must not bind to a foreign symbol");
        assert_eq!(resolution, "unresolved");
    }
}

#[test]
fn swift_suppresses_and_re_resolves_attached_macro_candidates() {
    let conn = seeded_conn();
    let source = add_file(&conn, "Caller.swift", NEW);
    let definitions = add_file(&conn, "Macros.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id IN (?1, ?2)", params![
        source,
        definitions
    ])
    .unwrap();
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence) \
         VALUES (?1, 'Observable', 'uses_macro', 'NameOnly', 'unresolved', '@Observable'), (?1, \
         'available', 'uses_macro', 'NameOnly', 'unresolved', '@available(*, deprecated)'), (?1, \
         'external', 'uses_macro', 'NameOnly', 'unresolved', '#external')",
        [source],
    )
    .unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let suppressed_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges_data d
             JOIN name_strings r ON r.id = d.resolution_id
             WHERE r.value = 'suppressed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(suppressed_count, 2, "attached candidates remain available for later resolution");
    let attached_visible_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE to_name IN ('Observable', 'available')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attached_visible_count, 0, "unresolved attributes stay out of graph queries");
    let freestanding_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges WHERE to_name = 'external'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(freestanding_count, 1, "unresolved freestanding macros remain graph evidence");
    assert_hidden_agrees_with_visibility(&conn);

    let observable = add_symbol_kind_language(
        &conn,
        definitions,
        "Observable",
        "Macros.swift::Observable",
        "macro",
        Language::Swift,
    );
    resolve_all_edges(&conn).unwrap();
    let resolved_macro: Option<i64> = conn
        .query_row("SELECT to_symbol_id FROM edges WHERE to_name = 'Observable'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(resolved_macro, Some(observable), "a later macro definition must heal the edge");
    assert_hidden_agrees_with_visibility(&conn);

    conn.execute("DELETE FROM symbols WHERE id = ?1", [observable]).unwrap();
    resolve_all_edges(&conn).unwrap();
    let visible_after_removal: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges WHERE to_name = 'Observable'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(visible_after_removal, 0, "removing the macro must suppress the candidate again");
    assert_hidden_agrees_with_visibility(&conn);
}

#[test]
fn full_rebuild_uses_language_of_symbol_less_swift_files() {
    let conn = seeded_conn();
    let source = add_file(&conn, "script.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [source]).unwrap();
    let foreign = add_file(&conn, "foreign.rs", NEW);
    let target_id = add_symbol_kind(&conn, foreign, "parse", "foreign.rs::parse", "function");
    let target = crate::index::symbols::Symbol {
        name: "parse".to_string(),
        qualified_name: "foreign.rs::parse".to_string(),
        scope_path: "parse".to_string(),
        kind: "function".to_string(),
        start_byte: 0,
        end_byte: 10,
        start_line: 1,
        end_line: 1,
        signature: None,
        docs: None,
        is_test: false,
        facts: Vec::new(),
    };
    let candidate = EdgeCandidate {
        from_symbol_id: None,
        from_name: Some("script.swift".to_string()),
        to_name: "parse".to_string(),
        target_qualified_name: None,
        evidence: Some("parse()".to_string()),
        receiver_hint: None,
        source_span: EdgeSpan { start_line: 1, end_line: 1, start_byte: 0, end_byte: 7 },
        callee_span: None,
        import_scope: None,
        edge_kind: EdgeKind::CallsName,
        confidence: EdgeConfidence::NameOnly,
    };
    let mut graph = FullRebuildGraph::default();
    graph.push_symbol(target_id, foreign, Language::Rust, &target);
    graph.push_edge(source, &candidate, &[]);

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_and_insert_edges(&conn, graph).unwrap();

    let edge: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap();
    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, None, "the full driver must apply Swift policy without a source symbol");
    assert_eq!(resolution, "unresolved");
}

#[test]
fn swift_qualified_edges_do_not_bind_foreign_scope_paths() {
    let conn = seeded_conn();
    let source = add_file(&conn, "Caller.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [source]).unwrap();
    let foreign = add_file(&conn, "foreign.rs", NEW);
    add_symbol_scope_language(
        &conn,
        foreign,
        "fetch",
        "foreign.rs::fetch",
        "Client::fetch",
        "function",
        Language::Rust,
    );
    add_symbol_scope_language(
        &conn,
        foreign,
        "Request",
        "foreign.rs::Request",
        "API::Request",
        "struct",
        Language::Rust,
    );
    let call = add_edge(&conn, source, "fetch", "Client::fetch");
    let type_ref = {
        conn.execute(
            "INSERT INTO edges(source_file_id, to_name, target_qualified_name, edge_kind, \
             confidence, resolution) VALUES (?1, 'Request', 'API::Request', 'references_type', \
             'NameOnly', 'unresolved')",
            params![source],
        )
        .unwrap();
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
    };

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    for edge in [call, type_ref] {
        let (to, _, resolution) = edge_state(&conn, edge);
        assert_eq!(to, None, "qualified Swift references must not bind foreign scope paths");
        assert_eq!(resolution, "unresolved");
    }
}

/// An enum case is bindable by BARE NAME only through Swift's shorthand `.idle`, the one shape that
/// evidences a case. A value-receiver method call (`client.idle()`) and a static member read
/// (`Config.idle`) share the same `calls_name` edge kind but carry a receiver/qualifier — binding
/// those to an unrelated `enum Status { case idle }` would report an ordinary call or property read
/// as a caller of that case. Poisons the index with ONLY the case symbol, so a resolution can come
/// from nowhere else: without the bare-shape rule the receiver/qualified edges bind it and this
/// fails.
#[test]
fn swift_enum_cases_bind_by_bare_name_only_for_the_shorthand_shape() {
    let conn = seeded_conn();
    let source = add_file(&conn, "Caller.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [source]).unwrap();
    let definitions = add_file(&conn, "Status.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [definitions]).unwrap();
    // The ONLY `idle` in the repo is an enum case.
    let idle = add_symbol_scope_language(
        &conn,
        definitions,
        "idle",
        "Status.swift::idle",
        "Status::idle",
        "enum_case",
        Language::Swift,
    );

    // `client.idle()` — a value receiver, not a case.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         receiver_hint) VALUES (?1, 'idle', 'calls_name', 'NameOnly', 'unresolved', 'client')",
        params![source],
    )
    .unwrap();
    let receiver_call: i64 =
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap();

    // `Config.idle` — a static member read of a type that has no such member.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         target_qualified_name, receiver_hint) VALUES (?1, 'idle', 'calls_name', 'NameOnly', \
         'unresolved', 'Config::idle', 'Config')",
        params![source],
    )
    .unwrap();
    let static_read: i64 =
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap();

    // `.idle` — the shorthand: no qualifier, no receiver. This one IS the case.
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution) VALUES \
         (?1, 'idle', 'calls_name', 'NameOnly', 'unresolved')",
        params![source],
    )
    .unwrap();
    let shorthand: i64 =
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (receiver_target, ..) = edge_state(&conn, receiver_call);
    assert_eq!(
        receiver_target, None,
        "a value-receiver call must not bind an enum case by bare name"
    );
    let (static_target, ..) = edge_state(&conn, static_read);
    assert_eq!(
        static_target, None,
        "a static member read must not bind an unrelated enum case by bare name"
    );
    let (shorthand_target, ..) = edge_state(&conn, shorthand);
    assert_eq!(
        shorthand_target,
        Some(idle),
        "the shorthand `.idle` shape still resolves to the case"
    );
}

#[test]
fn swift_value_receiver_calls_resolve_by_bare_name() {
    let conn = seeded_conn();
    let source = add_file(&conn, "Caller.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [source]).unwrap();
    let definitions = add_file(&conn, "Client.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [definitions]).unwrap();
    let fetch = add_symbol_scope_language(
        &conn,
        definitions,
        "fetch",
        "Client.swift::fetch",
        "Client::fetch",
        "function",
        Language::Swift,
    );
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
         receiver_hint) VALUES (?1, 'fetch', 'calls_name', 'NameOnly', 'unresolved', 'client')",
        params![source],
    )
    .unwrap();
    let call: i64 = conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap();

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, confidence, resolution) = edge_state(&conn, call);
    assert_eq!(to, Some(fetch));
    assert_eq!(confidence, "Syntactic");
    assert_eq!(resolution, "target_name_fallback");
    let receiver: String = conn
        .query_row("SELECT receiver_hint FROM edges WHERE id = ?1", [call], |row| row.get(0))
        .unwrap();
    assert_eq!(receiver, "client");
}

#[test]
fn swift_local_receivers_override_external_bare_name_suppression() {
    let mut make = preferred_candidate(1, Language::Swift, "function");
    make.name = "make".to_string();
    make.qualified_name = "Store.swift::make".to_string();
    make.scope_path = "Store::make".to_string();
    let symbols = [make];
    let index = SymbolIndex::build(&symbols);

    for receiver in ["Self", "self", "super"] {
        let qualified = format!("{receiver}::make");
        let resolved = resolve_symbol(
            ResolveSymbolRequest {
                name: "make",
                target_qualified_name: Some(&qualified),
                edge_kind: EdgeKind::CallsName,
                evidence: Some("make()"),
                receiver_hint: Some(receiver),
                source_file_id: 1,
                source_language: Some(Language::Swift.as_str()),
                imported_external: true,
            },
            &index,
        );
        let (target, confidence, resolution) =
            resolved.unwrap_or_else(|| panic!("{receiver} must override external suppression"));
        assert_eq!(target.id, symbols[0].id);
        assert_eq!(confidence, EdgeConfidence::Syntactic);
        assert_eq!(resolution, "target_name_fallback");
    }
}

#[test]
fn swift_self_and_super_init_calls_resolve_to_constructors() {
    let conn = seeded_conn();
    let source = add_file(&conn, "Child.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [source]).unwrap();
    let definitions = add_file(&conn, "Parent.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [definitions]).unwrap();
    let init = add_symbol_kind_language(
        &conn,
        definitions,
        "init",
        "Parent.swift::init",
        "constructor",
        Language::Swift,
    );
    let calls = ["Self", "self", "super"].map(|receiver| {
        conn.execute(
            "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, \
             receiver_hint) VALUES (?1, 'init', 'calls_name', 'NameOnly', 'unresolved', ?2)",
            params![source, receiver],
        )
        .unwrap();
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get(0)).unwrap()
    });

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    for call in calls {
        let (to, confidence, resolution) = edge_state(&conn, call);
        assert_eq!(to, Some(init));
        assert_eq!(confidence, "Syntactic");
        assert_eq!(resolution, "target_name_fallback");
    }
}

#[test]
fn swift_enum_constructions_resolve_to_enum_symbols() {
    let conn = seeded_conn();
    let source = add_file(&conn, "Caller.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [source]).unwrap();
    add_symbol_kind_language(
        &conn,
        source,
        "caller",
        "Caller.swift::caller",
        "function",
        Language::Swift,
    );
    let definitions = add_file(&conn, "Status.swift", NEW);
    conn.execute("UPDATE main.files SET language = 'swift' WHERE id = ?1", [definitions]).unwrap();
    let status = add_symbol_kind_language(
        &conn,
        definitions,
        "Status",
        "Status.swift::Status",
        "enum",
        Language::Swift,
    );
    let construction = add_named_edge(&conn, source, "Status", EdgeKind::Constructs);

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    resolve_all_edges(&conn).unwrap();

    let (to, confidence, resolution) = edge_state(&conn, construction);
    assert_eq!(to, Some(status));
    assert_eq!(confidence, "Syntactic");
    assert_eq!(resolution, "target_name_fallback");
}

#[test]
fn swift_overloads_do_not_collapse_to_one_logical_variant() {
    let mut by_id = preferred_candidate(1, Language::Swift, "function");
    by_id.name = "fetch".to_string();
    by_id.qualified_name = "Service.swift::fetch".to_string();
    by_id.scope_path = "Service::fetch".to_string();
    by_id.start_byte = 10;
    by_id.end_byte = 30;

    let mut by_name = by_id.clone();
    by_name.id = 2;
    by_name.start_byte = 40;
    by_name.end_byte = 65;

    assert!(
        !same_logical_symbol(&[&by_id, &by_name]),
        "same-name Swift overloads must remain ambiguous until signatures enter stored identity"
    );
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
    // The source function owns the type-reference edges; `files.language` selects Rust's strict
    // type namespace policy.
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
    add_symbol_scope_language(
        conn,
        file_id,
        name,
        qualified,
        scope_path,
        "function",
        Language::Rust,
    )
}

fn add_symbol_scope_language(
    conn: &Connection,
    file_id: i64,
    name: &str,
    qualified: &str,
    scope_path: &str,
    kind: &str,
    language: Language,
) -> i64 {
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, scope_path, kind, \
         start_byte, end_byte, start_line, end_line)
         VALUES (?1, ?2, ?3, (SELECT id FROM name_strings WHERE value = ?4), ?5, ?6, 0, 10, 1, 1)",
        params![file_id, language.as_str(), name, qualified, scope_path, kind],
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
    // The subclass declaration owns the edge; `files.language` selects Python's class preference.
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

/// Stage `file_ids` into `temp.edge_rewrite_files` the way `begin_scoped_edge_rewrite` + the
/// capture seams do, so a connection-level test can drive [`resolve_changed_edges`] directly
/// (#827).
fn stage_edge_rewrite_files(conn: &Connection, file_ids: &[i64]) {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS edge_rewrite_files(file_id INTEGER PRIMARY KEY);
         DELETE FROM temp.edge_rewrite_files;",
    )
    .unwrap();
    for &file_id in file_ids {
        conn.execute(
            "INSERT OR IGNORE INTO temp.edge_rewrite_files(file_id) VALUES (?1)",
            params![file_id],
        )
        .unwrap();
    }
}

/// #827: a scoped re-resolve rewrites ONLY the source files staged in `temp.edge_rewrite_files`,
/// leaving every other file's edges untouched — even one a FULL re-resolve WOULD bind. The unstaged
/// caller's edge is left `unresolved` with its target present in the corpus: if
/// `resolve_changed_edges` fell back to a full pass it would bind it, so the surviving `unresolved`
/// state is what proves the narrowing is real (the fast path disagrees with the fallback — not the
/// full path in disguise).
#[test]
fn scoped_resolve_rewrites_only_staged_source_files() {
    let conn = seeded_conn();
    let caller_staged = add_file(&conn, "a.rs", NEW);
    let caller_unstaged = add_file(&conn, "b.rs", NEW);
    let defs = add_file(&conn, "d.rs", NEW);
    let target = add_symbol(&conn, defs, "target", "crate::d::target");
    add_symbol(&conn, caller_staged, "caller_a", "crate::a::caller_a");
    add_symbol(&conn, caller_unstaged, "caller_b", "crate::b::caller_b");
    let edge_staged = add_edge(&conn, caller_staged, "d::target", "d::target");
    let edge_unstaged = add_edge(&conn, caller_unstaged, "d::target", "d::target");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    // Both edges arrive `unresolved`. Stage ONLY a.rs; b.rs is the poison a full pass would bind.
    stage_edge_rewrite_files(&conn, &[caller_staged]);
    resolve_changed_edges(&conn).unwrap();

    let (to, confidence, resolution) = edge_state(&conn, edge_staged);
    assert_eq!(to, Some(target), "the staged file's edge is re-resolved against the full pool");
    assert_eq!(confidence, "Syntactic");
    assert_eq!(resolution, "qualified_suffix");

    let (to, _, resolution) = edge_state(&conn, edge_unstaged);
    assert_eq!(to, None, "the UNSTAGED file's edge is left untouched by the scoped pass");
    assert_eq!(
        resolution, "unresolved",
        "a full re-resolve would bind this identical edge — its surviving unresolved state proves \
         the scoped pass did NOT fall back to a full pass"
    );
}

/// #827: staging the SOURCE FILE of an in-edge (what `remove_file_in_scope` captures at file
/// granularity) re-points that in-edge onto the changed file's NEW symbol id — so a caller in an
/// UNCHANGED file survives a change to its target's file (the `find_callers` recall floor).
#[test]
fn scoped_resolve_repoints_staged_inedge_onto_moved_target() {
    let conn = seeded_conn();
    let caller = add_file(&conn, "a.rs", NEW);
    let defs = add_file(&conn, "d.rs", NEW);
    add_symbol(&conn, caller, "caller", "crate::a::caller");
    let old_target = add_symbol(&conn, defs, "target", "crate::d::target");
    let edge = add_edge(&conn, caller, "d::target", "d::target");
    // The caller's edge starts resolved to the target's ORIGINAL id.
    conn.execute(
        "UPDATE edges SET to_symbol_id = ?2, confidence = 'Syntactic', resolution = \
         'qualified_suffix' WHERE id = ?1",
        params![edge, old_target],
    )
    .unwrap();

    // Simulate the target file changing: its symbol is deleted and re-inserted with a NEW id, and
    // the in-edge is NULLed exactly as `remove_file_in_scope` does. Stage BOTH files, as the
    // capture seams would (the changed file d.rs plus the source file a.rs of the NULLed
    // in-edge).
    conn.execute("DELETE FROM symbols WHERE id = ?1", params![old_target]).unwrap();
    conn.execute(
        "UPDATE edges SET to_symbol_id = NULL, confidence = 'NameOnly', resolution = 'unresolved' \
         WHERE id = ?1",
        params![edge],
    )
    .unwrap();
    let new_target = add_symbol(&conn, defs, "target", "crate::d::target");
    assert_ne!(new_target, old_target, "the re-inserted target must carry a fresh id");

    crate::index::install_scope_view(&conn, NEW, "").unwrap();
    stage_edge_rewrite_files(&conn, &[caller, defs]);
    resolve_changed_edges(&conn).unwrap();

    let (to, _, resolution) = edge_state(&conn, edge);
    assert_eq!(to, Some(new_target), "the staged in-edge re-points onto the target's NEW id");
    assert_eq!(resolution, "qualified_suffix");
}
