//! The clone-detection candidate substrate: the loaded fingerprint bag ([`SymbolBag`] /
//! [`TokenPosting`]) and the SourcererCC candidate-pair algorithm (design rev 4 §3b) that turns
//! scoped baseline fingerprints into `(a, b)` candidate pairs and union-found components.
//!
//! The candidate read is a `struct_hash` exact fast path ([`add_struct_hash_pairs`]), a
//! deterministic-total-order sub-block filter over scoped baseline postings
//! ([`sub_block_candidate_pairs`]), and an EXACT max-denominator overlap verify ([`verified_clone`]
//! / [`overlap`]). df is a *selectivity hint only* — admissibility comes from the shared total
//! order plus the exact verify, so a missing/stale df never drops a true clone. These primitives
//! are the shared substrate the `precompute` (persisted clone-edge graph) and `of_text`
//! (arbitrary-text clone check) child modules reuse, guaranteeing the persisted / text-checked set
//! equals the live [`candidate_pairs_from_bags`] set.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rayon::prelude::*;
use rusqlite::Connection;

use super::THETA;
use crate::index::clones::NORM_VERSION;

/// Pairwise metric work cap for huge components: when a component exceeds this count, the
/// O(n²) pairwise metric loop (`similarity_min`, medoid, `similarity_medoid_min`,
/// `containment_max`) runs over ONLY the first `METRIC_SAMPLE_CAP` members instead of the full
/// upper triangle. `member_count` / `total_members` / `class_key` still reflect the FULL
/// component; only the metric computation is sampled, and `metrics_sampled` is set to `true` on
/// the returned class so callers can distinguish sampled from exact metrics.
///
/// For typical-size components (the overwhelming common case, and ALL existing tests) this cap is
/// never reached: behavior is identical to the pre-cap code and `metrics_sampled` is `false`.
pub(crate) const METRIC_SAMPLE_CAP: usize = 200;

/// Sentinel df for tokens with no `clone_token_df` row (LEFT JOIN miss). i64::MAX sorts them LAST
/// in `(coalesced_df ASC, token_hash ASC)` order — they are treated as maximally common (least
/// selective), which is the conservative choice: it can only widen the sub-block, never shrink it,
/// so no candidate is dropped. df is selectivity-only; correctness depends on the shared total
/// order + exact verify, never on df accuracy (design rev-4 §2).
pub(crate) const DF_FALLBACK: i64 = i64::MAX;

/// #271: a sub-block token whose postings list exceeds this is treated as NON-DISCRIMINATING and
/// emits NO candidate pairs. The inverted index emits a token's full upper triangle (K postings →
/// K²/2 pairs); `df` only HINTS rarity (`DF_FALLBACK` for missing-df tokens, and a short symbol
/// near `MIN_TOKENS` puts its whole bag in the sub-block), so a hot token can still land in
/// thousands of sub-blocks and blow up candidate generation (measured: drivers/net, 5.67M pairs
/// from 2,874 fns). Recall-safe: a pair sharing ONLY a hot token is low-similarity and fails the
/// exact overlap verify anyway, and any GENUINE clone pair also shares rarer tokens (each well
/// under this cap) so it is still generated via those. The cap is far above any realistic
/// clone-family size, so a real repeated-block family is never silently dropped. ABSOLUTE (not a
/// fraction of the corpus): a token in this many functions' rarest-token sets is non-discriminating
/// regardless of corpus size, and an absolute bound keeps per-token emission O(cap²) instead of
/// growing with N².
pub(crate) const HOT_TOKEN_POSTINGS_CAP: usize = 256;

/// One scoped baseline symbol's fingerprint, loaded for the candidate read.
#[derive(Clone)]
pub(crate) struct SymbolBag {
    pub(crate) symbol_id: i64,
    pub(crate) language: String,
    pub(crate) struct_hash: String,
    pub(crate) token_len: i64,
    /// `(token_hash, freq, coalesced_df)` for every distinct token in the symbol's bag.
    pub(crate) tokens: Vec<TokenPosting>,
}

#[derive(Clone)]
pub(crate) struct TokenPosting {
    pub(crate) token_hash: i64,
    pub(crate) freq: i64,
    pub(crate) coalesced_df: i64,
}

/// Extract candidate pairs from already-loaded bags (avoids a second DB round-trip in
/// `find_clones` vs the original `candidate_pairs` path). `theta` is the similarity threshold
/// applied to both the sub-block prefix length and the exact overlap/max verify — passing the
/// caller's `min_similarity` widens (or narrows) candidate generation to match the requested
/// floor, instead of generating at the const [`THETA`] and post-filtering.
pub(crate) fn candidate_pairs_from_bags(bags: &[SymbolBag], theta: f64) -> Vec<(i64, i64)> {
    let mut pairs: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();
    add_struct_hash_pairs(bags, &mut pairs);
    let candidate = sub_block_candidate_pairs(bags, theta);
    let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
    // Verify candidates in parallel: `verified_clone` is pure token math (no DB, no shared
    // mutation; `by_id` is read-only), so the candidate set — the bulk of candidate-gen cost —
    // fans out across cores. Output stays deterministic: the verified pairs land in the sorted
    // `pairs` BTreeSet regardless of completion order.
    let verified: Vec<(i64, i64)> = candidate
        .into_par_iter()
        .filter(|&(a, b)| verified_clone(by_id[&a], by_id[&b], theta))
        .collect();
    pairs.extend(verified);
    pairs.into_iter().collect()
}

/// Candidate pairs for a query: the persisted clone-graph FAST PATH (#286) when one is eligible
/// (present, fresh-enough, θ≥0.7, base scope), else the live [`candidate_pairs_from_bags`]
/// recompute. The fast path is a pure optimization — it returns the SAME pair set the live path
/// would (the parity test pins this), so every downstream stage (`components_from_pairs` →
/// `coherence_split` → `build_class` → refine) is identical regardless of which source produced the
/// pairs.
pub(crate) fn pairs_for_query(
    conn: &Connection,
    bags: &[SymbolBag],
    theta: f64,
) -> anyhow::Result<Vec<(i64, i64)>> {
    let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
    if let Some(pairs) = super::precompute::precomputed_pairs_if_eligible(conn, &by_id, theta)? {
        return Ok(pairs);
    }
    Ok(candidate_pairs_from_bags(bags, theta))
}

/// `(symbol_id, symbol_id)` candidate pairs (a < b), both within the scoped `files` view.
///
/// Combines the `struct_hash` exact fast path with the sub-block + exact-verify candidate read
/// (design rev 4 §3b). Returns deduplicated `(a, b)` pairs with `a < b` for the union-find.
pub(crate) fn candidate_pairs(conn: &Connection) -> anyhow::Result<Vec<(i64, i64)>> {
    // `candidate_clone_components` keeps the const THETA; the persisted-graph fast path serves it
    // when eligible, else the live SourcererCC recompute (struct-hash + sub-block@THETA + exact
    // verify) runs in `candidate_pairs_from_bags`.
    let bags = load_scoped_baseline_bags(conn)?;
    pairs_for_query(conn, &bags, THETA)
}

/// Load every scoped baseline symbol's fingerprint + full token bag with LEFT-JOINed df. Both the
/// fingerprint and its postings are filtered to the scoped `files` view through `symbols.file_id`,
/// so only the ACTIVE version of each file participates (SCOPED-VIEW REQUIREMENT #89). df is read
/// via LEFT JOIN + COALESCE so a missing-df token is never dropped (design rev-4 §2).
pub(crate) fn load_scoped_baseline_bags(conn: &Connection) -> anyhow::Result<Vec<SymbolBag>> {
    // Load `clone_token_df` ONCE into a map (#231 R3): the token bag now lives in an opaque
    // `token_bag` BLOB, so df can no longer be a per-token SQL JOIN. Each decoded token's df is
    // looked up here in Rust and COALESCEd to the fallback sentinel — a missing-df token must NOT
    // be dropped (design rev-4 §2). Only the baseline normalizer feeds candidate recall.
    // Post-A5 df is per-repo (its PK carries `repo_id`), so scope the read to the active repo —
    // `{df_repo_clause}` is empty pre-A5. The writer (`refresh_clone_token_df` / the incremental
    // bump) stamps the same repo, so the two agree.
    let df_scope = crate::index::schema::periphery_repo_scope(conn, "clone_token_df")?;
    let df_repo_clause =
        crate::index::schema::periphery_repo_scope_clause(&df_scope, "clone_token_df");
    let mut df_stmt = conn.prepare(&format!(
        "SELECT token_hash, df FROM clone_token_df WHERE normalizer_kind = \
         'baseline'{df_repo_clause}"
    ))?;
    let df_by_token: std::collections::HashMap<i64, i64> = df_stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
        .collect::<Result<_, _>>()?;

    // Scoped baseline fingerprints + their token-bag BLOB, in one read (no per-token join).
    // `files.generated = 0` excludes generated files (e.g. `src/generated/…`, `.d.ts`) from the
    // candidate read. As of #232 #6 generated files are NO LONGER fingerprinted at index time
    // (`prep.rs` / `file_index.rs` gate the compute on `!file_is_generated`), so this filter is now
    // defense-in-depth: it still guards a file that flipped to `generated = 1` AFTER its
    // fingerprints were written (a target reclassification without a reindex of that file).
    // `token_len` comes from the COLUMN (R3); the bag itself is decoded from the BLOB.
    let mut fp_stmt = conn.prepare(
        "SELECT sf.symbol_id, symbols.language, sf.struct_hash, sf.token_len, sf.token_bag
         FROM symbol_fingerprints sf
         JOIN symbols ON symbols.id = sf.symbol_id
         JOIN files ON files.id = symbols.file_id
         WHERE sf.normalizer_kind = 'baseline'
           AND sf.normalizer_version = ?1
           AND files.generated = 0",
    )?;
    let mut bags: Vec<SymbolBag> = Vec::new();
    let mut rows = fp_stmt.query([NORM_VERSION])?;
    while let Some(row) = rows.next()? {
        // R4: a NULL `token_bag` (un-reindexed after the V032 migration) is a NO-BAG row — SKIP it
        // (not an empty bag, no panic). Byte-identical recall holds only for a FULLY (re)indexed
        // DB; clone recall is undefined for NULL-bag symbols until the post-migration
        // reindex.
        let Some(blob) = row.get::<_, Option<Vec<u8>>>(4)? else {
            continue;
        };
        let Some(bag_pairs) = crate::index::clones::bag_blob::decode_token_bag(&blob) else {
            // A stale/corrupt blob (version mismatch / truncation) decodes to None — treat as
            // no-bag, same as NULL. It is repopulated on the next reindex.
            continue;
        };
        let tokens: Vec<TokenPosting> = bag_pairs
            .into_iter()
            .map(|(token_hash, freq)| TokenPosting {
                token_hash,
                freq,
                coalesced_df: df_by_token.get(&token_hash).copied().unwrap_or(DF_FALLBACK),
            })
            .collect();
        // The BLOB is stored token_hash-sorted (the producer's invariant), which is exactly the
        // order `overlap`'s two-pointer merge and `sub_block_tokens` expect — so no re-sort is
        // needed on read. Assert it in debug to catch a producer regression.
        debug_assert!(
            tokens.windows(2).all(|w| w[0].token_hash <= w[1].token_hash),
            "decoded token_bag must be token_hash-sorted"
        );
        bags.push(SymbolBag {
            symbol_id: row.get(0)?,
            language: row.get(1)?,
            struct_hash: row.get(2)?,
            token_len: row.get(3)?,
            tokens,
        });
    }

    // Return bags in `symbol_id` order, matching the prior `BTreeMap<symbol_id, _>` keyset (the SQL
    // has no ORDER BY). The candidate set is order-independent (it dedups into a BTreeSet), but a
    // stable order keeps any incidental iteration deterministic.
    bags.sort_unstable_by_key(|bag| bag.symbol_id);
    Ok(bags)
}

/// Exact fast path: every group of symbols sharing a `struct_hash` AND `language` is
/// identical-after-normalization, so it contributes all its pairwise pairs (no overlap math).
/// Language partition is required: different languages share no grammar token space, so a
/// struct_hash collision across languages is a false positive.
pub(crate) fn add_struct_hash_pairs(
    bags: &[SymbolBag],
    pairs: &mut std::collections::BTreeSet<(i64, i64)>,
) {
    // Key: (struct_hash, language) — only same-language symbols can be struct-hash clones.
    let mut by_hash: BTreeMap<(&str, &str), Vec<i64>> = BTreeMap::new();
    for bag in bags {
        by_hash
            .entry((bag.struct_hash.as_str(), bag.language.as_str()))
            .or_default()
            .push(bag.symbol_id);
    }
    for ids in by_hash.values() {
        for (i, &a) in ids.iter().enumerate() {
            for &b in &ids[i + 1..] {
                pairs.insert((a.min(b), a.max(b)));
            }
        }
    }
}

/// Build the inverted index over sub-block tokens only and emit candidate pairs `(a < b)` for every
/// pair of symbols sharing a sub-block token. Admissibility (design rev-4 §3b): two symbols can
/// reach similarity ≥ θ only if their sub-blocks share a token hash, so this yields every true
/// candidate pair regardless of df accuracy (given the shared total order).
///
/// Language partition: only same-language pairs are emitted — different languages have disjoint
/// grammar token spaces, so a token-hash collision across languages is a false positive.
/// Returns the candidate pair set as a SORTED, deduplicated `Vec` (canonical `a < b`). The upper-
/// triangle emission per posting list is the dominant candidate-gen cost (millions of pairs on a
/// dense corpus — ~4.5M / ~11s release on cargo's `src/cargo`, vs ~0.5s to verify them), so it runs
/// in PARALLEL: each posting list is independent, so `par_iter` over the inverted index emits each
/// list's pairs across cores, then one `par_sort_unstable` + `dedup` canonicalizes the set.
/// Deterministic: the sort+dedup is order-independent, so the result is byte-identical regardless
/// of completion order (returns a sorted `Vec`, not a `BTreeSet`, to keep the merge parallel — a
/// 4.5M-element `BTreeSet` build is sequential).
pub(crate) fn sub_block_candidate_pairs(bags: &[SymbolBag], theta: f64) -> Vec<(i64, i64)> {
    // id → language for the partition guard applied at pair-emit time.
    let lang_of: BTreeMap<i64, &str> =
        bags.iter().map(|b| (b.symbol_id, b.language.as_str())).collect();

    let mut inverted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for bag in bags {
        for token_hash in sub_block_tokens(bag, theta) {
            inverted.entry(token_hash).or_default().push(bag.symbol_id);
        }
    }

    let mut candidate: Vec<(i64, i64)> = inverted
        .par_iter()
        // #271: a token in more than HOT_TOKEN_POSTINGS_CAP sub-blocks is non-discriminating — its
        // K²/2 pairs are noise that fails verify, and real clones are still found via their rarer
        // shared tokens. Skip it to bound candidate generation on dense corpora.
        .filter(|(_token, ids)| ids.len() <= HOT_TOKEN_POSTINGS_CAP)
        .flat_map_iter(|(_token, ids)| {
            let mut local: Vec<(i64, i64)> = Vec::new();
            for (i, &a) in ids.iter().enumerate() {
                for &b in &ids[i + 1..] {
                    // Language partition: skip cross-language pairs.
                    if lang_of[&a] == lang_of[&b] {
                        local.push((a.min(b), a.max(b)));
                    }
                }
            }
            local
        })
        .collect();
    candidate.par_sort_unstable();
    candidate.dedup();
    candidate
}

/// A symbol's sub-block: the distinct token hashes whose occurrences reach into the first `p`
/// occurrences under the deterministic total order `(coalesced_df ASC, token_hash ASC)`.
///
/// `p = token_len - ceil(theta * token_len) + 1` is the sub-block OCCURRENCE length (clamped to ≥
/// 0; if `p >= token_len` the whole bag is the sub-block). The sub-block is defined over EXPANDED
/// token occurrences (Σ freq), not distinct posting rows, so it matches the multiset `Σ min(freq)`
/// verifier (design rev-4 §3): walking distinct tokens in order accumulating `freq`, a token is
/// included if the running occurrence-count BEFORE it is `< p` (i.e. any of its occurrences falls
/// in the prefix).
pub(crate) fn sub_block_tokens(bag: &SymbolBag, theta: f64) -> Vec<i64> {
    let p = sub_block_len(bag.token_len, theta);
    if p <= 0 {
        return Vec::new();
    }

    let mut ordered: Vec<&TokenPosting> = bag.tokens.iter().collect();
    ordered.sort_by_key(|t| (t.coalesced_df, t.token_hash));

    let mut sub_block = Vec::new();
    let mut occurrences_before: i64 = 0;
    for token in ordered {
        // Include this token if any of its occurrences falls within the first `p` occurrences, i.e.
        // the running count BEFORE it is still inside the prefix.
        if occurrences_before < p {
            sub_block.push(token.token_hash);
        } else {
            break; // every later token starts past the prefix too (occurrences only grow).
        }
        occurrences_before += token.freq;
    }
    sub_block
}

/// Sub-block occurrence length `p = token_len - ceil(theta * token_len) + 1`, clamped to ≥ 0.
fn sub_block_len(token_len: i64, theta: f64) -> i64 {
    let threshold = (theta * token_len as f64).ceil() as i64;
    (token_len - threshold + 1).max(0)
}

/// Size prune + EXACT max-denominator verify (design rev-4 §3b). With `min_len`/`max_len` = the two
/// token_lens: cheap size prune `min_len >= ceil(theta * max_len)`; then `overlap = Σ min(freq_a,
/// freq_b)` over the FULL bags, kept iff `overlap >= ceil(theta * max_len)`. The GATE is
/// `similarity = overlap / max_len`; containment = `overlap / min_len` is NOT gated here.
pub(crate) fn verified_clone(a: &SymbolBag, b: &SymbolBag, theta: f64) -> bool {
    let min_len = a.token_len.min(b.token_len);
    let max_len = a.token_len.max(b.token_len);
    let threshold = (theta * max_len as f64).ceil() as i64;

    // Size prune: a smaller block can't reach θ against a larger one.
    if min_len < threshold {
        return false;
    }

    overlap(a, b) >= threshold
}

/// Exact multiset overlap `Σ min(freq_a, freq_b)` over the two FULL token bags.
///
/// Requires both bags' `tokens` slices to be sorted by `token_hash` ascending (guaranteed by
/// the encoded token-bag BLOB, which is sorted at encode time by `tokens::token_bag` and
/// asserted by the `debug_assert` in `bag_blob::encode_token_bag`). Uses an allocation-free
/// two-pointer merge: no `BTreeMap` rebuild per call — O(|a| + |b|) time, zero heap allocation.
pub(crate) fn overlap(a: &SymbolBag, b: &SymbolBag) -> i64 {
    let (mut ia, mut ib) = (0, 0);
    let (ta, tb) = (a.tokens.as_slice(), b.tokens.as_slice());
    let mut total: i64 = 0;
    while ia < ta.len() && ib < tb.len() {
        match ta[ia].token_hash.cmp(&tb[ib].token_hash) {
            std::cmp::Ordering::Less => ia += 1,
            std::cmp::Ordering::Greater => ib += 1,
            std::cmp::Ordering::Equal => {
                total += ta[ia].freq.min(tb[ib].freq);
                ia += 1;
                ib += 1;
            },
        }
    }
    total
}

/// Union-find the pairs into components of size >= 2 (sorted for determinism).
pub(crate) fn components_from_pairs(pairs: &[(i64, i64)]) -> Vec<Vec<i64>> {
    use std::collections::BTreeMap;

    fn find(parent: &mut BTreeMap<i64, i64>, x: i64) -> i64 {
        let mut root = x;
        while let Some(&p) = parent.get(&root) {
            if p == root {
                break;
            }
            root = p;
        }
        let mut cur = x;
        while let Some(&p) = parent.get(&cur) {
            if p == root {
                break;
            }
            parent.insert(cur, root);
            cur = p; // p captured before the insert, so advancing to the pre-compression parent is safe
        }
        root
    }

    let mut parent: BTreeMap<i64, i64> = BTreeMap::new();
    for &(a, b) in pairs {
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent.insert(ra.max(rb), ra.min(rb));
        }
    }
    let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let members: Vec<i64> = parent.keys().copied().collect(); // collect keys first: find() needs &mut parent
    for member in members {
        let root = find(&mut parent, member);
        groups.entry(root).or_default().push(member);
    }
    groups
        .into_values()
        .filter(|g| g.len() >= 2)
        .map(|mut g| {
            g.sort_unstable();
            g
        })
        .collect()
}

/// The `subject` symbol's connected component + its verified θ-edges, computed by a BFS from the
/// subject over the SAME candidate graph the full [`candidate_pairs_from_bags`] builds — instead of
/// generating EVERY candidate pair and EVERY component and then `.position()`-ing the subject's
/// (#270). The two inverted indexes are built once over all bags (O(scope), unavoidable — it's what
/// bounds the win to "still load + index the scope"), then only the subject's component is explored
/// (O(component)). The largest win is when the subject's component is a small fraction of the repo.
///
/// EQUIVALENCE (pinned by a test): the returned `(component, edges)` equals what
/// `candidate_pairs_from_bags` → `components_from_pairs` (pick the subject's component) +
/// `bucket_edges_by_component` would produce for the subject — every incident edge is generated by
/// exactly the same two rules: (a) same `(struct_hash, language)` → automatic edge (no verify), and
/// (b) a shared NON-hot sub-block token (posting ≤ [`HOT_TOKEN_POSTINGS_CAP`], same language) that
/// passes [`verified_clone`]. `component` has length < 2 when the subject cohered with no peer (the
/// analog of `components_from_pairs` dropping size-1 groups / the old `.position` miss).
pub(crate) fn subject_component_bfs(
    by_id: &BTreeMap<i64, &SymbolBag>,
    subject: i64,
    theta: f64,
) -> (Vec<i64>, Vec<(i64, i64)>) {
    // Index 1: (struct_hash, language) → symbol_ids. Same-hash same-language symbols are
    // identical-after-normalization, so each such pair is an edge with no overlap math
    // (mirrors `add_struct_hash_pairs`).
    let mut by_hash: BTreeMap<(&str, &str), Vec<i64>> = BTreeMap::new();
    // Index 2: sub_block_token → symbol_ids. A pair is a sub-block CANDIDATE iff they share a token
    // in this index; hot tokens (posting > cap) are dropped at emit time, exactly as
    // `sub_block_candidate_pairs` filters, so BFS and the full scan see the same candidate set.
    let mut inverted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for bag in by_id.values() {
        by_hash
            .entry((bag.struct_hash.as_str(), bag.language.as_str()))
            .or_default()
            .push(bag.symbol_id);
        for token_hash in sub_block_tokens(bag, theta) {
            inverted.entry(token_hash).or_default().push(bag.symbol_id);
        }
    }

    let mut edges: BTreeSet<(i64, i64)> = BTreeSet::new();
    let mut visited: BTreeSet<i64> = BTreeSet::new();
    let mut frontier: VecDeque<i64> = VecDeque::new();
    if by_id.contains_key(&subject) {
        visited.insert(subject);
        frontier.push_back(subject);
    }
    while let Some(x) = frontier.pop_front() {
        let bx = by_id[&x];
        // (a) struct-hash edges: every same-(struct_hash, language) peer, no verify.
        if let Some(ids) = by_hash.get(&(bx.struct_hash.as_str(), bx.language.as_str())) {
            for &y in ids {
                if y == x {
                    continue;
                }
                edges.insert((x.min(y), x.max(y)));
                if visited.insert(y) {
                    frontier.push_back(y);
                }
            }
        }
        // (b) sub-block edges: peers sharing one of x's NON-hot sub-block tokens, same language,
        // that pass the exact `verified_clone` gate.
        for token_hash in sub_block_tokens(bx, theta) {
            let Some(ids) = inverted.get(&token_hash) else {
                continue;
            };
            if ids.len() > HOT_TOKEN_POSTINGS_CAP {
                continue; // hot token dropped, matching `sub_block_candidate_pairs`
            }
            for &y in ids {
                if y == x {
                    continue;
                }
                let by = by_id[&y];
                if bx.language != by.language {
                    continue; // language partition
                }
                if verified_clone(bx, by, theta) {
                    edges.insert((x.min(y), x.max(y)));
                    if visited.insert(y) {
                        frontier.push_back(y);
                    }
                }
            }
        }
    }
    // `visited` == the connected component (BTreeSet → sorted, matching `components_from_pairs`).
    (visited.into_iter().collect(), edges.into_iter().collect())
}

/// Partition the θ-verified candidate `pairs` into per-component edge lists, parallel to
/// `components` (entry `i` holds the edges whose endpoints belong to `components[i]`) (#256).
///
/// The coherence split seeds its clique cover from a component's edge list; supplying the
/// precomputed edges makes seeding O(edges) instead of the old O(n²) all-pairs scan (the reason the
/// removed `SPLIT_MAX` member cap existed). Both endpoints of a pair share a component by
/// construction (`components_from_pairs` union-finds the same pairs), so a node→component-index map
/// resolves every edge in ONE O(|pairs|) pass. An edge whose endpoints fall in a dropped singleton
/// component (not in the map) is skipped — it can never appear, but the guard keeps the partition
/// total.
pub(crate) fn bucket_edges_by_component(
    pairs: &[(i64, i64)],
    components: &[Vec<i64>],
) -> Vec<Vec<(i64, i64)>> {
    let mut node_to_component: BTreeMap<i64, usize> = BTreeMap::new();
    for (idx, component) in components.iter().enumerate() {
        for &node in component {
            node_to_component.insert(node, idx);
        }
    }
    let mut edges_by_component: Vec<Vec<(i64, i64)>> = vec![Vec::new(); components.len()];
    for &(a, b) in pairs {
        if let Some(&idx) = node_to_component.get(&a) {
            // `a` and `b` are unioned into the same component, so indexing by `a` is correct.
            edges_by_component[idx].push((a, b));
        }
    }
    edges_by_component
}
