use super::*;

pub fn traverse(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
) -> anyhow::Result<Vec<GraphHop>> {
    traverse_with_options(conn, symbol, reverse, limit, &GraphTraversalOptions::default())
}
pub fn traverse_with_options(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
    options: &GraphTraversalOptions,
) -> anyhow::Result<Vec<GraphHop>> {
    let edge_kinds =
        if reverse { options.caller_edge_kinds()? } else { options.callee_edge_kinds()? };
    let quoted = quoted_placeholders(edge_kinds.len());
    let unique_short_name = unique_symbol_name(conn, short_name(symbol))?;
    let mode = options.resolution_mode;
    let sql = if reverse {
        let predicate = reverse_predicate(mode, options.logical_symbol_id.is_some());
        let tier = reverse_tier(mode);
        format!(
            "
            SELECT COALESCE(from_qn.value, edges.from_name) AS from_symbol,
                   COALESCE(to_qn.value, edges.to_name) AS to_symbol,
                   edges.id AS edge_id,
                   edges.edge_kind AS edge_kind,
                   edges.confidence AS confidence,
                   edges.to_name AS target,
                   edges.target_qualified_name AS target_qualified_name,
                   edges.evidence AS evidence,
                   edges.receiver_hint AS receiver_hint,
                   edges.resolution AS edge_resolution,
                   edges.to_symbol_id IS NOT NULL AS verified_target_symbol,
                   source_files.path AS callsite_path,
                   COALESCE(NULLIF(edges.source_start_line, 0), 1) AS callsite_start_line,
                   COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), \
             1) AS callsite_end_line,
                   {tier} AS match_tier
            FROM edges
            JOIN files source_files ON source_files.id = edges.source_file_id
            LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
            LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
            LEFT JOIN name_strings from_qn ON from_qn.id = from_symbols.qualified_name_id
            LEFT JOIN name_strings to_qn ON to_qn.id = to_symbols.qualified_name_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({predicate})
            ORDER BY match_tier,
                CASE edges.confidence
                    WHEN 'Exact' THEN 0
                    WHEN 'Syntactic' THEN 1
                    WHEN 'NameOnly' THEN 2
                    ELSE 3
                END,
                edges.edge_kind,
                edges.from_name
            LIMIT ?5
            "
        )
    } else {
        let predicate = forward_source_predicate(mode, options.logical_symbol_id.is_some());
        let target_filter = forward_target_filter(mode, options);
        let visibility_filter = forward_visibility_filter(options);
        format!(
            "
            SELECT COALESCE(from_qn.value, edges.from_name) AS from_symbol,
                   COALESCE(to_qn.value, edges.to_name) AS to_symbol,
                   edges.id AS edge_id,
                   edges.edge_kind AS edge_kind,
                   edges.confidence AS confidence,
                   edges.to_name AS target,
                   edges.target_qualified_name AS target_qualified_name,
                   edges.evidence AS evidence,
                   edges.receiver_hint AS receiver_hint,
                   edges.resolution AS edge_resolution,
                   edges.to_symbol_id IS NOT NULL AS verified_target_symbol,
                   source_files.path AS callsite_path,
                   COALESCE(NULLIF(edges.source_start_line, 0), 1) AS callsite_start_line,
                   COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), \
             1) AS callsite_end_line,
                   0 AS match_tier
            FROM edges
            JOIN files source_files ON source_files.id = edges.source_file_id
            LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
            LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
            LEFT JOIN name_strings from_qn ON from_qn.id = from_symbols.qualified_name_id
            LEFT JOIN name_strings to_qn ON to_qn.id = to_symbols.qualified_name_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({predicate})
              AND ({target_filter})
              AND ({visibility_filter})
              AND ?4 IN ('true', 'false')
            ORDER BY
                CASE edges.confidence
                    WHEN 'Exact' THEN 0
                    WHEN 'Syntactic' THEN 1
                    WHEN 'NameOnly' THEN 2
                    ELSE 3
                END,
                edges.edge_kind,
                edges.to_name
            LIMIT ?5
            "
        )
    };
    let params = traversal_params(
        symbol,
        limit,
        &edge_kinds,
        options.symbol_id,
        options.logical_symbol_id,
        unique_short_name,
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |row| {
        let edge_kind: String = row.get("edge_kind")?;
        let confidence: String = row.get("confidence")?;
        let verified_target_symbol = row.get("verified_target_symbol")?;
        let resolution = resolution_label(
            mode,
            row.get::<_, String>("edge_resolution")?,
            row.get("match_tier")?,
            verified_target_symbol,
        );
        let callsite_path: String = row.get("callsite_path")?;
        let callsite_start = row.get("callsite_start_line")?;
        let callsite_end = row.get("callsite_end_line")?;
        let confidence = normalize_confidence(&confidence).to_string();
        Ok(GraphHop {
            edge_id: row.get("edge_id")?,
            from_symbol: row.get("from_symbol")?,
            to_symbol: row.get("to_symbol")?,
            edge_kind: edge_kind.clone(),
            confidence: confidence.clone(),
            edge_confidence: confidence,
            target: row.get("target")?,
            target_qualified_name: row.get("target_qualified_name")?,
            evidence: row.get("evidence")?,
            receiver_hint: row.get("receiver_hint")?,
            resolution,
            // Heuristic traversal sets no oracle fields; the `IndexDatabase` enrichment pass
            // (`enrich_hops_with_oracle`) fills these from a current, in-scope `edge_oracle` join.
            resolution_reason: None,
            resolved_external: None,
            verified_target_symbol,
            shown_by_default: CALL_EDGE_KINDS.contains(&edge_kind.as_str()),
            callsite: Some(Callsite {
                path: callsite_path,
                line: callsite_start,
                span: [callsite_start, callsite_end],
            }),
            // Filled by the `impact_surface` load-bearing enrichment pass (`IndexDatabase`), which
            // has the active scope + oracle context; heuristic traversal leaves it absent.
            importance: None,
        })
    })?;
    let mut hops = Vec::new();
    for row in rows {
        hops.push(row?);
    }
    dedupe_hops(&mut hops);
    Ok(hops)
}
pub(crate) fn dedupe_hops(hops: &mut Vec<GraphHop>) {
    let mut seen = BTreeSet::new();
    hops.retain(|hop| {
        let callsite = hop.callsite.as_ref();
        seen.insert((
            hop.from_symbol.clone(),
            hop.to_symbol.clone(),
            hop.edge_id,
            hop.edge_kind.clone(),
            hop.target.clone(),
            hop.target_qualified_name.clone(),
            hop.receiver_hint.clone(),
            callsite.map(|value| value.path.clone()),
            callsite.map(|value| value.span),
        ))
    });
}
pub fn traversal_summary(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    limit: u32,
    options: &GraphTraversalOptions,
    returned_count: usize,
) -> anyhow::Result<GraphTraversalSummary> {
    let edge_kinds =
        if reverse { options.caller_edge_kinds()? } else { options.callee_edge_kinds()? };
    let quoted = quoted_placeholders(edge_kinds.len());
    let unique_short_name = unique_symbol_name(conn, short_name(symbol))?;
    let mode = options.resolution_mode;
    let sql = if reverse {
        let predicate = reverse_predicate(mode, options.logical_symbol_id.is_some());
        format!(
            "
            SELECT
                COUNT(*),
                SUM(CASE WHEN edges.to_symbol_id IS NOT NULL THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'Syntactic' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'NameOnly' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'Ambiguous' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.to_symbol_id IS NULL THEN 1 ELSE 0 END)
            FROM edges
            JOIN files source_files ON source_files.id = edges.source_file_id
            LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
            LEFT JOIN name_strings to_qn ON to_qn.id = to_symbols.qualified_name_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({predicate})
            "
        )
    } else {
        let predicate = forward_source_predicate(mode, options.logical_symbol_id.is_some());
        let target_filter = forward_target_filter(mode, options);
        let visibility_filter = forward_visibility_filter(options);
        format!(
            "
            SELECT
                COUNT(*),
                SUM(CASE WHEN edges.to_symbol_id IS NOT NULL THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'Syntactic' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'NameOnly' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.confidence = 'Ambiguous' THEN 1 ELSE 0 END),
                SUM(CASE WHEN edges.to_symbol_id IS NULL THEN 1 ELSE 0 END)
            FROM edges
            JOIN files source_files ON source_files.id = edges.source_file_id
            LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
            LEFT JOIN name_strings from_qn ON from_qn.id = from_symbols.qualified_name_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({predicate})
              AND ({target_filter})
              AND ({visibility_filter})
              AND ?4 IN ('true', 'false')
            "
        )
    };
    let params = traversal_params(
        symbol,
        limit,
        &edge_kinds,
        options.symbol_id,
        options.logical_symbol_id,
        unique_short_name,
    );
    let mut summary = conn.query_row(&sql, params_from_iter(params), |row| {
        Ok(GraphTraversalSummary {
            returned_count: u64::try_from(returned_count).unwrap_or(u64::MAX),
            total_matching_edges: count_col(row, 0)?,
            truncated: false,
            exact_verified: count_col(row, 1)?,
            syntactic: count_col(row, 2)?,
            name_only: count_col(row, 3)?,
            ambiguous: count_col(row, 4)?,
            unresolved: count_col(row, 5)?,
            false_positive_risk: String::new(),
            completeness_risk: String::new(),
            completeness_note: None,
        })
    })?;
    let hidden_unresolved = hidden_unresolved_candidate_count(
        conn,
        symbol,
        reverse,
        &edge_kinds,
        options,
        unique_short_name,
    )?;
    summary.total_matching_edges = summary.total_matching_edges.saturating_add(hidden_unresolved);
    summary.unresolved = summary.unresolved.saturating_add(hidden_unresolved);
    summary.truncated = summary.total_matching_edges > u64::from(limit);
    summary.false_positive_risk = false_positive_risk(&summary, mode).to_string();
    summary.completeness_risk = completeness_risk(&summary).to_string();
    // #200: a `find_callers` (reverse) that found ZERO candidate edges must not read `low` — a
    // static call graph cannot prove the ABSENCE of callers. Callers reached via message/enum
    // dispatch (the actor-channel pattern), dynamic dispatch, trait objects, FFI, or reflection
    // are invisible, as are external/entry-point callers. Escalate a `low` to `medium` and
    // attach a note so "0 callers" isn't trusted as complete. (Forward `trace_callees` with 0
    // callees is a genuine leaf — left alone.)
    if reverse && summary.total_matching_edges == 0 {
        if summary.completeness_risk == "low" {
            summary.completeness_risk = "medium".to_string();
        }
        summary.completeness_note = Some(super::NO_STATIC_CALLERS_NOTE.to_string());
    }
    Ok(summary)
}
pub(crate) fn count_col(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, Option<i64>>(index)?.unwrap_or(0);
    Ok(u64::try_from(value).unwrap_or(0))
}
/// Normalize a raw DB edge-confidence value (`Exact`/`Syntactic`/`NameOnly`/`Ambiguous`) to the
/// snake_case form used everywhere in tool output, so graph traversal, read_chunk, and search all
/// serialize confidence identically.
pub fn normalize_confidence(value: &str) -> &'static str {
    match value {
        "Exact" => "exact",
        "Syntactic" => "syntactic",
        "NameOnly" => "name_only",
        "Ambiguous" => "ambiguous",
        _ => "name_only",
    }
}
/// Effective confidence ordering AFTER oracle enrichment, lowest rank = highest priority (so a
/// stable ascending sort puts the strongest tier first). `compiler` (the SCIP oracle tier) ranks
/// ABOVE `exact` — that is the whole point of the tier — then the heuristic ladder. Unknown strings
/// rank last so a future tier can't silently jump the queue. Used to re-sort the overfetched
/// candidate set before truncating to the caller's limit, so a compiler-upgraded low-confidence
/// edge isn't dropped by the heuristic `LIMIT` (#82 finding 4).
pub fn effective_confidence_rank(confidence: &str) -> u8 {
    match confidence {
        "compiler" => 0,
        "exact" => 1,
        "syntactic" => 2,
        "name_only" => 3,
        "ambiguous" => 4,
        _ => 5,
    }
}
/// The overfetch cap for an oracle-aware traversal: traverse this many heuristic candidates so a
/// compiler-upgraded low-confidence edge — which the heuristic ranks below the `limit` cutoff — is
/// still in the candidate set when enrichment + re-sort run, before truncating back to `limit`.
/// `4x` with a floor of 200 gives generous headroom for the common small `limit`; the `5000`
/// ceiling caps the extra headroom so a pathological `limit` can't materialize the whole graph, but
/// the result is never below `limit` itself (we must still fetch what the caller asked for). An
/// edge upgraded beyond this window is the accepted residual (the heuristic already ranked it very
/// far down) (#82 finding 4).
pub fn oracle_overfetch_limit(limit: u32) -> u32 {
    let headroom = limit.saturating_mul(4).clamp(200, 5000);
    limit.max(headroom)
}
pub(crate) fn false_positive_risk(
    summary: &GraphTraversalSummary,
    mode: GraphResolutionMode,
) -> &'static str {
    // Risk reflects whether the *returned* edges could be wrong, not the mode alone. Syntactic is
    // the default mode, so charging it "medium" unconditionally mislabels results where every edge
    // resolved to a verified target symbol (the common, trustworthy case). Only bump for syntactic
    // mode when some matching edge was NOT verified against the target.
    let has_unverified = summary.exact_verified < summary.total_matching_edges;
    if summary.ambiguous > 0 || mode == GraphResolutionMode::Fuzzy {
        "high"
    } else if summary.name_only > 0
        || summary.unresolved > 0
        || (mode == GraphResolutionMode::Syntactic && has_unverified)
    {
        "medium"
    } else {
        "low"
    }
}
pub(crate) fn completeness_risk(summary: &GraphTraversalSummary) -> &'static str {
    if summary.truncated
        || summary.unresolved > summary.exact_verified.saturating_add(summary.syntactic)
    {
        "high"
    } else if summary.unresolved > 0 || summary.name_only > 0 || summary.ambiguous > 0 {
        "medium"
    } else {
        "low"
    }
}
pub(crate) fn hidden_unresolved_candidate_count(
    conn: &Connection,
    symbol: &str,
    reverse: bool,
    edge_kinds: &[String],
    options: &GraphTraversalOptions,
    unique_short_name: bool,
) -> anyhow::Result<u64> {
    let mode = options.resolution_mode;
    let quoted = quoted_placeholders(edge_kinds.len());
    let sql = if reverse {
        let predicate = reverse_predicate(mode, options.logical_symbol_id.is_some());
        format!(
            "
            SELECT COUNT(*)
            FROM edges
            JOIN files source_files ON source_files.id = edges.source_file_id
            LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
            LEFT JOIN name_strings to_qn ON to_qn.id = to_symbols.qualified_name_id
            WHERE edges.edge_kind IN ({quoted})
              AND edges.to_symbol_id IS NULL
              AND NOT ({predicate})
              AND (
                edges.target_qualified_name = ?1
                OR edges.target_qualified_name LIKE ?2
                OR edges.to_name_id = (SELECT id FROM name_strings WHERE value = ?3)
              )
            "
        )
    } else {
        let source_predicate = forward_source_predicate(mode, options.logical_symbol_id.is_some());
        let target_filter = forward_target_filter(mode, options);
        let visibility_filter = forward_visibility_filter(options);
        format!(
            "
            SELECT COUNT(*)
            FROM edges
            JOIN files source_files ON source_files.id = edges.source_file_id
            LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
            LEFT JOIN name_strings from_qn ON from_qn.id = from_symbols.qualified_name_id
            WHERE edges.edge_kind IN ({quoted})
              AND ({source_predicate})
              AND edges.to_symbol_id IS NULL
              AND NOT (({target_filter}) AND ({visibility_filter}))
              AND ?4 IN ('true', 'false')
            "
        )
    };
    let params = traversal_params(
        symbol,
        0,
        edge_kinds,
        options.symbol_id,
        options.logical_symbol_id,
        unique_short_name,
    );
    let count = conn.query_row(&sql, params_from_iter(params), |row| count_col(row, 0))?;
    Ok(count)
}
