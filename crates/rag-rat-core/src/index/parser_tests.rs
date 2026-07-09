use std::path::Path;

use crate::index::parser::{self, ParserKind};
use crate::language::Language;

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
