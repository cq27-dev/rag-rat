use rag_rat_base::language::Language;
use tree_sitter::Parser;

use super::*;

/// A `use` is an ITEM, and an item shadows an outer parameter of the same name. Verified with
/// rustc: with `mod items { pub struct worker; impl worker { pub fn run(&self) -> u16 } }`,
/// `fn f(worker: A) -> u16 { use crate::items::worker; worker.run() }` COMPILES — the call went
/// to the imported type, so reading the parameter binds the call to the wrong owner.
#[test]
fn an_import_that_binds_the_receiver_name_declines_the_hint() {
    let shadowed = "fn f(worker: A) { use crate::items::worker; worker.run(); }";
    assert_eq!(extract_call_hints(shadowed), vec![None]);
    // An import of some OTHER name says nothing about this receiver.
    let unrelated = "fn f(worker: A) { use crate::items::other; worker.run(); }";
    assert_eq!(extract_call_hints(unrelated), vec![Some("A".to_string())]);
    // A glob may import a same-named item, so the outer parameter is no longer proven.
    let globbed = "fn f(worker: A) { use crate::items::*; worker.run(); }";
    assert_eq!(extract_call_hints(globbed), vec![None]);
    // A closer explicit declaration proves the binding despite an earlier glob.
    let explicit = "fn f(worker: A) { use crate::items::*; let worker: B = value; worker.run(); }";
    assert_eq!(extract_call_hints(explicit), vec![Some("B".to_string())]);
    // An aliased import binds the ALIAS, not the original name.
    let aliased = "fn f(worker: A) { use crate::items::thing as worker; worker.run(); }";
    assert_eq!(extract_call_hints(aliased), vec![None]);
}

/// Rust allows whitespace at any legal token boundary, so a spelling is not an identity. The
/// receiver-type predicates are string predicates, and `Self ::Assoc` slipping past the
/// `Self::` check produced a qualified hint whose tail could bind an unrelated concrete
/// `Assoc::run` — while the same type spelled tightly correctly declined.
#[test]
fn a_spaced_path_separator_names_the_same_type_as_a_tight_one() {
    for (spaced, tight) in [
        (
            "impl Tr for W { fn f(&self, x: Self ::Assoc) { x.run(); } }",
            "impl Tr for W { fn f(&self, x: Self::Assoc) { x.run(); } }",
        ),
        ("fn f(w: a :: b :: Worker) { w.run(); }", "fn f(w: a::b::Worker) { w.run(); }"),
        ("fn f(w: crate :: Worker) { w.run(); }", "fn f(w: crate::Worker) { w.run(); }"),
    ] {
        assert_eq!(
            extract_call_hints(spaced),
            extract_call_hints(tight),
            "spacing is not identity: {spaced}"
        );
    }
}

#[test]
fn receiver_type_nameability_follows_the_ast_shape() {
    assert_eq!(extract_call_hints("fn f(w: &mut (((Worker)))) { w.run(); }"), vec![Some(
        "Worker".to_string()
    )]);
    for type_name in [
        "*mut Worker",
        "(Worker, Other)",
        "[Worker; 2]",
        "dyn Service",
        "impl Service",
        "fn() -> Worker",
        "<Worker as Service>::Assoc",
        "Receiver!()",
    ] {
        let code = format!("fn f(w: {type_name}) {{ w.run(); }}");
        assert_eq!(extract_call_hints(&code), vec![None], "{type_name} is not one owner path");
    }
}

#[test]
fn the_binding_owner_comes_from_the_scoped_path_node() {
    let code = r#"
            mod factory {
                impl Factory { fn new() -> Worker { Worker } }
            }
            fn f() {
                let worker = factory :: Factory :: new();
                worker.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None, Some("factory::Worker".to_string())]);
}

fn extract_call_hints(code: &str) -> Vec<Option<String>> {
    let mut parser = Parser::new();
    let language = tree_sitter_rust::LANGUAGE;
    parser.set_language(&language.into()).unwrap();
    let tree = parser.parse(code, None).unwrap();
    let root = tree.root_node();

    let mut hints = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            hints.push(infer_rust_receiver_type_hint(node, code));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    hints.reverse();
    hints
}

#[test]
fn test_self_in_impl() {
    let code = r#"
            impl Worker {
                fn run(&self) {
                    self.execute();
                }
            }
        "#;
    let hints = extract_call_hints(code);
    assert_eq!(hints, vec![Some("Worker".to_string())]);
}

#[test]
fn an_explicit_reference_self_type_keeps_the_impl_owner() {
    let code = r#"
            impl Worker {
                fn run(self: &Self) {
                    self.execute();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("Worker".to_string())]);
}

#[test]
fn an_explicit_deref_wrapper_self_type_declines_single_owner_inference() {
    for receiver in ["Box<Self>", "Rc<Self>", "Arc<Self>", "Pin<Box<Self>>"] {
        let code = format!("impl Worker {{ fn run(self: {receiver}) {{ self.execute(); }} }}");
        assert_eq!(
            extract_call_hints(&code),
            vec![None],
            "{receiver} may dispatch before dereferencing to Worker"
        );
    }
}

#[test]
fn test_associated_self_in_impl() {
    let code = r#"
            impl Worker {
                fn run() {
                    Self::execute();
                }
            }
        "#;
    let hints = extract_call_hints(code);
    assert_eq!(hints, vec![Some("Worker".to_string())]);
}

#[test]
fn projected_self_call_does_not_name_the_impl_owner() {
    let code = r#"
            trait Holder { type Assoc; }
            struct Worker;
            struct Factory;
            impl Holder for Factory {
                type Assoc = Worker;
                fn run() { Self::Assoc::execute(); }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None]);

    let unconditional = r#"
            fn test() {
                #[allow(unused)]
                let worker: Alpha = alpha;
                worker.run();
            }
        "#;
    assert_eq!(extract_call_hints(unconditional), vec![Some("Alpha".to_string())]);
}

#[test]
fn test_simple_param() {
    let code = r#"
            fn process(worker: &mut Worker) {
                worker.run();
            }
        "#;
    let hints = extract_call_hints(code);
    assert_eq!(hints, vec![Some("Worker".to_string())]);
}

#[test]
fn test_annotated_let_binding() {
    let code = r#"
            fn test() {
                let w: Worker = get_worker();
                w.run();
            }
        "#;
    let hints = extract_call_hints(code);
    // First call is get_worker() (no receiver), second call is w.run()
    assert_eq!(hints, vec![None, Some("Worker".to_string())]);
}

#[test]
fn test_constructor_without_visible_declaration_declined() {
    let code = r#"
            fn test() {
                let w = Worker::new();
                w.run();
            }
        "#;
    let hints = extract_call_hints(code);
    assert_eq!(hints, vec![None, None]);
}

#[test]
fn a_declared_return_of_another_type_retypes_the_hint() {
    let code = r#"
            impl Factory {
                fn new() -> Worker { Worker }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
    // The declared return type is visible in this file: the receiver is a Worker, not the
    // owner the call is scoped to.
    assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
}

/// The callee's NAME is not what makes the hint sound — its declared return type is — so any
/// same-file associated item answers, whatever it is called and whatever it does.
///
/// `new`/`default` used to be the only names considered, on the reasoning that Rust forces no
/// method to return `Self`. True, and exactly why the declaration is read: a builder-shaped
/// `with_capacity` that declares `-> Self` IS returning the owner, and one that declares
/// something else says so in the same place. Nothing checks that the callee constructs
/// anything, so a UFCS method call and a trait impl's associated fn answer on the same terms.
#[test]
fn a_same_file_declaration_answers_for_any_associated_item() {
    let self_returning = r#"
            impl Buffer {
                fn with_capacity(n: usize) -> Self { todo!() }
            }
            fn test() {
                let b = Buffer::with_capacity(8);
                b.run();
            }
        "#;
    assert_eq!(
        extract_call_hints(self_returning),
        vec![None, Some("Buffer".to_string())],
        "a `-> Self` builder types its binding like any constructor",
    );

    // And the declaration still decides WHICH type, under a name no convention covers.
    let other_returning = r#"
            impl Factory {
                fn spawn_worker() -> Worker { todo!() }
            }
            fn test() {
                let w = Factory::spawn_worker();
                w.run();
            }
        "#;
    assert_eq!(
        extract_call_hints(other_returning),
        vec![None, Some("Worker".to_string())],
        "the declared return re-types the hint regardless of the name",
    );

    // A `&self` method called through UFCS is a scoped call like any other, and its return is
    // declared in the same place — the callee constructs nothing and still answers.
    let ufcs_method = r#"
            impl Store {
                fn handle(&self) -> Handle { todo!() }
            }
            fn test(st: Store) {
                let h = Store::handle(&st);
                h.zap();
            }
        "#;
    assert_eq!(
        extract_call_hints(ufcs_method),
        vec![None, Some("Handle".to_string())],
        "a UFCS method call is typed by its declared return, not by being a constructor",
    );

    // `Self` in a TRAIT impl is the impl's `type`, not the trait — `impl From<u8> for Config`
    // returning `Self` is a `Config`, never a `From<u8>`.
    let trait_impl = r#"
            impl From<u8> for Config {
                fn from(v: u8) -> Self { todo!() }
            }
            fn test() {
                let c = Config::from(3u8);
                c.zap();
            }
        "#;
    assert_eq!(
        extract_call_hints(trait_impl),
        vec![None, Some("Config".to_string())],
        "a trait impl's `Self` is the implementing type",
    );
}

/// A declared return this pass cannot peel names no receiver. `-> Result<Self, E>` reads as a
/// nameable path right up to its arguments, and carrying it through would be worse than
/// silence: nothing resolves `Result<Self,E>`, and a receiver type that is PRESENT closes the
/// bare-name fallback the call had without any hint at all.
#[test]
fn a_declared_return_carrying_generic_arguments_declines() {
    for declared in ["Result<Self, E>", "Option<Self>", "Vec<Worker>", "Result<Worker, E>"] {
        let code = format!(
            r#"
                    impl Worker {{
                        fn make() -> {declared} {{ todo!() }}
                    }}
                    fn test() {{
                        let w = Worker::make();
                        w.run();
                    }}
                "#
        );
        assert_eq!(extract_call_hints(&code), vec![None, None], "{declared}");
    }
}

/// One type can implement `From` many times, and every impl declares a `from`. Nothing in the
/// call `Config::from(3u8)` says which one it reaches — the argument types would, and this pass
/// does not read them — so the honest answer is no hint at all.
#[test]
fn duplicate_trait_impls_on_one_type_go_ambiguous() {
    let code = r#"
            impl From<u8> for Config {
                fn from(v: u8) -> Self { todo!() }
            }
            impl From<u16> for Config {
                fn from(v: u16) -> Config { todo!() }
            }
            fn test() {
                let c = Config::from(3u8);
                c.zap();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None, None]);
}

/// A binder is in force at every position inside the item that declares it, so the binder
/// question is asked of the POSITION. These are the shapes where the enclosing item — not the
/// function the old code looked at — is what introduces the name.
#[test]
fn test_binders_from_every_enclosing_item_are_declined() {
    // (source, how many call hints the fixture produces)
    let cases = [
        // The impl binds it; the function does not.
        ("impl<Entry: Runs> Bag<Entry> { fn drive(&self, item: Entry) { item.run(); } }", 1),
        // A `let` annotation under an impl binder.
        ("impl<Entry> Bag<Entry> { fn drive(&self) { let v: Entry = make(); v.run(); } }", 2),
        // A trait's default-method body sits under the TRAIT's binders.
        ("trait Feeder<Entry> { fn feed(&self, e: Entry) { e.run(); } }", 1),
        // A blanket impl binds its own Self type, so `self` names no concrete owner.
        ("impl<X: Runs> Render for X { fn render(&self) { self.tick(); } }", 1),
        // Nested: the binder comes from an impl two levels above the call.
        (
            "impl<Entry> Bag<Entry> { fn drive(&self) { if true { let v: Entry = make(); v.run(); \
             } } }",
            2,
        ),
    ];
    for (code, hints) in cases {
        let got = extract_call_hints(code);
        assert_eq!(got.len(), hints, "fixture shape changed for {code}");
        assert!(
            got.iter().all(Option::is_none),
            "a generic binder must never be read as a concrete receiver type: {code} -> {got:?}"
        );
    }
}

/// The flip side: a real type of the same shape, with no binder declaring it, still resolves.
#[test]
fn test_a_concrete_type_is_not_mistaken_for_a_binder() {
    let code = r#"
            struct Entry;
            impl Bag { fn drive(&self, item: Entry) { item.run(); } }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("Entry".to_string())]);
}

/// A blanket impl's target is its own binder, so its `new` is not a candidate constructor for
/// a same-spelled concrete owner — counting it would outvote the real one into ambiguity.
#[test]
fn test_a_blanket_impl_does_not_outvote_the_real_constructor() {
    let code = r#"
            impl<Factory: Build> Build for Factory {
                fn new() -> Factory { todo!() }
            }
            impl Factory {
                fn new() -> Worker { todo!() }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
}

/// A constructor's own generic binder is not a type name. `fn new<U>() -> U` returns whatever
/// the CALL SITE instantiates, so reading `U` as the receiver type would hand the call to any
/// module-level item that happens to be spelled `U`.
#[test]
fn test_constructor_own_generic_return_declined() {
    let code = r#"
            struct U;
            impl U { fn run(&self) {} }
            impl Factory {
                fn new<U>() -> U { todo!() }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None, None]);
}

/// The same rule for a binder introduced by the IMPL rather than the function.
#[test]
fn test_constructor_impl_generic_return_declined() {
    let code = r#"
            impl<T> Factory<T> {
                fn new() -> T { todo!() }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None, None]);
}

#[test]
fn test_same_file_self_returning_constructor_confirms_the_owner() {
    let code = r#"
            impl Worker {
                fn new() -> Self { Worker }
            }
            fn test() {
                let w = Worker::new();
                w.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
}

#[test]
fn test_self_constructor_resolves_against_enclosing_impl() {
    let code = r#"
            impl Worker {
                fn new() -> Self { Worker }
                fn make() {
                    let worker = Self::new();
                    worker.run();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![
        Some("Worker".to_string()),
        Some("Worker".to_string())
    ]);
}

#[test]
fn test_bare_hint_is_canonical_against_the_lexical_module() {
    let code = r#"
            struct Worker;
            mod inner {
                struct Worker;
                fn f(w: &Worker) {
                    w.run();
                }
            }
        "#;
    // The parameter's `Worker` is `inner::Worker` — a bare hint would exact-match the ROOT
    // `Worker::run` scope instead. Canonicalization pins the lexical module.
    assert_eq!(extract_call_hints(code), vec![Some("inner::Worker".to_string())]);
}

#[test]
fn test_raw_pointer_receiver_type_declined() {
    let code = r#"
            fn f(p: *mut Worker) {
                p.is_null();
            }
        "#;
    // `*mut Worker` is a pointer, not a dereferenced `Worker` — no hint.
    assert_eq!(extract_call_hints(code), vec![None]);
}

#[test]
fn test_sibling_module_constructor_owners_stay_separate() {
    let code = r#"
            mod a {
                impl Factory {
                    fn new() -> WorkerA { WorkerA }
                }
                fn make() {
                    let worker = Factory::new();
                    worker.run();
                }
            }
            mod b {
                impl Factory {
                    fn new() -> WorkerB { WorkerB }
                }
            }
        "#;
    // The unqualified `Factory::new()` inside `mod a` is `a::Factory::new` — module b's
    // same-tail impl must not classify it, and the produced hint is canonical against the
    // call's lexical module. Calls: Factory::new(), worker.run().
    assert_eq!(extract_call_hints(code), vec![None, Some("a::WorkerA".to_string())]);
}

#[test]
fn test_constructor_return_with_same_tail_uses_full_path() {
    let code = r#"
            mod a {
                impl Factory {
                    fn new() -> crate::b::Factory { crate::b::Factory }
                }
            }
            mod b {
                struct Factory;
            }
            fn make() {
                let factory = a::Factory::new();
                factory.run();
            }
        "#;
    // The declaration returns `b::Factory`, which is not self-like merely because its tail
    // matches `a::Factory`. Calls: a::Factory::new(), factory.run().
    assert_eq!(extract_call_hints(code), vec![None, Some("b::Factory".to_string())]);
}

#[test]
fn test_constructor_relative_return_uses_declaration_module() {
    let code = r#"
            mod a {
                struct Worker;
                impl Factory {
                    fn new() -> Worker { Worker }
                }
            }
            mod c {
                fn make() {
                    let worker = crate::a::Factory::new();
                    worker.run();
                }
            }
        "#;
    // `Worker` is written in module a's constructor declaration, not at the call in module c.
    // Calls: crate::a::Factory::new(), worker.run().
    assert_eq!(extract_call_hints(code), vec![None, Some("a::Worker".to_string())]);
}

#[test]
fn test_indistinguishable_constructor_candidates_decline() {
    let code = r#"
            #[cfg(feature = "alpha")]
            impl Factory {
                fn new() -> WorkerA { WorkerA }
            }
            #[cfg(not(feature = "alpha"))]
            impl Factory {
                fn new() -> WorkerB { WorkerB }
            }
            fn make() {
                let worker = Factory::new();
                worker.run();
            }
        "#;
    // Two same-module candidates the canonical path cannot tell apart disagree on the
    // return type — ambiguity must decline, not pick the first traversal hit.
    assert_eq!(extract_call_hints(code), vec![None, None]);
}

#[test]
fn test_unit_returning_constructor_declined() {
    let code = r#"
            impl Worker {
                fn new() {}
            }
            fn test() {
                let w = Worker::new();
                w.run();
            }
        "#;
    // A same-file `new` that returns `()` constructs nothing — the binding's type is unknown.
    assert_eq!(extract_call_hints(code), vec![None, None]);
}

#[test]
fn reassignment_preserves_the_declared_static_type() {
    let code = r#"
            fn test(mut w: Worker, replacement: Worker) {
                w = replacement;
                w.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("Worker".to_string())]);
}

#[test]
fn reassignment_preserves_the_inferred_static_type() {
    let code = r#"
            impl Worker {
                fn new() -> Self { Worker }
            }
            fn test(replacement: Worker) {
                let mut w = Worker::new();
                w = replacement;
                w.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
}

#[test]
fn test_closed_inner_scope_does_not_shadow_parameter() {
    let code = r#"
            fn test(worker: &Alpha) {
                {
                    let worker: Beta;
                }
                worker.run();
            }
        "#;
    let hints = extract_call_hints(code);
    assert_eq!(hints, vec![Some("Alpha".to_string())]);
}

#[test]
fn test_unknown_same_scope_shadow_declined() {
    let code = r#"
            fn test(worker: &Alpha) {
                let worker = unknown;
                worker.run();
            }
        "#;
    let hints = extract_call_hints(code);
    assert_eq!(hints, vec![None]);
}

#[test]
fn conditional_bindings_decline_the_hint() {
    let code = r#"
            fn test() {
                #[cfg(unix)]
                let worker: Alpha = alpha;
                #[cfg(windows)]
                let worker: Beta = beta;
                worker.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None]);
}

#[test]
fn a_preceding_macro_may_introduce_the_receiver_binding() {
    let code = r#"
            fn test(worker: Alpha) {
                bind_worker!(worker);
                worker.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None]);

    let explicit = r#"
            fn test(worker: Alpha) {
                bind_worker!(worker);
                let worker: Beta = beta;
                worker.run();
            }
        "#;
    assert_eq!(extract_call_hints(explicit), vec![Some("Beta".to_string())]);
}

#[test]
fn test_destructuring_shadow_declined() {
    let code = r#"
            fn test(worker: &Alpha) {
                let (worker, _rest): (Beta, u8) = pair;
                worker.run();
            }
        "#;
    let hints = extract_call_hints(code);
    assert_eq!(hints, vec![None]);
}

#[test]
fn test_struct_pattern_shorthand_shadow_declined() {
    let code = r#"
            fn test(x: &Alpha) {
                let Point { x, y: _ } = point;
                x.run();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None]);
}

#[test]
fn test_if_let_struct_pattern_shorthand_shadow_declined() {
    let code = r#"
            fn test(x: &Alpha) {
                if let Point { x, .. } = point {
                    x.run();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None]);
}

/// A smart pointer has an ordered receiver chain that one hint cannot represent. A local trait
/// method on `Box<Worker>` wins before deref reaches `Worker::run`, so naming either owner as
/// authoritative can produce a false edge.
#[test]
fn a_deref_wrapper_declines_single_owner_inference() {
    for wrapper in ["Box<Worker>", "Rc<Worker>", "Arc<Worker>", "std::sync::Arc<Worker>"] {
        let code = format!("fn test(w: {wrapper}) {{ w.run(); }}");
        assert_eq!(
            extract_call_hints(&code),
            vec![None],
            "{wrapper} may dispatch before dereferencing to Worker"
        );
    }
    assert_eq!(extract_call_hints("fn test(w: Cow<'a, Worker>) { w.run(); }"), vec![None]);
    assert_eq!(extract_call_hints("fn test(w: Arc<Box<Worker>>) { w.run(); }"), vec![None]);
    assert_eq!(extract_call_hints("fn test(w: Arc<Mutex<Worker>>) { w.lock(); }"), vec![None]);
}

/// A wrapper name is only the standard pointer while nothing nearer declares it. A crate with
/// its own `struct Box<T>` puts `run` on the WRAPPER, so peeling would name the wrong owner —
/// and since a present-but-failing hint also closes the fallback, it would take the call's last
/// chance too. An import of a standard wrapper still declines because local traits can add
/// wrapper-level methods to it.
#[test]
fn a_locally_declared_wrapper_name_is_not_the_standard_pointer() {
    let shadowed = r#"
            struct Box<T>(T);
            impl<T> Box<T> { fn run(&self) {} }
            fn test(w: Box<Worker>) { w.run(); }
        "#;
    assert_eq!(extract_call_hints(shadowed), vec![Some("Box<Worker>".to_string())]);
    let imported = r#"
            use std::sync::Arc;
            fn test(w: Arc<Worker>) { w.run(); }
        "#;
    assert_eq!(extract_call_hints(imported), vec![None]);
    for (declaration, receiver) in [("Box", "r#Box"), ("r#Box", "Box")] {
        let code = format!(
            "struct {declaration}<T>(T); impl<T> {declaration}<T> {{ fn run(&self) {{}} }} fn \
             test(w: {receiver}<Worker>) {{ w.run(); }}"
        );
        assert_eq!(
            extract_call_hints(&code),
            vec![Some("Box<Worker>".to_string())],
            "raw and ordinary spellings name the same local wrapper"
        );
    }
}

/// A QUALIFIED head is judged by its whole path, not its tail. `custom::Box<Worker>` ends in a
/// wrapper name while being somebody else's type, and no local declaration is in scope to say
/// so — the declaration check looks for a bare `Box`, which a foreign module never provides.
#[test]
fn a_qualified_head_is_a_wrapper_only_when_it_is_rooted_where_the_wrappers_live() {
    let foreign = r#"
            fn test(w: custom::Box<Worker>) { w.run(); }
        "#;
    assert_eq!(extract_call_hints(foreign), vec![Some("custom::Box<Worker>".to_string())]);
    // The standard crates are the roots that name the real pointer, but that pointer may carry
    // a local trait method before deref reaches the inner type.
    for rooted in ["std::boxed::Box", "alloc::sync::Arc", "core::pin::Pin"] {
        let code = format!("struct Box<T>(T);\nfn test(w: {rooted}<Worker>) {{ w.run(); }}");
        assert_eq!(
            extract_call_hints(&code),
            vec![None],
            "{rooted} has more than one possible receiver layer"
        );
    }
    for raw in ["r#Box<Worker>", "r#std::boxed::Box<Worker>"] {
        let code = format!("fn test(w: {raw}) {{ w.run(); }}");
        assert_eq!(extract_call_hints(&code), vec![None], "raw spelling is the same wrapper");
    }
}

/// Only the deref-transparent wrappers peel. `Option<Worker>::run` is a compile error, so
/// unwrapping one would invent a receiver Rust never reaches.
#[test]
fn a_container_that_does_not_deref_keeps_its_own_name() {
    for container in ["Option<Worker>", "Vec<Worker>", "Result<Worker, E>"] {
        let code = format!("fn test(w: {container}) {{ w.run(); }}");
        assert_eq!(
            extract_call_hints(&code),
            vec![Some(container.replace(", ", ","))],
            "{container} does not deref"
        );
    }
}

/// The qualifier of a scoped call is resolved as a PATH, so a local binding of that name says
/// nothing about it. rustc confirms the split: with `mod worker { fn run() -> u16 }` beside
/// `fn f(worker: Alpha)`, `worker::run()` is the module's function while `worker.run()` is the
/// parameter's method. Reading the parameter for the path form bound the call to `Alpha::run`.
#[test]
fn a_path_qualifier_is_not_a_value_receiver() {
    let code = r#"
            mod worker { pub fn run() {} }
            fn test(worker: &Alpha) {
                worker::run();
                worker.run();
            }
        "#;
    // The path call declines; the method call on the same name still reads the parameter.
    assert_eq!(extract_call_hints(code), vec![None, Some("Alpha".to_string())]);
}

/// Lowercase `self::` is a path to the CURRENT MODULE, not the enclosing impl —
/// `self::helper()` calls the module's free function. Only `Self::` names the type.
#[test]
fn a_self_path_qualifier_is_the_module_not_the_type() {
    let code = r#"
            fn helper() {}
            impl Worker {
                fn go(&self) {
                    self::helper();
                    Self::make();
                    self.run();
                }
                fn make() {}
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![
        None,
        Some("Worker".to_string()),
        Some("Worker".to_string())
    ]);
}

/// A type alias is a second name for something else, and the impls are on what it names. A hint
/// of `Alias` probes `Alias::run`, finds nothing, and — because a present receiver type also
/// closes the bare-name fallback — takes the call's last chance with it. Declining leaves that
/// chance open.
#[test]
fn a_local_type_alias_declines_rather_than_naming_itself() {
    let code = r#"
            type Alias = Worker;
            fn test(w: Alias) { w.run(); }
        "#;
    assert_eq!(extract_call_hints(code), vec![None]);
    // A type of the same name that is NOT an alias still resolves normally.
    let concrete = r#"
            struct Alias;
            fn test(w: Alias) { w.run(); }
        "#;
    assert_eq!(extract_call_hints(concrete), vec![Some("Alias".to_string())]);
}

/// A block ITEM owns its name for the whole block, including above its own line. rustc reports
/// the parameter unused here and resolves `worker` to the const, so reading the parameter's
/// type would bind the call to the wrong method. There is no expression to type the item from,
/// so the hint declines.
#[test]
fn a_value_item_declared_below_the_call_still_shadows_the_parameter() {
    for item in [
        "const worker: Beta = Beta;",
        "static worker: Beta = Beta;",
        "fn worker() {}",
        // A unit or tuple struct introduces a CONSTRUCTOR, so it takes the value namespace
        // too.
        "struct worker;",
        "struct worker(u8);",
    ] {
        let code = format!(
            r#"
                fn test(worker: &Alpha) {{
                    worker.run();
                    {item}
                }}
            "#
        );
        assert_eq!(extract_call_hints(&code), vec![None], "{item} shadows the parameter");
    }
}

/// The same rule in the TYPE namespace, on the import side: a block-local `struct Worker`
/// shadows the module's own `use dep::Worker`, so the receiver is the local type and must not
/// keep the import's bare form — which `ReceiverTypeIdentity` would classify as external and
/// suppress. Module-qualifying it is what matches the local `impl`'s own scope, since a
/// function body contributes no scope segment.
#[test]
fn a_block_local_type_shadows_its_modules_import() {
    let code = r#"
            mod inner {
                use dep::Worker;
                fn test() {
                    struct Worker;
                    impl Worker { fn run(&self) {} }
                    let w: Worker = value;
                    w.run();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("inner::Worker".to_string())]);
}

/// The control: with no local declaration the same import still wins, bare.
#[test]
fn an_import_survives_a_block_that_declares_nothing() {
    let code = r#"
            mod inner {
                use dep::Worker;
                fn test() {
                    let w: Worker = value;
                    w.run();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("Worker".to_string())]);
}

/// An imported name keeps its BARE form rather than picking up the lexical module: `Url`
/// here is NOT `inner::Url`. Whether the import leaves the workspace is not decided at
/// extraction — see `receiver_type_identity_classification` and
/// `external_receiver_type_hint_never_binds_locally` for the layer that declines it.
#[test]
fn test_inline_module_import_keeps_the_bare_name() {
    let code = r#"
            mod inner {
                use url::Url;
                fn test(url: &Url) {
                    url.join("child");
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("Url".to_string())]);
}

/// An impl body is not a module boundary, so a file-root `use` is in scope inside it.
#[test]
fn test_impl_method_sees_module_level_import() {
    let code = r#"
            use url::Url;
            struct Client;
            impl Client {
                fn join(u: Url) {
                    u.join("next");
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("Url".to_string())]);
}

/// A `crate::`-rooted import is KNOWN local, but that does not make the lexical module its
/// owner: prefixing the module chain here would produce `inner::Worker`, a module that does
/// not hold the type, and a same-tail `Worker` inside `inner` would then capture the call.
/// The imported name keeps its BARE form — the scope a top-level declaration actually carries.
#[test]
fn test_inline_module_local_import_drops_the_lexical_module_prefix() {
    let code = r#"
            mod inner {
                use crate::workers::Worker;
                fn test(w: &Worker) {
                    w.run();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("Worker".to_string())]);
}

/// `self::`/`super::` roots are resolved against the USE's module, not the reference's, so
/// they get the same treatment as `crate::`.
#[test]
fn test_relative_local_imports_keep_the_bare_name() {
    for import in ["use self::workers::Worker;", "use super::workers::Worker;"] {
        let code = format!("mod inner {{ {import} fn test(w: &Worker) {{ w.run(); }} }}");
        assert_eq!(
            extract_call_hints(&code),
            vec![Some("Worker".to_string())],
            "{import} must not pick up the lexical module prefix"
        );
    }
}

/// An explicitly-written local path is NOT an import — it names its own owner verbatim, so it
/// still earns the fully qualified hint. This is the boundary the rule above must not cross.
#[test]
fn test_inline_module_explicit_path_still_earns_a_qualified_hint() {
    let code = r#"
            mod inner {
                use crate::workers::Worker;
                fn test(w: &crate::workers::Worker) {
                    w.run();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("workers::Worker".to_string())]);
}

/// An import binds the ROOT of a qualified path just as it binds a bare name. Prefixing the
/// lexical module onto `ext::Url` would mint `inner::ext::Url` — a locally-rooted path whose
/// tail retry can land on any local `Url`. The written path is what resolution can classify.
#[test]
fn test_inline_module_qualified_alias_keeps_the_written_path() {
    let code = r#"
            mod inner {
                use url::api as ext;
                fn test(w: &ext::Url) {
                    w.join("child");
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("ext::Url".to_string())]);
}

/// A SIBLING WORKSPACE CRATE is the case extraction must not adjudicate. A `text::Decoder`
/// reached through `use libb::text;` resolves EXACTLY against the sibling's symbols, because
/// the import scope knows `libb` is a local crate root. Extraction sees only the `use`'s own
/// root, which looks identical to a third-party dependency — so it emits the written path and
/// leaves the call to `ReceiverTypeIdentity::classify`. Declining here would destroy a
/// correct resolution.
#[test]
fn test_sibling_workspace_crate_import_keeps_the_written_path() {
    let code = r#"
            use libb::text;
            fn drive(d: &text::Decoder) {
                d.decode();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("text::Decoder".to_string())]);
}

/// A `use` belongs to the module it is written in and does NOT descend into a child `mod`.
/// The file-root `use url::api;` is out of scope inside `mod inner`, where `api` is the child
/// module — so the type is the LOCAL `inner::api::Url`, and the outward walk must stop at the
/// module boundary rather than mistake it for the import.
#[test]
fn test_a_parent_modules_import_does_not_reach_into_a_child_module() {
    let code = r#"
            use url::api;
            mod inner {
                pub mod api { pub struct Url; }
                fn test(u: &api::Url) {
                    u.join("child");
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("inner::api::Url".to_string())]);
}

/// `Self::Assoc` is an associated item of the enclosing impl, not a placeable type — a hint
/// of `Self::Inner` would tail-retry onto any local `Inner`.
#[test]
fn test_self_qualified_associated_type_declined() {
    let code = r#"
            struct Holder;
            impl Holder {
                fn test(w: Self::Inner) {
                    w.run();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None]);
}

/// The same rule with a local import: `use crate::workers;` re-roots `workers::Worker` at the
/// crate root, so the path stands AS WRITTEN — the lexical `inner` is not its owner.
#[test]
fn test_inline_module_qualified_local_import_keeps_the_written_path() {
    let code = r#"
            mod inner {
                use crate::workers;
                fn test(w: &workers::Worker) {
                    w.run();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![Some("workers::Worker".to_string())]);
}

#[test]
fn test_closure_receiver_declined() {
    let code = r#"
            fn test(worker: &Worker) {
                let _closure = || worker.run();
            }
        "#;
    let hints = extract_call_hints(code);
    assert_eq!(hints, vec![None]);
}

#[test]
fn test_crate_qualified_param_type_strips_the_root() {
    let code = r#"
            fn test(w: &crate::workers::Worker) {
                w.run();
            }
        "#;
    // `crate::` never appears in a container-based scope path — the stored hint drops it.
    assert_eq!(extract_call_hints(code), vec![Some("workers::Worker".to_string())]);
}

#[test]
fn test_super_qualified_param_type_declined() {
    let code = r#"
            fn test(w: &super::Worker) {
                w.run();
            }
        "#;
    // `super::` is relative to a module the extractor cannot resolve — decline.
    assert_eq!(extract_call_hints(code), vec![None]);
}

#[test]
fn test_for_loop_rebind_declined() {
    let code = r#"
            fn test(worker: &Alpha) {
                for worker in fetch_betas() {
                    worker.run();
                }
                worker.report();
            }
        "#;
    // fetch_betas() has no receiver; the loop-rebound worker.run() must decline; the call
    // after the loop is back in the parameter's scope and sees Alpha again.
    assert_eq!(extract_call_hints(code), vec![None, None, Some("Alpha".to_string())]);
}

#[test]
fn test_if_let_rebind_declined() {
    let code = r#"
            fn test(msg: &Alpha) {
                if let Some(msg) = incoming() {
                    msg.send();
                }
            }
        "#;
    // incoming() has no receiver; msg.send() must not inherit the Alpha parameter.
    assert_eq!(extract_call_hints(code), vec![None, None]);
}

#[test]
fn test_while_let_rebind_declined() {
    let code = r#"
            fn test(job: &Alpha) {
                while let Some(job) = queue_pop() {
                    job.execute();
                }
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![None, None]);
}

#[test]
fn test_match_arm_rebind_scoped_per_arm() {
    let code = r#"
            fn test(event: &Alpha) {
                match next_event() {
                    Some(event) => event.apply(),
                    None => event.apply(),
                }
            }
        "#;
    // The first arm rebinds `event` (decline); the second arm's pattern does not, so the
    // Alpha parameter is still the receiver there — arm scoping is per-arm, not per-match.
    assert_eq!(extract_call_hints(code), vec![None, None, Some("Alpha".to_string())]);
}

#[test]
fn test_generic_and_trait_object_receivers_declined() {
    let generic = r#"
            fn test<T>(worker: T) {
                worker.run();
            }
        "#;
    let trait_object = r#"
            fn test(worker: &dyn Service) {
                worker.run();
            }
        "#;
    assert_eq!(extract_call_hints(generic), vec![None]);
    assert_eq!(extract_call_hints(trait_object), vec![None]);
}

#[test]
fn concrete_generic_receiver_arguments_are_preserved() {
    let code = r#"
            fn test(first: Foo<u8>, second: Foo<u16>) {
                first.run::<u8>();
                second.run::<u16>();
            }
        "#;
    assert_eq!(extract_call_hints(code), vec![
        Some("Foo<u8>".to_string()),
        Some("Foo<u16>".to_string()),
    ]);
}

#[test]
fn generic_arguments_keep_the_existing_nameability_boundary() {
    for type_name in [
        "Envelope<(Worker, Other)>",
        "Matrix<[u8; 4]>",
        "Callback<fn() -> Worker>",
        "Marker<'*'>",
        "Envelope<Ty!{\"for<\"}>",
        "Envelope<Ty!{for<}>",
    ] {
        let code = format!("fn f(value: {type_name}) {{ value.run(); }}");
        assert_eq!(
            extract_call_hints(&code),
            vec![None],
            "the structural refactor must not widen persisted hints"
        );
    }
}

#[test]
fn canonical_constructor_returns_still_decline_aliases_and_binders() {
    let raw_alias = r#"
            struct Worker;
            type r#Alias = Worker;
            struct Factory;
            impl Factory { fn new() -> r#Alias { Worker } }
            fn f() { let worker = Factory::new(); worker.run(); }
        "#;
    assert_eq!(extract_call_hints(raw_alias), vec![None, None]);

    let decomposed = "Cafe\u{301}";
    let generic = format!(
        "struct Worker; struct Factory; impl Factory {{ fn new<{decomposed}>(value: {decomposed}) \
         -> {decomposed} {{ value }} }} fn f() {{ let worker = Factory::new(Worker); \
         worker.run(); }}"
    );
    assert_eq!(extract_call_hints(&generic), vec![None, None]);
}

#[test]
fn canonical_constructor_returns_recognize_raw_and_nfc_imports() {
    let raw = r#"
            mod dep { pub struct Worker; }
            mod inner {
                use crate::dep::r#Worker;
                struct Factory;
                impl Factory { fn new() -> r#Worker { todo!() } }
                fn f() { let worker = Factory::new(); worker.run(); }
            }
        "#;
    assert_eq!(extract_call_hints(raw), vec![None, Some("Worker".to_string())]);

    let decomposed = "Cafe\u{301}";
    let imported = format!(
        "mod dep {{ pub struct Café; }} mod inner {{ use crate::dep::{decomposed}; struct \
         Factory; impl Factory {{ fn new() -> {decomposed} {{ todo!() }} }} fn f() {{ let value = \
         Factory::new(); value.run(); }} }}"
    );
    assert_eq!(extract_call_hints(&imported), vec![None, Some("Café".to_string())]);
}

#[test]
fn qualified_constructor_modules_use_canonical_identifiers() {
    let raw = r#"
            mod r#type {
                pub struct Worker;
                pub struct Factory;
                impl Factory { pub fn new() -> Worker { Worker } }
            }
            fn f() { let worker = r#type::Factory::new(); worker.run(); }
        "#;
    assert_eq!(extract_call_hints(raw), vec![None, Some("type::Worker".to_string())]);

    let decomposed = "Cafe\u{301}";
    let unicode = format!(
        "mod {decomposed} {{ pub struct Worker; pub struct Factory; impl Factory {{ pub fn new() \
         -> Worker {{ Worker }} }} }} fn f() {{ let worker = {decomposed}::Factory::new(); \
         worker.run(); }}"
    );
    assert_eq!(extract_call_hints(&unicode), vec![None, Some("Café::Worker".to_string())]);
}

#[test]
fn comments_do_not_make_a_receiver_type_unnameable() {
    assert_eq!(extract_call_hints("fn f(w: Foo</* note */ Worker>) { w.run(); }"), vec![Some(
        "Foo<Worker>".to_string()
    )]);
    assert_eq!(extract_call_hints("fn f(w: &/* note */ Worker) { w.run(); }"), vec![Some(
        "Worker".to_string()
    )]);
}

#[test]
fn turbofish_dot_calls_keep_receiver_context() {
    let code = "fn test(worker: Worker) { worker.run::<u8>(); factory().run(); }";
    let calls = edge_candidates(
        std::path::Path::new("lib.rs"),
        rag_rat_base::language::Language::Rust,
        code,
        &[],
    )
    .unwrap()
    .into_iter()
    .filter(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == "run")
    .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);

    let typed = calls.iter().find(|edge| edge.receiver_hint.as_deref() == Some("worker")).unwrap();
    assert_eq!(typed.receiver_type_hint.as_deref(), Some("Worker"));
    assert_eq!(typed.target_qualified_name.as_deref(), Some("worker::run"));

    let unknown =
        calls.iter().find(|edge| edge.receiver_hint.as_deref() == Some("factory()")).unwrap();
    assert_eq!(unknown.receiver_type_hint, None);
    assert_eq!(unknown.target_qualified_name.as_deref(), Some("factory()::run"));
}

#[test]
fn receiver_hint_scan_handles_twenty_thousand_nearby_bindings() {
    let mut code = String::from("fn test() {");
    for _ in 0..20_000 {
        code.push_str("let worker = value; worker.run();");
    }
    code.push('}');

    let hints = extract_call_hints(&code);
    assert_eq!(hints.len(), 20_000);
    assert!(hints.into_iter().all(|hint| hint.is_none()));
}

/// A plain-identifier initializer never enters the same-file declaration scan, so the case
/// above pins only the backward `let` walk. This one pins the scan itself, on the answer it
/// gives most often: the callee is declared in another file.
///
/// The ceiling is the ORDER inside that scan. Deciding "no impl here declares it" reads this
/// file's impl headers; canonicalizing the owner instead scans every enclosing scope for a
/// shadowing item or import, which is a pass over the enclosing block. Pay the second one
/// before the first and the cost is bindings x block size, and this fixture stops finishing.
#[test]
fn receiver_hint_scan_handles_twenty_thousand_scoped_call_initializers() {
    let mut code = String::from("fn test() {");
    for _ in 0..20_000 {
        code.push_str("let worker = Owner::make(); worker.run();");
    }
    code.push('}');

    let hints = extract_call_hints(&code);
    assert_eq!(hints.len(), 40_000);
    assert!(hints.into_iter().all(|hint| hint.is_none()));
}

/// The case above holds no impl at all, so its scan stops at the header walk. This one pins
/// where that walk leads: an impl whose tail matches sends the binding on to canonicalize its
/// owner — a pass over the enclosing block — and the file's other impls are re-walked for every
/// binding that asks. Both terms are paid under any callee name now, where the `new`/`default`
/// gate used to return before either, so the header walk has to decline a non-matching impl on
/// an identifier compare rather than a canonical render.
#[test]
fn receiver_hint_scan_handles_a_matching_impl_among_thousands() {
    let mut code = String::from("impl Owner { fn make() -> Self { todo!() } }\n");
    for i in 0..2_000 {
        code.push_str(&format!("impl Other{i} {{ fn make() -> Self {{ todo!() }} }}\n"));
    }
    code.push_str("fn test() {");
    for _ in 0..1_000 {
        code.push_str("let worker = Owner::make(); worker.run();");
    }
    code.push('}');

    let hints = extract_call_hints(&code);
    assert_eq!(hints.len(), 2_000);
    assert_eq!(
        hints.iter().filter(|hint| hint.as_deref() == Some("Owner")).count(),
        1_000,
        "the one impl whose tail matches types every binding",
    );
}

fn impl_facts(
    source: &str,
    kind: EdgeKind,
) -> Vec<crate::index::languages::test_support::EdgeFact> {
    crate::index::languages::test_support::edge_facts("src/lib.rs", Language::Rust, source, kind)
}

/// The trait comes from the impl's `trait` field, so a generic binder or a lifetime in the header
/// is never the implemented trait.
#[test]
fn an_impl_implements_its_trait_field() {
    use crate::index::languages::test_support::fact;
    assert_eq!(impl_facts("impl<T: Display> Trait for Foo<T> {}", EdgeKind::Implements), vec![
        fact(EdgeKind::Implements, "Trait", None, None)
    ]);
    assert_eq!(impl_facts("impl<'a> Tr for &'a Bar {}", EdgeKind::Implements), vec![fact(
        EdgeKind::Implements,
        "Tr",
        None,
        None
    )]);
    assert_eq!(impl_facts("impl a::Tr<u8> for b::Baz {}", EdgeKind::Implements), vec![fact(
        EdgeKind::Implements,
        "Tr",
        Some("a::Tr"),
        None
    )]);
}

/// A `for` loop in an inherent impl's body does not make it a trait impl.
#[test]
fn an_inherent_impl_with_a_for_loop_implements_nothing() {
    let source = "impl Foo { fn f(v: Vec<u8>) { for x in v { g(x); } } }";
    assert_eq!(impl_facts(source, EdgeKind::Implements), vec![]);
}

/// The edge hangs off the impl symbol that encloses it, by that symbol's qualified name — not a
/// bare name split out of the header.
#[test]
fn an_implements_edge_comes_from_the_impl_symbol() {
    let source = "impl<T: Display> Trait for Foo<T> {}";
    let impl_symbol = IndexedSymbol {
        id: 7,
        file_id: 0,
        language: "rust".to_string(),
        name: "Foo".to_string(),
        qualified_name: "src/lib.rs::Foo<_> as Trait".to_string(),
        scope_path: "Foo<_> as Trait".to_string(),
        kind: "impl".to_string(),
        start_byte: 0,
        end_byte: source.len(),
        start_line: 1,
        end_line: 1,
    };
    let edges = syntactic_edges(
        std::path::Path::new("src/lib.rs"),
        Language::Rust,
        source,
        std::slice::from_ref(&impl_symbol),
    )
    .unwrap();
    let implements =
        edges.iter().find(|edge| edge.edge_kind == EdgeKind::Implements).expect("implements edge");
    assert_eq!(implements.from_symbol_id, Some(7));
    assert_eq!(implements.from_name.as_deref(), Some("src/lib.rs::Foo<_> as Trait"));
}

/// A subscript, a call result or a closure is a callee with no name: the identifiers inside it are
/// an index, an argument or a parameter, never the called function.
#[test]
fn a_call_on_an_unnamed_value_names_no_callee() {
    use crate::index::languages::test_support::fact;
    let calls = |body: &str| impl_facts(&format!("fn f() {{ {body} }}"), EdgeKind::CallsName);
    assert_eq!(calls("handlers[key](x);"), vec![]);
    assert_eq!(calls("make(a)(b);"), vec![fact(EdgeKind::CallsName, "make", None, None)]);
    assert_eq!(calls("(|x| x)(1);"), vec![]);
    assert_eq!(calls("get(k)?(x);"), vec![fact(EdgeKind::CallsName, "get", None, None)]);
    assert_eq!(calls("self.0(x);"), vec![]);
    assert_eq!(calls("(*make(a))(x);"), vec![fact(EdgeKind::CallsName, "make", None, None)]);
    // A named callee is still read through a turbofish and parentheses.
    assert_eq!(calls("a::run::<T>(x);"), vec![fact(
        EdgeKind::CallsName,
        "run",
        Some("a::run"),
        Some("a")
    )]);
    assert_eq!(calls("(run)(x);"), vec![fact(EdgeKind::CallsName, "run", None, None)]);
}

/// A generic type references its base type and each type argument once: the `generic_type` node
/// itself is not a reference to its last argument.
#[test]
fn a_generic_type_references_each_type_once() {
    use crate::index::languages::test_support::fact;
    let types = impl_facts("fn f() { let v: HashMap<Key, Val> = x; }", EdgeKind::ReferencesType);
    assert_eq!(types, vec![
        fact(EdgeKind::ReferencesType, "HashMap", None, None),
        fact(EdgeKind::ReferencesType, "Key", None, None),
        fact(EdgeKind::ReferencesType, "Val", None, None),
    ]);
}
