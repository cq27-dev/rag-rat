use super::*;
use crate::index::table_row_count;

pub(crate) fn sync_from_refs<C: GitHubClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &GitHubContext,
) -> anyhow::Result<GitHubSyncReport> {
    sync_from_refs_with_progress(conn, root, client, offline, ctx, |_| {})
}
pub(crate) fn sync_from_refs_with_progress<C: GitHubClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &GitHubContext,
    mut progress: impl FnMut(GitHubSyncProgress),
) -> anyhow::Result<GitHubSyncReport> {
    let refs = discover_and_store_refs(conn, root, ctx)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("github sync requires a client"))?;
        sync_refs(conn, client, refs.iter(), &mut progress)?
    };
    set_meta(conn, "github_last_sync_ms", &now_ms().to_string())?;
    Ok(GitHubSyncReport {
        offline,
        discovered_refs: refs.len(),
        skipped_refs: sync.skipped_refs,
        failed_refs: sync.failed_refs,
        synced_items: sync.synced_items,
        errors: sync.errors,
        status: status(conn, ctx)?,
    })
}
pub(crate) fn sync_issue<C: GitHubClient>(
    conn: &Connection,
    issue_ref: &str,
    client: Option<&C>,
    offline: bool,
    ctx: &GitHubContext,
) -> anyhow::Result<GitHubSyncReport> {
    let parsed = parse_issue_ref(issue_ref, ctx.default_repo())
        .ok_or_else(|| anyhow::anyhow!("invalid GitHub issue reference `{issue_ref}`"))?;
    store_ref(conn, &GitHubRef {
        owner: parsed.owner,
        repo: parsed.repo,
        number: parsed.number,
        ref_kind: "unknown".to_string(),
        source_kind: "manual".to_string(),
        source_path: None,
        source_commit: None,
        source_text: issue_ref.to_string(),
    })?;
    let refs = refs(conn)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("github sync requires a client"))?;
        sync_refs(conn, client, refs.iter().filter(|r| r.number == parsed.number), &mut |_| {})?
    };
    set_meta(conn, "github_last_sync_ms", &now_ms().to_string())?;
    Ok(GitHubSyncReport {
        offline,
        discovered_refs: refs.len(),
        skipped_refs: sync.skipped_refs,
        failed_refs: sync.failed_refs,
        synced_items: sync.synced_items,
        errors: sync.errors,
        status: status(conn, ctx)?,
    })
}
pub(crate) fn status(conn: &Connection, ctx: &GitHubContext) -> anyhow::Result<GitHubStatus> {
    Ok(GitHubStatus {
        refs: table_row_count(conn, "github_refs")?,
        issues: table_row_count(conn, "github_issues")?,
        comments: table_row_count(conn, "github_comments")?,
        pulls: table_row_count(conn, "github_pull_requests")?,
        reviews: table_row_count(conn, "github_reviews")?,
        review_comments: table_row_count(conn, "github_review_comments")?,
        last_sync_ms: meta(conn, "github_last_sync_ms")?.and_then(|value| value.parse().ok()),
        capability: if ctx.gh_available {
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
) -> anyhow::Result<Vec<GitHubEvidence>> {
    search_fts(conn, query, Some("issue"), limit)
}
pub(crate) fn rationale_search(
    conn: &Connection,
    query: &str,
    limit: u32,
    ctx: &GitHubContext,
) -> anyhow::Result<Vec<GitHubEvidence>> {
    let mut evidence = Vec::new();
    for reference in parse_refs(query, ctx.default_repo()) {
        evidence.extend(evidence_for_issue(
            conn,
            &reference.owner,
            &reference.repo,
            reference.number,
            limit,
        )?);
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
) -> anyhow::Result<Vec<GitHubRef>> {
    let mut stmt = conn.prepare(
        "
        SELECT owner, repo, number, ref_kind, source_kind, source_path, source_commit, source_text
        FROM github_refs
        WHERE source_path = ?1
        ORDER BY id DESC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![path, i64::from(limit)], ref_row)?;
    collect_rows(rows)
}
pub(crate) fn papertrail_for_chunk(
    conn: &Connection,
    chunk: &crate::query::ReadChunk,
    limit: u32,
    ctx: &GitHubContext,
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
        github_evidence: evidence,
        fallback_github_evidence: Vec::new(),
    })
}
pub(crate) fn papertrail_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    limit: u32,
    ctx: &GitHubContext,
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
        github_evidence: evidence,
        fallback_github_evidence: Vec::new(),
    })
}
pub(crate) fn papertrail_for_commit(
    conn: &Connection,
    commit_hash: &str,
    limit: u32,
    ctx: &GitHubContext,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_commit_refs(conn, commit_hash, limit)?;
    let mut fallback_evidence = Vec::new();
    if evidence.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT path FROM git_file_changes WHERE commit_hash LIKE ?1 ORDER BY path LIMIT ?2",
        )?;
        let commit_like = format!("{commit_hash}%");
        let rows =
            stmt.query_map(params![commit_like, i64::from(limit)], |row| row.get::<_, String>(0))?;
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
    Ok(Papertrail {
        current_source: None,
        github_evidence: evidence,
        fallback_github_evidence: fallback_evidence,
    })
}
pub(crate) fn mark_fallback_evidence(evidence: &mut [GitHubEvidence]) {
    for item in evidence {
        item.evidence_kind = match item.evidence_kind {
            "literal_github_ref" => "fallback_literal_github_ref",
            "historical_github" => "fallback_historical_github",
            _ => "fallback_github_evidence",
        };
        item.score = item.score.min(0.25);
    }
}
