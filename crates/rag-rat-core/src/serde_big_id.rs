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

struct BigIdVisitor;

impl Visitor<'_> for BigIdVisitor {
    type Value = BigId;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an i64 as a decimal string or a JSON number")
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<BigId, E> {
        Ok(BigId(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<BigId, E> {
        i64::try_from(value)
            .map(BigId)
            .map_err(|_| E::invalid_value(Unexpected::Unsigned(value), &self))
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
    fn required_big_id_still_accepts_a_number_for_back_compat() {
        // A small in-safe-range value sent as a JSON number must still deserialize.
        let back: Req = serde_json::from_str(r#"{"id":1001}"#).unwrap();
        assert_eq!(back.id, 1001);
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
