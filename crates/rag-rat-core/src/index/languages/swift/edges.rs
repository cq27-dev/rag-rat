//! Swift graph-edge extraction for the shared structural edge walk.

use std::path::Path;

use rag_rat_db::EdgeConfidence;
use tree_sitter::Node;

use super::syntax;
use crate::index::edges::*;

pub(in crate::index::languages) fn swift_edges(
    EdgeVisit { text, node, symbols, path, locator }: EdgeVisit<'_, '_, '_>,
    out: &mut EdgeEmitter<'_>,
) {
    match node.kind() {
        "source_file"
            if text.contains(',')
                && (text.contains("higherThan:") || text.contains("lowerThan:")) =>
            swift_precedence_group_relation_list_edges(text, node, symbols, out),
        "import_declaration" => swift_import_edges(text, node, path, out),
        "call_expression" => swift_call_edges(text, node, locator, out),
        "constructor_expression" => swift_constructor_edges(text, node, locator, out),
        "macro_invocation" => swift_macro_edges(text, node, locator, out),
        "operator_declaration" => swift_precedence_group_edges(text, node, locator, out),
        "precedence_group_attribute" =>
            swift_precedence_group_relation_edges(text, node, locator, out),
        "postfix_expression"
        | "prefix_expression"
        | "multiplicative_expression"
        | "additive_expression"
        | "range_expression"
        | "infix_expression"
        | "comparison_expression"
        | "equality_expression"
        | "conjunction_expression"
        | "disjunction_expression"
        | "bitwise_operation" => swift_operator_or_shorthand_case_edges(text, node, locator, out),
        "navigation_expression" => swift_qualified_case_edges(text, node, locator, out),
        "attribute" if !swift_node_is_import_modifier(node) =>
            swift_attribute_macro_edges(text, node, locator, out),
        "inheritance_specifier" => swift_inheritance_edges(text, node, locator, out),
        "user_type"
            if !swift_node_is_declaration_name(node)
                && !swift_node_is_import_modifier(node)
                && !swift_has_ancestor_kind(node, "attribute")
                && !swift_node_is_type_parameter_reference(node, text) =>
            swift_user_type_edges(text, node, locator, out),
        "type_identifier"
            if node.parent().is_none_or(|parent| parent.kind() != "user_type")
                && !swift_node_is_declaration_name(node)
                && !swift_node_is_type_parameter_declaration(node)
                && !swift_node_is_type_parameter_reference(node, text)
                && !swift_has_ancestor_kind(node, "attribute")
                && !swift_node_is_import_modifier(node) =>
            swift_type_identifier_edges(text, node, locator, out),
        _ => {},
    }
}

fn swift_import_edges(text: &str, node: Node<'_>, path: &Path, out: &mut EdgeEmitter<'_>) {
    let identifiers = swift_import_identifiers(node, text);
    if !identifiers.is_empty() {
        out.push(file_edge(path, node, text, identifiers.join("::"), EdgeKind::Imports));
    }
}

fn swift_inheritance_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(type_path) = swift_inherited_type_name(node) else {
        return;
    };
    let identifier_nodes = syntax::identifier_nodes(type_path);
    let identifiers =
        identifier_nodes.iter().map(|&identifier| node_text(identifier, text)).collect::<Vec<_>>();
    let Some(name) = identifiers.last().cloned() else {
        return;
    };
    let edge_kind = if swift_inheritance_is_enum_raw_type(node, text) {
        EdgeKind::ReferencesType
    } else {
        EdgeKind::Implements
    };
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        name,
        edge_kind,
        swift_edge_context(&identifiers),
        identifier_nodes.last().copied().map(CalleeRange::of_node),
    ));
}

fn swift_user_type_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let identifier_nodes = syntax::identifier_nodes(node);
    let Some(type_node) = identifier_nodes.last().copied() else {
        return;
    };
    let identifiers =
        identifier_nodes.iter().map(|&identifier| node_text(identifier, text)).collect::<Vec<_>>();
    let Some(name) = identifiers.last().cloned() else {
        return;
    };
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        name,
        EdgeKind::ReferencesType,
        swift_edge_context(&identifiers),
        Some(CalleeRange::of_node(type_node)),
    ));
}

fn swift_type_identifier_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let name = node_text(node, text);
    out.push(symbol_edge(
        locator,
        node,
        name,
        EdgeKind::ReferencesType,
        Some(CalleeRange::of_node(node)),
    ));
}

fn swift_operator_or_shorthand_case_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(operation) =
        node.child_by_field_name("op").or_else(|| node.child_by_field_name("operation"))
    else {
        return;
    };
    if operation.kind() == "." {
        let Some(case_name) = node.child_by_field_name("target") else {
            return;
        };
        out.push(symbol_edge(
            locator,
            node,
            node_text(case_name, text),
            EdgeKind::CallsName,
            Some(CalleeRange::of_node(case_name)),
        ));
        return;
    }
    if node.kind() == "postfix_expression" && node_text(operation, text) == "!" {
        // Swift reserves postfix `!` for optional force-unwrapping. Other postfix operator
        // tokens remain callable, but this language construct must not bind to an overload.
        return;
    }
    if !syntax::is_operator_token(operation.kind()) {
        return;
    }
    out.push(symbol_edge(
        locator,
        node,
        node_text(operation, text),
        EdgeKind::UsesOperator,
        Some(CalleeRange::of_node(operation)),
    ));
    out.push(symbol_edge(
        locator,
        node,
        node_text(operation, text),
        EdgeKind::CallsName,
        Some(CalleeRange::of_node(operation)),
    ));
}

fn swift_qualified_case_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    if node.parent().is_some_and(|parent| {
        parent.kind() == "call_expression" && swift_call_target(parent) == Some(node)
    }) {
        return;
    }
    let identifier_nodes = syntax::identifier_nodes(node);
    let identifiers =
        identifier_nodes.iter().map(|&identifier| node_text(identifier, text)).collect::<Vec<_>>();
    if identifiers.len() < 2 || !looks_like_type_name(&identifiers[0]) {
        return;
    }
    let Some(case_name) = identifier_nodes.last().copied() else {
        return;
    };
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        node_text(case_name, text),
        EdgeKind::CallsName,
        swift_edge_context(&identifiers),
        Some(CalleeRange::of_node(case_name)),
    ));
}

fn swift_call_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    if swift_subscript_suffix(node, text).is_some() {
        let Some(target) = swift_call_target(node).map(swift_callee_operand) else {
            return;
        };
        let Some((mut identifiers, _)) = swift_call_target_parts(target, text) else {
            return;
        };
        if identifiers.len() == 1
            && swift_name_is_type_parameter_in_scope(&identifiers[0], target, text)
        {
            return;
        }
        identifiers.push("subscript".to_string());
        emit_swift_call_edges(text, node, locator, out, identifiers, None, false);
        return;
    }
    let Some(target) = swift_call_target(node).map(swift_callee_operand) else {
        return;
    };
    let Some((identifiers, callee_range)) = swift_call_target_parts(target, text) else {
        return;
    };
    if identifiers.len() == 1
        && swift_name_is_type_parameter_in_scope(&identifiers[0], target, text)
    {
        return;
    }
    let constructs = identifiers.last().is_some_and(|name| looks_like_type_name(name));
    emit_swift_call_edges(text, node, locator, out, identifiers, callee_range, constructs);
}

fn swift_constructor_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(constructed_type) = node.child_by_field_name("constructed_type") else {
        return;
    };
    let Some((identifiers, callee_range)) = swift_call_target_parts(constructed_type, text) else {
        return;
    };
    if identifiers.len() == 1
        && swift_name_is_type_parameter_in_scope(&identifiers[0], constructed_type, text)
    {
        return;
    }
    emit_swift_call_edges(text, node, locator, out, identifiers, callee_range, true);
}

fn emit_swift_call_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
    identifiers: Vec<String>,
    callee_range: Option<CalleeRange>,
    constructs: bool,
) {
    let Some(name) = identifiers.last().cloned() else {
        return;
    };
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        name.clone(),
        if constructs { EdgeKind::Constructs } else { EdgeKind::CallsName },
        if constructs {
            swift_edge_context(&identifiers)
        } else {
            swift_call_edge_context(&identifiers)
        },
        callee_range,
    ));
    if constructs {
        out.push(symbol_edge_with_context(
            locator,
            node,
            Some(text),
            name,
            EdgeKind::ReferencesType,
            swift_edge_context(&identifiers),
            callee_range,
        ));
    }
}

fn swift_macro_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(name_node) = syntax::identifier_nodes(node).first().copied() else {
        return;
    };
    if node_text(name_node, text) == "externalMacro"
        && swift_has_ancestor_kind(node, "macro_declaration")
    {
        return;
    }
    out.push(symbol_edge(
        locator,
        node,
        node_text(name_node, text),
        EdgeKind::UsesMacro,
        Some(CalleeRange::of_node(name_node)),
    ));
}

fn swift_has_ancestor_kind(node: Node<'_>, kind: &str) -> bool {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if current.kind() == kind {
            return true;
        }
        ancestor = current.parent();
    }
    false
}

fn swift_attribute_macro_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(attribute_name) = node.named_child(0) else {
        return;
    };
    let identifier_nodes = syntax::identifier_nodes(attribute_name);
    let identifiers =
        identifier_nodes.iter().map(|&identifier| node_text(identifier, text)).collect::<Vec<_>>();
    let Some(name_node) = identifier_nodes.last().copied() else {
        return;
    };
    let Some(name) = identifiers.last().cloned() else {
        return;
    };
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        name.clone(),
        EdgeKind::UsesMacro,
        swift_edge_context(&identifiers),
        Some(CalleeRange::of_node(name_node)),
    ));
    // The grammar uses the same attribute shape for macros, property wrappers, and result
    // builders. Emit both semantic possibilities; language-policy resolution keeps only the one
    // whose declaration kind exists and suppresses both candidates when the attribute is external.
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        name,
        EdgeKind::ReferencesType,
        swift_edge_context(&identifiers),
        Some(CalleeRange::of_node(name_node)),
    ));
}

fn swift_precedence_group_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(group) = syntax::identifier_nodes(node).last().copied() else {
        return;
    };
    out.push(symbol_edge(
        locator,
        node,
        node_text(group, text),
        EdgeKind::UsesPrecedenceGroup,
        Some(CalleeRange::of_node(group)),
    ));
}

fn swift_precedence_group_relation_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let identifiers = syntax::identifier_nodes(node);
    let Some((relation, dependencies)) = identifiers.split_first() else {
        return;
    };
    if dependencies.is_empty()
        || !matches!(node_text(*relation, text).as_str(), "higherThan" | "lowerThan")
    {
        return;
    }
    for dependency in dependencies {
        let edge = symbol_edge(
            locator,
            node,
            node_text(*dependency, text),
            EdgeKind::UsesPrecedenceGroup,
            Some(CalleeRange::of_node(*dependency)),
        );
        if !out.iter().any(|existing| {
            existing.edge_kind == edge.edge_kind
                && existing.to_name == edge.to_name
                && existing.callee_span.is_some_and(|span| {
                    edge.callee_span.is_some_and(|candidate| {
                        span.start_byte == candidate.start_byte
                            && span.end_byte == candidate.end_byte
                    })
                })
        }) {
            out.push(edge);
        }
    }
}

/// tree-sitter-swift 0.7 models one precedence dependency per attribute even though Swift accepts
/// comma-separated lists. Recover relation lists from comment/string-masked declaration bodies;
/// single dependencies still deduplicate against the structured attribute-node path above.
fn swift_precedence_group_relation_list_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut EdgeEmitter<'_>,
) {
    let source = node_text(node, text);
    let code = swift_code_without_comments_or_strings(&source);
    let mut declaration_cursor = 0;
    while let Some(relative) = code[declaration_cursor..].find("precedencegroup") {
        let declaration_start = declaration_cursor + relative;
        let keyword_end = declaration_start + "precedencegroup".len();
        let has_identifier_prefix =
            code[..declaration_start].chars().next_back().is_some_and(is_swift_identifier_char);
        let has_required_separator =
            code.as_bytes().get(keyword_end).is_some_and(u8::is_ascii_whitespace);
        if has_identifier_prefix || !has_required_separator {
            declaration_cursor = keyword_end;
            continue;
        }
        let name_start = keyword_end;
        let name_start =
            name_start + code[name_start..].bytes().take_while(u8::is_ascii_whitespace).count();
        let name_len = swift_identifier_len(&code[name_start..]);
        let after_name = name_start + name_len;
        let open =
            after_name + code[after_name..].bytes().take_while(u8::is_ascii_whitespace).count();
        if name_len == 0 || code.as_bytes().get(open) != Some(&b'{') {
            declaration_cursor = keyword_end;
            continue;
        }
        let mut depth = 1_usize;
        let mut close = open + 1;
        for (offset, byte) in code[open + 1..].bytes().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + 1 + offset;
                        break;
                    }
                },
                _ => {},
            }
        }
        if depth != 0 {
            break;
        }
        let group = &source[name_start..name_start + name_len];
        let source_symbol =
            symbols.iter().find(|symbol| symbol.kind == "precedence_group" && symbol.name == group);
        swift_recovered_precedence_relations(
            text,
            node.start_byte(),
            &source,
            &code,
            open + 1,
            close,
            source_symbol,
            out,
        );
        declaration_cursor = close + 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn swift_recovered_precedence_relations(
    text: &str,
    source_offset: usize,
    source: &str,
    code: &str,
    body_start: usize,
    body_end: usize,
    source_symbol: Option<&IndexedSymbol>,
    out: &mut EdgeEmitter<'_>,
) {
    let mut cursor = body_start;
    while cursor < body_end {
        let relation = ["higherThan:", "lowerThan:"]
            .into_iter()
            .filter_map(|label| code[cursor..body_end].find(label).map(|offset| (label, offset)))
            .filter(|(_, offset)| {
                let start = cursor + *offset;
                start == body_start
                    || !code[..start].chars().next_back().is_some_and(is_swift_identifier_char)
            })
            .min_by_key(|(_, offset)| *offset);
        let Some((label, relative)) = relation else {
            break;
        };
        let relation_start = cursor + relative;
        let mut dependency_cursor = relation_start + label.len();
        let mut relation_end = dependency_cursor;
        let mut dependencies = Vec::new();
        loop {
            dependency_cursor += code[dependency_cursor..body_end]
                .bytes()
                .take_while(u8::is_ascii_whitespace)
                .count();
            let name_len = swift_identifier_len(&code[dependency_cursor..body_end]);
            if name_len == 0 {
                break;
            }
            let name = &source[dependency_cursor..dependency_cursor + name_len];
            let start_byte = source_offset + dependency_cursor;
            relation_end = dependency_cursor + name_len;
            dependencies.push((name.to_string(), start_byte, name_len));
            dependency_cursor = relation_end;
            dependency_cursor += code[dependency_cursor..body_end]
                .bytes()
                .take_while(u8::is_ascii_whitespace)
                .count();
            if code.as_bytes().get(dependency_cursor) != Some(&b',') {
                break;
            }
            dependency_cursor += 1;
        }
        let source_start = source_offset + relation_start;
        let source_end = source_offset + relation_end;
        let start_line =
            i64::try_from(text[..source_start].bytes().filter(|&b| b == b'\n').count())
                .unwrap_or(i64::MAX)
                + 1;
        let end_line = i64::try_from(text[..source_end].bytes().filter(|&b| b == b'\n').count())
            .unwrap_or(i64::MAX)
            + 1;
        let evidence = source[relation_start..relation_end].trim().to_string();
        let source_span = EdgeSpan {
            start_line,
            end_line,
            start_byte: i64::try_from(source_start).unwrap_or(i64::MAX),
            end_byte: i64::try_from(source_end).unwrap_or(i64::MAX),
        };
        for (name, start_byte, name_len) in dependencies {
            let edge = EdgeCandidate {
                from_symbol_id: source_symbol.map(|symbol| symbol.id),
                from_name: source_symbol.map(|symbol| symbol.qualified_name.clone()),
                to_name: name,
                target_qualified_name: None,
                evidence: Some(evidence.clone()),
                receiver_hint: None,
                receiver_type_hint: None,
                source_span,
                callee_span: Some(CalleeRange { start_byte, end_byte: start_byte + name_len }),
                import_scope: None,
                edge_kind: EdgeKind::UsesPrecedenceGroup,
                confidence: EdgeConfidence::NameOnly,
            };
            if !out.iter().any(|existing| {
                existing.edge_kind == edge.edge_kind
                    && existing.to_name == edge.to_name
                    && existing.callee_span.is_some_and(|span| span.start_byte == start_byte)
            }) {
                out.push(edge);
            }
        }
        cursor = relation_end.max(relation_start + label.len());
    }
}

fn swift_identifier_len(text: &str) -> usize {
    if let Some(rest) = text.strip_prefix('`') {
        return rest.find('`').map_or(0, |end| end + 2);
    }
    text.char_indices()
        .take_while(|(_, character)| is_swift_identifier_char(*character))
        .map(|(offset, character)| offset + character.len_utf8())
        .last()
        .unwrap_or(0)
}

fn is_swift_identifier_char(character: char) -> bool {
    character == '_' || character.is_alphanumeric() || !character.is_ascii()
}

fn swift_code_without_comments_or_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = bytes.to_vec();
    let mut cursor = 0;
    let mut block_depth = 0_u32;
    let mut string_delimiter: Option<(usize, bool)> = None;
    while cursor < bytes.len() {
        if let Some((hashes, multiline)) = string_delimiter {
            let quote_count = if multiline { 3 } else { 1 };
            let terminator_len = quote_count + hashes;
            let is_terminator = bytes
                .get(cursor..cursor + quote_count)
                .is_some_and(|quotes| quotes.iter().all(|&byte| byte == b'"'))
                && bytes
                    .get(cursor + quote_count..cursor + terminator_len)
                    .is_some_and(|suffix| suffix.iter().all(|&byte| byte == b'#'));
            if is_terminator {
                for byte in &mut code[cursor..cursor + terminator_len] {
                    *byte = b' ';
                }
                cursor += terminator_len;
                string_delimiter = None;
                continue;
            }
            if hashes == 0 && bytes[cursor] == b'\\' {
                code[cursor] = b' ';
                if cursor + 1 < bytes.len() {
                    code[cursor + 1] = b' ';
                }
                cursor += 2;
                continue;
            }
            if bytes[cursor] != b'\n' {
                code[cursor] = b' ';
            }
            cursor += 1;
            continue;
        }
        if block_depth > 0 {
            if bytes[cursor..].starts_with(b"/*") {
                block_depth += 1;
                code[cursor] = b' ';
                code[cursor + 1] = b' ';
                cursor += 2;
            } else if bytes[cursor..].starts_with(b"*/") {
                block_depth -= 1;
                code[cursor] = b' ';
                code[cursor + 1] = b' ';
                cursor += 2;
            } else {
                if bytes[cursor] != b'\n' {
                    code[cursor] = b' ';
                }
                cursor += 1;
            }
            continue;
        }
        if bytes[cursor..].starts_with(b"//") {
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                code[cursor] = b' ';
                cursor += 1;
            }
        } else if bytes[cursor..].starts_with(b"/*") {
            block_depth = 1;
            code[cursor] = b' ';
            code[cursor + 1] = b' ';
            cursor += 2;
        } else if bytes[cursor] == b'"' || bytes[cursor] == b'#' {
            let hashes = bytes[cursor..].iter().take_while(|&&byte| byte == b'#').count();
            let quote = cursor + hashes;
            if bytes.get(quote) == Some(&b'"') {
                let multiline = bytes[quote..].starts_with(b"\"\"\"");
                let opener_len = hashes + if multiline { 3 } else { 1 };
                for byte in &mut code[cursor..cursor + opener_len] {
                    *byte = b' ';
                }
                cursor += opener_len;
                string_delimiter = Some((hashes, multiline));
            } else {
                cursor += 1;
            }
        } else {
            cursor += 1;
        }
    }
    String::from_utf8(code).expect("masking source bytes preserves UTF-8")
}

fn swift_edge_context(identifiers: &[String]) -> EdgeContext {
    EdgeContext {
        target_qualified_name: (identifiers.len() > 1)
            .then(|| syntax::canonical_name(identifiers))
            .flatten(),
        receiver_hint: identifiers.first().filter(|_| identifiers.len() > 1).cloned(),
        ..Default::default()
    }
}

fn swift_call_edge_context(identifiers: &[String]) -> EdgeContext {
    let mut context = swift_edge_context(identifiers);
    if identifiers.first().is_some_and(|receiver| {
        identifiers.len() > 1
            && !looks_like_type_name(receiver)
            && !super::is_local_qualified_root(receiver)
    }) {
        // A value receiver is not a lexical symbol scope: `client::fetch` can never match the
        // method declared under its nominal type. Keep the receiver hint, but resolve by callee.
        context.target_qualified_name = None;
    }
    context
}

fn swift_call_target(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node).find(|child| child.kind() != "call_suffix")
}

/// Swift binary-operator expressions that can end up holding a call's CALLEE.
const BINARY_OPERATOR_EXPRESSIONS: &[&str] = &[
    "additive_expression",
    "bitwise_operation",
    "comparison_expression",
    "conjunction_expression",
    "disjunction_expression",
    "equality_expression",
    "infix_expression",
    "multiplicative_expression",
    "nil_coalescing_expression",
    "range_expression",
];

/// The real callee of a call whose TARGET is a binary-operator expression.
///
/// tree-sitter-swift binds the argument list to the WHOLE expression on the operator's left, so
/// `p + g()` parses as `call_expression(additive_expression(p + g), call_suffix(()))` — the call's
/// target is the OPERATOR node, not `g`. Swift evaluates that as `p + (g())`, so the callee is the
/// operator expression's RIGHTMOST operand.
///
/// Without this unwrap `swift_call_target_parts` refuses the operator node (it is not a callable
/// static path) and the call is DROPPED — silently losing every call that appears on the right of a
/// binary operator: `total + item.price()`, `x == compute()`, `value ?? fallback()`. That is a
/// large share of real call sites, and nothing about the missing edge is visible downstream: the
/// call just never enters the graph. Nested operators (`a + b + g()`) unwrap repeatedly.
fn swift_callee_operand(target: Node<'_>) -> Node<'_> {
    let mut current = target;
    while BINARY_OPERATOR_EXPRESSIONS.contains(&current.kind()) {
        let Some(rightmost) = named_children(current).last() else {
            break;
        };
        current = rightmost;
    }
    current
}

/// The callee path for a call whose TARGET carries a binary operator on its left spine.
/// `total + item.price()` parses as `call_expression(navigation_expression(additive(total + item),
/// price), ())`, and `base + Module.Client.make()` nests one level deeper —
/// `navigation(navigation(additive(base + Module), Client), make)`. In both, the target itself
/// contains the operator, so `swift_callable_is_static_path` refuses it and the whole call is
/// dropped — losing the single most common shape there is: a method call added to something.
///
/// [`swift_operator_stripped_path`] peels the navigation layers off the target, strips the
/// operator's LEFT operand (Swift evaluates `base + X.y()` as `base + (X.y())`), and returns the
/// ordered identifier nodes of the real call path (`item.price`, `Module.Client.make`). Those run
/// through the SAME [`swift_normalize_call_path`] as the plain arm, so `base + Service.init()`
/// collapses to a construction of `Service` exactly as `Service.init()` does.
///
/// `None` when there is no operator on the target's left spine (ordinary paths keep their existing
/// naming), or when the receiver is DYNAMIC (`base + foo().bar()`, `x + a[0].m()`) — a value
/// receiver with no nameable path, which the baseline drops with or without the operator.
fn swift_operator_receiver_call_parts(
    target: Node<'_>,
    text: &str,
) -> Option<(Vec<String>, Option<CalleeRange>)> {
    let identifier_nodes = swift_operator_stripped_path(target)?;
    swift_normalize_call_path(identifier_nodes, text)
}

/// The ordered identifier nodes of a call target with a binary operator on its left spine, minus
/// the operator's left operand — or `None` when the spine holds no operator, or a dynamic (call /
/// subscript) receiver that is not a nameable static path.
///
/// Recurses down the leftmost `navigation_expression` chain, appending each suffix, until it
/// reaches the operator; the operator's RIGHTMOST operand is where the real path begins. A base
/// case that is neither a navigation nor an operator (a bare identifier, a call, a subscript)
/// yields `None`, so an operator-free path and a dynamic receiver both fall through to the normal
/// arm.
fn swift_operator_stripped_path(target: Node<'_>) -> Option<Vec<Node<'_>>> {
    if BINARY_OPERATOR_EXPRESSIONS.contains(&target.kind()) {
        let operand = swift_callee_operand(target);
        // `base + foo().bar` bottoms out here with a dynamic operand — not a qualifiable path.
        return swift_callable_is_static_path(operand).then(|| syntax::identifier_nodes(operand));
    }
    // A force-unwrap is a transparent receiver wrapper. tree-sitter left-associates the operator
    // into it — `base + obj!.method()` parses as `navigation(postfix(additive(base + obj), !),
    // method)` — so descend into the unwrapped target to reach the operator. Matches the
    // `swift_callable_is_static_path` handling so the operator and plain forms agree.
    if swift_postfix_is_force_unwrap(target) {
        let inner = target.child_by_field_name("target")?;
        return rag_rat_base::stack::grow_stack(|| swift_operator_stripped_path(inner));
    }
    if target.kind() == "navigation_expression" {
        let children = named_children(target).collect::<Vec<_>>();
        let (&receiver, &suffix) = (children.first()?, children.last()?);
        // A pathological left-leaning navigation chain (`a.b.c.d…` thousands deep) recurses to full
        // depth; grow the stack rather than overflow the indexer on hostile input.
        let mut path = rag_rat_base::stack::grow_stack(|| swift_operator_stripped_path(receiver))?;
        path.push(syntax::identifier_nodes(suffix).last().copied()?);
        return Some(path);
    }
    None
}

fn swift_call_target_parts(
    target: Node<'_>,
    text: &str,
) -> Option<(Vec<String>, Option<CalleeRange>)> {
    if let Some(parts) = swift_operator_receiver_call_parts(target, text) {
        return Some(parts);
    }
    match target.kind() {
        "array_type" | "array_literal" =>
            Some((vec!["Array".to_string()], Some(CalleeRange::of_node(target)))),
        "dictionary_type" | "dictionary_literal" =>
            Some((vec!["Dictionary".to_string()], Some(CalleeRange::of_node(target)))),
        _ => {
            if !swift_callable_is_static_path(target) {
                return None;
            }
            swift_normalize_call_path(syntax::identifier_nodes(target), text)
        },
    }
}

/// Turn an ordered call-path identifier list into `(names, callee range)`, applying the
/// `Type.init` → construction collapse: a trailing `init` preceded by a type name is dropped so the
/// caller emits a `Constructs`/type edge to the TYPE, not a `calls_name` to `init`.
///
/// Shared by the plain static-path arm and the operator-RHS recovery so both treat `init` the same
/// way. Extracting it is the fix for #650: the operator path hand-rolled its own identifier
/// collection and skipped this, so `base + Service.init()` emitted `calls_name → init` instead of
/// `Constructs → Service`.
fn swift_normalize_call_path(
    mut identifier_nodes: Vec<Node<'_>>,
    text: &str,
) -> Option<(Vec<String>, Option<CalleeRange>)> {
    if identifier_nodes.is_empty() {
        return None;
    }
    if identifier_nodes.len() > 1
        && identifier_nodes.last().is_some_and(|&node| node_text(node, text) == "init")
        && identifier_nodes.get(identifier_nodes.len() - 2).is_some_and(|&node| {
            let receiver = node_text(node, text);
            looks_like_type_name(&receiver) && !super::is_local_qualified_root(&receiver)
        })
    {
        identifier_nodes.pop();
    }
    let identifiers =
        identifier_nodes.iter().map(|&node| node_text(node, text)).collect::<Vec<_>>();
    let callee_range = identifier_nodes.last().copied().map(CalleeRange::of_node);
    Some((identifiers, callee_range))
}

/// A force-unwrap postfix (`obj!`) — as opposed to a custom postfix operator (`obj++`). Only the
/// force-unwrap `!` is a NAMED `bang` node under `operation`; custom postfix tokens are anonymous.
/// Keying on `bang` is what lets force-unwrap be treated as a transparent receiver wrapper without
/// swallowing a genuine postfix-operator value expression.
fn swift_postfix_is_force_unwrap(node: Node<'_>) -> bool {
    node.kind() == "postfix_expression"
        && node.child_by_field_name("operation").is_some_and(|op| op.kind() == "bang")
}

fn swift_callable_is_static_path(root: Node<'_>) -> bool {
    let mut stack = vec![root];
    let mut children = Vec::new();
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier" | "simple_identifier" | "type_identifier" | "type_arguments"
            | "self_expression" | "super_expression" => continue,
            "user_type" | "navigation_expression" | "navigation_suffix" => {},
            // A force-unwrap is a transparent receiver wrapper: `obj!.method` names the same path
            // as `obj.method`, so descend into the unwrapped target ONLY (its `bang`
            // child is not a path segment). Optional chaining (`obj?.method`) needs
            // nothing here — the `?` is an anonymous token directly under
            // `navigation_expression`, already handled. A NON force-unwrap postfix
            // (`obj++`) is a value expression, not a nameable path, so the
            // whitelist rejects it as before.
            "postfix_expression" if swift_postfix_is_force_unwrap(node) => {
                if let Some(target) = node.child_by_field_name("target") {
                    stack.push(target);
                }
                continue;
            },
            _ => return false,
        }
        children.clear();
        children.extend(named_children(node));
        for &child in children.iter().rev() {
            stack.push(child);
        }
    }
    true
}

fn swift_subscript_suffix<'tree>(node: Node<'tree>, text: &str) -> Option<Node<'tree>> {
    named_children(node)
        .filter(|child| child.kind() == "call_suffix")
        .find(|suffix| node_text(*suffix, text).trim_start().starts_with('['))
}

fn swift_import_identifiers(node: Node<'_>, text: &str) -> Vec<String> {
    named_children(node)
        .filter(|child| child.kind() == "identifier")
        .map(|child| node_text(child, text))
        .collect()
}

fn swift_node_is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() == "class_declaration" && parent.child_by_field_name("name") == Some(node) {
        return parent
            .child_by_field_name("declaration_kind")
            .is_none_or(|kind| kind.kind() != "extension");
    }
    matches!(
        parent.kind(),
        "protocol_declaration"
            | "function_declaration"
            | "protocol_function_declaration"
            | "typealias_declaration"
            | "associatedtype_declaration"
    ) && parent.child_by_field_name("name") == Some(node)
}

fn swift_node_is_type_parameter_declaration(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| parent.kind() == "type_parameter")
}

fn swift_node_is_type_parameter_reference(node: Node<'_>, text: &str) -> bool {
    let reference_segments = syntax::identifier_nodes(node);
    let Some(reference_root) = reference_segments.first().copied() else {
        return false;
    };
    let reference_root = node_text(reference_root, text);
    reference_root == "Self" || swift_name_is_type_parameter_in_scope(&reference_root, node, text)
}

fn swift_name_is_type_parameter_in_scope(reference_name: &str, node: Node<'_>, text: &str) -> bool {
    let mut ancestor = node.parent();
    while let Some(scope) = ancestor {
        for parameters in named_children(scope).filter(|child| {
            matches!(
                child.kind(),
                "type_parameters" | "enum_type_parameters" | "lambda_function_type_parameters"
            )
        }) {
            for parameter in
                named_children(parameters).filter(|child| child.kind() == "type_parameter")
            {
                if syntax::identifier_nodes(parameter)
                    .first()
                    .is_some_and(|&name| node_text(name, text) == reference_name)
                {
                    return true;
                }
            }
        }
        ancestor = scope.parent();
    }
    false
}

fn swift_node_is_import_modifier(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if current.kind() == "import_declaration" {
            return true;
        }
        ancestor = current.parent();
    }
    false
}

fn swift_inherited_type_name(node: Node<'_>) -> Option<Node<'_>> {
    let inherited = node.child_by_field_name("inherits_from")?;
    if inherited.kind() == "user_type" {
        return Some(inherited);
    }
    last_identifier_node(inherited).map(final_segment_node)
}

fn swift_inheritance_is_enum_raw_type(node: Node<'_>, text: &str) -> bool {
    let mut ancestor = node.parent();
    let declaration = loop {
        let Some(current) = ancestor else {
            return false;
        };
        if current.kind() == "class_declaration" {
            break current;
        }
        ancestor = current.parent();
    };
    if declaration.child_by_field_name("declaration_kind").is_none_or(|kind| kind.kind() != "enum")
    {
        return false;
    }
    let first_inheritance =
        named_children(declaration).find(|child| child.kind() == "inheritance_specifier");
    first_inheritance == Some(node)
        && (swift_inheritance_is_builtin_raw_type(node, text)
            || swift_enum_has_explicit_raw_value(declaration))
}

fn swift_inheritance_is_builtin_raw_type(node: Node<'_>, text: &str) -> bool {
    let Some(type_name) = swift_inherited_type_name(node) else {
        return false;
    };
    let identifiers = syntax::identifier_nodes(type_name);
    let segments =
        identifiers.iter().map(|&identifier| node_text(identifier, text)).collect::<Vec<_>>();
    let raw_type = match segments.as_slice() {
        [raw_type] => raw_type.as_str(),
        [module, raw_type] if module == "Swift" => raw_type.as_str(),
        _ => return false,
    };
    if raw_type.is_empty() {
        return false;
    }
    matches!(
        raw_type,
        "Character"
            | "String"
            | "Int"
            | "Int8"
            | "Int16"
            | "Int32"
            | "Int64"
            | "UInt"
            | "UInt8"
            | "UInt16"
            | "UInt32"
            | "UInt64"
            | "Float"
            | "Double"
            | "Float80"
    )
}

fn swift_enum_has_explicit_raw_value(declaration: Node<'_>) -> bool {
    let mut stack = vec![declaration];
    let mut children = Vec::new();
    while let Some(node) = stack.pop() {
        if node.kind() == "enum_entry" && node.child_by_field_name("raw_value").is_some() {
            return true;
        }
        if node != declaration && node.kind() == "class_declaration" {
            continue;
        }
        children.clear();
        children.extend(named_children(node));
        stack.extend(children.iter().copied());
    }
    false
}

#[cfg(test)]
#[path = "edges_tests.rs"]
mod tests;
