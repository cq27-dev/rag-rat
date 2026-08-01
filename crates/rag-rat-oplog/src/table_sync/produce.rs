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
use crate::stream::StreamId;

/// The row ops that carry this device's current state of `spec`'s table for `repo_id` to peers.
/// Empty when everything is already published (the steady state).
///
/// NOT READ-ONLY, despite reading like a query: settling a stale-version row that turns out to be
/// unchanged rewrites its `sync_published_rows` record in place (#1002). An empty return therefore
/// does NOT mean "nothing to commit" — a caller that skips the commit on `Ok(vec![])` merely
/// discards the restamps and repeats the winner lookups next pass, and a read-only transaction
/// would fail here outright.
pub(crate) fn produce_row_ops(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<Vec<RowOp>> {
    let mut ops = Vec::new();
    let mut live: BTreeSet<String> = BTreeSet::new();

    for scanned in apply::read_all_rows(tx, spec, repo_id)? {
        let (pk, cells) = match scanned {
            apply::ScannedRow::Readable { pk, cells } => (pk, cells),
            // The row exists and cannot be carried, so it is published as whatever it last was —
            // but its identity MUST still count as live. Dropping it here instead would leave a
            // published identity with no live row, which the loop below reads as a local delete and
            // authors a `Remove` for: an unreadable cell on one device would then delete the row on
            // every peer.
            apply::ScannedRow::Unpublishable { pk } => {
                live.insert(row_op::row_pk_string(&pk));
                continue;
            },
            // No pk means no identity to keep alive, so this row is deliberately NOT added to
            // `live`: whatever pk was published genuinely has no row carrying it any more, and a
            // `Remove` for that identity is the same answer a delete would get.
            apply::ScannedRow::Unaddressable => continue,
        };
        let row_pk = row_op::row_pk_string(&pk);
        live.insert(row_pk.clone());
        let hash = row_op::cells_hash(&cells);
        let changed =
            match apply::published_hash_on_stream(tx, stream, repo_id, spec.name, &row_pk)? {
                // Published under THIS binary's column set: a differing hash is a real local
                // change.
                Some((published, version)) if version == spec.spec_version => published != hash,
                // Published under a DIFFERENT column set: the two hashes cover different cell
                // lists, so comparing them says nothing (they differ structurally
                // whether or not the row changed). Resolve it against the op that
                // actually established the row, projected under this spec — the
                // only thing that CAN settle it.
                //
                // Reading the raw mismatch as a delta instead would re-author EVERY row of the
                // table at a fresh winning lamport on EVERY upgrading device;
                // ignoring it entirely (the #1001 conservatism this replaces) left
                // the row permanently un-authorable, even when genuinely edited.
                Some(_) =>
                    match apply::stale_row_disposition(tx, spec, repo_id, stream, &pk, &cells)? {
                        // Untouched since it landed: nothing to say, just restamp the bookkeeping
                        // so the row is comparable again from here on.
                        apply::StaleRow::Unchanged => {
                            apply::record_published(
                                tx,
                                stream,
                                repo_id,
                                spec.name,
                                &row_pk,
                                &hash,
                                spec.spec_version,
                            )?;
                            false
                        },
                        // A proven local change — author it. This is the exit from the frozen
                        // state.
                        apply::StaleRow::LocallyChanged => true,
                        // Unprovable (the winning entry is gone, does not project here, or is not
                        // this row's op). AUTHOR IT — the two readers of
                        // this verdict must not both defer.
                        // `row_has_unsent_local_change` reads `Unknown` as "there may be an unsent
                        // edit, so refuse to replay over it"; if the producer also
                        // declined, the row would be permanently unauthorable AND
                        // would permanently block its own pending entries, silently
                        // losing a genuine local edit with no way out and nothing to report it.
                        // Authoring is the safe direction: at worst it re-emits a row that was
                        // already correct (bounded churn, and the restamp
                        // makes the next pass cheap), and at
                        // best it publishes an edit that would otherwise have been lost. Not
                        // authoring has no such floor.
                        apply::StaleRow::Unknown => true,
                    },
                // Never published: a genuinely new local row.
                None => true,
            };
        if changed {
            ops.push(RowOp::Upsert {
                table: spec.name.to_string(),
                spec_version: spec.spec_version,
                pk,
                cells,
            });
        }
    }

    // A published identity with no live row is a local delete. Deliberately version-AGNOSTIC: a
    // `Remove` carries only the pk, so which column set the stored hash covered is irrelevant.
    // Gating this on the version would make a locally-deleted row permanently undeletable across a
    // column change — the delete could never be authored, and peers would keep the row forever.
    for row_pk in published_row_pks(tx, stream, repo_id, spec.name)? {
        if !live.contains(&row_pk) {
            ops.push(RowOp::Remove {
                table: spec.name.to_string(),
                spec_version: spec.spec_version,
                pk: row_op::row_pk_values(&row_pk)?,
            });
        }
    }
    Ok(ops)
}

fn published_row_pks(
    tx: &Transaction<'_>,
    stream: StreamId,
    repo_id: &str,
    table: &str,
) -> anyhow::Result<Vec<String>> {
    let mut stmt = tx.prepare(
        "SELECT row_pk FROM sync_published_rows
              WHERE stream_id = ?1 AND repo_id = ?2 AND table_name = ?3",
    )?;
    let rows = stmt
        .query_map(params![stream.to_bytes().as_slice(), repo_id, table], |row| {
            row.get::<_, String>(0)
        })?
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
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &["resolved_rowid"],
        repo_column: None,
    };

    /// Any stable stream id: these tests never reach the winner lookup (no row is ever stale).
    fn test_stream() -> crate::stream::StreamId {
        crate::stream::StreamId::from_bytes([0; 32])
    }

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
            spec_version: 1,
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
        let ops = produce_row_ops(&tx, &SPEC, "repo", test_stream()).unwrap();
        assert_eq!(ops.len(), 1, "an unpublished local row is produced");
        assert!(matches!(&ops[0], RowOp::Upsert { .. }));

        // Applying our own op records the published hash; the next pass sees no delta.
        apply_row_op(&tx, &SPEC, "repo", &ops[0], OpMeta {
            lamport: 1,
            device: DeviceFingerprint::from_bytes([1; 32]),
        })
        .unwrap();
        assert!(
            produce_row_ops(&tx, &SPEC, "repo", test_stream()).unwrap().is_empty(),
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
            produce_row_ops(&tx, &SPEC, "repo", test_stream()).unwrap().is_empty(),
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
        let ops = produce_row_ops(&tx, &SPEC, "repo", test_stream()).unwrap();
        assert_eq!(ops.len(), 1, "a changed row is produced again");
    }

    #[test]
    fn a_repo_scoped_producer_emits_only_the_current_repo() {
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
             ) STRICT;
             INSERT INTO t_scoped(repo_id, id, title) VALUES ('A','r1','a1'), ('B','r2','b1');",
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        let ops = produce_row_ops(&tx, &SCOPED, "A", test_stream()).unwrap();
        assert_eq!(ops.len(), 1, "only repo A's row is produced — never repo B's");
        assert_eq!(
            ops[0].pk()[0],
            TypedValue::Text("A".to_string()),
            "the produced row belongs to the repo being synced",
        );
    }

    /// A spec whose synced column is a `Bool`, plus its table — the one shape a STRICT schema can
    /// leave unreadable (#1017).
    const FLAGGED: TableSpec = TableSpec {
        name: "t_flagged",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("flag", ValueType::Bool)],
        local_columns: &[],
        repo_column: None,
    };

    fn flagged_conn() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&c, &crate::test_hooks()).unwrap();
        c.execute_batch("CREATE TABLE t_flagged(id TEXT PRIMARY KEY, flag INTEGER) STRICT;")
            .unwrap();
        c
    }

    fn flag_upsert(id: &str, flag: bool) -> RowOp {
        RowOp::Upsert {
            spec_version: 1,
            table: "t_flagged".to_string(),
            pk: vec![TypedValue::Text(id.to_string())],
            cells: vec![Cell { column: "flag".to_string(), value: TypedValue::Bool(flag) }],
        }
    }

    #[test]
    fn a_row_that_cannot_be_read_is_neither_authored_nor_removed() {
        // The trap: an unreadable row that is simply skipped disappears from the live set, and the
        // published identity it leaves behind reads as a local delete — so one device's malformed
        // cell would author a `Remove` that deletes the row on EVERY peer.
        let mut c = flagged_conn();
        let tx = c.transaction().unwrap();
        apply_row_op(&tx, &FLAGGED, "repo", &flag_upsert("r1", true), OpMeta {
            lamport: 1,
            device: DeviceFingerprint::from_bytes([1; 32]),
        })
        .unwrap();
        assert!(produce_row_ops(&tx, &FLAGGED, "repo", test_stream()).unwrap().is_empty());

        // A raw local write puts the column outside the Bool domain STRICT cannot pin.
        tx.execute("UPDATE t_flagged SET flag = 2 WHERE id = 'r1'", []).unwrap();
        assert!(
            produce_row_ops(&tx, &FLAGGED, "repo", test_stream()).unwrap().is_empty(),
            "an unreadable row is not carried in an op — and above all is not deleted on peers",
        );
    }

    #[test]
    fn an_unreadable_row_does_not_stop_the_rest_of_the_table() {
        // One bad row must not cost the pass: before #1017 the scan errored, taking every other
        // row's op with it.
        let mut c = flagged_conn();
        let tx = c.transaction().unwrap();
        tx.execute("INSERT INTO t_flagged(id, flag) VALUES ('bad', 2), ('good', 1)", []).unwrap();
        let ops = produce_row_ops(&tx, &FLAGGED, "repo", test_stream()).unwrap();
        assert_eq!(ops.len(), 1, "the readable row is still produced");
        assert_eq!(ops[0].pk(), vec![TypedValue::Text("good".to_string())]);
    }

    #[test]
    fn a_row_whose_pk_cannot_be_read_releases_the_identity_it_was_published_under() {
        // The pk case is the other half, and it goes the other way: with no readable pk there is no
        // identity to keep alive, so the pk that WAS published genuinely has no row carrying it —
        // exactly what a delete means. Suppressing the `Remove` instead would leave peers holding a
        // row this device can never speak about again.
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
        c.execute_batch("CREATE TABLE t_flag(active INTEGER PRIMARY KEY, label TEXT) STRICT;")
            .unwrap();
        let tx = c.transaction().unwrap();
        apply_row_op(
            &tx,
            &FLAG_PK,
            "repo",
            &RowOp::Upsert {
                spec_version: 1,
                table: "t_flag".to_string(),
                pk: vec![TypedValue::Bool(true)],
                cells: vec![Cell {
                    column: "label".to_string(),
                    value: TypedValue::Text("on".into()),
                }],
            },
            OpMeta { lamport: 1, device: DeviceFingerprint::from_bytes([1; 32]) },
        )
        .unwrap();
        assert!(produce_row_ops(&tx, &FLAG_PK, "repo", test_stream()).unwrap().is_empty());

        tx.execute("UPDATE t_flag SET active = 2", []).unwrap();
        let ops = produce_row_ops(&tx, &FLAG_PK, "repo", test_stream()).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(
            matches!(&ops[0], RowOp::Remove { .. }),
            "the published pk no longer names a live row, so it is a delete",
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
        let ops = produce_row_ops(&tx, &SPEC, "repo", test_stream()).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], RowOp::Remove { .. }), "a deleted row produces a Remove");
    }
}
