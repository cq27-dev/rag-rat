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
use super::control_policy::{AccountControlPolicy, UnsupportedAccountControlVersion};
use super::control_v2::{ops as v2_ops, views};
use super::envelope::{
    AccountEntryHeader, VerifiedAccountEntry, sign_account_entry, signed_entry_len,
};
use super::id::{AccountEntryHash, AccountId, GrantId, OwnerId};
use super::limits::ACCOUNT_ENVELOPE_MAX_BYTES;
use super::ops::{self, AccountOp};
use super::storage::{self, CandidateInsert};
use super::{AuthorityQuery, fold};
use crate::device::{DeviceSecret, DeviceX25519Secret};
use crate::identity::LocalDevice;
use crate::local_device;
use crate::op::DeviceFingerprint;
use crate::stream::{self, StreamId};

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
    // its genesis entry hash (the account root every control op cites as `parent_ref`) WITHOUT
    // minting.
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot ensure a /2 owned stream before the store's local account is minted (call \
         local_account first)",
    )?;
    // An ensure is AUTHORING intent, so it refuses under every pin — including one this binary
    // folds. The check-fact-first read below answers from the rebuilt projection and would report
    // an already-owned stream as ensured; the authoring branch past it is gated, but handing a
    // caller success for a mutation this account cannot perform is the wrong answer.
    super::control_policy::require_supported_account_control(tx, account_id)?;
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
    author_account_op_in_tx(tx, &device, account_id, genesis_hash, &op, None, now_ms)?;

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

/// The `/2` owner stream id for `repo_id` owned by an EXPLICIT `owner_account_id` at `access_mode`
/// — a pure derivation with no local-account read. A granted contributor (#1164) uses this to
/// target the OWNER's stream; every other `/2` resolver binds the LOCAL account, which is wrong for
/// a contributor. The contributor learns `owner_account_id` from its configured contribution owner.
pub fn owner_stream_v2_id_for_account(
    repo_id: &str,
    owner_account_id: AccountId,
    access_mode: stream::AccessMode,
) -> anyhow::Result<StreamId> {
    let mut spec = stream::owner_stream_v2(repo_id, owner_account_id);
    spec.access_mode = access_mode;
    stream::derive_v2(&spec)
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
    genesis_hash: AccountEntryHash,
    op: &AccountOp,
    pre_cut_view: Option<[u8; 32]>,
    now_ms: i64,
) -> anyhow::Result<AccountEntryHash> {
    // The account's control version decides what this seam SIGNS, not merely whether it may sign.
    // A pinned account that still signed v1 bytes would author an entry its own fold can never make
    // effective, while unpinned peers folded the same bytes as a real operation — so the version is
    // chosen here, at the one seam every control op passes through, rather than by a relaxed gate.
    let pin = match super::control_policy::account_control_policy(tx, account_id)? {
        AccountControlPolicy::LegacyV1 => None,
        AccountControlPolicy::ControlV2(pin) => Some(pin),
        AccountControlPolicy::UnsupportedVersion(pin) =>
            return Err(UnsupportedAccountControlVersion { pin }.into()),
    };
    let fingerprint = device.fingerprint();
    // The owner incarnation this device acts under: its OWN, which is the genesis only for the
    // founder. Citing the genesis from any other owner — or from a founder that was demoted and
    // re-promoted, whose genesis incarnation is closed — signs an entry its own fold rejects.
    let incarnation = storage::effective_owner_incarnation_for_device(tx, account_id, fingerprint)?
        .context("this device is not an owner of the account, so it cannot author a control op")?;
    // Chain from this device's OWN control-log tail. An empty chain is the origin slot for every
    // device but the founder (whose seq 0 is the genesis), so a second owner's first control op
    // starts its own chain at seq 0 with no predecessor — exactly as the secrets log does.
    let tail = account_chain_tail(tx, account_id, fingerprint, fold::CONTROL_LOG)?;
    if pin.is_some() {
        // Under a pin the held tail must also be the accepted tail. Branch selection builds each
        // device's accepted chain only from EFFECTIVE entries, contiguously from seq 0, so an entry
        // chained onto an unaccepted tail is forked; and chaining from the accepted tail instead
        // would share a slot with an entry whose verdict can still change, deciding between two of
        // our own entries by hash. Both chains being empty agrees.
        let accepted = account_accepted_chain_tail(tx, account_id, fingerprint, fold::CONTROL_LOG)?;
        anyhow::ensure!(
            accepted == tail,
            "this device's control chain ends in an entry the checkpoint-accepted chain did not \
             accept, so a control v2 entry chained onto it could never take effect; this device \
             cannot author control ops for this account, and another owner can remove it",
        );
    }
    let (seq, prev_hash) = match tail {
        Some((tail_seq, tail_hash)) => (
            tail_seq
                .checked_add(1)
                .context("account control chain tail is at u64::MAX seq; cannot extend")?,
            Some(tail_hash),
        ),
        None => (0, None),
    };

    // Cite our own CURRENT effective control-fold length as `auth_len`, read BEFORE authoring: the
    // fold parks an entry whose asserted `auth_len` runs ahead of the fold it lands in (§7), so
    // citing the count as-of now means our own entry never parks `auth_len_ahead` against our own
    // fold — a revoking cut included, since the fold credits it the entries it condemns. Mirrors
    // the `/3` content seam's freshness citation.
    let auth_len = storage::account_effective_count(tx, account_id)?;

    let (op_version, payload) = match &pin {
        None => {
            // v1 has no notion of a nominated view, so a caller that built one and reached here
            // would be committing to evidence these bytes never name. Refuse rather than sign a
            // payload that silently drops it.
            anyhow::ensure!(
                pre_cut_view.is_none(),
                "a pre-cut view was supplied for an account that is not pinned; v1 control bytes \
                 carry no view and would silently discard it",
            );
            (
                1,
                ops::encode(op)
                    .map_err(|err| anyhow::anyhow!("encoding the account op failed: {err}"))?,
            )
        },
        Some(pin) => (
            v2_ops::CONTROL_VERSION,
            v2_ops::ControlOp {
                checkpoint: pin.checkpoint_digest,
                // `ControlOp::encode` requires a view for exactly `DeviceRemove` and `OwnerDemote`
                // and refuses it for everything else, so a caller that supplies the wrong one fails
                // loudly here rather than authoring a revocation that credits nothing.
                //
                // `StreamRevoke` is revocation-SHAPED but deliberately not in the encoder's set:
                // its cuts travel in the op, not in a nominated view. It never arrives pinned
                // regardless — the revoke seam gates itself, because under a pin its cut plan would
                // come back empty.
                pre_cut_view,
                op: op.clone(),
            }
            .encode()?,
        ),
    };

    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: fingerprint,
        seq,
        // Null exactly at seq 0 (the header nullity rule); otherwise the device-chain predecessor.
        prev_hash,
        // The account root every non-genesis control op cites as its parent (§6): the genesis hash,
        // NOT the device-chain tail (that is `prev_hash`'s job, and the two are distinct — the tail
        // only equals the genesis for the first post-genesis entry). The fold reads `parent_ref`
        // only to reject a malformed genesis (a non-null one), but every account entry pins it to
        // the genesis; naming the tail instead would diverge from that convention and a stricter
        // peer could reject it.
        parent_ref: Some(genesis_hash),
        entry_type: ops::entry_type_of(op),
        op_version,
        crypto_suite: 0,
        auth_len,
        key_id: None,
        authority_ref: Some(incarnation),
    };
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
/// never consumes this reservation. Headroom is net of every invite reservation outstanding NOW —
/// judged against the wall clock, since an invite TTL is wall-clock (#1362) — so two invites cannot
/// be minted against the same capacity. Read in the caller's snapshot; the mint transaction
/// re-reads it under the writer lock.
pub fn enrollment_authoring_fits(
    conn: &Connection,
    account_id: AccountId,
    streams: &[StreamId],
    role: ops::DeviceRole,
    label: Option<&str>,
) -> anyhow::Result<()> {
    let (required_entries, required_bytes) =
        enrollment_authoring_requirements(conn, account_id, streams, role, label)?;
    let required_entries = i64::try_from(required_entries)?;
    let required_bytes = i64::try_from(required_bytes)?;
    let headroom = storage::candidate_capacity_headroom(conn, account_id)?;
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
        prev_hash: Some(AccountEntryHash::from_bytes([u8::MAX; 32])),
        parent_ref: Some(AccountEntryHash::from_bytes([u8::MAX; 32])),
        entry_type: ops::entry_type_of(&op),
        op_version: 1,
        crypto_suite: 0,
        auth_len: u64::MAX,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes([u8::MAX; 32])),
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
/// Any owner may author it: like every control op it cites the local device's own open owner
/// incarnation as `authority_ref`. A `DeviceAdd` from a non-owner still folds `Rejected`, so this
/// verifies the joiner became roster-effective and errors otherwise, rather than reporting a
/// rejected enrollment as success.
pub fn author_device_add_in_tx(
    tx: &Transaction<'_>,
    joiner: EnrollingDevice,
    role: ops::DeviceRole,
    now_ms: i64,
) -> anyhow::Result<AccountEntryHash> {
    author_device_add_with_promotion_in_tx(tx, joiner, role, now_ms, DeviceAddPromotion::Retry)
}

/// Enrollment-specific DeviceAdd authoring. Mandatory wraps and the durable receipt are committed
/// before latent pre-verify work is retried, so opaque queue state cannot consume their capacity.
///
/// Any owner may author it: the joiner accepts a DeviceAdd from whichever owner signed it, because
/// its fold decides who was entitled. Everything that could refuse it runs before redemption spends
/// the nonce — the shared control seam refuses a non-owner before signing, and the roster fact
/// check below refuses a DeviceAdd its own fold does not make effective.
pub fn author_enrollment_device_add_in_tx(
    tx: &Transaction<'_>,
    joiner: EnrollingDevice,
    role: ops::DeviceRole,
    now_ms: i64,
) -> anyhow::Result<AccountEntryHash> {
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
) -> anyhow::Result<AccountEntryHash> {
    validate_device_add_label(joiner.label.as_deref())?;
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot enroll a device before the store's local account is minted (call local_account \
         first)",
    )?;
    // Enrollment states its own control gate instead of inheriting one. The shared op seam now
    // SIGNS under a pin rather than refusing, so a path that must stay v1-only has to say so here;
    // opening enrollment under a pin needs the invite ticket to carry the checkpoint digest, which
    // is a separate change.
    super::control_policy::require_supported_account_control(tx, account_id)?;
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
    let entry_hash =
        author_account_op_in_tx(tx, &device, account_id, genesis_hash, &op, None, now_ms)?;
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
            if roster_ref == entry_hash.into() && effective_role == role => {},
        _ => anyhow::bail!(
            "the DeviceAdd did not become the joiner's effective roster entry at the requested \
             role — the local device lacks effective owner authority to enroll, the device was \
             previously removed, or a concurrent enrollment won the fold",
        ),
    }
    Ok(entry_hash)
}

/// Author a `DeviceRemove` closing `subject`'s roster seat on the local (owner) account's control
/// log, then VERIFY THE FACT — the seat is closed — never the authored entry's status.
///
/// Owner-only: a `DeviceRemove` from a device without effective owner authority folds `Rejected`,
/// leaving the seat open, so the fact check errors. The subject must be roster-effective NOW, since
/// removing a device that was never enrolled folds `Rejected(Ineffective)` rather than tombstoning
/// a fingerprint no enrollment ever added.
///
/// Under a control-v2 pin this authors the cut's detached view manifest FIRST: `ControlOp::encode`
/// refuses a revocation naming no view, and the executor applies the cut only once the manifest is
/// a stored row it can read. The view nominates nothing, and that is deliberate — nomination widens
/// a cut's freshness CREDIT and never decides who is condemned, because the register sweep runs
/// over every applied v2 operation regardless. An empty view is conservative, not a gap.
///
/// Does NOT rotate stream keys. Rotation on removal is lazy and fires from the seal path when a
/// removed device still holds the current key: local policy, deliberately not a chained authority
/// act. `reason` is carried verbatim for operators and peers; nothing in the fold reads it.
pub fn author_device_remove_in_tx(
    tx: &Transaction<'_>,
    subject: DeviceFingerprint,
    reason: &str,
    now_ms: i64,
) -> anyhow::Result<AccountEntryHash> {
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot author a device removal before the store's local account is minted (call \
         local_account first)",
    )?;
    let device = local_device(tx, now_ms)?;
    anyhow::ensure!(
        storage::effective_owner_incarnation_for_device(tx, account_id, device.fingerprint())?
            .is_some(),
        "the local device holds no effective owner incarnation on this account, so it cannot \
         remove a device",
    );
    // A self-removal is structurally dead, not merely unwise: the op is authored at this device's
    // own chain tail + 1, so it sits BEYOND any watermark it could name on that chain and its own
    // register condemns it. The fold pins that as spec. Refusing here names the real reason —
    // without it the post-check fires instead and blames missing owner authority, which is false.
    anyhow::ensure!(
        subject != device.fingerprint(),
        "a device cannot remove itself: the removal would sit beyond the watermark it names on \
         its own chain and self-condemn. Another owner device has to author it",
    );
    // Load-bearing, not defensive: the post-check below reads `None` for a device that was never
    // enrolled just as it does for one this removal closed, so WITHOUT this the seam reports
    // success for an entry that folds `Rejected(Ineffective)` — tombstoning a fingerprint no
    // enrollment added, which permanently bars a legitimate future `DeviceAdd` for it.
    anyhow::ensure!(
        storage::effective_roster_entry_in_snapshot(tx, account_id, subject)?.is_some(),
        "that device is not roster-effective on this account; removing one that was never \
         enrolled folds ineffective and would tombstone a fingerprint no enrollment added",
    );

    // Bound the subject's own chains at what THIS store accepted. `Cut::Empty` is NOT the safe
    // default: it means nothing on the chain is valid, retroactively invalidating entries the
    // account's own history may already rest on. It is correct only where there is nothing to keep.
    let cut_at_accepted_tail = |log: u8| -> anyhow::Result<super::cut::Cut> {
        let accepted = account_accepted_chain_tail(tx, account_id, subject, log)?;
        // A cut names the accepted tail, so entries above it are condemned. That is only a hazard
        // where peers may hold work this store has not DECIDED on yet — and `accepted` is not the
        // complement of "undecided". A `Rejected`, `Condemned` or `Forked` entry is decided and can
        // never be accepted, so refusing on any unaccepted row would make a device whose chain tail
        // merely lost a race permanently unremovable: candidate storage is grow-only and the
        // verdict is deterministic, so nothing could ever clear it. A second device racing
        // `ensure_owned_stream_v2_in_tx` produces exactly that shape, and it is not an error.
        //
        // Only a PARKED entry can still become effective later, so only a parked row above the
        // accepted tail means a peer may already hold what this cut would condemn.
        //
        // `retained_unfolded` is deliberately excluded, and the reason DIFFERS BY LOG — this
        // closure runs for both, so do not carry the control-log argument across.
        //
        // On the control log it costs nothing: that tag set is closed, so a retained entry
        // truncates its author's accepted chain on every binary, and nothing above it is accepted
        // anywhere.
        //
        // The secrets log is the deliberate opposite. A non-evaluable log-1 entry is slot-eligible
        // and PREFIX-TRANSPARENT, precisely so a newer binary can accept it — so excluding it here
        // is a knowing OVER-revocation: this cut may condemn an entry a newer peer accepted, and
        // `Cut::Empty` (a subject that has accepted nothing) is the maximal form of that.
        //
        // It is chosen anyway. The alternative hands the SUBJECT of a cut a veto over its own
        // removal: plant one entry at an unknown `op_version` and become permanently unremovable.
        // An availability failure on the one operation aimed at a device outweighs a convergence
        // cost on that device's forward-compatibility entries.
        let parked_above: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM account_entries e
                   JOIN account_entry_status s ON s.entry_hash = e.entry_hash
                  WHERE e.account_id = ?1 AND e.log_id = ?2 AND e.device_fingerprint = ?3
                    AND e.seq > ?4 AND s.status = ?5)",
            params![
                account_id.to_bytes().as_slice(),
                log,
                subject.to_bytes().as_slice(),
                accepted.map_or(-1i64, |(seq, _)| seq as i64),
                fold::EntryStatus::Parked.as_db_str(),
            ],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            !parked_above,
            "this store holds undecided entries on the subject's log-{log} chain above the last \
             one it accepted, so a cut naming that tail could condemn work its peers have already \
             accepted; catch this store up before removing that device",
        );
        Ok(match accepted {
            Some((seq, hash)) => super::cut::Cut::At { seq, hash },
            None => super::cut::Cut::Empty,
        })
    };
    let op = AccountOp::DeviceRemove {
        device_fingerprint: subject,
        control_cut: cut_at_accepted_tail(fold::CONTROL_LOG)?,
        secrets_cut: cut_at_accepted_tail(fold::SECRETS_LOG)?,
        // An absent per-stream boundary answers `Closed` against a CLOSED roster fact, which is
        // exactly what removing the whole device means. Naming cuts here would NARROW that to a
        // per-stream prefix, not widen it.
        content_cuts: Vec::new(),
        reason: reason.to_string(),
    };

    // Build the view but author NOTHING yet. Its digest is a pure function of its contents, so the
    // cut can name it before it is stored — which is the order this has to happen in (below).
    let view = match super::control_policy::account_control_policy(tx, account_id)? {
        AccountControlPolicy::LegacyV1 => None,
        AccountControlPolicy::ControlV2(pin) =>
            Some(views::ViewManifest { checkpoint: pin.checkpoint_digest, entries: Vec::new() }),
        AccountControlPolicy::UnsupportedVersion(pin) =>
            return Err(UnsupportedAccountControlVersion { pin }.into()),
    };
    // The cut names its evidence by the digest of the manifest PAYLOAD — never by the annex entry
    // hash that authoring returns. They are different 32-byte values, and naming the wrong one
    // parks the cut forever waiting on a manifest that will never be found.
    let pre_cut_view = view.as_ref().map(views::ViewManifest::digest).transpose()?;

    let entry_hash =
        author_account_op_in_tx(tx, &device, account_id, genesis_hash, &op, pre_cut_view, now_ms)?;

    // The manifest goes SECOND, and the order is load-bearing. `insert_candidate` grants a view
    // manifest the raised ceiling only when some STORED cut already cites it, and that citation is
    // recorded when the control row lands. Authoring the manifest first charges it the ordinary
    // per-account budget, so an account whose ordinary budget is full could never author a
    // revocation at all — the exact starvation the reserve exists to prevent, reached by the one
    // party whose revocation must always be authorable. Until the manifest is stored the cut simply
    // parks for want of it, and storing it here is what applies it.
    //
    // Signed by the CUT'S OWN device: an unknown signer parks in the pre-verify queue, which evicts
    // oldest-first, so the cut's evidence could be dropped out from under it.
    if let Some(view) = view {
        super::annex::author::author_view_manifest_in_tx(tx, &device, account_id, &view, now_ms)?;
    }

    // The FACT, not the entry status: `None` means no OPEN roster row survives for the subject.
    // That direction is robust where its complement is not — one fingerprint can hold several open
    // roster rows, so asserting presence would be nondeterministic, while absence is unambiguous.
    //
    // It is only a SUCCESS check because the precondition above established the subject held a seat
    // to begin with. On its own `None` is satisfied by a device that was never enrolled, so the two
    // checks have to be read as a pair.
    anyhow::ensure!(
        storage::effective_roster_entry_in_snapshot(tx, account_id, subject)?.is_none(),
        "the DeviceRemove did not close the subject's roster seat — the local device lacks \
         effective owner authority, or a concurrent operation won the fold",
    );
    Ok(entry_hash)
}

/// Author a `StreamGrant` granting `grantee_account_id` `role` on `stream_id`, on the local
/// (owner) account's control log, then VERIFY THE FACT — the grant became effective at the
/// requested role for that subject — not the entry status. Returns the `grant_id` (the grant
/// entry's hash).
///
/// Owner-only by construction: a `StreamGrant` from a device without effective owner authority
/// folds `Rejected`, leaving no effective grant, so the fact check errors. Mirrors
/// [`author_device_add_with_promotion_in_tx`]'s "tie the result to our own entry" discipline — a
/// concurrently-ingested grant for the same subject cannot make a rejected entry look successful.
/// Runs entirely in the caller's IMMEDIATE txn (no open/commit/ingest).
pub fn author_stream_grant_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    grantee_account_id: AccountId,
    role: ops::GrantRole,
    now_ms: i64,
) -> anyhow::Result<GrantId> {
    // A grantee whose control this binary cannot execute gets no grant, whichever path asks for
    // one (a direct grant or a redeemed writer invite): the grant would admit content this
    // store could only retract.
    super::control_policy::require_supported_account_control(tx, grantee_account_id)?;
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot author a stream grant before the store's local account is minted (call \
         local_account first)",
    )?;
    let device = local_device(tx, now_ms)?;
    let op = AccountOp::StreamGrant { stream_id, grantee_account_id, grant_role: role };
    let grant_id =
        author_account_op_in_tx(tx, &device, account_id, genesis_hash, &op, None, now_ms)?;
    match storage::grant_effective_in_snapshot(
        tx,
        account_id,
        grant_id.into(),
        stream_id,
        grantee_account_id,
    )? {
        AuthorityQuery::Effective(fact) if fact.role == role => {},
        _ => anyhow::bail!(
            "the stream grant did not become an effective {role:?} grant — the local device lacks \
             effective owner authority on this account, or a concurrent op won the fold",
        ),
    }
    Ok(grant_id.into())
}

/// Why a writer grant is revoked — the machine-readable token that DRIVES the cut semantics
/// (OpenPGP's hard/soft split, the only prior art that exposes the choice at all): a compromised
/// key's own timeline cannot be trusted — its holder can backdate forged entries to sit before
/// any watermark the device itself reported — so only `Compromised` quarantines everything. The
/// departing reasons keep prior accepted work valid via chain-tail cuts taken from the OWNER's
/// own store, which the revoked device cannot rewrite. The token is the frozen wire `reason`
/// string; tests pin the exact spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum RevokeReason {
    Departed,
    Rotated,
    Superseded,
    Compromised,
}

impl RevokeReason {
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db_str(token: &str) -> anyhow::Result<Self> {
        token.parse().map_err(|_| {
            anyhow::anyhow!(
                "unknown revoke reason `{token}` — one of: departed, rotated, superseded, \
                 compromised"
            )
        })
    }

    /// Hard revocation: no self-reported boundary is trusted, everything from the grantee is
    /// quarantined unless the owner explicitly vouches via `--keep-until`.
    fn is_hard(self) -> bool {
        matches!(self, Self::Compromised)
    }
}

/// The authored revocation `author_stream_revoke_in_tx` reports back to the operator.
#[derive(Debug, Clone)]
pub struct StreamRevocation {
    /// The WRITER grants this revoke closed — plural, because double-granting authors two
    /// effective grant ids and leaving either open would leave the grantee writing.
    pub grant_ids: Vec<GrantId>,
    /// The authored `StreamRevoke` entries, one per closed grant.
    pub revoke_ids: Vec<AccountEntryHash>,
    /// The chain cuts each revoke carries — what prior work stays valid.
    pub cuts: Vec<ops::DeviceCut>,
}

/// Author a `StreamRevoke` closing the local account's open grant to `grantee_account_id` on
/// `stream_id`, then verify the fact under the same snapshot (the sibling of
/// [`author_stream_grant_in_tx`]'s author-then-verify discipline). The cut plan follows `reason`:
///
/// * soft (`departed`/`rotated`/`superseded`): one chain-tail cut per grantee device at the highest
///   entry THIS store has accepted — the owner's local copy predates the revocation and the revoked
///   device cannot rewrite it, so the owner is the independent witness and prior work stays valid
///   exactly as far as it vouches. A device this store never accepted work from gets no cut (there
///   is nothing to vouch for) and the fold quarantines it.
/// * hard (`compromised`): empty cuts — everything from the grantee is quarantined. `keep_until`
///   `(device, seq)` may carve one chain back in, but only up to an entry this store already holds
///   ACCEPTED for that chain; it refuses to vouch past what the owner holds.
///
/// `keep_until` with a soft reason is refused: the departing reasons already keep all prior
/// accepted work. Runs entirely in the caller's IMMEDIATE txn.
pub fn author_stream_revoke_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    grantee_account_id: AccountId,
    reason: RevokeReason,
    keep_until: Option<(DeviceFingerprint, u64)>,
    now_ms: i64,
) -> anyhow::Result<StreamRevocation> {
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot author a stream revoke before the store's local account is minted (call \
         local_account first)",
    )?;
    // Revocation states its own control gate, and must, because its CUT PLAN is derived from
    // accepted content. A pin retracts content acceptance for the account and its streams, so under
    // a pin `accepted_chain_tails` answers empty and a SOFT reason would author
    // `StreamRevoke { device_cuts: [] }` — byte-identical to the hard shape, silently quarantining
    // all of the grantee's prior work instead of keeping it valid as far as it vouches. Refuse
    // until content is evaluable under a pin; nothing here errors on its own.
    super::control_policy::require_supported_account_control(tx, account_id)?;
    let device = local_device(tx, now_ms)?;
    let grant_ids = storage::open_writer_grants(tx, account_id, stream_id, grantee_account_id)?;
    anyhow::ensure!(
        !grant_ids.is_empty(),
        "no open writer grant to that account on this repo's stream — `sync grants` lists them"
    );
    let cuts = match (reason.is_hard(), keep_until) {
        (false, None) => super::content::accepted_chain_tails(tx, stream_id, grantee_account_id)?,
        (false, Some(_)) => anyhow::bail!(
            "--keep-until only applies to --reason compromised — the departing reasons already \
             keep all prior accepted work"
        ),
        (true, None) => Vec::new(),
        (true, Some((device_fingerprint, seq))) => {
            let hash = super::content::accepted_entry_at(
                tx,
                stream_id,
                grantee_account_id,
                device_fingerprint,
                seq,
            )?
            .context(
                "--keep-until names an entry this store has not accepted — the owner's own store \
                 is the witness, and it cannot vouch past what it holds",
            )?;
            vec![ops::DeviceCut { device_fingerprint, seq, hash }]
        },
    };
    let mut revoke_ids = Vec::with_capacity(grant_ids.len());
    for grant_id in &grant_ids {
        let op = AccountOp::StreamRevoke {
            stream_id,
            grantee_account_id,
            grant_id: (*grant_id),
            device_cuts: cuts.clone(),
            reason: reason.as_db_str().to_string(),
        };
        revoke_ids.push(author_account_op_in_tx(
            tx,
            &device,
            account_id,
            genesis_hash,
            &op,
            None,
            now_ms,
        )?);
    }
    // Verify the FACT: no open writer grant remains — closing one of several would leave the
    // grantee writing while the command reports success.
    anyhow::ensure!(
        storage::open_writer_grants(tx, account_id, stream_id, grantee_account_id)?.is_empty(),
        "the stream revoke did not close every writer grant — the local device lacks effective \
         owner authority on this account, or a concurrent op won the fold",
    );
    Ok(StreamRevocation { grant_ids, revoke_ids, cuts })
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
/// `None` for an empty chain, which on every log is a device's first entry (seq 0, no predecessor).
/// On the CONTROL log only the founder's chain already begins with the genesis at seq 0; any other
/// owner's first control op opens its own chain, exactly as a first wrap does on the SECRETS log.
/// `log_id`-parameterized because the secrets chain is `(account, device)`-scoped across ALL
/// streams (never per-stream), so C4.3a reads its dense seq from the shared `log = 1` tail via this
/// same reader. Unlike `/3` content, an `account_entries.seq` is a plain numeric INTEGER, so `ORDER
/// BY seq DESC` is a numeric comparison (NOT a fixed-width big-endian blob compare).
pub(super) fn account_chain_tail(
    tx: &Transaction<'_>,
    account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
    log_id: u8,
) -> anyhow::Result<Option<(u64, AccountEntryHash)>> {
    // Every account signing path extends its chain from here, so this is where a forked chain stops
    // (#1417): the held tail of a forked chain may be the losing sibling, and everything signed on
    // top of it would be silently forked too.
    if let Some(seq) = super::fork::account_chain_fork(tx, account_id, device_fingerprint, log_id)?
    {
        return Err(super::fork::ForkedChain {
            lane: super::fork::ForkedLane::Account { account_id, log_id },
            seq,
        }
        .into());
    }
    chain_tail(tx, account_id, device_fingerprint, log_id, TailScope::Held)
}

/// The device's highest-seq entry on `log_id` that the fold ACCEPTED, which is a different question
/// from [`account_chain_tail`]: the candidate DAG is grow-only and holds retained and forked rows
/// above the accepted frontier. Authoring reads both and refuses when they disagree.
fn account_accepted_chain_tail(
    tx: &Transaction<'_>,
    account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
    log_id: u8,
) -> anyhow::Result<Option<(u64, AccountEntryHash)>> {
    chain_tail(tx, account_id, device_fingerprint, log_id, TailScope::Accepted)
}

/// Which rows a tail read considers. `Accepted` adds the `accepted = 1` predicate the partial
/// unique index `account_accepted_slot` keys on, so it yields at most one row per slot.
#[derive(Clone, Copy, Eq, PartialEq)]
enum TailScope {
    Held,
    Accepted,
}

/// Shared so the two tails cannot parse `seq` differently. `account_entries.seq` is a plain numeric
/// INTEGER — `ORDER BY seq DESC` is a numeric compare and the value reads back as i64 — NOT the
/// fixed-width big-endian blob `content_entries.seq` uses. Mixing the two silently misorders
/// chains.
fn chain_tail(
    tx: &Transaction<'_>,
    account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
    log_id: u8,
    scope: TailScope,
) -> anyhow::Result<Option<(u64, AccountEntryHash)>> {
    // `entry_hash` breaks the tie deterministically. `Accepted` cannot equivocate — the partial
    // unique index admits one accepted row per slot — but `Held` can, and without a tiebreak
    // SQLite may return either row, so the same stored state could permit or refuse authoring from
    // one call to the next.
    let sql = match scope {
        TailScope::Held =>
            "SELECT seq, entry_hash FROM account_entries
             WHERE account_id = ?1 AND log_id = ?2 AND device_fingerprint = ?3
             ORDER BY seq DESC, entry_hash LIMIT 1",
        TailScope::Accepted =>
            "SELECT seq, entry_hash FROM account_entries
             WHERE account_id = ?1 AND log_id = ?2 AND device_fingerprint = ?3 AND accepted = 1
             ORDER BY seq DESC, entry_hash LIMIT 1",
    };
    let row: Option<(i64, Vec<u8>)> = tx
        .query_row(
            sql,
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
        let hash: AccountEntryHash = hash
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

    /// Mint the account and own one `PublicRead` `/2` stream, BEFORE any pin: an ensure is
    /// authoring intent and refuses under every pin, so the ownership a grant needs cannot be
    /// established afterwards.
    fn account_owning_a_public_stream(conn: &Connection) -> (AccountId, crate::stream::StreamId) {
        let account = bootstrap::local_account(conn, NOW).unwrap();
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let stream = ensure_owned_stream_v2_with_mode_in_tx(
            &tx,
            "repo-a",
            crate::stream::AccessMode::PublicRead,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
        (account, stream)
    }

    /// The control version of this account's newest control-log entry. The version is a header
    /// field inside the signed envelope, not a column, so the row has to be decoded to read it.
    fn control_tail_op_version(conn: &Connection, account: AccountId) -> u32 {
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM account_entries
                 WHERE account_id = ?1 AND log_id = 0 ORDER BY seq DESC LIMIT 1",
                params![account.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        crate::account::envelope::decode_account_signed(&bytes).unwrap().header.op_version
    }

    /// The version a control op is signed at follows the ACCOUNT's control version, not the
    /// caller's. Authoring a grant under an executable pin signs control v2 — and
    /// `author_stream_grant_in_tx` verifies the FACT before returning, so an `Ok` here is itself
    /// the proof the v2 grant folded effective rather than being authored and ignored.
    #[test]
    fn a_pinned_account_signs_its_grant_as_control_v2() {
        let conn = db();
        let (account, stream) = account_owning_a_public_stream(&conn);
        assert_eq!(control_tail_op_version(&conn, account), 1, "unpinned authoring stays v1");

        crate::account::test_support::install_real_pin(&conn, account, NOW);

        let grantee = AccountId::from_bytes([0x5a; 32]);
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_stream_grant_in_tx(&tx, stream, grantee, ops::GrantRole::Writer, NOW)
            .expect("a pinned owner authors an effective v2 grant");
        tx.commit().unwrap();

        assert_eq!(
            control_tail_op_version(&conn, account),
            v2_ops::CONTROL_VERSION,
            "the stored grant is signed at the account's control version, not v1",
        );
    }

    /// A row above the device's ACCEPTED tail makes the next v2 entry unfoldable forever, so the
    /// seam refuses while nothing has been authored. An entry at an `op_version` no binary folds is
    /// retained-but-never-accepted, which is exactly the shape a v1 op authored between proposing
    /// and installing a pin leaves behind.
    #[test]
    fn an_unaccepted_tail_refuses_the_grant_before_any_entry_exists() {
        let conn = db();
        let (account, stream) = account_owning_a_public_stream(&conn);
        crate::account::test_support::install_real_pin(&conn, account, NOW);

        let device = local_device(&conn, NOW).unwrap();
        let (tail_seq, tail_hash) = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let tail =
                account_chain_tail(&tx, account, device.fingerprint(), fold::CONTROL_LOG).unwrap();
            tx.commit().unwrap();
            tail.unwrap()
        };
        let (_, stream_own) = crate::account::test_support::stream_own_public(account);
        let unfoldable = sign_account_entry(
            device.secret(),
            &AccountEntryHeader {
                account_id: account,
                log_id: fold::CONTROL_LOG,
                device_fingerprint: device.fingerprint(),
                seq: tail_seq + 1,
                prev_hash: Some(tail_hash),
                parent_ref: Some(tail_hash),
                entry_type: ops::entry_type_of(&stream_own),
                // No binary folds this, so it is retained and never accepted.
                op_version: 99,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: Some(tail_hash.into()),
            },
            &ops::encode(&stream_own).unwrap(),
        )
        .unwrap();
        storage::account_ingest(&conn, &unfoldable.signed_bytes, NOW).unwrap();

        let grantee = AccountId::from_bytes([0x5b; 32]);
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        // Both counts are read INSIDE the transaction. Reading them outside would compare two
        // post-rollback states, which match whether or not the seam inserted a candidate — an
        // assertion that cannot fail.
        let before: i64 =
            tx.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
        let err = author_stream_grant_in_tx(&tx, stream, grantee, ops::GrantRole::Writer, NOW)
            .expect_err("a chain ending above the accepted tail must refuse");
        let after: i64 =
            tx.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
        drop(tx);
        assert!(
            err.to_string().contains("did not accept"),
            "expected the accepted-tail refusal, got: {err}",
        );
        assert_eq!(before, after, "the refusal authored nothing");
    }

    /// A revocation must not quietly change meaning under a pin. A SOFT reason takes its cuts from
    /// accepted content, and a pin retracts content acceptance for the account and its streams, so
    /// the cut plan would come back empty — the byte-identical shape a HARD revocation authors.
    /// `departed` promises prior work stays valid as far as it vouches; silently quarantining all
    /// of it instead is worse than refusing, and nothing else on this path errors.
    /// Enrol `subject` on the account's roster BEFORE any pin — enrollment states its own strict
    /// gate, so a device cannot be added once the account is pinned.
    fn enrol(conn: &Connection, seed: u8) -> DeviceFingerprint {
        let joiner = DeviceSecret::from_seed(&[seed; 32]);
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        author_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: joiner.public().to_bytes(),
                x25519_pubkey: DeviceX25519Secret::from_seed(&[seed.wrapping_add(1); 32])
                    .public()
                    .to_bytes(),
                label: None,
            },
            ops::DeviceRole::Member,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
        joiner.public().fingerprint()
    }

    /// Every annex-log payload this account holds.
    fn annex_payloads(conn: &Connection, account: AccountId) -> Vec<Vec<u8>> {
        let mut stmt = conn
            .prepare(
                "SELECT signed_bytes FROM account_entries
                 WHERE account_id = ?1 AND log_id = ?2 ORDER BY seq",
            )
            .unwrap();
        let rows: Vec<Vec<u8>> = stmt
            .query_map(params![account.to_bytes().as_slice(), fold::ANNEX_LOG], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows.iter()
            .map(|bytes| {
                crate::account::envelope::decode_account_signed(bytes).unwrap().payload.to_vec()
            })
            .collect()
    }

    /// A verifiable pin over the account's own evidence, proposed but NOT installed.
    fn proposed_pin(
        conn: &Connection,
        account: AccountId,
    ) -> (
        crate::account::checkpoint::TrustedCheckpointPin,
        crate::account::checkpoint::VerifiedCheckpoint,
    ) {
        let device = crate::local_device(conn, NOW).unwrap();
        // The public seam: a DEFERRED read, dropped. Re-rolling it here took a write lock and
        // committed an empty transaction for nothing.
        let bundle =
            crate::account::checkpoint::propose_checkpoint(conn, account, &device).unwrap();
        let pin = crate::account::checkpoint::TrustedCheckpointPin {
            account_id: account,
            checkpoint_digest: bundle.certificate_digest(),
            required_control_version: 2,
        };
        (pin, crate::account::checkpoint::verify_checkpoint(pin, &bundle).unwrap())
    }

    fn install_pin(
        conn: &Connection,
        pin: crate::account::checkpoint::TrustedCheckpointPin,
        proof: &crate::account::checkpoint::VerifiedCheckpoint,
    ) -> crate::PinInstallOutcome {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let outcome = crate::pin_checkpoint_in_tx(&tx, pin, proof).unwrap();
        tx.commit().unwrap();
        outcome
    }

    /// Leave only the genesis: a store restored without its own control history, still holding the
    /// pin. The genesis stays because `read_local_account` resolves the pointer through it —
    /// without it the store would forget which account is its own.
    fn forget_all_but_genesis(conn: &Connection, account: AccountId) {
        let genesis = crate::read_local_account_genesis(conn).unwrap().unwrap();
        conn.execute(
            "DELETE FROM account_entries WHERE account_id = ?1 AND entry_hash != ?2",
            rusqlite::params![account.to_bytes().as_slice(), genesis.as_slice()],
        )
        .unwrap();
    }

    /// Author a v2 removal of `subject` and assert it actually took effect.
    fn remove_and_assert_revoked(
        conn: &Connection,
        account: AccountId,
        subject: DeviceFingerprint,
    ) {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, subject, "left", NOW)
            .expect("an owner holding the pin authors a v2 removal under it");
        tx.commit().unwrap();
        let closed: Option<i64> = conn
            .query_row(
                "SELECT closed_at FROM account_roster_history
                  WHERE account_id = ?1 AND device_fingerprint = ?2",
                rusqlite::params![account.to_bytes().as_slice(), subject.to_bytes().as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(closed.is_some(), "the revocation takes effect: the device leaves the roster");
    }

    /// An owner restored without its own control history can still revoke under its pin.
    ///
    /// It does NOT fail closed, which is the point. The accepted tail equals the raw tail, so the
    /// pin guard PASSES and the removal is authored — at a seq the checkpoint already froze for
    /// THIS device. It is then discarded as contesting a frozen slot and the device keeps its
    /// roster seat. Stub the storage out and what fires is `author_device_remove_in_tx`'s
    /// post-check, never a refusal.
    ///
    /// The fixture's local device is the founder, whose chain the deletion truncates — which is the
    /// hazard exactly. A device holding its OWN seq-0 entry authors at its own tail+1, a slot the
    /// checkpoint never froze for it, so this is a restore case rather than a second-device one.
    #[test]
    fn an_owner_that_installed_its_pin_without_the_history_can_author_under_it() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let subject = enrol(&conn, 0x72);
        let (pin, proof) = proposed_pin(&conn, account);
        forget_all_but_genesis(&conn, account);

        assert_eq!(install_pin(&conn, pin, &proof), crate::PinInstallOutcome::Installed);
        remove_and_assert_revoked(&conn, account, subject);
    }

    /// Re-installing the SAME pin repairs a store that holds the pin but lacks its evidence — the
    /// state of any store pinned before install began storing it. `AlreadyPinned` must not short
    /// out before the evidence is stored.
    #[test]
    fn reinstalling_a_pin_restores_the_evidence_it_was_installed_without() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let subject = enrol(&conn, 0x73);
        let (pin, proof) = proposed_pin(&conn, account);
        assert_eq!(install_pin(&conn, pin, &proof), crate::PinInstallOutcome::Installed);
        forget_all_but_genesis(&conn, account);

        assert_eq!(install_pin(&conn, pin, &proof), crate::PinInstallOutcome::AlreadyPinned);
        remove_and_assert_revoked(&conn, account, subject);
    }

    /// A capacity refusal part-way through storing the evidence leaves NOTHING behind through the
    /// production seam: no pin, and not the prefix of evidence already inserted.
    ///
    /// Evidence is stored one candidate at a time, so the refusal lands with some rows already in
    /// the transaction. `account_entries` has no delete path, so a caller that committed past the
    /// error would keep that partial history for an account that never pinned. The reservation
    /// below leaves exactly one slot, so at least one insert succeeds before the refusal.
    #[test]
    fn a_capacity_refused_install_leaves_neither_a_pin_nor_partial_evidence() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        enrol(&conn, 0x74);
        let (pin, proof) = proposed_pin(&conn, account);
        assert!(proof.bundle().evidence.len() >= 3, "room for one insert, then a refusal");
        forget_all_but_genesis(&conn, account);
        let before = stored_entries(&conn, account);
        reserve_all_but(&conn, account, before, 1);

        assert!(
            crate::install_checkpoint(&conn, pin, proof.bundle()).is_err(),
            "the evidence does not fit",
        );
        assert_eq!(
            stored_entries(&conn, account),
            before,
            "no prefix of the evidence survives the refusal",
        );
        assert!(!crate::account_is_pinned(&conn, account).unwrap(), "and nothing was pinned");
    }

    fn stored_entries(conn: &Connection, account: AccountId) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM account_entries WHERE account_id = ?1",
            [account.to_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Reserve the account's ordinary candidate budget down to `slots_left` free entries.
    fn reserve_all_but(conn: &Connection, account: AccountId, held: i64, slots_left: i64) {
        conn.execute(
            "INSERT INTO account_candidate_reservations
                 (reservation_id, account_id, reserved_entries, reserved_bytes, expires_at_ms)
             VALUES (?1, ?2, ?3, 0, ?4)",
            rusqlite::params![
                [0xee_u8; 32].as_slice(),
                account.to_bytes().as_slice(),
                crate::account::storage::ORDINARY_CANDIDATES_PER_ACCOUNT_MAX as i64
                    - held
                    - slots_left,
                i64::MAX / 2,
            ],
        )
        .unwrap();
    }

    /// The repair path can itself be refused for capacity, and then leaves the pin untouched —
    /// the case where "untouched either way" is load-bearing, since the pin is already permanent.
    #[test]
    fn a_capacity_refused_repair_leaves_the_pin_and_stores_nothing() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        enrol(&conn, 0x75);
        let (pin, proof) = proposed_pin(&conn, account);
        assert_eq!(install_pin(&conn, pin, &proof), crate::PinInstallOutcome::Installed);
        forget_all_but_genesis(&conn, account);

        let before = stored_entries(&conn, account);
        // ONE free slot, not zero: with none, the refusal fires before anything is inserted and
        // "stored nothing" is a tautology. With one, a row lands and is then rolled back, so the
        // assertion below is about the rollback rather than about nothing having happened.
        reserve_all_but(&conn, account, before, 1);
        assert!(
            crate::install_checkpoint(&conn, pin, proof.bundle()).is_err(),
            "the repair does not fit the candidate budget",
        );
        assert_eq!(
            stored_entries(&conn, account),
            before,
            "the row inserted before the refusal is rolled back with it",
        );
    }

    /// A pinned removal names its manifest by the digest of the manifest's PAYLOAD, and that
    /// manifest is a stored row.
    ///
    /// This is the binding the whole cut rests on, and the two candidate values are easy to
    /// confuse: authoring the manifest returns the annex ENTRY hash, while the cut must name
    /// `sha256(payload)`. Naming the entry hash would park the cut forever, waiting on evidence no
    /// executor can match. Hashing the stored payload here — rather than re-encoding the view this
    /// test believes was authored — is what makes it a test of the binding instead of of my
    /// arithmetic.
    #[test]
    fn a_pinned_removal_names_its_manifest_by_the_stored_payload_digest() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let subject = enrol(&conn, 0x71);
        crate::account::test_support::install_real_pin(&conn, account, NOW);

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, subject, "left", NOW).expect("a pinned owner removes");
        tx.commit().unwrap();

        assert_eq!(
            control_tail_op_version(&conn, account),
            v2_ops::CONTROL_VERSION,
            "the removal is signed at the account's control version",
        );
        let payloads = annex_payloads(&conn, account);
        assert_eq!(payloads.len(), 1, "exactly one manifest was authored");

        let cut_bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM account_entries
                 WHERE account_id = ?1 AND log_id = 0 ORDER BY seq DESC LIMIT 1",
                params![account.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let cut = crate::account::envelope::decode_account_signed(&cut_bytes).unwrap();
        let named = v2_ops::decode(cut.header.entry_type, &cut.payload)
            .unwrap()
            .pre_cut_view
            .expect("a revocation names a pre-cut view");
        assert_eq!(
            named,
            crate::cbor::sha256(&payloads[0]),
            "the cut names sha256 of the stored manifest payload, not the annex entry hash",
        );
    }

    /// The cut bounds the subject's chain at its ACCEPTED tail, not at `Cut::Empty`.
    ///
    /// The distinction is the whole point of computing a watermark: `Cut::Empty` means nothing on
    /// the chain is valid, retroactively invalidating every entry that device ever authored —
    /// including ones the account's own history rests on. A subject enrolled as a Member has
    /// authored nothing, so its cuts are legitimately `Empty` and could not tell a real watermark
    /// from a hardcoded one; the founder is the cheapest subject that actually has accepted
    /// entries to preserve.
    #[test]
    fn a_removal_cuts_at_the_subjects_accepted_tail_rather_than_closing_the_chain() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);

        // The subject must have accepted entries of its OWN, or its cut is legitimately `Empty`
        // and the assertion below could not tell a real watermark from a hardcoded one. Enrolling
        // as `Owner` mints an incarnation whose id is the DeviceAdd's entry hash, which is what
        // the subject's own control op then cites as its authority.
        let dev = crate::account::test_support::Dev::new(0x77);
        let incarnation = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let hash = author_device_add_in_tx(
                &tx,
                EnrollingDevice { ed25519_pubkey: dev.ed, x25519_pubkey: dev.x, label: None },
                ops::DeviceRole::Owner,
                NOW,
            )
            .unwrap();
            tx.commit().unwrap();
            hash
        };
        let (_, own) = crate::account::test_support::stream_own_mode(
            account,
            crate::stream::AccessMode::PublicRead,
            "repo-b",
        );
        let (bytes, authored) = crate::account::test_support::control_op(
            account,
            &dev,
            0,
            None,
            Some(incarnation.into()),
            &own,
        );
        storage::account_ingest(&conn, &bytes, NOW).unwrap();
        let accepted: i64 = conn
            .query_row(
                "SELECT accepted FROM account_entries WHERE entry_hash = ?1",
                params![authored.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(accepted, 1, "the subject's own control entry is accepted");

        let tail = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let tail =
                account_accepted_chain_tail(&tx, account, dev.fp, fold::CONTROL_LOG).unwrap();
            tx.commit().unwrap();
            tail.expect("the subject has an accepted control entry")
        };

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, dev.fp, "left", NOW).expect("the owner removes a device");
        tx.commit().unwrap();

        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM account_entries
                 WHERE account_id = ?1 AND log_id = 0 ORDER BY seq DESC LIMIT 1",
                params![account.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let stored = crate::account::envelope::decode_account_signed(&bytes).unwrap();
        let op = match ops::decode(stored.header.entry_type, &stored.payload).unwrap() {
            ops::DecodedAccountOp::Known(op) => op,
            _ => panic!("the authored removal decodes"),
        };
        let AccountOp::DeviceRemove { control_cut, .. } = op else {
            panic!("the authored op is a DeviceRemove");
        };
        assert_eq!(
            control_cut,
            crate::account::cut::Cut::At { seq: tail.0, hash: tail.1 },
            "the cut names the subject's accepted tail, so its prior work stays valid",
        );
    }

    /// A device cannot remove itself, and the refusal names the real reason.
    ///
    /// The removal would be authored at this device's own chain tail + 1, so it sits beyond any
    /// watermark it can name on that chain and its own register condemns it. Without the explicit
    /// precondition the seam still fails — but at the post-check, reporting missing owner
    /// authority, which is not what happened.
    #[test]
    fn a_device_cannot_remove_itself() {
        let conn = db();
        let (_account, _) = account_owning_a_public_stream(&conn);
        let own = local_device(&conn, NOW).unwrap().fingerprint();

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err = author_device_remove_in_tx(&tx, own, "left", NOW)
            .expect_err("a device cannot remove itself");
        assert!(
            err.to_string().contains("cannot remove itself"),
            "expected the self-removal refusal, got: {err}",
        );
    }

    /// A DECIDED-but-unaccepted entry on the subject's chain must not block its removal.
    ///
    /// An entry at an `op_version` no binary folds is retained, never accepted — and it can never
    /// become accepted here. Refusing on it would let the subject of a cut author one and make
    /// ITSELF unremovable, which is an availability attack on the one operation aimed at it. The
    /// same reasoning covers rejected, condemned and forked entries, which is the commoner case: a
    /// second device racing an ownership ensure legitimately folds `Rejected(Ineffective)`.
    #[test]
    fn a_retained_entry_on_the_subjects_chain_does_not_block_its_removal() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let subject = enrol(&conn, 0x79);
        let secret = DeviceSecret::from_seed(&[0x79; 32]);

        let (_, op) = crate::account::test_support::stream_own_mode(
            account,
            crate::stream::AccessMode::PublicRead,
            "repo-c",
        );
        let retained = sign_account_entry(
            &secret,
            &AccountEntryHeader {
                account_id: account,
                log_id: fold::CONTROL_LOG,
                device_fingerprint: subject,
                seq: 0,
                prev_hash: None,
                parent_ref: None,
                entry_type: ops::entry_type_of(&op),
                // No binary folds this, so it is retained and never accepted.
                op_version: 99,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: None,
            },
            &ops::encode(&op).unwrap(),
        )
        .unwrap();
        storage::account_ingest(&conn, &retained.signed_bytes, NOW).unwrap();

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, subject, "left", NOW)
            .expect("a retained entry the subject authored cannot veto its own removal");
        tx.commit().unwrap();
    }

    /// The same exclusion applies on the SECRETS log, where it is a knowing over-revocation.
    ///
    /// A non-evaluable log-1 entry is prefix-transparent — slot-eligible, and acceptable to a NEWER
    /// binary — so cutting past it may condemn something a newer peer accepted. That is chosen
    /// deliberately: including it would let the subject of a cut plant one unfoldable secrets entry
    /// and make itself permanently unremovable. The control-log argument (a retained entry
    /// quarantines the rest of that chain everywhere) does NOT carry here, which is why this axis
    /// is pinned separately rather than left to the log-0 case.
    #[test]
    fn a_retained_secrets_entry_on_the_subjects_chain_does_not_block_its_removal() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let subject = enrol(&conn, 0x7f);
        let secret = DeviceSecret::from_seed(&[0x7f; 32]);

        let retained = sign_account_entry(
            &secret,
            &AccountEntryHeader {
                account_id: account,
                log_id: fold::SECRETS_LOG,
                device_fingerprint: subject,
                seq: 0,
                prev_hash: None,
                parent_ref: None,
                entry_type: 0,
                // A future version on log 1 is retained and slot-eligible, never accepted here.
                op_version: 99,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: None,
            },
            &[0x81, 0x01],
        )
        .unwrap();
        storage::account_ingest(&conn, &retained.signed_bytes, NOW).unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM account_entry_status WHERE entry_hash = ?1",
                params![retained.entry_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "retained_unfolded",
            "the fixture must actually be retained-not-accepted, or this proves nothing",
        );

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, subject, "left", NOW)
            .expect("a retained secrets entry cannot veto its subject's removal");
        tx.commit().unwrap();
    }

    /// A subject with UNDECIDED entries above its accepted tail cannot be cut.
    ///
    /// A parked entry can still become effective once this store catches up, so peers may already
    /// hold work a cut at the accepted tail would condemn — and the §11.4 repair has no authoring
    /// seam and is dropped outright under a pin, so the too-low cut would stand. `AuthLenAhead` is
    /// the reachable shape: an entry citing a fold length this store has not reached, which the
    /// fold documents as recoverable by syncing.
    #[test]
    fn a_subject_with_parked_entries_above_its_accepted_tail_refuses() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let dev = crate::account::test_support::Dev::new(0x7b);
        // Enrolled as Owner so its own control op resolves an incarnation and parks on freshness
        // rather than being rejected for want of authority.
        let incarnation = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let hash = author_device_add_in_tx(
                &tx,
                EnrollingDevice { ed25519_pubkey: dev.ed, x25519_pubkey: dev.x, label: None },
                ops::DeviceRole::Owner,
                NOW,
            )
            .unwrap();
            tx.commit().unwrap();
            hash
        };
        let (_, op) = crate::account::test_support::stream_own_mode(
            account,
            crate::stream::AccessMode::PublicRead,
            "repo-d",
        );
        let ahead = sign_account_entry(
            &dev.secret,
            &AccountEntryHeader {
                account_id: account,
                log_id: fold::CONTROL_LOG,
                device_fingerprint: dev.fp,
                seq: 0,
                prev_hash: None,
                parent_ref: None,
                entry_type: ops::entry_type_of(&op),
                op_version: 1,
                crypto_suite: 0,
                // Far beyond this store's effective count, so the fold parks it as recoverable.
                auth_len: 9_999,
                key_id: None,
                authority_ref: Some(incarnation.into()),
            },
            &ops::encode(&op).unwrap(),
        )
        .unwrap();
        storage::account_ingest(&conn, &ahead.signed_bytes, NOW).unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM account_entry_status WHERE entry_hash = ?1",
                params![ahead.entry_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "parked", "the fixture must actually park, or this proves nothing");

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err = author_device_remove_in_tx(&tx, dev.fp, "left", NOW)
            .expect_err("a subject this store has not caught up on cannot be cut");
        assert!(
            err.to_string().contains("undecided entries"),
            "expected the catch-up refusal, got: {err}",
        );
    }

    /// A pinned removal — the cut AND its manifest — is authorable with the ordinary budget
    /// exhausted (#1409).
    ///
    /// Zero ordinary slots also pins the authoring order: `insert_candidate` grants a manifest the
    /// raised ceiling only when a STORED cut already cites it, so a manifest authored before its
    /// cut would be charged the ordinary budget and refused.
    #[test]
    fn a_pinned_removal_is_authorable_with_the_ordinary_budget_exhausted() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let subject = enrol(&conn, 0x7d);
        crate::account::test_support::install_real_pin(&conn, account, NOW);
        reserve_all_but(&conn, account, stored_entries(&conn, account), 0);

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, subject, "left", NOW)
            .expect("the cut and its manifest both come from the revocation reserve");
        tx.commit().unwrap();
        assert_eq!(annex_payloads(&conn, account).len(), 1, "the manifest was stored");
    }

    /// The v1 removal an unpinned account authors is just as reachable from an exhausted budget:
    /// removal is where every recovery starts, and capacity never drains (#1409).
    #[test]
    fn an_unpinned_removal_is_authorable_with_the_ordinary_budget_exhausted() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let subject = enrol(&conn, 0x7e);
        reserve_all_but(&conn, account, stored_entries(&conn, account), 0);

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, subject, "left", NOW)
            .expect("the removal comes from the revocation reserve");
        tx.commit().unwrap();
    }

    /// An unpinned account removes a device the v1 way: no view exists to name, so authoring one
    /// would be committing to evidence the signed bytes never reference.
    #[test]
    fn an_unpinned_removal_stays_v1_and_authors_no_manifest() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        let subject = enrol(&conn, 0x73);

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, subject, "left", NOW).expect("an unpinned owner removes");
        tx.commit().unwrap();

        assert_eq!(control_tail_op_version(&conn, account), 1, "v1 bytes for a legacy account");
        assert!(annex_payloads(&conn, account).is_empty(), "and no manifest was authored");
    }

    /// Removing a device that is not roster-effective would fold `Rejected(Ineffective)` and
    /// tombstone a fingerprint no enrollment ever added, so the seam refuses before authoring.
    #[test]
    fn removing_a_device_that_was_never_enrolled_authors_nothing() {
        let conn = db();
        let (_account, _) = account_owning_a_public_stream(&conn);
        let stranger = DeviceSecret::from_seed(&[0x75; 32]).public().fingerprint();

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        // Both counts are read INSIDE the transaction: two post-rollback reads agree whether or
        // not anything was inserted, so that assertion could never fail.
        let before: i64 =
            tx.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
        let err = author_device_remove_in_tx(&tx, stranger, "left", NOW)
            .expect_err("a device that was never enrolled cannot be removed");
        let after: i64 =
            tx.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
        drop(tx);
        assert!(
            err.to_string().contains("not roster-effective"),
            "expected the roster precondition, got: {err}",
        );
        assert_eq!(before, after, "the refusal authored nothing");
    }

    #[test]
    fn revocation_still_refuses_under_a_pin() {
        let conn = db();
        let (account, stream) = account_owning_a_public_stream(&conn);
        let grantee = AccountId::from_bytes([0x5c; 32]);
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_stream_grant_in_tx(&tx, stream, grantee, ops::GrantRole::Writer, NOW)
            .expect("an unpinned owner grants normally");
        tx.commit().unwrap();

        crate::account::test_support::install_real_pin(&conn, account, NOW);

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err =
            author_stream_revoke_in_tx(&tx, stream, grantee, RevokeReason::Departed, None, NOW)
                .expect_err("a pinned account must not author a stream revoke");
        assert!(
            err.downcast_ref::<UnsupportedAccountControlVersion>().is_some(),
            "expected the typed pin refusal, got: {err}",
        );
    }

    /// Enrollment carries its OWN gate rather than inheriting the shared seam's, which now signs
    /// under a pin instead of refusing. Opening enrollment needs the invite ticket to carry the
    /// checkpoint digest, so until then a pinned account enrolls nothing.
    #[test]
    fn enrollment_still_refuses_under_a_pin() {
        let conn = db();
        let (account, _) = account_owning_a_public_stream(&conn);
        crate::account::test_support::install_real_pin(&conn, account, NOW);

        let joiner = DeviceSecret::from_seed(&[0x61; 32]);
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err = author_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: joiner.public().to_bytes(),
                x25519_pubkey: DeviceX25519Secret::from_seed(&[0x62; 32]).public().to_bytes(),
                label: None,
            },
            ops::DeviceRole::Member,
            NOW,
        )
        .expect_err("a pinned account must not enrol a device");
        assert!(
            err.downcast_ref::<UnsupportedAccountControlVersion>().is_some(),
            "expected the typed pin refusal, got: {err}",
        );
    }

    /// The unsent-work guard's "ever a writer" fact must survive the roster projection being
    /// rebuilt without the enrolment (what a contested fold does), which only the stored log can
    /// promise: the founder and a member stay writers with the projection emptied, a read-only
    /// enrolment never is, and current effectiveness is a different question that does not.
    #[test]
    fn a_writer_enrolment_outlives_the_roster_projection() {
        let conn = db();
        let account = bootstrap::local_account(&conn, NOW).expect("mint local account");
        let founder = local_device(&conn, NOW).unwrap().fingerprint();
        let member = DeviceSecret::from_seed(&[0x71; 32]);
        let reader = DeviceSecret::from_seed(&[0x73; 32]);
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        for (device, seed, role) in
            [(&member, 0x72, ops::DeviceRole::Member), (&reader, 0x74, ops::DeviceRole::ReadOnly)]
        {
            author_device_add_in_tx(
                &tx,
                EnrollingDevice {
                    ed25519_pubkey: device.public().to_bytes(),
                    x25519_pubkey: DeviceX25519Secret::from_seed(&[seed; 32]).public().to_bytes(),
                    label: None,
                },
                role,
                NOW,
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let ever = |fp| storage::device_ever_enrolled_as_writer(&conn, account, fp).unwrap();
        let now = |fp| storage::device_is_effective_writer(&conn, account, fp).unwrap();
        let stranger = DeviceSecret::from_seed(&[0x75; 32]).public().fingerprint();
        assert!(ever(founder) && ever(member.public().fingerprint()));
        assert!(!ever(reader.public().fingerprint()) && !ever(stranger));
        assert!(now(founder) && now(member.public().fingerprint()));

        conn.execute("DELETE FROM account_roster_history WHERE account_id = ?1", [account
            .to_bytes()
            .as_slice()])
            .unwrap();
        assert!(ever(founder) && ever(member.public().fingerprint()), "the log still says so");
        assert!(!ever(reader.public().fingerprint()) && !ever(stranger));
        assert!(!now(founder) && !now(member.public().fingerprint()), "effectiveness is gone");
    }

    /// A memoised "never a writer" is invalidated by the enrolment that makes it wrong, however
    /// it arrives — the control-log length is the version, not the memo's lifetime.
    #[test]
    fn a_memoised_non_writer_becomes_a_writer_the_moment_its_enrolment_lands() {
        let conn = db();
        let account = bootstrap::local_account(&conn, NOW).expect("mint local account");
        let joiner = DeviceSecret::from_seed(&[0x71; 32]);
        let memo = crate::table_sync::LocalWriterMemo::default();
        let ask = |memo: &crate::table_sync::LocalWriterMemo| {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Deferred).unwrap();
            memo.ever_writer(&tx, account, joiner.public().fingerprint()).unwrap()
        };
        assert!(!ask(&memo));
        assert!(!ask(&memo), "and the cached answer is reused");

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        author_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: joiner.public().to_bytes(),
                x25519_pubkey: DeviceX25519Secret::from_seed(&[0x72; 32]).public().to_bytes(),
                label: None,
            },
            ops::DeviceRole::Member,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(ask(&memo), "the same memo sees the enrolment");
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

    /// The PublicRead ensure the grant/revoke tests need: grants fold only on public streams
    /// (the production grant path requires a published repo for the same reason).
    fn ensure_public_committed(conn: &Connection, repo_id: &str) -> StreamId {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let stream_id = ensure_owned_stream_v2_with_mode_in_tx(
            &tx,
            repo_id,
            stream::AccessMode::PublicRead,
            NOW,
        )
        .expect("ensure public");
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
    fn author_stream_grant_makes_the_grantee_an_effective_writer() {
        let conn = db();
        let account = bootstrap::local_account(&conn, NOW).expect("mint local account");
        let stream = ensure_public_committed(&conn, "repo-x");
        let grantee = AccountId::from_bytes([0x77; 32]);
        assert_ne!(grantee, account, "the grantee is a separate identity");

        let grant_id = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let g = author_stream_grant_in_tx(&tx, stream, grantee, ops::GrantRole::Writer, NOW)
                .expect("the owner authors a writer grant");
            tx.commit().unwrap();
            g
        };

        // The grant is the effective fact, at Writer, for exactly this (stream, grantee).
        match storage::grant_effective(&conn, account, grant_id, stream, grantee).unwrap() {
            AuthorityQuery::Effective(fact) => assert_eq!(fact.role, ops::GrantRole::Writer),
            other => panic!("expected an effective writer grant, got {other:?}"),
        }
        // The same grant does NOT cover a different grantee (subject binding).
        let other_grantee = AccountId::from_bytes([0x88; 32]);
        assert!(
            matches!(
                storage::grant_effective(&conn, account, grant_id, stream, other_grantee).unwrap(),
                AuthorityQuery::Invalid(_)
            ),
            "the grant binds to its named grantee, not any account",
        );
    }

    #[test]
    fn a_revoke_closes_the_grant_and_the_listing_shows_it() {
        let conn = db();
        let account = bootstrap::local_account(&conn, NOW).expect("mint local account");
        let stream = ensure_public_committed(&conn, "repo-x");
        let grantee = AccountId::from_bytes([0x77; 32]);
        let grant_id = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let g = author_stream_grant_in_tx(&tx, stream, grantee, ops::GrantRole::Writer, NOW)
                .unwrap();
            tx.commit().unwrap();
            g
        };
        assert_eq!(
            storage::open_writer_grants(&conn, account, stream, grantee).unwrap(),
            vec![grant_id],
            "the open writer grant resolves by grantee"
        );

        let revocation = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let r =
                author_stream_revoke_in_tx(&tx, stream, grantee, RevokeReason::Departed, None, NOW)
                    .expect("the owner revokes its own grant");
            tx.commit().unwrap();
            r
        };
        assert_eq!(revocation.grant_ids, vec![grant_id]);
        assert!(
            revocation.cuts.is_empty(),
            "no accepted grantee content means nothing to vouch for"
        );
        assert!(
            storage::open_writer_grants(&conn, account, stream, grantee).unwrap().is_empty(),
            "the grant is closed"
        );
        let listing = storage::stream_grants_for_owner(&conn, account, stream).unwrap();
        assert_eq!(listing.len(), 1);
        assert!(!listing[0].open, "the listing shows the grant revoked");
        assert_eq!(listing[0].grantee_account_id, grantee);
        assert_eq!(listing[0].role, "writer");

        // A second revoke has nothing to close.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err =
            author_stream_revoke_in_tx(&tx, stream, grantee, RevokeReason::Departed, None, NOW)
                .unwrap_err()
                .to_string();
        assert!(err.contains("no open writer grant"), "{err}");
    }

    #[test]
    fn a_revoke_targets_the_writer_grants_and_spares_a_reader_grant() {
        let conn = db();
        let account = bootstrap::local_account(&conn, NOW).expect("mint local account");
        let stream = ensure_public_committed(&conn, "repo-x");
        let grantee = AccountId::from_bytes([0x77; 32]);
        // The grantee holds BOTH roles, the reader granted LAST. Revoking write access must
        // target the writer grant: closing the newest row regardless of role would close the
        // reader while the grantee keeps authoring, and the command would still report success.
        let writer = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let writer =
                author_stream_grant_in_tx(&tx, stream, grantee, ops::GrantRole::Writer, NOW)
                    .unwrap();
            author_stream_grant_in_tx(&tx, stream, grantee, ops::GrantRole::Reader, NOW).unwrap();
            tx.commit().unwrap();
            writer
        };

        let revocation = {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let r =
                author_stream_revoke_in_tx(&tx, stream, grantee, RevokeReason::Departed, None, NOW)
                    .unwrap();
            tx.commit().unwrap();
            r
        };
        assert_eq!(revocation.grant_ids, vec![writer], "the writer grant is what closes");
        assert!(
            storage::open_writer_grants(&conn, account, stream, grantee).unwrap().is_empty(),
            "no writer grant remains"
        );
        let open_readers = storage::stream_grants_for_owner(&conn, account, stream)
            .unwrap()
            .into_iter()
            .filter(|grant| grant.open)
            .map(|grant| grant.role)
            .collect::<Vec<_>>();
        assert_eq!(open_readers, vec!["reader".to_string()], "the reader grant is untouched");
    }

    #[test]
    fn keep_until_is_refused_outside_a_compromised_revoke() {
        let conn = db();
        bootstrap::local_account(&conn, NOW).expect("mint local account");
        let stream = ensure_public_committed(&conn, "repo-x");
        let grantee = AccountId::from_bytes([0x77; 32]);
        {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            author_stream_grant_in_tx(&tx, stream, grantee, ops::GrantRole::Writer, NOW).unwrap();
            tx.commit().unwrap();
        }
        let device = DeviceFingerprint::from_bytes([0x11; 32]);
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let err = author_stream_revoke_in_tx(
            &tx,
            stream,
            grantee,
            RevokeReason::Departed,
            Some((device, 0)),
            NOW,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--keep-until only applies to --reason compromised"), "{err}");

        // And a compromised keep-until cannot vouch past what this store holds accepted.
        let err = author_stream_revoke_in_tx(
            &tx,
            stream,
            grantee,
            RevokeReason::Compromised,
            Some((device, 0)),
            NOW,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has not accepted"), "{err}");
    }

    #[test]
    fn revoke_reason_tokens_are_frozen() {
        // The token is the frozen wire `reason` string — pin the exact spellings and the round
        // trip, and that an unknown token names the alternatives.
        for (reason, token) in [
            (RevokeReason::Departed, "departed"),
            (RevokeReason::Rotated, "rotated"),
            (RevokeReason::Superseded, "superseded"),
            (RevokeReason::Compromised, "compromised"),
        ] {
            assert_eq!(reason.as_db_str(), token);
            assert_eq!(RevokeReason::from_db_str(token).unwrap(), reason);
        }
        let err = RevokeReason::from_db_str("fired").unwrap_err().to_string();
        assert_eq!(
            err,
            "unknown revoke reason `fired` — one of: departed, rotated, superseded, compromised"
        );
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

    /// A second store that joined `founder`'s account exactly as enrollment does: mint the joiner's
    /// own identity, have the founder author its `DeviceAdd` at `role`, and adopt the account's
    /// entries. Its local device is the joined one, so everything authored there is signed by it.
    /// Returns the store and the `DeviceAdd` hash, which for an Owner is its incarnation id.
    fn joined_store(
        founder: &Connection,
        account: AccountId,
        role: ops::DeviceRole,
    ) -> (Connection, AccountEntryHash) {
        let joined = db();
        let device = local_device(&joined, NOW).unwrap();
        let tx = Transaction::new_unchecked(founder, TransactionBehavior::Immediate).unwrap();
        let device_add = author_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: device.ed25519_public_key(),
                x25519_pubkey: device.x25519_public_key(),
                label: None,
            },
            role,
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();
        let entries: Vec<Vec<u8>> = storage::account_entries_for_enrollment(founder, account)
            .unwrap()
            .into_iter()
            .map(|entry| entry.signed_bytes)
            .collect();
        bootstrap::adopt_enrollment_bootstrap(&joined, bootstrap::EnrollmentBootstrap {
            account_entries: &entries,
            account_id: account,
            genesis_hash: bootstrap::read_local_account_genesis(founder).unwrap().unwrap(),
            device_fingerprint: device.fingerprint(),
            device_add_hash: device_add,
            now_ms: NOW + 1,
        })
        .unwrap();
        (joined, device_add)
    }

    fn authored_header(conn: &Connection, hash: AccountEntryHash) -> AccountEntryHeader {
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM account_entries WHERE entry_hash = ?1",
                [hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        crate::account::envelope::decode_account_signed(&bytes).unwrap().header
    }

    fn is_accepted(conn: &Connection, hash: AccountEntryHash) -> bool {
        conn.query_row(
            "SELECT accepted FROM account_entries WHERE entry_hash = ?1",
            [hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn still_enrolled(conn: &Connection, account: AccountId, device: DeviceFingerprint) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM account_roster_history
                            WHERE account_id = ?1 AND device_fingerprint = ?2
                              AND closed_at IS NULL)",
            params![account.to_bytes().as_slice(), device.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// A second owner authors its FIRST control op, from an empty chain: seq 0, no predecessor,
    /// under its own incarnation. The seam used to treat an empty chain as a programming error and
    /// cite the genesis, so no device but the founder could author anything — a lost founder device
    /// left an account nobody could administer.
    #[test]
    fn a_second_owner_authors_its_first_control_op_from_an_empty_chain() {
        let founder = db();
        let account = bootstrap::local_account(&founder, NOW).unwrap();
        let subject = enrol(&founder, 0x61);
        let (second, second_incarnation) = joined_store(&founder, account, ops::DeviceRole::Owner);
        assert!(still_enrolled(&second, account, subject), "the joined store sees the subject");

        let tx = Transaction::new_unchecked(&second, TransactionBehavior::Immediate).unwrap();
        let removal = author_device_remove_in_tx(&tx, subject, "lost", NOW + 2)
            .expect("a second owner authors a removal");
        tx.commit().unwrap();

        let header = authored_header(&second, removal);
        assert_eq!(
            (header.seq, header.prev_hash),
            (0, None),
            "its first control op opens its chain"
        );
        assert_eq!(
            header.authority_ref,
            Some(second_incarnation.into()),
            "it acts under its own incarnation, never the genesis",
        );
        assert!(is_accepted(&second, removal), "the removal is accepted after refold");
        assert!(!still_enrolled(&second, account, subject), "and the subject leaves the roster");
    }

    /// A device holding no owner incarnation is refused by the shared control seam before anything
    /// is signed. It used to sign an entry citing the genesis, which its own fold then rejected.
    ///
    /// Driven at the seam itself: `author_device_remove_in_tx` states its own owner check first, so
    /// a test through it would pass without this one.
    #[test]
    fn a_device_that_is_not_an_owner_is_refused_before_anything_is_signed() {
        let founder = db();
        let account = bootstrap::local_account(&founder, NOW).unwrap();
        let (member, _) = joined_store(&founder, account, ops::DeviceRole::Member);
        let device = local_device(&member, NOW).unwrap();
        let genesis = bootstrap::read_local_account_genesis(&member).unwrap().unwrap();
        let (_, op) = crate::account::test_support::stream_own_public(account);

        let tx = Transaction::new_unchecked(&member, TransactionBehavior::Immediate).unwrap();
        // Both counts are read INSIDE the transaction, before it is dropped: reading them after
        // would compare two post-rollback states that match whether or not anything was inserted.
        let count = |tx: &Transaction<'_>| -> i64 {
            tx.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap()
        };
        let before = count(&tx);
        let error = author_account_op_in_tx(&tx, &device, account, genesis, &op, None, NOW + 2)
            .expect_err("a Member cannot author a control op");
        let after = count(&tx);
        drop(tx);
        assert!(error.to_string().contains("not an owner"), "expected the owner refusal: {error}");
        assert_eq!(before, after, "nothing was signed or inserted");
    }

    /// The founder's authored bytes do not change: its own incarnation and its own enrollment are
    /// both the genesis, so resolving them from the device's standing resolves to what it cited
    /// before.
    #[test]
    fn the_founder_still_cites_the_genesis() {
        let founder = db();
        bootstrap::local_account(&founder, NOW).unwrap();
        let genesis = bootstrap::read_local_account_genesis(&founder).unwrap().unwrap();
        let subject = enrol(&founder, 0x63);

        let tx = Transaction::new_unchecked(&founder, TransactionBehavior::Immediate).unwrap();
        let stream = ensure_owned_stream_v2_in_tx(&tx, "repo-x", NOW).unwrap();
        let removal = author_device_remove_in_tx(&tx, subject, "left", NOW).unwrap();
        let content = super::super::content::author_content_batch_in_tx(
            &tx,
            stream,
            &[node_create("n1", "by the founder")],
            NOW,
        )
        .unwrap();
        tx.commit().unwrap();

        let header = authored_header(&founder, removal);
        assert_eq!(header.authority_ref, Some(genesis.into()), "control cites the genesis");
        assert!(header.seq > 0 && header.prev_hash.is_some(), "and continues the founder's chain");
        let roster_ref: Vec<u8> = founder
            .query_row(
                "SELECT roster_ref FROM content_entries WHERE entry_hash = ?1",
                [content[0].as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(roster_ref, genesis.as_slice(), "content cites the genesis enrollment");
    }

    /// A Member device writes content under its OWN enrollment. It used to cite the genesis, which
    /// acceptance checks against the signer's own roster entry — so no device but the founder could
    /// write.
    #[test]
    fn a_member_authors_content_under_its_own_enrollment() {
        let founder = db();
        let account = bootstrap::local_account(&founder, NOW).unwrap();
        let tx = Transaction::new_unchecked(&founder, TransactionBehavior::Immediate).unwrap();
        let stream = ensure_owned_stream_v2_in_tx(&tx, "repo-x", NOW).unwrap();
        tx.commit().unwrap();
        let (member, member_enrollment) = joined_store(&founder, account, ops::DeviceRole::Member);

        let tx = Transaction::new_unchecked(&member, TransactionBehavior::Immediate).unwrap();
        let hashes = super::super::content::author_content_batch_in_tx(
            &tx,
            stream,
            &[node_create("n1", "by a member")],
            NOW + 2,
        )
        .expect("a Member authors content on its account's stream");
        tx.commit().unwrap();

        let (status, roster_ref): (String, Vec<u8>) = member
            .query_row(
                "SELECT s.status, e.roster_ref
                   FROM content_entry_status s JOIN content_entries e USING (entry_hash)
                  WHERE entry_hash = ?1",
                [hashes[0].as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(roster_ref, member_enrollment.as_slice(), "it cites its own enrollment");
        assert_eq!(status, "accepted", "and the content is accepted through the real fold");
    }

    /// Carry everything `from` holds for `account` into `to`, as sync would, and refold there.
    fn deliver(from: &Connection, to: &Connection, account: AccountId) {
        for entry in storage::account_entries_for_enrollment(from, account).unwrap() {
            storage::account_ingest(to, &entry.signed_bytes, NOW + 3).unwrap();
        }
    }

    /// Removing the founder device actually stops it: once the removal reaches the founder's own
    /// store, it can no longer author. This became reachable with non-founder authoring — nothing
    /// but the founder could author a removal of the founder before.
    #[test]
    fn a_removed_founder_can_no_longer_author() {
        let founder = db();
        let account = bootstrap::local_account(&founder, NOW).unwrap();
        let founder_fp = local_device(&founder, NOW).unwrap().fingerprint();
        let (second, _) = joined_store(&founder, account, ops::DeviceRole::Owner);

        let tx = Transaction::new_unchecked(&second, TransactionBehavior::Immediate).unwrap();
        author_device_remove_in_tx(&tx, founder_fp, "lost", NOW + 2)
            .expect("another owner removes the founder device");
        tx.commit().unwrap();
        deliver(&second, &founder, account);
        assert!(!still_enrolled(&founder, account, founder_fp), "the founder sees its removal");

        let tx = Transaction::new_unchecked(&founder, TransactionBehavior::Immediate).unwrap();
        let joiner = DeviceSecret::from_seed(&[0x65; 32]);
        let result = author_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: joiner.public().to_bytes(),
                x25519_pubkey: DeviceX25519Secret::from_seed(&[0x66; 32]).public().to_bytes(),
                label: None,
            },
            ops::DeviceRole::Member,
            NOW + 4,
        );
        drop(tx);
        // The removal closed its incarnation, so it is refused as a non-owner before signing — not
        // by some later failure that would pass an `is_err` just as well.
        let error = result.expect_err("a removed founder cannot enroll anyone");
        assert!(error.to_string().contains("not an owner"), "refused as a non-owner: {error}");
    }

    /// A founder that was demoted and re-promoted authors again. Its genesis incarnation closed at
    /// the demotion, so citing the genesis — as every control op used to — signed entries its own
    /// fold rejected: the account's founder could no longer administer it. It now acts under the
    /// incarnation the promotion minted.
    #[test]
    fn a_founder_demoted_and_re_promoted_authors_again() {
        let founder = db();
        let account = bootstrap::local_account(&founder, NOW).unwrap();
        let genesis = bootstrap::read_local_account_genesis(&founder).unwrap().unwrap();
        let founder_fp = local_device(&founder, NOW).unwrap().fingerprint();
        let (second, _) = joined_store(&founder, account, ops::DeviceRole::Owner);

        // No production seam authors a promotion or demotion, so the second owner drives the shared
        // control seam directly — the same one every control op passes through.
        let second_device = local_device(&second, NOW).unwrap();
        let tx = Transaction::new_unchecked(&second, TransactionBehavior::Immediate).unwrap();
        let (tip_seq, tip_hash) =
            account_chain_tail(&tx, account, founder_fp, fold::CONTROL_LOG).unwrap().unwrap();
        let demote = AccountOp::OwnerDemote {
            device_fingerprint: founder_fp,
            owner_id: genesis.into(),
            control_cut: crate::account::cut::Cut::At { seq: tip_seq, hash: tip_hash },
            secrets_cut: crate::account::cut::Cut::Empty,
            reason: "demoted".into(),
        };
        author_account_op_in_tx(&tx, &second_device, account, genesis, &demote, None, NOW + 2)
            .unwrap();
        let promote = AccountOp::OwnerPromote { device_fingerprint: founder_fp };
        let promotion =
            author_account_op_in_tx(&tx, &second_device, account, genesis, &promote, None, NOW + 2)
                .unwrap();
        tx.commit().unwrap();
        deliver(&second, &founder, account);
        assert!(
            is_accepted(&founder, promotion),
            "the founder's store accepts its re-promotion, or this pins nothing",
        );

        let joiner = DeviceSecret::from_seed(&[0x67; 32]);
        let tx = Transaction::new_unchecked(&founder, TransactionBehavior::Immediate).unwrap();
        let enrolled = author_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: joiner.public().to_bytes(),
                x25519_pubkey: DeviceX25519Secret::from_seed(&[0x68; 32]).public().to_bytes(),
                label: None,
            },
            ops::DeviceRole::Member,
            NOW + 4,
        )
        .expect("a re-promoted founder enrolls a device");
        tx.commit().unwrap();
        assert_eq!(
            authored_header(&founder, enrolled).authority_ref,
            Some(promotion.into()),
            "it acts under the incarnation the promotion minted, not the closed genesis one",
        );

        // An ENROLLMENT DeviceAdd too: the joiner accepts it from whichever owner signed it, so the
        // re-promoted founder can enroll again under the incarnation the promotion minted.
        let later = DeviceSecret::from_seed(&[0x69; 32]);
        let tx = Transaction::new_unchecked(&founder, TransactionBehavior::Immediate).unwrap();
        let enrollment = author_enrollment_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: later.public().to_bytes(),
                x25519_pubkey: DeviceX25519Secret::from_seed(&[0x6a; 32]).public().to_bytes(),
                label: None,
            },
            ops::DeviceRole::Member,
            NOW + 5,
        )
        .expect("a re-promoted founder authors an enrollment");
        tx.commit().unwrap();
        assert_eq!(
            authored_header(&founder, enrollment).authority_ref,
            Some(promotion.into()),
            "under the promotion's incarnation",
        );
    }

    /// A second owner authors an enrollment DeviceAdd, and the joiner's verifier accepts it: the
    /// verifier authenticates whoever signed it, and the fold decides who was entitled.
    #[test]
    fn a_second_owner_authors_an_enrollment_the_joiner_verifies() {
        let founder = db();
        let account = bootstrap::local_account(&founder, NOW).unwrap();
        let (second, _) = joined_store(&founder, account, ops::DeviceRole::Owner);
        let joiner = DeviceSecret::from_seed(&[0x6b; 32]);
        let joiner_x = DeviceX25519Secret::from_seed(&[0x6c; 32]).public().to_bytes();

        let tx = Transaction::new_unchecked(&second, TransactionBehavior::Immediate).unwrap();
        let device_add = author_enrollment_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: joiner.public().to_bytes(),
                x25519_pubkey: joiner_x,
                label: None,
            },
            ops::DeviceRole::Member,
            NOW + 2,
        )
        .expect("a second owner authors an enrollment");
        tx.commit().unwrap();

        let entries = storage::account_entries_for_enrollment(&second, account).unwrap();
        let signed =
            entries.iter().find(|e| e.entry_hash == device_add).unwrap().signed_bytes.clone();
        let bootstrap: Vec<Vec<u8>> = entries.into_iter().map(|e| e.signed_bytes).collect();
        storage::verify_enrollment_device_add(
            &bootstrap,
            account,
            device_add,
            &signed,
            joiner.public().to_bytes(),
            joiner_x,
        )
        .expect("the joiner verifies a DeviceAdd a second owner signed");
    }

    /// Under a pin, a second owner's first control op still opens its own chain. Its held and
    /// accepted tails are both empty, and that agrees; requiring an accepted tail to exist would
    /// refuse every device with no control history of its own.
    #[test]
    fn under_a_pin_a_second_owners_first_control_op_opens_its_chain() {
        let founder = db();
        let account = bootstrap::local_account(&founder, NOW).unwrap();
        let subject = enrol(&founder, 0x71);
        let (second, _) = joined_store(&founder, account, ops::DeviceRole::Owner);
        crate::account::test_support::install_real_pin(&second, account, NOW + 2);

        let tx = Transaction::new_unchecked(&second, TransactionBehavior::Immediate).unwrap();
        let removal = author_device_remove_in_tx(&tx, subject, "lost", NOW + 3)
            .expect("a second owner authors its first control op under the pin");
        tx.commit().unwrap();

        let header = authored_header(&second, removal);
        assert_eq!((header.seq, header.prev_hash), (0, None), "it opens its own chain");
        assert_eq!(header.op_version, v2_ops::CONTROL_VERSION, "signed as control v2");
        assert!(!still_enrolled(&second, account, subject), "and the removal takes effect");
    }

    /// Depth two: an owner that a non-founder owner enrolled enrolls a device in turn. The joiner's
    /// verifier resolves its signer through two DeviceAdds to the genesis, and adoption's fold
    /// authorizes it through two incarnations — neither of which is the founder's.
    #[test]
    fn an_owner_enrolled_by_another_owner_enrolls_in_turn() {
        let founder = db();
        let account = bootstrap::local_account(&founder, NOW).unwrap();
        let (second, _) = joined_store(&founder, account, ops::DeviceRole::Owner);
        let (third, third_incarnation) = joined_store(&second, account, ops::DeviceRole::Owner);

        let joined = db();
        let device = local_device(&joined, NOW).unwrap();
        let tx = Transaction::new_unchecked(&third, TransactionBehavior::Immediate).unwrap();
        let device_add = author_enrollment_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: device.ed25519_public_key(),
                x25519_pubkey: device.x25519_public_key(),
                label: None,
            },
            ops::DeviceRole::Member,
            NOW + 2,
        )
        .expect("an owner enrolled by a non-founder owner enrolls a device");
        tx.commit().unwrap();
        assert_eq!(
            authored_header(&third, device_add).authority_ref,
            Some(third_incarnation.into()),
            "under its own incarnation",
        );

        let entries = storage::account_entries_for_enrollment(&third, account).unwrap();
        let signed =
            entries.iter().find(|e| e.entry_hash == device_add).unwrap().signed_bytes.clone();
        let bootstrap_bytes: Vec<Vec<u8>> = entries.into_iter().map(|e| e.signed_bytes).collect();
        let genesis_hash = storage::verify_enrollment_device_add(
            &bootstrap_bytes,
            account,
            device_add,
            &signed,
            device.ed25519_public_key(),
            device.x25519_public_key(),
        )
        .expect("the verifier resolves the signer through two DeviceAdds");
        bootstrap::adopt_enrollment_bootstrap(&joined, bootstrap::EnrollmentBootstrap {
            account_entries: &bootstrap_bytes,
            account_id: account,
            genesis_hash,
            device_fingerprint: device.fingerprint(),
            device_add_hash: device_add,
            now_ms: NOW + 3,
        })
        .expect("and adoption's fold authorizes it");
        assert_eq!(bootstrap::read_local_account(&joined).unwrap(), Some(account));
    }

    /// A fork stops only the chain it is on (#1417). The same device keeps signing its other
    /// chains, which is what keeps recovery reachable: a sole owner whose secrets chain forked
    /// still needs its control chain to promote another device and have itself removed.
    #[test]
    fn a_forked_secrets_chain_refuses_secrets_authoring_and_leaves_control_open() {
        let dir = tempfile::tempdir().unwrap();
        let live = db();
        let (account, _) = account_owning_a_public_stream(&live);
        let incarnation = |conn: &Connection, repo: &str| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
            let hash = crate::advance_repo_incarnation_in_tx(&tx, repo, NOW)?;
            tx.commit().unwrap();
            anyhow::Ok(hash)
        };
        incarnation(&live, "repo-a").unwrap();
        let path = dir.path().join("restored.db");
        live.execute("VACUUM INTO ?1", [path.to_str().unwrap()]).unwrap();
        let restored = Connection::open(&path).unwrap();

        incarnation(&live, "repo-b").unwrap();
        let sibling = incarnation(&restored, "repo-c").expect("the stale copy cannot tell");
        let bytes: Vec<u8> = restored
            .query_row(
                "SELECT signed_bytes FROM account_entries WHERE entry_hash = ?1",
                [sibling.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        storage::account_ingest(&live, &bytes, NOW + 1).unwrap();

        let forked = crate::ForkedChain {
            lane: crate::ForkedLane::Account { account_id: account, log_id: fold::SECRETS_LOG },
            seq: 1,
        };
        let err = incarnation(&live, "repo-d").expect_err("a forked chain is not extended");
        assert_eq!(err.downcast_ref::<crate::ForkedChain>(), Some(&forked), "got: {err:#}");
        assert_eq!(crate::local_forked_chains(&live).unwrap(), vec![forked]);

        let tx = Transaction::new_unchecked(&live, TransactionBehavior::Immediate).unwrap();
        ensure_owned_stream_v2_in_tx(&tx, "repo-e", NOW).expect("the control chain is not forked");
        tx.commit().unwrap();
    }
}
