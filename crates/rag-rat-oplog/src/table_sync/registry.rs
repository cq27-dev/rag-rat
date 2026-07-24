//! The declarative registry of syncable tables.
//!
//! A [`TableSpec`] declares, for one physical table, which columns replicate (the `pk` identity
//! plus the synced `columns`) and which are re-derived locally and must NEVER travel
//! (`local_columns`). The engine is generic over `&[TableSpec]`, so the mechanism is exercised
//! against a synthetic spec in tests; [`SYNCABLE_TABLES`] stays empty until the per-scope
//! milestones register real tables (anchors, overlay, distill).
//!
//! [`assert_spec_covers_schema`] is the load-bearing invariant: every physical column must be
//! classified exactly once — as pk, synced, or local. A newly-added physical column can never be
//! silently unclassified (neither replicated nor deliberately local), which would be an invisible
//! correctness gap.

use std::collections::BTreeSet;

use rusqlite::Connection;

/// The storage/wire type of a synced column. A cell whose runtime value disagrees with its column's
/// declared type is quarantined by the applier rather than silently coerced.
///
/// `Bool` stores as a STRICT `INTEGER` and must hold only 0 or 1. SQLite does not enforce that
/// domain without a `CHECK (col IN (0, 1))`, which no pragma exposes for the lint to require — so a
/// `Bool` column SHOULD carry that CHECK, and `read_typed` fail-closes (errors, halting the pass —
/// never silently coercing) on any other integer as the runtime backstop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueType {
    Text,
    I64,
    Bool,
    Blob,
}

/// One synced, non-pk column: its name and wire type. Merge is whole-row (all synced columns move
/// together under the row's write clock), so a column carries no per-column merge policy.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ColumnSpec {
    pub name: &'static str,
    pub value_type: ValueType,
}

/// One syncable table. `pk` names the identity columns (encoded as the row op's `pk`); `columns`
/// are the non-pk synced columns (encoded as the op's cells; the whole row is folded as a unit
/// under its write clock); `local_columns` are re-derived from the local index and never
/// replicated. `scope_id` names the `/4` stream this table rides — the routing key that binds it to
/// an auth tier
/// + retention class + flood budget.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TableSpec {
    pub name: &'static str,
    pub scope_id: &'static str,
    /// The identity columns, with types — the applier validates each incoming pk value against its
    /// declared type so SQLite affinity can't coerce a mismatched pk (e.g. `I64(1)` onto a `TEXT`
    /// key `'1'`) and split a row's bookkeeping.
    pub pk: &'static [ColumnSpec],
    pub columns: &'static [ColumnSpec],
    pub local_columns: &'static [&'static str],
    /// The column that scopes rows to a project, if the table is repo-scoped. It MUST be a
    /// primary-key column (the exhaustiveness lint enforces this): the producer emits only rows
    /// whose value here matches the repo being synced (so foreign-repo rows are never signed into
    /// the wrong repo's stream), and the applier rejects an incoming op naming a different repo.
    /// `None` for a table with no repo dimension.
    pub repo_column: Option<&'static str>,
}

impl TableSpec {
    /// The position of `repo_column` within `pk`, if the repo scope is a primary-key column — the
    /// index the applier checks against the repo being synced.
    pub fn repo_pk_index(&self) -> Option<usize> {
        let repo_column = self.repo_column?;
        self.pk.iter().position(|c| c.name == repo_column)
    }
}

/// The registry of tables the engine replicates. Empty until the per-scope milestones (anchors,
/// overlay, distill) register their tables; the engine's apply/produce take a `&[TableSpec]`, so
/// nothing here is load-bearing yet — the mechanism is proven against a synthetic spec in tests.
pub(crate) const SYNCABLE_TABLES: &[TableSpec] = &[];

/// Assert a spec classifies EVERY physical column of its table exactly once — as pk, a synced
/// column, or a local column — and names no column the table doesn't have. This is the invariant
/// that stops a new column being silently unclassified: it is either replicated or deliberately
/// local, never neither. Returns a human-readable diff on mismatch.
pub(crate) fn assert_spec_covers_schema(conn: &Connection, spec: &TableSpec) -> Result<(), String> {
    let columns = physical_column_info(conn, spec.name)
        .map_err(|err| format!("cannot read columns of `{}`: {err}", spec.name))?;
    let physical: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();

    let mut classified: BTreeSet<&str> = BTreeSet::new();
    let mut duplicated: Vec<&str> = Vec::new();
    let declared = spec
        .pk
        .iter()
        .map(|c| c.name)
        .chain(spec.columns.iter().map(|c| c.name))
        .chain(spec.local_columns.iter().copied());
    for name in declared {
        if !classified.insert(name) {
            duplicated.push(name);
        }
    }
    if !duplicated.is_empty() {
        return Err(format!(
            "`{}`: column(s) classified more than once: {duplicated:?}",
            spec.name
        ));
    }

    let physical_set: BTreeSet<&str> = physical.iter().copied().collect();
    let unclassified: Vec<&str> = physical_set.difference(&classified).copied().collect();
    let absent: Vec<&str> = classified.difference(&physical_set).copied().collect();
    if !unclassified.is_empty() || !absent.is_empty() {
        return Err(format!(
            "`{}` registry/schema mismatch: physical columns not classified {unclassified:?}; \
             classified columns absent from the table {absent:?}",
            spec.name
        ));
    }

    // A repo scope must be a primary-key column: only then does the applier's repo-identity gate
    // fire on every incoming op. A non-pk repo column would filter the producer but leave ingest
    // unguarded, so a peer could write another repo's row into the shared table.
    if let Some(repo_column) = spec.repo_column {
        match spec.repo_pk_index() {
            None => {
                return Err(format!(
                    "`{}`: repo_column `{repo_column}` must be a primary-key column so the ingest \
                     repo gate applies",
                    spec.name
                ));
            },
            // The applier's repo gate compares the repo pk value to `TypedValue::Text(repo_id)`, so
            // a non-Text scope key never matches — every locally-produced row self-quarantines and
            // the whole table can never sync.
            Some(idx) if spec.pk[idx].value_type != ValueType::Text => {
                return Err(format!(
                    "`{}`: repo_column `{repo_column}` must be ValueType::Text (the repo gate \
                     compares it to the text repo_id)",
                    spec.name
                ));
            },
            Some(_) => {},
        }
    }

    // The whole-row apply/produce SQL builds a `SELECT`/`SET` over the synced columns; a spec with
    // no synced non-key column would emit empty-column SQL (`SELECT  FROM …`) at apply time. A
    // key-only (pure set-membership) table would need a deliberately designed empty-row path
    // (existence-only hash, no-op update) that whole-row LWW does not have — reject it here rather
    // than emit invalid SQL when its first row is applied.
    if spec.columns.is_empty() {
        return Err(format!(
            "`{}`: a syncable table must declare at least one synced non-key column; a key-only \
             table is not supported by the whole-row apply path",
            spec.name
        ));
    }

    // The declared `pk` must be EXACTLY the table's real primary key, in order. The classification
    // check above only matched column NAMES, so a spec could name a non-key column as `pk` (or bury
    // a real key column in `columns`/`local_columns`): row identity would then be non-unique — one
    // op could update or delete several physical rows through `pk_where`, and the per-row clock /
    // tombstone key would not identify a single row. Compare against `PRAGMA table_info`'s pk
    // order.
    let mut pk_cols: Vec<&PhysicalColumn> = columns.iter().filter(|c| c.pk_position > 0).collect();
    pk_cols.sort_by_key(|c| c.pk_position);
    let actual_pk: Vec<&str> = pk_cols.iter().map(|c| c.name.as_str()).collect();
    let declared_pk: Vec<&str> = spec.pk.iter().map(|c| c.name).collect();
    if actual_pk != declared_pk {
        return Err(format!(
            "`{}`: declared pk {declared_pk:?} does not match the table's primary key \
             {actual_pk:?} (identical columns, identical order required)",
            spec.name
        ));
    }

    // Every pk column must be NOT NULL. A rowid table's bare `id TEXT PRIMARY KEY` is NULLABLE
    // (SQLite's historic quirk) — a NULL pk is unaddressable, so `read_all_rows` emits a Null pk
    // that self-apply quarantines, and `produce_and_author` then re-signs that ghost row on
    // every pass. A STRICT table makes its pk NOT NULL (table_info reports it), which is the
    // intended shape.
    for col in &pk_cols {
        if !col.not_null {
            return Err(format!(
                "`{}`: primary-key column `{}` is nullable — declare it NOT NULL (a STRICT table \
                 does this implicitly); a NULL pk is unaddressable and self-quarantines",
                spec.name, col.name
            ));
        }
    }

    // Every pk column must use BINARY equality. A non-binary collation (e.g. `COLLATE NOCASE`)
    // makes SQLite treat values differing only by collation as ONE row in the `WHERE`
    // predicates, but `row_op::row_pk_string` encodes them as DIFFERENT bookkeeping identities
    // — so one physical row would carry two write clocks / published hashes and diverge (or
    // suppress the wrong update).
    if let Some(col) = pk_column_with_non_binary_collation(conn, spec.name)
        .map_err(|err| format!("cannot read the pk collation of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: primary-key column `{col}` uses a non-BINARY collation — the row-clock \
             encoding is byte-exact, so collation-equal keys would split one row's bookkeeping; \
             use BINARY",
            spec.name
        ));
    }

    // Whole-row LWW converges per row INDEPENDENTLY — each row's fate is decided solely by its own
    // write clock. A CROSS-ROW constraint breaks that: the same op set can fold to different states
    // under different arrival orders (two rows racing for one UNIQUE value: whichever loses is
    // quarantined, and WHICH loses depends on order), so peers diverge with no dirty-local edit. A
    // foreign key is the same class (a delete/insert can fail against another row). Reject both
    // until a deterministic cross-row conflict rule exists.
    if table_has_foreign_key(conn, spec.name)
        .map_err(|err| format!("cannot read foreign keys of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: a foreign key makes whole-row LWW order-dependent (an op can fail against \
             another row) — not supported",
            spec.name
        ));
    }
    // The inbound direction is the same hazard: a table REFERENCED by another's FK can have a
    // `Remove` blocked (FK RESTRICT) on a peer that holds a child row but not on one that doesn't →
    // the delete quarantines on one side, applies on the other, and the replicas diverge.
    if table_is_referenced_by_foreign_key(conn, spec.name)
        .map_err(|err| format!("cannot scan foreign keys referencing `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: another table has a foreign key referencing it — a delete can be blocked on \
             one peer but not another, so whole-row LWW diverges — not supported",
            spec.name
        ));
    }
    // A trigger breaks the whole-row fold's assumption that a row write is independent and
    // deterministic: an INSERT/UPDATE/DELETE trigger can adjust the row (or others) from local
    // derived state, so the SAME received op folds to different physical results on two devices,
    // and apply_upsert then publishes each divergent result — the replicas stay divergent.
    if let Some(trigger) = table_trigger(conn, spec.name)
        .map_err(|err| format!("cannot read triggers of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: trigger `{trigger}` can mutate a row from local/derived state, so the same op \
             folds differently across devices — not supported",
            spec.name
        ));
    }
    if let Some(index) = non_pk_unique_index(conn, spec.name)
        .map_err(|err| format!("cannot read indexes of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: UNIQUE index `{index}` is a cross-row constraint that makes whole-row LWW \
             order-dependent (two rows racing for one value diverge by arrival order) — not \
             supported",
            spec.name
        ));
    }

    // Every local (never-replicated) column must be nullable or carry a DB default: a remote upsert
    // INSERTs only the pk + synced columns (a local column is re-derived here, not sent), so a NOT
    // NULL local column with no default makes that insert fail and the applier quarantine the op —
    // the row would then be absent on every new peer that never authored it locally.
    for local in spec.local_columns {
        if let Some(col) = columns.iter().find(|c| c.name == *local)
            && col.not_null
            && !col.has_default
        {
            return Err(format!(
                "`{}`: local column `{local}` is NOT NULL without a default, so a remote insert \
                 (pk + synced columns only) cannot materialize the row",
                spec.name
            ));
        }
    }

    // The table MUST be STRICT. STRICT enforces the declared column type at write time, so a value
    // the producer read (by its `ValueType`) can never be affinity-coerced to a different stored
    // type — which would make the post-write `synced_row_hash` read-back throw and wedge ingest
    // after the row was already written. It also makes pk columns NOT NULL. It is the schema
    // convention for every new table regardless.
    if !table_is_strict(conn, spec.name)
        .map_err(|err| format!("cannot read the schema of `{}`: {err}", spec.name))?
    {
        return Err(format!(
            "`{}`: a syncable table must be STRICT (enforced column types keep an applied value \
             from being coerced to a type the producer did not read)",
            spec.name
        ));
    }

    // Each replicated column's declared `ValueType` must match its physical STRICT type, so the
    // value the producer reads round-trips through SQLite unchanged and a peer's op passes the
    // applier's type check. (Local columns are re-derived, never sent, so they are exempt.)
    for spec_col in spec.pk.iter().chain(spec.columns.iter()) {
        let Some(phys) = columns.iter().find(|c| c.name == spec_col.name) else {
            continue; // classified/absent already checked above
        };
        if !value_type_matches_declared(spec_col.value_type, &phys.decl_type) {
            return Err(format!(
                "`{}`: column `{}` is declared {:?} in the spec but has physical type `{}` — the \
                 two must agree so values round-trip unchanged",
                spec.name, spec_col.name, spec_col.value_type, phys.decl_type
            ));
        }
    }
    Ok(())
}

/// Whether `table` is a STRICT table. SQLite exposes no pragma for this, so read the table options
/// that follow the column-list's closing `)` (all column-level parens nest inside it, so the LAST
/// `)` is always that closer) and look for the `STRICT` keyword.
fn table_is_strict(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    let options = sql.rsplit(')').next().unwrap_or_default();
    Ok(options
        .to_ascii_uppercase()
        .split(|c: char| c == ',' || c.is_whitespace())
        .any(|token| token == "STRICT"))
}

/// Whether a declared `ValueType` matches a physical STRICT column type. `Bool` and `I64` both
/// store as INTEGER; `Text`/`Blob` map to their obvious types. The permissive `ANY` is rejected — a
/// typed column must pin its type so a stored value can never be a different type than the producer
/// reads.
fn value_type_matches_declared(vt: ValueType, decl_type: &str) -> bool {
    match vt {
        ValueType::Text => decl_type == "TEXT",
        ValueType::I64 | ValueType::Bool => decl_type == "INTEGER" || decl_type == "INT",
        ValueType::Blob => decl_type == "BLOB",
    }
}

/// Assert the registry is internally consistent: no physical table name registered under more than
/// one spec. Two specs for one table would share its `(repo_id, table, row_pk)` clock / tombstone /
/// published rows across two streams — cross-scope LWW interference (the first stream to publish a
/// row silences the second, and received writes compete through one clock). Called over the whole
/// [`SYNCABLE_TABLES`] set, complementing the per-spec [`assert_spec_covers_schema`].
pub(crate) fn assert_registry_consistent(registry: &[TableSpec]) -> Result<(), String> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for spec in registry {
        if !seen.insert(spec.name) {
            return Err(format!(
                "table `{}` is registered under more than one spec/scope; its per-row bookkeeping \
                 keys on (repo_id, table, row_pk) and would be shared across streams",
                spec.name
            ));
        }
    }
    Ok(())
}

/// One physical column of a table, from `PRAGMA table_info` (`conn.pragma` quotes the table name,
/// so a spec name never needs manual escaping). `pk_position` is the 1-based position within the
/// primary key, or `0` for a non-key column. `decl_type` is the declared column type (uppercased) —
/// on a STRICT table one of `INT`/`INTEGER`/`REAL`/`TEXT`/`BLOB`/`ANY`.
struct PhysicalColumn {
    name: String,
    decl_type: String,
    not_null: bool,
    has_default: bool,
    pk_position: i64,
}

/// Whether `table` declares any foreign key (`PRAGMA foreign_key_list` returns a row per FK).
fn table_has_foreign_key(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let mut any = false;
    conn.pragma(None, "foreign_key_list", table, |_row| {
        any = true;
        Ok(())
    })?;
    Ok(any)
}

/// The name of `table`'s first UNIQUE index that is NOT the primary key, if any (a cross-row
/// constraint). `PRAGMA index_list` columns: 0 seq, 1 name, 2 unique, 3 origin, 4 partial; `origin`
/// is `pk` for the primary key's implicit index, `u` for a UNIQUE constraint, `c` for a
/// `CREATE UNIQUE INDEX`.
fn non_pk_unique_index(conn: &Connection, table: &str) -> rusqlite::Result<Option<String>> {
    let mut found = None;
    conn.pragma(None, "index_list", table, |row| {
        let unique: i64 = row.get(2)?;
        let origin: String = row.get(3)?;
        if unique != 0 && origin != "pk" && found.is_none() {
            found = Some(row.get::<_, String>(1)?);
        }
        Ok(())
    })?;
    Ok(found)
}

/// The first pk column that uses a non-BINARY collation, if any. Reads the primary key's index
/// (`PRAGMA index_list` origin=`pk` → `index_xinfo`, whose col 2 is the column name — NULL for the
/// implicit rowid — and col 4 the collation). An INTEGER-rowid pk has no such index (integers have
/// no collation), so it returns `None`.
fn pk_column_with_non_binary_collation(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<Option<String>> {
    let mut pk_index = None;
    conn.pragma(None, "index_list", table, |row| {
        if row.get::<_, String>(3)? == "pk" {
            pk_index = Some(row.get::<_, String>(1)?);
        }
        Ok(())
    })?;
    let Some(index) = pk_index else { return Ok(None) };
    let mut offending = None;
    conn.pragma(None, "index_xinfo", &index, |row| {
        // index_xinfo columns: 0 seqno, 1 cid, 2 name (NULL for the rowid), 3 desc, 4 coll, 5 key.
        let name: Option<String> = row.get(2)?;
        let collation: String = row.get(4)?;
        if let Some(name) = name
            && !collation.eq_ignore_ascii_case("BINARY")
            && offending.is_none()
        {
            offending = Some(name);
        }
        Ok(())
    })?;
    Ok(offending)
}

/// The name of the first trigger on `table`, if any (`sqlite_master` type='trigger'). A trigger can
/// mutate the row (or other rows) from local/derived state, so the SAME received op could fold to
/// different physical results on two devices — divergence the whole-row fold can't detect.
fn table_trigger(conn: &Connection, table: &str) -> rusqlite::Result<Option<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1")?;
    let mut rows = stmt.query([table])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Whether any OTHER table declares a foreign key REFERENCING `table` (an inbound reference). Scans
/// every base table's `PRAGMA foreign_key_list` (col 2 is the referenced table).
fn table_is_referenced_by_foreign_key(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let mut tables = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            tables.push(row?);
        }
    }
    for other in tables {
        if other == table {
            continue;
        }
        let mut references = false;
        conn.pragma(None, "foreign_key_list", &other, |row| {
            // foreign_key_list columns: 0 id, 1 seq, 2 table (the referenced table), 3 from, 4 to.
            if row.get::<_, String>(2)? == table {
                references = true;
            }
            Ok(())
        })?;
        if references {
            return Ok(true);
        }
    }
    Ok(false)
}

fn physical_column_info(conn: &Connection, table: &str) -> rusqlite::Result<Vec<PhysicalColumn>> {
    let mut cols = Vec::new();
    conn.pragma(None, "table_info", table, |row| {
        // PRAGMA table_info columns: 0 cid, 1 name, 2 type, 3 notnull, 4 dflt_value, 5 pk.
        cols.push(PhysicalColumn {
            name: row.get::<_, String>(1)?,
            decl_type: row.get::<_, String>(2)?.to_ascii_uppercase(),
            not_null: row.get::<_, i64>(3)? != 0,
            has_default: row.get::<_, Option<String>>(4)?.is_some(),
            pk_position: row.get::<_, i64>(5)?,
        });
        Ok(())
    })?;
    Ok(cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO_PK: &[ColumnSpec] = &[ColumnSpec { name: "id", value_type: ValueType::Text }];
    const DEMO_COLUMNS: &[ColumnSpec] =
        &[ColumnSpec { name: "title", value_type: ValueType::Text }, ColumnSpec {
            name: "count",
            value_type: ValueType::I64,
        }];
    const DEMO_LOCAL: &[&str] = &["resolved_rowid"];
    const DEMO_SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
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

    #[test]
    fn a_spec_that_classifies_every_column_passes() {
        assert!(assert_spec_covers_schema(&demo_conn(), &DEMO_SPEC).is_ok());
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
            "CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT NOT NULL, resolved_rowid \
             INTEGER) STRICT;",
        )
        .unwrap();
        let err = assert_spec_covers_schema(&conn, &DEMO_SPEC).unwrap_err();
        assert!(err.contains("count"), "the phantom column is named: {err}");
    }

    #[test]
    fn a_column_classified_twice_fails() {
        const DOUBLED: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "repo_id", value_type: ValueType::Text }],
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "group_id", value_type: ValueType::Text }, ColumnSpec {
                name: "member_id",
                value_type: ValueType::Text,
            }],
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
            "CREATE TABLE t_pk(a TEXT NOT NULL, b TEXT NOT NULL, v TEXT, PRIMARY KEY(a, b)) \
             STRICT;",
        )
        .unwrap();
        const WRONG_PK: TableSpec = TableSpec {
            name: "t_pk",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "a", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "b", value_type: ValueType::Text }, ColumnSpec {
                name: "v",
                value_type: ValueType::Text,
            }],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &WRONG_PK).unwrap_err();
        assert!(
            err.contains("does not match the table's primary key"),
            "the pk mismatch is named: {err}"
        );
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "syn", value_type: ValueType::Text }],
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
            "CREATE TABLE t_def_local(id TEXT PRIMARY KEY, syn TEXT, loc INTEGER NOT NULL DEFAULT \
             0) STRICT;",
        )
        .unwrap();
        const DEF_LOCAL: TableSpec = TableSpec {
            name: "t_def_local",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "syn", value_type: ValueType::Text }],
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }, ColumnSpec {
                name: "p",
                value_type: ValueType::Text,
            }],
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
        conn.execute_batch(
            "CREATE TABLE t_u(id TEXT NOT NULL PRIMARY KEY, email TEXT UNIQUE) STRICT;",
        )
        .unwrap();
        const U: TableSpec = TableSpec {
            name: "t_u",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "email", value_type: ValueType::Text }],
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "n", value_type: ValueType::Text }],
            local_columns: &[],
            repo_column: None,
        };
        let err = assert_spec_covers_schema(&conn, &TM).unwrap_err();
        assert!(
            err.contains("physical type"),
            "a ValueType/physical-type mismatch is rejected: {err}"
        );
    }

    #[test]
    fn a_table_registered_under_two_scopes_fails() {
        const A: TableSpec = TableSpec {
            name: "t_dup",
            scope_id: "scope-a/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
            local_columns: &[],
            repo_column: None,
        };
        const B: TableSpec = TableSpec {
            name: "t_dup",
            scope_id: "scope-b/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
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
            "CREATE TABLE t_ri(rid INTEGER NOT NULL, id TEXT NOT NULL, v TEXT, PRIMARY KEY(rid, \
             id)) STRICT;",
        )
        .unwrap();
        const RI: TableSpec = TableSpec {
            name: "t_ri",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "rid", value_type: ValueType::I64 }, ColumnSpec {
                name: "id",
                value_type: ValueType::Text,
            }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
            local_columns: &[],
            repo_column: Some("rid"),
        };
        let err = assert_spec_covers_schema(&conn, &RI).unwrap_err();
        assert!(
            err.contains("must be ValueType::Text"),
            "a non-Text repo column is rejected: {err}"
        );
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
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
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
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
            "CREATE TABLE t_trig(id TEXT NOT NULL PRIMARY KEY, v TEXT, n INTEGER NOT NULL DEFAULT \
             0) STRICT;
             CREATE TRIGGER t_trig_ai AFTER INSERT ON t_trig
                 BEGIN UPDATE t_trig SET n = n + 1 WHERE id = NEW.id; END;",
        )
        .unwrap();
        const TRIG: TableSpec = TableSpec {
            name: "t_trig",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }, ColumnSpec {
                name: "n",
                value_type: ValueType::I64,
            }],
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
            assert_spec_covers_schema(&conn, spec).unwrap_or_else(|err| {
                panic!("registered table `{}` is not covered: {err}", spec.name)
            });
        }
    }
}
