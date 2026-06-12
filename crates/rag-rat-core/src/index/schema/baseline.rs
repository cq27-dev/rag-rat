use super::*;

pub(crate) fn apply_baseline(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS index_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS files(
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

        CREATE TABLE IF NOT EXISTS chunks(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL,
            chunk_kind TEXT NOT NULL,
            symbol_path TEXT,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            text TEXT NOT NULL,
            text_hash TEXT NOT NULL,
            source_revision TEXT NOT NULL DEFAULT '',
            anchor_version INTEGER NOT NULL DEFAULT 1,
            normalized_hash TEXT NOT NULL DEFAULT '',
            start_boundary_hash TEXT NOT NULL DEFAULT '',
            end_boundary_hash TEXT NOT NULL DEFAULT '',
            start_context_hash TEXT NOT NULL DEFAULT '',
            end_context_hash TEXT NOT NULL DEFAULT '',
            context_radius INTEGER NOT NULL DEFAULT 2,
            embedding_policy TEXT NOT NULL DEFAULT 'Embed',
            embedding_priority INTEGER NOT NULL DEFAULT 1,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL,
            language TEXT NOT NULL,
            name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            kind TEXT NOT NULL,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            signature TEXT,
            docs TEXT,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );

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

        CREATE TABLE IF NOT EXISTS symbol_facts(
            symbol_id INTEGER NOT NULL,
            fact_kind TEXT NOT NULL,
            fact_value TEXT NOT NULL,
            PRIMARY KEY(symbol_id, fact_kind, fact_value),
            FOREIGN KEY(symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS edges(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_file_id INTEGER,
            from_symbol_id INTEGER,
            to_symbol_id INTEGER,
            from_name TEXT,
            to_name TEXT NOT NULL,
            source_start_line INTEGER NOT NULL DEFAULT 0,
            source_end_line INTEGER NOT NULL DEFAULT 0,
            source_start_byte INTEGER NOT NULL DEFAULT 0,
            source_end_byte INTEGER NOT NULL DEFAULT 0,
            target_start_line INTEGER,
            target_end_line INTEGER,
            target_qualified_name TEXT,
            evidence TEXT,
            receiver_hint TEXT,
            resolution TEXT NOT NULL DEFAULT 'unresolved',
            -- Callee identifier byte range (the SCIP occurrence key, #67); NULL for non-call /
            -- file-level edges. source_*_byte covers the whole call_expression, these the token.
            callee_start_byte INTEGER,
            callee_end_byte INTEGER,
            edge_kind TEXT NOT NULL,
            confidence TEXT NOT NULL,
            FOREIGN KEY(source_file_id) REFERENCES files(id) ON DELETE CASCADE,
            FOREIGN KEY(from_symbol_id) REFERENCES symbols(id) ON DELETE SET NULL,
            FOREIGN KEY(to_symbol_id) REFERENCES symbols(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS docs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id INTEGER NOT NULL,
            source_kind TEXT NOT NULL,
            heading_path TEXT
        );

        CREATE TABLE IF NOT EXISTS parser_failures(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            message TEXT NOT NULL
        );

        DROP TABLE IF EXISTS embeddings;
        DROP TABLE IF EXISTS chunk_summaries;

        CREATE TABLE IF NOT EXISTS ai_models(
            model_id TEXT PRIMARY KEY,
            capability TEXT NOT NULL,
            embedding_dim INTEGER,
            runtime TEXT NOT NULL DEFAULT 'local',
            installed INTEGER NOT NULL DEFAULT 0,
            disabled INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'MissingModel',
            installed_at_ms INTEGER,
            last_error TEXT
        );

        CREATE TABLE IF NOT EXISTS chunk_embeddings(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id INTEGER NOT NULL,
            model_id TEXT NOT NULL,
            model_version TEXT NOT NULL DEFAULT 'v1',
            source_text_hash TEXT NOT NULL,
            input_hash TEXT NOT NULL DEFAULT '',
            embedding_text_version TEXT NOT NULL DEFAULT '',
            embedding_policy TEXT NOT NULL DEFAULT 'Embed',
            embedding_priority INTEGER NOT NULL DEFAULT 1,
            input_chars INTEGER NOT NULL DEFAULT 0,
            input_truncated INTEGER NOT NULL DEFAULT 0,
            embedding_dim INTEGER NOT NULL DEFAULT 0,
            vector_blob BLOB NOT NULL,
            status TEXT NOT NULL,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error_class TEXT,
            next_retry_after_ms INTEGER,
            computed_at_ms INTEGER,
            created_at_ms INTEGER NOT NULL,
            last_error TEXT,
            UNIQUE(chunk_id, model_id),
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

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

        CREATE TABLE IF NOT EXISTS reconcile_attempts(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at_ms INTEGER NOT NULL,
            finished_at_ms INTEGER,
            limit_count INTEGER,
            processed_chunks INTEGER NOT NULL DEFAULT 0,
            embeddings_written INTEGER NOT NULL DEFAULT 0,
            blocked_chunks INTEGER NOT NULL DEFAULT 0,
            elapsed_ms INTEGER NOT NULL DEFAULT 0,
            input_chars INTEGER NOT NULL DEFAULT 0,
            batch_size INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL,
            message TEXT
        );

        CREATE TABLE IF NOT EXISTS git_commits(
            hash TEXT PRIMARY KEY,
            author_name TEXT NOT NULL,
            author_email TEXT NOT NULL,
            authored_at_s INTEGER NOT NULL,
            committed_at_s INTEGER NOT NULL,
            subject TEXT NOT NULL,
            body TEXT NOT NULL,
            changed_file_count INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS git_file_changes(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            commit_hash TEXT NOT NULL,
            path TEXT NOT NULL,
            additions INTEGER,
            deletions INTEGER,
            change_kind TEXT NOT NULL DEFAULT 'modified',
            FOREIGN KEY(commit_hash) REFERENCES git_commits(hash) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS git_chunk_blame(
            chunk_id INTEGER PRIMARY KEY,
            source_text_hash TEXT NOT NULL,
            path TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            line_count INTEGER NOT NULL,
            dominant_commit TEXT,
            dominant_commit_lines INTEGER NOT NULL DEFAULT 0,
            newest_commit TEXT,
            newest_commit_time_s INTEGER,
            oldest_commit TEXT,
            oldest_commit_time_s INTEGER,
            commit_counts_json TEXT NOT NULL,
            computed_at_ms INTEGER NOT NULL,
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS github_refs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            ref_kind TEXT NOT NULL DEFAULT 'unknown',
            source_kind TEXT NOT NULL,
            source_path TEXT,
            source_commit TEXT,
            source_text TEXT NOT NULL,
            discovered_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_issues(
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
            UNIQUE(owner, repo, number)
        );

        CREATE TABLE IF NOT EXISTS github_comments(
            id INTEGER PRIMARY KEY,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            synced_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_pull_requests(
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
            UNIQUE(owner, repo, number)
        );

        CREATE TABLE IF NOT EXISTS github_reviews(
            id INTEGER PRIMARY KEY,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            html_url TEXT,
            state TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            submitted_at TEXT,
            synced_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_review_comments(
            id INTEGER PRIMARY KEY,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            path TEXT,
            html_url TEXT NOT NULL,
            body TEXT NOT NULL,
            author TEXT,
            created_at TEXT,
            updated_at TEXT,
            synced_at_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS github_ref_sync(
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            status TEXT NOT NULL,
            synced_at_ms INTEGER NOT NULL,
            last_error TEXT,
            PRIMARY KEY(owner, repo, number)
        );

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

        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(
            text,
            content='chunks',
            content_rowid='id',
            tokenize='porter'
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS commit_fts USING fts5(
            subject,
            body,
            content='git_commits',
            content_rowid='rowid',
            tokenize='porter'
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS github_fts USING fts5(
            owner,
            repo,
            number UNINDEXED,
            item_kind UNINDEXED,
            item_id UNINDEXED,
            url UNINDEXED,
            title,
            body,
            classification,
            tokenize='porter'
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS repo_memory_fts USING fts5(
            memory_id UNINDEXED,
            title,
            body,
            kind,
            tags,
            tokenize='porter'
        );

        CREATE INDEX IF NOT EXISTS idx_files_language ON files(language);
        CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);
        CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS idx_symbols_qualified_name ON symbols(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
        CREATE INDEX IF NOT EXISTS idx_symbol_facts_kind_value
            ON symbol_facts(fact_kind, fact_value);
        CREATE INDEX IF NOT EXISTS idx_logical_symbols_qualified_name
            ON logical_symbols(qualified_name);
        CREATE INDEX IF NOT EXISTS idx_logical_symbol_members_symbol
            ON logical_symbol_members(symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edges_from_symbol ON edges(from_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edges_to_symbol ON edges(to_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_git_file_changes_path ON git_file_changes(path);
        CREATE INDEX IF NOT EXISTS idx_git_file_changes_commit ON git_file_changes(commit_hash);
        CREATE INDEX IF NOT EXISTS idx_github_refs_path ON github_refs(source_path);
        CREATE INDEX IF NOT EXISTS idx_github_refs_issue ON github_refs(owner, repo, number);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_github_refs_unique
            ON github_refs(owner, repo, number, source_kind, COALESCE(source_path, ''), \
         COALESCE(source_commit, ''), source_text);
        CREATE INDEX IF NOT EXISTS idx_github_review_comments_path ON github_review_comments(path);
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
    migrate_files(conn)?;
    migrate_chunks(conn)?;
    migrate_edges(conn)?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_edges_from_name ON edges(from_name);
        CREATE INDEX IF NOT EXISTS idx_edges_to_name ON edges(to_name);
        ",
    )?;
    apply_embedding_vector_metadata(conn)?;
    apply_derived_artifact_reconcile_metadata(conn)?;
    apply_edge_source_target_spans(conn)?;
    apply_embedding_policy_and_input_hash(conn)?;
    apply_logical_symbol_groups(conn)?;
    apply_github_ref_sync(conn)?;
    apply_symbol_facts(conn)?;
    apply_repo_memories(conn)?;
    apply_repo_memory_call_paths(conn)?;
    apply_repo_memory_call_path_edges(conn)?;
    apply_graph_file_lookup_indexes(conn)?;
    Ok(())
}

pub(crate) fn rebuild_fts(conn: &Connection) -> anyhow::Result<()> {
    // `chunk_fts` / `commit_fts` are external-content FTS5 tables (content='chunks' /
    // content='git_commits'). The canonical way to rebuild an external-content index is the
    // built-in 'rebuild' command, which re-reads the content table. An unqualified
    // `DELETE FROM <table>` on an external-content FTS5 table corrupts the index when the FTS
    // and content tables are out of sync (`database disk image is malformed`, SQLite 11/267) —
    // see #51. 'rebuild' is desync-safe.
    conn.execute_batch(
        "
        INSERT INTO chunk_fts(chunk_fts) VALUES('rebuild');
        INSERT INTO commit_fts(commit_fts) VALUES('rebuild');
        ",
    )?;
    Ok(())
}

#[cfg(test)]
mod rebuild_fts_tests {
    use rusqlite::Connection;

    // Reproduces #51: chunks present that were never inserted into the external-content
    // `chunk_fts`. The old `DELETE FROM chunk_fts` rebuild corrupts the index on this desync;
    // the 'rebuild' command recovers it and indexes the content.
    #[test]
    fn rebuild_fts_recovers_a_desynced_external_content_index() {
        let conn = Connection::open_in_memory().unwrap();
        super::super::apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES ('src/a.rs', 'rust', 'source', 'h', 0, 0)",
            [],
        )
        .unwrap();
        // Seed a chunk WITHOUT a matching chunk_fts row — the out-of-sync state from #51.
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte,
                                start_line, end_line, text, text_hash)
             VALUES (1, 'symbol', 'alpha', 0, 10, 1, 5, 'fn alpha() { beta() }', 'th')",
            [],
        )
        .unwrap();

        super::rebuild_fts(&conn).unwrap();

        let hits: i64 = conn
            .query_row("SELECT count(*) FROM chunk_fts WHERE chunk_fts MATCH 'alpha'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(hits, 1, "rebuilt FTS index must be queryable and contain the chunk");
    }
}
