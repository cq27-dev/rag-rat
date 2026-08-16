//! The ladder's end state, provisioned in one pass on a database that has nothing in it yet.
//!
//! A fresh database used to reach the current schema the same way a lived-in one does: provision
//! the V001 baseline, then replay all 111 additive migrations, several of which rebuild tables the
//! step before them just created. That is the right shape for a database with data in it, and the
//! wrong shape for an empty one — it costs ~390ms to arrive somewhere reachable in ~25ms, and the
//! test suite pays that per test because each test opens its own scratch database.
//!
//! So an EMPTY database is provisioned from [`END_STATE_SQL`] — a generated dump of what the
//! ladder produces, schema and seeded rows both — and the ladder is left untouched for every
//! database that already exists. `migrate_forward` is not involved here at all; a database with
//! any object in it never reaches this module.
//!
//! The generated file is only as trustworthy as the check that it matches, so
//! `end_state_matches_the_ladder` rebuilds both databases and compares them object by object and
//! row by row. Adding a migration without regenerating fails that test, and
//! `RR_REGEN_END_STATE=1 cargo test -p rag-rat-db end_state` regenerates it.

use rusqlite::Connection;

/// The dump `regenerate` writes: the ladder's schema and its seeded rows, in creation order.
const END_STATE_SQL: &str = include_str!("end_state.sql");

/// `index_meta`'s rows are provenance — the binary version and path that migrated the store — so
/// they are re-stamped by `record_migration_provenance` on every path and would otherwise bake one
/// machine's executable path into a checked-in file.
///
/// Only the generator and the equivalence check build a dump; provisioning just replays the
/// generated file, so the machinery that produces it is test-only.
#[cfg(test)]
const GENERATED_WITHOUT_ROWS: &[&str] = &["index_meta"];

/// True when the database holds no objects at all — the only state in which the ladder can be
/// skipped, because there is nothing for it to migrate. A store that crashed midway through
/// `provision_baseline` still has `schema_version`, so it is not empty and takes the ladder.
pub(super) fn database_is_empty(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0))
        .map(|count| count == 0)
}

/// Bring an empty database to the ladder's end state.
pub(super) fn provision_end_state(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(END_STATE_SQL)?;
    // The ledger's timestamps are whenever the dump was generated, which is not when this store
    // was migrated. Every row lands at once here, so they legitimately share one instant.
    conn.execute("UPDATE schema_version SET applied_at_ms = ?1", [rag_rat_base::time::now_ms()])?;
    Ok(())
}

/// Everything the database contains, as SQL: each object's DDL in creation order, then each
/// table's rows. Used to generate the dump and to compare two databases, so a difference the
/// comparison would miss is a difference the dump would also have dropped.
///
/// `skip_rows_of` names tables whose rows are excluded — the dump leaves out provenance, while the
/// equivalence check leaves out nothing.
#[cfg(test)]
fn dump(conn: &Connection, skip_rows_of: &[&str]) -> rusqlite::Result<String> {
    let mut stmt = conn.prepare(
        "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid",
    )?;
    let objects: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    // An FTS5 table's shadow tables (`<name>_data`, `_idx`, `_config`, …) are created by SQLite
    // when the virtual table is declared and recreated the same way on replay, so the dump must
    // declare the virtual table and NOT its shadows. Nothing in the schema distinguishes a shadow
    // from an ordinary table by name alone; the prefix rule below would wrongly drop a real table
    // named after a virtual one, which is why `end_state_matches_the_ladder` compares objects
    // rather than trusting it.
    let virtual_tables: Vec<&str> = objects
        .iter()
        .filter(|(kind, _, sql)| kind == "table" && sql.contains("VIRTUAL TABLE"))
        .map(|(_, name, _)| name.as_str())
        .collect();
    let is_shadow = |name: &str| {
        virtual_tables.iter().any(|owner| {
            name.len() > owner.len() + 1
                && name.starts_with(owner)
                && name.as_bytes()[owner.len()] == b'_'
        })
    };
    // SQLite's own bookkeeping (`sqlite_sequence`, auto-indexes) is maintained by SQLite as the
    // rows that drive it are written, and cannot be created directly.
    let is_internal = |name: &str| name.starts_with("sqlite_");

    // Persisted in the database header rather than in any table, so a schema-and-rows dump would
    // silently drop them. Nothing sets either today; emitting them keeps that true by construction
    // if a migration ever starts to.
    let mut out = String::new();
    for pragma in ["application_id", "user_version"] {
        let value: i64 = conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))?;
        out.push_str(&format!("PRAGMA {pragma} = {value};\n"));
    }
    for (_, name, sql) in &objects {
        if is_internal(name) || is_shadow(name) {
            continue;
        }
        out.push_str(sql.trim_end());
        out.push_str(";\n");
    }

    for (kind, name, _) in &objects {
        if kind != "table"
            || is_internal(name)
            || is_shadow(name)
            || skip_rows_of.contains(&&**name)
        {
            continue;
        }
        for row in table_rows_as_inserts(conn, name)? {
            out.push_str(&row);
            out.push('\n');
        }
    }
    Ok(out)
}

/// One `INSERT` per row, with `quote()` rendering each value as the literal SQLite would parse
/// back to it — including NULLs, blobs, and reals. Ordered by every column so the dump is stable
/// across regenerations regardless of how the rows were written.
#[cfg(test)]
fn table_rows_as_inserts(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let columns: Vec<String> = {
        let stmt = conn.prepare(&format!("SELECT * FROM \"{table}\" LIMIT 0"))?;
        stmt.column_names().into_iter().map(str::to_owned).collect()
    };
    if columns.is_empty() {
        return Ok(Vec::new());
    }
    let quoted: Vec<String> = columns.iter().map(|c| format!("quote(\"{c}\")")).collect();
    let names: Vec<String> = columns.iter().map(|c| format!("\"{c}\"")).collect();
    let sql = format!(
        "SELECT 'INSERT INTO \"{table}\"({}) VALUES(' || {} || ');' FROM \"{table}\" ORDER BY {}",
        names.join(","),
        quoted.join(" || ',' || "),
        names.join(",")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows =
        stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{GENERATED_WITHOUT_ROWS, dump, provision_end_state};
    use crate::hooks::MigrationHooks;

    fn laddered() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::content_digest::register_content_digest_fold(&conn).unwrap();
        super::super::apply_ladder(&conn, &MigrationHooks::noop()).unwrap();
        conn
    }

    fn provisioned() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::content_digest::register_content_digest_fold(&conn).unwrap();
        provision_end_state(&conn).unwrap();
        crate::schema::migrations::record_migration_provenance(&conn).unwrap();
        conn
    }

    /// The dump is a checked-in copy of what the ladder builds, so it is exactly as correct as
    /// this comparison is thorough: every object's DDL and every table's rows, on both sides.
    /// A migration added without regenerating fails here rather than shipping a store whose
    /// schema silently lags the code that reads it.
    #[test]
    fn end_state_matches_the_ladder() {
        let ladder = laddered();
        if std::env::var_os("RR_REGEN_END_STATE").is_some() {
            let generated = dump(&ladder, GENERATED_WITHOUT_ROWS).unwrap();
            std::fs::write(
                concat!(env!("CARGO_MANIFEST_DIR"), "/src/schema/end_state.sql"),
                generated,
            )
            .unwrap();
            panic!(
                "regenerated src/schema/end_state.sql — re-run without RR_REGEN_END_STATE to \
                 verify"
            );
        }

        // When each store was migrated differs by construction — in the ledger, and in the
        // provenance `record_migration_provenance` stamps. Everything else must be identical.
        let normalize = |conn: &Connection| {
            conn.execute("UPDATE schema_version SET applied_at_ms = 0", []).unwrap();
            conn.execute("UPDATE index_meta SET value = '0' WHERE key = 'last_migration_at_ms'", [
            ])
            .unwrap();
            dump(conn, &[]).unwrap()
        };
        let fresh = provisioned();
        let (fresh, ladder) = (normalize(&fresh), normalize(&ladder));
        if fresh != ladder {
            let first_difference = fresh
                .lines()
                .zip(ladder.lines())
                .find(|(a, b)| a != b)
                .map(|(a, b)| format!("end_state: {a}\n  ladder: {b}"))
                .unwrap_or_else(|| {
                    format!(
                        "line counts differ: {} vs {}",
                        fresh.lines().count(),
                        ladder.lines().count()
                    )
                });
            panic!(
                "src/schema/end_state.sql no longer matches the ladder — regenerate it with \
                 RR_REGEN_END_STATE=1 cargo test -p rag-rat-db end_state\n{first_difference}"
            );
        }
    }

    /// The ledger is what `status` and `migrate_forward` read to decide what a store still owes.
    /// A provisioned store owes nothing, so it must record the whole ladder as applied — with the
    /// same checksums, or the next open reports a corrupt migration.
    #[test]
    fn a_provisioned_store_owes_no_migrations() {
        let conn = provisioned();
        let status = super::super::status(&conn).unwrap();
        assert_eq!(status.state, super::super::SchemaState::Compatible, "{status:?}");
        assert_eq!(status.current_version, super::super::LATEST_SCHEMA_VERSION);
        super::super::migrate_forward(&conn, &MigrationHooks::noop())
            .expect("a provisioned store is already forward-migrated");
    }

    /// Provisioning is only sound where there is nothing to migrate. A store with any object in it
    /// — the `index --full` replay path, or a torn `provision_baseline` — must take the ladder,
    /// because loading the end state over it would drop what is already there.
    #[test]
    fn a_store_with_any_object_in_it_is_not_empty() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(super::database_is_empty(&conn).unwrap());
        conn.execute_batch("CREATE TABLE anything(x)").unwrap();
        assert!(!super::database_is_empty(&conn).unwrap());
    }

    /// `index --full` re-runs `apply` against a populated store to repair it. That store is not
    /// empty, so it replays the ladder — and its rows must survive.
    #[test]
    fn applying_over_a_populated_store_replays_the_ladder_and_keeps_its_rows() {
        let conn = Connection::open_in_memory().unwrap();
        crate::content_digest::register_content_digest_fold(&conn).unwrap();
        super::super::apply(&conn, &MigrationHooks::noop()).unwrap();
        conn.execute("INSERT INTO index_meta(key, value) VALUES ('probe', 'kept')", []).unwrap();

        super::super::apply(&conn, &MigrationHooks::noop()).unwrap();

        let kept: String = conn
            .query_row("SELECT value FROM index_meta WHERE key = 'probe'", [], |row| row.get(0))
            .expect("re-applying over a populated store preserves its rows");
        assert_eq!(kept, "kept");
    }
}
