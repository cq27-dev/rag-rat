//! File-row reads and scope mutations: fetch the file row, mark/remove files in the active scope,
//! and count indexed files.

use rag_rat_base::paths::path_string;

use super::*;

impl IndexDatabase {
    pub(super) fn mark_file_deleted(&self, path: &Path) -> anyhow::Result<()> {
        self.write_tombstone_in_scope(path, &self.active_worktree_id)
    }

    /// Write a `kind='deleted'` overlay tombstone for `path` in an EXPLICIT `worktree_id` scope
    /// (not necessarily the active one). The scope view excludes such a row from the overlay
    /// branch AND (because the committed branch's `path NOT IN (overlay paths)` subquery still
    /// counts it) suppresses the base committed row — so the path is HIDDEN rather than falling
    /// through to the base. That is exactly what a linked worktree's branch-deleted file needs
    /// (#219); `mark_file_deleted` is the active-scope special case.
    pub(super) fn write_tombstone_in_scope(
        &self,
        path: &Path,
        worktree_id: &str,
    ) -> anyhow::Result<()> {
        let path = path_string(path);
        self.remove_file_in_scope(Path::new(&path), "", worktree_id)?;
        self.storage.connection().execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
             indexed_at_ms, indexed_revision, commit_sha, worktree_id, repo_id, generation)
             VALUES (?1, 'unknown', 'deleted', '', 0, 0, ?2, '', '', ?3, ?4, ?5)
             ON CONFLICT(repo_id, path, commit_sha, worktree_id, generation) DO UPDATE SET
                kind = 'deleted',
                sha256 = '',
                modified_at_ms = 0,
                indexed_at_ms = excluded.indexed_at_ms",
            // A6: the tombstone lands on the connection's live generation, and the ON CONFLICT
            // target matches the V043 UNIQUE (repo_id, path, commit_sha, worktree_id,
            // generation).
            params![path, now_ms(), worktree_id, self.active_repo_id, self.active_generation],
        )?;
        self.mark_fts_dirty()?;
        Ok(())
    }

    pub(super) fn remove_file_in_scope(
        &self,
        path: &Path,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<()> {
        // #493: every live-scope file replacement funnels through here, so this is the seam that
        // memoizes the drift-heal snapshot BEFORE the symbol deletes below destroy its
        // member-signature evidence. A no-op unless the key-version stamp is stale, and paid at
        // most once per pass.
        self.capture_drift_snapshot_before_removal()?;
        let path = path_string(path);
        // Direct edges_data writes (#79): these statements touch up to every in-edge of a file's
        // symbols, so they must not pay the view triggers' per-row dictionary probes.
        // 'NameOnly' is the EdgeConfidence demotion the resolver applies to a target-less edge.
        let name_only_id = edges::intern_edge_string(self.storage.connection(), "NameOnly")?;
        let repo_id = self.active_repo_id.as_str();
        // Every delete below carries the WRITER'S generation (A6, P2 review): the V043 UNIQUE
        // admits one row per (repo, path, commit, worktree) PER GENERATION, so a scope key alone
        // over-matches — a lockless heal or incremental replacement running mid-rebuild (its
        // connection at the LIVE generation) would otherwise also delete the STAGED generation's
        // freshly committed row for the same scope key, punching a hole in the generation about
        // to be published. The writer's `active_generation` is live for heals/incremental and
        // the staging target on the rebuild connection, so each writer removes only its own
        // generation's rows.
        let generation = self.active_generation;
        self.storage.connection().execute(
            "UPDATE edges_data
             SET to_symbol_id = NULL,
                 confidence_id = ?4
             WHERE to_symbol_id IN (
                 SELECT symbols.id FROM symbols
                 JOIN main.files ON main.files.id = symbols.file_id
                 WHERE main.files.path = ?1
                   AND main.files.commit_sha = ?2
                   AND main.files.worktree_id = ?3
                   AND main.files.repo_id = ?5
                   AND main.files.generation = ?6
             )",
            params![path, commit_sha, worktree_id, name_only_id, repo_id, generation],
        )?;
        self.storage.connection().execute(
            "DELETE FROM edges_data
             WHERE source_file_id IN (
                    SELECT id FROM main.files
                    WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3 AND repo_id = ?4
                      AND generation = ?5
                )
                OR from_symbol_id IN (
                    SELECT symbols.id FROM symbols
                    JOIN main.files ON main.files.id = symbols.file_id
                    WHERE main.files.path = ?1
                      AND main.files.commit_sha = ?2
                      AND main.files.worktree_id = ?3
                      AND main.files.repo_id = ?4
                      AND main.files.generation = ?5
                )",
            params![path, commit_sha, worktree_id, repo_id, generation],
        )?;
        // `parser_failures` is keyed by `(repo_id, path)` (V040). A LINKED-WORKTREE OVERLAY pass
        // must NOT delete: that would clear a REAL parse failure recorded for the same path
        // by the base (or a sibling) scope. The overlay never WRITES this table either (see
        // `insert_parser_failure`), so it has nothing of its own to remove (#219 review). Scoped by
        // `repo_id` (A3) so a sibling REPO's failure at the same path is never clobbered (the
        // inventory-#12 cross-repo clobber the PK change fixes).
        if !self.active_scope_is_linked_overlay() {
            self.storage.connection().execute(
                "DELETE FROM parser_failures WHERE repo_id = ?1 AND path = ?2",
                params![repo_id, &path],
            )?;
        }
        self.storage.connection().execute(
            "DELETE FROM chunk_fts
             WHERE rowid IN (
                 SELECT chunks.id FROM chunks
                 JOIN main.files ON main.files.id = chunks.file_id
                 WHERE main.files.path = ?1
                   AND main.files.commit_sha = ?2
                   AND main.files.worktree_id = ?3
                   AND main.files.repo_id = ?4
                   AND main.files.generation = ?5
             )",
            params![path, commit_sha, worktree_id, repo_id, generation],
        )?;
        // Deleting the chunks cascades (ON DELETE CASCADE, foreign_keys=ON) to git_chunk_blame,
        // chunk_embeddings, chunk_summaries, and chunk_text — so the gate skipping the full
        // git-history wipe does NOT leak blame, and compressed text (#77) doesn't orphan. (`docs`
        // has no FK and is not cleaned here — a pre-existing gap, tracked separately.) The
        // full-rebuild / GC path (delete_staged_files_cascade) enumerates these explicitly for the
        // FK-off-during-migration case; this live path runs with foreign_keys = ON.
        self.storage.connection().execute(
            "DELETE FROM chunks
             WHERE file_id IN (
                SELECT id FROM main.files
                WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3 AND repo_id = ?4
                  AND generation = ?5
             )",
            params![path, commit_sha, worktree_id, repo_id, generation],
        )?;
        self.storage.connection().execute(
            "DELETE FROM symbols
             WHERE file_id IN (
                SELECT id FROM main.files
                WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3 AND repo_id = ?4
                  AND generation = ?5
             )",
            params![path, commit_sha, worktree_id, repo_id, generation],
        )?;
        self.storage.connection().execute(
            "DELETE FROM main.files
             WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3 AND repo_id = ?4
               AND generation = ?5",
            params![path, commit_sha, worktree_id, repo_id, generation],
        )?;
        self.mark_fts_dirty()?;
        Ok(())
    }

    /// Adopt retained committed rows into the ACTIVE commit scope by re-stamping
    /// `files.commit_sha` in place (#502): the row id — and every chunk, symbol, edge,
    /// embedding, fingerprint, FTS entry, and memory binding hanging off it — survives, so a
    /// HEAD move (pull, branch checkout) costs roughly its diff instead of a full re-derive.
    /// The overlay→commit re-stamp in [`Self::heal_stale_overlay_rows`] is the precedent.
    ///
    /// The candidates come from [`discovery_plan`](super::discovery::discovery_plan), which
    /// selects only paths ABSENT from the active scope, and the caller's pass transaction
    /// isolates selection from application — so the V043 UNIQUE
    /// `(repo_id, path, commit_sha, worktree_id, generation)` cannot collide. The WHERE still
    /// pins the row to this writer's repo + generation + base scope (belt to the plan's
    /// selection), so a stale candidate updates nothing rather than the wrong row.
    pub(super) fn carry_retained_files_into_active_scope(
        &self,
        carried: &[i64],
    ) -> anyhow::Result<usize> {
        if carried.is_empty() || self.active_commit_sha.is_empty() {
            return Ok(0);
        }
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(
            "UPDATE main.files SET commit_sha = ?2
             WHERE id = ?1 AND repo_id = ?3 AND generation = ?4 AND worktree_id = ''
               AND commit_sha != ?2",
        )?;
        let mut restamped = 0usize;
        for file_id in carried {
            restamped += stmt.execute(params![
                file_id,
                self.active_commit_sha,
                self.active_repo_id,
                self.active_generation,
            ])?;
        }
        if restamped > 0 {
            tracing::info!(
                target: "rag_rat_core::maintenance",
                carried = restamped,
                commit = %self.active_commit_sha,
                "carried retained committed rows into the active scope (HEAD move)"
            );
        }
        Ok(restamped)
    }

    /// The `modified_at_ms` (source-file mtime) of the CURRENT file row at `(active repo, path,
    /// commit_sha, worktree_id, active generation)`, or `None` when no such row exists — the #561
    /// concurrent-writer guard. The incremental write phase compares this against the mtime it
    /// prepared: a row whose disk mtime is NEWER means a lockless heal indexed a fresher version
    /// during the OFF-lock prepare window, so the caller skips its (now-stale) overwrite. Using
    /// disk mtime (not the indexing clock `indexed_at_ms`) is what makes this
    /// false-positive-free: the row's OWN prior stamp is always older-or-equal (edits only
    /// advance mtime), so a legitimate re-index never trips the guard — only a genuinely newer
    /// index does. A tombstone's `modified_at_ms = 0` never exceeds a real prepared mtime, so
    /// it never blocks a resurrection. Point lookup on the V043 UNIQUE `(repo_id, path,
    /// commit_sha, worktree_id, generation)`; direct `main.files` probe (not the scope view),
    /// so it carries `repo_id` + `generation` explicitly like the other file-row probes here.
    pub(super) fn scope_row_modified_at_ms(
        &self,
        path: &Path,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<Option<i64>> {
        self.storage
            .connection()
            .query_row(
                "SELECT modified_at_ms FROM main.files
                 WHERE repo_id = ?1 AND path = ?2 AND commit_sha = ?3 AND worktree_id = ?4
                   AND generation = ?5",
                params![
                    self.active_repo_id,
                    path_string(path),
                    commit_sha,
                    worktree_id,
                    self.active_generation
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// The `(sha256, language, kind)` identity of the row for this exact scope key (`None` when no
    /// such row exists) — the no-op-skip signal for the explicit-path flow: an `index --paths` over
    /// a CLEAN/reverted file prepares a row whose identity matches the existing one, and
    /// `write_prepared_incremental_files` skips the remove+insert so the row id and its chunk
    /// embeddings are not needlessly churned (#659 review). Includes `language`/`kind` so a TARGET
    /// identity change with UNCHANGED bytes (an extension-precedence upgrade re-languages a path
    /// without touching its content) is NOT skipped — mirroring discovery's `(sha256, language,
    /// kind)` staleness ([`super::discovery::target_for_path`] drift). Sibling of
    /// [`Self::scope_row_modified_at_ms`].
    pub(super) fn scope_row_identity(
        &self,
        path: &Path,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<Option<(String, String, String)>> {
        self.storage
            .connection()
            .query_row(
                "SELECT sha256, language, kind FROM main.files
                 WHERE repo_id = ?1 AND path = ?2 AND commit_sha = ?3 AND worktree_id = ?4
                   AND generation = ?5",
                params![
                    self.active_repo_id,
                    path_string(path),
                    commit_sha,
                    worktree_id,
                    self.active_generation
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Whether the ACTIVE scope (any commit/worktree at the live generation) has a row for `path` —
    /// used by the explicit-path flow to tombstone a vanished supplied path ONLY when it was really
    /// indexed, never a never-indexed typo / out-of-target file (a spurious `kind='deleted'`
    /// overlay row would shadow any real committed file later appearing at that path) (#659
    /// review).
    pub(super) fn path_has_indexed_row(&self, path: &Path) -> anyhow::Result<bool> {
        self.storage
            .connection()
            .query_row(
                "SELECT 1 FROM files WHERE path = ?1 LIMIT 1",
                params![path_string(path)],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(Into::into)
    }

    pub(super) fn file_row(&self, path: &Path) -> anyhow::Result<FileRow> {
        self.storage
            .connection()
            .query_row(
                "SELECT language, kind FROM files WHERE path = ?1",
                [path_string(path)],
                |row| {
                    let language: String = row.get(0)?;
                    let kind: String = row.get(1)?;
                    Ok((language, kind))
                },
            )
            .map_err(Into::into)
            .and_then(|(language, kind)| {
                Ok(FileRow { language: language.parse()?, kind: kind.parse()? })
            })
    }

    pub(super) fn indexed_files(&self) -> anyhow::Result<Vec<IndexedFile>> {
        let mut stmt =
            self.storage.connection().prepare("SELECT path, sha256 FROM files ORDER BY path")?;
        let rows =
            stmt.query_map([], |row| Ok(IndexedFile { path: row.get(0)?, sha256: row.get(1)? }))?;
        let mut files = Vec::new();
        for row in rows {
            files.push(row?);
        }
        Ok(files)
    }

    /// Re-derive `files.generated` from the current [`is_generated_path`] heuristic (the single
    /// source of truth) for every file whose stored flag disagrees, gated on
    /// [`GENERATED_FLAGS_VERSION`] so it runs once per definition change. Needed because
    /// incremental discovery rewrites a file row only on sha/language/kind change — when the
    /// *meaning* of the flag changes (#202) the inputs are identical, so nothing would refresh
    /// it. Idempotent. Runs only on a write-bearing open (read-only opens see the stale version
    /// and fall back).
    pub(super) fn ensure_generated_flags_current(&self) -> anyhow::Result<()> {
        if self.meta(GENERATED_FLAGS_VERSION_KEY)?.as_deref() == Some(GENERATED_FLAGS_VERSION) {
            return Ok(());
        }
        self.storage.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = self.rederive_generated_flags();
        if result.is_err() {
            let _ = self.storage.execute_batch("ROLLBACK");
            result?;
        }
        self.set_meta(GENERATED_FLAGS_VERSION_KEY, GENERATED_FLAGS_VERSION)?;
        self.storage.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Stamp the generated-flags version current. Called after a full rebuild / incremental pass,
    /// which already write correct flags via `file_is_generated`, so the next open skips the
    /// re-derive.
    pub(super) fn mark_generated_flags_current(&self) -> anyhow::Result<()> {
        self.set_meta(GENERATED_FLAGS_VERSION_KEY, GENERATED_FLAGS_VERSION)
    }

    /// Stamp the embedding-policy freshness for THIS repo (`repo_meta`, not the DB-global
    /// `index_meta`), certifying that every `chunks.embedding_policy` reflects the current
    /// classifier ([`EMBEDDING_POLICY_VERSION`](crate::index::ai::EMBEDDING_POLICY_VERSION)) at
    /// the default cap. The reconcile skip-summary then reads the column via `GROUP BY` instead
    /// of re-parsing every file (#530). Called ONLY where every chunk was (re)derived by
    /// current code — a full rebuild and the reconcile self-heal — NEVER an incremental pass,
    /// which restamps only changed files and so cannot certify unchanged chunks.
    pub(super) fn mark_embedding_policy_current(&self) -> anyhow::Result<()> {
        self.set_repo_meta(ai::EMBEDDING_POLICY_VERSION_KEY, ai::EMBEDDING_POLICY_VERSION)?;
        self.set_repo_meta(
            ai::EMBEDDING_POLICY_CAP_KEY,
            &ai::DEFAULT_MAX_EMBEDDING_CHARS.to_string(),
        )
    }

    fn rederive_generated_flags(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        // The generated flag is a property of the file PATH (+ target kind), not the active scope,
        // so re-derive over the base `main.files` for every row — NOT the per-connection `files`
        // scope view (a non-updatable UNION; #89).
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = conn.prepare("SELECT id, path, kind FROM main.files")?;
            let mapped = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?;
            mapped.collect::<rusqlite::Result<_>>()?
        };
        // Mirror `file_is_generated` without parsing `kind` (skips the `deleted`/`unknown` markers
        // cleanly): explicit generated target OR the path heuristic.
        let mut update = conn.prepare_cached(
            "UPDATE main.files SET generated = ?2 WHERE id = ?1 AND generated != ?2",
        )?;
        for (id, path, kind) in rows {
            let generated = kind == TargetKind::Generated.as_str() || is_generated_path(&path);
            update.execute(params![id, generated])?;
        }
        Ok(())
    }

    pub(super) fn indexed_file_count(&self) -> anyhow::Result<usize> {
        let count =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub(super) fn repo_generation_file_count(
        &self,
        include_retained_commit_fallback: bool,
    ) -> anyhow::Result<usize> {
        let count = self.storage.connection().query_row(
            "SELECT COUNT(*) FROM (
                 SELECT path FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2
                   AND worktree_id = ?3 AND worktree_id != '' AND kind != 'deleted'
                 UNION
                 SELECT path FROM main.files
                 WHERE repo_id = ?1 AND generation = ?2
                   AND worktree_id = '' AND kind != 'deleted'
                   AND (
                       commit_sha = ?4
                       OR (
                           ?5 AND ?4 != '' AND commit_sha != '' AND NOT EXISTS (
                               SELECT 1 FROM main.files
                               WHERE repo_id = ?1 AND generation = ?2
                                 AND worktree_id = '' AND commit_sha = ?4 AND kind != 'deleted'
                           )
                       )
                   )
                   AND path NOT IN (
                       SELECT path FROM main.files
                       WHERE repo_id = ?1 AND generation = ?2
                         AND worktree_id = ?3 AND worktree_id != ''
                   )
             )",
            params![
                self.active_repo_id,
                self.active_generation,
                self.active_worktree_id,
                self.active_commit_sha,
                include_retained_commit_fallback,
            ],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }
}
