use super::*;
use crate::index::{repo_meta, schema, scoped_table_row_count, set_repo_meta};

pub(crate) async fn sync_from_refs<C: PapertrailClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailSyncReport> {
    sync_from_refs_with_progress(conn, root, client, offline, ctx, |_| {}).await
}
pub(crate) async fn sync_from_refs_with_progress<C: PapertrailClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
    mut progress: impl FnMut(PapertrailSyncProgress),
) -> anyhow::Result<PapertrailSyncReport> {
    let refs = discover_and_store_refs(conn, root, ctx)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("papertrail sync requires a client"))?;
        // The stacked schema lets production discovery persist every provider's refs. Live
        // transport is still GitHub-only until #589 lands, so never send another provider's
        // identity through GitHubClient. Multi-provider mirror dispatch is the remaining #590
        // sync slice, intentionally outside this config/grammar PR.
        sync_refs(
            conn,
            client,
            refs.iter().filter(|reference| reference.tracker == Tracker::Github),
            &mut progress,
        )
        .await?
    };
    let repo_id = schema::active_repo_id(conn)?;
    set_repo_meta(conn, &repo_id, "papertrail_last_sync_ms", &now_ms().to_string())?;
    Ok(PapertrailSyncReport {
        offline,
        discovered_refs: refs.len(),
        skipped_refs: sync.skipped_refs,
        failed_refs: sync.failed_refs,
        synced_items: sync.synced_items,
        errors: sync.errors,
        status: status(conn, ctx)?,
    })
}
pub(crate) async fn sync_issue<C: PapertrailClient>(
    conn: &Connection,
    issue_ref: &str,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailSyncReport> {
    let parsed = parse_issue_ref(issue_ref, ctx.default_repo())
        .ok_or_else(|| anyhow::anyhow!("invalid tracker item reference `{issue_ref}`"))?;
    let project = parsed.project.clone();
    let item_key = parsed.number.to_string();
    store_ref(conn, &parsed.into_ref("manual", None, None, issue_ref.to_string()))?;
    let refs = refs(conn)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("papertrail sync requires a client"))?;
        sync_refs(
            conn,
            client,
            refs.iter().filter(|reference| {
                reference.tracker == Tracker::Github
                    && reference.project == project
                    && reference.item_key == item_key
            }),
            &mut |_| {},
        )
        .await?
    };
    let repo_id = schema::active_repo_id(conn)?;
    set_repo_meta(conn, &repo_id, "papertrail_last_sync_ms", &now_ms().to_string())?;
    Ok(PapertrailSyncReport {
        offline,
        discovered_refs: refs.len(),
        skipped_refs: sync.skipped_refs,
        failed_refs: sync.failed_refs,
        synced_items: sync.synced_items,
        errors: sync.errors,
        status: status(conn, ctx)?,
    })
}
pub(crate) fn status(
    conn: &Connection,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailStatus> {
    // The papertrail_* tables are direct-scoped; report only the ACTIVE repo's counts, not the
    // union across a consolidated DB.
    let repo_id = schema::active_repo_id(conn)?;
    let items_by_kind = |kind: ItemKind| -> anyhow::Result<u64> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM papertrail_items WHERE item_kind = ?1 AND repo_id = ?2",
            params![kind.as_db_str(), repo_id],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or_default())
    };
    Ok(PapertrailStatus {
        refs: scoped_table_row_count(conn, "papertrail_refs", &repo_id)?,
        issues: items_by_kind(ItemKind::Issue)?,
        change_requests: items_by_kind(ItemKind::ChangeRequest)?,
        comments: scoped_table_row_count(conn, "papertrail_comments", &repo_id)?,
        last_sync_ms: repo_meta(conn, &repo_id, "papertrail_last_sync_ms")?
            .and_then(|value| value.parse().ok()),
        capability: if ctx.github_cli_available {
            "gh_cli_available".to_string()
        } else {
            "gh_cli_missing".to_string()
        },
    })
}
pub(crate) fn issue_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    // Item rows only (issues AND change requests — their titles/bodies), not comment rows.
    search_fts(conn, query, Some("item"), limit)
}
pub(crate) fn rationale_search(
    conn: &Connection,
    query: &str,
    limit: u32,
    ctx: &PapertrailContext,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let mut evidence = Vec::new();
    for parsed in parse_tracker_refs(query, &ctx.trackers) {
        evidence.extend(evidence_for_item(
            conn,
            parsed.provider,
            &parsed.project,
            &parsed.item_key,
            parsed.item_kind,
            limit,
        )?);
    }
    if ctx.trackers.is_empty() {
        for parsed in parse_refs(query, None) {
            evidence.extend(evidence_for_item(
                conn,
                Tracker::Github,
                &parsed.project,
                &parsed.number.to_string(),
                None,
                limit,
            )?);
        }
    }
    evidence.extend(search_fts(conn, query, None, limit)?);
    dedupe_evidence(&mut evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(evidence)
}
pub(crate) fn refs_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailRef>> {
    let repo_id = schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT tracker, project, item_key, item_kind, ref_kind, source_kind, source_path, \
         source_commit, source_text
        FROM papertrail_refs
        WHERE source_path = ?1 AND repo_id = ?3
        ORDER BY id DESC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![path, i64::from(limit), repo_id], ref_row)?;
    collect_rows(rows)
}
pub(crate) fn papertrail_for_chunk(
    conn: &Connection,
    chunk: &crate::query::ReadChunk,
    limit: u32,
    ctx: &PapertrailContext,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_path(conn, &chunk.path, limit)?;
    if evidence.is_empty() {
        evidence = rationale_search(conn, &chunk.path, limit, ctx)?;
    }
    Ok(Papertrail {
        current_source: Some(CurrentSourceEvidence {
            chunk_id: Some(chunk.chunk_id),
            path: chunk.path.clone(),
            start_line: Some(chunk.start_line),
            end_line: Some(chunk.end_line),
            symbol: chunk.symbol_path.clone(),
        }),
        evidence,
        fallback_evidence: Vec::new(),
    })
}
pub(crate) fn papertrail_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    limit: u32,
    ctx: &PapertrailContext,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_path(conn, &symbol.path, limit)?;
    evidence.extend(rationale_search(conn, &symbol.qualified_name, limit, ctx)?);
    dedupe_evidence(&mut evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let (start_line, end_line, chunk_id) = current_symbol_span(conn, symbol)?;
    Ok(Papertrail {
        current_source: Some(CurrentSourceEvidence {
            chunk_id,
            path: symbol.path.clone(),
            start_line,
            end_line,
            symbol: Some(symbol.qualified_name.clone()),
        }),
        evidence,
        fallback_evidence: Vec::new(),
    })
}
pub(crate) fn papertrail_for_commit(
    conn: &Connection,
    commit_hash: &str,
    limit: u32,
    ctx: &PapertrailContext,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_commit_refs(conn, commit_hash, limit)?;
    let mut fallback_evidence = Vec::new();
    if evidence.is_empty() {
        // `git_file_changes` is direct-scoped (V040): the prefix probe must not resolve a sibling
        // repo's commit in a consolidated DB (forks share hashes).
        let repo_id = schema::active_repo_id(conn)?;
        let mut stmt = conn.prepare(
            "SELECT path FROM git_file_changes WHERE commit_hash LIKE ?1 AND repo_id = ?3 ORDER \
             BY path LIMIT ?2",
        )?;
        let commit_like = format!("{commit_hash}%");
        let rows = stmt.query_map(params![commit_like, i64::from(limit), repo_id], |row| {
            row.get::<_, String>(0)
        })?;
        for row in rows {
            fallback_evidence.extend(evidence_for_path(conn, &row?, limit)?);
        }
        fallback_evidence.extend(rationale_search(conn, commit_hash, limit, ctx)?);
        mark_fallback_evidence(&mut fallback_evidence);
    }
    dedupe_evidence(&mut evidence);
    dedupe_evidence(&mut fallback_evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    fallback_evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(Papertrail { current_source: None, evidence, fallback_evidence })
}
pub(crate) fn mark_fallback_evidence(evidence: &mut [PapertrailEvidence]) {
    for item in evidence {
        item.evidence_kind = match item.evidence_kind {
            "literal_tracker_ref" => "fallback_literal_tracker_ref",
            "historical_tracker" => "fallback_historical_tracker",
            _ => "fallback_tracker_evidence",
        };
        item.score = item.score.min(0.25);
    }
}
