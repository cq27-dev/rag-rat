//! Incremental delta maintenance of the persisted clone-edge graph (#473): when a small set of
//! files changes, update the LIVE Complete generation in place — delete the changed files' edges +
//! postings, recompute only their symbols against the persisted postings, and bump the
//! generation's `source_revision` — instead of discarding and rebuilding the whole generation
//! (`reconcile_clone_edges_pass`), which costs ~1 GB of DB writes per cycle on this repo.
//!
//! WHY THIS IS SOUND (the two load-bearing facts):
//! - Edges are VERIFICATION RESULTS: `clone_edges.overlap` depends only on the two endpoint bags,
//!   so an edit to file X cannot change any edge not touching X — edges between unchanged files
//!   stay valid verbatim.
//! - Prefix filtering is recall-lossless under ANY single consistent token order, and the
//!   generation's `clone_df_epoch` snapshot (#479, superseding the #473 whole-table freeze) pins
//!   the order its postings were built under — the delta reads THAT, so its sub-blocks and the
//!   persisted postings always agree even while the live `clone_token_df` moves on incremental
//!   passes.
//!
//! PARITY DISCIPLINE (pinned by `clone_graph_delta_matches_a_full_rebuild_over_an_edit_sequence`):
//! the delta-maintained edge set must equal a from-scratch rebuild's at the same content. Two
//! rules keep it exact:
//! - Corpus filters match the BUILD's (`load_scoped_baseline_bags`): scoped `files` view,
//!   `generated = 0`, baseline + `NORM_VERSION`, non-NULL bag — and deliberately NO
//!   `symbols.is_test` filter (that narrower filter belongs to the `of_text` write-time corpus, not
//!   the persisted graph).
//! - NO `HOT_TOKEN_POSTINGS_CAP` filtering: the #271 cap belongs to the LIVE candidate paths
//!   (`sub_block_candidate_pairs` / `subject_component_bfs`) — the persisted-graph build walks
//!   every sub-block token uncapped, so the delta does too, or a stable-hot shared token would
//!   silently drop edges a full rebuild keeps (pinned by
//!   `delta_keeps_hot_token_edges_the_build_would_emit`).
//!
//! CHANGED-SET HINT + SELF-HEAL (#830): the delta's file set is derived either from a full DB scan
//! ([`delta_paths`], the [`CloneDeltaHint::FullScan`]/[`CloneDeltaHint::SelfHeal`] paths) or from a
//! reconcile-supplied set of the base paths a pass reindexed or deleted ([`delta_paths_from_hint`],
//! the [`CloneDeltaHint::Paths`] path). The hint replaces the two per-pass corpus scans with
//! indexed point-lookups; it is sound ONLY because it is a SUPERSET of the truly-changed
//! clone-relevant paths: post-#828 `content_revision()` moves iff the `(path, sha256)` multiset of
//! non-deleted files changes, so a revision-moving edit reaches the changed-set derivation, and the
//! reconcile hint (reindexed ∪ deleted) names every file whose `(path, sha256)` it moved.
//!
//! Two things the hint cannot see, both closed by the gc-cadence [`CloneDeltaHint::SelfHeal`]
//! sweep: (1) a stale-overlay heal changes `files` rows the reindexed/deleted set does not name —
//! the reconcile withholds the hint for a healed pass, so it falls to a scan anyway; (2) a
//! `generated`-flag flip changes `files.generated` (not `path`/`sha256`), so it moves NO revision
//! and every `Paths`/`FullScan` delta returns `Noop` before the derivation — only `SelfHeal`, which
//! bypasses that revision-equality early return, scans and repairs it. The watcher runs `SelfHeal`
//! on the gc cadence (`GC_EVERY_PASSES`), so drift the digest cannot reflect is bounded to that
//! window. When `SelfHeal` finds MORE such drift than the delta cap (`CLONE_DELTA_MAX_FILES`, e.g.
//! a mass generated-reclassification) it `Escalate`s; because that drift is revision-neutral the
//! quiet gate never arms (the graph looks fresh against the revision), so the watcher forces a full
//! rebuild past the gate for exactly that fresh-graph Escalate (see `watch::pass`) rather than
//! leaving it to the `delta_files_applied >= CLONE_GRAPH_DRIFT_REBUILD_FILES` drift rebuild, which
//! a revision-neutral Escalate never advances. The `SelfHeal` scan is load-bearing, NOT redundant —
//! do not "optimize" it into a plain `FullScan`, whose early `Noop` would leave generated-flip
//! drift for the full rebuild alone.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use rag_rat_clones::NORM_VERSION;
use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};
use serde::Serialize;

use super::precompute::{
    Anchor, CLONE_GRAPH_DRIFT_REBUILD_FILES, CLONE_PRECOMPUTE_THETA, EdgeRow, PostingGroup,
    clone_generation_scope_clause, insert_edge_rows, insert_posting_groups, live_generation_row,
    make_edge,
};
use super::substrate::{
    SymbolBag, add_struct_hash_pairs, load_scoped_baseline_bags_for_paths, overlap,
    sub_block_tokens, verified_clone,
};
use crate::index::IndexDatabase;

/// SQLite bind-variable safety chunk for `IN (…)` lists (mirrors `of_text::HYDRATION_CHUNK`).
const DELTA_SQL_CHUNK: usize = 400;

/// The background tails' delta size cap: a change touching more files than this escalates to a
/// full rebuild (a branch switch re-anchors most of the corpus — patching it file-by-file costs
/// more than one clean generation build).
pub const CLONE_DELTA_MAX_FILES: usize = 64;

/// Closed status tokens shared by clone-delta reports and their consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum CloneDeltaStatus {
    Applied,
    Noop,
    NotEligible,
    Escalate,
}

impl CloneDeltaStatus {
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        value.parse().ok()
    }
}

/// Outcome of one [`IndexDatabase::apply_clone_graph_delta`] attempt. `status`:
/// - `Applied` — the live generation now matches `content_revision()`; counts say what changed.
/// - `Noop` — already current, nothing to do.
/// - `NotEligible` — no live Complete postings-aware generation to patch (or a full rebuild is in
///   flight, or an overlay scope is active); the caller falls back to the full-rebuild path.
/// - `Escalate` — the delta is too large to patch in place (more changed files than the caller's
///   cap, or candidate hydration exceeded the posting-row work budget — #598); the caller schedules
///   a full rebuild instead. Nothing was written.
#[derive(Debug, Clone, Serialize)]
pub struct CloneDeltaReport {
    pub status: CloneDeltaStatus,
    pub reason: Option<String>,
    pub files_changed: u64,
    pub edges_added: u64,
    pub edges_removed: u64,
    /// Set on `Applied`/`Noop` when the live generation's accumulated `delta_files_applied` has
    /// reached [`CLONE_GRAPH_DRIFT_REBUILD_FILES`]: the graph is FRESH (keep serving it) but its
    /// frozen df epoch owes a full rebuild — the caller lets the quiet-gated full path take it.
    pub full_rebuild_owed: bool,
    pub elapsed_ms: u64,
    /// Posting rows candidate hydration ASKED for, cache hits included (#598) — the combinatorics
    /// proxy the work budget meters: it also tracks the per-candidate verify CPU that follows,
    /// which physical I/O alone (see `posting_rows_fetched`) stops measuring once hot lists are
    /// served from the per-application cache.
    pub posting_rows_requested: u64,
    /// Posting rows physically read from `clone_subblock_postings` (cache misses only).
    pub posting_rows_fetched: u64,
}

/// Floor for the derived posting-row work budget (#598), so small corpora — where even heavy
/// token sharing is cheap in absolute terms — never flap a healthy delta to Escalate.
const CLONE_DELTA_MIN_POSTING_ROW_BUDGET: u64 = 100_000;

/// How a delta derives its changed-file set (#830). `Paths` and `FullScan` produce the SAME set
/// when the hint is a superset of the truly-changed clone-relevant paths (see the module doc's
/// soundness note); `SelfHeal` is the gc-cadence sweep that also repairs drift the content digest
/// cannot see.
#[derive(Clone, Copy)]
pub(crate) enum CloneDeltaHint<'a> {
    /// Derive the changed set from the full DB scan ([`delta_paths`]), but honor the
    /// revision-equality fast path: the default for callers with no reconcile hint (the CLI
    /// one-shot) and the fallback when a pass cannot offer a hint yet the revision moved
    /// (a stale-overlay heal / bootstrap rebuild).
    FullScan,
    /// Restrict the changed set to the pass's touched base paths ([`delta_paths_from_hint`]), using
    /// indexed point-lookups instead of the two corpus scans.
    Paths(&'a BTreeSet<String>),
    /// A full DB scan that runs EVEN when `content_revision()` is unchanged — the gc-cadence
    /// self-heal. It is the only path that repairs drift the digest does not reflect: a
    /// `generated`-flag flip changes `files.generated` (not `path`/`sha256`), so it never moves the
    /// revision, and a `Paths`/`FullScan` delta returns `Noop` before the changed-set derivation.
    /// Bypassing that early return here is what makes the self-heal on `run_gc` genuinely
    /// load-bearing rather than defeated by the fast path.
    SelfHeal,
}

impl CloneDeltaHint<'_> {
    /// Whether this hint scans regardless of revision movement — only [`Self::SelfHeal`] does.
    fn scans_when_revision_unchanged(&self) -> bool {
        matches!(self, CloneDeltaHint::SelfHeal)
    }
}

impl IndexDatabase {
    /// Apply one clone-graph delta toward the current `content_revision()`, bounded to
    /// `max_files` changed files (larger deltas escalate — a branch switch is cheaper to rebuild).
    /// MUST run under the caller's write lock (like `reconcile_clone_edges_pass`); the read phase
    /// runs lock-stable outside a transaction, and all writes commit in ONE transaction, so a
    /// reader sees either the old graph or the fully-applied delta.
    ///
    /// Derives the changed set from the full DB scan ([`CloneDeltaHint::FullScan`]) — the
    /// self-healing default for callers with no reconcile-supplied changed-set (the CLI one-shot,
    /// the differential parity pin). The watcher uses [`Self::apply_clone_graph_delta_hinted`] to
    /// pass the pass's touched paths (#830).
    pub fn apply_clone_graph_delta(&self, max_files: usize) -> anyhow::Result<CloneDeltaReport> {
        self.apply_clone_graph_delta_inner(max_files, CloneDeltaHint::FullScan, None)
    }

    /// [`Self::apply_clone_graph_delta`] with an explicit changed-set `hint` (#830): the watcher
    /// passes [`CloneDeltaHint::Paths`] with the base paths the reconcile just reindexed/deleted so
    /// the read phase skips the two corpus scans, and [`CloneDeltaHint::FullScan`] on a cadence so
    /// any hint/DB drift self-heals.
    pub(crate) fn apply_clone_graph_delta_hinted(
        &self,
        max_files: usize,
        hint: CloneDeltaHint,
    ) -> anyhow::Result<CloneDeltaReport> {
        self.apply_clone_graph_delta_inner(max_files, hint, None)
    }

    /// [`Self::apply_clone_graph_delta`] with an explicit posting-row work budget — the test seam
    /// for the #598 Escalate bail (production derives the budget from the generation's postings
    /// table size). Test-gated like other test-only helpers, or non-test clippy fails on
    /// dead_code (#467).
    #[cfg(test)]
    pub(crate) fn apply_clone_graph_delta_with_budget(
        &self,
        max_files: usize,
        posting_row_budget: u64,
    ) -> anyhow::Result<CloneDeltaReport> {
        self.apply_clone_graph_delta_inner(
            max_files,
            CloneDeltaHint::FullScan,
            Some(posting_row_budget),
        )
    }

    fn apply_clone_graph_delta_inner(
        &self,
        max_files: usize,
        hint: CloneDeltaHint,
        posting_row_budget: Option<u64>,
    ) -> anyhow::Result<CloneDeltaReport> {
        let started = Instant::now();
        let conn = self.storage.connection();

        // Eligibility — anything here sends the caller to the full-rebuild path instead.
        if self.active_scope_is_linked_overlay() {
            // The graph is built in the BASE scope only (see `clone_check_indexed_generation`).
            return Ok(report(
                CloneDeltaStatus::NotEligible,
                Some("linked-overlay scope"),
                0,
                0,
                0,
                started,
            ));
        }
        let Some(live) = live_generation_row(conn)? else {
            return Ok(report(
                CloneDeltaStatus::NotEligible,
                Some("no live generation"),
                0,
                0,
                0,
                started,
            ));
        };
        if live.normalizer_version != NORM_VERSION || !live.postings_written {
            return Ok(report(
                CloneDeltaStatus::NotEligible,
                Some("live generation predates the current normalizer or postings"),
                0,
                0,
                0,
                started,
            ));
        }
        if building_generation_exists(conn)? {
            // A partial full build is owed; patching the live generation now would race its
            // eventual publish. Let the full-rebuild path finish (or discard) it.
            return Ok(report(
                CloneDeltaStatus::NotEligible,
                Some("a full rebuild is in flight"),
                0,
                0,
                0,
                started,
            ));
        }
        if !super::precompute::clone_df_epoch_exists(conn, live.generation)? {
            // #479: the delta computes sub-blocks under the generation's pinned epoch; without
            // the epoch rows the build order is unrecoverable, and patching under a different
            // order would silently drop edges. One full rebuild re-pins it.
            return Ok(report(
                CloneDeltaStatus::NotEligible,
                Some("live generation has no df epoch (pre-epoch build)"),
                0,
                0,
                0,
                started,
            ));
        }
        // The digest this delta settles freshness TOWARD — compared against the live
        // generation's stamp above and written back as the new stamp below, so it MUST describe
        // the `files` rows this delta reads. `content_revision()` is an O(1) read of the
        // trigger-maintained digest (#828), so it always reflects the `files` rows as committed —
        // there is no scan to pin or reuse.
        let revision = self.content_revision()?;
        // Revision-equality fast path: nothing content-addressable changed, so skip the changed-set
        // derivation — EXCEPT for a `SelfHeal` sweep (#830), which scans anyway to repair drift the
        // content digest cannot see (a `generated`-flag flip moves no `(path, sha256)`, so it never
        // moves the revision). Without this exception the gc-cadence self-heal would always return
        // here and never reach `delta_paths`, leaving that drift for the full rebuild alone.
        if live.source_revision == revision && !hint.scans_when_revision_unchanged() {
            return Ok(CloneDeltaReport {
                full_rebuild_owed: live.delta_files_applied >= CLONE_GRAPH_DRIFT_REBUILD_FILES,
                ..report(CloneDeltaStatus::Noop, None, 0, 0, 0, started)
            });
        }
        let generation = live.generation;
        let drift_after = |absorbed: i64| -> bool {
            live.delta_files_applied + absorbed >= CLONE_GRAPH_DRIFT_REBUILD_FILES
        };

        // ---- Read phase (write lock held; no transaction needed for consistency) ----

        // #830: FullScan / SelfHeal derive the changed set from the whole postings/fingerprint
        // corpus (the self-heal); Paths restricts it to the reconcile's touched base paths via
        // indexed point-lookups. Everything after keys off this `paths` Vec identically.
        let paths = match hint {
            CloneDeltaHint::FullScan | CloneDeltaHint::SelfHeal => delta_paths(conn, generation)?,
            CloneDeltaHint::Paths(touched) => delta_paths_from_hint(conn, generation, touched)?,
        };
        if paths.len() > max_files {
            return Ok(report(
                CloneDeltaStatus::Escalate,
                Some("more changed files than the delta cap — a full rebuild is cheaper"),
                paths.len() as u64,
                0,
                0,
                started,
            ));
        }
        if paths.is_empty() {
            // Nothing clone-relevant changed. Two ways here: the revision MOVED but only a
            // clone-irrelevant file did (e.g. a docs-target edit) — re-pin the freshness key; or a
            // `SelfHeal` sweep ran with the revision UNCHANGED and found no drift — leave the stamp
            // alone so an idle gc pass stays write-free (#63). The `revision != source_revision`
            // guard collapses to "always re-pin" on every non-`SelfHeal` path (they only reach here
            // past the moved-revision check above).
            let repinned = revision != live.source_revision;
            if repinned {
                conn.execute(
                    "UPDATE clone_graph_generations SET source_revision = ?1 WHERE generation = ?2",
                    params![revision, generation],
                )?;
            }
            return Ok(CloneDeltaReport {
                full_rebuild_owed: drift_after(0),
                ..report(
                    if repinned { CloneDeltaStatus::Applied } else { CloneDeltaStatus::Noop },
                    None,
                    0,
                    0,
                    0,
                    started,
                )
            });
        }

        // #479: the delta's bags — and therefore every sub-block computed below — are ordered by
        // the generation's FROZEN epoch, not the live `clone_token_df` (which moves on
        // incremental passes). This is what keeps delta-emitted postings byte-compatible with the
        // build's.
        let epoch_df = super::substrate::load_clone_df_epoch(conn, generation)?;
        let delta_bags = load_scoped_baseline_bags_for_paths(conn, &paths, &epoch_df)?;
        let anchors = anchors_for_paths(conn, &paths)?;
        let sub_blocks: BTreeMap<i64, Vec<i64>> = delta_bags
            .iter()
            .map(|bag| (bag.symbol_id, sub_block_tokens(bag, CLONE_PRECOMPUTE_THETA)))
            .collect();

        // ---- Emission (RAM; reads only) ----
        //
        // Deliberately NO #271 hot-token filtering anywhere below: the persisted-graph build
        // walks every sub-block token uncapped (the cap belongs to the live candidate paths), so
        // the delta must too — see the module doc's parity discipline. What IS bounded (#598) is
        // the delta's total hydration work: the file-count cap above can't see posting fan-out,
        // and a ≤`max_files` delta whose bags hit hot tokens was observed grinding one core for
        // 38+ minutes under the write lock. When hydration requests more posting rows than the
        // budget, the delta escalates — the same nothing-written escape as the file-count bail,
        // and the full rebuild (budgeted + resumable) reads each posting once anyway. Default
        // budget: ~two sweeps of the generation's postings table; past that the rebuild is
        // provably competitive on I/O and the per-candidate verify CPU is the next cliff.
        let posting_row_budget = match posting_row_budget {
            Some(budget) => budget,
            None =>
                CLONE_DELTA_MIN_POSTING_ROW_BUDGET.max(2 * postings_row_count(conn, generation)?),
        };

        let delta_path_set: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
        let by_id: BTreeMap<i64, &SymbolBag> =
            delta_bags.iter().map(|b| (b.symbol_id, b)).collect();
        let mut edge_batch: Vec<EdgeRow> = Vec::new();
        let mut posting_groups: Vec<PostingGroup> = Vec::new();
        let mut hydrator = CandidateHydrator::new(generation, &delta_path_set, posting_row_budget);

        // (a) delta symbol vs the UNCHANGED corpus.
        for bag in &delta_bags {
            let Some(s_anchor) = anchors.get(&bag.symbol_id) else { continue };
            let sub = &sub_blocks[&bag.symbol_id];

            // Struct-hash exact partners (sim 1.0, no verify), mirroring the build's rule.
            let mut struct_partner_keys: BTreeSet<(String, i64)> = BTreeSet::new();
            for (p_anchor, p_len) in old_struct_partners(conn, bag, &delta_path_set, s_anchor)? {
                struct_partner_keys.insert((p_anchor.0.clone(), p_anchor.1));
                edge_batch.push(make_edge(
                    s_anchor,
                    bag.token_len,
                    &p_anchor,
                    p_len,
                    bag.token_len,
                    1.0,
                    "struct_hash",
                ));
            }

            // Near candidates via the persisted postings (every sub-block token, uncapped —
            // build parity), memoized across bags and metered against the work budget (#598).
            let Some(candidates) = hydrator.candidates(conn, sub, &bag.language)? else {
                return Ok(CloneDeltaReport {
                    posting_rows_requested: hydrator.posting_rows_requested,
                    posting_rows_fetched: hydrator.posting_rows_fetched,
                    ..report(
                        CloneDeltaStatus::Escalate,
                        Some(
                            "posting hydration exceeded the delta work budget — a full rebuild is \
                             cheaper",
                        ),
                        paths.len() as u64,
                        0,
                        0,
                        started,
                    )
                });
            };
            for (t_bag, t_anchor) in candidates {
                if t_anchor.0 == s_anchor.0 && t_anchor.1 == s_anchor.1 {
                    continue; // self
                }
                if struct_partner_keys.contains(&(t_anchor.0.clone(), t_anchor.1)) {
                    continue; // already emitted as a struct-hash exact pair
                }
                if verified_clone(bag, &t_bag, CLONE_PRECOMPUTE_THETA) {
                    let ov = overlap(bag, &t_bag);
                    let max_len = bag.token_len.max(t_bag.token_len);
                    edge_batch.push(make_edge(
                        s_anchor,
                        bag.token_len,
                        &t_anchor,
                        t_bag.token_len,
                        ov,
                        ov as f64 / max_len as f64,
                        "sub_block",
                    ));
                }
            }

            // Postings for every walked delta symbol — unconditionally, like the build (the cap
            // gates pair EMISSION only, never the persisted postings).
            if !sub.is_empty() {
                posting_groups.push(PostingGroup { anchor: s_anchor.clone(), tokens: sub.clone() });
            }
        }

        // (b) delta vs delta — the build's two rules over just the delta bags.
        let mut struct_pairs: BTreeSet<(i64, i64)> = BTreeSet::new();
        add_struct_hash_pairs(&delta_bags, &mut struct_pairs);
        for &(a, b) in &struct_pairs {
            let (Some(a_anchor), Some(b_anchor)) = (anchors.get(&a), anchors.get(&b)) else {
                continue;
            };
            edge_batch.push(make_edge(
                a_anchor,
                by_id[&a].token_len,
                b_anchor,
                by_id[&b].token_len,
                by_id[&a].token_len,
                1.0,
                "struct_hash",
            ));
        }
        let mut local_inverted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for bag in &delta_bags {
            for &t in &sub_blocks[&bag.symbol_id] {
                local_inverted.entry(t).or_default().push(bag.symbol_id);
            }
        }
        let mut local_pairs: BTreeSet<(i64, i64)> = BTreeSet::new();
        for ids in local_inverted.values() {
            for (i, &a) in ids.iter().enumerate() {
                for &b in &ids[i + 1..] {
                    if by_id[&a].language == by_id[&b].language {
                        local_pairs.insert((a.min(b), a.max(b)));
                    }
                }
            }
        }
        for &(a, b) in &local_pairs {
            if struct_pairs.contains(&(a, b)) {
                continue;
            }
            let (Some(a_anchor), Some(b_anchor)) = (anchors.get(&a), anchors.get(&b)) else {
                continue;
            };
            let (ba, bb) = (by_id[&a], by_id[&b]);
            if verified_clone(ba, bb, CLONE_PRECOMPUTE_THETA) {
                let ov = overlap(ba, bb);
                let max_len = ba.token_len.max(bb.token_len);
                edge_batch.push(make_edge(
                    a_anchor,
                    ba.token_len,
                    b_anchor,
                    bb.token_len,
                    ov,
                    ov as f64 / max_len as f64,
                    "sub_block",
                ));
            }
        }

        // ---- Write phase: one transaction ----

        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<(u64, u64)> {
            let mut edges_removed = 0u64;
            let mut postings_removed = 0u64;
            for chunk in paths.chunks(DELTA_SQL_CHUNK) {
                let placeholders: Vec<String> =
                    (0..chunk.len()).map(|i| format!("?{}", i + 2)).collect();
                let in_list = placeholders.join(", ");
                let mut values: Vec<Value> = Vec::with_capacity(1 + chunk.len());
                values.push(Value::Integer(generation));
                values.extend(chunk.iter().map(|p| Value::Text(p.clone())));
                edges_removed += conn.execute(
                    &format!(
                        "DELETE FROM clone_edges WHERE build_generation = ?1 AND a_path IN \
                         ({in_list})"
                    ),
                    params_from_iter(values.clone()),
                )? as u64;
                edges_removed += conn.execute(
                    &format!(
                        "DELETE FROM clone_edges WHERE build_generation = ?1 AND b_path IN \
                         ({in_list})"
                    ),
                    params_from_iter(values.clone()),
                )? as u64;
                // Indexed by V049's idx_clone_subblock_postings_path. Capture the deleted row
                // count to maintain the cached `postings_row_count` (#830) below.
                postings_removed += conn.execute(
                    &format!(
                        "DELETE FROM clone_subblock_postings WHERE build_generation = ?1 AND path \
                         IN ({in_list})"
                    ),
                    params_from_iter(values),
                )? as u64;
            }
            let edges_added = insert_edge_rows(conn, generation, &edge_batch)?;
            let postings_added = insert_posting_groups(conn, generation, &posting_groups)?;
            conn.execute(
                // #830: `postings_row_count` is maintained by the net (added − removed) delta in
                // the SAME transaction as the postings writes, so it always equals
                // `COUNT(*)` for this generation. `MAX(…, 0)` guards it exactly
                // like `edges_written` — a defensive floor against a torn prior
                // state, never expected to bind.
                "UPDATE clone_graph_generations
                    SET source_revision = ?1,
                        delta_files_applied = delta_files_applied + ?2,
                        edges_written = MAX(edges_written + ?3, 0),
                        postings_row_count = MAX(postings_row_count + ?4, 0)
                  WHERE generation = ?5",
                params![
                    revision,
                    paths.len() as i64,
                    edges_added as i64 - edges_removed as i64,
                    postings_added as i64 - postings_removed as i64,
                    generation
                ],
            )?;
            Ok((edges_added, edges_removed))
        })();
        match result {
            Ok((edges_added, edges_removed)) => {
                conn.execute_batch("COMMIT")?;
                Ok(CloneDeltaReport {
                    full_rebuild_owed: drift_after(paths.len() as i64),
                    posting_rows_requested: hydrator.posting_rows_requested,
                    posting_rows_fetched: hydrator.posting_rows_fetched,
                    ..report(
                        CloneDeltaStatus::Applied,
                        None,
                        paths.len() as u64,
                        edges_added,
                        edges_removed,
                        started,
                    )
                })
            },
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(err)
            },
        }
    }
}

fn report(
    status: CloneDeltaStatus,
    reason: Option<&str>,
    files_changed: u64,
    edges_added: u64,
    edges_removed: u64,
    started: Instant,
) -> CloneDeltaReport {
    CloneDeltaReport {
        status,
        reason: reason.map(str::to_string),
        files_changed,
        edges_added,
        edges_removed,
        full_rebuild_owed: false,
        elapsed_ms: started.elapsed().as_millis() as u64,
        posting_rows_requested: 0,
        posting_rows_fetched: 0,
    }
}

fn building_generation_exists(conn: &Connection) -> anyhow::Result<bool> {
    let repo_clause = clone_generation_scope_clause(conn)?;
    let exists: i64 = conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM clone_graph_generations WHERE status = \
             'Building'{repo_clause})"
        ),
        [],
        |r| r.get(0),
    )?;
    Ok(exists != 0)
}

/// The delta's file set, derived from the DB alone (idempotent, self-healing — no plumbing from
/// discover): postings anchors whose `(path, file_sha)` no longer matches an eligible current
/// file (edited, deleted, or generated-flipped files), plus eligible fingerprinted files with no
/// postings at all (new files; every non-empty bag emits ≥1 posting, so only bagless files can
/// linger here, and they recompute to nothing).
fn delta_paths(conn: &Connection, generation: i64) -> anyhow::Result<Vec<String>> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    let mut stale = conn.prepare(
        "SELECT DISTINCT p.path FROM clone_subblock_postings p
          WHERE p.build_generation = ?1
            AND NOT EXISTS (SELECT 1 FROM files f
                             WHERE f.path = p.path AND f.sha256 = p.file_sha
                               AND f.generated = 0)",
    )?;
    for row in stale.query_map(params![generation], |r| r.get::<_, String>(0))? {
        set.insert(row?);
    }
    let mut fresh = conn.prepare(
        "SELECT DISTINCT f.path FROM files f
           JOIN symbols s ON s.file_id = f.id
           JOIN symbol_fingerprints sf ON sf.symbol_id = s.id
          WHERE f.generated = 0
            AND sf.normalizer_kind = 'baseline' AND sf.normalizer_version = ?1
            AND sf.token_bag IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM clone_subblock_postings p
                             WHERE p.build_generation = ?2 AND p.path = f.path)",
    )?;
    for row in fresh.query_map(params![NORM_VERSION, generation], |r| r.get::<_, String>(0))? {
        set.insert(row?);
    }
    Ok(set.into_iter().collect())
}

/// [`delta_paths`] RESTRICTED to a reconcile-supplied changed-set `hint` (#830): keep only the
/// hinted paths that are actually clone-relevant under this generation, decided by INDEXED
/// point-lookups per path — never a corpus scan (avoiding that scan is the entire point). The
/// result equals `delta_paths()` ∩ `hint`, which is `delta_paths()` itself when the hint is a
/// superset of the truly-changed clone-relevant paths (the module doc's soundness note).
///
/// A hinted path is included iff EITHER predicate from `delta_paths`, narrowed to that one path:
/// - STALE: it has postings under this generation whose `(path, file_sha)` no longer matches an
///   eligible current file (edited / deleted / generated-flipped); or
/// - FRESH: it is an eligible fingerprinted file (`generated = 0`, baseline + `NORM_VERSION`,
///   non-NULL bag) with NO postings under this generation yet.
///
/// The STALE lookup and the FRESH `NOT EXISTS` both key `clone_subblock_postings` on
/// `(build_generation, path)` — served by V050's `idx_clone_subblock_postings_path` — and the
/// file lookups key `files.path`, so no full postings scan runs (verified via EXPLAIN QUERY PLAN).
fn delta_paths_from_hint(
    conn: &Connection,
    generation: i64,
    hint: &BTreeSet<String>,
) -> anyhow::Result<Vec<String>> {
    // STALE: postings under this generation for `?2` with no eligible matching current file. The
    // `p.path = ?2` seek matches `delta_paths`' STALE predicate narrowed to one path.
    let mut stale = conn.prepare(
        "SELECT EXISTS(
            SELECT 1 FROM clone_subblock_postings p
             WHERE p.build_generation = ?1 AND p.path = ?2
               AND NOT EXISTS (SELECT 1 FROM files f
                                WHERE f.path = p.path AND f.sha256 = p.file_sha
                                  AND f.generated = 0))",
    )?;
    // FRESH: `?2` is an eligible fingerprinted file with no postings under this generation yet.
    // Semantically `delta_paths`' FRESH predicate narrowed to one path, but written with `files f`
    // as the SOLE outer table so the planner drives from the `f.path = ?2` seek (indexed via the
    // scoped `files` view) instead of scanning every baseline fingerprint — the eligible-symbol
    // check and the postings check are correlated EXISTS keyed by `s.file_id` / the postings path
    // index.
    let mut fresh = conn.prepare(
        "SELECT EXISTS(
            SELECT 1 FROM files f
             WHERE f.path = ?2 AND f.generated = 0
               AND EXISTS (SELECT 1 FROM symbols s
                             JOIN symbol_fingerprints sf ON sf.symbol_id = s.id
                            WHERE s.file_id = f.id
                              AND sf.normalizer_kind = 'baseline'
                              AND sf.normalizer_version = ?3
                              AND sf.token_bag IS NOT NULL)
               AND NOT EXISTS (SELECT 1 FROM clone_subblock_postings p
                                WHERE p.build_generation = ?1 AND p.path = f.path))",
    )?;
    let mut set: BTreeSet<String> = BTreeSet::new();
    for path in hint {
        let is_stale: i64 = stale.query_row(params![generation, path], |r| r.get(0))?;
        if is_stale != 0 {
            set.insert(path.clone());
            continue;
        }
        let is_fresh: i64 =
            fresh.query_row(params![generation, path, NORM_VERSION], |r| r.get(0))?;
        if is_fresh != 0 {
            set.insert(path.clone());
        }
    }
    Ok(set.into_iter().collect())
}

/// `(path, start_byte, file_sha)` anchors for every scoped, non-generated symbol in `paths` —
/// `resolve_symbol_anchors` narrowed to the delta files.
fn anchors_for_paths(conn: &Connection, paths: &[String]) -> anyhow::Result<BTreeMap<i64, Anchor>> {
    let mut map = BTreeMap::new();
    for chunk in paths.chunks(DELTA_SQL_CHUNK) {
        let placeholders: Vec<String> = (0..chunk.len()).map(|i| format!("?{}", i + 1)).collect();
        let mut stmt = conn.prepare(&format!(
            "SELECT s.id, f.path, s.start_byte, f.sha256
               FROM symbols s JOIN files f ON f.id = s.file_id
              WHERE f.generated = 0 AND f.path IN ({})",
            placeholders.join(", ")
        ))?;
        let values: Vec<Value> = chunk.iter().map(|p| Value::Text(p.clone())).collect();
        let rows = stmt.query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                (row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?),
            ))
        })?;
        for row in rows {
            let (id, anchor) = row?;
            map.insert(id, anchor);
        }
    }
    Ok(map)
}

/// The UNCHANGED corpus's struct-hash exact partners of one delta bag: same `(struct_hash,
/// language)`, BUILD-corpus filters (scoped `files` view, `generated = 0`, baseline +
/// `NORM_VERSION`, non-NULL bag — deliberately NO `is_test` filter, unlike the `of_text` corpus),
/// excluding the delta files themselves (their pairs are emitted by the delta-vs-delta stage) and
/// the subject's own anchor.
fn old_struct_partners(
    conn: &Connection,
    bag: &SymbolBag,
    delta_path_set: &BTreeSet<&str>,
    s_anchor: &Anchor,
) -> anyhow::Result<Vec<(Anchor, i64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.path, s.start_byte, f.sha256, sf.token_len
           FROM symbol_fingerprints sf
           JOIN symbols s ON s.id = sf.symbol_id
           JOIN files f ON f.id = s.file_id
          WHERE sf.normalizer_kind = 'baseline' AND sf.normalizer_version = ?1
            AND f.generated = 0 AND s.language = ?2 AND sf.struct_hash = ?3
            AND sf.token_bag IS NOT NULL",
    )?;
    let rows = stmt.query_map(params![NORM_VERSION, bag.language, bag.struct_hash], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?),
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut partners = Vec::new();
    for row in rows {
        let (anchor, token_len) = row?;
        if delta_path_set.contains(anchor.0.as_str()) {
            continue;
        }
        if anchor.0 == s_anchor.0 && anchor.1 == s_anchor.1 {
            continue;
        }
        partners.push((anchor, token_len));
    }
    Ok(partners)
}

/// The generation's cached posting-row count — sizes the default #598 work budget. Read from the
/// maintained `clone_graph_generations.postings_row_count` column (#830) rather than a full
/// `COUNT(*)` of the postings table: the column is seeded at build (`complete_generation`) and kept
/// exact by each delta write-back, so it equals `COUNT(*)` for the generation on every read. (The
/// generation id — itself resolved repo-scoped — is the scope; `clone_subblock_postings` is
/// generation-keyed, not `repo_id`-scoped.)
fn postings_row_count(conn: &Connection, generation: i64) -> anyhow::Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT postings_row_count FROM clone_graph_generations WHERE generation = ?1",
        [generation],
        |r| r.get(0),
    )?;
    Ok(count.max(0) as u64)
}

/// One delta application's candidate-hydration state (#598), replacing the bare per-bag query.
/// Two properties that query lacked:
/// - MEMOIZATION: posting lists (`postings_by_token`) and hydrated anchors (`bags_by_anchor`) are
///   cached across bags. Hot tokens are shared by MANY bags — the observed pathology re-walked the
///   same ~3k-row posting lists once per bag, cold page by cold page, for 38+ minutes under the
///   write lock.
/// - METERING: every posting row a bag's tokens ask for counts against `budget`, CACHE HITS
///   INCLUDED — requested rows are the combinatorics proxy (each hydrated candidate also buys a
///   `verified_clone` call later), which physical I/O stops measuring once hot lists are cached.
///   Exhaustion surfaces as `Ok(None)`; the caller escalates to the full rebuild with nothing
///   written.
///
/// Corpus-filter PARITY with the build is unchanged (the module-doc discipline): anchor
/// hydration keeps `generated = 0`, baseline + `NORM_VERSION`, non-NULL bag, and the posting-sha
/// staleness check. The per-bag LANGUAGE filter moved from the hydration SQL to the assembly
/// step — an anchor is hydrated once and served to any bag whose language matches, returning
/// exactly the rows a per-bag query would.
struct CandidateHydrator<'a> {
    generation: i64,
    delta_path_set: &'a BTreeSet<&'a str>,
    /// token_hash → its posting rows (anchor path, start_byte, build-time file sha). A token
    /// with no postings caches an EMPTY list so it is never re-queried.
    postings_by_token: BTreeMap<i64, Vec<(String, i64, String)>>,
    /// (path, start_byte) → decoded bag + LIVE file sha, or `None` when the anchor doesn't
    /// hydrate under the build-corpus filters (no fingerprint row, NULL/undecodable bag) — the
    /// negative is cached too, or every bag sharing the token would re-query it.
    bags_by_anchor: BTreeMap<(String, i64), Option<HydratedAnchor>>,
    posting_rows_requested: u64,
    posting_rows_fetched: u64,
    budget: u64,
}

struct HydratedAnchor {
    bag: SymbolBag,
    live_sha: String,
}

impl<'a> CandidateHydrator<'a> {
    fn new(generation: i64, delta_path_set: &'a BTreeSet<&'a str>, budget: u64) -> Self {
        Self {
            generation,
            delta_path_set,
            postings_by_token: BTreeMap::new(),
            bags_by_anchor: BTreeMap::new(),
            posting_rows_requested: 0,
            posting_rows_fetched: 0,
            budget,
        }
    }

    /// Hydrated candidates for one bag's sub-block `tokens`: distinct non-delta anchors from the
    /// tokens' posting lists whose build-time sha still matches the live file (a stale posting is
    /// dead weight from a torn state — never a live candidate) and whose symbol language matches.
    /// `Ok(None)` = the work budget is exhausted; the caller escalates.
    fn candidates(
        &mut self,
        conn: &Connection,
        tokens: &[i64],
        language: &str,
    ) -> anyhow::Result<Option<Vec<(SymbolBag, Anchor)>>> {
        if tokens.is_empty() {
            return Ok(Some(Vec::new()));
        }
        // Fetch the missing posting lists. Rows streamed here are counted as BOTH fetched and
        // requested (the fetch happens on behalf of this asking bag), so a first giant list
        // trips the budget MID-STREAM instead of after materializing it.
        let Some(just_fetched) = self.load_postings(conn, tokens)? else {
            return Ok(None);
        };
        // Cache-hit tokens' rows are requested-only; `just_fetched` ones were already counted.
        for token in tokens {
            if just_fetched.contains(token) {
                continue;
            }
            let len = self.postings_by_token.get(token).map_or(0, |rows| rows.len() as u64);
            self.posting_rows_requested += len;
            if self.posting_rows_requested > self.budget {
                return Ok(None);
            }
        }
        // Distinct candidate anchors for THIS bag (first-seen posting sha wins, matching the
        // pre-#598 `or_insert`), excluding anchors under the delta files (their postings were
        // just accounted for deletion; their pairs belong to the delta-vs-delta stage).
        let mut anchor_sha: BTreeMap<(String, i64), String> = BTreeMap::new();
        for token in tokens {
            let Some(rows) = self.postings_by_token.get(token) else { continue };
            for (path, start_byte, file_sha) in rows {
                if self.delta_path_set.contains(path.as_str()) {
                    continue;
                }
                anchor_sha.entry((path.clone(), *start_byte)).or_insert_with(|| file_sha.clone());
            }
        }
        if anchor_sha.is_empty() {
            return Ok(Some(Vec::new()));
        }
        self.hydrate_anchors(conn, &anchor_sha)?;
        let mut out: Vec<(SymbolBag, Anchor)> = Vec::new();
        for ((path, start_byte), posting_sha) in &anchor_sha {
            let Some(Some(hydrated)) = self.bags_by_anchor.get(&(path.clone(), *start_byte)) else {
                continue;
            };
            if hydrated.bag.language != language || hydrated.live_sha != *posting_sha {
                continue;
            }
            out.push((
                hydrated.bag.clone(),
                (path.clone(), *start_byte, hydrated.live_sha.clone()),
            ));
        }
        Ok(Some(out))
    }

    /// Load the posting lists for `tokens` not yet cached, metering streamed rows. Returns the
    /// set of tokens fetched by THIS call (their rows are already counted as requested), or
    /// `None` when the budget tripped mid-stream.
    fn load_postings(
        &mut self,
        conn: &Connection,
        tokens: &[i64],
    ) -> anyhow::Result<Option<BTreeSet<i64>>> {
        let missing: Vec<i64> =
            tokens.iter().copied().filter(|t| !self.postings_by_token.contains_key(t)).collect();
        for &token in &missing {
            // Pre-seed empty so a no-postings token is cached (never re-queried) even when the
            // stream below returns nothing for it.
            self.postings_by_token.insert(token, Vec::new());
        }
        for chunk in missing.chunks(DELTA_SQL_CHUNK) {
            let placeholders: Vec<String> =
                (0..chunk.len()).map(|i| format!("?{}", i + 2)).collect();
            let mut stmt = conn.prepare(&format!(
                "SELECT token_hash, path, start_byte, file_sha FROM clone_subblock_postings
                  WHERE build_generation = ?1 AND token_hash IN ({})",
                placeholders.join(", ")
            ))?;
            let mut values: Vec<Value> = Vec::with_capacity(1 + chunk.len());
            values.push(Value::Integer(self.generation));
            values.extend(chunk.iter().map(|&t| Value::Integer(t)));
            let rows = stmt.query_map(params_from_iter(values), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (token, path, start_byte, file_sha) = row?;
                self.posting_rows_fetched += 1;
                self.posting_rows_requested += 1;
                if self.posting_rows_requested > self.budget {
                    return Ok(None);
                }
                self.postings_by_token.entry(token).or_default().push((path, start_byte, file_sha));
            }
        }
        Ok(Some(missing.into_iter().collect()))
    }

    /// Batch-hydrate the anchors in `anchor_sha` not yet cached, with the BUILD-corpus filters
    /// (parity: `generated = 0`, baseline + `NORM_VERSION`, non-NULL decodable bag). Anchors the
    /// query does not return cache `None`.
    fn hydrate_anchors(
        &mut self,
        conn: &Connection,
        anchor_sha: &BTreeMap<(String, i64), String>,
    ) -> anyhow::Result<()> {
        let missing: Vec<(String, i64)> = anchor_sha
            .keys()
            .filter(|key| !self.bags_by_anchor.contains_key(*key))
            .cloned()
            .collect();
        for key in &missing {
            self.bags_by_anchor.insert(key.clone(), None);
        }
        for chunk in missing.chunks(DELTA_SQL_CHUNK / 2) {
            let tuples: Vec<String> =
                (0..chunk.len()).map(|i| format!("(?{}, ?{})", 2 * i + 2, 2 * i + 3)).collect();
            let mut stmt = conn.prepare(&format!(
                "SELECT f.path, s.start_byte, f.sha256, s.language, sf.struct_hash, sf.token_len,
                        sf.token_bag, s.id
                   FROM symbol_fingerprints sf
                   JOIN symbols s ON s.id = sf.symbol_id
                   JOIN files f ON f.id = s.file_id
                  WHERE sf.normalizer_kind = 'baseline' AND sf.normalizer_version = ?1
                    AND f.generated = 0
                    AND (f.path, s.start_byte) IN (VALUES {})",
                tuples.join(", ")
            ))?;
            let mut values: Vec<Value> = Vec::with_capacity(1 + 2 * chunk.len());
            values.push(Value::Integer(NORM_VERSION));
            for (path, start_byte) in chunk {
                values.push(Value::Text(path.clone()));
                values.push(Value::Integer(*start_byte));
            }
            let rows = stmt.query_map(params_from_iter(values), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<Vec<u8>>>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })?;
            for row in rows {
                let (path, start_byte, live_sha, lang, struct_hash, token_len, blob, symbol_id) =
                    row?;
                let Some(blob) = blob else { continue };
                let Some(bag_pairs) = rag_rat_clones::bag_blob::decode_token_bag(&blob) else {
                    continue;
                };
                let tokens = bag_pairs
                    .into_iter()
                    .map(|(token_hash, freq)| super::substrate::TokenPosting {
                        token_hash,
                        freq,
                        // df is irrelevant for the VERIFY side (overlap ignores it); DF_FALLBACK
                        // keeps the struct well-formed without loading the df map per candidate.
                        coalesced_df: super::substrate::DF_FALLBACK,
                    })
                    .collect();
                self.bags_by_anchor.insert(
                    (path.clone(), start_byte),
                    Some(HydratedAnchor {
                        bag: SymbolBag {
                            symbol_id,
                            language: lang,
                            struct_hash,
                            token_len,
                            tokens,
                        },
                        live_sha,
                    }),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "delta_tests.rs"]
mod tests;
