use super::*;

pub fn parser_failure_count(conn: &Connection) -> anyhow::Result<u64> {
    // `parser_failures` is direct-scoped (V040): count only the ACTIVE repo's failures, matching
    // the scoped `IndexDatabase::parser_failure_count` twin — a sibling repo's parse failures must
    // not depress this repo's graph-coverage confidence in a consolidated DB.
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    rag_rat_db::meta::scoped_table_row_count(conn, "parser_failures", &repo_id)
}

pub(crate) fn historical_evidence(
    conn: &Connection,
    paths: &[String],
    query: &str,
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    if paths.is_empty() || surface.len() >= limit {
        return Ok(());
    }
    git_commits_for_paths(conn, paths, surface, limit.saturating_sub(surface.len()))?;
    if surface.len() >= limit {
        return Ok(());
    }
    papertrail_refs_for_paths(conn, paths, surface, limit.saturating_sub(surface.len()))?;
    if surface.len() >= limit {
        return Ok(());
    }
    papertrail_rationale_for_query(conn, query, surface, limit.saturating_sub(surface.len()))?;
    Ok(())
}

pub(crate) fn git_commits_for_paths(
    conn: &Connection,
    paths: &[String],
    surface: &mut ImpactSurface,
    budget: usize,
) -> anyhow::Result<()> {
    // `budget` is the number of distinct history ITEMS this section may add. All commits for one
    // path collapse to a single `(path, "git_commit_touched_file")` item, so we budget PER PATH
    // (not per commit row) — otherwise one hot file's commits exhaust the budget and starve later
    // paths' items, hiding flat-shape truncation (#150 review). Per-path evidence is bounded by
    // `budget` rows. The structured caller (`git_commit_items`) always passes a single path, so its
    // behaviour is unchanged.
    if budget == 0 {
        return Ok(());
    }
    // `git_commits` / `git_file_changes` are direct-scoped (V040); join AND filter on `repo_id` so
    // a consolidated DB never attributes a sibling repo's history to this path (a fork shares
    // hashes).
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let mut added = 0usize;
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind,
               git_commits.hash, git_commits.subject, git_commits.authored_at_s
        FROM git_file_changes
        JOIN git_commits ON git_commits.hash = git_file_changes.commit_hash
                        AND git_commits.repo_id = git_file_changes.repo_id
        LEFT JOIN files ON files.path = git_file_changes.path
        WHERE git_file_changes.path = ?1 AND git_file_changes.repo_id = ?3
        ORDER BY git_commits.authored_at_s DESC, git_commits.hash
        LIMIT ?2
        ",
    )?;
    for path in paths {
        if added >= budget {
            break;
        }
        let before = surface.len();
        let file = file_for_path(conn, path)?;
        let rows = stmt.query_map(
            params![path, i64::try_from(budget).unwrap_or(i64::MAX), repo_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )?;
        for row in rows {
            let (row_path, language, kind, hash, subject, authored_at_s) = row?;
            let file_symbol = FileSymbol {
                path: row_path.unwrap_or_else(|| file.path.clone()),
                language: language.unwrap_or_else(|| file.language.clone()),
                kind: kind.unwrap_or_else(|| file.kind.clone()),
                symbol: None,
            };
            surface.push(
                ImpactCategory::HistoricalPapertrail,
                file_symbol,
                "git_commit_touched_file",
                format!("{} touched {path} at {authored_at_s}: {subject}", short_hash(&hash)),
            );
        }
        if surface.len() > before {
            added += 1;
        }
    }
    Ok(())
}

pub(crate) fn papertrail_refs_for_paths(
    conn: &Connection,
    paths: &[String],
    surface: &mut ImpactSurface,
    budget: usize,
) -> anyhow::Result<()> {
    // Budget per PATH (one `(path, "papertrail")` item per path), not per ref row — same
    // item-vs-row reasoning as `git_commits_for_paths` (#150 review).
    if budget == 0 {
        return Ok(());
    }
    // `papertrail_refs` is direct-scoped: only surface the ACTIVE repo's refs for this path.
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let mut added = 0usize;
    let mut stmt = conn.prepare(
        "
        SELECT tracker, project, item_key, ref_kind, source_kind, source_text
        FROM papertrail_refs
        WHERE source_path = ?1 AND repo_id = ?3
        ORDER BY id DESC
        LIMIT ?2
        ",
    )?;
    for path in paths {
        if added >= budget {
            break;
        }
        let before = surface.len();
        let file = file_for_path(conn, path)?;
        let rows = stmt.query_map(
            params![path, i64::try_from(budget).unwrap_or(i64::MAX), repo_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )?;
        for row in rows {
            let (tracker, project, item_key, ref_kind, source_kind, source_text) = row?;
            surface.push(
                ImpactCategory::HistoricalPapertrail,
                file.clone(),
                "papertrail",
                format!("{tracker}:{project}#{item_key} {ref_kind}/{source_kind}: {source_text}"),
            );
        }
        if surface.len() > before {
            added += 1;
        }
    }
    Ok(())
}

pub(crate) fn papertrail_rationale_for_query(
    conn: &Connection,
    query: &str,
    surface: &mut ImpactSurface,
    limit: usize,
) -> anyhow::Result<()> {
    let fts_query = fts_escape(query);
    if fts_query.is_empty() {
        return Ok(());
    }
    // `papertrail_fts` is one index over every repo's papertrail; the `repo_id` filter is
    // MANDATORY so a MATCH here never surfaces a sibling repo's issue in a consolidated DB.
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT url, title, classification
        FROM papertrail_fts
        WHERE papertrail_fts MATCH ?1 AND repo_id = ?3
        ORDER BY rank
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(
        params![fts_query, i64::try_from(limit).unwrap_or(i64::MAX), repo_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
    )?;
    for row in rows {
        let (url, title, classification) = row?;
        surface.push(
            ImpactCategory::HistoricalPapertrail,
            FileSymbol {
                path: "(papertrail)".to_string(),
                language: "papertrail".to_string(),
                kind: "papertrail".to_string(),
                symbol: None,
            },
            "papertrail",
            format!("{classification}: {title} ({url})"),
        );
    }
    Ok(())
}

pub(crate) fn file_for_path(conn: &Connection, path: &str) -> anyhow::Result<FileSymbol> {
    let row = conn
        .query_row("SELECT path, language, kind FROM files WHERE path = ?1", [path], |row| {
            Ok(FileSymbol {
                path: row.get(0)?,
                language: row.get(1)?,
                kind: row.get(2)?,
                symbol: None,
            })
        })
        .optional()?;
    Ok(row.unwrap_or_else(|| FileSymbol {
        path: path.to_string(),
        language: "unknown".to_string(),
        kind: "historical".to_string(),
        symbol: None,
    }))
}

pub(crate) fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

pub(crate) fn fts_escape(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(|part| format!("\"{}\"", part.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub(crate) fn rows_to_items(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<ImpactItem>>,
) -> anyhow::Result<Vec<ImpactItem>> {
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
}

pub(crate) fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;

    use super::*;

    fn commit(conn: &Connection, hash: &str, path: &str, ts: i64) {
        conn.execute(
            "INSERT OR IGNORE INTO git_commits(hash, author_name, author_email, authored_at_s, \
             committed_at_s, subject, body) VALUES (?1, 'a', 'a@b', ?2, ?2, ?3, '')",
            rusqlite::params![hash, ts, format!("touch {path}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO git_file_changes(commit_hash, path) VALUES (?1, ?2)",
            rusqlite::params![hash, path],
        )
        .unwrap();
    }

    /// #150 review: `git_commits_for_paths` budgets by ITEM (one per path), not by commit row — so a
    /// hot file with many commits can't exhaust the budget and starve a later path's history item.
    #[test]
    fn git_commits_budget_is_per_path_not_per_commit_row() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &rag_rat_core::index::migration_hooks()).unwrap();
        // `hot.rs` has THREE commits; `cool.rs` has one. All of hot.rs's commits collapse to a
        // single `(hot.rs, "git_commit_touched_file")` item.
        commit(&conn, "c1", "hot.rs", 30);
        commit(&conn, "c2", "hot.rs", 20);
        commit(&conn, "c3", "hot.rs", 10);
        commit(&conn, "c4", "cool.rs", 5);

        let mut surface = ImpactSurface::default();
        // Budget of 2 ITEMS. The old per-row budget would spend both slots on hot.rs's commits and
        // never reach cool.rs; the per-path budget must surface BOTH files.
        git_commits_for_paths(
            &conn,
            &["hot.rs".to_string(), "cool.rs".to_string()],
            &mut surface,
            2,
        )
        .unwrap();

        let items = surface.into_items(10);
        let paths: std::collections::BTreeSet<&str> =
            items.iter().map(|item| item.path.as_str()).collect();
        assert!(paths.contains("hot.rs"), "hot.rs item present: {items:?}");
        assert!(
            paths.contains("cool.rs"),
            "cool.rs must not be starved by hot.rs's commits: {items:?}"
        );
        // hot.rs's three commits collapsed into one item carrying multiple evidence lines.
        let hot = items.iter().find(|item| item.path == "hot.rs").unwrap();
        assert!(hot.evidence.len() >= 2, "collapsed commits accumulate evidence: {hot:?}");
    }
}
