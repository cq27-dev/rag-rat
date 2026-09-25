use std::path::Path;

use tree_sitter::Node;

use super::{
    ErrorRecovery, ParserBackend, ReceiverFallback, RecoveryContext, ResolutionPolicy, SymbolMatch,
};
use crate::index::edges::named_children;
use crate::index::parser::{self, ParserKind};

mod edges;
pub(super) use edges::kotlin_edges;

/// The node kinds that name a declaration when it has no `name` field (`parser::child_name`).
const NAME_KINDS: &[&str] = &["identifier"];

/// The node kinds the identifier helpers ([`crate::index::edges::identifiers_under`] and friends)
/// collect.
const IDENTIFIER_KINDS: &[&str] = NAME_KINDS;

pub(super) static SUPPORT: Kotlin = Kotlin;

const TOP_LEVEL_DECLARATIONS: &[&str] =
    &["class_declaration", "object_declaration", "function_declaration", "property_declaration"];
const MEMBER_DECLARATIONS: &[&str] = &[
    "class_declaration",
    "object_declaration",
    "function_declaration",
    "property_declaration",
    "companion_object",
];

/// kotlin-ng cannot parse a member on the same line as its class's opening brace
/// (`class A { fun f() {} }`): the member lands in an ERROR inside the class body, though the
/// member itself parses whole.
static ERROR_RECOVERY: ErrorRecovery = ErrorRecovery {
    file_root: "source_file",
    contexts: &[
        RecoveryContext { kind: "source_file", within: &[], legal: TOP_LEVEL_DECLARATIONS },
        RecoveryContext { kind: "class_body", within: &[], legal: MEMBER_DECLARATIONS },
        RecoveryContext { kind: "enum_class_body", within: &[], legal: MEMBER_DECLARATIONS },
    ],
    container_keywords: &["class", "object", "interface"],
};

pub(super) struct Kotlin;

impl ParserBackend for Kotlin {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["class", "function", "object", "property"]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Kotlin
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        match node.kind() {
            "class_declaration" => Some(("class", parser::child_name(node, NAME_KINDS)?)),
            "object_declaration" => Some(("object", parser::child_name(node, NAME_KINDS)?)),
            "function_declaration" => Some(("function", parser::child_name(node, NAME_KINDS)?)),
            "property_declaration" => Some(("property", property_name(node)?)),
            "companion_object" => Some(("object", companion_name(node).unwrap_or(node))),
            _ => None,
        }
    }

    fn error_recovery(&self) -> Option<&'static ErrorRecovery> {
        Some(&ERROR_RECOVERY)
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        match node.kind() {
            "class_declaration" | "object_declaration" =>
                parser::node_text(parser::child_name(node, NAME_KINDS)?, text),
            _ => None,
        }
    }

    fn is_test_symbol(&self, text: &str, node: Node<'_>, _scope_path: &str, _name: &str) -> bool {
        named_children(node).any(|child| {
            child.kind() == "modifiers"
                && parser::node_text(child, text)
                    .as_deref()
                    .is_some_and(modifiers_have_test_annotation)
        })
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || matches!(node.kind(), "import" | "package_header")
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
        if let Some(child) = node.child(index)
            && child.kind() == "companion"
        {
            return Some(child);
        }
    }
    named_children(node).find(|child| child.kind() == "identifier")
}

fn property_name(node: Node<'_>) -> Option<Node<'_>> {
    parser::child_name(variable_declaration(node).unwrap_or(node), NAME_KINDS)
}

fn variable_declaration(node: Node<'_>) -> Option<Node<'_>> {
    rag_rat_base::stack::grow_stack(|| {
        named_children(node).find_map(|child| {
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

pub(super) const RESOLVER_POLICY: ResolutionPolicy = ResolutionPolicy {
    receiver_fallback: ReceiverFallback::TypeAndValue,
    ..ResolutionPolicy::DEFAULT
};

#[cfg(test)]
mod tests {
    use tree_sitter::Parser;

    use super::*;

    /// An import-only stretch of a Kotlin file carries no embed-worthy signal, the same as Go's
    /// imports and Swift's. kotlin-ng names the node `import` (one per statement, directly under
    /// the file), so that is the kind the plumbing check has to name.
    #[test]
    fn imports_package_header_and_comments_are_plumbing() {
        let source =
            "package com.example\n\n// a comment\nimport kotlin.io.println\n\nfun main() {}\n";
        let mut parser = Parser::new();
        parser
            .set_language(&parser::grammar_for(ParserKind::Kotlin).expect("kotlin grammar"))
            .expect("set kotlin language");
        let tree = parser.parse(source, None).expect("parse kotlin source");

        let plumbing = named_children(tree.root_node())
            .filter(|child| SUPPORT.is_plumbing_node(*child))
            .map(|child| child.kind())
            .collect::<Vec<_>>();

        assert_eq!(plumbing, vec!["package_header", "line_comment", "import"]);
    }
}
