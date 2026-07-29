use std::path::Path;

use tree_sitter::Node;

use super::{
    ParserBackend, QualifiedRoot, ReceiverFallback, ReferenceDisposition, ResolutionPolicy,
    SymbolMatch, TypeBinding,
};
use crate::index::edges::EdgeKind;
use crate::index::parser::{self, ParsedSymbolFact, ParserKind};

mod binders;
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

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = match node.kind() {
            "mod_item" | "trait_item" => parser::child_name(node)?,
            "impl_item" => {
                let segment = impl_owner_segment(node, text)?;
                // `impl Trait for Type`: keep the trait in the scope segment (`Type as Trait`)
                // so two traits' same-named, same-signature methods on one type stay DISTINCT
                // logical symbols instead of collapsing into one. Resolution folds the
                // ` as Trait` marker away (`normalized_scope_path`), so the receiver surface both
                // traits expose is still `Type::method` — and a call that could hit either
                // declines as ambiguous (#567).
                return match node.child_by_field_name("trait") {
                    Some(trait_node) => {
                        let trait_text = parser::node_text(trait_node, text)?;
                        Some(match trait_marker(&trait_text) {
                            Some(marker) => format!("{segment} as {marker}"),
                            None => segment,
                        })
                    },
                    None => Some(segment),
                };
            },
            _ => return None,
        };
        parser::node_text(name, text)
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

/// The trait half of a `Type as Trait` scope marker: the trait's WHOLE path, generics folded,
/// whitespace and a leading `::` stripped, and `::` rewritten to `.`.
///
/// The whole path, because a tail alone gives `impl a::Runs for Worker` and `impl b::Runs for
/// Worker` the same scope — collapsing two distinct traits' identically-signatured methods into
/// one logical symbol, the exact leak trait-qualified scopes exist to prevent. `.` for `::`,
/// because the marker has to stay ONE `::`-segment: `normalized_scope_path` folds ` as Trait`
/// away by splitting the scope on `::` and then on " as ", and a trait path that kept its own
/// `::` would be indistinguishable from the segments that follow it. Whitespace and the leading
/// `::` go because this is raw source text on an IDENTITY field: `impl a :: Runs` and
/// `impl a::Runs` name one trait and must not become two logical symbols.
///
/// What this does NOT canonicalize, deliberately, because extraction cannot resolve a trait path
/// to its declaration:
/// - Import spelling. `use std::fmt; impl fmt::Display for X` and `impl std::fmt::Display for X`
///   yield different markers, so two cfg variants of one impl spelled differently SPLIT into two
///   logical symbols. Splitting is the safe direction (a handle answers for less than it should,
///   rather than for something it should not), and it is the same direction the rest of this PR's
///   inference chooses when it cannot prove an identity.
/// - Generic arguments. `degeneric_path` folds them, so `impl TryFrom<&str> for Cfg` and `impl
///   TryFrom<String> for Cfg` share `Cfg as TryFrom` and their same-signature associated types
///   still collapse. Keeping them here would not help — `logical_scope_path` degenericks the scope
///   AGAIN for the logical key, on purpose, so that cfg variants with different binder names group.
///   Closing that needs alpha-normalized binders (#1012), not a wider marker.
fn trait_marker(trait_text: &str) -> Option<String> {
    let degeneric = crate::index::edges::degeneric_path(trait_text);
    let dense = degeneric.chars().filter(|ch| !ch.is_whitespace()).collect::<String>();
    let marker = dense.strip_prefix("::").unwrap_or(&dense).replace("::", ".");
    (!marker.is_empty()).then_some(marker)
}

/// The impl's SELF TYPE, rendered as identity rather than picked out as a node.
///
/// Two impls in one file are the same entity exactly when a single compilation could not hold
/// both, and rustc's coherence check is the authority on that: `impl Runs for Worker`,
/// `for &Worker`, and `for *const Worker` all compile together, so the wrapper is part of who
/// they are — peeling it (as taking the innermost nominal node does) gives all three the same
/// scope and collapses their same-signature members into one logical symbol. The idiomatic
/// operator quartet (`impl Neg for T` / `for &T`, whose `type Output` is byte-identical across
/// all of them) is the shape that hits this constantly.
///
/// A non-nominal target (`()`, `(A, B)`, `[T]`, `dyn X`) has no node to name, and returning
/// `None` used to drop the impl's scope segment ENTIRELY — its members then sat at file scope,
/// where they could collide with free functions. Rendering the target keeps them owned.
///
/// LIFETIMES ARE ERASED, because coherence erases them: `impl<'a> Tr for &'a W` and
/// `impl Tr for &'static W` are a conflicting-implementations error, so a lifetime can never be
/// what separates two coexisting impls. Erasing also puts `Foo<'a>`'s members back on the plain
/// `Foo::method` surface that exact resolution looks for.
///
/// Binder NAMES still leak (`impl<T> Foo<T>` vs `impl<U> Foo<U>` render differently and so split
/// where they should group) and concrete generic arguments are still folded away downstream by
/// `logical_scope_path`. Both are the alpha-normalization half of this rule, deferred to #1012;
/// splitting is the safe direction to be wrong in meanwhile.
fn impl_owner_segment(node: Node<'_>, text: &str) -> Option<String> {
    let target = node.child_by_field_name("type")?;
    let rendered = render_type_without_lifetimes(target, text);
    (!rendered.is_empty()).then_some(rendered)
}

/// Source text of a type with every lifetime removed and whitespace normalized. Splices out the
/// `lifetime` nodes rather than re-rendering the tree, so every other token — `&`, `&mut`,
/// `*const`, `*mut`, tuples, slices, `dyn`, concrete arguments — survives exactly as written.
fn render_type_without_lifetimes(node: Node<'_>, text: &str) -> String {
    let start = node.start_byte();
    let Some(source) = text.get(start..node.end_byte()) else {
        return String::new();
    };
    let (lifetimes, literals) = collect_spans(node, start, source.len());
    // Drop the lifetimes, then normalize only OUTSIDE literal tokens. A const-generic length can
    // carry a string (`[u8; { "a, b".len() }]`), and two such owners differing only in the spaces
    // inside that literal are genuinely different types — collapsing their whitespace would render
    // them identically and merge two distinct impls.
    let mut kept = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (from, to) in lifetimes {
        if from >= cursor && to <= source.len() {
            kept.push_str(&tidy_between(source, cursor, from, &literals));
            cursor = to;
        }
    }
    kept.push_str(&tidy_between(source, cursor.min(source.len()), source.len(), &literals));
    tidy_joins(&kept)
}

/// Normalize `source[from..to]`, copying any byte inside a literal span verbatim.
fn tidy_between(source: &str, from: usize, to: usize, literals: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(to - from);
    let mut at = from;
    for &(literal_from, literal_to) in literals {
        if literal_to <= at || literal_from >= to {
            continue;
        }
        let plain_to = literal_from.max(at).min(to);
        out.push_str(&tidy_type_text(&source[at..plain_to]));
        let copy_to = literal_to.min(to);
        out.push_str(&source[plain_to..copy_to]);
        at = copy_to;
    }
    out.push_str(&tidy_type_text(&source[at.min(to)..to]));
    out
}

/// Lifetime spans to drop and literal spans to preserve, both relative to `start` and clamped to
/// the rendered slice.
fn collect_spans(
    node: Node<'_>,
    start: usize,
    len: usize,
) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    let (mut lifetimes, mut literals) = (Vec::new(), Vec::new());
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        let span = (
            current.start_byte().saturating_sub(start).min(len),
            current.end_byte().saturating_sub(start).min(len),
        );
        match current.kind() {
            "lifetime" => {
                lifetimes.push(span);
                continue;
            },
            kind if kind.ends_with("string_literal") || kind == "char_literal" => {
                literals.push(span);
                continue;
            },
            _ => {},
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    lifetimes.sort_unstable();
    literals.sort_unstable();
    (lifetimes, literals)
}

/// Collapse whitespace and the punctuation lifetime removal leaves behind, in a stretch of type
/// text known to contain no literal token: `Foo< , T>` → `Foo<T>`, `& mut W` → `&mut W`.
fn tidy_type_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !matches!(out.chars().last(), None | Some(' ')) {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
    // `&'a W` leaves `& W`, and `&'a mut W` leaves `& mut W` — close the reference sigil back up.
    // `*const`/`*mut` keep their space; it is part of the spelling.
    out.replace(" ,", ",")
        .replace(", ", ",")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace("& ", "&")
}

/// Whole-render cleanup, after the literal-preserving pieces are joined: an argument list emptied
/// by lifetime removal loses its brackets, and a leading `::` is not part of the identity.
fn tidy_joins(raw: &str) -> String {
    let mut cleaned = raw.to_string();
    // A dropped lifetime leaves its comma behind, and the pieces either side were normalized
    // independently, so the dangling punctuation only shows up once they are joined.
    while cleaned.contains("<,") || cleaned.contains(",,") || cleaned.contains(",>") {
        cleaned = cleaned.replace("<,", "<").replace(",,", ",").replace(",>", ">");
    }
    while let Some(empty) = cleaned.find("<>") {
        cleaned.replace_range(empty..empty + 2, "");
    }
    cleaned.trim().trim_start_matches("::").trim().to_string()
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
mod tests {
    use std::path::Path;

    use rag_rat_base::language::Language;

    use super::*;

    #[test]
    fn generic_trait_scope_uses_the_degenericized_trait_path() {
        let parsed = parser::parse_file(
            Path::new("src/lib.rs"),
            Language::Rust,
            "impl crate::traits::Runs<Item> for Worker { fn run(&self) {} }",
        )
        .expect("Rust parses");
        let run = parsed.symbols.iter().find(|symbol| symbol.name == "run").expect("method symbol");
        assert_eq!(run.scope_path, "Worker as crate.traits.Runs::run");
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

    /// The render normalizes whitespace, but a const-generic length can carry a string literal
    /// whose spaces are part of the VALUE — `[u8; { "a, b".len() }]` is `[u8; 4]` and
    /// `[u8; { "a,b".len() }]` is `[u8; 3]`, two different owners. Literal tokens are copied
    /// through untouched so the two do not render identically and merge.
    #[test]
    fn a_literal_inside_a_const_generic_owner_is_left_alone() {
        let spaced = scope_paths(r#"impl Tr for [u8; { "a, b".len() }] { fn f(&self) {} }"#, "f");
        let dense = scope_paths(r#"impl Tr for [u8; { "a,b".len() }] { fn f(&self) {} }"#, "f");
        assert_ne!(spaced, dense, "two different array lengths must not share one scope");
        assert!(
            spaced[0].contains(r#""a, b""#),
            "the literal is copied through verbatim: {spaced:?}"
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
            ("impl<'a, T> Pair<'a, T> { fn run(&self) {} }", "Pair<T>::run"),
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
    /// trait into two logical symbols. Interior whitespace and a leading `::` are normalized away.
    #[test]
    fn cosmetic_trait_path_spelling_does_not_change_the_marker() {
        for source in [
            "impl a::Runs for Worker { fn run(&self) {} }",
            "impl a :: Runs for Worker { fn run(&self) {} }",
            "impl ::a::Runs for Worker { fn run(&self) {} }",
            "impl a::\n    Runs for Worker { fn run(&self) {} }",
        ] {
            assert_eq!(
                scope_paths(source, "run"),
                vec!["Worker as a.Runs::run".to_string()],
                "{source} must normalize to the same marker"
            );
        }
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
