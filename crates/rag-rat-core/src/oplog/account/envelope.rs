//! The signed account-entry envelope — `rag-rat/account-entry/1` / `rag-rat/account-signed/1` (§6).
//!
//! An account log entry wraps an opaque control/secrets-op payload in a per-`(account, log,
//! device)` signed, hash-chained record. Three nested canonical-CBOR objects, all definite-length +
//! minimal-header (the same discipline [`super::super::cbor`] enforces):
//!
//! ```text
//! header_bytes = cbor([ "rag-rat/account-entry/1", account_id, log_id, device_fingerprint, seq,
//!                       prev_hash|null, parent_ref|null, entry_type, op_version, crypto_suite,
//!                       auth_len, key_id|null, authority_ref|null ])          -- 13 parts, fixed order
//! body_bytes   = cbor([ header_bytes (bstr), payload (bstr) ])
//! signed_bytes = cbor([ "rag-rat/account-signed/1", body_bytes (bstr), signature (64-byte bstr) ])
//! ```
//!
//! - The header is a SELF-CONTAINED canonical-CBOR **bstr** inside the body — it is the AAD unit
//!   for the C4/C5 sealed suites, so a decryptor authenticates the exact header content. `payload`
//!   is ALWAYS a bstr (plaintext canonical-CBOR op bytes today; ciphertext once sealing ships).
//! - **The signature covers `body_bytes`** (header + payload), and `entry_hash =
//!   sha256(body_bytes)` is the chain link + content address. The AEAD AAD is the header
//!   **content** bytes ([`header_aad`]) — NOT the bstr wrapper.
//! - `entry_type` / `op_version` / `auth_len` / `authority_ref` are carried as opaque scalars here,
//!   exactly as [`super::super::entry`] carries `op_bytes` opaquely: this layer validates the
//!   SELF-CONTAINED structural rules (canonicity, arity, `prev_hash` ⇔ `seq==0`, `key_id` ⇔
//!   `crypto_suite==0`) and leaves every entry-type-SEMANTIC rule (the `authority_ref` ⇔
//!   `AccountGenesis` coupling, payload shape, authority) to the ops/fold layers that interpret
//!   them.

use minicbor::Encoder;
use minicbor::data::Type;
use minicbor::decode::{Decoder, Error as CborError};

use super::AccountId;
use super::limits::{ACCOUNT_ENTRY_DOMAIN, ACCOUNT_ENVELOPE_MAX_BYTES, ACCOUNT_SIGNED_DOMAIN};
use crate::oplog::cbor;
use crate::oplog::device::{DevicePublic, DeviceSecret};
use crate::oplog::op::DeviceFingerprint;

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible) — mirrors `super::super`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// The 13-part account-entry header (§6), fixed field order. `domain` (part 0) is a constant and is
/// not stored — it is written at encode and asserted at decode. `entry_type` and `authority_ref`
/// are opaque to this layer; the ops/fold layers interpret them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AccountEntryHeader {
    pub(super) account_id: AccountId,
    pub(super) log_id: u8,
    pub(super) device_fingerprint: DeviceFingerprint,
    pub(super) seq: u64,
    pub(super) prev_hash: Option<[u8; 32]>,
    pub(super) parent_ref: Option<[u8; 32]>,
    pub(super) entry_type: u32,
    pub(super) op_version: u32,
    pub(super) crypto_suite: u8,
    pub(super) auth_len: u64,
    pub(super) key_id: Option<[u8; 32]>,
    pub(super) authority_ref: Option<[u8; 32]>,
}

/// A signed account-entry: its structured header, the opaque payload, the signature over
/// `body_bytes`, and all three wire encodings. `signed_bytes` is the transport/storage form;
/// `header_bytes` is the AAD unit; `body_bytes` is exactly what the signature and `entry_hash`
/// cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SignedAccountEntry {
    pub(super) header: AccountEntryHeader,
    pub(super) payload: Vec<u8>,
    pub(super) signature: [u8; 64],
    pub(super) header_bytes: Vec<u8>,
    pub(super) body_bytes: Vec<u8>,
    pub(super) signed_bytes: Vec<u8>,
    pub(super) entry_hash: [u8; 32],
}

/// A decoded + cryptographically-verified account entry (payload left opaque).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VerifiedAccountEntry {
    pub(super) header: AccountEntryHeader,
    pub(super) payload: Vec<u8>,
    pub(super) entry_hash: [u8; 32],
}

/// Author a signed account entry: encode the header, wrap `[header, payload]` as the body, sign the
/// body under `secret`, and build the transport envelope. Pure + deterministic given `secret`
/// (ed25519 signing is deterministic). The author device is DERIVED from `secret` — the header's
/// `device_fingerprint` is overwritten with `secret.public().fingerprint()` — so a signed entry can
/// never name a device other than its signer (mirrors `super::super::entry::sign_entry`).
pub(super) fn sign_account_entry(
    secret: &DeviceSecret,
    header: &AccountEntryHeader,
    payload: &[u8],
) -> SignedAccountEntry {
    // The signing key IS the author device: derive `device_fingerprint` from `secret` rather than
    // trusting the caller-supplied field. A `debug_assert` would be compiled out in release and let
    // a release-build authoring bug ship a self-invalid entry; overwriting is a structural
    // guarantee that holds in every build.
    let mut header = header.clone();
    header.device_fingerprint = secret.public().fingerprint();
    let header_bytes = encode_header(&header);
    let body_bytes = encode_body(&header_bytes, payload);
    let entry_hash = cbor::sha256(&body_bytes);
    let signature = secret.sign(&body_bytes);
    let signed_bytes = encode_signed(&body_bytes, &signature);
    SignedAccountEntry {
        header,
        payload: payload.to_vec(),
        signature,
        header_bytes,
        body_bytes,
        signed_bytes,
        entry_hash,
    }
}

/// Decode a signed account entry, STRUCTURE only (no signature check — that is
/// [`verify_account_signed`]). Enforces the §18a size limit, canonicity at every nesting level, the
/// arities + domains, and the self-contained header nullity rules.
pub(super) fn decode_account_signed(bytes: &[u8]) -> anyhow::Result<SignedAccountEntry> {
    if bytes.len() > ACCOUNT_ENVELOPE_MAX_BYTES {
        anyhow::bail!(
            "account entry wire is {} bytes, over the {ACCOUNT_ENVELOPE_MAX_BYTES}-byte §18a limit",
            bytes.len(),
        );
    }
    decode_account_signed_cbor(bytes)
        .map_err(|err| anyhow::anyhow!("account entry decode failed: {err}"))
}

/// Decode + cryptographically verify a signed account entry under `pubkey`: `verify_strict` over
/// `body_bytes`, then assert `pubkey.fingerprint() == header.device_fingerprint` so the key is
/// bound to the device the header names (mirrors [`super::super::entry::verify_signed`]).
pub(super) fn verify_account_signed(
    bytes: &[u8],
    pubkey: &DevicePublic,
) -> anyhow::Result<VerifiedAccountEntry> {
    let signed = decode_account_signed(bytes)?;
    pubkey.verify(&signed.body_bytes, &signed.signature)?;
    if pubkey.fingerprint() != signed.header.device_fingerprint {
        anyhow::bail!("device fingerprint does not match the verifying key");
    }
    Ok(VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    })
}

/// The AAD unit for the sealed suites (C4/C5): the header CONTENT bytes (the inner canonical CBOR),
/// NOT the bstr wrapper the body uses to carry them. A decryptor authenticates exactly these bytes.
pub(super) fn header_aad(signed: &SignedAccountEntry) -> &[u8] {
    &signed.header_bytes
}

/// Encode the 13-part header array (§6). `Option` hashes become CBOR `null`; every integer is
/// minimal (minicbor emits the smallest form), so the encoding is canonical.
fn encode_header(h: &AccountEntryHeader) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(13).expect(INFALLIBLE);
        enc.str(ACCOUNT_ENTRY_DOMAIN).expect(INFALLIBLE);
        enc.bytes(&h.account_id.to_bytes()).expect(INFALLIBLE);
        enc.u8(h.log_id).expect(INFALLIBLE);
        enc.bytes(&h.device_fingerprint.to_bytes()).expect(INFALLIBLE);
        enc.u64(h.seq).expect(INFALLIBLE);
        encode_opt_hash(&mut enc, h.prev_hash);
        encode_opt_hash(&mut enc, h.parent_ref);
        enc.u32(h.entry_type).expect(INFALLIBLE);
        enc.u32(h.op_version).expect(INFALLIBLE);
        enc.u8(h.crypto_suite).expect(INFALLIBLE);
        enc.u64(h.auth_len).expect(INFALLIBLE);
        encode_opt_hash(&mut enc, h.key_id);
        encode_opt_hash(&mut enc, h.authority_ref);
    }
    buf
}

/// Encode the body: `[header_bytes (bstr), payload (bstr)]`. THESE are the bytes the signature and
/// `entry_hash` cover.
fn encode_body(header_bytes: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(header_bytes.len() + payload.len() + 16);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(2).expect(INFALLIBLE);
        enc.bytes(header_bytes).expect(INFALLIBLE);
        enc.bytes(payload).expect(INFALLIBLE);
    }
    buf
}

/// Encode the outer transport envelope: `[domain, body_bytes (bstr), signature (bstr)]`. NOT
/// signed.
fn encode_signed(body_bytes: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(body_bytes.len() + 96);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(3).expect(INFALLIBLE);
        enc.str(ACCOUNT_SIGNED_DOMAIN).expect(INFALLIBLE);
        enc.bytes(body_bytes).expect(INFALLIBLE);
        enc.bytes(signature).expect(INFALLIBLE);
    }
    buf
}

/// A 32-byte bstr for `Some`, CBOR `null` for `None` — the distinguished "no value" marker (never
/// an all-zero hash).
fn encode_opt_hash(enc: &mut Encoder<&mut Vec<u8>>, hash: Option<[u8; 32]>) {
    match hash {
        Some(hash) => {
            enc.bytes(&hash).expect(INFALLIBLE);
        },
        None => {
            enc.null().expect(INFALLIBLE);
        },
    }
}

fn decode_account_signed_cbor(bytes: &[u8]) -> Result<SignedAccountEntry, CborError> {
    // The whole wire is exactly one canonical CBOR item, no trailing bytes.
    cbor::require_canonical_cbor(bytes)?;
    let mut d = Decoder::new(bytes);
    cbor::expect_array(&mut d, 3)?;
    cbor::expect_domain(&mut d, ACCOUNT_SIGNED_DOMAIN)?;
    let body_bytes = d.bytes()?.to_vec();
    let signature = cbor::fixed_bytes::<64>(d.bytes()?, "signature")?;
    let (header, header_bytes, payload) = decode_body(&body_bytes)?;
    let entry_hash = cbor::sha256(&body_bytes);
    Ok(SignedAccountEntry {
        header,
        payload,
        signature,
        header_bytes,
        body_bytes,
        signed_bytes: bytes.to_vec(),
        entry_hash,
    })
}

fn decode_body(body_bytes: &[u8]) -> Result<(AccountEntryHeader, Vec<u8>, Vec<u8>), CborError> {
    // The body must independently be exactly one canonical CBOR item; the header bstr is validated
    // in turn by `decode_header`, and `payload` stays an opaque bstr.
    cbor::require_canonical_cbor(body_bytes)?;
    let mut d = Decoder::new(body_bytes);
    cbor::expect_array(&mut d, 2)?;
    let header_bytes = d.bytes()?.to_vec();
    let payload = d.bytes()?.to_vec();
    let header = decode_header(&header_bytes)?;
    Ok((header, header_bytes, payload))
}

fn decode_header(header_bytes: &[u8]) -> Result<AccountEntryHeader, CborError> {
    cbor::require_canonical_cbor(header_bytes)?;
    let mut d = Decoder::new(header_bytes);
    cbor::expect_array(&mut d, 13)?;
    cbor::expect_domain(&mut d, ACCOUNT_ENTRY_DOMAIN)?;
    let account_id = AccountId::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "account_id")?);
    let log_id = d.u8()?;
    let device_fingerprint =
        DeviceFingerprint::from_bytes(cbor::fixed_bytes::<32>(d.bytes()?, "device_fingerprint")?);
    let seq = d.u64()?;
    let prev_hash = decode_opt_hash(&mut d, "prev_hash")?;
    let parent_ref = decode_opt_hash(&mut d, "parent_ref")?;
    let entry_type = d.u32()?;
    let op_version = d.u32()?;
    let crypto_suite = d.u8()?;
    let auth_len = d.u64()?;
    let key_id = decode_opt_hash(&mut d, "key_id")?;
    let authority_ref = decode_opt_hash(&mut d, "authority_ref")?;
    // Self-contained structural nullity rules (§6). The `authority_ref` ⇔ `AccountGenesis` coupling
    // needs entry_type SEMANTICS and is enforced where entry_type is interpreted (ops / fold).
    if (seq == 0) != prev_hash.is_none() {
        return Err(CborError::message("prev_hash must be null iff seq == 0"));
    }
    if (crypto_suite == 0) != key_id.is_none() {
        return Err(CborError::message("key_id must be null iff crypto_suite == 0"));
    }
    Ok(AccountEntryHeader {
        account_id,
        log_id,
        device_fingerprint,
        seq,
        prev_hash,
        parent_ref,
        entry_type,
        op_version,
        crypto_suite,
        auth_len,
        key_id,
        authority_ref,
    })
}

/// Decode a hash slot: CBOR null → `None`, else a 32-byte bstr.
fn decode_opt_hash(d: &mut Decoder<'_>, field: &str) -> Result<Option<[u8; 32]>, CborError> {
    if d.datatype()? == Type::Null {
        d.null()?;
        Ok(None)
    } else {
        Ok(Some(cbor::fixed_bytes::<32>(d.bytes()?, field)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn secret() -> DeviceSecret {
        DeviceSecret::from_seed(&[7u8; 32])
    }

    fn account() -> AccountId {
        AccountId::from_bytes([0xaau8; 32])
    }

    /// A valid genesis-shaped header: control log, seq 0, no predecessor / parent / authority,
    /// plaintext suite, `auth_len` 0. The device fingerprint is the fixture key's.
    fn genesis_header() -> AccountEntryHeader {
        AccountEntryHeader {
            account_id: account(),
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
        }
    }

    /// A fixed opaque payload — the envelope treats it as bytes.
    fn payload() -> Vec<u8> {
        vec![0x85, 0x01, 0x02, 0x03, 0x04]
    }

    #[test]
    fn account_signed_pins_the_envelope() {
        // Frozen primitive: the signature, entry_hash, and chain all build on these bytes; a
        // canonical-rule change must break this and force a deliberate domain bump.
        let signed = sign_account_entry(&secret(), &genesis_header(), &payload());
        assert_eq!(
            hex(&signed.body_bytes),
            "8258678d777261672d7261742f6163636f756e742d656e7472792f315820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa005820fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff55226388900f6f600010000f6f6458501020304",
            "body_bytes golden",
        );
        assert_eq!(
            hex(&signed.signed_bytes),
            "8378187261672d7261742f6163636f756e742d7369676e65642f3158708258678d777261672d7261742f6163636f756e742d656e7472792f315820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa005820fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff55226388900f6f600010000f6f645850102030458405ddb630fd5cb881a048e989ed09ff5d4b6b259a178430d08cf3e55dfafaaae78c71eee26b6cc4671bd5d5ed9d43aadc6123d7dcfc946cc5427d6df150a92ff02",
            "signed_bytes golden",
        );
    }

    #[test]
    fn header_bytes_have_the_expected_13_part_structure() {
        let signed = sign_account_entry(&secret(), &genesis_header(), &payload());
        let h = &signed.header_bytes;
        assert_eq!(h[0], 0x8d, "13-element array header");
        assert_eq!(h[1], 0x77, "text header, len 23");
        assert_eq!(&h[2..25], b"rag-rat/account-entry/1");
        assert_eq!(&h[25..27], &[0x58, 0x20], "account_id bstr header, len 32");
        assert_eq!(&h[27..59], &account().to_bytes(), "account_id bytes");
        assert_eq!(h[59], 0x00, "log_id 0, minimal uint");
        assert_eq!(&h[60..62], &[0x58, 0x20], "device_fingerprint bstr header");
        assert_eq!(&h[62..94], &secret().public().fingerprint().to_bytes(), "device_fingerprint");
        assert_eq!(h[94], 0x00, "seq 0");
        assert_eq!(h[95], 0xf6, "prev_hash null (genesis)");
        assert_eq!(h[96], 0xf6, "parent_ref null (genesis)");
        assert_eq!(h[97], 0x00, "entry_type 0");
        assert_eq!(h[98], 0x01, "op_version 1");
        assert_eq!(h[99], 0x00, "crypto_suite 0 (plaintext)");
        assert_eq!(h[100], 0x00, "auth_len 0");
        assert_eq!(h[101], 0xf6, "key_id null (plaintext)");
        assert_eq!(h[102], 0xf6, "authority_ref null (genesis)");
        assert_eq!(h.len(), 103);
        cbor::require_canonical_cbor(h).expect("header is canonical CBOR");
    }

    #[test]
    fn body_wraps_header_and_payload_as_bstrs() {
        let signed = sign_account_entry(&secret(), &genesis_header(), &payload());
        let b = &signed.body_bytes;
        assert_eq!(b[0], 0x82, "2-element array (header, payload)");
        assert_eq!(b[1], 0x58, "header bstr header (1-byte len)");
        assert_eq!(b[2], 0x67, "header len 103");
        assert_eq!(&b[3..106], signed.header_bytes.as_slice(), "header content");
        assert_eq!(b[106], 0x45, "payload bstr, len 5");
        assert_eq!(&b[107..112], payload().as_slice(), "payload content");
        assert_eq!(
            signed.entry_hash,
            cbor::sha256(&signed.body_bytes),
            "entry_hash = sha256(body)"
        );
    }

    #[test]
    fn round_trips_through_verify() {
        let signed = sign_account_entry(&secret(), &genesis_header(), &payload());
        let verified =
            verify_account_signed(&signed.signed_bytes, &secret().public()).expect("verifies");
        assert_eq!(verified.header, genesis_header());
        assert_eq!(verified.payload, payload());
        assert_eq!(verified.entry_hash, signed.entry_hash);
        // Structural decode alone yields the same header + payload.
        let decoded = decode_account_signed(&signed.signed_bytes).unwrap();
        assert_eq!(decoded.header, genesis_header());
        assert_eq!(decoded.payload, payload());
    }

    #[test]
    fn verify_rejects_the_wrong_key_and_a_fingerprint_mismatch() {
        let signed = sign_account_entry(&secret(), &genesis_header(), &payload());
        let wrong = DeviceSecret::from_seed(&[9u8; 32]).public();
        assert!(verify_account_signed(&signed.signed_bytes, &wrong).is_err(), "wrong key");

        // A header claiming device B's fingerprint but signed by A must fail under both keys. Build
        // the wire MANUALLY — `sign_account_entry`'s debug_assert deliberately refuses to author a
        // header/key mismatch, so this exercises verify's fingerprint bind at the wire level.
        let secret_a = secret();
        let secret_b = DeviceSecret::from_seed(&[9u8; 32]);
        let mut forged = genesis_header();
        forged.device_fingerprint = secret_b.public().fingerprint();
        let header_bytes = encode_header(&forged);
        let body_bytes = encode_body(&header_bytes, &payload());
        let signature = secret_a.sign(&body_bytes);
        let forged_wire = encode_signed(&body_bytes, &signature);
        assert!(
            verify_account_signed(&forged_wire, &secret_a.public()).is_err(),
            "A's key vs a header claiming B",
        );
        assert!(
            verify_account_signed(&forged_wire, &secret_b.public()).is_err(),
            "B's key vs a body A signed",
        );
    }

    #[test]
    fn tampering_any_byte_is_rejected() {
        // Every byte of the wire is load-bearing: flipping one in the outer frame / domain / body
        // bstr header breaks the structural decode, and flipping one in the body or signature
        // breaks the signature. So verification must fail no matter which byte is flipped.
        let signed = sign_account_entry(&secret(), &genesis_header(), &payload());
        for index in 0..signed.signed_bytes.len() {
            let mut wire = signed.signed_bytes.clone();
            wire[index] ^= 0x01;
            assert!(
                verify_account_signed(&wire, &secret().public()).is_err(),
                "flipping byte {index} must fail decode or verification",
            );
        }
    }

    #[test]
    fn structural_rejections() {
        let good = sign_account_entry(&secret(), &genesis_header(), &payload());
        // Trailing bytes.
        let mut trailing = good.signed_bytes.clone();
        trailing.push(0x00);
        assert!(decode_account_signed(&trailing).is_err(), "trailing bytes");
        // Empty / non-array.
        assert!(decode_account_signed(&[0x00]).is_err(), "not an array");
        // Over the §18a size limit.
        let huge = vec![0u8; ACCOUNT_ENVELOPE_MAX_BYTES + 1];
        assert!(decode_account_signed(&huge).is_err(), "over the §18a limit");
    }

    #[test]
    fn prev_hash_and_key_id_nullity_rules_are_enforced() {
        // seq > 0 with a null prev_hash is rejected.
        let mut bad_prev = genesis_header();
        bad_prev.seq = 5;
        bad_prev.prev_hash = None;
        let signed = sign_account_entry(&secret(), &bad_prev, &payload());
        assert!(decode_account_signed(&signed.signed_bytes).is_err(), "seq>0 needs prev_hash");

        // seq 0 with a non-null prev_hash is rejected.
        let mut bad_genesis = genesis_header();
        bad_genesis.prev_hash = Some([0x11; 32]);
        let signed = sign_account_entry(&secret(), &bad_genesis, &payload());
        assert!(decode_account_signed(&signed.signed_bytes).is_err(), "genesis has no prev_hash");

        // crypto_suite 0 (plaintext) with a non-null key_id is rejected (§15).
        let mut bad_key = genesis_header();
        bad_key.key_id = Some([0x22; 32]);
        let signed = sign_account_entry(&secret(), &bad_key, &payload());
        assert!(decode_account_signed(&signed.signed_bytes).is_err(), "plaintext has no key_id");

        // crypto_suite 1 (sealed) with a null key_id is rejected.
        let mut bad_sealed = genesis_header();
        bad_sealed.crypto_suite = 1;
        bad_sealed.key_id = None;
        let signed = sign_account_entry(&secret(), &bad_sealed, &payload());
        assert!(decode_account_signed(&signed.signed_bytes).is_err(), "sealed needs a key_id");
    }

    #[test]
    fn header_aad_is_the_header_content_not_the_payload() {
        // Same header, different payload ⇒ identical AAD (the header bytes); the payload is not in
        // it.
        let a = sign_account_entry(&secret(), &genesis_header(), &payload());
        let b = sign_account_entry(&secret(), &genesis_header(), &[0xff, 0xfe]);
        assert_eq!(header_aad(&a), header_aad(&b), "AAD is independent of the payload");
        assert_eq!(header_aad(&a), a.header_bytes.as_slice(), "AAD is the header content bytes");
        // A changed header field changes the AAD.
        let mut other = genesis_header();
        other.auth_len = 9;
        let c = sign_account_entry(&secret(), &other, &payload());
        assert_ne!(header_aad(&a), header_aad(&c), "a header change changes the AAD");
    }
}
