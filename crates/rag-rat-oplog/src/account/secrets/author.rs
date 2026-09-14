//! The in-tx secrets-log (`log_id = 1`) content-key mint + `StreamKeyWrap` author seam (C4.3a,
//! #607).
//!
//! The owner's counterpart to the C4.2b acceptance evaluator: it mints a fresh per-stream content
//! key, seals it to every roster-EFFECTIVE device, and authors a `StreamKeyWrap` onto the account's
//! secrets chain inside the caller's IMMEDIATE transaction — verify-accepted-or-rollback, mirroring
//! [`super::super::content::author_content_batch_in_tx`]. It authors wraps that fold `accepted`;
//! the ephemeral key is later recovered for content sealing through C4.3b's `key_id` adoption
//! cross-check and derived sealing-key projection.
//!
//! C4.4 (#607) adds LAZY rotation on device removal: [`rotate_stream_key_in_tx`] mints a fresh key
//! at a HIGHER epoch re-sealed only to the remaining effective devices — rotation is exactly a mint
//! whose SOLE delta from the initial one is the epoch, so it reuses
//! [`author_stream_key_wrap_in_tx`] unchanged. [`ensure_stream_key_current_in_tx`] is the lazy
//! trigger the seal path calls: it rotates only when a removed device still holds the current key,
//! and returns a typed [`RotationOutcome`] (never an error) so a MEMBER device — which cannot
//! author a rotation but may legitimately seal — does not roll its seal txn back. Nothing calls the
//! C4.4 entry points are consumed by sealed content authoring. New-device read catch-up is a
//! separate same-key fan-out: [`catch_up_stream_keys_for_device_in_tx`] authors one-recipient
//! siblings for [`super::sealing::live_stream_key_targets_for_device`]'s exact live targets
//! without minting, rotating, or advancing epochs.
//!
//! Three load-bearing invariants the fold alone does NOT enforce:
//!
//! - **The secrets chain is `(account, device)`-scoped across ALL streams, never per-stream.**
//!   Every stream's wraps interleave on one dense chain, so the seq is read from the shared `log =
//!   1` tail ([`authoring::account_chain_tail`] with [`SECRETS_LOG`]), NOT a per-stream tail. A
//!   per-stream tail would restart `seq` at 0 per stream, collide the second stream's `wrap@0` at
//!   the same `(account, log, device, seq)` accepted slot, fork, and roll the mint back.
//! - **Wrap-to-self is mandatory and must be byte-reproducible at adoption.** The plaintext key is
//!   EPHEMERAL (no plaintext store); the minter recovers it later via the uniform adoption unwrap
//!   (C4.3b), so it must seal to itself. Because the local device is a roster-effective recipient,
//!   its wrap is already in the fan-out; an authoring-time self-unwrap round-trip
//!   ([`assert_self_wrap_round_trips`]) proves recoverability, which verify-accepted does NOT (a
//!   WrapContext drift folds `accepted` yet bricks the stream at C5).
//! - **`WrapContext.key_epoch` at seal MUST equal the op's `key_epoch`.** They are independent
//!   values in code; a mismatch leaves the wrap unopenable by everyone, the minter included, and is
//!   invisible at epoch 0.

use anyhow::Context;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::super::bootstrap::{self, LocalAccountRef};
use super::super::envelope::{
    AccountEntryHeader, VerifiedAccountEntry, sign_account_entry, signed_entry_len,
};
use super::super::fold::{EntryStatus, SECRETS_LOG, SUPPORTED_OP_VERSION};
use super::super::id::{AccountEntryHash, OwnerId};
use super::super::keywrap::{self, ContentKey, SealedKeyWrap, WrapContext};
use super::super::storage::{self, CandidateInsert};
use super::super::{AccountId, authoring, limits};
use super::ops::{self, RepoIncarnation, StreamKeyWrap, WrapEntry};
use super::storage::RepoIncarnationState;
use crate::identity::LocalDevice;
use crate::local_device;
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

/// The key epoch a stream's content key first mints at. C4.4 lazy rotation bumps it on device
/// removal; C4.3a only ever mints the initial epoch.
const INITIAL_KEY_EPOCH: u64 = 0;

/// Return the current owner-authorized repository incarnation, authoring the first one only on the
/// founder while it remains an owner. Restricting automatic bootstrap to one immutable device keeps
/// disconnected owner projections from choosing competing predecessor-less roots.
pub fn ensure_repo_incarnation(
    conn: &Connection,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<Option<AccountEntryHash>> {
    let _durability = bootstrap::AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let Some(LocalAccountRef { account_id, genesis_hash }) = bootstrap::local_account_ref(&tx)?
    else {
        return Ok(None);
    };
    match super::storage::repo_incarnation_state(&tx, account_id, repo_id)? {
        RepoIncarnationState::Current(reference) => return Ok(Some(reference)),
        RepoIncarnationState::Contested => anyhow::bail!(
            "repository incarnation authority is contested; refusing to choose a fork locally"
        ),
        RepoIncarnationState::Absent => {},
    }
    let device = local_device(&tx, now_ms)?;
    let founder: Vec<u8> = tx.query_row(
        "SELECT device_fingerprint FROM account_entries
         WHERE entry_hash = ?1 AND account_id = ?2 AND accepted = 1",
        rusqlite::params![genesis_hash.as_slice(), account_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    let founder_is_owner: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_owner_incarnations
             WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NULL)",
        rusqlite::params![account_id.to_bytes().as_slice(), founder.as_slice()],
        |row| row.get(0),
    )?;
    if !founder_is_owner {
        return Ok(None);
    }
    if founder.as_slice() != device.fingerprint().to_bytes().as_slice() {
        return Ok(None);
    }
    if storage::effective_owner_incarnation_for_device(&tx, account_id, device.fingerprint())?
        .is_none()
    {
        return Ok(None);
    }
    let reference = advance_repo_incarnation_in_tx(&tx, repo_id, now_ms)?;
    tx.commit()?;
    Ok(Some(reference))
}

/// Explicitly advance `repo_id` to a fresh owner-authorized incarnation. The accepted signed-entry
/// hash is the new incarnation reference. Local repository removal never calls this seam.
pub fn advance_repo_incarnation_in_tx(
    tx: &Transaction<'_>,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<AccountEntryHash> {
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?
        .context("cannot author a repository incarnation before the local account is minted")?;
    let predecessor_ref = match super::storage::repo_incarnation_state(tx, account_id, repo_id)? {
        RepoIncarnationState::Absent => None,
        RepoIncarnationState::Current(reference) => Some(reference),
        RepoIncarnationState::Contested => anyhow::bail!(
            "repository incarnation authority is contested; refusing to choose a fork locally"
        ),
    };
    let device = local_device(tx, now_ms)?;
    let fingerprint = device.fingerprint();
    let authority_ref =
        storage::effective_owner_incarnation_for_device(tx, account_id, fingerprint)?.context(
            "the local device holds no live owner incarnation; cannot advance the repository",
        )?;
    let (seq, prev_hash) =
        match authoring::account_chain_tail(tx, account_id, fingerprint, SECRETS_LOG)? {
            Some((tail_seq, tail_hash)) =>
                (tail_seq.checked_add(1).context("secrets chain is at u64::MAX")?, Some(tail_hash)),
            None => (0, None),
        };
    let op = RepoIncarnation { repo_id: repo_id.to_string(), predecessor_ref };
    let payload = ops::encode_repo_incarnation(&op)
        .map_err(|error| anyhow::anyhow!("encoding repository incarnation failed: {error}"))?;
    let header = AccountEntryHeader {
        account_id,
        log_id: SECRETS_LOG,
        device_fingerprint: fingerprint,
        seq,
        prev_hash,
        parent_ref: Some(genesis_hash),
        entry_type: ops::entry_type::REPO_INCARNATION,
        op_version: SUPPORTED_OP_VERSION,
        crypto_suite: 0,
        auth_len: storage::account_effective_count(tx, account_id)?,
        key_id: None,
        authority_ref: Some(authority_ref),
    };
    let signed = sign_account_entry(device.secret(), &header, &payload)?;
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    match storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)? {
        CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {},
        CandidateInsert::AtCapacity(scope) => {
            anyhow::bail!("account candidate capacity reached at {scope:?}")
        },
    }
    let statuses = storage::refold_in_tx(tx, account_id, now_ms)?;
    anyhow::ensure!(
        statuses.get(&verified.entry_hash).copied() == Some(EntryStatus::Accepted),
        "authored repository incarnation did not fold accepted",
    );
    anyhow::ensure!(
        super::storage::repo_incarnation_state(tx, account_id, repo_id)?
            == RepoIncarnationState::Current(verified.entry_hash),
        "authored repository incarnation did not become the unique current reference",
    );
    Ok(verified.entry_hash)
}

/// Mint a fresh content key for `stream_id` and author a `StreamKeyWrap` sealing it to every
/// roster-effective device, WITHIN the caller's transaction: verify the wrap folds `accepted` and
/// roll back otherwise. Returns the authored entry hash. Neither opens nor commits the txn.
///
/// The caller must have (1) minted the store's local account ([`bootstrap::local_account`], which
/// self-transacts and cannot nest here) and (2) made `stream_id` OWNED and effective (e.g. via
/// [`authoring::ensure_owned_stream_v2_in_tx`]) before calling — a wrap authored before its
/// `StreamOwn` is effective parks `unknown_account` and rolls the batch back.
///
/// The plaintext key is EPHEMERAL: sealed to every recipient, self-unwrap-verified, then dropped
/// (zeroized on drop) — it is NOT persisted anywhere in plaintext, so C4.3a writes only the wrap
/// op.
pub fn mint_and_author_stream_key_wrap_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    let key = ContentKey::generate()?;
    author_stream_key_wrap_in_tx(tx, stream_id, &key, INITIAL_KEY_EPOCH, now_ms)
}

/// The outcome of [`ensure_stream_key_current_in_tx`]. Every variant is a NON-error signal: `Err`
/// from the ensure/rotate path is reserved for infra/DB failures, never a policy outcome (a member
/// legitimately can't rotate, and that must not roll back a seal txn).
#[derive(Debug)]
pub enum RotationOutcome {
    /// Rotation was needed and this (owner) device authored a fresh higher-epoch `StreamKeyWrap`.
    /// Every op the rotation authored — a large roster's fan-out spans several (#764).
    Rotated(Vec<AccountEntryHash>),
    /// No rotation needed — every recipient of the current wrap is still roster-effective (or the
    /// stream has no current wrap at all, so there is nothing to rotate).
    Current,
    /// Rotation IS needed but this device holds no live owner incarnation, so it cannot author one.
    /// A member device still seals under the CURRENT key — roster membership is READ access, not
    /// authoring authority — so the caller must proceed with the seal, NOT fail it. New-device
    /// catch-up is a separate owner-authored same-key fan-out, not a rotation outcome.
    StaleButNotOwner,
}

/// The exact live key groups handled by one catch-up pass. `authored` contains groups for which
/// this call wrote a same-key sibling; `already_covered` were no-ops because an accepted sibling
/// already named the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUpReport {
    pub target: DeviceFingerprint,
    pub authored: Vec<super::sealing::LiveKeyEpoch>,
    pub already_covered: Vec<super::sealing::LiveKeyEpoch>,
}

/// Rotate `stream_id`'s content key: mint a FRESH key at `current_max_accepted_epoch + 1` and
/// author it sealed to the roster-EFFECTIVE devices, WITHIN the caller's transaction
/// (verify-accepted-or- rollback). Returns the authored entry hash. Neither opens nor commits the
/// txn.
///
/// Rotation is a mint whose ONLY delta from the initial one is the epoch: recipients (which now
/// EXCLUDE the removed device), the owner-only `authority_ref`, the self-unwrap round-trip, and
/// verify-accepted all carry from [`author_stream_key_wrap_in_tx`] unchanged. It does NOT cite the
/// triggering `DeviceRemove` — lazy rotation is local policy, not a chained authority act.
///
/// Errors if the stream has NO prior accepted wrap (nothing to rotate; the seal path mints an
/// initial key via `current_sealing_key` → `NoCurrentKey` instead), or if the local device is not a
/// current owner (the inherited owner-only gate bails). [`ensure_stream_key_current_in_tx`] never
/// reaches either error — it returns `Current`/`StaleButNotOwner` first.
pub fn rotate_stream_key_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    let account_id = bootstrap::local_account_ref(tx)?
        .context(
            "cannot rotate a StreamKeyWrap before the store's local account is minted (call \
             local_account first)",
        )?
        .account_id;

    // Epoch = accepted_max + 1 over EFFECTIVE ACCEPTED wraps only. `select_current_sealing_wrap`
    // reads `accepted = 1` rows, so condemned wraps never feed the max — this is deliberate: taking
    // the max over condemned wraps would let a removed owner author epoch `u64::MAX` (→ condemned)
    // and DoS every future rotation by forcing the overflow error below.
    let current = super::sealing::select_current_sealing_wrap(tx, account_id, stream_id)?.context(
        "cannot rotate a stream with no prior accepted StreamKeyWrap (nothing to rotate)",
    )?;
    // `checked_add`: ERROR at `u64::MAX`, never wrap. A wrap-to-0 would silently lose the max-epoch
    // selection and regress sealing to an old key — a confidentiality footgun. Re-stamping a
    // numeric epoch that a condemned wrap once used is fine and deliberate (the condemned wrap
    // is gone from the accepted set); we add NO high-water mark (sticky local state would
    // diverge derive-on-read).
    let next_epoch = current
        .key_epoch
        .checked_add(1)
        .context("stream key epoch is at u64::MAX; cannot rotate")?;

    let key = ContentKey::generate()?;
    author_stream_key_wrap_in_tx(tx, stream_id, &key, next_epoch, now_ms)
}

/// The lazy rotation trigger the seal path (and a future CLI) calls before sealing `stream_id`:
/// rotate the content key IF a removed device still holds the current key, otherwise noop. Returns
/// a typed [`RotationOutcome`], never an error for a policy reason.
///
/// The rotation-NEEDED test ([`super::sealing::stream_key_rotation_needed`]) is device-independent
/// (current-wrap recipients vs the roster). The owner gate is checked HERE, BEFORE calling
/// [`rotate_stream_key_in_tx`]: a non-owner would make `rotate` bail (→ `Err` → the caller's seal
/// txn rolls back), which a member's legitimate seal must not suffer — so a member sees
/// `StaleButNotOwner` and seals under the current key instead.
pub fn ensure_stream_key_current_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<RotationOutcome> {
    let account_id = bootstrap::local_account_ref(tx)?
        .context(
            "cannot ensure a stream key is current before the store's local account is minted",
        )?
        .account_id;

    if !super::sealing::stream_key_rotation_needed(tx, account_id, stream_id)? {
        return Ok(RotationOutcome::Current);
    }

    // Rotation is needed — but only a current owner can author one. Gate on the live owner
    // incarnation (exactly what `rotate` → `author_stream_key_wrap_in_tx` would require) so a
    // member returns `StaleButNotOwner` rather than triggering a rollback-inducing `bail!` in
    // `rotate`.
    let device = local_device(tx, now_ms)?;
    if storage::effective_owner_incarnation_for_device(tx, account_id, device.fingerprint())?
        .is_none()
    {
        return Ok(RotationOutcome::StaleButNotOwner);
    }

    Ok(RotationOutcome::Rotated(rotate_stream_key_in_tx(tx, stream_id, now_ms)?))
}

/// Re-wrap every live content key in `streams` not already available to `target`, inside the
/// caller's IMMEDIATE transaction. Every requested stream must currently be owned by the local
/// account. This is same-key fan-out only: it never mints a key, advances an epoch, or invokes
/// rotation, and acceptance depends only on owner authority rather than local key possession.
///
/// All target/authority reads, exact historical recovery, sealing, inserts, refold, and acceptance
/// verification use this transaction snapshot. Every required key is recovered and sealed before
/// the first insert, so an unavailable or corrupt key cannot produce a partial fan-out.
pub fn catch_up_stream_keys_for_device_in_tx(
    tx: &Transaction<'_>,
    target: DeviceFingerprint,
    streams: &[StreamId],
    now_ms: i64,
) -> anyhow::Result<CatchUpReport> {
    rewrap_live_stream_keys_for_device_in_tx(tx, target, streams, now_ms, ExistingCoverage::Credit)
}

/// Re-wrap every live content key to the key certified by a newly authored `DeviceAdd`, even when
/// an older accepted wrap names the same fingerprint. Enrollment cannot credit fingerprint-only
/// coverage: that ciphertext may have been sealed to a different X25519 key before the roster
/// certificate existed.
pub fn enroll_stream_keys_for_device_in_tx(
    tx: &Transaction<'_>,
    target: DeviceFingerprint,
    streams: &[StreamId],
    now_ms: i64,
) -> anyhow::Result<CatchUpReport> {
    rewrap_live_stream_keys_for_device_in_tx(tx, target, streams, now_ms, ExistingCoverage::Ignore)
}

#[derive(Clone, Copy)]
enum ExistingCoverage {
    Credit,
    Ignore,
}

fn rewrap_live_stream_keys_for_device_in_tx(
    tx: &Transaction<'_>,
    target: DeviceFingerprint,
    streams: &[StreamId],
    now_ms: i64,
    existing_coverage: ExistingCoverage,
) -> anyhow::Result<CatchUpReport> {
    let LocalAccountRef { account_id, .. } = bootstrap::local_account_ref(tx)?
        .context("cannot catch up stream keys before the store's local account is minted")?;
    let device = local_device(tx, now_ms)?;
    storage::effective_owner_incarnation_for_device(tx, account_id, device.fingerprint())?
        .context(
            "the local device holds no live owner incarnation; cannot author catch-up \
             StreamKeyWraps",
        )?;
    let target_public = storage::effective_roster_x25519_pubkey(tx, account_id, target)?.context(
        "stream-key catch-up target is not currently roster-effective in the local account",
    )?;
    let targets = match existing_coverage {
        ExistingCoverage::Credit =>
            super::sealing::live_stream_key_targets_for_device(tx, target, streams)?,
        ExistingCoverage::Ignore => super::sealing::LiveKeyTargets {
            required: super::sealing::live_stream_key_epochs(tx, account_id, streams)?,
            already_covered: Vec::new(),
        },
    };

    let mut authored_wraps = Vec::with_capacity(targets.required.len());
    for live in &targets.required {
        let key =
            super::sealing::recover_exact_historical_content_key(tx, account_id, *live, &device)?
                .with_context(|| {
                format!(
                    "cannot recover required live content key for stream {:?} epoch {}",
                    live.stream_id.to_bytes(),
                    live.key_epoch,
                )
            })?;
        anyhow::ensure!(
            key.key_id() == live.key_id,
            "recovered catch-up key does not match its exact signed key_id",
        );
        let ctx = WrapContext {
            account_id: account_id.to_bytes(),
            stream_id: live.stream_id.to_bytes(),
            key_epoch: live.key_epoch,
            recipient_pub: target_public.to_bytes(),
        };
        let sealed = keywrap::seal_content_key(&key, &ctx, &target_public)?;
        authored_wraps.push(StreamKeyWrap {
            stream_id: live.stream_id,
            key_id: live.key_id.to_bytes(),
            key_epoch: live.key_epoch,
            wraps: vec![WrapEntry { recipient_fp: target, sealed }],
        });
    }

    author_stream_key_wrap_batch_in_tx(tx, &authored_wraps, now_ms)?;
    Ok(CatchUpReport {
        target,
        authored: targets.required,
        already_covered: targets.already_covered,
    })
}

/// The mint core with the content key and epoch injected, so a test can pin the key
/// (confidentiality checks) and drive a NONZERO epoch (the `WrapContext.key_epoch == op.key_epoch`
/// invariant is invisible at epoch 0). The caller owns the key's lifetime; it drops (and zeroizes)
/// at the call site once this returns.
fn author_stream_key_wrap_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    key: &ContentKey,
    key_epoch: u64,
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    let LocalAccountRef { account_id, .. } = bootstrap::local_account_ref(tx)?.context(
        "cannot author a StreamKeyWrap before the store's local account is minted (call \
         local_account first)",
    )?;
    let device = local_device(tx, now_ms)?;

    // Recipients = the roster-EFFECTIVE devices (SHOULD-FIX-1). Sealing a fresh key to a REMOVED
    // device would re-grant it read access and defeat rotation-on-removal, so
    // `stored_device_pubkeys` (fold-independent, keeps removed devices) is the wrong reader
    // here.
    let recipients = storage::list_effective_roster_x25519_pubkeys(tx, account_id)?;

    let mut wraps = Vec::with_capacity(recipients.len());
    for (recipient_fp, recipient_pub) in &recipients {
        // The AAD binds (account, stream, epoch, recipient) so no wrap can be transplanted across
        // any of them. `key_epoch` here is the op's `key_epoch` — the two MUST be one value.
        let ctx = WrapContext {
            account_id: account_id.to_bytes(),
            stream_id: stream_id.to_bytes(),
            key_epoch,
            recipient_pub: recipient_pub.to_bytes(),
        };
        let sealed = keywrap::seal_content_key(key, &ctx, recipient_pub)?;
        wraps.push(WrapEntry { recipient_fp: *recipient_fp, sealed });
    }

    // Prove the minter can recover the key from its OWN wrap BEFORE committing — verify-accepted
    // proves the op folds accepted, not that it is unwrappable (independent failure modes).
    assert_self_wrap_round_trips(&device, account_id, stream_id, key_epoch, key, &wraps)?;

    let batch = pack_wraps_into_ops(stream_id, key.key_id().to_bytes(), key_epoch, wraps)?;
    author_stream_key_wrap_batch_in_tx(tx, &batch, now_ms)
}

/// Split one recipient fan-out across as many `StreamKeyWrap` ops as the 64 KiB envelope requires
/// (#764).
///
/// A single op sealing the key to the whole roster hits the §18a envelope limit at roughly 460
/// recipients, so a larger account could not mint or rotate at all. The frozen wire already allows
/// several ops per `(stream, key_id)` — `WRAP_RECIPIENTS_MAX` deliberately exceeds what one
/// envelope can hold — and every consumer resolves the fan-out as a SET: key recovery and the
/// rotation-needed predicate both filter accepted wraps by `(key_epoch, key_id)` and flatten across
/// ALL matching ops, while `select_from_wraps` chooses only the epoch/key identity, which every
/// sibling shares. So a recipient in the third op is found exactly like one in the first.
///
/// Capacity is MEASURED against the real encoder rather than assumed from a per-entry byte count:
/// CBOR array headers grow at 24/256/65536 elements, so arithmetic on a fixed entry size would
/// silently drift. The estimate is then VERIFIED — any op that still exceeds the budget shrinks the
/// capacity and repacks — so an encoding change can make this less efficient but never incorrect.
fn pack_wraps_into_ops(
    stream_id: StreamId,
    key_id: [u8; 32],
    key_epoch: u64,
    wraps: Vec<WrapEntry>,
) -> anyhow::Result<Vec<StreamKeyWrap>> {
    let budget =
        limits::ACCOUNT_ENVELOPE_MAX_BYTES.saturating_sub(limits::ACCOUNT_ENVELOPE_SIGNED_RESERVE);
    let op_of = |entries: &[WrapEntry]| StreamKeyWrap {
        stream_id,
        key_id,
        key_epoch,
        wraps: entries.to_vec(),
    };
    let encoded_len = |entries: &[WrapEntry]| -> anyhow::Result<usize> {
        Ok(ops::encode(&op_of(entries))
            .map_err(|err| anyhow::anyhow!("encoding a StreamKeyWrap chunk failed: {err}"))?
            .len())
    };
    if wraps.len() <= 1 || encoded_len(&wraps)? <= budget {
        return Ok(vec![op_of(&wraps)]);
    }

    let empty = encoded_len(&[])?;
    let per_entry = encoded_len(&wraps[..1])?.saturating_sub(empty).max(1);
    let mut capacity = budget.saturating_sub(empty) / per_entry;
    capacity = capacity.clamp(1, limits::WRAP_RECIPIENTS_MAX);
    loop {
        let chunks: Vec<StreamKeyWrap> = wraps.chunks(capacity).map(op_of).collect();
        let oversized = chunks
            .iter()
            .map(|chunk| encoded_len(&chunk.wraps))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .any(|len| len > budget);
        if !oversized {
            return Ok(chunks);
        }
        anyhow::ensure!(
            capacity > 1,
            "a StreamKeyWrap with a single recipient does not fit the {}-byte envelope budget",
            budget,
        );
        // Back off multiplicatively so a bad estimate converges in a few passes, not one per
        // recipient.
        capacity = (capacity * 3 / 4).max(1);
    }
}

/// Sign, store, refold, and verify one batch of already-sealed wraps. The secrets chain is shared
/// across streams, so the tail is read once and advanced in memory; one refold classifies the whole
/// batch.
/// The exact signed-envelope byte length of a one-recipient `StreamKeyWrap` entry at the maximum
/// header width — what enrollment's mint preflight charges per live key target (#945). Measured
/// from the real encoder + [`signed_entry_len`] (never a hand-computed sum): a wrap is a small
/// fixed-size payload, so charging the §18a envelope maximum per entry would put an artificial
/// enrollment ceiling on accounts with many streams or many retained live historical keys.
pub(in crate::account) fn single_recipient_wrap_envelope_bytes() -> usize {
    let wrap = StreamKeyWrap {
        stream_id: StreamId::from_bytes([u8::MAX; 32]),
        key_id: [u8::MAX; 32],
        key_epoch: u64::MAX,
        wraps: vec![WrapEntry {
            recipient_fp: DeviceFingerprint::from_bytes([u8::MAX; 32]),
            sealed: SealedKeyWrap { ephemeral_pubkey: [u8::MAX; 32], ciphertext: [u8::MAX; 48] },
        }],
    };
    let payload = ops::encode(&wrap).expect("a single canonical recipient encodes");
    let header = AccountEntryHeader {
        account_id: AccountId::from_bytes([u8::MAX; 32]),
        log_id: SECRETS_LOG,
        device_fingerprint: DeviceFingerprint::from_bytes([u8::MAX; 32]),
        seq: u64::MAX,
        prev_hash: Some(AccountEntryHash::from_bytes([u8::MAX; 32])),
        parent_ref: Some(AccountEntryHash::from_bytes([u8::MAX; 32])),
        entry_type: ops::entry_type_of(&wrap),
        op_version: SUPPORTED_OP_VERSION,
        crypto_suite: 0,
        auth_len: u64::MAX,
        key_id: None,
        authority_ref: Some(OwnerId::from_bytes([u8::MAX; 32])),
    };
    signed_entry_len(&header, &payload)
}

fn author_stream_key_wrap_batch_in_tx(
    tx: &Transaction<'_>,
    wraps: &[StreamKeyWrap],
    now_ms: i64,
) -> anyhow::Result<Vec<AccountEntryHash>> {
    if wraps.is_empty() {
        return Ok(Vec::new());
    }
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot author a StreamKeyWrap before the store's local account is minted (call \
         local_account first)",
    )?;
    let device = local_device(tx, now_ms)?;
    let fingerprint = device.fingerprint();

    // Dense seq from the shared `(account, device)` secrets tail across ALL streams (BLOCKER-1).
    // The secrets log has no genesis, so an empty chain is the legitimate first-wrap case (seq
    // 0, no predecessor) — unlike the control chain, where an empty tail is a programming
    // error.
    let (mut seq, mut prev_hash) =
        match authoring::account_chain_tail(tx, account_id, fingerprint, SECRETS_LOG)? {
            Some((tail_seq, tail_hash)) => (
                tail_seq
                    .checked_add(1)
                    .context("secrets chain tail is at u64::MAX seq; cannot extend")?,
                Some(tail_hash),
            ),
            None => (0, None),
        };

    // Owner-only authority (B3): cite the local device's LIVE owner incarnation, resolved from the
    // current fold. For the founder this resolves to the genesis hash (a founder's owner_id IS its
    // genesis), but a demoted-then-repromoted or non-founder owner gets its CURRENT incarnation — a
    // hard-coded genesis would cite a CLOSED incarnation and roll every mint back. `None` means the
    // local device is not a current owner, so it cannot author a StreamKeyWrap at all.
    let authority_ref =
        storage::effective_owner_incarnation_for_device(tx, account_id, fingerprint)?.context(
            "the local device holds no live owner incarnation; cannot author a StreamKeyWrap \
             (owner-only authority)",
        )?;

    let auth_len = storage::account_effective_count(tx, account_id)?;
    let mut authored = Vec::with_capacity(wraps.len());
    for (index, wrap) in wraps.iter().enumerate() {
        let header = AccountEntryHeader {
            account_id,
            log_id: SECRETS_LOG,
            device_fingerprint: fingerprint,
            seq,
            prev_hash,
            parent_ref: Some(genesis_hash),
            entry_type: ops::entry_type_of(wrap),
            op_version: SUPPORTED_OP_VERSION,
            crypto_suite: 0,
            auth_len,
            key_id: None,
            authority_ref: Some(authority_ref),
        };
        let payload = ops::encode(wrap)
            .map_err(|err| anyhow::anyhow!("encoding the StreamKeyWrap op failed: {err}"))?;
        let signed = sign_account_entry(device.secret(), &header, &payload)?;
        let verified = VerifiedAccountEntry {
            header: signed.header,
            payload: signed.payload,
            entry_hash: signed.entry_hash,
        };
        match storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)? {
            CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {},
            CandidateInsert::AtCapacity(scope) => anyhow::bail!(
                "the account candidate store is at capacity ({scope:?}); cannot author the \
                 StreamKeyWrap",
            ),
        }
        authored.push(verified.entry_hash);
        prev_hash = Some(verified.entry_hash);
        if index + 1 < wraps.len() {
            seq = seq
                .checked_add(1)
                .context("secrets chain tail is at u64::MAX seq; cannot extend batch")?;
        }
    }

    // ONE account-scoped refold (its secrets pass classifies the wrap), then verify-accepted. NEVER
    // persist an unaccepted wrap: the secrets candidate tail must stay equal to the accepted tail,
    // or the next mint's seq self-forks off an orphaned candidate. An owner authoring on its
    // own owned stream accepts, so anything else is an authority gap (missing/uneffective
    // `StreamOwn`, a stale `auth_len`, a contested account) and the whole caller mutation must
    // roll back.
    let statuses = storage::refold_in_tx(tx, account_id, now_ms)?;
    for entry_hash in &authored {
        match statuses.get(entry_hash).copied() {
            Some(EntryStatus::Accepted) => {},
            other => {
                let other = other.map(EntryStatus::as_db_str);
                anyhow::bail!(
                    "authored StreamKeyWrap did not fold accepted (status {other:?}); rolling back",
                )
            },
        }
    }
    Ok(authored)
}

/// Prove the minting device recovers the key from its OWN wrap in the fan-out, under the exact
/// `WrapContext` adoption (C4.3b) will reconstruct: `{owning account, stream, op.key_epoch, the
/// device's ROSTER x25519}`. The minter must be a recipient (its key is ephemeral, with no
/// plaintext store), and any drift in that context — wrong account/stream/epoch, or a non-roster
/// x25519 — would brick the stream at C5 with no recovery, so it is caught HERE rather than left to
/// the fold.
fn assert_self_wrap_round_trips(
    device: &LocalDevice,
    account_id: AccountId,
    stream_id: StreamId,
    key_epoch: u64,
    key: &ContentKey,
    wraps: &[WrapEntry],
) -> anyhow::Result<()> {
    let fingerprint = device.fingerprint();
    let self_wrap = wraps.iter().find(|w| w.recipient_fp == fingerprint).context(
        "the minting device is not among the wrap recipients; the content key would be \
         unrecoverable (the roster-effective recipient set must include the local device)",
    )?;
    // `recipient_pub` is the device's ROSTER x25519 — the same key the genesis certified and the
    // adoption unwrap reconstructs from; NOT a freshly re-derived one.
    let ctx = WrapContext {
        account_id: account_id.to_bytes(),
        stream_id: stream_id.to_bytes(),
        key_epoch,
        recipient_pub: device.x25519_public().to_bytes(),
    };
    let recovered = keywrap::unwrap_content_key(&self_wrap.sealed, device.x25519_secret(), &ctx)
        .context("the minting device cannot unwrap its own StreamKeyWrap")?;
    anyhow::ensure!(
        recovered.key_id() == key.key_id(),
        "self-unwrap recovered a key whose key_id differs from the minted key",
    );
    Ok(())
}

#[cfg(test)]
#[path = "author/tests.rs"]
mod tests;

#[cfg(test)]
mod wrap_packing_tests {
    use super::tests::{NOW, db};
    use super::*;
    use crate::account::keywrap::SealedKeyWrap;

    fn entry(seed: u16) -> WrapEntry {
        let mut fp = [0u8; 32];
        fp[..2].copy_from_slice(&seed.to_be_bytes());
        WrapEntry {
            recipient_fp: DeviceFingerprint::from_bytes(fp),
            // Real wraps are fixed-size (an X25519 ephemeral public key plus the AEAD output), so a
            // synthetic one packs identically to a sealed one.
            sealed: SealedKeyWrap { ephemeral_pubkey: [0xab; 32], ciphertext: [0xcd; 48] },
        }
    }

    fn pack(count: usize) -> Vec<StreamKeyWrap> {
        let entries: Vec<WrapEntry> = (0..count).map(|i| entry(i as u16)).collect();
        pack_wraps_into_ops(StreamId::from_bytes([7; 32]), [9; 32], 3, entries).expect("pack")
    }

    fn budget() -> usize {
        limits::ACCOUNT_ENVELOPE_MAX_BYTES - limits::ACCOUNT_ENVELOPE_SIGNED_RESERVE
    }

    /// A roster that fits stays ONE op — chunking must not fragment the common case.
    #[test]
    fn a_small_roster_is_a_single_op() {
        let ops = pack(8);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].wraps.len(), 8);
    }

    /// The bug: one op sealing to the whole roster exceeds the 64 KiB envelope at ~460 recipients,
    /// so mint and rotation failed outright for a larger account. The fan-out now spans ops.
    #[test]
    fn a_roster_too_large_for_one_envelope_fans_out_across_ops() {
        let count = limits::WRAP_RECIPIENTS_MAX;
        let ops = pack(count);
        assert!(ops.len() > 1, "{count} recipients cannot fit one envelope, so they must span ops",);
        for op in &ops {
            let encoded = ops::encode(op).expect("encode").len();
            assert!(
                encoded <= budget(),
                "every op must fit the payload budget: {encoded} > {}",
                budget(),
            );
            assert!(
                op.wraps.len() <= limits::WRAP_RECIPIENTS_MAX,
                "and stay within the per-op §18a recipient bound",
            );
        }
    }

    /// The fan-out is a PARTITION: every recipient appears exactly once, in order, across the ops.
    /// Consumers union the recipient sets by `(key_epoch, key_id)`, so a dropped or duplicated
    /// recipient would silently deny or double-seal a device.
    #[test]
    fn the_fan_out_partitions_the_recipients_and_shares_one_key_identity() {
        let count = limits::WRAP_RECIPIENTS_MAX;
        let ops = pack(count);
        let seen: Vec<DeviceFingerprint> =
            ops.iter().flat_map(|op| op.wraps.iter().map(|w| w.recipient_fp)).collect();
        let expected: Vec<DeviceFingerprint> =
            (0..count).map(|i| entry(i as u16).recipient_fp).collect();
        assert_eq!(seen, expected, "every recipient appears exactly once, in the original order");
        for op in &ops {
            assert_eq!(op.key_id, [9; 32], "siblings share the key identity selection keys on");
            assert_eq!(op.key_epoch, 3);
            assert_eq!(op.stream_id, StreamId::from_bytes([7; 32]));
            assert!(!op.wraps.is_empty(), "no empty op is emitted");
        }
    }

    /// Capacity is measured, not assumed: the packer fills each op close to the budget rather than
    /// falling back to something tiny. A regression here is silent — correctness holds while the
    /// op count balloons — so it is pinned.
    #[test]
    fn packing_fills_each_op_rather_than_emitting_many_small_ones() {
        let ops = pack(limits::WRAP_RECIPIENTS_MAX);
        let full = &ops[0];
        let encoded = ops::encode(full).expect("encode").len();
        assert!(
            encoded * 10 > budget() * 9,
            "the first op should fill >90% of the budget, got {encoded} of {}",
            budget(),
        );
    }

    /// The packer budgets against the PAYLOAD, but §18a bounds the SIGNED wire — so the reserve
    /// held back for the header and signature has to actually cover them. A full op fills its
    /// budget to within a couple of bytes, so an undersized reserve would not be approximately
    /// wrong, it would reject the very op the packer just built.
    #[test]
    fn a_maximally_packed_op_still_fits_the_signed_envelope() {
        let conn = db();
        let account = bootstrap::local_account(&conn, NOW).expect("mint local account");
        let device = local_device(&conn, NOW).unwrap();
        let ops = pack(limits::WRAP_RECIPIENTS_MAX);
        let fullest = ops.iter().max_by_key(|op| op.wraps.len()).expect("at least one op");
        let payload = ops::encode(fullest).expect("encode");

        let header = AccountEntryHeader {
            account_id: account,
            log_id: SECRETS_LOG,
            device_fingerprint: device.fingerprint(),
            seq: u64::MAX,
            prev_hash: Some(AccountEntryHash::from_bytes([0xff; 32])),
            parent_ref: Some(AccountEntryHash::from_bytes([0xff; 32])),
            entry_type: ops::entry_type::STREAM_KEY_WRAP,
            op_version: SUPPORTED_OP_VERSION,
            crypto_suite: 0,
            auth_len: u64::MAX,
            // A plaintext op carries no `key_id` (the header rejects one when crypto_suite == 0).
            // Every OTHER field is at its widest so the reserve is measured against the largest
            // header a real wrap op can have.
            key_id: None,
            authority_ref: Some(OwnerId::from_bytes([0xff; 32])),
        };
        let signed = sign_account_entry(device.secret(), &header, &payload)
            .expect("a maximally packed op must sign within the envelope");
        assert!(
            signed.signed_bytes.len() <= limits::ACCOUNT_ENVELOPE_MAX_BYTES,
            "signed wire is {} bytes, over the {} limit — the reserve is too small",
            signed.signed_bytes.len(),
            limits::ACCOUNT_ENVELOPE_MAX_BYTES,
        );
    }
}
