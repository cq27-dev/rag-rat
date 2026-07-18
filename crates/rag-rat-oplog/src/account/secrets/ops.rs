//! The secrets-log op payloads (`log_id = 1`) and their canonical-CBOR wire (§15).
//!
//! The secrets log is a SECOND account log at `log_id = SECRETS_LOG`. It shares the account-entry
//! envelope ([`super::super::envelope`]) — its ops ride as the opaque `payload` bstr, disambiguated
//! by the header's `entry_type`, whose frozen tag set here is a SEPARATE per-log namespace
//! ([`entry_type`]) with fresh numbering: a secrets tag `0` is NOT control's `AccountGenesis`. An
//! UNKNOWN secrets `entry_type` is retained opaque (forward-compat), never rejected — it stays a
//! valid chain link for its descendants (the slot-eligibility rule).
//!
//! Like the control ops, this layer owns STRUCTURAL wire validity only: arity, canonicity
//! (`encode(decode) == bytes`), sorted-unique recipients, the §18a `WRAP_RECIPIENTS_MAX` bound, and
//! that each `wrapped_key` decodes as a C4.1 [`SealedKeyWrap`]. Every authority/acceptance rule is
//! the secrets evaluator's ([`super::acceptance`]).

use minicbor::Encoder;
use minicbor::decode::{Decoder, Error as CborError};

use super::super::keywrap::SealedKeyWrap;
use super::super::limits::WRAP_RECIPIENTS_MAX;
use crate::cbor;
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible) — mirrors `super::super`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// The frozen secrets-log `entry_type` tag set (header part 7), a per-log namespace with fresh
/// numbering. A tag is a wire constant — a renumber is a wire bump. Decode of these tags is gated
/// on `log_id == SECRETS_LOG` so a secrets tag can never be misread as the control tag with the
/// same number.
pub(in crate::account) mod entry_type {
    /// A per-`(stream, key_id)` fan-out of content-key wraps (§15).
    pub(in crate::account) const STREAM_KEY_WRAP: u32 = 0;
}

/// One recipient's wrap inside a [`StreamKeyWrap`]: the recipient device and the C4.1 sealed key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct WrapEntry {
    pub(in crate::account) recipient_fp: DeviceFingerprint,
    pub(in crate::account) sealed: SealedKeyWrap,
}

/// The one secrets-log op today (§15): a per-`(stream, key_id)` fan-out of a content key sealed to
/// each granted device. `key_epoch` is a rotation-lag hint (not a header field). Wire order is
/// frozen; goldens pin the bytes. NO leading domain element — account-op payloads are bare
/// fixed-arity arrays, disambiguated by `entry_type`/envelope (mirrors the control ops).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct StreamKeyWrap {
    pub(in crate::account) stream_id: StreamId,
    pub(in crate::account) key_id: [u8; 32],
    pub(in crate::account) key_epoch: u64,
    pub(in crate::account) wraps: Vec<WrapEntry>,
}

/// The result of decoding a secrets-op payload against its header `entry_type`: a known op or a
/// retained opaque forward-version op (never an error for an unrecognized `entry_type`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) enum DecodedSecretsOp {
    Known(StreamKeyWrap),
    Unknown { entry_type: u32, bytes: Vec<u8> },
}

/// The `entry_type` tag a secrets op carries in its header (§15). One op today, so it is a
/// constant; takes the op by ref for symmetry with the control `entry_type_of` (and to stay total
/// as ops grow).
pub(in crate::account) fn entry_type_of(_op: &StreamKeyWrap) -> u32 {
    entry_type::STREAM_KEY_WRAP
}

/// The ingest-time structural-validation twin (the secrets mirror of the control-plaintext arm of
/// `validate_storable_header_payload`): a KNOWN secrets tag is fully validated (arity,
/// sorted-unique recipients, `WRAP_RECIPIENTS_MAX`, each `wrapped_key` decodes), an unknown tag is
/// checked only as one canonical CBOR array (retained opaque). The caller gates this on `log_id ==
/// SECRETS_LOG && crypto_suite == 0 && op_version == 1`, so it auto-covers the pre-verify promotion
/// path.
pub(in crate::account) fn validate_storable_secrets_payload(
    entry_type: u32,
    payload: &[u8],
) -> Result<(), CborError> {
    decode(entry_type, payload).map(|_| ())
}

/// Encode a secrets op to its canonical-CBOR payload (§15). Sorted wraps are emitted in
/// recipient-fingerprint order (the decoder rejects an unsorted wire, so `encode(decode) ==
/// bytes`).
pub(in crate::account) fn encode(op: &StreamKeyWrap) -> Result<Vec<u8>, CborError> {
    Ok(encode_canonical(&canonicalize(op)?))
}

/// Validate + canonicalize a secrets op so its encoding ALWAYS round-trips through [`decode`] — the
/// authoring path must never sign a payload the sorted-unique / bounded decoder (and peers) would
/// reject. Sorts + dedups + reject-conflicting + bound-checks the wrap fan-out. Everything decode
/// enforces at the op level is enforced here too, so `encode` cannot emit dead bytes.
fn canonicalize(op: &StreamKeyWrap) -> Result<StreamKeyWrap, CborError> {
    Ok(StreamKeyWrap { wraps: canonical_wraps(&op.wraps)?, ..op.clone() })
}

/// Serialize an ALREADY-canonical op (see [`canonicalize`]) to CBOR. Infallible — the wrap array is
/// already sorted, deduped, and bounded.
fn encode_canonical(op: &StreamKeyWrap) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(4).expect(INFALLIBLE);
        enc.bytes(&op.stream_id.to_bytes()).expect(INFALLIBLE);
        enc.bytes(&op.key_id).expect(INFALLIBLE);
        enc.u64(op.key_epoch).expect(INFALLIBLE);
        write_wraps(&mut enc, &op.wraps);
    }
    buf
}

/// Decode a secrets-op payload against its header `entry_type`. A known type is fully validated
/// (structure, canonicity via re-encode, sorted-unique recipients, bounds, each wrap decodes); an
/// unknown type is retained opaque (mirrors the control-op decoder).
pub(in crate::account) fn decode(
    entry_type: u32,
    bytes: &[u8],
) -> Result<DecodedSecretsOp, CborError> {
    let op = match entry_type {
        entry_type::STREAM_KEY_WRAP => decode_stream_key_wrap(bytes)?,
        other => {
            // A forward-version secrets op is RETAINED opaque (we can't re-encode it), but it must
            // STILL be exactly one canonical CBOR array — otherwise a future binary that learns
            // this entry_type would run the `encode(decode) == bytes` check below and
            // REJECT an entry an older peer accepted + chained, splitting consensus on
            // a signed log. The envelope validates the payload only as an opaque bstr,
            // so this is the ONLY place the interior is checked. (Mirrors the
            // control-op decoder.)
            cbor::require_canonical_cbor(bytes)?;
            cbor::expect_definite_len(&mut Decoder::new(bytes))?;
            return Ok(DecodedSecretsOp::Unknown { entry_type: other, bytes: bytes.to_vec() });
        },
    };
    // Canonicity guarantee for a KNOWN op: the decoded value must re-encode to the exact wire (this
    // rejects non-minimal ints, unsorted wraps, trailing bytes, a non-canonical inner wrap, etc. in
    // one check). A decoded op already satisfies every invariant `canonicalize` checks, so the
    // re-encode cannot fail.
    if encode(&op)? != bytes {
        return Err(CborError::message("secrets op payload is not canonical (re-encode differs)"));
    }
    Ok(DecodedSecretsOp::Known(op))
}

fn decode_stream_key_wrap(bytes: &[u8]) -> Result<StreamKeyWrap, CborError> {
    let mut d = Decoder::new(bytes);
    cbor::expect_array(&mut d, 4)?;
    let stream_id = StreamId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "stream_id")?);
    let key_id = cbor::fixed_bytes::<32>(d.bytes()?, "key_id")?;
    let key_epoch = d.u64()?;
    let wraps = decode_wraps(&mut d)?;
    Ok(StreamKeyWrap { stream_id, key_id, key_epoch, wraps })
}

/// Validate + canonicalize the wrap fan-out (mirrors the control cut arrays): sort by recipient
/// fingerprint, drop EXACT duplicates, reject two CONFLICTING wraps for one recipient (a silent
/// drop would lose a wrap, so this errors), enforce the §18a `WRAP_RECIPIENTS_MAX` bound, and
/// require each `wrapped_key` be a valid canonical [`SealedKeyWrap`].
fn canonical_wraps(wraps: &[WrapEntry]) -> Result<Vec<WrapEntry>, CborError> {
    let mut sorted = wraps.to_vec();
    sorted.sort_by_key(|wrap| wrap.recipient_fp.to_bytes());
    sorted.dedup();
    if sorted.windows(2).any(|pair| pair[0].recipient_fp == pair[1].recipient_fp) {
        return Err(CborError::message("wraps has conflicting entries for one recipient"));
    }
    if sorted.len() > WRAP_RECIPIENTS_MAX {
        return Err(CborError::message("wraps exceeds the §18a WRAP_RECIPIENTS_MAX bound"));
    }
    Ok(sorted)
}

/// Emit an ALREADY-canonical wrap array (see [`canonical_wraps`]). Each `wrapped_key` is the opaque
/// canonical [`SealedKeyWrap::to_cbor`] bstr — NOT an inline array (§15 S-c).
fn write_wraps(enc: &mut Encoder<&mut Vec<u8>>, wraps: &[WrapEntry]) {
    enc.array(wraps.len() as u64).expect(INFALLIBLE);
    for wrap in wraps {
        enc.array(2).expect(INFALLIBLE);
        enc.bytes(&wrap.recipient_fp.to_bytes()).expect(INFALLIBLE);
        enc.bytes(&wrap.sealed.to_cbor()).expect(INFALLIBLE);
    }
}

fn decode_wraps(d: &mut Decoder<'_>) -> Result<Vec<WrapEntry>, CborError> {
    let len = cbor::expect_definite_len(d)?;
    if len > WRAP_RECIPIENTS_MAX as u64 {
        return Err(CborError::message("wraps exceeds the §18a WRAP_RECIPIENTS_MAX bound"));
    }
    let mut wraps = Vec::with_capacity(len as usize);
    let mut prev: Option<[u8; 32]> = None;
    for _ in 0..len {
        cbor::expect_array(d, 2)?;
        let recipient = cbor::fixed_bytes::<32>(d.bytes()?, "wrap recipient_fp")?;
        // Strictly ascending by recipient ⇒ sorted AND unique in one check.
        if prev.is_some_and(|p| recipient <= p) {
            return Err(CborError::message("wraps not sorted-unique by recipient_fp"));
        }
        prev = Some(recipient);
        // The wrapped_key is an opaque bstr carrying C4.1 SealedKeyWrap CBOR; `from_cbor` re-checks
        // canonicity + width, so a non-canonical inner wrap is a structural reject here.
        let sealed = SealedKeyWrap::from_cbor(d.bytes()?).map_err(|err| {
            CborError::message(format!("wrapped_key is not a SealedKeyWrap: {err}"))
        })?;
        wraps.push(WrapEntry { recipient_fp: DeviceFingerprint::from_bytes(recipient), sealed });
    }
    Ok(wraps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::keywrap::{ContentKey, WrapContext, seal_content_key};
    use crate::device::{DeviceX25519Public, DeviceX25519Secret};

    fn recipient(seed: u8) -> DeviceX25519Public {
        DeviceX25519Secret::from_seed(&[seed; 32]).public()
    }

    fn sealed_for(seed: u8) -> SealedKeyWrap {
        let key = ContentKey::from_seed(&[1; 32]);
        let recipient = recipient(seed);
        let ctx = WrapContext {
            account_id: [2; 32],
            stream_id: [3; 32],
            key_epoch: 0,
            recipient_pub: recipient.to_bytes(),
        };
        seal_content_key(&key, &ctx, &recipient).unwrap()
    }

    fn wrap_entry(seed: u8) -> WrapEntry {
        WrapEntry {
            recipient_fp: DeviceFingerprint::from_bytes([seed; 32]),
            sealed: sealed_for(seed),
        }
    }

    fn op(wraps: Vec<WrapEntry>) -> StreamKeyWrap {
        StreamKeyWrap {
            stream_id: StreamId::from_bytes([9; 32]),
            key_id: [8; 32],
            key_epoch: 4,
            wraps,
        }
    }

    #[test]
    fn round_trips_and_sorts_recipients() {
        // Author out of order; canonical encode sorts by recipient, and decode round-trips.
        let authored = op(vec![wrap_entry(0x30), wrap_entry(0x10), wrap_entry(0x20)]);
        let bytes = encode(&authored).unwrap();
        let DecodedSecretsOp::Known(decoded) = decode(entry_type::STREAM_KEY_WRAP, &bytes).unwrap()
        else {
            panic!("known op");
        };
        let recipients: Vec<[u8; 32]> =
            decoded.wraps.iter().map(|w| w.recipient_fp.to_bytes()).collect();
        assert_eq!(recipients, vec![[0x10; 32], [0x20; 32], [0x30; 32]], "sorted by recipient_fp");
        assert_eq!(decoded.stream_id, authored.stream_id);
        assert_eq!(decoded.key_id, authored.key_id);
        assert_eq!(decoded.key_epoch, authored.key_epoch);
        assert_eq!(entry_type_of(&decoded), entry_type::STREAM_KEY_WRAP);
    }

    #[test]
    fn unsorted_and_duplicate_recipients_are_rejected_on_the_wire() {
        // A hand-built unsorted wire (encode always sorts, so build it manually).
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(4).unwrap();
            enc.bytes(&[9; 32]).unwrap();
            enc.bytes(&[8; 32]).unwrap();
            enc.u64(4).unwrap();
            enc.array(2).unwrap();
            enc.array(2).unwrap();
            enc.bytes(&[0x30; 32]).unwrap();
            enc.bytes(&sealed_for(0x30).to_cbor()).unwrap();
            enc.array(2).unwrap();
            enc.bytes(&[0x10; 32]).unwrap(); // out of order
            enc.bytes(&sealed_for(0x10).to_cbor()).unwrap();
        }
        assert!(decode(entry_type::STREAM_KEY_WRAP, &buf).is_err(), "unsorted recipients rejected");

        // A duplicate recipient is likewise not sorted-unique.
        let mut dup = Vec::new();
        {
            let mut enc = Encoder::new(&mut dup);
            enc.array(4).unwrap();
            enc.bytes(&[9; 32]).unwrap();
            enc.bytes(&[8; 32]).unwrap();
            enc.u64(4).unwrap();
            enc.array(2).unwrap();
            enc.array(2).unwrap();
            enc.bytes(&[0x10; 32]).unwrap();
            enc.bytes(&sealed_for(0x10).to_cbor()).unwrap();
            enc.array(2).unwrap();
            enc.bytes(&[0x10; 32]).unwrap();
            enc.bytes(&sealed_for(0x11).to_cbor()).unwrap();
        }
        assert!(decode(entry_type::STREAM_KEY_WRAP, &dup).is_err(), "duplicate recipient rejected");
    }

    #[test]
    fn an_undecodable_wrapped_key_is_a_structural_reject() {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(4).unwrap();
            enc.bytes(&[9; 32]).unwrap();
            enc.bytes(&[8; 32]).unwrap();
            enc.u64(4).unwrap();
            enc.array(1).unwrap();
            enc.array(2).unwrap();
            enc.bytes(&[0x10; 32]).unwrap();
            enc.bytes(&[0xde, 0xad, 0xbe, 0xef]).unwrap(); // not a SealedKeyWrap
        }
        assert!(decode(entry_type::STREAM_KEY_WRAP, &buf).is_err(), "bad wrapped_key rejected");
    }

    #[test]
    fn an_unknown_secrets_tag_is_retained_opaque_when_canonical() {
        // A single canonical CBOR array under an unknown tag is retained, not rejected.
        let mut buf = Vec::new();
        Encoder::new(&mut buf).array(0).unwrap();
        let decoded = decode(7, &buf).unwrap();
        assert_eq!(decoded, DecodedSecretsOp::Unknown { entry_type: 7, bytes: buf.clone() });
        // A non-canonical / non-array payload under an unknown tag is still a structural reject —
        // else an older peer would accept bytes a newer peer (which learns the tag) cannot fold.
        assert!(decode(7, &[0x00]).is_err(), "a non-array unknown payload is rejected");
        let mut trailing = buf.clone();
        trailing.push(0x00);
        assert!(decode(7, &trailing).is_err(), "trailing bytes under an unknown tag rejected");
    }

    #[test]
    fn non_canonical_known_wire_is_rejected_by_re_encode() {
        // A trailing byte after a valid StreamKeyWrap fails the encode(decode) == bytes check.
        let good = encode(&op(vec![wrap_entry(0x10)])).unwrap();
        let mut trailing = good.clone();
        trailing.push(0x00);
        assert!(decode(entry_type::STREAM_KEY_WRAP, &trailing).is_err(), "trailing bytes rejected");
    }
}
