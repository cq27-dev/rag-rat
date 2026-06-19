use super::*;

pub(crate) fn graph_neighbors(
    conn: &Connection,
    targets: &[SymbolTarget],
    target_names: &[String],
    reverse: bool,
    resolution_mode: GraphResolutionMode,
    surface: &mut ImpactSurface,
) -> anyhow::Result<()> {
    let reason = if reverse { "direct_caller" } else { "direct_callee" };
    let source_path_col = if reverse {
        "COALESCE(source_files.path, from_files.path)"
    } else {
        "COALESCE(to_files.path, source_files.path)"
    };
    let source_language_col = if reverse {
        "COALESCE(source_files.language, from_files.language)"
    } else {
        "COALESCE(to_files.language, source_files.language)"
    };
    let source_kind_col = if reverse {
        "COALESCE(source_files.kind, from_files.kind)"
    } else {
        "COALESCE(to_files.kind, source_files.kind)"
    };
    let source_symbol_col = if reverse {
        "COALESCE(from_qn.value, edges.from_name)"
    } else {
        "COALESCE(to_qn.value, edges.to_name)"
    };
    let predicate = impact_graph_predicate(reverse, resolution_mode);
    let sql = format!(
        "
        SELECT {source_path_col}, {source_language_col}, {source_kind_col},
               {source_symbol_col}, edges.edge_kind, edges.confidence
        FROM edges
        LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
        LEFT JOIN files from_files ON from_files.id = from_symbols.file_id
        LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
        LEFT JOIN files to_files ON to_files.id = to_symbols.file_id
        LEFT JOIN name_strings from_qn ON from_qn.id = from_symbols.qualified_name_id
        LEFT JOIN name_strings to_qn ON to_qn.id = to_symbols.qualified_name_id
        LEFT JOIN files source_files ON source_files.id = edges.source_file_id
        WHERE edges.edge_kind IN ('calls_name', 'constructs', 'implements')
          AND ({predicate})
          AND {source_path_col} IS NOT NULL
        ORDER BY
            CASE edges.confidence
                WHEN 'Exact' THEN 0
                WHEN 'Syntactic' THEN 1
                WHEN 'NameOnly' THEN 2
                ELSE 3
            END,
            edges.edge_kind,
            {source_path_col},
            {source_symbol_col}
        ",
    );
    let mut stmt = conn.prepare(&sql)?;
    for target in targets {
        let rows = stmt.query_map(params![target.id, target.qualified_name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (path, language, kind, symbol, edge_kind, confidence) = row?;
            surface.push(
                ImpactCategory::DirectStructural,
                FileSymbol { path, language, kind, symbol },
                reason,
                format!("{edge_kind} edge to {} ({confidence})", target.qualified_name),
            );
        }
    }
    for name in target_names {
        if resolution_mode != GraphResolutionMode::Fuzzy && !is_qualified_symbol(name) {
            continue;
        }
        let rows = stmt.query_map(params![Option::<i64>::None, name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (path, language, kind, symbol, edge_kind, confidence) = row?;
            surface.push(
                ImpactCategory::DirectStructural,
                FileSymbol { path, language, kind, symbol },
                reason,
                format!("{edge_kind} edge matching {name} ({confidence})"),
            );
        }
    }
    Ok(())
}

pub(crate) fn impact_graph_predicate(reverse: bool, mode: GraphResolutionMode) -> &'static str {
    match (reverse, mode) {
        (true, GraphResolutionMode::Exact) => "edges.to_symbol_id = ?1",
        (false, GraphResolutionMode::Exact) =>
            "edges.from_symbol_id = ?1 AND edges.to_symbol_id IS NOT NULL",
        (true, GraphResolutionMode::Syntactic) =>
            "edges.to_symbol_id = ?1 OR edges.target_qualified_name = ?2",
        (false, GraphResolutionMode::Syntactic) =>
            "(edges.from_symbol_id = ?1 OR edges.from_name = ?2)
             AND (edges.to_symbol_id IS NOT NULL OR edges.target_qualified_name IS NOT NULL)",
        (true, GraphResolutionMode::Fuzzy) =>
            "edges.to_symbol_id = ?1 OR edges.to_name_id = (SELECT id FROM name_strings WHERE \
             value = ?2)",
        (false, GraphResolutionMode::Fuzzy) => "edges.from_symbol_id = ?1 OR edges.from_name = ?2",
    }
}

pub(crate) fn import_export_dependents(
    conn: &Connection,
    targets: &[SymbolTarget],
    target_names: &[String],
    surface: &mut ImpactSurface,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind, edges.from_name,
               edges.edge_kind, edges.confidence
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.edge_kind IN ('imports', 'exports')
          AND (edges.to_symbol_id = ?1 OR edges.to_name_id = (SELECT id FROM name_strings WHERE \
         value = ?2))
        ORDER BY files.kind, files.path, edges.edge_kind
        ",
    )?;
    for target in targets {
        let rows = stmt.query_map(params![target.id, target.qualified_name], import_export_row)?;
        push_import_export_rows(rows, target.qualified_name.as_str(), surface)?;
    }
    for name in target_names {
        let rows = stmt.query_map(params![Option::<i64>::None, name], import_export_row)?;
        push_import_export_rows(rows, name, surface)?;
    }
    Ok(())
}

pub(crate) fn import_export_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, String, Option<String>, String, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
}

pub(crate) fn push_import_export_rows(
    rows: rusqlite::MappedRows<
        '_,
        impl FnMut(
            &rusqlite::Row<'_>,
        )
            -> rusqlite::Result<(String, String, String, Option<String>, String, String)>,
    >,
    target: &str,
    surface: &mut ImpactSurface,
) -> anyhow::Result<()> {
    for row in rows {
        let (path, language, kind, symbol, edge_kind, confidence) = row?;
        surface.push(
            ImpactCategory::DirectStructural,
            FileSymbol { path, language, kind, symbol },
            "import_export_dependent",
            format!("{edge_kind} edge matching {target} ({confidence})"),
        );
    }
    Ok(())
}

pub(crate) fn same_file_siblings(
    conn: &Connection,
    targets: &[SymbolTarget],
    surface: &mut ImpactSurface,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind, qn.value
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE symbols.file_id = ?1 AND symbols.id != ?2
        ORDER BY symbols.start_byte
        LIMIT 20
        ",
    )?;
    for target in targets {
        let rows = stmt.query_map(params![target.file_id, target.id], |row| {
            Ok(FileSymbol {
                path: row.get(0)?,
                language: row.get(1)?,
                kind: row.get(2)?,
                symbol: row.get(3)?,
            })
        })?;
        for row in rows {
            surface.push(
                ImpactCategory::DirectStructural,
                row?,
                "same_file_sibling",
                format!("shares file with {}", target.qualified_name),
            );
        }
    }
    Ok(())
}

pub(crate) fn textual_fallback(
    conn: &Connection,
    query: &str,
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    if limit == 0 {
        return Ok(());
    }
    let like = format!("%{query}%");
    // Chunk-text fallback goes through the `chunk_fts` index (MATCH), not a raw `chunks.text LIKE`
    // full scan — tokenized + indexed, reads no raw text (#77). The FTS subquery yields chunk
    // file_ids across all scopes; the outer `files` is the active-scope view, so `files.id IN
    // (...)` keeps only in-scope files. `None` (no token) → omit the clause. No `LEFT JOIN
    // chunks` needed.
    let fts = fts_phrase_query(query);
    let chunk_clause = if fts.is_some() {
        "OR files.id IN (SELECT chunks.file_id FROM chunks JOIN chunk_fts ON chunk_fts.rowid = \
         chunks.id WHERE chunk_fts MATCH ?3)"
    } else {
        ""
    };
    let sql = format!(
        "
        SELECT DISTINCT files.path, files.language, files.kind, qn.value,
               CASE
                   WHEN files.path LIKE ?1 THEN 'path LIKE fallback'
                   WHEN symbols.name LIKE ?1 OR qn.value LIKE ?1 THEN 'symbol LIKE fallback'
                   ELSE 'chunk text match fallback'
               END
        FROM files
        LEFT JOIN symbols ON symbols.file_id = files.id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE files.path LIKE ?1
           OR symbols.name LIKE ?1
           OR qn.value LIKE ?1
           {chunk_clause}
        ORDER BY files.kind, files.path, qn.value
        LIMIT ?2
        "
    );
    let limit_param = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&like, &limit_param];
    if let Some(fts) = &fts {
        binds.push(fts);
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
        Ok((
            FileSymbol {
                path: row.get(0)?,
                language: row.get(1)?,
                kind: row.get(2)?,
                symbol: row.get(3)?,
            },
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (file_symbol, evidence) = row?;
        surface.push(ImpactCategory::ProbableTextual, file_symbol, "textual_fallback", evidence);
    }
    Ok(())
}
