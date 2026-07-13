use std::path::Path;

use tree_sitter::Node;

use super::{EdgeBackend, ParserBackend, ResolverPolicy, SymbolMatch};
use crate::index::edges::{EdgeCandidate, IndexedSymbol};
use crate::index::parser::{self, ParserKind};

mod edges;

pub(super) static SUPPORT: Kotlin = Kotlin;

pub(super) struct Kotlin;

impl ParserBackend for Kotlin {
    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Kotlin
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        match node.kind() {
            "class_declaration" => Some(("class", parser::child_name(node)?)),
            "object_declaration" => Some(("object", parser::child_name(node)?)),
            "function_declaration" => Some(("function", parser::child_name(node)?)),
            "property_declaration" => Some(("property", property_name(node)?)),
            "companion_object" | "companion_object_declaration" =>
                Some(("object", companion_name(node).unwrap_or(node))),
            _ => None,
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        match node.kind() {
            "class_declaration" | "object_declaration" =>
                parser::node_text(parser::child_name(node)?, text),
            _ => None,
        }
    }

    fn is_test_symbol(&self, text: &str, node: Node<'_>, _scope_path: &str, _name: &str) -> bool {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).any(|child| {
            child.kind() == "modifiers"
                && parser::node_text(child, text)
                    .as_deref()
                    .is_some_and(modifiers_have_test_annotation)
        })
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || matches!(node.kind(), "import_header" | "package_header")
    }
}

fn modifiers_have_test_annotation(modifiers: &str) -> bool {
    modifiers.split('@').skip(1).any(|annotation| {
        let name = annotation.split(['(', ' ', '\n', '\t', '\r']).next().unwrap_or_default();
        let last = name.rsplit('.').next().unwrap_or(name);
        matches!(last, "Test" | "ParameterizedTest" | "RepeatedTest" | "TestFactory")
    })
}

fn companion_name(node: Node<'_>) -> Option<Node<'_>> {
    for index in 0..node.child_count() {
        let Some(index) = u32::try_from(index).ok() else {
            continue;
        };
        if let Some(child) = node.child(index)
            && child.kind() == "companion"
        {
            return Some(child);
        }
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "simple_identifier" | "type_identifier"))
}

fn property_name(node: Node<'_>) -> Option<Node<'_>> {
    parser::child_name(variable_declaration(node).unwrap_or(node))
}

fn variable_declaration(node: Node<'_>) -> Option<Node<'_>> {
    crate::index::grow_stack(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).find_map(|child| {
            if child.kind() == "variable_declaration" {
                Some(child)
            } else if matches!(child.kind(), "modifiers" | "type_parameters" | "type_constraints") {
                None
            } else {
                variable_declaration(child)
            }
        })
    })
}

impl EdgeBackend for Kotlin {
    fn edges(
        &self,
        text: &str,
        node: Node<'_>,
        symbols: &[IndexedSymbol],
        path: &Path,
        out: &mut Vec<EdgeCandidate>,
    ) {
        edges::kotlin_edges(text, node, symbols, path, out);
    }
}

impl ResolverPolicy for Kotlin {
    fn preferred_kinds(&self, _edge_kind: &str) -> Option<super::KindPreference> {
        None
    }

    fn allow_type_receiver_fallback(&self) -> bool {
        true
    }

    fn allow_value_receiver_fallback(&self) -> bool {
        true
    }
}
