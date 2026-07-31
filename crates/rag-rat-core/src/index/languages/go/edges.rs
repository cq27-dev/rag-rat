//! Go graph-edge extraction for the shared structural edge walk.
//!
//! Node kinds here were read off the pinned tree-sitter-go v0.25.0 `node-types.json` and
//! confirmed against real parse trees, never guessed. The three shapes this file relies on:
//!
//! - `import_spec { name?, path }` — `path` is an `interpreted_string_literal` (`"fmt"`) or a
//!   `raw_string_literal` (`` `fmt` ``); both wrap a single `*_content` child holding the
//!   already-unquoted path, so the content child is the quote-stripping mechanism.
//! - `call_expression { function, arguments }` — `function` is either a bare `identifier`
//!   (`Helper()`) or a `selector_expression { operand, field }` (`fmt.Println()`, `s.Start()`).
//! - `type_identifier` — the single node kind Go uses for every *named* type mention, in every
//!   grammatical position (field type, parameter type, result type, embedded field, composite
//!   literal type, type assertion, `qualified_type`'s tail, …).
//!
//! One consequence worth stating because it is easy to assume otherwise: a generic type-parameter
//! BINDER (`T` in `[T any]`) is a plain `identifier`, so it self-excludes, but a type-parameter
//! USE (`[]T`, `k K`) is an ordinary `type_identifier` and does emit a reference. Extraction stays
//! purely syntactic; deciding that such a target is a local binder is resolution's job.

use std::path::Path;

use tree_sitter::Node;

use super::super::{ReceiverFallback, ResolutionPolicy};
use crate::index::edges::extract::*;
use crate::index::edges::*;

pub(in crate::index::languages) fn go_edges(
    EdgeVisit { text, node, symbols: _, path, locator }: EdgeVisit<'_, '_, '_>,
    emit: &mut EdgeEmitter<'_>,
) {
    let out = emit;
    match node.kind() {
        "import_spec" => go_import_edges(text, node, path, out),
        "call_expression" => go_call_edges(text, node, locator, out),
        "type_identifier" => go_type_reference_edges(text, node, locator, out),
        _ => {},
    }
}

/// `import "fmt"` and the grouped `import ( "fmt" )` form produce the SAME `import_spec` node —
/// the parenthesized group only adds an intervening `import_spec_list`, which the shared
/// pre-order walk descends through. Matching the spec (not the declaration) therefore handles
/// both forms with one arm and yields one edge per imported package.
///
/// Go's import target is a package PATH string, not an identifier, so the edge is file-level:
/// there is no callee token to anchor and no enclosing symbol that "owns" a file's imports —
/// the same reason Swift's `import_declaration` uses `file_edge`.
fn go_import_edges(text: &str, node: Node<'_>, path: &Path, out: &mut EdgeEmitter<'_>) {
    let Some(package) = go_import_path(node, text) else {
        return;
    };
    out.push(file_edge(path, node, text, package, EdgeKind::Imports, EdgeConfidence::NameOnly));
}

/// The unquoted import path of an `import_spec`.
///
/// Both literal kinds expose their body as a single `*_content` named child, so reading that
/// child strips the surrounding quotes/backticks without any string slicing — and correctly
/// yields `None` for the empty literal `""`, whose content child is absent.
fn go_import_path(node: Node<'_>, text: &str) -> Option<String> {
    let literal = node.child_by_field_name("path")?;
    let content = literal.named_child(0)?;
    let package = node_text(content, text);
    (!package.is_empty()).then_some(package)
}

/// A Go call is `f(...)` or `x.f(...)`; the grammar records the callee under the `function`
/// field either way. For the selector form the resolvable name is the `field` — the trailing
/// method/function identifier — while the `operand` is a receiver hint (`fmt` in `fmt.Println`,
/// `s` in `s.Start`). This mirrors how Swift's call extractor keys the edge on the LAST path
/// segment and keeps the leading segment as a hint rather than as the target.
///
/// The receiver hint is only recorded when the operand is a bare `identifier`. A chained or
/// computed operand (`a.b().c()`, `m[k].Do()`) has no single meaningful receiver token, and
/// inventing one would mislead resolution.
fn go_call_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let (name_node, receiver_hint) = match function.kind() {
        "selector_expression" => {
            let Some(field) = function.child_by_field_name("field") else {
                return;
            };
            let receiver = function
                .child_by_field_name("operand")
                .filter(|operand| operand.kind() == "identifier")
                .map(|operand| node_text(operand, text));
            (field, receiver)
        },
        // A bare `identifier` callee is a package-local function; anything else (a call on a
        // parenthesized expression, an immediately-invoked func literal, an index expression)
        // has no name to key an edge on.
        "identifier" => (function, None),
        _ => return,
    };
    let name = node_text(name_node, text);
    if name.is_empty() {
        return;
    }
    out.push(symbol_edge_with_context(
        locator,
        node,
        text,
        name,
        EdgeKind::CallsName,
        EdgeConfidence::NameOnly,
        EdgeContext { target_qualified_name: None, receiver_hint, receiver_type_hint: None },
        Some(CalleeRange::of_node(name_node)),
    ));
}

/// Go funnels every named type mention through one node kind, `type_identifier`, so matching it
/// directly covers all the reference positions at once — struct field types, parameter types,
/// result types, embedded fields, `var`/`const` types, composite-literal types, type assertions
/// and the tail of a `qualified_type` (`pkg.Remote` → `Remote`). This is the same
/// match-the-type-node strategy Rust's extractor uses, and it is strictly more complete than
/// enumerating parent positions by hand.
///
/// The one position that must be EXCLUDED is a declaration's own name: `type Server struct{…}`
/// and `type Alias = Base` both store the declared name as a `type_identifier` under the `name`
/// field of `type_spec` / `type_alias`. Those are definitions, not references — emitting them
/// would give every named type a spurious self-reference. The type being aliased sits under the
/// `type` field of the same parent and is still emitted.
///
/// Type-parameter BINDERS (`T` in `[T any]`) need no exclusion: the grammar makes the binding
/// occurrence a plain `identifier`, so it never reaches this arm. Their USES (`[]T`, `k K`) are
/// `type_identifier`s and are emitted like any other type mention — extraction stays syntactic
/// and lets resolution decide the target is a local binder. Constraints (`any`, `comparable`)
/// arrive as `type_identifier`s too and are likewise emitted, consistent with how other backends
/// treat builtin type names — resolution simply finds no target and drops them.
fn go_type_reference_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    if go_is_declaration_name(node) {
        return;
    }
    let name = node_text(node, text);
    if name.is_empty() {
        return;
    }
    out.push(symbol_edge(
        locator,
        node,
        name,
        EdgeKind::ReferencesType,
        EdgeConfidence::NameOnly,
        Some(CalleeRange::of_node(node)),
    ));
}

/// Whether this `type_identifier` IS the name being declared rather than a type being referenced.
///
/// Checked by node identity against the parent's `name` field, so a declaration whose name and
/// referenced type share spelling (`type Server Server`) still excludes only the declared side.
fn go_is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(parent.kind(), "type_spec" | "type_alias") {
        return false;
    }
    parent.child_by_field_name("name").is_some_and(|name| name.id() == node.id())
}

/// `s.Start()` on `func (s *Server) Start()` resolves through the RECEIVER'S DECLARED TYPE
/// (`Server`), the same shape Rust's `impl Server { fn start() }` method calls resolve through —
/// Go has no runtime value-binding to track beyond that static type, so `Type` (not `TypeAndValue`,
/// Kotlin/Swift's richer value-tracking fallback) is the correct match.
pub(in crate::index::languages) const RESOLVER_POLICY: ResolutionPolicy =
    ResolutionPolicy { receiver_fallback: ReceiverFallback::Type, ..ResolutionPolicy::DEFAULT };

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rag_rat_base::language::Language;

    use super::*;

    fn edges(src: &str) -> Vec<EdgeCandidate> {
        syntactic_edges(Path::new("cmd/app/main.go"), Language::Go, src, &[])
            .expect("Go fixture parses")
    }

    fn has(edges: &[EdgeCandidate], kind: EdgeKind, target: &str) -> bool {
        edges.iter().any(|edge| edge.edge_kind == kind && edge.to_name == target)
    }

    fn find<'a>(edges: &'a [EdgeCandidate], kind: EdgeKind, target: &str) -> &'a EdgeCandidate {
        edges
            .iter()
            .find(|edge| edge.edge_kind == kind && edge.to_name == target)
            .unwrap_or_else(|| panic!("missing {kind:?} edge for {target}: {edges:#?}"))
    }

    fn names(edges: &[EdgeCandidate], kind: EdgeKind) -> Vec<String> {
        edges
            .iter()
            .filter(|edge| edge.edge_kind == kind)
            .map(|edge| edge.to_name.clone())
            .collect()
    }

    fn callee_text<'a>(edge: &EdgeCandidate, src: &'a str) -> Option<&'a str> {
        let span = edge.callee_span?;
        src.get(span.start_byte..span.end_byte)
    }

    #[test]
    fn grouped_and_single_imports_emit_one_unquoted_package_edge_each() {
        // Arrange — the parenthesized group and the bare form in one file, plus a raw-string
        // path and the aliased / blank-identifier spellings.
        let src = concat!(
            "package main\n\n",
            "import (\n",
            "\t\"fmt\"\n",
            "\tstdhttp \"net/http\"\n",
            "\t_ \"embed\"\n",
            "\t`raw/pkg`\n",
            ")\n\n",
            "import \"os\"\n",
        );

        // Act
        let edges = edges(src);

        // Assert — every spec yields its unquoted path, from both the grouped and bare forms.
        let mut imports = names(&edges, EdgeKind::Imports);
        imports.sort();
        assert_eq!(
            imports,
            vec!["embed", "fmt", "net/http", "os", "raw/pkg"],
            "each import_spec must contribute one unquoted package path: {edges:#?}"
        );
    }

    #[test]
    fn an_import_edge_is_file_level_and_keeps_the_quotes_out_of_the_target() {
        // Arrange
        let src = "package main\n\nimport \"net/http\"\n";

        // Act
        let edges = edges(src);

        // Assert — file-level edges hang off the path, not an enclosing symbol.
        let edge = find(&edges, EdgeKind::Imports, "net/http");
        assert_eq!(edge.from_symbol_id, None, "imports are file-level: {edge:#?}");
        assert_eq!(edge.from_name.as_deref(), Some("cmd/app/main.go"));
        assert!(!edge.to_name.contains('"'), "quotes must be stripped: {edge:#?}");
    }

    #[test]
    fn plain_and_selector_calls_both_emit_calls_name_edges() {
        // Arrange
        let src = concat!(
            "package main\n\n",
            "func run(s *Server) {\n",
            "\tHelper()\n",
            "\tfmt.Println(\"hi\")\n",
            "\ts.Start()\n",
            "}\n",
        );

        // Act
        let edges = edges(src);

        // Assert — the callee name is the trailing identifier in every form.
        assert!(has(&edges, EdgeKind::CallsName, "Helper"), "plain call missing: {edges:#?}");
        assert!(
            has(&edges, EdgeKind::CallsName, "Println"),
            "package-qualified call missing: {edges:#?}"
        );
        assert!(has(&edges, EdgeKind::CallsName, "Start"), "method call missing: {edges:#?}");
        assert!(
            !has(&edges, EdgeKind::CallsName, "fmt"),
            "the operand is a receiver, never the callee: {edges:#?}"
        );
    }

    #[test]
    fn a_selector_call_records_its_operand_as_the_receiver_hint() {
        // Arrange
        let src = "package main\n\nfunc run(s *Server) {\n\ts.Start()\n}\n";

        // Act
        let edges = edges(src);

        // Assert
        let edge = find(&edges, EdgeKind::CallsName, "Start");
        assert_eq!(edge.receiver_hint.as_deref(), Some("s"), "receiver hint missing: {edge:#?}");
    }

    #[test]
    fn a_plain_call_has_no_receiver_hint() {
        // Arrange
        let src = "package main\n\nfunc run() {\n\tHelper()\n}\n";

        // Act
        let edges = edges(src);

        // Assert
        let edge = find(&edges, EdgeKind::CallsName, "Helper");
        assert_eq!(edge.receiver_hint, None, "package-local calls have no receiver: {edge:#?}");
    }

    #[test]
    fn a_chained_call_operand_is_not_mistaken_for_a_receiver() {
        // Arrange — the operand of the outer call is itself a call, not a bare identifier.
        let src = "package main\n\nfunc run() {\n\tbuild().Start()\n}\n";

        // Act
        let edges = edges(src);

        // Assert — both callees are still recorded, but the computed operand yields no hint.
        assert!(has(&edges, EdgeKind::CallsName, "build"), "inner call missing: {edges:#?}");
        let outer = find(&edges, EdgeKind::CallsName, "Start");
        assert_eq!(
            outer.receiver_hint, None,
            "a computed operand has no single receiver token: {outer:#?}"
        );
    }

    #[test]
    fn a_callee_with_no_name_token_emits_no_call_edge() {
        // Arrange — an immediately-invoked literal, a parenthesized callee and an index
        // expression are all `call_expression`s whose `function` is neither `identifier` nor
        // `selector_expression`, so there is no name to key an edge on.
        let src = concat!(
            "package main\n\n",
            "func run() {\n",
            "\tfunc() {}()\n",
            "\t(fn)()\n",
            "\thandlers[k]()\n",
            "}\n",
        );

        // Act
        let edges = edges(src);

        // Assert
        assert!(
            names(&edges, EdgeKind::CallsName).is_empty(),
            "a nameless callee must not invent a target: {edges:#?}"
        );
    }

    #[test]
    fn a_conversion_is_emitted_as_a_call_because_the_grammar_cannot_tell_them_apart() {
        // Arrange — `int64(x)` is a type conversion, but Go parses it identically to a call.
        let src = "package main\n\nfunc run(x int) {\n\t_ = int64(x)\n}\n";

        // Act
        let edges = edges(src);

        // Assert — extraction stays syntactic and emits the candidate; resolution finds no
        // matching symbol for a builtin and drops it, exactly as for builtin type names.
        assert!(
            has(&edges, EdgeKind::CallsName, "int64"),
            "conversion syntax is indistinguishable from a call: {edges:#?}"
        );
    }

    #[test]
    fn a_call_edge_anchors_its_callee_range_on_the_callee_token() {
        // Arrange
        let src = "package main\n\nfunc run() {\n\tfmt.Println(\"hi\")\n}\n";

        // Act
        let edges = edges(src);

        // Assert — the range covers only `Println`, not the whole `fmt.Println(...)` call.
        let edge = find(&edges, EdgeKind::CallsName, "Println");
        assert_eq!(callee_text(edge, src), Some("Println"), "wrong callee range: {edge:#?}");
    }

    #[test]
    fn struct_field_parameter_result_and_embedded_types_emit_type_references() {
        // Arrange — one fixture covering each type-reference position that matters.
        let src = concat!(
            "package main\n\n",
            "type Server struct {\n",
            "\tLogger *Logger\n",
            "\tStore  Store\n",
            "\tHandler\n",
            "\tPeer   pkg.Remote\n",
            "}\n\n",
            "func build(cfg Config) (*Server, error) { return nil, nil }\n",
        );

        // Act
        let edges = edges(src);

        // Assert
        assert!(has(&edges, EdgeKind::ReferencesType, "Logger"), "field type: {edges:#?}");
        assert!(has(&edges, EdgeKind::ReferencesType, "Store"), "bare field type: {edges:#?}");
        assert!(has(&edges, EdgeKind::ReferencesType, "Handler"), "embedded field: {edges:#?}");
        assert!(
            has(&edges, EdgeKind::ReferencesType, "Remote"),
            "qualified type resolves to its bare tail: {edges:#?}"
        );
        assert!(has(&edges, EdgeKind::ReferencesType, "Config"), "parameter type: {edges:#?}");
        assert!(has(&edges, EdgeKind::ReferencesType, "Server"), "result type: {edges:#?}");
    }

    #[test]
    fn interface_method_signatures_emit_type_references() {
        // Arrange
        let src = "package main\n\ntype Handler interface {\n\tServe(w Writer) Result\n}\n";

        // Act
        let edges = edges(src);

        // Assert
        assert!(has(&edges, EdgeKind::ReferencesType, "Writer"), "method param: {edges:#?}");
        assert!(has(&edges, EdgeKind::ReferencesType, "Result"), "method result: {edges:#?}");
    }

    #[test]
    fn a_declared_type_name_is_not_a_reference_to_itself() {
        // Arrange — `Celsius` and `Alias` are declarations; `float64` and `Base` are references.
        let src = "package main\n\ntype Celsius float64\n\ntype Alias = Base\n";

        // Act
        let edges = edges(src);

        // Assert
        assert!(
            !has(&edges, EdgeKind::ReferencesType, "Celsius"),
            "a type_spec name is a definition, not a reference: {edges:#?}"
        );
        assert!(
            !has(&edges, EdgeKind::ReferencesType, "Alias"),
            "a type_alias name is a definition, not a reference: {edges:#?}"
        );
        assert!(has(&edges, EdgeKind::ReferencesType, "float64"), "underlying type: {edges:#?}");
        assert!(has(&edges, EdgeKind::ReferencesType, "Base"), "aliased type: {edges:#?}");
    }

    #[test]
    fn a_self_named_underlying_type_excludes_only_the_declared_side() {
        // Arrange — both `type_identifier`s spell `Server`; only the `name` one is a definition.
        let src = "package main\n\ntype Server Server\n";

        // Act
        let edges = edges(src);

        // Assert — identity-based exclusion keeps exactly one reference.
        assert_eq!(
            names(&edges, EdgeKind::ReferencesType),
            vec!["Server".to_string()],
            "only the underlying type is a reference: {edges:#?}"
        );
    }

    #[test]
    fn a_method_receiver_type_is_a_reference() {
        // Arrange — the receiver names the type the method is attached to.
        let src = "package main\n\nfunc (s *Server) Start() error { return nil }\n";

        // Act
        let edges = edges(src);

        // Assert
        assert!(
            has(&edges, EdgeKind::ReferencesType, "Server"),
            "receiver type reference missing: {edges:#?}"
        );
    }

    #[test]
    fn generic_type_parameter_binders_are_not_type_references() {
        // Arrange — `T`/`K` appear only as binders here; `any`/`comparable` are their constraints.
        let src = "package main\n\ntype Stack[T any] struct{}\n\nfunc pick[K comparable]() {}\n";

        // Act
        let edges = edges(src);

        // Assert — the grammar spells a binder name as a plain `identifier`, not a
        // `type_identifier`, so a binding occurrence never reaches the type-reference arm.
        assert!(
            !has(&edges, EdgeKind::ReferencesType, "T"),
            "a type parameter binder is not a reference: {edges:#?}"
        );
        assert!(
            !has(&edges, EdgeKind::ReferencesType, "K"),
            "a type parameter binder is not a reference: {edges:#?}"
        );
        assert!(
            !has(&edges, EdgeKind::ReferencesType, "Stack"),
            "the generic type's own name is a definition: {edges:#?}"
        );
    }

    #[test]
    fn generic_type_parameter_uses_are_type_references() {
        // Arrange — the same `T`/`K` names, now also USED in a field type and a parameter type.
        let src = concat!(
            "package main\n\n",
            "type Stack[T any] struct{ items []T }\n\n",
            "func pick[K comparable](k K) {}\n",
        );

        // Act
        let edges = edges(src);

        // Assert — a use site IS a `type_identifier`, so it emits like any other type mention;
        // resolution, not extraction, decides that the target is a local binder.
        assert!(
            has(&edges, EdgeKind::ReferencesType, "T"),
            "a type parameter use is a reference: {edges:#?}"
        );
        assert!(
            has(&edges, EdgeKind::ReferencesType, "K"),
            "a type parameter use is a reference: {edges:#?}"
        );
    }

    #[test]
    fn a_package_only_file_emits_no_edges() {
        // Arrange
        let src = "package main\n";

        // Act
        let edges = edges(src);

        // Assert
        assert!(edges.is_empty(), "expected no edges, got {edges:#?}");
    }

    #[test]
    fn malformed_source_produces_no_panic() {
        // Arrange — unbalanced delimiters plus NUL/BOM bytes.
        let src = "package !!!\n\nfunc ( { struct interface }}} type = =\n\u{0}\u{feff}";

        // Act
        let edges = edges(src);

        // Assert — bounded error recovery: whatever survives, nothing panics on the way.
        assert!(
            edges.iter().all(|edge| !edge.to_name.is_empty()),
            "no empty edge targets from malformed source: {edges:#?}"
        );
    }
}
