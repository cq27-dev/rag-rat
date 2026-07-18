//! Shared canonical-CBOR discipline for the op-log wire (phase B, §5.4/§5.6).
//!
//! Every op-log object — the op envelope ([`super::op`]) and the signed, hash-chained entry
//! ([`super::entry`]) — serializes to CANONICAL, deterministic CBOR (RFC 8949 §4.2
//! core-deterministic): DEFINITE lengths only, MINIMAL-length argument headers, sorted + unique map
//! keys, no floats (this wire is integer-only). Canonicity is the security boundary: a signature or
//! `entry_hash` over these bytes is only unambiguous if the SAME logical value has exactly ONE
//! accepted encoding.
//!
//! Two complementary gates live here, both used by op + entry:
//! - [`require_canonical_cbor`] walks RAW bytes and rejects every non-canonical encoding — the
//!   check a retained-opaque value (an unknown op, an opaque `op_bytes` inside an entry body)
//!   needs, since it can't be re-encoded from a decoded form.
//! - [`expect_array`] / [`expect_definite_len`] are the `minicbor::Decoder` structural helpers that
//!   read a definite-length array header (canonical CBOR is definite-length only; an indefinite or
//!   wrong-arity header is a hard error).
//!
//! A KNOWN object gets an even stronger, value-level guarantee from its module by re-encoding and
//! demanding `encode(decode(bytes)) == bytes` (e.g. sorted+deduped tags in `op`); this module is
//! the encoding-level floor beneath that.

use minicbor::decode::{Decoder, Error as CborError};
use sha2::{Digest, Sha256};

/// Validate that `bytes` is EXACTLY one canonical CBOR item (RFC 8949 §4.2 core-deterministic) with
/// no trailing bytes: MINIMAL-length argument headers, DEFINITE lengths only, sorted + unique map
/// keys, and no floats (this wire format is integer-only). This is the encoding-level canonicity a
/// retained-opaque value needs; a known object gets the same guarantee (plus value-level rules)
/// from `encode(decode) == bytes`.
pub(super) fn require_canonical_cbor(bytes: &[u8]) -> Result<(), CborError> {
    let mut pos = 0;
    check_canonical_item(bytes, &mut pos, 0)?;
    if pos != bytes.len() {
        return Err(CborError::message("trailing bytes after canonical CBOR item"));
    }
    Ok(())
}

/// Max CBOR nesting depth. Real objects nest shallowly (envelope → payload array → content array →
/// tags array ≈ depth 4); a deeper structure is malformed/hostile, and unbounded recursion in the
/// validator would overflow the stack — so cap it well above any real object and reject beyond.
pub(super) const MAX_CBOR_DEPTH: usize = 32;

/// Validate one canonical CBOR item at `*pos`, advancing past it; recurses into arrays/maps/tags
/// with a bounded `depth` (a pathologically nested input errors instead of overflowing the stack).
fn check_canonical_item(bytes: &[u8], pos: &mut usize, depth: usize) -> Result<(), CborError> {
    if depth > MAX_CBOR_DEPTH {
        return Err(CborError::message("CBOR nesting too deep"));
    }
    let (major, arg) = read_canonical_header(bytes, pos)?;
    match major {
        0 | 1 => Ok(()), // uint / negative int — the header IS the value
        2 => {
            // byte string: `arg` opaque content bytes (no UTF-8 requirement).
            advance_string(bytes, pos, arg)
        },
        3 => {
            // text string: `arg` bytes that MUST be valid UTF-8. A future decoder reads this field
            // with `d.str()`, which rejects invalid UTF-8 — so a non-UTF-8 text string is not a
            // re-foldable canonical object even though its length header is well-formed.
            let start = *pos;
            advance_string(bytes, pos, arg)?;
            str::from_utf8(&bytes[start..*pos])
                .map_err(|_| CborError::message("invalid UTF-8 in CBOR text string"))?;
            Ok(())
        },
        4 => {
            for _ in 0..arg {
                check_canonical_item(bytes, pos, depth + 1)?;
            }
            Ok(())
        },
        5 => {
            // Map: keys must be strictly ascending by their encoded bytes (sorted + no duplicates).
            let mut prev_key: Option<&[u8]> = None;
            for _ in 0..arg {
                let key_start = *pos;
                check_canonical_item(bytes, pos, depth + 1)?;
                let key = &bytes[key_start..*pos];
                if prev_key.is_some_and(|prev| key <= prev) {
                    return Err(CborError::message("map keys not sorted or duplicated"));
                }
                prev_key = Some(key);
                check_canonical_item(bytes, pos, depth + 1)?;
            }
            Ok(())
        },
        6 => check_canonical_item(bytes, pos, depth + 1), // tag: one following item
        7 => Ok(()),                                      /* simple value (null/bool/…); floats */
        // already rejected by the header
        // reader
        _ => Err(CborError::message("invalid CBOR major type")),
    }
}

/// Read one CBOR item header at `*pos`, returning `(major, argument)` and advancing past it.
/// Rejects non-minimal argument encodings, indefinite/reserved lengths, and floats.
fn read_canonical_header(bytes: &[u8], pos: &mut usize) -> Result<(u8, u64), CborError> {
    let first = read_u8(bytes, pos)?;
    let major = first >> 5;
    let arg = match first & 0x1f {
        info @ 0..=23 => u64::from(info),
        24 => {
            let v = u64::from(read_u8(bytes, pos)?);
            require(v >= 24, "non-minimal 1-byte CBOR argument")?;
            v
        },
        25 => {
            require(major != 7, "float is non-canonical (integer-only wire format)")?;
            let v = read_be(bytes, pos, 2)?;
            require(v > u64::from(u8::MAX), "non-minimal 2-byte CBOR argument")?;
            v
        },
        26 => {
            require(major != 7, "float is non-canonical (integer-only wire format)")?;
            let v = read_be(bytes, pos, 4)?;
            require(v > u64::from(u16::MAX), "non-minimal 4-byte CBOR argument")?;
            v
        },
        27 => {
            require(major != 7, "float is non-canonical (integer-only wire format)")?;
            let v = read_be(bytes, pos, 8)?;
            require(v > u64::from(u32::MAX), "non-minimal 8-byte CBOR argument")?;
            v
        },
        _ => return Err(CborError::message("reserved or indefinite CBOR length")), // 28..=31
    };
    Ok((major, arg))
}

/// Advance `*pos` past `arg` string content bytes, bounds-checking against `bytes`.
fn advance_string(bytes: &[u8], pos: &mut usize, arg: u64) -> Result<(), CborError> {
    let len = usize::try_from(arg).map_err(|_| CborError::message("CBOR length overflow"))?;
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| CborError::message("CBOR string runs past end"))?;
    *pos = end;
    Ok(())
}

fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, CborError> {
    let byte = *bytes.get(*pos).ok_or_else(|| CborError::message("unexpected end of CBOR"))?;
    *pos += 1;
    Ok(byte)
}

/// Read `n` (≤ 8) big-endian bytes into a `u64`.
fn read_be(bytes: &[u8], pos: &mut usize, n: usize) -> Result<u64, CborError> {
    let end = pos
        .checked_add(n)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| CborError::message("unexpected end of CBOR"))?;
    let value = bytes[*pos..end].iter().fold(0u64, |acc, &byte| (acc << 8) | u64::from(byte));
    *pos = end;
    Ok(value)
}

fn require(condition: bool, message: &'static str) -> Result<(), CborError> {
    if condition { Ok(()) } else { Err(CborError::message(message)) }
}

/// Read a definite-length array header and assert its element count — canonical CBOR is
/// definite-length only, and a wrong count is a structural (hard) error.
pub(super) fn expect_array(d: &mut Decoder<'_>, want: u64) -> Result<(), CborError> {
    let got = expect_definite_len(d)?;
    if got == want {
        Ok(())
    } else {
        Err(CborError::message(format!("expected a {want}-element array, got {got}")))
    }
}

/// Read a definite-length array header, returning its element count. Rejects an indefinite-length
/// array (canonical CBOR is definite-length only).
pub(super) fn expect_definite_len(d: &mut Decoder<'_>) -> Result<u64, CborError> {
    d.array()?.ok_or_else(|| CborError::message("expected a definite-length array"))
}

/// `sha256` into a fixed 32-byte array — the entry-hash / content-address primitive shared by the
/// op-log entry envelope ([`super::entry`]) and the account layer ([`super::account`]).
pub(super) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

/// Convert an opaque byte slice to a fixed `[u8; N]`, erroring with the field name on a length
/// mismatch (a wrong-length hash / fingerprint / signature is a structural error). Shared by every
/// signed-wire decoder in `oplog`.
pub(super) fn fixed_bytes<const N: usize>(bytes: &[u8], field: &str) -> Result<[u8; N], CborError> {
    <[u8; N]>::try_from(bytes)
        .map_err(|_| CborError::message(format!("{field} must be {N} bytes, got {}", bytes.len())))
}

/// Read a leading domain string and assert it matches `want` — a wrong/absent tag is a foreign or
/// version-bumped object an old binary must reject, never misread. Shared by every domain-tagged
/// decoder in `oplog`.
pub(super) fn expect_domain(d: &mut Decoder<'_>, want: &str) -> Result<(), CborError> {
    let got = d.str()?;
    if got == want {
        Ok(())
    } else {
        Err(CborError::message(format!("unknown domain tag `{got}` (expected `{want}`)")))
    }
}
