use super::*;

pub(crate) fn evidence_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let refs = refs_for_path(conn, path, limit)?;
    let mut evidence = Vec::new();
    for reference in refs {
        evidence.extend(evidence_for_issue(
            conn,
            &reference.owner,
            &reference.repo,
            reference.number,
            limit,
        )?);
    }
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(evidence)
}
pub(crate) fn current_symbol_span(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
) -> anyhow::Result<(Option<i64>, Option<i64>, Option<i64>)> {
    let span = conn
        .query_row(
            "
            SELECT chunks.id, chunks.start_line, chunks.end_line
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            WHERE files.path = ?1
              AND (chunks.symbol_path = ?2 OR chunks.symbol_path = ?3)
            ORDER BY
              CASE WHEN chunks.symbol_path = ?2 THEN 0 ELSE 1 END,
              chunks.start_line
            LIMIT 1
            ",
            params![symbol.path, symbol.qualified_name, symbol.symbol_path],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
        )
        .optional()?;
    Ok(match span {
        Some((chunk_id, start_line, end_line)) =>
            (Some(start_line), Some(end_line), Some(chunk_id)),
        None => (None, None, None),
    })
}
pub(crate) fn evidence_for_issue(
    conn: &Connection,
    owner: &str,
    repo: &str,
    number: i64,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT owner, repo, number, item_kind, item_id, url, title, body, classification, 0.0
        FROM github_fts
        WHERE owner = ?1 AND repo = ?2 AND number = ?3 AND repo_id = ?5
        LIMIT ?4
        ",
    )?;
    let rows =
        stmt.query_map(params![owner, repo, number, i64::from(limit), repo_id], evidence_row)?;
    let mut evidence = collect_rows(rows)?;
    for item in &mut evidence {
        item.evidence_kind = "literal_github_ref";
        item.score = 1.0;
    }
    Ok(evidence)
}
pub(crate) fn evidence_for_commit_refs(
    conn: &Connection,
    commit_hash: &str,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT owner, repo, number
        FROM github_refs
        WHERE source_kind = 'commit'
          AND source_commit LIKE ?1
          AND repo_id = ?3
        ORDER BY ref_kind = 'closing' DESC, id DESC
        LIMIT ?2
        ",
    )?;
    let commit_like = format!("{commit_hash}%");
    let refs = stmt.query_map(params![commit_like, i64::from(limit), repo_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
    })?;
    let mut evidence = Vec::new();
    for reference in refs {
        let (owner, repo, number) = reference?;
        evidence.extend(evidence_for_issue(conn, &owner, &repo, number, limit)?);
    }
    dedupe_evidence(&mut evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(evidence)
}
pub(crate) fn search_fts(
    conn: &Connection,
    query: &str,
    kind: Option<&str>,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let fts_query = fts_query(query);
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    // The `repo_id` filter is MANDATORY (V041): `github_fts` is one index over every repo's
    // papertrail in a consolidated DB, so a bare MATCH would surface a sibling repo's issues. Its
    // parameter index trails the optional `item_kind` bind, so it shifts with the kind clause.
    let (kind_clause, repo_index) = match kind {
        Some(_) => ("AND item_kind = ?3", 4),
        None => ("", 3),
    };
    let sql = format!(
        "
        SELECT owner, repo, number, item_kind, item_id, url, title, body, classification,
               bm25(github_fts) AS score
        FROM github_fts
        WHERE github_fts MATCH ?1
        {kind_clause}
          AND repo_id = ?{repo_index}
        ORDER BY score
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(kind) = kind {
        stmt.query_map(params![fts_query, i64::from(limit), kind, repo_id], evidence_row)?
    } else {
        stmt.query_map(params![fts_query, i64::from(limit), repo_id], evidence_row)?
    };
    let mut hits = collect_rows(rows)?;
    for (rank, hit) in hits.iter_mut().enumerate() {
        hit.score = positive_rank_score(rank);
    }
    Ok(hits)
}
pub(crate) fn positive_rank_score(rank: usize) -> f64 {
    1.0 / ((rank + 1) as f64).sqrt()
}
pub(crate) fn dedupe_evidence(evidence: &mut Vec<PapertrailEvidence>) {
    let mut seen = BTreeSet::new();
    evidence.retain(|item| {
        seen.insert((
            item.owner.clone(),
            item.repo.clone(),
            item.number,
            item.item_kind.clone(),
            item.item_id.clone(),
        ))
    });
}
pub(crate) fn evidence_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PapertrailEvidence> {
    let title: String = row.get(6)?;
    let body: String = row.get(7)?;
    Ok(PapertrailEvidence {
        owner: row.get(0)?,
        repo: row.get(1)?,
        number: row.get(2)?,
        item_kind: row.get(3)?,
        item_id: row.get(4)?,
        url: row.get(5)?,
        title,
        snippet: snippet(&body),
        classification: row.get(8)?,
        evidence_kind: "historical_github",
        score: row.get(9)?,
    })
}
pub(crate) fn ref_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PapertrailRef> {
    Ok(PapertrailRef {
        owner: row.get(0)?,
        repo: row.get(1)?,
        number: row.get(2)?,
        ref_kind: row.get(3)?,
        source_kind: row.get(4)?,
        source_path: row.get(5)?,
        source_commit: row.get(6)?,
        source_text: row.get(7)?,
    })
}
pub(crate) fn refs(conn: &Connection) -> anyhow::Result<Vec<PapertrailRef>> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "SELECT owner, repo, number, ref_kind, source_kind, source_path, source_commit, \
         source_text FROM github_refs WHERE repo_id = ?1",
    )?;
    let rows = stmt.query_map([repo_id], ref_row)?;
    collect_rows(rows)
}
