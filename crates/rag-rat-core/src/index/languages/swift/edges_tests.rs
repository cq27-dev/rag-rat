use std::path::Path;

use rag_rat_base::language::Language;

use super::*;

fn edges(src: &str) -> Vec<EdgeCandidate> {
    syntactic_edges(Path::new("Sources/App/App.swift"), Language::Swift, src, &[])
        .expect("Swift fixture parses")
}

fn has(edges: &[EdgeCandidate], kind: EdgeKind, target: &str) -> bool {
    edges.iter().any(|edge| edge.edge_kind == kind && edge.to_name == target)
}

fn callee_text<'a>(edge: &EdgeCandidate, src: &'a str) -> Option<&'a str> {
    let span = edge.callee_span?;
    src.get(span.start_byte..span.end_byte)
}

fn source_text<'a>(edge: &EdgeCandidate, src: &'a str) -> Option<&'a str> {
    let start = usize::try_from(edge.source_span.start_byte).ok()?;
    let end = usize::try_from(edge.source_span.end_byte).ok()?;
    src.get(start..end)
}

#[test]
fn extracts_import_calls_construction_conformance_and_exact_callee_ranges() {
    let src = r#"
import Foundation

protocol Worker<Element> {}

class Parent {}
class Child: Parent {}

struct Runner: Worker<Service> {
    func run(service: Service?) async {
        let client = Client()
        await client.fetch(id: 1) { value in print(value) }
        service?.ping()
    }
}
"#;
    let edges = edges(src);
    assert!(has(&edges, EdgeKind::Imports, "Foundation"), "import missing: {edges:#?}");
    assert!(has(&edges, EdgeKind::Implements, "Worker"), "conformance missing: {edges:#?}");
    assert!(
        !has(&edges, EdgeKind::Implements, "Service"),
        "generic arguments are type references, not conformances: {edges:#?}"
    );
    assert!(has(&edges, EdgeKind::Implements, "Parent"), "inheritance missing: {edges:#?}");
    assert!(has(&edges, EdgeKind::Constructs, "Client"), "construction missing: {edges:#?}");
    assert!(has(&edges, EdgeKind::ReferencesType, "Service"), "type ref missing: {edges:#?}");

    for callee in ["Client", "fetch", "ping"] {
        let edge = edges
            .iter()
            .find(|edge| edge.to_name == callee)
            .unwrap_or_else(|| panic!("missing {callee} edge: {edges:#?}"));
        assert_eq!(callee_text(edge, src), Some(callee), "wrong callee range for {callee}");
        let source = source_text(edge, src)
            .unwrap_or_else(|| panic!("missing source range for {callee}: {edges:#?}"));
        assert!(
            source.contains(callee),
            "source range for {callee} must cover its callee: {source:?}"
        );
    }

    let fetch = edges
        .iter()
        .find(|edge| edge.to_name == "fetch")
        .unwrap_or_else(|| panic!("missing awaited fetch edge: {edges:#?}"));
    assert!(
        source_text(fetch, src).is_some_and(|source| source.contains("client.fetch")),
        "await must preserve the underlying call-expression source range: {fetch:#?}"
    );
    assert_eq!(fetch.target_qualified_name, None, "value receivers resolve by callee name");
    assert_eq!(fetch.receiver_hint.as_deref(), Some("client"));
}

#[test]
fn generic_declarations_are_not_type_references_and_outer_types_keep_their_names() {
    let src = r#"
struct T {}
struct Namespace { struct T {} }
struct Box<Element> {}

func transform<T: Codable, U>(_ value: Box<T>, _ qualified: Namespace.T) -> Box<U> { value }
"#;
    let edges = edges(src);
    let references = |target: &str| {
        edges
            .iter()
            .filter(|edge| edge.edge_kind == EdgeKind::ReferencesType && edge.to_name == target)
            .count()
    };

    assert_eq!(references("Element"), 0, "generic declarations are not uses: {edges:#?}");
    assert_eq!(references("T"), 1, "only Namespace.T is a nominal reference: {edges:#?}");
    assert_eq!(references("U"), 0, "generic return types are lexical bindings: {edges:#?}");
    assert_eq!(references("Codable"), 1, "generic constraints remain type uses: {edges:#?}");
    assert_eq!(references("Box"), 2, "generic arguments must not replace Box: {edges:#?}");
    assert!(
        edges.iter().any(|edge| {
            edge.edge_kind == EdgeKind::ReferencesType
                && edge.to_name == "T"
                && edge.target_qualified_name.as_deref() == Some("Namespace::T")
        }),
        "qualified nominal types are not lexical generic references: {edges:#?}"
    );
    assert!(
        edges
            .iter()
            .filter(|edge| edge.to_name == "Box")
            .all(|edge| { callee_text(edge, src) == Some("Box") }),
        "outer type edges must retain exact Box ranges: {edges:#?}"
    );
}

#[test]
fn operator_and_enum_case_expressions_emit_callable_edges() {
    let src = r####"
enum Status { case idle, failed(Error) }
precedencegroup BasePrecedence {}
precedencegroup SecondaryPrecedence {}
precedencegroup MergePrecedence {
    // } must not close the declaration scanner.
    // note { higherThan: BasePrecedence, SecondaryPrecedence
    higherThan: BasePrecedence,
        SecondaryPrecedence
    /* higherThan: BasePrecedence, SecondaryPrecedence */
    associativity: left
}
precedencegroup InlinePrecedence { higherThan: BasePrecedence, SecondaryPrecedence }
precedencegroup LowerPrecedence { lowerThan: BasePrecedence }
precedencegroup Métrique { higherThan: Élément, `class` }
precedencegroup Élément {}
precedencegroup `class` {}
precedencegroup Broken
let malformedApostrophe = 'x'
precedencegroup GoodPrecedence { higherThan: BasePrecedence, SecondaryPrecedence }
let relationString = "higherThan: BasePrecedence, SecondaryPrecedence"
let rawRelation = #"ignored " precedencegroup Fake { higherThan: BasePrecedence, SecondaryPrecedence }"#
let rawMultiline = ##"""
precedencegroup AlsoFake { higherThan: BasePrecedence, SecondaryPrecedence }
"""##
/*
higherThan: BasePrecedence, SecondaryPrecedence
*/
infix operator <+>: MergePrecedence
prefix operator !
func <+>(lhs: Int, rhs: Int) -> Int { lhs + rhs }

let merged = lhs <+> rhs
let inverted = !flag
let qualified = Status.idle
let shorthand: Status = .idle
let qualifiedPayload = Status.failed(error)
let shorthandPayload: Status = .failed(error)
let unrelated = client.idle
"####;
    let edges = edges(src);
    for operator in ["<+>", "!"] {
        for kind in [EdgeKind::CallsName, EdgeKind::UsesOperator] {
            let edge = edges
                .iter()
                .find(|edge| edge.edge_kind == kind && edge.to_name == operator)
                .unwrap_or_else(|| {
                    panic!("missing {kind:?} operator edge to {operator}: {edges:#?}")
                });
            assert_eq!(callee_text(edge, src), Some(operator));
        }
    }
    let precedence = edges
        .iter()
        .find(|edge| {
            edge.edge_kind == EdgeKind::UsesPrecedenceGroup && edge.to_name == "MergePrecedence"
        })
        .unwrap_or_else(|| panic!("missing precedence-group dependency: {edges:#?}"));
    assert_eq!(callee_text(precedence, src), Some("MergePrecedence"));
    let group_dependencies = edges
        .iter()
        .filter(|edge| {
            edge.edge_kind == EdgeKind::UsesPrecedenceGroup && edge.to_name == "BasePrecedence"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        group_dependencies.len(),
        4,
        "valid declarations each emit once while malformed and raw decoys do not: {edges:#?}"
    );
    assert!(group_dependencies.iter().all(|edge| callee_text(edge, src) == Some("BasePrecedence")));
    let secondary_dependencies = edges
        .iter()
        .filter(|edge| {
            edge.edge_kind == EdgeKind::UsesPrecedenceGroup && edge.to_name == "SecondaryPrecedence"
        })
        .collect::<Vec<_>>();
    assert_eq!(secondary_dependencies.len(), 3, "all valid list forms emit once: {edges:#?}");
    let recovered = edges
        .iter()
        .filter(|edge| {
            edge.edge_kind == EdgeKind::UsesPrecedenceGroup
                && edge.evidence.as_deref().is_some_and(|evidence| evidence.contains(','))
        })
        .collect::<Vec<_>>();
    assert_eq!(recovered.len(), 8, "comments and strings must not create dependencies");
    assert!(recovered.iter().all(|edge| {
        callee_text(edge, src) == Some(edge.to_name.as_str())
            && source_text(edge, src)
                .is_some_and(|source| source.contains("higherThan:") && source.len() < src.len())
    }));
    for (owner, dependency) in [("Métrique", "Élément"), ("Métrique", "`class`")] {
        let edge = edges
            .iter()
            .find(|edge| {
                edge.edge_kind == EdgeKind::UsesPrecedenceGroup && edge.to_name == dependency
            })
            .unwrap_or_else(|| panic!("missing {owner} -> {dependency}: {edges:#?}"));
        assert_eq!(callee_text(edge, src), Some(dependency));
    }
    assert!(edges.iter().all(|edge| !matches!(edge.to_name.as_str(), "Fake" | "AlsoFake")));

    let cases = |name: &str| {
        edges
            .iter()
            .filter(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == name)
            .collect::<Vec<_>>()
    };
    let idle = cases("idle");
    assert_eq!(idle.len(), 2, "qualified and shorthand cases each emit once: {edges:#?}");
    assert!(
        idle.iter().any(|edge| {
            edge.target_qualified_name.as_deref() == Some("Status::idle")
                && edge.receiver_hint.as_deref() == Some("Status")
        }),
        "qualified cases retain enum context: {edges:#?}"
    );
    assert!(
        idle.iter().all(|edge| callee_text(edge, src) == Some("idle")),
        "case edges retain exact leaf ranges: {edges:#?}"
    );
    assert_eq!(
        cases("failed").len(),
        2,
        "qualified and shorthand associated-value cases each emit once: {edges:#?}"
    );
}

#[test]
fn forced_unwraps_are_not_operator_calls_but_other_postfix_tokens_are() {
    let src = r#"
postfix operator ++
postfix func ++(value: Int) -> Int { value }

let unwrapped = maybe!
let transformed = value++
"#;
    let edges = edges(src);
    for kind in [EdgeKind::CallsName, EdgeKind::UsesOperator] {
        assert!(
            edges.iter().any(|edge| {
                edge.edge_kind == kind
                    && edge.to_name == "++"
                    && callee_text(edge, src) == Some("++")
            }),
            "non-bang postfix token must emit {kind:?}: {edges:#?}"
        );
        assert!(
            !edges.iter().any(|edge| edge.edge_kind == kind && edge.to_name == "!"),
            "force unwrap must not emit {kind:?}: {edges:#?}"
        );
    }
}

#[test]
fn local_receiver_calls_keep_their_qualified_roots() {
    let src = r#"
class Parent { class func make() {} }
class Store: Parent {
    class func make() {
        Self.make()
        self.make()
        super.make()
    }
}
"#;
    let edges = edges(src);
    for receiver in ["Self", "self", "super"] {
        let qualified = format!("{receiver}::make");
        let call = edges
            .iter()
            .find(|edge| {
                edge.edge_kind == EdgeKind::CallsName
                    && edge.target_qualified_name.as_deref() == Some(qualified.as_str())
            })
            .unwrap_or_else(|| panic!("missing {receiver}.make call: {edges:#?}"));
        assert_eq!(call.receiver_hint.as_deref(), Some(receiver));
        assert_eq!(callee_text(call, src), Some("make"));
    }
}

#[test]
fn attributed_imports_only_name_the_imported_module() {
    let src = r#"
@testable import App
@_exported import Foo.Bar
"#;
    let edges = edges(src);
    assert!(has(&edges, EdgeKind::Imports, "App"), "testable import missing: {edges:#?}");
    assert!(has(&edges, EdgeKind::Imports, "Foo.Bar"), "re-export import missing: {edges:#?}");
    assert!(
        !edges.iter().any(|edge| {
            edge.edge_kind == EdgeKind::Imports
                && (edge.to_name.contains("testable") || edge.to_name.contains("_exported"))
        }),
        "import modifiers must not enter module names: {edges:#?}"
    );
    assert!(
        !has(&edges, EdgeKind::ReferencesType, "testable")
            && !has(&edges, EdgeKind::ReferencesType, "_exported"),
        "import attributes are not type references: {edges:#?}"
    );
    assert!(
        !has(&edges, EdgeKind::UsesMacro, "testable")
            && !has(&edges, EdgeKind::UsesMacro, "_exported"),
        "import modifiers are not attached macro uses: {edges:#?}"
    );
}

#[test]
fn enum_raw_types_are_references_while_protocols_remain_conformances() {
    let src = r#"
protocol Codable {}
enum Status: String, Codable { case ready = "ready" }
enum Direction: Int { case north, south }
enum Empty: String {}
enum Plain: Codable { case ready }
enum Qualified: Domain.String { case ready }
enum Outer: Codable {
    enum Inner: String { case value = "value" }
}
"#;
    let edges = edges(src);
    assert!(
        has(&edges, EdgeKind::ReferencesType, "String"),
        "raw enum type must be a type reference: {edges:#?}"
    );
    assert!(
        !edges.iter().any(|edge| {
            edge.edge_kind == EdgeKind::Implements
                && edge.to_name == "String"
                && edge.target_qualified_name.is_none()
        }),
        "unqualified raw enum types must not become conformances: {edges:#?}"
    );
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.edge_kind == EdgeKind::Implements && edge.to_name == "Codable")
            .count(),
        3,
        "protocol conformances on raw, plain, and nested enums remain visible: {edges:#?}"
    );
    assert!(
        edges.iter().any(|edge| {
            edge.edge_kind == EdgeKind::Implements
                && edge.target_qualified_name.as_deref() == Some("Domain::String")
        }),
        "an arbitrary qualified String is a conformance without raw-value evidence: {edges:#?}"
    );
    for raw_type in ["Int", "String"] {
        assert!(
            has(&edges, EdgeKind::ReferencesType, raw_type),
            "implicit and empty raw enums retain {raw_type} type edges: {edges:#?}"
        );
    }
}

#[test]
fn bracket_syntax_emits_subscript_calls_with_receiver_context() {
    let src = r#"
let value = store[id]
let staticValue = Store[id]
func generic<T>(_ index: Int) {
    _ = T[index]
    _ = Namespace.T[index]
}
"#;
    let edges = edges(src);
    let subscripts = edges
        .iter()
        .filter(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == "subscript")
        .collect::<Vec<_>>();
    assert_eq!(subscripts.len(), 3, "eligible bracket expressions emit one call: {edges:#?}");
    assert!(subscripts.iter().any(|edge| {
        edge.receiver_hint.as_deref() == Some("store") && edge.target_qualified_name.is_none()
    }));
    assert!(subscripts.iter().any(|edge| {
        edge.receiver_hint.as_deref() == Some("Store")
            && edge.target_qualified_name.as_deref() == Some("Store::subscript")
    }));
    assert!(subscripts.iter().any(|edge| {
        edge.receiver_hint.as_deref() == Some("Namespace")
            && edge.target_qualified_name.as_deref() == Some("Namespace::T::subscript")
    }));
    assert!(
        !subscripts.iter().any(|edge| {
            edge.receiver_hint.as_deref() == Some("T")
                && edge.target_qualified_name.as_deref() == Some("T::subscript")
        }),
        "a lexical generic root must not bind to a nominal subscript: {edges:#?}"
    );
}

#[test]
fn generic_calls_and_constructor_expressions_keep_the_constructed_type() {
    let src = r#"
struct Box<T> {}
protocol DefaultInit {}

func make() {
    let box = Box<Int>()
    let array = Array<String>()
    let dictionary = [String: Int]()
}

func makeGeneric<T: DefaultInit>() -> T {
    let direct = T()
    let explicit = T.init()
    return direct
}
"#;
    let edges = edges(src);
    for target in ["Box", "Array", "Dictionary"] {
        let edge = edges
            .iter()
            .find(|edge| edge.edge_kind == EdgeKind::Constructs && edge.to_name == target)
            .unwrap_or_else(|| panic!("missing {target} construction: {edges:#?}"));
        assert!(
            source_text(edge, src).is_some_and(|source| source.contains('(')),
            "construction source must cover the initializer call: {edge:#?}"
        );
    }
    assert!(
        !has(&edges, EdgeKind::Constructs, "Int")
            && !has(&edges, EdgeKind::Constructs, "String")
            && !has(&edges, EdgeKind::Constructs, "T"),
        "generic arguments are not constructed call targets: {edges:#?}"
    );
    assert!(
        !edges.iter().any(|edge| {
            matches!(edge.edge_kind, EdgeKind::CallsName | EdgeKind::Constructs)
                && edge.to_name == "T"
        }),
        "generic initializer spellings must not emit callable edges: {edges:#?}"
    );
    let box_edge = edges
        .iter()
        .find(|edge| edge.edge_kind == EdgeKind::Constructs && edge.to_name == "Box")
        .unwrap();
    assert_eq!(callee_text(box_edge, src), Some("Box"));
}

#[test]
fn explicit_init_calls_construct_the_qualifying_type() {
    let src = r#"
func make() {
    let client = Client.init()
    let qualified = Module.Service.init()
    client.init()
    Self.init()
    self.init()
    super.init()
}
"#;
    let edges = edges(src);
    for target in ["Client", "Service"] {
        let edge = edges
            .iter()
            .find(|edge| edge.edge_kind == EdgeKind::Constructs && edge.to_name == target)
            .unwrap_or_else(|| panic!("missing {target} construction: {edges:#?}"));
        assert_eq!(callee_text(edge, src), Some(target));
    }
    assert!(
        has(&edges, EdgeKind::CallsName, "init"),
        "lowercase receiver init remains a method call: {edges:#?}"
    );
    assert!(
        edges.iter().any(|edge| {
            edge.edge_kind == EdgeKind::CallsName
                && edge.to_name == "init"
                && edge.receiver_hint.as_deref() == Some("client")
        }),
        "a value-receiver init must retain its receiver hint: {edges:#?}"
    );
    for call in ["Self.init()", "self.init()", "super.init()"] {
        let receiver = call.split_once('.').map(|(receiver, _)| receiver).unwrap();
        let qualified = format!("{receiver}::init");
        assert!(
            edges.iter().any(|edge| {
                edge.edge_kind == EdgeKind::CallsName
                    && edge.to_name == "init"
                    && edge.evidence.as_deref() == Some(call)
                    && edge.receiver_hint.as_deref() == Some(receiver)
                    && edge.target_qualified_name.as_deref() == Some(qualified.as_str())
            }),
            "{call} must retain its local receiver context: {edges:#?}"
        );
    }
    assert!(
        !edges.iter().any(|edge| edge.edge_kind == EdgeKind::Constructs && edge.to_name == "init"),
        "init is not itself a constructed type: {edges:#?}"
    );
}

#[test]
fn qualified_type_references_keep_the_canonical_path() {
    let src = "func load(_ request: API.Request) -> Other.Request { request }";
    let edges = edges(src);
    let qualified_names = edges
        .iter()
        .filter(|edge| edge.edge_kind == EdgeKind::ReferencesType && edge.to_name == "Request")
        .map(|edge| {
            (
                edge.target_qualified_name.as_deref(),
                edge.receiver_hint.as_deref(),
                callee_text(edge, src),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(qualified_names, vec![
        (Some("API::Request"), Some("API"), Some("Request")),
        (Some("Other::Request"), Some("Other"), Some("Request")),
    ]);
}

#[test]
fn generic_and_self_associated_type_projections_are_not_nominal_references() {
    let src = r#"
struct Element {}
protocol Container { associatedtype Item }
func generic<T: Container>(_ value: T) -> T.Item { value as! T.Item }
extension Container { func local() -> Self.Item { fatalError() } }
"#;
    let edges = edges(src);
    assert!(
        !edges.iter().any(|edge| {
            edge.edge_kind == EdgeKind::ReferencesType
                && matches!(edge.to_name.as_str(), "T" | "Item" | "Self")
        }),
        "generic/Self projections must not bind unrelated nominal types: {edges:#?}"
    );
    assert!(has(&edges, EdgeKind::ReferencesType, "Container"));
}

#[test]
fn qualified_inheritance_and_construction_share_canonical_paths() {
    let src = "class Child: API.Parent { let request = API.Request() }";
    let edges = edges(src);
    for (kind, target, qualified) in [
        (EdgeKind::Implements, "Parent", "API::Parent"),
        (EdgeKind::Constructs, "Request", "API::Request"),
        (EdgeKind::ReferencesType, "Request", "API::Request"),
    ] {
        assert!(
            edges.iter().any(|edge| {
                edge.edge_kind == kind
                    && edge.to_name == target
                    && edge.target_qualified_name.as_deref() == Some(qualified)
            }),
            "missing canonical {kind:?} edge to {qualified}: {edges:#?}"
        );
    }
}

#[test]
fn macro_invocations_emit_uses_macro_edges() {
    let src = r#"
macro stringify<T>(_ value: T) = #externalMacro(module: "Macros", type: "StringifyMacro")
let result = #stringify(value)
"#;
    let edges = edges(src);
    let invocation = edges
        .iter()
        .find(|edge| edge.edge_kind == EdgeKind::UsesMacro && edge.to_name == "stringify")
        .unwrap_or_else(|| panic!("missing Swift macro-use edge: {edges:#?}"));
    assert_eq!(callee_text(invocation, src), Some("stringify"));
    assert!(source_text(invocation, src).is_some_and(|source| source == "#stringify(value)"));
    assert!(
        !has(&edges, EdgeKind::UsesMacro, "externalMacro"),
        "the compiler implementation hook is not a macro call: {edges:#?}"
    );
}

#[test]
fn ordinary_attributes_and_extension_targets_emit_type_references() {
    let src = r#"
@MainActor
@Macros.Observable(source: "fixture")
class Model {
    @Wrapper var value: Int
}

extension Model {
    func refresh() {}
}
"#;
    let edges = edges(src);
    for target in ["MainActor", "Observable", "Wrapper"] {
        assert!(
            has(&edges, EdgeKind::ReferencesType, target),
            "attribute type {target} must remain visible: {edges:#?}"
        );
        assert!(has(&edges, EdgeKind::UsesMacro, target));
    }
    assert!(
        !has(&edges, EdgeKind::UsesMacro, "source"),
        "attribute argument labels are not macro names: {edges:#?}"
    );
    let extension_target = edges
        .iter()
        .find(|edge| {
            edge.edge_kind == EdgeKind::ReferencesType
                && edge.to_name == "Model"
                && source_text(edge, src).is_some_and(|source| source.starts_with("Model"))
        })
        .unwrap_or_else(|| panic!("extension target must reference Model: {edges:#?}"));
    assert_eq!(callee_text(extension_target, src), Some("Model"));
}

#[test]
fn attached_attributes_emit_resolver_candidates_without_argument_labels() {
    let src = r#"
@attached(member) macro Observable() = #externalMacro(module: "Macros", type: "ObservableMacro")
@Observable struct Model {}
@Macros.Observable struct QualifiedModel {}
@available(*, deprecated) struct LegacyModel {}
@objc class ObjectiveCModel {}
@MainActor class ActorModel {}
"#;
    let edges = edges(src);
    let macro_uses =
        edges.iter().filter(|edge| edge.edge_kind == EdgeKind::UsesMacro).collect::<Vec<_>>();
    for attribute in ["attached", "Observable", "available", "objc", "MainActor"] {
        assert!(
            macro_uses.iter().any(|edge| edge.to_name == attribute),
            "attribute {attribute} must reach resolver policy: {edges:#?}"
        );
    }
    let qualified = macro_uses
        .iter()
        .find(|edge| edge.target_qualified_name.as_deref() == Some("Macros::Observable"))
        .unwrap_or_else(|| panic!("qualified macro use must retain module context: {edges:#?}"));
    assert_eq!(qualified.receiver_hint.as_deref(), Some("Macros"));
    assert!(!has(&edges, EdgeKind::UsesMacro, "member"));
    assert!(!has(&edges, EdgeKind::UsesMacro, "externalMacro"));
}

#[test]
fn expression_valued_callables_do_not_invent_outer_call_targets() {
    let src = r#"
let immediate = { helper() }()
let selected = handlers[key]()
plain()
client.fetch()
self.refresh()
super.finish()
"#;
    let edges = edges(src);
    let helper_calls = edges
        .iter()
        .filter(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == "helper")
        .collect::<Vec<_>>();
    assert_eq!(helper_calls.len(), 1, "closure IIFE must emit only its inner call");
    assert_eq!(source_text(helper_calls[0], src), Some("helper()"));
    assert!(has(&edges, EdgeKind::CallsName, "plain"), "direct call missing");
    assert!(has(&edges, EdgeKind::CallsName, "fetch"), "static member call missing");
    assert!(has(&edges, EdgeKind::CallsName, "refresh"), "self member call missing");
    assert!(has(&edges, EdgeKind::CallsName, "finish"), "super member call missing");
    assert!(
        !has(&edges, EdgeKind::CallsName, "key") && !has(&edges, EdgeKind::CallsName, "handlers"),
        "subscripted/dynamic callable must not invent a named outer call: {edges:#?}"
    );
}

/// tree-sitter-swift binds a call's argument list to the WHOLE binary expression on the
/// operator's left — `p + g()` parses as `call_expression(additive_expression(p + g), ())` — so
/// the call target is the operator node, not the callee. Every call on the right of a binary
/// operator used to be dropped outright (the operator node is not a callable path), losing
/// `total + item.price()`, `x == compute()`, `value ?? fallback()` and friends from the graph.
/// The callee is the operator expression's rightmost operand.
#[test]
fn calls_on_the_right_of_a_binary_operator_are_not_dropped() {
    // A bare call and a call inside an additive expression must BOTH reach the graph.
    let additive = edges("func f() -> String { return p + g() }");
    assert!(
        has(&additive, EdgeKind::CallsName, "g"),
        "a call on the right of `+` must still be a call: {additive:#?}"
    );

    // Both operands are calls; the right one used to vanish.
    let both = edges("func f() -> String { return s.g(1) + s.h(2) }");
    assert!(has(&both, EdgeKind::CallsName, "g"), "left operand call: {both:#?}");
    assert!(has(&both, EdgeKind::CallsName, "h"), "right operand call: {both:#?}");

    // The callee range still lands on the callee itself, not on the operator expression.
    let src = "func f() -> String { return p + résumé() }";
    let accented = edges(src);
    let call = accented
        .iter()
        .find(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == "résumé")
        .unwrap_or_else(|| panic!("missing call: {accented:#?}"));
    assert_eq!(
        callee_text(call, src),
        Some("résumé"),
        "the callee range must cover the callee, not the operator expression"
    );

    // Chained operators unwrap all the way to the rightmost operand.
    let chained = edges("func f() -> Int { return a + b + tail() }");
    assert!(has(&chained, EdgeKind::CallsName, "tail"), "chained operators: {chained:#?}");

    // Nil-coalescing is the same shape.
    let coalescing = edges("func f() -> Int { return value ?? fallback() }");
    assert!(has(&coalescing, EdgeKind::CallsName, "fallback"), "nil-coalescing: {coalescing:#?}");

    // A construction on the right of an operator is still a construction.
    let constructed = edges("func f() -> Int { return base + Service() }");
    assert!(has(&constructed, EdgeKind::Constructs, "Service"), "construction: {constructed:#?}");

    // An EXPLICIT initializer on the right of an operator is a construction too —
    // `Service.init` must collapse to `Constructs → Service`, never a `calls_name →
    // init` (the operator path must run the SAME `.init` normalization as a plain
    // `Service.init()`).
    let explicit_init = edges("func f() -> Int { return base + Service.init() }");
    assert!(
        has(&explicit_init, EdgeKind::Constructs, "Service"),
        "explicit init on operator RHS constructs the type: {explicit_init:#?}"
    );
    assert!(
        !has(&explicit_init, EdgeKind::CallsName, "init"),
        "explicit init must not become a call to `init`: {explicit_init:#?}"
    );

    // A qualified type method on the operator RHS keeps its receiver qualifier.
    let qualified = edges("func f() -> Int { return total + Factory.make() }");
    assert!(has(&qualified, EdgeKind::CallsName, "make"), "qualified method: {qualified:#?}");

    // A MULTI-SEGMENT receiver on the operator RHS: tree-sitter nests the operator under an
    // inner navigation, so the operator is not the direct receiver. These must still be
    // recovered — unlike the dynamic case, `Module.Client.make()` WITHOUT the operator resolves
    // fine, so dropping the operator form would be a real gap, not baseline parity.
    let nested = edges("func f() -> Int { return base + Module.Client.make() }");
    assert!(has(&nested, EdgeKind::CallsName, "make"), "nested-navigation method: {nested:#?}");
    let deep = edges("func f() -> Int { return total + config.section.value() }");
    assert!(has(&deep, EdgeKind::CallsName, "value"), "value-receiver chain: {deep:#?}");
    // Init normalization survives the nested case too: `Module.Service.init()` constructs.
    let nested_init = edges("func f() -> Int { return base + Module.Service.init() }");
    assert!(
        has(&nested_init, EdgeKind::Constructs, "Service"),
        "nested init constructs the type: {nested_init:#?}"
    );
    assert!(
        !has(&nested_init, EdgeKind::CallsName, "init"),
        "nested init must not be a call to `init`: {nested_init:#?}"
    );

    // A DYNAMIC receiver on the operator RHS (`make().render()`) must behave EXACTLY as it does
    // WITHOUT the operator: the inner `make()` is emitted, and the outer `.render` on a
    // dynamic (call/subscript) receiver is dropped. The baseline never emits an outer call on a
    // non-nameable receiver, so the operator case must match it — not manufacture a bogus
    // qualifier, and not diverge by emitting an edge the plain form wouldn't. The plain form is
    // the control. (A vacuous `.all()` over a missing `render` edge would hide exactly this, so
    // both directions are asserted explicitly.)
    let dynamic = edges("func f() -> Int { return base + make().render() }");
    assert!(has(&dynamic, EdgeKind::CallsName, "make"), "inner dynamic call: {dynamic:#?}");
    assert!(
        !has(&dynamic, EdgeKind::CallsName, "render"),
        "an outer call on a dynamic receiver is dropped, same as the non-operator baseline: \
         {dynamic:#?}"
    );
    let control = edges("func f() -> Int { return make().render() }");
    assert!(has(&control, EdgeKind::CallsName, "make"), "control inner call: {control:#?}");
    assert!(
        !has(&control, EdgeKind::CallsName, "render"),
        "control: the baseline also drops the outer dynamic-receiver call: {control:#?}"
    );
}
/// A force-unwrapped RECEIVER (`obj!.method()`) is recovered like an optional-chained one
/// (`obj?.method()`) — Swift's `X!` names the same receiver path as `X`, so the two must agree.
/// Force-unwrap used to be dropped everywhere (its `postfix_expression` failed the static-path
/// check) while optional-chain resolved — an asymmetry this closes (#655). A CUSTOM postfix
/// (`obj++`) stays dropped: it is a value expression, not a nameable path.
#[test]
fn force_unwrapped_receiver_calls_are_recovered_like_optional_chained() {
    let force = edges("func f() { obj!.method() }");
    assert!(has(&force, EdgeKind::CallsName, "method"), "force-unwrap receiver: {force:#?}");
    // Plain force-unwrap and optional-chain now AGREE — same callee, same receiver hint.
    let optional = edges("func f() { obj?.method() }");
    let hint = |es: &[EdgeCandidate]| {
        es.iter()
            .find(|e| e.edge_kind == EdgeKind::CallsName && e.to_name == "method")
            .and_then(|e| e.receiver_hint.clone())
    };
    assert_eq!(hint(&force), Some("obj".to_string()), "force-unwrap receiver hint: {force:#?}");
    assert_eq!(hint(&force), hint(&optional), "force-unwrap must match optional-chain");

    // A custom postfix operator is a value expression, not a receiver path — still dropped.
    let custom = edges("func f() { obj++.method() }");
    assert!(
        !has(&custom, EdgeKind::CallsName, "method"),
        "a custom postfix receiver is not a nameable path: {custom:#?}"
    );

    // Force-unwrap `!` must NOT be emitted as an operator call (the existing suppression
    // holds).
    assert!(
        !has(&force, EdgeKind::UsesOperator, "!") && !has(&force, EdgeKind::CallsName, "!"),
        "force-unwrap `!` is not a callable operator: {force:#?}"
    );

    // Multi-segment and chained force-unwraps, and force-unwrap on an operator RHS.
    let qualified = edges("func f() { self.cache!.render() }");
    assert!(has(&qualified, EdgeKind::CallsName, "render"), "qualified: {qualified:#?}");
    let chained = edges("func f() { a!.b!.c() }");
    assert!(has(&chained, EdgeKind::CallsName, "c"), "chained force-unwraps: {chained:#?}");
    let on_operator = edges("func f() -> Int { return total + obj!.value() }");
    assert!(
        has(&on_operator, EdgeKind::CallsName, "value"),
        "force-unwrap on operator RHS: {on_operator:#?}"
    );
}
