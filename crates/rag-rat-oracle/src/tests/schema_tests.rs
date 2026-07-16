use super::*;

/// The migration creates the side tables with the expected columns + STRICT mode.
#[test]
fn migration_creates_oracle_side_tables() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();

    for table in ["oracle_runs", "edge_oracle"] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{table} table must exist");
    }

    // STRICT mode (repo convention for new tables); NO FK to edges_data (#248 — the V018 cascade
    // wiped verdicts on reindex).
    let edge_oracle_sql: String = conn
        .query_row("SELECT sql FROM sqlite_master WHERE name = 'edge_oracle'", [], |row| row.get(0))
        .unwrap();
    assert!(edge_oracle_sql.contains("STRICT"), "edge_oracle must be STRICT");
    assert!(
        !edge_oracle_sql.to_uppercase().contains("FOREIGN KEY"),
        "edge_oracle must NOT carry an FK to edges_data — a reindex CASCADE would wipe verdicts"
    );

    // Content-key columns (#248) replace the volatile `edge_id`.
    let columns = table_columns(&conn, "edge_oracle");
    for expected in [
        "source_path",
        "source_start_byte",
        "source_end_byte",
        "callee_start_byte",
        "callee_end_byte",
        "edge_kind",
        "file_sha",
        "tool",
        "tool_version",
        "resolved_symbol_id",
        "scip_symbol",
        "kind",
        "computed_at",
    ] {
        assert!(columns.contains(&expected.to_string()), "edge_oracle missing {expected}");
    }
    assert!(!columns.contains(&"edge_id".to_string()), "edge_id column was dropped");
}

/// The V019 moniker migration: the `logical_symbol_monikers` table (STRICT, NO foreign key — see
/// the migration's invariant comment: an FK would cascade-wipe monikers on every
/// `rebuild_logical_symbols` DELETE-all pass) plus the moniker provenance + relocation-reason
/// columns on `repo_memory_bindings`.
#[test]
fn migration_creates_moniker_table_and_binding_columns() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();

    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'logical_symbol_monikers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("STRICT"), "logical_symbol_monikers must be STRICT");
    assert!(
        !sql.to_uppercase().contains("FOREIGN KEY"),
        "logical_symbol_monikers must NOT carry an FK to logical_symbols — the DELETE-all \
         logical-symbol rebuild would cascade-wipe monikers on every index pass"
    );

    let columns = table_columns(&conn, "logical_symbol_monikers");
    for expected in ["logical_symbol_id", "tool", "tool_version", "moniker", "computed_at"] {
        assert!(
            columns.contains(&expected.to_string()),
            "logical_symbol_monikers missing {expected}"
        );
    }

    let binding_columns = table_columns(&conn, "repo_memory_bindings");
    for expected in ["moniker_tool", "moniker_tool_version", "relocation_reason"] {
        assert!(
            binding_columns.contains(&expected.to_string()),
            "repo_memory_bindings missing {expected}"
        );
    }
}

fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// #248: deleting an `edges` row no longer cascades to its `edge_oracle` verdict (the FK is gone —
/// it CASCADE-wiped every verdict on reindex). Instead the verdict stops RESOLVING: the content
/// join finds no live edge, so the surfacing/metric reads return nothing — the moniker model
/// (dangling never resolves). The physical row persists until the next run's clear or gc sweeps it.
#[test]
fn deleting_an_edge_leaves_a_dangling_verdict_that_does_not_resolve() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); }\n");
    let edge = h.add_edge(f, "target", 14, 20, "NameOnly", None);
    let file_sha: String = h
        .conn
        .query_row("SELECT sha256 FROM files WHERE id = ?1", params![f], |r| r.get(0))
        .unwrap();
    h.write_verdict(edge, &file_sha, None, "s", OracleResolutionKind::Upgrade);
    assert!(h.verdict(edge).is_some(), "verdict resolves before delete");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        1,
        "the live verdict is counted before delete"
    );

    h.conn.execute("DELETE FROM edges WHERE id = ?1", params![edge]).unwrap();

    assert!(h.verdict(edge).is_none(), "no live edge → the verdict no longer resolves");
    assert_eq!(
        store::count_edge_oracle_scoped(&h.conn, TOOL, VERSION, COMMIT, WORKTREE, None).unwrap(),
        0,
        "the dangling verdict is excluded from the scoped count (live-edge join)"
    );
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(
        remaining, 1,
        "no FK cascade: the physical row survives the edge delete (swept later)"
    );
}

// ---------------------------------------------------------------------------
// store.rs — side-table I/O round trips, candidate scoping, staleness key.
// ---------------------------------------------------------------------------
