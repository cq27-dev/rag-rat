use super::*;

/// Identity of the file whose chunks are being inserted — passed from the caller (which just
/// inserted the file row) so `insert_chunks` doesn't re-`SELECT` it per file (#57).
struct ChunkInsertFile<'a> {
    file_id: i64,
    source_revision: &'a str,
}

impl IndexDatabase {
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        schema::rebuild_fts(self.storage.connection())?;
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_meta("fts_dirty", "false")?;
        Ok(())
    }

    pub fn sync_fts(&self) -> anyhow::Result<()> {
        self.record_content_revision()?;
        self.record_fts_current()?;
        self.set_meta("fts_dirty", "false")?;
        Ok(())
    }

    fn record_fts_current(&self) -> anyhow::Result<()> {
        self.set_meta("fts_synced_at_ms", &now_ms().to_string())?;
        let revision = self.content_revision()?;
        self.set_meta("fts_source_revision", &revision)?;
        Ok(())
    }

    fn record_content_revision(&self) -> anyhow::Result<String> {
        let revision = self.content_revision()?;
        self.set_meta("content_revision", &revision)?;
        Ok(revision)
    }

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
                matches!(kind, TargetKind::Generated),
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
                    matches!(file.kind, TargetKind::Generated),
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
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte, \
                 end_byte, start_line, end_line, signature, docs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?
            .execute(params![
                file_id,
                language.as_str(),
                symbol.name,
                symbol.qualified_name,
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

    /// Write a meta key only if the stored value differs. Returns whether a write happened — so a
    /// no-change incremental/sweep pass can avoid dirtying a WAL page (see issue #63).
    pub(super) fn set_meta_if_changed(&self, key: &str, value: &str) -> anyhow::Result<bool> {
        if self.meta(key)?.as_deref() == Some(value) {
            return Ok(false);
        }
        self.set_meta(key, value)?;
        Ok(true)
    }

    /// Refresh `git_commit` / `git_dirty` meta, writing only the keys that actually changed.
    /// Returns whether either was written (so the caller can tell a no-op pass from a real one).
    pub(super) fn write_git_meta(&self, root: &Path) -> anyhow::Result<bool> {
        let commit = git_output(root, &["rev-parse", "HEAD"]).unwrap_or_default();
        let dirty = !git_output(root, &["status", "--porcelain"]).unwrap_or_default().is_empty();
        let commit_changed = self.set_meta_if_changed("git_commit", &commit)?;
        let dirty_changed =
            self.set_meta_if_changed("git_dirty", if dirty { "true" } else { "false" })?;
        Ok(commit_changed || dirty_changed)
    }

    /// O(1) check of whether the indexed git history is still current for `root`'s HEAD, so the
    /// incremental path can skip the full `git log` re-read + table wipe. See
    /// [`git_history::is_history_current`].
    pub(super) fn git_history_is_current(&self, root: &Path) -> bool {
        git_history::is_history_current(self.storage.connection(), root)
    }

    pub(super) fn apply_prepared_git_history(
        &self,
        root: &Path,
        handle: JoinHandle<anyhow::Result<git_history::PreparedGitHistory>>,
    ) -> anyhow::Result<GitHistoryIndexStatus> {
        let prepared = join_git_history_prepare(handle)?;
        git_history::apply_prepared(self.storage.connection(), root, prepared)
    }

    pub(super) fn git_history_status(&self) -> anyhow::Result<GitHistoryIndexStatus> {
        let Some(root) = self.storage.source_root() else {
            return git_history::status(self.storage.connection(), Path::new("."));
        };
        git_history::status(self.storage.connection(), root)
    }

    pub(super) fn github_status(&self) -> anyhow::Result<GitHubStatus> {
        github::status(self.storage.connection(), &self.github)
    }

    pub(super) fn mark_fts_dirty(&self) -> anyhow::Result<()> {
        self.set_meta("fts_dirty", "true")
    }

    pub(super) fn resolve_edges(&self) -> anyhow::Result<()> {
        edges::resolve_all_edges(self.storage.connection())
    }

    pub(super) fn rebuild_logical_symbols(&self) -> anyhow::Result<()> {
        // The insert below re-derives the COMPLETE logical-symbol table from all current symbols,
        // so clear it entirely first. A member-join "rebuild set" misses logical_symbols whose
        // members were cascade-deleted with their symbols (clear_full_rebuild_tables deletes
        // files → symbols → logical_symbol_members via FK, but logical_symbols has no such FK).
        // Those orphans would then collide with the deterministic stable id on re-insert.
        self.storage.connection().execute_batch(
            "
            DELETE FROM main.logical_symbol_members;
            DELETE FROM main.logical_symbols;
            ",
        )?;

        // STREAM the grouping instead of materializing every symbol. The previous version built a
        // `BTreeMap<LogicalSymbolKey, Vec<row>>` over ALL symbols — at kernel scale ~3.5M rows ×
        // six owned `String`s each (plus a cloned key per group). That structure, allocated in the
        // trailing rebuild phase AFTER the edge accumulator is freed, was the dominant full-rebuild
        // peak-RSS spike (~6 GB transient at the very end of a whole-kernel index). Ordering the
        // SELECT by the key's `Ord` (language, path, name, qualified_name, kind, signature — SQLite
        // ASC sorts NULL first, matching Rust `None < Some`) then the within-group member order
        // (start_byte, end_byte, which the old per-group Vec preserved) makes each group's rows
        // arrive contiguously, so we flush a group the moment its key changes and hold only the
        // current group's members (kilobytes). Byte-identical: same grouping, same
        // `logical_symbols` insert order (ids are content-derived via `stable_id`, not rowids), and
        // the same member order, verified against the golden index.
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(
            "
            SELECT symbols.id, files.path, symbols.language, symbols.name, symbols.qualified_name,
                   symbols.kind, symbols.signature, symbols.start_line, symbols.end_line
            FROM symbols
            JOIN files ON files.id = symbols.file_id
            ORDER BY symbols.language, files.path, symbols.name, symbols.qualified_name,
                     symbols.kind, symbols.signature, symbols.start_byte, symbols.end_byte
            ",
        )?;
        let mut rows = stmt.query([])?;
        let mut current: Option<(LogicalSymbolKey, Vec<LogicalSymbolMemberRow>)> = None;
        while let Some(row) = rows.next()? {
            let member = LogicalSymbolMemberRow {
                symbol_id: row.get(0)?,
                path: row.get(1)?,
                language: row.get(2)?,
                name: row.get(3)?,
                qualified_name: row.get(4)?,
                kind: row.get(5)?,
                signature: row.get(6)?,
                start_line: row.get(7)?,
                end_line: row.get(8)?,
            };
            // Compare the member's key fields against the current group WITHOUT allocating a key
            // per row (only per group, on a boundary).
            let same_group = current.as_ref().is_some_and(|(key, _)| {
                key.language == member.language
                    && key.path == member.path
                    && key.name == member.name
                    && key.qualified_name == member.qualified_name
                    && key.kind == member.kind
                    && key.signature == member.signature
            });
            if same_group {
                current.as_mut().expect("same_group implies Some").1.push(member);
            } else {
                if let Some((key, members)) = current.take() {
                    Self::insert_logical_group(conn, &key, &members)?;
                }
                let key = LogicalSymbolKey::from(&member);
                current = Some((key, vec![member]));
            }
        }
        if let Some((key, members)) = current.take() {
            Self::insert_logical_group(conn, &key, &members)?;
        }
        Ok(())
    }

    /// Insert one logical symbol and its members. Extracted so `rebuild_logical_symbols` can flush
    /// each group as its key changes (streaming) rather than buffering every group. Both INSERTs
    /// are `prepare_cached` — the member insert runs once per symbol, so recompiling the SQL each
    /// call would dominate.
    fn insert_logical_group(
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

    pub(super) fn graph_coverage(
        &self,
        paths: BTreeSet<String>,
    ) -> anyhow::Result<crate::query::graph::GraphCoverage> {
        let indexed_files =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        let parser_failure_paths = self.parser_failure_paths()?;
        let parser_failures = u64::try_from(parser_failure_paths.len()).unwrap_or(0);
        let known_index_gaps = parser_failure_paths
            .iter()
            .map(|failure| {
                format!(
                    "{} parser failed for {}: {}",
                    failure.language, failure.path, failure.message
                )
            })
            .collect::<Vec<_>>();
        let mut stale_files = 0_u64;
        let mut parser_coverage_for_paths = Vec::new();
        for path in paths {
            let Some(row) = self.graph_path_row(&path)? else {
                parser_coverage_for_paths.push(crate::query::graph::GraphPathCoverage {
                    path,
                    language: "unknown".to_string(),
                    parser_status: "missing_from_index".to_string(),
                    graph_status: "missing_from_index".to_string(),
                    last_indexed_revision: None,
                });
                continue;
            };
            let stale = self.source_path_is_stale(&path, &row.sha256);
            if stale {
                stale_files += 1;
            }
            let parser_failed = parser_failure_paths.iter().any(|failure| failure.path == path);
            parser_coverage_for_paths.push(crate::query::graph::GraphPathCoverage {
                path,
                language: row.language,
                parser_status: if parser_failed { "failed" } else { "ok" }.to_string(),
                graph_status: if stale {
                    "stale_source"
                } else if parser_failed {
                    "parser_failed"
                } else {
                    "ok"
                }
                .to_string(),
                last_indexed_revision: (!row.indexed_revision.is_empty())
                    .then_some(row.indexed_revision),
            });
        }
        Ok(crate::query::graph::GraphCoverage {
            indexed_files: u64::try_from(indexed_files).unwrap_or(0),
            parser_failures,
            stale_files,
            known_index_gaps,
            parser_coverage_for_paths,
        })
    }

    fn graph_path_row(&self, path: &str) -> anyhow::Result<Option<GraphPathRow>> {
        self.storage
            .connection()
            .query_row(
                "SELECT language, sha256, indexed_revision FROM files WHERE path = ?1",
                [path],
                |row| {
                    Ok(GraphPathRow {
                        language: row.get(0)?,
                        sha256: row.get(1)?,
                        indexed_revision: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    fn source_path_is_stale(&self, path: &str, indexed_sha256: &str) -> bool {
        let Some(root) = self.storage.source_root() else {
            return false;
        };
        let Ok(bytes) = fs::read(root.join(path)) else {
            return true;
        };
        hex_sha256(&bytes) != indexed_sha256
    }

    pub(super) fn regex_hits(
        &self,
        pattern: &str,
        regex: &Regex,
        include_tests: bool,
    ) -> anyhow::Result<Vec<crate::query::graph::TextOnlyHit>> {
        let Some(root) = self.storage.source_root() else {
            anyhow::bail!("cannot compare graph to text: source_root is missing from index_meta");
        };
        let mut stmt = self.storage.connection().prepare("SELECT path FROM files ORDER BY path")?;
        let paths =
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        let mut hits = Vec::new();
        for path in paths {
            if !include_tests && is_test_like_path(&path) {
                continue;
            }
            let full_path = root.join(&path);
            let Ok(text) = fs::read_to_string(&full_path) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    hits.push(crate::query::graph::TextOnlyHit {
                        path: path.clone(),
                        line: i64::try_from(index + 1).unwrap_or(i64::MAX),
                        text: line.trim().to_string(),
                        reason: "text pattern matched".to_string(),
                        likely_gap: pattern.to_string(),
                    });
                }
            }
        }
        Ok(hits)
    }

    pub(super) fn current_line_text(
        &self,
        path: &str,
        line: i64,
    ) -> anyhow::Result<Option<String>> {
        let Some(root) = self.storage.source_root() else {
            return Ok(None);
        };
        let Ok(text) = fs::read_to_string(root.join(path)) else {
            return Ok(None);
        };
        let Some(index) = usize::try_from(line.saturating_sub(1)).ok() else {
            return Ok(None);
        };
        Ok(text.lines().nth(index).map(|line| line.trim().to_string()))
    }

    pub(super) fn ensure_graph_index_current(&self) -> anyhow::Result<()> {
        if self.meta("graph_index_version")?.as_deref() == Some(GRAPH_INDEX_VERSION) {
            return Ok(());
        }
        let Some(root) = self.storage.source_root().map(Path::to_path_buf) else {
            return Ok(());
        };
        self.storage.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| -> anyhow::Result<()> {
            self.storage.connection().execute("DELETE FROM edges_data", [])?;
            let files = self.graph_reindex_files()?;
            for file in files {
                if file.kind == TargetKind::Generated || file.language == Language::Markdown {
                    continue;
                }
                let full_path = root.join(&file.path);
                let Ok(text) = fs::read_to_string(full_path) else {
                    continue;
                };
                if text.len() > edges::MAX_GRAPH_PARSE_BYTES {
                    continue;
                }
                edges::index_file_edges(
                    self.storage.connection(),
                    file.id,
                    Path::new(&file.path),
                    file.language,
                    &text,
                )?;
            }
            self.resolve_edges()?;
            self.mark_graph_index_current()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.storage.execute_batch("ROLLBACK");
        }
        result?;
        self.storage.execute_batch("COMMIT")?;
        Ok(())
    }

    pub(super) fn mark_graph_index_current(&self) -> anyhow::Result<()> {
        self.set_meta("graph_index_version", GRAPH_INDEX_VERSION)
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
        meta_for(self.storage.connection(), key)
    }

    fn insert_parser_failure(
        &self,
        path: &Path,
        language: Language,
        message: &str,
    ) -> anyhow::Result<()> {
        self.storage.connection().execute(
            "INSERT INTO parser_failures(path, language, message) VALUES (?1, ?2, ?3)",
            params![path_string(path), language.as_str(), message],
        )?;
        Ok(())
    }

    pub(super) fn parser_failure_count(&self) -> anyhow::Result<u64> {
        let count = self.storage.connection().query_row(
            "SELECT COUNT(*) FROM parser_failures",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub(super) fn parser_failure_paths(&self) -> anyhow::Result<Vec<ParserFailure>> {
        let mut stmt = self.storage.connection().prepare(
            "SELECT path, language, message FROM parser_failures ORDER BY path, language, message",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ParserFailure { path: row.get(0)?, language: row.get(1)?, message: row.get(2)? })
        })?;
        let mut failures = Vec::new();
        for row in rows {
            failures.push(row?);
        }
        Ok(failures)
    }

    pub(super) fn search_with_heal(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
        allow_heal: bool,
        explain: bool,
        options: SearchOptions,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let hits = crate::search::lexical::search_with_options(
            self.storage.connection(),
            query,
            limit,
            include_generated,
            explain,
            options,
        )?;
        if !allow_heal {
            return Ok(hits);
        }
        let stale = self.stale_hit_paths(&hits)?;
        if stale.is_empty() {
            return Ok(hits);
        }
        if stale.len() > MAX_AUTO_HEAL_FILES_PER_CALL {
            anyhow::bail!(IndexError::NeedsReindex {
                stale_files: stale.len(),
                cap: MAX_AUTO_HEAL_FILES_PER_CALL,
            });
        }
        for path in stale {
            self.heal_file(Path::new(&path))?;
        }
        self.sync_fts()?;
        self.search_with_heal(query, limit, include_generated, false, explain, options)
    }

    fn stale_hit_paths(&self, hits: &[SearchHit]) -> anyhow::Result<Vec<String>> {
        let Some(root) = self.storage.source_root() else {
            return Ok(Vec::new());
        };
        let mut stale = Vec::new();
        let mut seen = BTreeSet::new();
        for hit in hits {
            if !seen.insert(hit.path.clone()) {
                continue;
            }
            let source_path = root.join(&hit.path);
            let Ok(text) = fs::read_to_string(source_path) else {
                stale.push(hit.path.clone());
                continue;
            };
            let chunk = crate::query::read_chunk(self.storage.connection(), hit.chunk_id)?;
            let Some(chunk) = chunk else {
                stale.push(hit.path.clone());
                continue;
            };
            let anchor = self.chunk_anchor(hit.chunk_id)?;
            let status = anchors::validate(
                &chunk.text,
                usize::try_from(chunk.start_line).unwrap_or(1),
                usize::try_from(chunk.end_line).unwrap_or(1),
                &anchor,
                &text,
            );
            if !matches!(status, AnchorStatus::Exact) {
                stale.push(hit.path.clone());
            }
        }
        Ok(stale)
    }

    pub(super) fn chunk_anchor(&self, chunk_id: i64) -> anyhow::Result<ChunkAnchor> {
        Ok(self.storage.connection().query_row(
            "
            SELECT anchor_version, normalized_hash, start_boundary_hash, end_boundary_hash,
                   start_context_hash, end_context_hash, context_radius
            FROM chunks WHERE id = ?1
            ",
            [chunk_id],
            |row| {
                Ok(ChunkAnchor {
                    version: row.get(0)?,
                    normalized_hash: row.get(1)?,
                    start_boundary_hash: row.get(2)?,
                    end_boundary_hash: row.get(3)?,
                    start_context_hash: row.get(4)?,
                    end_context_hash: row.get(5)?,
                    context_radius: row.get(6)?,
                })
            },
        )?)
    }

    pub(super) fn mark_file_deleted(&self, path: &Path) -> anyhow::Result<()> {
        let path = path_string(path);
        self.remove_file_in_scope(Path::new(&path), "", &self.active_worktree_id)?;
        self.storage.connection().execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, generated, \
             indexed_at_ms, indexed_revision, commit_sha, worktree_id)
             VALUES (?1, 'unknown', 'deleted', '', 0, 0, ?2, '', '', ?3)
             ON CONFLICT(path, commit_sha, worktree_id) DO UPDATE SET
                kind = 'deleted',
                sha256 = '',
                modified_at_ms = 0,
                indexed_at_ms = excluded.indexed_at_ms",
            params![path, now_ms(), self.active_worktree_id],
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
        let path = path_string(path);
        // Direct edges_data writes (#79): these statements touch up to every in-edge of a file's
        // symbols, so they must not pay the view triggers' per-row dictionary probes.
        // 'NameOnly' is the EdgeConfidence demotion the resolver applies to a target-less edge.
        let name_only_id = edges::intern_edge_string(self.storage.connection(), "NameOnly")?;
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
             )",
            params![path, commit_sha, worktree_id, name_only_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM edges_data
             WHERE source_file_id IN (
                    SELECT id FROM main.files
                    WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
                )
                OR from_symbol_id IN (
                    SELECT symbols.id FROM symbols
                    JOIN main.files ON main.files.id = symbols.file_id
                    WHERE main.files.path = ?1
                      AND main.files.commit_sha = ?2
                      AND main.files.worktree_id = ?3
                )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage
            .connection()
            .execute("DELETE FROM parser_failures WHERE path = ?1", [&path])?;
        self.storage.connection().execute(
            "DELETE FROM chunk_fts
             WHERE rowid IN (
                 SELECT chunks.id FROM chunks
                 JOIN main.files ON main.files.id = chunks.file_id
                 WHERE main.files.path = ?1
                   AND main.files.commit_sha = ?2
                   AND main.files.worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        // Deleting the chunks cascades (ON DELETE CASCADE, foreign_keys=ON) to git_chunk_blame,
        // chunk_embeddings, and chunk_summaries — so the gate skipping the full git-history wipe
        // does NOT leak blame. (`docs` has no FK and is not cleaned here — a pre-existing gap,
        // tracked separately.)
        self.storage.connection().execute(
            "DELETE FROM chunks
             WHERE file_id IN (
                SELECT id FROM main.files
                WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM symbols
             WHERE file_id IN (
                SELECT id FROM main.files
                WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3
             )",
            params![path, commit_sha, worktree_id],
        )?;
        self.storage.connection().execute(
            "DELETE FROM main.files WHERE path = ?1 AND commit_sha = ?2 AND worktree_id = ?3",
            params![path, commit_sha, worktree_id],
        )?;
        self.mark_fts_dirty()?;
        Ok(())
    }

    pub(super) fn ensure_fts_fresh(&self) -> anyhow::Result<()> {
        let content_revision = self.content_revision()?;
        let fts_source_revision = self.meta("fts_source_revision")?;
        if !self.fts_dirty()? && fts_source_revision.as_deref() == Some(content_revision.as_str()) {
            return Ok(());
        }
        self.rebuild_fts()?;
        let refreshed_revision = self.meta("fts_source_revision")?;
        if refreshed_revision.as_deref() != Some(content_revision.as_str()) {
            anyhow::bail!(
                "FTS freshness invariant failed: content_revision={content_revision}, \
                 fts_source_revision={}",
                refreshed_revision.unwrap_or_else(|| "<missing>".to_string())
            );
        }
        Ok(())
    }

    pub(super) fn fts_dirty(&self) -> anyhow::Result<bool> {
        Ok(self.meta("fts_dirty")?.as_deref() == Some("true"))
    }

    fn file_row(&self, path: &Path) -> anyhow::Result<FileRow> {
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

    fn graph_reindex_files(&self) -> anyhow::Result<Vec<GraphReindexFile>> {
        let mut stmt = self
            .storage
            .connection()
            .prepare("SELECT id, path, language, kind FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            let language: String = row.get(2)?;
            let kind: String = row.get(3)?;
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, language, kind))
        })?;
        let mut files = Vec::new();
        for row in rows {
            let (id, path, language, kind) = row?;
            files.push(GraphReindexFile {
                id,
                path,
                language: language.parse()?,
                kind: kind.parse()?,
            });
        }
        Ok(files)
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

    pub(super) fn indexed_file_count(&self) -> anyhow::Result<usize> {
        let count =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub(super) fn content_revision(&self) -> anyhow::Result<String> {
        let value = self.storage.connection().query_row(
            "SELECT COALESCE(string_agg(path || ':' || sha256, ',' ORDER BY path), '') FROM files",
            [],
            |row| row.get::<_, String>(0),
        )?;
        Ok(hex_sha256(value.as_bytes()))
    }
}
