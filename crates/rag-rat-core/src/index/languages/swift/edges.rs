//! Swift graph-edge extraction for the shared structural edge walk.

use std::path::Path;

use tree_sitter::Node;

use super::syntax;
use crate::index::edges::extract::*;
use crate::index::edges::*;

pub(super) fn swift_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "source_file"
            if text.contains(',')
                && (text.contains("higherThan:") || text.contains("lowerThan:")) =>
            swift_precedence_group_relation_list_edges(text, node, symbols, out),
        "import_declaration" => {
            let identifiers = swift_import_identifiers(node, text);
            if !identifiers.is_empty() {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    identifiers.join("::"),
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "call_expression" => swift_call_edges(text, node, symbols, out),
        "constructor_expression" => swift_constructor_edges(text, node, symbols, out),
        "macro_invocation" => swift_macro_edges(text, node, symbols, out),
        "operator_declaration" => swift_precedence_group_edges(text, node, symbols, out),
        "precedence_group_attribute" =>
            swift_precedence_group_relation_edges(text, node, symbols, out),
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
        | "bitwise_operation" => swift_operator_or_shorthand_case_edges(text, node, symbols, out),
        "navigation_expression" => swift_qualified_case_edges(text, node, symbols, out),
        "attribute" if !swift_node_is_import_modifier(node) =>
            swift_attribute_macro_edges(text, node, symbols, out),
        "inheritance_specifier" => {
            let Some(type_path) = swift_inherited_type_name(node) else {
                return;
            };
            let identifier_nodes = syntax::identifier_nodes(type_path);
            let identifiers = identifier_nodes
                .iter()
                .map(|&identifier| node_text(identifier, text))
                .collect::<Vec<_>>();
            let Some(name) = identifiers.last().cloned() else {
                return;
            };
            let edge_kind = if swift_inheritance_is_enum_raw_type(node, text) {
                EdgeKind::ReferencesType
            } else {
                EdgeKind::Implements
            };
            out.push(symbol_edge_with_context(
                symbols,
                node,
                text,
                name,
                edge_kind,
                EdgeConfidence::NameOnly,
                swift_edge_context(&identifiers),
                identifier_nodes.last().copied().map(CalleeRange::of_node),
            ));
        },
        "user_type"
            if !swift_node_is_declaration_name(node)
                && !swift_node_is_import_modifier(node)
                && !swift_has_ancestor_kind(node, "attribute")
                && !swift_node_is_type_parameter_reference(node, text) =>
        {
            let identifier_nodes = syntax::identifier_nodes(node);
            let Some(type_node) = identifier_nodes.last().copied() else {
                return;
            };
            let identifiers = identifier_nodes
                .iter()
                .map(|&identifier| node_text(identifier, text))
                .collect::<Vec<_>>();
            let Some(name) = identifiers.last().cloned() else {
                return;
            };
            out.push(symbol_edge_with_context(
                symbols,
                node,
                text,
                name,
                EdgeKind::ReferencesType,
                EdgeConfidence::NameOnly,
                swift_edge_context(&identifiers),
                Some(CalleeRange::of_node(type_node)),
            ));
        },
        "type_identifier"
            if node.parent().is_none_or(|parent| parent.kind() != "user_type")
                && !swift_node_is_declaration_name(node)
                && !swift_node_is_type_parameter_declaration(node)
                && !swift_node_is_type_parameter_reference(node, text)
                && !swift_has_ancestor_kind(node, "attribute")
                && !swift_node_is_import_modifier(node) =>
        {
            let name = node_text(node, text);
            out.push(symbol_edge(
                symbols,
                node,
                name,
                EdgeKind::ReferencesType,
                EdgeConfidence::NameOnly,
                Some(CalleeRange::of_node(node)),
            ));
        },
        _ => {},
    }
}

fn swift_operator_or_shorthand_case_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
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
            symbols,
            node,
            node_text(case_name, text),
            EdgeKind::CallsName,
            EdgeConfidence::NameOnly,
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
        symbols,
        node,
        node_text(operation, text),
        EdgeKind::UsesOperator,
        EdgeConfidence::NameOnly,
        Some(CalleeRange::of_node(operation)),
    ));
    out.push(symbol_edge(
        symbols,
        node,
        node_text(operation, text),
        EdgeKind::CallsName,
        EdgeConfidence::NameOnly,
        Some(CalleeRange::of_node(operation)),
    ));
}

fn swift_qualified_case_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
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
        symbols,
        node,
        text,
        node_text(case_name, text),
        EdgeKind::CallsName,
        EdgeConfidence::NameOnly,
        swift_edge_context(&identifiers),
        Some(CalleeRange::of_node(case_name)),
    ));
}

fn swift_call_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
) {
    if swift_subscript_suffix(node, text).is_some() {
        let Some(target) = swift_call_target(node) else {
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
        emit_swift_call_edges(text, node, symbols, out, identifiers, None, false);
        return;
    }
    let Some(target) = swift_call_target(node) else {
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
    emit_swift_call_edges(text, node, symbols, out, identifiers, callee_range, constructs);
}

fn swift_constructor_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
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
    emit_swift_call_edges(text, node, symbols, out, identifiers, callee_range, true);
}

fn emit_swift_call_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
    identifiers: Vec<String>,
    callee_range: Option<CalleeRange>,
    constructs: bool,
) {
    let Some(name) = identifiers.last().cloned() else {
        return;
    };
    out.push(symbol_edge_with_context(
        symbols,
        node,
        text,
        name.clone(),
        if constructs { EdgeKind::Constructs } else { EdgeKind::CallsName },
        EdgeConfidence::NameOnly,
        if constructs {
            swift_edge_context(&identifiers)
        } else {
            swift_call_edge_context(&identifiers)
        },
        callee_range,
    ));
    if constructs {
        out.push(symbol_edge_with_context(
            symbols,
            node,
            text,
            name,
            EdgeKind::ReferencesType,
            EdgeConfidence::NameOnly,
            swift_edge_context(&identifiers),
            callee_range,
        ));
    }
}

fn swift_macro_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
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
        symbols,
        node,
        node_text(name_node, text),
        EdgeKind::UsesMacro,
        EdgeConfidence::NameOnly,
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
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
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
        symbols,
        node,
        text,
        name.clone(),
        EdgeKind::UsesMacro,
        EdgeConfidence::NameOnly,
        swift_edge_context(&identifiers),
        Some(CalleeRange::of_node(name_node)),
    ));
    // The grammar uses the same attribute shape for macros, property wrappers, and result
    // builders. Emit both semantic possibilities; language-policy resolution keeps only the one
    // whose declaration kind exists and suppresses both candidates when the attribute is external.
    out.push(symbol_edge_with_context(
        symbols,
        node,
        text,
        name,
        EdgeKind::ReferencesType,
        EdgeConfidence::NameOnly,
        swift_edge_context(&identifiers),
        Some(CalleeRange::of_node(name_node)),
    ));
}

fn swift_precedence_group_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
) {
    let Some(group) = syntax::identifier_nodes(node).last().copied() else {
        return;
    };
    out.push(symbol_edge(
        symbols,
        node,
        node_text(group, text),
        EdgeKind::UsesPrecedenceGroup,
        EdgeConfidence::NameOnly,
        Some(CalleeRange::of_node(group)),
    ));
}

fn swift_precedence_group_relation_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
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
            symbols,
            node,
            node_text(*dependency, text),
            EdgeKind::UsesPrecedenceGroup,
            EdgeConfidence::NameOnly,
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
    out: &mut Vec<EdgeCandidate>,
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
    out: &mut Vec<EdgeCandidate>,
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
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|child| child.kind() != "call_suffix")
}

fn swift_call_target_parts(
    target: Node<'_>,
    text: &str,
) -> Option<(Vec<String>, Option<CalleeRange>)> {
    match target.kind() {
        "array_type" | "array_literal" =>
            Some((vec!["Array".to_string()], Some(CalleeRange::of_node(target)))),
        "dictionary_type" | "dictionary_literal" =>
            Some((vec!["Dictionary".to_string()], Some(CalleeRange::of_node(target)))),
        _ => {
            if !swift_callable_is_static_path(target) {
                return None;
            }
            let mut identifier_nodes = syntax::identifier_nodes(target);
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
        },
    }
}

fn swift_callable_is_static_path(root: Node<'_>) -> bool {
    let mut stack = vec![root];
    let mut children = Vec::new();
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier" | "simple_identifier" | "type_identifier" | "type_arguments"
            | "self_expression" | "super_expression" => continue,
            "user_type" | "navigation_expression" | "navigation_suffix" => {},
            _ => return false,
        }
        let mut cursor = node.walk();
        children.clear();
        children.extend(node.named_children(&mut cursor));
        for &child in children.iter().rev() {
            stack.push(child);
        }
    }
    true
}

fn swift_subscript_suffix<'tree>(node: Node<'tree>, text: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "call_suffix")
        .find(|suffix| node_text(*suffix, text).trim_start().starts_with('['))
}

fn swift_import_identifiers(node: Node<'_>, text: &str) -> Vec<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
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
        let mut cursor = scope.walk();
        for parameters in scope.named_children(&mut cursor).filter(|child| {
            matches!(
                child.kind(),
                "type_parameters" | "enum_type_parameters" | "lambda_function_type_parameters"
            )
        }) {
            let mut parameter_cursor = parameters.walk();
            for parameter in parameters
                .named_children(&mut parameter_cursor)
                .filter(|child| child.kind() == "type_parameter")
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
    let mut cursor = declaration.walk();
    let first_inheritance = declaration
        .named_children(&mut cursor)
        .find(|child| child.kind() == "inheritance_specifier");
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
        let mut cursor = node.walk();
        children.clear();
        children.extend(node.named_children(&mut cursor));
        stack.extend(children.iter().copied());
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::language::Language;

    fn edges(src: &str) -> Vec<EdgeCandidate> {
        syntactic_edges(Path::new("Sources/App/App.swift"), Language::Swift, src, &[])
            .expect("Swift fixture parses")
    }

    fn has(edges: &[EdgeCandidate], kind: EdgeKind, target: &str) -> bool {
        edges.iter().any(|edge| edge.edge_kind == kind && edge.to_name == target)
    }

    fn callee_text<'a>(edge: &EdgeCandidate, src: &'a str) -> Option<&'a str> {
        let span = edge.callee_span?;
        src.get(span.start_byte..span.end_byte)
    }

    fn source_text<'a>(edge: &EdgeCandidate, src: &'a str) -> Option<&'a str> {
        let start = usize::try_from(edge.source_span.start_byte).ok()?;
        let end = usize::try_from(edge.source_span.end_byte).ok()?;
        src.get(start..end)
    }

    #[test]
    fn extracts_import_calls_construction_conformance_and_exact_callee_ranges() {
        let src = r#"
import Foundation

protocol Worker<Element> {}

class Parent {}
class Child: Parent {}

struct Runner: Worker<Service> {
    func run(service: Service?) async {
        let client = Client()
        await client.fetch(id: 1) { value in print(value) }
        service?.ping()
    }
}
"#;
        let edges = edges(src);
        assert!(has(&edges, EdgeKind::Imports, "Foundation"), "import missing: {edges:#?}");
        assert!(has(&edges, EdgeKind::Implements, "Worker"), "conformance missing: {edges:#?}");
        assert!(
            !has(&edges, EdgeKind::Implements, "Service"),
            "generic arguments are type references, not conformances: {edges:#?}"
        );
        assert!(has(&edges, EdgeKind::Implements, "Parent"), "inheritance missing: {edges:#?}");
        assert!(has(&edges, EdgeKind::Constructs, "Client"), "construction missing: {edges:#?}");
        assert!(has(&edges, EdgeKind::ReferencesType, "Service"), "type ref missing: {edges:#?}");

        for callee in ["Client", "fetch", "ping"] {
            let edge = edges
                .iter()
                .find(|edge| edge.to_name == callee)
                .unwrap_or_else(|| panic!("missing {callee} edge: {edges:#?}"));
            assert_eq!(callee_text(edge, src), Some(callee), "wrong callee range for {callee}");
            let source = source_text(edge, src)
                .unwrap_or_else(|| panic!("missing source range for {callee}: {edges:#?}"));
            assert!(
                source.contains(callee),
                "source range for {callee} must cover its callee: {source:?}"
            );
        }

        let fetch = edges
            .iter()
            .find(|edge| edge.to_name == "fetch")
            .unwrap_or_else(|| panic!("missing awaited fetch edge: {edges:#?}"));
        assert!(
            source_text(fetch, src).is_some_and(|source| source.contains("client.fetch")),
            "await must preserve the underlying call-expression source range: {fetch:#?}"
        );
        assert_eq!(fetch.target_qualified_name, None, "value receivers resolve by callee name");
        assert_eq!(fetch.receiver_hint.as_deref(), Some("client"));
    }

    #[test]
    fn generic_declarations_are_not_type_references_and_outer_types_keep_their_names() {
        let src = r#"
struct T {}
struct Namespace { struct T {} }
struct Box<Element> {}

func transform<T: Codable, U>(_ value: Box<T>, _ qualified: Namespace.T) -> Box<U> { value }
"#;
        let edges = edges(src);
        let references = |target: &str| {
            edges
                .iter()
                .filter(|edge| edge.edge_kind == EdgeKind::ReferencesType && edge.to_name == target)
                .count()
        };

        assert_eq!(references("Element"), 0, "generic declarations are not uses: {edges:#?}");
        assert_eq!(references("T"), 1, "only Namespace.T is a nominal reference: {edges:#?}");
        assert_eq!(references("U"), 0, "generic return types are lexical bindings: {edges:#?}");
        assert_eq!(references("Codable"), 1, "generic constraints remain type uses: {edges:#?}");
        assert_eq!(references("Box"), 2, "generic arguments must not replace Box: {edges:#?}");
        assert!(
            edges.iter().any(|edge| {
                edge.edge_kind == EdgeKind::ReferencesType
                    && edge.to_name == "T"
                    && edge.target_qualified_name.as_deref() == Some("Namespace::T")
            }),
            "qualified nominal types are not lexical generic references: {edges:#?}"
        );
        assert!(
            edges
                .iter()
                .filter(|edge| edge.to_name == "Box")
                .all(|edge| { callee_text(edge, src) == Some("Box") }),
            "outer type edges must retain exact Box ranges: {edges:#?}"
        );
    }

    #[test]
    fn operator_and_enum_case_expressions_emit_callable_edges() {
        let src = r####"
enum Status { case idle, failed(Error) }
precedencegroup BasePrecedence {}
precedencegroup SecondaryPrecedence {}
precedencegroup MergePrecedence {
    // } must not close the declaration scanner.
    // note { higherThan: BasePrecedence, SecondaryPrecedence
    higherThan: BasePrecedence,
        SecondaryPrecedence
    /* higherThan: BasePrecedence, SecondaryPrecedence */
    associativity: left
}
precedencegroup InlinePrecedence { higherThan: BasePrecedence, SecondaryPrecedence }
precedencegroup LowerPrecedence { lowerThan: BasePrecedence }
precedencegroup Métrique { higherThan: Élément, `class` }
precedencegroup Élément {}
precedencegroup `class` {}
precedencegroup Broken
let malformedApostrophe = 'x'
precedencegroup GoodPrecedence { higherThan: BasePrecedence, SecondaryPrecedence }
let relationString = "higherThan: BasePrecedence, SecondaryPrecedence"
let rawRelation = #"ignored " precedencegroup Fake { higherThan: BasePrecedence, SecondaryPrecedence }"#
let rawMultiline = ##"""
precedencegroup AlsoFake { higherThan: BasePrecedence, SecondaryPrecedence }
"""##
/*
higherThan: BasePrecedence, SecondaryPrecedence
*/
infix operator <+>: MergePrecedence
prefix operator !
func <+>(lhs: Int, rhs: Int) -> Int { lhs + rhs }

let merged = lhs <+> rhs
let inverted = !flag
let qualified = Status.idle
let shorthand: Status = .idle
let qualifiedPayload = Status.failed(error)
let shorthandPayload: Status = .failed(error)
let unrelated = client.idle
"####;
        let edges = edges(src);
        for operator in ["<+>", "!"] {
            for kind in [EdgeKind::CallsName, EdgeKind::UsesOperator] {
                let edge = edges
                    .iter()
                    .find(|edge| edge.edge_kind == kind && edge.to_name == operator)
                    .unwrap_or_else(|| {
                        panic!("missing {kind:?} operator edge to {operator}: {edges:#?}")
                    });
                assert_eq!(callee_text(edge, src), Some(operator));
            }
        }
        let precedence = edges
            .iter()
            .find(|edge| {
                edge.edge_kind == EdgeKind::UsesPrecedenceGroup && edge.to_name == "MergePrecedence"
            })
            .unwrap_or_else(|| panic!("missing precedence-group dependency: {edges:#?}"));
        assert_eq!(callee_text(precedence, src), Some("MergePrecedence"));
        let group_dependencies = edges
            .iter()
            .filter(|edge| {
                edge.edge_kind == EdgeKind::UsesPrecedenceGroup && edge.to_name == "BasePrecedence"
            })
            .collect::<Vec<_>>();
        assert_eq!(
            group_dependencies.len(),
            4,
            "valid declarations each emit once while malformed and raw decoys do not: {edges:#?}"
        );
        assert!(
            group_dependencies.iter().all(|edge| callee_text(edge, src) == Some("BasePrecedence"))
        );
        let secondary_dependencies = edges
            .iter()
            .filter(|edge| {
                edge.edge_kind == EdgeKind::UsesPrecedenceGroup
                    && edge.to_name == "SecondaryPrecedence"
            })
            .collect::<Vec<_>>();
        assert_eq!(secondary_dependencies.len(), 3, "all valid list forms emit once: {edges:#?}");
        let recovered = edges
            .iter()
            .filter(|edge| {
                edge.edge_kind == EdgeKind::UsesPrecedenceGroup
                    && edge.evidence.as_deref().is_some_and(|evidence| evidence.contains(','))
            })
            .collect::<Vec<_>>();
        assert_eq!(recovered.len(), 8, "comments and strings must not create dependencies");
        assert!(recovered.iter().all(|edge| {
            callee_text(edge, src) == Some(edge.to_name.as_str())
                && source_text(edge, src).is_some_and(|source| {
                    source.contains("higherThan:") && source.len() < src.len()
                })
        }));
        for (owner, dependency) in [("Métrique", "Élément"), ("Métrique", "`class`")] {
            let edge = edges
                .iter()
                .find(|edge| {
                    edge.edge_kind == EdgeKind::UsesPrecedenceGroup && edge.to_name == dependency
                })
                .unwrap_or_else(|| panic!("missing {owner} -> {dependency}: {edges:#?}"));
            assert_eq!(callee_text(edge, src), Some(dependency));
        }
        assert!(edges.iter().all(|edge| !matches!(edge.to_name.as_str(), "Fake" | "AlsoFake")));

        let cases = |name: &str| {
            edges
                .iter()
                .filter(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == name)
                .collect::<Vec<_>>()
        };
        let idle = cases("idle");
        assert_eq!(idle.len(), 2, "qualified and shorthand cases each emit once: {edges:#?}");
        assert!(
            idle.iter().any(|edge| {
                edge.target_qualified_name.as_deref() == Some("Status::idle")
                    && edge.receiver_hint.as_deref() == Some("Status")
            }),
            "qualified cases retain enum context: {edges:#?}"
        );
        assert!(
            idle.iter().all(|edge| callee_text(edge, src) == Some("idle")),
            "case edges retain exact leaf ranges: {edges:#?}"
        );
        assert_eq!(
            cases("failed").len(),
            2,
            "qualified and shorthand associated-value cases each emit once: {edges:#?}"
        );
    }

    #[test]
    fn forced_unwraps_are_not_operator_calls_but_other_postfix_tokens_are() {
        let src = r#"
postfix operator ++
postfix func ++(value: Int) -> Int { value }

let unwrapped = maybe!
let transformed = value++
"#;
        let edges = edges(src);
        for kind in [EdgeKind::CallsName, EdgeKind::UsesOperator] {
            assert!(
                edges.iter().any(|edge| {
                    edge.edge_kind == kind
                        && edge.to_name == "++"
                        && callee_text(edge, src) == Some("++")
                }),
                "non-bang postfix token must emit {kind:?}: {edges:#?}"
            );
            assert!(
                !edges.iter().any(|edge| edge.edge_kind == kind && edge.to_name == "!"),
                "force unwrap must not emit {kind:?}: {edges:#?}"
            );
        }
    }

    #[test]
    fn local_receiver_calls_keep_their_qualified_roots() {
        let src = r#"
class Parent { class func make() {} }
class Store: Parent {
    class func make() {
        Self.make()
        self.make()
        super.make()
    }
}
"#;
        let edges = edges(src);
        for receiver in ["Self", "self", "super"] {
            let qualified = format!("{receiver}::make");
            let call = edges
                .iter()
                .find(|edge| {
                    edge.edge_kind == EdgeKind::CallsName
                        && edge.target_qualified_name.as_deref() == Some(qualified.as_str())
                })
                .unwrap_or_else(|| panic!("missing {receiver}.make call: {edges:#?}"));
            assert_eq!(call.receiver_hint.as_deref(), Some(receiver));
            assert_eq!(callee_text(call, src), Some("make"));
        }
    }

    #[test]
    fn attributed_imports_only_name_the_imported_module() {
        let src = r#"
@testable import App
@_exported import Foo.Bar
"#;
        let edges = edges(src);
        assert!(has(&edges, EdgeKind::Imports, "App"), "testable import missing: {edges:#?}");
        assert!(has(&edges, EdgeKind::Imports, "Foo.Bar"), "re-export import missing: {edges:#?}");
        assert!(
            !edges.iter().any(|edge| {
                edge.edge_kind == EdgeKind::Imports
                    && (edge.to_name.contains("testable") || edge.to_name.contains("_exported"))
            }),
            "import modifiers must not enter module names: {edges:#?}"
        );
        assert!(
            !has(&edges, EdgeKind::ReferencesType, "testable")
                && !has(&edges, EdgeKind::ReferencesType, "_exported"),
            "import attributes are not type references: {edges:#?}"
        );
        assert!(
            !has(&edges, EdgeKind::UsesMacro, "testable")
                && !has(&edges, EdgeKind::UsesMacro, "_exported"),
            "import modifiers are not attached macro uses: {edges:#?}"
        );
    }

    #[test]
    fn enum_raw_types_are_references_while_protocols_remain_conformances() {
        let src = r#"
protocol Codable {}
enum Status: String, Codable { case ready = "ready" }
enum Direction: Int { case north, south }
enum Empty: String {}
enum Plain: Codable { case ready }
enum Qualified: Domain.String { case ready }
enum Outer: Codable {
    enum Inner: String { case value = "value" }
}
"#;
        let edges = edges(src);
        assert!(
            has(&edges, EdgeKind::ReferencesType, "String"),
            "raw enum type must be a type reference: {edges:#?}"
        );
        assert!(
            !edges.iter().any(|edge| {
                edge.edge_kind == EdgeKind::Implements
                    && edge.to_name == "String"
                    && edge.target_qualified_name.is_none()
            }),
            "unqualified raw enum types must not become conformances: {edges:#?}"
        );
        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.edge_kind == EdgeKind::Implements && edge.to_name == "Codable")
                .count(),
            3,
            "protocol conformances on raw, plain, and nested enums remain visible: {edges:#?}"
        );
        assert!(
            edges.iter().any(|edge| {
                edge.edge_kind == EdgeKind::Implements
                    && edge.target_qualified_name.as_deref() == Some("Domain::String")
            }),
            "an arbitrary qualified String is a conformance without raw-value evidence: {edges:#?}"
        );
        for raw_type in ["Int", "String"] {
            assert!(
                has(&edges, EdgeKind::ReferencesType, raw_type),
                "implicit and empty raw enums retain {raw_type} type edges: {edges:#?}"
            );
        }
    }

    #[test]
    fn bracket_syntax_emits_subscript_calls_with_receiver_context() {
        let src = r#"
let value = store[id]
let staticValue = Store[id]
func generic<T>(_ index: Int) {
    _ = T[index]
    _ = Namespace.T[index]
}
"#;
        let edges = edges(src);
        let subscripts = edges
            .iter()
            .filter(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == "subscript")
            .collect::<Vec<_>>();
        assert_eq!(subscripts.len(), 3, "eligible bracket expressions emit one call: {edges:#?}");
        assert!(subscripts.iter().any(|edge| {
            edge.receiver_hint.as_deref() == Some("store") && edge.target_qualified_name.is_none()
        }));
        assert!(subscripts.iter().any(|edge| {
            edge.receiver_hint.as_deref() == Some("Store")
                && edge.target_qualified_name.as_deref() == Some("Store::subscript")
        }));
        assert!(subscripts.iter().any(|edge| {
            edge.receiver_hint.as_deref() == Some("Namespace")
                && edge.target_qualified_name.as_deref() == Some("Namespace::T::subscript")
        }));
        assert!(
            !subscripts.iter().any(|edge| {
                edge.receiver_hint.as_deref() == Some("T")
                    && edge.target_qualified_name.as_deref() == Some("T::subscript")
            }),
            "a lexical generic root must not bind to a nominal subscript: {edges:#?}"
        );
    }

    #[test]
    fn generic_calls_and_constructor_expressions_keep_the_constructed_type() {
        let src = r#"
struct Box<T> {}
protocol DefaultInit {}

func make() {
    let box = Box<Int>()
    let array = Array<String>()
    let dictionary = [String: Int]()
}

func makeGeneric<T: DefaultInit>() -> T {
    let direct = T()
    let explicit = T.init()
    return direct
}
"#;
        let edges = edges(src);
        for target in ["Box", "Array", "Dictionary"] {
            let edge = edges
                .iter()
                .find(|edge| edge.edge_kind == EdgeKind::Constructs && edge.to_name == target)
                .unwrap_or_else(|| panic!("missing {target} construction: {edges:#?}"));
            assert!(
                source_text(edge, src).is_some_and(|source| source.contains('(')),
                "construction source must cover the initializer call: {edge:#?}"
            );
        }
        assert!(
            !has(&edges, EdgeKind::Constructs, "Int")
                && !has(&edges, EdgeKind::Constructs, "String")
                && !has(&edges, EdgeKind::Constructs, "T"),
            "generic arguments are not constructed call targets: {edges:#?}"
        );
        assert!(
            !edges.iter().any(|edge| {
                matches!(edge.edge_kind, EdgeKind::CallsName | EdgeKind::Constructs)
                    && edge.to_name == "T"
            }),
            "generic initializer spellings must not emit callable edges: {edges:#?}"
        );
        let box_edge = edges
            .iter()
            .find(|edge| edge.edge_kind == EdgeKind::Constructs && edge.to_name == "Box")
            .unwrap();
        assert_eq!(callee_text(box_edge, src), Some("Box"));
    }

    #[test]
    fn explicit_init_calls_construct_the_qualifying_type() {
        let src = r#"
func make() {
    let client = Client.init()
    let qualified = Module.Service.init()
    client.init()
    Self.init()
    self.init()
    super.init()
}
"#;
        let edges = edges(src);
        for target in ["Client", "Service"] {
            let edge = edges
                .iter()
                .find(|edge| edge.edge_kind == EdgeKind::Constructs && edge.to_name == target)
                .unwrap_or_else(|| panic!("missing {target} construction: {edges:#?}"));
            assert_eq!(callee_text(edge, src), Some(target));
        }
        assert!(
            has(&edges, EdgeKind::CallsName, "init"),
            "lowercase receiver init remains a method call: {edges:#?}"
        );
        assert!(
            edges.iter().any(|edge| {
                edge.edge_kind == EdgeKind::CallsName
                    && edge.to_name == "init"
                    && edge.receiver_hint.as_deref() == Some("client")
            }),
            "a value-receiver init must retain its receiver hint: {edges:#?}"
        );
        for call in ["Self.init()", "self.init()", "super.init()"] {
            let receiver = call.split_once('.').map(|(receiver, _)| receiver).unwrap();
            let qualified = format!("{receiver}::init");
            assert!(
                edges.iter().any(|edge| {
                    edge.edge_kind == EdgeKind::CallsName
                        && edge.to_name == "init"
                        && edge.evidence.as_deref() == Some(call)
                        && edge.receiver_hint.as_deref() == Some(receiver)
                        && edge.target_qualified_name.as_deref() == Some(qualified.as_str())
                }),
                "{call} must retain its local receiver context: {edges:#?}"
            );
        }
        assert!(
            !edges
                .iter()
                .any(|edge| edge.edge_kind == EdgeKind::Constructs && edge.to_name == "init"),
            "init is not itself a constructed type: {edges:#?}"
        );
    }

    #[test]
    fn qualified_type_references_keep_the_canonical_path() {
        let src = "func load(_ request: API.Request) -> Other.Request { request }";
        let edges = edges(src);
        let qualified_names = edges
            .iter()
            .filter(|edge| edge.edge_kind == EdgeKind::ReferencesType && edge.to_name == "Request")
            .map(|edge| {
                (
                    edge.target_qualified_name.as_deref(),
                    edge.receiver_hint.as_deref(),
                    callee_text(edge, src),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(qualified_names, vec![
            (Some("API::Request"), Some("API"), Some("Request")),
            (Some("Other::Request"), Some("Other"), Some("Request")),
        ]);
    }

    #[test]
    fn generic_and_self_associated_type_projections_are_not_nominal_references() {
        let src = r#"
struct Element {}
protocol Container { associatedtype Item }
func generic<T: Container>(_ value: T) -> T.Item { value as! T.Item }
extension Container { func local() -> Self.Item { fatalError() } }
"#;
        let edges = edges(src);
        assert!(
            !edges.iter().any(|edge| {
                edge.edge_kind == EdgeKind::ReferencesType
                    && matches!(edge.to_name.as_str(), "T" | "Item" | "Self")
            }),
            "generic/Self projections must not bind unrelated nominal types: {edges:#?}"
        );
        assert!(has(&edges, EdgeKind::ReferencesType, "Container"));
    }

    #[test]
    fn qualified_inheritance_and_construction_share_canonical_paths() {
        let src = "class Child: API.Parent { let request = API.Request() }";
        let edges = edges(src);
        for (kind, target, qualified) in [
            (EdgeKind::Implements, "Parent", "API::Parent"),
            (EdgeKind::Constructs, "Request", "API::Request"),
            (EdgeKind::ReferencesType, "Request", "API::Request"),
        ] {
            assert!(
                edges.iter().any(|edge| {
                    edge.edge_kind == kind
                        && edge.to_name == target
                        && edge.target_qualified_name.as_deref() == Some(qualified)
                }),
                "missing canonical {kind:?} edge to {qualified}: {edges:#?}"
            );
        }
    }

    #[test]
    fn macro_invocations_emit_uses_macro_edges() {
        let src = r#"
macro stringify<T>(_ value: T) = #externalMacro(module: "Macros", type: "StringifyMacro")
let result = #stringify(value)
"#;
        let edges = edges(src);
        let invocation = edges
            .iter()
            .find(|edge| edge.edge_kind == EdgeKind::UsesMacro && edge.to_name == "stringify")
            .unwrap_or_else(|| panic!("missing Swift macro-use edge: {edges:#?}"));
        assert_eq!(callee_text(invocation, src), Some("stringify"));
        assert!(source_text(invocation, src).is_some_and(|source| source == "#stringify(value)"));
        assert!(
            !has(&edges, EdgeKind::UsesMacro, "externalMacro"),
            "the compiler implementation hook is not a macro call: {edges:#?}"
        );
    }

    #[test]
    fn ordinary_attributes_and_extension_targets_emit_type_references() {
        let src = r#"
@MainActor
@Macros.Observable(source: "fixture")
class Model {
    @Wrapper var value: Int
}

extension Model {
    func refresh() {}
}
"#;
        let edges = edges(src);
        for target in ["MainActor", "Observable", "Wrapper"] {
            assert!(
                has(&edges, EdgeKind::ReferencesType, target),
                "attribute type {target} must remain visible: {edges:#?}"
            );
            assert!(has(&edges, EdgeKind::UsesMacro, target));
        }
        assert!(
            !has(&edges, EdgeKind::UsesMacro, "source"),
            "attribute argument labels are not macro names: {edges:#?}"
        );
        let extension_target = edges
            .iter()
            .find(|edge| {
                edge.edge_kind == EdgeKind::ReferencesType
                    && edge.to_name == "Model"
                    && source_text(edge, src).is_some_and(|source| source.starts_with("Model"))
            })
            .unwrap_or_else(|| panic!("extension target must reference Model: {edges:#?}"));
        assert_eq!(callee_text(extension_target, src), Some("Model"));
    }

    #[test]
    fn attached_attributes_emit_resolver_candidates_without_argument_labels() {
        let src = r#"
@attached(member) macro Observable() = #externalMacro(module: "Macros", type: "ObservableMacro")
@Observable struct Model {}
@Macros.Observable struct QualifiedModel {}
@available(*, deprecated) struct LegacyModel {}
@objc class ObjectiveCModel {}
@MainActor class ActorModel {}
"#;
        let edges = edges(src);
        let macro_uses =
            edges.iter().filter(|edge| edge.edge_kind == EdgeKind::UsesMacro).collect::<Vec<_>>();
        for attribute in ["attached", "Observable", "available", "objc", "MainActor"] {
            assert!(
                macro_uses.iter().any(|edge| edge.to_name == attribute),
                "attribute {attribute} must reach resolver policy: {edges:#?}"
            );
        }
        let qualified = macro_uses
            .iter()
            .find(|edge| edge.target_qualified_name.as_deref() == Some("Macros::Observable"))
            .unwrap_or_else(|| {
                panic!("qualified macro use must retain module context: {edges:#?}")
            });
        assert_eq!(qualified.receiver_hint.as_deref(), Some("Macros"));
        assert!(!has(&edges, EdgeKind::UsesMacro, "member"));
        assert!(!has(&edges, EdgeKind::UsesMacro, "externalMacro"));
    }

    #[test]
    fn expression_valued_callables_do_not_invent_outer_call_targets() {
        let src = r#"
let immediate = { helper() }()
let selected = handlers[key]()
plain()
client.fetch()
self.refresh()
super.finish()
"#;
        let edges = edges(src);
        let helper_calls = edges
            .iter()
            .filter(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == "helper")
            .collect::<Vec<_>>();
        assert_eq!(helper_calls.len(), 1, "closure IIFE must emit only its inner call");
        assert_eq!(source_text(helper_calls[0], src), Some("helper()"));
        assert!(has(&edges, EdgeKind::CallsName, "plain"), "direct call missing");
        assert!(has(&edges, EdgeKind::CallsName, "fetch"), "static member call missing");
        assert!(has(&edges, EdgeKind::CallsName, "refresh"), "self member call missing");
        assert!(has(&edges, EdgeKind::CallsName, "finish"), "super member call missing");
        assert!(
            !has(&edges, EdgeKind::CallsName, "key")
                && !has(&edges, EdgeKind::CallsName, "handlers"),
            "subscripted/dynamic callable must not invent a named outer call: {edges:#?}"
        );
    }
}
