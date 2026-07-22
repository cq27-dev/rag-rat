//! Background precompute of the clone-edge graph (#286): the resumable, generation-staged build
//! that writes `clone_edges` so `find_clones` reads a persisted graph instead of recomputing the
//! super-linear SourcererCC candidate pairs every query (it does not finish in 240s on a
//! 118k-function index). The read side (the `find_clones` fast path) is Phase C.
//!
//! Generation-staged: each build writes a fresh `clone_graph_generations` row; reads serve the
//! latest `Complete` generation (the `clone_graph_live_generation` meta key); the pointer flips
//! atomically on completion so a half-built generation is never served. The build streams symbols
//! in `symbol_id` order and emits each clone pair from its SMALLER endpoint exactly once (the
//! SourcererCC structure), so it is chunkable + cursor-resumable under a `max_seconds` budget and
//! dedup is structural. It reuses the EXACT candidate-gen primitives of the parent `clones` module
//! (`sub_block_tokens`, `overlap`, `verified_clone`) so the persisted set equals the live
//! `candidate_pairs_from_bags` set (the Phase-C parity test pins this). Edges are CONTENT-ANCHORED
//! on `(path, start_byte, file_sha)` (the #248 rule — no `symbol_id` FK); reads resolve back to
//! live symbols.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use rag_rat_base::time::now_ms;
use rag_rat_clones::NORM_VERSION;
use rusqlite::{Connection, params};
use serde::Serialize;

use super::THETA;
// The current-df bag load stays test-only here: production build passes read the pinned epoch.
#[cfg(test)]
use super::substrate::load_scoped_baseline_bags;
use super::substrate::{
    SymbolBag, load_clone_df_epoch, load_scoped_baseline_bags_with_df, overlap, sub_block_tokens,
    verified_clone,
};
use crate::index::{ContentRevisionSnapshot, IndexDatabase};

/// θ the graph is precomputed at — the default `find_clones` threshold. Queries at θ ≥ this read
/// the stored edges (filtering the exact gate inputs); θ below falls back to the live path.
pub(crate) const CLONE_PRECOMPUTE_THETA: f64 = THETA;

const DEFAULT_BATCH_SIZE: usize = 512;

/// Per-repo meta keys for the #472 quiet-window gate: the stale content revision under
/// observation and when it was first observed (epoch ms).
const CLONE_GRAPH_QUIET_REVISION_META: &str = "clone_graph_quiet_candidate_revision";
const CLONE_GRAPH_QUIET_SINCE_META: &str = "clone_graph_quiet_candidate_since_ms";

/// One #472 quiet-gate probe outcome (#821): whether the deferred FULL rebuild is due, plus the
/// content digest the probe computed — pinned to the connection state — so the SAME pass's clone
/// delta can reuse it instead of paying the `main.files` digest scan again. `revision` is `None`
/// when the gate short-circuited without probing (nothing armed, no probe permission) or the
/// probe errored ([`Default`] is the caller's error fallback).
#[derive(Debug, Default)]
pub(crate) struct CloneGraphRebuildProbe {
    pub(crate) due: bool,
    pub(crate) revision: Option<ContentRevisionSnapshot>,
}

/// Soft per-pass budget + checkpoint granularity for one
/// [`IndexDatabase::reconcile_clone_edges_pass`].
#[derive(Debug, Clone)]
pub struct CloneEdgeOptions {
    /// Stop the pass once this many seconds elapse (after ≥1 batch, so progress is always made);
    /// `None` runs the pass to completion.
    pub max_seconds: Option<u64>,
    pub batch_size: usize,
    /// Rebuild even when the live generation already matches the current content.
    pub force: bool,
}

impl Default for CloneEdgeOptions {
    fn default() -> Self {
        Self { max_seconds: None, batch_size: DEFAULT_BATCH_SIZE, force: false }
    }
}

/// Outcome of a precompute pass (or loop). `status`: `Current` (skip — already fresh), `Complete`
/// (the generation finished and is now live), or `Partial` (budget tripped mid-build; resume next).
#[derive(Debug, Clone, Serialize)]
pub struct CloneEdgeReport {
    pub status: String,
    pub generation: i64,
    pub symbols_total: u64,
    pub symbols_processed: u64,
    pub edges_written: u64,
    pub source_revision: String,
    pub elapsed_ms: u64,
}

/// Reindex-stable identity of a symbol endpoint: `(path, start_byte, file_sha)`.
pub(super) type Anchor = (String, i64, String);

pub(super) struct EdgeRow {
    a_path: String,
    a_start_byte: i64,
    a_file_sha: String,
    b_path: String,
    b_start_byte: i64,
    b_file_sha: String,
    overlap: i64,
    a_token_len: i64,
    b_token_len: i64,
    similarity: f64,
    edge_source: &'static str,
}

/// The sub-block postings for ONE walked symbol (#296 phase 2): its content anchor plus its
/// `sub_block_tokens` set. Each `PostingGroup` expands to one `clone_subblock_postings` row per
/// token at flush time. Grouping is deliberate — it clones the `(path, file_sha)` strings ONCE per
/// symbol instead of once per token (a large function has hundreds of sub-block tokens).
pub(super) struct PostingGroup {
    pub(super) anchor: Anchor,
    pub(super) tokens: Vec<i64>,
}

pub(super) struct GenerationRow {
    pub(super) generation: i64,
    pub(super) source_revision: String,
    pub(super) normalizer_version: i64,
    cursor_symbol_id: i64,
    edges_written: u64,
    /// Whether this generation is POSTINGS-AWARE (#296 phase 2): its `clone_subblock_postings` are
    /// written in-band by a postings-aware binary. Set to 1 at Building creation and preserved
    /// through `Complete`. A generation created before the feature has `postings_written = 0`.
    /// Because a postings-aware binary writes every walked symbol's postings BEFORE advancing the
    /// cursor, a *Complete* postings-aware generation is fully populated — so the live-generation
    /// completeness gate (`pending_clone_graph` / the Phase-0 skip) reads this as "postings
    /// complete", and a `Building` one reads it as "resumable without a postings gap" (review R2).
    pub(super) postings_written: bool,
    /// Files absorbed by in-place deltas since this generation's full build (#473) — the df-drift
    /// signal: past [`CLONE_GRAPH_DRIFT_REBUILD_FILES`] the quiet gate schedules a full rebuild to
    /// restore sub-block selectivity (df is frozen at the build's epoch, so long-lived generations
    /// slowly lose candidate-pruning efficiency — never correctness).
    pub(super) delta_files_applied: i64,
}

/// How many delta-absorbed files a live generation tolerates before the background tail owes a
/// FULL rebuild (df epoch refresh + fresh postings). Drift degrades candidate-generation
/// efficiency only — edges stay exact — so this is a performance valve, sized generously: a
/// typical editing session touches far fewer files between natural quiet windows.
pub(super) const CLONE_GRAPH_DRIFT_REBUILD_FILES: i64 = 256;

impl IndexDatabase {
    /// Precompute the clone-edge graph to completion under the caller's write lock (loops resumable
    /// passes until the generation is `Complete` or already `Current`). `max_seconds` bounds a
    /// SINGLE pass (checkpoint granularity); the loop still runs to completion.
    pub fn precompute_clone_graph(
        &self,
        max_seconds: Option<u64>,
    ) -> anyhow::Result<CloneEdgeReport> {
        loop {
            let report = self.reconcile_clone_edges_pass(&CloneEdgeOptions {
                max_seconds,
                ..CloneEdgeOptions::default()
            })?;
            if report.status != "Partial" {
                return Ok(report);
            }
        }
    }

    /// One budgeted clone-graph pass for the watcher / `maintenance` tail (#286): a single
    /// resumable pass bounded by the remaining shared pass budget. A thin wrapper over
    /// [`Self::reconcile_clone_edges_pass`] so those callers needn't name `CloneEdgeOptions`.
    pub fn reconcile_clone_edges_with_budget(
        &self,
        max_seconds: Option<u64>,
    ) -> anyhow::Result<CloneEdgeReport> {
        self.reconcile_clone_edges_pass(&CloneEdgeOptions {
            max_seconds,
            ..CloneEdgeOptions::default()
        })
    }

    /// True when the precomputed graph is ABSENT or STALE vs the current content. The background
    /// watcher / maintenance tails wrap this in [`Self::clone_graph_rebuild_due`] (the #472
    /// quiet-window gate); explicit callers (`clones --precompute`) read the staleness directly.
    pub fn pending_clone_graph(&self) -> anyhow::Result<bool> {
        let revision = self.content_revision()?;
        self.clone_graph_stale_against(&revision)
    }

    /// The staleness core of [`Self::pending_clone_graph`], against an already-computed
    /// `content_revision()` so the quiet gate pays the digest once per probe.
    fn clone_graph_stale_against(&self, content_revision: &str) -> anyhow::Result<bool> {
        let conn = self.storage.connection();
        let Some(live) = live_generation_row(conn)? else {
            return Ok(true); // no completed generation yet
        };
        Ok(live.source_revision != content_revision
            || live.normalizer_version != NORM_VERSION
            // A live generation built before postings existed (`postings_written = 0`) is pending
            // so one rebuild pass fills `clone_subblock_postings` — else an upgraded DB with an
            // already-Complete clone graph would keep an empty postings table forever (review R2).
            || !live.postings_written
            // Same self-heal contract for the df epoch (#479, the R2 shape): a Complete
            // generation without its `clone_df_epoch` rows (the V051 backfill's empty-df edge, a
            // torn epoch) is unservable by the fast path and the delta — it must read as pending
            // so one rebuild re-pins it, not skip as current forever.
            || !clone_df_epoch_serves(conn, live.generation)?)
    }

    /// The background quiet-window gate for the clone-graph tail (#472): true when the graph is
    /// pending AND the content revision has been stable for `quiet_ms`. Under sustained editing
    /// `content_revision()` moves every pass, so an ungated tail discards the in-flight Building
    /// generation and rebuilds the whole graph from symbol 0 each time (measured ~1 GB of DB
    /// writes per pass); the gate defers the rebuild until the churn pauses. Bookkeeping lives in
    /// per-repo meta (`clone_graph_quiet_*`), so the watcher and the hook-driven CLI `maintenance`
    /// share one window across processes:
    /// - graph current → drop any armed candidate, not due;
    /// - stale revision != armed candidate (or nothing armed) → (re-)arm the window, not due;
    /// - stale revision == armed candidate for ≥ `quiet_ms` → due.
    ///
    /// `probe_without_candidate = false` lets an idle pass skip the probe — and its
    /// content-revision digest — entirely when no deferred rebuild is owed (nothing armed).
    /// `quiet_ms = 0` disables the gate (a pending graph is immediately due). Explicit rebuild
    /// paths (`clones --precompute`, full `index`) bypass the gate entirely.
    pub fn clone_graph_rebuild_due(
        &self,
        quiet_ms: i64,
        probe_without_candidate: bool,
    ) -> anyhow::Result<bool> {
        Ok(self.clone_graph_rebuild_probe(quiet_ms, probe_without_candidate)?.due)
    }

    /// [`Self::clone_graph_rebuild_due`] returning the probe's full outcome (#821): the due
    /// verdict PLUS the content digest the probe computed, pinned to the connection state, so
    /// the same pass's clone delta can reuse the digest instead of re-scanning `main.files` —
    /// see [`Self::apply_clone_graph_delta_reusing_revision`].
    pub(crate) fn clone_graph_rebuild_probe(
        &self,
        quiet_ms: i64,
        probe_without_candidate: bool,
    ) -> anyhow::Result<CloneGraphRebuildProbe> {
        self.clone_graph_rebuild_probe_at(now_ms(), quiet_ms, probe_without_candidate)
    }

    /// [`Self::clone_graph_rebuild_due`] with the clock injected (tests).
    #[cfg(test)]
    pub(crate) fn clone_graph_rebuild_due_at(
        &self,
        now_ms: i64,
        quiet_ms: i64,
        probe_without_candidate: bool,
    ) -> anyhow::Result<bool> {
        Ok(self.clone_graph_rebuild_probe_at(now_ms, quiet_ms, probe_without_candidate)?.due)
    }

    /// [`Self::clone_graph_rebuild_probe`] with the clock injected.
    ///
    /// "Owed" here means a FULL rebuild: the graph is stale in a way the #473 delta can't settle
    /// (absent / normalizer bump / postings gap), OR the live generation has absorbed enough
    /// delta files ([`CLONE_GRAPH_DRIFT_REBUILD_FILES`]) that its frozen df epoch owes a refresh.
    /// A merely revision-stale-but-delta-eligible graph also arms here — if the delta settles it
    /// first, the next probe sees it current and disarms.
    fn clone_graph_rebuild_probe_at(
        &self,
        now_ms: i64,
        quiet_ms: i64,
        probe_without_candidate: bool,
    ) -> anyhow::Result<CloneGraphRebuildProbe> {
        let candidate = self.clone_graph_quiet_candidate()?;
        if candidate.is_none() && !probe_without_candidate {
            return Ok(CloneGraphRebuildProbe::default());
        }
        // Captured BEFORE the digest: brackets it against a cross-connection commit landing
        // between the digest read and the pin capture (see `pin_content_revision`).
        let data_version_before_digest = self.connection_data_version()?;
        let revision = self.content_revision()?;
        let drifted = {
            let conn = self.storage.connection();
            live_generation_row(conn)?
                .is_some_and(|live| live.delta_files_applied >= CLONE_GRAPH_DRIFT_REBUILD_FILES)
        };
        let due = if !self.clone_graph_stale_against(&revision)? && !drifted {
            if candidate.is_some() {
                self.clear_clone_graph_quiet_candidate()?;
            }
            false
        } else if quiet_ms == 0 {
            true
        } else {
            match candidate {
                Some((armed_revision, since_ms)) if armed_revision == revision =>
                    now_ms.saturating_sub(since_ms) >= quiet_ms,
                _ => {
                    self.set_repo_meta(CLONE_GRAPH_QUIET_REVISION_META, &revision)?;
                    self.set_repo_meta(CLONE_GRAPH_QUIET_SINCE_META, &now_ms.to_string())?;
                    false
                },
            }
        };
        // Pin AFTER the gate's own bookkeeping writes (arming or clearing the quiet candidate):
        // those touch repo_meta only, never `files`, but they bump `total_changes()` — pinning
        // before them would make the reuse check read the probe's own writes as an intervening
        // mutation and never fire. The pre-digest `data_version` bracket still guards the whole
        // window against other connections.
        let revision = self.pin_content_revision(revision, data_version_before_digest)?;
        Ok(CloneGraphRebuildProbe { due, revision })
    }

    /// Whether the #472 quiet gate holds an armed candidate. Watch-level tests assert the #817
    /// posture through this (an overlay-only pass neither probes nor arms) — `repo_meta` is not
    /// reachable from outside the `index` module tree.
    #[cfg(test)]
    pub(crate) fn clone_graph_quiet_candidate_armed(&self) -> bool {
        self.clone_graph_quiet_candidate().ok().flatten().is_some()
    }

    /// The armed quiet-window candidate, if any: the stale revision under observation and when it
    /// was first seen. A torn/corrupt pair reads as absent (the next probe re-arms it).
    fn clone_graph_quiet_candidate(&self) -> anyhow::Result<Option<(String, i64)>> {
        let Some(revision) = self.repo_meta(CLONE_GRAPH_QUIET_REVISION_META)? else {
            return Ok(None);
        };
        let since_ms = self
            .repo_meta(CLONE_GRAPH_QUIET_SINCE_META)?
            .and_then(|since| since.parse::<i64>().ok());
        Ok(since_ms.map(|since_ms| (revision, since_ms)))
    }

    fn clear_clone_graph_quiet_candidate(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        rag_rat_db::meta::delete_repo_meta(
            conn,
            &self.active_repo_id,
            CLONE_GRAPH_QUIET_REVISION_META,
        )?;
        rag_rat_db::meta::delete_repo_meta(
            conn,
            &self.active_repo_id,
            CLONE_GRAPH_QUIET_SINCE_META,
        )?;
        Ok(())
    }

    /// The live clone-graph generation the write-time postings fast path may read from —
    /// `Some(gen)` ONLY when the persisted postings are safe to serve, `None` otherwise (→ the
    /// caller uses the RAM fallback). Eligibility is EXACT-freshness, deliberately STRICTER
    /// than the `find_clones` edge fast path's "mildly-stale-OK" (review R1): the postings cover
    /// exactly the file set of `source_revision`, so a generation drifted from
    /// `content_revision()` could disagree with what the live index would compute — a silent
    /// missed near-clone. So require:
    /// - a `Complete` live generation (the meta live-pointer only ever names a Complete one),
    /// - `normalizer_version == NORM_VERSION`,
    /// - `postings_written` (a postings-complete, postings-aware generation — review R2),
    /// - `source_revision == content_revision()` EXACTLY (not merely present), AND
    /// - the generation's `clone_df_epoch` rows exist (#479 — the order to read the postings by).
    pub fn clone_check_indexed_generation(&self) -> anyhow::Result<Option<i64>> {
        // BASE-SCOPE ONLY. The clone graph (edges + postings) is built in the BASE scope —
        // maintenance restores it before the clone-graph pass, and `content_revision()` is GLOBAL
        // over `main.files` so it CANNOT encode which scope produced the postings. Under a
        // linked-worktree OVERLAY the postings cover only base-scope symbols, while the RAM
        // fallback reads the overlay's branch-only symbols; serving the fast path there
        // would silently miss overlay near-clones. So disable it under a linked overlay —
        // those scopes fall back to the correct, overlay-scoped RAM build.
        if self.active_scope_is_linked_overlay() {
            return Ok(None);
        }
        let conn = self.storage.connection();
        let Some(live) = live_generation_row(conn)? else {
            return Ok(None);
        };
        let eligible = live.normalizer_version == NORM_VERSION
            && live.postings_written
            && live.source_revision == self.content_revision()?
            // #479: the postings are ordered by the generation's pinned epoch; without the epoch
            // rows (a pre-V051 build the backfill could not cover) the reader cannot reproduce
            // that order — fall back rather than silently miss near-clones.
            && clone_df_epoch_serves(conn, live.generation)?;
        Ok(eligible.then_some(live.generation))
    }

    /// ONE precompute pass: resume (or start) the building generation toward the current content
    /// revision, stream symbols from the resume cursor emitting verified clone edges, checkpoint
    /// per batch, and — if the walk finishes within budget — publish the generation as live.
    /// Mirrors the embedding `reconcile` pass shape (skip-when-current, budgeted batch loop,
    /// `Partial`/`Complete`).
    pub(crate) fn reconcile_clone_edges_pass(
        &self,
        options: &CloneEdgeOptions,
    ) -> anyhow::Result<CloneEdgeReport> {
        let started = Instant::now();
        let conn = self.storage.connection();
        let source_revision = self.content_revision()?;

        // Phase 0 — skip-when-current: a live generation built against the current revision +
        // normalizer is already fresh, so do nothing (no write lock churn on an idle repo).
        if !options.force
            && let Some(live) = live_generation_row(conn)?
            && live.source_revision == source_revision
            && live.normalizer_version == NORM_VERSION
            // Also require postings-completeness, so an upgrade from a pre-postings graph does not
            // skip forever with an empty `clone_subblock_postings` (review R2). A postings-less
            // live generation falls through and rebuilds a postings-full one.
            && live.postings_written
            // And the pinned df epoch (#479, same R2 shape): an epoch-less Complete generation is
            // unservable by the fast path and the delta, so skipping-as-current would strand them
            // on the fallback forever — fall through and rebuild one that pins its epoch.
            && clone_df_epoch_serves(conn, live.generation)?
            // #473: a generation that has absorbed enough in-place delta files owes a df-epoch
            // refresh — it is FRESH (deltas keep `source_revision` current) but must not
            // skip-as-current, or the drift rebuild the quiet gate scheduled would no-op forever.
            && live.delta_files_applied < CLONE_GRAPH_DRIFT_REBUILD_FILES
        {
            return Ok(CloneEdgeReport {
                status: "Current".to_string(),
                generation: live.generation,
                symbols_total: 0,
                symbols_processed: 0,
                edges_written: live.edges_written,
                source_revision,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }

        // Phase 1 — open or resume a Building generation toward THIS revision. A Building
        // generation toward a different revision (a reindex landed since it started) is
        // discarded — its symbol-id cursor is meaningless against the new symbol rows.
        let building = open_building_generation(conn, &source_revision)?;
        // A FRESH build (cursor 0 ⇒ no symbol walked ⇒ zero postings staged for this generation)
        // moves the df epoch to now (#477 review): the #473 drift rebuild exists to restore
        // sub-block selectivity, so a full build that kept the old df would reset the drift
        // counter without delivering the refresh it promises — and more generally, every fresh
        // generation's postings must be ordered by the df of ITS OWN build. The refreshed df is
        // then pinned durably in `clone_df_epoch` (#479): that snapshot — not the live
        // `clone_token_df`, which moves on incremental passes — is what the delta pass and the
        // write-time fast path read back for this generation. A RESUMED partial (cursor > 0)
        // must NOT refresh or re-snapshot: its persisted postings are ordered by the epoch its
        // build opened under.
        if building.cursor_symbol_id == 0 {
            self.refresh_clone_token_df()?;
            snapshot_clone_df_epoch(conn, building.generation)?;
        }

        // Load the scoped baseline bags + the content anchors for every scoped symbol, and build
        // the struct-hash buckets + sub-block inverted index in RAM (rebuilt each pass —
        // cheap relative to the pair emission this avoids persisting postings for). Bags are
        // ordered by the generation's PINNED epoch (#479): identical to the just-refreshed live
        // df on a fresh build, and — the case that matters — the OPEN-time order on a resumed
        // partial, whose remaining postings must match the ones already staged even if the live
        // table moved between the paused passes (Codex review of this change).
        let epoch_df = load_clone_df_epoch(conn, building.generation)?;
        let bags = load_scoped_baseline_bags_with_df(conn, &epoch_df)?;
        let symbols_total = bags.len() as u64;
        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
        let anchors = resolve_symbol_anchors(conn)?;
        let struct_buckets = build_struct_hash_buckets(&bags);
        let inverted = build_sub_block_index(&bags, CLONE_PRECOMPUTE_THETA);

        let deadline = options.max_seconds.map(|s| started + Duration::from_secs(s));
        let mut cursor = building.cursor_symbol_id;
        let mut edges_written = building.edges_written;
        let mut processed: u64 = 0;
        let mut batch: Vec<EdgeRow> = Vec::with_capacity(options.batch_size);
        let mut postings: Vec<PostingGroup> = Vec::with_capacity(options.batch_size);
        let mut budget_tripped = false;

        for bag in bags.iter().filter(|b| b.symbol_id > building.cursor_symbol_id) {
            let s = bag.symbol_id;
            let Some(s_anchor) = anchors.get(&s) else { continue }; // unscoped/raced symbol — skip

            // Struct-hash exact partners (t > s, same (struct_hash, language)) — similarity 1.0, no
            // verify.
            let mut struct_partners: BTreeSet<i64> = BTreeSet::new();
            if let Some(ids) =
                struct_buckets.get(&(bag.struct_hash.as_str(), bag.language.as_str()))
            {
                for &t in ids {
                    if t > s
                        && let Some(t_anchor) = anchors.get(&t)
                    {
                        struct_partners.insert(t);
                        batch.push(make_edge(
                            s_anchor,
                            bag.token_len,
                            t_anchor,
                            by_id[&t].token_len,
                            bag.token_len,
                            1.0,
                            "struct_hash",
                        ));
                    }
                }
            }

            // Sub-block candidates (t > s, same language, sharing a sub-block token) — verified. A
            // pair already emitted as a struct-hash exact pair is skipped (it would
            // re-verify to sim 1.0).
            // This symbol's sub-block tokens, computed ONCE: they drive both candidate generation
            // (below) and the persisted postings (further below). They are exactly what
            // `build_sub_block_index` stores per symbol, so the persisted set is parity-identical.
            let sub_tokens = sub_block_tokens(bag, CLONE_PRECOMPUTE_THETA);
            let mut candidates: BTreeSet<i64> = BTreeSet::new();
            for token in &sub_tokens {
                if let Some(ids) = inverted.get(token) {
                    for &t in ids {
                        if t > s
                            && !struct_partners.contains(&t)
                            && by_id[&t].language == bag.language
                        {
                            candidates.insert(t);
                        }
                    }
                }
            }
            for t in candidates {
                let Some(t_anchor) = anchors.get(&t) else { continue };
                let other = by_id[&t];
                if verified_clone(bag, other, CLONE_PRECOMPUTE_THETA) {
                    let ov = overlap(bag, other);
                    let max_len = bag.token_len.max(other.token_len);
                    let sim = ov as f64 / max_len as f64;
                    batch.push(make_edge(
                        s_anchor,
                        bag.token_len,
                        t_anchor,
                        other.token_len,
                        ov,
                        sim,
                        "sub_block",
                    ));
                }
            }

            // Persist this symbol's sub-block postings, content-anchored, in the SAME generation as
            // its edges. Emitted for EVERY walked symbol — including one with zero verified
            // partners (no edges) — and staged BEFORE the cursor advances, so a
            // budget-split resume can never leave a walked symbol without postings
            // (review R6). Idempotent under the content-key PK.
            if !sub_tokens.is_empty() {
                postings.push(PostingGroup { anchor: s_anchor.clone(), tokens: sub_tokens });
            }

            processed += 1;
            cursor = s;

            // Flush on EITHER accumulator filling: postings are per-symbol (far more numerous than
            // per-pair edges), so a run of high-posting / low-edge symbols must still checkpoint
            // and bound RAM. Both accumulators flush together in one transaction with
            // the cursor.
            if batch.len() >= options.batch_size || postings.len() >= options.batch_size {
                edges_written += flush_batch(
                    conn,
                    building.generation,
                    &mut batch,
                    &mut postings,
                    cursor,
                    edges_written,
                )?;
                if let Some(dl) = deadline
                    && Instant::now() >= dl
                {
                    budget_tripped = true;
                    break;
                }
            }
        }
        // Flush the remainder + checkpoint the final cursor for this pass.
        edges_written += flush_batch(
            conn,
            building.generation,
            &mut batch,
            &mut postings,
            cursor,
            edges_written,
        )?;

        let status = if budget_tripped {
            "Partial"
        } else {
            // The walk reached the last symbol: publish this generation as live, GC the rest.
            self.complete_generation(building.generation, edges_written)?;
            "Complete"
        };

        Ok(CloneEdgeReport {
            status: status.to_string(),
            generation: building.generation,
            symbols_total,
            symbols_processed: processed,
            edges_written,
            source_revision,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// Mark a generation `Complete`, flip the live pointer to it (the atomic publish), and GC every
    /// other generation (CASCADE drops their edges). The `set_meta` flip is the last write, so a
    /// reader sees either the previous live generation or this one, never a half-built one.
    fn complete_generation(&self, generation: i64, edges_written: u64) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        conn.execute(
            "UPDATE clone_graph_generations
                SET status = 'Complete', finished_at_ms = ?1, edges_written = ?2,
                    postings_written = 1
              WHERE generation = ?3",
            params![now_ms(), edges_written as i64, generation],
        )?;
        self.set_repo_meta("clone_graph_live_generation", &generation.to_string())?;
        // GC superseded generations PER REPO (A5): repo-scoped so completing repo A's generation
        // never deletes (and CASCADE-wipes the edges/postings of) a sibling repo's generations —
        // the "clone precompute on repo A leaves repo B's generation untouched" contract. This
        // real `repo_id` predicate (from V042's `clone_graph_generations.repo_id`) SUPERSEDES the
        // A3 `multiple_real_repos` seam guard. `{repo_clause}` is empty pre-A5, restoring the
        // original global sweep.
        let repo_clause = clone_generation_scope_clause(conn)?;
        conn.execute(
            &format!("DELETE FROM clone_graph_generations WHERE generation != ?1{repo_clause}"),
            params![generation],
        )?;
        Ok(())
    }
}

/// The `find_clones` / `clones_for_symbol` READ fast path (Phase C): the verified candidate pairs
/// for the active scope, read from the persisted graph instead of recomputed — when one is
/// eligible. Returns `None` (→ caller falls back to the live `candidate_pairs_from_bags`) when:
/// - `theta < CLONE_PRECOMPUTE_THETA` (the persisted θ=0.7 set is a SUPERSET of any θ≥0.7 set, but
///   not of a wider θ<0.7 set), or
/// - no `Complete` generation is published, or
/// - the live generation was built under a different `NORM_VERSION`.
///
/// Otherwise it resolves every stored edge's content-anchored endpoints back to LIVE `symbol_id`s
/// by joining `files`/`symbols` on `(path, start_byte)` AND `files.sha256 = *_file_sha` — so a
/// deleted or edited endpoint does not resolve and its (now-stale) edge drops (the #248 read
/// discipline). It then SCOPE-filters to pairs whose both endpoints are in the active `by_id` bag
/// set, and θ-FILTERS with the exact `verified_clone` gate (`overlap >= ceil(theta * max_len)`) so
/// θ>0.7 reproduces the live result precisely (struct-hash edges carry `similarity = 1.0` /
/// `overlap = token_len`, so they survive every θ). A present-but-STALE generation
/// (content_revision drifted) is still served — the "mildly stale OK" contract; per-edge staleness
/// is dropped by the `file_sha` join.
pub(super) fn precomputed_pairs_if_eligible(
    conn: &Connection,
    by_id: &BTreeMap<i64, &SymbolBag>,
    theta: f64,
) -> anyhow::Result<Option<Vec<(i64, i64)>>> {
    if theta < CLONE_PRECOMPUTE_THETA {
        return Ok(None);
    }
    let Some(live) = live_generation_row(conn)? else {
        return Ok(None);
    };
    if live.normalizer_version != NORM_VERSION {
        return Ok(None);
    }

    // Resolve each stored edge's content-anchored endpoints to LIVE symbol ids IN RAM. A per-edge
    // 4-way SQL join (files×symbols, twice) is catastrophically slow at scale — measured SLOWER
    // than a full live recompute on net/ipv4 (the whole point is to be faster). Instead build a
    // `(path, start_byte) -> (symbol_id, live_sha)` index once from the scoped symbols, then look
    // up each endpoint: a `file_sha` mismatch (file edited since the build) or a missing key
    // (file deleted) drops the now-stale edge — the #248 read discipline, done in memory.
    let by_anchor = build_anchor_index(conn)?;

    let mut stmt = conn.prepare(
        "SELECT a_path, a_start_byte, a_file_sha, b_path, b_start_byte, b_file_sha,
                overlap, a_token_len, b_token_len
           FROM clone_edges WHERE build_generation = ?1",
    )?;
    let rows = stmt.query_map(params![live.generation], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })?;

    let mut pairs: Vec<(i64, i64)> = Vec::new();
    for row in rows {
        let (
            a_path,
            a_start_byte,
            a_file_sha,
            b_path,
            b_start_byte,
            b_file_sha,
            overlap,
            a_len,
            b_len,
        ) = row?;
        let Some((sa, live_sha_a)) = by_anchor.get(&(a_path, a_start_byte)) else { continue };
        if *live_sha_a != a_file_sha {
            continue; // endpoint's file edited since the build → stale edge drops
        }
        let Some((sb, live_sha_b)) = by_anchor.get(&(b_path, b_start_byte)) else { continue };
        if *live_sha_b != b_file_sha {
            continue;
        }
        let (sa, sb) = (*sa, *sb);
        // Scope guard: both endpoints must be in the active bag set (a multi-worktree `files` row
        // can't leak an out-of-scope symbol in).
        if !by_id.contains_key(&sa) || !by_id.contains_key(&sb) {
            continue;
        }
        let max_len = a_len.max(b_len);
        if overlap >= (theta * max_len as f64).ceil() as i64 {
            pairs.push((sa.min(sb), sa.max(sb)));
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    Ok(Some(pairs))
}

/// `(path, start_byte) -> (symbol_id, file_sha)` over the scoped non-generated symbols — the
/// inverse of [`resolve_symbol_anchors`], used by the read fast path to resolve content-anchored
/// edges in RAM (one scan + hash lookups) instead of a per-edge SQL join.
fn build_anchor_index(conn: &Connection) -> anyhow::Result<HashMap<(String, i64), (i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, f.path, s.start_byte, f.sha256
           FROM symbols s JOIN files f ON f.id = s.file_id
          WHERE f.generated = 0",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (r.get::<_, String>(1)?, r.get::<_, i64>(2)?),
            (r.get::<_, i64>(0)?, r.get::<_, String>(3)?),
        ))
    })?;
    let mut map: HashMap<(String, i64), (i64, String)> = HashMap::new();
    for row in rows {
        let (key, value) = row?;
        map.insert(key, value);
    }
    Ok(map)
}

/// The `(struct_hash, language) -> [symbol_id]` buckets — the exact key `add_struct_hash_pairs`
/// uses, so the emitted struct-hash pairs match the live path.
fn build_struct_hash_buckets(bags: &[SymbolBag]) -> BTreeMap<(&str, &str), Vec<i64>> {
    let mut buckets: BTreeMap<(&str, &str), Vec<i64>> = BTreeMap::new();
    for bag in bags {
        buckets
            .entry((bag.struct_hash.as_str(), bag.language.as_str()))
            .or_default()
            .push(bag.symbol_id);
    }
    buckets
}

/// The `token_hash -> [symbol_id]` inverted index over sub-block tokens — the same index
/// `sub_block_candidate_pairs` builds, so a symbol's candidates match the live path.
fn build_sub_block_index(bags: &[SymbolBag], theta: f64) -> BTreeMap<i64, Vec<i64>> {
    let mut inverted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for bag in bags {
        for token_hash in sub_block_tokens(bag, theta) {
            inverted.entry(token_hash).or_default().push(bag.symbol_id);
        }
    }
    inverted
}

/// Resolve every scoped, non-generated symbol to its reindex-stable content anchor `(path,
/// start_byte, file_sha)`. Mirrors `load_scoped_baseline_bags`'s `symbols JOIN files` + `generated
/// = 0` scope so the anchor set covers exactly the bag symbols.
fn resolve_symbol_anchors(conn: &Connection) -> anyhow::Result<BTreeMap<i64, Anchor>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, f.path, s.start_byte, f.sha256
           FROM symbols s
           JOIN files f ON f.id = s.file_id
          WHERE f.generated = 0",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            (row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?),
        ))
    })?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (id, anchor) = row?;
        map.insert(id, anchor);
    }
    Ok(map)
}

/// Build a content-anchored edge with endpoints in canonical `(path, start_byte)` order (the PK
/// order). `overlap`/`similarity` are symmetric; the per-endpoint `token_len`s follow the chosen
/// orientation.
pub(super) fn make_edge(
    s_anchor: &Anchor,
    s_token_len: i64,
    t_anchor: &Anchor,
    t_token_len: i64,
    overlap: i64,
    similarity: f64,
    edge_source: &'static str,
) -> EdgeRow {
    let s_key = (s_anchor.0.as_str(), s_anchor.1);
    let t_key = (t_anchor.0.as_str(), t_anchor.1);
    let ((a, a_len), (b, b_len)) = if s_key <= t_key {
        ((s_anchor, s_token_len), (t_anchor, t_token_len))
    } else {
        ((t_anchor, t_token_len), (s_anchor, s_token_len))
    };
    EdgeRow {
        a_path: a.0.clone(),
        a_start_byte: a.1,
        a_file_sha: a.2.clone(),
        b_path: b.0.clone(),
        b_start_byte: b.1,
        b_file_sha: b.2.clone(),
        overlap,
        a_token_len: a_len,
        b_token_len: b_len,
        similarity,
        edge_source,
    }
}

/// Insert a batch of edges AND the walked symbols' sub-block postings (both idempotent under resume
/// via `INSERT OR IGNORE` on their content-key PKs) and checkpoint the generation's cursor + edge
/// count — all in ONE transaction, so postings, edges, and the cursor advance atomically together
/// (review R6: a symbol's postings are durable before its symbol id is checkpointed as done).
/// Returns the EDGE rows actually inserted (dedup-ignored rows don't count).
fn flush_batch(
    conn: &Connection,
    generation: i64,
    batch: &mut Vec<EdgeRow>,
    postings: &mut Vec<PostingGroup>,
    cursor: i64,
    cumulative_edges: u64,
) -> anyhow::Result<u64> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> anyhow::Result<u64> {
        let inserted = insert_edge_rows(conn, generation, batch)?;
        insert_posting_groups(conn, generation, postings)?;
        conn.execute(
            "UPDATE clone_graph_generations SET cursor_symbol_id = ?1, edges_written = ?2
              WHERE generation = ?3",
            params![cursor, (cumulative_edges + inserted) as i64, generation],
        )?;
        Ok(inserted)
    })();
    match result {
        Ok(inserted) => {
            conn.execute_batch("COMMIT")?;
            batch.clear();
            postings.clear();
            Ok(inserted)
        },
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        },
    }
}

/// Insert edge rows for `generation` with the shared idempotent write discipline (`INSERT OR
/// IGNORE` on the content-key PK). Returns the rows actually inserted. Shared by `flush_batch`
/// (the full build) and the delta pass so the write shape lives in exactly one place. Runs inside
/// the CALLER's transaction.
pub(super) fn insert_edge_rows(
    conn: &Connection,
    generation: i64,
    batch: &[EdgeRow],
) -> anyhow::Result<u64> {
    let mut inserted = 0u64;
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO clone_edges
            (build_generation, a_path, a_start_byte, a_file_sha, b_path, b_start_byte,
             b_file_sha, overlap, a_token_len, b_token_len, similarity, edge_source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for e in batch {
        inserted += stmt.execute(params![
            generation,
            e.a_path,
            e.a_start_byte,
            e.a_file_sha,
            e.b_path,
            e.b_start_byte,
            e.b_file_sha,
            e.overlap,
            e.a_token_len,
            e.b_token_len,
            e.similarity,
            e.edge_source,
        ])? as u64;
    }
    Ok(inserted)
}

/// Insert every posting group's per-token rows for `generation` (idempotent, content-key PK).
/// Shared by `flush_batch` and the delta pass; runs inside the CALLER's transaction.
pub(super) fn insert_posting_groups(
    conn: &Connection,
    generation: i64,
    postings: &[PostingGroup],
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO clone_subblock_postings
            (build_generation, token_hash, path, start_byte, file_sha)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for g in postings {
        let (path, start_byte, file_sha) = &g.anchor;
        for &token_hash in &g.tokens {
            stmt.execute(params![generation, token_hash, path, start_byte, file_sha])?;
        }
    }
    Ok(())
}

/// Whether `generation`'s postings can be ORDERED for serving (#479): its pinned df epoch
/// exists, or there is nothing to order (a zero-postings generation — a docs-only or
/// fingerprint-less repo — has no order to lose, and refusing it would make `pending_clone_graph`
/// rebuild an already-current empty graph forever). Only "postings exist but the epoch is gone"
/// (a pre-V051 build the backfill's empty-df edge could not cover, a torn epoch) is unservable —
/// every eligibility gate treats that like `postings_written = 0` (fall back / refuse; one full
/// rebuild self-heals).
pub(super) fn clone_df_epoch_serves(conn: &Connection, generation: i64) -> anyhow::Result<bool> {
    if clone_df_epoch_exists(conn, generation)? {
        return Ok(true);
    }
    let has_postings = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM clone_subblock_postings WHERE build_generation = ?1)",
        params![generation],
        |r| r.get::<_, i64>(0),
    )? != 0;
    Ok(!has_postings)
}

/// The STRICT probe: `generation` has pinned epoch rows. The delta pass requires this — not the
/// [`clone_df_epoch_serves`] sentinel — because it may CREATE the generation's first postings,
/// and emitting them under an empty epoch map would leave postings no reader can order (Codex
/// review of this change). An epoch-less generation goes to the full-rebuild path, which pins one.
pub(super) fn clone_df_epoch_exists(conn: &Connection, generation: i64) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM clone_df_epoch WHERE build_generation = ?1)",
        params![generation],
        |r| r.get::<_, i64>(0),
    )? != 0)
}

/// Pin a FRESH generation's df order (#479): copy the active repo's just-refreshed
/// `clone_token_df` (baseline — the only normalizer kind the graph builds) into `clone_df_epoch`
/// for `generation`. Runs only when a Building generation opens fresh (cursor 0); a resumed
/// partial keeps the epoch it opened under, and CASCADE sweeps the rows with the generation.
/// DELETE-first: a torn pass that snapshotted, died before its first checkpoint, and re-entered
/// the fresh branch re-pins against the re-refreshed df instead of tripping the PK.
fn snapshot_clone_df_epoch(conn: &Connection, generation: i64) -> anyhow::Result<()> {
    conn.execute("DELETE FROM clone_df_epoch WHERE build_generation = ?1", params![generation])?;
    let df_scope = rag_rat_db::schema::periphery_repo_scope(conn, "clone_token_df")?;
    let df_clause = rag_rat_db::schema::periphery_repo_scope_clause(&df_scope, "clone_token_df");
    conn.execute(
        &format!(
            "INSERT INTO clone_df_epoch(build_generation, token_hash, df)
             SELECT ?1, token_hash, df FROM clone_token_df
             WHERE normalizer_kind = 'baseline'{df_clause}"
        ),
        params![generation],
    )?;
    Ok(())
}

/// The ` AND clone_graph_generations.repo_id = '…'` predicate for the per-repo generation SWEEPS,
/// or `""` on the pre-A5 schema (the generations table is still repo-global). The generation
/// INTEGER stays globally unique (allocated `MAX(generation)+1` over ALL repos), so the transitive
/// `clone_edges` / `clone_subblock_postings` are scoped for free by `build_generation`; only the
/// generation lifecycle sweeps (allocate/build/complete/invalidate) filter `repo_id` so a repo's
/// precompute never touches a sibling's generations. See `schema::periphery_repo_scope`.
pub(super) fn clone_generation_scope_clause(conn: &Connection) -> anyhow::Result<String> {
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "clone_graph_generations")?;
    Ok(rag_rat_db::schema::periphery_repo_scope_clause(&scope, "clone_graph_generations"))
}

/// The live (Complete) generation row, if one is published.
pub(super) fn live_generation_row(conn: &Connection) -> anyhow::Result<Option<GenerationRow>> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let Some(live) = rag_rat_db::meta::repo_meta(conn, &repo_id, "clone_graph_live_generation")?
    else {
        return Ok(None);
    };
    let Ok(generation) = live.parse::<i64>() else {
        return Ok(None);
    };
    read_generation(conn, generation)
}

/// Open the Building generation toward `source_revision`: resume it if it already targets this
/// revision + normalizer, otherwise discard any stale Building generation and allocate a fresh one.
fn open_building_generation(
    conn: &Connection,
    source_revision: &str,
) -> anyhow::Result<GenerationRow> {
    // Per-repo (A5): resume / discard only THIS repo's Building generation, via a real `repo_id`
    // predicate from V042's `clone_graph_generations.repo_id` — this SUPERSEDES the A3
    // `multiple_real_repos` seam guard that used to gate the whole resume/discard block.
    // `{repo_clause}` empty pre-A5. The MAX(generation) allocation below stays GLOBAL so the
    // generation integer is unique across repos (keeping the transitive edges/postings scoped by
    // build_generation).
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "clone_graph_generations")?;
    let repo_clause =
        rag_rat_db::schema::periphery_repo_scope_clause(&scope, "clone_graph_generations");
    let existing: Option<GenerationRow> = conn
        .query_row(
            &format!(
                "SELECT generation, source_revision, normalizer_version, cursor_symbol_id, \
                 edges_written, postings_written, delta_files_applied
                   FROM clone_graph_generations WHERE status = 'Building'{repo_clause}
                  ORDER BY generation DESC LIMIT 1"
            ),
            [],
            map_generation_row,
        )
        .ok();
    if let Some(row) = existing {
        if row.source_revision == source_revision
            && row.normalizer_version == NORM_VERSION
            // Resume only a POSTINGS-AWARE partial. A pre-feature Building generation
            // (`postings_written = 0`) has no postings for its already-walked symbols; resuming it
            // would Complete a generation with a permanent postings gap. Discard it instead so the
            // fresh generation below writes postings from symbol 0 (review R2).
            && row.postings_written
        {
            return Ok(row);
        }
        // Stale partial (a reindex landed) OR a pre-feature postings-less partial: discard it
        // (CASCADE drops its edges + postings) and start over.
        conn.execute(
            &format!("DELETE FROM clone_graph_generations WHERE status = 'Building'{repo_clause}"),
            [],
        )?;
    }
    let generation: i64 = conn.query_row(
        "SELECT COALESCE(MAX(generation), 0) + 1 FROM clone_graph_generations",
        [],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO clone_graph_generations
            (generation, status, theta_floor, normalizer_kind, normalizer_version, source_revision,
             cursor_symbol_id, edges_written, postings_written, started_at_ms)
         VALUES (?1, 'Building', ?2, 'baseline', ?3, ?4, 0, 0, 1, ?5)",
        params![generation, CLONE_PRECOMPUTE_THETA, NORM_VERSION, source_revision, now_ms()],
    )?;
    // Stamp the active repo on the just-allocated generation (A5). No-op pre-A5 (no repo_id
    // column).
    if let Some(repo_id) = &scope {
        conn.execute(
            "UPDATE clone_graph_generations SET repo_id = ?1 WHERE generation = ?2",
            params![repo_id, generation],
        )?;
    }
    Ok(GenerationRow {
        generation,
        source_revision: source_revision.to_string(),
        normalizer_version: NORM_VERSION,
        cursor_symbol_id: 0,
        edges_written: 0,
        postings_written: true,
        delta_files_applied: 0,
    })
}

fn read_generation(conn: &Connection, generation: i64) -> anyhow::Result<Option<GenerationRow>> {
    Ok(conn
        .query_row(
            "SELECT generation, source_revision, normalizer_version, cursor_symbol_id, \
             edges_written, postings_written, delta_files_applied
               FROM clone_graph_generations WHERE generation = ?1",
            params![generation],
            map_generation_row,
        )
        .ok())
}

fn map_generation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GenerationRow> {
    Ok(GenerationRow {
        generation: row.get(0)?,
        source_revision: row.get(1)?,
        normalizer_version: row.get(2)?,
        cursor_symbol_id: row.get(3)?,
        edges_written: row.get::<_, i64>(4)? as u64,
        postings_written: row.get::<_, i64>(5)? != 0,
        delta_files_applied: row.get(6)?,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// The config for a fixed two-file fixture with two renamed-clone groups
    /// (load_user/load_order and compute_totals/tally_amounts). Identical file CONTENT across tags
    /// → identical content-key edges, so two builds are directly comparable. Split out so a
    /// test can `rebuild` the SAME config twice (identical content) to exercise the
    /// full-rebuild df refresh.
    pub(in super::super) fn clone_fixture_config(tag: &str) -> rag_rat_base::config::Config {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-precompute-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\npub fn \
             compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in items { s += it * \
             2; } s + 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/b.rs"),
            "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 \
             }\npub fn tally_amounts(values: Vec<i64>) -> i64 { let mut t = 0; for v in values { \
             t += v * 2; } t + 1 }\n",
        )
        .unwrap();
        rag_rat_base::config::Config {
            trackers: Vec::new(),
            papertrail: Default::default(),
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![rag_rat_base::config::ResolvedTarget {
                name: "rust".to_string(),
                language: rag_rat_base::language::Language::Rust,
                directories: vec![std::path::PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: rag_rat_base::config::TargetKind::Source,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
        }
    }

    /// A fixed two-file fixture (see [`clone_fixture_config`]), rebuilt fresh.
    fn build_clone_fixture(tag: &str) -> crate::IndexDatabase {
        crate::IndexDatabase::rebuild(&clone_fixture_config(tag)).unwrap()
    }

    /// The content-key set of the live-or-only generation's edges, sorted — the build-stable
    /// identity of the persisted graph (symbol_id-independent).
    pub(in super::super) fn edge_keys(
        db: &crate::IndexDatabase,
    ) -> Vec<(String, i64, String, i64)> {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT a_path, a_start_byte, b_path, b_start_byte FROM clone_edges
                 ORDER BY a_path, a_start_byte, b_path, b_start_byte",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    #[test]
    fn precompute_writes_graph_and_skips_when_current() {
        // Asserts a whole-DB `clone_graph_generations` count; opt out of the poison harness whose
        // sibling seeds another repo's generation.
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let db = build_clone_fixture("write");
        let report = db.precompute_clone_graph(None).unwrap();
        assert_eq!(report.status, "Complete", "fresh precompute completes");
        assert!(!edge_keys(&db).is_empty(), "renamed-clone fixture writes edges");

        let conn = db.storage.connection();
        let live: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM repo_meta WHERE key = \
                 'clone_graph_live_generation'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM clone_graph_generations WHERE generation = ?1",
                [live],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "Complete", "the live generation is Complete");

        // Re-running on unchanged content is a skip-when-current no-op (no new generation).
        let again = db.precompute_clone_graph(None).unwrap();
        assert_eq!(again.status, "Current");
        let generations: i64 = conn
            .query_row("SELECT COUNT(*) FROM clone_graph_generations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(generations, 1, "skip-when-current adds no generation");
    }

    #[test]
    fn precompute_resume_matches_single_pass() {
        // Reference: one uninterrupted pass.
        let single = build_clone_fixture("single");
        single.precompute_clone_graph(None).unwrap();
        let expected = edge_keys(&single);
        assert!(!expected.is_empty());

        // Resumed: a per-symbol budget that trips after every batch forces many resumable passes.
        let resumed = build_clone_fixture("resume");
        let mut passes = 0;
        loop {
            let report = resumed
                .reconcile_clone_edges_pass(&CloneEdgeOptions {
                    max_seconds: Some(0),
                    batch_size: 1,
                    force: false,
                })
                .unwrap();
            passes += 1;
            assert!(passes < 10_000, "must converge");
            if report.status != "Partial" {
                assert_eq!(report.status, "Complete");
                break;
            }
        }
        assert!(passes >= 2, "a tiny budget forces multiple resumable passes, got {passes}");
        assert_eq!(
            edge_keys(&resumed),
            expected,
            "the resumed (checkpointed) graph equals the single-pass graph — the smaller-endpoint \
             partition is correct across checkpoints"
        );
    }

    /// A stable, symbol-id-independent projection of a `find_clones` result: each class as its
    /// sorted member refs, classes sorted. Equal projections ⇒ the same clone classes.
    fn class_projection(result: &crate::index::FindClonesResult) -> Vec<Vec<String>> {
        let mut classes: Vec<Vec<String>> = result
            .classes
            .iter()
            .map(|c| {
                let mut refs: Vec<String> = c.members.iter().map(|m| m.r#ref.clone()).collect();
                refs.sort();
                refs
            })
            .collect();
        classes.sort();
        classes
    }

    /// THE CORNERSTONE (#286 Phase C): `find_clones` served from the persisted graph is IDENTICAL
    /// to `find_clones` recomputed live, at θ = 0.7 (the precompute floor) and above (where the
    /// stored edges are θ-filtered). Proven on the same index: capture live first (no graph →
    /// live path), precompute, capture again (graph present → fast path), assert equal. This is
    /// what makes the fast path a pure optimization rather than a behavior change.
    #[test]
    fn find_clones_precomputed_matches_live() {
        use crate::index::FindClonesOptions;

        for theta in [0.7_f64, 0.8, 0.9] {
            let db = build_clone_fixture(&format!("parity-{}", (theta * 100.0) as i64));
            let opts =
                || FindClonesOptions { min_similarity: Some(theta), min_copies: None, limit: None };

            // No graph yet → live path.
            let live = class_projection(&db.find_clones(opts()).unwrap());
            assert!(!live.is_empty(), "renamed-clone fixture has classes at θ={theta}");

            // Build the graph → subsequent find_clones takes the fast path.
            assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
            let fast = class_projection(&db.find_clones(opts()).unwrap());

            assert_eq!(fast, live, "precomputed find_clones must equal live at θ={theta}");
        }
    }

    /// The content-key set of the live generation's postings, sorted — the build-stable identity of
    /// the persisted postings (symbol-id-independent, the postings analogue of [`edge_keys`]).
    fn posting_keys(db: &crate::IndexDatabase) -> Vec<(i64, String, i64, String)> {
        let conn = db.storage.connection();
        let mut stmt = conn
            .prepare(
                "SELECT token_hash, path, start_byte, file_sha FROM clone_subblock_postings
                 ORDER BY token_hash, path, start_byte, file_sha",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// PARITY (design §Invariants #2): every persisted posting resolves to a symbol that
    /// `build_sub_block_index` — the RAM index the live candidate-gen uses — places under the SAME
    /// token, and vice versa. Pinning the persisted set as a byte-for-byte mirror of the RAM index
    /// is what will make the Phase-C postings fast path return the same candidates as the fallback.
    #[test]
    fn precompute_postings_match_sub_block_index() {
        let db = build_clone_fixture("postings-parity");
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");

        let conn = db.storage.connection();
        // Expected: token_hash -> {symbol_id} from the in-RAM sub-block index over the scoped bags.
        let bags = load_scoped_baseline_bags(conn).unwrap();
        assert!(!bags.is_empty(), "the fixture has scoped bags");
        let expected: BTreeMap<i64, BTreeSet<i64>> =
            build_sub_block_index(&bags, CLONE_PRECOMPUTE_THETA)
                .into_iter()
                .map(|(token, ids)| (token, ids.into_iter().collect()))
                .collect();

        // Actual: token_hash -> {symbol_id} from the persisted postings, resolving each content
        // anchor (path, start_byte) back to its live symbol id (the read-path resolution shape).
        let by_anchor: HashMap<(String, i64), i64> = resolve_symbol_anchors(conn)
            .unwrap()
            .into_iter()
            .map(|(id, (path, start_byte, _sha))| ((path, start_byte), id))
            .collect();
        let mut actual: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
        let mut stmt = conn
            .prepare("SELECT token_hash, path, start_byte FROM clone_subblock_postings")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .unwrap();
        for row in rows {
            let (token, path, start_byte) = row.unwrap();
            actual.entry(token).or_default().insert(by_anchor[&(path, start_byte)]);
        }

        assert_eq!(actual, expected, "persisted postings mirror the RAM sub-block index exactly");
    }

    /// RESUME IDEMPOTENCY (review R6): a budget-split precompute (one symbol per pass) yields the
    /// SAME postings as a single uninterrupted pass. Guards the "postings staged before the cursor
    /// advances" contract — a split between "postings written" and "cursor advanced" would drop a
    /// symbol's postings on resume. Mirrors [`precompute_resume_matches_single_pass`] for edges.
    #[test]
    fn precompute_postings_resume_matches_single_pass() {
        let single = build_clone_fixture("postings-single");
        single.precompute_clone_graph(None).unwrap();
        let expected = posting_keys(&single);
        assert!(!expected.is_empty(), "the fixture writes postings");

        let resumed = build_clone_fixture("postings-resume");
        let mut passes = 0;
        loop {
            let report = resumed
                .reconcile_clone_edges_pass(&CloneEdgeOptions {
                    max_seconds: Some(0),
                    batch_size: 1,
                    force: false,
                })
                .unwrap();
            passes += 1;
            assert!(passes < 10_000, "must converge");
            if report.status != "Partial" {
                assert_eq!(report.status, "Complete");
                break;
            }
        }
        assert!(passes >= 2, "a tiny budget forces multiple resumable passes, got {passes}");
        assert_eq!(
            posting_keys(&resumed),
            expected,
            "the resumed (checkpointed) postings equal the single-pass postings"
        );
    }

    /// UPGRADE REPOPULATION (review R2): a DB whose clone graph was already `Complete` BEFORE
    /// postings existed (`postings_written = 0`, empty `clone_subblock_postings`) is treated as
    /// pending and rebuilt ONCE to fill the postings — instead of skip-when-current leaving the
    /// table empty forever. Self-correcting: no `content_revision` change or manual rebuild needed.
    #[test]
    fn precompute_repopulates_postings_on_upgrade() {
        let db = build_clone_fixture("postings-upgrade");
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");

        // Simulate the pre-feature on-disk state: a Complete live generation that predates
        // postings.
        db.storage
            .connection()
            .execute_batch(
                "UPDATE clone_graph_generations SET postings_written = 0;
                 DELETE FROM clone_subblock_postings;",
            )
            .unwrap();
        assert!(db.pending_clone_graph().unwrap(), "a postings-less live generation is pending");

        // One reconcile pass rebuilds a postings-full generation and clears the pending state.
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        let postings: i64 = db
            .storage
            .connection()
            .query_row("SELECT COUNT(*) FROM clone_subblock_postings", [], |r| r.get(0))
            .unwrap();
        assert!(postings > 0, "the upgrade rebuild fills clone_subblock_postings");
        assert!(!db.pending_clone_graph().unwrap(), "no longer pending after the rebuild");
    }

    /// The background quiet-window gate (#472): a pending clone graph does NOT fire a rebuild on
    /// first observation — the probe ARMS the window by recording the stale revision — and fires
    /// only once that revision has stayed stable past the window. This is what stops sustained
    /// editing from treadmilling full-generation rebuilds on every watcher/maintenance pass.
    #[test]
    fn clone_graph_quiet_gate_arms_then_fires_after_the_window() {
        let db = build_clone_fixture("quiet-gate-arms");
        assert!(db.pending_clone_graph().unwrap(), "fresh fixture has no generation yet");
        assert!(
            !db.clone_graph_rebuild_due_at(1_000, 300_000, true).unwrap(),
            "first observation arms the window instead of firing"
        );
        assert!(
            !db.clone_graph_rebuild_due_at(1_000 + 299_999, 300_000, true).unwrap(),
            "still inside the window"
        );
        assert!(
            db.clone_graph_rebuild_due_at(1_000 + 300_000, 300_000, true).unwrap(),
            "a stable revision past the window fires"
        );
    }

    /// Content moving while armed re-arms the window for the NEW revision: sustained editing keeps
    /// deferring (the treadmill fix), and only a revision that stays put for the full window fires.
    #[test]
    fn clone_graph_quiet_gate_rearms_when_the_revision_moves() {
        let config = clone_fixture_config("quiet-gate-rearm");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert!(!db.clone_graph_rebuild_due_at(1_000, 300_000, true).unwrap(), "arm");
        drop(db);

        // Edit a fixture file and re-index so `content_revision()` moves past the armed candidate
        // (the armed `clone_graph_quiet_*` repo_meta survives the rebuild, like the live-generation
        // pointer does).
        let a = config.root.join("src/a.rs");
        let mut text = std::fs::read_to_string(&a).unwrap();
        text.push_str("pub fn freshly_added(x: i32) -> i32 { x + 41 }\n");
        std::fs::write(&a, text).unwrap();
        let db = crate::IndexDatabase::rebuild(&config).unwrap();

        assert!(
            !db.clone_graph_rebuild_due_at(10_000_000, 300_000, true).unwrap(),
            "a moved revision re-arms instead of firing, however long the old candidate sat"
        );
        assert!(
            db.clone_graph_rebuild_due_at(10_000_000 + 300_000, 300_000, true).unwrap(),
            "the new revision fires once it has been stable for the window"
        );
    }

    /// `probe_without_candidate = false` (an idle watcher pass with no deferred rebuild owed)
    /// skips the probe entirely — nothing is armed, so an idle server never pays the
    /// content-revision digest for the gate.
    #[test]
    fn clone_graph_quiet_gate_skips_the_probe_without_a_candidate() {
        let db = build_clone_fixture("quiet-gate-cold");
        assert!(
            !db.clone_graph_rebuild_due_at(1_000, 300_000, false).unwrap(),
            "no candidate + no probe permission means not due"
        );
        // The cold call did no bookkeeping: a later probing call still only ARMS.
        assert!(
            !db.clone_graph_rebuild_due_at(50_000_000, 300_000, true).unwrap(),
            "the probing call after a cold one arms fresh"
        );
        assert!(db.clone_graph_rebuild_due_at(50_000_000 + 300_000, 300_000, true).unwrap());
    }

    /// An ARMED candidate bypasses `probe_without_candidate = false`: an overlay-only or idle
    /// pass — which carries no probe permission since #817 — still fires a quiet-elapsed owed
    /// rebuild. Arming is what needs permission (a content/gc/backlog pass); firing is not.
    #[test]
    fn clone_graph_quiet_gate_fires_an_armed_candidate_without_probe_permission() {
        let db = build_clone_fixture("quiet-gate-armed-fires");
        assert!(!db.clone_graph_rebuild_due_at(1_000, 300_000, true).unwrap(), "arm");
        assert!(
            !db.clone_graph_rebuild_due_at(2_000, 300_000, false).unwrap(),
            "armed but not quiet-elapsed: not due"
        );
        assert!(
            db.clone_graph_rebuild_due_at(1_000 + 300_000, 300_000, false).unwrap(),
            "a quiet-elapsed armed candidate fires on a pass with no probe permission"
        );
    }

    /// Once the graph is current the gate reports not-due and drops the armed candidate, so idle
    /// passes go back to the cheap no-candidate path.
    #[test]
    fn clone_graph_quiet_gate_clears_once_current() {
        let db = build_clone_fixture("quiet-gate-clear");
        assert!(!db.clone_graph_rebuild_due_at(1_000, 300_000, true).unwrap(), "arm");
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        assert!(
            !db.clone_graph_rebuild_due_at(90_000_000, 300_000, true).unwrap(),
            "a current graph is never due, regardless of the armed candidate's age"
        );
        let leftover: i64 = db
            .storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM repo_meta WHERE key LIKE 'clone_graph_quiet_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0, "the armed candidate is dropped once the graph is current");
    }

    /// #479: a FRESH Building generation pins its build-time df order durably in
    /// `clone_df_epoch`, so the persisted postings survive later movement of the live
    /// `clone_token_df`. A RESUMED partial must not re-snapshot — its postings are ordered by the
    /// epoch its build opened under, and a mid-build df movement must not leak in.
    #[test]
    fn a_fresh_build_snapshots_the_df_epoch_and_a_resume_preserves_it() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let epoch_rows = |db: &crate::IndexDatabase, generation: i64| -> Vec<(i64, i64)> {
            let conn = db.storage.connection();
            let mut stmt = conn
                .prepare(
                    "SELECT token_hash, df FROM clone_df_epoch WHERE build_generation = ?1
                     ORDER BY token_hash",
                )
                .unwrap();
            stmt.query_map([generation], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let df_rows = |db: &crate::IndexDatabase| -> Vec<(i64, i64)> {
            let conn = db.storage.connection();
            let mut stmt = conn
                .prepare(
                    "SELECT token_hash, df FROM clone_token_df WHERE normalizer_kind = 'baseline'
                     ORDER BY token_hash",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };

        let db = build_clone_fixture("df-epoch-fresh");
        let report = db.precompute_clone_graph(None).unwrap();
        assert_eq!(report.status, "Complete");
        let fresh_epoch = epoch_rows(&db, report.generation);
        assert!(!fresh_epoch.is_empty(), "a fresh build snapshots its df epoch");
        assert_eq!(fresh_epoch, df_rows(&db), "the snapshot equals the df the build ran under");

        // Resume: trip the budget so a Building generation persists, move the live df between
        // passes (what an interleaved incremental bump does), and finish the build.
        let resumed = build_clone_fixture("df-epoch-resume");
        let first = resumed
            .reconcile_clone_edges_pass(&CloneEdgeOptions {
                max_seconds: Some(0),
                batch_size: 1,
                force: false,
            })
            .unwrap();
        assert_eq!(first.status, "Partial", "a zero-second budget trips after one batch");
        let open_epoch = epoch_rows(&resumed, first.generation);
        assert!(!open_epoch.is_empty(), "the epoch is pinned when the generation opens");
        // Adversarial mid-build live-df movement: invert the whole table between the paused
        // passes (an incremental bump storm's worst case).
        resumed
            .storage
            .connection()
            .execute("UPDATE clone_token_df SET df = 1000000 - df", [])
            .unwrap();
        let mut passes = 0;
        let completed = loop {
            let report = resumed
                .reconcile_clone_edges_pass(&CloneEdgeOptions {
                    max_seconds: Some(0),
                    batch_size: 1,
                    force: false,
                })
                .unwrap();
            passes += 1;
            assert!(passes < 10_000, "must converge");
            if report.status != "Partial" {
                assert_eq!(report.status, "Complete");
                break report;
            }
        };
        assert_eq!(
            epoch_rows(&resumed, completed.generation),
            open_epoch,
            "a resume preserves the open-time epoch — the mid-build df movement must not leak in"
        );
        // And the resumed passes must EMIT under that epoch too (Codex review): every persisted
        // posting — including the symbols walked after the inversion — matches the sub-block
        // selection under the pinned epoch, not the moved live table.
        let conn = resumed.storage.connection();
        let epoch_df = load_clone_df_epoch(conn, completed.generation).unwrap();
        let live_df = super::super::substrate::load_current_clone_df(conn).unwrap();
        let union_under = |df: &std::collections::HashMap<i64, i64>| -> BTreeSet<i64> {
            load_scoped_baseline_bags_with_df(conn, df)
                .unwrap()
                .iter()
                .flat_map(|bag| sub_block_tokens(bag, CLONE_PRECOMPUTE_THETA))
                .collect()
        };
        let under_epoch = union_under(&epoch_df);
        assert_ne!(
            under_epoch,
            union_under(&live_df),
            "precondition: the inversion must actually change the prefix selection"
        );
        let persisted: BTreeSet<i64> = conn
            .prepare(
                "SELECT DISTINCT token_hash FROM clone_subblock_postings
                 WHERE build_generation = ?1",
            )
            .unwrap()
            .query_map(params![completed.generation], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            persisted, under_epoch,
            "resumed passes emit postings under the pinned epoch, not the moved live df"
        );
    }

    /// #479 empty-graph sentinel (Codex review): a repo with no baseline fingerprints (docs-only,
    /// data-only) builds a Complete generation with ZERO postings and therefore ZERO epoch rows —
    /// a legitimately empty order, not a lost one. It must read as current, or
    /// `pending_clone_graph` would schedule a rebuild of the already-current empty graph on every
    /// maintenance pass, forever.
    #[test]
    fn an_empty_generation_without_epoch_rows_stays_current() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let mut config = clone_fixture_config("df-epoch-empty");
        // Replace the clone fixture's sources with a data-only file: no functions, so nothing is
        // fingerprinted and the built graph is empty.
        std::fs::remove_file(config.root.join("src/a.rs")).unwrap();
        std::fs::remove_file(config.root.join("src/b.rs")).unwrap();
        std::fs::write(config.root.join("src/data.rs"), "pub struct OnlyData { pub x: i64 }\n")
            .unwrap();
        config.allow_empty = true;
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        assert!(
            !db.pending_clone_graph().unwrap(),
            "an empty generation with no epoch rows is current, not perpetually pending"
        );
        assert_eq!(
            db.precompute_clone_graph(None).unwrap().status,
            "Current",
            "and the next pass skips instead of rebuilding the empty graph forever"
        );
        drop(db);

        // The FIRST fingerprinted content arrives: the delta must REFUSE (it would otherwise
        // write the generation's first postings under an empty epoch map — postings no reader
        // could order; Codex review) and the full path builds a fresh, epoch-pinned generation.
        std::fs::write(
            config.root.join("src/first.rs"),
            "pub fn first_function(q: i64) -> i64 { q * 13 + 1 }\n",
        )
        .unwrap();
        let (db, _changed) = crate::IndexDatabase::index_discover_reporting(&config).unwrap();
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(
            report.status, "NotEligible",
            "the delta must not create first postings on an epoch-less generation: {report:?}"
        );
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        assert!(
            db.clone_check_indexed_generation().unwrap().is_some(),
            "the full rebuild pins an epoch and the fast path serves"
        );
    }

    /// #479 upgrade defense: postings without their generation's epoch rows cannot be ordered
    /// correctly (the reader would fall to DF_FALLBACK for every token — a silently different
    /// order than the postings were built under). A missing epoch must behave like
    /// `postings_written = 0`: the fast path falls back and the delta refuses, so one full
    /// rebuild self-heals instead of silently losing recall.
    #[test]
    fn a_generation_without_epoch_rows_is_not_servable() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let db = build_clone_fixture("df-epoch-eligibility");
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        assert!(
            db.clone_check_indexed_generation().unwrap().is_some(),
            "a fresh build with its epoch serves the fast path"
        );
        db.storage.connection().execute("DELETE FROM clone_df_epoch", []).unwrap();
        assert!(
            db.clone_check_indexed_generation().unwrap().is_none(),
            "an epoch-less generation must not serve the postings fast path"
        );
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(
            report.status, "NotEligible",
            "the delta must not patch postings whose build order is unknown: {report:?}"
        );
        // The self-heal loop must CLOSE (Codex review of this change): the unservable state has
        // to read as pending and rebuild on the next pass — not skip as "Current" forever, which
        // would strand the fast path and the delta on the fallback.
        assert!(
            db.pending_clone_graph().unwrap(),
            "an epoch-less generation reads as pending, scheduling the healing rebuild"
        );
        let heal = db.precompute_clone_graph(None).unwrap();
        assert_eq!(heal.status, "Complete", "the pass rebuilds instead of skipping as current");
        assert!(
            db.clone_check_indexed_generation().unwrap().is_some(),
            "the rebuilt generation pins a fresh epoch and serves again"
        );
    }

    /// `quiet_ms = 0` disables the gate — a pending graph is immediately due (the pre-#472
    /// immediate-rebuild behavior).
    #[test]
    fn clone_graph_quiet_gate_zero_window_fires_immediately() {
        let db = build_clone_fixture("quiet-gate-zero");
        assert!(db.clone_graph_rebuild_due_at(1_000, 0, true).unwrap());
    }

    /// #479: incremental passes bump the LIVE `clone_token_df` (a new file's tokens get real df
    /// instead of riding `DF_FALLBACK` until the next full build — the live-fallback recall fix),
    /// while the published generation's PINNED epoch stays byte-identical and its postings stay
    /// servable. This inverts the #473 whole-table freeze: the freeze now lives per generation in
    /// `clone_df_epoch`, not on the live table.
    #[test]
    fn incremental_index_bumps_live_df_and_keeps_the_generation_epoch_frozen() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("df-live-bump");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        let report = db.precompute_clone_graph(None).unwrap();
        assert_eq!(report.status, "Complete");
        let generation = report.generation;
        let df_rows = |db: &crate::IndexDatabase| -> Vec<(i64, i64)> {
            let conn = db.storage.connection();
            let mut stmt = conn
                .prepare("SELECT token_hash, df FROM clone_token_df ORDER BY token_hash")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let epoch_rows = |db: &crate::IndexDatabase| -> Vec<(i64, i64)> {
            let conn = db.storage.connection();
            let mut stmt = conn
                .prepare(
                    "SELECT token_hash, df FROM clone_df_epoch WHERE build_generation = ?1
                     ORDER BY token_hash",
                )
                .unwrap();
            stmt.query_map([generation], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let live_df = df_rows(&db);
        let pinned_epoch = epoch_rows(&db);
        assert!(!live_df.is_empty(), "the full rebuild computed the df");
        assert_eq!(live_df, pinned_epoch, "at build time the live df IS the epoch");
        drop(db);

        // A new file with brand-new tokens through the watcher/maintenance incremental path.
        std::fs::write(
            config.root.join("src/live_probe.rs"),
            "pub fn live_bump_probe(zx: u64) -> u64 { zx.rotate_left(9) ^ 0xfeed_beef }\n",
        )
        .unwrap();
        crate::watch::maintenance_pass(&config, false).unwrap();

        let db = crate::IndexDatabase::open_config(&config).unwrap();
        assert!(
            df_rows(&db).len() > live_df.len(),
            "the incremental pass bumps the live df with the new file's tokens"
        );
        assert_eq!(
            epoch_rows(&db),
            pinned_epoch,
            "the generation's pinned epoch never moves after its build"
        );
        assert!(
            db.clone_check_indexed_generation().unwrap().is_some(),
            "the live df movement must not invalidate the generation's postings (they are \
             epoch-pinned; the maintenance pass's delta keeps them fresh)"
        );
    }

    /// End-to-end through the watcher pass (#472): a content-changing maintenance pass ARMS the
    /// gate and DEFERS the clone rebuild; once the armed candidate has sat past the quiet window,
    /// an otherwise-idle pass picks the owed rebuild up (the gate is also a tail trigger) and
    /// completes it.
    #[test]
    fn maintenance_pass_defers_the_clone_rebuild_until_the_quiet_window() {
        // Asserts whole-DB `clone_graph_generations` counts; opt out of the poison harness whose
        // sibling seeds another repo's generation.
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("quiet-gate-pass");
        drop(crate::IndexDatabase::rebuild(&config).unwrap());

        // A content change lands, then a maintenance pass runs while the window is still open: it
        // must arm and defer (no generation built), not discard-and-rebuild.
        let a = config.root.join("src/a.rs");
        let mut text = std::fs::read_to_string(&a).unwrap();
        text.push_str("pub fn freshly_edited(x: i32) -> i32 { x * 3 }\n");
        std::fs::write(&a, text).unwrap();
        crate::watch::maintenance_pass(&config, false).unwrap();

        let db = crate::IndexDatabase::open_config(&config).unwrap();
        let generations: i64 = db
            .storage
            .connection()
            .query_row("SELECT COUNT(*) FROM clone_graph_generations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(generations, 0, "a pass inside the quiet window defers the clone rebuild");

        // Backdate the armed candidate past the window, then run an IDLE pass: the owed rebuild
        // is now due, forces the otherwise-skipped tail, and completes.
        db.storage
            .connection()
            .execute(
                "UPDATE repo_meta SET value = '1' WHERE key = \
                 'clone_graph_quiet_candidate_since_ms'",
                [],
            )
            .unwrap();
        drop(db);
        crate::watch::maintenance_pass(&config, false).unwrap();

        let db = crate::IndexDatabase::open_config(&config).unwrap();
        let complete: i64 = db
            .storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM clone_graph_generations WHERE status = 'Complete'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(complete, 1, "the quiet-elapsed pass builds the graph to completion");
        assert!(!edge_keys(&db).is_empty(), "and the fixture's clone edges are persisted");
    }

    /// A second FULL index rebuild over identical content leaves the clone graph transiently
    /// pending — `content_revision()` digests raw `main.files`, and the freshly staged file
    /// generation coexists with the superseded one until gc, so the digest moves — and one
    /// precompute settles it. #479 note: the df refresh the rebuild runs no longer invalidates
    /// anything (`invalidate_clone_graph_postings` is gone — postings are pinned to their own
    /// `clone_df_epoch`); the pending window here is purely the revision-key movement.
    #[test]
    fn a_second_full_rebuild_leaves_the_graph_pending_until_one_precompute() {
        let config = clone_fixture_config("df-refresh");
        let db1 = crate::IndexDatabase::rebuild(&config).unwrap();
        db1.precompute_clone_graph(None).unwrap();
        assert!(!db1.pending_clone_graph().unwrap(), "a fresh precompute is current");
        assert!(
            db1.clone_check_indexed_generation().unwrap().is_some(),
            "and the write-time fast path is eligible"
        );
        drop(db1); // release the DB file before the second rebuild takes the write lock

        // Second FULL rebuild over IDENTICAL content: the clone graph survives (it is content-
        // anchored), but the staged file generation moves the revision key until gc.
        let db2 = crate::IndexDatabase::rebuild(&config).unwrap();
        assert!(
            db2.pending_clone_graph().unwrap(),
            "the staged-generation revision drift leaves the graph pending"
        );
        assert!(
            db2.clone_check_indexed_generation().unwrap().is_none(),
            "and the write-time fast path falls back to RAM until the graph settles"
        );

        // Settles on the next precompute (a maintenance pass's delta would re-pin it likewise).
        assert_eq!(db2.precompute_clone_graph(None).unwrap().status, "Complete");
        assert!(!db2.pending_clone_graph().unwrap(), "current again after the rebuild");
        assert!(db2.clone_check_indexed_generation().unwrap().is_some(), "eligible again");
    }

    // --- #413 finding #6: the global clone-generation cleanups are guarded on a multi-repo DB
    // (`clone_graph_generations` has no `repo_id` until the V042 seam). ---

    /// Register two REAL repos so `schema::multiple_real_repos` reports the consolidated shape. The
    /// fixture DB is non-git (unadopted), so its registry holds only the placeholder; inserting the
    /// rows directly is the white-box multi-repo construction (`register_repo` refuses a second
    /// real repo until A7).
    fn make_multi_repo(conn: &rusqlite::Connection) {
        conn.execute_batch(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-x', 'x', 0);
             INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-y', 'y', 0);",
        )
        .unwrap();
    }

    /// Insert a `clone_graph_generations` row in `status` toward `source_revision`.
    fn seed_generation(
        conn: &rusqlite::Connection,
        generation: i64,
        status: &str,
        revision: &str,
        repo_id: &str,
    ) {
        // Stamp `repo_id` explicitly: since V042 `complete_generation` / `open_building_generation`
        // scope by `clone_graph_generations.repo_id` (superseding the old `multiple_real_repos`
        // guard), a "sibling" generation must carry a DIFFERENT id than the active repo for these
        // tests to exercise the per-repo predicate.
        conn.execute(
            "INSERT INTO clone_graph_generations
                (generation, status, theta_floor, normalizer_kind, normalizer_version,
                 source_revision, cursor_symbol_id, edges_written, postings_written, started_at_ms,
                 repo_id)
             VALUES (?1, ?2, 0.0, 'baseline', ?3, ?4, 0, 0, 1, 0, ?5)",
            params![generation, status, NORM_VERSION, revision, repo_id],
        )
        .unwrap();
    }

    fn generation_count(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM clone_graph_generations", [], |r| r.get(0)).unwrap()
    }

    /// `complete_generation` GCs every OTHER generation of the ACTIVE repo; the V042 `repo_id`
    /// predicate scopes that delete, so a sibling repo's live generation is spared (superseding the
    /// old `multiple_real_repos` guard).
    #[test]
    fn complete_generation_spares_sibling_generations_on_a_multi_repo_db() {
        // Whole-DB `generation_count` — opt out of the poison harness (whose sibling seeds another
        // generation) so this test controls the exact generation set it asserts on.
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let db = build_clone_fixture("mr-complete");
        {
            let conn = db.storage.connection();
            make_multi_repo(conn);
            // this repo's, to complete
            seed_generation(conn, 1, "Building", "rev-a", &db.active_repo_id);
            // a sibling repo's live generation
            seed_generation(conn, 2, "Complete", "rev-sibling", "repo-x");
        }
        db.complete_generation(1, 0).unwrap();

        let conn = db.storage.connection();
        assert_eq!(
            generation_count(conn),
            2,
            "the per-repo predicate spared the sibling repo's generation"
        );
        let g1_status: String = conn
            .query_row("SELECT status FROM clone_graph_generations WHERE generation = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(g1_status, "Complete", "this repo's generation still completes + publishes");
    }

    /// The complement: with only the active repo's own generations present, `complete_generation`
    /// GCs every OTHER one of them.
    #[test]
    fn complete_generation_prunes_other_generations_on_a_single_repo_db() {
        // Whole-DB `generation_count` — opt out of the poison harness (its sibling generation is a
        // different repo's and is correctly spared, but would inflate this whole-DB count).
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let db = build_clone_fixture("sr-complete");
        {
            let conn = db.storage.connection();
            seed_generation(conn, 1, "Building", "rev-a", &db.active_repo_id);
            seed_generation(conn, 2, "Complete", "rev-old", &db.active_repo_id);
        }
        db.complete_generation(1, 0).unwrap();

        let conn = db.storage.connection();
        assert_eq!(generation_count(conn), 1, "GC drops every other generation of the active repo");
    }

    /// `open_building_generation` discards a stale (different-revision) Building row before
    /// starting fresh; on a multi-repo DB that discard is global, so the guard must skip it and
    /// leave the sibling's in-progress row intact while still allocating a fresh generation.
    #[test]
    fn open_building_generation_spares_a_sibling_building_row_on_a_multi_repo_db() {
        let db = build_clone_fixture("mr-open");
        let conn = db.storage.connection();
        make_multi_repo(conn);
        // a sibling repo's in-progress build
        seed_generation(conn, 7, "Building", "sibling-rev", "repo-x");

        let opened = open_building_generation(conn, "my-new-rev").unwrap();
        assert_ne!(opened.generation, 7, "a fresh generation is allocated, not the sibling's");
        let sibling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clone_graph_generations WHERE generation = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sibling, 1, "the sibling repo's Building row is not globally discarded");
    }

    /// #413 round-5: the multi-repo guard must not just skip the DISCARD — it must also skip the
    /// RESUME. `source_revision` is `content_revision()`, GLOBAL over `main.files`, so a sibling's
    /// Building row at the SAME revision (+ postings-aware) would otherwise be RESUMED and then
    /// published under this repo's live pointer. On a multi-repo DB `open_building_generation` must
    /// allocate a FRESH generation and leave the sibling's Building row untouched, even on a match.
    #[test]
    fn open_building_generation_does_not_resume_a_sibling_building_row_on_a_multi_repo_db() {
        let db = build_clone_fixture("mr-resume");
        let conn = db.storage.connection();
        make_multi_repo(conn);
        // A SIBLING repo's in-progress build at the SAME revision this repo is about to open (a
        // matching, postings-aware row — `seed_generation` writes `postings_written = 1`). The
        // pre-predicate code would RESUME generation 9 and hand it to this repo.
        seed_generation(conn, 9, "Building", "shared-rev", "repo-x");

        let opened = open_building_generation(conn, "shared-rev").unwrap();
        assert_ne!(
            opened.generation, 9,
            "a matching sibling Building row is NOT resumed on a multi-repo DB — fresh generation",
        );
        let sibling_status: String = conn
            .query_row("SELECT status FROM clone_graph_generations WHERE generation = 9", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(sibling_status, "Building", "the sibling's Building row is left intact");
    }

    /// The complement: a single-repo DB discards its own stale Building row as before.
    #[test]
    fn open_building_generation_discards_the_stale_building_row_on_a_single_repo_db() {
        let db = build_clone_fixture("sr-open");
        let conn = db.storage.connection();
        seed_generation(conn, 7, "Building", "stale-rev", &db.active_repo_id);

        open_building_generation(conn, "new-rev").unwrap();
        let stale: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clone_graph_generations WHERE generation = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "single-repo discards the stale Building row as before");
    }
}
