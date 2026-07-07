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

use rusqlite::{Connection, params};
use serde::Serialize;

use super::THETA;
use super::substrate::{
    SymbolBag, load_scoped_baseline_bags, overlap, sub_block_tokens, verified_clone,
};
use crate::index::IndexDatabase;
use crate::index::clones::NORM_VERSION;
use crate::index::util::now_ms;

/// θ the graph is precomputed at — the default `find_clones` threshold. Queries at θ ≥ this read
/// the stored edges (filtering the exact gate inputs); θ below falls back to the live path.
pub(crate) const CLONE_PRECOMPUTE_THETA: f64 = THETA;

const DEFAULT_BATCH_SIZE: usize = 512;

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
type Anchor = (String, i64, String);

struct EdgeRow {
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
struct PostingGroup {
    anchor: Anchor,
    tokens: Vec<i64>,
}

struct GenerationRow {
    generation: i64,
    source_revision: String,
    normalizer_version: i64,
    cursor_symbol_id: i64,
    edges_written: u64,
    /// Whether this generation is POSTINGS-AWARE (#296 phase 2): its `clone_subblock_postings` are
    /// written in-band by a postings-aware binary. Set to 1 at Building creation and preserved
    /// through `Complete`. A generation created before the feature has `postings_written = 0`.
    /// Because a postings-aware binary writes every walked symbol's postings BEFORE advancing the
    /// cursor, a *Complete* postings-aware generation is fully populated — so the live-generation
    /// completeness gate (`pending_clone_graph` / the Phase-0 skip) reads this as "postings
    /// complete", and a `Building` one reads it as "resumable without a postings gap" (review R2).
    postings_written: bool,
}

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

    /// True when the precomputed graph is ABSENT or STALE vs the current content — the gate the
    /// watcher / maintenance pass uses to decide whether to spend budget on a recompute.
    pub fn pending_clone_graph(&self) -> anyhow::Result<bool> {
        let conn = self.storage.connection();
        let Some(live) = live_generation_row(conn)? else {
            return Ok(true); // no completed generation yet
        };
        Ok(live.source_revision != self.content_revision()?
            || live.normalizer_version != NORM_VERSION
            // A live generation built before postings existed (`postings_written = 0`) is pending
            // so one rebuild pass fills `clone_subblock_postings` — else an upgraded DB with an
            // already-Complete clone graph would keep an empty postings table forever (review R2).
            || !live.postings_written)
    }

    /// Invalidate the persisted clone-graph postings — mark EVERY generation postings-stale
    /// (`postings_written = 0`) so `pending_clone_graph` reports pending and the next maintenance
    /// pass rebuilds the graph. Called when `clone_token_df` is recomputed (a full rebuild): the
    /// postings freeze `sub_block_tokens`' ordering by the df table AS OF their build, but a full
    /// rebuild corrects df drift WITHOUT changing file content, so `content_revision()` (the
    /// clone-graph freshness key) stays equal and nothing else would catch the drift. Serving those
    /// postings would rank a new function with the current df while reading postings ordered by the
    /// old df, silently missing near-clones whose selected sub-block token shifted. Until the
    /// rebuild, the write-time fast path falls back to the RAM build; `find_clones`' edges are
    /// df-INDEPENDENT (they store the verified overlap) and are unaffected. Incremental indexing
    /// bumps df alongside file-content changes, so `content_revision()` already gates that path —
    /// only this content-invariant refresh needs the explicit nudge.
    pub(crate) fn invalidate_clone_graph_postings(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        // Per-repo (A5): only the active repo's generations are invalidated — a full rebuild of one
        // repo must not force a sibling repo's postings stale. `{repo_clause}` is empty pre-A5.
        let repo_clause = clone_generation_scope_clause(conn)?;
        conn.execute(
            &format!(
                "UPDATE clone_graph_generations SET postings_written = 0 WHERE 1=1{repo_clause}"
            ),
            [],
        )?;
        Ok(())
    }

    /// The live clone-graph generation the write-time postings fast path may read from —
    /// `Some(gen)` ONLY when the persisted postings are safe to serve, `None` otherwise (→ the
    /// caller uses the RAM fallback). Eligibility is EXACT-freshness, deliberately STRICTER
    /// than the `find_clones` edge fast path's "mildly-stale-OK" (review R1): the persisted
    /// sub-block token set for a symbol depends on `sub_block_tokens`' ordering by the CURRENT
    /// `clone_token_df`, so a generation whose `source_revision` has drifted from
    /// `content_revision()` could disagree with what the live index would compute — a silent
    /// missed near-clone. So require:
    /// - a `Complete` live generation (the meta live-pointer only ever names a Complete one),
    /// - `normalizer_version == NORM_VERSION`,
    /// - `postings_written` (a postings-complete, postings-aware generation — review R2), AND
    /// - `source_revision == content_revision()` EXACTLY (not merely present).
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
            && live.source_revision == self.content_revision()?;
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

        // Load the scoped baseline bags + the content anchors for every scoped symbol, and build
        // the struct-hash buckets + sub-block inverted index in RAM (rebuilt each pass —
        // cheap relative to the pair emission this avoids persisting postings for).
        let bags = load_scoped_baseline_bags(conn)?;
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
fn make_edge(
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
        let mut inserted = 0u64;
        {
            let mut stmt = conn.prepare_cached(
                "INSERT OR IGNORE INTO clone_edges
                    (build_generation, a_path, a_start_byte, a_file_sha, b_path, b_start_byte,
                     b_file_sha, overlap, a_token_len, b_token_len, similarity, edge_source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for e in batch.iter() {
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
        }
        {
            let mut stmt = conn.prepare_cached(
                "INSERT OR IGNORE INTO clone_subblock_postings
                    (build_generation, token_hash, path, start_byte, file_sha)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for g in postings.iter() {
                let (path, start_byte, file_sha) = &g.anchor;
                for &token_hash in &g.tokens {
                    stmt.execute(params![generation, token_hash, path, start_byte, file_sha])?;
                }
            }
        }
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

/// The ` AND clone_graph_generations.repo_id = '…'` predicate for the per-repo generation SWEEPS,
/// or `""` on the pre-A5 schema (the generations table is still repo-global). The generation
/// INTEGER stays globally unique (allocated `MAX(generation)+1` over ALL repos), so the transitive
/// `clone_edges` / `clone_subblock_postings` are scoped for free by `build_generation`; only the
/// generation lifecycle sweeps (allocate/build/complete/invalidate) filter `repo_id` so a repo's
/// precompute never touches a sibling's generations. See `schema::periphery_repo_scope`.
fn clone_generation_scope_clause(conn: &Connection) -> anyhow::Result<String> {
    let scope = crate::index::schema::periphery_repo_scope(conn, "clone_graph_generations")?;
    Ok(crate::index::schema::periphery_repo_scope_clause(&scope, "clone_graph_generations"))
}

/// The live (Complete) generation row, if one is published.
fn live_generation_row(conn: &Connection) -> anyhow::Result<Option<GenerationRow>> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    let Some(live) = crate::index::repo_meta(conn, &repo_id, "clone_graph_live_generation")? else {
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
    let scope = crate::index::schema::periphery_repo_scope(conn, "clone_graph_generations")?;
    let repo_clause =
        crate::index::schema::periphery_repo_scope_clause(&scope, "clone_graph_generations");
    let existing: Option<GenerationRow> = conn
        .query_row(
            &format!(
                "SELECT generation, source_revision, normalizer_version, cursor_symbol_id, \
                 edges_written, postings_written
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
    })
}

fn read_generation(conn: &Connection, generation: i64) -> anyhow::Result<Option<GenerationRow>> {
    Ok(conn
        .query_row(
            "SELECT generation, source_revision, normalizer_version, cursor_symbol_id, \
             edges_written, postings_written
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config for a fixed two-file fixture with two renamed-clone groups
    /// (load_user/load_order and compute_totals/tally_amounts). Identical file CONTENT across tags
    /// → identical content-key edges, so two builds are directly comparable. Split out so a
    /// test can `rebuild` the SAME config twice (identical content) to exercise the
    /// full-rebuild df refresh.
    fn clone_fixture_config(tag: &str) -> crate::Config {
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
        crate::Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![crate::config::ResolvedTarget {
                name: "rust".to_string(),
                language: crate::language::Language::Rust,
                directories: vec![std::path::PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: crate::config::TargetKind::Source,
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
    fn edge_keys(db: &crate::IndexDatabase) -> Vec<(String, i64, String, i64)> {
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

    /// A full rebuild recomputes `clone_token_df` authoritatively (correcting incremental drift)
    /// WITHOUT changing file content, so `content_revision()` — the clone-graph freshness key —
    /// stays equal. The persisted postings freeze the OLD df ordering, so serving them under
    /// the fresh df could silently miss near-clones; the rebuild must therefore INVALIDATE the
    /// postings (mark the generation stale) so they rebuild against the fresh df. The
    /// write-time fast path falls back to RAM until then. Self-heals on the next precompute.
    #[test]
    fn full_rebuild_invalidates_postings_for_df_freshness() {
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
        // anchored), but `refresh_clone_token_df` runs, so the postings must be invalidated.
        let db2 = crate::IndexDatabase::rebuild(&config).unwrap();
        assert!(
            db2.pending_clone_graph().unwrap(),
            "the full-rebuild df refresh invalidates the postings, so the graph is pending"
        );
        assert!(
            db2.clone_check_indexed_generation().unwrap().is_none(),
            "and the write-time fast path falls back to RAM until the postings are rebuilt"
        );

        // Self-heals: re-precompute rebuilds the postings against the fresh df.
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
