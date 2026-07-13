//! Kotlin graph-edge extraction for the shared structural edge walk.
use std::path::Path;

use tree_sitter::Node;

use crate::index::edges::extract::*;
use crate::index::edges::*;

pub(super) fn kotlin_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
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
            let identifiers = identifiers_under(node, text);
            // Parallel to `identifiers` (same node, same traversal order) so the callee `.last()`
            // and receiver/constructor `.first()` nodes line up with the string picks (#67).
            let identifier_nodes = identifier_nodes_under(node);
            if let Some(name) =
                identifiers.last().cloned().or_else(|| first_identifier_text(node, text))
            {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: dotted_qualified_name(&identifiers),
                        receiver_hint: identifiers
                            .first()
                            .filter(|_| identifiers.len() > 1)
                            .cloned(),
                    },
                    identifier_nodes.last().copied().map(CalleeRange::of_node),
                ));
            }
            if let Some(receiver) = identifiers.first().filter(|_| identifiers.len() > 1).cloned() {
                out.push(symbol_edge(
                    symbols,
                    node,
                    receiver,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    identifier_nodes
                        .first()
                        .filter(|_| identifier_nodes.len() > 1)
                        .copied()
                        .map(CalleeRange::of_node),
                ));
            }
            if let Some(constructor) =
                identifiers.first().filter(|name| looks_like_type_name(name)).cloned()
            {
                // Both the type reference and the construct point at the constructor — the FIRST
                // identifier (matching `identifiers.first()`).
                let constructor_range = identifier_nodes.first().copied().map(CalleeRange::of_node);
                out.push(symbol_edge(
                    symbols,
                    node,
                    constructor.clone(),
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    constructor_range,
                ));
                out.push(symbol_edge_with_context(
                    symbols,
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
                    symbols,
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
                    symbols,
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
