//! `IndexDatabase` meta accessors — the engine-handle surface over the db layer's
//! `rag_rat_db::meta` free functions.

use rag_rat_base::config::ResolvedTarget;
use rag_rat_base::hash::hex_sha256;
use rag_rat_db::meta::*;

use super::*;

/// A [`IndexDatabase::content_revision`] digest pinned to the connection state it was computed
/// under (#821), so a LATER stage of the same pass can reuse the digest instead of paying the
/// full `main.files` scan again — but only when nothing can have moved it in between. Consumed
/// through [`IndexDatabase::content_revision_reusing`], which enforces the reuse rule.
#[derive(Debug)]
pub(crate) struct ContentRevisionSnapshot {
    revision: String,
    /// `PRAGMA data_version` at capture — moves iff ANOTHER connection committed to this
    /// database since. The database is shared cross-process (and, consolidated, cross-repo:
    /// a sibling repo's writer moves the GLOBAL digest without touching this repo's rows).
    data_version: i64,
    /// SQL `total_changes()` at capture — moves iff THIS connection wrote any row since.
    total_changes: i64,
}

impl IndexDatabase {
    pub(super) fn record_content_revision(&self) -> anyhow::Result<String> {
        let revision = self.content_revision()?;
        self.record_content_revision_value(&revision)?;
        Ok(revision)
    }

    /// [`Self::record_content_revision`] with the digest already in hand (#821): `sync_fts`
    /// computes ONE `main.files` digest and stamps both `content_revision` and
    /// `fts_source_revision` from it instead of paying the full-table scan twice.
    pub(super) fn record_content_revision_value(&self, revision: &str) -> anyhow::Result<()> {
        // GLOBAL, not per-repo (V040 reclassification): `content_revision()` digests the WHOLE
        // `main.files` (no repo filter — see the method below), so its stored value is scope- and
        // repo-invariant. V039 relocated it to `repo_meta` under the one-DB-per-repo assumption;
        // per-repo copies would make a consolidated DB's FTS freshness alternate. `set_meta` writes
        // the global `index_meta`. (V040's `move_repo_meta_keys_to_global` migrates any stale
        // per-repo copy back; the shared relocate helper no longer re-relocates it.)
        self.set_meta("content_revision", revision)
    }

    /// The connection's `PRAGMA data_version` — differs between two reads on THIS connection iff
    /// ANOTHER connection committed to the database in between.
    pub(super) fn connection_data_version(&self) -> anyhow::Result<i64> {
        Ok(self.storage.connection().query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// The connection's SQL `total_changes()` — moves iff THIS connection wrote any row.
    fn connection_total_changes(&self) -> anyhow::Result<i64> {
        Ok(self.storage.connection().query_row("SELECT total_changes()", [], |row| row.get(0))?)
    }

    /// Pin `revision` — a digest [`Self::content_revision`] just computed on this connection —
    /// to the connection's current write state, for [`Self::content_revision_reusing`] (#821).
    ///
    /// `data_version_before_digest` is [`Self::connection_data_version`] captured BEFORE the
    /// digest was computed: it brackets the digest against the cross-connection TOCTOU. Without
    /// it, another connection committing between the digest read and this capture would bake the
    /// NEW `data_version` into a pin whose digest describes the OLD rows — a later no-write
    /// window would then validate the mismatched pin. When the bracket detects such a commit the
    /// pin is refused (`None`) and the consumer recomputes.
    pub(super) fn pin_content_revision(
        &self,
        revision: String,
        data_version_before_digest: i64,
    ) -> anyhow::Result<Option<ContentRevisionSnapshot>> {
        let data_version = self.connection_data_version()?;
        if data_version != data_version_before_digest {
            return Ok(None);
        }
        Ok(Some(ContentRevisionSnapshot {
            revision,
            data_version,
            total_changes: self.connection_total_changes()?,
        }))
    }

    /// The pinned digest when it is PROVABLY still current, else a fresh recompute (#821).
    ///
    /// REUSE RULE: a pinned digest describes `main.files` only while nothing has written to the
    /// database, and stages run between the pin and its consumer — in the watcher pass, the base
    /// reconcile runs between the clone quiet-gate probe and the clone delta. No stage report
    /// distinguishes "wrote `files` rows" (the reconcile's `"Current"` covers both a no-op and a
    /// completed embedding run), and other processes share the database file. So the gate is the
    /// connection's own counters: reuse ONLY when `PRAGMA data_version` (other connections) AND
    /// `total_changes()` (this connection) both still match the capture — ANY intervening write,
    /// `files` or not, forces the recompute. Conservative by design: a false negative costs one
    /// redundant digest; a false positive would let the consumer stamp or compare a digest that
    /// does not describe the rows it actually read.
    pub(super) fn content_revision_reusing(
        &self,
        pinned: Option<&ContentRevisionSnapshot>,
    ) -> anyhow::Result<String> {
        // Validation order matters: own-connection `total_changes()` FIRST, the cross-connection
        // `data_version` LAST — read last, it covers every sibling commit up to this moment, so
        // the validation itself has no internal race window. What remains is the gap between
        // this check and the consumer's own reads — the SAME gap the recompute path always had
        // (a digest is computed, then consumed outside a transaction) — and it is benign: the
        // consumer holds the per-repo write lock, so the rows it reads cannot move; only ANOTHER
        // repo's rows in a consolidated DB can, which lags the GLOBAL freshness stamp by one
        // pass (the next probe recomputes and the delta re-pins) and fails safe wherever the
        // stamp is compared for exact freshness (an older-than-content stamp disables the
        // postings fast path, never enables it).
        if let Some(pinned) = pinned
            && self.connection_total_changes()? == pinned.total_changes
            && self.connection_data_version()? == pinned.data_version
        {
            return Ok(pinned.revision.clone());
        }
        self.content_revision()
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
    /// ([`Self::overlay_target_config_reconcile`]). The base scope's discovery marker embeds its
    /// target fingerprint; when it equals the branch's, no file can re-language, so the
    /// O(base-files) scan is skipped — the common no-divergent-branch-config case that would
    /// otherwise re-scan every base file on every overlay refresh × worktree, undoing the #577
    /// event-scoping win. Conservatively returns `true` when the marker is absent (a match
    /// can't be proven).
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
