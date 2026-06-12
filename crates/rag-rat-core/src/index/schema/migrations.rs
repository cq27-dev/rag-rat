use super::*;

pub(crate) fn migrate_files(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "files", "indexed_revision", "TEXT NOT NULL DEFAULT ''")?;
    conn.execute("UPDATE files SET indexed_revision = sha256 WHERE indexed_revision = ''", [])?;
    Ok(())
}

pub(crate) fn migrate_chunks(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "chunks", "source_revision", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "anchor_version", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(conn, "chunks", "normalized_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "start_boundary_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "end_boundary_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "start_context_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "end_context_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "chunks", "context_radius", "INTEGER NOT NULL DEFAULT 2")?;
    add_column_if_missing(conn, "chunks", "embedding_policy", "TEXT NOT NULL DEFAULT 'Embed'")?;
    add_column_if_missing(conn, "chunks", "embedding_priority", "INTEGER NOT NULL DEFAULT 1")?;
    conn.execute(
        "
        UPDATE chunks
        SET source_revision = (
            SELECT files.indexed_revision
            FROM files
            WHERE files.id = chunks.file_id
        )
        WHERE source_revision = ''
        ",
        [],
    )?;
    Ok(())
}

pub(crate) fn migrate_edges(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges", "source_file_id", "INTEGER")?;
    add_column_if_missing(conn, "edges", "from_name", "TEXT")?;
    add_column_if_missing(conn, "edges", "to_name", "TEXT NOT NULL DEFAULT ''")?;
    apply_edge_source_target_spans(conn)?;
    apply_edge_evidence_and_resolution(conn)?;
    conn.execute(
        "
        UPDATE edges
        SET from_name = COALESCE(from_name, (
                SELECT qualified_name FROM symbols WHERE symbols.id = edges.from_symbol_id
            )),
            to_name = CASE
                WHEN to_name != '' THEN to_name
                ELSE COALESCE((SELECT qualified_name FROM symbols WHERE symbols.id = \
         edges.to_symbol_id), '')
            END
        ",
        [],
    )?;
    conn.execute("DELETE FROM edges WHERE to_name = ''", [])?;
    conn.execute(
        "
        UPDATE edges
        SET confidence = 'NameOnly'
        WHERE confidence NOT IN ('Exact', 'Syntactic', 'NameOnly', 'Ambiguous')
        ",
        [],
    )?;
    Ok(())
}

pub(crate) fn apply_edge_source_target_spans(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges", "source_start_line", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "edges", "source_end_line", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "edges", "source_start_byte", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "edges", "source_end_byte", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "edges", "target_start_line", "INTEGER")?;
    add_column_if_missing(conn, "edges", "target_end_line", "INTEGER")?;
    Ok(())
}

pub(crate) fn apply_edge_evidence_and_resolution(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges", "target_qualified_name", "TEXT")?;
    add_column_if_missing(conn, "edges", "evidence", "TEXT")?;
    add_column_if_missing(conn, "edges", "receiver_hint", "TEXT")?;
    add_column_if_missing(conn, "edges", "resolution", "TEXT NOT NULL DEFAULT 'unresolved'")?;
    conn.execute(
        "
        UPDATE edges
        SET resolution = CASE
            WHEN to_symbol_id IS NOT NULL AND confidence = 'Exact' THEN 'exact'
            WHEN to_symbol_id IS NOT NULL AND confidence = 'Syntactic' THEN 'syntactic'
            WHEN to_symbol_id IS NOT NULL AND confidence = 'Ambiguous' THEN 'ambiguous'
            WHEN to_symbol_id IS NOT NULL THEN 'name_fallback'
            ELSE COALESCE(NULLIF(resolution, ''), 'unresolved')
        END
        ",
        [],
    )?;
    Ok(())
}

pub(crate) fn apply_embedding_vector_metadata(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "ai_models", "embedding_dim", "INTEGER")?;
    add_column_if_missing(conn, "ai_models", "runtime", "TEXT NOT NULL DEFAULT 'local'")?;
    add_column_if_missing(conn, "chunk_embeddings", "embedding_dim", "INTEGER NOT NULL DEFAULT 0")?;
    conn.execute(
        "
        UPDATE ai_models
        SET embedding_dim = CASE
                WHEN capability = 'embedding' THEN COALESCE(embedding_dim, 384)
                ELSE embedding_dim
            END,
            runtime = COALESCE(runtime, 'local')
        ",
        [],
    )?;
    Ok(())
}

pub(crate) fn apply_derived_artifact_reconcile_metadata(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "chunk_embeddings", "model_version", "TEXT NOT NULL DEFAULT 'v1'")?;
    add_column_if_missing(conn, "chunk_embeddings", "attempt_count", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "chunk_embeddings", "last_error_class", "TEXT")?;
    add_column_if_missing(conn, "chunk_embeddings", "next_retry_after_ms", "INTEGER")?;
    add_column_if_missing(conn, "chunk_embeddings", "computed_at_ms", "INTEGER")?;
    conn.execute(
        "
        UPDATE chunk_embeddings
        SET model_version = CASE
                WHEN model_id = 'embedding-hash' AND model_version = 'v1' THEN 'hash-v1'
                WHEN model_id = 'fastembed-all-minilm-l6-v2' AND model_version = 'v1'
                    THEN 'fastembed-all-minilm-l6-v2-v1'
                ELSE model_version
            END,
            computed_at_ms = COALESCE(computed_at_ms, created_at_ms),
            attempt_count = CASE
                WHEN attempt_count = 0 AND status IN ('Current', 'Failed', 'Blocked') THEN 1
                ELSE attempt_count
            END,
            last_error_class = CASE
                WHEN last_error IS NOT NULL AND last_error_class IS NULL THEN status
                ELSE last_error_class
            END
        ",
        [],
    )?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS chunk_summaries(
            chunk_id INTEGER NOT NULL,
            model_id TEXT NOT NULL,
            prompt_version TEXT NOT NULL,
            input_hash TEXT NOT NULL,
            text_hash TEXT NOT NULL,
            summary TEXT NOT NULL,
            status TEXT NOT NULL,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error_class TEXT,
            next_retry_after_ms INTEGER,
            computed_at_ms INTEGER,
            PRIMARY KEY(chunk_id, model_id, prompt_version),
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS reconcile_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_embedding_policy_and_input_hash(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "chunks", "embedding_policy", "TEXT NOT NULL DEFAULT 'Embed'")?;
    add_column_if_missing(conn, "chunks", "embedding_priority", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(conn, "chunk_embeddings", "input_hash", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(
        conn,
        "chunk_embeddings",
        "embedding_text_version",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "chunk_embeddings",
        "embedding_policy",
        "TEXT NOT NULL DEFAULT 'Embed'",
    )?;
    add_column_if_missing(
        conn,
        "chunk_embeddings",
        "embedding_priority",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(conn, "chunk_embeddings", "input_chars", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(
        conn,
        "chunk_embeddings",
        "input_truncated",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "reconcile_attempts", "elapsed_ms", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "reconcile_attempts", "input_chars", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "reconcile_attempts", "batch_size", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

pub(crate) fn apply_github_ref_sync(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS github_ref_sync(
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            status TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL,
            last_error TEXT,
            PRIMARY KEY(owner, repo, number)
        );
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_symbol_facts(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS symbol_facts(
            symbol_id INTEGER NOT NULL,
            fact_kind TEXT NOT NULL,
            fact_value TEXT NOT NULL,
            PRIMARY KEY(symbol_id, fact_kind, fact_value),
            FOREIGN KEY(symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_symbol_facts_kind_value
            ON symbol_facts(fact_kind, fact_value);
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_repo_memories(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS repo_memories(
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            confidence TEXT NOT NULL,
            status TEXT NOT NULL,
            created_by TEXT,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            source TEXT NOT NULL,
            source_text_hash TEXT,
            input_hash TEXT,
            memory_version TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS repo_memory_bindings(
            memory_id TEXT NOT NULL,
            binding_kind TEXT NOT NULL,
            binding_id TEXT NOT NULL,
            path TEXT,
            start_line INTEGER,
            end_line INTEGER,
            logical_symbol_id INTEGER,
            symbol_id INTEGER,
            chunk_id INTEGER,
            edge_id INTEGER,
            commit_hash TEXT,
            github_owner TEXT,
            github_repo TEXT,
            github_number INTEGER,
            anchor_status TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(memory_id, binding_kind, binding_id),
            FOREIGN KEY(memory_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS repo_memory_tags(
            memory_id TEXT NOT NULL,
            tag TEXT NOT NULL,
            PRIMARY KEY(memory_id, tag),
            FOREIGN KEY(memory_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS repo_memory_call_paths(
            memory_id TEXT NOT NULL,
            start_logical_symbol_id INTEGER,
            end_logical_symbol_id INTEGER,
            edge_sequence_hash TEXT NOT NULL,
            path_summary TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(memory_id, edge_sequence_hash),
            FOREIGN KEY(memory_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS repo_memory_fts USING fts5(
            memory_id UNINDEXED,
            title,
            body,
            kind,
            tags,
            tokenize='porter'
        );

        CREATE INDEX IF NOT EXISTS idx_repo_memory_bindings_logical_symbol
            ON repo_memory_bindings(logical_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_repo_memory_bindings_symbol
            ON repo_memory_bindings(symbol_id);
        CREATE INDEX IF NOT EXISTS idx_repo_memory_bindings_chunk
            ON repo_memory_bindings(chunk_id);
        CREATE INDEX IF NOT EXISTS idx_repo_memory_bindings_edge
            ON repo_memory_bindings(edge_id);
        CREATE INDEX IF NOT EXISTS idx_repo_memory_bindings_path
            ON repo_memory_bindings(path);
        CREATE INDEX IF NOT EXISTS idx_repo_memory_call_paths_start
            ON repo_memory_call_paths(start_logical_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_repo_memory_call_paths_end
            ON repo_memory_call_paths(end_logical_symbol_id);
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_repo_memory_call_paths(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS repo_memory_call_paths(
            memory_id TEXT NOT NULL,
            start_logical_symbol_id INTEGER,
            end_logical_symbol_id INTEGER,
            edge_sequence_hash TEXT NOT NULL,
            path_summary TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(memory_id, edge_sequence_hash),
            FOREIGN KEY(memory_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_repo_memory_bindings_edge
            ON repo_memory_bindings(edge_id);
        CREATE INDEX IF NOT EXISTS idx_repo_memory_call_paths_start
            ON repo_memory_call_paths(start_logical_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_repo_memory_call_paths_end
            ON repo_memory_call_paths(end_logical_symbol_id);
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_repo_memory_call_path_edges(conn: &Connection) -> rusqlite::Result<()> {
    // The ordered edges behind a server-derived call-path hash (#38). `edge_fingerprint` is the
    // exact, row-id-independent identity (path+lines+names+kind+target); the looser
    // from/to/kind/target columns let validation re-find an edge that moved lines (relocated)
    // rather than reporting the whole path gone. One row per edge, ordered by `ordinal`.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS repo_memory_call_path_edges(
            memory_id TEXT NOT NULL,
            edge_sequence_hash TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            edge_fingerprint TEXT NOT NULL,
            from_name TEXT,
            to_name TEXT NOT NULL,
            edge_kind TEXT NOT NULL,
            target_qualified_name TEXT,
            receiver_hint TEXT,
            PRIMARY KEY(memory_id, edge_sequence_hash, ordinal),
            FOREIGN KEY(memory_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_repo_memory_call_path_edges_hash
            ON repo_memory_call_path_edges(edge_sequence_hash);
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_memory_binding_signals(conn: &Connection) -> rusqlite::Result<()> {
    // Durable corroboration signals for cross-file relocation: a moved symbol keeps its
    // kind + signature even when its path-qualified name (and rowids) change.
    add_column_if_missing(conn, "repo_memory_bindings", "symbol_kind", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "signature_hash", "TEXT")?;
    Ok(())
}

pub(crate) fn apply_graph_file_lookup_indexes(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges", "source_file_id", "INTEGER")?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
        CREATE INDEX IF NOT EXISTS idx_edges_source_file ON edges(source_file_id);
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_logical_symbol_groups(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS logical_symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            language TEXT NOT NULL,
            path TEXT NOT NULL,
            logical_name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            variant_count INTEGER NOT NULL,
            group_reason TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS logical_symbol_members(
            logical_symbol_id INTEGER NOT NULL,
            symbol_id INTEGER NOT NULL,
            cfg_expr TEXT,
            signature_hash TEXT,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            PRIMARY KEY(logical_symbol_id, symbol_id),
            FOREIGN KEY(logical_symbol_id) REFERENCES logical_symbols(id) ON DELETE CASCADE,
            FOREIGN KEY(symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_logical_symbols_qualified_name
            ON logical_symbols(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_logical_symbol_members_symbol
            ON logical_symbol_members(symbol_id);
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_symbol_line_spans(conn: &Connection) -> rusqlite::Result<()> {
    // Carry the symbol's 1-based line span (already known at parse time) on the row itself.
    // Before this, every reader of `symbols` (edge extraction, edge resolution, logical-symbol
    // rebuild) recomputed start_line/end_line with a per-symbol correlated subquery against
    // `chunks` — O(symbols × chunks) and the dominant cost of a full rebuild. DEFAULT 0 is a
    // sentinel only for rows migrated in place; a full reindex repopulates real values.
    add_column_if_missing(conn, "symbols", "start_line", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "symbols", "end_line", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

pub(crate) fn apply_edge_callee_byte_range(conn: &Connection) -> rusqlite::Result<()> {
    // Byte range of the callee identifier token on symbol-referencing edges (the SCIP-oracle
    // prerequisite, #67). `source_start_byte`/`source_end_byte` cover the whole call_expression;
    // SCIP occurrences key on the identifier token, so these two columns carry its range instead.
    // Additive + NULLABLE on purpose: existing rows and non-call edges (contains / imports /
    // exports / file-level) keep NULL, so the change is byte-identical for prior data. `(line,
    // col)` in the document's position encoding is derived at join time from checkout bytes —
    // not stored.
    add_column_if_missing(conn, "edges", "callee_start_byte", "INTEGER")?;
    add_column_if_missing(conn, "edges", "callee_end_byte", "INTEGER")?;
    Ok(())
}

pub(crate) fn apply_oracle_tables(conn: &Connection) -> rusqlite::Result<()> {
    // SCIP-oracle side tables (#68). Greenfield, STRICT per repo convention.
    //
    // `oracle_runs`: one row per oracle pass (a `.scip` consumed against a checkout). `stats_json`
    // is an opaque per-run `OracleReport` snapshot, suffixed `_json` per the naming convention.
    //
    // `edge_oracle`: the compiler-grade resolution for an edge, kept **beside** the heuristic
    // resolution that lives on the `edges` row — the heuristic row is NEVER overwritten, so eval
    // can diff the two and `compare_graph_to_scip` (#69) has both. INVARIANT: writing an
    // `edge_oracle` row must not UPDATE `edges.resolution` / `edges.to_symbol_id`.
    //
    // Staleness key is `(file_sha, tool, tool_version)` (content addressing, exactly like the
    // embedding `input_hash`): a row is valid iff the file bytes it was computed against are
    // unchanged. `file_sha` is the `files.sha256` of the edge's source file at compute time, so a
    // changed file's oracle rows are detectably stale without an indexer re-run on unchanged files.
    //
    // `kind` is the oracle resolution outcome (upgrade / resolved-external / confirm / contradict);
    // `resolved_symbol_id` is our `symbols.id` when the SCIP definition mapped inside the corpus,
    // NULL for `resolved-external`. `scip_symbol` is the raw SCIP symbol string for provenance.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS oracle_runs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            -- The checkout the run was scoped to. A multi-worktree DB holds runs from sibling
            -- checkouts under the same `(tool, tool_version, commit_sha)`; without this the status
            -- read's `last_run_meta` could surface a SIBLING worktree's run as THIS checkout's \
         last
            -- run (the verdict counts are already worktree-scoped, so the two would disagree). \
         Added
            -- in V018 directly (this is the unshipped oracle migration) — no separate migration.
            worktree_id TEXT NOT NULL DEFAULT '',
            started_at INTEGER NOT NULL,
            status TEXT NOT NULL,
            stats_json TEXT NOT NULL DEFAULT '{}'
        ) STRICT;

        CREATE TABLE IF NOT EXISTS edge_oracle(
            edge_id INTEGER NOT NULL,
            file_sha TEXT NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            resolved_symbol_id INTEGER,
            scip_symbol TEXT NOT NULL,
            kind TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(edge_id, tool, tool_version),
            -- A verdict is meaningless once its edge is gone. The full rebuild and
            -- `remove_file_in_scope` delete + reinsert edges with new ids and recompute the \
         oracle,
            -- so cascading old verdicts away (rather than orphaning them, where status/eval would
            -- keep counting them) is correct. `PRAGMA foreign_keys=ON` is set, so this fires.
            FOREIGN KEY(edge_id) REFERENCES edges(id) ON DELETE CASCADE
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_edge_oracle_staleness
            ON edge_oracle(file_sha, tool, tool_version);
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_symbol
            ON edge_oracle(resolved_symbol_id);
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_scip_moniker_anchors(conn: &Connection) -> rusqlite::Result<()> {
    // SCIP moniker anchors (#70, phase 3). Greenfield table STRICT per repo convention.
    //
    // `logical_symbol_monikers`: the SCIP symbol string ("moniker") for a logical symbol, written
    // by `oracle run` from the `.scip` definition map. Keyed by `logical_symbols.id`, which is a
    // CONTENT-DERIVED stable id (language/path/name/qualified_name/kind/signature — see
    // `LogicalSymbolKey::stable_id`), NOT a rowid.
    //
    // INVARIANT (load-bearing): NO foreign key to `logical_symbols`, on purpose.
    // `rebuild_logical_symbols` runs on EVERY index pass and rebuilds the table wholesale
    // (DELETE-all + reinsert) — an FK cascade would wipe every moniker on every reindex, defeating
    // the relocation fallback. Because the id is content-derived, an unchanged symbol's reinserted
    // row keeps its id and its moniker row stays valid across rebuilds with no re-run. A CHANGED
    // symbol mints a new id and its old moniker row dangles; every read joins live
    // `logical_symbols`, so a dangling row never resolves, and the next `oracle run`'s
    // authoritative per-tool clear removes it.
    //
    // PK `(logical_symbol_id, tool)`: one moniker per logical symbol per tool — cfg-gated Rust
    // variants share the logical symbol, hence share the moniker by construction. `tool_version`
    // rides along so a relocation match against a binding recorded under a different version can
    // be treated as lower confidence (#70).
    //
    // `repo_memory_bindings` gains the moniker provenance pair (set on `scip_moniker`-kind binding
    // rows) and `relocation_reason` (e.g. `moniker-match`), all nullable/additive.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS logical_symbol_monikers(
            logical_symbol_id INTEGER NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            moniker TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(logical_symbol_id, tool)
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_logical_symbol_monikers_moniker
            ON logical_symbol_monikers(moniker, tool);
        ",
    )?;
    add_column_if_missing(conn, "repo_memory_bindings", "moniker_tool", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "moniker_tool_version", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "relocation_reason", "TEXT")?;
    Ok(())
}

pub(crate) fn applied_migrations(conn: &Connection) -> anyhow::Result<Vec<AppliedMigration>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, applied_at_ms, checksum, description
        FROM schema_version
        ORDER BY applied_at_ms, id
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AppliedMigration {
            id: row.get(0)?,
            applied_at_ms: row.get(1)?,
            checksum: row.get(2)?,
            description: row.get(3)?,
        })
    })?;
    let mut migrations = Vec::new();
    for row in rows {
        migrations.push(row?);
    }
    Ok(migrations)
}

pub(crate) fn known_version(migrations: &[AppliedMigration]) -> u32 {
    migrations
        .iter()
        .filter_map(|migration| match migration.id.as_str() {
            MIGRATION_001_ID => Some(1),
            MIGRATION_002_ID => Some(2),
            MIGRATION_003_ID => Some(3),
            MIGRATION_004_ID => Some(4),
            MIGRATION_005_ID => Some(5),
            MIGRATION_006_ID => Some(6),
            MIGRATION_007_ID => Some(7),
            MIGRATION_008_ID => Some(8),
            MIGRATION_009_ID => Some(9),
            MIGRATION_010_ID => Some(10),
            MIGRATION_011_ID => Some(11),
            MIGRATION_012_ID => Some(12),
            MIGRATION_013_ID => Some(13),
            MIGRATION_014_ID => Some(14),
            MIGRATION_015_ID => Some(15),
            MIGRATION_016_ID => Some(16),
            MIGRATION_017_ID => Some(17),
            MIGRATION_018_ID => Some(18),
            MIGRATION_019_ID => Some(19),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

pub(crate) fn known_migration(id: &str) -> bool {
    matches!(
        id,
        MIGRATION_001_ID
            | MIGRATION_002_ID
            | MIGRATION_003_ID
            | MIGRATION_004_ID
            | MIGRATION_005_ID
            | MIGRATION_006_ID
            | MIGRATION_007_ID
            | MIGRATION_008_ID
            | MIGRATION_009_ID
            | MIGRATION_010_ID
            | MIGRATION_011_ID
            | MIGRATION_012_ID
            | MIGRATION_013_ID
            | MIGRATION_014_ID
            | MIGRATION_015_ID
            | MIGRATION_016_ID
            | MIGRATION_017_ID
            | MIGRATION_018_ID
            | MIGRATION_019_ID
            | DIRTY_MIGRATION_ID
    )
}

pub(crate) fn migration_checksum_mismatch(migration: &AppliedMigration) -> bool {
    match migration.id.as_str() {
        MIGRATION_001_ID => migration.checksum != MIGRATION_001_CHECKSUM,
        MIGRATION_002_ID => migration.checksum != MIGRATION_002_CHECKSUM,
        MIGRATION_003_ID => migration.checksum != MIGRATION_003_CHECKSUM,
        MIGRATION_004_ID => migration.checksum != MIGRATION_004_CHECKSUM,
        MIGRATION_005_ID => migration.checksum != MIGRATION_005_CHECKSUM,
        MIGRATION_006_ID => migration.checksum != MIGRATION_006_CHECKSUM,
        MIGRATION_007_ID => migration.checksum != MIGRATION_007_CHECKSUM,
        MIGRATION_008_ID => migration.checksum != MIGRATION_008_CHECKSUM,
        MIGRATION_009_ID => migration.checksum != MIGRATION_009_CHECKSUM,
        MIGRATION_010_ID => migration.checksum != MIGRATION_010_CHECKSUM,
        MIGRATION_011_ID => migration.checksum != MIGRATION_011_CHECKSUM,
        MIGRATION_012_ID => migration.checksum != MIGRATION_012_CHECKSUM,
        MIGRATION_013_ID => migration.checksum != MIGRATION_013_CHECKSUM,
        MIGRATION_014_ID => migration.checksum != MIGRATION_014_CHECKSUM,
        MIGRATION_015_ID => migration.checksum != MIGRATION_015_CHECKSUM,
        MIGRATION_016_ID => migration.checksum != MIGRATION_016_CHECKSUM,
        MIGRATION_017_ID => migration.checksum != MIGRATION_017_CHECKSUM,
        MIGRATION_018_ID => migration.checksum != MIGRATION_018_CHECKSUM,
        MIGRATION_019_ID => migration.checksum != MIGRATION_019_CHECKSUM,
        _ => false,
    }
}

pub(crate) fn record_migration(
    conn: &Connection,
    id: &str,
    checksum: &str,
    description: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES (?1, ?2, ?3, ?4)",
        params![id, now_ms(), checksum, description],
    )?;
    Ok(())
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'virtual table') AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(exists)
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub(crate) fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }

    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))
}

pub(crate) fn apply_commit_addressable_worktrees(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "files", "commit_sha", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "files", "worktree_id", "TEXT NOT NULL DEFAULT ''")?;
    rebuild_files_table_for_commit_scopes(conn)?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_files_commit_path ON files(commit_sha, path);
        CREATE INDEX IF NOT EXISTS idx_files_worktree_path ON files(worktree_id, path);
        ",
    )?;
    Ok(())
}

pub(crate) fn rebuild_files_table_for_commit_scopes(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        PRAGMA foreign_keys = OFF;

        CREATE TABLE IF NOT EXISTS files_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            kind TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0,
            indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT '',
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            UNIQUE(path, commit_sha, worktree_id)
        );

        INSERT OR IGNORE INTO files_new(
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id
        )
        SELECT
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, COALESCE(commit_sha, ''), COALESCE(worktree_id, '')
        FROM files;

        DROP TABLE files;
        ALTER TABLE files_new RENAME TO files;

        PRAGMA foreign_keys = ON;
        ",
    )
}
