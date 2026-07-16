//! `IndexDatabase` meta accessors — the engine-handle surface over the db layer's
//! `rag_rat_db::meta` free functions.

use rag_rat_base::config::ResolvedTarget;
use rag_rat_base::hash::hex_sha256;
use rag_rat_db::meta::*;

use super::*;

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

    /// Record the resident watcher's watch-placement failures as a HIGH-WATER MARK — raised only
    /// when it exceeds the stored value, never lowered. Returns whether the stored value rose.
    ///
    /// Never lowered because several watcher processes can share this database (a checkout of one
    /// repo can have more than one live watcher). A healthy or freshly-restarted watcher (whose
    /// process-local source count is 0 or low) must not erase a degraded watcher's recorded
    /// failures, so [`bump_repo_meta_high_water`] takes the max ATOMICALLY in one upsert — no
    /// read-then-write to race. Once any watcher has degraded, `index_status` never falsely reports
    /// zero. Trade-off: distinct processes dropping watches at once report the max, not the sum —
    /// an acceptable under-count for a "watching is degraded" signal that never over- or
    /// zero-reports.
    pub(crate) fn record_watch_placement_failures(&self, failures: u64) -> anyhow::Result<bool> {
        // Atomic max-upsert (a single statement) — never a read-then-write, so a concurrent
        // same-repo writer cannot interleave and regress the high-water mark.
        Ok(bump_repo_meta_high_water(
            self.storage.connection(),
            &self.active_repo_id,
            WATCH_PLACEMENT_FAILURES_META,
            failures,
        )?)
    }

    /// The persisted watch-placement failure high-water mark (0 if never written — e.g. a repo
    /// whose watcher has never dropped a watch, or a checkout with no resident watcher).
    pub(crate) fn watch_placement_failures(&self) -> anyhow::Result<u64> {
        Ok(self
            .repo_meta(WATCH_PLACEMENT_FAILURES_META)?
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0))
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

    /// Whether `branch_targets` (a linked overlay's config targets) MAY differ from the config the
    /// base scope was indexed with — the cheap gate for the per-file target-identity drift scan
    /// ([`Self::base_scope_target_drift`]). The base scope's discovery marker embeds its target
    /// fingerprint; when it equals the branch's, no file can re-language, so the O(base-files) scan
    /// is skipped — the common no-divergent-branch-config case that would otherwise re-scan every
    /// base file on every overlay refresh × worktree, undoing the #577 event-scoping win.
    /// Conservatively returns `true` when the marker is absent (a match can't be proven).
    pub(super) fn overlay_targets_may_drift(
        &self,
        branch_targets: &[ResolvedTarget],
    ) -> anyhow::Result<bool> {
        let Some(marker) = self.repo_meta(BASE_SCOPE_DISCOVERED_META)? else {
            return Ok(true);
        };
        // Marker layout: `generation=…;<scope>;targets=<hex fingerprint>` — the fingerprint is the
        // trailing segment (a hex sha256, so it holds no `;targets=`).
        let base_fingerprint = marker.rsplit_once(";targets=").map(|(_, fingerprint)| fingerprint);
        Ok(base_fingerprint != Some(target_scope_fingerprint(branch_targets).as_str()))
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
