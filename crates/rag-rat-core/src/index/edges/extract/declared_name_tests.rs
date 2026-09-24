//! A declaration's own name and its type-parameter binders are not type references, and one token
//! references one type once — across every language, through the one gate in `EdgeEmitter::push`.

use std::path::Path;

use rag_rat_base::language::Language;

use super::syntactic_edges;
use crate::index::edges::EdgeKind;

/// Every `ReferencesType` edge for `source`, as the text of the token it was read from.
fn type_references(path: &str, language: Language, source: &str) -> Vec<String> {
    syntactic_edges(Path::new(path), language, source, &[])
        .expect("fixture parses")
        .into_iter()
        .filter(|edge| edge.edge_kind == EdgeKind::ReferencesType)
        .map(|edge| {
            let callee = edge.callee_span.expect("a type reference names its token");
            format!("{}@{}", edge.to_name, callee.start_byte)
        })
        .collect()
}

#[test]
fn rust_struct_enum_and_trait_names_are_not_references_to_themselves() {
    assert_eq!(
        type_references("a.rs", Language::Rust, "struct S { a: u8 }\nenum E { A }\ntrait T {}\n"),
        Vec::<String>::new()
    );
}

#[test]
fn a_rust_type_that_mentions_itself_still_references_itself() {
    // Only the declared name is dropped; the recursive use inside the body is a real reference.
    assert_eq!(
        type_references("a.rs", Language::Rust, "struct Node { next: Option<Box<Node>> }\n"),
        ["Option@20", "Box@27", "Node@31"]
    );
}

#[test]
fn a_rust_impl_references_its_self_type() {
    // An impl is named by its self type, and that name is a use of the struct.
    assert_eq!(type_references("a.rs", Language::Rust, "struct S;\nimpl S {}\n"), ["S@15"]);
}

#[test]
fn rust_type_parameter_binders_are_not_references() {
    assert_eq!(
        type_references(
            "a.rs",
            Language::Rust,
            "struct G<T>(T);\nfn f<U>(u: U) {}\nimpl<T> Tr for Box<T> {}\n"
        ),
        ["T@12", "U@27", "Tr@41", "Box@48", "T@52"]
    );
}

#[test]
fn a_rust_turbofish_call_references_its_type_once() {
    assert_eq!(
        type_references("a.rs", Language::Rust, "fn g() { let v = Vec::<u8>::new(); }\n"),
        ["Vec@17"]
    );
}

#[test]
fn typescript_class_interface_and_alias_names_are_not_references_to_themselves() {
    assert_eq!(
        type_references(
            "a.ts",
            Language::TypeScript,
            "class Main {}\ninterface Iface { a: number }\ntype Alias = string;\n"
        ),
        Vec::<String>::new()
    );
}

#[test]
fn typescript_type_parameter_binders_are_not_references() {
    // The binders `<T>` / `<U>` are dropped; each later use of them is kept.
    assert_eq!(
        type_references(
            "a.ts",
            Language::TypeScript,
            "function id<T>(x: T): T { return x; }\nclass Box<U> { v: U }\n"
        ),
        ["T@18", "T@22", "U@56"]
    );
}

#[test]
fn a_c_function_pointer_typedef_is_not_a_reference_to_itself() {
    assert_eq!(
        type_references("a.c", Language::C, "typedef int (*cb_t)(int);\n"),
        Vec::<String>::new()
    );
}

#[test]
fn c_struct_and_typedef_names_are_not_references_to_themselves() {
    // The bodyless `struct S *p` is a use of the definition and stays a reference.
    assert_eq!(
        type_references(
            "a.c",
            Language::C,
            "struct S { int a; };\ntypedef struct P { int x; } P;\nstruct S *p;\n"
        ),
        ["S@59"]
    );
}

#[test]
fn cpp_class_and_typedef_names_are_not_references_to_themselves() {
    assert_eq!(
        type_references("a.cpp", Language::Cpp, "class A { void f(); };\ntypedef int cb_t;\n"),
        Vec::<String>::new()
    );
}

#[test]
fn a_cpp_template_type_parameter_binder_is_not_a_reference() {
    assert_eq!(
        type_references("a.cpp", Language::Cpp, "template<typename T> class X { T t; };\n"),
        ["T@31"]
    );
}

#[test]
fn a_cpp_out_of_line_definition_references_its_scope_not_its_own_name() {
    // The last segment of `Outer::Fwd` / `Outer::run` is the declaration; `Outer` is a use.
    assert_eq!(
        type_references("a.cpp", Language::Cpp, "struct Outer::Fwd {};\nvoid Outer::run() {}\n"),
        ["Outer@7", "Outer@27"]
    );
}

#[test]
fn a_qualified_cpp_specialization_references_its_primary_template() {
    // `Box` in `ns::Box<int>` is the primary template, not the specialization's own name.
    assert_eq!(
        type_references("a.cpp", Language::Cpp, "template<> struct ns::Box<int> { int t; };\n"),
        ["Box@22", "ns@18"]
    );
}

#[test]
fn a_cpp_qualified_type_references_its_type_once() {
    assert_eq!(type_references("a.cpp", Language::Cpp, "ns::Thing x;\n"), ["Thing@4", "ns@0"]);
}

#[test]
fn go_receiver_type_parameter_binders_are_not_references() {
    let source = concat!(
        "package main\n\n",
        "type Pair[K comparable, V any] struct{}\n\n",
        "func (p Pair[K, V]) M() {}\n\n",
        "func (p *Pair[K, V]) N() {}\n",
    );
    assert_eq!(type_references("a.go", Language::Go, source), [
        "comparable@26",
        "any@40",
        "Pair@63",
        "Pair@92",
    ]);
}

#[test]
fn python_type_parameter_binders_are_not_references() {
    assert_eq!(
        type_references("a.py", Language::Python, "class P[T]:\n    x: T\n\ntype X[U] = list[U]\n"),
        ["T@19", "list@34", "U@39"]
    );
}

#[test]
fn bounded_and_constrained_python_binders_are_not_references() {
    // `T: int` wraps the binder in a `constrained_type`; only the bound is a use.
    assert_eq!(
        type_references(
            "a.py",
            Language::Python,
            "class P[T: int]: ...\ndef f[U: (int, str)](u: U): ...\ntype X[V: int] = V\n"
        ),
        ["int@11", "int@31", "str@36", "U@45", "int@63", "V@70"]
    );
}

#[test]
fn a_swift_extension_references_the_type_it_extends() {
    assert_eq!(type_references("a.swift", Language::Swift, "struct S {}\nextension S {}\n"), [
        "S@22"
    ]);
}

#[test]
fn rust_union_and_associated_type_names_are_not_references_to_themselves() {
    assert_eq!(
        type_references("a.rs", Language::Rust, "union U { a: u8 }\ntrait T { type Item; }\n"),
        Vec::<String>::new()
    );
}

#[test]
fn a_typescript_abstract_class_name_is_not_a_reference_to_itself() {
    assert_eq!(
        type_references("a.ts", Language::TypeScript, "abstract class AC {}\n"),
        Vec::<String>::new()
    );
}

#[test]
fn typescript_mapped_type_and_infer_binders_are_not_references() {
    // `[K in ...]` and `infer U` bind like `<T>`; each later use of them is kept.
    assert_eq!(
        type_references(
            "a.ts",
            Language::TypeScript,
            "type M<T> = { [K in keyof T]: T[K] };\ntype I<T> = T extends Array<infer U> ? U : \
             never;\n"
        ),
        ["T@26", "T@30", "K@32", "T@50", "Array@60", "U@77"]
    );
}
