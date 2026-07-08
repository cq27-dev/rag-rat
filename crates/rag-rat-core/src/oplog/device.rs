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

#[cfg(test)]
mod tests {
    use super::*;

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
}
