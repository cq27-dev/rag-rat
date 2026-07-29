use super::*;

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
pub(crate) fn precomputed_pairs_if_eligible(
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
pub(crate) fn build_anchor_index(
    conn: &Connection,
) -> anyhow::Result<HashMap<(String, i64), (i64, String)>> {
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
