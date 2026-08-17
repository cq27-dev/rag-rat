use super::*;

#[test]
fn migration_073_builds_the_distill_substrate() {
    // The absolute-tip pin moved to `migration_074_*` (V074 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // The closing-edge table exists with the full column set, repo_id included from birth.
    let cols = conn_table_columns(&conn, "papertrail_closing_edges");
    for col in [
        "tracker",
        "project",
        "issue_kind",
        "issue_key",
        "closer_kind",
        "closer_key",
        "closer_commit",
        "source",
        "synced_at_ms",
        "repo_id",
    ] {
        assert!(cols.contains(&col.to_string()), "V073 closing-edge column `{col}` exists");
    }
    // The natural key is the (issue, closer) PAIR — `source` is an attribute, so the same edge
    // discovered by the text tier and then the provider tier converges to one row instead of two.
    conn.execute(
        "INSERT INTO papertrail_closing_edges(tracker, project, issue_kind, issue_key, \
         closer_kind, closer_key, source, synced_at_ms, repo_id) VALUES ('github', 'o/r', \
         'issue', '5', 'change_request', '9', 'text', 1, 'r')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO papertrail_closing_edges(tracker, project, issue_kind, issue_key, \
             closer_kind, closer_key, source, synced_at_ms, repo_id) VALUES ('github', 'o/r', \
             'issue', '5', 'change_request', '9', 'provider', 2, 'r')",
            [],
        )
        .is_err(),
        "the natural key rejects a second row for the same (issue, closer) pair",
    );
    // A DIFFERENT closer for the same issue is a distinct edge (an issue can be closed by a
    // change request AND referenced by the closing commit).
    conn.execute(
        "INSERT INTO papertrail_closing_edges(tracker, project, issue_kind, issue_key, \
         closer_kind, closer_key, source, synced_at_ms, repo_id) VALUES ('github', 'o/r', \
         'issue', '5', 'commit', 'abc123', 'text', 1, 'r')",
        [],
    )
    .unwrap();

    // The item outcome columns exist on items; the author facets on comments too.
    let item_cols = conn_table_columns(&conn, "papertrail_items");
    for col in [
        "closed_at",
        "resolution",
        "merge_commit_sha",
        "state_normalized",
        "author_kind",
        "author_association",
    ] {
        assert!(item_cols.contains(&col.to_string()), "V073 item column `{col}` exists");
    }
    let comment_cols = conn_table_columns(&conn, "papertrail_comments");
    for col in ["author_kind", "author_association"] {
        assert!(comment_cols.contains(&col.to_string()), "V073 comment column `{col}` exists");
    }
}

#[test]
fn migration_073_backfills_state_normalized_from_the_provider_truthful_pair() {
    // The trap this column exists for: GitLab merged MRs carry state='merged', GitHub merged PRs
    // carry state='closed' + merged_at — a consumer filtering raw `WHERE state='closed'` silently
    // drops every merged GitLab MR. The backfill derives the normalized value for pre-V073 rows;
    // rerunning it is a no-op for already-stamped rows (idempotent replay).
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    let insert = |key: &str, state: &str, merged_at: Option<&str>| {
        conn.execute(
            "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, \
             title, body, merged_at, synced_at_ms, repo_id, state_normalized) VALUES ('github', \
             'o/r', 'change_request', ?1, 'u', ?2, 't', 'b', ?3, 1, 'r', '')",
            rusqlite::params![key, state, merged_at],
        )
        .unwrap();
    };
    insert("1", "closed", Some("2026-01-03T00:00:00Z")); // GitHub merged PR
    insert("2", "merged", None); // GitLab merged MR
    insert("3", "closed", None); // closed unmerged
    insert("4", "open", None);
    rag_rat_db::schema::migrations::apply_papertrail_distill_substrate(&conn).unwrap();
    let normalized = |key: &str| -> String {
        conn.query_row(
            "SELECT state_normalized FROM papertrail_items WHERE item_key = ?1",
            [key],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(normalized("1"), "merged", "merged_at wins over raw closed state");
    assert_eq!(normalized("2"), "merged", "GitLab's state='merged' normalizes to merged");
    assert_eq!(normalized("3"), "closed");
    assert_eq!(normalized("4"), "open");
    // Idempotent: a stamped row is untouched by a replay (the WHERE '' predicate skips it).
    conn.execute("UPDATE papertrail_items SET state_normalized = 'closed' WHERE item_key = '4'", [
    ])
    .unwrap();
    rag_rat_db::schema::migrations::apply_papertrail_distill_substrate(&conn).unwrap();
    assert_eq!(normalized("4"), "closed", "replay does not re-derive a stamped row");
}

#[test]
fn migration_074_refreshes_the_edges_view() {
    // The absolute-tip pin moved to `migration_076_*` (V076 is the tip now); this drops to the
    // symbolic `current_version == LATEST` freshness check, per the ladder convention.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // A DB that migrated through V068-V073 carries the ORIGINAL view text (per-row
    // `NOT IN (SELECT ...)` suppressed-edge probe). Simulate it: swap the current materialized
    // WHERE back to the historical inline predicates, truncate the ledger to V073, and seed one
    // suppressed candidate.
    let current_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let old_view = current_view.replace(
        "WHERE d.hidden = 0",
        "WHERE d.edge_kind_id NOT IN (
            SELECT id FROM name_strings WHERE value IN ('dispatch_construct', 'dispatch_handle')
        )
        AND d.resolution_id NOT IN (
            SELECT id FROM name_strings WHERE value = 'suppressed'
        )",
    );
    assert_ne!(old_view, current_view, "fixture must reconstruct the pre-V074 clause");
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
    truncate_schema_to(&conn, 73);
    conn.execute_batch("DROP VIEW edges;").unwrap();
    conn.execute_batch(&old_view).unwrap();

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    let refreshed_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // The V074 refresh runs inside the same forward pass as V075, so the re-installed view is
    // the current shape: the materialized flag, no inline membership probes.
    assert!(
        refreshed_view.contains("WHERE d.hidden = 0"),
        "the forward pass re-installs the materialized-visibility view: {refreshed_view}"
    );
    // Semantics preserved across the refresh: the suppressed candidate stays in edges_data for
    // the resolver but never surfaces through the compatibility view.
    let raw_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges_data", [], |row| row.get(0)).unwrap();
    let visible_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0)).unwrap();
    assert_eq!(raw_count, 1, "suppressed candidates remain available to the resolver");
    assert_eq!(visible_count, 0, "suppressed candidates stay out of query-layer reads");
    let v74_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '074_edges_view_scalar_suppression'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v74_recorded, 1, "the forward migration records V074");
}

#[test]
fn migration_075_materializes_edge_visibility() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id) VALUES ('App.swift', 'swift', 'source', 'sha', 0, 0, 'head', '')",
        [],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    // One row per visibility class: a suppressed candidate, an internal dispatch FACT, and a
    // public edge.
    for (to_name, edge_kind, resolution) in [
        ("Observable", "uses_macro", "suppressed"),
        ("Msg::Start", "dispatch_construct", "unresolved"),
        ("spawn_worker", "calls_name", "unresolved"),
    ] {
        conn.execute(
            "INSERT INTO edges(source_file_id, to_name, edge_kind, confidence, resolution) VALUES \
             (?1, ?2, ?3, 'NameOnly', ?4)",
            rusqlite::params![file_id, to_name, edge_kind, resolution],
        )
        .unwrap();
    }

    // Simulate a pre-V075 DB: rows never carried the flag (zero it under the writers' backs) and
    // the view still evaluates the V074 scalar clause inline. The backfill — not the insert
    // trigger that already stamped these rows — must be what restores visibility.
    conn.execute("UPDATE edges_data SET hidden = 0", []).unwrap();
    let current_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let v74_view = current_view.replace(
        "WHERE d.hidden = 0",
        "WHERE d.edge_kind_id NOT IN (
            SELECT id FROM name_strings WHERE value IN ('dispatch_construct', 'dispatch_handle')
        )
        AND d.resolution_id <> COALESCE(
            (SELECT id FROM name_strings WHERE value = 'suppressed'), -1
        )",
    );
    assert_ne!(v74_view, current_view, "fixture must reconstruct the V074 view shape");
    truncate_schema_to(&conn, 74);
    conn.execute_batch("DROP VIEW edges;").unwrap();
    conn.execute_batch(&v74_view).unwrap();

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

    // The backfill re-derives the flag from the kind/resolution predicates.
    let hidden_for = |to_name: &str| -> i64 {
        conn.query_row(
            "SELECT hidden FROM edges_data
             WHERE to_name_id = (SELECT id FROM name_strings WHERE value = ?1)",
            [to_name],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(hidden_for("Observable"), 1, "suppressed candidates backfill hidden");
    assert_eq!(hidden_for("Msg::Start"), 1, "dispatch FACT rows backfill hidden");
    assert_eq!(hidden_for("spawn_worker"), 0, "public edges stay visible");

    // The refreshed view filters on the flag alone — the per-row kind/resolution machinery is
    // gone from the read path (the point of the migration).
    let refreshed_view: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'edges'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        refreshed_view.contains("WHERE d.hidden = 0"),
        "V075 installs the materialized-visibility filter: {refreshed_view}"
    );
    let visible: Vec<String> = conn
        .prepare("SELECT to_name FROM edges ORDER BY to_name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(visible, ["spawn_worker"], "only the public edge surfaces through the view");
    let v75_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '075_edges_hidden_flag'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v75_recorded, 1, "the forward migration records V075");
}

#[test]
fn migration_076_adds_sync_security_events() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    // Simulate a pre-V076 DB: the table's DDL is not part of truncate_schema_to (it rolls the
    // ledger only), so drop it, then roll the ledger back to V075.
    truncate_schema_to(&conn, 75);
    conn.execute_batch("DROP TABLE sync_security_events;").unwrap();

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();

    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = \
             'sync_security_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_exists, 1, "the forward migration re-creates sync_security_events");
    let dedup_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = \
             'sync_security_events_dedup'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dedup_index, 1, "the dedup unique index is installed");
    let v76_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '076_sync_security_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v76_recorded, 1, "the forward migration records V076");
}

#[test]
fn migration_077_builds_the_distill_record_store() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(
        schema::status(&conn).unwrap().current_version,
        schema::LATEST_SCHEMA_VERSION,
        "schema at LATEST after apply",
    );

    // The record row carries the derived-facet columns + the raw status floors; repo_id from birth.
    let cols = conn_table_columns(&conn, "papertrail_distill");
    for col in [
        "tracker",
        "project",
        "item_kind",
        "item_key",
        "distill_input_hash",
        "pipeline_version",
        "prompt_version",
        "model_input_hash",
        "root_issue",
        "root_cause",
        "root_cause_class",
        "decision_chosen",
        "outcome_summary",
        "outcome_status_model",
        "epistemic_status_decision",
        "epistemic_status_outcome",
        "fix_edge_source",
        "quotes_materialized",
        "anchors_qualified_count",
        "thread_shape",
        "outcome_claim_verified",
        "decision_provenance_verified",
        "revert_override",
        "closing_keyword_floor",
        "distilled_at_ms",
        "repo_id",
    ] {
        assert!(cols.contains(&col.to_string()), "V077 distill column `{col}` exists");
    }

    // The record is keyed to the coalesced work-unit thread: one row per
    // (repo_id, tracker, project, item_kind, item_key). A regenerated body replaces in place.
    conn.execute(
        "INSERT INTO papertrail_distill(tracker, project, item_kind, item_key, \
         distill_input_hash, pipeline_version, fix_edge_source, thread_shape, distilled_at_ms, \
         repo_id) VALUES ('github', 'o/r', 'issue', '5', 'h1', 1, 'provider', 'investigation', 1, \
         'r')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO papertrail_distill(tracker, project, item_kind, item_key, \
             distill_input_hash, pipeline_version, fix_edge_source, thread_shape, \
             distilled_at_ms, repo_id) VALUES ('github', 'o/r', 'issue', '5', 'h2', 2, 'text', \
             'thin', 2, 'r')",
            [],
        )
        .is_err(),
        "the natural key holds one record per coalesced thread",
    );

    // The junction/companion tables all land in the same migration.
    for table in [
        "papertrail_distill_evidence",
        "papertrail_distill_anchors",
        "papertrail_distill_alternatives",
        "papertrail_distill_record_commits",
        "papertrail_distill_edges",
        "papertrail_distill_queue",
        "papertrail_distill_runs",
    ] {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "V077 companion table `{table}` exists");
    }

    let v77_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '077_distill_record_store'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v77_recorded, 1, "the forward migration records V077");
}

#[test]
fn migration_078_distinguishes_candidates_from_selections() {
    #[derive(Debug, PartialEq, Eq)]
    struct UpgradedAnchor {
        item_key: String,
        candidate_ordinal: i64,
        selected: i64,
        logical_symbol_id: Option<String>,
        file_path: Option<String>,
    }

    // Exercise the real V077 -> V078 upgrade with existing rows. Row-id order is the deterministic
    // tie-break within each thread; a second thread starts its own zero-based ordinal sequence.
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_distill_record_store(&legacy).unwrap();
    legacy
        .execute_batch(
            "
            INSERT INTO papertrail_distill_anchors
                (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, file_path,
                 name, resolved, repo_id)
            VALUES
                ('github', 'o/r', 'issue', '5', 'file', NULL, 'src/widget.rs',
                 'src/widget.rs', 1, 'r'),
                ('github', 'o/r', 'issue', '5', 'symbol', 'sym_3e7', 'src/widget.rs',
                 'render_widget', 1, 'r'),
                ('github', 'o/r', 'issue', '6', 'file', NULL, 'src/other.rs',
                 'src/other.rs', 1, 'r');
            ",
        )
        .unwrap();
    // Simulate a crash after V078's first ADD COLUMN but before its backfill/index. The migration
    // must recognize the missing completion index and reconverge rather than keep three ordinal-0
    // rows and fail unique-index creation forever.
    legacy
        .execute_batch(
            "ALTER TABLE papertrail_distill_anchors ADD COLUMN candidate_ordinal
                 INTEGER NOT NULL DEFAULT 0 CHECK(candidate_ordinal >= 0);",
        )
        .unwrap();
    schema::migrations::apply_distill_anchor_selection(&legacy).unwrap();

    let upgraded: Vec<UpgradedAnchor> = legacy
        .prepare(
            "SELECT item_key, candidate_ordinal, selected, logical_symbol_id, file_path
             FROM papertrail_distill_anchors ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok(UpgradedAnchor {
                item_key: row.get(0)?,
                candidate_ordinal: row.get(1)?,
                selected: row.get(2)?,
                logical_symbol_id: row.get(3)?,
                file_path: row.get(4)?,
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        upgraded,
        vec![
            UpgradedAnchor {
                item_key: "5".into(),
                candidate_ordinal: 0,
                selected: 0,
                logical_symbol_id: None,
                file_path: Some("src/widget.rs".into()),
            },
            UpgradedAnchor {
                item_key: "5".into(),
                candidate_ordinal: 1,
                selected: 0,
                logical_symbol_id: Some("sym_3e7".into()),
                file_path: Some("src/widget.rs".into()),
            },
            UpgradedAnchor {
                item_key: "6".into(),
                candidate_ordinal: 0,
                selected: 0,
                logical_symbol_id: None,
                file_path: Some("src/other.rs".into()),
            },
        ],
        "backfill is per-thread, deterministic, unselected, and preserves exact anchors",
    );

    assert!(
        legacy
            .execute_batch(
                "UPDATE papertrail_distill_anchors SET candidate_ordinal = -1 WHERE id = 1",
            )
            .is_err(),
        "candidate ordinals are non-negative",
    );
    assert!(
        legacy
            .execute("UPDATE papertrail_distill_anchors SET selected = 2 WHERE id = 1", [])
            .is_err(),
        "selected is a checked SQLite boolean",
    );
    assert!(
        legacy
            .execute(
                "UPDATE papertrail_distill_anchors SET candidate_ordinal = 0 WHERE id = 2",
                [],
            )
            .is_err(),
        "candidate ordinals are unique within a thread",
    );
    legacy.execute("UPDATE papertrail_distill_anchors SET selected = 1 WHERE id = 2", []).unwrap();
    schema::migrations::apply_distill_anchor_selection(&legacy).unwrap();
    let selected: i64 = legacy
        .query_row("SELECT selected FROM papertrail_distill_anchors WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(selected, 1, "replaying V078 does not erase a later model selection");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let v78_recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '078_distill_anchor_selection'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v78_recorded, 1, "the forward migration records V078");
    for index in
        ["idx_papertrail_distill_anchors_candidate", "idx_papertrail_distill_anchors_selected"]
    {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "V078 index `{index}` exists");
    }
}

#[test]
fn migration_079_builds_safe_input_snapshots() {
    // `LATEST_SCHEMA_VERSION` pin moved to `migration_083_*`, the new tip; this uses only the
    // symbolic checks (the hardcoded-LATEST footgun).
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_distill_record_store(&legacy).unwrap();
    schema::migrations::apply_distill_anchor_selection(&legacy).unwrap();
    assert!(!conn_table_columns(&legacy, "papertrail_distill").contains(&"prompt_version".into()));
    assert!(!schema::table_exists(&legacy, "papertrail_distill_sources").unwrap());

    schema::migrations::apply_distill_safe_input_snapshot(&legacy).unwrap();
    let distill_columns = conn_table_columns(&legacy, "papertrail_distill");
    assert!(distill_columns.contains(&"prompt_version".into()));
    assert!(distill_columns.contains(&"model_input_hash".into()));
    for table in ["papertrail_distill_sources", "papertrail_distill_units"] {
        assert!(schema::table_exists(&legacy, table).unwrap(), "V079 table `{table}` exists");
    }

    legacy
        .execute(
            "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, repo_id)
             VALUES ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','title','5',
                     'same','repoA')",
            [],
        )
        .unwrap();
    legacy
        .execute(
            "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, repo_id)
             VALUES ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','title','5',
                     'same','repoB')",
            [],
        )
        .unwrap();
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_sources
                     (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                      source_item_kind, source_item_key, source_kind, source_part, source_id,
                      exact_text, repo_id)
                 VALUES ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','body','5',
                         'same','repoA')",
                [],
            )
            .is_err(),
        "source ordinals are unique only inside the full repo-scoped record identity",
    );
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_sources
                     (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                      source_item_kind, source_item_key, source_kind, source_part, source_id,
                      exact_text, repo_id)
                 VALUES ('github','o/r','issue','5',1,'partner',NULL,'change_request','6','item',
                         'body','6','x','repoA')",
                [],
            )
            .is_err(),
        "partner sources require a partner ordinal",
    );
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_units
                     (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal,
                      byte_start, byte_end, repo_id)
                 VALUES ('github','o/r','issue','5',0,0,4,4,'repoA')",
                [],
            )
            .is_err(),
        "unit spans are non-empty half-open byte ranges",
    );

    // Torn replay: both columns and the source table survived, while the unit table/index did not.
    // The additive applier must converge without touching existing source rows.
    legacy.execute_batch("DROP TABLE papertrail_distill_units;").unwrap();
    schema::migrations::apply_distill_safe_input_snapshot(&legacy).unwrap();
    assert!(schema::table_exists(&legacy, "papertrail_distill_units").unwrap());
    let sources: i64 = legacy
        .query_row("SELECT COUNT(*) FROM papertrail_distill_sources", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sources, 2, "replay preserves existing snapshots");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '079_distill_safe_input_snapshot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V079");
}

#[test]
fn migration_080_builds_enriched_context_snapshots() {
    // The `LATEST_SCHEMA_VERSION` pin lives on `migration_083_*`, the current tip; this uses
    // only the symbolic checks (the hardcoded-LATEST footgun).
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_distill_record_store(&legacy).unwrap();
    schema::migrations::apply_distill_anchor_selection(&legacy).unwrap();
    schema::migrations::apply_distill_safe_input_snapshot(&legacy).unwrap();
    assert!(!schema::table_exists(&legacy, "papertrail_distill_fix_diffs").unwrap());
    assert!(!schema::table_exists(&legacy, "papertrail_distill_xrefs").unwrap());

    schema::migrations::apply_distill_enriched_context(&legacy).unwrap();
    for table in ["papertrail_distill_fix_diffs", "papertrail_distill_xrefs"] {
        assert!(schema::table_exists(&legacy, table).unwrap(), "V080 table `{table}` exists");
    }

    legacy
        .execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github','o/r','issue','5','abc123','src/lib.rs','patch A','repoA')",
            [],
        )
        .unwrap();
    legacy
        .execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github','o/r','issue','5','abc123','src/lib.rs','patch B','repoB')",
            [],
        )
        .unwrap();
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_fix_diffs
                     (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
                 VALUES ('github','o/r','issue','5','abc123','src/lib.rs','dup','repoA')",
                [],
            )
            .is_err(),
        "one patch row per (record, commit, path) inside the full repo-scoped record identity",
    );
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_xrefs
                     (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                      target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                      repo_id)
                 VALUES ('github','o/r','issue','5',-1,'github','o/r','issue','9','reference',
                         't','o','repoA')",
                [],
            )
            .is_err(),
        "xref ordinals are non-negative",
    );
    legacy
        .execute(
            "INSERT INTO papertrail_distill_xrefs
                 (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                  target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                  repo_id)
             VALUES ('github','o/r','issue','5',0,'github','o/r','issue','9','reference','t','o',
                     'repoA')",
            [],
        )
        .unwrap();
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_xrefs
                     (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                      target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                      repo_id)
                 VALUES ('github','o/r','issue','5',0,'github','o/r','issue','10','reference',
                         't','o','repoA')",
                [],
            )
            .is_err(),
        "xref ordinals are unique within a record",
    );

    // Torn replay: the diff table survived, the xref table did not. The additive applier must
    // converge without touching existing rows.
    legacy.execute_batch("DROP TABLE papertrail_distill_xrefs;").unwrap();
    schema::migrations::apply_distill_enriched_context(&legacy).unwrap();
    assert!(schema::table_exists(&legacy, "papertrail_distill_xrefs").unwrap());
    let diffs: i64 = legacy
        .query_row("SELECT COUNT(*) FROM papertrail_distill_fix_diffs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(diffs, 2, "replay preserves existing diff snapshots");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '080_distill_enriched_context'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V080");
}

#[test]
fn migration_081_adds_evidence_source_part() {
    // The `LATEST_SCHEMA_VERSION` pin lives on `migration_083_*`, the current tip.

    // The evidence table predates the column: build the record store (V077) without V081, then
    // apply V081 and confirm the column appears.
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrations::apply_distill_record_store(&legacy).unwrap();
    assert!(
        !schema::column_exists(&legacy, "papertrail_distill_evidence", "source_part").unwrap(),
        "V077 evidence has no source_part",
    );

    schema::migrations::apply_distill_evidence_source_part(&legacy).unwrap();
    assert!(
        schema::column_exists(&legacy, "papertrail_distill_evidence", "source_part").unwrap(),
        "V081 adds the source_part column",
    );

    // A pre-V081 row is representable with NULL source_part (the value CHECK passes NULL).
    legacy
        .execute(
            "INSERT INTO papertrail_distill_evidence
                 (tracker, project, item_kind, item_key, field, source_kind, source_id,
                  byte_start, byte_end, quote, repo_id)
             VALUES ('github','o/r','issue','5','root_cause','item','5',0,4,'quot','repoA')",
            [],
        )
        .unwrap();
    // A new row must carry one of title|body|comment.
    legacy
        .execute(
            "INSERT INTO papertrail_distill_evidence
                 (tracker, project, item_kind, item_key, field, source_kind, source_part,
                  source_id, byte_start, byte_end, quote, repo_id)
             VALUES ('github','o/r','issue','5','decision','item','title','5',0,4,'quot','repoA')",
            [],
        )
        .unwrap();
    assert!(
        legacy
            .execute(
                "INSERT INTO papertrail_distill_evidence
                     (tracker, project, item_kind, item_key, field, source_kind, source_part,
                      source_id, byte_start, byte_end, quote, repo_id)
                 VALUES ('github','o/r','issue','5','decision','item','headline','5',0,4,'q',
                         'repoA')",
                [],
            )
            .is_err(),
        "source_part is CHECK-constrained to title|body|comment",
    );

    // Torn replay: the column already exists, so re-applying is a no-op, not a duplicate-column
    // error, and existing rows survive.
    schema::migrations::apply_distill_evidence_source_part(&legacy).unwrap();
    let rows: i64 = legacy
        .query_row("SELECT COUNT(*) FROM papertrail_distill_evidence", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2, "an idempotent re-apply preserves existing evidence rows");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '081_distill_evidence_source_part'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V081");
}

#[test]
fn migration_082_accounts_for_content_refold_work() {
    // The `LATEST_SCHEMA_VERSION` pin lives on `migration_084_*`, the current tip.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    let insert_candidate =
        |conn: &rusqlite::Connection, hash: u8, stream: u8, signed_bytes: &[u8], received_at_ms| {
            conn.execute(
                "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                     grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                     received_at_ms)
                 VALUES(?1, ?2, zeroblob(32), zeroblob(32), zeroblob(8), NULL, NULL, zeroblob(32),
                        zeroblob(8), zeroblob(8), 0, ?3, ?4)",
                rusqlite::params![vec![hash; 32], vec![stream; 32], signed_bytes, received_at_ms],
            )
        };

    // Reconstruct a pre-V082 index: retain the V081 distill migrations and their data, but
    // restore the V072 queue shape and remove every V082 object, then replay forward from V081
    // (V082 does the content-refold work; later tips are orthogonal to it).
    conn.execute_batch(
        "DROP TRIGGER content_stream_stats_after_insert;
         DROP TRIGGER content_stream_stats_after_delete;
         DROP TRIGGER content_stream_stats_after_update;
         DROP TABLE content_stream_stats;
         DROP INDEX content_streams_pending_refold_order;
         DROP TABLE content_streams_pending_refold;
         CREATE TABLE content_streams_pending_refold(
             stream_id BLOB PRIMARY KEY CHECK(length(stream_id) = 32)
         ) STRICT;",
    )
    .unwrap();
    insert_candidate(&conn, 1, 0x11, b"abc", 300).unwrap();
    insert_candidate(&conn, 2, 0x11, b"12345", 100).unwrap();
    insert_candidate(&conn, 3, 0x22, b"payload", 200).unwrap();
    conn.execute(
        "INSERT INTO content_streams_pending_refold(stream_id) VALUES (?1), (?2)",
        rusqlite::params![vec![0x11u8; 32], vec![0x33u8; 32]],
    )
    .unwrap();
    truncate_schema_to(&conn, 81);

    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    // Replaying from 81 runs V082 and everything after it, so pin the TIP rather than a literal —
    // this assertion is about the forward path completing, not about which migration is last.
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);

    let queued: Vec<(Vec<u8>, i64, i64, i64)> = conn
        .prepare(
            "SELECT stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms
             FROM content_streams_pending_refold ORDER BY stream_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // Stream 0x22 holds content but had no legacy queue row: V082 leaves it alone, and the later
    // V113 (replayed in the same forward pass) queues every content-bearing stream for the
    // lamport-clamp re-judgment — so it appears here with V113's zero timestamps.
    assert_eq!(
        queued,
        vec![
            (vec![0x11u8; 32], 1, 100, 300),
            (vec![0x22u8; 32], 1, 0, 0),
            (vec![0x33u8; 32], 1, 0, 0)
        ],
        "legacy rows become content-candidate work with deterministic source-derived times",
    );

    let stats: Vec<(Vec<u8>, i64, i64)> = conn
        .prepare(
            "SELECT stream_id, candidate_count, candidate_bytes
             FROM content_stream_stats ORDER BY stream_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        stats,
        vec![(vec![0x11u8; 32], 2, 72), (vec![0x22u8; 32], 1, 39)],
        "candidate_bytes is signed_bytes plus the separately loaded 32-byte entry hash per row",
    );

    let ordered_index: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master
              WHERE type = 'index' AND name = 'content_streams_pending_refold_order'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ordered_index, 1, "ordered pending selection has its composite index");

    // Insert, ignored duplicate, failed duplicate, signed-byte update, stream move, and deletes all
    // flow through database-owned accounting. Removing the final candidate removes the sparse stats
    // row rather than retaining a zero-work stream.
    insert_candidate(&conn, 4, 0x22, b"xx", 400).unwrap();
    let duplicate_ignored = conn
        .execute(
            "INSERT OR IGNORE INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, roster_ref,
                 owner_auth_len, author_auth_len, signed_bytes, received_at_ms)
             SELECT entry_hash, ?1, author_account_id, device_fingerprint, seq, roster_ref,
                    owner_auth_len, author_auth_len, x'00', received_at_ms
             FROM content_entries WHERE entry_hash = ?2",
            rusqlite::params![vec![0x44u8; 32], vec![4u8; 32]],
        )
        .unwrap();
    assert_eq!(duplicate_ignored, 0);
    assert!(insert_candidate(&conn, 4, 0x44, b"different", 401).is_err());
    conn.execute(
        "UPDATE content_entries SET signed_bytes = ?1, stream_id = ?2 WHERE entry_hash = ?3",
        rusqlite::params![b"updated", vec![0x44u8; 32], vec![4u8; 32]],
    )
    .unwrap();
    conn.execute("DELETE FROM content_entries WHERE stream_id = ?1", [vec![0x44u8; 32]]).unwrap();
    let stream_44_stats: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_stream_stats WHERE stream_id = ?1",
            [vec![0x44u8; 32]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stream_44_stats, 0, "last-row deletion removes the stats row");
    let stream_22_stats: (i64, i64) = conn
        .query_row(
            "SELECT candidate_count, candidate_bytes FROM content_stream_stats WHERE stream_id = \
             ?1",
            [vec![0x22u8; 32]],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stream_22_stats, (1, 39), "move/delete and duplicate attempts do not drift peers");

    for sql in [
        "INSERT INTO content_stream_stats VALUES(zeroblob(31), 0, 0)",
        "INSERT INTO content_stream_stats VALUES(zeroblob(32), -1, 0)",
        "INSERT INTO content_stream_stats VALUES(zeroblob(32), 0, -1)",
        "INSERT INTO content_streams_pending_refold VALUES(zeroblob(31), 1, 0, 0)",
        "INSERT INTO content_streams_pending_refold VALUES(randomblob(32), 0, 0, 0)",
        "INSERT INTO content_streams_pending_refold VALUES(randomblob(32), 4, 0, 0)",
    ] {
        assert!(conn.execute_batch(sql).is_err(), "constraint must reject `{sql}`");
    }

    // A full-ladder replay recomputes the same source-derived stats and leaves queue metadata
    // intact.
    schema::migrations::apply_content_refold_queue_and_stats(&conn).unwrap();
    schema::migrations::apply_content_refold_queue_and_stats(&conn).unwrap();
    let replay_stats: Vec<(Vec<u8>, i64, i64)> = conn
        .prepare(
            "SELECT stream_id, candidate_count, candidate_bytes
             FROM content_stream_stats ORDER BY stream_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(replay_stats, vec![(vec![0x11u8; 32], 2, 72), (vec![0x22u8; 32], 1, 39)]);
    assert_eq!(
        conn.query_row(
            "SELECT first_enqueued_at_ms, last_enqueued_at_ms
             FROM content_streams_pending_refold WHERE stream_id = ?1",
            [vec![0x11u8; 32]],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap(),
        (100, 300),
        "replay does not rebuild or overwrite the upgraded queue",
    );
    let v81_recorded: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_version WHERE id = '081_distill_evidence_source_part'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v81_recorded, 1, "the previous-tip V081 migration remains recorded");
    let v82_recorded: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_version WHERE id = '082_content_refold_queue_and_stats'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v82_recorded, 1, "forward migration records V082");
}

/// V083 recomputes `logical_symbols.group_reason` from member evidence.
///
/// The column is derived but PERSISTED, so a new labelling rule alone would never reach an existing
/// index: a query-only server over an unchanged repository never runs `rebuild_logical_symbols` and
/// would keep serving the old `cfg_variant` for every multi-member group indefinitely.
#[test]
fn migration_083_relabels_logical_groups_by_evidence() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();

    // Two `files` ROWS for one path — what a worktree-overlay or commit scope produces.
    for (id, worktree) in [(1_i64, "base"), (2_i64, "wt")] {
        conn.execute(
            "INSERT INTO files(id, path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               commit_sha, worktree_id)
             VALUES (?1, 'src/lib.rs', 'rust', 'source', 'sha', 0, 0, 'head', ?2)",
            rusqlite::params![id, worktree],
        )
        .unwrap();
    }
    let symbol = |id: i64, file_id: i64, name: &str| {
        conn.execute(
            "INSERT INTO symbols(id, file_id, language, name, kind, start_byte, end_byte)
             VALUES (?1, ?2, 'rust', ?3, 'function', 0, 1)",
            rusqlite::params![id, file_id, name],
        )
        .unwrap();
    };
    // `replicated` is ONE symbol seen in both scopes; `collided` is TWO symbols in one file.
    symbol(1, 1, "replicated");
    symbol(2, 2, "replicated");
    symbol(3, 1, "collided");
    symbol(4, 1, "collided");
    symbol(5, 2, "solo");

    let group = |id: i64, name: &str, members: &[i64]| {
        conn.execute(
            "INSERT INTO logical_symbols(id, language, path, logical_name, kind, variant_count,
                                         group_reason)
             VALUES (?1, 'rust', 'src/lib.rs', ?2, 'function', ?3, 'cfg_variant')",
            rusqlite::params![id, name, members.len() as i64],
        )
        .unwrap();
        for symbol_id in members {
            conn.execute(
                "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line,
                                                    end_line)
                 VALUES (?1, ?2, 1, 2)",
                rusqlite::params![id, symbol_id],
            )
            .unwrap();
        }
    };
    group(100, "replicated", &[1, 2]);
    group(200, "collided", &[3, 4]);
    group(300, "solo", &[5]);

    truncate_schema_to(&conn, 82);
    schema::migrate_forward(&conn, &crate::index::migration_hooks()).unwrap();
    // Replaying from 82 runs V083 and everything after it, so pin the TIP rather than a literal.
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);

    let reason = |id: i64| -> String {
        conn.query_row("SELECT group_reason FROM logical_symbols WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap()
    };
    assert_eq!(
        reason(100),
        "scope_replica",
        "one symbol indexed in two scopes is a replica, not a cfg variant — this is the case the \
         old label got wrong for the overwhelming majority of groups",
    );
    assert_eq!(
        reason(200),
        "same_file_multi",
        "two symbols inside one file row genuinely share an identity",
    );
    assert_eq!(reason(300), "single");
}

/// V084 links each chunk to the symbol it was cut from. (The absolute schema-tip pin moved to
/// `migration_085_*` — V085 is the tip now; this test keeps only a per-migration check.)
#[test]
fn migration_084_links_chunks_to_symbols() {
    // The chunks table predates the column (it lives in the baseline). Build bare chunks + symbols
    // tables WITHOUT symbol_id, seed pre-migration rows, apply V084 in ISOLATION, and confirm the
    // column appears AND the backfill links each chunk — asserting absence against this migration's
    // own precondition, never the full ladder's end state.
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE chunks(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_id INTEGER NOT NULL,
                 chunk_kind TEXT NOT NULL,
                 symbol_path TEXT,
                 start_byte INTEGER NOT NULL,
                 end_byte INTEGER NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL,
                 text_hash TEXT NOT NULL);
             CREATE TABLE name_strings(id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL);
             CREATE TABLE symbols(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_id INTEGER NOT NULL,
                 qualified_name_id INTEGER NOT NULL,
                 start_byte INTEGER NOT NULL,
                 end_byte INTEGER NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL);",
        )
        .unwrap();
    assert!(
        !schema::column_exists(&legacy, "chunks", "symbol_id").unwrap(),
        "pre-V084 chunks has no symbol_id",
    );

    // Symbols in one file: an OUTER `outer` (id 1, bytes 100..200) with a DIFFERENT-named NESTED
    // `inner` (id 2, bytes 130..160); a PAIR of same-name `g` (ids 3 & 4, disjoint bytes on one
    // physical line — the minified same-line case); and an OUTER `wrap` (id 5, bytes 400..600) with
    // a same-name NESTED `wrap` (id 6, bytes 500..560). Their line spans are 0/0 — the migration
    // DEFAULT for a symbol never reindexed since the line columns were added — so the backfill MUST
    // key off byte spans. Chunks carry the qualified name they were cut from (a split continuation
    // appends `#<n>`).
    legacy
        .execute_batch(
            "INSERT INTO name_strings(id, value) VALUES (1,'outer'), (2,'inner'), (3,'g'), \
             (4,'wrap'), (5,'src/od#d.rs::run'), (6,'trailing2');
             INSERT INTO symbols(id, file_id, qualified_name_id, start_byte, end_byte, start_line, \
             end_line)
                 VALUES (1, 1, 1, 100, 200, 0, 0), (2, 1, 2, 130, 160, 0, 0),
                        (3, 1, 3, 300, 310, 0, 0), (4, 1, 3, 320, 330, 0, 0),
                        (5, 1, 4, 400, 600, 0, 0), (6, 1, 4, 500, 560, 0, 0),
                        (7, 1, 5, 700, 720, 0, 0), (8, 1, 6, 800, 820, 0, 0);
             INSERT INTO chunks(file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line,
                                end_line, text_hash)
                 VALUES (1,'code','outer',100,200,13,14,'h'),
                        (1,'code','inner',130,160,14,15,'h'),
                        (1,'code','outer#1',130,160,13,14,'h'),
                        (1,'code','g',300,330,30,30,'h'),
                        (1,'code','wrap#1',520,540,55,58,'h'),
                        (1,'code',NULL,900,910,80,81,'h'),
                        (1,'code','src/od#d.rs::run',700,720,70,71,'h'),
                        (1,'code','trailing2',800,820,75,76,'h');",
        )
        .unwrap();

    schema::migrations::apply_chunk_symbol_id(&legacy).unwrap();
    assert!(
        schema::column_exists(&legacy, "chunks", "symbol_id").unwrap(),
        "V084 adds the symbol_id column",
    );

    // The backfill binds a chunk to the same-named symbol it overlaps in BYTES only when that
    // symbol is UNIQUE — matching by NAME (line spans are 0, so a line predicate would match
    // nothing):
    //   * `outer` -> id 1, even though the `inner` bytes also overlap (`inner` is a different name,
    //     so it is not a candidate);
    //   * `inner` -> id 2;
    //   * `outer#1` CONTINUATION -> id 1: the `#1` suffix is stripped to `outer`, whose only
    //     overlapping symbol is the outer — recovered, and NOT mis-bound to the nested `inner`.
    // It leaves NULL every case no stored data can settle: the same-line `g` chunk (its whole-line
    // bytes overlap both `g` symbols), the `wrap#1` continuation (overlaps both the outer and
    // nested `wrap`), and the uncovered chunk (no symbol_path).
    let linked: Vec<(Option<String>, Option<i64>)> = legacy
        .prepare("SELECT symbol_path, symbol_id FROM chunks ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        linked,
        vec![
            (Some("outer".to_string()), Some(1)),
            (Some("inner".to_string()), Some(2)),
            (Some("outer#1".to_string()), Some(1)),
            (Some("g".to_string()), None),
            (Some("wrap#1".to_string()), None),
            (None, None),
            // A `#` inside the FILE PATH is not a continuation marker. Splitting on the FIRST `#`
            // would truncate this to `src/od`, match nothing, and — since unchanged files are
            // never re-chunked — strand the row at NULL forever. Only a TRAILING
            // `#<digits>` is a suffix.
            (Some("src/od#d.rs::run".to_string()), Some(7)),
            // Trailing digits that are part of the NAME are likewise not a suffix: there is no `#`
            // before them.
            (Some("trailing2".to_string()), Some(8)),
        ],
        "backfill links a uniquely-named container (including a stripped continuation); a \
         same-name tie / nested continuation / uncovered chunk stays NULL, and a `#` inside the \
         FILE PATH is never mistaken for a continuation marker",
    );

    // Torn replay: the column already exists and every chunk is already linked, so re-applying is a
    // no-op — it neither errors nor overwrites a resolved symbol_id.
    schema::migrations::apply_chunk_symbol_id(&legacy).unwrap();
    let relinked: Vec<(Option<String>, Option<i64>)> = legacy
        .prepare("SELECT symbol_path, symbol_id FROM chunks ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(relinked, linked, "an idempotent re-apply preserves the backfilled links");

    // Full ladder: the tip provisions cleanly and records V084 with the column present.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    assert!(
        schema::column_exists(&conn, "chunks", "symbol_id").unwrap(),
        "the full ladder ends with chunks.symbol_id present",
    );
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '084_chunk_symbol_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V084");
}

/// V085 adds sync provenance (`origin`) to the read tables + edge tombstones (`present`) to the
/// content-edge projection. The absolute schema-tip pin lives on the newest migration's test
/// (V088, `migration_088_caches_the_generation_posting_row_count`).
#[test]
fn migration_085_adds_origin_and_edge_present() {
    // Bare pre-V085 tables (no origin / present), each with a pre-existing row, so the migration is
    // asserted against its OWN precondition — not the full ladder's end state.
    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE repo_memories(id TEXT PRIMARY KEY, repo_id TEXT);
             CREATE TABLE repo_node_edges(edge_key TEXT PRIMARY KEY, repo_id TEXT);
             CREATE TABLE content_projected_edges(
                 stream_id BLOB NOT NULL, edge_key TEXT NOT NULL,
                 spec_json TEXT NOT NULL, resolved_json TEXT,
                 PRIMARY KEY(stream_id, edge_key)) STRICT;
             INSERT INTO repo_memories(id, repo_id) VALUES ('mem_1', 'r');
             INSERT INTO repo_node_edges(edge_key, repo_id) VALUES ('e_1', 'r');
             INSERT INTO content_projected_edges(stream_id, edge_key, spec_json)
                 VALUES (x'00', 'e_1', '{}');",
        )
        .unwrap();
    let added = [
        ("repo_memories", "origin"),
        ("repo_node_edges", "origin"),
        ("content_projected_edges", "present"),
    ];
    for (t, c) in added {
        assert!(!schema::column_exists(&legacy, t, c).unwrap(), "pre-V085 {t} has no {c}");
    }

    schema::migrations::apply_sync_origin_and_edge_tombstone(&legacy).unwrap();

    for (t, c) in added {
        assert!(schema::column_exists(&legacy, t, c).unwrap(), "V085 adds {t}.{c}");
    }
    // Pre-existing rows backfill to the correct defaults.
    let mem_origin: String = legacy
        .query_row("SELECT origin FROM repo_memories WHERE id = 'mem_1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mem_origin, "local", "existing memories default to local origin");
    let edge_origin: String = legacy
        .query_row("SELECT origin FROM repo_node_edges WHERE edge_key = 'e_1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(edge_origin, "local");
    let present: i64 = legacy
        .query_row("SELECT present FROM content_projected_edges WHERE edge_key = 'e_1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(present, 1, "existing projected edges default to present");
    // The CHECK rejects an out-of-domain origin.
    assert!(
        legacy
            .execute(
                "INSERT INTO repo_memories(id, repo_id, origin) VALUES ('mem_2','r','bogus')",
                []
            )
            .is_err(),
        "the origin CHECK rejects a value that is neither local nor synced",
    );

    // The full ladder ends with the columns present and records V085.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    for (t, c) in added {
        assert!(schema::column_exists(&conn, t, c).unwrap(), "the full ladder ends with {t}.{c}");
    }
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_version WHERE id = '085_sync_origin_and_edge_tombstone'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the forward migration records V085");
}
