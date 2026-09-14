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
        "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, generated_at_ms)
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
    assert_eq!(reason, "too slow", "the nullable reason column is preserved through the rebuild");
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

/// V113 queues every stream holding `/3` content for a refold (so pre-clamp accepted lamports
/// get re-judged), merges into an existing queue row instead of clobbering it, and is
/// idempotent on replay.
#[test]
fn v113_queues_every_content_stream_for_refold_and_merges_existing_queue_rows() {
    let conn = Connection::open_in_memory().unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    for (stream, entry) in [([0x41_u8; 32], [0x01_u8; 32]), ([0x42; 32], [0x02; 32])] {
        conn.execute(
            "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?5, ?5, 1, x'00', 0)",
            rusqlite::params![
                entry.as_slice(),
                stream.as_slice(),
                [0x11_u8; 32].as_slice(),
                [0x12_u8; 32].as_slice(),
                0_u64.to_be_bytes().as_slice(),
                [0x13_u8; 32].as_slice(),
            ],
        )
        .unwrap();
    }
    // Stream 0x41 is already queued for an account change (mask 2) with live timestamps: the
    // migration must OR the content bit in, not replace the row.
    conn.execute(
        "INSERT INTO content_streams_pending_refold(
                 stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
             VALUES(?1, 2, 7, 7)",
        [[0x41_u8; 32].as_slice()],
    )
    .unwrap();
    let hooks = crate::hooks::MigrationHooks::noop();
    super::apply_refold_content_streams_for_lamport_clamp(&conn, &hooks).unwrap();
    super::apply_refold_content_streams_for_lamport_clamp(&conn, &hooks).unwrap();
    let rows: Vec<(Vec<u8>, i64, i64)> = conn
        .prepare(
            "SELECT stream_id, reason_mask, first_enqueued_at_ms
                 FROM content_streams_pending_refold ORDER BY stream_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![(vec![0x41; 32], 3, 7), (vec![0x42; 32], 1, 0)]);
}

/// V121 queues every stream that holds content for a refold, ORing into an existing queue row,
/// and is idempotent on replay (#1282). The all-account refold it also triggers is a hook,
/// exercised in the op-log crate.
#[test]
fn v121_queues_every_content_stream_for_a_refold() {
    let conn = Connection::open_in_memory().unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    for (entry, stream) in [([0x31_u8; 32], [0x41_u8; 32]), ([0x32; 32], [0x42; 32])] {
        conn.execute(
            "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?5, ?5, 1, x'00', 0)",
            rusqlite::params![
                entry.as_slice(),
                stream.as_slice(),
                [0x11_u8; 32].as_slice(),
                [0x12_u8; 32].as_slice(),
                0_u64.to_be_bytes().as_slice(),
                [0x13_u8; 32].as_slice(),
            ],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO content_streams_pending_refold(
                 stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
             VALUES(?1, 2, 7, 7)",
        [[0x41_u8; 32].as_slice()],
    )
    .unwrap();
    super::apply_refold_for_held_control_log_freshness(&conn).unwrap();
    super::apply_refold_for_held_control_log_freshness(&conn).unwrap();
    let rows: Vec<(Vec<u8>, i64, i64)> = conn
        .prepare(
            "SELECT stream_id, reason_mask, first_enqueued_at_ms
                 FROM content_streams_pending_refold ORDER BY stream_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![(vec![0x41; 32], 3, 7), (vec![0x42; 32], 1, 0)]);
}

/// V126 (#1319) creates the per-memory summary table and seeds each memory's newest row from
/// the retired per-hash table, for repos that exist; a replay adds nothing — including for a
/// retired id whose repo row adoption has since dropped.
#[test]
fn v126_seeds_the_newest_summary_per_memory_and_is_idempotent() {
    let conn = Connection::open_in_memory().unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    conn.execute_batch(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0);
             INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, \
         generated_at_ms)
             VALUES ('m', 'r', 'old', 'stale', 1), ('m', 'r', 'new', 'current', 2),
                    ('n', 'r', 'tie-b', 'b', 5), ('n', 'r', 'tie-a', 'a', 5),
                    ('o', 'gone', 'h', 'orphan', 9);
             DELETE FROM memory_note_summaries;",
    )
    .unwrap();
    super::apply_memory_note_summaries(&conn).unwrap();
    conn.execute("UPDATE memory_note_summaries SET summary = 'edited' WHERE memory_id = 'm'", [])
        .unwrap();
    super::apply_memory_note_summaries(&conn).unwrap();
    let rows: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT memory_id, content_hash, summary FROM memory_note_summaries ORDER BY memory_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![
        ("m".to_string(), "new".to_string(), "edited".to_string()),
        ("n".to_string(), "tie-b".to_string(), "b".to_string()),
    ]);

    // Adoption re-points the live rows and drops the source repo row, leaving the retired
    // rows behind; the next full replay must not seed them back.
    conn.execute_batch(
        "UPDATE memory_note_summaries SET repo_id = 'real';
             DELETE FROM repos WHERE repo_id = 'r';",
    )
    .unwrap();
    super::apply_memory_note_summaries(&conn).unwrap();
    let reseeded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_note_summaries WHERE repo_id != 'real'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reseeded, 0, "nothing comes back under an id no repo owns");
}

/// V125 (#1301) queues every content stream for the next settle, exactly as V121 does, and
/// is idempotent on replay. The all-account refold it also triggers is a hook, exercised in the
/// op-log crate.
#[test]
fn v125_queues_every_content_stream_for_a_refold() {
    let conn = Connection::open_in_memory().unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    conn.execute(
        "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                 prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                 accepted, signed_bytes, received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?5, ?5, 1, x'00', 0)",
        rusqlite::params![
            [0x31_u8; 32].as_slice(),
            [0x41_u8; 32].as_slice(),
            [0x11_u8; 32].as_slice(),
            [0x12_u8; 32].as_slice(),
            0_u64.to_be_bytes().as_slice(),
            [0x13_u8; 32].as_slice(),
        ],
    )
    .unwrap();
    super::apply_refold_for_concurrent_cut_vouch(&conn).unwrap();
    super::apply_refold_for_concurrent_cut_vouch(&conn).unwrap();
    let rows: Vec<(Vec<u8>, i64)> = conn
        .prepare("SELECT stream_id, reason_mask FROM content_streams_pending_refold")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![(vec![0x41; 32], 1)]);
}

/// V114 adds the nullable denormalized lamport column and its partial accepted-rows index,
/// and is idempotent on replay. The backfill itself is a hook (the lamport lives in the
/// signed CBOR envelope), exercised in the op-log crate; with noop hooks the column simply
/// stays NULL.
#[test]
fn v114_adds_the_lamport_column_and_its_accepted_index() {
    let conn = Connection::open_in_memory().unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    let has_column: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('content_entries') WHERE name = 'lamport'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_column, 1, "content_entries carries the lamport column");
    let has_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index'
                 AND name = 'idx_content_entries_stream_accepted_lamport'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_index, 1, "the partial accepted-rows lamport index exists");
    let has_clocks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'
                 AND name = 'content_stream_clocks'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_clocks, 1, "the refold-persisted stream clock table exists");
}

/// The full ladder re-keys anchors onto the natural key, drops the AUTOINCREMENT id, and is
/// idempotent on a second apply. V078's index names survive (non-unique) so its replay guard
/// and the drive-by `selected = 1` lookups stay indexed.
#[test]
fn v112_rekeys_anchors_onto_the_natural_key() {
    let conn = Connection::open_in_memory().unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    assert_eq!(primary_key_columns(&conn, "papertrail_distill_anchors").unwrap(), [
        "repo_id",
        "tracker",
        "project",
        "item_kind",
        "item_key",
        "candidate_ordinal"
    ]);
    let has_id: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('papertrail_distill_anchors')
                 WHERE name = 'id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_id, 0, "the AUTOINCREMENT id is dropped");
    for index in [
        "idx_papertrail_distill_anchors_candidate",
        "idx_papertrail_distill_anchors_selected",
        "idx_papertrail_distill_anchors_symbol",
    ] {
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "V112 keeps index `{index}`");
    }
}

/// The rebuild copies the device-local columns (`logical_symbol_id`, `resolved`) and the
/// selection state verbatim — the whole-row-LWW applier never sees these, so the migration is
/// the only thing that can lose them.
#[test]
fn v112_preserves_local_resolution_and_selection() {
    let conn = Connection::open_in_memory().unwrap();
    super::apply_distill_record_store(&conn).unwrap();
    super::apply_distill_anchor_selection(&conn).unwrap();
    conn.execute(
        "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, candidate_ordinal, anchor_kind,
                  logical_symbol_id, file_path, name, resolved, selected, repo_id)
             VALUES ('github', 'o/r', 'issue', '9', 0, 'symbol',
                     'sym_abc', 'src/x.rs', 'Foo', 1, 1, 'r')",
        [],
    )
    .unwrap();
    super::apply_syncable_distill_anchors(&conn).unwrap();
    let (sym, resolved, selected): (String, i64, i64) = conn
        .query_row(
            "SELECT logical_symbol_id, resolved, selected FROM papertrail_distill_anchors
                 WHERE repo_id = 'r' AND item_key = '9' AND candidate_ordinal = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((sym.as_str(), resolved, selected), ("sym_abc", 1, 1));
}

/// V127 (#1295) creates the tombstone statements table and backfills one statement per
/// tombstone at its own identity; a replay never lowers a statement that has since advanced.
#[test]
fn v127_backfills_one_statement_per_tombstone_and_never_lowers_one() {
    let conn = Connection::open_in_memory().unwrap();
    super::super::apply(&conn, &crate::hooks::MigrationHooks::noop()).unwrap();
    conn.execute_batch(
        "INSERT INTO sync_row_tombstones(
                 stream_id, repo_id, table_name, row_pk, lamport, device_fingerprint)
             VALUES (zeroblob(32), 'r', 't', 'p1', 5, 'aa'), (zeroblob(32), 'r', 't', 'p2', 9, \
         'bb');
             DELETE FROM sync_tombstone_statements;",
    )
    .unwrap();
    super::apply_tombstone_statements(&conn).unwrap();
    conn.execute("UPDATE sync_tombstone_statements SET lamport = 40 WHERE row_pk = 'p1'", [])
        .unwrap();
    super::apply_tombstone_statements(&conn).unwrap();
    let rows: Vec<(String, String, i64)> = conn
        .prepare(
            "SELECT row_pk, device_fingerprint, lamport FROM sync_tombstone_statements ORDER BY \
             row_pk",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![
        ("p1".to_string(), "aa".to_string(), 40),
        ("p2".to_string(), "bb".to_string(), 9),
    ]);
}
