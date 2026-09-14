//! The retired reference-driven sync lane: fetch exactly the items that indexed text references,
//! one at a time, through a caller-supplied client. It is CROSS-CRATE TEST SUPPORT ONLY — the
//! engine crate's schema tests drive it — and nothing on a production path may call into it:
//! production synchronization is the whole-project mirror ([`crate::sync_mirror`] /
//! [`crate::sync_mirror_scheduled`]), and discovered references are annotations only. It is not
//! `#[cfg(test)]`-gated because another crate's tests cannot see test-gated items (the same
//! posture as [`crate::transport::stub`]).

use rag_rat_db::meta::set_repo_meta;
use rag_rat_db::schema;

use super::*;

#[derive(Debug, Clone)]
pub struct PapertrailSyncProgress {
    pub current: usize,
    pub total: usize,
    pub project: String,
    pub item_key: String,
    pub action: PapertrailSyncAction,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PapertrailSyncAction {
    Syncing,
    Skipped,
    Synced,
    Failed,
}

#[derive(Default)]
pub struct SyncRefsReport {
    synced_items: usize,
    skipped_refs: usize,
    failed_refs: usize,
    errors: Vec<PapertrailSyncError>,
}

pub async fn sync_from_refs<C: PapertrailClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailSyncReport> {
    sync_from_refs_with_progress(conn, root, client, offline, ctx, |_| {}).await
}
pub async fn sync_from_refs_with_progress<C: PapertrailClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
    mut progress: impl FnMut(PapertrailSyncProgress),
) -> anyhow::Result<PapertrailSyncReport> {
    let refs = discover_and_store_refs(conn, root, ctx)?;
    // One-time (versioned): rows cached before store-time mining existed get their text mined.
    sync::backfill_mined_refs(conn, &ctx.trackers)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("papertrail sync requires a client"))?;
        // Production discovery persists every provider's refs, but live sync remains GitHub-only
        // until the provider-client PRs build on the shared transport. Never send another
        // provider's identity through GitHubClient.
        sync_refs(
            conn,
            client,
            &ctx.trackers,
            refs.iter()
                .filter(|reference| discovered_ref_uses_legacy_github_client(ctx, reference)),
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
        bindings: Vec::new(),
        errors: sync.errors,
        status: {
            // Referenced targets were just cached: re-derive the commit-tier closers (see the
            // mirror entry) before assembling the status snapshot.
            sync::rederive_commit_closers(conn, root, ctx)?;
            status(conn, ctx)?
        },
    })
}
pub async fn sync_issue<C: PapertrailClient>(
    conn: &Connection,
    root: &Path,
    issue_ref: &str,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailSyncReport> {
    // Versioned mined-evidence backfill: EVERY sync entry converges pre-mining caches — the
    // scheduled lane and the single-issue lane must not strand rows the manual lanes would heal.
    sync::backfill_mined_refs(conn, &ctx.trackers)?;
    let parsed = parse_issue_ref(issue_ref, ctx.default_repo())
        .ok_or_else(|| anyhow::anyhow!("invalid tracker item reference `{issue_ref}`"))?;
    let project = parsed.project.clone();
    let item_key = parsed.number.to_string();
    store_ref(conn, &parsed.into_ref(RefSourceKind::Manual, None, None, issue_ref.to_string()))?;
    let refs = refs(conn)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("papertrail sync requires a client"))?;
        sync_refs(
            conn,
            client,
            &ctx.trackers,
            // `parse_issue_ref` is the explicit legacy GitHub command grammar. Its routing must
            // not depend on which discovery bindings happen to be configured for this repo.
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
        bindings: Vec::new(),
        errors: sync.errors,
        status: {
            // The just-synced item may be the target an earlier commit ref's kind check needed.
            sync::rederive_commit_closers(conn, root, ctx)?;
            status(conn, ctx)?
        },
    })
}
/// Whether a discovered ref may be fetched through the legacy GitHub client: GitHub refs claimed by
/// a cloud (not Enterprise) binding, or every GitHub ref when nothing is configured.
fn discovered_ref_uses_legacy_github_client(
    ctx: &PapertrailContext,
    reference: &PapertrailRef,
) -> bool {
    if reference.tracker != Tracker::Github {
        return false;
    }
    if ctx.trackers.is_empty() {
        return true;
    }
    parse_tracker_refs_with_bindings(&reference.source_text, &ctx.trackers)
        .into_iter()
        .find(|(_, parsed)| {
            parsed.provider == reference.tracker
                && parsed.project == reference.project
                && parsed.item_key == reference.item_key
                // V060-migrated GitHub refs have NULL kind even when their source URL now parses
                // as `/pull/N`; NULL is unknown, not a conflicting kind.
                && reference
                    .item_kind
                    .is_none_or(|kind| parsed.item_kind == Some(kind))
        })
        .is_some_and(|(binding_index, _)| ctx.trackers[binding_index].base_url.is_none())
}

pub async fn sync_refs<'a, C: PapertrailClient>(
    conn: &Connection,
    client: &C,
    trackers: &[ResolvedTracker],
    refs: impl Iterator<Item = &'a PapertrailRef>,
    progress: &mut impl FnMut(PapertrailSyncProgress),
) -> anyhow::Result<SyncRefsReport> {
    let refs = refs.collect::<Vec<_>>();
    let identity = |reference: &PapertrailRef| {
        (
            reference.tracker.as_db_str(),
            reference.project.clone(),
            reference.item_kind.map(ItemKind::as_db_str),
            reference.item_key.clone(),
        )
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
        match sync_one_ref(conn, client, trackers, reference).await {
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
                let status = if is_not_found_error(&message) {
                    SyncErrorStatus::NotFound
                } else {
                    SyncErrorStatus::Failed
                };
                report.failed_refs += 1;
                report.errors.push(PapertrailSyncError {
                    tracker: reference.tracker,
                    project: reference.project.clone(),
                    item_key: reference.item_key.clone(),
                    status,
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
async fn sync_one_ref<C: PapertrailClient>(
    conn: &Connection,
    client: &C,
    trackers: &[ResolvedTracker],
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
    // The referenced-sync lane honors the same store-time mining contract as the mirror.
    mine_item_refs(&tx, reference.tracker, trackers, &item)?;
    let mut synced = 1;
    for comment in &comments {
        store_comment(&tx, reference.tracker, comment)?;
        mine_comment_refs(&tx, reference.tracker, trackers, comment)?;
        synced += 1;
    }
    tx.commit()?;
    Ok(synced)
}
fn sync_progress(
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
pub fn papertrail_ref_synced(conn: &Connection, reference: &PapertrailRef) -> anyhow::Result<bool> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
fn is_not_found_error(message: &str) -> bool {
    message.contains("HTTP 404") || message.to_ascii_lowercase().contains("not found")
}
