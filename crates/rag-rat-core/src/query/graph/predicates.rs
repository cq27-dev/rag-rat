use super::*;

// #224 ALIAS CONTRACT: symbol qualified-names are interned into `name_strings`, so these predicate
// fragments reference the joined pool value `from_qn.value` / `to_qn.value` (NOT a
// `from_symbols.qualified_name` column, which no longer exists). EVERY query that splices a
// fragment from this module MUST also provide the matching `LEFT JOIN name_strings from_qn ON
// from_qn.id = from_symbols.qualified_name_id` (and/or `to_qn`) for the alias it uses — `reverse_*`
// use `to_qn`, `forward_*` use `from_qn`. The interned-id arms (`edges.to_name_id = (SELECT id FROM
// name_strings WHERE value = ?3)`) are an edge-side pool lookup and are independent of these symbol
// joins.

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
            GraphResolutionMode::Syntactic =>
                "(edges.to_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                  )
                  OR edges.target_qualified_name = ?1)",
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
    match mode {
        GraphResolutionMode::Exact =>
            "edges.to_symbol_id IS NOT NULL
             AND (edges.to_symbol_id = ?6 OR to_qn.value = ?1)",
        GraphResolutionMode::Syntactic =>
            "(edges.to_symbol_id = ?6
              OR to_qn.value = ?1
              OR (?7 = 'true' AND to_symbols.name = ?3)
              OR edges.target_qualified_name = ?1)",
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
                "from_symbols.id IS NOT NULL
                 AND from_symbols.id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )",
            GraphResolutionMode::Syntactic =>
                "from_symbols.id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )
                 OR edges.from_name = ?1",
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
    match mode {
        GraphResolutionMode::Exact =>
            "from_symbols.id IS NOT NULL
             AND (from_symbols.id = ?6 OR from_qn.value = ?1)",
        GraphResolutionMode::Syntactic =>
            "from_symbols.id = ?6
             OR from_qn.value = ?1
             OR (?7 = 'true' AND from_symbols.name = ?3)
             OR edges.from_name = ?1",
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
            OR edges.edge_kind = 'uses_macro'
            OR edges.edge_kind NOT IN ('calls_name', 'constructs')
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
            OR edges.edge_kind = 'uses_macro'
            OR edges.edge_kind NOT IN ('calls_name', 'constructs')
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
                OR edges.edge_kind NOT IN ('calls_name', 'constructs')
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
                OR edges.edge_kind NOT IN ('calls_name', 'constructs')
            )
            ",
    }
}
pub(crate) fn unique_symbol_name(conn: &Connection, name: &str) -> anyhow::Result<bool> {
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
