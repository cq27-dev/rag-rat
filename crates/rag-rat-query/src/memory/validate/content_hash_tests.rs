use super::content_hash;

#[test]
fn folds_payload_and_schema_version() {
    let base = content_hash("t", "b", None);
    assert_ne!(base, content_hash("t", "b", Some(r#"{"schema_version":1,"x":1}"#)));
    // A payload VALUE change changes the hash.
    assert_ne!(
        content_hash("t", "b", Some(r#"{"schema_version":1,"x":1}"#)),
        content_hash("t", "b", Some(r#"{"schema_version":1,"x":2}"#)),
    );
    // A schema_version bump is a DELIBERATE identity change (§5.5), even with identical fields.
    assert_ne!(
        content_hash("t", "b", Some(r#"{"schema_version":1,"x":1}"#)),
        content_hash("t", "b", Some(r#"{"schema_version":2,"x":1}"#)),
    );
}

#[test]
fn payload_is_canonicalized_key_order_and_whitespace_dont_matter() {
    assert_eq!(
        content_hash("t", "b", Some(r#"{"a":1,"b":2}"#)),
        content_hash("t", "b", Some(r#"{ "b": 2, "a": 1 }"#)),
    );
}

#[test]
fn title_body_are_trimmed_and_nfc_normalized() {
    assert_eq!(content_hash("t", "b", None), content_hash("  t  ", "\nb\t", None));
    // é precomposed (U+00E9) vs e + combining acute (U+0065 U+0301).
    assert_eq!(content_hash("caf\u{00e9}", "b", None), content_hash("cafe\u{0301}", "b", None),);
}

#[test]
fn no_payload_differs_from_empty_object() {
    // `None` (CBOR null) is a distinct identity from an empty-object payload `{}`.
    assert_ne!(content_hash("t", "b", None), content_hash("t", "b", Some("{}")));
}

#[test]
fn non_object_legacy_payload_does_not_collide_with_none() {
    // A non-object text payload (`"null"`, a scalar) folds as raw BYTES, not structured CBOR —
    // else `"null"` would encode to the same CBOR-null element as `None` and collide.
    assert_ne!(content_hash("t", "b", Some("null")), content_hash("t", "b", None));
    assert_ne!(content_hash("t", "b", Some("42")), content_hash("t", "b", None));
}

#[test]
fn validate_rejects_non_integer_number_payloads() {
    // A float can't be a reliable content-hash input (binary64 collapse) — rejected on write.
    let err = super::validate_payload("Task", Some(r#"{"score":0.85}"#)).unwrap_err();
    assert!(err.to_string().contains("canonically encodable"), "{err}");
}

#[test]
fn an_oversized_payload_json_is_rejected_at_write_validation() {
    // #680: the payload is the only uncapped envelope input, so a create/update with an
    // oversized payload is how an un-authorable row (its signed /3 envelope over the op-log
    // cap) would be minted. Reject it at the write boundary. Build a valid JSON object
    // that still blows the byte cap so the SIZE check — not the object/canonical check
    // — is what fires.
    let big = "x".repeat(super::MAX_MEMORY_PAYLOAD_LEN);
    let payload = format!("{{\"v\":\"{big}\"}}");
    let err = super::validate_payload("Task", Some(&payload)).unwrap_err();
    assert!(
        err.to_string().contains("over the"),
        "the byte cap rejects an oversized payload: {err}"
    );
    // A payload at/under the cap still validates.
    super::validate_payload("Task", Some(r#"{"v":"ok"}"#)).unwrap();
}

#[test]
fn an_oversized_edge_string_is_rejected_at_write_validation() {
    // #680: `target_anchor` / `target_repo_id` are the only otherwise-uncapped edge inputs, so
    // an oversized one is how an un-authorable `EdgeAdd` (its signed /3 envelope over the
    // op-log cap) would be minted. Reject it at the write boundary — the edge twin of
    // the payload cap.
    let big = "x".repeat(super::MAX_EDGE_ANCHOR_LEN + 1);
    let err = super::validate_edge_len("target_anchor", &big).unwrap_err();
    assert!(
        err.to_string().contains("over the"),
        "the byte cap rejects an oversized edge string: {err}"
    );
    // A short identifier (the normal case) still validates.
    super::validate_edge_len("target_anchor", "mem_1700000000000_abcdef").unwrap();
}

#[test]
fn validate_rejects_nfc_duplicate_payload_keys() {
    // A payload with "café" precomposed AND decomposed collapses to one NFC key on write —
    // rejected so `content_hash` never sees an ambiguous dup-key map.
    let dup = "{\"caf\u{00e9}\":1,\"cafe\u{0301}\":2}";
    let err = super::validate_payload("Task", Some(dup)).unwrap_err();
    assert!(err.to_string().contains("canonically encodable"), "{err}");
}

#[test]
fn validate_rejects_literal_duplicate_payload_keys() {
    // serde_json would silently keep the last; reject so the cross-device hash is well-defined.
    let err = super::validate_payload("Task", Some(r#"{"a":1,"a":2}"#)).unwrap_err();
    assert!(err.to_string().contains("duplicate object key"), "{err}");
    // Nested duplicates are caught at any depth.
    let nested = super::validate_payload("Task", Some(r#"{"x":{"a":1,"a":2}}"#)).unwrap_err();
    assert!(nested.to_string().contains("duplicate object key"), "{nested}");
}

#[test]
fn legacy_dup_key_payloads_hash_as_raw_bytes() {
    // A dup-key payload can't be created (validate rejects it), but a legacy / out-of-band one
    // must NOT crash the dream pass. BOTH dup kinds — NFC-normalized and LITERAL — fold the raw
    // bytes: total, deterministic, and PARSER-INDEPENDENT.
    let nfc_dup = "{\"caf\u{00e9}\":1,\"cafe\u{0301}\":2}";
    let literal_dup = r#"{"a":1,"a":2}"#;
    for dup in [nfc_dup, literal_dup] {
        let h = content_hash("t", "b", Some(dup));
        assert_eq!(h, content_hash("t", "b", Some(dup)), "deterministic fallback");
        assert_eq!(h.len(), 64, "still a sha-256 hex digest");
    }
    // A literal-dup must NOT collapse to serde's silent last-wins interpretation.
    assert_ne!(
        content_hash("t", "b", Some(literal_dup)),
        content_hash("t", "b", Some(r#"{"a":2}"#)),
        "raw-bytes fold is distinct from serde last-wins",
    );
}

#[test]
fn golden_vector_pins_the_canonical_rule() {
    // A stored dream freshness hash is content-addressed on this exact encoding — pin it so any
    // change to the §5.5 canonical rule (which would silently re-derive every dream overlay) is
    // caught and the domain tag `rag-rat/content-hash/1` bumped deliberately.
    assert_eq!(
        content_hash("title", "body", Some(r#"{"schema_version":1,"status":"todo"}"#)),
        "5a07a01d8bc81c1dc9a80a2ea8707fc9d3f3bcfac5a6c31761deb3d2be2107b2"
    );
}
