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
        refs.push(parsed.into_ref("branch", None, None, branch.clone()));
    }
    let mut unique = BTreeSet::new();
    refs.retain(|r| {
        unique.insert((
            r.tracker.as_db_str(),
            r.project.clone(),
            r.item_key.clone(),
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
    let identity = |reference: &PapertrailRef| {
        (reference.tracker.as_db_str(), reference.project.clone(), reference.item_key.clone())
    };
    let total = refs.iter().map(|reference| identity(reference)).collect::<BTreeSet<_>>().len();
    let mut report = SyncRefsReport::default();
    let mut seen = BTreeSet::new();
    for reference in refs {
        if !seen.insert(identity(reference)) {
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
                report.failed_refs += 1;
                report.errors.push(PapertrailSyncError {
                    tracker: reference.tracker,
                    project: reference.project.clone(),
                    item_key: reference.item_key.clone(),
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
    Ok(report)
}
/// Sync ONE referenced item: fetch the item AND its comments, then store both. Fetch-then-store
/// ordering is LOAD-BEARING: the per-ref sync state machine (`github_ref_sync`) is gone, so
/// [`papertrail_ref_synced`]'s only skip signal is "the item is cached" — a partial store (item
/// row landed, comment fetch failed) would masquerade as a completed sync forever. Storing nothing
/// until every fetch succeeded keeps a failed ref retryable with no state row. The FTS mirror
/// follows incrementally inside the store writers.
pub(crate) async fn sync_one_ref<C: PapertrailClient>(
    conn: &Connection,
    client: &C,
    reference: &PapertrailRef,
) -> anyhow::Result<usize> {
    // Discovered refs don't carry a kind (a bare `#N` could be either); ask as an issue and let
    // the provider resolve, then fetch comments under the RESOLVED kind.
    let item = client.item(&reference.project, ItemKind::Issue, &reference.item_key).await?;
    let comments =
        client.item_comments(&reference.project, item.item_kind, &reference.item_key).await?;
    // The cached item is the ref lane's completion marker. Commit it atomically with every
    // comment/FTS row so a failed comment write cannot leave an item that suppresses retries.
    let tx = conn.unchecked_transaction()?;
    store_item(&tx, reference.tracker, &item)?;
    let mut synced = 1;
    for comment in &comments {
        store_comment(&tx, reference.tracker, comment)?;
        synced += 1;
    }
    tx.commit()?;
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
        project: reference.project.clone(),
        item_key: reference.item_key.clone(),
        action,
        message,
    }
}
/// Whether a discovered ref's item is already cached — the ONLY skip signal now that the per-ref
/// synced/not_found/failed state machine is deleted (`papertrail_sync_cursor` replaces it for the
/// mirror sync; the ref lane keeps no per-ref state). No `item_kind` filter: a bare `#N` ref could
/// name either kind, and either cached kind means the item was synced. A not-found item retries on
/// every sync (no memo) — acceptable for the referenced-only lane the mirror sync supersedes.
pub(crate) fn papertrail_ref_synced(
    conn: &Connection,
    reference: &PapertrailRef,
) -> anyhow::Result<bool> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let cached = conn.query_row(
        "
        SELECT EXISTS(
            SELECT 1 FROM papertrail_items
            WHERE tracker = ?1 AND project = ?2 AND item_key = ?3 AND repo_id = ?4
        )
        ",
        params![reference.tracker.as_db_str(), reference.project, reference.item_key, repo_id],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(cached)
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
                out.push(parsed.into_ref("commit", None, Some(hash.clone()), text.clone()));
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
                out.push(parsed.into_ref(
                    "file",
                    Some(path.clone()),
                    None,
                    line.trim().to_string(),
                ));
            }
        }
    }
    Ok(())
}
