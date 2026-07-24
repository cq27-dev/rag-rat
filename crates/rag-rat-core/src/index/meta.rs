//! `IndexDatabase` meta accessors — the engine-handle surface over the db layer's
//! `rag_rat_db::meta` free functions.

use rag_rat_base::config::ResolvedTarget;
use rag_rat_base::hash::hex_sha256;
use rag_rat_db::meta::*;

use super::*;

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

    /// The content digest over EVERY indexed file row — an O(1) read of the incrementally
    /// maintained `content_digest_state` (#828), rendered `ms1-<64 hex>`. GLOBAL (no repo/scope
    /// filter): the FTS mirror (`chunk_fts`), the `content_revision` meta, and the clone-graph
    /// stamps are all global, so their freshness must track global content — reading a scoped view
    /// here once made `fts_source_revision` ALTERNATE between base/overlay digests and rebuild FTS
    /// every interleaved read (#219). The digest is a pure function of the current multiset
    /// `{(path, sha256) : main.files, kind != 'deleted'}`, so it is content-stable across rebuilds,
    /// generation flips, gc, and row-order/rowid churn — see the `rag_rat_db::content_digest`
    /// module doc for the multiset-hash invariant.
    ///
    /// Maintained by the `files_content_digest_*` triggers on every write, so this is a single-row
    /// SELECT. If the row is ABSENT (pathological — hand-deleted, or a future migration bug), fall
    /// back to the from-scratch scan fold, WARN, and do NOT write from this read path (read-only
    /// opens exist). A write-lock-holding context reseeds via
    /// [`Self::verify_content_digest_parity`].
    pub(super) fn content_revision(&self) -> anyhow::Result<String> {
        let stored: Option<String> = self
            .storage
            .connection()
            .query_row("SELECT state FROM content_digest_state WHERE id = 1", [], |row| row.get(0))
            .optional()?;
        match stored {
            Some(state) =>
                Ok(format!("{}{state}", rag_rat_db::content_digest::CONTENT_REVISION_PREFIX)),
            None => {
                tracing::warn!(
                    "content_digest_state row absent; falling back to a from-scratch \
                     content_revision scan (reseed via gc or `heal_index`)"
                );
                self.content_revision_from_scan()
            },
        }
    }

    /// The from-scratch, order-free content digest over the current non-deleted `main.files`,
    /// rendered `ms1-…` — using the SAME per-row hash the trigger fold and the migration seed use,
    /// so a recompute can never disagree with the trigger-maintained state. Shared by the read
    /// fallback above and the parity self-check. Read-only.
    pub(super) fn content_revision_from_scan(&self) -> anyhow::Result<String> {
        let (state, _rows_folded) = self.content_digest_from_scan()?;
        Ok(rag_rat_db::content_digest::render_revision(&state))
    }

    /// `(state, rows_folded)` recomputed from a full `main.files` scan — the raw form the parity
    /// self-check and reseed compare and write. Ordering is irrelevant (the fold is commutative).
    fn content_digest_from_scan(
        &self,
    ) -> anyhow::Result<(rag_rat_db::content_digest::DigestState, i64)> {
        let conn = self.storage.connection();
        let mut state = [0u64; 4];
        let mut rows_folded = 0i64;
        let mut stmt =
            conn.prepare("SELECT path, sha256 FROM main.files WHERE kind != 'deleted'")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let sha256: String = row.get(1)?;
            let hash = rag_rat_db::content_digest::content_row_hash(&path, &sha256);
            rag_rat_db::content_digest::fold_row(&mut state, &hash, true);
            rows_folded += 1;
        }
        Ok((state, rows_folded))
    }

    /// Belt-and-suspenders parity self-check (#828 §9.1): recompute the digest from a full scan and
    /// compare it (state + `rows_folded`) with the maintained `content_digest_state`. On a
    /// mismatch — which no KNOWN path can cause (the triggers are fail-closed), so it means a
    /// migration bug or corruption — `tracing::error!` both values and RESEED the state row in
    /// place. MUST run only under a write lock (gc cadence, `heal_index`, full-index finalize).
    /// Deliberately does NOT re-stamp `fts_source_revision`/clone stamps: a drift has unknown
    /// provenance, so letting the mismatch drive the normal freshness rebuilds is the safe
    /// direction. Also reseeds when the row is absent.
    pub(super) fn verify_content_digest_parity(&self) -> anyhow::Result<()> {
        let (state, rows_folded) = self.content_digest_from_scan()?;
        let expected_state = rag_rat_db::content_digest::encode_state(&state);
        let conn = self.storage.connection();
        let stored: Option<(String, i64)> = conn
            .query_row(
                "SELECT state, rows_folded FROM content_digest_state WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if stored.as_ref() == Some(&(expected_state.clone(), rows_folded)) {
            return Ok(());
        }
        tracing::error!(
            ?stored,
            expected_state,
            expected_rows_folded = rows_folded,
            "content_digest_state parity mismatch — reseeding from scan (a trigger/migration \
             regression drifted the incrementally maintained content_revision)"
        );
        conn.execute(
            "INSERT OR REPLACE INTO content_digest_state(id, state, rows_folded) VALUES (1, ?1, \
             ?2)",
            params![expected_state, rows_folded],
        )?;
        Ok(())
    }
}
