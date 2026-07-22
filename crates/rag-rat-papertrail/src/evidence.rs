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
    symbol: &super::api::SymbolRef<'_>,
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
        // Populated by `attach_records` after the hit set is built; a bare match carries none.
        record: None,
    })
}

/// Attach the distilled decision record (#705) to every hit whose thread has one, resolving the
/// active repo once and caching per thread so many comment hits on one item load the record once.
/// A hit for a merged PR coalesced into an issue picks up the ISSUE's record (redirect-aware).
pub(crate) fn attach_records(
    conn: &Connection,
    evidence: &mut [PapertrailEvidence],
) -> anyhow::Result<()> {
    if evidence.is_empty() {
        return Ok(());
    }
    // The distilled-record store is OPTIONAL enrichment: an index built before the distill schema
    // (or a partial-migration state — e.g. a legacy backfill that has `papertrail_fts` but not yet
    // the distill tables) must still serve bare search hits. Attaching is a no-op there rather than
    // erroring the whole search on a missing table.
    if !rag_rat_db::schema::table_exists(conn, "papertrail_distill")? {
        return Ok(());
    }
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let mut cache: std::collections::HashMap<RecordKey, Option<DistilledRecord>> =
        std::collections::HashMap::new();
    for hit in evidence.iter_mut() {
        let key = RecordKey {
            tracker: hit.tracker.clone(),
            project: hit.project.clone(),
            item_kind: hit.item_kind.clone(),
            item_key: hit.item_key.clone(),
        };
        let record = match cache.get(&key) {
            Some(cached) => cached.clone(),
            None => {
                let loaded = distilled_record_for_thread(conn, &repo_id, &key)?;
                cache.insert(key, loaded.clone());
                loaded
            },
        };
        hit.record = record;
    }
    Ok(())
}

/// Collapse coalesced hits so one distilled record answers as ONE result (#705): all hits sharing a
/// canonical record (an issue and the merged PR(s) coalesced into it) reduce to the BEST-RANKED
/// thread that carried it — keeping the strongest textual match's position and snippet, since the
/// same record is attached whichever thread survives. Hits keyed to that chosen thread survive (so
/// a comment and its item, which share a thread identity, both stay); hits keyed to a DIFFERENT
/// thread that resolved to the same record collapse away. Hits with no record are distinct bare
/// matches and are never touched. The canonical/thread identity is the FULL key
/// (tracker + project + item_kind + item_key) so records with the same number in different projects
/// a single repo mirrors never collide. Call after [`attach_records`]; input is in rank order.
pub(crate) fn coalesce_pairs(evidence: &mut Vec<PapertrailEvidence>) {
    type ThreadKey = (String, String, String, String);
    let raw_key = |e: &PapertrailEvidence| -> ThreadKey {
        (e.tracker.clone(), e.project.clone(), e.item_kind.clone(), e.item_key.clone())
    };
    let canonical_key = |r: &DistilledRecord| -> ThreadKey {
        (r.tracker.clone(), r.project.clone(), r.item_kind.clone(), r.item_key.clone())
    };
    // The representative thread for each record is the FIRST (best-ranked) hit that carried it.
    let mut representative: std::collections::HashMap<ThreadKey, ThreadKey> =
        std::collections::HashMap::new();
    for hit in evidence.iter() {
        if let Some(record) = &hit.record {
            representative.entry(canonical_key(record)).or_insert_with(|| raw_key(hit));
        }
    }
    evidence.retain(|hit| match &hit.record {
        Some(record) =>
            representative.get(&canonical_key(record)).is_none_or(|rep| *rep == raw_key(hit)),
        None => true,
    });
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
// Not test-gated: the engine crate's schema tests drive this cross-crate (test support only).
pub fn refs(conn: &Connection) -> anyhow::Result<Vec<PapertrailRef>> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "SELECT tracker, project, item_key, item_kind, ref_kind, source_kind, source_path, \
         source_commit, source_text FROM papertrail_refs WHERE repo_id = ?1",
    )?;
    let rows = stmt.query_map([repo_id], ref_row)?;
    collect_rows(rows)
}

#[cfg(test)]
mod payload_tests {
    use rusqlite::Connection;

    use super::{attach_records, coalesce_pairs};
    use crate::{DistilledRecord, FixEdgeSource, OutcomeStatus, PapertrailEvidence, ThreadShape};

    fn ev(kind: &str, key: &str, record: Option<DistilledRecord>) -> PapertrailEvidence {
        ev_in("o/r", kind, key, record)
    }

    fn ev_in(
        project: &str,
        kind: &str,
        key: &str,
        record: Option<DistilledRecord>,
    ) -> PapertrailEvidence {
        PapertrailEvidence {
            tracker: "github".into(),
            project: project.into(),
            item_kind: kind.into(),
            item_key: key.into(),
            doc_kind: "item".into(),
            comment_id: None,
            url: String::new(),
            title: String::new(),
            snippet: String::new(),
            classification: String::new(),
            evidence_kind: "historical_tracker",
            score: 1.0,
            record,
        }
    }

    fn record_for(kind: &str, key: &str) -> DistilledRecord {
        record_in("o/r", kind, key)
    }

    fn record_in(project: &str, kind: &str, key: &str) -> DistilledRecord {
        DistilledRecord {
            tracker: "github".into(),
            project: project.into(),
            item_kind: kind.into(),
            item_key: key.into(),
            root_issue: None,
            root_cause: None,
            root_cause_class: None,
            decision_chosen: None,
            rejected_alternatives: vec![],
            outcome_status: OutcomeStatus::Landed,
            outcome_status_model: None,
            outcome_summary: None,
            epistemic_status_decision: None,
            epistemic_status_outcome: None,
            fix_edge_source: FixEdgeSource::Provider,
            thread_shape: ThreadShape::Investigation,
            outcome_claim_verified: false,
            decision_provenance_verified: false,
            anchors_qualified_count: 0,
            fixing_commits: vec![],
            coalesced: vec![],
        }
    }

    #[test]
    fn coalesce_pairs_drops_a_redirected_pr_when_its_issue_also_matched() {
        // issue#5 owns the record; PR#6 was redirected to #5's record; issue#9 is unrelated.
        let mut hits = vec![
            ev("issue", "5", Some(record_for("issue", "5"))),
            ev("change_request", "6", Some(record_for("issue", "5"))),
            ev("issue", "9", Some(record_for("issue", "9"))),
        ];
        coalesce_pairs(&mut hits);
        let keys: Vec<_> =
            hits.iter().map(|h| (h.item_kind.as_str(), h.item_key.as_str())).collect();
        assert_eq!(
            keys,
            vec![("issue", "5"), ("issue", "9")],
            "the coalesced PR collapses into the issue that owns the record",
        );
    }

    #[test]
    fn coalesce_pairs_collapses_two_prs_of_one_issue_when_the_issue_is_absent() {
        // Two merged PRs closed issue #5 and both matched, but the issue itself did not. Both
        // resolve to #5's record — they must still answer as ONE result (the best-ranked PR), not
        // two rows for the same record.
        let mut hits = vec![
            ev("change_request", "6", Some(record_for("issue", "5"))),
            ev("change_request", "8", Some(record_for("issue", "5"))),
        ];
        coalesce_pairs(&mut hits);
        assert_eq!(hits.len(), 1, "the two PRs collapse to one result");
        assert_eq!(hits[0].item_key, "6", "the best-ranked PR represents the record");
    }

    #[test]
    fn coalesce_pairs_keeps_the_best_ranked_hit_even_when_it_is_the_pr() {
        // PR#6 (redirects to issue#5's record) ranks ABOVE issue#5. The strong PR match must
        // survive — carrying the record — and the lower-ranked issue collapses away, so the record
        // answers at the strongest match's position rather than being demoted to the issue's rank.
        let mut hits = vec![
            ev("change_request", "6", Some(record_for("issue", "5"))),
            ev("issue", "5", Some(record_for("issue", "5"))),
        ];
        coalesce_pairs(&mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            (hits[0].item_kind.as_str(), hits[0].item_key.as_str()),
            ("change_request", "6")
        );
        assert_eq!(
            hits[0].record.as_ref().unwrap().item_key,
            "5",
            "still carries the issue record"
        );
    }

    #[test]
    fn coalesce_pairs_does_not_collapse_the_same_number_across_projects() {
        // A single repo mirrors two projects; issue #5 exists in each with its own record. They
        // share (item_kind, item_key) but differ by project — the full thread key must keep them
        // as two distinct results.
        let mut hits = vec![
            ev_in("proj/a", "issue", "5", Some(record_in("proj/a", "issue", "5"))),
            ev_in("proj/b", "issue", "5", Some(record_in("proj/b", "issue", "5"))),
        ];
        coalesce_pairs(&mut hits);
        assert_eq!(hits.len(), 2, "same number in different projects stays two results");
    }

    #[test]
    fn coalesce_pairs_keeps_a_lone_pr_carrying_its_issue_record() {
        // Only the PR matched; its owning issue is not a separate hit → keep the PR hit, which
        // still carries the issue's record via the redirect.
        let mut hits = vec![ev("change_request", "6", Some(record_for("issue", "5")))];
        coalesce_pairs(&mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item_key, "6");
        assert_eq!(hits[0].record.as_ref().unwrap().item_key, "5");
    }

    #[test]
    fn attach_records_populates_hits_from_the_store_and_leaves_bare_hits_bare() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply_distill_record_store(&conn).unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', \
             'repoA')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  root_issue, fix_edge_source, thread_shape, distilled_at_ms, repo_id)
             VALUES ('github','o/r','issue','5','sha256:h',3,'The bug.','provider','investigation',
                     1,'repoA')",
            [],
        )
        .unwrap();

        let mut hits = vec![ev("issue", "5", None), ev("issue", "404", None)];
        attach_records(&conn, &mut hits).unwrap();
        assert_eq!(
            hits[0].record.as_ref().unwrap().root_issue.as_deref(),
            Some("The bug."),
            "a distilled thread's hit carries the record",
        );
        assert!(hits[1].record.is_none(), "a thread without a record stays a bare match");
    }

    #[test]
    fn attach_records_is_a_no_op_when_the_distill_store_is_absent() {
        // A pre-distill or partially-migrated index has papertrail_fts but no papertrail_distill;
        // search must still serve bare hits rather than erroring on the missing table.
        let conn = Connection::open_in_memory().unwrap();
        let mut hits = vec![ev("issue", "5", None)];
        attach_records(&conn, &mut hits).unwrap();
        assert!(hits[0].record.is_none(), "no distill store → bare hit, no error");
    }

    #[test]
    fn classification_is_never_serialized_to_a_read_surface() {
        // #705: the coarse keyword label is internal-only (the eval harness scores against it); the
        // distilled `record` supersedes it as the decision signal. A populated `classification`
        // must NOT reach the JSON a retrieval consumer sees, while ordinary fields still do.
        let mut hit = ev("issue", "5", None);
        hit.classification = "decision".into();
        let json = serde_json::to_string(&hit).unwrap();
        assert!(!json.contains("classification"), "classification must not surface: {json}");
        assert!(json.contains("\"item_key\":\"5\""), "ordinary fields still serialize: {json}");
    }
}
