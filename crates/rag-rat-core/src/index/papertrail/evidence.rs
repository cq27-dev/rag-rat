use super::*;

pub(crate) fn evidence_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let refs = refs_for_path(conn, path, limit)?;
    let mut evidence = Vec::new();
    for reference in refs {
        evidence.extend(evidence_for_item(
            conn,
            reference.tracker,
            &reference.project,
            &reference.item_key,
            reference.item_kind,
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
/// Every mirror row (the item's own text AND its comments) for ONE tracker item. No `item_kind`
/// filter: refs don't know the kind of the item they name, and either cached kind is the item.
pub(crate) fn evidence_for_item(
    conn: &Connection,
    tracker: Tracker,
    project: &str,
    item_key: &str,
    item_kind: Option<ItemKind>,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT tracker, project, item_kind, item_key, doc_kind, comment_id, url, title, body, \
         classification, 0.0
        FROM papertrail_fts
        WHERE tracker = ?1 AND project = ?2 AND item_key = ?3 AND repo_id = ?5
          AND (?6 IS NULL OR item_kind = ?6)
        LIMIT ?4
        ",
    )?;
    let rows = stmt.query_map(
        params![
            tracker.as_db_str(),
            project,
            item_key,
            i64::from(limit),
            repo_id,
            item_kind.map(ItemKind::as_db_str)
        ],
        evidence_row,
    )?;
    let mut evidence = collect_rows(rows)?;
    for item in &mut evidence {
        item.evidence_kind = "literal_tracker_ref";
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
        SELECT tracker, project, item_key, item_kind
        FROM papertrail_refs
        WHERE source_kind = 'commit'
          AND source_commit LIKE ?1
          AND repo_id = ?3
        ORDER BY ref_kind = 'closing' DESC, id DESC
        LIMIT ?2
        ",
    )?;
    let commit_like = format!("{commit_hash}%");
    let refs = stmt.query_map(params![commit_like, i64::from(limit), repo_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut evidence = Vec::new();
    for reference in refs {
        let (tracker, project, item_key, item_kind) = reference?;
        evidence.extend(evidence_for_item(
            conn,
            Tracker::from_db_str(&tracker)?,
            &project,
            &item_key,
            item_kind.as_deref().map(ItemKind::from_db_str).transpose()?,
            limit,
        )?);
    }
    dedupe_evidence(&mut evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(evidence)
}
pub(crate) fn search_fts(
    conn: &Connection,
    query: &str,
    doc_kind: Option<&str>,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let fts_query = fts_query(query);
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    // The `repo_id` filter is MANDATORY (V041): `papertrail_fts` is one index over every repo's
    // papertrail in a consolidated DB, so a bare MATCH would surface a sibling repo's issues. Its
    // parameter index trails the optional `doc_kind` bind, so it shifts with the kind clause.
    let (kind_clause, repo_index) = match doc_kind {
        Some(_) => ("AND doc_kind = ?3", 4),
        None => ("", 3),
    };
    let sql = format!(
        "
        SELECT tracker, project, item_kind, item_key, doc_kind, comment_id, url, title, body, \
         classification,
               bm25(papertrail_fts) AS score
        FROM papertrail_fts
        WHERE papertrail_fts MATCH ?1
        {kind_clause}
          AND repo_id = ?{repo_index}
        ORDER BY score
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(doc_kind) = doc_kind {
        stmt.query_map(params![fts_query, i64::from(limit), doc_kind, repo_id], evidence_row)?
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
            item.tracker.clone(),
            item.project.clone(),
            item.item_kind.clone(),
            item.item_key.clone(),
            item.doc_kind.clone(),
            item.comment_id.clone(),
        ))
    });
}
pub(crate) fn evidence_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PapertrailEvidence> {
    let doc_kind: String = row.get(4)?;
    let comment_id: String = row.get(5)?;
    let title: String = row.get(7)?;
    let body: String = row.get(8)?;
    Ok(PapertrailEvidence {
        tracker: row.get(0)?,
        project: row.get(1)?,
        item_kind: row.get(2)?,
        item_key: row.get(3)?,
        doc_kind,
        comment_id: (!comment_id.is_empty()).then_some(comment_id),
        url: row.get(6)?,
        title,
        snippet: snippet(&body),
        classification: row.get(9)?,
        evidence_kind: "historical_tracker",
        score: row.get(10)?,
    })
}
pub(crate) fn ref_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PapertrailRef> {
    let tracker: String = row.get(0)?;
    Ok(PapertrailRef {
        tracker: Tracker::from_db_str(&tracker).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown tracker token `{tracker}`").into(),
            )
        })?,
        project: row.get(1)?,
        item_key: row.get(2)?,
        item_kind: row
            .get::<_, Option<String>>(3)?
            .map(|kind| ItemKind::from_db_str(&kind))
            .transpose()
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    err.into(),
                )
            })?,
        ref_kind: row.get(4)?,
        source_kind: row.get(5)?,
        source_path: row.get(6)?,
        source_commit: row.get(7)?,
        source_text: row.get(8)?,
    })
}
#[cfg(test)]
pub(crate) fn refs(conn: &Connection) -> anyhow::Result<Vec<PapertrailRef>> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "SELECT tracker, project, item_key, item_kind, ref_kind, source_kind, source_path, \
         source_commit, source_text FROM papertrail_refs WHERE repo_id = ?1",
    )?;
    let rows = stmt.query_map([repo_id], ref_row)?;
    collect_rows(rows)
}
