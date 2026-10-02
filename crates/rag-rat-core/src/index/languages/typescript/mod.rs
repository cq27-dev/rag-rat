use std::path::Path;

use tree_sitter::Node;

use super::{ErrorRecovery, ParserBackend, RecoveryContext, SymbolMatch};
use crate::index::parser::{self, ParserKind};

mod edges;
pub(super) use edges::typescript_edges;

/// The node kinds that name a declaration when it has no `name` field (`parser::child_name`).
const NAME_KINDS: &[&str] = &["identifier", "type_identifier", "property_identifier"];

/// The node kinds the identifier helpers ([`crate::index::edges::identifiers_under`] and friends)
/// collect.
const IDENTIFIER_KINDS: &[&str] =
    &["identifier", "type_identifier", "property_identifier", "shorthand_property_identifier"];

pub(super) static SUPPORT: TypeScript = TypeScript;

/// The declarations recovered from beneath an ERROR at the top level or in a namespace body.
const TOP_LEVEL_DECLARATIONS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "class_declaration",
    "abstract_class_declaration",
    "interface_declaration",
    "type_alias_declaration",
    "lexical_declaration",
    "variable_declaration",
];

/// One broken statement can swallow the declarations after it into a top-level ERROR (a missing
/// `)` makes the rest of the file a parameter list); those that still parsed whole are kept, and
/// so are an `export` of one and a variable declaration, whose declarators the backend emits.
/// A namespace or `declare module` body is a `statement_block`, which is a container body only
/// beneath one: a function body's declarations are locals. There is no `class_body` context: an
/// error in a class body leaves its ERROR beside the methods, never around one, or folds the whole
/// class into a top-level ERROR.
static ERROR_RECOVERY: ErrorRecovery = ErrorRecovery {
    file_root: "program",
    contexts: &[
        RecoveryContext { kind: "program", within: &[], legal: TOP_LEVEL_DECLARATIONS },
        RecoveryContext {
            kind: "statement_block",
            within: &["internal_module", "module"],
            legal: TOP_LEVEL_DECLARATIONS,
        },
    ],
    container_keywords: &["class", "interface", "namespace", "module"],
};

pub(super) struct TypeScript;

impl ParserBackend for TypeScript {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["class", "const", "function", "interface", "type"]
    }

    fn function_scopes(&self) -> &'static [&'static str] {
        &[
            "function_declaration",
            "generator_function_declaration",
            "method_definition",
            "function_expression",
            "generator_function",
            "arrow_function",
            "class_static_block",
        ]
    }

    fn local_variable_kinds(&self) -> &'static [&'static str] {
        &["const"]
    }

    fn member_bodies(&self) -> &'static [&'static str] {
        &["class_body"]
    }

    fn parser_kind(&self, path: &Path) -> ParserKind {
        if path.extension().and_then(|ext| ext.to_str()) == Some("tsx") {
            ParserKind::Tsx
        } else {
            ParserKind::TypeScript
        }
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        match declaration_kind(node) {
            "function_declaration" | "method_definition" | "generator_function_declaration" =>
                Some(("function", parser::child_name(node, NAME_KINDS)?)),
            "class_declaration" => Some(("class", parser::child_name(node, NAME_KINDS)?)),
            "interface_declaration" => Some(("interface", parser::child_name(node, NAME_KINDS)?)),
            "type_alias_declaration" => Some(("type", parser::child_name(node, NAME_KINDS)?)),
            "variable_declarator" | "public_field_definition" =>
                Some(("const", parser::child_name(node, NAME_KINDS)?)),
            _ => None,
        }
    }

    fn for_each_declared_name<'tree>(
        &self,
        node: Node<'tree>,
        text: &str,
        emit: &mut dyn FnMut(Node<'tree>),
    ) {
        match declaration_kind(node) {
            // `[K in ...]` binds `K` the way `<T>` binds `T`; an abstract class is not a symbol but
            // still declares its name.
            "type_parameter" | "mapped_type_clause" | "abstract_class_declaration" =>
                node.child_by_field_name("name").into_iter().for_each(emit),
            // `infer U [extends C]`: the binder is the first named child, the constraint follows.
            "infer_type" => node
                .named_child(0)
                .filter(|binder| binder.kind() == "type_identifier")
                .into_iter()
                .for_each(emit),
            _ => self.for_each_declared_symbol_name(node, text, emit),
        }
    }

    fn error_recovery(&self) -> Option<&'static ErrorRecovery> {
        Some(&ERROR_RECOVERY)
    }

    /// An `export` is judged by the declaration it exports.
    fn declaration_kind<'tree>(&self, node: Node<'tree>) -> &'tree str {
        match node.child_by_field_name("declaration") {
            Some(exported) if node.kind() == "export_statement" => declaration_kind(exported),
            _ => declaration_kind(node),
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = match declaration_kind(node) {
            "class_declaration" | "interface_declaration" | "internal_module" | "module" =>
                parser::child_name(node, NAME_KINDS)?,
            _ => return None,
        };
        parser::node_text(name, text)
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "import_statement"
    }
}

/// A node's kind read as a declaration. A named class expression directly beneath an ERROR node,
/// in statement position, is a class declaration the parser could not attach as a statement
/// (`class K {}` after a broken line swallowed the statement boundary), so it declares `K` and
/// scopes its members. Statement position is first beneath the ERROR, after a named node, or after
/// one of the tokens `}` and `;` (a statement's end) or `export`, `default` and `abstract` (a
/// declaration's modifiers). After `abstract` it is an abstract class declaration, read exactly as
/// a clean parse reads one: it declares its name but is no symbol and scopes nothing, so a broken
/// line elsewhere does not change its members' keys. Anywhere else a class expression is a value
/// and declares nothing: an anonymous one has no name of its own (reading one would borrow a
/// member's or the `extends` target's), and one after a bare token expects an operand (`f(class K
/// {})`, `[class K {}`, `x = class K {}`), so it is an argument or an initializer.
fn declaration_kind<'tree>(node: Node<'tree>) -> &'tree str {
    if node.kind() != "class"
        || !node.parent().is_some_and(|parent| parent.is_error())
        || node.child_by_field_name("name").is_none()
    {
        return node.kind();
    }
    // Comments are extras the parser attaches anywhere, so the lookbehind skips them: a comment
    // between a token and the class neither ends a statement nor hides an `abstract`. An ERROR can
    // be flagged extra too, but it is the folded statement the lookbehind reads, so it is kept.
    let is_comment = |n: &Node<'_>| n.is_extra() && !n.is_error();
    let prev =
        std::iter::successors(node.prev_sibling(), |n| n.prev_sibling()).find(|n| !is_comment(n));
    // The `abstract` modifier is the class's previous sibling, or the last token of an ERROR the
    // parser folded it into together with the broken statement before it.
    let mut last_token = prev;
    while let Some(child) = last_token.and_then(|token| {
        (0..token.child_count()).rev().filter_map(|i| token.child(i)).find(|n| !is_comment(n))
    }) {
        last_token = Some(child);
    }
    if last_token.is_some_and(|token| token.kind() == "abstract") {
        return "abstract_class_declaration";
    }
    // ponytail: one-token lookbehind; an operand-expecting token folded into a preceding ERROR is
    // not seen.
    match prev {
        None => "class_declaration",
        Some(prev)
            if prev.is_named() || matches!(prev.kind(), "}" | ";" | "export" | "default") =>
            "class_declaration",
        Some(_) => node.kind(),
    }
}
