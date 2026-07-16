use std::path::Path;

use rag_rat_base::language::Language;

use crate::index::parser::{self, ParserKind};

#[test]
fn extracts_rust_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/lib.rs");
    let symbols = parser::parse_symbols(Path::new("src/lib.rs"), Language::Rust, text).unwrap();
    assert_symbol(&symbols, "function", "open_database");
    assert_symbol(&symbols, "const", "MAX_OPEN_DATABASES");
    assert_symbol(&symbols, "static", "DEFAULT_DATABASE_NAME");
    assert_symbol(&symbols, "type", "DatabaseId");
    assert_symbol(&symbols, "macro", "database_event");
    assert_symbol(&symbols, "module", "handles");
    assert_symbol(&symbols, "struct", "DatabaseHandle");
    assert_symbol(&symbols, "impl", "DatabaseHandle");
    assert_symbol(&symbols, "function", "id");
    assert_symbol(&symbols, "enum", "DatabaseState");
    assert_symbol(&symbols, "trait", "DatabaseLifecycle");
}

#[test]
fn extracts_rust_uniffi_export_symbol_facts() {
    let text = r#"
#[uniffi::export]
pub fn exported_fn() {}

#[cfg_attr(not(target_arch = "wasm32"), uniffi::export(async_runtime = "tokio"))]
impl Runtime {
    pub fn route_search_query(&self) {}
}

pub struct Runtime;

/// Not #[uniffi::export]: this is an internal helper.
pub fn internal_helper() {}
"#;
    let symbols = parser::parse_symbols(Path::new("src/lib.rs"), Language::Rust, text).unwrap();
    assert_symbol_fact(&symbols, "function", "exported_fn", "rust_attr", "uniffi_export");
    assert_symbol_fact(&symbols, "impl", "Runtime", "rust_attr", "uniffi_export");
    assert_no_symbol_fact(&symbols, "function", "internal_helper", "rust_attr", "uniffi_export");
}

#[test]
fn extracts_typescript_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/index.ts");
    let symbols =
        parser::parse_symbols(Path::new("src/index.ts"), Language::TypeScript, text).unwrap();
    assert_eq!(
        parser::parser_kind(Path::new("src/index.ts"), Language::TypeScript),
        ParserKind::TypeScript
    );
    assert_symbol(&symbols, "function", "openDatabase");
    assert_symbol(&symbols, "type", "BridgeState");
    assert_symbol(&symbols, "interface", "BridgeConfig");
    assert_symbol(&symbols, "class", "BridgeClient");
    assert_symbol(&symbols, "function", "open");
    assert_symbol(&symbols, "const", "bridgeName");
    assert_symbol(&symbols, "const", "useBridge");
    assert_symbol(&symbols, "const", "BridgeBadge");
}

#[test]
fn extracts_tsx_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/App.tsx");
    let symbols =
        parser::parse_symbols(Path::new("src/App.tsx"), Language::TypeScript, text).unwrap();
    assert_eq!(
        parser::parser_kind(Path::new("src/App.tsx"), Language::TypeScript),
        ParserKind::Tsx
    );
    assert_symbol(&symbols, "function", "HeldStatusCard");
    assert_symbol(&symbols, "const", "useHeldStatus");
}

#[test]
fn extracts_kotlin_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/Main.kt");
    let symbols = parser::parse_symbols(Path::new("src/Main.kt"), Language::Kotlin, text).unwrap();
    assert_symbol(&symbols, "class", "MainBridge");
    assert_symbol(&symbols, "property", "bridgeName");
    assert_symbol(&symbols, "function", "openDatabase");
    assert_symbol(&symbols, "function", "syncOnce");
    assert_symbol(&symbols, "object", "companion");
    assert_symbol(&symbols, "property", "DEFAULT_NAME");
    assert_symbol(&symbols, "function", "create");
    assert_symbol(&symbols, "object", "BridgeRegistry");
    assert_symbol(&symbols, "property", "active");
}

#[test]
fn extracts_swift_symbols_and_nested_scope_paths() {
    let text = r#"
import Foundation

protocol Repository: Sendable {
    associatedtype Item
    var count: Int { get }
    func load(id: Int) async throws -> Item
}

extension Repository {
    func cached(id: Int) async throws -> Item { try await load(id: id) }
}

actor Store<T>: Repository {
    typealias Item = T
    @Wrapper(source: "fixture") private var values: [T] = []

    init(seed: T) { values = [seed] }
    deinit {}
    subscript(index: Int) -> T { values[index] }

    func load(id: Int) async throws -> T { values[id] }
}

extension Store {
    func mapped<U>(_ transform: (T) -> U) -> [U] { values.map(transform) }
}

class BaseService {}

class Service: BaseService {
    struct Request {
        let id: Int
    }

    func fetch(_ id: Int) {}
    func fetch(_ name: String) {}
}

enum AppState { case idle, failed(Error), running }

precedencegroup MergePrecedence {
    associativity: left
    higherThan: AdditionPrecedence
}
infix operator <+>: MergePrecedence
prefix operator /* built-in operator comment */ !

struct Client {
    func run() async {
        func local() {}
        local()
    }
}
"#;
    let symbols = parser::parse_symbols(Path::new("Sources/App/App.swift"), Language::Swift, text)
        .expect("Swift fixture parses");
    assert_eq!(
        parser::parser_kind(Path::new("Sources/App/App.swift"), Language::Swift),
        ParserKind::Swift
    );
    assert_symbol(&symbols, "protocol", "Repository");
    assert_symbol(&symbols, "type", "Item");
    assert_symbol(&symbols, "property", "count");
    assert_symbol(&symbols, "function", "load");
    assert!(
        symbols
            .iter()
            .any(|symbol| symbol.name == "cached" && symbol.scope_path == "Repository::cached"),
        "protocol extension method should carry the extended protocol: {symbols:#?}"
    );
    assert_symbol(&symbols, "actor", "Store");
    assert_symbol(&symbols, "property", "values");
    assert_no_symbol(&symbols, "property", "source");
    assert_symbol(&symbols, "constructor", "init");
    assert_symbol(&symbols, "function", "deinit");
    assert_symbol(&symbols, "function", "subscript");
    assert_symbol(&symbols, "extension", "extension Store");
    assert_no_symbol(&symbols, "extension", "Store");
    assert_symbol(&symbols, "function", "mapped");
    assert_symbol(&symbols, "class", "Service");
    assert!(
        symbols
            .iter()
            .any(|symbol| symbol.name == "Request" && symbol.scope_path == "Service::Request"),
        "nested nominal types should carry their enclosing type: {symbols:#?}"
    );
    assert_eq!(
        symbols.iter().filter(|symbol| symbol.name == "fetch").count(),
        2,
        "both overloads must remain independently indexed: {symbols:#?}"
    );
    let int_fetch = symbols
        .iter()
        .find(|symbol| {
            symbol.name == "fetch"
                && symbol.signature.as_deref() == Some("func fetch(_ id: Int) {}")
        })
        .unwrap_or_else(|| panic!("missing exact Swift signature: {symbols:#?}"));
    assert_eq!(
        text.get(int_fetch.start_byte..int_fetch.end_byte),
        Some("func fetch(_ id: Int) {}"),
        "Swift symbol byte range must cover the complete declaration"
    );
    assert_eq!(int_fetch.start_line, int_fetch.end_line);
    assert_symbol(&symbols, "enum", "AppState");
    let enum_cases = ["idle", "failed", "running"].map(|case| {
        let symbol = symbols
            .iter()
            .find(|symbol| {
                symbol.kind == "enum_case"
                    && symbol.name == case
                    && symbol.scope_path == format!("AppState::{case}")
            })
            .unwrap_or_else(|| {
                panic!("enum case {case} should carry its enclosing enum scope: {symbols:#?}")
            });
        assert_eq!(
            text.get(symbol.start_byte..symbol.end_byte),
            Some(case),
            "multi-case declarations need identifier-precise symbol spans"
        );
        assert_eq!(
            symbol.signature.as_deref(),
            Some("case idle, failed(Error), running"),
            "each case retains the complete declaration signature"
        );
        (symbol.start_byte, symbol.end_byte)
    });
    assert_eq!(
        enum_cases.into_iter().collect::<std::collections::HashSet<_>>().len(),
        3,
        "each case needs a distinct source span"
    );
    assert_symbol(&symbols, "precedence_group", "MergePrecedence");
    assert_symbol(&symbols, "operator", "<+>");
    assert_symbol(&symbols, "operator", "!");
    assert_symbol(&symbols, "struct", "Client");
    assert!(
        symbols.iter().any(|symbol| symbol.name == "load" && symbol.scope_path == "Store::load"),
        "actor method should carry its enclosing type: {symbols:#?}"
    );
    assert!(
        symbols
            .iter()
            .any(|symbol| symbol.name == "local" && symbol.scope_path == "Client::run::local"),
        "nested function should carry type and function scopes: {symbols:#?}"
    );
}

#[test]
fn extracts_each_swift_property_binding_with_a_unique_span() {
    let text = r#"
struct Size {
    let depth: Int
    let width, height: Int
    var x = 0, y = 0
}
let (row, column) = (1, 2)
let (x: a, y: (b, c)) = (x: 1, y: (2, 3))
"#;
    let symbols = parser::parse_symbols(Path::new("Sources/App/Size.swift"), Language::Swift, text)
        .expect("Swift fixture parses");

    for name in ["width", "height", "x", "y", "row", "column", "a", "b", "c"] {
        let symbol = symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing property {name}: {symbols:#?}"));
        let expected_scope = if matches!(name, "row" | "column" | "a" | "b" | "c") {
            name.to_string()
        } else {
            format!("Size::{name}")
        };
        assert_eq!(symbol.scope_path, expected_scope);
        assert!(
            symbol.signature.as_deref().is_some_and(|signature| signature.contains(name)),
            "property signature should retain its complete declaration: {symbol:#?}"
        );
    }

    let mut spans = symbols
        .iter()
        .filter(|symbol| {
            ["width", "height", "x", "y", "row", "column", "a", "b", "c"]
                .contains(&symbol.name.as_str())
        })
        .map(|symbol| (symbol.start_byte, symbol.end_byte))
        .collect::<Vec<_>>();
    spans.sort_unstable();
    spans.dedup();
    assert_eq!(spans.len(), 9, "each binding must own a unique chunk span: {symbols:#?}");
    assert_eq!(
        symbols.iter().filter(|symbol| matches!(symbol.name.as_str(), "x" | "y")).count(),
        2,
        "tuple labels must not be indexed as additional bindings: {symbols:#?}"
    );

    let depth = symbols.iter().find(|symbol| symbol.name == "depth").unwrap();
    assert_eq!(
        text.get(depth.start_byte..depth.end_byte),
        Some("let depth: Int"),
        "single-binding properties should retain their complete declaration chunk"
    );
}

#[test]
fn swift_local_declarations_include_special_member_scopes() {
    let text = r#"
struct Collection {
    init() { func validateInit() {} }
    deinit { func validateDeinit() {} }
    subscript(index: Int) -> Int {
        func validateSubscript() {}
        return index
    }
}
"#;
    let symbols =
        parser::parse_symbols(Path::new("Sources/App/Collection.swift"), Language::Swift, text)
            .expect("Swift fixture parses");

    for (name, scope) in [
        ("validateInit", "Collection::init::validateInit"),
        ("validateDeinit", "Collection::deinit::validateDeinit"),
        ("validateSubscript", "Collection::subscript::validateSubscript"),
    ] {
        assert!(
            symbols.iter().any(|symbol| symbol.name == name && symbol.scope_path == scope),
            "missing {scope}: {symbols:#?}"
        );
    }
}

#[test]
fn swift_macro_definition_body_is_not_a_second_symbol() {
    let text = r#"
macro stringify<T>(_ value: T) = #externalMacro(module: "Macros", type: "StringifyMacro")
"#;
    let symbols =
        parser::parse_symbols(Path::new("Sources/App/Macros.swift"), Language::Swift, text)
            .expect("Swift fixture parses");

    assert_eq!(
        symbols.iter().filter(|symbol| symbol.kind == "macro").count(),
        1,
        "only the outer macro declaration should be indexed: {symbols:#?}"
    );
    assert_symbol(&symbols, "macro", "stringify");
    assert_no_symbol(&symbols, "macro", "module");
}

/// Swift test symbols are recognized by their FRAMEWORK, not just by living under a `Tests/` path:
/// swift-testing's `@Test`/`@Suite` attributes and XCTest's `XCTestCase` inheritance. The fixture
/// path here deliberately has NO test segment (`Sources/App/…`), so a passing assertion can only
/// come from symbol-level detection — an XCTestCase beside the code it exercises would otherwise be
/// indexed as production source and never demoted in search or `repo_brief`.
#[test]
fn swift_test_symbols_are_detected_by_framework_not_only_by_path() {
    let text = r#"
import Testing
import XCTest

@Test func checksTheThing() {}

@Suite struct ClientSuite {
    @Test func checksAnother() {}
}

class ClientTests: XCTestCase {
    func testFetchSucceeds() {}
}

// An XCTestCase whose name carries NO `Tests`/`TestCase` suffix: its members are still test code,
// and only an ancestor walk can see that (their scope path root is just `LoginFlow`).
class LoginFlow: XCTestCase {
    func testLogin() {}
    func makeFixture() -> Int { 1 }
}

@TestHarness struct NotATest {}

func realWork() -> Int { 1 }
"#;
    let symbols =
        parser::parse_symbols(Path::new("Sources/App/Client.swift"), Language::Swift, text)
            .expect("Swift fixture parses");

    let is_test = |name: &str| {
        symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("missing symbol {name}: {symbols:#?}"))
            .is_test
    };

    assert!(is_test("checksTheThing"), "@Test function is a test symbol");
    assert!(is_test("ClientSuite"), "@Suite type is a test symbol");
    assert!(is_test("checksAnother"), "@Test method inside a suite is a test symbol");
    assert!(is_test("ClientTests"), "an XCTestCase subclass is a test symbol");
    assert!(is_test("testFetchSucceeds"), "a test* method of an XCTestCase is a test symbol");
    // Members of an XCTestCase are test code even when the class name carries no `Tests` suffix —
    // the scope path alone cannot tell (`LoginFlow::testLogin`), so this needs the ancestor walk.
    assert!(is_test("LoginFlow"), "an XCTestCase named without a Tests suffix is still a test");
    assert!(is_test("testLogin"), "a test method of a suffix-less XCTestCase is a test symbol");
    assert!(is_test("makeFixture"), "a HELPER inside an XCTestCase is test scaffolding too");
    // Neither a lookalike attribute nor ordinary code is a test.
    assert!(!is_test("NotATest"), "@TestHarness is not @Test");
    assert!(!is_test("realWork"), "production code in a non-test path stays production code");
}

#[test]
fn qualified_swift_extension_members_use_canonical_scope_paths() {
    let text = r#"
enum API { struct Request {} }
extension API.Request {
    func decode() {}
}
"#;
    let symbols =
        parser::parse_symbols(Path::new("Sources/App/Request.swift"), Language::Swift, text)
            .expect("Swift fixture parses");

    let decode = symbols
        .iter()
        .find(|symbol| symbol.name == "decode")
        .unwrap_or_else(|| panic!("missing extension member: {symbols:#?}"));
    assert_eq!(decode.scope_path, "API::Request::decode");
}

#[test]
fn extracts_python_symbols() {
    let text = include_str!("../../../../tests/fixtures/held-mini/src/Main.py");
    let symbols = parser::parse_symbols(Path::new("src/Main.py"), Language::Python, text).unwrap();
    assert_eq!(parser::parser_kind(Path::new("src/Main.py"), Language::Python), ParserKind::Python);
    assert_symbol(&symbols, "class", "Api");
    // A decorator (`@classmethod` / `@property`) must NOT hide the inner method symbol.
    assert_symbol(&symbols, "function", "from_url");
    assert_symbol(&symbols, "function", "host");
    assert_symbol(&symbols, "function", "make");
    // SCREAMING_SNAKE_CASE module assignment is a constant…
    assert_symbol(&symbols, "const", "DEFAULT_TIMEOUT");
    // …but a lowercase assignment is NOT — we don't flood the symbol table with every local.
    assert_no_symbol(&symbols, "const", "default_retries");
    // `adapter` (a lowercase local inside `from_url`) is likewise not a symbol.
    assert_no_symbol(&symbols, "const", "adapter");
    // And `LOCAL_MAX` — SCREAMING_SNAKE but a FUNCTION-local — is not a constant either: the const
    // rule is gated to module/class scope.
    assert_no_symbol(&symbols, "const", "LOCAL_MAX");

    // A decorated def's symbol span includes its decorator line (so `@classmethod` etc. — often the
    // API surface — is in the chunk), not just the bare `def`.
    let from_url = symbols.iter().find(|s| s.name == "from_url").unwrap();
    let decorator_line = text.lines().position(|l| l.trim() == "@classmethod").unwrap() + 1;
    assert_eq!(
        from_url.start_line, decorator_line,
        "decorated symbol span should start at the @classmethod line"
    );
    // …but the SIGNATURE is the `def` declaration, not the `@classmethod` decorator (it feeds
    // logical-symbol member hashing + memory anchoring, which must key on the declaration).
    assert_eq!(
        from_url.signature.as_deref(),
        Some("def from_url(cls, url: str) -> \"Api\":"),
        "decorated signature must be the def line, not the decorator"
    );
}

#[test]
fn extracts_python_type_alias() {
    // PEP 695 `type X = …` is indexed as a type symbol (like Rust/TS/C++ aliases).
    let symbols =
        parser::parse_symbols(Path::new("src/a.py"), Language::Python, "type UserId = int\n")
            .unwrap();
    assert_symbol(&symbols, "type", "UserId");
}

#[test]
fn extracts_kotlin_kdoc_without_closing_delimiter_residue() {
    let text = r#"
/**
 * Builds a proposal.
 */
class WatchProposalBuilder {
    /**
     * Builds the current proposal.
     */
    suspend fun build() {}
}
"#;
    let symbols = parser::parse_symbols(Path::new("src/Main.kt"), Language::Kotlin, text).unwrap();
    let class_docs =
        symbols.iter().find(|symbol| symbol.name == "WatchProposalBuilder").unwrap().docs.as_ref();
    assert_eq!(class_docs.map(String::as_str), Some("Builds a proposal."));
    let function_docs = symbols.iter().find(|symbol| symbol.name == "build").unwrap().docs.as_ref();
    assert_eq!(function_docs.map(String::as_str), Some("Builds the current proposal."));
}

#[test]
fn extracts_c_symbols() {
    let text = r#"
#include <stdio.h>

typedef struct Runtime Runtime;

struct Runtime {
    int state;
};

enum RuntimeState {
    RuntimeOpen,
};

int runtime_open(Runtime *runtime) {
    return runtime->state;
}

int runtime_close(Runtime *runtime);

#define runtime_debug(value) value
"#;
    let symbols = parser::parse_symbols(Path::new("src/runtime.c"), Language::C, text).unwrap();
    assert_eq!(parser::parser_kind(Path::new("src/runtime.c"), Language::C), ParserKind::C);
    assert_symbol(&symbols, "struct", "Runtime");
    assert_symbol(&symbols, "enum", "RuntimeState");
    assert_symbol(&symbols, "function", "runtime_open");
    assert_symbol(&symbols, "macro", "runtime_debug");
    // `int runtime_close(Runtime *runtime);` is a bare prototype (declaration), not a definition —
    // not indexed (#61). Only `function_definition`s are. The `typedef struct Runtime Runtime;`
    // also references `struct Runtime` bodyless, but the `struct Runtime { … }` definition is what
    // supplies the indexed `struct Runtime` symbol above.
    assert_no_symbol(&symbols, "function", "runtime_close");
}

/// #61: C/C++ index type DEFINITIONS, not forward declarations or uses. A bodyless `struct X;`
/// (forward decl) and `struct X *p` (use) must NOT produce a symbol — only `struct X { … }` does —
/// so a `references_type` edge resolves to the real definition, not a tiny bodyless occurrence.
#[test]
fn c_forward_declarations_and_uses_are_not_symbols() {
    let text = r#"
struct Defined { int field; };
struct Forward;
union UForward;
enum EForward;

struct Forward *use_forward(struct Defined *d) {
    return (struct Forward *)d;
}
"#;
    let symbols = parser::parse_symbols(Path::new("src/types.c"), Language::C, text).unwrap();
    // The definition (has a body) is indexed.
    assert_symbol(&symbols, "struct", "Defined");
    // Forward declarations (no body) and the bodyless uses of `Forward` are not.
    assert_no_symbol(&symbols, "struct", "Forward");
    assert_no_symbol(&symbols, "union", "UForward");
    assert_no_symbol(&symbols, "enum", "EForward");
}

#[test]
fn extracts_cpp_symbols() {
    let text = r#"
#include <memory>

namespace held {
class Runtime {
public:
    Runtime();
    void open();
};

struct RuntimeConfig {
    int workers;
};

using RuntimePtr = std::shared_ptr<Runtime>;

void Runtime::open() {}
}
"#;
    let symbols = parser::parse_symbols(Path::new("src/runtime.cpp"), Language::Cpp, text).unwrap();
    assert_eq!(parser::parser_kind(Path::new("src/runtime.cpp"), Language::Cpp), ParserKind::Cpp);
    assert_symbol(&symbols, "namespace", "held");
    assert_symbol(&symbols, "class", "Runtime");
    assert_symbol(&symbols, "struct", "RuntimeConfig");
    assert_symbol(&symbols, "type", "RuntimePtr");
    assert_symbol(&symbols, "function", "open");
}

#[test]
fn markdown_uses_no_tree_sitter_symbols() {
    assert_eq!(
        parser::parser_kind(Path::new("docs/search.md"), Language::Markdown),
        ParserKind::Markdown
    );
    let symbols =
        parser::parse_symbols(Path::new("docs/search.md"), Language::Markdown, "# Search").unwrap();
    assert!(symbols.is_empty());
}

fn assert_symbol(symbols: &[parser::ParsedSymbol], kind: &str, name: &str) {
    assert!(
        symbols.iter().any(|symbol| symbol.kind == kind && symbol.name == name),
        "missing {kind} {name}; got {:?}",
        symbols.iter().map(|symbol| (&symbol.kind, &symbol.name)).collect::<Vec<_>>()
    );
}

/// #61: `scope_path` encodes the enclosing semantic scope (module + impl type), ending in the
/// symbol's own name — the resolution key that aligns with an edge's source-derived
/// `target_qualified_name`. A top-level item's scope_path is just its name.
#[test]
fn scope_path_encodes_enclosing_module_and_impl_type() {
    let text = "\
mod core {
    pub struct Client;
    impl Client {
        pub fn new() -> Self { Self }
    }
}
pub fn entry() {}
";
    let symbols = parser::parse_symbols(Path::new("src/lib.rs"), Language::Rust, text).unwrap();
    let scope_of = |name: &str, kind: &str| {
        symbols
            .iter()
            .find(|s| s.name == name && s.kind == kind)
            .map(|s| s.scope_path.as_str())
            .unwrap_or("<missing>")
    };
    assert_eq!(
        scope_of("new", "function"),
        "core::Client::new",
        "method carries module + impl type"
    );
    assert_eq!(scope_of("Client", "struct"), "core::Client", "type carries its module");
    assert_eq!(scope_of("entry", "function"), "entry", "a top-level item is just its name");
}

fn assert_no_symbol(symbols: &[parser::ParsedSymbol], kind: &str, name: &str) {
    assert!(
        !symbols.iter().any(|symbol| symbol.kind == kind && symbol.name == name),
        "unexpected {kind} {name}; got {:?}",
        symbols.iter().map(|symbol| (&symbol.kind, &symbol.name)).collect::<Vec<_>>()
    );
}

fn assert_symbol_fact(
    symbols: &[parser::ParsedSymbol],
    kind: &str,
    name: &str,
    fact_kind: &str,
    fact_value: &str,
) {
    let symbol = symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| panic!("missing {kind} {name}: {symbols:?}"));
    assert!(
        symbol.facts.iter().any(|fact| fact.kind == fact_kind && fact.value == fact_value),
        "missing fact {fact_kind}={fact_value} on {kind} {name}; got {:?}",
        symbol.facts
    );
}

fn assert_no_symbol_fact(
    symbols: &[parser::ParsedSymbol],
    kind: &str,
    name: &str,
    fact_kind: &str,
    fact_value: &str,
) {
    let symbol = symbols
        .iter()
        .find(|symbol| symbol.kind == kind && symbol.name == name)
        .unwrap_or_else(|| panic!("missing {kind} {name}: {symbols:?}"));
    assert!(
        !symbol.facts.iter().any(|fact| fact.kind == fact_kind && fact.value == fact_value),
        "unexpected fact {fact_kind}={fact_value} on {kind} {name}; got {:?}",
        symbol.facts
    );
}

#[test]
fn deeply_nested_input_does_not_overflow_the_symbol_walk() {
    // A pathological deeply-nested file (thousands of `{` blocks) parses fine, but the resulting
    // tree is thousands of nodes deep. The symbol walk must not recurse per node — that overflows
    // the stack on a real worker thread (#520). Run on a deliberately small stack so a per-node
    // recursive walk would overflow HERE; the iterative walk uses O(1) stack and completes.
    let depth = 8_000;
    let src = format!("fn deep_marker_fn() {}{}\n", "{".repeat(depth), "}".repeat(depth));
    let symbols = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || parser::parse_symbols(Path::new("deep.rs"), Language::Rust, &src))
        .expect("spawn walk thread")
        .join()
        .expect("the symbol walk must not overflow the stack on deeply-nested input")
        .expect("parse");
    assert!(
        symbols.iter().any(|symbol| symbol.name == "deep_marker_fn"),
        "the function symbol survives the deep walk",
    );
}

/// #543 tripwire: EVERY function under `src/index/` that both recurses (references its own name)
/// AND descends via `named_children` / `.children()` must wrap its recursion in `grow_stack`.
/// Point-wrapping known helpers kept missing new ones; this enforces the invariant at test time so
/// a newly-added tree-sitter helper can't silently reintroduce the stack-overflow class. Dogfoods
/// `parse_symbols` to split functions (no hand brace-matching), and walks the whole `index/` tree
/// so a new `edges/extract/<lang>.rs` is covered automatically.
///
/// KNOWN LIMITATIONS (this catches the common direct-recursion mistake, not every conceivable
/// shape): mutual recursion `A -> B -> A` where neither references its own name evades it — the two
/// intentional wrapper/`_impl` splits here are that shape, both verified `grow_stack`-guarded; and
/// a recurser that descends ONLY via `.child(i)` / `named_child(i)` / `goto_first_child` /
/// `child_by_field_name` (no `named_children`/`.children()` loop) is not seen as descending. The
/// paren-callee regression test is the end-to-end backstop.
#[test]
fn every_recursive_tree_descender_grows_the_stack() {
    // Whole-word occurrence of `name` in `body` — matches a direct call `name(` AND a
    // function-pointer reference `.any(name)` / `find_map(.. name)` (no trailing `(`). Word
    // boundaries on both sides so `foo` matches neither `foo_bar` nor `xfoo`.
    fn references_self(body: &str, name: &str) -> bool {
        let bytes = body.as_bytes();
        let is_word = |b: u8| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_');
        let mut from = 0;
        while let Some(rel) = body[from..].find(name) {
            let at = from + rel;
            let end = at + name.len();
            let before_ok = at == 0 || !is_word(bytes[at - 1]);
            let after_ok = end >= bytes.len() || !is_word(bytes[end]);
            if before_ok && after_ok {
                return true;
            }
            from = at + name.len();
        }
        false
    }

    fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}")) {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    let index_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/index");
    let mut files = Vec::new();
    rs_files(&index_root, &mut files);
    assert!(files.len() > 20, "index/ walk found only {} files; wrong root?", files.len());

    let mut offenders = Vec::new();
    for path in &files {
        let rel =
            path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(path).display().to_string();
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        let symbols = parser::parse_symbols(path, Language::Rust, &src).expect("parse source");
        for symbol in symbols.iter().filter(|s| s.kind == "function") {
            let span = &src[symbol.start_byte..symbol.end_byte];
            // Body only (skip the signature, which contains the function's own name).
            let body = span.split_once('{').map(|(_, rest)| rest).unwrap_or(span);
            let recursive = references_self(body, &symbol.name);
            let descends = body.contains("named_children") || body.contains(".children(");
            if recursive && descends && !body.contains("grow_stack(") {
                offenders.push(format!("{rel}::{}", symbol.name));
            }
        }
    }
    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "recursive tree descenders missing a grow_stack wrap (a deeply-nested source file \
         overflows the indexer stack via these — wrap the recursion in \
         rag_rat_base::stack::grow_stack, #543):\n{offenders:#?}",
    );
}
