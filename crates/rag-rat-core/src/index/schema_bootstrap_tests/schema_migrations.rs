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
    schema::migrations::apply_oplog_storage(&conn).unwrap();
    schema::migrations::apply_oplog_storage(&conn).expect("replay is a no-op");
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
    schema::migrations::apply_oplog_stream_scoping(&conn).unwrap();
    schema::migrations::apply_oplog_stream_scoping(&conn).expect("replay reconverges");
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
    schema::migrations::apply_oplog_device_identity(&conn).unwrap();
    schema::migrations::apply_oplog_device_identity(&conn).expect("replay is a no-op");
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
    schema::migrations::apply_git_change_couplings(&conn).unwrap();
    schema::migrations::apply_git_change_couplings(&conn).expect("replay is a no-op");
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
    schema::migrations::apply_external_symbols(&conn).unwrap();
    schema::migrations::apply_external_symbols(&conn).expect("replay is a no-op");
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
    schema::migrations::apply_oplog_device_identity(&isolated).unwrap();
    for column in ["x25519_secret", "x25519_public"] {
        assert!(
            !conn_table_columns(&isolated, "oplog_device_identity").contains(&column.to_string()),
            "the V054 table alone lacks the {column} column"
        );
    }
    schema::migrations::apply_oplog_device_x25519(&isolated).unwrap();
    schema::migrations::apply_oplog_device_x25519(&isolated).expect("replay is a no-op");
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
    schema::migrations::apply_account_candidate_dag(&isolated).unwrap();
    schema::migrations::apply_account_candidate_dag(&isolated).expect("replay is a no-op");
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
    schema::migrations::apply_account_authority_projection(&isolated).unwrap();
    schema::migrations::apply_account_authority_projection(&isolated)
        .expect("V064 replay is idempotent");
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
    schema::migrations::apply_account_authority_boundaries(&conn)
        .expect("V065 replay is idempotent");
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
    schema::migrations::apply_content_candidate_dag(&conn).expect("V066 replay is idempotent");
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
    schema::migrations::apply_oplog_local_account(&isolated).unwrap();
    schema::migrations::apply_oplog_local_account(&isolated).expect("replay is a no-op");
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
    schema::migrations::apply_content_projected_tables(&isolated).unwrap();
    schema::migrations::apply_content_projected_tables(&isolated).expect("replay is a no-op");
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
    schema::migrations::apply_edge_target_qname_index(&conn).unwrap();
    schema::migrations::apply_edge_target_qname_index(&conn).expect("replay is a no-op");
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
    schema::migrations::apply_content_streams_pending_refold(&isolated).unwrap();
    schema::migrations::apply_content_streams_pending_refold(&isolated).expect("replay is a no-op");
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

    schema::migrations::apply_memory_model_failures_table(&conn).unwrap();

    assert!(
        conn_table_columns(&conn, "memory_model_failures").contains(&"reason".to_string()),
        "V047 drops the torn scratch table and creates the real shape"
    );
    schema::migrations::apply_memory_model_failures_table(&conn).expect("replay is a no-op");
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

    schema::migrations::apply_memory_verification_tables(&conn).unwrap();

    assert!(conn_table_exists(&conn, "memory_reality"), "V046 creates memory_reality");
    assert!(conn_table_exists(&conn, "memory_summaries"), "V046 creates memory_summaries");
    assert!(
        conn_table_columns(&conn, "memory_summaries").contains(&"repo_id".to_string()),
        "the torn scratch table was dropped and recreated with the real shape"
    );
    // Replay short-circuits on the sentinel.
    schema::migrations::apply_memory_verification_tables(&conn).expect("replay is a no-op");

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

    schema::migrations::apply_table_sync_tables(&bare).unwrap();

    for t in added {
        assert!(schema::table_exists(&bare, t).unwrap(), "V087 adds {t}");
    }
    // The applier is idempotent (CREATE TABLE IF NOT EXISTS) — a second run is a no-op, not an
    // error.
    schema::migrations::apply_table_sync_tables(&bare).unwrap();
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
    schema::migrations::apply_clone_postings_row_count(&conn).unwrap();
    let backfilled: i64 = conn
        .query_row(
            "SELECT postings_row_count FROM clone_graph_generations WHERE generation = 7",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(backfilled, 3, "the backfill counts the generation's actual postings");

    // Additive + idempotent: a re-apply keeps the column and its value.
    schema::migrations::apply_clone_postings_row_count(&conn).unwrap();
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
    schema::migrations::apply_sync_invites(&bare).unwrap();
    schema::migrations::apply_sync_invites(&bare).expect("replay is a no-op");
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
    schema::migrations::apply_account_candidate_reservations(&bare).unwrap();
    schema::migrations::apply_account_candidate_reservations(&bare).expect("replay is a no-op");
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
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_sync_invites(&bare).unwrap();
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
    schema::migrations::apply_sync_invites_normalized_receipts(&bare).unwrap();
    schema::migrations::apply_sync_invites_normalized_receipts(&bare).expect("replay is a no-op");
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
    schema::migrations::apply_sync_invites_normalized_receipts(&bare).unwrap();
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
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
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

/// V096 (#1058) adds the table holding entries whose chain predecessor has not arrived.
///
/// Asserted against the migration's own DDL on a bare connection, not the full ladder's end state:
/// the directory rule for this suite is that a table's arrival is proved by the step that creates
/// it, so the assertion keeps meaning something when a later migration also touches it.
#[test]
fn migration_096_holds_entries_awaiting_a_chain_predecessor() {
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    assert!(
        !schema::table_exists(&bare, "table_sync_gapped_entries").unwrap(),
        "the table arrives with V096, not before"
    );
    schema::migrations::apply_table_sync_gapped_entries(&bare).unwrap();
    schema::migrations::apply_table_sync_gapped_entries(&bare).expect("replay is a no-op");
    assert!(schema::table_exists(&bare, "table_sync_gapped_entries").unwrap());

    // A held entry always cites a predecessor — a genesis can never gap — so the column is NOT
    // NULL.
    let without_predecessor = bare.execute(
        "INSERT INTO table_sync_gapped_entries(entry_hash, stream_id, device_fingerprint, \
         lamport, prev_hash, signed_bytes, gapped_at_ms)
         VALUES (X'01', X'02', X'03', 1, NULL, X'04', 0)",
        [],
    );
    assert!(without_predecessor.is_err(), "a held entry always names the predecessor it waits on");

    // Two equivocating siblings at ONE lamport must both be storable, or whichever arrived first
    // would block the legitimate one. Identity is the entry hash alone.
    bare.execute(
        "INSERT INTO table_sync_gapped_entries(entry_hash, stream_id, device_fingerprint, \
         lamport, prev_hash, signed_bytes, gapped_at_ms)
         VALUES (X'01', X'02', X'03', 1, X'09', X'04', 0)",
        [],
    )
    .unwrap();
    bare.execute(
        "INSERT INTO table_sync_gapped_entries(entry_hash, stream_id, device_fingerprint, \
         lamport, prev_hash, signed_bytes, gapped_at_ms)
         VALUES (X'99', X'02', X'03', 1, X'09', X'04', 0)",
        [],
    )
    .expect("a second entry at the same lamport is a fork to resolve later, not a refusal now");

    // Both indexes carry explicit rationale in the DDL — one serves the promote walks, the other
    // the cap's count and eviction — and no behavioral test can observe a missing index, only a
    // slower one. Pin them.
    for index in [
        "table_sync_gapped_entries_child",
        "table_sync_gapped_entries_chain_lamport",
        "table_sync_gapped_entries_predecessor",
    ] {
        let present: i64 = bare
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1 AND \
                 tbl_name = 'table_sync_gapped_entries'",
                [index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "{index} is created with the table");
    }
    // The predecessor index must actually be CHOSEN, not merely present: the two chain indexes both
    // lead with device_fingerprint, so without it these two per-acceptance probes fall back to the
    // stream_id prefix and scan every held row on the stream. A plan assertion is the only way to
    // see that — a behavioral test observes the same answer either way, just slower.
    for probe in [
        "SELECT entry_hash FROM table_sync_gapped_entries WHERE stream_id = X'01' AND prev_hash = \
         X'02'",
        "SELECT entry_hash FROM table_sync_gapped_entries WHERE stream_id = X'01' AND prev_hash = \
         X'02' AND device_fingerprint != X'03'",
    ] {
        let plan: String =
            bare.query_row(&format!("EXPLAIN QUERY PLAN {probe}"), [], |row| row.get(3)).unwrap();
        assert!(
            plan.contains("table_sync_gapped_entries_predecessor"),
            "the predecessor probe must seek the predecessor index, not scan the stream: {plan}"
        );
    }

    // STRICT, per the schema convention for every new table: a held entry's lamport and hashes are
    // compared as stored, so a silently-coerced value would misorder or mis-key a promotion.
    let ddl: String = bare
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = \
             'table_sync_gapped_entries'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ddl.to_ascii_uppercase().contains("STRICT"), "the table is STRICT: {ddl}");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '096_table_sync_gapped_entries'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V096");
}

/// V095 (#1002) records the TABLE's spec version on each published row, so an unrelated projector
/// bump no longer marks every table's rows incomparable.
#[test]
fn migration_095_records_the_table_spec_version() {
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_table_sync_tables(&bare).unwrap();
    schema::migrations::apply_table_sync_projection_state(&bare).unwrap();
    assert!(
        schema::column_exists(&bare, "sync_published_rows", "projector_version").unwrap(),
        "V093 records the store-global projector version"
    );

    use rusqlite::OptionalExtension;
    // A row published under V093 must SURVIVE the rebuild, not be dropped with the old table.
    bare.execute(
        "INSERT INTO sync_published_rows(repo_id, table_name, row_pk, synced_hash, \
         projector_version)
         VALUES ('repo', 't', 'carried', 'h0', 1)",
        [],
    )
    .unwrap();

    schema::migrations::apply_table_sync_spec_version(&bare).unwrap();
    schema::migrations::apply_table_sync_spec_version(&bare).expect("replay is a no-op");
    let carried: Option<i64> = bare
        .query_row(
            "SELECT spec_version FROM sync_published_rows WHERE row_pk = 'carried'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(
        carried,
        Some(0),
        "the rebuild carries existing records across, stamped as an unknown column set — dropping \
         a published record would strand that row's unsent deletion permanently"
    );
    assert!(schema::column_exists(&bare, "sync_published_rows", "spec_version").unwrap());
    assert!(
        !schema::column_exists(&bare, "sync_published_rows", "projector_version").unwrap(),
        "the store-global version is replaced, not left behind as a dead column"
    );

    // The rebuilt table keeps its identity and its NOT NULL version.
    bare.execute(
        "INSERT INTO sync_published_rows(repo_id, table_name, row_pk, synced_hash, spec_version)
         VALUES ('repo', 't', 'r', 'h', 2)",
        [],
    )
    .unwrap();
    let duplicate = bare.execute(
        "INSERT INTO sync_published_rows(repo_id, table_name, row_pk, synced_hash, spec_version)
         VALUES ('repo', 't', 'r', 'other', 2)",
        [],
    );
    assert!(duplicate.is_err(), "one published record per row identity");
    let missing_version = bare.execute(
        "INSERT INTO sync_published_rows(repo_id, table_name, row_pk, synced_hash)
         VALUES ('repo', 't', 'r2', 'h')",
        [],
    );
    assert!(missing_version.is_err(), "a published hash always states the column set it covers");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '095_table_sync_spec_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V096");

    // `schema::apply` is the `index --full` recovery, and it re-runs the WHOLE ladder over an
    // EXISTING store — so V093 would re-add the column V095 replaced, and V095 can only restore its
    // shape by REBUILDING the table. Both halves of that must hold: the replay converges on the
    // fresh schema, and it does not destroy publication state on the way. The second is the sharp
    // one — `produce_row_ops` finds a locally-deleted row by its surviving published record, so a
    // dropped table strands every unsent deletion and leaves peers holding the row forever.
    conn.execute(
        "INSERT INTO sync_published_rows(
             stream_id, repo_id, table_name, row_pk, synced_hash, spec_version
         ) VALUES (zeroblob(32), 'repo', 't', 'live', 'hash', 3)",
        [],
    )
    .unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).expect("the ladder replays");
    assert!(
        schema::column_exists(&conn, "sync_published_rows", "spec_version").unwrap(),
        "the replayed ladder still ends at the V095 shape"
    );
    assert!(
        !schema::column_exists(&conn, "sync_published_rows", "projector_version").unwrap(),
        "V093's column must not come back on a full-ladder replay — the recovery path has to \
         converge on the same schema as a fresh install"
    );
    let survivor: Option<i64> = conn
        .query_row(
            "SELECT spec_version FROM sync_published_rows WHERE row_pk = 'live'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(
        survivor,
        Some(3),
        "the replay must not rebuild a populated table — a dropped published record reads as an \
         unsent deletion that can never be authored"
    );
}

/// V093 (#1001) records what the table-sync projector could not project, the apply context the
/// one-way stream id hashes away, and the column set each anti-echo hash covers.
#[test]
fn migration_093_adds_table_sync_projection_state() {
    // Absence is asserted against the PRE-V093 DDL in isolation, never against the full ladder
    // (which runs past V093 and would make the check vacuous).
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_table_sync_tables(&bare).unwrap();
    assert!(
        !schema::column_exists(&bare, "table_sync_entries", "pending_reason").unwrap(),
        "the pending mark arrives with V093, not before"
    );
    assert!(
        !schema::column_exists(&bare, "sync_published_rows", "projector_version").unwrap(),
        "the anti-echo hash is unversioned before V093"
    );
    assert!(!schema::table_exists(&bare, "table_sync_streams").unwrap());

    schema::migrations::apply_table_sync_projection_state(&bare).unwrap();
    schema::migrations::apply_table_sync_projection_state(&bare).expect("replay is a no-op");

    assert!(schema::column_exists(&bare, "table_sync_entries", "pending_reason").unwrap());
    assert!(
        schema::column_exists(&bare, "table_sync_entries", "pending_projector_version").unwrap()
    );
    assert!(schema::column_exists(&bare, "table_sync_entries", "quarantine_reason").unwrap());
    assert!(schema::column_exists(&bare, "sync_published_rows", "projector_version").unwrap());
    assert!(schema::table_exists(&bare, "table_sync_streams").unwrap());

    // Each column is guarded INDIVIDUALLY. A store carrying only part of this migration's shape
    // still gains the rest: the ladder records V093 as applied and never re-runs it, so a
    // group-guarded add would leave a column missing on a store that reports itself current.
    let partial = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_table_sync_tables(&partial).unwrap();
    partial.execute("ALTER TABLE table_sync_entries ADD COLUMN pending_reason TEXT", []).unwrap();
    schema::migrations::apply_table_sync_projection_state(&partial).unwrap();
    assert!(
        schema::column_exists(&partial, "table_sync_entries", "quarantine_reason").unwrap(),
        "a partially-migrated store still gains the columns it is missing"
    );
    assert!(schema::column_exists(&partial, "sync_published_rows", "projector_version").unwrap());

    // The directory is STRICT and keyed by the stream id, so one stream resolves to exactly one
    // apply context.
    bare.execute(
        "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, scope_id)
         VALUES (zeroblob(32), 'repo', zeroblob(32), 'anchors/1')",
        [],
    )
    .unwrap();
    let duplicate = bare.execute(
        "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, scope_id)
         VALUES (zeroblob(32), 'other', zeroblob(32), 'overlay/1')",
        [],
    );
    assert!(duplicate.is_err(), "a stream id resolves to one apply context");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '093_table_sync_projection_state'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V093");
}

/// V100 (#976) adds receiver_type_hint_id to edges_data and updates the edges view.
#[test]
fn migration_100_receiver_type_hint_interning() {
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&bare, &crate::index::migration_hooks()).unwrap();
    schema::migrations::apply_receiver_type_hint_interning(&bare).unwrap();
    schema::migrations::apply_receiver_type_hint_interning(&bare).expect("replay is a no-op");
    assert!(schema::column_exists(&bare, "edges_data", "receiver_type_hint_id").unwrap());
    assert!(
        schema::column_exists(&bare, "repo_memory_call_path_edges", "callee_logical_symbol_id")
            .unwrap()
    );
    assert!(
        schema::column_exists(&bare, "repo_memory_call_path_edges", "callee_identity_known")
            .unwrap()
    );

    assert_eq!(schema::status(&bare).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = bare
        .query_row(
            "SELECT COUNT(*) FROM schema_version
              WHERE id = '100_receiver_type_hint_interning'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V100");
}

/// V101 (#1014) makes graph and scope healing resumable per file row.
#[test]
fn migration_101_file_graph_version_provenance() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE files(path TEXT, repo_id TEXT, generation INTEGER);
         CREATE TABLE repo_meta(repo_id TEXT, key TEXT, value TEXT);
         INSERT INTO repo_meta VALUES ('repo', 'graph_index_version', '15');
         INSERT INTO repo_meta VALUES ('repo', 'logical_key_version', '2');
         INSERT INTO files VALUES ('src/lib.rs', 'repo', 1);",
    )
    .unwrap();

    schema::migrations::apply_file_graph_version_provenance(&conn).unwrap();
    let versions: (i64, i64) = conn
        .query_row(
            "SELECT graph_version, scope_version FROM files WHERE path = 'src/lib.rs'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(versions, (15, 2), "existing rows inherit both prior repo stamps");

    conn.execute("UPDATE files SET graph_version = 16, scope_version = 3", []).unwrap();
    schema::migrations::apply_file_graph_version_provenance(&conn).expect("replay is a no-op");
    let versions: (i64, i64) = conn
        .query_row("SELECT graph_version, scope_version FROM files", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(versions, (16, 3), "replay preserves per-row progress");
    for index in
        ["idx_files_repo_generation_graph_version", "idx_files_repo_generation_scope_version"]
    {
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)",
                [index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "{index} exists");
    }
}

/// V103 (#1109) makes memory bindings deterministic whole-row `anchors/1` state.
#[test]
fn migration_103_syncable_memory_bindings() {
    assert_eq!(schema::LATEST_SCHEMA_VERSION, 103, "move this pin with the next schema migration");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute_batch(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms)
             VALUES ('repo-a', 'repo-a', 0);
         INSERT INTO repo_memories(
             id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
             input_hash, memory_version, repo_id)
             VALUES ('memory-a', 'Invariant', 'title', 'body', 'high', 'active', 0, 0,
                     'agent', 'hash', 'v1', 'repo-a');
         INSERT INTO repo_memory_bindings(
             repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
             logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, tracker, project,
             item_key, anchor_status, created_at_ms, symbol_kind, signature_hash, moniker_tool,
             moniker_tool_version, relocation_reason, downgrade_pending_at_ms)
             VALUES ('repo-a', 'memory-a', 'path', 'src/lib.rs', 'src/lib.rs', 1, 2,
                     3, 4, 5, 6, 'commit', 'github', 'owner/repo', '7', 'relocated', 8,
                     'function', 'signature', 'scip-rust', '1', 'moniker-match', 9);
         ALTER TABLE repo_memory_bindings RENAME TO repo_memory_bindings_v103_shape;
         CREATE TABLE repo_memory_bindings(
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
             PRIMARY KEY(memory_id, binding_kind, binding_id),
             FOREIGN KEY(memory_id) REFERENCES repo_memories(id) ON DELETE CASCADE
         );
         INSERT INTO repo_memory_bindings SELECT * FROM repo_memory_bindings_v103_shape;
         DROP TABLE repo_memory_bindings_v103_shape;
         CREATE TRIGGER memory_bindings_lens_revision_insert
             AFTER INSERT ON repo_memory_bindings BEGIN SELECT 1; END;",
    )
    .unwrap();

    schema::migrations::apply_syncable_memory_bindings(&conn).unwrap();
    schema::migrations::apply_syncable_memory_bindings(&conn).expect("replay is a no-op");

    let strict: i64 = conn
        .query_row(
            "SELECT strict FROM pragma_table_list
             WHERE schema = 'main' AND name = 'repo_memory_bindings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(strict, 1);
    let mut pk = conn
        .prepare(
            "SELECT name FROM pragma_table_info('repo_memory_bindings')
             WHERE pk > 0 ORDER BY pk",
        )
        .unwrap();
    let pk: Vec<String> =
        pk.query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(pk, ["repo_id", "memory_id", "binding_kind", "binding_id"]);
    let foreign_keys: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_list('repo_memory_bindings')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(foreign_keys, 0);
    let triggers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'trigger' AND tbl_name = 'repo_memory_bindings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(triggers, 0);
    let preserved: (String, i64, i64, i64, String, i64) = conn
        .query_row(
            "SELECT path, logical_symbol_id, symbol_id, chunk_id, anchor_status,
                    downgrade_pending_at_ms
             FROM repo_memory_bindings
             WHERE repo_id = 'repo-a' AND memory_id = 'memory-a'",
            [],
            |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
            },
        )
        .unwrap();
    assert_eq!(preserved, ("src/lib.rs".into(), 3, 4, 5, "relocated".into(), 9));
}

/// V091 (#949) tracks the live key-target count each invite reservation covers, so fold-time
/// growth — local or synced — can top reservations up to the current mandatory cost.
#[test]
fn migration_091_adds_account_candidate_reservation_targets() {
    let bare = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_account_candidate_reservations(&bare).unwrap();
    bare.execute(
        "INSERT INTO account_candidate_reservations(
             reservation_id, account_id, reserved_entries, reserved_bytes, expires_at_ms
         ) VALUES (zeroblob(32), zeroblob(32), 4, 900, 10)",
        [],
    )
    .unwrap();
    schema::migrations::apply_account_candidate_reservation_targets(&bare).unwrap();
    schema::migrations::apply_account_candidate_reservation_targets(&bare)
        .expect("replay is a no-op");
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

#[test]
fn migration_094_tracks_lens_enrichment_changes_in_constant_time() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO papertrail_refs(
             tracker, project, item_key, item_kind, ref_kind, source_kind, source_path,
             source_text, discovered_at_ms, repo_id
         ) VALUES ('github', 'owner/repo', '1', 'issue', 'reference', 'path', 'src/lib.rs',
                   'mentions #1', 1, '__unassigned__')",
        [],
    )
    .unwrap();
    let revision = || {
        conn.query_row(
            "SELECT CAST(value AS INTEGER) FROM repo_meta
             WHERE repo_id = '__unassigned__' AND key = ?1",
            [rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(revision(), 1);

    conn.execute(
        "UPDATE papertrail_refs SET ref_kind = 'closing' WHERE repo_id = '__unassigned__'",
        [],
    )
    .unwrap();
    assert_eq!(revision(), 2, "an in-place promotion increments the clock");
    conn.execute("DELETE FROM papertrail_refs WHERE repo_id = '__unassigned__'", []).unwrap();
    assert_eq!(revision(), 3, "deletion increments the clock");

    let trigger_count = |prefix: &str| {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name LIKE ?1",
            [format!("{prefix}%")],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_eq!(
        trigger_count("git_file_changes_lens_revision_"),
        0,
        "bulk history writes must not carry per-row Lens triggers"
    );
    conn.execute(
        "INSERT INTO git_commits(
             hash, author_name, author_email, authored_at_s, committed_at_s,
             subject, body, changed_file_count, repo_id
         ) VALUES ('bulk', 'a', 'a@b', 1, 1, 'bulk', '', 2, '__unassigned__')",
        [],
    )
    .unwrap();
    for path in ["src/a.rs", "src/b.rs"] {
        conn.execute(
            "INSERT INTO git_file_changes(
                 commit_hash, path, additions, deletions, change_kind, repo_id
             ) VALUES ('bulk', ?1, 0, 0, 'modified', '__unassigned__')",
            [path],
        )
        .unwrap();
    }
    assert_eq!(revision(), 3, "history rows are clocked by their transactional writer");

    conn.execute(
        "INSERT INTO oracle_runs(
             repo_id, tool, tool_version, commit_sha, worktree_id, started_at, status, stats_json
         ) VALUES ('__unassigned__', 'scip', '1', 'head', 'active-wt', 1, 'complete', '{}')",
        [],
    )
    .unwrap();
    assert_eq!(revision(), 4, "a zero-verdict Oracle run invalidates Lens");
    conn.execute("UPDATE oracle_runs SET status = 'failed' WHERE repo_id = '__unassigned__'", [])
        .unwrap();
    assert_eq!(revision(), 5, "Oracle run updates invalidate Lens");
    conn.execute("DELETE FROM oracle_runs WHERE repo_id = '__unassigned__'", []).unwrap();
    assert_eq!(revision(), 6, "Oracle run deletion invalidates Lens");

    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms)
         VALUES ('oracle-sibling', 'oracle sibling', 0)",
        [],
    )
    .unwrap();
    rag_rat_db::meta::set_repo_meta(
        &conn,
        "oracle-sibling",
        rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
        "20",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO oracle_runs(
             repo_id, tool, tool_version, commit_sha, worktree_id, started_at, status, stats_json
         ) VALUES ('oracle-sibling', 'scip', '1', 'branch-head', 'linked-wt', 2, 'complete', '{}')",
        [],
    )
    .unwrap();
    let sibling_revision: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM repo_meta
             WHERE repo_id = 'oracle-sibling' AND key = ?1",
            [rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sibling_revision, 21, "the run clocks its owning repo");
    assert_eq!(revision(), 6, "a sibling repo's linked-worktree run stays isolated");

    schema::migrations::apply_lens_enrichment_revision(&conn).unwrap();
    assert_eq!(trigger_count("oracle_runs_lens_revision_"), 3);
    assert_eq!(trigger_count("git_file_changes_lens_revision_"), 0);
    conn.execute(
        "INSERT INTO oracle_runs(
             repo_id, tool, tool_version, commit_sha, worktree_id, started_at, status, stats_json
         ) VALUES ('__unassigned__', 'scip', '2', 'head', 'active-wt', 3, 'complete', '{}')",
        [],
    )
    .unwrap();
    assert_eq!(revision(), 7, "V093 replay must not duplicate Oracle triggers");

    // An Oracle pass writes one `edge_oracle` verdict per resolved edge inside the transaction
    // that commits its run row. Clocking those rows individually would advance the revision
    // hundreds of thousands of times for one publication, so the verdicts carry no trigger at all
    // and the run row is what Lens sees.
    assert_eq!(
        trigger_count("edge_oracle_lens_revision_"),
        0,
        "bulk verdict writes must not carry per-row Lens triggers"
    );
    let before_verdicts = revision();
    for callee_start in 0..64 {
        conn.execute(
            "INSERT INTO edge_oracle(
                 repo_id, source_path, source_start_byte, source_end_byte,
                 callee_start_byte, callee_end_byte, edge_kind, file_sha,
                 tool, tool_version, resolved_symbol_id, scip_symbol, kind, computed_at
             ) VALUES ('__unassigned__', 'src/a.rs', 0, 1, ?1, ?1, 'calls_name', 'sha',
                       'scip', '1', NULL, 'sym', 'confirm', 0)",
            [callee_start],
        )
        .unwrap();
    }
    conn.execute("UPDATE edge_oracle SET kind = 'upgrade' WHERE repo_id = '__unassigned__'", [])
        .unwrap();
    conn.execute("DELETE FROM edge_oracle WHERE repo_id = '__unassigned__'", []).unwrap();
    assert_eq!(
        revision(),
        before_verdicts,
        "verdict rows are clocked by their transactional writer, not one bump per edge"
    );

    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms)
         VALUES ('retired', 'retired', 0)",
        [],
    )
    .unwrap();
    rag_rat_db::meta::set_repo_meta(&conn, "retired", "clone_graph_live_generation", "7").unwrap();
    rag_rat_db::meta::set_repo_meta(
        &conn,
        "retired",
        rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
        "1",
    )
    .unwrap();
    conn.execute("DELETE FROM repos WHERE repo_id = 'retired'", [])
        .expect("retiring a repo with Lens metadata must not reinsert rows during FK cascade");
}

/// V097 (#1048) rekeys the persisted Windows path spellings an older binary wrote in the `\\?\`
/// verbatim form, so an upgrade does not orphan every scoped row it indexed.
///
/// Both halves have to be host-independent, and this pins the LEDGER half. A store migrated on
/// Linux must read as current to a Windows binary — otherwise it is `Older` there and the whole
/// tail re-runs — and a store migrated on Windows must not read as `Newer` on Linux and refuse to
/// open at all. Asserted on whichever platform runs it, so the legs of CI check it between them.
///
/// The ROW half is host-independent too, and is pinned separately by
/// `the_production_pass_rekeys_a_windows_store_on_any_host`: the rekey rule decides a property of
/// the stored string rather than of the host, precisely so this ledger row cannot be stamped by a
/// binary that did not do the work.
#[test]
fn migration_097_records_the_same_ladder_entry_on_every_platform() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    let status = schema::status(&conn).unwrap();
    assert_eq!(status.current_version, schema::LATEST_SCHEMA_VERSION);
    // `Compatible` is the load-bearing half: a ledger row that recorded a DIFFERENT checksum on
    // this platform would surface as `Dirty` here, and a skipped stamp as `Older`.
    assert_eq!(
        status.state,
        schema::SchemaState::Compatible,
        "V097's ledger row must be stamped identically wherever the migration runs",
    );
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '097_windows_verbatim_path_rekey'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V097");
    // `schema::apply` is the `index --full` recovery and replays the WHOLE ladder over an existing
    // store, so the pass has to be replay-safe on the real schema, not just on a fixture.
    schema::migrations::apply_windows_verbatim_path_rekey(&conn).expect("replay is a no-op");
}

/// The rekey's table list is the WHOLE class, checked against the live schema rather than against
/// the author's memory: any table carrying a `worktree_id` is keyed by a canonicalized checkout
/// path and goes stale on the same upgrade, so a new one must join the sweep.
///
/// This is the guard that makes the fix a class fix. Without it, the next table to grow a
/// `worktree_id` silently misses the rekey and its rows are pruned as a dead checkout on the next
/// Windows upgrade — exactly the failure V097 exists to prevent, one table at a time.
#[test]
fn migration_097_covers_every_worktree_id_column_in_the_schema() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT m.name FROM sqlite_master m
             WHERE m.type = 'table'
               AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) p WHERE p.name = 'worktree_id')
             ORDER BY m.name",
        )
        .unwrap();
    let in_schema: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    let mut covered: Vec<String> = schema::migrations::V097_WORKTREE_ID_SCOPED_TABLES
        .iter()
        .map(|t| (*t).to_string())
        .collect();
    covered.sort();
    assert_eq!(
        in_schema, covered,
        "every table with a `worktree_id` must be in V097_WORKTREE_ID_SCOPED_TABLES — an \
         uncovered one keeps the old Windows spelling on upgrade, falls out of the active scope, \
         and is deleted by the next GC as a checkout that no longer exists",
    );
}
