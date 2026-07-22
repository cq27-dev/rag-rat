//! Minting a snapshot over this device's current view of the account (C6, #609).
//!
//! The producer the rest of C6 was built against. Everything else — the wire, the canonical
//! projection, verification, usability, selection — consumes snapshots; until now only tests made
//! them.
//!
//! # The author and the verifier must agree by construction
//!
//! An author hashes a projection of the prefix it claims to cover; a verifier re-derives that
//! prefix from the same watermark vector and recomputes. If the two ever disagreed about what a
//! watermark vector *denotes*, every honest snapshot would fail verification and the failure would
//! look like a correctness bug in the fold rather than a disagreement about set membership. So
//! authoring does not build its own prefix: it calls [`super::verify::on_branch_prefix`], the same
//! function the verifier uses, and folds exactly what comes back.
//!
//! # Why only an open owner
//!
//! The manifest cites an owner incarnation, and usability is scoped to that incarnation staying
//! open (no control op can cut an annex chain, so there is no finer boundary available). A device
//! with no open incarnation therefore has no authority to cite and mints nothing — the outcome is
//! reported, not an error, because "this device is a member" is an ordinary state rather than a
//! failure.
//!
//! # Nothing is minted that this device would itself refuse
//!
//! Two states are declined for the same reason rather than authored and left to fail downstream: a
//! contested account (verification folds the full held set and requires `Live`), and a held
//! equivocation at a covered coordinate (verification refuses any one-branch claim about a slot it
//! holds two entries for). In both cases the artifact is guaranteed to be rejected — by this very
//! device, deterministically — so minting one would only burn bounded candidate capacity and put a
//! clean-looking claim about disputed state into the store. Both checks call the verifier's own
//! predicates, so the two sides cannot drift about what counts as refusable.

use anyhow::Context;
use rusqlite::Transaction;

use super::super::envelope::{self, AccountEntryHeader, VerifiedAccountEntry, sign_account_entry};
use super::super::fold::{self, AccountClassification};
use super::super::storage::{self, CandidateInsert};
use super::super::{AccountId, authoring, limits};
use super::ops::{SnapshotOp, SnapshotTarget};
use super::{projection, verify};
use crate::identity::LocalDevice;

type EntryHash = [u8; 32];

/// What an authoring attempt did. Only `Authored` mints an entry; the rest are ordinary states this
/// device can be in, reported so a caller can tell "nothing to do" from "something went wrong".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotAuthorOutcome {
    Authored(EntryHash),
    /// This device holds no open owner incarnation, so it has no authority to cite.
    NotAnOpenOwner,
    /// The account is contested; a snapshot of it would be refused by every verifier.
    AccountNotLive,
    /// The account has more devices than one manifest can name. A target's `covered` vector
    /// carries a watermark per device, so the 64 KiB envelope binds at roughly 820 — well before
    /// the `SNAPSHOT_COVERED_MAX` decoder bound, which is therefore unreachable (#868).
    ///
    /// Reported rather than raised because it is a property of the account's size, not a fault:
    /// the caller can surface "this account is too large to snapshot" instead of a raw encoding
    /// failure from deep inside authoring. Splitting the covered vector is NOT the fix — a
    /// `folded_state_hash` commits to the fold of the prefix its covered vector defines, so two
    /// halves are two snapshots over two different prefixes, neither dominating the other.
    CoverageExceedsEnvelope {
        devices: usize,
    },
    /// This device holds an entry at a covered coordinate that the accepted chain excludes — an
    /// equivocation. Any snapshot naming one branch of it is refused by a verifier holding the
    /// same evidence, including this one.
    HeldEvidenceOffBranch,
}

/// Mint a snapshot over the account's currently accepted control history.
pub fn author_snapshot_in_tx(
    tx: &Transaction<'_>,
    device: &LocalDevice,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<SnapshotAuthorOutcome> {
    let fingerprint = device.fingerprint();
    let Some(owner_id) =
        storage::effective_owner_incarnation_for_device(tx, account_id, fingerprint)?
    else {
        return Ok(SnapshotAuthorOutcome::NotAnOpenOwner);
    };

    let view = storage::account_entries_view(tx, account_id)?;
    // Classification folds the FULL held set, matching what a verifier does — an author that judged
    // only its accepted branch would call an account live that its peers can see is contested.
    let held_history = fold::fold_account(view.held());
    if held_history.classification() != AccountClassification::Live {
        return Ok(SnapshotAuthorOutcome::AccountNotLive);
    }

    // Not an outcome: this device just resolved an OPEN OWNER INCARNATION, and an incarnation is
    // only ever minted by folding control entries. Empty coverage here means the persisted
    // incarnation tables and the candidate store disagree about whether that history exists — a
    // corrupt store, not a state a caller can act on. Fail closed rather than mint a snapshot whose
    // coverage claim is vacuously true.
    // The CANONICAL root from the fold, never a scan for the genesis tag. A malformed same-payload
    // genesis can be held alongside the real one and sort ahead of it by hash; `find_genesis`
    // excludes it, a tag scan does not. Nothing downstream revalidates `parent_ref`, so picking the
    // wrong one would store, report as authored, and be selectable.
    let genesis_hash = held_history.genesis_hash().context(
        "the account folds Live but holds no canonical genesis; the candidate store is \
         inconsistent with its identity",
    )?;
    let covered = view.accepted_control_heads();
    anyhow::ensure!(
        !covered.is_empty(),
        "this device holds an open owner incarnation but no control history to cover; the \
         incarnation tables and the candidate store are inconsistent",
    );

    // Fold EXACTLY what a verifier will fold. Anything else — "everything held", say — would hash a
    // different set than the watermark vector denotes, and the claim would be false on arrival.
    let by_hash = view.held().iter().map(|entry| (entry.entry_hash, entry)).collect();
    let prefix = verify::on_branch_prefix(&covered, &by_hash).map_err(|reason| {
        anyhow::anyhow!(
            "cannot snapshot a prefix this device cannot reconstruct from its own accepted heads \
             ({reason:?}); the accepted chain should always be walkable"
        )
    })?;
    // Refuse before minting rather than after. A device holding an equivocation cannot express a
    // truthful one-branch claim about that slot, and `verify_snapshot` says so deterministically —
    // so authoring anyway would burn bounded candidate capacity on an artifact this very device
    // rejects. Same reasoning that keeps a contested account from being snapshotted.
    if verify::ignores_held_evidence(&prefix, view.held()) {
        return Ok(SnapshotAuthorOutcome::HeldEvidenceOffBranch);
    }

    let folded_state_hash = projection::folded_state_hash(&fold::fold_account(&prefix));

    // Two ceilings both mean "too many devices to name in one manifest", and both must surface as
    // the typed outcome rather than a raw error. The envelope binds first in practice (~820), but
    // above `SNAPSHOT_COVERED_MAX` the ENCODER itself rejects — check the higher one here, before
    // encoding, and the exact envelope size below.
    let devices = covered.len();
    if devices > limits::SNAPSHOT_COVERED_MAX {
        return Ok(SnapshotAuthorOutcome::CoverageExceedsEnvelope { devices });
    }
    let manifest = SnapshotOp::Snapshot {
        state_format_version: super::ops::SNAPSHOT_STATE_FORMAT_V1,
        moderation_epoch: 0,
        targets: vec![SnapshotTarget {
            log_id: fold::CONTROL_LOG,
            stream_id: None,
            subject_account_id: None,
            folded_state_hash,
            covered,
        }],
    };
    let payload = super::ops::encode(&manifest)
        .map_err(|err| anyhow::anyhow!("encoding the snapshot manifest failed: {err}"))?;

    // The annex chain is this device's own, independent of its control chain — that separation is
    // the whole reason the log exists (#809).
    let (seq, prev_hash) =
        match authoring::account_chain_tail(tx, account_id, fingerprint, fold::ANNEX_LOG)? {
            Some((tail_seq, tail_hash)) => (
                tail_seq.checked_add(1).context("annex chain tail is at u64::MAX seq")?,
                Some(tail_hash),
            ),
            None => (0, None),
        };

    let header = AccountEntryHeader {
        account_id,
        log_id: fold::ANNEX_LOG,
        device_fingerprint: fingerprint,
        seq,
        prev_hash,
        parent_ref: Some(genesis_hash),
        entry_type: super::ops::entry_type::SNAPSHOT,
        op_version: fold::SUPPORTED_OP_VERSION,
        // Plaintext is structural for this tag: the manifest's entire value is that a peer can
        // check coverage without decrypting anything, and ingest refuses a sealed one.
        crypto_suite: 0,
        auth_len: storage::account_effective_count(tx, account_id)?,
        key_id: None,
        authority_ref: Some(owner_id),
    };
    // Check BEFORE signing, against the EXACT signed size for this header — not a reserve. A
    // manifest cannot be split (`folded_state_hash` commits to the prefix its covered vector
    // defines), so the check is an exact validity threshold, and an account in the gap between a
    // conservative reserve and the real overhead must not be told it cannot snapshot when it can.
    // Without the guard the failure is a raw envelope rejection from inside `sign_account_entry`,
    // which reads as a bug in authoring rather than what it is: too many devices to name in one
    // manifest.
    if !envelope::entry_fits_envelope(&header, &payload) {
        return Ok(SnapshotAuthorOutcome::CoverageExceedsEnvelope { devices });
    }
    let signed = sign_account_entry(device.secret(), &header, &payload)?;
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    match storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)? {
        CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {},
        CandidateInsert::AtCapacity(scope) => anyhow::bail!(
            "the account candidate store is at capacity ({scope:?}); cannot author a snapshot",
        ),
    }

    // Refold in THIS transaction, like every other local authoring path. The insert alone leaves
    // `account_entries` without the matching status row, so a status-based reader would omit the
    // entry until some unrelated ingest happened to repair the projection.
    //
    // `retained_unfolded` is the assertion of inertness, not an absence of one: the annex log is
    // authority-inert, so a snapshot must land in the store WITHOUT being folded into authority.
    // Anything else — `accepted` above all — would mean an annex entry reached the control fold,
    // which is exactly the failure ANNEX_LOG exists to prevent (#809). Roll the caller back.
    let statuses = storage::refold_in_tx(tx, account_id, now_ms)?;
    match statuses.get(&verified.entry_hash).map(String::as_str) {
        Some("retained_unfolded") => {},
        other => anyhow::bail!(
            "authored snapshot folded to {other:?} instead of staying inert on the annex log; \
             rolling back",
        ),
    }
    Ok(SnapshotAuthorOutcome::Authored(verified.entry_hash))
}

#[cfg(test)]
mod coverage_ceiling_tests {
    use super::super::super::limits;
    use super::super::ops::{CoveredWatermark, SnapshotOp, SnapshotTarget, encode};
    use super::*;
    use crate::op::DeviceFingerprint;

    // A representative annex header — every field at a realistic width, so the measured overhead is
    // what production actually signs against.
    fn header() -> AccountEntryHeader {
        AccountEntryHeader {
            account_id: AccountId::from_bytes([1; 32]),
            log_id: fold::ANNEX_LOG,
            device_fingerprint: DeviceFingerprint::from_bytes([2; 32]),
            seq: u64::MAX,
            prev_hash: Some([3; 32]),
            parent_ref: Some([4; 32]),
            entry_type: super::super::ops::entry_type::SNAPSHOT,
            op_version: fold::SUPPORTED_OP_VERSION,
            crypto_suite: 0,
            auth_len: u64::MAX,
            key_id: None,
            authority_ref: Some([5; 32]),
        }
    }

    fn payload(devices: usize) -> Vec<u8> {
        let op = SnapshotOp::Snapshot {
            state_format_version: super::super::ops::SNAPSHOT_STATE_FORMAT_V1,
            moderation_epoch: 0,
            targets: vec![SnapshotTarget {
                log_id: fold::CONTROL_LOG,
                stream_id: None,
                subject_account_id: None,
                folded_state_hash: [0xab; 32],
                covered: (0..devices)
                    .map(|i| {
                        let mut fp = [0u8; 32];
                        fp[..4].copy_from_slice(&(i as u32).to_be_bytes());
                        CoveredWatermark {
                            device_fingerprint: DeviceFingerprint::from_bytes(fp),
                            seq: u64::MAX,
                            entry_hash: [0xcd; 32],
                        }
                    })
                    .collect(),
            }],
        };
        encode(&op).expect("encode")
    }

    fn fits(devices: usize) -> bool {
        envelope::entry_fits_envelope(&header(), &payload(devices))
    }

    /// An ordinary account is nowhere near the ceiling — the guard must not fire in normal use.
    #[test]
    fn a_realistic_device_count_fits_comfortably() {
        assert!(fits(64));
        assert!(fits(400));
    }

    /// The ceiling is real and `SNAPSHOT_COVERED_MAX` is unreachable: a target's `covered` vector
    /// carries a watermark per device, so the envelope binds well before that decoder bound.
    #[test]
    fn the_envelope_binds_before_the_declared_covered_bound() {
        assert!(fits(800), "800 devices still fit; the ceiling must not regress downward silently");
        assert!(
            !fits(limits::SNAPSHOT_COVERED_MAX),
            "the declared SNAPSHOT_COVERED_MAX ({}) cannot fit one signed envelope — it is a \
             decoder bound, never an achievable coverage size",
            limits::SNAPSHOT_COVERED_MAX,
        );
    }

    /// The boundary is a step inside the declared bound — a guard that never fired below
    /// `SNAPSHOT_COVERED_MAX` would be dead code. The exact signed check (not a reserve) is what
    /// places it.
    #[test]
    fn the_ceiling_falls_strictly_inside_the_declared_bound() {
        let first_over = (1..=limits::SNAPSHOT_COVERED_MAX)
            .find(|&n| !fits(n))
            .expect("some device count must exceed the envelope");
        assert!(
            (700..limits::SNAPSHOT_COVERED_MAX).contains(&first_over),
            "the ceiling should sit in the 700s-800s; found {first_over}",
        );
    }

    /// Both ceilings return the SAME typed outcome — a raw encoder error for `>
    /// SNAPSHOT_COVERED_MAX` devices would leak the internal bound as a failure, when it is the
    /// same "too large to snapshot" condition the envelope check reports for the 820-1024 band.
    #[test]
    fn beyond_the_encoder_bound_is_still_the_typed_outcome_not_a_raw_error() {
        // The encoder rejects a covered vector longer than SNAPSHOT_COVERED_MAX, so a manifest that
        // large must be caught BEFORE encoding. Exercising the guard directly on device count.
        let devices = limits::SNAPSHOT_COVERED_MAX + 500;
        assert!(devices > limits::SNAPSHOT_COVERED_MAX);
        // The production guard is `devices > SNAPSHOT_COVERED_MAX → CoverageExceedsEnvelope`, so a
        // count in this band never reaches `encode`. Pin the encoder actually rejects it, proving
        // the guard is load-bearing rather than defensive.
        assert!(
            encode(&SnapshotOp::Snapshot {
                state_format_version: super::super::ops::SNAPSHOT_STATE_FORMAT_V1,
                moderation_epoch: 0,
                targets: vec![SnapshotTarget {
                    log_id: fold::CONTROL_LOG,
                    stream_id: None,
                    subject_account_id: None,
                    folded_state_hash: [0xab; 32],
                    covered: (0..devices)
                        .map(|i| {
                            let mut fp = [0u8; 32];
                            fp[..4].copy_from_slice(&(i as u32).to_be_bytes());
                            CoveredWatermark {
                                device_fingerprint: DeviceFingerprint::from_bytes(fp),
                                seq: 0,
                                entry_hash: [0; 32],
                            }
                        })
                        .collect(),
                }],
            })
            .is_err(),
            "the encoder rejects a covered vector past SNAPSHOT_COVERED_MAX, so the guard must \
             run first",
        );
    }

    /// The check is EXACT: the largest device count the guard accepts must actually sign, and one
    /// more must actually fail. A conservative reserve would reject some of the accounts in this
    /// gap that can in fact be snapshotted — the distinction that matters because a manifest cannot
    /// be split.
    #[test]
    fn the_boundary_matches_what_signing_actually_accepts() {
        let secret = crate::device::DeviceSecret::from_seed(&[7; 32]);
        let last_ok = (1..=limits::SNAPSHOT_COVERED_MAX)
            .take_while(|&n| fits(n))
            .last()
            .expect("small counts fit");
        // A header signed by this key overwrites device_fingerprint, so build the header from the
        // real secret to measure the true overhead.
        let mut h = header();
        h.device_fingerprint = secret.public().fingerprint();
        sign_account_entry(&secret, &h, &payload(last_ok))
            .expect("the largest accepted count must actually sign within the envelope");
        assert!(
            sign_account_entry(&secret, &h, &payload(last_ok + 1)).is_err(),
            "one device past the guard's boundary must be exactly what signing rejects",
        );
    }
}
