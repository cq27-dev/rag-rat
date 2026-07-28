//! The forward-compat refold: replay the entries this store retained but could not project.
//!
//! Storage is gated on the CHAIN, application on the PAYLOAD, so a chain-continuous entry is kept
//! even when this binary cannot project it — an unknown column, an unknown op-kind, an undecodable
//! payload, a table outside this scope. Each such entry is marked pending ([`super::store`]), and
//! this module is what redeems the mark: when a later binary understands more, it replays exactly
//! the outstanding set. Without the replay the payload is unrecoverable, because redelivery
//! short-circuits on `entry_exists` and never reconsiders the op.
//!
//! Two properties keep the replay boring, and both are deliberate:
//!
//! - **It goes through the unmodified [`super::apply::apply_row_op`] gates**, with each entry's
//!   ORIGINAL `OpMeta`. No clock bypass, no reordering. That is what makes every interaction
//!   correct for free: a parked entry superseded by a later winner loses the LWW comparison; one
//!   superseded by a delete loses to the tombstone and cannot resurrect the row; a parked `Remove`
//!   needs no winner lookup. A bypass would have to re-derive all of that, and a bypass that skips
//!   the clock comparison is one refactor away from skipping the tombstone comparison too.
//! - **It is bounded by the pending set**, not the log: the steady state (nothing pending) costs
//!   one indexed probe, so the cost is proportional to what is actually outstanding.
//!
//! Version discipline mirrors the `/3` content projector: [`refold_stale_table_sync_projections`]
//! is the ONLY writer of the stamp, and a store stamped by a NEWER projector is refused rather than
//! folded down by an older binary.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::apply::{self, ApplyOutcome};
use super::registry::TableSpec;
use super::row_op::{self, DecodedRowOp};
use super::store::{self, PendingEntry, PendingReason};
use crate::entry;
use crate::op::OpMeta;

/// What this binary can project into synced tables.
///
/// BUMP THIS on any change that widens understanding — a table registered, a column added to a
/// spec, a new row-op kind — because that is exactly when retained entries become projectable and
/// when previously recorded anti-echo hashes stop covering the current column set. Forgetting the
/// bump leaves pending entries unreplayed (data that arrived is never applied); it cannot corrupt,
/// because the per-row `projector_version` still marks stale hashes as not comparable.
pub(crate) const TABLE_SYNC_PROJECTOR_VERSION: i64 = 1;

const TABLE_SYNC_PROJECTOR_VERSION_KEY: &str = "table_sync_projector_version";

/// Replay every pending entry against this binary's registry, then stamp the projector version.
/// Returns whether a refold ran.
///
/// Call at store open, BEFORE producing: an upgraded binary that produces first will simply emit
/// nothing for rows whose hashes are no longer comparable (safe, by design), but the entries
/// waiting on the new understanding stay unapplied until this runs.
pub fn refold_stale_table_sync_projections(conn: &Connection) -> anyhow::Result<bool> {
    refold_stale_projections_against(conn, super::registry::SYNCABLE_TABLES)
}

/// [`refold_stale_table_sync_projections`] against an explicit registry — the seam the engine tests
/// drive with a synthetic spec, since `SYNCABLE_TABLES` is empty until the scope milestones land.
pub(crate) fn refold_stale_projections_against(
    conn: &Connection,
    registry: &[TableSpec],
) -> anyhow::Result<bool> {
    // A store mid-migration has no projection state to refold and nothing to stamp against; mirrors
    // the `/3` projector's pre-V070 guard, and runs before the meta read so a bare DB never errors.
    if !projection_state_present(conn)? {
        return Ok(false);
    }
    if !refold_owed(conn)? {
        return Ok(false);
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // `refold_owed` already excludes a newer stamp; re-assert inside the write txn so this stays
    // honest if that predicate ever changes.
    assert_projector_not_newer(&tx)?;
    for pending in store::pending_entries(&tx)? {
        replay_pending_entry(&tx, registry, &pending)?;
    }
    stamp_projector_version(&tx)?;
    tx.commit()?;
    Ok(true)
}

/// Whether a refold is owed. TWO independent triggers, because the store-global stamp alone is not
/// a complete record of what has been evaluated:
///
/// 1. **The stamp is behind** — the ordinary upgrade: this binary understands more than whatever
///    last folded the store.
/// 2. **Some entry was evaluated by an OLDER projector than this one**, even though the stamp is
///    current. A shared store reached by binaries of different versions (linked worktrees are a
///    first-class configuration here) produces exactly this: a newer binary stamps the store, an
///    older one then ingests and parks an entry it cannot project, marking it with ITS version. On
///    the stamp alone the newer binary would short-circuit and never replay an entry it fully
///    understands — and redelivery cannot rescue it, because that short-circuits on `entry_exists`.
///
/// A newer stamp is never a trigger: an older binary must not re-park what a newer one understood.
fn refold_owed(conn: &Connection) -> anyhow::Result<bool> {
    let stamp_behind = match stored_projector_version(conn)? {
        Some(version) => {
            if version > TABLE_SYNC_PROJECTOR_VERSION {
                return Ok(false);
            }
            version < TABLE_SYNC_PROJECTOR_VERSION
        },
        None => true,
    };
    Ok(stamp_behind
        || oldest_pending_projector_version(conn)?
            .is_some_and(|oldest| oldest < TABLE_SYNC_PROJECTOR_VERSION))
}

/// Re-attempt one retained entry under this binary's understanding.
///
/// The stored bytes were signature-verified when they were accepted and have not left this store
/// since, so replay decodes without re-verifying: it is a re-projection of our own log, not a trust
/// decision. (Authority was likewise settled at accept time — the roster gate cannot be re-run
/// here, because a device legitimately removed since then would have its long-accepted entries
/// dropped.)
fn replay_pending_entry(
    tx: &Transaction<'_>,
    registry: &[TableSpec],
    pending: &PendingEntry,
) -> anyhow::Result<()> {
    // The stream id is a ONE-WAY hash of (repo_id, account_id, scope_id), so an entry whose stream
    // predates the directory cannot be placed. Leave it pending rather than guessing a repo.
    // Unreachable by construction — the directory row is written in the same transaction, before
    // the entry is stored, on BOTH the ingest and produce paths — so assert rather than skip
    // silently.
    let Some(context) = store::stream_context(tx, pending.stream_id)? else {
        debug_assert!(false, "a stored entry has no stream context to replay against");
        return Ok(());
    };
    // These bytes were signature-verified when accepted and have not left this store since, so a
    // decode failure here means LOCAL corruption. Skip this entry (it stays pending, and stays
    // stored as evidence) instead of propagating: one unreadable row must not wedge the replay of
    // every other pending entry, which — because the stamp only advances on success — would
    // otherwise repeat on every future open.
    let Ok(signed) = entry::decode_signed(&pending.signed_bytes) else {
        return Ok(());
    };
    let op = match row_op::decode(&signed.entry.op_bytes) {
        Ok(DecodedRowOp::Known(op)) => op,
        Ok(DecodedRowOp::Unknown { .. }) =>
            return repark(tx, pending, PendingReason::UnknownOpKind),
        Err(_) => return repark(tx, pending, PendingReason::UndecodablePayload),
    };
    let Some(spec) =
        registry.iter().find(|s| s.scope_id == context.scope_id && s.name == op.table())
    else {
        return repark(tx, pending, PendingReason::TableNotInScope);
    };
    // NEVER replay over an unsent local edit. A raw local write does not advance the row clock, so
    // the LWW comparison cannot see it and this older entry would simply win — silently destroying
    // a change no peer has ever seen, at store open, before anything has had a chance to author
    // it. Leaving the entry pending costs nothing: once the producer authors that edit (at a
    // lamport above this entry's, since authoring counts parked entries), a later replay lands
    // and loses on the merits.
    if apply::row_has_unsent_local_change(tx, spec, &context.repo_id, op.pk())? {
        return Ok(());
    }
    let meta = OpMeta { lamport: signed.entry.lamport, device: signed.entry.device_fingerprint };
    match apply::apply_row_op(tx, spec, &context.repo_id, &op, meta)? {
        // Folded. `Applied` also covers "deliberately lost" — superseded by a newer winner, or
        // suppressed by a tombstone — which are correct folds, not outstanding work.
        ApplyOutcome::Applied => store::clear_entry_pending(tx, &pending.entry_hash),
        // Terminal, so it stops being outstanding: a type mismatch or a constraint violation is a
        // BROKEN PRODUCER — the values do not fit the declared column types or the table's
        // constraints, and no future binary makes them fit. (A missing column is NOT this case: it
        // is an older producer, which reports `Unprojectable` and stays on the worklist.) Recorded
        // rather than merely cleared: this path has no caller to return an outcome to, so without a
        // durable reason a rejected payload would be indistinguishable from a projected one.
        ApplyOutcome::Quarantined(why) =>
            store::record_entry_quarantine(tx, &pending.entry_hash, &why),
        // Still ahead of us — record which gap, under this version, so the next bump can tell
        // "newly stuck" from "stuck since v1".
        ApplyOutcome::Unprojectable(reason) => repark(tx, pending, reason),
    }
}

fn repark(
    tx: &Transaction<'_>,
    pending: &PendingEntry,
    reason: PendingReason,
) -> anyhow::Result<()> {
    store::mark_entry_pending(tx, &pending.entry_hash, reason, TABLE_SYNC_PROJECTOR_VERSION)
}

/// Whether the V093 projection substrate exists yet (the directory is the load-bearing half — no
/// apply context means no replay is possible at all).
fn projection_state_present(conn: &Connection) -> anyhow::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'table_sync_streams'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Refuse to fold — or write into — a store a NEWER projector already folded: an older binary would
/// re-park entries the newer one understood, stamp the version down, and record anti-echo hashes
/// under its narrower column set, all of which the newer binary would then have to distrust.
///
/// Called both by the refold and by the engine's write entry points, so an older binary sharing a
/// store (linked worktrees on different versions are a first-class configuration here) fails loudly
/// instead of quietly degrading what the newer one already understood.
pub(crate) fn assert_projector_not_newer(conn: &Connection) -> anyhow::Result<()> {
    if let Some(stored) = stored_projector_version(conn)?
        && stored > TABLE_SYNC_PROJECTOR_VERSION
    {
        anyhow::bail!(
            "the table-sync projection was folded by a newer rag-rat (table-sync projector \
             v{stored} > v{TABLE_SYNC_PROJECTOR_VERSION}); upgrade to write this store"
        );
    }
    Ok(())
}

fn stamp_projector_version(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![TABLE_SYNC_PROJECTOR_VERSION_KEY, TABLE_SYNC_PROJECTOR_VERSION.to_string()],
    )?;
    Ok(())
}

/// The oldest projector version that evaluated any still-pending entry, or `None` when nothing is
/// pending — the second refold trigger (see [`refold_owed`]).
fn oldest_pending_projector_version(conn: &Connection) -> anyhow::Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT MIN(pending_projector_version) FROM table_sync_entries
          WHERE pending_reason IS NOT NULL",
        [],
        |row| row.get::<_, Option<i64>>(0),
    )?)
}

fn stored_projector_version(conn: &Connection) -> anyhow::Result<Option<i64>> {
    conn.query_row(
        "SELECT value FROM oplog_meta WHERE key = ?1",
        params![TABLE_SYNC_PROJECTOR_VERSION_KEY],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|value| {
        value.parse::<i64>().context("oplog table_sync_projector_version is not an integer")
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_sync::engine::{self, IngestOutcome, SyncCtx};
    use crate::table_sync::registry::{ColumnSpec, ValueType};
    use crate::table_sync::row_op::{Cell, RowOp, TypedValue};
    use crate::table_sync::scope_stream::scope_stream_id;
    use crate::{AccountId, LocalDevice};

    /// The physical table carries both columns; which of them a binary KNOWS is what the two specs
    /// below differ on. That is exactly the real situation — the schema migration adds the column,
    /// and the registry starts replicating it — so an "older" binary is modelled by a narrower spec
    /// over the same table, not by a different table.
    const OLD: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
        columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }],
        local_columns: &["later_col"],
        repo_column: None,
    };
    const NEW: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        pk: &[ColumnSpec { name: "id", value_type: ValueType::Text }],
        columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }, ColumnSpec {
            name: "later_col",
            value_type: ValueType::Text,
        }],
        local_columns: &[],
        repo_column: None,
    };
    const OLD_REGISTRY: &[TableSpec] = &[OLD];
    const NEW_REGISTRY: &[TableSpec] = &[NEW];

    const ACCOUNT: [u8; 32] = [42; 32];

    fn account() -> AccountId {
        AccountId::from_bytes(ACCOUNT)
    }

    struct Device {
        conn: rusqlite::Connection,
        local: LocalDevice,
    }

    impl Device {
        fn new() -> Self {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            conn.execute_batch(
                "CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT, later_col TEXT) STRICT;",
            )
            .unwrap();
            let local = crate::local_device(&conn, 0).unwrap();
            Self { conn, local }
        }

        fn pubkey(&self) -> crate::device::DevicePublic {
            self.local.secret().public()
        }

        /// Enroll `fp` as a roster-effective writer, so the #935 ingest gate admits its entries.
        fn enroll(&self, fp: crate::op::DeviceFingerprint) {
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO account_roster_history
                         (roster_ref, account_id, device_fingerprint, role, effective_at, \
                     closed_at)
                     VALUES (?1, ?2, ?3, 'owner', 0, NULL)",
                    params![fp.to_bytes().as_slice(), ACCOUNT.as_slice(), fp.to_bytes().as_slice()],
                )
                .unwrap();
        }

        fn produce(&mut self, registry: &[TableSpec], repo_id: &str) -> Vec<Vec<u8>> {
            let tx = self.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id,
                account_id: account(),
                device: &self.local,
                registry,
                now_ms: 0,
            };
            let out = engine::produce_and_author(&tx, &ctx).unwrap();
            tx.commit().unwrap();
            out
        }

        fn ingest(
            &mut self,
            registry: &[TableSpec],
            repo_id: &str,
            entries: &[Vec<u8>],
            from: &crate::device::DevicePublic,
        ) -> Vec<IngestOutcome> {
            let tx = self.conn.transaction().unwrap();
            let ctx = SyncCtx {
                repo_id,
                account_id: account(),
                device: &self.local,
                registry,
                now_ms: 0,
            };
            let out = entries
                .iter()
                .map(|bytes| engine::ingest(&tx, &ctx, "demo/1", bytes, from).unwrap())
                .collect();
            tx.commit().unwrap();
            out
        }

        fn row(&self) -> Option<(String, Option<String>)> {
            self.conn
                .query_row("SELECT title, later_col FROM t_demo WHERE id = 'r1'", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()
                .unwrap()
        }

        fn pending_count(&self) -> i64 {
            self.conn
                .query_row(
                    "SELECT COUNT(*) FROM table_sync_entries WHERE pending_reason IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        }
    }

    /// Author r1 on a NEW-registry device, carrying a column an OLD-registry peer cannot project.
    fn author_wide_row(a: &mut Device) -> Vec<Vec<u8>> {
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v1', 'wide')", [])
            .unwrap();
        let entries = a.produce(NEW_REGISTRY, "repo");
        assert_eq!(entries.len(), 1);
        entries
    }

    #[test]
    fn a_parked_column_is_recovered_by_the_refold_and_then_the_producer_stays_quiet() {
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);

        // B (old registry) cannot project the op: it is retained and marked, NOT partially applied.
        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::UnknownColumn.as_db_str())
        ],);
        assert_eq!(b.row(), None, "nothing is written for an op we cannot fully project");
        assert_eq!(b.pending_count(), 1, "the entry is marked for replay");

        // B upgrades: the refold replays exactly that entry, and the row lands COMPLETE — including
        // the column the old binary could not read. Redelivery could never have restored it (it
        // short-circuits on `entry_exists`), which is why the mark exists.
        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.row(), Some(("v1".to_string(), Some("wide".to_string()))));
        assert_eq!(b.pending_count(), 0, "the entry is no longer outstanding");

        // And the recovered row is published under the new column set, so B's producer has nothing
        // to say about it — no re-author, no echo.
        assert!(b.produce(NEW_REGISTRY, "repo").is_empty());
        // The refold is one-shot: the stamp is current, so a second open does no work.
        assert!(!refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
    }

    #[test]
    fn a_column_set_change_alone_re_authors_nothing() {
        // The storm case. Every row published by an older binary carries a hash over the OLD cell
        // list, so under a grown column set every hash mismatches STRUCTURALLY — whether or not the
        // row changed. If the producer read that as a local delta it would re-author the entire
        // table at fresh winning lamports, on every upgrading device at once.
        let mut a = Device::new();
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v1', NULL)", [])
            .unwrap();
        assert_eq!(a.produce(OLD_REGISTRY, "repo").len(), 1, "published under the old column set");

        // Stamp the published row as recorded by an older projector (what an upgrade looks like).
        a.conn
            .execute("UPDATE sync_published_rows SET projector_version = ?1", params![
                TABLE_SYNC_PROJECTOR_VERSION - 1
            ])
            .unwrap();

        assert!(
            a.produce(NEW_REGISTRY, "repo").is_empty(),
            "a wider column set alone must not re-author a single row"
        );
    }

    #[test]
    fn a_local_delete_still_propagates_after_a_column_set_change() {
        // The mirror of the storm guard: `Remove` carries only the pk, so it is deliberately NOT
        // version-gated. Were it gated, a row deleted after a column change could never be
        // authored as deleted, and every peer would keep it forever.
        let mut a = Device::new();
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v1', NULL)", [])
            .unwrap();
        a.produce(OLD_REGISTRY, "repo");
        a.conn
            .execute("UPDATE sync_published_rows SET projector_version = ?1", params![
                TABLE_SYNC_PROJECTOR_VERSION - 1
            ])
            .unwrap();

        a.conn.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();
        assert_eq!(a.produce(NEW_REGISTRY, "repo").len(), 1, "the delete is authored");
    }

    #[test]
    fn a_local_edit_beats_a_parked_entry_and_survives_the_refold() {
        // Whole-row LWW, working as specified: authoring takes the stream-global MAX(lamport)+1,
        // which counts the PARKED entry, so a causally later local edit outranks it — and still
        // does when the refold finally replays that entry.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());

        // B edits the same row locally while the entry sits parked, and AUTHORS it — so the row is
        // published, not unsent (the unsent case is its own test below).
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'mine', NULL)", [])
            .unwrap();
        assert_eq!(b.produce(OLD_REGISTRY, "repo").len(), 1, "the local edit is authored");
        // Published by the OLD binary, so its hash covers the old column set (the single projector
        // const cannot express two binaries; stamping models it, as the storm test does).
        b.conn
            .execute("UPDATE sync_published_rows SET projector_version = ?1", params![
                TABLE_SYNC_PROJECTOR_VERSION - 1
            ])
            .unwrap();

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.row().unwrap().0,
            "mine",
            "the replayed entry loses to the causally later local edit"
        );
        assert_eq!(b.pending_count(), 0, "and it stops being outstanding either way");
    }

    #[test]
    fn a_parked_entry_cannot_resurrect_a_row_deleted_after_it() {
        // Replay goes through the unmodified gates, so the tombstone raised by the later delete
        // suppresses the older parked upsert exactly as it would on the live path. A
        // clock-bypassing replay is what would get this wrong.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());

        // B creates and then deletes the row locally, both authored above the parked entry.
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'local', NULL)", [])
            .unwrap();
        b.produce(OLD_REGISTRY, "repo");
        b.conn.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();
        b.produce(OLD_REGISTRY, "repo");
        assert_eq!(b.row(), None);

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.row(), None, "the replayed entry must not resurrect a row deleted after it");
    }

    #[test]
    fn the_refold_covers_every_repo_in_the_store_not_just_one() {
        // The store is shared across linked worktrees, and the projector stamp is store-global — so
        // one refold must drain the pending entries of EVERY repo, not the caller's alone.
        // Otherwise a bump triggered from one checkout would stamp the version current while
        // leaving a sibling checkout's entries unreplayed forever.
        // Repo-SCOPED specs, because a registered table must be (an unscoped table replicated
        // through per-repo streams has no per-repo bookkeeping and cannot fold under LWW at all —
        // the deferred account-global gap). Each repo therefore owns a distinct physical row.
        const SCOPED_OLD: TableSpec = TableSpec {
            name: "t_scoped",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "repo_id", value_type: ValueType::Text }, ColumnSpec {
                name: "id",
                value_type: ValueType::Text,
            }],
            columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }],
            local_columns: &["later_col"],
            repo_column: Some("repo_id"),
        };
        const SCOPED_NEW: TableSpec = TableSpec {
            name: "t_scoped",
            scope_id: "demo/1",
            pk: &[ColumnSpec { name: "repo_id", value_type: ValueType::Text }, ColumnSpec {
                name: "id",
                value_type: ValueType::Text,
            }],
            columns: &[ColumnSpec { name: "title", value_type: ValueType::Text }, ColumnSpec {
                name: "later_col",
                value_type: ValueType::Text,
            }],
            local_columns: &[],
            repo_column: Some("repo_id"),
        };
        let scoped_table = "CREATE TABLE t_scoped(
                 repo_id TEXT NOT NULL, id TEXT NOT NULL, title TEXT, later_col TEXT,
                 PRIMARY KEY(repo_id, id)
             ) STRICT;";
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute_batch(scoped_table).unwrap();
        b.conn.execute_batch(scoped_table).unwrap();
        b.enroll(a.pubkey().fingerprint());

        for repo in ["repo-one", "repo-two"] {
            a.conn
                .execute(
                    "INSERT INTO t_scoped(repo_id, id, title, later_col)
                     VALUES (?1, 'r1', ?1, 'wide')",
                    [repo],
                )
                .unwrap();
            let entries = a.produce(&[SCOPED_NEW], repo);
            assert_eq!(entries.len(), 1, "one row authored for {repo}");
            b.ingest(&[SCOPED_OLD], repo, &entries, &a.pubkey());
        }
        assert_eq!(b.pending_count(), 2, "one parked entry per repo");

        // One refold, driven from a single checkout's connection.
        assert!(refold_stale_projections_against(&b.conn, &[SCOPED_NEW]).unwrap());
        assert_eq!(b.pending_count(), 0, "both repos' entries were replayed");
        let clocks: i64 = b
            .conn
            .query_row("SELECT COUNT(DISTINCT repo_id) FROM sync_row_clocks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(clocks, 2, "each repo's row was projected under its own scope");
    }

    #[test]
    fn a_store_folded_by_a_newer_projector_is_refused() {
        let b = Device::new();
        b.conn
            .execute("INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)", params![
                TABLE_SYNC_PROJECTOR_VERSION_KEY,
                (TABLE_SYNC_PROJECTOR_VERSION + 1).to_string()
            ])
            .unwrap();
        // A newer binary already folded this store: an older one must not re-park what the newer
        // one understood, nor stamp the version back down.
        assert!(!refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
    }

    #[test]
    fn an_entry_parked_by_an_older_binary_is_replayed_even_under_a_current_stamp() {
        // The mixed-version shared store: a NEWER binary folds and stamps the store, then an OLDER
        // one ingests an op it cannot project and parks it under ITS version. Keying the refold on
        // the store-global stamp alone would short-circuit here and strand an entry the newer
        // binary fully understands — permanently, since redelivery short-circuits on
        // `entry_exists`.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());
        assert_eq!(b.pending_count(), 1);

        // The store looks fully folded (stamp current) while the entry was evaluated by an older
        // projector — exactly the state an older sibling binary leaves behind.
        b.conn
            .execute(
                "UPDATE table_sync_entries SET pending_projector_version = ?1
                  WHERE pending_reason IS NOT NULL",
                params![TABLE_SYNC_PROJECTOR_VERSION - 1],
            )
            .unwrap();
        b.conn
            .execute("INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)", params![
                TABLE_SYNC_PROJECTOR_VERSION_KEY,
                TABLE_SYNC_PROJECTOR_VERSION.to_string()
            ])
            .unwrap();

        assert!(
            refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "an entry evaluated by an older projector is still owed a replay"
        );
        assert_eq!(b.row(), Some(("v1".to_string(), Some("wide".to_string()))));
        assert_eq!(b.pending_count(), 0);
    }

    #[test]
    fn an_older_binary_refuses_to_write_into_a_store_a_newer_projector_folded() {
        // Loud failure rather than quiet degradation: an older binary would record anti-echo hashes
        // over its narrower column set and re-park entries the newer projector already folded.
        let mut b = Device::new();
        b.conn
            .execute("INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)", params![
                TABLE_SYNC_PROJECTOR_VERSION_KEY,
                (TABLE_SYNC_PROJECTOR_VERSION + 1).to_string()
            ])
            .unwrap();
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v', NULL)", [])
            .unwrap();

        let tx = b.conn.transaction().unwrap();
        let ctx = SyncCtx {
            repo_id: "repo",
            account_id: account(),
            device: &b.local,
            registry: OLD_REGISTRY,
            now_ms: 0,
        };
        let produced = engine::produce_and_author(&tx, &ctx);
        assert!(produced.is_err(), "an older projector must not author into a newer store");
        assert!(
            produced.unwrap_err().to_string().contains("newer rag-rat"),
            "the refusal names the cause"
        );
    }

    #[test]
    fn redelivery_of_a_parked_entry_changes_nothing_and_keeps_its_mark() {
        // Redelivery cannot rescue a parked entry (it short-circuits on `entry_exists`) — which is
        // exactly why the mark has to survive it. If redelivery cleared the mark, the refold would
        // have nothing left to replay and the payload would be lost for good.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());

        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey()), vec![
            IngestOutcome::AlreadyPresent
        ],);
        assert_eq!(b.pending_count(), 1, "redelivery leaves the entry outstanding");

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.row(), Some(("v1".to_string(), Some("wide".to_string()))));
    }

    #[test]
    fn a_parked_entry_loses_to_a_later_winner_from_a_third_device() {
        // Three devices: the parked entry must not resurface over a row a third device legitimately
        // won in the meantime. Replay goes through the ordinary LWW comparison, so it simply loses.
        let mut a = Device::new();
        let mut c = Device::new();
        let mut b = Device::new();
        let wide = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.enroll(c.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &wide, &a.pubkey());

        // C authors the same row later (its stream clock counts the parked entry), and B applies
        // it.
        c.enroll(a.pubkey().fingerprint());
        c.ingest(NEW_REGISTRY, "repo", &wide, &a.pubkey());
        c.conn.execute("UPDATE t_demo SET title = 'from-c' WHERE id = 'r1'", []).unwrap();
        let from_c = c.produce(NEW_REGISTRY, "repo");
        assert_eq!(from_c.len(), 1);
        b.ingest(OLD_REGISTRY, "repo", &from_c, &c.pubkey());

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.row().unwrap().0,
            "from-c",
            "the replayed entry loses to the later winner it was always behind"
        );
    }

    #[test]
    fn the_refold_leaves_an_unrelated_unsent_local_edit_unpublished() {
        // The refold publishes ONLY what it actually replays. A row it never touches keeps its
        // pending local edit unpublished, so the producer still emits it — the refold must not
        // become a path that silently marks unsent work as sent.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());

        // An unrelated row, authored and published, then edited raw (unsent).
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r2', 'v1', NULL)", [])
            .unwrap();
        b.produce(OLD_REGISTRY, "repo");
        b.conn.execute("UPDATE t_demo SET title = 'edited' WHERE id = 'r2'", []).unwrap();

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        let ops = b.produce(NEW_REGISTRY, "repo");
        assert_eq!(ops.len(), 1, "the unsent local edit is still pending, not silently published");
    }

    #[test]
    fn the_refold_never_overwrites_an_unsent_local_edit() {
        // The refold runs at STORE OPEN, before anything has had a chance to author. A raw local
        // write does not advance the row clock, so an older retained entry would win the ordinary
        // LWW comparison and destroy a change no peer has ever seen — the one thing this pass must
        // never do. (The live ingest path tolerates the same exposure only because the driver's
        // contract is to author local rows first; there is no driver here.)
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());

        // B writes the same row locally and exits before the next producer pass.
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'unsent', NULL)", [])
            .unwrap();

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.row().unwrap().0, "unsent", "the unsent local edit survives the refold");
        assert_eq!(b.pending_count(), 1, "and the entry stays outstanding rather than being lost");

        // Authoring it settles the race on the merits: the local edit takes a lamport above the
        // parked entry (authoring counts parked entries), so it wins wherever both are seen.
        assert_eq!(b.produce(NEW_REGISTRY, "repo").len(), 1, "the local edit is still authorable");
    }

    #[test]
    fn a_quarantine_found_during_replay_is_recorded_rather_than_silently_cleared() {
        // An entry can park under a narrow registry and then be REJECTED under a wider one: here
        // the op types `later_col` as an integer while the wider spec declares it text.
        // That is a broken producer, not a version gap, so it leaves the worklist — but the
        // rejection has to be durable, or a retained-but-rejected payload is
        // indistinguishable from a projected one and nothing downstream could ever report
        // it. This path has no caller to return an outcome to.
        let mut a = Device::new();
        let mut b = Device::new();
        let mistyped = RowOp::Upsert {
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r1".to_string())],
            cells: vec![
                Cell { column: "later_col".to_string(), value: TypedValue::I64(7) },
                Cell { column: "title".to_string(), value: TypedValue::Text("v".to_string()) },
            ],
        };
        let signed = {
            let tx = a.conn.transaction().unwrap();
            let stream = scope_stream_id("repo", account(), "demo/1");
            let signed =
                store::author_row_entry(&tx, stream, a.local.secret(), &mistyped, 0).unwrap();
            tx.commit().unwrap();
            signed.signed_bytes
        };

        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &[signed], &a.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::UnknownColumn.as_db_str())
        ],);

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.pending_count(), 0, "a broken payload leaves the retry worklist");
        let quarantine: Option<String> = b
            .conn
            .query_row("SELECT quarantine_reason FROM table_sync_entries", [], |r| r.get(0))
            .unwrap();
        assert!(
            quarantine.is_some_and(|why| why.contains("later_col")),
            "the rejection is durable and names what did not fit"
        );
        assert_eq!(b.row(), None, "and nothing was written");
    }

    #[test]
    fn pending_reasons_round_trip_through_their_stored_tokens() {
        // Exhaustive over the enum, so a variant added later cannot skip the round trip.
        for reason in <PendingReason as strum::IntoEnumIterator>::iter() {
            assert_eq!(PendingReason::from_db_str(reason.as_db_str()), Some(reason));
        }
        assert_eq!(
            PendingReason::from_db_str("not_a_reason"),
            None,
            "unknown tokens do not coerce"
        );
    }

    #[test]
    fn pending_reason_tokens_are_pinned() {
        // These strings are STORED, so they are schema: changing one (or the `serialize_all` rule
        // that derives them) silently reclassifies every row a prior binary wrote. Pin them
        // literally — the derive keeps write and parse in step, this keeps the values themselves
        // from moving.
        assert_eq!(PendingReason::UnknownColumn.as_db_str(), "unknown_column");
        assert_eq!(PendingReason::PartialAfterImage.as_db_str(), "partial_after_image");
        assert_eq!(PendingReason::UnknownOpKind.as_db_str(), "unknown_op_kind");
        assert_eq!(PendingReason::UndecodablePayload.as_db_str(), "undecodable_payload");
        assert_eq!(PendingReason::TableNotInScope.as_db_str(), "table_not_in_scope");
    }
}
