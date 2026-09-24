//! TypeScript graph-edge extraction for the shared structural edge walk.

use tree_sitter::Node;

use crate::index::edges::*;

pub(in crate::index::languages) fn typescript_edges(
    EdgeVisit { text, node, symbols: _, path, locator }: EdgeVisit<'_, '_, '_>,
    out: &mut EdgeEmitter<'_>,
) {
    match node.kind() {
        "import_statement" =>
            for name in identifiers_under(node, text, super::IDENTIFIER_KINDS) {
                out.push(file_edge(path, node, text, name, EdgeKind::Imports));
            },
        "export_statement" =>
            for name in exported_names(node, text) {
                out.push(file_edge(path, node, text, name, EdgeKind::Exports));
            },
        "call_expression" | "new_expression" => {
            let (callee_field, edge_kind) = if node.kind() == "new_expression" {
                ("constructor", EdgeKind::Constructs)
            } else {
                ("function", EdgeKind::CallsName)
            };
            let Some(callee) = node.child_by_field_name(callee_field) else {
                return;
            };
            let identifiers = IdentifierPath::member_chain(callee, text, super::IDENTIFIER_KINDS);
            if let Some(edge) = qualified_call_edge(locator, node, text, &identifiers, edge_kind) {
                out.push(edge);
            }
            if let Some(receiver) = identifiers.receiver_text().map(ToOwned::to_owned) {
                out.push(symbol_edge(
                    locator,
                    node,
                    receiver,
                    EdgeKind::ReferencesType,
                    identifiers.receiver_node().map(CalleeRange::of_node),
                ));
            }
        },
        "jsx_opening_element" | "jsx_self_closing_element" => {
            let Some(tag) = node.child_by_field_name("name") else {
                return;
            };
            // `<Inner.Part />` renders `Part`; `Inner` is only where it lives.
            let identifiers = IdentifierPath::member_chain(tag, text, super::IDENTIFIER_KINDS);
            if let Some(name) = identifiers.last_text().map(ToOwned::to_owned) {
                out.push(symbol_edge_with_context(
                    locator,
                    node,
                    None,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeContext {
                        target_qualified_name: identifiers.qualified_name(),
                        receiver_hint: identifiers.receiver_text().map(ToOwned::to_owned),
                        ..Default::default()
                    },
                    identifiers.last_node().map(CalleeRange::of_node),
                ));
            }
        },
        "type_identifier" => {
            if let Some(name) = node.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned) {
                out.push(symbol_edge(
                    locator,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    // `node` is itself the `type_identifier` token — its range is the callee
                    // range.
                    Some(CalleeRange::of_node(node)),
                ));
            }
        },
        _ => {},
    }
}

/// The names an `export` statement makes visible — never the identifiers inside a declaration's
/// parameters, types or body.
///
/// - `export function f` / `class C` / `const a = …, b = …` → the declared names;
/// - `export { a, b as c }` (with or without `from`) → `a`, `c`: the names the module exports;
/// - `export * as ns from "./x"` → `ns`; `export * from "./x"` → the module specifier `./x`, since
///   the statement re-exports a whole module and names nothing else;
/// - `export default Foo` / `export = Foo` → `Foo`. An anonymous default names nothing.
fn exported_names(node: Node<'_>, text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let declaration = node.child_by_field_name("declaration");
    let value = node.child_by_field_name("value");
    if let Some(declaration) = declaration {
        declared_names(declaration, text, &mut names);
    }
    if let Some(value) = value {
        names.extend(node_text_if_named(value, text));
    }
    for child in named_children(node) {
        match child.kind() {
            "export_clause" =>
                for specifier in named_children(child) {
                    let exported = specifier
                        .child_by_field_name("alias")
                        .or_else(|| specifier.child_by_field_name("name"));
                    names.extend(exported.and_then(|name| node_text_if_named(name, text)));
                },
            "namespace_export" =>
                names.extend(named_children(child).find_map(|name| node_text_if_named(name, text))),
            // `export = Foo` carries its target as an unnamed child.
            "identifier" if value.is_none_or(|value| value.id() != child.id()) =>
                names.extend(node_text_if_named(child, text)),
            _ => {},
        }
    }
    let is_bare_star = names.is_empty() && declaration.is_none() && value.is_none();
    if is_bare_star && let Some(source) = node.child_by_field_name("source") {
        let specifier = node_text(source, text);
        let specifier = specifier.trim_matches(['"', '\'', '`']);
        if !specifier.is_empty() {
            names.push(specifier.to_string());
        }
    }
    names
}

/// The names one exported declaration declares. A destructuring pattern
/// (`export const { a } = o`) declares no single name and is skipped.
fn declared_names(declaration: Node<'_>, text: &str, names: &mut Vec<String>) {
    match declaration.kind() {
        "lexical_declaration" | "variable_declaration" =>
            for declarator in named_children(declaration) {
                if let Some(name) = declarator.child_by_field_name("name") {
                    names.extend(node_text_if_named(name, text));
                }
            },
        // `export declare function f(): void` wraps the declaration once more.
        "ambient_declaration" =>
            for inner in named_children(declaration) {
                // grow_stack: the recursion is shallow in practice, but the guard is uniform for
                // every tree descender (#543).
                rag_rat_base::stack::grow_stack(|| declared_names(inner, text, names));
            },
        _ =>
            if let Some(name) = declaration.child_by_field_name("name") {
                // `export namespace A.B {}` declares `B` inside `A`.
                names.extend(
                    IdentifierPath::member_chain(name, text, super::IDENTIFIER_KINDS)
                        .last_text()
                        .map(ToOwned::to_owned),
                );
            },
    }
}

/// The text of `node` when it is a plain identifier; `None` for any other expression.
fn node_text_if_named(node: Node<'_>, text: &str) -> Option<String> {
    super::IDENTIFIER_KINDS
        .contains(&node.kind())
        .then(|| node_text(node, text))
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
#[path = "edges_tests.rs"]
mod typescript_edge_tests;
