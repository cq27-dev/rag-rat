//! The signed, hash-chained op-log entry envelope (phase B op-log, layer 1).
//!
//! Wraps an opaque op ([`super::op`]) in a per-`(stream, device)`, append-only signed log. Two
//! domain-tagged canonical CBOR objects, both DEFINITE-length + minimal-header (the same
//! discipline `super::op` and `super::cbor` enforce):
//!
//! ```text
//! body_bytes   = cbor([ "rag-rat/entry/2", stream_id, prev_hash, lamport, device_fingerprint,
//!                       op_bytes ])
//! signed_bytes = cbor([ "rag-rat/signed-entry/1", body_bytes (bstr), signature (64-byte bstr) ])
//! ```
//!
//! - `stream_id` is the immutable identity of the stream this entry belongs to (32-byte bstr,
//!   [`super::stream`], #509). It lives INSIDE the signed body so stream membership is
//!   signature-protected: a signed entry cannot be replayed from one stream into another (which
//!   would let a peer/relay contaminate a filtered view with another view's chains).
//! - `prev_hash` is the previous entry's `entry_hash` (32-byte bstr), or CBOR `null` for the
//!   genesis (first) entry — the hash-chain link. Chains are per `(stream_id, device)`.
//! - `op_bytes` = `op::encode(op)`, carried as an opaque bstr. Layer-1 forwards op bytes verbatim:
//!   an UNKNOWN op still signs, verifies, and chains — verification NEVER requires `op::decode` to
//!   succeed.
//! - `entry_hash = sha256(body_bytes)` — the chain link + content address (the `(seq, entry_hash)`
//!   watermark primitive). Because the body folds `stream_id`, entry hashes are globally unique
//!   across streams.
//!
//! **The signature covers exactly `body_bytes`** — the canonical CBOR of
//! `[domain, stream_id, prev_hash, lamport, device_fingerprint, op_bytes]`, and nothing else.
//! `signed_bytes` (the outer envelope) is NOT signed; it is the transport form bundling the signed
//! body with its signature. This is the security boundary: a byte that is not inside `body_bytes`
//! is not protected by the signature, and the canonical rule guarantees the SAME logical body has
//! exactly one signed encoding.

use anyhow::Context;
use minicbor::Encoder;
use minicbor::data::Type;
use minicbor::decode::{Decoder, Error as CborError};
use sha2::{Digest, Sha256};

use super::cbor;
use super::device::{DevicePublic, DeviceSecret};
use super::op::{self, DeviceFingerprint, MemoryOp};
use super::stream::StreamId;

/// Domain tag + version for the signed BODY (the bytes the signature covers). Bump the version to
/// evolve the entry wire deliberately — an old binary then rejects the new domain rather than
/// misreading it. (`/2` added the in-body `stream_id`; `/1` never shipped and has no decoder.)
const ENTRY_DOMAIN: &str = "rag-rat/entry/2";

/// Domain tag + version for the outer transport envelope (body + signature).
const SIGNED_DOMAIN: &str = "rag-rat/signed-entry/1";

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible), so every encode step
/// `.expect`s this — mirrors `super::op`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// The decoded, structurally-validated contents of an entry BODY. The op is carried as opaque
/// `op_bytes`, so an UNKNOWN op still verifies + chains; decode it with `op::decode` only when the
/// projection actually needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VerifiedEntry {
    /// The stream this entry belongs to — signature-protected, so it cannot be re-homed.
    pub(super) stream_id: StreamId,
    /// The previous entry's `entry_hash`, or `None` for the genesis entry.
    pub(super) prev_hash: Option<[u8; 32]>,
    pub(super) lamport: u64,
    pub(super) device_fingerprint: DeviceFingerprint,
    /// `op::encode(op)` — the frozen op wire, opaque at this layer.
    pub(super) op_bytes: Vec<u8>,
    /// `sha256(body_bytes)` — the chain link + content address.
    pub(super) entry_hash: [u8; 32],
}

/// A signed op-log entry: its structured body, the signature over `body_bytes`, and both wire
/// encodings. `signed_bytes` is the transport/storage form; `body_bytes` is exactly what the
/// signature and `entry_hash` cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SignedEntry {
    pub(super) entry: VerifiedEntry,
    pub(super) signature: [u8; 64],
    pub(super) body_bytes: Vec<u8>,
    pub(super) signed_bytes: Vec<u8>,
}

/// The pieces of one entry body, grouped so the encoder takes a single named argument rather than a
/// five-primitive train (three of which are 32-byte arrays that are easy to transpose).
struct BodyParts<'a> {
    stream_id: StreamId,
    prev_hash: Option<[u8; 32]>,
    lamport: u64,
    device_fingerprint: DeviceFingerprint,
    op_bytes: &'a [u8],
}

/// Author a signed entry: encode + sign the body (`device_fingerprint = secret.public()`), and
/// build the transport envelope. Pure and deterministic given `secret` (ed25519 signing is
/// deterministic).
pub(super) fn sign_entry(
    secret: &DeviceSecret,
    stream_id: StreamId,
    prev_hash: Option<[u8; 32]>,
    lamport: u64,
    op: &MemoryOp,
) -> SignedEntry {
    let device_fingerprint = secret.public().fingerprint();
    let op_bytes = op::encode(op);
    let body_bytes = encode_body(&BodyParts {
        stream_id,
        prev_hash,
        lamport,
        device_fingerprint,
        op_bytes: &op_bytes,
    });
    let entry_hash = sha256(&body_bytes);
    let signature = secret.sign(&body_bytes);
    let signed_bytes = encode_signed(&body_bytes, &signature);
    SignedEntry {
        entry: VerifiedEntry {
            stream_id,
            prev_hash,
            lamport,
            device_fingerprint,
            op_bytes,
            entry_hash,
        },
        signature,
        body_bytes,
        signed_bytes,
    }
}

/// Sign a body over ARBITRARY op bytes (not `op::encode` of a known op) — the storage layer's
/// poison guard and its unknown-op retention path need entries whose `op_bytes` are undecodable or
/// decode to `Unknown`, which the typed [`sign_entry`] can't produce. Test-only; the production
/// wire is untouched.
#[cfg(test)]
pub(super) fn sign_entry_from_op_bytes(
    secret: &DeviceSecret,
    stream_id: StreamId,
    prev_hash: Option<[u8; 32]>,
    lamport: u64,
    op_bytes: Vec<u8>,
) -> SignedEntry {
    let device_fingerprint = secret.public().fingerprint();
    let body_bytes = encode_body(&BodyParts {
        stream_id,
        prev_hash,
        lamport,
        device_fingerprint,
        op_bytes: &op_bytes,
    });
    let entry_hash = sha256(&body_bytes);
    let signature = secret.sign(&body_bytes);
    let signed_bytes = encode_signed(&body_bytes, &signature);
    SignedEntry {
        entry: VerifiedEntry {
            stream_id,
            prev_hash,
            lamport,
            device_fingerprint,
            op_bytes,
            entry_hash,
        },
        signature,
        body_bytes,
        signed_bytes,
    }
}

/// Decode a signed entry envelope, STRUCTURE only — canonical (no trailing) outer + body, extract
/// the fields. Does NOT verify the signature (that is [`verify_signed`]).
pub(super) fn decode_signed(bytes: &[u8]) -> anyhow::Result<SignedEntry> {
    decode_signed_cbor(bytes).map_err(|err| anyhow::anyhow!("signed entry decode failed: {err}"))
}

/// Decode + cryptographically verify a signed entry under `pubkey`: `verify_strict` over
/// `body_bytes`, then assert `pubkey.fingerprint() == body.device_fingerprint` so the key is bound
/// to the device the body names. Returns the [`VerifiedEntry`] (op left opaque). A tampered body /
/// header / signature, a wrong key, or a fingerprint mismatch each → `Err`.
pub(super) fn verify_signed(bytes: &[u8], pubkey: &DevicePublic) -> anyhow::Result<VerifiedEntry> {
    let signed = decode_signed(bytes)?;
    pubkey.verify(&signed.body_bytes, &signed.signature)?;
    // Bind key ↔ device: the signature proves SOME holder of `pubkey` signed the body; this proves
    // `pubkey` IS the device the body claims. Without it, a valid signature under an unrelated key
    // could be paired with any device_fingerprint.
    if pubkey.fingerprint() != signed.entry.device_fingerprint {
        anyhow::bail!("device fingerprint does not match the verifying key");
    }
    Ok(signed.entry)
}

/// Verify a SINGLE device's append-only chain under `pubkey`: one `stream_id` throughout (a chain
/// is per `(stream, device)` — a spliced-in foreign-stream entry must not read as continuity),
/// genesis `prev_hash == None`, each subsequent `prev_hash == previous.entry_hash`, STRICTLY
/// increasing `lamport`, and every signature valid (re-checked from `signed_bytes`, not trusted
/// from the struct's cached fields). Cross-device merge is the fold (`super::project`), never here.
pub(super) fn verify_chain(entries: &[SignedEntry], pubkey: &DevicePublic) -> anyhow::Result<()> {
    let mut prev: Option<VerifiedEntry> = None;
    for (idx, signed) in entries.iter().enumerate() {
        // Trust only the signed bytes: re-verify the signature + fingerprint from the wire, so a
        // hand-forged `SignedEntry` whose cached `entry` disagrees with `signed_bytes` can't pass.
        let verified = verify_signed(&signed.signed_bytes, pubkey)
            .with_context(|| format!("entry {idx} failed signature verification"))?;
        match &prev {
            None =>
                if verified.prev_hash.is_some() {
                    anyhow::bail!("genesis entry {idx} must have prev_hash == None");
                },
            Some(previous) => {
                if verified.stream_id != previous.stream_id {
                    anyhow::bail!("entry {idx} belongs to a different stream than the chain");
                }
                if verified.prev_hash != Some(previous.entry_hash) {
                    anyhow::bail!("entry {idx} prev_hash does not link to the previous entry_hash");
                }
                if verified.lamport <= previous.lamport {
                    anyhow::bail!(
                        "entry {idx} lamport {} is not strictly greater than {}",
                        verified.lamport,
                        previous.lamport
                    );
                }
            },
        }
        prev = Some(verified);
    }
    Ok(())
}

/// Encode the signed body: `[domain, stream_id, prev_hash | null, lamport, device_fingerprint,
/// op_bytes]`, definite lengths + minimal headers. THESE are the bytes the signature and
/// `entry_hash` cover.
fn encode_body(parts: &BodyParts<'_>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(6).expect(INFALLIBLE);
        enc.str(ENTRY_DOMAIN).expect(INFALLIBLE);
        enc.bytes(&parts.stream_id.to_bytes()).expect(INFALLIBLE);
        match parts.prev_hash {
            // A 32-byte bstr for a linked entry, CBOR null for the genesis — an unambiguous,
            // distinct "no predecessor" marker (not an all-zero hash).
            Some(hash) => enc.bytes(&hash).expect(INFALLIBLE),
            None => enc.null().expect(INFALLIBLE),
        };
        enc.u64(parts.lamport).expect(INFALLIBLE);
        enc.bytes(&parts.device_fingerprint.to_bytes()).expect(INFALLIBLE);
        enc.bytes(parts.op_bytes).expect(INFALLIBLE);
    }
    buf
}

/// Encode the outer transport envelope: `[domain, body_bytes (bstr), signature (bstr)]`. NOT signed
/// — it bundles the already-signed body with its signature.
fn encode_signed(body_bytes: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(body_bytes.len() + 96);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(3).expect(INFALLIBLE);
        enc.str(SIGNED_DOMAIN).expect(INFALLIBLE);
        enc.bytes(body_bytes).expect(INFALLIBLE);
        enc.bytes(signature).expect(INFALLIBLE);
    }
    buf
}

fn decode_signed_cbor(bytes: &[u8]) -> Result<SignedEntry, CborError> {
    // The whole wire must be exactly one canonical CBOR item, no trailing bytes (body_bytes and the
    // signature are validated as opaque bstrs here; the body's own canonicity is checked below).
    cbor::require_canonical_cbor(bytes)?;
    let mut d = Decoder::new(bytes);
    cbor::expect_array(&mut d, 3)?;
    expect_domain(&mut d, SIGNED_DOMAIN)?;
    let body_bytes = d.bytes()?.to_vec();
    let signature = fixed_bytes::<64>(d.bytes()?, "signature")?;
    let entry = decode_body(&body_bytes)?;
    Ok(SignedEntry { entry, signature, body_bytes, signed_bytes: bytes.to_vec() })
}

fn decode_body(body_bytes: &[u8]) -> Result<VerifiedEntry, CborError> {
    // The body must independently be exactly one canonical CBOR item (op_bytes stays an opaque bstr
    // — layer-1 does NOT require the op to be a recognizable / canonical op here).
    cbor::require_canonical_cbor(body_bytes)?;
    let mut d = Decoder::new(body_bytes);
    cbor::expect_array(&mut d, 6)?;
    expect_domain(&mut d, ENTRY_DOMAIN)?;
    let stream_id = StreamId::from_bytes(fixed_bytes::<32>(d.bytes()?, "stream_id")?);
    let prev_hash = decode_prev_hash(&mut d)?;
    let lamport = d.u64()?;
    let device_fingerprint =
        DeviceFingerprint::from_bytes(fixed_bytes::<32>(d.bytes()?, "device fingerprint")?);
    let op_bytes = d.bytes()?.to_vec();
    let entry_hash = sha256(body_bytes);
    Ok(VerifiedEntry { stream_id, prev_hash, lamport, device_fingerprint, op_bytes, entry_hash })
}

/// Read the leading domain string and assert it matches `want` — a wrong/absent tag is a foreign or
/// version-bumped object an old binary must reject, never misread.
fn expect_domain(d: &mut Decoder<'_>, want: &str) -> Result<(), CborError> {
    let got = d.str()?;
    if got == want {
        Ok(())
    } else {
        Err(CborError::message(format!("unknown domain tag `{got}` (expected `{want}`)")))
    }
}

/// Decode the `prev_hash` slot: CBOR null → genesis (`None`), else a 32-byte bstr link.
fn decode_prev_hash(d: &mut Decoder<'_>) -> Result<Option<[u8; 32]>, CborError> {
    if d.datatype()? == Type::Null {
        d.null()?;
        Ok(None)
    } else {
        Ok(Some(fixed_bytes::<32>(d.bytes()?, "prev_hash")?))
    }
}

/// Convert an opaque byte slice to a fixed `[u8; N]`, erroring with the field name on a length
/// mismatch (a wrong-length hash / fingerprint / signature is a structural error).
fn fixed_bytes<const N: usize>(bytes: &[u8], field: &str) -> Result<[u8; N], CborError> {
    <[u8; N]>::try_from(bytes)
        .map_err(|_| CborError::message(format!("{field} must be {N} bytes, got {}", bytes.len())))
}

/// `sha256` into a fixed 32-byte array — the entry-hash / content-address primitive.
fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::op::{NodeContent, NodeId};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn secret() -> DeviceSecret {
        DeviceSecret::from_seed(&[7u8; 32])
    }

    /// A fixed stream identity for wire fixtures — deterministic and visually distinctive.
    fn stream() -> StreamId {
        StreamId::from_bytes([3u8; 32])
    }

    /// A representative content op for round-trip / tamper tests.
    fn node_create() -> MemoryOp {
        MemoryOp::NodeCreate {
            node_id: NodeId::from("mem_1"),
            content: NodeContent {
                kind: "Invariant".to_string(),
                title: "title".to_string(),
                body: "body".to_string(),
                confidence: "high".to_string(),
                source: "agent".to_string(),
                tags: vec!["a".to_string(), "b".to_string()],
                payload: None,
            },
        }
    }

    #[test]
    fn golden_vectors_pin_the_entry_wire_format() {
        // The entry wire is a frozen primitive: the signature, `entry_hash`, and the chain all
        // build on these exact bytes. A canonical-rule change must break this test and
        // force a deliberate domain bump (as adding `stream_id` bumped `/1` to `/2`).
        // Fixture: genesis Snapshot on stream [3; 32], lamport 1, seed [7; 32] — fully
        // deterministic (ed25519 signing is deterministic).
        let signed = sign_entry(&secret(), stream(), None, 1, &MemoryOp::Snapshot);
        assert_eq!(
            hex(&signed.body_bytes),
            "866f7261672d7261742f656e7472792f3258200303030303030303030303030303030303030303030303030303030303030303f6015820fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638895818836c7261672d7261742f6f702f3168736e617073686f74f6",
            "body_bytes golden",
        );
        assert_eq!(
            hex(&signed.signed_bytes),
            "83767261672d7261742f7369676e65642d656e7472792f315871866f7261672d7261742f656e7472792f3258200303030303030303030303030303030303030303030303030303030303030303f6015820fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638895818836c7261672d7261742f6f702f3168736e617073686f74f658400aab0cf060dd7eee539e0e84c48fdf5be81eaba1ece62324a513cd6adc5a29e508c275dd0b4a4d2030254375a62ee171527e562136102e702c40924d7488a504",
            "signed_bytes golden",
        );
    }

    #[test]
    fn body_bytes_has_the_expected_canonical_structure() {
        // Structure-level proof (independent of the opaque fingerprint / signature bytes) that the
        // encoder emits the frozen shape the golden vector pins.
        let signed = sign_entry(&secret(), stream(), None, 1, &MemoryOp::Snapshot);
        let body = &signed.body_bytes;
        assert_eq!(body[0], 0x86, "6-element array header");
        assert_eq!(body[1], 0x6f, "text header, len 15");
        assert_eq!(&body[2..17], b"rag-rat/entry/2");
        assert_eq!(&body[17..19], &[0x58, 0x20], "stream_id bstr header, len 32");
        assert_eq!(&body[19..51], &stream().to_bytes(), "stream_id bytes");
        assert_eq!(body[51], 0xf6, "genesis prev_hash is CBOR null");
        assert_eq!(body[52], 0x01, "lamport 1, minimal uint");
        assert_eq!(&body[53..55], &[0x58, 0x20], "device_fp bstr header, len 32");
        assert_eq!(
            &body[55..87],
            &secret().public().fingerprint().to_bytes(),
            "device_fp bytes == sha256(pubkey)",
        );
        assert_eq!(&body[87..89], &[0x58, 0x18], "op_bytes bstr header, len 24");
        assert_eq!(&body[89..], &op::encode(&MemoryOp::Snapshot), "op_bytes == op::encode");
        assert_eq!(body.len(), 113);
    }

    #[test]
    fn signed_bytes_has_the_expected_canonical_structure() {
        let signed = sign_entry(&secret(), stream(), None, 1, &MemoryOp::Snapshot);
        let wire = &signed.signed_bytes;
        assert_eq!(wire[0], 0x83, "3-element array header");
        assert_eq!(wire[1], 0x76, "text header, len 22");
        assert_eq!(&wire[2..24], b"rag-rat/signed-entry/1");
        assert_eq!(&wire[24..26], &[0x58, 0x71], "body bstr header, len 113");
        assert_eq!(&wire[26..139], signed.body_bytes.as_slice());
        assert_eq!(&wire[139..141], &[0x58, 0x40], "signature bstr header, len 64");
        assert_eq!(&wire[141..205], &signed.signature);
        assert_eq!(wire.len(), 205);
    }

    #[test]
    fn entry_hash_is_sha256_of_the_body() {
        let signed = sign_entry(&secret(), stream(), None, 1, &MemoryOp::Snapshot);
        assert_eq!(signed.entry.entry_hash, sha256(&signed.body_bytes));
    }

    #[test]
    fn genesis_round_trips_through_verify() {
        let secret = secret();
        let signed = sign_entry(&secret, stream(), None, 1, &node_create());
        let verified = verify_signed(&signed.signed_bytes, &secret.public()).expect("verifies");
        assert_eq!(verified, signed.entry);
        assert_eq!(verified.prev_hash, None);
        assert_eq!(verified.stream_id, stream());
        // decode_signed alone (no signature check) yields the same structure.
        assert_eq!(decode_signed(&signed.signed_bytes).unwrap().entry, signed.entry);
    }

    #[test]
    fn non_genesis_round_trips_through_verify() {
        let secret = secret();
        let genesis = sign_entry(&secret, stream(), None, 1, &MemoryOp::Snapshot);
        let second =
            sign_entry(&secret, stream(), Some(genesis.entry.entry_hash), 2, &node_create());
        let verified = verify_signed(&second.signed_bytes, &secret.public()).expect("verifies");
        assert_eq!(verified.prev_hash, Some(genesis.entry.entry_hash));
        assert_eq!(verified.lamport, 2);
    }

    #[test]
    fn verify_rejects_the_wrong_key() {
        let signed = sign_entry(&secret(), stream(), None, 1, &MemoryOp::Snapshot);
        let wrong = DeviceSecret::from_seed(&[9u8; 32]).public();
        assert!(
            verify_signed(&signed.signed_bytes, &wrong).is_err(),
            "a signature must not verify under an unrelated key",
        );
    }

    #[test]
    fn verify_rejects_a_fingerprint_mismatch() {
        // Sign under key A but re-wrap the body with key B's fingerprint: the signature is over the
        // ORIGINAL body (fingerprint A), so decoding + re-signing under B is needed to forge — but
        // the honest check is: a body claiming a fingerprint that isn't the verifying key's is
        // rejected. Build a body whose device_fingerprint is B's while signing with A.
        let secret_a = secret();
        let public_b = DeviceSecret::from_seed(&[9u8; 32]).public();
        let op_bytes = op::encode(&MemoryOp::Snapshot);
        let forged_body = encode_body(&BodyParts {
            stream_id: stream(),
            prev_hash: None,
            lamport: 1,
            device_fingerprint: public_b.fingerprint(),
            op_bytes: &op_bytes,
        });
        let signature = secret_a.sign(&forged_body);
        let forged = encode_signed(&forged_body, &signature);
        // Verifying under A fails the SIGNATURE-fingerprint bind (A's fingerprint != claimed B).
        assert!(verify_signed(&forged, &secret_a.public()).is_err());
        // Verifying under B fails the SIGNATURE (A signed it, not B).
        assert!(verify_signed(&forged, &public_b).is_err());
    }

    /// Flip one byte at `index` of an otherwise-valid signed entry and assert verification fails.
    fn assert_tamper_rejected(index: usize, label: &str) {
        let secret = secret();
        let signed = sign_entry(&secret, stream(), Some([0x11; 32]), 5, &node_create());
        let mut wire = signed.signed_bytes.clone();
        wire[index] ^= 0x01;
        assert!(
            verify_signed(&wire, &secret.public()).is_err(),
            "tampering {label} (byte {index}) must be rejected",
        );
    }

    #[test]
    fn tampering_any_signed_field_is_rejected() {
        // Locate each field inside the fixture's wire and flip a byte within it. Layout (a
        // non-genesis node_create entry): outer [0x83, 0x76, 22-byte domain, 0x58 len, body,
        // 0x58 0x40, sig(64)]; body [0x86, 0x6f, 15-byte domain, 0x58 0x20 stream_id(32),
        // 0x58 0x20 prev_hash(32), lamport, 0x58 0x20 device_fp(32), 0x58 .. op_bytes(..)].
        let secret = secret();
        let signed = sign_entry(&secret, stream(), Some([0x11; 32]), 5, &node_create());
        let wire = &signed.signed_bytes;
        // Body starts after outer header [0x83, 0x76, 22 domain bytes, 0x58, len] = 2 + 22 + 2 =
        // 26.
        let body_start = 26;
        // Within the body: [0]=array, [1..17)=domain(0x6f+15), stream_id header [17,18],
        // stream_id = [19..51), prev_hash header [51,52], prev_hash = [53..85), lamport at 85
        // (5 → 0x05), device_fp header [86,87], fp [88..120), op header at 120, op_bytes after.
        let stream_byte = body_start + 19; // inside the 32-byte stream_id
        let prev_hash_byte = body_start + 53; // inside the 32-byte prev_hash
        let lamport_byte = body_start + 85; // the lamport uint
        let device_fp_byte = body_start + 88; // inside the 32-byte device_fp
        let op_byte = body_start + 122; // inside op_bytes
        let signature_byte = wire.len() - 1; // last signature byte
        for (index, label) in [
            (stream_byte, "stream_id"),
            (prev_hash_byte, "prev_hash"),
            (lamport_byte, "lamport"),
            (device_fp_byte, "device_fingerprint"),
            (op_byte, "op_bytes"),
            (signature_byte, "signature"),
        ] {
            assert_tamper_rejected(index, label);
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let signed = sign_entry(&secret(), stream(), None, 1, &MemoryOp::Snapshot);
        let mut wire = signed.signed_bytes.clone();
        wire.push(0x00);
        assert!(decode_signed(&wire).is_err(), "trailing bytes after the wire are rejected");
    }

    #[test]
    fn non_canonical_lamport_header_is_rejected() {
        // A lamport encoded with a non-minimal 8-byte header is non-canonical: the body must have
        // exactly one accepted encoding, so `decode_body` rejects it.
        let secret = secret();
        let op_bytes = op::encode(&MemoryOp::Snapshot);
        let good = encode_body(&BodyParts {
            stream_id: stream(),
            prev_hash: None,
            lamport: 1,
            device_fingerprint: secret.public().fingerprint(),
            op_bytes: &op_bytes,
        });
        // good[52] is the minimal lamport uint (0x01). Splice a non-minimal 8-byte header for 1.
        assert_eq!(good[52], 0x01);
        let mut overlong = good[..52].to_vec();
        overlong.extend_from_slice(&[0x1b, 0, 0, 0, 0, 0, 0, 0, 1]); // uint 1, non-minimal 8-byte
        overlong.extend_from_slice(&good[53..]);
        let signature = secret.sign(&overlong);
        let wire = encode_signed(&overlong, &signature);
        assert!(verify_signed(&wire, &secret.public()).is_err(), "non-canonical lamport rejected");
    }

    #[test]
    fn malformed_and_wrong_domain_bytes_are_rejected() {
        assert!(decode_signed(&[0x00]).is_err(), "not even a CBOR array");
        assert!(decode_signed(&[]).is_err(), "empty input");
        // A wrong outer domain tag.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.str("rag-rat/signed-entry/2").unwrap();
            enc.bytes(&[0x80]).unwrap();
            enc.bytes(&[0u8; 64]).unwrap();
        }
        assert!(decode_signed(&buf).is_err(), "a bumped outer domain must not decode");
    }

    #[test]
    fn an_unknown_op_still_signs_verifies_and_chains() {
        // Layer-1 forwards op bytes opaque: a future op KIND this binary can't decode must still
        // sign, verify, and chain. Craft an unknown-but-canonical op envelope directly as op_bytes.
        let secret = secret();
        let mut unknown_op = Vec::new();
        {
            let mut enc = Encoder::new(&mut unknown_op);
            enc.array(3).unwrap();
            enc.str("rag-rat/op/1").unwrap();
            enc.str("future_op").unwrap();
            enc.u64(42).unwrap();
        }
        // Sanity: op::decode keeps it opaque (Unknown), never errors.
        assert!(op::decode(&unknown_op).is_ok());
        let body = encode_body(&BodyParts {
            stream_id: stream(),
            prev_hash: None,
            lamport: 1,
            device_fingerprint: secret.public().fingerprint(),
            op_bytes: &unknown_op,
        });
        let signature = secret.sign(&body);
        let wire = encode_signed(&body, &signature);
        let verified = verify_signed(&wire, &secret.public()).expect("unknown op still verifies");
        assert_eq!(verified.op_bytes, unknown_op, "op_bytes forwarded verbatim");
        // And it chains as a single-entry genesis.
        let genesis =
            SignedEntry { entry: verified, signature, body_bytes: body, signed_bytes: wire };
        verify_chain(&[genesis], &secret.public()).expect("unknown op chains");
    }

    /// A good three-entry chain from one device on one stream: genesis + two links, strictly
    /// increasing lamport.
    fn good_chain(secret: &DeviceSecret) -> Vec<SignedEntry> {
        let e0 = sign_entry(secret, stream(), None, 1, &MemoryOp::Snapshot);
        let e1 = sign_entry(secret, stream(), Some(e0.entry.entry_hash), 2, &node_create());
        let e2 = sign_entry(secret, stream(), Some(e1.entry.entry_hash), 5, &MemoryOp::Snapshot);
        vec![e0, e1, e2]
    }

    #[test]
    fn a_good_three_entry_chain_verifies() {
        let secret = secret();
        verify_chain(&good_chain(&secret), &secret.public()).expect("a well-formed chain verifies");
    }

    #[test]
    fn a_chain_under_the_wrong_key_fails() {
        let secret = secret();
        let chain = good_chain(&secret);
        let wrong = DeviceSecret::from_seed(&[9u8; 32]).public();
        assert!(verify_chain(&chain, &wrong).is_err());
    }

    #[test]
    fn a_broken_prev_hash_link_fails() {
        let secret = secret();
        let mut chain = good_chain(&secret);
        // Re-author entry 2 pointing at the WRONG predecessor hash (still a valid signature).
        chain[2] = sign_entry(&secret, stream(), Some([0xaa; 32]), 5, &MemoryOp::Snapshot);
        assert!(verify_chain(&chain, &secret.public()).is_err(), "a broken link must fail");
    }

    #[test]
    fn a_non_monotonic_lamport_fails() {
        let secret = secret();
        let e0 = sign_entry(&secret, stream(), None, 5, &MemoryOp::Snapshot);
        // entry 1 links correctly but does NOT advance the lamport (5 is not > 5).
        let e1 = sign_entry(&secret, stream(), Some(e0.entry.entry_hash), 5, &node_create());
        assert!(
            verify_chain(&[e0, e1], &secret.public()).is_err(),
            "lamport must strictly increase",
        );
    }

    #[test]
    fn a_non_none_genesis_prev_hash_fails() {
        let secret = secret();
        // A genesis entry that claims a predecessor is not a valid chain head.
        let genesis = sign_entry(&secret, stream(), Some([0x22; 32]), 1, &MemoryOp::Snapshot);
        assert!(
            verify_chain(&[genesis], &secret.public()).is_err(),
            "genesis prev_hash must be None",
        );
    }

    #[test]
    fn a_foreign_stream_entry_cannot_continue_a_chain() {
        // A validly-signed entry that links the predecessor hash and advances the lamport, but
        // belongs to a DIFFERENT stream, is not continuity: chains are per (stream, device).
        let secret = secret();
        let e0 = sign_entry(&secret, stream(), None, 1, &MemoryOp::Snapshot);
        let other = StreamId::from_bytes([4u8; 32]);
        let e1 = sign_entry(&secret, other, Some(e0.entry.entry_hash), 2, &node_create());
        assert!(
            verify_chain(&[e0, e1], &secret.public()).is_err(),
            "a chain must carry exactly one stream_id",
        );
    }

    #[test]
    fn a_single_bad_signature_fails_the_chain() {
        let secret = secret();
        let mut chain = good_chain(&secret);
        // Corrupt one byte of the middle entry's wire signature.
        let last = chain[1].signed_bytes.len() - 1;
        chain[1].signed_bytes[last] ^= 0x01;
        assert!(
            verify_chain(&chain, &secret.public()).is_err(),
            "a single bad signature fails the whole chain",
        );
    }
}
