use rag_rat_base::language::Language;

use crate::index::edges::EdgeKind;
use crate::index::languages::test_support::{EdgeFact, edge_facts, fact};

fn cpp_calls(body: &str) -> Vec<EdgeFact> {
    edge_facts(
        "src/main.cpp",
        Language::Cpp,
        &format!("void f() {{ {body} }}"),
        EdgeKind::CallsName,
    )
}

/// A template argument list is not part of the callee path.
#[test]
fn a_template_call_names_the_template_not_its_argument() {
    assert_eq!(cpp_calls("std::make_unique<Box<int>>();"), vec![fact(
        EdgeKind::CallsName,
        "make_unique",
        Some("std::make_unique"),
        Some("std")
    )]);
}

/// A member spelled with its own scope qualifies the target by that scope; the object is only the
/// receiver.
#[test]
fn a_scope_qualified_member_call_is_qualified_by_its_scope() {
    assert_eq!(cpp_calls("w->Widget::run();"), vec![fact(
        EdgeKind::CallsName,
        "run",
        Some("Widget::run"),
        Some("w")
    )]);
}

/// tree-sitter-cpp nests a scope path to the RIGHT (`a` + `b::c`), so the inner scope node is the
/// path continuing, not a member spelling its own scope: the whole path is the qualified target.
#[test]
fn a_nested_namespace_call_keeps_its_root_namespace() {
    assert_eq!(cpp_calls("a::b::c(x);"), vec![fact(
        EdgeKind::CallsName,
        "c",
        Some("a::b::c"),
        Some("a")
    )]);
    assert_eq!(cpp_calls("std::chrono::steady_clock::now();"), vec![fact(
        EdgeKind::CallsName,
        "now",
        Some("std::chrono::steady_clock::now"),
        Some("std")
    )]);
}

#[test]
fn a_member_spelling_a_nested_scope_is_qualified_by_that_whole_scope() {
    assert_eq!(cpp_calls("w->ns::Widget::run();"), vec![fact(
        EdgeKind::CallsName,
        "run",
        Some("ns::Widget::run"),
        Some("w")
    )]);
}

/// A type reference is named along its `scope`/`name` fields: a template argument is its own
/// reference, never the name of the template or scope path it sits in.
#[test]
fn a_type_reference_names_the_template_not_its_argument() {
    let types = |body: &str| {
        let mut facts = edge_facts(
            "src/main.cpp",
            Language::Cpp,
            &format!("void f() {{ {body} }}"),
            EdgeKind::ReferencesType,
        );
        facts.sort_by(|a, b| (&a.1, &a.2).cmp(&(&b.1, &b.2)));
        facts
    };
    let ty =
        |name: &str, qualified: Option<&str>| fact(EdgeKind::ReferencesType, name, qualified, None);
    assert_eq!(types("std::vector<Gadget> v;"), vec![
        ty("Gadget", None),
        ty("std", None),
        ty("vector", None),
        ty("vector", Some("std::vector")),
    ]);
    assert_eq!(types("auto t = new ns::Thing<Item>(q);"), vec![
        ty("Item", None),
        ty("Thing", None),
        ty("Thing", Some("ns::Thing")),
        ty("ns", None),
    ]);
}
