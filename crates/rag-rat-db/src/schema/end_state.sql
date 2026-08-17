PRAGMA application_id = 0;
PRAGMA user_version = 0;
CREATE TABLE schema_version(
            id TEXT PRIMARY KEY,
            applied_at_ms INTEGER NOT NULL,
            checksum TEXT NOT NULL,
            description TEXT NOT NULL
        );
CREATE TABLE index_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
CREATE TABLE chunks(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL,
            chunk_kind TEXT NOT NULL,
            symbol_path TEXT,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            -- Chunk text lives ONLY in the compressed chunk_text store (#77 Phase 2); there is no
            -- inline text column. text_hash stays (it keys embedding/anchor freshness, not text).
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
            embedding_priority INTEGER NOT NULL DEFAULT 1, symbol_id INTEGER,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );
CREATE TABLE chunk_text(
            chunk_id INTEGER PRIMARY KEY,
            blob BLOB NOT NULL,
            -- raw_len is the decompress capacity; CHECK(>= 0) so a bad write can't become a huge
            -- usize at the read-side cast and blow up Vec::with_capacity.
            raw_len INTEGER NOT NULL CHECK(raw_len >= 0),
            -- Which chunk_text_dict version compressed this blob: a zstd blob is only decodable
            -- against the dict it was made with, so the dict is a per-blob decode key (#77 Phase 2).
            dict_version INTEGER NOT NULL,
            FOREIGN KEY(chunk_id) REFERENCES chunks(id) ON DELETE CASCADE
        ) STRICT;
CREATE TABLE chunk_text_dict(
            version INTEGER PRIMARY KEY,
            dict BLOB NOT NULL
        ) STRICT;
CREATE TABLE symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL,
            language TEXT NOT NULL,
            name TEXT NOT NULL,
            -- The qualified name is INTERNED into the shared `name_strings` pool (#224): edge
            -- call-target names already store ~85% of symbol qnames, so an integer id into the pool
            -- replaces the inline TEXT column + its 49 MB string B-tree (idx on qualified_name_id is
            -- ~13 MB of 3-byte ids). NULLABLE on purpose: a forward-migrated DB ADDs the column
            -- (which can't be NOT NULL on a populated table) before backfilling, so a freshly-built
            -- DB must match that shape. Readers reconstruct the text via a JOIN on name_strings;
            -- gc MUST count this as a referencing column (query_api/gc.rs) or it nulls live qnames.
            qualified_name_id INTEGER,
            kind TEXT NOT NULL,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            signature TEXT,
            docs TEXT, start_line INTEGER NOT NULL DEFAULT 0, end_line INTEGER NOT NULL DEFAULT 0, scope_path TEXT, is_test INTEGER NOT NULL DEFAULT 0,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );
CREATE TABLE logical_symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            language TEXT NOT NULL,
            path TEXT NOT NULL,
            logical_name TEXT NOT NULL,
            -- Interned into `name_strings` (#224), same as symbols.qualified_name_id above. NULLABLE
            -- for the same forward-migrate-then-backfill reason; gc counts it as a referencing
            -- column.
            qualified_name_id INTEGER,
            kind TEXT NOT NULL,
            variant_count INTEGER NOT NULL,
            group_reason TEXT NOT NULL
        , repo_id TEXT NOT NULL DEFAULT '__unassigned__');
CREATE TABLE logical_symbol_members(
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
CREATE TABLE symbol_facts(
            symbol_id INTEGER NOT NULL,
            fact_kind TEXT NOT NULL,
            fact_value TEXT NOT NULL,
            PRIMARY KEY(symbol_id, fact_kind, fact_value),
            FOREIGN KEY(symbol_id) REFERENCES symbols(id) ON DELETE CASCADE
        );
CREATE TABLE name_strings(
            id INTEGER PRIMARY KEY,
            value TEXT NOT NULL UNIQUE
        ) STRICT;
CREATE TABLE edges_data(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_file_id INTEGER,
            from_symbol_id INTEGER,
            to_symbol_id INTEGER,
            from_name_id INTEGER,
            to_name_id INTEGER NOT NULL,
            source_start_line INTEGER NOT NULL DEFAULT 0,
            source_end_line INTEGER NOT NULL DEFAULT 0,
            source_start_byte INTEGER NOT NULL DEFAULT 0,
            source_end_byte INTEGER NOT NULL DEFAULT 0,
            target_start_line INTEGER,
            target_end_line INTEGER,
            target_qualified_name_id INTEGER,
            -- `evidence` stays INLINE: ~40% of its values are distinct, so interning costs more
            -- (dictionary row + UNIQUE-index entry per value) than the dedup saves. It is also
            -- the lazy-materialization candidate (#79 step 3), which wants the raw text local.
            evidence TEXT,
            receiver_hint_id INTEGER,
            receiver_type_hint_id INTEGER,
            resolution_id INTEGER NOT NULL,
            callee_start_byte INTEGER,
            callee_end_byte INTEGER,
            -- Module-aware import scope (#61, V022): the enclosing module/block byte range a Rust
            -- `use` (or inline `mod`) is scoped to, plus the enclosing module body's start byte as
            -- `import_mod_id`. DEDICATED — not a callee_* overload — so the oracle's
            -- `callee_start_byte IS NOT NULL` candidate filter is untouched. NULL on non-import edges.
            import_scope_start_byte INTEGER,
            import_scope_end_byte INTEGER,
            import_mod_id INTEGER,
            edge_kind_id INTEGER NOT NULL,
            confidence_id INTEGER NOT NULL,
            -- Materialized visibility (#734): 1 exactly when the row is not a public graph edge —
            -- an internal dispatch FACT kind (#200) or a suppressed unresolved candidate (V068).
            -- The `edges` view filters on `hidden = 0` (one integer compare per row); every
            -- writer keeps the flag in lockstep with that predicate (see `edge_hidden_flag`).
            hidden INTEGER NOT NULL DEFAULT 0,
            FOREIGN KEY(source_file_id) REFERENCES files(id) ON DELETE CASCADE,
            FOREIGN KEY(from_symbol_id) REFERENCES symbols(id) ON DELETE SET NULL,
            FOREIGN KEY(to_symbol_id) REFERENCES symbols(id) ON DELETE SET NULL
        ) STRICT;
CREATE TABLE docs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chunk_id INTEGER NOT NULL,
            source_kind TEXT NOT NULL,
            heading_path TEXT
        , repo_id TEXT NOT NULL DEFAULT '__unassigned__');
CREATE TABLE ai_models(
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
CREATE TABLE chunk_embeddings(
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
CREATE TABLE chunk_summaries(
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
CREATE TABLE reconcile_meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
CREATE TABLE reconcile_attempts(
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
        , repo_id TEXT NOT NULL DEFAULT '__unassigned__');
CREATE TABLE git_chunk_blame(
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
CREATE TABLE repo_memories(
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
        , repo_id TEXT NOT NULL DEFAULT '__unassigned__', payload_json TEXT, origin TEXT NOT NULL DEFAULT 'local' CHECK (origin IN ('local', 'synced')));
CREATE TABLE repo_memory_tags(
            memory_id TEXT NOT NULL,
            tag TEXT NOT NULL,
            PRIMARY KEY(memory_id, tag),
            FOREIGN KEY(memory_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        );
CREATE TABLE repo_memory_call_paths(
            memory_id TEXT NOT NULL,
            start_logical_symbol_id INTEGER,
            end_logical_symbol_id INTEGER,
            edge_sequence_hash TEXT NOT NULL,
            path_summary TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(memory_id, edge_sequence_hash),
            FOREIGN KEY(memory_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        );
CREATE VIRTUAL TABLE chunk_fts USING fts5(
            text,
            content='',
            contentless_delete=1,
            tokenize='porter'
        );
CREATE VIRTUAL TABLE commit_fts USING fts5(
            subject,
            body,
            content='git_commits',
            content_rowid='rowid',
            tokenize='porter'
        );
CREATE INDEX idx_chunks_file ON chunks(file_id);
CREATE INDEX idx_symbols_name ON symbols(name);
CREATE INDEX idx_symbols_file ON symbols(file_id);
CREATE INDEX idx_symbol_facts_kind_value
            ON symbol_facts(fact_kind, fact_value);
CREATE INDEX idx_logical_symbol_members_symbol
            ON logical_symbol_members(symbol_id);
CREATE INDEX idx_repo_memory_call_paths_start
            ON repo_memory_call_paths(start_logical_symbol_id);
CREATE INDEX idx_repo_memory_call_paths_end
            ON repo_memory_call_paths(end_logical_symbol_id);
CREATE INDEX idx_edges_from_symbol ON edges_data(from_symbol_id);
CREATE INDEX idx_edges_to_symbol ON edges_data(to_symbol_id);
CREATE INDEX idx_edges_source_file ON edges_data(source_file_id);
CREATE INDEX idx_edges_from_name ON edges_data(from_name_id);
CREATE INDEX idx_edges_to_name ON edges_data(to_name_id);
CREATE INDEX idx_edges_target_qname ON edges_data(target_qualified_name_id);
CREATE INDEX idx_logical_symbols_qualified_name_id
                ON logical_symbols(qualified_name_id);
CREATE TABLE papertrail_items(
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
        , closed_at TEXT, resolution TEXT, merge_commit_sha TEXT, state_normalized TEXT NOT NULL DEFAULT '', author_kind TEXT, author_association TEXT, full_rewalk_seen INTEGER NOT NULL DEFAULT 0) STRICT;
CREATE UNIQUE INDEX idx_papertrail_items_natural_key
            ON papertrail_items(repo_id, tracker, project, item_kind, item_key);
CREATE TABLE papertrail_comments(
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
        , author_kind TEXT, author_association TEXT) STRICT;
CREATE UNIQUE INDEX idx_papertrail_comments_natural_key
            ON papertrail_comments(repo_id, tracker, project, comment_id);
CREATE INDEX idx_papertrail_comments_item
            ON papertrail_comments(repo_id, tracker, project, item_kind, item_key);
CREATE INDEX idx_papertrail_comments_anchor_path
            ON papertrail_comments(anchor_path);
CREATE TABLE papertrail_refs(
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
CREATE INDEX idx_papertrail_refs_path ON papertrail_refs(source_path);
CREATE TABLE papertrail_sync_cursor(
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            high_mark_at TEXT,
            low_mark_at TEXT,
            probe_etag TEXT,
            backfill_done INTEGER NOT NULL DEFAULT 0,
            filter_fingerprint TEXT,
            last_probe_ms INTEGER,
            last_full_sync_ms INTEGER,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__', comment_high_mark_at TEXT, comment_page_token TEXT, comment_scan_since TEXT, comment_stream_cursors TEXT, item_delta_page_token TEXT, item_delta_scan_since TEXT, item_delta_high_mark_at TEXT, backfill_page_cursor TEXT, item_thread_cursor TEXT, item_delta_in_progress INTEGER NOT NULL DEFAULT 0, item_delta_replay_required INTEGER NOT NULL DEFAULT 0, delta_processed_keys TEXT, backfill_processed_keys TEXT, full_rewalk INTEGER NOT NULL DEFAULT 0, last_attempt_ms INTEGER, last_successful_probe_ms INTEGER, last_successful_mirror_ms INTEGER, retry_not_before_ms INTEGER, error_class TEXT, error_detail TEXT,
            PRIMARY KEY(repo_id, tracker, project)
        ) STRICT;
CREATE TABLE papertrail_item_tags(
            tracker TEXT NOT NULL,
            project TEXT NOT NULL,
            item_kind TEXT NOT NULL,
            item_key TEXT NOT NULL,
            tag TEXT NOT NULL,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, tag)
        ) STRICT;
CREATE VIRTUAL TABLE papertrail_fts USING fts5(
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
CREATE TABLE papertrail_closing_edges(
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
CREATE UNIQUE INDEX idx_papertrail_closing_edges_natural_key
            ON papertrail_closing_edges(repo_id, tracker, project, issue_kind, issue_key, closer_kind, closer_key);
CREATE INDEX idx_papertrail_closing_edges_closer
            ON papertrail_closing_edges(repo_id, tracker, project, closer_kind, closer_key);
CREATE TABLE repo_memory_call_path_edges(
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
CREATE INDEX idx_repo_memory_call_path_edges_hash
            ON repo_memory_call_path_edges(edge_sequence_hash);
CREATE INDEX idx_symbols_qualified_name_id
                ON symbols(qualified_name_id);
CREATE TABLE symbol_fingerprints(
        symbol_id          INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
        normalizer_kind    TEXT    NOT NULL,            -- baseline | scip
        normalizer_version INTEGER NOT NULL,
        oracle_run_id      INTEGER,                     -- NULL for baseline rows
        struct_hash        TEXT    NOT NULL,
        token_len          INTEGER NOT NULL,
        created_at_ms      INTEGER NOT NULL, token_bag BLOB,
        PRIMARY KEY (symbol_id, normalizer_kind)
    ) STRICT;
CREATE INDEX idx_symbol_fingerprints_struct
        ON symbol_fingerprints(normalizer_kind, struct_hash);
CREATE TABLE oracle_runs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            -- The checkout the run was scoped to. A multi-worktree DB holds runs from sibling
            -- checkouts under the same `(tool, tool_version, commit_sha)`; without this the status
            -- read's `last_run_meta` could surface a SIBLING worktree's run as THIS checkout's last
            -- run (the verdict counts are already worktree-scoped, so the two would disagree). Added
            -- in V018 directly (this is the unshipped oracle migration) — no separate migration.
            worktree_id TEXT NOT NULL DEFAULT '',
            started_at INTEGER NOT NULL,
            status TEXT NOT NULL,
            stats_json TEXT NOT NULL DEFAULT '{}'
        , repo_id TEXT NOT NULL DEFAULT '__unassigned__') STRICT;
CREATE TABLE clone_graph_generations(
        generation         INTEGER PRIMARY KEY,
        status             TEXT    NOT NULL CHECK (status IN ('Building', 'Complete')),
        theta_floor        REAL    NOT NULL,
        normalizer_kind    TEXT    NOT NULL,            -- baseline
        normalizer_version INTEGER NOT NULL,            -- NORM_VERSION at build
        source_revision    TEXT    NOT NULL,            -- content_revision() this generation builds toward
        cursor_symbol_id   INTEGER NOT NULL DEFAULT 0,  -- build-local resume point (last symbol_id emitted)
        edges_written      INTEGER NOT NULL DEFAULT 0,
        started_at_ms      INTEGER NOT NULL,
        finished_at_ms     INTEGER
    , postings_written INTEGER NOT NULL DEFAULT 0, repo_id TEXT NOT NULL DEFAULT '__unassigned__', delta_files_applied INTEGER NOT NULL DEFAULT 0, postings_row_count INTEGER NOT NULL DEFAULT 0) STRICT;
CREATE TABLE clone_edges(
        build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation) ON DELETE CASCADE,
        -- Content-anchored endpoints: NO symbol_id FK (#248 rule). Canonical a < b by (path, start_byte).
        a_path           TEXT    NOT NULL,
        a_start_byte     INTEGER NOT NULL,
        a_file_sha       TEXT    NOT NULL,              -- files.sha256 at compute; read-time staleness filter
        b_path           TEXT    NOT NULL,
        b_start_byte     INTEGER NOT NULL,
        b_file_sha       TEXT    NOT NULL,
        overlap          INTEGER NOT NULL,              -- Σ min(freq) = verified_clone overlap
        a_token_len      INTEGER NOT NULL,
        b_token_len      INTEGER NOT NULL,
        similarity       REAL    NOT NULL,              -- overlap/max_len; 1.0 for struct-hash-exact pairs
        edge_source      TEXT    NOT NULL,              -- 'struct_hash' | 'sub_block'
        PRIMARY KEY (build_generation, a_path, a_start_byte, b_path, b_start_byte)
    ) STRICT;
CREATE INDEX idx_clone_edges_b
        ON clone_edges(build_generation, b_path, b_start_byte);
CREATE TABLE embedding_cache(
             input_hash TEXT NOT NULL PRIMARY KEY,
             model_id TEXT NOT NULL,
             embedding_dim INTEGER NOT NULL,
             vector_blob BLOB NOT NULL,
             computed_at_ms INTEGER NOT NULL,
             last_used_at_ms INTEGER NOT NULL
         ) STRICT;
CREATE TABLE clone_subblock_postings(
        build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation) ON DELETE CASCADE,
        token_hash       INTEGER NOT NULL,
        -- Content anchor (reindex-stable), NOT symbol_id (the #248 rule).
        path             TEXT    NOT NULL,
        start_byte       INTEGER NOT NULL,
        file_sha         TEXT    NOT NULL,              -- files.sha256 at compute; read-time staleness key
        PRIMARY KEY (build_generation, token_hash, path, start_byte)
    ) STRICT;
CREATE INDEX idx_clone_subblock_postings_token
        ON clone_subblock_postings(build_generation, token_hash);
CREATE TABLE repos(
        repo_id          TEXT PRIMARY KEY,
        display_name     TEXT NOT NULL,
        registered_at_ms INTEGER NOT NULL
    ) STRICT;
CREATE TABLE repo_roots(
        repo_id          TEXT NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
        root             TEXT NOT NULL,
        registered_at_ms INTEGER NOT NULL,
        PRIMARY KEY(repo_id, root)
    ) STRICT;
CREATE TABLE repo_meta(
        repo_id TEXT NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
        key     TEXT NOT NULL,
        value   TEXT,
        PRIMARY KEY(repo_id, key)
    ) STRICT;
CREATE TABLE "packages"(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            manifest_dir TEXT NOT NULL,
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            local_roots_json TEXT NOT NULL DEFAULT '[]',
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            UNIQUE(repo_id, manifest_dir, commit_sha, worktree_id)
        ) STRICT;
CREATE INDEX idx_packages_scope ON packages(commit_sha, worktree_id);
CREATE TABLE "parser_failures"(
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            message TEXT NOT NULL,
            PRIMARY KEY(repo_id, path)
        ) STRICT;
CREATE TABLE "git_commits"(
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
CREATE TABLE "git_file_changes"(
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
CREATE INDEX idx_git_file_changes_path ON git_file_changes(path);
CREATE INDEX idx_git_file_changes_commit ON git_file_changes(commit_hash);
CREATE TABLE "clone_token_df"(
            repo_id         TEXT    NOT NULL DEFAULT '__unassigned__',
            normalizer_kind TEXT    NOT NULL,
            token_hash      INTEGER NOT NULL,
            df              INTEGER NOT NULL,
            PRIMARY KEY (repo_id, normalizer_kind, token_hash)
        ) STRICT;
CREATE TABLE "clone_refinements"(
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
CREATE TABLE "edge_oracle"(
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
CREATE INDEX idx_edge_oracle_staleness
            ON edge_oracle(file_sha, tool, tool_version);
CREATE INDEX idx_edge_oracle_symbol
            ON edge_oracle(resolved_symbol_id);
CREATE INDEX idx_edge_oracle_anchor
            ON edge_oracle(source_path, callee_start_byte, callee_end_byte, edge_kind);
CREATE TABLE "logical_symbol_monikers"(
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            logical_symbol_id INTEGER NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            moniker TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(repo_id, logical_symbol_id, tool)
        ) STRICT;
CREATE INDEX idx_logical_symbol_monikers_moniker
            ON logical_symbol_monikers(moniker, tool);
CREATE TABLE "dream_findings"(
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
CREATE INDEX idx_dream_findings_status ON dream_findings(status);
CREATE INDEX idx_dream_findings_subject ON dream_findings(kind, subject);
CREATE VIRTUAL TABLE repo_memory_fts USING fts5(
            repo_id UNINDEXED,
            memory_id UNINDEXED,
            title,
            body,
            kind,
            tags,
            tokenize='porter'
        );
CREATE TABLE "files"(
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
            generation INTEGER NOT NULL DEFAULT 0, graph_version INTEGER NOT NULL DEFAULT 0, scope_version INTEGER NOT NULL DEFAULT 0,
            UNIQUE(repo_id, path, commit_sha, worktree_id, generation)
        );
CREATE INDEX idx_files_language ON files(language);
CREATE INDEX idx_files_commit_path ON files(commit_sha, path);
CREATE INDEX idx_files_worktree_path ON files(worktree_id, path);
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
            -- Keyed WITH content_hash (trimmed title + body) so a title OR body edit changes the key
            -- and self-invalidates the stale summary.
            content_hash    TEXT    NOT NULL,
            summary         TEXT    NOT NULL,
            model_id        TEXT,
            prompt_version  TEXT,
            generated_at_ms INTEGER NOT NULL,
            PRIMARY KEY (repo_id, memory_id, content_hash)
        ) STRICT;
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
CREATE TABLE repo_node_edges(
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
            created_at_ms INTEGER NOT NULL, origin TEXT NOT NULL DEFAULT 'local' CHECK (origin IN ('local', 'synced')),
            FOREIGN KEY(source_node_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        ) STRICT;
CREATE INDEX idx_repo_node_edges_source
            ON repo_node_edges(source_node_id, relation);
CREATE INDEX idx_repo_node_edges_target
            ON repo_node_edges(target_kind, target_anchor);
CREATE INDEX idx_clone_subblock_postings_path
             ON clone_subblock_postings(build_generation, path);
CREATE TABLE clone_df_epoch(
             build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation)
                                               ON DELETE CASCADE,
             token_hash       INTEGER NOT NULL,
             df               INTEGER NOT NULL,
             PRIMARY KEY (build_generation, token_hash)
         ) STRICT;
CREATE TABLE oplog_meta(
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         ) STRICT;
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
CREATE TABLE oplog_projected_nodes(
             stream_id    BLOB NOT NULL,
             node_id      TEXT NOT NULL,
             content_json TEXT NOT NULL,
             status       TEXT NOT NULL,
             PRIMARY KEY(stream_id, node_id)
         ) STRICT;
CREATE TABLE oplog_projected_edges(
             stream_id     BLOB NOT NULL,
             edge_key      TEXT NOT NULL,
             spec_json     TEXT NOT NULL,
             resolved_json TEXT,
             PRIMARY KEY(stream_id, edge_key)
         ) STRICT;
CREATE TABLE oplog_fork_evidence(
             stream_id              BLOB NOT NULL,
             entry_hash             BLOB NOT NULL,
             device_fingerprint     BLOB NOT NULL,
             lamport                INTEGER NOT NULL,
             signed_bytes           BLOB NOT NULL,
             conflicting_entry_hash BLOB,
             observed_at_ms         INTEGER NOT NULL,
             PRIMARY KEY(stream_id, entry_hash)
         ) STRICT;
CREATE TABLE oplog_device_identity(
             id            INTEGER PRIMARY KEY CHECK (id = 0),
             seed          BLOB NOT NULL,
             public_key    BLOB NOT NULL,
             fingerprint   BLOB NOT NULL,
             created_at_ms INTEGER NOT NULL
         , x25519_secret BLOB, x25519_public BLOB) STRICT;
CREATE TABLE git_change_couplings(
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
CREATE INDEX idx_git_change_couplings_b
            ON git_change_couplings(repo_id, path_b);
CREATE TABLE external_symbols(
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
CREATE INDEX idx_external_symbols_deprecated
            ON external_symbols(repo_id, tool, commit_sha, worktree_id, deprecated);
CREATE TABLE account_entries(
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
CREATE INDEX account_entries_chain
             ON account_entries(account_id, log_id, device_fingerprint, seq);
CREATE UNIQUE INDEX account_accepted_slot
             ON account_entries(account_id, log_id, device_fingerprint, seq) WHERE accepted = 1;
CREATE TABLE account_entry_status(
             entry_hash BLOB PRIMARY KEY,
             status     TEXT NOT NULL,
             detail     TEXT
         ) STRICT;
CREATE TABLE account_pre_verify(
             signed_hash         BLOB    PRIMARY KEY,
             entry_hash          BLOB    NOT NULL,
             claimed_account_id  BLOB    NOT NULL,
             claimed_fingerprint BLOB    NOT NULL,
             raw_bytes           BLOB    NOT NULL,
             received_at_ms      INTEGER NOT NULL
         ) STRICT;
CREATE INDEX account_pre_verify_account
             ON account_pre_verify(claimed_account_id);
CREATE INDEX idx_papertrail_refs_item
             ON papertrail_refs(repo_id, tracker, project, item_kind, item_key);
CREATE UNIQUE INDEX idx_papertrail_refs_unique
             ON papertrail_refs(repo_id, tracker, project, COALESCE(item_kind, ''), item_key,
                                source_kind, COALESCE(source_path, ''),
                                COALESCE(source_commit, ''), source_text);
CREATE TABLE account_auth_state(
             account_id        BLOB PRIMARY KEY,
             classification    TEXT NOT NULL,
             contested_depth   INTEGER,
             successor_account_id BLOB,
             effective_count   INTEGER NOT NULL
         ) STRICT;
CREATE TABLE account_roster_history(
             roster_ref         BLOB PRIMARY KEY,
             account_id         BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             role               TEXT NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         , control_boundary TEXT NOT NULL DEFAULT 'open', control_seq BLOB, control_hash BLOB, secrets_boundary TEXT NOT NULL DEFAULT 'open', secrets_seq BLOB, secrets_hash BLOB) STRICT;
CREATE INDEX account_roster_history_account
             ON account_roster_history(account_id, device_fingerprint);
CREATE TABLE account_owner_incarnations(
             owner_id           BLOB PRIMARY KEY,
             account_id         BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         , control_boundary TEXT NOT NULL DEFAULT 'open', control_seq BLOB, control_hash BLOB, secrets_boundary TEXT NOT NULL DEFAULT 'open', secrets_seq BLOB, secrets_hash BLOB) STRICT;
CREATE INDEX account_owner_incarnations_account
             ON account_owner_incarnations(account_id, device_fingerprint);
CREATE TABLE account_stream_ownership(
             stream_id      BLOB PRIMARY KEY,
             account_id     BLOB NOT NULL,
             own_id         BLOB NOT NULL,
             effective_at   INTEGER NOT NULL
         ) STRICT;
CREATE INDEX account_stream_ownership_account
             ON account_stream_ownership(account_id);
CREATE TABLE account_stream_grants(
             grant_id           BLOB PRIMARY KEY,
             owner_account_id   BLOB NOT NULL,
             stream_id          BLOB NOT NULL,
             grantee_account_id BLOB NOT NULL,
             role               TEXT NOT NULL,
             effective_at       INTEGER NOT NULL,
             closed_at          INTEGER
         ) STRICT;
CREATE INDEX account_stream_grants_owner
             ON account_stream_grants(owner_account_id, stream_id, grantee_account_id);
CREATE TABLE account_stream_grant_cuts(
             grant_id           BLOB NOT NULL,
             owner_account_id   BLOB NOT NULL,
             device_fingerprint BLOB NOT NULL,
             -- Fixed-width big-endian bytes preserve the full protocol u64 domain and sort in
             -- unsigned numeric order; SQLite INTEGER is signed and would reject high cuts.
             seq                BLOB NOT NULL CHECK(length(seq) = 8),
             entry_hash         BLOB NOT NULL,
             PRIMARY KEY(grant_id, device_fingerprint)
         ) STRICT;
CREATE INDEX account_stream_grant_cuts_owner
             ON account_stream_grant_cuts(owner_account_id, grant_id);
CREATE TABLE account_roster_content_boundaries(
             roster_ref BLOB NOT NULL,
             account_id BLOB NOT NULL,
             stream_id  BLOB NOT NULL,
             seq        BLOB NOT NULL CHECK(length(seq) = 8),
             entry_hash BLOB NOT NULL CHECK(length(entry_hash) = 32),
             PRIMARY KEY(roster_ref, stream_id)
         ) STRICT;
CREATE INDEX account_roster_content_boundaries_account
             ON account_roster_content_boundaries(account_id, roster_ref);
CREATE TABLE content_entries(
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
CREATE INDEX content_entries_chain
             ON content_entries(stream_id, author_account_id, device_fingerprint, seq);
CREATE INDEX content_entries_predecessor
             ON content_entries(prev_hash, stream_id, author_account_id, device_fingerprint);
CREATE UNIQUE INDEX content_accepted_slot
             ON content_entries(stream_id, author_account_id, device_fingerprint, seq)
             WHERE accepted = 1;
CREATE TABLE content_entry_status(
             entry_hash BLOB PRIMARY KEY CHECK(length(entry_hash) = 32),
             status     TEXT NOT NULL,
             detail     TEXT
         ) STRICT;
CREATE TABLE content_pre_verify(
             signed_hash               BLOB    PRIMARY KEY CHECK(length(signed_hash) = 32),
             entry_hash                BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             claimed_stream_id         BLOB    NOT NULL CHECK(length(claimed_stream_id) = 32),
             claimed_author_account_id BLOB    NOT NULL CHECK(length(claimed_author_account_id) = 32),
             claimed_fingerprint       BLOB    NOT NULL CHECK(length(claimed_fingerprint) = 32),
             roster_ref                BLOB    NOT NULL CHECK(length(roster_ref) = 32),
             raw_bytes                 BLOB    NOT NULL,
             received_at_ms            INTEGER NOT NULL
         ) STRICT;
CREATE INDEX content_pre_verify_author
             ON content_pre_verify(claimed_author_account_id, roster_ref);
CREATE TABLE oplog_local_account(
             id                 INTEGER PRIMARY KEY CHECK (id = 0),
             genesis_entry_hash BLOB NOT NULL CHECK (length(genesis_entry_hash) = 32),
             created_at_ms      INTEGER NOT NULL
         ) STRICT;
CREATE TABLE content_projected_nodes(
             stream_id    BLOB NOT NULL,
             node_id      TEXT NOT NULL,
             content_json TEXT NOT NULL,
             status       TEXT NOT NULL,
             PRIMARY KEY(stream_id, node_id)
         ) STRICT;
CREATE TABLE content_projected_edges(
             stream_id     BLOB NOT NULL,
             edge_key      TEXT NOT NULL,
             spec_json     TEXT NOT NULL,
             resolved_json TEXT, present INTEGER NOT NULL DEFAULT 1,
             PRIMARY KEY(stream_id, edge_key)
         ) STRICT;
CREATE TABLE sync_security_events(
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
CREATE UNIQUE INDEX sync_security_events_dedup
             ON sync_security_events(kind, entry_hash);
CREATE TABLE papertrail_distill_queue(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tracker TEXT NOT NULL, project TEXT NOT NULL,
            item_kind TEXT NOT NULL, item_key TEXT NOT NULL,
            enqueued_at_ms INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            raw_reply TEXT,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__'
        ) STRICT;
CREATE UNIQUE INDEX idx_papertrail_distill_queue_key
            ON papertrail_distill_queue(repo_id, tracker, project, item_kind, item_key);
CREATE TABLE papertrail_distill_runs(
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
CREATE TABLE papertrail_distill_sources(
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
CREATE UNIQUE INDEX idx_papertrail_distill_sources_ordinal
            ON papertrail_distill_sources(
                repo_id, tracker, project, item_kind, item_key, source_ordinal
            );
CREATE INDEX idx_papertrail_distill_sources_identity
            ON papertrail_distill_sources(
                repo_id, tracker, project, source_item_kind, source_item_key, source_kind, source_id
            );
CREATE TABLE papertrail_distill_units(
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
CREATE UNIQUE INDEX idx_papertrail_distill_units_ordinal
            ON papertrail_distill_units(
                repo_id, tracker, project, item_kind, item_key, unit_ordinal
            );
CREATE INDEX idx_papertrail_distill_units_source
            ON papertrail_distill_units(
                repo_id, tracker, project, item_kind, item_key, source_ordinal, unit_ordinal
            );
CREATE TABLE papertrail_distill_fix_diffs(
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
CREATE UNIQUE INDEX idx_papertrail_distill_fix_diffs_file
            ON papertrail_distill_fix_diffs(
                repo_id, tracker, project, item_kind, item_key, commit_sha, path
            );
CREATE TABLE papertrail_distill_xrefs(
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
CREATE UNIQUE INDEX idx_papertrail_distill_xrefs_ordinal
            ON papertrail_distill_xrefs(
                repo_id, tracker, project, item_kind, item_key, xref_ordinal
            );
CREATE TABLE "content_streams_pending_refold"(
                 stream_id           BLOB    PRIMARY KEY CHECK(length(stream_id) = 32),
                 reason_mask         INTEGER NOT NULL DEFAULT 1 CHECK(reason_mask BETWEEN 1 AND 3),
                 first_enqueued_at_ms INTEGER NOT NULL DEFAULT 0,
                 last_enqueued_at_ms  INTEGER NOT NULL DEFAULT 0
             ) STRICT;
CREATE INDEX content_streams_pending_refold_order
             ON content_streams_pending_refold(first_enqueued_at_ms, stream_id);
CREATE TABLE content_stream_stats(
             stream_id       BLOB    PRIMARY KEY CHECK(length(stream_id) = 32),
             candidate_count INTEGER NOT NULL CHECK(candidate_count >= 0),
             candidate_bytes INTEGER NOT NULL CHECK(candidate_bytes >= 0)
         ) STRICT;
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
         END;
CREATE TABLE content_digest_state(
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    state       TEXT    NOT NULL,   -- 64 lowercase hex chars: 4 LE u64 lanes
    rows_folded INTEGER NOT NULL
) STRICT;
CREATE TRIGGER files_content_digest_ai
AFTER INSERT ON files WHEN NEW.kind != 'deleted'
BEGIN
    UPDATE content_digest_state
       SET state = rr_content_digest_fold(state, NEW.path, NEW.sha256, NEW.kind, 1),
           rows_folded = rows_folded + 1
     WHERE id = 1;
END;
CREATE TRIGGER files_content_digest_ad
AFTER DELETE ON files WHEN OLD.kind != 'deleted'
BEGIN
    UPDATE content_digest_state
       SET state = rr_content_digest_fold(state, OLD.path, OLD.sha256, OLD.kind, -1),
           rows_folded = rows_folded - 1
     WHERE id = 1;
END;
CREATE TRIGGER files_content_digest_au
AFTER UPDATE OF path, sha256, kind ON files
BEGIN
    UPDATE content_digest_state
       SET state = rr_content_digest_fold(
                       rr_content_digest_fold(state, OLD.path, OLD.sha256, OLD.kind, -1),
                       NEW.path, NEW.sha256, NEW.kind, 1),
           rows_folded = rows_folded
                         + (NEW.kind != 'deleted') - (OLD.kind != 'deleted')
     WHERE id = 1;
END;
CREATE TABLE table_sync_entries(
             entry_hash         BLOB    NOT NULL PRIMARY KEY,
             stream_id          BLOB    NOT NULL,
             device_fingerprint BLOB    NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB,
             signed_bytes       BLOB    NOT NULL,
             received_at_ms     INTEGER NOT NULL, pending_reason TEXT, pending_projector_version INTEGER, quarantine_reason TEXT,
             UNIQUE(stream_id, device_fingerprint, lamport)
         ) STRICT;
CREATE INDEX table_sync_entries_stream_lamport
             ON table_sync_entries(stream_id, lamport);
CREATE TABLE account_candidate_reservations(
             reservation_id BLOB    NOT NULL PRIMARY KEY CHECK(length(reservation_id) = 32),
             account_id     BLOB    NOT NULL CHECK(length(account_id) = 32),
             reserved_entries INTEGER NOT NULL CHECK(reserved_entries >= 0),
             reserved_bytes   INTEGER NOT NULL CHECK(reserved_bytes >= 0),
             expires_at_ms    INTEGER NOT NULL
         , reserved_targets INTEGER NOT NULL DEFAULT 0) STRICT;
CREATE INDEX account_candidate_reservations_account_expiry
             ON account_candidate_reservations(account_id, expires_at_ms);
CREATE TABLE "sync_invites"(
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
         ) STRICT;
CREATE INDEX sync_invites_account_expiry
             ON sync_invites(account_id, expires_at_ms);
CREATE INDEX table_sync_entries_pending
             ON table_sync_entries(pending_reason)
             WHERE pending_reason IS NOT NULL;
CREATE TRIGGER memories_lens_revision_insert
                  AFTER INSERT ON repo_memories
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER memories_lens_revision_delete
                 AFTER DELETE ON repo_memories
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER memories_lens_revision_update
                 AFTER UPDATE ON repo_memories
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER papertrail_items_lens_revision_insert
                  AFTER INSERT ON papertrail_items
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER papertrail_items_lens_revision_delete
                 AFTER DELETE ON papertrail_items
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER papertrail_items_lens_revision_update
                 AFTER UPDATE ON papertrail_items
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER papertrail_refs_lens_revision_insert
                  AFTER INSERT ON papertrail_refs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER papertrail_refs_lens_revision_delete
                 AFTER DELETE ON papertrail_refs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER papertrail_refs_lens_revision_update
                 AFTER UPDATE ON papertrail_refs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER clone_refinements_lens_revision_insert
                  AFTER INSERT ON clone_refinements
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER clone_refinements_lens_revision_delete
                 AFTER DELETE ON clone_refinements
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER clone_refinements_lens_revision_update
                 AFTER UPDATE ON clone_refinements
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER oracle_runs_lens_revision_insert
                  AFTER INSERT ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER oracle_runs_lens_revision_delete
                 AFTER DELETE ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER oracle_runs_lens_revision_update
                 AFTER UPDATE ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER clone_graph_generations_lens_revision_insert
         AFTER INSERT ON clone_graph_generations
         WHEN EXISTS (
             SELECT 1 FROM repo_meta
             WHERE repo_id = NEW.repo_id AND key = 'clone_graph_live_generation'
               AND CAST(value AS INTEGER) = NEW.generation
         )
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value) VALUES (NEW.repo_id, 'lens_enrichment_revision', '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
CREATE TRIGGER clone_graph_generations_lens_revision_delete
         AFTER DELETE ON clone_graph_generations
         WHEN EXISTS (
             SELECT 1 FROM repo_meta
             WHERE repo_id = OLD.repo_id AND key = 'clone_graph_live_generation'
               AND CAST(value AS INTEGER) = OLD.generation
         )
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value) VALUES (OLD.repo_id, 'lens_enrichment_revision', '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
CREATE TRIGGER clone_graph_generations_lens_revision_update
         AFTER UPDATE ON clone_graph_generations
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value)
             SELECT NEW.repo_id, 'lens_enrichment_revision', '1'
             WHERE EXISTS (
                 SELECT 1 FROM repo_meta
                 WHERE repo_id = NEW.repo_id AND key = 'clone_graph_live_generation'
                   AND CAST(value AS INTEGER) = NEW.generation
             )
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
             INSERT INTO repo_meta(repo_id, key, value)
             SELECT OLD.repo_id, 'lens_enrichment_revision', '1'
             WHERE (OLD.repo_id != NEW.repo_id OR OLD.generation != NEW.generation)
               AND EXISTS (
                 SELECT 1 FROM repo_meta
                 WHERE repo_id = OLD.repo_id AND key = 'clone_graph_live_generation'
                   AND CAST(value AS INTEGER) = OLD.generation
             )
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
CREATE TABLE table_sync_gapped_entries(
             entry_hash         BLOB    NOT NULL PRIMARY KEY,
             stream_id          BLOB    NOT NULL,
             device_fingerprint BLOB    NOT NULL,
             lamport            INTEGER NOT NULL,
             prev_hash          BLOB    NOT NULL,
             signed_bytes       BLOB    NOT NULL,
             gapped_at_ms       INTEGER NOT NULL
         ) STRICT;
CREATE INDEX table_sync_gapped_entries_child
             ON table_sync_gapped_entries(
                 stream_id, device_fingerprint, prev_hash, lamport, entry_hash);
CREATE INDEX table_sync_gapped_entries_chain_lamport
             ON table_sync_gapped_entries(stream_id, device_fingerprint, lamport);
CREATE INDEX table_sync_gapped_entries_predecessor
             ON table_sync_gapped_entries(
                 stream_id, prev_hash, device_fingerprint, entry_hash);
CREATE TABLE account_repo_incarnation_current(
             account_id       BLOB NOT NULL CHECK(length(account_id) = 32),
             repository_id    TEXT NOT NULL,
             incarnation_ref  BLOB CHECK(incarnation_ref IS NULL OR length(incarnation_ref) = 32),
             PRIMARY KEY(account_id, repository_id)
         ) STRICT;
CREATE TABLE table_sync_chain_tips(
             stream_id          BLOB    NOT NULL CHECK(length(stream_id) = 32),
             device_fingerprint BLOB    NOT NULL CHECK(length(device_fingerprint) = 32),
             lamport            INTEGER NOT NULL,
             entry_hash         BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             PRIMARY KEY(stream_id, device_fingerprint)
         ) STRICT;
CREATE INDEX table_sync_chain_tips_stream_lamport
             ON table_sync_chain_tips(stream_id, lamport);
CREATE TABLE table_sync_streams(
                 stream_id       BLOB NOT NULL PRIMARY KEY CHECK(length(stream_id) = 32),
                 repo_id         TEXT NOT NULL,
                 account_id      BLOB NOT NULL CHECK(length(account_id) = 32),
                 incarnation_ref BLOB NOT NULL CHECK(length(incarnation_ref) = 32),
                 scope_id        TEXT NOT NULL,
                 UNIQUE(repo_id, account_id, incarnation_ref, scope_id)
             ) STRICT;
CREATE TABLE sync_published_rows(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, synced_hash TEXT NOT NULL,
                 spec_version INTEGER NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;
CREATE TABLE sync_row_clocks(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, lamport INTEGER NOT NULL,
                 device_fingerprint TEXT NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;
CREATE TABLE sync_row_tombstones(
                 stream_id BLOB NOT NULL CHECK(length(stream_id) = 32), repo_id TEXT NOT NULL,
                 table_name TEXT NOT NULL, row_pk TEXT NOT NULL, lamport INTEGER NOT NULL,
                 device_fingerprint TEXT NOT NULL,
                 PRIMARY KEY(stream_id, table_name, row_pk)
             ) STRICT;
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
               -- `callee_start_byte IS NOT NULL` candidate filter stays correct — import rows leave
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
CREATE TRIGGER edges_view_insert INSTEAD OF INSERT ON edges BEGIN
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
CREATE TRIGGER edges_view_update INSTEAD OF UPDATE ON edges BEGIN
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
CREATE TRIGGER edges_view_delete INSTEAD OF DELETE ON edges BEGIN
            DELETE FROM edges_data WHERE id = OLD.id;
        END;
CREATE INDEX idx_files_repo_generation_graph_version
                 ON files(repo_id, generation, graph_version);
CREATE INDEX idx_files_repo_generation_scope_version
                 ON files(repo_id, generation, scope_version);
CREATE TRIGGER memories_lane_revision_insert
                  AFTER INSERT ON repo_memories
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_memories_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER memories_lane_revision_delete
                 AFTER DELETE ON repo_memories
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_memories_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER memories_lane_revision_update
                 AFTER UPDATE ON repo_memories
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_memories_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_memories_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER papertrail_items_lane_revision_insert
                  AFTER INSERT ON papertrail_items
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_papertrail_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER papertrail_items_lane_revision_delete
                 AFTER DELETE ON papertrail_items
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_papertrail_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER papertrail_items_lane_revision_update
                 AFTER UPDATE ON papertrail_items
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_papertrail_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_papertrail_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER papertrail_refs_lane_revision_insert
                  AFTER INSERT ON papertrail_refs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_papertrail_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER papertrail_refs_lane_revision_delete
                 AFTER DELETE ON papertrail_refs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_papertrail_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER papertrail_refs_lane_revision_update
                 AFTER UPDATE ON papertrail_refs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_papertrail_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_papertrail_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER clone_refinements_lane_revision_insert
                  AFTER INSERT ON clone_refinements
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_clones_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER clone_refinements_lane_revision_delete
                 AFTER DELETE ON clone_refinements
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_clones_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER clone_refinements_lane_revision_update
                 AFTER UPDATE ON clone_refinements
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_clones_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_clones_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER oracle_symbols_lane_revision_insert
                  AFTER INSERT ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_symbols_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER oracle_symbols_lane_revision_delete
                 AFTER DELETE ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_symbols_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER oracle_symbols_lane_revision_update
                 AFTER UPDATE ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_symbols_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_symbols_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER oracle_clones_lane_revision_insert
                  AFTER INSERT ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_clones_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER oracle_clones_lane_revision_delete
                 AFTER DELETE ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_clones_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                 END;
CREATE TRIGGER oracle_clones_lane_revision_update
                 AFTER UPDATE ON oracle_runs
                 BEGIN
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT NEW.repo_id, 'lens_clones_revision', '1'
                     WHERE EXISTS (SELECT 1 FROM repos WHERE repo_id = NEW.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                     INSERT INTO repo_meta(repo_id, key, value)
                     SELECT OLD.repo_id, 'lens_clones_revision', '1'
                     WHERE OLD.repo_id != NEW.repo_id
                       AND EXISTS (SELECT 1 FROM repos WHERE repo_id = OLD.repo_id)
                     ON CONFLICT(repo_id, key) DO UPDATE SET
                         value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
                  END;
CREATE TRIGGER clone_graph_generations_lane_revision_insert
         AFTER INSERT ON clone_graph_generations
         WHEN EXISTS (
             SELECT 1 FROM repo_meta
             WHERE repo_id = NEW.repo_id AND key = 'clone_graph_live_generation'
               AND CAST(value AS INTEGER) = NEW.generation
         )
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value) VALUES (NEW.repo_id, 'lens_clones_revision', '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
CREATE TRIGGER clone_graph_generations_lane_revision_delete
         AFTER DELETE ON clone_graph_generations
         WHEN EXISTS (
             SELECT 1 FROM repo_meta
             WHERE repo_id = OLD.repo_id AND key = 'clone_graph_live_generation'
               AND CAST(value AS INTEGER) = OLD.generation
         )
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value) VALUES (OLD.repo_id, 'lens_clones_revision', '1')
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
CREATE TRIGGER clone_graph_generations_lane_revision_update
         AFTER UPDATE ON clone_graph_generations
         BEGIN
             INSERT INTO repo_meta(repo_id, key, value)
             SELECT NEW.repo_id, 'lens_clones_revision', '1'
             WHERE EXISTS (
                 SELECT 1 FROM repo_meta
                 WHERE repo_id = NEW.repo_id AND key = 'clone_graph_live_generation'
                   AND CAST(value AS INTEGER) = NEW.generation
             )
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
             INSERT INTO repo_meta(repo_id, key, value)
             SELECT OLD.repo_id, 'lens_clones_revision', '1'
             WHERE (OLD.repo_id != NEW.repo_id OR OLD.generation != NEW.generation)
               AND EXISTS (
                 SELECT 1 FROM repo_meta
                 WHERE repo_id = OLD.repo_id AND key = 'clone_graph_live_generation'
                   AND CAST(value AS INTEGER) = OLD.generation
             )
             ON CONFLICT(repo_id, key) DO UPDATE SET
                 value = CAST(COALESCE(value, '0') AS INTEGER) + 1;
         END;
CREATE TABLE "repo_memory_bindings"(
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
CREATE INDEX idx_repo_memory_bindings_logical_symbol
             ON repo_memory_bindings(logical_symbol_id);
CREATE INDEX idx_repo_memory_bindings_symbol ON repo_memory_bindings(symbol_id);
CREATE INDEX idx_repo_memory_bindings_chunk ON repo_memory_bindings(chunk_id);
CREATE INDEX idx_repo_memory_bindings_edge ON repo_memory_bindings(edge_id);
CREATE INDEX idx_repo_memory_bindings_path ON repo_memory_bindings(path);
CREATE TABLE table_sync_readoption_work(
              account_id BLOB NOT NULL CHECK(length(account_id) = 32),
              device_fingerprint BLOB NOT NULL CHECK(length(device_fingerprint) = 32),
              stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
              roster_ref BLOB NOT NULL CHECK(length(roster_ref) = 32),
              removed_at_epoch INTEGER NOT NULL,
              enqueued_at_ms INTEGER NOT NULL,
              processed_at_ms INTEGER,
              PRIMARY KEY(account_id, device_fingerprint, stream_id)
          ) STRICT;
CREATE INDEX table_sync_readoption_work_pending
              ON table_sync_readoption_work(account_id, stream_id)
              WHERE processed_at_ms IS NULL;
CREATE TABLE table_sync_retained_floors(
             stream_id          BLOB    NOT NULL CHECK(length(stream_id) = 32),
             device_fingerprint BLOB    NOT NULL CHECK(length(device_fingerprint) = 32),
             lamport            INTEGER NOT NULL,
             entry_hash         BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             compacted_at_ms    INTEGER NOT NULL,
             PRIMARY KEY(stream_id, device_fingerprint)
         ) STRICT;
CREATE TABLE "table_sync_readoption_audit"(
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
CREATE INDEX table_sync_readoption_audit_stream
             ON table_sync_readoption_audit(account_id, stream_id);
CREATE TABLE "papertrail_distill"(
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
             -- COUNTS, not booleans (quotes_materialized is the evidence-unit count) — no 0/1 CHECK.
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
CREATE TABLE "papertrail_distill_edges"(
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
CREATE TABLE "papertrail_distill_alternatives"(
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
CREATE TABLE "papertrail_distill_record_commits"(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             commit_sha TEXT NOT NULL,
             created_at_ms INTEGER NOT NULL DEFAULT 0,
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, commit_sha)
         ) STRICT;
CREATE TABLE "papertrail_distill_evidence"(
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
CREATE TABLE "papertrail_distill_anchors"(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             candidate_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(candidate_ordinal >= 0),
             anchor_kind TEXT NOT NULL,
             logical_symbol_id TEXT,
             file_path TEXT,
             name TEXT NOT NULL,
             resolved INTEGER NOT NULL DEFAULT 0,
             selected INTEGER NOT NULL DEFAULT 0 CHECK(selected IN (0, 1)),
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, candidate_ordinal)
         ) STRICT;
CREATE INDEX idx_papertrail_distill_anchors_symbol
             ON papertrail_distill_anchors(repo_id, logical_symbol_id);
CREATE INDEX idx_papertrail_distill_anchors_candidate
             ON papertrail_distill_anchors(repo_id, tracker, project, item_kind, item_key,
                                           candidate_ordinal);
CREATE INDEX idx_papertrail_distill_anchors_selected
             ON papertrail_distill_anchors(repo_id, tracker, project, item_kind, item_key,
                                           candidate_ordinal) WHERE selected = 1;
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('001_sqlite_storage_baseline',1786958291300,'sha256:rag-rat-sqlite-baseline-v1','SQLite storage baseline with FTS, tree-sitter graph edges, git/GitHub, and local AI metadata');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('002_embedding_vector_metadata',1786958291300,'sha256:rag-rat-embedding-vector-metadata-v2','Add embedding model dimension metadata and per-vector dimensions for hybrid vector search');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('003_derived_artifact_reconcile_metadata',1786958291300,'sha256:rag-rat-derived-artifact-reconcile-metadata-v3','Add model version, retry metadata, summaries, and reconcile meta for diff-based derived artifact reconciliation');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('004_edge_source_target_spans',1786958291300,'sha256:rag-rat-edge-source-target-spans-v4','Add exact source call-site spans and resolved target line spans to graph edges');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('005_edge_evidence_and_resolution',1786958291301,'sha256:rag-rat-edge-evidence-resolution-v5','Add raw graph edge evidence, receiver hints, qualified targets, and resolution reasons');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('006_embedding_policy_and_input_hash',1786958291301,'sha256:rag-rat-embedding-policy-input-hash-v6','Add embedding eligibility policy, priority, bounded input hash, and reconcile throughput metadata');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('007_logical_symbol_groups',1786958291301,'sha256:rag-rat-logical-symbol-groups-v7','Add logical symbol groups for cfg variants and duplicate definitions');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('008_commit_addressable_worktrees',1786958291301,'sha256:rag-rat-commit-addressable-worktrees-v8','Add commit_sha and worktree_id to files table for multi-worktree / multi-branch support');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('009_github_ref_sync_state',1786958291301,'sha256:rag-rat-github-ref-sync-state-v9','Add per-GitHub-ref sync state for resumable papertrail cache updates');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('010_symbol_facts',1786958291301,'sha256:rag-rat-symbol-facts-v10','Add normalized symbol facts for parsed language metadata such as Rust attributes');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('011_repo_memories',1786958291301,'sha256:rag-rat-repo-memories-v11','Add source-anchored repo memories bound to symbols, chunks, paths, and papertrail refs');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('012_repo_memory_call_paths',1786958291301,'sha256:rag-rat-repo-memory-call-paths-v12','Add edge and call-path memory bindings for graph traversal surfacing');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('013_graph_file_lookup_indexes',1786958291301,'sha256:rag-rat-graph-file-lookup-indexes-v13','Add graph file lookup indexes for ownership clustering and file-level graph summaries');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('014_repo_memory_binding_signals',1786958291307,'sha256:rag-rat-repo-memory-binding-signals-v14','Add symbol_kind + signature_hash to repo_memory_bindings for durable cross-file relocation');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('015_repo_memory_call_path_edges',1786958291307,'sha256:rag-rat-repo-memory-call-path-edges-v15','Add ordered edge fingerprints behind server-derived call-path hashes for validation');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('016_symbol_line_spans',1786958291312,'sha256:rag-rat-symbol-line-spans-v16','Store start_line/end_line on symbols so readers skip the per-symbol chunk-containment subqueries');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('017_edge_callee_byte_range',1786958291312,'sha256:rag-rat-edge-callee-byte-range-v17','Add callee identifier byte range to edges for the SCIP occurrence join (#61 prerequisite)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('018_scip_oracle_tables',1786958291313,'sha256:rag-rat-scip-oracle-tables-v18','Add oracle_runs + edge_oracle side tables for SCIP compiler-grade edge resolution (#68)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('019_scip_moniker_anchors',1786958291318,'sha256:rag-rat-scip-moniker-anchors-v19','Add logical_symbol_monikers + moniker provenance and relocation reason on repo memory bindings (#70)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('020_edge_string_interning',1786958291319,'sha256:rag-rat-edge-string-interning-v20','Normalize repeated edge strings into the name_strings dictionary behind the edges compatibility view (#79)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('021_symbol_scope_path',1786958291321,'sha256:rag-rat-symbol-scope-path-v21','Add symbols.scope_path (semantic enclosing-scope path) for scope-aware edge resolution (#61)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('022_per_package_import_scope',1786958291323,'sha256:rag-rat-per-package-import-scope-v22','Add packages table + dedicated edge import-scope columns for per-package, module-aware import resolution (#61)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('023_dispatch_edge_facts_view_exclusion',1786958291324,'sha256:rag-rat-dispatch-edge-facts-view-exclusion-v23','Recreate the edges compatibility view to exclude internal dispatch FACT rows from query-layer reads (#200)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('024_files_has_test_code',1786958291324,'sha256:rag-rat-files-has-test-code-v24','Add files.has_test_code flag (precomputed test-marker detection) so impact_surface avoids a chunks.text scan (#77)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('025_chunk_text_compression_tables',1786958291324,'sha256:rag-rat-chunk-text-compression-tables-v25','Add chunk_text (zstd blob) + chunk_text_dict (shared dictionary) tables for compressed chunk text (#77)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('026_contentless_chunk_fts',1786958291325,'sha256:rag-rat-contentless-chunk-fts-v26','Recreate chunk_fts as a contentless FTS5 index and repopulate it, so chunks.text can be dropped (#77 Phase 2)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('027_drop_chunks_text',1786958291325,'sha256:rag-rat-drop-chunks-text-v27','Build the compressed chunk_text store from chunks.text, then drop the chunks.text column (#77 Phase 2)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('028_intern_symbol_qualified_names',1786958291325,'sha256:rag-rat-intern-symbol-qualified-names-v28','Intern symbols/logical_symbols qualified_name into the shared name_strings pool, then drop the columns (#224)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('029_clone_fingerprint_tables',1786958291325,'sha256:rag-rat-clone-fingerprint-tables-v29','Add symbol_fingerprints + symbol_token_postings + clone_token_df + clone_refinements for clone detection (#215)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('030_clone_refinements_lcs_sampled',1786958291325,'sha256:rag-rat-clone-refinements-lcs-sampled-v30','Add clone_refinements.lcs_sampled (additive; heals indexes already at V029)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('031_edge_oracle_content_anchor',1786958291325,'sha256:rag-rat-edge-oracle-content-anchor-v31','Rebuild edge_oracle content-anchored (drop edges_data FK + edge_id PK) so verdicts survive reindex (#248)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('032_clone_token_bag_blob',1786958291325,'sha256:rag-rat-clone-token-bag-blob-v32','Add symbol_fingerprints.token_bag BLOB + drop symbol_token_postings (BLOB-pack the clone token bag) (#231)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('033_dream_findings',1786958291325,'sha256:rag-rat-dream-findings-v33','Add dream_findings (dream-mode worklist: findings ABOUT memories, identity-keyed supersede/decay, never mutate memories) (#122)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('034_clone_graph_precompute',1786958291326,'sha256:rag-rat-clone-graph-precompute-v34','Add clone_graph_generations + clone_edges (content-anchored precomputed clone-edge graph so find_clones reads a persisted graph instead of recomputing candidate pairs every query)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('035_symbols_is_test',1786958291327,'sha256:rag-rat-symbols-is-test-v35','Add symbols.is_test (cross-language test-code marker: test-file path, Rust #[test]/#[cfg(test)], Kotlin @Test, Python test_*/TestCase) so clone detection can exclude tests from the corpus');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('036_embedding_content_cache',1786958291327,'sha256:rag-rat-embedding-content-cache-v36','Add embedding_cache (content-addressed vectors keyed by input_hash) so embeddings survive reindex / branch-switch and reconcile reuses unchanged content across contexts instead of re-embedding; seeded from current chunk_embeddings (#357)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('037_clone_subblock_postings',1786958291330,'sha256:rag-rat-clone-subblock-postings-v37','Add clone_subblock_postings (persisted content-anchored sub-block postings, generation-staged like clone_edges) + clone_graph_generations.postings_written so the write-time clone check does a bounded indexed lookup instead of rebuilding the RAM index and scales past the 40k guard (#296)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('038_repos_registry',1786958291330,'sha256:rag-rat-repos-registry-v38','Add repos registry + repo_roots + repo_meta (per-machine repo identity registry and per-repo key/value store) with the __unassigned__ adoption placeholder — the substrate for the global consolidated database and repo_id scoping (memory-sync phase A)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('039_per_repo_meta',1786958291330,'sha256:rag-rat-per-repo-meta-v39','Relocate the per-repo singleton meta keys from the global index_meta / reconcile_meta into repo_meta under the __unassigned__ placeholder (memory-sync phase A2)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('040_repo_id_core_scoping',1786958291375,'sha256:rag-rat-repo-id-core-scoping-v40','Add repo_id scoping to the core tables (files, packages, logical_symbols, docs, parser_failures, git_commits + git_file_changes) with rebuilt UNIQUE / PK keys and the re-pointed commit_fts external content, plus the two active-embedding-model provenance meta keys moved to repo_meta (memory-sync phase A3)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('041_github_repo_id_scoping',1786958291375,'sha256:rag-rat-github-repo-id-scoping-v41','Repo-scope the GitHub papertrail cache: add repo_id to the seven github_* tables (refs, issues, comments, pull_requests, reviews, review_comments, ref_sync) and rebuild github_fts with a repo_id UNINDEXED column, so lexical and papertrail queries in a consolidated database never surface a sibling repo''s refs or issues (memory-sync phase A4)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('042_repo_id_periphery_scoping',1786958291394,'sha256:rag-rat-repo-id-periphery-scoping-v42','Repo-scope the clone / oracle / reconcile / memory periphery: add repo_id to clone_graph_generations, oracle_runs, reconcile_attempts, repo_memories, repo_memory_bindings (additive) and rebuild clone_token_df, clone_refinements, edge_oracle, logical_symbol_monikers, dream_findings with repo_id in their PK / UNIQUE plus repo_memory_fts with a repo_id UNINDEXED column, so clone stats, oracle runs, and memory search in a consolidated database never pool or surface a sibling repo''s rows (memory-sync phase A5)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('043_files_generation',1786958291397,'sha256:rag-rat-files-generation-v43','Add files.generation and widen the UNIQUE key to (repo_id, path, commit_sha, worktree_id, generation), so a full rebuild can stage a fresh generation of every file row alongside the live one and flip readers over atomically instead of clearing-then-reinserting inside one long write-locked transaction (memory-sync phase A6)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('044_github_natural_key_widening',1786958291397,'sha256:rag-rat-github-natural-key-widening-v44','Fold repo_id into the (owner, repo, number)-style GitHub natural keys — widen github_issues / github_pull_requests UNIQUE and github_ref_sync PRIMARY KEY to (repo_id, owner, repo, number) and re-create idx_github_refs_unique with a leading repo_id — so two repos in a consolidated database can each cache the same external issue/PR/ref without one repo''s sync overwriting the other''s row (memory-sync phase A7)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('045_github_child_key_widening',1786958291397,'sha256:rag-rat-github-child-key-widening-v45','Fold repo_id into the id-keyed GitHub child caches — rebuild github_comments / github_reviews / github_review_comments with (repo_id, id) uniqueness, backfilling one copy per owning-parent repo — so two repos sharing an external issue/PR each keep that item''s comments and reviews in their scoped papertrail instead of last-syncer-owns restamping (memory-sync phase A7)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('046_memory_verification_reality_summaries',1786958291398,'sha256:rag-rat-memory-verification-reality-summaries-v46','Add the dream verification sibling tables memory_reality (one derived verdict/check row per memory, keyed (repo_id, memory_id)) and memory_summaries (one per (repo_id, memory_id, content_hash) so a body edit self-invalidates), both STRICT and repo_id-scoped. They hold derived, regenerable data so dream verifies memories without ever mutating a repo_memories row (dream v2 pass 0)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('047_memory_model_failures',1786958291398,'sha256:rag-rat-memory-model-failures-v47','Add memory_model_failures, a repo_id-scoped dream sibling table that records deterministic verdict/compaction model failures with stable enum tokens and input/model freshness stamps, so rejected current attempts do not rerun every dream pass');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('048_memory_payload_json',1786958291399,'sha256:rag-rat-memory-payload-json-v48','Add repo_memories.payload_json, a nullable opaque canonical-JSON payload for polymorphic memory nodes (the Task / Concept kinds), folded into the content_hash so a payload edit self-invalidates the derived dream summary/verdict rows exactly as a title/body edit does');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('049_repo_node_edges',1786958291399,'sha256:rag-rat-repo-node-edges-v49','Add repo_node_edges, the typed content-addressed cross-repo edge set (#464): relation-typed edges from a memory node to another node or a code/github target, with explicit owner + target repo ids and a stable edge_key, no FK to volatile graph rows (only the durable source memory)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('050_clone_delta_maintenance',1786958291400,'sha256:rag-rat-clone-delta-maintenance-v50','Add the clone_subblock_postings (build_generation, path) index and the clone_graph_generations.delta_files_applied counter, so the incremental clone-graph delta pass can delete a changed file''s postings without a table scan and track df drift toward the next full rebuild');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('051_clone_df_epoch',1786958291400,'sha256:rag-rat-clone-df-epoch-v51','Add clone_df_epoch, the per-generation snapshot of clone_token_df taken at each fresh clone-graph build (#479), so the persisted postings and the delta pass read their own build''s frozen token order while the live candidate paths read a clone_token_df that moves again on incremental passes; backfilled from the current (freeze-pinned) df for existing generations');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('052_oplog_storage',1786958291400,'sha256:rag-rat-oplog-storage-v52','Add the memory op-log storage tables (#503, phase B C4): oplog_entries — the layer-1 opaque signed entry log, content-addressed on entry_hash, no FK; and the layer-2 shadow projection (oplog_projected_nodes / oplog_projected_edges) plus oplog_meta, wholly rebuilt by the full-replay fold. Fresh tables (no backfill); nothing is wired to the live write path yet');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('053_oplog_stream_scoping',1786958291401,'sha256:rag-rat-oplog-stream-scoping-v53','Scope the op-log by immutable stream identity (#509): rebuild the still-unwired (and therefore empty) V052 op-log tables with a stream_id dimension — one signed chain per (stream_id, device), UNIQUE(stream_id, device_fingerprint, lamport), projection keyed per stream — and add oplog_fork_evidence, the quarantine that durably preserves BOTH heads of a detected equivocation. Nothing is wired to the live write path yet');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('054_oplog_device_identity',1786958291401,'sha256:rag-rat-oplog-device-identity-v54','Add oplog_device_identity (#513, phase B): the ONE persisted ed25519 keypair per store that the op-log write path signs every entry with — a single-row (id = 0) STRICT table holding the 32-byte seed, its derived public_key, and the sha256(public_key) fingerprint. Store-global, not repo-scoped (a device is a machine identity). Purely additive; nothing is wired to the live write path yet');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('055_binding_downgrade_marker',1786958291402,'sha256:rag-rat-binding-downgrade-marker-v55','Add repo_memory_bindings.downgrade_pending_at_ms (#492), the anchor-status downgrade hysteresis marker: a validate pass that observes a non-gone binding as gone arms the marker instead of stamping, and only a SECOND consecutive gone observation persists the downgrade — so a single torn observation (a validate racing a rebuild window, or a sweep from a narrower checkout) cannot flip a healthy anchor to gone and hand doctor destructive advice');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('056_git_change_couplings',1786958291402,'sha256:rag-rat-git-change-couplings-v56','Add git_change_couplings (#566), the windowed file-pair change-coupling table derived from git_file_changes: one STRICT symmetric row per unordered pair (path_a < path_b) holding raw co-change + endpoint counts over a bounded recency window of eligible commits, keyed (repo_id, path_a, path_b) with a secondary (repo_id, path_b) index. A DerivedIndex table (repo_id-scoped, no FK to the volatile history rows): wholesale-recomputed lazily on the impact_surface read path against a repo_meta ''git_coupling_stamp'', never patched incrementally. Fresh + empty on create; the first git-inclusive impact read fills it');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('057_external_symbols',1786958291402,'sha256:rag-rat-external-symbols-v57','Add external_symbols (#114), the per-moniker dependency contract oracle run parses out of the .scip index.external_symbols (kind, display_name, signature_documentation text, documentation, a derived deprecated flag) — data from_index previously discarded. Oracle-persisted, content/moniker-keyed with NO reindex-cascading FK, checkout-scoped (repo_id, tool, commit_sha, worktree_id) from birth; moniker is the RAW SCIP symbol string so it exact-joins edge_oracle.scip_symbol. Backs the check_library_usage tool that surfaces the current signature/docs at external call sites and flags deprecated usage');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('058_oplog_device_x25519',1786958291404,'sha256:rag-rat-oplog-device-x25519-v58','Add x25519_secret + x25519_public (nullable BLOB) to oplog_device_identity (sync phase C, §5): the device''s X25519 ENCRYPTION keypair beside its ed25519 signing key. Additive on the STRICT table; an existing ed25519-only row is backfilled at the next local_device open via a CAS UPDATE that mirrors the ed25519 mint-if-absent race, so concurrent opens converge on one encryption identity. C1 only mints/persists/validates the key; ECDH + HKDF is C4');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('059_account_candidate_dag',1786958291404,'sha256:rag-rat-account-candidate-dag-v59-signed-envelope-key','The account-log CANDIDATE DAG (sync phase C, §16.1): account_entries (all branches, grow-only, no seq-uniqueness — equivocation heads are first-class; the derived `accepted` flag + the account_accepted_slot partial unique index pin accepted-set uniqueness per slot, I10a), account_entry_status (the projected §16.3 taxonomy), and account_pre_verify (entries whose signing device isn''t yet resolvable, retried on a later DeviceAdd/AccountGenesis arrival). All CREATE ... IF NOT EXISTS + STRICT tables');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('060_papertrail_provider_neutral_schema',1786958291404,'sha256:rag-rat-papertrail-provider-neutral-schema-v60','Normalize the GitHub papertrail cache into the provider-neutral papertrail_* tables (#588): papertrail_items (tracker + item_kind in the natural key, issue-shadow deduped), unified papertrail_comments (reviews fold in behind review_state / anchor_path), papertrail_refs (annotation layer only), papertrail_sync_cursor (one row per repo/tracker/project — the per-ref github_ref_sync state machine is deleted), papertrail_item_tags, and the incrementally-maintained papertrail_fts mirror; backfills mechanically from the seven github_* tables then DROPS them (hard rename, no aliases); renames the memory binding kind github -> tracker (tracker/project/item_key columns backfilled, github_* columns dropped) and the github_last_sync_ms repo_meta key to papertrail_last_sync_ms');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('061_papertrail_ref_item_kind',1786958291404,'sha256:rag-rat-papertrail-ref-item-kind-v61','Preserve the nullable item_kind on papertrail_refs so providers with separate issue/change-request namespaces cannot collapse #N and !N annotations');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('062_papertrail_comment_cursor',1786958291406,'sha256:rag-rat-papertrail-comment-cursor-v62','Split repo-wide comment progress from the item watermark and persist comment pagination only after each stored page');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('063_papertrail_mirror_resume_state',1786958291419,'sha256:rag-rat-papertrail-mirror-resume-state-v63e','Persist item-page, item-thread, Search-tie, per-stream comment-scan, immutable item-delta windows, and full-rewalk state so every stored unit resumes without replay or lost pruning');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('064_account_authority_projection',1786958291433,'sha256:rag-rat-account-authority-projection-v64b','Persist the fully folded account classification, roster and owner incarnations, immutable stream ownership, exact grant incarnations, and revoke device cuts. refold_account rewrites these shadow tables in the same IMMEDIATE transaction as accepted/status, so /3 authority checks never rescan the bounded candidate DAG');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('065_account_authority_boundaries',1786958291433,'sha256:rag-rat-account-authority-boundaries-v65a','Persist closed roster and owner chain boundaries for bounded historical citations');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('066_content_candidate_dag',1786958291433,'sha256:rag-rat-content-candidate-dag-v66a','Persist every structurally valid /3 content candidate, bounded pre-verification work, and derived status while reserving accepted-slot uniqueness for C3 authority acceptance');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('067_papertrail_binding_health',1786958291440,'sha256:rag-rat-papertrail-binding-health-v67e','Persist per-binding attempt, successful probe/mirror, and closed failure state for automatic scheduling and status');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('068_suppressed_edge_candidates',1786958291441,'sha256:rag-rat-suppressed-edge-candidates-v68','Hide suppressed unresolved edge candidates from the compatibility view while retaining them for later incremental re-resolution');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('069_oplog_local_account',1786958291441,'sha256:rag-rat-oplog-local-account-v69','Add oplog_local_account (sync phase C3.4a): the single-row (id = 0) STRICT pointer naming the genesis_entry_hash of this store''s one local account, minted once by local_account and reused so later C3.4 slices author owner-bound /3 content under a stable identity. Store-global, not repo-scoped; purely additive, nothing pre-existing to backfill');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('070_content_projected_tables',1786958291441,'sha256:rag-rat-content-projected-tables-v70','Add content_projected_nodes/content_projected_edges (sync phase C3.4b-i): the stream-keyed memory projection of the accepted /3 content DAG, mirroring the /1 oplog_projected_* shadow tables but updated only when acceptance changes (the content refold), never by the /1 projector sweep — kept separate so a projector-version bump cannot wipe the /3 projection. Purely additive, nothing pre-existing to backfill');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('071_edge_target_qname_index',1786958291441,'sha256:rag-rat-edge-target-qname-index-v71','Add idx_edges_target_qname on edges_data(target_qualified_name_id) so find_callers/trace_callees seed the graph traversal on an indexed id column (MULTI-INDEX OR) instead of full-scanning the edge table when matching unresolved edges by target_qualified_name. Purely additive; CREATE INDEX IF NOT EXISTS, nothing pre-existing to backfill');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('072_content_streams_pending_refold',1786958291441,'sha256:rag-rat-content-streams-pending-refold-v72','Add content_streams_pending_refold (issue #652): the deferred-refold work queue for the /3 content-ingest path. content_ingest no longer folds acceptance per entry (O(n^2) under the writer lock as a stream is built one candidate at a time); it enqueues the stream here and settle_pending_content_refolds folds each dirty stream once. Purely additive; CREATE ... IF NOT EXISTS, nothing pre-existing to backfill');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('073_papertrail_distill_substrate',1786958291441,'sha256:rag-rat-papertrail-distill-substrate-v73','Add papertrail_closing_edges (issue #702): first-class provider-attested issue<->closer edges for the distillation substrate, plus papertrail_items closed_at / resolution / merge_commit_sha (merged-only) / state_normalized (backfilled) / author facets and papertrail_comments author facets. Additive; CREATE IF NOT EXISTS + add_column_if_missing + an idempotent state_normalized backfill');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('074_edges_view_scalar_suppression',1786958291442,'sha256:rag-rat-edges-view-scalar-suppression-v74','Re-install the edges compatibility view so the V068 suppressed-edge exclusion is a scalar compare instead of a per-row NOT IN membership probe (the query_warm regression: the probe taxed every per-hit graph-evidence query). Pure view DDL refresh via ensure_edges_view; no data change');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('075_edges_hidden_flag',1786958291443,'sha256:rag-rat-edges-hidden-flag-v75','Materialize edge visibility as edges_data.hidden and filter the edges view on it (issue #734): visibility is decided once at write time instead of re-deriving the dispatch-fact + suppressed-candidate predicates on every view row. Adds the column, backfills it from the predicate the view WHERE used to evaluate, and refreshes the view via ensure_edges_view');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('076_sync_security_events',1786958291443,'sha256:rag-rat-sync-security-events-v76','Add sync_security_events (sync phase C4.3b, #607): the local-only audit log the sealing-key adoption cross-check writes when an accepted StreamKeyWrap naming this device fails to unwrap (AEAD tag failure) or unwraps to a key whose key_id disagrees with the op''s signed key_id. Never on the wire, never a fold input. Additive; CREATE ... IF NOT EXISTS + a dedup unique index, nothing pre-existing to backfill');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('077_distill_record_store',1786958291444,'sha256:rag-rat-distill-record-store-v77','Add the distillation record store (issue #703): papertrail_distill (derived, regenerable decision records with provenance-facet confidence, not a fused label), plus junction children (evidence with materialized quotes + snapshotted provenance, sym_<hex> anchors, alternatives, mechanical fixing-commits), thread-keyed edges (survive record regeneration), the distill work queue, and per-run stats. Additive; CREATE IF NOT EXISTS, nothing pre-existing to backfill');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('078_distill_anchor_selection',1786958291446,'sha256:rag-rat-distill-anchor-selection-v78','Distinguish mined anchor candidates from model selections (issue #704): add a stable, zero-based candidate ordinal and selected state to papertrail_distill_anchors; deterministically backfill V077 rows in insertion order per thread; enforce ordinal uniqueness and boolean selected values; index selected anchors. Additive; existing anchor identity/path columns are unchanged');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('079_distill_safe_input_snapshot',1786958291449,'sha256:rag-rat-distill-safe-input-snapshot-v79','Add extraction-owned safe-input snapshots for distillation (issue #704): exact ordered title/body/comment sources with full thread and partner identity, provenance, timestamps, and every deterministic block-unit byte span; add prompt_version/model_input_hash model-output stamps. Additive and intentionally does not backfill snapshots from the mutable mirror');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('080_distill_enriched_context',1786958291449,'sha256:rag-rat-distill-enriched-context-v80','Add extraction-owned enriched-context snapshots for distillation (issue #800): per-fix-commit unified diffs restricted to files with symbol anchor candidates, and cross-referenced item titles + opening paragraphs mined from the thread''s outbound papertrail refs. Additive and intentionally does not backfill from mutable git/mirror state');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('081_distill_evidence_source_part',1786958291451,'sha256:rag-rat-distill-evidence-source-part-v81','Persist source-part identity (title|body|comment) on distilled evidence rows (issue #801) so a citation from an item''s title is distinguishable from one in its body (both share the item key as source_id). Nullable and additive: existing rows keep NULL, the drain populates new rows from its snapshot, and no SQL backfill is performed (a re-drain rewrites evidence)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('082_content_refold_queue_and_stats',1786958291455,'sha256:rag-rat-content-refold-queue-and-stats-v82','Extend content_streams_pending_refold with reason bits and deterministic enqueue timestamps, add ordered pending selection, and materialize per-stream candidate count/work bytes from content_entries. SQLite triggers keep the stats exact for inserts, deletes, and mutable stream_id/signed_bytes updates; existing queue rows backfill as content-candidate work with min/max candidate receive times');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('083_logical_group_reason_by_evidence',1786958291456,'sha256:rag-rat-logical-group-reason-by-evidence-v83','Recompute logical_symbols.group_reason from member evidence — the old value asserted cfg_variant for every multi-member group (#855)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('084_chunk_symbol_id',1786958291457,'sha256:rag-rat-chunk-symbol-id-v84','Add chunks.symbol_id: the direct rowid of the symbol a code chunk was cut from, written at index time from the same parse that assigned the symbol its rowid. Replaces position-based chunk→symbol resolution, which could not disambiguate same-name symbols that nest or share a physical line. Nullable; backfills on the next reindex of each file (derived data, no SQL backfill)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('085_sync_origin_and_edge_tombstone',1786958291461,'sha256:rag-rat-sync-origin-and-edge-tombstone-v85','Add repo_memories.origin and repo_node_edges.origin (''local''|''synced'') and content_projected_edges.present. The origin column gates the memory reconcile so a synced row is never re-authored as local /3 content (forging local authorship / re-legitimizing revoked content); the present column retains edge tombstones so a foreign EdgeRemove is honored instead of resurrected in an op-log growth loop. Additive; existing rows default to local/present');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('086_content_digest_state',1786958291461,'sha256:rag-rat-content-digest-state-v86','Incrementally maintain content_revision (#828): add the one-row content_digest_state table and the three files_content_digest_* triggers that fold a 256-bit additive multiset hash of {(path, sha256) : main.files, kind != ''deleted''} via the registered rr_content_digest_fold scalar, seed the state from a from-scratch Rust fold, and re-stamp every freshness stamp (index_meta fts_source_revision/content_revision, clone_graph_generations.source_revision, the clone-graph quiet candidate) that equals the frozen legacy digest so no one-time FTS/clone rebuild fires. Replaces the O(N) main.files scan with an O(1) state read');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('087_table_sync_bookkeeping',1786958291462,'sha256:rag-rat-table-sync-bookkeeping-v87','Add the table→log sync engine''s bookkeeping tables: sync_published_rows (post-apply synced-column hash that stops a remotely-applied row being re-signed and rebroadcast — the anti-echo record), sync_row_clocks (the per-row whole-row last-writer-wins clock an upsert or delete must beat to win the row), sync_row_tombstones (a per-row deletion clock so an out-of-order stale delete cannot win and an even older insert cannot resurrect), and table_sync_entries (the engine''s own signed hash-chained entry log, separate from oplog_entries so the memory-content re-fold never sees a table op). All STRICT; no authored content — pure sync bookkeeping the fold and producer read');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('088_clone_postings_row_count',1786958291463,'sha256:rag-rat-clone-postings-row-count-v88','Cache each clone generation''s posting-row count on the generation row (#830): add clone_graph_generations.postings_row_count and backfill it from COUNT(*) of clone_subblock_postings per generation. The #598 delta work budget sizes off this count; reading a maintained column replaces a full COUNT(*) scan of the postings table on every delta pass. Additive; existing rows backfill from the current postings, and the count is then maintained transactionally at build (complete_generation) and in each delta write-back');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('089_sync_invites',1786958291464,'sha256:rag-rat-sync-invites-bootstrap-replay-v89','Add the durable one-time enrollment invite store: a random nonce binds one account, granted device role, optional label, and expiry; successful redemption stores the exact request identity, signed DeviceAdd, and exact account-log bootstrap receipt in the same transaction as invite consumption and key catch-up, so delivery failures can replay the acknowledged enrollment idempotently and a fresh joiner can authorize its first closed sync');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('090_account_candidate_reservations',1786958291464,'sha256:rag-rat-account-candidate-reservations-v90','Add durable candidate-capacity reservations for outstanding enrollment invites (#949): a minted invite reserves the exact entries/bytes its mandatory DeviceAdd plus stream-key wraps will consume, and candidate admission charges active reservations against the same grow-only counters, so ordinary ingest or a second mint cannot strand an already-minted ticket. Redemption releases its reservation under the writer lock; expiry frees it');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('091_account_candidate_reservation_targets',1786958291465,'sha256:rag-rat-account-candidate-reservation-targets-v91','Track the live key-target count each outstanding invite reservation covers (account_candidate_reservations.reserved_targets, #949): any fold that grows the target set — local key mints or remotely synced StreamOwn/wrap entries — tops reservations up to the current mandatory redemption cost, so a minted ticket cannot be stranded by later growth. Backfilled from reserved_entries - 1, exact for every V090-era row');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('092_sync_invites_normalized_receipts',1786958291471,'sha256:rag-rat-sync-invites-normalized-receipts-v92','Drop sync_invites.receipt_bytes (#949): consumed invites keep only the joiner-specific DeviceAdd envelope; the account bootstrap is already durable in the grow-only candidate DAG, and receipt replay reconstructs the snapshot from it instead of storing one full copy per invite (quadratic growth across a fleet). Table rebuild preserving every row');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('093_table_sync_projection_state',1786958291477,'sha256:rag-rat-table-sync-projection-state-v93','Table-sync forward-compat projection substrate (#1001): mark entries this binary cannot fully project (pending_reason / pending_projector_version) so a later binary replays them instead of losing their payload; a table_sync_streams directory recovering the (repo_id, account_id, scope_id) apply context that the one-way stream id hashes away, without which a stored entry cannot be replayed at all; and sync_published_rows.projector_version, since the anti-echo hash covers the hashing binary''s column set and is meaningless without that set''s identity');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('094_lens_enrichment_revision',1786958291479,'sha256:rag-rat-lens-enrichment-revision-v94-transactional-history-and-oracle','Add SQLite triggers that increment a per-repo repo_meta revision when Lens-visible memories, dream state, papertrail records, clone refinements, Oracle runs, or the live clone graph change. Bulk writers whose transaction touches one row per indexed edge or commit — git-history imports and Oracle verdict passes — increment the same clock once at their transaction boundary instead of once per row. The Lens SSE freshness probe reads only O(1) indexed rows instead of rescanning enrichment and files tables every polling interval');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('095_table_sync_spec_version',1786958291485,'sha256:rag-rat-table-sync-spec-version-v95','Per-table spec versioning for table-sync (#1002): sync_published_rows records the TABLE''s spec_version rather than the store-global projector version, so an unrelated projector bump no longer marks every table''s rows incomparable. The table is necessarily empty (no table is registered), so it is rebuilt into its final shape rather than carrying a dead column');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('096_table_sync_gapped_entries',1786958291485,'sha256:rag-rat-table-sync-gapped-entries-v96','Retention for table-sync entries whose chain predecessor has not arrived (#1058): table_sync_gapped_entries holds a verified entry that links to an unheld predecessor until that predecessor is accepted, at which point it is promoted through the ordinary accept and apply path. Previously such an entry was dropped, so a chain delivered out of causal order could only converge through redelivery in exact order. Deliberately its own table rather than a status column: six queries read table_sync_entries as the accepted chain — the authoring Lamport clock, the lamport-advance bound, the chain tail, entry existence, the LWW winner lookup, and the refold''s pending set — and every one of them must keep excluding an entry that is not on a chain');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('097_windows_verbatim_path_rekey',1786958291486,'sha256:rag-rat-windows-verbatim-path-rekey-v97','Rekey the persisted Windows path spellings an older binary wrote in the \\?\ verbatim form (#1048): every worktree_id scope key, repo_roots.root, the path-valued meta keys (source_root and the git_history_indexed_root reload cursor), and the worktree_overlay_basis keys whose suffix is a worktree_id. Production now canonicalizes to the plain spelling, and these values are compared textually against it — left stale, the overlay and dirty rows fall out of the active scope and GC prunes them as a dead checkout, and the git-history gate forces a full revwalk plus a blame-cache wipe. Rewriting uses the same rule canonicalization does, so a verbatim path that is still load-bearing (UNC, >MAX_PATH, reserved DOS names) is kept. Runs on every host: which spellings a store carries is a property of the store, not of the binary that opens it');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('098_reindex_after_unix_backslash_rendering',1786958291486,'sha256:rag-rat-reindex-after-unix-backslash-rendering-v98','Force the next ordinary index pass to re-walk the tree and reload git history, so a store an older binary wrote before the Unix backslash-rendering fix (#1032) re-derives its path-keyed rows off the corrected spelling. That binary collapsed a literal backslash in a Unix filename to a separator, so files.path and the symbol identities keyed on it could not be told apart from a genuinely nested sibling; the old rendering was lossy, so the stored spelling cannot be repaired in place — only a re-walk recovers the truth. This deletes the freshness markers that gate that work: the base-scope discovery marker (files re-walk, which cascades chunks, symbols, and edges), the git-history root cursor (a full revwalk, which re-derives the commit and file-change rows and the change couplings folded off its freshness key), and the worktree_overlay_basis keys (per-checkout overlay re-derive); it also clears parser_failures, the one path-keyed derived table a file re-walk does not cascade. Runs on every host: which spellings a store carries is a property of the store, not of the binary that opens it');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('099_table_sync_repo_incarnations',1786958291486,'sha256:rag-rat-table-sync-repo-incarnations-v99','Add the owner-authorized repository-incarnation projection and /5 table-sync substrate: incarnation-bound stream contexts, stream-isolated row clocks/publication/tombstones, and retained per-device chain-tip witnesses that survive local repository purge. Pre-transport /4 table-sync state is cleared because it has no account-authorized incarnation identity');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('100_receiver_type_hint_interning',1786958291487,'sha256:rag-rat-receiver-type-hint-and-callee-aware-edge-identity-v100','Add edges_data.receiver_type_hint_id for conservative Rust receiver-type resolution and persist stable callee identity for call-path validation');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('101_file_graph_version_provenance',1786958291491,'sha256:rag-rat-file-graph-version-provenance-v101','Add per-file graph and scope derivation provenance so an active checkout can refresh only rows whose bytes it can verify, while linked worktrees retain and later complete their own upgrade');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('102_lens_lane_revisions',1786958291494,'sha256:rag-rat-lens-lane-revisions-v102','Add independent O(1), per-repo Lens revision clocks for symbols, clones, memories, coupling, and papertrail so editor clients refetch only lanes whose backing data changed while the aggregate legacy clock remains intact');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('103_syncable_memory_bindings',1786958291504,'sha256:rag-rat-syncable-memory-bindings-v103','Rebuild memory bindings as a strict, repository-keyed, dependency-free table suitable for deterministic anchors/1 whole-row replication');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('104_table_sync_readoption',1786958291504,'sha256:rag-rat-table-sync-readoption-v104','Add the durable table-sync re-adoption worklist and audit log (#997): an effective DeviceRemove enqueues one item per affected stream, which a current writer drains by re-authoring the removed writer''s surviving LWW state under its own chain');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('105_table_sync_retained_floors',1786958291504,'sha256:rag-rat-table-sync-retained-floors-v105','Add table_sync_retained_floors (#1127): the per-(stream, device) chain prefix floor an accepted-entry compaction has reclaimed below, so peers and the accept path can tell an intentionally pruned prefix from a chain gap');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('106_readoption_audit_nullable_winner',1786958291515,'sha256:rag-rat-readoption-audit-nullable-winner-v106','Make table_sync_readoption_audit.original_entry_hash nullable (#1127): a winner reclaimed by accepted-entry compaction before re-adoption ran has no hash to record; the slot stays named by (stream, device, lamport)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('107_syncable_overlay_tables',1786958291516,'sha256:rag-rat-syncable-overlay-tables-v107','Drop the Lens revision triggers on memory_reality and memory_summaries (#1133): the overlay/1 table-sync scope applies these rows under whole-row LWW, and a trigger firing on a wire-applied row is a device-local side effect; the dream write and the sync apply advance the Lens lanes explicitly instead');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('108_syncable_distill_records',1786958291523,'sha256:rag-rat-syncable-distill-records-v108','Rebuild papertrail_distill onto the thread natural key (repo_id first), dropping the device-local AUTOINCREMENT id, so the distill/1 table-sync scope can replicate distilled records under whole-row LWW (#1135); also drops its Lens revision triggers (the sync apply advances the papertrail lane explicitly)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('109_syncable_distill_edges_and_alternatives',1786958291538,'sha256:rag-rat-syncable-distill-edges-and-alternatives-v109','Rebuild papertrail_distill_edges and papertrail_distill_alternatives onto their thread natural keys (repo_id first), dropping the device-local AUTOINCREMENT id, so these distill enrichment children replicate on the distill/1 table-sync scope under whole-row LWW (#1137)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('110_syncable_distill_record_commits',1786958291545,'sha256:rag-rat-syncable-distill-record-commits-v110','Rebuild papertrail_distill_record_commits onto its natural key (repo_id first), dropping the device-local AUTOINCREMENT id and adding a created_at_ms non-key column so the key-only table can replicate on the distill/1 table-sync scope under whole-row LWW (#1139)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('111_syncable_distill_evidence',1786958291553,'sha256:rag-rat-syncable-distill-evidence-v111','Rebuild papertrail_distill_evidence onto its natural key (repo_id first) with a per-thread ordinal, dropping the device-local AUTOINCREMENT id, so the distill evidence child replicates on the distill/1 table-sync scope under whole-row LWW (#1139)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('112_syncable_distill_anchors',1786958291561,'sha256:rag-rat-syncable-distill-anchors-v112','Rebuild papertrail_distill_anchors onto its natural key (repo_id first), dropping the device-local AUTOINCREMENT id and its Lens revision triggers, so the distill anchors child replicates on the distill/1 table-sync scope under whole-row LWW; logical_symbol_id and resolved stay checkout-local and never replicate (#1139)');
INSERT INTO "schema_version"("id","applied_at_ms","checksum","description") VALUES('113_refold_content_streams_for_lamport_clamp',1786958291561,'sha256:rag-rat-refold-content-streams-for-lamport-clamp-v113','Queue every /3 content stream for an acceptance refold so entries accepted before the lamport clamp existed are re-judged under it; a stale accepted near-ceiling lamport would otherwise keep dominating LWW and blocking authoring on an upgraded store while a fresh replica parks the same entry and diverges (#1176)');
INSERT INTO "repos"("repo_id","display_name","registered_at_ms") VALUES('__unassigned__','',0);
INSERT INTO "content_digest_state"("id","state","rows_folded") VALUES(1,'0000000000000000000000000000000000000000000000000000000000000000',0);
