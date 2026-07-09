use super::*;

mod c_like;
mod kotlin;
mod python;
mod rust;
mod rust_dispatch;
mod typescript;

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
/// `Contains` edges: each symbol → its immediate enclosing symbol (the smallest-span container).
///
/// Symbols come from the AST, so their byte ranges form a properly-nested forest — any two are
/// either disjoint or one contains the other, never partially overlapping, and no two distinct
/// symbols share an exact `(start, end)` span. Under that invariant the smallest-span container of
/// a symbol IS its immediate parent in the nesting forest, so a single nesting-stack sweep finds
/// every parent in O(S log S) instead of the old O(S²) per-child rescan (#519). Edges are still
/// emitted in the input's original order, so the output is byte-identical.
pub(crate) fn contains_edges(symbols: &[IndexedSymbol]) -> Vec<EdgeCandidate> {
    // Visit outer symbols before the ones they enclose: `start` ascending puts a container first,
    // and on an equal start `end` DESCENDING puts the larger (containing) span first.
    let mut order = (0..symbols.len()).collect::<Vec<_>>();
    order.sort_by(|&a, &b| {
        symbols[a]
            .start_byte
            .cmp(&symbols[b].start_byte)
            .then_with(|| symbols[b].end_byte.cmp(&symbols[a].end_byte))
    });

    // `stack` holds the currently-open enclosers, innermost on top. For each symbol we pop the
    // enclosers whose span ends before it (they can't contain it); the remaining top is its
    // immediate parent.
    let mut parent = vec![None; symbols.len()];
    let mut stack = Vec::<usize>::new();
    for &index in &order {
        let end = symbols[index].end_byte;
        while stack.last().is_some_and(|&top| symbols[top].end_byte < end) {
            stack.pop();
        }
        parent[index] = stack.last().copied();
        stack.push(index);
    }

    let mut out = Vec::new();
    for (index, child) in symbols.iter().enumerate() {
        let Some(parent) = parent[index].map(|parent_index| &symbols[parent_index]) else {
            continue;
        };
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
    out
}
pub(crate) fn syntactic_edges(
    path: &Path,
    language: Language,
    text: &str,
    symbols: &[IndexedSymbol],
) -> anyhow::Result<Vec<EdgeCandidate>> {
    // Single source of truth for the grammar mapping (#519); `None` (Markdown / no grammar) yields
    // no edges, same as the old `ParserKind::Markdown` arm.
    let Some(grammar) = parser::grammar_for(parser::parser_kind(path, language)) else {
        return Ok(Vec::new());
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
        Language::Rust => rust::rust_edges(text, node, symbols, path, out),
        Language::TypeScript => typescript::typescript_edges(text, node, symbols, path, out),
        Language::Kotlin => kotlin::kotlin_edges(text, node, symbols, path, out),
        Language::C | Language::Cpp => c_like::c_like_edges(text, node, symbols, path, out),
        Language::Python => python::python_edges(text, node, symbols, path, out),
        Language::Markdown => {},
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_edges(language, text, child, symbols, path, out);
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
mod contains_edges_tests {
    use super::*;

    fn sym(id: i64, start: usize, end: usize) -> IndexedSymbol {
        IndexedSymbol {
            id,
            file_id: 0,
            language: "rust".to_string(),
            name: format!("s{id}"),
            qualified_name: format!("s{id}"),
            scope_path: String::new(),
            kind: "function".to_string(),
            start_byte: start,
            end_byte: end,
            start_line: 0,
            end_line: 0,
        }
    }

    /// The pre-#519 O(S²) implementation: for each child, scan ALL symbols for the smallest-span
    /// symbol that encloses it. Kept verbatim so the nesting-sweep rewrite is proven to emit the
    /// same (parent → child) edges in the same order.
    fn reference(symbols: &[IndexedSymbol]) -> Vec<EdgeCandidate> {
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

    /// (parent from_symbol_id, child to_name) in emission order — the identity of a Contains edge.
    fn projection(edges: &[EdgeCandidate]) -> Vec<(Option<i64>, String)> {
        edges.iter().map(|edge| (edge.from_symbol_id, edge.to_name.clone())).collect()
    }

    fn assert_equiv(symbols: &[IndexedSymbol]) {
        assert_eq!(
            projection(&contains_edges(symbols)),
            projection(&reference(symbols)),
            "contains_edges diverged from the O(S^2) reference",
        );
    }

    #[test]
    fn a_nested_chain_links_each_symbol_to_its_immediate_parent() {
        // module(0..100) ⊃ function(10..90) ⊃ struct(40..60); each child's parent is the next
        // enclosing span, and the outermost has none.
        let symbols = [sym(1, 0, 100), sym(2, 10, 90), sym(3, 40, 60)];
        let edges = contains_edges(&symbols);
        // Emitted in original (child) array order: s2's parent (the module), then s3's parent
        // (the function).
        assert_eq!(projection(&edges), vec![
            (Some(1), "s2".to_string()), // function's parent is the module
            (Some(2), "s3".to_string()), // struct's parent is the function
        ]);
        assert_equiv(&symbols);
    }

    #[test]
    fn siblings_share_a_parent_and_the_outermost_has_none() {
        let symbols = [sym(1, 0, 100), sym(2, 10, 30), sym(3, 40, 60), sym(4, 70, 90)];
        assert_equiv(&symbols);
        assert_eq!(contains_edges(&symbols).len(), 3, "three children, one root");
    }

    #[test]
    fn a_symbol_with_no_container_emits_no_edge() {
        let symbols = [sym(1, 0, 10), sym(2, 20, 30)];
        assert!(contains_edges(&symbols).is_empty());
    }

    #[test]
    fn equal_start_container_is_the_parent() {
        // A container sharing its child's start byte (e.g. `impl X { fn y }` where the grammar
        // happened to align starts) must still be recognised as the enclosing parent.
        let symbols = [sym(1, 0, 50), sym(2, 0, 20)];
        assert_eq!(projection(&contains_edges(&symbols)), vec![(Some(1), "s2".to_string())]);
        assert_equiv(&symbols);
    }

    #[test]
    fn matches_the_reference_across_nested_and_sibling_shapes() {
        // Deep nesting plus siblings at several depths, in the (start ASC, end ASC) order both real
        // input paths produce — sweep the whole shape against the O(S²) reference.
        let symbols = [
            sym(1, 0, 200),
            sym(2, 10, 90),
            sym(3, 20, 40),
            sym(4, 50, 80),
            sym(5, 55, 60),
            sym(6, 100, 190),
            sym(7, 110, 120),
            sym(8, 130, 180),
        ];
        assert_equiv(&symbols);
    }
}
