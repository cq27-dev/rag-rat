//! The forward-compat refold: replay the entries this store retained but could not project.
//!
//! Storage is gated on the CHAIN, application on the PAYLOAD, so a chain-continuous entry is kept
//! even when this binary cannot project it — an unknown column, an unknown op-kind, an undecodable
//! payload, a table outside this scope. Each such entry is marked pending ([`super::store`]), and
//! this module is what redeems the mark: when a later binary understands more, it replays exactly
//! the outstanding set. Without the replay the payload is unrecoverable, because redelivery
//! short-circuits on `entry_exists` and never reconsiders the op.
//!
//! An entry can also be outstanding for a reason that has nothing to do with understanding: the row
//! it would land on holds LOCAL WORK no peer has seen, or its repository-incarnation authority is
//! unresolved. The two families are marked apart
//! ([`PendingReason::is_deferral`]) because they are redeemed by different events, and therefore
//! need different retry triggers: a version gap clears when the binary widens, which the projector
//! version records; a deferral clears when mutable row or account-authority state changes, which no
//! table-projector version can express. See [`refold_owed`].
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
//!   one indexed probe, so the cost is proportional to what is actually outstanding. A pass owed
//!   ONLY by a deferral narrows further, to the deferral family alone — that trigger fires at every
//!   open while mutable state blocks progress, so it must not drag the rest along.
//!
//! Version discipline mirrors the `/3` content projector: [`refold_stale_table_sync_projections`]
//! is the ONLY writer of the stamps, and a store stamped by a NEWER projector is refused rather
//! than folded down by an older binary. Two versions are stamped, answering different questions —
//! [`TABLE_SYNC_PROJECTOR_VERSION`] for what this binary can PROJECT, and
//! [`TABLE_SYNC_DEFERRAL_VOCABULARY`] for how it CLASSIFIES what it could not.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::apply::{self, ApplyOutcome};
use super::registry::TableSpec;
use super::row_op::{self, DecodedRowOp};
use super::store::{self, PendingEntry, PendingReason, Worklist};
use crate::entry;
use crate::op::OpMeta;

/// What this binary can project into synced tables.
///
/// BUMP THIS on any change that widens understanding — a table registered, a column added to a
/// spec, a new row-op kind — because that is exactly when retained entries become projectable and
/// when previously recorded anti-echo hashes stop covering the current column set. Forgetting the
/// bump leaves pending entries unreplayed (data that arrived is never applied); it cannot corrupt,
/// because the per-row `spec_version` still marks stale hashes as not comparable.
///
/// This is NOT left to discipline for the registry-driven cases. It equals the length of
/// [`super::registry::PROJECTOR_GENERATIONS`], whose last entry must match the live registry — so
/// registering a table or widening a spec forces an append, and an append is the bump. A widening
/// that is not a registry change (a new row-op kind) still has to append a generation by hand,
/// repeating the previous snapshot.
pub(crate) const TABLE_SYNC_PROJECTOR_VERSION: i64 = 2;

const TABLE_SYNC_PROJECTOR_VERSION_KEY: &str = "table_sync_projector_version";

/// How this binary CLASSIFIES a retained entry — the [`PendingReason`] vocabulary and, through
/// [`PendingReason::is_deferral`], which retry trigger each reason answers to.
///
/// BUMP THIS whenever a reason changes families, or a new reason lands in a family an existing
/// state used to be recorded under. Deliberately separate from
/// [`TABLE_SYNC_PROJECTOR_VERSION`], which answers a different question — what this binary can
/// PROJECT — and is mechanically pinned to the registry, so it cannot be moved for a
/// classification change and would say the wrong thing if it were.
///
/// It exists because the classification is recorded DURABLY, and a mark left by a binary with an
/// older vocabulary is indistinguishable from one this binary would write. #1005 is the worked
/// example: before it, an entry blocked behind local row state kept whatever reason it was parked
/// with at ingest, so after the upgrade it reads as a version gap at the current version — no
/// trigger owns it, and it is stranded exactly the way #1005 exists to prevent. One full pass
/// re-derives every pending entry's reason under the current vocabulary, which is all it takes.
///
/// One store-global stamp is enough, and it is worth saying why, because "an older binary shares
/// this store and could re-create the legacy state after we stamped" is the obvious objection.
/// There are exactly TWO writers of a pending mark, and neither can:
///
/// - **Ingest** ([`super::engine`]) reaches a version gap only through
///   [`super::apply::PayloadVerdict::Gap`], decided on the payload alone — so the reason it records
///   there means the same thing under every vocabulary, whatever the row holds. (Since #1056 it
///   records deferrals too, but only reasons from THIS vocabulary: a binary predating a vocabulary
///   has no way to name its states, so it cannot write them.)
/// - **[`repark`]** only runs inside a pass, and a pass is gated on [`refold_owed`]. An older
///   binary's triggers are all version-based, so once the projector stamp is current and every
///   pending entry sits at that version, its refold does not run at all — which is precisely the
///   state this stamp is written in.
///
/// So the legacy shape can only be created by a refold that predates this vocabulary, and that
/// refold cannot run again once the vocabulary is stamped. Binding the vocabulary per mark (a
/// migration) would buy nothing over dominating the one path that writes stale classifications.
const TABLE_SYNC_DEFERRAL_VOCABULARY: i64 = 2;

const TABLE_SYNC_DEFERRAL_VOCABULARY_KEY: &str = "table_sync_deferral_vocabulary";

/// Replay every pending entry against this binary's registry, then stamp the projector version.
/// Returns whether a refold ran.
///
/// Call at store open, BEFORE producing: an upgraded binary that produces first will simply emit
/// nothing for rows whose hashes are no longer comparable (safe, by design), but the entries
/// waiting on the new understanding stay unapplied until this runs.
pub fn refold_stale_table_sync_projections(conn: &Connection) -> anyhow::Result<bool> {
    refold_stale_projections_against(conn, super::registry::SYNCABLE_TABLES)
}

/// [`refold_stale_table_sync_projections`] against an explicit registry — the seam engine tests use
/// to exercise synthetic specs without changing the production registry.
pub(crate) fn refold_stale_projections_against(
    conn: &Connection,
    registry: &[TableSpec],
) -> anyhow::Result<bool> {
    // A store mid-migration has no projection state to refold and nothing to stamp against; mirrors
    // the `/3` projector's pre-V070 guard, and runs before the meta read so a bare DB never errors.
    if !projection_state_present(conn)? {
        return Ok(false);
    }
    let owed = refold_owed(conn)?;
    if !owed.any() {
        return Ok(false);
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // `refold_owed` already excludes a newer stamp; re-assert inside the write txn so this stays
    // honest if that predicate ever changes.
    assert_projector_not_newer(&tx)?;
    replay_worklist(&tx, registry, owed.worklist())?;
    stamp_fold_versions(&tx)?;
    tx.commit()?;
    Ok(true)
}

/// Replay entries deferred behind LOCAL ROW STATE, inside the CALLER's transaction.
///
/// The producer's counterpart to the store-open pass, and the reason a deferral is a usable state
/// rather than one that only clears on restart: the moment local work is authored, the thing those
/// entries were blocked behind is gone, so this is exactly when they are worth another look.
///
/// Running it here is also what makes "author local before applying remote" hold BY CONSTRUCTION at
/// this seam, instead of being a convention every future driver has to remember. It is deliberately
/// narrowed to the deferral family — authoring says nothing about a version gap — and deliberately
/// does not stamp: it is not a full fold, and claiming one would let a genuine gap go unreplayed.
pub(crate) fn replay_deferred_entries(
    tx: &Transaction<'_>,
    registry: &[TableSpec],
) -> anyhow::Result<()> {
    replay_worklist(tx, registry, Worklist::Deferrals)
}

fn replay_worklist(
    tx: &Transaction<'_>,
    registry: &[TableSpec],
    worklist: Worklist,
) -> anyhow::Result<()> {
    for pending in store::pending_entries(tx, worklist)? {
        replay_pending_entry(tx, registry, &pending)?;
    }
    Ok(())
}

/// What a refold pass is owed, and therefore how much of the pending set it has to replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RefoldOwed {
    /// Triggers 1 or 2 — this binary may understand more than whatever last evaluated the pending
    /// set, so every entry's outcome is back in question.
    widened: bool,
    /// Trigger 3 — at least one entry is blocked behind mutable row or account-authority state.
    deferrals: bool,
}

impl RefoldOwed {
    const NOTHING: Self = Self { widened: false, deferrals: false };

    fn any(self) -> bool {
        self.widened || self.deferrals
    }

    /// Narrow the pass to the deferral family when a deferral is the ONLY thing owing it.
    ///
    /// Safe because a version gap's outcome is decided on the PAYLOAD ALONE — the version gate and
    /// the projection, neither of which reads row state — so under an unchanged binary replaying
    /// one is guaranteed to re-park it at the same reason. And the stamp stays honest: "trigger 3
    /// alone" means the stamp and every pending entry are already AT this projector version, so
    /// re-stamping claims nothing that was not already claimed.
    fn worklist(self) -> Worklist {
        if self.widened { Worklist::All } else { Worklist::Deferrals }
    }
}

/// Whether a refold is owed. THREE independent triggers, because neither the store-global stamp nor
/// the per-entry one is a complete record of what could have changed:
///
/// 1. **The stamp is behind** — the ordinary upgrade: this binary understands more than whatever
///    last folded the store.
/// 2. **Some entry was evaluated by an OLDER projector than this one**, even though the stamp is
///    current. A shared store reached by binaries of different versions (linked worktrees are a
///    first-class configuration here) produces exactly this: a newer binary stamps the store, an
///    older one then ingests and parks an entry it cannot project, marking it with ITS version. On
///    the stamp alone the newer binary would short-circuit and never replay an entry it fully
///    understands — and redelivery cannot rescue it, because that short-circuits on `entry_exists`.
/// 3. **Some entry is deferred behind MUTABLE STATE** ([`PendingReason::is_deferral`]), at any
///    version. Versions cannot carry this trigger: such an entry is redeemed by a row changing or
///    repository-incarnation authority resolving, which no table-projector stamp records and no
///    binary upgrade implies. Parked by an OLDER binary it is already covered by trigger 2; parked
///    by THIS one it carries the current version, and without this trigger nothing would ever look
///    at it again (#1005). Its cost is bounded by narrowing the pass — see
///    [`RefoldOwed::worklist`].
///
/// Trigger 1 covers the CLASSIFICATION vocabulary as well as the projector version, because a mark
/// written under an older vocabulary is not re-derivable from the mark itself
/// ([`TABLE_SYNC_DEFERRAL_VOCABULARY`]). It owes a FULL pass, not a narrowed one: the entries
/// needing reclassification are exactly the ones the deferral probe cannot yet recognize.
///
/// A newer stamp is never a trigger, for any of the three: an older binary must not re-park what a
/// newer one understood, and must not act on deferral tokens it may not even have names for.
fn refold_owed(conn: &Connection) -> anyhow::Result<RefoldOwed> {
    let stamp_behind = match stored_projector_version(conn)? {
        Some(version) => {
            if version > TABLE_SYNC_PROJECTOR_VERSION {
                return Ok(RefoldOwed::NOTHING);
            }
            version < TABLE_SYNC_PROJECTOR_VERSION
        },
        None => true,
    };
    let vocabulary_behind = stored_meta_i64(conn, TABLE_SYNC_DEFERRAL_VOCABULARY_KEY)?
        .is_none_or(|stored| stored < TABLE_SYNC_DEFERRAL_VOCABULARY);
    let pending = pending_summary(conn)?;
    Ok(RefoldOwed {
        widened: stamp_behind
            || vocabulary_behind
            || pending.oldest_projector_version.is_some_and(|v| v < TABLE_SYNC_PROJECTOR_VERSION),
        deferrals: pending.any_deferral,
    })
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
    // The stream id is a ONE-WAY hash of (repo_id, account_id, incarnation_ref, scope_id), so an
    // entry with no directory row cannot be placed at all — there is no repo to apply it to, no
    // incarnation to validate, and no scope to resolve its spec. Record that and leave it: a purged
    // repo's history must not project, and the state is now legible rather than unexplained pending
    // work.
    //
    // Re-parking is what makes it CHEAP, not just legible. The mark is what the entry carried when
    // some older binary last touched it, so without this the entry keeps a stale version forever
    // and trigger 2 is permanently true — an IMMEDIATE transaction and a pending scan at every
    // store open, for an entry that can never be placed. `NoStreamContext` is not a deferral, so
    // once it is stamped at the current version nothing retries it.
    let Some(context) = store::stream_context(tx, pending.stream_id)? else {
        return repark(tx, pending, PendingReason::NoStreamContext);
    };
    let account_id = store::stream_account_id(tx, pending.stream_id)?;
    match crate::account::repo_incarnation_state(tx, account_id, &context.repo_id)? {
        crate::account::RepoIncarnationState::Current(current)
            if current == context.incarnation_ref => {},
        // Account evidence is non-monotone: a late secrets cut can condemn the apparent successor
        // and restore this stream's reference. A different current reference is therefore no more
        // terminal than absent/contested authority for already-retained history.
        crate::account::RepoIncarnationState::Current(_)
        | crate::account::RepoIncarnationState::Absent
        | crate::account::RepoIncarnationState::Contested =>
            return repark(tx, pending, PendingReason::DeferredIncarnationAuthority),
    }
    // These bytes were signature-verified when accepted and have not left this store since, so a
    // decode failure here means LOCAL corruption. Record it TERMINALLY rather than propagating or
    // re-parking: propagating would roll the whole refold back and re-fail on every future open,
    // and leaving it pending would keep `refold_owed` true forever — an IMMEDIATE transaction and a
    // pending scan at every store open, for bytes no future binary can decode. The entry stays
    // stored as evidence, and the reason is discoverable.
    let Ok(signed) = entry::decode_signed(&pending.signed_bytes) else {
        return store::record_entry_quarantine(
            tx,
            &pending.entry_hash,
            "stored entry bytes no longer decode",
        );
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
    // NEVER replay over unsent local work. A raw local write does not advance the row clock, so the
    // LWW comparison cannot see it and this older entry would simply win — silently destroying a
    // change no peer has ever seen, at store open, before anything has had a chance to author it.
    // Park it on WHICH state is in the way: once the producer authors that edit (at a lamport above
    // this entry's, since authoring counts parked entries), a later replay lands and loses on the
    // merits — and until then the deferral reason is what brings the entry back for another look,
    // since no version bump can signal that the row moved.
    //
    // `DeferOnAnyDoubt`: nothing ordered a producer before this pass, so a row whose state cannot
    // be established either way is treated as unsafe to write over.
    if let apply::PreApply::Park(reason) = apply::pre_apply(
        tx,
        spec,
        &context.repo_id,
        pending.stream_id,
        &op,
        apply::RowDoubt::DeferOnAnyDoubt,
    )? {
        return repark(tx, pending, reason);
    }
    let meta = OpMeta { lamport: signed.entry.lamport, device: signed.entry.device_fingerprint };
    match apply::apply_row_op_on_stream(tx, spec, &context.repo_id, pending.stream_id, &op, meta)? {
        // Folded. `Superseded` — outranked by a newer winner, or suppressed by a tombstone — is
        // equally a correct fold and equally not outstanding work: the entry was evaluated and
        // lost on the merits, and no later binary changes that.
        ApplyOutcome::Applied | ApplyOutcome::Superseded =>
            store::clear_entry_pending(tx, &pending.entry_hash),
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

/// Re-record why an entry is still outstanding, under THIS projector version.
///
/// Skips the write when nothing would change. A steady deferral is re-evaluated at every store open
/// by design (trigger 3), and re-marking it would rewrite an identical row every time — WAL traffic
/// proportional to opens rather than to anything happening.
fn repark(
    tx: &Transaction<'_>,
    pending: &PendingEntry,
    reason: PendingReason,
) -> anyhow::Result<()> {
    if pending.reason == Some(reason)
        && pending.projector_version == Some(TABLE_SYNC_PROJECTOR_VERSION)
    {
        return Ok(());
    }
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

/// Record what this pass folded: the projection understanding it applied, and the vocabulary it
/// classified the pending set under.
///
/// The vocabulary is raised, never lowered — an older binary sharing the store must not walk it
/// back and make a newer one redo the reclassification. (The projector version is guarded more
/// strongly, by refusing the pass outright; that guard cannot be reused here, because a store
/// folded by a newer vocabulary is still perfectly foldable by this binary.)
fn stamp_fold_versions(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![TABLE_SYNC_PROJECTOR_VERSION_KEY, TABLE_SYNC_PROJECTOR_VERSION.to_string()],
    )?;
    tx.execute(
        "INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE CAST(excluded.value AS INTEGER) > CAST(oplog_meta.value AS INTEGER)",
        params![TABLE_SYNC_DEFERRAL_VOCABULARY_KEY, TABLE_SYNC_DEFERRAL_VOCABULARY.to_string()],
    )?;
    Ok(())
}

/// The two facts [`refold_owed`]'s triggers 2 and 3 need, read in ONE pass over the partial
/// `pending_reason` index — the same cost the single fact used to take, which matters because this
/// runs at every store open and the steady state (nothing pending) must stay one cheap probe.
struct PendingSummary {
    /// The oldest projector version that evaluated any still-pending entry; `None` when nothing is
    /// pending.
    oldest_projector_version: Option<i64>,
    any_deferral: bool,
}

fn pending_summary(conn: &Connection) -> anyhow::Result<PendingSummary> {
    let deferrals = store::deferral_tokens_sql();
    Ok(conn.query_row(
        &format!(
            "SELECT MIN(pending_projector_version), MAX(pending_reason IN ({deferrals}))
               FROM table_sync_entries
              WHERE pending_reason IS NOT NULL"
        ),
        [],
        |row| {
            Ok(PendingSummary {
                oldest_projector_version: row.get(0)?,
                any_deferral: row.get::<_, Option<bool>>(1)?.unwrap_or(false),
            })
        },
    )?)
}

fn stored_projector_version(conn: &Connection) -> anyhow::Result<Option<i64>> {
    stored_meta_i64(conn, TABLE_SYNC_PROJECTOR_VERSION_KEY)
}

fn stored_meta_i64(conn: &Connection, key: &str) -> anyhow::Result<Option<i64>> {
    conn.query_row("SELECT value FROM oplog_meta WHERE key = ?1", params![key], |row| {
        row.get::<_, String>(0)
    })
    .optional()?
    .map(|value| value.parse::<i64>().with_context(|| format!("oplog {key} is not an integer")))
    .transpose()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rag_rat_base::test_scratch::ScratchDir;
    use rag_rat_db::storage::IndexConnection;

    use super::*;
    use crate::table_sync::engine::{self, IngestOutcome, SyncCtx};
    use crate::table_sync::registry::{ColumnSpec, DefaultValue, ValueType};
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
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("title", ValueType::Text)],
        local_columns: &["later_col"],
        repo_column: None,
    };
    const NEW: TableSpec = TableSpec {
        name: "t_demo",
        scope_id: "demo/1",
        // A LATER column set: `later_col` was added, so ops from the older spec fill it from the
        // declared default (the physical column has no DEFAULT clause, hence `Null`).
        spec_version: 2,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[
            ColumnSpec::required("title", ValueType::Text),
            ColumnSpec::added("later_col", ValueType::Text, 2, DefaultValue::Null),
        ],
        local_columns: &[],
        repo_column: None,
    };
    const OLD_REGISTRY: &[TableSpec] = &[OLD];
    const NEW_REGISTRY: &[TableSpec] = &[NEW];

    const ACCOUNT: [u8; 32] = [42; 32];

    fn account() -> AccountId {
        AccountId::from_bytes(ACCOUNT)
    }

    fn seed_incarnation(conn: &rusqlite::Connection) {
        seed_repo_incarnation(conn, "repo");
    }

    fn seed_repo_incarnation(conn: &rusqlite::Connection, repo_id: &str) {
        conn.execute(
            "INSERT INTO account_repo_incarnation_current(
                 account_id, repository_id, incarnation_ref
             ) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, repository_id) DO NOTHING",
            params![ACCOUNT.as_slice(), repo_id, [0x44u8; 32].as_slice()],
        )
        .unwrap();
    }

    struct Device {
        conn: rusqlite::Connection,
        local: LocalDevice,
    }

    impl Device {
        fn new() -> Self {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            seed_incarnation(&conn);
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
                incarnation_ref: [0x44; 32],
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
                incarnation_ref: [0x44; 32],
                device: &self.local,
                registry,
                now_ms: 0,
            };
            let out = entries
                .iter()
                .map(|bytes| engine::ingest(&tx, &ctx, "demo/1", bytes, from).unwrap().outcome)
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

        /// The `(reason token, projector version)` on the first pending entry. Which entry that is
        /// only matters where more than one is outstanding, so callers pin `pending_count` too.
        fn pending_mark(&self) -> Option<(String, i64)> {
            self.conn
                .query_row(
                    "SELECT pending_reason, pending_projector_version FROM table_sync_entries
                      WHERE pending_reason IS NOT NULL
                      ORDER BY lamport",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .unwrap()
        }

        /// Pretend an OLDER binary was the last to evaluate every pending entry — the mixed-version
        /// shared store, which is what makes trigger 2 fire under a current stamp.
        fn age_pending_marks(&self) {
            self.conn
                .execute(
                    "UPDATE table_sync_entries SET pending_projector_version = ?1
                      WHERE pending_reason IS NOT NULL",
                    params![TABLE_SYNC_PROJECTOR_VERSION - 1],
                )
                .unwrap();
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

        // B (old registry) cannot project an op from a NEWER column set: retained and marked, never
        // partially applied.
        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::NewerSpecVersion.as_db_str())
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
    fn absent_or_contested_incarnation_authority_keeps_retryable_refold_debt() {
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::NewerSpecVersion.as_db_str())
        ]);

        b.conn
            .execute(
                "DELETE FROM account_repo_incarnation_current
                  WHERE account_id = ?1 AND repository_id = 'repo'",
                [ACCOUNT.as_slice()],
            )
            .unwrap();
        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.pending_mark().unwrap().0,
            PendingReason::DeferredIncarnationAuthority.as_db_str()
        );
        assert_eq!(b.row(), None);

        b.conn
            .execute(
                "INSERT INTO account_repo_incarnation_current(
                     account_id, repository_id, incarnation_ref
                 ) VALUES (?1, 'repo', NULL)",
                [ACCOUNT.as_slice()],
            )
            .unwrap();
        assert!(
            refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "contested authority remains retryable at the current projector version",
        );
        assert_eq!(b.pending_count(), 1);
        assert_eq!(b.row(), None);

        b.conn
            .execute(
                "UPDATE account_repo_incarnation_current
                    SET incarnation_ref = ?1
                  WHERE account_id = ?2 AND repository_id = 'repo'",
                params![[0x44u8; 32].as_slice(), ACCOUNT.as_slice()],
            )
            .unwrap();
        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.pending_count(), 0, "authority recovery settles the retained debt");
        assert_eq!(b.row(), Some(("v1".into(), Some("wide".into()))));
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

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.row().unwrap().0,
            "mine",
            "the replayed entry loses to the causally later local edit"
        );
        assert_eq!(b.pending_count(), 0, "and it stops being outstanding either way");
    }

    #[test]
    fn the_refold_never_overwrites_an_unsent_edit_on_a_row_published_under_an_older_column_set() {
        // THE CROSS-COLUMN-SET PROTECTIVE ARM. The row IS published (so the never-published guard
        // does not fire) but under a NARROWER column set, so the anti-echo hashes are not directly
        // comparable and the only way to tell an unsent edit from an untouched row is to project
        // the row's winning entry and compare. If that arm answers "no unsent change", the refold
        // replays A's higher-lamport entry straight over work no peer has ever seen.
        //
        // This is not a hypothetical: with the arm returning `false` the assertion below fails with
        // the row holding A's value instead of the local edit. Nothing else in the suite reaches
        // this arm with a non-`Unchanged` verdict — the neighbouring tests hit the never-published
        // arm, the absent-row arm, or an AUTHORED edit (which settles as `Unchanged`).
        let mut a = Device::new();
        let mut b = Device::new();

        // B publishes r1 FIRST, under the old column set, at lamport 0.
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'mine', NULL)", [])
            .unwrap();
        assert_eq!(b.produce(OLD_REGISTRY, "repo").len(), 1, "r1 is published under the old spec");

        // B then edits it RAW — no authoring, so no clock advance and no peer has seen it.
        b.conn.execute("UPDATE t_demo SET title = 'edited' WHERE id = 'r1'", []).unwrap();

        // A authors the same row three times under the wider spec, so its winning lamport (2)
        // strictly beats B's clock (0) — no fingerprint tie deciding this by accident.
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'a1', 'wide')", [])
            .unwrap();
        let mut entries = a.produce(NEW_REGISTRY, "repo");
        for title in ["a2", "a3"] {
            a.conn.execute("UPDATE t_demo SET title = ?1 WHERE id = 'r1'", [title]).unwrap();
            entries.extend(a.produce(NEW_REGISTRY, "repo"));
        }
        assert_eq!(entries.len(), 3, "three authored generations, lamports 0..2");

        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());
        assert_eq!(b.pending_count(), 3, "all three park — B cannot project the wider spec");

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.row().unwrap().0,
            "edited",
            "the unsent local edit must survive the replay of a strictly higher-lamport entry"
        );
        assert_eq!(b.pending_count(), 3, "the entries stay outstanding rather than landing");
    }

    #[test]
    fn authoring_that_cannot_win_its_own_self_apply_fails_instead_of_looping() {
        // A locally-authored op takes the stream's `MAX(lamport) + 1`, so while a row's clock was
        // set by an op on THIS stream it cannot lose. A clock holding a lamport the stream cannot
        // reach therefore means the row's bookkeeping and the stream have come apart — the shape a
        // changed scope or account leaves behind, since `sync_row_clocks` is keyed only by
        // `(repo_id, table_name, row_pk)` and survives a move its lamports have no meaning after.
        //
        // Folded into `Applied`, that reads as settlement: no publication record is written, so the
        // next pass re-derives the identical delta and signs it again — unbounded log growth, and
        // every peer discards the entries too. Fail attributably instead.
        let mut b = Device::new();
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v', NULL)", [])
            .unwrap();
        assert_eq!(b.produce(OLD_REGISTRY, "repo").len(), 1, "published on this stream");

        // A clock from a stream this one cannot catch: nothing local can outrank it.
        b.conn
            .execute("UPDATE sync_row_clocks SET lamport = 9_999 WHERE table_name = 't_demo'", [])
            .unwrap();
        b.conn.execute("UPDATE t_demo SET title = 'edited' WHERE id = 'r1'", []).unwrap();

        let tx = b.conn.transaction().unwrap();
        let ctx = SyncCtx {
            repo_id: "repo",
            account_id: account(),
            incarnation_ref: [0x44; 32],
            device: &b.local,
            registry: OLD_REGISTRY,
            now_ms: 0,
        };
        let err = engine::produce_and_author(&tx, &ctx)
            .expect_err("a self-apply that cannot win must not be reported as settled");
        assert!(
            err.to_string().contains("lost its own self-apply"),
            "the error names the condition: {err}"
        );
    }

    #[test]
    fn a_winner_lookup_that_lands_on_another_rows_entry_resolves_to_unknown() {
        // The lookup keys on `(stream, device, lamport)`, which identifies an entry uniquely WITHIN
        // a stream — so it is this row's op only while the row's clock and the queried stream are
        // the same stream. Moving a table to another scope re-derives the stream while the clocks
        // survive, and the same `(device, lamport)` then belongs to some other op entirely.
        //
        // The dangerous outcome is not a wrong `LocallyChanged` (conservative anyway) but a wrong
        // `Unchanged`, so both rows are given the SAME synced cells: the foreign op then projects
        // to exactly what this row holds, and an unverified lookup reports "untouched" for a row
        // that actually carries an unsent edit — which is a licence to replay over it.
        let mut b = Device::new();
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'x', NULL)", [])
            .unwrap();
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r2', 'shared', NULL)", [])
            .unwrap();
        assert_eq!(b.produce(OLD_REGISTRY, "repo").len(), 2, "both rows are published");

        // r1 now holds an unsent edit that happens to equal r2's published content.
        b.conn.execute("UPDATE t_demo SET title = 'shared' WHERE id = 'r1'", []).unwrap();

        let stream = scope_stream_id("repo", account(), [0x44; 32], "demo/1");
        let (r2_lamport, r2_device): (i64, String) = b
            .conn
            .query_row(
                "SELECT lamport, device_fingerprint FROM sync_row_clocks
                  WHERE table_name = 't_demo' AND row_pk = ?1",
                [&row_op::row_pk_string(&[TypedValue::Text("r2".into())])],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // Repoint r1's clock at r2's entry — the shape a stream change produces.
        b.conn
            .execute(
                "UPDATE sync_row_clocks SET lamport = ?1, device_fingerprint = ?2
                  WHERE table_name = 't_demo' AND row_pk = ?3",
                params![
                    r2_lamport,
                    r2_device,
                    row_op::row_pk_string(&[TypedValue::Text("r1".into())])
                ],
            )
            .unwrap();

        let tx = b.conn.transaction().unwrap();
        let verdict = apply::stale_row_disposition(
            &tx,
            &OLD,
            "repo",
            stream,
            &[TypedValue::Text("r1".into())],
            &[Cell { column: "title".to_string(), value: TypedValue::Text("shared".into()) }],
        )
        .unwrap();
        assert_eq!(
            verdict,
            apply::StaleRow::Unknown,
            "an entry that is not this row's op must not produce a verdict about this row"
        );
    }

    #[test]
    fn ingest_defers_an_upsert_whose_row_state_cannot_be_established() {
        // The live path's exemption is for `Remove` ONLY. An unprovable verdict against an UPSERT
        // still defers: the row may hold an unsent raw edit, and a winning upsert rewrites every
        // synced column and records its own hash as published — destroying the edit with nothing
        // left to author it from, which is exactly #1056. The convergence argument that lets a
        // deletion through does not reach an upsert, because an upsert is version-gated anyway, so
        // deferring one costs nothing the skew was not costing already.
        let mut a = Device::new();
        let mut b = Device::new();

        // B publishes r1 under the OLD column set, edits it raw, then loses its own winning entry —
        // the shape retention/GC leaves behind if it drops a winner without first refreshing the
        // row's publication record.
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'mine', NULL)", [])
            .unwrap();
        assert_eq!(b.produce(OLD_REGISTRY, "repo").len(), 1, "r1 is published under the old spec");
        b.conn.execute("UPDATE t_demo SET title = 'edited' WHERE id = 'r1'", []).unwrap();
        b.conn.execute("DELETE FROM table_sync_entries", []).unwrap();

        // A authors twice, so the entry that matters outranks B's row clock and would win.
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'a1', 'wide')", [])
            .unwrap();
        let mut entries = a.produce(NEW_REGISTRY, "repo");
        a.conn.execute("UPDATE t_demo SET title = 'a2' WHERE id = 'r1'", []).unwrap();
        entries.extend(a.produce(NEW_REGISTRY, "repo"));

        // Ingested under the WIDER registry, so the payload projects cleanly and the only thing in
        // question is the row.
        b.enroll(a.pubkey().fingerprint());
        let outcomes = b.ingest(NEW_REGISTRY, "repo", &entries, &a.pubkey());
        assert!(
            outcomes.iter().all(|outcome| *outcome
                == IngestOutcome::Retained(PendingReason::DeferredUnresolvedWinner.as_db_str())),
            "each upsert is deferred rather than applied: {outcomes:?}",
        );
        assert_eq!(b.row().unwrap().0, "edited", "the possibly-unsent local edit survives");
        // The retained chain-tip witness now deliberately blocks B from authoring until continuity
        // is restored; the row-survival assertion above is the behavior this test owns.
    }

    #[test]
    fn the_refold_defers_when_the_rows_winner_cannot_be_resolved_at_all() {
        // The UNPROVABLE arm of the same guard. When a row's winning entry cannot be resolved, the
        // projection comparison is unavailable and there is no way to tell an untouched row from an
        // unsent edit — so the refold must refuse to replay, exactly as it does for a proven edit.
        // Answering "no unsent change" here silently destroys the edit.
        //
        // The entry is deleted to model what entry retention/GC will do once it lands: it MUST
        // refresh a row's publication record before dropping that row's winner, or every such row
        // enters this state. (The producer's half of that contract is to author on the same verdict
        // rather than also deferring — otherwise the row would be stuck from both sides forever.)
        let mut a = Device::new();
        let mut b = Device::new();

        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'mine', NULL)", [])
            .unwrap();
        assert_eq!(b.produce(OLD_REGISTRY, "repo").len(), 1, "r1 is published under the old spec");
        b.conn.execute("UPDATE t_demo SET title = 'edited' WHERE id = 'r1'", []).unwrap();
        // Drop B's own winning entry, leaving the row's clock pointing at nothing.
        b.conn.execute("DELETE FROM table_sync_entries", []).unwrap();

        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'a1', 'wide')", [])
            .unwrap();
        let mut entries = a.produce(NEW_REGISTRY, "repo");
        for title in ["a2", "a3"] {
            a.conn.execute("UPDATE t_demo SET title = ?1 WHERE id = 'r1'", [title]).unwrap();
            entries.extend(a.produce(NEW_REGISTRY, "repo"));
        }

        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.row().unwrap().0,
            "edited",
            "an unresolvable winner must not license replaying over the row"
        );
        assert_eq!(
            b.pending_mark().unwrap().0,
            PendingReason::DeferredUnresolvedWinner.as_db_str(),
            "and the entry says WHY it is stuck — this reason, unlike the other three, is the one \
             retention/GC has to be able to find"
        );
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
            spec_version: 1,
            pk: &[
                ColumnSpec::required("repo_id", ValueType::Text),
                ColumnSpec::required("id", ValueType::Text),
            ],
            columns: &[ColumnSpec::required("title", ValueType::Text)],
            local_columns: &["later_col"],
            repo_column: Some("repo_id"),
        };
        const SCOPED_NEW: TableSpec = TableSpec {
            name: "t_scoped",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[
                ColumnSpec::required("repo_id", ValueType::Text),
                ColumnSpec::required("id", ValueType::Text),
            ],
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("later_col", ValueType::Text),
            ],
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
            seed_repo_incarnation(&a.conn, repo);
            seed_repo_incarnation(&b.conn, repo);
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
            incarnation_ref: [0x44; 32],
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
    fn an_older_binary_refuses_to_ingest_into_a_store_a_newer_projector_folded() {
        // The producer's refusal has a sibling on the ingest path, and it is the more important of
        // the two: ingesting is what would re-park, under this binary's narrower understanding, an
        // entry the newer projector already folded.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.conn
            .execute("INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)", params![
                TABLE_SYNC_PROJECTOR_VERSION_KEY,
                (TABLE_SYNC_PROJECTOR_VERSION + 1).to_string()
            ])
            .unwrap();

        let tx = b.conn.transaction().unwrap();
        let ctx = SyncCtx {
            repo_id: "repo",
            account_id: account(),
            incarnation_ref: [0x44; 32],
            device: &b.local,
            registry: OLD_REGISTRY,
            now_ms: 0,
        };
        let ingested = engine::ingest(&tx, &ctx, "demo/1", &entries[0], &a.pubkey());
        assert!(ingested.is_err(), "an older projector must not ingest into a newer store");
        assert!(
            ingested.unwrap_err().to_string().contains("newer rag-rat"),
            "the refusal names the cause"
        );
    }

    #[test]
    fn an_older_producers_row_applies_with_its_missing_columns_defaulted() {
        // The unfreeze. A row that is COMPLETE under the author's narrower spec is PARTIAL under a
        // wider one, and before the op stated its version the receiver could only park it forever —
        // nothing on the receiving side could ever redeem it. Now the version says "this predates
        // `later_col`", so the column it predates is filled from its declared default and the row
        // applies immediately.
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'narrow', NULL)", [])
            .unwrap();
        let narrow = a.produce(OLD_REGISTRY, "repo");
        assert_eq!(narrow.len(), 1, "authored under the narrow spec");

        b.enroll(a.pubkey().fingerprint());
        assert_eq!(
            b.ingest(NEW_REGISTRY, "repo", &narrow, &a.pubkey()),
            vec![IngestOutcome::Applied],
            "an older producer's row is no longer frozen out",
        );
        assert_eq!(
            b.row(),
            Some(("narrow".to_string(), None)),
            "the column the op predates takes its declared default"
        );
        assert_eq!(b.pending_count(), 0, "nothing is left outstanding");
        // And the applied row is published under THIS spec, so the producer has nothing to say.
        assert!(b.produce(NEW_REGISTRY, "repo").is_empty());
    }

    #[test]
    fn a_column_without_a_declared_default_still_parks_an_older_op() {
        // Default-fill is opt-in per column: a column with no declared default cannot be invented,
        // so an op omitting it is still a partial after-image. That is what keeps a genuinely
        // broken producer from being silently completed.
        const NO_DEFAULT: TableSpec = TableSpec {
            name: "t_demo",
            scope_id: "demo/1",
            spec_version: 2,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[
                ColumnSpec::required("title", ValueType::Text),
                ColumnSpec::required("later_col", ValueType::Text),
            ],
            local_columns: &[],
            repo_column: None,
        };
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'narrow', NULL)", [])
            .unwrap();
        let narrow = a.produce(OLD_REGISTRY, "repo");
        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(&[NO_DEFAULT], "repo", &narrow, &a.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::PartialAfterImage.as_db_str())
        ],);
        assert_eq!(b.row(), None, "nothing is invented for a column with no declared default");
    }

    #[test]
    fn a_stale_version_row_edited_locally_is_authored_rather_than_frozen() {
        // The dead zone, closed. Before the op stated its version, a row published under an older
        // column set could never be re-authored — its hash was not comparable, so even a genuine
        // local edit produced nothing and the change could never reach a peer. Now the row is
        // settled against the op that established it: different means a real local change, and it
        // is authored.
        let mut a = Device::new();
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v1', NULL)", [])
            .unwrap();
        assert_eq!(a.produce(OLD_REGISTRY, "repo").len(), 1, "published under the old column set");

        // The binary learns `later_col`, and the user edits the row before anything re-publishes
        // it.
        a.conn.execute("UPDATE t_demo SET title = 'edited' WHERE id = 'r1'", []).unwrap();

        let ops = a.produce(NEW_REGISTRY, "repo");
        assert_eq!(ops.len(), 1, "the edit is authored, not frozen out");
        match authored_op(&ops[0]) {
            RowOp::Upsert { spec_version, cells, .. } => {
                assert_eq!(spec_version, NEW.spec_version, "authored under the current spec");
                assert!(
                    cells.iter().any(
                        |c| c.column == "title" && c.value == TypedValue::Text("edited".into())
                    ),
                    "and carries the local edit"
                );
            },
            other => panic!("expected an upsert, got {other:?}"),
        }
    }

    /// Decode the row op inside one authored entry's signed wire bytes.
    fn authored_op(signed_bytes: &[u8]) -> RowOp {
        let signed = crate::entry::decode_signed(signed_bytes).unwrap();
        match crate::table_sync::row_op::decode(&signed.entry.op_bytes).unwrap() {
            crate::table_sync::row_op::DecodedRowOp::Known(op) => op,
            other => panic!("expected a known op, got {other:?}"),
        }
    }

    #[test]
    fn an_untouched_stale_version_row_is_restamped_without_authoring() {
        // The other half: a row that has NOT changed since it landed is simply restamped, so it
        // becomes comparable again without a signed entry. Re-authoring it instead would put every
        // row of the table back on the wire on every upgrading device.
        let mut a = Device::new();
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v1', NULL)", [])
            .unwrap();
        a.produce(OLD_REGISTRY, "repo");

        assert!(
            a.produce(NEW_REGISTRY, "repo").is_empty(),
            "nothing to say about an untouched row"
        );
        let version: i64 = a
            .conn
            .query_row("SELECT spec_version FROM sync_published_rows", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, i64::from(NEW.spec_version), "and it is comparable again");
        // Comparable means an ordinary later edit is authored by the ordinary path.
        a.conn.execute("UPDATE t_demo SET title = 'later' WHERE id = 'r1'", []).unwrap();
        assert_eq!(a.produce(NEW_REGISTRY, "repo").len(), 1);
    }

    #[test]
    fn a_remove_crosses_a_spec_version_skew_unimpeded() {
        // A remove names only the row identity, so no column set is involved and its version is
        // never acted on. Gating it would delay deletions across a skew for no benefit.
        //
        // This is also the case that pins ingest's one guard exemption
        // (`RowDoubt::DeferExceptUnprovableRemoval`): B's row was published under the wider column
        // set, so the unsent-work guard cannot resolve its winner under the narrower spec and
        // returns `DeferredUnresolvedWinner`. Deferring on that would make a row deleted after a
        // column change undeletable until B upgrades.
        let mut a = Device::new();
        let mut b = Device::new();
        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v', 'wide')", [])
            .unwrap();
        let created = a.produce(NEW_REGISTRY, "repo");
        b.enroll(a.pubkey().fingerprint());
        b.ingest(NEW_REGISTRY, "repo", &created, &a.pubkey());
        assert!(b.row().is_some());

        a.conn.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();
        let removed = a.produce(NEW_REGISTRY, "repo");
        assert_eq!(removed.len(), 1);
        // B is on the OLDER spec — the delete still lands.
        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &removed, &a.pubkey()), vec![
            IngestOutcome::Applied
        ],);
        assert_eq!(b.row(), None, "a newer-spec remove still deletes on an older peer");
    }

    #[test]
    fn devices_at_different_spec_versions_converge_in_both_directions() {
        // The property the whole change rests on. An op's projection is a pure function of (op,
        // receiver registry) — never of prior row state — so whole-row LWW lands every receiver on
        // the same result regardless of arrival order or who authored last.
        let mut old_dev = Device::new();
        let mut new_dev = Device::new();
        old_dev.enroll(new_dev.pubkey().fingerprint());
        old_dev.enroll(old_dev.local.fingerprint());
        new_dev.enroll(old_dev.pubkey().fingerprint());
        new_dev.enroll(new_dev.local.fingerprint());

        // NEWER authors first; the older peer cannot project it and parks.
        new_dev
            .conn
            .execute(
                "INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'from-new', 'wide')",
                [],
            )
            .unwrap();
        let from_new = new_dev.produce(NEW_REGISTRY, "repo");
        assert_eq!(old_dev.ingest(OLD_REGISTRY, "repo", &from_new, &new_dev.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::NewerSpecVersion.as_db_str())
        ],);

        // OLDER then authors its own row; the newer peer APPLIES it, filling the column it
        // predates.
        old_dev
            .conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'from-old', NULL)", [])
            .unwrap();
        let from_old = old_dev.produce(OLD_REGISTRY, "repo");
        assert_eq!(
            new_dev.ingest(NEW_REGISTRY, "repo", &from_old, &old_dev.pubkey()),
            vec![IngestOutcome::Applied],
            "older→newer is no longer frozen",
        );

        // The older device's write is causally later (authoring counts the parked entry), so it
        // wins on the newer device — and the newer device's row reflects it with the column
        // defaulted.
        assert_eq!(new_dev.row(), Some(("from-old".to_string(), None)));

        // Once the older device upgrades, the parked entry replays and loses on the merits, so both
        // devices agree on the same winner.
        assert!(refold_stale_projections_against(&old_dev.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            old_dev.row(),
            new_dev.row(),
            "the two devices converge on the same row once both understand the column set"
        );
        assert_eq!(old_dev.pending_count(), 0, "and nothing is left outstanding");
    }

    #[test]
    fn a_row_this_binary_cannot_read_does_not_fail_the_refold_and_is_repaired_by_it() {
        // #1017, and the reason it is severe: this pass runs at STORE OPEN, so an error here does
        // not fail one replay — it fails the open, and `index --full` takes the same path, so there
        // is no recovery. A `Bool` column outside 0/1 is the one cell a STRICT schema cannot rule
        // out, and no registry lint can require the `CHECK (col IN (0, 1))` that would.
        const BOOL_OLD: TableSpec = TableSpec {
            name: "t_bool",
            scope_id: "demo/1",
            spec_version: 1,
            pk: &[ColumnSpec::required("id", ValueType::Text)],
            columns: &[ColumnSpec::required("flag", ValueType::Bool)],
            local_columns: &["later"],
            repo_column: None,
        };
        const BOOL_NEW: TableSpec = TableSpec {
            spec_version: 2,
            columns: &[
                ColumnSpec::required("flag", ValueType::Bool),
                ColumnSpec::added("later", ValueType::Text, 2, DefaultValue::Null),
            ],
            local_columns: &[],
            ..BOOL_OLD
        };
        let table = "CREATE TABLE t_bool(id TEXT PRIMARY KEY, flag INTEGER, later TEXT) STRICT;";

        let mut a = Device::new();
        let mut b = Device::new();
        a.conn.execute_batch(table).unwrap();
        b.conn.execute_batch(table).unwrap();

        // A authors r1 under the wider column set; B cannot project it and parks it.
        a.conn.execute("INSERT INTO t_bool(id, flag, later) VALUES ('r1', 0, 'wide')", []).unwrap();
        let entries = a.produce(&[BOOL_NEW], "repo");
        assert_eq!(entries.len(), 1);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(&[BOOL_OLD], "repo", &entries, &a.pubkey());
        assert_eq!(b.pending_count(), 1);

        // B's own copy of the row is outside the Bool domain — written raw, so it never went
        // through the applier's typed cells.
        b.conn.execute("INSERT INTO t_bool(id, flag, later) VALUES ('r1', 2, NULL)", []).unwrap();

        assert!(
            refold_stale_projections_against(&b.conn, &[BOOL_NEW]).unwrap(),
            "the refold must complete — an error here is an index that never opens again",
        );
        let row: (i64, Option<String>) = b
            .conn
            .query_row("SELECT flag, later FROM t_bool WHERE id = 'r1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            row,
            (0, Some("wide".to_string())),
            "and the replayed winner rewrites the whole row, which is what repairs the bad cell",
        );
        assert_eq!(b.pending_count(), 0, "the entry is no longer outstanding");
    }

    #[test]
    fn a_parked_remove_does_not_delete_a_row_this_binary_cannot_read() {
        // The other half of #1017's guard, and the destructive one. An UPSERT may replay over an
        // unreadable row because a winner rewrites every synced column and repairs it. A REMOVE has
        // no such floor: it deletes the row outright — local-only columns included — so a row whose
        // unreadable cell is an unsent local edit would be destroyed at store open, before anything
        // had a chance to author it.
        let mut a = Device::new();
        let mut b = Device::new();
        park_a_remove_over_an_unreadable_row(&mut a, &mut b);

        assert!(refold_stale_projections_against(&b.conn, &[BOOL]).unwrap());
        let survived: (i64, Option<String>) = b
            .conn
            .query_row("SELECT flag, local_only FROM t_bool WHERE id = 'r1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            survived,
            (2, Some("keep me".to_string())),
            "the unsent row survives the replayed remove, local-only column included",
        );
        assert_eq!(b.pending_count(), 1, "and the entry stays outstanding rather than being lost");
    }

    /// A table whose synced column is a `Bool`, so a stored `2` is a cell no binary can read
    /// (#1017), plus a local-only column a delete would take with it.
    const BOOL: TableSpec = TableSpec {
        name: "t_bool",
        scope_id: "demo/1",
        spec_version: 1,
        pk: &[ColumnSpec::required("id", ValueType::Text)],
        columns: &[ColumnSpec::required("flag", ValueType::Bool)],
        local_columns: &["local_only"],
        repo_column: None,
    };

    /// A publishes r1 and B ingests it; B edits its copy raw into a state it cannot read; A then
    /// deletes the row and authors the `Remove`, which B retains rather than applies.
    fn park_a_remove_over_an_unreadable_row(a: &mut Device, b: &mut Device) {
        let table =
            "CREATE TABLE t_bool(id TEXT PRIMARY KEY, flag INTEGER, local_only TEXT) STRICT;";
        a.conn.execute_batch(table).unwrap();
        b.conn.execute_batch(table).unwrap();

        // A publishes r1; B ingests it normally, so B's chain holds the Remove's predecessor.
        a.conn.execute("INSERT INTO t_bool(id, flag) VALUES ('r1', 1)", []).unwrap();
        let upserts = a.produce(&[BOOL], "repo");
        assert_eq!(upserts.len(), 1, "the row is published first");
        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(&[BOOL], "repo", &upserts, &a.pubkey()), vec![IngestOutcome::Applied]);

        // B then edits its copy raw — unsent, outside the Bool domain, and carrying a local-only
        // column a delete would take with it.
        b.conn
            .execute("UPDATE t_bool SET flag = 2, local_only = 'keep me' WHERE id = 'r1'", [])
            .unwrap();

        // A deletes the row and authors the `Remove`. B ingests it through a binary whose registry
        // does not carry the table — the mixed-version shared store this engine treats as
        // first-class — so it is retained for the refold rather than applied.
        a.conn.execute("DELETE FROM t_bool WHERE id = 'r1'", []).unwrap();
        let removes = a.produce(&[BOOL], "repo");
        assert_eq!(removes.len(), 1, "the local delete is authored as a Remove");
        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &removes, &a.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::TableNotInScope.as_db_str())
        ]);
        assert_eq!(b.pending_count(), 1);
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
    fn the_refold_never_resurrects_a_row_deleted_locally_but_not_yet_authored() {
        // The delete counterpart of the unsent-edit guard, and the subtler half: the row is GONE,
        // so there is no current state to compare — but the surviving published identity is
        // exactly what the producer's `Remove` branch keys on. Replaying an upsert over it
        // would recreate the row AND re-record its published hash, after which the producer
        // sees no delta and the deletion is gone for good.
        let mut a = Device::new();
        let mut b = Device::new();
        park_over_an_unauthored_local_delete(&mut a, &mut b);

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.row(), None, "the unsent deletion survives the refold");
        // And it is still authorable, so the removal reaches peers.
        assert_eq!(b.produce(NEW_REGISTRY, "repo").len(), 1, "the delete is still authorable");
    }

    /// B holds r1 published, A authors r1 twice under the wide column set (so both entries park on
    /// B), and B then deletes the row locally without authoring the removal.
    ///
    /// A authors TWICE so the entry that matters sits at a strictly HIGHER lamport than B's row
    /// clock. Without that the replay could lose the lamport-0 tie on fingerprint, and the row
    /// would stay deleted for reasons having nothing to do with the guard.
    fn park_over_an_unauthored_local_delete(a: &mut Device, b: &mut Device) {
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v1', NULL)", [])
            .unwrap();
        b.produce(OLD_REGISTRY, "repo");

        a.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'first', 'wide')", [])
            .unwrap();
        let first = a.produce(NEW_REGISTRY, "repo");
        a.conn.execute("UPDATE t_demo SET title = 'second' WHERE id = 'r1'", []).unwrap();
        let second = a.produce(NEW_REGISTRY, "repo");
        assert_eq!((first.len(), second.len()), (1, 1));

        // Both park on B (each carries the column B does not know); the chain needs both.
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &first, &a.pubkey());
        b.ingest(OLD_REGISTRY, "repo", &second, &a.pubkey());
        assert_eq!(b.pending_count(), 2);

        b.conn.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();
    }

    #[test]
    fn a_malformed_key_is_quarantined_rather_than_failing_the_whole_refold() {
        // An entry parked as out-of-scope never passed `apply_row_op`'s arity check. When a later
        // registry recognizes its table, the unsent-change probe must not bind that key against a
        // mismatched placeholder count — that is a parameter-count ERROR, and propagating it out of
        // the refold would roll the transaction back and fail EVERY subsequent store open on the
        // same entry. It has to reach the normal quarantine path instead.
        let mut a = Device::new();
        let mut b = Device::new();
        let two_key = RowOp::Upsert {
            spec_version: 1,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r1".to_string()), TypedValue::Text("extra".to_string())],
            cells: vec![Cell { column: "title".to_string(), value: TypedValue::Text("v".into()) }],
        };
        let signed = {
            let tx = a.conn.transaction().unwrap();
            let stream = scope_stream_id("repo", account(), [0x44; 32], "demo/1");
            let signed =
                store::author_row_entry(&tx, stream, a.local.secret(), &two_key, 0).unwrap();
            tx.commit().unwrap();
            signed.signed_bytes
        };

        // Parked because this scope has no such table for the ingesting registry.
        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(&[], "repo", &[signed], &a.pubkey()), vec![IngestOutcome::Retained(
            PendingReason::TableNotInScope.as_db_str()
        )],);

        // The refold must complete — not error — and the malformed entry must be rejected durably.
        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.pending_count(), 0);
        let quarantined: i64 = b
            .conn
            .query_row(
                "SELECT COUNT(*) FROM table_sync_entries WHERE quarantine_reason IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(quarantined, 1, "the malformed key is quarantined, not fatal");
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
        // Stamped at the WIDER spec, consistent with carrying `later_col` — a v1 stamp on an op
        // that names a column introduced at v2 is self-contradictory and parks as a mis-stamp
        // before any type check runs, which would test something else entirely.
        let mistyped = RowOp::Upsert {
            spec_version: 2,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r1".to_string())],
            cells: vec![
                Cell { column: "later_col".to_string(), value: TypedValue::I64(7) },
                Cell { column: "title".to_string(), value: TypedValue::Text("v".to_string()) },
            ],
        };
        let signed = {
            let tx = a.conn.transaction().unwrap();
            let stream = scope_stream_id("repo", account(), [0x44; 32], "demo/1");
            let signed =
                store::author_row_entry(&tx, stream, a.local.secret(), &mistyped, 0).unwrap();
            tx.commit().unwrap();
            signed.signed_bytes
        };

        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &[signed], &a.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::NewerSpecVersion.as_db_str())
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
    fn a_terminal_payload_over_an_unsent_edit_is_quarantined_not_deferred() {
        // The cross-product the ordering rule exists for. This op is broken on its own merits (it
        // types `later_col` as an integer), AND the row it names holds an unsent local edit, so the
        // guard would fire too. Asking the guard first files it as a deferral — and a deferral is
        // retried at EVERY store open, forever, for a payload that can never apply. The payload
        // question has to be settled first, and it is terminal, so the row never gets a say.
        let mut a = Device::new();
        let mut b = Device::new();
        let mistyped = RowOp::Upsert {
            spec_version: 2,
            table: "t_demo".to_string(),
            pk: vec![TypedValue::Text("r1".to_string())],
            cells: vec![
                Cell { column: "later_col".to_string(), value: TypedValue::I64(7) },
                Cell { column: "title".to_string(), value: TypedValue::Text("v".to_string()) },
            ],
        };
        let signed = {
            let tx = a.conn.transaction().unwrap();
            let stream = scope_stream_id("repo", account(), [0x44; 32], "demo/1");
            let signed =
                store::author_row_entry(&tx, stream, a.local.secret(), &mistyped, 0).unwrap();
            tx.commit().unwrap();
            signed.signed_bytes
        };
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &[signed], &a.pubkey());

        // The blocking row state: written raw, never authored, so the guard has something to find.
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'unsent', NULL)", [])
            .unwrap();

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.pending_count(), 0, "the broken payload leaves the worklist for good");
        assert_eq!(
            b.row().unwrap().0,
            "unsent",
            "and quarantining it wrote nothing, so the edit is untouched either way",
        );
        assert!(
            !refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "nothing is owed afterwards — a terminal payload must not join the every-open family",
        );
    }

    // ---------------------------------------------------------------------------------------------
    // Deferrals: entries blocked behind LOCAL ROW STATE rather than behind this binary's
    // understanding (#1005). They are redeemed by the row changing, which nothing stamps, so they
    // need a retry trigger of their own — and a bound on what that trigger costs.

    /// Ingest A's wide entry on B, then write r1 locally raw so the replay is refused, and fold.
    /// Leaves B holding exactly one entry deferred behind an unsent local edit, AT THIS PROJECTOR
    /// VERSION.
    ///
    /// The last part is the whole point of the fixture and easy to lose: an entry left at an OLDER
    /// version rides trigger 2, and a test built that way passes with trigger 3 deleted.
    fn defer_over_an_unsent_edit(a: &mut Device, b: &mut Device) {
        let entries = author_wide_row(a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'unsent', NULL)", [])
            .unwrap();

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.pending_mark(),
            Some((
                PendingReason::DeferredUnsentEdit.as_db_str().to_string(),
                TABLE_SYNC_PROJECTOR_VERSION
            )),
            "the replay is refused to protect the unsent edit, and parked on WHICH state is in \
             the way — at this version, which is what strands it",
        );
        assert_eq!(
            stored_projector_version(&b.conn).unwrap(),
            Some(TABLE_SYNC_PROJECTOR_VERSION),
            "and the store is stamped, so neither of the version triggers can carry it either",
        );
    }

    #[test]
    fn authoring_the_local_edit_settles_the_deferral_in_the_same_pass() {
        // The producer is the deferral's natural redeemer: the moment the local work is published,
        // the thing the entry was blocked behind is gone. Doing it inside that pass is what makes
        // author-before-apply hold by construction, and it means a deferral does not have to wait
        // for a restart to clear (#1056).
        let mut a = Device::new();
        let mut b = Device::new();
        defer_over_an_unsent_edit(&mut a, &mut b);

        assert_eq!(b.produce(NEW_REGISTRY, "repo").len(), 1, "the local edit is authored");
        assert_eq!(b.pending_count(), 0, "and the deferred entry settles in that same pass");
        assert_eq!(
            b.row().unwrap().0,
            "unsent",
            "losing the LWW comparison to the edit it was protecting, rather than by default",
        );
        assert!(
            !refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "leaving nothing for the next store open to do"
        );
    }

    #[test]
    fn a_deferral_marked_at_the_current_version_is_retried_on_the_next_open() {
        // The #1005 bug, on the path the producer does NOT redeem. An entry deferred behind local
        // row state is redeemed by the ROW changing, which no projector version can express and no
        // upgrade implies — so without a trigger keyed on the deferral itself, an entry parked by
        // THIS binary is never looked at again, and redelivery cannot rescue it (it short-circuits
        // on `entry_exists`).
        //
        // Redemption here is the local row being DISCARDED, not authored — the row was purely local
        // (no apply ever published it), so once it is gone there is nothing left to protect.
        // Nothing produces, so the store-open trigger is the only thing that can settle it.
        let mut a = Device::new();
        let mut b = Device::new();
        defer_over_an_unsent_edit(&mut a, &mut b);

        b.conn.execute("DELETE FROM t_demo WHERE id = 'r1'", []).unwrap();

        assert!(
            refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "a deferral is owed a look at every open, because only the row can redeem it"
        );
        assert_eq!(b.pending_count(), 0, "and the entry finally settles, on the merits");
        assert_eq!(
            b.row(),
            Some(("v1".to_string(), Some("wide".to_string()))),
            "the replayed entry lands in full once nothing is in its way",
        );
    }

    #[test]
    fn a_deferral_left_by_a_binary_with_the_older_vocabulary_is_reclassified_on_upgrade() {
        // The upgrade boundary. A binary predating this classification left a deferred entry
        // carrying its INGEST reason — a version gap, at the then-current projector version — and
        // stamped the store. Nothing in that mark says the entry is actually blocked behind the
        // row, and no projector version moved, so all three triggers read false and the entry is
        // stranded in precisely the way #1005 exists to prevent. The vocabulary stamp is what
        // buys the one full pass that re-derives the reason.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'unsent', NULL)", [])
            .unwrap();

        // Exactly the state the older binary leaves: the ingest reason, at the current projector
        // version, under a store it has already stamped — and no vocabulary marker, because it had
        // no vocabulary to record.
        b.conn
            .execute("INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)", params![
                TABLE_SYNC_PROJECTOR_VERSION_KEY,
                TABLE_SYNC_PROJECTOR_VERSION.to_string()
            ])
            .unwrap();
        assert_eq!(
            b.pending_mark(),
            Some((
                PendingReason::NewerSpecVersion.as_db_str().to_string(),
                TABLE_SYNC_PROJECTOR_VERSION
            )),
            "the fixture is the legacy shape: a version-gap reason at the current version",
        );

        assert!(
            refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "an older classification vocabulary owes one full pass"
        );
        assert_eq!(
            b.pending_mark().unwrap().0,
            PendingReason::DeferredUnsentEdit.as_db_str(),
            "which re-derives the reason, moving the entry into the family that can redeem it",
        );

        // From here the ordinary deferral machinery takes over, and the entry is no longer
        // stranded: authoring the edit settles it.
        assert_eq!(b.produce(NEW_REGISTRY, "repo").len(), 1);
        assert_eq!(b.pending_count(), 0);
    }

    #[test]
    fn ingest_marks_an_unprojectable_payload_as_a_version_gap_even_over_unsent_work() {
        // Two properties at once, and they are the same property.
        //
        // The ORDERING: an entry this binary cannot project is a version gap whatever the row
        // holds, so the payload has to be settled before the row is consulted. The row here
        // holds an unsent local edit, so the guard would fire — and filing that as a
        // deferral would erase the version gap and move the entry into the family replayed
        // at every store open, where nothing could redeem it.
        //
        // The VOCABULARY argument that lets one store-global stamp cover a shared store also rests
        // on this: a version-gap reason means the same thing under every vocabulary, so ingest
        // recording one is safe in any binary. Ingest also records deferrals now, but only ones
        // this vocabulary can name.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());

        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'unsent', NULL)", [])
            .unwrap();
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());

        let (reason, version) = b.pending_mark().expect("the entry parked");
        assert_eq!(
            (PendingReason::from_db_str(&reason).map(PendingReason::is_deferral), version),
            (Some(false), TABLE_SYNC_PROJECTOR_VERSION),
            "ingest marks a version gap at the current version — never a deferral",
        );
    }

    #[test]
    fn the_vocabulary_stamp_is_raised_once_and_never_lowered() {
        // The pass is one-shot: having reclassified, it must not keep paying for a full pass at
        // every open. And an older binary sharing the store must not walk the stamp back and make a
        // newer one redo the work — a store folded by a newer vocabulary is still foldable here,
        // so the projector's refuse-outright guard does not apply and cannot be reused.
        let b = Device::new();
        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            stored_meta_i64(&b.conn, TABLE_SYNC_DEFERRAL_VOCABULARY_KEY).unwrap(),
            Some(TABLE_SYNC_DEFERRAL_VOCABULARY),
        );
        assert!(
            !refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "the reclassifying pass is owed once, not at every open"
        );

        // A newer binary has since reclassified under a wider vocabulary. Force a pass anyway — an
        // older projector stamp is the state that gets one — so the stamp write is genuinely
        // exercised rather than skipped along with the whole pass.
        b.conn
            .execute("UPDATE oplog_meta SET value = ?2 WHERE key = ?1", params![
                TABLE_SYNC_DEFERRAL_VOCABULARY_KEY,
                (TABLE_SYNC_DEFERRAL_VOCABULARY + 1).to_string()
            ])
            .unwrap();
        b.conn
            .execute("UPDATE oplog_meta SET value = ?2 WHERE key = ?1", params![
                TABLE_SYNC_PROJECTOR_VERSION_KEY,
                (TABLE_SYNC_PROJECTOR_VERSION - 1).to_string()
            ])
            .unwrap();
        assert!(
            refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "the pass runs, so its stamp writes are the thing under test"
        );
        assert_eq!(
            stored_meta_i64(&b.conn, TABLE_SYNC_DEFERRAL_VOCABULARY_KEY).unwrap(),
            Some(TABLE_SYNC_DEFERRAL_VOCABULARY + 1),
            "a newer vocabulary survives an older binary's pass",
        );
    }

    #[test]
    fn a_deferred_delete_settles_once_the_removal_is_authored() {
        let mut a = Device::new();
        let mut b = Device::new();
        park_over_an_unauthored_local_delete(&mut a, &mut b);

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.pending_mark(),
            Some((
                PendingReason::DeferredUnsentDelete.as_db_str().to_string(),
                TABLE_SYNC_PROJECTOR_VERSION
            )),
        );
        // SETTLEMENT is the observable here, not the row. The authored `Remove` takes
        // `MAX(lamport) + 1` counting the parked entries, so the row stays deleted whether or not
        // they are ever replayed — a content assertion would pass with the retry deleted.
        assert_eq!(b.produce(NEW_REGISTRY, "repo").len(), 1, "the removal is authored");
        assert_eq!(b.pending_count(), 0, "both deferred upserts lose to the tombstone and clear");
        assert_eq!(b.row(), None);
    }

    #[test]
    fn a_deferred_remove_replays_once_the_unreadable_cell_is_repaired() {
        // The stuck state #1017 chose deliberately: a `Remove` over a row this binary cannot read
        // is deferred rather than applied, because it would delete the row outright and repair
        // nothing. Redemption is the cell being fixed — an event with no version and no stamp, so
        // without a deferral trigger the entry would sit there after the repair, forever.
        let mut a = Device::new();
        let mut b = Device::new();
        park_a_remove_over_an_unreadable_row(&mut a, &mut b);
        assert!(refold_stale_projections_against(&b.conn, &[BOOL]).unwrap());
        assert_eq!(
            b.pending_mark(),
            Some((
                PendingReason::DeferredUnreadableRow.as_db_str().to_string(),
                TABLE_SYNC_PROJECTOR_VERSION
            )),
        );

        // The cell is repaired back into the Bool domain, matching what was published.
        b.conn.execute("UPDATE t_bool SET flag = 1 WHERE id = 'r1'", []).unwrap();

        assert!(refold_stale_projections_against(&b.conn, &[BOOL]).unwrap());
        assert_eq!(
            b.conn.query_row("SELECT COUNT(*) FROM t_bool", [], |r| r.get::<_, i64>(0)).unwrap(),
            0,
            "the deferred remove lands, taking the whole row — which is what a synced delete means",
        );
        assert_eq!(b.pending_count(), 0);
    }

    #[test]
    fn a_version_gap_is_not_retried_before_the_next_bump() {
        // The other family, and why the partition has to be exact. A version gap's outcome is
        // decided on the payload alone, so replaying one under an unchanged binary is guaranteed to
        // re-park it at the same reason. Retrying it at every open is pure cost.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());

        assert!(refold_stale_projections_against(&b.conn, OLD_REGISTRY).unwrap());
        assert_eq!(b.pending_mark().unwrap().0, PendingReason::NewerSpecVersion.as_db_str());
        assert!(
            !refold_stale_projections_against(&b.conn, OLD_REGISTRY).unwrap(),
            "a version gap waits for the binary to widen, not for another look"
        );
    }

    #[test]
    fn a_version_gapped_entry_over_an_unsent_edit_stays_a_version_gap() {
        // The ordering rule, as behavior. The guard fires here too — the row holds an unsent local
        // edit — but this entry cannot project whatever the row holds. Asking the guard first would
        // record the deferral instead, which both erases the version gap and moves the entry into
        // the family replayed at EVERY open, where nothing could redeem it until some unrelated
        // local edit happened to be authored.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());
        b.conn
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'unsent', NULL)", [])
            .unwrap();

        assert!(refold_stale_projections_against(&b.conn, OLD_REGISTRY).unwrap());
        assert_eq!(
            b.pending_mark().unwrap().0,
            PendingReason::NewerSpecVersion.as_db_str(),
            "the payload question is settled before the row question",
        );
        assert!(
            !refold_stale_projections_against(&b.conn, OLD_REGISTRY).unwrap(),
            "so it stays in the family whose trigger can actually redeem it",
        );
    }

    #[test]
    fn a_deferral_alone_replays_only_the_deferral_family() {
        // Trigger 3 fires at every open for as long as a deferral is steady, so it must not drag
        // the whole pending set along: nothing else's outcome can have changed under an unchanged
        // binary. Probed with an entry carrying a token this binary has no name for — a NEWER
        // binary's reason — over a row that projects cleanly, so a full pass would visibly clear it
        // and a narrowed pass visibly cannot.
        let mut a = Device::new();
        let mut b = Device::new();

        // r2 first, under the old column set, so B applies it outright.
        a.conn.execute("INSERT INTO t_demo(id, title) VALUES ('r2', 'v1')", []).unwrap();
        let narrow = a.produce(OLD_REGISTRY, "repo");
        assert_eq!(narrow.len(), 1);
        b.enroll(a.pubkey().fingerprint());
        assert_eq!(b.ingest(OLD_REGISTRY, "repo", &narrow, &a.pubkey()), vec![
            IngestOutcome::Applied
        ]);

        defer_over_an_unsent_edit(&mut a, &mut b);
        let poisoned = b
            .conn
            .execute(
                "UPDATE table_sync_entries
                    SET pending_reason = 'a_reason_from_the_future',
                        pending_projector_version = ?1
                  WHERE pending_reason IS NULL",
                params![TABLE_SYNC_PROJECTOR_VERSION],
            )
            .unwrap();
        assert_eq!(poisoned, 1, "exactly r2's applied entry is marked");

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            future_token_count(&b.conn),
            1,
            "a pass owed only by a deferral leaves the rest of the pending set alone",
        );

        // And the narrowing is CONDITIONAL, not absolute: once a version trigger fires, everything
        // outstanding is back in question and that same entry is replayed and cleared.
        b.age_pending_marks();
        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(future_token_count(&b.conn), 0, "a widened pass covers the whole pending set");
    }

    fn future_token_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM table_sync_entries
              WHERE pending_reason = 'a_reason_from_the_future'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_steady_deferral_is_not_rewritten_on_every_open() {
        // Trigger 3 brings the entry back at every open by design, so re-marking it unconditionally
        // would write an identical row every time — WAL traffic proportional to opens rather than
        // to anything happening.
        let mut a = Device::new();
        let mut b = Device::new();
        defer_over_an_unsent_edit(&mut a, &mut b);

        let before = b.conn.total_changes();
        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(b.conn.total_changes() - before, 1, "the projector stamp, and nothing else",);
    }

    #[test]
    fn an_entry_whose_stream_directory_is_gone_stops_owing_a_refold() {
        // A stream id is a ONE-WAY hash of (repo_id, account_id, incarnation_ref, scope_id), so an
        // entry with no directory row can never be placed. It used to be skipped in silence,
        // keeping whatever version it was last marked at — which made trigger 2 permanently true,
        // and cost every open of every checkout an IMMEDIATE transaction and a full pending scan
        // for an entry that can never project. Recording it at this version, in a family nothing
        // retries, is what ends that.
        let mut a = Device::new();
        let mut b = Device::new();
        let entries = author_wide_row(&mut a);
        b.enroll(a.pubkey().fingerprint());
        b.ingest(OLD_REGISTRY, "repo", &entries, &a.pubkey());
        b.conn.execute("DELETE FROM table_sync_streams", []).unwrap();
        b.age_pending_marks();

        assert!(refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap());
        assert_eq!(
            b.pending_mark(),
            Some((
                PendingReason::NoStreamContext.as_db_str().to_string(),
                TABLE_SYNC_PROJECTOR_VERSION
            )),
            "the state is recorded rather than left as an unexplained pending mark",
        );
        assert!(
            !refold_stale_projections_against(&b.conn, NEW_REGISTRY).unwrap(),
            "and it stops costing a write transaction at every store open",
        );
        assert_eq!(b.row(), None, "a purged repo's history still must not project");
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
        //
        // Pinned as the WHOLE SET, not variant by variant: a per-variant list silently omits any
        // variant added later (`NewerSpecVersion` was added and left unpinned exactly that way),
        // which is the case that most needs the pin. Adding a variant now fails here until it is
        // listed.
        //
        // The third column pins the RETRY FAMILY (`is_deferral`), for the same reason and with the
        // same consequences: it decides whether an entry is replayed at every store open or waits
        // for a projector bump, and the default a new variant lands in should be a decision, not a
        // match-arm accident.
        let pinned: Vec<(PendingReason, &str, bool)> =
            <PendingReason as strum::IntoEnumIterator>::iter()
                .map(|reason| (reason, reason.as_db_str(), reason.is_deferral()))
                .collect::<Vec<_>>();
        assert_eq!(
            pinned,
            vec![
                (PendingReason::UnknownColumn, "unknown_column", false),
                (PendingReason::NewerSpecVersion, "newer_spec_version", false),
                (PendingReason::PartialAfterImage, "partial_after_image", false),
                (PendingReason::MisstampedSpecVersion, "misstamped_spec_version", false),
                (PendingReason::UnknownOpKind, "unknown_op_kind", false),
                (PendingReason::UndecodablePayload, "undecodable_payload", false),
                (PendingReason::TableNotInScope, "table_not_in_scope", false),
                (PendingReason::NoStreamContext, "no_stream_context", false),
                (
                    PendingReason::DeferredIncarnationAuthority,
                    "deferred_incarnation_authority",
                    true,
                ),
                (PendingReason::DeferredUnsentEdit, "deferred_unsent_edit", true),
                (PendingReason::DeferredUnsentDelete, "deferred_unsent_delete", true),
                (PendingReason::DeferredUnreadableRow, "deferred_unreadable_row", true),
                (PendingReason::DeferredUnresolvedWinner, "deferred_unresolved_winner", true),
            ],
            "a stored classification token moved, or a new variant is unpinned"
        );
    }

    #[test]
    fn the_deferral_sql_list_is_derived_from_the_enum() {
        // The narrowed worklist and trigger 3 both probe by TOKEN, in SQL, where the compiler
        // cannot see the enum. Deriving the list is what keeps a new deferral variant from being
        // marked by the refold and then never picked up by the query that retries it — a silent
        // regression to the #1005 bug, for one reason only.
        let expected: Vec<String> = <PendingReason as strum::IntoEnumIterator>::iter()
            .filter(|reason| reason.is_deferral())
            .map(|reason| format!("'{}'", reason.as_db_str()))
            .collect();
        assert_eq!(store::deferral_tokens_sql(), expected.join(", "));
        assert!(
            store::deferral_tokens_sql().contains("'deferred_unsent_edit'"),
            "and it is not vacuously empty"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // Two openers of ONE database file.
    //
    // A store reached by binaries of different versions is a first-class configuration here —
    // linked worktrees share one database — but every test above drives it from a single
    // `Connection`, with the other binary simulated by hand-editing the stamp or an entry's
    // `pending_projector_version`. That cannot show what a second opener changes: whether the
    // pending mark, the projector stamp, and the unsent-local-edit guard are FILE state rather than
    // connection state, and whether the refold's IMMEDIATE transaction really contends for the
    // store's single write lock. The tests below open the same file twice, for real.

    /// One database file, schema-applied once, plus the scratch dir that owns it.
    struct SharedStore {
        dir: ScratchDir,
    }

    impl SharedStore {
        fn new(tag: &str) -> Self {
            let store = Self { dir: ScratchDir::new(tag) };
            let db = IndexConnection::open(&store.path()).unwrap();
            rag_rat_db::schema::apply(db.connection(), &crate::test_hooks()).unwrap();
            seed_incarnation(db.connection());
            db.execute_batch(
                "CREATE TABLE t_demo(id TEXT PRIMARY KEY, title TEXT, later_col TEXT) STRICT;",
            )
            .unwrap();
            store
        }

        fn path(&self) -> PathBuf {
            self.dir.join("index.sqlite")
        }

        fn open(&self) -> Opener {
            open_at(&self.path())
        }
    }

    /// A production opener of `path`. [`IndexConnection::open`] is what pins the real write-path
    /// pragmas — WAL plus `busy_timeout = 5000` — and those decide whether a contended IMMEDIATE
    /// transaction waits or fails, so a hand-rolled `Connection::open` here would exercise a
    /// configuration that does not ship.
    fn open_at(path: &Path) -> Opener {
        let db = IndexConnection::open(path).unwrap();
        let local = crate::local_device(db.connection(), 0).unwrap();
        Opener { db, local }
    }

    /// One opener of a [`SharedStore`]. Deliberately not a [`Device`]: `Device` OWNS its
    /// connection, while an opener borrows one from the `IndexConnection` carrying the pragmas
    /// above — so its write paths go through `Transaction::new_unchecked`, exactly as the
    /// engine's own callers do.
    struct Opener {
        db: IndexConnection,
        local: LocalDevice,
    }

    impl Opener {
        fn conn(&self) -> &Connection {
            self.db.connection()
        }

        fn fingerprint(&self) -> crate::op::DeviceFingerprint {
            self.local.secret().public().fingerprint()
        }

        fn enroll(&self, fp: crate::op::DeviceFingerprint) {
            self.conn()
                .execute(
                    "INSERT OR IGNORE INTO account_roster_history
                         (roster_ref, account_id, device_fingerprint, role, effective_at, \
                     closed_at)
                     VALUES (?1, ?2, ?3, 'owner', 0, NULL)",
                    params![fp.to_bytes().as_slice(), ACCOUNT.as_slice(), fp.to_bytes().as_slice()],
                )
                .unwrap();
        }

        /// Fallible, because the newer-projector refusal is one of the behaviors under test.
        fn produce(&self, registry: &[TableSpec], repo_id: &str) -> anyhow::Result<Vec<Vec<u8>>> {
            let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Deferred)?;
            let ctx = SyncCtx {
                repo_id,
                account_id: account(),
                incarnation_ref: [0x44; 32],
                device: &self.local,
                registry,
                now_ms: 0,
            };
            let out = engine::produce_and_author(&tx, &ctx)?;
            tx.commit()?;
            Ok(out)
        }

        fn ingest(
            &self,
            registry: &[TableSpec],
            repo_id: &str,
            entries: &[Vec<u8>],
            from: &crate::device::DevicePublic,
        ) -> Vec<IngestOutcome> {
            let tx =
                Transaction::new_unchecked(self.conn(), TransactionBehavior::Deferred).unwrap();
            let ctx = SyncCtx {
                repo_id,
                account_id: account(),
                incarnation_ref: [0x44; 32],
                device: &self.local,
                registry,
                now_ms: 0,
            };
            let out = entries
                .iter()
                .map(|bytes| engine::ingest(&tx, &ctx, "demo/1", bytes, from).unwrap().outcome)
                .collect();
            tx.commit().unwrap();
            out
        }

        fn refold(&self, registry: &[TableSpec]) -> anyhow::Result<bool> {
            refold_stale_projections_against(self.conn(), registry)
        }

        fn row(&self) -> Option<(String, Option<String>)> {
            self.conn()
                .query_row("SELECT title, later_col FROM t_demo WHERE id = 'r1'", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()
                .unwrap()
        }

        fn pending_count(&self) -> i64 {
            self.conn()
                .query_row(
                    "SELECT COUNT(*) FROM table_sync_entries WHERE pending_reason IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        }
    }

    /// The complete r1 row the wide entry projects to once a NEW-registry binary understands it.
    fn refolded_row() -> Option<(String, Option<String>)> {
        Some(("v1".to_string(), Some("wide".to_string())))
    }

    /// Park one entry in `store`: an in-memory peer authors r1 under the NEW column set, and an
    /// opener ingests it under the OLD one, which cannot project it. The opener is dropped, so the
    /// parked state survives only in the file.
    fn park_wide_entry(store: &SharedStore) {
        let mut peer = Device::new();
        let entries = author_wide_row(&mut peer);
        let opener = store.open();
        opener.enroll(peer.pubkey().fingerprint());
        assert_eq!(opener.ingest(OLD_REGISTRY, "repo", &entries, &peer.pubkey()), vec![
            IngestOutcome::Retained(PendingReason::NewerSpecVersion.as_db_str())
        ]);
        assert_eq!(opener.pending_count(), 1, "the fixture leaves exactly one entry parked");
    }

    #[test]
    fn a_deferral_recorded_by_one_opener_is_retried_by_the_other() {
        // Deferral marks and their trigger have to be FILE state, like every other part of the
        // pending record. Linked worktrees share one database, so the opener that records a
        // deferral is routinely not the one that later removes what it was blocked behind.
        let store = SharedStore::new("table-sync-two-openers-deferral");
        park_wide_entry(&store);

        let first = store.open();
        first
            .conn()
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'unsent', NULL)", [])
            .unwrap();
        assert!(first.refold(NEW_REGISTRY).unwrap());
        assert_eq!(first.pending_count(), 1, "the replay is refused to protect the unsent edit");
        drop(first);

        let second = store.open();
        assert_eq!(
            second.produce(NEW_REGISTRY, "repo").unwrap().len(),
            1,
            "this opener authors the edit"
        );
        assert_eq!(
            second.pending_count(),
            0,
            "and settles the deferral the OTHER opener recorded — the mark and its trigger are \
             file state, not connection state",
        );
        assert_eq!(second.row().unwrap().0, "unsent");
    }

    #[test]
    fn a_second_opener_refolds_what_the_first_one_parked() {
        let store = SharedStore::new("table-sync-two-openers-refold");
        park_wide_entry(&store);

        let first = store.open();
        let second = store.open();
        assert_eq!(
            first.fingerprint(),
            second.fingerprint(),
            "two openers of one file are one device identity, not two peers"
        );
        assert_eq!(
            second.pending_count(),
            1,
            "the pending mark is file state, not connection state"
        );
        assert_eq!(second.row(), None, "and nothing was applied in part");

        assert!(second.refold(NEW_REGISTRY).unwrap());
        assert_eq!(second.row(), refolded_row());

        // The other, still-open connection sees the committed result — including the stamp, so it
        // does not redo the work, and the anti-echo hashes, so it emits nothing. Every
        // single-connection test above assumes exactly this and cannot check it.
        assert_eq!(first.row(), refolded_row());
        assert_eq!(first.pending_count(), 0);
        assert!(!first.refold(NEW_REGISTRY).unwrap(), "the refold is one-shot across openers");
        assert!(first.produce(NEW_REGISTRY, "repo").unwrap().is_empty(), "and nothing echoes back");
    }

    #[test]
    fn the_refold_contends_for_the_stores_single_write_lock() {
        let store = SharedStore::new("table-sync-two-openers-busy");
        park_wide_entry(&store);

        let writer = store.open();
        let refolder = store.open();
        // A refold that will not wait, so the contention is observable rather than absorbed by the
        // production timeout (the sibling test covers the waiting half).
        refolder.conn().busy_timeout(std::time::Duration::ZERO).unwrap();

        // WAL admits exactly one writer, and the other opener is it.
        let held =
            Transaction::new_unchecked(writer.conn(), TransactionBehavior::Immediate).unwrap();
        held.execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r9', 'held', NULL)", [])
            .unwrap();

        let err = refolder
            .refold(NEW_REGISTRY)
            .expect_err("the refold must not proceed without the store's write lock");
        assert!(rag_rat_db::storage::is_busy(&err), "the refusal is SQLITE_BUSY, got: {err}");
        assert_eq!(refolder.pending_count(), 1, "and the worklist is untouched");

        // The same refold then succeeds, so the failure was contention rather than a worklist this
        // opener could never redeem.
        held.commit().unwrap();
        assert!(refolder.refold(NEW_REGISTRY).unwrap());
        assert_eq!(refolder.row(), refolded_row());
    }

    /// Set by [`note_blocked_then_retry`] the first time SQLite reports the write lock unavailable
    /// — the only reliable proof that a refold actually BLOCKED rather than finding the lock
    /// free. Read and reset by the single test below; nothing else touches it, so it is safe
    /// under the shared-process `cargo test` runner as well as under nextest.
    static REFOLD_BLOCKED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Stands in for the production `busy_timeout` (pinned separately by
    /// `setup_pins_the_write_path_pragmas` in `rag-rat-db`), because `sqlite3_busy_handler` gives a
    /// callback and the timeout pragma does not: the hand-off below has to be CAUSAL, not timed. A
    /// sleep-based hand-off fails both ways — the holder commits before the refold reaches `BEGIN`
    /// (the test passes without contending), or the holder is descheduled past the timeout (the
    /// test fails for scheduling reasons).
    ///
    /// The ~30s ceiling is a HANG-STOP, not a timeout under test: the holder releases the lock
    /// within its own 5s watchdog, so patience an order of magnitude past that cannot be reached by
    /// a correct run — and matching the production 5s here would just reintroduce the spurious
    /// failure the causal hand-off exists to remove.
    fn note_blocked_then_retry(attempts: i32) -> bool {
        REFOLD_BLOCKED.store(true, std::sync::atomic::Ordering::SeqCst);
        if attempts > 3_000 {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        true
    }

    #[test]
    fn the_refold_blocks_on_a_concurrent_writer_and_completes_once_it_commits() {
        use std::sync::atomic::Ordering;

        let store = SharedStore::new("table-sync-two-openers-wait");
        park_wide_entry(&store);
        REFOLD_BLOCKED.store(false, Ordering::SeqCst);

        // Opened BEFORE the write lock is taken: `IndexConnection::open` writes pragmas of its own,
        // and this test is about the refold's contention, not the opener's.
        let refolder = store.open();
        refolder.conn().busy_handler(Some(note_blocked_then_retry)).unwrap();

        let path = store.path();
        let (holding, lock_held) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let opener = open_at(&path);
            let tx =
                Transaction::new_unchecked(opener.conn(), TransactionBehavior::Immediate).unwrap();
            tx.execute(
                "INSERT INTO t_demo(id, title, later_col) VALUES ('r9', 'concurrent', NULL)",
                [],
            )
            .unwrap();
            holding.send(()).unwrap();
            // Hold until the refolder is provably blocked, so the release is caused by the
            // contention rather than by a clock. The deadline is only a watchdog: if the refold
            // never blocks, this returns and the assertion below reports it instead of hanging.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !REFOLD_BLOCKED.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            tx.commit().unwrap();
        });

        lock_held.recv().unwrap();
        assert!(refolder.refold(NEW_REGISTRY).unwrap(), "the refold completes once the lock frees");
        writer.join().unwrap();
        assert!(
            REFOLD_BLOCKED.load(Ordering::SeqCst),
            "the refold must have contended for the write lock, not found it free"
        );

        // And neither write was lost to the other.
        assert_eq!(refolder.row(), refolded_row());
        let concurrent: String = refolder
            .conn()
            .query_row("SELECT title FROM t_demo WHERE id = 'r9'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(concurrent, "concurrent");
    }

    #[test]
    fn an_opener_refuses_to_write_a_store_the_other_folded_with_a_newer_projector() {
        let store = SharedStore::new("table-sync-two-openers-newer");
        let newer = store.open();
        let older = store.open();

        // One process has one `TABLE_SYNC_PROJECTOR_VERSION`, so the newer binary can only be
        // modelled by the stamp it leaves behind — but the stamp CROSSING two connections is real,
        // and that crossing is what the refusal depends on in the field.
        newer
            .conn()
            .execute("INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)", params![
                TABLE_SYNC_PROJECTOR_VERSION_KEY,
                (TABLE_SYNC_PROJECTOR_VERSION + 1).to_string()
            ])
            .unwrap();
        older
            .conn()
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'v', NULL)", [])
            .unwrap();

        let err = older
            .produce(OLD_REGISTRY, "repo")
            .expect_err("an older projector must not author into a store a newer one folded");
        assert!(
            err.to_string().contains("newer rag-rat"),
            "the refusal names the cause, got: {err}"
        );
        assert!(!older.refold(NEW_REGISTRY).unwrap(), "nor fold the stamp back down");
    }

    #[test]
    fn the_unsent_local_edit_guard_sees_the_other_openers_committed_write() {
        // The guard that keeps the refold from destroying a change no peer has seen must read the
        // STORE, not one connection's view of it: on a shared file the local edit routinely arrives
        // through a different opener than the one that refolds at open.
        let store = SharedStore::new("table-sync-two-openers-unsent");
        park_wide_entry(&store);

        let editor = store.open();
        editor
            .conn()
            .execute("INSERT INTO t_demo(id, title, later_col) VALUES ('r1', 'unsent', NULL)", [])
            .unwrap();

        let refolder = store.open();
        assert!(refolder.refold(NEW_REGISTRY).unwrap());
        assert_eq!(
            refolder.row().unwrap().0,
            "unsent",
            "the other opener's unsent edit survives the refold"
        );
        assert_eq!(refolder.pending_count(), 1, "and the entry stays outstanding rather than lost");
    }
}
