use super::*;

mod bootstrap;
mod distill;
mod memory_features;

#[test]
fn migration_052_adds_oplog_storage_tables() {
    const OPLOG_TABLES: [&str; 4] =
        ["oplog_entries", "oplog_projected_nodes", "oplog_projected_edges", "oplog_meta"];
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V053 now holds the absolute tip pin (migration_053's test); this drops to the symbolic
    // `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    for table in OPLOG_TABLES {
        assert!(conn_table_exists(&conn, table), "V052 creates {table}");
    }

    // Deferred-absence in ISOLATION: drop the tables and re-run the applier alone (never against
    // the full ladder's prior state). It recreates them, and a second run is a no-op (CREATE …
    // IF NOT EXISTS), matching the replay-write-free discipline.
    conn.execute_batch(
        "DROP TABLE oplog_entries;
         DROP TABLE oplog_projected_nodes;
         DROP TABLE oplog_projected_edges;
         DROP TABLE oplog_meta;",
    )
    .unwrap();
    assert!(!conn_table_exists(&conn, "oplog_entries"), "dropped before the isolated apply");
    schema::apply_oplog_storage(&conn).unwrap();
    schema::apply_oplog_storage(&conn).expect("replay is a no-op");
    for table in OPLOG_TABLES {
        assert!(conn_table_exists(&conn, table), "the isolated applier recreates {table}");
    }
}

#[test]
fn migration_053_scopes_the_oplog_by_stream_and_adds_fork_evidence() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V054 now holds the absolute tip pin (migration_054's test); this drops to the symbolic
    // `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The rebuilt tables carry the stream dimension; the quarantine table exists.
    for (table, column) in [
        ("oplog_entries", "stream_id"),
        ("oplog_projected_nodes", "stream_id"),
        ("oplog_projected_edges", "stream_id"),
        ("oplog_fork_evidence", "conflicting_entry_hash"),
    ] {
        assert!(
            conn_table_columns(&conn, table).contains(&column.to_string()),
            "V053 gives {table} its {column} column"
        );
    }

    // One chain slot per (stream, device, lamport): the same (device, lamport) is legal on two
    // DIFFERENT streams, and an equivocation within one stream trips the UNIQUE tripwire.
    conn.execute_batch(
        "INSERT INTO oplog_entries VALUES (x'01', x'aa', x'dd', 1, NULL, x'00', 0);
         INSERT INTO oplog_entries VALUES (x'02', x'bb', x'dd', 1, NULL, x'00', 0);",
    )
    .expect("the same (device, lamport) slot on two streams is two distinct chains");
    assert!(
        conn.execute(
            "INSERT INTO oplog_entries VALUES (x'03', x'aa', x'dd', 1, NULL, x'00', 0)",
            [],
        )
        .is_err(),
        "UNIQUE(stream_id, device_fingerprint, lamport) rejects a same-stream slot collision"
    );

    // Deferred-absence in ISOLATION: reduce the tables to the V052 shape (no stream_id, no
    // quarantine) and re-run the applier alone — it rebuilds the stream-scoped shape, and a
    // replay reconverges (the rebuild is safe precisely because the log is un-wired and empty;
    // see the applier's invariant comment).
    conn.execute_batch(
        "DROP TABLE oplog_entries;
         DROP TABLE oplog_projected_nodes;
         DROP TABLE oplog_projected_edges;
         DROP TABLE oplog_fork_evidence;
         CREATE TABLE oplog_entries(entry_hash BLOB PRIMARY KEY) STRICT;",
    )
    .unwrap();
    schema::apply_oplog_stream_scoping(&conn).unwrap();
    schema::apply_oplog_stream_scoping(&conn).expect("replay reconverges");
    for table in
        ["oplog_entries", "oplog_projected_nodes", "oplog_projected_edges", "oplog_fork_evidence"]
    {
        assert!(
            conn_table_columns(&conn, table).contains(&"stream_id".to_string()),
            "the isolated applier rebuilds {table} stream-scoped"
        );
    }
    assert!(conn_table_exists(&conn, "oplog_meta"), "oplog_meta is left untouched");
}

#[test]
fn migration_054_adds_the_single_row_device_identity_table() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The absolute-tip pin moved to `migration_055_*` (V055 is the tip now); this drops to the
    // symbolic check, per the ladder convention.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The identity table exists with its full column set.
    for column in ["seed", "public_key", "fingerprint", "created_at_ms"] {
        assert!(
            conn_table_columns(&conn, "oplog_device_identity").contains(&column.to_string()),
            "V054 gives oplog_device_identity its {column} column"
        );
    }

    // `CHECK (id = 0)` + the primary key make it a strict single-row table: id 0 inserts once; a
    // non-zero id is refused by the CHECK; a second id-0 insert is refused by the PK. (Columns are
    // named because V058 added the nullable x25519 columns — a positional 5-value insert no longer
    // matches the 7-column table.)
    conn.execute(
        "INSERT INTO oplog_device_identity(id, seed, public_key, fingerprint, created_at_ms)
         VALUES (0, x'00', x'11', x'22', 0)",
        [],
    )
    .expect("the sole id=0 identity row inserts");
    assert!(
        conn.execute(
            "INSERT INTO oplog_device_identity(id, seed, public_key, fingerprint, created_at_ms)
             VALUES (1, x'00', x'11', x'22', 0)",
            [],
        )
        .is_err(),
        "CHECK (id = 0) rejects a second, non-zero identity"
    );
    assert!(
        conn.execute(
            "INSERT INTO oplog_device_identity(id, seed, public_key, fingerprint, created_at_ms)
             VALUES (0, x'99', x'88', x'77', 1)",
            [],
        )
        .is_err(),
        "the id=0 primary key rejects a second identity"
    );

    // Deferred-absence in ISOLATION: drop the table and re-run the applier alone (never against the
    // full ladder's end state). It recreates the table, and a replay is a no-op (CREATE … IF NOT
    // EXISTS).
    conn.execute_batch("DROP TABLE oplog_device_identity;").unwrap();
    assert!(
        !conn_table_exists(&conn, "oplog_device_identity"),
        "dropped before the isolated apply"
    );
    schema::apply_oplog_device_identity(&conn).unwrap();
    schema::apply_oplog_device_identity(&conn).expect("replay is a no-op");
    assert!(
        conn_table_columns(&conn, "oplog_device_identity").contains(&"seed".to_string()),
        "the isolated applier recreates the table"
    );
}

#[test]
fn migration_055_adds_the_binding_downgrade_marker_column() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The absolute tip pin lives with the newest migration's test (`migration_057_*` now); this
    // drops to the symbolic `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    assert!(
        conn_table_columns(&conn, "repo_memory_bindings")
            .contains(&"downgrade_pending_at_ms".to_string()),
        "repo_memory_bindings carries the downgrade hysteresis marker (#492)"
    );
    // Additive + nullable: a re-apply is idempotent and the column survives.
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert!(
        conn_table_columns(&conn, "repo_memory_bindings")
            .contains(&"downgrade_pending_at_ms".to_string()),
        "downgrade_pending_at_ms survives a re-apply"
    );
    // A forward migrate over a ledger truncated below V055 replays the step and lands the
    // column (the standard lagging-index path).
    truncate_schema_to(&conn, 54);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
}

#[test]
fn migration_056_adds_the_git_change_couplings_table() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The absolute tip pin lives with the newest migration's test (`migration_057_*` now); this
    // drops to the symbolic `current_version == LATEST` freshness check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    assert!(
        conn_table_exists(&conn, "git_change_couplings"),
        "V056 creates the git_change_couplings table"
    );

    // STRICT + composite (repo_id, path_a, path_b) PK: a duplicate pair is rejected.
    conn.execute(
        "INSERT INTO git_change_couplings(repo_id, path_a, path_b, co_change_count, \
         path_a_change_count, path_b_change_count, window_commit_count, last_co_change_at_s, \
         computed_at_ms) VALUES ('r', 'a.rs', 'b.rs', 2, 3, 4, 10, 100, 1)",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO git_change_couplings(repo_id, path_a, path_b, co_change_count, \
             path_a_change_count, path_b_change_count, window_commit_count, last_co_change_at_s, \
             computed_at_ms) VALUES ('r', 'a.rs', 'b.rs', 9, 9, 9, 9, 9, 9)",
            [],
        )
        .is_err(),
        "the composite PK rejects a duplicate (repo_id, path_a, path_b)"
    );

    // Deferred-absence in ISOLATION: drop + re-run the applier alone; it recreates, replay is a
    // no-op.
    conn.execute_batch("DROP TABLE git_change_couplings;").unwrap();
    assert!(!conn_table_exists(&conn, "git_change_couplings"), "dropped before the isolated apply");
    schema::apply_git_change_couplings(&conn).unwrap();
    schema::apply_git_change_couplings(&conn).expect("replay is a no-op");
    assert!(
        conn_table_exists(&conn, "git_change_couplings"),
        "the isolated applier recreates the table"
    );

    // A forward migrate over a ledger truncated below V056 replays the step and reaches the tip.
    truncate_schema_to(&conn, 55);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
}

#[test]
fn migration_057_adds_the_external_symbols_table() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The absolute tip pin moved to `migration_058_*` (V058 is the tip now); this drops to the
    // symbolic `current_version == LATEST` check, per the ladder convention.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The external-symbol contract table exists with its full column set.
    for column in [
        "repo_id",
        "tool",
        "tool_version",
        "commit_sha",
        "worktree_id",
        "moniker",
        "kind",
        "display_name",
        "signature_text",
        "signature_language",
        "documentation",
        "deprecated",
        "computed_at_ms",
    ] {
        assert!(
            conn_table_columns(&conn, "external_symbols").contains(&column.to_string()),
            "V057 gives external_symbols its {column} column"
        );
    }

    // PK `(repo_id, tool, commit_sha, worktree_id, moniker)`: a second row with the same key is
    // rejected even when the payload differs; the SAME moniker under a DIFFERENT checkout inserts
    // (the multi-worktree isolation), as does a distinct moniker.
    let insert = "INSERT INTO external_symbols(repo_id, tool, tool_version, commit_sha, \
                  worktree_id, moniker, kind, display_name, signature_text, signature_language, \
                  documentation, deprecated, computed_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, \
                  ?8, ?9, ?10, ?11, ?12, ?13)";
    conn.execute(insert, rusqlite::params![
        "r",
        "rust-analyzer",
        "1.0",
        "sha1",
        "",
        "crate a 1.0 mod/get().",
        "Function",
        "get",
        "fn get()",
        "rust",
        "docs",
        0,
        123_i64
    ])
    .expect("first external-symbol row inserts");
    assert!(
        conn.execute(insert, rusqlite::params![
            "r",
            "rust-analyzer",
            "2.0",
            "sha1",
            "",
            "crate a 1.0 mod/get().",
            "Method",
            "get",
            "fn get(x)",
            "rust",
            "other",
            1,
            456_i64
        ])
        .is_err(),
        "the (repo_id, tool, commit_sha, worktree_id, moniker) primary key rejects a duplicate"
    );
    conn.execute(insert, rusqlite::params![
        "r",
        "rust-analyzer",
        "1.0",
        "sha2",
        "",
        "crate a 1.0 mod/get().",
        "Function",
        "get",
        "fn get()",
        "rust",
        "docs",
        0,
        123_i64
    ])
    .expect("the same moniker under a different checkout (commit_sha) inserts — worktree-isolated");
    conn.execute(insert, rusqlite::params![
        "r",
        "rust-analyzer",
        "1.0",
        "sha1",
        "",
        "crate a 1.0 mod/other().",
        "Function",
        "other",
        "fn other()",
        "rust",
        "docs",
        0,
        123_i64
    ])
    .expect("a distinct moniker inserts");

    // Deferred-absence in ISOLATION: drop the table and re-run the applier alone (never against the
    // full ladder's end state). It recreates the table, and a replay is a no-op (CREATE … IF NOT
    // EXISTS).
    conn.execute_batch("DROP TABLE external_symbols;").unwrap();
    assert!(!conn_table_exists(&conn, "external_symbols"), "dropped before the isolated apply");
    schema::apply_external_symbols(&conn).unwrap();
    schema::apply_external_symbols(&conn).expect("replay is a no-op");
    assert!(
        conn_table_columns(&conn, "external_symbols").contains(&"moniker".to_string()),
        "the isolated applier recreates the table"
    );

    // A forward migrate over a ledger truncated below V057 replays the step and lands the table.
    conn.execute_batch("DROP TABLE external_symbols;").unwrap();
    truncate_schema_to(&conn, 56);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
    assert!(
        conn_table_exists(&conn, "external_symbols"),
        "the forward migrate re-creates external_symbols"
    );
}

#[test]
fn migration_058_adds_the_oplog_device_x25519_columns() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V058 is no longer the tip — symbolic tip check.
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The identity table gains the X25519 encryption columns (sync phase C, §5).
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            conn_table_columns(&conn, "oplog_device_identity").contains(&column.to_string()),
            "V058 gives oplog_device_identity its {column} column"
        );
    }

    // Deferred-absence in ISOLATION: build the table from the V054 DDL ALONE (never the full
    // ladder's end state) — the x25519 columns are absent — then the V058 applier adds them, and a
    // replay is an idempotent no-op (add_column_if_missing).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_oplog_device_identity(&isolated).unwrap();
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            !conn_table_columns(&isolated, "oplog_device_identity").contains(&column.to_string()),
            "the V054 table alone lacks the {column} column"
        );
    }
    schema::apply_oplog_device_x25519(&isolated).unwrap();
    schema::apply_oplog_device_x25519(&isolated).expect("replay is a no-op");
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            conn_table_columns(&isolated, "oplog_device_identity").contains(&column.to_string()),
            "the isolated V058 applier adds the {column} column"
        );
    }

    // A forward migrate over a ledger truncated below V058 replays the step and keeps the columns.
    truncate_schema_to(&conn, 57);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            conn_table_columns(&conn, "oplog_device_identity").contains(&column.to_string()),
            "the forward migrate keeps the {column} column"
        );
    }
}

#[test]
fn migration_059_creates_the_account_candidate_dag() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // V059 is no longer the tip (the absolute pin moved to the V060 papertrail test).
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );

    // The candidate-DAG tables + indexes exist (sync phase C, §16.1).
    for table in ["account_entries", "account_entry_status", "account_pre_verify"] {
        assert!(conn_table_exists(&conn, table), "V059 creates {table}");
    }
    assert!(conn_index_exists(&conn, "account_entries_chain"), "V059 creates the chain index");
    assert!(
        conn_index_exists(&conn, "account_accepted_slot"),
        "V059 creates the accepted-slot partial unique index (I10a)"
    );
    assert!(
        conn_index_exists(&conn, "account_pre_verify_account"),
        "V059 creates the pre-verify claimed_account_id index"
    );
    let pre_verify_columns = conn_table_columns(&conn, "account_pre_verify");
    assert!(pre_verify_columns.contains(&"signed_hash".to_string()));
    assert!(pre_verify_columns.contains(&"entry_hash".to_string()));

    // Deferred-absence in ISOLATION: a bare DB lacks the tables until the V059 applier runs, and a
    // replay is an idempotent no-op (every statement is CREATE ... IF NOT EXISTS).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    assert!(!conn_table_exists(&isolated, "account_entries"), "bare DB lacks account_entries");
    schema::apply_account_candidate_dag(&isolated).unwrap();
    schema::apply_account_candidate_dag(&isolated).expect("replay is a no-op");
    for table in ["account_entries", "account_entry_status", "account_pre_verify"] {
        assert!(conn_table_exists(&isolated, table), "the isolated V059 applier creates {table}");
    }
    assert!(conn_index_exists(&isolated, "account_accepted_slot"), "and the partial unique index");

    // A forward migrate over a ledger truncated below V059 replays the step and keeps the tables.
    truncate_schema_to(&conn, 58);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip"
    );
    assert!(
        conn_table_exists(&conn, "account_entries"),
        "the forward migrate keeps account_entries"
    );
}

#[test]
fn migration_064_creates_account_authority_shadow_tables() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    for table in [
        "account_auth_state",
        "account_roster_history",
        "account_owner_incarnations",
        "account_stream_ownership",
        "account_stream_grants",
        "account_stream_grant_cuts",
    ] {
        assert!(conn_table_exists(&conn, table), "V064 creates {table}");
    }
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_account_authority_projection(&isolated).unwrap();
    schema::apply_account_authority_projection(&isolated).expect("V064 replay is idempotent");
    assert!(conn_table_exists(&isolated, "account_auth_state"));
    let seq_type: String = isolated
        .query_row(
            "SELECT type FROM pragma_table_info('account_stream_grant_cuts') WHERE name = 'seq'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seq_type, "BLOB", "device cuts retain the full unsigned u64 domain");
    assert!(
        isolated
            .execute(
                "INSERT INTO account_stream_grant_cuts(
                     grant_id, owner_account_id, device_fingerprint, seq, entry_hash
                 ) VALUES (?1, ?1, ?1, ?2, ?1)",
                rusqlite::params![[0u8; 32].as_slice(), [0u8; 7].as_slice()],
            )
            .is_err(),
        "the fixed-width cut coordinate rejects corrupt stored values",
    );

    truncate_schema_to(&conn, 63);
    conn.execute_batch(
        "DROP TABLE account_stream_grant_cuts;
         DROP TABLE account_stream_grants;
         DROP TABLE account_stream_ownership;
         DROP TABLE account_owner_incarnations;
         DROP TABLE account_roster_history;
         DROP TABLE account_auth_state;",
    )
    .unwrap();
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    assert!(conn_table_exists(&conn, "account_auth_state"));
}

#[test]
fn migration_065_adds_historical_authority_boundaries() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert!(conn_table_exists(&conn, "account_roster_content_boundaries"));
    for table in ["account_roster_history", "account_owner_incarnations"] {
        for column in [
            "control_boundary",
            "control_seq",
            "control_hash",
            "secrets_boundary",
            "secrets_seq",
            "secrets_hash",
        ] {
            assert!(
                conn_table_columns(&conn, table).contains(&column.to_string()),
                "V065 adds {table}.{column}"
            );
        }
    }
    schema::apply_account_authority_boundaries(&conn).expect("V065 replay is idempotent");
}

#[test]
fn migration_066_adds_the_content_candidate_dag() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    for table in ["content_entries", "content_entry_status", "content_pre_verify"] {
        assert!(conn_table_exists(&conn, table), "V066 creates {table}");
    }
    let columns = conn_table_columns(&conn, "content_entries");
    for column in [
        "stream_id",
        "author_account_id",
        "device_fingerprint",
        "seq",
        "prev_hash",
        "grant_id",
        "roster_ref",
        "owner_auth_len",
        "author_auth_len",
        "accepted",
        "signed_bytes",
    ] {
        assert!(columns.contains(&column.to_string()), "V066 adds content_entries.{column}");
    }
    schema::apply_content_candidate_dag(&conn).expect("V066 replay is idempotent");
    let insert = |hash: u8, seq_width: usize| {
        conn.execute(
            "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, 0, ?9, 0)",
            rusqlite::params![
                vec![hash; 32],
                vec![1_u8; 32],
                vec![2_u8; 32],
                vec![3_u8; 32],
                vec![0_u8; seq_width],
                vec![4_u8; 32],
                vec![0_u8; 8],
                vec![0_u8; 8],
                vec![hash],
            ],
        )
    };
    assert!(insert(1, 7).is_err(), "V066 rejects truncated unsigned counters");
    insert(1, 8).unwrap();
    insert(2, 8).expect("equivocating candidates are first-class while unaccepted");
    conn.execute("UPDATE content_entries SET accepted = 1 WHERE entry_hash = ?1", [vec![1; 32]])
        .unwrap();
    assert!(
        conn.execute("UPDATE content_entries SET accepted = 1 WHERE entry_hash = ?1", [vec![
            2;
            32
        ]])
        .is_err(),
        "V066 permits at most one accepted candidate per dense slot"
    );
    truncate_schema_to(&conn, 65);
    conn.execute_batch(
        "DROP TABLE content_pre_verify;
         DROP TABLE content_entry_status;
         DROP TABLE content_entries;",
    )
    .unwrap();
    assert!(!conn_table_exists(&conn, "content_entries"));
    schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("V065 upgrades through V066");
    assert!(conn_index_exists(&conn, "content_entries_chain"));
    assert!(conn_index_exists(&conn, "content_entries_predecessor"));
    assert!(conn_index_exists(&conn, "content_accepted_slot"));
    assert!(conn_index_exists(&conn, "content_pre_verify_author"));
    schema::migrate_forward(&conn, &crate::index::migration_hooks())
        .expect("a second V066 forward migration is a no-op");
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
}

#[test]
fn migration_068_hides_suppressed_edge_candidates() {
    // The absolute-tip pin moved to `migration_069_*` (V069 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES ('App.swift', 'swift', 'source', 'sha', 0, 0, 'head', '')",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution, evidence) \
         VALUES (?1, 'Observable', 'uses_macro', 'NameOnly', 'suppressed', '@Observable')",
        [file_id],
    )
    .unwrap();
    truncate_schema_to(&conn, 67);
    let current_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // The current view filters on the materialized `hidden` flag (V075); the pre-V068 view had
    // only the dispatch-fact exclusion, evaluated inline. Reconstruct that shape so the row (a
    // suppressed `uses_macro` candidate) is visible before the upgrade.
    let v67_view = current_view.replace(
        "WHERE d.hidden = 0",
        "WHERE d.edge_kind_id NOT IN (
            SELECT id FROM name_strings WHERE value IN ('dispatch_construct', 'dispatch_handle')
        )",
    );
    assert_ne!(v67_view, current_view, "fixture must remove the V068 public-edge filter");
    conn.execute_batch("DROP VIEW edges;").unwrap();
    conn.execute_batch(&v67_view).unwrap();
    let visible_before_upgrade: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0)).unwrap();
    assert_eq!(visible_before_upgrade, 1, "the V067 view exposes the retained candidate");

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    let raw_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges_data", [], |row| row.get(0)).unwrap();
    let visible_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0)).unwrap();
    assert_eq!(raw_count, 1, "suppressed candidates remain available to the resolver");
    assert_eq!(visible_count, 0, "suppressed candidates stay out of query-layer reads");
    let v68_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '068_suppressed_edge_candidates'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v68_recorded, 1, "the forward migration records V068");
}

#[test]
fn migration_069_adds_the_local_account_pointer() {
    // The absolute-tip pin moved to `migration_070_*` (V070 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // The single-row pointer table exists with its full column set.
    for column in ["id", "genesis_entry_hash", "created_at_ms"] {
        assert!(
            conn_table_columns(&conn, "oplog_local_account").contains(&column.to_string()),
            "V069 gives oplog_local_account its {column} column",
        );
    }

    // `CHECK (id = 0)` + the primary key make it a strict single-row table: id 0 inserts once; a
    // non-zero id is refused by the CHECK; a second id-0 insert is refused by the PK. The
    // `length(genesis_entry_hash) = 32` CHECK rejects a wrong-width pointer.
    conn.execute(
        "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
         VALUES (0, zeroblob(32), 0)",
        [],
    )
    .expect("the sole id=0 pointer row inserts");
    assert!(
        conn.execute(
            "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
             VALUES (1, zeroblob(32), 0)",
            [],
        )
        .is_err(),
        "CHECK (id = 0) rejects a second, non-zero pointer",
    );
    assert!(
        conn.execute(
            "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
             VALUES (0, zeroblob(32), 1)",
            [],
        )
        .is_err(),
        "the id=0 primary key rejects a second pointer",
    );
    assert!(
        conn.execute(
            "UPDATE oplog_local_account SET genesis_entry_hash = zeroblob(31) WHERE id = 0",
            [],
        )
        .is_err(),
        "the length(genesis_entry_hash) = 32 CHECK rejects a wrong-width hash",
    );

    // Deferred-absence in ISOLATION: a bare DB lacks the table until the V069 applier runs, and a
    // replay is an idempotent no-op (CREATE ... IF NOT EXISTS).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    assert!(
        !conn_table_exists(&isolated, "oplog_local_account"),
        "bare DB lacks oplog_local_account before the isolated apply",
    );
    schema::apply_oplog_local_account(&isolated).unwrap();
    schema::apply_oplog_local_account(&isolated).expect("replay is a no-op");
    assert!(
        conn_table_columns(&isolated, "oplog_local_account")
            .contains(&"genesis_entry_hash".to_string()),
        "the isolated applier recreates the table",
    );

    // A forward migrate over a ledger truncated below V069 replays the step and records V069.
    truncate_schema_to(&conn, 68);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip",
    );
    let v69_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '069_oplog_local_account'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v69_recorded, 1, "the forward migration records V069");
}

#[test]
fn migration_070_adds_the_content_projected_tables() {
    // The absolute-tip pin moved to `migration_071_*` (V071 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // Both /3 projection tables exist with their full column set, mirroring the stream-keyed /1
    // shadow tables (V053).
    for column in ["stream_id", "node_id", "content_json", "status"] {
        assert!(
            conn_table_columns(&conn, "content_projected_nodes").contains(&column.to_string()),
            "V070 gives content_projected_nodes its {column} column",
        );
    }
    for column in ["stream_id", "edge_key", "spec_json", "resolved_json"] {
        assert!(
            conn_table_columns(&conn, "content_projected_edges").contains(&column.to_string()),
            "V070 gives content_projected_edges its {column} column",
        );
    }

    // The primary key is the composite (stream_id, node_id) / (stream_id, edge_key): the SAME
    // (node_id / edge_key) may recur under a DIFFERENT stream, but a duplicate under one stream is
    // refused — the stream-keying that keeps two /2 streams' projections from colliding.
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
         VALUES (zeroblob(32), 'n1', '{}', 'active')",
        [],
    )
    .expect("first node row inserts");
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
         VALUES (randomblob(32), 'n1', '{}', 'active')",
        [],
    )
    .expect("the same node_id under a different stream is a distinct row");
    assert!(
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (zeroblob(32), 'n1', '{}', 'active')",
            [],
        )
        .is_err(),
        "the (stream_id, node_id) primary key rejects a duplicate within one stream",
    );

    // Deferred-absence in ISOLATION: a bare DB lacks the tables until the V070 applier runs, and a
    // replay is an idempotent no-op (CREATE ... IF NOT EXISTS).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    assert!(
        !conn_table_exists(&isolated, "content_projected_nodes"),
        "bare DB lacks content_projected_nodes before the isolated apply",
    );
    schema::apply_content_projected_tables(&isolated).unwrap();
    schema::apply_content_projected_tables(&isolated).expect("replay is a no-op");
    assert!(
        conn_table_columns(&isolated, "content_projected_edges").contains(&"spec_json".to_string()),
        "the isolated applier recreates the tables",
    );

    // A forward migrate over a ledger truncated below V070 replays the step and records V070.
    truncate_schema_to(&conn, 69);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip",
    );
    let v70_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '070_content_projected_tables'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v70_recorded, 1, "the forward migration records V070");
}

#[test]
fn migration_071_indexes_edge_target_qname() {
    // The absolute-tip pin moved to `migration_072_*` (V072 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    let index_on = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_edges_target_qname' AND tbl_name = 'edges_data'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(index_on(&conn), 1, "V071 creates idx_edges_target_qname on edges_data");

    // Idempotent applier: drop it, re-run twice — CREATE INDEX IF NOT EXISTS reconverges.
    conn.execute("DROP INDEX idx_edges_target_qname", []).unwrap();
    assert_eq!(index_on(&conn), 0, "index dropped");
    schema::apply_edge_target_qname_index(&conn).unwrap();
    schema::apply_edge_target_qname_index(&conn).expect("replay is a no-op");
    assert_eq!(index_on(&conn), 1, "the isolated applier recreates the index");

    // A forward migrate over a ledger truncated below V071 replays the step and records V071.
    truncate_schema_to(&conn, 70);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip",
    );
    let v71_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '071_edge_target_qname_index'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v71_recorded, 1, "the forward migration records V071");
}

#[test]
fn migration_072_queues_pending_refold() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    // The deferred-refold work queue exists with just its stream_id primary key.
    assert!(
        conn_table_columns(&conn, "content_streams_pending_refold")
            .contains(&"stream_id".to_string()),
        "V072 gives content_streams_pending_refold its stream_id column",
    );

    // PRIMARY KEY(stream_id) is what dedups repeat enqueues of one stream into a single queued
    // refold: a plain duplicate is refused, and the INSERT OR IGNORE the ingest path uses is a
    // no-op rather than an error.
    conn.execute("INSERT INTO content_streams_pending_refold(stream_id) VALUES (zeroblob(32))", [])
        .expect("first enqueue inserts");
    assert!(
        conn.execute(
            "INSERT INTO content_streams_pending_refold(stream_id) VALUES (zeroblob(32))",
            [],
        )
        .is_err(),
        "the stream_id primary key rejects a duplicate enqueue of one stream",
    );
    conn.execute(
        "INSERT OR IGNORE INTO content_streams_pending_refold(stream_id) VALUES (zeroblob(32))",
        [],
    )
    .expect("INSERT OR IGNORE dedups a repeat enqueue without erroring");

    // CHECK(length(stream_id) = 32) rejects a non-32-byte stream_id — a stream id is a sha256, so a
    // shorter/longer blob is corruption, matching every sibling 32-byte-blob column.
    assert!(
        conn.execute(
            "INSERT INTO content_streams_pending_refold(stream_id) VALUES (zeroblob(31))",
            [],
        )
        .is_err(),
        "a 31-byte stream_id violates the length CHECK",
    );

    // Deferred-absence in ISOLATION: a bare DB lacks the table until the V072 applier runs, and a
    // replay is an idempotent no-op (CREATE ... IF NOT EXISTS).
    let isolated = rusqlite::Connection::open_in_memory().unwrap();
    assert!(
        !conn_table_exists(&isolated, "content_streams_pending_refold"),
        "bare DB lacks content_streams_pending_refold before the isolated apply",
    );
    schema::apply_content_streams_pending_refold(&isolated).unwrap();
    schema::apply_content_streams_pending_refold(&isolated).expect("replay is a no-op");
    assert!(
        conn_table_columns(&isolated, "content_streams_pending_refold")
            .contains(&"stream_id".to_string()),
        "the isolated applier recreates the table",
    );

    // A forward migrate over a ledger truncated below V072 replays the step and records V072.
    truncate_schema_to(&conn, 71);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "forward migrate reaches the tip",
    );
    let v72_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '072_content_streams_pending_refold'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v72_recorded, 1, "the forward migration records V072");
}

#[test]
fn migration_047_deferred_absence_and_reconverges_from_torn_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE memory_model_failures(leftover INTEGER);").unwrap();
    assert!(
        !conn_table_columns(&conn, "memory_model_failures").contains(&"reason".to_string()),
        "torn table lacks the sentinel column"
    );

    schema::apply_memory_model_failures_table(&conn).unwrap();

    assert!(
        conn_table_columns(&conn, "memory_model_failures").contains(&"reason".to_string()),
        "V047 drops the torn scratch table and creates the real shape"
    );
    schema::apply_memory_model_failures_table(&conn).expect("replay is a no-op");
    conn.execute(
        "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
         prompt_version, reason, failed_at_ms) VALUES \
         ('m','r','verify','h','model','prompt','fabricated_evidence',0)",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
             prompt_version, reason, failed_at_ms) VALUES \
             ('m','r','verify','h2','model','prompt','malformed_verdict',1)",
            [],
        )
        .is_err(),
        "PK(repo_id, memory_id, pass) rejects a second current failure for the same pass"
    );
    conn.execute(
        "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
         prompt_version, reason, failed_at_ms) VALUES \
         ('m','r','compact','h','model','prompt','summary_guard_rejected',0)",
        [],
    )
    .expect("verify and compact failures are distinct pass rows");
}

/// V046 in ISOLATION against a bare connection: the sibling tables are ABSENT before the migration
/// runs (the deferred-absence assertion anchored to the migration DDL, NOT the full ladder — the
/// documented breakage class), it re-converges from a torn `memory_summaries` scratch table,
/// replays as a no-op, and its keys hold (a duplicate reality row violates the PK; a new
/// content_hash is a new summary row).
#[test]
fn migration_046_deferred_absence_and_reconverges_from_torn_state() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    // A crashed prior V046 pass could leave a partial `memory_summaries`; the migration's leading
    // DROP re-converges from it.
    conn.execute_batch("CREATE TABLE memory_summaries(leftover INTEGER);").unwrap();
    // Deferred-absence, anchored to the migration DDL in isolation (never against the full ladder,
    // whose end state always has the table): the sentinel table is absent before V046 runs.
    assert!(!conn_table_exists(&conn, "memory_reality"), "memory_reality absent before V046 runs");

    schema::apply_memory_verification_tables(&conn).unwrap();

    assert!(conn_table_exists(&conn, "memory_reality"), "V046 creates memory_reality");
    assert!(conn_table_exists(&conn, "memory_summaries"), "V046 creates memory_summaries");
    assert!(
        conn_table_columns(&conn, "memory_summaries").contains(&"repo_id".to_string()),
        "the torn scratch table was dropped and recreated with the real shape"
    );
    // Replay short-circuits on the sentinel.
    schema::apply_memory_verification_tables(&conn).expect("replay is a no-op");

    // memory_reality PK (repo_id, memory_id): one row per memory; a duplicate is rejected.
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_at_ms) VALUES \
         ('m','r','h',0)",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_at_ms) VALUES \
             ('m','r','h2',1)",
            [],
        )
        .is_err(),
        "PK(repo_id, memory_id) rejects a second reality row for the same memory"
    );
    // memory_summaries admits a second content_hash for the same memory (the self-invalidation
    // shape).
    conn.execute(
        "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, generated_at_ms) \
         VALUES ('m','r','h','s',0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, generated_at_ms) \
         VALUES ('m','r','h2','s2',0)",
        [],
    )
    .expect("a new content_hash is a distinct summary row");
}

/// #585: bringing the schema current records WHO did it into the global `index_meta`, so a stranded
/// fleet is diagnosable in one query instead of forensics. Covers the `schema::apply` (create /
/// funnel-2) path.
#[test]
fn applying_the_schema_records_migration_provenance() {
    use rusqlite::OptionalExtension;

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    let read = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
            .optional()
            .unwrap()
    };

    assert_eq!(
        read("last_migration_to_version").as_deref(),
        Some(schema::LATEST_SCHEMA_VERSION.to_string().as_str()),
        "provenance records the schema version migrated TO"
    );
    assert_eq!(
        read("last_migration_binary_version").as_deref(),
        Some(rag_rat_base::version::binary_version()),
        "provenance records this binary's version string"
    );
    assert!(read("last_migration_binary_exe").is_some(), "provenance records the binary path");
    assert!(
        read("last_migration_at_ms").and_then(|value| value.parse::<i64>().ok()).unwrap_or(0) > 0,
        "provenance records a timestamp"
    );
}

/// #585: the `Newer`-schema refusal (what every stranded process prints) names this binary's schema
/// ceiling AND who last migrated the store, so the fleet outage is diagnosable from the error text.
#[test]
fn newer_schema_refusal_names_the_migrating_binary_and_ceiling() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap(); // stamps provenance for THIS binary
    // Fabricate a Newer schema: an applied migration this binary doesn't know.
    conn.execute(
        "INSERT INTO schema_version(id, applied_at_ms, checksum, description) VALUES \
         ('999_from_the_future', 0, 'sha256:future', 'a migration this binary lacks')",
        [],
    )
    .unwrap();

    let status = schema::status(&conn).unwrap();
    assert_eq!(status.state, schema::SchemaState::Newer);
    assert!(
        status.message.contains(rag_rat_base::version::binary_version()),
        "refusal should name the migrating binary version; got: {}",
        status.message
    );
    assert!(
        status.message.contains(&schema::LATEST_SCHEMA_VERSION.to_string()),
        "refusal should name this binary's schema ceiling; got: {}",
        status.message
    );
}

/// #585: a forward migration of an existing store (the `migrate_forward` funnel) also stamps
/// provenance — the stranding path, not just create.
#[test]
fn forward_migration_records_migration_provenance() {
    use rusqlite::OptionalExtension;

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // Roll back one migration and clear provenance, then forward-migrate.
    conn.execute(
        "DELETE FROM schema_version WHERE id = (SELECT id FROM schema_version ORDER BY id DESC \
         LIMIT 1)",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM index_meta WHERE key LIKE 'last_migration_%'", []).unwrap();
    assert_eq!(schema::status(&conn).unwrap().state, schema::SchemaState::Older);

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

    let to: Option<String> = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'last_migration_to_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(to.as_deref(), Some(schema::LATEST_SCHEMA_VERSION.to_string().as_str()));
}

/// V087 adds the table→log sync engine's bookkeeping tables. (No longer the schema tip — the
/// absolute pin moved to `migration_088_caches_the_generation_posting_row_count`.)
#[test]
fn migration_087_adds_table_sync_tables() {
    let added =
        ["table_sync_entries", "sync_published_rows", "sync_row_tombstones", "sync_row_clocks"];

    // Absence asserted against the migration's OWN precondition (a bare connection), not the full
    // ladder's end state — a fresh in-memory DB has none of these tables before the applier runs.
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    for t in added {
        assert!(!schema::table_exists(&bare, t).unwrap(), "pre-V087 has no {t}");
    }

    schema::apply_table_sync_tables(&bare).unwrap();

    for t in added {
        assert!(schema::table_exists(&bare, t).unwrap(), "V087 adds {t}");
    }
    // The applier is idempotent (CREATE TABLE IF NOT EXISTS) — a second run is a no-op, not an
    // error.
    schema::apply_table_sync_tables(&bare).unwrap();
    // The whole-row LWW clock is keyed per row; a duplicate row key collides on the composite PK.
    bare.execute(
        "INSERT INTO sync_row_clocks(repo_id, table_name, row_pk, lamport, device_fingerprint) \
         VALUES ('r', 't', 'pk', 1, 'dev')",
        [],
    )
    .unwrap();
    assert!(
        bare.execute(
            "INSERT INTO sync_row_clocks(repo_id, table_name, row_pk, lamport, \
             device_fingerprint) VALUES ('r', 't', 'pk', 2, 'dev')",
            [],
        )
        .is_err(),
        "the row write clock has one row per (repo, table, row_pk)",
    );

    // The full ladder ends with the tables present and records V087.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    for t in added {
        assert!(schema::table_exists(&conn, t).unwrap(), "the full ladder ends with {t}");
    }
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '087_table_sync_bookkeeping'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V087");
}

/// V088 (#830) caches each clone generation's posting-row count on its generation row so the #598
/// delta work budget reads one column instead of a full `COUNT(*)` scan of the postings table.
/// Verifies the column exists on a fresh ladder and that the applier backfills an existing
/// generation from its actual postings.
#[test]
fn migration_088_caches_the_generation_posting_row_count() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply"
    );
    assert!(
        conn_table_columns(&conn, "clone_graph_generations")
            .contains(&"postings_row_count".to_string()),
        "generations carry the cached posting-row count that sizes the #598 work budget (#830)"
    );

    // Seed a generation with three postings, then DROP the column and re-run the applier in
    // isolation: the backfill must recompute the count from the actual postings, not leave it 0.
    conn.execute(
        "INSERT INTO clone_graph_generations
            (generation, status, theta_floor, normalizer_kind, normalizer_version,
             source_revision, started_at_ms, postings_written)
         VALUES (7, 'Complete', 0.7, 'baseline', 3, 'rev', 0, 1)",
        [],
    )
    .unwrap();
    for token in [10, 20, 30] {
        conn.execute(
            "INSERT INTO clone_subblock_postings
                (build_generation, token_hash, path, start_byte, file_sha)
             VALUES (7, ?1, 'src/a.rs', ?1, 'sha')",
            [token],
        )
        .unwrap();
    }
    conn.execute_batch("ALTER TABLE clone_graph_generations DROP COLUMN postings_row_count;")
        .unwrap();
    schema::apply_clone_postings_row_count(&conn).unwrap();
    let backfilled: i64 = conn
        .query_row(
            "SELECT postings_row_count FROM clone_graph_generations WHERE generation = 7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(backfilled, 3, "the backfill counts the generation's actual postings");

    // Additive + idempotent: a re-apply keeps the column and its value.
    schema::apply_clone_postings_row_count(&conn).unwrap();
    let after_reapply: i64 = conn
        .query_row(
            "SELECT postings_row_count FROM clone_graph_generations WHERE generation = 7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after_reapply, 3, "a re-apply recomputes the same exact count");
}

/// V089 (#945) adds the durable one-time invite row redeemed by the enrollment ALPN.
#[test]
fn migration_089_adds_sync_invites() {
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_sync_invites(&bare).unwrap();
    schema::apply_sync_invites(&bare).expect("replay is a no-op");
    assert!(conn_table_exists(&bare, "sync_invites"));
    assert!(
        !conn_table_exists(&bare, "account_candidate_reservations"),
        "V089 stays frozen; the reservation table is V090",
    );
    assert!(
        bare.execute(
            "INSERT INTO sync_invites(
                 nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms
             ) VALUES (zeroblob(32), zeroblob(32), 'administrator', NULL, 2, 1, NULL)",
            [],
        )
        .is_err(),
        "the store rejects roles outside the frozen three-level vocabulary",
    );
    assert!(
        bare.execute(
            "INSERT INTO sync_invites(
                 nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms
             ) VALUES (zeroblob(32), zeroblob(32), 'member', NULL, 2, 1, 2)",
            [],
        )
        .is_err(),
        "a consumed invite must persist its complete replay identity and receipt",
    );

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version WHERE id = '089_sync_invites'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V089");
}

/// V090 (#949) adds durable candidate-capacity reservations for outstanding enrollment invites.
#[test]
fn migration_090_adds_account_candidate_reservations() {
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_account_candidate_reservations(&bare).unwrap();
    schema::apply_account_candidate_reservations(&bare).expect("replay is a no-op");
    assert!(conn_table_exists(&bare, "account_candidate_reservations"));
    assert!(
        bare.execute(
            "INSERT INTO account_candidate_reservations(
                 reservation_id, account_id, reserved_entries, reserved_bytes, expires_at_ms
             ) VALUES (zeroblob(32), zeroblob(32), -1, 0, 2)",
            [],
        )
        .is_err(),
        "a reservation cannot hold a negative entry count",
    );

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '090_account_candidate_reservations'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V090");
}

/// V092 (#949) drops the duplicated per-invite receipt copy; replay reconstructs the bootstrap
/// from the grow-only candidate DAG.
#[test]
fn migration_092_normalizes_invite_receipts() {
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 92, "move this pin with the next schema migration");

    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_sync_invites(&bare).unwrap();
    bare.execute(
        "INSERT INTO sync_invites(
             nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms,
             used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
             receipt_hash, receipt_signed, receipt_bytes
         ) VALUES (zeroblob(32), zeroblob(32), 'member', NULL, 10, 1, 2,
                   zeroblob(32), zeroblob(32), zeroblob(32),
                   zeroblob(32), X'01', X'01020304')",
        [],
    )
    .unwrap();
    schema::apply_sync_invites_normalized_receipts(&bare).unwrap();
    schema::apply_sync_invites_normalized_receipts(&bare).expect("replay is a no-op");
    let (role, receipt, legacy): (String, Vec<u8>, Vec<u8>) = bare
        .query_row(
            "SELECT role, receipt_signed, receipt_bytes FROM sync_invites
              WHERE nonce = zeroblob(32)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (role.as_str(), receipt.as_slice(), legacy.as_slice()),
        ("member", &[1][..], &[1, 2, 3, 4][..]),
        "a consumed invite keeps replaying through its window"
    );

    // Replaying V092 over a POST-V092 row (the `index --full` recovery path) preserves the
    // normalized manifest rather than nulling it into the consumed-row CHECK.
    bare.execute(
        "INSERT INTO sync_invites(
             nonce, account_id, role, label, expires_at_ms, created_at_ms, used_at_ms,
             used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
             receipt_hash, receipt_signed, receipt_entries, receipt_bytes
         ) VALUES (X'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', zeroblob(32), 'member', NULL, 10, 1, 2,
                   zeroblob(32), zeroblob(32), zeroblob(32),
                   zeroblob(32), X'01', X'02020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202020202', NULL)",
        [],
    )
    .unwrap();
    schema::apply_sync_invites_normalized_receipts(&bare).unwrap();
    let manifest: Vec<u8> = bare
        .query_row(
            "SELECT receipt_entries FROM sync_invites
              WHERE nonce = X'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(manifest.len(), 64, "the re-applied migration preserves stored manifests");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, 92);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version
              WHERE id = '092_sync_invites_normalized_receipts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V092");
}

/// V091 (#949) tracks the live key-target count each invite reservation covers, so fold-time
/// growth — local or synced — can top reservations up to the current mandatory cost.
#[test]
fn migration_091_adds_account_candidate_reservation_targets() {
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_account_candidate_reservations(&bare).unwrap();
    bare.execute(
        "INSERT INTO account_candidate_reservations(
             reservation_id, account_id, reserved_entries, reserved_bytes, expires_at_ms
         ) VALUES (zeroblob(32), zeroblob(32), 4, 900, 10)",
        [],
    )
    .unwrap();
    schema::apply_account_candidate_reservation_targets(&bare).unwrap();
    schema::apply_account_candidate_reservation_targets(&bare).expect("replay is a no-op");
    let targets: i64 = bare
        .query_row(
            "SELECT reserved_targets FROM account_candidate_reservations
              WHERE reservation_id = zeroblob(32)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(targets, 3, "the backfill recovers reserved_entries - 1 covered targets");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version
              WHERE id = '091_account_candidate_reservation_targets'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V091");
}
