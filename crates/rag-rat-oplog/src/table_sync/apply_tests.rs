use super::*;
use crate::op::DeviceFingerprint;
use crate::table_sync::registry::ColumnSpec;

const SPEC: TableSpec = TableSpec {
    name: "t_demo",
    scope_id: "demo/1",
    spec_version: 1,
    pk: &[ColumnSpec::required("id", ValueType::Text)],
    columns: &[ColumnSpec::required("title", ValueType::Text)],
    local_columns: &["resolved_rowid"],
    repo_column: None,
};

fn conn() -> rusqlite::Connection {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch(
        "CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT, resolved_rowid INTEGER) STRICT;",
    )
    .unwrap();
    c
}

fn device(seed: u8) -> DeviceFingerprint {
    DeviceFingerprint::from_bytes([seed; 32])
}

fn upsert(cells: &[(&str, TypedValue)]) -> RowOp {
    RowOp::Upsert {
        spec_version: 1,
        table: "t_demo".to_string(),
        pk: vec![TypedValue::Text("r1".to_string())],
        cells: cells
            .iter()
            .map(|(c, v)| Cell { column: (*c).to_string(), value: v.clone() })
            .collect(),
    }
}

fn title(conn: &rusqlite::Connection) -> Option<String> {
    conn.query_row("SELECT title FROM t_demo WHERE id = 'r1'", [], |r| r.get(0)).optional().unwrap()
}

#[test]
fn an_insert_writes_the_row_and_records_the_clock_and_hash() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let out = apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("hi".to_string()))]),
        OpMeta { lamport: 5, device: device(2) },
    )
    .unwrap();
    assert_eq!(out, ApplyOutcome::Applied);
    let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
    assert!(published_hash(&tx, "repo", "t_demo", &row_pk).unwrap().is_some());
    assert_eq!(current_row_clock(&tx, "repo", "t_demo", &row_pk).unwrap().unwrap().0, 5);
    tx.commit().unwrap();
    assert_eq!(title(&c).as_deref(), Some("hi"));
}

/// A `repo_memory_bindings` upsert over the production spec, every synced cell supplied.
fn binding_upsert(path: &str) -> RowOp {
    binding_upsert_of("symbol", path)
}

fn binding_upsert_of(kind: &str, path: &str) -> RowOp {
    let text = |value: &str| TypedValue::Text(value.to_string());
    RowOp::Upsert {
        spec_version: 1,
        table: "repo_memory_bindings".to_string(),
        pk: vec![text("repo-a"), text("memory-a"), text(kind), text("src/lib.rs::Run")],
        cells: [
            ("path", text(path)),
            ("start_line", TypedValue::I64(4)),
            ("end_line", TypedValue::I64(9)),
            ("commit_hash", text("")),
            ("tracker", text("")),
            ("project", text("")),
            ("item_key", text("")),
            ("created_at_ms", TypedValue::I64(1)),
            ("symbol_kind", text("struct")),
            ("signature_hash", text("sig")),
            ("moniker_tool", text("")),
            ("moniker_tool_version", text("")),
        ]
        .into_iter()
        .map(|(column, value)| Cell { column: column.to_string(), value })
        .collect(),
    }
}

/// The local columns of the one production binding row: what this store resolved beside
/// the authored anchor.
fn binding_resolution(
    conn: &rusqlite::Connection,
) -> (Option<i64>, Option<i64>, Option<String>, Option<String>) {
    conn.query_row(
        "SELECT logical_symbol_id, symbol_id, resolved_binding_id, resolved_symbol_kind
             FROM repo_memory_bindings WHERE memory_id = 'memory-a'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .unwrap()
}

/// A winning upsert that CHANGES a binding's authored columns resets this store's resolution
/// of the old anchor — where relocation landed — while keeping every handle validation
/// re-derives the target from; one that RESTATES the row already held leaves the resolution
/// alone. Without the reset the location and discriminators recorded for the old anchor
/// would outlive the author's change as this store's view (#1297).
#[test]
fn a_changed_binding_upsert_resets_the_local_resolution_and_a_restated_one_keeps_it() {
    let spec = super::super::registry::SYNCABLE_TABLES
        .iter()
        .find(|spec| spec.name == "repo_memory_bindings")
        .expect("the anchors spec");
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let meta = |lamport: u64| OpMeta { lamport, device: device(2) };
    assert_eq!(
        apply_row_op(&tx, spec, "repo-a", &binding_upsert("src/lib.rs"), meta(5)).unwrap(),
        ApplyOutcome::Applied
    );
    tx.execute(
        "UPDATE repo_memory_bindings
                SET logical_symbol_id = 71, symbol_id = 72, resolved_binding_id = \
         'src/lib.rs::Ran',
                    resolved_symbol_kind = 'impl'
              WHERE memory_id = 'memory-a'",
        [],
    )
    .unwrap();
    // Restated at a higher clock — a sibling writing the same row: nothing to invalidate.
    assert_eq!(
        apply_row_op(&tx, spec, "repo-a", &binding_upsert("src/lib.rs"), meta(6)).unwrap(),
        ApplyOutcome::Applied
    );
    assert_eq!(
        binding_resolution(&tx),
        (Some(71), Some(72), Some("src/lib.rs::Ran".to_string()), Some("impl".to_string())),
        "a restated row keeps this store's resolution",
    );
    // Changed: a new authored statement; the old resolution goes, the handle stays.
    assert_eq!(
        apply_row_op(&tx, spec, "repo-a", &binding_upsert("src/moved.rs"), meta(7)).unwrap(),
        ApplyOutcome::Applied
    );
    assert_eq!(
        binding_resolution(&tx),
        (Some(71), Some(72), None, None),
        "a changed row resets the resolution and keeps every handle",
    );
}

/// A call-path binding's resolution is the KEY of its local call-path rows, not evidence:
/// an authored restatement leaves it in place (`registry::reset_on_upsert_keeps`), or the
/// rows under the resolved hash would be unreachable and the anchor `gone`.
#[test]
fn a_changed_call_path_upsert_keeps_the_resolution_the_local_rows_are_keyed_by() {
    let spec = super::super::registry::SYNCABLE_TABLES
        .iter()
        .find(|spec| spec.name == "repo_memory_bindings")
        .expect("the anchors spec");
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let meta = |lamport: u64| OpMeta { lamport, device: device(2) };
    let op = |path: &str| binding_upsert_of("call_path", path);
    assert_eq!(
        apply_row_op(&tx, spec, "repo-a", &op("a"), meta(5)).unwrap(),
        ApplyOutcome::Applied
    );
    tx.execute(
        "UPDATE repo_memory_bindings SET resolved = 1, resolved_binding_id = 'converged-hash'
              WHERE memory_id = 'memory-a'",
        [],
    )
    .unwrap();
    assert_eq!(
        apply_row_op(&tx, spec, "repo-a", &op("b"), meta(6)).unwrap(),
        ApplyOutcome::Applied
    );
    let (flag, hash): (Option<i64>, Option<String>) = tx
        .query_row(
            "SELECT resolved, resolved_binding_id FROM repo_memory_bindings
                 WHERE memory_id = 'memory-a'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((flag, hash.as_deref()), (Some(1), Some("converged-hash")));
}

#[test]
fn a_new_incarnation_does_not_inherit_the_old_streams_row_clock() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let old = StreamId::from_bytes([1; 32]);
    let new = StreamId::from_bytes([2; 32]);
    apply_row_op_on_stream(
        &tx,
        &SPEC,
        "repo",
        old,
        &upsert(&[("title", TypedValue::Text("old".into()))]),
        OpMeta { lamport: 10_000, device: device(1) },
    )
    .unwrap();
    let outcome = apply_row_op_on_stream(
        &tx,
        &SPEC,
        "repo",
        new,
        &upsert(&[("title", TypedValue::Text("new".into()))]),
        OpMeta { lamport: 0, device: device(2) },
    )
    .unwrap();
    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(title(&tx).as_deref(), Some("new"));
    let clocks: i64 =
        tx.query_row("SELECT COUNT(*) FROM sync_row_clocks", [], |row| row.get(0)).unwrap();
    assert_eq!(clocks, 2, "each incarnation keeps an independent row clock");
}

#[test]
fn whole_row_lww_the_winner_takes_the_whole_row_no_per_column_merge() {
    // The chosen semantics: a concurrent edit to a DIFFERENT column does NOT merge in — the
    // higher-lamport op owns the entire row. Deterministic (both peers converge to the same
    // winner) and matches the whole-row real tables. Two-column table so the merge could
    // differ.
    const TWO_COL: TableSpec = TableSpec {
        name: "t_two",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("count", ValueType::I64),
        ],
        local_columns: &[],
        repo_column: None,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch("CREATE TABLE t_two(id TEXT PRIMARY KEY, title TEXT, count INTEGER) STRICT;")
        .unwrap();
    let full = |title: &str, count: i64| RowOp::Upsert {
        spec_version: 1,
        table: "t_two".to_string(),
        pk: vec![TypedValue::Text("r1".into())],
        cells: vec![Cell { column: "title".into(), value: TypedValue::Text(title.into()) }, Cell {
            column: "count".into(),
            value: TypedValue::I64(count),
        }],
    };
    let tx = c.transaction().unwrap();
    // Higher lamport sets {A, 1}...
    apply_row_op(&tx, &TWO_COL, "repo", &full("A", 1), OpMeta { lamport: 6, device: device(2) })
        .unwrap();
    // ...a lower-lamport op editing a different column loses the WHOLE row, not just `title`.
    let out = apply_row_op(&tx, &TWO_COL, "repo", &full("B", 2), OpMeta {
        lamport: 5,
        device: device(1),
    })
    .unwrap();
    assert_eq!(out, ApplyOutcome::Superseded, "outranked by the row's write clock");
    tx.commit().unwrap();
    let row: (String, i64) = c
        .query_row("SELECT title, count FROM t_two WHERE id = 'r1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((row.0.as_str(), row.1), ("A", 1), "the whole row is the winning op's — no merge");
}

#[test]
fn a_higher_lamport_wins_and_a_lower_lamport_loses() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply_row_op(&tx, &SPEC, "repo", &upsert(&[("title", TypedValue::Text("a".into()))]), OpMeta {
        lamport: 5,
        device: device(2),
    })
    .unwrap();
    // A later lamport overwrites.
    apply_row_op(&tx, &SPEC, "repo", &upsert(&[("title", TypedValue::Text("b".into()))]), OpMeta {
        lamport: 6,
        device: device(2),
    })
    .unwrap();
    // A stale (lower-lamport) op is ignored.
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("stale".into()))]),
        OpMeta { lamport: 4, device: device(9) },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(title(&c).as_deref(), Some("b"));
}

#[test]
fn a_lamport_tie_is_broken_by_the_smaller_fingerprint() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("dev5".into()))]),
        OpMeta { lamport: 7, device: device(5) },
    )
    .unwrap();
    // Same lamport, SMALLER fingerprint → wins.
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("dev1".into()))]),
        OpMeta { lamport: 7, device: device(1) },
    )
    .unwrap();
    // Same lamport, LARGER fingerprint → loses.
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("dev9".into()))]),
        OpMeta { lamport: 7, device: device(9) },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(title(&c).as_deref(), Some("dev1"), "the smaller fingerprint wins the tie");
}

#[test]
fn a_type_mismatch_quarantines_the_op_and_writes_nothing() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    // `title` is declared Text; an I64 value is a broken producer.
    let out = apply_row_op(&tx, &SPEC, "repo", &upsert(&[("title", TypedValue::I64(7))]), OpMeta {
        lamport: 1,
        device: device(2),
    })
    .unwrap();
    assert!(matches!(out, ApplyOutcome::Quarantined { .. }));
    tx.commit().unwrap();
    assert_eq!(title(&c), None, "a quarantined op leaves the table untouched");
}

#[test]
fn a_partial_upsert_missing_a_synced_column_is_parked() {
    // Whole-row LWW needs a full after-image; a two-column table given only one column can't be
    // cleanly replaced, so the op is quarantined rather than applied as a hybrid row.
    const TWO_COL: TableSpec = TableSpec {
        name: "t_two",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("count", ValueType::I64),
        ],
        local_columns: &[],
        repo_column: None,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch("CREATE TABLE t_two(id TEXT PRIMARY KEY, title TEXT, count INTEGER) STRICT;")
        .unwrap();
    let tx = c.transaction().unwrap();
    let partial = RowOp::Upsert {
        spec_version: 1,
        table: "t_two".to_string(),
        pk: vec![TypedValue::Text("r1".into())],
        cells: vec![Cell { column: "title".into(), value: TypedValue::Text("only-title".into()) }],
    };
    let out =
        apply_row_op(&tx, &TWO_COL, "repo", &partial, OpMeta { lamport: 1, device: device(2) })
            .unwrap();
    assert_eq!(
        out,
        ApplyOutcome::Unprojectable(PendingReason::PartialAfterImage),
        "a partial after-image is PARKED, not quarantined: the likely cause is an older producer \
         whose narrower complete row is partial under this spec, and #1002's declared defaults \
         redeem it — quarantining would drop it off the refold worklist for good"
    );
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_two", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0, "nothing was written");
}

#[test]
fn a_newer_op_parks_on_its_version_alone_even_carrying_only_known_columns() {
    // The version gate must do work the unknown-column gate does not. Every "newer" fixture
    // elsewhere ALSO names a column the receiver lacks, so deleting the version check entirely
    // only changed a reason string — the op parked either way. Here the op is well within this
    // registry's vocabulary and is refused purely for claiming a later generation, which is the
    // whole point: a later spec may mean something by these same columns that we cannot know.
    assert_eq!(
        project_cells(&SPEC, SPEC.spec_version + 1, &[Cell {
            column: "title".into(),
            value: TypedValue::Text("known".into()),
        }]),
        Projection::Park(PendingReason::NewerSpecVersion),
        "a newer generation parks on the version alone"
    );
    // And the converse, so the gate cannot simply park everything.
    assert!(matches!(
        project_cells(&SPEC, SPEC.spec_version, &[Cell {
            column: "title".into(),
            value: TypedValue::Text("known".into()),
        }]),
        Projection::Complete(_)
    ));
}

#[test]
fn a_cell_newer_than_the_ops_own_claimed_version_is_a_misstamp() {
    // Self-contradictory: a producer at v1 cannot have known a column introduced at v2. Left
    // unchecked, the understated version would default-fill every column it claims to predate,
    // resetting them on every receiver — so this is the one half of the advisory stamp a
    // receiver can verify against its own registry.
    const WIDE: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        spec_version: 2,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("d")),
        ],
        local_columns: &[],
        repo_column: None,
    };
    let cells = |version: u32| {
        project_cells(&WIDE, version, &[
            Cell { column: "later".into(), value: TypedValue::Text("v".into()) },
            Cell { column: "title".into(), value: TypedValue::Text("t".into()) },
        ])
    };
    assert_eq!(
        cells(1),
        Projection::Park(PendingReason::MisstampedSpecVersion),
        "claiming v1 while carrying a v2 column is a mis-stamp, not an old complete row"
    );
    assert!(matches!(cells(2), Projection::Complete(_)), "the honest stamp projects");
}

#[test]
fn every_default_variant_fills_its_own_typed_value() {
    // One case per `DefaultValue` variant. Without this, only `Text` and `Null` were exercised,
    // and collapsing `Bool`/`I64`/`Blob` to `TypedValue::Null` passed the whole suite — which
    // on a NOT NULL column quarantines the op terminally, and on a nullable one
    // diverges from the migration's backfill at the same clock.
    const TYPED: TableSpec = TableSpec {
        name: "t_typed",
        scope_id: "demo/1",
        spec_version: 2,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[
            ColumnSpec::added("flag", ValueType::Bool, 2, DefaultValue::Bool(true)),
            ColumnSpec::added("count", ValueType::I64, 2, DefaultValue::I64(7)),
            ColumnSpec::added("note", ValueType::Text, 2, DefaultValue::Text("d")),
            ColumnSpec::added("raw", ValueType::Blob, 2, DefaultValue::Blob(&[1, 2])),
            ColumnSpec::added("empty", ValueType::Text, 2, DefaultValue::Null),
        ],
        local_columns: &[],
        repo_column: None,
    };
    assert_eq!(
        project_cells(&TYPED, 1, &[]),
        Projection::Complete(vec![
            ("flag", TypedValue::Bool(true)),
            ("count", TypedValue::I64(7)),
            ("note", TypedValue::Text("d".into())),
            ("raw", TypedValue::Blob(vec![1, 2])),
            ("empty", TypedValue::Null),
        ]),
        "each declared default fills as its own typed value, not as NULL"
    );
}

#[test]
fn the_default_fill_window_is_per_column_not_merely_older_than_the_spec() {
    // Once a table reaches a THIRD version, "older than the current spec" stops being a safe
    // test for "predates this column". A v2 op that omits a column v2 already had is a broken
    // partial, and must park — filling it would reset that column to its default for EVERY
    // receiver under whole-row LWW, silently and with no local edit to signal it. Only an op
    // older than the column's OWN introducing version may be filled.
    const THREE: TableSpec = TableSpec {
        name: "t_three",
        scope_id: "demo/1",
        spec_version: 3,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("v2-default")),
            ColumnSpec::added("latest", ValueType::Text, 3, DefaultValue::Text("v3-default")),
        ],
        local_columns: &[],
        repo_column: None,
    };
    let project = |version: u32, cells: &[(&str, &str)]| {
        let cells: Vec<Cell> = cells
            .iter()
            .map(|(c, v)| Cell {
                column: (*c).to_string(),
                value: TypedValue::Text((*v).to_string()),
            })
            .collect();
        project_cells(&THREE, version, &cells)
    };

    // v1 predates BOTH added columns — each is filled from its own declared default.
    assert_eq!(
        project(1, &[("title", "t")]),
        Projection::Complete(vec![
            ("title", TypedValue::Text("t".into())),
            ("later", TypedValue::Text("v2-default".into())),
            ("latest", TypedValue::Text("v3-default".into())),
        ]),
        "an op older than both columns is completed from both defaults"
    );

    // v2 predates only `latest`; `later` existed in v2, so a v2 op carrying it is complete.
    assert_eq!(
        project(2, &[("title", "t"), ("later", "sent")]),
        Projection::Complete(vec![
            ("title", TypedValue::Text("t".into())),
            ("later", TypedValue::Text("sent".into())),
            ("latest", TypedValue::Text("v3-default".into())),
        ]),
        "only the column the op's own version predates is filled"
    );

    // THE REGRESSION: a v2 op omitting `later` is a broken partial, not an old complete row.
    assert_eq!(
        project(2, &[("title", "t")]),
        Projection::Park(PendingReason::PartialAfterImage),
        "a column the op's own version already had must never be defaulted — the op is a partial \
         and parks"
    );
}

#[test]
fn an_unknown_column_parks_the_whole_op_and_writes_nothing() {
    // A newer producer's column: this op cannot become a COMPLETE after-image here, so nothing
    // is written and the entry is left for the refold. Applying the known cell instead (the
    // pre-#1001 behavior) would leave a row no device authored and — once this binary learned
    // the column — re-author it with the hole at a winning lamport, destroying the real value
    // on every peer.
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let out = apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[
            ("title", TypedValue::Text("kept".into())),
            ("future_col", TypedValue::Text("dropped".into())),
        ]),
        OpMeta { lamport: 1, device: device(2) },
    )
    .unwrap();
    assert_eq!(out, ApplyOutcome::Unprojectable(PendingReason::UnknownColumn));

    let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
    assert!(published_hash(&tx, "repo", "t_demo", &row_pk).unwrap().is_none());
    assert!(current_row_clock(&tx, "repo", "t_demo", &row_pk).unwrap().is_none());
    tx.commit().unwrap();
    assert_eq!(title(&c), None, "a parked op leaves the table untouched");
}

#[test]
fn an_unknown_column_parks_without_disturbing_the_row_it_would_have_replaced() {
    // The dangerous variant: the row already exists from an earlier, fully-understood entry.
    // The parked op must not partially overwrite it, and must not touch its clock or
    // published hash — the existing row stays exactly the complete after-image its
    // author signed.
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("v1".into()))]),
        OpMeta { lamport: 5, device: device(2) },
    )
    .unwrap();
    let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
    let published_before = published_hash(&tx, "repo", "t_demo", &row_pk).unwrap();

    // A LATER op (higher lamport, would otherwise win) carrying an unknown column.
    let out = apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[
            ("title", TypedValue::Text("v2".into())),
            ("future_col", TypedValue::Text("unknown".into())),
        ]),
        OpMeta { lamport: 9, device: device(2) },
    )
    .unwrap();
    assert_eq!(out, ApplyOutcome::Unprojectable(PendingReason::UnknownColumn));
    assert_eq!(
        current_row_clock(&tx, "repo", "t_demo", &row_pk).unwrap().unwrap().0,
        5,
        "a parked op does not advance the row clock"
    );
    assert_eq!(
        published_hash(&tx, "repo", "t_demo", &row_pk).unwrap(),
        published_before,
        "a parked op does not touch the anti-echo record"
    );
    tx.commit().unwrap();
    assert_eq!(title(&c).as_deref(), Some("v1"), "the previous complete row survives intact");
}

#[test]
fn a_published_hash_carries_the_column_set_it_covers() {
    // The hash alone is ambiguous across column-set changes, so the version it was recorded
    // under is stored with it. `produce` relies on this to tell "changed locally" from
    // "hashed under a different column set" (see `super::produce`).
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply_row_op(&tx, &SPEC, "repo", &upsert(&[("title", TypedValue::Text("v".into()))]), OpMeta {
        lamport: 1,
        device: device(2),
    })
    .unwrap();
    let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
    let (_, version) = published_hash(&tx, "repo", "t_demo", &row_pk).unwrap().unwrap();
    assert_eq!(version, SPEC.spec_version);
}

#[test]
fn a_remove_deletes_the_row_and_its_bookkeeping() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply_row_op(&tx, &SPEC, "repo", &upsert(&[("title", TypedValue::Text("x".into()))]), OpMeta {
        lamport: 1,
        device: device(2),
    })
    .unwrap();
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &RowOp::Remove {
            spec_version: 1,
            table: "t_demo".into(),
            pk: vec![TypedValue::Text("r1".into())],
        },
        OpMeta { lamport: 2, device: device(2) },
    )
    .unwrap();
    let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
    assert!(published_hash(&tx, "repo", "t_demo", &row_pk).unwrap().is_none());
    assert!(current_row_clock(&tx, "repo", "t_demo", &row_pk).unwrap().is_none());
    tx.commit().unwrap();
    assert_eq!(title(&c), None);
}

fn remove() -> RowOp {
    RowOp::Remove {
        spec_version: 1,
        table: "t_demo".to_string(),
        pk: vec![TypedValue::Text("r1".to_string())],
    }
}

#[test]
fn a_stale_remove_after_a_newer_upsert_does_not_delete() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("keep".into()))]),
        OpMeta { lamport: 5, device: device(2) },
    )
    .unwrap();
    // A delete older than the row's cell clock loses — the row survives.
    let out = apply_row_op(&tx, &SPEC, "repo", &remove(), OpMeta { lamport: 3, device: device(2) })
        .unwrap();
    assert_eq!(
        out,
        ApplyOutcome::Superseded,
        "the delete landed but did not delete: reported distinctly from a delete that took \
         effect, because a locally-authored op can never legitimately land here"
    );
    tx.commit().unwrap();
    assert_eq!(title(&c).as_deref(), Some("keep"), "a stale delete cannot remove a newer row");
}

#[test]
fn an_upsert_older_than_a_remove_cannot_resurrect() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply_row_op(&tx, &SPEC, "repo", &remove(), OpMeta { lamport: 5, device: device(2) }).unwrap();
    // An insert older than the tombstone is suppressed.
    let out = apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("ghost".into()))]),
        OpMeta { lamport: 3, device: device(2) },
    )
    .unwrap();
    assert_eq!(out, ApplyOutcome::Superseded, "suppressed by the tombstone, not applied");
    tx.commit().unwrap();
    assert_eq!(title(&c), None, "an insert older than the delete cannot resurrect the row");
}

#[test]
fn an_upsert_newer_than_a_remove_resurrects() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply_row_op(&tx, &SPEC, "repo", &remove(), OpMeta { lamport: 3, device: device(2) }).unwrap();
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("back".into()))]),
        OpMeta { lamport: 5, device: device(2) },
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(title(&c).as_deref(), Some("back"), "an insert newer than the delete resurrects");
}

#[test]
fn a_remove_and_upsert_converge_regardless_of_arrival_order() {
    let end_state = |ops: &[(RowOp, OpMeta)]| {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        for (op, meta) in ops {
            apply_row_op(&tx, &SPEC, "repo", op, *meta).unwrap();
        }
        tx.commit().unwrap();
        title(&c)
    };
    let up = (upsert(&[("title", TypedValue::Text("v".into()))]), OpMeta {
        lamport: 5,
        device: device(2),
    });
    let rm = (remove(), OpMeta { lamport: 3, device: device(2) });
    assert_eq!(
        end_state(&[up.clone(), rm.clone()]),
        end_state(&[rm, up]),
        "the fold converges regardless of the order the delete and edit arrive",
    );
}

#[test]
fn a_losing_upsert_does_not_publish_an_unsent_local_edit() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    // Establish + publish a row at lamport 5 (as authoring would).
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("v1".into()))]),
        OpMeta { lamport: 5, device: device(2) },
    )
    .unwrap();
    let row_pk = row_op::row_pk_string(&[TypedValue::Text("r1".to_string())]);
    let published_before = published_hash(&tx, "repo", "t_demo", &row_pk).unwrap();
    // A local direct edit (no op authored yet): the row changes but stays unpublished.
    tx.execute("UPDATE t_demo SET title = 'local-edit' WHERE id = 'r1'", []).unwrap();
    // A STALE remote upsert (lower lamport) loses the LWW; it must NOT publish the local edit.
    apply_row_op(
        &tx,
        &SPEC,
        "repo",
        &upsert(&[("title", TypedValue::Text("stale".into()))]),
        OpMeta { lamport: 3, device: device(9) },
    )
    .unwrap();
    assert_eq!(
        published_hash(&tx, "repo", "t_demo", &row_pk).unwrap(),
        published_before,
        "a losing op must not advance the published hash over an unsent local edit",
    );
    let current = synced_row_hash(&tx, &SPEC, &[TypedValue::Text("r1".into())]).unwrap();
    assert_ne!(
        current,
        published_before.map(|(hash, _version)| hash),
        "the local edit is still pending, not silently dropped"
    );
}

#[test]
fn a_delete_races_the_row_write_clock_on_a_content_addressed_row() {
    // A content-hash-keyed row (the shape the old insert-only flag targeted) records a row
    // write clock like any other, so a stale delete loses and a newer one wins.
    const HASHED: TableSpec = TableSpec {
        name: "t_io",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("hash", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch("CREATE TABLE t_io(id TEXT PRIMARY KEY, hash TEXT) STRICT;").unwrap();
    let tx = c.transaction().unwrap();
    let io_upsert = RowOp::Upsert {
        spec_version: 1,
        table: "t_io".to_string(),
        pk: vec![TypedValue::Text("r".into())],
        cells: vec![Cell { column: "hash".into(), value: TypedValue::Text("h".into()) }],
    };
    let io_remove = RowOp::Remove {
        spec_version: 1,
        table: "t_io".to_string(),
        pk: vec![TypedValue::Text("r".into())],
    };

    apply_row_op(&tx, &HASHED, "repo", &io_upsert, OpMeta { lamport: 5, device: device(2) })
        .unwrap();
    // A stale remove (lamport 3) must NOT delete the newer row.
    apply_row_op(&tx, &HASHED, "repo", &io_remove, OpMeta { lamport: 3, device: device(2) })
        .unwrap();
    let count: i64 =
        tx.query_row("SELECT COUNT(*) FROM t_io WHERE id = 'r'", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1, "a stale delete cannot drop a newer row");
    // A newer remove (lamport 7) does delete it.
    apply_row_op(&tx, &HASHED, "repo", &io_remove, OpMeta { lamport: 7, device: device(2) })
        .unwrap();
    let after: i64 =
        tx.query_row("SELECT COUNT(*) FROM t_io WHERE id = 'r'", [], |r| r.get(0)).unwrap();
    assert_eq!(after, 0, "a delete newer than the row's write clock removes it");
}

#[test]
fn an_op_naming_a_foreign_repo_is_quarantined() {
    const SCOPED: TableSpec = TableSpec {
        name: "t_scoped",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[
            ColumnSpec::required("repo_id", ValueType::Text),
            ColumnSpec::required("id", ValueType::Text),
        ],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &[],
        repo_column: Some("repo_id"),
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch(
        "CREATE TABLE t_scoped(
                 repo_id TEXT NOT NULL, id TEXT NOT NULL, title TEXT, PRIMARY KEY(repo_id, id)
             ) STRICT;",
    )
    .unwrap();
    let tx = c.transaction().unwrap();
    let foreign = RowOp::Upsert {
        spec_version: 1,
        table: "t_scoped".to_string(),
        pk: vec![TypedValue::Text("B".into()), TypedValue::Text("r1".into())],
        cells: vec![Cell { column: "title".into(), value: TypedValue::Text("x".into()) }],
    };
    // Applied on repo A's stream but naming repo B → rejected, nothing written.
    let out = apply_row_op(&tx, &SCOPED, "A", &foreign, OpMeta { lamport: 1, device: device(2) })
        .unwrap();
    assert!(matches!(out, ApplyOutcome::Quarantined { .. }), "a foreign-repo op is rejected");
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_scoped", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0, "no cross-repo row was written");
    // The matching-repo op applies.
    let own = RowOp::Upsert {
        spec_version: 1,
        table: "t_scoped".to_string(),
        pk: vec![TypedValue::Text("A".into()), TypedValue::Text("r1".into())],
        cells: vec![Cell { column: "title".into(), value: TypedValue::Text("x".into()) }],
    };
    assert_eq!(
        apply_row_op(&tx, &SCOPED, "A", &own, OpMeta { lamport: 2, device: device(2) }).unwrap(),
        ApplyOutcome::Applied,
    );
}

#[test]
fn a_null_primary_key_is_quarantined() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let op = RowOp::Upsert {
        spec_version: 1,
        table: "t_demo".to_string(),
        pk: vec![TypedValue::Null],
        cells: vec![Cell { column: "title".to_string(), value: TypedValue::Text("x".into()) }],
    };
    let out =
        apply_row_op(&tx, &SPEC, "repo", &op, OpMeta { lamport: 1, device: device(2) }).unwrap();
    assert!(matches!(out, ApplyOutcome::Quarantined { .. }), "a null pk is not addressable");
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_demo", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0, "a quarantined null-pk op writes nothing");
}

#[test]
fn a_pk_value_of_the_wrong_type_is_quarantined() {
    // `t_demo`'s pk `id` is declared Text. An I64 pk would take SQLite affinity onto a
    // different physical row than its type-exact `row_pk` clock identity — quarantine
    // before the WHERE.
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let op = RowOp::Upsert {
        spec_version: 1,
        table: "t_demo".to_string(),
        pk: vec![TypedValue::I64(1)],
        cells: vec![Cell { column: "title".to_string(), value: TypedValue::Text("x".into()) }],
    };
    let out =
        apply_row_op(&tx, &SPEC, "repo", &op, OpMeta { lamport: 1, device: device(2) }).unwrap();
    assert!(
        matches!(out, ApplyOutcome::Quarantined { .. }),
        "a pk value that disagrees with its declared type is quarantined"
    );
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_demo", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0, "a quarantined type-mismatched-pk op writes nothing");
}

#[test]
fn a_null_in_a_not_null_column_is_quarantined_not_a_fatal_error() {
    // A NOT NULL constraint violation on a synced column must surface as a quarantine (the
    // entry is retained, the row untouched), never a hard error that would wedge the
    // ingest loop.
    const NOT_NULL: TableSpec = TableSpec {
        name: "t_nn",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch("CREATE TABLE t_nn(id TEXT PRIMARY KEY, title TEXT NOT NULL) STRICT;").unwrap();
    let tx = c.transaction().unwrap();
    // A well-typed NULL cell (Text column, Null value) passes the type check but violates the
    // table's NOT NULL constraint on insert.
    let op = RowOp::Upsert {
        spec_version: 1,
        table: "t_nn".to_string(),
        pk: vec![TypedValue::Text("r".into())],
        cells: vec![Cell { column: "title".to_string(), value: TypedValue::Null }],
    };
    let out = apply_row_op(&tx, &NOT_NULL, "repo", &op, OpMeta { lamport: 1, device: device(2) })
        .unwrap();
    assert!(
        matches!(out, ApplyOutcome::Quarantined { .. }),
        "a constraint violation is quarantined, not a fatal error"
    );
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM t_nn", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0, "a quarantined constraint-violating op writes nothing");
}

#[test]
fn the_producer_reads_a_bool_pk_as_bool_and_the_applier_accepts_it() {
    // A `Bool` pk is stored as INTEGER 0/1. The producer must emit `TypedValue::Bool` (not
    // `I64`), or the op it signs fails the applier's typed-pk check and self-quarantines.
    const FLAG: TableSpec = TableSpec {
        name: "t_flag",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("active", ValueType::Bool)],
        columns: &[ColumnSpec::required("label", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let mut src = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&src, &crate::test_hooks()).unwrap();
    src.execute_batch("CREATE TABLE t_flag(active INTEGER PRIMARY KEY, label TEXT) STRICT;")
        .unwrap();
    src.execute("INSERT INTO t_flag(active, label) VALUES (1, 'on')", []).unwrap();
    let src_tx = src.transaction().unwrap();
    let rows = read_all_rows(&src_tx, &FLAG, "repo").unwrap();
    assert_eq!(rows.len(), 1);
    let ScannedRow::Readable { pk, cells } = &rows[0] else {
        panic!("a 0/1 Bool pk is readable");
    };
    assert_eq!(pk, &vec![TypedValue::Bool(true)], "a Bool pk is emitted as Bool, not I64");

    // The op the producer would sign applies cleanly on a peer (the typed-pk check passes).
    let op = RowOp::Upsert {
        spec_version: 1,
        table: "t_flag".to_string(),
        pk: pk.clone(),
        cells: cells.clone(),
    };
    let mut peer = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&peer, &crate::test_hooks()).unwrap();
    peer.execute_batch("CREATE TABLE t_flag(active INTEGER PRIMARY KEY, label TEXT) STRICT;")
        .unwrap();
    let peer_tx = peer.transaction().unwrap();
    assert_eq!(
        apply_row_op(&peer_tx, &FLAG, "repo", &op, OpMeta { lamport: 1, device: device(2) })
            .unwrap(),
        ApplyOutcome::Applied,
        "the applier accepts the Bool-pk op the producer emits",
    );
}

/// A table whose only synced column is a `Bool`, plus a store holding one row of it at `flag`.
/// STRICT keeps every other column mapping total, so this is the one shape that can be
/// unreadable (#1017).
const FLAGGED: TableSpec = TableSpec {
    name: "t_flagged",
    scope_id: "demo/1",
    spec_version: 1,
    pk: &[ColumnSpec::required("id", ValueType::Text)],
    columns: &[ColumnSpec::required("flag", ValueType::Bool)],
    local_columns: &[],
    repo_column: None,
};

fn flagged_store(flag: i64) -> rusqlite::Connection {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch("CREATE TABLE t_flagged(id TEXT PRIMARY KEY, flag INTEGER) STRICT;").unwrap();
    c.execute("INSERT INTO t_flagged(id, flag) VALUES ('r', ?1)", [flag]).unwrap();
    c
}

#[test]
fn a_bool_column_holding_a_non_boolean_int_is_unreadable_not_normalized() {
    // Coercing 2 to `true` would replicate a value that differs from the stored row. Reporting
    // it as unreadable is the alternative — and it must stay a VALUE, because every reader here
    // runs under a path that cannot fail (#1017).
    let mut c = flagged_store(2);
    let tx = c.transaction().unwrap();

    let rows = read_all_rows(&tx, &FLAGGED, "repo").unwrap();
    assert_eq!(rows.len(), 1, "the row is still scanned, not dropped or errored");
    match &rows[0] {
        ScannedRow::Unpublishable { pk } => {
            assert_eq!(pk, &vec![TypedValue::Text("r".into())], "it stays addressable")
        },
        _ => panic!("a Bool column holding 2 makes the row unpublishable, never `true`"),
    }
}

#[test]
fn an_unreadable_row_is_distinguished_from_an_absent_one() {
    // The load-bearing distinction: the refold's guard reads `Absent` as a local delete
    // awaiting authorship and refuses to replay over it, so collapsing the two would
    // block the entry for a row that is merely unreadable.
    let mut c = flagged_store(2);
    let tx = c.transaction().unwrap();
    let present = read_synced_cells(&tx, &FLAGGED, &[TypedValue::Text("r".into())]).unwrap();
    assert!(matches!(present, SyncedRow::Unreadable(_)), "a present-but-unreadable row");

    let missing = read_synced_cells(&tx, &FLAGGED, &[TypedValue::Text("nope".into())]).unwrap();
    assert!(matches!(missing, SyncedRow::Absent), "and a genuinely absent one");
}

#[test]
fn a_bool_pk_holding_a_non_boolean_int_leaves_the_row_unaddressable() {
    // The pk case is separate because the producer must treat it differently: with no readable
    // pk the row has no identity at all, so there is nothing to keep alive.
    const FLAG_PK: TableSpec = TableSpec {
        name: "t_flag",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("active", ValueType::Bool)],
        columns: &[ColumnSpec::required("label", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch("CREATE TABLE t_flag(active INTEGER PRIMARY KEY, label TEXT) STRICT;").unwrap();
    c.execute("INSERT INTO t_flag(active, label) VALUES (2, 'on')", []).unwrap();
    let tx = c.transaction().unwrap();

    let rows = read_all_rows(&tx, &FLAG_PK, "repo").unwrap();
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0], ScannedRow::Unaddressable), "no pk means no identity");
}

/// A stable stream id for the guard tests, which never reach the winner lookup.
fn guard_stream() -> StreamId {
    crate::table_sync::scope_stream::scope_stream_id(
        "repo",
        crate::AccountId::from_bytes([7; 32]),
        [0x44; 32],
        "demo/1",
    )
}

/// An op of each kind against the `FLAGGED` row, for the asymmetry below.
fn flagged_op(remove: bool) -> RowOp {
    let pk = vec![TypedValue::Text("r".into())];
    if remove {
        RowOp::Remove { table: "t_flagged".to_string(), spec_version: 1, pk }
    } else {
        RowOp::Upsert {
            table: "t_flagged".to_string(),
            spec_version: 1,
            pk,
            cells: vec![Cell { column: "flag".to_string(), value: TypedValue::Bool(true) }],
        }
    }
}

#[test]
fn an_upsert_may_replay_over_a_row_it_cannot_read() {
    // Both readers of "is there unsent work here" must not defer: the producer cannot author an
    // unreadable row either, so deferring every op would leave the row unauthorable AND
    // permanently block its own pending entries. The upsert has a floor — it still has to win
    // on the clock, and a winner rewrites the column that is unreadable.
    let mut c = flagged_store(2);
    let tx = c.transaction().unwrap();
    assert_eq!(
        unsent_work_blocking_replay(&tx, &FLAGGED, "repo", guard_stream(), &flagged_op(false))
            .unwrap(),
        None,
        "an unreadable row proves nothing against an upsert that would repair it",
    );
}

#[test]
fn a_remove_may_not_replay_over_a_row_it_cannot_read() {
    // The asymmetry. A remove deletes the row outright — local-only columns included — and
    // repairs nothing, so letting it through would destroy an unsent local edit that merely
    // happens to be unreadable. Deferring is the safe stuck state: the row survives, and the
    // entry replays on the merits once the cell is repaired.
    let mut c = flagged_store(2);
    let tx = c.transaction().unwrap();
    assert_eq!(
        unsent_work_blocking_replay(&tx, &FLAGGED, "repo", guard_stream(), &flagged_op(true))
            .unwrap(),
        Some(PendingReason::DeferredUnreadableRow),
        "a remove over an unreadable row has no repair to offer, so it must defer",
    );
}

#[test]
fn a_text_column_holding_invalid_utf8_is_unreadable_too() {
    // STRICT pins the storage CLASS, not the value's domain within it: `CAST(X'80' AS TEXT)`
    // stores with `typeof() = 'text'` and fails the moment it is read as a `String`. Reading it
    // as an error would fail the store open exactly the way a malformed Bool used to, so the
    // mapping is total over (declared type, storage class) rather than argued from STRICT.
    const LABELLED: TableSpec = TableSpec {
        name: "t_labelled",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("label", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch(
        "CREATE TABLE t_labelled(id TEXT PRIMARY KEY, label TEXT) STRICT;
             INSERT INTO t_labelled(id, label) VALUES ('r', CAST(X'80' AS TEXT));",
    )
    .unwrap();
    let tx = c.transaction().unwrap();

    let cells = read_synced_cells(&tx, &LABELLED, &[TypedValue::Text("r".into())]).unwrap();
    assert!(matches!(cells, SyncedRow::Unreadable(_)), "invalid UTF-8 is unreadable, not an error");
    let rows = read_all_rows(&tx, &LABELLED, "repo").unwrap();
    assert!(
        matches!(&rows[0], ScannedRow::Unpublishable { .. }),
        "and the scan carries the row rather than failing the pass",
    );
}

#[test]
fn a_storage_class_that_does_not_match_the_declared_type_is_unreadable() {
    // Unreachable on a STRICT table — which is exactly the argument that let the previous
    // version fail an open, so the mapping covers it as a value instead of assuming it away.
    const LABELLED: TableSpec = TableSpec {
        name: "t_loose",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("label", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    // No STRICT here: that is what lets an INTEGER land in a column declared TEXT.
    c.execute_batch(
        "CREATE TABLE t_loose(id TEXT PRIMARY KEY, label BLOB);
             INSERT INTO t_loose(id, label) VALUES ('r', 7);",
    )
    .unwrap();
    let tx = c.transaction().unwrap();
    assert!(
        matches!(
            read_synced_cells(&tx, &LABELLED, &[TypedValue::Text("r".into())]).unwrap(),
            SyncedRow::Unreadable(_)
        ),
        "a mismatched storage class is carried, not raised",
    );
}

#[test]
fn a_remove_blocked_by_a_foreign_key_is_quarantined_not_wedged() {
    // A delete that hits an FK RESTRICT (a child references the row) must quarantine — NOT
    // return a hard error, which would roll back the already-stored entry and wedge the chain.
    const PARENT: TableSpec = TableSpec {
        name: "parent",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
    c.execute_batch(
        "PRAGMA foreign_keys = ON;
             CREATE TABLE parent(id TEXT NOT NULL PRIMARY KEY, v TEXT) STRICT;
             CREATE TABLE child(id TEXT NOT NULL PRIMARY KEY, p TEXT NOT NULL REFERENCES \
         parent(id)) STRICT;
             INSERT INTO parent(id, v) VALUES ('r', 'x');
             INSERT INTO child(id, p) VALUES ('c', 'r');",
    )
    .unwrap();
    let tx = c.transaction().unwrap();
    let op = RowOp::Remove {
        spec_version: 1,
        table: "parent".to_string(),
        pk: vec![TypedValue::Text("r".into())],
    };
    let out =
        apply_row_op(&tx, &PARENT, "repo", &op, OpMeta { lamport: 1, device: device(2) }).unwrap();
    assert!(
        matches!(out, ApplyOutcome::Quarantined { .. }),
        "an FK-blocked delete quarantines, it does not error: {out:?}"
    );
    let n: i64 =
        tx.query_row("SELECT COUNT(*) FROM parent WHERE id = 'r'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "the FK-blocked parent row is left untouched");
}

// ── restatements (#1295) ─────────────────────────────────────────────────────────────────────

fn stated(id: &str, seed: u8, lamport: u64) -> StatedDelete {
    StatedDelete { pk: vec![TypedValue::Text(id.to_string())], device: device(seed), lamport }
}

fn restate(deletes: Vec<StatedDelete>) -> RowOp {
    RowOp::Restate { spec_version: 1, table: "t_demo".to_string(), deletes }
}

fn apply(tx: &Transaction<'_>, op: &RowOp, lamport: u64, seed: u8) -> ApplyOutcome {
    apply_row_op(tx, &SPEC, "repo", op, OpMeta { lamport, device: device(seed) }).unwrap()
}

fn r1_key() -> String {
    row_op::row_pk_string(&[TypedValue::Text("r1".to_string())])
}

fn statement(tx: &Transaction<'_>, seed: u8) -> Option<u64> {
    let row_pk = r1_key();
    let key = RowKey {
        stream: StreamId::from_bytes([0; 32]),
        repo_id: "repo",
        table: "t_demo",
        row_pk: &row_pk,
    };
    statement_on_stream(tx, &key, &device(seed).to_string()).unwrap()
}

fn tombstone(tx: &Transaction<'_>) -> Option<(u64, String)> {
    let row_pk = r1_key();
    let key = RowKey {
        stream: StreamId::from_bytes([0; 32]),
        repo_id: "repo",
        table: "t_demo",
        row_pk: &row_pk,
    };
    current_tombstone(tx, &key).unwrap()
}

/// A stated delete settles at ITS identity exactly as the `Remove` that first stated it would:
/// it wins a row written before it, is idempotent, keeps a row written after it, is a no-op once
/// a newer delete owns the row — and moves only the signer's statement, never the identity.
#[test]
fn a_restated_delete_settles_exactly_like_its_remove() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply(&tx, &upsert(&[("title", TypedValue::Text("v1".to_string()))]), 3, 2);

    // Device 5 restates device 2's delete at lamport 5, signed at 9: the delete beats the write.
    let op = restate(vec![stated("r1", 2, 5)]);
    assert_eq!(apply(&tx, &op, 9, 5), ApplyOutcome::Applied);
    assert_eq!(title(&tx), None, "the delete wins the row written before it");
    assert_eq!(tombstone(&tx), Some((5, device(2).to_string())), "identity is the delete's");
    assert_eq!(statement(&tx, 5), Some(9), "the signer states it at its own entry");
    assert_eq!(statement(&tx, 2), None, "the identity's device never stated it here");
    assert!(current_row_clock(&tx, "repo", "t_demo", &r1_key()).unwrap().is_none());

    assert_eq!(apply(&tx, &op, 9, 5), ApplyOutcome::Superseded, "a redelivery changes nothing");

    // A write after the delete resurrects the row; the same stated delete now keeps it, but the
    // signer's statement of the (still current) identity advances.
    assert_eq!(
        apply(&tx, &upsert(&[("title", TypedValue::Text("v2".to_string()))]), 7, 2),
        ApplyOutcome::Applied
    );
    assert_eq!(apply(&tx, &op, 11, 5), ApplyOutcome::Applied);
    assert_eq!(title(&tx).as_deref(), Some("v2"), "a newer write keeps the row");
    assert_eq!(statement(&tx, 5), Some(11));

    // A newer delete takes the identity, and the old identity's statements go with it.
    assert_eq!(apply(&tx, &remove(), 12, 3), ApplyOutcome::Applied);
    assert_eq!(tombstone(&tx), Some((12, device(3).to_string())));
    assert_eq!(statement(&tx, 5), None, "statements of the outranked identity are gone");
    assert_eq!(statement(&tx, 3), Some(12));

    // Restating the outranked identity is a no-op: nothing moves, and the signer gains nothing.
    assert_eq!(apply(&tx, &op, 13, 5), ApplyOutcome::Superseded);
    assert_eq!(statement(&tx, 5), None);
}

/// The guard asks a restatement only about the rows it would physically remove: an unpublished
/// local row a stated delete would destroy defers the entry, exactly as a `Remove` of it would;
/// a stated delete whose row is absent, or whose clock beats it, cannot park anything.
#[test]
fn a_restate_is_deferred_only_by_unsent_work_on_a_row_it_would_change() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let stream = StreamId::from_bytes([0; 32]);
    // r1: a raw local row nothing has published — the delete would remove it.
    tx.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'unsent')", []).unwrap();
    let over_r1 = restate(vec![stated("r1", 2, 5)]);
    assert_eq!(
        unsent_work_blocking_replay(&tx, &SPEC, "repo", stream, &over_r1).unwrap(),
        Some(PendingReason::DeferredUnsentEdit)
    );
    assert_eq!(
        pre_apply(&tx, &SPEC, "repo", stream, &over_r1, 20, RowDoubt::DeferOnAnyDoubt).unwrap(),
        PreApply::Park(PendingReason::DeferredUnsentEdit)
    );
    // r2: absent — nothing to destroy, whatever the tombstone table says.
    let over_r2 = restate(vec![stated("r2", 2, 5)]);
    assert_eq!(unsent_work_blocking_replay(&tx, &SPEC, "repo", stream, &over_r2).unwrap(), None);
    // r1 again, but now published under a clock the stated delete cannot beat: not touched.
    apply(&tx, &upsert(&[("title", TypedValue::Text("v9".to_string()))]), 9, 2);
    assert_eq!(unsent_work_blocking_replay(&tx, &SPEC, "repo", stream, &over_r1).unwrap(), None);
    assert_eq!(apply(&tx, &over_r1, 20, 5), ApplyOutcome::Applied, "the tombstone is still raised");
    assert_eq!(title(&tx).as_deref(), Some("v9"));
}

/// A stated lamport at or above the carrying entry's own is a delete identity the signer's chain
/// never held; the wire cannot see it, so the applier quarantines the entry with no effect.
#[test]
fn accept_and_replay_reject_a_stated_lamport_at_or_above_the_entrys() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply(&tx, &upsert(&[("title", TypedValue::Text("v1".to_string()))]), 3, 2);
    let op = restate(vec![stated("r1", 2, 5)]);
    let ApplyOutcome::Quarantined { why, .. } = apply(&tx, &op, 5, 5) else {
        panic!("quarantined")
    };
    assert!(why.contains("at or above its own lamport"), "{why}");
    assert_eq!(title(&tx).as_deref(), Some("v1"), "nothing was written");
    assert_eq!(tombstone(&tx), None);
}

/// One malformed element quarantines the whole batch before anything is written.
#[test]
fn an_invalid_element_quarantines_the_batch_without_partial_effects() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    apply(&tx, &upsert(&[("title", TypedValue::Text("v1".to_string()))]), 3, 2);
    let bad = StatedDelete { pk: vec![TypedValue::I64(1)], device: device(2), lamport: 4 };
    let op = restate(vec![stated("r1", 2, 5), bad]);
    assert!(matches!(apply(&tx, &op, 9, 5), ApplyOutcome::Quarantined { .. }));
    assert_eq!(title(&tx).as_deref(), Some("v1"), "the valid element did not apply either");
    assert_eq!(tombstone(&tx), None);
    assert_eq!(statement(&tx, 5), None);
}

/// A constraint failure on one stated delete quarantines the entry AFTER every other row settled,
/// and the outcome says whether anything moved: true the first time (the sibling delete raised
/// its tombstone), false on a redelivery (nothing left to settle), which is what lets a caller
/// sweep once and never again.
#[test]
fn a_quarantined_restate_settles_its_other_rows_and_reports_whether_anything_moved() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    for id in ["r1", "r2"] {
        tx.execute("INSERT INTO t_demo(id, title) VALUES (?1, 'v')", [id]).unwrap();
    }
    tx.execute_batch(
        "CREATE TRIGGER t_demo_keep_r2 BEFORE DELETE ON t_demo WHEN OLD.id = 'r2'
         BEGIN SELECT RAISE(ABORT, 'r2 is kept'); END;",
    )
    .unwrap();
    let op = restate(vec![stated("r1", 2, 5), stated("r2", 2, 6)]);
    assert!(matches!(apply(&tx, &op, 9, 5), ApplyOutcome::Quarantined { changed: true, .. }));
    assert_eq!(title(&tx), None, "r1 settled although r2 could not");
    assert_eq!(tombstone(&tx), Some((5, device(2).to_string())));
    assert_eq!(statement(&tx, 5), Some(9));
    let kept: i64 =
        tx.query_row("SELECT COUNT(*) FROM t_demo WHERE id = 'r2'", [], |r| r.get(0)).unwrap();
    assert_eq!(kept, 1, "the failing delete left its row and raised no tombstone");
    assert!(matches!(apply(&tx, &op, 9, 5), ApplyOutcome::Quarantined { changed: false, .. }));
}

/// The batch is exempted or deferred as a whole, so the verdict is the STRONGEST blocker over
/// every stated row: an unprovable verdict on one row (a stale-version publication whose winning
/// entry is gone) must not hide a later row's proven unsent edit, which the removal exemption
/// would otherwise apply straight over.
#[test]
fn a_proven_unsent_edit_on_one_stated_row_outranks_an_unprovable_verdict_on_another() {
    let mut c = conn();
    let tx = c.transaction().unwrap();
    let stream = StreamId::from_bytes([0; 32]);
    // a1: published under an older column set, with a clock whose entry is not retained — the
    // guard cannot prove anything about it.
    tx.execute("INSERT INTO t_demo(id, title) VALUES ('a1', 'old')", []).unwrap();
    let a1 = row_op::row_pk_string(&[TypedValue::Text("a1".to_string())]);
    record_published(&tx, stream, "repo", "t_demo", &a1, "stale-hash", 0).unwrap();
    tx.execute(
        "INSERT INTO sync_row_clocks(
             stream_id, repo_id, table_name, row_pk, lamport, device_fingerprint)
         VALUES (?1, 'repo', 't_demo', ?2, 1, ?3)",
        rusqlite::params![stream.to_bytes().as_slice(), a1, device(2).to_string()],
    )
    .unwrap();
    // b1: a raw local row nothing has published — a proven unsent edit.
    tx.execute("INSERT INTO t_demo(id, title) VALUES ('b1', 'unsent')", []).unwrap();
    let op = restate(vec![stated("a1", 2, 5), stated("b1", 2, 6)]);
    assert_eq!(
        unsent_work_blocking_replay(&tx, &SPEC, "repo", stream, &op).unwrap(),
        Some(PendingReason::DeferredUnsentEdit),
        "the proven blocker wins over a1's unresolved winner"
    );
    assert_eq!(
        pre_apply(&tx, &SPEC, "repo", stream, &op, 9, RowDoubt::DeferExceptUnprovableRemoval)
            .unwrap(),
        PreApply::Park(PendingReason::DeferredUnsentEdit),
        "and the removal exemption does not apply"
    );
    // With b1 published, only the unprovable verdict remains and the exemption applies.
    let b1 = row_op::row_pk_string(&[TypedValue::Text("b1".to_string())]);
    let hash = synced_row_hash(&tx, &SPEC, &[TypedValue::Text("b1".to_string())]).unwrap().unwrap();
    record_published(&tx, stream, "repo", "t_demo", &b1, &hash, SPEC.spec_version).unwrap();
    assert_eq!(
        unsent_work_blocking_replay(&tx, &SPEC, "repo", stream, &op).unwrap(),
        Some(PendingReason::DeferredUnresolvedWinner)
    );
    assert_eq!(
        pre_apply(&tx, &SPEC, "repo", stream, &op, 9, RowDoubt::DeferExceptUnprovableRemoval)
            .unwrap(),
        PreApply::Apply
    );
}
