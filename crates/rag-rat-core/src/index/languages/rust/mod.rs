use std::path::Path;

use tree_sitter::Node;

use super::{
    ParserBackend, QualifiedRoot, ReceiverFallback, ReferenceDisposition, ResolutionPolicy,
    SymbolMatch, TypeBinding,
};
use crate::index::edges::{EdgeKind, scope_grammar};
use crate::index::parser::{self, ParsedSymbolFact, ParserKind};

mod dispatch;
mod edges;
pub(super) use edges::rust_edges;

pub(super) static SUPPORT: Rust = Rust;

pub(super) struct Rust;

impl ParserBackend for Rust {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &[
            "const", "enum", "function", "impl", "macro", "module", "static", "struct", "trait",
            "type",
        ]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Rust
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        match node.kind() {
            "function_item" => Some(("function", parser::child_name(node)?)),
            "struct_item" => Some(("struct", parser::child_name(node)?)),
            "enum_item" => Some(("enum", parser::child_name(node)?)),
            "trait_item" => Some(("trait", parser::child_name(node)?)),
            "impl_item" => Some(("impl", impl_name(node).unwrap_or(node))),
            "mod_item" => Some(("module", parser::child_name(node)?)),
            "const_item" => Some(("const", parser::child_name(node)?)),
            "static_item" => Some(("static", parser::child_name(node)?)),
            "type_item" => Some(("type", parser::child_name(node)?)),
            "macro_definition" => Some(("macro", parser::child_name(node)?)),
            _ => None,
        }
    }

    /// An impl is NAMED by the same canonical renderer that gives it its scope.
    ///
    /// The default takes the name node's source text, which for `impl<T> Tr for Foo<T>` is
    /// `Foo<T>` — so the binder-renamed twin is named `Foo<U>`. Both feed `LogicalSymbolKey`
    /// alongside the scope, so the two impl BLOCKS kept separate handles even once their members
    /// had been given one: the canonicalization reached the methods and stopped short of their
    /// container. Rendering the name the same way closes that.
    fn symbol_name(&self, node: Node<'_>, name_node: Node<'_>, text: &str) -> String {
        if node.kind() == "impl_item" && name_node.id() != node.id() {
            return render_owner(name_node, text, &declared_binders(node, text));
        }
        parser::node_text(name_node, text).unwrap_or_default()
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = match node.kind() {
            "mod_item" | "trait_item" => parser::child_name(node)?,
            "impl_item" => {
                // Both halves of the segment substitute the same binders, and `scope_segment` runs
                // once per member of the impl — so the parameter list is walked once here rather
                // than twice per method.
                let binders = declared_binders(node, text);
                let segment = impl_owner_segment(node, text, &binders)?;
                // `impl Trait for Type`: keep the trait in the scope segment (`Type as Trait`)
                // so two traits' same-named, same-signature methods on one type stay DISTINCT
                // logical symbols instead of collapsing into one. Resolution folds the
                // ` as Trait` marker away (`normalized_scope_path`), so the receiver surface both
                // traits expose is still `Type::method` — and a call that could hit either
                // declines as ambiguous (#567).
                return match node.child_by_field_name("trait") {
                    Some(trait_node) => Some(match trait_marker(trait_node, text, &binders) {
                        Some(marker) => format!("{segment} as {marker}"),
                        None => segment,
                    }),
                    None => Some(segment),
                };
            },
            _ => return None,
        };
        parser::node_text(name, text)
    }

    /// An impl's name is its self type, so `impl A for W` and `impl B for W` would both be `W` —
    /// and with the trait on a later line their captured signatures are `impl` too, leaving the two
    /// impl symbols with one logical key. The `Type as Trait` segment is what tells them apart.
    fn own_scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        (node.kind() == "impl_item").then(|| self.scope_segment(node, text)).flatten()
    }

    fn is_test_symbol(&self, text: &str, node: Node<'_>, _scope_path: &str, _name: &str) -> bool {
        attribute_items(text, node).iter().any(|attribute| attribute_is_test(attribute))
            || in_cfg_test_module(node, text)
    }

    fn symbol_facts(&self, text: &str, node: Node<'_>) -> Vec<ParsedSymbolFact> {
        let mut facts = Vec::new();
        for attribute in attribute_items(text, node) {
            if attribute.contains("uniffi::export") || attribute.contains("::uniffi::export") {
                facts.push(ParsedSymbolFact {
                    kind: "rust_attr".to_string(),
                    value: "uniffi_export".to_string(),
                });
            }
        }
        facts.sort_by(|left, right| (&left.kind, &left.value).cmp(&(&right.kind, &right.value)));
        facts.dedup();
        facts
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "use_declaration"
    }
}

/// The trait half of a `Type as Trait` scope marker: the trait ref rendered canonically, with
/// `::` rewritten to `.`.
///
/// The WHOLE path, because a tail alone gives `impl a::Runs for W` and `impl b::Runs for W` the
/// same scope. `.` for `::`, because the marker has to stay ONE `::`-segment for the resolution
/// fold to split on. Generic ARGUMENTS are kept — `impl TryFrom<&str> for C` and
/// `impl TryFrom<String> for C` coexist, so they are two entities and their same-signature
/// associated types must not share one logical symbol.
///
/// Import spelling is NOT canonicalized and splits deliberately: `use std::fmt;` +
/// `impl fmt::Display` versus `impl std::fmt::Display` cannot be told apart without name
/// resolution, and folding use-expansion into identity would couple `stable_id` to a file's import
/// list — every `use` refactor would churn logical ids and strand the bindings anchored to them.
fn trait_marker(trait_node: Node<'_>, text: &str, binders: &[String]) -> Option<String> {
    let rendered = render_owner(trait_node, text, binders);
    // Rewrite EXACTLY the separators the resolution fold splits on — nothing else needs to move,
    // and moving it costs identity. A `::` nested anywhere is part of an expression or an argument,
    // not the path: `a::c` (a module's const) and `a.c` (a value's field) are both legal in
    // `Tr<[u8; _]>` and name different values, so rewriting one into the other renders two distinct
    // impls identically. Splitting with `segments` is how the two stay in step — it is the same
    // top-level rule the fold applies, rather than a second copy of it written out beside it.
    let marker = scope_grammar::segments(&rendered).join(".");
    (!marker.is_empty()).then_some(marker)
}

/// The impl's SELF TYPE, rendered as identity rather than picked out as a node.
///
/// Two impls in one file are the same entity exactly when a single compilation could not hold
/// both, and rustc's coherence check is the authority. `impl Runs for W`, `for &W` and
/// `for *const W` all compile together, so the wrapper is part of who they are; peeling it gives
/// all three one scope and collapses their same-signature members. A non-nominal target (`()`,
/// `(A, B)`, `[T]`, `dyn X`) has no node to name, and declining used to drop the impl's scope
/// segment ENTIRELY, leaving its members at file scope where they collide with free functions.
fn impl_owner_segment(node: Node<'_>, text: &str, binders: &[String]) -> Option<String> {
    let target = node.child_by_field_name("type")?;
    let rendered = render_owner(target, text, binders);
    (!rendered.is_empty()).then_some(rendered)
}

/// The generic parameter names an item declares itself.
fn declared_binders(node: Node<'_>, text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let Some(parameters) = node.child_by_field_name("type_parameters") else {
        return names;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if matches!(parameter.kind(), "type_parameter" | "const_parameter")
            && let Some(name) = crate::index::edges::child_name_text(parameter, text)
        {
            // `r#T` and `T` are one parameter — the raw prefix is lexical escaping. The occurrence
            // side strips it before matching, so a declaration that kept it never matched its own
            // uses and the binder went un-substituted. NFC for the same reason the occurrence side
            // normalizes: the two must be byte-equal to compare.
            let name = nfc_ident(name.strip_prefix("r#").unwrap_or(&name)).into_owned();
            // A CONST parameter lives in the value namespace, and a bare identifier in argument
            // position is read as a TYPE. So when a type of the same name is in scope, rustc binds
            // that name to the TYPE there and to the parameter only inside a braced const
            // expression: `struct N; impl<const N: usize> Tr<{ N }> for F<N>` puts the struct in
            // `F<N>`, which is why it coexists with its `M`-spelled twin instead of conflicting.
            // Substituting the name would render the two identically and merge two entities, so a
            // const parameter a type declaration shadows is not treated as a binder at all.
            //
            // Erring toward the shadow costs a SPLIT in the narrower case where such a parameter is
            // used only inside braces — two entities kept apart, rather than two merged.
            if parameter.kind() == "const_parameter"
                && a_type_of_that_name_is_in_scope(node, &name, text)
            {
                continue;
            }
            names.push(name);
        }
    }
    names
}

/// Whether a `use` declaration could bring `name` into scope. A wildcard could bring in anything,
/// so it answers yes for every name — see the caller for why erring this way is the safe direction.
///
/// Walks the use-tree with an explicit worklist rather than recursion: a nested group nests without
/// bound, and this stays off the recursive-descent path entirely.
fn use_could_bind(declaration: Node<'_>, name: &str, text: &str) -> bool {
    let Some(argument) = declaration.child_by_field_name("argument") else {
        return false;
    };
    let mut cursor = declaration.walk();
    let mut pending = vec![argument];
    while let Some(node) = pending.pop() {
        match node.kind() {
            // A wildcard binds names this pass cannot enumerate.
            "use_wildcard" => return true,
            // `use p::X as Y` binds Y, not X.
            "use_as_clause" => {
                if let Some(alias) = node.child_by_field_name("alias")
                    && node_src(alias, text).trim().trim_start_matches("r#") == name
                {
                    return true;
                }
                continue;
            },
            // A group's members are separate leaves; the path prefix binds nothing on its own.
            "use_list" | "scoped_use_list" => {
                pending.extend(node.named_children(&mut cursor).filter(|child| {
                    !matches!(child.kind(), "scoped_identifier" | "identifier")
                        || child.parent().is_some_and(|parent| parent.kind() == "use_list")
                }));
                continue;
            },
            // A path binds its LAST segment.
            "scoped_identifier" => {
                if let Some(leaf) = node.child_by_field_name("name")
                    && node_src(leaf, text).trim().trim_start_matches("r#") == name
                {
                    return true;
                }
                continue;
            },
            "identifier" | "type_identifier" => {
                if node_src(node, text).trim().trim_start_matches("r#") == name {
                    return true;
                }
                continue;
            },
            _ => pending.extend(node.named_children(&mut cursor)),
        }
    }
    false
}

/// A Rust identifier normalized the way rustc compares them — NFC. ASCII is already NFC, which is
/// the overwhelming case, so it borrows and only the rare non-ASCII identifier allocates.
fn nfc_ident(token: &str) -> std::borrow::Cow<'_, str> {
    if token.is_ascii() {
        std::borrow::Cow::Borrowed(token)
    } else {
        std::borrow::Cow::Owned(rag_rat_base::canonical::nfc(token))
    }
}

/// Types the std prelude brings into every module without a `use`. A const parameter named after
/// one of these is shadowed by the type in argument position, exactly as a locally declared or
/// imported type shadows it — so the same coexisting impls (`impl<const Vec: usize> Tr<{ Vec }> for
/// F<Vec<u8>>` and its `Box` twin) must not be merged by substituting the name.
const PRELUDE_TYPES: [&str; 5] = ["Box", "Vec", "String", "Option", "Result"];

/// Whether a TYPE called `name` could be in scope, which is what decides whether a bare identifier
/// in argument position means that type or a same-named const parameter.
///
/// An import counts, and so does a GLOB: `use p::{N, M};` beside `impl<const N: usize> Tr<{ N }>
/// for F<N>` makes that impl coexist with its `M`-spelled twin, because `F<N>` names the imported
/// type. Reading only local declarations rendered both `F<_>` and merged two entities.
///
/// Deliberately answers "could", not "does" — a glob is treated as binding every name. Being wrong
/// that way leaves a const binder un-substituted, which SPLITS two spellings of one impl. Being
/// wrong the other way MERGES two impls that coexist, handing them one logical handle and each
/// other's bindings. Only the first is recoverable.
fn a_type_of_that_name_is_in_scope(from: Node<'_>, name: &str, text: &str) -> bool {
    if PRELUDE_TYPES.contains(&name) {
        return true;
    }
    binding_scopes(from).any(|scope| {
        let mut cursor = scope.walk();
        scope.named_children(&mut cursor).any(|item| {
            if matches!(
                item.kind(),
                "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item"
            ) {
                return crate::index::edges::child_name_text(item, text)
                    .is_some_and(|declared| declared == name);
            }
            item.kind() == "use_declaration" && use_could_bind(item, name, text)
        })
    })
}

/// The scopes that can bind a name for a reference at `from`, innermost first: enclosing blocks up
/// to and including the nearest module body, which is where the walk stops.
///
/// The stop is the load-bearing part — an item declared outside a `mod` is not in scope for
/// anything inside it — and it is stated once here so the predicates built on it cannot drift apart
/// about where a name stops being visible.
pub(super) fn binding_scopes(from: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut current = Some(from);
    let mut done = false;
    std::iter::from_fn(move || {
        while !done {
            let scope = current?;
            current = scope.parent();
            let module_body = scope.kind() == "source_file"
                || (scope.kind() == "declaration_list"
                    && scope.parent().is_some_and(|parent| parent.kind() == "mod_item"));
            done = module_body;
            if module_body || scope.kind() == "block" {
                return Some(scope);
            }
        }
        None
    })
}

/// Render a type as CANONICAL IDENTITY.
///
/// Three transformations, each one derived from what rustc's coherence check does — the authority
/// on whether two impls are one entity:
///
/// - **Binder names become `_`.** `impl<T> Foo<T>` and `impl<U> Foo<U>` are a conflicting-
///   implementations error, and E0119 renders the conflict as `Foo<_>`; the binder's spelling is
///   never what separates two impls. Substitution follows what NAMES a binder this impl declares: a
///   bare identifier, or the ROOT of a qualified path (`T::Assoc`, which rustc also reports as one
///   impl under renaming, rendering it `<_ as Bound>::Assoc`). A qualified path's TAIL is excluded,
///   because it names an item in another namespace: `Pair<T, module::T>` and `Pair<V, module::V>`
///   compile together, so folding their tails would MERGE two entities. In binder position a
///   generic parameter shadows any same-named outer type, so substitution cannot fire on a concrete
///   type, and a missed occurrence only splits — the failure mode stays one-sided by construction.
/// - **Concrete arguments are preserved.** `impl A for F<u8>` and `impl A for F<u16>` compile
///   together, so the argument is identity. This is the half that erasing generics wholesale got
///   backwards.
/// - **Lifetimes are erased**, along with the syntax that held only them (`for<'a>`, a trailing
///   `+`), because coherence erases lifetimes before comparing: `impl<'a> Tr for &'a W` and `impl
///   Tr for &'static W` conflict.
///
/// Cosmetic spelling is normalized so it cannot become identity: whitespace, a raw identifier's
/// `r#`, and redundant parentheses around a single type (`(W)` is `W`; `(W,)` is a one-tuple and
/// stays). A leading `::` is NOT cosmetic and is kept: since the 2018 edition it selects the extern
/// prelude, so a crate holding a local `mod core` can implement its `core::Marker` and
/// `::core::fmt::Debug` for one type — two different traits. That matches the rule this file
/// already follows for the rest of a path, where `fmt::Display` and `std::fmt::Display` stay
/// separate because telling them apart needs name resolution.
///
/// A const-generic block is carried through near-verbatim — it holds an expression whose text is
/// part of the type, and whose `<`/`,` are not type punctuation. Only what cannot carry meaning is
/// normalized inside it: comments, and the width of a whitespace run outside a literal.
fn render_owner(node: Node<'_>, text: &str, binders: &[String]) -> String {
    let mut out = String::new();
    print_type(node, text, binders, Position::Type, &mut out);
    out.trim().to_string()
}

/// Whether an identifier at this point could NAME one of the impl's binders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    /// A type position: a bare `T` here is the parameter.
    Type,
    /// The tail of a qualified path — `module::T` is `module`'s `T`, never this impl's parameter.
    Name,
}

/// Print one type node canonically.
///
/// Every byte of the output is EMITTED, not copied, except at the leaves: an identifier's own
/// token, and the two opaque regions (a const-generic block, a macro's argument list) whose text is
/// the type. That is what makes spelling unrepresentable rather than repeatedly patched — a
/// comment, a newline, or a space around a delimiter has no node to be printed from, so it cannot
/// reach identity in the first place.
///
/// An unrecognized node prints its source opaquely. That is a SPLIT if the grammar ever grows a
/// shape this does not model, which is the tolerable direction; guessing at its structure could
/// merge two owners.
fn print_type(
    node: Node<'_>,
    text: &str,
    binders: &[String],
    position: Position,
    out: &mut String,
) {
    // grow_stack: a type nests without bound (`Vec<Vec<Vec<…>>>`), so grow rather than overflow the
    // indexer on a hostile or generated file (#543).
    rag_rat_base::stack::grow_stack(|| print_type_inner(node, text, binders, position, out))
}

fn print_type_inner(
    node: Node<'_>,
    text: &str,
    binders: &[String],
    position: Position,
    out: &mut String,
) {
    match node.kind() {
        // A lifetime is erased before coherence compares, so it is never printed — and because that
        // happens per NODE, it cannot reach inside a macro's tokens, where `M!('a)` and
        // `M!('static)` may expand to different types.
        // Lifetimes are erased. They HAVE to be: `impl<'a> Tr for &'a W` and `impl Tr for &'static
        // W` overlap, so rustc rejects them as conflicting — one entity, one identity. The
        // single exception is a function pointer under the deprecated
        // `coherence_leak_check` behavior, where `fn(&'static W)` and `for<'a> fn(&'a W)`
        // can coexist; those merge here. Splitting them would mean carving a
        // lifetime-sensitive case out of a rule whose whole point is that lifetimes never
        // reach identity, to serve a shape a future-incompatibility lint is removing. The
        // merge is the accepted cost.
        "lifetime" | "for_lifetimes" => {},
        "type_identifier" | "identifier" | "primitive_type" | "field_identifier" => {
            let token = node_src(node, text).trim();
            // `r#Foo` and `Foo` name one type — the raw prefix is lexical escaping, and the
            // non-raw spelling of a keyword cannot exist as a competing type.
            let token = token.strip_prefix("r#").unwrap_or(token);
            // rustc normalizes identifiers to NFC, so `Café` composed and decomposed name one
            // type; without this they render different owners and split one impl's members.
            let token = nfc_ident(token);
            let token = token.as_ref();
            // In binder position a generic parameter shadows any same-named outer type, so this
            // cannot fire on a concrete type; in NAME position it is another namespace's item.
            if position == Position::Type && binders.iter().any(|binder| binder == token) {
                out.push('_');
            } else {
                out.push_str(token);
            }
        },
        "scoped_type_identifier" | "scoped_identifier" => {
            // An absent path means a leading `::`, which is NOT cosmetic: since the 2018 edition
            // it selects the extern prelude, so a crate with a local `mod core` has both a
            // `core::Marker` and a `::core::fmt::Debug` to implement. Printing nothing before the
            // separator is what keeps the two apart.
            if let Some(path) = node.child_by_field_name("path") {
                // The ROOT is a real type position: in `T::Assoc` the `T` IS the binder, and rustc
                // renders that conflict `<_ as Bound>::Assoc`.
                print_type(path, text, binders, Position::Type, out);
            }
            out.push_str("::");
            if let Some(name) = node.child_by_field_name("name") {
                print_type(name, text, binders, Position::Name, out);
            }
        },
        "generic_type" => {
            if let Some(inner) = node.child_by_field_name("type") {
                print_type(inner, text, binders, position, out);
            }
            if let Some(args) = node.child_by_field_name("type_arguments") {
                print_type(args, text, binders, Position::Type, out);
            }
        },
        // Concrete arguments are identity — `F<u8>` and `F<u16>` compile together. An argument list
        // that held only lifetimes leaves nothing, and the brackets go with it.
        "type_arguments" | "type_parameters" => {
            print_joined(node, text, binders, ",", out, true);
        },
        "reference_type" => {
            // The pointer shape is identity: `for W`, `for &W` and `for *const W` all compile
            // together, so peeling the wrapper would collapse three impls into one.
            out.push('&');
            if has_kind(node, "mutable_specifier") {
                out.push_str("mut ");
            }
            if let Some(inner) = node.child_by_field_name("type") {
                print_type(inner, text, binders, Position::Type, out);
            }
        },
        "pointer_type" => {
            out.push('*');
            out.push_str(if has_kind(node, "mutable_specifier") { "mut " } else { "const " });
            if let Some(inner) = node.child_by_field_name("type") {
                print_type(inner, text, binders, Position::Type, out);
            }
        },
        "tuple_type" => {
            let mut cursor = node.walk();
            let members = node.named_children(&mut cursor).count();
            // `(W)` is `W`; `(W,)` is a one-tuple. The grammar gives both ONE child, so the
            // trailing comma is the only thing that tells them apart.
            let one_tuple = members == 1 && has_kind(node, ",");
            if members == 1 && !one_tuple {
                let mut inner = node.walk();
                if let Some(only) = node.named_children(&mut inner).next() {
                    print_type(only, text, binders, position, out);
                }
                return;
            }
            out.push('(');
            print_joined(node, text, binders, ",", out, false);
            if one_tuple {
                out.push(',');
            }
            out.push(')');
        },
        "unit_type" => out.push_str("()"),
        "array_type" => {
            out.push('[');
            if let Some(element) = node.child_by_field_name("element") {
                print_type(element, text, binders, Position::Type, out);
            }
            if let Some(length) = node.child_by_field_name("length") {
                out.push(';');
                print_type(length, text, binders, Position::Type, out);
            }
            out.push(']');
        },
        "function_type" => {
            match node.child_by_field_name("trait") {
                Some(name) => print_type(name, text, binders, Position::Type, out),
                None => out.push_str("fn"),
            }
            if let Some(parameters) = node.child_by_field_name("parameters") {
                print_type(parameters, text, binders, Position::Type, out);
            }
            if let Some(ret) = node.child_by_field_name("return_type") {
                out.push_str("->");
                print_type(ret, text, binders, Position::Type, out);
            }
        },
        "parameters" => {
            out.push('(');
            print_joined(node, text, binders, ",", out, false);
            out.push(')');
        },
        // `dyn A + 'a` and `dyn A` are one type once lifetimes are erased, and a bound list that
        // loses one leaves no separator behind because the `+` is emitted between what REMAINS.
        "bounded_type" => {
            print_joined(node, text, binders, "+", out, false);
        },
        "dynamic_type" => {
            out.push_str("dyn ");
            if let Some(inner) = node.child_by_field_name("trait") {
                print_type(inner, text, binders, Position::Type, out);
            }
        },
        "abstract_type" => {
            out.push_str("impl ");
            if let Some(inner) = node.child_by_field_name("trait") {
                print_type(inner, text, binders, Position::Type, out);
            }
        },
        // `for<'a> Fn(&'a u8)` binds only lifetimes, which coherence erases — so the binder goes
        // and the type it qualified is printed alone.
        "higher_ranked_trait_bound" =>
            if let Some(inner) = node.child_by_field_name("type") {
                print_type(inner, text, binders, position, out);
            },
        "bracketed_type" => {
            out.push('<');
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                print_type(child, text, binders, Position::Type, out);
            }
            out.push('>');
        },
        "qualified_type" => {
            if let Some(inner) = node.child_by_field_name("type") {
                print_type(inner, text, binders, Position::Type, out);
            }
            out.push_str(" as ");
            if let Some(alias) = node.child_by_field_name("alias") {
                print_type(alias, text, binders, Position::Type, out);
            }
        },
        // A macro's arguments are TOKENS the macro matches on, so nothing here is spelling: `r#Foo`
        // and `Foo` are arms a `macro_rules!` can tell apart, and a binder name is whatever the
        // macro does with it rather than a reference to the parameter.
        "macro_invocation" => {
            if let Some(name) = node.child_by_field_name("macro") {
                print_type(name, text, binders, Position::Name, out);
            }
            out.push('!');
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "token_tree" {
                    render_opaque(child, text, binders, false, out);
                }
            }
        },
        // A const-generic block holds an EXPRESSION whose text is part of the type. A binder
        // referenced inside it is still a binder — `F<{ N }>` and `F<{ M }>` are one impl under
        // renaming — which `collect_binder_spans` decides, including the `let` that shadows one.
        //
        // The expression's own spelling is NOT canonicalized, so `F<{1 + 2}>` and `F<{ 1+2 }>` are
        // both `F<3>` and conflict, yet get separate identities. That is a split, which costs a
        // shared handle; the alternative costs correctness. Whitespace here is not uniformly
        // insignificant — collapsing it would turn `x as u8` into `xasu8` — so canonicalizing an
        // arbitrary const expression means tokenizing it and re-emitting minimal separation, not
        // rewriting its bytes. Worth doing, but as its own change (#1084).
        // A function pointer's parameter NAME is not part of its type: `fn(x: u8)` and `fn(y: u8)`
        // are both `fn(u8)`, and rustc rejects impls for the two as conflicting. Printing the whole
        // node kept the pattern text, splitting one entity — and sent the parameter's TYPE through
        // the opaque path, where a binder inside it was never substituted either.
        "parameter" =>
            if let Some(declared) = node.child_by_field_name("type") {
                print_type(declared, text, binders, Position::Type, out);
            },
        "block" => render_opaque(node, text, binders, true, out),
        _ => render_opaque(node, text, binders, false, out),
    }
}

/// Print `node`'s named children joined by `separator`, dropping the ones that print to nothing
/// (a lifetime). Returns how many were printed. `bracketed` wraps a non-empty list in `<…>`.
fn print_joined(
    node: Node<'_>,
    text: &str,
    binders: &[String],
    separator: &str,
    out: &mut String,
    bracketed: bool,
) {
    let mut cursor = node.walk();
    let mut parts = Vec::new();
    for child in node.named_children(&mut cursor) {
        let mut part = String::new();
        print_type(child, text, binders, Position::Type, &mut part);
        if !part.is_empty() {
            parts.push(part);
        }
    }
    if parts.is_empty() {
        return;
    }
    if bracketed {
        out.push('<');
    }
    out.push_str(&parts.join(separator));
    if bracketed {
        out.push('>');
    }
}

fn node_src<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    text.get(node.byte_range()).unwrap_or_default()
}

/// Whether `node` has a direct child of this kind, named or not. Mutability arrives as a NAMED
/// `mutable_specifier` while a tuple's trailing comma is an anonymous token, and reading only one
/// of the two rendered `&mut W` as `&W` and `*mut W` as `*const W` — a merge of impls that coexist.
fn has_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

/// Copy a region whose text IS the type — a const-generic block or a macro's token list.
///
/// Only what cannot carry meaning is normalized: a comment goes, and a whitespace RUN outside a
/// literal collapses to one space. A literal is copied byte for byte, because `F<{ "}".len() }>`
/// and `F<{ "} ".len() }>` are `F<1>` and `F<2>`. This normalizes SPELLING, not value: `F<{1+2}>`
/// and `F<{ 1 + 2 }>` stay two owners, since closing that would mean deleting the space BETWEEN
/// tokens and could fuse two different expressions — trading a split for a merge.
fn render_opaque(
    node: Node<'_>,
    text: &str,
    binders: &[String],
    substitute: bool,
    out: &mut String,
) {
    let start = node.start_byte();
    let source = node_src(node, text);
    let spans = if substitute {
        collect_binder_spans(node, start, source.len(), text, binders)
    } else {
        Vec::new()
    };
    let mut at_span = 0usize;
    scope_grammar::scan(source, |at, ch, _, span_kind| {
        if span_kind == scope_grammar::Span::Comment {
            return;
        }
        while at_span < spans.len() && spans[at_span].1 <= at {
            at_span += 1;
        }
        if let Some(&(from, to)) = spans.get(at_span)
            && at >= from
            && at < to
        {
            if at == from {
                out.push('_');
            }
            return;
        }
        if span_kind.is_code() && ch.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    });
}

/// Spans of bare identifiers that name one of `binders`, to be replaced by `_`.
fn collect_binder_spans(
    node: Node<'_>,
    start: usize,
    len: usize,
    text: &str,
    binders: &[String],
) -> Vec<(usize, usize)> {
    if binders.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        // A macro's arguments are TOKENS, not type references. Tree-sitter reports them as ordinary
        // identifiers, but what they mean is whatever the macro does with them — `M!(T)` can match
        // a rule keyed on the literal token `T` and expand to something `M!(U)` does not. Renaming
        // the binder is then not a no-op, so folding both to `M!(_)` would MERGE two owners that
        // compile together. Nothing here models expansion, so the tokens are left alone.
        if current.kind() == "macro_invocation" {
            continue;
        }
        // A `let` inside a const-generic block shadows the impl binder — but only from its own
        // declaration onward, so this is decided per OCCURRENCE rather than per block: in
        // `{ let before = N; let N = 1; before }` the first `N` is still the parameter.
        if matches!(current.kind(), "type_identifier" | "identifier")
            && let Some(name) = text.get(current.byte_range())
            && binders.iter().any(|binder| binder == name)
            && !shadowed_before(current, name, text)
        {
            spans.push((
                current.start_byte().saturating_sub(start).min(len),
                current.end_byte().saturating_sub(start).min(len),
            ));
            continue;
        }
        // Descend everything EXCEPT a `name` field. That field is what a node CALLS something in
        // another namespace, never a reference to this impl's parameter, and substituting there
        // would MERGE distinct owners. It is one rule because the shapes it covers are open-ended:
        // `module::T`'s tail (`Pair<T, module::T>` and `Pair<V, module::V>` compile together), and
        // an associated-type binding's name (`Parts<T = u8>` names the TRAIT's member, so renaming
        // the impl binder must leave it alone — rustc rejects the two spellings as conflicting).
        // Enumerating the node kinds instead meant meeting each new shape as its own bug.
        //
        // Every other field is a real type position, including a qualified path's ROOT: in
        // `T::Assoc` the `T` IS the binder, and rustc renders that conflict `<_ as Bound>::Assoc`.
        let named = current.child_by_field_name("name");
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor).filter(|child| Some(*child) != named));
    }
    spans.sort_unstable();
    spans
}

/// Whether an enclosing block binds `name` with a `let` written BEFORE this occurrence, which is
/// what makes the occurrence the local's rather than the impl's parameter.
fn shadowed_before(occurrence: Node<'_>, name: &str, text: &str) -> bool {
    let at = occurrence.start_byte();
    let mut current = occurrence.parent();
    while let Some(scope) = current {
        if scope.kind() == "block" {
            let mut cursor = scope.walk();
            let shadows = scope.named_children(&mut cursor).any(|child| {
                child.kind() == "let_declaration"
                    && child.start_byte() < at
                    && child.child_by_field_name("pattern").is_some_and(|pattern| {
                        pattern_binds_any(pattern, text, &[name.to_string()])
                    })
            });
            if shadows {
                return true;
            }
        }
        current = scope.parent();
    }
    false
}

/// Whether a `let` pattern binds any of `binders`. What a pattern binds are its own IDENTIFIERS,
/// so a destructuring form (`let (N,) = …`) shadows exactly as a plain `let N` does — comparing the
/// pattern's whole source text instead recognized only the simplest shape.
fn pattern_binds_any(pattern: Node<'_>, text: &str, binders: &[String]) -> bool {
    let mut stack = vec![pattern];
    while let Some(current) = stack.pop() {
        if matches!(current.kind(), "identifier" | "shorthand_field_identifier")
            && let Some(name) = text.get(current.byte_range())
            && binders.iter().any(|binder| binder == name)
        {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    false
}

/// The node naming the type an `impl` block is FOR — never the trait it implements.
///
/// `impl Trait for Type` puts the trait in the `trait` field and the owner in the `type` field.
/// When the owner is not a plain nominal type (`impl Display for &Foo`, `for (A, B)`, `for [T]`,
/// `for dyn X`) there is no name to take, and a positional scan over the children would hand
/// back the TRAIT instead — collapsing `impl Display for &Foo` and `impl Display for &Bar` onto
/// one owner, the exact leak trait-qualified scopes exist to prevent. Reference and pointer
/// wrappers are unwrapped (`&mut Foo` is still owned by `Foo`); anything else declines.
fn impl_name(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(target_type) = node.child_by_field_name("type") {
        return unwrap_impl_type(target_type);
    }
    // No `type` field at all (a partial parse): scan positionally, but never adopt the trait.
    let trait_id = node.child_by_field_name("trait").map(|node| node.id());
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| is_impl_type_node(child.kind()) && Some(child.id()) != trait_id)
}

/// Peel `&`/`&mut`/`*const`/`*mut` off an impl target, then keep it only if it names a type.
fn unwrap_impl_type(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node;
    while matches!(current.kind(), "reference_type" | "pointer_type") {
        current = current.child_by_field_name("type")?;
    }
    is_impl_type_node(current.kind()).then_some(current)
}

fn is_impl_type_node(kind: &str) -> bool {
    matches!(kind, "type_identifier" | "generic_type" | "scoped_type_identifier")
}

fn attribute_is_test(attribute: &str) -> bool {
    let inner = attribute
        .trim()
        .trim_start_matches('#')
        .trim_start_matches('!')
        .trim_start_matches('[')
        .trim_end_matches(']');
    let head = inner.split(['(', '[']).next().unwrap_or_default().trim();
    let last = head.rsplit("::").next().unwrap_or(head).trim();
    last == "test" || last == "rstest" || last.starts_with("test_case")
}

fn in_cfg_test_module(node: Node<'_>, text: &str) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "mod_item"
            && attribute_items(text, ancestor)
                .iter()
                .any(|attribute| attribute.contains("cfg") && attribute.contains("test"))
        {
            return true;
        }
        current = ancestor.parent();
    }
    false
}

fn attribute_items(text: &str, node: Node<'_>) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "attribute_item" {
            attributes.push(parser::node_text(child, text).unwrap_or_default());
        }
    }

    let mut preceding = Vec::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        if previous.kind() != "attribute_item" {
            break;
        }
        preceding.push(parser::node_text(previous, text).unwrap_or_default());
        sibling = previous.prev_named_sibling();
    }
    preceding.reverse();
    preceding.extend(attributes);
    preceding
}

pub(super) const RESOLVER_POLICY: ResolutionPolicy = ResolutionPolicy {
    reference_disposition,
    type_binding: TypeBinding::DefinitionsOnly,
    receiver_fallback: ReceiverFallback::Type,
    qualified_root,
    ..ResolutionPolicy::DEFAULT
};

fn reference_disposition(edge_kind: EdgeKind, name: &str) -> ReferenceDisposition {
    if edge_kind == EdgeKind::ReferencesType && type_ref_is_unresolvable(name) {
        ReferenceDisposition::Unresolvable
    } else {
        ReferenceDisposition::Resolve
    }
}

fn qualified_root(root: &str) -> QualifiedRoot {
    if matches!(root, "crate" | "self" | "super") {
        QualifiedRoot::Local
    } else if is_external_root(root) {
        QualifiedRoot::External
    } else {
        QualifiedRoot::Neutral
    }
}

fn type_ref_is_unresolvable(name: &str) -> bool {
    match name.split_once("::") {
        Some((root, _)) => root == "Self" || looks_like_type_parameter(root),
        None => looks_like_type_parameter(name),
    }
}

fn looks_like_type_parameter(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_uppercase())
        && chars.all(|rest| rest.is_ascii_digit())
}

fn is_external_root(value: &str) -> bool {
    matches!(
        value,
        "std"
            | "core"
            | "alloc"
            | "tokio"
            | "serde"
            | "serde_json"
            | "anyhow"
            | "thiserror"
            | "rusqlite"
            | "tree_sitter"
            | "tracing"
            | "log"
            | "Vec"
            | "String"
            | "Option"
            | "Result"
            | "HashMap"
            | "BTreeMap"
            | "HashSet"
            | "BTreeSet"
    )
}

#[cfg(test)]
mod printer_grammar_tests {
    /// Every node kind the type printer dispatches on must EXIST in the grammar it reads.
    ///
    /// The printer matches on kind strings with a catch-all that falls back to opaque source
    /// rendering. That catch-all is load-bearing for genuinely unmodelled nodes, but it also means
    /// a typo — or a tree-sitter-rust upgrade that RENAMES a kind — silently routes a type that
    /// used to be printed structurally into the opaque path instead. Nothing would fail: the
    /// printer still returns a string, so identity quietly changes for every type of that shape
    /// and the ids churn. Reading the arms out of the source and asking the compiled grammar
    /// whether each name is real turns that silent drift into a failing test.
    ///
    /// The arms are read with the parser rather than by scanning for quotes, so field-name
    /// arguments and punctuation the printer emits are not mistaken for node kinds.
    ///
    /// This checks the names are SPELLED for real nodes, not that the handling of each is correct —
    /// the binder-renaming rows and the reformat-invariance test cover behavior.
    #[test]
    fn every_kind_the_type_printer_names_exists_in_the_grammar() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let source = include_str!("mod.rs");
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).expect("the rust grammar loads");
        let tree = parser.parse(source, None).expect("this file parses");

        // The printer's own match, found by walking to the function and taking its match arms.
        let mut named = Vec::new();
        let mut cursor = tree.root_node().walk();
        let mut stack = vec![tree.root_node()];
        let mut inside_printer = false;
        while let Some(node) = stack.pop() {
            if node.kind() == "function_item"
                && node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                    == Some("print_type_inner")
            {
                inside_printer = true;
                collect_arm_kinds(node, source, &mut named);
            }
            stack.extend(node.children(&mut cursor));
        }
        assert!(inside_printer, "the printer is in this file");
        assert!(named.len() > 15, "expected the printer's arm list, found {named:?}");

        let kinds: std::collections::HashSet<&str> = (0..language.node_kind_count())
            .filter_map(|id| {
                let id = u16::try_from(id).expect("kind ids fit a u16");
                language.node_kind_is_named(id).then(|| language.node_kind_for_id(id)).flatten()
            })
            .collect();
        let unknown: Vec<&String> = named.iter().filter(|k| !kinds.contains(k.as_str())).collect();
        assert!(
            unknown.is_empty(),
            "the type printer names node kinds the grammar does not have: {unknown:?}. Either the \
             name is a typo or tree-sitter-rust renamed it; either way those types now render \
             opaquely and their identities have changed."
        );
    }

    /// The string literals in every `match_arm` PATTERN under `node` — the kinds it dispatches on,
    /// without the literals its arm bodies emit.
    fn collect_arm_kinds(node: tree_sitter::Node<'_>, source: &str, out: &mut Vec<String>) {
        let mut cursor = node.walk();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if current.kind() == "match_arm"
                && let Some(pattern) = current.child_by_field_name("pattern")
            {
                let mut inner = pattern.walk();
                let mut patterns = vec![pattern];
                while let Some(part) = patterns.pop() {
                    if part.kind() == "string_content"
                        && let Ok(text) = part.utf8_text(source.as_bytes())
                    {
                        out.push(text.to_string());
                    }
                    patterns.extend(part.children(&mut inner));
                }
            }
            stack.extend(current.children(&mut cursor));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rag_rat_base::language::Language;

    use super::*;

    #[test]
    fn a_trait_ref_keeps_its_concrete_arguments() {
        let parsed = parser::parse_file(
            Path::new("src/lib.rs"),
            Language::Rust,
            "impl crate::traits::Runs<Item> for Worker { fn run(&self) {} }",
        )
        .expect("Rust parses");
        let run = parsed.symbols.iter().find(|symbol| symbol.name == "run").expect("method symbol");
        assert_eq!(run.scope_path, "Worker as crate.traits.Runs<Item>::run");
    }

    fn scope_paths(source: &str, method: &str) -> Vec<String> {
        parser::parse_file(Path::new("src/lib.rs"), Language::Rust, source)
            .expect("Rust parses")
            .symbols
            .iter()
            .filter(|symbol| symbol.name == method)
            .map(|symbol| symbol.scope_path.clone())
            .collect()
    }

    /// `impl Trait for &Type` puts a `reference_type` in the impl's `type` field. Taking the first
    /// nominal child instead would hand back the TRAIT, giving every `impl Display for &_` in a
    /// file the same owner — the collapse trait-qualified scopes exist to prevent.
    #[test]
    fn a_reference_impl_target_keeps_its_own_owner() {
        let scopes = scope_paths(
            "struct Alpha;\nstruct Beta;\nimpl std::fmt::Display for &Alpha { fn fmt(&self) {} \
             }\nimpl std::fmt::Display for &Beta { fn fmt(&self) {} }\n",
            "fmt",
        );
        assert_eq!(scopes, vec!["&Alpha as std.fmt.Display::fmt", "&Beta as std.fmt.Display::fmt"]);
    }

    /// `impl Tr for W`, `for &W`, `for &mut W`, and `for *const W` all compile together, so they
    /// are four entities and their identically-signatured members must not share one logical
    /// symbol. The idiomatic operator quartet (`impl Neg for T` / `for &T`, whose `type Output` is
    /// byte-identical across all of them) is the shape that hits this in real code.
    #[test]
    fn every_pointer_shape_of_one_owner_is_its_own_impl() {
        let scopes = scope_paths(
            "struct W;\nimpl Neg for W { fn neg(&self) {} }\nimpl Neg for &W { fn neg(&self) {} \
             }\nimpl Neg for &mut W { fn neg(&self) {} }\nimpl Neg for *const W { fn neg(&self) \
             {} }\n",
            "neg",
        );
        assert_eq!(scopes, vec![
            "W as Neg::neg",
            "&W as Neg::neg",
            "&mut W as Neg::neg",
            "*const W as Neg::neg",
        ]);
        // …but all four expose the SAME receiver surface: an autoref call site cannot tell which
        // impl it lands in, so resolution folds the wrapper away and a call that could hit either
        // declines as ambiguous.
        for scope in &scopes {
            assert_eq!(
                crate::index::edges::normalized_scope_path(scope, Some(Language::Rust.as_str()))
                    .as_ref(),
                "W::neg",
                "{scope} must still answer to the plain receiver surface"
            );
        }
    }

    /// Erasing a lifetime can leave syntax that held nothing else. rustc treats
    /// `impl Tr for for<'a> fn(&'a T)` and `impl Tr for fn(&T)` as conflicting implementations —
    /// one entity — so the residue has to go with the lifetime or the two owners split.
    #[test]
    fn lifetime_only_syntax_leaves_no_residue() {
        for (with_lifetime, without) in [
            (
                "impl Tr for for<'a> fn(&'a T) { fn f(&self) {} }",
                "impl Tr for fn(&T) { fn f(&self) {} }",
            ),
            (
                "impl Tr for dyn Runs + 'a { fn f(&self) {} }",
                "impl Tr for dyn Runs { fn f(&self) {} }",
            ),
            (
                "impl Tr for Box<dyn Runs + 'static> { fn f(&self) {} }",
                "impl Tr for Box<dyn Runs> { fn f(&self) {} }",
            ),
        ] {
            assert_eq!(
                scope_paths(with_lifetime, "f"),
                scope_paths(without, "f"),
                "{with_lifetime} and {without} are one impl to rustc"
            );
        }
    }

    /// The coherence table: two impls are ONE entity iff a single compilation could not hold
    /// both, so canonical scopes must be equal exactly when rustc reports a conflict. Each row
    /// here was probed against rustc directly; `coexists` is what it answered.
    #[test]
    fn canonical_scopes_match_rustc_coherence() {
        // (impl A, impl B, coexists) — coexisting impls are distinct entities and must NOT share
        // a scope; conflicting impls are one entity and MUST. Every row was checked against rustc;
        // do not add a row that has not been.
        let cases = [
            ("impl Tr for F<u8> { fn f(&self) {} }", "impl Tr for F<u16> { fn f(&self) {} }", true),
            (
                "impl<T> Tr for F<T> { fn f(&self) {} }",
                "impl<U> Tr for F<U> { fn f(&self) {} }",
                false,
            ),
            ("impl Tr for W { fn f(&self) {} }", "impl Tr for &W { fn f(&self) {} }", true),
            ("impl Tr for &W { fn f(&self) {} }", "impl Tr for &mut W { fn f(&self) {} }", true),
            ("impl Tr for W { fn f(&self) {} }", "impl Tr for *const W { fn f(&self) {} }", true),
            (
                "impl<'a> Tr for &'a W { fn f(&self) {} }",
                "impl Tr for &'static W { fn f(&self) {} }",
                false,
            ),
            (
                "impl TryFrom<&str> for C { fn f(&self) {} }",
                "impl TryFrom<String> for C { fn f(&self) {} }",
                true,
            ),
            (
                "impl a::Runs for W { fn f(&self) {} }",
                "impl b::Runs for W { fn f(&self) {} }",
                true,
            ),
            ("impl Tr for (W) { fn f(&self) {} }", "impl Tr for W { fn f(&self) {} }", false),
            ("impl Tr for (W,) { fn f(&self) {} }", "impl Tr for W { fn f(&self) {} }", true),
            ("impl Tr for r#Foo { fn f(&self) {} }", "impl Tr for Foo { fn f(&self) {} }", false),
            // A binder at a qualified path's ROOT is the binder: rustc reports these as one impl,
            // rendering the conflict `Tr<<_ as Bound>::Assoc> for Foo<_>`.
            (
                "impl<T: Bound> Tr<T::Assoc> for F<T> { fn f(&self) {} }",
                "impl<U: Bound> Tr<U::Assoc> for F<U> { fn f(&self) {} }",
                false,
            ),
            // A brace block is an expression whose spelling is the type: these are `F<1>`/`F<2>`.
            (
                r#"impl Tr for F<{ "}".len() }> { fn f(&self) {} }"#,
                r#"impl Tr for F<{ "} ".len() }> { fn f(&self) {} }"#,
                true,
            ),
            // A module's const and a value's field can share a name, so these are two impls.
            (
                "impl Tr<{ a::c }> for W { fn f(&self) {} }",
                "impl Tr<{ a.c }> for W { fn f(&self) {} }",
                true,
            ),
            // Whitespace around the angle is spelling, on either side of it.
            (
                "impl Tr for F<u8> { fn f(&self) {} }",
                "impl Tr for F < u8 > { fn f(&self) {} }",
                false,
            ),
            // An associated-type binding names the TRAIT's member, not this impl's binder.
            (
                "impl<T> Tr for Pair<T, dyn Parts<T = u8>> { fn f(&self) {} }",
                "impl<U> Tr for Pair<U, dyn Parts<T = u8>> { fn f(&self) {} }",
                false,
            ),
        ];
        for (left, right, coexists) in cases {
            let a = scope_paths(left, "f");
            let b = scope_paths(right, "f");
            if coexists {
                assert_ne!(
                    a, b,
                    "rustc allows both, so they are distinct entities: {left} / {right}"
                );
            } else {
                assert_eq!(
                    a, b,
                    "rustc rejects both together, so they are one entity: {left} / {right}"
                );
            }
        }
    }

    /// Binder substitution is the only step that can MERGE two owners, so a qualified path's TAIL
    /// is off limits: it names an item in another namespace, `Pair<T, module::T>` and
    /// `Pair<V, module::V>` coexist, and folding the tails would give them one scope.
    #[test]
    fn a_binder_name_under_a_qualified_path_is_not_substituted() {
        let left = scope_paths("impl<T> Tr for Pair<T, module::T> { fn f(&self) {} }", "f");
        let right = scope_paths("impl<V> Tr for Pair<V, module::V> { fn f(&self) {} }", "f");
        assert_ne!(left, right, "these impls coexist, so they must not share a scope");
        assert_eq!(left, vec!["Pair<_,module::T> as Tr::f".to_string()]);
        // The whole owner being qualified is likewise a concrete type, not the binder.
        assert_eq!(scope_paths("impl<T> Tr<T> for crate::T { fn f(&self) {} }", "f"), vec![
            "crate::T as Tr<_>::f".to_string()
        ]);
    }

    /// A `let` inside a const block shadows the impl binder, so those identifiers belong to the
    /// local. rustc rejects the two spellings as one impl — renaming the parameter cannot change a
    /// name the block rebinds — so substituting by spelling alone would split them.
    #[test]
    fn a_block_that_rebinds_a_binder_keeps_its_own_local() {
        let shadowed = scope_paths(
            "impl<const N: usize> Tr<N> for F<{ { let N = 1; N } }> { fn f(&self) {} }",
            "f",
        );
        assert_eq!(
            shadowed,
            scope_paths(
                "impl<const M: usize> Tr<M> for F<{ { let N = 1; N } }> { fn f(&self) {} }",
                "f",
            ),
        );
        assert!(shadowed[0].contains("let N = 1"), "the local keeps its name: {}", shadowed[0]);
        // A `let` shadows only from its own line onward, so an occurrence BEFORE it is still the
        // parameter and still canonicalizes.
        assert_eq!(
            scope_paths(
                "impl<const N: usize> Tr<N> for F<{ { let before = N; let N = 1; before } }> { fn \
                 f(&self) {} }",
                "f",
            ),
            scope_paths(
                "impl<const M: usize> Tr<M> for F<{ { let before = M; let N = 1; before } }> { fn \
                 f(&self) {} }",
                "f",
            ),
        );
        // A destructuring pattern binds through its own identifiers, so it shadows the same way.
        assert_eq!(
            scope_paths(
                "impl<const N: usize> Tr<N> for F<{ { let (N,) = (1,); N } }> { fn f(&self) {} }",
                "f",
            ),
            scope_paths(
                "impl<const M: usize> Tr<M> for F<{ { let (N,) = (1,); N } }> { fn f(&self) {} }",
                "f",
            ),
        );
        // A block that does NOT rebind still canonicalizes the binder.
        assert_eq!(
            scope_paths("impl<const N: usize> Tr for F<{ N }> { fn f(&self) {} }", "f"),
            scope_paths("impl<const M: usize> Tr for F<{ M }> { fn f(&self) {} }", "f"),
        );
    }

    /// THE PROPERTY THE PRINTER EXISTS FOR: spelling cannot reach identity.
    ///
    /// Every byte of a rendered owner is emitted from a node rather than copied from source, so
    /// reformatting an impl header — inserting whitespace anywhere punctuation allows it, adding a
    /// comment, writing an identifier raw — cannot move its scope. This one assertion covers the
    /// whole class that used to be found an instance at a time: each new spelling here is a review
    /// round that does not need to happen.
    /// A `::` is rewritten to `.` only where the resolution fold would split the scope — at the
    /// TOP level of the trait path. Nested anywhere, it is part of an expression or an argument,
    /// and `a::c` (a module's const) names a different value than `a.c` (a field), so rewriting one
    /// into the other hands two coexisting impls one identity.
    #[test]
    fn a_separator_is_rewritten_only_where_the_fold_would_split() {
        for (with_path, with_field) in [
            // A bracket raises group depth, not brace depth — the distinction a hand-written
            // `!in_brace` guard missed.
            (
                "impl Tr<[u8; a::c]> for W { fn f(&self) {} }",
                "impl Tr<[u8; a.c]> for W { fn f(&self) {} }",
            ),
            (
                "impl Tr<{ a::c }> for W { fn f(&self) {} }",
                "impl Tr<{ a.c }> for W { fn f(&self) {} }",
            ),
            (
                "impl Tr<[u8; (a::c)]> for W { fn f(&self) {} }",
                "impl Tr<[u8; (a.c)]> for W { fn f(&self) {} }",
            ),
        ] {
            assert_ne!(
                scope_paths(with_path, "f"),
                scope_paths(with_field, "f"),
                "a nested `::` is not a path separator: {with_path}"
            );
        }
        // The trait path's own separators still fold, so the marker stays one `::`-segment.
        let marked = scope_paths("impl a::Runs for W { fn f(&self) {} }", "f");
        assert_eq!(marked, vec!["W as a.Runs::f".to_string()]);
        // And two traits that differ only in their path stay distinct.
        assert_ne!(marked, scope_paths("impl b::Runs for W { fn f(&self) {} }", "f"));
    }

    /// A const parameter named after a PRELUDE type is shadowed by that type in argument position,
    /// exactly as a locally declared or imported one is. Verified with rustc: `impl<const Vec:
    /// usize> Tr<{ Vec }> for F<Vec<u8>>` and its `Box`-named twin COMPILE TOGETHER, so the owners
    /// are the prelude types and the two impls coexist.
    #[test]
    fn a_const_binder_named_after_a_prelude_type_is_not_substituted() {
        let vec_named =
            "struct F<T>(T); impl<const Vec: usize> Tr<{ Vec }> for F<Vec<u8>> { fn f(&self) {} }";
        let box_named =
            "struct F<T>(T); impl<const Box: usize> Tr<{ Box }> for F<Box<u8>> { fn f(&self) {} }";
        assert_ne!(scope_paths(vec_named, "f"), scope_paths(box_named, "f"));
    }

    /// rustc normalizes identifiers to NFC, so a type spelled with a composed `é` and one spelled
    /// with a combining accent are ONE type — rustc rejects impls for the two spellings as
    /// conflicting. Rendering the source bytes unchanged split one impl's members across handles.
    #[test]
    fn an_identifier_is_normalized_to_nfc() {
        let composed = "impl Tr for Caf\u{00e9} { fn f(&self) {} }";
        let decomposed = "impl Tr for Cafe\u{0301} { fn f(&self) {} }";
        assert_eq!(scope_paths(composed, "f"), scope_paths(decomposed, "f"));
        assert!(!scope_paths(composed, "f").is_empty(), "the fixture must parse");
    }

    /// A const binder is left alone whenever a TYPE of that name could be in scope — including one
    /// that arrives by `use`, and including a glob that might carry it. Verified with rustc: with
    /// `use p::{N, M};` the two impls below COMPILE TOGETHER, so they are two entities.
    #[test]
    fn an_imported_type_shadows_a_const_binder_too() {
        let imported = |binder: &str| {
            format!(
                "mod p {{ pub struct N; pub struct M; }} use p::{{N, M}}; struct F<T>(T); \
                 impl<const {binder}: usize> Tr<{{ {binder} }}> for F<{binder}> {{ fn f(&self) \
                 {{}} }}"
            )
        };
        assert_ne!(scope_paths(&imported("N"), "f"), scope_paths(&imported("M"), "f"));

        // A glob could carry the type, and this pass cannot see through it — so it declines to
        // substitute rather than risk merging two impls that coexist.
        let globbed = |binder: &str| {
            format!(
                "use p::*; struct F<T>(T); impl<const {binder}: usize> Tr<{{ {binder} }}> for \
                 F<{binder}> {{ fn f(&self) {{}} }}"
            )
        };
        assert_ne!(scope_paths(&globbed("N"), "f"), scope_paths(&globbed("M"), "f"));

        // A `use` that binds some OTHER name is not evidence about this one.
        let unrelated = |binder: &str| {
            format!(
                "use p::Other; struct G<const C: usize>; impl<const {binder}: usize> Tr for \
                 G<{binder}> {{ fn f(&self) {{}} }}"
            )
        };
        assert_eq!(scope_paths(&unrelated("N"), "f"), scope_paths(&unrelated("M"), "f"));
    }

    /// `r#T` and `T` are one parameter. The occurrence side strips the raw prefix before matching,
    /// so a declaration that kept it never matched its own uses: `impl<r#T> Tr for F<r#T>` rendered
    /// `F<T>` beside the `U` form's `F<_>`, splitting an impl rustc rejects as conflicting.
    #[test]
    fn a_raw_spelled_binder_is_still_a_binder() {
        assert_eq!(
            scope_paths("impl<r#T> Tr for F<r#T> { fn f(&self) {} }", "f"),
            scope_paths("impl<U> Tr for F<U> { fn f(&self) {} }", "f"),
        );
        assert_eq!(scope_paths("impl<r#T> Tr for F<r#T> { fn f(&self) {} }", "f"), vec![
            "F<_> as Tr::f".to_string()
        ]);
    }

    /// A CONST parameter and a TYPE of the same name are different things, and rustc picks the
    /// type for a bare identifier in argument position. Verified with rustc: `struct N;` beside
    /// `impl<const N: usize> Tr<{ N }> for F<N>` and its `M`-spelled twin COMPILE TOGETHER — the
    /// owners are the two structs, so they are two entities. Substituting the name in both places
    /// rendered them identically and merged them.
    #[test]
    fn a_const_binder_a_type_shadows_is_not_substituted() {
        let with_n =
            "struct N; struct F<T>(T); impl<const N: usize> Tr<{ N }> for F<N> { fn f(&self) {} }";
        let with_m =
            "struct M; struct F<T>(T); impl<const M: usize> Tr<{ M }> for F<M> { fn f(&self) {} }";
        assert_ne!(scope_paths(with_n, "f"), scope_paths(with_m, "f"));
        assert_eq!(scope_paths(with_n, "f"), vec!["F<N> as Tr<{ N }>::f".to_string()]);
        // With no such type in scope the bare name IS the parameter, and renaming it is a no-op —
        // rustc rejects these two together, so they stay one entity.
        assert_eq!(
            scope_paths(
                "struct G<const C: usize>; impl<const N: usize> Tr for G<N> { fn f(&self) {} }",
                "f"
            ),
            scope_paths(
                "struct G<const C: usize>; impl<const M: usize> Tr for G<M> { fn f(&self) {} }",
                "f"
            ),
        );
    }

    /// A function pointer's parameter NAME is not part of its type: `fn(x: u8)` and `fn(y: u8)` are
    /// both `fn(u8)`, so impls for them conflict and are one entity. Printing the parameter node
    /// whole kept the pattern text AND sent the parameter's type down the opaque path, where a
    /// binder inside it was never canonicalized either.
    #[test]
    fn a_function_pointer_parameter_is_named_by_its_type_alone() {
        assert_eq!(
            scope_paths("impl Tr for fn(x: u8) { fn f(&self) {} }", "f"),
            scope_paths("impl Tr for fn(y: u8) { fn f(&self) {} }", "f"),
        );
        assert_eq!(scope_paths("impl Tr for fn(x: u8) { fn f(&self) {} }", "f"), vec![
            "fn(u8) as Tr::f".to_string()
        ]);
        assert_eq!(
            scope_paths("impl<T> Tr for fn(x: T) { fn f(&self) {} }", "f"),
            scope_paths("impl<U> Tr for fn(x: U) { fn f(&self) {} }", "f"),
        );
    }

    /// The impl BLOCK is canonicalized too, not only its members. `name` and `qualified_name` feed
    /// `LogicalSymbolKey` beside the scope, so leaving the impl symbol named `Foo<T>` gave the
    /// binder-renamed twin a different handle even once their methods shared one.
    #[test]
    fn an_impl_symbol_is_named_canonically() {
        let named = |source: &str| {
            parser::parse_file(Path::new("src/lib.rs"), Language::Rust, source)
                .expect("Rust parses")
                .symbols
                .iter()
                .filter(|symbol| symbol.kind == "impl")
                .map(|symbol| (symbol.name.clone(), symbol.qualified_name.clone()))
                .collect::<Vec<_>>()
        };
        let with_t = named("impl<T> Tr for Foo<T> { fn f(&self) {} }");
        assert_eq!(with_t, vec![("Foo<_>".to_string(), "src/lib.rs::Foo<_>".to_string())]);
        assert_eq!(with_t, named("impl<U> Tr for Foo<U> { fn f(&self) {} }"));
        // A non-generic impl is unchanged.
        assert_eq!(named("impl Tr for Foo { fn f(&self) {} }"), vec![(
            "Foo".to_string(),
            "src/lib.rs::Foo".to_string()
        )]);
    }

    #[test]
    fn every_type_form_renames_its_binder() {
        // One row per form a Rust type can take, so a form whose binder is left un-substituted is
        // caught here rather than by whichever repository happens to use it. Renaming the binder
        // cannot change the owner: rustc says the two impls conflict, so they are one entity and
        // must render to one string.
        //
        // Two forms are deliberately absent. A macro type is covered by its own test below, which
        // asserts the OPPOSITE — its tokens are not a type until expansion, so substituting inside
        // them would claim an equivalence rustc has not granted. And `builtin # pattern_type(..)`
        // is unstable, has no tree-sitter node, and cannot appear in a compiling repository.
        for (with_t, with_u) in [
            ("Foo<T>", "Foo<U>"),
            ("[T; 4]", "[U; 4]"),
            ("[T]", "[U]"),
            ("(T)", "(U)"),
            ("(T, T)", "(U, U)"),
            ("&T", "&U"),
            ("*const T", "*const U"),
            ("dyn Tr<T>", "dyn Tr<U>"),
            // A trait object may be written without `dyn`, which parses as a bounded type instead.
            ("Tr<T> + Send", "Tr<U> + Send"),
            ("impl Tr<T>", "impl Tr<U>"),
            ("fn(T) -> T", "fn(U) -> U"),
            ("for<'a> fn(&'a T)", "for<'a> fn(&'a U)"),
            ("<T as Tr>::A", "<U as Tr>::A"),
        ] {
            let render = |binder: &str, form: &str| {
                let source = format!("impl<{binder}> Q for {form} {{ fn f(&self) {{}} }}");
                let paths = scope_paths(&source, "f");
                assert!(!paths.is_empty(), "the fixture must parse: {source}");
                paths
            };
            assert_eq!(
                render("T", with_t),
                render("U", with_u),
                "renaming the binder changed the owner of `{with_t}`"
            );
        }
    }

    /// The forms with no binder to rename still have to render as themselves rather than fall into
    /// an opaque path that re-spells them.
    #[test]
    fn a_type_form_without_a_binder_still_renders() {
        for form in ["!", "_", "()"] {
            let source = format!("impl Q for {form} {{ fn f(&self) {{}} }}");
            let paths = scope_paths(&source, "f");
            assert!(!paths.is_empty(), "the fixture must parse: {source}");
            assert!(
                paths.iter().any(|path| path.contains(form)),
                "`{form}` should render as itself, got {paths:?}"
            );
        }
    }

    /// A macro's tokens are NOT a type — they are tokens, and what they mean is decided by an
    /// expansion this indexer does not perform. So `m!(T)` and `m!(U)` stay distinct: substituting
    /// inside them would claim an equivalence rustc has not granted.
    #[test]
    fn a_binder_inside_macro_tokens_is_not_renamed() {
        let render = |binder: &str| {
            let source = format!("impl<{binder}> Q for m!({binder}) {{ fn f(&self) {{}} }}");
            scope_paths(&source, "f")
        };
        let (with_t, with_u) = (render("T"), render("U"));
        assert!(!with_t.is_empty(), "the fixture must parse");
        assert_ne!(with_t, with_u, "macro tokens are not substituted");
    }

    #[test]
    fn spelling_cannot_change_an_owner() {
        for (canonical, spellings) in [
            ("impl Tr for Foo<u8> {}", vec![
                "impl Tr for Foo < u8 > {}",
                "impl Tr for Foo<u8 > {}",
                "impl Tr for /* c */ Foo<u8> {}",
                "impl Tr for Foo<\n    u8,\n> {}",
                "impl Tr for r#Foo<u8> {}",
            ]),
            ("impl Tr for fn(u8) -> W {}", vec![
                "impl Tr for fn ( u8 ) -> W {}",
                "impl Tr for fn(u8)->W {}",
                "impl Tr for fn(\n    u8,\n) -> W {}",
            ]),
            ("impl Tr for &mut W {}", vec![
                "impl Tr for & mut W {}",
                "impl Tr for &'a mut W {}",
                "impl Tr for &'static mut W {}",
            ]),
            ("impl Tr for *const W {}", vec!["impl Tr for * const W {}"]),
            ("impl Tr for (A, B) {}", vec!["impl Tr for ( A , B ) {}", "impl Tr for (A, B,) {}"]),
            ("impl Tr for a::b::W {}", vec![
                "impl Tr for a :: b :: W {}",
                "impl Tr for a::/* mid */b::W {}",
            ]),
            ("impl Tr for Box<dyn Runs + 'a> {}", vec![
                "impl Tr for Box<dyn Runs> {}",
                "impl Tr for Box< dyn Runs + 'static > {}",
            ]),
            ("impl Tr for [u8; 4] {}", vec!["impl Tr for [ u8 ; 4 ] {}"]),
        ] {
            let expected = scope_paths(&format!("{canonical} fn f(&self) {{}}"), "f");
            assert!(!expected.is_empty(), "the fixture must parse: {canonical}");
            for spelling in spellings {
                assert_eq!(
                    scope_paths(&format!("{spelling} fn f(&self) {{}}"), "f"),
                    expected,
                    "{spelling} must render as {canonical}"
                );
            }
        }
    }

    /// The other half of the same property: what the printer must NOT fold. A macro's arguments are
    /// tokens, so a lifetime there is one — erasing it inside the token list would merge owners a
    /// macro can expand differently.
    #[test]
    fn a_lifetime_inside_macro_tokens_is_a_token() {
        assert_ne!(
            scope_paths("impl Tr for M!('a) { fn f(&self) {} }", "f"),
            scope_paths("impl Tr for M!('static) { fn f(&self) {} }", "f"),
        );
        // …while a lifetime in a real type position is erased.
        assert_eq!(
            scope_paths("impl<'a> Tr for Foo<'a, u8> { fn f(&self) {} }", "f"),
            scope_paths("impl Tr for Foo<'static, u8> { fn f(&self) {} }", "f"),
        );
    }

    /// A macro's argument list is verbatim for the same reason a const block is: the text is
    /// TOKENS the macro matches on, so the cosmetic rewrites are not cosmetic there. `r#Foo` and
    /// `Foo` are different tokens a `macro_rules!` arm can tell apart, and the two owners coexist.
    #[test]
    fn cosmetic_rewrites_stop_at_a_macro_argument_list() {
        let raw = scope_paths("impl Tr for M!(r#Foo) { fn f(&self) {} }", "f");
        assert_eq!(raw, vec!["M!(r#Foo) as Tr::f".to_string()]);
        assert_ne!(raw, scope_paths("impl Tr for M!(Foo) { fn f(&self) {} }", "f"));
        // Outside the token list the macro's own path is ordinary type text.
        assert_eq!(scope_paths("impl Tr for a :: M!(Foo) { fn f(&self) {} }", "f"), vec![
            "a::M!(Foo) as Tr::f".to_string()
        ]);
    }

    /// An impl symbol's NAME is its self type, so two impls of different traits for one type share
    /// it. With the trait on a later line the captured signature is `impl` for both as well, so
    /// nothing but the scope path separates them — and a handle or memory bound to either impl
    /// block would answer for both.
    #[test]
    fn two_traits_impls_on_one_type_are_two_impl_symbols() {
        let scope_of = |source: &str| {
            parser::parse_file(Path::new("src/lib.rs"), Language::Rust, source)
                .expect("Rust parses")
                .symbols
                .iter()
                .find(|symbol| symbol.kind == "impl")
                .map(|symbol| symbol.scope_path.clone())
                .expect("the fixture declares an impl")
        };
        assert_eq!(scope_of("impl\n A for W { fn f(&self) {} }"), "W as A");
        assert_ne!(
            scope_of("impl\n A for W { fn f(&self) {} }"),
            scope_of("impl\n B for W { fn f(&self) {} }"),
        );
        // An inherent impl still ends its path at the type it is for.
        assert_eq!(scope_of("impl W { fn f(&self) {} }"), "W");
    }

    /// A macro's arguments are tokens the macro consumes, not references to the binder, and what
    /// they expand to is not knowable here. Substituting them claims a renaming is a no-op, which
    /// a rule keyed on the literal token makes false — so the tokens stay as written and the two
    /// owners stay apart.
    #[test]
    fn a_binder_inside_a_macro_argument_is_left_as_written() {
        let left = scope_paths("impl<T> Tr for Pair<T, M!(T)> { fn f(&self) {} }", "f");
        assert_eq!(left, vec!["Pair<_,M!(T)> as Tr::f".to_string()]);
        assert_ne!(left, scope_paths("impl<U> Tr for Pair<U, M!(U)> { fn f(&self) {} }", "f"));
    }

    /// The ROOT of a qualified path is the opposite case: `T::Assoc` is a projection off the
    /// binder, rustc rejects the two impls together, and the trait marker has to fold with the
    /// owner — a walk that skipped the whole qualified subtree left `Tr<T::Assoc>` beside
    /// `Tr<U::Assoc>` and split one entity in two.
    #[test]
    fn a_binder_at_a_qualified_paths_root_is_substituted() {
        // The projection keeps its `::`. A trait marker rewrites only its TOP-LEVEL separators —
        // this one is inside a generic argument, where a `::` is never a segment boundary.
        assert_eq!(
            scope_paths("impl<T: Bound> Tr<T::Assoc> for F<T> { fn f(&self) {} }", "f"),
            vec!["F<_> as Tr<_::Assoc>::f".to_string()]
        );
        assert_eq!(
            scope_paths("impl<T: Bound> Tr<T::Assoc> for F<T> { fn f(&self) {} }", "f"),
            scope_paths("impl<U: Bound> Tr<U::Assoc> for F<U> { fn f(&self) {} }", "f"),
        );
    }

    /// A brace inside a string literal is content, not the end of the const-generic block. Closing
    /// there sends the rest of the literal through the cosmetic tidier, whose whitespace collapse
    /// erases the difference between `F<1>` and `F<2>` — a MERGE of two distinct types.
    #[test]
    fn a_brace_inside_a_literal_does_not_end_the_const_block() {
        let one = scope_paths(r#"impl Tr for F<{ "}".len() }> { fn f(&self) {} }"#, "f");
        let two = scope_paths(r#"impl Tr for F<{ "} ".len() }> { fn f(&self) {} }"#, "f");
        assert_eq!(one, vec![r#"F<{ "}".len() }> as Tr::f"#.to_string()]);
        assert_ne!(one, two, "these are `F<1>` and `F<2>`, so they must not share a scope");
        // A raw string's unescaped quotes do not end it either.
        assert_eq!(
            scope_paths(r##"impl Tr for F<{ r#""}"#.len() }> { fn f(&self) {} }"##, "f"),
            vec![r##"F<{ r#""}"#.len() }> as Tr::f"##.to_string()]
        );
    }

    /// A const binder referenced inside a const-generic block is still a binder: the block's
    /// syntax is verbatim, but renaming `N` to `M` does not make a second impl.
    #[test]
    fn a_const_binder_inside_a_block_still_canonicalizes() {
        assert_eq!(
            scope_paths("impl<const N: usize> Tr for F<{ N }> { fn f(&self) {} }", "f"),
            scope_paths("impl<const M: usize> Tr for F<{ M }> { fn f(&self) {} }", "f"),
        );
    }

    /// A comma inside a generic argument list is not a tuple separator, so the redundant
    /// parentheses around a single type still come off.
    #[test]
    fn parens_around_one_generic_type_are_redundant() {
        assert_eq!(
            scope_paths("impl Tr for (Pair<u8, u16>) { fn f(&self) {} }", "f"),
            scope_paths("impl Tr for Pair<u8, u16> { fn f(&self) {} }", "f"),
        );
        // A real one-tuple keeps them.
        assert_ne!(
            scope_paths("impl Tr for (W,) { fn f(&self) {} }", "f"),
            scope_paths("impl Tr for W { fn f(&self) {} }", "f"),
        );
    }

    /// A lifetime never separates two impls that coexist — rustc erases lifetimes before the
    /// coherence check, so `impl<'a> Tr for &'a W` and `impl Tr for &'static W` conflict. Erasing
    /// them here also puts `Foo<'a>`'s members back on the plain `Foo::method` surface that exact
    /// resolution looks for.
    #[test]
    fn lifetimes_are_erased_from_the_owner() {
        for (source, expected) in [
            ("impl<'a> Runs for &'a W { fn run(&self) {} }", "&W as Runs::run"),
            ("impl<'a> Index<'a> { fn run(&self) {} }", "Index::run"),
            ("impl<'a, T> Pair<'a, T> { fn run(&self) {} }", "Pair<_>::run"),
        ] {
            assert_eq!(scope_paths(source, "run"), vec![expected.to_string()], "{source}");
        }
    }

    /// Two same-tailed traits from different modules are two traits. Reducing both to `Runs`
    /// would give their identically-signatured methods one scope — and, with `scope_path` in the
    /// logical key, ONE logical symbol, so a memory or graph handle bound to either would answer
    /// for both. The whole trait path is what keeps them apart; `::` becomes `.` so the marker
    /// stays a single `::`-segment for `normalized_scope_path` to fold.
    #[test]
    fn same_tailed_traits_on_one_type_keep_separate_scopes() {
        let scopes = scope_paths(
            "struct Worker;\nimpl a::Runs for Worker { fn run(&self) {} }\nimpl b::Runs for \
             Worker { fn run(&self) {} }\n",
            "run",
        );
        assert_eq!(scopes, vec!["Worker as a.Runs::run", "Worker as b.Runs::run"]);
    }

    /// The receiver surface must be unchanged by the wider marker: resolution folds ` as Trait`
    /// away, so both impls' methods still answer to `Worker::run`.
    ///
    /// This drives the fold with EMITTED scopes, not literals — the fold itself is unchanged
    /// code, so a test that hand-writes `Worker as a.Runs::run` passes no matter what
    /// `trait_marker` emits. Going through the parser is what makes this fail if the marker ever
    /// regresses to carrying its own `::`, which would leave the fold unable to tell the trait
    /// path from the segments after it.
    #[test]
    fn every_emitted_trait_marker_folds_to_the_plain_receiver_scope() {
        let sources = [
            "impl a::Runs for Worker { fn run(&self) {} }",
            "impl std::fmt::Display for Worker { fn run(&self) {} }",
            "impl ::a::b::Runs for Worker { fn run(&self) {} }",
            "impl crate::traits::Runs<Item> for Worker { fn run(&self) {} }",
            "impl Runs for Worker { fn run(&self) {} }",
        ];
        for source in sources {
            let scope = scope_paths(source, "run").pop().expect("a method scope");
            assert!(
                scope.starts_with("Worker as ") && scope.ends_with("::run"),
                "{source} must emit one trait-marked segment, got {scope}"
            );
            assert_eq!(
                scope.matches("::").count(),
                1,
                "the marker must stay ONE ::-segment, got {scope}"
            );
            let folded =
                crate::index::edges::normalized_scope_path(&scope, Some(Language::Rust.as_str()));
            assert_eq!(folded.as_ref(), "Worker::run", "{source} folded to {folded}");
        }
    }

    /// The marker is raw source text on an IDENTITY field, so cosmetic spelling must not split one
    /// trait into two logical symbols. Interior whitespace is cosmetic; a comment is too.
    #[test]
    fn cosmetic_trait_path_spelling_does_not_change_the_marker() {
        for source in [
            "impl a::Runs for Worker { fn run(&self) {} }",
            "impl a :: Runs for Worker { fn run(&self) {} }",
            "impl a::\n    Runs for Worker { fn run(&self) {} }",
        ] {
            assert_eq!(
                scope_paths(source, "run"),
                vec!["Worker as a.Runs::run".to_string()],
                "{source} must normalize to the same marker"
            );
        }
    }

    /// A leading `::` is the one path token that is NOT cosmetic. Since the 2018 edition it selects
    /// the extern prelude, so with a local `mod a` in scope `a::Runs` and `::a::Runs` name traits
    /// from different crates — rustc accepts both impls on one type. Folding the prefix away gave
    /// them one scope and merged their `run` methods into a single logical symbol.
    #[test]
    fn a_leading_path_root_is_not_cosmetic() {
        assert_eq!(scope_paths("impl ::a::Runs for Worker { fn run(&self) {} }", "run"), vec![
            "Worker as .a.Runs::run".to_string()
        ]);
        assert_ne!(
            scope_paths("impl ::a::Runs for Worker { fn run(&self) {} }", "run"),
            scope_paths("impl a::Runs for Worker { fn run(&self) {} }", "run"),
        );
        // Same on the owner side: `::a::W` is another crate's type, not this module's `a::W`.
        assert_ne!(
            scope_paths("impl Runs for ::a::W { fn run(&self) {} }", "run"),
            scope_paths("impl Runs for a::W { fn run(&self) {} }", "run"),
        );
    }

    /// A comment and the width of a whitespace run cannot carry a type's meaning, so neither may
    /// split one const-generic owner into two. What the block holds inside a LITERAL is meaning and
    /// stays byte for byte.
    #[test]
    fn a_const_block_normalizes_only_what_cannot_carry_meaning() {
        let plain = scope_paths("impl Tr for F<{ N + 1 }> { fn f(&self) {} }", "f");
        assert_eq!(
            scope_paths("impl Tr for F<{  N  +  1  }> { fn f(&self) {} }", "f"),
            plain,
            "the width of a whitespace run is not part of the type"
        );
        assert_eq!(
            scope_paths("impl Tr for F<{ /* why */ N + 1 }> { fn f(&self) {} }", "f"),
            plain,
            "a comment is not part of the type"
        );
        // A brace inside a comment must not end the block early — that would hand the tail to the
        // whitespace-collapsing tidier and render two distinct owners identically.
        let one = scope_paths(r#"impl Tr for F<{ /* } */ "a ".len() }> { fn f(&self) {} }"#, "f");
        let two = scope_paths(r#"impl Tr for F<{ /* } */ "a  ".len() }> { fn f(&self) {} }"#, "f");
        assert_ne!(one, two, "these are `F<2>` and `F<3>`, so they must not share a scope");
        assert_eq!(one, vec![r#"F<{ "a ".len() }> as Tr::f"#.to_string()]);
    }

    /// An impl target with no name to take (unit, tuple, slice, `dyn`) is RENDERED, never dropped
    /// and never the trait's. Dropping it left the impl with no scope segment at all, so its
    /// members sat at FILE scope, where they could collide with free functions.
    #[test]
    fn a_non_nominal_impl_target_owns_its_members() {
        for (target, owner) in [
            ("()", "()"),
            ("(Alpha, Beta)", "(Alpha,Beta)"),
            ("[Alpha]", "[Alpha]"),
            ("dyn Runs", "dyn Runs"),
        ] {
            let source = format!("impl std::fmt::Display for {target} {{ fn fmt(&self) {{}} }}");
            assert_eq!(
                scope_paths(&source, "fmt"),
                vec![format!("{owner} as std.fmt.Display::fmt")],
                "`impl Display for {target}` must own its members without adopting `Display`"
            );
        }
    }
}
