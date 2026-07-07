//! Index key-value meta (the `index_meta` table) and the content-revision digest.

use super::*;
use crate::config::ResolvedTarget;

const WATCH_SHUTDOWN_RECONCILE_PENDING_META: &str = "watch_shutdown_reconcile_pending";
const BASE_SCOPE_DISCOVERED_META: &str = "files_base_scope_discovered";

impl IndexDatabase {
    pub(super) fn record_content_revision(&self) -> anyhow::Result<String> {
        let revision = self.content_revision()?;
        // GLOBAL, not per-repo (V040 reclassification): `content_revision()` digests the WHOLE
        // `main.files` (no repo filter — see the method below), so its stored value is scope- and
        // repo-invariant. V039 relocated it to `repo_meta` under the one-DB-per-repo assumption;
        // per-repo copies would make a consolidated DB's FTS freshness alternate. `set_meta` writes
        // the global `index_meta`. (V040's `move_repo_meta_keys_to_global` migrates any stale
        // per-repo copy back; the shared relocate helper no longer re-relocates it.)
        self.set_meta("content_revision", &revision)?;
        Ok(revision)
    }

    /// Read a per-repo meta value (`repo_meta`) for the repo owning this connection — the ergonomic
    /// per-connection twin of the [`repo_meta`] free primitive, scoped by `self.active_repo_id`
    /// (resolved at open: `register_repo` on a config open, the sole repo on a bare open).
    pub(super) fn repo_meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.storage.connection();
        // Bare call → the free `repo_meta` primitive below, never this method (methods need a
        // receiver); the two share a name intentionally (primitive + per-connection wrapper).
        Ok(repo_meta(conn, &self.active_repo_id, key)?)
    }

    /// Upsert a per-repo meta value for the repo owning this connection.
    pub(super) fn set_repo_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        set_repo_meta(self.storage.connection(), &self.active_repo_id, key, value)?;
        Ok(())
    }

    /// Upsert a per-repo meta value only when it changes — returns whether a write happened, so a
    /// no-change incremental/sweep pass avoids dirtying a WAL page (issue #63).
    pub(super) fn set_repo_meta_if_changed(&self, key: &str, value: &str) -> anyhow::Result<bool> {
        Ok(set_repo_meta_if_changed(self.storage.connection(), &self.active_repo_id, key, value)?)
    }

    pub(crate) fn mark_watch_shutdown_reconcile_pending(&self) -> anyhow::Result<bool> {
        self.set_repo_meta_if_changed(WATCH_SHUTDOWN_RECONCILE_PENDING_META, "1")
    }

    pub(crate) fn watch_shutdown_reconcile_pending(&self) -> anyhow::Result<bool> {
        Ok(self.repo_meta(WATCH_SHUTDOWN_RECONCILE_PENDING_META)?.as_deref() == Some("1"))
    }

    pub(crate) fn clear_watch_shutdown_reconcile_pending(&self) -> anyhow::Result<bool> {
        if !self.watch_shutdown_reconcile_pending()? {
            return Ok(false);
        }
        let conn = self.storage.connection();
        delete_repo_meta(conn, &self.active_repo_id, WATCH_SHUTDOWN_RECONCILE_PENDING_META)?;
        Ok(true)
    }

    pub(super) fn active_base_scope_discovered(
        &self,
        targets: &[ResolvedTarget],
    ) -> anyhow::Result<bool> {
        let Some(marker) = self.base_scope_discovery_marker(targets) else {
            return Ok(false);
        };
        Ok(self.repo_meta(BASE_SCOPE_DISCOVERED_META)?.as_deref() == Some(marker.as_str()))
    }

    pub(super) fn mark_active_base_scope_discovered(
        &self,
        targets: &[ResolvedTarget],
    ) -> anyhow::Result<bool> {
        let Some(marker) = self.base_scope_discovery_marker(targets) else {
            return Ok(false);
        };
        self.set_repo_meta_if_changed(BASE_SCOPE_DISCOVERED_META, &marker)
    }

    fn base_scope_discovery_marker(&self, targets: &[ResolvedTarget]) -> Option<String> {
        let scope = if self.active_commit_sha.is_empty() {
            if self.active_worktree_id.is_empty() {
                return None;
            }
            format!("worktree={}", hex_sha256(self.active_worktree_id.as_bytes()))
        } else {
            format!("commit={}", self.active_commit_sha)
        };
        Some(format!(
            "generation={};{};targets={}",
            self.active_generation,
            scope,
            target_scope_fingerprint(targets)
        ))
    }

    pub(super) fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub(super) fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        read_meta(self.storage.connection(), key)
    }

    /// The content digest over EVERY indexed file row — read from the GLOBAL `main.files`, NOT the
    /// scoped `temp.files` view. The FTS mirror (`chunk_fts`), the `content_revision` meta, and the
    /// `fts_dirty` flag are all GLOBAL (one FTS5 index over the whole `chunks` table), so their
    /// freshness must track global content. Reading the scoped view here made `fts_source_revision`
    /// ALTERNATE: `sync_fts` under a linked-overlay scope recorded the overlay-view digest, then a
    /// base read recomputed the base-view digest, saw a mismatch, and rebuilt FTS — and the next
    /// overlay read rebuilt it back, so interleaved base/overlay reads rebuilt the global FTS every
    /// time even though the per-row FTS entries were already in sync (#219 review). The global
    /// digest is scope-invariant, so the freshness check is stable regardless of the active
    /// connection scope.
    pub(super) fn content_revision(&self) -> anyhow::Result<String> {
        // group_concat over an ORDER BY subquery, not `string_agg(... ORDER BY path)`: string_agg()
        // and aggregate ORDER BY are SQLite 3.44+, and this portable idiom (works on any SQLite)
        // avoids tying the query to a SQLite version. The subquery's ORDER BY feeds rows to
        // group_concat in path order, so the digest input is the same deterministic string. Order
        // only has to be stable per-machine (this digest is compared against a previously-stored
        // value on the same DB), which this idiom guarantees.
        let value = self.storage.connection().query_row(
            "SELECT COALESCE(group_concat(pv, ','), '') FROM (SELECT path || ':' || sha256 AS pv \
             FROM main.files WHERE kind != 'deleted' ORDER BY path)",
            [],
            |row| row.get::<_, String>(0),
        )?;
        Ok(hex_sha256(value.as_bytes()))
    }
}

fn target_scope_fingerprint(targets: &[ResolvedTarget]) -> String {
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
pub(crate) fn repo_meta(
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

/// Upsert a per-repo meta value into `repo_meta` (keyed by `(repo_id, key)`). The repo-scoped twin
/// of [`IndexDatabase::set_meta`].
pub(crate) fn set_repo_meta(
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
pub(crate) fn set_repo_meta_if_changed(
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
pub(crate) fn delete_repo_meta(
    conn: &rusqlite::Connection,
    repo_id: &str,
    key: &str,
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", params![repo_id, key])?;
    Ok(())
}
