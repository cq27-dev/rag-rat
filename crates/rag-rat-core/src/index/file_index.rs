//! The file → rows indexing pipeline: parse/chunk/symbol one file and write its chunk,
//! symbol, and logical-group rows; heal a stale file in place.

use super::*;
use crate::index::graph_index::{LogicalSymbolKey, LogicalSymbolMemberRow};

/// Identity of the file whose chunks are being inserted — passed from the caller (which just
/// inserted the file row) so `insert_chunks` doesn't re-`SELECT` it per file (#57).
struct ChunkInsertFile<'a> {
    file_id: i64,
    source_revision: &'a str,
}

impl IndexDatabase {
    pub fn heal_file(&self, path: &Path) -> anyhow::Result<()> {
        // A heal reads bytes from `source_root` (the MAIN checkout). Under a linked-worktree
        // overlay scope that would shadow the branch's row with MAIN's content, so skip it
        // — the overlay is maintained only by `index_worktree_overlay` (#219 review).
        if self.active_scope_is_linked_overlay() {
            return Ok(());
        }
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("index has no source_root metadata; rebuild required");
        };
        let row = self.file_row(path)?;
        let full_path = root.join(path);
        let text = match fs::read_to_string(&full_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // File deleted on disk since indexing — drop it from the index rather than
                // hard-erroring the whole search. The caller (search_with_heal) re-runs
                // search without it. Mirrors read_chunk_current's deletion handling.
                self.mark_file_deleted(path)?;
                return Ok(());
            },
            Err(e) => return Err(e.into()),
        };

        let changes = git_changed_paths(root).unwrap_or_default();
        let is_dirty = changes.changed.contains(path);
        let has_base_commit = !self.active_commit_sha.is_empty();
        let scope = if !has_base_commit || is_dirty {
            FileScope::worktree(self.active_worktree_id.clone())
        } else {
            FileScope::commit(self.active_commit_sha.clone())
        };
        self.remove_file_in_scope(path, &scope.commit_sha, &scope.worktree_id)?;

        self.index_file(
            path,
            row.language,
            row.kind,
            file_metadata_ms(&full_path)?,
            &text,
            &scope,
        )?;
        self.rebuild_logical_symbols()?;
        self.resolve_edges()
    }

    fn index_file(
        &self,
        path: &Path,
        language: Language,
        kind: TargetKind,
        modified_at_ms: i64,
        text: &str,
        scope: &FileScope,
    ) -> anyhow::Result<()> {
        if language != Language::Markdown && kind != TargetKind::Generated {
            if text.len() > chunker::MAX_STRUCTURAL_PARSE_BYTES {
                // Large source files are intentionally coarse-indexed to keep full-repo indexing
                // responsive. This is not a parser failure.
            } else if let Some(message) = parser::parse_error(path, language, text)
                .unwrap_or_else(|err| Some(err.to_string()))
            {
                self.insert_parser_failure(path, language, &message)?;
            }
        }
        let sha256 = hex_sha256(text.as_bytes());
        let chunks = if kind == TargetKind::Generated {
            chunker::generated_chunks_for_file(path, text)
        } else {
            chunker::chunks_for_file(path, language, text)
        };
        let chunks = prepare_chunks(path, language.as_str(), kind.as_str(), chunks, text);
        // has_test_code from the SAME chunk-text marker set as insert_prepared_file + the V024
        // backfill, so a file healed through this path matches a fully-indexed one (#77). The heal
        // path is a second files-insert site; chunks are prepared up here (they don't need file_id)
        // so the flag can be set in the INSERT instead of left at the default 0.
        let has_test_code = chunks.iter().any(|pc| text_has_test_marker(&pc.chunk.text));
        let file_id = self.storage.connection().query_row(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
             indexed_at_ms, indexed_revision, commit_sha, worktree_id, has_test_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             RETURNING id",
            params![
                path_string(path),
                language.as_str(),
                kind.as_str(),
                sha256,
                modified_at_ms,
                file_is_generated(kind, &path_string(path)),
                now_ms(),
                sha256,
                &scope.commit_sha,
                &scope.worktree_id,
                has_test_code,
            ],
            |row| row.get::<_, i64>(0),
        )?;
        let symbols =
            if kind == TargetKind::Generated || text.len() > chunker::MAX_STRUCTURAL_PARSE_BYTES {
                Vec::new()
            } else {
                symbols::symbols_for_file(path, language, text)
            };
        // Inline/heal path: keep chunk_fts in sync per row (partial file replace would otherwise
        // desync the external-content index until the next forced rebuild).
        self.insert_chunks(ChunkInsertFile { file_id, source_revision: &sha256 }, &chunks)?;
        let symbol_ids = self.insert_symbols(file_id, language, &symbols)?;
        self.store_symbol_fingerprints(language, path, text, &symbols, &symbol_ids)?;
        if kind != TargetKind::Generated && text.len() <= edges::MAX_GRAPH_PARSE_BYTES {
            edges::index_file_edges(self.storage.connection(), file_id, path, language, text)?;
        }
        self.mark_fts_dirty()?;
        Ok(())
    }

    /// `graph`: on a full rebuild, `Some` accumulator — symbols (with their new DB ids) and
    /// remapped edge candidates are collected for one in-memory resolve-and-insert pass after
    /// the loop, and NO edges are inserted here. `None` (incremental) inserts edges unresolved
    /// per file as before, to be resolved by the DB-based `resolve_edges`.
    pub(super) fn insert_prepared_file(
        &self,
        prepared_file: &PreparedIndexFile,
        graph: Option<&mut edges::FullRebuildGraph>,
    ) -> anyhow::Result<()> {
        let file = &prepared_file.file;
        let prepared = match &prepared_file.prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                self.insert_parser_failure(&file.relative_path, file.language, &err.to_string())?;
                return Ok(());
            },
        };
        if let Some(message) = &prepared.parser_failure {
            self.insert_parser_failure(&file.relative_path, file.language, message)?;
        }
        let path = path_string(&file.relative_path);
        // Precompute the file-level test-code flag (#77): impact_surface's "tests touching this
        // symbol" query reads this indexed flag instead of scanning `chunks.text` for the markers.
        // Computed from the chunk text we already hold; same marker set as the V024 backfill.
        let has_test_code = prepared.chunks.iter().any(|pc| text_has_test_marker(&pc.chunk.text));
        // prepare_cached so the per-file/-chunk/-symbol INSERTs compile once per connection and
        // are reused across the whole rebuild instead of re-parsing the SQL on every row (#57).
        let file_id = self
            .storage
            .connection()
            .prepare_cached(
                "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
                 indexed_at_ms, indexed_revision, commit_sha, worktree_id, has_test_code)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 RETURNING id",
            )?
            .query_row(
                params![
                    path,
                    file.language.as_str(),
                    file.kind.as_str(),
                    prepared.sha256,
                    prepared.modified_at_ms,
                    file_is_generated(file.kind, &path),
                    now_ms(),
                    prepared.sha256,
                    file.commit_sha,
                    file.worktree_id,
                    has_test_code,
                ],
                |row| row.get::<_, i64>(0),
            )?;
        self.insert_chunks(
            ChunkInsertFile { file_id, source_revision: &prepared.sha256 },
            &prepared.chunks,
        )?;
        let symbol_db_ids = self.insert_symbols(file_id, file.language, &prepared.symbols)?;
        // Clone fingerprints were computed in the parallel prepare phase from the same parse used
        // for symbols/edges (#230) — no second read, no second parse here, just the DB write.
        self.write_symbol_fingerprints(&symbol_db_ids, &prepared.symbol_fingerprints)?;
        // Edge candidates were computed in the parallel prepare phase with LOCAL symbol indices;
        // remap them to the real DB ids just assigned.
        match graph {
            // Full rebuild: accumulate symbols (with ids) + remapped edges for one in-memory
            // resolve-and-insert pass after the loop. No edge insert here.
            Some(graph) => {
                for (symbol, &id) in prepared.symbols.iter().zip(&symbol_db_ids) {
                    graph.push_symbol(id, file_id, file.language, symbol);
                }
                for candidate in &prepared.edge_candidates {
                    graph.push_edge(file_id, candidate, &symbol_db_ids);
                }
            },
            // Incremental: insert unresolved edges per file; resolve_edges resolves them afterward.
            None =>
                if !prepared.edge_candidates.is_empty() {
                    let mut candidates = prepared.edge_candidates.clone();
                    for candidate in &mut candidates {
                        candidate.remap_from_symbol_id(&symbol_db_ids);
                    }
                    edges::insert_candidates(self.storage.connection(), file_id, candidates)?;
                },
        }
        self.mark_fts_dirty()?;
        Ok(())
    }

    /// Compute + persist baseline clone fingerprints for the file's function symbols (#215). A
    /// fingerprint is a pure function of the symbol body, so it is scope-independent and keyed by
    /// symbol_id; the FK cascade discards it when the symbol is removed on reindex. Re-parses the
    /// file to walk the AST — the incremental/heal path that calls this already re-reads the file,
    /// so the extra parse is local to that path. The full-rebuild path computes fingerprints in the
    /// parallel prepare phase (#230) and calls `write_symbol_fingerprints` directly instead.
    fn store_symbol_fingerprints(
        &self,
        language: Language,
        path: &Path,
        text: &str,
        symbols: &[Symbol],
        symbol_ids: &[i64],
    ) -> anyhow::Result<()> {
        // Only `kind == "function"` symbols are fingerprinted (#215). Bail before re-parsing the
        // file when none qualify — the parse is the expensive part of this incremental/heal
        // wrapper.
        if symbols.iter().all(|s| s.kind != "function") {
            return Ok(());
        }
        let Some(parsed) = parser::parse_file(path, language, text) else {
            return Ok(());
        };
        let fingerprints = clones::fingerprint_symbols(parsed.root(), text, symbols);
        self.write_symbol_fingerprints(symbol_ids, &fingerprints)
    }

    /// Write precomputed baseline clone fingerprints (#215). `fingerprints` carries
    /// `(local_symbol_index, fingerprint)` pairs from `clones::fingerprint_symbols`; each index
    /// selects the matching DB id in `symbol_db_ids`. This is the DB-write half shared by the
    /// full-rebuild prepare phase (#230) and the incremental `store_symbol_fingerprints` wrapper,
    /// so the per-row insert discipline lives in exactly one place.
    fn write_symbol_fingerprints(
        &self,
        symbol_db_ids: &[i64],
        fingerprints: &[(usize, clones::SymbolFingerprint)],
    ) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let normalizer_kind = clones::NormalizerKind::Baseline.as_db_str();
        for (local_index, fp) in fingerprints {
            let symbol_id = symbol_db_ids[*local_index];
            conn.prepare_cached(
                "INSERT INTO symbol_fingerprints(symbol_id, normalizer_kind, normalizer_version, \
                 oracle_run_id, struct_hash, token_len, created_at_ms)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6)",
            )?
            .execute(params![
                symbol_id,
                normalizer_kind,
                clones::NORM_VERSION,
                fp.struct_hash,
                fp.token_len,
                now_ms(),
            ])?;
            // Write the inverted index: one posting row per distinct token, and bump the global
            // document frequency for that token (#215). df is a selectivity hint only (the
            // candidate read LEFT-JOINs + COALESCEs it), so the incremental/heal bump
            // here is drift-tolerated; a full rebuild recomputes df authoritatively
            // from postings (see rebuild.rs).
            for &(token_hash, freq) in &fp.token_bag {
                conn.prepare_cached(
                    "INSERT INTO symbol_token_postings(symbol_id, normalizer_kind, token_hash, \
                     freq)
                     VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![symbol_id, normalizer_kind, token_hash, freq])?;
                conn.prepare_cached(
                    "INSERT INTO clone_token_df(normalizer_kind, token_hash, df)
                     VALUES (?1, ?2, 1)
                     ON CONFLICT(normalizer_kind, token_hash) DO UPDATE SET df = df + 1",
                )?
                .execute(params![normalizer_kind, token_hash])?;
            }
        }
        Ok(())
    }

    /// Serial insert: pure prepared-statement stepping. The text hash, relocation anchor, and
    /// embedding policy were all computed in the parallel prepare phase (see `prepare_chunks`), so
    /// nothing here hashes or parses.
    ///
    /// Writes the per-row `chunk_fts` token row inline on EVERY path. `chunk_fts` is contentless
    /// (#77 Phase 2), so it cannot be bulk-rebuilt from a content column — the only way it stays in
    /// sync is this inline write at index time (full rebuild, incremental, and heal all insert
    /// here). The full rebuild's `finalize_full_rebuild_fts` only bulk-rebuilds the separate
    /// external-content `commit_fts`; `chunk_fts` recovery lives in `rebuild_chunk_fts`
    /// (decompresses the store).
    fn insert_chunks(
        &self,
        file: ChunkInsertFile<'_>,
        chunks: &[PreparedChunk],
    ) -> anyhow::Result<()> {
        let ChunkInsertFile { file_id, source_revision } = file;
        // Maintain the compressed store inline when a dict exists (incremental + heal, and a full
        // rebuild after the first one). Compress against the LATEST dict version and record it on
        // the row — the dict is an immutable decode key (#77 Phase 2). When NO dict exists
        // (the very first full rebuild), there is nothing to compress against yet, so the
        // text is staged and build_chunk_text_store trains version 1 at the end. Load the
        // dict BEFORE taking `conn` to avoid a nested connection borrow. Empty dict bytes =
        // the no-dict (plain zstd) sentinel.
        let latest_dict = self.latest_chunk_text_dict()?;
        let mut compressor = latest_dict
            .as_ref()
            .map(|(_, dict)| text_compression::ChunkCompressor::new(dict))
            .transpose()?;
        let dict_version = latest_dict.as_ref().map(|(version, _)| *version);
        let conn = self.storage.connection();
        for prepared in chunks {
            let chunk = &prepared.chunk;
            let anchor = &prepared.anchor;
            conn.prepare_cached(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, \
                 start_line, end_line, text_hash,
                                    source_revision, anchor_version, normalized_hash, \
                 start_boundary_hash, end_boundary_hash,
                                    start_context_hash, end_context_hash, context_radius, \
                 embedding_policy, embedding_priority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18)",
            )?
            .execute(params![
                file_id,
                chunk.kind,
                chunk.symbol_path,
                i64::try_from(chunk.start_byte)?,
                i64::try_from(chunk.end_byte)?,
                i64::try_from(chunk.start_line)?,
                i64::try_from(chunk.end_line)?,
                prepared.text_hash,
                source_revision,
                anchor.version,
                anchor.normalized_hash,
                anchor.start_boundary_hash,
                anchor.end_boundary_hash,
                anchor.start_context_hash,
                anchor.end_context_hash,
                anchor.context_radius,
                prepared.embedding.policy,
                prepared.embedding.priority,
            ])?;
            let chunk_id = conn.last_insert_rowid();
            // chunk_fts is contentless (#77 Phase 2): tokens come from the in-memory text here, on
            // EVERY indexing path, never from a chunks.text column (a contentless FTS can't be
            // bulk-rebuilt from a content table, so the inline write is what keeps it in sync).
            conn.prepare_cached("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)")?
                .execute(params![chunk_id, chunk.text])?;
            match compressor.as_mut() {
                // Dict present: compress inline against its version into the durable store.
                Some(compressor) => {
                    let blob = compressor.compress(chunk.text.as_bytes())?;
                    conn.prepare_cached(
                        "INSERT INTO chunk_text(chunk_id, blob, raw_len, dict_version) VALUES \
                         (?1, ?2, ?3, ?4)",
                    )?
                    .execute(params![
                        chunk_id,
                        blob,
                        i64::try_from(chunk.text.len())?,
                        dict_version.expect("dict_version is Some whenever the compressor is"),
                    ])?;
                },
                // No dict yet (the FIRST full rebuild): stage the text in the rebuild temp table so
                // `build_chunk_text_store` trains version 1 over a corpus sample and compresses
                // every chunk at the end. There is no chunks.text column to read
                // from.
                None => {
                    conn.prepare_cached(
                        "INSERT INTO temp.rebuild_chunk_text(chunk_id, text) VALUES (?1, ?2)",
                    )?
                    .execute(params![chunk_id, chunk.text])?;
                },
            }
        }
        Ok(())
    }

    /// Inserts symbols and returns their assigned DB ids in the SAME order as `symbols`, so the
    /// caller can remap prepared edge candidates' local symbol indices to real ids.
    fn insert_symbols(
        &self,
        file_id: i64,
        language: Language,
        symbols: &[Symbol],
    ) -> anyhow::Result<Vec<i64>> {
        let conn = self.storage.connection();
        let mut symbol_ids = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            // Intern the qualified name into the shared `name_strings` pool and store its id (#224)
            // — the qualified_name TEXT column was dropped in V028. The pool is shared with edge
            // call-target names (~85% overlap), so most inserts hit an existing id.
            let qualified_name_id =
                crate::index::edges::intern_edge_string(conn, &symbol.qualified_name)?;
            conn.prepare_cached(
                "INSERT INTO symbols(file_id, language, name, qualified_name_id, scope_path, \
                 kind, start_byte, end_byte, start_line, end_line, signature, docs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?
            .execute(params![
                file_id,
                language.as_str(),
                symbol.name,
                qualified_name_id,
                symbol.scope_path,
                symbol.kind,
                i64::try_from(symbol.start_byte)?,
                i64::try_from(symbol.end_byte)?,
                i64::try_from(symbol.start_line)?,
                i64::try_from(symbol.end_line)?,
                symbol.signature,
                symbol.docs,
            ])?;
            let symbol_id = conn.last_insert_rowid();
            symbol_ids.push(symbol_id);
            for fact in &symbol.facts {
                conn.prepare_cached(
                    "INSERT OR IGNORE INTO symbol_facts(symbol_id, fact_kind, fact_value)
                     VALUES (?1, ?2, ?3)",
                )?
                .execute(params![symbol_id, fact.kind, fact.value])?;
            }
        }
        Ok(symbol_ids)
    }

    /// Insert one logical symbol and its members. Extracted so `rebuild_logical_symbols` can flush
    /// each group as its key changes (streaming) rather than buffering every group. Both INSERTs
    /// are `prepare_cached` — the member insert runs once per symbol, so recompiling the SQL each
    /// call would dominate.
    pub(super) fn insert_logical_group(
        conn: &rusqlite::Connection,
        key: &LogicalSymbolKey,
        members: &[LogicalSymbolMemberRow],
    ) -> anyhow::Result<()> {
        let group_reason = if members.len() > 1 { "cfg_variant" } else { "single" };
        let logical_symbol_id = key.stable_id();
        // Intern the qualified name into the shared `name_strings` pool (#224) — the
        // qualified_name TEXT column was dropped in V028.
        let qualified_name_id = crate::index::edges::intern_edge_string(conn, &key.qualified_name)?;
        conn.prepare_cached(
            "
            INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind, \
             variant_count, group_reason)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
        )?
        .execute(params![
            logical_symbol_id,
            key.language,
            key.path,
            key.name,
            qualified_name_id,
            key.kind,
            i64::try_from(members.len()).unwrap_or(i64::MAX),
            group_reason,
        ])?;
        for member in members {
            let signature_hash =
                member.signature.as_deref().map(|signature| hex_sha256(signature.as_bytes()));
            conn.prepare_cached(
                "
                INSERT INTO logical_symbol_members(
                    logical_symbol_id, symbol_id, cfg_expr, signature_hash, start_line, end_line
                )
                VALUES (?1, ?2, NULL, ?3, ?4, ?5)
                ",
            )?
            .execute(params![
                logical_symbol_id,
                member.symbol_id,
                signature_hash,
                member.start_line,
                member.end_line,
            ])?;
        }
        Ok(())
    }
}
