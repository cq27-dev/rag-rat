//! The `/3` content envelope — `rag-rat/entry/3` inside `rag-rat/signed-entry/1` (§8).
//!
//! The signature covers `body_bytes = cbor([header_bytes, payload])`; the header content bytes are
//! the future C5 AEAD AAD. The payload stays opaque here so plaintext and sealed entries have the
//! same chain and signature semantics.

use chacha20poly1305::aead::{Aead, AeadInOut, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use minicbor::Encoder;
use minicbor::data::Type;
use minicbor::decode::{Decoder, Error as CborError};
use zeroize::Zeroizing;

use super::super::AccountId;
use super::super::keywrap::ContentKey;
use super::super::limits::{
    CONTENT_ENTRY_DOMAIN, CONTENT_ENVELOPE_MAX_BYTES, CONTENT_SIGNED_DOMAIN,
};
use crate::cbor;
use crate::device::{DevicePublic, DeviceSecret};
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// The suite-1 (XChaCha20-Poly1305 sealed) crypto-suite id a sealed `/3` header carries. The
/// suite-0 sibling [`sign_content_entry`] never authors this; [`sign_sealed_content_entry`] is the
/// ONLY suite-1 authoring path (sync phase C5a, #608).
const SEALED_CRYPTO_SUITE: u64 = 1;

/// The XChaCha20-Poly1305 wire nonce width (192 bits) prepended to every suite-1 payload:
/// `payload = nonce[24] || ciphertext`. A FRESH RANDOM nonce per seal — a content key is
/// long-lived, so unlike the keywrap there is no per-seal secret to derive a nonce from — is what
/// keeps `(key, nonce)` unique regardless of author-seam refactors. Freezing this width freezes the
/// suite-1 payload layout.
pub(super) const SEALED_NONCE_LEN: usize = 24;

/// The Poly1305 tag width XChaCha20-Poly1305 appends to the ciphertext.
pub(super) const SEALED_AEAD_TAG_LEN: usize = 16;

/// The fixed 13-part `/3` header. The domain is encoded as part 0 and therefore is not stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentEntryHeader {
    pub stream_id: StreamId,
    pub author_account_id: AccountId,
    pub device_fingerprint: DeviceFingerprint,
    pub seq: u64,
    pub lamport: u64,
    pub prev_hash: Option<[u8; 32]>,
    pub grant_id: Option<[u8; 32]>,
    pub roster_ref: [u8; 32],
    pub owner_auth_len: u64,
    pub author_auth_len: u64,
    pub crypto_suite: u64,
    pub key_id: Option<[u8; 32]>,
}

/// A structurally decoded content entry plus every signed wire unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedContentEntry {
    pub header: ContentEntryHeader,
    pub payload: Vec<u8>,
    pub signature: [u8; 64],
    pub header_bytes: Vec<u8>,
    pub body_bytes: Vec<u8>,
    pub signed_bytes: Vec<u8>,
    pub entry_hash: [u8; 32],
}

/// A decoded and signature-verified `/3` entry. Authority and branch acceptance are C3 concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedContentEntry {
    pub header: ContentEntryHeader,
    pub payload: Vec<u8>,
    pub header_bytes: Vec<u8>,
    pub entry_hash: [u8; 32],
}

/// Encode and sign one `/3` entry. The signing key always determines the device fingerprint.
pub fn sign_content_entry(
    secret: &DeviceSecret,
    header: &ContentEntryHeader,
    payload: &[u8],
) -> anyhow::Result<SignedContentEntry> {
    if payload.len() > CONTENT_ENVELOPE_MAX_BYTES {
        anyhow::bail!("content payload exceeds the §18a envelope limit");
    }
    let mut header = header.clone();
    header.device_fingerprint = secret.public().fingerprint();
    if let Some(message) = header_nullity_error(&header) {
        anyhow::bail!(message);
    }
    if header.crypto_suite != 0 {
        anyhow::bail!("C2 authoring supports only plaintext_signed crypto suite 0");
    }
    cbor::require_canonical_cbor(payload)
        .map_err(|error| anyhow::anyhow!("plaintext op payload is not canonical CBOR: {error}"))?;
    let header_bytes = encode_header(&header);
    let body_bytes = encode_body(&header_bytes, payload);
    let signature = secret.sign(&body_bytes);
    let signed_bytes = encode_signed(&body_bytes, &signature);
    if signed_bytes.len() > CONTENT_ENVELOPE_MAX_BYTES {
        anyhow::bail!(
            "content entry is {} bytes, over the {CONTENT_ENVELOPE_MAX_BYTES}-byte §18a limit",
            signed_bytes.len(),
        );
    }
    let entry_hash = cbor::sha256(&body_bytes);
    Ok(SignedContentEntry {
        header,
        payload: payload.to_vec(),
        signature,
        header_bytes,
        body_bytes,
        signed_bytes,
        entry_hash,
    })
}

/// Encode, SEAL, and sign one suite-1 `/3` content entry (sync phase C5a, #608). The op body is
/// encrypted under `key` with XChaCha20-Poly1305; the wire payload is
/// `nonce[24] || ciphertext(op_bytes + 16-byte Poly1305 tag)`, and the AEAD AAD is the FINAL signed
/// header content bytes.
///
/// The header is finalized BEFORE it is encoded — the signing key sets `device_fingerprint`, and
/// this path (never the caller) sets `crypto_suite = 1` + `key_id = key.key_id()` — so the AAD
/// binds exactly the header bytes the signature covers. Sealing against a pre-finalization header
/// encode would brick decrypt on an entry that still folds `accepted` (an un-recoverable accepted
/// entry).
///
/// `nonce` is injected so golden vectors are byte-reproducible; production authoring samples it
/// from the OS CSPRNG via [`seal_and_sign_content_entry`]. The suite-0 sibling
/// [`sign_content_entry`] keeps its `crypto_suite != 0` bail, so a suite-1-over-plaintext header is
/// unconstructible.
pub fn sign_sealed_content_entry(
    secret: &DeviceSecret,
    header: &ContentEntryHeader,
    op_bytes: &[u8],
    key: &ContentKey,
    nonce: [u8; SEALED_NONCE_LEN],
) -> anyhow::Result<SignedContentEntry> {
    // §18a cap BEFORE the AEAD encrypt/allocate (mirrors `sign_content_entry`'s up-front payload
    // check): the sealed wire payload is `nonce[24] || ciphertext(op_bytes + tag[16])`, so bound
    // the op body plus that fixed 40-byte AEAD expansion against the cap here. Rejecting up
    // front means an oversized op is never encrypted or buffered — this is the exact bound
    // `content_op_is_sealed_authorable` reserves against. `saturating_add` cannot overflow the cap.
    if op_bytes.len().saturating_add(SEALED_NONCE_LEN + SEALED_AEAD_TAG_LEN)
        > CONTENT_ENVELOPE_MAX_BYTES
    {
        anyhow::bail!("sealed content payload exceeds the §18a envelope limit");
    }

    // The op body is the plaintext that becomes the ciphertext; require it canonical up front. The
    // suite-1 wire payload is opaque, so `decode_body`'s suite-0 payload-canonicity check cannot
    // cover it — but decrypt-at-projection recovers exactly these bytes for `op::decode`, so a
    // non-canonical op body would round-trip into an un-decodable projection input.
    cbor::require_canonical_cbor(op_bytes)
        .map_err(|error| anyhow::anyhow!("sealed op payload is not canonical CBOR: {error}"))?;

    // Finalize the header FIRST (device_fingerprint + suite/key_id), then encode — the AAD below is
    // those FINAL bytes, never a pre-finalization encode.
    let mut header = header.clone();
    header.device_fingerprint = secret.public().fingerprint();
    header.crypto_suite = SEALED_CRYPTO_SUITE;
    header.key_id = Some(key.key_id().to_bytes());
    if let Some(message) = header_nullity_error(&header) {
        anyhow::bail!(message);
    }
    let header_bytes = encode_header(&header);

    // AEAD-seal the op body with AAD = the final header content bytes; prepend the wire nonce.
    let ciphertext = seal_op_bytes(key, &nonce, op_bytes, &header_bytes)?;
    let mut payload = Vec::with_capacity(SEALED_NONCE_LEN + ciphertext.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ciphertext);

    let body_bytes = encode_body(&header_bytes, &payload);
    let signature = secret.sign(&body_bytes);
    let signed_bytes = encode_signed(&body_bytes, &signature);
    // The framed signed envelope must still fit the §18a cap — the header + CBOR framing sit on top
    // of the sealed payload the pre-seal check already bounded.
    if signed_bytes.len() > CONTENT_ENVELOPE_MAX_BYTES {
        anyhow::bail!(
            "sealed content entry is {} bytes, over the {CONTENT_ENVELOPE_MAX_BYTES}-byte §18a \
             limit",
            signed_bytes.len(),
        );
    }
    let entry_hash = cbor::sha256(&body_bytes);
    Ok(SignedContentEntry {
        header,
        payload,
        signature,
        header_bytes,
        body_bytes,
        signed_bytes,
        entry_hash,
    })
}

/// Sample a fresh OS-CSPRNG wire nonce, then [`sign_sealed_content_entry`] — the production sealing
/// entry point. A fresh 192-bit XChaCha nonce per seal makes each ciphertext unique with no
/// per-seal secret; golden tests inject the nonce directly instead.
pub fn seal_and_sign_content_entry(
    secret: &DeviceSecret,
    header: &ContentEntryHeader,
    op_bytes: &[u8],
    key: &ContentKey,
) -> anyhow::Result<SignedContentEntry> {
    sign_sealed_content_entry(secret, header, op_bytes, key, sample_wire_nonce()?)
}

/// Test-only constructor for structurally valid signed envelopes carrying arbitrary opaque suite
/// payloads. Production authoring must use the suite-specific constructors above.
#[cfg(test)]
pub(super) fn sign_opaque_content_entry_for_test(
    secret: &DeviceSecret,
    header: &ContentEntryHeader,
    payload: &[u8],
) -> anyhow::Result<SignedContentEntry> {
    let mut header = header.clone();
    header.device_fingerprint = secret.public().fingerprint();
    if let Some(message) = header_nullity_error(&header) {
        anyhow::bail!(message);
    }
    let header_bytes = encode_header(&header);
    let body_bytes = encode_body(&header_bytes, payload);
    let signature = secret.sign(&body_bytes);
    let signed_bytes = encode_signed(&body_bytes, &signature);
    let entry_hash = cbor::sha256(&body_bytes);
    Ok(SignedContentEntry {
        header,
        payload: payload.to_vec(),
        signature,
        header_bytes,
        body_bytes,
        signed_bytes,
        entry_hash,
    })
}

/// Sample a fresh 192-bit XChaCha wire nonce straight from the OS CSPRNG, returned BY VALUE. A
/// content key is long-lived, so a fresh random nonce per seal is what keeps `(key, nonce)` unique;
/// the value the seal receives is the CSPRNG output — there is no reused or hard-coded nonce
/// constant in its provenance.
fn sample_wire_nonce() -> anyhow::Result<[u8; SEALED_NONCE_LEN]> {
    let mut nonce = [0u8; SEALED_NONCE_LEN];
    // `getrandom::Error` only implements `std::error::Error` behind getrandom's `std` feature (off
    // under `--no-default-features`), so format via `Display` rather than `.context`.
    getrandom::fill(&mut nonce)
        .map_err(|e| anyhow::anyhow!("OS CSPRNG failed to sample a content-seal nonce: {e}"))?;
    Ok(nonce)
}

/// Decode structure and canonical CBOR only. Signature verification is separate.
pub fn decode_content_signed(bytes: &[u8]) -> anyhow::Result<SignedContentEntry> {
    if bytes.len() > CONTENT_ENVELOPE_MAX_BYTES {
        anyhow::bail!(
            "content entry wire is {} bytes, over the {CONTENT_ENVELOPE_MAX_BYTES}-byte §18a limit",
            bytes.len(),
        );
    }
    decode_content_signed_cbor(bytes)
        .map_err(|error| anyhow::anyhow!("content entry decode failed: {error}"))
}

/// Decode and verify the signature and signer fingerprint. Authority remains unresolved.
pub fn verify_content_signed(
    bytes: &[u8],
    public: &DevicePublic,
) -> anyhow::Result<VerifiedContentEntry> {
    let signed = decode_content_signed(bytes)?;
    public.verify(&signed.body_bytes, &signed.signature)?;
    if public.fingerprint() != signed.header.device_fingerprint {
        anyhow::bail!("device fingerprint does not match the verifying key");
    }
    Ok(VerifiedContentEntry {
        header: signed.header,
        payload: signed.payload,
        header_bytes: signed.header_bytes,
        entry_hash: signed.entry_hash,
    })
}

fn encode_header(header: &ContentEntryHeader) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(320);
    let mut encoder = Encoder::new(&mut bytes);
    encoder.array(13).expect(INFALLIBLE);
    encoder.str(CONTENT_ENTRY_DOMAIN).expect(INFALLIBLE);
    encoder.bytes(&header.stream_id.to_bytes()).expect(INFALLIBLE);
    encoder.bytes(&header.author_account_id.to_bytes()).expect(INFALLIBLE);
    encoder.bytes(&header.device_fingerprint.to_bytes()).expect(INFALLIBLE);
    encoder.u64(header.seq).expect(INFALLIBLE);
    encoder.u64(header.lamport).expect(INFALLIBLE);
    encode_opt_hash(&mut encoder, header.prev_hash);
    encode_opt_hash(&mut encoder, header.grant_id);
    encoder.bytes(&header.roster_ref).expect(INFALLIBLE);
    encoder.u64(header.owner_auth_len).expect(INFALLIBLE);
    encoder.u64(header.author_auth_len).expect(INFALLIBLE);
    encoder.u64(header.crypto_suite).expect(INFALLIBLE);
    encode_opt_hash(&mut encoder, header.key_id);
    bytes
}

fn encode_body(header_bytes: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(header_bytes.len() + payload.len() + 16);
    let mut encoder = Encoder::new(&mut bytes);
    encoder.array(2).expect(INFALLIBLE);
    encoder.bytes(header_bytes).expect(INFALLIBLE);
    encoder.bytes(payload).expect(INFALLIBLE);
    bytes
}

fn encode_signed(body_bytes: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(body_bytes.len() + 96);
    let mut encoder = Encoder::new(&mut bytes);
    encoder.array(3).expect(INFALLIBLE);
    encoder.str(CONTENT_SIGNED_DOMAIN).expect(INFALLIBLE);
    encoder.bytes(body_bytes).expect(INFALLIBLE);
    encoder.bytes(signature).expect(INFALLIBLE);
    bytes
}

/// XChaCha20-Poly1305-seal `op_bytes` under `key` with `nonce`, binding `aad`. Returns the tagged
/// ciphertext (`op_bytes.len() + 16` bytes); the caller prepends the nonce to form the wire
/// payload.
fn seal_op_bytes(
    key: &ContentKey,
    nonce: &[u8; SEALED_NONCE_LEN],
    op_bytes: &[u8],
    aad: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
        .map_err(|_| anyhow::anyhow!("content key has the wrong length"))?;
    cipher
        .encrypt(&XNonce::from(*nonce), Payload { msg: op_bytes, aad })
        .map_err(|_| anyhow::anyhow!("content AEAD sealing failed"))
}

/// Open a suite-1 `nonce[24] || ciphertext` payload under `key`, authenticating the exact retained
/// signed header bytes as AAD. The plaintext buffer is scrubbed on drop.
pub fn open_sealed_payload(
    key: &ContentKey,
    payload: &[u8],
    aad: &[u8],
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    if payload.len() < SEALED_NONCE_LEN + SEALED_AEAD_TAG_LEN {
        anyhow::bail!("sealed content payload is shorter than nonce + tag");
    }
    let (nonce, ciphertext) = payload.split_at(SEALED_NONCE_LEN);
    let nonce: [u8; SEALED_NONCE_LEN] = nonce
        .try_into()
        .map_err(|_| anyhow::anyhow!("sealed content nonce has the wrong length"))?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
        .map_err(|_| anyhow::anyhow!("content key has the wrong length"))?;
    let mut plaintext = Zeroizing::new(ciphertext.to_vec());
    cipher
        .decrypt_in_place(&XNonce::from(nonce), aad, &mut *plaintext)
        .map_err(|_| anyhow::anyhow!("sealed payload AEAD open failed"))?;
    Ok(plaintext)
}

fn encode_opt_hash(encoder: &mut Encoder<&mut Vec<u8>>, hash: Option<[u8; 32]>) {
    match hash {
        Some(hash) => encoder.bytes(&hash).expect(INFALLIBLE),
        None => encoder.null().expect(INFALLIBLE),
    };
}

fn decode_content_signed_cbor(bytes: &[u8]) -> Result<SignedContentEntry, CborError> {
    cbor::require_canonical_cbor(bytes)?;
    let mut decoder = Decoder::new(bytes);
    cbor::expect_array(&mut decoder, 3)?;
    cbor::expect_domain(&mut decoder, CONTENT_SIGNED_DOMAIN)?;
    let body_bytes = decoder.bytes()?.to_vec();
    let signature = cbor::fixed_bytes::<64>(decoder.bytes()?, "signature")?;
    let (header, header_bytes, payload) = decode_body(&body_bytes)?;
    let entry_hash = cbor::sha256(&body_bytes);
    Ok(SignedContentEntry {
        header,
        payload,
        signature,
        header_bytes,
        body_bytes,
        signed_bytes: bytes.to_vec(),
        entry_hash,
    })
}

fn decode_body(body_bytes: &[u8]) -> Result<(ContentEntryHeader, Vec<u8>, Vec<u8>), CborError> {
    cbor::require_canonical_cbor(body_bytes)?;
    let mut decoder = Decoder::new(body_bytes);
    cbor::expect_array(&mut decoder, 2)?;
    let header_bytes = decoder.bytes()?.to_vec();
    let payload = decoder.bytes()?.to_vec();
    let header = decode_header(&header_bytes)?;
    if header.crypto_suite == 0 {
        cbor::require_canonical_cbor(&payload)?;
    }
    Ok((header, header_bytes, payload))
}

fn decode_header(bytes: &[u8]) -> Result<ContentEntryHeader, CborError> {
    cbor::require_canonical_cbor(bytes)?;
    let mut decoder = Decoder::new(bytes);
    cbor::expect_array(&mut decoder, 13)?;
    cbor::expect_domain(&mut decoder, CONTENT_ENTRY_DOMAIN)?;
    let stream_id = StreamId::from_bytes(cbor::fixed_bytes::<32>(decoder.bytes()?, "stream_id")?);
    let author_account_id =
        AccountId::from_bytes(cbor::fixed_bytes::<32>(decoder.bytes()?, "author_account_id")?);
    let device_fingerprint = DeviceFingerprint::from_bytes(cbor::fixed_bytes::<32>(
        decoder.bytes()?,
        "device_fingerprint",
    )?);
    let header = ContentEntryHeader {
        stream_id,
        author_account_id,
        device_fingerprint,
        seq: decoder.u64()?,
        lamport: decoder.u64()?,
        prev_hash: decode_opt_hash(&mut decoder, "prev_hash")?,
        grant_id: decode_opt_hash(&mut decoder, "grant_id")?,
        roster_ref: cbor::fixed_bytes::<32>(decoder.bytes()?, "roster_ref")?,
        owner_auth_len: decoder.u64()?,
        author_auth_len: decoder.u64()?,
        crypto_suite: decoder.u64()?,
        key_id: decode_opt_hash(&mut decoder, "key_id")?,
    };
    if let Some(message) = header_nullity_error(&header) {
        return Err(CborError::message(message));
    }
    Ok(header)
}

fn header_nullity_error(header: &ContentEntryHeader) -> Option<&'static str> {
    if (header.seq == 0) != header.prev_hash.is_none() {
        return Some("prev_hash must be null iff seq == 0");
    }
    if (header.crypto_suite == 0) != header.key_id.is_none() {
        return Some("key_id must be null iff crypto_suite == 0");
    }
    None
}

fn decode_opt_hash(decoder: &mut Decoder<'_>, field: &str) -> Result<Option<[u8; 32]>, CborError> {
    if decoder.datatype()? == Type::Null {
        decoder.null()?;
        Ok(None)
    } else {
        Ok(Some(cbor::fixed_bytes::<32>(decoder.bytes()?, field)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> DeviceSecret {
        DeviceSecret::from_seed(&[7; 32])
    }

    fn header() -> ContentEntryHeader {
        ContentEntryHeader {
            stream_id: StreamId::from_bytes([0x11; 32]),
            author_account_id: AccountId::from_bytes([0x22; 32]),
            device_fingerprint: secret().public().fingerprint(),
            seq: 0,
            lamport: 9,
            prev_hash: None,
            grant_id: Some([0x33; 32]),
            roster_ref: [0x44; 32],
            owner_auth_len: 5,
            author_auth_len: 7,
            crypto_suite: 0,
            key_id: None,
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn content_header_and_signed_wire_are_golden_pinned() {
        let signed = sign_content_entry(&secret(), &header(), &[0x81, 0x01]).unwrap();
        assert_eq!(
            hex(&signed.header_bytes),
            "8d6f7261672d7261742f656e7472792f3358201111111111111111111111111111111111111111111111111111111111111111582022222222222222222222222222222222222222222222222222222222222222225820fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638890009f65820333333333333333333333333333333333333333333333333333333333333333358204444444444444444444444444444444444444444444444444444444444444444050700f6",
        );
        assert_eq!(
            hex(&signed.signed_bytes),
            "83767261672d7261742f7369676e65642d656e7472792f3158c88258c28d6f7261672d7261742f656e7472792f3358201111111111111111111111111111111111111111111111111111111111111111582022222222222222222222222222222222222222222222222222222222222222225820fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638890009f65820333333333333333333333333333333333333333333333333333333333333333358204444444444444444444444444444444444444444444444444444444444444444050700f64281015840fc886fdaa187e6708853dcec77a399dc03dbc094c0e125d1081eabdcdf8cf68d2765aba308b540b49c0a301b2d40148bb5d4b48a6734dca6b136769420289c08",
        );
        assert_eq!(signed.entry_hash, cbor::sha256(&signed.body_bytes));
        assert_eq!(
            hex(&signed.entry_hash),
            "a459efda17f48afb37d4beb4501461b8aef49a33fd3987cc8d2d08d46fdc4f99"
        );
    }

    #[test]
    fn sign_decode_and_verify_are_symmetric() {
        let signed = sign_content_entry(&secret(), &header(), &[0x82, 0x01, 0x02]).unwrap();
        let decoded = decode_content_signed(&signed.signed_bytes).unwrap();
        assert_eq!(decoded.header, header());
        assert_eq!(decoded.payload, [0x82, 0x01, 0x02]);
        let verified = verify_content_signed(&signed.signed_bytes, &secret().public()).unwrap();
        assert_eq!(verified.header, header());
        assert_eq!(verified.payload, [0x82, 0x01, 0x02]);
        assert_eq!(verified.header_bytes, signed.header_bytes);
        assert_eq!(verified.entry_hash, signed.entry_hash);
    }

    #[test]
    fn signer_fingerprint_is_derived_and_wrong_key_is_rejected() {
        let mut claimed = header();
        claimed.device_fingerprint = DeviceFingerprint::from_bytes([0xaa; 32]);
        let signed = sign_content_entry(&secret(), &claimed, &[0xf6]).unwrap();
        assert_eq!(signed.header.device_fingerprint, secret().public().fingerprint());
        let wrong = DeviceSecret::from_seed(&[8; 32]).public();
        assert!(verify_content_signed(&signed.signed_bytes, &wrong).is_err());

        // The signature is valid under `secret`, but the signed header names another device. This
        // reaches the explicit fingerprint binding after cryptographic verification succeeds.
        let header_bytes = encode_header(&claimed);
        let body_bytes = encode_body(&header_bytes, &[0xf6]);
        let signature = secret().sign(&body_bytes);
        let wire = encode_signed(&body_bytes, &signature);
        assert!(verify_content_signed(&wire, &secret().public()).is_err());
    }

    #[test]
    fn nullity_rules_fail_at_authoring_and_decode() {
        let mut bad_prev = header();
        bad_prev.seq = 1;
        let mut bad_genesis = header();
        bad_genesis.prev_hash = Some([1; 32]);
        let mut bad_plaintext = header();
        bad_plaintext.key_id = Some([2; 32]);
        let mut bad_sealed = header();
        bad_sealed.crypto_suite = 1;

        for bad in [bad_prev, bad_genesis, bad_plaintext, bad_sealed] {
            assert!(sign_content_entry(&secret(), &bad, &[0xf6]).is_err());
            let header_bytes = encode_header(&bad);
            let body_bytes = encode_body(&header_bytes, &[0xf6]);
            let signature = secret().sign(&body_bytes);
            let wire = encode_signed(&body_bytes, &signature);
            assert!(decode_content_signed(&wire).is_err());
        }
    }

    #[test]
    fn canonical_shape_and_fixed_width_fields_are_rejected_defensively() {
        let signed = sign_content_entry(&secret(), &header(), &[0xf6]).unwrap();
        let mut trailing = signed.signed_bytes.clone();
        trailing.push(0);
        assert!(decode_content_signed(&trailing).is_err());

        let mut short_roster = header();
        let mut bytes = encode_header(&short_roster);
        let roster_start = bytes
            .windows(34)
            .position(|window| window == [vec![0x58, 0x20], vec![0x44; 32]].concat())
            .unwrap();
        // Rewrite the canonical 32-byte bstr (`58 20` + 32 bytes) as a canonical 31-byte bstr
        // (`57` + 31 bytes). This reaches the fixed-width check instead of being rejected merely
        // because the CBOR length header is non-minimal.
        bytes.remove(roster_start + 33);
        bytes.remove(roster_start + 1);
        bytes[roster_start] = 0x57;
        let body = encode_body(&bytes, &[0xf6]);
        let signature = secret().sign(&body);
        assert!(decode_content_signed(&encode_signed(&body, &signature)).is_err());

        short_roster.roster_ref = [0x55; 32];
        assert!(sign_content_entry(&secret(), &short_roster, &[0xf6]).is_ok());
    }

    #[test]
    fn plaintext_payload_is_canonical_and_header_is_the_aad_unit() {
        let unknown_but_canonical = [0x82, 0x18, 0x2a, 0xf6];
        let a = sign_content_entry(&secret(), &header(), &unknown_but_canonical).unwrap();
        let b = sign_content_entry(&secret(), &header(), &[1]).unwrap();
        assert_eq!(a.header_bytes, b.header_bytes);
        assert_eq!(decode_content_signed(&a.signed_bytes).unwrap().payload, unknown_but_canonical);
        assert!(sign_content_entry(&secret(), &header(), &[0xff]).is_err());

        for invalid in [&[0xff][..], &[0x18, 0x00][..]] {
            let body = encode_body(&a.header_bytes, invalid);
            let signature = secret().sign(&body);
            assert!(decode_content_signed(&encode_signed(&body, &signature)).is_err());
        }
    }

    #[test]
    fn full_width_chain_and_freshness_fields_round_trip() {
        let mut full = header();
        full.seq = u64::MAX;
        full.lamport = u64::MAX;
        full.prev_hash = Some([0x88; 32]);
        full.grant_id = None;
        full.owner_auth_len = u64::MAX;
        full.author_auth_len = u64::MAX;
        let signed = sign_content_entry(&secret(), &full, &[0xf6]).unwrap();
        assert_eq!(decode_content_signed(&signed.signed_bytes).unwrap().header, full);
    }

    #[test]
    fn unknown_sealed_suite_is_retained_but_cannot_be_authored() {
        let mut sealed = header();
        sealed.crypto_suite = u64::from(u32::MAX) + 1;
        sealed.key_id = Some([0x77; 32]);
        assert!(sign_content_entry(&secret(), &sealed, &[0xff]).is_err());

        let header_bytes = encode_header(&sealed);
        let body_bytes = encode_body(&header_bytes, &[0xff, 0xfe]);
        let signature = secret().sign(&body_bytes);
        let decoded = decode_content_signed(&encode_signed(&body_bytes, &signature)).unwrap();
        assert_eq!(decoded.header.crypto_suite, u64::from(u32::MAX) + 1);
        assert_eq!(decoded.payload, [0xff, 0xfe]);
    }

    #[test]
    fn verification_rejects_header_payload_and_signature_mutations() {
        let signed = sign_content_entry(&secret(), &header(), &[0x81, 0x01]).unwrap();

        let mut changed = signed.header.clone();
        changed.author_auth_len += 1;
        let changed_header = encode_header(&changed);
        let changed_body = encode_body(&changed_header, &signed.payload);
        let changed_wire = encode_signed(&changed_body, &signed.signature);
        assert!(verify_content_signed(&changed_wire, &secret().public()).is_err());

        let changed_body = encode_body(&signed.header_bytes, &[0x81, 0x02]);
        let changed_wire = encode_signed(&changed_body, &signed.signature);
        assert!(verify_content_signed(&changed_wire, &secret().public()).is_err());

        let mut changed_signature = signed.signature;
        changed_signature[0] ^= 1;
        let changed_wire = encode_signed(&signed.body_bytes, &changed_signature);
        assert!(verify_content_signed(&changed_wire, &secret().public()).is_err());

        let mut wrong_domain = signed.header_bytes.clone();
        let domain_end = 1 + 1 + CONTENT_ENTRY_DOMAIN.len();
        wrong_domain[domain_end - 1] = b'4';
        let body = encode_body(&wrong_domain, &signed.payload);
        let signature = secret().sign(&body);
        assert!(decode_content_signed(&encode_signed(&body, &signature)).is_err());
    }

    #[test]
    fn content_account_and_legacy_envelopes_cannot_cross_decode() {
        use crate::op::MemoryOp;

        let content = sign_content_entry(&secret(), &header(), &[0xf6]).unwrap();
        assert!(crate::entry::decode_signed(&content.signed_bytes).is_err());
        assert!(
            super::super::super::envelope::decode_account_signed(&content.signed_bytes).is_err()
        );

        let legacy = crate::entry::sign_entry(
            &secret(),
            StreamId::from_bytes([0x91; 32]),
            None,
            1,
            &MemoryOp::Snapshot,
        );
        assert!(decode_content_signed(&legacy.signed_bytes).is_err());

        let account_header = super::super::super::envelope::AccountEntryHeader {
            account_id: AccountId::from_bytes([0x92; 32]),
            log_id: 0,
            device_fingerprint: secret().public().fingerprint(),
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: 0,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let account =
            super::super::super::envelope::sign_account_entry(&secret(), &account_header, &[0x80])
                .unwrap();
        assert!(decode_content_signed(&account.signed_bytes).is_err());
    }

    #[test]
    fn authoring_refuses_an_over_size_entry() {
        let too_large = vec![0; CONTENT_ENVELOPE_MAX_BYTES + 1];
        assert!(sign_content_entry(&secret(), &header(), &too_large).is_err());
        assert!(decode_content_signed(&too_large).is_err());

        // One canonical bstr whose payload itself fits the limit, but whose signed envelope does
        // not. This reaches the exact post-encoding bound rather than failing CBOR validation.
        let mut near_limit = vec![0x5a];
        near_limit.extend_from_slice(&((CONTENT_ENVELOPE_MAX_BYTES - 5) as u32).to_be_bytes());
        near_limit.resize(CONTENT_ENVELOPE_MAX_BYTES, 0);
        assert!(cbor::require_canonical_cbor(&near_limit).is_ok());
        assert!(sign_content_entry(&secret(), &header(), &near_limit).is_err());
    }

    // ── C5a: sealed suite-1 authoring (XChaCha20-Poly1305, random wire nonce) ──

    fn content_key() -> ContentKey {
        ContentKey::from_seed(&[0x20; 32])
    }

    #[test]
    fn sealed_content_wire_is_golden_pinned_under_an_injected_nonce() {
        // Deterministic: a seed-fixed content key + an injected nonce freeze the suite-1 payload
        // layout (nonce || ciphertext(op + tag)) and the signed envelope. A regression that moves
        // the nonce, changes the AAD, or drops the tag reddens here.
        //
        // The fixed nonce below is a golden-vector requirement — a static analyzer may flag it as a
        // hard-coded cryptographic value, but it is a deterministic TEST vector for the injectable
        // API; production seals via `seal_and_sign_content_entry`, which samples a random nonce.
        let key = content_key();
        let signed =
            sign_sealed_content_entry(&secret(), &header(), &[0x81, 0x01], &key, [0x5c; 24])
                .unwrap();
        // The sealed signer sets suite 1 + the key's id itself, overwriting the plaintext input.
        assert_eq!(signed.header.crypto_suite, 1);
        assert_eq!(signed.header.key_id, Some(key.key_id().to_bytes()));
        // payload = nonce[24] || ciphertext(op_bytes[2] + tag[16]) = 42 bytes.
        assert_eq!(signed.payload.len(), SEALED_NONCE_LEN + 2 + SEALED_AEAD_TAG_LEN);
        assert_eq!(&signed.payload[..SEALED_NONCE_LEN], &[0x5c; 24]);
        assert_eq!(
            hex(&signed.payload),
            "5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5cd28d288c5c101ae99a532ea8621a1487e26f",
        );
        assert_eq!(
            hex(&signed.entry_hash),
            "dd019c5639f96b715d0e1c9254b175e8d98a890e5e49e2dcdcb997f2c9e7438a",
        );
        // Byte-reproducible under the same key + nonce.
        let again =
            sign_sealed_content_entry(&secret(), &header(), &[0x81, 0x01], &key, [0x5c; 24])
                .unwrap();
        assert_eq!(signed.signed_bytes, again.signed_bytes);
    }

    #[test]
    fn a_sealed_payload_round_trips_under_the_content_key_and_header_aad() {
        let key = content_key();
        let op = [0x82, 0x01, 0x02];
        // The nonce value is not load-bearing here (round-trip holds for any nonce), so exercise
        // the random-nonce production entry point rather than a fixed injected nonce.
        let signed = seal_and_sign_content_entry(&secret(), &header(), &op, &key).unwrap();
        // AAD = the FINAL signed header content bytes (`header_bytes`), never a re-encode.
        let recovered = open_sealed_payload(&key, &signed.payload, &signed.header_bytes).unwrap();
        assert_eq!(
            recovered.as_slice(),
            op,
            "decrypt under the same key + header AAD recovers the op bytes"
        );
    }

    #[test]
    fn a_wrong_key_or_a_tampered_aad_fails_the_sealed_tag() {
        let key = content_key();
        // The nonce value is not load-bearing here, so use the random-nonce production entry point.
        let signed =
            seal_and_sign_content_entry(&secret(), &header(), &[0x81, 0x01], &key).unwrap();
        // The right key + AAD opens (control).
        assert!(open_sealed_payload(&key, &signed.payload, &signed.header_bytes).is_ok());
        // A different content key cannot open.
        let wrong = ContentKey::from_seed(&[0x21; 32]);
        assert!(open_sealed_payload(&wrong, &signed.payload, &signed.header_bytes).is_err());
        // Tampering ANY header field flips the AAD and fails the tag even under the right key — the
        // seal authenticates the whole signed header.
        let mut tampered = signed.header.clone();
        tampered.author_auth_len ^= 1;
        let tampered_aad = encode_header(&tampered);
        assert_ne!(tampered_aad, signed.header_bytes);
        assert!(open_sealed_payload(&key, &signed.payload, &tampered_aad).is_err());
    }

    #[test]
    fn sealed_open_rejects_a_short_or_tampered_payload() {
        let key = content_key();
        assert!(open_sealed_payload(&key, &[0; SEALED_NONCE_LEN], b"aad").is_err());
        let signed =
            seal_and_sign_content_entry(&secret(), &header(), &[0x81, 0x01], &key).unwrap();
        let mut tampered = signed.payload.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open_sealed_payload(&key, &tampered, &signed.header_bytes).is_err());
    }

    #[test]
    fn each_os_nonce_seal_yields_a_fresh_nonce_and_ciphertext() {
        // The wire carries the nonce, and security rests on it being FRESH per seal. Two seals of
        // the SAME op under the SAME key + header must differ in BOTH the nonce and the ciphertext;
        // a regression to a fixed/derived nonce would reuse (key, nonce) — a classic AEAD break —
        // yet still pass the injected-nonce golden above, so this is the only freshness guard.
        let key = content_key();
        let a = seal_and_sign_content_entry(&secret(), &header(), &[0x81, 0x01], &key).unwrap();
        let b = seal_and_sign_content_entry(&secret(), &header(), &[0x81, 0x01], &key).unwrap();
        assert_eq!(
            a.header_bytes, b.header_bytes,
            "same header ⇒ same AAD; only the nonce differs"
        );
        assert_ne!(
            &a.payload[..SEALED_NONCE_LEN],
            &b.payload[..SEALED_NONCE_LEN],
            "each seal samples a fresh 24-byte wire nonce",
        );
        assert_ne!(
            a.payload, b.payload,
            "a fresh nonce yields different ciphertext for the same op"
        );
        // Freshness must not cost recoverability: both open to the original op.
        assert_eq!(open_sealed_payload(&key, &a.payload, &a.header_bytes).unwrap().as_slice(), [
            0x81, 0x01
        ],);
        assert_eq!(open_sealed_payload(&key, &b.payload, &b.header_bytes).unwrap().as_slice(), [
            0x81, 0x01
        ],);
    }

    #[test]
    fn a_sealed_entry_decodes_and_verifies_with_an_opaque_suite_one_payload() {
        // The already-sealed-ready ingest/decode path treats a suite-1 payload as opaque: it
        // decodes + verifies (signature + fingerprint) without touching the ciphertext, and the AAD
        // unit (`header_bytes`) round-trips for projection decrypt.
        let key = content_key();
        // The nonce value is not load-bearing here, so use the random-nonce production entry point.
        let signed =
            seal_and_sign_content_entry(&secret(), &header(), &[0x82, 0x01, 0x02], &key).unwrap();
        let decoded = decode_content_signed(&signed.signed_bytes).unwrap();
        assert_eq!(decoded.header.crypto_suite, 1);
        assert_eq!(decoded.payload, signed.payload, "the sealed payload is retained opaque");
        let verified = verify_content_signed(&signed.signed_bytes, &secret().public()).unwrap();
        assert_eq!(verified.header_bytes, signed.header_bytes, "the AAD unit round-trips");
    }

    #[test]
    fn sealing_rejects_a_non_canonical_op_body() {
        // The op body is the plaintext that becomes ciphertext; a non-canonical body is refused up
        // front (the opaque suite-1 payload can't be canonicity-checked at decode).
        let key = content_key();
        // `0x18 0x00` is a non-minimal (non-canonical) encoding of 0.
        assert!(
            sign_sealed_content_entry(&secret(), &header(), &[0x18, 0x00], &key, [0x44; 24])
                .is_err()
        );
    }

    #[test]
    fn sealing_refuses_an_over_size_op_before_the_aead() {
        // The sealed wire payload adds a fixed 40 bytes (24-byte nonce + 16-byte tag), so an op
        // body over `CONTENT_ENVELOPE_MAX_BYTES - 40` can never fit the §18a envelope. The
        // bound is enforced BEFORE the AEAD encrypt/allocate; drive it through the
        // random-nonce production entry point so no fixed nonce is involved. The sealed
        // twin of `authoring_refuses_an_over_size_entry`.
        let key = content_key();
        let sealed_body_max = CONTENT_ENVELOPE_MAX_BYTES - (SEALED_NONCE_LEN + SEALED_AEAD_TAG_LEN);
        // A canonical bstr one byte past the sealed body bound (`0x5a` + 4-byte length + content).
        let mut over = vec![0x5a];
        over.extend_from_slice(&((sealed_body_max + 1 - 5) as u32).to_be_bytes());
        over.resize(sealed_body_max + 1, 0);
        assert!(cbor::require_canonical_cbor(&over).is_ok(), "the op body is canonical CBOR");
        assert!(
            seal_and_sign_content_entry(&secret(), &header(), &over, &key).is_err(),
            "an op body over CAP - 40 is refused before the AEAD seal",
        );
    }
}
