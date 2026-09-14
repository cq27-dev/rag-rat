use super::*;

fn base_parts<'a>(receiver_type_hint: Option<&'a str>) -> EdgeFingerprintParts<'a> {
    EdgeFingerprintParts {
        path: "src/lib.rs",
        start_line: 10,
        end_line: 10,
        from_name: Some("caller"),
        to_name: Some("run"),
        edge_kind: "calls_name",
        target_qualified_name: None,
        receiver_hint: Some("recv"),
        receiver_type_hint,
        callee_logical_symbol_id: None,
    }
}

#[test]
fn receiver_type_hint_repoint_changes_the_stable_fingerprint() {
    // path, span, from_name, to_name, edge_kind, target_qualified_name, and receiver_hint all
    // stay identical — only `receiver_type_hint` differs, as when Rust receiver-type inference
    // re-points `recv.run()` from `Alpha::run` to `Beta::run` on reindex (#567). The
    // fingerprint MUST change, or `edge_by_fingerprint`/`edge_by_id` would keep
    // validating a call-path anchor `current` against a target it no longer resolves
    // to.
    let alpha = edge_fingerprint(base_parts(Some("Alpha")));
    let beta = edge_fingerprint(base_parts(Some("Beta")));
    assert_ne!(alpha, beta, "different receiver_type_hint must yield different fingerprints");
}

#[test]
fn resolved_callee_repoint_changes_the_stable_fingerprint() {
    let unresolved = edge_fingerprint(base_parts(Some("Worker")));
    let alpha = edge_fingerprint(EdgeFingerprintParts {
        callee_logical_symbol_id: Some(11),
        ..base_parts(Some("Worker"))
    });
    let beta = edge_fingerprint(EdgeFingerprintParts {
        callee_logical_symbol_id: Some(22),
        ..base_parts(Some("Worker"))
    });
    assert_ne!(unresolved, alpha, "resolution changes edge identity");
    assert_ne!(alpha, beta, "retargeting with the same receiver hint changes edge identity");
}

#[test]
fn legacy_helper_preserves_the_pre_versioned_format() {
    // Bindings persisted before the version line hold exactly this 8-field byte format —
    // `legacy_edge_fingerprint` must reproduce it, and the versioned format must NEVER
    // collide with it (hint present or not).
    let legacy_format =
        hex_sha256("src/lib.rs\n10\n10\ncaller\nrun\ncalls_name\n\nrecv".as_bytes());
    assert_eq!(legacy_edge_fingerprint(base_parts(None)), legacy_format);
    assert_eq!(legacy_edge_fingerprint(base_parts(Some("Alpha"))), legacy_format);
    assert_ne!(edge_fingerprint(base_parts(None)), legacy_format);
    assert_ne!(edge_fingerprint(base_parts(Some("Alpha"))), legacy_format);
}

#[test]
fn versioned_hintless_binding_is_not_masked_by_a_later_hint_gain() {
    // A binding created AFTER the upgrade on a then-hintless edge stores the versioned
    // hintless value. When the same call span later gains a hint (an untyped binding
    // becomes typed), the stored value must match neither the edge's new versioned
    // fingerprint nor its legacy compatibility shadow — the change is detected, not
    // silently reported current.
    let stored = edge_fingerprint(base_parts(None));
    assert_ne!(stored, edge_fingerprint(base_parts(Some("Alpha"))));
    assert_ne!(stored, legacy_edge_fingerprint(base_parts(Some("Alpha"))));
}
