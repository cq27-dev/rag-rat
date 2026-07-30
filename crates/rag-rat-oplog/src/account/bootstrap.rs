//! The store's persisted local account — its ONE self-authorizing `AccountGenesis`, minted once and
//! reused (sync phase C3.4a, #662).
//!
//! Where [`crate::local_device`] is the machine's signing identity, the local account is the
//! *principal* this install authors under: a seq-0, self-authorizing account genesis whose founder
//! is the local device. It is minted from OS entropy on first use and pinned by the single-row
//! `oplog_local_account` pointer table (`CHECK (id = 0)`), so later C3.4 slices author owner-bound
//! `/3` content under a stable account identity instead of a fresh genesis per call. Store-global,
//! not repo-scoped — the exact analog of the device identity.
//!
//! [`local_account`] is the single accessor. The pointer stores the genesis entry's content hash,
//! NOT the account_id: the id is recovered by resolving that hash in the candidate DAG (§4 commits
//! the account_id inside the genesis payload), so the pointer and the stored genesis stay one
//! source of truth — the mint transaction commits both, or neither.
//!
//! GENESIS ONLY (this slice): the general non-genesis account-op seam (StreamOwn, DeviceAdd, …) is
//! a later slice. This mint reuses the account layer's own [`super::storage::insert_candidate`] +
//! [`super::storage::refold_in_tx`] seams directly; it MUST NOT go through
//! [`super::storage::account_ingest`], which self-transacts with its own `BEGIN IMMEDIATE` and
//! cannot be nested inside the mint transaction.
//!
//! RACE SAFETY. Each mint attempt draws a fresh nonce, so two racing first-callers propose DISTINCT
//! accounts; convergence comes from the pointer. A concurrent first-open therefore behaves exactly
//! like the device path: the process that loses the single-row insert adopts the winner's account
//! and discards its own proposal — the loser NEVER authors a second genesis, because a second
//! genesis is a second permanent, unrecoverable account identity. The pointer insert is the FIRST
//! write and gates the genesis insert; the genesis is authored only if our hash won the pointer,
//! and it is authored in the SAME transaction, so a durably-committed pointer always names an
//! accepted genesis.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::AccountId;
use super::envelope::{
    self, AccountEntryHeader, SignedAccountEntry, VerifiedAccountEntry, sign_account_entry,
};
use super::id::account_id_from_genesis_payload;
use super::ops::{self, AccountOp};
use super::storage::{self, CandidateInsert};
use crate::local_device;
use crate::op::DeviceFingerprint;

/// Return this store's persisted local account, minting its self-authorizing genesis on the first
/// call and returning the SAME account stably thereafter. `now_ms` stamps a freshly-minted genesis
/// and its pointer row (injected, matching the op-log store convention); it is ignored once an
/// account exists.
///
/// The account_id is the §4 content-commitment of the genesis payload, resolved through the
/// pointer: idempotent, and a concurrent first-open converges on one account (the loser of the
/// pointer race adopts the winner's genesis — see the module header).
pub fn local_account(conn: &Connection, now_ms: i64) -> anyhow::Result<AccountId> {
    // Fast path: the pointer is already minted — adopt it WITHOUT taking the writer lock.
    // Candidates are grow-only, so a genesis this read resolves can never disappear underneath
    // us.
    if let Some(account_id) = read_local_account(conn)? {
        return Ok(account_id);
    }
    // First mint: an authored, irreplaceable identity write → durable (#560). Raise `synchronous =
    // FULL` for this txn only, restored on drop; set OUTSIDE the txn (SQLite applies a
    // `synchronous` change to SUBSEQUENT transactions only).
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    mint_local_account_in_tx(&tx, now_ms)?;
    tx.commit()?;
    // Mandatory re-read (mirrors the device path): a concurrent first-caller that WON the pointer
    // race committed its genesis before us, so adopt whichever genesis the pointer now names — ours
    // if we won, the incumbent's if we lost.
    read_local_account(conn)?
        .context("local account pointer missing immediately after mint (single-row insert lost?)")
}

/// Adopt an already-ingested, accepted account genesis as this store's local account. Enrollment
/// uses this after cryptographically verifying and ingesting the inviter's bootstrap; unlike
/// [`local_account`], it never mints a competing genesis.
pub fn adopt_local_account(
    conn: &Connection,
    account_id: AccountId,
    genesis_hash: [u8; 32],
    now_ms: i64,
) -> anyhow::Result<()> {
    if let Some(existing) = read_local_account(conn)? {
        anyhow::ensure!(existing == account_id, "store already belongs to another local account");
        return Ok(());
    }
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    adopt_local_account_in_tx(&tx, account_id, genesis_hash, now_ms)?;
    tx.commit()?;
    Ok(())
}

/// Atomically ingest a verified enrollment bootstrap, confirm the acknowledged device is
/// effective, and adopt its genesis as this store's local account. No bootstrap row can become
/// durable without the matching pointer, so a crash or rejected later entry cannot strand the
/// one-time enrollment between identities.
pub struct EnrollmentBootstrap<'a> {
    pub account_entries: &'a [Vec<u8>],
    pub account_id: AccountId,
    pub genesis_hash: [u8; 32],
    pub device_fingerprint: DeviceFingerprint,
    pub device_add_hash: [u8; 32],
    pub now_ms: i64,
}

/// The joiner-side admission budget this store can offer an enrollment redemption, clamped at
/// zero (headroom goes negative past the grow-only caps). Sent in the enrollment request so the
/// owner can measure its exact receipt against it BEFORE consuming the one-time nonce (#945).
///
/// Already-held candidates are NOT credited here — the request carries their hashes separately
/// ([`held_account_entry_hashes`]), so the owner credits exactly the confirmed intersection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnrollmentBudget {
    pub account_entries_remaining: u64,
    pub account_bytes_remaining: u64,
    pub global_entries_remaining: u64,
    pub global_bytes_remaining: u64,
}

/// Complete candidate inventory bound for one enrollment request.
pub const ENROLLMENT_HELD_ENTRY_HASHES_MAX: usize = storage::CANDIDATES_PER_ACCOUNT_MAX;

pub fn enrollment_budget(
    conn: &Connection,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<EnrollmentBudget> {
    let headroom = storage::candidate_capacity_headroom(conn, account_id, now_ms)?;
    let clamp = |value: i64| u64::try_from(value).unwrap_or(0);
    Ok(EnrollmentBudget {
        account_entries_remaining: clamp(headroom.account_entries_remaining),
        account_bytes_remaining: clamp(headroom.account_bytes_remaining),
        global_entries_remaining: clamp(headroom.global_entries_remaining),
        global_bytes_remaining: clamp(headroom.global_bytes_remaining),
    })
}

/// Durable candidate-capacity reservations for outstanding enrollment invites (#945). A minted
/// invite reserves the exact entries/bytes its mandatory `DeviceAdd` plus stream-key wraps will
/// consume, and [`storage::insert_candidate`] charges active reservations against the same
/// grow-only counters — so ordinary ingest or another mint cannot consume headroom an outstanding
/// ticket was measured against and strand it permanently. Redemption releases its own reservation
/// under the writer lock; expiry frees it (`expires_at_ms` is the invite's own expiry).
pub fn upsert_account_candidate_reservation_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    reservation_id: [u8; 32],
    reserved_entries: u64,
    reserved_bytes: u64,
    reserved_targets: u64,
    expires_at_ms: i64,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO account_candidate_reservations(
             reservation_id, account_id, reserved_entries, reserved_bytes, reserved_targets,
             expires_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(reservation_id) DO UPDATE SET
             account_id = excluded.account_id,
             reserved_entries = excluded.reserved_entries,
             reserved_bytes = excluded.reserved_bytes,
             reserved_targets = excluded.reserved_targets,
             expires_at_ms = excluded.expires_at_ms",
        params![
            reservation_id.as_slice(),
            account_id.to_bytes().as_slice(),
            i64::try_from(reserved_entries)?,
            i64::try_from(reserved_bytes)?,
            i64::try_from(reserved_targets)?,
            expires_at_ms,
        ],
    )?;
    Ok(())
}

/// Release one invite's reservation. Called inside the redemption transaction immediately before
/// the mandatory entries are authored, so the freed capacity is consumed by exactly the writes it
/// was measured for; a rollback restores the reservation with everything else.
pub fn release_account_candidate_reservation_in_tx(
    tx: &Transaction<'_>,
    reservation_id: [u8; 32],
) -> anyhow::Result<()> {
    tx.execute("DELETE FROM account_candidate_reservations WHERE reservation_id = ?1", [
        reservation_id.as_slice(),
    ])?;
    Ok(())
}

/// Delete expired reservations. Correctness never depends on this — the counters filter on
/// `expires_at_ms` — it only keeps the table bounded.
pub fn prune_account_candidate_reservations_in_tx(
    conn: &Connection,
    now_ms: i64,
) -> anyhow::Result<()> {
    conn.execute("DELETE FROM account_candidate_reservations WHERE expires_at_ms <= ?1", [now_ms])?;
    Ok(())
}

/// Every authenticated candidate hash this store holds for `account_id`, sorted for canonical
/// enrollment encoding. Parked unauthenticated envelopes are normal-sync work, not bootstrap data.
pub fn held_account_entry_hashes(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<[u8; 32]>> {
    let mut stmt = conn.prepare(
        "SELECT entry_hash FROM account_entries WHERE account_id = ?1 ORDER BY entry_hash",
    )?;
    let hashes = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        hashes.len() <= ENROLLMENT_HELD_ENTRY_HASHES_MAX,
        "account held-proof inventory exceeds the enrollment wire bound"
    );
    hashes
        .into_iter()
        .map(|hash| {
            <[u8; 32]>::try_from(hash.as_slice())
                .map_err(|_| anyhow::anyhow!("stored entry_hash is not exactly 32 bytes"))
        })
        .collect()
}

/// One bootstrap entry staged for causal ingestion: its raw bytes plus the structural decode (the
/// full signature verify still happens in `account_ingest_in_tx`). Undecodable entries sort last
/// so they reach ingest — and their rejection rolls the adoption back — after every decodable
/// entry had its chance.
struct BootstrapEntry<'a> {
    bytes: &'a [u8],
    decoded: Option<SignedAccountEntry>,
}

impl BootstrapEntry<'_> {
    /// The canonical deterministic pre-order the causal worklist scans: `(log_id, seq,
    /// entry_hash, signed_bytes)` — the same key `account_entries_for_sync` emits, with
    /// `signed_bytes` as the tie-break because two envelopes can share an `entry_hash` yet carry
    /// different signatures.
    fn causal_pre_order(&self, other: &Self) -> std::cmp::Ordering {
        match (&self.decoded, &other.decoded) {
            (Some(a), Some(b)) => (a.header.log_id, a.header.seq, a.entry_hash, &a.signed_bytes)
                .cmp(&(b.header.log_id, b.header.seq, b.entry_hash, &b.signed_bytes)),
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, None) => self.bytes.cmp(other.bytes),
        }
    }
}

pub fn adopt_enrollment_bootstrap(
    conn: &Connection,
    bootstrap: EnrollmentBootstrap<'_>,
) -> anyhow::Result<()> {
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Ingest in CAUSAL order — an entry only after the entry introducing its signer's key. The
    // receipt's raw `(log_id, seq, entry_hash)` order puts EVERY promoted device's seq-0 control
    // entry before the founder-chain DeviceAdd introducing its key, so raw-order ingestion parks
    // them all simultaneously and can trip the pre-verify eviction budget — a deterministic
    // adoption failure replayed for 24h against a committed enrollment (#945). The fold is
    // order-free (I8), so reordering changes only transient parking, never the final accepted
    // set; receipt bytes and their canonical-CBOR identity are untouched.
    let mut pending: Vec<BootstrapEntry<'_>> = bootstrap
        .account_entries
        .iter()
        .map(|bytes| BootstrapEntry { bytes, decoded: envelope::decode_account_signed(bytes).ok() })
        .collect();
    pending.sort_by(BootstrapEntry::causal_pre_order);
    // Seed resolvability from what this store ALREADY holds (a prior sync session may have
    // delivered some candidates), then drain the worklist: ingest every entry whose signer
    // resolves, growing the map with the keys each ingested entry itself certifies — the exact
    // `stored_device_pubkeys` + `add_self_pubkey` semantics ingest applies.
    let mut resolved = storage::stored_device_pubkeys(&tx, bootstrap.account_id)?;
    loop {
        let mut progressed = false;
        let mut still_pending = Vec::with_capacity(pending.len());
        for entry in pending {
            let ready = entry.decoded.as_ref().is_some_and(|signed| {
                resolved.contains_key(&signed.header.device_fingerprint)
                    || storage::self_certifies_signer(&signed.header, &signed.payload)
            });
            if ready {
                let signed = entry.decoded.as_ref().expect("ready implies decoded");
                let resolved_signer = resolved.get(&signed.header.device_fingerprint).copied();
                storage::stage_enrollment_bootstrap_entry_in_tx(
                    &tx,
                    entry.bytes,
                    resolved_signer,
                    bootstrap.now_ms,
                )?;
                storage::add_self_pubkey(&mut resolved, &signed.header, &signed.payload);
                progressed = true;
            } else {
                still_pending.push(entry);
            }
        }
        pending = still_pending;
        if !progressed {
            break;
        }
    }
    anyhow::ensure!(
        pending.is_empty(),
        "enrollment bootstrap contains an entry whose signer is not certified by the candidate \
         snapshot"
    );
    // The one-time adoption fold contains ONLY receipt candidates and already-authenticated local
    // candidates; the acknowledged DeviceAdd must win THAT fold before the local-account pointer
    // commits. Pre-existing parked rows are retried only after the adoption is durable — a newly
    // resolvable parked sibling can no longer roll the one-time bootstrap back, and its later
    // promotion is ordinary best-effort queue work.
    storage::finish_enrollment_bootstrap_in_tx(&tx, bootstrap.account_id, bootstrap.now_ms)?;
    let enrolled: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_roster_history
              WHERE account_id = ?1
                AND device_fingerprint = ?2
                AND roster_ref = ?3
                AND closed_at IS NULL
        )",
        params![
            bootstrap.account_id.to_bytes().as_slice(),
            bootstrap.device_fingerprint.to_bytes().as_slice(),
            bootstrap.device_add_hash.as_slice(),
        ],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        enrolled,
        "account bootstrap did not make the acknowledged DeviceAdd effective"
    );
    adopt_local_account_in_tx(&tx, bootstrap.account_id, bootstrap.genesis_hash, bootstrap.now_ms)?;
    tx.commit()?;
    // The caller retries parked rows the receipt's keys may now resolve in a SEPARATE
    // best-effort maintenance pass (`retry_enrollment_pre_verify`): the enrollment and
    // local-account pointer are durable at this point, so queue maintenance must never
    // invalidate the completed enrollment or re-enter its one-time fold.
    Ok(())
}

fn adopt_local_account_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    genesis_hash: [u8; 32],
    now_ms: i64,
) -> anyhow::Result<()> {
    if let Some(existing) = read_local_account(tx)? {
        anyhow::ensure!(existing == account_id, "store already belongs to another local account");
        return Ok(());
    }
    let accepted: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1
               FROM account_entries e
               JOIN account_entry_status s ON s.entry_hash = e.entry_hash
              WHERE e.entry_hash = ?1
                AND e.account_id = ?2
                AND e.log_id = 0
                AND e.seq = 0
                AND s.status = 'accepted'
         )",
        params![genesis_hash.as_slice(), account_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    anyhow::ensure!(accepted, "enrollment genesis is not accepted for the expected account");
    tx.execute(
        "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
         VALUES (0, ?1, ?2)",
        params![genesis_hash.as_slice(), now_ms],
    )?;
    Ok(())
}

/// Mint the local account inside the caller's IMMEDIATE transaction, or adopt an incumbent and
/// author nothing. See the module header for the race-safety contract.
fn mint_local_account_in_tx(tx: &Transaction<'_>, now_ms: i64) -> anyhow::Result<()> {
    // Re-check UNDER the writer lock: a racer may have minted between the lock-free fast path and
    // our IMMEDIATE acquire. If the pointer already exists, adopt it and author NOTHING — a second
    // genesis would be a second permanent account identity (unrecoverable).
    if read_pointer_hash(tx)?.is_some() {
        return Ok(());
    }
    let device = local_device(tx, now_ms)?;

    // A fresh 16-byte nonce from OS entropy makes each mint attempt derive a DISTINCT account_id,
    // so two racing first-callers propose different accounts; the pointer insert below is what
    // forces them to converge on exactly one.
    let mut nonce16 = [0u8; 16];
    getrandom::fill(&mut nonce16)
        .map_err(|e| anyhow::anyhow!("OS CSPRNG failed to seed the account genesis nonce: {e}"))?;

    let op = AccountOp::AccountGenesis {
        ed25519_pubkey: device.public().to_bytes(),
        x25519_pubkey: device.x25519_public().to_bytes(),
        nonce16,
        created_at_ms: u64::try_from(now_ms)
            .map_err(|_| anyhow::anyhow!("now_ms is negative; cannot stamp a genesis"))?,
        label: None,
    };
    let payload = ops::encode(&op)
        .map_err(|err| anyhow::anyhow!("encoding the account genesis op failed: {err}"))?;
    let account_id = account_id_from_genesis_payload(&payload);
    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: device.fingerprint(),
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: ops::entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let signed = sign_account_entry(device.secret(), &header, &payload)?;

    // The pointer insert is the FIRST write and the race gate. `ON CONFLICT DO NOTHING` lets a
    // racer that already minted keep its row; we then re-read and author OUR genesis only if
    // OUR hash won the pointer. (Belt-and-suspenders under IMMEDIATE — the re-check above
    // already adopts a committed incumbent — but it keeps "one committed pointer ⇒ exactly one
    // genesis" true regardless of isolation, since the genesis insert cannot precede a lost
    // pointer race.)
    tx.execute(
        "INSERT INTO oplog_local_account(id, genesis_entry_hash, created_at_ms)
         VALUES (0, ?1, ?2) ON CONFLICT(id) DO NOTHING",
        params![signed.entry_hash.as_slice(), now_ms],
    )?;
    let pointer = read_pointer_hash(tx)?
        .context("local account pointer missing immediately after its own insert")?;
    if pointer != signed.entry_hash {
        // Lost the pointer race: adopt the incumbent, author no genesis.
        return Ok(());
    }

    // We won the pointer — author the genesis candidate and fold it in THIS transaction. Because
    // the pointer insert and the genesis insert share one transaction, any failure below
    // (including a genesis that does not fold effective) rolls BOTH back, so a
    // durably-committed pointer always names an accepted genesis and incumbent adoption is
    // unconditionally safe.
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    match storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)? {
        CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {},
        CandidateInsert::AtCapacity(scope) => anyhow::bail!(
            "the account candidate store is at capacity ({scope:?}); cannot mint the local account",
        ),
    }
    let statuses = storage::refold_in_tx(tx, account_id, now_ms)?;
    // Verify the genesis folded EFFECTIVE (accepted), not merely inserted — never return a wedged
    // account.
    match statuses.get(&verified.entry_hash).map(String::as_str) {
        Some("accepted") => Ok(()),
        other => anyhow::bail!(
            "the local account genesis did not fold effective (status {other:?}); refusing to \
             mint a wedged account",
        ),
    }
}

/// This store's local account resolved from the pointer: its `account_id` and the `genesis_hash`
/// that names its self-authorizing genesis (the roster_ref an owner-authored `/3` entry cites).
pub(super) struct LocalAccountRef {
    pub(super) account_id: AccountId,
    pub(super) genesis_hash: [u8; 32],
}

/// Resolve the already-minted local account from the pointer WITHOUT minting — the in-tx content
/// author seam needs both the `account_id` (the `/3` `author_account_id`) and the genesis entry
/// hash (its `roster_ref`), reading whatever snapshot `conn` is already in. `None` when no account
/// has been minted yet: [`local_account`] (which self-transacts and cannot nest) is the mint path,
/// so a caller authoring `/3` content inside its own IMMEDIATE txn must have minted the account
/// first.
pub(super) fn local_account_ref(conn: &Connection) -> anyhow::Result<Option<LocalAccountRef>> {
    let Some(genesis_hash) = read_pointer_hash(conn)? else {
        return Ok(None);
    };
    let account_id = resolve_account_for_genesis(conn, &genesis_hash)?;
    Ok(Some(LocalAccountRef { account_id, genesis_hash }))
}

/// Resolve this store's local account from the pointer, or `None` if none is minted yet — the
/// read-only, non-minting twin of [`local_account`] (which mints a genesis on first call). Callers
/// that must NOT create identity state (e.g. a server that requires an already-enrolled account)
/// use this. The pointer is a content address; the account_id is recovered by looking the genesis
/// up in the candidate DAG (§4 commits the account_id inside the genesis payload, so this is the
/// one true id).
pub fn read_local_account(conn: &Connection) -> anyhow::Result<Option<AccountId>> {
    let Some(genesis_hash) = read_pointer_hash(conn)? else {
        return Ok(None);
    };
    Ok(Some(resolve_account_for_genesis(conn, &genesis_hash)?))
}

/// Recover the account_id the pointer names by looking its genesis up in the candidate DAG (§4
/// commits the account_id inside the genesis payload, so this is the one true id). A pointer naming
/// a genesis absent from `account_entries` is a corrupted pointer and errors.
fn resolve_account_for_genesis(
    conn: &Connection,
    genesis_hash: &[u8; 32],
) -> anyhow::Result<AccountId> {
    let account_bytes: Vec<u8> = conn
        .query_row(
            "SELECT account_id FROM account_entries WHERE entry_hash = ?1",
            params![genesis_hash.as_slice()],
            |row| row.get(0),
        )
        .optional()?
        .context(
            "oplog_local_account points at a genesis absent from account_entries (corrupted \
             pointer)",
        )?;
    let account_id: [u8; 32] = account_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("stored account_id is not exactly 32 bytes"))?;
    Ok(AccountId::from_bytes(account_id))
}

/// This store's genesis entry hash, or `None` when no account has been minted — the account's one
/// permanently stable secret-ish identifier.
///
/// Distinct from [`read_local_account`], which returns the account_id: the two are DIFFERENT
/// digests of the same genesis payload, so holding one does not yield the other. That asymmetry is
/// load-bearing for callers that need a value every enrolled device shares but a peer who has only
/// seen the account_id cannot compute. Non-minting, like [`read_local_account`].
pub fn read_local_account_genesis(conn: &Connection) -> anyhow::Result<Option<[u8; 32]>> {
    read_pointer_hash(conn)
}

/// Read the single-row pointer's genesis hash, or `None` when no account has been minted.
fn read_pointer_hash(conn: &Connection) -> anyhow::Result<Option<[u8; 32]>> {
    let hash: Option<Vec<u8>> = conn
        .query_row("SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0", [], |row| {
            row.get(0)
        })
        .optional()?;
    hash.map(|bytes| {
        <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| anyhow::anyhow!("stored genesis_entry_hash is not exactly 32 bytes"))
    })
    .transpose()
}

/// Raise SQLite `synchronous = FULL` for the duration of an authored write, restoring `NORMAL` on
/// drop (#560). The index connection defaults to `NORMAL` (the right policy for high-frequency,
/// fully reconstructable derived-index writes); an authored identity mint is the opposite class —
/// irreplaceable and it returns success to the caller, so it must fsync the WAL on commit. `begin`
/// MUST run OUTSIDE the transaction (SQLite applies a `synchronous` change to SUBSEQUENT
/// transactions only); the guard is held across the mint `BEGIN .. COMMIT` and restores on every
/// path (including error/panic), so a shared connection is never stranded at FULL. Mirrors
/// `query::memory::authoring::AuthoredDurability`, which the account layer cannot reach up into
/// (the dependency is one-way).
pub struct AuthoredDurability<'a> {
    conn: &'a Connection,
}

impl<'a> AuthoredDurability<'a> {
    pub fn begin(conn: &'a Connection) -> anyhow::Result<Self> {
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        Ok(Self { conn })
    }
}

impl Drop for AuthoredDurability<'_> {
    fn drop(&mut self) {
        // Best-effort restore of the connection default; runs after the mint txn committed/rolled
        // back, so no transaction is open. A stray failure could only leave the connection on the
        // *safer*, slower setting, never a less durable one.
        let _ = self.conn.execute_batch("PRAGMA synchronous = NORMAL;");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use rag_rat_db::schema;

    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    fn genesis_row_count(conn: &Connection) -> i64 {
        // Gated on `log_id == CONTROL_LOG` (S-f): a fresh-numbered secrets tag colliding with the 0
        // number must not inflate this witness.
        conn.query_row(
            "SELECT COUNT(*) FROM account_entries WHERE entry_type = ?1 AND log_id = ?2",
            params![ops::entry_type::ACCOUNT_GENESIS, crate::account::fold::CONTROL_LOG],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn pointer_row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oplog_local_account", [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn mint_once_then_reuse_returns_the_same_account_and_one_genesis() {
        let conn = db();
        let first = local_account(&conn, NOW).expect("mint");
        // A later call (a different `now_ms`, as a reopen would have) returns the SAME account and
        // mints no second genesis or pointer.
        let second = local_account(&conn, NOW + 5_000).expect("reuse");
        assert_eq!(first, second, "the local account is stable across calls");
        assert_eq!(genesis_row_count(&conn), 1, "re-calling does not mint a second genesis");
        assert_eq!(pointer_row_count(&conn), 1, "exactly one pointer row");
    }

    #[test]
    fn the_genesis_folds_effective_and_the_account_is_live() {
        let conn = db();
        let account_id = local_account(&conn, NOW).expect("mint");
        // The pointer names an ACCEPTED genesis (folded effective, not merely inserted).
        let genesis_hash: Vec<u8> = conn
            .query_row(
                "SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM account_entry_status WHERE entry_hash = ?1",
                params![genesis_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "accepted", "the local account genesis folds effective");
        // The account projects LIVE with a positive effective control-fold length.
        let (classification, effective_count): (String, i64) = conn
            .query_row(
                "SELECT classification, effective_count FROM account_auth_state
                 WHERE account_id = ?1",
                params![account_id.to_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(classification, "live", "the account is live");
        assert!(effective_count > 0, "the genesis counts toward the effective fold");
    }

    #[test]
    fn the_pointer_resolves_to_the_genesis_committed_account_id() {
        let conn = db();
        let account_id = local_account(&conn, NOW).expect("mint");
        // The stored account_id is exactly the §4 commitment of the stored genesis payload.
        let signed_bytes: Vec<u8> = conn
            .query_row(
                "SELECT e.signed_bytes
                 FROM oplog_local_account p
                 JOIN account_entries e ON e.entry_hash = p.genesis_entry_hash
                 WHERE p.id = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let signed = super::super::envelope::decode_account_signed(&signed_bytes).unwrap();
        assert_eq!(
            account_id_from_genesis_payload(&signed.payload),
            account_id,
            "the resolved account_id is the genesis commitment",
        );
    }

    #[test]
    fn enrollment_inventory_returns_every_candidate_beyond_the_old_prefix() {
        let conn = db();
        let account = local_account(&conn, NOW).unwrap();
        for ordinal in 1u64..=120 {
            let mut hash = [0u8; 32];
            hash[24..].copy_from_slice(&ordinal.to_be_bytes());
            conn.execute(
                "INSERT INTO account_entries(
                     entry_hash, account_id, log_id, device_fingerprint, seq, prev_hash,
                     parent_ref, authority_ref, entry_type, accepted, signed_bytes, received_at_ms)
                 VALUES (?1, ?2, 3, ?3, ?4, NULL, NULL, NULL, 255, 0, X'00', ?5)",
                params![
                    hash.as_slice(),
                    account.to_bytes().as_slice(),
                    [9u8; 32].as_slice(),
                    i64::try_from(ordinal).unwrap(),
                    NOW,
                ],
            )
            .unwrap();
        }

        let parked_entry_hash = [0xf0; 32];
        let parked_signed_hash = [0xf1; 32];
        conn.execute(
            "INSERT INTO account_pre_verify(
                 signed_hash, entry_hash, claimed_account_id, claimed_fingerprint, raw_bytes,
                 received_at_ms)
             VALUES (?1, ?2, ?3, ?4, X'00', ?5)",
            params![
                parked_signed_hash.as_slice(),
                parked_entry_hash.as_slice(),
                account.to_bytes().as_slice(),
                [0xf2u8; 32].as_slice(),
                NOW,
            ],
        )
        .unwrap();

        let hashes = held_account_entry_hashes(&conn, account).unwrap();
        assert_eq!(hashes.len(), 121, "genesis and every authenticated candidate are advertised");
        assert!(!hashes.contains(&parked_signed_hash));
        assert!(!hashes.contains(&parked_entry_hash));
        assert!(hashes.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn the_in_tx_mint_adopts_an_incumbent_pointer_without_a_second_genesis() {
        let conn = db();
        // An incumbent is already minted (the "pre-inserted pointer + genesis").
        let incumbent = local_account(&conn, NOW).expect("incumbent mint");
        // Drive the in-txn mint body directly with the pointer already present: it must adopt (the
        // under-lock re-check short-circuits) rather than author a second, distinct genesis.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        mint_local_account_in_tx(&tx, NOW + 1).expect("adopt");
        tx.commit().unwrap();
        assert_eq!(
            genesis_row_count(&conn),
            1,
            "the in-txn adopt short-circuit authors no second genesis",
        );
        assert_eq!(
            local_account(&conn, NOW + 2).expect("reuse"),
            incumbent,
            "the incumbent account survives",
        );
    }

    #[test]
    fn concurrent_first_callers_converge_on_one_account_and_one_genesis() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-account.db");
        let setup = Connection::open(&path).unwrap();
        schema::apply(&setup, &crate::test_hooks()).unwrap();
        drop(setup);

        // Two first-callers race from a fresh DB: each proposes a DISTINCT account (fresh nonce),
        // so convergence can only come from the pointer race — the loser must adopt the
        // winner.
        let barrier = Arc::new(Barrier::new(2));
        let spawn = || {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(Duration::from_secs(5)).unwrap();
                barrier.wait();
                local_account(&conn, NOW).expect("mint")
            })
        };
        let first = spawn();
        let second = spawn();
        let a = first.join().unwrap();
        let b = second.join().unwrap();
        assert_eq!(a, b, "both first-callers converge on one account");

        let conn = Connection::open(&path).unwrap();
        assert_eq!(genesis_row_count(&conn), 1, "the pointer race admits exactly one genesis");
        assert_eq!(pointer_row_count(&conn), 1, "exactly one pointer row");
    }
}
