//! Provider-neutral whole-project mirror runner. This module owns cursor arbitration,
//! fetch-then-commit page semantics, tag pruning, pause/resume, and full healing.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, params};

use super::transport::{PauseReason, TransportError};
use super::*;

// GitHub Search accepts years only through 2970. This remains safely beyond any real item while
// keeping the initial strict `updated:<boundary` query valid.
const INITIAL_BACKFILL_BOUNDARY: &str = "2970-12-31T23:59:59Z";
/// An empty initial walk has consumed no provider item. Persist the lowest practical timestamp so
/// later runs enter the normal probe/delta path and discover items created after that walk.
const EMPTY_PROJECT_HIGH_MARK: &str = "1970-01-01T00:00:00Z";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct ProcessedItem {
    kind: String,
    key: String,
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CommentStreamCursor {
    high_mark_at: Option<String>,
    page_token: Option<String>,
    scan_since: Option<String>,
    #[serde(default)]
    scan_high_mark_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ItemThreadCursor {
    item: ProcessedItem,
    lane: PageLane,
    stream_index: usize,
    page_cursor: Option<PageCursor>,
    #[serde(default)]
    seen_comment_ids: BTreeSet<String>,
    #[serde(default)]
    previous_comment_ids: Option<BTreeSet<String>>,
    #[serde(default)]
    saw_pagination: bool,
}

#[derive(Debug, Clone, Default)]
struct MirrorCursor {
    high_mark_at: Option<String>,
    comment_high_mark_at: Option<String>,
    comment_page_token: Option<String>,
    comment_scan_since: Option<String>,
    comment_stream_cursors: BTreeMap<String, CommentStreamCursor>,
    low_mark_at: Option<String>,
    probe_etag: Option<String>,
    backfill_done: bool,
    filter_fingerprint: String,
    item_delta_page_token: Option<String>,
    item_delta_scan_since: Option<String>,
    item_delta_high_mark_at: Option<String>,
    item_delta_in_progress: bool,
    item_delta_replay_required: bool,
    backfill_page_cursor: Option<PageCursor>,
    item_thread_cursor: Option<ItemThreadCursor>,
    delta_processed_keys: BTreeSet<ProcessedItem>,
    backfill_processed_keys: BTreeSet<ProcessedItem>,
    full_rewalk: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum MirrorContinuation {
    #[default]
    None,
    Incremental,
    Full,
}

impl MirrorCursor {
    fn continuation(&self) -> MirrorContinuation {
        if self.full_rewalk {
            return MirrorContinuation::Full;
        }
        if self.item_delta_in_progress
            || self.item_delta_replay_required
            || self.item_delta_page_token.is_some()
            || self.backfill_page_cursor.is_some()
            || self.item_thread_cursor.is_some()
            || (!self.backfill_done
                && (self.low_mark_at.is_some()
                    || self.high_mark_at.is_some()
                    || !self.backfill_processed_keys.is_empty()))
            || self.comment_page_token.is_some()
            || self.comment_stream_cursors.values().any(|stream| stream.page_token.is_some())
        {
            return MirrorContinuation::Incremental;
        }
        MirrorContinuation::None
    }
}

pub(crate) fn load_mirror_continuation(
    conn: &Connection,
    binding: &ResolvedTracker,
) -> anyhow::Result<MirrorContinuation> {
    Ok(load_cursor(conn, binding)?.continuation())
}

#[derive(Debug, Clone, Serialize)]
pub struct MirrorBindingReport {
    pub tracker: Tracker,
    pub project: String,
    pub stored_items: usize,
    pub stored_comments: usize,
    pub pruned_items: usize,
    pub paused_until_ms: Option<i64>,
    pub pause_reason: Option<String>,
    pub completed_full_walk: bool,
}

pub(crate) async fn mirror_binding<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    client: &C,
    full: bool,
) -> anyhow::Result<MirrorBindingReport> {
    let mut cursor = load_cursor(conn, binding)?;
    let resumed_continuation = cursor.continuation();
    let had_completed_backfill = cursor.backfill_done;
    let fingerprint = binding.filter_fingerprint();
    let filter_changed = cursor.filter_fingerprint != fingerprint;
    let starting_full_rewalk = full && !cursor.full_rewalk;
    if starting_full_rewalk {
        reset_for_full_rewalk(conn, binding, &mut cursor)?;
    }
    if filter_changed {
        cursor.low_mark_at = None;
        cursor.backfill_done = false;
        cursor.filter_fingerprint = fingerprint;
        cursor.comment_page_token = None;
        cursor.comment_scan_since = None;
        cursor.comment_stream_cursors.clear();
        cursor.item_delta_page_token = None;
        cursor.item_delta_scan_since = None;
        cursor.item_delta_high_mark_at = None;
        cursor.item_delta_in_progress = false;
        cursor.item_delta_replay_required = false;
        cursor.backfill_page_cursor = None;
        cursor.item_thread_cursor = None;
        cursor.delta_processed_keys.clear();
        cursor.backfill_processed_keys.clear();
        if cursor.full_rewalk {
            reset_full_seen(conn, binding)?;
        }
    }
    // Opaque item-comment page tokens are not snapshots. IDs seen before a process-level pause
    // cannot prove that those comments still exist when the walk resumes, so restart the thread's
    // mark phase from its first stream. Stored rows remain intact until the restarted walk
    // finishes.
    if let Some(thread) = cursor.item_thread_cursor.as_mut()
        && (thread.stream_index != 0 || thread.page_cursor.is_some())
    {
        thread.stream_index = 0;
        thread.page_cursor = None;
        thread.seen_comment_ids.clear();
    }
    let mut report = MirrorBindingReport {
        tracker: binding.provider,
        project: binding.project.clone(),
        stored_items: 0,
        stored_comments: 0,
        pruned_items: 0,
        paused_until_ms: None,
        pause_reason: None,
        completed_full_walk: false,
    };
    if filter_changed {
        report.pruned_items += prune_unmatched(conn, binding)?;
        save_cursor(conn, binding, &cursor, false)?;
    }

    let result = mirror_binding_inner(conn, binding, client, &mut cursor, &mut report).await;
    match result {
        Ok(()) => {
            report.completed_full_walk = cursor.backfill_done
                && (!had_completed_backfill
                    || starting_full_rewalk
                    || resumed_continuation == MirrorContinuation::Full
                    || filter_changed);
            Ok(report)
        },
        Err(error) if pause(&error).is_some() => {
            let (resume_at_ms, reason) = pause(&error).expect("checked");
            report.paused_until_ms = Some(resume_at_ms);
            report.pause_reason = Some(reason.as_str().to_string());
            Ok(report)
        },
        Err(error) => Err(error),
    }
}

async fn mirror_binding_inner<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    client: &C,
    cursor: &mut MirrorCursor,
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    let previous_high = cursor.high_mark_at.clone();
    if !cursor.full_rewalk
        && let Some(high) = previous_high.as_deref()
    {
        if cursor.item_delta_in_progress {
            cursor.item_delta_scan_since.get_or_insert_with(|| overlap_timestamp(high));
            save_cursor(conn, binding, cursor, false)?;
            sync_item_delta(conn, binding, client, cursor, report).await?;
        } else {
            let probe = client
                .freshness_probe(&binding.project, &FreshnessProbe {
                    updated_since: Some(high.to_string()),
                    etag: cursor.probe_etag.clone(),
                })
                .await?;
            cursor.probe_etag = probe.etag;
            if !probe.not_modified {
                cursor.item_delta_in_progress = true;
                cursor.item_delta_scan_since = Some(overlap_timestamp(high));
                cursor.item_delta_high_mark_at = probe.latest;
                save_cursor(conn, binding, cursor, false)?;
                sync_item_delta(conn, binding, client, cursor, report).await?;
            } else {
                save_cursor(conn, binding, cursor, false)?;
            }
        }
        sync_comment_delta(conn, binding, client, cursor, report).await?;
    }

    while !cursor.backfill_done {
        let boundary =
            cursor.low_mark_at.clone().unwrap_or_else(|| INITIAL_BACKFILL_BOUNDARY.to_string());
        let request = cursor.backfill_page_cursor.clone().unwrap_or_else(|| PageCursor {
            updated_before: Some(boundary.clone()),
            ..PageCursor::default()
        });
        let page = client.items_page(&binding.project, &request).await?;
        if page.items.is_empty() && page.next.is_none() && page.backfill_boundary.is_none() {
            cursor.backfill_done = true;
            cursor.high_mark_at.get_or_insert_with(|| EMPTY_PROJECT_HIGH_MARK.to_string());
            cursor.backfill_processed_keys.clear();
            save_cursor(conn, binding, cursor, false)?;
            break;
        }
        if let Some(next) = &page.next {
            ensure_cursor_advanced(&request, next, "backfill")?;
        }
        store_item_page_resumably(
            conn,
            binding,
            client,
            &page.items,
            PageLane::Backfill,
            cursor,
            report,
        )
        .await?;
        if cursor.high_mark_at.is_none() {
            cursor.high_mark_at = max_item_updated_at(&page.items);
        }
        if let Some(next) = page.next {
            cursor.backfill_page_cursor = Some(next);
            save_cursor(conn, binding, cursor, false)?;
            continue;
        }
        let next_low = page
            .backfill_boundary
            .or_else(|| min_item_updated_at(&page.items))
            .or_else(|| request.page_token.as_ref().and_then(|_| request.updated_before.clone()))
            .ok_or_else(|| anyhow::anyhow!("backfill page has no updated_at boundary"))?;
        anyhow::ensure!(next_low < boundary, "backfill boundary did not advance below {boundary}");
        cursor.low_mark_at = Some(next_low);
        cursor.backfill_page_cursor = None;
        cursor.backfill_processed_keys.clear();
        save_cursor(conn, binding, cursor, false)?;
    }
    if previous_high.is_none() || cursor.full_rewalk {
        sync_comment_delta(conn, binding, client, cursor, report).await?;
    }
    if cursor.full_rewalk {
        let tx = conn.unchecked_transaction()?;
        report.pruned_items += prune_missing(&tx, binding)?;
        rebuild_fts(&tx)?;
        cursor.full_rewalk = false;
        save_cursor(&tx, binding, cursor, true)?;
        tx.commit()?;
    }
    Ok(())
}

async fn sync_item_delta<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    client: &C,
    cursor: &mut MirrorCursor,
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    let mut request = PageCursor {
        updated_since: cursor.item_delta_scan_since.clone(),
        page_token: cursor.item_delta_page_token.clone(),
        ..PageCursor::default()
    };
    loop {
        let page = client.items_page(&binding.project, &request).await?;
        let first_page_high = request
            .page_token
            .is_none()
            .then(|| page.items.iter().filter_map(|item| item.updated_at.clone()).max());
        let next = if let Some(mut next) = page.next {
            next.updated_since = cursor.item_delta_scan_since.clone();
            next.updated_before = None;
            ensure_cursor_advanced(&request, &next, "item delta")?;
            Some(next)
        } else {
            None
        };
        if let Some(first_page_high) = first_page_high {
            // GitHub's updated-order REST pages are mutable. Once pagination is present, only
            // the first page is a proven consumed prefix: an edit can move a row from that page
            // later and shift an unseen boundary row behind the current offset. Persisting the
            // first page's inclusive upper timestamp makes the next scan replay that boundary.
            // A single physical page consumed the whole observed window, but still must not
            // jump to a probe timestamp that the list response did not contain.
            let reached_probe =
                if cursor.item_delta_replay_required && cursor.item_delta_high_mark_at.is_none() {
                    first_page_high == cursor.high_mark_at
                } else {
                    first_page_high == cursor.item_delta_high_mark_at && next.is_none()
                };
            cursor.item_delta_high_mark_at = first_page_high;
            cursor.item_delta_replay_required = !reached_probe;
            if cursor.item_delta_replay_required {
                // The next run must probe unconditionally so this conservative frontier is
                // replayed even when the prior probe's ETag is otherwise still current.
                cursor.probe_etag = None;
            }
        }
        store_item_page_resumably(
            conn,
            binding,
            client,
            &page.items,
            PageLane::Delta,
            cursor,
            report,
        )
        .await?;
        if let Some(next) = next {
            cursor.delta_processed_keys.clear();
            cursor.item_delta_page_token = next.page_token.clone();
            cursor.item_delta_in_progress = true;
            save_cursor(conn, binding, cursor, false)?;
            request = next;
        } else {
            cursor.high_mark_at =
                max_timestamp(cursor.high_mark_at.take(), cursor.item_delta_high_mark_at.take());
            cursor.delta_processed_keys.clear();
            cursor.item_delta_page_token = None;
            cursor.item_delta_scan_since = None;
            cursor.item_delta_in_progress = false;
            save_cursor(conn, binding, cursor, false)?;
            break;
        }
    }
    Ok(())
}

async fn sync_comment_delta<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    client: &C,
    cursor: &mut MirrorCursor,
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    // A legacy shared watermark may seed every stream exactly once. Never seed a stream from the
    // aggregate while this loop is advancing siblings: that recreates the cross-stream race this
    // map exists to prevent. A provider stream added later starts from scratch, which is safe.
    let legacy_high = cursor
        .comment_stream_cursors
        .is_empty()
        .then(|| cursor.comment_high_mark_at.clone())
        .flatten();
    for stream in client.comment_streams() {
        let state =
            cursor.comment_stream_cursors.entry((*stream).to_string()).or_insert_with(|| {
                CommentStreamCursor {
                    high_mark_at: legacy_high.clone(),
                    page_token: None,
                    scan_since: None,
                    scan_high_mark_at: None,
                }
            });
        let scan_since = state.scan_since.clone().unwrap_or_else(|| {
            state.high_mark_at.as_deref().map(overlap_timestamp).unwrap_or_default()
        });
        let mut request = PageCursor {
            stream: Some((*stream).to_string()),
            updated_since: (!scan_since.is_empty()).then_some(scan_since.clone()),
            page_token: state.page_token.clone(),
            ..PageCursor::default()
        };
        loop {
            let page = client.comments_page(&binding.project, &request).await?;
            let first_page_high = request.page_token.is_none().then(|| {
                page.comments.iter().filter_map(|comment| comment.updated_at.clone()).max()
            });
            let next = if let Some(mut next) = page.next {
                anyhow::ensure!(
                    next.stream.as_deref().is_none_or(|next_stream| next_stream == *stream),
                    "comment pagination crossed from `{stream}` into another stream"
                );
                next.stream = Some((*stream).to_string());
                next.updated_since = (!scan_since.is_empty()).then_some(scan_since.clone());
                ensure_cursor_advanced(&request, &next, "repository comment")?;
                Some(next)
            } else {
                None
            };
            store_repo_comments(conn, binding, &page.comments, report)?;
            let state = cursor.comment_stream_cursors.get_mut(*stream).expect("stream inserted");
            if let Some(first_page_high) = first_page_high {
                // As with item deltas, a mutable continuation proves no more than the first
                // ascending page. Replaying its inclusive upper boundary prevents offset shifts
                // from stranding an unseen or stale comment below the durable watermark.
                state.scan_high_mark_at = first_page_high;
            }
            state.page_token = next.as_ref().and_then(|next| next.page_token.clone());
            state.scan_since = next.as_ref().map(|_| scan_since.clone());
            if next.is_none() {
                state.high_mark_at =
                    max_timestamp(state.high_mark_at.take(), state.scan_high_mark_at.take());
            }
            cursor.comment_high_mark_at = common_comment_high_mark(&cursor.comment_stream_cursors);
            save_cursor(conn, binding, cursor, false)?;
            let Some(next) = next else { break };
            request = next;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PageLane {
    Delta,
    Backfill,
}

async fn store_item_page_resumably<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    client: &C,
    items: &[PapertrailItem],
    lane: PageLane,
    cursor: &mut MirrorCursor,
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    if let Some(active) = cursor.item_thread_cursor.clone() {
        if let Some(item) = items.iter().find(|item| {
            item.item_kind.as_db_str() == active.item.kind && item.item_key == active.item.key
        }) {
            let current = processed_item(item);
            if current != active.item {
                let mut item = item.clone();
                client.enrich_item(&mut item).await?;
                if binding.tracks(item.tags.iter().map(String::as_str)) {
                    begin_item_thread(conn, binding, &item, lane, cursor, report)?;
                } else {
                    report.pruned_items +=
                        usize::from(delete_item(conn, binding, item.item_kind, &item.item_key)?);
                    cursor.item_thread_cursor = None;
                    mark_processed(cursor, lane, current);
                    save_cursor(conn, binding, cursor, false)?;
                }
            }
        }
        if cursor.item_thread_cursor.is_some() {
            resume_item_thread(conn, binding, client, cursor, report).await?;
        }
    }
    for item in items {
        let key = processed_item(item);
        let already_stored = match lane {
            PageLane::Delta => cursor.delta_processed_keys.contains(&key),
            PageLane::Backfill => cursor.backfill_processed_keys.contains(&key),
        };
        if already_stored {
            continue;
        }
        let mut item = item.clone();
        client.enrich_item(&mut item).await?;
        if binding.tracks(item.tags.iter().map(String::as_str)) {
            begin_item_thread(conn, binding, &item, lane, cursor, report)?;
            resume_item_thread(conn, binding, client, cursor, report).await?;
        } else {
            report.pruned_items +=
                usize::from(delete_item(conn, binding, item.item_kind, &item.item_key)?);
            mark_processed(cursor, lane, key);
            save_cursor(conn, binding, cursor, false)?;
        }
    }
    Ok(())
}

fn processed_item(item: &PapertrailItem) -> ProcessedItem {
    ProcessedItem {
        kind: item.item_kind.as_db_str().to_string(),
        key: item.item_key.clone(),
        updated_at: item.updated_at.clone(),
    }
}

fn begin_item_thread(
    conn: &Connection,
    binding: &ResolvedTracker,
    item: &PapertrailItem,
    lane: PageLane,
    cursor: &mut MirrorCursor,
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    store_item(&tx, binding.provider, item)?;
    replace_tags(&tx, binding, item)?;
    if cursor.full_rewalk {
        mark_full_seen(&tx, binding, item)?;
    }
    cursor.item_thread_cursor = Some(ItemThreadCursor {
        item: processed_item(item),
        lane,
        stream_index: 0,
        page_cursor: None,
        seen_comment_ids: BTreeSet::new(),
        previous_comment_ids: None,
        saw_pagination: false,
    });
    save_cursor(&tx, binding, cursor, false)?;
    tx.commit()?;
    report.stored_items += 1;
    Ok(())
}

async fn resume_item_thread<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    client: &C,
    cursor: &mut MirrorCursor,
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    loop {
        let thread = cursor.item_thread_cursor.clone().expect("active item thread");
        let kind = ItemKind::from_db_str(&thread.item.kind)?;
        let streams = client.item_comment_streams(kind);
        let Some(stream) = streams.get(thread.stream_index) else {
            if thread.saw_pagination
                && thread.previous_comment_ids.as_ref() != Some(&thread.seen_comment_ids)
            {
                // GitHub item-comment continuations are mutable page numbers. Require two
                // identical complete walks before treating absence as deletion; a row shifted
                // behind one walk is rediscovered by the next instead of being pruned locally.
                let active = cursor.item_thread_cursor.as_mut().expect("active item thread");
                active.previous_comment_ids = Some(thread.seen_comment_ids);
                active.seen_comment_ids.clear();
                active.stream_index = 0;
                active.page_cursor = None;
                save_cursor(conn, binding, cursor, false)?;
                continue;
            }
            let tx = conn.unchecked_transaction()?;
            prune_unseen_item_comments(
                &tx,
                binding,
                kind,
                &thread.item.key,
                &thread.seen_comment_ids,
            )?;
            cursor.item_thread_cursor = None;
            mark_processed(cursor, thread.lane, thread.item);
            save_cursor(&tx, binding, cursor, false)?;
            tx.commit()?;
            return Ok(());
        };
        let request = thread.page_cursor.clone().unwrap_or_else(|| PageCursor {
            stream: Some((*stream).to_string()),
            ..PageCursor::default()
        });
        let page = match client
            .item_comments_page(&binding.project, kind, &thread.item.key, &request)
            .await
        {
            Ok(page) => page,
            Err(error) if is_item_not_found(&error) => {
                // GitHub deliberately uses 404 both for absent and inaccessible resources, and
                // a stale comment continuation says nothing about the parent. Unpin ordinary
                // sync without erasing the last complete cache; a successful full rewalk owns
                // authoritative item pruning.
                let tx = conn.unchecked_transaction()?;
                cursor.item_thread_cursor = None;
                mark_processed(cursor, thread.lane, thread.item);
                save_cursor(&tx, binding, cursor, false)?;
                tx.commit()?;
                return Ok(());
            },
            Err(error) => return Err(error),
        };
        let next = page.next;
        if let Some(next) = &next {
            anyhow::ensure!(
                next.stream.as_deref().is_none_or(|next_stream| next_stream == *stream),
                "item-comment pagination crossed from `{stream}` into another stream"
            );
            ensure_cursor_advanced(&request, next, "item comment")?;
        }
        let active = cursor.item_thread_cursor.as_mut().expect("active item thread");
        active
            .seen_comment_ids
            .extend(page.comments.iter().map(|comment| comment.comment_id.clone()));
        if let Some(mut next) = next {
            next.stream = Some((*stream).to_string());
            active.page_cursor = Some(next);
            active.saw_pagination = true;
        } else {
            active.stream_index += 1;
            active.page_cursor = None;
        }
        let tx = conn.unchecked_transaction()?;
        for comment in &page.comments {
            store_comment(&tx, binding.provider, comment)?;
        }
        save_cursor(&tx, binding, cursor, false)?;
        tx.commit()?;
        report.stored_comments += page.comments.len();
    }
}

fn ensure_cursor_advanced(
    current: &PageCursor,
    next: &PageCursor,
    lane: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(next != current, "{lane} pagination cursor did not advance");
    Ok(())
}

fn is_item_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<PapertrailClientError>(),
            Some(PapertrailClientError::ItemNotFound)
        )
    })
}

fn mark_processed(cursor: &mut MirrorCursor, lane: PageLane, key: ProcessedItem) {
    let stored = match lane {
        PageLane::Delta => &mut cursor.delta_processed_keys,
        PageLane::Backfill => &mut cursor.backfill_processed_keys,
    };
    stored.retain(|item| item.kind != key.kind || item.key != key.key);
    stored.insert(key);
}

fn load_cursor(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<MirrorCursor> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    Ok(conn
        .query_row(
            "SELECT high_mark_at, comment_high_mark_at, comment_page_token, comment_scan_since,
                    comment_stream_cursors, low_mark_at, probe_etag, backfill_done, \
             filter_fingerprint,
                    item_delta_page_token, item_delta_scan_since, item_delta_high_mark_at,
                    item_delta_in_progress, item_delta_replay_required, backfill_page_cursor,
                    item_thread_cursor, delta_processed_keys, backfill_processed_keys, full_rewalk
             FROM papertrail_sync_cursor
             WHERE tracker = ?1 AND project = ?2 AND repo_id = ?3",
            params![binding.provider.as_db_str(), binding.project, repo_id],
            |row| {
                Ok(MirrorCursor {
                    high_mark_at: row.get(0)?,
                    comment_high_mark_at: row.get(1)?,
                    comment_page_token: row.get(2)?,
                    comment_scan_since: row.get(3)?,
                    comment_stream_cursors: decode_json(row.get(4)?, 4)?,
                    low_mark_at: row.get(5)?,
                    probe_etag: row.get(6)?,
                    backfill_done: row.get(7)?,
                    filter_fingerprint: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
                    item_delta_page_token: row.get(9)?,
                    item_delta_scan_since: row.get(10)?,
                    item_delta_high_mark_at: row.get(11)?,
                    item_delta_in_progress: row.get(12)?,
                    item_delta_replay_required: row.get(13)?,
                    backfill_page_cursor: decode_json(row.get(14)?, 14)?,
                    item_thread_cursor: decode_json(row.get(15)?, 15)?,
                    delta_processed_keys: decode_processed_items(row.get(16)?, 16)?,
                    backfill_processed_keys: decode_processed_items(row.get(17)?, 17)?,
                    full_rewalk: row.get(18)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

fn save_cursor(
    conn: &Connection,
    binding: &ResolvedTracker,
    cursor: &MirrorCursor,
    full: bool,
) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let delta_processed_keys = serde_json::to_string(&cursor.delta_processed_keys)?;
    let backfill_processed_keys = serde_json::to_string(&cursor.backfill_processed_keys)?;
    let comment_stream_cursors = serde_json::to_string(&cursor.comment_stream_cursors)?;
    let backfill_page_cursor =
        cursor.backfill_page_cursor.as_ref().map(serde_json::to_string).transpose()?;
    let item_thread_cursor =
        cursor.item_thread_cursor.as_ref().map(serde_json::to_string).transpose()?;
    conn.execute(
        "INSERT INTO papertrail_sync_cursor(
             tracker, project, high_mark_at, comment_high_mark_at, comment_page_token,
             comment_scan_since, comment_stream_cursors, low_mark_at, probe_etag, backfill_done, \
         filter_fingerprint,
             item_delta_page_token, item_delta_scan_since, item_delta_high_mark_at,
             item_delta_in_progress, item_delta_replay_required, backfill_page_cursor,
             item_thread_cursor, delta_processed_keys, backfill_processed_keys, full_rewalk,
             last_probe_ms, last_full_sync_ms, repo_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                   ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, CASE WHEN ?23 THEN ?22 END, ?24)
         ON CONFLICT(repo_id, tracker, project) DO UPDATE SET
             high_mark_at = excluded.high_mark_at,
             comment_high_mark_at = excluded.comment_high_mark_at,
             comment_page_token = excluded.comment_page_token,
             comment_scan_since = excluded.comment_scan_since,
             comment_stream_cursors = excluded.comment_stream_cursors,
             low_mark_at = excluded.low_mark_at,
             probe_etag = excluded.probe_etag,
             backfill_done = excluded.backfill_done,
             filter_fingerprint = excluded.filter_fingerprint,
             item_delta_page_token = excluded.item_delta_page_token,
             item_delta_scan_since = excluded.item_delta_scan_since,
             item_delta_high_mark_at = excluded.item_delta_high_mark_at,
             item_delta_in_progress = excluded.item_delta_in_progress,
             item_delta_replay_required = excluded.item_delta_replay_required,
             backfill_page_cursor = excluded.backfill_page_cursor,
             item_thread_cursor = excluded.item_thread_cursor,
             delta_processed_keys = excluded.delta_processed_keys,
             backfill_processed_keys = excluded.backfill_processed_keys,
             full_rewalk = excluded.full_rewalk,
             last_probe_ms = excluded.last_probe_ms,
             last_full_sync_ms = CASE WHEN ?23 THEN excluded.last_full_sync_ms
                                      ELSE papertrail_sync_cursor.last_full_sync_ms END",
        params![
            binding.provider.as_db_str(),
            binding.project,
            cursor.high_mark_at,
            cursor.comment_high_mark_at,
            cursor.comment_page_token,
            cursor.comment_scan_since,
            comment_stream_cursors,
            cursor.low_mark_at,
            cursor.probe_etag,
            cursor.backfill_done,
            cursor.filter_fingerprint,
            cursor.item_delta_page_token,
            cursor.item_delta_scan_since,
            cursor.item_delta_high_mark_at,
            cursor.item_delta_in_progress,
            cursor.item_delta_replay_required,
            backfill_page_cursor,
            item_thread_cursor,
            delta_processed_keys,
            backfill_processed_keys,
            cursor.full_rewalk,
            now_ms(),
            full,
            repo_id,
        ],
    )?;
    Ok(())
}

fn decode_json<T: serde::de::DeserializeOwned + Default>(
    value: Option<String>,
    column: usize,
) -> rusqlite::Result<T> {
    value.map_or_else(
        || Ok(T::default()),
        |value| {
            serde_json::from_str(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
            })
        },
    )
}

fn decode_processed_items(
    value: Option<String>,
    column: usize,
) -> rusqlite::Result<BTreeSet<ProcessedItem>> {
    let Some(value) = value else { return Ok(BTreeSet::new()) };
    if let Ok(items) = serde_json::from_str(&value) {
        return Ok(items);
    }
    serde_json::from_str::<BTreeSet<(String, String)>>(&value)
        .map(|legacy| {
            legacy
                .into_iter()
                .map(|(kind, key)| ProcessedItem { kind, key, updated_at: None })
                .collect()
        })
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
        })
}

fn reset_for_full_rewalk(
    conn: &Connection,
    binding: &ResolvedTracker,
    cursor: &mut MirrorCursor,
) -> anyhow::Result<()> {
    cursor.high_mark_at = None;
    cursor.comment_high_mark_at = None;
    cursor.comment_page_token = None;
    cursor.comment_scan_since = None;
    cursor.comment_stream_cursors.clear();
    cursor.low_mark_at = None;
    cursor.backfill_done = false;
    cursor.item_delta_page_token = None;
    cursor.item_delta_in_progress = false;
    cursor.item_delta_replay_required = false;
    cursor.backfill_page_cursor = None;
    cursor.item_thread_cursor = None;
    cursor.delta_processed_keys.clear();
    cursor.backfill_processed_keys.clear();
    cursor.full_rewalk = true;
    reset_full_seen(conn, binding)?;
    save_cursor(conn, binding, cursor, false)
}

fn reset_full_seen(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    conn.execute(
        "UPDATE papertrail_items SET full_rewalk_seen=0
         WHERE repo_id=?1 AND tracker=?2 AND project=?3",
        params![repo_id, binding.provider.as_db_str(), binding.project],
    )?;
    Ok(())
}

fn mark_full_seen(
    conn: &Connection,
    binding: &ResolvedTracker,
    item: &PapertrailItem,
) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    conn.execute(
        "UPDATE papertrail_items SET full_rewalk_seen=1
         WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND item_kind=?4 AND item_key=?5",
        params![
            repo_id,
            binding.provider.as_db_str(),
            binding.project,
            item.item_kind.as_db_str(),
            item.item_key,
        ],
    )?;
    Ok(())
}

fn replace_tags(
    conn: &Connection,
    binding: &ResolvedTracker,
    item: &PapertrailItem,
) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    conn.execute(
        "DELETE FROM papertrail_item_tags WHERE tracker=?1 AND project=?2 AND item_kind=?3 AND \
         item_key=?4 AND repo_id=?5",
        params![
            binding.provider.as_db_str(),
            binding.project,
            item.item_kind.as_db_str(),
            item.item_key,
            repo_id
        ],
    )?;
    for tag in normalized_tags(&item.tags) {
        conn.execute(
            "INSERT INTO papertrail_item_tags(tracker, project, item_kind, item_key, tag, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                binding.provider.as_db_str(),
                binding.project,
                item.item_kind.as_db_str(),
                item.item_key,
                tag,
                repo_id
            ],
        )?;
    }
    Ok(())
}

fn prune_unmatched(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<usize> {
    if binding.tags.is_empty() {
        return Ok(0);
    }
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let wanted = normalized_tags(&binding.tags);
    let mut stmt = conn.prepare(
        "SELECT i.item_kind, i.item_key, t.tag
         FROM papertrail_items i
         LEFT JOIN papertrail_item_tags t ON t.repo_id=i.repo_id AND t.tracker=i.tracker
              AND t.project=i.project AND t.item_kind=i.item_kind AND t.item_key=i.item_key
         WHERE i.tracker=?1 AND i.project=?2 AND i.repo_id=?3
         ORDER BY i.item_kind, i.item_key",
    )?;
    let rows =
        stmt.query_map(params![binding.provider.as_db_str(), binding.project, repo_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
    let mut grouped = std::collections::BTreeMap::<(String, String), Vec<String>>::new();
    for row in rows {
        let (kind, key, tag) = row?;
        if let Some(tag) = tag {
            grouped.entry((kind, key)).or_default().push(tag);
        } else {
            grouped.entry((kind, key)).or_default();
        }
    }
    drop(stmt);
    let mut pruned = 0;
    for ((kind, key), tags) in grouped {
        if !tags.iter().any(|tag| wanted.binary_search(tag).is_ok()) {
            pruned += usize::from(delete_item(conn, binding, ItemKind::from_db_str(&kind)?, &key)?);
        }
    }
    Ok(pruned)
}

fn delete_item(
    conn: &Connection,
    binding: &ResolvedTracker,
    kind: ItemKind,
    key: &str,
) -> anyhow::Result<bool> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let args =
        params![repo_id, binding.provider.as_db_str(), binding.project, kind.as_db_str(), key];
    conn.execute(
        "DELETE FROM papertrail_fts WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND \
         item_kind=?4 AND item_key=?5",
        args,
    )?;
    conn.execute(
        "DELETE FROM papertrail_comments WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND \
         item_kind=?4 AND item_key=?5",
        params![repo_id, binding.provider.as_db_str(), binding.project, kind.as_db_str(), key],
    )?;
    conn.execute(
        "DELETE FROM papertrail_item_tags WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND \
         item_kind=?4 AND item_key=?5",
        params![repo_id, binding.provider.as_db_str(), binding.project, kind.as_db_str(), key],
    )?;
    Ok(conn.execute(
        "DELETE FROM papertrail_items WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND \
         item_kind=?4 AND item_key=?5",
        params![repo_id, binding.provider.as_db_str(), binding.project, kind.as_db_str(), key],
    )? > 0)
}

fn prune_unseen_item_comments(
    conn: &Connection,
    binding: &ResolvedTracker,
    kind: ItemKind,
    key: &str,
    seen: &BTreeSet<String>,
) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let stale = {
        let mut stmt = conn.prepare(
            "SELECT comment_id FROM papertrail_comments
             WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND item_kind=?4 AND item_key=?5",
        )?;
        stmt.query_map(
            params![repo_id, binding.provider.as_db_str(), binding.project, kind.as_db_str(), key],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|comment_id| !seen.contains(comment_id))
        .collect::<Vec<_>>()
    };
    for comment_id in stale {
        conn.execute(
            "DELETE FROM papertrail_fts WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND
             item_kind=?4 AND item_key=?5 AND comment_id=?6 AND doc_kind='comment'",
            params![
                repo_id,
                binding.provider.as_db_str(),
                binding.project,
                kind.as_db_str(),
                key,
                comment_id,
            ],
        )?;
        conn.execute(
            "DELETE FROM papertrail_comments WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND
             item_kind=?4 AND item_key=?5 AND comment_id=?6",
            params![
                repo_id,
                binding.provider.as_db_str(),
                binding.project,
                kind.as_db_str(),
                key,
                comment_id,
            ],
        )?;
    }
    Ok(())
}

fn prune_missing(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<usize> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let cached = {
        let mut stmt = conn.prepare(
            "SELECT item_kind, item_key FROM papertrail_items
             WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND full_rewalk_seen=0",
        )?;
        stmt.query_map(params![repo_id, binding.provider.as_db_str(), binding.project], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut pruned = 0;
    for (kind, key) in cached {
        let kind = ItemKind::from_db_str(&kind)?;
        pruned += usize::from(delete_item(conn, binding, kind, &key)?);
    }
    Ok(pruned)
}

fn store_repo_comments(
    conn: &Connection,
    binding: &ResolvedTracker,
    comments: &[PapertrailComment],
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let tx = conn.unchecked_transaction()?;
    for comment in comments {
        let kind = tx
            .query_row(
                "SELECT item_kind FROM papertrail_items WHERE repo_id=?1 AND tracker=?2 AND \
                 project=?3 AND item_key=?4 LIMIT 1",
                params![repo_id, binding.provider.as_db_str(), binding.project, comment.item_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(kind) = kind else { continue };
        let mut comment = comment.clone();
        comment.item_kind = ItemKind::from_db_str(&kind)?;
        store_comment(&tx, binding.provider, &comment)?;
        report.stored_comments += 1;
    }
    tx.commit()?;
    Ok(())
}

fn min_item_updated_at(items: &[PapertrailItem]) -> Option<String> {
    items.iter().filter_map(|item| item.updated_at.clone()).min()
}

fn max_item_updated_at(items: &[PapertrailItem]) -> Option<String> {
    items.iter().filter_map(|item| item.updated_at.clone()).max()
}

fn max_timestamp(left: Option<String>, right: Option<String>) -> Option<String> {
    left.into_iter().chain(right).max()
}

fn common_comment_high_mark(streams: &BTreeMap<String, CommentStreamCursor>) -> Option<String> {
    streams
        .values()
        .map(|stream| stream.high_mark_at.clone())
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .min()
}

fn overlap_timestamp(timestamp: &str) -> String {
    let Some(core) = timestamp.strip_suffix('Z') else { return timestamp.to_string() };
    let Some((date, time)) = core.split_once('T') else { return timestamp.to_string() };
    let Some((year, month, day)) = parse_date(date) else { return timestamp.to_string() };
    let Some((mut hour, mut minute, mut second)) = parse_time(time) else {
        return timestamp.to_string();
    };
    let (mut year, mut month, mut day) = (year, month, day);
    if second > 0 {
        second -= 1;
    } else {
        second = 59;
        if minute > 0 {
            minute -= 1;
        } else {
            minute = 59;
            if hour > 0 {
                hour -= 1;
            } else {
                hour = 23;
                if day > 1 {
                    day -= 1;
                } else {
                    if month > 1 {
                        month -= 1;
                    } else {
                        year -= 1;
                        month = 12;
                    }
                    day = days_in_month(year, month);
                }
            }
        }
    }
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn parse_date(value: &str) -> Option<(i32, u32, u32)> {
    let mut parts = value.split('-');
    let parsed =
        (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    parts.next().is_none().then_some(parsed)
}

fn parse_time(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    let parsed =
        (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    parts.next().is_none().then_some(parsed)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

fn pause(error: &anyhow::Error) -> Option<(i64, PauseReason)> {
    error.chain().find_map(|cause| {
        cause.downcast_ref::<TransportError>().and_then(|error| match error {
            TransportError::Paused { resume_at_ms, reason } => Some((*resume_at_ms, *reason)),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;
    use crate::index::schema;

    struct ScriptClient {
        comment_streams: &'static [&'static str],
        pages: RefCell<VecDeque<anyhow::Result<ItemsPage>>>,
        probes: RefCell<VecDeque<FreshnessResult>>,
        item_comments: RefCell<VecDeque<anyhow::Result<Vec<PapertrailComment>>>>,
        item_comment_pages: RefCell<VecDeque<anyhow::Result<CommentsPage>>>,
        repo_comments: RefCell<VecDeque<anyhow::Result<CommentsPage>>>,
        item_page_requests: RefCell<Vec<PageCursor>>,
        item_comment_requests: RefCell<Vec<String>>,
        item_comment_page_requests: RefCell<Vec<PageCursor>>,
        repo_comment_requests: RefCell<Vec<PageCursor>>,
    }

    impl ScriptClient {
        fn new(pages: Vec<anyhow::Result<ItemsPage>>) -> Self {
            Self {
                comment_streams: &["default"],
                pages: RefCell::new(pages.into()),
                probes: RefCell::new(VecDeque::new()),
                item_comments: RefCell::new(VecDeque::new()),
                item_comment_pages: RefCell::new(VecDeque::new()),
                repo_comments: RefCell::new(VecDeque::new()),
                item_page_requests: RefCell::new(Vec::new()),
                item_comment_requests: RefCell::new(Vec::new()),
                item_comment_page_requests: RefCell::new(Vec::new()),
                repo_comment_requests: RefCell::new(Vec::new()),
            }
        }

        fn with_probe(self, probe: FreshnessResult) -> Self {
            self.probes.borrow_mut().push_back(probe);
            self
        }

        fn with_comment_streams(mut self, streams: &'static [&'static str]) -> Self {
            self.comment_streams = streams;
            self
        }

        fn with_item_comments(self, comments: Vec<PapertrailComment>) -> Self {
            self.item_comments.borrow_mut().push_back(Ok(comments));
            self
        }

        fn with_item_comment_results(
            self,
            results: Vec<anyhow::Result<Vec<PapertrailComment>>>,
        ) -> Self {
            self.item_comments.borrow_mut().extend(results);
            self
        }

        fn with_item_comment_pages(self, pages: Vec<anyhow::Result<CommentsPage>>) -> Self {
            self.item_comment_pages.borrow_mut().extend(pages);
            self
        }

        fn with_repo_comments(self, comments: Vec<PapertrailComment>) -> Self {
            self.repo_comments.borrow_mut().push_back(Ok(CommentsPage { comments, next: None }));
            self
        }

        fn with_repo_comment_pages(self, pages: Vec<anyhow::Result<CommentsPage>>) -> Self {
            self.repo_comments.borrow_mut().extend(pages);
            self
        }
    }

    impl PapertrailClient for ScriptClient {
        fn comment_streams(&self) -> &'static [&'static str] {
            self.comment_streams
        }

        async fn item(
            &self,
            _project: &str,
            _kind: ItemKind,
            _key: &str,
        ) -> anyhow::Result<PapertrailItem> {
            anyhow::bail!("unused")
        }

        async fn item_comments(
            &self,
            _project: &str,
            _kind: ItemKind,
            key: &str,
        ) -> anyhow::Result<Vec<PapertrailComment>> {
            self.item_comment_requests.borrow_mut().push(key.to_string());
            self.item_comments.borrow_mut().pop_front().unwrap_or(Ok(Vec::new()))
        }

        async fn item_comments_page(
            &self,
            _project: &str,
            _kind: ItemKind,
            key: &str,
            cursor: &PageCursor,
        ) -> anyhow::Result<CommentsPage> {
            self.item_comment_requests.borrow_mut().push(key.to_string());
            self.item_comment_page_requests.borrow_mut().push(cursor.clone());
            if let Some(page) = self.item_comment_pages.borrow_mut().pop_front() {
                return page;
            }
            Ok(CommentsPage {
                comments: self.item_comments.borrow_mut().pop_front().unwrap_or(Ok(Vec::new()))?,
                next: None,
            })
        }

        async fn items_page(
            &self,
            _project: &str,
            cursor: &PageCursor,
        ) -> anyhow::Result<ItemsPage> {
            self.item_page_requests.borrow_mut().push(cursor.clone());
            self.pages.borrow_mut().pop_front().expect("scripted item page")
        }

        async fn comments_page(
            &self,
            _project: &str,
            cursor: &PageCursor,
        ) -> anyhow::Result<CommentsPage> {
            self.repo_comment_requests.borrow_mut().push(cursor.clone());
            self.repo_comments
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(CommentsPage { comments: Vec::new(), next: None }))
        }

        async fn freshness_probe(
            &self,
            _project: &str,
            probe: &FreshnessProbe,
        ) -> anyhow::Result<FreshnessResult> {
            Ok(self.probes.borrow_mut().pop_front().unwrap_or(FreshnessResult {
                latest: None,
                etag: probe.etag.clone(),
                not_modified: true,
            }))
        }
    }

    #[test]
    fn provider_pagination_must_advance_some_cursor_state() {
        let current =
            PageCursor { page_token: Some("page-2".to_string()), ..PageCursor::default() };
        assert!(ensure_cursor_advanced(&current, &current, "test").is_err());

        let mut next = current.clone();
        next.provider_state = Some("opaque-state".to_string());
        ensure_cursor_advanced(&current, &next, "test").unwrap();
    }

    fn binding(tags: &[&str]) -> ResolvedTracker {
        ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
        }
    }

    fn item(key: &str, updated_at: &str, title: &str, tags: &[&str]) -> PapertrailItem {
        PapertrailItem {
            project: "o/r".to_string(),
            item_kind: ItemKind::Issue,
            item_key: key.to_string(),
            url: format!("https://github.com/o/r/issues/{key}"),
            state: "open".to_string(),
            title: title.to_string(),
            body: String::new(),
            author: None,
            created_at: None,
            updated_at: Some(updated_at.to_string()),
            merged_at: None,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
        }
    }

    fn page(items: Vec<PapertrailItem>) -> anyhow::Result<ItemsPage> {
        Ok(ItemsPage { items, next: None, backfill_boundary: None })
    }

    fn comment(key: &str, id: &str, updated_at: &str) -> PapertrailComment {
        PapertrailComment {
            project: "o/r".to_string(),
            item_kind: ItemKind::Issue,
            item_key: key.to_string(),
            comment_id: id.to_string(),
            url: None,
            body: "comment".to_string(),
            author: None,
            created_at: Some(updated_at.to_string()),
            updated_at: Some(updated_at.to_string()),
            review_state: None,
            anchor_path: None,
        }
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn keys(conn: &Connection) -> Vec<String> {
        let mut stmt =
            conn.prepare("SELECT item_key FROM papertrail_items ORDER BY item_key").unwrap();
        stmt.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect()
    }

    #[test]
    fn lifo_backfill_resumes_after_pause_without_gaps_or_duplicates() {
        let conn = db();
        let binding = binding(&[]);
        let paused = anyhow::Error::new(TransportError::Paused {
            resume_at_ms: 42,
            reason: PauseReason::QuotaReserve,
        });
        let first = ScriptClient::new(vec![
            page(vec![
                item("3", "2026-01-03T00:00:00Z", "three", &[]),
                item("2", "2026-01-02T00:00:00Z", "two", &[]),
            ]),
            Err(paused),
        ]);
        let report = block_on(mirror_binding(&conn, &binding, &first, false)).unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert_eq!(keys(&conn), vec!["2", "3"]);

        let second = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &second, false)).unwrap();
        assert_eq!(keys(&conn), vec!["1", "2", "3"]);
        let distinct: i64 = conn
            .query_row("SELECT COUNT(DISTINCT item_key) FROM papertrail_items", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(distinct, 3);
    }

    #[test]
    fn item_threads_commit_individually_and_resume_without_refetching_finished_threads() {
        let conn = db();
        let binding = binding(&[]);
        let same_page = vec![
            item("2", "2026-01-02T00:00:00Z", "two", &[]),
            item("1", "2026-01-01T00:00:00Z", "one", &[]),
        ];
        let first =
            ScriptClient::new(vec![page(same_page.clone())]).with_item_comment_results(vec![
                Ok(vec![comment("2", "two-comment", "2026-01-02T01:00:00Z")]),
                Err(anyhow::Error::new(TransportError::Paused {
                    resume_at_ms: 42,
                    reason: PauseReason::PassBudget,
                })),
            ]);
        let report = block_on(mirror_binding(&conn, &binding, &first, false)).unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert_eq!(keys(&conn), vec!["1", "2"]);

        let resumed = ScriptClient::new(vec![page(same_page), page(Vec::new())])
            .with_item_comments(vec![comment("1", "one-comment", "2026-01-01T01:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &resumed, false)).unwrap();
        assert_eq!(resumed.item_comment_requests.borrow().as_slice(), ["1"]);
        assert_eq!(keys(&conn), vec!["1", "2"]);
    }

    #[test]
    fn resumed_item_thread_revalidates_earlier_pages_before_pruning_deleted_comments() {
        let conn = db();
        let binding = binding(&[]);
        let provider_page = vec![item("1", "2026-01-01T00:00:00Z", "one", &[])];
        let next = PageCursor {
            stream: Some("default".to_string()),
            page_token: Some("thread-page-2".to_string()),
            ..PageCursor::default()
        };
        let first =
            ScriptClient::new(vec![page(provider_page.clone())]).with_item_comment_pages(vec![
                Ok(CommentsPage {
                    comments: vec![comment("1", "first", "2026-01-01T01:00:00Z")],
                    next: Some(next),
                }),
                Err(anyhow::Error::new(TransportError::Paused {
                    resume_at_ms: 42,
                    reason: PauseReason::PassBudget,
                })),
            ]);
        let report = block_on(mirror_binding(&conn, &binding, &first, false)).unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert_eq!(report.stored_comments, 1);
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(
            cursor
                .item_thread_cursor
                .as_ref()
                .and_then(|thread| thread.page_cursor.as_ref())
                .and_then(|cursor| cursor.page_token.as_deref()),
            Some("thread-page-2")
        );

        let resumed = ScriptClient::new(vec![page(provider_page), page(Vec::new())])
            .with_item_comment_pages(vec![
                Ok(CommentsPage {
                    // The first-page comment was deleted while this thread was paused.
                    comments: Vec::new(),
                    next: Some(PageCursor {
                        stream: Some("default".to_string()),
                        page_token: Some("thread-page-2".to_string()),
                        ..PageCursor::default()
                    }),
                }),
                Ok(CommentsPage {
                    comments: vec![comment("1", "second", "2026-01-01T02:00:00Z")],
                    next: None,
                }),
                // The confirming walk is identical, so absence of the deleted first comment is
                // now safe to apply destructively.
                Ok(CommentsPage {
                    comments: vec![comment("1", "second", "2026-01-01T02:00:00Z")],
                    next: None,
                }),
            ]);
        block_on(mirror_binding(&conn, &binding, &resumed, false)).unwrap();
        let requests = resumed.item_comment_page_requests.borrow();
        assert_eq!(requests[0].page_token, None);
        assert_eq!(requests[1].page_token.as_deref(), Some("thread-page-2"));
        assert_eq!(requests[2].page_token, None);
        assert!(load_cursor(&conn, &binding).unwrap().item_thread_cursor.is_none());
        let comments: Vec<String> = conn
            .prepare("SELECT comment_id FROM papertrail_comments ORDER BY comment_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(comments, vec!["second"]);
    }

    #[test]
    fn search_tie_pages_commit_and_resume_from_the_opaque_provider_cursor() {
        let conn = db();
        let binding = binding(&[]);
        let continuation = PageCursor {
            stream: Some("search_backfill".to_string()),
            updated_before: Some("2026-01-02T00:00:00Z".to_string()),
            page_token: Some("tie-page-2".to_string()),
            provider_state: Some("opaque-tie-state".to_string()),
            ..PageCursor::default()
        };
        let first = ScriptClient::new(vec![
            Ok(ItemsPage {
                items: vec![item("2", "2026-01-02T00:00:00Z", "two", &[])],
                next: Some(continuation),
                backfill_boundary: None,
            }),
            Err(anyhow::Error::new(TransportError::Paused {
                resume_at_ms: 42,
                reason: PauseReason::QuotaReserve,
            })),
        ]);
        let report = block_on(mirror_binding(&conn, &binding, &first, false)).unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert_eq!(keys(&conn), vec!["2"]);
        assert_eq!(
            load_cursor(&conn, &binding)
                .unwrap()
                .backfill_page_cursor
                .and_then(|cursor| cursor.page_token),
            Some("tie-page-2".to_string())
        );

        let resumed = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-02T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &resumed, false)).unwrap();
        let request = &resumed.item_page_requests.borrow()[0];
        assert_eq!(request.page_token.as_deref(), Some("tie-page-2"));
        assert_eq!(request.provider_state.as_deref(), Some("opaque-tie-state"));
        assert_eq!(keys(&conn), vec!["1", "2"]);
    }

    #[test]
    fn compound_provider_boundary_advances_below_every_physical_stream_page() {
        let conn = db();
        let binding = binding(&[]);
        let client = ScriptClient::new(vec![
            Ok(ItemsPage {
                items: vec![item("5", "2026-01-05T00:00:00Z", "pull", &[])],
                next: None,
                backfill_boundary: Some("2026-01-01T00:00:00Z".to_string()),
            }),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &client, false)).unwrap();

        let requests = client.item_page_requests.borrow();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].updated_before.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn resumed_page_refreshes_a_completed_item_when_its_provider_version_changed() {
        let conn = db();
        let binding = binding(&[]);
        let old_page = vec![
            item("2", "2026-01-02T00:00:00Z", "old", &[]),
            item("1", "2026-01-01T00:00:00Z", "one", &[]),
        ];
        let first = ScriptClient::new(vec![page(old_page)]).with_item_comment_results(vec![
            Ok(vec![comment("2", "old-comment", "2026-01-02T01:00:00Z")]),
            Err(anyhow::Error::new(TransportError::Paused {
                resume_at_ms: 42,
                reason: PauseReason::PassBudget,
            })),
        ]);
        block_on(mirror_binding(&conn, &binding, &first, false)).unwrap();

        let changed_page = vec![
            item("2", "2026-01-03T00:00:00Z", "new", &[]),
            item("1", "2026-01-01T00:00:00Z", "one", &[]),
        ];
        let resumed = ScriptClient::new(vec![page(changed_page), page(Vec::new())])
            .with_item_comment_results(vec![
                Ok(Vec::new()),
                Ok(vec![comment("2", "new-comment", "2026-01-03T01:00:00Z")]),
            ]);
        block_on(mirror_binding(&conn, &binding, &resumed, false)).unwrap();

        assert_eq!(resumed.item_comment_requests.borrow().as_slice(), ["1", "2"]);
        let title: String = conn
            .query_row("SELECT title FROM papertrail_items WHERE item_key='2'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "new");
        let comment_ids = conn
            .prepare("SELECT comment_id FROM papertrail_comments WHERE item_key='2'")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(comment_ids, vec!["new-comment"]);
    }

    #[test]
    fn resumed_changed_item_is_pruned_when_it_no_longer_matches_the_binding() {
        let conn = db();
        let binding = binding(&["bug"]);
        let first =
            ScriptClient::new(vec![page(vec![item("1", "2026-01-01T00:00:00Z", "old", &["bug"])])])
                .with_item_comment_results(vec![Err(anyhow::Error::new(TransportError::Paused {
                    resume_at_ms: 42,
                    reason: PauseReason::PassBudget,
                }))]);
        let report = block_on(mirror_binding(&conn, &binding, &first, false)).unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert_eq!(keys(&conn), vec!["1"]);

        let resumed = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-02T00:00:00Z", "changed", &["feature"])]),
            page(Vec::new()),
        ]);
        let report = block_on(mirror_binding(&conn, &binding, &resumed, false)).unwrap();
        assert_eq!(report.pruned_items, 1);
        assert!(keys(&conn).is_empty());
        assert!(resumed.item_comment_requests.borrow().is_empty());
        assert!(load_cursor(&conn, &binding).unwrap().item_thread_cursor.is_none());
    }

    #[test]
    fn empty_initial_project_enters_delta_polling_and_discovers_later_items() {
        let conn = db();
        let binding = binding(&[]);
        block_on(mirror_binding(
            &conn,
            &binding,
            &ScriptClient::new(vec![page(Vec::new())]),
            false,
        ))
        .unwrap();
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(cursor.backfill_done);
        assert_eq!(cursor.high_mark_at.as_deref(), Some(EMPTY_PROJECT_HIGH_MARK));

        let later =
            ScriptClient::new(vec![page(vec![item("1", "2026-01-01T00:00:00Z", "later", &[])])])
                .with_probe(FreshnessResult {
                    latest: Some("2026-01-01T00:00:00Z".to_string()),
                    etag: Some("v1".to_string()),
                    not_modified: false,
                });
        block_on(mirror_binding(&conn, &binding, &later, false)).unwrap();
        assert_eq!(keys(&conn), vec!["1"]);
        assert_eq!(
            later.item_page_requests.borrow()[0].updated_since.as_deref(),
            Some("1969-12-31T23:59:59Z")
        );
    }

    #[test]
    fn delta_catches_an_item_updated_while_backfill_is_paused() {
        let conn = db();
        let binding = binding(&[]);
        let first = ScriptClient::new(vec![
            page(vec![item("2", "2026-01-02T00:00:00Z", "old", &[])]),
            Err(anyhow::Error::new(TransportError::Paused {
                resume_at_ms: 42,
                reason: PauseReason::PassBudget,
            })),
        ]);
        block_on(mirror_binding(&conn, &binding, &first, false)).unwrap();
        let second = ScriptClient::new(vec![
            page(vec![item("2", "2026-01-04T00:00:00Z", "updated", &[])]),
            page(Vec::new()),
            page(Vec::new()),
        ])
        .with_probe(FreshnessResult {
            latest: Some("2026-01-04T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        });
        block_on(mirror_binding(&conn, &binding, &second, false)).unwrap();
        let title: String = conn
            .query_row("SELECT title FROM papertrail_items WHERE item_key='2'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "updated");
    }

    #[test]
    fn changed_etag_replays_the_overlap_even_when_latest_timestamp_is_tied() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-02T00:00:00Z", "old", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let tied = ScriptClient::new(vec![page(vec![item(
            "2",
            "2026-01-02T00:00:00Z",
            "same-second",
            &[],
        )])])
        .with_probe(FreshnessResult {
            latest: None,
            etag: Some("changed".to_string()),
            not_modified: false,
        });
        block_on(mirror_binding(&conn, &binding, &tied, false)).unwrap();
        assert_eq!(keys(&conn), vec!["1", "2"]);
        assert_eq!(
            load_cursor(&conn, &binding).unwrap().high_mark_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
    }

    #[test]
    fn item_delta_persists_the_next_page_before_a_pause() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let next = PageCursor {
            updated_since: Some("2025-12-31T23:59:59Z".to_string()),
            page_token: Some("delta-page-2".to_string()),
            ..PageCursor::default()
        };
        let paused = ScriptClient::new(vec![
            Ok(ItemsPage {
                items: vec![item("2", "2026-01-02T00:00:00Z", "two", &[])],
                next: Some(next),
                backfill_boundary: None,
            }),
            Err(anyhow::Error::new(TransportError::Paused {
                resume_at_ms: 42,
                reason: PauseReason::PassBudget,
            })),
        ])
        .with_probe(FreshnessResult {
            latest: Some("2026-01-02T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        });
        let report = block_on(mirror_binding(&conn, &binding, &paused, false)).unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(cursor.item_delta_in_progress);
        assert_eq!(cursor.item_delta_page_token.as_deref(), Some("delta-page-2"));

        let resumed =
            ScriptClient::new(vec![page(vec![item("3", "2026-01-03T00:00:00Z", "three", &[])])]);
        block_on(mirror_binding(&conn, &binding, &resumed, false)).unwrap();
        assert_eq!(
            resumed.item_page_requests.borrow()[0].page_token.as_deref(),
            Some("delta-page-2")
        );
        assert_eq!(
            resumed.item_page_requests.borrow()[0].updated_since.as_deref(),
            Some("2025-12-31T23:59:59Z")
        );
        assert_eq!(keys(&conn), vec!["1", "2", "3"]);
    }

    #[test]
    fn paginated_item_delta_advances_only_to_the_first_page_frontier() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let next =
            PageCursor { page_token: Some("delta-page-2".to_string()), ..Default::default() };
        let delta = ScriptClient::new(vec![
            Ok(ItemsPage {
                items: vec![item("2", "2026-01-02T00:00:00Z", "two", &[])],
                next: Some(next),
                backfill_boundary: None,
            }),
            page(vec![item("3", "2026-01-04T00:00:00Z", "three", &[])]),
        ])
        .with_probe(FreshnessResult {
            latest: Some("2026-01-05T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        });
        block_on(mirror_binding(&conn, &binding, &delta, false)).unwrap();

        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(cursor.high_mark_at.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert!(cursor.probe_etag.is_none(), "the conservative frontier must force a replay");
        assert_eq!(keys(&conn), vec!["1", "2", "3"]);

        let replay = ScriptClient::new(vec![page(vec![item(
            "4",
            "2026-01-03T00:00:00Z",
            "shifted boundary row",
            &[],
        )])])
        .with_probe(FreshnessResult {
            latest: Some("2026-01-05T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        });
        block_on(mirror_binding(&conn, &binding, &replay, false)).unwrap();
        assert_eq!(keys(&conn), vec!["1", "2", "3", "4"]);
    }

    #[test]
    fn item_delta_does_not_advance_to_an_unobserved_probe_timestamp() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let raced = ScriptClient::new(vec![page(Vec::new())]).with_probe(FreshnessResult {
            latest: Some("2026-01-05T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        });
        block_on(mirror_binding(&conn, &binding, &raced, false)).unwrap();

        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(cursor.high_mark_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert!(cursor.probe_etag.is_none());
    }

    #[test]
    fn a_stable_paginated_tie_settles_after_one_forced_replay() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let tied_page = || {
            Ok(ItemsPage {
                items: vec![item("2", "2026-01-02T00:00:00Z", "tie", &[])],
                next: Some(PageCursor {
                    page_token: Some("tie-page-2".to_string()),
                    ..PageCursor::default()
                }),
                backfill_boundary: None,
            })
        };
        let first =
            ScriptClient::new(vec![tied_page(), page(Vec::new())]).with_probe(FreshnessResult {
                latest: Some("2026-01-02T00:00:00Z".to_string()),
                etag: Some("v2".to_string()),
                not_modified: false,
            });
        block_on(mirror_binding(&conn, &binding, &first, false)).unwrap();
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(cursor.item_delta_replay_required);
        assert!(cursor.probe_etag.is_none());

        let replay =
            ScriptClient::new(vec![tied_page(), page(Vec::new())]).with_probe(FreshnessResult {
                latest: None,
                etag: Some("v2".to_string()),
                not_modified: false,
            });
        block_on(mirror_binding(&conn, &binding, &replay, false)).unwrap();
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(!cursor.item_delta_replay_required);
        assert_eq!(cursor.probe_etag.as_deref(), Some("v2"));
    }

    #[test]
    fn changing_tag_filter_prunes_and_backfills_the_new_match_set() {
        let conn = db();
        let bug = binding(&["bug"]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-02T00:00:00Z", "bug", &["bug"])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &bug, &initial, false)).unwrap();
        assert_eq!(keys(&conn), vec!["1"]);

        let docs = binding(&["docs"]);
        let changed = ScriptClient::new(vec![
            page(vec![item("2", "2026-01-03T00:00:00Z", "docs", &["docs"])]),
            page(Vec::new()),
        ]);
        let report = block_on(mirror_binding(&conn, &docs, &changed, false)).unwrap();
        assert_eq!(report.pruned_items, 1);
        assert!(report.completed_full_walk);
        assert_eq!(keys(&conn), vec!["2"]);
    }

    #[test]
    fn forced_full_rewalk_heals_a_poisoned_row() {
        let conn = db();
        let binding = binding(&[]);
        store_item(&conn, Tracker::Github, &item("1", "2026-01-01T00:00:00Z", "poison", &[]))
            .unwrap();
        store_item(&conn, Tracker::Github, &item("2", "2026-01-01T00:00:00Z", "deleted", &[]))
            .unwrap();
        let client = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "healed", &[])]),
            page(Vec::new()),
        ]);
        let report = block_on(mirror_binding(&conn, &binding, &client, true)).unwrap();
        let title: String = conn
            .query_row("SELECT title FROM papertrail_items WHERE item_key='1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "healed");
        assert_eq!(report.pruned_items, 1);
        assert_eq!(keys(&conn), vec!["1"]);
    }

    #[test]
    fn full_rewalk_pruning_survives_a_pause_and_an_ordinary_resume() {
        let conn = db();
        let binding = binding(&[]);
        store_item(&conn, Tracker::Github, &item("1", "2026-01-01T00:00:00Z", "kept", &[]))
            .unwrap();
        store_item(&conn, Tracker::Github, &item("2", "2026-01-01T00:00:00Z", "gone", &[]))
            .unwrap();
        let paused = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "kept", &[])]),
            Err(anyhow::Error::new(TransportError::Paused {
                resume_at_ms: 42,
                reason: PauseReason::PassBudget,
            })),
        ]);
        let report = block_on(mirror_binding(&conn, &binding, &paused, true)).unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert!(load_cursor(&conn, &binding).unwrap().full_rewalk);
        assert_eq!(keys(&conn), vec!["1", "2"]);

        let resumed = ScriptClient::new(vec![page(Vec::new())]);
        let report = block_on(mirror_binding(&conn, &binding, &resumed, false)).unwrap();
        assert!(report.completed_full_walk);
        assert_eq!(report.pruned_items, 1);
        assert!(!load_cursor(&conn, &binding).unwrap().full_rewalk);
        assert_eq!(keys(&conn), vec!["1"]);
    }

    #[test]
    fn continuation_classification_covers_every_persisted_resume_lane() {
        assert_eq!(MirrorCursor::default().continuation(), MirrorContinuation::None);
        let mut cursor = MirrorCursor { backfill_done: true, ..Default::default() };
        assert_eq!(cursor.continuation(), MirrorContinuation::None);

        cursor.item_delta_replay_required = true;
        assert_eq!(cursor.continuation(), MirrorContinuation::Incremental);
        cursor.item_delta_replay_required = false;
        cursor.comment_stream_cursors.insert("default".to_string(), CommentStreamCursor {
            page_token: Some("next".to_string()),
            ..Default::default()
        });
        assert_eq!(cursor.continuation(), MirrorContinuation::Incremental);

        cursor.full_rewalk = true;
        assert_eq!(cursor.continuation(), MirrorContinuation::Full);

        let partial_backfill = MirrorCursor {
            low_mark_at: Some("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        assert_eq!(partial_backfill.continuation(), MirrorContinuation::Incremental);
    }

    #[test]
    fn non_pause_errors_propagate_and_pause_classifier_ignores_them() {
        let conn = db();
        let binding = binding(&[]);
        let client = ScriptClient::new(vec![Err(anyhow::anyhow!("provider failed"))]);
        let error = block_on(mirror_binding(&conn, &binding, &client, false)).unwrap_err();
        assert_eq!(error.to_string(), "provider failed");
        assert!(pause(&error).is_none());

        let unused = block_on(client.item("o/r", ItemKind::Issue, "1")).unwrap_err();
        assert_eq!(unused.to_string(), "unused");
    }

    #[test]
    fn failed_comment_refresh_keeps_the_previous_complete_thread() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "initial", &[])]),
            page(Vec::new()),
        ])
        .with_item_comments(vec![comment("1", "old", "2026-01-01T01:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let failed =
            ScriptClient::new(vec![page(vec![item("1", "2026-01-02T00:00:00Z", "updated", &[])])])
                .with_probe(FreshnessResult {
                    latest: Some("2026-01-02T00:00:00Z".to_string()),
                    etag: Some("v2".to_string()),
                    not_modified: false,
                })
                .with_item_comment_results(vec![Err(anyhow::anyhow!("comment fetch failed"))]);
        let error = block_on(mirror_binding(&conn, &binding, &failed, false)).unwrap_err();
        assert_eq!(error.to_string(), "comment fetch failed");
        let comments: Vec<String> = conn
            .prepare("SELECT comment_id FROM papertrail_comments ORDER BY comment_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(comments, vec!["old"]);
    }

    #[test]
    fn invalid_item_delta_continuation_is_not_persisted() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "initial", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let unchanged = PageCursor {
            updated_since: Some("2025-12-31T23:59:59Z".to_string()),
            ..PageCursor::default()
        };
        let invalid = ScriptClient::new(vec![Ok(ItemsPage {
            items: vec![item("2", "2026-01-02T00:00:00Z", "must-not-commit", &[])],
            next: Some(unchanged),
            backfill_boundary: None,
        })])
        .with_probe(FreshnessResult {
            latest: Some("2026-01-02T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        });
        let error = block_on(mirror_binding(&conn, &binding, &invalid, false)).unwrap_err();
        assert!(error.to_string().contains("did not advance"));
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(cursor.item_delta_page_token, None);
        assert!(cursor.item_delta_in_progress, "the valid scan start remains retryable");
        assert_eq!(keys(&conn), vec!["1"]);
    }

    #[test]
    fn invalid_repo_comment_continuation_commits_neither_page_nor_cursor() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "initial", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let invalid = ScriptClient::new(Vec::new())
            .with_probe(FreshnessResult {
                latest: None,
                etag: Some("v1".to_string()),
                not_modified: true,
            })
            .with_repo_comment_pages(vec![Ok(CommentsPage {
                comments: vec![comment("1", "must-not-commit", "2026-01-02T00:00:00Z")],
                next: Some(PageCursor {
                    stream: Some("other".to_string()),
                    page_token: Some("page-2".to_string()),
                    ..PageCursor::default()
                }),
            })]);
        let error = block_on(mirror_binding(&conn, &binding, &invalid, false)).unwrap_err();
        assert!(error.to_string().contains("crossed"));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM papertrail_comments", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(cursor.comment_stream_cursors["default"].page_token.is_none());
        assert!(cursor.comment_stream_cursors["default"].scan_high_mark_at.is_none());
    }

    #[test]
    fn delta_walks_continuations_and_updates_repo_wide_comments() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "initial", &[])]),
            page(Vec::new()),
        ])
        .with_item_comments(vec![comment("1", "initial", "2026-01-01T01:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let continuation = PageCursor {
            updated_since: Some("2026-01-01T00:00:00Z".to_string()),
            page_token: Some("page-2".to_string()),
            ..PageCursor::default()
        };
        let delta = ScriptClient::new(vec![
            Ok(ItemsPage {
                items: vec![item("1", "2026-01-02T00:00:00Z", "updated", &[])],
                next: Some(continuation),
                backfill_boundary: None,
            }),
            page(Vec::new()),
        ])
        .with_probe(FreshnessResult {
            latest: Some("2026-01-03T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        })
        .with_item_comments(vec![comment("1", "item", "2026-01-02T01:00:00Z")])
        .with_repo_comments(vec![
            comment("1", "repo", "2026-01-03T00:00:00Z"),
            comment("404", "orphan", "2026-01-04T00:00:00Z"),
        ]);
        let report = block_on(mirror_binding(&conn, &binding, &delta, false)).unwrap();
        assert_eq!(report.stored_items, 1);
        assert_eq!(report.stored_comments, 2);
        let comments: i64 = conn
            .query_row("SELECT COUNT(*) FROM papertrail_comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(comments, 2, "the complete item thread replaces its stale predecessor");
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(
            cursor.high_mark_at.as_deref(),
            Some("2026-01-02T00:00:00Z"),
            "a mutable continuation advances only through the first consumed page"
        );
        assert_eq!(cursor.comment_high_mark_at.as_deref(), Some("2026-01-04T00:00:00Z"));
        assert!(cursor.probe_etag.is_none());
    }

    #[test]
    fn comment_pages_commit_progress_before_a_pause_and_resume_from_the_token() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ])
        .with_repo_comments(vec![comment("1", "seed", "2026-01-01T00:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let next = PageCursor { page_token: Some("page-2".to_string()), ..PageCursor::default() };
        let paused = ScriptClient::new(Vec::new())
            .with_probe(FreshnessResult {
                latest: None,
                etag: Some("v1".to_string()),
                not_modified: true,
            })
            .with_repo_comment_pages(vec![
                Ok(CommentsPage {
                    comments: vec![comment("1", "first", "2026-01-03T00:00:00Z")],
                    next: Some(next),
                }),
                Err(anyhow::Error::new(TransportError::Paused {
                    resume_at_ms: 42,
                    reason: PauseReason::PassBudget,
                })),
            ]);
        let report = block_on(mirror_binding(&conn, &binding, &paused, false)).unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        let cursor = load_cursor(&conn, &binding).unwrap();
        let stream = &cursor.comment_stream_cursors["default"];
        assert_eq!(stream.page_token.as_deref(), Some("page-2"));
        assert_eq!(cursor.comment_high_mark_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(stream.scan_high_mark_at.as_deref(), Some("2026-01-03T00:00:00Z"));

        let resumed = ScriptClient::new(Vec::new())
            .with_probe(FreshnessResult {
                latest: None,
                etag: Some("v1".to_string()),
                not_modified: true,
            })
            .with_repo_comments(vec![comment("1", "second", "2026-01-02T00:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &resumed, false)).unwrap();
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(cursor.comment_stream_cursors["default"].page_token.is_none());
        assert_eq!(cursor.comment_high_mark_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        let comments: i64 = conn
            .query_row("SELECT COUNT(*) FROM papertrail_comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(comments, 3);
    }

    #[test]
    fn paginated_repo_comments_advance_only_to_the_first_page_frontier() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ])
        .with_repo_comments(vec![comment("1", "seed", "2026-01-01T00:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let next =
            PageCursor { page_token: Some("comments-page-2".to_string()), ..Default::default() };
        let delta = ScriptClient::new(Vec::new())
            .with_probe(FreshnessResult {
                latest: None,
                etag: Some("v1".to_string()),
                not_modified: true,
            })
            .with_repo_comment_pages(vec![
                Ok(CommentsPage {
                    comments: vec![comment("1", "first", "2026-01-02T00:00:00Z")],
                    next: Some(next),
                }),
                Ok(CommentsPage {
                    comments: vec![comment("1", "later", "2026-01-04T00:00:00Z")],
                    next: None,
                }),
            ]);
        block_on(mirror_binding(&conn, &binding, &delta, false)).unwrap();

        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(
            cursor.comment_stream_cursors["default"].high_mark_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
    }

    #[test]
    fn comment_not_found_skips_the_thread_without_erasing_cached_evidence() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ])
        .with_item_comments(vec![comment("1", "seed", "2026-01-01T01:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let missing =
            ScriptClient::new(vec![page(vec![item("1", "2026-01-02T00:00:00Z", "updated", &[])])])
                .with_probe(FreshnessResult {
                    latest: Some("2026-01-02T00:00:00Z".to_string()),
                    etag: Some("v2".to_string()),
                    not_modified: false,
                })
                .with_item_comment_results(vec![Err(PapertrailClientError::ItemNotFound.into())]);
        let report = block_on(mirror_binding(&conn, &binding, &missing, false)).unwrap();

        assert_eq!(report.pruned_items, 0);
        assert_eq!(keys(&conn), vec!["1"]);
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(cursor.item_thread_cursor.is_none());
        assert!(!cursor.item_delta_in_progress);
    }

    #[test]
    fn repo_comment_delta_overlaps_its_watermark_and_keeps_the_scan_boundary_across_pages() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ])
        .with_repo_comments(vec![comment("1", "first", "2026-01-02T00:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let next =
            PageCursor { page_token: Some("comments-page-2".to_string()), ..Default::default() };
        let delta = ScriptClient::new(Vec::new())
            .with_probe(FreshnessResult {
                latest: None,
                etag: Some("v1".to_string()),
                not_modified: true,
            })
            .with_repo_comment_pages(vec![
                Ok(CommentsPage { comments: Vec::new(), next: Some(next) }),
                Ok(CommentsPage { comments: Vec::new(), next: None }),
            ]);
        block_on(mirror_binding(&conn, &binding, &delta, false)).unwrap();
        let requests = delta.repo_comment_requests.borrow();
        assert_eq!(requests[0].updated_since.as_deref(), Some("2026-01-01T23:59:59Z"));
        assert_eq!(requests[1].updated_since, requests[0].updated_since);
    }

    #[test]
    fn independent_comment_streams_do_not_advance_an_unscanned_stream() {
        let conn = db();
        let binding = binding(&[]);
        let initial = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ])
        .with_comment_streams(&["issue_comments", "review_comments"])
        .with_repo_comment_pages(vec![
            Ok(CommentsPage {
                comments: vec![comment("1", "issue", "2026-01-01T00:00:00Z")],
                next: None,
            }),
            Ok(CommentsPage {
                comments: vec![comment("1", "review", "2026-01-03T00:00:00Z")],
                next: None,
            }),
        ]);
        block_on(mirror_binding(&conn, &binding, &initial, false)).unwrap();

        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(
            cursor.comment_stream_cursors["issue_comments"].high_mark_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(
            cursor.comment_stream_cursors["review_comments"].high_mark_at.as_deref(),
            Some("2026-01-03T00:00:00Z")
        );
        assert_eq!(cursor.comment_high_mark_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        let initial_requests = initial.repo_comment_requests.borrow();
        assert!(initial_requests.iter().all(|request| request.updated_since.is_none()));

        let next = ScriptClient::new(Vec::new())
            .with_comment_streams(&["issue_comments", "review_comments"])
            .with_probe(FreshnessResult {
                latest: None,
                etag: Some("v1".to_string()),
                not_modified: true,
            })
            .with_repo_comment_pages(vec![
                Ok(CommentsPage {
                    comments: vec![comment("1", "late-issue", "2026-01-02T00:00:00Z")],
                    next: None,
                }),
                Ok(CommentsPage { comments: Vec::new(), next: None }),
            ]);
        block_on(mirror_binding(&conn, &binding, &next, false)).unwrap();
        let requests = next.repo_comment_requests.borrow();
        assert_eq!(requests[0].stream.as_deref(), Some("issue_comments"));
        assert_eq!(requests[0].updated_since.as_deref(), Some("2025-12-31T23:59:59Z"));
        assert_eq!(requests[1].stream.as_deref(), Some("review_comments"));
        assert_eq!(requests[1].updated_since.as_deref(), Some("2026-01-02T23:59:59Z"));
    }

    #[test]
    fn item_thread_snapshots_do_not_advance_the_repo_comment_watermark() {
        let conn = db();
        let binding = binding(&[]);
        let client = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ])
        .with_item_comments(vec![comment("1", "thread", "2026-02-01T00:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, &client, false)).unwrap();
        assert_eq!(client.repo_comment_requests.borrow()[0].updated_since, None);
        assert_eq!(load_cursor(&conn, &binding).unwrap().comment_high_mark_at, None);
    }

    #[test]
    fn delta_overlap_rewinds_one_second_without_changing_provider_tokens() {
        assert_eq!(overlap_timestamp("2026-01-02T00:00:00Z"), "2026-01-01T23:59:59Z");
        assert_eq!(overlap_timestamp("2026-01-01T00:00:01Z"), "2026-01-01T00:00:00Z");
        assert_eq!(overlap_timestamp("2026-01-01T00:01:00Z"), "2026-01-01T00:00:59Z");
        assert_eq!(overlap_timestamp("2026-01-01T01:00:00Z"), "2026-01-01T00:59:59Z");
        assert_eq!(overlap_timestamp("2026-05-01T00:00:00Z"), "2026-04-30T23:59:59Z");
        assert_eq!(overlap_timestamp("2024-03-01T00:00:00Z"), "2024-02-29T23:59:59Z");
        assert_eq!(overlap_timestamp("2026-03-01T00:00:00Z"), "2026-02-28T23:59:59Z");
        assert_eq!(overlap_timestamp("2026-01-01T00:00:00Z"), "2025-12-31T23:59:59Z");
        assert_eq!(overlap_timestamp("2026-01-01TinvalidZ"), "2026-01-01TinvalidZ");
        assert_eq!(overlap_timestamp("provider-token"), "provider-token");

        let outside = anyhow::Error::new(TransportError::UrlOutsideBinding {
            url: "https://other.example".to_string(),
            host: "github.example".to_string(),
            problem: "origin differs",
        });
        assert!(pause(&outside).is_none());

        let legacy = decode_processed_items(Some(r#"[["issue","1"]]"#.to_string()), 0).unwrap();
        assert!(legacy.contains(&ProcessedItem {
            kind: "issue".to_string(),
            key: "1".to_string(),
            updated_at: None,
        }));
    }

    #[test]
    fn unmatched_pages_skip_comments_and_delete_cached_items() {
        let conn = db();
        let all = binding(&[]);
        assert_eq!(prune_unmatched(&conn, &all).unwrap(), 0);
        store_item(&conn, Tracker::Github, &item("1", "2026-01-01T00:00:00Z", "cached", &["docs"]))
            .unwrap();

        let bugs = binding(&["bug"]);
        let client = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-02T00:00:00Z", "still docs", &["docs"])]),
            page(Vec::new()),
        ]);
        let report = block_on(mirror_binding(&conn, &bugs, &client, false)).unwrap();
        assert!(report.pruned_items >= 1);
        assert!(keys(&conn).is_empty());
    }
}
