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
            let function = node.child_by_field_name("function").unwrap_or(node);
            let identifiers = IdentifierPath::under(function, text, super::IDENTIFIER_KINDS);
            if let Some(edge) = qualified_call_edge(
                locator,
                node,
                text,
                &identifiers,
                super::IDENTIFIER_KINDS,
                EdgeKind::CallsName,
            ) {
                out.push(edge);
            }
        },
        "type_identifier" | "qualified_identifier" | "namespace_identifier" => {
            if let Some(name) = last_identifier_text(node, text, super::IDENTIFIER_KINDS) {
                out.push(symbol_edge(
                    locator,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    last_identifier_node(node, super::IDENTIFIER_KINDS)
                        .map(final_segment_node)
                        .map(CalleeRange::of_node),
                ));
            }
        },
        _ => {},
    }
}
