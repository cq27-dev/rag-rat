//! Windowed file-pair change coupling, derived from `git_file_changes` (V056, #566).
//!
//! Persists a *symmetric* per-pair co-change table over a bounded recency window of eligible
//! commits, surfaced as `impact_surface`'s "changes-alongside" section.
//!
//! INVARIANT — the stored table is a PURE FUNCTION of `(git-history window, params version)`. It
//! has NO dependence on the `files` view (generation, generated flag, worktree scope): the
//! co-change counts, endpoint counts, and the write-time storage filter all read only `git_commits`
//! / `git_file_changes`. This makes the freshness stamp (`history_freshness_key:params`) COMPLETE
//! by construction — a reindex, a generated-flag flip, a new files generation, or a worktree-scope
//! change can never make the stored rows stale, because none of them shape the table. The generated
//! / absent-at-HEAD filter is a READ-time concern (a partner that isn't surfaceable stays stored).
//!
//! The write-time storage bound is the LIFT FLOOR, not a per-file cap. A pair is stored iff
//! `co_change_count >= MIN_COUPLING_SUPPORT` AND `lift = co * N / (a_count * b_count) >=
//! MIN_COUPLING_LIFT` — all four inputs are pure history. This bounds the table without a per-file
//! cap: a HUB file (touched in most window commits) has `lift <= ~1` with everything, so its pairs
//! are dropped at write; genuinely coupled high-lift pairs survive regardless of how many partners
//! either endpoint has. No ranking over unsurfaced partners, so there is no cap to mis-rank.
//!
//! Substrate semantics this pass must respect (see the `git_file_changes` co-change memory bound to
//! `git_history.rs`, and `plan-change-coupling-signal.md`):
//!  - Merge commits record no per-file rows (`changed_file_count = 0`), so the `>= 2` eligibility
//!    gate drops them automatically.
//!  - `changed_file_count` is populated at insert time — a free per-commit set-size filter, used
//!    here for single-file and mega-commit exclusion without scanning `git_file_changes`.
//!  - Renames land once at the destination path, so a renamed file starts a fresh coupling
//!    identity; co-change history does not bridge the rename.
//!  - `git_commits` / `git_file_changes` are direct `repo_id`-scoped (V040): every read joins AND
//!    filters on `repo_id`, because forks share commit hashes.
//!  - History rows are append-mostly but occasionally *wholesale-replaced* (rebase / amend /
//!    force-push / shallow / root-drift / torn state). So this is a `DerivedIndex` table: it is
//!    fully recomputed on a freshness-stamp mismatch, never patched incrementally.
//!
//! Two co-change notions coexist until clusters converge: this *windowed / persisted* table
//! (impact) versus `query/clusters.rs::co_touch_pairs`' *unwindowed / transient* one
//! (`repo_clusters`). The shared [`MAX_COUPLING_COMMIT_FILES`] cap keeps the two consumers aligned.

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::index::{repo_meta, set_repo_meta};

/// The last N *eligible* commits per repo define the window, ordered `(committed_at_s DESC, hash
/// DESC)` for determinism (commit timestamps collide, so the hash breaks the tie). A commit-count
/// window bounds compute and table size independent of repo age — a time window would decay to
/// empty on a dormant repo and grow unbounded on a monorepo.
pub(crate) const COUPLING_WINDOW_COMMITS: i64 = 1000;

/// A commit touching more than this many files contributes *no* pairs: a 500-file `cargo fmt` sweep
/// or license-header pass would otherwise emit `C(500, 2)` spurious pairs and couple everything to
/// everything. Shared with `query/clusters.rs::co_touch_pairs` (formerly its own
/// `MAX_COTOUCH_COMMIT_FILES`) so the two consumers' caps can never drift.
pub(crate) const MAX_COUPLING_COMMIT_FILES: usize = 40;

/// Write-time support floor: a single co-occurrence is noise and would dominate the row count.
pub(crate) const MIN_COUPLING_SUPPORT: i64 = 2;

/// WRITE-TIME storage floor (the table's size bound; also a redundant read-time defense). A pair is
/// stored iff `lift = co * N / (a_count * b_count) >= MIN_COUPLING_LIFT`. This is a base-rate
/// correction: a HUB file present in most commits scores `lift ~= 1` with everything, so its pairs
/// are dropped here — bounding the table WITHOUT a per-file cap and without ranking over unsurfaced
/// partners. All four inputs (`co`, `N`, `a_count`, `b_count`) are pure git history, so the filter
/// keeps the stored set a pure function of history.
pub(crate) const MIN_COUPLING_LIFT: f64 = 1.5;

/// Bump when ANY storage-affecting knob changes — the window size ([`COUPLING_WINDOW_COMMITS`]),
/// the mega-commit gate ([`MAX_COUPLING_COMMIT_FILES`]), the support floor
/// ([`MIN_COUPLING_SUPPORT`]), or the lift floor ([`MIN_COUPLING_LIFT`]) — because each now changes
/// the STORED set. It is folded into the freshness stamp, so a change invalidates every repo's
/// coupling rows *without* a schema migration (the `GENERATED_FLAGS_VERSION` pattern).
pub(crate) const COUPLING_PARAMS_VERSION: u32 = 1;

/// `repo_meta` key holding the freshness stamp
/// `"{history_freshness_key}:{COUPLING_PARAMS_VERSION}"` — the freshness authority for the derived
/// table (row-level integrity is deliberately not, so the table survives a history full-replace
/// between recompute passes). The stored table is a pure function of `(git-history window,
/// params)`, so these two axes are COMPLETE: the git-history cursor snapshot (head + root +
/// shallow + complete — so a rewrite at the same HEAD invalidates) and the params version (so a
/// window / gate / floor const change invalidates without a migration). Deliberately NO
/// files-generation / generated-flag / worktree axis — the table does not depend on the `files`
/// view.
const COUPLING_STAMP_META: &str = "git_coupling_stamp";

/// One coupled file for a queried path, with the read-time metrics already derived from the stored
/// counts. `confidence = P(other | this)`; `lift` is the symmetric base-rate correction.
#[derive(Debug, Clone)]
pub(crate) struct CoupledFile {
    pub other_path: String,
    pub co_change_count: i64,
    pub this_change_count: i64,
    pub window_commit_count: i64,
    pub confidence: f64,
    pub lift: f64,
    pub last_co_change_at_s: i64,
    pub language: String,
    pub kind: String,
}

/// Per-pair accumulator over the window. `last_at_s` is the newest co-change's `committed_at_s`
/// (recency evidence); it starts at `i64::MIN` so the first observation always wins regardless of
/// the timestamp's sign.
struct PairAcc {
    count: i64,
    last_at_s: i64,
}

/// Recompute-if-stale gate: the `DerivedIndex` self-heal, mirroring `ensure_fts_fresh` /
/// `cached_blame`. Cheap stamp SELECT, then a full recompute on mismatch. Idempotent — concurrent
/// writers serialize on SQLite's write lock and last-writer-wins an identical result; readers see
/// old-or-new rows, never a mix (the recompute is one transaction).
pub(crate) fn ensure_coupling_fresh(conn: &Connection, now_ms: i64) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    if coupling_stamp_current(conn, &repo_id)? {
        return Ok(());
    }
    recompute_couplings(conn, &repo_id, now_ms)
}

/// The stamp a fresh table for `repo_id` must carry, over the ONLY two axes the stored table
/// depends on: the git-history freshness key (the `is_history_current` cursor snapshot — reused
/// verbatim so a history rewrite at the same HEAD is caught) and the params version. There is
/// deliberately no files-generation component: the stored table is a pure function of git history,
/// so a files-view change can never stale it.
fn coupling_stamp_value(conn: &Connection, repo_id: &str) -> anyhow::Result<String> {
    let history_key =
        crate::index::git_history::history_freshness_key(conn, repo_id)?.unwrap_or_default();
    Ok(format!("{history_key}:{COUPLING_PARAMS_VERSION}"))
}

fn coupling_stamp_current(conn: &Connection, repo_id: &str) -> anyhow::Result<bool> {
    let stored = repo_meta(conn, repo_id, COUPLING_STAMP_META)?;
    Ok(stored.as_deref() == Some(coupling_stamp_value(conn, repo_id)?.as_str()))
}

/// Full recompute for one repo: read the window, accumulate pair + endpoint counts in memory, prune
/// (support + lift floors — both pure git history), then `DELETE` + batched `INSERT` + stamp in one
/// transaction. Never patches incrementally — the window slides under append and a history
/// full-replace invalidates everything anyway, so a bounded full recompute is the simple correct
/// choice (see the module header).
pub(crate) fn recompute_couplings(
    conn: &Connection,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    // Stamp value is captured against the CURRENT head before we write, so a head move mid-flight
    // is caught by the next read (this recompute stamps the head it actually read).
    let stamp = coupling_stamp_value(conn, repo_id)?;

    // The window size actually used (shallow repos have fewer commits) — the base-rate population N
    // for lift, counted from the eligible-commit CTE directly (by `changed_file_count`). Pure git
    // history, matching the accumulation below.
    let window_commit_count = eligible_window_commit_count(conn, repo_id)?;

    let (pairs, path_window_count, paths) = accumulate_window_pairs(conn, repo_id)?;

    let surviving = prune_pairs(pairs, &path_window_count, window_commit_count);

    let tx = conn.unchecked_transaction()?;
    conn.execute("DELETE FROM git_change_couplings WHERE repo_id = ?1", params![repo_id])?;
    {
        let mut insert = conn.prepare(
            "INSERT INTO git_change_couplings(
                 repo_id, path_a, path_b, co_change_count, path_a_change_count,
                 path_b_change_count, window_commit_count, last_co_change_at_s, computed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for pair in &surviving {
            // Canonicalize by PATH STRING (the `path_a < path_b` BINARY invariant), not by interned
            // id — the interner assigns ids in first-seen order, which is unrelated to path order.
            let path_lo = &paths[pair.lo as usize];
            let path_hi = &paths[pair.hi as usize];
            let (path_a, id_a, path_b, id_b) = if path_lo < path_hi {
                (path_lo, pair.lo, path_hi, pair.hi)
            } else {
                (path_hi, pair.hi, path_lo, pair.lo)
            };
            let a_count = path_window_count.get(&id_a).copied().unwrap_or(0);
            let b_count = path_window_count.get(&id_b).copied().unwrap_or(0);
            insert.execute(params![
                repo_id,
                path_a,
                path_b,
                pair.count,
                a_count,
                b_count,
                window_commit_count,
                pair.last_at_s,
                now_ms,
            ])?;
        }
    }
    set_repo_meta(conn, repo_id, COUPLING_STAMP_META, &stamp)?;
    tx.commit()?;
    Ok(())
}

/// Count the eligible commits in the window (`changed_file_count BETWEEN 2 AND cap`, capped at
/// [`COUPLING_WINDOW_COMMITS`]) — the base-rate population N, a pure function of git history (no
/// `files` dependency).
fn eligible_window_commit_count(conn: &Connection, repo_id: &str) -> anyhow::Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM (
             SELECT hash FROM git_commits
             WHERE repo_id = ?1 AND changed_file_count BETWEEN 2 AND ?2
             ORDER BY committed_at_s DESC, hash DESC
             LIMIT ?3
         )",
        params![repo_id, MAX_COUPLING_COMMIT_FILES as i64, COUPLING_WINDOW_COMMITS],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// A canonicalized (by interned id) unordered pair with its window counts.
struct SurvivingPair {
    lo: u32,
    hi: u32,
    count: i64,
    last_at_s: i64,
}

/// Stream the window rows grouped per commit (mirroring `co_touch_pairs`' streaming loop) and
/// accumulate: `HashMap<(lo, hi), PairAcc>` for co-change counts and `HashMap<id, count>` for each
/// endpoint's window touch count. Returns the two maps plus the id→path interner table.
#[allow(clippy::type_complexity)]
fn accumulate_window_pairs(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<(HashMap<(u32, u32), PairAcc>, HashMap<u32, i64>, Vec<String>)> {
    // PURE GIT HISTORY — deliberately does NOT join `files`. Co-change counts and endpoint counts
    // read only `git_commits` / `git_file_changes`, so the stored table is a pure function of the
    // history window (+ params), and the `history:params` stamp fully covers it. Generated /
    // absent-at-HEAD paths DO enter the accumulator here (a generated partner is stored but not
    // surfaced — the generated/existence filter is a READ-time concern in
    // `coupled_files_for_path`). The storage bound is the lift floor at write, not a files-view
    // filter. Repo isolation is the `changes.repo_id = ?1` predicate (V040).
    let mut stmt = conn.prepare(
        "
        WITH window_commits AS (
            SELECT hash, committed_at_s
            FROM git_commits
            WHERE repo_id = ?1
              AND changed_file_count BETWEEN 2 AND ?2
            ORDER BY committed_at_s DESC, hash DESC
            LIMIT ?3
        )
        SELECT wc.hash, wc.committed_at_s, changes.path
        FROM window_commits wc
        JOIN git_file_changes changes
          ON changes.commit_hash = wc.hash AND changes.repo_id = ?1
        ORDER BY wc.committed_at_s DESC, wc.hash DESC
        ",
    )?;
    let mut rows =
        stmt.query(params![repo_id, MAX_COUPLING_COMMIT_FILES as i64, COUPLING_WINDOW_COMMITS])?;

    let mut path_ids: HashMap<String, u32> = HashMap::new();
    let mut paths: Vec<String> = Vec::new();
    let mut pairs: HashMap<(u32, u32), PairAcc> = HashMap::new();
    let mut path_window_count: HashMap<u32, i64> = HashMap::new();

    let mut current_hash = String::new();
    let mut current_at_s: i64 = 0;
    let mut commit_ids: Vec<u32> = Vec::new();

    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let at_s: i64 = row.get(1)?;
        let path: String = row.get(2)?;
        if current_hash.is_empty() {
            current_hash = hash.clone();
            current_at_s = at_s;
        }
        if hash != current_hash {
            flush_commit(&mut pairs, &mut path_window_count, &mut commit_ids, current_at_s);
            current_hash = hash;
            current_at_s = at_s;
        }
        let id = intern_path(&mut path_ids, &mut paths, path);
        commit_ids.push(id);
    }
    if !current_hash.is_empty() {
        flush_commit(&mut pairs, &mut path_window_count, &mut commit_ids, current_at_s);
    }

    Ok((pairs, path_window_count, paths))
}

fn intern_path(path_ids: &mut HashMap<String, u32>, paths: &mut Vec<String>, path: String) -> u32 {
    if let Some(&id) = path_ids.get(&path) {
        return id;
    }
    let id = u32::try_from(paths.len()).expect("window path count exceeds u32");
    path_ids.insert(path.clone(), id);
    paths.push(path);
    id
}

/// Fold one commit's file set into the accumulators, then clear the scratch buffer. All of the
/// commit's paths reach here (pure git history — no files filter). Every path bumps its window
/// touch count (the confidence denominator). Pairs are only generated for
/// `2..=MAX_COUPLING_COMMIT_FILES` distinct paths; the upper bound mirrors the `changed_file_count`
/// window gate and the `< 2` bound is a defensive guard (a windowed commit already has `>= 2`
/// change rows).
fn flush_commit(
    pairs: &mut HashMap<(u32, u32), PairAcc>,
    path_window_count: &mut HashMap<u32, i64>,
    commit_ids: &mut Vec<u32>,
    at_s: i64,
) {
    commit_ids.sort_unstable();
    commit_ids.dedup();
    for &id in commit_ids.iter() {
        *path_window_count.entry(id).or_insert(0) += 1;
    }
    let n = commit_ids.len();
    if (2..=MAX_COUPLING_COMMIT_FILES).contains(&n) {
        for left in 0..n {
            for right in (left + 1)..n {
                // commit_ids is sorted asc + deduped, so (lo, hi) is canonical by interned id.
                let acc = pairs
                    .entry((commit_ids[left], commit_ids[right]))
                    .or_insert(PairAcc { count: 0, last_at_s: i64::MIN });
                acc.count += 1;
                acc.last_at_s = acc.last_at_s.max(at_s);
            }
        }
    }
    commit_ids.clear();
}

/// Prune to the persisted set with the two PURE-GIT-HISTORY storage floors: `co >=
/// MIN_COUPLING_SUPPORT` AND `lift = co * N / (a_count * b_count) >= MIN_COUPLING_LIFT`. The lift
/// floor is what bounds the table without a per-file cap: a hub file (`a_count ~= N`) satisfies
/// `lift = co / b_count <= 1` for every partner, so its pairs are dropped here — no per-file
/// ranking (so nothing can mis-rank an unsurfaced partner), no files-view dependence.
fn prune_pairs(
    pairs: HashMap<(u32, u32), PairAcc>,
    path_window_count: &HashMap<u32, i64>,
    window_commit_count: i64,
) -> Vec<SurvivingPair> {
    pairs
        .into_iter()
        .filter(|((lo, hi), acc)| {
            if acc.count < MIN_COUPLING_SUPPORT {
                return false;
            }
            let a_count = path_window_count.get(lo).copied().unwrap_or(0);
            let b_count = path_window_count.get(hi).copied().unwrap_or(0);
            let denom = a_count as f64 * b_count as f64;
            let lift = if denom > 0.0 {
                acc.count as f64 * window_commit_count as f64 / denom
            } else {
                0.0
            };
            lift >= MIN_COUPLING_LIFT
        })
        .map(|((lo, hi), acc)| SurvivingPair { lo, hi, count: acc.count, last_at_s: acc.last_at_s })
        .collect()
}

/// Given a file `path` in `repo_id`, the top coupled files ranked by asymmetric confidence
/// `P(other | path)`, then truncated to `limit`. This is the ONLY files-view touch: the partner
/// join applies the generated / existence filter (a generated or absent-at-HEAD partner is stored
/// but not surfaced) and dedups the bare-view multi-row-per-path (finding 4). The stored rows
/// already pass the lift floor (enforced at write), so the read-time lift check is a redundant
/// defense.
pub(crate) fn coupled_files_for_path(
    conn: &Connection,
    repo_id: &str,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<CoupledFile>> {
    // `git_change_couplings` is direct `repo_id`-scoped (V040): filter on repo_id so a fork sharing
    // commit hashes never surfaces a sibling repo's couplings. The partner subquery is the sole
    // files-view dependence and does two READ-time jobs: (1) `WHERE generated = 0` drops a partner
    // that is generated or absent-at-HEAD (stored but not surfaceable); (2) `GROUP BY path`
    // collapses the BARE repo-generation `files` view's MULTIPLE rows per path (distinct commit_sha
    // / worktree_id at one live generation — the MCP `call_tool` read path, `write_repo_generation_
    // view`, no dedup) to one row, so a plain join can't emit a duplicate partner that eats `limit`
    // (finding 4). `c` already has exactly one row per (repo, path_a, path_b).
    let mut stmt = conn.prepare(
        "
        SELECT
            CASE WHEN c.path_a = ?2 THEN c.path_b ELSE c.path_a END AS other_path,
            c.co_change_count,
            CASE WHEN c.path_a = ?2 THEN c.path_a_change_count
                 ELSE c.path_b_change_count END                     AS this_count,
            c.path_a_change_count,
            c.path_b_change_count,
            c.window_commit_count,
            c.last_co_change_at_s,
            files.language,
            files.kind
        FROM git_change_couplings c
        JOIN (SELECT path, MIN(language) AS language, MIN(kind) AS kind
              FROM files WHERE generated = 0 GROUP BY path)
             files ON files.path = CASE WHEN c.path_a = ?2 THEN c.path_b ELSE c.path_a END
        WHERE c.repo_id = ?1
          AND (c.path_a = ?2 OR c.path_b = ?2)
          AND c.co_change_count >= ?3
        ",
    )?;
    let rows = stmt.query_map(params![repo_id, path, MIN_COUPLING_SUPPORT], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (other_path, co, this_count, a_count, b_count, window, last_at_s, language, kind) =
            row?;
        let confidence = if this_count > 0 { co as f64 / this_count as f64 } else { 0.0 };
        let denom = a_count as f64 * b_count as f64;
        let lift = if denom > 0.0 { co as f64 * window as f64 / denom } else { 0.0 };
        // Redundant defense: the lift floor is the WRITE-time storage bound, so every stored row
        // already passes it. Kept so a read stays correct even if rows predate a floor change that
        // hasn't recomputed yet (the params-version stamp forces that recompute on the next read).
        if lift < MIN_COUPLING_LIFT {
            continue;
        }
        out.push(CoupledFile {
            other_path,
            co_change_count: co,
            this_change_count: this_count,
            window_commit_count: window,
            confidence,
            lift,
            last_co_change_at_s: last_at_s,
            language,
            kind,
        });
    }
    out.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.co_change_count.cmp(&a.co_change_count))
            .then_with(|| a.other_path.cmp(&b.other_path))
    });
    out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(out)
}
