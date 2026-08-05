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
/// (the index-time compute) and `test_items`'s filter, or migrated vs reindexed rows would diverge.
/// Uses `instr` (case-sensitive, literal substring), NOT `LIKE` — SQLite `LIKE` is case-insensitive
/// for ASCII, so it would match an uppercase `TEST(` that the case-sensitive `str::contains` at
/// index time does not, diverging a forward-migrated row from a freshly-indexed one. (`instr` also
/// needs no `%`-escaping of the `[`/`(` in the markers.)
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

pub(crate) fn known_version(migrations: &[AppliedMigration]) -> u32 {
    migrations.iter().filter_map(|migration| shipped_version(&migration.id)).max().unwrap_or(0)
}

pub(crate) fn known_migration(id: &str) -> bool {
    shipped_version(id).is_some() || id == DIRTY_MIGRATION_ID
}

pub(crate) fn migration_checksum_mismatch(migration: &AppliedMigration) -> bool {
    shipped_checksum(&migration.id).is_some_and(|checksum| migration.checksum != checksum)
}

/// The ladder position a shipped migration id maps to: the baseline (001) is 1, and each
/// [`ADDITIVE_MIGRATIONS`] entry is its 1-based position after it. `None` for a row written by a
/// future binary (or the dirty marker, which has no version).
fn shipped_version(id: &str) -> Option<u32> {
    if id == MIGRATION_001_ID {
        return Some(1);
    }
    ADDITIVE_MIGRATIONS
        .iter()
        .position(|step| step.id == id)
        .and_then(|index| u32::try_from(index + 2).ok())
}

fn shipped_checksum(id: &str) -> Option<&'static str> {
    if id == MIGRATION_001_ID {
        return Some(MIGRATION_001_CHECKSUM);
    }
    ADDITIVE_MIGRATIONS.iter().find(|step| step.id == id).map(|step| step.checksum)
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
        params![id, rag_rat_base::time::now_ms(), checksum, description],
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
            rag_rat_base::version::binary_version(),
            MIGRATION_PROVENANCE_KEYS[1],
            exe,
            MIGRATION_PROVENANCE_KEYS[2],
            LATEST_SCHEMA_VERSION.to_string(),
            MIGRATION_PROVENANCE_KEYS[3],
            rag_rat_base::time::now_ms().to_string(),
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

pub fn table_exists(conn: &Connection, table: &str) -> anyhow::Result<bool> {
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

pub fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
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
pub fn apply_clone_fingerprint_tables(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_clone_graph_tables(conn: &Connection) -> rusqlite::Result<()> {
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
/// [`rag_rat_base::repo_identity::LEGACY_REPO_ID`]) marks a legacy single-repo DB awaiting
/// adoption: the first post-migration open calls [`super::register_repo`], which rewrites the
/// placeholder to the real content-derived `repo_id` in one step. A consolidated DB holding more
/// than one repo never carries the placeholder — `register_repo` refuses to adopt when a different
/// real id already owns the DB.
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
pub fn apply_repos_registry(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(REPOS_REGISTRY_DDL)
}

/// V038 DDL. The placeholder literal `'__unassigned__'` MUST equal
/// [`rag_rat_base::repo_identity::LEGACY_REPO_ID`] — `super::register_repo` reads that constant
/// when it adopts the row (a matching bootstrap test pins the two together).
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
/// [`rag_rat_base::repo_identity::LEGACY_REPO_ID`] placeholder V038 seeds. Targeting the sole row
/// (not a hardcoded placeholder) is what keeps the `repo_meta → repos` FK satisfied on an ADOPTED
/// DB, where the placeholder row is gone. The read/write call sites move to the `repo_meta`
/// accessors in the same change, so a moved key is never read from the table it was deleted from.
///
/// Idempotent (`INSERT OR IGNORE` deduped by the `(repo_id, key)` PK + `DELETE` of the source
/// rows): a fresh DB has empty meta tables → the copy/delete are no-ops, and a forward-migrated
/// legacy DB converges on the identical shape. Copy-before-delete on each table means even a torn
/// run (crash between the copy and the delete) re-converges: the re-run's copy is ignored and the
/// delete finishes, and readers already read `repo_meta` (the authoritative side).
pub fn apply_move_per_repo_meta(conn: &Connection) -> rusqlite::Result<()> {
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
/// adopted via [`super::register_repo`], else the [`rag_rat_base::repo_identity::LEGACY_REPO_ID`]
/// placeholder V038 seeds.
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
/// [`rag_rat_base::repo_identity::LEGACY_REPO_ID`] placeholder: existing rows backfill to the
/// placeholder, which `register_repo` rewrites to the real id at adoption (the A1/A2 pattern), and
/// any writer that forgets to stamp still produces a scannable single-repo value rather than NULL.
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
pub fn apply_repo_id_core_scoping(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
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
        (hooks.realign_logical_symbol_ids)(conn)?;
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
    if target == rag_rat_base::repo_identity::LEGACY_REPO_ID {
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
            rag_rat_base::repo_identity::LEGACY_REPO_ID,
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
pub fn apply_files_generation(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_github_repo_id_scoping(conn: &Connection) -> rusqlite::Result<()> {
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
    if target == rag_rat_base::repo_identity::LEGACY_REPO_ID {
        return Ok(());
    }
    for table in V041_GITHUB_SCOPED_TABLES {
        conn.execute(&format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"), [
            target.as_str(),
            rag_rat_base::repo_identity::LEGACY_REPO_ID,
        ])?;
    }
    // The own-content FTS mirror carries its own `repo_id UNINDEXED` value; re-point it in place
    // too.
    conn.execute("UPDATE github_fts SET repo_id = ?1 WHERE repo_id = ?2", [
        target.as_str(),
        rag_rat_base::repo_identity::LEGACY_REPO_ID,
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
pub fn apply_github_child_key_widening(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_memory_verification_tables(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_memory_model_failures_table(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_papertrail_provider_neutral_schema(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
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
            (hooks.rebuild_papertrail_fts)(conn)?;
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
pub fn apply_papertrail_mirror_resume_state(conn: &Connection) -> rusqlite::Result<()> {
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

/// V067 (#592): scheduling and failures are binding-local. Error classes are stable machine
/// values; detail is sanitized and bounded by the recording API rather than used for policy.
pub fn apply_papertrail_binding_health(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_sync_cursor", "last_attempt_ms", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "last_successful_probe_ms", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "last_successful_mirror_ms", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "retry_not_before_ms", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "error_class", "TEXT")?;
    add_column_if_missing(conn, "papertrail_sync_cursor", "error_detail", "TEXT")?;
    conn.execute(
        "UPDATE papertrail_sync_cursor
         SET last_successful_probe_ms=last_probe_ms
         WHERE last_successful_probe_ms IS NULL AND last_probe_ms IS NOT NULL",
        [],
    )?;
    // Before V066, an ordinary initial backfill was a complete project walk but only forced
    // `--full` runs populated `last_full_sync_ms`. Preserve that completed-walk fact using the
    // cursor's last successful provider contact; incomplete cursors must remain due for healing.
    conn.execute(
        "UPDATE papertrail_sync_cursor
         SET last_full_sync_ms=last_probe_ms
         WHERE backfill_done=1 AND last_full_sync_ms IS NULL AND last_probe_ms IS NOT NULL",
        [],
    )?;
    conn.execute(
        "UPDATE papertrail_sync_cursor
         SET last_successful_mirror_ms=last_probe_ms
         WHERE backfill_done=1
           AND last_successful_mirror_ms IS NULL
           AND last_probe_ms IS NOT NULL",
        [],
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
pub fn apply_repo_id_periphery_scoping(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
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
        (hooks.rederive_dream_finding_ids)(conn)?;
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
    if target == rag_rat_base::repo_identity::LEGACY_REPO_ID {
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
            rag_rat_base::repo_identity::LEGACY_REPO_ID,
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
pub fn rebuild_repo_memory_fts_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_clone_delta_maintenance(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_clone_df_epoch(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_oplog_storage(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_oplog_stream_scoping(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_oplog_device_identity(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_oplog_device_x25519(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_account_candidate_dag(conn: &Connection) -> rusqlite::Result<()> {
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

/// V066 (sync phase C2, §16): the owner-bound `/3` content candidate DAG.
///
/// Full-width unsigned wire counters are fixed-width big-endian blobs. SQLite INTEGER is signed
/// i64 and would reject or truncate valid `u64` sequence/authentication counters. Lexicographic
/// ordering of equal-width big-endian blobs is the unsigned numeric ordering required by dense
/// chain queries.
pub fn apply_content_candidate_dag(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_entries(
             entry_hash          BLOB    PRIMARY KEY CHECK(length(entry_hash) = 32),
             stream_id           BLOB    NOT NULL CHECK(length(stream_id) = 32),
             author_account_id   BLOB    NOT NULL CHECK(length(author_account_id) = 32),
             device_fingerprint  BLOB    NOT NULL CHECK(length(device_fingerprint) = 32),
             seq                 BLOB    NOT NULL CHECK(length(seq) = 8),
             prev_hash           BLOB    CHECK(prev_hash IS NULL OR length(prev_hash) = 32),
             grant_id            BLOB    CHECK(grant_id IS NULL OR length(grant_id) = 32),
             roster_ref          BLOB    NOT NULL CHECK(length(roster_ref) = 32),
             owner_auth_len      BLOB    NOT NULL CHECK(length(owner_auth_len) = 8),
             author_auth_len     BLOB    NOT NULL CHECK(length(author_auth_len) = 8),
             accepted            INTEGER NOT NULL DEFAULT 0 CHECK(accepted IN (0, 1)),
             signed_bytes        BLOB    NOT NULL,
             received_at_ms      INTEGER NOT NULL
         ) STRICT;

         -- Candidate history is grow-only: equivocations share a coordinate and remain distinct.
         CREATE INDEX IF NOT EXISTS content_entries_chain
             ON content_entries(stream_id, author_account_id, device_fingerprint, seq);
         CREATE INDEX IF NOT EXISTS content_entries_predecessor
             ON content_entries(prev_hash, stream_id, author_account_id, device_fingerprint);

         -- C2 never sets accepted=1. C3 owns the atomic authority+branch refold that activates it.
         CREATE UNIQUE INDEX IF NOT EXISTS content_accepted_slot
             ON content_entries(stream_id, author_account_id, device_fingerprint, seq)
             WHERE accepted = 1;

         CREATE TABLE IF NOT EXISTS content_entry_status(
             entry_hash BLOB PRIMARY KEY CHECK(length(entry_hash) = 32),
             status     TEXT NOT NULL,
             detail     TEXT
         ) STRICT;

         CREATE TABLE IF NOT EXISTS content_pre_verify(
             signed_hash               BLOB    PRIMARY KEY CHECK(length(signed_hash) = 32),
             entry_hash                BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             claimed_stream_id         BLOB    NOT NULL CHECK(length(claimed_stream_id) = 32),
             claimed_author_account_id BLOB    NOT NULL CHECK(length(claimed_author_account_id) = \
         32),
             claimed_fingerprint       BLOB    NOT NULL CHECK(length(claimed_fingerprint) = 32),
             roster_ref                BLOB    NOT NULL CHECK(length(roster_ref) = 32),
             raw_bytes                 BLOB    NOT NULL,
             received_at_ms            INTEGER NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS content_pre_verify_author
             ON content_pre_verify(claimed_author_account_id, roster_ref);",
    )
}

/// V069 (sync phase C3.4a): the store-global local-account pointer. `oplog_local_account` is a
/// single-row (`CHECK (id = 0)`) STRICT table naming the `genesis_entry_hash` of THIS store's one
/// local account — the seq-0, self-authorizing `AccountGenesis` minted once by
/// `rag_rat_oplog::local_account` and reused thereafter, so later C3.4 slices author owner-bound
/// `/3` content under a stable account identity. The pointer is a 32-byte content address into
/// `account_entries`, not the account_id itself: the id is resolved by looking the genesis up in
/// the candidate DAG, so the pointer + genesis stay a single source of truth (one committed
/// atomically with the other by the minting transaction). Store-global like
/// `oplog_device_identity`, not repo-scoped. Purely additive; `CREATE ... IF NOT EXISTS`, so a torn
/// replay reconverges without a wrapping transaction.
pub fn apply_oplog_local_account(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS oplog_local_account(
             id                 INTEGER PRIMARY KEY CHECK (id = 0),
             genesis_entry_hash BLOB NOT NULL CHECK (length(genesis_entry_hash) = 32),
             created_at_ms      INTEGER NOT NULL
         ) STRICT;",
    )
}

/// V070 (sync phase C3.4b-i): the accepted-`/3` → memory projection tables.
/// `content_projected_nodes` / `content_projected_edges` mirror the `/1` shadow tables
/// `oplog_projected_nodes` / `oplog_projected_edges` (stream-keyed since V053) but materialize the
/// acceptance-gated `/3` DAG: `rag_rat_oplog::reproject_accepted_content_stream` decodes each
/// `content_entries` row where `accepted = 1`, folds via the shared memory projector, and rewrites
/// the keyed rows for one `/2` stream. Kept SEPARATE from the `/1` tables on purpose (decision 7):
/// the `/1` projector sweep (`store::reproject_all_streams`) `DELETE`s the `oplog_projected_*`
/// tables wholesale and rebuilds only streams present in `oplog_entries`, so sharing them would let
/// a projector-version bump wipe the `/3` projection and never rebuild it — mass duplicate
/// re-authoring into the immutable `/3` log. These tables are owned by the memory layer and updated
/// only when acceptance changes (the content refold), never by the `/1` sweep. Purely additive;
/// `CREATE ... IF NOT EXISTS`, so a torn replay reconverges without a wrapping transaction; nothing
/// pre-existing to backfill.
pub fn apply_content_projected_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_projected_nodes(
             stream_id    BLOB NOT NULL,
             node_id      TEXT NOT NULL,
             content_json TEXT NOT NULL,
             status       TEXT NOT NULL,
             PRIMARY KEY(stream_id, node_id)
         ) STRICT;

         CREATE TABLE IF NOT EXISTS content_projected_edges(
             stream_id     BLOB NOT NULL,
             edge_key      TEXT NOT NULL,
             spec_json     TEXT NOT NULL,
             resolved_json TEXT,
             PRIMARY KEY(stream_id, edge_key)
         ) STRICT;",
    )
}

/// V072 (issue #652): the deferred-refold work queue for the `/3` content-ingest path.
/// `content_ingest` used to fold acceptance over the whole stream on EVERY ingested entry — an
/// O(n^2) cost as an n-entry stream is built one candidate at a time under the writer lock, which
/// an attacker amplifies by varying cited `auth_len` to defeat the per-refold freshness cache. It
/// now records structural classification and enqueues the stream here instead; the settle seam
/// (`settle_pending_content_refolds`) folds each dirty stream ONCE.
/// INVARIANT: a `stream_id` is present while a refold + reproject is still owed. The row is
/// discharged ONLY by `refold_and_project_stream_in_tx` — reached either from the settle seam or
/// from a TRUSTED/local account fold (`finalize_affected_streams`) — and only after both steps
/// succeed. The untrusted remote account-ingest path never clears it here; it only ADDS debt
/// (`ACCOUNT_CHANGE`) for settle to drain.
/// Purely additive; `CREATE ... IF NOT EXISTS`, so a torn replay reconverges without a wrapping
/// transaction; nothing pre-existing to backfill.
pub fn apply_content_streams_pending_refold(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_streams_pending_refold(
             stream_id BLOB PRIMARY KEY CHECK (length(stream_id) = 32)
         ) STRICT;",
    )
}

/// V082 (#698): reasoned/ordered content-refold work and O(1) per-stream fold-cost accounting.
///
/// `candidate_bytes` matches the payload that a full refold's `load_stream_headers` query copies
/// out of SQLite: `length(signed_bytes) + 32` for the separately materialized `entry_hash` on every
/// row. It deliberately does not guess at allocator/container overhead after decode. The source
/// rows remain authoritative: this migration rebuilds the aggregate once, and database triggers
/// maintain it for every writer thereafter. The update trigger covers direct mutation of
/// `stream_id` or `signed_bytes`; runtime writers currently treat both as immutable.
///
/// The triggers do NOT cover one shape: SQLite performs `INSERT OR REPLACE`'s implicit row deletion
/// WITHOUT firing `AFTER DELETE` triggers unless `PRAGMA recursive_triggers` is on, which this
/// store never sets — so a `REPLACE` into `content_entries` fires the insert trigger only and
/// drifts the aggregate upward permanently. Upward drift is fail-safe for admission (the stream
/// looks expensive and is skipped, never silently unbounded), but it makes the stream invisible to
/// normal-mode settle forever. No writer uses `REPLACE` today and a source tripwire test keeps it
/// that way (`rag-rat-oplog`); `INSERT OR IGNORE` and non-accounting `UPDATE`s are correctly inert.
///
/// Existing V072 queue rows predate enqueue timestamps. Their first/last times derive
/// deterministically from the stream's minimum/maximum candidate `received_at_ms`; an orphan queue
/// row with no remaining candidate receives `0` for both. Timestamp defaults remain zero in this
/// schema-only slice so the existing enqueue helper continues to work until the runtime slice
/// begins stamping and merging queue metadata.
pub fn apply_content_refold_queue_and_stats(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    if !column_exists(&tx, "content_streams_pending_refold", "reason_mask")? {
        tx.execute_batch(
            "DROP TABLE IF EXISTS content_streams_pending_refold_v082;
             CREATE TABLE content_streams_pending_refold_v082(
                 stream_id           BLOB    PRIMARY KEY CHECK(length(stream_id) = 32),
                 reason_mask         INTEGER NOT NULL DEFAULT 1 CHECK(reason_mask BETWEEN 1 AND 3),
                 first_enqueued_at_ms INTEGER NOT NULL DEFAULT 0,
                 last_enqueued_at_ms  INTEGER NOT NULL DEFAULT 0
             ) STRICT;
             INSERT INTO content_streams_pending_refold_v082(
                 stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
             SELECT q.stream_id,
                    1,
                    COALESCE(MIN(e.received_at_ms), 0),
                    COALESCE(MAX(e.received_at_ms), 0)
             FROM content_streams_pending_refold q
             LEFT JOIN content_entries e ON e.stream_id = q.stream_id
             GROUP BY q.stream_id;
             DROP TABLE content_streams_pending_refold;
             ALTER TABLE content_streams_pending_refold_v082
                 RENAME TO content_streams_pending_refold;",
        )?;
    }

    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS content_streams_pending_refold_order
             ON content_streams_pending_refold(first_enqueued_at_ms, stream_id);

         CREATE TABLE IF NOT EXISTS content_stream_stats(
             stream_id       BLOB    PRIMARY KEY CHECK(length(stream_id) = 32),
             candidate_count INTEGER NOT NULL CHECK(candidate_count >= 0),
             candidate_bytes INTEGER NOT NULL CHECK(candidate_bytes >= 0)
         ) STRICT;

         DROP TRIGGER IF EXISTS content_stream_stats_after_insert;
         DROP TRIGGER IF EXISTS content_stream_stats_after_delete;
         DROP TRIGGER IF EXISTS content_stream_stats_after_update;

         DELETE FROM content_stream_stats;
         INSERT INTO content_stream_stats(stream_id, candidate_count, candidate_bytes)
         SELECT stream_id, count(*), sum(length(signed_bytes) + 32)
         FROM content_entries
         GROUP BY stream_id;

         CREATE TRIGGER content_stream_stats_after_insert
         AFTER INSERT ON content_entries
         BEGIN
             INSERT INTO content_stream_stats(stream_id, candidate_count, candidate_bytes)
             VALUES (NEW.stream_id, 1, length(NEW.signed_bytes) + 32)
             ON CONFLICT(stream_id) DO UPDATE SET
                 candidate_count = candidate_count + 1,
                 candidate_bytes = candidate_bytes + length(NEW.signed_bytes) + 32;
         END;

         CREATE TRIGGER content_stream_stats_after_delete
         AFTER DELETE ON content_entries
         BEGIN
             UPDATE content_stream_stats
             SET candidate_count = candidate_count - 1,
                 candidate_bytes = candidate_bytes - length(OLD.signed_bytes) - 32
             WHERE stream_id = OLD.stream_id;
             DELETE FROM content_stream_stats
             WHERE stream_id = OLD.stream_id AND candidate_count = 0;
         END;

         CREATE TRIGGER content_stream_stats_after_update
         AFTER UPDATE OF stream_id, signed_bytes ON content_entries
         BEGIN
             UPDATE content_stream_stats
             SET candidate_count = candidate_count - 1,
                 candidate_bytes = candidate_bytes - length(OLD.signed_bytes) - 32
             WHERE stream_id = OLD.stream_id;
             DELETE FROM content_stream_stats
             WHERE stream_id = OLD.stream_id AND candidate_count = 0;
             INSERT INTO content_stream_stats(stream_id, candidate_count, candidate_bytes)
             VALUES (NEW.stream_id, 1, length(NEW.signed_bytes) + 32)
             ON CONFLICT(stream_id) DO UPDATE SET
                 candidate_count = candidate_count + 1,
                 candidate_bytes = candidate_bytes + length(NEW.signed_bytes) + 32;
         END;",
    )?;

    tx.commit()
}

/// V083 (#855/#860) — persist the direct chunk→symbol link. `chunks.symbol_id` is the rowid of the
/// symbol a code chunk was cut from, stamped at index time by `insert_chunks` from the same parse
/// that assigned the symbol its rowid. It replaces position-based resolution (match by
/// `(path, qualified_name)` then narrow by byte/line geometry), which could not disambiguate
/// same-name symbols that nest or share a physical line — `qualified_name` is `path::simple_name`,
/// not scope-qualified, so overloads and nested same-name functions collide, and no geometric
/// metric attributes a chunk to one of two coincident symbols.
pub fn apply_chunk_symbol_id(conn: &Connection) -> rusqlite::Result<()> {
    // One transaction for the column add + backfill. An established index can hold hundreds of
    // thousands of symbol-bearing chunks; committing each per-row UPDATE in its own autocommit
    // (a WAL fsync apiece) would make the startup migration prohibitively slow. It is also atomic —
    // an interrupted upgrade rolls back to no column and no partial backfill, and replay redoes it
    // from scratch (and is idempotent on any rows a later run finds already linked).
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    add_column_if_missing(&tx, "chunks", "symbol_id", "INTEGER")?;
    backfill_chunk_symbol_ids(&tx)?;
    tx.commit()
}

/// One-time backfill of `chunks.symbol_id` for chunks indexed before V083. Unlike the V079/V081
/// derived stores, chunks are NOT rewritten on a regular cadence — incremental/discover indexing
/// skips UNCHANGED files, so a stable file's chunks would keep NULL `symbol_id` (and lose their
/// drive-by records) indefinitely, not "until the next reindex".
///
/// Resolve each chunk from the identity it ALREADY carries: `symbol_path` is the defining symbol's
/// bare qualified name, except a split continuation which appends `#<n>` (a code path holds at most
/// one `#`, so stripping from the first `#` recovers the bare name; context / whole-file / markdown
/// paths keep theirs and simply match no qualified name). Link a chunk ONLY when exactly one
/// same-file symbol of that qualified name OVERLAPS it in bytes.
///
/// BYTES, not lines: `symbols.start_byte`/`end_byte` are baseline columns, always populated,
/// whereas `start_line`/`end_line` were added later with `DEFAULT 0`, so a never-reindexed legacy
/// symbol can carry `0`/`0` line spans — a line predicate would silently match nothing and strand
/// exactly the rows this repairs. A chunk is cut from the whole lines around its symbol, so it
/// always overlaps that symbol's byte span (part 0 and every continuation part alike).
///
/// This never guesses the cases the direct link exists to resolve: a continuation whose outer is
/// UNIQUELY named links back to that outer (even when a differently-named nested symbol's bytes it
/// falls within also exist), but two same-name symbols that nest or share a line match more than
/// one (`HAVING COUNT(*) = 1` excludes them → NULL), and a non-qualified-name path matches nothing
/// (→ NULL). A NULL chunk surfaces no record; a wrong guess would surface the WRONG one and
/// persist. `rag-rat index` re-stamps every chunk precisely from the parse.
///
/// One set-based statement (no per-row round-trip, no materializing every chunk in memory), run
/// inside the caller's transaction so the whole backfill is a single commit. Idempotent: only
/// chunks still NULL are considered, so a re-run never overwrites an exact index-time id.
fn backfill_chunk_symbol_ids(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        // `base.qname` strips ONLY a trailing `#<digits>` continuation suffix, matching what the
        // chunker appends. Splitting on the FIRST `#` instead would truncate a qualified name
        // whose FILE PATH legitimately contains one (`src/foo#bar.rs::run`), and since
        // unchanged files are never re-chunked those rows would keep a NULL `symbol_id` —
        // and lose their drive-by records — indefinitely. `rtrim` removes trailing digits;
        // the suffix is real only if that shortened the string AND what remains ends in
        // `#`.
        "WITH base AS (
             SELECT c.id AS chunk_id, c.file_id AS file_id,
                    c.start_byte AS start_byte, c.end_byte AS end_byte,
                    CASE
                        WHEN rtrim(c.symbol_path, '0123456789') <> c.symbol_path
                         AND substr(rtrim(c.symbol_path, '0123456789'), -1) = '#'
                        THEN substr(rtrim(c.symbol_path, '0123456789'), 1,
                                    length(rtrim(c.symbol_path, '0123456789')) - 1)
                        ELSE c.symbol_path
                    END AS qname
             FROM chunks c
             WHERE c.symbol_id IS NULL
               AND c.symbol_path IS NOT NULL
         ),
         resolved AS (
             SELECT base.chunk_id AS chunk_id, s.id AS symbol_id
             FROM base
             JOIN symbols s ON s.file_id = base.file_id
             JOIN name_strings ns ON ns.id = s.qualified_name_id
             WHERE ns.value = base.qname
               AND s.start_byte < base.end_byte
               AND base.start_byte < s.end_byte
             GROUP BY base.chunk_id
             HAVING COUNT(*) = 1
         )
         UPDATE chunks
         SET symbol_id = resolved.symbol_id
         FROM resolved
         WHERE chunks.id = resolved.chunk_id;",
    )
}

/// V076 (sync phase C4.3b, #607): the sealing-key adoption audit log. A recipient device records a
/// row here when an accepted `StreamKeyWrap` naming it either fails to unwrap (AEAD tag failure —
/// the primary manifestation of a substituted wrap) or unwraps to a key whose `key_id` disagrees
/// with the op's signed `key_id`.
///
/// INVARIANT: local-only. These rows are never on the wire, never a fold input, and the adoption
/// seam never mutates a fold verdict — the shared fold stays device-independent (convergent), so a
/// recipient-only unwrap check can only ever be LOCAL evidence. `key_epoch` is stored as 8-byte BE
/// (not INT) so a `u64` epoch round-trips without the `i64` narrowing hazard. `UNIQUE(kind,
/// entry_hash)` + `INSERT OR IGNORE` (the write path) keep a hot seal-path retry from re-appending
/// the same evidence for one op. Purely additive; CREATE ... IF NOT EXISTS, nothing to backfill.
pub(crate) fn apply_sync_security_events(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_security_events(
             id              INTEGER PRIMARY KEY,
             kind            TEXT NOT NULL,
             account_id      BLOB NOT NULL,
             stream_id       BLOB NOT NULL,
             key_epoch       BLOB NOT NULL,
             entry_hash      BLOB NOT NULL,
             expected_key_id BLOB,
             observed_key_id BLOB,
             observed_at_ms  INT NOT NULL
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS sync_security_events_dedup
             ON sync_security_events(kind, entry_hash);",
    )
}

/// V064 (sync phase C1, §16): query-ready authority facts derived from the accepted account fold.
/// These are shadow tables, never independent sources of truth: `refold_account` deletes and
/// rewrites every row for one account inside the SAME IMMEDIATE transaction as accepted/status.
/// History intervals (`effective_at`, `closed_at`) preserve audit/projection facts. `auth_len` is
/// only a synchronization assertion: ahead parks for refetch, while behind is informational and
/// never selects historical authority. Exact citations and device cuts make authorization keyed
/// lookups instead of adversary-amplified replay of up to 4096 candidates.
pub fn apply_account_authority_projection(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_account_authority_boundaries(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_git_change_couplings(conn: &Connection) -> rusqlite::Result<()> {
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
pub fn apply_dream_findings(conn: &Connection) -> rusqlite::Result<()> {
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

/// V073 (issue #702): the provider-attested closing-edge substrate for issue distillation.
/// `papertrail_closing_edges` is a FIRST-CLASS issue↔closer edge table, deliberately NOT more
/// `papertrail_refs` rows: refs are an annotation layer whose unique index coalesces on
/// `source_text`, while a closing edge's identity is the (issue, closer) PAIR and its trust
/// semantics differ by `source` — `provider` rows are attested by the tracker's own data
/// (GraphQL closing references, closed-event closers), `text` rows are mined from
/// commit/item/comment text and remain the degradation tier.
/// INVARIANT: `source` is an attribute, not part of the natural key — the same (issue, closer)
/// pair discovered by both tiers converges to ONE row, and a `provider` row is never downgraded
/// back to `text` (the store upsert enforces the precedence).
/// The new `papertrail_items` columns are all fillable from payloads the mirror already parses
/// (zero extra API calls):
///  - `closed_at` — the temporal axis for supersession ordering (`created_at` is NOT it);
///  - `resolution` — the provider-NEUTRAL outcome enum (`completed | not_planned | duplicate |
///    superseded | unknown`); GitHub `state_reason` maps in, Jira's resolution field maps in
///    richer, GitLab is mostly `unknown`;
///  - `merge_commit_sha` — INVARIANT: stored ONLY for merged change requests. GitHub returns a
///    non-null `merge_commit_sha` for closed-UNMERGED PRs too (its ephemeral test-merge commit,
///    possibly on no branch); the store write path must gate on `merged_at`/merged state, never
///    trust the field's presence;
///  - `state_normalized` — `open | closed | merged`. GitLab merged MRs carry `state='merged'`, so a
///    plain `WHERE state='closed'` silently drops every merged GitLab MR; consumers filter on THIS
///    column, never on raw `state`. Backfilled below from the provider-truthful pair (`state`,
///    `merged_at`); new rows are stamped at store time.
///  - `author_kind` / `author_association` (items AND comments) — thread-shape facets (`user.type:
///    "Bot"`, `OWNER`/`MEMBER`/…), already in the parsed payloads.
///
/// Additive only: `CREATE TABLE IF NOT EXISTS` + `add_column_if_missing` + an idempotent
/// backfill UPDATE, so a torn replay reconverges without a wrapping transaction.
pub fn apply_papertrail_distill_substrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS papertrail_closing_edges(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            issue_kind TEXT NOT NULL,
            issue_key TEXT NOT NULL,
            -- 'change_request' | 'commit' (ClosingEdgeCloserKind::as_db_str)
            closer_kind TEXT NOT NULL,
            -- change_request: the closer item's key in the same project; commit: the full sha.
            closer_key TEXT NOT NULL,
            -- The closing/merge commit sha when known (a change_request closer's merge commit).
            closer_commit TEXT,
            -- 'provider' | 'text' (ClosingEdgeSource::as_db_str). Attribute, NOT key: the store
            -- upsert converges both tiers onto one row and never downgrades provider -> text.
            source TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_closing_edges_natural_key
            ON papertrail_closing_edges(repo_id, tracker, project, issue_kind, issue_key, \
         closer_kind, closer_key);
        CREATE INDEX IF NOT EXISTS idx_papertrail_closing_edges_closer
            ON papertrail_closing_edges(repo_id, tracker, project, closer_kind, closer_key);
        ",
    )?;
    add_column_if_missing(conn, "papertrail_items", "closed_at", "TEXT")?;
    add_column_if_missing(conn, "papertrail_items", "resolution", "TEXT")?;
    // INVARIANT: non-null ONLY for merged change requests — see the migration doc above.
    add_column_if_missing(conn, "papertrail_items", "merge_commit_sha", "TEXT")?;
    add_column_if_missing(
        conn,
        "papertrail_items",
        "state_normalized",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(conn, "papertrail_items", "author_kind", "TEXT")?;
    add_column_if_missing(conn, "papertrail_items", "author_association", "TEXT")?;
    add_column_if_missing(conn, "papertrail_comments", "author_kind", "TEXT")?;
    add_column_if_missing(conn, "papertrail_comments", "author_association", "TEXT")?;
    // Backfill the normalized state for pre-existing rows from the provider-truthful pair:
    // 'merged' state (GitLab MRs) or a recorded merged_at (GitHub PRs) wins over raw 'closed';
    // anything else non-closed is 'open'. Idempotent: the predicate re-derives the same value.
    conn.execute_batch(
        "
        UPDATE papertrail_items SET state_normalized = CASE
            WHEN state = 'merged' OR merged_at IS NOT NULL THEN 'merged'
            WHEN state = 'closed' THEN 'closed'
            ELSE 'open'
        END
        WHERE state_normalized = '';
        ",
    )
}

/// V077 — the distillation RECORD STORE (issue #703): the derived, regenerable
/// `papertrail_distill` table plus its junction children, thread-keyed edges, work queue, and
/// run-stats. Consumes the V073 substrate; produced by the #704 LLM pass.
///
/// DESIGN INVARIANTS (locked here because these are the costliest columns to change post-landing):
/// - **Findings-not-facts.** Records are DERIVED and regenerable, never written into trusted
///   memories. Regeneration identity is `(distill_input_hash, pipeline_version)`; the record row is
///   replaced in place on its natural key `(repo_id, tracker, project, item_kind, item_key)`.
/// - **Confidence is provenance FACETS, never a fused label** (a fused high/med/low label does not
///   discriminate accuracy — measured). Any display label is computed in the read layer.
/// - **Edges key to the THREAD, not the record row** (`papertrail_distill_edges`), so LWW body
///   edits / regeneration replace the record while supersession/coalesce/promotion edges survive.
/// - **Mechanical status floors kept raw** (`revert_override`, `closing_keyword_floor`, and the
///   `fix_edge_source='none'` no-fix-edge floor); the EFFECTIVE status is computed read-layer with
///   precedence revert > closing-keyword > no-fix-edge > `outcome_status_model`. Fixing commits are
///   mechanical (`papertrail_distill_record_commits`), NEVER LLM-emitted.
/// - **No CSV-in-TEXT**: alternatives / commits / anchors / evidence are junction tables.
/// - **Anchors born as `sym_<hex>` bindings** (relocation-compatible) with EXACT file paths; no
///   basename fallback. `epistemic_status_*` makes proposed-not-landed / projected representable.
///
/// Additive: CREATE ... IF NOT EXISTS; nothing pre-existing to backfill.
pub fn apply_distill_record_store(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS papertrail_distill(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            -- the coalesced work-unit thread; the ISSUE side when an issue<->PR pair is coalesced.
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            -- regeneration identity: the row is replaced on the natural key when either changes.
            distill_input_hash TEXT NOT NULL,
            pipeline_version INTEGER NOT NULL,
            root_issue TEXT,
            -- NULL is an HONEST null (no failure, or none established) — not missing data.
            root_cause TEXT,
            -- free-text detail in v1; the induced-taxonomy FK is deferred (#705 non-goal).
            root_cause_class TEXT,
            decision_chosen TEXT,
            outcome_summary TEXT,
            -- MODEL-emitted status (OutcomeStatus::as_db_str). The EFFECTIVE status is computed in
            -- the read layer: revert_override > closing_keyword_floor > no-fix-edge > this.
            outcome_status_model TEXT,
            -- event-factuality (EpistemicStatus): \
         asserted_landed|projected|proposed_not_landed|superseded.
            epistemic_status_decision TEXT,
            epistemic_status_outcome TEXT,
            -- provenance FACETS (NOT a fused confidence label).
            fix_edge_source TEXT NOT NULL,               -- FixEdgeSource: provider|text|none
            quotes_materialized INTEGER NOT NULL DEFAULT 0,
            anchors_qualified_count INTEGER NOT NULL DEFAULT 0,
            thread_shape TEXT NOT NULL,                  -- ThreadShape: \
         investigation|review_stream|thin
            outcome_claim_verified INTEGER NOT NULL DEFAULT 0,
            decision_provenance_verified INTEGER NOT NULL DEFAULT 0,
            -- raw status-floor inputs (precedence applied in the read layer, never here).
            revert_override INTEGER NOT NULL DEFAULT 0,
            closing_keyword_floor TEXT,
            distilled_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_natural_key
            ON papertrail_distill(repo_id, tracker, project, item_kind, item_key);

        -- Evidence units: byte-span SELECTIONS with SNAPSHOTTED provenance + a MATERIALIZED quote
        -- (raw spans dangle under the mirror's per-row LWW body edits).
        CREATE TABLE IF NOT EXISTS papertrail_distill_evidence(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            field TEXT NOT NULL,                 -- record field supported: \
         root_cause|decision|outcome
            source_kind TEXT NOT NULL,           -- 'item' | 'comment'
            source_id TEXT NOT NULL,
            byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
            quote TEXT NOT NULL,
            author TEXT, author_kind TEXT, author_association TEXT,
            unit_created_at_ms INTEGER,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_evidence_thread
            ON papertrail_distill_evidence(repo_id, tracker, project, item_kind, item_key);

        -- Anchor candidates: index-validated, born as sym_<hex> bindings; EXACT file paths only.
        -- V078 adds their stable candidate ordinals and model-selection state.
        CREATE TABLE IF NOT EXISTS papertrail_distill_anchors(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            anchor_kind TEXT NOT NULL,           -- AnchorKind: \
         symbol|file|schema_object|crate|config_key
            logical_symbol_id TEXT,              -- sym_<hex> when anchor_kind='symbol' AND \
         resolved
            file_path TEXT,
            name TEXT NOT NULL,
            resolved INTEGER NOT NULL DEFAULT 0,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_thread
            ON papertrail_distill_anchors(repo_id, tracker, project, item_kind, item_key);
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_symbol
            ON papertrail_distill_anchors(repo_id, logical_symbol_id);

        -- Rejected alternatives (junction; ordinal-stable, no CSV).
        CREATE TABLE IF NOT EXISTS papertrail_distill_alternatives(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            alternative TEXT NOT NULL, reason TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_alternatives_key
            ON papertrail_distill_alternatives(repo_id, tracker, project, item_kind, item_key, \
         ordinal);

        -- Fixing commits: MECHANICAL, from the closing edge; outcome.commits is never LLM-emitted.
        CREATE TABLE IF NOT EXISTS papertrail_distill_record_commits(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_record_commits_key
            ON papertrail_distill_record_commits(repo_id, tracker, project, item_kind, item_key, \
         commit_sha);

        -- Thread-keyed edges: survive record regeneration.
        CREATE TABLE IF NOT EXISTS papertrail_distill_edges(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            src_item_kind TEXT NOT NULL, src_item_key TEXT NOT NULL,
            dst_item_kind TEXT NOT NULL, dst_item_key TEXT NOT NULL,
            edge_kind TEXT NOT NULL,             -- DistillEdgeKind: coalesced|supersedes|promoted
            created_at_ms INTEGER NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_edges_key
            ON papertrail_distill_edges(repo_id, tracker, project, src_item_kind, src_item_key,
                                        dst_item_kind, dst_item_key, edge_kind);

        -- Work queue: enqueued at mirror sync (cheap SQL), DRAINED only by the dream-lane pass.
        CREATE TABLE IF NOT EXISTS papertrail_distill_queue(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            enqueued_at_ms INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            raw_reply TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_queue_key
            ON papertrail_distill_queue(repo_id, tracker, project, item_kind, item_key);

        -- Per-run stats: the #704 verification bar (output-ladder rung + gate counters).
        CREATE TABLE IF NOT EXISTS papertrail_distill_runs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_at_ms INTEGER NOT NULL,
            threads INTEGER NOT NULL DEFAULT 0,
            rung_guided INTEGER NOT NULL DEFAULT 0,
            rung_serde INTEGER NOT NULL DEFAULT 0,
            rung_unguided INTEGER NOT NULL DEFAULT 0,
            rung_tolerant INTEGER NOT NULL DEFAULT 0,
            failed INTEGER NOT NULL DEFAULT 0,
            stats_json TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        ",
    )
}

/// V078 — separate mechanically mined anchor CANDIDATES from model SELECTED anchors (#704).
/// Existing V077 rows receive deterministic zero-based ordinals in row-id order within each
/// thread. The exact-path and `sym_<hex>` identity columns are deliberately untouched.
pub fn apply_distill_anchor_selection(conn: &Connection) -> rusqlite::Result<()> {
    // Key the backfill guard to its completion artifact, not merely column presence. If a process
    // dies after ADD COLUMN (whose default makes every legacy row ordinal 0) but before the
    // backfill/index, replay must backfill again rather than fail forever on duplicate ordinals.
    let candidate_index_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'index' AND name = 'idx_papertrail_distill_anchors_candidate'",
        [],
        |row| row.get(0),
    )?;
    add_column_if_missing(
        conn,
        "papertrail_distill_anchors",
        "candidate_ordinal",
        "INTEGER NOT NULL DEFAULT 0 CHECK(candidate_ordinal >= 0)",
    )?;
    add_column_if_missing(
        conn,
        "papertrail_distill_anchors",
        "selected",
        "INTEGER NOT NULL DEFAULT 0 CHECK(selected IN (0, 1))",
    )?;
    if candidate_index_exists == 0 {
        conn.execute_batch(
            "
        UPDATE papertrail_distill_anchors AS anchor
        SET candidate_ordinal = (
            SELECT COUNT(*)
            FROM papertrail_distill_anchors AS earlier
            WHERE earlier.repo_id = anchor.repo_id
              AND earlier.tracker = anchor.tracker
              AND earlier.project = anchor.project
              AND earlier.item_kind = anchor.item_kind
              AND earlier.item_key = anchor.item_key
              AND earlier.id < anchor.id
        );
        ",
        )?;
    }
    conn.execute_batch(
        "
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_candidate
            ON papertrail_distill_anchors(
                repo_id, tracker, project, item_kind, item_key, candidate_ordinal
            );
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_selected
            ON papertrail_distill_anchors(
                repo_id, tracker, project, item_kind, item_key, candidate_ordinal
            ) WHERE selected = 1;
        ",
    )
}

/// V079 — extraction-owned, immutable-for-one-input source and unit snapshots (#704).
///
/// There is deliberately no SQL backfill: old derived records must regenerate under the bumped
/// extraction pipeline and snapshot the mirror rows read in that same transaction. Backfilling
/// here would falsely attach today's mutable mirror text to an older `distill_input_hash`.
pub fn apply_distill_safe_input_snapshot(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "papertrail_distill", "prompt_version", "INTEGER")?;
    add_column_if_missing(conn, "papertrail_distill", "model_input_hash", "TEXT")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS papertrail_distill_sources(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            source_ordinal INTEGER NOT NULL CHECK(source_ordinal >= 0),
            role TEXT NOT NULL CHECK(role IN ('primary', 'partner')),
            partner_ordinal INTEGER CHECK(partner_ordinal >= 0),
            source_item_kind TEXT NOT NULL,
            source_item_key TEXT NOT NULL,
            source_kind TEXT NOT NULL CHECK(source_kind IN ('item', 'comment')),
            source_part TEXT NOT NULL CHECK(source_part IN ('title', 'body', 'comment')),
            source_id TEXT NOT NULL,
            exact_text TEXT NOT NULL,
            author TEXT,
            author_kind TEXT,
            author_association TEXT,
            created_at_ms INTEGER,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            CHECK((role = 'primary' AND partner_ordinal IS NULL) OR
                  (role = 'partner' AND partner_ordinal IS NOT NULL)),
            CHECK((source_kind = 'item' AND source_part IN ('title', 'body')) OR
                  (source_kind = 'comment' AND source_part = 'comment'))
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_sources_ordinal
            ON papertrail_distill_sources(
                repo_id, tracker, project, item_kind, item_key, source_ordinal
            );
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_sources_identity
            ON papertrail_distill_sources(
                repo_id, tracker, project, source_item_kind, source_item_key, source_kind, \
         source_id
            );

        CREATE TABLE IF NOT EXISTS papertrail_distill_units(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            unit_ordinal INTEGER NOT NULL CHECK(unit_ordinal >= 0),
            source_ordinal INTEGER NOT NULL CHECK(source_ordinal >= 0),
            byte_start INTEGER NOT NULL CHECK(byte_start >= 0),
            byte_end INTEGER NOT NULL CHECK(byte_end > byte_start),
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_units_ordinal
            ON papertrail_distill_units(
                repo_id, tracker, project, item_kind, item_key, unit_ordinal
            );
        CREATE INDEX IF NOT EXISTS idx_papertrail_distill_units_source
            ON papertrail_distill_units(
                repo_id, tracker, project, item_kind, item_key, source_ordinal, unit_ordinal
            );
        ",
    )
}

/// V080 — extraction-owned enriched-context snapshots (#800): the fix diff (restricted to files
/// with symbol anchor candidates) and the thread's cross-referenced item titles + opening
/// paragraphs, snapshotted so the drain never reads mutable git/mirror state.
///
/// There is deliberately no SQL backfill, same doctrine as V079: old derived records must
/// regenerate under the bumped extraction pipeline and snapshot the git/mirror state read in that
/// same transaction.
pub fn apply_distill_enriched_context(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS papertrail_distill_fix_diffs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            path TEXT NOT NULL,
            patch TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_fix_diffs_file
            ON papertrail_distill_fix_diffs(
                repo_id, tracker, project, item_kind, item_key, commit_sha, path
            );

        CREATE TABLE IF NOT EXISTS papertrail_distill_xrefs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            xref_ordinal INTEGER NOT NULL CHECK(xref_ordinal >= 0),
            target_tracker TEXT NOT NULL,
            target_project TEXT NOT NULL,
            target_item_kind TEXT,
            target_item_key TEXT NOT NULL,
            ref_kind TEXT NOT NULL,
            title TEXT NOT NULL,
            opening TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_papertrail_distill_xrefs_ordinal
            ON papertrail_distill_xrefs(
                repo_id, tracker, project, item_kind, item_key, xref_ordinal
            );
        ",
    )
}

/// V081 — persist source-part identity on distilled evidence rows (#801). V077's
/// `papertrail_distill_evidence` stores `source_kind`/`source_id` but not WHICH part of an item a
/// citation came from: an item's title and body share the same `source_id` (the item key), so two
/// citations with identical or overlapping spans were indistinguishable in the persisted record —
/// even though the V079 snapshot substrate keeps full identity at drain time. Add `source_part`
/// (title|body|comment, matching the V079 CHECK), populated by the drain from its `SourceSnapshot`.
///
/// Nullable: existing rows predate the column and keep NULL (which passes the value CHECK). No SQL
/// backfill — evidence is derived and rewritten wholesale on every drain, so a re-drain repopulates
/// it (the derived-data doctrine of V079/V080). The source ITEM identity is deliberately NOT added:
/// distilled evidence is primary-only (the drain rejects partner units), so the source item is
/// always the record's own `(item_kind, item_key)` already stored on the row. Guarded by
/// `column_exists` so a torn replay (column added, migration row not yet recorded) is a no-op
/// rather than a duplicate-column error.
pub fn apply_distill_evidence_source_part(conn: &Connection) -> rusqlite::Result<()> {
    if column_exists(conn, "papertrail_distill_evidence", "source_part")? {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE papertrail_distill_evidence
             ADD COLUMN source_part TEXT CHECK(source_part IN ('title', 'body', 'comment'));",
    )
}

/// V085 (#691 A-pre): memory-sync provenance + edge tombstones — the write-path foundation for
/// projecting synced content into the read tables without corrupting the local reconcile.
///
/// - `origin` on `repo_memories` / `repo_node_edges` distinguishes a locally-authored row from one
///   projected from a synced sibling's `/3` content. It is LOAD-BEARING on the WRITE path: the
///   memory reconcile authors every read-table row MISSING from the accepted-`/3` projection, so a
///   synced row whose acceptance is later revoked must NOT be re-authored as local `/3` (that
///   forges local authorship and re-legitimizes revoked content). The reconcile gates on `origin =
///   'local'`. Every existing row is locally authored, so the `'local'` default is a correct
///   backfill.
/// - `present` on `content_projected_edges` retains edge TOMBSTONES (`present = 0`) rather than
///   dropping removed edges. Without it a foreign `EdgeRemove` leaves the local `repo_node_edges`
///   row a ghost, the reconcile re-authors it at a fresh Lamport, and the remove loses LWW forever
///   — a cross-device op-log growth loop. Live edges default `present = 1`; the projector-version
///   bump rebuilds the projection to write tombstones going forward.
///
/// Additive columns with correct defaults; atomic under one immediate txn; idempotent via
/// `add_column_if_missing`.
pub fn apply_sync_origin_and_edge_tombstone(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    add_column_if_missing(
        &tx,
        "repo_memories",
        "origin",
        "TEXT NOT NULL DEFAULT 'local' CHECK (origin IN ('local', 'synced'))",
    )?;
    add_column_if_missing(
        &tx,
        "repo_node_edges",
        "origin",
        "TEXT NOT NULL DEFAULT 'local' CHECK (origin IN ('local', 'synced'))",
    )?;
    add_column_if_missing(&tx, "content_projected_edges", "present", "INTEGER NOT NULL DEFAULT 1")?;
    tx.commit()
}

/// V086 (#828): stand up the incrementally-maintained `content_revision` digest.
///
/// Invariant established here: `content_digest_state.state` (one row, id = 1) is the 256-bit
/// additive multiset hash of `{(path, sha256) : main.files, kind != 'deleted'}` at every
/// transaction boundary, maintained by the three `files_content_digest_*` triggers
/// [`crate::content_digest::ensure_content_digest`] creates. `content_revision()` becomes an O(1)
/// read of that row (rendered `ms1-…`) instead of the O(N) `main.files` scan-sort-concat-hash.
///
/// The body runs in ONE immediate transaction, in order:
///  1. Create the state table + triggers (idempotent; a future `files`-rebuild migration MUST call
///     the same helper and reseed, because `DROP TABLE files` silently drops the triggers).
///  2. Seed the state row with a from-scratch Rust fold over the current non-deleted `files` — the
///     SAME per-row hash the trigger fold uses, so the trigger-maintained state and a recompute can
///     never disagree. Atomic with trigger creation, so no write slips between.
///  3. Re-stamp every freshness stamp that equals the FROZEN legacy digest (the pre-#828
///     `hex_sha256(group_concat(path||':'||sha256 ORDER BY path))`, inlined because migrations are
///     snapshots) to the new rendered digest. This is the ONLY place the digest value change is
///     absorbed: a stamp equal to the legacy value was fresh, so pointing it at the new value
///     avoids a one-time full FTS re-tokenize (`fts_source_revision`), a ~1 GB clone-graph rebuild
///     (`clone_graph_generations.source_revision`), and a reset of the clone quiet window
///     (`clone_graph_quiet_candidate_revision`). A stamp that did NOT equal the legacy digest was
///     already stale and is left for the normal freshness machinery — exactly as it would have
///     been.
///
/// If step 3 were dropped everything still self-heals (one FTS rebuild, one quiet-gated clone
/// rebuild); the re-stamp is one cheap legacy scan that avoids that first-use rebuild storm.
pub fn apply_content_digest_state(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    // 1. Table + triggers.
    crate::content_digest::ensure_content_digest(&tx)?;

    // 2. From-scratch seed fold over the current non-deleted rows (order-free — the fold is
    //    commutative, so no ORDER BY is needed).
    let mut state = [0u64; 4];
    let mut rows_folded: i64 = 0;
    {
        let mut stmt = tx.prepare("SELECT path, sha256 FROM main.files WHERE kind != 'deleted'")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let sha256: String = row.get(1)?;
            let hash = crate::content_digest::content_row_hash(&path, &sha256);
            crate::content_digest::fold_row(&mut state, &hash, true);
            rows_folded += 1;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO content_digest_state(id, state, rows_folded) VALUES (1, ?1, ?2)",
        params![crate::content_digest::encode_state(&state), rows_folded],
    )?;

    // 3. Re-stamp the frozen legacy digest -> the new rendered digest wherever it was still
    //    current.
    let legacy_concat: String = tx.query_row(
        "SELECT COALESCE(group_concat(pv, ','), '') FROM (SELECT path || ':' || sha256 AS pv FROM \
         main.files WHERE kind != 'deleted' ORDER BY path)",
        [],
        |row| row.get(0),
    )?;
    let legacy_digest = rag_rat_base::hash::hex_sha256(legacy_concat.as_bytes());
    let new_digest = crate::content_digest::render_revision(&state);
    // The GLOBAL freshness stamps (`content_revision` keeps `global_status`'s rollup consistent;
    // `fts_source_revision` prevents a full chunk-text re-tokenize on first `ensure_fts_fresh`).
    tx.execute(
        "UPDATE index_meta SET value = ?1
         WHERE key IN ('fts_source_revision', 'content_revision') AND value = ?2",
        params![new_digest, legacy_digest],
    )?;
    // Every clone-graph generation stamped at the legacy digest keeps the postings fast path
    // serving and skips a one-time full rebuild.
    tx.execute(
        "UPDATE clone_graph_generations SET source_revision = ?1 WHERE source_revision = ?2",
        params![new_digest, legacy_digest],
    )?;
    // A per-repo armed quiet-window candidate survives the upgrade instead of resetting its
    // stability clock (the key literal matches CLONE_GRAPH_QUIET_REVISION_META in rag-rat-core).
    tx.execute(
        "UPDATE repo_meta SET value = ?1
         WHERE key = 'clone_graph_quiet_candidate_revision' AND value = ?2",
        params![new_digest, legacy_digest],
    )?;

    tx.commit()
}

/// V087 — the table→log sync engine's bookkeeping tables (transport-independent).
///
/// The engine replicates derived/metadata rows as self-describing typed-CBOR ops on a signed
/// per-scope stream, folded by WHOLE-ROW last-writer-wins. The side tables carry the state the fold
/// and producer need; none holds authored content (that lives in the replicated tables themselves)
/// — these are pure sync bookkeeping.
///
/// - `sync_published_rows` is the anti-echo record: the post-apply hash of a row's SYNCED columns.
///   The producer skips a row whose current synced-hash already matches, so a remotely-applied row
///   is never re-signed and rebroadcast (the echo-republish loop). Local re-resolution churn must
///   never enter this hash, so it covers synced columns only.
/// - `sync_row_tombstones` is the per-row deletion clock: a `Remove` records `(lamport,
///   device_fingerprint)`, and a later `Upsert` older than it is suppressed (never resurrects a
///   deleted row) while a newer one overrides it. Without it, out-of-order delivery would let a
///   stale delete win and an even older insert resurrect.
/// - `sync_row_clocks` is the per-row latest-write clock — the whole-row LWW authority. An `Upsert`
///   wins the entire row, and a `Remove` deletes it, only when it beats this clock; the winner then
///   raises it. So convergence and delete/insert ordering hold regardless of arrival order.
/// - `table_sync_entries` is the engine's OWN signed hash-chained entry log — deliberately separate
///   from `oplog_entries`, whose upgrade re-fold (`reproject_all_streams`) decodes every stored
///   stream as a memory-content op and would choke on a table op. One chain per `(stream_id,
///   device_fingerprint)`, `lamport` strictly increasing.
///
/// All STRICT; `CREATE TABLE IF NOT EXISTS` so the migration is idempotent; one IMMEDIATE txn.
pub fn apply_table_sync_tables(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_entries(
             entry_hash         BLOB    NOT NULL PRIMARY KEY,
             stream_id          BLOB    NOT NULL,
             device_fingerprint BLOB    NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB,
             signed_bytes       BLOB    NOT NULL,
             received_at_ms     INTEGER NOT NULL,
             UNIQUE(stream_id, device_fingerprint, lamport)
         ) STRICT;
         -- Read the stream's Lamport tip (`MAX(lamport) WHERE stream_id = ?`, on every author and
         -- accept) from an index tail instead of scanning the stream; the UNIQUE index above leads
         -- with device_fingerprint, so it cannot answer a per-stream MAX.
         CREATE INDEX IF NOT EXISTS table_sync_entries_stream_lamport
             ON table_sync_entries(stream_id, lamport);
         CREATE TABLE IF NOT EXISTS sync_published_rows(
             repo_id     TEXT NOT NULL,
             table_name  TEXT NOT NULL,
             row_pk      TEXT NOT NULL,
             synced_hash TEXT NOT NULL,
             PRIMARY KEY(repo_id, table_name, row_pk)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS sync_row_tombstones(
             repo_id            TEXT    NOT NULL,
             table_name         TEXT    NOT NULL,
             row_pk             TEXT    NOT NULL,
             lamport            INTEGER NOT NULL,
             device_fingerprint TEXT    NOT NULL,
             PRIMARY KEY(repo_id, table_name, row_pk)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS sync_row_clocks(
             repo_id            TEXT    NOT NULL,
             table_name         TEXT    NOT NULL,
             row_pk             TEXT    NOT NULL,
             lamport            INTEGER NOT NULL,
             device_fingerprint TEXT    NOT NULL,
             PRIMARY KEY(repo_id, table_name, row_pk)
         ) STRICT;",
    )?;
    tx.commit()
}

/// V088 (#830): cache each clone generation's posting-row count on its generation row.
///
/// The #598 delta work budget is `max(100_000, 2 * postings_row_count(generation))`; deriving that
/// with `COUNT(*) FROM clone_subblock_postings WHERE build_generation = ?` scanned the whole
/// (generation-keyed) postings table on every delta pass. This adds a maintained
/// `postings_row_count` column so the budget reads one generation row instead. The count is kept
/// exact going forward — seeded at build (`complete_generation`) and adjusted by
/// (inserted − deleted) in each delta write-back, both inside the same transaction as the postings
/// change — so the column always equals `COUNT(*)` for that generation.
///
/// Additive + idempotent. The column type is STRICT-valid and defaulted so a row the backfill does
/// not reach reads back 0. The backfill is UNCONDITIONAL (not gated on the column being freshly
/// added): it recomputes the exact `COUNT(*)` the column is maintained to hold, so on a maintained
/// DB a re-apply is a value no-op, and a torn add-then-crash retry (which re-enters with the column
/// already present) still backfills instead of stranding an all-zero column. Re-scanning on the
/// rare `index --full` re-apply is the acceptable cost of that robustness — the per-delta scan this
/// migration removes is the hot one.
pub fn apply_clone_postings_row_count(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(
        conn,
        "clone_graph_generations",
        "postings_row_count",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch(
        "UPDATE clone_graph_generations
            SET postings_row_count = (
                SELECT COUNT(*) FROM clone_subblock_postings
                 WHERE build_generation = clone_graph_generations.generation);",
    )?;
    Ok(())
}

/// V089 (#945): durable, single-use enrollment invites.
pub fn apply_sync_invites(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_invites(
             nonce         BLOB    NOT NULL PRIMARY KEY CHECK(length(nonce) = 32),
             account_id    BLOB    NOT NULL CHECK(length(account_id) = 32),
             role          TEXT    NOT NULL CHECK(role IN ('read_only', 'member', 'owner')),
             label         TEXT,
             expires_at_ms INTEGER NOT NULL,
             created_at_ms INTEGER NOT NULL,
             used_at_ms    INTEGER,
             used_transport_node BLOB CHECK(
                 used_transport_node IS NULL OR length(used_transport_node) = 32
             ),
             used_ed25519_pubkey BLOB CHECK(
                 used_ed25519_pubkey IS NULL OR length(used_ed25519_pubkey) = 32
             ),
             used_x25519_pubkey BLOB CHECK(
                 used_x25519_pubkey IS NULL OR length(used_x25519_pubkey) = 32
             ),
             receipt_hash BLOB CHECK(receipt_hash IS NULL OR length(receipt_hash) = 32),
             receipt_signed BLOB,
             receipt_bytes BLOB,
             CHECK(
                 (used_at_ms IS NULL
                  AND used_transport_node IS NULL
                  AND used_ed25519_pubkey IS NULL
                  AND used_x25519_pubkey IS NULL
                  AND receipt_hash IS NULL
                  AND receipt_signed IS NULL
                  AND receipt_bytes IS NULL)
                 OR
                 (used_at_ms IS NOT NULL
                  AND used_transport_node IS NOT NULL
                  AND used_ed25519_pubkey IS NOT NULL
                  AND used_x25519_pubkey IS NOT NULL
                  AND receipt_hash IS NOT NULL
                  AND receipt_signed IS NOT NULL
                  AND receipt_bytes IS NOT NULL)
             )
         ) STRICT;
         CREATE INDEX IF NOT EXISTS sync_invites_account_expiry
             ON sync_invites(account_id, expires_at_ms);",
    )
}

/// V090 (#949): durable candidate-capacity reservations for outstanding enrollment invites.
pub fn apply_account_candidate_reservations(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_candidate_reservations(
             reservation_id BLOB    NOT NULL PRIMARY KEY CHECK(length(reservation_id) = 32),
             account_id     BLOB    NOT NULL CHECK(length(account_id) = 32),
             reserved_entries INTEGER NOT NULL CHECK(reserved_entries >= 0),
             reserved_bytes   INTEGER NOT NULL CHECK(reserved_bytes >= 0),
             expires_at_ms    INTEGER NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS account_candidate_reservations_account_expiry
             ON account_candidate_reservations(account_id, expires_at_ms);",
    )
}

/// V091 (#949): track the live key-target count each invite reservation covers, so any fold that
/// grows the target set — local authoring or REMOTELY synced `StreamOwn`/wrap entries — can top
/// the reservation up to the current mandatory redemption cost.
///
/// Additive + idempotent. The backfill is exact for every reservation written since V090: those
/// rows hold `reserved_entries = 1 (DeviceAdd) + covered_targets`, so `reserved_entries - 1`
/// recovers the covered target count.
pub fn apply_account_candidate_reservation_targets(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(
        conn,
        "account_candidate_reservations",
        "reserved_targets",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch(
        "UPDATE account_candidate_reservations
            SET reserved_targets = MAX(0, reserved_entries - 1);",
    )?;
    Ok(())
}

/// V093: maintain one O(1) Lens enrichment revision per repository. The revision is deliberately
/// a counter, not a timestamp: multiple writes in one millisecond, in-place promotions, and clone
/// graph publication must still invalidate connected editors.
pub fn apply_lens_enrichment_revision(conn: &Connection) -> rusqlite::Result<()> {
    // History imports and Oracle passes advance the clock once at their transaction boundary
    // instead of once per written row. Both write their table in bulk inside a single
    // transaction — an Oracle run rewrites a verdict for every resolved edge, hundreds of
    // thousands of them on a large index — and Lens freshness only needs the one revision change
    // the publication makes visible. `edge_oracle` gets that bump from the `oracle_runs` row its
    // run commits alongside the verdicts; the dead-checkout verdict sweep, which writes no run
    // row, bumps the clock itself. Always remove the per-row triggers so replaying this migration
    // repairs a database initialized by an older build.
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS git_file_changes_lens_revision_insert;
         DROP TRIGGER IF EXISTS git_file_changes_lens_revision_delete;
         DROP TRIGGER IF EXISTS git_file_changes_lens_revision_update;
         DROP TRIGGER IF EXISTS edge_oracle_lens_revision_insert;
         DROP TRIGGER IF EXISTS edge_oracle_lens_revision_delete;
         DROP TRIGGER IF EXISTS edge_oracle_lens_revision_update;",
    )?;
    for (table, trigger_prefix) in [
        ("repo_memories", "memories_lens_revision"),
        ("repo_memory_bindings", "memory_bindings_lens_revision"),
        ("memory_reality", "memory_reality_lens_revision"),
        ("memory_summaries", "memory_summaries_lens_revision"),
        ("papertrail_items", "papertrail_items_lens_revision"),
        ("papertrail_refs", "papertrail_refs_lens_revision"),
        ("papertrail_distill", "papertrail_distill_lens_revision"),
        ("papertrail_distill_anchors", "papertrail_distill_anchors_lens_revision"),
        ("clone_refinements", "clone_refinements_lens_revision"),
        ("oracle_runs", "oracle_runs_lens_revision"),
    ] {
        create_repo_scoped_lens_revision_triggers(
            conn,
            table,
            trigger_prefix,
            crate::meta::LENS_ENRICHMENT_REVISION_META,
        )?;
    }
    create_live_clone_graph_revision_triggers(
        conn,
        "clone_graph_generations_lens_revision",
        crate::meta::LENS_ENRICHMENT_REVISION_META,
    )?;
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS clone_graph_pointer_lens_revision_insert;
         DROP TRIGGER IF EXISTS clone_graph_pointer_lens_revision_delete;
         DROP TRIGGER IF EXISTS clone_graph_pointer_lens_revision_update;",
    )?;
    Ok(())
}

/// V102: split the aggregate Lens enrichment clock into the five editor data lanes.
pub fn apply_lens_lane_revisions(conn: &Connection) -> rusqlite::Result<()> {
    use crate::meta;

    for (table, trigger_prefix, key) in [
        ("repo_memories", "memories_lane_revision", meta::LENS_MEMORIES_REVISION_META),
        (
            "repo_memory_bindings",
            "memory_bindings_lane_revision",
            meta::LENS_MEMORIES_REVISION_META,
        ),
        ("memory_reality", "memory_reality_lane_revision", meta::LENS_MEMORIES_REVISION_META),
        ("memory_summaries", "memory_summaries_lane_revision", meta::LENS_MEMORIES_REVISION_META),
        ("papertrail_items", "papertrail_items_lane_revision", meta::LENS_PAPERTRAIL_REVISION_META),
        ("papertrail_refs", "papertrail_refs_lane_revision", meta::LENS_PAPERTRAIL_REVISION_META),
        (
            "papertrail_distill",
            "papertrail_distill_lane_revision",
            meta::LENS_PAPERTRAIL_REVISION_META,
        ),
        (
            "papertrail_distill_anchors",
            "papertrail_distill_anchors_lane_revision",
            meta::LENS_PAPERTRAIL_REVISION_META,
        ),
        ("clone_refinements", "clone_refinements_lane_revision", meta::LENS_CLONES_REVISION_META),
        ("oracle_runs", "oracle_symbols_lane_revision", meta::LENS_SYMBOLS_REVISION_META),
        ("oracle_runs", "oracle_clones_lane_revision", meta::LENS_CLONES_REVISION_META),
    ] {
        create_repo_scoped_lens_revision_triggers(conn, table, trigger_prefix, key)?;
    }
    create_live_clone_graph_revision_triggers(
        conn,
        "clone_graph_generations_lane_revision",
        meta::LENS_CLONES_REVISION_META,
    )
}

/// V092 (#949): stop duplicating every enrollment receipt in the invite row.
///
/// Each consumed invite used to store the complete signed account bootstrap (`receipt_bytes`),
/// so a fleet enrolling within the replay window kept one full history copy PER invite —
/// quadratic growth in a grow-only store. New redemptions persist only the joiner-specific
/// `DeviceAdd` envelope plus the manifest of receipt entry hashes (`receipt_entries`, 32 bytes
/// each in receipt order), and replay reconstructs the EXACT acknowledged receipt from the
/// grow-only candidate DAG. The legacy `receipt_bytes` column is RETAINED (never written again)
/// so invites consumed before this migration keep replaying through their 24h window; pruning
/// drops both forms. Rebuilds the table, preserving every row.
pub fn apply_sync_invites_normalized_receipts(conn: &Connection) -> rusqlite::Result<()> {
    // A re-apply (the sanctioned `index --full` recovery) sees the V092 shape: preserve its
    // `receipt_entries` manifests rather than nulling them into the consumed-row CHECK.
    let already_normalized = column_exists(conn, "sync_invites", "receipt_entries")?;
    let entries_expr = if already_normalized { "receipt_entries" } else { "NULL" };
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE sync_invites_v092(
             nonce         BLOB    NOT NULL PRIMARY KEY CHECK(length(nonce) = 32),
             account_id    BLOB    NOT NULL CHECK(length(account_id) = 32),
             role          TEXT    NOT NULL CHECK(role IN ('read_only', 'member', 'owner')),
             label         TEXT,
             expires_at_ms INTEGER NOT NULL,
             created_at_ms INTEGER NOT NULL,
             used_at_ms    INTEGER,
             used_transport_node BLOB CHECK(
                 used_transport_node IS NULL OR length(used_transport_node) = 32
             ),
             used_ed25519_pubkey BLOB CHECK(
                 used_ed25519_pubkey IS NULL OR length(used_ed25519_pubkey) = 32
             ),
             used_x25519_pubkey BLOB CHECK(
                 used_x25519_pubkey IS NULL OR length(used_x25519_pubkey) = 32
             ),
             receipt_hash BLOB CHECK(receipt_hash IS NULL OR length(receipt_hash) = 32),
             receipt_signed BLOB,
             receipt_entries BLOB CHECK(
                 receipt_entries IS NULL OR length(receipt_entries) % 32 = 0
             ),
             receipt_bytes BLOB,
             CHECK(
                 (used_at_ms IS NULL
                  AND used_transport_node IS NULL
                  AND used_ed25519_pubkey IS NULL
                  AND used_x25519_pubkey IS NULL
                  AND receipt_hash IS NULL
                  AND receipt_signed IS NULL
                  AND receipt_entries IS NULL
                  AND receipt_bytes IS NULL)
                 OR
                 (used_at_ms IS NOT NULL
                  AND used_transport_node IS NOT NULL
                  AND used_ed25519_pubkey IS NOT NULL
                  AND used_x25519_pubkey IS NOT NULL
                  AND receipt_hash IS NOT NULL
                  AND receipt_signed IS NOT NULL
                  AND (receipt_entries IS NOT NULL OR receipt_bytes IS NOT NULL))
              )
         ) STRICT;",
    )?;
    tx.execute_batch(&format!(
        "INSERT INTO sync_invites_v092(
             nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms,
             used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
             receipt_hash, receipt_signed, receipt_entries, receipt_bytes)
          SELECT nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms,
             used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
             receipt_hash, receipt_signed, {entries_expr}, receipt_bytes
            FROM sync_invites;
         DROP TABLE sync_invites;
         ALTER TABLE sync_invites_v092 RENAME TO sync_invites;
         CREATE INDEX IF NOT EXISTS sync_invites_account_expiry
             ON sync_invites(account_id, expires_at_ms);"
    ))?;
    tx.commit()
}

/// V093 (#1001): the table-sync forward-compat projection substrate — facts the engine could not
/// previously record, each of which silently corrupts a synced table once one registers.
///
/// - `table_sync_entries.pending_reason` / `.pending_projector_version`: an entry this binary
///   cannot fully project (unknown column, unknown op-kind, undecodable payload, table out of
///   scope) is retained but was never marked, so redelivery short-circuits on `entry_exists` and
///   the payload is unrecoverable. Marking it lets a later binary that understands it replay
///   exactly the outstanding set. NULL reason = fully projected.
/// - `table_sync_entries.quarantine_reason`: the TERMINAL counterpart. A payload rejected on its
///   own merits — a type mismatch, a constraint violation — is not a version gap, and no later
///   binary makes those data fit, so it must leave the replay worklist. Recording WHY keeps it
///   discoverable, instead of a retained entry that looks fully projected but was actually
///   rejected.
/// - `table_sync_streams`: `stream_id` is a ONE-WAY sha256 of `(repo_id, account_id, scope_id)`,
///   and entries store only the stream id. Replay needs `repo_id` to apply and the scope to resolve
///   the table spec, so without this directory a stored entry cannot be replayed at all.
/// - `sync_published_rows.projector_version`: the anti-echo hash is computed over the hashing
///   binary's `spec.columns`, so a stored hash means "this row under column set C" with C implicit.
///   Once a column set grows, every stored hash mismatches structurally and every row reads as a
///   local delta — re-authoring the whole table at fresh winning lamports on every upgrading
///   device. Recording the version makes a mismatched hash detectable as NOT COMPARABLE instead.
///
/// Additive and idempotent: the sanctioned `index --full` re-apply sees the V093 shape and skips
/// the column adds. Nothing backfills, because no table is registered yet — `SYNCABLE_TABLES` is
/// empty, so `table_sync_entries` and `sync_published_rows` are necessarily empty too, and the
/// `projector_version` default of 0 is therefore unreachable rather than a lie about existing rows.
pub fn apply_table_sync_projection_state(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    // Per-column, never one guard for the group: a store that applied an EARLIER shape of this
    // migration already has the first column, so a group guard would skip the rest forever — the
    // ladder records the migration as applied and never re-runs it, leaving a column missing on a
    // store that reports itself current.
    add_column_if_missing(&tx, "table_sync_entries", "pending_reason", "TEXT")?;
    add_column_if_missing(&tx, "table_sync_entries", "pending_projector_version", "INTEGER")?;
    add_column_if_missing(&tx, "table_sync_entries", "quarantine_reason", "TEXT")?;
    // V095 REPLACED this column with the per-table `spec_version`. `schema::apply` — the
    // `index --full` recovery — re-runs the WHOLE ladder over an existing store, so without this
    // check a store already at V095 would get the dead column back on every full reindex, and V095
    // (which can only restore its shape by rebuilding the table) would have to DROP a table that by
    // then holds live publication state. Losing that state is not merely churn: `produce_row_ops`
    // finds a locally-deleted row by its surviving published record, so wiping the table strands
    // every unsent deletion and leaves peers holding the row forever.
    if !column_exists(&tx, "sync_published_rows", "spec_version")? {
        add_column_if_missing(
            &tx,
            "sync_published_rows",
            "projector_version",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_streams(
             stream_id  BLOB NOT NULL PRIMARY KEY,
             repo_id    TEXT NOT NULL,
             account_id BLOB NOT NULL,
             scope_id   TEXT NOT NULL
         ) STRICT;
         -- The refold's worklist. Partial, so a projector bump costs O(outstanding entries) rather
         -- than a scan of the whole log, and the steady state (nothing pending) costs nothing.
         CREATE INDEX IF NOT EXISTS table_sync_entries_pending
             ON table_sync_entries(pending_reason)
             WHERE pending_reason IS NOT NULL;",
    )?;
    tx.commit()
}

fn create_repo_scoped_lens_revision_triggers(
    conn: &Connection,
    table: &str,
    trigger_prefix: &str,
    key: &str,
) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS {trigger_prefix}_insert;
         DROP TRIGGER IF EXISTS {trigger_prefix}_delete;
         DROP TRIGGER IF EXISTS {trigger_prefix}_update;"
    ))?;
    if !column_exists(conn, table, "repo_id")? {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "CREATE TRIGGER {trigger_prefix}_insert
                  AFTER INSERT ON {table}
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, '{key}', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
             CREATE TRIGGER {trigger_prefix}_delete
                 AFTER DELETE ON {table}
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, '{key}', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
             CREATE TRIGGER {trigger_prefix}_update
                 AFTER UPDATE ON {table}
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, '{key}', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, '{key}', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;"
    ))?;
    Ok(())
}

fn create_live_clone_graph_revision_triggers(
    conn: &Connection,
    trigger_prefix: &str,
    key: &str,
) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS {trigger_prefix}_insert;
         DROP TRIGGER IF EXISTS {trigger_prefix}_delete;
         DROP TRIGGER IF EXISTS {trigger_prefix}_update;"
    ))?;
    if !column_exists(conn, "clone_graph_generations", "repo_id")? {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "CREATE TRIGGER {trigger_prefix}_insert
         AFTER INSERT ON clone_graph_generations
         WHEN EXISTS (
             SELECT 1 FROM repo_meta
             WHERE repo_id = NEW.repo_id AND key = 'clone_graph_live_generation'
               AND CAST(value AS INTEGER) = NEW.generation
         )
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value) VALUES (NEW.repo_id, '{key}', '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
         CREATE TRIGGER {trigger_prefix}_delete
         AFTER DELETE ON clone_graph_generations
         WHEN EXISTS (
             SELECT 1 FROM repo_meta
             WHERE repo_id = OLD.repo_id AND key = 'clone_graph_live_generation'
               AND CAST(value AS INTEGER) = OLD.generation
         )
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value) VALUES (OLD.repo_id, '{key}', '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
         CREATE TRIGGER {trigger_prefix}_update
         AFTER UPDATE ON clone_graph_generations
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value)
             SELECT NEW.repo_id, '{key}', '1'
             WHERE EXISTS (
                 SELECT 1 FROM repo_meta
                 WHERE repo_id = NEW.repo_id AND key = 'clone_graph_live_generation'
                   AND CAST(value AS INTEGER) = NEW.generation
             )
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
             INSERT INTO repo_meta(repo_id, key, value)
             SELECT OLD.repo_id, '{key}', '1'
             WHERE (OLD.repo_id != NEW.repo_id OR OLD.generation != NEW.generation)
               AND EXISTS (
                 SELECT 1 FROM repo_meta
                 WHERE repo_id = OLD.repo_id AND key = 'clone_graph_live_generation'
                   AND CAST(value AS INTEGER) = OLD.generation
             )
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;"
    ))?;
    Ok(())
}

#[cfg(test)]
mod lens_lane_revision_migration_tests {
    use super::*;

    #[test]
    fn lane_triggers_are_idempotent_and_repo_scoped() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE repos(repo_id TEXT PRIMARY KEY);
             CREATE TABLE repo_meta(
                 repo_id TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT,
                 PRIMARY KEY(repo_id, key)
             );
             CREATE TABLE repo_memories(repo_id TEXT NOT NULL);
             CREATE TABLE papertrail_items(repo_id TEXT NOT NULL);
             CREATE TABLE clone_refinements(repo_id TEXT NOT NULL);
             CREATE TABLE oracle_runs(repo_id TEXT NOT NULL);
             CREATE TABLE clone_graph_generations(repo_id TEXT NOT NULL, generation INTEGER);
             INSERT INTO repos VALUES ('active'), ('sibling');",
        )
        .unwrap();

        apply_lens_lane_revisions(&conn).unwrap();
        apply_lens_lane_revisions(&conn).expect("trigger replay is a no-op");
        conn.execute("INSERT INTO repo_memories VALUES ('active')", []).unwrap();
        conn.execute("INSERT INTO papertrail_items VALUES ('sibling')", []).unwrap();
        conn.execute("INSERT INTO oracle_runs VALUES ('active')", []).unwrap();
        conn.execute(
            "INSERT INTO repo_meta VALUES ('active', 'clone_graph_live_generation', '7')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO clone_graph_generations VALUES ('active', 7)", []).unwrap();

        let revision = |repo_id, key| {
            crate::meta::repo_meta(&conn, repo_id, key)
                .unwrap()
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0)
        };
        assert_eq!(revision("active", crate::meta::LENS_MEMORIES_REVISION_META), 1);
        assert_eq!(revision("sibling", crate::meta::LENS_MEMORIES_REVISION_META), 0);
        assert_eq!(revision("sibling", crate::meta::LENS_PAPERTRAIL_REVISION_META), 1);
        assert_eq!(revision("active", crate::meta::LENS_PAPERTRAIL_REVISION_META), 0);
        assert_eq!(revision("active", crate::meta::LENS_SYMBOLS_REVISION_META), 1);
        assert_eq!(revision("active", crate::meta::LENS_CLONES_REVISION_META), 2);
    }
}

#[cfg(test)]
mod syncable_overlay_migration_tests {
    use super::*;

    fn triggers_on(conn: &Connection, table: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1",
            [table],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// The overlay tables must carry NO triggers after the ladder, and stay that way when the whole
    /// ladder replays (V093/V102 recreate the revision triggers ahead of V107 on every
    /// `index --full`, so V107's drop has to be unconditional to survive the replay).
    #[test]
    fn v107_leaves_the_overlay_tables_trigger_free_across_a_full_replay() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        assert_eq!(triggers_on(&conn, "memory_reality"), 0);
        assert_eq!(triggers_on(&conn, "memory_summaries"), 0);
    }

    /// With the triggers gone, a raw row write no longer advances the memories Lens lane — the
    /// whole point of V107: under overlay/1 the lane is advanced by the dream write and the
    /// sync apply explicitly, never as a side effect of the physical write.
    #[test]
    fn a_raw_overlay_write_no_longer_bumps_the_memories_lane() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
            [],
        )
        .unwrap();
        let lane = || crate::meta::repo_meta(&conn, "r", crate::meta::LENS_MEMORIES_REVISION_META);
        let before = lane().unwrap();
        conn.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_at_ms)
             VALUES ('m', 'r', 'h', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, \
             generated_at_ms)
             VALUES ('m', 'r', 'h', 's', 0)",
            [],
        )
        .unwrap();
        assert_eq!(before, lane().unwrap(), "a raw overlay write must not move the lane");
    }

    /// V108 rebuilds `papertrail_distill` onto the thread natural key, drops the device-local `id`,
    /// and drops its papertrail-lane triggers — and stays that way across a full ladder replay
    /// (V093/V102 recreate the triggers ahead of V108 on every `index --full`).
    #[test]
    fn v108_rebuilds_distill_to_the_natural_key_and_drops_triggers_across_a_replay() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        assert_eq!(triggers_on(&conn, "papertrail_distill"), 0);
        assert_eq!(primary_key_columns(&conn, "papertrail_distill").unwrap(), [
            "repo_id",
            "tracker",
            "project",
            "item_kind",
            "item_key"
        ]);
        let has_id: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('papertrail_distill') WHERE name = 'id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_id, 0, "the device-local AUTOINCREMENT id is dropped");
    }

    /// The rebuild copies every row: a distilled record present in the pre-V108 shape survives,
    /// re-keyed by its natural key.
    #[test]
    fn v108_preserves_distilled_records_through_the_rebuild() {
        let conn = Connection::open_in_memory().unwrap();
        super::apply_distill_record_store(&conn).unwrap();
        // V079 adds the two columns the rebuild's INSERT..SELECT reads.
        super::apply_distill_safe_input_snapshot(&conn).unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  root_cause, fix_edge_source, thread_shape, distilled_at_ms, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 'sha256:in', 2, 'the cause', 'provider',
                     'investigation', 1, 'r')",
            [],
        )
        .unwrap();
        super::apply_syncable_distill_records(&conn).unwrap();
        let (cause, pipeline): (String, i64) = conn
            .query_row(
                "SELECT root_cause, pipeline_version FROM papertrail_distill
                 WHERE repo_id = 'r' AND tracker = 'github' AND project = 'o/r'
                   AND item_kind = 'issue' AND item_key = '7'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(cause, "the cause");
        assert_eq!(pipeline, 2);
        assert_eq!(primary_key_columns(&conn, "papertrail_distill").unwrap(), [
            "repo_id",
            "tracker",
            "project",
            "item_kind",
            "item_key"
        ]);
    }

    /// V109 rebuilds the edges + alternatives children onto their natural keys, dropping `id`,
    /// across a full ladder replay.
    #[test]
    fn v109_rebuilds_edges_and_alternatives_to_natural_keys_across_a_replay() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        assert_eq!(primary_key_columns(&conn, "papertrail_distill_edges").unwrap(), [
            "repo_id",
            "tracker",
            "project",
            "src_item_kind",
            "src_item_key",
            "dst_item_kind",
            "dst_item_key",
            "edge_kind"
        ]);
        assert_eq!(primary_key_columns(&conn, "papertrail_distill_alternatives").unwrap(), [
            "repo_id",
            "tracker",
            "project",
            "item_kind",
            "item_key",
            "ordinal"
        ]);
        for table in ["papertrail_distill_edges", "papertrail_distill_alternatives"] {
            let has_id: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = 'id'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(has_id, 0, "{table} drops the AUTOINCREMENT id");
        }
    }

    /// The rebuild copies every edge and alternative row, re-keyed by its natural key.
    #[test]
    fn v109_preserves_edge_and_alternative_rows_through_the_rebuild() {
        let conn = Connection::open_in_memory().unwrap();
        super::apply_distill_record_store(&conn).unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_edges
                 (tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                  edge_kind, created_at_ms, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 'change_request', '8', 'coalesced', 5, 'r')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_alternatives
                 (tracker, project, item_kind, item_key, ordinal, alternative, reason, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 0, 'do X', 'too slow', 'r')",
            [],
        )
        .unwrap();
        super::apply_syncable_distill_edges_and_alternatives(&conn).unwrap();
        let created_at: i64 = conn
            .query_row(
                "SELECT created_at_ms FROM papertrail_distill_edges
                 WHERE repo_id = 'r' AND edge_kind = 'coalesced'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created_at, 5);
        let (alternative, reason): (String, String) = conn
            .query_row(
                "SELECT alternative, reason FROM papertrail_distill_alternatives
                 WHERE repo_id = 'r' AND item_key = '7' AND ordinal = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(alternative, "do X");
        assert_eq!(
            reason, "too slow",
            "the nullable reason column is preserved through the rebuild"
        );
    }

    /// V110 rebuilds record_commits onto its natural key, drops `id`, and adds `created_at_ms`,
    /// across a full ladder replay.
    #[test]
    fn v110_rebuilds_record_commits_to_the_natural_key_across_a_replay() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        assert_eq!(primary_key_columns(&conn, "papertrail_distill_record_commits").unwrap(), [
            "repo_id",
            "tracker",
            "project",
            "item_kind",
            "item_key",
            "commit_sha"
        ]);
        let has_id: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('papertrail_distill_record_commits')
                 WHERE name = 'id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_id, 0, "the AUTOINCREMENT id is dropped");
        let has_created: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('papertrail_distill_record_commits')
                 WHERE name = 'created_at_ms'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_created, 1, "the created_at_ms non-key column is added");
    }

    /// The rebuild copies every commit link (legacy rows get created_at_ms = 0).
    #[test]
    fn v110_preserves_record_commit_rows_through_the_rebuild() {
        let conn = Connection::open_in_memory().unwrap();
        super::apply_distill_record_store(&conn).unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_record_commits
                 (tracker, project, item_kind, item_key, commit_sha, repo_id)
             VALUES ('github', 'o/r', 'issue', '7', 'abc123', 'r')",
            [],
        )
        .unwrap();
        super::apply_syncable_distill_record_commits(&conn).unwrap();
        let (sha, created): (String, i64) = conn
            .query_row(
                "SELECT commit_sha, created_at_ms FROM papertrail_distill_record_commits
                 WHERE repo_id = 'r' AND item_key = '7'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(sha, "abc123");
        assert_eq!(created, 0, "a legacy row gets created_at_ms = 0 until it is re-mined");
    }

    /// V111 rebuilds evidence onto its natural key with a per-thread ordinal, drops `id`, across a
    /// full ladder replay.
    #[test]
    fn v111_rebuilds_evidence_to_the_natural_key_with_an_ordinal_across_a_replay() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        assert_eq!(primary_key_columns(&conn, "papertrail_distill_evidence").unwrap(), [
            "repo_id",
            "tracker",
            "project",
            "item_kind",
            "item_key",
            "ordinal"
        ]);
        let has_id: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('papertrail_distill_evidence')
                 WHERE name = 'id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_id, 0, "the AUTOINCREMENT id is dropped");
    }

    /// The rebuild copies every evidence row and assigns per-thread ordinals in id order.
    #[test]
    fn v111_backfills_per_thread_evidence_ordinals_in_id_order() {
        let conn = Connection::open_in_memory().unwrap();
        super::apply_distill_record_store(&conn).unwrap();
        super::apply_distill_evidence_source_part(&conn).unwrap();
        // Two evidence rows on one thread, inserted in order → ordinals 0, 1.
        for quote in ["first", "second"] {
            conn.execute(
                "INSERT INTO papertrail_distill_evidence
                     (tracker, project, item_kind, item_key, field, source_kind, source_id,
                      byte_start, byte_end, quote, repo_id)
                 VALUES ('github', 'o/r', 'issue', '7', 'root_cause', 'item', '7', 0, 5, ?1, 'r')",
                [quote],
            )
            .unwrap();
        }
        super::apply_syncable_distill_evidence(&conn).unwrap();
        let rows: Vec<(i64, String)> = conn
            .prepare(
                "SELECT ordinal, quote FROM papertrail_distill_evidence
                 WHERE repo_id = 'r' AND item_key = '7' ORDER BY ordinal",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![(0, "first".to_string()), (1, "second".to_string())]);
    }
}

/// V095 (#1002): per-TABLE spec versioning, plus a direct pointer from a row to its winning entry.
///
/// `sync_published_rows.spec_version` replaces V093's `projector_version`. The anti-echo hash
///   covers one TABLE's synced column set, so recording the store-global projector version was too
///   coarse: registering an unrelated table (or learning a new op-kind) bumps that version and
/// would   mark EVERY table's rows incomparable, freezing their producers and forcing needless
/// winner   lookups. The projector version keeps its own job — deciding when parked entries are
/// replayed. The row's winning entry is NOT denormalized onto the clock: `sync_row_clocks` already
/// carries `(lamport, device_fingerprint)`, and `table_sync_entries` is UNIQUE on
/// `(stream_id, device_fingerprint, lamport)`, so the producer resolves it exactly once it knows
/// the stream — which it derives from the account it is already syncing. The only cost is
/// reconciling the two encodings of a fingerprint (hex TEXT on the clock, BLOB on the entry), which
/// the fingerprint type already round-trips.
///
/// The table is necessarily EMPTY: no table is registered (`SYNCABLE_TABLES` is empty) and there
/// is no transport, so nothing has ever written either. The published-rows table is therefore
/// rebuilt into its final shape rather than accumulating a dead column, and no backfill is owed.
pub fn apply_table_sync_spec_version(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    // The rebuild PRESERVES rows rather than dropping them, even though the table is provably empty
    // at this transition today. Emptiness is an emergent property of another crate
    // (`SYNCABLE_TABLES` has no entries yet) and the one-shot-ness depends on V093 declining to
    // re-add the column it replaced — a conjunction spanning two files that nothing enforces. A
    // bare DROP would make this migration's data safety rest on that conjunction holding
    // forever, and the failure would be silent and severe: `produce_row_ops` finds a
    // locally-deleted row by its surviving published record, so a wiped table strands every
    // unsent deletion and leaves peers holding the row. Copying instead demotes the guard below
    // to a performance detail, and costs nothing on an empty table.
    //
    // `spec_version` backfills to 0, which is below every real spec version (the registry lint
    // bounds those at >= 1), so a carried row reads as "column set unknown" — the not-comparable
    // path that resolves itself on the next produce. That is strictly better than deleting it.
    if !column_exists(&tx, "sync_published_rows", "spec_version")? {
        tx.execute_batch(
            "ALTER TABLE sync_published_rows RENAME TO sync_published_rows_pre_v095;
             CREATE TABLE sync_published_rows(
                 repo_id      TEXT    NOT NULL,
                 table_name   TEXT    NOT NULL,
                 row_pk       TEXT    NOT NULL,
                 synced_hash  TEXT    NOT NULL,
                 spec_version INTEGER NOT NULL,
                 PRIMARY KEY(repo_id, table_name, row_pk)
             ) STRICT;
             INSERT INTO sync_published_rows(repo_id, table_name, row_pk, synced_hash, \
             spec_version)
                 SELECT repo_id, table_name, row_pk, synced_hash, 0
                   FROM sync_published_rows_pre_v095;
             DROP TABLE sync_published_rows_pre_v095;",
        )?;
    }
    tx.commit()
}

/// V096 (#1058): retention for a table-sync entry whose chain predecessor has not arrived.
///
/// A verified entry that links to a predecessor this device does not hold used to be DROPPED, so a
/// chain delivered out of causal order — the normal condition on a transport — could only converge
/// through redelivery in exact order. It is now retained here until the predecessor is accepted,
/// then promoted through the ordinary accept-and-apply path.
///
/// This is deliberately its OWN table rather than a status column on `table_sync_entries`, because
/// six queries read that table as "the accepted chain" and every one of them must keep excluding an
/// entry that is not on a chain: the authoring Lamport clock and the lamport-advance bound (a
/// retained entry must not drag either), the chain tail (the promote target), entry existence (how
/// fork is told from gap), the LWW winner lookup, and the refold's pending set. A status column
/// would put a filter on each, and the one that got missed would fail silently.
///
/// `prev_hash` is NOT NULL: a genesis has no predecessor and so can never gap.
///
/// There is deliberately no `UNIQUE(stream_id, device_fingerprint, lamport)`. Two equivocating
/// entries at one lamport must both be storable, or the first one retained blocks the legitimate
/// one; identity here is the entry hash, and the conflict is resolved when the predecessor arrives
/// and one of them takes the successor slot. Accepted entries keep their own uniqueness — a losing
/// sibling never reaches `table_sync_entries`.
pub fn apply_table_sync_gapped_entries(conn: &Connection) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_gapped_entries(
             entry_hash         BLOB    NOT NULL PRIMARY KEY,
             stream_id          BLOB    NOT NULL,
             device_fingerprint BLOB    NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB    NOT NULL,
             signed_bytes       BLOB    NOT NULL,
             gapped_at_ms       INTEGER NOT NULL
         ) STRICT;
         -- Both walks of the promote loop: the child of the entry just accepted, and the siblings
         -- of its predecessor (which the acceptance just proved to be forks). The trailing
         -- (lamport, entry_hash) is the total order those walks take siblings in, so the index
         -- answers the ordering too instead of the query sorting a temp b-tree per probe.
         CREATE INDEX IF NOT EXISTS table_sync_gapped_entries_child
             ON table_sync_gapped_entries(
                 stream_id, device_fingerprint, prev_hash, lamport, entry_hash);
         -- The per-chain cap's count and its eviction victim, from an index tail instead of a \
         scan.
         CREATE INDEX IF NOT EXISTS table_sync_gapped_entries_chain_lamport
             ON table_sync_gapped_entries(stream_id, device_fingerprint, lamport);
         -- Children of a hash ACROSS devices: the abandoned-subtree walk and the cross-chain
         -- citation sweep, both of which run per accepted entry. The two indexes above lead with
         -- device_fingerprint and so answer neither — SQLite would fall back to the stream_id
         -- prefix and scan every held row on the stream, once per acceptance, which is quadratic
         -- over a reverse-delivered chain and is NOT bounded by the per-chain cap when several
         -- devices share the stream.
         --
         -- It carries device_fingerprint and entry_hash so it COVERS both probes. Without them the
         -- planner prefers the wider child index — which it can only use for its stream_id prefix,
         -- i.e. the scan this index exists to avoid — because that one is covering and this one
         -- would not be. Verified with EXPLAIN QUERY PLAN, not assumed.
         CREATE INDEX IF NOT EXISTS table_sync_gapped_entries_predecessor
             ON table_sync_gapped_entries(
                 stream_id, prev_hash, device_fingerprint, entry_hash);",
    )?;
    tx.commit()
}

/// V099 (#1049): account-authorized repository incarnations and incarnation-safe table streams.
///
/// The table-sync engine is still unreachable in production (`SYNCABLE_TABLES` is empty and no
/// transport exists), so legacy `/4` projection state has no peer-visible authority and cannot be
/// assigned an incarnation honestly. The migration retains only chain-tip witnesses, then clears
/// that unreachable state and rebuilds row bookkeeping with `stream_id` in every key.
pub fn apply_table_sync_repo_incarnations(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_repo_incarnation_current(
             account_id       BLOB NOT NULL CHECK(length(account_id) = 32),
             repository_id    TEXT NOT NULL,
             incarnation_ref  BLOB CHECK(incarnation_ref IS NULL OR length(incarnation_ref) = 32),
             PRIMARY KEY(account_id, repository_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS table_sync_chain_tips(
             stream_id          BLOB    NOT NULL CHECK(length(stream_id) = 32),
             device_fingerprint BLOB    NOT NULL CHECK(length(device_fingerprint) = 32),
             lamport            INTEGER NOT NULL,
             entry_hash         BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             PRIMARY KEY(stream_id, device_fingerprint)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS table_sync_chain_tips_stream_lamport
             ON table_sync_chain_tips(stream_id, lamport);",
    )?;

    // Rebuild accepted-chain high-water independently of the directory shape. A sanctioned full
    // schema replay may be repairing a dropped/incomplete witness table on an already-V099 store,
    // where `table_sync_streams.incarnation_ref` is present and the shape conversion below skips.
    conn.execute_batch(
        "INSERT INTO table_sync_chain_tips(stream_id, device_fingerprint, lamport, entry_hash)
         SELECT e.stream_id, e.device_fingerprint, e.lamport, e.entry_hash
           FROM table_sync_entries e
          WHERE NOT EXISTS (
                SELECT 1 FROM table_sync_entries newer
                 WHERE newer.stream_id = e.stream_id
                   AND newer.device_fingerprint = e.device_fingerprint
                   AND newer.lamport > e.lamport
          )
         ON CONFLICT(stream_id, device_fingerprint) DO UPDATE SET
             lamport = excluded.lamport, entry_hash = excluded.entry_hash
         WHERE excluded.lamport > table_sync_chain_tips.lamport;",
    )?;

    if !column_exists(conn, "table_sync_streams", "incarnation_ref")? {
        conn.execute_batch(
            "DELETE FROM table_sync_gapped_entries;
             DELETE FROM table_sync_entries;
             DROP TABLE table_sync_streams;
             CREATE TABLE table_sync_streams(
                 stream_id       BLOB NOT NULL PRIMARY KEY CHECK(length(stream_id) = 32),
                 repo_id         TEXT NOT NULL,
                 account_id      BLOB NOT NULL CHECK(length(account_id) = 32),
                 incarnation_ref BLOB NOT NULL CHECK(length(incarnation_ref) = 32),
                 scope_id        TEXT NOT NULL,
                 UNIQUE(repo_id, account_id, incarnation_ref, scope_id)
             ) STRICT;",
        )?;
    }
    if !column_exists(conn, "sync_published_rows", "stream_id")? {
        conn.execute_batch(
            "DROP TABLE sync_published_rows;
             CREATE TABLE sync_published_rows(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, synced_hash TEXT NOT NULL,
                 spec_version INTEGER NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;",
        )?;
    }
    if !column_exists(conn, "sync_row_clocks", "stream_id")? {
        conn.execute_batch(
            "DROP TABLE sync_row_clocks;
             CREATE TABLE sync_row_clocks(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, lamport INTEGER NOT NULL,
                 device_fingerprint TEXT NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;",
        )?;
    }
    if !column_exists(conn, "sync_row_tombstones", "stream_id")? {
        conn.execute_batch(
            "DROP TABLE sync_row_tombstones;
             CREATE TABLE sync_row_tombstones(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, lamport INTEGER NOT NULL,
                 device_fingerprint TEXT NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod table_sync_repo_incarnation_migration_tests {
    use super::*;

    #[test]
    fn legacy_table_sync_state_becomes_a_retained_witness_and_incarnation_scoped_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE table_sync_entries(
                 entry_hash BLOB PRIMARY KEY, stream_id BLOB NOT NULL,
                 device_fingerprint BLOB NOT NULL, lamport INTEGER NOT NULL,
                 prev_hash BLOB, signed_bytes BLOB NOT NULL, received_at_ms INTEGER NOT NULL
             );
             CREATE TABLE table_sync_gapped_entries(
                 entry_hash BLOB PRIMARY KEY, stream_id BLOB NOT NULL,
                 device_fingerprint BLOB NOT NULL, lamport INTEGER NOT NULL,
                 prev_hash BLOB NOT NULL, signed_bytes BLOB NOT NULL, gapped_at_ms INTEGER NOT NULL
             );
             CREATE TABLE table_sync_streams(
                 stream_id BLOB PRIMARY KEY, repo_id TEXT NOT NULL,
                 account_id BLOB NOT NULL, scope_id TEXT NOT NULL
             );
             CREATE TABLE sync_published_rows(
                 repo_id TEXT, table_name TEXT, row_pk TEXT, synced_hash TEXT, spec_version INTEGER
             );
             CREATE TABLE sync_row_clocks(
                 repo_id TEXT, table_name TEXT, row_pk TEXT, lamport INTEGER,
                 device_fingerprint TEXT
             );
             CREATE TABLE sync_row_tombstones(
                 repo_id TEXT, table_name TEXT, row_pk TEXT, lamport INTEGER,
                 device_fingerprint TEXT
             );
             INSERT INTO table_sync_streams VALUES(zeroblob(32), 'repo', zeroblob(32), 'demo/1');
             INSERT INTO table_sync_entries VALUES(
                 randomblob(32), zeroblob(32), zeroblob(32), 7, NULL, X'00', 0
             );",
        )
        .unwrap();

        apply_table_sync_repo_incarnations(&conn).unwrap();
        assert!(column_exists(&conn, "table_sync_streams", "incarnation_ref").unwrap());
        for table in ["sync_published_rows", "sync_row_clocks", "sync_row_tombstones"] {
            assert!(column_exists(&conn, table, "stream_id").unwrap(), "{table}");
        }
        let witnesses: i64 = conn
            .query_row("SELECT COUNT(*) FROM table_sync_chain_tips", [], |row| row.get(0))
            .unwrap();
        assert_eq!(witnesses, 1);
        let entries: i64 = conn
            .query_row("SELECT COUNT(*) FROM table_sync_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(entries, 0, "un-authorized /4 history is not assigned a /5 incarnation");
        assert!(!column_exists(&conn, "table_sync_chain_tips", "repo_id").unwrap());
    }

    #[test]
    fn full_ladder_replay_repairs_a_missing_chain_tip_table() {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        conn.execute(
            "INSERT INTO table_sync_entries(
                 entry_hash, stream_id, device_fingerprint, lamport, prev_hash, signed_bytes,
                 received_at_ms
             ) VALUES (?1, ?2, ?3, 3, NULL, X'00', 0)",
            rusqlite::params![[3u8; 32].as_slice(), [1u8; 32].as_slice(), [2u8; 32].as_slice()],
        )
        .unwrap();
        conn.execute_batch("DROP TABLE table_sync_chain_tips").unwrap();

        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        let restored: (i64, Vec<u8>) = conn
            .query_row(
                "SELECT lamport, entry_hash FROM table_sync_chain_tips
                  WHERE stream_id = ?1 AND device_fingerprint = ?2",
                rusqlite::params![[1u8; 32].as_slice(), [2u8; 32].as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(restored, (3, vec![3u8; 32]));

        crate::schema::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM table_sync_chain_tips", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "accepted-tip repair is idempotent");
    }
}

/// Every table whose `worktree_id` column holds a CHECKOUT PATH — the scope key `worktree_id_of`
/// derives by canonicalizing a checkout directory. `files` is the load-bearing one (its
/// `(commit_sha, worktree_id)` pair is the active-scope view every read goes through); the other
/// three are keyed the same way, and GC prunes all four off the same live worktree set.
///
/// `migration_097_covers_every_worktree_id_column_in_the_schema` pins this list against the live
/// schema, so a table that grows a `worktree_id` cannot silently miss the rekey.
pub const V097_WORKTREE_ID_SCOPED_TABLES: &[&str] =
    &["files", "packages", "oracle_runs", "external_symbols"];

/// The `repo_meta` key prefix whose SUFFIX is a `worktree_id` — the overlay refresh basis, THE
/// same constant `rag-rat-core`'s overlay reads and writes. Rekeying the row's VALUE is not enough
/// here: the worktree identity is in the KEY, so a stale key is a basis record the rekeyed scope
/// can never find.
const V097_WORKTREE_OVERLAY_BASIS_PREFIX: &str = crate::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX;

/// Every meta key whose VALUE is a checkout path — the `repo_meta` / `index_meta` rows a freshly
/// canonicalized root is compared against TEXTUALLY.
///
/// Deliberately NOT here, having been checked one by one: `git_history_indexed_head` and
/// `git_commit` (commit hashes), `git_history_indexed_shallow` / `_complete` (flags),
/// `local_crate_roots` (Cargo crate NAMES), and everything model/embedding/FTS-related (ids,
/// versions, counters). `files.path` and `packages.manifest_dir` are stored RELATIVE to the root,
/// so a root respelling never reaches them.
///
/// `pub` for the same reason as [`V097_WORKTREE_ID_SCOPED_TABLES`]: a meta key is just a string, so
/// nothing about adding a path-valued one fails to compile.
/// `every_absolute_path_in_the_meta_bag_is_rekeyed_or_reviewed` walks a real index and requires
/// every absolute-path value to be either in this list or in an explicitly reviewed exception set.
pub const V097_PATH_VALUED_META_KEYS: &[&str] =
    &["source_root", crate::meta::GIT_HISTORY_INDEXED_ROOT_META];

/// The `repo_meta` freshness markers V098 deletes to force the next ordinary index pass to
/// re-derive the path-keyed rows an older binary keyed under a collapsed backslash spelling.
/// Deleting the marker is the lever, not deleting the table: each gates a re-derivation the indexer
/// already owns. `BASE_SCOPE_DISCOVERED_META` gone promotes the next pass to a full tree re-walk,
/// which replaces `files` and everything that CASCADES from it (chunks, symbols, edges, embeddings,
/// blame). `GIT_HISTORY_INDEXED_ROOT_META` gone fails `is_history_current`, forcing a full revwalk
/// that re-reads the commit and file-change rows and restamps the change couplings folded off its
/// freshness key. The `worktree_overlay_basis` keys are cleared separately (their suffix is a
/// worktree_id, so they need a prefix match, not an equality).
const V098_CLEARED_FRESHNESS_KEYS: &[&str] =
    &[crate::meta::BASE_SCOPE_DISCOVERED_META, crate::meta::GIT_HISTORY_INDEXED_ROOT_META];

/// V098 (#1032): make the next ordinary `index` pass re-walk the tree and reload git history, so a
/// store written before the Unix backslash-rendering fix re-derives its path-keyed rows off the
/// corrected spelling.
///
/// The pre-fix binary rendered a path by replacing every backslash with a separator — right on
/// Windows, where a backslash IS the separator, but wrong on Unix, where a literal backslash is an
/// ordinary filename byte. So `foo\bar.rs` was persisted as `foo/bar.rs`: one `files.path`, one
/// `path::name` symbol identity, shared with a genuinely nested `foo/bar.rs`. That rendering was
/// LOSSY, so unlike the Windows verbatim rekey (V097) the stored spelling cannot be repaired in
/// place — `foo/bar.rs` in the store cannot be told apart from a rewritten `foo\bar.rs`. Only a
/// re-walk off the corrected renderer recovers the truth, and this forces one by deleting the
/// freshness markers that would otherwise let the pass skip an unchanged file.
///
/// WHAT KEEPS A PRE-FIX BINARY OFF A STORE THIS HAS CONVERTED — and why that is the whole story.
/// The fence is the SCHEMA VERSION, the ladder's, not this migration's: recording V098 puts an id
/// in `schema_version` a pre-V098 binary does not know, so [`super::status`] answers `Newer` and
/// every open refuses. It reaches a resident process wherever it re-opens — a watcher pass, a CLI
/// or MCP read — because those re-open per operation. This is the SAME fence V097 relies on for the
/// sibling bug; that reasoning applies here unchanged, including the one bounded residual it
/// documents: a pass already past its status check when the upgrade commits can still write once
/// under the old rendering, self-healing on the following pass. That residual is accepted rather
/// than closed with cross-repo write-lock enumeration, which a consolidated store cannot order.
///
/// LEDGER-ATOMIC because the marker deletion and the `schema_version` stamp must never be
/// separately visible: a store whose markers are gone but whose V098 row has not landed still
/// answers `Compatible` to a pre-V098 binary, which would re-walk under the OLD renderer and
/// re-collapse the very spellings this exists to correct — for a moment on a healthy upgrade,
/// indefinitely after a crash between two commits. So the body takes the ladder's transaction and
/// opens none of its own.
///
/// DELIBERATELY LEFT to a read-time filter or an ordinary later pass, not swept here:
///  * the oracle verdict tables — `edge_oracle` is joined on `file_sha = files.sha256`, so a stale
///    row only resurfaces against a real sibling of identical content and edge geometry, and it
///    re-derives on the next `oracle run`;
///  * the clone posting family — rotated out by the next clone generation;
///  * durable path-anchored DECISION data — a persisted human/model choice keyed by a path, which a
///    reindex must not destroy. Two tables hold it today: `repo_memory_bindings` (re-anchored by
///    the relocation engine) and `papertrail_distill_anchors` rows with `selected = 1` (a model
///    selection the distill extractor preserves across a rerun of unchanged input, by its own V078
///    invariant — so quarantining or regenerating them here would lose the decision, which is why
///    the reindex leaves them). Any future table of this shape belongs in this bucket, not in the
///    sweep.
///
/// Reaching any of those needs a lossy collision with a real slash-spelled sibling AND a
/// pre-existing backslash-named Unix file — the near-impossible case the correctness fix stops from
/// ever recurring. Without a sibling the stale row simply points at a path no file has, and is
/// inert.
///
/// Runs on every platform: which spellings a store carries is a property of the store, not of the
/// host reading it, and the ladder is forward-only, so a skip would record V098 as applied without
/// doing the work.
pub fn apply_reindex_after_unix_backslash_rendering(conn: &Connection) -> rusqlite::Result<()> {
    for key in V098_CLEARED_FRESHNESS_KEYS {
        conn.execute("DELETE FROM repo_meta WHERE key = ?1", [key])?;
    }
    // The overlay basis lives under one key PER checkout, its worktree_id in the suffix — a prefix
    // match clears them all. The constant holds no `%`/`_`/`\`, so a bare LIKE needs no ESCAPE.
    conn.execute("DELETE FROM repo_meta WHERE key LIKE ?1 || '%'", [
        crate::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX,
    ])?;
    // The one path-keyed DERIVED table a file re-walk does not cascade: no FK to `files`, keyed by
    // a bare path, re-recorded per file on the next parse. Whole-table because every row
    // re-derives.
    conn.execute("DELETE FROM parser_failures", [])?;
    Ok(())
}

/// V097 (#1048): rewrite every persisted path spelling that the pre-fix `canonicalize` wrote in
/// the Windows `\\?\` VERBATIM form into the plain spelling this binary now produces.
///
/// The upgrade hazard this closes is silent on Windows. These stored strings are compared
/// TEXTUALLY against a freshly-canonicalized path:
///  * `worktree_id` — a canonicalized checkout path, carried by every linked worktree's overlay
///    rows and every dirty row (committed base rows are shared across checkouts under `worktree_id
///    = ''` and are not implicated). Once production answers `C:\…` and the rows still say
///    `\\?\C:\…`, those rows fall out of the active scope AND out of the GC live set, which is
///    built from the same fresh canonicalization: `garbage_collect` reads every stored id as a
///    checkout that no longer exists and DELETES its rows. Registered, live worktrees, pruned as
///    dead on the first maintenance pass after the upgrade.
///  * `repo_roots.root` / `repo_meta[source_root]` — `repo_indexed_at_this_root` is the "this
///    checkout was indexed here" signal behind the empty-index guard. A stale spelling makes an
///    established checkout look first-time, so an index run whose files have just been deleted is
///    refused as an accidental empty repo instead of pruning, and the deleted files' rows stay live
///    until the user finds `--allow-empty`.
///  * `repo_meta[git_history_indexed_root]` — the git-history reload gate's root cursor. A stale
///    spelling fails the `is_history_current` / `prepare_plan` comparison, so the first pass after
///    the upgrade takes the FULL path: the whole commit + file-change set is deleted and re-read
///    off a fresh revwalk, and the repo's blame cache is wiped with it. Self-healing after one
///    pass, but a minutes-long stall and a cold blame cache on a large repo.
///
/// One derived value is knowingly left to re-derive: `repo_meta[git_coupling_stamp]` folds the
/// history cursor snapshot (root spelling included) into its own freshness key, so rekeying the
/// cursor makes it stale. That is the change-coupling table's ordinary invalidation path — a
/// bounded window recompute, with an in-memory fallback that keeps reads correct meanwhile — and
/// the same recompute happens anyway on the next history apply. Rewriting a composite freshness
/// stamp from a migration would buy nothing and couple the ladder to that stamp's format.
///
/// Rewriting goes through `paths::rekeyed_from_verbatim`, the SAME rule production canonicalizes
/// with, not a blind prefix strip: verbatim form is still produced (and still correct) for UNC
/// shares, paths past `MAX_PATH`, and reserved DOS names, and rewriting those would BREAK the match
/// this exists to preserve.
///
/// WHAT KEEPS A PRE-UPGRADE BINARY OFF A STORE THIS HAS CONVERTED. Rekeying is only safe if no
/// binary that predates it can still write to, or garbage-collect, the rekeyed rows — an older
/// build derives the live worktree set from the OLD spelling, so it would read every rekeyed id as
/// a checkout that no longer exists. The fence is the SCHEMA VERSION, and it is the ladder's, not
/// this migration's: recording V097 puts a migration id in `schema_version` that a pre-V097 binary
/// does not know, so [`super::status`] answers `Newer` and every open refuses. It covers a RESIDENT
/// process, not just a fresh command, WHEREVER THAT PROCESS RE-OPENS, which every path that
/// indexes, queries, or garbage-collects does per operation: a watcher pass re-opens through
/// `open_and_migrate` at its start, inside the per-repo write flock and long before its gc stage,
/// so the pass after the upgrade fails at the gate with nothing written; the lighter watch-counter
/// flush tests `status() == Compatible` on its own connection before writing; CLI and MCP reads go
/// through `open_and_migrate` or `try_open_config_read_only`.
///
/// ONE resident writer does hold a connection across that check, and it is the reason this
/// paragraph is not a general rule: `sync serve` / `sync init` open the index once and then run the
/// accept loop on that same connection for the process's whole life, ingesting peer op-log entries
/// without re-checking the schema. A server started before the upgrade keeps writing to a store a
/// newer binary has converted. That is outside THIS migration's hazard for reasons specific to what
/// the loop writes, not because the fence reaches it: no table is registered for table-sync
/// (`SYNCABLE_TABLES` is empty), `repo_roots` is written only by the indexing path's
/// `register_repo`, and the loop runs no gc and derives no live-worktree set — so it can neither
/// prune a rekeyed row nor write a path-spelled column back in the old spelling. A FUTURE
/// data-converting migration over a table the op-log projection or table-sync touches must re-check
/// that for itself rather than inherit this conclusion.
///
/// That fence only holds if the conversion and the stamp are never separately visible, so this
/// migration is in `LEDGER_ATOMIC_MIGRATIONS`: the ladder runs the sweep inside the same IMMEDIATE
/// transaction that writes the `schema_version` row. Committed separately, the rekeyed store would
/// answer `Compatible` to a pre-V097 binary until the stamp landed — for a moment on a healthy
/// upgrade, indefinitely after a crash between the two commits — and every refusal above would wave
/// that binary through onto rows it reads as dead checkouts.
///
/// The one case the version cannot fence is a pass ALREADY past that check when the upgrade
/// commits. A new binary's INDEXING opens cannot cause it (they take the per-repo write flock
/// before migrating, so they wait behind the in-flight pass); only a non-indexing open — a query,
/// an MCP read — migrates under the global schema lock alone, which by design does not serialize
/// against per-repo writers. Deliberately left: the migration must not take per-repo flocks it
/// would have to enumerate and order across every repo in a consolidated store, and the exposure
/// is bounded — the rows at risk are derived overlay/dirty rows, which the next overlay refresh
/// re-derives, and committed base rows (`worktree_id = ''`, a live `commit_sha`) are outside it.
///
/// IT RUNS ON EVERY PLATFORM, not only on Windows. Which spellings a store carries is a property of
/// the STORE, not of the host reading it: one repository directory reachable from both a Windows
/// path and a WSL/container mount is one SQLite file, and whichever binary opens first is the one
/// that runs the ladder. Skipping the sweep off Windows would let that first opener record V097 as
/// applied without converting anything — and the ladder is forward-only, so the Windows binary
/// would never revisit it and would keep the spellings that get its rows collected as a dead
/// checkout, which is the failure this migration exists to prevent. `rekeyed_from_verbatim` decides
/// droppability textually for that reason. The cost of dropping the host skip is small and was
/// measured rather than assumed: the sweep's reads are `SELECT DISTINCT` over four `worktree_id`
/// columns, and `files` — the only large one — answers from `idx_files_worktree_path` as a covering
/// scan; on a ~2 GB twenty-repo store the whole sweep is tens of milliseconds, once, on the open
/// that upgrades it.
pub fn apply_windows_verbatim_path_rekey(conn: &Connection) -> rusqlite::Result<()> {
    rekey_persisted_path_spellings(conn, rag_rat_base::paths::rekeyed_from_verbatim)
}

/// [`apply_windows_verbatim_path_rekey`] with the spelling rule injected.
///
/// The rule is a parameter so the ROW-WALKING half — which columns and which meta keys the pass
/// covers — can be driven over a REAL index built on the host running the test. The production rule
/// only ever rewrites the Windows verbatim shape, which no checkout on a Unix CI runner is spelled
/// in; injecting a rule that maps the spellings such an index actually holds is what lets the Linux
/// leg observe the scope-restore and the GC-survival, rather than observing "nothing happened" and
/// passing just as well against a pass that covers no tables at all. `pub` for that reason alone.
///
/// No transaction of its own: the ladder runs this migration inside one and commits it WITH the
/// `schema_version` row, because a converted store that is not yet stamped still answers
/// `Compatible` to the pre-V097 binary the conversion locks out. Opening a nested transaction here
/// would fail outright, and committing one would reintroduce that window.
pub fn rekey_persisted_path_spellings(
    conn: &Connection,
    rekey: fn(&str) -> Option<String>,
) -> rusqlite::Result<()> {
    for table in V097_WORKTREE_ID_SCOPED_TABLES {
        if column_exists(conn, table, "worktree_id")? {
            rekey_column(conn, table, "worktree_id", rekey)?;
        }
    }
    if column_exists(conn, "repo_roots", "root")? {
        rekey_column(conn, "repo_roots", "root", rekey)?;
    }
    // These keys land in `repo_meta` at V039; a pre-V039 store still carries them in the global
    // `index_meta`, and the ladder replays in order, so by here they have moved. Both are swept
    // anyway — the pass is idempotent and a stale copy either table kept is equally stale. The
    // sweep is KEY-SCOPED: most meta values are not paths, and rewriting one that merely starts
    // with those bytes would corrupt it.
    for (table, column) in [("repo_meta", "value"), ("index_meta", "value")] {
        if column_exists(conn, table, column)? {
            for key in V097_PATH_VALUED_META_KEYS {
                rekey_meta_value_at_key(conn, table, key, rekey)?;
            }
        }
    }
    if column_exists(conn, "repo_meta", "key")? {
        rekey_worktree_overlay_basis_keys(conn, rekey)?;
    }
    Ok(())
}

/// Rewrite the stale spellings in `table.column` in place.
///
/// Reads the DISTINCT values first: a `worktree_id` column has one value per checkout however many
/// million rows carry it, so the rule runs a handful of times and each rewrite is one indexed
/// UPDATE.
///
/// `UPDATE OR IGNORE` because the target spelling can already be present. On the real upgrade it
/// cannot be — the old binary only ever wrote the verbatim form — but a store written by a MIX of
/// binaries can hold both, and there the plain-spelled row is the one production just wrote and
/// must win. Skipping leaves the verbatim row for GC, which is the correct disposition for a
/// superseded duplicate; the alternatives are worse in both directions (`OR REPLACE` would cascade
/// the live row's children away, a bare UPDATE would abort the upgrade on a constraint failure).
fn rekey_column(
    conn: &Connection,
    table: &str,
    column: &str,
    rekey: fn(&str) -> Option<String>,
) -> rusqlite::Result<()> {
    let stored: Vec<String> = {
        let mut stmt = conn
            .prepare(&format!("SELECT DISTINCT {column} FROM main.{table} WHERE {column} != ''"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for old in stored {
        let Some(new) = rekey(&old) else { continue };
        conn.execute(
            &format!("UPDATE OR IGNORE main.{table} SET {column} = ?1 WHERE {column} = ?2"),
            params![new, old],
        )?;
    }
    Ok(())
}

/// Rewrite the VALUE of a single meta row whose key is `key` — the `source_root` case, where the
/// path is the value rather than part of the key.
fn rekey_meta_value_at_key(
    conn: &Connection,
    table: &str,
    key: &str,
    rekey: fn(&str) -> Option<String>,
) -> rusqlite::Result<()> {
    let stored: Vec<String> = {
        let mut stmt =
            conn.prepare(&format!("SELECT DISTINCT value FROM main.{table} WHERE key = ?1"))?;
        let rows = stmt.query_map([key], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for old in stored {
        let Some(new) = rekey(&old) else { continue };
        conn.execute(
            &format!("UPDATE OR IGNORE main.{table} SET value = ?1 WHERE key = ?2 AND value = ?3"),
            params![new, key, old],
        )?;
    }
    Ok(())
}

/// Rewrite the overlay-basis rows whose KEY embeds a stale `worktree_id`
/// (`worktree_overlay_basis:<worktree_id>`).
///
/// Left behind, the basis record is unreachable under the rekeyed scope, and the overlay refresh
/// reads a missing basis as "never refreshed" — correct but wasteful (a full re-derive per linked
/// checkout). The GC that prunes basis rows outside the live worktree set would then delete it,
/// which is harmless once the row is orphaned but leaves the quiet-window anchor gone. Moving the
/// key keeps the basis attached to the checkout it describes.
fn rekey_worktree_overlay_basis_keys(
    conn: &Connection,
    rekey: fn(&str) -> Option<String>,
) -> rusqlite::Result<()> {
    let stored: Vec<String> = {
        // `\` is not a LIKE metacharacter in SQLite, but `_` in the prefix is — match on the
        // literal prefix with `substr` instead of relying on an ESCAPE clause.
        let mut stmt =
            conn.prepare("SELECT DISTINCT key FROM main.repo_meta WHERE substr(key, 1, ?1) = ?2")?;
        let prefix_len = V097_WORKTREE_OVERLAY_BASIS_PREFIX.len() as i64;
        let rows = stmt
            .query_map(params![prefix_len, V097_WORKTREE_OVERLAY_BASIS_PREFIX], |row| {
                row.get::<_, String>(0)
            })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for old_key in stored {
        let Some(worktree_id) = old_key.strip_prefix(V097_WORKTREE_OVERLAY_BASIS_PREFIX) else {
            continue;
        };
        let Some(rekeyed) = rekey(worktree_id) else { continue };
        conn.execute("UPDATE OR IGNORE main.repo_meta SET key = ?1 WHERE key = ?2", params![
            format!("{V097_WORKTREE_OVERLAY_BASIS_PREFIX}{rekeyed}"),
            old_key
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod windows_verbatim_rekey_tests {
    use super::*;

    /// The stand-in spelling rule: a blind prefix strip. Used for the ROW-WALKING assertions —
    /// which columns and which meta keys the sweep reaches — so those stay legible against a rule
    /// with one obvious answer per input, and so the two halves fail separately. Which spellings
    /// the PRODUCTION rule declines is `paths`' concern and is asserted there;
    /// `the_production_pass_rekeys_a_windows_store_on_any_host` covers the two composed.
    fn strip_verbatim(stored: &str) -> Option<String> {
        stored.strip_prefix(r"\\?\").map(str::to_string)
    }

    /// A store shaped like a pre-V097 Windows index: every scope key and recorded root spelled the
    /// way `std::fs::canonicalize` used to answer.
    fn poisoned_store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r"
            CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT NOT NULL, commit_sha TEXT NOT NULL
                DEFAULT '', worktree_id TEXT NOT NULL DEFAULT '',
                UNIQUE(path, commit_sha, worktree_id));
            CREATE TABLE packages(manifest_dir TEXT NOT NULL, commit_sha TEXT NOT NULL DEFAULT '',
                worktree_id TEXT NOT NULL DEFAULT '');
            CREATE TABLE oracle_runs(id INTEGER PRIMARY KEY, worktree_id TEXT NOT NULL DEFAULT '');
            CREATE TABLE external_symbols(name TEXT NOT NULL, worktree_id TEXT NOT NULL);
            CREATE TABLE repo_roots(repo_id TEXT NOT NULL, root TEXT NOT NULL,
                PRIMARY KEY(repo_id, root));
            CREATE TABLE repo_meta(repo_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                PRIMARY KEY(repo_id, key));
            CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);

            INSERT INTO files(path, commit_sha, worktree_id)
                VALUES ('src/a.rs', 'headsha', '\\?\C:\repo'),
                       ('src/b.rs', '', '\\?\C:\linked');
            INSERT INTO packages VALUES ('crate', 'headsha', '\\?\C:\repo');
            INSERT INTO oracle_runs(worktree_id) VALUES ('\\?\C:\repo');
            INSERT INTO external_symbols VALUES ('Ext', '\\?\C:\repo');
            INSERT INTO repo_roots VALUES ('repo-1', '\\?\C:\repo');
            INSERT INTO repo_meta VALUES ('repo-1', 'source_root', '\\?\C:\repo');
            INSERT INTO repo_meta VALUES ('repo-1', 'git_history_indexed_root', '\\?\C:\repo');
            -- The reload cursor's SIBLING key, given a value the rule would happily rewrite so a
            -- blanket value-sweep would visibly corrupt it. In a real store this holds a commit
            -- hash; the sweep must be scoped to the keys that carry a path.
            INSERT INTO repo_meta VALUES ('repo-1', 'git_history_indexed_head', '\\?\C:\repo');
            INSERT INTO repo_meta VALUES
                ('repo-1', 'worktree_overlay_basis:\\?\C:\linked',
                 'base' || char(10) || 'linked' || char(10) || '42');
            INSERT INTO index_meta VALUES ('source_root', '\\?\C:\repo');
            ",
        )
        .unwrap();
        conn
    }

    fn scalar(conn: &Connection, sql: &str) -> String {
        conn.query_row(sql, [], |row| row.get::<_, String>(0)).unwrap()
    }

    /// The whole class in one pass: every table whose `worktree_id` names a checkout, the recorded
    /// root behind the empty-index guard, the `source_root` fallback beside it, the git-history
    /// reload cursor's root, and the overlay basis whose worktree identity lives in the KEY — plus
    /// the negative half, a meta key that is NOT a path staying put.
    ///
    /// Against the unfixed state (no migration at all) every one of these still reads the verbatim
    /// spelling, which is precisely the state where the active scope selects nothing and GC prunes
    /// the rows as dead.
    #[test]
    fn the_rekey_covers_every_persisted_path_spelling() {
        let conn = poisoned_store();
        rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();

        for (table, column) in [
            ("files", "worktree_id"),
            ("packages", "worktree_id"),
            ("oracle_runs", "worktree_id"),
            ("external_symbols", "worktree_id"),
        ] {
            let remaining: i64 = conn
                .query_row(
                    &format!(r"SELECT COUNT(*) FROM {table} WHERE substr({column}, 1, 4) = '\\?\'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(remaining, 0, "{table}.{column} still holds a verbatim spelling");
        }
        assert_eq!(
            scalar(&conn, "SELECT worktree_id FROM files WHERE path = 'src/a.rs'"),
            r"C:\repo",
            "the base index's scope key is rekeyed, not just the overlay's",
        );
        assert_eq!(
            scalar(&conn, "SELECT worktree_id FROM files WHERE path = 'src/b.rs'"),
            r"C:\linked",
        );
        assert_eq!(scalar(&conn, "SELECT root FROM repo_roots"), r"C:\repo");
        assert_eq!(
            scalar(&conn, "SELECT value FROM repo_meta WHERE key = 'source_root'"),
            r"C:\repo",
        );
        assert_eq!(
            scalar(&conn, "SELECT value FROM index_meta WHERE key = 'source_root'"),
            r"C:\repo",
        );
        assert_eq!(
            scalar(&conn, "SELECT value FROM repo_meta WHERE key = 'git_history_indexed_root'"),
            r"C:\repo",
            "the git-history reload cursor is a ROOT PATH, not a commit hash — left stale it \
             fails the freshness comparison and forces a full revwalk plus a blame-cache wipe",
        );
        assert_eq!(
            scalar(&conn, "SELECT value FROM repo_meta WHERE key = 'git_history_indexed_head'"),
            r"\\?\C:\repo",
            "a meta key that does not carry a path is left alone — the sweep is key-scoped, not a \
             blanket rewrite of every value that happens to start with those bytes",
        );
        assert_eq!(
            scalar(&conn, "SELECT key FROM repo_meta WHERE key LIKE 'worktree_overlay_basis:%'",),
            r"worktree_overlay_basis:C:\linked",
            "the basis key carries the worktree identity, so the KEY must move with it",
        );
        assert_eq!(
            scalar(&conn, "SELECT value FROM repo_meta WHERE key LIKE 'worktree_overlay_basis:%'",),
            "base\nlinked\n42",
            "the basis payload rides the key move intact",
        );
    }

    /// Re-running the pass changes nothing — the ladder can replay it, and a store already written
    /// by a fixed binary must come through untouched.
    #[test]
    fn the_rekey_is_idempotent() {
        let conn = poisoned_store();
        rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();
        let snapshot =
            scalar(&conn, "SELECT group_concat(worktree_id, '|') FROM files ORDER BY id");
        rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();
        assert_eq!(
            scalar(&conn, "SELECT group_concat(worktree_id, '|') FROM files ORDER BY id"),
            snapshot,
        );
    }

    /// A spelling the rule declines to rewrite is LEFT ALONE. This is the half a blind `\\?\`
    /// strip would get wrong: verbatim form is still produced for UNC shares and reserved names,
    /// and rewriting one would break the very match the migration exists to preserve.
    #[test]
    fn a_spelling_the_rule_declines_is_untouched() {
        let conn = poisoned_store();
        rekey_persisted_path_spellings(&conn, |_| None).unwrap();
        assert_eq!(
            scalar(&conn, "SELECT worktree_id FROM files WHERE path = 'src/a.rs'"),
            r"\\?\C:\repo",
            "a declined rewrite must not be applied anyway",
        );
        assert_eq!(scalar(&conn, "SELECT root FROM repo_roots"), r"\\?\C:\repo");
    }

    /// A mixed-binary store already holds the plain spelling for one of the rows being rekeyed.
    /// The pass must not abort the upgrade on the unique constraint, and must not cascade the live
    /// plain-spelled row away: it keeps that row and leaves the superseded verbatim duplicate for
    /// GC.
    #[test]
    fn a_colliding_plain_row_survives_the_rekey() {
        let conn = poisoned_store();
        conn.execute(
            r"INSERT INTO files(path, commit_sha, worktree_id) VALUES ('src/a.rs', 'headsha', 'C:\repo')",
            [],
        )
        .unwrap();
        rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();
        let live: i64 = conn
            .query_row(
                r"SELECT COUNT(*) FROM files WHERE path = 'src/a.rs' AND worktree_id = 'C:\repo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live, 1, "the row production just wrote is intact, not replaced or cascaded");
    }

    /// The real entry point, end-to-end, on WHICHEVER platform runs it — deliberately not
    /// `cfg`-gated.
    ///
    /// Which spellings a store holds is a property of the store, not of the host that opens it: one
    /// repository directory reachable from both a Windows path and a WSL/container mount is one
    /// SQLite file, and whichever binary opens first is the one that runs the ladder. A pass that
    /// converted only on Windows would let a non-Windows opener stamp V097 as applied without
    /// converting anything, and the forward-only ladder would never revisit it — leaving the
    /// Windows binary with exactly the spellings whose rows its next GC prunes as a dead checkout.
    ///
    /// So this asserts the conversion on Unix too, where the host-gated version returned before
    /// opening the transaction and left every value below verbatim.
    #[test]
    fn the_production_pass_rekeys_a_windows_store_on_any_host() {
        let conn = poisoned_store();
        apply_windows_verbatim_path_rekey(&conn).unwrap();
        assert_eq!(
            scalar(&conn, "SELECT worktree_id FROM files WHERE path = 'src/a.rs'"),
            r"C:\repo",
        );
        assert_eq!(scalar(&conn, "SELECT root FROM repo_roots"), r"C:\repo");
        assert_eq!(
            scalar(&conn, "SELECT value FROM repo_meta WHERE key = 'git_history_indexed_root'"),
            r"C:\repo",
        );
        assert_eq!(
            scalar(&conn, "SELECT key FROM repo_meta WHERE key LIKE 'worktree_overlay_basis:%'"),
            r"worktree_overlay_basis:C:\linked",
        );
    }

    /// A store that predates a covered table (a partial-schema bootstrap fixture) must not abort
    /// the ladder — every sweep is guarded on the column actually being there.
    #[test]
    fn a_missing_table_is_skipped_rather_than_failing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r"CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT NOT NULL,
                  worktree_id TEXT NOT NULL DEFAULT '');
              INSERT INTO files(path, worktree_id) VALUES ('a.rs', '\\?\C:\repo');",
        )
        .unwrap();
        rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();
        assert_eq!(scalar(&conn, "SELECT worktree_id FROM files"), r"C:\repo");
    }
}

/// V100 (#976): intern the Rust receiver-type hint and make call-path identity target-aware.
///
/// Conservative receiver-type inference records the type a method call was made ON, so resolution
/// can bind `worker.run()` to `Worker::run` instead of every `run` in the repo. The value is an
/// interned `name_strings` id, like the sibling name columns, because the same handful of type
/// paths repeat across every call site in a file. The `edges` view is rebuilt so readers see the
/// new column without a second migration.
pub fn apply_receiver_type_hint_interning(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges_data", "receiver_type_hint_id", "INTEGER")?;
    add_column_if_missing(
        conn,
        "repo_memory_call_path_edges",
        "callee_logical_symbol_id",
        "INTEGER",
    )?;
    add_column_if_missing(
        conn,
        "repo_memory_call_path_edges",
        "callee_identity_known",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_edges_view(conn)?;
    Ok(())
}

/// V101 (#1014): record which graph extractor version produced each file row.
///
/// Existing rows inherit the repository stamp they previously relied on. A later extractor bump
/// can then mark only rows whose exact bytes are readable from the active checkout, leaving a
/// divergent linked-worktree row owed until that checkout opens the shared database itself.
pub fn apply_file_graph_version_provenance(conn: &Connection) -> rusqlite::Result<()> {
    let had_graph_version = column_exists(conn, "files", "graph_version")?;
    let had_scope_version = column_exists(conn, "files", "scope_version")?;
    add_column_if_missing(conn, "files", "graph_version", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "files", "scope_version", "INTEGER NOT NULL DEFAULT 0")?;
    let has_repo_meta = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'repo_meta')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !had_graph_version && column_exists(conn, "files", "repo_id")? && has_repo_meta {
        conn.execute(
            "UPDATE files
             SET graph_version = COALESCE((
                 SELECT CAST(value AS INTEGER)
                 FROM repo_meta
                 WHERE repo_meta.repo_id = files.repo_id
                   AND repo_meta.key = 'graph_index_version'
             ), 0)",
            [],
        )?;
    }
    if !had_scope_version && column_exists(conn, "files", "repo_id")? && has_repo_meta {
        conn.execute(
            "UPDATE files
             SET scope_version = COALESCE((
                 SELECT CAST(value AS INTEGER)
                 FROM repo_meta
                 WHERE repo_meta.repo_id = files.repo_id
                   AND repo_meta.key = 'logical_key_version'
             ), 0)",
            [],
        )?;
    }
    if column_exists(conn, "files", "repo_id")? && column_exists(conn, "files", "generation")? {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_files_repo_generation_graph_version
                 ON files(repo_id, generation, graph_version);
             CREATE INDEX IF NOT EXISTS idx_files_repo_generation_scope_version
                 ON files(repo_id, generation, scope_version);",
        )?;
    }
    Ok(())
}

/// V103 (#1109): make memory bindings a deterministic whole-row table for `anchors/1`.
///
/// The old shape was non-STRICT, keyed without `repo_id`, and cascaded from `repo_memories`.
/// Those properties are incompatible with table sync: repository identity must be enforced by the
/// row key, cross-row constraints make LWW arrival-order-dependent, and remote inserts omit
/// checkout-local resolution state. The rebuilt table therefore has no FK or triggers and gives the
/// one non-null local column a deterministic default. Parent cleanup is explicit in the memory
/// drain after this migration.
pub fn apply_syncable_memory_bindings(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_bindings_lens_revision_insert;
         DROP TRIGGER IF EXISTS memory_bindings_lens_revision_delete;
         DROP TRIGGER IF EXISTS memory_bindings_lens_revision_update;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_insert;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_delete;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_update;",
    )?;
    if table_is_strict(conn, "repo_memory_bindings")?
        && primary_key_columns(conn, "repo_memory_bindings")?
            == ["repo_id", "memory_id", "binding_kind", "binding_id"]
    {
        return Ok(());
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS repo_memory_bindings_v103;
         CREATE TABLE repo_memory_bindings_v103(
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
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
             tracker TEXT,
             project TEXT,
             item_key TEXT,
             anchor_status TEXT NOT NULL DEFAULT 'unverified',
             created_at_ms INTEGER NOT NULL,
             symbol_kind TEXT,
             signature_hash TEXT,
             moniker_tool TEXT,
             moniker_tool_version TEXT,
             relocation_reason TEXT,
             downgrade_pending_at_ms INTEGER,
             PRIMARY KEY(repo_id, memory_id, binding_kind, binding_id)
         ) STRICT;
         INSERT INTO repo_memory_bindings_v103(
             repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
             logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, tracker, project,
             item_key, anchor_status, created_at_ms, symbol_kind, signature_hash, moniker_tool,
             moniker_tool_version, relocation_reason, downgrade_pending_at_ms
         )
         SELECT repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
             logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, tracker, project,
             item_key, anchor_status, created_at_ms, symbol_kind, signature_hash, moniker_tool,
             moniker_tool_version, relocation_reason, downgrade_pending_at_ms
         FROM repo_memory_bindings;
         DROP TABLE repo_memory_bindings;
         ALTER TABLE repo_memory_bindings_v103 RENAME TO repo_memory_bindings;
         CREATE INDEX idx_repo_memory_bindings_logical_symbol
             ON repo_memory_bindings(logical_symbol_id);
         CREATE INDEX idx_repo_memory_bindings_symbol ON repo_memory_bindings(symbol_id);
         CREATE INDEX idx_repo_memory_bindings_chunk ON repo_memory_bindings(chunk_id);
         CREATE INDEX idx_repo_memory_bindings_edge ON repo_memory_bindings(edge_id);
         CREATE INDEX idx_repo_memory_bindings_path ON repo_memory_bindings(path);",
    )
}

fn table_is_strict(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT strict FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
        [table],
        |row| row.get(0),
    )
}

/// V104 (#997): the durable re-adoption worklist and audit log.
///
/// An effective `DeviceRemove` makes the #935 ingest gate refuse every later copy of that
/// device's entries, so rows whose whole-row LWW winner is the removed writer never reach a
/// replica enrolled after the removal. The account fold records each removal here, and a
/// roster-effective writer drains the worklist by re-authoring the surviving state under its
/// own chain. `roster_ref` is NOT a foreign key: the projection deletes and rewrites roster
/// history on every fold, and the worklist must survive that rewrite.
pub fn apply_table_sync_readoption(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_readoption_work(
              account_id BLOB NOT NULL CHECK(length(account_id) = 32),
              device_fingerprint BLOB NOT NULL CHECK(length(device_fingerprint) = 32),
              stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
              roster_ref BLOB NOT NULL CHECK(length(roster_ref) = 32),
              removed_at_epoch INTEGER NOT NULL,
              enqueued_at_ms INTEGER NOT NULL,
              processed_at_ms INTEGER,
              PRIMARY KEY(account_id, device_fingerprint, stream_id)
          ) STRICT;
          CREATE INDEX IF NOT EXISTS table_sync_readoption_work_pending
              ON table_sync_readoption_work(account_id, stream_id)
              WHERE processed_at_ms IS NULL;
          CREATE TABLE IF NOT EXISTS table_sync_readoption_audit(
              audit_id INTEGER PRIMARY KEY,
              account_id BLOB NOT NULL CHECK(length(account_id) = 32),
              removed_fingerprint BLOB NOT NULL CHECK(length(removed_fingerprint) = 32),
              adopter_fingerprint BLOB NOT NULL CHECK(length(adopter_fingerprint) = 32),
              stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
              repo_id TEXT NOT NULL,
              scope_id TEXT NOT NULL,
              table_name TEXT NOT NULL,
              row_pk TEXT NOT NULL,
              original_lamport INTEGER NOT NULL,
              original_entry_hash BLOB NOT NULL CHECK(length(original_entry_hash) = 32),
              adopted_entry_hash BLOB NOT NULL CHECK(length(adopted_entry_hash) = 32),
              adopted_at_ms INTEGER NOT NULL
          ) STRICT;
          CREATE INDEX IF NOT EXISTS table_sync_readoption_audit_stream
              ON table_sync_readoption_audit(account_id, stream_id);",
    )
}

/// V105 (#1127): the per-(stream, device) retained floor.
///
/// Accepted-entry compaction drops a chain prefix below the floor; this table is how a peer
/// (and the local accept path) tells an intentionally reclaimed prefix from a chain gap. It is
/// swept with the stream directory on repository purge, NOT retained like the chain-tip
/// witnesses: once the accepted log itself is gone, "the prefix below F was compacted" stops
/// being true, and a surviving floor would short-circuit the re-offered prefix the restored
/// chain then waits on forever.
pub fn apply_table_sync_retained_floors(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_retained_floors(
             stream_id          BLOB    NOT NULL CHECK(length(stream_id) = 32),
             device_fingerprint BLOB    NOT NULL CHECK(length(device_fingerprint) = 32),
             lamport            INTEGER NOT NULL,
             entry_hash         BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             compacted_at_ms    INTEGER NOT NULL,
             PRIMARY KEY(stream_id, device_fingerprint)
         ) STRICT;",
    )
}

/// V106 (#1127): re-adoption audit provenance for a compacted winner.
///
/// Re-adoption candidates derive from the merge state, which survives compaction; a winning entry
/// below the retained floor is gone, so the audit records its slot by `(stream, device, lamport)`
/// and stores NULL for the hash. Rebuilds the just-shipped V104 table — it holds only locally
/// authored audit rows, copied verbatim.
pub fn apply_readoption_audit_nullable_winner(conn: &Connection) -> rusqlite::Result<()> {
    let already_nullable: bool = conn.query_row(
        "SELECT NOT \"notnull\" FROM pragma_table_info('table_sync_readoption_audit')
         WHERE name = 'original_entry_hash'",
        [],
        |row| row.get(0),
    )?;
    if already_nullable {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE table_sync_readoption_audit_v106(
             audit_id INTEGER PRIMARY KEY,
             account_id BLOB NOT NULL CHECK(length(account_id) = 32),
             removed_fingerprint BLOB NOT NULL CHECK(length(removed_fingerprint) = 32),
             adopter_fingerprint BLOB NOT NULL CHECK(length(adopter_fingerprint) = 32),
             stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
             repo_id TEXT NOT NULL,
             scope_id TEXT NOT NULL,
             table_name TEXT NOT NULL,
             row_pk TEXT NOT NULL,
             original_lamport INTEGER NOT NULL,
             original_entry_hash BLOB CHECK(
                 original_entry_hash IS NULL OR length(original_entry_hash) = 32
             ),
             adopted_entry_hash BLOB NOT NULL CHECK(length(adopted_entry_hash) = 32),
             adopted_at_ms INTEGER NOT NULL
         ) STRICT;
         INSERT INTO table_sync_readoption_audit_v106
         SELECT * FROM table_sync_readoption_audit;
         DROP TABLE table_sync_readoption_audit;
         ALTER TABLE table_sync_readoption_audit_v106
             RENAME TO table_sync_readoption_audit;
         CREATE INDEX IF NOT EXISTS table_sync_readoption_audit_stream
             ON table_sync_readoption_audit(account_id, stream_id);",
    )
}

/// V107 (#1133): make `memory_reality` and `memory_summaries` syncable on the `overlay/1` scope by
/// dropping their Lens revision triggers.
///
/// Both tables carry two trigger families that bump a per-repo Lens lane on every physical row
/// write: the V093 `*_lens_revision_*` set (the aggregate enrichment clock) and the V102
/// `*_lane_revision_*` set (the split memories lane). Under `overlay/1` a row arrives by whole-row
/// LWW apply, and a trigger firing on a wire-applied row is a device-local side effect that also
/// fires on redeliveries, losing writes, and refold replay — exactly what the sync apply must not
/// do. The dream write path and the sync apply path advance the enrichment and memories lanes
/// explicitly instead (`meta::bump_lens_revisions`), so the only remaining lane movement is the one
/// the code chooses.
///
/// The DROP is UNCONDITIONAL: `schema::apply` replays the whole additive ladder, so V093/V102
/// recreate these triggers ahead of this migration on every `index --full`; short-circuiting on
/// table shape would let the recreated triggers survive the replay.
pub fn apply_syncable_overlay_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_reality_lens_revision_insert;
         DROP TRIGGER IF EXISTS memory_reality_lens_revision_delete;
         DROP TRIGGER IF EXISTS memory_reality_lens_revision_update;
         DROP TRIGGER IF EXISTS memory_reality_lane_revision_insert;
         DROP TRIGGER IF EXISTS memory_reality_lane_revision_delete;
         DROP TRIGGER IF EXISTS memory_reality_lane_revision_update;
         DROP TRIGGER IF EXISTS memory_summaries_lens_revision_insert;
         DROP TRIGGER IF EXISTS memory_summaries_lens_revision_delete;
         DROP TRIGGER IF EXISTS memory_summaries_lens_revision_update;
         DROP TRIGGER IF EXISTS memory_summaries_lane_revision_insert;
         DROP TRIGGER IF EXISTS memory_summaries_lane_revision_delete;
         DROP TRIGGER IF EXISTS memory_summaries_lane_revision_update;",
    )
}

/// V108 (#1135): make `papertrail_distill` a deterministic whole-row table for `distill/1`.
///
/// The old shape keyed on an AUTOINCREMENT `id` (device-local, non-deterministic) with the thread
/// natural key only as a UNIQUE index — incompatible with whole-row LWW sync. The rebuilt table
/// keys on `(repo_id, tracker, project, item_kind, item_key)` (repo_id part of the PK, as the
/// transport requires), drops `id`, has no FK or triggers, and adds the `CHECK(x IN (0,1))` the
/// store lint asks for on the genuine boolean facets (NOT on `quotes_materialized`/
/// `anchors_qualified_count`, which are counts). Children reference the thread by its natural key,
/// never `id`, so dropping it breaks nothing.
///
/// The trigger DROP is UNCONDITIONAL and precedes the shape short-circuit: `schema::apply` replays
/// the whole ladder, so V093/V102 recreate the papertrail-lane triggers ahead of this migration on
/// every `index --full`. Under `distill/1` a row arrives by whole-row LWW, so the distill write and
/// the sync apply advance the papertrail Lens lane explicitly instead of via a trigger.
pub fn apply_syncable_distill_records(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS papertrail_distill_lens_revision_insert;
         DROP TRIGGER IF EXISTS papertrail_distill_lens_revision_delete;
         DROP TRIGGER IF EXISTS papertrail_distill_lens_revision_update;
         DROP TRIGGER IF EXISTS papertrail_distill_lane_revision_insert;
         DROP TRIGGER IF EXISTS papertrail_distill_lane_revision_delete;
         DROP TRIGGER IF EXISTS papertrail_distill_lane_revision_update;",
    )?;
    if table_is_strict(conn, "papertrail_distill")?
        && primary_key_columns(conn, "papertrail_distill")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key"]
    {
        return Ok(());
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS papertrail_distill_v108;
         CREATE TABLE papertrail_distill_v108(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             distill_input_hash TEXT NOT NULL,
             pipeline_version INTEGER NOT NULL,
             root_issue TEXT,
             root_cause TEXT,
             root_cause_class TEXT,
             decision_chosen TEXT,
             outcome_summary TEXT,
             outcome_status_model TEXT,
             epistemic_status_decision TEXT,
             epistemic_status_outcome TEXT,
             fix_edge_source TEXT NOT NULL,
             -- COUNTS, not booleans (quotes_materialized is the evidence-unit count) — no 0/1 \
         CHECK.
             quotes_materialized INTEGER NOT NULL DEFAULT 0,
             anchors_qualified_count INTEGER NOT NULL DEFAULT 0,
             thread_shape TEXT NOT NULL,
             -- Genuine 0/1 facets: carry the CHECK the store lint asks for (Bool has no pragma the
             -- lint can require it through).
             outcome_claim_verified INTEGER NOT NULL DEFAULT 0
                 CHECK(outcome_claim_verified IN (0, 1)),
             decision_provenance_verified INTEGER NOT NULL DEFAULT 0
                 CHECK(decision_provenance_verified IN (0, 1)),
             revert_override INTEGER NOT NULL DEFAULT 0 CHECK(revert_override IN (0, 1)),
             closing_keyword_floor TEXT,
             distilled_at_ms INTEGER NOT NULL,
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             prompt_version INTEGER,
             model_input_hash TEXT,
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key)
         ) STRICT;
         INSERT INTO papertrail_distill_v108(
             tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
             root_issue, root_cause, root_cause_class, decision_chosen, outcome_summary,
             outcome_status_model, epistemic_status_decision, epistemic_status_outcome,
             fix_edge_source, quotes_materialized, anchors_qualified_count, thread_shape,
             outcome_claim_verified, decision_provenance_verified, revert_override,
             closing_keyword_floor, distilled_at_ms, repo_id, prompt_version, model_input_hash)
         SELECT
             tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
             root_issue, root_cause, root_cause_class, decision_chosen, outcome_summary,
             outcome_status_model, epistemic_status_decision, epistemic_status_outcome,
             fix_edge_source, quotes_materialized, anchors_qualified_count, thread_shape,
             outcome_claim_verified, decision_provenance_verified, revert_override,
             closing_keyword_floor, distilled_at_ms, repo_id, prompt_version, model_input_hash
         FROM papertrail_distill;
         DROP TABLE papertrail_distill;
         ALTER TABLE papertrail_distill_v108 RENAME TO papertrail_distill;",
    )
}

/// V109 (#1137): make the distill `edges` and `alternatives` children syncable on `distill/1`.
///
/// Like the parent (V108), each keyed on a device-local AUTOINCREMENT `id` with its natural key
/// only as a UNIQUE index. Rebuild each onto the natural key (repo_id first), dropping `id`;
/// neither carries triggers, local columns, or FKs. Each block short-circuits once its table is
/// already in the rebuilt shape, so a full-ladder replay is a no-op.
pub fn apply_syncable_distill_edges_and_alternatives(conn: &Connection) -> rusqlite::Result<()> {
    if !(table_is_strict(conn, "papertrail_distill_edges")?
        && primary_key_columns(conn, "papertrail_distill_edges")?
            == [
                "repo_id",
                "tracker",
                "project",
                "src_item_kind",
                "src_item_key",
                "dst_item_kind",
                "dst_item_key",
                "edge_kind",
            ])
    {
        conn.execute_batch(
            "DROP TABLE IF EXISTS papertrail_distill_edges_v109;
             CREATE TABLE papertrail_distill_edges_v109(
                 tracker TEXT NOT NULL,
                 project TEXT NOT NULL,
                 src_item_kind TEXT NOT NULL,
                 src_item_key TEXT NOT NULL,
                 dst_item_kind TEXT NOT NULL,
                 dst_item_key TEXT NOT NULL,
                 edge_kind TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 repo_id TEXT NOT NULL DEFAULT '__unassigned__',
                 PRIMARY KEY(repo_id, tracker, project, src_item_kind, src_item_key,
                             dst_item_kind, dst_item_key, edge_kind)
             ) STRICT;
             INSERT INTO papertrail_distill_edges_v109(
                 tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                 edge_kind, created_at_ms, repo_id)
             SELECT tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                 edge_kind, created_at_ms, repo_id
             FROM papertrail_distill_edges;
             DROP TABLE papertrail_distill_edges;
             ALTER TABLE papertrail_distill_edges_v109 RENAME TO papertrail_distill_edges;",
        )?;
    }
    if !(table_is_strict(conn, "papertrail_distill_alternatives")?
        && primary_key_columns(conn, "papertrail_distill_alternatives")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key", "ordinal"])
    {
        conn.execute_batch(
            "DROP TABLE IF EXISTS papertrail_distill_alternatives_v109;
             CREATE TABLE papertrail_distill_alternatives_v109(
                 tracker TEXT NOT NULL,
                 project TEXT NOT NULL,
                 item_kind TEXT NOT NULL,
                 item_key TEXT NOT NULL,
                 ordinal INTEGER NOT NULL,
                 alternative TEXT NOT NULL,
                 reason TEXT,
                 repo_id TEXT NOT NULL DEFAULT '__unassigned__',
                 PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, ordinal)
             ) STRICT;
             INSERT INTO papertrail_distill_alternatives_v109(
                 tracker, project, item_kind, item_key, ordinal, alternative, reason, repo_id)
             SELECT tracker, project, item_kind, item_key, ordinal, alternative, reason, repo_id
             FROM papertrail_distill_alternatives;
             DROP TABLE papertrail_distill_alternatives;
             ALTER TABLE papertrail_distill_alternatives_v109
                 RENAME TO papertrail_distill_alternatives;",
        )?;
    }
    Ok(())
}

/// V110 (#1139): make the distill `record_commits` child syncable on `distill/1`.
///
/// The table was key-only (its only columns were the natural key + `commit_sha`), which the
/// whole-row apply path rejects — a syncable table needs at least one non-key synced column. The
/// rebuild keys on `(repo_id, tracker, project, item_kind, item_key, commit_sha)` (its former
/// UNIQUE index), drops the device-local AUTOINCREMENT `id`, and adds `created_at_ms` (when the
/// fixing-commit link was recorded) as that non-key column. Legacy rows get `0`; a record
/// regeneration rewrites them with the real timestamp (the mechanical junctions are cleared and
/// re-mined). No triggers, local columns, or FKs. Short-circuits once the table is already in the
/// rebuilt shape.
pub fn apply_syncable_distill_record_commits(conn: &Connection) -> rusqlite::Result<()> {
    if table_is_strict(conn, "papertrail_distill_record_commits")?
        && primary_key_columns(conn, "papertrail_distill_record_commits")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key", "commit_sha"]
    {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS papertrail_distill_record_commits_v110;
         CREATE TABLE papertrail_distill_record_commits_v110(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             commit_sha TEXT NOT NULL,
             created_at_ms INTEGER NOT NULL DEFAULT 0,
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, commit_sha)
         ) STRICT;
         INSERT INTO papertrail_distill_record_commits_v110(
             tracker, project, item_kind, item_key, commit_sha, created_at_ms, repo_id)
         SELECT tracker, project, item_kind, item_key, commit_sha, 0, repo_id
         FROM papertrail_distill_record_commits;
         DROP TABLE papertrail_distill_record_commits;
         ALTER TABLE papertrail_distill_record_commits_v110
             RENAME TO papertrail_distill_record_commits;",
    )
}

/// V111 (#1139): make the distill `evidence` child syncable on `distill/1`.
///
/// The table had no natural unique key — a model can cite the same unit twice for one field, and
/// title/body citations share `source_id` — so a composite discriminator is duplicate-unsafe. The
/// rebuild adds a stable per-thread `ordinal` (assigned at insert in citation order by the drain,
/// backfilled here by `id` order within each thread so existing rows get a deterministic sequence),
/// keys on `(repo_id, tracker, project, item_kind, item_key, ordinal)`, and drops the device-local
/// AUTOINCREMENT `id`. No triggers, local columns, or FKs; `source_part` keeps its CHECK.
/// Short-circuits once the table is already in the rebuilt shape.
pub fn apply_syncable_distill_evidence(conn: &Connection) -> rusqlite::Result<()> {
    if table_is_strict(conn, "papertrail_distill_evidence")?
        && primary_key_columns(conn, "papertrail_distill_evidence")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key", "ordinal"]
    {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS papertrail_distill_evidence_v111;
         CREATE TABLE papertrail_distill_evidence_v111(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             ordinal INTEGER NOT NULL,
             field TEXT NOT NULL,
             source_kind TEXT NOT NULL,
             source_part TEXT CHECK(source_part IN ('title', 'body', 'comment')),
             source_id TEXT NOT NULL,
             byte_start INTEGER NOT NULL,
             byte_end INTEGER NOT NULL,
             quote TEXT NOT NULL,
             author TEXT,
             author_kind TEXT,
             author_association TEXT,
             unit_created_at_ms INTEGER,
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, ordinal)
         ) STRICT;
         INSERT INTO papertrail_distill_evidence_v111(
             tracker, project, item_kind, item_key, ordinal, field, source_kind, source_part,
             source_id, byte_start, byte_end, quote, author, author_kind, author_association,
             unit_created_at_ms, repo_id)
         SELECT
             e.tracker, e.project, e.item_kind, e.item_key,
             (SELECT COUNT(*) FROM papertrail_distill_evidence AS earlier
              WHERE earlier.repo_id = e.repo_id AND earlier.tracker = e.tracker
                AND earlier.project = e.project AND earlier.item_kind = e.item_kind
                AND earlier.item_key = e.item_key AND earlier.id < e.id),
             e.field, e.source_kind, e.source_part, e.source_id, e.byte_start, e.byte_end,
             e.quote, e.author, e.author_kind, e.author_association, e.unit_created_at_ms, \
         e.repo_id
         FROM papertrail_distill_evidence AS e;
         DROP TABLE papertrail_distill_evidence;
         ALTER TABLE papertrail_distill_evidence_v111 RENAME TO papertrail_distill_evidence;",
    )
}

fn primary_key_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk > 0 ORDER BY pk")?;
    stmt.query_map([table], |row| row.get(0))?.collect()
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

#[cfg(test)]
mod sync_security_events_migration_tests {
    use super::*;

    /// Insert one adoption-audit event via the write path (`INSERT OR IGNORE`); returns rows
    /// changed so a dedup can be observed as `0`.
    fn insert_event(conn: &Connection, kind: &str, entry_hash: &[u8]) -> usize {
        conn.execute(
            "INSERT OR IGNORE INTO sync_security_events(
                 kind, account_id, stream_id, key_epoch, entry_hash,
                 expected_key_id, observed_key_id, observed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                kind,
                [1u8; 32].as_slice(),
                [2u8; 32].as_slice(),
                0u64.to_be_bytes().as_slice(),
                entry_hash,
                Option::<Vec<u8>>::None,
                Option::<Vec<u8>>::None,
                123i64,
            ],
        )
        .unwrap()
    }

    #[test]
    fn table_dedupes_on_kind_and_entry_hash() {
        let conn = Connection::open_in_memory().unwrap();
        apply_sync_security_events(&conn).unwrap();
        assert!(table_exists(&conn, "sync_security_events").unwrap(), "the table is created");

        // A first event lands; a second with the SAME (kind, entry_hash) is IGNOREd (the hot
        // seal-path-retry guard); a different kind for the same entry_hash is a distinct event.
        assert_eq!(insert_event(&conn, "wrap_unwrap_failed", &[9u8; 32]), 1, "first event inserts");
        assert_eq!(
            insert_event(&conn, "wrap_unwrap_failed", &[9u8; 32]),
            0,
            "a duplicate (kind, entry_hash) is ignored, not re-appended",
        );
        assert_eq!(
            insert_event(&conn, "wrap_key_id_mismatch", &[9u8; 32]),
            1,
            "a distinct kind for the same entry is its own event",
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_security_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2, "exactly the two distinct events survive");
    }

    #[test]
    fn strict_typing_rejects_a_non_blob_account_id() {
        let conn = Connection::open_in_memory().unwrap();
        apply_sync_security_events(&conn).unwrap();
        // STRICT: `account_id` is declared BLOB, so an INTEGER literal there is a datatype mismatch
        // rather than a silently coerced value.
        let inserted = conn.execute(
            "INSERT INTO sync_security_events(
                 kind, account_id, stream_id, key_epoch, entry_hash, observed_at_ms)
             VALUES ('wrap_unwrap_failed', 5, x'02', x'0000000000000000', x'09', 1)",
            [],
        );
        assert!(inserted.is_err(), "STRICT rejects an INTEGER in the BLOB account_id column");
    }
}

#[cfg(test)]
mod reindex_after_unix_backslash_rendering_tests {
    use super::*;

    /// A `repo_meta` + `parser_failures` pair minimal enough to drive the V098 body, seeded with
    /// every marker the migration clears plus one control row it must leave untouched.
    fn seed(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE repo_meta(
                 repo_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                 PRIMARY KEY(repo_id, key));
             CREATE TABLE parser_failures(
                 repo_id TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(repo_id, path));",
        )
        .unwrap();
        let put = |key: &str, value: &str| {
            conn.execute(
                "INSERT INTO repo_meta(repo_id, key, value) VALUES ('r', ?1, ?2)",
                rusqlite::params![key, value],
            )
            .unwrap();
        };
        put(crate::meta::BASE_SCOPE_DISCOVERED_META, "1");
        put(crate::meta::GIT_HISTORY_INDEXED_ROOT_META, "/some/root");
        put(
            &format!("{}abc123", crate::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX),
            "base\nlinked\n42",
        );
        // A control the migration must NOT touch: source_root is a path-valued key too, but it is a
        // config record the next index resets from config, not a walk gate — deleting it would
        // break reads until that pass rather than merely forcing one.
        put("source_root", "/some/root");
        conn.execute("INSERT INTO parser_failures(repo_id, path) VALUES ('r', 'foo/bar.rs')", [])
            .unwrap();
    }

    fn has_key(conn: &Connection, key: &str) -> bool {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM repo_meta WHERE key = ?1)", [key], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn clears_every_freshness_marker_and_leaves_the_control_row() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);

        apply_reindex_after_unix_backslash_rendering(&conn).unwrap();

        assert!(
            !has_key(&conn, crate::meta::BASE_SCOPE_DISCOVERED_META),
            "base-scope marker gone → the next pass re-walks the tree",
        );
        assert!(
            !has_key(&conn, crate::meta::GIT_HISTORY_INDEXED_ROOT_META),
            "history root cursor gone → the next pass full-revwalks and re-reads file changes",
        );
        assert!(
            !has_key(&conn, &format!("{}abc123", crate::meta::WORKTREE_OVERLAY_BASIS_META_PREFIX)),
            "overlay basis gone → each linked checkout re-derives its overlay",
        );
        let failures: i64 =
            conn.query_row("SELECT COUNT(*) FROM parser_failures", [], |r| r.get(0)).unwrap();
        assert_eq!(failures, 0, "the standalone parser-failure row is cleared for re-derivation");
        assert!(
            has_key(&conn, "source_root"),
            "a path record the next index resets from config, not a walk gate, is left intact",
        );
    }
}
