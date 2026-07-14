use std::path::Path;

use tree_sitter::Node;

use super::{EdgeBackend, ParserBackend, SymbolMatch};
use crate::index::edges::{EdgeCandidate, IndexedSymbol};
use crate::index::parser::{self, ParserKind};

mod edges;

pub(super) static SUPPORT: TypeScript = TypeScript;

pub(super) struct TypeScript;

impl ParserBackend for TypeScript {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["class", "const", "function", "interface", "type"]
    }

    fn parser_kind(&self, path: &Path) -> ParserKind {
        if path.extension().and_then(|ext| ext.to_str()) == Some("tsx") {
            ParserKind::Tsx
        } else {
            ParserKind::TypeScript
        }
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        match node.kind() {
            "function_declaration" | "method_definition" | "generator_function_declaration" =>
                Some(("function", parser::child_name(node)?)),
            "class_declaration" => Some(("class", parser::child_name(node)?)),
            "interface_declaration" => Some(("interface", parser::child_name(node)?)),
            "type_alias_declaration" => Some(("type", parser::child_name(node)?)),
            "variable_declarator" | "public_field_definition" =>
                Some(("const", parser::child_name(node)?)),
            _ => None,
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "internal_module"
            | "module"
            | "namespace_declaration" => parser::child_name(node)?,
            _ => return None,
        };
        parser::node_text(name, text)
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "import_statement"
    }
}

impl EdgeBackend for TypeScript {
    fn edges(
        &self,
        text: &str,
        node: Node<'_>,
        symbols: &[IndexedSymbol],
        path: &Path,
        out: &mut Vec<EdgeCandidate>,
    ) {
        edges::typescript_edges(text, node, symbols, path, out);
    }
}
