//! Read layer for clone detection (#215). Plan 1 ships only the candidate-component read that
//! proves the fingerprint substrate; the `find_clones` / `clones_for_symbol` surface is Plan 2.
//!
//! The candidate read is the SourcererCC algorithm (design rev 4 §3b): a `struct_hash` exact fast
//! path, a deterministic-total-order sub-block filter over scoped baseline postings, and an EXACT
//! max-denominator overlap verify. df is a *selectivity hint only* — admissibility comes from the
//! shared total order plus the exact verify, so a missing/stale df never drops a true clone.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::index::IndexDatabase;

/// Similarity threshold θ: a candidate pair is kept iff `overlap / max_len >= THETA`. The MAX
/// denominator is deliberate (design rev-4 §3b) — it bounds the member length ratio to ≈1/θ, the
/// whole-symbol bias, so a tiny helper contained in a giant function (overlap/min ≈ 1.0) is NOT a
/// clone. Tunable later via the query surface.
const THETA: f64 = 0.7;

/// Sentinel df for tokens with no `clone_token_df` row (LEFT JOIN miss). i64::MAX sorts them LAST
/// in `(coalesced_df ASC, token_hash ASC)` order — they are treated as maximally common (least
/// selective), which is the conservative choice: it can only widen the sub-block, never shrink it,
/// so no candidate is dropped. df is selectivity-only; correctness depends on the shared total
/// order + exact verify, never on df accuracy (design rev-4 §2).
const DF_FALLBACK: i64 = i64::MAX;

/// One scoped baseline symbol's fingerprint, loaded for the candidate read.
struct SymbolBag {
    symbol_id: i64,
    struct_hash: String,
    token_len: i64,
    /// `(token_hash, freq, coalesced_df)` for every distinct token in the symbol's bag.
    tokens: Vec<TokenPosting>,
}

struct TokenPosting {
    token_hash: i64,
    freq: i64,
    coalesced_df: i64,
}

impl IndexDatabase {
    /// Candidate clone components over the ACTIVE scope, via the SourcererCC algorithm (design rev
    /// 4 §3b): a `struct_hash` exact fast path plus sub-block-filtered candidate pairs verified
    /// by EXACT max-denominator overlap, union-found into connected components. Both endpoints
    /// are filtered to the scoped `files` view BEFORE pairing, so a component never mixes
    /// out-of-scope symbols. Baseline postings only (recall is oracle-independent).
    /// Over-generated on purpose — Plan 2's refine stage splits each component into coherent
    /// clone classes.
    pub fn candidate_clone_components(&self) -> anyhow::Result<Vec<Vec<i64>>> {
        let conn = self.storage.connection();
        let pairs = candidate_pairs(conn)?;
        Ok(components_from_pairs(&pairs))
    }
}

/// `(symbol_id, symbol_id)` candidate pairs (a < b), both within the scoped `files` view.
///
/// Combines the `struct_hash` exact fast path with the sub-block + exact-verify candidate read
/// (design rev 4 §3b). Returns deduplicated `(a, b)` pairs with `a < b` for the union-find.
fn candidate_pairs(conn: &Connection) -> anyhow::Result<Vec<(i64, i64)>> {
    let bags = load_scoped_baseline_bags(conn)?;

    let mut pairs: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();

    // 1. Exact fast path: every same-struct_hash set contributes all its pairwise pairs.
    add_struct_hash_pairs(&bags, &mut pairs);

    // 2. Sub-block candidate pairs via the inverted index over sub-block tokens only.
    let candidate = sub_block_candidate_pairs(&bags);

    // 3. Size prune + EXACT max-denominator verify over the FULL bags.
    let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
    for (a, b) in candidate {
        let (ba, bb) = (by_id[&a], by_id[&b]);
        if verified_clone(ba, bb) {
            pairs.insert((a, b));
        }
    }

    Ok(pairs.into_iter().collect())
}

/// Load every scoped baseline symbol's fingerprint + full token bag with LEFT-JOINed df. Both the
/// fingerprint and its postings are filtered to the scoped `files` view through `symbols.file_id`,
/// so only the ACTIVE version of each file participates (SCOPED-VIEW REQUIREMENT #89). df is read
/// via LEFT JOIN + COALESCE so a missing-df token is never dropped (design rev-4 §2).
fn load_scoped_baseline_bags(conn: &Connection) -> anyhow::Result<Vec<SymbolBag>> {
    // Scoped baseline fingerprints: struct_hash + token_len per in-scope symbol.
    let mut fp_stmt = conn.prepare(
        "SELECT sf.symbol_id, sf.struct_hash, sf.token_len
         FROM symbol_fingerprints sf
         JOIN symbols ON symbols.id = sf.symbol_id
         JOIN files ON files.id = symbols.file_id
         WHERE sf.normalizer_kind = 'baseline'",
    )?;
    let mut bags: BTreeMap<i64, SymbolBag> = fp_stmt
        .query_map([], |row| {
            let symbol_id: i64 = row.get(0)?;
            Ok((symbol_id, SymbolBag {
                symbol_id,
                struct_hash: row.get(1)?,
                token_len: row.get(2)?,
                tokens: Vec::new(),
            }))
        })?
        .collect::<Result<_, _>>()?;

    // Full token bag per scoped baseline symbol, with each token's df LEFT-JOINed + COALESCEd to
    // the fallback sentinel (missing-df tokens must NOT be dropped — rev-4 §2).
    let mut tok_stmt = conn.prepare(
        "SELECT stp.symbol_id, stp.token_hash, stp.freq, COALESCE(df.df, ?1)
         FROM symbol_token_postings stp
         JOIN symbols ON symbols.id = stp.symbol_id
         JOIN files ON files.id = symbols.file_id
         LEFT JOIN clone_token_df df
           ON df.normalizer_kind = stp.normalizer_kind AND df.token_hash = stp.token_hash
         WHERE stp.normalizer_kind = 'baseline'",
    )?;
    let rows = tok_stmt.query_map([DF_FALLBACK], |row| {
        Ok((row.get::<_, i64>(0)?, TokenPosting {
            token_hash: row.get(1)?,
            freq: row.get(2)?,
            coalesced_df: row.get(3)?,
        }))
    })?;
    for row in rows {
        let (symbol_id, posting) = row?;
        if let Some(bag) = bags.get_mut(&symbol_id) {
            bag.tokens.push(posting);
        }
    }

    Ok(bags.into_values().collect())
}

/// Exact fast path: every group of symbols sharing a `struct_hash` is
/// identical-after-normalization, so it contributes all its pairwise pairs (no overlap math).
fn add_struct_hash_pairs(bags: &[SymbolBag], pairs: &mut std::collections::BTreeSet<(i64, i64)>) {
    let mut by_hash: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for bag in bags {
        by_hash.entry(bag.struct_hash.as_str()).or_default().push(bag.symbol_id);
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
fn sub_block_candidate_pairs(bags: &[SymbolBag]) -> std::collections::BTreeSet<(i64, i64)> {
    let mut inverted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for bag in bags {
        for token_hash in sub_block_tokens(bag) {
            inverted.entry(token_hash).or_default().push(bag.symbol_id);
        }
    }

    let mut candidate: std::collections::BTreeSet<(i64, i64)> = std::collections::BTreeSet::new();
    for ids in inverted.values() {
        for (i, &a) in ids.iter().enumerate() {
            for &b in &ids[i + 1..] {
                candidate.insert((a.min(b), a.max(b)));
            }
        }
    }
    candidate
}

/// A symbol's sub-block: the distinct token hashes whose occurrences reach into the first `p`
/// occurrences under the deterministic total order `(coalesced_df ASC, token_hash ASC)`.
///
/// `p = token_len - ceil(THETA * token_len) + 1` is the sub-block OCCURRENCE length (clamped to ≥
/// 0; if `p >= token_len` the whole bag is the sub-block). The sub-block is defined over EXPANDED
/// token occurrences (Σ freq), not distinct posting rows, so it matches the multiset `Σ min(freq)`
/// verifier (design rev-4 §3): walking distinct tokens in order accumulating `freq`, a token is
/// included if the running occurrence-count BEFORE it is `< p` (i.e. any of its occurrences falls
/// in the prefix).
fn sub_block_tokens(bag: &SymbolBag) -> Vec<i64> {
    let p = sub_block_len(bag.token_len);
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

/// Sub-block occurrence length `p = token_len - ceil(THETA * token_len) + 1`, clamped to ≥ 0.
fn sub_block_len(token_len: i64) -> i64 {
    let threshold = (THETA * token_len as f64).ceil() as i64;
    (token_len - threshold + 1).max(0)
}

/// Size prune + EXACT max-denominator verify (design rev-4 §3b). With `min_len`/`max_len` = the two
/// token_lens: cheap size prune `min_len >= ceil(THETA * max_len)`; then `overlap = Σ min(freq_a,
/// freq_b)` over the FULL bags, kept iff `overlap >= ceil(THETA * max_len)`. The GATE is
/// `similarity = overlap / max_len`; containment = `overlap / min_len` is NOT gated here.
fn verified_clone(a: &SymbolBag, b: &SymbolBag) -> bool {
    let min_len = a.token_len.min(b.token_len);
    let max_len = a.token_len.max(b.token_len);
    let threshold = (THETA * max_len as f64).ceil() as i64;

    // Size prune: a smaller block can't reach θ against a larger one.
    if min_len < threshold {
        return false;
    }

    overlap(a, b) >= threshold
}

/// Exact multiset overlap `Σ min(freq_a, freq_b)` over the two FULL token bags.
fn overlap(a: &SymbolBag, b: &SymbolBag) -> i64 {
    let freq_a: BTreeMap<i64, i64> = a.tokens.iter().map(|t| (t.token_hash, t.freq)).collect();
    let mut total = 0;
    for token in &b.tokens {
        if let Some(&fa) = freq_a.get(&token.token_hash) {
            total += fa.min(token.freq);
        }
    }
    total
}

/// Union-find the pairs into components of size >= 2 (sorted for determinism).
fn components_from_pairs(pairs: &[(i64, i64)]) -> Vec<Vec<i64>> {
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

#[cfg(test)]
mod tests {
    use super::components_from_pairs;

    #[test]
    fn union_find_groups_transitively_and_drops_singletons() {
        // 1-2, 2-3 => {1,2,3}; 5-6 => {5,6}; 9 alone => dropped.
        let comps = components_from_pairs(&[(1, 2), (2, 3), (5, 6)]);
        assert_eq!(comps, vec![vec![1, 2, 3], vec![5, 6]]);
    }
}
