use super::*;

/// One commit-replay eval case (#120): the commit message is the QUERY and the diff's changed paths
/// are the GOLD ("the diff is the gold"). Built from the indexed `git_commits` / `git_file_changes`
/// so no working-tree checkout is needed to enumerate cases.
#[derive(Debug, Clone)]
pub struct ReplayCase {
    pub hash: String,
    pub subject: String,
    pub body: String,
    /// Paths the commit's diff touched — the recall gold for this case.
    pub changed_paths: Vec<String>,
}

/// Build commit-replay eval cases from the indexed git history, newest first. Caveats designed in
/// (#120): `max_files` drops BULK/mechanical commits (renames, formatting sweeps) whose path-recall
/// is noise; merge commits are excluded (their diff is a union, not a focused change).
pub fn replay_commit_cases(
    conn: &Connection,
    limit: u32,
    max_files: u32,
) -> anyhow::Result<Vec<ReplayCase>> {
    // Direct-scoped (V040): the eval cases come only from the ACTIVE repo's history.
    let repo_id = schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT hash, subject, body
        FROM git_commits
        WHERE changed_file_count BETWEEN 1 AND ?2
          AND subject NOT LIKE 'Merge %'
          AND repo_id = ?3
        ORDER BY authored_at_s DESC
        LIMIT ?1
        ",
    )?;
    let commits: Vec<(String, String, String)> = stmt
        .query_map(params![i64::from(limit), i64::from(max_files), repo_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut paths_stmt = conn.prepare(
        "SELECT path FROM git_file_changes WHERE commit_hash = ?1 AND repo_id = ?2 ORDER BY path",
    )?;
    let mut cases = Vec::with_capacity(commits.len());
    for (hash, subject, body) in commits {
        let changed_paths: Vec<String> = paths_stmt
            .query_map(params![hash, repo_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        if changed_paths.is_empty() {
            continue;
        }
        cases.push(ReplayCase { hash, subject, body, changed_paths });
    }
    Ok(cases)
}

/// The set of paths the repo config currently indexes (live `files` rows, excluding deletion
/// tombstones). The commit-replay eval (#120) restricts its gold to this set: a path the config
/// doesn't index (`.github/**`, `tools/**`, a root manifest, …) can never be retrieved, so counting
/// it as missing gold would make recall@k track the file mix of recent commits rather than search
/// quality (#315).
pub fn indexed_path_set(conn: &Connection) -> anyhow::Result<BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT path FROM files WHERE kind != 'deleted'")?;
    let paths = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<String>>>()?;
    Ok(paths)
}

/// Distinct `chunks.symbol_path` for chunks in `path` whose line span overlaps any of `ranges`
/// (inclusive). Commit-replay (#120) uses this to derive symbol-level gold: the symbols a commit
/// touched, in the SAME `symbol_path` format the search results carry, so symbol-recall is
/// measurable. `ranges` are PARENT-side diff line ranges, queried against a PARENT-state index.
pub fn chunk_symbol_paths_in_ranges(
    conn: &Connection,
    path: &str,
    ranges: &[(i64, i64)],
) -> anyhow::Result<Vec<String>> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT chunks.symbol_path
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        WHERE files.path = ?1
          AND chunks.symbol_path IS NOT NULL
          AND chunks.start_line <= ?3
          AND chunks.end_line >= ?2
        ",
    )?;
    let mut symbols = BTreeSet::new();
    for &(start, end) in ranges {
        let rows = stmt.query_map(params![path, start, end], |row| row.get::<_, String>(0))?;
        for symbol in rows {
            symbols.insert(symbol?);
        }
    }
    Ok(symbols.into_iter().collect())
}

pub fn commit_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<CommitSearchHit>> {
    let fts_query = fts_query(query);
    // `commit_fts` is external-content over the direct-scoped `git_commits` (V040); the MATCH runs
    // over every repo's commits, so the join predicate `git_commits.repo_id = ?` is what keeps a
    // sibling repo's commits out of THIS repo's results in a consolidated DB.
    let repo_id = schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT git_commits.hash, git_commits.author_name, git_commits.author_email,
               git_commits.authored_at_s, git_commits.committed_at_s,
               git_commits.subject, git_commits.body, git_commits.changed_file_count,
               bm25(commit_fts) AS score
        FROM commit_fts
        JOIN git_commits ON git_commits.rowid = commit_fts.rowid
        WHERE commit_fts MATCH ?1 AND git_commits.repo_id = ?3
        ORDER BY score, git_commits.authored_at_s DESC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![fts_query, i64::from(limit), repo_id], |row| {
        Ok(CommitSearchHit {
            hash: row.get(0)?,
            author_name: row.get(1)?,
            author_email: row.get(2)?,
            authored_at_s: row.get(3)?,
            committed_at_s: row.get(4)?,
            subject: row.get(5)?,
            body: row.get(6)?,
            changed_file_count: row.get(7)?,
            score: row.get(8)?,
            evidence_kind: "historical",
        })
    })?;
    let mut hits = collect_rows(rows)?;
    for (rank, hit) in hits.iter_mut().enumerate() {
        hit.score = positive_rank_score(rank);
    }
    Ok(hits)
}

fn positive_rank_score(rank: usize) -> f64 {
    rag_rat_query::round_score(1.0 / ((rank + 1) as f64).sqrt())
}

pub fn history_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<PathHistoryItem>> {
    // `git_file_changes` / `git_commits` are direct-scoped (V040): join AND filter on `repo_id` so
    // a fork sharing a commit hash can't surface a sibling repo's change rows for this path.
    let repo_id = schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT git_commits.hash, git_file_changes.path, git_file_changes.additions,
               git_file_changes.deletions, git_file_changes.change_kind,
               git_commits.author_name, git_commits.authored_at_s, git_commits.subject
        FROM git_file_changes
        JOIN git_commits ON git_commits.hash = git_file_changes.commit_hash
                        AND git_commits.repo_id = git_file_changes.repo_id
        WHERE git_file_changes.path = ?1 AND git_file_changes.repo_id = ?3
        ORDER BY git_commits.authored_at_s DESC, git_commits.hash
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![path, i64::from(limit), repo_id], path_history_row)?;
    collect_rows(rows)
}

pub fn commits_touching_query(
    conn: &Connection,
    query: &str,
    limit: u32,
    current_hits: &[SearchHit],
) -> anyhow::Result<Vec<QueryCommitHit>> {
    let mut combined = BTreeMap::<String, QueryCommitHit>::new();
    for (rank, hit) in commit_search(conn, query, limit)?.into_iter().enumerate() {
        combined.insert(hit.hash.clone(), QueryCommitHit {
            hash: hit.hash,
            author_name: hit.author_name,
            authored_at_s: hit.authored_at_s,
            subject: hit.subject,
            changed_file_count: hit.changed_file_count,
            evidence: vec!["commit_message".to_string()],
            score: rank as f64,
            evidence_kind: "historical",
        });
    }

    let mut paths = BTreeSet::new();
    for hit in current_hits {
        paths.insert(hit.path.as_str());
    }
    for path in paths {
        for item in history_for_path(conn, path, limit)? {
            let entry = combined.entry(item.hash.clone()).or_insert_with(|| QueryCommitHit {
                hash: item.hash.clone(),
                author_name: item.author_name.clone(),
                authored_at_s: item.authored_at_s,
                subject: item.subject.clone(),
                changed_file_count: 0,
                evidence: Vec::new(),
                score: f64::from(limit),
                evidence_kind: "historical",
            });
            if !entry.evidence.iter().any(|value| value == "file_change") {
                entry.evidence.push("file_change".to_string());
            }
            entry.score -= 0.25;
        }
    }

    let mut hits = combined.into_values().collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        left.score
            .partial_cmp(&right.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.authored_at_s.cmp(&left.authored_at_s))
    });
    hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hits)
}

fn path_history_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PathHistoryItem> {
    Ok(PathHistoryItem {
        hash: row.get(0)?,
        path: row.get(1)?,
        additions: row.get(2)?,
        deletions: row.get(3)?,
        change_kind: row.get(4)?,
        author_name: row.get(5)?,
        authored_at_s: row.get(6)?,
        subject: row.get(7)?,
        evidence_kind: "historical",
    })
}

pub(super) fn blame_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkBlameSummary> {
    let counts_json: String = row.get(12)?;
    let commit_counts = serde_json::from_str(&counts_json).unwrap_or_default();
    Ok(ChunkBlameSummary {
        chunk_id: row.get(0)?,
        path: row.get(1)?,
        start_line: row.get(2)?,
        end_line: row.get(3)?,
        source_text_hash: row.get(4)?,
        line_count: row.get(5)?,
        dominant_commit: row.get(6)?,
        dominant_commit_lines: row.get(7)?,
        newest_commit: row.get(8)?,
        newest_commit_time_s: row.get(9)?,
        oldest_commit: row.get(10)?,
        oldest_commit_time_s: row.get(11)?,
        commit_counts,
        evidence_kind: "historical",
    })
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn fts_query(query: &str) -> String {
    let terms = query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() { "\"\"".to_string() } else { terms.join(" OR ") }
}
