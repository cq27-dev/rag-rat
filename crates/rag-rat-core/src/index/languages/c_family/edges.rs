//! C and C++ graph-edge extraction for the shared structural edge walk.

use crate::index::edges::*;

pub(in crate::index::languages) fn c_like_edges(
    EdgeVisit { text, node, symbols: _, path, locator }: EdgeVisit<'_, '_, '_>,
    out: &mut EdgeEmitter<'_>,
) {
    match node.kind() {
        "preproc_include" => {
            let include = node_text(node, text)
                .trim()
                .trim_start_matches("#include")
                .trim()
                .trim_matches(['<', '>', '"'])
                .to_string();
            if !include.is_empty() {
                out.push(file_edge(path, node, text, include, EdgeKind::Imports));
            }
        },
        "call_expression" => {
            let Some(function) = node.child_by_field_name("function") else {
                return;
            };
            let identifiers = IdentifierPath::member_chain(function, text, super::IDENTIFIER_KINDS);
            if let Some(edge) =
                qualified_call_edge(locator, node, text, &identifiers, EdgeKind::CallsName)
            {
                out.push(edge);
            }
        },
        "type_identifier" | "qualified_identifier" | "namespace_identifier" => {
            // Read the name along the grammar's `scope`/`name` fields, never from the whole
            // subtree: a template argument (`ns::Thing<Item>`) is not the name it qualifies.
            let identifiers = IdentifierPath::member_chain(node, text, super::IDENTIFIER_KINDS);
            let Some(name) = identifiers.last_text() else {
                return;
            };
            out.push(symbol_edge_with_context(
                locator,
                node,
                None,
                name.to_owned(),
                EdgeKind::ReferencesType,
                EdgeContext {
                    target_qualified_name: identifiers.qualified_name(),
                    ..Default::default()
                },
                identifiers.last_node().map(CalleeRange::of_node),
            ));
        },
        _ => {},
    }
}

#[cfg(test)]
#[path = "edges_tests.rs"]
mod c_family_edge_tests;
