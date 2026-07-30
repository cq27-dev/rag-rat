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
        for op in produce::produce_row_ops(tx, spec, ctx.repo_id, stream)? {
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
                // A locally-authored op CANNOT lose its own self-apply while the row's bookkeeping
                // belongs to this stream: the entry took `MAX(lamport) + 1` over the whole stream,
                // so it outranks every clock any op on it could have set. Losing therefore proves
                // the row's clock came from a DIFFERENT stream — the shape a changed scope or
                // account leaves behind, where `sync_row_clocks` (keyed only by
                // `(repo_id, table_name, row_pk)`) survives a move its lamports have no meaning
                // after.
                //
                // FAIL rather than accept it. A superseded self-apply writes no published record,
                // so the next pass re-derives the identical delta and signs it again — growing the
                // log without bound and broadcasting entries every peer will also discard, until
                // the new stream's lamport happens to climb past the stale clock. Bailing rolls
                // back the entry `author_row_entry` just inserted, exactly as the quarantine arm
                // below does, and leaves an attributable error instead of silent churn.
                // No `debug_assert` here, unlike the arm below: that one is provably unreachable
                // (the lint rejects every shape that quarantines), so asserting is a dev-time
                // tripwire for a lint gap. This one is REACHABLE whenever a row's bookkeeping and
                // its stream come apart, which is precisely the condition worth reporting — a
                // panic would replace a diagnosable error with a crash.
                apply::ApplyOutcome::Superseded => {
                    anyhow::bail!(
                        "table-sync: a locally-produced op lost its own self-apply on `{}` — the \
                         row's write clock carries a lamport from another stream, so authoring \
                         cannot settle it",
                        spec.name
                    );
                },
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
    // Local work is now published, so anything ingest deferred behind it is unblocked — replay it
    // here rather than leaving it for the next store open. This is where author-before-apply
    // actually holds: a remote op deferred to protect a local edit gets its rematch immediately
    // after that edit is authored, and loses on the merits instead of by default.
    refold::replay_deferred_entries(tx, ctx.registry)?;
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
                // NEVER apply over unsent local work. A raw local write does not advance the row
                // clock, so the LWW comparison below cannot see it: this op would simply win and
                // record its OWN hash as published, after which the producer sees no delta and the
                // local change is gone with nothing left to author it from.
                //
                // Deferring is safe to do HERE, which it was not before #1005: a deferral now
                // carries its own refold trigger, so the entry is retried once the local work is
                // authored rather than being parked and forgotten. The chain still advances — the
                // entry is stored either way — and convergence stays on the merits, because the
                // local edit is authored at `MAX(lamport) + 1` counting this parked entry and so
                // wins the comparison this op would otherwise have won by default.
                //
                // `DeferExceptUnprovableRemoval`, unlike the refold's blanket caution: a deletion
                // must not be held back on a verdict that may never resolve, or a row deleted after
                // a column change becomes undeletable across the skew. That exemption is for
                // `Remove` only — an unprovable verdict against an `Upsert` still defers, because
                // applying it would destroy the very edit this guard exists to protect.
                if let apply::PreApply::Park(deferral) = apply::pre_apply(
                    tx,
                    spec,
                    ctx.repo_id,
                    stream,
                    &op,
                    apply::RowDoubt::DeferExceptUnprovableRemoval,
                )? {
                    store::mark_entry_pending(
                        tx,
                        &entry_hash,
                        deferral,
                        refold::TABLE_SYNC_PROJECTOR_VERSION,
                    )?;
                    return Ok(IngestOutcome::Retained(deferral.as_db_str()));
                }
                match apply::apply_row_op(tx, spec, ctx.repo_id, &op, meta)? {
                    // A received op that lost on the merits still landed: the entry is
                    // stored, nothing is outstanding, and redelivery stays idempotent.
                    ApplyOutcome::Applied | ApplyOutcome::Superseded => IngestOutcome::Applied,
                    // Durably recorded as well as returned: the caller sees this one, but nothing
                    // later could tell a rejected payload from a projected one without the mark.
                    ApplyOutcome::Quarantined(why) => {
                        store::record_entry_quarantine(tx, &entry_hash, &why)?;
                        IngestOutcome::Quarantined(why)
                    },
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
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
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

        fn delete_row(&self) {
            self.conn.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();
        }

        fn ingest_all(&mut self, entries: &[Vec<u8>], from: &DevicePublic) -> Vec<IngestOutcome> {
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
            let out = entries.iter().map(|bytes| ingest(&tx, &ctx, "demo/1", bytes, from).unwrap());
            let out = out.collect();
            tx.commit().unwrap();
            out
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
    fn a_remote_upsert_does_not_clobber_an_unpublished_local_edit() {
        // The ingest path's half of the unsent-local-work problem. A raw local write does not
        // advance the row clock, so the LWW comparison cannot see it: a remote op simply wins and
        // records its OWN hash as published, after which the producer sees no delta and the local
        // edit is gone with nothing left to author it from.
        //
        // The refold has guarded this since #1002 because it runs at store open with no driver to
        // order it. Ingest has the identical exposure and no guard.
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());
        assert_eq!(b.title().as_deref(), Some("base"), "both devices start converged");

        // B writes locally and does not get to author before A's next entry arrives.
        b.set_title("unsent-B");
        a.set_title("from-A");
        let from_a = a.produce();
        assert_eq!(from_a.len(), 1);
        b.ingest_all(&from_a, &a.pubkey());

        assert_eq!(b.title().as_deref(), Some("unsent-B"), "the unsent local edit survives");
        assert_eq!(
            b.produce().len(),
            1,
            "and is still authorable, so it competes on the merits instead of vanishing",
        );
    }

    #[test]
    fn a_remote_upsert_does_not_resurrect_a_row_deleted_locally_but_not_yet_authored() {
        // The delete half, and the subtler one: the row is GONE, so there is no current state for a
        // comparison to catch — but the surviving published identity is exactly what the producer's
        // `Remove` branch keys on. A remote upsert recreates the row AND re-records its published
        // hash, after which the producer sees no delta and the deletion is undone for good.
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());

        // B deletes locally and does not get to author the removal.
        b.delete_row();
        a.set_title("from-A");
        b.ingest_all(&a.produce(), &a.pubkey());

        assert_eq!(b.title(), None, "the unauthored local deletion survives");
        assert_eq!(b.produce().len(), 1, "and is still authorable, so it reaches peers");
    }

    #[test]
    fn two_devices_with_unsent_edits_converge_through_the_deferral() {
        // Deferring at ingest must not cost convergence — it has to change WHEN an op is applied,
        // never whether the devices agree. Both devices hold an unsent edit, and B receives A's
        // entry before it has authored its own, so B defers.
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'base')", []).unwrap();
        b.ingest_all(&a.produce(), &a.pubkey());

        a.set_title("from-A");
        b.set_title("from-B");

        // A authors first. B, still holding its own unsent edit, defers rather than clobbering it.
        let from_a = a.produce();
        assert_eq!(b.ingest_all(&from_a, &a.pubkey()), vec![IngestOutcome::Retained(
            crate::table_sync::store::PendingReason::DeferredUnsentEdit.as_db_str()
        )],);
        assert_eq!(b.title().as_deref(), Some("from-B"), "B's edit is intact");

        // B authors, which takes `MAX(lamport) + 1` counting the entry it parked — so B's edit is
        // causally later — and settles that entry in the same pass.
        let from_b = b.produce();
        assert_eq!(from_b.len(), 1);
        a.ingest_all(&from_b, &b.pubkey());

        assert_eq!(
            (a.title().as_deref(), b.title().as_deref()),
            (Some("from-B"), Some("from-B")),
            "both devices converge on the causally-later edit, not on a coin flip",
        );
        for device in [&a, &b] {
            assert_eq!(
                device
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM table_sync_entries WHERE pending_reason IS NOT NULL",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                0,
                "and nothing is left outstanding on either side",
            );
        }
        // Steady state: neither device has anything more to say.
        assert!(a.produce().is_empty() && b.produce().is_empty());
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
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
            local_columns: &[],
            repo_column: None,
        };
        const TB: TableSpec = TableSpec {
            name: "t_b",
            scope_id: "multi/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("v", ValueType::Text)],
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
