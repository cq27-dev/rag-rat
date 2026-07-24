//! Content hashing shared across subsystems.

use sha2::{Digest, Sha256};

/// Lower-hex encode a byte slice (two chars per byte, `0`-padded). The one hex encoder for the
/// workspace — there is no `hex` crate dependency, so every caller that needs raw-bytes→hex (digest
/// rendering, a hex-encoded meta value, the table-sync golden vectors) routes through here rather
/// than hand-rolling the loop.
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Hex SHA-256 of a byte slice. This is the hash space of `files.sha256` (the indexer writes
/// `hex_sha256(fs::read(file))`), so every consumer that compares against stored file hashes —
/// the content-integrity check, the oracle's scip-vs-disk gate (#82 TOCTOU) — MUST hash through
/// this function to stay in the same space.
pub fn hex_sha256(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}
