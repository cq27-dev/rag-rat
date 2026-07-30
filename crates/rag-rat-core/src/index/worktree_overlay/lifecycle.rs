use super::*;

impl IndexDatabase {
    /// The parsed refresh-basis value for `worktree_id`, or `None` when never refreshed (or
    /// written by a pre-#577 build). The single parse under both projection readers below.
    fn read_worktree_overlay_basis(
        &self,
        worktree_id: &str,
    ) -> anyhow::Result<Option<RecordedOverlayBasis>> {
        let key = format!("{WORKTREE_OVERLAY_BASIS_META_PREFIX}{worktree_id}");
        Ok(self.repo_meta(&key)?.and_then(|value| {
            let mut lines = value.splitn(3, '\n');
            let base_sha = lines.next()?.to_string();
            let linked_head_sha = lines.next()?.to_string();
            // Absent on a pre-#822 two-line value; an unparsable third line degrades the same
            // way (no quiet skip) rather than invalidating the still-meaningful pair.
            let refreshed_at_ms = lines.next().and_then(|at_ms| at_ms.parse().ok());
            Some(RecordedOverlayBasis { base_sha, linked_head_sha, refreshed_at_ms })
        }))
    }

    /// The recorded refresh basis for `worktree_id`: `(base_sha, linked_head_sha)` at the last
    /// COMPLETE overlay refresh, or `None` when never refreshed (or written by a pre-#577
    /// build) — the caller then refreshes unconditionally.
    pub(crate) fn worktree_overlay_basis(
        &self,
        worktree_id: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        Ok(self
            .read_worktree_overlay_basis(worktree_id)?
            .map(|basis| (basis.base_sha, basis.linked_head_sha)))
    }

    /// When `worktree_id`'s last COMPLETE refresh recorded its basis (epoch ms) — the #822
    /// quiet-window anchor — or `None` when no basis is recorded or it predates the timestamp
    /// (either way the quiet window never holds; the refresh side of that coin is always safe).
    pub(crate) fn worktree_overlay_basis_refreshed_at_ms(
        &self,
        worktree_id: &str,
    ) -> anyhow::Result<Option<i64>> {
        Ok(self.read_worktree_overlay_basis(worktree_id)?.and_then(|basis| basis.refreshed_at_ms))
    }

    /// Upsert the refresh basis after a COMPLETE overlay refresh, stamped with `refreshed_at_ms`
    /// (injected by the caller — the quiet-window anchor, #822). The timestamp advances on every
    /// complete refresh, so unlike the pre-#822 pair-only value this writes one `repo_meta` row
    /// per recording refresh — bounded by the #822 gate itself (a scoped pass inside the window
    /// skips the whole refresh, and with it this write) and negligible next to the tree diff /
    /// status walk the recording refresh just paid.
    pub(crate) fn record_worktree_overlay_basis(
        &self,
        worktree_id: &str,
        base_sha: &str,
        linked_head_sha: &str,
        refreshed_at_ms: i64,
    ) -> anyhow::Result<()> {
        let key = format!("{WORKTREE_OVERLAY_BASIS_META_PREFIX}{worktree_id}");
        self.set_repo_meta_if_changed(
            &key,
            &format!("{base_sha}\n{linked_head_sha}\n{refreshed_at_ms}"),
        )?;
        Ok(())
    }

    /// Drop the refresh-basis keys of worktrees outside `live_worktrees` (gc, alongside the
    /// overlay-row prune) so a removed checkout's marker doesn't accumulate forever. The rows are
    /// one-per-worktree, so the filtering runs in Rust over the prefix-selected keys.
    pub(crate) fn prune_worktree_overlay_basis_outside(
        &self,
        live_worktrees: &[String],
    ) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let mut stmt = conn
            .prepare("SELECT key FROM repo_meta WHERE repo_id = ?1 AND key LIKE ?2 ESCAPE '\\'")?;
        // LIKE-escape the prefix's wildcard characters (it carries literal underscores).
        let escaped = WORKTREE_OVERLAY_BASIS_META_PREFIX
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let keys = stmt
            .query_map(params![self.active_repo_id, pattern], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for key in keys {
            let Some(worktree_id) = key.strip_prefix(WORKTREE_OVERLAY_BASIS_META_PREFIX) else {
                continue;
            };
            if !live_worktrees.iter().any(|live| live == worktree_id) {
                rag_rat_db::meta::delete_repo_meta(conn, &self.active_repo_id, &key)?;
            }
        }
        Ok(())
    }

    /// Drop `worktree_id`'s refresh basis. The watcher calls this when a refresh FAILED or was
    /// PARTIAL: neither HEAD may have moved (a dirty edit doesn't), so a previously recorded
    /// basis would keep matching and scoped passes would skip the stale overlay until an `All`
    /// pass (#577 review).
    pub(crate) fn clear_worktree_overlay_basis(&self, worktree_id: &str) -> anyhow::Result<()> {
        let key = format!("{WORKTREE_OVERLAY_BASIS_META_PREFIX}{worktree_id}");
        rag_rat_db::meta::delete_repo_meta(self.storage.connection(), &self.active_repo_id, &key)?;
        Ok(())
    }

    /// Refresh JUST the linked overlay's package/import scope (and re-resolve its edges against the
    /// new map) — the `index --paths <linked>/Cargo.toml` entry point when the supplied manifest is
    /// CLEAN/committed. `index_worktree_overlay` derives its manifest signal from the WORKING-TREE
    /// STATUS (dirty-only, for idle-safety), so a supplied but clean manifest produces no indexed
    /// rows and would leave the package map stale — this honors the base `Paths` flow's
    /// supplied-manifest signal for the linked route (#659 review). Its own `BEGIN IMMEDIATE` (the
    /// caller is not mid-transaction) and idempotent, so re-running it after a dirty-status refresh
    /// is harmless. No-op when `linked_path` is not a valid linked sibling. Leaves the connection
    /// scoped to the overlay.
    pub fn refresh_worktree_overlay_packages(
        &mut self,
        config: &Config,
        linked_path: &Path,
    ) -> anyhow::Result<()> {
        let Some(ResolvedOverlayScope { base_sha, worktree_id, source_root, .. }) =
            resolve_overlay_scope(config, linked_path)?
        else {
            return Ok(());
        };
        self.set_context(&base_sha, &worktree_id)?;
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<()> {
            self.refresh_packages(&source_root)?;
            self.resolve_overlay_edges(&worktree_id)?;
            self.bump_lens_enrichment_revision()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.storage.execute_batch("COMMIT")?;
                Ok(())
            },
            Err(err) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                Err(err)
            },
        }
    }

    /// The post-write finalize tail of `index_worktree_overlay`, run INSIDE its transaction — ONLY
    /// when something changed, so an unchanged worktree refresh is a true no-op (the watcher
    /// refreshes overlays every pass; this keeps idle passes write-free and clear of the
    /// self-sustaining re-index loop):
    /// - rebuild_logical_symbols: symbol_lookup / graph nav resolve through `logical_symbols`, so a
    ///   NEWLY-ADDED overlay file's symbols are invisible until regrouped (a modified file's
    ///   unchanged symbols resolve via the base's logical rows — which is why only added files were
    ///   missing). This is the field-reported bug. `Deferred` (#819) replaces the rebuild with a
    ///   persisted pending marker in this same transaction; the batch caller runs ONE rebuild for
    ///   all its worktrees via `apply_pending_logical_rebuild`. When EVERY indexed change kept its
    ///   file's logical-key multiset (#820 — the body-only-edit refresh), neither runs: the grouped
    ///   table is already what a rebuild would produce, so the surviving logical rows are
    ///   re-pointed at the replacement symbol ids instead, and no obligation is created. A
    ///   tombstone or prune always changes the key set, so those refreshes keep the rebuild/marker.
    /// - refresh_packages: write the overlay scope's `packages` rows from the LINKED checkout's
    ///   manifests BEFORE resolving, so the per-package import scope (#61) is correct for the
    ///   branch. The resolver's `load_package_roots_into_scope` reads `packages` at the active
    ///   `(base_sha, linked_worktree_id)` scope — the base rows live at `(base_sha, '')` and are
    ///   invisible to it — so without this the overlay resolves imports against an empty package
    ///   map (every file falls open to the global union → external-dep suppression downgraded /
    ///   wrong for a branch with a new or changed Cargo.toml or path-dep alias) (#219 review).
    /// - resolve_overlay_edges: the overlay inserted edges unresolved. Uses the OVERLAY-SCOPED
    ///   resolver (NOT the base `resolve_edges`): resolution targets span the full overlay view,
    ///   but only the worktree's OWN overlay source rows are re-resolved — a shared committed
    ///   (base) caller's `edges_data`, visible in the overlay view but owned by the base scope,
    ///   must not be rewritten or base `find_callers`/impact would corrupt until a base pass
    ///   resolved it back, flipping by whichever worktree refreshed last (#219 P1).
    /// - sync_fts: so semantic_search (BM25) sees the overlay's chunks.
    ///
    /// A manifest-only change (`manifest_changed`, no source rows) refreshes JUST the package scope
    /// — the logical-symbol/edge/FTS steps depend on source-row changes, so they stay gated on the
    /// counts (#659 review).
    pub(super) fn finalize_overlay_refresh(
        &self,
        source_root: &Path,
        worktree_id: &str,
        counts: OverlayChangeCounts,
        manifest_changed: bool,
        logical_rebuild: OverlayLogicalRebuild,
        mut grouping: graph_index::LogicalGroupingUpkeep,
    ) -> anyhow::Result<()> {
        if counts.any_changed() {
            // #820: a tombstone or prune removes a whole file's keys from the grouped corpus —
            // never key-stable, whatever the indexed half of the delta looked like.
            if counts.tombstoned > 0 || counts.pruned > 0 {
                grouping.require_rebuild();
            }
            match grouping {
                graph_index::LogicalGroupingUpkeep::RelinkMembers(relinks) => {
                    // Every indexed change was a key-stable replacement: re-link the members
                    // inside this transaction and create NO rebuild obligation. A PRE-EXISTING
                    // #819 marker is deliberately left in place — a relink is not a rebuild,
                    // only `rebuild_logical_symbols` may clear the marker, and the entry-point
                    // / batch-tail settles still consume it.
                    self.apply_logical_member_relinks(&relinks)?;
                },
                graph_index::LogicalGroupingUpkeep::RebuildRequired => match logical_rebuild {
                    OverlayLogicalRebuild::Inline => {
                        // #826: re-derive ONLY this worktree's changed paths' logical groups
                        // (staged into `temp.logical_rederive_paths` by the
                        // overlay apply above) instead of the whole repo —
                        // UNLESS a #493 drift heal or a #819 deferred rebuild is
                        // owed, which the scoped path cannot serve. The re-derive reads raw
                        // `main.files`, so under this overlay scope view it still regroups each
                        // changed path across ALL its scopes (base + every worktree). Defer the
                        // STAMP regardless: an overlay refresh re-parsed
                        // only the worktree's own files, so it must not
                        // stamp the logical-key version — the base scope's drift is still
                        // in the future (#493).
                        if self.can_scope_logical_rederive()? {
                            self.rederive_changed_logical_symbols()?;
                        } else {
                            self.rebuild_logical_symbols(graph_index::KeyVersionStamp::Defer)?;
                        }
                    },
                    OverlayLogicalRebuild::Deferred => {
                        // Mark the repo-global rebuild pending IN THIS transaction (#819).
                        // Committed overlay rows without a follow-up rebuild would leave a newly
                        // added file's symbols unresolvable, and a later pass would idle-skip the
                        // then-unchanged rows — the persisted marker survives a crash between this
                        // commit and the batch tail, so `apply_pending_logical_rebuild` still
                        // runs. `if_changed`: the second changed worktree of a batch finds it
                        // already set.
                        self.set_repo_meta_if_changed(OVERLAY_LOGICAL_REBUILD_PENDING_META, "1")?;
                    },
                },
            }
            self.refresh_packages(source_root)?;
            self.resolve_overlay_edges(worktree_id)?;
            self.sync_fts()?;
        } else if manifest_changed {
            // A dirty `Cargo.toml` with no source-row change: the base flow's manifest signal
            // refreshes the package map even with zero indexed files, and the overlay must match so
            // the branch resolves imports against its own manifest. `manifest_changed` is
            // status-derived (self-clears on commit), so this does not rewrite `packages` on every
            // idle overlay pass (#659 review).
            self.refresh_packages(source_root)?;
            // Overlay EDGES resolve THROUGH the package/import scope, so a package-map change must
            // re-resolve them too — otherwise callers/callees keep returning targets resolved
            // against the OLD manifest until an unrelated source change triggers a resolve (#659
            // review).
            self.resolve_overlay_edges(worktree_id)?;
        }
        if counts.any_changed() || manifest_changed {
            self.bump_lens_enrichment_revision()?;
        }
        Ok(())
    }

    fn bump_lens_enrichment_revision(&self) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1",
            params![self.active_repo_id, rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META],
        )?;
        Ok(())
    }

    /// Apply the #577 basis leg of an overlay refresh's tail, INSIDE the refresh transaction
    /// (#824): record the caller's pair on a COMPLETE refresh; clear the worktree's recorded
    /// basis on a PARTIAL one (a dirty edit moves no HEAD, so a stale pair would keep matching
    /// and scoped passes would skip the stale overlay until an `All` sweep — #577 review); no-op
    /// when the caller maintains no basis. FAILED refreshes never reach this: the transaction
    /// rolls back, and the caller clears the basis outside it.
    pub(in crate::index) fn apply_overlay_basis_tail(
        &self,
        worktree_id: &str,
        status_complete: bool,
        basis: Option<OverlayBasisUpdate<'_>>,
    ) -> anyhow::Result<()> {
        let Some(basis) = basis else { return Ok(()) };
        if status_complete {
            // The quiet-window anchor (#822) is stamped here, at the one seam every recording
            // refresh passes through; `now_ms` is the codebase's single sanctioned wall-clock
            // read. Riding the same value as the pair means clear-on-partial (below) and the
            // caller-side clear-on-failure drop the timestamp with it — a cleared basis can
            // never leave a stale quiet-skip behind.
            self.record_worktree_overlay_basis(
                worktree_id,
                basis.base_sha,
                basis.linked_head_sha,
                rag_rat_base::time::now_ms(),
            )
        } else {
            self.clear_worktree_overlay_basis(worktree_id)
        }
    }

    /// Run the batch-deferred repo-global logical-symbol rebuild if one is pending (#819) — the
    /// REQUIRED tail of any batch of [`OverlayLogicalRebuild::Deferred`] refreshes, in its own
    /// `BEGIN IMMEDIATE` (the caller must not hold an open transaction). One rebuild serves the
    /// whole batch: `logical_symbols` is repo-scoped but scope-independent, so per-worktree
    /// rebuilds are redundant — with K changed worktrees only the last one's output survives.
    /// This is also the mid-batch-error / crash backstop: the pending marker committed with each
    /// worktree's rows, so earlier worktrees' committed refreshes get their rebuild even when a
    /// later worktree failed, or when the process died between the overlay transactions and this
    /// tail (the next pass finds the marker and heals). Beyond the batch callers, EVERY indexing
    /// entry point runs this before returning — the Inline overlay exits (via
    /// [`Self::settle_pending_logical_rebuild_inline`]) and the base incremental writer's tail —
    /// so no entry point exits past a committed obligation just because its own delta was empty.
    /// Returns whether a rebuild ran; `Ok(false)` = nothing pending, write-free (the #63 idle
    /// backstop).
    pub fn apply_pending_logical_rebuild(&self) -> anyhow::Result<bool> {
        // Lockless fast path: the common idle pass never opens a write transaction at all.
        if self.repo_meta(OVERLAY_LOGICAL_REBUILD_PENDING_META)?.is_none() {
            return Ok(false);
        }
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<bool> {
            // RE-CHECK under the write transaction: a concurrent tail (another watcher/hook
            // process) may have rebuilt and cleared the marker between the read above and
            // BEGIN IMMEDIATE — proceeding blind would pay a second wholesale rebuild for
            // nothing.
            if self.repo_meta(OVERLAY_LOGICAL_REBUILD_PENDING_META)?.is_none() {
                return Ok(false);
            }
            // Defer the #493 stamp for the same reason each deferred refresh would have: the
            // batch re-parsed only worktree deltas, never the full corpus. The rebuild clears
            // the pending marker itself, in this same transaction — on failure both roll back,
            // so the obligation survives for the next pass to retry.
            self.rebuild_logical_symbols(graph_index::KeyVersionStamp::Defer)?;
            Ok(true)
        })();
        match result {
            Ok(ran) => {
                self.storage.execute_batch("COMMIT")?;
                Ok(ran)
            },
            Err(err) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                Err(err)
            },
        }
    }

    /// The Inline entry-point exit settle (#819 review): every overlay indexing entry point
    /// consumes a pending batch obligation before returning — even when its OWN delta was
    /// empty. An interrupted [`OverlayLogicalRebuild::Deferred`] batch leaves the marker
    /// committed, and an Inline refresh with no row changes skips `finalize_overlay_refresh`
    /// (and with it the inline rebuild, the marker's sole clearer) entirely — without this,
    /// `index --worktree` over an unchanged checkout would exit leaving BOTH the marker and the
    /// stale `logical_symbols` in place, branch-only symbols unresolvable until an unrelated
    /// pass happened to rebuild. Inline-only: `Deferred` callers own their batch tail
    /// ([`Self::apply_pending_logical_rebuild`] after their loop) — settling per worktree here
    /// would undo the #819 batching. Runs OUTSIDE the refresh transaction (its own
    /// `BEGIN IMMEDIATE`); write-free when nothing is pending (one meta read).
    pub(super) fn settle_pending_logical_rebuild_inline(
        &self,
        logical_rebuild: OverlayLogicalRebuild,
    ) -> anyhow::Result<()> {
        if logical_rebuild == OverlayLogicalRebuild::Inline {
            self.apply_pending_logical_rebuild()?;
        }
        Ok(())
    }

    /// Remove overlay rows of `worktree_id` whose path is no longer in the delta (the file matches
    /// the base again), so the scope view falls back to the base row for them. Returns the count.
    pub(super) fn prune_overlay_rows_not_in_delta(
        &self,
        worktree_id: &str,
        shadowing: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<Vec<PathBuf>> {
        let existing: Vec<String> = {
            let conn = self.storage.connection();
            // Direct `main.files` probe → explicit `repo_id` predicate (A3), same class as the
            // heal probes in `incremental.rs`.
            let mut stmt = conn.prepare(
                "SELECT path FROM main.files
                 WHERE repo_id = ?1 AND worktree_id = ?2 AND worktree_id != ''",
            )?;
            let rows = stmt.query_map(params![self.active_repo_id, worktree_id], |row| {
                row.get::<_, String>(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        // The pruned PATHS, not just a count: dropping an overlay row un-shadows the base version
        // for this checkout, so the path's effective content changed and a per-checkout consumer
        // must be told about it (#1010).
        let mut pruned = Vec::new();
        for path in existing {
            if !shadowing.contains(Path::new(&path)) {
                self.remove_file_in_scope(Path::new(&path), "", worktree_id)?;
                pruned.push(PathBuf::from(path));
            }
        }
        Ok(pruned)
    }
}
