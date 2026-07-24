use rusqlite::{Connection, params};

use super::{
    GIT_RECENT_WEIGHT, GIT_TOTAL_WEIGHT, RECENT_CAP, RECENT_WINDOW_SECS, SearchOptions, TOTAL_CAP,
};

#[derive(Debug, Clone, Default)]
pub(super) struct HistoricalBoost {
    pub(super) git: f64,
    pub(super) papertrail: f64,
}

pub(super) fn historical_boost(
    conn: &Connection,
    path: &str,
    options: SearchOptions,
    repo_id: &str,
) -> anyhow::Result<HistoricalBoost> {
    // `git_file_changes` / `papertrail_refs` are direct-scoped and queried by PATH here
    // (bypassing the scope view), so the `repo_id` predicate keeps a sibling repo's history from
    // boosting a same-named path in a consolidated DB.
    let git = if options.include_git {
        conn.query_row(
            "SELECT COUNT(*) FROM git_file_changes WHERE path = ?1 AND repo_id = ?2 LIMIT 1",
            params![path, repo_id],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        0
    };
    let papertrail = if options.include_papertrail {
        conn.query_row(
            "SELECT COUNT(*) FROM papertrail_refs WHERE source_path = ?1 AND repo_id = ?2 LIMIT 1",
            params![path, repo_id],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        0
    };
    Ok(HistoricalBoost {
        git: if git > 0 { 1.0 } else { 0.0 },
        papertrail: if papertrail > 0 { 1.0 } else { 0.0 },
    })
}

/// Saturating recency+churn magnitude in [0,1] for one candidate path (graded-git rerank, #109).
/// `recent_touch_count` = commits touching the path within the last 90 days; `commit_touch_count` =
/// total distinct commits touching it. A path with no git history scores 0.0. The caps and the
/// recent/total split are A/B-tunable consts above.
pub(super) fn git_score(recent_touch_count: i64, commit_touch_count: i64) -> f64 {
    let recent = (recent_touch_count.max(0) as f64 / RECENT_CAP).min(1.0);
    let total = (commit_touch_count.max(0) as f64 / TOTAL_CAP).min(1.0);
    GIT_RECENT_WEIGHT * recent + GIT_TOTAL_WEIGHT * total
}

/// Per-path graded-git scores keyed by candidate path, computed in ONE batched aggregation query
/// over the whole candidate pool (NOT per candidate — at limit*8 ≈ 80 candidates a per-candidate
/// git query would be the new hottest query). Mirrors the `churn` CTE in
/// `query::repo_brief::file_rows`; `idx_git_file_changes_path` keeps the `path IN (...)` seek
/// cheap. Paths absent from the map (or with no git history) score 0.0.
pub(super) fn graded_git_scores(
    conn: &Connection,
    paths: &[String],
    repo_id: &str,
) -> anyhow::Result<std::collections::HashMap<String, f64>> {
    let mut scores = std::collections::HashMap::new();
    if paths.is_empty() {
        return Ok(scores);
    }
    // `git_commits` / `git_file_changes` are direct-scoped (V040); the newest-commit floor and the
    // churn aggregate both filter `repo_id` so a consolidated DB grades against THIS repo's history
    // only (a fork shares hashes and paths).
    let newest_commit: i64 = conn.query_row(
        "SELECT COALESCE(MAX(authored_at_s), 0) FROM git_commits WHERE repo_id = ?1",
        params![repo_id],
        |row| row.get(0),
    )?;
    // Resolve the 90-day recency floor ONCE per query (not per path).
    let recent_floor = newest_commit.saturating_sub(RECENT_WINDOW_SECS);
    let placeholders = std::iter::repeat_n("?", paths.len()).collect::<Vec<_>>().join(", ");
    // ?1 = recent_floor, ?2..?(paths.len()+1) = paths, ?(paths.len()+2) = repo_id.
    let repo_index = paths.len() + 2;
    let sql = format!(
        "
        SELECT git_file_changes.path,
               COUNT(DISTINCT git_file_changes.commit_hash) AS commit_touch_count,
               SUM(CASE WHEN git_commits.authored_at_s >= ?1 THEN 1 ELSE 0 END) AS \
         recent_touch_count
        FROM git_file_changes
        JOIN git_commits ON git_commits.hash = git_file_changes.commit_hash
                        AND git_commits.repo_id = git_file_changes.repo_id
        WHERE git_file_changes.path IN ({placeholders})
          AND git_file_changes.repo_id = ?{repo_index}
        GROUP BY git_file_changes.path
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params = Vec::<&dyn rusqlite::ToSql>::with_capacity(paths.len() + 2);
    params.push(&recent_floor);
    for path in paths {
        params.push(path);
    }
    params.push(&repo_id);
    let rows = stmt.query_map(params.as_slice(), |row| {
        let path: String = row.get(0)?;
        let commit_touch_count: i64 = row.get(1)?;
        let recent_touch_count: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
        Ok((path, commit_touch_count, recent_touch_count))
    })?;
    for row in rows {
        let (path, commit_touch_count, recent_touch_count) = row?;
        scores.insert(path, git_score(recent_touch_count, commit_touch_count));
    }
    Ok(scores)
}
