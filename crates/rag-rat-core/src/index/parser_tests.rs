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

fn c_symbols(text: &str) -> Vec<(String, String)> {
    parser::parse_symbols(Path::new("src/types.c"), Language::C, text)
        .unwrap()
        .into_iter()
        .map(|symbol| (symbol.kind, symbol.name))
        .collect()
}

/// The name is read down the `declarator` chain; the parameter lists hang off it and are never
/// searched, so a function returning a function pointer is not named after its parameter.
#[test]
fn c_function_returning_a_function_pointer_is_named_by_its_declarator() {
    assert_eq!(c_symbols("void (*get_handler(int sig))(int) { return 0; }"), vec![(
        "function".to_string(),
        "get_handler".to_string()
    )]);
}

/// An aggregate with no `name` field declares no symbol of its own: a member (`x`) or an
/// enumerator (`A`) is not its name. A typedef still names the type.
#[test]
fn c_anonymous_aggregates_are_not_symbols() {
    assert_eq!(c_symbols("typedef struct { int x; } Pt;"), vec![(
        "type".to_string(),
        "Pt".to_string()
    )]);
    assert_eq!(c_symbols("enum { A, B };"), vec![]);
}

/// A typedef is named by its `declarator` field, not by the first name under it: with a plain
/// named source type, that first name is the aliased type.
#[test]
fn c_typedef_is_named_by_its_declarator() {
    let typedef =
        |text: &str| c_symbols(text).into_iter().map(|(_, name)| name).collect::<Vec<_>>();
    assert_eq!(typedef("typedef MyInt Len;"), vec!["Len"]);
    assert_eq!(typedef("typedef MyInt *LenPtr;"), vec!["LenPtr"]);
    assert_eq!(typedef("typedef Ret (*Handler)(Arg a);"), vec!["Handler"]);
}

fn cpp_symbols(text: &str) -> Vec<(String, String)> {
    parser::parse_symbols(Path::new("src/types.cpp"), Language::Cpp, text)
        .unwrap()
        .into_iter()
        .map(|symbol| (symbol.kind, symbol.name))
        .collect()
}

/// A C++ function name is read along its declarator's `name` fields, so template arguments and
/// scopes are never its name, and an operator is named by its operator.
#[test]
fn cpp_function_is_named_through_template_scope_and_operator_names() {
    let functions = |text: &str| {
        cpp_symbols(text)
            .into_iter()
            .filter(|(kind, _)| kind == "function")
            .map(|(_, name)| name)
            .collect::<Vec<_>>()
    };
    assert_eq!(functions("template<> void foo<Bar>(Bar b) {}"), vec!["foo"]);
    assert_eq!(functions("void ns::run<Gizmo>() {}"), vec!["run"]);
    assert_eq!(functions("Foo& Foo::operator=(const Foo& o) { return *this; }"), vec!["operator="]);
    assert_eq!(functions("Foo::~Foo() {}"), vec!["Foo"]);
    assert_eq!(functions("typedef MyInt Len; void a::b::c() {}"), vec!["c"]);
    assert_eq!(cpp_symbols("typedef MyInt Len;"), vec![("type".to_string(), "Len".to_string())]);
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
    // A pathological deeply-nested file parses fine, but the resulting tree is thousands of nodes
    // deep. The symbol walk must not recurse per node — that overflows the stack on a real worker
    // thread (#520). Run on a deliberately small stack so a per-node recursive walk would overflow
    // HERE, in every language; the iterative walk uses O(1) stack and completes.
    for (language, fixture) in crate::index::languages::test_support::fixtures() {
        let src = fixture.deep_call(8_000);
        let symbols = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || parser::parse_symbols(Path::new(fixture.path), language, &src))
            .expect("spawn walk thread")
            .join()
            .unwrap_or_else(|_| panic!("{language}: the symbol walk overflowed the stack"))
            .expect("parse");
        assert!(
            symbols.iter().any(|symbol| symbol.name == "deep_marker_fn"),
            "{language}: the function symbol survives the deep walk: {symbols:?}",
        );
    }
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

#[test]
fn declarations_beneath_an_error_node_are_recovered_with_their_scopes() {
    for (language, fixture) in crate::index::languages::test_support::fixtures() {
        let parsed =
            parser::parse_file(Path::new(fixture.path), language, fixture.broken_declaration)
                .expect("parse");
        assert!(parsed.has_error, "{language}: the fixture must stay malformed");
        let symbols = parsed
            .symbols
            .iter()
            .map(|symbol| (symbol.kind.as_str(), symbol.scope_path.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(symbols, fixture.broken_declaration_symbols, "{language}");
    }
}

/// The parser folded `class A {` into a top-level ERROR, so `s` and `g` are A's members. Scoped
/// from their ancestors they would read as top-level declarations; they are not recovered. The
/// `${1}` interpolation's closing `}` must not be read as closing A.
#[test]
fn members_of_a_container_whose_header_the_error_swallowed_are_not_recovered() {
    let src = "class A { val s = \"${1}\"\n fun g() {} }\nobject O { fun h() {} }\n";
    let parsed = parser::parse_file(Path::new("s.kt"), Language::Kotlin, src).expect("parse");
    assert!(parsed.root().child(0).is_some_and(|node| node.is_error()), "the file is one ERROR");
    assert!(parsed.symbols.is_empty(), "{:?}", parsed.symbols);
}

/// A method is legal only in a class body. One that parsed whole directly beneath a top-level
/// ERROR is not recovered as a top-level declaration.
#[test]
fn a_declaration_is_not_recovered_where_its_kind_is_illegal() {
    let src = "function broken( { target(); }\nasync m() {}\n";
    let parsed = parser::parse_file(Path::new("s.ts"), Language::TypeScript, src).expect("parse");
    let error = parsed.root().child(0).expect("the top-level ERROR");
    assert!(error.is_error());
    assert!(
        crate::index::edges::named_children(error).any(|node| node.kind() == "method_definition"),
        "the method must parse whole beneath the ERROR for this to test anything",
    );
    assert!(parsed.symbols.is_empty(), "{:?}", parsed.symbols);
}

/// `K`'s type parameter list is missing its `>`, so its header did not parse as written: the class
/// is not recovered, and neither is its method.
#[test]
fn a_declaration_whose_header_is_broken_is_not_recovered() {
    let src = "function broken( { target(); }\nclass K<T { m(){} }\n";
    let parsed = parser::parse_file(Path::new("s.ts"), Language::TypeScript, src).expect("parse");
    assert!(parsed.symbols.is_empty(), "{:?}", parsed.symbols);
}

/// `(kind, scope path)` of each symbol parsed from `src`.
fn symbol_scopes(path: &str, language: Language, src: &str) -> Vec<(String, String)> {
    let parsed = parser::parse_file(Path::new(path), language, src).expect("parse");
    assert!(parsed.has_error, "the source must stay malformed");
    parsed.symbols.iter().map(|symbol| (symbol.kind.clone(), symbol.scope_path.clone())).collect()
}

/// The ERROR folded each class header, and a lambda brace in the header (a constructor argument,
/// a default value) comes before the class body's `{`. That lambda brace is not the container's,
/// so `f` is still read as the folded class's member and is not recovered.
#[test]
fn a_brace_inside_a_folded_container_header_does_not_open_its_body() {
    for src in [
        "class A : Base({ }) { fun f() {} init { } }\n",
        "class A(val cb: () -> Unit = {}) { fun f() {} init { } }\n",
    ] {
        let parsed = parser::parse_file(Path::new("s.kt"), Language::Kotlin, src).expect("parse");
        let error = parsed.root().child(0).expect("the top-level ERROR");
        assert!(error.is_error(), "{src}");
        assert!(
            crate::index::edges::named_children(error)
                .any(|node| node.kind() == "function_declaration"),
            "{src}: `f` must parse whole beneath the ERROR for this to test anything",
        );
        assert!(parsed.symbols.is_empty(), "{src}: {:?}", parsed.symbols);
    }
}

/// `broken` leaves its `(` unclosed, so the folded class `C` is read inside a bracket. Its body's
/// `{` is at the same depth as its `class` keyword, so it still opens `C`, and `m` is not
/// recovered.
#[test]
fn a_container_folded_after_an_unclosed_bracket_still_encloses_its_members() {
    let src = "void b( {\nclass C { void m() {}\n";
    assert!(symbol_scopes("s.cpp", Language::Cpp, src).is_empty());
}

/// The ERROR in `C`'s body swallowed `C`'s closing `}` (the second `}` after `int x;`), so `S`
/// follows the end of `C`: scoped from its ancestors it would read as `C::S`, and it is not
/// recovered. `T`, before that `}`, is `C`'s member and is.
#[test]
fn a_declaration_after_the_error_closes_the_enclosing_container_is_not_recovered() {
    let src = "class C {\nstruct T {\nint x; } } struct S {\n}\n}\n";
    assert_eq!(symbol_scopes("s.cpp", Language::Cpp, src), [
        ("class".to_owned(), "C".to_owned()),
        ("struct".to_owned(), "C::T".to_owned()),
    ]);
}

/// `S` sits in `broken`'s body, which parsed as a node of its own beneath the ERROR. Only direct
/// children of the ERROR are recovered, so `S` is not read as a top-level struct.
#[test]
fn a_declaration_nested_in_parsed_syntax_beneath_the_error_is_not_recovered() {
    let src = "void broken( { struct S { int x; }; }\n";
    assert!(symbol_scopes("s.cpp", Language::Cpp, src).is_empty());
}

/// A stray `}` in a file-root ERROR closes nothing: the file root has no end to reach, so the
/// declarations after it are still recovered.
#[test]
fn a_stray_brace_in_a_file_root_error_does_not_stop_recovery() {
    let src = "function broken( { target(); } }\nclass K { m(){} }\n";
    assert_eq!(symbol_scopes("s.ts", Language::TypeScript, src), [
        ("class".to_owned(), "K".to_owned()),
        ("function".to_owned(), "K::m".to_owned()),
    ]);
}

/// A class expression beneath the ERROR is recovered only as a named class in statement position.
/// An anonymous one has no name of its own, so it would be named after its first method or its
/// `extends` target; one after `(` or `[` is an argument or an element. None of them, nor their
/// methods, is recovered. The named class in statement position still is, including behind a
/// leading `export` or `export default`, whether the parser folds that modifier into an ERROR of
/// its own or leaves it as a bare token beside the class.
#[test]
fn a_class_expression_beneath_the_error_is_recovered_only_as_a_named_statement() {
    for src in [
        "function broken( { target(); }\nclass { m(){} }\n",
        "function broken( { target(); }\nclass extends Base { m(){} }\n",
        "function broken( { target(); }\nfoo(class { m(){} });\n",
        "function broken( { target(); }\nregister(class Handler extends Base { handle(){} });\n",
        "let x = [\nclass { run(){} },\n",
        // A comment is an extra, so it neither ends a statement nor hides the operand position.
        "function broken( { target(); }\nregister( // the handler\nclass Handler extends Base { \
         handle(){} });\n",
        "function broken( { target(); }\nregister(/* cb */ class Handler { handle(){} });\n",
    ] {
        assert!(symbol_scopes("s.ts", Language::TypeScript, src).is_empty(), "{src}");
    }
    for modifier in ["", "export ", "export default "] {
        let src = format!("function broken( {{ target(); }}\n{modifier}class K {{ m(){{}} }}\n");
        assert_eq!(
            symbol_scopes("s.ts", Language::TypeScript, &src),
            [("class".to_owned(), "K".to_owned()), ("function".to_owned(), "K::m".to_owned())],
            "{src}",
        );
    }
    // The broken statement's tokens and the `export` share one inner ERROR, so the bare `export`
    // token is the class's previous sibling.
    let src =
        "function broken( { target(); }\nexport class E { m(){ x(; } }\nclass F { n(){ y(; } }\n";
    assert_eq!(symbol_scopes("s.ts", Language::TypeScript, src), [
        ("class".to_owned(), "E".to_owned()),
        ("function".to_owned(), "E::m".to_owned()),
        ("class".to_owned(), "F".to_owned()),
        ("function".to_owned(), "F::n".to_owned()),
    ]);
}

/// An abstract class declares its name but is no symbol and scopes nothing, so its method is an
/// unscoped `function m`. Recovered beneath an ERROR it keys the same as a clean parse, whether the
/// parser leaves `abstract` as a bare token beside a class expression or keeps the whole
/// `abstract_class_declaration`: a broken line elsewhere must not change its members' keys.
#[test]
fn a_recovered_abstract_class_keys_its_members_as_a_clean_parse_does() {
    let clean = "abstract class Q { m(){} }\n";
    let expected = [("function".to_owned(), "m".to_owned())];
    let parsed = parser::parse_file(Path::new("s.ts"), Language::TypeScript, clean).expect("parse");
    assert!(!parsed.has_error);
    let clean_scopes: Vec<_> = parsed
        .symbols
        .iter()
        .map(|symbol| (symbol.kind.clone(), symbol.scope_path.clone()))
        .collect();
    assert_eq!(clean_scopes, expected);
    for prefix in ["function broken( { target(); }\n", "function broken( { target(); }\nexport "] {
        let src = format!("{prefix}{clean}");
        assert_eq!(symbol_scopes("s.ts", Language::TypeScript, &src), expected, "{src}");
    }
    // A comment between `abstract` and `class` is an extra and does not hide the modifier.
    let src = "function broken( { target(); }\nabstract /* why */ class Q { m(){} }\n";
    assert_eq!(symbol_scopes("s.ts", Language::TypeScript, src), expected, "{src}");
    let src = format!("do\n{clean}class R {{ n(){{}} }}\n");
    assert_eq!(
        symbol_scopes("s.ts", Language::TypeScript, &src),
        [
            ("function".to_owned(), "m".to_owned()),
            ("class".to_owned(), "R".to_owned()),
            ("function".to_owned(), "R::n".to_owned()),
        ],
        "{src}",
    );
}

/// A conditional splits `f` across its branches and the file becomes one ERROR. The `struct` of
/// `f`'s return type and the `class` of its template parameter are container keywords, but each
/// sits in a node the parser built (a bodiless specifier, a template parameter), so neither heads
/// a folded container: `f`'s body brace is not taken for one, and `after` is still recovered.
#[test]
fn a_container_keyword_in_a_parsed_type_does_not_fold_the_next_brace() {
    let split = |header_a: &str, header_b: &str| {
        format!(
            "#ifdef X\n{header_a} {{\n#else\n{header_b} {{\n#endif\n  return 0;\n}}\nint \
             after(void){{ return 0; }}\n"
        )
    };
    for (path, language, src) in [
        ("s.c", Language::C, split("struct S *f(int a)", "struct S *f(int a, int b)")),
        ("s.c", Language::C, split("union U f(int a)", "union U f(int a, int b)")),
        ("s.cpp", Language::Cpp, split("struct S *f(int a)", "struct S *f(int a, int b)")),
        (
            "s.cpp",
            Language::Cpp,
            split("template <class T> T f(T a)", "template <class T> T f(T a, T b)"),
        ),
    ] {
        assert_eq!(
            symbol_scopes(path, language, &src),
            [("function".to_owned(), "after".to_owned())],
            "{src}"
        );
    }
}

/// One source per recovery context whose ERROR sits directly in that context and holds a whole
/// declaration of a kind legal there. Each context in a backend's policy needs a case here: one no
/// real input reaches is policy without evidence behind it. The file-root contexts of TypeScript, C
/// and C++ are the shared fixtures' `broken_declaration` cases, and C++ `field_declaration_list` is
/// `a_declaration_after_the_error_closes_the_enclosing_container_is_not_recovered`.
#[test]
fn every_recovery_context_recovers_a_declaration_with_its_scope() {
    /// `(path, language, context, source, expected (kind, scope path) symbols)`.
    type Case = (
        &'static str,
        Language,
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str)],
    );
    let cases: [Case; 6] = [
        // kotlin-ng cannot parse two declarations on one line: the first lands in an ERROR.
        ("s.kt", Language::Kotlin, "source_file", "fun a() {} fun b() {}\nfun c() {}\n", &[
            ("function", "a"),
            ("function", "b"),
            ("function", "c"),
        ]),
        // As above, inside a class whose body parsed as a `class_body`.
        ("s.kt", Language::Kotlin, "class_body", "class A : B {\n  fun f() {} fun g() {}\n}\n", &[
            ("class", "A"),
            ("function", "A::f"),
            ("function", "A::g"),
        ]),
        // Kotlin's `enum_class_body` case is the shared fixture's `class A { fun f() {} }`.
        ("s.kt", Language::Kotlin, "enum_class_body", "class A { fun f() {} }\n", &[
            ("class", "A"),
            ("function", "A::f"),
        ]),
        // The stray `x(` puts the struct after it in an ERROR inside the namespace body.
        (
            "s.cpp",
            Language::Cpp,
            "declaration_list",
            "namespace N {\nint a() { return 0; }\nx(struct S { int x; };\nint b() { return 0; \
             }\n}\n",
            &[("namespace", "N"), ("function", "N::a"), ("struct", "N::S"), ("function", "N::b")],
        ),
        // A TypeScript namespace body is a `statement_block` beneath `internal_module`.
        (
            "s.ts",
            Language::TypeScript,
            "statement_block",
            "namespace N {\n  function broken( { target(); }\n  class K { m(){} }\n}\n",
            &[("class", "N::K"), ("function", "N::K::m")],
        ),
        // A `declare module` body is a `statement_block` beneath `module`.
        (
            "s.ts",
            Language::TypeScript,
            "statement_block",
            "declare module M {\n  function broken( { target(); }\n  class K { m(){} }\n}\n",
            &[("class", "M::K"), ("function", "M::K::m")],
        ),
    ];
    for (path, language, context, src, expected) in cases {
        let parsed = parser::parse_file(Path::new(path), language, src).expect("parse");
        let mut stack = vec![parsed.root()];
        let mut error_in_context = false;
        while let Some(node) = stack.pop() {
            error_in_context |=
                node.is_error() && node.parent().is_some_and(|parent| parent.kind() == context);
            stack.extend(crate::index::edges::named_children(node));
        }
        assert!(error_in_context, "{src}: the ERROR must sit in `{context}` for this to test it");
        let symbols = parsed
            .symbols
            .iter()
            .map(|symbol| (symbol.kind.as_str(), symbol.scope_path.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(symbols, expected, "{src}");
    }
}

/// A function body is a `statement_block` too, but not a namespace body: the class the ERROR in
/// it holds is a local, and it is not recovered as one of the file's declarations.
#[test]
fn a_statement_block_is_a_recovery_context_only_as_a_namespace_body() {
    let src = "function f() {\n  function broken( { target(); }\n  class K { m(){} }\n}\n";
    assert_eq!(symbol_scopes("s.ts", Language::TypeScript, src), [(
        "function".to_owned(),
        "f".to_owned()
    )]);
}

/// An error at the file's first token makes the ERROR the tree's root, with no file-root node
/// above it. It is judged as sitting in the file root, so the declarations after it are recovered.
#[test]
fn declarations_beneath_an_error_at_the_tree_root_are_recovered() {
    type Case = (&'static str, Language, &'static str, &'static [(&'static str, &'static str)]);
    let cases: [Case; 4] = [
        ("s.c", Language::C, "void b( {\nint after(void){ return 0; }\n", &[("function", "after")]),
        // A template is whole though its function's body has an error, like the plain function.
        (
            "s.cpp",
            Language::Cpp,
            "void b( {\ntemplate <typename T> T id(T x) { return x +; }\nint plain(int x) { \
             return x +; }\n",
            &[("function", "id"), ("function", "plain")],
        ),
        ("s.cpp", Language::Cpp, "void b( {\nint after(void){ return 0; }\n", &[(
            "function", "after",
        )]),
        ("s.ts", Language::TypeScript, "f(1,\nexport class K { m(){} }\n", &[
            ("class", "K"),
            ("function", "K::m"),
        ]),
    ];
    for (path, language, src, expected) in cases {
        let parsed = parser::parse_file(Path::new(path), language, src).expect("parse");
        assert!(parsed.root().is_error(), "{src}: the ERROR must be the tree's root");
        let symbols = parsed
            .symbols
            .iter()
            .map(|symbol| (symbol.kind.as_str(), symbol.scope_path.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(symbols, expected, "{src}");
    }
}

/// Most top-level TypeScript is `export`ed or a variable declaration. An `export` is recovered
/// when the declaration it exports is legal where it sits, and a `const` like any declaration.
/// Neither wrapper has a body of its own: an error inside the exported declaration's body, or the
/// body of the arrow function a `const` holds, does not reject it; one outside every body does.
#[test]
fn an_export_or_a_variable_declaration_beneath_the_error_is_recovered() {
    let src = "do\nexport function after(){}\nexport class E { m(){} }\nconst c = 1;\n";
    let parsed = parser::parse_file(Path::new("s.ts"), Language::TypeScript, src).expect("parse");
    let error = parsed.root().child(0).expect("the top-level ERROR");
    assert!(error.is_error());
    assert!(
        crate::index::edges::named_children(error).any(|node| node.kind() == "lexical_declaration"),
        "the declaration must sit directly beneath the ERROR for this to test anything",
    );
    assert_eq!(symbol_scopes("s.ts", Language::TypeScript, src), [
        ("function".to_owned(), "after".to_owned()),
        ("class".to_owned(), "E".to_owned()),
        ("function".to_owned(), "E::m".to_owned()),
        ("const".to_owned(), "c".to_owned()),
    ]);
    let src = "do\nexport function after(){ y(; }\nexport class E { m(){ y(; } }\nconst c = () => \
               { let s = ; };\n";
    assert_eq!(symbol_scopes("s.ts", Language::TypeScript, src), [
        ("function".to_owned(), "after".to_owned()),
        ("class".to_owned(), "E".to_owned()),
        ("function".to_owned(), "E::m".to_owned()),
        ("const".to_owned(), "c".to_owned()),
        ("const".to_owned(), "s".to_owned()),
    ]);
    let src = "do\nconst a = () => {}, b = f(;\n";
    assert!(symbol_scopes("s.ts", Language::TypeScript, src).is_empty(), "{src}");
}
