use super::*;

pub(crate) fn discover_and_store_refs(
    conn: &Connection,
    root: &Path,
    ctx: &PapertrailContext,
) -> anyhow::Result<Vec<PapertrailRef>> {
    let default_repo = ctx.default_repo().map(str::to_string);
    let mut refs = Vec::new();
    discover_commit_refs(conn, default_repo.as_deref(), &mut refs)?;
    discover_file_refs(conn, root, default_repo.as_deref(), &mut refs)?;
    let branch = crate::index::git_context::discover_repo(root)
        .ok()
        .and_then(|repo| repo.head_name().ok().flatten())
        .map(|name| name.shorten().to_string())
        .unwrap_or_default();
    for parsed in parse_refs(&branch, default_repo.as_deref()) {
        refs.push(PapertrailRef {
            owner: parsed.owner,
            repo: parsed.repo,
            number: parsed.number,
            ref_kind: parsed.kind,
            source_kind: "branch".to_string(),
            source_path: None,
            source_commit: None,
            source_text: branch.clone(),
        });
    }
    let mut unique = BTreeSet::new();
    refs.retain(|r| {
        unique.insert((
            r.owner.clone(),
            r.repo.clone(),
            r.number,
            r.source_kind.clone(),
            r.source_path.clone(),
            r.source_commit.clone(),
            r.source_text.clone(),
        ))
    });
    for reference in &refs {
        store_ref(conn, reference)?;
    }
    Ok(refs)
}
pub(crate) async fn sync_refs<'a, C: PapertrailClient>(
    conn: &Connection,
    client: &C,
    refs: impl Iterator<Item = &'a PapertrailRef>,
    progress: &mut impl FnMut(PapertrailSyncProgress),
) -> anyhow::Result<SyncRefsReport> {
    let refs = refs.collect::<Vec<_>>();
    let total = refs
        .iter()
        .map(|reference| (reference.owner.clone(), reference.repo.clone(), reference.number))
        .collect::<BTreeSet<_>>()
        .len();
    let mut report = SyncRefsReport::default();
    let mut seen = BTreeSet::new();
    for reference in refs {
        if !seen.insert((reference.owner.clone(), reference.repo.clone(), reference.number)) {
            continue;
        }
        let current = seen.len();
        if papertrail_ref_synced(conn, reference)? {
            report.skipped_refs += 1;
            progress(sync_progress(reference, current, total, PapertrailSyncAction::Skipped, None));
            continue;
        }
        progress(sync_progress(reference, current, total, PapertrailSyncAction::Syncing, None));
        match sync_one_ref(conn, client, reference).await {
            Ok(items) => {
                report.synced_items += items;
                mark_ref_sync(conn, reference, "synced", None)?;
                progress(sync_progress(
                    reference,
                    current,
                    total,
                    PapertrailSyncAction::Synced,
                    None,
                ));
            },
            Err(err) => {
                let message = err.to_string();
                let status = if is_not_found_error(&message) { "not_found" } else { "failed" };
                mark_ref_sync(conn, reference, status, Some(&message))?;
                report.failed_refs += 1;
                report.errors.push(PapertrailSyncError {
                    owner: reference.owner.clone(),
                    repo: reference.repo.clone(),
                    number: reference.number,
                    status: status.to_string(),
                    error: message.clone(),
                });
                progress(sync_progress(
                    reference,
                    current,
                    total,
                    PapertrailSyncAction::Failed,
                    Some(message),
                ));
            },
        }
    }
    progress(PapertrailSyncProgress {
        current: total,
        total,
        owner: String::new(),
        repo: String::new(),
        number: 0,
        action: PapertrailSyncAction::RebuildingFts,
        message: None,
    });
    rebuild_fts(conn)?;
    Ok(report)
}
pub(crate) async fn sync_one_ref<C: PapertrailClient>(
    conn: &Connection,
    client: &C,
    reference: &PapertrailRef,
) -> anyhow::Result<usize> {
    let project = format!("{}/{}", reference.owner, reference.repo);
    let key = reference.number.to_string();
    let item = client.item(&project, &key).await?;
    let mut synced = store_item(conn, &item)?;
    for comment in client.item_comments(&project, &key).await? {
        store_comment(conn, &comment)?;
        synced += 1;
    }
    Ok(synced)
}
pub(crate) fn sync_progress(
    reference: &PapertrailRef,
    current: usize,
    total: usize,
    action: PapertrailSyncAction,
    message: Option<String>,
) -> PapertrailSyncProgress {
    PapertrailSyncProgress {
        current,
        total,
        owner: reference.owner.clone(),
        repo: reference.repo.clone(),
        number: reference.number,
        action,
        message,
    }
}
pub(crate) fn papertrail_ref_synced(
    conn: &Connection,
    reference: &PapertrailRef,
) -> anyhow::Result<bool> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let status = conn
        .query_row(
            "
            SELECT status
            FROM github_ref_sync
            WHERE owner = ?1 AND repo = ?2 AND number = ?3 AND repo_id = ?4
            ",
            params![reference.owner, reference.repo, reference.number, repo_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if matches!(status.as_deref(), Some("synced" | "not_found")) {
        return Ok(true);
    }
    let cached_issue = conn.query_row(
        "
        SELECT EXISTS(
            SELECT 1 FROM github_issues
            WHERE owner = ?1 AND repo = ?2 AND number = ?3 AND repo_id = ?4
        )
        ",
        params![reference.owner, reference.repo, reference.number, repo_id],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(cached_issue)
}
pub(crate) fn mark_ref_sync(
    conn: &Connection,
    reference: &PapertrailRef,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    conn.execute(
        "
        INSERT INTO github_ref_sync(owner, repo, number, status, synced_at_ms, last_error, repo_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT(repo_id, owner, repo, number) DO UPDATE SET
            status = excluded.status,
            synced_at_ms = excluded.synced_at_ms,
            last_error = excluded.last_error
        ",
        params![
            reference.owner,
            reference.repo,
            reference.number,
            status,
            now_ms(),
            error,
            repo_id
        ],
    )?;
    Ok(())
}
pub(crate) fn is_not_found_error(message: &str) -> bool {
    message.contains("HTTP 404") || message.to_ascii_lowercase().contains("not found")
}
pub(crate) fn discover_commit_refs(
    conn: &Connection,
    default_repo: Option<&str>,
    out: &mut Vec<PapertrailRef>,
) -> anyhow::Result<()> {
    // `git_commits` is direct-scoped (V040), so discovery only mines the ACTIVE repo's commit
    // messages for issue refs — a consolidated DB must not attribute a sibling repo's `#N` refs to
    // this repo.
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let mut stmt =
        conn.prepare("SELECT hash, subject, body FROM git_commits WHERE repo_id = ?1")?;
    let rows = stmt.query_map([&repo_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (hash, subject, body) = row?;
        for text in [subject, body] {
            for parsed in parse_refs(&text, default_repo) {
                out.push(PapertrailRef {
                    owner: parsed.owner,
                    repo: parsed.repo,
                    number: parsed.number,
                    ref_kind: parsed.kind,
                    source_kind: "commit".to_string(),
                    source_path: None,
                    source_commit: Some(hash.clone()),
                    source_text: text.clone(),
                });
            }
        }
    }
    Ok(())
}
pub(crate) fn discover_file_refs(
    conn: &Connection,
    root: &Path,
    default_repo: Option<&str>,
    out: &mut Vec<PapertrailRef>,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let path = row?;
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        for line in text.lines() {
            for parsed in parse_refs(line, default_repo) {
                out.push(PapertrailRef {
                    owner: parsed.owner,
                    repo: parsed.repo,
                    number: parsed.number,
                    ref_kind: parsed.kind,
                    source_kind: "file".to_string(),
                    source_path: Some(path.clone()),
                    source_commit: None,
                    source_text: line.trim().to_string(),
                });
            }
        }
    }
    Ok(())
}
