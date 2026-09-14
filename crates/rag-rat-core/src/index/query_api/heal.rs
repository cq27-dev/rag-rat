//! Lazy source healing for query results.

use rag_rat_base::hash::hex_sha256;
use rag_rat_base::paths::path_string;

use super::*;

impl IndexDatabase {
    /// The indexed `files.sha256` for `path` in the ACTIVE checkout (via the per-connection `files`
    /// scope view), or `None` when the path isn't indexed in this scope.
    fn indexed_sha_for_path(&self, path: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .storage
            .connection()
            .query_row("SELECT sha256 FROM files WHERE path = ?1 LIMIT 1", [path], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }

    /// Of `paths`, those whose on-disk content differs from the indexed sha (or are unreadable) —
    /// results drawn from them may be stale relative to the working tree. Deduped; paths not
    /// indexed in scope are skipped (nothing to be stale against); no source root → empty
    /// (bare/copied index). One file read + hash per distinct path — callers pass the small
    /// result set, not the whole corpus.
    pub(super) fn stale_source_paths(&self, paths: &[String]) -> anyhow::Result<Vec<String>> {
        // Under a LINKED-WORKTREE OVERLAY scope, `source_root` is the MAIN checkout — NOT the
        // branch these rows came from. Hashing the overlay rows against main's copy reports every
        // branch-changed file as stale even though the overlay is current, which makes
        // symbol_lookup's matched-file heal trip `NeedsReindex` (and `heal_file` no-ops under the
        // overlay anyway) and impact's `stale_files` caveat lie. The overlay rows are authoritative
        // (maintained by `index_worktree_overlay`), so report nothing stale — same rationale as the
        // read_chunk overlay skip (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(Vec::new());
        }
        let Some(root) = self.storage.source_root().map(Path::to_path_buf) else {
            return Ok(Vec::new());
        };
        let mut stale = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for path in paths {
            if !seen.insert(path.as_str()) {
                continue;
            }
            let Some(indexed) = self.indexed_sha_for_path(path)? else {
                continue;
            };
            match fs::read(root.join(path)) {
                Ok(bytes) if hex_sha256(&bytes) == indexed => {},
                _ => stale.push(path.clone()),
            }
        }
        Ok(stale)
    }

    /// Reindex stale files inline so the next read sees current positions (#147), mirroring
    /// `search_with_heal`: bounded by `MAX_AUTO_HEAL_FILES_PER_CALL` (raises `NeedsReindex` beyond
    /// it so a huge dirty set can't turn a read into an unbounded rebuild), then sync FTS.
    pub(super) fn heal_stale_paths(&self, stale: &[String]) -> anyhow::Result<()> {
        if stale.is_empty() {
            return Ok(());
        }
        if stale.len() > MAX_AUTO_HEAL_FILES_PER_CALL {
            anyhow::bail!(IndexError::NeedsReindex {
                stale_files: stale.len(),
                cap: MAX_AUTO_HEAL_FILES_PER_CALL,
            });
        }
        for path in stale {
            self.heal_file(Path::new(path))?;
        }
        self.sync_fts()?;
        Ok(())
    }

    /// #152: a name lookup that found NOTHING may be a symbol just added (or renamed into
    /// existence) the watcher hasn't indexed yet. Index the working-tree change set — changed
    /// source files that are dirty-vs-index OR not-yet-indexed — bounded by
    /// `MAX_AUTO_HEAL_FILES_PER_CALL` (over the cap → do nothing; never raise `NeedsReindex`, since
    /// the common zero-hit is a plain typo'd miss that must stay cheap), then re-derive logical
    /// symbols + edges so the re-resolve sees the new symbol. Returns whether anything was indexed.
    /// No-op without a stored `Config` (rebuild/open/tests) or a git root — a genuine miss on a
    /// clean tree costs at most one `git status` and no write.
    pub(super) fn heal_changed_for_zero_hit(&self) -> anyhow::Result<bool> {
        // Under a LINKED-WORKTREE OVERLAY scope this would scan `config.root` (the MAIN checkout)
        // for working-tree changes and index them into the overlay scope — main's edits, not the
        // branch's. The overlay is maintained by `index_worktree_overlay`; leave the heal to it
        // (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(false);
        }
        let Some(config) = self.config.as_ref() else {
            return Ok(false);
        };
        let Ok(changes) = crate::index::git_changed_paths(&config.root) else {
            return Ok(false);
        };
        if changes.changed.is_empty() {
            return Ok(false);
        }
        // Classify changed paths against the indexed targets (include/exclude/language), keeping
        // only those dirty-vs-index OR not-yet-indexed — a file already current adds nothing.
        let mut healable = Vec::new();
        for file in collect_changed_index_files(config, &changes)? {
            let needs_index = match self.indexed_sha_for_path(&path_string(&file.relative_path))? {
                None => true,
                Some(indexed) => match fs::read(&file.full_path) {
                    Ok(bytes) => hex_sha256(&bytes) != indexed,
                    Err(_) => false,
                },
            };
            if needs_index {
                healable.push(file);
            }
            if healable.len() > MAX_AUTO_HEAL_FILES_PER_CALL {
                // Too many newly-changed files to heal inline on a lookup miss — leave it to the
                // watcher rather than turn a read into a large rebuild.
                return Ok(false);
            }
        }
        if healable.is_empty() {
            return Ok(false);
        }
        let files = self.assign_file_scopes(healable, &changes);
        // Apply with the SAME write discipline as the incremental indexer
        // (`index_incremental_with_progress`), not a corner-cut version (PR #158 review):
        //  - one `BEGIN IMMEDIATE` txn so a concurrent reader never observes the index between the
        //    logical-symbol DELETE-all and its rebuild (which would return empty logical handles);
        //  - apply `changes.deleted` so a removed file's stale rows don't survive the re-resolve
        //    and let the new file's edges bind to a deleted definition;
        //  - `refresh_packages` BEFORE `resolve_edges`, since per-package import scope (#61) is
        //    read at resolve time — a change set that adds/edits a Cargo.toml must resolve against
        //    the fresh package map, not the stale one.
        // On the read-only MCP connection the BEGIN trips SQLITE_READONLY and the dispatch retries
        // read-write (like the #147 heal), so the txn always runs on a writable connection.
        self.storage.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> anyhow::Result<()> {
            self.apply_incremental_file_plan(files, changes.deleted.clone(), &mut |_| {})?;
            self.refresh_packages(&config.root)?;
            // Defer: this heal re-parsed only the STALE files, so it must not stamp the
            // logical-key version — untouched files' drift is still in the future (#493).
            self.rebuild_logical_symbols(graph_index::KeyVersionStamp::Defer)?;
            self.resolve_edges()?;
            self.sync_fts()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.storage.execute_batch("COMMIT")?;
                Ok(true)
            },
            Err(error) => {
                let _ = self.storage.execute_batch("ROLLBACK");
                Err(error)
            },
        }
    }

    pub fn heal_index(&self, limit: Option<u32>) -> anyhow::Result<HealIndexReport> {
        // `heal_index` reads file bytes from `source_root` (the MAIN checkout) and would write into
        // the active scope. Under a linked-worktree overlay scope that reindexes the overlay with
        // MAIN's contents or tombstones branch-only files, so refuse — the overlay is owned by
        // `index_worktree_overlay`. Callers scope writes to the base (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(HealIndexReport {
                checked_files: 0,
                healed_files: 0,
                removed_files: 0,
                skipped_files: 0,
                fts_fresh: !self.fts_dirty()?,
                fts_healed: Vec::new(),
                fts_deferred: Vec::new(),
                message: Some(
                    "skipped: heal does not run under a linked-worktree overlay scope".to_string(),
                ),
            });
        }
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("heal_index requires source_root metadata; run `rag-rat index` first");
        };
        // #767 review: fail closed when the active repo was `rag-rat rm`-removed after this
        // connection resolved its scope (a stale MCP `heal_index` writer). This is the PREFLIGHT
        // that stops the batch before any per-file work; the AUTHORITATIVE checks live at each
        // per-file write boundary — `heal_file` and `mark_file_deleted_if_not_removed` re-check
        // the tombstone inside their IMMEDIATE mutation transactions, which serialize with rm's
        // purge on the SQLite write lock (the heal path deliberately stays flock-free so it can
        // run alongside a mid-flight rebuild).
        self.assert_active_repo_not_removed()?;
        let indexed_files = self.indexed_files()?;
        let max_repairs = limit.map(usize::try_from).transpose()?.unwrap_or(usize::MAX);
        let mut report = HealIndexReport {
            checked_files: 0,
            healed_files: 0,
            removed_files: 0,
            skipped_files: 0,
            fts_fresh: false,
            fts_healed: Vec::new(),
            fts_deferred: Vec::new(),
            message: None,
        };

        for file in indexed_files {
            report.checked_files += 1;
            let path = Path::new(&file.path);
            let full_path = root.join(path);
            let Ok(text) = fs::read_to_string(&full_path) else {
                if usize::try_from(report.healed_files + report.removed_files).unwrap_or(usize::MAX)
                    >= max_repairs
                {
                    report.message =
                        Some("limit reached; rerun heal_index to continue".to_string());
                    break;
                }
                self.mark_file_deleted_if_not_removed(path)?;
                report.removed_files += 1;
                continue;
            };
            let sha256 = hex_sha256(text.as_bytes());
            if sha256 == file.sha256 {
                report.skipped_files += 1;
                continue;
            }
            if usize::try_from(report.healed_files + report.removed_files).unwrap_or(usize::MAX)
                >= max_repairs
            {
                report.message = Some("limit reached; rerun heal_index to continue".to_string());
                break;
            }
            self.heal_file(path)?;
            report.healed_files += 1;
        }

        // #828 §9.1: heal is the explicit operator remedy — verify the content-digest parity and
        // reseed `content_digest_state` on drift BEFORE the FTS freshness pass below, so
        // `ensure_fts_fresh` compares against a healed digest rather than a drifted one.
        self.verify_content_digest_parity()?;

        // Probe for FTS shadow corruption BEFORE the freshness pass (#582): a dirty-flagged
        // index would otherwise be incidentally repaired by `ensure_fts_fresh`'s rebuild and the
        // report would under-attribute what was actually corrupt.
        let fts_outcome = self.heal_fts_if_corrupt()?;
        report.fts_healed = fts_outcome.healed;
        report.fts_deferred = fts_outcome.deferred;
        if report.healed_files > 0 || report.removed_files > 0 {
            self.sync_fts()?;
        } else {
            self.ensure_fts_fresh()?;
        }
        // A deferred corrupt mirror is NOT fresh, whatever the dirty flag says — operators key
        // health off this field.
        report.fts_fresh = !self.fts_dirty()? && report.fts_deferred.is_empty();
        Ok(report)
    }
}
