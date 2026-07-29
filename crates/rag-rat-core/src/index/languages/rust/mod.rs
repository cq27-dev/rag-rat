use std::path::Path;

use tree_sitter::Node;

use super::{
    ParserBackend, QualifiedRoot, ReceiverFallback, ReferenceDisposition, ResolutionPolicy,
    SymbolMatch, TypeBinding,
};
use crate::index::edges::EdgeKind;
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

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = match node.kind() {
            "mod_item" | "trait_item" => parser::child_name(node)?,
            "impl_item" => {
                let segment = parser::node_text(impl_name(node)?, text)?;
                // `impl Trait for Type`: keep the trait's tail in the scope segment
                // (`Type as Trait`) so two traits' same-named, same-signature methods on one
                // type stay DISTINCT logical symbols instead of collapsing into one. Resolution
                // folds the ` as Trait` marker away (`normalized_scope_path`), so the receiver
                // surface both traits expose is still `Type::method` — and a call that could hit
                // either declines as ambiguous (#567).
                return match node.child_by_field_name("trait") {
                    Some(trait_node) => {
                        let trait_text = parser::node_text(trait_node, text)?;
                        let trait_path = crate::index::edges::degeneric_path(&trait_text);
                        let tail = trait_path.rsplit("::").next().unwrap_or(&trait_path).trim();
                        Some(if tail.is_empty() { segment } else { format!("{segment} as {tail}") })
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
    fn generic_trait_scope_uses_the_degenericized_trait_tail() {
        let parsed = parser::parse_file(
            Path::new("src/lib.rs"),
            Language::Rust,
            "impl crate::traits::Runs<Item> for Worker { fn run(&self) {} }",
        )
        .expect("Rust parses");
        let run = parsed.symbols.iter().find(|symbol| symbol.name == "run").expect("method symbol");
        assert_eq!(run.scope_path, "Worker as Runs::run");
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
        assert_eq!(scopes, vec!["Alpha as Display::fmt", "Beta as Display::fmt"]);
    }

    /// An impl target with no name to take (unit, tuple, slice, `dyn`) must yield NO owner rather
    /// than borrow the trait's.
    #[test]
    fn a_non_nominal_impl_target_claims_no_owner() {
        for target in ["()", "(Alpha, Beta)", "[Alpha]", "dyn Runs"] {
            let source = format!("impl std::fmt::Display for {target} {{ fn fmt(&self) {{}} }}");
            assert_eq!(
                scope_paths(&source, "fmt"),
                vec!["fmt".to_string()],
                "`impl Display for {target}` must not adopt `Display` as its owner"
            );
        }
    }
}
