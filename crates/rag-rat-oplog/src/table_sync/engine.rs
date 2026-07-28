//! The engine's orchestration surface: turn local table changes into authored entries, and fold
//! received entries back into tables.
//!
//! [`produce_and_author`] scans every registered table, authors an entry per changed row on that
//! table's scope stream, and self-applies it so the LWW clock and published-row record capture this
//! authorship (a later remote op competes at the authored lamport; the next producer pass sees no
//! delta). [`ingest`] verifies, stores, and applies one received entry, routing it to the right
//! table within its scope.
//!
//! This is the transport-independent seam the milestone's loopback test drives; the iroh milestone
//! wraps a per-scope `SyncStore` around exactly these two calls.

use rusqlite::Transaction;

use super::apply::{self, ApplyOutcome};
use super::registry::TableSpec;
use super::scope_stream::scope_stream_id;
use super::store::{self, AcceptOutcome};
use super::{produce, refold};
use crate::device::DevicePublic;
use crate::op::OpMeta;
use crate::{AccountId, LocalDevice};

/// The stable dependencies of a sync pass: the project being synced, the owning account (which the
/// scope stream ids derive from), this device, the syncable-table registry, and the injected clock.
pub(crate) struct SyncCtx<'a> {
    pub repo_id: &'a str,
    pub account_id: AccountId,
    pub device: &'a LocalDevice,
    pub registry: &'a [TableSpec],
    pub now_ms: i64,
}

/// What ingesting one received entry did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IngestOutcome {
    Applied,
    /// Stored and relayed, but not applied — an undecodable/unknown/out-of-scope payload. The
    /// `&str` is the reason. Forward-compatible: the chain advanced, the payload is retained.
    Retained(&'static str),
    AlreadyPresent,
    /// A chain gap or fork — the transport milestone resolves it (backfill / fork evidence).
    Parked(&'static str),
    /// A type mismatch — stored, unprojectable, surfaced.
    Quarantined(String),
    /// The signing device is not a roster-effective writer (off-roster, removed, or read-only), so
    /// the entry was DROPPED (#935). RETRYABLE — the local fold may lag the author's `DeviceAdd`; a
    /// caller must not treat it as peer misbehavior, and the frontier re-offers it once the account
    /// log delivers the enrollment.
    Unauthorized,
}

/// Author the row ops that bring peers up to this device's state, returning each as signed wire
/// bytes. Empty when everything is already published.
pub(crate) fn produce_and_author(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
) -> anyhow::Result<Vec<Vec<u8>>> {
    // Never author into a store a NEWER projector folded: our narrower column set would record
    // anti-echo hashes and park decisions the newer binary has to distrust.
    refold::assert_projector_not_newer(tx)?;
    let mut authored = Vec::new();
    for spec in ctx.registry {
        let stream = scope_stream_id(ctx.repo_id, ctx.account_id, spec.scope_id);
        // Record the apply context for every stream we author on: the stream id hashes
        // (repo_id, account_id, scope_id) one-way, so without the directory a retained entry could
        // never be replayed by a later binary (see [`super::refold`]).
        store::record_stream_context(tx, stream, ctx.repo_id, ctx.account_id, spec.scope_id)?;
        for op in produce::produce_row_ops(tx, spec, ctx.repo_id)? {
            let signed = store::author_row_entry(tx, stream, ctx.device.secret(), &op, ctx.now_ms)?;
            let meta =
                OpMeta { lamport: signed.entry.lamport, device: signed.entry.device_fingerprint };
            // Self-apply so this authorship enters the LWW clock and published-row record: a later
            // remote op competes at this lamport, and the producer never re-emits it (no
            // self-echo). A locally-produced op MUST self-apply cleanly — the registry lint rejects
            // every shape that would quarantine (nullable pk, cross-row constraint, and the
            // producer reads well-typed values). If it quarantines anyway, the
            // published hash is NOT recorded, so the next pass would re-author the same
            // row forever (unbounded signed-log growth) and peers would quarantine each
            // copy: surface it and do NOT transmit the junk op.
            match apply::apply_row_op(tx, spec, ctx.repo_id, &op, meta)? {
                apply::ApplyOutcome::Applied => authored.push(signed.signed_bytes),
                // A locally-produced op failing to self-apply is UNREACHABLE for a registered
                // table: the lint rejects every shape that would quarantine (nullable pk, cross-row
                // constraint, ValueType/physical-type mismatch), the producer reads well-typed
                // values, and it emits only columns from THIS registry so it can never carry one we
                // do not know. If it somehow happens, FAIL the pass — `author_row_entry` has
                // already inserted this entry in the caller's txn, so bailing rolls
                // it back rather than leaving it stored-but-unpublished and
                // re-authored (re-signed) every pass (unbounded log growth). Assert
                // first, so a test catches the lint/producer gap.
                outcome @ (apply::ApplyOutcome::Quarantined(_)
                | apply::ApplyOutcome::Unprojectable(_)) => {
                    debug_assert!(
                        false,
                        "table-sync: a locally-produced op did not self-apply on `{}`: {outcome:?}",
                        spec.name
                    );
                    anyhow::bail!(
                        "table-sync: a locally-produced op did not self-apply on `{}`: {outcome:?}",
                        spec.name
                    );
                },
            }
        }
    }
    Ok(authored)
}

/// Verify, store, and apply one received entry for `scope_id`'s stream, signed by `pubkey`. A scope
/// may carry several tables (overlay, distill), so the target spec is resolved from the decoded
/// op's table against every registry entry in the scope — not fixed by the caller. The op's table
/// is validated against the scope's table set BEFORE the entry is stored, so a misrouted op never
/// advances the chain and orphans itself.
pub(crate) fn ingest(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
    scope_id: &str,
    signed_bytes: &[u8],
    pubkey: &DevicePublic,
) -> anyhow::Result<IngestOutcome> {
    // Same refusal as the producer: an older binary must not re-park, under its own version, an
    // entry a newer projector already understood and folded.
    refold::assert_projector_not_newer(tx)?;
    let stream = scope_stream_id(ctx.repo_id, ctx.account_id, scope_id);
    // The apply context for anything this stream retains — recorded before the entry is stored, so
    // a pending entry is never left without the mapping its replay needs.
    store::record_stream_context(tx, stream, ctx.repo_id, ctx.account_id, scope_id)?;
    let scope_tables: Vec<&str> =
        ctx.registry.iter().filter(|s| s.scope_id == scope_id).map(|s| s.name).collect();
    Ok(
        match store::accept_row_entry(
            tx,
            ctx.account_id,
            stream,
            &scope_tables,
            signed_bytes,
            pubkey,
            ctx.now_ms,
        )? {
            AcceptOutcome::Stored { op, meta, entry_hash } => {
                // `accept_row_entry` already validated the op's table is in `scope_tables`, so
                // exactly one spec matches; the fallback is defensive, never
                // reached.
                let Some(spec) =
                    ctx.registry.iter().find(|s| s.scope_id == scope_id && s.name == op.table())
                else {
                    return Ok(IngestOutcome::Parked("table not in scope"));
                };
                match apply::apply_row_op(tx, spec, ctx.repo_id, &op, meta)? {
                    ApplyOutcome::Applied => IngestOutcome::Applied,
                    ApplyOutcome::Quarantined(why) => IngestOutcome::Quarantined(why),
                    // A newer producer's column: nothing was written. Mark the stored entry so the
                    // refold replays it once this binary learns the column — only the applier can
                    // see this, which is why `accept_row_entry` handed back the entry hash.
                    ApplyOutcome::Unprojectable(reason) => {
                        store::mark_entry_pending(
                            tx,
                            &entry_hash,
                            reason,
                            refold::TABLE_SYNC_PROJECTOR_VERSION,
                        )?;
                        IngestOutcome::Retained(reason.as_db_str())
                    },
                }
            },
            AcceptOutcome::StoredInert(reason) => IngestOutcome::Retained(reason.as_db_str()),
            AcceptOutcome::AlreadyPresent => IngestOutcome::AlreadyPresent,
            AcceptOutcome::MissingPredecessor => IngestOutcome::Parked("missing predecessor"),
            AcceptOutcome::Fork => IngestOutcome::Parked("fork"),
            AcceptOutcome::Unauthorized => IngestOutcome::Unauthorized,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_sync::registry::{ColumnSpec, ValueType};

    const SPEC: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
        columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }],
        local_columns: &[],
        repo_column: None,
    };
    const REGISTRY: &[TableSpec] = &[SPEC];

    /// One account's device: a fully-migrated store with the synthetic table and a minted identity.
    struct Device {
        conn: rusqlite::Connection,
        local: LocalDevice,
    }

    impl Device {
        fn new() -> Self {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            conn.execute_batch("CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT) STRICT;")
                .unwrap();
            let local = crate::local_device(&conn, 0).unwrap();
            Self { conn, local }
        }

        fn pubkey(&self) -> DevicePublic {
            self.local.secret().public()
        }

        fn set_title(&self, title: &str) {
            self.conn.execute("UPDATE t_demo SET title = ?1 WHERE id = 'r1'", [title]).unwrap();
        }

        fn title(&self) -> Option<String> {
            self.conn
                .query_row("SELECT title FROM t_demo WHERE id = 'r1'", [], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
        }

        fn produce(&mut self) -> Vec<Vec<u8>> {
            let tx = self.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: AccountId::from_bytes([42; 32]),
                device: &self.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            let out = produce_and_author(&tx, &ctx).unwrap();
            tx.commit().unwrap();
            out
        }

        fn ingest_all(&mut self, entries: &[Vec<u8>], from: &DevicePublic) {
            // The receiver has folded the author's DeviceAdd, so it is an effective writer here —
            // otherwise the #935 authority gate would drop every entry as Unauthorized.
            enroll_writer(&self.conn, AccountId::from_bytes([42; 32]), from.fingerprint());
            let tx = self.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: AccountId::from_bytes([42; 32]),
                device: &self.local,
                registry: REGISTRY,
                now_ms: 0,
            };
            for bytes in entries {
                ingest(&tx, &ctx, "demo/1", bytes, from).unwrap();
            }
            tx.commit().unwrap();
        }
    }

    /// Enroll `fp` as a roster-effective writer (Owner) of `account`, so the #935 ingest gate
    /// admits its entries — the receiver-side view after it has folded the author's
    /// `DeviceAdd`.
    fn enroll_writer(
        conn: &rusqlite::Connection,
        account: AccountId,
        fp: crate::op::DeviceFingerprint,
    ) {
        conn.execute(
            "INSERT OR IGNORE INTO account_roster_history
                 (roster_ref, account_id, device_fingerprint, role, effective_at, closed_at)
             VALUES (?1, ?2, ?3, 'owner', 0, NULL)",
            rusqlite::params![
                fp.to_bytes().as_slice(),
                account.to_bytes().as_slice(),
                fp.to_bytes().as_slice()
            ],
        )
        .unwrap();
    }

    #[test]
    fn a_row_written_on_one_device_appears_on_the_other_and_never_echoes() {
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();

        let entries = a.produce();
        assert_eq!(entries.len(), 1, "one changed row is authored");
        b.ingest_all(&entries, &a.pubkey());
        assert_eq!(b.title().as_deref(), Some("base"), "the row appears on the peer");

        // The flagship: the peer does not re-emit a row it received.
        assert!(b.produce().is_empty(), "a received row never echoes back");
        // And the author does not re-emit its own already-published row.
        assert!(a.produce().is_empty(), "a published row is not re-authored");
    }

    #[test]
    fn a_later_edit_supersedes_across_devices_and_both_converge() {
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());

        // A edits, syncs to B; then B edits (its lamport is now one past A's) and syncs back. B's
        // edit is causally later, so it wins on BOTH devices — a proper Lamport-clock supersede,
        // not a fingerprint coin-flip.
        a.set_title("from-A");
        b.ingest_all(&a.produce(), &a.pubkey());
        assert_eq!(b.title().as_deref(), Some("from-A"), "A's edit reached B");

        b.set_title("from-B");
        a.ingest_all(&b.produce(), &b.pubkey());

        assert_eq!(a.title(), b.title(), "the devices converge");
        assert_eq!(a.title().as_deref(), Some("from-B"), "the causally-later edit wins on both");

        // Steady state: nothing left to produce on either side.
        assert!(a.produce().is_empty());
        assert!(b.produce().is_empty());
    }

    #[test]
    fn a_scope_with_multiple_tables_routes_each_op_to_its_table() {
        const TA: TableSpec = TableSpec {
            name: "t_a",
            scope_id: "multi/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
            local_columns: &[],
            repo_column: None,
        };
        const TB: TableSpec = TableSpec {
            name: "t_b",
            scope_id: "multi/1",
            pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
            columns: &[ColumnSpec { name: "v", value_type: ValueType::Text }],
            local_columns: &[],
            repo_column: None,
        };
        const MULTI: &[TableSpec] = &[TA, TB];

        let setup = || {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            conn.execute_batch(
                "CREATE TABLE t_a(id TEXT PRIMARY KEY, v TEXT) STRICT;
                 CREATE TABLE t_b(id TEXT PRIMARY KEY, v TEXT) STRICT;",
            )
            .unwrap();
            let local = crate::local_device(&conn, 0).unwrap();
            (conn, local)
        };
        let (mut a_conn, a_dev) = setup();
        let (mut b_conn, b_dev) = setup();
        let account = AccountId::from_bytes([42; 32]);

        // A writes a row into each table (both tables share the `multi/1` scope stream).
        a_conn.execute("INSERT INTO t_a(id, v) VALUES ('r', 'in-a')", []).unwrap();
        a_conn.execute("INSERT INTO t_b(id, v) VALUES ('r', 'in-b')", []).unwrap();
        let entries = {
            let tx = a_conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                device: &a_dev,
                registry: MULTI,
                now_ms: 0,
            };
            let e = produce_and_author(&tx, &ctx).unwrap();
            tx.commit().unwrap();
            e
        };
        assert_eq!(entries.len(), 2, "one op per table in the scope");

        // B has folded A's DeviceAdd, so A is an effective writer here (else the #935 gate drops
        // it).
        enroll_writer(&b_conn, account, a_dev.secret().public().fingerprint());
        // B ingests both over the ONE shared scope stream; each must route to its own table.
        {
            let tx = b_conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id: "repo",
                account_id: account,
                device: &b_dev,
                registry: MULTI,
                now_ms: 0,
            };
            for bytes in &entries {
                assert_eq!(
                    ingest(&tx, &ctx, "multi/1", bytes, &a_dev.secret().public()).unwrap(),
                    IngestOutcome::Applied,
                );
            }
            tx.commit().unwrap();
        }
        let a_val: String =
            b_conn.query_row("SELECT v FROM t_a WHERE id = 'r'", [], |r| r.get(0)).unwrap();
        let b_val: String =
            b_conn.query_row("SELECT v FROM t_b WHERE id = 'r'", [], |r| r.get(0)).unwrap();
        assert_eq!(
            (a_val.as_str(), b_val.as_str()),
            ("in-a", "in-b"),
            "each op landed in its own table"
        );
    }
}
