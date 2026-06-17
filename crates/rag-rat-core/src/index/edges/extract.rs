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
            // Record an alias for the rebind ONLY when the from-import is BOTH module-bound AND
            // RELATIVE (`from .compat import X as Y`). Two gates:
            //  - module-bound: a `def`/`class`-nested import binds the alias only in that local
            //    scope, so a whole-file alias scope would rebind unrelated same-name references; it
            //    binds file-wide at top level and inside transparent `if`/`try`/`with`/`for`
            //    blocks.
            //  - relative: a relative import provably names an IN-CORPUS sibling module, so
            //    rebinding its alias to the in-corpus target is safe. An ABSOLUTE import (`from
            //    urllib3.util import Timeout as TimeoutSauce`) is usually EXTERNAL — rebinding
            //    `TimeoutSauce` → bare `Timeout` would mis-bind to a same-named LOCAL class
            //    (`requests.exceptions .Timeout`), a real precision regression measured on
            //    psf/requests (#174 review). Distinguishing absolute-in-corpus from
            //    absolute-external needs a Python package model we don't have; relative is the
            //    correct-by-construction in-corpus subset.
            // A non-recorded alias still emits its plain target Imports edge (dependency captured).
            let record_alias =
                is_python_module_bound(node) && python_from_import_is_relative(node, text);
            let import_start = node.start_byte();
            // The module root is needed to bound the alias's scope at the next module-scope
            // rebinding of the alias name (#174 review) — see `python_import_target`.
            let module_root = record_alias.then(|| python_module_root(node)).flatten();
            let module_id = module.map(|m| m.id());
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if Some(child.id()) == module_id {
                    continue;
                }
                python_import_target(
                    child,
                    text,
                    path,
                    record_alias,
                    import_start,
                    module_root,
                    out,
                );
            }
        },
        // `import <module>` / `import <module> as alias` — Imports edge to the module, not the
        // alias.
        "import_statement" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                python_import_target(child, text, path, false, node.start_byte(), None, out);
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
                    // Emit Implements ONLY for a STATIC head — a plain identifier, or an attribute
                    // whose receiver chain is all identifiers and not `self`/`cls` (`pkg.Base`),
                    // after unwrapping generic/subscript/paren wrappers (#172 review). A DYNAMIC
                    // base has no compile-time class — `factory()`,
                    // `factory().Base`, `self.Base`, `Base if flag else Other`,
                    // a lambda, … — so claiming an Implements edge would
                    // let the Python class preference mis-bind it to a same-named local class. An
                    // allowlist (vs blocklisting each dynamic form) is robust to new expression
                    // kinds.
                    //
                    // Implements targets the base HEAD's LEAF name (`Base` for
                    // `pkg.Base`/`Generic[T]` → `Generic`); ReferencesType
                    // (below) covers the head and every type argument. The edge
                    // is bare-name (no qualified context): a module-qualified base `pkg.Base`
                    // is resolved by the leaf `Base` exactly like a bare base, because a top-level
                    // Python class's `scope_path` is the bare name, not `pkg::Base`. The cost is
                    // that an EXTERNAL `pkg.Base`/bare imported base can still
                    // bind a same-named local class — the general "Python has
                    // no external-import suppression" gap (#172/#174
                    // review), which needs an in-corpus Python module model to close, not a
                    // per-base special case.
                    if let Some(head) = python_static_base_head(base, text)
                        && let Some(name) = last_identifier_text(head, text)
                    {
                        let callee = last_identifier_node(head)
                            .map(final_segment_node)
                            .map(CalleeRange::of_node);
                        out.push(symbol_edge(
                            symbols,
                            base,
                            name,
                            EdgeKind::Implements,
                            EdgeConfidence::NameOnly,
                            callee,
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
        "identifier" =>
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
        // A QUALIFIED type reference (`pkg.Account`, `a.b.C`, `Account.Inner`): record the receiver
        // AND a dotted qualified name. The receiver_hint marks it qualified (so a bare-leaf alias
        // rebind skips it — `pkg.Account` is NOT the local alias `Account`) AND lets a
        // RECEIVER-alias rebind rewrite the root: `Account.Inner` with `Account` an alias
        // for `User` resolves `User::Inner`, not bare `Inner` (#174 review). Without the
        // qualified name the rebind would have nothing to rewrite and resolution would fall
        // back to the ambiguous bare tail.
        "attribute" | "dotted_name" =>
            if let Some(name) = last_identifier_text(node, text) {
                let identifiers = identifiers_under(node, text);
                let receiver = node
                    .child_by_field_name("object")
                    .map(|object| node_text(object, text))
                    .or_else(|| identifiers.first().cloned());
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    "",
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        receiver_hint: receiver,
                        target_qualified_name: dotted_qualified_name(&identifiers),
                    },
                    last_identifier_node(node).map(final_segment_node).map(CalleeRange::of_node),
                ));
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

/// The STATIC head node of a Python base-class expression — a plain `identifier`, or an
/// `attribute`/ `dotted_name` whose receiver chain is all identifiers (`pkg.Base`) — after
/// unwrapping the wrappers `python_type_head` strips (`type`/`generic_type`/`subscript`) plus
/// parentheses. `None` for a DYNAMIC base whose runtime class isn't a compile-time name: a `call`
/// (`factory()`), an attribute off a call (`factory().Base`), a `conditional_expression` (`Base if
/// flag else Other`), a lambda, a binary operator, etc. (#172 review). Only a static head should
/// claim an `Implements` edge — an allowlist, so a NEW dynamic expression form is excluded by
/// default rather than mis-bound by the Python class preference. Bounded loop guards a pathological
/// tree.
fn python_static_base_head<'a>(base: Node<'a>, text: &str) -> Option<Node<'a>> {
    let mut node = base;
    for _ in 0..8 {
        node = match node.kind() {
            "type" | "generic_type" | "parenthesized_expression" => node.named_child(0)?,
            "subscript" => node.child_by_field_name("value")?,
            "identifier" => return Some(node),
            "attribute" | "dotted_name" =>
                return python_attribute_is_static(node, text).then_some(node),
            // call / conditional_expression / lambda / binary_operator / … → no static class.
            _ => return None,
        };
    }
    None
}

/// Whether an `attribute`/`dotted_name` chain is purely static (`pkg.Base`, `a.b.C`) — every
/// receiver is an identifier or another attribute, never a `call`/`subscript` (`factory().Base` is
/// dynamic). A `self`/`cls`-rooted chain (`self.Base`) is DYNAMIC: the base comes off the runtime
/// instance, not a compile-time class (#172 review). Bounded loop guards a pathological tree.
fn python_attribute_is_static(node: Node<'_>, text: &str) -> bool {
    let mut current = node;
    for _ in 0..16 {
        match current.kind() {
            "identifier" => return !matches!(node_text(current, text).as_str(), "self" | "cls"),
            "dotted_name" =>
                return !matches!(
                    first_identifier_text(current, text).as_deref(),
                    Some("self" | "cls")
                ),
            "attribute" => match current.child_by_field_name("object") {
                Some(object) => current = object,
                None => return false,
            },
            _ => return false,
        }
    }
    false
}

/// Emit an Imports edge for one import clause (`dotted_name` or `aliased_import`), targeting the
/// imported name's dotted tail — NEVER the `as` alias (which is a local binding, not the import). A
/// comma / parenthesized list (`from pkg import A, B`) nests its clauses under an `import_list`, so
/// recurse into that.
/// Whether a Python import statement binds its names in the MODULE namespace: true at the top level
/// and inside top-level `if`/`try`/`with`/`for` blocks (so `try: from x import Y as Z except` still
/// scopes `Z` file-wide), false inside a `def`/`class`/lambda body (those bind locally). Walks
/// ancestors to the `module` root, treating block statements as transparent (#174 review).
fn is_python_module_bound(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        match ancestor.kind() {
            "module" => return true,
            "function_definition" | "class_definition" | "lambda" => return false,
            _ => current = ancestor.parent(),
        }
    }
    false
}

/// Whether a `from … import …` statement is RELATIVE (`from . import x`, `from .compat import y`,
/// `from ..pkg import z`) rather than absolute (`from urllib3.util import …`). A relative import
/// names a sibling/parent module WITHIN the same Python package, so its alias is almost always
/// in-corpus and safe to rebind; an absolute import may be external (#174 review). Detected by a
/// `relative_import` module node, with a leading-dot text fallback for the `from . import x` shape.
///
/// LIMITATION (documented): when the index root/targets cover only a SUBpackage, a parent relative
/// import (`from ..models import X`) can still point OUTSIDE the indexed targets, and the rebind
/// would resolve the bare target against an unrelated in-corpus symbol. Closing this needs an
/// in-corpus Python module/package model (resolve the relative module to an indexed file) — the
/// same machinery the absolute-in-corpus case wants — which rag-rat does not have yet.
fn python_from_import_is_relative(node: Node<'_>, text: &str) -> bool {
    if node.child_by_field_name("module_name").is_some_and(|m| m.kind() == "relative_import") {
        return true;
    }
    node_text(node, text)
        .strip_prefix("from")
        .map(|rest| rest.trim_start())
        .is_some_and(|rest| rest.starts_with('.'))
}

/// The enclosing `module` node (Python file root), walking up from `node`. `None` only for a
/// detached node with no module ancestor.
fn python_module_root(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = Some(node);
    while let Some(ancestor) = current {
        if ancestor.kind() == "module" {
            return Some(ancestor);
        }
        current = ancestor.parent();
    }
    None
}

/// The byte at which the next MODULE-SCOPE rebinding of `name` strictly after `after_byte` takes
/// effect, or `None` (#174 review). Bounds a from-import alias's scope: Python is order-dependent,
/// so a later `name = …` / `def name` / `class name` / `type name = …` / re-import reassigns the
/// name and the alias must not rebind references past that point. Only UNCONDITIONAL top-level
/// statements (DIRECT children of the `module`) count: a rebinding inside a `def`/`class` body is
/// local, and one inside a conditional block (`if`/`try`/…) isn't guaranteed — the `try: import …
/// except: import …` fallback is left ambiguous downstream instead of shadowed by byte order.
/// Bindings BEFORE the import are excluded by `after_byte`.
fn python_next_module_binding(
    module: Node<'_>,
    name: &str,
    after_byte: usize,
    text: &str,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut cursor = module.walk();
    for child in module.named_children(&mut cursor) {
        if let Some(byte) = python_rebinding_effective_byte(child, name, text)
            && byte > after_byte
        {
            best = Some(best.map_or(byte, |current: usize| current.min(byte)));
        }
    }
    best
}

/// The byte at which a top-level statement's rebinding of `name` TAKES EFFECT (past which the alias
/// is dead), or `None` if it doesn't rebind `name` (#174 review). For an ASSIGNMENT this is the
/// statement END — the right-hand side still sees the old alias (`Account = Account()` resolves its
/// RHS to the import) — for a `def`/`class`/`type`/import it is the START (the def's own body refs
/// are a separate scope; the rare header-annotation case stays bound to the new name). A bare
/// annotation without a value (`Account: T`) records `__annotations__` but does NOT bind `name`, so
/// it is skipped. A plain `import a.b.c` binds the TOP-LEVEL `a`, never the dotted tail.
fn python_rebinding_effective_byte(node: Node<'_>, name: &str, text: &str) -> Option<usize> {
    match node.kind() {
        "function_definition" | "class_definition" | "type_alias_statement" => node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0))
            .and_then(|name_node| last_identifier_text(name_node, text))
            .filter(|defined| defined == name)
            .map(|_| node.start_byte()),
        "expression_statement" =>
            node.named_child(0).and_then(|inner| python_rebinding_effective_byte(inner, name, text)),
        "assignment"
            if node.child_by_field_name("right").is_some()
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| python_assignment_target_binds(left, name, text)) =>
            Some(node.end_byte()),
        "import_from_statement" | "import_statement" =>
            python_import_binds_name(node, name, text).then(|| node.start_byte()),
        // `del Account` at module scope removes the binding, so the alias is dead from there (#174
        // review). `del Account.attr` / `del Account[i]` do NOT unbind `Account` itself —
        // `python_assignment_target_binds` returns false for attribute/subscript targets.
        "delete_statement" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .any(|target| python_assignment_target_binds(target, name, text))
                .then(|| node.start_byte())
        },
        _ => None,
    }
}

/// Whether a target binds the bare name `name` — a plain `identifier`, a tuple/list-unpacking
/// target containing it (`name, other = …`), a starred target (`*name, rest = …`), or a `del`
/// expression list. An `attribute`/`subscript` target (`name.attr`, `name[i]`) does NOT rebind
/// `name`.
fn python_assignment_target_binds(target: Node<'_>, name: &str, text: &str) -> bool {
    match target.kind() {
        "identifier" => node_text(target, text) == name,
        "pattern_list" | "tuple_pattern" | "list_pattern" | "splat_pattern"
        | "list_splat_pattern" | "expression_list" => {
            let mut cursor = target.walk();
            target
                .named_children(&mut cursor)
                .any(|element| python_assignment_target_binds(element, name, text))
        },
        _ => false,
    }
}

/// Whether an import statement binds `name`. The bound name depends on the import FORM (#174
/// review): a `from m import T as name` / `from m import name` binds the imported LEAF, but a plain
/// `import a.b.c` binds only the TOP-LEVEL `a` (Python doesn't bind the dotted tail) — `import
/// a.b.c as name` binds the alias. So an `import other.Account` does NOT rebind `Account`.
fn python_import_binds_name(node: Node<'_>, name: &str, text: &str) -> bool {
    let from_import = node.kind() == "import_from_statement";
    let module_id = node.child_by_field_name("module_name").map(|module| module.id());
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(|child| {
        if Some(child.id()) == module_id {
            return false;
        }
        match child.kind() {
            "aliased_import" => child
                .child_by_field_name("alias")
                .and_then(|alias| last_identifier_text(alias, text))
                .is_some_and(|alias| alias == name),
            // from-import: the imported leaf (`from m import Account`). plain import: the top-level
            // segment of the dotted module path (`import other.Account` binds `other`).
            "dotted_name" if from_import =>
                last_identifier_text(child, text).is_some_and(|leaf| leaf == name),
            "dotted_name" => first_identifier_text(child, text).is_some_and(|root| root == name),
            "import_list" => python_import_binds_name(child, name, text),
            _ => false,
        }
    })
}

fn python_import_target(
    child: Node<'_>,
    text: &str,
    path: &Path,
    record_alias: bool,
    import_start: usize,
    module_root: Option<Node<'_>>,
    out: &mut Vec<EdgeCandidate>,
) {
    if child.kind() == "import_list" {
        let mut cursor = child.walk();
        for clause in child.named_children(&mut cursor) {
            python_import_target(clause, text, path, record_alias, import_start, module_root, out);
        }
        return;
    }
    // `from <module> import <target> as <alias>` — a SYMBOL alias (#174). Emit the Imports edge to
    // the target (so the in-corpus dependency is recorded) but carry the alias in `evidence` + an
    // import scope, so resolution can rebind a later `alias` reference to `target`. Recorded only
    // for a top-level import (`record_alias`, checked by the caller); `import x as m` (module
    // alias) is a qualified-resolution problem, left out of scope.
    if record_alias
        && child.kind() == "aliased_import"
        && let Some(target_node) = child.child_by_field_name("name")
        && let Some(target) = last_identifier_text(target_node, text)
        && let Some(alias_node) = child.child_by_field_name("alias")
        && let Some(alias) = last_identifier_text(alias_node, text)
    {
        // The alias binding is valid from the import until the name is REBOUND at module scope —
        // Python is order-dependent, so a later `alias = …` / `def alias` / `class alias` /
        // re-import reassigns the name and the alias must not rebind references past that point
        // (#174 review). `scope_end` is that next module-scope rebinding, else end of file. Only
        // module-scope bindings count (a binding inside a def/class body is local), and the scan is
        // ordered by byte, so a definition BEFORE the import does not shrink the scope.
        let scope_end = module_root
            .and_then(|root| python_next_module_binding(root, &alias, import_start, text))
            .unwrap_or(text.len());
        let scope =
            ImportScopeRange { scope_start: import_start, scope_end, mod_id: MOD_FILE_ROOT };
        out.push(file_edge_scoped(
            path,
            target_node,
            target,
            Some(alias),
            EdgeKind::Imports,
            EdgeConfidence::NameOnly,
            Some(scope),
        ));
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
    fn call_shaped_base_emits_no_implements() {
        // `class Sub(factory())` — a DYNAMIC base (the head is a callable, not the base class). No
        // Implements edge (the resolver's class preference would mis-bind it), but the base call is
        // still captured as a CallsName so the dependency on `factory` isn't lost (#172 review).
        let e = edges("class Sub(factory()):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "factory"),
            "a call-shaped base must NOT emit Implements: {e:?}"
        );
        assert!(
            has(&e, EdgeKind::CallsName, "factory"),
            "the base call is still captured as a CallsName: {e:?}"
        );
    }

    #[test]
    fn parenthesized_call_shaped_base_emits_no_implements() {
        // `class Sub((factory()))` — tree-sitter nests the `call` under a
        // `parenthesized_expression`, so the immediate-kind check missed it (#172 review round 2).
        // Still a DYNAMIC base: no Implements, but the call dependency is captured.
        let e = edges("class Sub((factory())):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "factory"),
            "a parenthesized call-shaped base must NOT emit Implements: {e:?}"
        );
        assert!(
            has(&e, EdgeKind::CallsName, "factory"),
            "the base call is still captured as a CallsName: {e:?}"
        );
    }

    #[test]
    fn subscript_call_shaped_base_emits_no_implements() {
        // `class Sub(factory()[T])` — tree-sitter exposes the base as a `subscript` whose value is
        // the `call`; `python_type_head` unwraps the subscript to that call, so the dynamic-base
        // check must unwrap it too (#172 review round 3). Still dynamic: no Implements.
        let e = edges("class Sub(factory()[T]):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "factory"),
            "a subscript-on-call base must NOT emit Implements: {e:?}"
        );
        assert!(
            has(&e, EdgeKind::CallsName, "factory"),
            "the base call is still captured as a CallsName: {e:?}"
        );
    }

    #[test]
    fn attribute_on_call_base_emits_no_implements() {
        // `class Sub(factory().Base)` — the base is an `attribute` whose receiver is a `call`, so
        // `Base` comes off the factory RESULT, not a static class (#172 review round 4). Dynamic:
        // no Implements; subscripted `factory().Base[T]` is the same shape under a
        // subscript.
        let e = edges("class Sub(factory().Base):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "Base"),
            "an attribute on a call result must NOT emit Implements: {e:?}"
        );
        let e = edges("class Sub(factory().Base[T]):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "Base"),
            "a subscripted attribute on a call result must NOT emit Implements: {e:?}"
        );
    }

    #[test]
    fn static_attribute_base_emits_a_bare_implements() {
        // `class Sub(pkg.Base)` — a static qualified base (receiver is a module, not a call) is a
        // real superclass; the Implements edge targets the LEAF `Base` and is BARE (no qualified
        // context) so it resolves like a bare base — a top-level Python class's `scope_path` is the
        // bare name, not `pkg::Base` (#172 review).
        let e = edges("class Sub(pkg.Base):\n    pass\n");
        let imp = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::Implements && c.to_name == "Base")
            .expect("qualified base Implements edge");
        assert_eq!(imp.receiver_hint, None, "qualified base implements is bare-name");
        assert_eq!(imp.target_qualified_name, None, "qualified base implements is bare-name");
    }

    #[test]
    fn self_qualified_base_emits_no_implements() {
        // `class Sub(self.Base)` (e.g. inside a method) — the base comes off the runtime instance,
        // not a compile-time class, so it is DYNAMIC: no Implements (#172 review).
        let e = edges(
            "class Outer:\n    def make(self):\n        class Sub(self.Base):\n            pass\n",
        );
        assert!(
            !has(&e, EdgeKind::Implements, "Base"),
            "a `self.`-rooted base must NOT emit Implements: {e:?}"
        );
    }

    #[test]
    fn conditional_expression_base_emits_no_implements() {
        // `class Sub(Base if flag else Other)` — the runtime base is expression-dependent, so it is
        // NOT a static class; emitting an Implements to the last identifier (`Other`) would let the
        // class preference mis-bind it (#172 review). The allowlist excludes the whole expression.
        let e = edges("class Sub(Base if flag else Other):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "Other"),
            "a conditional-expression base must NOT emit Implements: {e:?}"
        );
        assert!(
            !has(&e, EdgeKind::Implements, "Base"),
            "a conditional-expression base must NOT emit Implements: {e:?}"
        );
    }

    #[test]
    fn parenthesized_class_base_still_emits_implements() {
        // `class Sub((Base))` — a parenthesized but STATIC base is a real superclass, so the
        // Implements edge must survive the dynamic-base guard.
        let e = edges("class Sub((Base)):\n    pass\n");
        assert!(
            has(&e, EdgeKind::Implements, "Base"),
            "a parenthesized class base must still emit Implements: {e:?}"
        );
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

    /// The Imports edge carrying a from-import alias (`from m import User as Account`): its
    /// `to_name` is the imported target, `evidence` is the alias, and — unlike the plain dependency
    /// edge (whose evidence defaults to its own name) — it carries an import scope so resolution
    /// can rebind alias references (#174). The import scope is what marks it as an alias
    /// binding.
    fn aliased_import<'e>(edges: &'e [EdgeCandidate], target: &str) -> Option<&'e EdgeCandidate> {
        edges.iter().find(|e| {
            e.edge_kind == EdgeKind::Imports && e.to_name == target && e.import_scope.is_some()
        })
    }

    #[test]
    fn top_level_relative_from_import_alias_carries_scope_and_evidence() {
        // A module-level RELATIVE `from .models import User as Account` binds `Account` file-wide
        // and is safe to rebind (the module is provably in-corpus) (#174 review).
        let e = edges("from .models import User as Account\n");
        let imp = aliased_import(&e, "User").expect("aliased import edge for User");
        assert_eq!(imp.evidence.as_deref(), Some("Account"), "alias must ride on evidence");
        assert!(imp.import_scope.is_some(), "module-level alias must carry an import scope");
    }

    #[test]
    fn absolute_external_from_import_alias_is_not_recorded() {
        // `from urllib3.util import Timeout as TimeoutSauce` — an ABSOLUTE import, usually
        // EXTERNAL. Rebinding `TimeoutSauce` → bare `Timeout` would mis-bind to a
        // same-named LOCAL class (the measured psf/requests precision regression), so no
        // alias scope is recorded — only the plain dependency edge to the target (#174
        // review). The dotted module tail still imports.
        let e = edges("from urllib3.util import Timeout as TimeoutSauce\n");
        assert!(
            has(&e, EdgeKind::Imports, "Timeout"),
            "the dependency edge is still emitted: {e:?}"
        );
        assert!(
            aliased_import(&e, "Timeout").is_none(),
            "an absolute (external) alias must NOT carry a rebind scope: {e:?}"
        );
    }

    #[test]
    fn try_block_from_import_alias_is_module_bound() {
        // The `try: import X as Y except ImportError:` fallback pattern binds in the MODULE
        // namespace — a top-level `try` block is transparent (#174 review).
        let e = edges(
            "try:\n    from .fast import Engine as DB\nexcept ImportError:\n    from .slow import \
             Engine as DB\n",
        );
        let imp = aliased_import(&e, "Engine").expect("aliased import edge for Engine");
        assert_eq!(imp.evidence.as_deref(), Some("DB"));
        assert!(imp.import_scope.is_some(), "try-block alias must still be module-scoped");
    }

    #[test]
    fn function_nested_from_import_alias_is_not_recorded() {
        // An import inside a `def` binds the alias LOCALLY, not file-wide — recording it whole-file
        // would rebind unrelated same-name references, so no alias is carried (#174 review). The
        // plain dependency edge to the target is still emitted.
        let e =
            edges("def load():\n    from .models import User as Account\n    return Account()\n");
        assert!(
            has(&e, EdgeKind::Imports, "User"),
            "the dependency edge to the target is still emitted: {e:?}"
        );
        assert!(
            aliased_import(&e, "User").is_none(),
            "a function-nested alias must not be recorded file-wide: {e:?}"
        );
    }

    #[test]
    fn qualified_type_reference_carries_a_receiver_hint() {
        // `x: pkg.Account` is a qualified type reference — the receiver hint marks it so the alias
        // rebind skips it (`pkg.Account` is not the local alias `Account`) (#174 review).
        let e = edges("def f(x: pkg.Account) -> None:\n    pass\n");
        let ref_edge = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::ReferencesType && c.to_name == "Account")
            .expect("qualified type reference edge");
        assert_eq!(ref_edge.receiver_hint.as_deref(), Some("pkg"));
    }

    /// The alias edge's `scope_end` — where extraction decided the alias stops applying because the
    /// name is rebound at module scope (#174 review).
    fn alias_scope_end(edges: &[EdgeCandidate], target: &str) -> usize {
        aliased_import(edges, target)
            .and_then(|e| e.import_scope)
            .expect("aliased import edge with a scope")
            .scope_end
    }

    #[test]
    fn alias_scope_runs_to_eof_with_no_redefinition() {
        // No later rebinding of `Account`: the alias is in effect for the whole file.
        let src = "from .models import User as Account\nAccount()\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.len(), "scope should run to EOF");
    }

    #[test]
    fn alias_scope_ends_at_a_later_class_redefinition() {
        // `class Account` after the import reassigns the name at module scope; the alias must stop
        // there so a later `Account()` resolves to the local class, not the import.
        let src = "from .models import User as Account\nclass Account:\n    pass\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.find("class Account").unwrap());
    }

    #[test]
    fn alias_scope_ends_at_a_later_module_assignment() {
        // A plain lowercase module assignment (`Account = ...`) is NOT indexed as a symbol, so only
        // this extraction-time scan catches it — the round-2 `latest_def_before` could not. The
        // scope ends at the assignment's END, not its start, so the RHS still sees the alias
        // (`Account = Account()` resolves its RHS to the import, #174 review).
        let src = "from .models import User as Account\nAccount = make_account()\n";
        let e = edges(src);
        let stmt = "Account = make_account()";
        let expected = src.find(stmt).unwrap() + stmt.len();
        assert_eq!(
            alias_scope_end(&e, "User"),
            expected,
            "scope ends after the assignment, not before"
        );
    }

    #[test]
    fn alias_scope_ends_at_a_starred_unpacking_rebinding() {
        // `*Account, rest = rows` rebinds `Account` at module scope through a starred target (#174
        // review) — the scan must look inside `splat_pattern`/`list_splat_pattern`.
        let src = "from .models import User as Account\n*Account, rest = rows\n";
        let e = edges(src);
        let stmt = "*Account, rest = rows";
        let expected = src.find(stmt).unwrap() + stmt.len();
        assert_eq!(alias_scope_end(&e, "User"), expected, "a starred unpack must end the scope");
    }

    #[test]
    fn alias_scope_ends_at_a_module_level_del() {
        // `del Account` removes the binding, so the alias is dead from there (#174 review).
        let src = "from .models import User as Account\ndel Account\nAccount()\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.find("del Account").unwrap());
    }

    #[test]
    fn alias_scope_ignores_a_del_of_an_attribute() {
        // `del Account.cache` removes an attribute, NOT the `Account` binding — the alias survives.
        let src = "from .models import User as Account\ndel Account.cache\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "a `del` of an attribute must not end the scope"
        );
    }

    #[test]
    fn alias_scope_covers_the_rhs_of_its_own_rebinding() {
        // `Account = Account()` — the RHS `Account()` evaluates BEFORE the new binding takes
        // effect, so it must still be inside the alias scope (#174 review). The RHS call's
        // byte is < scope_end.
        let src = "from .models import User as Account\nAccount = Account()\n";
        let e = edges(src);
        let scope_end = alias_scope_end(&e, "User");
        let rhs_call = src.rfind("Account()").unwrap();
        assert!(rhs_call < scope_end, "the RHS reference must fall within the alias scope");
    }

    #[test]
    fn alias_scope_ends_at_a_later_type_alias() {
        // PEP 695 `type Account = int` rebinds the name at module scope (#174 review).
        let src = "from .models import User as Account\ntype Account = int\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.find("type Account").unwrap());
    }

    #[test]
    fn alias_scope_ignores_a_value_less_annotation() {
        // `Account: type[User]` records `__annotations__` but does NOT bind `Account` (no value),
        // so the alias is still in effect afterward (#174 review).
        let src = "from .models import User as Account\nAccount: type[User]\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "a value-less annotation must not shrink scope"
        );
    }

    #[test]
    fn alias_scope_ignores_a_plain_dotted_import() {
        // `import other.Account` binds the top-level `other`, never the dotted tail `Account`, so
        // it must not end an `Account` alias (#174 review).
        let src = "from .models import User as Account\nimport other.Account\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "a plain dotted import of the tail must not shrink scope"
        );
    }

    #[test]
    fn alias_scope_ignores_a_conditional_rebinding() {
        // A rebinding inside a top-level `if`/`try` block isn't guaranteed to execute, so it must
        // not shrink the alias scope by byte order (#174 review) — the ambiguity is left to
        // the resolver.
        let src =
            "from .models import User as Account\nif flag:\n    Account = other()\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "a conditional rebinding must not shrink scope"
        );
    }

    #[test]
    fn qualified_type_reference_carries_a_qualified_name() {
        // `x: Account.Inner` — a qualified type ref carries BOTH the receiver hint and a dotted
        // qualified name, so a receiver-alias rebind can rewrite the root to `User::Inner` (#174
        // review).
        let e = edges("def f(x: Account.Inner) -> None:\n    pass\n");
        let ref_edge = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::ReferencesType && c.to_name == "Inner")
            .expect("qualified type reference edge");
        assert_eq!(ref_edge.receiver_hint.as_deref(), Some("Account"));
        assert_eq!(ref_edge.target_qualified_name.as_deref(), Some("Account::Inner"));
    }

    #[test]
    fn alias_scope_ignores_a_nested_redefinition() {
        // A `class Account` INSIDE a function binds locally, not at module scope, so it must not
        // shrink the alias scope (the round-2 whole-symbol scan wrongly counted nested defs).
        let src =
            "from .models import User as Account\ndef f():\n    class Account:\n        pass\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.len(), "a nested redef must not shrink scope");
    }

    #[test]
    fn alias_scope_ignores_an_attribute_assignment() {
        // `Account.config = x` mutates an attribute; it does NOT rebind `Account` itself, so the
        // alias scope must not stop there.
        let src = "from .models import User as Account\nAccount.config = 1\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "an attribute set must not shrink scope"
        );
    }

    #[test]
    fn alias_scope_starts_after_a_definition_before_the_import() {
        // A `class Account` BEFORE the import does not shadow it (the import is the later binding):
        // scope_start is the import, scope_end runs to EOF.
        let src = "class Account:\n    pass\n\n\nfrom .models import User as Account\nAccount()\n";
        let e = edges(src);
        let scope = aliased_import(&e, "User").and_then(|edge| edge.import_scope).expect("scope");
        assert_eq!(
            scope.scope_start,
            src.find("from .models").unwrap(),
            "scope starts at the import"
        );
        assert_eq!(scope.scope_end, src.len(), "a redef before the import does not shrink scope");
    }
}
