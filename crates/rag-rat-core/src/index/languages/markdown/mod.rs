use std::path::Path;

use tree_sitter::Node;

use super::{ParserBackend, SymbolMatch};
use crate::index::parser::ParserKind;

pub(super) static SUPPORT: Markdown = Markdown;

pub(super) struct Markdown;

impl ParserBackend for Markdown {
    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Markdown
    }

    fn symbol_node<'tree>(&self, _node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        None
    }

    fn scope_segment(&self, _node: Node<'_>, _text: &str) -> Option<String> {
        None
    }

    fn is_plumbing_node(&self, _node: Node<'_>) -> bool {
        true
    }
}
