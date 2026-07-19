//! The op-log's ed25519 device identity (phase B op-log, §C4 layer-1).
//!
//! A device is an ed25519 keypair. [`DeviceSecret`] signs entry bodies; [`DevicePublic`] verifies
//! them and derives the op model's opaque [`DeviceFingerprint`] = `sha256(pubkey_bytes)` — the
//! bytes that bind a signed entry to a key without embedding the full 32-byte pubkey in every
//! entry.
//!
//! Keys are SEED-DETERMINISTIC on purpose: [`DeviceSecret::from_seed`] derives the key from a
//! caller-supplied 32-byte seed via `SigningKey::from_bytes` (no CSPRNG), so golden vectors and
//! chain tests are byte-reproducible. Production CSPRNG keygen is a later concern. The seed is
//! secret material: the local copy is scrubbed via `Zeroizing`, and `SigningKey` zeroizes its own
//! seed on drop (ed25519-dalek's `zeroize` feature).
//!
//! Verification is [`ed25519_dalek::VerifyingKey::verify_strict`], which REJECTS malleable and
//! small-order signatures — the strict ed25519 check the protocol requires (a plain `verify` would
//! accept signatures a peer could re-encode).

use anyhow::Context;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519PublicKey, SharedSecret, StaticSecret};
use zeroize::Zeroizing;

use super::op::DeviceFingerprint;

/// A device's ed25519 secret key — the signing capability, built deterministically from a 32-byte
/// seed. Holds a `SigningKey`, which zeroizes its seed on drop. Intentionally not `Debug` /
/// `Clone`: secret material should not be trivially printed or duplicated.
pub(super) struct DeviceSecret(SigningKey);

impl DeviceSecret {
    /// Derive the device key from a 32-byte seed (the ed25519 secret-scalar seed). Deterministic:
    /// the same seed always yields the same key, so signatures and fingerprints are reproducible.
    pub(super) fn from_seed(seed: &[u8; 32]) -> Self {
        // Copy the seed into a scrubbing buffer so this stack copy can't linger after use;
        // `SigningKey` keeps (and later zeroizes on drop) its own internal copy.
        let seed = Zeroizing::new(*seed);
        Self(SigningKey::from_bytes(&seed))
    }

    /// Generate a FRESH random device key from OS entropy — the production keygen path. The 32-byte
    /// seed is filled by the system CSPRNG (`getrandom`) and flows through [`from_seed`], so
    /// seeding stays the one construction point (deterministic tests keep their fixed seeds).
    /// Fails only if the OS entropy source is unavailable.
    pub(super) fn generate() -> anyhow::Result<Self> {
        let mut seed = Zeroizing::new([0u8; 32]);
        // `getrandom::Error` only implements `std::error::Error` behind getrandom's `std` feature
        // (off under `--no-default-features`), so format it via `Display` rather than `.context`.
        getrandom::fill(seed.as_mut_slice())
            .map_err(|e| anyhow::anyhow!("OS CSPRNG failed to seed a device key: {e}"))?;
        Ok(Self::from_seed(&seed))
    }

    /// The 32-byte secret seed, in a scrubbing buffer. The persistence layer stores this so a
    /// reopened store re-derives the SAME key via [`from_seed`] — it is the only durable copy, so
    /// the accessor deliberately exposes the secret (see the plaintext-at-rest decision).
    pub(super) fn seed(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.0.to_bytes())
    }

    /// The matching public identity (for verification + fingerprint derivation).
    pub(super) fn public(&self) -> DevicePublic {
        DevicePublic(self.0.verifying_key())
    }

    /// Sign `msg` (the canonical entry body bytes), returning the raw 64-byte ed25519 signature.
    pub(super) fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.0.sign(msg).to_bytes()
    }
}

/// A device's ed25519 public key — the verification capability and the source of the opaque
/// [`DeviceFingerprint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DevicePublic(VerifyingKey);

impl DevicePublic {
    /// Parse a compressed 32-byte ed25519 public key, REJECTING an invalid curve point. Validation
    /// happens here at construction so a [`DevicePublic`] is always a well-formed verifying key.
    pub(super) fn from_bytes(bytes: &[u8; 32]) -> anyhow::Result<Self> {
        VerifyingKey::from_bytes(bytes).map(Self).context("invalid ed25519 public key point")
    }

    /// The 32-byte compressed public key.
    pub(super) fn to_bytes(self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// The opaque device fingerprint the op model carries: `sha256(pubkey_bytes)`. A 32-byte hash
    /// of the key, so a signed entry binds to a key without embedding the whole pubkey.
    pub(super) fn fingerprint(&self) -> DeviceFingerprint {
        let mut fp = [0u8; 32];
        fp.copy_from_slice(&Sha256::digest(self.to_bytes()));
        DeviceFingerprint::from_bytes(fp)
    }

    /// Verify `sig` over `msg` under this key with `verify_strict` (rejects malleable / small-order
    /// signatures). A `Signature` from 64 raw bytes is always constructible — validity is decided
    /// here, at verification.
    pub(super) fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> anyhow::Result<()> {
        let signature = Signature::from_bytes(sig);
        self.0.verify_strict(msg, &signature).context("ed25519 signature verification failed")
    }
}

/// A device's X25519 ENCRYPTION secret — minted BESIDE the ed25519 signing key (sync phase C, §5),
/// and, like it, rebuilt deterministically from persisted bytes. C1 only holds, persists, and
/// validates it; no ECDH happens until C4. Its seed is INDEPENDENT of the ed25519 seed — the
/// signing and encryption identities never share entropy. Intentionally not `Debug` / `Clone`.
pub(super) struct DeviceX25519Secret(StaticSecret);

impl DeviceX25519Secret {
    /// Rebuild the X25519 secret from its persisted 32-byte scalar. `StaticSecret::from` and
    /// `to_bytes` are inverses — x25519-dalek 3 stores the scalar VERBATIM (clamping happens at DH
    /// / public derivation, not construction) — so the round-trip through
    /// [`secret_bytes`](Self::secret_bytes) re-derives the SAME key. Deterministic (the backfill
    /// tests rely on it).
    pub(super) fn from_seed(seed: &[u8; 32]) -> Self {
        // Scrub the stack copy; `StaticSecret` zeroizes its own copy on drop (dalek `zeroize`).
        let seed = Zeroizing::new(*seed);
        Self(StaticSecret::from(*seed))
    }

    /// Mint a FRESH X25519 secret from OS entropy — the production keygen path, routed through
    /// [`from_seed`](Self::from_seed) so seeding stays the one construction point. Fails only if
    /// the OS CSPRNG is unavailable.
    pub(super) fn generate() -> anyhow::Result<Self> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(seed.as_mut_slice())
            .map_err(|e| anyhow::anyhow!("OS CSPRNG failed to seed an X25519 device key: {e}"))?;
        Ok(Self::from_seed(&seed))
    }

    /// The 32-byte secret scalar, scrubbed. The persistence layer stores this so a reopened store
    /// re-derives the same key — the only durable copy (plaintext-at-rest, D4).
    pub(super) fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.0.to_bytes())
    }

    /// The matching public encryption key.
    pub(super) fn public(&self) -> DeviceX25519Public {
        DeviceX25519Public(X25519PublicKey::from(&self.0).to_bytes())
    }

    /// The X25519 ECDH shared secret with `peer` — the C4 layer the C1 doc reserved ("ECDH + HKDF
    /// is C4"). Returns the RAW [`SharedSecret`]: the caller MUST reject a non-contributory
    /// (all-zero) result via [`SharedSecret::was_contributory`] before deriving any key material —
    /// the RFC 7748 §6.1 output check that backstops the small-order blocklist on
    /// [`DeviceX25519Public::from_bytes`]. The peer's `PublicKey` is reconstructed from its 32
    /// bytes (a `PublicKey` here is only ever built from bytes a caller already validated).
    pub(super) fn diffie_hellman(&self, peer: &DeviceX25519Public) -> SharedSecret {
        self.0.diffie_hellman(&X25519PublicKey::from(peer.to_bytes()))
    }
}

/// A device's X25519 ENCRYPTION public key — a Montgomery-u coordinate. Every 32-byte string
/// decodes to *some* u-coordinate, so unlike an ed25519 key there is no curve-point rejection;
/// [`from_bytes`](Self::from_bytes) is where the identity + small-order points are refused instead
/// (the C1 half of §5 — the all-zero-shared-secret check is C4).
///
/// The type does NOT itself guarantee "blocklist-validated": [`DeviceX25519Secret::public`]
/// constructs one directly (a key derived from a real secret is never small-order). The guarantee
/// only matters for PEER keys — C4's `DeviceAdd` handling MUST route an incoming public key through
/// [`from_bytes`](Self::from_bytes) to hit the blocklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeviceX25519Public([u8; 32]);

impl DeviceX25519Public {
    /// Parse a 32-byte X25519 public key, rejecting the identity and the known small-order points
    /// (a key that would force an all-zero shared secret). X25519 ignores bit 255 of the
    /// u-coordinate, so the comparison masks it — a `| 0x80` variant of a blocklisted point is
    /// the same point and must not slip past.
    pub(super) fn from_bytes(bytes: &[u8; 32]) -> anyhow::Result<Self> {
        if is_small_order(bytes) {
            anyhow::bail!("X25519 public key is a small-order / identity point");
        }
        Ok(Self(*bytes))
    }

    /// The 32-byte public key.
    pub(super) fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// `p + delta` (`p = 2^255 - 19`) in little-endian 32-byte form, with bit 255 cleared. `delta ∈
/// {0,1,2}` builds `p-1`, `p`, `p+1` (their byte-0 is `0xec`/`0xed`/`0xee`; the rest is the `0xff…`
/// run ending `0x7f`). A `const fn` so the `0xff` run can't be mis-transcribed by hand.
const fn p_offset(byte0: u8) -> [u8; 32] {
    let mut v = [0xffu8; 32];
    v[0] = byte0;
    v[31] = 0x7f;
    v
}

/// The `u = 1` low-order X25519 point in little-endian 32-byte form.
const fn low_order_one() -> [u8; 32] {
    let mut v = [0u8; 32];
    v[0] = 1;
    v
}

/// The canonical X25519 small-order point blocklist (RFC 7748 §6.1 / the libsodium
/// `crypto_scalarmult_curve25519` blacklist). Every entry is a low-order point: DH with ANY scalar
/// yields the all-zero shared secret, so accepting one as a peer's public key would silently
/// produce a predictable key. A public key equal (mod the ignored high bit) to any of these is
/// refused at `DeviceAdd`. Each is annotated by its u-coordinate, not its torsion order — order
/// labels vary between references, whereas the verified property is the all-zero DH output.
const X25519_SMALL_ORDER_POINTS: [[u8; 32]; 7] = [
    // u = 0.
    [0u8; 32],
    // u = 1.
    low_order_one(),
    // u = 325606250916557431795983626356110631294008115727848805560023387167927233504.
    [
        0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4,
        0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49,
        0xb8, 0x00,
    ],
    // u = 39382357235489614581723060781553021112529911719440698176882885853963445705823.
    [
        0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83, 0xef,
        0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd, 0xd0, 0x9f,
        0x11, 0x57,
    ],
    // u = p - 1  (p = 2^255 - 19).
    p_offset(0xec),
    // u = p      (≡ u = 0 mod p).
    p_offset(0xed),
    // u = p + 1  (≡ u = 1 mod p).
    p_offset(0xee),
];

/// Whether `bytes` encodes a small-order / identity X25519 point. Masks bit 255 (which X25519
/// clears) before comparing, so a high-bit-set variant of a blocklisted point is still caught. A
/// plain (non-constant-time) compare is fine: the input is a public key, not secret material.
fn is_small_order(bytes: &[u8; 32]) -> bool {
    let mut candidate = *bytes;
    candidate[31] &= 0x7f;
    X25519_SMALL_ORDER_POINTS.iter().any(|bad| {
        let mut bad = *bad;
        bad[31] &= 0x7f;
        candidate == bad
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_rejects_small_order_signature() {
        // The module doc pins the CHOICE of `verify_strict` precisely BECAUSE it rejects
        // malleable / small-order signatures a plain `verify` would accept. Guard that choice with
        // a genuine small-order vector: the pubkey A and the signature's R are both the curve
        // IDENTITY point (compressed `01 00..00`), with S = 0.
        //
        // Why this vector isolates the strict check:
        //   - `DevicePublic::from_bytes` ACCEPTS A: `from_bytes` only rejects encodings that fail
        //     to decompress, and the identity decompresses (y = 1, x = 0). Small-order points are
        //     deliberately NOT refused there — that is verification's job.
        //   - a plain (cofactorless) `verify` ACCEPTS this signature: with A = identity, `[k]A` is
        //     the identity for any k, so the group equation `[S]B == R + [k]A` reduces to `[0]B ==
        //     R`, i.e. `identity == identity`. (Asserted below, so the vector genuinely DISAGREES
        //     with the strict path — a regression to non-strict `verify` would start ACCEPTING it
        //     and fail this test.)
        //   - `verify_strict` REJECTS it: `R.is_small_order() || A.is_small_order()` fires.
        //
        // This is NOT the S+L malleability transform: S here is the canonical scalar 0, not a
        // non-canonical `s + L`. A non-canonical S is rejected by plain `verify` too, so it would
        // pass this assertion even after a regression to non-strict verify and would NOT guard the
        // documented choice. A small-order component is the only property that separates the two.
        let identity = {
            let mut a = [0u8; 32];
            a[0] = 1; // compressed Edwards identity: y = 1, sign bit 0
            a
        };
        let sig_bytes = {
            let mut s = [0u8; 64];
            s[0] = 1; // R = compressed identity; the S half stays all-zero (the scalar 0)
            s
        };
        let msg = b"small-order rejection vector";

        // Premise: the plain, non-strict cofactorless verify ACCEPTS this signature — the
        // disagreement that makes the strict assertion below load-bearing.
        {
            use ed25519_dalek::Verifier;
            let vk = VerifyingKey::from_bytes(&identity).expect("identity decompresses");
            let sig = Signature::from_bytes(&sig_bytes);
            assert!(
                vk.verify(msg, &sig).is_ok(),
                "premise: plain verify accepts the small-order vector (so only strict rejects it)",
            );
        }

        // `from_bytes` accepts the small-order/identity pubkey (it is a well-formed curve point)…
        let public = DevicePublic::from_bytes(&identity).expect("identity is a valid curve point");
        // …but the module's `verify` (verify_strict) rejects the small-order signature.
        assert!(
            public.verify(msg, &sig_bytes).is_err(),
            "verify_strict must reject a small-order R / identity-key signature",
        );
    }

    /// Two distinct fixed seeds → two distinct devices; deterministic across calls.
    fn seed_a() -> [u8; 32] {
        [7u8; 32]
    }

    fn seed_b() -> [u8; 32] {
        [9u8; 32]
    }

    #[test]
    fn from_seed_is_deterministic() {
        // The same seed must always yield the same key material (pubkey + signature) — the property
        // the golden vectors and chain reproducibility rely on.
        let a1 = DeviceSecret::from_seed(&seed_a());
        let a2 = DeviceSecret::from_seed(&seed_a());
        assert_eq!(a1.public().to_bytes(), a2.public().to_bytes());
        let msg = b"the same message";
        assert_eq!(a1.sign(msg), a2.sign(msg), "ed25519 signing is deterministic per seed");
        // A different seed is a different device.
        let b = DeviceSecret::from_seed(&seed_b());
        assert_ne!(a1.public().to_bytes(), b.public().to_bytes());
    }

    #[test]
    fn generate_is_nondeterministic() {
        // Two independent generations must not collide — the CSPRNG, not a fixed seed, is the
        // source. (A collision here would mean the entropy source is broken.)
        let a = DeviceSecret::generate().expect("OS CSPRNG available");
        let b = DeviceSecret::generate().expect("OS CSPRNG available");
        assert_ne!(a.public().to_bytes(), b.public().to_bytes(), "generate must not repeat a key");
    }

    #[test]
    fn generated_key_seed_round_trips() {
        // The persisted seed must reconstruct the IDENTICAL key: `generate` → `seed` → `from_seed`
        // yields the same pubkey, fingerprint, and signatures. This is what makes a reopened store
        // keep one stable identity.
        let generated = DeviceSecret::generate().expect("OS CSPRNG available");
        let reloaded = DeviceSecret::from_seed(&generated.seed());
        assert_eq!(generated.public().to_bytes(), reloaded.public().to_bytes());
        assert_eq!(
            generated.public().fingerprint().to_bytes(),
            reloaded.public().fingerprint().to_bytes()
        );
        let msg = b"canonical entry body bytes";
        assert_eq!(generated.sign(msg), reloaded.sign(msg), "the reloaded key signs identically");
    }

    #[test]
    fn fingerprint_is_sha256_of_the_pubkey() {
        let secret = DeviceSecret::from_seed(&seed_a());
        let public = secret.public();
        let mut expected = [0u8; 32];
        expected.copy_from_slice(&Sha256::digest(public.to_bytes()));
        assert_eq!(public.fingerprint().to_bytes(), expected);
    }

    #[test]
    fn public_bytes_round_trip() {
        let public = DeviceSecret::from_seed(&seed_a()).public();
        let parsed = DevicePublic::from_bytes(&public.to_bytes()).expect("valid pubkey re-parses");
        assert_eq!(parsed, public);
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let secret = DeviceSecret::from_seed(&seed_a());
        let public = secret.public();
        let msg = b"canonical entry body bytes";
        let sig = secret.sign(msg);
        assert!(public.verify(msg, &sig).is_ok());
        // A different message under the same signature must fail.
        assert!(public.verify(b"tampered body bytes", &sig).is_err());
    }

    #[test]
    fn verify_rejects_the_wrong_key() {
        let secret = DeviceSecret::from_seed(&seed_a());
        let wrong = DeviceSecret::from_seed(&seed_b()).public();
        let msg = b"body";
        let sig = secret.sign(msg);
        assert!(wrong.verify(msg, &sig).is_err(), "the wrong key must not verify a signature");
    }

    #[test]
    fn from_bytes_rejects_encodings_that_are_not_curve_points() {
        // ~half of all 32-byte strings do not decompress to a valid Edwards point; `from_bytes`
        // must reject those so a `DevicePublic` is always a well-formed verifying key. (An all-zero
        // encoding is NOT a good negative case — it's a valid small-order point `from_bytes`
        // accepts; only `verify_strict` refuses small-order.) Scan deterministic encodings and
        // assert at least one is refused, and that a genuine pubkey is accepted.
        let refused = (2u8..32)
            .filter(|&lsb| {
                let mut candidate = [0u8; 32];
                candidate[0] = lsb;
                DevicePublic::from_bytes(&candidate).is_err()
            })
            .count();
        assert!(refused > 0, "from_bytes must reject non-curve encodings");
        let real = DeviceSecret::from_seed(&seed_a()).public().to_bytes();
        assert!(DevicePublic::from_bytes(&real).is_ok(), "a genuine pubkey must parse");
    }

    #[test]
    fn x25519_from_seed_is_deterministic() {
        // The same seed yields the same X25519 key; a different seed differs — the property the
        // reopen/backfill path relies on.
        let a1 = DeviceX25519Secret::from_seed(&seed_a());
        let a2 = DeviceX25519Secret::from_seed(&seed_a());
        assert_eq!(a1.public().to_bytes(), a2.public().to_bytes());
        let b = DeviceX25519Secret::from_seed(&seed_b());
        assert_ne!(
            a1.public().to_bytes(),
            b.public().to_bytes(),
            "a different seed is a different key"
        );
        // The secret round-trips through its persisted bytes (the reopen path).
        let reloaded = DeviceX25519Secret::from_seed(&a1.secret_bytes());
        assert_eq!(
            a1.public().to_bytes(),
            reloaded.public().to_bytes(),
            "secret_bytes reconstructs the key"
        );
    }

    #[test]
    fn x25519_generate_is_nondeterministic() {
        // Two independent generations must not collide — the CSPRNG, not a fixed seed, is the
        // source.
        let a = DeviceX25519Secret::generate().expect("OS CSPRNG available");
        let b = DeviceX25519Secret::generate().expect("OS CSPRNG available");
        assert_ne!(a.public().to_bytes(), b.public().to_bytes(), "generate must not repeat a key");
    }

    #[test]
    fn x25519_diffie_hellman_agrees_and_is_contributory() {
        // Both parties derive the SAME shared secret (esk·B == bsk·A), and DH between two genuine
        // keys is always contributory — the invariant `seal`/`unwrap` rely on before HKDF.
        let a = DeviceX25519Secret::from_seed(&seed_a());
        let b = DeviceX25519Secret::from_seed(&seed_b());
        let a_pub = DeviceX25519Public::from_bytes(&a.public().to_bytes()).unwrap();
        let b_pub = DeviceX25519Public::from_bytes(&b.public().to_bytes()).unwrap();
        let ab = a.diffie_hellman(&b_pub);
        let ba = b.diffie_hellman(&a_pub);
        assert_eq!(ab.as_bytes(), ba.as_bytes(), "X25519 DH must be symmetric");
        assert!(ab.was_contributory(), "DH between genuine keys is contributory");
        assert!(ba.was_contributory());
    }

    #[test]
    fn x25519_from_bytes_rejects_small_order_and_identity_points() {
        // Every canonical small-order encoding (identity + order 2/4/8 + p-1/p/p+1) must be
        // refused, INCLUDING the high-bit-set variant X25519 treats as the same point —
        // else a peer could add a device whose encryption key forces an all-zero shared
        // secret.
        for bad in X25519_SMALL_ORDER_POINTS {
            assert!(
                DeviceX25519Public::from_bytes(&bad).is_err(),
                "small-order point accepted: {bad:02x?}",
            );
            let mut high_bit = bad;
            high_bit[31] |= 0x80;
            assert!(
                DeviceX25519Public::from_bytes(&high_bit).is_err(),
                "high-bit variant of a small-order point accepted: {high_bit:02x?}",
            );
        }
        // A genuine device public key parses.
        let real = DeviceX25519Secret::from_seed(&seed_a()).public().to_bytes();
        assert!(
            DeviceX25519Public::from_bytes(&real).is_ok(),
            "a genuine X25519 pubkey must parse"
        );
    }
}
