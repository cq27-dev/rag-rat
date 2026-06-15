//! Low-level index utilities: time, hashing, path stringification, simple SQLite reads.

use super::*;

pub(crate) fn read_meta(conn: &rusqlite::Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()?)
}

pub(crate) fn table_row_count(conn: &rusqlite::Connection, table: &str) -> anyhow::Result<u64> {
    // `table` is always an internal string literal, never user input.
    let count = conn
        .query_row(&format!("SELECT COUNT(*) FROM main.{table}"), [], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
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

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(crate) fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `path_string` for the read path: the importance auto-seed normalizes a `git_changed_paths` entry
/// to the same `/`-separated form the `files` table stores, so the scoped-view lookup matches.
pub(crate) fn path_string_for_seed(path: &Path) -> String {
    path_string(path)
}
