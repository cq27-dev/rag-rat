use super::*;

impl IndexDatabase {
    /// Index a linked worktree's branch/working-tree delta as overlay rows that shadow the base
    /// scope, and tombstone the files it removed (#219 stage 2). No-op (empty `worktree_id` in the
    /// report) when `linked_path` is not a valid linked sibling of `config.root`'s repo. Leaves the
    /// connection scope set to the overlay; callers re-`set_context` if they need another scope.
    ///
    /// The standalone shape ([`OverlayRefreshTail::STANDALONE`]): the repo-global logical-symbol
    /// rebuild runs inline, atomic with the overlay transaction, and no #577 basis is maintained.
    /// Callers refreshing SEVERAL worktrees in one pass use
    /// [`Self::index_worktree_overlay_with_tail`] instead, so the batch pays the rebuild once
    /// (#819).
    pub fn index_worktree_overlay<F>(
        &mut self,
        config: &Config,
        linked_path: &Path,
        progress: &mut F,
    ) -> anyhow::Result<WorktreeOverlayReport>
    where
        F: FnMut(IndexProgress),
    {
        self.index_worktree_overlay_with_tail(
            config,
            linked_path,
            OverlayRefreshTail::STANDALONE,
            progress,
        )
    }

    /// [`Self::index_worktree_overlay`] with caller-owned tail handling (#819/#824): the batch
    /// shape. `tail` decides whether the repo-global logical-symbol rebuild runs inline or is
    /// deferred to one [`Self::apply_pending_logical_rebuild`] per batch, and whether the
    /// worktree's #577 refresh basis is maintained inside this refresh's own transaction.
    pub fn index_worktree_overlay_with_tail<F>(
        &mut self,
        config: &Config,
        linked_path: &Path,
        tail: OverlayRefreshTail<'_>,
        progress: &mut F,
    ) -> anyhow::Result<WorktreeOverlayReport>
    where
        F: FnMut(IndexProgress),
    {
        // `source_root` is the LINKED checkout's equivalent of `config.root` — bytes are read from
        // there, not the raw `linked_path` (which may be a subdir of the checkout, e.g. `--worktree
        // .` from `/wt/src`, or the git dir from a hook) (#219 review).
        let Some(overlay) = resolve_overlay_scope(config, linked_path)? else {
            // Fell back to base → not a valid linked sibling; nothing to overlay. Still an
            // entry-point exit, so it settles a pending batch obligation like every other one.
            self.settle_pending_logical_rebuild_inline(tail.logical_rebuild)?;
            return Ok(WorktreeOverlayReport::default());
        };
        // Scope the connection to the overlay (base commit + linked worktree id) so context-
        // dependent steps (tombstones, FTS, edge resolution) operate in the linked scope.
        self.set_context(&overlay.base_sha, &overlay.worktree_id)?;

        let committed = self.resolve_committed_delta_source(&overlay, config)?;
        let mut delta = compute_linked_worktree_delta(config, &overlay, committed)?;
        let ResolvedOverlayScope { base_sha, worktree_id, source_root, .. } = overlay;
        // Fold in TARGET-IDENTITY drift: a branch config change that re-languages or drops a
        // byte-identical file is invisible to the content delta, but the overlay's (language, kind)
        // must still track the branch config, like discovery's staleness (#659 review). This also
        // covers the `index --paths <linked>/foo.rs` case for a clean re-languaged file without
        // threading the supplied paths — the scan sees every base-scope file. GATED on the branch
        // config's targets differing from the base's (fingerprint match → no file can re-language),
        // so the common no-divergent-config worktree does NOT pay an O(base-files) scan on every
        // overlay refresh (#577 event-scoping).
        if self.overlay_targets_may_drift(&config.targets)? {
            let (drift_readable, drift_tombstones) = self.overlay_target_config_reconcile(
                &base_sha,
                config,
                &source_root,
                &delta.shadowing_paths(),
            )?;
            delta.readable.extend(drift_readable);
            delta.tombstones.extend(drift_tombstones);
        }
        let scope = FileScope::worktree(worktree_id.clone());
        // ONE transaction around the whole overlay update — incremental file replacement, tombstone
        // writes, the prune, AND the global logical-symbol/package/edge/FTS refresh — mirroring the
        // incremental pass (`index_incremental_with_progress`). Without it a concurrent reader can
        // observe partially replaced overlay rows or the globally cleared `logical_symbols` table
        // mid-rebuild, and an error midway leaves the overlay half-applied. BEGIN IMMEDIATE
        // acquires the write lock up front so a racing writer waits out busy_timeout
        // instead of failing the deferred read→write upgrade with SQLITE_BUSY; ROLLBACK on
        // any error (#219 review).
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<(usize, usize, usize)> {
            // #826: arm scoped logical re-derive capture — the overlay's file removals / inserts
            // below stage the worktree's changed PATHS so `finalize_overlay_refresh`'s Inline case
            // can re-derive only those paths' logical groups instead of the whole repo. Armed (and
            // cleared) here — INSIDE the guarded closure so a temp-table create/clear failure rolls
            // the transaction back rather than stranding it open on this reusable `&mut self` (the
            // next refresh would then fail "cannot start a transaction within a transaction"). A
            // batch's next worktree starts fresh; the Deferred (batch) case ignores the staged
            // paths and settles via one whole-repo rebuild at the batch tail.
            self.begin_scoped_logical_rederive()?;
            let applied = self.index_explicit_paths_from_root(
                config,
                &source_root,
                &delta.readable,
                &scope,
                progress,
            )?;
            let indexed = applied.written;
            // Write a tombstone only when one isn't already present, so a re-run on a static
            // worktree writes nothing (idle-safety, like the readable sha-skip).
            let mut tombstoned = 0;
            for path in &delta.tombstones {
                if !self.overlay_tombstone_exists(path, &worktree_id)? {
                    self.write_tombstone_in_scope(path, &worktree_id)?;
                    tombstoned += 1;
                }
            }
            // Prune overlay rows that no longer differ from the base — but ONLY when the delta is
            // complete. A partial delta (the working-tree status read failed → `status_complete`
            // false) is missing untracked / working-tree-deleted paths, so pruning against its
            // `shadowing_paths` would delete valid overlay rows; skip the prune and let the
            // next complete pass reconcile (mirrors gc's empty-live-set guard) (#219 review).
            let pruned = if delta.status_complete {
                self.prune_overlay_rows_not_in_delta(&worktree_id, &delta.shadowing_paths())?
            } else {
                0
            };
            self.finalize_overlay_refresh(
                &source_root,
                &worktree_id,
                OverlayChangeCounts { indexed, tombstoned, pruned },
                delta.manifest_changed,
                tail.logical_rebuild,
                applied.logical,
            )?;
            // #824: the basis write rides the SAME transaction as the rows it proves current —
            // previously a separate autocommit per worktree per pass (an extra WAL-dirtying
            // commit each). Un-gated on the counts: a COMPLETE no-change refresh must still
            // record its basis (that skip proof is the whole point of #577).
            self.apply_overlay_basis_tail(&worktree_id, delta.status_complete, tail.basis)?;
            Ok((indexed, tombstoned, pruned))
        })();
        // #826: disarm scoped logical re-derive capture on BOTH Ok and Err paths — this pass's
        // `&mut self` outlives an error return, so the flag must not leak into the caller's next
        // use.
        self.finish_scoped_logical_rederive();
        let (indexed, tombstoned, pruned) = match result {
            Ok(counts) => {
                self.storage.execute_batch("COMMIT")?;
                counts
            },
            Err(err) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                return Err(err);
            },
        };
        // AFTER the commit (its own transaction): an Inline refresh whose delta was EMPTY
        // skipped the finalize above, so this is the exit that consumes a stale pending
        // obligation instead of returning past it (#819 review). A failed refresh skips it —
        // the obligation survives for the next pass, like the batch callers' error arms.
        self.settle_pending_logical_rebuild_inline(tail.logical_rebuild)?;

        Ok(WorktreeOverlayReport {
            worktree_id,
            indexed,
            tombstoned,
            pruned,
            status_complete: delta.status_complete,
        })
    }

    /// Index EXACTLY the supplied `paths` of a linked worktree as overlay rows — the PATH-SCOPED
    /// twin of [`Self::index_worktree_overlay`] (#679), so a linked-worktree `index --paths`
    /// (the edit hook) refreshes just those paths' overlay rows instead of the checkout's WHOLE
    /// base↔branch delta. Mirrors the base `IndexMode::Paths` exact-path semantics on the
    /// linked route: a single-file edit no longer pulls in unrelated in-flight changes
    /// elsewhere in the same worktree, and pays no full tree-diff / status walk.
    ///
    /// Each supplied path is categorized against the LINKED checkout + branch config: present +
    /// target-matching → readable (indexed with the branch identity; the identity-skip in
    /// [`Self::index_explicit_paths_from_root`] keeps an unchanged file write-free); absent, OR
    /// present but no longer targeted by the branch config, while the BASE scope still has a live
    /// row → tombstone (shadow that base row); nothing to shadow otherwise.
    ///
    /// Does NOT prune (a partial path set is not authoritative over the whole overlay — pruning
    /// would delete valid rows for the paths it didn't inspect) and reports `status_complete =
    /// false`, so the caller ([`crate::watch::reindex_paths`]) clears the overlay basis and the
    /// next full sweep reconciles anything else in the worktree. The supplied-manifest
    /// package-map refresh stays the caller's job (via `refresh_worktree_overlay_packages`),
    /// exactly as on the whole-delta route. No-op (empty `worktree_id`) when `linked_path` is
    /// not a valid linked sibling. Leaves the connection scoped to the overlay.
    ///
    /// `logical_rebuild` (#819): `Deferred` skips the repo-global logical-symbol rebuild and
    /// marks it pending, for callers batching several overlay refreshes — the batch then runs
    /// [`Self::apply_pending_logical_rebuild`] once. Basis maintenance stays caller-side here
    /// (this refresh is never complete, so there is never a pair to record).
    pub fn index_worktree_overlay_paths<F>(
        &mut self,
        config: &Config,
        linked_path: &Path,
        paths: &[PathBuf],
        logical_rebuild: OverlayLogicalRebuild,
        progress: &mut F,
    ) -> anyhow::Result<WorktreeOverlayReport>
    where
        F: FnMut(IndexProgress),
    {
        let Some(ResolvedOverlayScope { base_sha, worktree_id, source_root, .. }) =
            resolve_overlay_scope(config, linked_path)?
        else {
            // Not a valid linked sibling — still an entry-point exit (#819 review): settle a
            // pending batch obligation like the whole-delta route's no-op arm does.
            self.settle_pending_logical_rebuild_inline(logical_rebuild)?;
            return Ok(WorktreeOverlayReport::default());
        };
        self.set_context(&base_sha, &worktree_id)?;
        // Classify each supplied path with the SAME symlink-safe, ignore-aware guards the base
        // `IndexMode::Paths` walker applies (#659), since a supplied path may be arbitrary (a
        // crafted `..`-escape, a symlink-crossing spelling, or an ignored file) — reuse the
        // shared primitives (`lexically_normalized_within_root` / `resolves_within_root` /
        // `path_crosses_symlink`) rather than a naive `is_file()`, so this route can't
        // drift from the base one. `ignore` is the LINKED checkout's matcher (a branch
        // `.gitignore` governs the overlay's indexable set), recompiled per call so a
        // branch ignore edit takes effect immediately.
        let canonical_source = source_root.canonicalize().unwrap_or_else(|_| source_root.clone());
        let ignore =
            ignore_rules::IgnoreMatcher::compile(&source_root, &config.target_directories());
        // Present + indexable = a regular, in-root, non-symlink-crossed, NON-ignored file the
        // branch config targets — the base walker's exact set. A closure so the same check
        // RE-VALIDATES a removal inside the transaction (below), where the write lock is
        // held.
        let is_present_indexable = |rel: &Path, full: &Path| {
            resolves_within_root(full, &canonical_source)
                && !path_crosses_symlink(&source_root, rel)
                && full.is_file()
                && !ignore.is_ignored(full, false)
                && target_for_path(config, rel).is_some()
        };
        let mut readable = Vec::new();
        let mut tombstones = Vec::new();
        let mut removal_candidates = Vec::new();
        for path in paths {
            // Rebase to the config-root-relative key (the spelling every overlay row + target match
            // uses), retrying against the CANONICAL source root for a symlinked spelling; then
            // reject a `..`-escape. A path not under the source root is dropped
            // (defensive).
            let raw = match path.strip_prefix(&source_root) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => {
                    let Some(rel) = canonicalize_nearest_ancestor(path).and_then(|canonical| {
                        canonical.strip_prefix(&canonical_source).ok().map(Path::to_path_buf)
                    }) else {
                        continue;
                    };
                    rel
                },
            };
            let Some(rel) = lexically_normalized_within_root(&raw) else { continue };
            let full = source_root.join(&rel);
            if is_present_indexable(&rel, &full) {
                readable.push(rel);
            } else if self.base_scope_has_path(&base_sha, &rel)? {
                // Non-indexable (delete / ignored-now / de-targeted / symlink-replaced) but the
                // base still has a row → shadow it with a tombstone (mirrors the whole-delta
                // overlay). Carry `full` so the write RE-VALIDATES under the write lock, like
                // removals.
                tombstones.push((rel, full));
            } else {
                // Non-indexable AND no base row to shadow: a BRANCH-ONLY file. If it was overlay-
                // indexed, its stale row must be REMOVED (the whole-delta prune does this; a
                // path-scoped pass skips the prune). Deferred to the transaction, where existence +
                // non-indexability are RE-VALIDATED under the write lock (#679 review).
                removal_candidates.push((rel, full));
            }
        }
        let scope = FileScope::worktree(worktree_id.clone());
        // ONE transaction (see `index_worktree_overlay` for the rationale): index the readable set,
        // write tombstones, then the gated logical-symbol/edge/FTS refresh. BEGIN IMMEDIATE up
        // front; ROLLBACK on any error.
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<(usize, usize, usize)> {
            // #826: arm scoped logical re-derive capture — the overlay's file removals / inserts
            // below stage the worktree's changed PATHS so `finalize_overlay_refresh`'s Inline case
            // can re-derive only those paths' logical groups instead of the whole repo. Armed (and
            // cleared) here — INSIDE the guarded closure so a temp-table create/clear failure rolls
            // the transaction back rather than stranding it open on this reusable `&mut self` (the
            // next refresh would then fail "cannot start a transaction within a transaction"). A
            // batch's next worktree starts fresh; the Deferred (batch) case ignores the staged
            // paths and settles via one whole-repo rebuild at the batch tail.
            self.begin_scoped_logical_rederive()?;
            let applied = self.index_explicit_paths_from_root(
                config,
                &source_root,
                &readable,
                &scope,
                progress,
            )?;
            let indexed = applied.written;
            let mut tombstoned = 0;
            for (rel, full) in &tombstones {
                // RE-VALIDATE under the write lock (like removals): if the file was recreated /
                // became indexable after classification, do NOT write a tombstone that would hide
                // the now-valid content — `BEGIN IMMEDIATE` freezes DB writers, not
                // the filesystem. The next full sweep reconciles the recreated file
                // (#679 review).
                if !is_present_indexable(rel, full)
                    && !self.overlay_tombstone_exists(rel, &worktree_id)?
                {
                    self.write_tombstone_in_scope(rel, &worktree_id)?;
                    tombstoned += 1;
                }
            }
            // Targeted removal of stale branch-only overlay rows — the per-path equivalent of the
            // whole-delta prune this scoped pass skips. RE-VALIDATED here under the write lock
            // (BEGIN IMMEDIATE froze other writers): only remove a still-non-indexable path whose
            // overlay row still exists, so a concurrent heal that re-indexed the path (it
            // reappeared) between classification and now can't have its fresh row
            // deleted (#679 review).
            let mut pruned = 0;
            for (rel, full) in &removal_candidates {
                if !is_present_indexable(rel, full)
                    && self.overlay_source_row_exists(rel, &worktree_id)?
                {
                    self.remove_file_in_scope(rel, "", &worktree_id)?;
                    pruned += 1;
                }
            }
            // No global prune: a partial path set is not authoritative over the whole overlay. The
            // supplied-manifest package refresh is the caller's job, so `manifest_changed = false`.
            self.finalize_overlay_refresh(
                &source_root,
                &worktree_id,
                OverlayChangeCounts { indexed, tombstoned, pruned },
                false,
                logical_rebuild,
                applied.logical,
            )?;
            Ok((indexed, tombstoned, pruned))
        })();
        // #826: disarm scoped logical re-derive capture on BOTH Ok and Err paths — this pass's
        // `&mut self` outlives an error return, so the flag must not leak into the caller's next
        // use.
        self.finish_scoped_logical_rederive();
        let (indexed, tombstoned, pruned) = match result {
            Ok(counts) => {
                self.storage.execute_batch("COMMIT")?;
                counts
            },
            Err(err) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                return Err(err);
            },
        };
        // The Inline entry-point exit settle (#819 review), AFTER the commit: an unchanged
        // supplied path identity-skips (the finalize above never ran), and a stale pending
        // obligation must not survive this exit.
        self.settle_pending_logical_rebuild_inline(logical_rebuild)?;
        Ok(WorktreeOverlayReport {
            worktree_id,
            indexed,
            tombstoned,
            pruned,
            // A path-scoped pass never fully reconciles the overlay — signal incomplete so the
            // caller clears the basis and the next full sweep reconciles the rest
            // (#679).
            status_complete: false,
        })
    }

    /// Index an EXPLICIT set of repo-relative `paths`, reading bytes from `source_root` (which may
    /// differ from `config.root` — e.g. a sibling linked worktree) but keying every row by the
    /// repo-relative logical path, with `scope`. The reusable primitive under
    /// `index_worktree_overlay` — it knows nothing about worktree deltas, just "index these
    /// paths from this root into this scope", applying the same target include/exclude policy
    /// as discovery and reusing the per-file prepare/insert pipeline
    /// (`apply_incremental_file_plan`).
    pub(super) fn index_explicit_paths_from_root<F>(
        &self,
        config: &Config,
        source_root: &Path,
        paths: &[PathBuf],
        scope: &FileScope,
        progress: &mut F,
    ) -> anyhow::Result<incremental::IncrementalWriteOutcome>
    where
        F: FnMut(IndexProgress),
    {
        // Existing rows in this scope (path → identity) so an UNCHANGED file is skipped: re-running
        // the overlay on a static worktree then writes nothing, so the watcher can refresh overlays
        // every maintenance pass without churn — preserving the idle backstop (#63) and not
        // tripping the self-sustaining re-index loop. The identity is `(sha256, language, kind)`,
        // not sha alone: a branch config change that RE-LANGUAGES a byte-identical file
        // must still rewrite the overlay row, mirroring discovery / the base `Paths` flow's
        // staleness (#659).
        let existing = self.scope_file_identities(&scope.commit_sha, &scope.worktree_id)?;
        let mut files = Vec::new();
        for rel in paths {
            let full_path = source_root.join(rel);
            let Ok(bytes) = std::fs::read(&full_path) else {
                continue; // not a readable regular file
            };
            let Some((language, kind)) = target_for_path(config, rel) else {
                continue;
            };
            if existing.get(path_string(rel).as_str())
                == Some(&(
                    hex_sha256(&bytes),
                    language.as_str().to_string(),
                    kind.as_str().to_string(),
                ))
            {
                continue; // unchanged since the last overlay index (content AND target identity)
            }
            files.push(IndexFile {
                full_path,
                relative_path: rel.clone(),
                language,
                kind,
                commit_sha: scope.commit_sha.clone(),
                worktree_id: scope.worktree_id.clone(),
            });
        }
        self.apply_incremental_file_plan(files, BTreeSet::new(), progress)
    }
}
