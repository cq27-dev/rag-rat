//! Serde for the symbol handle (`logical_symbol_id` and its call-path `start_/end_` variants) — a
//! content-derived `i64` far above JSON's 2^53 safe-integer range (~2.5e18 vs
//! 9_007_199_254_740_992). A JSON *number* cannot carry such a value losslessly: a client that
//! parses numbers as f64 (every JS-based MCP client) silently rounds it, corrupting the id in
//! transit (#130).
//!
//! The handle therefore crosses every serde boundary (CLI `--json`, MCP JSON, MCP TOON) as an
//! opaque `sym_<hex>` token (#149). A decimal string would also be lossless, but it still *looks*
//! like a number and tempts a client to `parseInt` it (and round); the `sym_` prefix + hex make it
//! unmistakably a handle, never a number — copy it verbatim and round-trip it intact. Deserialize
//! accepts ONLY the token, so a stale numeric rowid passed by habit fails loudly.
//!
//! Apply with `#[serde(with = "crate::serde_big_id::sym_handle")]` on an `i64` field or
//! `#[serde(with = "crate::serde_big_id::sym_handle_opt")]` on an `Option<i64>` field. On an MCP
//! arg struct that derives `JsonSchema`, also add `#[schemars(with = "String")]` (or
//! `Option<String>`) so the advertised input schema asks clients for a string.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, Unexpected, Visitor};
use serde::{Serialize, Serializer};

/// An `i64` symbol handle that crosses serde as an opaque `sym_<hex>` token. Hex is over the raw
/// u64 bit pattern so any `i64` round-trips exactly. Private — the public surface is the
/// `sym_handle` / `sym_handle_opt` serde modules.
struct SymHandle(i64);

const SYM_HANDLE_PREFIX: &str = "sym_";

/// Format an `i64` symbol handle as its opaque `sym_<hex>` token (hex over the raw u64 bits, so any
/// `i64` round-trips). The single source of truth for the on-the-wire handle shape — serde and any
/// non-serde caller (e.g. seed-selector parsing) go through this and [`parse_sym_handle`].
pub fn format_sym_handle(value: i64) -> String {
    format!("{SYM_HANDLE_PREFIX}{:x}", value as u64)
}

/// Parse an opaque `sym_<hex>` handle back to its `i64`. `None` if the `sym_` prefix is missing or
/// the remainder isn't valid hex — callers decide whether that's an error or a fall-through.
pub fn parse_sym_handle(token: &str) -> Option<i64> {
    let hex = token.strip_prefix(SYM_HANDLE_PREFIX)?;
    u64::from_str_radix(hex, 16).ok().map(|bits| bits as i64)
}

impl Serialize for SymHandle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_sym_handle(self.0))
    }
}

impl<'de> Deserialize<'de> for SymHandle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(SymHandleVisitor)
    }
}

struct SymHandleVisitor;

impl Visitor<'_> for SymHandleVisitor {
    type Value = SymHandle;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a symbol handle string of the form `sym_<hex>`")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<SymHandle, E> {
        let hex = value.strip_prefix(SYM_HANDLE_PREFIX).ok_or_else(|| {
            E::custom(format!("symbol handle must start with `{SYM_HANDLE_PREFIX}`: `{value}`"))
        })?;
        u64::from_str_radix(hex, 16)
            .map(|bits| SymHandle(bits as i64))
            .map_err(|_| E::invalid_value(Unexpected::Str(value), &self))
    }
}

/// `#[serde(with = "...")]` module for a required `i64` symbol handle (`sym_<hex>` out, token in).
pub mod sym_handle {
    use super::{Deserialize, Deserializer, Serialize, Serializer, SymHandle};

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        SymHandle(*value).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        SymHandle::deserialize(deserializer).map(|handle| handle.0)
    }
}

/// `#[serde(with = "...")]` module for an `Option<i64>` symbol handle — `null`/absent stays `None`.
pub mod sym_handle_opt {
    use super::{Deserialize, Deserializer, Serialize, Serializer, SymHandle};

    pub fn serialize<S: Serializer>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error> {
        value.map(SymHandle).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<i64>, D::Error> {
        Ok(Option::<SymHandle>::deserialize(deserializer)?.map(|handle| handle.0))
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    /// A content-derived logical_symbol_id beyond JS's 2^53 safe-integer ceiling — the value that
    /// rounded to `...343700` as a JSON number in the original field report (#130).
    const BIG: i64 = 2_574_604_874_062_343_519;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Handle {
        #[serde(with = "super::sym_handle")]
        id: i64,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct HandleOpt {
        #[serde(with = "super::sym_handle_opt")]
        id: Option<i64>,
    }

    #[test]
    fn sym_handle_serializes_as_an_opaque_token() {
        let json = serde_json::to_string(&Handle { id: BIG }).unwrap();
        assert_eq!(json, r#"{"id":"sym_23bad57dfb79ad5f"}"#, "must emit sym_<hex>, not a number");
    }

    #[test]
    fn sym_handle_round_trips_losslessly_including_negative_and_zero() {
        for value in [BIG, 0_i64, 1, -1, i64::MAX, i64::MIN] {
            let json = serde_json::to_string(&Handle { id: value }).unwrap();
            let back: Handle = serde_json::from_str(&json).unwrap();
            assert_eq!(back.id, value, "round-trip failed for {value}");
        }
    }

    #[test]
    fn sym_handle_rejects_a_bare_number_and_a_missing_prefix() {
        // Clean break (#149): a numeric id (the old form) is no longer accepted, so a stale id
        // passed by habit fails loudly instead of resolving to nothing.
        assert!(serde_json::from_str::<Handle>(r#"{"id":2574604874062343519}"#).is_err());
        assert!(
            serde_json::from_str::<Handle>(r#"{"id":"23bad57dfb79ad5f"}"#).is_err(),
            "needs prefix"
        );
        assert!(serde_json::from_str::<Handle>(r#"{"id":"sym_zzz"}"#).is_err(), "bad hex");
    }

    #[test]
    fn parse_and_format_sym_handle_round_trip() {
        for value in [BIG, 0_i64, -1, i64::MAX, i64::MIN] {
            let token = super::format_sym_handle(value);
            assert!(token.starts_with("sym_"), "token must carry the prefix: {token}");
            assert_eq!(super::parse_sym_handle(&token), Some(value), "round-trip via helpers");
        }
        assert_eq!(super::parse_sym_handle("23bad57dfb79ad5f"), None, "missing prefix → None");
        assert_eq!(super::parse_sym_handle("sym_zzz"), None, "bad hex → None");
        assert_eq!(super::parse_sym_handle("12345"), None, "bare number → None");
    }

    #[test]
    fn sym_handle_opt_serializes_token_or_null() {
        assert_eq!(
            serde_json::to_string(&HandleOpt { id: Some(BIG) }).unwrap(),
            r#"{"id":"sym_23bad57dfb79ad5f"}"#
        );
        assert_eq!(serde_json::to_string(&HandleOpt { id: None }).unwrap(), r#"{"id":null}"#);
        let back: HandleOpt = serde_json::from_str(r#"{"id":"sym_23bad57dfb79ad5f"}"#).unwrap();
        assert_eq!(back.id, Some(BIG));
    }
}
