//! Low-level index utilities: time, hashing, path stringification, simple SQLite reads.

use super::*;

/// When fewer than this many bytes of stack remain, [`grow_stack`] allocates a fresh segment
/// before running its closure. Comfortably larger than the deepest single (per-recursion-level)
/// frame chain so a level can never straddle the boundary and overflow before the next check.
const STACK_RED_ZONE: usize = 128 * 1024;
/// Size of each stack segment [`grow_stack`] allocates when the red zone is hit.
const STACK_SEGMENT: usize = 4 * 1024 * 1024;

/// Run `f`, first growing the stack if it is near exhaustion (#543). Wrap the body of any
/// tree-sitter descent helper that recurses to unbounded subtree depth — a callee that is thousands
/// of nested parens, a thousands-deep generic type — so parsing a hostile source file grows the
/// stack instead of overflowing the indexer's worker-thread stack (`stacker`, the mechanism rustc
/// uses). It is a no-op fast path (one stack-pointer comparison) when ample stack remains, so real
/// shallow inputs pay effectively nothing, and it does not change any output.
pub(crate) fn grow_stack<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(STACK_RED_ZONE, STACK_SEGMENT, f)
}

pub(crate) fn read_meta(conn: &rusqlite::Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()?)
}

/// Whole-table row count — DELIBERATELY UNSCOPED, so on a consolidated multi-repo DB it reports
/// the union across every repo. TEST-ONLY, structurally: the A7 sweep converted every production
/// reporting caller to [`scoped_table_row_count`] (direct `repo_id` tables) or
/// [`scoped_chunk_row_count`] (the files-transitive `chunks`), and the `#[cfg(test)]` gate keeps
/// a future reporting path from reaching for the lying union count — single-repo test fixtures
/// asserting whole-fixture totals are the only legitimate use.
#[cfg(test)]
pub(crate) fn table_row_count(conn: &rusqlite::Connection, table: &str) -> anyhow::Result<u64> {
    // `table` is always an internal string literal, never user input.
    let count = conn
        .query_row(&format!("SELECT COUNT(*) FROM main.{table}"), [], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// The repo's TOTAL chunk count across ALL its contexts and generations. `chunks` carries no
/// `repo_id` of its own (it scopes transitively through `files`), so this joins `main.files`
/// directly — NOT the scoped temp view, which filters to the active commit/worktree/generation
/// and is narrower than a whole-repo report (gc prunes across all of a repo's contexts).
pub(crate) fn scoped_chunk_row_count(
    conn: &rusqlite::Connection,
    repo_id: &str,
) -> anyhow::Result<u64> {
    let count = conn.query_row(
        "SELECT COUNT(*) FROM main.chunks c JOIN main.files f ON f.id = c.file_id
         WHERE f.repo_id = ?1",
        [repo_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// `table_row_count` for a directly-`repo_id`-scoped table: counts only the rows owned by `repo_id`
/// (the active repo), so a status/freshness read reports THIS repo's totals rather than the union
/// across every repo in a consolidated DB. `table` is always an internal string literal, never user
/// input, and MUST carry a `repo_id` column (the V040/V041 direct-scoped tables — git_commits,
/// git_file_changes, the papertrail_* tables).
pub(crate) fn scoped_table_row_count(
    conn: &rusqlite::Connection,
    table: &str,
    repo_id: &str,
) -> anyhow::Result<u64> {
    let count = conn.query_row(
        &format!("SELECT COUNT(*) FROM main.{table} WHERE repo_id = ?1"),
        [repo_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Whether `text` contains a test marker — the file-level `files.has_test_code` signal that lets
/// `impact_surface`'s "tests touching this symbol" query filter on an indexed flag instead of a
/// `chunks.text LIKE` scan (#77). INVARIANT: this marker set MUST match the V024 backfill SQL in
/// `schema::migrations::apply_files_has_test_code` and `test_items`'s filter, or an incrementally
/// reindexed file and a forward-migrated one would disagree. (The `it(` / `test(` substrings are
/// intentionally broad — they reproduce the original `LIKE '%it(%'` / `'%test(%'` behavior exactly,
/// noise included, so the precomputed flag is a faithful drop-in for the scan it replaces.)
pub(crate) fn text_has_test_marker(text: &str) -> bool {
    text.contains("#[cfg(test)]")
        || text.contains("describe(")
        || text.contains("it(")
        || text.contains("test(")
}

pub(crate) fn file_metadata_ms(path: &Path) -> anyhow::Result<i64> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(duration_ms(modified.duration_since(UNIX_EPOCH)?))
}

pub(crate) fn now_ms() -> i64 {
    duration_ms(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default())
}

pub(crate) fn duration_ms(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

pub(crate) fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `path_string` for the read path: the importance auto-seed normalizes a `git_changed_paths` entry
/// to the same `/`-separated form the `files` table stores, so the scoped-view lookup matches.
pub(crate) fn path_string_for_seed(path: &Path) -> String {
    path_string(path)
}
