use super::*;

// #224 ALIAS CONTRACT: symbol qualified-names are interned into `name_strings`, so these predicate
// fragments reference the joined pool value `from_qn.value` / `to_qn.value` (NOT a
// `from_symbols.qualified_name` column, which no longer exists). EVERY query that splices a
// fragment from this module MUST also provide the matching `LEFT JOIN name_strings from_qn ON
// from_qn.id = from_symbols.qualified_name_id` (and/or `to_qn`) for the alias it uses — `reverse_*`
// use `to_qn`, `forward_*` use `from_qn`. The interned-id arms (`edges.to_name_id = (SELECT id FROM
// name_strings WHERE value = ?3)`) are an edge-side pool lookup and are independent of these symbol
// joins.

/// The guard EVERY query that admits `uses_operator` edges must carry.
///
/// An UNRESOLVED `uses_operator` row is a BUILT-IN operator token — Swift emits one for every `+`,
/// `==`, `!` in the repo — not a use of an in-repo `operator` declaration. Admitting those rows
/// into a query that falls back to matching by NAME makes every `Int + Int` expression a "caller"
/// of any same-named custom operator, flooding callers/impact with dependencies that do not exist.
/// A resolved row (`to_symbol_id IS NOT NULL`) is by definition bound to a real operator symbol, so
/// requiring resolution is what separates the two.
///
/// Valid wherever the edges table is named or aliased `edges`. `forward_visibility_filter` /
/// `reverse_visibility_filter` below encode the same rule in positive form, inside their OR-chains,
/// and `graph_meta` splices this guard directly.
/// `search_and_read_chunk_attach_bounded_graph_evidence` poisons the index with one unresolved
/// operator edge and asserts EVERY consumer — search graph metadata and all three `impact_surface`
/// resolution modes — leaves it out; impact was the lane that had been missing the guard.
pub(crate) const RESOLVED_OPERATOR_ONLY: &str =
    "(edges.edge_kind != 'uses_operator' OR edges.to_symbol_id IS NOT NULL)";

pub(crate) fn validate_edge_kinds(edge_kinds: &[String]) -> anyhow::Result<()> {
    for edge_kind in edge_kinds {
        if !OPTIONAL_EDGE_KINDS.contains(&edge_kind.as_str()) {
            anyhow::bail!("unknown graph edge kind `{edge_kind}`");
        }
    }
    Ok(())
}
pub(crate) fn traversal_params(
    symbol: &str,
    limit: u32,
    edge_kinds: &[String],
    symbol_id: Option<i64>,
    logical_symbol_id: Option<i64>,
    unique_short_name: bool,
) -> Vec<String> {
    let qualified = symbol.to_string();
    let short = short_name(symbol).to_string();
    let fuzzy_qualified = format!("%::{qualified}");
    let allow_name_fallback = (!is_qualified_symbol(symbol)).to_string();
    let mut params = vec![
        qualified,
        fuzzy_qualified,
        short,
        allow_name_fallback,
        limit.to_string(),
        symbol_id.unwrap_or(-1).to_string(),
        unique_short_name.to_string(),
        logical_symbol_id.unwrap_or(-1).to_string(),
    ];
    params.extend(edge_kinds.iter().cloned());
    params
}
pub(crate) fn quoted_placeholders(count: usize) -> String {
    (0..count).map(|index| format!("?{}", index + 9)).collect::<Vec<_>>().join(", ")
}
pub(crate) fn reverse_predicate(mode: GraphResolutionMode, logical: bool) -> &'static str {
    if logical {
        return match mode {
            GraphResolutionMode::Exact =>
                "edges.to_symbol_id IS NOT NULL
                 AND edges.to_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )",
            // LOGICAL ATTRIBUTION RULE (mirrored in `forward_source_predicate`): a RESOLVED
            // endpoint is attributed by logical membership ALONE. The name arm applies only where
            // `to_symbol_id IS NULL` — an edge the resolver could not bind, whose recorded target
            // name is the only attribution there is (and shared with any same-name sibling). Were
            // the name arm to run on resolved rows too it would hand this symbol an edge the
            // resolver bound to a DIFFERENT symbol of the same qualified name, which is the
            // overload collapse the logical seed exists to end (#1028). Dropping the arm entirely
            // is the opposite error: this SQL runs BEFORE the read-side oracle enrichment, which
            // rewrites hops in memory and never touches the `edges` row — so an unresolved edge
            // refused here is one no compiler verdict can put back, in any later run.
            GraphResolutionMode::Syntactic =>
                "(edges.to_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                  )
                  OR (edges.to_symbol_id IS NULL
                      AND edges.target_qualified_name_id =
                            (SELECT id FROM name_strings WHERE value = ?1)))",
            GraphResolutionMode::Fuzzy =>
                "edges.to_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )
                 OR to_symbols.name = ?3
                 OR to_qn.value = ?1
                 OR to_qn.value LIKE ?2
                 OR edges.target_qualified_name = ?1
                 OR edges.target_qualified_name LIKE ?2
                 OR (?4 = 'true' AND edges.to_name_id =
                        (SELECT id FROM name_strings WHERE value = ?3))",
        };
    }
    // #682 INDEXED-SEED CONTRACT: the Exact/Syntactic seed branches compare the `edges` view's raw
    // dictionary/id columns (`to_symbol_id`, `target_qualified_name_id`) against a constant — a
    // bare id or a `(SELECT id FROM name_strings WHERE value = ?)` — so the planner drives a
    // MULTI-INDEX OR over `idx_edges_to_symbol` + `idx_edges_target_qname` instead of
    // full-scanning `edges_data` through the view's value joins (`to_qn.value`,
    // `edges.target_qualified_name`). Faithful transform of the value-column forms:
    // `to_qn.value = ?1` (the to-symbol's qualified name) is exactly the edges whose
    // `to_symbol_id` is a symbol with that interned qualified name, and `to_symbols.name = ?3`
    // those whose to-symbol has that short name. Fuzzy keeps the `LIKE` forms (opt-in,
    // inherently non-sargable). See the raw-id note in `ensure_edges_view`.
    match mode {
        GraphResolutionMode::Exact =>
            "edges.to_symbol_id IS NOT NULL
             AND (edges.to_symbol_id = ?6
                  OR edges.to_symbol_id IN (
                     SELECT id FROM symbols
                     WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)))",
        GraphResolutionMode::Syntactic =>
            "(edges.to_symbol_id = ?6
              OR edges.to_symbol_id IN (
                 SELECT id FROM symbols
                 WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1))
              OR (?7 = 'true' AND edges.to_symbol_id IN (
                 SELECT id FROM symbols WHERE name = ?3))
              OR edges.target_qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1))",
        GraphResolutionMode::Fuzzy =>
            "to_symbols.name = ?3
             OR to_qn.value = ?1
             OR to_qn.value LIKE ?2
             OR edges.target_qualified_name = ?1
             OR edges.target_qualified_name LIKE ?2
             OR (?4 = 'true' AND edges.to_name_id =
                    (SELECT id FROM name_strings WHERE value = ?3))",
    }
}
pub(crate) fn reverse_tier(mode: GraphResolutionMode) -> &'static str {
    match mode {
        GraphResolutionMode::Exact => "0",
        GraphResolutionMode::Syntactic =>
            "CASE
                WHEN edges.to_symbol_id IS NOT NULL THEN 0
                WHEN edges.target_qualified_name = ?1 THEN 1
                ELSE 4
             END",
        GraphResolutionMode::Fuzzy =>
            "CASE
                WHEN edges.to_symbol_id IS NOT NULL THEN 0
                WHEN edges.target_qualified_name = ?1 OR edges.target_qualified_name LIKE ?2 THEN 1
                WHEN ?4 = 'true' AND edges.to_name_id =
                    (SELECT id FROM name_strings WHERE value = ?3) THEN 2
                ELSE 4
             END",
    }
}
pub(crate) fn forward_source_predicate(mode: GraphResolutionMode, logical: bool) -> &'static str {
    if logical {
        return match mode {
            GraphResolutionMode::Exact =>
                "edges.from_symbol_id IS NOT NULL
                 AND edges.from_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )",
            // Mirror of the logical attribution rule on `reverse_predicate`: membership decides a
            // resolved source, the name arm covers only a source the resolver left unbound — a
            // file-level edge, or a body whose enclosing symbol was not indexed. `from_name` holds
            // the ENCLOSING SYMBOL'S QUALIFIED NAME, which two overloads share, so an ungated arm
            // would put a sibling overload's outgoing edges on this symbol's callee list.
            GraphResolutionMode::Syntactic =>
                "edges.from_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )
                 OR (edges.from_symbol_id IS NULL
                     AND edges.from_name_id = (SELECT id FROM name_strings WHERE value = ?1))",
            GraphResolutionMode::Fuzzy =>
                "from_symbols.id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )
                 OR from_symbols.name = ?3
                 OR from_qn.value = ?1
                 OR from_qn.value LIKE ?2
                 OR edges.from_name = ?1
                 OR edges.from_name LIKE ?2",
        };
    }
    // #682 INDEXED-SEED CONTRACT (mirror of reverse_predicate): Exact/Syntactic seed on the raw
    // `edges` id columns (`from_symbol_id`, `from_name_id`) against a constant so the planner
    // drives `idx_edges_from_symbol` + `idx_edges_from_name` instead of scanning `edges_data`
    // through the view's `from_qn.value` / `edges.from_name` value joins. Fuzzy keeps the
    // `LIKE` forms.
    match mode {
        GraphResolutionMode::Exact =>
            "edges.from_symbol_id IS NOT NULL
             AND (edges.from_symbol_id = ?6
                  OR edges.from_symbol_id IN (
                     SELECT id FROM symbols
                     WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)))",
        GraphResolutionMode::Syntactic =>
            "edges.from_symbol_id = ?6
             OR edges.from_symbol_id IN (
                SELECT id FROM symbols
                WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1))
             OR (?7 = 'true' AND edges.from_symbol_id IN (
                SELECT id FROM symbols WHERE name = ?3))
             OR edges.from_name_id = (SELECT id FROM name_strings WHERE value = ?1)",
        GraphResolutionMode::Fuzzy =>
            "from_symbols.name = ?3
             OR from_qn.value = ?1
             OR from_qn.value LIKE ?2
             OR edges.from_name = ?1
             OR edges.from_name LIKE ?2",
    }
}
pub(crate) fn forward_target_filter(
    mode: GraphResolutionMode,
    options: &GraphTraversalOptions,
) -> &'static str {
    match mode {
        GraphResolutionMode::Exact => "edges.to_symbol_id IS NOT NULL",
        GraphResolutionMode::Syntactic =>
            if options.include_unresolved {
                "1 = 1"
            } else if options.include_macros {
                "
                edges.to_symbol_id IS NOT NULL
                OR edges.target_qualified_name IS NOT NULL
                OR edges.edge_kind = 'uses_macro'
                "
            } else {
                "edges.to_symbol_id IS NOT NULL OR edges.target_qualified_name IS NOT NULL"
            },
        GraphResolutionMode::Fuzzy => "1 = 1",
    }
}
pub(crate) fn forward_visibility_filter(options: &GraphTraversalOptions) -> &'static str {
    match (options.include_unresolved, options.include_macros, options.include_common_methods) {
        (true, true, true) => "1 = 1",
        (true, true, false) =>
            "
            (
                edges.edge_kind != 'calls_name'
                OR edges.to_name NOT IN (
                    'clone', 'map', 'map_err', 'and_then', 'unwrap_or', 'unwrap_or_else',
                    'to_string', 'to_owned', 'as_ref', 'as_mut', 'get', 'insert',
                    'new', 'default', 'into', 'from', 'iter', 'collect', 'unwrap',
                    'expect', 'ok', 'err'
                )
                OR edges.to_symbol_id IS NOT NULL
            )
            ",
        (true, false, true) => "edges.edge_kind != 'uses_macro'",
        (true, false, false) =>
            "
            edges.edge_kind != 'uses_macro'
            AND (
                edges.edge_kind != 'calls_name'
                OR edges.to_name NOT IN (
                    'clone', 'map', 'map_err', 'and_then', 'unwrap_or', 'unwrap_or_else',
                    'to_string', 'to_owned', 'as_ref', 'as_mut', 'get', 'insert',
                    'new', 'default', 'into', 'from', 'iter', 'collect', 'unwrap',
                    'expect', 'ok', 'err'
                )
                OR edges.to_symbol_id IS NOT NULL
            )
            ",
        (false, true, true) =>
            "
            (
                edges.edge_kind = 'calls_name'
                AND (
                    edges.to_symbol_id IS NOT NULL
                    OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT NULL)
                )
            )
            OR (
                edges.edge_kind = 'constructs'
                AND edges.to_symbol_id IS NOT NULL
            )
            OR (
                edges.edge_kind = 'uses_operator'
                AND edges.to_symbol_id IS NOT NULL
            )
            OR edges.edge_kind = 'uses_macro'
            OR edges.edge_kind NOT IN ('calls_name', 'constructs', 'uses_operator')
            ",
        (false, true, false) =>
            "
            (
                edges.edge_kind = 'calls_name'
                AND (
                    edges.to_symbol_id IS NOT NULL
                    OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT NULL)
                )
                AND (
                    edges.to_name NOT IN (
                        'clone', 'map', 'map_err', 'and_then', 'unwrap_or', 'unwrap_or_else',
                        'to_string', 'to_owned', 'as_ref', 'as_mut', 'get', 'insert',
                        'new', 'default', 'into', 'from', 'iter', 'collect', 'unwrap',
                        'expect', 'ok', 'err'
                    )
                    OR edges.to_symbol_id IS NOT NULL
                )
            )
            OR (
                edges.edge_kind = 'constructs'
                AND edges.to_symbol_id IS NOT NULL
            )
            OR (
                edges.edge_kind = 'uses_operator'
                AND edges.to_symbol_id IS NOT NULL
            )
            OR edges.edge_kind = 'uses_macro'
            OR edges.edge_kind NOT IN ('calls_name', 'constructs', 'uses_operator')
            ",
        (false, false, true) =>
            "
            edges.edge_kind != 'uses_macro'
            AND (
                (
                    edges.edge_kind = 'calls_name'
                    AND (
                        edges.to_symbol_id IS NOT NULL
                        OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT \
             NULL)
                    )
                )
                OR (
                    edges.edge_kind = 'constructs'
                    AND edges.to_symbol_id IS NOT NULL
                )
                OR (
                    edges.edge_kind = 'uses_operator'
                    AND edges.to_symbol_id IS NOT NULL
                )
                OR edges.edge_kind NOT IN ('calls_name', 'constructs', 'uses_operator')
            )
            ",
        (false, false, false) =>
            "
            edges.edge_kind != 'uses_macro'
            AND (
                (
                    edges.edge_kind = 'calls_name'
                    AND (
                        edges.to_symbol_id IS NOT NULL
                        OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT \
             NULL)
                    )
                    AND (
                        edges.to_name NOT IN (
                            'clone', 'map', 'map_err', 'and_then', 'unwrap_or', 'unwrap_or_else',
                            'to_string', 'to_owned', 'as_ref', 'as_mut', 'get', 'insert',
                            'new', 'default', 'into', 'from', 'iter', 'collect', 'unwrap',
                            'expect', 'ok', 'err'
                        )
                        OR edges.to_symbol_id IS NOT NULL
                    )
                )
                OR (
                    edges.edge_kind = 'constructs'
                    AND edges.to_symbol_id IS NOT NULL
                )
                OR (
                    edges.edge_kind = 'uses_operator'
                    AND edges.to_symbol_id IS NOT NULL
                )
                OR edges.edge_kind NOT IN ('calls_name', 'constructs', 'uses_operator')
            )
            ",
    }
}
pub fn unique_symbol_name(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    // GENERATION-SCOPED via the `files` view (batch 6, count-scoping class): an unscoped
    // `COUNT(*) FROM symbols` counts the SAME live name once per generation during a
    // dead-generation window (post-flip pre-gc, or crashed pre-flip staging) → count == 2 →
    // "not unique" → the caller's `unique_short_name` gate DISABLES the short-name fallback
    // branch and suppresses a genuine LIVE hop from the returned rows. Joining the scoped view
    // counts only the live generation's symbols, matching the row queries in `traverse` /
    // `traversal_summary`.
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS symbol_count FROM symbols
         JOIN files ON files.id = symbols.file_id
         WHERE symbols.name = ?1",
        [name],
        |row| row.get("symbol_count"),
    )?;
    Ok(count == 1)
}
/// How many active-scope symbols a NAME seed expands to under the `Syntactic` predicates above.
///
/// Lives beside those predicates because it answers the same question they do and must not drift
/// from them: a symbol is a seed match when its qualified name IS `symbol`, or — only while the
/// seed's short name is unique, which is the `?7` gate the predicates themselves carry — when its
/// name is that short name. A caller that counts the qualified arm alone reports zero for the
/// unqualified seed a traversal resolves perfectly well through the short-name arm.
///
/// GENERATION-SCOPED via the `files` view, for the reason spelled out on `unique_symbol_name`.
pub fn syntactic_seed_symbol_count(conn: &Connection, symbol: &str) -> anyhow::Result<u64> {
    let short = short_name(symbol);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS symbol_count FROM symbols
         JOIN files ON files.id = symbols.file_id
         LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
         WHERE qn.value = ?1 OR (?2 = 1 AND symbols.name = ?3)",
        rusqlite::params![symbol, unique_symbol_name(conn, short)?, short],
        |row| row.get("symbol_count"),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}
pub(crate) fn resolution_label(
    mode: GraphResolutionMode,
    stored: String,
    tier: i64,
    verified_target_symbol: bool,
) -> String {
    if mode == GraphResolutionMode::Exact && verified_target_symbol {
        return "exact".to_string();
    }
    if stored != "unresolved" {
        return stored;
    }
    match tier {
        1 => "target_qualified_suffix".to_string(),
        2 => "target_name_fallback".to_string(),
        _ => stored,
    }
}
pub(crate) fn short_name(symbol: &str) -> &str {
    symbol.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(symbol)
}
pub(crate) fn is_qualified_symbol(symbol: &str) -> bool {
    symbol.contains("::")
        || symbol.contains(".rs:")
        || symbol.contains(".ts:")
        || symbol.contains(".tsx:")
        || symbol.contains(".kt:")
        || symbol.contains('/')
}
