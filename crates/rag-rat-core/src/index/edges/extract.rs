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
        ParserKind::Python => tree_sitter_python::LANGUAGE.into(),
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
        Language::Python => python_edges(text, node, symbols, path, out),
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
pub(crate) fn python_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        // `from <module> import <name|name as alias>, ...` — emit Imports edges to the MODULE and
        // to each imported NAME, never the local alias. A relative import (`.sessions`)
        // normalizes to its dotted tail (the leading dots aren't identifiers), so the
        // module name is recorded separately from any `as` alias.
        "import_from_statement" => {
            let module = node.child_by_field_name("module_name");
            if let Some(module) = module
                && let Some(name) = last_identifier_text(module, text)
            {
                out.push(file_edge(
                    path,
                    module,
                    text,
                    name,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
            let module_id = module.map(|m| m.id());
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if Some(child.id()) == module_id {
                    continue;
                }
                python_import_target(child, text, path, out);
            }
        },
        // `import <module>` / `import <module> as alias` — Imports edge to the module, not the
        // alias.
        "import_statement" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                python_import_target(child, text, path, out);
            }
        },
        // Function / method / constructor call. Mirror the C handler: the callee is the LAST
        // identifier under the `function` child (`f()` → `f`, `obj.method()` → `method`), the
        // receiver is the first (recorded only as a NameOnly hint — never claimed as exact;
        // resolving it is the oracle's job, not the heuristic's).
        "call" => {
            let function = node.child_by_field_name("function").unwrap_or(node);
            let identifiers = identifiers_under(function, text);
            let identifier_nodes = identifier_nodes_under(function);
            // `handlers[key]()` — the callee is the subscript RESULT, not the index variable
            // `last()` would pick. There's no clean callee identifier, so emit nothing (a wrong
            // `calls_name key` is worse than a missing edge).
            if function.kind() == "subscript" {
                // fall through to recursion without emitting a call edge
            } else if let Some(name) = identifiers.last().cloned() {
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
        },
        // `class Foo(Base, Generic[T], metaclass=Meta)` — each POSITIONAL base is an Implements +
        // ReferencesType edge. Keyword (`metaclass=`) and splat (`*bases`/`**kw`) arguments are not
        // superclasses, and a parameterized base resolves to its head (`Generic`, not `T`).
        "class_definition" =>
            if let Some(supers) = node.child_by_field_name("superclasses") {
                let mut cursor = supers.walk();
                for base in supers.named_children(&mut cursor) {
                    if matches!(base.kind(), "keyword_argument" | "list_splat" | "dictionary_splat")
                    {
                        continue;
                    }
                    // Implements points at the base HEAD only (`Generic`, not `T`); ReferencesType
                    // covers the head AND every type argument (`Mapping[str, Api]` → `Mapping`,
                    // `str`, `Api`).
                    let head = python_type_head(base);
                    if let Some(name) = last_identifier_text(head, text) {
                        out.push(symbol_edge(
                            symbols,
                            base,
                            name,
                            EdgeKind::Implements,
                            EdgeConfidence::NameOnly,
                            last_identifier_node(head)
                                .map(final_segment_node)
                                .map(CalleeRange::of_node),
                        ));
                    }
                    emit_python_type_refs(base, symbols, text, out);
                }
            },
        // Type annotations (`x: T`, `-> T`) wrap their type in a `type` node.
        // `emit_python_type_refs` walks the whole type expression — generics (`Box[Item]`),
        // qualified generics (`typing.Optional[Api]`), unions (`A | B`), nested
        // (`Optional[list[Api]]`), `Callable` param lists — emitting a ReferencesType per
        // referenced type. The alias NAME in a `type X = …` is skipped (it's a definition,
        // not a reference). String forward refs (`-> "Api"`) carry no identifier → no edge.
        "type" if !python_is_type_alias_name(node) => {
            emit_python_type_refs(node, symbols, text, out);
        },
        // A bare decorator (`@requires_auth`, `@pytest.fixture`) is an identifier/attribute, not a
        // `call` — applying it is a call-like dependency, so emit a NameOnly call edge. A
        // parenthesized decorator (`@foo(...)`) is a `call` child, already handled by the call arm
        // via recursion.
        "decorator" => {
            if let Some(inner) = node.named_child(0)
                && matches!(inner.kind(), "identifier" | "attribute")
                && let Some(name) = last_identifier_text(inner, text)
            {
                // Preserve the qualifier so a qualified decorator (`@pytest.fixture`) carries its
                // `pytest` receiver + dotted path — same context the call arm records — so the
                // resolver doesn't fall back to a bare local `fixture` of the same name.
                let identifiers = identifiers_under(inner, text);
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
                    last_identifier_node(inner).map(final_segment_node).map(CalleeRange::of_node),
                ));
            }
        },
        _ => {},
    }
}

/// Emit `ReferencesType` edges for a subscript-form generic's type arguments, RECURSIVELY so nested
/// generics (`typing.Optional[list[Api]]` → both `list` and `Api`) are all referenced. Each arg's
/// head is emitted, then its own subscript args are walked; non-subscript args terminate.
/// Emit a `ReferencesType` edge for every type referenced in a Python type expression, walking the
/// whole shape: `type`/`generic_type`/`subscript`/`binary_operator` (PEP 604 `A |
/// B`)/`list`/`tuple` (`Callable[[int, A], B]`)/`type_parameter` recurse into their children;
/// `identifier`/`attribute`/ `dotted_name` are leaf references (the dotted tail is the referenced
/// type). Anything else (string forward refs, `None`, literals) terminates with no edge. This
/// single walker handles plain, generic, qualified, union, nested, and callable annotations
/// uniformly.
fn emit_python_type_refs(
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    text: &str,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "type" | "generic_type" | "subscript" | "binary_operator" | "list" | "tuple"
        | "type_parameter" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                emit_python_type_refs(child, symbols, text, out);
            }
        },
        "identifier" | "attribute" | "dotted_name" => {
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

/// Whether `node` is the alias NAME being DEFINED in `type X = …` (the first child of a
/// `type_alias_statement`) — a definition, not a reference, so it must not emit a `ReferencesType`
/// self-edge. The value side (the second `type`) is referenced normally.
fn python_is_type_alias_name(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "type_alias_statement"
            && parent.named_child(0).map(|first| first.id()) == Some(node.id())
    })
}

/// The "head" type node to anchor a Python type reference on, for a parameterized type. Unwraps the
/// applicable wrappers to reach the base: a `type` wrapper, a `generic_type` (`Box[Item]` in an
/// annotation → base `Box`), and a `subscript` (`Generic[T]` in a base-class list → value
/// `Generic`). Plain identifiers/attributes pass through unchanged, so the type argument
/// (`Item`/`T`) is never mistaken for the base. Bounded loop guards against a pathological tree.
fn python_type_head(node: Node<'_>) -> Node<'_> {
    let mut head = node;
    for _ in 0..8 {
        head = match head.kind() {
            "type" | "generic_type" => head.named_child(0).unwrap_or(head),
            "subscript" => head.child_by_field_name("value").unwrap_or(head),
            _ => return head,
        };
    }
    head
}

/// Emit an Imports edge for one import clause (`dotted_name` or `aliased_import`), targeting the
/// imported name's dotted tail — NEVER the `as` alias (which is a local binding, not the import). A
/// comma / parenthesized list (`from pkg import A, B`) nests its clauses under an `import_list`, so
/// recurse into that.
fn python_import_target(child: Node<'_>, text: &str, path: &Path, out: &mut Vec<EdgeCandidate>) {
    if child.kind() == "import_list" {
        let mut cursor = child.walk();
        for clause in child.named_children(&mut cursor) {
            python_import_target(clause, text, path, out);
        }
        return;
    }
    let target = match child.kind() {
        "aliased_import" => child.child_by_field_name("name"),
        "dotted_name" => Some(child),
        _ => None,
    };
    if let Some(target) = target
        && let Some(name) = last_identifier_text(target, text)
    {
        out.push(file_edge(path, target, text, name, EdgeKind::Imports, EdgeConfidence::NameOnly));
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

#[cfg(test)]
mod python_edge_tests {
    use std::path::Path;

    use super::*;
    use crate::language::Language;

    fn edges(src: &str) -> Vec<EdgeCandidate> {
        // No symbol table needed: the syntactic pass emits NameOnly candidates regardless of
        // resolution, which is exactly the signal these assertions check.
        syntactic_edges(Path::new("src/Main.py"), Language::Python, src, &[]).unwrap()
    }

    fn has(edges: &[EdgeCandidate], kind: EdgeKind, name: &str) -> bool {
        edges.iter().any(|e| e.edge_kind == kind && e.to_name == name)
    }

    #[test]
    fn relative_import_normalized_and_alias_not_treated_as_import() {
        let e = edges("from .sessions import Session as ClientSession\n");
        // The relative module `.sessions` normalizes to its dotted tail; the imported symbol is
        // recorded separately.
        assert!(has(&e, EdgeKind::Imports, "sessions"), "module import missing: {e:?}");
        assert!(has(&e, EdgeKind::Imports, "Session"), "imported name missing: {e:?}");
        // The local `as` alias is NOT an import target.
        assert!(!has(&e, EdgeKind::Imports, "ClientSession"), "alias wrongly imported: {e:?}");
    }

    #[test]
    fn plain_and_dotted_imports_use_module_not_alias() {
        let e = edges("from requests.adapters import HTTPAdapter\nimport urllib3 as http\n");
        assert!(has(&e, EdgeKind::Imports, "adapters"), "dotted module tail missing: {e:?}");
        assert!(has(&e, EdgeKind::Imports, "HTTPAdapter"));
        assert!(has(&e, EdgeKind::Imports, "urllib3"), "import module missing: {e:?}");
        assert!(!has(&e, EdgeKind::Imports, "http"), "import alias wrongly recorded: {e:?}");
    }

    #[test]
    fn method_call_records_receiver_hint_at_name_only_not_exact() {
        let e = edges("def f():\n    http.disable_warnings()\n");
        let call = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::CallsName && c.to_name == "disable_warnings")
            .expect("method call edge");
        assert_eq!(call.receiver_hint.as_deref(), Some("http"));
        // The heuristic must NOT claim exact resolution for a member call — that's the oracle's
        // job.
        assert_eq!(call.confidence, EdgeConfidence::NameOnly);
    }

    #[test]
    fn alias_call_emits_name_only_call_edge() {
        // A call through an imported alias is a NameOnly call to the alias name (resolving the
        // alias to its imported symbol is the resolver/oracle's job, not the syntactic
        // pass).
        let e = edges("def make():\n    return ClientSession()\n");
        let call = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::CallsName && c.to_name == "ClientSession")
            .expect("alias call edge");
        assert_eq!(call.confidence, EdgeConfidence::NameOnly);
    }

    #[test]
    fn base_class_emits_implements_and_references_type() {
        let e = edges("class Api(Session):\n    pass\n");
        assert!(has(&e, EdgeKind::Implements, "Session"), "base class Implements missing: {e:?}");
        assert!(
            has(&e, EdgeKind::ReferencesType, "Session"),
            "base class ReferencesType missing: {e:?}"
        );
    }

    #[test]
    fn generic_base_resolves_to_base_not_type_arg() {
        // `class Repo(Generic[T])` — the base is `Generic`, NOT the type argument `T`.
        let e = edges("class Repo(Generic[T]):\n    pass\n");
        assert!(has(&e, EdgeKind::Implements, "Generic"), "base should be Generic: {e:?}");
        assert!(!has(&e, EdgeKind::Implements, "T"), "must not resolve to type arg T: {e:?}");
    }

    #[test]
    fn metaclass_keyword_argument_is_not_a_base_class() {
        // `class Model(Base, metaclass=Meta)` — `Base` is a base; `metaclass=Meta` is not.
        let e = edges("class Model(Base, metaclass=Meta):\n    pass\n");
        assert!(has(&e, EdgeKind::Implements, "Base"), "Base should be a base: {e:?}");
        assert!(
            !has(&e, EdgeKind::Implements, "Meta"),
            "metaclass kwarg must not be a base: {e:?}"
        );
    }

    #[test]
    fn generic_annotation_anchors_the_head_type() {
        // `x: Box[Item]` references `Box` (the head), which would otherwise be invisible.
        let e = edges("def f(x: Box[Item]) -> None:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "Box"), "annotation head Box missing: {e:?}");
    }

    #[test]
    fn qualified_generic_annotation_emits_head_and_arg() {
        // `x: typing.Optional[Api]` is a `subscript` — emit BOTH the head (`Optional`) and the type
        // argument (`Api`); the latter is a plain expression the recursion otherwise misses.
        let e = edges("def f(x: typing.Optional[Api]) -> None:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "Optional"), "head Optional missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "Api"), "type arg Api missing: {e:?}");
    }

    #[test]
    fn union_annotation_emits_both_operands() {
        // PEP 604 `A | B` — both operands are referenced types (was: only the last).
        let e = edges("def f(x: A | B) -> None:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "A"), "union operand A missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "B"), "union operand B missing: {e:?}");
    }

    #[test]
    fn subscript_callee_does_not_record_the_index() {
        // `handlers[key]()` — `key` is the index, not the callee; emit no bogus call edge.
        let e = edges("def f():\n    handlers[key]()\n");
        assert!(
            !has(&e, EdgeKind::CallsName, "key"),
            "index var wrongly recorded as callee: {e:?}"
        );
    }

    #[test]
    fn generic_base_class_emits_type_args() {
        // `class C(Mapping[str, Api])` — Implements the head `Mapping`, ReferencesType the arg
        // `Api`.
        let e = edges("class C(Mapping[str, Api]):\n    pass\n");
        assert!(has(&e, EdgeKind::Implements, "Mapping"), "base head Implements missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "Api"), "base type arg Api missing: {e:?}");
        assert!(!has(&e, EdgeKind::Implements, "Api"), "type arg must not be Implements: {e:?}");
    }

    #[test]
    fn type_alias_does_not_self_reference() {
        // `type UserId = int` references `int`, NOT the alias name `UserId` being defined.
        let e = edges("type UserId = int\n");
        assert!(has(&e, EdgeKind::ReferencesType, "int"), "alias value int missing: {e:?}");
        assert!(!has(&e, EdgeKind::ReferencesType, "UserId"), "alias name self-referenced: {e:?}");
    }

    #[test]
    fn nested_qualified_generic_annotation_emits_all_heads() {
        // `typing.Optional[list[Api]]` → Optional + list + the nested project type Api.
        let e = edges("def f(x: typing.Optional[list[Api]]) -> None:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "Optional"), "Optional missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "list"), "list missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "Api"), "nested Api missing: {e:?}");
    }

    #[test]
    fn bare_decorator_emits_a_call_edge() {
        // `@requires_auth` (no parens) is a dependency of the decorated symbol.
        let e = edges("@requires_auth\ndef handler():\n    pass\n");
        assert!(
            has(&e, EdgeKind::CallsName, "requires_auth"),
            "bare decorator edge missing: {e:?}"
        );
    }

    #[test]
    fn qualified_bare_decorator_keeps_its_receiver() {
        // `@pytest.fixture` records the `pytest` receiver so it can't fall back to a local
        // `fixture`.
        let e = edges("@pytest.fixture\ndef t():\n    pass\n");
        let edge = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::CallsName && c.to_name == "fixture")
            .expect("qualified decorator edge");
        assert_eq!(edge.receiver_hint.as_deref(), Some("pytest"));
    }

    #[test]
    fn multi_import_emits_each_imported_name() {
        // `from pkg import A, B` nests the names under an `import_list`.
        let e = edges("from pkg import A, B\n");
        assert!(has(&e, EdgeKind::Imports, "A"), "import A missing: {e:?}");
        assert!(has(&e, EdgeKind::Imports, "B"), "import B missing: {e:?}");
    }

    #[test]
    fn annotation_emits_references_type() {
        let e = edges("def f(url: str) -> None:\n    pass\n");
        assert!(
            has(&e, EdgeKind::ReferencesType, "str"),
            "annotation ReferencesType missing: {e:?}"
        );
    }
}
