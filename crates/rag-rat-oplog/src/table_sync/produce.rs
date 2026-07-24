//! The producer: a full-pass scan that emits the row ops needed to bring peers up to this device's
//! state, and NOTHING for a row already in sync.
//!
//! For each current row it recomputes the synced-column hash and compares it against the
//! published-rows record. No record or a stale one means a local change → an `Upsert`. A published
//! identity with no live row means a local delete → a `Remove`. A row whose hash already matches
//! the published record is skipped — and because the applier records that hash when it lands a
//! remote row, **a row this device received is never re-emitted**. That is the anti-echo invariant:
//! without it every applied remote row would be re-signed and rebroadcast by every peer, forever.

use std::collections::BTreeSet;

use rusqlite::{Transaction, params};

use super::apply;
use super::registry::TableSpec;
use super::row_op::{self, RowOp};

/// The row ops that carry this device's current state of `spec`'s table for `repo_id` to peers.
/// Empty when everything is already published (the steady state).
pub(crate) fn produce_row_ops(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
) -> anyhow::Result<Vec<RowOp>> {
    let mut ops = Vec::new();
    let mut live: BTreeSet<String> = BTreeSet::new();

    for (pk, cells) in apply::read_all_rows(tx, spec, repo_id)? {
        let row_pk = row_op::row_pk_string(&pk);
        live.insert(row_pk.clone());
        let hash = row_op::cells_hash(&cells);
        if apply::published_hash(tx, repo_id, spec.name, &row_pk)?.as_deref() != Some(hash.as_str())
        {
            ops.push(RowOp::Upsert { table: spec.name.to_string(), pk, cells });
        }
    }

    // A published identity with no live row is a local delete.
    for row_pk in published_row_pks(tx, repo_id, spec.name)? {
        if !live.contains(&row_pk) {
            ops.push(RowOp::Remove {
                table: spec.name.to_string(),
                pk: row_op::row_pk_values(&row_pk)?,
            });
        }
    }
    Ok(ops)
}

fn published_row_pks(
    tx: &Transaction<'_>,
    repo_id: &str,
    table: &str,
) -> anyhow::Result<Vec<String>> {
    let mut stmt = tx
        .prepare("SELECT row_pk FROM sync_published_rows WHERE repo_id = ?1 AND table_name = ?2")?;
    let rows = stmt
        .query_map(params![repo_id, table], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{DeviceFingerprint, OpMeta};
    use crate::table_sync::apply::apply_row_op;
    use crate::table_sync::registry::{ColumnSpec, TableSpec, ValueType};
    use crate::table_sync::row_op::{Cell, TypedValue};

    const SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
        columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }],
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

    fn upsert(id: &str, title: &str) -> RowOp {
        RowOp::Upsert {
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text(id.to_string())],
            cells: vec![Cell {
                column: "title".to_string(),
                value: TypedValue::Text(title.to_string()),
            }],
        }
    }

    #[test]
    fn a_locally_written_row_is_produced_once_then_never_again() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        // A row written WITHOUT going through the published-hash record (a raw local insert).
        tx.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'hi')", []).unwrap();
        let ops = produce_row_ops(&tx, &SPEC, "repo").unwrap();
        assert_eq!(ops.len(), 1, "an unpublished local row is produced");
        assert!(matches!(&ops[0], RowOp::Upsert { .. }));

        // Applying our own op records the published hash; the next pass sees no delta.
        apply_row_op(&tx, &SPEC, "repo", &ops[0], OpMeta {
            lamport: 1,
            device: DeviceFingerprint::from_bytes([1; 32]),
        })
        .unwrap();
        assert!(
            produce_row_ops(&tx, &SPEC, "repo").unwrap().is_empty(),
            "a published row is not re-produced"
        );
    }

    #[test]
    fn a_received_row_is_never_re_emitted() {
        // The flagship anti-echo case: applying a REMOTE op records the published hash, so this
        // device's producer emits nothing for it — no ping-pong.
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(&tx, &SPEC, "repo", &upsert("r1", "from-peer"), OpMeta {
            lamport: 9,
            device: DeviceFingerprint::from_bytes([7; 32]),
        })
        .unwrap();
        assert!(
            produce_row_ops(&tx, &SPEC, "repo").unwrap().is_empty(),
            "a row received from a peer must not be re-signed and rebroadcast",
        );
    }

    #[test]
    fn a_changed_row_is_re_produced() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(&tx, &SPEC, "repo", &upsert("r1", "v1"), OpMeta {
            lamport: 1,
            device: DeviceFingerprint::from_bytes([1; 32]),
        })
        .unwrap();
        // A raw local edit (not through apply) leaves the published hash stale.
        tx.execute("UPDATE t_demo SET title = 'v2' WHERE id = 'r1'", []).unwrap();
        let ops = produce_row_ops(&tx, &SPEC, "repo").unwrap();
        assert_eq!(ops.len(), 1, "a changed row is produced again");
    }

    #[test]
    fn a_repo_scoped_producer_emits_only_the_current_repo() {
        const SCOPED: TableSpec = TableSpec {
            name: "t_scoped",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "repo_id", value_type: ValueType::Text }, ColumnSpec {
                name: "id",
                value_type: ValueType::Text,
            }],
            columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }],
            local_columns: &[],
            repo_column: Some("repo_id"),
        };
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch(
            "CREATE TABLE t_scoped(
                 repo_id TEXT NOT NULL, id TEXT NOT NULL, title TEXT, PRIMARY KEY(repo_id, id)
             ) STRICT;
             INSERT INTO t_scoped(repo_id, id, title) VALUES ('A','r1','a1'), ('B','r2','b1');",
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        let ops = produce_row_ops(&tx, &SCOPED, "A").unwrap();
        assert_eq!(ops.len(), 1, "only repo A's row is produced — never repo B's");
        assert_eq!(
            ops[0].pk()[0],
            TypedValue::Text("A".to_string()),
            "the produced row belongs to the repo being synced",
        );
    }

    #[test]
    fn a_deleted_row_produces_a_remove() {
        let mut c = conn();
        let tx = c.transaction().unwrap();
        apply_row_op(&tx, &SPEC, "repo", &upsert("r1", "v1"), OpMeta {
            lamport: 1,
            device: DeviceFingerprint::from_bytes([1; 32]),
        })
        .unwrap();
        // Row gone but its published identity remains → a Remove is produced.
        tx.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();
        let ops = produce_row_ops(&tx, &SPEC, "repo").unwrap();
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], RowOp::Remove { .. }), "a deleted row produces a Remove");
    }
}
