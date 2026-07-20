//! The in-tx NON-genesis account-op author seam + the composed `/2`-ownership ensure (sync phase
//! C3.4b-ii, #676).
//!
//! Where [`super::bootstrap`] mints the ONE self-authorizing `AccountGenesis`, this module authors
//! the account's LATER control ops (`StreamOwn`, and — as future slices motivate them —
//! `DeviceAdd`, `StreamGrant`, …) from the control-chain tail, inside the caller's IMMEDIATE
//! transaction. It reuses the same account-layer seams the mint does
//! ([`super::storage::insert_candidate`] + [`super::storage::refold_in_tx`]) rather than the
//! self-transacting [`super::storage::account_ingest`], which cannot nest inside the caller's txn.
//!
//! The one seam exported upward is [`ensure_owned_stream_v2_in_tx`]: the idempotent
//! ensure-the-repo's-`/2`-stream-is-owned primitive #664 calls before authoring owner-bound `/3`
//! content. It is check-fact-first: it authors a `StreamOwn` only when the ownership FACT is
//! absent, and verifies the FACT (not the authored entry's status) afterwards — a duplicate/raced
//! `StreamOwn{same stream}` folds `Rejected(Ineffective)` even though ownership still holds (§10),
//! so trusting the entry status would spuriously fail. Under the caller's IMMEDIATE txn the fact
//! gate serializes racers, so a duplicate `StreamOwn` is never authored at all.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::bootstrap::{self, LocalAccountRef};
use super::envelope::{AccountEntryHeader, VerifiedAccountEntry, sign_account_entry};
use super::id::AccountId;
use super::ops::{self, AccountOp};
use super::storage::{self, CandidateInsert};
use super::{AuthorityQuery, fold};
use crate::identity::LocalDevice;
use crate::local_device;
use crate::op::DeviceFingerprint;
use crate::stream::{self, StreamId};

type EntryHash = [u8; 32];

/// Ensure the repo's `/2` owner stream is owned by the store's local account, authoring exactly one
/// `StreamOwn` if the ownership fact is not already present, and return the `/2` `stream_id`.
/// Neither opens nor commits the txn. Idempotent: a re-ensure (or a concurrent racer under the
/// IMMEDIATE gate) authors NO second `StreamOwn` and returns the same id.
///
/// Requires the store's local account to be minted already (see [`bootstrap::local_account`]); the
/// caller mints it before opening this txn — the mint self-transacts and cannot nest here, the same
/// contract the `/3` content seam holds.
pub fn ensure_owned_stream_v2_in_tx(
    tx: &Transaction<'_>,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<StreamId> {
    // The local account (author == owner of its `/2` streams) must already exist; resolve it and
    // its genesis entry hash (the founder incarnation a control op cites) WITHOUT minting.
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot ensure a /2 owned stream before the store's local account is minted (call \
         local_account first)",
    )?;
    let spec = stream::owner_stream_v2(repo_id, account_id);
    let stream_id = stream::derive_v2(&spec)?;

    // Check-fact-first. If the account already owns this stream, author NOTHING: a second
    // `StreamOwn{same stream}` folds `Rejected(Ineffective)`, so re-authoring would be wasted work
    // whose entry status could not be trusted to report success. Read the fact in the caller's
    // snapshot (`_in_snapshot`), NOT the conn-level `stream_owner_effective`, which opens its own
    // Deferred txn and would fail at BEGIN inside the caller's IMMEDIATE txn. Under IMMEDIATE this
    // read also serializes racers, so at most one `StreamOwn` is ever authored across processes.
    if let AuthorityQuery::Effective(_) =
        storage::stream_owner_effective_in_snapshot(tx, account_id, stream_id)?
    {
        return Ok(stream_id);
    }

    // Not yet owned: author one `StreamOwn` over the canonical `/2` spec and refold.
    let device = local_device(tx, now_ms)?;
    let op = AccountOp::StreamOwn {
        stream_id,
        stream_spec_bytes: stream::canonical_spec_v2_bytes(&spec)?,
    };
    author_account_op_in_tx(tx, &device, account_id, genesis_hash, &op, now_ms)?;

    // Verify the FACT, never the authored entry's status. Under a race two ensures could each reach
    // here and the loser's `StreamOwn` folds `Rejected(Ineffective)` — but ownership holds either
    // way, and that is what the caller needs. Require the ownership fact to resolve effective in
    // this same snapshot; a miss means an authority gap (not our stream, contested account) and
    // the whole caller mutation must roll back rather than report an unowned stream as owned.
    match storage::stream_owner_effective_in_snapshot(tx, account_id, stream_id)? {
        AuthorityQuery::Effective(_) => Ok(stream_id),
        other => anyhow::bail!(
            "StreamOwn authored but the /2 stream did not fold owned (fact {other:?}); refusing \
             to report a stream that is not owned by the local account",
        ),
    }
}

/// Derive the repo's owner-bound `/2` stream id under the store's local account, or `None` when no
/// local account is minted yet. PURE derivation — resolves the account pointer and hashes the spec,
/// opening NO nested transaction, so it is safe both in autocommit and inside an open IMMEDIATE txn
/// (pass a `&Transaction`, which derefs to `&Connection`). The live authoring seam's stream
/// resolver: a `None` means "no principal to author under yet", so the caller SKIPS authoring
/// rather than forcing a mint — the exact analog of an unstable scope.
pub fn owned_stream_v2_id(conn: &Connection, repo_id: &str) -> anyhow::Result<Option<StreamId>> {
    let Some(LocalAccountRef { account_id, .. }) = bootstrap::local_account_ref(conn)? else {
        return Ok(None);
    };
    Ok(Some(stream::derive_v2(&stream::owner_stream_v2(repo_id, account_id))?))
}

/// The repo's `/2` owner stream, but ONLY once it is fully ESTABLISHED: the local account is minted
/// AND its `StreamOwn` has folded `Effective` (the ownership fact is live), so an owner-authored
/// `/3` batch on it would accept. `None` when no account is minted OR ownership has not folded
/// effective yet — so a fresh repo whose anti-join is empty only because it has never published
/// ownership resolves `None` here and is NOT mistaken for "nothing to do" (the caller establishes
/// ownership rather than early-returning). AUTOCOMMIT-ONLY: [`storage::stream_owner_effective`]
/// opens its OWN Deferred transaction, so this MUST NOT be called inside an open transaction (use
/// [`owned_stream_v2_id`] there). The reconcile's fast-path probe.
pub fn established_owned_stream_v2(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<StreamId>> {
    let Some(LocalAccountRef { account_id, .. }) = bootstrap::local_account_ref(conn)? else {
        return Ok(None);
    };
    let stream_id = stream::derive_v2(&stream::owner_stream_v2(repo_id, account_id))?;
    match storage::stream_owner_effective(conn, account_id, stream_id)? {
        AuthorityQuery::Effective(_) => Ok(Some(stream_id)),
        AuthorityQuery::Unknown | AuthorityQuery::Invalid(_) => Ok(None),
    }
}

/// Author `op` as a NON-genesis account control op on the local account's control log (`log_id =
/// 0`) WITHIN the caller's transaction: chain it off the device's control-chain tail, insert it as
/// a candidate, and refold the account ONCE. Returns the authored entry hash. Neither opens nor
/// commits the txn, and — unlike the `/3` content seam — does NOT verify acceptance itself: the
/// caller owns the fold interpretation (a `StreamOwn` legitimately folds `Ineffective` on a
/// duplicate, which is not an authoring error). Reuses the account layer's own candidate seams
/// directly; it MUST NOT go through the self-transacting [`storage::account_ingest`].
fn author_account_op_in_tx(
    tx: &Transaction<'_>,
    device: &LocalDevice,
    account_id: AccountId,
    genesis_hash: EntryHash,
    op: &AccountOp,
    now_ms: i64,
) -> anyhow::Result<EntryHash> {
    let fingerprint = device.fingerprint();
    // Chain from the control-log tail. Post-genesis the tail is never empty (the genesis is seq 0);
    // an empty chain here means the caller skipped the mint, which is a programming error.
    let (tail_seq, tail_hash) = account_chain_tail(tx, account_id, fingerprint, fold::CONTROL_LOG)?
        .context(
            "cannot author a non-genesis account op on an empty control chain (mint the genesis \
             first)",
        )?;
    let seq = tail_seq
        .checked_add(1)
        .context("account control chain tail is at u64::MAX seq; cannot extend")?;

    // Cite our own CURRENT effective control-fold length as `auth_len`, read BEFORE authoring: the
    // fold parks an entry whose asserted `auth_len` runs ahead of the fold it lands in (§7), so
    // citing the count as-of now means our own entry never parks `auth_len_ahead` against our own
    // fold. Mirrors the `/3` content seam's freshness citation.
    let auth_len = storage::account_effective_count(tx, account_id)?;

    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: fingerprint,
        seq,
        // seq > 0 ⇒ prev_hash non-null (the header nullity rule); the device-chain predecessor.
        prev_hash: Some(tail_hash),
        // The account root every non-genesis control op cites as its parent (§6): the genesis hash,
        // NOT the device-chain tail (that is `prev_hash`'s job, and the two are distinct — the tail
        // only equals the genesis for the first post-genesis entry). The fold reads `parent_ref`
        // only to reject a malformed genesis (a non-null one), but every account entry pins it to
        // the genesis; naming the tail instead would diverge from that convention and a stricter
        // peer could reject it.
        parent_ref: Some(genesis_hash),
        entry_type: ops::entry_type_of(op),
        op_version: 1,
        crypto_suite: 0,
        auth_len,
        key_id: None,
        // The founder incarnation this op acts under (§"authority rule"): the account's own
        // genesis, whose incarnation id is its own entry hash.
        authority_ref: Some(genesis_hash),
    };
    let payload =
        ops::encode(op).map_err(|err| anyhow::anyhow!("encoding the account op failed: {err}"))?;
    let signed = sign_account_entry(device.secret(), &header, &payload)?;
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    match storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)? {
        CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {},
        CandidateInsert::AtCapacity(scope) => anyhow::bail!(
            "the account candidate store is at capacity ({scope:?}); cannot author the account op",
        ),
    }
    storage::refold_in_tx(tx, account_id, now_ms)?;
    Ok(verified.entry_hash)
}

/// One `(account, log_id, device)` chain's tail: its highest-`seq` entry as `(seq, entry_hash)`, or
/// `None` for an empty chain. The empty case is the CALLER's to interpret: on the CONTROL log a
/// non-genesis author treats it as a programming error (the genesis is always seq 0), while on the
/// SECRETS log (which has no genesis) it is the legitimate first-wrap case (seq 0, no predecessor).
/// `log_id`-parameterized because the secrets chain is `(account, device)`-scoped across ALL
/// streams (never per-stream), so C4.3a reads its dense seq from the shared `log = 1` tail via this
/// same reader. Unlike `/3` content, an `account_entries.seq` is a plain numeric INTEGER, so `ORDER
/// BY seq DESC` is a numeric comparison (NOT a fixed-width big-endian blob compare).
pub(super) fn account_chain_tail(
    tx: &Transaction<'_>,
    account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
    log_id: u8,
) -> anyhow::Result<Option<(u64, EntryHash)>> {
    let row: Option<(i64, Vec<u8>)> = tx
        .query_row(
            "SELECT seq, entry_hash FROM account_entries
             WHERE account_id = ?1 AND log_id = ?2 AND device_fingerprint = ?3
             ORDER BY seq DESC LIMIT 1",
            params![
                account_id.to_bytes().as_slice(),
                log_id,
                device_fingerprint.to_bytes().as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(seq, hash)| {
        let seq = u64::try_from(seq)
            .map_err(|_| anyhow::anyhow!("account chain tail seq is negative: {seq}"))?;
        let hash: EntryHash = hash
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("entry_hash is not 32 bytes"))?;
        Ok((seq, hash))
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use rag_rat_db::schema;
    use rusqlite::{Connection, TransactionBehavior};

    use super::*;
    use crate::op::{MemoryOp, NodeContent, NodeId};

    const NOW: i64 = 1_700_000_000_000;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    /// Count the account's `StreamOwn` candidate rows — the idempotency witness. Gated on
    /// `log_id == CONTROL_LOG` (S-f): a fresh-numbered secrets tag colliding with the 6 number must
    /// not inflate this witness.
    fn stream_own_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM account_entries WHERE entry_type = ?1 AND log_id = ?2",
            params![ops::entry_type::STREAM_OWN, crate::account::fold::CONTROL_LOG],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn owns(conn: &Connection, account: AccountId, stream: StreamId) -> bool {
        matches!(
            storage::stream_owner_effective(conn, account, stream).unwrap(),
            AuthorityQuery::Effective(_)
        )
    }

    /// Run the ensure in its own IMMEDIATE txn and commit — the shape a live caller uses.
    fn ensure_committed(conn: &Connection, repo_id: &str) -> StreamId {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let stream_id = ensure_owned_stream_v2_in_tx(&tx, repo_id, NOW).expect("ensure");
        tx.commit().unwrap();
        stream_id
    }

    fn node_create(id: &str, title: &str) -> MemoryOp {
        MemoryOp::NodeCreate {
            node_id: NodeId::from(id),
            content: NodeContent {
                kind: "Invariant".to_string(),
                title: title.to_string(),
                body: "body".to_string(),
                confidence: "high".to_string(),
                source: "agent".to_string(),
                tags: Vec::new(),
                payload: None,
            },
        }
    }

    #[test]
    fn ensure_authors_a_stream_own_that_folds_owned_and_returns_the_derived_id() {
        let conn = db();
        let account = bootstrap::local_account(&conn, NOW).expect("mint local account");

        let stream_id = ensure_committed(&conn, "repo-x");

        // The returned id is exactly the `/2` derivation for this (repo, account).
        let expected = stream::derive_v2(&stream::owner_stream_v2("repo-x", account)).unwrap();
        assert_eq!(stream_id, expected, "the returned id is the derive_v2 id");
        // The ownership fact resolves effective (the StreamOwn folded, the own_id is queryable).
        assert!(owns(&conn, account, stream_id), "the StreamOwn folds effective ownership");
        // Exactly one StreamOwn was authored.
        assert_eq!(stream_own_count(&conn), 1, "one ensure authors one StreamOwn");
    }

    #[test]
    fn re_ensure_is_idempotent_and_authors_no_second_stream_own() {
        let conn = db();
        bootstrap::local_account(&conn, NOW).expect("mint local account");

        let first = ensure_committed(&conn, "repo-x");
        let second = ensure_committed(&conn, "repo-x");

        assert_eq!(first, second, "a re-ensure returns the same stream id");
        assert_eq!(
            stream_own_count(&conn),
            1,
            "the check-fact-first gate authors no second StreamOwn on re-ensure",
        );
    }

    #[test]
    fn a_second_control_op_cites_the_genesis_as_parent_not_the_chain_tail() {
        // Regression: `parent_ref` names the account ROOT (genesis), while `prev_hash` names the
        // device-chain predecessor. For the FIRST post-genesis op the two coincide (the tail IS the
        // genesis), so a `parent_ref = tail` bug hides there. Force chain depth 2 — own a SECOND,
        // distinct stream — so the tail (StreamOwn-A) is no longer the genesis, then prove the new
        // op still cites the genesis as its parent and only `prev_hash` advances to the tail.
        let conn = db();
        bootstrap::local_account(&conn, NOW).expect("mint local account");
        ensure_committed(&conn, "repo-a"); // seq 1: tail is now the StreamOwn-A entry
        ensure_committed(&conn, "repo-b"); // seq 2: tail is StreamOwn-A, root is the genesis

        // Read the (seq, entry_hash, prev_hash, parent_ref) of the control chain in order.
        let mut stmt = conn
            .prepare(
                "SELECT seq, entry_hash, prev_hash, parent_ref FROM account_entries
                 WHERE log_id = 0 ORDER BY seq ASC",
            )
            .unwrap();
        // (seq, entry_hash, prev_hash, parent_ref) for one control-chain row.
        type ChainRow = (i64, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);
        let rows: Vec<ChainRow> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 3, "genesis + two StreamOwns");

        let (_, genesis_hash, genesis_prev, genesis_parent) = &rows[0];
        assert_eq!(*genesis_prev, None, "genesis has null prev_hash");
        assert_eq!(*genesis_parent, None, "genesis has null parent_ref");

        let (_, stream_own_a_hash, _, _) = &rows[1];
        let (_, _, b_prev, b_parent) = &rows[2];
        assert_eq!(
            b_parent.as_ref(),
            Some(genesis_hash),
            "the second control op cites the genesis as its parent_ref",
        );
        assert_eq!(
            b_prev.as_ref(),
            Some(stream_own_a_hash),
            "the second control op's prev_hash is the device-chain tail (StreamOwn-A), not the \
             genesis",
        );
        assert_ne!(
            b_parent, b_prev,
            "parent_ref (genesis root) and prev_hash (chain tail) diverge once the chain has \
             depth > 1",
        );
    }

    #[test]
    fn racing_ensures_converge_on_one_stream_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ensure-race.db");
        let setup = Connection::open(&path).unwrap();
        schema::apply(&setup, &crate::test_hooks()).unwrap();
        // Pre-mint the account (the mint self-transacts and cannot nest inside the ensure txn),
        // then race two ensures from separate connections.
        bootstrap::local_account(&setup, NOW).expect("pre-mint the local account");
        drop(setup);

        let barrier = Arc::new(Barrier::new(2));
        let spawn = || {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(Duration::from_secs(5)).unwrap();
                barrier.wait();
                let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
                let stream_id = ensure_owned_stream_v2_in_tx(&tx, "repo-x", NOW).expect("ensure");
                tx.commit().unwrap();
                stream_id
            })
        };
        let a = spawn();
        let b = spawn();
        let ida = a.join().unwrap();
        let idb = b.join().unwrap();
        assert_eq!(ida, idb, "both racers converge on one stream id");

        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            stream_own_count(&conn),
            1,
            "the IMMEDIATE check-fact-first gate admits exactly one StreamOwn under a race",
        );
    }

    #[test]
    fn ensure_before_mint_errors() {
        let conn = db();
        // No local account minted → the ensure cannot resolve the account and must error rather
        // than mint one (the mint self-transacts and cannot nest).
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let result = ensure_owned_stream_v2_in_tx(&tx, "repo-x", NOW);
        assert!(result.is_err(), "ensure requires a pre-minted local account");
    }

    #[test]
    fn genesis_then_stream_own_then_content_accepts_through_the_real_fold() {
        // The whole C3 chain end-to-end WITHOUT seeding any fact by hand: mint the genesis, ensure
        // the `/2` stream is owned (a real StreamOwn folds), then author owner-bound `/3` content
        // on that stream and prove it accepts because the ownership fact ensure published
        // is real.
        let conn = db();
        bootstrap::local_account(&conn, NOW).expect("mint local account");

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let stream_id = ensure_owned_stream_v2_in_tx(&tx, "repo-x", NOW).expect("ensure ownership");
        let hashes = super::super::content::author_content_batch_in_tx(
            &tx,
            stream_id,
            &[node_create("n1", "first")],
            NOW,
        )
        .expect("author /3 content on the freshly-owned stream");
        tx.commit().unwrap();

        assert_eq!(hashes.len(), 1, "one op authors one /3 entry");
        let status: String = conn
            .query_row(
                "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
                [hashes[0].as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "accepted",
            "genesis → StreamOwn → /3 content accepts through the real fold, no seeded facts",
        );
    }
}
