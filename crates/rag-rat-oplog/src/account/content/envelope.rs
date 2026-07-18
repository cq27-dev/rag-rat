//! The `/3` content envelope — `rag-rat/entry/3` inside `rag-rat/signed-entry/1` (§8).
//!
//! The signature covers `body_bytes = cbor([header_bytes, payload])`; the header content bytes are
//! the future C5 AEAD AAD. The payload stays opaque here so plaintext and sealed entries have the
//! same chain and signature semantics.

use minicbor::Encoder;
use minicbor::data::Type;
use minicbor::decode::{Decoder, Error as CborError};

use super::super::AccountId;
use super::super::limits::{
    CONTENT_ENTRY_DOMAIN, CONTENT_ENVELOPE_MAX_BYTES, CONTENT_SIGNED_DOMAIN,
};
use crate::cbor;
use crate::device::{DevicePublic, DeviceSecret};
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

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
}
