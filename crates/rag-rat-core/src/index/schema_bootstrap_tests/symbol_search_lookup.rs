use super::*;

#[test]
fn stale_generated_flags_are_rederived_on_open() {
    // #202 review (P2): incremental discovery rewrites a file row only on sha/language/kind change,
    // so an index built BEFORE the flag's definition changed keeps the old `files.generated` and
    // would still surface generated bindings. The version-gated re-derive must heal it on next
    // open.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/generated")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn shared_symbol() {}\n").unwrap();
    fs::write(root.join("src/generated/bindings.rs"), "pub fn shared_symbol() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Simulate a pre-#202 index: the generated-dir file was flagged 0, and the flags-version meta
    // is absent/stale so the gate will fire.
    db.storage
        .connection()
        .execute("UPDATE main.files SET generated = 0 WHERE path LIKE '%/generated/%'", [])
        .unwrap();
    db.storage
        .connection()
        .execute("DELETE FROM index_meta WHERE key = ?1", [GENERATED_FLAGS_VERSION_KEY])
        .unwrap();
    let by_name = rag_rat_query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("shared_symbol".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    };
    // Pre-heal: the mis-flagged generated copy leaks into the default (exclude-generated) search.
    let before = db.symbol_candidates(&by_name, false).unwrap();
    assert!(
        before.candidates.iter().any(|c| c.path.contains("/generated/")),
        "precondition: stale flag leaks the generated copy"
    );

    // The version-gated re-derive heals every mis-flagged row from the path heuristic.
    db.ensure_generated_flags_current().unwrap();
    let after = db.symbol_candidates(&by_name, false).unwrap();
    assert!(!after.candidates.is_empty(), "source symbol still resolves");
    assert!(
        after.candidates.iter().all(|c| !c.path.contains("/generated/")),
        "re-derive must re-exclude the generated copy: {:?}",
        after.candidates.iter().map(|c| &c.path).collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_and_read_chunk_attach_bounded_graph_evidence() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn helper() {}\n\npub fn caller() {\n    helper();\n}\n\npub fn operator_noise() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO edges(source_file_id, from_symbol_id, to_name, edge_kind, confidence, \
             resolution) SELECT file_id, id, 'helper', 'uses_operator', 'NameOnly', 'unresolved' \
             FROM symbols WHERE name = 'operator_noise'",
            [],
        )
        .unwrap();

    let hits = db.search("helper caller", 10, false).unwrap();
    let helper_hit = hits
        .iter()
        .find(|hit| hit.symbol_path.as_deref().is_some_and(|path| path.ends_with("helper")))
        .expect("helper search hit");
    let helper_graph = helper_hit.graph.as_ref().expect("helper graph evidence");
    assert_eq!(helper_graph.caller_count, 1);
    assert!(helper_graph.top_callers.iter().any(|caller| {
        caller.symbol_path.ends_with("caller")
            && caller.callsite.line == 4
            && caller.callsite.span == [4, 4]
            && caller.confidence == "syntactic"
    }));
    assert!(
        helper_graph.top_callers.iter().all(|caller| caller.edge_kind != "uses_operator"),
        "unresolved operator uses must not fall back to a same-named symbol: {helper_graph:#?}"
    );
    assert!(helper_graph.callers.is_empty(), "search keeps graph compact");

    // The SAME poisoned edge must be invisible to `impact_surface`, whose Fuzzy mode matches by
    // NAME — the last consumer still admitting it. An unresolved `uses_operator` row is a BUILT-IN
    // operator token (Swift emits one per `+` / `==`), so letting it through reports every
    // arithmetic expression as a direct caller of any same-named symbol. `operator_noise` only ever
    // "calls" `helper` through that poisoned edge, so its presence here is exactly the bug.
    for resolution_mode in [
        rag_rat_query::graph::GraphResolutionMode::Exact,
        rag_rat_query::graph::GraphResolutionMode::Syntactic,
        rag_rat_query::graph::GraphResolutionMode::Fuzzy,
    ] {
        let items = db.impact_surface_with_options("helper", 20, resolution_mode).unwrap();
        // The graph lane records the edge kind in its evidence ("<kind> edge to <symbol>"), so an
        // admitted operator edge is visible there. `operator_noise` still legitimately appears via
        // OTHER lanes (it is a same-file sibling of `helper`), which is why this asserts on the
        // evidence rather than on the symbol's mere presence.
        assert!(
            items
                .iter()
                .flat_map(|item| &item.evidence)
                .all(|evidence| !evidence.contains("uses_operator")),
            "an unresolved operator use must not surface as an impact neighbor \
             ({resolution_mode:?}): {items:#?}"
        );
    }

    let caller_hit = hits
        .iter()
        .find(|hit| hit.symbol_path.as_deref().is_some_and(|path| path.ends_with("caller")))
        .expect("caller search hit");
    let caller_graph = caller_hit.graph.as_ref().expect("caller graph evidence");
    assert!(caller_graph.top_callees.iter().any(|callee| {
        callee.target == "helper"
            && callee.callsite.line == 4
            && callee.callsite.span == [4, 4]
            && callee.confidence == "syntactic"
    }));

    let chunk = db.read_chunk(caller_hit.chunk_id).unwrap().expect("caller chunk");
    let full_graph = chunk.graph.as_ref().expect("full read_chunk graph");
    assert!(full_graph.symbol.as_ref().is_some_and(|symbol| symbol.name == "caller"));
    assert!(
        full_graph
            .callees
            .iter()
            .any(|callee| callee.target == "helper" && callee.callsite.line == 4)
    );
    assert!(full_graph.notes.iter().any(|note| note.contains("tree-sitter/syntactic")));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn graph_exact_mode_requires_verified_symbol_identity() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn helper() {}\n\npub fn caller() {\n    helper();\n}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let helper = db.symbols("helper", Some(Language::Rust), 10).unwrap().remove(0);
    let caller = db.symbols("caller", Some(Language::Rust), 10).unwrap().remove(0);

    let bare_exact = db
        .find_callers_with_options("helper", 10, &rag_rat_query::graph::GraphTraversalOptions {
            resolution_mode: rag_rat_query::graph::GraphResolutionMode::Exact,
            ..Default::default()
        })
        .unwrap();
    assert!(bare_exact.is_empty(), "bare exact lookup should not fall back: {bare_exact:?}");

    let exact_callers = db
        .find_callers_with_options("helper", 10, &rag_rat_query::graph::GraphTraversalOptions {
            resolution_mode: rag_rat_query::graph::GraphResolutionMode::Exact,
            symbol_id: Some(helper.symbol_id),
            ..Default::default()
        })
        .unwrap();
    assert!(
        exact_callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("caller"))
                && edge.verified_target_symbol
        }),
        "exact callers: {exact_callers:?}"
    );
    assert!(exact_callers.iter().all(|edge| edge.verified_target_symbol));

    let exact_callees = db
        .trace_callees_with_options("caller", 10, &rag_rat_query::graph::GraphTraversalOptions {
            resolution_mode: rag_rat_query::graph::GraphResolutionMode::Exact,
            symbol_id: Some(caller.symbol_id),
            ..Default::default()
        })
        .unwrap();
    assert!(
        exact_callees.iter().any(|edge| {
            edge.target.as_deref() == Some("helper") && edge.verified_target_symbol
        }),
        "exact callees: {exact_callees:?}"
    );
    assert!(exact_callees.iter().all(|edge| edge.verified_target_symbol));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn symbol_lookup_ranks_type_definitions_before_impl_blocks() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
impl Database {
    pub fn open() -> Self {
        Database
    }
}

pub struct Database;
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let hits = db.symbols("Database", Some(Language::Rust), 10).unwrap();
    assert!(hits.len() >= 2, "fixture should expose both impl and struct symbols: {hits:?}");
    assert_eq!(hits[0].kind, "struct", "Database lookup should prefer type definition");
    assert!(
        hits.iter().any(|hit| hit.kind == "impl"),
        "impl Database should still be available after the struct: {hits:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn distinct_same_named_methods_do_not_merge_and_logical_ids_are_stable() {
    // Two `new` on different impls share a `qualified_name` (`…lib.rs::new`) but differ in
    // signature — they must NOT collapse into one "cfg_variant" logical symbol. And the
    // logical id must be stable across a reindex (it is content-derived, not an autoincrement).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct A;
pub struct B;

impl A {
    pub fn new(name: String) -> Self { A }
}

impl B {
    pub fn new(count: usize, flag: bool) -> Self { B }
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let selector = rag_rat_query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("new".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    };
    let lookup = db.symbol_candidates(&selector, false).unwrap();
    let new_candidates: Vec<_> =
        lookup.candidates.iter().filter(|candidate| candidate.name == "new").collect();
    assert_eq!(new_candidates.len(), 2, "both constructors present: {new_candidates:?}");
    let logical_ids: std::collections::BTreeSet<i64> =
        new_candidates.iter().filter_map(|candidate| candidate.logical_symbol_id).collect();
    assert_eq!(logical_ids.len(), 2, "distinct signatures get distinct logical ids");
    for candidate in &new_candidates {
        assert_eq!(
            candidate.logical_group_reason.as_deref(),
            Some("single"),
            "differently-signed methods are not cfg variants: {candidate:?}"
        );
    }

    // Reindex and confirm the logical ids are unchanged (content-derived, not churned).
    let db = IndexDatabase::rebuild(&config).unwrap();
    let relookup = db.symbol_candidates(&selector, false).unwrap();
    let reindexed_ids: std::collections::BTreeSet<i64> = relookup
        .candidates
        .iter()
        .filter(|candidate| candidate.name == "new")
        .filter_map(|candidate| candidate.logical_symbol_id)
        .collect();
    assert_eq!(reindexed_ids, logical_ids, "logical ids must be stable across reindex");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn logical_symbol_exact_mode_covers_duplicate_rust_variants() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_blocking() {}

#[cfg(target_arch = "wasm32")]
pub fn spawn_blocking() {}

pub fn caller() {
    spawn_blocking();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let lookup = db
        .symbol_candidates(
            &rag_rat_query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: None,
                symbol: Some("spawn_blocking".to_string()),
                language: Some(Language::Rust),
                allow_ambiguous: true,
                limit: 10,
            },
            false,
        )
        .unwrap();
    let logical_symbol_id = lookup.candidates[0].logical_symbol_id.expect("logical id");
    assert_eq!(lookup.candidates[0].logical_variant_count, Some(2));
    assert_eq!(lookup.candidates[0].logical_group_reason.as_deref(), Some("cfg_variant"));

    let exact_variant_callers = db
        .find_callers_with_options(
            "spawn_blocking",
            10,
            &rag_rat_query::graph::GraphTraversalOptions {
                resolution_mode: rag_rat_query::graph::GraphResolutionMode::Exact,
                symbol_id: Some(lookup.candidates[1].symbol_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        exact_variant_callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
                && edge.target.as_deref() == Some("spawn_blocking")
                && edge.verified_target_symbol
        }),
        "symbol_id exact should include its logical cfg group: {exact_variant_callers:?}"
    );
    assert!(exact_variant_callers.iter().all(|edge| edge.verified_target_symbol));

    let exact_logical = db
        .graph_traversal_report(
            "find_callers",
            &lookup.candidates[0],
            true,
            10,
            &rag_rat_query::graph::GraphTraversalOptions {
                resolution_mode: rag_rat_query::graph::GraphResolutionMode::Exact,
                symbol_id: Some(lookup.candidates[0].symbol_id),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(exact_logical.query.logical_symbol_id, Some(logical_symbol_id));
    assert_eq!(exact_logical.logical_symbol.as_ref().map(|symbol| symbol.variant_count), Some(2));
    assert_eq!(exact_logical.variants.len(), 2);
    assert!(exact_logical.results.iter().all(|edge| edge.verified_target_symbol));
    assert!(
        exact_logical.results.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
                && edge.target.as_deref() == Some("spawn_blocking")
        }),
        "logical exact callers: {exact_logical:?}"
    );

    // #201 review (P2): feeding the candidate's `sym_<hex>` handle back through the
    // `ref`/symbol_path slot must resolve to the logical group's members WITHOUT reporting
    // disambiguation — every member shares that one handle, so the client has no more specific
    // token to give. (Before the fix, `disambiguation_required` was computed from the raw
    // 2-candidate count → a dead end.)
    let handle = rag_rat_base::serde_big_id::format_sym_handle(logical_symbol_id);
    let by_ref_handle = db
        .symbol_candidates(
            &rag_rat_query::symbol::SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: Some(handle),
                symbol: None,
                language: None,
                allow_ambiguous: false,
                limit: 10,
            },
            false,
        )
        .unwrap();
    assert_eq!(by_ref_handle.candidates.len(), 2, "handle resolves the whole cfg group");
    assert!(
        by_ref_handle.candidates.iter().all(|c| c.logical_symbol_id == Some(logical_symbol_id)),
        "every member shares the queried handle"
    );
    assert!(
        !by_ref_handle.disambiguation_required,
        "a handle in the ref slot is not ambiguous: {by_ref_handle:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_real_world_rust_graph_patterns() {
    let root = fixture_temp_root("graph-realworld/rust");
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "src/lib.rs", "worker", "imports", "Syntactic");
    assert_edge(&db, "src/lib.rs", "Worker", "exports", "Syntactic");
    // `Worker::new()` / `Client::new()` now resolve via the semantic scope path (`Worker::new` /
    // `Client::new`) → Exact, where bare-name matching previously left two same-named `new` methods
    // ambiguous (NameOnly) (#61 scope-path resolution).
    assert_edge(&db, "entry", "new", "calls_name", "Exact");
    assert_edge(&db, "entry", "Client", "references_type", "Syntactic");
    assert_edge(&db, "drive", "serve", "calls_name", "NameOnly");
    assert_edge(&db, "drive", "GenericRunner", "references_type", "Syntactic");
    assert_edge(&db, "Worker", "Service", "implements", "Syntactic");
    assert_edge(&db, "generic_call", "T", "references_type", "NameOnly");
    assert_edge(&db, "entry", "generated_call", "uses_macro", "NameOnly");
    let syntactic_callers = db.find_callers("serve", 10).unwrap();
    assert!(
        syntactic_callers.is_empty(),
        "syntactic serve callers should avoid receiver/name fallback: {syntactic_callers:?}"
    );
    let callers = db
        .find_callers_with_options("serve", 10, &rag_rat_query::graph::GraphTraversalOptions {
            resolution_mode: rag_rat_query::graph::GraphResolutionMode::Fuzzy,
            ..Default::default()
        })
        .unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.edge_kind == "calls_name"
                && edge.edge_confidence == edge.confidence
                && edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("drive"))
        }),
        "serve callers: {callers:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_typescript_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/helper.ts"),
        "export function helper() {}\nexport const Card = () => null;\n",
    )
    .unwrap();
    fs::write(
        root.join("src/App.tsx"),
        r#"
import { helper, Card } from "./helper";

export function run() {
  helper();
  return <Card />;
}

export const callRun = () => run();
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::TypeScript);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "run", "helper", "calls_name", "Syntactic");
    assert_edge(&db, "run", "Card", "references_type", "Syntactic");
    assert_edge(&db, "src/App.tsx", "helper", "imports", "Syntactic");
    assert_edge(&db, "src/App.tsx", "run", "exports", "Syntactic");
    let callees = db.trace_callees("callRun", 10).unwrap();
    assert!(
        callees.iter().any(|edge| {
            edge.to_symbol.as_deref().is_some_and(|name| name.ends_with("run"))
                && edge.confidence == "syntactic"
        }),
        "callRun callees: {callees:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_swift_symbols_chunks_and_graph_edges_end_to_end() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let source = r#"
import Foundation

protocol Worker<Element> {}

@propertyWrapper
struct Wrapper<Value> {
    var wrappedValue: Value
}

struct Service {}
struct Client {}
struct T {}
struct Vector {}

@Observable struct Model {}
@available(*, deprecated) struct LegacyModel {}

enum Status { case idle }
precedencegroup BasePrecedence {}
precedencegroup SecondaryPrecedence {}
precedencegroup MergePrecedence {
    higherThan: BasePrecedence,
        SecondaryPrecedence
}
infix operator <+>: MergePrecedence
func <+>(lhs: Int, rhs: Int) -> Int { lhs + rhs }
func +(lhs: Vector, rhs: Vector) -> Vector { lhs }

func helper() {}
func identity<T>(_ value: T) -> T { value }

struct Runner: Worker<Service> {
    @Wrapper var count: Int = 0

    func fetch(_ id: Int) -> Int { id }
    func fetch(_ name: String) -> Int { name.count }

    func run() {
        let client = Client()
        let state = Status.idle
        let next: Status = .idle
        let merged = 1 <+> 2
        helper()
    }
}
"#;
    fs::write(
        root.join("src/Macros.swift"),
        "@attached(member) macro Observable() = #externalMacro(module: \"Macros\", type: \
         \"ObservableMacro\")\n",
    )
    .unwrap();
    fs::write(root.join("src/App.swift"), source).unwrap();
    let config = source_config(root.clone(), Language::Swift);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let conn = db.storage.connection();
    let property_names = |name: &str| {
        conn.query_row(
            "SELECT COUNT(*) FROM symbols WHERE kind = 'property' AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(property_names("count"), 1, "property wrapper must not replace the property name");
    assert_eq!(property_names("Wrapper"), 0, "wrapper type must not become the property name");

    let overloads = conn
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT signature) FROM symbols WHERE name = 'fetch'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(overloads, (2, 2), "overloads must retain distinct symbol rows and signatures");
    let overload_chunks = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks WHERE chunk_kind = 'code' AND symbol_path LIKE '%::fetch'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(overload_chunks, 2, "each overload must retain its structural chunk");

    assert_edge(&db, "src/App.swift", "Foundation", "imports", "NameOnly");
    assert_edge(&db, "Runner", "Worker", "implements", "Syntactic");
    assert_edge(&db, "Runner", "run", "contains", "Exact");
    assert_edge(&db, "run", "Client", "constructs", "Syntactic");
    assert_edge(&db, "run", "idle", "calls_name", "Exact");
    assert_edge(&db, "run", "idle", "calls_name", "Syntactic");
    assert_edge(&db, "run", "<+>", "calls_name", "Syntactic");
    assert_edge(&db, "run", "<+>", "uses_operator", "Syntactic");
    assert_edge(&db, "<+>", "MergePrecedence", "uses_precedence_group", "Syntactic");
    assert_edge(&db, "MergePrecedence", "BasePrecedence", "uses_precedence_group", "Syntactic");
    assert_edge(
        &db,
        "MergePrecedence",
        "SecondaryPrecedence",
        "uses_precedence_group",
        "Syntactic",
    );
    assert_edge(&db, "run", "helper", "calls_name", "Syntactic");
    assert_edge(&db, "src/App.swift", "Observable", "uses_macro", "Syntactic");
    assert_edge(&db, "src/App.swift", "Wrapper", "references_type", "Syntactic");
    let false_attribute_macros = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE edge_kind = 'uses_macro' AND to_name IN \
             ('available', 'attached', 'externalMacro')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(false_attribute_macros, 0, "unresolved attributes and compiler hooks are omitted");
    let false_attribute_types = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE edge_kind = 'references_type' AND to_name IN \
             ('available', 'attached', 'externalMacro')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(false_attribute_types, 0, "unresolved attribute type candidates are omitted");
    for (edge_kind, target_kind) in [("calls_name", "function"), ("uses_operator", "operator")] {
        let resolved_kind = conn
            .query_row(
                "
                SELECT symbols.kind
                FROM edges
                JOIN symbols ON symbols.id = edges.to_symbol_id
                WHERE edges.edge_kind = ?1
                  AND edges.to_name = '<+>'
                  AND edges.from_name LIKE '%run%'
                ",
                [edge_kind],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(
            resolved_kind, target_kind,
            "operator calls and declaration uses must resolve independently"
        );
    }
    let operator_function_id = conn
        .query_row("SELECT id FROM symbols WHERE name = '<+>' AND kind = 'function'", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    for resolution_mode in [
        rag_rat_query::graph::GraphResolutionMode::Exact,
        rag_rat_query::graph::GraphResolutionMode::Syntactic,
    ] {
        let callees = db
            .trace_callees_with_options("<+>", 20, &rag_rat_query::graph::GraphTraversalOptions {
                resolution_mode,
                symbol_id: Some(operator_function_id),
                ..Default::default()
            })
            .unwrap();
        assert!(
            callees.iter().all(|edge| {
                edge.edge_kind != "uses_operator" || edge.target.as_deref() != Some("+")
            }),
            "unresolved built-in operators must stay out of {resolution_mode:?} traversal: \
             {callees:?}"
        );
    }
    let resolved_builtin_operator_calls = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE edge_kind = 'calls_name' AND to_name = '+' AND \
             to_symbol_id IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(
        resolved_builtin_operator_calls, 0,
        "built-in operator syntax must not bind to a same-named local overload"
    );
    let generic_type_false_positives = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE edge_kind = 'references_type' AND to_name = 'T'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(
        generic_type_false_positives, 0,
        "generic parameter uses must not resolve to the nominal type T"
    );
    let false_conformances = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE edge_kind = 'implements' AND to_name = 'Service'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(false_conformances, 0, "generic arguments must not become conformances");

    for callee in ["Client", "helper"] {
        let (start, end) = callee_byte_range(
            &db,
            "run",
            callee,
            if callee == "Client" { "constructs" } else { "calls_name" },
        )
        .unwrap_or_else(|| panic!("missing persisted callee range for {callee}"));
        assert_eq!(&source[start as usize..end as usize], callee);
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_c_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/runtime.c"),
        r#"
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
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::C);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "runtime_open", "helper", "calls_name", "Syntactic");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_c_file_scope_macro_regions_for_search() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("drivers/entropy")).unwrap();
    fs::write(
        root.join("drivers/entropy/entropy.c"),
        r#"
static int entropy_init(const struct device *dev)
{
    ARG_UNUSED(dev);
    return 0;
}

/* Entropy driver APIs structure */
static DEVICE_API(entropy, entropy_cryptoacc_trng_api) = {
    .get_entropy = entropy_cryptoacc_trng_get_entropy,
};

DEVICE_DT_INST_DEFINE(0, entropy_init, NULL, NULL, NULL,
                      PRE_KERNEL_1, CONFIG_ENTROPY_INIT_PRIORITY,
                      &entropy_cryptoacc_trng_api);
"#,
    )
    .unwrap();
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "c".to_string(),
            language: Language::C,
            directories: vec![PathBuf::from("drivers/entropy")],
            include: vec!["**/*.c".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let db = IndexDatabase::rebuild(&config).unwrap();

    let hits = db.search("DEVICE_API", 5, false).unwrap();
    assert!(
        hits.iter().any(|hit| {
            hit.path == "drivers/entropy/entropy.c" && hit.summary.contains("DEVICE_API")
        }),
        "DEVICE_API hits: {hits:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_cpp_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/runtime.cpp"),
        r#"
namespace held {
class Runtime {
public:
  void open();
};

void helper() {}

void Runtime::open() {
  helper();
}
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Cpp);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "open", "helper", "calls_name", "Syntactic");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_real_world_typescript_graph_patterns() {
    let root = fixture_temp_root("graph-realworld/typescript");
    let config = source_config(root.clone(), Language::TypeScript);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "src/lib.tsx", "DefaultWidget", "imports", "Syntactic");
    assert_edge(&db, "src/lib.tsx", "WidgetNS", "imports", "NameOnly");
    assert_edge(&db, "src/lib.tsx", "WidgetProps", "imports", "Syntactic");
    assert_edge(&db, "src/lib.tsx", "ReExportedWidget", "exports", "NameOnly");
    assert_edge(&db, "useWidget", "useMemo", "calls_name", "NameOnly");
    assert_edge(&db, "useWidget", "DefaultWidget", "calls_name", "Syntactic");
    assert_edge(&db, "Shell", "renderWidget", "calls_name", "NameOnly");
    assert_edge(&db, "Shell", "WidgetNS", "references_type", "NameOnly");
    assert_edge(&db, "Shell", "DefaultWidget", "references_type", "Syntactic");
    assert_edge(&db, "DefaultWidget", "WidgetProps", "references_type", "Syntactic");
    let callees = db
        .trace_callees_with_options("Shell", 10, &rag_rat_query::graph::GraphTraversalOptions {
            include_references: true,
            edge_kinds: None,
            ..Default::default()
        })
        .unwrap();
    assert!(
        callees.iter().any(|edge| {
            edge.edge_kind == "references_type"
                && edge.edge_confidence == edge.confidence
                && edge.to_symbol.as_deref().is_some_and(|name| name.ends_with("DefaultWidget"))
        }),
        "Shell callees: {callees:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn rust_macro_edges_do_not_resolve_to_same_named_modules() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
mod format;

fn execute_one() {
    let _value = format!("hello");
}
"#,
    )
    .unwrap();
    fs::write(root.join("src/format.rs"), "pub fn helper() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge = db
        .storage
        .connection()
        .query_row(
            "
                SELECT edge_kind, to_name, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE edge_kind = 'uses_macro'
                  AND to_name = 'format'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "uses_macro");
    assert_eq!(edge.1, "format");
    assert_eq!(edge.2, None);
    assert_eq!(edge.3, "NameOnly");
    assert_eq!(edge.4, "unresolved");
    assert!(edge.5.as_deref().is_some_and(|value| value.contains("format!")));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn opening_old_graph_policy_rebuilds_stale_macro_edges() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
mod format;

fn execute_one() {
    let _value = format!("hello");
}
"#,
    )
    .unwrap();
    fs::write(root.join("src/format.rs"), "pub fn helper() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.storage
        .connection()
        .execute("UPDATE repo_meta SET value = 'old' WHERE key = 'graph_index_version'", [])
        .unwrap();
    db.storage
        .connection()
        .execute(
            "
                UPDATE edges
                SET edge_kind = 'calls_name',
                    to_symbol_id = (SELECT id FROM symbols WHERE name = 'format' LIMIT 1),
                    confidence = 'Syntactic',
                    evidence = NULL,
                    resolution = 'syntactic'
                WHERE to_name = 'format'
                ",
            [],
        )
        .unwrap();
    drop(db);

    let reopened = IndexDatabase::open(&config.database).unwrap();
    let edge = reopened
        .storage
        .connection()
        .query_row(
            "
                SELECT edge_kind, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE to_name = 'format'
                  AND edge_kind = 'uses_macro'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "uses_macro");
    assert_eq!(edge.1, None);
    assert_eq!(edge.2, "NameOnly");
    assert_eq!(edge.3, "unresolved");
    assert!(edge.4.as_deref().is_some_and(|value| value.contains("format!")));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn qualified_common_member_calls_do_not_resolve_by_short_name() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct AlertsStore;

impl AlertsStore {
    pub fn new() -> Self {
        Self
    }
}

pub fn caller() {
    let _items: Vec<String> = Vec::new();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge = db
        .storage
        .connection()
        .query_row(
            "
                SELECT to_name, target_qualified_name, to_symbol_id, confidence, resolution
                FROM edges
                WHERE from_name LIKE '%caller'
                  AND edge_kind = 'calls_name'
                  AND to_name = 'new'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "new");
    assert_eq!(edge.1.as_deref(), Some("Vec::new"));
    assert_eq!(edge.2, None);
    assert_eq!(edge.3, "NameOnly");
    assert_eq!(edge.4, "unresolved");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn macro_edges_do_not_resolve_to_same_named_typescript_symbols() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
fn rust_entry() {
    let _payload = json!({"ok": true});
}
"#,
    )
    .unwrap();
    fs::write(root.join("src/preferences.ts"), "export function json() { return {}; }\n").unwrap();
    let mut config = source_config(root.clone(), Language::Rust);
    config.targets.push(ResolvedTarget {
        name: "typescript".to_string(),
        language: Language::TypeScript,
        directories: vec![PathBuf::from("src")],
        include: vec!["**/*.ts".to_string()],
        exclude: Vec::new(),
        kind: TargetKind::Source,
    });
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge = db
        .storage
        .connection()
        .query_row(
            "
                SELECT edge_kind, to_name, to_symbol_id, confidence, resolution, evidence
                FROM edges
                WHERE edge_kind = 'uses_macro'
                  AND to_name = 'json'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "uses_macro");
    assert_eq!(edge.1, "json");
    assert_eq!(edge.2, None);
    assert_eq!(edge.3, "NameOnly");
    assert_eq!(edge.4, "unresolved");
    assert!(edge.5.as_deref().is_some_and(|value| value.contains("json!")));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn qualified_crate_helper_callers_use_name_fallback() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod task_spawn {
    pub fn spawn_blocking() {}
}

pub fn first() {
    crate::task_spawn::spawn_blocking();
}

pub fn second() {
    task_spawn::spawn_blocking();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let callers = db.find_callers("spawn_blocking", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("first"))
                && edge.edge_kind == "calls_name"
                && edge.resolution == "target_name_fallback"
        }),
        "spawn_blocking callers: {callers:?}"
    );
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("second"))
                && edge.edge_kind == "calls_name"
        }),
        "spawn_blocking callers: {callers:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn caller_lookup_does_not_match_related_names_or_chain_evidence() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod runtime {
    pub mod task_spawn {
        pub fn spawn() {}
        pub fn spawn_blocking() -> JoinHandle {
            JoinHandle
        }
        pub fn spawn_blocking_handle() {}
        pub fn spawn_blocking_offload() -> JoinHandle {
            JoinHandle
        }
    }
}

pub struct JoinHandle;

impl JoinHandle {
    pub fn map_err(self) {}
}

pub fn direct() {
    crate::runtime::task_spawn::spawn_blocking();
}

pub fn related_handle() {
    crate::runtime::task_spawn::spawn_blocking_handle();
}

pub fn related_offload_chain() {
    crate::runtime::task_spawn::spawn_blocking_offload().map_err();
}

pub fn related_spawn_with_text() {
    crate::runtime::task_spawn::spawn();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let callers = db.find_callers("spawn_blocking", 20).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("direct"))
                && edge.target.as_deref() == Some("spawn_blocking")
                && edge.edge_kind == "calls_name"
        }),
        "spawn_blocking callers: {callers:?}"
    );
    assert!(
        callers.iter().all(|edge| {
            !edge.from_symbol.as_deref().is_some_and(|name| {
                name.ends_with("related_handle")
                    || name.ends_with("related_offload_chain")
                    || name.ends_with("related_spawn_with_text")
            }) && !matches!(
                edge.target.as_deref(),
                Some("spawn_blocking_handle" | "spawn_blocking_offload" | "spawn" | "map_err")
            )
        }),
        "caller lookup leaked related names or chain evidence: {callers:?}"
    );

    let qualified_callers = db.find_callers("src/lib.rs::spawn_blocking", 20).unwrap();
    assert!(
        qualified_callers.iter().any(|edge| {
            edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("direct"))
                && edge.target.as_deref() == Some("spawn_blocking")
                && edge.edge_kind == "calls_name"
        }),
        "qualified spawn_blocking callers: {qualified_callers:?}"
    );
    assert!(
        qualified_callers.iter().all(|edge| {
            !edge.from_symbol.as_deref().is_some_and(|name| {
                name.ends_with("related_handle")
                    || name.ends_with("related_offload_chain")
                    || name.ends_with("related_spawn_with_text")
            }) && !matches!(
                edge.target.as_deref(),
                Some("spawn_blocking_handle" | "spawn_blocking_offload" | "spawn" | "map_err")
            )
        }),
        "qualified caller lookup leaked related names or chain evidence: {qualified_callers:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn files_past_the_old_structural_cap_still_contribute_symbols_and_edges() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let filler = (0..700).map(|idx| format!("pub fn filler_{idx}() {{}}\n")).collect::<String>();
    fs::write(
        root.join("src/lib.rs"),
        format!(
            r#"
pub mod task_spawn {{
    pub fn spawn_blocking() {{}}
}}

{filler}

pub fn caller() {{
    crate::task_spawn::spawn_blocking();
}}
"#
        ),
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    assert!(fs::metadata(root.join("src/lib.rs")).unwrap().len() > 10_000);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbols = db.symbols("caller", Some(Language::Rust), 10).unwrap();
    assert!(symbols.iter().any(|symbol| symbol.name == "caller"), "caller symbols: {symbols:?}");
    let callers = db.find_callers("spawn_blocking", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.edge_kind == "calls_name"
                && edge.target.as_deref() == Some("spawn_blocking")
                && edge.callsite.as_ref().is_some_and(|callsite| callsite.line > 700)
        }),
        "spawn_blocking callers: {callers:?}"
    );
    let impact =
        db.impact_surface("callers of crate::task_spawn::spawn_blocking in src", 10).unwrap();
    assert!(
        impact.iter().any(|item| {
            item.category == "Direct structural impact" && item.reason == "direct_caller"
        }),
        "impact: {impact:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn impact_surface_uses_high_signal_query_symbols_and_call_edges() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub mod runtime {
    pub fn unrelated_runtime_symbol() {}
}

pub mod task_spawn {
    pub fn spawn_blocking<F, T>(f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        f()
    }
}

pub fn caller() {
    crate::task_spawn::spawn_blocking(|| 1);
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let impact = db
        .impact_surface(
            "change runtime task_spawn spawn_blocking wasm inline native blocking pool",
            20,
        )
        .unwrap();
    assert!(
        impact.iter().any(|item| {
            item.category == "Direct structural impact"
                && item.reason == "direct_caller"
                && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("caller"))
        }),
        "spawn_blocking caller should be present: {impact:?}"
    );
    assert!(
        impact.iter().all(|item| {
            !(item.reason == "exact_symbol_definition"
                && item.symbol.as_deref().is_some_and(|symbol| symbol.ends_with("runtime")))
        }),
        "broad `runtime` token should not become an exact impact seed: {impact:?}"
    );
    assert!(
        impact.iter().all(|item| {
            !item.evidence.iter().any(|evidence| evidence.contains("references_type"))
                && item.symbol.as_deref() != Some("Send")
        }),
        "type references should not appear as direct impact: {impact:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn impact_surface_flat_signals_truncation_when_capped() {
    // #150: the flat (free-text query) impact shape capped at `limit` SILENTLY — a capped result
    // read as complete. A capped result must now carry a visible `completeness` sentinel (the
    // flat-shape analogue of the structured report's `truncated_sections`).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn leaf_a() {}
pub fn leaf_b() {}
pub fn leaf_c() {}
pub fn leaf_d() {}

pub fn hub() {
    leaf_a();
    leaf_b();
    leaf_c();
    leaf_d();
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A tiny limit forces truncation: `hub`'s own definition plus four callee neighbors overflow
    // it.
    let limit = 2u32;
    let impact = db.impact_surface("hub", limit).unwrap();
    let sentinel = impact
        .iter()
        .find(|item| item.category == "completeness")
        .expect("a capped flat result must carry a completeness sentinel (#150)");
    assert!(sentinel.reason.contains("capped"), "sentinel explains the cap: {sentinel:?}");
    assert!(
        sentinel.evidence.iter().any(|evidence| evidence.contains("beyond the limit")),
        "sentinel signals more exist: {sentinel:?}"
    );
    let real_items = impact.iter().filter(|item| item.category != "completeness").count();
    assert_eq!(real_items, limit as usize, "exactly `limit` real items, plus the sentinel");

    // A generous limit drops nothing, so no sentinel appears — it must not cry wolf.
    let full = db.impact_surface("hub", 100).unwrap();
    assert!(
        full.iter().all(|item| item.category != "completeness"),
        "an uncapped result must NOT carry a sentinel: {full:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn impact_surface_flat_signals_truncation_from_a_capped_textual_fallback() {
    // #150 (Codex review): the per-section caps clamp `textual_fallback`/`historical_evidence` to
    // exactly `limit`, so a free-text query with MORE matching files than `limit` used to fill the
    // surface to exactly `limit` and skip the sentinel — silent truncation on the very path the fix
    // targets. The probe-one-past-limit detection must catch it.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // The token `needle_token` appears only in COMMENTS across many files — no symbol is named it,
    // so there are no exact targets / graph neighbors; everything comes from textual fallback.
    for n in 0..6 {
        fs::write(
            root.join(format!("src/file_{n}.rs")),
            format!("// needle_token marker {n}\npub fn unrelated_{n}() {{}}\n"),
        )
        .unwrap();
    }
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let limit = 2u32;
    let impact = db.impact_surface("needle_token", limit).unwrap();
    assert!(
        impact.iter().any(|item| item.category == "completeness"),
        "a capped textual-fallback result must still signal truncation (#150 Codex): {impact:?}"
    );
    let real_items = impact.iter().filter(|item| item.category != "completeness").count();
    assert_eq!(real_items, limit as usize, "exactly `limit` real items, plus the sentinel");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn impact_surface_collapses_file_matches_to_one_row_per_file() {
    // Regression for #48: a file-granularity match (path/chunk text) used to fan out into one
    // row per symbol in the file. Each such section must now yield at most one row per file.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/widget_store.rs"),
        "pub fn widget_alpha() {}\npub fn widget_beta() {}\npub fn widget_gamma() {}\npub fn \
         widget_delta() {}\n",
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let selector = rag_rat_query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some("widget_alpha".to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: false,
        limit: 10,
    };
    let symbol = db.select_symbol(&selector).unwrap().unwrap().expect("symbol");
    let report = db
        .impact_surface_report_for_selected_symbol(
            &symbol,
            50,
            &rag_rat_query::impact::ImpactSurfaceOptions::default(),
        )
        .unwrap();

    for section in [
        &report.text_fallback_hits,
        &report.tests_touching_symbol_path,
        &report.docs_mentioning_symbol_path,
    ] {
        let total = section.len();
        let mut paths: Vec<&str> = section.iter().map(|item| item.path.as_str()).collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), total, "section must have one row per file: {section:?}");

        // Precedence: a path match must not carry a spurious symbol (a qualified name is
        // `path::symbol`, so a path needle matches every symbol in the file).
        for item in section {
            if item.evidence.iter().any(|evidence| evidence.starts_with("path match")) {
                assert!(item.symbol.is_none(), "path match must not name a symbol: {item:?}");
            }
        }
    }

    let store_rows = report
        .text_fallback_hits
        .iter()
        .filter(|item| item.path.ends_with("widget_store.rs"))
        .count();
    assert_eq!(store_rows, 1, "a file with four symbols collapses to one fallback row");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn docs_for_symbol_prefers_local_source_context_before_broad_markdown() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/runtime")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("src/runtime/task_spawn.rs"),
        r#"
pub fn spawn_blocking<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    f()
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("docs/phrase-persistence.md"),
        "# Phrase persistence\nUnrelated notes mention spawn_blocking in passing.\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/task_spawn.md"),
        "# task_spawn\nLocal task_spawn notes explain spawn_blocking.\n",
    )
    .unwrap();
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
            ResolvedTarget {
                name: "markdown".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            },
        ],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let db = IndexDatabase::rebuild(&config).unwrap();
    let symbol = db.symbols("spawn_blocking", Some(Language::Rust), 10).unwrap().remove(0);
    let hits = db.docs_for_selected_symbol(&symbol, 10).unwrap();
    assert_eq!(hits[0].path, "src/runtime/task_spawn.rs", "docs hits: {hits:?}");
    let phrase_index = hits.iter().position(|hit| hit.path == "docs/phrase-persistence.md");
    let task_spawn_index = hits.iter().position(|hit| hit.path == "docs/task_spawn.md");
    assert!(
        phrase_index.is_none_or(|phrase| task_spawn_index.is_some_and(|local| local < phrase)),
        "path-local task_spawn docs should outrank unrelated phrase docs: {hits:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn partial_tree_sitter_trees_still_contribute_valid_symbols_and_edges() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn helper() {}

pub fn caller() {
    helper();
}

fn broken( {
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let symbols = db.symbols("caller", Some(Language::Rust), 10).unwrap();
    assert!(symbols.iter().any(|symbol| symbol.name == "caller"), "caller symbols: {symbols:?}");
    assert_edge(&db, "caller", "helper", "calls_name", "Syntactic");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn receiver_method_calls_do_not_bind_to_same_named_free_functions() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn spawn_blocking() {}

pub fn caller(joinset: JoinSet) {
    joinset.spawn_blocking();
}

pub struct JoinSet;
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let edge = db
        .storage
        .connection()
        .query_row(
            "
                SELECT to_name, target_qualified_name, to_symbol_id, confidence, resolution, \
             receiver_hint
                FROM edges
                WHERE from_name LIKE '%caller'
                  AND edge_kind = 'calls_name'
                  AND to_name = 'spawn_blocking'
                ",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(edge.0, "spawn_blocking");
    assert_eq!(edge.1.as_deref(), Some("joinset::spawn_blocking"));
    assert_eq!(edge.2, None);
    assert_eq!(edge.3, "NameOnly");
    assert_eq!(edge.4, "unresolved");
    assert_eq!(edge.5.as_deref(), Some("joinset"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn trace_callees_excludes_type_references_by_default() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub struct JoinError;
pub enum Result<T, E> { Ok(T), Err(E) }
pub fn helper() {}

pub fn spawn_blocking<F, T>(f: F) -> Result<T, JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    helper();
    tokio::task::spawn_blocking(f)
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let default_callees = db.trace_callees("spawn_blocking", 20).unwrap();
    assert!(
        default_callees.iter().any(|edge| {
            edge.edge_kind == "calls_name"
                && edge.target.as_deref() == Some("helper")
                && edge.verified_target_symbol
        }),
        "default callees: {default_callees:?}"
    );
    assert!(
        default_callees.iter().all(
            |edge| edge.target_qualified_name.as_deref() != Some("tokio::task::spawn_blocking")
        ),
        "default callees leaked unresolved external call: {default_callees:?}"
    );
    assert!(
        default_callees.iter().all(|edge| edge.edge_kind != "references_type"),
        "default callees leaked type refs: {default_callees:?}"
    );
    assert!(
        default_callees.iter().all(|edge| !matches!(
            edge.target.as_deref(),
            Some("F" | "T" | "Send" | "Result" | "JoinError")
        )),
        "default callees leaked generic/type targets: {default_callees:?}"
    );

    let with_refs = db
        .trace_callees_with_options(
            "spawn_blocking",
            20,
            &rag_rat_query::graph::GraphTraversalOptions {
                include_references: true,
                edge_kinds: None,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        with_refs.iter().any(|edge| edge.edge_kind == "references_type"),
        "reference-enabled callees: {with_refs:?}"
    );

    let with_unresolved = db
        .trace_callees_with_options(
            "spawn_blocking",
            20,
            &rag_rat_query::graph::GraphTraversalOptions {
                include_unresolved: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        with_unresolved.iter().any(
            |edge| edge.target_qualified_name.as_deref() == Some("tokio::task::spawn_blocking")
        ),
        "unresolved-enabled callees: {with_unresolved:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn trace_callees_defaults_to_repo_relevant_calls() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub fn repo_helper() {}

pub fn caller(input: Result<String, String>) -> String {
    repo_helper();
    let values: Vec<String> = Vec::new();
    let _ = input.map_err(|error| error.to_string());
    let _ = Some("value").unwrap_or_else(|| "fallback");
    let _ = format!("hello");
    values.get(0).unwrap_or_else(|| "fallback").to_string()
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let default_callees = db.trace_callees("caller", 20).unwrap();
    assert!(
        default_callees.iter().any(|edge| edge.target.as_deref() == Some("repo_helper")),
        "default callees should keep repo-local calls: {default_callees:?}"
    );
    assert!(
        default_callees.iter().all(|edge| {
            edge.edge_kind != "uses_macro"
                && !matches!(
                    edge.target.as_deref(),
                    Some("new" | "map_err" | "unwrap_or_else" | "to_string" | "format")
                )
        }),
        "default callees leaked low-signal calls: {default_callees:?}"
    );

    let expanded = db
        .trace_callees_with_options("caller", 20, &rag_rat_query::graph::GraphTraversalOptions {
            include_unresolved: true,
            include_macros: true,
            include_common_methods: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        expanded.iter().any(|edge| edge.edge_kind == "uses_macro"),
        "macro-enabled callees: {expanded:?}"
    );
    assert!(
        expanded.iter().any(|edge| edge.target.as_deref() == Some("unwrap_or_else")),
        "common-method-enabled callees: {expanded:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_kotlin_graph_edges_from_tree_sitter() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/Main.kt"),
        r#"
package dev.cq27.test

import dev.cq27.lib.ExternalThing

interface Syncable

class MainBridge : Syncable {
  suspend fun syncOnce() {
    helper()
    ExternalThing()
  }
}

fun helper() {}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Kotlin);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "syncOnce", "helper", "calls_name", "Syntactic");
    assert_edge(&db, "MainBridge", "Syncable", "implements", "Syntactic");
    assert_edge(&db, "src/Main.kt", "ExternalThing", "imports", "NameOnly");
    let impact = db.impact_surface("helper", 10).unwrap();
    assert!(
        impact.iter().any(|item| {
            item.category == "Direct structural impact" && item.reason == "direct_caller"
        }),
        "impact: {impact:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn indexes_real_world_kotlin_graph_patterns() {
    let root = fixture_temp_root("graph-realworld/kotlin");
    let config = source_config(root.clone(), Language::Kotlin);
    let db = IndexDatabase::rebuild(&config).unwrap();

    assert_edge(&db, "src/Main.kt", "ExternalFactory", "imports", "NameOnly");
    assert_edge(&db, "Worker", "companion", "contains", "Exact");
    assert_edge(&db, "companion", "create", "contains", "Exact");
    // `Worker.create()` / `SingletonRunner.run()` now resolve via the semantic scope path
    // (`Worker::create` / `SingletonRunner::run`) → Exact, where they were a weaker suffix match
    // before (#61 scope-path resolution).
    assert_edge(&db, "syncOnce", "create", "calls_name", "Exact");
    assert_edge(&db, "syncOnce", "Worker", "references_type", "Syntactic");
    assert_edge(&db, "syncOnce", "run", "calls_name", "Exact");
    assert_edge(&db, "syncOnce", "SingletonRunner", "references_type", "Syntactic");
    assert_edge(&db, "syncOnce", "ExternalFactory", "calls_name", "NameOnly");
    assert_edge(&db, "syncOnce", "ExternalFactory", "references_type", "NameOnly");
    assert_edge(&db, "syncOnce", "cleaned", "calls_name", "Syntactic");
    let callers = db.find_callers("cleaned", 10).unwrap();
    assert!(
        callers.iter().any(|edge| {
            edge.edge_kind == "calls_name"
                && edge.edge_confidence == edge.confidence
                && edge.from_symbol.as_deref().is_some_and(|name| name.ends_with("syncOnce"))
        }),
        "cleaned callers: {callers:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn kotlin_caller_lookup_respects_qualified_receivers_for_common_method_names() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/Main.kt"),
        r#"
package dev.cq27.test

object WatchProposalBuilder {
  fun build(): String = "proposal"
}

class AndroidDialogBuilder {
  fun build(): String = "dialog"
}

fun actualCaller() {
  WatchProposalBuilder.build()
}

fun unrelatedBuilderCalls(dialog: AndroidDialogBuilder) {
  dialog.build()
  AndroidDialogBuilder().build()
}
"#,
    )
    .unwrap();
    let config = source_config(root.clone(), Language::Kotlin);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let target = db
        .symbols("build", Some(Language::Kotlin), 10)
        .unwrap()
        .into_iter()
        .find(|symbol| symbol.qualified_name.contains("WatchProposalBuilder"))
        .expect("WatchProposalBuilder.build symbol");
    let callers = db
        .find_callers_with_options("build", 20, &rag_rat_query::graph::GraphTraversalOptions {
            resolution_mode: rag_rat_query::graph::GraphResolutionMode::Exact,
            symbol_id: Some(target.symbol_id),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        callers
            .iter()
            .filter(|edge| edge
                .from_symbol
                .as_deref()
                .is_some_and(|name| name.ends_with("actualCaller")))
            .count(),
        1,
        "actual caller should be present once: {callers:?}"
    );
    assert!(
        callers.iter().all(|edge| edge
            .from_symbol
            .as_deref()
            .is_none_or(|name| !name.ends_with("unrelatedBuilderCalls"))),
        "unrelated builder calls should not resolve to WatchProposalBuilder.build: {callers:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn papertrail_sync_caches_rationale_without_query_time_crawling() {
    let (root, config) =
        markdown_config("# Decision\nRefs cq27-dev/rag-rat#42\nwe will keep sqlite\n");
    let mut db = IndexDatabase::rebuild(&config).unwrap();
    // Resolve the repo context explicitly so db.rationale_search("Fixes #42") qualifies the bare
    // ref without shelling out to `gh` (#60).
    db.set_papertrail_context(Some("cq27-dev/rag-rat"));
    let mock = MockGitHubClient;

    let offline = sync_from_refs_blocking::<MockGitHubClient>(
        db.storage.connection(),
        &root,
        None,
        true,
        &test_gh_ctx(),
    )
    .unwrap();
    assert!(offline.offline);
    assert_eq!(offline.discovered_refs, 1);
    assert_eq!(offline.synced_items, 0);

    let report =
        sync_from_refs_blocking(db.storage.connection(), &root, Some(&mock), false, &test_gh_ctx())
            .unwrap();
    assert!(!report.offline);
    assert_eq!(report.discovered_refs, 1);
    // One item (the mock change request — no issue-shadow duplication) + 4 unified comments.
    assert_eq!(report.synced_items, 5);
    assert_eq!(report.status.issues, 0);
    assert_eq!(report.status.change_requests, 1);
    assert_eq!(report.status.comments, 4);

    let issue_hits = db.papertrail_issue_search("sqlite", 10).unwrap();
    assert_eq!(issue_hits.len(), 1);
    assert_eq!(issue_hits[0].title, "Decision: keep sqlite");
    assert_eq!(issue_hits[0].evidence_kind, "historical_tracker");

    let refs = db.papertrail_refs_for_path("docs/search.md", 10).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].source_kind, "file");

    let rationale = db.rationale_search("risk", 10).unwrap();
    assert!(rationale.iter().any(|item| item.snippet.contains("live crawling")));
    let issue_ref_rationale = db.rationale_search("Fixes #42", 10).unwrap();
    assert_eq!(issue_ref_rationale.first().map(|item| item.item_key.as_str()), Some("42"));
    assert_eq!(
        issue_ref_rationale.first().map(|item| item.evidence_kind),
        Some("literal_tracker_ref")
    );
    assert_eq!(issue_ref_rationale.first().map(|item| item.score), Some(1.0));
    assert!(
        issue_ref_rationale.iter().any(|item| item.item_key == "42"),
        "issue ref rationale should use structured tracker refs: {issue_ref_rationale:?}"
    );

    let chunk_id = first_chunk_id(&db);
    let papertrail = db.papertrail_for_chunk(chunk_id, 10).unwrap().unwrap();
    assert!(papertrail.current_source.is_some());
    assert!(!papertrail.evidence.is_empty());
    assert!(papertrail.evidence.iter().all(|item| {
        matches!(item.evidence_kind, "historical_tracker" | "literal_tracker_ref")
    }));

    let _ = fs::remove_dir_all(root);
}
