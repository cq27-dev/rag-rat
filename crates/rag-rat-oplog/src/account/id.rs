//! The account identity — [`AccountId`] and its genesis-committing derivation (§4).
//!
//! `account_id = sha256(cbor(["rag-rat/account/1", genesis_payload_bytes]))` commits to the ENTIRE
//! founding state of the account (the `AccountGenesis` payload). Two devices claiming one account
//! is therefore cryptographically impossible, and any peer can verify the id offline from the
//! genesis entry alone. Store-global and immutable — the exact analog of a `StreamId` for a
//! principal.

use minicbor::Encoder;

use super::limits::ACCOUNT_ID_DOMAIN;
use crate::cbor::{self, VecEncoderExt};

/// A stored 32-byte (or other fixed-width) blob as an array — a wrong length is a corrupt row.
pub(in crate::account) fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("stored blob is {} bytes, not {N}", bytes.len()))
}

// Fixed-width representations preserve byte order at the persistence and wire boundaries.
macro_rules! byte_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 32]);
        // Preserve the raw-array diagnostic representation at existing error boundaries.
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(&self.0, f)
            }
        }
        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }
            pub fn to_bytes(self) -> [u8; 32] {
                self.0
            }
            pub fn as_slice(&self) -> &[u8] {
                &self.0
            }
        }
        impl From<[u8; 32]> for $name {
            fn from(bytes: [u8; 32]) -> Self {
                Self::from_bytes(bytes)
            }
        }
        impl From<$name> for [u8; 32] {
            fn from(id: $name) -> Self {
                id.to_bytes()
            }
        }
        impl TryFrom<&[u8]> for $name {
            type Error = std::array::TryFromSliceError;
            fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
                bytes.try_into().map(Self::from_bytes)
            }
        }
        impl TryFrom<Vec<u8>> for $name {
            type Error = Vec<u8>;
            fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
                bytes.try_into().map(Self::from_bytes)
            }
        }
    };
}
byte_id!(AccountEntryHash, "A candidate entry's body hash. Ordering is lexicographic byte order.");
byte_id!(SignedHash, "The hash of a complete signed envelope, distinct from its body address.");
byte_id!(OwnerId, "The control entry that mints one owner incarnation.");
byte_id!(GrantId, "The control entry that mints one stream grant.");
byte_id!(RosterRef, "The control entry that enrolls a device in the roster.");

macro_rules! entry_reference {
    ($name:ident) => {
        impl From<AccountEntryHash> for $name {
            fn from(hash: AccountEntryHash) -> Self {
                Self::from_bytes(hash.to_bytes())
            }
        }
        impl From<$name> for AccountEntryHash {
            fn from(reference: $name) -> Self {
                Self::from_bytes(reference.to_bytes())
            }
        }
        impl $name {
            pub fn entry_hash(self) -> AccountEntryHash {
                self.into()
            }
        }
    };
}
entry_reference!(OwnerId);
entry_reference!(GrantId);
entry_reference!(RosterRef);

/// An account's immutable, content-derived identity: `sha256` of the domain-tagged genesis
/// commitment. The store-global key for a principal's roster, grants, and folds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountId([u8; 32]);

impl AccountId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// By value — `AccountId` is `Copy` (clippy `wrong_self_convention` flags `to_*` on `&self`).
    pub fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    /// Decode the 64-hex form an operator copies between machines — the inverse of
    /// `hash::hex_lower(id.to_bytes())`, which is what `sync whoami` prints and what `sync grant` /
    /// `sync contribute` / `sync pull` accept.
    ///
    /// Lives on the type rather than at each call site: the format is this type's, and the error
    /// wording is the same wherever an operator pastes an id. The digit loop is deliberate rather
    /// than `hash::hex_decode` — a paste is hand-handled, so the failing POSITION is worth
    /// reporting, and the shared decoder returns only `None`.
    pub fn from_hex(value: &str) -> anyhow::Result<Self> {
        let value = value.trim();
        anyhow::ensure!(
            value.len() == 64,
            "an account id is 64 hex characters (got {}) — take it from `rag-rat sync whoami`",
            value.len()
        );
        let mut bytes = [0u8; 32];
        for (index, &[hi, lo]) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let high = rag_rat_base::hash::hex_nibble(hi).ok_or_else(|| {
                anyhow::anyhow!("account id has invalid hex at position {}", index * 2)
            })?;
            let low = rag_rat_base::hash::hex_nibble(lo).ok_or_else(|| {
                anyhow::anyhow!("account id has invalid hex at position {}", index * 2 + 1)
            })?;
            bytes[index] = high << 4 | low;
        }
        Ok(Self(bytes))
    }
}

/// Derive the `account_id` from the canonical-CBOR bytes of the account's `AccountGenesis` payload
/// (§4). The payload is carried as an opaque byte string — this layer commits to it verbatim, so a
/// change to ANY genesis byte yields a different account. The domain tag disambiguates the hash
/// from every other object on the wire.
///
/// Consumed by the fold / ingest genesis self-hash check (Phase 4/5).
pub(super) fn account_id_from_genesis_payload(genesis_payload_bytes: &[u8]) -> AccountId {
    let mut buf = Vec::with_capacity(64);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.put_array(2);
        enc.put_str(ACCOUNT_ID_DOMAIN);
        enc.put_bytes(genesis_payload_bytes);
    }
    AccountId(cbor::sha256(&buf))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reference_ids_preserve_byte_order_and_diagnostics() {
        // A last-byte difference must sort before a first-byte difference, not as a
        // little-endian integer. These types participate in consensus tie-breaks.
        let mut low = [0; 32];
        low[31] = 255;
        let mut high = [0; 32];
        high[0] = 1;
        macro_rules! check {
            ($id:ty) => {{
                let a = <$id>::from_bytes(low);
                let b = <$id>::from_bytes(high);
                assert!(a < b);
                assert_eq!(a.as_slice(), low.as_slice());
                assert_eq!(a.to_bytes(), low);
                assert_eq!(format!("{a:?}"), format!("{low:?}"));
                assert_eq!(format!("{a:#?}"), format!("{low:#?}"));
            }};
        }
        check!(AccountEntryHash);
        check!(OwnerId);
        check!(GrantId);
        check!(RosterRef);
        check!(SignedHash);
    }

    /// `from_hex` is the one decoder every operator-facing surface uses (`sync grant` /
    /// `contribute` / `pull`), so its rejections are the error text a person actually reads.
    #[test]
    fn from_hex_round_trips_and_names_the_failing_position() {
        let id = AccountId::from_bytes([0xAB; 32]);
        let hex = rag_rat_base::hash::hex_lower(&id.to_bytes());
        assert_eq!(AccountId::from_hex(&hex).unwrap(), id, "round trips");
        assert_eq!(
            AccountId::from_hex(&format!("  {hex}  ")).unwrap(),
            id,
            "a pasted id carries whitespace",
        );

        let short = AccountId::from_hex("abcd").unwrap_err().to_string();
        assert!(short.contains("64 hex characters"), "length is named: {short}");
        assert!(short.contains("whoami"), "and where to get one: {short}");

        // Position 5 (the 6th character) is the bad nibble.
        let mut bad = hex.clone();
        bad.replace_range(5..6, "z");
        let err = AccountId::from_hex(&bad).unwrap_err().to_string();
        assert!(err.contains("position 5"), "the failing position is reported: {err}");
    }

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::cbor;

    fn hex(bytes: &[u8]) -> String {
        rag_rat_base::hash::hex_lower(bytes)
    }

    /// A fixed opaque "genesis payload". The id derivation treats it as opaque bytes, so its
    /// internal structure is irrelevant here — the real `AccountGenesis` payload encoding is
    /// pinned in `ops.rs`. Distinctive bytes make the fixture visually recognizable in the
    /// wire.
    fn genesis_payload() -> Vec<u8> {
        vec![0x85, 0x01, 0x02, 0x03, 0x04]
    }

    #[test]
    fn account_id_pins_the_genesis_commitment() {
        // Frozen primitive: stream ids, grants, and folds all key on these 32 bytes, so a
        // canonical-rule change must break this test and force a deliberate `rag-rat/account/1`
        // domain bump.
        let id = account_id_from_genesis_payload(&genesis_payload());
        assert_eq!(
            hex(&id.to_bytes()),
            "8e305e528169a19412449905c460978472d61f38d12bf532898c88a40c961dcf",
            "account_id golden",
        );
    }

    #[test]
    fn account_id_is_sha256_of_the_domain_committed_payload() {
        // Independent recomputation of the frozen `[domain, payload-bstr]` preimage shape, so the
        // structure is pinned separately from the opaque golden hash.
        let payload = genesis_payload();
        let mut preimage = Vec::new();
        {
            let mut enc = Encoder::new(&mut preimage);
            enc.array(2).unwrap();
            enc.str("rag-rat/account/1").unwrap();
            enc.bytes(&payload).unwrap();
        }
        cbor::require_canonical_cbor(&preimage).expect("preimage is canonical CBOR");
        let mut expected = [0u8; 32];
        expected.copy_from_slice(&Sha256::digest(&preimage));
        assert_eq!(account_id_from_genesis_payload(&payload).to_bytes(), expected);
    }

    #[test]
    fn account_id_is_a_function_of_the_whole_genesis_payload() {
        let baseline = account_id_from_genesis_payload(&genesis_payload());
        // Flipping any byte of the payload yields a different account.
        for index in 0..genesis_payload().len() {
            let mut mutated = genesis_payload();
            mutated[index] ^= 0x01;
            assert_ne!(
                account_id_from_genesis_payload(&mutated),
                baseline,
                "flipping payload byte {index} must change the account_id",
            );
        }
    }
}
