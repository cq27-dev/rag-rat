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

/// One hex nibble's value (`0-9`, `a-f`, `A-F`), or `None` for any other byte. Shared so the
/// fingerprint/digest parsers don't each re-derive the table.
pub fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode an even-length hex string. `None` on an odd length or any non-hex byte; callers that
/// need the failing position iterate [`hex_nibble`] themselves for the precise error.
pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push(hex_nibble(pair[0])? << 4 | hex_nibble(pair[1])?);
    }
    bytes.chunks_exact(2).remainder().is_empty().then_some(out)
}

/// Hex SHA-256 of a byte slice. This is the hash space of `files.sha256` (the indexer writes
/// `hex_sha256(fs::read(file))`), so every consumer that compares against stored file hashes —
/// the content-integrity check, the oracle's scip-vs-disk gate (#82 TOCTOU) — MUST hash through
/// this function to stay in the same space.
pub fn hex_sha256(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}
