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

use super::super::control_v2::views;
use super::super::envelope::{self, AccountEntryHeader, VerifiedAccountEntry, sign_account_entry};
use super::super::fold::{self, AccountClassification, EntryStatus};
use super::super::id::{AccountEntryHash, OwnerId};
use super::super::storage::{self, CandidateInsert};
use super::super::{AccountId, authoring, limits};
use super::ops::{AnnexOp, SnapshotTarget};
use super::{projection, verify};
use crate::identity::LocalDevice;

/// What an authoring attempt did. Only `Authored` mints an entry; the rest are ordinary states this
/// device can be in, reported so a caller can tell "nothing to do" from "something went wrong".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotAuthorOutcome {
    Authored(AccountEntryHash),
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
    let manifest = AnnexOp::Snapshot {
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

    let authored = author_annex_entry_in_tx(tx, AnnexEntry {
        device,
        account_id,
        entry_type: super::ops::entry_type::SNAPSHOT,
        payload,
        // A snapshot binds the account root it claims coverage over and cites the incarnation its
        // USABILITY is scoped to; `usable_snapshots` reads both back.
        parent_ref: Some(genesis_hash),
        authority_ref: Some(owner_id),
        auth_len: storage::account_effective_count(tx, account_id)?,
        now_ms,
    })?;
    Ok(match authored {
        AnnexAuthored::Authored(entry_hash) => SnapshotAuthorOutcome::Authored(entry_hash),
        // The second of the two ceilings, and the same meaning as the first: too many devices to
        // name in one manifest. It is exact rather than a reserve because a manifest cannot be
        // split — `folded_state_hash` commits to the prefix its covered vector defines — so an
        // account in the gap between a conservative reserve and the real overhead must not be told
        // it cannot snapshot when it can.
        AnnexAuthored::TooLargeToSign => SnapshotAuthorOutcome::CoverageExceedsEnvelope { devices },
    })
}

/// One entry on this device's own annex chain: the payload, its tag, and the two header slots an
/// artifact class may or may not have anything to put in.
pub(in crate::account) struct AnnexEntry<'a> {
    pub(in crate::account) device: &'a LocalDevice,
    pub(in crate::account) account_id: AccountId,
    pub(in crate::account) entry_type: u32,
    pub(in crate::account) payload: Vec<u8>,
    /// The account root, for an artifact that binds one. Nothing on this log revalidates
    /// `parent_ref`, so `None` states "this artifact names no root" rather than omitting one.
    pub(in crate::account) parent_ref: Option<AccountEntryHash>,
    /// The owner incarnation, for an artifact whose USABILITY is scoped to one staying open.
    pub(in crate::account) authority_ref: Option<OwnerId>,
    pub(in crate::account) auth_len: u64,
    pub(in crate::account) now_ms: i64,
}

/// What authoring one annex entry did. `TooLargeToSign` is a fact about the payload's size, which
/// each artifact class reports in its own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) enum AnnexAuthored {
    Authored(AccountEntryHash),
    TooLargeToSign,
}

/// The authoring sequence every annex artifact shares: chain tail → header → envelope fit → sign →
/// insert → refold → PROVE the entry stayed inert.
///
/// Neither of the last two steps is bookkeeping. The insert alone leaves `account_entries` without
/// the matching status row, so a status-based reader would omit the entry until some unrelated
/// ingest happened to repair the projection. And `retained_unfolded` is the assertion of inertness,
/// not an absence of one: the annex log is authority-inert, so an entry must land WITHOUT being
/// folded into authority. Anything else — `accepted` above all — would mean an annex entry reached
/// the control fold, which is exactly the failure ANNEX_LOG exists to prevent (#809). Roll the
/// caller back.
pub(in crate::account) fn author_annex_entry_in_tx(
    tx: &Transaction<'_>,
    entry: AnnexEntry<'_>,
) -> anyhow::Result<AnnexAuthored> {
    let fingerprint = entry.device.fingerprint();
    // The annex chain is this device's own, independent of its control chain — that separation is
    // the whole reason the log exists (#809).
    let (seq, prev_hash) =
        match authoring::account_chain_tail(tx, entry.account_id, fingerprint, fold::ANNEX_LOG)? {
            Some((tail_seq, tail_hash)) => (
                tail_seq.checked_add(1).context("annex chain tail is at u64::MAX seq")?,
                Some(tail_hash),
            ),
            None => (0, None),
        };

    let header = AccountEntryHeader {
        account_id: entry.account_id,
        log_id: fold::ANNEX_LOG,
        device_fingerprint: fingerprint,
        seq,
        prev_hash,
        parent_ref: entry.parent_ref,
        entry_type: entry.entry_type,
        op_version: fold::SUPPORTED_OP_VERSION,
        // Plaintext is structural for this log's artifacts: their whole value is that a peer can
        // read them without decrypting anything, and ingest refuses a sealed snapshot outright.
        crypto_suite: 0,
        auth_len: entry.auth_len,
        key_id: None,
        authority_ref: entry.authority_ref,
    };
    // Check BEFORE signing, against the EXACT signed size for this header — not a reserve — so an
    // over-size payload surfaces as a typed outcome instead of a raw envelope rejection from inside
    // `sign_account_entry`, which reads as a bug in authoring rather than what it is.
    if !envelope::entry_fits_envelope(&header, &entry.payload) {
        return Ok(AnnexAuthored::TooLargeToSign);
    }
    let signed = sign_account_entry(entry.device.secret(), &header, &entry.payload)?;
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    match storage::insert_candidate(tx, &verified, &signed.signed_bytes, entry.now_ms)? {
        CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {},
        CandidateInsert::AtCapacity(scope) => anyhow::bail!(
            "the account candidate store is at capacity ({scope:?}); cannot author an annex entry",
        ),
    }

    let statuses = storage::refold_in_tx(tx, entry.account_id, entry.now_ms)?;
    match statuses.get(&verified.entry_hash).copied() {
        Some(EntryStatus::RetainedUnfolded) => {},
        other => {
            let other = other.map(EntryStatus::as_db_str);
            anyhow::bail!(
                "authored annex entry folded to {other:?} instead of staying inert on the annex \
                 log; rolling back",
            )
        },
    }
    Ok(AnnexAuthored::Authored(verified.entry_hash))
}

/// Store one control-v2 pre-cut view, so the cut that names its digest can be verified from held
/// rows alone rather than from a bundle a peer happens to attach.
///
/// **Author it from the device that authors the cut.** A manifest signed by a device the account
/// does not certify parks in `account_pre_verify`, which is capped per account and evicts
/// oldest-first — evidence a cut depends on permanently would then be evictable, and the cut would
/// return to `ParkCause::Manifest` long after it applied. Same-author is what makes the manifest
/// exactly as durable as the cut: whenever the cut is storable, its evidence is too.
///
/// Unlike its snapshot sibling on this log, the entry names no root and no incarnation. That is
/// deliberate rather than an omission: a manifest's integrity is the digest the cut signed, so
/// nothing reads either field — and carrying one would invite a later reader to gate on it, which
/// would make a revoked author's manifest vanish and re-park a cut that had already applied.
#[allow(
    dead_code,
    reason = "no production path authors a v2 cut yet — pin install is test-only (#1311)"
)]
pub(in crate::account) fn author_view_manifest_in_tx(
    tx: &Transaction<'_>,
    device: &LocalDevice,
    account_id: AccountId,
    view: &views::ViewManifest,
    now_ms: i64,
) -> anyhow::Result<AccountEntryHash> {
    // VERBATIM: a cut names its evidence by `sha256` of exactly these bytes.
    let payload = view.encode()?;
    match author_annex_entry_in_tx(tx, AnnexEntry {
        device,
        account_id,
        entry_type: super::ops::entry_type::VIEW_MANIFEST,
        payload,
        parent_ref: None,
        authority_ref: None,
        auth_len: 0,
        now_ms,
    })? {
        AnnexAuthored::Authored(entry_hash) => Ok(entry_hash),
        // Unreachable while `views::MAX_VIEW_ENTRIES` stays under what one envelope carries, which
        // is why that bound is declared in envelope terms rather than in the planner's.
        AnnexAuthored::TooLargeToSign => anyhow::bail!(
            "a view naming {} identities does not fit one signed annex entry; \
             views::MAX_VIEW_ENTRIES sits above the envelope ceiling",
            view.entries.len(),
        ),
    }
}

#[cfg(test)]
mod coverage_ceiling_tests {
    use super::super::super::id::OwnerId;
    use super::super::super::limits;
    use super::super::ops::{AnnexOp, CoveredWatermark, SnapshotTarget, encode};
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
            prev_hash: Some(AccountEntryHash::from_bytes([3; 32])),
            parent_ref: Some(AccountEntryHash::from_bytes([4; 32])),
            entry_type: super::super::ops::entry_type::SNAPSHOT,
            op_version: fold::SUPPORTED_OP_VERSION,
            crypto_suite: 0,
            auth_len: u64::MAX,
            key_id: None,
            authority_ref: Some(OwnerId::from_bytes([5; 32])),
        }
    }

    fn payload(devices: usize) -> Vec<u8> {
        let op = AnnexOp::Snapshot {
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
                            entry_hash: AccountEntryHash::from_bytes([0xcd; 32]),
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
            encode(&AnnexOp::Snapshot {
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
                                entry_hash: AccountEntryHash::from_bytes([0; 32]),
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

    /// `MAX_VIEW_ENTRIES` is a promise that a view that large can actually be SIGNED, so measure
    /// the signed envelope rather than the payload — the payload is the smaller of the two.
    ///
    /// The header measured is the one [`author_view_manifest_in_tx`] actually writes, in BOTH
    /// shapes it can take, because the production case is the expensive one: a manifest names no
    /// root and no incarnation, but it does chain, so every manifest after a device's first
    /// carries a `prev_hash` and a grown `seq`/`auth_len`. Measuring only the origin shape would
    /// leave the question a reader actually has — does a real chained manifest still fit? —
    /// answerable only by building that header by hand.
    #[test]
    fn a_view_at_the_declared_bound_fits_one_signed_annex_entry() {
        let view = |entries: usize| views::ViewManifest {
            checkpoint: [0xaa; 32],
            entries: (0..entries)
                .map(|i| {
                    let mut hash = [0u8; 32];
                    hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
                    AccountEntryHash::from_bytes(hash)
                })
                .collect(),
        };
        let manifest_header = |linked: bool| AccountEntryHeader {
            seq: if linked { u64::MAX } else { 0 },
            prev_hash: linked.then(|| AccountEntryHash::from_bytes([3; 32])),
            parent_ref: None,
            entry_type: super::super::ops::entry_type::VIEW_MANIFEST,
            auth_len: if linked { u64::MAX } else { 0 },
            authority_ref: None,
            ..header()
        };
        let payload = view(views::MAX_VIEW_ENTRIES).encode().expect("the declared bound encodes");
        assert!(view(views::MAX_VIEW_ENTRIES + 1).encode().is_err(), "one past it is refused");

        let (origin, chained) = (manifest_header(false), manifest_header(true));
        assert!(
            envelope::entry_fits_envelope(&chained, &payload),
            "a view at the declared bound must actually sign, on a CHAINED manifest and not only \
             on a device's first",
        );
        let spare = |header: &AccountEntryHeader| {
            limits::ACCOUNT_ENVELOPE_MAX_BYTES - envelope::signed_entry_len(header, &payload)
        };
        let (origin_spare, chained_spare) = (spare(&origin), spare(&chained));
        assert!(chained_spare < origin_spare, "chaining a manifest costs envelope, not saves it");

        // Two bounds, because either one alone answers nothing. The UPPER bound is measured on the
        // loosest shape: if even a device's FIRST manifest has this little room, the declared bound
        // really does sit just under the ceiling and is not arbitrarily cautious — lowering the
        // constant would silently shrink what a cut may name.
        assert!(
            origin_spare < 34 * 64,
            "the declared bound should sit just under the real ceiling; origin {origin_spare}, \
             chained {chained_spare}",
        );
        // The LOWER bound is measured on the TIGHTEST shape, and it is the margin a new header
        // field would have to eat before the declared bound became unsignable in production. A
        // 32-byte field costs ~34 bytes on this wire, so this leaves room for about three of them;
        // a change that eats past it has to bring `MAX_VIEW_ENTRIES` down with it rather than
        // discover the ceiling at signing time.
        assert!(
            chained_spare > 128,
            "a chained manifest at the declared bound has too little margin left; origin \
             {origin_spare}, chained {chained_spare}",
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
