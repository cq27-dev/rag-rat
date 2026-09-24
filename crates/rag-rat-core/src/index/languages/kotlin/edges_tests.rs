use rag_rat_base::language::Language;

use crate::index::edges::EdgeKind;
use crate::index::languages::test_support::{EdgeFact, edge_facts, fact};

fn kotlin(source: &str, kind: EdgeKind) -> Vec<EdgeFact> {
    edge_facts("src/Main.kt", Language::Kotlin, source, kind)
}

fn calls(body: &str) -> Vec<EdgeFact> {
    kotlin(&format!("fun f() {{ {body} }}"), EdgeKind::CallsName)
}

/// The callee is the call's first child; the value argument `x` is never the callee.
#[test]
fn a_method_call_names_the_method_not_its_argument() {
    assert_eq!(calls("list.add(x)"), vec![fact(
        EdgeKind::CallsName,
        "add",
        Some("list::add"),
        Some("list")
    )]);
}

#[test]
fn a_nested_call_argument_is_its_own_call_not_the_outer_callee() {
    assert_eq!(calls("foo(bar(x))"), vec![
        fact(EdgeKind::CallsName, "foo", None, None),
        fact(EdgeKind::CallsName, "bar", None, None),
    ]);
}

/// A trailing lambda follows the callee; `it` inside it is not the callee, and a call result
/// (`listOf(1)`) is not a named receiver.
#[test]
fn a_trailing_lambda_never_supplies_the_callee() {
    assert_eq!(calls("listOf(1).map { it + 1 }"), vec![
        fact(EdgeKind::CallsName, "map", None, None),
        fact(EdgeKind::CallsName, "listOf", None, None),
    ]);
    assert_eq!(calls("run { helper() }"), vec![
        fact(EdgeKind::CallsName, "run", None, None),
        fact(EdgeKind::CallsName, "helper", None, None),
    ]);
}

/// kotlin-ng binds `!` tighter than the call, so the callee sits under a `unary_expression`.
#[test]
fn a_negated_call_still_names_its_callee() {
    assert_eq!(calls("if (!granted(Manifest.permission.RECORD_AUDIO)) return"), vec![fact(
        EdgeKind::CallsName,
        "granted",
        None,
        None
    )]);
}

/// A type-shaped RECEIVER does not make a call a construction: `Result.success` calls a member.
#[test]
fn a_member_call_on_a_type_is_not_a_construction() {
    let source = "fun f() { Result.success(1) }";
    assert_eq!(kotlin(source, EdgeKind::Constructs), vec![]);
    assert_eq!(kotlin(source, EdgeKind::CallsName), vec![fact(
        EdgeKind::CallsName,
        "success",
        Some("Result::success"),
        Some("Result")
    )]);
    assert_eq!(kotlin("fun f() { Outer.Inner(1) }", EdgeKind::Constructs), vec![fact(
        EdgeKind::Constructs,
        "Inner",
        Some("Outer::Inner"),
        None
    )]);
}

#[test]
fn a_delegated_supertype_implements_the_type_not_the_delegate() {
    assert_eq!(kotlin("class Impl : Other by delegate", EdgeKind::Implements), vec![fact(
        EdgeKind::Implements,
        "Other",
        None,
        None
    )]);
    assert_eq!(kotlin("class A : Base(arg), Map<K, V>", EdgeKind::Implements), vec![
        fact(EdgeKind::Implements, "Base", None, None),
        fact(EdgeKind::Implements, "Map", None, None),
    ]);
}

/// One Imports edge per statement, named by the last segment and qualified by the full path.
#[test]
fn an_import_is_one_edge_for_the_full_path() {
    assert_eq!(kotlin("import a.b.C", EdgeKind::Imports), vec![fact(
        EdgeKind::Imports,
        "C",
        Some("a::b::C"),
        None
    )]);
    assert_eq!(kotlin("import a.b.C as D", EdgeKind::Imports), vec![fact(
        EdgeKind::Imports,
        "C",
        Some("a::b::C"),
        None
    )]);
}
