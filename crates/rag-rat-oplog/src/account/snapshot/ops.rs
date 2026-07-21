//! The annex-log op payloads (`log_id = 3`) and their canonical-CBOR wire (C6, #609).
//!
//! The annex log is a THIRD account log at `log_id = ANNEX_LOG`, carrying authority-inert
//! bookkeeping artifacts. It shares the account-entry envelope ([`super::super::envelope`]) — its
//! ops ride as the opaque `payload` bstr, disambiguated by the header's `entry_type`, whose frozen
//! tag set here is a SEPARATE per-log namespace ([`entry_type`]) with fresh numbering: an annex tag
//! `0` is NOT control's `AccountGenesis`, and the log gate runs before any tag dispatch.
//!
//! **Why a separate log, not a control `entry_type`.** A snapshot must never be an EFFECTIVE op: an
//! effective one shifts `effective_count`, and every later entry asserting the higher `auth_len`
//! then parks `auth_len_ahead` un-healably on any binary that does not know the type. But a
//! never-effective entry cannot ride the control log either — control branch selection walks
//! EFFECTIVE entries with contiguous `seq`, so a non-effective entry mid-chain orphans every later
//! entry from that device (#809). The two requirements are only jointly satisfiable off the control
//! chain, where inertness is topological.
//!
//! Like the control and secrets ops, this layer owns STRUCTURAL wire validity only: arity,
//! canonicity (`encode(decode) == bytes`), the log/stream qualification coupling, sorted-unique
//! coverage, and the §18a bounds. Whether a snapshot's claim is TRUE — whether re-folding its
//! covered prefix reproduces `folded_state_hash` — is a read-time question and is deliberately not
//! answerable here: it depends on what this device holds, and an acceptance rule that reads local
//! inventory would make the verdict device-dependent.

use minicbor::Encoder;
use minicbor::decode::{Decoder, Error as CborError};

use super::super::AccountId;
use super::super::limits::{SNAPSHOT_COVERED_MAX, SNAPSHOT_TARGETS_MAX};
use super::super::ops::{decode_opt_b32, encode_opt_b32};
use crate::cbor;
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible) — mirrors `super::super`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// The frozen annex-log `entry_type` tag set (header part 7), a per-log namespace with fresh
/// numbering. A tag is a wire constant — a renumber is a wire bump.
pub(in crate::account) mod entry_type {
    /// A signed coverage claim over folded history (§4.7).
    pub(in crate::account) const SNAPSHOT: u32 = 0;
}

/// The canonical-projection encoding a `folded_state_hash` is taken over. A snapshot authored under
/// one version is unverifiable under another, so a change to what the fold projects (phase E's
/// `fold_exclude` alters the effective set) bumps this rather than silently invalidating every
/// stored manifest.
pub(in crate::account) const SNAPSHOT_STATE_FORMAT_V1: u32 = 1;

/// The account logs a snapshot target may name without stream/account qualification.
const CONTROL_LOG_ID: u8 = 0;
const SECRETS_LOG_ID: u8 = 1;
/// The content chain (`ChainKind::Content`). A content target is expressible on this wire so #406
/// adds content snapshots without a bump; nothing authors one this phase.
const CONTENT_LOG_ID: u8 = 2;

/// One `(device, seq, entry_hash)` coordinate a snapshot claims to cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct CoveredWatermark {
    pub(in crate::account) device_fingerprint: DeviceFingerprint,
    pub(in crate::account) seq: u64,
    pub(in crate::account) entry_hash: [u8; 32],
}

/// One log (or, from #406, one content stream) a snapshot covers, with the hash of the canonical
/// folded projection its author computed over exactly `covered`.
///
/// `stream_id` / `subject_account_id` are `None` for the account logs and `Some` for a content
/// stream: `/3` chains are dense per `(stream, author_account, device)`, so a bare `log_id` cannot
/// name a content coordinate. Carrying them now means #406 adds content snapshots without a wire
/// bump. Per-target hashes exist for the same reason — one hash cannot bind heterogeneous
/// projections (control authority state and content LWW state are not the same shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct SnapshotTarget {
    pub(in crate::account) log_id: u8,
    pub(in crate::account) stream_id: Option<StreamId>,
    pub(in crate::account) subject_account_id: Option<AccountId>,
    pub(in crate::account) folded_state_hash: [u8; 32],
    pub(in crate::account) covered: Vec<CoveredWatermark>,
}

/// The annex ops. One today; the log is named for the CLASS (authority-inert bookkeeping) rather
/// than the op, so the next inert artifact takes tag 1 instead of minting a fourth log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) enum SnapshotOp {
    Snapshot {
        state_format_version: u32,
        /// Reserved for the §4.4 moderation epoch (#407); MUST be 0 until `fold_exclude` exists.
        /// Reserved rather than omitted because the manifest is a signed wire and phase E is a
        /// certainty — one integer now is cheaper than a bump later.
        moderation_epoch: u64,
        targets: Vec<SnapshotTarget>,
    },
}

/// The result of decoding an annex payload against its header `entry_type`: a known op or a
/// retained opaque forward-version op (never an error for an unrecognized `entry_type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) enum DecodedSnapshotOp {
    Known(SnapshotOp),
    Unknown { entry_type: u32, bytes: Vec<u8> },
}

pub(in crate::account) fn entry_type_of(op: &SnapshotOp) -> u32 {
    match op {
        SnapshotOp::Snapshot { .. } => entry_type::SNAPSHOT,
    }
}

pub(in crate::account) fn encode(op: &SnapshotOp) -> Result<Vec<u8>, CborError> {
    Ok(encode_canonical(&canonicalize(op)?))
}

/// Validate + canonicalize so the encoding ALWAYS round-trips through [`decode`] — the authoring
/// path must never sign a payload the sorted-unique / bounded / self-consistent decoder (and peers)
/// would reject.
fn canonicalize(op: &SnapshotOp) -> Result<SnapshotOp, CborError> {
    match op {
        SnapshotOp::Snapshot { state_format_version, moderation_epoch, targets } => {
            check_state_format(*state_format_version)?;
            check_reserved_epoch(*state_format_version, *moderation_epoch)?;
            Ok(SnapshotOp::Snapshot {
                state_format_version: *state_format_version,
                moderation_epoch: *moderation_epoch,
                targets: canonical_targets(*state_format_version, targets)?,
            })
        },
    }
}

/// `moderation_epoch` is RESERVED at this state-format version: `fold_exclude` does not exist
/// (#407), so a non-zero epoch names coverage semantics nothing can evaluate. Enforced rather than
/// merely documented — an unenforced "MUST be 0" is just a comment, and this is a signed wire.
///
/// Scoped to the version that reserves it: a future format version defines its own rules, and a
/// binary that does not know that version must still be able to decode and retain the entry rather
/// than hard-rejecting a legitimate future manifest.
fn check_state_format(state_format_version: u32) -> Result<(), CborError> {
    // Below the first defined format there are no projection semantics at all, so such a manifest
    // could never be verified or refuted by any implementation, present or future. Reserve it.
    // Versions ABOVE the current one are a different case entirely — they are legitimately newer
    // and must be retained, not rejected.
    if state_format_version < SNAPSHOT_STATE_FORMAT_V1 {
        return Err(CborError::message("snapshot state_format_version 0 is reserved"));
    }
    Ok(())
}

fn check_reserved_epoch(state_format_version: u32, moderation_epoch: u64) -> Result<(), CborError> {
    if state_format_version == SNAPSHOT_STATE_FORMAT_V1 && moderation_epoch != 0 {
        return Err(CborError::message(
            "moderation_epoch is reserved and must be 0 at snapshot state format 1",
        ));
    }
    Ok(())
}

/// Two couplings are enforced here and mirrored in the decoder. (1) The account logs carry no
/// stream/account qualification and a content target MUST carry both — a bare `log_id` cannot name
/// a `/3` coordinate, and a half-qualified target would be ambiguous. (2) Coverage is a SET: a
/// target may name a device once, and the target list may name a `(log_id, stream_id, account_id)`
/// once. A byte-identical repeat is the SAME claim and canonicalizes away; two DIFFERENT claims
/// about one coordinate are contradictory and are refused rather than silently resolved.
fn canonical_targets(
    state_format_version: u32,
    targets: &[SnapshotTarget],
) -> Result<Vec<SnapshotTarget>, CborError> {
    if targets.len() > SNAPSHOT_TARGETS_MAX {
        return Err(CborError::message("snapshot targets exceeds the §18a bound"));
    }
    let mut sorted = Vec::with_capacity(targets.len());
    for target in targets {
        check_qualification(
            state_format_version,
            target.log_id,
            target.stream_id.is_some(),
            target.subject_account_id.is_some(),
        )?;
        if target.covered.len() > SNAPSHOT_COVERED_MAX {
            return Err(CborError::message("snapshot covered exceeds the §18a bound"));
        }
        let mut covered = target.covered.clone();
        covered.sort_by_key(|w| w.device_fingerprint.to_bytes());
        covered.dedup();
        if covered.windows(2).any(|pair| pair[0].device_fingerprint == pair[1].device_fingerprint) {
            return Err(CborError::message(
                "snapshot covered has conflicting entries for one device_fingerprint",
            ));
        }
        sorted.push(SnapshotTarget { covered, ..target.clone() });
    }
    sorted.sort_by_key(target_key);
    sorted.dedup();
    if sorted.windows(2).any(|pair| target_key(&pair[0]) == target_key(&pair[1])) {
        return Err(CborError::message("snapshot targets has conflicting entries for one log"));
    }
    Ok(sorted)
}

/// A target names exactly one of the three defined coordinate shapes. The `log_id` set is CLOSED
/// here on purpose: an unknown log has no defined folded projection, so a manifest claiming one
/// would be a coverage claim no implementation could ever verify or refute. Extending the set is a
/// deliberate `state_format_version` change, not something a peer may assert into existence.
fn check_qualification(
    state_format_version: u32,
    log_id: u8,
    has_stream: bool,
    has_account: bool,
) -> Result<(), CborError> {
    match log_id {
        CONTROL_LOG_ID | SECRETS_LOG_ID =>
            if has_stream || has_account {
                return Err(CborError::message(
                    "snapshot target for an account log must not carry stream_id/account_id",
                ));
            },
        CONTENT_LOG_ID =>
            if !has_stream || !has_account {
                return Err(CborError::message(
                    "snapshot target for the content log requires stream_id and account_id",
                ));
            },
        // Closing the set is a rule OF FORMAT 1, not a structural one: a later format may define
        // another targetable log, and an older binary must retain that manifest (it simply cannot
        // verify it) rather than structurally reject a legitimate newer artifact. Same scoping as
        // the reserved epoch above — applying it unconditionally here would contradict it.
        _ =>
            if state_format_version == SNAPSHOT_STATE_FORMAT_V1 {
                return Err(CborError::message(
                    "snapshot target names a log undefined at state format 1",
                ));
            },
    }
    Ok(())
}

/// The total order targets are sorted by — also their uniqueness key. Unqualified account logs sort
/// before any content target for the same `log_id` because `None` maps to all-zero bytes.
fn target_key(target: &SnapshotTarget) -> (u8, [u8; 32], [u8; 32]) {
    (
        target.log_id,
        target.stream_id.map(StreamId::to_bytes).unwrap_or([0u8; 32]),
        target.subject_account_id.map(AccountId::to_bytes).unwrap_or([0u8; 32]),
    )
}

fn encode_canonical(op: &SnapshotOp) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    {
        let mut enc = Encoder::new(&mut buf);
        match op {
            SnapshotOp::Snapshot { state_format_version, moderation_epoch, targets } => {
                enc.array(3).expect(INFALLIBLE);
                enc.u32(*state_format_version).expect(INFALLIBLE);
                enc.u64(*moderation_epoch).expect(INFALLIBLE);
                enc.array(targets.len() as u64).expect(INFALLIBLE);
                for target in targets {
                    enc.array(5).expect(INFALLIBLE);
                    enc.u8(target.log_id).expect(INFALLIBLE);
                    encode_opt_b32(&mut enc, target.stream_id.map(StreamId::to_bytes));
                    encode_opt_b32(&mut enc, target.subject_account_id.map(AccountId::to_bytes));
                    enc.bytes(&target.folded_state_hash).expect(INFALLIBLE);
                    enc.array(target.covered.len() as u64).expect(INFALLIBLE);
                    for w in &target.covered {
                        enc.array(3).expect(INFALLIBLE);
                        enc.bytes(&w.device_fingerprint.to_bytes()).expect(INFALLIBLE);
                        enc.u64(w.seq).expect(INFALLIBLE);
                        enc.bytes(&w.entry_hash).expect(INFALLIBLE);
                    }
                }
            },
        }
    }
    buf
}

pub(in crate::account) fn decode(
    entry_type: u32,
    bytes: &[u8],
) -> Result<DecodedSnapshotOp, CborError> {
    let op = match entry_type {
        entry_type::SNAPSHOT => decode_snapshot(bytes)?,
        other => {
            // A forward-version annex op is RETAINED opaque, but it must STILL be exactly one
            // canonical CBOR array — otherwise a future binary that learns this tag would run the
            // `encode(decode) == bytes` check below and reject bytes an older peer stored,
            // splitting consensus on a signed log. Mirrors the control and secrets
            // decoders.
            cbor::require_canonical_cbor(bytes)?;
            cbor::expect_definite_len(&mut Decoder::new(bytes))?;
            return Ok(DecodedSnapshotOp::Unknown { entry_type: other, bytes: bytes.to_vec() });
        },
    };
    // Canonicity guarantee for a KNOWN op: the decoded value must re-encode to the exact wire,
    // which rejects non-minimal ints, unsorted coverage, and trailing bytes in one check.
    if encode_canonical(&op) != bytes {
        return Err(CborError::message("annex op payload is not canonical"));
    }
    Ok(DecodedSnapshotOp::Known(op))
}

fn decode_snapshot(bytes: &[u8]) -> Result<SnapshotOp, CborError> {
    let mut d = Decoder::new(bytes);
    cbor::expect_array(&mut d, 3)?;
    let state_format_version = d.u32()?;
    let moderation_epoch = d.u64()?;
    // Enforced on BOTH sides: `canonicalize` guards authoring, but decode reaches
    // `encode_canonical` (the raw writer) directly for its canonicity compare, so a peer's bytes
    // would otherwise slip past the reserved-field rule.
    check_state_format(state_format_version)?;
    check_reserved_epoch(state_format_version, moderation_epoch)?;
    let len = cbor::expect_definite_len(&mut d)?;
    if len > SNAPSHOT_TARGETS_MAX as u64 {
        return Err(CborError::message("snapshot targets exceeds the §18a bound"));
    }
    let mut targets = Vec::with_capacity(len as usize);
    let mut prev_key: Option<(u8, [u8; 32], [u8; 32])> = None;
    for _ in 0..len {
        cbor::expect_array(&mut d, 5)?;
        let log_id = d.u8()?;
        let stream_id =
            decode_opt_b32(&mut d, "snapshot target stream_id")?.map(StreamId::from_bytes);
        let subject_account_id =
            decode_opt_b32(&mut d, "snapshot target account_id")?.map(AccountId::from_bytes);
        check_qualification(
            state_format_version,
            log_id,
            stream_id.is_some(),
            subject_account_id.is_some(),
        )?;
        let folded_state_hash = cbor::fixed_bytes::<32>(d.bytes()?, "folded_state_hash")?;
        let covered = decode_covered(&mut d)?;
        let target =
            SnapshotTarget { log_id, stream_id, subject_account_id, folded_state_hash, covered };
        let key = target_key(&target);
        // Strictly ascending ⇒ sorted AND unique in one check.
        if prev_key.is_some_and(|p| key <= p) {
            return Err(CborError::message("snapshot targets not sorted-unique"));
        }
        prev_key = Some(key);
        targets.push(target);
    }
    Ok(SnapshotOp::Snapshot { state_format_version, moderation_epoch, targets })
}

fn decode_covered(d: &mut Decoder<'_>) -> Result<Vec<CoveredWatermark>, CborError> {
    let len = cbor::expect_definite_len(d)?;
    if len > SNAPSHOT_COVERED_MAX as u64 {
        return Err(CborError::message("snapshot covered exceeds the §18a bound"));
    }
    let mut covered = Vec::with_capacity(len as usize);
    let mut prev: Option<[u8; 32]> = None;
    for _ in 0..len {
        cbor::expect_array(d, 3)?;
        let fp = cbor::fixed_bytes::<32>(d.bytes()?, "covered device_fingerprint")?;
        if prev.is_some_and(|p| fp <= p) {
            return Err(CborError::message("snapshot covered not sorted-unique by device"));
        }
        prev = Some(fp);
        let seq = d.u64()?;
        let entry_hash = cbor::fixed_bytes::<32>(d.bytes()?, "covered entry_hash")?;
        covered.push(CoveredWatermark {
            device_fingerprint: DeviceFingerprint::from_bytes(fp),
            seq,
            entry_hash,
        });
    }
    Ok(covered)
}

/// The ingest-time structural gate for an annex payload (the per-log twin of the control and
/// secrets validators): a known tag is fully decoded, an unknown tag is retained opaque.
pub(in crate::account) fn validate_storable_snapshot_payload(
    entry_type: u32,
    payload: &[u8],
) -> Result<(), CborError> {
    decode(entry_type, payload).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(byte: u8) -> StreamId {
        StreamId::from_bytes([byte; 32])
    }

    fn account(byte: u8) -> AccountId {
        AccountId::from_bytes([byte; 32])
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn snapshot(targets: Vec<SnapshotTarget>) -> SnapshotOp {
        SnapshotOp::Snapshot {
            state_format_version: SNAPSHOT_STATE_FORMAT_V1,
            moderation_epoch: 0,
            targets,
        }
    }

    fn account_target(log_id: u8, fps: &[u8]) -> SnapshotTarget {
        SnapshotTarget {
            log_id,
            stream_id: None,
            subject_account_id: None,
            folded_state_hash: [0x2a; 32],
            covered: fps
                .iter()
                .map(|fp| CoveredWatermark {
                    device_fingerprint: DeviceFingerprint::from_bytes([*fp; 32]),
                    seq: 1,
                    entry_hash: [0x1d; 32],
                })
                .collect(),
        }
    }

    fn sample() -> SnapshotOp {
        snapshot(vec![SnapshotTarget {
            folded_state_hash: [0x2a; 32],
            covered: vec![CoveredWatermark {
                device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
                seq: 12,
                entry_hash: [0x1d; 32],
            }],
            ..account_target(0, &[])
        }])
    }

    #[test]
    fn golden_snapshot() {
        // The coverage manifest is a signed wire: pin its exact bytes. A change here is a wire
        // bump, not a refactor.
        assert_eq!(hex(&encode(&sample()).unwrap()), "830100818500f6f658202a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a81835820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0c58201d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d");
    }

    #[test]
    fn an_annex_tag_is_its_own_namespace() {
        // Annex tag 0 is SNAPSHOT, not control's AccountGenesis. Decode is only ever reached with
        // `log_id == ANNEX_LOG`, so the numbering cannot collide across logs.
        assert_eq!(entry_type::SNAPSHOT, 0);
        assert_eq!(entry_type_of(&sample()), entry_type::SNAPSHOT);
    }

    #[test]
    fn snapshot_round_trips_every_target_shape() {
        let op = snapshot(vec![
            account_target(0, &[0x11, 0x22]),
            account_target(1, &[0x33]),
            SnapshotTarget {
                log_id: 2,
                stream_id: Some(stream(0x44)),
                subject_account_id: Some(account(0x55)),
                folded_state_hash: [0x66; 32],
                covered: vec![CoveredWatermark {
                    device_fingerprint: DeviceFingerprint::from_bytes([0x77; 32]),
                    seq: 9,
                    entry_hash: [0x88; 32],
                }],
            },
        ]);
        let bytes = encode(&op).unwrap();
        assert_eq!(decode(entry_type::SNAPSHOT, &bytes).unwrap(), DecodedSnapshotOp::Known(op));
    }

    #[test]
    fn snapshot_authoring_sorts_targets_and_covered() {
        // `encode` canonicalizes, so a caller handing them in any order signs the same bytes.
        let unsorted = snapshot(vec![account_target(1, &[0x33]), account_target(0, &[0x22, 0x11])]);
        let sorted = snapshot(vec![account_target(0, &[0x11, 0x22]), account_target(1, &[0x33])]);
        assert_eq!(encode(&unsorted).unwrap(), encode(&sorted).unwrap());
    }

    #[test]
    fn snapshot_rejects_conflicting_coverage_claims() {
        // Coverage is a SET, so a byte-identical repeat is the SAME claim and canonicalizes away.
        let repeated = snapshot(vec![account_target(0, &[0x11, 0x11])]);
        let once = snapshot(vec![account_target(0, &[0x11])]);
        assert_eq!(
            encode(&repeated).unwrap(),
            encode(&once).unwrap(),
            "an identical repeat is one claim, not a conflict",
        );

        // Two DIFFERENT watermarks for one device are contradictory claims about the same
        // coordinate — never something to merge, and never something to silently pick between.
        let mut conflicting = account_target(0, &[0x11]);
        conflicting.covered.push(CoveredWatermark {
            device_fingerprint: DeviceFingerprint::from_bytes([0x11; 32]),
            seq: 99,
            entry_hash: [0xee; 32],
        });
        assert!(
            encode(&snapshot(vec![conflicting])).is_err(),
            "one device may be covered at only one coordinate per target",
        );

        let dup_target = snapshot(vec![account_target(0, &[0x11]), account_target(0, &[0x22])]);
        assert!(encode(&dup_target).is_err(), "one log may be claimed once per snapshot");
    }

    #[test]
    fn snapshot_rejects_a_half_qualified_target() {
        let account_log_with_stream = snapshot(vec![SnapshotTarget {
            stream_id: Some(stream(0x44)),
            ..account_target(0, &[0x11])
        }]);
        assert!(encode(&account_log_with_stream).is_err(), "an account log carries no stream_id");
        let content_without_account = snapshot(vec![SnapshotTarget {
            log_id: 2,
            stream_id: Some(stream(0x44)),
            ..account_target(2, &[0x11])
        }]);
        assert!(encode(&content_without_account).is_err(), "a content target needs both ids");
    }

    #[test]
    fn snapshot_decoder_rejects_unsorted_wire_bytes() {
        // The sorted-unique rule must be enforced on DECODE, not just authoring: a peer may hand us
        // bytes our own encoder would never emit, and `encode(decode) == bytes` is what keeps a
        // signed log from splitting on ordering.
        let sorted = encode(&snapshot(vec![account_target(0, &[0x11, 0x22])])).unwrap();
        let mut swapped = sorted.clone();
        let first = swapped.iter().position(|b| *b == 0x11).expect("first covered fingerprint");
        let second = swapped.iter().position(|b| *b == 0x22).expect("second covered fingerprint");
        for offset in 0..32 {
            swapped.swap(first + offset, second + offset);
        }
        assert_ne!(swapped, sorted);
        assert!(decode(entry_type::SNAPSHOT, &swapped).is_err(), "descending order is rejected");
    }

    #[test]
    fn snapshot_rejects_bounds_violations() {
        let too_many_covered = snapshot(vec![SnapshotTarget {
            covered: (0..=SNAPSHOT_COVERED_MAX)
                .map(|i| CoveredWatermark {
                    device_fingerprint: DeviceFingerprint::from_bytes(fp_bytes(i)),
                    seq: 1,
                    entry_hash: [0x1d; 32],
                })
                .collect(),
            ..account_target(0, &[])
        }]);
        assert!(encode(&too_many_covered).is_err(), "§18a bounds the covered array");
    }

    #[test]
    fn an_unknown_annex_tag_is_retained_not_rejected() {
        // Forward-compat: an annex tag this binary does not know stays a valid, chainable entry.
        let payload = vec![0x81, 0x01];
        assert_eq!(decode(7, &payload).unwrap(), DecodedSnapshotOp::Unknown {
            entry_type: 7,
            bytes: payload
        });
        // ...but it must still be exactly one canonical CBOR array, or a future binary that learns
        // the tag would reject bytes this one stored.
        assert!(decode(7, &[0xa1, 0x01, 0x02]).is_err(), "a non-array annex payload is refused");
        assert!(decode(7, &[0x81, 0x01, 0xff]).is_err(), "trailing bytes are refused");
    }

    #[test]
    fn the_reserved_moderation_epoch_is_enforced_not_merely_documented() {
        // `fold_exclude` does not exist (#407), so a non-zero epoch at format 1 names coverage
        // semantics nothing can evaluate. An unenforced "MUST be 0" is just a comment.
        let nonzero = SnapshotOp::Snapshot {
            state_format_version: SNAPSHOT_STATE_FORMAT_V1,
            moderation_epoch: 1,
            targets: vec![account_target(0, &[0x11])],
        };
        assert!(encode(&nonzero).is_err(), "authoring refuses a reserved-field value");

        // Wire bytes a peer hands us are refused the same way...
        let mut bytes = encode(&snapshot(vec![account_target(0, &[0x11])])).unwrap();
        assert_eq!(bytes[2], 0x00, "moderation_epoch is the third header item");
        bytes[2] = 0x01;
        assert!(decode(entry_type::SNAPSHOT, &bytes).is_err(), "decoding refuses it too");

        // ...but the rule is scoped to the version that reserves it: a FUTURE state format defines
        // its own semantics, and this binary must retain such an entry rather than hard-reject a
        // legitimate future manifest it simply cannot verify.
        let future = SnapshotOp::Snapshot {
            state_format_version: SNAPSHOT_STATE_FORMAT_V1 + 1,
            moderation_epoch: 7,
            targets: vec![account_target(0, &[0x11])],
        };
        let future_bytes = encode(&future).expect("a future format version still encodes");
        assert_eq!(
            decode(entry_type::SNAPSHOT, &future_bytes).unwrap(),
            DecodedSnapshotOp::Known(future),
        );
    }

    #[test]
    fn an_undefined_target_log_is_refused_at_format_1_but_retained_at_a_future_format() {
        // At format 1 the log set is CLOSED: an unknown log has no defined folded projection, so a
        // coverage claim about it could never be verified or refuted. That includes the annex log
        // itself — a snapshot does not cover snapshots.
        for undefined in [fold_annex_log(), 4, 200] {
            let target = SnapshotTarget {
                log_id: undefined,
                stream_id: Some(stream(0x44)),
                subject_account_id: Some(account(0x55)),
                ..account_target(0, &[0x11])
            };
            assert!(
                encode(&snapshot(vec![target])).is_err(),
                "log {undefined} has no projection defined at format 1",
            );
        }

        // But closing the set is a rule OF FORMAT 1. A later format may define another targetable
        // log, and this binary must RETAIN that manifest rather than structurally reject a
        // legitimate newer artifact it merely cannot verify — the same scoping as the reserved
        // epoch. Rejecting here unconditionally would contradict that.
        let future = SnapshotOp::Snapshot {
            state_format_version: SNAPSHOT_STATE_FORMAT_V1 + 1,
            moderation_epoch: 0,
            targets: vec![SnapshotTarget {
                log_id: 200,
                stream_id: Some(stream(0x44)),
                subject_account_id: Some(account(0x55)),
                ..account_target(0, &[0x11])
            }],
        };
        let bytes = encode(&future).expect("a future format may name a log this one does not know");
        assert_eq!(decode(entry_type::SNAPSHOT, &bytes).unwrap(), DecodedSnapshotOp::Known(future));
    }

    #[test]
    fn state_format_version_zero_is_reserved() {
        // Below the first defined format there are no projection semantics at all, so a version-0
        // manifest is a signed claim nothing could ever evaluate. Reserved on both paths — and
        // distinct from a FUTURE version, which is legitimately newer and must be retained.
        let undefined = SnapshotOp::Snapshot {
            state_format_version: 0,
            moderation_epoch: 0,
            targets: vec![account_target(0, &[0x11])],
        };
        assert!(encode(&undefined).is_err(), "authoring refuses the reserved version");

        let mut bytes = encode(&snapshot(vec![account_target(0, &[0x11])])).unwrap();
        assert_eq!(bytes[1], 0x01, "state_format_version is the second header item");
        bytes[1] = 0x00;
        assert!(decode(entry_type::SNAPSHOT, &bytes).is_err(), "decoding refuses it too");
    }

    /// The annex log's own id, spelled locally so this module stays free of a `fold` dependency.
    fn fold_annex_log() -> u8 {
        3
    }

    /// Distinct 32-byte fingerprints for bound tests.
    fn fp_bytes(i: usize) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[..8].copy_from_slice(&(i as u64).to_be_bytes());
        out
    }
}
