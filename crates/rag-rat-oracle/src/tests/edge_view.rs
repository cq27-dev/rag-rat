use super::*;

// ---------------------------------------------------------------------------
// Edge string interning (#79): compat view shape, round-trip writes, dedup, V020 conversion.
// ---------------------------------------------------------------------------

/// The V020 shape: `edges` is a VIEW over `edges_data` + the `name_strings` dictionary, with
/// INSTEAD OF triggers; both backing tables are STRICT; the int indexes replaced the TEXT ones.
#[test]
fn edges_is_a_compat_view_over_interned_tables() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();

    let object_type = |name: &str| -> String {
        conn.query_row("SELECT type FROM sqlite_master WHERE name = ?1", [name], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(object_type("edges"), "view");
    assert_eq!(object_type("edges_data"), "table");
    assert_eq!(object_type("name_strings"), "table");
    for trigger in ["edges_view_insert", "edges_view_update", "edges_view_delete"] {
        assert_eq!(object_type(trigger), "trigger", "{trigger} must exist");
    }
    let index_table: String = conn
        .query_row("SELECT tbl_name FROM sqlite_master WHERE name = 'idx_edges_to_name'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(index_table, "edges_data", "TEXT indexes were replaced by int indexes");
}

/// Writes through the view round-trip with the legacy semantics: defaults for omitted columns,
/// UPDATE rewrites, DELETE, and shared strings deduplicate in the dictionary.
#[test]
fn view_writes_round_trip_and_dedup() {
    let h = Harness::new();
    let f = h.add_file("a.rs", "fn caller() { target(); helper(); }\n");
    // Two edges sharing from_name/edge_kind/confidence; insert via the view, omitting the
    // defaulted columns (source_* spans, resolution) like legacy SQL could.
    h.conn
        .execute(
            "INSERT INTO edges(source_file_id, from_name, to_name, edge_kind, confidence) VALUES \
             (?1, 'a.rs::caller', 'target', 'calls_name', 'NameOnly')",
            params![f],
        )
        .unwrap();
    h.conn
        .execute(
            "INSERT INTO edges(source_file_id, from_name, to_name, edge_kind, confidence) VALUES \
             (?1, 'a.rs::caller', 'helper', 'calls_name', 'NameOnly')",
            params![f],
        )
        .unwrap();

    let (resolution, start_line): (String, i64) = h
        .conn
        .query_row(
            "SELECT resolution, source_start_line FROM edges WHERE to_name = 'target'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(resolution, "unresolved", "legacy DEFAULT applies through the trigger");
    assert_eq!(start_line, 0, "legacy DEFAULT applies through the trigger");

    // Shared strings appear once: from_name, edge_kind, confidence, resolution are common.
    let shared: i64 = h
        .conn
        .query_row(
            "SELECT COUNT(*) FROM name_strings WHERE value IN ('a.rs::caller', 'calls_name', \
             'NameOnly', 'unresolved')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(shared, 4, "each shared string interned exactly once across both edges");

    // UPDATE through the view rewrites the row (the maintenance/migration path).
    h.conn
        .execute("UPDATE edges SET confidence = 'Syntactic' WHERE to_name = 'target'", [])
        .unwrap();
    let confidence: String = h
        .conn
        .query_row("SELECT confidence FROM edges WHERE to_name = 'target'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(confidence, "Syntactic");

    // DELETE through the view removes the backing row.
    h.conn.execute("DELETE FROM edges WHERE to_name = 'helper'", []).unwrap();
    let remaining: i64 =
        h.conn.query_row("SELECT COUNT(*) FROM edges_data", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 1);
}

/// V020 conversion: a legacy `edges` TABLE (pre-interning shape, with rows) converts into the
/// dictionary + `edges_data` behind the view, byte-equal through the view, ids preserved, and the
/// `edge_oracle` FK re-pointed so verdict cascade still fires.
#[test]
fn v020_converts_a_legacy_edges_table() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    // Recreate the LEGACY world: drop the view shape and install a real old-format table with a
    // row, plus an edge_oracle row referencing it.
    conn.execute_batch(
        "
        DROP TRIGGER edges_view_insert;
        DROP TRIGGER edges_view_update;
        DROP TRIGGER edges_view_delete;
        DROP VIEW edges;
        DELETE FROM edges_data;
        DELETE FROM name_strings;
        CREATE TABLE edges(
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
            callee_start_byte INTEGER,
            callee_end_byte INTEGER,
            edge_kind TEXT NOT NULL,
            confidence TEXT NOT NULL
        );
        DROP TABLE edge_oracle;
        CREATE TABLE edge_oracle(
            edge_id INTEGER NOT NULL,
            file_sha TEXT NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            resolved_symbol_id INTEGER,
            scip_symbol TEXT NOT NULL,
            kind TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(edge_id, tool, tool_version),
            FOREIGN KEY(edge_id) REFERENCES edges(id) ON DELETE CASCADE
        ) STRICT;
        INSERT INTO edges(id, to_name, from_name, edge_kind, confidence, resolution, evidence)
        VALUES (7, 'target', 'caller', 'calls_name', 'Syntactic', 'qualified_suffix', 'target()');
        INSERT INTO edge_oracle(edge_id, file_sha, tool, tool_version, resolved_symbol_id, \
         scip_symbol, kind, computed_at)
        VALUES (7, 'sha', 'rust-analyzer', 'v1', NULL, 'sym', 'upgrade', 0);
        ",
    )
    .unwrap();

    schema::migrations::apply_edge_string_interning(&conn).unwrap();

    let (to_name, evidence, resolution): (String, String, String) = conn
        .query_row("SELECT to_name, evidence, resolution FROM edges WHERE id = 7", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(
        (to_name.as_str(), evidence.as_str(), resolution.as_str()),
        ("target", "target()", "qualified_suffix")
    );
    let object_type: String = conn
        .query_row("SELECT type FROM sqlite_master WHERE name = 'edges'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(object_type, "view", "legacy table converted to the view shape");

    // The re-pointed FK still cascades verdicts away with their edge.
    conn.execute("DELETE FROM edges_data WHERE id = 7", []).unwrap();
    let verdicts: i64 =
        conn.query_row("SELECT COUNT(*) FROM edge_oracle", [], |r| r.get(0)).unwrap();
    assert_eq!(verdicts, 0, "edge_oracle FK was re-pointed at edges_data");
}

/// The query_warm regression (#79 follow-up): an OR-branch string equality on a dictionary
/// column cannot be transformed by the planner through the view's value joins — it silently
/// picks a non-selective index (`to_symbol_id IS NULL` scans most of the table) instead of
/// `idx_edges_to_name`. Hot readers therefore compare the view's exposed `to_name_id` against a
/// constant dictionary-lookup subquery. This pins the PLAN: the caller-count predicate shape
/// must drive the to_name int index, and the legacy string form must never silently return.
#[test]
fn or_branch_name_predicates_use_the_to_name_index() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();

    let plan = |sql: &str| -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.join("\n")
    };

    // The production count_callers / callers() / traversal predicate shape.
    let fixed = plan(
        "SELECT COUNT(*) FROM edges WHERE edge_kind IN ('calls_name','constructs','uses_macro') \
         AND (to_symbol_id = 5 OR (to_symbol_id IS NULL AND to_name_id = (SELECT id FROM \
         name_strings WHERE value = 'x')))",
    );
    assert!(
        fixed.contains("idx_edges_to_name"),
        "the OR's name branch must drive the to_name int index, got plan:\n{fixed}"
    );

    // Simple equality through the view still transforms on its own (no subquery needed).
    let simple = plan("SELECT id FROM edges WHERE to_name = 'x'");
    assert!(
        simple.contains("idx_edges_to_name"),
        "plain to_name equality must use the int index, got plan:\n{simple}"
    );
}

/// #682: the graph-traversal SEED predicate (behind `find_callers` / `trace_callees`) must drive the
/// `edges_data` id indexes, not full-scan the ~1M-row edge table. Before the fix the Syntactic seed
/// OR mixed the indexed `to_symbol_id` with value-joined columns (`to_qn.value`,
/// `edges.target_qualified_name`), so the planner abandoned the indexes and scanned every edge
/// (~1.8-2.2s/call on a real index). The fix compares the view's raw id columns against a constant,
/// exactly like the caller-count predicate above — this pins that the reverse seed drives
/// `idx_edges_to_symbol` + `idx_edges_target_qname` and the forward seed `idx_edges_from_symbol` +
/// `idx_edges_from_name`, and that NO full `SCAN` of the edge rows (`SCAN d` / `SCAN edges_data`)
/// survives. The `idx_edges_target_qname` index (V071) is required for the reverse unresolved
/// branch.
#[test]
fn graph_traversal_seed_predicates_use_edge_id_indexes() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();

    let plan = |sql: &str| -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
    };

    // The V071 index the reverse unresolved-edge branch needs actually exists on edges_data.
    let idx_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_edges_target_qname' AND tbl_name = 'edges_data'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx_count, 1, "V071 idx_edges_target_qname on edges_data");

    // Reverse (find_callers) Syntactic seed shape — mirror of predicates::reverse_predicate.
    let reverse = plan(
        "SELECT id FROM edges
         WHERE edge_kind IN ('calls_name', 'constructs')
           AND (to_symbol_id = 5
                OR to_symbol_id IN (
                   SELECT id FROM symbols
                   WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = 'x'))
                OR ('true' = 'true' AND to_symbol_id IN (SELECT id FROM symbols WHERE name = 'y'))
                OR target_qualified_name_id = (SELECT id FROM name_strings WHERE value = 'x'))",
    );
    assert!(
        reverse.contains("idx_edges_to_symbol"),
        "reverse seed must drive idx_edges_to_symbol, got plan:\n{reverse}"
    );
    assert!(
        reverse.contains("idx_edges_target_qname"),
        "reverse unresolved branch must drive idx_edges_target_qname (V071), got plan:\n{reverse}"
    );
    assert!(
        !reverse.contains("SCAN d") && !reverse.contains("SCAN edges_data"),
        "reverse seed must not full-scan the edge rows, got plan:\n{reverse}"
    );

    // Forward (trace_callees) Syntactic seed shape — mirror of
    // predicates::forward_source_predicate.
    let forward = plan(
        "SELECT id FROM edges
         WHERE edge_kind IN ('calls_name', 'constructs')
           AND (from_symbol_id = 5
                OR from_symbol_id IN (
                   SELECT id FROM symbols
                   WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = 'x'))
                OR ('true' = 'true' AND from_symbol_id IN (SELECT id FROM symbols WHERE name = \
         'y'))
                OR from_name_id = (SELECT id FROM name_strings WHERE value = 'x'))",
    );
    assert!(
        forward.contains("idx_edges_from_symbol"),
        "forward seed must drive idx_edges_from_symbol, got plan:\n{forward}"
    );
    assert!(
        forward.contains("idx_edges_from_name"),
        "forward unresolved branch must drive idx_edges_from_name, got plan:\n{forward}"
    );
    assert!(
        !forward.contains("SCAN d") && !forward.contains("SCAN edges_data"),
        "forward seed must not full-scan the edge rows, got plan:\n{forward}"
    );
}

/// #692: the same indexed-seed contract for the other hot edges-view readers #682 did not touch —
/// `grep_augment::edge_counts`' caller-count (runs on every grep-augmented hit) and
/// `impact_surface`'s Syntactic/Fuzzy neighbor predicates. Each seed must drive the edge id
/// indexes, never full-scan `edges_data` through the view's value joins.
#[test]
fn impact_and_grep_augment_seeds_use_edge_id_indexes() {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();

    let plan = |sql: &str| -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
    };

    // grep_augment::edge_counts caller-count shape (reverse: to_symbol_id OR
    // target_qualified_name_id).
    let callers = plan(
        "SELECT COUNT(*) FROM edges JOIN files source_files ON source_files.id = \
         edges.source_file_id
         WHERE edges.to_symbol_id = 5
            OR edges.target_qualified_name_id = (SELECT id FROM name_strings WHERE value = 'x')",
    );
    assert!(
        callers.contains("idx_edges_to_symbol") && callers.contains("idx_edges_target_qname"),
        "grep_augment caller-count must drive the edge id indexes, got:\n{callers}"
    );
    assert!(
        !callers.contains("SCAN d") && !callers.contains("SCAN edges_data"),
        "grep_augment caller-count must not full-scan the edge rows, got:\n{callers}"
    );

    // impact_surface forward Syntactic/Fuzzy neighbor shape (from_symbol_id OR from_name_id).
    let fwd = plan(
        "SELECT id FROM edges
         WHERE edges.from_symbol_id = 5
            OR edges.from_name_id = (SELECT id FROM name_strings WHERE value = 'x')",
    );
    assert!(
        fwd.contains("idx_edges_from_symbol") && fwd.contains("idx_edges_from_name"),
        "impact forward neighbor seed must drive the edge id indexes, got:\n{fwd}"
    );
    assert!(
        !fwd.contains("SCAN d") && !fwd.contains("SCAN edges_data"),
        "impact forward neighbor seed must not full-scan the edge rows, got:\n{fwd}"
    );
}
