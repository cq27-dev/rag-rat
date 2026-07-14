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
    // Legacy-only (pre-V060): `github_ref_sync` exists so the later V041/V044 github migrations
    // can widen it before V060 folds the whole legacy cache into the papertrail_* tables. On a
    // fresh DB the baseline creates NO github_* tables at all (`github_fts` absent is the
    // post-V060 signature), so creating this one here would only manufacture a dead legacy table
    // for V060 to drop — skip instead.
    if !sqlite_object_exists(conn, "table", "github_fts")? {
        return Ok(());
    }
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

/// V056 (#114): `external_symbols` — the per-moniker `SymbolInformation` (`kind`, `display_name`,
/// `signature_documentation.text`, `documentation`, a derived `deprecated` flag) that `oracle run`
/// parses out of `index.external_symbols` and previously DISCARDED. This is the dependency-side
/// contract that `check_library_usage` joins to external call sites to surface signature/docs as
/// inline context and to assert deprecated-but-compiling usage.
///
/// JOIN CONTRACT (load-bearing): `moniker` is the RAW SCIP symbol string, stored byte-for-byte as
/// it appears in `SymbolInformation.symbol` — the SAME form `edge_oracle.scip_symbol` stores (an
/// occurrence's `symbol`, unstabilized; see `oracle::join::classify_edge`). The read join is an
/// exact string match on `moniker = edge_oracle.scip_symbol`; applying `stabilize_moniker_version`
/// to one side and not the other would silently break it. External monikers carry the dependency's
/// real version, so a cross-version re-index naturally produces distinct rows (the drift the spike
/// targets).
///
/// INVARIANT (oracle-persisted, #248): NO foreign key — the table is content/moniker-keyed and its
/// reads JOIN live `edge_oracle`, so a dangling row never resolves rather than being CASCADE-wiped
/// on reindex. Listed in [`super::ORACLE_PERSISTED_TABLES`]. Born post-A5, so `repo_id` is a birth
/// column and leads the PK.
///
/// CHECKOUT-SCOPED like its run sibling `oracle_runs` (NOT like `logical_symbol_monikers`): the
/// contract set is the product of ONE oracle run in ONE checkout, so the PK carries `(commit_sha,
/// worktree_id)`. Two linked worktrees of the same repo — at different dependency versions — keep
/// DISJOINT contract sets, so the later run's authoritative per-`(tool, checkout)` clear cannot
/// erase a sibling checkout's contracts (the same multi-worktree isolation `edge_oracle` /
/// `oracle_runs` already enforce). `tool_version` rides along as write provenance; the moniker's
/// version component still distinguishes dependency versions within a checkout.
pub(crate) fn apply_external_symbols(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS external_symbols(
            repo_id            TEXT NOT NULL,
            tool               TEXT NOT NULL,
            tool_version       TEXT NOT NULL,
            commit_sha         TEXT NOT NULL,
            worktree_id        TEXT NOT NULL,
            moniker            TEXT NOT NULL,
            kind               TEXT NOT NULL,
            display_name       TEXT NOT NULL,
            signature_text     TEXT NOT NULL,
            signature_language TEXT NOT NULL,
            documentation      TEXT NOT NULL,
            deprecated         INTEGER NOT NULL,
            computed_at_ms     INTEGER NOT NULL,
            PRIMARY KEY(repo_id, tool, commit_sha, worktree_id, moniker)
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_external_symbols_deprecated
            ON external_symbols(repo_id, tool, commit_sha, worktree_id, deprecated);
        ",
    )
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
            MIGRATION_034_ID => Some(34),
            MIGRATION_035_ID => Some(35),
            MIGRATION_036_ID => Some(36),
            MIGRATION_037_ID => Some(37),
            MIGRATION_038_ID => Some(38),
            MIGRATION_039_ID => Some(39),
            MIGRATION_040_ID => Some(40),
            MIGRATION_041_ID => Some(41),
            MIGRATION_042_ID => Some(42),
            MIGRATION_043_ID => Some(43),
            MIGRATION_044_ID => Some(44),
            MIGRATION_045_ID => Some(45),
            MIGRATION_046_ID => Some(46),
            MIGRATION_047_ID => Some(47),
            MIGRATION_048_ID => Some(48),
            MIGRATION_049_ID => Some(49),
            MIGRATION_050_ID => Some(50),
            MIGRATION_051_ID => Some(51),
            MIGRATION_052_ID => Some(52),
            MIGRATION_053_ID => Some(53),
            MIGRATION_054_ID => Some(54),
            MIGRATION_055_ID => Some(55),
            MIGRATION_056_ID => Some(56),
            MIGRATION_057_ID => Some(57),
            MIGRATION_058_ID => Some(58),
            MIGRATION_059_ID => Some(59),
            MIGRATION_060_ID => Some(60),
            MIGRATION_061_ID => Some(61),
            MIGRATION_062_ID => Some(62),
            MIGRATION_063_ID => Some(63),
            MIGRATION_064_ID => Some(64),
            MIGRATION_065_ID => Some(65),
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
            | MIGRATION_034_ID
            | MIGRATION_035_ID
            | MIGRATION_036_ID
            | MIGRATION_037_ID
            | MIGRATION_038_ID
            | MIGRATION_039_ID
            | MIGRATION_040_ID
            | MIGRATION_041_ID
            | MIGRATION_042_ID
            | MIGRATION_043_ID
            | MIGRATION_044_ID
            | MIGRATION_045_ID
            | MIGRATION_046_ID
            | MIGRATION_047_ID
            | MIGRATION_048_ID
            | MIGRATION_049_ID
            | MIGRATION_050_ID
            | MIGRATION_051_ID
            | MIGRATION_052_ID
            | MIGRATION_053_ID
            | MIGRATION_054_ID
            | MIGRATION_055_ID
            | MIGRATION_056_ID
            | MIGRATION_057_ID
            | MIGRATION_058_ID
            | MIGRATION_059_ID
            | MIGRATION_060_ID
            | MIGRATION_061_ID
            | MIGRATION_062_ID
            | MIGRATION_063_ID
            | MIGRATION_064_ID
            | MIGRATION_065_ID
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
        MIGRATION_034_ID => migration.checksum != MIGRATION_034_CHECKSUM,
        MIGRATION_035_ID => migration.checksum != MIGRATION_035_CHECKSUM,
        MIGRATION_036_ID => migration.checksum != MIGRATION_036_CHECKSUM,
        MIGRATION_037_ID => migration.checksum != MIGRATION_037_CHECKSUM,
        MIGRATION_038_ID => migration.checksum != MIGRATION_038_CHECKSUM,
        MIGRATION_039_ID => migration.checksum != MIGRATION_039_CHECKSUM,
        MIGRATION_040_ID => migration.checksum != MIGRATION_040_CHECKSUM,
        MIGRATION_041_ID => migration.checksum != MIGRATION_041_CHECKSUM,
        MIGRATION_042_ID => migration.checksum != MIGRATION_042_CHECKSUM,
        MIGRATION_043_ID => migration.checksum != MIGRATION_043_CHECKSUM,
        MIGRATION_044_ID => migration.checksum != MIGRATION_044_CHECKSUM,
        MIGRATION_045_ID => migration.checksum != MIGRATION_045_CHECKSUM,
        MIGRATION_046_ID => migration.checksum != MIGRATION_046_CHECKSUM,
        MIGRATION_047_ID => migration.checksum != MIGRATION_047_CHECKSUM,
        MIGRATION_048_ID => migration.checksum != MIGRATION_048_CHECKSUM,
        MIGRATION_049_ID => migration.checksum != MIGRATION_049_CHECKSUM,
        MIGRATION_050_ID => migration.checksum != MIGRATION_050_CHECKSUM,
        MIGRATION_051_ID => migration.checksum != MIGRATION_051_CHECKSUM,
        MIGRATION_052_ID => migration.checksum != MIGRATION_052_CHECKSUM,
        MIGRATION_053_ID => migration.checksum != MIGRATION_053_CHECKSUM,
        MIGRATION_054_ID => migration.checksum != MIGRATION_054_CHECKSUM,
        MIGRATION_055_ID => migration.checksum != MIGRATION_055_CHECKSUM,
        MIGRATION_056_ID => migration.checksum != MIGRATION_056_CHECKSUM,
        MIGRATION_057_ID => migration.checksum != MIGRATION_057_CHECKSUM,
        MIGRATION_058_ID => migration.checksum != MIGRATION_058_CHECKSUM,
        MIGRATION_059_ID => migration.checksum != MIGRATION_059_CHECKSUM,
        MIGRATION_060_ID => migration.checksum != MIGRATION_060_CHECKSUM,
        MIGRATION_061_ID => migration.checksum != MIGRATION_061_CHECKSUM,
        MIGRATION_062_ID => migration.checksum != MIGRATION_062_CHECKSUM,
        MIGRATION_063_ID => migration.checksum != MIGRATION_063_CHECKSUM,
        MIGRATION_064_ID => migration.checksum != MIGRATION_064_CHECKSUM,
        MIGRATION_065_ID => migration.checksum != MIGRATION_065_CHECKSUM,
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

/// GLOBAL `index_meta` keys recording WHO last brought this store's schema current (#585). About
/// the DB file, not a repo — so they live in `index_meta`, not `repo_meta`. Overwritten each time a
/// migration/create runs, so a stranded fleet is diagnosable in one query instead of forensics.
pub(crate) const MIGRATION_PROVENANCE_KEYS: &[&str] = &[
    "last_migration_binary_version",
    "last_migration_binary_exe",
    "last_migration_to_version",
    "last_migration_at_ms",
];

/// Stamp the migration-provenance keys after the schema is brought current. The binary version is
/// [`crate::binary_version`] (the CLI's git-stamped `RAG_RAT_VERSION`, else `CARGO_PKG_VERSION`),
/// so a dev build that migrates a shared store leaves a `+g<hash>` fingerprint that names it.
///
/// ONE atomic multi-row upsert: statement-level atomicity means a mid-write failure (e.g. a
/// concurrent writer's `SQLITE_BUSY`) leaves NO partial provenance — all four keys land or none do,
/// so the reader never sees a half-written record. Call sites treat a failure here as best-effort
/// (the migration already committed; provenance is diagnostic), so a stamp failure never fails the
/// migration — it just leaves the record absent until the next one, which the reader handles.
pub(crate) fn record_migration_provenance(conn: &Connection) -> rusqlite::Result<()> {
    let exe =
        std::env::current_exe().ok().map(|path| path.display().to_string()).unwrap_or_default();
    conn.execute(
        "INSERT INTO index_meta(key, value) VALUES (?1, ?2), (?3, ?4), (?5, ?6), (?7, ?8) ON \
         CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![
            MIGRATION_PROVENANCE_KEYS[0],
            crate::binary_version(),
            MIGRATION_PROVENANCE_KEYS[1],
            exe,
            MIGRATION_PROVENANCE_KEYS[2],
            LATEST_SCHEMA_VERSION.to_string(),
            MIGRATION_PROVENANCE_KEYS[3],
            now_ms().to_string(),
        ],
    )?;
    Ok(())
}

/// A human note naming who last migrated this store, appended to the `Newer` refusal (#585). Empty
/// when no provenance is recorded (an older DB, or one never migrated by a provenance-aware
/// binary). Defensive: any read error yields "" — `status` must never fail on a weird DB.
pub(crate) fn migration_provenance_note(conn: &Connection) -> String {
    let read = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .filter(|value| !value.is_empty())
    };
    match (read("last_migration_binary_version"), read("last_migration_binary_exe")) {
        (Some(version), Some(exe)) => {
            format!("; the index was last migrated by rag-rat {version} at {exe}")
        },
        (Some(version), None) => format!("; the index was last migrated by rag-rat {version}"),
        _ => String::new(),
    }
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

/// V034: the precomputed clone-edge graph, so `find_clones` reads a persisted graph instead of
/// recomputing the super-linear SourcererCC candidate pairs every query (it does not finish in 240s
/// on a 118k-function index). Computed at θ = `CLONE_PRECOMPUTE_THETA` (0.7).
///
/// Generation-staged: the resumable background recompute writes a new `build_generation`; reads
/// serve the latest `Complete` generation (the `clone_graph_live_generation` meta key); the pointer
/// flips atomically on completion so a half-built generation is never served. GC of a superseded
/// generation CASCADEs its edges — `clone_graph_generations` is DURABLE precompute metadata, not a
/// `REINDEX_VOLATILE_PARENT`, so that CASCADE FK is allowed.
///
/// CONTENT-ANCHORED endpoints (the #248 bug-class rule, enforced by
/// `no_table_has_a_reindex_cascading_fk_to_a_volatile_parent`): this is durable output that MUST
/// survive reindex, so it carries NO `ON DELETE CASCADE` FK to `symbols` (a
/// `REINDEX_VOLATILE_PARENT` whose ids are reassigned on reindex — keying on `symbol_id` is the
/// exact #248 bug that wiped `edge_oracle` verdicts). Each endpoint is the reindex-stable `(path,
/// start_byte)` of a symbol plus the `file_sha` (`files.sha256`) at compute time — the same
/// content-key/staleness pattern as `edge_oracle`. Reads resolve an endpoint by joining live
/// `symbols`/`files` on `(path, start_byte)` AND `files.sha256 = *_file_sha`; a deleted or edited
/// endpoint simply does not resolve, so a dangling/stale edge is dropped at read (never a ghost
/// member).
///
/// `overlap` + both `token_len`s are the exact `verified_clone` gate inputs, so any query θ ≥ 0.7
/// reproduces `overlap >= ceil(θ * max_len)` precisely by filtering stored rows. Struct-hash exact
/// pairs carry `similarity = 1.0` so they survive every θ.
///
/// Population gap (as with the V029 clone tables): this migration only CREATEs the tables. They
/// populate when a precompute pass runs (watcher maintenance / `rag-rat clones --precompute`);
/// there is no backfill at migration time. Until then `find_clones` uses its live path unchanged.
/// The sub-block inverted index the resumable build streams against is rebuilt in RAM from
/// `symbol_fingerprints.token_bag` each pass (cheap relative to pair emission); a PERSISTED
/// postings table is deferred to the incremental-maintenance follow-up, where it would itself be
/// content-anchored.
pub(crate) const CLONE_GRAPH_DDL: &str = "
    CREATE TABLE IF NOT EXISTS clone_graph_generations(
        generation         INTEGER PRIMARY KEY,
        status             TEXT    NOT NULL CHECK (status IN ('Building', 'Complete')),
        theta_floor        REAL    NOT NULL,
        normalizer_kind    TEXT    NOT NULL,            -- baseline
        normalizer_version INTEGER NOT NULL,            -- NORM_VERSION at build
        source_revision    TEXT    NOT NULL,            -- content_revision() this generation \
                                          builds toward
        cursor_symbol_id   INTEGER NOT NULL DEFAULT 0,  -- build-local resume point (last \
                                          symbol_id emitted)
        edges_written      INTEGER NOT NULL DEFAULT 0,
        started_at_ms      INTEGER NOT NULL,
        finished_at_ms     INTEGER
    ) STRICT;
    CREATE TABLE IF NOT EXISTS clone_edges(
        build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation) ON DELETE \
                                          CASCADE,
        -- Content-anchored endpoints: NO symbol_id FK (#248 rule). Canonical a < b by (path, \
                                          start_byte).
        a_path           TEXT    NOT NULL,
        a_start_byte     INTEGER NOT NULL,
        a_file_sha       TEXT    NOT NULL,              -- files.sha256 at compute; read-time \
                                          staleness filter
        b_path           TEXT    NOT NULL,
        b_start_byte     INTEGER NOT NULL,
        b_file_sha       TEXT    NOT NULL,
        overlap          INTEGER NOT NULL,              -- Σ min(freq) = verified_clone overlap
        a_token_len      INTEGER NOT NULL,
        b_token_len      INTEGER NOT NULL,
        similarity       REAL    NOT NULL,              -- overlap/max_len; 1.0 for \
                                          struct-hash-exact pairs
        edge_source      TEXT    NOT NULL,              -- 'struct_hash' | 'sub_block'
        PRIMARY KEY (build_generation, a_path, a_start_byte, b_path, b_start_byte)
    ) STRICT;
    CREATE INDEX IF NOT EXISTS idx_clone_edges_b
        ON clone_edges(build_generation, b_path, b_start_byte);
";

/// V034: create the precomputed clone-graph tables (see [`CLONE_GRAPH_DDL`]).
pub(crate) fn apply_clone_graph_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CLONE_GRAPH_DDL)?;
    Ok(())
}

/// V037 (#296): PERSIST the sub-block postings the write-time clone check reads, so that check
/// SCALES past the 40k-function guard. `clones_of_text` (the write-time hook engine) today rebuilds
/// the whole `(sub_block_token -> symbol)` inverted index in RAM on every call (O(functions)), so
/// the hook no-ops above `MAX_CLONE_CHECK_FUNCTIONS`; persisting the postings lets it do a bounded
/// indexed lookup per new function instead. This is the follow-up the V034 doc named: the slot was
/// deliberately DEFERRED there (see [`CLONE_GRAPH_DDL`]) and is filled here.
///
/// CONTENT-ANCHORED endpoints (the #248 bug-class rule, enforced by
/// `no_table_has_a_reindex_cascading_fk_to_a_volatile_parent`): a posting anchors to the
/// reindex-stable `(path, start_byte)` of a symbol plus the `file_sha` (`files.sha256`) at compute
/// time — NEVER a `symbol_id` FK (`symbols` is a `REINDEX_VOLATILE_PARENT` whose ids are reassigned
/// on reindex; keying on `symbol_id` is the exact #248 bug that wiped `edge_oracle` verdicts). The
/// read resolves an anchor by joining live `symbols`/`files` on `(path, start_byte)` and drops any
/// row whose `file_sha` no longer matches the last-indexed `files.sha256`, so a stale posting is
/// silently ignored rather than matched against changed content. The ONLY FK is the `ON DELETE
/// CASCADE` to the DURABLE `clone_graph_generations` (precompute metadata, not a
/// `REINDEX_VOLATILE_PARENT`, so that CASCADE is allowed): postings live and die with their build
/// generation, and a superseded generation's postings are GC'd by that cascade — the same
/// generation-staged lifecycle `clone_edges` uses, no independent freshness key.
///
/// The write-time lookup is `WHERE build_generation = ? AND token_hash IN (…)`, which
/// `idx_clone_subblock_postings_token` covers directly. This migration only CREATEs the empty table
/// (population + the read-path switch land in later phases of #296); until then the write-time
/// check keeps its RAM-index fallback unchanged.
pub(crate) const CLONE_SUBBLOCK_POSTINGS_DDL: &str = "
    CREATE TABLE IF NOT EXISTS clone_subblock_postings(
        build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation) ON DELETE \
                                                      CASCADE,
        token_hash       INTEGER NOT NULL,
        -- Content anchor (reindex-stable), NOT symbol_id (the #248 rule).
        path             TEXT    NOT NULL,
        start_byte       INTEGER NOT NULL,
        file_sha         TEXT    NOT NULL,              -- files.sha256 at compute; read-time \
                                                      staleness key
        PRIMARY KEY (build_generation, token_hash, path, start_byte)
    ) STRICT;
    CREATE INDEX IF NOT EXISTS idx_clone_subblock_postings_token
        ON clone_subblock_postings(build_generation, token_hash);
";

/// V037 (#296): create the persisted sub-block postings table (see
/// [`CLONE_SUBBLOCK_POSTINGS_DDL`]) and add `clone_graph_generations.postings_written` — the
/// upgrade-repopulation gate (review R2). A clone-graph generation built before this feature has
/// `postings_written = 0`, which the (phase-2) precompute reads as "not postings-complete" and uses
/// to force one rebuild pass that fills the postings, instead of leaving an upgraded DB with an
/// empty table forever. Idempotent: `CREATE TABLE IF NOT EXISTS` + `add_column_if_missing`.
pub(crate) fn apply_clone_subblock_postings_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CLONE_SUBBLOCK_POSTINGS_DDL)?;
    add_column_if_missing(
        conn,
        "clone_graph_generations",
        "postings_written",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

/// V038 (memory-sync phase A1): the per-machine `repos` registry + per-repo `repo_meta` k/v store —
/// the substrate the global-DB consolidation scopes every other table against. All greenfield,
/// STRICT per repo convention.
///
/// The seed placeholder row (`repo_id = '__unassigned__'`, which MUST equal
/// [`super::LEGACY_REPO_ID`]) marks a legacy single-repo DB awaiting adoption: the first
/// post-migration open calls [`super::register_repo`], which rewrites the placeholder to the real
/// content-derived `repo_id` in one step. A consolidated DB holding more than one repo never
/// carries the placeholder — `register_repo` refuses to adopt when a different real id already
/// owns the DB.
///
/// `repo_roots`/`repo_meta` carry an `ON DELETE CASCADE` FK to `repos` (NOT to a reindex-volatile
/// parent), so the volatile-FK trip-wire does not flag them and they need no allowlist entry.
///
/// Idempotent AND adoption-safe: `CREATE TABLE IF NOT EXISTS` + a placeholder seed guarded by "no
/// real repo row exists yet". `schema::apply` re-runs every additive migration, and
/// `IndexDatabase::rebuild` takes that path (via `create_or_migrate`) on an ALREADY-adopted DB — so
/// the seed must not re-mint the placeholder after `register_repo` UPDATE'd its PK to the real id.
/// A fresh DB (empty `repos`), a forward-migrated V037 index (empty `repos`), and a re-apply of an
/// adopted DB (a real row present) all converge correctly: the first two seed the placeholder, the
/// last leaves the real row untouched.
pub(crate) fn apply_repos_registry(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(REPOS_REGISTRY_DDL)
}

/// V038 DDL. The placeholder literal `'__unassigned__'` MUST equal [`super::LEGACY_REPO_ID`] —
/// `super::register_repo` reads that constant when it adopts the row (a matching bootstrap test
/// pins the two together).
pub(crate) const REPOS_REGISTRY_DDL: &str = "
    CREATE TABLE IF NOT EXISTS repos(
        repo_id          TEXT PRIMARY KEY,
        display_name     TEXT NOT NULL,
        registered_at_ms INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE IF NOT EXISTS repo_roots(
        repo_id          TEXT NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
        root             TEXT NOT NULL,
        registered_at_ms INTEGER NOT NULL,
        PRIMARY KEY(repo_id, root)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS repo_meta(
        repo_id TEXT NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
        key     TEXT NOT NULL,
        value   TEXT,
        PRIMARY KEY(repo_id, key)
    ) STRICT;
    -- Adoption placeholder (MUST equal schema::LEGACY_REPO_ID). A legacy single-repo DB carries
    -- exactly this one row until register_repo() rewrites it to the real content-derived repo_id.
    -- Seed ONLY when no real (non-placeholder) repo already owns the DB: schema::apply re-runs \
                                             every
    -- additive migration, and IndexDatabase::rebuild reaches it (via create_or_migrate) on an
    -- ALREADY-adopted DB — where the placeholder's PK has been UPDATE'd to the real id, so a plain
    -- INSERT OR IGNORE would find no conflict and resurrect the marker beside the real row. The
    -- WHERE NOT EXISTS guard makes the seed a no-op once a real repo exists; INSERT OR IGNORE \
                                             keeps
    -- it idempotent when only the placeholder is present (fresh / forward-migrated re-apply).
    INSERT OR IGNORE INTO repos(repo_id, display_name, registered_at_ms)
        SELECT '__unassigned__', '', 0
        WHERE NOT EXISTS (SELECT 1 FROM repos WHERE repo_id != '__unassigned__');
";

/// The `index_meta` keys V039 relocates into `repo_meta` (memory-sync phase A2) — the per-repo
/// singletons that were global-by-accident under the one-DB-per-repo assumption. Kept as ONE list
/// so the copy and the matching delete cannot drift. Machine/db-level keys
/// (`generated_flags_version`, the reencode DONE gate, the active-model provenance flag + remote
/// config, the throughput-tune cache) deliberately STAY in `index_meta` — they are not per-repo, or
/// are scoped by a later workstream.
const V039_INDEX_META_KEYS: &[&str] = &[
    "source_root",
    "content_revision",
    "git_commit",
    "git_dirty",
    "graph_index_version",
    "active_embedding_model",
    "clone_graph_live_generation",
    "github_last_sync_ms",
    "git_history_indexed_head",
    "git_history_indexed_root",
    "git_history_indexed_shallow",
    "local_crate_roots",
    "indexed_at_ms",
    "fts_dirty",
    "fts_source_revision",
    "fts_synced_at_ms",
];

/// The `reconcile_meta` keys V039 relocates into `repo_meta`. The `reconcile_meta` TABLE is
/// intentionally NOT dropped here — a later cleanup migration owns that (the remaining reconcile
/// timing keys still live in it); only these two per-repo keys move.
const V039_RECONCILE_META_KEYS: &[&str] =
    &["embedding_active_model_version", "vector_int8_reencode_cursor"];

/// V039 (memory-sync phase A2): relocate the per-repo singleton meta keys out of the global
/// `index_meta` / `reconcile_meta` into `repo_meta`, under the SOLE `repos` row that owns this DB
/// (see [`sole_repo_id`]) — the real repo_id when the DB was already adopted, else the
/// [`super::LEGACY_REPO_ID`] placeholder V038 seeds. Targeting the sole row (not a hardcoded
/// placeholder) is what keeps the `repo_meta → repos` FK satisfied on an ADOPTED DB, where the
/// placeholder row is gone. The read/write call sites move to the `repo_meta` accessors in the same
/// change, so a moved key is never read from the table it was deleted from.
///
/// Idempotent (`INSERT OR IGNORE` deduped by the `(repo_id, key)` PK + `DELETE` of the source
/// rows): a fresh DB has empty meta tables → the copy/delete are no-ops, and a forward-migrated
/// legacy DB converges on the identical shape. Copy-before-delete on each table means even a torn
/// run (crash between the copy and the delete) re-converges: the re-run's copy is ignored and the
/// delete finishes, and readers already read `repo_meta` (the authoritative side).
pub(crate) fn apply_move_per_repo_meta(conn: &Connection) -> rusqlite::Result<()> {
    relocate_meta_keys(conn, "index_meta", V039_INDEX_META_KEYS)?;
    relocate_meta_keys(conn, "reconcile_meta", V039_RECONCILE_META_KEYS)?;
    Ok(())
}

/// Copy the listed keys from `source_table` (a `(key, value)` k/v table) into `repo_meta` under the
/// repo that owns this DB ([`sole_repo_id`]), then delete them from the source. Resolving the
/// target HERE — inside the shared helper — means every future key-move caller inherits the correct
/// target (real-after-adoption / placeholder-before), instead of each re-deriving a placeholder
/// that a prior adoption may have deleted. `source_table` and `keys` are internal string literals
/// (never user input), so interpolating them into the SQL is safe.
fn relocate_meta_keys(
    conn: &Connection,
    source_table: &str,
    keys: &[&str],
) -> rusqlite::Result<()> {
    // V040 RECLASSIFICATION: some keys V039 lists here are GLOBAL infrastructure, not per-repo, and
    // V040 moves them back to `index_meta` (see [`RECLASSIFIED_GLOBAL_INDEX_META_KEYS`] +
    // [`move_repo_meta_keys_to_global`]). V039 is frozen (merged), so it still names them — filter
    // them out HERE so this shared helper never re-relocates a now-global key OUT of `index_meta`.
    // Two failures this prevents on a ladder replay (`schema::apply` / `index --full`, which
    // re-runs V039): (1) it would undo the reclassification every full rebuild; (2) with the
    // key present in `index_meta` the movable-keys gate below would fire, resolve
    // `sole_repo_id`, and HARD-ERROR on a consolidated >1-repo DB — regressing the round-4
    // replay property. Every genuinely-per-repo key is unaffected, so V039's relocation is
    // byte-identical for them.
    let keys: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|key| !RECLASSIFIED_GLOBAL_INDEX_META_KEYS.contains(key))
        .collect();
    if keys.is_empty() {
        return Ok(());
    }
    let in_list = keys.iter().map(|key| format!("'{key}'")).collect::<Vec<_>>().join(", ");
    // IDEMPOTENCE GATE, resolved BEFORE `sole_repo_id`: on a re-apply the source keys are already
    // gone (schema::apply re-runs the WHOLE ladder — a `create_or_migrate` / `rag-rat index --full`
    // full rebuild), so there is nothing to move. Return without resolving the sole repo. This is
    // load-bearing on a CONSOLIDATED (>1 real repo) DB, where `sole_repo_id`'s exactly-one-row
    // expectation would otherwise HARD-ERROR the ladder replay even though the move is a proven
    // no-op — breaking `index --full` for every repo in the DB. Only a genuinely-unmigrated
    // (single-repo / legacy) DB, whose source rows still exist, reaches the resolution. A torn run
    // (crash between the copy and the delete) still re-converges: the source rows survived the
    // missing delete, so the gate finds them present and finishes the move.
    let has_movable_keys: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {source_table} WHERE key IN ({in_list}))"),
        [],
        |row| row.get(0),
    )?;
    if !has_movable_keys {
        return Ok(());
    }
    let target_repo_id = sole_repo_id(conn)?;
    conn.execute(
        &format!(
            "INSERT OR IGNORE INTO repo_meta(repo_id, key, value)
             SELECT ?1, key, value FROM {source_table} WHERE key IN ({in_list})"
        ),
        [target_repo_id.as_str()],
    )?;
    conn.execute(&format!("DELETE FROM {source_table} WHERE key IN ({in_list})"), [])?;
    Ok(())
}

/// The single `repos` row that owns this DB at migration time — the real repo_id if it was already
/// adopted via [`super::register_repo`], else the [`super::LEGACY_REPO_ID`] placeholder V038 seeds.
///
/// The relocation MUST target this id, not a hardcoded placeholder: on an adopted DB the
/// placeholder row is deleted, so an `INSERT` into `repo_meta` under it trips the `repo_meta →
/// repos` FK — with `foreign_keys = ON` (production) that aborts the whole `migrate_forward`; with
/// it off it orphans rows the per-repo accessors ([`super::sole_repo_id`]) can never resolve.
/// V038 always leaves exactly one `repos` row and `register_repo` keeps it at one, so 0 or >1 is a
/// broken invariant — surfaced as an attributable migration error rather than a silent FK abort or
/// a wrong-scope pick. (Distinct from the runtime [`super::sole_repo_id`], which `ORDER BY`s the
/// placeholder last and `LIMIT 1`s without a hard error; a migration wants the hard error on `!= 1`
/// so a broken invariant is loud.)
fn sole_repo_id(conn: &Connection) -> rusqlite::Result<String> {
    let mut stmt = conn.prepare("SELECT repo_id FROM repos")?;
    let mut ids =
        stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if ids.len() == 1 {
        return Ok(ids.pop().expect("length checked to be exactly 1"));
    }
    Err(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some(format!(
            "per-repo meta relocation expects exactly one repos row (the sole repo owning this \
             DB), found {}",
            ids.len()
        )),
    ))
}

/// The `repo_id` column added to every direct-scoped core table in V040. `NOT NULL DEFAULT` the
/// [`super::LEGACY_REPO_ID`] placeholder: existing rows backfill to the placeholder, which
/// `register_repo` rewrites to the real id at adoption (the A1/A2 pattern), and any writer that
/// forgets to stamp still produces a scannable single-repo value rather than NULL.
const REPO_ID_COLUMN_DEF: &str = "TEXT NOT NULL DEFAULT '__unassigned__'";

/// The `index_meta` keys V040 relocates into `repo_meta` — the active-embedding-model provenance
/// pair that stayed behind in A2's V039 sweep (they postdated the plan's inventory). Reuniting them
/// with `active_embedding_model` / `embedding_active_model_version` (already in `repo_meta`) so the
/// whole model-provenance family is per-repo. Idempotent: on a re-apply the source keys are already
/// gone, so the copy inserts nothing.
const V040_INDEX_META_KEYS: &[&str] =
    &["active_embedding_model_provisional", "active_embedding_remote_config"];

/// The keys V039 relocated to `repo_meta` that V040 RECLASSIFIES as GLOBAL infrastructure and moves
/// BACK to `index_meta` (see [`move_repo_meta_keys_to_global`]). V039 (frozen) misfiled these under
/// the one-DB-per-repo assumption, but they are not per-repo state:
///  * `content_revision` — digest over the WHOLE `main.files` (no `repo_id` filter);
///  * `fts_dirty` / `fts_source_revision` / `fts_synced_at_ms` — freshness of the ONE global
///    `chunk_fts` FTS5 index (never repo-scoped);
///  * `vector_int8_reencode_cursor` — resume marker for a walk over the WHOLE `chunk_embeddings`
///    table, beside an already-global done-gate.
///
/// Per-repo copies caused stale-dirty loops (a consolidated DB paying a full FTS rebuild forever
/// after a sibling synced) and cross-repo resume confusion. The runtime accessors write these to
/// the global `index_meta` (`self.meta` / `ai::set_meta`); this migration relocates any
/// pre-existing per-repo copy.
const V040_REPO_META_KEYS_TO_GLOBAL: &[&str] = &[
    "content_revision",
    "fts_dirty",
    "fts_source_revision",
    "fts_synced_at_ms",
    "vector_int8_reencode_cursor",
];

/// The subset of [`V040_REPO_META_KEYS_TO_GLOBAL`] that also appears in [`V039_INDEX_META_KEYS`],
/// so [`relocate_meta_keys`] must EXCLUDE them from V039's `index_meta → repo_meta` sweep (the
/// runtime now stores them globally in `index_meta`). The reencode cursor is reclassified too but
/// lives in `V039_RECONCILE_META_KEYS` (a different source table) AND, once global, sits in
/// `index_meta` where neither V039 sweep names it — so it needs no exclusion here; V040's move-back
/// handles a stale per-repo copy.
const RECLASSIFIED_GLOBAL_INDEX_META_KEYS: &[&str] =
    &["content_revision", "fts_dirty", "fts_source_revision", "fts_synced_at_ms"];

/// Move the listed keys OUT of the per-repo `repo_meta` and back into the GLOBAL `index_meta` — the
/// inverse of [`relocate_meta_keys`], correcting V039's over-relocation of global-infrastructure
/// state (see [`V040_REPO_META_KEYS_TO_GLOBAL`]).
///
/// IDEMPOTENT + MULTI-REPO-REPLAY-SAFE (the round-4 pattern): resolves NO `sole_repo_id`, so it
/// never hard-errors on a consolidated DB. `INSERT OR IGNORE` into the `key`-PK'd `index_meta`
/// keeps ONE global value — every repo computes the same content digest so their copies agree;
/// where a per-repo copy could differ (`fts_dirty`) the first row wins and the `DELETE` still
/// clears every copy, and the next FTS freshness check self-corrects the surviving global value.
/// The `value IS NOT NULL` guard skips a `repo_meta` NULL (its `value` is nullable;
/// `index_meta.value` is `NOT NULL`). On a re-apply the `repo_meta` rows are already gone, so both
/// statements are no-ops — the whole helper is safe to run on every `schema::apply`.
fn move_repo_meta_keys_to_global(conn: &Connection, keys: &[&str]) -> rusqlite::Result<()> {
    let in_list = keys.iter().map(|key| format!("'{key}'")).collect::<Vec<_>>().join(", ");
    conn.execute(
        &format!(
            "INSERT OR IGNORE INTO index_meta(key, value)
             SELECT key, value FROM repo_meta WHERE key IN ({in_list}) AND value IS NOT NULL"
        ),
        [],
    )?;
    conn.execute(&format!("DELETE FROM repo_meta WHERE key IN ({in_list})"), [])?;
    Ok(())
}

/// V040 (memory-sync phase A3): add `repo_id` scoping to the core tables that have no FK path to
/// `files` (they scope DIRECTLY; the `files`-reachable tables scope transitively through the
/// `files.repo_id` the scope view filters on). Per the disposition table:
///  * `files` — direct `repo_id`; UNIQUE becomes `(repo_id, path, commit_sha, worktree_id)`.
///  * `packages` — direct; UNIQUE becomes `(repo_id, manifest_dir, commit_sha, worktree_id)`.
///  * `logical_symbols`, `docs` — direct `repo_id` (add column; no key change).
///  * `parser_failures` — direct; PK becomes `(repo_id, path)` (was a bare autoincrement id, so
///    `remove_file_in_scope`'s path-only delete clobbered a sibling repo — inventory #12).
///  * `git_commits` — direct; PK becomes `(repo_id, hash)`; `commit_fts` external content follows
///    and `git_file_changes` gains `repo_id` + a composite `(repo_id, commit_hash)` FK.
///
/// Also reunites the two straggler active-model meta keys with their family in `repo_meta`.
///
/// The key/PK changes force table REBUILDS (SQLite can't alter a UNIQUE/PK in place). Modeled on
/// the V031 STRICT-rebuild recipe: `foreign_keys = OFF` OUTSIDE `BEGIN IMMEDIATE`,
/// RENAME→CREATE→copy→ DROP preserving rowids/ids, ROLLBACK on error, then `COMMIT; foreign_keys =
/// ON`. Every rebuild is wrapped in ONE transaction so a failure leaves the schema untouched;
/// `files.repo_id` existing is the all-or-nothing sentinel (atomic ⇒ present iff the whole V040
/// committed), so a re-apply (fresh DB already transformed, or `create_or_migrate`/`rebuild` on an
/// applied DB) short-circuits.
///
/// #51 GUARD: `commit_fts` is external-content over `git_commits`; after the git_commits rebuild its
/// stored rowids are stale, so it is resynced with the desync-safe `'rebuild'` command, NEVER a
/// `DELETE FROM commit_fts` (which corrupts a desynced external-content index).
pub(crate) fn apply_repo_id_core_scoping(conn: &Connection) -> rusqlite::Result<()> {
    // Reclassify the global-infrastructure keys V039 over-relocated to `repo_meta` back to the
    // GLOBAL `index_meta`. Runs BEFORE the `files.repo_id` short-circuit so it also corrects a DB a
    // PRIOR commit of THIS (unreleased) branch already migrated to V040 with those keys in
    // `repo_meta` — that DB's `files.repo_id` exists, so the short-circuit would otherwise skip it.
    // Idempotent + multi-repo-safe (resolves no `sole_repo_id`), so running it on every apply is a
    // no-op once the keys are global.
    move_repo_meta_keys_to_global(conn, V040_REPO_META_KEYS_TO_GLOBAL)?;

    // All-or-nothing sentinel: the rebuilds below run under one atomic transaction, so `files`
    // carrying `repo_id` means the whole migration already committed — a fresh-from-target DB or a
    // `create_or_migrate`/`rebuild` re-apply. Short-circuit before taking the write lock.
    if column_exists(conn, "files", "repo_id")? {
        return Ok(());
    }

    // The table rebuilds need FK enforcement OFF (they RENAME/DROP FK-referenced parents); a PRAGMA
    // toggle is a no-op inside a transaction, so set it BEFORE BEGIN (V031 recipe).
    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        rebuild_files_table_with_repo_id(conn)?;
        rebuild_packages_table_with_repo_id(conn)?;
        // `logical_symbols` / `docs` are FK-less on their scoping key (no UNIQUE/PK change), so a
        // plain additive column suffices — no rebuild.
        add_column_if_missing(conn, "logical_symbols", "repo_id", REPO_ID_COLUMN_DEF)?;
        add_column_if_missing(conn, "docs", "repo_id", REPO_ID_COLUMN_DEF)?;
        rebuild_parser_failures_table_with_repo_id(conn)?;
        rebuild_git_commits_tables_with_repo_id(conn)?;
        // Re-point the placeholder-backfilled rows onto the sole repo that owns this DB. On an
        // already-adopted DB this is what keeps the rebuilt rows visible; on a not-yet-adopted DB
        // it is a no-op (see the helper). Runs inside the txn, atomic with the rebuilds.
        backfill_repo_id_to_sole_repo(conn)?;
        // Now that every `logical_symbols` row carries its final `repo_id`, migrate its
        // content-derived id to the new `repo_id`-folded derivation and re-point every reference
        // (memories, monikers, members), so pre-V040 memory/oracle handles survive the first
        // `rebuild_logical_symbols` after upgrade instead of dangling. FK enforcement is OFF for
        // the whole V040 transaction (set before BEGIN), which is exactly what the
        // parent-id remap needs. On a not-yet-adopted DB the rows carry the placeholder
        // here; `register_repo` runs the same realign again after adopting the real id
        // (idempotent — already-aligned rows are skipped).
        crate::index::graph_index::realign_logical_symbol_ids(conn)?;
        // Reunite the two active-model provenance stragglers with their family in `repo_meta`
        // (relocated under the sole repo via `relocate_meta_keys`' own `sole_repo_id` resolution;
        // `register_repo` keeps them there). Runs inside the txn so it is atomic with the rebuilds.
        relocate_meta_keys(conn, "index_meta", V040_INDEX_META_KEYS)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
        return result;
    }
    conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// Re-point every V040 direct-scoped table's placeholder-backfilled rows onto the sole repo that
/// owns this DB — the same established resolution [`relocate_meta_keys`] uses for the meta move
/// (and the V039 fix e2a2bc5 established).
///
/// The rebuilds / `add_column`s above stamp every existing row with the STATIC `__unassigned__`
/// column DEFAULT (a DEFAULT cannot be a runtime-resolved value). That is correct on a NOT-yet-
/// adopted DB — the sole repo IS the placeholder, so the rows already carry the right id and
/// [`super::register_repo`] re-points them at adoption. But on an ALREADY-ADOPTED DB (V038/V039 ran
/// `register_repo`, so `repos` holds the real id and NO placeholder), `register_repo` takes the
/// "already registered" fast path that never re-points — so the rebuilt rows would orphan under
/// `__unassigned__` and the active real-repo scope view would see an EMPTY index after upgrade.
/// Resolve the sole `repos` row (real when adopted, else the placeholder) via [`sole_repo_id`] and
/// UPDATE the placeholder rows onto it; skip the no-op churn when the DB is not yet adopted.
///
/// FK enforcement is OFF for the whole V040 transaction, so `git_commits`' `ON UPDATE CASCADE` does
/// NOT fire here — `git_file_changes` is re-pointed EXPLICITLY (unlike the runtime `register_repo`
/// path, which runs with FK ON and relies on that cascade, so it lists only `git_commits`).
fn backfill_repo_id_to_sole_repo(conn: &Connection) -> rusqlite::Result<()> {
    let target = sole_repo_id(conn)?;
    // Not adopted yet: rows already carry the placeholder; `register_repo` re-points at adoption.
    if target == super::LEGACY_REPO_ID {
        return Ok(());
    }
    for table in [
        "files",
        "packages",
        "logical_symbols",
        "docs",
        "parser_failures",
        "git_commits",
        "git_file_changes",
    ] {
        conn.execute(&format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"), [
            target.as_str(),
            super::LEGACY_REPO_ID,
        ])?;
    }
    Ok(())
}

/// Rebuild `files` with a leading `repo_id` column and the widened UNIQUE key. `files` is the
/// scoping ROOT and is FK-referenced by `chunks` / `symbols` / `edges_data` (all `REFERENCES
/// files(id)`), so the `id` values MUST be preserved — the rebuild copies them verbatim (the V008
/// `rebuild_files_table_for_commit_scopes` precedent). The full current column set (through V024's
/// `has_test_code`) is reproduced; `files` stays non-STRICT to match its baseline shape.
fn rebuild_files_table_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        -- Re-convergence: migrations run in AUTOCOMMIT, so a crash that killed a prior V040 pass
        -- mid-rebuild could leave the scratch table behind (the enclosing BEGIN/ROLLBACK normally
        -- prevents this, but a hard process kill bypasses a clean rollback). Drop it so the \
         rebuild
        -- restarts from a clean slate rather than failing on CREATE.
        DROP TABLE IF EXISTS files_new;
        CREATE TABLE files_new(
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
            has_test_code INTEGER NOT NULL DEFAULT 0,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            UNIQUE(repo_id, path, commit_sha, worktree_id)
        );
        INSERT OR IGNORE INTO files_new(
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id, has_test_code, repo_id
        )
        SELECT
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id, has_test_code, '__unassigned__'
        FROM files;
        DROP TABLE files;
        ALTER TABLE files_new RENAME TO files;
        CREATE INDEX IF NOT EXISTS idx_files_language ON files(language);
        CREATE INDEX IF NOT EXISTS idx_files_commit_path ON files(commit_sha, path);
        CREATE INDEX IF NOT EXISTS idx_files_worktree_path ON files(worktree_id, path);
        ",
    )
}

/// V043 (phase A6): add `files.generation` and widen the UNIQUE to `(repo_id, path, commit_sha,
/// worktree_id, generation)`, so a full rebuild can STAGE a fresh generation of every file row
/// ALONGSIDE the live one (same `(repo_id, path, commit_sha, worktree_id)`, different `generation`)
/// and flip readers over atomically, instead of clearing-then-reinserting inside one long
/// write-locked transaction (spec §3.3 — bounded writer holds + reader consistency).
///
/// SENTINEL: `files.generation` present ⇒ the whole migration already committed (the rebuild below
/// is atomic), so a `create_or_migrate`/`rebuild`/`index --full` re-apply short-circuits before the
/// write lock — the V040/V042 recipe.
///
/// NO placeholder→sole-repo backfill (unlike V040–V042): `generation` is REPO-NEUTRAL. Every
/// pre-V043 row is the current live generation of its repo, and `DEFAULT 0` stamps them all 0 —
/// which is exactly the live generation a fresh index carries (`repo_meta[live_files_generation]`
/// absent ⇒ 0). So existing rows stay visible under the live-generation scope view with no per-repo
/// resolution, on an adopted or an un-adopted DB alike.
pub(crate) fn apply_files_generation(conn: &Connection) -> rusqlite::Result<()> {
    // All-or-nothing sentinel: the rebuild commits atomically, so `files.generation` present means
    // the whole migration already ran. Short-circuit before taking the write lock.
    if column_exists(conn, "files", "generation")? {
        return Ok(());
    }
    // The rebuild RENAMEs an FK-referenced parent (`chunks`/`symbols`/`edges_data` REFERENCE
    // files(id)); FK enforcement must be OFF, and a PRAGMA toggle is a no-op inside a transaction,
    // so set it BEFORE BEGIN (the V040 recipe).
    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
    let result = rebuild_files_table_with_generation(conn);
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
        return result;
    }
    conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// Rebuild `files` adding a trailing `generation INTEGER NOT NULL DEFAULT 0` column and the widened
/// `UNIQUE(repo_id, path, commit_sha, worktree_id, generation)`. Copies `id` verbatim (FK target of
/// `chunks`/`symbols`/`edges_data` — the V040 `rebuild_files_table_with_repo_id` precedent) and
/// stamps every existing row `generation = 0` (the live generation of a not-yet-restaged index).
/// `files` stays non-STRICT to match its baseline shape. Leading `DROP TABLE IF EXISTS files_new` =
/// torn-state re-convergence (a hard kill of a prior V043 pass could leave the scratch table
/// behind).
fn rebuild_files_table_with_generation(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS files_new;
        CREATE TABLE files_new(
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
            has_test_code INTEGER NOT NULL DEFAULT 0,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            generation INTEGER NOT NULL DEFAULT 0,
            UNIQUE(repo_id, path, commit_sha, worktree_id, generation)
        );
        INSERT OR IGNORE INTO files_new(
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id, has_test_code, repo_id, generation
        )
        SELECT
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id, has_test_code, repo_id, 0
        FROM files;
        DROP TABLE files;
        ALTER TABLE files_new RENAME TO files;
        CREATE INDEX IF NOT EXISTS idx_files_language ON files(language);
        CREATE INDEX IF NOT EXISTS idx_files_commit_path ON files(commit_sha, path);
        CREATE INDEX IF NOT EXISTS idx_files_worktree_path ON files(worktree_id, path);
        ",
    )
}

/// Rebuild `packages` (STRICT) with a leading `repo_id` column and the widened UNIQUE key.
/// `packages` has no FK children (the file→package map is computed at load time, never persisted —
/// #106), so no id preservation is required, but the rows are copied to keep an already-populated
/// index intact through the migration.
fn rebuild_packages_table_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS packages_new;
        CREATE TABLE packages_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            manifest_dir TEXT NOT NULL,
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            local_roots_json TEXT NOT NULL DEFAULT '[]',
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            UNIQUE(repo_id, manifest_dir, commit_sha, worktree_id)
        ) STRICT;
        INSERT OR IGNORE INTO packages_new(
            id, manifest_dir, commit_sha, worktree_id, local_roots_json, repo_id
        )
        SELECT id, manifest_dir, commit_sha, worktree_id, local_roots_json, '__unassigned__'
        FROM packages;
        DROP TABLE packages;
        ALTER TABLE packages_new RENAME TO packages;
        CREATE INDEX IF NOT EXISTS idx_packages_scope ON packages(commit_sha, worktree_id);
        ",
    )
}

/// Rebuild `parser_failures` with PK `(repo_id, path)` (was a bare autoincrement `id`). The old
/// shape allowed multiple rows per path and `remove_file_in_scope` deleted by bare path —
/// clobbering a sibling repo's failure once repos share a DB (inventory #12). The new PK collapses
/// to one row per `(repo_id, path)`; `INSERT OR IGNORE` dedupes any legacy multi-row-per-path data.
/// Nothing FK-references `parser_failures`, so dropping the `id` column is safe. Rebuilt STRICT.
fn rebuild_parser_failures_table_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS parser_failures_new;
        CREATE TABLE parser_failures_new(
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            message TEXT NOT NULL,
            PRIMARY KEY(repo_id, path)
        ) STRICT;
        INSERT OR IGNORE INTO parser_failures_new(repo_id, path, language, message)
        SELECT '__unassigned__', path, language, message FROM parser_failures;
        DROP TABLE parser_failures;
        ALTER TABLE parser_failures_new RENAME TO parser_failures;
        ",
    )
}

/// Rebuild `git_commits` with PK `(repo_id, hash)` and `git_file_changes` with `repo_id` + a
/// composite `(repo_id, commit_hash)` FK to it (`ON DELETE CASCADE ON UPDATE CASCADE` — the UPDATE
/// cascade is what lets `register_repo` adoption re-point `git_commits.repo_id` and carry the
/// changes along). `git_commits` rowids are PRESERVED (copied explicitly) so the `commit_fts`
/// external-content rowid mapping stays valid; it is then resynced with the desync-safe `'rebuild'`
/// (#51 — never `DELETE FROM commit_fts`). These two tables were repo-GLOBAL before; scoping them
/// stops a `git history` reindex of one repo from wiping another's commits.
fn rebuild_git_commits_tables_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS git_commits_new;
        DROP TABLE IF EXISTS git_file_changes_new;
        CREATE TABLE git_commits_new(
            hash TEXT NOT NULL,
            author_name TEXT NOT NULL,
            author_email TEXT NOT NULL,
            authored_at_s INTEGER NOT NULL,
            committed_at_s INTEGER NOT NULL,
            subject TEXT NOT NULL,
            body TEXT NOT NULL,
            changed_file_count INTEGER NOT NULL DEFAULT 0,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, hash)
        ) STRICT;
        INSERT OR IGNORE INTO git_commits_new(
            rowid, hash, author_name, author_email, authored_at_s, committed_at_s,
            subject, body, changed_file_count, repo_id
        )
        SELECT
            rowid, hash, author_name, author_email, authored_at_s, committed_at_s,
            subject, body, changed_file_count, '__unassigned__'
        FROM git_commits;
        DROP TABLE git_commits;
        ALTER TABLE git_commits_new RENAME TO git_commits;

        CREATE TABLE git_file_changes_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            commit_hash TEXT NOT NULL,
            path TEXT NOT NULL,
            additions INTEGER,
            deletions INTEGER,
            change_kind TEXT NOT NULL DEFAULT 'modified',
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            FOREIGN KEY(repo_id, commit_hash)
                REFERENCES git_commits(repo_id, hash) ON DELETE CASCADE ON UPDATE CASCADE
        );
        INSERT OR IGNORE INTO git_file_changes_new(
            id, commit_hash, path, additions, deletions, change_kind, repo_id
        )
        SELECT id, commit_hash, path, additions, deletions, change_kind, '__unassigned__'
        FROM git_file_changes;
        DROP TABLE git_file_changes;
        ALTER TABLE git_file_changes_new RENAME TO git_file_changes;
        CREATE INDEX IF NOT EXISTS idx_git_file_changes_path ON git_file_changes(path);
        CREATE INDEX IF NOT EXISTS idx_git_file_changes_commit ON git_file_changes(commit_hash);

        -- Resync the external-content FTS after the content-table rebuild (#51: 'rebuild', never a
        -- DELETE, on a possibly-desynced external-content index).
        INSERT INTO commit_fts(commit_fts) VALUES('rebuild');
        ",
    )
}

/// The seven GitHub papertrail tables V041 (phase A4) gives a direct `repo_id` column. Order is
/// stable so the adoption re-point loop and this migration agree; `github_fts` is NOT here — it is
/// the standalone FTS mirror, rebuilt separately by [`rebuild_github_fts_with_repo_id`].
const V041_GITHUB_SCOPED_TABLES: &[&str] = &[
    "github_refs",
    "github_issues",
    "github_comments",
    "github_pull_requests",
    "github_reviews",
    "github_review_comments",
    "github_ref_sync",
];

/// V041 (memory-sync phase A4): repo-scope the GitHub papertrail cache so a lexical/papertrail
/// query in a consolidated DB never surfaces a sibling repo's refs or issues. The seven
/// [`V041_GITHUB_SCOPED_TABLES`] were repo-GLOBAL before; each gains a `repo_id` column
/// ([`REPO_ID_COLUMN_DEF`] — existing rows backfill to the placeholder that `register_repo`
/// re-points at adoption, the A1/A2/A3 pattern), and the standalone `github_fts` gains a `repo_id
/// UNINDEXED` column via a REBUILD (its FTS5 columns can't be ALTERed).
///
/// SCOPE (deliberate): the base tables gain the column but keep their existing keys
/// (`github_issues`/`github_pull_requests` `UNIQUE(owner, repo, number)`; `github_ref_sync`'s PK;
/// the id-keyed caches). Phase A keeps ONE repo per DB (`register_repo` refuses a second real repo
/// until A7), so those keys stay correct; widening them to include `repo_id` for the eventual
/// multi-repo DB is an A7 concern. Reads scope by the connection's active `repo_id`; writes stamp
/// it.
///
/// TORN-STATE / RE-APPLY: the whole migration self-wraps in ONE `BEGIN IMMEDIATE ... COMMIT` (the
/// ladder runs apply fns in AUTOCOMMIT), so the base-table columns, the `github_fts` rebuild, AND
/// the already-adopted-DB backfill commit together — the `github_fts.repo_id` sentinel flips only
/// once every step (backfill included) has landed. `add_column_if_missing` and the rebuild's
/// leading `DROP ... IF EXISTS` keep a re-run after a torn intermediate idempotent. All-or-nothing
/// sentinel: `github_fts` carrying `repo_id` means the whole migration already committed, so a
/// fresh-from-target DB or a `create_or_migrate` / `rebuild` re-apply short-circuits before
/// touching anything (and never resolves `sole_repo_id`, keeping the ladder replay-safe on a
/// consolidated DB).
pub(crate) fn apply_github_repo_id_scoping(conn: &Connection) -> rusqlite::Result<()> {
    // Post-V060 the legacy github_* tables no longer exist AT ALL: the baseline creates the
    // provider-neutral papertrail_* tables instead, so on a fresh DB (or any DB the V060
    // normalization already converged) there is nothing for this migration to scope — the ladder
    // still replays it in order, and it must no-op instead of ALTERing absent tables. Only
    // reachable-legacy DBs (github_fts present, created by a pre-V060 baseline) take the body.
    if !sqlite_object_exists(conn, "table", "github_fts")? {
        return Ok(());
    }
    if column_exists(conn, "github_fts", "repo_id")? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        // Each base-table column is independent additive DDL (one atomic ALTER).
        for table in V041_GITHUB_SCOPED_TABLES {
            add_column_if_missing(conn, table, "repo_id", REPO_ID_COLUMN_DEF)?;
        }
        rebuild_github_fts_with_repo_id(conn)?;
        // P1 (the V039/V040 class): the static `DEFAULT '__unassigned__'` stamps existing rows the
        // placeholder, and on an ALREADY-ADOPTED DB `register_repo`'s fast path never re-points —
        // so scoped papertrail reads would return NOTHING for the real repo until the next github
        // sync. Resolve the sole `repos` row and re-point the placeholder rows onto it (no-op on an
        // un-adopted DB, where the rows correctly wait for `register_repo` to adopt them).
        backfill_github_repo_id_to_sole_repo(conn)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

/// Rebuild the standalone (own-content) `github_fts` with an added `repo_id UNINDEXED` column via
/// the create-new / copy / drop / rename recipe. Its rows are copied stamped the placeholder; the
/// caller's [`backfill_github_repo_id_to_sole_repo`] then re-points them onto the real id on an
/// adopted DB.
///
/// Runs INSIDE the caller's transaction ([`apply_github_repo_id_scoping`] wraps the whole V041
/// migration in one `BEGIN IMMEDIATE`), so it opens no txn of its own. The leading `DROP TABLE IF
/// EXISTS github_fts_new` drops a hard-kill scratch artifact (which bypasses rollback) so the
/// rebuild re-converges rather than failing on CREATE — the V040 recipe. `github_fts` has no FK, so
/// no `foreign_keys` toggle is needed.
fn rebuild_github_fts_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_fts_new;
        CREATE VIRTUAL TABLE github_fts_new USING fts5(
            owner,
            repo,
            number UNINDEXED,
            item_kind UNINDEXED,
            item_id UNINDEXED,
            url UNINDEXED,
            title,
            body,
            classification,
            repo_id UNINDEXED,
            tokenize='porter'
        );
        INSERT INTO github_fts_new(
            owner, repo, number, item_kind, item_id, url, title, body, classification, repo_id
        )
        SELECT owner, repo, number, item_kind, item_id, url, title, body, classification,
               '__unassigned__'
        FROM github_fts;
        DROP TABLE github_fts;
        ALTER TABLE github_fts_new RENAME TO github_fts;
        ",
    )
}

/// P1 backfill (the V040 [`backfill_repo_id_to_sole_repo`] pattern, github edition): re-point every
/// V041 github row from the placeholder onto the sole `repos` id when the DB is ALREADY ADOPTED, so
/// a scoped papertrail read sees this repo's cached refs/issues immediately after the upgrade
/// rather than only after the next sync. No-op on an un-adopted DB (`sole_repo_id` == the
/// placeholder — the rows correctly wait for `register_repo` to adopt them). Runs inside the
/// caller's V041 txn, so it is atomic with the sentinel-setting FTS rebuild. The sentinel
/// short-circuit keeps a replay off this path entirely; the explicit `repos`-cardinality gate
/// below additionally makes the FIRST apply safe on a CONSOLIDATED DB (leave-at-placeholder
/// instead of `sole_repo_id`'s one-row hard error aborting the upgrade).
fn backfill_github_repo_id_to_sole_repo(conn: &Connection) -> rusqlite::Result<()> {
    // Cardinality gate: the backfill re-points onto THE sole owner of a single-repo DB, and only
    // that shape has one. A CONSOLIDATED DB (multiple `repos` rows) that reaches V041 without the
    // sentinel must not abort the forward-migration on `sole_repo_id`'s one-row hard error — there
    // is no single correct owner to pick. Invariant: on `!= 1` repos rows the github rows stay
    // under the placeholder, which is safe because (a) the papertrail is a refetchable CACHE, not
    // authored data; (b) every scoped reader filters `repo_id = <active>`, so placeholder rows are
    // simply invisible — never misattributed; and (c) the github writers RECLAIM stranded rows by
    // UPSERT: the tables keep natural keys WITHOUT `repo_id` (the phase-A deviation), so a
    // placeholder row OCCUPIES its key and a bare `INSERT OR IGNORE` could never repopulate it —
    // instead each writer's `ON CONFLICT ... DO UPDATE` (or `OR REPLACE` on the id-keyed caches)
    // re-stamps `repo_id` and refreshes the content on the next sync touching the key, and the
    // sync-tail `rebuild_fts` re-derives the mirror accordingly (the upsert-reclaim invariant in
    // `github/store.rs`).
    let repos_rows: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |row| row.get(0))?;
    if repos_rows != 1 {
        return Ok(());
    }
    let target = sole_repo_id(conn)?;
    if target == super::LEGACY_REPO_ID {
        return Ok(());
    }
    for table in V041_GITHUB_SCOPED_TABLES {
        conn.execute(&format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"), [
            target.as_str(),
            super::LEGACY_REPO_ID,
        ])?;
    }
    // The own-content FTS mirror carries its own `repo_id UNINDEXED` value; re-point it in place
    // too.
    conn.execute("UPDATE github_fts SET repo_id = ?1 WHERE repo_id = ?2", [
        target.as_str(),
        super::LEGACY_REPO_ID,
    ])?;
    Ok(())
}

// ============================================================================================
// Memory-sync phase A7 (GitHub natural-key widening) — V044.
//
// V041 gave the GitHub papertrail tables a `repo_id` column but DELIBERATELY left their
// `(owner, repo, number)`-style natural keys un-widened (one repo per DB until A7, so those keys
// stayed correct). A7 makes multi-repo the default: two distinct repos in one consolidated DB can
// each reference the SAME external `(owner, repo, number)` — a shared upstream issue, a fork — and
// the un-widened key would make the second repo's sync UPSERT OVER the first's row (stamping the
// wrong `repo_id`, so a scoped read then loses it). V044 folds `repo_id` into every such key.
// ============================================================================================

/// The unique index V044 creates on `github_issues` — also the migration's all-or-nothing SENTINEL
/// (it exists iff the whole atomic migration committed). Distinct from `idx_github_refs_unique`,
/// which predates V044 (baseline) and so cannot mark this migration's completion.
const GITHUB_ISSUES_REPO_UNIQUE_INDEX: &str = "idx_github_issues_repo_unique";

/// V044 (memory-sync phase A7): widen the `(owner, repo, number)`-style GitHub natural keys to fold
/// `repo_id`, so a consolidated multi-repo DB can cache the same external issue/PR/ref for two
/// repos without one repo's sync UPSERTing over the other's row. Three shapes:
///
///  * INLINE-UNIQUE REBUILD: `github_issues` / `github_pull_requests` carried an inline
///    `UNIQUE(owner, repo, number)` table constraint (droppable only by rebuild). Each is rebuilt
///    WITHOUT the inline constraint plus a NAMED unique index `(repo_id, owner, repo, number)` —
///    the named index doubles as the migration sentinel and as the writers' `ON CONFLICT` target.
///  * INLINE-PK REBUILD: `github_ref_sync` carried an inline `PRIMARY KEY(owner, repo, number)`;
///    rebuilt with `PRIMARY KEY(repo_id, owner, repo, number)`.
///  * INDEX SWAP: `github_refs` scopes uniqueness through the SEPARATE `idx_github_refs_unique`
///    index (its own PK is `id AUTOINCREMENT`), so it needs only a DROP + CREATE of that index with
///    a leading `repo_id` — no table rebuild.
///
/// The id-keyed caches (`github_comments` / `github_reviews` / `github_review_comments`) are NOT
/// touched here — V044 originally left them last-syncer-owns, which V045
/// ([`apply_github_child_key_widening`]) superseded with `(repo_id, id)` uniqueness after the
/// restamping proved to evict a sibling repo's scoped papertrail (see the V045 block comment).
///
/// SAFE ON EXISTING DATA: this migration only ever runs on a SINGLE-repo DB (multi-repo does not
/// exist until A7 registration, which post-dates the migration ladder), so every existing github
/// row shares one `repo_id` and the old `(owner, repo, number)` uniqueness already guarantees the
/// widened key is unique — a plain `INSERT ... SELECT` copies faithfully with no dup risk.
///
/// MECHANICS: the github base tables carry NO incoming FK (nothing REFERENCES them), so unlike V040
/// no `foreign_keys` toggle is needed — the DROP/RENAME rebuilds touch no FK parent. The whole
/// migration self-wraps in ONE `BEGIN IMMEDIATE` (the ladder runs apply fns in AUTOCOMMIT), so the
/// rebuilds and index swaps commit together; the named `github_issues` unique index present is the
/// all-or-nothing sentinel, so a `create_or_migrate` / `rebuild` / `index --full` re-apply
/// short-circuits before the write lock. Each rebuild's leading `DROP TABLE IF EXISTS
/// <scratch>_new` re-converges after a hard-kill that bypassed a clean rollback (the V040 recipe).
pub(crate) fn apply_github_natural_key_widening(conn: &Connection) -> rusqlite::Result<()> {
    // Post-V060 (and on every fresh DB) the legacy github_* tables do not exist — the baseline
    // creates the provider-neutral papertrail_* tables instead. The ladder still replays this
    // migration in order; with nothing to widen it must no-op rather than rebuild absent tables.
    if !sqlite_object_exists(conn, "table", "github_issues")? {
        return Ok(());
    }
    // All-or-nothing sentinel (see the doc comment): the whole migration commits atomically, so the
    // named github_issues unique index existing means every rebuild + index swap already landed.
    if sqlite_object_exists(conn, "index", GITHUB_ISSUES_REPO_UNIQUE_INDEX)? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        rebuild_github_issues_with_repo_scoped_key(conn)?;
        rebuild_github_pull_requests_with_repo_scoped_key(conn)?;
        rebuild_github_ref_sync_with_repo_scoped_key(conn)?;
        widen_github_refs_unique_index(conn)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

/// Whether a `type`-kind object named `name` exists in `sqlite_master` — the V044 sentinel probe
/// (an index, but generic so the same helper reads for a table if a later widening needs it).
fn sqlite_object_exists(conn: &Connection, kind: &str, name: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2",
            [kind, name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Rebuild `github_issues` dropping the inline `UNIQUE(owner, repo, number)` and adding a NAMED
/// unique index `(repo_id, owner, repo, number)` (the V044 sentinel + the `store_item` `ON
/// CONFLICT` target). The full V041 column set (through `repo_id`) is reproduced verbatim; `id`
/// values are copied so a mid-flight papertrail resync's `github_fts.item_id` stays stable. Runs
/// inside the caller's transaction (opens none of its own).
fn rebuild_github_issues_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_issues_new;
        CREATE TABLE github_issues_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            is_pull_request INTEGER NOT NULL DEFAULT 0,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_issues_new(
            id, owner, repo, number, html_url, state, title, body, author, created_at, updated_at,
            is_pull_request, synced_at_ms, repo_id
        )
        SELECT id, owner, repo, number, html_url, state, title, body, author, created_at,
               updated_at, is_pull_request, synced_at_ms, repo_id
        FROM github_issues;
        DROP TABLE github_issues;
        ALTER TABLE github_issues_new RENAME TO github_issues;
        CREATE UNIQUE INDEX idx_github_issues_repo_unique
            ON github_issues(repo_id, owner, repo, number);
        ",
    )
}

/// Rebuild `github_pull_requests` dropping the inline `UNIQUE(owner, repo, number)` and adding a
/// NAMED unique index `(repo_id, owner, repo, number)` (the `store_item` change-request `ON
/// CONFLICT` target). See [`rebuild_github_issues_with_repo_scoped_key`] for the shared rationale.
fn rebuild_github_pull_requests_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_pull_requests_new;
        CREATE TABLE github_pull_requests_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            merged_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_pull_requests_new(
            id, owner, repo, number, html_url, state, title, body, author, created_at, updated_at,
            merged_at, synced_at_ms, repo_id
        )
        SELECT id, owner, repo, number, html_url, state, title, body, author, created_at,
               updated_at, merged_at, synced_at_ms, repo_id
        FROM github_pull_requests;
        DROP TABLE github_pull_requests;
        ALTER TABLE github_pull_requests_new RENAME TO github_pull_requests;
        CREATE UNIQUE INDEX idx_github_pull_requests_repo_unique
            ON github_pull_requests(repo_id, owner, repo, number);
        ",
    )
}

/// Rebuild `github_ref_sync` widening its inline `PRIMARY KEY(owner, repo, number)` to
/// `PRIMARY KEY(repo_id, owner, repo, number)` (the `github/sync.rs` `ON CONFLICT` target). Copies
/// the full V041 column set verbatim.
fn rebuild_github_ref_sync_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_ref_sync_new;
        CREATE TABLE github_ref_sync_new(
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            status TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL,
            last_error TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, owner, repo, number)
        );
        INSERT INTO github_ref_sync_new(owner, repo, number, status, synced_at_ms, last_error, \
         repo_id)
        SELECT owner, repo, number, status, synced_at_ms, last_error, repo_id FROM github_ref_sync;
        DROP TABLE github_ref_sync;
        ALTER TABLE github_ref_sync_new RENAME TO github_ref_sync;
        ",
    )
}

/// Widen `github_refs`' uniqueness index to lead with `repo_id`. `github_refs`' own PK is `id
/// AUTOINCREMENT`, so its `(owner, repo, number, source_kind, …)` uniqueness lives in the SEPARATE
/// `idx_github_refs_unique` index — a DROP + CREATE swap suffices, no table rebuild. The trailing
/// columns match `store_ref`'s widened `ON CONFLICT` target exactly.
fn widen_github_refs_unique_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP INDEX IF EXISTS idx_github_refs_unique;
        CREATE UNIQUE INDEX idx_github_refs_unique
            ON github_refs(repo_id, owner, repo, number, source_kind, COALESCE(source_path, ''),
                           COALESCE(source_commit, ''), source_text);
        ",
    )
}

// ============================================================================================
// Memory-sync phase A7 (GitHub id-keyed child widening) — V045.
//
// V044 widened the `(owner, repo, number)` natural keys but left the id-keyed CHILD caches
// (`github_comments` / `github_reviews` / `github_review_comments`) keyed by GitHub's own item id
// alone, documented as "last-syncer-owns". The consequence on a shared external issue/PR: each
// repo's sync RESTAMPS the child rows to itself, the sync-tail FTS rebuild copies that repo_id
// into the mirror, and every scoped reader filters `repo_id = active` — so repo A's papertrail
// LOSES the PR's comments/reviews the moment repo B syncs, oscillating on every re-sync. V045
// folds `repo_id` into the child keys so each repo owns its own copy.
// ============================================================================================

/// The V045 sentinel: the named unique index on `github_comments` (created inside the atomic
/// migration, so its existence ⇒ the whole migration committed).
const GITHUB_COMMENTS_REPO_UNIQUE_INDEX: &str = "idx_github_comments_repo_unique";

/// V045 (memory-sync phase A7): widen the id-keyed GitHub child caches to `(repo_id, id)`
/// uniqueness, so two repos sharing an external issue/PR each keep their own copy of its
/// comments/reviews instead of last-syncer-owns oscillation (see the block comment above). Each
/// table is rebuilt WITHOUT the inline `id INTEGER PRIMARY KEY` (the implicit rowid remains) plus
/// a NAMED unique index `(repo_id, id)` — the V044 recipe; the `github_comments` index doubles as
/// the all-or-nothing sentinel.
///
/// BACKFILL RULE (per row, lossless): a child row is copied once PER OWNING-PARENT repo — the
/// DISTINCT `repo_id`s of the parent rows matching its `(owner, repo, number)` (comments parent =
/// issues ∪ pull requests, both stored via the issues API; reviews / review comments parent =
/// pull requests) — because post-V044 a shared parent exists under MULTIPLE repos and each owner's
/// scoped papertrail must see the children. Additionally every row survives under its OWN stamped
/// `repo_id` when not already covered (`NOT EXISTS` second pass): orphans with no cached parent,
/// and the defensive case of a stamped repo whose parent row is missing. No row is ever dropped.
///
/// MECHANICS: no incoming FKs (no `foreign_keys` toggle needed); ONE self-wrapped
/// `BEGIN IMMEDIATE`; leading `DROP TABLE IF EXISTS <scratch>_new` re-converges a hard-killed
/// prior pass; the unique indexes are created AFTER the copies (the copy dedupes via UNION /
/// NOT EXISTS, never via constraint suppression). The writers keep `INSERT OR REPLACE`, which now
/// resolves through the widened unique index — a same-repo re-sync replaces in place, a sibling
/// repo's sync inserts its own row. The `github_fts` mirror is re-derived INSIDE the migration
/// ([`rebuild_github_fts_from_widened_bases`]) so the duplicated rows are scoped-searchable
/// immediately, not only after the next sync.
pub(crate) fn apply_github_child_key_widening(conn: &Connection) -> rusqlite::Result<()> {
    // Post-V060 / fresh-DB no-op, exactly like V041/V044: the legacy child caches don't exist.
    if !sqlite_object_exists(conn, "table", "github_comments")? {
        return Ok(());
    }
    if sqlite_object_exists(conn, "index", GITHUB_COMMENTS_REPO_UNIQUE_INDEX)? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        rebuild_github_comments_with_repo_scoped_key(conn)?;
        rebuild_github_reviews_with_repo_scoped_key(conn)?;
        rebuild_github_review_comments_with_repo_scoped_key(conn)?;
        rebuild_github_fts_from_widened_bases(conn)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

// Dream v2 pass 0 (memory verification sibling tables) — V046.
//
// The dream verify/compact passes need per-memory DERIVED state, and the load-bearing invariant is
// that dream NEVER mutates a `repo_memories` row (a human/strong agent confirms; dream only
// proposes). So the derived state lives in SIBLING tables, not new columns on `repo_memories`:
//
//  * `memory_reality` — one row per memory (keyed `(repo_id, memory_id)`), the verification
//    bookmark + verdict. `content_hash` and `checked_inputs_hash` are the churn-skip comparators
//    the verification queue reads: a memory is re-queued when its current content_hash differs from
//    the stored one (a TITLE or body edit — both prompts audit the whole note, so the hash covers
//    both) OR its recomputed evidence hash differs (source/identifier churn under the note).
//    `checked_inputs_hash` is a DELIBERATE addition to the plan's schema — a cheap sha comparison
//    over the memory's evidence pack beats a commit-ancestry walk. The verdict columns
//    (`verdict`/`direction`/`model_id`/`prompt_version`/`evidence_json`) are NULL in pass 0
//    (deterministic) and filled by the phase-B model verdict pass.
//  * `memory_summaries` — one row per `(repo_id, memory_id, content_hash)`, so a title or body edit
//    changes the key and self-invalidates the stale summary (a LEFT JOIN on the current
//    content_hash misses, the compaction pass in phase C regenerates).
//
// Both are STRICT and carry `repo_id` (repo-unique memory ids post-A5, mirroring how
// `repo_memory_bindings` scopes by `repo_id`); they hold regenerable data, so no FK to
// `repo_memories` — a deleted memory's orphan rows never surface (scoped reads join active
// memories) and gc sweeps them, matching the no-FK `dream_findings` periphery posture.
// ============================================================================================

/// The `memory_reality` table — also the V046 all-or-nothing SENTINEL (it exists iff the whole
/// atomic migration committed).
const MEMORY_REALITY_TABLE: &str = "memory_reality";

/// V046 (dream v2 pass 0): create the `memory_reality` / `memory_summaries` sibling tables that
/// hold the dream verification state, so the verify/compact passes persist derived per-memory data
/// WITHOUT ever touching a `repo_memories` row. Both are fresh CREATEs (no rebuild, no FK parent),
/// so unlike V040/V042 there is no `foreign_keys` toggle to do.
///
/// MECHANICS (the V044 no-FK-toggle recipe): the ladder runs apply fns in AUTOCOMMIT, so the two
/// CREATEs self-wrap in ONE `BEGIN IMMEDIATE` and commit together — `memory_reality` existing is
/// then the all-or-nothing sentinel (present iff both tables landed), so a `create_or_migrate` /
/// `rebuild` / `index --full` re-apply short-circuits before taking the write lock. The leading
/// `DROP TABLE IF EXISTS` re-converges after a hard-kill that bypassed a clean rollback (the tables
/// hold only regenerable data, so a drop-and-recreate is always safe here).
pub(crate) fn apply_memory_verification_tables(conn: &Connection) -> rusqlite::Result<()> {
    // All-or-nothing sentinel (see the doc comment): both CREATEs commit atomically, so
    // `memory_reality` present means the whole migration already ran. Probes `sqlite_master`
    // directly (a `rusqlite::Result`, like V044's `sqlite_object_exists`) so the ladder's
    // `rusqlite::Result` apply signature carries no `anyhow` conversion.
    let sentinel_present = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [MEMORY_REALITY_TABLE],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if sentinel_present {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = conn.execute_batch(
        "
        DROP TABLE IF EXISTS memory_reality;
        DROP TABLE IF EXISTS memory_summaries;
        CREATE TABLE memory_reality(
            memory_id              TEXT    NOT NULL,
            repo_id                TEXT    NOT NULL DEFAULT '__unassigned__',
            -- sha256 over the memory's NOTE content (trimmed title + body) — the churn-skip
            -- comparator that self-invalidates a stored verdict on any title OR body edit.
            content_hash           TEXT    NOT NULL,
            verdict                TEXT,
            direction              TEXT,
            -- Informational, for human review: the commit the note was checked against.
            checked_against_commit TEXT,
            -- sha256 over the sorted sha256s of the memory's bound files at check time — the
            -- churn-skip comparator (cheaper than a commit-ancestry walk).
            checked_inputs_hash    TEXT,
            evidence_json          TEXT,
            model_id               TEXT,
            prompt_version         TEXT,
            checked_at_ms          INTEGER NOT NULL,
            PRIMARY KEY (repo_id, memory_id)
        ) STRICT;
        CREATE TABLE memory_summaries(
            memory_id       TEXT    NOT NULL,
            repo_id         TEXT    NOT NULL DEFAULT '__unassigned__',
            -- Keyed WITH content_hash (trimmed title + body) so a title OR body edit changes the \
         key
            -- and self-invalidates the stale summary.
            content_hash    TEXT    NOT NULL,
            summary         TEXT    NOT NULL,
            model_id        TEXT,
            prompt_version  TEXT,
            generated_at_ms INTEGER NOT NULL,
            PRIMARY KEY (repo_id, memory_id, content_hash)
        ) STRICT;
        ",
    );
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

/// V047: record deterministic dream model failures in a derived sibling table.
///
/// The table is keyed one current row per `(repo_id, memory_id, pass)`, with the same freshness
/// stamps the queues already consult (`content_hash`, optional `checked_inputs_hash`,
/// `prompt_version`, `model_id`). A current row whose persisted enum reason is deterministic
/// suppresses another model call; content/evidence/prompt/model churn invalidates it.
pub(crate) fn apply_memory_model_failures_table(conn: &Connection) -> rusqlite::Result<()> {
    if column_exists(conn, "memory_model_failures", "reason")? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = conn.execute_batch(
        "
        DROP TABLE IF EXISTS memory_model_failures;
        CREATE TABLE memory_model_failures(
            memory_id           TEXT    NOT NULL,
            repo_id             TEXT    NOT NULL DEFAULT '__unassigned__',
            pass                TEXT    NOT NULL,
            content_hash        TEXT    NOT NULL,
            checked_inputs_hash TEXT,
            model_id            TEXT    NOT NULL,
            prompt_version      TEXT    NOT NULL,
            reason              TEXT    NOT NULL,
            detail              TEXT,
            failed_at_ms        INTEGER NOT NULL,
            attempts            INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (repo_id, memory_id, pass)
        ) STRICT;
        CREATE INDEX idx_memory_model_failures_reason
            ON memory_model_failures(repo_id, pass, reason);
        ",
    );
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

/// Re-derive the standalone `github_fts` mirror from the freshly-widened base tables, INSIDE the
/// V045 transaction — the V042 memory-FTS posture. Without this, the repos that just gained
/// duplicated child rows still cannot FIND them through the scoped FTS readers
/// (`rationale_search` / papertrail filter `repo_id = active`) until some later github sync
/// happens to run the sync-tail rebuild — the migration would fix the base tables while the
/// mirror kept serving the last-syncer state. The derivation mirrors `papertrail::rebuild_fts`'s
/// column mapping exactly (per-kind title slots; reviews' `COALESCE(html_url,'')`); the
/// `classification` column is a Rust-derived label (`classify_text`), so it is CARRIED from the
/// old mirror by `(item_kind, item_id)` — identical per item across per-repo duplicates — with
/// `'other'` for never-mirrored rows (refreshed to the real label at the next sync's rebuild).
/// GUARDED on `github_fts` presence: the schema-bootstrap isolation fixtures seed only the base
/// tables. Torn-state safe: runs inside the caller's single transaction; the temp scratch is
/// per-connection and re-created per run.
fn rebuild_github_fts_from_widened_bases(conn: &Connection) -> rusqlite::Result<()> {
    if !sqlite_object_exists(conn, "table", "github_fts")? {
        return Ok(());
    }
    conn.execute_batch(
        "
        CREATE TEMP TABLE IF NOT EXISTS v045_fts_class(
            item_kind TEXT NOT NULL,
            item_id TEXT NOT NULL,
            classification TEXT NOT NULL,
            PRIMARY KEY(item_kind, item_id)
        );
        DELETE FROM temp.v045_fts_class;
        INSERT OR IGNORE INTO temp.v045_fts_class(item_kind, item_id, classification)
            SELECT item_kind, item_id, classification FROM github_fts;
        DELETE FROM github_fts;
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT i.owner, i.repo, i.number, 'issue', CAST(i.id AS TEXT), i.html_url, i.title, \
         i.body, COALESCE(c.classification, 'other'), i.repo_id
        FROM github_issues i
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'issue' AND c.item_id = CAST(i.id AS TEXT);
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT m.owner, m.repo, m.number, 'comment', CAST(m.id AS TEXT), m.html_url, '', m.body,
               COALESCE(c.classification, 'other'), m.repo_id
        FROM github_comments m
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'comment' AND c.item_id = CAST(m.id AS TEXT);
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT p.owner, p.repo, p.number, 'pull', CAST(p.id AS TEXT), p.html_url, p.title, p.body, \
         COALESCE(c.classification, 'other'), p.repo_id
        FROM github_pull_requests p
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'pull' AND c.item_id = CAST(p.id AS TEXT);
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT r.owner, r.repo, r.number, 'review', CAST(r.id AS TEXT), COALESCE(r.html_url, ''), \
         '', r.body, COALESCE(c.classification, 'other'), r.repo_id
        FROM github_reviews r
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'review' AND c.item_id = CAST(r.id AS TEXT);
        INSERT INTO github_fts(owner, repo, number, item_kind, item_id, url, title, body, \
         classification, repo_id)
        SELECT rc.owner, rc.repo, rc.number, 'review_comment', CAST(rc.id AS TEXT), rc.html_url, \
         COALESCE(rc.path, ''), rc.body, COALESCE(c.classification, 'other'), rc.repo_id
        FROM github_review_comments rc
        LEFT JOIN temp.v045_fts_class c
          ON c.item_kind = 'review_comment' AND c.item_id = CAST(rc.id AS TEXT);
        DROP TABLE temp.v045_fts_class;
        ",
    )
}

// ============================================================================================
// Provider-neutral papertrail schema (#588) — V060.
//
// The seven GitHub-shaped cache tables (github_refs / github_issues / github_comments /
// github_pull_requests / github_reviews / github_review_comments / github_ref_sync) and the
// github_fts mirror normalize into the provider-neutral papertrail_* tables: items carry a
// `tracker` token (closed set, `papertrail::Tracker`) and an `item_kind` that is PART OF THE
// IDENTITY (`issue` | `change_request`), comments unify the three GitHub comment shapes behind
// nullable `review_state` / `anchor_path` markers, refs become a pure annotation layer, and the
// per-ref sync state machine is DELETED in favor of the per-(repo, tracker, project)
// `papertrail_sync_cursor` the mirror sync will drive. HARD RENAME — no legacy aliases, no
// compatibility views; the github_* tables are dropped after the backfill.
// ============================================================================================

/// Create the provider-neutral papertrail tables + indexes (idempotent `IF NOT EXISTS` DDL).
/// SHARED between `apply_baseline` (a fresh DB gets the current schema directly — no legacy
/// github_* tables are ever created) and [`apply_papertrail_provider_neutral_schema`] (so the
/// migration is self-contained when driven against an isolation fixture). V060 is these tables'
/// birth migration, so sharing the DDL cannot clobber an older shape; a future shape change must
/// land as its own migration, not an edit here.
pub(crate) fn create_papertrail_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        -- Every table carries repo_id from birth and folds it into its natural key (the V044/V045
        -- discipline). item_kind is part of an item's identity: GitHub's shared issue/PR
        -- numbering is the exception, not the rule (GitLab namespaces them separately).
        CREATE TABLE IF NOT EXISTS papertrail_items(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            url TEXT NOT NULL,
            state TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            merged_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_items_natural_key
            ON papertrail_items(repo_id, tracker, project, item_kind, item_key);

        -- One unified comment shape: a review event carries review_state, a file-anchored review
        -- comment carries anchor_path, a plain thread comment carries neither. The parent item is
        -- named by (item_kind, item_key); comment_id is source-qualified by providers whose
        -- thread-comment / review / review-comment resources have overlapping id spaces.
        CREATE TABLE IF NOT EXISTS papertrail_comments(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            comment_id TEXT NOT NULL,
            url TEXT,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            review_state TEXT,
            anchor_path TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_comments_natural_key
            ON papertrail_comments(repo_id, tracker, project, comment_id);
        CREATE INDEX IF NOT EXISTS idx_papertrail_comments_item
            ON papertrail_comments(repo_id, tracker, project, item_kind, item_key);
        CREATE INDEX IF NOT EXISTS idx_papertrail_comments_anchor_path
            ON papertrail_comments(anchor_path);

        -- Discovered path/commit/branch -> item links. Annotation layer ONLY (evidence ranking);
        -- refs no longer gate sync. No item_kind: a discovered `#N` cannot know the kind.
        CREATE TABLE IF NOT EXISTS papertrail_refs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_key TEXT NOT NULL,
            item_kind TEXT,
            ref_kind TEXT NOT NULL DEFAULT 'unknown',
            source_kind TEXT NOT NULL,
            source_path TEXT,
            source_commit TEXT,
            source_text TEXT NOT NULL,
            discovered_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_papertrail_refs_path ON papertrail_refs(source_path);
        CREATE INDEX IF NOT EXISTS idx_papertrail_refs_item
            ON papertrail_refs(repo_id, tracker, project, item_kind, item_key);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_refs_unique
            ON papertrail_refs(repo_id, tracker, project, COALESCE(item_kind, ''), item_key, \
         source_kind,
                               COALESCE(source_path, ''), COALESCE(source_commit, ''), \
         source_text);

        -- The mirror-sync resume cursor: ONE row per (repo, tracker, project) — REPLACES the
        -- per-ref github_ref_sync synced/not_found/failed state machine, which is deleted (not
        -- migrated). high_mark_at is the delta lane's newest-seen provider timestamp; low_mark_at
        -- is the LIFO backfill descent position; backfill_done flips once the descent reaches the
        -- oldest item; filter_fingerprint invalidates the cursor when the binding's tag filter
        -- changes. Created empty by this schema generation; the mirror sync that drives it lands
        -- separately.
        CREATE TABLE IF NOT EXISTS papertrail_sync_cursor(
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            high_mark_at TEXT,
            low_mark_at TEXT,
            probe_etag TEXT,
            backfill_done INTEGER NOT NULL DEFAULT 0,
            filter_fingerprint TEXT,
            last_probe_ms INTEGER,
            last_full_sync_ms INTEGER,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, tracker, project)
        ) STRICT;

        -- Item -> tag junction (provider labels): client-side tag filtering, config-change
        -- pruning, and label surfacing in results. Populated by the mirror sync (the legacy cache
        -- never stored labels, so there is nothing to backfill).
        CREATE TABLE IF NOT EXISTS papertrail_item_tags(
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            tag TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, tag)
        ) STRICT;

        -- Standalone (own-content) FTS mirror of papertrail_items + papertrail_comments,
        -- maintained INCREMENTALLY by the store writers (each item/comment refreshes only its own
        -- row); the whole-table rebuild is reserved for the full re-walk / recovery paths.
        -- doc_kind is 'item' or 'comment'; comment_id is '' on item rows. repo_id is the
        -- V041-style UNINDEXED scope column every MATCH must filter on.
        CREATE VIRTUAL TABLE IF NOT EXISTS papertrail_fts USING fts5(
            tracker UNINDEXED,
            project,
            item_kind UNINDEXED,
            item_key UNINDEXED,
            doc_kind UNINDEXED,
            comment_id UNINDEXED,
            url UNINDEXED,
            title,
            body,
            classification,
            repo_id UNINDEXED,
            tokenize='porter'
        );
        ",
    )
}

/// V060 (#588): normalize the GitHub-shaped papertrail cache into the provider-neutral
/// papertrail_* tables and hard-rename the memory binding kind `github` -> `tracker`. Four steps,
/// all inside ONE self-wrapped `BEGIN IMMEDIATE` (the ladder runs apply fns in AUTOCOMMIT), each
/// individually conditional/idempotent so a replay converges:
///
///  1. [`create_papertrail_tables`] — `IF NOT EXISTS` no-ops on any modern DB (the baseline already
///     ran it); real work only for an isolation fixture driving this fn directly.
///  2. FIRST-APPLY-ONLY BACKFILL, gated on the legacy `github_issues` table existing: mechanical
///     copy — `tracker = 'github'`, `project = owner || '/' || repo`, `item_key = CAST(number AS
///     TEXT)`, `item_kind` from `is_pull_request` — with the GitHub issue-shadow DEDUPED (a change
///     request becomes ONE row: the pulls copy wins, a shadow-only row falls back via `INSERT OR
///     IGNORE` on the natural key); reviews / review comments fold into `papertrail_comments`
///     behind `review_state` / `anchor_path`; refs copy verbatim. `repo_id` copies VERBATIM
///     (placeholder rows stay placeholder for `register_repo` to adopt; the V044/V045 per-repo-copy
///     semantics carry over unchanged). The `papertrail_fts` mirror is re-derived from the
///     freshly-backfilled base tables (the V045 in-migration posture) via the standing
///     [`crate::index::papertrail::rebuild_fts`], which recomputes `classification` with the
///     current classifier. The seven github_* tables + `github_fts` are then DROPPED — the gate can
///     never fire again, so the backfill is structurally first-apply-only.
///  3. Memory bindings: gated on the legacy `github_owner` column existing — `binding_kind =
///     'github'` rows become `binding_kind = 'tracker'` with `binding_id = 'github:' || owner ||
///     '/' || repo || '#' || number` and the new `tracker` / `project` / `item_key` columns
///     populated; the three github_* columns are then dropped. The `github` binding kind ceases to
///     exist.
///  4. The `github_last_sync_ms` repo_meta key renames to `papertrail_last_sync_ms`.
pub(crate) fn apply_papertrail_provider_neutral_schema(conn: &Connection) -> rusqlite::Result<()> {
    // Fast path: nothing legacy left anywhere — the common post-V060 replay
    // (`create_or_migrate` / `rebuild` / `index --full`) short-circuits before the write lock.
    if sqlite_object_exists(conn, "table", "papertrail_items")?
        && !sqlite_object_exists(conn, "table", "github_issues")?
        && !column_exists(conn, "repo_memory_bindings", "github_owner")?
    {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        create_papertrail_tables(conn)?;
        if sqlite_object_exists(conn, "table", "github_issues")? {
            backfill_papertrail_from_github_tables(conn)?;
            // Re-derive the mirror from the freshly-backfilled base tables so the migrated cache
            // is scoped-searchable immediately (no sync required) — the V045 posture.
            crate::index::papertrail::rebuild_fts(conn)?;
            conn.execute_batch(
                "
                DROP TABLE IF EXISTS github_refs;
                DROP TABLE IF EXISTS github_issues;
                DROP TABLE IF EXISTS github_comments;
                DROP TABLE IF EXISTS github_pull_requests;
                DROP TABLE IF EXISTS github_reviews;
                DROP TABLE IF EXISTS github_review_comments;
                DROP TABLE IF EXISTS github_ref_sync;
                DROP TABLE IF EXISTS github_fts;
                ",
            )?;
        }
        migrate_memory_bindings_to_tracker_kind(conn)?;
        if sqlite_object_exists(conn, "table", "repo_meta")? {
            conn.execute_batch(
                "
                INSERT OR REPLACE INTO repo_meta(repo_id, key, value)
                    SELECT repo_id, 'papertrail_last_sync_ms', value
                    FROM repo_meta WHERE key = 'github_last_sync_ms';
                DELETE FROM repo_meta WHERE key = 'github_last_sync_ms';
                ",
            )?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

pub(crate) fn apply_papertrail_ref_item_kind(conn: &Connection) -> rusqlite::Result<()> {
    if !column_exists(conn, "papertrail_refs", "item_kind")? {
        conn.execute_batch("ALTER TABLE papertrail_refs ADD COLUMN item_kind TEXT;")?;
    }
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_papertrail_refs_item;
         DROP INDEX IF EXISTS idx_papertrail_refs_unique;
         CREATE INDEX idx_papertrail_refs_item
             ON papertrail_refs(repo_id, tracker, project, item_kind, item_key);
         CREATE UNIQUE INDEX idx_papertrail_refs_unique
             ON papertrail_refs(repo_id, tracker, project, COALESCE(item_kind, ''), item_key,
                                source_kind, COALESCE(source_path, ''),
                                COALESCE(source_commit, ''), source_text);",
    )
}

/// V062 (#591): repo-wide comments have an independent timestamp lane and a page token that is
/// committed only after its page is stored. Existing cursors start with no comment watermark, so
/// the first native pass safely replays the comment streams instead of inheriting the item mark.
pub(crate) fn apply_papertrail_comment_cursor(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_sync_cursor", "comment_high_mark_at", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "comment_page_token", "TEXT")?;
    Ok(())
}

/// V063 (#591): every multi-request mirror lane persists enough state to resume after a governed
/// pause. The processed-key sets are bounded by one provider page; `full_rewalk_seen` lives on the
/// item row so a full walk can mark/sweep across arbitrarily many invocations without a giant
/// in-memory set.
pub(crate) fn apply_papertrail_mirror_resume_state(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_sync_cursor", "comment_scan_since", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "comment_stream_cursors", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "item_delta_page_token", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "item_delta_scan_since", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "item_delta_high_mark_at", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "backfill_page_cursor", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "item_thread_cursor", "TEXT")?;
    add_column_if_missing(
        conn,
        "papertrail_sync_cursor",
        "item_delta_in_progress",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "papertrail_sync_cursor",
        "item_delta_replay_required",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "delta_processed_keys", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "backfill_processed_keys", "TEXT")?;
    add_column_if_missing(
        conn,
        "papertrail_sync_cursor",
        "full_rewalk",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "papertrail_items",
        "full_rewalk_seen",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

/// The V060 base-table backfill (step 2 of [`apply_papertrail_provider_neutral_schema`]): runs
/// INSIDE the caller's transaction, only when the legacy tables exist. Every copy is
/// `INSERT OR IGNORE` on the new natural keys, so the ordering below defines the winner where the
/// legacy cache held two views of one item: the pulls-endpoint copy of a change request (which
/// carries `merged_at`) wins over its issues-endpoint shadow.
fn backfill_papertrail_from_github_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        -- Plain issues.
        INSERT OR IGNORE INTO papertrail_items(
            tracker, project, item_kind, item_key, url, state, title, body, author, created_at,
            updated_at, merged_at, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'issue', CAST(number AS TEXT), html_url, state,
               title, body, author, created_at, updated_at, NULL, synced_at_ms, repo_id
        FROM github_issues WHERE is_pull_request = 0;

        -- Change requests, from the richer pulls rows (merged_at) ...
        INSERT OR IGNORE INTO papertrail_items(
            tracker, project, item_kind, item_key, url, state, title, body, author, created_at,
            updated_at, merged_at, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'change_request', CAST(number AS TEXT), html_url,
               state, title, body, author, created_at, updated_at, merged_at, synced_at_ms, repo_id
        FROM github_pull_requests;

        -- ... and from issue-shadow rows whose pulls row never landed (a partial legacy cache):
        -- the OR IGNORE dedupes against the pulls copy above on the natural key.
        INSERT OR IGNORE INTO papertrail_items(
            tracker, project, item_kind, item_key, url, state, title, body, author, created_at,
            updated_at, merged_at, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'change_request', CAST(number AS TEXT), html_url,
               state, title, body, author, created_at, updated_at, NULL, synced_at_ms, repo_id
        FROM github_issues WHERE is_pull_request = 1;

        -- Thread comments: the parent kind resolves through the cached parent rows (a comment on
        -- a change request keeps its real parent kind); an orphan defaults to 'issue' — the
        -- provisional kind the comment mappers also use for GitHub's shared numbering.
        INSERT OR IGNORE INTO papertrail_comments(
            tracker, project, item_kind, item_key, comment_id, url, body, author, created_at,
            updated_at, review_state, anchor_path, synced_at_ms, repo_id
        )
        SELECT 'github', c.owner || '/' || c.repo,
               CASE WHEN EXISTS (
                        SELECT 1 FROM github_issues i
                        WHERE i.repo_id = c.repo_id AND i.owner = c.owner AND i.repo = c.repo
                          AND i.number = c.number AND i.is_pull_request = 1
                    ) OR EXISTS (
                        SELECT 1 FROM github_pull_requests p
                        WHERE p.repo_id = c.repo_id AND p.owner = c.owner AND p.repo = c.repo
                          AND p.number = c.number
                    ) THEN 'change_request' ELSE 'issue' END,
               CAST(c.number AS TEXT), 'comment:' || CAST(c.id AS TEXT), c.html_url, c.body, \
         c.author,
               c.created_at, c.updated_at, NULL, NULL, c.synced_at_ms, c.repo_id
        FROM github_comments c;

        -- Review events: review_state marks them; reviews only exist on change requests.
        INSERT OR IGNORE INTO papertrail_comments(
            tracker, project, item_kind, item_key, comment_id, url, body, author, created_at,
            updated_at, review_state, anchor_path, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'change_request', CAST(number AS TEXT),
               'review:' || CAST(id AS TEXT), html_url, body, author, submitted_at, submitted_at,
               state, NULL,
               synced_at_ms, repo_id
        FROM github_reviews;

        -- File-anchored review comments: anchor_path marks them.
        INSERT OR IGNORE INTO papertrail_comments(
            tracker, project, item_kind, item_key, comment_id, url, body, author, created_at,
            updated_at, review_state, anchor_path, synced_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, 'change_request', CAST(number AS TEXT),
               'review_comment:' || CAST(id AS TEXT), html_url, body, author, created_at,
               updated_at, NULL, path,
               synced_at_ms, repo_id
        FROM github_review_comments;

        -- A legacy failed ref sync is an explicit retry marker. The referenced-only lane now uses
        -- the presence of a cached item as its sole completion signal, so carrying a partially
        -- cached item across V060 would turn that failure into a permanent skip. Remove both the
        -- item and any partial children; a successful legacy row keeps its complete cache.
        DELETE FROM papertrail_comments
        WHERE tracker = 'github' AND EXISTS (
            SELECT 1 FROM github_ref_sync s
            WHERE s.status = 'failed'
              AND s.repo_id = papertrail_comments.repo_id
              AND s.owner || '/' || s.repo = papertrail_comments.project
              AND CAST(s.number AS TEXT) = papertrail_comments.item_key
        );
        DELETE FROM papertrail_items
        WHERE tracker = 'github' AND EXISTS (
            SELECT 1 FROM github_ref_sync s
            WHERE s.status = 'failed'
              AND s.repo_id = papertrail_items.repo_id
              AND s.owner || '/' || s.repo = papertrail_items.project
              AND CAST(s.number AS TEXT) = papertrail_items.item_key
        );

        -- Refs copy verbatim (annotation layer). The per-ref github_ref_sync state machine is
        -- DELETED, not migrated — papertrail_sync_cursor starts empty.
        INSERT OR IGNORE INTO papertrail_refs(
            tracker, project, item_key, ref_kind, source_kind, source_path, source_commit,
            source_text, discovered_at_ms, repo_id
        )
        SELECT 'github', owner || '/' || repo, CAST(number AS TEXT), ref_kind, source_kind,
               source_path, source_commit, source_text, discovered_at_ms, repo_id
        FROM github_refs;
        ",
    )
}

/// The V060 memory-binding rename (step 3): `binding_kind = 'github'` -> `'tracker'`, the three
/// legacy columns fold into `tracker` / `project` / `item_key`, and the legacy columns are
/// dropped. Gated on the legacy `github_owner` column so a fresh DB (baseline already creates the
/// new columns) and a replay are clean no-ops. The `binding_id` rewrite keeps the PK 1:1 (every
/// legacy id `owner/repo#N` maps to exactly one `github:owner/repo#N`).
fn migrate_memory_bindings_to_tracker_kind(conn: &Connection) -> rusqlite::Result<()> {
    if !column_exists(conn, "repo_memory_bindings", "github_owner")? {
        return Ok(());
    }
    add_column_if_missing(conn, "repo_memory_bindings", "tracker", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "project", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "item_key", "TEXT")?;
    conn.execute_batch(
        "
        UPDATE repo_memory_bindings SET
            binding_kind = 'tracker',
            binding_id = 'github:' || github_owner || '/' || github_repo || '#' ||
                         CAST(github_number AS TEXT),
            tracker = 'github',
            project = github_owner || '/' || github_repo,
            item_key = CAST(github_number AS TEXT)
        WHERE binding_kind = 'github';
        ALTER TABLE repo_memory_bindings DROP COLUMN github_owner;
        ALTER TABLE repo_memory_bindings DROP COLUMN github_repo;
        ALTER TABLE repo_memory_bindings DROP COLUMN github_number;
        ",
    )
}

/// Rebuild `github_comments` with `(repo_id, id)` uniqueness, backfilled once per owning parent
/// (issues ∪ pull requests — issue comments cover both) plus the own-repo_id lossless pass. Runs
/// inside the caller's transaction.
fn rebuild_github_comments_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_comments_new;
        CREATE TABLE github_comments_new(
            id INTEGER NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_comments_new(
            id, owner, repo, number, html_url, body, author, created_at, updated_at, synced_at_ms,
            repo_id
        )
        SELECT c.id, c.owner, c.repo, c.number, c.html_url, c.body, c.author, c.created_at,
               c.updated_at, c.synced_at_ms, p.repo_id
        FROM github_comments c
        JOIN (SELECT owner, repo, number, repo_id FROM github_issues
              UNION
              SELECT owner, repo, number, repo_id FROM github_pull_requests) p
          ON p.owner = c.owner AND p.repo = c.repo AND p.number = c.number;
        INSERT INTO github_comments_new(
            id, owner, repo, number, html_url, body, author, created_at, updated_at, synced_at_ms,
            repo_id
        )
        SELECT c.id, c.owner, c.repo, c.number, c.html_url, c.body, c.author, c.created_at,
               c.updated_at, c.synced_at_ms, c.repo_id
        FROM github_comments c
        WHERE NOT EXISTS (SELECT 1 FROM github_comments_new n
                          WHERE n.repo_id = c.repo_id AND n.id = c.id);
        DROP TABLE github_comments;
        ALTER TABLE github_comments_new RENAME TO github_comments;
        CREATE UNIQUE INDEX idx_github_comments_repo_unique ON github_comments(repo_id, id);
        ",
    )
}

/// Rebuild `github_reviews` with `(repo_id, id)` uniqueness, backfilled once per owning
/// pull-request repo plus the own-repo_id lossless pass.
fn rebuild_github_reviews_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_reviews_new;
        CREATE TABLE github_reviews_new(
            id INTEGER NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT,
            state TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            submitted_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_reviews_new(
            id, owner, repo, number, html_url, state, body, author, submitted_at, synced_at_ms,
            repo_id
        )
        SELECT r.id, r.owner, r.repo, r.number, r.html_url, r.state, r.body, r.author,
               r.submitted_at, r.synced_at_ms, p.repo_id
        FROM github_reviews r
        JOIN (SELECT DISTINCT owner, repo, number, repo_id FROM github_pull_requests) p
          ON p.owner = r.owner AND p.repo = r.repo AND p.number = r.number;
        INSERT INTO github_reviews_new(
            id, owner, repo, number, html_url, state, body, author, submitted_at, synced_at_ms,
            repo_id
        )
        SELECT r.id, r.owner, r.repo, r.number, r.html_url, r.state, r.body, r.author,
               r.submitted_at, r.synced_at_ms, r.repo_id
        FROM github_reviews r
        WHERE NOT EXISTS (SELECT 1 FROM github_reviews_new n
                          WHERE n.repo_id = r.repo_id AND n.id = r.id);
        DROP TABLE github_reviews;
        ALTER TABLE github_reviews_new RENAME TO github_reviews;
        CREATE UNIQUE INDEX idx_github_reviews_repo_unique ON github_reviews(repo_id, id);
        ",
    )
}

/// Rebuild `github_review_comments` with `(repo_id, id)` uniqueness, backfilled once per owning
/// pull-request repo plus the own-repo_id lossless pass.
fn rebuild_github_review_comments_with_repo_scoped_key(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_review_comments_new;
        CREATE TABLE github_review_comments_new(
            id INTEGER NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            path TEXT,
            html_url TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        );
        INSERT INTO github_review_comments_new(
            id, owner, repo, number, path, html_url, body, author, created_at, updated_at,
            synced_at_ms, repo_id
        )
        SELECT c.id, c.owner, c.repo, c.number, c.path, c.html_url, c.body, c.author,
               c.created_at, c.updated_at, c.synced_at_ms, p.repo_id
        FROM github_review_comments c
        JOIN (SELECT DISTINCT owner, repo, number, repo_id FROM github_pull_requests) p
          ON p.owner = c.owner AND p.repo = c.repo AND p.number = c.number;
        INSERT INTO github_review_comments_new(
            id, owner, repo, number, path, html_url, body, author, created_at, updated_at,
            synced_at_ms, repo_id
        )
        SELECT c.id, c.owner, c.repo, c.number, c.path, c.html_url, c.body, c.author,
               c.created_at, c.updated_at, c.synced_at_ms, c.repo_id
        FROM github_review_comments c
        WHERE NOT EXISTS (SELECT 1 FROM github_review_comments_new n
                          WHERE n.repo_id = c.repo_id AND n.id = c.id);
        DROP TABLE github_review_comments;
        ALTER TABLE github_review_comments_new RENAME TO github_review_comments;
        CREATE UNIQUE INDEX idx_github_review_comments_repo_unique
            ON github_review_comments(repo_id, id);
        ",
    )
}

// ============================================================================================
// Memory-sync phase A5 (periphery scoping) — V042.
//
// Registered in `ADDITIVE_MIGRATIONS` as V042 (id `042_repo_id_periphery_scoping`): the whole
// schema change is `apply_repo_id_periphery_scoping` below, shaped like every other ladder step
// (idempotent, atomic, torn-state re-convergent, placeholder-backfilled). It stacks on V041 (the
// GitHub papertrail scoping); the two were authored in parallel workstreams and consolidated here.
//
// GATING (why the queries still probe): every periphery query sweep gates its `repo_id` predicate
// on [`super::periphery_repo_scope`] — column present ⇒ scope by the active repo, column absent ⇒
// the original unscoped SQL. On a normal open `schema::apply` runs the full ladder including V042,
// so the column is always present and the scoped path always taken. The absent branch is the
// defensive path for a raw connection that never ran the ladder (and the pre-migration schema in
// forward-migration bootstrap tests): it degrades to the pre-A5 repo-global behavior instead of
// referencing a column that does not exist. Keeping the probe is what lets raw-connection callers
// and partial-schema fixtures share these code paths without a separate unscoped variant.
// ============================================================================================

/// The periphery tables A5 scopes DIRECTLY by `repo_id` via an ADDITIVE `repo_id` column (no PK /
/// UNIQUE change — [`add_column_if_missing`] suffices, idempotent + AUTOCOMMIT-safe). Adoption
/// re-points their placeholder rows (see `registry::A5_PERIPHERY_DIRECT_SCOPED_TABLES`).
const A5_ADDITIVE_SCOPED_TABLES: &[&str] = &[
    // Per-repo generation counter (the generation integer stays globally unique — see the apply
    // fn — so the transitive `clone_edges`/`clone_subblock_postings` need no `repo_id`).
    "clone_graph_generations",
    // Oracle run log; the read "key" (repo_id, tool, tool_version, commit_sha, worktree_id) is a
    // query predicate, not a UNIQUE, so a plain column is enough.
    "oracle_runs",
    // Reconcile run log (no UNIQUE); the "latest attempt" read scopes by repo_id.
    "reconcile_attempts",
    // Memories + per-binding repo_id (spec §4.5 cross-repo bindings default to the parent memory's
    // repo; the PK id TEXT / (memory_id, binding_kind, binding_id) are unchanged).
    "repo_memories",
    "repo_memory_bindings",
];

/// V042 (memory-sync phase A5, see the block comment above): add `repo_id`
/// scoping to the clone / oracle / reconcile / memory PERIPHERY tables — every direct-scoped table
/// left after A3's core sweep (per the plan's disposition table). Two shapes:
///
///  * ADDITIVE column ([`A5_ADDITIVE_SCOPED_TABLES`]): `clone_graph_generations`, `oracle_runs`,
///    `reconcile_attempts`, `repo_memories`, `repo_memory_bindings` — no key change, so
///    `add_column_if_missing` (self-guarding) is all that is needed.
///  * KEY REBUILD (`repo_id` joins the PK / UNIQUE, so SQLite forces a table rebuild):
///    `clone_token_df` (PK `(repo_id, normalizer_kind, token_hash)` — df must not pool across
///    repos), `clone_refinements` (PK `(repo_id, class_key)` — content class-keys collide across
///    repos), `edge_oracle` (content-key PK gains a leading `repo_id`), `logical_symbol_monikers`
///    (PK `(repo_id, logical_symbol_id, tool)`), `dream_findings` (UNIQUE `(repo_id, kind, subject,
///    claim_hash)`).
///  * STANDALONE FTS rebuild: `repo_memory_fts` gains `repo_id UNINDEXED` and a mandatory filter —
///    rebuilt from `repo_memories` (the same content `upsert_memory_fts` writes), so `repo_id`
///    comes from each memory's freshly-added column.
///
/// `clone_edges` / `clone_subblock_postings` are NOT scoped directly (disposition: transitive via
/// the generations FK). The generation integer stays GLOBALLY unique (allocated `MAX(generation) +
/// 1` over ALL repos), so a `build_generation` value belongs to exactly one repo and those children
/// are scoped for free; only the generation SWEEPS (`complete_generation`, the Building cleanup,
/// the postings invalidation) must add a `repo_id` predicate so a repo's precompute never deletes a
/// sibling's generations. `symbol_fingerprints` / `symbol_token_postings` stay `symbol_id`-keyed
/// (transitive via `symbols -> files.repo_id`) — the "NEVER add scope columns here" rule in
/// [`CLONE_FINGERPRINT_DDL`] is about the COMMIT/WORKTREE scope axis; `repo_id` is a different
/// (cross-repo) axis and only `clone_token_df` / `clone_refinements` there take it.
///
/// MECHANICS (identical to V040): the key rebuilds RENAME/DROP tables, which needs FK enforcement
/// OFF; a PRAGMA toggle is a no-op inside a transaction, so it is set BEFORE `BEGIN IMMEDIATE`. The
/// whole migration runs under ONE atomic transaction (the ladder runs apply fns in AUTOCOMMIT, so a
/// multi-statement rebuild must self-wrap), and `repo_memories.repo_id` existing is the
/// all-or-nothing sentinel: because every statement commits together, that column is present iff
/// the whole migration committed, so a re-apply (fresh DB already transformed, or an `index --full`
/// re-run once this is registered) short-circuits before taking the write lock. Each rebuild starts
/// `DROP TABLE IF EXISTS <scratch>_new` so a prior pass killed mid-rebuild (bypassing a clean
/// rollback) re-converges from a clean slate rather than failing on `CREATE`.
pub(crate) fn apply_repo_id_periphery_scoping(conn: &Connection) -> rusqlite::Result<()> {
    // All-or-nothing sentinel (see the doc comment): everything below commits atomically, so
    // `repo_memories.repo_id` present means the whole migration already ran.
    if column_exists(conn, "repo_memories", "repo_id")? {
        return Ok(());
    }

    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        // --- Additive columns (no key change): idempotent, placeholder-backfilled. ---
        for table in A5_ADDITIVE_SCOPED_TABLES {
            add_column_if_missing(conn, table, "repo_id", REPO_ID_COLUMN_DEF)?;
        }

        // --- Key rebuilds (repo_id joins the PK / UNIQUE). ---
        rebuild_clone_token_df_with_repo_id(conn)?;
        rebuild_clone_refinements_with_repo_id(conn)?;
        rebuild_edge_oracle_with_repo_id(conn)?;
        rebuild_logical_symbol_monikers_with_repo_id(conn)?;
        rebuild_dream_findings_with_repo_id(conn)?;

        // --- Standalone FTS rebuild (repo_id UNINDEXED), rebuilt from `repo_memories`. ---
        rebuild_repo_memory_fts_with_repo_id(conn)?;
        // P1 (the V039/V040 class): the additive columns' static `DEFAULT '__unassigned__'`, the
        // rebuilds' literal `'__unassigned__'` copy, and the FTS repopulated from those rows all
        // stamp the placeholder — and on an ALREADY-ADOPTED DB `register_repo`'s fast path never
        // re-points, so the scoped periphery reads (clones / oracle / memories) would miss this
        // repo's rows. Resolve the sole `repos` row and re-point them onto it (no-op on an
        // un-adopted DB, where the rows correctly wait for `register_repo`). Atomic with the
        // sentinel-setting steps because it runs inside this same txn.
        backfill_periphery_repo_id_to_sole_repo(conn)?;
        // Finding ids now FOLD `repo_id` (`dream::repo_folded_finding_id` — the stable_id
        // precedent), so re-derive every persisted id under its post-backfill `repo_id`. Runs
        // AFTER the backfill so an adopted DB's ids fold the real repo, not the placeholder.
        // `superseded_by` (the only persisted reference to a finding id, in-table) is remapped by
        // the helper. Replay-safe: the sentinel short-circuits a re-apply, and the re-derivation
        // is idempotent anyway.
        crate::dream::rederive_finding_ids(conn)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
        return result;
    }
    conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// P1 backfill (the V040 [`backfill_repo_id_to_sole_repo`] pattern, periphery edition): re-point
/// every V042 periphery row — the additive-column tables, the PK-rebuilt tables, AND the
/// `repo_memory_fts` mirror — from the placeholder onto the sole `repos` id when the DB is ALREADY
/// ADOPTED, so the scoped clone / oracle / memory reads see this repo's rows immediately after the
/// upgrade. No-op on an un-adopted DB (`sole_repo_id` == the placeholder — the rows correctly wait
/// for `register_repo`). Runs inside the V042 txn (FK OFF), so it is atomic with the sentinel. The
/// sentinel short-circuit keeps a replay off this path entirely; the explicit `repos`-cardinality
/// gate below additionally makes the FIRST apply safe on a CONSOLIDATED DB (leave-at-placeholder
/// instead of `sole_repo_id`'s one-row hard error aborting the upgrade). The table set mirrors
/// `A5_PERIPHERY_DIRECT_SCOPED_TABLES` (registry.rs), the same rows adoption re-points.
fn backfill_periphery_repo_id_to_sole_repo(conn: &Connection) -> rusqlite::Result<()> {
    // Cardinality gate (the V041 github-backfill twin): on `!= 1` repos rows there is no single
    // owner to re-point onto, so the periphery rows stay under the placeholder rather than
    // aborting the forward-migration. One nuance vs the github edition: placeholder-stranded
    // `repo_memories` would be USER-AUTHORED data invisible to scoped reads, not a refetchable
    // cache. That state is only reachable by hand-driving `register_repo` pre-consolidation
    // (phase A refuses a second real repo; the real multi-repo entry path arrives with the
    // consolidate importer, which attributes memories at import) — and `memory doctor` surfaces
    // any placeholder-scoped memories (one `placeholder_repo` entry each, `doctor_report`) so
    // they are visible, not silently lost. The clone / oracle / reconcile tables are derived
    // caches the next rebuild / oracle run / reconcile re-populates under the proper stamp.
    //
    // No github-style RECLAIM is needed here (audited per table): unlike the V041 github tables,
    // whose natural keys exclude `repo_id` and whose stranded rows therefore OCCUPY the key the
    // next sync writes to (see the upsert-reclaim invariant in `github/store.rs`), every V042
    // periphery key either LEADS with `repo_id` (clone_token_df, clone_refinements, edge_oracle,
    // logical_symbol_monikers, dream_findings' UNIQUE), is an append-only autoincrement log
    // (oracle_runs, reconcile_attempts), a fresh generated id (repo_memories + its bindings/fts,
    // keyed by the repo-unique memory id), or a globally-unique counter (clone_graph_generations,
    // allocated MAX(generation)+1 across ALL repos). New writes under the real repo can never
    // conflict with a placeholder row — stranded rows merely LINGER invisibly until a later gc
    // sweep reclaims the space (acceptable; nothing blocks repopulation).
    let repos_rows: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |row| row.get(0))?;
    if repos_rows != 1 {
        return Ok(());
    }
    let target = sole_repo_id(conn)?;
    if target == super::LEGACY_REPO_ID {
        return Ok(());
    }
    for table in [
        "clone_graph_generations",
        "clone_token_df",
        "clone_refinements",
        "oracle_runs",
        "edge_oracle",
        "logical_symbol_monikers",
        "reconcile_attempts",
        "dream_findings",
        "repo_memories",
        "repo_memory_bindings",
        "repo_memory_fts",
    ] {
        conn.execute(&format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"), [
            target.as_str(),
            super::LEGACY_REPO_ID,
        ])?;
    }
    Ok(())
}

/// Rebuild `clone_token_df` with a leading `repo_id` in the PK so document-frequency stats never
/// pool across repos (a shared df corrupts the SourcererCC selectivity ordering for both repos).
/// No FK children; STRICT preserved.
fn rebuild_clone_token_df_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS clone_token_df_new;
        CREATE TABLE clone_token_df_new(
            repo_id         TEXT    NOT NULL DEFAULT '__unassigned__',
            normalizer_kind TEXT    NOT NULL,
            token_hash      INTEGER NOT NULL,
            df              INTEGER NOT NULL,
            PRIMARY KEY (repo_id, normalizer_kind, token_hash)
        ) STRICT;
        INSERT OR IGNORE INTO clone_token_df_new(repo_id, normalizer_kind, token_hash, df)
        SELECT '__unassigned__', normalizer_kind, token_hash, df FROM clone_token_df;
        DROP TABLE clone_token_df;
        ALTER TABLE clone_token_df_new RENAME TO clone_token_df;
        ",
    )
}

/// Rebuild `clone_refinements` with a leading `repo_id` in the PK — `class_key` is content-derived
/// (normalized code shape), so two repos with the same clone shape would otherwise clobber each
/// other's cached refinement. STRICT preserved; the full V030 column set (incl. `lcs_sampled`) is
/// reproduced.
fn rebuild_clone_refinements_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS clone_refinements_new;
        CREATE TABLE clone_refinements_new(
            repo_id                 TEXT    NOT NULL DEFAULT '__unassigned__',
            class_key               TEXT    NOT NULL,
            language                TEXT    NOT NULL,
            refine_mode             TEXT    NOT NULL,
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
            lcs_sampled             INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (repo_id, class_key)
        ) STRICT;
        INSERT OR IGNORE INTO clone_refinements_new(
            repo_id, class_key, language, refine_mode, template, variation_points_json,
            proposed_signature_json, confidence, anti_unify_coverage, lcs_ratio, refactorability,
            norm_version, alignment_version, created_at_ms, lcs_sampled
        )
        SELECT
            '__unassigned__', class_key, language, refine_mode, template, variation_points_json,
            proposed_signature_json, confidence, anti_unify_coverage, lcs_ratio, refactorability,
            norm_version, alignment_version, created_at_ms, lcs_sampled
        FROM clone_refinements;
        DROP TABLE clone_refinements;
        ALTER TABLE clone_refinements_new RENAME TO clone_refinements;
        ",
    )
}

/// Rebuild `edge_oracle` with a leading `repo_id` in the content-key PK. It stays content-anchored
/// with NO FK to `edges_data` (the #248 rule — the read join to live edges filters dangling rows),
/// so prepending `repo_id` is a pure PK-widening rebuild; the three indexes are recreated. STRICT
/// preserved. A cross-repo content-key collision (same source span in two repos) would otherwise
/// surface one repo's verdict for the other; the scoped reads (store.rs) filter `repo_id`.
fn rebuild_edge_oracle_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS edge_oracle_new;
        CREATE TABLE edge_oracle_new(
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
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
                repo_id, tool, tool_version, source_path,
                source_start_byte, source_end_byte,
                callee_start_byte, callee_end_byte, edge_kind
            )
        ) STRICT;
        INSERT OR IGNORE INTO edge_oracle_new(
            repo_id, source_path, source_start_byte, source_end_byte, callee_start_byte,
            callee_end_byte, edge_kind, file_sha, tool, tool_version, resolved_symbol_id,
            scip_symbol, kind, computed_at
        )
        SELECT
            '__unassigned__', source_path, source_start_byte, source_end_byte, callee_start_byte,
            callee_end_byte, edge_kind, file_sha, tool, tool_version, resolved_symbol_id,
            scip_symbol, kind, computed_at
        FROM edge_oracle;
        DROP TABLE edge_oracle;
        ALTER TABLE edge_oracle_new RENAME TO edge_oracle;
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_staleness
            ON edge_oracle(file_sha, tool, tool_version);
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_symbol
            ON edge_oracle(resolved_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_anchor
            ON edge_oracle(source_path, callee_start_byte, callee_end_byte, edge_kind);
        ",
    )
}

/// Rebuild `logical_symbol_monikers` with a leading `repo_id` in the PK. The id is content-derived
/// (`LogicalSymbolKey::stable_id`) so it collides across repos; NO FK to `logical_symbols` (the #70
/// rule — reads join live logical symbols). The moniker index is recreated. STRICT preserved.
fn rebuild_logical_symbol_monikers_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS logical_symbol_monikers_new;
        CREATE TABLE logical_symbol_monikers_new(
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            logical_symbol_id INTEGER NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            moniker TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(repo_id, logical_symbol_id, tool)
        ) STRICT;
        INSERT OR IGNORE INTO logical_symbol_monikers_new(
            repo_id, logical_symbol_id, tool, tool_version, moniker, computed_at
        )
        SELECT '__unassigned__', logical_symbol_id, tool, tool_version, moniker, computed_at
        FROM logical_symbol_monikers;
        DROP TABLE logical_symbol_monikers;
        ALTER TABLE logical_symbol_monikers_new RENAME TO logical_symbol_monikers;
        CREATE INDEX IF NOT EXISTS idx_logical_symbol_monikers_moniker
            ON logical_symbol_monikers(moniker, tool);
        ",
    )
}

/// Rebuild `dream_findings` with `repo_id` in the UNIQUE `(repo_id, kind, subject, claim_hash)` —
/// the dream worklist's identity key is content-derived (subject + claim hash of a memory-anchored
/// finding), so it must not merge two repos' findings. The `id TEXT PRIMARY KEY` is unchanged; both
/// indexes are recreated. STRICT preserved.
fn rebuild_dream_findings_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS dream_findings_new;
        CREATE TABLE dream_findings_new(
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
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            UNIQUE(repo_id, kind, subject, claim_hash)
        ) STRICT;
        INSERT OR IGNORE INTO dream_findings_new(
            id, kind, subject, claim_hash, evidence, base_rank, status, superseded_by,
            first_seen_at_ms, last_seen_at_ms, reviewed_at_ms, repo_id
        )
        SELECT
            id, kind, subject, claim_hash, evidence, base_rank, status, superseded_by,
            first_seen_at_ms, last_seen_at_ms, reviewed_at_ms, '__unassigned__'
        FROM dream_findings;
        DROP TABLE dream_findings;
        ALTER TABLE dream_findings_new RENAME TO dream_findings;
        CREATE INDEX IF NOT EXISTS idx_dream_findings_status ON dream_findings(status);
        CREATE INDEX IF NOT EXISTS idx_dream_findings_subject ON dream_findings(kind, subject);
        ",
    )
}

/// Rebuild the standalone `repo_memory_fts` with a leading `repo_id UNINDEXED` column and
/// repopulate it from `repo_memories` (the same title/body/kind/tags content `upsert_memory_fts`
/// writes, tags space-joined). Rebuilding from source — rather than copying the old FTS rows — lets
/// `repo_id` come from each memory's freshly-added column and needs no FTS `RENAME` (which has
/// historically been fragile on shadow tables); the DROP + CREATE keeps the canonical table name.
/// `memory_search` then filters `repo_id` after the MATCH.
fn rebuild_repo_memory_fts_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS repo_memory_fts;
        CREATE VIRTUAL TABLE repo_memory_fts USING fts5(
            repo_id UNINDEXED,
            memory_id UNINDEXED,
            title,
            body,
            kind,
            tags,
            tokenize='porter'
        );
        INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
        SELECT
            m.repo_id, m.id, m.title, m.body, m.kind,
            COALESCE(
                (SELECT group_concat(t.tag, ' ')
                 FROM repo_memory_tags t WHERE t.memory_id = m.id),
                ''
            )
        FROM repo_memories m;
        ",
    )
}

/// V035: add `symbols.is_test` (cross-language test-code marker computed at parse time; see
/// `parser::detect_is_test`) so clone detection can keep tests out of the corpus. Idempotent via
/// `add_column_if_missing`. Existing rows default to 0 (non-test) until the next reindex
/// repopulates them — accurate `is_test` needs a reindex with this binary.
/// V048 (#465): `repo_memories.payload_json` — a nullable opaque canonical-JSON payload for
/// polymorphic memory nodes. The `Task` / `Concept` kinds carry a kind-specific,
/// `schema_version`-tagged payload here; the core stores it verbatim and folds its canonical form
/// into `content_hash` (`dream::note_content_hash`), so a payload edit self-invalidates the derived
/// dream summary/verdict rows exactly as a title/body edit does. Additive + nullable: existing rows
/// read back `NULL` (no payload).
pub(crate) fn apply_memory_payload_json(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "repo_memories", "payload_json", "TEXT")?;
    Ok(())
}

/// V049 (#464): `repo_node_edges` — the typed, content-addressed, cross-repo edge set. One row per
/// edge from a source memory NODE to a target that is either another node or a code/github anchor.
///
/// Design invariants baked into the shape:
///   - `edge_key` (PK) is the stable content-addressed identity: a `rebind` re-resolves the local
///     rowids WITHOUT changing the key, so a sync fold keeps presence/tombstones keyed by it.
///   - `repo_id` is the OWNER repo (the source node's repo — the periphery scope + adoption key);
///     `target_repo_id` is the target's repo, which MAY differ (a cross-repo edge into a sibling).
///   - Portable `target_kind` + `target_anchor` carry the durable target identity; the resolved
///     local rowids (`target_node_id` / `target_logical_symbol_id`) are re-derivable and carry NO
///     FK to volatile graph rows (the #248 rule — a reindex must never cascade-delete a durable
///     edge). The ONLY FK is `source_node_id` -> `repo_memories(id)` (a durable node), cascading so
///     an edge dies with its source.
///   - `anchor_status` is `current` | `gone` | `unresolved` (the last for a cross-repo target whose
///     repo is not present locally yet — re-resolved when it is indexed, never a hard failure).
pub(crate) fn apply_repo_node_edges(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS repo_node_edges(
            edge_key TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            source_node_id TEXT NOT NULL,
            relation TEXT NOT NULL,
            target_repo_id TEXT NOT NULL,
            target_kind TEXT NOT NULL,
            target_anchor TEXT NOT NULL,
            target_node_id TEXT,
            target_logical_symbol_id INTEGER,
            symbol_kind TEXT,
            signature_hash TEXT,
            anchor_status TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(source_node_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_repo_node_edges_source
            ON repo_node_edges(source_node_id, relation);
        -- Reverse traversal (edges_into) matches on the globally-unique
        -- (target_kind, target_anchor) only, not target_repo_id (the anchor
        -- determines its repo), so the index leads with exactly those two columns.
        CREATE INDEX IF NOT EXISTS idx_repo_node_edges_target
            ON repo_node_edges(target_kind, target_anchor);
        ",
    )
}

/// V050 (#473): incremental clone-graph delta maintenance. The delta pass deletes a changed file's
/// postings by `(build_generation, path)` — unindexed until now (the PK leads with `token_hash`) —
/// and tracks how many files the live generation has absorbed since its full build
/// (`delta_files_applied`, the df-drift signal that schedules the next full rebuild). Both
/// additive + idempotent; the column type is STRICT-valid and defaulted so existing generation
/// rows read back 0 (no deltas absorbed).
///
/// Pre-freeze postings are NOT delta-ready (#477 review): binaries before the df epoch freeze
/// bumped `clone_token_df` on incremental passes WITHOUT invalidating the postings, so an
/// upgraded index can hold a live generation whose postings are ordered by an older df than the
/// current table — a delta patching it would compute sub-blocks under the moved df and silently
/// miss edges. Clear `postings_written` so those generations take one full rebuild (which re-pins
/// the epoch at its own build). Gated on the delta column being freshly ADDED, so only the first
/// run (a genuinely pre-freeze index) invalidates — a re-apply on an already-frozen index must
/// not throw away a valid graph.
pub(crate) fn apply_clone_delta_maintenance(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_clone_subblock_postings_path
             ON clone_subblock_postings(build_generation, path);",
    )?;
    // ORDER IS LOAD-BEARING (torn-retry safety): the invalidation runs BEFORE the gate column is
    // added. A kill between the two leaves the column absent, so the retry re-runs the
    // (idempotent) invalidation and then adds the column — the gate can never read "already
    // frozen" while the clear is still owed.
    if !column_exists(conn, "clone_graph_generations", "delta_files_applied")? {
        conn.execute_batch("UPDATE clone_graph_generations SET postings_written = 0;")?;
    }
    add_column_if_missing(
        conn,
        "clone_graph_generations",
        "delta_files_applied",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

/// V051 (#479): per-generation df snapshot. The #473 freeze made `clone_token_df` itself the
/// postings' frozen order, which left the LIVE candidate paths reading stale df — a newly indexed
/// token rides `DF_FALLBACK` (sorted last) until the next full build, so a new clone family's
/// sub-block prefixes prefer old tokens, and at the hot-token-cap margin its candidates drop
/// entirely. `clone_df_epoch` pins each generation's build-time df durably (CASCADE-swept with the
/// generation row, like edges/postings), so the persisted-graph consumers read their own build's
/// order while `clone_token_df` is free to move again on incremental passes.
///
/// BACKFILL (the V050→V051 bridge): a pre-epoch DB's servable generations were built under the
/// CURRENT `clone_token_df` — under the #473 freeze df cannot have moved since (any refresh
/// invalidated `postings_written`, and a postings-invalid Building generation is discarded, never
/// resumed) — so snapshotting current df per generation is exact and no forced rebuild is needed.
/// Backfill targets only generations with ZERO epoch rows (each per-generation INSERT is one
/// atomic statement, so a torn retry resumes cleanly), which also keeps a re-apply from folding
/// post-V051 (moving) df rows into an already-pinned epoch. Known degenerate edge: a generation
/// built while its repo's df table was empty backfills nothing; the runtime treats a missing
/// epoch like `postings_written = 0` (one self-healing rebuild).
pub(crate) fn apply_clone_df_epoch(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clone_df_epoch(
             build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation)
                                               ON DELETE CASCADE,
             token_hash       INTEGER NOT NULL,
             df               INTEGER NOT NULL,
             PRIMARY KEY (build_generation, token_hash)
         ) STRICT;",
    )?;
    // Enumerate the epoch-less generations FIRST, then snapshot each with its own atomic INSERT.
    // A single self-referencing `INSERT … WHERE NOT EXISTS(SELECT … FROM clone_df_epoch)` is not
    // used deliberately: SQLite may interleave the SELECT with the insertion, so rows landed for a
    // generation earlier in the same statement could suppress its remaining rows.
    let epoch_less: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT g.generation FROM clone_graph_generations g
             LEFT JOIN clone_df_epoch e ON e.build_generation = g.generation
             WHERE e.build_generation IS NULL
             GROUP BY g.generation",
        )?;
        stmt.query_map([], |row| row.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?
    };
    for generation in epoch_less {
        conn.execute(
            "INSERT INTO clone_df_epoch(build_generation, token_hash, df)
             SELECT g.generation, d.token_hash, d.df
             FROM clone_graph_generations g
             JOIN clone_token_df d
               ON d.repo_id = g.repo_id AND d.normalizer_kind = g.normalizer_kind
             WHERE g.generation = ?1",
            params![generation],
        )?;
    }
    Ok(())
}

/// V052 (#503, phase B C4): the memory op-log storage tables — a durable home for the pure `oplog`
/// primitives (op model, signed hash-chained entry, deterministic fold). Two layers (§5.4):
///
/// - **`oplog_entries`** is layer 1: the opaque signed entry log. `signed_bytes` is the sole source
///   of truth; `entry_hash` (its content address + chain link + idempotency key),
///   `device_fingerprint`, `lamport`, and `prev_hash` are denormalized from the SAME verified entry
///   for indexed access and cannot drift. NO FK — the log is authored data that must OUTLIVE a
///   reindex (the #248 content- addressed discipline); a reindex rewrites the code graph, never
///   this table. `UNIQUE(device_ fingerprint, lamport)` pins each device's chain as strictly linear
///   (a tripwire against a same-slot equivocation; per-`stream_id` fork DETECTION is a later S2
///   increment).
/// - **`oplog_projected_nodes` / `oplog_projected_edges`** are layer 2: a DERIVED shadow
///   projection, wholly rebuilt by the full-replay fold (`store::reproject`) — never a source of
///   truth, so a `DELETE`-all + reinsert is the whole update. `oplog_meta` stamps the projector
///   version so a binary that learns a new op kind re-folds on demand (§5.4 upgrade re-fold) rather
///   than trusting a stale materialization.
///
/// Idempotent (`CREATE TABLE IF NOT EXISTS`), self-transaction-free, replay-write-free (#498); the
/// tables are fresh with no backfill.
pub(crate) fn apply_oplog_storage(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS oplog_entries(
             entry_hash         BLOB PRIMARY KEY,
             device_fingerprint BLOB NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB,
             signed_bytes       BLOB NOT NULL,
             received_at_ms     INTEGER NOT NULL,
             UNIQUE(device_fingerprint, lamport)
         ) STRICT;

         CREATE TABLE IF NOT EXISTS oplog_projected_nodes(
             node_id      TEXT PRIMARY KEY,
             content_json TEXT NOT NULL,
             status       TEXT NOT NULL
         ) STRICT;

         CREATE TABLE IF NOT EXISTS oplog_projected_edges(
             edge_key      TEXT PRIMARY KEY,
             spec_json     TEXT NOT NULL,
             resolved_json TEXT
         ) STRICT;

         CREATE TABLE IF NOT EXISTS oplog_meta(
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         ) STRICT;",
    )?;
    Ok(())
}

/// V053 (#509): scope the op-log by immutable stream identity. One signed chain, watermark, and
/// projection exists per `(stream_id, device)`, so the V052 tables gain a `stream_id` dimension:
/// `UNIQUE(stream_id, device_fingerprint, lamport)` pins each device's chain as strictly linear
/// PER STREAM, and the shadow-projection tables key on `(stream_id, node_id / edge_key)` so a
/// re-fold of one stream never touches another's rows. `oplog_fork_evidence` is new — the
/// quarantine that durably preserves BOTH heads of a detected equivocation (the store previously
/// only RETURNED the colliding entry to the caller); `signed_bytes` is the rejected head verbatim,
/// `conflicting_entry_hash` points at the stored entry it collided with.
///
/// INVARIANT: this is a DROP + CREATE rebuild, which is safe ONLY because nothing writes the
/// op-log tables yet (the module is un-wired until the write-path increment, so every database's
/// copies are empty — there is no data to preserve). Once the log is wired, a re-shape must use a
/// data-preserving recipe instead. `oplog_meta` is left untouched (its projector-version stamp is
/// not stream-scoped). Idempotent (a replay rebuilds the same empty tables); the self-wrapped
/// IMMEDIATE transaction makes an interrupted rebuild reconverge on the next run.
pub(crate) fn apply_oplog_stream_scoping(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         DROP TABLE IF EXISTS oplog_entries;
         CREATE TABLE oplog_entries(
             entry_hash         BLOB PRIMARY KEY,
             stream_id          BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB,
             signed_bytes       BLOB NOT NULL,
             received_at_ms     INTEGER NOT NULL,
             UNIQUE(stream_id, device_fingerprint, lamport)
         ) STRICT;

         DROP TABLE IF EXISTS oplog_projected_nodes;
         CREATE TABLE oplog_projected_nodes(
             stream_id    BLOB NOT NULL,
             node_id      TEXT NOT NULL,
             content_json TEXT NOT NULL,
             status       TEXT NOT NULL,
             PRIMARY KEY(stream_id, node_id)
         ) STRICT;

         DROP TABLE IF EXISTS oplog_projected_edges;
         CREATE TABLE oplog_projected_edges(
             stream_id     BLOB NOT NULL,
             edge_key      TEXT NOT NULL,
             spec_json     TEXT NOT NULL,
             resolved_json TEXT,
             PRIMARY KEY(stream_id, edge_key)
         ) STRICT;

         CREATE TABLE IF NOT EXISTS oplog_fork_evidence(
             stream_id              BLOB NOT NULL,
             entry_hash             BLOB NOT NULL,
             device_fingerprint     BLOB NOT NULL,
             lamport                INTEGER NOT NULL,
             signed_bytes           BLOB NOT NULL,
             conflicting_entry_hash BLOB,
             observed_at_ms         INTEGER NOT NULL,
             PRIMARY KEY(stream_id, entry_hash)
         ) STRICT;

         COMMIT;",
    )?;
    Ok(())
}

/// V054 (#513): the op-log's persisted local device identity. ONE ed25519 keypair per store, so
/// every entry this install authors — live or backfilled — signs under a stable fingerprint
/// instead of a fresh per-process key. Store-global, NOT repo-scoped: a device is a machine
/// identity, orthogonal to the per-repo owner streams it signs (and, later, doubles as the
/// transport node key — a machine singleton). `id INTEGER PRIMARY KEY CHECK (id = 0)` is the
/// single-row guard: a second identity cannot be inserted. `seed` is the 32-byte secret scalar
/// seed; `public_key` (32 bytes) and `fingerprint` (= sha256(public_key)) are derivable from it but
/// stored too so the row is legible and a load can assert they still agree.
///
/// Purely ADDITIVE (`CREATE TABLE IF NOT EXISTS` — a brand-new table, nothing to drop or backfill),
/// unlike the V053 rebuild. Idempotent; the self-wrapped IMMEDIATE transaction reconverges an
/// interrupted create on the next run.
pub(crate) fn apply_oplog_device_identity(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         CREATE TABLE IF NOT EXISTS oplog_device_identity(
             id            INTEGER PRIMARY KEY CHECK (id = 0),
             seed          BLOB NOT NULL,
             public_key    BLOB NOT NULL,
             fingerprint   BLOB NOT NULL,
             created_at_ms INTEGER NOT NULL
         ) STRICT;

         COMMIT;",
    )?;
    Ok(())
}

/// V058 (sync phase C, §5): give the single device identity an X25519 ENCRYPTION keypair beside its
/// ed25519 signing key. Two nullable `BLOB` columns — `x25519_secret` (the 32-byte scalar, the sole
/// durable copy, D4) and `x25519_public` — added to the STRICT `oplog_device_identity` table
/// (`BLOB` is a valid STRICT type). Nullable + additive: an existing row keeps its ed25519 identity
/// and is backfilled at the next `local_device` open via a CAS UPDATE (mirroring the ed25519
/// mint-if-absent race), so a concurrent open cannot split into two encryption identities.
/// Idempotent via `add_column_if_missing`; on a fresh DB this runs right after V054 creates the
/// table, so both columns are present before the first `local_device` call.
pub(crate) fn apply_oplog_device_x25519(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "oplog_device_identity", "x25519_secret", "BLOB")?;
    add_column_if_missing(conn, "oplog_device_identity", "x25519_public", "BLOB")?;
    Ok(())
}

/// V059 (sync phase C, §16.1): the account-log CANDIDATE DAG. `account_entries` stores EVERY
/// structurally-valid, signature-valid account entry — all branches of an equivocating chain are
/// first-class, so the candidate table has NO seq-uniqueness; grow-only (I8). The `accepted` flag
/// is DERIVED, rewritten by every `refold_account` (the fold + branch selection §16.2), and the
/// partial unique index `account_accepted_slot` pins accepted-set uniqueness per `(account, log,
/// device, seq)` slot (I10a). `account_entry_status` holds the projected §16.3 taxonomy per entry;
/// `account_pre_verify` durably holds an entry whose signing device can't yet be resolved
/// (`sha256(pk) == fingerprint` not found among genesis + stored candidates), retried when a later
/// DeviceAdd/AccountGenesis for the claimed account arrives (Codex-8). Idempotent — every statement
/// is `CREATE ... IF NOT EXISTS`, so a torn replay reconverges without a wrapping txn.
pub(crate) fn apply_account_candidate_dag(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_entries(
             entry_hash         BLOB    PRIMARY KEY,
             account_id         BLOB    NOT NULL,
             log_id             INTEGER NOT NULL,
             device_fingerprint BLOB    NOT NULL,
             seq                INTEGER NOT NULL,
             prev_hash          BLOB,
             parent_ref         BLOB,
             authority_ref      BLOB,
             entry_type         INTEGER NOT NULL,
             accepted           INTEGER NOT NULL DEFAULT 0,
             signed_bytes       BLOB    NOT NULL,
             received_at_ms     INTEGER NOT NULL
         ) STRICT;

         CREATE INDEX IF NOT EXISTS account_entries_chain
             ON account_entries(account_id, log_id, device_fingerprint, seq);

         CREATE UNIQUE INDEX IF NOT EXISTS account_accepted_slot
             ON account_entries(account_id, log_id, device_fingerprint, seq) WHERE accepted = 1;

         CREATE TABLE IF NOT EXISTS account_entry_status(
             entry_hash BLOB PRIMARY KEY,
             status     TEXT NOT NULL,
             detail     TEXT
         ) STRICT;

         CREATE TABLE IF NOT EXISTS account_pre_verify(
             signed_hash         BLOB    PRIMARY KEY,
             entry_hash          BLOB    NOT NULL,
             claimed_account_id  BLOB    NOT NULL,
             claimed_fingerprint BLOB    NOT NULL,
             raw_bytes           BLOB    NOT NULL,
             received_at_ms      INTEGER NOT NULL
         ) STRICT;

         -- Promotion scans the queue by claimed_account_id while holding the ingest write lock;
         -- index it so a backlog for OTHER accounts is never full-scanned per ingest.
         CREATE INDEX IF NOT EXISTS account_pre_verify_account
             ON account_pre_verify(claimed_account_id);",
    )
}

/// V064 (sync phase C1, §16): query-ready authority facts derived from the accepted account fold.
/// These are shadow tables, never independent sources of truth: `refold_account` deletes and
/// rewrites every row for one account inside the SAME IMMEDIATE transaction as accepted/status.
/// History intervals (`effective_at`, `closed_at`) preserve audit/projection facts. `auth_len` is
/// only a synchronization assertion: ahead parks for refetch, while behind is informational and
/// never selects historical authority. Exact citations and device cuts make authorization keyed
/// lookups instead of adversary-amplified replay of up to 4096 candidates.
pub(crate) fn apply_account_authority_projection(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_auth_state(
             account_id        BLOB PRIMARY KEY,
             classification    TEXT NOT NULL,
             contested_depth   INTEGER,
             successor_account_id BLOB,
             effective_count   INTEGER NOT NULL
         ) STRICT;

         CREATE TABLE IF NOT EXISTS account_roster_history(
             roster_ref         BLOB PRIMARY KEY,
             account_id         BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             role               TEXT NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_roster_history_account
             ON account_roster_history(account_id, device_fingerprint);

         CREATE TABLE IF NOT EXISTS account_owner_incarnations(
             owner_id           BLOB PRIMARY KEY,
             account_id         BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_owner_incarnations_account
             ON account_owner_incarnations(account_id, device_fingerprint);

         CREATE TABLE IF NOT EXISTS account_stream_ownership(
             stream_id      BLOB PRIMARY KEY,
             account_id     BLOB NOT NULL,
             own_id         BLOB NOT NULL,
             effective_at   INTEGER NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_stream_ownership_account
             ON account_stream_ownership(account_id);

         CREATE TABLE IF NOT EXISTS account_stream_grants(
             grant_id           BLOB PRIMARY KEY,
             owner_account_id   BLOB NOT NULL,
             stream_id          BLOB NOT NULL,
             grantee_account_id BLOB NOT NULL,
             role               TEXT NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_stream_grants_owner
             ON account_stream_grants(owner_account_id, stream_id, grantee_account_id);

         CREATE TABLE IF NOT EXISTS account_stream_grant_cuts(
             grant_id           BLOB NOT NULL,
             owner_account_id   BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             -- Fixed-width big-endian bytes preserve the full protocol u64 domain and sort in
             -- unsigned numeric order; SQLite INTEGER is signed and would reject high cuts.
             seq                BLOB NOT NULL CHECK(length(seq) = 8),
             entry_hash         BLOB NOT NULL,
             PRIMARY KEY(grant_id, device_fingerprint)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_stream_grant_cuts_owner
             ON account_stream_grant_cuts(owner_account_id, grant_id);",
    )?;
    // V064's transactional backfill immediately calls the current projection writer. Provision
    // the additive boundary shape here too, so a database upgrading from V063 can be backfilled by
    // this binary; V065 repeats it idempotently for databases that already recorded old V064.
    apply_account_authority_boundaries(conn)
}

/// V065: retain the exact chain boundaries of closed roster and owner citations. The V064 rows
/// remain historical facts; these columns make their valid prefix explicit instead of forcing a
/// caller to choose between accepting a revoked citation and rejecting valid late delivery.
pub(crate) fn apply_account_authority_boundaries(conn: &Connection) -> rusqlite::Result<()> {
    for (table, prefix) in [
        ("account_roster_history", "control"),
        ("account_roster_history", "secrets"),
        ("account_owner_incarnations", "control"),
        ("account_owner_incarnations", "secrets"),
    ] {
        add_column_if_missing(
            conn,
            table,
            &format!("{prefix}_boundary"),
            "TEXT NOT NULL DEFAULT 'open'",
        )?;
        add_column_if_missing(conn, table, &format!("{prefix}_seq"), "BLOB")?;
        add_column_if_missing(conn, table, &format!("{prefix}_hash"), "BLOB")?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_roster_content_boundaries(
             roster_ref BLOB NOT NULL,
             account_id BLOB NOT NULL,
             stream_id  BLOB NOT NULL,
             seq        BLOB NOT NULL CHECK(length(seq) = 8),
             entry_hash BLOB NOT NULL CHECK(length(entry_hash) = 32),
             PRIMARY KEY(roster_ref, stream_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_roster_content_boundaries_account
             ON account_roster_content_boundaries(account_id, roster_ref);",
    )
}

/// V055 (#492): the anchor-status downgrade hysteresis marker. INVARIANT: NULL means "no gone
/// observation is pending" — every persisted non-deferred stamp clears it, so a non-null marker
/// only ever bridges two CONSECUTIVE gone observations of the same binding. Nullable and
/// additive: existing rows start unarmed, and the pre-V055 behavior (immediate downgrade) simply
/// becomes the two-pass rule from the next validate on.
pub(crate) fn apply_binding_downgrade_marker(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "repo_memory_bindings", "downgrade_pending_at_ms", "INTEGER")?;
    Ok(())
}

/// V056 (#566): the windowed file-pair change-coupling table, derived from `git_file_changes` over
/// a bounded recency window of eligible commits. INVARIANTS:
///  * PURE GIT-HISTORY table: the stored rows are a function of `(git-history window, params)` ONLY
///    — no `files`-view dependence (generation / generated flag / worktree scope). This makes the
///    freshness stamp complete by construction; the generated / existence filter is a READ-time
///    concern (a generated or absent-at-HEAD partner is stored but not surfaced).
///  * `path_a < path_b` (BINARY) — exactly ONE symmetric row per unordered pair; the two
///    directional confidences (`P(B|A)` / `P(A|B)`) are derived at READ time from the endpoint
///    counts.
///  * DerivedIndex posture: rows are wholesale `DELETE` + `INSERT`ed per recompute inside one
///    transaction, stamped via `repo_meta` `'git_coupling_stamp'` (=
///    `history_freshness_key:params`). Rows are never patched incrementally; a stale/absent stamp
///    means "recompute or treat as absent", never "trust the rows". No FK to `git_file_changes` /
///    `git_commits`: the row aggregates over commits and must survive a history full-replace
///    between recompute passes (the stamp, not row integrity, is the freshness authority).
///  * WRITE-time storage floors (both pure git-history): `co_change_count >= MIN_COUPLING_SUPPORT`
///    AND `lift = co * N / (a_count * b_count) >= MIN_COUPLING_LIFT`. The lift floor bounds the
///    table without a per-file cap — a hub file scores `lift ~= 1` with everything and is dropped
///    here.
///  * Direct `repo_id` scope (V040): every reader joins AND filters on `repo_id` — a fork sharing
///    commit hashes must never surface a sibling repo's couplings. The PK covers `(repo_id,
///    path_a)` lookups; the secondary index covers `(repo_id, path_b)`, so one `OR` query serves
///    both directions of the symmetric row.
///
/// Additive + idempotent (`CREATE ... IF NOT EXISTS`); a fresh DB creates it empty and the first
/// git-inclusive `impact_surface` read fills it.
pub(crate) fn apply_git_change_couplings(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS git_change_couplings(
            repo_id TEXT NOT NULL,
            path_a TEXT NOT NULL,
            path_b TEXT NOT NULL,
            co_change_count INTEGER NOT NULL,
            path_a_change_count INTEGER NOT NULL,
            path_b_change_count INTEGER NOT NULL,
            window_commit_count INTEGER NOT NULL,
            last_co_change_at_s INTEGER NOT NULL,
            computed_at_ms INTEGER NOT NULL,
            PRIMARY KEY(repo_id, path_a, path_b)
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_git_change_couplings_b
            ON git_change_couplings(repo_id, path_b);
        ",
    )
}

pub(crate) fn apply_symbols_is_test(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "symbols", "is_test", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

/// V036 (#357): content-address embeddings so they SURVIVE reindex. `chunk_embeddings` is keyed by
/// `chunk_id` with `ON DELETE CASCADE`, so every reindex / branch-switch deletes a chunk and its
/// embedding — even when the content is unchanged — forcing a re-embed. `embedding_cache` keys the
/// vector by `input_hash` alone (which already folds model id + model version + the exact embedding
/// input text), so it is context-INDEPENDENT: reconcile reuses a vector for identical content
/// across reindexes, branches, and worktrees instead of paying the embedder. Seeded from the
/// current embeddings so existing vectors are preserved through the first post-migration reindex.
/// Idempotent (`CREATE TABLE IF NOT EXISTS` + `INSERT OR IGNORE`); seeding an empty table is a
/// no-op on a fresh DB.
pub(crate) fn apply_embedding_content_cache(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS embedding_cache(
             input_hash TEXT NOT NULL PRIMARY KEY,
             model_id TEXT NOT NULL,
             embedding_dim INTEGER NOT NULL,
             vector_blob BLOB NOT NULL,
             computed_at_ms INTEGER NOT NULL,
             last_used_at_ms INTEGER NOT NULL
         ) STRICT;
         -- Preserve existing vectors: seed the cache from the current embeddings by content so the
         -- first reindex after this migration reuses instead of re-embedding.
         INSERT OR IGNORE INTO embedding_cache(
             input_hash, model_id, embedding_dim, vector_blob, computed_at_ms, last_used_at_ms
         )
         SELECT input_hash, model_id, embedding_dim, vector_blob,
                COALESCE(computed_at_ms, created_at_ms, 0), COALESCE(computed_at_ms, \
         created_at_ms, 0)
         FROM chunk_embeddings
         WHERE status = 'Current' AND input_hash != '' AND length(vector_blob) > 0;",
    )?;
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

/// Whether `files` already carries a UNIQUE index whose columns include `commit_sha` — i.e. it is
/// already commit-addressable (the V008 `UNIQUE(path, commit_sha, worktree_id)` or the V040
/// `UNIQUE(repo_id, path, commit_sha, worktree_id)` that supersedes it). Used to make the V008
/// files rebuild idempotent + non-clobbering on `apply`'s full re-run.
fn files_has_commit_scoped_unique(conn: &Connection) -> rusqlite::Result<bool> {
    let unique_indexes: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA index_list(files)")?;
        let rows = stmt.query_map([], |row| {
            // index_list columns: (seq, name, unique, origin, partial).
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })?;
        rows.filter_map(|row| match row {
            Ok((name, true)) => Some(Ok(name)),
            Ok((_, false)) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<rusqlite::Result<_>>()?
    };
    for index in unique_indexes {
        let mut stmt = conn.prepare(&format!("PRAGMA index_info({index})"))?;
        // index_info columns: (seqno, cid, name).
        let mut cols = stmt.query_map([], |row| row.get::<_, Option<String>>(2))?;
        if cols.any(|col| matches!(col, Ok(Some(name)) if name == "commit_sha")) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn rebuild_files_table_for_commit_scopes(conn: &Connection) -> rusqlite::Result<()> {
    // IDEMPOTENCE / NON-CLOBBER (load-bearing since V040): this V008 rebuild recreates `files` with
    // the columns it knew at V008 — it has NO `repo_id` column. `apply` re-runs EVERY migration
    // (the `create_or_migrate`/`rebuild` path), so on an already-V040 DB an unconditional rebuild
    // here would DROP the `repo_id` column (and its real values) BEFORE V040 re-adds it as the
    // placeholder — and a case-1 `register_repo` (already adopted) would not re-backfill it,
    // leaving every file row stranded under `__unassigned__`. The rebuild's ONLY job beyond the
    // additive columns (already added by `apply_commit_addressable_worktrees`) is the
    // commit-scoped UNIQUE; once `files` already carries a UNIQUE that includes `commit_sha`
    // (this V008 one, or the V040 `(repo_id, path, commit_sha, worktree_id)` that supersedes
    // it), the rebuild is redundant.
    if files_has_commit_scoped_unique(conn)? {
        return Ok(());
    }
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

#[cfg(test)]
mod memory_model_failure_migration_tests {
    use super::*;

    #[test]
    fn rolls_back_when_reason_index_is_blocked() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE blocker(x TEXT);
            CREATE INDEX idx_memory_model_failures_reason ON blocker(x);
            ",
        )
        .unwrap();

        let err = apply_memory_model_failures_table(&conn)
            .expect_err("conflicting index name must make the migration fail");

        assert!(
            err.to_string().contains("idx_memory_model_failures_reason"),
            "the failure should come from the blocked index name, got {err}"
        );
        assert!(
            !table_exists(&conn, "memory_model_failures").unwrap(),
            "CREATE TABLE is inside the transaction and rolls back with the failed index"
        );
        let blocker_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM blocker", [], |r| r.get(0)).unwrap();
        assert_eq!(blocker_rows, 0, "the preexisting blocker table/index is left intact");
    }
}
