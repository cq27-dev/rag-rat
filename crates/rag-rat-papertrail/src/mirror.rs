//! Provider-neutral whole-project mirror runner. This module owns cursor arbitration,
//! fetch-then-commit page semantics, tag pruning, pause/resume, and full healing.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, params};

use super::transport::{PauseReason, TransportError};
use super::*;

// The first backfill page's strict upper boundary: a sentinel above any real item's `updated_at`,
// so the newest-first descent starts at the top. Its value is capped by the strictest provider —
// GitHub Search accepts years only through 2970, and the strict `updated:<boundary` query must
// stay valid there.
const INITIAL_BACKFILL_BOUNDARY: &str = "2970-12-31T23:59:59Z";
/// An empty initial walk has consumed no provider item. Persist the lowest practical timestamp so
/// later runs enter the normal probe/delta path and discover items created after that walk.
const EMPTY_PROJECT_HIGH_MARK: &str = "1970-01-01T00:00:00Z";

/// One item's identity in the cursor's `delta_processed_keys` / `backfill_processed_keys` /
/// `item_thread_cursor` JSON. `kind` serializes as its [`ItemKind`] token (`issue` /
/// `change_request`), so the stored spelling is the persisted token. The sets serialize in variant
/// order (`Ord`), not lexical token order; every read is a membership test, so order is
/// meaningless.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct ProcessedItem {
    kind: ItemKind,
    key: String,
    updated_at: Option<String>,
}

/// A stored [`ProcessedItem`] with its kind still a raw token, so an entry whose kind is outside
/// [`ItemKind`] can be skipped instead of failing the whole cursor decode.
#[derive(Deserialize)]
struct StoredProcessedItem {
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

    /// Drop the resumable progress shared by every restart-from-the-top: backfill, item-delta and
    /// comment page state, the thread cursor, and the processed-key sets. The high-water marks,
    /// filter fingerprint and item-delta window are left to the caller — a filter change and a
    /// full rewalk reset different ones.
    fn reset_walk_progress(&mut self) {
        self.low_mark_at = None;
        self.backfill_done = false;
        self.comment_page_token = None;
        self.comment_scan_since = None;
        self.comment_stream_cursors.clear();
        self.item_delta_page_token = None;
        self.item_delta_in_progress = false;
        self.item_delta_replay_required = false;
        self.backfill_page_cursor = None;
        self.item_thread_cursor = None;
        self.delta_processed_keys.clear();
        self.backfill_processed_keys.clear();
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
    pub pause_reason: Option<PauseReason>,
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

impl MirrorBindingReport {
    /// Record a rate-governed stop: the run keeps what it landed and resumes at `resume_at_ms`.
    fn record_pause(&mut self, resume_at_ms: i64, reason: PauseReason) {
        self.paused_until_ms = Some(resume_at_ms);
        self.pause_reason = Some(reason);
    }
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
        cursor.reset_walk_progress();
        cursor.filter_fingerprint = fingerprint;
        // Unlike a full rewalk, a filter change also drops the item-delta window and keeps the
        // item and comment high-water marks.
        cursor.item_delta_scan_since = None;
        cursor.item_delta_high_mark_at = None;
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
        MirrorWalk { conn, binding, trackers, client, cursor: &mut cursor, report: &mut report }
            .run()
            .await;
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
                    Some((resume_at_ms, reason)) => report.record_pause(resume_at_ms, reason),
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
        Err(error) => match pause(&error) {
            Some((resume_at_ms, reason)) => {
                report.record_pause(resume_at_ms, reason);
                Ok(report)
            },
            None => Err(error),
        },
    }
}

/// One binding's resumable mirror walk: the connection and binding it writes for, the configured
/// tracker set its ref mining parses against, the provider client it pages, and the cursor and
/// report every stage advances.
struct MirrorWalk<'a, C> {
    conn: &'a Connection,
    binding: &'a ResolvedTracker,
    trackers: &'a [ResolvedTracker],
    client: &'a C,
    cursor: &'a mut MirrorCursor,
    report: &'a mut MirrorBindingReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PageLane {
    Delta,
    Backfill,
}

fn processed_item(item: &PapertrailItem) -> ProcessedItem {
    ProcessedItem {
        kind: item.item_kind,
        key: item.item_key.clone(),
        updated_at: item.updated_at.clone(),
    }
}

impl<C: PapertrailClient> MirrorWalk<'_, C> {
    async fn run(&mut self) -> anyhow::Result<()> {
        let previous_high = self.cursor.high_mark_at.clone();
        if !self.cursor.full_rewalk
            && let Some(high) = previous_high.as_deref()
        {
            if self.cursor.item_delta_in_progress {
                self.cursor.item_delta_scan_since.get_or_insert_with(|| overlap_timestamp(high));
                save_cursor(self.conn, self.binding, self.cursor, false)?;
                self.sync_item_delta().await?;
            } else {
                let probe = self
                    .client
                    .freshness_probe(&self.binding.project, &FreshnessProbe {
                        updated_since: Some(high.to_string()),
                        etag: self.cursor.probe_etag.clone(),
                    })
                    .await?;
                self.cursor.probe_etag = probe.etag;
                // A quiet probe must not starve an OWED replay: when the prior delta left its
                // conservative frontier below the probe target, the boundary replay has to run even
                // if nothing new moved — some providers (GitLab) report a timestamp tie as
                // not_modified, and the stranded boundary row would otherwise wait for the daily
                // full walk. probe.latest is None on that path, which sync_item_delta already
                // treats as "replay against the durable high mark".
                if !probe.not_modified || self.cursor.item_delta_replay_required {
                    self.cursor.item_delta_in_progress = true;
                    self.cursor.item_delta_scan_since = Some(overlap_timestamp(high));
                    self.cursor.item_delta_high_mark_at = probe.latest;
                    save_cursor(self.conn, self.binding, self.cursor, false)?;
                    self.sync_item_delta().await?;
                } else {
                    self.report.probe_not_modified = true;
                    save_cursor(self.conn, self.binding, self.cursor, false)?;
                }
            }
            self.sync_comment_delta().await?;
        }

        while !self.cursor.backfill_done {
            let boundary = self
                .cursor
                .low_mark_at
                .clone()
                .unwrap_or_else(|| INITIAL_BACKFILL_BOUNDARY.to_string());
            let request = self.cursor.backfill_page_cursor.clone().unwrap_or_else(|| PageCursor {
                updated_before: Some(boundary.clone()),
                ..PageCursor::default()
            });
            let page = self.client.items_page(&self.binding.project, &request).await?;
            if page.items.is_empty() && page.next.is_none() && page.backfill_boundary.is_none() {
                self.cursor.backfill_done = true;
                self.cursor.high_mark_at.get_or_insert_with(|| EMPTY_PROJECT_HIGH_MARK.to_string());
                self.cursor.backfill_processed_keys.clear();
                // The consumed continuation must not outlive the walk: a chained provider leg (the
                // request that produced THIS empty page) was persisted as `backfill_page_cursor` on
                // the previous iteration, and leaving it behind makes `continuation()` misread the
                // COMPLETED walk as interrupted work forever.
                self.cursor.backfill_page_cursor = None;
                save_cursor(self.conn, self.binding, self.cursor, false)?;
                break;
            }
            if let Some(next) = &page.next {
                ensure_cursor_advanced(&request, next, "backfill")?;
            }
            self.store_item_page_resumably(&page.items, PageLane::Backfill).await?;
            if self.cursor.high_mark_at.is_none() {
                self.cursor.high_mark_at = max_item_updated_at(&page.items);
            }
            if let Some(next) = page.next {
                self.cursor.backfill_page_cursor = Some(next);
                save_cursor(self.conn, self.binding, self.cursor, false)?;
                continue;
            }
            let next_low = page
                .backfill_boundary
                .or_else(|| min_item_updated_at(&page.items))
                .or_else(|| {
                    request.page_token.as_ref().and_then(|_| request.updated_before.clone())
                })
                .ok_or_else(|| anyhow::anyhow!("backfill page has no updated_at boundary"))?;
            anyhow::ensure!(
                next_low < boundary,
                "backfill boundary did not advance below {boundary}"
            );
            self.cursor.low_mark_at = Some(next_low);
            self.cursor.backfill_page_cursor = None;
            self.cursor.backfill_processed_keys.clear();
            save_cursor(self.conn, self.binding, self.cursor, false)?;
        }
        if previous_high.is_none() || self.cursor.full_rewalk {
            self.sync_comment_delta().await?;
        }
        if self.cursor.full_rewalk {
            let tx = self.conn.unchecked_transaction()?;
            self.report.pruned_items += prune_missing(&tx, self.binding)?;
            rebuild_fts(&tx)?;
            self.cursor.full_rewalk = false;
            save_cursor(&tx, self.binding, self.cursor, true)?;
            tx.commit()?;
        }
        Ok(())
    }

    async fn sync_item_delta(&mut self) -> anyhow::Result<()> {
        let mut request = PageCursor {
            updated_since: self.cursor.item_delta_scan_since.clone(),
            page_token: self.cursor.item_delta_page_token.clone(),
            ..PageCursor::default()
        };
        loop {
            let page = self.client.items_page(&self.binding.project, &request).await?;
            let first_page_high = request
                .page_token
                .is_none()
                .then(|| page.items.iter().filter_map(|item| item.updated_at.clone()).max());
            let next = if let Some(mut next) = page.next {
                next.updated_since = self.cursor.item_delta_scan_since.clone();
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
                let reached_probe = if self.cursor.item_delta_replay_required
                    && self.cursor.item_delta_high_mark_at.is_none()
                {
                    first_page_high == self.cursor.high_mark_at
                } else {
                    first_page_high == self.cursor.item_delta_high_mark_at && next.is_none()
                };
                self.cursor.item_delta_high_mark_at = first_page_high;
                self.cursor.item_delta_replay_required = !reached_probe;
                if self.cursor.item_delta_replay_required {
                    // The next run must probe unconditionally so this conservative frontier is
                    // replayed even when the prior probe's ETag is otherwise still current.
                    self.cursor.probe_etag = None;
                }
            }
            self.store_item_page_resumably(&page.items, PageLane::Delta).await?;
            if let Some(next) = next {
                self.cursor.delta_processed_keys.clear();
                self.cursor.item_delta_page_token = next.page_token.clone();
                self.cursor.item_delta_in_progress = true;
                save_cursor(self.conn, self.binding, self.cursor, false)?;
                request = next;
            } else {
                self.cursor.high_mark_at = max_timestamp(
                    self.cursor.high_mark_at.take(),
                    self.cursor.item_delta_high_mark_at.take(),
                );
                self.cursor.delta_processed_keys.clear();
                self.cursor.item_delta_page_token = None;
                self.cursor.item_delta_scan_since = None;
                self.cursor.item_delta_in_progress = false;
                save_cursor(self.conn, self.binding, self.cursor, false)?;
                break;
            }
        }
        Ok(())
    }

    async fn sync_comment_delta(&mut self) -> anyhow::Result<()> {
        // A legacy shared watermark may seed every stream exactly once. Never seed a stream from
        // the aggregate while this loop is advancing siblings: that recreates the
        // cross-stream race this map exists to prevent. A provider stream added later
        // starts from scratch, which is safe.
        let legacy_high = self
            .cursor
            .comment_stream_cursors
            .is_empty()
            .then(|| self.cursor.comment_high_mark_at.clone())
            .flatten();
        for stream in self.client.comment_streams() {
            let state =
                self.cursor.comment_stream_cursors.entry((*stream).to_string()).or_insert_with(
                    || CommentStreamCursor {
                        high_mark_at: legacy_high.clone(),
                        page_token: None,
                        scan_since: None,
                        scan_high_mark_at: None,
                    },
                );
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
                let page = self.client.comments_page(&self.binding.project, &request).await?;
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
                store_repo_comments(
                    self.conn,
                    self.binding,
                    self.trackers,
                    &page.comments,
                    self.report,
                )?;
                let state =
                    self.cursor.comment_stream_cursors.get_mut(*stream).expect("stream inserted");
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
                self.cursor.comment_high_mark_at =
                    common_comment_high_mark(&self.cursor.comment_stream_cursors);
                save_cursor(self.conn, self.binding, self.cursor, false)?;
                let Some(next) = next else { break };
                request = next;
            }
        }
        Ok(())
    }

    async fn store_item_page_resumably(
        &mut self,
        items: &[PapertrailItem],
        lane: PageLane,
    ) -> anyhow::Result<()> {
        if let Some(active) = self.cursor.item_thread_cursor.clone() {
            if let Some(item) = items
                .iter()
                .find(|item| item.item_kind == active.item.kind && item.item_key == active.item.key)
            {
                let current = processed_item(item);
                if current != active.item {
                    let mut item = item.clone();
                    self.client.enrich_item(&mut item).await?;
                    if self.binding.tracks(item.tags.iter().map(String::as_str)) {
                        self.begin_item_thread(&item, lane)?;
                    } else {
                        self.report.pruned_items += usize::from(delete_item(
                            self.conn,
                            self.binding,
                            item.item_kind,
                            &item.item_key,
                        )?);
                        self.cursor.item_thread_cursor = None;
                        mark_processed(self.cursor, lane, current);
                        save_cursor(self.conn, self.binding, self.cursor, false)?;
                    }
                }
            }
            if self.cursor.item_thread_cursor.is_some() {
                self.resume_item_thread().await?;
            }
        }
        for item in items {
            let key = processed_item(item);
            let already_stored = match lane {
                PageLane::Delta => self.cursor.delta_processed_keys.contains(&key),
                PageLane::Backfill => self.cursor.backfill_processed_keys.contains(&key),
            };
            if already_stored {
                continue;
            }
            let mut item = item.clone();
            self.client.enrich_item(&mut item).await?;
            if self.binding.tracks(item.tags.iter().map(String::as_str)) {
                self.begin_item_thread(&item, lane)?;
                self.resume_item_thread().await?;
            } else {
                self.report.pruned_items += usize::from(delete_item(
                    self.conn,
                    self.binding,
                    item.item_kind,
                    &item.item_key,
                )?);
                mark_processed(self.cursor, lane, key);
                save_cursor(self.conn, self.binding, self.cursor, false)?;
            }
        }
        Ok(())
    }

    fn begin_item_thread(&mut self, item: &PapertrailItem, lane: PageLane) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        store_item(&tx, self.binding.provider, item)?;
        // #702: the item's own text is mined in the same transaction — refs (`source_kind='item'`)
        // and, for a change request with closing keywords, the text-tier closing edge.
        sync::mine_item_refs(&tx, self.binding.provider, self.trackers, item)?;
        replace_tags(&tx, self.binding, item)?;
        if self.cursor.full_rewalk {
            mark_full_seen(&tx, self.binding, item)?;
        }
        self.cursor.item_thread_cursor = Some(ItemThreadCursor {
            item: processed_item(item),
            lane,
            stream_index: 0,
            page_cursor: None,
            seen_comment_ids: BTreeSet::new(),
            previous_comment_ids: None,
            saw_pagination: false,
        });
        save_cursor(&tx, self.binding, self.cursor, false)?;
        tx.commit()?;
        self.report.stored_items += 1;
        Ok(())
    }

    async fn resume_item_thread(&mut self) -> anyhow::Result<()> {
        loop {
            let thread = self.cursor.item_thread_cursor.clone().expect("active item thread");
            let kind = thread.item.kind;
            let streams = self.client.item_comment_streams(kind);
            let Some(stream) = streams.get(thread.stream_index) else {
                if thread.saw_pagination
                    && thread.previous_comment_ids.as_ref() != Some(&thread.seen_comment_ids)
                {
                    // GitHub item-comment continuations are mutable page numbers. Require two
                    // identical complete walks before treating absence as deletion; a row shifted
                    // behind one walk is rediscovered by the next instead of being pruned locally.
                    let active =
                        self.cursor.item_thread_cursor.as_mut().expect("active item thread");
                    active.previous_comment_ids = Some(thread.seen_comment_ids);
                    active.seen_comment_ids.clear();
                    active.stream_index = 0;
                    active.page_cursor = None;
                    save_cursor(self.conn, self.binding, self.cursor, false)?;
                    continue;
                }
                let tx = self.conn.unchecked_transaction()?;
                prune_unseen_item_comments(
                    &tx,
                    self.binding,
                    kind,
                    &thread.item.key,
                    &thread.seen_comment_ids,
                )?;
                self.cursor.item_thread_cursor = None;
                mark_processed(self.cursor, thread.lane, thread.item);
                save_cursor(&tx, self.binding, self.cursor, false)?;
                tx.commit()?;
                return Ok(());
            };
            let request = thread.page_cursor.clone().unwrap_or_else(|| PageCursor {
                stream: Some((*stream).to_string()),
                ..PageCursor::default()
            });
            let page = match self
                .client
                .item_comments_page(&self.binding.project, kind, &thread.item.key, &request)
                .await
            {
                Ok(page) => page,
                Err(error) if is_item_not_found(&error) => {
                    // GitHub deliberately uses 404 both for absent and inaccessible resources, and
                    // a stale comment continuation says nothing about the parent. Unpin ordinary
                    // sync without erasing the last complete cache; a successful full rewalk owns
                    // authoritative item pruning.
                    let tx = self.conn.unchecked_transaction()?;
                    self.cursor.item_thread_cursor = None;
                    mark_processed(self.cursor, thread.lane, thread.item);
                    save_cursor(&tx, self.binding, self.cursor, false)?;
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
            let active = self.cursor.item_thread_cursor.as_mut().expect("active item thread");
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
            let tx = self.conn.unchecked_transaction()?;
            for comment in &page.comments {
                store_comment(&tx, self.binding.provider, comment)?;
                sync::mine_comment_refs(&tx, self.binding.provider, self.trackers, comment)?;
            }
            save_cursor(&tx, self.binding, self.cursor, false)?;
            tx.commit()?;
            self.report.stored_comments += page.comments.len();
        }
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
                    item_thread_cursor: decode_item_thread_cursor(row.get(15)?, 15)?,
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

/// Decode a processed-key set. Legacy rows are bare `(kind, key)` pairs. An entry whose kind is
/// outside [`ItemKind`] is SKIPPED: the sets are membership-only, so a drifted entry costs one
/// re-processed item, where failing the decode would wedge the walk.
fn decode_processed_items(
    value: Option<String>,
    column: usize,
) -> rusqlite::Result<BTreeSet<ProcessedItem>> {
    let Some(value) = value else { return Ok(BTreeSet::new()) };
    let stored = serde_json::from_str::<Vec<StoredProcessedItem>>(&value)
        .or_else(|_| {
            serde_json::from_str::<Vec<(String, String)>>(&value).map(|legacy| {
                legacy
                    .into_iter()
                    .map(|(kind, key)| StoredProcessedItem { kind, key, updated_at: None })
                    .collect()
            })
        })
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
        })?;
    Ok(stored
        .into_iter()
        .filter_map(|item| {
            Some(ProcessedItem {
                kind: ItemKind::from_db_str(&item.kind).ok()?,
                key: item.key,
                updated_at: item.updated_at,
            })
        })
        .collect())
}

/// Decode the active item-thread cursor. A cursor whose item kind is outside [`ItemKind`] is
/// dropped, so that one item is re-processed from its page; any other malformation still fails
/// the decode exactly as [`decode_json`] does.
fn decode_item_thread_cursor(
    value: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<ItemThreadCursor>> {
    let drifted_kind = value
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|json| {
            json["item"]["kind"].as_str().map(|kind| ItemKind::from_db_str(kind).is_err())
        })
        .unwrap_or(false);
    if drifted_kind {
        return Ok(None);
    }
    decode_json(value, column)
}

fn reset_for_full_rewalk(
    conn: &Connection,
    binding: &ResolvedTracker,
    cursor: &mut MirrorCursor,
) -> anyhow::Result<()> {
    cursor.reset_walk_progress();
    // Unlike a filter change, a full rewalk also drops the item and comment high-water marks and
    // keeps the item-delta window.
    cursor.high_mark_at = None;
    cursor.comment_high_mark_at = None;
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
    Ok(rag_rat_db::meta::set_meta(conn, &key, sanitized.trim())?)
}

/// Clear a binding's persisted attested-walk failure — a clean completed attested walk.
pub(crate) fn clear_attested_error(
    conn: &Connection,
    binding: &ResolvedTracker,
) -> anyhow::Result<()> {
    let key = attested_meta_key(binding, conn, "error")?;
    Ok(rag_rat_db::meta::delete_meta(conn, &key)?)
}

/// Read a binding's persisted attested-walk failure, for the status snapshot.
pub(crate) fn read_attested_error(
    conn: &Connection,
    binding: &ResolvedTracker,
) -> anyhow::Result<Option<String>> {
    let key = attested_meta_key(binding, conn, "error")?;
    Ok(rag_rat_db::meta::read_meta(conn, &key)?)
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
    Ok(rag_rat_db::meta::delete_meta(conn, &key)?)
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
            report.attested_writes +=
                reap_provider_closers_for_issue(&tx, &repo_id, binding.provider, IssueTarget {
                    project: &binding.project,
                    issue_key,
                })?;
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
            if !cached_issue_is_closed(&tx, &repo_id, binding.provider, IssueTarget {
                project: &edge.project,
                issue_key: &edge.issue_key,
            })? {
                continue;
            }
            // Defer to the issue's ONE authoritative closer: never store an edge that CONFLICTS
            // with a provider closer already recorded for this issue (a different closer). The
            // issue phase reaps all of a re-read issue's provider edges earlier in THIS tx, so its
            // own fresh closer never conflicts; the PR phase does NOT reap, so without this a PR
            // edited after the watermark would resurrect `#5←#9` once #5's ClosedEvent had already
            // moved its closer elsewhere (and #5 sits below the watermark, never re-read). The
            // same-closer case is not a conflict, so an idempotent re-store still passes.
            if has_conflicting_provider_closer(&tx, &repo_id, binding.provider, edge)? {
                continue;
            }
            store_closing_edge(&tx, binding.provider, edge)?;
            report.attested_edges += 1;
        }
        for update in &page.item_updates {
            report.attested_writes += stamp_attested_resolution(
                &tx,
                &repo_id,
                binding.provider,
                &binding.project,
                update,
            )?;
            report.attested_writes += stamp_attested_merge_commit(
                &tx,
                &repo_id,
                binding.provider,
                &binding.project,
                update,
            )?;
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
    // The item identity. Every statement here keys on exactly this list except the two mined-ref
    // prunes, which also bind their source-kind token.
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
        args,
    )?;
    // #702: mined evidence dies with its source. The mined identities are constructible in SQL
    // (item: `project:kind:key`; comment: that + `:comment_id` — a prefix match), so the pruned
    // item's own mined refs AND every pruned comment's mined refs go in one pass; a pruned
    // change request also drops the text-tier closing edges it minted (provider-attested edges
    // outlive their text sources by design).
    conn.execute(
        "DELETE FROM papertrail_refs WHERE repo_id=?1 AND source_kind=?6 AND source_text = ?2 || \
         ':' || ?3 || ':' || ?4 || ':' || ?5",
        params![
            repo_id,
            binding.provider.as_db_str(),
            binding.project,
            kind.as_db_str(),
            key,
            RefSourceKind::Item.as_db_str(),
        ],
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
        "DELETE FROM papertrail_refs WHERE repo_id=?1 AND source_kind=?3 AND source_text LIKE ?2 \
         ESCAPE '\\'",
        params![repo_id, like_prefix, RefSourceKind::Comment.as_db_str()],
    )?;

    conn.execute(
        "DELETE FROM papertrail_item_tags WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND \
         item_kind=?4 AND item_key=?5",
        args,
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
            args,
        )?;
    }
    Ok(conn.execute(
        "DELETE FROM papertrail_items WHERE repo_id=?1 AND tracker=?2 AND project=?3 AND \
         item_kind=?4 AND item_key=?5",
        args,
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
             item_kind=?4 AND item_key=?5 AND comment_id=?6 AND doc_kind=?7",
            params![
                repo_id,
                binding.provider.as_db_str(),
                binding.project,
                kind.as_db_str(),
                key,
                comment_id,
                DocKind::Comment.as_db_str(),
            ],
        )?;
        // #702: a pruned comment takes its mined refs with it (exact identity — the source will
        // never be re-mined to replace the set once its row is gone).
        conn.execute(
            "DELETE FROM papertrail_refs WHERE repo_id=?1 AND source_kind=?7 AND source_text = ?2 \
             || ':' || ?3 || ':' || ?4 || ':' || ?5 || ':' || ?6",
            params![
                repo_id,
                binding.provider.as_db_str(),
                binding.project,
                kind.as_db_str(),
                key,
                comment_id,
                RefSourceKind::Comment.as_db_str(),
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

/// One second before `timestamp`. Malformed input comes back UNCHANGED (a zero overlap) rather
/// than erroring — unlike GitLab's `day_before`, which shares the calendar rule but rejects bad
/// dates; see [`parse_time`] for why that fallback must stay out of the common path.
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
                (year, month, day) = previous_civil_day(year, month, day);
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

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// The civil day before `(year, month, day)`: January 1 rolls back to December 31 of the prior
/// year, day 1 of any other month to that previous month's last day (leap years via
/// [`days_in_month`]), anything else to `day - 1`. Callers validate the input; the rule is shared,
/// each caller's malformed-input policy is not.
pub(crate) fn previous_civil_day(year: i32, month: u32, day: u32) -> (i32, u32, u32) {
    if day > 1 {
        (year, month, day - 1)
    } else if month > 1 {
        (year, month - 1, days_in_month(year, month - 1))
    } else {
        (year - 1, 12, 31)
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
#[path = "mirror_tests/mod.rs"]
mod tests;
