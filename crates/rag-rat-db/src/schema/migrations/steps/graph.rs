use rusqlite::{Connection, OptionalExtension};

use crate::schema::migrations::{add_column_if_missing, column_exists, sqlite_object_exists};

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

/// V135 (#1466): the local variables a file declares. A function-local variable binding is not a
/// symbol, but edge resolution still counts each one as a candidate for its name that is never
/// bound: a name that is ambiguous only because of a local stays unresolved instead of binding a
/// same-named symbol elsewhere in the repository, and a reference the local would win stays
/// unresolved instead of falling through to one. Rewritten with the file's symbols on every
/// reindex. `scope_path` is the path the binding would carry as a symbol; the qualified name and
/// language are the file's. Purely additive; the logical-key heal re-extracts the languages with
/// local variables, which fills it for files indexed before it.
///
/// `edges_data.local_binding_file_id` records, on a reference a local variable wins, the file that
/// declares that local. No in-edge leads from that file to the reference, so a scoped incremental
/// pass that rewrites the file stages the reference's source file through this column instead. NULL
/// on every other row; the partial index covers only the stamped rows.
pub fn apply_local_bindings(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges_data", "local_binding_file_id", "INTEGER")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS local_bindings(
            file_id INTEGER NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            scope_path TEXT NOT NULL,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            signature TEXT,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_local_bindings_file ON local_bindings(file_id);
        CREATE INDEX IF NOT EXISTS idx_local_bindings_name ON local_bindings(name);
        CREATE INDEX IF NOT EXISTS idx_edges_data_local_binding_file
            ON edges_data(local_binding_file_id) WHERE local_binding_file_id IS NOT NULL;
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
    // exact, row-id-independent identity (path+lines+names+kind+target+resolved callee); the looser
    // columns let validation re-find an edge that moved lines only when its stable callee identity
    // still agrees. `callee_identity_known` distinguishes a new unresolved edge (known NULL) from
    // a pre-V099 row that never recorded callee identity. One row per edge, ordered by `ordinal`.
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
            callee_logical_symbol_id INTEGER,
            callee_identity_known INTEGER NOT NULL DEFAULT 0,
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

pub fn apply_oracle_tables(conn: &Connection) -> rusqlite::Result<()> {
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

pub fn apply_scip_moniker_anchors(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_external_symbols(conn: &Connection) -> rusqlite::Result<()> {
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
        CREATE INDEX IF NOT EXISTS idx_edges_target_qname ON edges_data(target_qualified_name_id);
        ",
    )
}

/// V071 (#682): index the edge-side interned target-qualified-name id. The graph-traversal seed
/// predicate behind `find_callers` / `trace_callees` matches unresolved edges by
/// `edges.target_qualified_name_id = (SELECT id FROM name_strings WHERE value = ?)`; without an
/// index on that column the whole seed OR degrades to a full scan of `edges_data` (the other seed
/// branches are on the already-indexed `to_symbol_id` / `from_symbol_id` / `from_name_id`). This
/// index lets the planner drive a MULTI-INDEX OR instead. Purely additive and idempotent
/// (`CREATE INDEX IF NOT EXISTS`); a fresh DB gets it from `ensure_edges_data_indexes`, an existing
/// DB from this forward migration.
pub fn apply_edge_target_qname_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_edges_target_qname ON \
         edges_data(target_qualified_name_id);",
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
/// Recreate the `edges` compatibility view after a migration changes which persisted candidates
/// are public graph edges. `ensure_edges_view` is idempotent (DROP + CREATE), and the underlying
/// `edges_data` rows remain available to internal indexing passes.
pub(crate) fn apply_edges_view_refresh(conn: &Connection) -> rusqlite::Result<()> {
    ensure_edges_view(conn)
}

/// V024 (#77): add `files.has_test_code` and backfill it from existing chunk text. Additive +
/// idempotent — `add_column_if_missing` guards the ADD (a fresh DB already has the column from the
/// baseline), and the backfill recomputes from the SAME markers `impact_surface` previously scanned
/// for, so a forward-migrated index matches a freshly-indexed one immediately (no wait for
/// reindex). Invariant: the marker set here MUST stay in sync with `index::text_has_test_marker`
/// (the index-time compute) and `FileSection::Tests`'s filter, or migrated vs reindexed rows would
/// diverge. Uses `instr` (case-sensitive, literal substring), NOT `LIKE` — SQLite `LIKE` is
/// case-insensitive for ASCII, so it would match an uppercase `TEST(` that the case-sensitive
/// `str::contains` at index time does not, diverging a forward-migrated row from a freshly-indexed
/// one. (`instr` also needs no `%`-escaping of the `[`/`(` in the markers.)
pub fn apply_files_has_test_code(conn: &Connection) -> rusqlite::Result<()> {
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

/// V074: re-install the `edges` compatibility view so the V068 suppressed-edge exclusion is the
/// scalar compare `ensure_edges_view` now writes, not the original per-row `NOT IN (SELECT ...)`
/// probe (the query_warm regression). A DB already at the schema tip opens as `Compatible` and
/// never re-runs the view bootstrap, so without this ladder step only freshly migrated indexes
/// would pick up the cheap form.
pub(crate) fn apply_edges_view_scalar_suppression(conn: &Connection) -> rusqlite::Result<()> {
    ensure_edges_view(conn)
}

/// V075: materialize edge visibility as `edges_data.hidden` and filter the `edges` view on it.
/// V074's scalar compare removed the per-row membership probe but still charged every view row a
/// resolution-id comparison — measurable on the per-hit graph-evidence queries because they run
/// per search hit. A stamped flag moves the classification to write time (each row's visibility
/// is decided once, by the writer that knows it) and leaves the read side a single integer
/// compare. The backfill mirrors the predicate the view WHERE used to evaluate: dispatch FACT
/// kinds (#200) and suppressed unresolved candidates (V068). Idempotent — the ADD is guarded, the
/// UPDATE only ever promotes `hidden = 0` rows the predicate says are invisible, and the view
/// refresh is DROP + CREATE.
pub(crate) fn apply_edges_hidden_flag(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges_data", "hidden", "INTEGER NOT NULL DEFAULT 0")?;
    conn.execute_batch(
        "UPDATE edges_data SET hidden = 1
         WHERE hidden = 0
           AND (edge_kind_id IN (SELECT id FROM name_strings
                                 WHERE value IN ('dispatch_construct', 'dispatch_handle'))
                OR resolution_id IN (SELECT id FROM name_strings WHERE value = 'suppressed'));",
    )?;
    ensure_edges_view(conn)
}

pub(crate) fn ensure_edges_view(conn: &Connection) -> rusqlite::Result<()> {
    if sqlite_object_exists(conn, "table", "edges")? {
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
    add_column_if_missing(conn, "edges_data", "receiver_type_hint_id", "INTEGER")?;
    // Same guarantee for the materialized visibility flag (V075): the view WHERE below references
    // `d.hidden`, and this function runs at V020 — before V075 adds the column in the linear
    // ladder. The V075 backfill then hides any pre-existing dispatch-fact/suppressed rows.
    add_column_if_missing(conn, "edges_data", "hidden", "INTEGER NOT NULL DEFAULT 0")?;
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
               rth.value AS receiver_type_hint,
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
               d.receiver_type_hint_id,
               d.edge_kind_id,
               d.confidence_id,
               d.resolution_id
        FROM edges_data d
        LEFT JOIN name_strings fn ON fn.id = d.from_name_id
        LEFT JOIN name_strings tn ON tn.id = d.to_name_id
        LEFT JOIN name_strings tqn ON tqn.id = d.target_qualified_name_id
        LEFT JOIN name_strings rh ON rh.id = d.receiver_hint_id
        LEFT JOIN name_strings rth ON rth.id = d.receiver_type_hint_id
        LEFT JOIN name_strings res ON res.id = d.resolution_id
        LEFT JOIN name_strings ek ON ek.id = d.edge_kind_id
        LEFT JOIN name_strings conf ON conf.id = d.confidence_id
        -- Visibility is MATERIALIZED (#734): the writers stamp `hidden = 1` on every row that is
        -- not a public graph edge — the internal dispatch FACT kinds (#200: inputs to
        -- `synthesize_dispatch_edges`, where the handle fact duplicates the dispatcher's existing
        -- `calls_name`) and the suppressed unresolved candidates (V068). Filtering here keeps
        -- EVERY query-layer reader (graph traversal, repo_brief, clusters, grep-augment,
        -- orientation, …) structurally safe without each remembering an exclusion; the
        -- synthesized `dispatches` edge (a real edge) stays visible, and resolution + synthesis
        -- read `edges_data` directly so they still see the facts. A single integer compare per
        -- row is the point: evaluating the kind/resolution predicates inline — even as scalar
        -- subselects — taxed every per-hit graph-evidence query (the query_warm regression).
        WHERE d.hidden = 0;

        -- Interning per column: `INSERT OR IGNORE` + `value NOT NULL` means a NULL string is
        -- silently skipped and its id subselect yields NULL — exactly the legacy nullability.
        -- COALESCE mirrors the legacy table's column DEFAULTs for inserts that omit them.
        CREATE TRIGGER IF NOT EXISTS edges_view_insert INSTEAD OF INSERT ON edges BEGIN
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.from_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.to_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.target_qualified_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.receiver_hint);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.receiver_type_hint);
            INSERT OR IGNORE INTO name_strings(value)
                VALUES (COALESCE(NEW.resolution, 'unresolved'));
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.edge_kind);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.confidence);
            INSERT INTO edges_data(
                id, source_file_id, from_symbol_id, to_symbol_id, from_name_id, to_name_id,
                source_start_line, source_end_line, source_start_byte, source_end_byte,
                target_start_line, target_end_line, target_qualified_name_id, evidence,
                receiver_hint_id, receiver_type_hint_id, resolution_id,
                callee_start_byte, callee_end_byte,
                import_scope_start_byte, import_scope_end_byte, import_mod_id,
                edge_kind_id, confidence_id, hidden
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
                (SELECT id FROM name_strings WHERE value = NEW.receiver_type_hint),
                (SELECT id FROM name_strings
                 WHERE value = COALESCE(NEW.resolution, 'unresolved')),
                NEW.callee_start_byte, NEW.callee_end_byte,
                NEW.import_scope_start_byte, NEW.import_scope_end_byte, NEW.import_mod_id,
                (SELECT id FROM name_strings WHERE value = NEW.edge_kind),
                (SELECT id FROM name_strings WHERE value = NEW.confidence),
                -- The same visibility predicate the direct writers stamp (see the view WHERE).
                CASE WHEN NEW.edge_kind IN ('dispatch_construct', 'dispatch_handle')
                          OR COALESCE(NEW.resolution, 'unresolved') = 'suppressed'
                     THEN 1 ELSE 0 END
            );
        END;

        CREATE TRIGGER IF NOT EXISTS edges_view_update INSTEAD OF UPDATE ON edges BEGIN
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.from_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.to_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.target_qualified_name);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.receiver_hint);
            INSERT OR IGNORE INTO name_strings(value) VALUES (NEW.receiver_type_hint);
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
                receiver_type_hint_id =
                    (SELECT id FROM name_strings WHERE value = NEW.receiver_type_hint),
                resolution_id = (SELECT id FROM name_strings WHERE value = NEW.resolution),
                callee_start_byte = NEW.callee_start_byte,
                callee_end_byte = NEW.callee_end_byte,
                import_scope_start_byte = NEW.import_scope_start_byte,
                import_scope_end_byte = NEW.import_scope_end_byte,
                import_mod_id = NEW.import_mod_id,
                edge_kind_id = (SELECT id FROM name_strings WHERE value = NEW.edge_kind),
                confidence_id = (SELECT id FROM name_strings WHERE value = NEW.confidence),
                -- Recompute visibility from the updated kind/resolution (see the view WHERE).
                hidden = CASE WHEN NEW.edge_kind IN ('dispatch_construct', 'dispatch_handle')
                                   OR NEW.resolution = 'suppressed'
                              THEN 1 ELSE 0 END
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
pub fn apply_edge_string_interning(conn: &Connection) -> rusqlite::Result<()> {
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
            let has_oracle = sqlite_object_exists(conn, "table", "edge_oracle")?;
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

/// Recompute `logical_symbols.group_reason` from what the members actually show (#855).
///
/// The previous writer labelled EVERY multi-member group `cfg_variant`. On a representative index
/// that was wrong for 3,686 of 3,699 such groups: a source path carries one `files` row per index
/// scope (worktree-overlay and commit scopes), so a symbol defined once shows up once per scope and
/// gets grouped. Callers were told a symbol with a single definition had N cfg variants.
///
/// This runs as a migration rather than waiting for the next `rebuild_logical_symbols` because the
/// column is derived but PERSISTED: a query-only server over an unchanged repository never
/// rebuilds, and would keep serving the old label indefinitely.
///
/// The three outcomes match [`logical_group_reason`](../../../rag-rat-core) exactly — one member is
/// `single`; members spread one-per-`files`-row are a `scope_replica`; any `files` row holding two
/// or more members makes the group `same_file_multi`. A group whose members have all been deleted
/// falls back to `single` so the NOT NULL column always gets a value.
pub(crate) fn apply_logical_group_reason_by_evidence(conn: &Connection) -> rusqlite::Result<()> {
    // Both tables are in the baseline, so no existence guard is needed.
    conn.execute_batch(
        "
        UPDATE logical_symbols SET group_reason = COALESCE((
            SELECT CASE
                     WHEN SUM(per_file.members) <= 1 THEN 'single'
                     WHEN MAX(per_file.members) > 1 THEN 'same_file_multi'
                     ELSE 'scope_replica'
                   END
            FROM (
                SELECT COUNT(*) AS members
                FROM logical_symbol_members
                JOIN symbols ON symbols.id = logical_symbol_members.symbol_id
                WHERE logical_symbol_members.logical_symbol_id = logical_symbols.id
                GROUP BY symbols.file_id
            ) AS per_file
        ), 'single');
        ",
    )
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
    crate::chunk_text_store::build_store(conn, "(SELECT id AS chunk_id, text FROM chunks)")
        .map_err(|err| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!("V027 chunk_text backfill failed before dropping chunks.text: {err}")),
            )
        })?;
    conn.execute("ALTER TABLE chunks DROP COLUMN text", [])?;
    Ok(())
}
