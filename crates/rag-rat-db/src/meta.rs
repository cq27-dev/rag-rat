//! Index key-value meta (the `index_meta` table) and the content-revision digest.

use rag_rat_base::config::{Config, ResolvedTarget};
use rag_rat_base::hash::hex_sha256;
use rag_rat_base::paths::path_string;
use rusqlite::{Connection, OptionalExtension, params};

use crate::storage::IndexConnection;

/// Read one `index_meta` value.
pub fn read_meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()?)
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

/// Advance the per-repo Lens enrichment write clock once for one logical transaction.
pub fn bump_lens_enrichment_revision(
    conn: &rusqlite::Connection,
    repo_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, '1')
         ON CONFLICT(repo_id, key) DO UPDATE SET
             value = CAST(COALESCE(value, '0') AS INTEGER) + 1",
        params![repo_id, LENS_ENRICHMENT_REVISION_META],
    )?;
    Ok(())
}

pub fn target_scope_fingerprint(targets: &[ResolvedTarget]) -> String {
    let mut input = String::new();
    for target in targets {
        input.push_str("target\0");
        input.push_str(&target.name);
        input.push('\0');
        input.push_str(target.language.as_str());
        input.push('\0');
        input.push_str(target.kind.as_str());
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

/// Persist a watch-placement failure high-water mark for the watcher's out-of-band flush paths —
/// the non-blocking post-resync flush on the event loop and the shutdown flush (#658 review). It is
/// deliberately CHEAP, NON-BLOCKING, and SIDE-EFFECT-FREE:
/// - **No index creation.** Returns early (no write) when the DB file does not exist, and opens
///   NON-creating ([`IndexConnection::open_read_write_no_create_nowait`]) — a first-time-empty
///   checkout that never built an index must not gain a schemaless `.rag-rat/index.sqlite` here.
/// - **No blocking.** The no-wait open + `busy_timeout = 0` mean a concurrent writer (another repo
///   in a consolidated DB, a checkpoint) yields `SQLITE_BUSY`, treated as SKIP — the event loop
///   must never stall on classification/fleet triggers, and the count rides the next pass.
/// - **No on-open heals.** Unlike `open_config` (schema migration, graph-index / model-manifest /
///   generated-flags heals), so a degraded watcher exiting after a binary/schema-version change
///   can't spend an unbounded heal here.
///
/// Config-SCOPED via [`crate::schema::resolve_config_repo_id`] so it targets the SAME repo the
/// pass's persist did, even in a consolidated multi-repo DB (a bare sole-repo pick could hit a
/// sibling). Also skips when the repo isn't registered yet (nothing to surface into) or the schema
/// isn't Compatible (the next full pass migrates + persists). The caller owns the per-repo write
/// lock.
///
/// Returns whether the flush is SETTLED — `Ok(true)` means the count is now in the DB, or there is
/// definitively nothing to persist into (no index file yet, repo not registered, schema not
/// Compatible) so retrying is pointless; `Ok(false)` means a TRANSIENT `SQLITE_BUSY` skip, so the
/// caller should retry (the event-loop drain re-attempts next tick). A non-busy error propagates.
pub fn record_watch_placement_failures_scoped(
    config: &Config,
    failures: u64,
) -> anyhow::Result<bool> {
    // A first-time-empty checkout has no index yet: skip WITHOUT opening, so the flush never
    // creates a schemaless DB file (which would break the friendly no-index read path). Settled — a
    // later pass registers the repo and persists once real content appears.
    if !config.database.try_exists().unwrap_or(false) {
        return Ok(true);
    }
    match write_watch_placement_high_water(config, failures) {
        Ok(()) => Ok(true),
        // No-wait open/write: a busy DB (concurrent writer / checkpoint) is a TRANSIENT skip — the
        // caller retries (the count rides the next tick, pass, sweep, or shutdown flush). A file
        // that vanished in the race between the check above and the open surfaces as a non-busy
        // open error, propagated for the caller to log (best-effort).
        Err(err) if crate::storage::is_busy(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

fn write_watch_placement_high_water(config: &Config, failures: u64) -> anyhow::Result<()> {
    let storage = IndexConnection::open_read_write_no_create_nowait(&config.database)?;
    let conn = storage.connection();
    if crate::schema::status(conn)?.state != crate::schema::SchemaState::Compatible {
        return Ok(());
    }
    let Some(repo_id) = crate::schema::resolve_config_repo_id(
        conn,
        &config.root,
        config.repo_id_override.as_deref(),
    )?
    else {
        return Ok(());
    };
    bump_repo_meta_high_water(conn, &repo_id, WATCH_PLACEMENT_FAILURES_META, failures)?;
    Ok(())
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

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Remove an `index_meta` key (the global-scope companion to [`delete_repo_meta`]). Idempotent:
/// deleting an absent key is a no-op.
pub fn delete_meta(conn: &Connection, key: &str) -> anyhow::Result<()> {
    conn.execute("DELETE FROM index_meta WHERE key = ?1", [key])?;
    Ok(())
}

pub fn meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()?)
}
