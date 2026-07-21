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
//! # The version boundary — read this before adding a rule
//!
//! Every validation rule on this wire belongs to exactly one of two categories, and putting a rule
//! in the wrong one is a real bug in either direction: too broad silently breaks forward
//! compatibility (an older binary hard-rejects a legitimate newer manifest), too narrow admits
//! entries no implementation could ever evaluate.
//!
//! Rather than ask every future rule author to get that right, the boundary is STRUCTURAL and
//! decided in exactly one place ([`decode`]):
//!
//! - **Class invariants** — true of the artifact at every format version — live in THIS module.
//!   There are deliberately few: the framing (a definite-length array whose first element is the
//!   format version) and that version 0 is reserved. Plus one in the ingest layer: a snapshot is
//!   plaintext-signed.
//! - **Everything else** is a rule of ONE format and lives in [`format_v1`]. An unknown format is
//!   never decoded at all — it is retained opaque, exactly as an unknown `entry_type` is — so a
//!   rule inside `format_v1` *cannot* be version-conditional: `format_v1::canonicalize` refuses
//!   anything but format 1, making the version provably constant in there.
//!
//! Adding a manifest field? It is a `format_v1` rule unless you can argue it holds for formats that
//! do not exist yet. The decision table is pinned by `the_version_decision_table_is_total` — extend
//! that test rather than reasoning about it fresh.
//!
//! Like the control and secrets ops, this layer owns STRUCTURAL wire validity only. Whether a
//! snapshot's claim is TRUE — whether re-folding its covered prefix reproduces `folded_state_hash`
//! — is a read-time question and is deliberately not answerable here: it depends on what this
//! device holds, and an acceptance rule that reads local inventory would make the verdict
//! device-dependent.

use minicbor::decode::{Decoder, Error as CborError};

use super::super::AccountId;
use crate::cbor;
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

/// The frozen annex-log `entry_type` tag set (header part 7), a per-log namespace with fresh
/// numbering. A tag is a wire constant — a renumber is a wire bump.
pub(in crate::account) mod entry_type {
    /// A signed coverage claim over folded history (§4.7).
    pub(in crate::account) const SNAPSHOT: u32 = 0;
}

/// The canonical-projection encoding a `folded_state_hash` is taken over, and the only format this
/// binary can author or interpret. A snapshot authored under one version is unverifiable under
/// another, so a change to what the fold projects (phase E's `fold_exclude` alters the effective
/// set) bumps this rather than silently invalidating every stored manifest.
pub(in crate::account) const SNAPSHOT_STATE_FORMAT_V1: u32 = 1;

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
        /// Reserved for the §4.4 moderation epoch (#407); MUST be 0 at format 1. Reserved rather
        /// than omitted because the manifest is a signed wire and phase E is a certainty — one
        /// integer now is cheaper than a bump later.
        moderation_epoch: u64,
        targets: Vec<SnapshotTarget>,
    },
}

/// The result of decoding an annex payload. Three outcomes, and only the first is interpreted:
/// everything this binary cannot interpret is RETAINED rather than rejected, so it stays a valid,
/// chainable entry for the peer or future binary that can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) enum DecodedSnapshotOp {
    Known(SnapshotOp),
    /// A tag this binary does not know (forward-compat within the annex log).
    UnknownTag {
        entry_type: u32,
        bytes: Vec<u8>,
    },
    /// The snapshot tag at a format version this binary does not implement. Deliberately NOT
    /// decoded: interpreting a newer format through this one's rules is how an older binary comes
    /// to reject a manifest a newer one considers perfectly valid.
    FutureFormat {
        state_format_version: u32,
        bytes: Vec<u8>,
    },
}

pub(in crate::account) fn entry_type_of(op: &SnapshotOp) -> u32 {
    match op {
        SnapshotOp::Snapshot { .. } => entry_type::SNAPSHOT,
    }
}

/// Author a payload. This binary authors only the format it implements — signing a version it
/// cannot itself validate would be signing a claim it cannot check.
pub(in crate::account) fn encode(op: &SnapshotOp) -> Result<Vec<u8>, CborError> {
    Ok(format_v1::encode_canonical(&format_v1::canonicalize(op)?))
}

/// The framing every snapshot format shares, and the ONLY thing read before the version dispatch: a
/// definite-length array whose first element is the format version.
///
/// Deliberately does not pin the arity — a later format may carry more fields, and freezing three
/// here would be exactly the over-broad mistake this boundary exists to prevent.
fn peek_state_format_version(bytes: &[u8]) -> Result<u32, CborError> {
    cbor::require_canonical_cbor(bytes)?;
    let mut d = Decoder::new(bytes);
    if cbor::expect_definite_len(&mut d)? == 0 {
        return Err(CborError::message("snapshot payload carries no state_format_version"));
    }
    d.u32()
}

pub(in crate::account) fn decode(
    entry_type: u32,
    bytes: &[u8],
) -> Result<DecodedSnapshotOp, CborError> {
    if entry_type != entry_type::SNAPSHOT {
        // A forward-version annex op is RETAINED opaque, but it must STILL be exactly one canonical
        // CBOR array — otherwise a future binary that learns this tag would run the
        // `encode(decode) == bytes` check below and reject bytes an older peer stored, splitting
        // consensus on a signed log. Mirrors the control and secrets decoders.
        cbor::require_canonical_cbor(bytes)?;
        cbor::expect_definite_len(&mut Decoder::new(bytes))?;
        return Ok(DecodedSnapshotOp::UnknownTag { entry_type, bytes: bytes.to_vec() });
    }

    // ---- the entire version boundary, in one place ----
    let state_format_version = peek_state_format_version(bytes)?;
    if state_format_version < SNAPSHOT_STATE_FORMAT_V1 {
        // Below the first defined format there are no projection semantics at all, so such a
        // manifest is a signed claim nothing could ever evaluate. A class invariant, and the only
        // rejecting one here: reserved, not "future".
        return Err(CborError::message("snapshot state_format_version 0 is reserved"));
    }
    if state_format_version != SNAPSHOT_STATE_FORMAT_V1 {
        return Ok(DecodedSnapshotOp::FutureFormat { state_format_version, bytes: bytes.to_vec() });
    }

    let op = format_v1::decode(bytes)?;
    // Canonicity guarantee for an INTERPRETED op: the decoded value must re-encode to the exact
    // wire, which rejects non-minimal ints, unsorted coverage, and trailing bytes in one check.
    if format_v1::encode_canonical(&op) != bytes {
        return Err(CborError::message("annex op payload is not canonical"));
    }
    Ok(DecodedSnapshotOp::Known(op))
}

/// The ingest-time structural gate for an annex payload (the per-log twin of the control and
/// secrets validators): an interpretable payload is fully decoded, and anything else must still be
/// well-framed to be storable.
pub(in crate::account) fn validate_storable_snapshot_payload(
    entry_type: u32,
    payload: &[u8],
) -> Result<(), CborError> {
    decode(entry_type, payload).map(|_| ())
}

/// Format 1 of the snapshot manifest. Every rule in here is unconditional BY CONSTRUCTION: the
/// module is only ever reached for format 1, and [`canonicalize`] refuses anything else, so
/// `state_format_version` is provably constant within it. There is no scope for a rule here to
/// choose, which is the point — see the version-boundary section in the module header.
mod format_v1 {
    use minicbor::Encoder;
    use minicbor::decode::{Decoder, Error as CborError};

    use super::super::super::AccountId;
    use super::super::super::limits::{SNAPSHOT_COVERED_MAX, SNAPSHOT_TARGETS_MAX};
    use super::super::super::ops::{decode_opt_b32, encode_opt_b32};
    use super::{CoveredWatermark, SNAPSHOT_STATE_FORMAT_V1, SnapshotOp, SnapshotTarget};
    use crate::cbor;
    use crate::op::DeviceFingerprint;
    use crate::stream::StreamId;

    /// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible).
    const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

    /// The three coordinate shapes a format-1 target may name. The set is CLOSED: an undefined log
    /// has no folded projection, so a coverage claim about it could never be verified or refuted. A
    /// later format may define more — which is exactly why an unknown format never reaches here.
    const CONTROL_LOG_ID: u8 = 0;
    const SECRETS_LOG_ID: u8 = 1;
    const CONTENT_LOG_ID: u8 = 2;

    /// Validate + canonicalize so the encoding ALWAYS round-trips through [`decode`] — the
    /// authoring path must never sign a payload the sorted-unique / bounded / self-consistent
    /// decoder (and peers) would reject.
    ///
    /// The version check here is what makes every other rule in this module unconditional.
    pub(super) fn canonicalize(op: &SnapshotOp) -> Result<SnapshotOp, CborError> {
        match op {
            SnapshotOp::Snapshot { state_format_version, moderation_epoch, targets } => {
                if *state_format_version != SNAPSHOT_STATE_FORMAT_V1 {
                    return Err(CborError::message(
                        "this binary authors only snapshot state format 1",
                    ));
                }
                check_reserved_epoch(*moderation_epoch)?;
                Ok(SnapshotOp::Snapshot {
                    state_format_version: *state_format_version,
                    moderation_epoch: *moderation_epoch,
                    targets: canonical_targets(targets)?,
                })
            },
        }
    }

    /// `fold_exclude` does not exist (#407), so a non-zero epoch names coverage semantics nothing
    /// can evaluate. Enforced on both the authoring and decoding paths — an unenforced "MUST be 0"
    /// is just a comment, and this is a signed wire.
    fn check_reserved_epoch(moderation_epoch: u64) -> Result<(), CborError> {
        if moderation_epoch != 0 {
            return Err(CborError::message(
                "moderation_epoch is reserved and must be 0 at snapshot state format 1",
            ));
        }
        Ok(())
    }

    /// Coverage is a SET: a target may name a device once, and the target list may name a
    /// `(log_id, stream_id, account_id)` once. A byte-identical repeat is the SAME claim and
    /// canonicalizes away; two DIFFERENT claims about one coordinate are contradictory and are
    /// refused rather than silently resolved.
    fn canonical_targets(targets: &[SnapshotTarget]) -> Result<Vec<SnapshotTarget>, CborError> {
        if targets.len() > SNAPSHOT_TARGETS_MAX {
            return Err(CborError::message("snapshot targets exceeds the §18a bound"));
        }
        let mut sorted = Vec::with_capacity(targets.len());
        for target in targets {
            check_qualification(
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
            if covered
                .windows(2)
                .any(|pair| pair[0].device_fingerprint == pair[1].device_fingerprint)
            {
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

    /// The account logs carry no stream/account qualification; the content log requires both. A
    /// half-qualified target would be ambiguous, and an undefined log is refused outright.
    fn check_qualification(
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
            _ => return Err(CborError::message("snapshot target names an undefined log_id")),
        }
        Ok(())
    }

    /// The total order targets are sorted by — also their uniqueness key. Unqualified account logs
    /// sort before any content target for the same `log_id` because `None` maps to all-zero bytes.
    fn target_key(target: &SnapshotTarget) -> (u8, [u8; 32], [u8; 32]) {
        (
            target.log_id,
            target.stream_id.map(StreamId::to_bytes).unwrap_or([0u8; 32]),
            target.subject_account_id.map(AccountId::to_bytes).unwrap_or([0u8; 32]),
        )
    }

    pub(super) fn encode_canonical(op: &SnapshotOp) -> Vec<u8> {
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
                        encode_opt_b32(
                            &mut enc,
                            target.subject_account_id.map(AccountId::to_bytes),
                        );
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

    pub(super) fn decode(bytes: &[u8]) -> Result<SnapshotOp, CborError> {
        let mut d = Decoder::new(bytes);
        cbor::expect_array(&mut d, 3)?;
        let state_format_version = d.u32()?;
        let moderation_epoch = d.u64()?;
        check_reserved_epoch(moderation_epoch)?;
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
            check_qualification(log_id, stream_id.is_some(), subject_account_id.is_some())?;
            let folded_state_hash = cbor::fixed_bytes::<32>(d.bytes()?, "folded_state_hash")?;
            let covered = decode_covered(&mut d)?;
            let target = SnapshotTarget {
                log_id,
                stream_id,
                subject_account_id,
                folded_state_hash,
                covered,
            };
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
}

#[cfg(test)]
mod tests {
    use minicbor::Encoder;

    use super::super::super::limits::SNAPSHOT_COVERED_MAX;
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

    /// A payload this binary CANNOT author: a peer's manifest at some other format version, with an
    /// arbitrary field count. Hand-built precisely because `encode` refuses anything but format 1 —
    /// simulating a newer peer is the only honest way to test retention.
    fn peer_payload(state_format_version: u32, extra_fields: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf);
        enc.array(1 + extra_fields as u64).unwrap();
        enc.u32(state_format_version).unwrap();
        for field in 0..extra_fields {
            enc.u64(field as u64).unwrap();
        }
        buf
    }

    #[test]
    fn golden_snapshot() {
        // The coverage manifest is a signed wire: pin its exact bytes. A change here is a wire
        // bump, not a refactor.
        assert_eq!(hex(&encode(&sample()).unwrap()), "830100818500f6f658202a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a81835820bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0c58201d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d");
    }

    /// THE TRIPWIRE for the version boundary. Every row is a decision the wire makes; the table is
    /// exhaustive over the version axis, and it fails the moment a rule lands on the wrong side.
    ///
    /// A `format_v1` rule that leaks into the unconditional path breaks the RETAIN rows (an older
    /// binary would start rejecting legitimate newer manifests). A class invariant mistakenly
    /// scoped to format 1 breaks the REJECT row. Extend this table when adding a rule — do not
    /// re-derive the boundary from scratch.
    #[test]
    fn the_version_decision_table_is_total() {
        // version 0 — RESERVED: no projection semantics exist, so nothing could ever evaluate it.
        assert!(
            decode(entry_type::SNAPSHOT, &peer_payload(0, 2)).is_err(),
            "format 0 is reserved, not future",
        );

        // version 1 — INTERPRETED, with every format-1 rule enforced.
        let v1 = encode(&sample()).unwrap();
        assert!(matches!(decode(entry_type::SNAPSHOT, &v1).unwrap(), DecodedSnapshotOp::Known(_)));

        // versions 2.. — RETAINED, never interpreted and never rejected. Crucially this holds even
        // when the payload would violate format-1 rules (different field count, a moderation epoch,
        // an undefined target log): those are rules of format 1, and a newer format defines its
        // own. If any of them ran unconditionally, these rows would fail.
        for version in [2u32, 3, 17, u32::MAX] {
            for extra_fields in [0usize, 1, 2, 5] {
                let bytes = peer_payload(version, extra_fields);
                match decode(entry_type::SNAPSHOT, &bytes).unwrap() {
                    DecodedSnapshotOp::FutureFormat { state_format_version, bytes: retained } => {
                        assert_eq!(state_format_version, version);
                        assert_eq!(retained, bytes, "retained verbatim, never re-encoded");
                    },
                    other => panic!("format {version} must be retained, got {other:?}"),
                }
            }
        }

        // An unknown TAG is the other retention axis and behaves the same way.
        assert!(matches!(decode(9, &[0x81, 0x01]).unwrap(), DecodedSnapshotOp::UnknownTag {
            entry_type: 9,
            ..
        }));
    }

    #[test]
    fn only_the_implemented_format_can_be_authored() {
        // Signing a version this binary cannot validate would be signing a claim it cannot check.
        // This is also what makes every rule inside `format_v1` provably unconditional.
        let future = SnapshotOp::Snapshot {
            state_format_version: SNAPSHOT_STATE_FORMAT_V1 + 1,
            moderation_epoch: 0,
            targets: vec![account_target(0, &[0x11])],
        };
        assert!(encode(&future).is_err(), "a future format is retained on read, never authored");

        let reserved = SnapshotOp::Snapshot {
            state_format_version: 0,
            moderation_epoch: 0,
            targets: vec![account_target(0, &[0x11])],
        };
        assert!(encode(&reserved).is_err(), "and version 0 is reserved");
    }

    #[test]
    fn a_malformed_frame_is_refused_at_every_version() {
        // The framing is the ONE class invariant: a definite-length array whose first element is
        // the version. Without it there is nothing to dispatch on, so it cannot be version-scoped.
        assert!(decode(entry_type::SNAPSHOT, &[0xa1, 0x01, 0x02]).is_err(), "a map is not a frame");
        assert!(decode(entry_type::SNAPSHOT, &[0x80]).is_err(), "an empty array names no version");
        assert!(decode(entry_type::SNAPSHOT, &[0x81, 0x40]).is_err(), "the version must be a uint");
        let mut trailing = peer_payload(2, 1);
        trailing.push(0xff);
        assert!(decode(entry_type::SNAPSHOT, &trailing).is_err(), "trailing bytes are refused");
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
    fn a_target_naming_an_undefined_log_is_refused_at_format_1() {
        // At format 1 the log set is CLOSED — including the annex log itself, since a snapshot does
        // not cover snapshots. That this is a FORMAT-1 rule (a later format may define more) is
        // pinned by `the_version_decision_table_is_total`.
        for undefined in [3u8, 4, 200] {
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
    }

    #[test]
    fn the_reserved_moderation_epoch_is_enforced_on_both_paths() {
        // `fold_exclude` does not exist (#407), so a non-zero epoch names semantics nothing can
        // evaluate. An unenforced "MUST be 0" is just a comment.
        let nonzero = SnapshotOp::Snapshot {
            state_format_version: SNAPSHOT_STATE_FORMAT_V1,
            moderation_epoch: 1,
            targets: vec![account_target(0, &[0x11])],
        };
        assert!(encode(&nonzero).is_err(), "authoring refuses a reserved-field value");

        let mut bytes = encode(&snapshot(vec![account_target(0, &[0x11])])).unwrap();
        assert_eq!(bytes[2], 0x00, "moderation_epoch is the third header item");
        bytes[2] = 0x01;
        assert!(decode(entry_type::SNAPSHOT, &bytes).is_err(), "decoding refuses it too");
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
        // Forward-compat: an annex tag this binary does not know stays a valid, chainable entry...
        let payload = vec![0x81, 0x01];
        assert_eq!(decode(7, &payload).unwrap(), DecodedSnapshotOp::UnknownTag {
            entry_type: 7,
            bytes: payload
        });
        // ...but it must still be exactly one canonical CBOR array, or a future binary that learns
        // the tag would reject bytes this one stored.
        assert!(decode(7, &[0xa1, 0x01, 0x02]).is_err(), "a non-array annex payload is refused");
        assert!(decode(7, &[0x81, 0x01, 0xff]).is_err(), "trailing bytes are refused");
    }

    /// Distinct 32-byte fingerprints for bound tests.
    fn fp_bytes(i: usize) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[..8].copy_from_slice(&(i as u64).to_be_bytes());
        out
    }
}
