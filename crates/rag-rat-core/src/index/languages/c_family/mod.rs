use std::path::Path;

use tree_sitter::Node;

use super::{ParserBackend, ResolutionPolicy, SymbolMatch, TypeBinding};
use crate::index::edges::IdentifierPath;
use crate::index::parser::{self, ParserKind};

mod edges;
pub(super) use edges::c_like_edges;

/// The node kinds that name a declaration when it has no `name` field (`parser::child_name`).
const NAME_KINDS: &[&str] =
    &["identifier", "type_identifier", "field_identifier", "namespace_identifier"];

/// The node kinds the identifier helpers ([`crate::index::edges::identifiers_under`] and friends)
/// collect.
const IDENTIFIER_KINDS: &[&str] = NAME_KINDS;
#[cfg(test)]
mod query_spike;

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
            "function_definition" => Some(("function", declarator_name(node)?)),
            "struct_specifier" if has_body(node) =>
                Some(("struct", node.child_by_field_name("name")?)),
            "union_specifier" if has_body(node) =>
                Some(("union", node.child_by_field_name("name")?)),
            "enum_specifier" if has_body(node) => Some(("enum", node.child_by_field_name("name")?)),
            "type_definition" => Some(("type", declarator_name(node)?)),
            "preproc_function_def" => Some(("macro", parser::child_name(node, NAME_KINDS)?)),
            _ => None,
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        match node.kind() {
            "struct_specifier" | "union_specifier" if has_body(node) =>
                parser::node_text(node.child_by_field_name("name")?, text),
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
            "function_definition" => Some(("function", declarator_name(node)?)),
            "class_specifier" if has_body(node) =>
                Some(("class", node.child_by_field_name("name")?)),
            "struct_specifier" if has_body(node) =>
                Some(("struct", node.child_by_field_name("name")?)),
            "union_specifier" if has_body(node) =>
                Some(("union", node.child_by_field_name("name")?)),
            "enum_specifier" if has_body(node) => Some(("enum", node.child_by_field_name("name")?)),
            "type_definition" => Some(("type", declarator_name(node)?)),
            "alias_declaration" => Some(("type", node.child_by_field_name("name")?)),
            "namespace_definition" => Some(("namespace", node.child_by_field_name("name")?)),
            "preproc_function_def" => Some(("macro", parser::child_name(node, NAME_KINDS)?)),
            _ => None,
        }
    }

    fn for_each_declared_name<'tree>(
        &self,
        node: Node<'tree>,
        text: &str,
        emit: &mut dyn FnMut(Node<'tree>),
    ) {
        match node.kind() {
            // `template<typename T>` / `template<class... Ts>`: the binder has no field.
            "type_parameter_declaration" | "variadic_type_parameter_declaration" =>
                crate::index::edges::named_children(node)
                    .filter(|child| child.kind() == "type_identifier")
                    .for_each(emit),
            "optional_type_parameter_declaration" =>
                node.child_by_field_name("name").into_iter().for_each(emit),
            // An out-of-line definition (`struct Outer::Fwd {}`) names itself by its path's last
            // segment, the token its type reference is spelled by. A specialization
            // (`template<> struct ns::Box<int> {}`) does not: its `Box` is a use of the primary
            // template, as the unqualified spelling `Box<int>` already is.
            _ => self.for_each_declared_symbol_name(node, text, &mut |name| {
                emit(name);
                if name.kind() == "qualified_identifier"
                    && let Some(tail) =
                        IdentifierPath::member_chain(name, text, IDENTIFIER_KINDS).last_node()
                    && tail.parent().is_none_or(|parent| parent.kind() != "template_type")
                {
                    emit(tail);
                }
            }),
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = match node.kind() {
            "namespace_definition" => node.child_by_field_name("name")?,
            "struct_specifier" | "union_specifier" | "class_specifier" if has_body(node) =>
                node.child_by_field_name("name")?,
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

/// The name a declaration's `declarator` field declares, read down the declarator chain: through
/// pointer, reference, array, function and parenthesized declarators, then along a C++ name's own
/// `name` field (`ns::run`, `run<T>`). Parameter lists, template arguments and scopes hang off
/// that chain as other fields and are never searched, so `void (*get_handler(int sig))(int)`
/// declares `get_handler`, not `sig`, and `template<> void foo<Bar>(Bar)` declares `foo`, not
/// `Bar`. An operator is named by its `operator_name` (`operator=`). Any other declarator end
/// (`operator bool()`, a dependent name) yields `None`: no name is better than a wrong one.
fn declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut declarator = node.child_by_field_name("declarator")?;
    loop {
        if NAME_KINDS.contains(&declarator.kind()) || declarator.kind() == "operator_name" {
            return Some(declarator);
        }
        declarator = match declarator.kind() {
            "qualified_identifier" | "template_function" =>
                declarator.child_by_field_name("name")?,
            // These wrap their inner declarator or name without a field. A destructor is named by
            // its class name.
            "parenthesized_declarator" | "reference_declarator" | "destructor_name" =>
                declarator.named_child(0)?,
            _ => declarator.child_by_field_name("declarator")?,
        };
    }
}

pub(super) const RESOLVER_POLICY: ResolutionPolicy =
    ResolutionPolicy { type_binding: TypeBinding::DefinitionsOnly, ..ResolutionPolicy::DEFAULT };
