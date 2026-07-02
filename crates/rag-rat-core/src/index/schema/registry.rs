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

    // No real repo yet: adopt the placeholder in place, or insert fresh if it is somehow absent.
    let placeholder_present = conn
        .query_row("SELECT 1 FROM repos WHERE repo_id = ?1", [LEGACY_REPO_ID], |_| Ok(()))
        .optional()?
        .is_some();
    if placeholder_present {
        conn.execute(
            "UPDATE repos SET repo_id = ?1, display_name = ?2, registered_at_ms = ?3
             WHERE repo_id = ?4",
            params![identity.repo_id, identity.display_name, now_ms, LEGACY_REPO_ID],
        )?;
    } else {
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?2, ?3)",
            params![identity.repo_id, identity.display_name, now_ms],
        )?;
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
