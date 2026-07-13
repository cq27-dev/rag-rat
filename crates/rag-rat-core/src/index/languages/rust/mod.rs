use std::path::Path;

use tree_sitter::Node;

use super::{EdgeBackend, ParserBackend, ResolverPolicy, SymbolMatch};
use crate::index::edges::{EdgeCandidate, IndexedSymbol};
use crate::index::parser::{self, ParsedSymbolFact, ParserKind};

mod dispatch;
mod edges;

pub(super) static SUPPORT: Rust = Rust;

pub(super) struct Rust;

impl ParserBackend for Rust {
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
            "impl_item" => impl_name(node)?,
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

fn impl_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|child| {
        matches!(child.kind(), "type_identifier" | "generic_type" | "scoped_type_identifier")
    })
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

impl EdgeBackend for Rust {
    fn edges(
        &self,
        text: &str,
        node: Node<'_>,
        symbols: &[IndexedSymbol],
        path: &Path,
        out: &mut Vec<EdgeCandidate>,
    ) {
        edges::rust_edges(text, node, symbols, path, out);
    }
}

impl ResolverPolicy for Rust {
    fn preferred_kinds(&self, _edge_kind: &str) -> Option<super::KindPreference> {
        None
    }

    fn reference_is_unresolvable(&self, edge_kind: &str, name: &str) -> bool {
        edge_kind == crate::index::edges::EdgeKind::ReferencesType.as_str()
            && type_ref_is_unresolvable(name)
    }

    fn type_reference_requires_type_definition(&self) -> bool {
        true
    }

    fn allow_type_receiver_fallback(&self) -> bool {
        true
    }

    fn rejects_qualified_root(&self, root: &str) -> bool {
        is_external_root(root)
    }

    fn is_local_qualified_root(&self, root: &str) -> bool {
        matches!(root, "crate" | "self" | "super")
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
