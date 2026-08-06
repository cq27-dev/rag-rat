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
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::bootstrap::{self, LocalAccountRef};
use super::envelope::{
    AccountEntryHeader, VerifiedAccountEntry, sign_account_entry, signed_entry_len,
};
use super::id::AccountId;
use super::limits::ACCOUNT_ENVELOPE_MAX_BYTES;
use super::ops::{self, AccountOp};
use super::storage::{self, CandidateInsert};
use super::{AuthorityQuery, fold};
use crate::device::{DeviceSecret, DeviceX25519Secret};
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
    ensure_owned_stream_v2_with_mode_in_tx(tx, repo_id, stream::AccessMode::Private, now_ms)
}

/// Like [`ensure_owned_stream_v2_in_tx`], but authors the `/2` owner stream under an explicit
/// [`AccessMode`](stream::AccessMode). A `PublicRead` stream has a DISTINCT identity from the
/// repo's private `/2` stream — the mode folds into `stream_id` — so publishing a public knowledge
/// base is a separate stream a peer may read anonymously, never a flag flipped on the private one.
/// Same check-fact-first idempotence and in-txn contract as the private ensure.
pub fn ensure_owned_stream_v2_with_mode_in_tx(
    tx: &Transaction<'_>,
    repo_id: &str,
    access_mode: stream::AccessMode,
    now_ms: i64,
) -> anyhow::Result<StreamId> {
    // The local account (author == owner of its `/2` streams) must already exist; resolve it and
    // its genesis entry hash (the founder incarnation a control op cites) WITHOUT minting.
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot ensure a /2 owned stream before the store's local account is minted (call \
         local_account first)",
    )?;
    let mut spec = stream::owner_stream_v2(repo_id, account_id);
    spec.access_mode = access_mode;
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
    owned_stream_v2_id_with_mode(conn, repo_id, stream::AccessMode::Private)
}

/// [`owned_stream_v2_id`] for a chosen [`AccessMode`]. A `PublicRead` `/2` stream has a DISTINCT id
/// from the `Private` one (the mode folds into `stream_id`), so a caller publishing a public
/// knowledge base MUST resolve, author, drain, reconcile, and catch-up with the SAME mode or the
/// live-write and mirror paths target different streams. The authoring mode is the caller's
/// persisted intent, not derivable from an empty account — hence a parameter, not an op-log read.
pub fn owned_stream_v2_id_with_mode(
    conn: &Connection,
    repo_id: &str,
    access_mode: stream::AccessMode,
) -> anyhow::Result<Option<StreamId>> {
    let Some(LocalAccountRef { account_id, .. }) = bootstrap::local_account_ref(conn)? else {
        return Ok(None);
    };
    let mut spec = stream::owner_stream_v2(repo_id, account_id);
    spec.access_mode = access_mode;
    Ok(Some(stream::derive_v2(&spec)?))
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
    established_owned_stream_v2_with_mode(conn, repo_id, stream::AccessMode::Private)
}

/// [`established_owned_stream_v2`] for a chosen [`AccessMode`] — see
/// [`owned_stream_v2_id_with_mode`] on why the mode is a parameter.
pub fn established_owned_stream_v2_with_mode(
    conn: &Connection,
    repo_id: &str,
    access_mode: stream::AccessMode,
) -> anyhow::Result<Option<StreamId>> {
    let Some(LocalAccountRef { account_id, .. }) = bootstrap::local_account_ref(conn)? else {
        return Ok(None);
    };
    let mut spec = stream::owner_stream_v2(repo_id, account_id);
    spec.access_mode = access_mode;
    let stream_id = stream::derive_v2(&spec)?;
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

/// A device being enrolled onto the roster: the account signing identity the joiner generated (its
/// ed25519 + x25519 PUBLIC keys) and an optional human label. The `DeviceFingerprint` is DERIVED
/// (`sha256(ed25519_pubkey)`), never supplied — matching the op's own canonicalization.
pub struct EnrollingDevice {
    pub ed25519_pubkey: [u8; 32],
    pub x25519_pubkey: [u8; 32],
    pub label: Option<String>,
}

/// The exact mandatory candidate cost of redeeming an enrollment invite: one `DeviceAdd` plus one
/// stream-key wrap per live key target across `streams`, measured from the real encoders at the
/// maximum header width (the `DeviceAdd` at its actual role/label via
/// [`device_add_envelope_bytes`], a one-recipient wrap via
/// [`super::secrets::single_recipient_wrap_envelope_bytes`]). Mint persists this as the invite's
/// candidate-capacity reservation; redemption releases the reservation and consumes exactly it.
pub fn enrollment_authoring_requirements(
    conn: &Connection,
    account_id: AccountId,
    streams: &[StreamId],
    role: ops::DeviceRole,
    label: Option<&str>,
) -> anyhow::Result<(u64, u64)> {
    let targets =
        super::secrets::recoverable_live_stream_key_target_count(conn, account_id, streams)?;
    let required_entries = 1u64.saturating_add(u64::try_from(targets)?);
    let wrap_bytes = super::secrets::single_recipient_wrap_envelope_bytes();
    let required_bytes = u64::try_from(
        device_add_envelope_bytes(role, label)?.saturating_add(wrap_bytes.saturating_mul(targets)),
    )?;
    Ok((required_entries, required_bytes))
}

/// Refuse when the grow-only account candidate store cannot fit the mandatory entries redeeming an
/// enrollment invite authors: the `DeviceAdd` and one stream-key wrap per live key target across
/// `streams`. Latent pre-verify promotion is best-effort maintenance after enrollment commits and
/// never consumes this reservation. Headroom is net of every outstanding invite's reservation at
/// `now_ms`, so two invites cannot be minted against the same capacity. Read in the caller's
/// snapshot; the mint transaction re-reads it under the writer lock.
pub fn enrollment_authoring_fits(
    conn: &Connection,
    account_id: AccountId,
    streams: &[StreamId],
    role: ops::DeviceRole,
    label: Option<&str>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let (required_entries, required_bytes) =
        enrollment_authoring_requirements(conn, account_id, streams, role, label)?;
    let required_entries = i64::try_from(required_entries)?;
    let required_bytes = i64::try_from(required_bytes)?;
    let headroom = storage::candidate_capacity_headroom(conn, account_id, now_ms)?;
    anyhow::ensure!(
        headroom.account_entries_remaining >= required_entries
            && headroom.global_entries_remaining >= required_entries
            && headroom.account_bytes_remaining >= required_bytes
            && headroom.global_bytes_remaining >= required_bytes,
        "the account candidate store cannot fit this enrollment's DeviceAdd and stream-key wraps; \
         an invite minted now would be unredeemable (candidate capacity is grow-only)",
    );
    Ok(())
}

/// The exact signed-envelope byte length of the `DeviceAdd` [`author_device_add_in_tx`] authors
/// for `role`/`label`, at the maximum header width. Shared by [`validate_device_add_label`]
/// (which signs the shape) and [`enrollment_authoring_fits`] (which charges it).
fn device_add_envelope(
    role: ops::DeviceRole,
    label: Option<&str>,
) -> (AccountEntryHeader, Vec<u8>) {
    let author = DeviceSecret::from_seed(&[0x41; 32]);
    let joiner = DeviceSecret::from_seed(&[0x42; 32]);
    let joiner_x25519 = DeviceX25519Secret::from_seed(&[0x43; 32]);
    let op = AccountOp::DeviceAdd {
        device_fingerprint: joiner.public().fingerprint(),
        ed25519_pubkey: joiner.public().to_bytes(),
        x25519_pubkey: joiner_x25519.public().to_bytes(),
        role,
        label: label.map(str::to_owned),
    };
    let payload = ops::encode(&op).expect("a locally-built DeviceAdd encodes");
    let header = AccountEntryHeader {
        account_id: AccountId::from_bytes([u8::MAX; 32]),
        log_id: 0,
        device_fingerprint: author.public().fingerprint(),
        seq: u64::MAX,
        prev_hash: Some([u8::MAX; 32]),
        parent_ref: Some([u8::MAX; 32]),
        entry_type: ops::entry_type_of(&op),
        op_version: 1,
        crypto_suite: 0,
        auth_len: u64::MAX,
        key_id: None,
        authority_ref: Some([u8::MAX; 32]),
    };
    (header, payload)
}

pub(in crate::account) fn device_add_envelope_bytes(
    role: ops::DeviceRole,
    label: Option<&str>,
) -> anyhow::Result<usize> {
    if label.is_some_and(|label| label.len() > ACCOUNT_ENVELOPE_MAX_BYTES) {
        anyhow::bail!(
            "device label exceeds the {ACCOUNT_ENVELOPE_MAX_BYTES}-byte account envelope limit"
        );
    }
    let (header, payload) = device_add_envelope(role, label);
    Ok(signed_entry_len(&header, &payload))
}

/// Validate that `label` can fit in every authorable `DeviceAdd` signed envelope.
///
/// This is the shared preflight for invite minting and actual authoring. It deliberately builds
/// the largest possible header shape used by [`author_device_add_in_tx`] and routes the candidate
/// through the real op encoder + account-envelope signer, so transport code never duplicates the
/// account wire's size arithmetic.
pub fn validate_device_add_label(label: Option<&str>) -> anyhow::Result<()> {
    if label.is_some_and(|label| label.len() > ACCOUNT_ENVELOPE_MAX_BYTES) {
        anyhow::bail!(
            "device label exceeds the {ACCOUNT_ENVELOPE_MAX_BYTES}-byte account envelope limit"
        );
    }
    let (header, payload) = device_add_envelope(ops::DeviceRole::Owner, label);
    sign_account_entry(&DeviceSecret::from_seed(&[0x41; 32]), &header, &payload)
        .context("device label cannot fit in an authorable DeviceAdd")?;
    Ok(())
}

/// Author a `DeviceAdd` enrolling `joiner` onto the local account's roster at `role`, signed by the
/// local owner device, and refold WITHIN the caller's transaction. Returns the authored entry hash
/// (which, for `role == Owner`, IS the added device's owner-incarnation id). Neither opens nor
/// commits the txn — the pairing/enrollment seam a durable core wrapper drives.
///
/// FOUNDER-owner only for now: like [`author_account_op_in_tx`] it cites the account genesis as
/// `authority_ref`, so the LOCAL device must be the account founder. A `DeviceAdd` authored by a
/// non-founder (or any non-owner) folds `Rejected`; this verifies the joiner became
/// roster-effective and errors otherwise, rather than reporting a rejected enrollment as success.
/// Enrolling from a PROMOTED owner (citing that owner's own incarnation, not genesis) is a
/// follow-up.
pub fn author_device_add_in_tx(
    tx: &Transaction<'_>,
    joiner: EnrollingDevice,
    role: ops::DeviceRole,
    now_ms: i64,
) -> anyhow::Result<EntryHash> {
    author_device_add_with_promotion_in_tx(tx, joiner, role, now_ms, DeviceAddPromotion::Retry)
}

/// Enrollment-specific DeviceAdd authoring. Mandatory wraps and the durable receipt are committed
/// before latent pre-verify work is retried, so opaque queue state cannot consume their capacity.
pub fn author_enrollment_device_add_in_tx(
    tx: &Transaction<'_>,
    joiner: EnrollingDevice,
    role: ops::DeviceRole,
    now_ms: i64,
) -> anyhow::Result<EntryHash> {
    author_device_add_with_promotion_in_tx(tx, joiner, role, now_ms, DeviceAddPromotion::Defer)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DeviceAddPromotion {
    Retry,
    Defer,
}

fn author_device_add_with_promotion_in_tx(
    tx: &Transaction<'_>,
    joiner: EnrollingDevice,
    role: ops::DeviceRole,
    now_ms: i64,
    promotion: DeviceAddPromotion,
) -> anyhow::Result<EntryHash> {
    validate_device_add_label(joiner.label.as_deref())?;
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot enroll a device before the store's local account is minted (call local_account \
         first)",
    )?;
    let device = local_device(tx, now_ms)?;
    // The fingerprint is derived, not trusted from the caller — the op's canonicalization derives
    // it the same way, so this keeps the roster checks below honest.
    let fingerprint = DeviceFingerprint::from_bytes(crate::cbor::sha256(&joiner.ed25519_pubkey));

    // Enrollment is for a NOT-yet-roster device. If the fingerprint is ALREADY roster-effective, a
    // fresh `DeviceAdd` folds `DuplicateAdd` (rejected) while the post-author presence check below
    // would still pass on the OLD enrollment — so without this pre-check we'd return a rejected
    // entry's hash as success (and for `role == Owner` a hash that is NOT a live owner-incarnation
    // id) and silently keep whatever role the device already held. Reject up front; re-enrolling or
    // changing an existing device's role is a separate operation. (Read in the caller's snapshot.)
    if storage::list_effective_roster_fingerprints(tx, account_id)?.contains(&fingerprint) {
        anyhow::bail!(
            "the device is already enrolled on this account's roster; enrollment adds a \
             not-yet-roster device (changing an existing device's role is a separate operation)",
        );
    }

    let op = AccountOp::DeviceAdd {
        device_fingerprint: fingerprint,
        ed25519_pubkey: joiner.ed25519_pubkey,
        x25519_pubkey: joiner.x25519_pubkey,
        role,
        label: joiner.label,
    };
    let entry_hash = author_account_op_in_tx(tx, &device, account_id, genesis_hash, &op, now_ms)?;
    if promotion == DeviceAddPromotion::Retry {
        storage::promote_after_local_device_add_in_tx(tx, account_id, now_ms)?;
    }

    // Verify the FACT, and the SPECIFIC fact — the joiner's effective roster row is THIS DeviceAdd
    // (`roster_ref == entry_hash`) at the REQUESTED role — never the authored entry's status. Bare
    // presence is not enough: `author_account_op_in_tx` refolds the WHOLE account, so a
    // concurrently-ingested parked sibling `DeviceAdd` for the same fingerprint (authored by
    // another owner device, parked `auth_len_ahead` until our count advanced) can unpark and
    // win the fold while OUR entry folds `Rejected(DuplicateAdd)`. Presence alone would then
    // pass on the sibling's row and we'd return our rejected hash at the sibling's role.
    // Asserting `roster_ref` + role ties the result to our entry — and a `DeviceAdd` from a
    // device without effective owner authority folds `Rejected` (leaving no effective row of
    // ours), so this still errors for a non-owner.
    match storage::effective_roster_entry_in_snapshot(tx, account_id, fingerprint)? {
        Some((roster_ref, effective_role))
            if roster_ref == entry_hash && effective_role == role => {},
        _ => anyhow::bail!(
            "the DeviceAdd did not become the joiner's effective roster entry at the requested \
             role — the local device lacks effective owner authority to enroll (founder-owner \
             enrollment only for now), the device was previously removed, or a concurrent \
             enrollment won the fold",
        ),
    }
    Ok(entry_hash)
}

/// Retry pre-verify rows unlocked by a committed enrollment in a separate maintenance transaction.
pub fn retry_enrollment_pre_verify(
    conn: &Connection,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    storage::promote_after_local_device_add_in_tx(&tx, account_id, now_ms)?;
    tx.commit()?;
    Ok(())
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
    fn locally_authored_device_add_promotes_rows_signed_by_the_joiner() {
        let conn = db();
        let account = bootstrap::local_account(&conn, NOW).expect("mint local account");
        let joiner = DeviceSecret::from_seed(&[0x71; 32]);
        let joiner_x = DeviceX25519Secret::from_seed(&[0x72; 32]);
        let payload = ops::encode(&AccountOp::AccountGenesis {
            ed25519_pubkey: joiner.public().to_bytes(),
            x25519_pubkey: joiner_x.public().to_bytes(),
            nonce16: [8; 16],
            created_at_ms: NOW as u64,
            label: None,
        })
        .unwrap();
        let header = AccountEntryHeader {
            account_id: account,
            log_id: fold::CONTROL_LOG,
            device_fingerprint: joiner.public().fingerprint(),
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: ops::entry_type::ACCOUNT_GENESIS,
            op_version: fold::SUPPORTED_OP_VERSION + 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let parked = sign_account_entry(&joiner, &header, &payload).unwrap();
        assert_eq!(
            storage::account_ingest(&conn, &parked.signed_bytes, NOW).unwrap(),
            storage::IngestOutcome::PreVerify,
        );

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: joiner.public().to_bytes(),
                x25519_pubkey: joiner_x.public().to_bytes(),
                label: None,
            },
            ops::DeviceRole::ReadOnly,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();

        let parked_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_pre_verify WHERE signed_hash = ?1",
                [crate::cbor::sha256(&parked.signed_bytes).as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let candidate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_entries WHERE entry_hash = ?1",
                [parked.entry_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parked_count, 0, "the newly resolvable queue row is drained");
        assert_eq!(candidate_count, 1, "the joiner-signed row is promoted to the candidate DAG");
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
