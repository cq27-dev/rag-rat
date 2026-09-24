//! Kotlin graph-edge extraction for the shared structural edge walk.

use tree_sitter::Node;

use crate::index::edges::*;

pub(in crate::index::languages) fn kotlin_edges(
    EdgeVisit { text, node, symbols: _, path, locator }: EdgeVisit<'_, '_, '_>,
    out: &mut EdgeEmitter<'_>,
) {
    match node.kind() {
        "import" => kotlin_import_edge(text, node, path, out),
        "call_expression" => kotlin_call_edges(text, node, locator, out),
        "user_type" =>
            if let Some(name) = user_type_name(node) {
                out.push(symbol_edge(
                    locator,
                    node,
                    node_text(name, text),
                    EdgeKind::ReferencesType,
                    Some(CalleeRange::of_node(name)),
                ));
            },
        "delegation_specifier" =>
            if let Some(name) = delegated_type(node).and_then(user_type_name) {
                out.push(symbol_edge(
                    locator,
                    node,
                    node_text(name, text),
                    EdgeKind::Implements,
                    Some(CalleeRange::of_node(name)),
                ));
            },
        _ => {},
    }
}

/// `import a.b.C` (or `import a.b.C as D`, `import a.b.*`) — ONE Imports edge naming the last
/// segment, with the whole written path as its qualified target. The alias is a local binding, not
/// the import.
fn kotlin_import_edge(
    text: &str,
    node: Node<'_>,
    path: &std::path::Path,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(imported) = named_children(node).find(|child| child.kind() == "qualified_identifier")
    else {
        return;
    };
    let identifiers = IdentifierPath::member_chain(imported, text, super::IDENTIFIER_KINDS);
    let Some(name) = identifiers.last_text().map(ToOwned::to_owned) else {
        return;
    };
    let mut edge = file_edge(path, node, text, name, EdgeKind::Imports);
    edge.target_qualified_name = identifiers.qualified_name();
    out.push(edge);
}

/// A call's callee is the call's FIRST child (kotlin-ng gives it no field); the value arguments
/// and a trailing lambda follow it and are never part of the callee. `a.b.c(x)` calls `c` on `a.b`.
fn kotlin_call_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(callee) = node.named_child(0) else {
        return;
    };
    let identifiers = IdentifierPath::member_chain(callee, text, super::IDENTIFIER_KINDS);
    let Some(edge) = qualified_call_edge(locator, node, text, &identifiers, EdgeKind::CallsName)
    else {
        return;
    };
    out.push(edge);
    if let Some(receiver) = identifiers.receiver_text().map(ToOwned::to_owned) {
        out.push(symbol_edge(
            locator,
            node,
            receiver,
            EdgeKind::ReferencesType,
            identifiers.receiver_node().map(CalleeRange::of_node),
        ));
    }
    // Kotlin has no `new`: a call whose CALLEE is type-shaped constructs it (`Foo(1)`,
    // `Outer.Inner()`). A type-shaped receiver does not — `Result.success(1)` calls a companion
    // member of `Result`.
    let Some(constructor) = identifiers.last_text().filter(|name| looks_like_type_name(name))
    else {
        return;
    };
    let constructor_range = identifiers.last_node().map(CalleeRange::of_node);
    out.push(symbol_edge(
        locator,
        node,
        constructor.to_owned(),
        EdgeKind::ReferencesType,
        constructor_range,
    ));
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        constructor.to_owned(),
        EdgeKind::Constructs,
        EdgeContext { target_qualified_name: identifiers.qualified_name(), ..Default::default() },
        constructor_range,
    ));
}

/// The name a `user_type` refers to: its LAST direct identifier (`a.b.C<T>` names `C`). The type
/// arguments are `user_type`s of their own and get their own edges.
fn user_type_name(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node).filter(|child| child.kind() == "identifier").last()
}

/// The supertype a delegation specifier names: `Base`, `Base(args)` or `Base by delegate` — never
/// the constructor arguments or the delegate expression.
fn delegated_type(node: Node<'_>) -> Option<Node<'_>> {
    let specifier = node.named_child(0)?;
    match specifier.kind() {
        "user_type" => Some(specifier),
        "constructor_invocation" | "explicit_delegation" =>
            named_children(specifier).find(|child| child.kind() == "user_type"),
        _ => None,
    }
}

#[cfg(test)]
#[path = "edges_tests.rs"]
mod kotlin_edge_tests;
