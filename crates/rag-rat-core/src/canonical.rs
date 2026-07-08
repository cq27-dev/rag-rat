//! Canonical, deterministic encoding for signed / content-addressed objects (phase B, §5.5).
//!
//! Every hashed or signed object is encoded as a **canonical CBOR** value (RFC 8949 §4.2
//! core-deterministic): definite lengths, minimal integer encoding, map keys sorted by their
//! CBOR-encoded bytes, and UTF-8 text **NFC**-normalized. Hashes are always over a canonical CBOR
//! tuple/struct — NEVER raw string concatenation, which is delimiter-malleable. Each object is
//! DOMAIN-TAGGED and versioned (`"rag-rat/content-hash/1"`, …) as its first tuple element, so two
//! object kinds can never collide and the canonical rule can be versioned independently of the
//! data.
//!
//! This module is the shared substrate: `content_hash` (query/memory) and, later, the edge_key /
//! op-log entry hashes build their tuples on top of `encode_canonical_json` + `nfc`.

use std::fmt;

use minicbor::Encoder;
use minicbor::encode::Write;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// NFC-normalize a string (RFC 8949 identity requirement — canonically-equivalent Unicode must hash
/// identically).
pub(crate) fn nfc(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect()
}

/// Parse a payload JSON, ERRORING on a duplicate object key at ANY depth. `serde_json`'s default
/// `Value` silently keeps the LAST of a repeated key — but JSON parsers disagree on which wins, so
/// a dup-key payload would hash differently across devices and diverge sync. Rejecting it on write
/// keeps the cross-device content hash well-defined. (NFC-normalized duplicates are caught
/// separately by [`payload_encoding_error`].)
pub(crate) fn parse_rejecting_duplicate_keys(raw: &str) -> Result<Value, String> {
    struct Strict(Value);

    impl<'de> Deserialize<'de> for Strict {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Value;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a JSON value")
                }

                fn visit_unit<E>(self) -> Result<Value, E> {
                    Ok(Value::Null)
                }

                fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
                    Ok(Value::Bool(b))
                }

                fn visit_i64<E>(self, n: i64) -> Result<Value, E> {
                    Ok(n.into())
                }

                fn visit_u64<E>(self, n: u64) -> Result<Value, E> {
                    Ok(n.into())
                }

                fn visit_f64<E>(self, n: f64) -> Result<Value, E> {
                    Ok(serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number))
                }

                fn visit_str<E>(self, s: &str) -> Result<Value, E> {
                    Ok(Value::String(s.to_owned()))
                }

                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
                    let mut arr = Vec::new();
                    while let Some(Strict(v)) = seq.next_element()? {
                        arr.push(v);
                    }
                    Ok(Value::Array(arr))
                }

                fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Value, A::Error> {
                    let mut map = serde_json::Map::new();
                    while let Some((k, Strict(v))) = m.next_entry::<String, Strict>()? {
                        if map.insert(k.clone(), v).is_some() {
                            return Err(de::Error::custom(format!("duplicate object key `{k}`")));
                        }
                    }
                    Ok(Value::Object(map))
                }
            }
            d.deserialize_any(V).map(Strict)
        }
    }

    serde_json::from_str::<Strict>(raw).map(|s| s.0).map_err(|e| e.to_string())
}

/// Encode a JSON value as CANONICAL CBOR into `enc`:
/// - integers via minimal unsigned/negative encoding;
/// - text strings NFC-normalized;
/// - arrays in order; objects as maps whose keys are sorted by their CBOR-ENCODED bytes (§4.2.1),
///   not merely by code point — the length prefix participates in the ordering.
///
/// ERRORS on a NON-INTEGER number (JSON floats collapse to binary64 → an unreliable content-hash
/// input) or on two object keys that NFC-normalize to the same key (a dup-key map is
/// non-canonical). These are the only error paths when writing to a `Vec<u8>` (whose own writes are
/// infallible), and [`payload_encoding_error`] surfaces both at write time so a stored payload
/// never trips them later.
pub(crate) fn encode_canonical_json<W: Write>(
    value: &Value,
    enc: &mut Encoder<W>,
) -> Result<(), minicbor::encode::Error<W::Error>> {
    match value {
        Value::Null => {
            enc.null()?;
        },
        Value::Bool(b) => {
            enc.bool(*b)?;
        },
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                enc.i64(i)?;
            } else if let Some(u) = n.as_u64() {
                enc.u64(u)?;
            } else {
                // REJECT a non-integer number. JSON floats collapse to binary64 at parse time, so
                // `1.0` and `1.0000000000000001` would hash identically — a payload edit the
                // freshness key must NOT miss. A fractional value must be a STRING or a scaled
                // integer. (`validate_payload` surfaces this on write; `content_hash` raw-hashes a
                // legacy/out-of-band float payload.)
                return Err(minicbor::encode::Error::message(
                    "canonical encoding rejects non-integer numbers (use a string or a scaled \
                     integer for fractional values)",
                ));
            }
        },
        Value::String(s) => {
            enc.str(&nfc(s))?;
        },
        Value::Array(items) => {
            enc.array(items.len() as u64)?;
            for item in items {
                encode_canonical_json(item, enc)?;
            }
        },
        Value::Object(map) => {
            // Sort by the CBOR-encoded key bytes (§4.2.1) — a text string's length prefix sorts
            // before its content, so this is NOT the same as sorting the NFC strings directly.
            let mut entries: Vec<(Vec<u8>, String, &Value)> = map
                .iter()
                .map(|(k, v)| {
                    let key = nfc(k);
                    (encoded_str(&key), key, v)
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            // A canonical map forbids duplicate keys (§4.2.1). NFC-normalizing keys can collapse
            // two DISTINCT source keys (e.g. a precomposed vs a decomposed "café") to
            // the SAME key — reject rather than emit an ambiguous dup-key map.
            // (`validate_payload` runs this on write, so a stored payload never reaches
            // `content_hash` with such keys.)
            if entries.windows(2).any(|w| w[0].0 == w[1].0) {
                return Err(minicbor::encode::Error::message(
                    "canonical CBOR forbids duplicate map keys (two keys normalized to the same \
                     value)",
                ));
            }
            enc.map(entries.len() as u64)?;
            for (_, key, v) in &entries {
                enc.str(key)?;
                encode_canonical_json(v, enc)?;
            }
        },
    }
    Ok(())
}

/// The canonical-encoding error for a payload `value`, or `None` if it encodes cleanly. Currently
/// the sole failure is a pair of object keys that NFC-normalize to the same key. Called by
/// `validate_payload` on WRITE so a stored payload always hashes cleanly (`content_hash` can then
/// treat the encoding as infallible).
pub(crate) fn payload_encoding_error(value: &Value) -> Option<String> {
    let mut buf = Vec::new();
    encode_canonical_json(value, &mut Encoder::new(&mut buf)).err().map(|e| e.to_string())
}

/// The canonical CBOR encoding of a single text string — used only to derive a map key's sort
/// order.
fn encoded_str(s: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    Encoder::new(&mut buf).str(s).expect("encoding a str to a Vec is infallible");
    buf
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cbor(value: &Value) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_canonical_json(value, &mut Encoder::new(&mut buf)).unwrap();
        buf
    }

    #[test]
    fn object_key_order_is_normalized() {
        // The same entries in a different insertion order encode byte-identically.
        assert_eq!(cbor(&json!({"b": 1, "a": 2, "c": 3})), cbor(&json!({"c": 3, "a": 2, "b": 1})));
    }

    #[test]
    fn keys_sort_by_encoded_bytes_not_code_points() {
        // "z" (encoded 0x61 0x7a) precedes "aa" (0x62 0x61 0x61): the CBOR length prefix sorts
        // first, so this is NOT plain lexicographic order (where "aa" < "z").
        let bytes = cbor(&json!({"aa": 1, "z": 2}));
        let z = bytes.windows(2).position(|w| w == [0x61, 0x7a]).unwrap();
        let aa = bytes.windows(3).position(|w| w == [0x62, 0x61, 0x61]).unwrap();
        assert!(z < aa, "the shorter key sorts first by encoded bytes: {bytes:02x?}");
    }

    #[test]
    fn nfc_equivalent_strings_encode_identically() {
        // é precomposed (U+00E9) vs e + combining acute (U+0065 U+0301) are NFC-equivalent.
        assert_eq!(cbor(&json!("caf\u{00e9}")), cbor(&json!("cafe\u{0301}")));
    }

    #[test]
    fn integers_are_minimal_and_distinct_from_floats() {
        assert_eq!(cbor(&json!(5)), vec![0x05], "unsigned 5 is a single minimal byte");
    }

    #[test]
    fn non_integer_numbers_are_rejected() {
        // A float can't be a reliable content-hash input (it collapses to binary64), so the
        // canonical encoder rejects it at the top level AND nested in a payload object.
        let mut buf = Vec::new();
        assert!(encode_canonical_json(&json!(1.5), &mut Encoder::new(&mut buf)).is_err());
        assert!(payload_encoding_error(&json!({"x": 0.1})).is_some());
        // Integers are still fine.
        assert!(payload_encoding_error(&json!({"x": 42, "y": -3})).is_none());
    }

    #[test]
    fn strict_parse_accepts_every_json_value_kind() {
        // Exercise every Visitor arm: null, bool, signed/unsigned int, float, string, array,
        // object.
        let v = parse_rejecting_duplicate_keys(
            r#"{"n":null,"b":true,"i":-5,"u":9,"f":1.5,"s":"x","arr":[1,"y",false,null]}"#,
        )
        .unwrap();
        assert!(v.is_object());
        assert_eq!(v["arr"].as_array().unwrap().len(), 4);
        assert_eq!(v["i"], json!(-5));
        assert_eq!(v["b"], json!(true));
        // Top-level non-object values parse too (visit_seq / scalar arms at the root).
        assert_eq!(parse_rejecting_duplicate_keys("42").unwrap(), json!(42));
        assert_eq!(parse_rejecting_duplicate_keys("[1,2]").unwrap(), json!([1, 2]));
        // A genuine syntax error is surfaced (not a dup message).
        assert!(parse_rejecting_duplicate_keys("{bad").is_err());
    }

    #[test]
    fn rejects_keys_that_normalize_to_a_duplicate() {
        // Two DISTINCT source keys — "café" precomposed (U+00E9) and decomposed (e + U+0301) —
        // collapse to one NFC key, which would make an ambiguous dup-key map.
        let dup = json!({"caf\u{00e9}": 1, "cafe\u{0301}": 2});
        let mut buf = Vec::new();
        assert!(
            encode_canonical_json(&dup, &mut Encoder::new(&mut buf)).is_err(),
            "NFC-duplicate keys must be rejected"
        );
        // A normal object still encodes fine.
        assert!(payload_encoding_error(&json!({"a": 1, "b": 2})).is_none());
        assert!(payload_encoding_error(&dup).is_some());
    }
}
