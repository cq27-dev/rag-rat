use std::path::Path;

use tree_sitter::Node;

use super::{ParserBackend, ResolverPolicy, SymbolMatch};
use crate::index::parser::{self, ParserKind};

mod edges;
pub(super) use edges::c_like_edges;

pub(super) static C_SUPPORT: C = C;
pub(super) static CPP_SUPPORT: Cpp = Cpp;

pub(super) struct C;
pub(super) struct Cpp;

impl ParserBackend for C {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["enum", "function", "macro", "struct", "type", "union"]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::C
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        // Index definitions, not prototypes/forward declarations/uses: otherwise type-reference
        // edges bind to the tiny declaration occurrence instead of the real definition (#61).
        match node.kind() {
            "function_definition" =>
                Some(("function", function_name(node).or_else(|| parser::child_name(node))?)),
            "struct_specifier" if has_body(node) => Some(("struct", parser::child_name(node)?)),
            "union_specifier" if has_body(node) => Some(("union", parser::child_name(node)?)),
            "enum_specifier" if has_body(node) => Some(("enum", parser::child_name(node)?)),
            "type_definition" => Some(("type", parser::child_name(node)?)),
            "preproc_function_def" => Some(("macro", parser::child_name(node)?)),
            _ => None,
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        match node.kind() {
            "struct_specifier" | "union_specifier" if has_body(node) =>
                parser::node_text(parser::child_name(node)?, text),
            _ => None,
        }
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "preproc_include"
    }
}

impl ParserBackend for Cpp {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["class", "enum", "function", "macro", "namespace", "struct", "type", "union"]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Cpp
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        // As for C, bodyless declarations are deliberately not symbols (#61).
        match node.kind() {
            "function_definition" =>
                Some(("function", function_name(node).or_else(|| parser::child_name(node))?)),
            "class_specifier" if has_body(node) => Some(("class", parser::child_name(node)?)),
            "struct_specifier" if has_body(node) => Some(("struct", parser::child_name(node)?)),
            "union_specifier" if has_body(node) => Some(("union", parser::child_name(node)?)),
            "enum_specifier" if has_body(node) => Some(("enum", parser::child_name(node)?)),
            "type_definition" | "alias_declaration" => Some(("type", parser::child_name(node)?)),
            "namespace_definition" => Some(("namespace", parser::child_name(node)?)),
            "preproc_function_def" => Some(("macro", parser::child_name(node)?)),
            _ => None,
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = match node.kind() {
            "namespace_definition" => parser::child_name(node)?,
            "struct_specifier" | "union_specifier" | "class_specifier" if has_body(node) =>
                parser::child_name(node)?,
            _ => return None,
        };
        parser::node_text(name, text)
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "preproc_include"
    }
}

fn has_body(node: Node<'_>) -> bool {
    node.child_by_field_name("body").is_some()
}

fn function_name(node: Node<'_>) -> Option<Node<'_>> {
    let declarator = parser::first_descendant_node(node, &["function_declarator"]).unwrap_or(node);
    let name_root = declarator.child_by_field_name("declarator").unwrap_or(declarator);
    if parser::NAME_KINDS.contains(&name_root.kind()) {
        return Some(name_root);
    }
    parser::last_descendant_node(name_root, parser::NAME_KINDS)
}

macro_rules! impl_resolver_policy {
    ($backend:ty) => {
        impl ResolverPolicy for $backend {
            fn preferred_kinds(&self, _edge_kind: &str) -> Option<super::KindPreference> {
                None
            }

            fn type_reference_requires_type_definition(&self) -> bool {
                true
            }
        }
    };
}

impl_resolver_policy!(C);
impl_resolver_policy!(Cpp);
