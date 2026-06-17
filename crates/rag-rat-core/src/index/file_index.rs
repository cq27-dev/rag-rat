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
        let file_id = self.storage.connection().query_row(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
             indexed_at_ms, indexed_revision, commit_sha, worktree_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
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
            ],
            |row| row.get::<_, i64>(0),
        )?;
        let chunks = if kind == TargetKind::Generated {
            chunker::generated_chunks_for_file(path, text)
        } else {
            chunker::chunks_for_file(path, language, text)
        };
        let chunks = prepare_chunks(path, language.as_str(), kind.as_str(), chunks, text);
        let symbols =
            if kind == TargetKind::Generated || text.len() > chunker::MAX_STRUCTURAL_PARSE_BYTES {
                Vec::new()
            } else {
                symbols::symbols_for_file(path, language, text)
            };
        // Inline/heal path: keep chunk_fts in sync per row (partial file replace would otherwise
        // desync the external-content index until the next forced rebuild).
        self.insert_chunks(ChunkInsertFile { file_id, source_revision: &sha256 }, &chunks, true)?;
        self.insert_symbols(file_id, language, &symbols)?;
        if kind != TargetKind::Generated && text.len() <= edges::MAX_GRAPH_PARSE_BYTES {
            edges::index_file_edges(self.storage.connection(), file_id, path, language, text)?;
        }
        self.mark_fts_dirty()?;
        Ok(())
    }

    /// `write_fts`: false on a full rebuild (the closing bulk `rebuild_fts` repopulates
    /// `chunk_fts`), true on incremental discovery (per-file replace needs the external-content
    /// index kept in sync in place). See `insert_chunks`.
    /// `graph`: on a full rebuild, `Some` accumulator — symbols (with their new DB ids) and
    /// remapped edge candidates are collected for one in-memory resolve-and-insert pass after
    /// the loop, and NO edges are inserted here. `None` (incremental) inserts edges unresolved
    /// per file as before, to be resolved by the DB-based `resolve_edges`.
    pub(super) fn insert_prepared_file(
        &self,
        prepared_file: &PreparedIndexFile,
        write_fts: bool,
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
        // prepare_cached so the per-file/-chunk/-symbol INSERTs compile once per connection and
        // are reused across the whole rebuild instead of re-parsing the SQL on every row (#57).
        let file_id = self
            .storage
            .connection()
            .prepare_cached(
                "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
                 indexed_at_ms, indexed_revision, commit_sha, worktree_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
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
                ],
                |row| row.get::<_, i64>(0),
            )?;
        self.insert_chunks(
            ChunkInsertFile { file_id, source_revision: &prepared.sha256 },
            &prepared.chunks,
            write_fts,
        )?;
        let symbol_db_ids = self.insert_symbols(file_id, file.language, &prepared.symbols)?;
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

    /// Serial insert: pure prepared-statement stepping. The text hash, relocation anchor, and
    /// embedding policy were all computed in the parallel prepare phase (see `prepare_chunks`), so
    /// nothing here hashes or parses.
    ///
    /// `write_fts` controls the per-row `chunk_fts` insert. On a FULL REBUILD it's `false`: the
    /// rebuild empties `chunk_fts` and the closing `rebuild_fts` repopulates it from the content
    /// table in one bulk pass, so per-row writes are pure double work. Incremental / heal paths
    /// pass `true`: they delete and re-insert individual files, which would leave `chunks` rows
    /// without `chunk_fts` shadow entries (an external-content desync, #51) until a query
    /// forces a rebuild — the per-row insert keeps the index consistent in place.
    fn insert_chunks(
        &self,
        file: ChunkInsertFile<'_>,
        chunks: &[PreparedChunk],
        write_fts: bool,
    ) -> anyhow::Result<()> {
        let ChunkInsertFile { file_id, source_revision } = file;
        let conn = self.storage.connection();
        for prepared in chunks {
            let chunk = &prepared.chunk;
            let anchor = &prepared.anchor;
            conn.prepare_cached(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, \
                 start_line, end_line, text, text_hash,
                                    source_revision, anchor_version, normalized_hash, \
                 start_boundary_hash, end_boundary_hash,
                                    start_context_hash, end_context_hash, context_radius, \
                 embedding_policy, embedding_priority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18, ?19)",
            )?
            .execute(params![
                file_id,
                chunk.kind,
                chunk.symbol_path,
                i64::try_from(chunk.start_byte)?,
                i64::try_from(chunk.end_byte)?,
                i64::try_from(chunk.start_line)?,
                i64::try_from(chunk.end_line)?,
                chunk.text,
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
            if write_fts {
                let chunk_id = conn.last_insert_rowid();
                conn.prepare_cached("INSERT INTO chunk_fts(rowid, text) VALUES (?1, ?2)")?
                    .execute(params![chunk_id, chunk.text])?;
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
            conn.prepare_cached(
                "INSERT INTO symbols(file_id, language, name, qualified_name, scope_path, kind, \
                 start_byte, end_byte, start_line, end_line, signature, docs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?
            .execute(params![
                file_id,
                language.as_str(),
                symbol.name,
                symbol.qualified_name,
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
        conn.prepare_cached(
            "
            INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name, kind, \
             variant_count, group_reason)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
        )?
        .execute(params![
            logical_symbol_id,
            key.language,
            key.path,
            key.name,
            key.qualified_name,
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
