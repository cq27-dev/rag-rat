//! The file → rows indexing pipeline: parse/chunk/symbol one file and write its chunk,
//! symbol, and logical-group rows; heal a stale file in place.

use rag_rat_base::hash::hex_sha256;
use rag_rat_base::paths::path_string;
use rag_rat_base::time::now_ms;
use rag_rat_clones as clones;

use super::*;
use crate::index::graph_index::{LogicalSymbolKey, LogicalSymbolMemberRow};

/// Identity of the file whose chunks are being inserted — passed from the caller (which just
/// inserted the file row) so `insert_chunks` doesn't re-`SELECT` it per file (#57).
struct ChunkInsertFile<'a> {
    file_id: i64,
    source_revision: &'a str,
}

/// Whether `write_symbol_fingerprints` should bump the LIVE `clone_token_df` per token (#215,
/// restored by #479). Named so the call site reads its intent: `BumpDf(false)` on the
/// full-rebuild path (df is recomputed authoritatively at finalize, so the per-token bump is
/// wasted work), `BumpDf(true)` on the incremental/heal path (no finalize runs there, so the
/// bump keeps the live df current).
struct BumpDf(bool);

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
        // Read the file bytes + git status BEFORE opening the mutation transaction: pure I/O
        // holds no SQLite write lock, keeping the write window short.
        let text = match fs::read_to_string(&full_path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let changes = git_changed_paths(root).unwrap_or_default();

        // #767 review: run the whole heal mutation in ONE IMMEDIATE transaction with the removal
        // tombstone re-checked INSIDE it. A preflight-only check leaves a gap between the check
        // and the delete/reinsert below in which `rag-rat rm` could purge + tombstone, letting a
        // stale MCP/lazy heal recreate `files`/chunk/graph rows for the removed `repo_id` after
        // rm reported success (`files.repo_id` has no FK to `repos`). This transaction and rm's
        // purge are both IMMEDIATE, so they serialize on the SQLite write lock and the tombstone
        // state read here is stable for the whole heal: set → rm committed first, fail closed;
        // unset → rm's purge waits for this commit and sweeps the re-inserted rows itself. The
        // heal deliberately stays LOCKLESS at the flock level — it must run alongside a
        // mid-flight rebuild (`a_lockless_heal_mid_rebuild_does_not_remove_the_staged_row`).
        let conn = self.storage.connection();
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let active_repo_id = rag_rat_db::schema::active_repo_id(&tx)?;
        super::remove::assert_repo_not_removed(&tx, &active_repo_id)?;

        let Some(text) = text else {
            // File deleted on disk since indexing — drop it from the index rather than
            // hard-erroring the whole search. The caller (search_with_heal) re-runs
            // search without it. Mirrors read_chunk_current's deletion handling.
            self.mark_file_deleted(path)?;
            tx.commit()?;
            return Ok(());
        };

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
        // Defer: a single-file heal must not stamp the logical-key version — every other file's
        // drift is still in the future (#493).
        self.rebuild_logical_symbols(graph_index::KeyVersionStamp::Defer)?;
        self.resolve_edges()?;
        tx.commit()?;
        Ok(())
    }

    /// The deletion half of a heal, gated exactly like [`Self::heal_file`] (#767 review): the
    /// `kind='deleted'` tombstone is itself an INSERT stamped with the active `repo_id`, so a
    /// stale writer must not write it for a repo `rag-rat rm` already purged. Wraps the deletion
    /// in an IMMEDIATE transaction with the removal tombstone re-checked inside (the transaction
    /// serializes with rm's purge on the SQLite write lock — see `heal_file`).
    pub(crate) fn mark_file_deleted_if_not_removed(&self, path: &Path) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let active_repo_id = rag_rat_db::schema::active_repo_id(&tx)?;
        super::remove::assert_repo_not_removed(&tx, &active_repo_id)?;
        self.mark_file_deleted(path)?;
        tx.commit()?;
        Ok(())
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
        // Heal path: prepare from the bytes already in hand through the SAME single-parse core the
        // full-rebuild / changed-file passes use, then route through `insert_prepared_file` in
        // incremental mode (`graph = None`). ONE tree-sitter parse instead of 5, and no duplicated
        // generated-file gate / parser-failure / chunk-source / has_test_code logic (#518).
        // `heal_file`'s `remove_file_in_scope` already cleared the prior file row and its parser
        // failure, so the incremental insert writes the live generation in place; `full_path` is
        // unused by the insert (the bytes are already parsed).
        let content = prepare_index_content_from_text(path, language, kind, text, modified_at_ms);
        let prepared_file = PreparedIndexFile {
            file: IndexFile {
                full_path: path.to_path_buf(),
                relative_path: path.to_path_buf(),
                language,
                kind,
                commit_sha: scope.commit_sha.clone(),
                worktree_id: scope.worktree_id.clone(),
            },
            prepared: Ok(content),
        };
        self.insert_prepared_file(&prepared_file, None)
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
        // Parser-failure routing (A6, P2 review): the FULL REBUILD (`graph.is_some()` — the same
        // mode split the edge accumulator uses) STAGES its upserts/clears into the connection's
        // temp table, published atomically with the flip by `apply_staged_parser_failures`. The
        // per-wave commits land BEFORE the flip, so a direct write here would expose (and, on a
        // tail failure, strand) an UNPUBLISHED generation's failure state to readers still scoped
        // to the old generation. Incremental passes (`None`) write the LIVE generation in place,
        // so they keep the direct upsert/clear (their `remove_file_in_scope` already cleared; the
        // clear branch is the full-rebuild's clean-reparse path, a no-op for them).
        let staged_rebuild = graph.is_some();
        let prepared = match &prepared_file.prepared {
            Ok(prepared) => prepared,
            Err(err) => {
                if staged_rebuild {
                    self.stage_parser_failure(
                        &file.relative_path,
                        file.language,
                        Some(&err.to_string()),
                    )?;
                } else {
                    self.insert_parser_failure(
                        &file.relative_path,
                        file.language,
                        &err.to_string(),
                    )?;
                }
                return Ok(());
            },
        };
        if let Some(message) = &prepared.parser_failure {
            if staged_rebuild {
                self.stage_parser_failure(&file.relative_path, file.language, Some(message))?;
            } else {
                self.insert_parser_failure(&file.relative_path, file.language, message)?;
            }
        } else if staged_rebuild {
            self.stage_parser_failure(&file.relative_path, file.language, None)?;
        } else {
            self.clear_parser_failure(&file.relative_path)?;
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
                 indexed_at_ms, indexed_revision, commit_sha, worktree_id, has_test_code, \
                 repo_id, generation, graph_version, scope_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, CAST(?14 AS \
                 INTEGER), CAST(?15 AS INTEGER))
                 RETURNING id",
            )?
            .query_row(
                params![
                    path,
                    file.language.as_str(),
                    file.kind.as_str(),
                    prepared.sha256,
                    prepared.modified_at_ms,
                    rag_rat_base::path_class::file_is_generated(file.kind, &path),
                    now_ms(),
                    prepared.sha256,
                    file.commit_sha,
                    file.worktree_id,
                    has_test_code,
                    self.active_repo_id,
                    // A6: the WRITE generation of this pass — N+1 for a full rebuild (staged
                    // alongside the live generation, made live by the flip), the live generation
                    // for incremental (written in place).
                    self.active_generation,
                    GRAPH_INDEX_VERSION,
                    LOGICAL_KEY_VERSION,
                ],
                |row| row.get::<_, i64>(0),
            )?;
        // Insert symbols FIRST so their freshly assigned rowids are in hand to stamp on each code
        // chunk's `symbol_id` (the direct chunk→symbol link, #855/#860). Chunks and symbols are
        // independent INSERTs into their own tables, so the order is free — nothing between the two
        // depends on chunks preceding symbols.
        let symbol_db_ids = self.insert_symbols(file_id, file.language, &prepared.symbols)?;
        self.insert_chunks(
            ChunkInsertFile { file_id, source_revision: &prepared.sha256 },
            &prepared.chunks,
            &symbol_db_ids,
        )?;
        // Clone fingerprints were computed in the parallel prepare phase from the same parse used
        // for symbols/edges (#230) — no second read, no second parse here, just the DB write.
        // bump_df = graph.is_none(): the full-rebuild path (graph: Some) recomputes clone_token_df
        // authoritatively from the token-bag BLOBs in refresh_clone_token_df at finalize
        // (rebuild.rs), so per-token upserts here would be recomputed-and-discarded — pure waste
        // plus hot-row contention on common tokens. The incremental path (graph: None) runs no
        // such finalize, so the bump is how the LIVE df stays current (#479 — the persisted
        // postings' order is safe regardless: it is pinned per generation in `clone_df_epoch`).
        self.write_symbol_fingerprints(
            &symbol_db_ids,
            &prepared.symbol_fingerprints,
            BumpDf(graph.is_none()),
        )?;
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
            None => {
                // #827: register this (re)written file as set (a) of the scoped re-resolve write
                // set (no-op unless a scoped incremental pass armed capture).
                // Staged unconditionally — even an edge-less changed file may host
                // symbols a `resolve_changed_edges` pass will re-point in-edges
                // toward — so the write set matches what a full re-resolve touches
                // for the changed files.
                self.stage_edge_rewrite_file(file_id)?;
                // #826: register this file's PATH for the scoped logical re-derive — its symbols
                // were just (re)written, so its `logical_symbols` groups must be
                // re-derived. No-op unless a scoped logical re-derive pass armed
                // capture.
                self.stage_logical_rederive_path(&path)?;
                if !prepared.edge_candidates.is_empty() {
                    let mut candidates = prepared.edge_candidates.clone();
                    for candidate in &mut candidates {
                        candidate.remap_from_symbol_id(&symbol_db_ids);
                    }
                    edges::insert_candidates(self.storage.connection(), file_id, candidates)?;
                }
            },
        }
        self.mark_fts_dirty()?;
        Ok(())
    }

    /// Write precomputed baseline clone fingerprints (#215). `fingerprints` carries
    /// `(local_symbol_index, fingerprint)` pairs from `clones::fingerprint_symbols`; each index
    /// selects the matching DB id in `symbol_db_ids`. This is the DB-write half shared by the
    /// full-rebuild prepare phase (#230) and the incremental `store_symbol_fingerprints` wrapper,
    /// so the per-row insert discipline lives in exactly one place.
    ///
    /// The token bag is serialized into the `symbol_fingerprints.token_bag` BLOB column (#231),
    /// one BLOB per symbol — there is no longer a `symbol_token_postings` row-per-token write.
    ///
    /// `bump_df` gates the per-token LIVE `clone_token_df` upsert (#479 restored the #215
    /// mechanism the #473 freeze removed). On the full-rebuild path it is `BumpDf(false)`:
    /// `refresh_clone_token_df` (rebuild.rs) recomputes df exactly from the token-bag BLOBs at
    /// finalize, so bumping per token here is recomputed-and-discarded work plus hot-row B-tree
    /// contention on common tokens. On the incremental/heal paths it is `BumpDf(true)` — no
    /// finalize runs there, so the drift-tolerated bump is how the LIVE df stays current for the
    /// live candidate paths (a new token gets real selectivity instead of riding `DF_FALLBACK`
    /// until the next full build). The PERSISTED graph is unaffected either way: each
    /// generation's order is pinned in `clone_df_epoch` at its build.
    fn write_symbol_fingerprints(
        &self,
        symbol_db_ids: &[i64],
        fingerprints: &[(usize, clones::SymbolFingerprint)],
        bump_df: BumpDf,
    ) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let normalizer_kind = clones::NormalizerKind::Baseline.as_db_str();
        // Resolve the periphery scope ONCE (not per token). Post-A5 `clone_token_df`'s PK is
        // `(repo_id, normalizer_kind, token_hash)`, so the upsert must stamp `repo_id` AND target
        // it in the ON CONFLICT — the repo id is embedded as a per-call literal so the bound
        // params (`?1` kind, `?2` token) stay unchanged. Pre-A5 (no `repo_id` column) uses the
        // original SQL.
        let clone_df_bump_sql =
            match rag_rat_db::schema::periphery_repo_scope(conn, "clone_token_df")? {
                Some(repo_id) => format!(
                    "INSERT INTO clone_token_df(repo_id, normalizer_kind, token_hash, df)
                 VALUES ('{}', ?1, ?2, 1)
                 ON CONFLICT(repo_id, normalizer_kind, token_hash) DO UPDATE SET df = df + 1",
                    repo_id.replace('\'', "''")
                ),
                None => "INSERT INTO clone_token_df(normalizer_kind, token_hash, df)
                 VALUES (?1, ?2, 1)
                 ON CONFLICT(normalizer_kind, token_hash) DO UPDATE SET df = df + 1"
                    .to_string(),
            };
        for (local_index, fp) in fingerprints {
            let symbol_id = symbol_db_ids[*local_index];
            // The token bag rides the fingerprint row as ONE serialized BLOB (#231), replacing the
            // ~N-rows-per-symbol symbol_token_postings INSERTs (the dominant full-rebuild write
            // cost). `fp.token_bag` is already token_hash-sorted with no duplicate hashes, so the
            // BLOB is deterministic and the candidate read decodes it without re-sorting.
            let token_bag_blob = clones::bag_blob::encode_token_bag(&fp.token_bag);
            conn.prepare_cached(
                "INSERT INTO symbol_fingerprints(symbol_id, normalizer_kind, normalizer_version, \
                 oracle_run_id, struct_hash, token_len, token_bag, created_at_ms)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                symbol_id,
                normalizer_kind,
                clones::NORM_VERSION,
                fp.struct_hash,
                fp.token_len,
                token_bag_blob,
                now_ms(),
            ])?;
            // Bump the LIVE document frequency per distinct token ONLY when `bump_df`
            // (incremental/heal): df is a selectivity hint for the live candidate paths (the read
            // COALESCEs a missing row to DF_FALLBACK), so the increment-only drift is tolerated;
            // full builds recompute exactly. R7: iterate the IN-MEMORY `fp.token_bag` — no decode
            // round-trip.
            if bump_df.0 {
                for &(token_hash, _freq) in &fp.token_bag {
                    conn.prepare_cached(&clone_df_bump_sql)?
                        .execute(params![normalizer_kind, token_hash])?;
                }
            }
        }
        Ok(())
    }

    /// Serial insert: pure prepared-statement stepping. The text hash, relocation anchor, and
    /// embedding policy were all computed in the parallel prepare phase (see `prepare_chunks`), so
    /// nothing here hashes or parses.
    ///
    /// SINGLE WRITER of `chunks.embedding_policy`: full rebuild, incremental, and heal all route
    /// their chunk inserts through here, so a full rebuild's version stamp can certify the whole
    /// column for the reconcile fast path (#530). A NEW insert path that bypasses `prepare_chunks`
    /// would write the `NOT NULL DEFAULT 'Embed'` fallback and silently poison that fast path under
    /// a still-valid stamp — route any new chunk insert through here (or re-derive + restamp).
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
        symbol_db_ids: &[i64],
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
            .map(|(_, dict)| rag_rat_db::text_compression::ChunkCompressor::new(dict))
            .transpose()?;
        let dict_version = latest_dict.as_ref().map(|(version, _)| *version);
        let conn = self.storage.connection();
        for prepared in chunks {
            let chunk = &prepared.chunk;
            let anchor = &prepared.anchor;
            // Remap the chunk's LOCAL parsed-symbol index to the symbol's DB rowid (same convention
            // as edge candidates / clone fingerprints). `symbol_db_ids[i]` is the rowid of
            // `prepared.symbols[i]`, which is 1:1 with the `&[ParsedSymbol]` the chunker indexed —
            // so this is the exact symbol the chunk was cut from. `get` (not `[]`) stays defensive:
            // an out-of-range index resolves to NULL rather than panicking, and NULL simply yields
            // no drive-by record.
            let symbol_id: Option<i64> =
                chunk.symbol_index.and_then(|i| symbol_db_ids.get(i).copied());
            conn.prepare_cached(
                "INSERT INTO chunks(file_id, chunk_kind, symbol_path, symbol_id, start_byte, \
                 end_byte, start_line, end_line, text_hash,
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
                symbol_id,
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
                 kind, start_byte, end_byte, start_line, end_line, signature, docs, is_test)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                symbol.is_test,
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

    /// Label a logical group by what the members ACTUALLY show, not by a guess.
    ///
    /// This used to report `cfg_variant` for every multi-member group, which was wrong for almost
    /// all of them. A `files` PATH can have several `files` ROWS — worktree-overlay and commit
    /// scopes each carry one for the same source file — so the same single symbol shows up once
    /// per scope and gets grouped. On this repo's own index that accounted for 3,686 of 3,699
    /// multi-member groups: symbols that exist exactly once in the source were being presented
    /// as cfg variants with `variant_count` equal to the number of indexed scopes.
    ///
    /// What the members can actually distinguish:
    ///
    /// - `single` — one member.
    /// - `scope_replica` — several members, but no single `files` row contributes more than one.
    ///   This is one symbol observed in several index scopes; it is not a variant of anything.
    /// - `name_collision` — some `files` row contributes two or more members, so one file genuinely
    ///   holds multiple symbols that share name, kind, and declaration line. `impl Default for A`
    ///   and `impl Default for B` in one file land here.
    ///
    /// Deliberately NOT reported: `cfg_variant`. Genuine cfg variants are indistinguishable from a
    /// `name_collision` at this layer — both are several symbols in one file with identical
    /// declaration lines — and the group key carries no cfg evidence to separate them. Asserting
    /// the friendlier label is what made the old value useless.
    fn logical_group_reason(members: &[LogicalSymbolMemberRow]) -> &'static str {
        if members.len() <= 1 {
            return "single";
        }
        let mut per_file: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for member in members {
            *per_file.entry(member.file_id).or_default() += 1;
        }
        if per_file.values().any(|count| *count > 1) { "same_file_multi" } else { "scope_replica" }
    }

    /// Insert one logical symbol and its members. Extracted so `rebuild_logical_symbols` can flush
    /// each group as its key changes (streaming) rather than buffering every group. Both INSERTs
    /// are `prepare_cached` — the member insert runs once per symbol, so recompiling the SQL each
    /// call would dominate.
    pub(super) fn insert_logical_group(
        conn: &rusqlite::Connection,
        repo_id: &str,
        key: &LogicalSymbolKey,
        members: &[LogicalSymbolMemberRow],
    ) -> anyhow::Result<()> {
        let group_reason = Self::logical_group_reason(members);
        // Repo-distinct id (A3): `stable_id` folds `repo_id` into the content hash so two repos
        // with identical content don't collide on the `logical_symbols.id` PK in a
        // consolidated DB.
        let logical_symbol_id = key.stable_id(repo_id);
        // Intern the qualified name into the shared `name_strings` pool (#224) — the
        // qualified_name TEXT column was dropped in V028.
        let qualified_name_id = crate::index::edges::intern_edge_string(conn, &key.qualified_name)?;
        conn.prepare_cached(
            "
            INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind, \
             variant_count, group_reason, repo_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
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
            repo_id,
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

#[cfg(test)]
mod logical_group_reason_tests {
    use super::*;

    fn member(symbol_id: i64, file_id: i64) -> LogicalSymbolMemberRow {
        LogicalSymbolMemberRow {
            symbol_id,
            file_id,
            path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            name: "describe".to_string(),
            qualified_name: "src/lib.rs::describe".to_string(),
            scope_path: "describe".to_string(),
            kind: "function".to_string(),
            signature: Some("pub fn describe(&self) -> u32 {".to_string()),
            start_line: 1,
            end_line: 3,
        }
    }

    #[test]
    fn one_member_is_single() {
        assert_eq!(IndexDatabase::logical_group_reason(&[member(1, 10)]), "single");
    }

    /// The case the old label got wrong for 3,686 of 3,699 multi-member groups on this repo's own
    /// index: one symbol observed once per index scope. A path carries several `files` rows —
    /// worktree-overlay and commit scopes each add one — so the symbol is replicated, not varied.
    /// Reporting `cfg_variant` with `variant_count` equal to the number of indexed scopes told
    /// every caller a symbol that exists once in the source had N definitions.
    #[test]
    fn one_symbol_seen_in_several_scopes_is_a_replica_not_a_variant() {
        let members = [member(1, 10), member(2, 11), member(3, 12)];
        assert_eq!(IndexDatabase::logical_group_reason(&members), "scope_replica");
    }

    /// Two symbols inside ONE file row: genuinely several definitions sharing an identity. Covers
    /// both `#[cfg]`-gated pairs and unrelated collisions like two `impl Default for _` blocks —
    /// the label stays neutral because the group key cannot tell them apart.
    #[test]
    fn several_members_in_one_file_row_is_reported_as_same_file_multi() {
        let members = [member(1, 10), member(2, 10)];
        assert_eq!(IndexDatabase::logical_group_reason(&members), "same_file_multi");
    }

    /// A collision that ALSO spans scopes must not be downgraded to a replica: the same two
    /// same-file symbols seen in two scopes is still a genuine multi-definition group.
    #[test]
    fn a_same_file_collision_replicated_across_scopes_is_still_same_file_multi() {
        let members = [member(1, 10), member(2, 10), member(3, 11), member(4, 11)];
        assert_eq!(IndexDatabase::logical_group_reason(&members), "same_file_multi");
    }
}
