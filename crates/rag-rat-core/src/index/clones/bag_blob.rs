//! Serialized token-bag BLOB codec (#231). A symbol's `(token_hash, freq)` multiset is packed into
//! ONE `symbol_fingerprints.token_bag` BLOB instead of N `symbol_token_postings` rows — the
//! dominant clone-indexing write cost (~490k single-row INSERTs per full rebuild). Recall stays
//! byte-identical because the decoded bag is the same `(token_hash, freq)` multiset, sorted by
//! `token_hash`.
//!
//! Format (little-endian, fixed width):
//!   `u32 version` | `u32 count` | `count × (i64 token_hash, i64 freq)` sorted by `token_hash` ASC.
//! So `len == HEADER_LEN + count * PAIR_LEN`. The sort makes the BLOB deterministic + content-
//! addressable, and the read needs no re-sort (`overlap` requires token_hash order). The version
//! header guards against a format change: a mismatch (or any truncation / length violation) decodes
//! to `None`, treated as an absent/stale bag that a reindex repopulates.

/// Codec format version. Bumped only when the BLOB layout itself changes — independent of
/// `NORM_VERSION` (which invalidates fingerprint CONTENT). A decode of any other version is `None`.
const BAG_BLOB_VERSION: u32 = 1;

/// `u32 version` + `u32 count`.
const HEADER_LEN: usize = 8;
/// `i64 token_hash` + `i64 freq`.
const PAIR_LEN: usize = 16;

/// Pack a `(token_hash, freq)` multiset into the versioned BLOB. The input is expected sorted by
/// `token_hash` with no duplicate hashes (the contract of `tokens::token_bag`); this preserves that
/// order verbatim, so the decode is byte-lossless and the bag never needs re-sorting on read.
pub(crate) fn encode_token_bag(bag: &[(i64, i64)]) -> Vec<u8> {
    let count = bag.len();
    let mut buf = Vec::with_capacity(HEADER_LEN + count * PAIR_LEN);
    buf.extend_from_slice(&BAG_BLOB_VERSION.to_le_bytes());
    buf.extend_from_slice(&(count as u32).to_le_bytes());
    for &(token_hash, freq) in bag {
        buf.extend_from_slice(&token_hash.to_le_bytes());
        buf.extend_from_slice(&freq.to_le_bytes());
    }
    buf
}

/// Decode a token-bag BLOB back to its `(token_hash, freq)` pairs, or `None` if the bytes are not a
/// current-version, well-formed bag. `None` cases (all treated as "no bag", forcing a recompute on
/// the next reindex — never a panic): wrong version, header shorter than [`HEADER_LEN`], or a
/// length that disagrees with the declared count. The returned pairs keep the stored order
/// (token_hash ASC).
pub(crate) fn decode_token_bag(blob: &[u8]) -> Option<Vec<(i64, i64)>> {
    if blob.len() < HEADER_LEN {
        return None;
    }
    let version = u32::from_le_bytes(blob[0..4].try_into().ok()?);
    if version != BAG_BLOB_VERSION {
        return None;
    }
    let count = u32::from_le_bytes(blob[4..8].try_into().ok()?) as usize;
    // Reject any length that disagrees with the declared count: a truncated/over-long blob is
    // corrupt, not a partial bag.
    if blob.len() != HEADER_LEN + count * PAIR_LEN {
        return None;
    }
    let mut bag = Vec::with_capacity(count);
    for i in 0..count {
        let base = HEADER_LEN + i * PAIR_LEN;
        let token_hash = i64::from_le_bytes(blob[base..base + 8].try_into().ok()?);
        let freq = i64::from_le_bytes(blob[base + 8..base + 16].try_into().ok()?);
        bag.push((token_hash, freq));
    }
    Some(bag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::clones::tokens;

    #[test]
    fn token_bag_blob_round_trips() {
        let bag = vec![(-9_223_372_036_854_775_808, 3), (0, 1), (42, 7), (i64::MAX, 2)];
        let blob = encode_token_bag(&bag);
        assert_eq!(decode_token_bag(&blob).expect("decodes"), bag, "round-trips exactly");
    }

    #[test]
    fn empty_bag_round_trips() {
        let blob = encode_token_bag(&[]);
        assert_eq!(blob.len(), HEADER_LEN, "empty bag is header-only");
        assert_eq!(decode_token_bag(&blob).expect("decodes"), Vec::<(i64, i64)>::new());
    }

    #[test]
    fn large_bag_round_trips() {
        let bag: Vec<(i64, i64)> =
            (0..5000).map(|i| (i as i64 * 31 - 1000, (i % 9 + 1) as i64)).collect();
        let blob = encode_token_bag(&bag);
        assert_eq!(blob.len(), HEADER_LEN + bag.len() * PAIR_LEN);
        assert_eq!(decode_token_bag(&blob).expect("decodes"), bag);
    }

    #[test]
    fn wrong_version_decodes_to_none() {
        let mut blob = encode_token_bag(&[(1, 1)]);
        blob[0] = blob[0].wrapping_add(1); // corrupt the version byte
        assert_eq!(decode_token_bag(&blob), None, "version mismatch is None, not a panic");
    }

    #[test]
    fn truncated_blob_decodes_to_none() {
        let blob = encode_token_bag(&[(1, 1), (2, 2), (3, 3)]);
        // Drop the final byte: the length no longer matches the declared count.
        assert_eq!(decode_token_bag(&blob[..blob.len() - 1]), None);
        // A header claiming a count its body can't satisfy.
        assert_eq!(decode_token_bag(&blob[..HEADER_LEN]), None);
        // Too short even for a header.
        assert_eq!(decode_token_bag(&blob[..4]), None);
        assert_eq!(decode_token_bag(&[]), None);
    }

    #[test]
    fn over_long_blob_decodes_to_none() {
        let mut blob = encode_token_bag(&[(1, 1)]);
        blob.push(0); // trailing junk past the declared count
        assert_eq!(decode_token_bag(&blob), None);
    }

    /// R11: decoding the REAL `tokens::token_bag` output yields NO duplicate token_hash, and the
    /// decoded length equals `tokens::token_bag().len()` (so `token_len` stays derivable from the
    /// BLOB — the codec is lossless and the bag is a true multiset).
    #[test]
    fn decoded_real_token_bag_has_no_duplicate_hashes_and_is_lossless() {
        let toks: Vec<String> = ["if", "x", "(", "y", ")", "x", "y", "x", "y", "if", "z", "z", "z"]
            .iter()
            .map(|w| w.to_string())
            .collect();
        let bag = tokens::token_bag(&toks);
        let blob = encode_token_bag(&bag);
        let decoded = decode_token_bag(&blob).expect("decodes");

        assert_eq!(decoded, bag, "byte-lossless against the real producer");

        let mut hashes: Vec<i64> = decoded.iter().map(|&(h, _)| h).collect();
        let distinct = hashes.len();
        hashes.dedup();
        assert_eq!(hashes.len(), distinct, "no duplicate token_hash after decode");

        let mut sorted = hashes.clone();
        sorted.sort_unstable();
        assert_eq!(hashes, sorted, "decoded bag is token_hash-sorted (no re-sort needed on read)");
    }
}
