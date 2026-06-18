use super::*;

pub(crate) fn discover_and_store_refs(
    conn: &Connection,
    root: &Path,
    ctx: &GitHubContext,
) -> anyhow::Result<Vec<GitHubRef>> {
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
        refs.push(GitHubRef {
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
pub(crate) fn sync_refs<'a, C: GitHubClient>(
    conn: &Connection,
    client: &C,
    refs: impl Iterator<Item = &'a GitHubRef>,
    progress: &mut impl FnMut(GitHubSyncProgress),
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
        if github_ref_synced(conn, reference)? {
            report.skipped_refs += 1;
            progress(sync_progress(reference, current, total, GitHubSyncAction::Skipped, None));
            continue;
        }
        progress(sync_progress(reference, current, total, GitHubSyncAction::Syncing, None));
        match sync_one_ref(conn, client, reference) {
            Ok(items) => {
                report.synced_items += items;
                mark_ref_sync(conn, reference, "synced", None)?;
                progress(sync_progress(reference, current, total, GitHubSyncAction::Synced, None));
            },
            Err(err) => {
                let message = err.to_string();
                let status = if is_not_found_error(&message) { "not_found" } else { "failed" };
                mark_ref_sync(conn, reference, status, Some(&message))?;
                report.failed_refs += 1;
                report.errors.push(GitHubSyncError {
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
                    GitHubSyncAction::Failed,
                    Some(message),
                ));
            },
        }
    }
    progress(GitHubSyncProgress {
        current: total,
        total,
        owner: String::new(),
        repo: String::new(),
        number: 0,
        action: GitHubSyncAction::RebuildingFts,
        message: None,
    });
    rebuild_fts(conn)?;
    Ok(report)
}
pub(crate) fn sync_one_ref<C: GitHubClient>(
    conn: &Connection,
    client: &C,
    reference: &GitHubRef,
) -> anyhow::Result<usize> {
    let mut synced = 0;
    let issue = client.issue(&reference.owner, &reference.repo, reference.number)?;
    store_issue(conn, &issue)?;
    synced += 1;
    for comment in client.issue_comments(&reference.owner, &reference.repo, reference.number)? {
        store_comment(conn, &comment)?;
        synced += 1;
    }
    if let Some(pull) = client.pull(&reference.owner, &reference.repo, reference.number)? {
        store_pull(conn, &pull)?;
        synced += 1;
        for review in client.pull_reviews(&reference.owner, &reference.repo, reference.number)? {
            store_review(conn, &review)?;
            synced += 1;
        }
        for comment in
            client.pull_review_comments(&reference.owner, &reference.repo, reference.number)?
        {
            store_review_comment(conn, &comment)?;
            synced += 1;
        }
    }
    Ok(synced)
}
pub(crate) fn sync_progress(
    reference: &GitHubRef,
    current: usize,
    total: usize,
    action: GitHubSyncAction,
    message: Option<String>,
) -> GitHubSyncProgress {
    GitHubSyncProgress {
        current,
        total,
        owner: reference.owner.clone(),
        repo: reference.repo.clone(),
        number: reference.number,
        action,
        message,
    }
}
pub(crate) fn github_ref_synced(conn: &Connection, reference: &GitHubRef) -> anyhow::Result<bool> {
    let status = conn
        .query_row(
            "
            SELECT status
            FROM github_ref_sync
            WHERE owner = ?1 AND repo = ?2 AND number = ?3
            ",
            params![reference.owner, reference.repo, reference.number],
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
            WHERE owner = ?1 AND repo = ?2 AND number = ?3
        )
        ",
        params![reference.owner, reference.repo, reference.number],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(cached_issue)
}
pub(crate) fn mark_ref_sync(
    conn: &Connection,
    reference: &GitHubRef,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO github_ref_sync(owner, repo, number, status, synced_at_ms, last_error)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(owner, repo, number) DO UPDATE SET
            status = excluded.status,
            synced_at_ms = excluded.synced_at_ms,
            last_error = excluded.last_error
        ",
        params![reference.owner, reference.repo, reference.number, status, now_ms(), error],
    )?;
    Ok(())
}
pub(crate) fn is_not_found_error(message: &str) -> bool {
    message.contains("HTTP 404") || message.to_ascii_lowercase().contains("not found")
}
pub(crate) fn discover_commit_refs(
    conn: &Connection,
    default_repo: Option<&str>,
    out: &mut Vec<GitHubRef>,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("SELECT hash, subject, body FROM git_commits")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (hash, subject, body) = row?;
        for text in [subject, body] {
            for parsed in parse_refs(&text, default_repo) {
                out.push(GitHubRef {
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
    out: &mut Vec<GitHubRef>,
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
                out.push(GitHubRef {
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
