use super::*;

impl IndexDatabase {
    pub fn rebuild(config: &Config) -> anyhow::Result<Self> {
        Self::rebuild_with_progress(config, |_| {})
    }

    pub fn rebuild_with_progress<F>(config: &Config, mut progress: F) -> anyhow::Result<Self>
    where
        F: FnMut(IndexProgress),
    {
        progress(IndexProgress::Started {
            database: config.database.clone(),
            mode: IndexMode::Full,
        });
        let mut db = Self::create_or_migrate(&config.database)?;
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        progress(IndexProgress::IndexingGitHistory);
        let mut git_history = Some(spawn_git_history_prepare(&config.root));
        // RAM-first bulk build: a full rebuild is one big atomic write, so skip per-commit fsyncs
        // (synchronous=OFF) and give SQLite a large page cache. Restored to NORMAL after the
        // rebuild. Only `rebuild` uses this; incremental indexing and the watcher stay durable.
        //
        // NB: stay in WAL — switching journal_mode needs an EXCLUSIVE database lock, which fails
        // ("database is locked") whenever another connection is open (e.g. the watcher, or a
        // concurrent reader). `synchronous` and `cache_size` are per-connection and safe under
        // concurrency. Also do NOT touch `temp_store` — changing it drops the connection_context
        // overlay temp table created by `set_context` above.
        db.storage.execute_batch(
            "PRAGMA synchronous = OFF;
             PRAGMA cache_size = -262144;",
        )?;
        maybe_set_sqlite_soft_heap_limit();
        // Diagnostic: override wal_autocheckpoint for this rebuild. Setting it to 0 disables the
        // auto-checkpoint that fires at COMMIT — used to test whether the trailing peak spike is
        // the final checkpoint of the multi-GB WAL (mega-transaction) vs something else. No-op
        // unless RAG_RAT_WAL_AUTOCHECKPOINT is set.
        if let Ok(raw) = std::env::var("RAG_RAT_WAL_AUTOCHECKPOINT")
            && let Ok(pages) = raw.trim().parse::<i64>()
        {
            db.storage.execute_batch(&format!("PRAGMA wal_autocheckpoint = {pages};"))?;
        }
        let result = (|| -> anyhow::Result<()> {
            mem_trace("before clear (start of rebuild txn)");
            // BEGIN IMMEDIATE acquires the write lock up front. A plain BEGIN starts as a reader
            // and upgrades on the first mutation; if another writer raced in between, the upgrade
            // fails with SQLITE_BUSY *immediately* (busy_timeout doesn't apply to the upgrade,
            // since retrying would break snapshot isolation). IMMEDIATE makes the wait honor
            // busy_timeout instead — the fix for the intermittent multi-writer "deadlock".
            db.storage.execute_batch("BEGIN IMMEDIATE")?;
            db.clear_full_rebuild_tables()?;
            mem_trace("after clear_full_rebuild_tables (purge)");
            db.set_meta("source_root", &config.root.display().to_string())?;
            db.storage.set_source_root(config.root.clone());
            // Per-package import scope (#61): the `packages` rows + the global `local_crate_roots`
            // union are written by `refresh_packages`, called inside `index_targets_with_progress`
            // AFTER files are inserted but BEFORE the in-memory resolve pass (which computes each
            // file's package from those rows at load time) — the files do not exist yet here.
            db.write_git_meta(&config.root)?;
            let indexed = db.index_targets_with_progress(config, &mut progress)?;
            mem_trace("after index_targets (edges resolved+inserted)");
            db.apply_prepared_git_history(
                &config.root,
                git_history
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("git history preparation was already used"))?,
            )?;
            mem_trace("after git_history");
            progress(IndexProgress::RebuildingLogicalSymbols);
            db.rebuild_logical_symbols()?;
            mem_trace("after rebuild_logical_symbols");
            // Edges were resolved and inserted in one in-memory pass inside
            // index_targets_with_progress (full rebuild), so there is no separate resolve_edges
            // phase.
            progress(IndexProgress::ResolvingGraph);
            db.mark_graph_index_current()?;
            // Full rebuild writes correct `files.generated` via `file_is_generated`, so stamp the
            // flags version current and skip a redundant re-derive on next open (#202).
            db.mark_generated_flags_current()?;
            mem_trace("after mark_graph_index_current");
            // Derive the compressed chunk_text store (#77 Phase 2) from chunks.text inside the same
            // transaction, so the dict row + the blobs that use it are committed atomically.
            db.build_chunk_text_store()?;
            mem_trace("after build_chunk_text_store");
            progress(IndexProgress::RebuildingFts);
            // chunk_fts was written inline during chunk insert; only commit_fts
            // needs the bulk 'rebuild' here (#77 Phase 2).
            db.finalize_full_rebuild_fts()?;
            mem_trace("after finalize_full_rebuild_fts");
            // Recompute clone token document-frequency authoritatively from the token-bag BLOBs
            // just written (#231). The per-symbol incremental bump in store_symbol_fingerprints
            // drifts over edits; a full rebuild owns the whole checkout, so derive df exactly here.
            // df is a selectivity hint only (candidate read COALESCEs it), never a correctness
            // input — but keeping it exact maximizes the rare-first sub-block prune.
            db.refresh_clone_token_df()?;
            mem_trace("after refresh_clone_token_df");
            db.set_meta("indexed_at_ms", &now_ms().to_string())?;
            db.storage.execute_batch("COMMIT")?;
            mem_trace("after COMMIT");
            progress(IndexProgress::Finished { files: indexed });
            Ok(())
        })();
        if result.is_err() {
            if let Some(handle) = git_history.take() {
                let _ = join_git_history_prepare(handle);
            }
            let _ = db.storage.execute_batch("ROLLBACK");
        }
        // Restore durable fsync behavior for any later writes on this connection (reconcile, etc.).
        // cache_size is left bumped — harmless for the short remaining lifetime of the connection.
        let _ = db.storage.execute_batch("PRAGMA synchronous = NORMAL;");
        result?;
        Ok(db)
    }

    /// Recompute `clone_token_df` exactly from the `symbol_fingerprints.token_bag` BLOBs (#231).
    /// A Rust aggregate over the decoded bags replaces the former `GROUP BY symbol_token_postings`
    /// (that table is dropped in V032) with the same authoritative document frequency: the count of
    /// distinct symbols whose bag contains each token, per `(normalizer_kind, token_hash)`. Runs
    /// inside the rebuild transaction so the df and the fingerprints it summarizes commit
    /// atomically.
    ///
    /// R6 — SCOPE PARITY: this must match the old GROUP BY EXACTLY. That query had NO
    /// `files.generated` filter, so it counted generated-file symbols too; this aggregate likewise
    /// reads EVERY `symbol_fingerprints` row (generated included). df feeds candidate GENERATION
    /// (the `sub_block_tokens` ordering), not just ranking, so a scope mismatch would change
    /// recall. Each symbol's decoded bag has no duplicate `token_hash` (the codec invariant),
    /// so counting one increment per (symbol, token) pair equals `COUNT(DISTINCT symbol_id)`.
    fn refresh_clone_token_df(&self) -> anyhow::Result<()> {
        // Phase 1 (read): decode every fingerprint's bag and accumulate df in memory, off the
        // connection borrow. NULL `token_bag` rows (un-reindexed after the V032 migration) and any
        // stale/corrupt blob (decode → None) contribute nothing, exactly as a missing postings row
        // would have.
        let mut df: BTreeMap<(String, i64), i64> = BTreeMap::new();
        {
            let conn = self.storage.connection();
            let mut stmt =
                conn.prepare("SELECT normalizer_kind, token_bag FROM symbol_fingerprints")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let normalizer_kind: String = row.get(0)?;
                let Some(blob) = row.get::<_, Option<Vec<u8>>>(1)? else {
                    continue;
                };
                let Some(bag) = clones::bag_blob::decode_token_bag(&blob) else {
                    continue;
                };
                for (token_hash, _freq) in bag {
                    *df.entry((normalizer_kind.clone(), token_hash)).or_insert(0) += 1;
                }
            }
        }

        // Phase 2 (write): replace the table contents with the recomputed df.
        let conn = self.storage.connection();
        conn.execute_batch("DELETE FROM clone_token_df;")?;
        for ((normalizer_kind, token_hash), count) in df {
            conn.prepare_cached(
                "INSERT INTO clone_token_df(normalizer_kind, token_hash, df) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![normalizer_kind, token_hash, count])?;
        }
        Ok(())
    }

    fn clear_full_rebuild_tables(&self) -> anyhow::Result<()> {
        // Stage the active context's file ids, then cascade-delete them and their derived rows.
        //
        // AUTHORITATIVE (load-bearing, #87): a full rebuild owns the WHOLE checkout, so the
        // commit-scope staging deliberately does NOT mirror the scope VIEW's shadowing rule.
        // The view excludes a committed row whose path has a worktree-overlay row (overlay
        // wins for reads) — but exempting it from the CLEAR left it behind to collide with the
        // rebuild's fresh insert at the same `(path, commit_sha, '')` whenever a stale overlay
        // lingered (a dirty-then-committed file whose cleanup never ran), failing the whole
        // rebuild with a UNIQUE constraint error. Stage every row of the active commit AND
        // every row of the active worktree, shadowed or not. A sibling worktree at the same
        // commit self-heals on its next discover pass (its missing paths reindex).
        self.storage.execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS staged_file_ids(id INTEGER PRIMARY KEY);
            DELETE FROM temp.staged_file_ids;
            INSERT OR IGNORE INTO temp.staged_file_ids(id)
            SELECT id
            FROM main.files
            WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
             'worktree_id')
              AND worktree_id != '';
            INSERT OR IGNORE INTO temp.staged_file_ids(id)
            SELECT id
            FROM main.files
            WHERE commit_sha = (SELECT value FROM temp.connection_context WHERE key = 'commit_sha')
              AND commit_sha != '';
            ",
        )?;
        self.delete_staged_files_cascade()?;
        // Do NOT clear chunk_text_dict: dicts are IMMUTABLE decode keys (#77 Phase 2). Other
        // worktree contexts' blobs reference existing versions, and deleting a version would orphan
        // them. The staged cascade above already removed THIS context's chunk_text rows;
        // insert_chunks recompresses them against the latest existing dict version (or, on
        // the very first index when no dict exists yet, stages the text so
        // build_chunk_text_store trains version 1). Per-connection staging table for that
        // first-index path: insert_chunks writes the in-memory text here (there is no
        // chunks.text column) and build_chunk_text_store reads + clears it.
        self.storage.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS rebuild_chunk_text(
                 chunk_id INTEGER PRIMARY KEY,
                 text TEXT NOT NULL
             );
             DELETE FROM temp.rebuild_chunk_text;",
        )?;
        self.storage.execute_batch("DELETE FROM temp.staged_file_ids;")?;
        Ok(())
    }

    /// Cascade-delete every derived row (edges, symbols, chunks, embeddings, FTS, blame, docs,
    /// parser failures) for the file ids staged in `temp.staged_file_ids`, then the files
    /// themselves. The caller is responsible for populating and clearing the temp table.
    /// Shared by full rebuild (active context) and GC (dead, non-live contexts).
    pub(super) fn delete_staged_files_cascade(&self) -> anyhow::Result<()> {
        self.storage.execute_batch(
            "
            INSERT OR IGNORE INTO main.name_strings(value) VALUES ('unresolved');
            UPDATE main.edges_data
            SET to_symbol_id = NULL,
                target_start_line = NULL,
                target_end_line = NULL,
                resolution_id =
                    (SELECT id FROM main.name_strings WHERE value = 'unresolved')
            WHERE to_symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.edges_data
            WHERE source_file_id IN (SELECT id FROM temp.staged_file_ids)
               OR from_symbol_id IN (
                    SELECT symbols.id
                    FROM main.symbols
                    JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
               );

            DELETE FROM main.logical_symbol_members
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.logical_symbols
            WHERE id NOT IN (
                SELECT logical_symbol_id FROM main.logical_symbol_members
            );
            DELETE FROM main.symbol_facts
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            DELETE FROM main.chunk_fts
            WHERE rowid IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.chunk_summaries
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.chunk_embeddings
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.git_chunk_blame
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.docs
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            -- chunk_text cascades from chunks via FK, but enumerate it explicitly like the other
            -- chunk-child tables (#77): the migration runner toggles foreign_keys = OFF, so a \
             delete
            -- that ran while FK was off would orphan compressed blobs.
            DELETE FROM main.chunk_text
            WHERE chunk_id IN (
                SELECT chunks.id
                FROM main.chunks
                JOIN temp.staged_file_ids ON staged_file_ids.id = chunks.file_id
            );
            DELETE FROM main.parser_failures
            WHERE path IN (
                SELECT path
                FROM main.files
                JOIN temp.staged_file_ids ON staged_file_ids.id = files.id
            );
            DELETE FROM main.symbol_fingerprints
            WHERE symbol_id IN (
                SELECT symbols.id
                FROM main.symbols
                JOIN temp.staged_file_ids ON staged_file_ids.id = symbols.file_id
            );
            -- The token bag rides the symbol_fingerprints row deleted above as the `token_bag`
            -- BLOB column (#231); symbol_token_postings was dropped in V032, so there is no
            -- separate per-token table to cascade here (R1: a DELETE on it would error
            -- `no such table` on every reindex).
            DELETE FROM main.symbols
            WHERE file_id IN (SELECT id FROM temp.staged_file_ids);
            DELETE FROM main.chunks
            WHERE file_id IN (SELECT id FROM temp.staged_file_ids);
            DELETE FROM main.files
            WHERE id IN (SELECT id FROM temp.staged_file_ids);
            ",
        )?;
        Ok(())
    }
}
