//! The store's persisted local account — its ONE self-authorizing `AccountGenesis`, minted once and
//! reused (sync phase C3.4a, #662).
//!
//! Where [`crate::oplog::local_device`] is the machine's signing identity, the local account is the
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
use super::envelope::{AccountEntryHeader, VerifiedAccountEntry, sign_account_entry};
use super::id::account_id_from_genesis_payload;
use super::ops::{self, AccountOp};
use super::storage::{self, CandidateInsert};
use crate::oplog::local_device;

/// Return this store's persisted local account, minting its self-authorizing genesis on the first
/// call and returning the SAME account stably thereafter. `now_ms` stamps a freshly-minted genesis
/// and its pointer row (injected, matching the op-log store convention); it is ignored once an
/// account exists.
///
/// The account_id is the §4 content-commitment of the genesis payload, resolved through the
/// pointer: idempotent, and a concurrent first-open converges on one account (the loser of the
/// pointer race adopts the winner's genesis — see the module header).
pub(crate) fn local_account(conn: &Connection, now_ms: i64) -> anyhow::Result<AccountId> {
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
    let statuses = storage::refold_in_tx(tx, account_id)?;
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

/// Resolve this store's local account from the pointer, or `None` if none is minted yet. The
/// pointer is a content address; the account_id is recovered by looking the genesis up in the
/// candidate DAG (§4 commits the account_id inside the genesis payload, so this is the one true
/// id).
fn read_local_account(conn: &Connection) -> anyhow::Result<Option<AccountId>> {
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
struct AuthoredDurability<'a> {
    conn: &'a Connection,
}

impl<'a> AuthoredDurability<'a> {
    fn begin(conn: &'a Connection) -> anyhow::Result<Self> {
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

    use super::*;
    use crate::index::schema;

    const NOW: i64 = 1_700_000_000_000;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn genesis_row_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM account_entries WHERE entry_type = ?1",
            params![ops::entry_type::ACCOUNT_GENESIS],
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
        schema::apply(&setup).unwrap();
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
