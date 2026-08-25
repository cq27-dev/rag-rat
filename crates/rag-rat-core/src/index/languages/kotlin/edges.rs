//! Kotlin graph-edge extraction for the shared structural edge walk.
use crate::index::edges::*;

pub(in crate::index::languages) fn kotlin_edges(
    EdgeVisit { text, node, symbols: _, path, locator }: EdgeVisit<'_, '_, '_>,
    emit: &mut EdgeEmitter<'_>,
) {
    let out = emit;
    match node.kind() {
        "import" | "import_header" | "import_directive" => {
            for name in identifiers_under(node, text) {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    name,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "call_expression" => {
            let identifiers = IdentifierPath::under(node, text);
            if let Some(name) = identifiers
                .last_text()
                .map(ToOwned::to_owned)
                .or_else(|| first_identifier_text(node, text))
            {
                out.push(symbol_edge_with_context(
                    locator,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: identifiers.qualified_name(),
                        receiver_hint: identifiers
                            .first_text()
                            .filter(|_| identifiers.len() > 1)
                            .map(ToOwned::to_owned),
                        ..Default::default()
                    },
                    identifiers.last_node().map(CalleeRange::of_node),
                ));
            }
            if let Some(receiver) =
                identifiers.first_text().filter(|_| identifiers.len() > 1).map(ToOwned::to_owned)
            {
                out.push(symbol_edge(
                    locator,
                    node,
                    receiver,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    identifiers
                        .first_node()
                        .filter(|_| identifiers.len() > 1)
                        .map(CalleeRange::of_node),
                ));
            }
            if let Some(constructor) = identifiers
                .first_text()
                .filter(|name| looks_like_type_name(name))
                .map(ToOwned::to_owned)
            {
                // Both the type reference and the construct point at the constructor — the FIRST
                // identifier (matching `identifiers.first()`).
                let constructor_range = identifiers.first_node().map(CalleeRange::of_node);
                out.push(symbol_edge(
                    locator,
                    node,
                    constructor.clone(),
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    constructor_range,
                ));
                out.push(symbol_edge_with_context(
                    locator,
                    node,
                    text,
                    constructor,
                    EdgeKind::Constructs,
                    EdgeConfidence::NameOnly,
                    EdgeContext::default(),
                    constructor_range,
                ));
            }
        },
        "user_type" | "type_identifier" =>
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    locator,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    last_identifier_node(node).map(final_segment_node).map(CalleeRange::of_node),
                ));
            },
        "delegation_specifier" | "supertype" | "super_type" => {
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    locator,
                    node,
                    name,
                    EdgeKind::Implements,
                    EdgeConfidence::NameOnly,
                    last_identifier_node(node).map(final_segment_node).map(CalleeRange::of_node),
                ));
            }
        },
        _ => {},
    }
}
