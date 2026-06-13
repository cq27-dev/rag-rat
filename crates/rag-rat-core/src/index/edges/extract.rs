use super::*;

pub(crate) fn index_file_edges(
    conn: &Connection,
    file_id: i64,
    path: &Path,
    language: Language,
    text: &str,
) -> anyhow::Result<()> {
    // Inline path (incremental / graph-reindex): symbols come from the DB (they carry real ids),
    // so candidates can be inserted directly. The full-rebuild path instead computes candidates in
    // the parallel prepare phase via `edge_candidates` and remaps local ids at insert time.
    let symbols = symbols_for_file(conn, file_id)?;
    let candidates = edge_candidates(path, language, text, &symbols)?;
    insert_candidates(conn, file_id, candidates)
}

/// Compute graph edge candidates for a file from its parsed `symbols`. This is the CPU-heavy part
/// (it re-parses `text` with tree-sitter for syntactic edges), so the full-rebuild path runs it
/// inside the parallel prepare phase. `symbols`' ids are used as `from_symbol_id`; the caller is
/// responsible for those ids being meaningful (real DB ids on the inline path, local indices to be
/// remapped on the prepared path).
pub(crate) fn edge_candidates(
    path: &Path,
    language: Language,
    text: &str,
    symbols: &[IndexedSymbol],
) -> anyhow::Result<Vec<EdgeCandidate>> {
    if language == Language::Markdown {
        return Ok(Vec::new());
    }
    let mut candidates = contains_edges(symbols);
    candidates.extend(syntactic_edges(path, language, text, symbols)?);
    Ok(candidates)
}

/// Like [`edge_candidates`] but walks an already-parsed tree (`root`) instead of re-parsing `text`.
/// Used by the full-rebuild prepare phase, which parses each file once and shares the tree.
pub(crate) fn edge_candidates_from_root(
    path: &Path,
    language: Language,
    text: &str,
    root: Node<'_>,
    symbols: &[IndexedSymbol],
) -> Vec<EdgeCandidate> {
    if language == Language::Markdown {
        return Vec::new();
    }
    let mut candidates = contains_edges(symbols);
    collect_edges(language, text, root, symbols, path, &mut candidates);
    candidates
}
/// The last `::`-separated segment of a qualified name.
pub(crate) fn qn_tail(qualified_name: &str) -> &str {
    qualified_name.rsplit("::").next().unwrap_or(qualified_name)
}
pub(crate) fn contains_edges(symbols: &[IndexedSymbol]) -> Vec<EdgeCandidate> {
    let mut out = Vec::new();
    for child in symbols {
        let parent = symbols
            .iter()
            .filter(|candidate| {
                candidate.id != child.id
                    && candidate.start_byte <= child.start_byte
                    && candidate.end_byte >= child.end_byte
            })
            .min_by_key(|candidate| candidate.end_byte.saturating_sub(candidate.start_byte));
        if let Some(parent) = parent {
            out.push(EdgeCandidate {
                from_symbol_id: Some(parent.id),
                from_name: Some(parent.qualified_name.clone()),
                to_name: child.qualified_name.clone(),
                target_qualified_name: Some(child.qualified_name.clone()),
                evidence: Some(child.qualified_name.clone()),
                receiver_hint: None,
                source_span: child.span(),
                callee_span: None,
                import_scope: None,
                edge_kind: EdgeKind::Contains,
                confidence: EdgeConfidence::Exact,
            });
        }
    }
    out
}
pub(crate) fn syntactic_edges(
    path: &Path,
    language: Language,
    text: &str,
    symbols: &[IndexedSymbol],
) -> anyhow::Result<Vec<EdgeCandidate>> {
    let grammar = match parser::parser_kind(path, language) {
        ParserKind::Rust => tree_sitter_rust::LANGUAGE.into(),
        ParserKind::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ParserKind::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        ParserKind::Kotlin => tree_sitter_kotlin::LANGUAGE.into(),
        ParserKind::C => tree_sitter_c::LANGUAGE.into(),
        ParserKind::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        ParserKind::Markdown => return Ok(Vec::new()),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar)?;
    let Some(tree) = parser.parse(text, None) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    collect_edges(language, text, tree.root_node(), symbols, path, &mut out);
    Ok(out)
}
pub(crate) fn collect_edges(
    language: Language,
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    if node.is_error() || node.is_missing() {
        return;
    }
    match language {
        Language::Rust => rust_edges(text, node, symbols, path, out),
        Language::TypeScript => typescript_edges(text, node, symbols, path, out),
        Language::Kotlin => kotlin_edges(text, node, symbols, path, out),
        Language::C | Language::Cpp => c_like_edges(text, node, symbols, path, out),
        Language::Markdown => {},
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_edges(language, text, child, symbols, path, out);
    }
}
pub(crate) fn rust_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "use_declaration" => {
            let names = identifiers_under(node, text);
            let is_reexport = node_text(node, text).trim_start().starts_with("pub use ");
            // Module-aware import scope (#61): a Rust `use` is scoped to its enclosing module body
            // (or block, for a block-local `use`), not the whole file. Record that scope + the
            // enclosing module's id on the dedicated import-scope columns so resolution suppresses
            // a bare reference only inside the `use`'s scope (parent-`mod` `use`s don't
            // reach a child `mod`). Top-level `use` → whole file, `MOD_FILE_ROOT`.
            let scope = enclosing_use_scope(node, text);
            // The crate-aware scope rebuilds its {leaf → root} map by re-parsing an Imports edge's
            // `evidence` with `imports::parse_use`; `parse_use` returns EVERY leaf, so ONE edge
            // carrying the FULL (untruncated) use text populates the whole map for this `use`.
            // Attach the full text to only the FIRST emitted Imports edge — the default
            // `edge_evidence` truncates at 240 chars and would drop late braced leaves (#97 item 1)
            // — and let the rest carry standard evidence, so a multi-hundred-KB `use` isn't cloned
            // into every leaf's edge (#97 item 3).
            let mut full_use_evidence = Some(use_declaration_evidence(node, text));
            for name in names {
                if !is_rust_path_keyword(&name) {
                    let evidence =
                        full_use_evidence.take().unwrap_or_else(|| edge_evidence(node, text));
                    out.push(file_edge_scoped(
                        path,
                        node,
                        name,
                        Some(evidence),
                        EdgeKind::Imports,
                        EdgeConfidence::NameOnly,
                        Some(scope),
                    ));
                }
            }
            if is_reexport {
                for name in identifiers_under(node, text) {
                    if !is_rust_path_keyword(&name) {
                        out.push(file_edge(
                            path,
                            node,
                            text,
                            name,
                            EdgeKind::Exports,
                            EdgeConfidence::NameOnly,
                        ));
                    }
                }
            }
        },
        "mod_item" =>
            if let Some(name) = child_name_text(node, text) {
                // An INLINE `mod foo { … }` carries its body range + its own id as the import scope
                // so resolution can rebuild the per-file module interval set (the ref→mod-id
                // lookup) from edges alone, WITHOUT the tree (#61) — including modules that contain
                // no `use`. A non-inline `mod foo;` has no body and introduces no scope (NULL).
                let scope = inline_mod_scope(node);
                out.push(file_edge_scoped(
                    path,
                    node,
                    name,
                    Some(edge_evidence(node, text)),
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                    scope,
                ));
            },
        "call_expression" => {
            if let Some(name) = call_target_name(node, text) {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: target_qualified_name(node, text),
                        receiver_hint: scoped_receiver_name(node, text),
                    },
                    call_target_node(node).map(CalleeRange::of_node),
                ));
            }
            // A scoped call receiver is a type reference only when it names a type. By Rust
            // convention types are PascalCase, while module paths (`std::env::…`) and method
            // receivers on locals (`p.as_os_str()`) are snake_case — emitting those as
            // `references_type` produced bogus "types" like `std` and `p`. Gate on an
            // uppercase-leading receiver so `Foo::bar()` still records a type reference.
            if let Some(receiver) = scoped_receiver_name(node, text)
                && receiver.chars().next().is_some_and(char::is_uppercase)
            {
                out.push(symbol_edge(
                    symbols,
                    node,
                    receiver,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    // The type is the receiver — the LEADING `::` segment (`Foo` in `Foo::bar()`)
                    // — so anchor the range on the function path's first
                    // identifier, not its tail.
                    node.child_by_field_name("function")
                        .and_then(first_identifier_node)
                        .map(CalleeRange::of_node),
                ));
            }
        },
        "macro_invocation" =>
            if let Some(name) = first_identifier_text(node, text) {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::UsesMacro,
                    EdgeConfidence::NameOnly,
                    EdgeContext::default(),
                    first_identifier_node(node).map(CalleeRange::of_node),
                ));
            },
        "impl_item" => rust_impl_edges(text, node, symbols, out),
        "type_identifier" | "scoped_type_identifier" | "generic_type" => {
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    last_identifier_node(node).map(final_segment_node).map(CalleeRange::of_node),
                ));
            }
        },
        _ => {},
    }
}
pub(crate) fn rust_impl_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
) {
    let node_text = node_text(node, text);
    let header = node_text.split('{').next().unwrap_or_default();
    let type_names = header
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|part| !part.is_empty())
        .filter(|part| !matches!(*part, "impl" | "for" | "where"))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if node_text.contains(" for ") && type_names.len() >= 2 {
        let trait_name = type_names.first().cloned().unwrap_or_default();
        let type_name = type_names.last().cloned().unwrap_or_default();
        out.push(EdgeCandidate {
            from_symbol_id: containing_symbol(symbols, node.start_byte()).map(|symbol| symbol.id),
            from_name: Some(type_name),
            to_name: trait_name,
            target_qualified_name: None,
            evidence: Some(edge_evidence(node, text)),
            receiver_hint: None,
            source_span: span_for_node(node),
            // The trait/type names here come from string-splitting the impl header, not from a
            // located identifier node, so there is no clean callee range to record (#67).
            callee_span: None,
            import_scope: None,
            edge_kind: EdgeKind::Implements,
            confidence: EdgeConfidence::NameOnly,
        });
    } else if let Some(type_name) = type_names.first() {
        out.push(symbol_edge(
            symbols,
            node,
            type_name.clone(),
            EdgeKind::ReferencesType,
            EdgeConfidence::NameOnly,
            // `type_name` is string-split from the impl header, not a located node — no range
            // (#67).
            None,
        ));
    }
}
pub(crate) fn typescript_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "import_statement" =>
            for name in identifiers_under(node, text) {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    name,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            },
        "export_statement" =>
            for name in identifiers_under(node, text) {
                out.push(file_edge(
                    path,
                    node,
                    text,
                    name,
                    EdgeKind::Exports,
                    EdgeConfidence::NameOnly,
                ));
            },
        "call_expression" | "new_expression" => {
            let function = node.child_by_field_name("function").unwrap_or(node);
            let identifiers = identifiers_under(function, text);
            // Parallel to `identifiers` (same traversal order), so `.last()`/`.first()` pick the
            // node for the same token the string Vec does (#67).
            let identifier_nodes = identifier_nodes_under(function);
            if let Some(name) = identifiers.last().cloned().or_else(|| call_target_name(node, text))
            {
                let edge_kind = if node.kind() == "new_expression" {
                    EdgeKind::Constructs
                } else {
                    EdgeKind::CallsName
                };
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    edge_kind,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: dotted_qualified_name(&identifiers),
                        receiver_hint: identifiers
                            .first()
                            .filter(|_| identifiers.len() > 1)
                            .cloned(),
                    },
                    // The callee is the final segment — `.last()`, matching `identifiers.last()`.
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
                    // The type is the receiver — the FIRST segment, matching
                    // `identifiers.first()`.
                    identifier_nodes
                        .first()
                        .filter(|_| identifier_nodes.len() > 1)
                        .copied()
                        .map(CalleeRange::of_node),
                ));
            }
        },
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name) = first_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    first_identifier_node(node).map(CalleeRange::of_node),
                ));
            }
        },
        "type_identifier" => {
            if let Some(name) = node.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    // `node` is itself the `type_identifier` token — its range is the callee
                    // range.
                    Some(CalleeRange::of_node(node)),
                ));
            }
        },
        _ => {},
    }
}
pub(crate) fn kotlin_edges(
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
pub(crate) fn c_like_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
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
                out.push(file_edge(
                    path,
                    node,
                    text,
                    include,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
        },
        "call_expression" => {
            let function = node.child_by_field_name("function").unwrap_or(node);
            let identifiers = identifiers_under(function, text);
            // Parallel to `identifiers` so the callee `.last()` node matches the string pick (#67).
            let identifier_nodes = identifier_nodes_under(function);
            if let Some(name) = identifiers.last().cloned().or_else(|| call_target_name(node, text))
            {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: c_like_qualified_name(&identifiers),
                        receiver_hint: identifiers
                            .first()
                            .filter(|_| identifiers.len() > 1)
                            .cloned(),
                    },
                    identifier_nodes.last().copied().map(CalleeRange::of_node),
                ));
            }
        },
        "type_identifier" | "qualified_identifier" | "namespace_identifier" => {
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    last_identifier_node(node).map(final_segment_node).map(CalleeRange::of_node),
                ));
            }
        },
        _ => {},
    }
}
pub(crate) fn file_edge(
    path: &Path,
    node: Node<'_>,
    text: &str,
    to_name: String,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
) -> EdgeCandidate {
    EdgeCandidate {
        from_symbol_id: None,
        from_name: Some(path.to_string_lossy().replace('\\', "/")),
        to_name,
        target_qualified_name: None,
        evidence: Some(edge_evidence(node, text)),
        receiver_hint: None,
        source_span: span_for_node(node),
        // File-level edges (imports / exports / mod) have no callee identifier to anchor (#67).
        callee_span: None,
        import_scope: None,
        edge_kind,
        confidence,
    }
}
/// `file_edge` with caller-supplied evidence and an optional module-aware import scope, for the
/// `use_declaration` / inline `mod_item` arms (#61). The Imports edge carries the untruncated `use`
/// text (so the crate-aware scope re-parses every braced leaf, #97) plus the enclosing scope range
/// and module id in the DEDICATED `import_scope_*` / `import_mod_id` columns — never the `callee_*`
/// columns, which stay NULL on file-level edges so the oracle join is unaffected.
#[allow(clippy::too_many_arguments)]
pub(crate) fn file_edge_scoped(
    path: &Path,
    node: Node<'_>,
    to_name: String,
    evidence: Option<String>,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
    import_scope: Option<ImportScopeRange>,
) -> EdgeCandidate {
    EdgeCandidate {
        from_symbol_id: None,
        from_name: Some(path.to_string_lossy().replace('\\', "/")),
        to_name,
        target_qualified_name: None,
        evidence,
        receiver_hint: None,
        source_span: span_for_node(node),
        callee_span: None,
        import_scope,
        edge_kind,
        confidence,
    }
}
/// The module-aware scope of a Rust `use_declaration` (#61): walk ancestors to the nearest body
/// that resets import scope. A nested `mod m { … }`'s `declaration_list` is the scope AND its start
/// byte is the enclosing-module id. A `block` (fn body / inner block) does NOT reset import scope
/// (uses are inherited), but a block-local `use` is itself confined to the block — so a `block`
/// hit narrows the SCOPE to the block range while the module id is taken from the block's own
/// nearest-enclosing module. A top-level `use` has no such ancestor: it scopes the whole file with
/// `MOD_FILE_ROOT`. Rust `use` items are order-independent within their scope, so the WHOLE body
/// range (not "from the `use` onward") is correct.
pub(crate) fn enclosing_use_scope(node: Node<'_>, text: &str) -> ImportScopeRange {
    let mut parent = node.parent();
    let mut block_scope: Option<(usize, usize)> = None;
    while let Some(ancestor) = parent {
        match ancestor.kind() {
            // The nearest module body: it both bounds a module-level `use` and supplies the module
            // id for a block-local one (the first block we passed through narrowed the scope).
            "declaration_list" if ancestor.parent().is_some_and(|p| p.kind() == "mod_item") => {
                let mod_start = i64::try_from(ancestor.start_byte()).unwrap_or(MOD_FILE_ROOT);
                let (scope_start, scope_end) =
                    block_scope.unwrap_or((ancestor.start_byte(), ancestor.end_byte()));
                return ImportScopeRange { scope_start, scope_end, mod_id: mod_start };
            },
            // Record only the INNERMOST (first-seen) block as the confining scope; outer blocks
            // don't widen it. Don't return yet — keep walking for the enclosing module id.
            "block" if block_scope.is_none() => {
                block_scope = Some((ancestor.start_byte(), ancestor.end_byte()));
            },
            _ => {},
        }
        parent = ancestor.parent();
    }
    // No enclosing module: top-level `use` (or block-local at file root). Scope is the block if we
    // saw one, else the whole file; module id is the file root.
    let (scope_start, scope_end) = block_scope.unwrap_or((0, text.len()));
    ImportScopeRange { scope_start, scope_end, mod_id: MOD_FILE_ROOT }
}
/// The body range of an INLINE `mod foo { … }` as an import scope whose `mod_id` is its own body
/// start — so resolution rebuilds the per-file module interval set from these edges (#61). `None`
/// for a non-inline `mod foo;` (no body, introduces no scope).
pub(crate) fn inline_mod_scope(node: Node<'_>) -> Option<ImportScopeRange> {
    let body = node.child_by_field_name("body")?;
    let scope_start = body.start_byte();
    Some(ImportScopeRange {
        scope_start,
        scope_end: body.end_byte(),
        mod_id: i64::try_from(scope_start).unwrap_or(MOD_FILE_ROOT),
    })
}
pub(crate) fn symbol_edge(
    symbols: &[IndexedSymbol],
    node: Node<'_>,
    to_name: String,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
    callee_span: Option<CalleeRange>,
) -> EdgeCandidate {
    symbol_edge_with_context(
        symbols,
        node,
        "",
        to_name,
        edge_kind,
        confidence,
        EdgeContext::default(),
        callee_span,
    )
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn symbol_edge_with_context(
    symbols: &[IndexedSymbol],
    node: Node<'_>,
    text: &str,
    to_name: String,
    edge_kind: EdgeKind,
    confidence: EdgeConfidence,
    context: EdgeContext,
    // Byte range of the callee identifier token (the final `::`/`.` segment), or `None` when no
    // clean identifier node is available. `node` here is the whole call/reference expression, so
    // it can't be used for this — the caller locates the identifier via the `*_node` helpers
    // and passes its range. See [`CalleeRange`] and #67.
    callee_span: Option<CalleeRange>,
) -> EdgeCandidate {
    let byte = node.start_byte();
    let source = containing_symbol(symbols, byte);
    EdgeCandidate {
        from_symbol_id: source.map(|symbol| symbol.id),
        from_name: source.map(|symbol| symbol.qualified_name.clone()),
        to_name,
        target_qualified_name: context.target_qualified_name,
        evidence: (!text.is_empty()).then(|| edge_evidence(node, text)),
        receiver_hint: context.receiver_hint,
        source_span: span_for_node(node),
        callee_span,
        import_scope: None,
        edge_kind,
        confidence,
    }
}
