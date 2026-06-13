use super::*;

pub(crate) fn import_export_items(
    conn: &Connection,
    symbol_id: i64,
    qualified_name: &str,
    names: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind, edges.from_name,
               edges.edge_kind, edges.confidence
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.edge_kind IN ('imports', 'exports')
          AND (edges.to_symbol_id = ?1 OR edges.to_name_id = (SELECT id FROM edge_strings WHERE \
         value = ?2))
        ORDER BY files.kind, files.path, edges.edge_kind
        LIMIT ?3
        ",
    )?;
    for name in std::iter::once(qualified_name).chain(names.iter().map(String::as_str)) {
        let rows = stmt.query_map(params![symbol_id, name, i64::from(limit)], |row| {
            impact_item_row(row, "Import/export dependents", "import_export_dependent")
        })?;
        items.extend(rows_to_items(rows)?);
        if items.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
            break;
        }
    }
    dedupe_items(&mut items);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(items)
}

pub(crate) fn test_items(
    conn: &Connection,
    symbol: &SymbolHit,
    names: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for name in names_for_like(symbol, names) {
        items.extend(section_like_items(
            conn,
            &name,
            "Tests touching this symbol/path",
            "test_mentions_symbol_or_path",
            "
            files.kind = 'source'
            AND (
                files.path LIKE '%test%'
                OR files.path LIKE '%spec%'
                OR chunks.text LIKE '%#[cfg(test)]%'
                OR chunks.text LIKE '%describe(%'
                OR chunks.text LIKE '%it(%'
                OR chunks.text LIKE '%test(%'
            )
            ",
            limit,
        )?);
    }
    let mut items = collapse_by_path(items);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(items)
}

pub(crate) fn docs_items(
    conn: &Connection,
    symbol: &SymbolHit,
    names: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for name in names_for_like(symbol, names) {
        items.extend(section_like_items(
            conn,
            &name,
            "Docs mentioning symbol/path",
            "docs_mentions_symbol_or_path",
            "files.kind = 'docs'",
            limit,
        )?);
    }
    let mut items = collapse_by_path(items);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(items)
}

pub(crate) fn text_fallback_items(
    conn: &Connection,
    symbol: &SymbolHit,
    names: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for name in names_for_like(symbol, names) {
        items.extend(section_like_items(
            conn,
            &name,
            "Text fallback hits",
            "text_fallback",
            "1 = 1",
            limit,
        )?);
    }
    let mut items = collapse_by_path(items);
    items.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(items)
}

pub(crate) fn names_for_like(symbol: &SymbolHit, names: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    out.insert(symbol.name.clone());
    out.insert(symbol.qualified_name.clone());
    out.insert(symbol.path.clone());
    for name in names {
        out.insert(name.clone());
    }
    out.into_iter().collect()
}

pub(crate) fn section_like_items(
    conn: &Connection,
    needle: &str,
    category: &str,
    reason: &str,
    filter: &str,
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let like = format!("%{needle}%");
    // Collapse to ONE row per file. The previous `LEFT JOIN symbols` without aggregation fanned a
    // file out into one row per symbol whenever the match was file-level (path or chunk text),
    // flooding the output and letting one big file starve the `LIMIT` (see issue #48). Grouping by
    // file keeps the match kind (symbol > path > chunk text) and names the symbol only when it was
    // a genuine symbol match.
    let sql = format!(
        "
        SELECT files.path, files.language, files.kind,
               MAX(CASE WHEN symbols.name LIKE ?1 OR symbols.qualified_name LIKE ?1
                        THEN symbols.qualified_name END) AS matched_symbol,
               MAX(CASE WHEN files.path LIKE ?1 THEN 1 ELSE 0 END) AS path_match
        FROM files
        LEFT JOIN symbols ON symbols.file_id = files.id
        LEFT JOIN chunks ON chunks.file_id = files.id
        WHERE ({filter})
          AND (
              files.path LIKE ?1
              OR symbols.name LIKE ?1
              OR symbols.qualified_name LIKE ?1
              OR chunks.text LIKE ?1
          )
        GROUP BY files.path, files.language, files.kind
        ORDER BY files.kind, files.path
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![like, i64::from(limit)], |row| {
        let matched_symbol: Option<String> = row.get(3)?;
        let path_match: i64 = row.get(4)?;
        // Precedence is path > symbol > chunk text. A path match is checked first because a
        // qualified name is `path::symbol`, so when the needle is the file's own path EVERY symbol
        // in it matches `qualified_name LIKE` — naming any one of them (the lexically-greatest)
        // would be spurious. A genuine symbol-name match (needle is the bare symbol) still names
        // the symbol.
        let (symbol, match_kind) = if path_match == 1 {
            (None, "path match")
        } else if let Some(symbol) = matched_symbol {
            (Some(symbol), "symbol match")
        } else {
            (None, "chunk text match")
        };
        Ok(ImpactItem {
            path: row.get(0)?,
            language: row.get(1)?,
            kind: row.get(2)?,
            symbol,
            category: category.to_string(),
            reason: reason.to_string(),
            evidence: vec![format!("{match_kind} for `{needle}`")],
        })
    })?;
    rows_to_items(rows)
}

pub(crate) fn git_commit_items(
    conn: &Connection,
    paths: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut surface = ImpactSurface::default();
    git_commits_for_paths(conn, paths, &mut surface, usize::try_from(limit).unwrap_or(usize::MAX))?;
    Ok(surface.into_items(usize::try_from(limit).unwrap_or(usize::MAX)))
}

pub(crate) fn github_ref_items(
    conn: &Connection,
    paths: &[String],
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut surface = ImpactSurface::default();
    github_refs_for_paths(conn, paths, &mut surface, usize::try_from(limit).unwrap_or(usize::MAX))?;
    Ok(surface.into_items(usize::try_from(limit).unwrap_or(usize::MAX)))
}

pub(crate) fn github_rationale_items(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut surface = ImpactSurface::default();
    github_rationale_for_query(
        conn,
        query,
        &mut surface,
        usize::try_from(limit).unwrap_or(usize::MAX),
    )?;
    Ok(surface.into_items(usize::try_from(limit).unwrap_or(usize::MAX)))
}

pub(crate) fn impact_item_row(
    row: &rusqlite::Row<'_>,
    category: &'static str,
    reason: &'static str,
) -> rusqlite::Result<ImpactItem> {
    Ok(ImpactItem {
        path: row.get(0)?,
        language: row.get(1)?,
        kind: row.get(2)?,
        symbol: row.get(3)?,
        category: category.to_string(),
        reason: reason.to_string(),
        evidence: vec![format!("{} edge ({})", row.get::<_, String>(4)?, row.get::<_, String>(5)?)],
    })
}

/// Collapse a file-granularity section (tests / docs / text fallback) to one row per file. Across
/// the several search needles (symbol name, qualified name, path) the same file can surface more
/// than once — keep a single representative per path, preferring the row that named a symbol (a
/// symbol match) over a bare path/chunk match so the more specific evidence wins.
pub(crate) fn collapse_by_path(items: Vec<ImpactItem>) -> Vec<ImpactItem> {
    use std::collections::btree_map::Entry;

    let mut by_path: BTreeMap<String, ImpactItem> = BTreeMap::new();
    for item in items {
        match by_path.entry(item.path.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(item);
            },
            Entry::Occupied(mut slot) =>
                if slot.get().symbol.is_none() && item.symbol.is_some() {
                    slot.insert(item);
                },
        }
    }
    by_path.into_values().collect()
}

pub(crate) fn dedupe_items(items: &mut Vec<ImpactItem>) {
    let mut seen = BTreeSet::new();
    items.retain(|item| {
        seen.insert((
            item.category.clone(),
            item.path.clone(),
            item.symbol.clone(),
            item.reason.clone(),
        ))
    });
}
