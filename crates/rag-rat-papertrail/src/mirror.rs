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
    /// The item freshness probe answered not-modified. Combined with zero stored / pruned work
    /// this classifies the run as a successful PROBE — advancing probe freshness only, never the
    /// mirror or full-walk timestamps.
    pub probe_not_modified: bool,
    /// Provider-attested closing edges stored by the attested-closers walk (#702 stage 2).
    pub attested_edges: usize,
    /// Rows the attested-closers walk MUTATED besides fresh edge inserts (counted separately in
    /// `attested_edges`): per-closer replace-set DELETEs and item resolution / merge-sha UPDATEs.
    /// A run that only reaped a stale edge or stamped a resolution moved content — this keeps such
    /// a run from being misclassified as a probe when the item feed was not-modified.
    pub attested_writes: usize,
    /// The attested walk's failure, when it had one — EXPLICIT but non-fatal: the mirror data
    /// this run landed is kept, the watermark does not advance, and the next sync retries.
    pub attested_error: Option<String>,
}

pub(crate) async fn mirror_binding<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    trackers: &[ResolvedTracker],
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
        attested_edges: 0,
        attested_writes: 0,
        attested_error: None,
        paused_until_ms: None,
        pause_reason: None,
        completed_full_walk: false,
        probe_not_modified: false,
    };
    if filter_changed {
        report.pruned_items += prune_unmatched(conn, binding)?;
        // A widened filter caches newly-in-scope closed issues; a full-rewalk already ran the
        // same clear above. (Narrowing is handled by edge deletion on prune, but clearing here
        // covers both directions uniformly.) See `clear_attested_watermark` for the invariant.
        clear_attested_watermark(conn, binding)?;
        save_cursor(conn, binding, &cursor, false)?;
    }

    let result =
        mirror_binding_inner(conn, binding, trackers, client, &mut cursor, &mut report).await;
    match result {
        Ok(()) => {
            // Stage 2 (#702): the attested-closers walk runs after the item/comment walk so its
            // per-item outcome updates land on freshly-cached rows. Its failure is non-fatal —
            // the walk is an enrichment over data the mirror already landed and its watermark
            // stays put — BUT a rate-limit/pass-budget PAUSE must still surface as a pause, or
            // `run_binding_job` records the binding healthy and clears retry state while the
            // attested watermark is stale. Propagate the resume time; fold any other error into
            // the explicit non-fatal `attested_error`.
            if let Err(error) = sync_attested_closers(conn, binding, client, &mut report).await {
                match pause(&error) {
                    Some((resume_at_ms, reason)) => {
                        report.paused_until_ms = Some(resume_at_ms);
                        report.pause_reason = Some(reason.as_str().to_string());
                    },
                    None => report.attested_error = Some(error.to_string()),
                }
            }
            // Persist the attested-walk health for the durable status snapshot: a HARD failure is
            // stored, a clean completion clears it. A pause (retry clock already set) leaves the
            // prior state untouched.
            if let Some(detail) = &report.attested_error {
                set_attested_error(conn, binding, detail)?;
            } else if report.paused_until_ms.is_none() {
                clear_attested_error(conn, binding)?;
            }
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
    trackers: &[ResolvedTracker],
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
            sync_item_delta(conn, binding, trackers, client, cursor, report).await?;
        } else {
            let probe = client
                .freshness_probe(&binding.project, &FreshnessProbe {
                    updated_since: Some(high.to_string()),
                    etag: cursor.probe_etag.clone(),
                })
                .await?;
            cursor.probe_etag = probe.etag;
            // A quiet probe must not starve an OWED replay: when the prior delta left its
            // conservative frontier below the probe target, the boundary replay has to run even
            // if nothing new moved — some providers (GitLab) report a timestamp tie as
            // not_modified, and the stranded boundary row would otherwise wait for the daily
            // full walk. probe.latest is None on that path, which sync_item_delta already
            // treats as "replay against the durable high mark".
            if !probe.not_modified || cursor.item_delta_replay_required {
                cursor.item_delta_in_progress = true;
                cursor.item_delta_scan_since = Some(overlap_timestamp(high));
                cursor.item_delta_high_mark_at = probe.latest;
                save_cursor(conn, binding, cursor, false)?;
                sync_item_delta(conn, binding, trackers, client, cursor, report).await?;
            } else {
                report.probe_not_modified = true;
                save_cursor(conn, binding, cursor, false)?;
            }
        }
        sync_comment_delta(conn, binding, trackers, client, cursor, report).await?;
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
            // The consumed continuation must not outlive the walk: a chained provider leg (the
            // request that produced THIS empty page) was persisted as `backfill_page_cursor` on
            // the previous iteration, and leaving it behind makes `continuation()` misread the
            // COMPLETED walk as interrupted work forever.
            cursor.backfill_page_cursor = None;
            save_cursor(conn, binding, cursor, false)?;
            break;
        }
        if let Some(next) = &page.next {
            ensure_cursor_advanced(&request, next, "backfill")?;
        }
        store_item_page_resumably(
            conn,
            binding,
            trackers,
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
        sync_comment_delta(conn, binding, trackers, client, cursor, report).await?;
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
    trackers: &[ResolvedTracker],
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
            trackers,
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
    trackers: &[ResolvedTracker],
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
            store_repo_comments(conn, binding, trackers, &page.comments, report)?;
            let state = cursor.comment_stream_cursors.get_mut(*stream).expect("stream inserted");
            if let Some(first_page_high) = first_page_high {
                // As with item deltas, a mutable continuation proves no more than the first
                // ascending page. Replaying its inclusive upper boundary prevents offset shifts
                // from stranding an unseen or stale comment below the durable watermark.
                state.scan_high_mark_at = first_page_high;
            }
            // The provider-confirmed frontier is trusted on EVERY page — providers only set it
            // for immutable append-only feeds (see CommentsPage::frontier). Folding each page's
            // frontier carries a drained multi-page window past its LAST page, where the
            // first-page comment maximum alone would pin a busy window forever.
            if page.frontier.is_some() {
                state.scan_high_mark_at =
                    max_timestamp(state.scan_high_mark_at.take(), page.frontier.clone());
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

// One over the lint cap: the resumable walk threads (binding, trackers, cursor, report) through
// every stage, and bundling two of them into a one-off struct here would just rename the train.
#[allow(clippy::too_many_arguments)]
async fn store_item_page_resumably<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    trackers: &[ResolvedTracker],
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
                    begin_item_thread(conn, binding, trackers, &item, lane, cursor, report)?;
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
            resume_item_thread(conn, binding, trackers, client, cursor, report).await?;
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
            begin_item_thread(conn, binding, trackers, &item, lane, cursor, report)?;
            resume_item_thread(conn, binding, trackers, client, cursor, report).await?;
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
    trackers: &[ResolvedTracker],
    item: &PapertrailItem,
    lane: PageLane,
    cursor: &mut MirrorCursor,
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    store_item(&tx, binding.provider, item)?;
    // #702: the item's own text is mined in the same transaction — refs (`source_kind='item'`)
    // and, for a change request with closing keywords, the text-tier closing edge.
    sync::mine_item_refs(&tx, binding.provider, trackers, item)?;
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
    trackers: &[ResolvedTracker],
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
            sync::mine_comment_refs(&tx, binding.provider, trackers, comment)?;
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
    // A full rewalk re-caches every closed issue; clear the attested watermark so their
    // provider closers get re-fetched from the top (the twin of the filter-change reset).
    clear_attested_watermark(conn, binding)?;
    save_cursor(conn, binding, cursor, false)
}

fn reset_full_seen(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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

/// A per-binding `index_meta` key for the attested-closers lane. `kind` is a stable machine token
/// (`since` — the walk watermark; `error` — the last hard failure detail). Repo-scoped so a
/// consolidated multi-repo DB keeps each binding's lane state separate.
fn attested_meta_key(
    binding: &ResolvedTracker,
    conn: &Connection,
    kind: &str,
) -> anyhow::Result<String> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    Ok(format!(
        "papertrail_attested_closers_{kind}:{repo_id}:{}:{}",
        binding.provider.as_db_str(),
        binding.project
    ))
}

/// The `index_meta` key for a binding's attested-closers watermark — shared by the walk (read/
/// stamp) and the reset seams so they can never drift.
fn attested_since_key(binding: &ResolvedTracker, conn: &Connection) -> anyhow::Result<String> {
    attested_meta_key(binding, conn, "since")
}

/// Persist a HARD attested-walk failure so it stays visible in `papertrail_sync_status`. This is
/// SEPARATE from the item-mirror health (`error_class`/`error_detail`, owned by `record_success` /
/// `record_failure`): the item walk can succeed — clearing its own error state and advancing
/// freshness — while the enrichment walk fails every tick with a stale watermark. Without this the
/// failure is invisible in the durable status snapshot. Detail is length-capped and stripped of
/// control chars. A pause is NOT persisted here (it rides the retry clock); a clean walk clears it.
pub(crate) fn set_attested_error(
    conn: &Connection,
    binding: &ResolvedTracker,
    detail: &str,
) -> anyhow::Result<()> {
    let key = attested_meta_key(binding, conn, "error")?;
    let sanitized: String =
        detail.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).take(512).collect();
    rag_rat_db::meta::set_meta(conn, &key, sanitized.trim())
}

/// Clear a binding's persisted attested-walk failure — a clean completed attested walk.
pub(crate) fn clear_attested_error(
    conn: &Connection,
    binding: &ResolvedTracker,
) -> anyhow::Result<()> {
    let key = attested_meta_key(binding, conn, "error")?;
    rag_rat_db::meta::delete_meta(conn, &key)
}

/// Read a binding's persisted attested-walk failure, for the status snapshot.
pub(crate) fn read_attested_error(
    conn: &Connection,
    binding: &ResolvedTracker,
) -> anyhow::Result<Option<String>> {
    let key = attested_meta_key(binding, conn, "error")?;
    rag_rat_db::meta::read_meta(conn, &key)
}

/// Clear a binding's attested-closers `since` watermark. The INVARIANT: EVERY seam that forces a
/// full item re-walk (a WIDENED filter, or a full rewalk) must call this. Such a re-walk re-caches
/// closed issues whose provider-attested closers may PREDATE the stored `since`; a reused watermark
/// would stop the attested walk before revisiting them, so those issues would silently never regain
/// their provider edges. Clearing forces the next attested walk to re-scan from the top — its
/// upserts and per-closer replace-sets make the redo idempotent. Two seams reset the item backfill
/// (`filter_changed` and `reset_for_full_rewalk`); both route through here rather than each
/// inlining the delete, so a third reset seam can't forget it.
fn clear_attested_watermark(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<()> {
    let key = attested_since_key(binding, conn)?;
    rag_rat_db::meta::delete_meta(conn, &key)
}

/// The provider-attested closers walk (#702 stage 2): pages `attested_closers_page` until the
/// walk completes or falls behind the last completed walk's watermark, storing every attested
/// edge (the upsert's trust ladder upgrades text-tier rows in place) and applying per-item
/// outcome updates to CACHED rows. The watermark advances ONLY on a COMPLETED walk — an
/// interrupted run redoes from the top next time (idempotent upserts, no cursor sub-state).
async fn sync_attested_closers<C: PapertrailClient>(
    conn: &Connection,
    binding: &ResolvedTracker,
    client: &C,
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let key = attested_since_key(binding, conn)?;
    let since = rag_rat_db::meta::read_meta(conn, &key)?;
    let mut cursor: Option<String> = None;
    let mut frontier: Option<String> = None;
    let mut pages_seen = 0usize;
    loop {
        let Some(page) = client
            .attested_closers_page(&binding.project, cursor.as_deref(), since.as_deref())
            .await?
        else {
            // `None` on the FIRST page = no attested supply (provider without one, or the
            // capability probe failed on the opening call): the text tier is the only local
            // evidence and stage-2 storage is a clean no-op. `None` AFTER pages were stored is a
            // mid-walk capability trip (a transient probe-shaped failure): the walk is PARTIAL,
            // so surface it — the watermark stays put and the next sync redoes from the top.
            if pages_seen > 0 {
                report.attested_error = Some(
                    "attested-closers walk ended early: capability unavailable mid-walk".into(),
                );
            }
            return Ok(());
        };
        pages_seen += 1;
        if let Some(page_frontier) = &page.frontier {
            // Conservative MINIMUM across phases (ISO timestamps compare lexicographically):
            // neither stream's later updates can be skipped by the other's newer frontier.
            frontier = match frontier.take() {
                Some(existing) if existing <= *page_frontier => Some(existing),
                _ => Some(page_frontier.clone()),
            };
        }
        let tx = conn.unchecked_transaction()?;
        // ISSUE-KEYED REPLACE-SET: an issue has exactly one authoritative closer (its last
        // ClosedEvent), so re-reading it lets the walk reap EVERY provider closer edge targeting
        // it — any kind — before the current closer is reinserted below. A stale or changed
        // closer (reopened-then-reclosed by a different PR/commit) dies with the refresh. Reaping
        // is deliberately NOT closer-keyed (per-PR): a PR closes many issues, so deleting a PR's
        // outgoing edges would clobber UI-linked rows created from another issue's ClosedEvent
        // that the PR's `closingIssuesReferences` never lists. The PR phase only CREATES keyword
        // edges (idempotent upserts); it never reaps.
        for issue_key in &page.replaced_issue_closers {
            report.attested_writes += tx.execute(
                "DELETE FROM papertrail_closing_edges WHERE repo_id = ?1 AND tracker = ?2 AND \
                 project = ?3 AND source = 'provider' AND issue_key = ?4",
                params![repo_id, binding.provider.as_db_str(), binding.project, issue_key],
            )?;
        }
        for edge in &page.edges {
            // Store an attested edge ONLY when its target issue is a cached item that is NOT
            // open. Cached ⇒ the item passed this binding's tag filter (the item walk prunes
            // out-of-scope items), so a `tags = ["bug"]` binding never records closures for
            // untracked issues. Not-open ⇒ no closure evidence for a reopened issue (the API
            // may still list a merged PR's `closingIssuesReferences` for an issue that was
            // reopened). An un-mirrored or reopened target is skipped; a later closed+in-scope
            // walk records it. (`edge.project` is the issue's project — same-project after the
            // cross-repo skips, i.e. `binding.project`.)
            let target_closed = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM papertrail_items WHERE repo_id = ?1 AND tracker = ?2 \
                 AND project = ?3 AND item_kind = 'issue' AND item_key = ?4 AND state_normalized \
                 != 'open')",
                params![repo_id, binding.provider.as_db_str(), edge.project, edge.issue_key],
                |row| row.get::<_, bool>(0),
            )?;
            if !target_closed {
                continue;
            }
            // Defer to the issue's ONE authoritative closer: never store an edge that CONFLICTS
            // with a provider closer already recorded for this issue (a different closer). The
            // issue phase reaps all of a re-read issue's provider edges earlier in THIS tx, so its
            // own fresh closer never conflicts; the PR phase does NOT reap, so without this a PR
            // edited after the watermark would resurrect `#5←#9` once #5's ClosedEvent had already
            // moved its closer elsewhere (and #5 sits below the watermark, never re-read). The
            // same-closer case is not a conflict, so an idempotent re-store still passes.
            let conflicting_closer = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM papertrail_closing_edges WHERE repo_id = ?1 AND \
                 tracker = ?2 AND project = ?3 AND issue_kind = ?4 AND issue_key = ?5 AND source \
                 = 'provider' AND NOT (closer_kind = ?6 AND closer_key = ?7))",
                params![
                    repo_id,
                    binding.provider.as_db_str(),
                    edge.project,
                    edge.issue_kind.as_db_str(),
                    edge.issue_key,
                    edge.closer_kind.as_db_str(),
                    edge.closer_key,
                ],
                |row| row.get::<_, bool>(0),
            )?;
            if conflicting_closer {
                continue;
            }
            store_closing_edge(&tx, binding.provider, edge)?;
            report.attested_edges += 1;
        }
        for update in &page.item_updates {
            if let Some(resolution) = update.resolution {
                report.attested_writes += tx.execute(
                    "UPDATE papertrail_items SET resolution = ?6 WHERE repo_id = ?1 AND tracker = \
                     ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5",
                    params![
                        repo_id,
                        binding.provider.as_db_str(),
                        binding.project,
                        update.item_kind.as_db_str(),
                        update.item_key,
                        resolution.as_db_str(),
                    ],
                )?;
            }
            if let Some(sha) = &update.merge_commit_sha {
                // The merged-only invariant, enforced in SQL: an attested sha lands only on rows
                // the store already normalized as merged.
                report.attested_writes += tx.execute(
                    "UPDATE papertrail_items SET merge_commit_sha = ?6 WHERE repo_id = ?1 AND \
                     tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5 AND \
                     state_normalized = 'merged'",
                    params![
                        repo_id,
                        binding.provider.as_db_str(),
                        binding.project,
                        update.item_kind.as_db_str(),
                        update.item_key,
                        sha,
                    ],
                )?;
            }
        }
        tx.commit()?;
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    if let Some(frontier) = frontier {
        rag_rat_db::meta::set_meta(conn, &key, &frontier)?;
    }
    Ok(())
}

fn prune_unmatched(conn: &Connection, binding: &ResolvedTracker) -> anyhow::Result<usize> {
    if binding.tags.is_empty() {
        return Ok(0);
    }
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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

/// Test-only surface over [`delete_item`] for the sibling module's prune-cleanup tests.
#[cfg(test)]
pub(crate) fn delete_item_for_tests(
    conn: &Connection,
    binding: &ResolvedTracker,
    kind: ItemKind,
    key: &str,
) -> anyhow::Result<bool> {
    delete_item(conn, binding, kind, key)
}

/// Escape `LIKE` wildcards in an identity segment (paired with `ESCAPE '\\'`).
fn like_escape(segment: &str) -> String {
    segment.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn delete_item(
    conn: &Connection,
    binding: &ResolvedTracker,
    kind: ItemKind,
    key: &str,
) -> anyhow::Result<bool> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
    // #702: mined evidence dies with its source. The mined identities are constructible in SQL
    // (item: `project:kind:key`; comment: that + `:comment_id` — a prefix match), so the pruned
    // item's own mined refs AND every pruned comment's mined refs go in one pass; a pruned
    // change request also drops the text-tier closing edges it minted (provider-attested edges
    // outlive their text sources by design).
    conn.execute(
        "DELETE FROM papertrail_refs WHERE repo_id=?1 AND source_kind='item' AND source_text = ?2 \
         || ':' || ?3 || ':' || ?4 || ':' || ?5",
        params![repo_id, binding.provider.as_db_str(), binding.project, kind.as_db_str(), key],
    )?;
    // `_`/`%` are LIKE wildcards and provider project strings can contain `_` — escape the
    // identity so `foo_bar/repo` never matches `fooxbar/repo`'s mined comment rows.
    let like_prefix = format!(
        "{}:{}:{}:{}:%",
        like_escape(binding.provider.as_db_str()),
        like_escape(&binding.project),
        like_escape(kind.as_db_str()),
        like_escape(key)
    );
    conn.execute(
        "DELETE FROM papertrail_refs WHERE repo_id=?1 AND source_kind='comment' AND source_text \
         LIKE ?2 ESCAPE '\\'",
        params![repo_id, like_prefix],
    )?;

    conn.execute(
        "DELETE FROM papertrail_item_tags WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND \
         item_kind=?4 AND item_key=?5",
        params![repo_id, binding.provider.as_db_str(), binding.project, kind.as_db_str(), key],
    )?;
    // An issue leaving the cache (pruned out of scope, or deleted) takes its closing edges with
    // it — of BOTH tiers. The attested walk stores an edge only for a cached issue, so the cache
    // is the source of truth: no cached issue ⇒ no closing edges targeting it. Without this, a
    // narrowed tag filter prunes the issue row but strands its provider closer, and the attested
    // watermark won't revisit the closer to clean it (#727 review).
    if kind == ItemKind::Issue {
        conn.execute(
            "DELETE FROM papertrail_closing_edges WHERE repo_id=?1 AND tracker=?2 AND project=?3 \
             AND issue_kind=?4 AND issue_key=?5",
            params![repo_id, binding.provider.as_db_str(), binding.project, kind.as_db_str(), key],
        )?;
    }
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
        // #702: a pruned comment takes its mined refs with it (exact identity — the source will
        // never be re-mined to replace the set once its row is gone).
        conn.execute(
            "DELETE FROM papertrail_refs WHERE repo_id=?1 AND source_kind='comment' AND \
             source_text = ?2 || ':' || ?3 || ':' || ?4 || ':' || ?5 || ':' || ?6",
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
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
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
    trackers: &[ResolvedTracker],
    comments: &[PapertrailComment],
    report: &mut MirrorBindingReport,
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let fallback = item_numbering_is_shared(binding.provider);
    let tx = conn.unchecked_transaction()?;
    for comment in comments {
        // Resolve the parent item, PREFERRING the kind the provider put on the comment: under
        // namespaced numbering (GitLab) issue #N and change request !N coexist on one key, and
        // a key-only lookup would hitch the comment to whichever twin the scan returns first —
        // then rewrite the correctly-kinded row through the kind-less comment conflict key.
        // Falling back to the other kind is ONLY for providers whose feed cannot name the kind
        // (GitHub's issue-comment stream spans issues and pull requests) — there the key alone
        // IS unique. A namespaced provider names the kind authoritatively, so a missing
        // exact-kind parent (e.g. a merge request pruned by the tag filter while issue #N is
        // cached) means SKIP, never contaminate the twin namespace's evidence.
        let kind = tx
            .prepare_cached(
                "SELECT item_kind FROM papertrail_items WHERE repo_id=?1 AND tracker=?2 AND \
                 project=?3 AND item_key=?4 AND (item_kind = ?5 OR ?6) ORDER BY (item_kind = ?5) \
                 DESC LIMIT 1",
            )?
            .query_row(
                params![
                    repo_id,
                    binding.provider.as_db_str(),
                    binding.project,
                    comment.item_key,
                    comment.item_kind.as_db_str(),
                    fallback
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(kind) = kind else { continue };
        if kind == comment.item_kind.as_db_str() {
            store_comment(&tx, binding.provider, comment)?;
            sync::mine_comment_refs(&tx, binding.provider, trackers, comment)?;
        } else {
            let mut comment = comment.clone();
            comment.item_kind = ItemKind::from_db_str(&kind)?;
            store_comment(&tx, binding.provider, &comment)?;
            sync::mine_comment_refs(&tx, binding.provider, trackers, &comment)?;
        }
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

pub(crate) fn max_timestamp(left: Option<String>, right: Option<String>) -> Option<String> {
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

pub(crate) fn parse_date(value: &str) -> Option<(i32, u32, u32)> {
    let mut parts = value.split('-');
    let parsed =
        (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    parts.next().is_none().then_some(parsed)
}

fn parse_time(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    // Fractional seconds (GitLab emits millisecond stamps) truncate: the rewound overlap
    // timestamp compares lexicographically BELOW any fractional variant of the same second, so
    // truncation only widens the replay window. Refusing to parse them instead silently
    // returned the input unchanged — a ZERO overlap — and with a strict updated_after filter
    // the boundary row became unreachable, so the replay convergence check could never pass.
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.split('.').next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

pub(crate) fn days_in_month(year: i32, month: u32) -> u32 {
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

    use rag_rat_db::schema;

    use super::*;

    struct ScriptClient {
        comment_streams: &'static [&'static str],
        attested: RefCell<VecDeque<Option<AttestedClosersPage>>>,
        attested_pause: std::cell::Cell<bool>,
        attested_hard_error: std::cell::Cell<bool>,
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
                attested: RefCell::new(VecDeque::new()),
                attested_pause: std::cell::Cell::new(false),
                attested_hard_error: std::cell::Cell::new(false),
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
            self.repo_comments.borrow_mut().push_back(Ok(CommentsPage {
                comments,
                next: None,
                frontier: None,
            }));
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
                frontier: None,
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
            self.repo_comments.borrow_mut().pop_front().unwrap_or(Ok(CommentsPage {
                comments: Vec::new(),
                next: None,
                frontier: None,
            }))
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

        async fn attested_closers_page(
            &self,
            _project: &str,
            _cursor: Option<&str>,
            _since: Option<&str>,
        ) -> anyhow::Result<Option<AttestedClosersPage>> {
            if self.attested_pause.get() {
                return Err(anyhow::Error::new(TransportError::Paused {
                    resume_at_ms: 999_000,
                    reason: PauseReason::RetryAfter,
                }));
            }
            if self.attested_hard_error.get() {
                anyhow::bail!("attested walk boom");
            }
            Ok(self.attested.borrow_mut().pop_front().unwrap_or(None))
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
            closed_at: None,
            resolution: None,
            merge_commit_sha: None,
            author_kind: None,
            author_association: None,
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
            author_kind: None,
            author_association: None,
            created_at: Some(updated_at.to_string()),
            updated_at: Some(updated_at.to_string()),
            review_state: None,
            anchor_path: None,
        }
    }

    fn empty_report(binding: &ResolvedTracker) -> MirrorBindingReport {
        MirrorBindingReport {
            tracker: binding.provider,
            project: binding.project.clone(),
            stored_items: 0,
            stored_comments: 0,
            pruned_items: 0,
            attested_edges: 0,
            attested_writes: 0,
            attested_error: None,
            paused_until_ms: None,
            pause_reason: None,
            completed_full_walk: false,
            probe_not_modified: false,
        }
    }

    /// Cache a CLOSED in-scope issue so the attested-edge cached-closed gate admits its edges.
    fn cache_closed_issue(conn: &Connection, key: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO papertrail_items(tracker, project, item_kind, item_key, url, \
             state, title, body, synced_at_ms, repo_id, state_normalized) VALUES ('github', \
             'o/r', 'issue', ?1, 'u', 'closed', 't', 'b', 1, (SELECT COALESCE((SELECT repo_id \
             FROM repos LIMIT 1), '__unassigned__')), 'closed')",
            [key],
        )
        .unwrap();
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    fn keys(conn: &Connection) -> Vec<String> {
        let mut stmt =
            conn.prepare("SELECT item_key FROM papertrail_items ORDER BY item_key").unwrap();
        stmt.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect()
    }

    /// Namespaced numbering (GitLab): issue #N and change request !N share a key. The repo
    /// comment lane must resolve the parent by the comment's OWN kind first — a key-only lookup
    /// hitches the comment to whichever twin the scan returns first and then rewrites the
    /// correctly-kinded row through the kind-less comment conflict key.
    /// GitLab emits millisecond timestamps; the overlap rewind must handle the fraction —
    /// refusing to parse it silently returned the input unchanged (ZERO overlap), and with a
    /// strict updated_after filter the boundary row became unreachable, so replay convergence
    /// could never complete.
    #[test]
    fn overlap_timestamp_rewinds_fractional_second_stamps() {
        assert_eq!(overlap_timestamp("2026-07-15T13:15:56.837Z"), "2026-07-15T13:15:55Z");
        assert_eq!(overlap_timestamp("2026-01-01T00:00:00.001Z"), "2025-12-31T23:59:59Z");
        // Non-fractional stamps keep their existing behavior.
        assert_eq!(overlap_timestamp("2026-07-15T13:15:56Z"), "2026-07-15T13:15:55Z");
    }

    /// A quiet probe must not starve an OWED boundary replay: providers without a probe
    /// validator (GitLab) report a timestamp tie as not_modified, and the conservative frontier
    /// left by an interrupted delta would otherwise wait for the daily full walk.
    #[test]
    fn a_quiet_probe_never_starves_an_owed_replay() {
        let conn = db();
        let binding = binding(&[]);
        let first_walk = ScriptClient::new(vec![
            Ok(ItemsPage {
                items: vec![item("1", "2026-01-02T00:00:00Z", "one", &[])],
                next: None,
                backfill_boundary: None,
            }),
            Ok(ItemsPage { items: Vec::new(), next: None, backfill_boundary: None }),
        ])
        .with_repo_comments(Vec::new());
        block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &first_walk,
            false,
        ))
        .unwrap();

        let mut cursor = load_cursor(&conn, &binding).unwrap();
        cursor.item_delta_replay_required = true;
        save_cursor(&conn, &binding, &cursor, false).unwrap();

        let quiet = ScriptClient::new(vec![Ok(ItemsPage {
            items: Vec::new(),
            next: None,
            backfill_boundary: None,
        })])
        .with_probe(FreshnessResult { latest: None, etag: None, not_modified: true })
        .with_repo_comments(Vec::new());
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &quiet, false))
            .unwrap();

        let requests = quiet.item_page_requests.borrow();
        assert_eq!(requests.len(), 1, "the owed replay must run despite the quiet probe");
        assert!(requests[0].updated_since.is_some(), "the replay is a delta scan");
    }

    /// A filter change must RESET the attested-closers watermark: a widened filter caches
    /// newly-in-scope closed issues whose PRs/issues predate the stored `since`, and a reused
    /// watermark would stop the attested walk before ever visiting them. An unchanged filter must
    /// leave the watermark intact so the incremental walk stays incremental (#727 review).
    #[test]
    fn a_filter_change_resets_the_attested_watermark_but_a_stable_filter_keeps_it() {
        let conn = db();
        let binding = binding(&[]);
        let key = attested_since_key(&binding, &conn).unwrap();

        // Persist a cursor whose stored fingerprint will NOT match the binding's, forcing the
        // next sync onto the filter-changed path.
        let mut cursor = load_cursor(&conn, &binding).unwrap();
        cursor.filter_fingerprint = "stale-fingerprint".to_string();
        save_cursor(&conn, &binding, &cursor, false).unwrap();
        rag_rat_db::meta::set_meta(&conn, &key, "2020-01-01T00:00:00Z").unwrap();

        let changed = ScriptClient::new(vec![page(Vec::new()), page(Vec::new())])
            .with_repo_comments(Vec::new());
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &changed, false))
            .unwrap();
        assert!(
            rag_rat_db::meta::read_meta(&conn, &key).unwrap().is_none(),
            "the widened-filter path clears the attested watermark",
        );

        // The prior run stored the binding's own fingerprint, so a repeat sync sees no change:
        // the freshly re-seeded watermark must survive.
        rag_rat_db::meta::set_meta(&conn, &key, "2021-01-01T00:00:00Z").unwrap();
        let stable = ScriptClient::new(vec![page(Vec::new()), page(Vec::new())])
            .with_probe(FreshnessResult { latest: None, etag: None, not_modified: true })
            .with_repo_comments(Vec::new());
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &stable, false))
            .unwrap();
        assert_eq!(
            rag_rat_db::meta::read_meta(&conn, &key).unwrap().as_deref(),
            Some("2021-01-01T00:00:00Z"),
            "a stable filter leaves the attested watermark untouched",
        );
    }

    /// A FULL rewalk re-caches every closed issue, so it must clear the attested watermark for the
    /// same reason a widened filter does — otherwise the attested walk reads a stale `since` and
    /// silently never re-fetches provider closers for issues whose closer predates it. This is the
    /// full-rewalk seam in isolation: the filter is unchanged, so ONLY `reset_for_full_rewalk` can
    /// do the clearing.
    #[test]
    fn a_full_rewalk_clears_the_attested_watermark_even_with_an_unchanged_filter() {
        let conn = db();
        let binding = binding(&[]);
        let key = attested_since_key(&binding, &conn).unwrap();
        rag_rat_db::meta::set_meta(&conn, &key, "2026-01-01T00:00:00Z").unwrap();

        // full=true on a fresh cursor ⇒ starting_full_rewalk; tags=[] matches the stored empty
        // fingerprint ⇒ filter_changed=false, so the filter-change clear cannot fire.
        let full = ScriptClient::new(vec![page(Vec::new()), page(Vec::new())])
            .with_repo_comments(Vec::new());
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &full, true))
            .unwrap();

        assert!(
            rag_rat_db::meta::read_meta(&conn, &key).unwrap().is_none(),
            "reset_for_full_rewalk must clear the attested watermark",
        );
    }

    /// A comments page whose entries all map to no comment (GitLab events on commit/snippet
    /// notes) must still advance the stream through its `frontier`, or the scan replays the
    /// same pages on every sync forever.
    #[test]
    fn a_page_of_only_skipped_comment_events_still_advances_the_stream() {
        let conn = db();
        let binding = binding(&[]);
        let first_walk = ScriptClient::new(vec![
            Ok(ItemsPage {
                items: vec![item("1", "2026-01-02T00:00:00Z", "one", &[])],
                next: None,
                backfill_boundary: None,
            }),
            Ok(ItemsPage { items: Vec::new(), next: None, backfill_boundary: None }),
        ])
        .with_repo_comments(Vec::new());
        block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &first_walk,
            false,
        ))
        .unwrap();

        let quiet = ScriptClient::new(vec![])
            .with_probe(FreshnessResult { latest: None, etag: None, not_modified: true })
            .with_repo_comment_pages(vec![Ok(CommentsPage {
                comments: Vec::new(),
                next: None,
                frontier: Some("2026-02-01T00:00:00Z".to_string()),
            })]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &quiet, false))
            .unwrap();

        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(
            cursor.comment_stream_cursors.get("default").and_then(|s| s.high_mark_at.as_deref()),
            Some("2026-02-01T00:00:00Z"),
            "the frontier advances the durable stream mark even with zero returned comments"
        );
    }

    /// A drained multi-page comment window must advance past its LAST page's frontier: with a
    /// date-granular provider filter (GitLab events `after`), a first-page-only frontier keeps
    /// re-opening the same busy day and replays every later page on every poll, forever.
    #[test]
    fn a_drained_multi_page_window_advances_past_its_last_frontier() {
        let conn = db();
        let binding = binding(&[]);
        let first_walk = ScriptClient::new(vec![
            Ok(ItemsPage {
                items: vec![item("1", "2026-01-02T00:00:00Z", "one", &[])],
                next: None,
                backfill_boundary: None,
            }),
            Ok(ItemsPage { items: Vec::new(), next: None, backfill_boundary: None }),
        ])
        .with_repo_comments(Vec::new());
        block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &first_walk,
            false,
        ))
        .unwrap();

        let quiet = ScriptClient::new(vec![])
            .with_probe(FreshnessResult { latest: None, etag: None, not_modified: true })
            .with_repo_comment_pages(vec![
                Ok(CommentsPage {
                    comments: vec![comment("1", "early", "2026-02-01T08:00:00Z")],
                    next: Some(PageCursor {
                        page_token: Some("events-page-2".to_string()),
                        ..PageCursor::default()
                    }),
                    frontier: Some("2026-02-01T08:00:00Z".to_string()),
                }),
                Ok(CommentsPage {
                    comments: Vec::new(),
                    next: None,
                    frontier: Some("2026-02-01T20:00:00Z".to_string()),
                }),
            ]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &quiet, false))
            .unwrap();

        let cursor = load_cursor(&conn, &binding).unwrap();
        assert_eq!(
            cursor.comment_stream_cursors.get("default").and_then(|s| s.high_mark_at.as_deref()),
            Some("2026-02-01T20:00:00Z"),
            "the stream must clear the drained window, not pin to the first page's maximum"
        );
    }

    /// Namespaced providers name the comment's kind authoritatively: a missing exact-kind
    /// parent (a merge request pruned by the tag filter while issue #N is cached) means SKIP —
    /// never attach the comment across namespaces.
    #[test]
    fn namespaced_comments_never_fall_back_across_namespaces() {
        let conn = db();
        let mut binding = binding(&[]);
        binding.provider = Tracker::Gitlab;
        binding.project = "g/r".to_string();
        let mut issue = item("7", "2026-01-02T00:00:00Z", "issue seven", &[]);
        issue.project = "g/r".to_string();
        store_item(&conn, binding.provider, &issue).unwrap();

        let mut report = empty_report(&binding);
        let mut change_note = comment("7", "note:9", "2026-01-04T00:00:00Z");
        change_note.project = "g/r".to_string();
        change_note.item_kind = ItemKind::ChangeRequest;
        let mut issue_note = comment("7", "note:10", "2026-01-04T00:00:00Z");
        issue_note.project = "g/r".to_string();
        store_repo_comments(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &[change_note, issue_note],
            &mut report,
        )
        .unwrap();

        let rows: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT comment_id, item_kind FROM papertrail_comments ORDER BY comment_id",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(
            rows,
            vec![("note:10".to_string(), "issue".to_string())],
            "the merge-request note must be skipped, not attached to the issue"
        );
        assert_eq!(report.stored_comments, 1);
    }

    #[test]
    fn repo_comments_resolve_namespaced_twins_by_their_own_kind() {
        let conn = db();
        let mut binding = binding(&[]);
        binding.provider = Tracker::Gitlab;
        binding.project = "g/r".to_string();
        let mut issue = item("1", "2026-01-02T00:00:00Z", "issue one", &[]);
        issue.project = "g/r".to_string();
        let mut change = item("1", "2026-01-03T00:00:00Z", "mr one", &[]);
        change.project = "g/r".to_string();
        change.item_kind = ItemKind::ChangeRequest;
        store_item(&conn, binding.provider, &issue).unwrap();
        store_item(&conn, binding.provider, &change).unwrap();

        let mut report = empty_report(&binding);
        let mut issue_note = comment("1", "note:1", "2026-01-04T00:00:00Z");
        issue_note.project = "g/r".to_string();
        let mut change_note = comment("1", "note:2", "2026-01-04T00:00:00Z");
        change_note.project = "g/r".to_string();
        change_note.item_kind = ItemKind::ChangeRequest;
        store_repo_comments(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &[issue_note, change_note],
            &mut report,
        )
        .unwrap();

        let kinds: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT comment_id, item_kind FROM papertrail_comments ORDER BY comment_id",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(kinds, vec![
            ("note:1".to_string(), "issue".to_string()),
            ("note:2".to_string(), "change_request".to_string()),
        ]);
    }

    /// The fallback half of the twin resolution: a provider whose feed cannot name the kind
    /// (GitHub's issue-comment stream spans issues and pull requests) still resolves through the
    /// key alone when no item of the claimed kind exists.
    #[test]
    fn repo_comments_fall_back_to_the_key_when_the_claimed_kind_has_no_item() {
        let conn = db();
        let binding = binding(&[]);
        let mut pull = item("7", "2026-01-02T00:00:00Z", "pull seven", &[]);
        pull.item_kind = ItemKind::ChangeRequest;
        store_item(&conn, binding.provider, &pull).unwrap();

        let mut report = empty_report(&binding);
        // The GitHub feed guesses Issue; only the pull exists.
        store_repo_comments(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &[comment("7", "c1", "2026-01-04T00:00:00Z")],
            &mut report,
        )
        .unwrap();
        let kind: String = conn
            .query_row(
                "SELECT item_kind FROM papertrail_comments WHERE comment_id='c1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kind, "change_request");
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
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &first,
            false,
        ))
        .unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert_eq!(keys(&conn), vec!["2", "3"]);

        let second = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-01T00:00:00Z", "one", &[])]),
            page(Vec::new()),
        ]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &second, false))
            .unwrap();
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
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &first,
            false,
        ))
        .unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert_eq!(keys(&conn), vec!["1", "2"]);

        let resumed = ScriptClient::new(vec![page(same_page), page(Vec::new())])
            .with_item_comments(vec![comment("1", "one-comment", "2026-01-01T01:00:00Z")]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
            .unwrap();
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
                    frontier: None,
                }),
                Err(anyhow::Error::new(TransportError::Paused {
                    resume_at_ms: 42,
                    reason: PauseReason::PassBudget,
                })),
            ]);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &first,
            false,
        ))
        .unwrap();
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
                    frontier: None,
                }),
                Ok(CommentsPage {
                    comments: vec![comment("1", "second", "2026-01-01T02:00:00Z")],
                    next: None,
                    frontier: None,
                }),
                // The confirming walk is identical, so absence of the deleted first comment is
                // now safe to apply destructively.
                Ok(CommentsPage {
                    comments: vec![comment("1", "second", "2026-01-01T02:00:00Z")],
                    next: None,
                    frontier: None,
                }),
            ]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
            .unwrap();
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
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &first,
            false,
        ))
        .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();

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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
            .unwrap();

        let changed_page = vec![
            item("2", "2026-01-03T00:00:00Z", "new", &[]),
            item("1", "2026-01-01T00:00:00Z", "one", &[]),
        ];
        let resumed = ScriptClient::new(vec![page(changed_page), page(Vec::new())])
            .with_item_comment_results(vec![
                Ok(Vec::new()),
                Ok(vec![comment("2", "new-comment", "2026-01-03T01:00:00Z")]),
            ]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
            .unwrap();

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
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &first,
            false,
        ))
        .unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert_eq!(keys(&conn), vec!["1"]);

        let resumed = ScriptClient::new(vec![
            page(vec![item("1", "2026-01-02T00:00:00Z", "changed", &["feature"])]),
            page(Vec::new()),
        ]);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &resumed,
            false,
        ))
        .unwrap();
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
            std::slice::from_ref(&binding),
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &later, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &second, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &tied, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &paused,
            false,
        ))
        .unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(cursor.item_delta_in_progress);
        assert_eq!(cursor.item_delta_page_token.as_deref(), Some("delta-page-2"));

        let resumed =
            ScriptClient::new(vec![page(vec![item("3", "2026-01-03T00:00:00Z", "three", &[])])]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &delta, false))
            .unwrap();

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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &replay, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

        let raced = ScriptClient::new(vec![page(Vec::new())]).with_probe(FreshnessResult {
            latest: Some("2026-01-05T00:00:00Z".to_string()),
            etag: Some("v2".to_string()),
            not_modified: false,
        });
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &raced, false))
            .unwrap();

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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &first, false))
            .unwrap();
        let cursor = load_cursor(&conn, &binding).unwrap();
        assert!(cursor.item_delta_replay_required);
        assert!(cursor.probe_etag.is_none());

        let replay =
            ScriptClient::new(vec![tied_page(), page(Vec::new())]).with_probe(FreshnessResult {
                latest: None,
                etag: Some("v2".to_string()),
                not_modified: false,
            });
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &replay, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &bug, std::slice::from_ref(&bug), &initial, false)).unwrap();
        assert_eq!(keys(&conn), vec!["1"]);

        let docs = binding(&["docs"]);
        let changed = ScriptClient::new(vec![
            page(vec![item("2", "2026-01-03T00:00:00Z", "docs", &["docs"])]),
            page(Vec::new()),
        ]);
        let report =
            block_on(mirror_binding(&conn, &docs, std::slice::from_ref(&docs), &changed, false))
                .unwrap();
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
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &client,
            true,
        ))
        .unwrap();
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
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &paused,
            true,
        ))
        .unwrap();
        assert_eq!(report.paused_until_ms, Some(42));
        assert!(load_cursor(&conn, &binding).unwrap().full_rewalk);
        assert_eq!(keys(&conn), vec!["1", "2"]);

        let resumed = ScriptClient::new(vec![page(Vec::new())]);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &resumed,
            false,
        ))
        .unwrap();
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
        let error = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &client,
            false,
        ))
        .unwrap_err();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

        let failed =
            ScriptClient::new(vec![page(vec![item("1", "2026-01-02T00:00:00Z", "updated", &[])])])
                .with_probe(FreshnessResult {
                    latest: Some("2026-01-02T00:00:00Z".to_string()),
                    etag: Some("v2".to_string()),
                    not_modified: false,
                })
                .with_item_comment_results(vec![Err(anyhow::anyhow!("comment fetch failed"))]);
        let error = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &failed,
            false,
        ))
        .unwrap_err();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
        let error = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &invalid,
            false,
        ))
        .unwrap_err();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
                frontier: None,
            })]);
        let error = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &invalid,
            false,
        ))
        .unwrap_err();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &delta,
            false,
        ))
        .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
                    frontier: None,
                }),
                Err(anyhow::Error::new(TransportError::Paused {
                    resume_at_ms: 42,
                    reason: PauseReason::PassBudget,
                })),
            ]);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &paused,
            false,
        ))
        .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &resumed, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
                    frontier: None,
                }),
                Ok(CommentsPage {
                    comments: vec![comment("1", "later", "2026-01-04T00:00:00Z")],
                    next: None,
                    frontier: None,
                }),
            ]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &delta, false))
            .unwrap();

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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

        let missing =
            ScriptClient::new(vec![page(vec![item("1", "2026-01-02T00:00:00Z", "updated", &[])])])
                .with_probe(FreshnessResult {
                    latest: Some("2026-01-02T00:00:00Z".to_string()),
                    etag: Some("v2".to_string()),
                    not_modified: false,
                })
                .with_item_comment_results(vec![Err(PapertrailClientError::ItemNotFound.into())]);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &missing,
            false,
        ))
        .unwrap();

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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

        let next =
            PageCursor { page_token: Some("comments-page-2".to_string()), ..Default::default() };
        let delta = ScriptClient::new(Vec::new())
            .with_probe(FreshnessResult {
                latest: None,
                etag: Some("v1".to_string()),
                not_modified: true,
            })
            .with_repo_comment_pages(vec![
                Ok(CommentsPage { comments: Vec::new(), next: Some(next), frontier: None }),
                Ok(CommentsPage { comments: Vec::new(), next: None, frontier: None }),
            ]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &delta, false))
            .unwrap();
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
                frontier: None,
            }),
            Ok(CommentsPage {
                comments: vec![comment("1", "review", "2026-01-03T00:00:00Z")],
                next: None,
                frontier: None,
            }),
        ]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &initial, false))
            .unwrap();

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
                    frontier: None,
                }),
                Ok(CommentsPage { comments: Vec::new(), next: None, frontier: None }),
            ]);
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &next, false))
            .unwrap();
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
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
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
        let report =
            block_on(mirror_binding(&conn, &bugs, std::slice::from_ref(&bugs), &client, false))
                .unwrap();
        assert!(report.pruned_items >= 1);
        assert!(keys(&conn).is_empty());
    }
    #[test]
    fn attested_closers_walk_stores_upgrades_and_watermarks() {
        let conn = db();
        let binding = binding(&[]);
        // A pre-existing TEXT-tier edge for the same pair: the provider walk must upgrade it.
        crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: crate::CloserKind::ChangeRequest,
            closer_key: "9".into(),
            closer_commit: None,
            source: crate::ClosingEdgeSource::Text,
        })
        .unwrap();
        cache_closed_issue(&conn, "5");
        let client = ScriptClient::new(vec![page(Vec::new())]);
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            edges: vec![crate::ClosingEdge {
                project: "o/r".into(),
                issue_kind: ItemKind::Issue,
                issue_key: "5".into(),
                closer_kind: crate::CloserKind::ChangeRequest,
                closer_key: "9".into(),
                closer_commit: Some("abc123".into()),
                source: crate::ClosingEdgeSource::Provider,
            }],
            item_updates: Vec::new(),
            replaced_issue_closers: Vec::new(),
            next: Some("issues".into()),
            frontier: Some("2026-01-05T00:00:00Z".into()),
        }));
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage::default()));
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &client,
            false,
        ))
        .unwrap();
        assert_eq!(report.attested_edges, 1);
        let edges = crate::store::closing_edges_for_item(
            &conn,
            Tracker::Github,
            "o/r",
            ItemKind::Issue,
            "5",
        )
        .unwrap();
        assert_eq!(edges.len(), 1, "text and provider tiers converge on the pair");
        assert_eq!(edges[0].source, crate::ClosingEdgeSource::Provider);
        assert_eq!(edges[0].closer_commit.as_deref(), Some("abc123"));
        // The COMPLETED walk stamped its watermark.
        let repo_id = rag_rat_db::schema::active_repo_id(&conn).unwrap();
        let since = rag_rat_db::meta::read_meta(
            &conn,
            &format!("papertrail_attested_closers_since:{repo_id}:github:o/r"),
        )
        .unwrap();
        assert_eq!(since.as_deref(), Some("2026-01-05T00:00:00Z"));
    }

    #[test]
    fn attested_item_updates_touch_only_cached_merged_rows() {
        let conn = db();
        let binding = binding(&[]);
        // A cached CLOSED-UNMERGED change request: the attested sha must NOT land on it.
        let mut unmerged = item("9", "2026-01-01T00:00:00Z", "t", &[]);
        unmerged.item_kind = ItemKind::ChangeRequest;
        unmerged.state = "closed".into();
        crate::store::store_item(&conn, Tracker::Github, &unmerged).unwrap();
        let client = ScriptClient::new(vec![page(Vec::new())]);
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            edges: Vec::new(),
            item_updates: vec![crate::AttestedItemUpdate {
                item_kind: ItemKind::ChangeRequest,
                item_key: "9".into(),
                resolution: None,
                merge_commit_sha: Some("attested".into()),
            }],
            replaced_issue_closers: Vec::new(),
            next: None,
            frontier: None,
        }));
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
        let sha: Option<String> = conn
            .query_row(
                "SELECT merge_commit_sha FROM papertrail_items WHERE item_key = '9'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sha, None, "merged-only invariant holds against attested updates too");
    }
    #[test]
    fn issue_keyed_replace_set_drops_a_re_read_issues_stale_closer_of_any_kind() {
        let conn = db();
        let binding = binding(&[]);
        cache_closed_issue(&conn, "5");
        // Issue 5's PREVIOUS provider closer was commit `old`; this walk re-reads issue 5 and its
        // ClosedEvent now names PR 9. The issue-keyed replace-set reaps EVERY provider closer for
        // issue 5 (the commit included), so the stale commit closer dies and only the fresh PR
        // closer remains — reaping is keyed by the issue, not by any one closer.
        crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: crate::CloserKind::Commit,
            closer_key: "old".into(),
            closer_commit: Some("old".into()),
            source: crate::ClosingEdgeSource::Provider,
        })
        .unwrap();
        let client = ScriptClient::new(vec![page(Vec::new())]);
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            edges: vec![crate::ClosingEdge {
                project: "o/r".into(),
                issue_kind: ItemKind::Issue,
                issue_key: "5".into(),
                closer_kind: crate::CloserKind::ChangeRequest,
                closer_key: "9".into(),
                closer_commit: None,
                source: crate::ClosingEdgeSource::Provider,
            }],
            item_updates: Vec::new(),
            replaced_issue_closers: vec!["5".into()],
            next: None,
            frontier: None,
        }));
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
        let edges = crate::store::closing_edges_for_item(
            &conn,
            Tracker::Github,
            "o/r",
            ItemKind::Issue,
            "5",
        )
        .unwrap();
        assert_eq!(edges.len(), 1, "the stale commit closer died with the refresh");
        assert_eq!(edges[0].closer_kind, crate::CloserKind::ChangeRequest);
        assert_eq!(edges[0].closer_key, "9", "only the fresh authoritative closer remains");
    }

    /// The reported hazard the issue-keyed model fixes: a UI-linked closure (issue 5 <- PR 9 from
    /// ClosedEvent, NOT in PR 9's `closingIssuesReferences`) must SURVIVE a later PR-phase re-read
    /// of PR 9. Under the old closer-keyed replace-set, re-reading PR 9 deleted every
    /// `change_request` edge with closer 9 — including the issue-phase UI-linked row 5<-9, which
    /// the PR phase never re-provides — and issue 5 (older than the watermark) was never revisited
    /// to restore it, silently dropping attested closure evidence.
    #[test]
    fn a_pr_phase_re_read_does_not_clobber_an_issue_phase_ui_linked_edge() {
        let conn = db();
        let binding = binding(&[]);
        cache_closed_issue(&conn, "5");
        // Prior walk stored the UI-linked edge 5<-9 from issue 5's ClosedEvent.
        crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: crate::CloserKind::ChangeRequest,
            closer_key: "9".into(),
            closer_commit: Some("mergesha".into()),
            source: crate::ClosingEdgeSource::Provider,
        })
        .unwrap();
        // This incremental walk re-reads PR 9 in the PR phase (it edited after the watermark), but
        // PR 9's closingIssuesReferences does NOT list issue 5 (the closure was UI-linked). Issue 5
        // is older than the watermark, so the issue phase does NOT revisit it:
        // `replaced_issue_closers` is empty and there are no fresh edges.
        let client = ScriptClient::new(vec![page(Vec::new())]);
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            edges: Vec::new(),
            item_updates: Vec::new(),
            replaced_issue_closers: Vec::new(),
            next: None,
            frontier: None,
        }));
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
        let edges = crate::store::closing_edges_for_item(
            &conn,
            Tracker::Github,
            "o/r",
            ItemKind::Issue,
            "5",
        )
        .unwrap();
        assert_eq!(edges.len(), 1, "the UI-linked edge survives a PR-phase re-read of its closer");
        assert_eq!(edges[0].closer_key, "9");
    }

    /// The PR phase must not RESURRECT a stale closer: if issue 5's authoritative provider closer
    /// is already PR 7 (its ClosedEvent moved there after a reopen+reclose) and PR 9 — edited after
    /// the watermark — still lists #5 in `closingIssuesReferences` while #5 sits below the
    /// watermark (never re-read this walk, so no reap), storing `5<-9` would leave two
    /// conflicting provider closers. The conflicting-closer gate suppresses it; the same-closer
    /// idempotent case still passes.
    #[test]
    fn the_pr_phase_does_not_resurrect_a_closer_that_conflicts_with_the_authoritative_one() {
        let conn = db();
        let binding = binding(&[]);
        cache_closed_issue(&conn, "5");
        crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: crate::CloserKind::ChangeRequest,
            closer_key: "7".into(),
            closer_commit: None,
            source: crate::ClosingEdgeSource::Provider,
        })
        .unwrap();
        // PR-phase page (no reap: replaced_issue_closers empty) re-adding the stale 5<-9.
        let client = ScriptClient::new(vec![page(Vec::new())]);
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            edges: vec![crate::ClosingEdge {
                project: "o/r".into(),
                issue_kind: ItemKind::Issue,
                issue_key: "5".into(),
                closer_kind: crate::CloserKind::ChangeRequest,
                closer_key: "9".into(),
                closer_commit: None,
                source: crate::ClosingEdgeSource::Provider,
            }],
            item_updates: Vec::new(),
            replaced_issue_closers: Vec::new(),
            next: None,
            frontier: None,
        }));
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
        let edges = crate::store::closing_edges_for_item(
            &conn,
            Tracker::Github,
            "o/r",
            ItemKind::Issue,
            "5",
        )
        .unwrap();
        assert_eq!(edges.len(), 1, "the stale conflicting closer is not resurrected");
        assert_eq!(edges[0].closer_key, "7", "only the authoritative closer remains");
    }

    #[test]
    fn min_frontier_across_phases_cannot_skip_the_older_stream() {
        let conn = db();
        let binding = binding(&[]);
        let client = ScriptClient::new(vec![page(Vec::new())]);
        // PR phase frontier is NEWER than the issue phase's — the stored watermark must be the
        // conservative minimum so the next walk cannot skip issue updates in between.
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            frontier: Some("2026-01-09T00:00:00Z".into()),
            next: Some("issues".into()),
            ..Default::default()
        }));
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            frontier: Some("2026-01-03T00:00:00Z".into()),
            ..Default::default()
        }));
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &client, false))
            .unwrap();
        let repo_id = rag_rat_db::schema::active_repo_id(&conn).unwrap();
        let since = rag_rat_db::meta::read_meta(
            &conn,
            &format!("papertrail_attested_closers_since:{repo_id}:github:o/r"),
        )
        .unwrap();
        assert_eq!(since.as_deref(), Some("2026-01-03T00:00:00Z"));
    }
    #[test]
    fn a_mid_walk_capability_trip_surfaces_a_partial_signal() {
        let conn = db();
        let binding = binding(&[]);
        let client = ScriptClient::new(vec![page(Vec::new())]);
        // First page stores work and advances to the issues phase; the SECOND call returns
        // `None` (a mid-walk capability trip), so the walk is partial.
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            next: Some("issues".into()),
            frontier: Some("2026-01-05T00:00:00Z".into()),
            ..Default::default()
        }));
        client.attested.borrow_mut().push_back(None);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &client,
            false,
        ))
        .unwrap();
        assert!(
            report.attested_error.is_some(),
            "a mid-walk trip is reported, not folded into a clean no-supply result",
        );
        // The watermark did NOT advance: the partial walk redoes from the top next pass.
        let repo_id = rag_rat_db::schema::active_repo_id(&conn).unwrap();
        assert!(
            rag_rat_db::meta::read_meta(
                &conn,
                &format!("papertrail_attested_closers_since:{repo_id}:github:o/r"),
            )
            .unwrap()
            .is_none(),
        );
    }

    #[test]
    fn no_attested_supply_is_a_clean_no_op() {
        let conn = db();
        let binding = binding(&[]);
        let client = ScriptClient::new(vec![page(Vec::new())]);
        // FIRST page is `None` — the provider has no GraphQL supply.
        client.attested.borrow_mut().push_back(None);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &client,
            false,
        ))
        .unwrap();
        assert!(report.attested_error.is_none(), "no supply is clean, not an error");
        assert_eq!(report.attested_edges, 0);
    }
    #[test]
    fn attested_edge_for_an_uncached_or_open_target_is_skipped() {
        let conn = db();
        let binding = binding(&[]);
        // #5 is NOT cached (out of scope / not mirrored); #6 is cached but OPEN (reopened).
        conn.execute(
            "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, \
             title, body, synced_at_ms, repo_id, state_normalized) VALUES ('github', 'o/r', \
             'issue', '6', 'u', 'open', 't', 'b', 1, '__unassigned__', 'open')",
            [],
        )
        .unwrap();
        let client = ScriptClient::new(vec![page(Vec::new())]);
        client.attested.borrow_mut().push_back(Some(AttestedClosersPage {
            edges: vec![
                crate::ClosingEdge {
                    project: "o/r".into(),
                    issue_kind: ItemKind::Issue,
                    issue_key: "5".into(),
                    closer_kind: crate::CloserKind::Commit,
                    closer_key: "a".into(),
                    closer_commit: Some("a".into()),
                    source: crate::ClosingEdgeSource::Provider,
                },
                crate::ClosingEdge {
                    project: "o/r".into(),
                    issue_kind: ItemKind::Issue,
                    issue_key: "6".into(),
                    closer_kind: crate::CloserKind::Commit,
                    closer_key: "b".into(),
                    closer_commit: Some("b".into()),
                    source: crate::ClosingEdgeSource::Provider,
                },
            ],
            ..Default::default()
        }));
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &client,
            false,
        ))
        .unwrap();
        assert_eq!(report.attested_edges, 0, "neither an uncached nor an open target gets an edge");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM papertrail_closing_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn an_attested_pause_propagates_the_resume_time_not_a_generic_error() {
        let conn = db();
        let binding = binding(&[]);
        let client = ScriptClient::new(vec![page(Vec::new())]);
        client.attested_pause.set(true);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &client,
            false,
        ))
        .unwrap();
        assert_eq!(
            report.paused_until_ms,
            Some(999_000),
            "the scheduler must honor the resume time"
        );
        assert!(report.attested_error.is_none(), "a pause is a pause, not a swallowed error");
    }

    /// A HARD (non-pause) attested-walk failure must be PERSISTED so `papertrail_sync_status` shows
    /// it — the item mirror records success and clears its own error state, so without a separate
    /// persisted signal a doomed enrichment walk re-runs every tick looking healthy. A later clean
    /// attested walk clears it.
    #[test]
    fn a_hard_attested_failure_is_persisted_then_cleared_by_a_clean_walk() {
        let conn = db();
        let binding = binding(&[]);

        let failing = ScriptClient::new(vec![page(Vec::new())]).with_repo_comments(Vec::new());
        failing.attested_hard_error.set(true);
        let report = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &failing,
            false,
        ))
        .unwrap();
        assert!(report.attested_error.is_some(), "the hard error is surfaced on the report");
        assert!(report.paused_until_ms.is_none(), "a hard error is not a pause");
        assert_eq!(
            read_attested_error(&conn, &binding).unwrap().as_deref(),
            Some("attested walk boom"),
            "the failure is persisted for the durable status snapshot",
        );

        // A subsequent clean attested walk (no supply, no error) clears the persisted failure.
        let clean = ScriptClient::new(vec![page(Vec::new())]).with_repo_comments(Vec::new());
        block_on(mirror_binding(&conn, &binding, std::slice::from_ref(&binding), &clean, false))
            .unwrap();
        assert!(
            read_attested_error(&conn, &binding).unwrap().is_none(),
            "a clean attested walk clears the persisted failure",
        );
    }

    #[test]
    fn pruning_an_out_of_scope_issue_takes_its_provider_closing_edges() {
        let conn = db();
        // A tag-scoped binding; the item walk stores a `docs`-labelled issue #5, then a filter
        // narrowed to `bug` prunes it — its provider closing edge must go too.
        cache_closed_issue(&conn, "5");
        crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: crate::CloserKind::Commit,
            closer_key: "abc".into(),
            closer_commit: Some("abc".into()),
            source: crate::ClosingEdgeSource::Provider,
        })
        .unwrap();
        delete_item_for_tests(&conn, &binding(&["bug"]), ItemKind::Issue, "5").unwrap();
        assert!(
            crate::store::closing_edges_for_item(
                &conn,
                Tracker::Github,
                "o/r",
                ItemKind::Issue,
                "5"
            )
            .unwrap()
            .is_empty(),
            "a pruned issue's closing edges leave with it",
        );
    }
}
