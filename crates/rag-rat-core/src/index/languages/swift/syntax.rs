use tree_sitter::Node;

/// Swift source paths use `.` while the graph's canonical scope paths use `::`. Extract only the
/// lexical path segments, excluding generic arguments, so declarations and edges share one
/// representation (`API::Request`, never `API.Request` or `API::Request::Element`).
pub(crate) fn identifier_nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut identifiers = Vec::new();
    let mut stack = vec![root];
    let mut children = Vec::new();
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "type_arguments" | "type_parameters") {
            continue;
        }
        if matches!(
            node.kind(),
            "identifier"
                | "simple_identifier"
                | "type_identifier"
                | "self_expression"
                | "super_expression"
        ) {
            identifiers.push(node);
            continue;
        }
        let mut cursor = node.walk();
        children.clear();
        children.extend(node.named_children(&mut cursor));
        for &child in children.iter().rev() {
            stack.push(child);
        }
    }
    identifiers
}

pub(crate) fn segments(root: Node<'_>, text: &str) -> Vec<String> {
    identifier_nodes(root)
        .into_iter()
        .filter_map(|node| node.utf8_text(text.as_bytes()).ok().map(str::to_owned))
        .collect()
}

pub(crate) fn canonical_name(segments: &[String]) -> Option<String> {
    (!segments.is_empty()).then(|| segments.join("::"))
}

pub(crate) fn qualified_name(root: Node<'_>, text: &str) -> Option<String> {
    canonical_name(&segments(root, text))
}

pub(crate) fn is_operator_token(kind: &str) -> bool {
    matches!(
        kind,
        "custom_operator"
            | "bang"
            | "!="
            | "!=="
            | "%"
            | "%="
            | "&"
            | "*"
            | "*="
            | "+"
            | "++"
            | "+="
            | "-"
            | "--"
            | "-="
            | "/"
            | "/="
            | "<"
            | "<<"
            | "<="
            | "="
            | "=="
            | "==="
            | ">"
            | ">="
            | ">>"
            | "^"
            | "|"
            | "~"
    )
}
