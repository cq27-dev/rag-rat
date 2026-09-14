use super::*;

/// The `anchors/1` columns and `op::PortableAnchor` are ONE fact in two places: the `/3`
/// `node_anchors` op carries these columns across the account boundary, and the drain seeds
/// binding rows from them. A column added here without a matching op field would not fail to
/// compile — it would seed as NULL on every peer, discovered long after the fact — so this
/// fails at the edit instead.
///
/// Widening the op is a wire-format change, so the fix when this breaks is a new op kind, never
/// an extra field on the existing one.
#[test]
fn the_anchors_scope_columns_match_the_portable_anchor_op_fields() {
    let spec = SYNCABLE_TABLES
        .iter()
        .find(|spec| spec.scope_id == ScopeId::ANCHORS)
        .expect("the anchors/1 scope is registered");
    // The op omits the `(repo_id, memory_id)` head of the pk: the repo is the drain's context
    // and the memory is the op's own node id, so neither is repeated per anchor.
    let carried: Vec<&str> =
        spec.pk[2..].iter().chain(spec.columns.iter()).map(|column| column.name).collect();
    assert_eq!(carried, crate::op::PORTABLE_ANCHOR_FIELDS);
}

const DEMO_PK: &[ColumnSpec] = &[ColumnSpec::required("id", ValueType::Text)];
const DEMO_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec::required("title", ValueType::Text),
    ColumnSpec::required("count", ValueType::I64),
];
const DEMO_LOCAL: &[&str] = &["resolved_rowid"];
const DEMO_SPEC: TableSpec = TableSpec {
    name: "t_demo",
    scope_id: ScopeId::new("demo/1"),
    spec_version: 1,
    pk: DEMO_PK,
    columns: DEMO_COLUMNS,
    local_columns: DEMO_LOCAL,
    repo_column: None,
};

fn demo_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 resolved_rowid INTEGER
             ) STRICT;",
    )
    .unwrap();
    conn
}

/// Every production spec must classify its live physical schema exactly — the invariant that
/// stops a column being added to a registered table without a matching spec edit. Runs against
/// the real migration ladder, not a synthetic fixture, so a drift between the CREATE TABLE and
/// the `TableSpec` fails here.
#[test]
fn the_production_specs_cover_their_live_schema() {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
    for spec in SYNCABLE_TABLES {
        assert_spec_covers_schema(&conn, spec).unwrap_or_else(|err| {
            panic!("spec for `{}` does not cover its schema: {err}", spec.name)
        });
    }
    assert_registry_consistent(SYNCABLE_TABLES).unwrap();
}

/// Every registered scope must map to a non-empty Lens-lane set. `scope_lens_metas` is a
/// hand-maintained match on scope-id literals; without this a new scope silently drops Lens
/// invalidation (the apply/refold bump sites short-circuit cleanly on an empty set, so nothing
/// errors) — this test forces the match to be extended alongside `SYNCABLE_TABLES`.
#[test]
fn every_registered_scope_maps_to_a_lens_lane() {
    for spec in SYNCABLE_TABLES {
        assert!(
            !scope_lens_metas(spec.scope_id.as_db_str()).is_empty(),
            "scope `{}` (table `{}`) has no scope_lens_metas entry",
            spec.scope_id.as_db_str(),
            spec.name,
        );
    }
    assert!(scope_lens_metas("nonexistent/1").is_empty(), "an unregistered scope bumps nothing");
}

/// One table's replicated contract, in a shape both a recorded generation and the live registry
/// can be reduced to.
type TableShape = (
    &'static str,                                        // table
    ScopeId,                                             // scope_id
    u32,                                                 // spec_version
    Option<&'static str>,                                // repo_column
    Vec<(&'static str, ValueType)>,                      // pk
    Vec<(&'static str, ValueType, Option<AddedColumn>)>, // synced columns
);
type Snapshot = Vec<TableShape>;

fn snapshot_of_generation(generation: &[TableGeneration]) -> Snapshot {
    generation
        .iter()
        .map(|t| {
            (t.table, t.scope_id, t.spec_version, t.repo_column, t.pk.to_vec(), t.columns.to_vec())
        })
        .collect()
}

fn snapshot_of_live_registry() -> Snapshot {
    SYNCABLE_TABLES
        .iter()
        .map(|spec| {
            (
                spec.name,
                spec.scope_id,
                spec.spec_version,
                spec.repo_column,
                spec.pk.iter().map(|c| (c.name, c.value_type)).collect(),
                spec.columns.iter().map(|c| (c.name, c.value_type, c.added)).collect(),
            )
        })
        .collect()
}

#[test]
fn a_registry_change_cannot_land_without_a_projector_generation() {
    // The coupling that makes a widened registry actually reach parked entries. A refold is
    // owed only when the store's stamp is behind or an entry was parked by an OLDER projector,
    // so registering a table (or widening a spec) without moving the version leaves a store
    // already stamped at that version with entries it will never retry — and redelivery
    // short-circuits on `entry_exists`, so the payload is gone.
    //
    // A pin of the CURRENT registry cannot enforce this: editing the pin is exactly as easy as
    // making the change it is supposed to guard. Requiring the live registry to equal the LAST
    // recorded generation does, because the only way to satisfy it after a change is to append
    // — and the version IS the number of generations.
    assert_eq!(
        usize::try_from(super::super::refold::TABLE_SYNC_PROJECTOR_VERSION).unwrap(),
        PROJECTOR_GENERATIONS.len(),
        "the projector version is the count of recorded generations — append one, do not renumber"
    );
    assert_eq!(
        snapshot_of_generation(PROJECTOR_GENERATIONS.last().expect("at least one generation")),
        snapshot_of_live_registry(),
        "the live registry differs from the newest recorded generation — APPEND a generation \
         (which bumps the projector version), rather than editing the last one"
    );
}

#[test]
fn each_generation_only_extends_the_one_before_it() {
    // EVOLUTION IS ADDITIVE ONLY, enforced across history rather than asserted in prose. A
    // single binary cannot check this — it has no past registry to compare against — which is
    // why the rule is documented as un-lintable. The generation list IS that past, so every
    // consecutive pair can be checked.
    //
    // Both directions of "additive" matter, and they fail differently:
    //
    // - A column that DISAPPEARS (dropped or renamed) strands every stored or received op naming it
    //   on `project_cells`' `UnknownColumn` path — parked forever, since no future binary
    //   reintroduces the name, and redelivery short-circuits on `entry_exists`.
    // - A column that CHANGES its type or introduction tuple diverges silently. A declared default
    //   is the value every receiver synthesizes for an op predating the column, so changing it —
    //   even in step with the table's SQL default, which keeps the schema lint happy because that
    //   lint only ever sees the CURRENT schema — makes a device that folded an op before the change
    //   and one that folded it after hold different rows AT THE SAME CLOCK, with no local edit to
    //   signal it. `in_version` decides WHICH ops a column is filled for, so moving it
    //   retroactively rewrites what every stored op means.
    //
    // An accumulate-and-compare map catches only the second: a key that never reappears is
    // never revisited. Comparing each generation against its predecessor catches both.
    for (index, pair) in PROJECTOR_GENERATIONS.windows(2).enumerate() {
        let (previous, next) = (pair[0], pair[1]);
        let version = index + 2;
        for old in previous {
            let Some(new) = next.iter().find(|t| t.table == old.table) else {
                panic!(
                    "`{}` disappeared from the registry by generation {version} — ops already \
                     stored for it would park as `TableNotInScope` with nothing to redeem them. \
                     Retiring a table is a deliberate act, not a registry edit.",
                    old.table
                );
            };
            assert_eq!(
                old.pk, new.pk,
                "`{}` changed its primary key by generation {version} — the identity is what \
                 every clock, tombstone and published record is keyed on; a changed identity \
                 means a NEW TABLE, not a new spec version",
                old.table
            );
            // The scope selects the STREAM, and a projector bump cannot repair a move. Three
            // separate things break, none of them recoverable by replay:
            //   - a retained entry resolves its spec by the scope RECORDED ON THE ENTRY
            //     (`refold::replay_pending_entry`), so every stored op for this table reparks as
            //     `TableNotInScope` forever, whatever the projector version becomes;
            //   - `sync_row_clocks` / tombstones / `sync_published_rows` are keyed `(repo_id,
            //     table_name, row_pk)` with NO stream component, so they carry across silently and
            //     now hold lamports from a stream nobody writes — a locally-authored op starts from
            //     the NEW stream's max and loses its own self-apply;
            //   - the winner lookup keys on `(stream, device, lamport)`, so it lands on some
            //     sibling table's entry (guarded, but only down to "cannot resolve").
            // Moving a table between scopes is a data migration, not a registry edit.
            assert_eq!(
                old.scope_id,
                new.scope_id,
                "`{}` moved from scope `{}` to `{}` by generation {version} — its stream, and \
                 with it every retained entry and row clock, is derived from that scope; a scope \
                 change means a NEW TABLE",
                old.table,
                old.scope_id.as_db_str(),
                new.scope_id.as_db_str()
            );
            // The repo dimension decides WHICH rows this table replicates and which incoming
            // ops are accepted, while the bookkeeping it writes stays keyed by the caller's
            // repo either way. Dropping it to `None` is the sharp case: `read_all_rows` stops
            // filtering, so every physical row is emitted into EVERY repo's stream, and the
            // applier's repo-identity gate stops rejecting foreign ops — while one physical row
            // now collects an independent clock per repo. That is cross-repo leakage and
            // divergence at once, and no replay repairs it.
            assert_eq!(
                old.repo_column, new.repo_column,
                "`{}` changed its repo column from {:?} to {:?} by generation {version} — the \
                 repo dimension selects what replicates and what is accepted, but the clocks and \
                 published records it writes are keyed the same either way; changing it means a \
                 NEW TABLE",
                old.table, old.repo_column, new.repo_column
            );
            assert!(
                new.spec_version >= old.spec_version,
                "`{}`'s spec version went backwards by generation {version}",
                old.table
            );
            for (column, value_type, added) in old.columns {
                let carried = new.columns.iter().find(|(name, ..)| name == column);
                let Some((_, new_type, new_added)) = carried else {
                    panic!(
                        "`{}`.`{column}` disappeared by generation {version} — every op that \
                         names it would park as `UnknownColumn` forever. Removing or renaming a \
                         synced column means a NEW TABLE.",
                        old.table
                    );
                };
                assert_eq!(
                    (new_type, new_added),
                    (value_type, added),
                    "`{}`.`{column}` changed its type or introduction tuple by generation \
                     {version} — both are history the projection of older ops depends on",
                    old.table
                );
            }
            // A column that APPEARS must be fillable for every op the previous generation could
            // have authored, or older→newer replication stops for this table. The schema lint
            // cannot see this: it bounds `in_version` against the CURRENT spec version and
            // skips `required` columns entirely, both of which are judgements about one
            // generation in isolation. Only the predecessor says which versions are still out
            // there.
            for (column, _, added) in new.columns {
                if old.columns.iter().any(|(name, ..)| name == column) {
                    continue;
                }
                let Some(added) = added else {
                    panic!(
                        "`{}`.`{column}` was added in generation {version} as a REQUIRED column — \
                         an op from spec version {} omits it and parks as `PartialAfterImage` \
                         forever. A column added to a live table needs `ColumnSpec::added` with a \
                         declared default.",
                        old.table, old.spec_version
                    );
                };
                assert!(
                    added.in_version > old.spec_version,
                    "`{}`.`{column}` was added in generation {version} claiming to exist since \
                     spec version {}, but the previous generation shipped spec version {} — an op \
                     stamped {} omits the column yet is not old enough to have it filled, so it \
                     parks forever. Its introduction version must FOLLOW the previous spec.",
                    old.table,
                    added.in_version,
                    old.spec_version,
                    old.spec_version
                );
                assert!(
                    added.in_version <= new.spec_version,
                    "`{}`.`{column}` claims an introduction version above the spec version that \
                     introduces it ({} > {})",
                    old.table,
                    added.in_version,
                    new.spec_version
                );
            }
        }
    }
}

#[test]
fn a_spec_that_classifies_every_column_passes() {
    assert!(assert_spec_covers_schema(&demo_conn(), &DEMO_SPEC).is_ok());
}

/// A table with a later column, matching what an `ALTER TABLE ADD COLUMN` leaves behind.
fn widened_conn(default_clause: &str) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later TEXT{default_clause},
                 resolved_rowid INTEGER
             ) STRICT;"
    ))
    .unwrap();
    conn
}

macro_rules! widened_spec {
    ($later:expr) => {
        TableSpec {
            name: "t_demo",
            scope_id: ScopeId::new("demo/1"),
            spec_version: 2,
            pk: DEMO_PK,
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
                $later,
            ],
            local_columns: DEMO_LOCAL,
            repo_column: None,
        }
    };
}

#[test]
fn a_declared_default_must_equal_the_physical_default() {
    // THE CONVERGENCE GUARANTEE, not hygiene. Adding a column backfills existing rows with the
    // SQL default, while the applier fills a column an older op omits with the DECLARED one. If
    // the two disagree, a device that applied an op before upgrading and one that applied the
    // same op after hold DIFFERENT ROWS AT THE SAME CLOCK — silent divergence, with no local
    // edit to signal it.
    const MATCHING: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("x")));
    assert!(assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &MATCHING).is_ok());

    const DISAGREES: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("other")));
    let err = assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &DISAGREES)
        .expect_err("a declared default that disagrees with the schema is refused");
    assert!(err.contains("later"), "the error names the column: {err}");

    // An absent DEFAULT clause is SQLite's own NULL, and matches a declared Null.
    const NULL_DEFAULT: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Null));
    assert!(assert_spec_covers_schema(&widened_conn(""), &NULL_DEFAULT).is_ok());
    assert!(assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &NULL_DEFAULT).is_err());
}

#[test]
fn a_non_literal_default_is_refused() {
    // A per-device non-deterministic default would have two receivers fill the same op with
    // different values — divergence by construction. The lint cannot match it, so it refuses.
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("x")));
    assert!(
        assert_spec_covers_schema(&widened_conn(" DEFAULT (unixepoch())"), &SPEC).is_err(),
        "a non-literal default cannot be honored deterministically"
    );
}

#[test]
fn a_declared_default_must_match_the_columns_type() {
    // The fill goes straight into the row, so a mistyped default would write a value the
    // applier would have quarantined had it arrived on the wire.
    const MISTYPED: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::I64(7)));
    assert!(assert_spec_covers_schema(&widened_conn(" DEFAULT 7"), &MISTYPED).is_err());
}

#[test]
fn each_default_type_is_matched_against_its_own_sql_spelling() {
    // One case per `DefaultValue` variant, each asserted BOTH ways. Testing only `Text` left
    // every other arm of `default_matches_sql` free to return `true` unconditionally — i.e. a
    // declared default could disagree with the migration's backfill for any non-text column and
    // the lint that exists to catch exactly that would pass.
    for (declared, value_type, agrees, disagrees) in [
        (DefaultValue::Bool(true), ValueType::Bool, " DEFAULT 1", " DEFAULT 0"),
        (DefaultValue::I64(7), ValueType::I64, " DEFAULT 7", " DEFAULT 8"),
        (DefaultValue::Text("x"), ValueType::Text, " DEFAULT 'x'", " DEFAULT 'y'"),
        (DefaultValue::Blob(&[0xab]), ValueType::Blob, " DEFAULT X'ab'", " DEFAULT X'cd'"),
    ] {
        let sql_type = match value_type {
            ValueType::Bool | ValueType::I64 => "INTEGER",
            ValueType::Text => "TEXT",
            ValueType::Blob => "BLOB",
        };
        let conn = |clause: &str| {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE t_demo(
                         id TEXT PRIMARY KEY,
                         title TEXT NOT NULL,
                         count INTEGER NOT NULL,
                         later {sql_type}{clause},
                         resolved_rowid INTEGER
                     ) STRICT;"
            ))
            .unwrap();
            conn
        };
        let spec = TableSpec {
            name: "t_demo",
            scope_id: ScopeId::new("demo/1"),
            spec_version: 2,
            pk: DEMO_PK,
            // `columns` is `&'static`, and these vary per iteration — leaking a test-sized
            // array is simpler than a const per type.
            columns: Box::leak(Box::new([
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("count", ValueType::I64),
                ColumnSpec::added("later", value_type, 2, declared),
            ])),
            local_columns: DEMO_LOCAL,
            repo_column: None,
        };
        assert!(
            assert_spec_covers_schema(&conn(agrees), &spec).is_ok(),
            "{declared:?} must match the SQL default `{agrees}`"
        );
        assert!(
            assert_spec_covers_schema(&conn(disagrees), &spec).is_err(),
            "{declared:?} must NOT match the SQL default `{disagrees}`"
        );
    }
}

#[test]
fn a_text_default_must_be_one_literal_not_an_expression() {
    // SQLite strips the outer parentheses from a parenthesized default, so `DEFAULT ('x'||'y')`
    // comes back as `'x'||'y'` — quoted at both ends, but a CONCATENATION. Matching on the
    // outer quotes alone would accept it while SQLite backfills `xy` and the applier
    // synthesizes the raw text, which is the exact divergence the check exists to prevent.
    assert_eq!(schema_facts::single_quoted_literal("'x'"), Some("x".to_string()));
    assert_eq!(schema_facts::single_quoted_literal("'it''s'"), Some("it's".to_string()));
    assert_eq!(schema_facts::single_quoted_literal("''"), Some(String::new()));
    assert_eq!(
        schema_facts::single_quoted_literal("'x'||'y'"),
        None,
        "a concatenation is not a literal"
    );
    assert_eq!(schema_facts::single_quoted_literal("'a'||b"), None);
    assert_eq!(schema_facts::single_quoted_literal("unixepoch()"), None);

    // End to end: the spec declaring exactly what SQLite would evaluate is still refused,
    // because the physical default is not a literal at all.
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("x'||'y")));
    assert!(
        assert_spec_covers_schema(&widened_conn(" DEFAULT ('x'||'y')"), &SPEC).is_err(),
        "an expression default cannot be honored deterministically"
    );

    // Blobs need no equivalent case: a concatenation carries quotes and pipes, which cannot
    // compare equal to fixed-length pure hex, so the expression form is excluded already.
    const BLOB: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Blob, 2, DefaultValue::Blob(&[0xab])));
    assert!(assert_spec_covers_schema(&blob_conn(" DEFAULT X'ab'"), &BLOB).is_ok());
    assert!(
        assert_spec_covers_schema(&blob_conn(" DEFAULT (X'ab'||X'cd')"), &BLOB).is_err(),
        "an expression default is refused whatever it would evaluate to"
    );
}

/// `widened_conn`, but the later column is a BLOB.
fn blob_conn(default_clause: &str) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later BLOB{default_clause},
                 resolved_rowid INTEGER
             ) STRICT;"
    ))
    .unwrap();
    conn
}

#[test]
fn a_default_that_violates_its_own_check_is_refused() {
    // The sharp case, and the one a static reading of the constraint cannot decide: the CHECK
    // names ONLY this column, so it looks self-contained, and every other lint passes — the
    // declared default matches the SQL default, matches the ValueType, and is not Null on a
    // NOT NULL column. It is simply a value the table rejects. Every op older than the column
    // would be filled with it, fail the constraint at INSERT, and be quarantined TERMINALLY.
    //
    // `ALTER TABLE ... ADD COLUMN later INTEGER NOT NULL DEFAULT 0 CHECK(later > 0)` is
    // accepted by SQLite on an empty table, so this schema is reachable, not hypothetical.
    let violates = Connection::open_in_memory().unwrap();
    violates
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0 CHECK(later > 0),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&violates, &SPEC)
        .expect_err("a default its own CHECK rejects cannot be a default");
    assert!(err.contains("REJECTS"), "the error says what is wrong: {err}");

    // The same shape with a default the constraint accepts is fine — the lint DECIDES the
    // question rather than refusing the shape.
    let satisfied = Connection::open_in_memory().unwrap();
    satisfied
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 1 CHECK(later > 0),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    const OK_SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(1)));
    assert!(assert_spec_covers_schema(&satisfied, &OK_SPEC).is_ok());
}

#[test]
fn the_probe_honors_the_columns_collation_and_sqlites_truth_semantics() {
    // Both cases are ones an EVALUATED expression gets wrong, silently and in opposite
    // directions — which is why the probe re-declares the column instead.

    // COLLATION. A bare expression compares BINARY, so `'x' <> 'X'` reads true and the default
    // looks fine; the real column is NOCASE, where it is FALSE and the insert is refused. Under
    // an expression-based probe this schema would be accepted and then quarantine every older
    // op.
    let nocase = Connection::open_in_memory().unwrap();
    nocase
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT COLLATE NOCASE NOT NULL DEFAULT 'x' CHECK(later <> 'X'),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    const COLLATED: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("x")));
    assert!(
        assert_spec_covers_schema(&nocase, &COLLATED).is_err(),
        "the column's own collation decides, not BINARY"
    );

    // TRUTH SEMANTICS. SQLite violates a CHECK only when it evaluates to ZERO, so a REAL 0.5 is
    // satisfied. Reading the expression's result as an integer would fail to decode and refuse
    // a schema that works.
    let real = Connection::open_in_memory().unwrap();
    real.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0
                     CHECK(CASE WHEN later = 0 THEN 0.5 ELSE 1 END),
                 resolved_rowid INTEGER
             ) STRICT;",
    )
    .unwrap();
    const REAL_TRUTHY: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    assert!(
        assert_spec_covers_schema(&real, &REAL_TRUTHY).is_ok(),
        "a non-zero REAL satisfies a CHECK, so the default is fine"
    );
}

#[test]
fn a_check_keywords_case_does_not_hide_it() {
    // The keyword's case is not normalised in `sqlite_master.sql`, and locating the body by
    // trimming the literal text `CHECK` drops a mixed-case constraint SILENTLY — the
    // false-negative direction, where an unsafe default sails through.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 ChEcK(later > 0)
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err =
        assert_spec_covers_schema(&conn, &SPEC).expect_err("a mixed-case CHECK is still a CHECK");
    assert!(err.contains("REJECTS"), "the default is caught violating it: {err}");
}

#[test]
fn a_column_may_be_named_like_a_keyword_or_the_rowid() {
    // A QUOTED head is always a column name, never the keyword; and a table that DECLARES a
    // column called `rowid` shadows the implicit alias, so the name is an ordinary reference
    // rather than per-device state. Both were refused as impossible.
    let quoted = Connection::open_in_memory().unwrap();
    quoted
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     \"check\" INTEGER NOT NULL DEFAULT 0 CHECK(\"check\" >= 0),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    const QUOTED: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 2,
        pk: DEMO_PK,
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("count", ValueType::I64),
            ColumnSpec::added("check", ValueType::I64, 2, DefaultValue::I64(0)),
        ],
        local_columns: DEMO_LOCAL,
        repo_column: None,
    };
    assert!(
        assert_spec_covers_schema(&quoted, &QUOTED).is_ok(),
        "`\"check\"` is a column, not a constraint"
    );

    let shadowing = Connection::open_in_memory().unwrap();
    shadowing
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     rowid INTEGER NOT NULL DEFAULT 0 CHECK(rowid >= 0),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    const SHADOWING: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 2,
        pk: DEMO_PK,
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("count", ValueType::I64),
            ColumnSpec::added("rowid", ValueType::I64, 2, DefaultValue::I64(0)),
        ],
        local_columns: DEMO_LOCAL,
        repo_column: None,
    };
    assert!(
        assert_spec_covers_schema(&shadowing, &SHADOWING).is_ok(),
        "a declared `rowid` column shadows the implicit alias"
    );
}

#[test]
fn a_named_constraint_is_still_a_check() {
    // `CONSTRAINT c CHECK(...)` is the same constraint as a bare `CHECK(...)`. Missing the
    // named form drops it silently — the false-negative direction, where the unsafe default is
    // simply accepted.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CONSTRAINT later_is_positive CHECK(later > 0)
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&conn, &SPEC)
        .expect_err("a named constraint is still a constraint");
    assert!(err.contains("REJECTS"), "the default is caught violating it: {err}");
}

#[test]
fn a_double_quoted_column_reference_is_not_read_as_a_string() {
    // SQLite's double-quoted-string misfeature: `"later"` falls back to a STRING LITERAL when
    // no such column is in scope. Asking "does this resolve WITHOUT the column?" first would
    // therefore see it resolve and call the constraint irrelevant — accepting an unsafe
    // default. Asking "does it resolve with ONLY the column?" first settles it correctly.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CHECK(\"later\" > 10)
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&conn, &SPEC)
        .expect_err("a quoted reference to the column is a reference, not a string");
    assert!(err.contains("REJECTS"), "the default is caught violating it: {err}");
}

#[test]
fn a_check_whose_result_can_differ_between_devices_is_refused() {
    // Self-contained and satisfiable, yet worthless as a guarantee: the identical replicated op
    // can be accepted on one peer and quarantined on another, which is divergence with no local
    // edit to signal it.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CHECK(later >= 0 AND random() <> 0)
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&conn, &SPEC)
        .expect_err("a non-deterministic constraint cannot be relied on");
    assert!(err.contains("random"), "the error names the function: {err}");

    // A QUOTED function name is still a call: matching only bare words would let it past.
    let quoted_fn = Connection::open_in_memory().unwrap();
    quoted_fn
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0,
                     resolved_rowid INTEGER,
                     CHECK(later >= 0 AND \"random\"() <> 0)
                 ) STRICT;",
        )
        .unwrap();
    let err = assert_spec_covers_schema(&quoted_fn, &SPEC)
        .expect_err("a quoted function name is still a call");
    assert!(err.contains("random"), "the error names the function: {err}");

    // A date/time call is deterministic in its INPUTS; only an environment-dependent argument
    // reads the device. Refusing the whole family would block a legitimate schema.
    let dated = Connection::open_in_memory().unwrap();
    dated
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01'
                         CHECK(date(later) >= date('2000-01-01')),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    const DATED: TableSpec = widened_spec!(ColumnSpec::added(
        "later",
        ValueType::Text,
        2,
        DefaultValue::Text("2020-01-01")
    ));
    assert!(
        assert_spec_covers_schema(&dated, &DATED).is_ok(),
        "date() over the column's own value is deterministic"
    );

    // ...but the same function reading the clock is refused.
    let clock = Connection::open_in_memory().unwrap();
    clock
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01'
                         CHECK(date(later) <= date('now')),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    assert!(
        assert_spec_covers_schema(&clock, &DATED).is_err(),
        "date('now') reads the device clock"
    );

    // The environment literal must be an ARGUMENT of the date/time call, not merely present
    // somewhere in the constraint: here `'now'` is a value the column is compared against.
    let unrelated = Connection::open_in_memory().unwrap();
    unrelated
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01'
                         CHECK(date(later) IS NOT NULL AND later <> 'now'),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    assert!(
        assert_spec_covers_schema(&unrelated, &DATED).is_ok(),
        "a `'now'` outside the call's arguments does not make it clock-reading"
    );

    // A comment inside the call's arguments is not an argument. Scanning the raw text would
    // read it as one.
    let commented = Connection::open_in_memory().unwrap();
    commented
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01'
                         CHECK(date(later /* not 'now' */) IS NOT NULL),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    assert!(
        assert_spec_covers_schema(&commented, &DATED).is_ok(),
        "a commented-out `'now'` is not an argument"
    );

    // A build-varying function is refused even though SQLite flags it DETERMINISTIC: that flag
    // means "same answer for the same arguments within this build", which is not the question.
    // `fts5_source_id()` satisfies it and still returns a different string on a peer compiled
    // against another SQLite — which is why the rule is an allowlist rather than a denylist.
    let build_varying = Connection::open_in_memory().unwrap();
    build_varying
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0
                         CHECK(later = 0 AND fts5_source_id() IS NOT NULL),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    const ZERO: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&build_varying, &ZERO)
        .expect_err("a build-varying function is not a shared guarantee");
    assert!(err.contains("fts5_source_id"), "the error names it: {err}");

    // A DETERMINISTIC builtin is fine — the rule is about per-device variation, not calls.
    let ok = Connection::open_in_memory().unwrap();
    ok.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later TEXT NOT NULL DEFAULT 'ab' CHECK(length(later) = 2),
                 resolved_rowid INTEGER
             ) STRICT;",
    )
    .unwrap();
    const DETERMINISTIC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("ab")));
    assert!(assert_spec_covers_schema(&ok, &DETERMINISTIC).is_ok());
}

#[test]
fn a_sibling_named_after_a_keyword_does_not_break_the_probe() {
    // The probe's "does this resolve without the column" world lists the OTHER columns by name.
    // A column may legally be named after a keyword, and splicing such a name in bare fails
    // that CREATE on syntax — refusing a valid schema for a reason unrelated to the
    // constraint.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 \"order\" INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CHECK(title <> '')
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 2,
        pk: DEMO_PK,
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("order", ValueType::I64),
            ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)),
        ],
        local_columns: DEMO_LOCAL,
        repo_column: None,
    };
    assert!(
        assert_spec_covers_schema(&conn, &SPEC).is_ok(),
        "a sibling's name is quoted into the probe, so a keyword name is harmless"
    );
}

#[test]
fn a_check_sharing_a_segment_with_another_constraint_is_still_found() {
    // SQLite does not require a comma between table constraints, so a CHECK can share a segment
    // with a PRIMARY KEY. Dispatching on the segment's HEAD dropped that check entirely — the
    // verdict turned on where the author happened to put a comma.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT NOT NULL,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 PRIMARY KEY(id) CHECK(later >= count)
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&conn, &SPEC)
        .expect_err("a CHECK is a CHECK wherever it is written");
    assert!(
        err.contains("read something other than that column"),
        "refused as cross-column: {err}"
    );
}

#[test]
fn a_trailing_line_comment_does_not_break_the_probe() {
    // A declaration is spliced VERBATIM into the probe's DDL, so one ending in a `--` comment
    // would comment out everything after it on that line and the CREATE would fail as
    // incomplete input — refusing a perfectly ordinary table.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 resolved_rowid INTEGER,
                 later INTEGER NOT NULL DEFAULT 0 CHECK(later >= 0) -- how many
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    assert!(
        assert_spec_covers_schema(&conn, &SPEC).is_ok(),
        "a comment on the declaration is not a defect in the table"
    );
}

#[test]
fn a_generated_column_is_refused() {
    // A generated column is absent from `PRAGMA table_info`, so the exhaustiveness diff cannot
    // classify it as synced or local, and the applier never supplies it — yet it is not inert.
    // Its expression reads synced columns, so its own NOT NULL and CHECK constraints apply to a
    // value derived from whatever the applier filled in. Here `CHECK(derived > count)` fails
    // for a valid older op carrying `count = 5`, and the probe — which models the other columns
    // by NAME — cannot see the dependency that runs through `derived`. Refuse the shape.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 derived INTEGER GENERATED ALWAYS AS (later) VIRTUAL CHECK(derived > count)
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 2,
        pk: DEMO_PK,
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("count", ValueType::I64),
            ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)),
        ],
        local_columns: DEMO_LOCAL,
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &SPEC)
        .expect_err("a generated column cannot be classified or modelled");
    assert!(
        err.contains("GENERATED") && err.contains("derived"),
        "refused for being generated, not incidentally: {err}"
    );
}

#[test]
fn an_inline_check_on_a_sibling_column_is_still_a_constraint() {
    // An inline CHECK constrains the ROW, not the column it happens to be attached to. Reading
    // only table-level constraints plus the added column's own declaration therefore misses one
    // written on a SIBLING — and a valid older op carrying `count = 5` would be filled with
    // `later = 0`, rejected by SQLite, and quarantined terminally.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL CHECK(later >= count),
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&conn, &SPEC)
        .expect_err("where the constraint is WRITTEN does not change what it constrains");
    assert!(
        err.contains("read something other than that column"),
        "refused as cross-column: {err}"
    );

    // A sibling's inline CHECK that does NOT read this column is still irrelevant to it.
    let unrelated = Connection::open_in_memory().unwrap();
    unrelated
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL CHECK(count >= 0),
                     later INTEGER NOT NULL DEFAULT 0,
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    assert!(
        assert_spec_covers_schema(&unrelated, &SPEC).is_ok(),
        "a sibling constraint that never reads this column does not concern it"
    );
}

#[test]
fn a_clock_reading_date_call_is_refused_by_the_probe() {
    // Omitting the time value IS `'now'`: `date()` means `date('now')`, and `strftime('%s')` —
    // one argument, the format — reads the clock too.
    //
    // Nothing in this lint decides that. SQLite refuses a clock-reading date/time call inside a
    // CHECK at INSERT, which is exactly what the probe performs, so the verdict comes back as
    // not-self-contained on its own. That is why the date/time family sits on the allowlist:
    // SQLite draws the line more precisely than this lint could — it also catches a
    // `'localtime'` modifier, and it correctly permits `date(column, 'now')`, which is not a
    // clock read. This test pins that reliance so a future change cannot quietly lose it.
    let table = |constraint: &str| {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later TEXT NOT NULL DEFAULT '2020-01-01' CHECK({constraint}),
                     resolved_rowid INTEGER
                 ) STRICT;"
        ))
        .unwrap();
        conn
    };
    const SPEC: TableSpec = widened_spec!(ColumnSpec::added(
        "later",
        ValueType::Text,
        2,
        DefaultValue::Text("2020-01-01")
    ));

    // The WHOLE boundary is pinned, not just the two forms that prompted it: this lint now
    // depends on where SQLite draws the line, so a change in that line must fail here rather
    // than silently widen or narrow what the lint accepts.
    for constraint in [
        "later <= date()",                   // time value omitted
        "later <= strftime('%s')",           // format only, time value omitted
        "later <= date('now')",              // the clock, explicitly
        "later <= date(later, 'localtime')", // the device's timezone
    ] {
        let err = assert_spec_covers_schema(&table(constraint), &SPEC)
            .expect_err("a clock-reading call cannot be a shared guarantee");
        assert!(
            err.contains("non-deterministic use"),
            "refused because SQLite itself will not evaluate it: {err}"
        );
    }

    // ...and the forms SQLite permits stay permitted. `date(column, 'now')` is the sharp one:
    // `'now'` in a MODIFIER position is not a clock read, and a hand-rolled rule that scanned
    // for the literal would wrongly refuse it.
    for constraint in [
        "date(later) >= date('2000-01-01')",
        "strftime('%Y', later) >= '2000'",
        "date(later, '+1 day') > date('2000-01-01')",
        "date(later, 'now') > date('2000-01-01')",
    ] {
        assert!(
            assert_spec_covers_schema(&table(constraint), &SPEC).is_ok(),
            "deterministic in its inputs, so it is a shared guarantee: {constraint}"
        );
    }
}

#[test]
fn a_connection_configurable_operator_is_refused() {
    // `PRAGMA case_sensitive_like` changes both `a LIKE b` and `like(a, b)`, so the constraint
    // means different things on two peers. The OPERATOR form is the one a call-shaped scan
    // cannot see.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later TEXT NOT NULL DEFAULT 'ab' CHECK(later NOT LIKE 'Z%'),
                 resolved_rowid INTEGER
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Text("ab")));
    let err = assert_spec_covers_schema(&conn, &SPEC)
        .expect_err("LIKE is connection state, not a property of its operands");
    assert!(err.contains("like"), "the error names it: {err}");
}

#[test]
fn per_device_references_are_refused_in_every_form_they_take() {
    // The call-shaped scan and the bare-word rowid scan each missed a form. Neither gap is
    // visible from inside the other, and both are silent: the probe passes at ITS values, then
    // the same op is quarantined on a peer whose clock or insertion order differs.
    let table = |constraint: &str| {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 2 CHECK({constraint}),
                     resolved_rowid INTEGER
                 ) STRICT;"
        ))
        .unwrap();
        conn
    };
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(2)));

    // A QUOTED rowid alias resolves to the implicit rowid exactly as a bare one does. The
    // default passes at the probe's rowid 1 and fails on a peer whose row lands at rowid 2.
    let quoted = table("later <> \"rowid\"");
    let err = assert_spec_covers_schema(&quoted, &SPEC)
        .expect_err("a quoted rowid alias is still the rowid");
    assert!(err.contains("assigned per device"), "refused for the rowid rule: {err}");

    // A clock KEYWORD takes no parentheses, so a scan shaped around calls never reaches it.
    let clock = table("later > 0 AND CURRENT_TIMESTAMP IS NOT NULL");
    let err =
        assert_spec_covers_schema(&clock, &SPEC).expect_err("a clock keyword reads the device");
    assert!(err.contains("current_timestamp"), "the error names what reads the device: {err}");
}

#[test]
fn a_check_reading_the_implicit_rowid_is_refused() {
    // The rowid resolves in ANY rowid table — including the probe — so a constraint reading it
    // looks self-contained and passes. It is also per-device, assigned by insertion order, so
    // the same op lands at a different rowid on every peer.
    //
    // The body is deliberately one the default SATISFIES at the probe's rowid (1): a body that
    // merely fails there would be reported as `Violated` and the test would pass off SQLite's
    // own error text without ever exercising the rowid rule — which is how the first version of
    // this test was vacuous.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0 CHECK(later = 0 AND rowid < 10),
                 resolved_rowid INTEGER
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&conn, &SPEC)
        .expect_err("a rowid-dependent constraint cannot be proven safe");
    assert!(
        err.contains("assigned per device"),
        "refused for the rowid rule, not incidentally: {err}"
    );
}

#[test]
fn an_unrelated_constraint_is_not_dragged_into_the_probe() {
    // Which constraints involve the column is SQLite's answer, not a token match. `text` here
    // is a TYPE NAME inside a CAST, and the constraint does not read the `text` column at all —
    // a token match would import it into the probe, where `other` fails to resolve and the
    // schema is refused for a constraint that never concerned it.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 text TEXT NOT NULL DEFAULT '',
                 CHECK(CAST(title AS text) <> 'zz')
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 2,
        pk: DEMO_PK,
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("count", ValueType::I64),
            ColumnSpec::added("text", ValueType::Text, 2, DefaultValue::Text("")),
        ],
        local_columns: &[],
        repo_column: None,
    };
    assert!(
        assert_spec_covers_schema(&conn, &SPEC).is_ok(),
        "a constraint that does not read the column must not decide its fate"
    );
}

#[test]
fn a_table_qualified_self_reference_is_self_contained() {
    // `t_demo.later` names this very column. The probe rebuilds the column under the real
    // table's NAME so the qualification still resolves; otherwise a legal, genuinely
    // self-contained constraint would read as cross-column.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later INTEGER NOT NULL DEFAULT 0,
                 resolved_rowid INTEGER,
                 CHECK(t_demo.later >= 0)
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    assert!(
        assert_spec_covers_schema(&conn, &SPEC).is_ok(),
        "a qualified reference to the column itself is self-contained"
    );
}

#[test]
fn a_function_call_is_not_a_cross_column_reference() {
    // A built-in whose name matches a column of the table is not a reference to that column.
    // Token comparison alone cannot tell them apart; evaluation can, because SQLite resolves
    // `length(...)` as a function regardless of what the table's columns are called.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 length INTEGER NOT NULL DEFAULT 0,
                 later INTEGER NOT NULL DEFAULT 0 CHECK(later <= length('abc')),
                 resolved_rowid INTEGER
             ) STRICT;",
    )
    .unwrap();
    const SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 2,
        pk: DEMO_PK,
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("count", ValueType::I64),
            ColumnSpec::required("length", ValueType::I64),
            ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)),
        ],
        local_columns: DEMO_LOCAL,
        repo_column: None,
    };
    assert!(
        assert_spec_covers_schema(&conn, &SPEC).is_ok(),
        "`length(...)` is a call, not a reference to the `length` column"
    );
}

#[test]
fn an_added_column_may_not_participate_in_a_cross_column_check() {
    // The default and the other columns come from different places — the applier synthesizes
    // this one and takes the rest from the OP — so a constraint relating them can fail for a
    // perfectly valid older op and quarantine it TERMINALLY, ending older→newer replication.
    let cross = Connection::open_in_memory().unwrap();
    cross
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0 CHECK(later >= count),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    const CROSS: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::I64, 2, DefaultValue::I64(0)));
    let err = assert_spec_covers_schema(&cross, &CROSS)
        .expect_err("a default cannot satisfy a constraint that depends on the op's values");
    assert!(err.contains("later") && err.contains("count"), "names both sides: {err}");

    // A SELF-CONTAINED check is fine: it constrains only the synthesized value, statically.
    let alone = Connection::open_in_memory().unwrap();
    alone
        .execute_batch(
            "CREATE TABLE t_demo(
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     count INTEGER NOT NULL,
                     later INTEGER NOT NULL DEFAULT 0 CHECK(later IN (0, 1)),
                     resolved_rowid INTEGER
                 ) STRICT;",
        )
        .unwrap();
    assert!(
        assert_spec_covers_schema(&alone, &CROSS).is_ok(),
        "a check on the added column alone is decidable and allowed"
    );
}

#[test]
fn an_identity_column_may_not_declare_an_introduction_version() {
    // `added` promises a redemption path that does not exist for a key: an op authored before
    // the key grew carries fewer pk values, so `apply_row_op_on_stream`'s arity check
    // quarantines it TERMINALLY before any default could apply. Declaring it would
    // advertise older→newer replication while silently dropping every older op.
    const GROWN_KEY: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 2,
        pk: &[
            ColumnSpec::required("id", ValueType::Text),
            ColumnSpec::added("shard", ValueType::Text, 2, DefaultValue::Text("d")),
        ],
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::required("count", ValueType::I64),
        ],
        local_columns: DEMO_LOCAL,
        repo_column: None,
    };
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT NOT NULL,
                 shard TEXT NOT NULL DEFAULT 'd',
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 resolved_rowid INTEGER,
                 PRIMARY KEY(id, shard)
             ) STRICT;",
    )
    .unwrap();
    let err = assert_spec_covers_schema(&conn, &GROWN_KEY)
        .expect_err("a primary key cannot grow within a table's life");
    assert!(err.contains("shard") && err.contains("NEW TABLE"), "names the remedy: {err}");
}

#[test]
fn a_not_null_column_may_not_declare_a_null_default() {
    // Filling an older op from a Null default on a NOT NULL column fails the constraint at
    // INSERT, and the applier quarantines the op TERMINALLY — older→newer replication for the
    // table would be dead with nothing to redeem it. The SQL-default check cannot catch this:
    // a NOT NULL column with no DEFAULT clause reads as an absent physical default, which a
    // declared Null matches.
    const NULL_ON_NOT_NULL: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Null));
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 count INTEGER NOT NULL,
                 later TEXT NOT NULL,
                 resolved_rowid INTEGER
             ) STRICT;",
    )
    .unwrap();
    let err = assert_spec_covers_schema(&conn, &NULL_ON_NOT_NULL)
        .expect_err("a Null default on a NOT NULL column is refused");
    assert!(err.contains("later") && err.contains("NOT NULL"), "the error is specific: {err}");

    // The same declaration is fine once the column is nullable — the fill can succeed.
    assert!(assert_spec_covers_schema(&widened_conn(""), &NULL_ON_NOT_NULL).is_ok());
}

#[test]
fn an_added_columns_version_must_sit_inside_the_specs_history() {
    // `1` is the first version, so a column "added" there was present from the start and is
    // `required`; a version above the spec's own names a column this binary carries but does
    // not announce — a forgotten bump, which puts the fill window wrong in both directions.
    const AT_VERSION_1: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 1, DefaultValue::Text("x")));
    let err = assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &AT_VERSION_1)
        .expect_err("a column added in version 1 is `required`, not `added`");
    assert!(err.contains("later"), "the error names the column: {err}");

    // `widened_spec!` is spec_version 2, so 3 is beyond this spec's own history.
    const BEYOND_THE_SPEC: TableSpec =
        widened_spec!(ColumnSpec::added("later", ValueType::Text, 3, DefaultValue::Text("x")));
    assert!(
        assert_spec_covers_schema(&widened_conn(" DEFAULT 'x'"), &BEYOND_THE_SPEC).is_err(),
        "a column introduced beyond the spec's own version is a forgotten bump"
    );
}

#[test]
fn a_physical_column_missing_from_the_spec_fails() {
    // The table has an extra `note` column the spec forgot to classify.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(
                 id TEXT PRIMARY KEY, title TEXT NOT NULL, count INTEGER NOT NULL,
                 resolved_rowid INTEGER, note TEXT
             ) STRICT;",
    )
    .unwrap();
    let err = assert_spec_covers_schema(&conn, &DEMO_SPEC).unwrap_err();
    assert!(err.contains("note"), "the unclassified column is named: {err}");
}

#[test]
fn a_spec_column_absent_from_the_table_fails() {
    // The spec names `count` but the table doesn't have it.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT NOT NULL, resolved_rowid INTEGER) \
         STRICT;",
    )
    .unwrap();
    let err = assert_spec_covers_schema(&conn, &DEMO_SPEC).unwrap_err();
    assert!(err.contains("count"), "the phantom column is named: {err}");
}

#[test]
fn a_column_classified_twice_fails() {
    const DOUBLED: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: DEMO_PK,
        // `title` is both a synced column and (wrongly) a local column.
        columns: DEMO_COLUMNS,
        local_columns: &["resolved_rowid", "title"],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&demo_conn(), &DOUBLED).unwrap_err();
    assert!(err.contains("title"), "the doubly-classified column is named: {err}");
}

#[test]
fn a_repo_column_that_is_not_a_primary_key_fails() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t_x(id TEXT PRIMARY KEY, repo_id TEXT NOT NULL) STRICT;")
        .unwrap();
    const BAD: TableSpec = TableSpec {
        name: "t_x",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("repo_id", ValueType::Text)],
        local_columns: &[],
        repo_column: Some("repo_id"), /* a synced column, not a pk — the ingest gate would
                                       * miss it */
    };
    let err = assert_spec_covers_schema(&conn, &BAD).unwrap_err();
    assert!(err.contains("primary-key"), "a non-pk repo column is rejected: {err}");
}

#[test]
fn a_key_only_table_with_no_synced_columns_fails() {
    // A table whose entire row is its composite pk has no synced non-key column; the whole-row
    // apply path would build empty-column SQL, so the lint rejects the shape up front.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_members(group_id TEXT NOT NULL, member_id TEXT NOT NULL, PRIMARY \
         KEY(group_id, member_id)) STRICT;",
    )
    .unwrap();
    const KEY_ONLY: TableSpec = TableSpec {
        name: "t_members",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[
            ColumnSpec::required("group_id", ValueType::Text),
            ColumnSpec::required("member_id", ValueType::Text),
        ],
        columns: &[],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &KEY_ONLY).unwrap_err();
    assert!(
        err.contains("at least one synced non-key column"),
        "a key-only table is rejected: {err}"
    );
}

#[test]
fn a_declared_pk_that_does_not_match_the_schema_fails() {
    // The table's real primary key is (a, b), but the spec declares only `a` as pk and buries
    // the real key column `b` in `columns` — every name is still classified once, so only the
    // pk-vs-schema check catches the non-unique identity.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_pk(a TEXT NOT NULL, b TEXT NOT NULL, v TEXT, PRIMARY KEY(a, b)) STRICT;",
    )
    .unwrap();
    const WRONG_PK: TableSpec = TableSpec {
        name: "t_pk",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("a", ValueType::Text)],
        columns: &[
            ColumnSpec::required("b", ValueType::Text),
            ColumnSpec::required("v", ValueType::Text),
        ],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &WRONG_PK).unwrap_err();
    assert!(
        err.contains("does not match the table's primary key"),
        "the pk mismatch is named: {err}"
    );
}

/// The columns the applier nulls on a changed upsert must be local (never replicated, so the
/// null never crosses the wire) and nullable (so the null is storable) on the shipped schema.
#[test]
fn reset_on_upsert_names_nullable_local_columns_only() {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
    let mut any = false;
    for spec in SYNCABLE_TABLES {
        let columns = schema_facts::physical_column_info(&conn, spec.name).unwrap();
        for name in reset_on_upsert(spec) {
            any = true;
            assert!(
                spec.local_columns.contains(name),
                "{}: `{name}` is reset on upsert but is not a local column",
                spec.name
            );
            let column = columns
                .iter()
                .find(|column| column.name == *name)
                .unwrap_or_else(|| panic!("{}: `{name}` is not a column", spec.name));
            assert!(!column.not_null, "{}: `{name}` is reset on upsert but NOT NULL", spec.name);
        }
    }
    assert!(any, "the anchors spec declares a reset set");
}

#[test]
fn a_not_null_local_column_without_a_default_fails() {
    // A remote insert supplies only pk + synced columns, so a NOT NULL local column with no
    // default would make that insert fail — the lint rejects the shape up front.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_nn_local(id TEXT PRIMARY KEY, syn TEXT, loc TEXT NOT NULL) STRICT;",
    )
    .unwrap();
    const NN_LOCAL: TableSpec = TableSpec {
        name: "t_nn_local",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("syn", ValueType::Text)],
        local_columns: &["loc"],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &NN_LOCAL).unwrap_err();
    assert!(
        err.contains("NOT NULL without a default"),
        "the required local column is named: {err}"
    );
}

#[test]
fn a_not_null_local_column_with_a_default_passes() {
    // The same shape but with a DB default on the local column is fine — a remote insert leaves
    // it to the default, and the local index re-derives it.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_def_local(id TEXT PRIMARY KEY, syn TEXT, loc INTEGER NOT NULL DEFAULT 0) \
         STRICT;",
    )
    .unwrap();
    const DEF_LOCAL: TableSpec = TableSpec {
        name: "t_def_local",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("syn", ValueType::Text)],
        local_columns: &["loc"],
        repo_column: None,
    };
    assert!(assert_spec_covers_schema(&conn, &DEF_LOCAL).is_ok());
}

#[test]
fn a_nullable_primary_key_fails() {
    // A non-STRICT rowid table's bare `id TEXT PRIMARY KEY` is NULLABLE (SQLite quirk); a NULL
    // pk self-quarantines and gets re-signed every producer pass.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t_np(id TEXT PRIMARY KEY, v TEXT);").unwrap();
    const NP: TableSpec = TableSpec {
        name: "t_np",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &NP).unwrap_err();
    assert!(err.contains("nullable"), "a nullable pk is rejected: {err}");
}

#[test]
fn a_foreign_key_fails() {
    // A foreign key is a cross-row constraint: a delete/insert can fail against another row, so
    // whole-row LWW becomes order-dependent (peers diverge).
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE parent(id TEXT NOT NULL PRIMARY KEY, v TEXT) STRICT;
             CREATE TABLE t_fk(id TEXT NOT NULL PRIMARY KEY, v TEXT, p TEXT REFERENCES parent(id)) \
         STRICT;",
    )
    .unwrap();
    const FK: TableSpec = TableSpec {
        name: "t_fk",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[
            ColumnSpec::required("v", ValueType::Text),
            ColumnSpec::required("p", ValueType::Text),
        ],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &FK).unwrap_err();
    assert!(err.contains("foreign key"), "an FK table is rejected: {err}");
}

#[test]
fn a_non_pk_unique_index_fails() {
    // A UNIQUE constraint on a non-pk column is a cross-row constraint: two rows racing for one
    // value fold differently by arrival order, so peers diverge.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t_u(id TEXT NOT NULL PRIMARY KEY, email TEXT UNIQUE) STRICT;")
        .unwrap();
    const U: TableSpec = TableSpec {
        name: "t_u",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("email", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &U).unwrap_err();
    assert!(err.contains("UNIQUE index"), "a non-pk UNIQUE table is rejected: {err}");
}

#[test]
fn a_non_strict_table_fails() {
    // Otherwise valid (explicit NOT NULL pk, no FK/UNIQUE, matching types) but NOT STRICT — an
    // applied value could be affinity-coerced and wedge the post-write hash read-back.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t_ns(id TEXT NOT NULL PRIMARY KEY, v TEXT);").unwrap();
    const NS: TableSpec = TableSpec {
        name: "t_ns",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &NS).unwrap_err();
    assert!(err.contains("must be STRICT"), "a non-STRICT table is rejected: {err}");
}

#[test]
fn a_declared_value_type_that_disagrees_with_the_physical_type_fails() {
    // `n` is physically INTEGER but the spec declares it Text.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t_tm(id TEXT NOT NULL PRIMARY KEY, n INTEGER) STRICT;")
        .unwrap();
    const TM: TableSpec = TableSpec {
        name: "t_tm",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("n", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &TM).unwrap_err();
    assert!(err.contains("physical type"), "a ValueType/physical-type mismatch is rejected: {err}");
}

#[test]
fn a_table_registered_under_two_scopes_fails() {
    const A: TableSpec = TableSpec {
        name: "t_dup",
        scope_id: ScopeId::new("scope-a/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    const B: TableSpec = TableSpec {
        name: "t_dup",
        scope_id: ScopeId::new("scope-b/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_registry_consistent(&[A, B]).unwrap_err();
    assert!(err.contains("more than one spec"), "a table under two scopes is rejected: {err}");
    assert!(assert_registry_consistent(&[A]).is_ok(), "a single registration is fine");
}

#[test]
fn a_non_text_repo_column_fails() {
    // The applier's repo gate compares the repo pk value to a TEXT repo_id, so a non-Text repo
    // scope key never matches → every row self-quarantines.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_ri(rid INTEGER NOT NULL, id TEXT NOT NULL, v TEXT, PRIMARY KEY(rid, id)) \
         STRICT;",
    )
    .unwrap();
    const RI: TableSpec = TableSpec {
        name: "t_ri",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[
            ColumnSpec::required("rid", ValueType::I64),
            ColumnSpec::required("id", ValueType::Text),
        ],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: Some("rid"),
    };
    let err = assert_spec_covers_schema(&conn, &RI).unwrap_err();
    assert!(err.contains("must be ValueType::Text"), "a non-Text repo column is rejected: {err}");
}

#[test]
fn a_table_referenced_by_a_foreign_key_fails() {
    // No outbound FK, but a child table references it — the same cross-row delete hazard.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_ref(id TEXT NOT NULL PRIMARY KEY, v TEXT) STRICT;
             CREATE TABLE kid(id TEXT NOT NULL PRIMARY KEY, r TEXT REFERENCES t_ref(id)) STRICT;",
    )
    .unwrap();
    const REF: TableSpec = TableSpec {
        name: "t_ref",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &REF).unwrap_err();
    assert!(err.contains("referencing it"), "an inbound-FK table is rejected: {err}");
}

#[test]
fn a_non_binary_pk_collation_fails() {
    // A `COLLATE NOCASE` pk: SQLite treats "a"/"A" as one row, but the row-clock encoding is
    // byte-exact → split bookkeeping for one physical row.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_ci(id TEXT COLLATE NOCASE NOT NULL PRIMARY KEY, v TEXT) STRICT;",
    )
    .unwrap();
    const CI: TableSpec = TableSpec {
        name: "t_ci",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("v", ValueType::Text)],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &CI).unwrap_err();
    assert!(err.contains("non-BINARY collation"), "a NOCASE pk is rejected: {err}");
}

#[test]
fn a_table_with_a_trigger_fails() {
    // An AFTER INSERT trigger that mutates a synced cell makes the same received op fold to a
    // different physical row across devices.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t_trig(id TEXT NOT NULL PRIMARY KEY, v TEXT, n INTEGER NOT NULL DEFAULT 0) \
         STRICT;
             CREATE TRIGGER t_trig_ai AFTER INSERT ON t_trig
                 BEGIN UPDATE t_trig SET n = n + 1 WHERE id = NEW.id; END;",
    )
    .unwrap();
    const TRIG: TableSpec = TableSpec {
        name: "t_trig",
        scope_id: ScopeId::new("demo/1"),
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[
            ColumnSpec::required("v", ValueType::Text),
            ColumnSpec::required("n", ValueType::I64),
        ],
        local_columns: &[],
        repo_column: None,
    };
    let err = assert_spec_covers_schema(&conn, &TRIG).unwrap_err();
    assert!(err.contains("trigger"), "a table with a trigger is rejected: {err}");
}

#[test]
fn every_registered_table_is_covered_by_the_live_schema() {
    // Empty today; the moment a per-scope milestone registers a real table, this pins that its
    // spec classifies every physical column of the migrated schema.
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
    assert_registry_consistent(SYNCABLE_TABLES).expect("the registry has a duplicate table");
    for spec in SYNCABLE_TABLES {
        assert_spec_covers_schema(&conn, spec)
            .unwrap_or_else(|err| panic!("registered table `{}` is not covered: {err}", spec.name));
    }
}
