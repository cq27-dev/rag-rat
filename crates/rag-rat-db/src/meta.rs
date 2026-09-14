//! Index key-value meta (the `index_meta` table) and the content-revision digest.

use rag_rat_base::config::ResolvedTarget;
use rag_rat_base::hash::hex_sha256;
use rag_rat_base::paths::path_string;
use rusqlite::{Connection, OptionalExtension, params};

pub mod watch_placement;

/// Read one `index_meta` value.
pub fn read_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()
}

pub const WATCH_SHUTDOWN_RECONCILE_PENDING_META: &str = "watch_shutdown_reconcile_pending";
/// Watch-placement failure HIGH-WATER MARK the resident watcher has seen (see `watch::placement`).
/// Persisted per pass, never lowered, so `index_status` can surface silent inotify degradation
/// without one watcher process masking another's (see `record_watch_placement_failures`).
pub const WATCH_PLACEMENT_FAILURES_META: &str = "watch_placement_failures";
pub const BASE_SCOPE_DISCOVERED_META: &str = "files_base_scope_discovered";
/// Monotonic per-repo clock maintained by schema triggers and transactional bulk writers for
/// Lens-visible enrichment rows.
pub const LENS_ENRICHMENT_REVISION_META: &str = "lens_enrichment_revision";
pub const LENS_SYMBOLS_REVISION_META: &str = "lens_symbols_revision";
pub const LENS_CLONES_REVISION_META: &str = "lens_clones_revision";
pub const LENS_MEMORIES_REVISION_META: &str = "lens_memories_revision";
pub const LENS_COUPLING_REVISION_META: &str = "lens_coupling_revision";
pub const LENS_PAPERTRAIL_REVISION_META: &str = "lens_papertrail_revision";
pub const LENS_LANE_REVISION_METAS: &[&str] = &[
    LENS_SYMBOLS_REVISION_META,
    LENS_CLONES_REVISION_META,
    LENS_MEMORIES_REVISION_META,
    LENS_COUPLING_REVISION_META,
    LENS_PAPERTRAIL_REVISION_META,
];
/// Prefix of the per-worktree overlay refresh-basis key; the SUFFIX is a `worktree_id`, so the key
/// carries a checkout path.
///
/// Declared here rather than beside its runtime reads because the V097 rekey (#1048) must find
/// these keys to rewrite a stale Windows path spelling, and lives below the crate that reads them.
/// One literal, shared, so the migration and the reader cannot drift into a silent miss.
pub const WORKTREE_OVERLAY_BASIS_META_PREFIX: &str = "worktree_overlay_basis:";
/// Per-repo git-history reload cursor whose VALUE is the indexed root — the `config.root` spelling
/// the history rows were read at, compared TEXTUALLY against a freshly canonicalized root. Its
/// siblings (`_head`, `_shallow`, `_complete`) are a commit hash and two flags, so this is the only
/// one of the four that carries a path.
///
/// Declared here for the same reason as [`WORKTREE_OVERLAY_BASIS_META_PREFIX`]: the V097 rekey
/// (#1048) must rewrite this value and lives below the crate that reads it. One literal, shared, so
/// the migration and the reader cannot drift into a silent miss.
pub const GIT_HISTORY_INDEXED_ROOT_META: &str = "git_history_indexed_root";

/// Advance the per-repo Lens enrichment write clock once for one logical transaction.
pub fn bump_lens_enrichment_revision(
    conn: &rusqlite::Connection,
    repo_id: &str,
) -> rusqlite::Result<()> {
    bump_lens_revisions(conn, repo_id, &[LENS_ENRICHMENT_REVISION_META])
}

/// Advance one or more per-repo Lens lane clocks once for one logical transaction.
pub fn bump_lens_revisions(
    conn: &rusqlite::Connection,
    repo_id: &str,
    keys: &[&str],
) -> rusqlite::Result<()> {
    let mut statement = conn.prepare_cached(
        "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, '1')
         ON CONFLICT(repo_id, key) DO UPDATE SET
             value = CAST(COALESCE(value, '0') AS INTEGER) + 1",
    )?;
    for key in keys {
        statement.execute(params![repo_id, key])?;
    }
    Ok(())
}

pub fn target_scope_fingerprint(targets: &[ResolvedTarget]) -> String {
    let mut input = String::new();
    for target in targets {
        input.push_str("target\0");
        input.push_str(&target.name);
        input.push('\0');
        input.push_str(target.language.as_db_str());
        input.push('\0');
        input.push_str(target.kind.as_db_str());
        input.push('\0');
        for dir in &target.directories {
            input.push_str("dir\0");
            input.push_str(&path_string(dir));
            input.push('\0');
        }
        for include in &target.include {
            input.push_str("include\0");
            input.push_str(include);
            input.push('\0');
        }
        for exclude in &target.exclude {
            input.push_str("exclude\0");
            input.push_str(exclude);
            input.push('\0');
        }
    }
    hex_sha256(input.as_bytes())
}

/// Read a per-repo meta value from the `repo_meta` table — the repo-scoped twin of
/// [`read_meta`](crate::index::read_meta) (which reads the global `index_meta`). `repo_id` is the
/// owning repo — the caller's active-repo scope (`IndexDatabase::active_repo_id`, or
/// [`schema::active_repo_id`](crate::index::schema) on a free connection). Returns `None` when the
/// key is unset for that repo.
pub fn repo_meta(
    conn: &rusqlite::Connection,
    repo_id: &str,
    key: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM repo_meta WHERE repo_id = ?1 AND key = ?2",
        params![repo_id, key],
        |row| row.get(0),
    )
    .optional()
}

/// Atomically raise a per-repo INTEGER meta value to `value`, never lowering it — the max is
/// computed IN the upsert (one statement), so two watcher processes recording failures for the same
/// repo at once cannot interleave a read-then-write and regress the high-water mark. Returns
/// whether the stored value rose (a fresh insert, or an increase). A same-or-lower `value` is a
/// no-op.
pub fn bump_repo_meta_high_water(
    conn: &rusqlite::Connection,
    repo_id: &str,
    key: &str,
    value: u64,
) -> rusqlite::Result<bool> {
    conn.execute(
        "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value
             WHERE CAST(excluded.value AS INTEGER) > CAST(repo_meta.value AS INTEGER)",
        params![repo_id, key, value.to_string()],
    )?;
    Ok(conn.changes() > 0)
}

/// Upsert a per-repo meta value into `repo_meta` (keyed by `(repo_id, key)`). The repo-scoped twin
/// of [`IndexDatabase::set_meta`].
pub fn set_repo_meta(
    conn: &rusqlite::Connection,
    repo_id: &str,
    key: &str,
    value: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value",
        params![repo_id, key, value],
    )?;
    Ok(())
}

/// Upsert a per-repo meta value only when it differs from the stored value — returns whether a
/// write happened, so a no-change pass avoids dirtying a WAL page (the #63 property, mirrored from
/// [`IndexDatabase::set_meta_if_changed`]).
pub fn set_repo_meta_if_changed(
    conn: &rusqlite::Connection,
    repo_id: &str,
    key: &str,
    value: &str,
) -> rusqlite::Result<bool> {
    if repo_meta(conn, repo_id, key)?.as_deref() == Some(value) {
        return Ok(false);
    }
    set_repo_meta(conn, repo_id, key, value)?;
    Ok(true)
}

/// Delete a per-repo meta key (a no-op when absent) — needed by the clear paths of the relocated
/// model / reencode-cursor keys.
pub fn delete_repo_meta(
    conn: &rusqlite::Connection,
    repo_id: &str,
    key: &str,
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", params![repo_id, key])?;
    Ok(())
}

/// `table_row_count` for a directly-`repo_id`-scoped table: counts only the rows owned by `repo_id`
/// (the active repo), so a status/freshness read reports THIS repo's totals rather than the union
/// across every repo in a consolidated DB. `table` is always an internal string literal, never user
/// input, and MUST carry a `repo_id` column (the V040/V041 direct-scoped tables — git_commits,
/// git_file_changes, the papertrail_* tables).
pub fn scoped_table_row_count(
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

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Remove an `index_meta` key (the global-scope companion to [`delete_repo_meta`]). Idempotent:
/// deleting an absent key is a no-op.
pub fn delete_meta(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM index_meta WHERE key = ?1", [key])?;
    Ok(())
}

/// The values of every `index_meta` key starting with `prefix`. GLOB (not LIKE) so `_` in a key
/// prefix stays literal; callers pass fixed prefixes without GLOB metacharacters (`*?[`).
pub fn meta_values_with_prefix(conn: &Connection, prefix: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT value FROM index_meta WHERE key GLOB ?1 || '*' ORDER BY key")?;
    let rows = stmt.query_map([prefix], |row| row.get::<_, String>(0))?;
    rows.collect()
}
