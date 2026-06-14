//! Serde for `i64` ids that exceed JSON's 2^53 safe-integer range — content-derived
//! `logical_symbol_id` hashes (~2.5e18, far above 2^53 = 9_007_199_254_740_992). A JSON *number*
//! cannot carry such a value losslessly: a client that parses JSON numbers as f64 (every JS-based
//! MCP client) silently rounds it (`2574604874062343519` → `2574604874062343700`), so the id is
//! corrupted in transit before the server ever sees it (#130).
//!
//! The fix: these ids cross EVERY serde boundary (CLI `--json`, MCP JSON, MCP TOON) as a STRING.
//! - Serialize → always a string, so a reader copies an opaque string and round-trips it intact.
//! - Deserialize → accept a string OR a number, so callers within the safe range (and older
//!   clients) keep working; the string form is what makes a >2^53 id survive.
//!
//! Apply with `#[serde(with = "crate::serde_big_id::big_id")]` on an `i64` field or
//! `#[serde(with = "crate::serde_big_id::big_id_opt")]` on an `Option<i64>` field. On an MCP arg
//! struct that derives `JsonSchema`, also add `#[schemars(with = "String")]` (or
//! `Option<String>`) so the advertised input schema asks clients for a string.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, Unexpected, Visitor};
use serde::{Serialize, Serializer};

/// JSON's largest exactly-representable integer, `2^53 - 1`. A number outside `±MAX_SAFE_INTEGER`
/// can't survive a JSON-number parse that goes through an f64, so by the time such a value reaches
/// us as a number it has almost certainly already been rounded — accepting it would silently look
/// up the WRONG id. The numeric deserialize path is therefore bounded to the safe range; anything
/// larger MUST arrive as a string (#130 review).
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// An `i64` that serializes as a decimal string and deserializes from either a string or a number.
/// Private — the public surface is the `big_id` / `big_id_opt` serde modules.
struct BigId(i64);

impl Serialize for BigId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for BigId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(BigIdVisitor)
    }
}

/// The error for a numeric id outside the JSON safe-integer range — such a value can't be trusted
/// (it may already be f64-rounded), so we reject it and point the caller at the string form.
fn unsafe_number_message(value: impl fmt::Display) -> String {
    format!(
        "id {value} is outside JSON's safe-integer range (±2^53-1) and may have been rounded; \
         pass it as a string"
    )
}

struct BigIdVisitor;

impl Visitor<'_> for BigIdVisitor {
    type Value = BigId;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an i64 as a decimal string, or a JSON number within ±(2^53-1)")
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<BigId, E> {
        if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
            return Err(E::custom(unsafe_number_message(value)));
        }
        Ok(BigId(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<BigId, E> {
        if value > MAX_SAFE_INTEGER as u64 {
            return Err(E::custom(unsafe_number_message(value)));
        }
        Ok(BigId(value as i64))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<BigId, E> {
        value.parse::<i64>().map(BigId).map_err(|_| E::invalid_value(Unexpected::Str(value), &self))
    }
}

/// `#[serde(with = "...")]` module for a required `i64` big id (string out, string-or-number in).
pub mod big_id {
    use super::{BigId, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        BigId(*value).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        BigId::deserialize(deserializer).map(|big| big.0)
    }
}

/// `#[serde(with = "...")]` module for an `Option<i64>` big id — `null`/absent stays `None`, a
/// present value serializes as a string and deserializes from a string or a number.
pub mod big_id_opt {
    use super::{BigId, Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error> {
        value.map(BigId).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<i64>, D::Error> {
        Ok(Option::<BigId>::deserialize(deserializer)?.map(|big| big.0))
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    /// A content-derived logical_symbol_id beyond JS's 2^53 safe-integer ceiling — the value that
    /// rounded to `...343700` as a JSON number in the field report.
    const BIG: i64 = 2_574_604_874_062_343_519;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Req {
        #[serde(with = "super::big_id")]
        id: i64,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Opt {
        #[serde(with = "super::big_id_opt")]
        id: Option<i64>,
    }

    #[test]
    fn required_big_id_serializes_as_a_string() {
        let json = serde_json::to_string(&Req { id: BIG }).unwrap();
        assert_eq!(json, r#"{"id":"2574604874062343519"}"#, "must emit a string, not a number");
    }

    #[test]
    fn required_big_id_round_trips_losslessly_through_json() {
        let json = serde_json::to_string(&Req { id: BIG }).unwrap();
        let back: Req = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, BIG);
    }

    #[test]
    fn required_big_id_deserializes_from_a_string() {
        let back: Req = serde_json::from_str(r#"{"id":"2574604874062343519"}"#).unwrap();
        assert_eq!(back.id, BIG);
    }

    #[test]
    fn required_big_id_still_accepts_a_safe_number_for_back_compat() {
        // A small in-safe-range value sent as a JSON number must still deserialize.
        let back: Req = serde_json::from_str(r#"{"id":1001}"#).unwrap();
        assert_eq!(back.id, 1001);
        // The boundary value 2^53-1 is still accepted as a number.
        let edge: Req = serde_json::from_str(r#"{"id":9007199254740991}"#).unwrap();
        assert_eq!(edge.id, 9_007_199_254_740_991);
    }

    #[test]
    fn unsafe_number_is_rejected_so_a_rounded_id_never_silently_binds() {
        // A value above 2^53 sent as a JSON NUMBER may already be f64-rounded; reject it rather
        // than look up the wrong id. The same value as a STRING is exact and accepted (#130
        // review).
        let as_number = serde_json::from_str::<Req>(r#"{"id":2574604874062343519}"#);
        assert!(as_number.is_err(), "an unsafe numeric id must be rejected, not silently accepted");
        let as_string: Req = serde_json::from_str(r#"{"id":"2574604874062343519"}"#).unwrap();
        assert_eq!(as_string.id, BIG);
        // The optional variant rejects an unsafe number too.
        assert!(serde_json::from_str::<Opt>(r#"{"id":2574604874062343519}"#).is_err());
    }

    #[test]
    fn optional_big_id_serializes_string_or_null() {
        assert_eq!(
            serde_json::to_string(&Opt { id: Some(BIG) }).unwrap(),
            r#"{"id":"2574604874062343519"}"#
        );
        assert_eq!(serde_json::to_string(&Opt { id: None }).unwrap(), r#"{"id":null}"#);
    }

    #[test]
    fn optional_big_id_deserializes_string_number_and_null() {
        let s: Opt = serde_json::from_str(r#"{"id":"2574604874062343519"}"#).unwrap();
        assert_eq!(s.id, Some(BIG));
        let n: Opt = serde_json::from_str(r#"{"id":1001}"#).unwrap();
        assert_eq!(n.id, Some(1001));
        let z: Opt = serde_json::from_str(r#"{"id":null}"#).unwrap();
        assert_eq!(z.id, None);
    }
}
