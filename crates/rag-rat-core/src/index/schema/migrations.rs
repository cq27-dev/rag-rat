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
    // This runs inside `apply_baseline`, BEFORE the ladder, so the `symbols` shape depends on the
    // DB's age: a fresh post-V028 baseline has the interned `qualified_name_id` (and the dropped
    // `qualified_name`), while a pre-V020 legacy DB still has the inline `qualified_name` TEXT
    // column (V028 hasn't run yet). SQLite compiles the whole statement, so referencing a missing
    // column is a hard error even when zero rows match — pick the backfill source by which column
    // exists (#224). On a fresh DB this `edges` is the empty view; the UPDATE is a no-op either
    // way.
    let symbol_qname_expr = if column_exists(conn, "symbols", "qualified_name")? {
        "(SELECT qualified_name FROM symbols WHERE symbols.id = edges.{side}_symbol_id)"
    } else {
        "(SELECT value FROM name_strings WHERE name_strings.id =
              (SELECT qualified_name_id FROM symbols WHERE symbols.id = edges.{side}_symbol_id))"
    };
    let from_expr = symbol_qname_expr.replace("{side}", "from");
    let to_expr = symbol_qname_expr.replace("{side}", "to");
    conn.execute(
        &format!(
            "
        UPDATE edges
        SET from_name = COALESCE(from_name, {from_expr}),
            to_name = CASE
                WHEN to_name != '' THEN to_name
                ELSE COALESCE({to_expr}, '')
            END
        "
        ),
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
        CREATE INDEX IF NOT EXISTS idx_edges_source_file ON edges_data(source_file_id);
        ",
    )?;
    Ok(())
}

pub(crate) fn apply_logical_symbol_groups(conn: &Connection) -> rusqlite::Result<()> {
    // V007 created `logical_symbols` with an inline `qualified_name TEXT` and a string index. On a
    // fresh post-V028 baseline the table ALREADY exists with the interned `qualified_name_id`
    // shape, so the `CREATE TABLE IF NOT EXISTS` is a no-op — but the old `ON
    // logical_symbols(qualified_name)` index would reference a column that no longer exists and
    // fail. Create the qualified-name index on whichever column the table has (#224); a
    // pre-V028 DB gets the string index (V028 later swaps it for the id index), a fresh DB gets
    // the id index directly.
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

        CREATE INDEX IF NOT EXISTS idx_logical_symbol_members_symbol
            ON logical_symbol_members(symbol_id);
        ",
    )?;
    if column_exists(conn, "logical_symbols", "qualified_name")? {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_logical_symbols_qualified_name
                ON logical_symbols(qualified_name);",
        )?;
    } else {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_logical_symbols_qualified_name_id
                ON logical_symbols(qualified_name_id);",
        )?;
    }
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

pub(crate) fn apply_symbol_scope_path(conn: &Connection) -> rusqlite::Result<()> {
    // The symbol's SEMANTIC scope path (enclosing type/module/namespace names + own name, e.g.
    // `Workspace::new`) — the resolver's qualified-match key, aligned with edges' source-derived
    // `target_qualified_name`. Distinct from `qualified_name` (file-path form, kept as the stable
    // identity for logical-symbol grouping + memory anchoring, untouched here). NULLABLE: existing
    // rows read as `COALESCE(scope_path,'')` until a full reindex repopulates real values; the
    // resolver simply skips the scope path until then (#61).
    add_column_if_missing(conn, "symbols", "scope_path", "TEXT")?;
    Ok(())
}

/// V022 (#61 per-package + module-aware import-scope rework). Additive + idempotent, so it is
/// byte-identical under both a fresh full `apply()` and a forward-only migrate from an older index.
///
/// `packages`: one row per Cargo manifest in the corpus, scoped by `(commit_sha, worktree_id)` like
/// `files`. `local_roots_json` is this package's own importable crate roots — the workspace crate
/// names (global union) PLUS this manifest's in-corpus path-dependency alias keys — so a
/// `use alias::…` resolves local for the package that declares the alias and external everywhere
/// else (#1: per-package locality). The file→package mapping is NOT persisted on `files`: the
/// resolver computes it at LOAD time (`load_package_roots_into_scope`) by longest-`manifest_dir`-
/// prefix over the active scope's `packages` rows. A persisted `files.package_id` pointer was the
/// #106 multi-worktree leak — a clean file is a SHARED commit-scope row read by every worktree, but a
/// package row is worktree-scoped, so one worktree's refresh stamped its ids onto a sibling's
/// shared rows. Computing at load reads each scope's OWN `packages`, so no pointer can leak.
///
/// Edge columns are DEDICATED (`import_scope_*`, `import_mod_id`), NOT a callee_* overload: the
/// oracle's candidate filter is `callee_start_byte IS NOT NULL`, and overloading that column with a
/// non-identifier scope range would drag import rows into the SCIP occurrence join (the #100
/// collision). With dedicated NULL-on-non-import columns the oracle filter stays correct untouched.
/// They are added to `edges_data` (the real table); the `edges` compatibility view is recreated by
/// `ensure_edges_view` below so readers/tests can write them through the view.
pub(crate) fn apply_per_package_import_scope(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS packages(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            manifest_dir TEXT NOT NULL,
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            local_roots_json TEXT NOT NULL DEFAULT '[]',
            UNIQUE(manifest_dir, commit_sha, worktree_id)
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_packages_scope ON packages(commit_sha, worktree_id);
        ",
    )?;
    // The dedicated import-scope columns on the REAL edge table (the `edges` symbol is a view
    // post-V020) are added + the compatibility view recreated by `ensure_edges_view`, which owns
    // the view↔table column contract and is idempotent. (It already ran at V020; rerun so a
    // forward-migrate from a pre-V022 index that somehow skipped the V020 rerun still converges.)
    ensure_edges_view(conn)?;
    // This migration only adds the package table + edge COLUMNS — it does not backfill `packages`
    // or re-derive the `import_scope_*` edge columns on existing rows. That backfill rides the
    // `GRAPH_INDEX_VERSION` bump (→ 7) instead: an upgraded index has a stale
    // `graph_index_version`, so `ensure_graph_index_current` re-resolves on next open and (per
    // the `refresh_packages` call added to that path) repopulates `packages`. Without the
    // version bump the new per-package behavior would never engage post-migration. The
    // file→package mapping is computed at LOAD time from `packages`, so there is no
    // `files.package_id` column to add.
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
    // INVARIANT (load-bearing, #248): every oracle-DERIVED persisted table here is enumerated in
    // `schema::ORACLE_PERSISTED_TABLES` and MUST survive reindex — content-keyed with NO
    // reindex-cascading FK to a volatile parent (`schema::REINDEX_VOLATILE_PARENTS`); reads join
    // the live parents so a dangling row never resolves. A NEW oracle-derived table created
    // here must be added to that const, which forces it through the
    // `oracle_persisted_tables_have_no_ reindex_cascading_fk` trip-wire. See the const's doc
    // comment for the full rationale + the `logical_symbol_monikers` precedent.
    //
    // `oracle_runs`: one row per oracle pass (a `.scip` consumed against a checkout). `stats_json`
    // is an opaque per-run `OracleReport` snapshot, suffixed `_json` per the naming convention.
    //
    // `edge_oracle`: the compiler-grade resolution for an edge, kept **beside** the heuristic
    // resolution that lives on the `edges` row — the heuristic row is NEVER overwritten, so eval
    // can diff the two and `compare_graph_to_scip` (#69) has both. INVARIANT: writing an
    // `edge_oracle` row must not UPDATE `edges.resolution` / `edges.to_symbol_id`.
    //
    // CONTENT-ANCHORED (V031, #248): a verdict is keyed by the edge's CONTENT identity, NOT the
    // volatile `edges_data.id` rowid, and there is NO FK to `edges_data`. The original V018 shape
    // keyed on `edge_id` with `FOREIGN KEY(edge_id) REFERENCES edges_data(id) ON DELETE CASCADE` —
    // but every reindex rewrites `edges_data` (full rebuild + `remove_file_in_scope`), so the
    // cascade wiped EVERY verdict and the opt-in oracle never repopulated (it does not auto-run).
    // This mirrors `logical_symbol_monikers`: a content key + no reindex-cascading FK, with reads
    // joining LIVE `edges` so a dangling row never resolves. An UNCHANGED file (same
    // `files.sha256`) re-anchors its verdict to the reindexed edge for free; a CHANGED file's
    // sha differs so its verdict no longer matches (stale → not surfaced/counted, swept by the
    // next run's clear / gc).
    //
    // Content key = `(tool, tool_version, source_path, source_start_byte, source_end_byte,
    // callee_start_byte, callee_end_byte, edge_kind)`. Measured UNIQUE over the resolvable
    // (non-NULL callee range) edge population the oracle rows — the call SITE span + the callee
    // span
    // + the edge kind disambiguate (a `calls_name` and a `references_type` on the same identifier
    // token differ in `edge_kind`). `edge_kind` is stored as TEXT (resolved from
    // `edges_data.edge_kind_id` via `name_strings` at write time), NOT the interned id (which is
    // not guaranteed stable across reindex).
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
            source_path TEXT NOT NULL,
            source_start_byte INTEGER NOT NULL,
            source_end_byte INTEGER NOT NULL,
            callee_start_byte INTEGER NOT NULL,
            callee_end_byte INTEGER NOT NULL,
            edge_kind TEXT NOT NULL,
            file_sha TEXT NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            resolved_symbol_id INTEGER,
            scip_symbol TEXT NOT NULL,
            kind TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            -- Content key (#248): reindex-stable, no rowid. NO FK to edges_data — the read join to
            -- live edges (by this key + `files.sha256 = file_sha`) is what filters dangling rows,
            -- exactly like the moniker model.
            PRIMARY KEY(
                tool, tool_version, source_path,
                source_start_byte, source_end_byte,
                callee_start_byte, callee_end_byte, edge_kind
            )
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_edge_oracle_staleness
            ON edge_oracle(file_sha, tool, tool_version);
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_symbol
            ON edge_oracle(resolved_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_anchor
            ON edge_oracle(source_path, callee_start_byte, callee_end_byte, edge_kind);
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

/// The integer indexes on `edges_data` (#79) — the successors of the old TEXT indexes on `edges`.
/// Called from baseline (fresh DBs) AND after the V020 conversion (upgrading DBs, where the
/// same-named legacy indexes blocked `IF NOT EXISTS` until `DROP TABLE edges` removed them).
pub(crate) fn ensure_edges_data_indexes(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_edges_from_symbol ON edges_data(from_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edges_to_symbol ON edges_data(to_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edges_source_file ON edges_data(source_file_id);
        CREATE INDEX IF NOT EXISTS idx_edges_from_name ON edges_data(from_name_id);
        CREATE INDEX IF NOT EXISTS idx_edges_to_name ON edges_data(to_name_id);
        ",
    )
}

/// Create the `edges` compatibility VIEW + its INSTEAD OF triggers (#79) — but ONLY when no
/// legacy `edges` TABLE is present (an INSTEAD OF trigger on a table is a hard error, and the
/// legacy table must keep working until `apply_edge_string_interning` converts it).
///
/// The view reconstructs the historical column shape, so the entire read surface — graph
/// traversal, impact, memory fingerprints, oracle compare, dev-inspect SQL — keeps working
/// unchanged. All dictionary joins are LEFT JOINs against the `name_strings` PRIMARY KEY, so the
/// planner drops the joins a query doesn't reference.
///
/// The triggers make ad-hoc writes through the view work (tests, migrations, maintenance UPDATEs)
/// with the legacy semantics, including the old columns' DEFAULTs. CAVEAT (load-bearing):
/// `last_insert_rowid()` REVERTS after an INSTEAD OF trigger ends — an insert through the view
/// cannot read back the new edge id that way. The production insert paths write `edges_data`
/// directly with interned ids for this reason (and for bulk speed).
/// V023 (#200): recreate the `edges` compatibility view so its definition excludes the internal
/// dispatch FACT rows. `ensure_edges_view` is idempotent (DROP + CREATE), so this simply rebuilds
/// the view with the new body on an existing index; the underlying `edges_data` rows are untouched.
pub(crate) fn apply_dispatch_edge_facts_view_exclusion(conn: &Connection) -> rusqlite::Result<()> {
    ensure_edges_view(conn)
}

/// V024 (#77): add `files.has_test_code` and backfill it from existing chunk text. Additive +
/// idempotent — `add_column_if_missing` guards the ADD (a fresh DB already has the column from the
/// baseline), and the backfill recomputes from the SAME markers `impact_surface` previously scanned
/// for, so a forward-migrated index matches a freshly-indexed one immediately (no wait for
/// reindex). Invariant: the marker set here MUST stay in sync with `index::text_has_test_marker`
/// (the index-time compute) and `test_items`'s filter, or migrated vs reindexed rows would diverge.
/// Uses `instr` (case-sensitive, literal substring), NOT `LIKE` — SQLite `LIKE` is case-insensitive
/// for ASCII, so it would match an uppercase `TEST(` that the case-sensitive `str::contains` at
/// index time does not, diverging a forward-migrated row from a freshly-indexed one. (`instr` also
/// needs no `%`-escaping of the `[`/`(` in the markers.)
pub(crate) fn apply_files_has_test_code(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "files", "has_test_code", "INTEGER NOT NULL DEFAULT 0")?;
    // The backfill reads chunks.text. On a fresh DB the baseline omits that column (V027 retired
    // it), and there is nothing to backfill — chunks is empty and the index-time path sets
    // has_test_code. Only a pre-V027 forward-migrate (column still present, chunks populated)
    // needs the backfill.
    if !column_exists(conn, "chunks", "text")? {
        return Ok(());
    }
    conn.execute_batch(
        "UPDATE files SET has_test_code = 1 WHERE id IN (
             SELECT DISTINCT file_id FROM chunks
             WHERE instr(text, '#[cfg(test)]') > 0 OR instr(text, 'describe(') > 0
                OR instr(text, 'it(') > 0 OR instr(text, 'test(') > 0
         );",
    )
}

/// V025 (#77): create the chunk_text (zstd blob) + chunk_text_dict (shared dictionary) tables for
/// compressed chunk text. Additive + idempotent (CREATE TABLE IF NOT EXISTS); a fresh DB already
/// has them from the baseline. NO backfill here — populating chunk_text + retiring chunks.text is
/// driven by the index pipeline (compress at write) once the read paths decompress from chunk_text,
/// so the data move isn't an irreversible one-shot in the migration.
pub(crate) fn apply_chunk_text_compression_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS chunk_text(
            chunk_id INTEGER PRIMARY KEY,
            blob BLOB NOT NULL,
            raw_len INTEGER NOT NULL CHECK(raw_len >= 0),
            dict_version INTEGER NOT NULL,
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        ) STRICT;
        CREATE TABLE IF NOT EXISTS chunk_text_dict(
            version INTEGER PRIMARY KEY,
            dict BLOB NOT NULL
        ) STRICT;
        ",
    )
}

/// V026 (#77 Phase 2): recreate `chunk_fts` as a CONTENTLESS FTS5 index (it was external-content,
/// `content='chunks'`) and repopulate it from `chunks.text` — which still exists at migration time
/// (the column drop is the later V027). Going contentless is the prerequisite for dropping
/// `chunks.text`: an external-content index re-reads that column on every rebuild. After this,
/// tokens are written inline at index time, so the column drop can't break the index.
/// DROP+CREATE (not idempotent `IF NOT EXISTS`) is intended: a pre-V026 DB has the external-content
/// table and must be converted; a fresh DB already has the contentless table from the baseline and
/// this rebuilds it empty (the SELECT over zero chunks is a no-op).
pub(crate) fn apply_contentless_chunk_fts(conn: &Connection) -> rusqlite::Result<()> {
    // IDEMPOTENT: this migration's DROP+CREATE is destructive (it discards the chunk_fts index),
    // and `schema::apply` re-runs every additive migration on each call, so converting
    // unconditionally would wipe the inline-written contentless index on every open/rebuild.
    // Only convert when chunk_fts is still the OLD external-content table; if it is already
    // contentless (or absent — the baseline creates it contentless), do nothing.
    let already_contentless: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE name = 'chunk_fts' AND sql NOT LIKE '%content=''chunks''%'
         )",
        [],
        |row| row.get(0),
    )?;
    if already_contentless {
        return Ok(());
    }
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS chunk_fts;
        CREATE VIRTUAL TABLE chunk_fts USING fts5(
            text,
            content='',
            contentless_delete=1,
            tokenize='porter'
        );
        ",
    )?;
    // Repopulate from chunks.text only on a pre-V027 forward-migrate, where the column still
    // exists. On a fresh DB the baseline omits chunks.text (and chunks is empty), so there is
    // nothing to repopulate — the inline write path fills chunk_fts during the rebuild
    // instead.
    if column_exists(conn, "chunks", "text")? {
        conn.execute("INSERT INTO chunk_fts(rowid, text) SELECT id, text FROM chunks", [])?;
    }
    Ok(())
}

pub(crate) fn ensure_edges_view(conn: &Connection) -> rusqlite::Result<()> {
    let legacy_table: Option<String> = conn
        .query_row(
            "SELECT type FROM sqlite_master WHERE name = 'edges' AND type = 'table'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if legacy_table.is_some() {
        return Ok(());
    }
    // The view body below references the dedicated import-scope columns (V022). This function runs
    // at V020 too — BEFORE V022 in the linear apply — and on a forward-migrate from an older
    // view-shaped DB whose `edges_data` predates them, so guarantee they exist here (idempotent)
    // before the view is (re)defined against them. Without this the V020 CREATE VIEW resolves
    // `d.import_scope_start_byte` against a table that lacks it and fails (#61).
    add_column_if_missing(conn, "edges_data", "import_scope_start_byte", "INTEGER")?;
    add_column_if_missing(conn, "edges_data", "import_scope_end_byte", "INTEGER")?;
    add_column_if_missing(conn, "edges_data", "import_mod_id", "INTEGER")?;
    conn.execute_batch(
        "
        -- Recreate unconditionally: the view's definition evolves (e.g. the appended *_id
        -- columns below), and CREATE IF NOT EXISTS would freeze an older shape in any DB that
        -- already has one. Dropping the view drops its INSTEAD OF triggers too.
        DROP TRIGGER IF EXISTS edges_view_insert;
        DROP TRIGGER IF EXISTS edges_view_update;
        DROP TRIGGER IF EXISTS edges_view_delete;
        DROP VIEW IF EXISTS edges;
        CREATE VIEW edges AS
        SELECT d.id,
               d.source_file_id,
               d.from_symbol_id,
               d.to_symbol_id,
               fn.value AS from_name,
               tn.value AS to_name,
               d.source_start_line,
               d.source_end_line,
               d.source_start_byte,
               d.source_end_byte,
               d.target_start_line,
               d.target_end_line,
               tqn.value AS target_qualified_name,
               d.evidence,
               rh.value AS receiver_hint,
               res.value AS resolution,
               d.callee_start_byte,
               d.callee_end_byte,
               -- Import-scope columns (#61 per-package/per-module rework, V022): the enclosing
               -- module/block byte range a Rust `use` is lexically scoped to, plus the enclosing
               -- module body's id, so a bare reference is suppressed by this import only inside
               -- that scope. DEDICATED columns (not the callee_* overload) so the oracle's
               -- `callee_start_byte IS NOT NULL` candidate filter stays correct — import rows \
         leave
               -- callee_* NULL and never enter the SCIP occurrence join. NULL on non-import edges.
               d.import_scope_start_byte,
               d.import_scope_end_byte,
               d.import_mod_id,
               ek.value AS edge_kind,
               conf.value AS confidence,
               -- The raw dictionary ids, appended after the legacy shape: hot predicates that the
               -- planner cannot transform through the value joins (an OR-branch string equality
               -- picks a non-selective index otherwise — the query_warm regression) compare these
               -- against a constant `(SELECT id FROM name_strings WHERE value = ?)` instead.
               d.from_name_id,
               d.to_name_id,
               d.target_qualified_name_id,
               d.receiver_hint_id,
               d.edge_kind_id,
               d.confidence_id,
               d.resolution_id
        FROM edges_data d
        LEFT JOIN name_strings fn ON fn.id = d.from_name_id
        LEFT JOIN name_strings tn ON tn.id = d.to_name_id
        LEFT JOIN name_strings tqn ON tqn.id = d.target_qualified_name_id
        LEFT JOIN name_strings rh ON rh.id = d.receiver_hint_id
        LEFT JOIN name_strings res ON res.id = d.resolution_id
        LEFT JOIN name_strings ek ON ek.id = d.edge_kind_id
        LEFT JOIN name_strings conf ON conf.id = d.confidence_id
        -- #200: the internal dispatch FACT rows (`dispatch_construct`/`dispatch_handle`) are \
         inputs
        -- to `synthesize_dispatch_edges`, NOT real edges — the handle fact duplicates the
        -- dispatcher's existing `calls_name`. Hide them at the compatibility view so EVERY \
         query-layer
        -- reader (graph traversal, repo_brief, clusters, grep-augment, orientation, …) is
        -- structurally safe without each remembering an exclusion; the synthesized `dispatches` \
         edge
        -- (a real edge) stays visible. Resolution + synthesis read `edges_data` directly, so they
        -- still see the facts. Filter on the raw `edge_kind_id` (a one-time constant subselect) \
         so the
        -- planner keeps the indexed-id predicates, not a value-join string compare.
        WHERE d.edge_kind_id NOT IN (
            SELECT id FROM name_strings WHERE value IN ('dispatch_construct', 'dispatch_handle')
        );

        -- Interning per column: `INSERT OR IGNORE` + `value NOT NULL` means a NULL string is
        -- silently skipped and its id subselect yields NULL — exactly the legacy nullability.
        -- COALESCE mirrors the legacy table's column DEFAULTs for inserts that omit them.
        CREATE TRIGGER IF NOT EXISTS edges_view_insert INSTEAD OF INSERT ON edges BEGIN
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.from_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.to_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.target_qualified_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.receiver_hint);
            INSERT OR IGNORE INTO name_strings(value)
                VALUES (COALESCE(NEW.resolution, 'unresolved'));
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.edge_kind);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.confidence);
            INSERT INTO edges_data(
                id, source_file_id, from_symbol_id, to_symbol_id, from_name_id, to_name_id,
                source_start_line, source_end_line, source_start_byte, source_end_byte,
                target_start_line, target_end_line, target_qualified_name_id, evidence,
                receiver_hint_id, resolution_id, callee_start_byte, callee_end_byte,
                import_scope_start_byte, import_scope_end_byte, import_mod_id,
                edge_kind_id, confidence_id
            )
            VALUES (
                NEW.id, NEW.source_file_id, NEW.from_symbol_id, NEW.to_symbol_id,
                (SELECT id FROM name_strings WHERE value = NEW.from_name),
                (SELECT id FROM name_strings WHERE value = NEW.to_name),
                COALESCE(NEW.source_start_line, 0), COALESCE(NEW.source_end_line, 0),
                COALESCE(NEW.source_start_byte, 0), COALESCE(NEW.source_end_byte, 0),
                NEW.target_start_line, NEW.target_end_line,
                (SELECT id FROM name_strings WHERE value = NEW.target_qualified_name),
                NEW.evidence,
                (SELECT id FROM name_strings WHERE value = NEW.receiver_hint),
                (SELECT id FROM name_strings
                 WHERE value = COALESCE(NEW.resolution, 'unresolved')),
                NEW.callee_start_byte, NEW.callee_end_byte,
                NEW.import_scope_start_byte, NEW.import_scope_end_byte, NEW.import_mod_id,
                (SELECT id FROM name_strings WHERE value = NEW.edge_kind),
                (SELECT id FROM name_strings WHERE value = NEW.confidence)
            );
        END;

        CREATE TRIGGER IF NOT EXISTS edges_view_update INSTEAD OF UPDATE ON edges BEGIN
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.from_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.to_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.target_qualified_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.receiver_hint);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.resolution);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.edge_kind);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.confidence);
            UPDATE edges_data SET
                source_file_id = NEW.source_file_id,
                from_symbol_id = NEW.from_symbol_id,
                to_symbol_id = NEW.to_symbol_id,
                from_name_id = (SELECT id FROM name_strings WHERE value = NEW.from_name),
                to_name_id = (SELECT id FROM name_strings WHERE value = NEW.to_name),
                source_start_line = NEW.source_start_line,
                source_end_line = NEW.source_end_line,
                source_start_byte = NEW.source_start_byte,
                source_end_byte = NEW.source_end_byte,
                target_start_line = NEW.target_start_line,
                target_end_line = NEW.target_end_line,
                target_qualified_name_id =
                    (SELECT id FROM name_strings WHERE value = NEW.target_qualified_name),
                evidence = NEW.evidence,
                receiver_hint_id = (SELECT id FROM name_strings WHERE value = NEW.receiver_hint),
                resolution_id = (SELECT id FROM name_strings WHERE value = NEW.resolution),
                callee_start_byte = NEW.callee_start_byte,
                callee_end_byte = NEW.callee_end_byte,
                import_scope_start_byte = NEW.import_scope_start_byte,
                import_scope_end_byte = NEW.import_scope_end_byte,
                import_mod_id = NEW.import_mod_id,
                edge_kind_id = (SELECT id FROM name_strings WHERE value = NEW.edge_kind),
                confidence_id = (SELECT id FROM name_strings WHERE value = NEW.confidence)
            WHERE id = OLD.id;
        END;

        CREATE TRIGGER IF NOT EXISTS edges_view_delete INSTEAD OF DELETE ON edges BEGIN
            DELETE FROM edges_data WHERE id = OLD.id;
        END;
        ",
    )?;
    Ok(())
}

/// V020 (#79): convert a legacy `edges` TABLE into `name_strings` + `edges_data` and re-point
/// `edge_oracle`'s FK at `edges_data`. Idempotent: a DB already on the view shape skips the
/// conversion entirely. The copy runs in ONE transaction (legacy table intact on a crash);
/// `PRAGMA foreign_keys` toggles outside it (it is a no-op inside one).
pub(crate) fn apply_edge_string_interning(conn: &Connection) -> rusqlite::Result<()> {
    let legacy = conn
        .query_row("SELECT type FROM sqlite_master WHERE name = 'edges'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .as_deref()
        == Some("table");
    if legacy {
        conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
        let result = (|| -> rusqlite::Result<()> {
            conn.execute_batch(
                "
                INSERT OR IGNORE INTO name_strings(value)
                    SELECT DISTINCT from_name FROM main.edges WHERE from_name IS NOT NULL;
                INSERT OR IGNORE INTO name_strings(value) SELECT DISTINCT to_name FROM main.edges;
                INSERT OR IGNORE INTO name_strings(value)
                    SELECT DISTINCT target_qualified_name FROM main.edges
                    WHERE target_qualified_name IS NOT NULL;
                INSERT OR IGNORE INTO name_strings(value)
                    SELECT DISTINCT receiver_hint FROM main.edges WHERE receiver_hint IS NOT NULL;
                INSERT OR IGNORE INTO name_strings(value)
                    SELECT DISTINCT resolution FROM main.edges;
                INSERT OR IGNORE INTO name_strings(value)
                    SELECT DISTINCT edge_kind FROM main.edges;
                INSERT OR IGNORE INTO name_strings(value)
                    SELECT DISTINCT confidence FROM main.edges;
                INSERT INTO main.edges_data(
                    id, source_file_id, from_symbol_id, to_symbol_id, from_name_id, to_name_id,
                    source_start_line, source_end_line, source_start_byte, source_end_byte,
                    target_start_line, target_end_line, target_qualified_name_id, evidence,
                    receiver_hint_id, resolution_id, callee_start_byte, callee_end_byte,
                    edge_kind_id, confidence_id
                )
                SELECT e.id, e.source_file_id, e.from_symbol_id, e.to_symbol_id,
                       (SELECT id FROM name_strings WHERE value = e.from_name),
                       (SELECT id FROM name_strings WHERE value = e.to_name),
                       e.source_start_line, e.source_end_line,
                       e.source_start_byte, e.source_end_byte,
                       e.target_start_line, e.target_end_line,
                       (SELECT id FROM name_strings WHERE value = e.target_qualified_name),
                       e.evidence,
                       (SELECT id FROM name_strings WHERE value = e.receiver_hint),
                       (SELECT id FROM name_strings WHERE value = e.resolution),
                       e.callee_start_byte, e.callee_end_byte,
                       (SELECT id FROM name_strings WHERE value = e.edge_kind),
                       (SELECT id FROM name_strings WHERE value = e.confidence)
                FROM main.edges e;
                DROP TABLE main.edges;
                ",
            )?;
            // Re-point edge_oracle's FK from the (now dropped) legacy table at edges_data — ONLY
            // for the OLD `edge_id`-keyed V018 shape (the FK points at
            // `edges`/`edges_data`). On a fresh `apply`, V018's `apply_oracle_tables`
            // already created the V031 content-anchored shape (cols `source_path`, NO
            // FK), so there is nothing to re-point and copying its 13 columns
            // into the 8-column legacy template below would fail (#248). Skip when `source_path`
            // exists — V031 owns the final shape.
            let has_oracle: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = \
                 'edge_oracle')",
                [],
                |row| row.get(0),
            )?;
            if has_oracle && !column_exists(conn, "edge_oracle", "source_path")? {
                conn.execute_batch(
                    "
                    ALTER TABLE main.edge_oracle RENAME TO edge_oracle_legacy;
                    CREATE TABLE main.edge_oracle(
                        edge_id INTEGER NOT NULL,
                        file_sha TEXT NOT NULL,
                        tool TEXT NOT NULL,
                        tool_version TEXT NOT NULL,
                        resolved_symbol_id INTEGER,
                        scip_symbol TEXT NOT NULL,
                        kind TEXT NOT NULL,
                        computed_at INTEGER NOT NULL,
                        PRIMARY KEY(edge_id, tool, tool_version),
                        FOREIGN KEY(edge_id) REFERENCES edges_data(id) ON DELETE CASCADE
                    ) STRICT;
                    INSERT INTO main.edge_oracle SELECT * FROM main.edge_oracle_legacy;
                    DROP TABLE main.edge_oracle_legacy;
                    CREATE INDEX IF NOT EXISTS idx_edge_oracle_staleness
                        ON edge_oracle(file_sha, tool, tool_version);
                    CREATE INDEX IF NOT EXISTS idx_edge_oracle_symbol
                        ON edge_oracle(resolved_symbol_id);
                    ",
                )?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
            return result;
        }
        conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    }
    ensure_edges_data_indexes(conn)?;
    ensure_edges_view(conn)?;
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
            MIGRATION_020_ID => Some(20),
            MIGRATION_021_ID => Some(21),
            MIGRATION_022_ID => Some(22),
            MIGRATION_023_ID => Some(23),
            MIGRATION_024_ID => Some(24),
            MIGRATION_025_ID => Some(25),
            MIGRATION_026_ID => Some(26),
            MIGRATION_027_ID => Some(27),
            MIGRATION_028_ID => Some(28),
            MIGRATION_029_ID => Some(29),
            MIGRATION_030_ID => Some(30),
            MIGRATION_031_ID => Some(31),
            MIGRATION_032_ID => Some(32),
            MIGRATION_033_ID => Some(33),
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
            | MIGRATION_020_ID
            | MIGRATION_021_ID
            | MIGRATION_022_ID
            | MIGRATION_023_ID
            | MIGRATION_024_ID
            | MIGRATION_025_ID
            | MIGRATION_026_ID
            | MIGRATION_027_ID
            | MIGRATION_028_ID
            | MIGRATION_029_ID
            | MIGRATION_030_ID
            | MIGRATION_031_ID
            | MIGRATION_032_ID
            | MIGRATION_033_ID
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
        MIGRATION_020_ID => migration.checksum != MIGRATION_020_CHECKSUM,
        MIGRATION_021_ID => migration.checksum != MIGRATION_021_CHECKSUM,
        MIGRATION_022_ID => migration.checksum != MIGRATION_022_CHECKSUM,
        MIGRATION_023_ID => migration.checksum != MIGRATION_023_CHECKSUM,
        MIGRATION_024_ID => migration.checksum != MIGRATION_024_CHECKSUM,
        MIGRATION_025_ID => migration.checksum != MIGRATION_025_CHECKSUM,
        MIGRATION_026_ID => migration.checksum != MIGRATION_026_CHECKSUM,
        MIGRATION_027_ID => migration.checksum != MIGRATION_027_CHECKSUM,
        MIGRATION_028_ID => migration.checksum != MIGRATION_028_CHECKSUM,
        MIGRATION_029_ID => migration.checksum != MIGRATION_029_CHECKSUM,
        MIGRATION_030_ID => migration.checksum != MIGRATION_030_CHECKSUM,
        MIGRATION_031_ID => migration.checksum != MIGRATION_031_CHECKSUM,
        MIGRATION_032_ID => migration.checksum != MIGRATION_032_CHECKSUM,
        MIGRATION_033_ID => migration.checksum != MIGRATION_033_CHECKSUM,
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

pub(crate) fn column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// V027 (#77 Phase 2): retire the `chunks.text` column — the irreversible payoff step. A fresh DB's
/// baseline already omits the column; a forward-migrated index still has it plus a (possibly empty)
/// `chunk_text` store. Build the compressed store FROM `chunks.text` (its last read), guaranteeing
/// every chunk has a blob, THEN drop the column. Guarded by `column_exists` so a fresh DB (no
/// column) or a re-run is a clean no-op.
pub(crate) fn apply_drop_chunks_text(conn: &Connection) -> rusqlite::Result<()> {
    if !column_exists(conn, "chunks", "text")? {
        return Ok(());
    }
    crate::index::chunk_text_store::build_store(conn, "(SELECT id AS chunk_id, text FROM chunks)")
        .map_err(|err| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!("V027 chunk_text backfill failed before dropping chunks.text: {err}")),
            )
        })?;
    conn.execute("ALTER TABLE chunks DROP COLUMN text", [])?;
    Ok(())
}

/// V029 (#215, rework R1): clone-detection fingerprint substrate. All tables are scope-INDEPENDENT
/// — symbol_fingerprints/symbol_token_postings key by symbol_id (FK CASCADE discards them on
/// reindex); clone_token_df is a derived selectivity cache; clone_refinements keys by content
/// (class_key). NEVER add scope columns here (see mem_19ec7384a9b).
///
/// R1 replaces the MinHash/LSH fingerprint_bands table with a SourcererCC-style inverted-index
/// pair: symbol_token_postings (per-symbol token bag) + clone_token_df (document-frequency cache).
pub(crate) const CLONE_FINGERPRINT_DDL: &str = "
    CREATE TABLE IF NOT EXISTS symbol_fingerprints(
        symbol_id          INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
        normalizer_kind    TEXT    NOT NULL,            -- baseline | scip
        normalizer_version INTEGER NOT NULL,
        oracle_run_id      INTEGER,                     -- NULL for baseline rows
        struct_hash        TEXT    NOT NULL,
        token_len          INTEGER NOT NULL,
        created_at_ms      INTEGER NOT NULL,
        PRIMARY KEY (symbol_id, normalizer_kind)
    ) STRICT;
    CREATE INDEX IF NOT EXISTS idx_symbol_fingerprints_struct
        ON symbol_fingerprints(normalizer_kind, struct_hash);
    CREATE TABLE IF NOT EXISTS symbol_token_postings(
        symbol_id       INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
        normalizer_kind TEXT    NOT NULL,
        token_hash      INTEGER NOT NULL,              -- FNV-1a(token) as signed i64
        freq            INTEGER NOT NULL,
        PRIMARY KEY (symbol_id, normalizer_kind, token_hash)
    ) STRICT;
    -- Plan-1 candidate read loads postings by symbol_id (the PK prefix) and builds the inverted
    -- index in Rust; a token_hash secondary index is unused. Plan 2 re-adds one if
    -- clones_for_symbol moves the reverse lookup to SQL.
    CREATE TABLE IF NOT EXISTS clone_token_df(
        normalizer_kind TEXT    NOT NULL,
        token_hash      INTEGER NOT NULL,
        df              INTEGER NOT NULL,
        PRIMARY KEY (normalizer_kind, token_hash)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS clone_refinements(
        class_key               TEXT    PRIMARY KEY,
        language                TEXT    NOT NULL,
        refine_mode             TEXT    NOT NULL,        -- baseline | scip
        template                TEXT    NOT NULL,
        variation_points_json   TEXT    NOT NULL CHECK (json_valid(variation_points_json)),
        proposed_signature_json TEXT    NOT NULL CHECK (json_valid(proposed_signature_json)),
        confidence              TEXT    NOT NULL,
        anti_unify_coverage     REAL    NOT NULL,
        lcs_ratio               REAL    NOT NULL,
        refactorability         REAL    NOT NULL,
        norm_version            INTEGER NOT NULL,
        alignment_version       INTEGER NOT NULL,
        created_at_ms           INTEGER NOT NULL,
        -- 1 when this refinement's LCS fidelity engaged a cost cap (member-count sample or the
        -- per-pair length proxy). Persisted so a warm cache hit can still report `metrics_sampled`
        -- for the long-sequence dimension. Present in the V029 DDL for fresh DBs; existing-V029
        -- indexes get it via the V030 migration (apply_clone_refinements_lcs_sampled).
        lcs_sampled             INTEGER NOT NULL DEFAULT 0
    ) STRICT;
";

/// V029 (#215): create the clone-detection substrate tables.
///
/// `lcs_sampled` is present in the V029 CREATE TABLE DDL so fresh DBs get it here. Existing
/// indexes recorded at V029 before the column landed are healed by V030
/// (`apply_clone_refinements_lcs_sampled`) — an already-applied migration's apply fn is never
/// re-invoked on an existing DB, so this function cannot add the column retroactively.
///
/// Population gap: V029 only CREATEs the clone tables. Their rows (`symbol_fingerprints` /
/// `symbol_token_postings` / `clone_token_df`) populate as files are (re)indexed — there is no
/// backfill here. An existing index migrated forward therefore has EMPTY clone tables until a
/// `rag-rat index --full`. Backfilling at migration time is intentionally NOT done: it would
/// require parsing the entire repo inside a migration.
pub(crate) fn apply_clone_fingerprint_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CLONE_FINGERPRINT_DDL)?;
    Ok(())
}

/// V030 (#215 Plan 4a): add `clone_refinements.lcs_sampled` to indexes already recorded at V029.
///
/// Fresh DBs get the column from the V029 CREATE TABLE DDL (via baseline → apply_clone_fingerprint_
/// tables); this migration is the upgrade path for existing-V029 indexes where the column was
/// absent because V029 was applied before `lcs_sampled` landed. Idempotent: `add_column_if_missing`
/// is a no-op when the column already exists.
pub(crate) fn apply_clone_refinements_lcs_sampled(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "clone_refinements", "lcs_sampled", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

/// V031 (#248): rebuild `edge_oracle` content-anchored — drop the `edges_data` FK + the volatile
/// `edge_id` PK, key by the edge's CONTENT identity instead, so verdicts SURVIVE reindex.
///
/// THE BUG: V018 keyed `edge_oracle` on `edge_id` with `FOREIGN KEY(edge_id) REFERENCES
/// edges_data(id) ON DELETE CASCADE`. Every reindex rewrites `edges_data` (full rebuild +
/// `remove_file_in_scope`), so the cascade wiped every verdict; the oracle is opt-in (no auto-run)
/// so it never repopulated. The fix mirrors `logical_symbol_monikers`: content key + no
/// reindex-cascading FK, reads join LIVE edges so a dangling row never resolves.
///
/// DATA: the rebuild RENAME→CREATE→DROP **drops** any legacy verdicts. A DB where the oracle ran
/// but hasn't reindexed has rows, but the legacy rows lack the content-key columns and back-filling
/// them via `edge_id → edges_data` is lossy (the edges may already be gone). `edge_oracle` is
/// ephemeral + opt-in; the next `oracle run` repopulates with the new shape. We accept the one-time
/// drop — NO INSERT SELECT.
///
/// SQLite can't drop a FK in place, so this is a table REBUILD using the V020 recipe: `PRAGMA
/// foreign_keys=OFF` OUTSIDE `BEGIN IMMEDIATE`, RENAME→CREATE→DROP, recreate indexes, ROLLBACK on
/// error, then `COMMIT; PRAGMA foreign_keys=ON`.
///
/// IDEMPOTENT: a fresh DB ran the NEW `apply_oracle_tables` shape at V018, so V031 must be a no-op
/// there — short-circuit when `edge_oracle` already has the content-key columns (or no
/// `edges_data` FK). `migrate_forward` only replays unapplied steps, but a re-run after a partial
/// apply must still be safe, hence the guard.
pub(crate) fn apply_edge_oracle_content_anchor(conn: &Connection) -> rusqlite::Result<()> {
    // Short-circuit if already on the content-anchored shape (fresh DB at V018, or a re-run): the
    // new table has `source_path` and no `edges_data` FK. Detect via the column; the FK-list check
    // is the belt-and-suspenders companion (an empty foreign_key_list == no cascade to wipe).
    if column_exists(conn, "edge_oracle", "source_path")? {
        return Ok(());
    }

    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        conn.execute_batch(
            "
            ALTER TABLE main.edge_oracle RENAME TO edge_oracle_legacy;
            CREATE TABLE main.edge_oracle(
                source_path TEXT NOT NULL,
                source_start_byte INTEGER NOT NULL,
                source_end_byte INTEGER NOT NULL,
                callee_start_byte INTEGER NOT NULL,
                callee_end_byte INTEGER NOT NULL,
                edge_kind TEXT NOT NULL,
                file_sha TEXT NOT NULL,
                tool TEXT NOT NULL,
                tool_version TEXT NOT NULL,
                resolved_symbol_id INTEGER,
                scip_symbol TEXT NOT NULL,
                kind TEXT NOT NULL,
                computed_at INTEGER NOT NULL,
                PRIMARY KEY(
                    tool, tool_version, source_path,
                    source_start_byte, source_end_byte,
                    callee_start_byte, callee_end_byte, edge_kind
                )
            ) STRICT;
            -- NO INSERT SELECT: the legacy rows lack the content-key columns; we accept the
            -- one-time verdict drop (ephemeral/opt-in; the next oracle run repopulates).
            DROP TABLE main.edge_oracle_legacy;
            CREATE INDEX IF NOT EXISTS idx_edge_oracle_staleness
                ON edge_oracle(file_sha, tool, tool_version);
            CREATE INDEX IF NOT EXISTS idx_edge_oracle_symbol
                ON edge_oracle(resolved_symbol_id);
            CREATE INDEX IF NOT EXISTS idx_edge_oracle_anchor
                ON edge_oracle(source_path, callee_start_byte, callee_end_byte, edge_kind);
            ",
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
        return result;
    }
    conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// V032 (#231): BLOB-pack the clone token bag. Add `symbol_fingerprints.token_bag BLOB` and DROP
/// `symbol_token_postings` — collapsing the ~490k single-row postings INSERTs of a full rebuild
/// into ONE serialized `(token_hash, freq)` BLOB per fingerprint row
/// (`bag_blob::encode_token_bag`). The candidate read decodes the BLOB back into the same
/// `(token_hash, freq)` multiset, so recall is byte-identical on a fully (re)indexed DB;
/// `clone_token_df` is recomputed by aggregating the BLOBs.
///
/// SHAPE (R5): follows the V031 idempotency-guarded-transform pattern — V029's
/// `CLONE_FINGERPRINT_DDL` (which still CREATEs both tables) is NEVER edited. On a FRESH DB, V029
/// creates `symbol_token_postings` and this migration drops it; on an EXISTING DB it does the same.
/// Guard on the `token_bag` column so a re-run (or a fresh DB already transformed) is a clean
/// no-op.
///
/// NO BACK-FILL (R8): existing fingerprint rows get `token_bag = NULL` on `ADD COLUMN`. The
/// candidate read SKIPs NULL-bag rows, so clone recall is undefined for those symbols until the
/// post-migration reindex repopulates them — the same one-time-loss posture as V029/V031 (clone
/// data is rebuildable; no parse-the-whole-repo work belongs in a migration).
pub(crate) fn apply_token_bag_blob(conn: &Connection) -> rusqlite::Result<()> {
    // Both operations are individually idempotent — NO early-return guard. A short-circuit on the
    // column's presence would be a bug here: `apply` runs this transform from BOTH the baseline
    // (apply_baseline) AND the V032 migration step, and V029's `CREATE TABLE IF NOT EXISTS
    // symbol_token_postings` runs BETWEEN them (the migration replay re-creates the table the
    // baseline already dropped). An early return keyed on the now-present `token_bag` column would
    // skip the DROP and leave that re-created postings table behind. Running both ops
    // unconditionally (each a no-op when already in the target state) converges regardless of
    // call order.
    //
    // Additive column — STRICT-valid BLOB type; existing rows default to NULL (R8).
    add_column_if_missing(conn, "symbol_fingerprints", "token_bag", "BLOB")?;
    // The per-token inverted-index table is replaced by the BLOB; its indexes drop with it.
    conn.execute_batch("DROP TABLE IF EXISTS symbol_token_postings;")?;
    Ok(())
}

/// V033 (#122): the dream-mode worklist. Findings are ABOUT memories (a reviewable triage list),
/// NEVER mutations of them — dream mode proposes, a human/strong-agent confirms; nothing here ever
/// rewrites a `repo_memories` row. Identity is `(kind, subject, claim_hash)`: a re-run with the
/// same claim_hash REFRESHES (no duplicate), a materially-changed finding SUPERSEDES the prior one,
/// and a finding the run no longer reports is RESOLVED. `subject` is polymorphic (a
/// `repo_memories.id` for memory-scoped kinds, a symbol/path ref for `coverage_gap`) so it carries
/// NO FK. `status` drives the lifecycle; `base_rank` + the `first_seen_at_ms` clock drive age
/// decay. Additive + idempotent.
pub(crate) fn apply_dream_findings(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS dream_findings(
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            subject TEXT NOT NULL,
            claim_hash TEXT NOT NULL,
            evidence TEXT NOT NULL,
            base_rank REAL NOT NULL,
            status TEXT NOT NULL DEFAULT 'open',
            superseded_by TEXT,
            first_seen_at_ms INTEGER NOT NULL,
            last_seen_at_ms INTEGER NOT NULL,
            reviewed_at_ms INTEGER,
            UNIQUE(kind, subject, claim_hash)
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_dream_findings_status ON dream_findings(status);
        CREATE INDEX IF NOT EXISTS idx_dream_findings_subject ON dream_findings(kind, subject);
        ",
    )?;
    Ok(())
}

/// V028 (#224): intern `symbols.qualified_name` + `logical_symbols.qualified_name` into the shared
/// `name_strings` pool (the pool `edges_data` already references; the `edge_strings → name_strings`
/// rename rides this version bump and is performed in `provision_baseline`, which runs BEFORE this
/// replay — so `name_strings` is guaranteed present here). Backfill-before-drop, like the V027
/// chunk-text precedent, so the column drop is the last step rather than an irreversible one-shot.
///
/// Idempotent + guarded so a fresh-baseline DB (already `qualified_name_id`, no `qualified_name`)
/// is a clean no-op and a re-run after a half-apply is safe:
/// 1. ADD `qualified_name_id INTEGER` to both tables (guarded — NULLABLE because an `ADD COLUMN …
///    NOT NULL` fails on a populated table, so the fresh baseline matches).
/// 2. Backfill ONLY while the old `qualified_name` column still exists: insert the not-yet-present
///    qnames (`INSERT OR IGNORE` — the ~85% already stored as edge-target names are skipped; the
///    new ~102K get fresh ids, and plain-PK reuse of prior gc gaps is safe because gc only deletes
///    orphans), then set `qualified_name_id` from the pool.
/// 3. Create the id-keyed indexes; drop the old string indexes.
/// 4. Drop the `qualified_name` column (guarded by `column_exists`).
pub(crate) fn apply_intern_symbol_qualified_names(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "symbols", "qualified_name_id", "INTEGER")?;
    add_column_if_missing(conn, "logical_symbols", "qualified_name_id", "INTEGER")?;
    // Backfill reads the OLD text column; on a fresh-baseline DB it is already gone (and the tables
    // are empty), so guard each table independently — one may have migrated and the other not on a
    // re-run after a partial apply.
    if column_exists(conn, "symbols", "qualified_name")? {
        conn.execute_batch(
            "
            INSERT OR IGNORE INTO name_strings(value) SELECT qualified_name FROM symbols;
            UPDATE symbols
               SET qualified_name_id =
                   (SELECT id FROM name_strings WHERE name_strings.value = symbols.qualified_name);
            ",
        )?;
    }
    if column_exists(conn, "logical_symbols", "qualified_name")? {
        conn.execute_batch(
            "
            INSERT OR IGNORE INTO name_strings(value) SELECT qualified_name FROM logical_symbols;
            UPDATE logical_symbols
               SET qualified_name_id =
                   (SELECT id FROM name_strings
                    WHERE name_strings.value = logical_symbols.qualified_name);
            ",
        )?;
    }
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_symbols_qualified_name_id
            ON symbols(qualified_name_id);
        CREATE INDEX IF NOT EXISTS idx_logical_symbols_qualified_name_id
            ON logical_symbols(qualified_name_id);
        DROP INDEX IF EXISTS idx_symbols_qualified_name;
        DROP INDEX IF EXISTS idx_logical_symbols_qualified_name;
        ",
    )?;
    if column_exists(conn, "symbols", "qualified_name")? {
        conn.execute("ALTER TABLE symbols DROP COLUMN qualified_name", [])?;
    }
    if column_exists(conn, "logical_symbols", "qualified_name")? {
        conn.execute("ALTER TABLE logical_symbols DROP COLUMN qualified_name", [])?;
    }
    Ok(())
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
