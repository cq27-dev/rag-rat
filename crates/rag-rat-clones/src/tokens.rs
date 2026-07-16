//! Token-level helpers for clone-detection fingerprints: FNV-1a hashing, struct hash, and token
//! bag (multiset) construction. All hashing is deterministic and stable across machines and runs.

use std::collections::HashMap;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Content hash of the exact normalized token sequence — the exact-after-normalization fast path.
pub fn struct_hash(tokens: &[String]) -> String {
    rag_rat_base::hash::hex_sha256(tokens.join("\u{1}").as_bytes())
}

/// Build a `(token_hash, freq)` multiset from a token sequence. `token_hash` is
/// `fnv1a(token.as_bytes()) as i64` (reinterpreted as signed for SQLite INTEGER compatibility).
/// The result is sorted by `token_hash` for deterministic storage and lookup.
pub(crate) fn token_bag(tokens: &[String]) -> Vec<(i64, i64)> {
    let mut counts: HashMap<i64, i64> = HashMap::new();
    for token in tokens {
        let hash = fnv1a(token.as_bytes()) as i64;
        *counts.entry(hash).or_insert(0) += 1;
    }
    let mut bag: Vec<(i64, i64)> = counts.into_iter().collect();
    bag.sort_unstable_by_key(|&(hash, _)| hash);
    bag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn identical_token_streams_have_equal_token_bag() {
        let a = toks(&["if", "x", "(", "y", ")", "x", "y", "x", "y"]);
        let b = a.clone();
        assert_eq!(token_bag(&a), token_bag(&b));
    }

    #[test]
    fn repeated_token_has_freq_greater_than_one() {
        let tokens = toks(&["a", "b", "a", "c", "a"]);
        let bag = token_bag(&tokens);
        let a_hash = fnv1a(b"a") as i64;
        let entry = bag.iter().find(|&&(h, _)| h == a_hash).expect("a in bag");
        assert_eq!(entry.1, 3, "token 'a' appears 3 times");
    }

    #[test]
    fn distinct_token_streams_produce_different_bags() {
        let a = toks(&["foo", "bar", "baz"]);
        let b = toks(&["qux", "quux", "corge"]);
        assert_ne!(token_bag(&a), token_bag(&b));
    }

    #[test]
    fn token_bag_is_sorted_by_token_hash() {
        let tokens = toks(&["z", "a", "m", "b", "z", "a"]);
        let bag = token_bag(&tokens);
        let hashes: Vec<i64> = bag.iter().map(|&(h, _)| h).collect();
        let mut sorted = hashes.clone();
        sorted.sort();
        assert_eq!(hashes, sorted, "bag must be sorted by token_hash");
    }

    #[test]
    fn identical_token_streams_have_equal_struct_hash() {
        let a = toks(&["if", "ID0", "(", "ID1", ")", "ID0", "ID1", "ID0", "ID1"]);
        let b = a.clone();
        assert_eq!(struct_hash(&a), struct_hash(&b));
    }
}
