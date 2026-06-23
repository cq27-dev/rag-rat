use super::*;

mod python;

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
        ParserKind::Python => tree_sitter_python::LANGUAGE.into(),
        ParserKind::Markdown => return Ok(Vec::new()),
    };
    // Cancel/abandon a pathological parse (a grammar-ambiguity blowup, e.g. some Kotlin files)
    // rather than hang the indexer forever — a timed-out file yields no edges, same as a parse
    // failure (#210).
    let Some(tree) = crate::index::parser::parse_within_budget(
        grammar,
        text,
        crate::index::parser::PARSE_BUDGET,
    ) else {
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
        Language::Python => python::python_edges(text, node, symbols, path, out),
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
            // #200 dispatch construct fact: a tuple enum-variant construction `Enum::Variant(..)`.
            // Key off the FULL call path's last two PascalCase `::` segments, so
            // `crate::m::Msg::Start(..)` still yields `Msg::Start` (the bare receiver would be
            // `crate`). `Foo::new()` / `T::CONST` are excluded (tail not PascalCase).
            if let Some(key) =
                node.child_by_field_name("function").and_then(|f| enum_variant_key(f, text))
            {
                out.push(dispatch_fact(
                    symbols,
                    node,
                    key,
                    EdgeKind::DispatchConstruct,
                    EdgeContext::default(),
                    None,
                ));
            }
        },
        "struct_expression" =>
        // #200 dispatch construct fact: a struct enum-variant construction `Enum::Variant { .. }`.
        {
            if let Some(key) =
                node.child_by_field_name("name").and_then(|n| enum_variant_key(n, text))
            {
                out.push(dispatch_fact(
                    symbols,
                    node,
                    key,
                    EdgeKind::DispatchConstruct,
                    EdgeContext::default(),
                    None,
                ));
            }
        },
        "scoped_identifier" =>
        // #200 dispatch construct fact: a UNIT enum-variant `Enum::Stop` in a VALUE position — a
        // call argument (`send(Msg::Stop)`), a `let` initializer (`let m = Msg::Stop;`), an
        // assignment RHS, or a `return`/`break` value. Not a `call_expression`, so the arms above
        // miss it. The value-position gate keeps an ordinary type/module/use path out;
        // over-emitting for a non-enum `Foo::Bar` is harmless — synthesis only joins a
        // variant whose head is a unique in-scope `enum`, so a non-enum head never yields a
        // `dispatches` edge.
        {
            if scoped_identifier_in_value_position(node)
                && let Some(key) = enum_variant_key(node, text)
            {
                out.push(dispatch_fact(
                    symbols,
                    node,
                    key,
                    EdgeKind::DispatchConstruct,
                    EdgeContext::default(),
                    None,
                ));
            }
        },
        "match_arm" => rust_dispatch_handle_facts(text, node, symbols, out),
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

/// PascalCase test for the enum/variant convention (#200): first char uppercase AND at least one
/// lowercase — so `MlReq`/`Upsert` qualify but `new`, a SCREAMING `CONST`, and a bare `T` do not.
fn is_pascal_case(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase) && name.chars().any(char::is_lowercase)
}

/// The `Enum::Variant` dispatch key from a scoped path node — its last two `::`-segments when BOTH
/// are PascalCase, else `None`. Robust to longer paths (`crate::ml::MlReq::Upsert` →
/// `MlReq::Upsert`) so a construction site and a handler arm produce the SAME key regardless of
/// import depth (#200). Splits the node TEXT on `::` rather than counting identifier children —
/// tree-sitter treats a `scoped_(type_)identifier` as a single identifier token, so
/// `identifiers_under` returns the whole path as one element.
///
/// `Self::Variant` is rewritten to `<impl type>::Variant` using the enclosing `impl` block — `Self`
/// is NOT a stable cross-file identity, so two unrelated enums each writing `Self::Ripe` would
/// otherwise collapse to one key and cross-link (the actor pattern `impl MlReq { fn enqueue() {
/// send(Self::Upsert) } }` is exactly this). With no enclosing impl type, a `Self`-headed key is
/// dropped rather than admitted under the bare `Self` head.
fn enum_variant_key(node: Node<'_>, text: &str) -> Option<String> {
    let full = node.utf8_text(text.as_bytes()).ok()?;
    let segments: Vec<&str> =
        full.split("::").map(str::trim).filter(|segment| !segment.is_empty()).collect();
    let n = segments.len();
    if n < 2 || !is_pascal_case(segments[n - 1]) {
        return None;
    }
    let variant = segments[n - 1];
    let head = segments[n - 2];
    let head = if head == "Self" {
        enclosing_impl_type_name(node, text)?
    } else if is_pascal_case(head) {
        head.to_string()
    } else {
        return None;
    };
    Some(format!("{head}::{variant}"))
}

/// The base type name of the nearest enclosing `impl` block (`impl MlReq` / `impl Trait for MlReq`
/// / `impl Foo<T>` → `MlReq` / `Foo`), or `None` when `node` is not inside an impl. Used to resolve
/// a `Self`-headed dispatch key (#200 adversarial review). Strips generics and any module path.
fn enclosing_impl_type_name(node: Node<'_>, text: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "impl_item" {
            let type_node = ancestor.child_by_field_name("type")?;
            let raw = type_node.utf8_text(text.as_bytes()).ok()?;
            let base = raw.split('<').next().unwrap_or(raw).trim();
            let last = base.rsplit("::").next().unwrap_or(base).trim();
            return (!last.is_empty()).then(|| last.to_string());
        }
        current = ancestor.parent();
    }
    None
}

/// Every TOP-LEVEL `Enum::Variant` key carried by a `match` arm pattern (#200), paired with the
/// scoped-path NODE it came from, deduped by key. Each OR-alternative contributes ONLY its outer
/// constructor — `Msg::Start | Msg::Resume` yields both, but `Msg::Wrapped(Inner::Start)` yields
/// only `Msg::Wrapped` (the nested payload `Inner::Start` is NOT a handled variant; including it
/// would let an unrelated `Inner::Start` constructor be reported as a dispatch caller). The node is
/// returned so each emitted handle fact anchors at its OWN variant span: the full-rebuild insert
/// dedups on `(from, to_name, kind, span)` (NOT evidence), so two facts for the same delegate call
/// would otherwise collapse to one. Empty for a non-enum pattern.
fn pattern_enum_variant_keys<'a>(pattern: Node<'a>, text: &str) -> Vec<(String, Node<'a>)> {
    // Unwrap `match_pattern` (which also wraps any `if`-guard) to the actual pattern / or_pattern.
    let inner = if pattern.kind() == "match_pattern" {
        let mut cursor = pattern.walk();
        pattern.named_children(&mut cursor).next().unwrap_or(pattern)
    } else {
        pattern
    };
    let mut alternatives = Vec::new();
    if inner.kind() == "or_pattern" {
        let mut cursor = inner.walk();
        alternatives.extend(inner.named_children(&mut cursor));
    } else {
        alternatives.push(inner);
    }
    let mut keys: Vec<(String, Node<'a>)> = Vec::new();
    for alternative in alternatives {
        if let Some((key, node)) = alternative_constructor_key(alternative, text)
            && !keys.iter().any(|(existing, _)| existing == &key)
        {
            keys.push((key, node));
        }
    }
    keys
}

/// The outer-constructor `Enum::Variant` key (+ its node) of ONE match-arm alternative — the type
/// of a tuple/struct variant pattern, or a bare unit-variant path. `None` for a non-enum
/// alternative (a wildcard, literal, or binding). Does NOT look inside the pattern, so a nested
/// payload variant is not mistaken for the handled variant (#200 review).
fn alternative_constructor_key<'a>(
    alternative: Node<'a>,
    text: &str,
) -> Option<(String, Node<'a>)> {
    let path = match alternative.kind() {
        "tuple_struct_pattern" | "struct_pattern" => alternative.child_by_field_name("type")?,
        "scoped_identifier" | "scoped_type_identifier" => alternative,
        _ => return None,
    };
    enum_variant_key(path, text).map(|key| (key, path))
}

/// The DELEGATE handler call(s) a `match` arm routes to (#200/#207/#208). Traces the calls whose
/// result becomes the arm's RESPONSE through a CLOSED set of value adapters (wrappers,
/// constructors, containers, field/index/cast projections) and SIMPLE `let` bindings.
///
/// Deliberately CONSERVATIVE rather than a full dataflow analysis (#208 review, ~7 rounds): a
/// precise answer needs Rust value-provenance + name-resolution + mutation tracking, which leaks at
/// every un-modeled construct. Instead, anything not in the closed model emits NOTHING (a missed
/// edge, never a false one):
/// - if the arm REBINDS a local anywhere (`x = ..` / `(x, _) = ..`), bail entirely — a mutated
///   binding's value can't be tracked path-sensitively, so no edge is synthesized for the arm;
/// - only a plain `let x = value` maps `x` to its producer; any destructuring `let` invalidates its
///   bindings (can't attribute which producer feeds which name);
/// - `if`/`match` contribute only branch RESULTS (never a condition/scrutinee); a single match-arm
///   payload binding inherits the scrutinee's producer; a side-effect statement / struct field
///   LABEL / nested handler ARGUMENT is never a handler.
fn collect_handler_calls<'a>(node: Node<'a>, text: &str, out: &mut Vec<Node<'a>>) {
    if arm_rebinds_local(node) {
        return;
    }
    result_handler_calls(node, text, &std::collections::HashMap::new(), out);
}

/// Whether the arm body MUTATES a local through any `=`/`op=` whose target subtree contains an
/// identifier — anywhere under `node`. That covers an identifier (`resp = ..`), a destructuring
/// tuple/array/struct/tuple-struct (`(resp,_) = ..`, `Out { resp } = ..`), a wrapped form
/// (`(resp) = ..`, `*p = ..`), AND a field/index store on a local (`r.id = ..`, `buf[0] = ..` —
/// which can stale a returned `r.id`/`buf[0]` projection, #208 review round 10). Any of these can
/// make a `let` binding's stored producer stale in ways that need real control-flow / scope
/// dataflow, so the whole arm BAILS rather than risk a stale/false handler edge. Conservative: a
/// field store on a non-returned place (`self.metric = ..`) also bails — accepted recall.
fn arm_rebinds_local(node: Node<'_>) -> bool {
    if matches!(node.kind(), "assignment_expression" | "compound_assignment_expr")
        && let Some(left) = node.child_by_field_name("left")
        && subtree_has_identifier(left)
    {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(arm_rebinds_local)
}

/// Whether `node`'s subtree contains an `identifier` — used to decide if an assignment target can
/// rebind/stale a local (see [`arm_rebinds_local`]).
fn subtree_has_identifier(node: Node<'_>) -> bool {
    if node.kind() == "identifier" {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(subtree_has_identifier)
}

/// See [`collect_handler_calls`]. `scope` maps an in-scope binding name to the ALREADY-RESOLVED
/// handler calls its current value contributes (built in declaration order; a later `let` or
/// assignment of the same name replaces the entry). A binding read just contributes its stored
/// handlers — there is no binding→binding recursion, hence no depth/cycle guard.
fn result_handler_calls<'a>(
    node: Node<'a>,
    text: &str,
    scope: &std::collections::HashMap<String, Vec<Node<'a>>>,
    out: &mut Vec<Node<'a>>,
) {
    match node.kind() {
        "call_expression" => match classify_call(node, text) {
            // The handler — recorded, args NOT descended. (A method call on a scoped binding —
            // `v.into()`, `worker.run()` — is recorded too: a real handler-method like `worker.run`
            // resolves and surfaces, while a pure adapter like `v.into`/`v.clone` is a std trait
            // method that doesn't resolve to an in-corpus symbol, so it's dropped at resolution and
            // creates no edge — #208 review round 11.)
            CallRole::Delegate => out.push(node),
            CallRole::Skip => {},
            CallRole::Wrapper =>
            // A transparent wrapper / variant constructor returns its SINGLE wrapped value, so the
            // handler is whatever produced it. Descend only the lone payload arg — with MULTIPLE
            // args we can't tell which is the response (`Resp::X(handler(), metric())`). Comments
            // are NAMED children, so filter them before counting.
                if let Some(args) = node.child_by_field_name("arguments") {
                    let mut cursor = args.walk();
                    let arguments: Vec<Node<'a>> = args
                        .named_children(&mut cursor)
                        .filter(|arg| !matches!(arg.kind(), "line_comment" | "block_comment"))
                        .collect();
                    if let [only] = arguments.as_slice() {
                        result_handler_calls(*only, text, scope, out);
                    }
                },
        },
        "identifier" => {
            // A value-position read of a binding contributes its already-resolved handlers.
            if let Ok(name) = node.utf8_text(text.as_bytes())
                && let Some(handlers) = scope.get(name)
            {
                out.extend(handlers.iter().copied());
            }
        },
        "block" => {
            // Skip comments: tree-sitter exposes `line_comment`/`block_comment` as NAMED children,
            // so a trailing comment would otherwise masquerade as the block's tail expression.
            let mut cursor = node.walk();
            let children: Vec<Node<'a>> = node
                .named_children(&mut cursor)
                .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
                .collect();
            let Some((tail, statements)) = children.split_last() else {
                return;
            };
            // Resolve `let` bindings in declaration order. Only a PLAIN `let x = value` maps `x` to
            // its producer; a destructuring `let` (`let (a, b) = ..`, `let Out { x } = ..`) can't
            // attribute which producer feeds which binding, so it INVALIDATES its names. (The arm
            // has already been checked free of reassignment by `arm_rebinds_local`, so
            // a binding's mapped value is final.)
            let mut local = scope.clone();
            for statement in statements {
                if statement.kind() != "let_declaration" {
                    continue; // a bare side-effect statement is not a handler source
                }
                let Some(pattern) = statement.child_by_field_name("pattern") else {
                    continue;
                };
                if let Some(name) = simple_binding_name(pattern, text) {
                    let mut handlers = Vec::new();
                    if let Some(value) = statement.child_by_field_name("value") {
                        result_handler_calls(value, text, &local, &mut handlers);
                    }
                    local.insert(name, handlers);
                } else {
                    let mut names = Vec::new();
                    pattern_binding_names(pattern, text, &mut names);
                    for name in names {
                        local.remove(&name);
                    }
                }
            }
            let before = out.len();
            if tail.kind() != "let_declaration" {
                result_handler_calls(*tail, text, &local, out);
            }
            // EFFECT-ONLY fallback (#208, held feedback): a command/ack handler does its work in a
            // `?`-propagated side-effecting call and returns a FIXED value (`{ self.diarize(..)?;
            // Ok(Resp::Done) }`), so the value-trace above found nothing. Record the LAST `?`-stmt
            // whose payload is a DIRECT delegate call (`<call>.await?` / `<call>?`). Recording the
            // call DIRECTLY — not via scope-traced `result_handler_calls` — avoids resolving a
            // `let`-bound `?` (`task?`) against the final block scope, which a later shadowing
            // `let` could redirect to the wrong producer (#208 review round 11). The
            // `?` gate + direct-call requirement excludes fire-and-forget side effects
            // (`metrics::inc();`).
            if out.len() == before {
                for statement in children.iter().rev() {
                    if statement.kind() == "expression_statement"
                        && let Some(try_expr) = statement.named_child(0)
                        && try_expr.kind() == "try_expression"
                        && let Some(call) = unwrap_to_call(try_expr)
                        && matches!(classify_call(call, text), CallRole::Delegate)
                    {
                        out.push(call);
                        break;
                    }
                }
            }
        },
        "if_expression" => {
            // Branch RESULTS only — the `condition` field (a guard/scrutinee) is never a handler.
            // EXCEPT an `if let Pat = value` condition: its payload bindings are projections of
            // `value`, so the CONSEQUENCE inherits the value's handlers (like a match arm, #208).
            let consequence_scope = match node.child_by_field_name("condition") {
                Some(condition) if condition.kind() == "let_condition" => {
                    let mut scrutinee_handlers = Vec::new();
                    if let Some(value) = condition.child_by_field_name("value") {
                        result_handler_calls(value, text, scope, &mut scrutinee_handlers);
                    }
                    let mut inner = scope.clone();
                    if let Some(pattern) = condition.child_by_field_name("pattern") {
                        let mut bound = Vec::new();
                        pattern_binding_names(pattern, text, &mut bound);
                        bound.sort();
                        bound.dedup();
                        match bound.as_slice() {
                            [name] => {
                                inner.insert(name.clone(), scrutinee_handlers);
                            },
                            _ =>
                                for name in bound {
                                    inner.remove(&name);
                                },
                        }
                    }
                    inner
                },
                _ => scope.clone(),
            };
            if let Some(consequence) = node.child_by_field_name("consequence") {
                result_handler_calls(consequence, text, &consequence_scope, out);
            }
            if let Some(alternative) = node.child_by_field_name("alternative") {
                result_handler_calls(alternative, text, scope, out);
            }
        },
        "match_expression" => {
            // Each arm's RESULT only — never the scrutinee directly. An arm's pattern bindings are
            // projections of the SCRUTINEE, so they inherit the scrutinee's resolved handlers (a
            // returned payload `match load()? { Some(v) => Ok(Wrap(v)) }` traces `v` back to
            // `load`). This also overrides any outer `let` of the same name, so a
            // payload `Some(value)` never resolves to an unrelated outer `let value`
            // (#208 review).
            let scrutinee_handlers = match node.child_by_field_name("value") {
                Some(scrutinee) => {
                    let mut handlers = Vec::new();
                    result_handler_calls(scrutinee, text, scope, &mut handlers);
                    handlers
                },
                None => Vec::new(),
            };
            if let Some(body) = node.child_by_field_name("body") {
                let mut arm_cursor = body.walk();
                for arm in body.named_children(&mut arm_cursor) {
                    if arm.kind() == "match_arm"
                        && let Some(value) = arm.child_by_field_name("value")
                    {
                        let mut arm_scope = scope.clone();
                        if let Some(pattern) = arm.child_by_field_name("pattern") {
                            let mut bound = Vec::new();
                            pattern_binding_names(pattern, text, &mut bound);
                            // An or-pattern repeats the same binding per alternative
                            // (`Ok(v) | Err(v)`); dedup so it counts as the single projected
                            // payload.
                            bound.sort();
                            bound.dedup();
                            match bound.as_slice() {
                                // A single payload binding IS the projected scrutinee value —
                                // inherit its handlers. Multiple
                                // bindings can't each be the whole scrutinee
                                // (`(resp, _span)` would credit every binding with every producer),
                                // so mask them (#208 review).
                                [name] => {
                                    arm_scope.insert(name.clone(), scrutinee_handlers.clone());
                                },
                                _ =>
                                    for name in bound {
                                        arm_scope.remove(&name);
                                    },
                            }
                        }
                        result_handler_calls(value, text, &arm_scope, out);
                    }
                }
            }
        },
        "struct_expression" => {
            // Trace field VALUES and shorthand reads (`Resp { vector }`), never field LABELS
            // (`Resp { status: .. }` must not match a `status` local). ONLY when there is exactly
            // one field value — a multi-field struct can't attribute which field is the
            // returned response (`Resp { ok: handler(), metric: m() }`), so emit
            // nothing (no false edge, #208 review).
            let mut cursor = node.walk();
            let Some(fields) =
                node.named_children(&mut cursor).find(|c| c.kind() == "field_initializer_list")
            else {
                return;
            };
            let mut field_cursor = fields.walk();
            let values: Vec<Node<'a>> = fields
                .named_children(&mut field_cursor)
                .filter(|f| {
                    matches!(
                        f.kind(),
                        "field_initializer"
                            | "shorthand_field_initializer"
                            | "base_field_initializer"
                    )
                })
                .collect();
            if let [field] = values.as_slice() {
                match field.kind() {
                    "field_initializer" =>
                        if let Some(value) = field.child_by_field_name("value") {
                            result_handler_calls(value, text, scope, out);
                        },
                    _ => {
                        let mut inner_cursor = field.walk();
                        for inner in field.named_children(&mut inner_cursor) {
                            result_handler_calls(inner, text, scope, out);
                        }
                    },
                }
            }
        },
        "index_expression" => {
            // A projection `r[i]` of a result — trace ONLY the indexed receiver (`r`), never the
            // index expression (`choose_index()` selects, it doesn't produce the response).
            let mut cursor = node.walk();
            if let Some(receiver) = node.named_children(&mut cursor).next() {
                result_handler_calls(receiver, text, scope, out);
            }
        },
        "tuple_expression" | "array_expression" => {
            // A SINGLE-element container is a transparent wrapper (`(x,)`, `[x]`); a multi-element
            // one can't attribute which element is the returned response (`(handler(), metric())`),
            // so emit nothing rather than credit a discarded sibling (#208 review).
            let mut cursor = node.walk();
            let elements: Vec<Node<'a>> = node.named_children(&mut cursor).collect();
            if let [only] = elements.as_slice() {
                result_handler_calls(*only, text, scope, out);
            }
        },
        // Single-value projections/wrappers of a result (`&x`, `r.id`, `r as u32`, `*r`; the
        // `field_identifier`/type is not an `identifier`, so it contributes nothing), plus
        // control-flow and postfix wrappers — trace their children.
        "reference_expression"
        | "field_expression"
        | "type_cast_expression"
        | "unary_expression"
        | "else_clause"
        | "expression_statement"
        | "return_expression"
        | "break_expression"
        | "parenthesized_expression"
        | "unsafe_block"
        | "try_expression"
        | "await_expression" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                result_handler_calls(child, text, scope, out);
            }
        },
        _ => {},
    }
}

/// The single identifier a PLAIN `let` pattern binds (`let x` / `let mut x`), or `None` for a
/// destructuring pattern (which is invalidated, not mapped — see [`collect_handler_calls`]).
fn simple_binding_name(pattern: Node<'_>, text: &str) -> Option<String> {
    let identifier = match pattern.kind() {
        "identifier" => pattern,
        // `let mut x`: the `mut_pattern`'s first named child is the `mutable_specifier`, so find
        // the identifier child rather than taking `named_child(0)` (#208 review round 10).
        "mut_pattern" => {
            let mut cursor = pattern.walk();
            pattern.named_children(&mut cursor).find(|node| node.kind() == "identifier")?
        },
        _ => return None,
    };
    identifier.utf8_text(text.as_bytes()).ok().map(str::to_string)
}

/// Unwrap `?` / `.await` / parentheses to the underlying `call_expression`, for the effect-only
/// fallback (`<call>.await?` / `<call>?`). `None` if the payload isn't a direct call (e.g. `task?`
/// awaits a bound value — not a call — and must NOT be resolved against the block scope, #208
/// review round 11).
fn unwrap_to_call(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "call_expression" => Some(node),
        "try_expression" | "await_expression" | "parenthesized_expression" =>
            node.named_child(0).and_then(unwrap_to_call),
        _ => None,
    }
}

/// The local variable names a `let` / `match`-arm pattern BINDS — snake_case `identifier` leaves
/// and struct-pattern shorthands — excluding field labels (`field_identifier`), type/variant paths
/// (PascalCase identifiers, scoped paths), and literals. Used to map destructuring bindings to
/// their producer and to mask match-arm payloads so they don't resolve to an outer `let` (#208
/// review).
fn pattern_binding_names(pattern: Node<'_>, text: &str, out: &mut Vec<String>) {
    match pattern.kind() {
        "shorthand_field_identifier" =>
            if let Ok(name) = pattern.utf8_text(text.as_bytes()) {
                out.push(name.to_string());
            },
        "identifier" =>
        // A binding is snake_case / `_`-led; a PascalCase identifier pattern is a unit variant or
        // const, which binds nothing.
        {
            if let Ok(name) = pattern.utf8_text(text.as_bytes())
                && name.chars().next().is_some_and(|c| c == '_' || c.is_lowercase())
            {
                out.push(name.to_string());
            }
        },
        "match_pattern" => {
            // A guarded arm's `pattern` is `match_pattern` = the pattern + an `if <guard>` whose
            // `condition` holds READS, not bindings — recurse the pattern, skip the guard (#208).
            let guard = pattern.child_by_field_name("condition");
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                if Some(child) != guard {
                    pattern_binding_names(child, text, out);
                }
            }
        },
        "tuple_struct_pattern" | "struct_pattern" => {
            // Skip the `type` path qualifier (`status::Ready(v)` / `Out { .. }`) — a lowercase
            // module segment there is NOT a binding and must not mask an outer `let` of
            // that name (#208).
            let type_field = pattern.child_by_field_name("type");
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                if Some(child) != type_field {
                    pattern_binding_names(child, text, out);
                }
            }
        },
        // A bare `scoped_identifier` pattern is a UNIT-VARIANT / const path (`status::Ready`,
        // `Mod::CONST`); its segments are a qualifier + variant, never bindings (#208 review).
        "scoped_identifier" => {},
        _ => {
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                pattern_binding_names(child, text, out);
            }
        },
    }
}

/// How `result_handler_calls` treats a `call_expression` (#200/#208 — one classifier so the
/// delegate/wrapper decision can't disagree with itself, as it did across review rounds):
/// - `Delegate`: RECORD it as the handler (a free fn `run`, a method `self.embed`, a module-pathed
///   fn `crate::ml::embed::embed_text`).
/// - `Wrapper`: a TRANSPARENT wrapper / variant constructor whose single argument IS the response —
///   `Ok`/`Some` and ANY PascalCase-tail ctor (`MlResp::Embedded`, `dto::Wrapped`, bare `Wrapped`).
///   Trace its lone payload argument.
/// - `Skip`: emit nothing — `Err`/`None` (error/absence payload), a snake-tail `Type::assoc`
///   constructor (`Vec::with_capacity`, `Resp::empty` — its arg configures, isn't the response), or
///   a UFCS associated call (`<Resp as Default>::default()`).
///
/// Classification is by the path TAIL (constructor names are PascalCase; fns/methods are
/// snake_case), which is receiver-agnostic — so `dto::Wrapped` and `Resp::Embedded` are both
/// wrappers. Accepted recall: a bare PascalCase FFI fn (`CreateFileW`) reads as a wrapper (traced
/// through, not recorded) — there is no extraction-time signal vs a tuple-struct ctor, and
/// recording it risks crediting a constructor as a handler (a false edge), which the contract
/// forbids.
enum CallRole {
    Delegate,
    Wrapper,
    Skip,
}

fn classify_call(call: Node<'_>, text: &str) -> CallRole {
    let Some(function) = call.child_by_field_name("function") else {
        return CallRole::Delegate;
    };
    let Ok(raw) = function.utf8_text(text.as_bytes()) else {
        return CallRole::Delegate;
    };
    let raw = raw.trim();
    // A LEADING `<...>` is a UFCS qualifier (`<Resp as Default>::default()`), an
    // associated/constructor call — never a handler, and its arg (if any) isn't the response.
    if raw.starts_with('<') {
        return CallRole::Skip;
    }
    let stripped = strip_generics(raw);
    let segments: Vec<&str> =
        stripped.split("::").map(str::trim).filter(|segment| !segment.is_empty()).collect();
    let Some(tail) = segments.last() else {
        return CallRole::Delegate;
    };
    if matches!(*tail, "Err" | "None") {
        return CallRole::Skip;
    }
    if matches!(*tail, "Ok" | "Some") || is_pascal_case(tail) {
        // A PascalCase tail is a variant / tuple-struct constructor (`Resp::Embedded`,
        // `dto::Wrapped`, bare `Wrapped`) — a transparent wrapper of its payload.
        return CallRole::Wrapper;
    }
    // snake_case tail: a `Type::assoc(..)` (PascalCase receiver) is a config-taking constructor;
    // a bare fn / method / module-pathed fn is the handler.
    match segments.as_slice() {
        [.., receiver, _last] if is_pascal_case(receiver) => CallRole::Skip,
        _ => CallRole::Delegate,
    }
}

/// Whether a `scoped_identifier` sits in an expression VALUE position where a unit enum-variant is
/// being produced (#200): a call argument, a `let` initializer, an assignment RHS, or a
/// `return`/`break` value. Excludes type/pattern/use/path positions (a `use` leaf, a `let Pat = …`
/// pattern, a `T::Assoc` type). Field-checked where the node could be on either side (let pattern
/// vs value, assignment left vs right).
fn scoped_identifier_in_value_position(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "arguments" | "return_expression" | "break_expression" => true,
        "let_declaration" => parent.child_by_field_name("value") == Some(node),
        "assignment_expression" => parent.child_by_field_name("right") == Some(node),
        _ => false,
    }
}

/// Build a dispatch FACT edge candidate (#200) whose `from_symbol` is the enclosing function. For a
/// handle fact `evidence` carries the `Enum::Variant` key (safe: `resolve_symbol` never reads
/// `evidence` for a non-import kind — only `synthesize_dispatch_edges` does), and `context` carries
/// the same `target_qualified_name` / `receiver_hint` a normal call would, so the handler resolves
/// identically to a `calls_name` (e.g. `self.handle(x)` / `Self::handle(x)` bind to the right
/// method). `to_name` is the variant key (construct) or the handler name (handle). No callee range,
/// so the SCIP oracle skips these rows.
fn dispatch_fact(
    symbols: &[IndexedSymbol],
    node: Node<'_>,
    to_name: String,
    edge_kind: EdgeKind,
    context: EdgeContext,
    evidence: Option<String>,
) -> EdgeCandidate {
    let source = containing_symbol(symbols, node.start_byte());
    EdgeCandidate {
        from_symbol_id: source.map(|symbol| symbol.id),
        from_name: source.map(|symbol| symbol.qualified_name.clone()),
        to_name,
        target_qualified_name: context.target_qualified_name,
        evidence,
        receiver_hint: context.receiver_hint,
        source_span: span_for_node(node),
        callee_span: None,
        import_scope: None,
        edge_kind,
        confidence: EdgeConfidence::NameOnly,
    }
}

/// Emit `DispatchHandle` facts for a `match_arm` whose pattern names one or more `Enum::Variant`s
/// (#200): `evidence` = the variant key, `to_name` = the arm's DELEGATING call. Conservative on
/// both ends — fires only when the pattern carries a 2-segment PascalCase enum path (an integer/`_`
/// arm yields nothing), and binds only the tail/delegate call(s) (`collect_tail_calls`), so
/// side-effect statements (`metrics::inc(); handle(x)`), nested argument calls
/// (`handle(validate(x))`), and guard/scrutinee calls (`if ready() { a() }` → `a`, never `ready`)
/// are not spurious targets. An OR-pattern arm (`A | B => handle()`) emits a fact for EACH variant.
fn rust_dispatch_handle_facts(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
) {
    let Some(pattern) = node.child_by_field_name("pattern") else {
        return;
    };
    let keys = pattern_enum_variant_keys(pattern, text);
    if keys.is_empty() {
        return;
    }
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    let mut handler_calls = Vec::new();
    collect_handler_calls(value, text, &mut handler_calls);
    for call in &handler_calls {
        let Some(handler) = call_target_name(*call, text) else {
            continue;
        };
        let context = EdgeContext {
            target_qualified_name: target_qualified_name(*call, text),
            receiver_hint: scoped_receiver_name(*call, text),
        };
        for (key, variant_node) in &keys {
            // Anchor each fact at its OWN variant-pattern node (not the shared delegate call), so
            // distinct variants of an OR-pattern arm survive the span-keyed full-rebuild dedup. The
            // handler name + call context still come from the delegate call; from_symbol resolves
            // to the same dispatcher fn either way.
            out.push(dispatch_fact(
                symbols,
                *variant_node,
                handler.clone(),
                EdgeKind::DispatchHandle,
                context.clone(),
                Some(key.clone()),
            ));
        }
    }
}
