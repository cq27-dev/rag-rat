//! Read side of the git change-coupling signal: the top coupled partners for a file, ranked by
//! asymmetric confidence. The windowed computation + persistence (`git_change_couplings`) lives
//! with the indexer; this is the pure reader the impact surface consumes.

use rusqlite::{Connection, params};

/// Support floor: a pair must co-change at least this many times inside the window to be stored.
pub const MIN_COUPLING_SUPPORT: i64 = 2;
pub const MIN_COUPLING_LIFT: f64 = 1.5;

#[derive(Debug)]
pub struct CoupledFile {
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

pub fn coupled_files_for_path(
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
