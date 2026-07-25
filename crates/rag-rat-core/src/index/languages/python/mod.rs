use std::path::Path;

use tree_sitter::Node;

use super::{
    EdgeBackend, EdgeEmitter, EdgeVisit, ImportAliasRebind, ImportAliasRequest, KindPreference,
    ParserBackend, ResolverPolicy, SymbolMatch,
};
use crate::index::parser::{self, ParserKind};

mod edges;

pub(super) static SUPPORT: Python = Python;

pub(super) struct Python;

impl ParserBackend for Python {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["class", "const", "function", "type"]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Python
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, text: &str) -> Option<SymbolMatch<'tree>> {
        match node.kind() {
            // The decorated node owns the span so API-defining decorators stay in the chunk; its
            // inner declaration supplies the name and signature source.
            "decorated_definition" => {
                let inner = node.child_by_field_name("definition")?;
                let kind = match inner.kind() {
                    "function_definition" => "function",
                    "class_definition" => "class",
                    _ => return None,
                };
                Some((kind, parser::child_name(inner)?))
            },
            "function_definition" if !parent_is_decorated(node) =>
                Some(("function", parser::child_name(node)?)),
            "class_definition" if !parent_is_decorated(node) =>
                Some(("class", parser::child_name(node)?)),
            "type_alias_statement" => Some(("type", parser::child_name(node)?)),
            // Only module/class SCREAMING_SNAKE_CASE assignments are symbols, never uppercase
            // function-local temporaries.
            "assignment" if assignment_is_const_scope(node) => {
                let target = node.child_by_field_name("left")?;
                let name = parser::node_text(target, text)?;
                (target.kind() == "identifier" && is_screaming_snake_case(&name))
                    .then_some(("const", target))
            },
            _ => None,
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        match node.kind() {
            "class_definition" | "function_definition" =>
                parser::node_text(parser::child_name(node)?, text),
            _ => None,
        }
    }

    fn is_test_symbol(&self, _text: &str, _node: Node<'_>, scope_path: &str, name: &str) -> bool {
        name.starts_with("test_") || scope_path_has_test_class(scope_path)
    }

    fn signature_source_node<'tree>(&self, node: Node<'tree>) -> Node<'tree> {
        if node.kind() == "decorated_definition"
            && let Some(inner) = node.child_by_field_name("definition")
        {
            return inner;
        }
        node
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment")
            || matches!(
                node.kind(),
                "import_statement"
                    | "import_from_statement"
                    | "future_import_statement"
                    | "pass_statement"
            )
            || is_docstring_statement(node)
    }
}

fn is_docstring_statement(node: Node<'_>) -> bool {
    node.kind() == "expression_statement"
        && node.named_child_count() == 1
        && node.named_child(0).is_some_and(|child| child.kind() == "string")
}

fn scope_path_has_test_class(scope_path: &str) -> bool {
    scope_path.split("::").any(|segment| {
        segment.starts_with("Test") || segment.ends_with("Test") || segment.ends_with("TestCase")
    })
}

fn is_screaming_snake_case(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn parent_is_decorated(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| parent.kind() == "decorated_definition")
}

fn assignment_is_const_scope(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        match current.kind() {
            "function_definition" | "lambda" => return false,
            "class_definition" | "module" => return true,
            _ => ancestor = current.parent(),
        }
    }
    false
}

impl EdgeBackend for Python {
    fn edges(&self, visit: EdgeVisit<'_, '_, '_>, emit: &mut EdgeEmitter<'_>) {
        edges::python_edges(visit, emit);
    }
}

impl ResolverPolicy for Python {
    fn preferred_kinds(&self, edge_kind: &str) -> Option<KindPreference> {
        (edge_kind == "implements").then_some(KindPreference {
            symbol_kinds: &["class", "object"],
            same_language_only: true,
        })
    }

    fn import_edges_carry_aliases(&self) -> bool {
        true
    }

    fn rebind_import_alias(&self, request: ImportAliasRequest<'_>) -> ImportAliasRebind {
        match request.receiver_hint {
            Some(receiver) => {
                let Some(target) = (request.lookup)(receiver) else {
                    return ImportAliasRebind::default();
                };
                ImportAliasRebind {
                    name: None,
                    target_qualified_name: request
                        .target_qualified_name
                        .map(|qualified| replace_qualified_root(qualified, &target)),
                    receiver_hint: Some(target),
                }
            },
            None => ImportAliasRebind {
                name: (request.lookup)(short_name(request.to_name)),
                ..ImportAliasRebind::default()
            },
        }
    }
}

fn short_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

fn replace_qualified_root(qualified: &str, root: &str) -> String {
    match qualified.split_once("::") {
        Some((_, rest)) => format!("{root}::{rest}"),
        None => root.to_string(),
    }
}
