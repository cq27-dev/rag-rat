//! Component → [`CandidateCloneClass`] hydration, plus the staleness signal.
//!
//! [`build_class`] turns a union-found component (a slice of `symbols.id`) into a ranked
//! [`CandidateCloneClass`]: pairwise similarity / containment / medoid metrics (capped at
//! [`METRIC_SAMPLE_CAP`] for huge components), member hydration (chunked to respect SQLite's
//! host-param limit), the deterministic `class_key`, the REINDEX-STABLE `canonical_member_refs`,
//! and the Plan-2 ROI. [`count_stale_member_paths`] is the read-only "these results describe stale
//! file content" signal folded into [`CloneCompleteness::stale_members`].
//!
//! [`CloneCompleteness::stale_members`]: super::types::CloneCompleteness::stale_members

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use rag_rat_clones::NORM_VERSION;
use rusqlite::{Connection, OptionalExtension};

use super::scoring::{canonical_member_order_key, class_key_for};
use super::substrate::{METRIC_SAMPLE_CAP, SymbolBag, overlap};
use super::types::{CandidateCloneClass, CloneMember, RoiFactors};
use super::{HYDRATION_CHUNK, MAX_MEMBERS, MEMBER_VALUE_CAP};

/// Build a [`CandidateCloneClass`] from a component (a slice of symbol ids). Returns `None` if
/// any id is missing from `by_id` (shouldn't happen for a well-formed component derived from the
/// same bag set), or if member hydration yields nothing (TOCTOU: fingerprint rows vanished
/// mid-read). Members are capped at [`MAX_MEMBERS`].
///
/// `pin` is the subject `symbols.id` that MUST appear in the returned (capped) member list when it
/// is a member of the component — `clones_for_symbol` passes the resolved subject so the caller
/// always sees the symbol it asked about even when its id falls outside the first [`MAX_MEMBERS`]
/// by id. `find_clones` passes `None` (no subject to pin). `class_key` / `member_count` /
/// `total_members` are always over the FULL component regardless of `pin`.
pub(crate) fn build_class(
    component: &[i64],
    by_id: &BTreeMap<i64, &SymbolBag>,
    conn: &Connection,
    pin: Option<i64>,
) -> anyhow::Result<Option<CandidateCloneClass>> {
    let cancelled = AtomicBool::new(false);
    build_class_cancellable(component, by_id, conn, pin, &cancelled)
}

pub(crate) fn build_class_cancellable(
    component: &[i64],
    by_id: &BTreeMap<i64, &SymbolBag>,
    conn: &Connection,
    pin: Option<i64>,
    cancelled: &AtomicBool,
) -> anyhow::Result<Option<CandidateCloneClass>> {
    ensure_not_cancelled(cancelled)?;
    let bags: Vec<&SymbolBag> = component.iter().filter_map(|id| by_id.get(id).copied()).collect();
    if bags.len() != component.len() {
        return Ok(None);
    }

    let n = bags.len();

    // For huge components, cap the pairwise metric work to avoid O(n²) blowup. When the
    // component exceeds METRIC_SAMPLE_CAP members the metrics run over the FIRST
    // METRIC_SAMPLE_CAP members only (the component is deterministically sorted, so this is
    // stable). `metrics_sampled` is set true so callers know. For all normal-size components
    // (the typical case, including ALL existing tests) `metric_n == n` and behavior is identical.
    let metrics_sampled = n > METRIC_SAMPLE_CAP;
    let metric_n = n.min(METRIC_SAMPLE_CAP);
    let metric_bags = &bags[..metric_n];

    // Pairwise similarity (overlap/max_len) and containment (overlap/min_len), upper-triangle
    // over metric_bags (== full component when metric_n == n).
    let mut similarity_min = f64::MAX;
    let mut containment_max = 0.0_f64;
    let mut sim_sums = vec![0.0_f64; metric_n]; // for medoid selection

    for i in 0..metric_n {
        ensure_not_cancelled(cancelled)?;
        for j in (i + 1)..metric_n {
            let ov = overlap(metric_bags[i], metric_bags[j]);
            let max_len = metric_bags[i].token_len.max(metric_bags[j].token_len);
            let min_len = metric_bags[i].token_len.min(metric_bags[j].token_len);
            let sim = if max_len == 0 { 1.0 } else { ov as f64 / max_len as f64 };
            let cont = if min_len == 0 { 1.0 } else { ov as f64 / min_len as f64 };
            if sim < similarity_min {
                similarity_min = sim;
            }
            if cont > containment_max {
                containment_max = cont;
            }
            sim_sums[i] += sim;
            sim_sums[j] += sim;
        }
    }
    if similarity_min == f64::MAX {
        // Singleton component — shouldn't reach here after min_copies filter, but be safe.
        similarity_min = 1.0;
    }
    let cohesion_min_pairwise = similarity_min;

    // Medoid: member with maximum sum of similarities to all others (within metric_bags).
    let medoid_idx = sim_sums
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let medoid_bag = metric_bags[medoid_idx];
    let body_token_len_medoid = medoid_bag.token_len;
    // Thread the medoid's symbol_id out onto the class for Plan 4b Task 5d (anti-unify spine
    // anchor). This is the bag-overlap medoid (max Σ overlap/max_len over metric_bags), NOT an
    // LCS-distance medoid — sound as a template anchor for a coherence-split class (all pairs ≥ θ)
    // where the bag-overlap medoid is representatively central. When metrics_sampled is true,
    // medoid_idx is over the first METRIC_SAMPLE_CAP members (id-ASC stable); the resolved id is
    // still a real member.
    let medoid_symbol_id = Some(medoid_bag.symbol_id);

    // Min similarity of any member to the medoid (within metric_bags).
    let mut similarity_medoid_min = f64::MAX;
    for (i, bag) in metric_bags.iter().enumerate() {
        if i.is_multiple_of(32) {
            ensure_not_cancelled(cancelled)?;
        }
        if i == medoid_idx {
            continue;
        }
        let ov = overlap(medoid_bag, bag);
        let max_len = medoid_bag.token_len.max(bag.token_len);
        let sim = if max_len == 0 { 1.0 } else { ov as f64 / max_len as f64 };
        if sim < similarity_medoid_min {
            similarity_medoid_min = sim;
        }
    }
    if similarity_medoid_min == f64::MAX {
        similarity_medoid_min = 1.0;
    }

    let total_members = component.len();
    let cap = total_members.min(MAX_MEMBERS);

    // Hydrate CloneMembers via a scoped DB query with an IN clause, in batches of HYDRATION_CHUNK.
    // Fix 1 (#215): a single statement uses one host param per member id, so a component larger
    // than SQLITE_MAX_VARIABLE_NUMBER (999 on older non-bundled libs) would fail `conn.prepare`
    // and error the whole call. Chunking keeps every statement well under the limit; we
    // accumulate across chunks and re-sort by `symbol_id` so the deterministic id order is
    // restored regardless of chunk boundaries.
    // Fix 3 (#215): each chunk also filters normalizer_version so stale fingerprint rows don't
    // yield duplicate members or wrong token_len values. The version bind is appended as the
    // last positional param after that chunk's id list.
    // The tuple carries `start_byte` (Fix 2, #215 Plan 4b Codex round-4) so `canonical_member_refs`
    // can sort on the REINDEX-STABLE (struct_hash, path, start_byte) key — the SAME key
    // `load_refine_members` uses for the `per_member_values` ordinal basis. `start_byte` (not the
    // public `CloneMember.start_line`) is what `load_refine_members` orders by, and two symbols can
    // share a line but never a start byte, so it is the exact, total tiebreak that keeps the two
    // member orderings byte-for-byte identical.
    let mut raw_members: Vec<(i64, i64, CloneMember)> = Vec::with_capacity(total_members);
    for chunk in component.chunks(HYDRATION_CHUNK) {
        ensure_not_cancelled(cancelled)?;
        let id_placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let version_placeholder = format!("?{}", chunk.len() + 1);
        let sql = format!(
            "SELECT symbols.id, ns.value, files.path, symbols.start_line, symbols.end_line, \
             sf.token_len, symbols.language, symbols.start_byte
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             JOIN symbol_fingerprints sf
               ON sf.symbol_id = symbols.id
               AND sf.normalizer_kind = 'baseline'
               AND sf.normalizer_version = {version_placeholder}
             WHERE symbols.id IN ({})
             ORDER BY symbols.id",
            id_placeholders.join(", ")
        );
        let params: Vec<i64> = chunk.iter().copied().chain(std::iter::once(NORM_VERSION)).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let symbol_id: i64 = row.get(0)?;
            let start_byte: i64 = row.get(7)?;
            Ok((symbol_id, start_byte, CloneMember {
                r#ref: row.get(1)?,
                path: row.get(2)?,
                start_line: row.get(3)?,
                end_line: row.get(4)?,
                token_len: row.get(5)?,
                language: row.get(6)?,
            }))
        })?;
        for (index, row) in rows.enumerate() {
            if index.is_multiple_of(256) {
                ensure_not_cancelled(cancelled)?;
            }
            raw_members.push(row?);
        }
    }
    ensure_not_cancelled(cancelled)?;
    // Restore the deterministic `symbols.id ASC` order the single-statement path produced.
    raw_members.sort_unstable_by_key(|(symbol_id, _, _)| *symbol_id);

    // Fix 5 (#215): if hydration returned nothing (all fingerprint rows vanished mid-read), bail to
    // `None` rather than build an internally-inconsistent class (member_count from the component
    // but zero members, an empty language fallback, etc.).
    if raw_members.is_empty() {
        return Ok(None);
    }

    let language = raw_members
        .first()
        .map(|(_, _, m)| m.language.clone())
        .unwrap_or_else(|| bags[0].language.clone());

    // cross_module_spread counts ALL hydrated members (full component), not just the capped subset
    // — so it is consistent with member_count (both over the full population).
    let parent_dirs: std::collections::BTreeSet<String> = raw_members
        .iter()
        .map(|(_, _, m)| {
            std::path::Path::new(&m.path)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        })
        .collect();
    let cross_module_spread = parent_dirs.len();

    // Fix 3 (#215): the class key is built from per-member identity that includes the source
    // LOCATION (`ref@path:start-end`), not the qualified-name `ref` alone. Two distinct
    // components can share the same qualified-name multiset — overloads, cfg variants,
    // same-named methods on different impls — and would otherwise collide on
    // `clone_refinements.class_key` (a TEXT PRIMARY KEY in Plan 4), conflating two classes into
    // one. We deliberately do NOT use `symbols.id`: the rowid is reassigned on every reindex,
    // so a location-derived key stays stable across reindexes while still distinguishing
    // same-named members at different spans. Computed over ALL members (full component), not
    // just the capped slice — two classes sharing the first MAX_MEMBERS members but
    // differing later must get different keys.
    let key_material: Vec<String> = raw_members
        .iter()
        .map(|(_, _, m)| format!("{}@{}:{}-{}", m.r#ref, m.path, m.start_line, m.end_line))
        .collect();
    let class_key = class_key_for(&key_material);
    ensure_not_cancelled(cancelled)?;

    // Canonical-ordered member refs (#215 Plan 4b): the qualified `ref` of each member in the SAME
    // canonical `(struct_hash, path, start_byte)` order, capped at the same `MEMBER_VALUE_CAP`,
    // that `load_refine_members` uses — so this is ORDINAL-ALIGNED to a refined class's
    // `variation_points[*].per_member_values`. The `members` field above is `r#ref`-sorted (display
    // order) and cannot be mapped to a `per_member_values` slot; `clones --explain` zips THIS
    // vector with the values so each printed value carries its member identity. Computed here
    // (not in `apply_refinement`) because the warm cache path skips `load_refine_members`
    // entirely, while `raw_members` (carrying `symbol_id`, `start_byte`, and `r#ref`) and `by_id`
    // (the `struct_hash`) are always available on both paths.
    //
    // Fix 2 (#215 Plan 4b Codex round-4): the sort key is `(struct_hash, path, start_byte)`, NOT
    // `(struct_hash, symbol_id)`. `symbol_id` is a rowid reassigned on every reindex, so a warm
    // refinement (same content key → cached per_member_values frozen at the OLD member order) could
    // be served against a `canonical_member_refs` recomputed in a DIFFERENT order — labelling one
    // member's value with another's identity. `(path, start_byte)` uniquely identifies a member and
    // is stable across a file-unchanged reindex, so the recomputed order always matches the cached
    // value order. This MUST stay byte-for-byte identical to `load_refine_members`' sort key.
    //
    // Codex #4 (round-3): the identity carried per member is LOCATION-BEARING
    // (`ref@path:start-end`), the SAME identity `class_key` uses — NOT the bare qualified
    // `ref`. A class with DUPLICATE qualified refs (same-named methods/overloads in one file,
    // cfg variants) would otherwise label its `per_member_values` with indistinguishable names,
    // so a consumer (`clones --explain`, MCP output) could not map a value back to a UNIQUE
    // member. Only the string identity each entry carries changed in round-3; round-4 changes
    // only the SORT KEY.
    let canonical_member_refs: Vec<String> = {
        let mut ordered: Vec<(&str, &str, i64, String)> = raw_members
            .iter()
            .filter_map(|(id, start_byte, m)| {
                by_id.get(id).map(|b| {
                    (
                        b.struct_hash.as_str(),
                        m.path.as_str(),
                        *start_byte,
                        format!("{}@{}:{}-{}", m.r#ref, m.path, m.start_line, m.end_line),
                    )
                })
            })
            .collect();
        ordered.sort_unstable_by(|a, b| {
            canonical_member_order_key(a.0, a.1, a.2)
                .cmp(&canonical_member_order_key(b.0, b.1, b.2))
        });
        ensure_not_cancelled(cancelled)?;
        ordered.into_iter().take(MEMBER_VALUE_CAP).map(|(_, _, _, r)| r).collect()
    };

    // Cap the returned member list AFTER computing spread and key from the full set.
    // Fix 2 (#215): when a `pin` subject is supplied (clones_for_symbol) and that member exists but
    // would fall OUTSIDE the first `cap` members by id, guarantee its inclusion: keep the first
    // `cap - 1` by id plus the pinned member, so the caller always sees the symbol it asked about.
    // When `pin` is `None`, or the subject is already within the first `cap`, this is a no-op and
    // the selection is identical to the plain `take(cap)` path.
    let member_count = total_members;
    let members_returned = raw_members.len().min(cap);

    let pinned_idx = pin.and_then(|subject_id| {
        let pos = raw_members.iter().position(|(id, _, _)| *id == subject_id)?;
        // Only act when the pin would otherwise be dropped: it sits at or past `cap` in id order.
        (pos >= cap).then_some(pos)
    });

    let chosen: Vec<CloneMember> = match pinned_idx {
        Some(pos) => {
            // First `cap - 1` by id, plus the pinned member → exactly `cap` members.
            let mut chosen: Vec<CloneMember> =
                raw_members.iter().take(cap - 1).map(|(_, _, m)| m.clone()).collect();
            chosen.push(raw_members[pos].2.clone());
            chosen
        },
        None => raw_members.into_iter().take(cap).map(|(_, _, m)| m).collect(),
    };
    let mut members = chosen;
    members.sort_unstable_by(|a, b| a.r#ref.cmp(&b.r#ref));

    // Load-bearing factor: 1 + ln(1 + max_fan_in_score) over members. Fan-in proxy via
    // `scoped_weighted_fan_in` (heuristic-only, no oracle data at this call site).
    let oracle = rag_rat_query::load_bearing::OracleContext::none();
    let mut max_importance = 0.0_f64;
    for (index, &id) in component.iter().enumerate() {
        if index.is_multiple_of(32) {
            ensure_not_cancelled(cancelled)?;
        }
        if let Some(score) = rag_rat_query::load_bearing::scoped_weighted_fan_in(conn, id, &oracle)
            .ok()
            .flatten()
            .map(|entry| entry.score)
        {
            max_importance = max_importance.max(score);
        }
    }
    let load_bearing_factor = 1.0 + max_importance.ln_1p();

    // Median token_len across all bags in the component.
    let mut token_lens: Vec<i64> = bags.iter().map(|b| b.token_len).collect();
    ensure_not_cancelled(cancelled)?;
    token_lens.sort_unstable();
    ensure_not_cancelled(cancelled)?;
    let median_token_len = token_lens[token_lens.len() / 2];

    let roi = cross_module_spread as f64
        * member_count as f64
        * body_token_len_medoid as f64
        * load_bearing_factor
        * cohesion_min_pairwise;

    let roi_factors = RoiFactors {
        member_count,
        cross_module_spread,
        median_token_len,
        load_bearing_factor,
        cohesion_penalty: cohesion_min_pairwise,
    };

    Ok(Some(CandidateCloneClass {
        class_key,
        class_kind: "candidate_component",
        language,
        refined: false,
        members,
        member_count,
        members_returned,
        total_members,
        similarity_min,
        similarity_medoid_min,
        containment_max,
        cohesion_min_pairwise,
        cross_module_spread,
        body_token_len_medoid,
        roi,
        roi_factors,
        metrics_sampled,
        medoid_symbol_id,
        // Refinement fields are None on an un-refined candidate class; the two-phase driver in
        // `find_clones` / `clones_for_symbol` populates them (and flips `refined`/`class_kind`).
        lcs_ratio: None,
        confidence: None,
        refactorability: None,
        refine_mode: None,
        template: None,
        variation_points: None,
        proposed_signature: None,
        anti_unify_coverage: None,
        canonical_member_refs: Some(canonical_member_refs),
    }))
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> anyhow::Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "lens request cancelled");
    Ok(())
}

/// Count DISTINCT member file paths in `classes` whose on-disk content no longer matches the
/// indexed `files.sha256`. Fetches the indexed sha256 per distinct path from the `files` view
/// (one query per distinct path) then calls [`IndexDatabase::source_path_is_stale`] — the same
/// pattern as `graph_index.rs`. This is a read-only signal; callers should reindex if non-zero.
///
/// [`IndexDatabase::source_path_is_stale`]: crate::index::IndexDatabase
pub(crate) fn count_stale_member_paths(
    db: &crate::index::IndexDatabase,
    conn: &Connection,
    classes: &[CandidateCloneClass],
) -> anyhow::Result<usize> {
    // `source_path_is_stale` reads bytes from `source_root` (the MAIN checkout). Under a
    // linked-worktree overlay scope the scoped clone results come from the BRANCH bytes
    // (`index_worktree_overlay`), so a main-checkout comparison is meaningless: branch-only files
    // look "missing" (false stale) and same-path branch edits diff against base content (false
    // stale). Mirror `heal_file` / the search-heal path / `parser_failures`, which all early-return
    // under an overlay scope, and report 0 (no main-checkout staleness signal is available here).
    if db.active_scope_is_linked_overlay() {
        return Ok(0);
    }
    // Collect distinct paths across all returned members.
    let mut distinct_paths: BTreeSet<String> = BTreeSet::new();
    for class in classes {
        for member in &class.members {
            distinct_paths.insert(member.path.clone());
        }
    }
    let mut stale = 0usize;
    for path in &distinct_paths {
        let sha256: Option<String> = conn
            .query_row("SELECT sha256 FROM files WHERE path = ?1 LIMIT 1", [path.as_str()], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(sha256) = sha256 else {
            // Path not in index at all — treat as stale.
            stale += 1;
            continue;
        };
        if db.source_path_is_stale(path, &sha256) {
            stale += 1;
        }
    }
    Ok(stale)
}
