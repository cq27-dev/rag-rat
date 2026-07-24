use super::query::blame_row;
use super::*;

pub fn cached_blame(
    conn: &Connection,
    chunk_id: i64,
    source_text_hash: &str,
) -> anyhow::Result<Option<ChunkBlameSummary>> {
    conn.query_row(
        "
        SELECT chunk_id, path, start_line, end_line, source_text_hash, line_count,
               dominant_commit, dominant_commit_lines, newest_commit, newest_commit_time_s,
               oldest_commit, oldest_commit_time_s, commit_counts_json
        FROM git_chunk_blame
        WHERE chunk_id = ?1 AND source_text_hash = ?2
        ",
        params![chunk_id, source_text_hash],
        blame_row,
    )
    .optional()
    .map_err(Into::into)
}

pub fn store_blame(conn: &Connection, summary: &ChunkBlameSummary) -> anyhow::Result<()> {
    let counts = serde_json::to_string(&summary.commit_counts)?;
    conn.execute(
        "
        INSERT INTO git_chunk_blame(
            chunk_id, source_text_hash, path, start_line, end_line, line_count,
            dominant_commit, dominant_commit_lines, newest_commit, newest_commit_time_s,
            oldest_commit, oldest_commit_time_s, commit_counts_json, computed_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(chunk_id) DO UPDATE SET
            source_text_hash = excluded.source_text_hash,
            path = excluded.path,
            start_line = excluded.start_line,
            end_line = excluded.end_line,
            line_count = excluded.line_count,
            dominant_commit = excluded.dominant_commit,
            dominant_commit_lines = excluded.dominant_commit_lines,
            newest_commit = excluded.newest_commit,
            newest_commit_time_s = excluded.newest_commit_time_s,
            oldest_commit = excluded.oldest_commit,
            oldest_commit_time_s = excluded.oldest_commit_time_s,
            commit_counts_json = excluded.commit_counts_json,
            computed_at_ms = excluded.computed_at_ms
        ",
        params![
            summary.chunk_id,
            summary.source_text_hash,
            summary.path,
            summary.start_line,
            summary.end_line,
            summary.line_count,
            summary.dominant_commit,
            summary.dominant_commit_lines,
            summary.newest_commit,
            summary.newest_commit_time_s,
            summary.oldest_commit,
            summary.oldest_commit_time_s,
            counts,
            rag_rat_base::time::now_ms(),
        ],
    )?;
    Ok(())
}

pub fn blame_lines(root: &Path, path: &str, start_line: i64, end_line: i64) -> Vec<BlameLine> {
    blame_lines_via_gix(root, path, start_line, end_line).unwrap_or_default()
}

/// Blame `start_line..=end_line` (1-based) of `path` via gix, one `BlameLine` per source line in
/// order, attributing each to the commit gix's blame found and that commit's author time (#212).
fn blame_lines_via_gix(
    root: &Path,
    path: &str,
    start_line: i64,
    end_line: i64,
) -> anyhow::Result<Vec<BlameLine>> {
    let repo = rag_rat_base::repo_discover::discover_repo(root)?;
    let head = repo.head_id()?.detach();
    // gix blames a path relative to the WORKTREE root; the caller's path is relative to the index
    // root, which may be a subdirectory of the worktree.
    let worktree_root = repo.workdir().unwrap_or(root);
    let absolute = root.join(path);
    let relative = absolute.strip_prefix(worktree_root).unwrap_or_else(|_| Path::new(path));

    // Blame attributes lines in HEAD's version of the file. If the file is modified in the
    // worktree, its line numbers don't align with HEAD, so a HEAD blame would
    // shift/mis-attribute the chunk's lines — and `git_blame_chunk` would cache that wrong
    // result under the dirty content hash (#213 review). Skip blame for a modified file. Use
    // gix status (filter-aware: a clean file that merely differs from its blob via
    // .gitattributes / autocrlf / LFS normalization is NOT flagged) rather than a raw byte
    // compare, which would wrongly treat such clean files as dirty (#213 review). (`git blame`
    // with no revision blamed the worktree directly, marking uncommitted lines `0000000`; gix
    // blames committed objects only, so a clean-file guard is the faithful, safe equivalent.)
    if crate::index::git_context::path_is_dirty(&repo, relative) {
        return Ok(Vec::new());
    }

    let file_path = gix::path::into_bstr(relative);
    let start = u32::try_from(start_line.max(1)).unwrap_or(1);
    let end = u32::try_from(end_line.max(start_line)).unwrap_or(start);
    let options = gix::repository::blame_file::Options {
        ranges: gix::blame::BlameRanges::from_one_based_inclusive_range(start..=end)?,
        // Follow whole-file renames, like `git blame` does by default — otherwise every unchanged
        // line in a renamed file is attributed to the rename commit instead of its original author,
        // skewing the chunk's dominant/newest/oldest commit (#213 review).
        rewrites: Some(gix::diff::Rewrites::default()),
        ..Default::default()
    };
    let outcome = repo.blame_file(file_path.as_ref(), head, options)?;
    let mut entries = outcome.entries;
    entries.sort_by_key(|entry| entry.start_in_blamed_file);
    let mut author_time: std::collections::HashMap<gix::ObjectId, Option<i64>> =
        std::collections::HashMap::new();
    let mut lines = Vec::new();
    for entry in entries {
        let commit = entry.commit_id.to_hex().to_string();
        let time = *author_time.entry(entry.commit_id).or_insert_with(|| {
            repo.find_commit(entry.commit_id)
                .ok()
                .and_then(|c| c.author().ok().map(|a| a.seconds()))
        });
        for _ in 0..entry.len.get() {
            lines.push(BlameLine { commit: commit.clone(), author_time_s: time });
        }
    }
    Ok(lines)
}

#[derive(Debug, Clone)]
pub struct BlameLine {
    pub commit: String,
    pub author_time_s: Option<i64>,
}

pub fn source_text_hash(text: &str) -> String {
    hex_sha256(text.as_bytes())
}

/// Delete the cached chunk-blame rows belonging to `repo_id`'s chunks. `git_chunk_blame` carries no
/// `repo_id` (it is transitive via `chunks` → `files`), so scope the delete through the join to
/// `main.files.repo_id` — NOT the `temp.files` scope view, which would additionally narrow to the
/// active commit/worktree and leave this repo's other-context blame behind. A git-history reindex
/// invalidates blame for the whole repo, so every chunk of the active repo is cleared.
pub(super) fn delete_repo_chunk_blame(conn: &Connection, repo_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM git_chunk_blame
         WHERE chunk_id IN (
             SELECT chunks.id
             FROM chunks
             JOIN main.files ON main.files.id = chunks.file_id
             WHERE main.files.repo_id = ?1
         )",
        params![repo_id],
    )?;
    Ok(())
}
