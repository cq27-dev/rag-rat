//! Content-key crypto primitives — content-key generation, the deterministic `key_id`, and the
//! X25519 sealed-box that wraps a stream's content key to one recipient device (sync phase C4.1,
//! #607). Pure crypto: no DB, no async, no I/O beyond the OS CSPRNG.
//!
//! # Construction (hand-rolled HPKE base mode over DHKEM(X25519, HKDF-SHA256))
//!
//! [`seal_content_key`] is single-shot HPKE base mode (RFC 9180 §5.1.1) with an ephemeral X25519
//! sender key, mapped field-by-field to the RFC 9180 §4.1 DHKEM as follows:
//! - `Encap`: sample an ephemeral `(esk, epk)`; `dh = DH(esk, pkR)`; the wire `enc` is `epk`. `dh`
//!   is asserted contributory (RFC 7748 §6.1) and a non-contributory (all-zero) result is refused —
//!   the output check that backstops the small-order/identity blocklist on
//!   [`DeviceX25519Public::from_bytes`], which [`unwrap_content_key`] runs over `epk` first.
//! - `ExtractAndExpand(dh, kem_context)`: `HKDF-Extract(salt = "", ikm = dh)` then
//!   `HKDF-Expand(info)`, where `kem_context = epk || pkR` and `info = label || epk || pkR`. The
//!   `label` (`rag-rat/key-wrap/1` for the AEAD key, `rag-rat/key-wrap-nonce/1` for the nonce)
//!   stands in for RFC 9180's labeled-HKDF `suite_id` domain separation. The 32-byte AEAD key and
//!   the 24-byte nonce derive from the SAME PRK, so the wire carries no nonce: a fresh ephemeral
//!   per wrap yields a fresh PRK, hence a fresh `(key, nonce)` pair with no reuse.
//! - Seal: `XChaCha20-Poly1305(key, nonce).seal(pt = content_key, aad = WrapContext bytes)`. The
//!   [`WrapContext`] (`account_id || stream_id || key_epoch || recipient_pub`) is bound as the AEAD
//!   AAD so a wrap cannot be transplanted across account, stream, key epoch, or recipient.
//!
//! [`unwrap_content_key`] is the matching `Decap` + `Open`: validate `epk` through the small-order
//! blocklist, `dh = DH(skR, epk)`, assert contributory, re-derive `(key, nonce)` identically, and
//! open in place. A tag failure returns an OPAQUE error — no plaintext, and nothing distinguishing
//! a wrong key from a tampered ciphertext (no decryption oracle).
//!
//! # Crate properties relied on
//! `chacha20poly1305` 0.11 does constant-time (`subtle`) Poly1305 tag comparison and zeroizes its
//! cipher key on drop. Every secret local here — the [`ContentKey`], the ephemeral secret, the
//! derived wrap key, and the recovered-plaintext buffer — is `Zeroizing`. The HKDF PRK residue held
//! inside `Hkdf` is accepted (not separately scrubbed).

use anyhow::Context;
use chacha20poly1305::aead::{Aead, AeadInOut, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use minicbor::Encoder;
use minicbor::decode::{Decoder, Error as CborError};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::oplog::cbor;
use crate::oplog::device::{DeviceX25519Public, DeviceX25519Secret};

const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// Domain tag for the canonical-CBOR wire form of a [`SealedKeyWrap`].
const KEY_WRAP_DOMAIN: &str = "rag-rat/key-wrap/1";

/// HKDF `info` for the content-key id — one-way, non-circular (nothing derives FROM `key_id`).
const CONTENT_KEY_ID_INFO: &[u8] = b"rag-rat/content-key-id/1";
/// HKDF `info` prefix (before `epk || recipient_pub`) for the per-wrap AEAD key.
const KEY_WRAP_KDF_INFO: &[u8] = b"rag-rat/key-wrap/1";
/// HKDF `info` prefix (before `epk || recipient_pub`) for the per-wrap AEAD nonce — derived, never
/// stored on the wire.
const KEY_WRAP_NONCE_INFO: &[u8] = b"rag-rat/key-wrap-nonce/1";

/// The RFC 5869 empty salt. `HKDF-Extract(salt = "", ...)` matches the DHKEM `ExtractAndExpand`.
const EMPTY_SALT: &[u8] = &[];

/// The fixed-width AEAD AAD length: `account_id(32) || stream_id(32) || key_epoch(8, BE) ||
/// recipient_pub(32)`.
const WRAP_CONTEXT_AAD_LEN: usize = 32 + 32 + 8 + 32;

/// A stream's symmetric content key — the XChaCha20-Poly1305 key that C5 seals content entries
/// under. Independent random material: `key_id` is a one-way function OF it, nothing derives it.
/// Intentionally not `Debug` / `Clone` (secret material should not be trivially printed or copied);
/// the 32 bytes are scrubbed on drop.
pub(crate) struct ContentKey(Zeroizing<[u8; 32]>);

impl ContentKey {
    /// Mint a FRESH content key from OS entropy — the production path (the same CSPRNG source
    /// [`DeviceX25519Secret::generate`] uses). Fails only if the OS CSPRNG is unavailable.
    pub(crate) fn generate() -> anyhow::Result<Self> {
        let mut key = Zeroizing::new([0u8; 32]);
        getrandom::fill(key.as_mut_slice())
            .map_err(|e| anyhow::anyhow!("OS CSPRNG failed to generate a content key: {e}"))?;
        Ok(Self(key))
    }

    /// Build a content key from fixed bytes — deterministic, for golden vectors (mirrors the
    /// device-key `from_seed` split). The bytes ARE the key; no derivation.
    pub(crate) fn from_seed(seed: &[u8; 32]) -> Self {
        Self(Zeroizing::new(*seed))
    }

    /// The 32-byte key material.
    pub(crate) fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// The content-key id carried in the `/3` content-entry header:
    /// `HKDF-SHA256(ikm = key, salt = "", info = "rag-rat/content-key-id/1")`, expanded to 32
    /// bytes. One-way: publishing `key_id` in a clear header leaks nothing about the key, and a
    /// device that unwrapped the key can match an entry's `key_id` to it (and cross-check its
    /// peers' `key_id` at an epoch before adopting a key for sealing — the C4.3 authority
    /// cross-check).
    pub(crate) fn key_id(&self) -> KeyId {
        let hk = Hkdf::<Sha256>::new(Some(EMPTY_SALT), self.as_slice());
        let mut out = [0u8; 32];
        hk.expand(CONTENT_KEY_ID_INFO, &mut out)
            .expect("HKDF-SHA256 expand of 32 bytes is within the 255*HashLen bound");
        KeyId(out)
    }
}

/// The non-circular content-key identifier (32 bytes — the frozen `/3` header pins
/// `key_id: Option<[u8; 32]>`). Public: derived one-way from the key, safe to carry in the clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyId([u8; 32]);

impl KeyId {
    /// Wrap 32 header bytes as a [`KeyId`] (the decode-side counterpart to
    /// [`to_bytes`](Self::to_bytes)).
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 32-byte id.
    pub(crate) fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// The transplant binding sealed into every wrap as AEAD AAD. Bundling `account_id` (a device can
/// belong to several accounts via guest hosting), `stream_id`, `key_epoch`, and `recipient_pub`
/// means a wrap opens ONLY under the exact context it was sealed for — it can't be replayed against
/// another account, stream, epoch, or device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrapContext {
    pub(crate) account_id: [u8; 32],
    pub(crate) stream_id: [u8; 32],
    pub(crate) key_epoch: u64,
    pub(crate) recipient_pub: [u8; 32],
}

impl WrapContext {
    /// The ONE canonical fixed-width encoding: `account_id || stream_id || key_epoch(BE) ||
    /// recipient_pub` (104 bytes). Used verbatim as the AEAD AAD; golden-pinned.
    pub(crate) fn to_bytes(&self) -> [u8; WRAP_CONTEXT_AAD_LEN] {
        let mut out = [0u8; WRAP_CONTEXT_AAD_LEN];
        out[0..32].copy_from_slice(&self.account_id);
        out[32..64].copy_from_slice(&self.stream_id);
        out[64..72].copy_from_slice(&self.key_epoch.to_be_bytes());
        out[72..104].copy_from_slice(&self.recipient_pub);
        out
    }
}

/// A content key sealed to one device: the ephemeral X25519 public key plus the 48-byte tagged
/// ciphertext (32-byte key + 16-byte Poly1305 tag). No stored nonce — it is HKDF-derived on both
/// sides from `epk || recipient_pub`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SealedKeyWrap {
    pub(crate) ephemeral_pubkey: [u8; 32],
    pub(crate) ciphertext: [u8; 48],
}

impl SealedKeyWrap {
    /// The canonical CBOR wire form: `[domain, epk, ciphertext]` (definite-length, domain-tagged
    /// `rag-rat/key-wrap/1`), so a `key_wrap` op payload can carry it.
    pub(crate) fn to_cbor(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(96);
        let mut encoder = Encoder::new(&mut bytes);
        encoder.array(3).expect(INFALLIBLE);
        encoder.str(KEY_WRAP_DOMAIN).expect(INFALLIBLE);
        encoder.bytes(&self.ephemeral_pubkey).expect(INFALLIBLE);
        encoder.bytes(&self.ciphertext).expect(INFALLIBLE);
        bytes
    }

    /// Decode the canonical CBOR wire form, rejecting non-canonical encodings, the wrong domain,
    /// and wrong-width fields.
    pub(crate) fn from_cbor(bytes: &[u8]) -> anyhow::Result<Self> {
        Self::from_cbor_inner(bytes)
            .map_err(|error| anyhow::anyhow!("sealed key-wrap decode failed: {error}"))
    }

    fn from_cbor_inner(bytes: &[u8]) -> Result<Self, CborError> {
        cbor::require_canonical_cbor(bytes)?;
        let mut decoder = Decoder::new(bytes);
        cbor::expect_array(&mut decoder, 3)?;
        cbor::expect_domain(&mut decoder, KEY_WRAP_DOMAIN)?;
        let ephemeral_pubkey = cbor::fixed_bytes::<32>(decoder.bytes()?, "ephemeral_pubkey")?;
        let ciphertext = cbor::fixed_bytes::<48>(decoder.bytes()?, "ciphertext")?;
        Ok(Self { ephemeral_pubkey, ciphertext })
    }
}

/// Seal `key` to `recipient`, binding `ctx`. Samples a fresh ephemeral X25519 key from OS entropy;
/// see [`seal_with_ephemeral`] for the deterministic (seed-injected) variant the golden tests use.
pub(crate) fn seal_content_key(
    key: &ContentKey,
    ctx: &WrapContext,
    recipient: &DeviceX25519Public,
) -> anyhow::Result<SealedKeyWrap> {
    let ephemeral = DeviceX25519Secret::generate()?;
    seal_with_ephemeral(key, ctx, recipient, &ephemeral)
}

/// The seal core, with the ephemeral key injected so golden vectors are byte-reproducible. The
/// ephemeral secret is dropped (and zeroized) when this returns.
fn seal_with_ephemeral(
    key: &ContentKey,
    ctx: &WrapContext,
    recipient: &DeviceX25519Public,
    ephemeral: &DeviceX25519Secret,
) -> anyhow::Result<SealedKeyWrap> {
    // The KDF binds the CANONICAL recipient encoding (bit 255 masked). X25519 ignores bit 255, so
    // `DeviceX25519Public::from_bytes` accepts and preserves a non-canonical key; sealing with the
    // raw bytes while `unwrap` re-derives the recipient's canonical `public()` would produce two
    // different wrap keys, so a legitimately-addressed recipient could not decrypt. Masking here
    // makes seal/unwrap agree for any accepted encoding of the same point (Codex P2).
    let recipient_pub = canonical_x25519(&recipient.to_bytes());
    // The WrapContext AAD claims a recipient; it must name the device this wrap is actually sealed
    // to, or authoring code could emit an authenticated-but-inconsistent wrap whose stated
    // recipient differs from the key it encrypts to (Codex P2). Compare canonical encodings.
    if canonical_x25519(&ctx.recipient_pub) != recipient_pub {
        anyhow::bail!("WrapContext.recipient_pub does not name the sealing recipient");
    }
    let epk = ephemeral.public().to_bytes();
    let shared = ephemeral.diffie_hellman(recipient);
    if !shared.was_contributory() {
        anyhow::bail!("X25519 key-wrap produced a non-contributory shared secret");
    }
    let (wrap_key, nonce) = derive_wrap_key_and_nonce(shared.as_bytes(), &epk, &recipient_pub)?;
    let cipher = XChaCha20Poly1305::new_from_slice(wrap_key.as_slice())
        .map_err(|_| anyhow::anyhow!("derived wrap key has the wrong length"))?;
    let ciphertext_vec = cipher
        .encrypt(&XNonce::from(nonce), Payload { msg: key.as_slice(), aad: &ctx.to_bytes() })
        .map_err(|_| anyhow::anyhow!("content-key AEAD sealing failed"))?;
    let ciphertext = cbor::fixed_bytes::<48>(&ciphertext_vec, "sealed content-key ciphertext")
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(SealedKeyWrap { ephemeral_pubkey: epk, ciphertext })
}

/// Open `sealed` with `secret`, requiring `ctx` to match the sealing context byte-for-byte.
pub(crate) fn unwrap_content_key(
    sealed: &SealedKeyWrap,
    secret: &DeviceX25519Secret,
    ctx: &WrapContext,
) -> anyhow::Result<ContentKey> {
    // Validate the ephemeral public key through the small-order/identity blocklist BEFORE any DH,
    // so a malicious epk that would force a world-computable (all-zero) shared secret is
    // refused outright — an attacker must not be able to forge a wrap under a key everyone can
    // derive (B2).
    let epk = DeviceX25519Public::from_bytes(&sealed.ephemeral_pubkey)
        .context("sealed key-wrap has an invalid ephemeral public key")?;
    let shared = secret.diffie_hellman(&epk);
    if !shared.was_contributory() {
        anyhow::bail!("X25519 key-unwrap produced a non-contributory shared secret");
    }
    // Canonical recipient encoding (matches the seal side), and enforce the AAD names THIS device —
    // an unwrap whose context claims a different recipient is refused rather than silently opened
    // (the seal-side symmetry of the Codex P2 consistency check).
    let recipient_pub = canonical_x25519(&secret.public().to_bytes());
    if canonical_x25519(&ctx.recipient_pub) != recipient_pub {
        anyhow::bail!("WrapContext.recipient_pub does not name the unwrapping device");
    }
    let (wrap_key, nonce) =
        derive_wrap_key_and_nonce(shared.as_bytes(), &sealed.ephemeral_pubkey, &recipient_pub)?;
    let cipher = XChaCha20Poly1305::new_from_slice(wrap_key.as_slice())
        .map_err(|_| anyhow::anyhow!("derived wrap key has the wrong length"))?;
    // Decrypt into a scrubbing buffer (not `decrypt`, which returns an unzeroized `Vec`); on tag
    // failure return an opaque error — no plaintext, no oracle.
    let mut buffer = Zeroizing::new(sealed.ciphertext.to_vec());
    cipher
        .decrypt_in_place(&XNonce::from(nonce), &ctx.to_bytes(), &mut *buffer)
        .map_err(|_| anyhow::anyhow!("content-key unwrap failed"))?;
    if buffer.len() != 32 {
        anyhow::bail!("unwrapped content key has an unexpected length");
    }
    let mut recovered = Zeroizing::new([0u8; 32]);
    recovered.copy_from_slice(&buffer);
    Ok(ContentKey(recovered))
}

/// Derive the per-wrap AEAD key (32 bytes, scrubbed) and nonce (24 bytes) from the shared secret,
/// binding `epk` and `recipient_pub` into each `info`. Both come from ONE `HKDF-Extract` PRK
/// (RFC 9180 `ExtractAndExpand`); the differing `info` labels keep the key and nonce independent.
fn derive_wrap_key_and_nonce(
    shared: &[u8],
    epk: &[u8; 32],
    recipient_pub: &[u8; 32],
) -> anyhow::Result<(Zeroizing<[u8; 32]>, [u8; 24])> {
    let hk = Hkdf::<Sha256>::new(Some(EMPTY_SALT), shared);
    let mut wrap_key = Zeroizing::new([0u8; 32]);
    hk.expand(&kdf_info(KEY_WRAP_KDF_INFO, epk, recipient_pub), wrap_key.as_mut_slice())
        .map_err(|_| anyhow::anyhow!("HKDF expand of the wrap key failed"))?;
    let mut nonce = [0u8; 24];
    hk.expand(&kdf_info(KEY_WRAP_NONCE_INFO, epk, recipient_pub), &mut nonce)
        .map_err(|_| anyhow::anyhow!("HKDF expand of the wrap nonce failed"))?;
    Ok((wrap_key, nonce))
}

/// The canonical X25519 public-key encoding: bit 255 (the high bit of the little-endian
/// u-coordinate) masked to 0. X25519 ignores that bit in the scalar multiplication, so two
/// encodings differing only there name the same point and must derive the same wrap key; masking
/// gives one representative. Applied to the RECIPIENT key (whose bytes seal and unwrap source
/// differently) — NOT to `epk`, whose raw bytes are bound so any wire tamper is caught by the tag.
fn canonical_x25519(pubkey: &[u8; 32]) -> [u8; 32] {
    let mut out = *pubkey;
    out[31] &= 0x7f;
    out
}

/// The HKDF `info`: `label || epk || recipient_pub` — the DHKEM `kem_context` under a
/// domain-separation `label`.
fn kdf_info(label: &[u8], epk: &[u8; 32], recipient_pub: &[u8; 32]) -> Vec<u8> {
    let mut info = Vec::with_capacity(label.len() + 64);
    info.extend_from_slice(label);
    info.extend_from_slice(epk);
    info.extend_from_slice(recipient_pub);
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTENT_KEY_SEED: [u8; 32] = [0x11; 32];
    const RECIPIENT_SEED: [u8; 32] = [0x22; 32];
    const EPHEMERAL_SEED: [u8; 32] = [0x33; 32];
    const WRONG_SEED: [u8; 32] = [0x44; 32];

    fn recipient_secret() -> DeviceX25519Secret {
        DeviceX25519Secret::from_seed(&RECIPIENT_SEED)
    }

    fn recipient_pub() -> DeviceX25519Public {
        DeviceX25519Public::from_bytes(&recipient_secret().public().to_bytes())
            .expect("a genuine device pubkey is not small-order")
    }

    /// The wrap context used across the seal/unwrap tests. `recipient_pub` names the real
    /// recipient, so `unwrap_content_key` (which re-derives it from the secret) agrees.
    fn ctx() -> WrapContext {
        WrapContext {
            account_id: [0xa0; 32],
            stream_id: [0xb0; 32],
            key_epoch: 7,
            recipient_pub: recipient_secret().public().to_bytes(),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn wrap_context_bytes_are_golden_pinned() {
        // Fully hand-predictable: a pure fixed-width concatenation, big-endian epoch. Freezes the
        // AAD encoding.
        let context = WrapContext {
            account_id: [0x01; 32],
            stream_id: [0x02; 32],
            key_epoch: 0x0102_0304_0506_0708,
            recipient_pub: [0x04; 32],
        };
        let bytes = context.to_bytes();
        assert_eq!(bytes.len(), 104);
        assert_eq!(
            hex(&bytes),
            concat!(
                "0101010101010101010101010101010101010101010101010101010101010101",
                "0202020202020202020202020202020202020202020202020202020202020202",
                "0102030405060708",
                "0404040404040404040404040404040404040404040404040404040404040404",
            ),
        );
    }

    #[test]
    fn key_id_is_golden_deterministic_and_one_way() {
        let key = ContentKey::from_seed(&CONTENT_KEY_SEED);
        let id = key.key_id();
        assert_eq!(
            hex(&id.to_bytes()),
            "8b27ce8ea3cd54963287d8a8405bded4a6b9ffe3c8b9f132c0b2a8c4d234397f",
        );
        // Same key → same id; a different key → a different id.
        assert_eq!(id, ContentKey::from_seed(&CONTENT_KEY_SEED).key_id());
        assert_ne!(id, ContentKey::from_seed(&[0x12; 32]).key_id());
        // key_id is a one-way function OF the key — not the key bytes themselves.
        assert_ne!(id.to_bytes(), CONTENT_KEY_SEED);
        assert_ne!(id.to_bytes().as_slice(), key.as_slice());
        // `from_bytes` round-trips the header representation.
        assert_eq!(KeyId::from_bytes(id.to_bytes()), id);
    }

    #[test]
    fn sealed_key_wrap_wire_is_golden_pinned() {
        // Seed-fixed content key AND seed-fixed ephemeral → a byte-reproducible wrap. Freezes epk,
        // the 48-byte ciphertext, and the CBOR wire.
        let key = ContentKey::from_seed(&CONTENT_KEY_SEED);
        let ephemeral = DeviceX25519Secret::from_seed(&EPHEMERAL_SEED);
        let sealed = seal_with_ephemeral(&key, &ctx(), &recipient_pub(), &ephemeral).unwrap();
        assert_eq!(
            hex(&sealed.ephemeral_pubkey),
            "7b0d47d93427f8311160781c7c733fd89f88970aef490d8aa0ee19a4cb8a1b14",
        );
        assert_eq!(
            hex(&sealed.ciphertext),
            "e7675b95bab219a91fc65f3f2d9475216f192a9c2dd745b26053af71845e6d9a9829ff0dbd01f0760bf0f0c62d9d856a",
        );
        assert_eq!(
            hex(&sealed.to_cbor()),
            concat!(
                "83",                                   // array(3)
                "72",                                   // text(18)
                "7261672d7261742f6b65792d777261702f31", // "rag-rat/key-wrap/1"
                "5820",                                 // bytes(32)
                "7b0d47d93427f8311160781c7c733fd89f88970aef490d8aa0ee19a4cb8a1b14", // epk
                "5830",                                 // bytes(48)
                "e7675b95bab219a91fc65f3f2d9475216f192a9c2dd745b26053af71845e6d9a9829ff0dbd01f0760bf0f0c62d9d856a", // ct
            ),
        );
        // Deterministic across calls.
        assert_eq!(
            sealed,
            seal_with_ephemeral(&key, &ctx(), &recipient_pub(), &ephemeral).unwrap()
        );
    }

    #[test]
    fn seal_then_unwrap_recovers_the_key() {
        let key = ContentKey::from_seed(&CONTENT_KEY_SEED);
        let sealed = seal_content_key(&key, &ctx(), &recipient_pub()).unwrap();
        let recovered = unwrap_content_key(&sealed, &recipient_secret(), &ctx()).unwrap();
        assert_eq!(recovered.as_slice(), key.as_slice());
        // Round-tripping through the CBOR wire opens identically.
        let via_wire = SealedKeyWrap::from_cbor(&sealed.to_cbor()).unwrap();
        assert_eq!(via_wire, sealed);
        let recovered = unwrap_content_key(&via_wire, &recipient_secret(), &ctx()).unwrap();
        assert_eq!(recovered.as_slice(), key.as_slice());
    }

    #[test]
    fn unwrap_with_the_wrong_recipient_fails() {
        let key = ContentKey::from_seed(&CONTENT_KEY_SEED);
        let sealed = seal_content_key(&key, &ctx(), &recipient_pub()).unwrap();
        // A different device's secret: the DH diverges → the wrap key diverges → the tag fails.
        let wrong = DeviceX25519Secret::from_seed(&WRONG_SEED);
        assert!(unwrap_content_key(&sealed, &wrong, &ctx()).is_err());
    }

    #[test]
    fn unwrap_rejects_a_transplanted_wrap_context() {
        let key = ContentKey::from_seed(&CONTENT_KEY_SEED);
        let sealed = seal_content_key(&key, &ctx(), &recipient_pub()).unwrap();
        // Change EACH field independently: every one is bound in the AAD, so every one breaks the
        // tag.
        let transplants = [
            WrapContext { account_id: [0xff; 32], ..ctx() },
            WrapContext { stream_id: [0xff; 32], ..ctx() },
            WrapContext { key_epoch: 8, ..ctx() },
            WrapContext { recipient_pub: [0x22; 32], ..ctx() },
        ];
        for transplanted in transplants {
            assert!(
                unwrap_content_key(&sealed, &recipient_secret(), &transplanted).is_err(),
                "a transplanted wrap context must not open the wrap",
            );
        }
    }

    #[test]
    fn unwrap_rejects_tampered_bytes() {
        let key = ContentKey::from_seed(&CONTENT_KEY_SEED);
        let sealed = seal_content_key(&key, &ctx(), &recipient_pub()).unwrap();

        let mut bad_epk = sealed.clone();
        bad_epk.ephemeral_pubkey[0] ^= 1;
        assert!(unwrap_content_key(&bad_epk, &recipient_secret(), &ctx()).is_err());

        let mut bad_ct = sealed.clone();
        bad_ct.ciphertext[0] ^= 1;
        assert!(unwrap_content_key(&bad_ct, &recipient_secret(), &ctx()).is_err());

        let mut bad_tag = sealed.clone();
        let last = bad_tag.ciphertext.len() - 1;
        bad_tag.ciphertext[last] ^= 1;
        assert!(unwrap_content_key(&bad_tag, &recipient_secret(), &ctx()).is_err());
    }

    #[test]
    fn unwrap_rejects_a_small_order_ephemeral_pubkey() {
        // An epk of the identity point (u = 0) forces an all-zero shared secret. unwrap must refuse
        // it at the blocklist validation step, BEFORE any DH / AEAD work — not as a tag error.
        let sealed = SealedKeyWrap { ephemeral_pubkey: [0u8; 32], ciphertext: [0u8; 48] };
        // `ContentKey` is deliberately not `Debug` (secret hygiene), so match rather than
        // `unwrap_err` (which would require the Ok type to be `Debug`).
        let err = match unwrap_content_key(&sealed, &recipient_secret(), &ctx()) {
            Ok(_) => panic!("a small-order ephemeral pubkey must be rejected"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("ephemeral public key"),
            "expected an epk-validation rejection, got: {err}",
        );
    }

    #[test]
    fn sealed_key_wrap_cbor_is_canonical_and_strict() {
        let sealed = SealedKeyWrap { ephemeral_pubkey: [0x33; 32], ciphertext: [0x55; 48] };
        let wire = sealed.to_cbor();
        assert!(cbor::require_canonical_cbor(&wire).is_ok());
        assert_eq!(SealedKeyWrap::from_cbor(&wire).unwrap(), sealed);

        // A trailing byte breaks canonicity.
        let mut trailing = wire.clone();
        trailing.push(0);
        assert!(SealedKeyWrap::from_cbor(&trailing).is_err());

        // A wrong domain tag (flip the last domain byte) is refused.
        let mut wrong_domain = wire.clone();
        let domain_end = 1 + 1 + KEY_WRAP_DOMAIN.len();
        wrong_domain[domain_end - 1] ^= 1;
        assert!(SealedKeyWrap::from_cbor(&wrong_domain).is_err());

        // Truncated input is refused.
        assert!(SealedKeyWrap::from_cbor(&wire[..wire.len() - 1]).is_err());
    }

    #[test]
    fn generate_and_from_seed_agree_on_shape_and_differ_in_material() {
        let generated = ContentKey::generate().expect("OS CSPRNG available");
        assert_eq!(generated.as_slice().len(), 32);
        // Two generations must not collide (a collision would mean broken entropy).
        assert_ne!(
            ContentKey::generate().unwrap().as_slice(),
            ContentKey::generate().unwrap().as_slice(),
        );
        // from_seed is the deterministic counterpart.
        assert_eq!(
            ContentKey::from_seed(&CONTENT_KEY_SEED).as_slice(),
            ContentKey::from_seed(&CONTENT_KEY_SEED).as_slice(),
        );
    }

    #[test]
    fn seal_and_unwrap_agree_for_a_noncanonical_recipient_encoding() {
        // X25519 ignores bit 255; `from_bytes` preserves it. A recipient key encoded with bit 255
        // set names the same point as the device's canonical `public()`, so a wrap sealed to it
        // must still open. The KDF binds the canonical form on both sides — without that, seal and
        // unwrap derive different wrap keys and this fails with a tag error (Codex P2).
        let key = ContentKey::from_seed(&CONTENT_KEY_SEED);
        let secret = recipient_secret();
        let mut noncanon = secret.public().to_bytes();
        noncanon[31] |= 0x80; // set bit 255 — same point, non-canonical encoding
        let recipient = DeviceX25519Public::from_bytes(&noncanon).expect("still a valid point");
        let context = WrapContext {
            account_id: [0xa0; 32],
            stream_id: [0xb0; 32],
            key_epoch: 7,
            recipient_pub: noncanon,
        };
        let sealed = seal_content_key(&key, &context, &recipient).unwrap();
        let recovered = unwrap_content_key(&sealed, &secret, &context).unwrap();
        assert_eq!(recovered.as_slice(), key.as_slice());
    }

    #[test]
    fn seal_rejects_a_context_naming_a_different_recipient() {
        // The WrapContext AAD claims a recipient; sealing to a device other than the one it names
        // is refused, so authoring code cannot emit an authenticated wrap whose stated recipient
        // differs from the key it is encrypted to (Codex P2).
        let key = ContentKey::from_seed(&CONTENT_KEY_SEED);
        let wrong = DeviceX25519Secret::from_seed(&WRONG_SEED).public().to_bytes();
        let bad_ctx = WrapContext { recipient_pub: wrong, ..ctx() };
        assert!(seal_content_key(&key, &bad_ctx, &recipient_pub()).is_err());
    }
}
