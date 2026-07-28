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

use std::collections::{HashMap, HashSet};

use rusqlite::Transaction;

use super::apply::{self, ApplyOutcome};
use super::produce;
use super::registry::TableSpec;
use super::row_op::RowOp;
use super::scope_stream::scope_stream_id;
use super::store::{self, AcceptOutcome};
use crate::account::device_is_effective_writer;
use crate::device::DevicePublic;
use crate::op::{DeviceFingerprint, OpMeta};
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
///
/// Re-adopts orphaned rows first ([`readopt_orphaned_rows`]): a row or deletion whose whole-row-LWW
/// winner is a since-removed device is otherwise never re-authored, so a replica enrolled after
/// that device left never converges on it (#997). Folding re-adoption into the producer is
/// deliberate — it makes re-authoring an intrinsic part of "bring peers up to date," reachable
/// wherever the producer is driven, rather than a separate step a future driver must remember.
pub(crate) fn produce_and_author(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut authored = readopt_orphaned_rows(tx, ctx)?;
    for spec in ctx.registry {
        for op in produce::produce_row_ops(tx, spec, ctx.repo_id)? {
            authored.push(author_and_self_apply(tx, ctx, spec, &op)?);
        }
    }
    Ok(authored)
}

/// Author `op` on `spec`'s scope stream under the local device key, self-apply it, and return the
/// signed wire bytes. Self-applying is what enters this authorship into the LWW clock and
/// published-row record, so a later remote op competes at this lamport and the producer never
/// re-emits it (no self-echo).
///
/// A locally-produced op MUST self-apply cleanly: the registry lint rejects every shape that would
/// quarantine (nullable pk, cross-row constraint, ValueType/physical-type mismatch), and the
/// producer and re-adoption builders read well-typed values. If it quarantines anyway, FAIL the
/// pass — `author_row_entry` has already inserted this entry in the caller's txn, so bailing rolls
/// it back rather than leaving it stored-but-unpublished and re-authored (re-signed) every pass
/// (unbounded log growth). Assert first, so a test catches the lint/producer gap.
fn author_and_self_apply(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
    spec: &TableSpec,
    op: &RowOp,
) -> anyhow::Result<Vec<u8>> {
    let stream = scope_stream_id(ctx.repo_id, ctx.account_id, spec.scope_id);
    let signed = store::author_row_entry(tx, stream, ctx.device.secret(), op, ctx.now_ms)?;
    let meta = OpMeta { lamport: signed.entry.lamport, device: signed.entry.device_fingerprint };
    match apply::apply_row_op(tx, spec, ctx.repo_id, op, meta)? {
        ApplyOutcome::Applied => Ok(signed.signed_bytes),
        ApplyOutcome::Quarantined(reason) => {
            debug_assert!(
                false,
                "table-sync: a locally-produced op self-quarantined on `{}`: {reason}",
                spec.name
            );
            anyhow::bail!(
                "table-sync: a locally-produced op self-quarantined on `{}`: {reason}",
                spec.name
            )
        },
    }
}

/// Re-adopt rows and deletions orphaned by a removed writer (#997), returning the signed wire bytes
/// of each re-authored entry.
///
/// The #935 ingest gate accepts an entry only from a *currently* roster-effective writer, so state
/// whose whole-row-LWW winner is a since-removed device is dropped by any newly-enrolled replica
/// and re-authored by nobody — it never converges. Two kinds of orphan exist and both are
/// re-authored under THIS device's key at a fresh `next_stream_lamport`, which is strictly above
/// the orphan's clock so it wins LWW everywhere and establishes the state on a fresh replica:
///
/// - **Live rows** (`sync_row_clocks`): re-author the row's current full after-image as an
///   `Upsert`.
/// - **Deletions** (`sync_row_tombstones`): re-author a `Remove`. A delete authored by a removed
///   writer lives only in the tombstone table (`apply_remove` cleared the row clock), so without
///   this a fresh replica that accepts an older `Upsert` from a current writer would resurrect the
///   row. A tombstone whose row also has a live clock is skipped — a concurrent upsert already
///   resurrected it, and the live-row pass owns that row.
///
/// No wire/format change, and no overwrite of newer legitimate state (an orphan has no newer author
/// by definition). No-op unless the local device is itself a roster-effective writer: a removed
/// device's re-authorship would be dropped by every peer, so it must not emit futile, log-growing
/// entries.
///
/// Called by [`produce_and_author`] at the start of every producer pass (so it runs in the same
/// transaction, before any authoring). Exposed on its own so its detection logic is unit-testable
/// in isolation.
pub(crate) fn readopt_orphaned_rows(
    tx: &Transaction<'_>,
    ctx: &SyncCtx<'_>,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let local_fp = ctx.device.fingerprint();
    if !device_is_effective_writer(tx, ctx.account_id, local_fp)? {
        return Ok(Vec::new());
    }
    let local_hex = local_fp.to_string();
    // One roster probe per distinct winner fingerprint, not per row.
    let mut effective: HashMap<String, bool> = HashMap::new();
    let mut authored = Vec::new();
    for spec in ctx.registry {
        let clocks = apply::row_clock_winners(tx, ctx.repo_id, spec.name)?;
        // Rows with a live write clock — the tombstone pass skips these so a re-authored delete
        // never removes a row a concurrent upsert has since resurrected.
        let live_pks: HashSet<&str> = clocks.iter().map(|(pk, _)| pk.as_str()).collect();

        for (row_pk, winner_hex) in &clocks {
            if *winner_hex == local_hex
                || is_effective(tx, ctx.account_id, winner_hex, &mut effective)?
            {
                continue;
            }
            if let Some(op) = apply::readopt_upsert(tx, spec, row_pk)? {
                authored.push(author_and_self_apply(tx, ctx, spec, &op)?);
            }
        }

        for (row_pk, winner_hex) in apply::tombstone_winners(tx, ctx.repo_id, spec.name)? {
            if live_pks.contains(row_pk.as_str())
                || winner_hex == local_hex
                || is_effective(tx, ctx.account_id, &winner_hex, &mut effective)?
            {
                continue;
            }
            let op = apply::readopt_remove(spec, &row_pk)?;
            authored.push(author_and_self_apply(tx, ctx, spec, &op)?);
        }
    }
    Ok(authored)
}

/// Whether `winner_hex` is a roster-effective writer of `account_id`, memoized across rows within a
/// re-adoption pass. A stored winner is always the canonical lowercase hex the applier wrote; an
/// unparseable value cannot match a roster row, so it is treated as not-effective — re-adopting is
/// safe, since the orphan's current state is re-authored unchanged under the local key.
fn is_effective(
    tx: &Transaction<'_>,
    account_id: AccountId,
    winner_hex: &str,
    cache: &mut HashMap<String, bool>,
) -> anyhow::Result<bool> {
    if let Some(&known) = cache.get(winner_hex) {
        return Ok(known);
    }
    let known = match winner_hex.parse::<DeviceFingerprint>() {
        Ok(fp) => device_is_effective_writer(tx, account_id, fp)?,
        Err(_) => false,
    };
    cache.insert(winner_hex.to_string(), known);
    Ok(known)
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
    let stream = scope_stream_id(ctx.repo_id, ctx.account_id, scope_id);
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
            AcceptOutcome::Stored { op, meta } => {
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
                }
            },
            AcceptOutcome::StoredInert(reason) => IngestOutcome::Retained(reason),
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

    /// Close `fp`'s effective roster row — the device is removed from the account.
    fn remove_writer(
        conn: &rusqlite::Connection,
        account: AccountId,
        fp: crate::op::DeviceFingerprint,
    ) {
        let closed = conn
            .execute(
                "UPDATE account_roster_history SET closed_at = 1
                 WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NULL",
                rusqlite::params![account.to_bytes().as_slice(), fp.to_bytes().as_slice()],
            )
            .unwrap();
        assert_eq!(closed, 1, "the device was on the roster to remove");
    }

    const ACCT: [u8; 32] = [42; 32];

    fn account() -> AccountId {
        AccountId::from_bytes(ACCT)
    }

    fn ctx(device: &LocalDevice) -> SyncCtx<'_> {
        SyncCtx { repo_id: "repo", account_id: account(), device, registry: REGISTRY, now_ms: 0 }
    }

    /// Ingest entries WITHOUT the roster side effect `Device::ingest_all` bakes in — the caller
    /// sets up the roster explicitly so it can model a since-removed author.
    fn ingest_from(
        dev: &mut Device,
        entries: &[Vec<u8>],
        from: &DevicePublic,
    ) -> Vec<IngestOutcome> {
        let tx = dev.conn.transaction().unwrap();
        let ctx = ctx(&dev.local);
        let out = entries
            .iter()
            .map(|b| ingest(&tx, &ctx, "demo/1", b, from).unwrap())
            .collect::<Vec<_>>();
        tx.commit().unwrap();
        out
    }

    /// Run one re-adoption pass and return how many entries it re-authored.
    fn readopt(dev: &mut Device) -> usize {
        let tx = dev.conn.transaction().unwrap();
        let n = readopt_orphaned_rows(&tx, &ctx(&dev.local)).unwrap().len();
        tx.commit().unwrap();
        n
    }

    #[test]
    fn a_removed_writers_row_reaches_a_fresh_replica_via_re_adoption() {
        let mut a = Device::new(); // the original author, later removed from the roster
        let mut c = Device::new(); // a current writer that already holds A's row
        let mut d = Device::new(); // a replica enrolled only AFTER A had left

        // A authors r1.
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'distilled')", []).unwrap();
        let ea = a.produce();
        assert_eq!(ea.len(), 1);

        // C ingested A's row while A was an effective writer; A is then removed. C is itself a
        // current writer.
        enroll_writer(&c.conn, account(), a.pubkey().fingerprint());
        enroll_writer(&c.conn, account(), c.local.fingerprint());
        assert_eq!(ingest_from(&mut c, &ea, &a.pubkey()), vec![IngestOutcome::Applied]);
        assert_eq!(c.title().as_deref(), Some("distilled"), "C holds A's row");
        remove_writer(&c.conn, account(), a.pubkey().fingerprint());

        // D knows C as a writer but never knew A. It has no organic path to r1: A's own entry is
        // refused (A is off D's roster) — the divergence #935 leaves.
        enroll_writer(&d.conn, account(), c.local.fingerprint());
        assert!(
            ingest_from(&mut d, &ea, &a.pubkey()).iter().all(|o| *o == IngestOutcome::Unauthorized),
            "D refuses A's original entry",
        );
        assert_eq!(d.title(), None, "the orphaned row has not reached the fresh replica");

        // C's next producer pass re-adopts the orphaned row (folded into `produce_and_author`) and
        // re-authors it under C's current key.
        let re = c.produce();
        assert_eq!(re.len(), 1, "the orphaned row is re-authored");

        // D accepts C's re-authored entry (C is a current writer) and converges.
        assert_eq!(ingest_from(&mut d, &re, &c.pubkey()), vec![IngestOutcome::Applied]);
        assert_eq!(
            d.title().as_deref(),
            Some("distilled"),
            "the re-adopted row reaches the fresh replica",
        );
        // Idempotent: with the row now authored under C, there is nothing left to re-adopt.
        assert!(c.produce().is_empty(), "a re-adopted row is not re-authored again");
    }

    #[test]
    fn readopt_targets_only_rows_whose_winner_left_the_roster() {
        let mut c = Device::new();
        let mut keep = Device::new(); // stays on the roster
        let mut gone = Device::new(); // removed after authoring

        keep.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r_keep', 'k')", []).unwrap();
        gone.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r_gone', 'g')", []).unwrap();
        let ek = keep.produce();
        let eg = gone.produce();

        enroll_writer(&c.conn, account(), keep.pubkey().fingerprint());
        enroll_writer(&c.conn, account(), gone.pubkey().fingerprint());
        enroll_writer(&c.conn, account(), c.local.fingerprint());
        ingest_from(&mut c, &ek, &keep.pubkey());
        ingest_from(&mut c, &eg, &gone.pubkey());
        remove_writer(&c.conn, account(), gone.pubkey().fingerprint());

        // Only the removed author's row is orphaned — the current writer's row is left alone.
        assert_eq!(readopt(&mut c), 1, "exactly one row (the orphan) is re-adopted, not both");
        // The re-authored entry now owns the orphan and `r_keep` stays published under `keep`, so a
        // following producer pass has nothing to add.
        assert!(c.produce().is_empty(), "no further re-authoring after the orphan is re-adopted");
    }

    #[test]
    fn a_removed_writers_deletion_is_readopted_so_a_fresh_replica_does_not_resurrect_the_row() {
        let mut b = Device::new(); // creator + re-adopter, a current writer
        let mut a = Device::new(); // authors the DELETE, later removed
        let mut d = Device::new(); // a replica that received the create but not the delete

        // B creates r1; A ingests it (B is a writer on A) and then deletes it.
        b.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'v')", []).unwrap();
        let b_up = b.produce();
        enroll_writer(&a.conn, account(), b.pubkey().fingerprint());
        assert_eq!(ingest_from(&mut a, &b_up, &b.pubkey()), vec![IngestOutcome::Applied]);
        a.conn.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();
        let a_rm = a.produce();
        assert_eq!(a_rm.len(), 1, "A authors the deletion");

        // B ingested A's delete while A was a writer; A is then removed. On B, r1 is deleted under
        // a tombstone whose winner is the now-removed A.
        enroll_writer(&b.conn, account(), a.pubkey().fingerprint());
        enroll_writer(&b.conn, account(), b.local.fingerprint());
        assert_eq!(ingest_from(&mut b, &a_rm, &a.pubkey()), vec![IngestOutcome::Applied]);
        assert_eq!(b.title(), None, "r1 is deleted on B");
        remove_writer(&b.conn, account(), a.pubkey().fingerprint());

        // D knows B as a writer. It accepts B's create but refuses A's delete (A is off D's
        // roster), so WITHOUT re-adoption the deleted row resurrects on the fresh replica.
        enroll_writer(&d.conn, account(), b.local.fingerprint());
        assert_eq!(ingest_from(&mut d, &b_up, &b.pubkey()), vec![IngestOutcome::Applied]);
        assert!(
            ingest_from(&mut d, &a_rm, &a.pubkey())
                .iter()
                .all(|o| *o == IngestOutcome::Unauthorized),
            "D refuses A's deletion",
        );
        assert_eq!(
            d.title().as_deref(),
            Some("v"),
            "the row is (wrongly) live on the fresh replica"
        );

        // B's producer re-adopts the orphaned tombstone and re-authors the deletion under its key.
        let re = b.produce();
        assert_eq!(re.len(), 1, "the orphaned deletion is re-authored");

        // D accepts B's re-authored delete; the resurrected row is removed again — convergence.
        assert_eq!(ingest_from(&mut d, &re, &b.pubkey()), vec![IngestOutcome::Applied]);
        assert_eq!(d.title(), None, "the re-adopted deletion removes the resurrected row");
    }

    #[test]
    fn readopt_leaves_a_current_writers_row_untouched() {
        let mut c = Device::new();
        let mut peer = Device::new();
        peer.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'v')", []).unwrap();
        let e = peer.produce();
        enroll_writer(&c.conn, account(), peer.pubkey().fingerprint());
        enroll_writer(&c.conn, account(), c.local.fingerprint());
        ingest_from(&mut c, &e, &peer.pubkey());
        // `peer` stays on the roster, so its row is not orphaned.
        assert_eq!(readopt(&mut c), 0, "a row owned by a current writer is not re-adopted");
        assert!(c.produce().is_empty(), "and nothing is re-authored");
    }

    #[test]
    fn readopt_does_nothing_when_the_local_device_is_not_a_writer() {
        let mut c = Device::new();
        let mut a = Device::new();
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r1', 'v')", []).unwrap();
        let e = a.produce();
        // A is enrolled (so C accepts its entry); C is NOT a writer.
        enroll_writer(&c.conn, account(), a.pubkey().fingerprint());
        ingest_from(&mut c, &e, &a.pubkey());
        remove_writer(&c.conn, account(), a.pubkey().fingerprint()); // r1 is now orphaned...
        // ...but a removed/non-writer local device's re-authorship would be dropped by every peer,
        // so re-adoption is a no-op rather than futile log growth.
        assert_eq!(readopt(&mut c), 0, "a non-writer local device re-adopts nothing");
        assert!(c.produce().is_empty(), "and its producer stays silent for the orphaned row");
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
