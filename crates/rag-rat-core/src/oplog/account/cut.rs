//! The revocation `Cut` — the content-addressed watermark that bounds a chain's valid prefix (§11).
//!
//! `Cut = null | [seq, entry_hash]`. It is the L2 half of the two soundness lemmas (§2): an
//! owner-attested cut on a subject's own hash chain content-addressably fixes the valid prefix —
//! slots `seq ≤ s` are determined by the ancestry of `hash`, slots `seq > s` are condemned
//! regardless of any asserted ordering. Because the cut names a hash, "pretending older" is useless
//! (the ancestry is fixed) and "pretending newer" is impossible (it would need a hash collision).
//!
//! This module owns the WIRE + the seq-only [`beyond`] test (evaluable from `[seq]` alone — I11).
//! The ancestry walk, cut-target binding, and the `⊔` join operate over the candidate DAG and live
//! with the fold ([`super::fold`]), which holds those structures.

use minicbor::Encoder;
use minicbor::data::Type;
use minicbor::decode::{Decoder, Error as CborError};

use crate::oplog::cbor;

/// Writing CBOR into a `Vec` cannot fail (its `Write` impl is infallible) — mirrors `super::super`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// A cut (§11): `Empty` (CBOR `null`) means nothing on the chain is valid; `At { seq, hash }` pins
/// the valid prefix to slots `≤ seq` on the branch ending at `hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cut {
    Empty,
    At { seq: u64, hash: [u8; 32] },
}

impl Cut {
    /// Write the cut into an existing encoder (op payloads carry cuts inline): `null` for `Empty`,
    /// else the 2-array `[seq, hash]` with minimal integer + a 32-byte bstr.
    pub(super) fn encode_into(&self, enc: &mut Encoder<&mut Vec<u8>>) {
        match self {
            Cut::Empty => {
                enc.null().expect(INFALLIBLE);
            },
            Cut::At { seq, hash } => {
                enc.array(2).expect(INFALLIBLE);
                enc.u64(*seq).expect(INFALLIBLE);
                enc.bytes(hash).expect(INFALLIBLE);
            },
        }
    }

    /// Encode a standalone cut to canonical CBOR (used by goldens + tests).
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(40);
        {
            let mut enc = Encoder::new(&mut buf);
            self.encode_into(&mut enc);
        }
        buf
    }

    /// Decode a cut from a decoder positioned at the cut item: CBOR `null` → `Empty`, else `[seq,
    /// hash]` (a definite-length 2-array, minimal seq, 32-byte hash).
    pub(super) fn decode(d: &mut Decoder<'_>) -> Result<Cut, CborError> {
        if d.datatype()? == Type::Null {
            d.null()?;
            Ok(Cut::Empty)
        } else {
            cbor::expect_array(d, 2)?;
            let seq = d.u64()?;
            let hash = cbor::fixed_bytes::<32>(d.bytes()?, "cut hash")?;
            Ok(Cut::At { seq, hash })
        }
    }

    /// The watermark seq if this is an `At` cut.
    pub(super) fn seq(&self) -> Option<u64> {
        match self {
            Cut::Empty => None,
            Cut::At { seq, .. } => Some(*seq),
        }
    }

    /// The watermark entry_hash if this is an `At` cut.
    pub(super) fn hash(&self) -> Option<[u8; 32]> {
        match self {
            Cut::Empty => None,
            Cut::At { hash, .. } => Some(*hash),
        }
    }
}

/// Whether entry `entry_seq` is BEYOND `cut` — the seq-only condemnation test (I11), evaluable from
/// `[seq]` alone and NEVER needing the watermark entry. `Empty` ⇒ every slot is beyond (nothing
/// valid). This is what makes a back-dated forgery past the cut condemnable even when the watermark
/// entry itself has not been synced yet.
pub(super) fn beyond(entry_seq: u64, cut: &Cut) -> bool {
    match cut {
        Cut::Empty => true,
        Cut::At { seq, .. } => entry_seq > *seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn cut_wire_is_frozen() {
        // `Empty` is a bare CBOR null; `At` is `[seq, hash]`. These bytes ride inside DeviceRemove
        // / OwnerDemote / CutExtend payloads, so a change is a wire bump.
        assert_eq!(hex(&Cut::Empty.encode()), "f6", "empty cut is null");
        let at = Cut::At { seq: 5, hash: [0x11; 32] };
        assert_eq!(
            hex(&at.encode()),
            "82055820".to_string() + &"11".repeat(32),
            "At cut is [seq, hash]",
        );
    }

    #[test]
    fn cut_round_trips_through_decode() {
        for cut in
            [Cut::Empty, Cut::At { seq: 0, hash: [0u8; 32] }, Cut::At { seq: 42, hash: [0xab; 32] }]
        {
            let bytes = cut.encode();
            let mut d = Decoder::new(&bytes);
            assert_eq!(Cut::decode(&mut d).unwrap(), cut);
        }
    }

    #[test]
    fn non_canonical_and_malformed_cuts_are_rejected() {
        // A wrong-length hash.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(2).unwrap();
            enc.u64(1).unwrap();
            enc.bytes(&[0u8; 16]).unwrap();
        }
        let mut d = Decoder::new(&buf);
        assert!(Cut::decode(&mut d).is_err(), "16-byte cut hash is rejected");
        // A 3-element array.
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.array(3).unwrap();
            enc.u64(1).unwrap();
            enc.bytes(&[0u8; 32]).unwrap();
            enc.u64(0).unwrap();
        }
        let mut d = Decoder::new(&buf);
        assert!(Cut::decode(&mut d).is_err(), "wrong arity is rejected");
    }

    #[test]
    fn beyond_is_seq_only() {
        // Empty condemns everything; At condemns strictly-greater seqs, from `[seq]` alone.
        assert!(beyond(0, &Cut::Empty));
        assert!(beyond(u64::MAX, &Cut::Empty));
        let cut = Cut::At { seq: 41, hash: [0x11; 32] };
        assert!(!beyond(41, &cut), "the watermark slot itself is within the cut");
        assert!(!beyond(0, &cut), "under-cut slots are within");
        assert!(beyond(42, &cut), "slot past the watermark is beyond");
    }
}
