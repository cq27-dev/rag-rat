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
    pub status: String,
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

impl IndexDatabase {
    /// Apply one clone-graph delta toward the current `content_revision()`, bounded to
    /// `max_files` changed files (larger deltas escalate — a branch switch is cheaper to rebuild).
    /// MUST run under the caller's write lock (like `reconcile_clone_edges_pass`); the read phase
    /// runs lock-stable outside a transaction, and all writes commit in ONE transaction, so a
    /// reader sees either the old graph or the fully-applied delta.
    pub fn apply_clone_graph_delta(&self, max_files: usize) -> anyhow::Result<CloneDeltaReport> {
        self.apply_clone_graph_delta_inner(max_files, None)
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
        self.apply_clone_graph_delta_inner(max_files, Some(posting_row_budget))
    }

    fn apply_clone_graph_delta_inner(
        &self,
        max_files: usize,
        posting_row_budget: Option<u64>,
    ) -> anyhow::Result<CloneDeltaReport> {
        let started = Instant::now();
        let conn = self.storage.connection();

        // Eligibility — anything here sends the caller to the full-rebuild path instead.
        if self.active_scope_is_linked_overlay() {
            // The graph is built in the BASE scope only (see `clone_check_indexed_generation`).
            return Ok(report("NotEligible", Some("linked-overlay scope"), 0, 0, 0, started));
        }
        let Some(live) = live_generation_row(conn)? else {
            return Ok(report("NotEligible", Some("no live generation"), 0, 0, 0, started));
        };
        if live.normalizer_version != NORM_VERSION || !live.postings_written {
            return Ok(report(
                "NotEligible",
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
                "NotEligible",
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
                "NotEligible",
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
        if live.source_revision == revision {
            return Ok(CloneDeltaReport {
                full_rebuild_owed: live.delta_files_applied >= CLONE_GRAPH_DRIFT_REBUILD_FILES,
                ..report("Noop", None, 0, 0, 0, started)
            });
        }
        let generation = live.generation;
        let drift_after = |absorbed: i64| -> bool {
            live.delta_files_applied + absorbed >= CLONE_GRAPH_DRIFT_REBUILD_FILES
        };

        // ---- Read phase (write lock held; no transaction needed for consistency) ----

        let paths = delta_paths(conn, generation)?;
        if paths.len() > max_files {
            return Ok(report(
                "Escalate",
                Some("more changed files than the delta cap — a full rebuild is cheaper"),
                paths.len() as u64,
                0,
                0,
                started,
            ));
        }
        if paths.is_empty() {
            // The revision moved but no clone-relevant file changed (e.g. a docs-target edit):
            // the graph is semantically current — just re-pin the freshness key.
            conn.execute(
                "UPDATE clone_graph_generations SET source_revision = ?1 WHERE generation = ?2",
                params![revision, generation],
            )?;
            return Ok(CloneDeltaReport {
                full_rebuild_owed: drift_after(0),
                ..report("Applied", None, 0, 0, 0, started)
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
                        "Escalate",
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
                // Indexed by V049's idx_clone_subblock_postings_path.
                conn.execute(
                    &format!(
                        "DELETE FROM clone_subblock_postings WHERE build_generation = ?1 AND path \
                         IN ({in_list})"
                    ),
                    params_from_iter(values),
                )?;
            }
            let edges_added = insert_edge_rows(conn, generation, &edge_batch)?;
            insert_posting_groups(conn, generation, &posting_groups)?;
            conn.execute(
                "UPDATE clone_graph_generations
                    SET source_revision = ?1,
                        delta_files_applied = delta_files_applied + ?2,
                        edges_written = MAX(edges_written + ?3, 0)
                  WHERE generation = ?4",
                params![
                    revision,
                    paths.len() as i64,
                    edges_added as i64 - edges_removed as i64,
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
                        "Applied",
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
    status: &str,
    reason: Option<&str>,
    files_changed: u64,
    edges_added: u64,
    edges_removed: u64,
    started: Instant,
) -> CloneDeltaReport {
    CloneDeltaReport {
        status: status.to_string(),
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

/// The generation's persisted posting-row count — sizes the default #598 work budget.
/// (`clone_subblock_postings` is generation-keyed, not `repo_id`-scoped: the generation id —
/// itself resolved repo-scoped — is the scope.)
fn postings_row_count(conn: &Connection, generation: i64) -> anyhow::Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM clone_subblock_postings WHERE build_generation = ?1",
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
mod tests {
    use super::super::precompute::tests::{clone_fixture_config, edge_keys};
    use super::{
        BTreeSet, CLONE_PRECOMPUTE_THETA, load_scoped_baseline_bags_for_paths, params,
        sub_block_tokens,
    };
    use crate::index::query_api::clones::precompute::CloneEdgeOptions;

    /// One incremental index over the fixture root (the watcher's discover path — works without
    /// git), returning a fresh handle.
    fn reindex(config: &rag_rat_base::config::Config) -> crate::IndexDatabase {
        let (db, _changed) = crate::IndexDatabase::index_discover_reporting(config).unwrap();
        db
    }

    fn force_rebuild_edges(db: &crate::IndexDatabase) -> Vec<(String, i64, String, i64)> {
        let report = db
            .reconcile_clone_edges_pass(&CloneEdgeOptions { force: true, ..Default::default() })
            .unwrap();
        assert_eq!(report.status, "Complete", "forced rebuild runs to completion");
        edge_keys(db)
    }

    /// THE differential pin: after every delta, the maintained edge set equals what a from-scratch
    /// generation build produces at the same content (same frozen df epoch). Exercises add, edit,
    /// delete, and COMPOUND deltas (two deltas with no intermediate rebuild).
    #[test]
    fn clone_graph_delta_matches_a_full_rebuild_over_an_edit_sequence() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-differential");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        drop(db);

        let steps: &[(&str, Option<&str>)] = &[
            // Add a near-clone pair member + a unique function.
            (
                "src/c.rs",
                Some(
                    "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 \
                     }\npub fn distinct_worker(n: u64) -> u64 { n.rotate_left(3) ^ 0x0defaced }\n",
                ),
            ),
            // Edit an existing file: append another member of the tally family.
            (
                "src/a.rs",
                Some(
                    "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 \
                     }\npub fn compute_totals(items: Vec<i64>) -> i64 { let mut s = 0; for it in \
                     items { s += it * 2; } s + 1 }\npub fn sum_figures(rows: Vec<i64>) -> i64 { \
                     let mut f = 0; for r in rows { f += r * 2; } f + 1 }\n",
                ),
            ),
            // Delete a file that carries clone-family members.
            ("src/b.rs", None),
        ];
        for (path, content) in steps {
            let target = config.root.join(path);
            match content {
                Some(text) => std::fs::write(&target, text).unwrap(),
                None => std::fs::remove_file(&target).unwrap(),
            }
            let db = reindex(&config);
            let report = db.apply_clone_graph_delta(64).unwrap();
            assert_eq!(report.status, "Applied", "delta applies for {path}: {report:?}");
            let delta_edges = edge_keys(&db);
            let rebuilt_edges = force_rebuild_edges(&db);
            assert_eq!(
                delta_edges, rebuilt_edges,
                "delta-maintained edges equal a from-scratch rebuild after touching {path}"
            );
        }

        // COMPOUND: two deltas back-to-back with no intermediate rebuild, compared once at the
        // end — pins that parity is maintained inductively, not just from a fresh baseline.
        std::fs::write(
            config.root.join("src/d.rs"),
            "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\n",
        )
        .unwrap();
        let db = reindex(&config);
        assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, "Applied");
        drop(db);
        std::fs::write(
            config.root.join("src/c.rs"),
            "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\npub \
             fn distinct_worker(n: u64) -> u64 { n.rotate_left(4) ^ 0x0badf00d }\n",
        )
        .unwrap();
        let db = reindex(&config);
        assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, "Applied");
        let delta_edges = edge_keys(&db);
        let rebuilt_edges = force_rebuild_edges(&db);
        assert_eq!(delta_edges, rebuilt_edges, "compound deltas stay parity-equal");
        drop(db);

        // INTERLEAVED LIVE-INDEX STEP (#479): every reindex above already bumps the LIVE df; here
        // an adversarial whole-table inversion (the most live drift could ever diverge from the
        // pinned epoch) precedes one more edit + delta. Parity with a from-scratch rebuild must
        // still hold — the delta orders by the generation's epoch, not the live table. (The
        // byte-level discriminator lives in
        // `delta_postings_are_ordered_by_the_epoch_not_the_live_df`; this step pins the
        // end-to-end soundness claim on the edge set.)
        {
            let db = crate::IndexDatabase::open_config(&config).unwrap();
            db.storage
                .connection()
                .execute("UPDATE clone_token_df SET df = 1000000 - df", [])
                .unwrap();
        }
        std::fs::write(
            config.root.join("src/e.rs"),
            "pub fn load_shipment(db: Db) -> i32 { let s = db.get(50); validate(s); s + 1 }\n",
        )
        .unwrap();
        let db = reindex(&config);
        assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, "Applied");
        let delta_edges = edge_keys(&db);
        let rebuilt_edges = force_rebuild_edges(&db);
        assert_eq!(
            delta_edges, rebuilt_edges,
            "parity holds through an adversarial live-table inversion between deltas"
        );
    }

    /// The byte-level pin for the #479 df split: the delta's persisted postings are ordered by
    /// the generation's PINNED epoch, not the live `clone_token_df`. The live table is inverted
    /// before the delta, so the two orders provably select DIFFERENT sub-block prefixes for the
    /// touched file — and the postings must match the EPOCH's selection.
    #[test]
    fn delta_postings_are_ordered_by_the_epoch_not_the_live_df() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-epoch-postings");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        let built = db.precompute_clone_graph(None).unwrap();
        assert_eq!(built.status, "Complete");
        // Adversarial live drift: invert the whole live table.
        db.storage.connection().execute("UPDATE clone_token_df SET df = 1000000 - df", []).unwrap();
        drop(db);

        // A near-clone family member: enough shared (mid-df) and unique (df=1) tokens that the
        // epoch and inverted-live orders pick different prefixes.
        let touched = "src/shipment.rs".to_string();
        std::fs::write(
            config.root.join(&touched),
            "pub fn load_shipment(db: Db) -> i32 { let s = db.get(50); validate(s); s + 1 }\n",
        )
        .unwrap();
        let db = reindex(&config);
        assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, "Applied");

        let conn = db.storage.connection();
        let paths = vec![touched.clone()];
        let sub_block_union = |df: &std::collections::HashMap<i64, i64>| -> BTreeSet<i64> {
            load_scoped_baseline_bags_for_paths(conn, &paths, df)
                .unwrap()
                .iter()
                .flat_map(|bag| sub_block_tokens(bag, CLONE_PRECOMPUTE_THETA))
                .collect()
        };
        let epoch_df =
            super::super::substrate::load_clone_df_epoch(conn, built.generation).unwrap();
        let live_df = super::super::substrate::load_current_clone_df(conn).unwrap();
        let under_epoch = sub_block_union(&epoch_df);
        let under_live = sub_block_union(&live_df);
        assert_ne!(
            under_epoch, under_live,
            "precondition: the inversion must actually change the prefix selection — if this \
             fails the fixture has degenerated and the test is vacuous"
        );

        let persisted: BTreeSet<i64> = conn
            .prepare(
                "SELECT DISTINCT token_hash FROM clone_subblock_postings
                 WHERE build_generation = ?1 AND path = ?2",
            )
            .unwrap()
            .query_map(params![built.generation, touched], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            persisted, under_epoch,
            "delta-emitted postings are selected under the generation's pinned epoch"
        );
    }

    /// A current generation is a cheap no-op — and a SECOND delta right after an applied one must
    /// also be a no-op (idempotence at the write level).
    #[test]
    fn clone_graph_delta_is_noop_when_current() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-noop");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, "Noop");
        drop(db);

        std::fs::write(
            config.root.join("src/e.rs"),
            "pub fn delta_probe(q: i64) -> i64 { q * 7 + 5 }\n",
        )
        .unwrap();
        let db = reindex(&config);
        let applied = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(applied.status, "Applied");
        assert_eq!(applied.files_changed, 1, "exactly the touched file: {applied:?}");
        let again = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(again.status, "Noop", "an applied delta leaves nothing owed");
        assert_eq!(again.edges_added + again.edges_removed, 0);
    }

    /// Without a live Complete generation there is nothing to patch — the caller must use the
    /// full-rebuild path.
    #[test]
    fn clone_graph_delta_is_not_eligible_without_a_live_generation() {
        let config = clone_fixture_config("delta-no-gen");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(report.status, "NotEligible", "{report:?}");
    }

    /// The remaining eligibility gates: a postings-stale live generation (pre-upgrade or
    /// df-refresh-invalidated) and an in-flight Building generation both refuse the in-place
    /// patch — the full-rebuild path owns those states.
    #[test]
    fn clone_graph_delta_is_not_eligible_for_stale_postings_or_inflight_builds() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-ineligible");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        let conn = db.storage.connection();

        conn.execute("UPDATE clone_graph_generations SET postings_written = 0", []).unwrap();
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(report.status, "NotEligible", "postings-stale generation: {report:?}");
        conn.execute("UPDATE clone_graph_generations SET postings_written = 1", []).unwrap();

        // An in-flight (Building) generation means a full rebuild is owed — patching the live
        // generation now would race its eventual publish.
        conn.execute(
            "INSERT INTO clone_graph_generations
                (generation, status, theta_floor, normalizer_kind, normalizer_version,
                 source_revision, started_at_ms, postings_written, repo_id)
             VALUES (9999, 'Building', 0.7, 'baseline', ?1, 'inflight-rev', 0, 1, ?2)",
            rusqlite::params![rag_rat_clones::NORM_VERSION, db.active_repo_id],
        )
        .unwrap();
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(report.status, "NotEligible", "in-flight full rebuild: {report:?}");
    }

    /// A content-revision move with NO clone-relevant file change (a new file with no
    /// fingerprintable functions) re-pins `source_revision` without touching the graph —
    /// `Applied` with zero files, zero edge churn.
    #[test]
    fn clone_graph_delta_repins_freshness_for_clone_irrelevant_changes() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-irrelevant");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        let edges_before = edge_keys(&db);
        drop(db);

        // A type-only file: indexed (revision moves) but no function fingerprints.
        std::fs::write(config.root.join("src/j.rs"), "pub struct MarkerOnly;\n").unwrap();
        let db = reindex(&config);
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(report.status, "Applied", "{report:?}");
        assert_eq!(report.files_changed, 0, "no clone-relevant file changed");
        assert_eq!(report.edges_added + report.edges_removed, 0);
        assert_eq!(edge_keys(&db), edges_before, "the graph itself is untouched");
        assert!(!db.pending_clone_graph().unwrap(), "but the freshness key is re-pinned");
    }

    /// A delta larger than `max_files` escalates without writing anything — a huge delta (branch
    /// switch) is cheaper to rebuild than to patch file-by-file.
    #[test]
    fn clone_graph_delta_escalates_when_too_many_files_changed() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-escalate");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        let edges_before = edge_keys(&db);
        drop(db);

        std::fs::write(
            config.root.join("src/f.rs"),
            "pub fn escalate_probe(q: i64) -> i64 { q * 9 + 2 }\n",
        )
        .unwrap();
        let db = reindex(&config);
        let report = db.apply_clone_graph_delta(0).unwrap();
        assert_eq!(report.status, "Escalate", "{report:?}");
        assert_eq!(edge_keys(&db), edges_before, "an escalated delta writes nothing");
    }

    /// The PERSISTED graph does not apply the live path's #271 hot-token cap (the build walks
    /// every sub-block token; the cap belongs to `sub_block_candidate_pairs` / the RAM fallback
    /// only) — so neither may the delta. A stable-hot shared token (postings above the cap before
    /// AND after the delta) must still re-emit the changed file's verified edges, or the delta
    /// silently under-populates the graph a full rebuild would keep.
    #[test]
    fn delta_keeps_hot_token_edges_the_build_would_emit() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-hot-token");
        // A sub-block-only near-clone pair: same token bag family, DIFFERENT structure (the extra
        // trailing statement), so no struct-hash edge can mask a dropped sub-block edge.
        std::fs::write(
            config.root.join("src/a.rs"),
            "pub fn alpha_total(vals: Vec<i64>) -> i64 { let mut s = 0; for v in vals { s += v * \
             3; } s - 2 }\n",
        )
        .unwrap();
        std::fs::write(
            config.root.join("src/b.rs"),
            "pub fn omega_total(vals: Vec<i64>) -> i64 { let mut s = 0; for v in vals { s += v * \
             3; } let z = s; z - 2 }\n",
        )
        .unwrap();
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        let conn = db.storage.connection();
        let sub_block_edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clone_edges WHERE edge_source = 'sub_block'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sub_block_edges > 0, "the pair must be sub-block-discovered, not struct-exact");

        // Make EVERY shared discovery token stable-hot: inflate its postings past the cap with
        // rows anchored at the UNTOUCHED file's current sha (so they are neither stale nor part
        // of the delta set) at start_bytes that resolve to no symbol (hydration drops them).
        let b_sha: String = conn
            .query_row("SELECT sha256 FROM files WHERE path = 'src/b.rs'", [], |r| r.get(0))
            .unwrap();
        let generation: i64 = conn
            .query_row(
                "SELECT MAX(generation) FROM clone_graph_generations WHERE status = 'Complete'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let shared_tokens: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT p1.token_hash FROM clone_subblock_postings p1
                      WHERE p1.path = 'src/a.rs'
                        AND EXISTS (SELECT 1 FROM clone_subblock_postings p2
                                     WHERE p2.token_hash = p1.token_hash AND p2.path = 'src/b.rs')",
                )
                .unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        assert!(!shared_tokens.is_empty(), "the near-clone pair shares discovery tokens");
        for token in &shared_tokens {
            for i in 0..(super::super::substrate::HOT_TOKEN_POSTINGS_CAP as i64 + 8) {
                conn.execute(
                    "INSERT OR IGNORE INTO clone_subblock_postings
                        (build_generation, token_hash, path, start_byte, file_sha)
                     VALUES (?1, ?2, 'src/b.rs', ?3, ?4)",
                    rusqlite::params![generation, token, 1_000_000 + i, b_sha],
                )
                .unwrap();
            }
        }
        drop(db);

        // Edit the OTHER file trivially (append an unrelated fn): its symbols recompute through
        // the delta, and their edges to b.rs must survive despite every shared token being hot.
        let mut text = std::fs::read_to_string(config.root.join("src/a.rs")).unwrap();
        text.push_str("pub fn unrelated_probe(q: i64) -> i64 { q ^ 3 }\n");
        std::fs::write(config.root.join("src/a.rs"), text).unwrap();
        let db = reindex(&config);
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(report.status, "Applied", "{report:?}");
        let delta_edges = edge_keys(&db);
        let rebuilt_edges = force_rebuild_edges(&db);
        assert_eq!(
            delta_edges, rebuilt_edges,
            "stable-hot shared tokens must not drop edges the build would emit"
        );
    }

    /// The watcher tail applies the delta IN PLACE on the same pass that indexed the edit — no
    /// quiet window, no generation churn. The #472 gate now guards only the full-rebuild path.
    #[test]
    fn maintenance_pass_applies_the_delta_in_place() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-tail-inplace");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        let built = db.precompute_clone_graph(None).unwrap();
        assert_eq!(built.status, "Complete");
        drop(db);

        std::fs::write(
            config.root.join("src/h.rs"),
            "pub fn load_receipt(db: Db) -> i32 { let r = db.get(40); validate(r); r + 1 }\n",
        )
        .unwrap();
        crate::watch::maintenance_pass(&config, false).unwrap();

        let db = crate::IndexDatabase::open_config(&config).unwrap();
        assert!(
            !db.pending_clone_graph().unwrap(),
            "the SAME pass that indexed the edit settled the graph via the in-place delta"
        );
        let (generations, live_generation, absorbed): (i64, i64, i64) = db
            .storage
            .connection()
            .query_row(
                "SELECT COUNT(*), MAX(generation), MAX(delta_files_applied)
                   FROM clone_graph_generations WHERE status = 'Complete'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(generations, 1);
        assert_eq!(
            live_generation, built.generation,
            "patched in place — no generation was discarded or rebuilt"
        );
        assert_eq!(absorbed, 1, "the drift counter absorbed the edited file");
    }

    /// Accumulated delta drift past [`CLONE_GRAPH_DRIFT_REBUILD_FILES`] owes a df-epoch refresh:
    /// the graph keeps serving (fresh via deltas), and the full rebuild rides the #472 quiet
    /// window — deferred while edits land, executed on the first quiet-elapsed pass (the
    /// reconcile pass must NOT skip-as-current a drifted generation).
    #[test]
    fn drift_past_the_limit_schedules_a_quiet_gated_full_rebuild() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-drift-rebuild");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        let built = db.precompute_clone_graph(None).unwrap();
        assert_eq!(built.status, "Complete");
        db.storage
            .connection()
            .execute(
                "UPDATE clone_graph_generations SET delta_files_applied = ?1",
                rusqlite::params![super::CLONE_GRAPH_DRIFT_REBUILD_FILES],
            )
            .unwrap();
        drop(db);

        // A content pass inside the quiet window: the delta settles freshness in place; the
        // drift-owed FULL rebuild stays deferred. #479: the new file's tokens DO enter the LIVE
        // df immediately (the incremental bump), but the generation's PINNED epoch — what its
        // postings are ordered by — still predates them; that gap is exactly the drift the
        // counter measures.
        std::fs::write(
            config.root.join("src/i.rs"),
            "pub fn drift_probe(q: i64) -> i64 { q * 11 - 6 }\n",
        )
        .unwrap();
        crate::watch::maintenance_pass(&config, false).unwrap();
        let db = crate::IndexDatabase::open_config(&config).unwrap();
        let epoch_count = |db: &crate::IndexDatabase, generation: i64| -> i64 {
            db.storage
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM clone_df_epoch WHERE build_generation = ?1",
                    [generation],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let pinned_epoch = epoch_count(&db, built.generation);
        let live: i64 = db
            .storage
            .connection()
            .query_row(
                "SELECT MAX(generation) FROM clone_graph_generations WHERE status = 'Complete'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live, built.generation, "drift rebuild deferred inside the quiet window");

        // Quiet elapses (backdate the armed candidate): the next idle pass runs the full rebuild
        // — a NEW generation with the drift counter reset.
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
        let (live, absorbed): (i64, i64) = db
            .storage
            .connection()
            .query_row(
                "SELECT MAX(generation), MAX(delta_files_applied)
                   FROM clone_graph_generations WHERE status = 'Complete'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(live > built.generation, "the quiet-elapsed pass ran the drift full rebuild");
        assert_eq!(absorbed, 0, "the fresh generation starts with zero absorbed deltas");
        // The whole POINT of the drift rebuild (PR #477 review): it must move the PINNED epoch,
        // not just reset the counter — the fresh generation's postings are ordered by a df that
        // includes the delta-added file's tokens (#479: the live table already had them from the
        // incremental bump; the rebuild is what folds them into the served order).
        assert!(
            epoch_count(&db, live) > pinned_epoch,
            "the drift full rebuild re-pins the epoch with the delta-added tokens"
        );
    }

    /// The generation bookkeeping: an applied delta bumps `source_revision` to current (making
    /// the write-time postings fast path eligible again) and counts the absorbed files in
    /// `delta_files_applied` (the df-drift signal for scheduling the next full rebuild).
    #[test]
    fn clone_graph_delta_updates_generation_bookkeeping() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-bookkeeping");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        drop(db);

        std::fs::write(
            config.root.join("src/g.rs"),
            "pub fn bookkeeping_probe(q: i64) -> i64 { q * 3 - 4 }\n",
        )
        .unwrap();
        let db = reindex(&config);
        assert!(
            db.clone_check_indexed_generation().unwrap().is_none(),
            "stale revision → write-time fast path ineligible before the delta"
        );
        assert_eq!(db.apply_clone_graph_delta(64).unwrap().status, "Applied");
        assert!(
            db.clone_check_indexed_generation().unwrap().is_some(),
            "the applied delta restores exact freshness (source_revision == content_revision)"
        );
        let applied: i64 = db
            .storage
            .connection()
            .query_row(
                "SELECT delta_files_applied FROM clone_graph_generations WHERE status = 'Complete'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(applied, 1, "the drift counter absorbed one file");
        assert!(!db.pending_clone_graph().unwrap(), "the graph reads as current after the delta");
    }
    /// #598: the file-count Escalate bail can't see posting fan-out — a ≤64-file delta whose bags
    /// hit hot tokens hydrates candidate posting lists uncapped (build parity forbids filtering)
    /// and was observed pinning a core for 38+ minutes under the write lock. The delta now also
    /// escalates when hydration REQUESTS more posting rows than its work budget — same escape as
    /// the file-count bail: nothing written, the caller schedules the (budgeted, resumable) full
    /// rebuild.
    #[test]
    fn delta_escalates_when_posting_hydration_exceeds_the_work_budget() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-work-budget");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        drop(db);

        // A near-clone family member: its bag's tokens hit the corpus postings, so hydration
        // requests a non-zero number of posting rows — more than a zero budget allows.
        std::fs::write(
            config.root.join("src/c.rs"),
            "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\n",
        )
        .unwrap();
        let db = reindex(&config);
        let before = edge_keys(&db);

        let report = db.apply_clone_graph_delta_with_budget(64, 0).unwrap();
        assert_eq!(report.status, "Escalate", "budget exhaustion escalates: {report:?}");
        assert!(
            report.reason.as_deref().is_some_and(|r| r.contains("work budget")),
            "the reason names the work budget: {report:?}"
        );
        assert!(report.posting_rows_requested > 0, "the bail happened because rows were owed");
        assert_eq!(edge_keys(&db), before, "an escalated delta writes nothing");

        // The default budget applies the same delta — the bail is about pathology, not size 1.
        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(report.status, "Applied", "{report:?}");
    }

    /// #598: posting lists are hydrated ONCE per token per delta application — bags sharing hot
    /// tokens re-use the cached rows instead of re-walking the same b-tree lists (the observed
    /// pathology re-walked ~3k-row lists once per bag). `posting_rows_requested` deliberately
    /// still counts cache hits (it is the combinatorics proxy the work budget meters), so
    /// memoization shows as fetched < requested.
    #[test]
    fn posting_hydration_memoizes_shared_tokens_across_bags() {
        let _poison = crate::index::poison_sibling::disable_poison_sibling();
        let config = clone_fixture_config("delta-hydration-memo");
        let db = crate::IndexDatabase::rebuild(&config).unwrap();
        assert_eq!(db.precompute_clone_graph(None).unwrap().status, "Complete");
        drop(db);

        // TWO near-identical family members in the delta: their sub-block prefixes share tokens,
        // so the second bag's hydration must hit the first's cached posting lists.
        std::fs::write(
            config.root.join("src/c.rs"),
            "pub fn load_invoice(db: Db) -> i32 { let v = db.get(30); validate(v); v + 1 }\npub \
             fn load_receipt(db: Db) -> i32 { let r = db.get(40); validate(r); r + 1 }\n",
        )
        .unwrap();
        let db = reindex(&config);

        let report = db.apply_clone_graph_delta(64).unwrap();
        assert_eq!(report.status, "Applied", "{report:?}");
        assert!(report.posting_rows_fetched > 0, "hydration touched the postings: {report:?}");
        assert!(
            report.posting_rows_fetched < report.posting_rows_requested,
            "shared tokens are served from the per-application cache: {report:?}"
        );
    }
}
