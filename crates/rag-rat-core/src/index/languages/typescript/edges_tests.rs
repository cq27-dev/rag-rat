use rag_rat_base::language::Language;

use crate::index::edges::EdgeKind;
use crate::index::languages::test_support::{EdgeFact, edge_facts, fact};

fn ts(source: &str, kind: EdgeKind) -> Vec<EdgeFact> {
    edge_facts("src/index.ts", Language::TypeScript, source, kind)
}

/// `new_expression` names its callee in the `constructor` field; the arguments and type
/// arguments are never the constructed type.
#[test]
fn new_constructs_the_constructor_field() {
    assert_eq!(ts("new Foo(arg);", EdgeKind::Constructs), vec![fact(
        EdgeKind::Constructs,
        "Foo",
        None,
        None
    )]);
    assert_eq!(ts("new ns.Bar(a, b);", EdgeKind::Constructs), vec![fact(
        EdgeKind::Constructs,
        "Bar",
        Some("ns::Bar"),
        Some("ns")
    )]);
    assert_eq!(ts("new Map<string, Widget>();", EdgeKind::Constructs), vec![fact(
        EdgeKind::Constructs,
        "Map",
        None,
        None
    )]);
}

#[test]
fn an_exported_declaration_exports_only_its_name() {
    assert_eq!(ts("export function f(a: W) { helper(a) }", EdgeKind::Exports), vec![fact(
        EdgeKind::Exports,
        "f",
        None,
        None
    )]);
    assert_eq!(ts("export const x = 1, y = g();", EdgeKind::Exports), vec![
        fact(EdgeKind::Exports, "x", None, None),
        fact(EdgeKind::Exports, "y", None, None),
    ]);
}

#[test]
fn a_re_export_names_the_module_or_the_exported_binding() {
    assert_eq!(ts("export * from \"./x\";", EdgeKind::Exports), vec![fact(
        EdgeKind::Exports,
        "./x",
        None,
        None
    )]);
    assert_eq!(ts("export { a as b };", EdgeKind::Exports), vec![fact(
        EdgeKind::Exports,
        "b",
        None,
        None
    )]);
    assert_eq!(ts("export default Foo;", EdgeKind::Exports), vec![fact(
        EdgeKind::Exports,
        "Foo",
        None,
        None
    )]);
}

/// A call result has no name: `baz` hangs off `foo(bar)`, so there is no receiver, no qualified
/// target and no type reference to `foo` — and `bar` is never part of the path.
#[test]
fn a_call_on_a_call_result_has_no_receiver() {
    assert_eq!(ts("foo(bar).baz();", EdgeKind::CallsName), vec![
        fact(EdgeKind::CallsName, "baz", None, None),
        fact(EdgeKind::CallsName, "foo", None, None),
    ]);
    assert_eq!(ts("foo(bar).baz();", EdgeKind::ReferencesType), vec![]);
    assert_eq!(ts("expect(x).toBe(1);", EdgeKind::CallsName), vec![
        fact(EdgeKind::CallsName, "toBe", None, None),
        fact(EdgeKind::CallsName, "expect", None, None),
    ]);
}

/// A chain rooted in an unnamed value stays unrooted however deep it goes: `x` and `d` are members
/// of a call result and of `this`, not receivers, qualifiers or types.
#[test]
fn a_nested_member_of_an_unnamed_value_has_no_receiver() {
    assert_eq!(ts("foo(bar).x.baz();", EdgeKind::CallsName), vec![
        fact(EdgeKind::CallsName, "baz", None, None),
        fact(EdgeKind::CallsName, "foo", None, None),
    ]);
    assert_eq!(ts("foo(bar).x.baz();", EdgeKind::ReferencesType), vec![]);
    assert_eq!(ts("const a = this.d.e(f);", EdgeKind::CallsName), vec![fact(
        EdgeKind::CallsName,
        "e",
        None,
        None
    )]);
    assert_eq!(ts("const a = this.d.e(f);", EdgeKind::ReferencesType), vec![]);
}

#[test]
fn a_jsx_member_tag_references_its_last_segment() {
    let source = "const x = <Inner.Part />;";
    assert_eq!(
        edge_facts("src/App.tsx", Language::TypeScript, source, EdgeKind::ReferencesType),
        vec![fact(EdgeKind::ReferencesType, "Part", Some("Inner::Part"), Some("Inner"))]
    );
}

/// The non-null assertion `a!` is the same value as `a`, so it does not cut the chain.
#[test]
fn a_non_null_asserted_receiver_keeps_the_chain() {
    assert_eq!(ts("a!.b();", EdgeKind::CallsName), vec![fact(
        EdgeKind::CallsName,
        "b",
        Some("a::b"),
        Some("a")
    )]);
    assert_eq!(ts("a!.b();", EdgeKind::ReferencesType), vec![fact(
        EdgeKind::ReferencesType,
        "a",
        None,
        None
    )]);
}
