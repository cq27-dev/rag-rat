use std::collections::HashSet;

use rusqlite::params_from_iter;
use rusqlite::types::Value;

use super::*;

/// One `graph_neighbors` row: the neighbour's file/symbol columns, the edge's kind and stored
/// confidence, and `edges.id` — the handle the oracle seed list is keyed on.
type NeighborRow = (String, String, String, Option<String>, String, String, i64);

fn neighbor_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NeighborRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
}

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
    // Impact's Fuzzy predicate falls back to matching by NAME (`edges.to_name_id = …`), so without
    // this guard every built-in `+` / `==` token — which Swift emits as an unresolved
    // `uses_operator` edge — would surface as a direct caller of any same-named custom
    // operator. Graph traversal and graph metadata already require a resolved operator target;
    // impact must too.
    let resolved_operator_only = crate::graph::RESOLVED_OPERATOR_ONLY;
    let sql_for = |predicate: &str| {
        format!(
            "
        SELECT {source_path_col}, {source_language_col}, {source_kind_col},
               {source_symbol_col}, edges.edge_kind, edges.confidence, edges.id
        FROM edges
        LEFT JOIN symbols from_symbols ON from_symbols.id = edges.from_symbol_id
        LEFT JOIN files from_files ON from_files.id = from_symbols.file_id
        LEFT JOIN symbols to_symbols ON to_symbols.id = edges.to_symbol_id
        LEFT JOIN files to_files ON to_files.id = to_symbols.file_id
        LEFT JOIN name_strings from_qn ON from_qn.id = from_symbols.qualified_name_id
        LEFT JOIN name_strings to_qn ON to_qn.id = to_symbols.qualified_name_id
        LEFT JOIN files source_files ON source_files.id = edges.source_file_id
        WHERE edges.edge_kind IN (
            'calls_name', 'constructs', 'uses_operator', 'uses_precedence_group', 'implements'
        )
          AND {resolved_operator_only}
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
        "
        )
    };
    let mut stmt = conn.prepare(&sql_for(predicate))?;
    // Bind exactly the placeholders the CHOSEN predicate references. Every arm uses `?1` (the
    // symbol id), but only the name-matching arms use `?2` — the two `Exact` arms match on the id
    // alone. Binding an unreferenced `?2` makes rusqlite reject the statement outright ("Wrong
    // number of parameters passed to query. Got 2, needed 1"), which is why `impact_surface` with
    // `resolution = exact` returned an error instead of a surface.
    //
    // The oracle arm adds no placeholder — the edge ids are spliced, as they are in
    // `graph::reverse_predicate` — so this stays keyed on the base predicate.
    let binds_name = predicate.contains("?2");
    for target in targets {
        // REVERSE ONLY: an edge the resolver left unbound records the callee name AS WRITTEN at the
        // call site — a RECEIVER-qualified name (`h::target` for `h.target(..)`), never the
        // definition's path-qualified name — so neither the id arm nor the qualified-name arm above
        // reaches it, and a compiler verdict naming this target is the only way that caller can
        // surface. Enrichment could not add it either: it relabels rows the query already returned,
        // and this lane runs no enrichment pass at all. Forward neighbours have no such seed. With
        // no oracle verdict the list is empty and the SQL is the base statement, unchanged.
        //
        // `SymbolTarget` carries no logical id, so the seed runs NON-LOGICAL — attributed to the
        // target symbol plus its qualified-name twins, which is also the attribution this lane's
        // heuristic arms use. `find_callers` fills `logical_symbol_id` in, so a verdict resolving
        // to a SIBLING member of a logical group seeds there and still does not seed here: the two
        // lanes agree on the non-logical seed set, not on logical groups.
        let (oracle_arm, seeded_edge_ids) = if reverse {
            let options = GraphTraversalOptions {
                resolution_mode,
                symbol_id: Some(target.id),
                ..GraphTraversalOptions::default()
            };
            let edge_ids =
                graph::reverse_oracle_seeded_edge_ids(conn, &target.qualified_name, &options)?;
            (graph::oracle_seed_in_list(&edge_ids), HashSet::<i64>::from_iter(edge_ids))
        } else {
            (None, HashSet::new())
        };
        let mut seeded_stmt = match &oracle_arm {
            Some(list) => Some(conn.prepare(&sql_for(&format!("{predicate} OR {list}")))?),
            None => None,
        };
        let stmt = seeded_stmt.as_mut().unwrap_or(&mut stmt);
        let mut args = vec![Value::Integer(target.id)];
        if binds_name {
            args.push(Value::Text(target.qualified_name.clone()));
        }
        let rows = stmt.query_map(params_from_iter(args), neighbor_row)?;
        for row in rows {
            let (path, language, kind, symbol, edge_kind, confidence, edge_id) = row?;
            // A seeded row's STORED confidence describes the name the resolver wrote down, not why
            // the row is here: an unbound edge carries `NameOnly` against a receiver-qualified
            // name, so rendering that alone reports the best-evidenced caller in the surface as the
            // weakest and attributes it to a target the edge does not record. The seed query
            // applied the run-currency, content-key and live-definition gates, so membership in its
            // id list IS the evidence — but this lane runs no enrichment pass, so nothing has
            // re-checked the verdict against the row being rendered or arbitrated between tools.
            let confidence = if seeded_edge_ids.contains(&edge_id) {
                format!("{confidence}, compiler-verified")
            } else {
                confidence
            };
            surface.push(
                ImpactCategory::DirectStructural,
                FileSymbol { path, language, kind, symbol },
                reason,
                format!("{edge_kind} edge to {} ({confidence})", target.qualified_name),
            );
        }
    }
    for name in target_names {
        // Without a `?2` the predicate cannot match on a name at all, so the name pass has nothing
        // to contribute (an id-only `Exact` predicate against a NULL id matches nothing).
        if !binds_name
            || (resolution_mode != GraphResolutionMode::Fuzzy && !is_qualified_symbol(name))
        {
            continue;
        }
        let rows = stmt.query_map(params![Option::<i64>::None, name], neighbor_row)?;
        for row in rows {
            let (path, language, kind, symbol, edge_kind, confidence, _) = row?;
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
        // #692: seed on the `edges` view's raw id columns compared to an interned-id lookup, not
        // the value-joined `target_qualified_name` / `from_name`, so the planner drives a
        // MULTI-INDEX OR over idx_edges_to_symbol/idx_edges_target_qname (reverse) or
        // idx_edges_from_symbol/idx_edges_from_name (forward) instead of full-scanning edges_data.
        // Same class as #682; the Fuzzy REVERSE arm already used the id form.
        (true, GraphResolutionMode::Syntactic) =>
            "edges.to_symbol_id = ?1 OR edges.target_qualified_name_id = (SELECT id FROM \
             name_strings WHERE value = ?2)",
        (false, GraphResolutionMode::Syntactic) =>
            "(edges.from_symbol_id = ?1 OR edges.from_name_id = (SELECT id FROM name_strings WHERE \
             value = ?2))
             AND (edges.to_symbol_id IS NOT NULL OR edges.target_qualified_name IS NOT NULL)",
        (true, GraphResolutionMode::Fuzzy) =>
            "edges.to_symbol_id = ?1 OR edges.to_name_id = (SELECT id FROM name_strings WHERE \
             value = ?2)",
        (false, GraphResolutionMode::Fuzzy) =>
            "edges.from_symbol_id = ?1 OR edges.from_name_id = (SELECT id FROM name_strings WHERE \
             value = ?2)",
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

#[cfg(test)]
mod tests {
    use rag_rat_core::index::install_scope_view;

    use super::*;

    const COMMIT: &str = "c0ffee";

    /// The FLAT `Vec<ImpactItem>` lane — reached from a free-text `query` and from the
    /// `allow_ambiguous` fallback — must name the same callers `find_callers` does, or the same
    /// symbol reports a different caller set depending only on how it was selected (#1214).
    ///
    /// The fixture is the shape the seed exists for: a call the resolver could not bind, recorded
    /// under the callee name AS WRITTEN (`h::target`), which matches neither the target's
    /// qualified name nor any symbol id — so no heuristic arm reaches it and only the compiler
    /// verdict can.
    #[test]
    fn the_flat_lane_returns_an_oracle_seeded_caller() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_core::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               commit_sha, worktree_id)
             VALUES ('a.rs', 'rust', 'source', 'sha-a', 0, 0, ?1, '')",
            params![COMMIT],
        )
        .unwrap();
        let file = conn.last_insert_rowid();
        let add_symbol = |name: &str, qualified: &str| -> i64 {
            conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![
                qualified
            ])
            .unwrap();
            conn.execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind,
                                     start_byte, end_byte, signature, docs)
                 VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?3),
                         'function', 0, 10, NULL, NULL)",
                params![file, name, qualified],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let target = add_symbol("target", "a.rs::target");
        let caller = add_symbol("caller", "a.rs::caller");
        conn.execute(
            "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                               target_qualified_name, edge_kind, confidence,
                               source_start_byte, source_end_byte,
                               callee_start_byte, callee_end_byte)
             VALUES (?1, ?2, NULL, 'target', 'h::target', 'calls_name', 'NameOnly',
                     100, 105, 100, 105)",
            params![file, caller],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at,
                                     status)
             VALUES ('rust-analyzer', 'ra 1.0', ?1, '', 0, 'ok')",
            params![COMMIT],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO edge_oracle(source_path, source_start_byte, source_end_byte,
                                     callee_start_byte, callee_end_byte, edge_kind, file_sha,
                                     tool, tool_version, resolved_symbol_id, scip_symbol, kind,
                                     computed_at)
             VALUES ('a.rs', 100, 105, 100, 105, 'calls_name', 'sha-a',
                     'rust-analyzer', 'ra 1.0', ?1, 'scip x', 'upgrade', 0)",
            params![target],
        )
        .unwrap();
        install_scope_view(&conn, COMMIT, "").unwrap();

        let items =
            impact_surface_with_options(&conn, "a.rs::target", 50, GraphResolutionMode::Syntactic)
                .unwrap();
        let callers =
            items.iter().filter(|item| item.reason == "direct_caller").collect::<Vec<_>>();
        assert_eq!(
            callers.len(),
            1,
            "the compiler-resolved call site is the one caller: {items:#?}"
        );
        assert_eq!(callers[0].symbol.as_deref(), Some("a.rs::caller"));
        // The row is here because the compiler resolved it, and the evidence must say so: the
        // stored `NameOnly` alone would report the surface's best-evidenced caller as its weakest,
        // and attribute it to a target the edge does not record.
        assert_eq!(
            callers[0].evidence,
            vec!["calls_name edge to a.rs::target (NameOnly, compiler-verified)"],
            "the seeded row must carry the verdict in its evidence: {items:#?}"
        );
        // Forward neighbours carry no seed: the same verdict must not invent a callee.
        assert!(
            !items.iter().any(|item| item.reason == "direct_callee"),
            "the reverse-only seed must not leak into the callee lane: {items:#?}"
        );
    }

    /// #692: pin the real predicate strings to the indexed-id form so a refactor cannot silently
    /// reintroduce the value-joined `target_qualified_name` / `from_name` seed (which full-scans
    /// edges_data — the plan is asserted in
    /// `edge_view::impact_and_grep_augment_seeds_use_edge_id_indexes`).
    #[test]
    fn syntactic_and_fuzzy_predicates_seed_on_interned_ids() {
        use GraphResolutionMode::{Fuzzy, Syntactic};
        for (reverse, mode, id_col) in [
            (true, Syntactic, "target_qualified_name_id"),
            (false, Syntactic, "from_name_id"),
            (false, Fuzzy, "from_name_id"),
        ] {
            let p = impact_graph_predicate(reverse, mode);
            assert!(
                p.contains(&format!("{id_col} = (SELECT id FROM name_strings WHERE value = ?2)")),
                "({reverse}, {mode:?}) must seed on {id_col} via an interned-id lookup, got: {p}"
            );
            // The value-joined forms must never come back (they defeat the edge indexes).
            assert!(
                !p.contains("target_qualified_name = ?") && !p.contains("from_name = ?"),
                "({reverse}, {mode:?}) must not compare a value-joined column, got: {p}"
            );
            // `?2` must stay present so `graph_neighbors`' `binds_name` name-pass still fires.
            assert!(p.contains("?2"), "({reverse}, {mode:?}) must still bind ?2, got: {p}");
        }
    }
}
