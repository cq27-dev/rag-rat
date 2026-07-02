use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::repo_identity::RepoIdentity;

/// The placeholder `repo_id` a freshly-migrated single-repo DB carries until it is adopted (see the
/// V038 DDL) — and the backfill value every direct-scoped table gets when later migrations add
/// `repo_id` columns (phase A3). A consolidated DB holding more than one repo NEVER carries this id
/// (enforced by [`register_repo`]).
pub const LEGACY_REPO_ID: &str = "__unassigned__";

/// Register `identity` in this database's `repos`/`repo_roots` registry, returning the stored
/// `repo_id`.
///
/// Idempotent and safe to call on every open:
/// - **Fresh legacy DB** (only the [`LEGACY_REPO_ID`] placeholder row present) → *adoption*: the
///   placeholder row is rewritten to `identity.repo_id` in place (A1 rewrites only `repos`; phase
///   A3 extends adoption to every direct-scoped table once they gain `repo_id`). The working-tree
///   `root` is recorded in `repo_roots`.
/// - **Already this repo** → a no-op except that a not-yet-seen `root` is appended to `repo_roots`
///   (worktrees of one repo share a `repo_id`, so several roots on one machine are expected).
/// - **A different real repo_id already owns the DB** → refuses (returns an error) rather than
///   corrupting a single-repo DB. Multi-repo registration is deliberately out of scope until the
///   default-path flip (phase A7); until then one DB == one repo.
///
/// `now_ms` is the injected clock (see the repo's time-injection convention), stamped as the
/// registration time on adoption / first insert.
pub fn register_repo(
    conn: &Connection,
    identity: &RepoIdentity,
    root: &Path,
    now_ms: i64,
) -> rusqlite::Result<String> {
    // Defense in depth: the resolver already refuses a pinned placeholder and never derives it, but
    // a caller can hand-build a `RepoIdentity`. Registering the placeholder (or an empty/whitespace
    // id) would degenerate adoption — `real_repo_ids` filters the marker, so the adoption UPDATE
    // would rewrite the placeholder PK to itself, roots would pool under the marker, and this would
    // return success while the DB stayed unadopted (a later real registration then trips the child
    // FK or orphans roots under the marker).
    let trimmed_id = identity.repo_id.trim();
    if trimmed_id.is_empty() || trimmed_id == LEGACY_REPO_ID {
        return Err(registry_refusal(format!(
            "refusing to register the reserved or empty repo_id {:?}: `{LEGACY_REPO_ID}` is the \
             pre-adoption placeholder and an empty id cannot scope rows",
            identity.repo_id
        )));
    }

    let root = root.to_string_lossy();
    let real_ids = real_repo_ids(conn)?;

    // Already registered under this id → idempotent; just make sure the root is recorded.
    if real_ids.iter().any(|id| id == &identity.repo_id) {
        record_repo_root(conn, &identity.repo_id, &root, now_ms)?;
        return Ok(identity.repo_id.clone());
    }

    // A different real repo already owns this DB — refuse rather than adopt (single-repo invariant
    // for phase A; multi-repo registration lands with the default-path flip).
    if let Some(other) = real_ids.first() {
        return Err(registry_refusal(format!(
            "cannot register repo {}: this index is already registered to a different repo {}",
            identity.repo_id, other
        )));
    }

    // No real repo yet: adopt the placeholder. Insert the real `repos` row FIRST, then re-point the
    // placeholder's children, then drop the placeholder. Since V039 (phase A2) `repo_meta` carries
    // rows under the placeholder, and its FK to `repos` is enforced (`foreign_keys = ON`), the
    // placeholder PK cannot be rewritten in place while children reference it. Insert-first /
    // delete-last keeps every FK satisfied at each statement boundary without needing an enclosing
    // transaction or deferred checks. `repo_roots` never holds placeholder rows (it is only ever
    // written under a real id, by `record_repo_root` below), so only `repo_meta` needs re-pointing;
    // A3 extends this to the direct-scoped tables it adds.
    let placeholder_present = conn
        .query_row("SELECT 1 FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID], |_| Ok(()))
        .optional()?
        .is_some();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?2, ?3)",
        params![identity.repo_id, identity.display_name, now_ms],
    )?;
    if placeholder_present {
        conn.execute("UPDATE repo_meta SET repo_id = ?1 WHERE repo_id = ?2", params![
            identity.repo_id,
            LEGACY_REPO_ID
        ])?;
        conn.execute("DELETE FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID])?;
    }
    record_repo_root(conn, &identity.repo_id, &root, now_ms)?;
    Ok(identity.repo_id.clone())
}

/// A `SQLITE_CONSTRAINT` failure carrying `msg` — the registry's refusal shape. These are
/// single-repo-invariant violations (a reserved/empty id, or a different repo already owning the
/// DB), not real SQL constraint trips; the shape keeps them on `rusqlite::Result` without a bespoke
/// error type for the phase-A registry.
fn registry_refusal(msg: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some(msg),
    )
}

/// The single repo owning this database — the connection-level active-repo stand-in the per-repo
/// [`repo_meta`](crate::index::repo_meta) accessors resolve until phase A3 threads a real
/// `ScopeContext`. Phase A keeps ONE repo per DB ([`register_repo`] refuses a second), so `repos`
/// holds exactly one row: the adopted real id after [`register_repo`], or the [`LEGACY_REPO_ID`]
/// placeholder on a not-yet-adopted DB (a bare `schema::apply` in a test, or any open before A3
/// wires registration into `open_config`). A3 replaces every caller with the active repo id from
/// its scope context.
pub(crate) fn single_repo_id(conn: &Connection) -> rusqlite::Result<String> {
    debug_assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM repos", [], |row| row.get::<_, i64>(0))?,
        1,
        "phase A holds exactly one repos row per DB until the default-path flip (A3/A7)"
    );
    conn.query_row("SELECT repo_id FROM repos LIMIT 1", [], |row| row.get(0))
}

/// Every registered `repo_id` that is a real (adopted) id — i.e. excluding the [`LEGACY_REPO_ID`]
/// placeholder. A single-repo DB returns at most one.
fn real_repo_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT repo_id FROM repos WHERE repo_id != ?1 ORDER BY repo_id")?;
    let ids = stmt.query_map([LEGACY_REPO_ID], |row| row.get::<_, String>(0))?;
    ids.collect()
}

/// Append a working-tree root for `repo_id` (idempotent per `(repo_id, root)` — worktrees of one
/// repo pool under the same id, so several roots are expected).
fn record_repo_root(
    conn: &Connection,
    repo_id: &str,
    root: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO repo_roots(repo_id, root, registered_at_ms) VALUES (?1, ?2, ?3)",
        params![repo_id, root, now_ms],
    )?;
    Ok(())
}
