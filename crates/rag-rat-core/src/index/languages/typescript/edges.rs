//! TypeScript graph-edge extraction for the shared structural edge walk.

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
            for name in identifiers_under(node, text, super::IDENTIFIER_KINDS) {
                out.push(file_edge(path, node, text, name, EdgeKind::Exports));
            },
        "call_expression" | "new_expression" => {
            let function = node.child_by_field_name("function").unwrap_or(node);
            let identifiers = IdentifierPath::under(function, text, super::IDENTIFIER_KINDS);
            let edge_kind = if node.kind() == "new_expression" {
                EdgeKind::Constructs
            } else {
                EdgeKind::CallsName
            };
            if let Some(edge) = qualified_call_edge(
                locator,
                node,
                text,
                &identifiers,
                super::IDENTIFIER_KINDS,
                edge_kind,
            ) {
                out.push(edge);
            }
            if let Some(receiver) = identifiers.receiver_text().map(ToOwned::to_owned) {
                out.push(symbol_edge(
                    locator,
                    node,
                    receiver,
                    EdgeKind::ReferencesType,
                    // The type is the receiver — the FIRST segment, matching
                    // `identifiers.first()`.
                    identifiers.receiver_node().map(CalleeRange::of_node),
                ));
            }
        },
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name) = first_identifier_text(node, text, super::IDENTIFIER_KINDS) {
                out.push(symbol_edge(
                    locator,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    first_identifier_node(node, super::IDENTIFIER_KINDS).map(CalleeRange::of_node),
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
