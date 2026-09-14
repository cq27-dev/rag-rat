use super::*;

#[test]
fn device_fingerprint_hex_is_exact_and_canonical() {
    let expected = DeviceFingerprint::from_bytes([0xab; 32]);
    assert_eq!(expected.to_string(), "ab".repeat(32));
    assert_eq!("AB".repeat(32).parse::<DeviceFingerprint>().unwrap(), expected);

    let short = "ab".repeat(31).parse::<DeviceFingerprint>().unwrap_err();
    assert!(short.to_string().contains("exactly 64 hexadecimal characters"));
    let malformed = format!("{}az", "ab".repeat(31));
    let malformed = malformed.parse::<DeviceFingerprint>().unwrap_err();
    assert!(malformed.to_string().contains("non-hexadecimal character at byte 63"));
}

fn hex(bytes: &[u8]) -> String {
    rag_rat_base::hash::hex_lower(bytes)
}

/// Hand-roll a raw CBOR envelope, scoping the encoder so its borrow on the buffer ends before
/// the bytes are returned — the fixture builder for the forward-compat / corruption tests.
fn raw_envelope(write: impl FnOnce(&mut VecEncoder<'_>)) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        write(&mut enc);
    }
    buf
}

fn content() -> NodeContent {
    NodeContent {
        kind: "Invariant".to_string(),
        title: "title".to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        source: "agent".to_string(),
        // Already sorted — encode canonicalizes, so a round-trip yields sorted tags.
        tags: vec!["a".to_string(), "b".to_string()],
        payload: Some(r#"{"schema_version":1}"#.to_string()),
    }
}

fn edge_spec() -> EdgeSpec {
    EdgeSpec {
        source_node_id: NodeId::from("mem_src"),
        relation: EdgeRelation::DependsOn,
        target_repo_id: "repo_t".to_string(),
        target_kind: "node".to_string(),
        target_anchor: "mem_dst".to_string(),
        owner_repo_id: "repo_o".to_string(),
    }
}

fn resolved() -> ResolvedAnchor {
    ResolvedAnchor {
        target_repo_id: "repo_t".to_string(),
        target_node_id: Some("mem_dst".to_string()),
        anchor_status: "current".to_string(),
    }
}

/// A symbol binding and a tracker binding: between them every nullable column is exercised in
/// both states, since neither shape populates the other's columns. Already in identity order —
/// encode canonicalizes, so a round-trip yields the sorted set.
fn anchors() -> Vec<PortableAnchor> {
    vec![
        PortableAnchor {
            binding_kind: "symbol".to_string(),
            binding_id: "crates/x/src/lib.rs::run".to_string(),
            path: Some("crates/x/src/lib.rs".to_string()),
            start_line: Some(10),
            end_line: Some(20),
            commit_hash: Some("c0ffee".to_string()),
            tracker: None,
            project: None,
            item_key: None,
            created_at_ms: 1_700_000_000_000,
            symbol_kind: Some("function".to_string()),
            signature_hash: Some("5ig".to_string()),
            moniker_tool: Some("scip-rust".to_string()),
            moniker_tool_version: Some("0.3".to_string()),
        },
        PortableAnchor {
            binding_kind: "tracker".to_string(),
            binding_id: "github:owner/repo#7".to_string(),
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            tracker: Some("github".to_string()),
            project: Some("owner/repo".to_string()),
            item_key: Some("7".to_string()),
            created_at_ms: 1_700_000_000_001,
            symbol_kind: None,
            signature_hash: None,
            moniker_tool: None,
            moniker_tool_version: None,
        },
    ]
}

/// Write one anchor into a hand-rolled envelope — the fixture builder for the canonical-order
/// rejections, which need wire forms `encode` would never produce.
fn raw_anchor(enc: &mut VecEncoder<'_>, kind: &str, id: &str) {
    enc.array(14).unwrap();
    enc.str(kind).unwrap();
    enc.str(id).unwrap();
    enc.null().unwrap(); // path
    enc.null().unwrap(); // start_line
    enc.null().unwrap(); // end_line
    enc.null().unwrap(); // commit_hash
    enc.null().unwrap(); // tracker
    enc.null().unwrap(); // project
    enc.null().unwrap(); // item_key
    enc.i64(1).unwrap(); // created_at_ms
    enc.null().unwrap(); // symbol_kind
    enc.null().unwrap(); // signature_hash
    enc.null().unwrap(); // moniker_tool
    enc.null().unwrap(); // moniker_tool_version
}

/// One representative op per variant — the golden + round-trip fixtures.
///
/// A new `MemoryOp` variant MUST gain an entry here (and a pinned vector in
/// `golden_vectors_pin_the_op_wire_format`). Three tests read this list, and a variant missing
/// from it is silently exempt from all three: its bytes are unpinned, its decode arm can be
/// dropped without failing anything, and its wire limits go unchecked.
fn every_variant() -> Vec<(&'static str, MemoryOp)> {
    vec![
        ("node_create", MemoryOp::NodeCreate {
            node_id: NodeId::from("mem_1"),
            content: content(),
        }),
        ("node_update", MemoryOp::NodeUpdate {
            node_id: NodeId::from("mem_1"),
            content: content(),
        }),
        ("node_status", MemoryOp::NodeStatus {
            node_id: NodeId::from("mem_1"),
            status: NodeStatus::Obsolete,
        }),
        ("edge_add", MemoryOp::EdgeAdd { edge: edge_spec() }),
        ("edge_remove", MemoryOp::EdgeRemove { edge_key: EdgeKey::from("edgekey_1") }),
        ("rebind", MemoryOp::Rebind { edge_key: EdgeKey::from("edgekey_1"), resolved: resolved() }),
        ("node_anchors", MemoryOp::NodeAnchors {
            node_id: NodeId::from("mem_1"),
            anchors: anchors(),
        }),
        ("node_source_hash", MemoryOp::NodeSourceHash {
            node_id: NodeId::from("mem_1"),
            source_text_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        }),
        ("node_anchor_scopes", MemoryOp::NodeAnchorScopes {
            node_id: NodeId::from("mem_1"),
            scopes: scopes(),
        }),
        ("snapshot", MemoryOp::Snapshot),
    ]
}

/// One scope, for the symbol anchor in [`anchors`]. Already in identity order.
fn scopes() -> Vec<AnchorScope> {
    vec![AnchorScope {
        binding_kind: "symbol".to_string(),
        binding_id: "crates/x/src/lib.rs::run".to_string(),
        scope_hash: "5c0p3".to_string(),
    }]
}

#[test]
fn golden_vectors_pin_the_op_wire_format() {
    // The op wire format is a frozen primitive: a signed envelope, the fold, and (later) the
    // content-addressed identity all build on these exact bytes. Any change to the canonical
    // rule must break this test and force a deliberate `rag-rat/op/1` version bump.
    let got: Vec<(&str, String)> =
        every_variant().iter().map(|(name, op)| (*name, hex(&encode(op)))).collect();
    let want: Vec<(&str, &str)> = vec![
            (
                "node_create",
                "836c7261672d7261742f6f702f316b6e6f64655f63726561746582656d656d5f318769496e76617269616e74657469746c6564626f64796468696768656167656e748261616162747b22736368656d615f76657273696f6e223a317d",
            ),
            (
                "node_update",
                "836c7261672d7261742f6f702f316b6e6f64655f75706461746582656d656d5f318769496e76617269616e74657469746c6564626f64796468696768656167656e748261616162747b22736368656d615f76657273696f6e223a317d",
            ),
            ("node_status", "836c7261672d7261742f6f702f316b6e6f64655f73746174757382656d656d5f31686f62736f6c657465"),
            (
                "edge_add",
                "836c7261672d7261742f6f702f3168656467655f61646486676d656d5f7372636a646570656e64735f6f6e667265706f5f74646e6f6465676d656d5f647374667265706f5f6f",
            ),
            ("edge_remove", "836c7261672d7261742f6f702f316b656467655f72656d6f766569656467656b65795f31"),
            (
                "rebind",
                "836c7261672d7261742f6f702f3166726562696e648269656467656b65795f3183667265706f5f74676d656d5f6473746763757272656e74",
            ),
            (
                "node_anchors",
                "836c7261672d7261742f6f702f316c6e6f64655f616e63686f727382656d656d5f31828e6673796d626f6c78186372617465732f782f7372632f6c69622e72733a3a72756e736372617465732f782f7372632f6c69622e72730a1466633066666565f6f6f61b0000018bcfe568006866756e6374696f6e6335696769736369702d7275737463302e338e67747261636b6572736769746875623a6f776e65722f7265706f2337f6f6f6f6666769746875626a6f776e65722f7265706f61371b0000018bcfe56801f6f6f6f6",
            ),
            (
                "node_source_hash",
                "836c7261672d7261742f6f702f31706e6f64655f736f757263655f6861736882656d656d5f31784065336230633434323938666331633134396166626634633839393666623932343237616534316534363439623933346361343935393931623738353262383535",
            ),
            (
                "node_anchor_scopes",
                "836c7261672d7261742f6f702f31726e6f64655f616e63686f725f73636f70657382656d656d5f3181836673796d626f6c78186372617465732f782f7372632f6c69622e72733a3a72756e653563307033",
            ),
            ("snapshot", "836c7261672d7261742f6f702f3168736e617073686f74f6"),
        ];
    let got_refs: Vec<(&str, &str)> =
        got.iter().map(|(name, bytes)| (*name, bytes.as_str())).collect();
    assert_eq!(got_refs, want);
}

#[test]
fn every_variant_round_trips() {
    for (name, op) in every_variant() {
        let bytes = encode(&op);
        match decode(&bytes).unwrap() {
            DecodedOp::Known(decoded) => {
                assert_eq!(decoded, op, "{name} must round-trip through encode/decode");
            },
            DecodedOp::Unknown { tag, .. } => {
                panic!("{name} decoded as Unknown(tag={tag}), expected Known");
            },
        }
    }
}

/// An anchor set is a SET: the wire bytes must not depend on the order the caller assembled it
/// in, or two devices holding the same bindings would author byte-different ops.
#[test]
fn node_anchors_encodes_in_identity_order_whatever_the_input_order() {
    let sorted = MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors: anchors() };
    let mut reversed_anchors = anchors();
    reversed_anchors.reverse();
    let reversed =
        MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors: reversed_anchors };
    assert_eq!(encode(&sorted), encode(&reversed));
}

/// Two anchors naming ONE row: the op cannot say which wins, so it is rejected outright rather
/// than silently deduped. Encode does not dedup either, so such an op cannot round-trip.
#[test]
fn node_anchors_rejects_a_duplicate_row_identity() {
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("node_anchors").unwrap();
        enc.array(2).unwrap();
        enc.str("mem_1").unwrap();
        enc.array(2).unwrap();
        raw_anchor(enc, "symbol", "same");
        raw_anchor(enc, "symbol", "same");
    });

    let err = decode(&buf).unwrap_err().to_string();
    assert!(err.contains("strictly increasing"), "{err}");
}

/// The round-trip direction the hand-rolled duplicate test cannot reach: an op BUILT with two
/// anchors for one row must not survive its own encode. Pins that `decode`'s strict ordering is
/// what rejects it — the `encode == bytes` check cannot, since the stable sort re-encodes such
/// a payload to the very bytes it came from.
#[test]
fn an_op_carrying_a_duplicate_identity_cannot_round_trip() {
    let mut duplicated = anchors();
    duplicated.push(duplicated[0].clone());
    let op = MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors: duplicated };

    let err = decode(&encode(&op)).unwrap_err().to_string();
    assert!(err.contains("strictly increasing"), "{err}");
}

/// The authoring-side twin: such an op is refused BEFORE it is signed, so it never becomes a
/// permanent entry whose anchors every peer silently drops at projection.
#[test]
fn wire_limits_reject_what_decode_cannot_read_back() {
    let mut duplicated = anchors();
    duplicated.push(duplicated[0].clone());
    assert!(!within_wire_limits(&MemoryOp::NodeAnchors {
        node_id: NodeId::from("mem_1"),
        anchors: duplicated,
    }));

    let over_cap: Vec<PortableAnchor> = (0..=MAX_ANCHORS_PER_OP)
        .map(|index| PortableAnchor {
            binding_id: format!("id_{index:03}"),
            ..anchors()[0].clone()
        })
        .collect();
    assert!(!within_wire_limits(&MemoryOp::NodeAnchors {
        node_id: NodeId::from("mem_1"),
        anchors: over_cap,
    }));

    assert!(within_wire_limits(&MemoryOp::NodeAnchors {
        node_id: NodeId::from("mem_1"),
        anchors: anchors(),
    }));
    assert!(every_variant().iter().all(|(_, op)| within_wire_limits(op)));
}

/// The scope set is bounded and keyed like the anchor set it describes: a duplicated identity
/// and an over-cap count are refused before signing, and `decode` refuses the same bytes.
#[test]
fn anchor_scopes_share_the_anchor_set_s_wire_limits() {
    let scope =
        |index: usize| AnchorScope { binding_id: format!("id_{index:03}"), ..scopes()[0].clone() };
    let duplicated = vec![scope(1), scope(1)];
    let op = MemoryOp::NodeAnchorScopes { node_id: NodeId::from("mem_1"), scopes: duplicated };
    assert!(!within_wire_limits(&op));
    let err = decode(&encode(&op)).unwrap_err().to_string();
    assert!(err.contains("strictly increasing"), "{err}");

    let over_cap: Vec<AnchorScope> = (0..=MAX_ANCHORS_PER_OP).map(scope).collect();
    let op = MemoryOp::NodeAnchorScopes { node_id: NodeId::from("mem_1"), scopes: over_cap };
    assert!(!within_wire_limits(&op));
    let err = decode(&encode(&op)).unwrap_err().to_string();
    assert!(err.contains(&format!("over the {MAX_ANCHORS_PER_OP} limit")), "{err}");

    // A set is a SET: the wire bytes ignore the caller's order, and an empty set is a
    // retraction that round-trips.
    let sorted = MemoryOp::NodeAnchorScopes {
        node_id: NodeId::from("mem_1"),
        scopes: vec![scope(1), scope(2)],
    };
    let reversed = MemoryOp::NodeAnchorScopes {
        node_id: NodeId::from("mem_1"),
        scopes: vec![scope(2), scope(1)],
    };
    assert_eq!(encode(&sorted), encode(&reversed));
    let empty = MemoryOp::NodeAnchorScopes { node_id: NodeId::from("mem_1"), scopes: vec![] };
    assert_eq!(decode(&encode(&empty)).unwrap(), DecodedOp::Known(empty));
}

#[test]
fn node_anchors_rejects_an_unsorted_set() {
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("node_anchors").unwrap();
        enc.array(2).unwrap();
        enc.str("mem_1").unwrap();
        enc.array(2).unwrap();
        raw_anchor(enc, "tracker", "b");
        raw_anchor(enc, "symbol", "a");
    });

    let err = decode(&buf).unwrap_err().to_string();
    assert!(err.contains("strictly increasing"), "{err}");
}

/// The cap is judged from the array HEADER, before a single element is decoded — an
/// attacker-controlled count must not buy work (or an allocation) proportional to itself. The
/// envelope below declares an over-cap count and then carries NOTHING, so only a header-first
/// check can produce the cap error rather than a truncation error.
#[test]
fn node_anchors_rejects_an_over_cap_count_from_the_header() {
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("node_anchors").unwrap();
        enc.array(2).unwrap();
        enc.str("mem_1").unwrap();
        enc.array(MAX_ANCHORS_PER_OP as u64 + 1).unwrap();
    });

    let err = decode(&buf).unwrap_err().to_string();
    assert!(err.contains(&format!("over the {MAX_ANCHORS_PER_OP} limit")), "{err}");
}

/// The off-by-one guard the sibling `row_op` cap carries: a set exactly AT the limit is legal,
/// so the cap can never be read as "fewer than".
#[test]
fn an_anchor_count_at_the_cap_is_not_refused_by_the_cap() {
    let anchors: Vec<PortableAnchor> = (0..MAX_ANCHORS_PER_OP)
        .map(|index| PortableAnchor {
            binding_kind: "symbol".to_string(),
            // Zero-padded so identity order matches numeric order — an unpadded `10` would sort
            // before `9` and trip the strictly-increasing check for reasons unrelated to the
            // cap.
            binding_id: format!("id_{index:03}"),
            path: None,
            start_line: None,
            end_line: None,
            commit_hash: None,
            tracker: None,
            project: None,
            item_key: None,
            created_at_ms: 1,
            symbol_kind: None,
            signature_hash: None,
            moniker_tool: None,
            moniker_tool_version: None,
        })
        .collect();
    let op = MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors };
    assert_eq!(decode(&encode(&op)).unwrap(), DecodedOp::Known(op));
}

/// An anchor set is legitimately empty for an unanchored memory; that must be a valid op, not a
/// degenerate one, so the drain can distinguish "no bindings" from "no snapshot".
#[test]
fn node_anchors_accepts_an_empty_set() {
    let op = MemoryOp::NodeAnchors { node_id: NodeId::from("mem_1"), anchors: Vec::new() };
    let decoded = decode(&encode(&op)).unwrap();
    assert_eq!(decoded, DecodedOp::Known(op));
}

#[test]
fn unknown_op_kind_is_retained_not_projected() {
    // A future op kind: a well-formed `[domain, "future_op", payload]` envelope. It must decode
    // to `Unknown` with the tag + the ORIGINAL bytes retained (re-foldable after an upgrade),
    // never an error and never a silent drop.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("future_op").unwrap();
        enc.u64(42).unwrap();
    });

    match decode(&buf).unwrap() {
        DecodedOp::Unknown { tag, raw } => {
            assert_eq!(tag, "future_op");
            assert_eq!(raw, buf, "the raw bytes are retained verbatim for re-fold");
        },
        DecodedOp::Known(op) => panic!("expected Unknown, got Known({op:?})"),
    }
}

#[test]
fn unknown_relation_token_decodes_to_unknown() {
    // A future edge relation inside an otherwise-valid `edge_add` → the whole op is kept
    // opaque.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("edge_add").unwrap();
        enc.array(6).unwrap();
        enc.str("mem_src").unwrap();
        enc.str("mentors").unwrap(); // not a known EdgeRelation token
        enc.str("repo_t").unwrap();
        enc.str("node").unwrap();
        enc.str("mem_dst").unwrap();
        enc.str("repo_o").unwrap();
    });

    match decode(&buf).unwrap() {
        DecodedOp::Unknown { tag, raw } => {
            assert_eq!(tag, "edge_add");
            assert_eq!(raw, buf);
        },
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn unknown_status_token_decodes_to_unknown() {
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("node_status").unwrap();
        enc.array(2).unwrap();
        enc.str("mem_1").unwrap();
        enc.str("archived").unwrap(); // not a known NodeStatus token
    });

    match decode(&buf).unwrap() {
        DecodedOp::Unknown { tag, .. } => assert_eq!(tag, "node_status"),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn wrong_domain_tag_is_a_hard_error() {
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str("rag-rat/op/2").unwrap();
        enc.str("snapshot").unwrap();
        enc.null().unwrap();
    });
    assert!(decode(&buf).is_err(), "a bumped domain version must not silently decode");
}

#[test]
fn structurally_malformed_bytes_are_a_hard_error() {
    // A known kind whose payload array has the wrong arity is corruption, not forward-compat.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("node_status").unwrap();
        enc.array(1).unwrap(); // node_status wants a 2-element payload
        enc.str("mem_1").unwrap();
    });
    assert!(decode(&buf).is_err());
    // Not even a CBOR array.
    assert!(decode(&[0x00]).is_err());
}

#[test]
fn trailing_bytes_after_an_op_are_rejected() {
    // A complete, valid op followed by extra CBOR is not a canonical envelope — accepting it
    // would make the retained bytes differ from what `encode` produces (wire-identity drift).
    let mut buf = encode(&MemoryOp::Snapshot);
    buf.push(0x00); // a stray trailing CBOR unsigned 0
    assert!(decode(&buf).is_err(), "trailing bytes must be rejected");
}

#[test]
fn truncated_unknown_kind_payload_is_rejected() {
    // A future op kind whose declared payload array is short is corruption, not a retainable
    // opaque op — the skip-and-verify path rejects it.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("future_op").unwrap();
        enc.array(3).unwrap(); // claims three elements...
        enc.str("only_one").unwrap(); // ...supplies one
    });
    assert!(decode(&buf).is_err());
}

#[test]
fn truncated_edge_add_is_rejected_even_with_an_unknown_relation() {
    // The full payload is read before the relation is judged, so a short `edge_add` hard-errors
    // rather than being silently accepted as Unknown.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("edge_add").unwrap();
        enc.array(6).unwrap(); // claims six...
        enc.str("mem_src").unwrap();
        enc.str("mentors").unwrap(); // unknown relation
        enc.str("repo_t").unwrap(); // ...supplies three
    });
    assert!(decode(&buf).is_err());
}

#[test]
fn non_canonical_tag_order_is_rejected() {
    // Tags out of canonical (sorted) order re-encode differently → rejected. Otherwise the same
    // logical op would have two accepted wire representations under one signature.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("node_create").unwrap();
        enc.array(2).unwrap();
        enc.str("mem_1").unwrap();
        enc.array(7).unwrap();
        enc.str("Invariant").unwrap();
        enc.str("title").unwrap();
        enc.str("body").unwrap();
        enc.str("high").unwrap();
        enc.str("agent").unwrap();
        enc.array(2).unwrap();
        enc.str("b").unwrap(); // out of sorted order
        enc.str("a").unwrap();
        enc.null().unwrap(); // payload
    });
    assert!(decode(&buf).is_err(), "unsorted tags are non-canonical");
}

#[test]
fn duplicate_tags_are_deduped_and_rejected_on_the_wire() {
    // Tags are a SET: encode drops duplicates, so a dup and its deduped form encode
    // identically.
    let mut dup = content();
    dup.tags = vec!["a".to_string(), "a".to_string(), "b".to_string()];
    let mut deduped = content();
    deduped.tags = vec!["a".to_string(), "b".to_string()];
    assert_eq!(encode(&node_create(dup)), encode(&node_create(deduped)));
    // And a hand-built dup-tag envelope is non-canonical (re-encode differs) → rejected.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("node_create").unwrap();
        enc.array(2).unwrap();
        enc.str("mem_1").unwrap();
        enc.array(7).unwrap();
        enc.str("Invariant").unwrap();
        enc.str("title").unwrap();
        enc.str("body").unwrap();
        enc.str("high").unwrap();
        enc.str("agent").unwrap();
        enc.array(2).unwrap();
        enc.str("a").unwrap();
        enc.str("a").unwrap(); // duplicate
        enc.null().unwrap();
    });
    assert!(decode(&buf).is_err(), "duplicate tags are non-canonical on the wire");
}

#[test]
fn overlong_length_header_is_rejected() {
    // Splice the canonical snapshot's inline domain-length header (`0x6c`, len 12) into the
    // non-minimal 1-byte-length form (`0x78 0x0c`) — same string, non-canonical CBOR. `encode`
    // only ever emits the minimal header, so `decode` must reject the overlong input.
    let canonical = encode(&MemoryOp::Snapshot);
    assert_eq!(canonical[1], 0x6c, "domain length header is the inline minimal form");
    let mut overlong = vec![canonical[0], 0x78, 0x0c];
    overlong.extend_from_slice(&canonical[2..]);
    assert!(decode(&overlong).is_err(), "an overlong length header is non-canonical");
}

#[test]
fn a_huge_declared_tag_count_does_not_preallocate() {
    // A tiny payload declaring an enormous tag-array length must return a decode error, never
    // OOM or panic — the decoder grows with real bytes, so it errors at the first missing tag.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("node_create").unwrap();
        enc.array(2).unwrap();
        enc.str("mem_1").unwrap();
        enc.array(7).unwrap();
        enc.str("Invariant").unwrap();
        enc.str("title").unwrap();
        enc.str("body").unwrap();
        enc.str("high").unwrap();
        enc.str("agent").unwrap();
        enc.array(u64::MAX).unwrap(); // absurd declared tag count, with no elements following
    });
    assert!(decode(&buf).is_err(), "a bogus tag count must error, not allocate");
}

#[test]
fn non_canonical_unknown_op_bytes_are_rejected() {
    // A future op KIND whose payload uses a non-minimal length header is non-canonical and must
    // be rejected even though the kind is unknown — every RETAINED op is one canonical wire
    // form.
    let mut overlong = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("future_op").unwrap();
    });
    // Payload = text "x" with a non-minimal 1-byte length header (`0x78 0x01`) not inline
    // `0x61`.
    overlong.extend_from_slice(&[0x78, 0x01, b'x']);
    assert!(decode(&overlong).is_err(), "a non-canonical unknown payload is rejected");

    // The canonical (inline) form of the same unknown op IS retained.
    let mut canonical = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("future_op").unwrap();
    });
    canonical.extend_from_slice(&[0x61, b'x']);
    assert!(matches!(decode(&canonical).unwrap(), DecodedOp::Unknown { .. }));
}

#[test]
fn deeply_nested_unknown_op_is_rejected() {
    // Unbounded recursion in the canonical validator would overflow the stack; a pathologically
    // nested payload must return a decode error instead.
    let mut buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("future_op").unwrap();
    });
    // Payload = MAX_CBOR_DEPTH+2 nested single-element arrays (0x81) around a uint-0 leaf.
    buf.extend(std::iter::repeat_n(0x81u8, cbor::MAX_CBOR_DEPTH + 2));
    buf.push(0x00);
    assert!(decode(&buf).is_err(), "excessive CBOR nesting is rejected, not overflowed");
}

#[test]
fn invalid_utf8_text_in_unknown_op_is_rejected() {
    // A future op whose payload TEXT string is not valid UTF-8 (`0x61 0xff`) must be rejected:
    // a later decoder reads it with `d.str()` (UTF-8 required), so it is not
    // re-foldable and can't be retained as "canonical".
    let mut buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("future_op").unwrap();
    });
    buf.extend_from_slice(&[0x61, 0xff]); // text, length 1, content byte 0xff (invalid UTF-8)
    assert!(decode(&buf).is_err(), "invalid UTF-8 text in an unknown op is rejected");
}

#[test]
fn indefinite_length_unknown_op_is_rejected() {
    // An indefinite-length payload (`0x9f … 0xff`) is never canonical CBOR.
    let mut buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("future_op").unwrap();
    });
    buf.extend_from_slice(&[0x9f, 0xff]); // indefinite-length array, immediately closed
    assert!(decode(&buf).is_err());
}

/// Wrap raw `payload` CBOR bytes as the 3rd element of an unknown-KIND op envelope, so the
/// retention path's canonical-CBOR validator is what judges `payload`.
fn unknown_op_with_payload(payload: &[u8]) -> Vec<u8> {
    let mut buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("future_op").unwrap();
    });
    buf.extend_from_slice(payload);
    buf
}

#[test]
fn canonical_validator_accepts_every_canonical_cbor_shape() {
    // One canonical item per CBOR major type / integer width → each retained as Unknown.
    let shapes: &[&[u8]] = &[
        &[0x41, 0x00],                                           // byte string, len 1
        &[0xa1, 0x61, b'a', 0x00],                               // map {"a": 0} (single key)
        &[0xc0, 0x00],                                           // tag(0) wrapping uint 0
        &[0xf5],                                                 // simple value: true
        &[0x19, 0x01, 0x00],                                     // uint 256 (minimal 2-byte)
        &[0x1a, 0x00, 0x01, 0x00, 0x00],                         // uint 65536 (minimal 4-byte)
        &[0x1b, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00], // uint 2^32 (minimal 8-byte)
    ];
    for payload in shapes {
        let bytes = unknown_op_with_payload(payload);
        assert!(
            matches!(decode(&bytes).unwrap(), DecodedOp::Unknown { .. }),
            "canonical payload {payload:02x?} should be retained as Unknown",
        );
    }
}

#[test]
fn canonical_validator_rejects_every_non_canonical_cbor_shape() {
    let shapes: &[&[u8]] = &[
        &[0xa2, 0x61, b'b', 0x00, 0x61, b'a', 0x00], // map keys OUT of order ("b" then "a")
        &[0xa2, 0x61, b'a', 0x00, 0x61, b'a', 0x01], // DUPLICATE map key "a"
        &[0x19, 0x00, 0xff],                         // uint 255 in a non-minimal 2-byte header
        &[0x1a, 0x00, 0x00, 0x00, 0xff],             // uint 255 in a non-minimal 4-byte header
        &[0x1b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff], // …non-minimal 8-byte header
        &[0x62, b'a'],                               /* text claims length 2, only 1 byte
                                                      * present */
    ];
    for payload in shapes {
        let bytes = unknown_op_with_payload(payload);
        assert!(decode(&bytes).is_err(), "non-canonical payload {payload:02x?} should be rejected");
    }
}

#[test]
fn trailing_bytes_after_an_unknown_op_are_rejected() {
    // The Known path rejects trailing bytes via `encode == bytes`; the Unknown path must reject
    // them via the canonical validator's own no-trailing-bytes check.
    let mut buf = unknown_op_with_payload(&[0x00]); // canonical unknown op (payload = uint 0)
    buf.push(0x00); // a stray trailing CBOR byte
    assert!(decode(&buf).is_err(), "trailing bytes after an unknown op are rejected");
}

#[test]
fn a_non_null_snapshot_payload_is_rejected() {
    // `snapshot` is strictly null; a future manifest-carrying snapshot is a NEW kind, not a
    // non-null payload here — an old binary must reject, never misread.
    let buf = raw_envelope(|enc| {
        enc.array(3).unwrap();
        enc.str(DOMAIN).unwrap();
        enc.str("snapshot").unwrap();
        enc.u64(1).unwrap(); // non-null payload
    });
    assert!(decode(&buf).is_err());
}

#[test]
fn node_status_tokens_match_the_validated_memory_status_set() {
    // The op-log status tokens ARE the persisted `repo_memories.status` set — pin them against
    // the write-path validator so the two can never drift, and pin the exact db strings.
    for (status, token) in [
        (NodeStatus::Active, "active"),
        (NodeStatus::Stale, "stale"),
        (NodeStatus::Obsolete, "obsolete"),
        (NodeStatus::Rejected, "rejected"),
    ] {
        assert_eq!(status.as_db_str(), token);
        assert_eq!(NodeStatus::from_db_str(token), Some(status));
        memory::validate_status(token)
            .unwrap_or_else(|_| panic!("`{token}` must be a valid memory status"));
    }
    assert_eq!(NodeStatus::from_db_str("archived"), None);
    assert_eq!(NodeStatus::default(), NodeStatus::Active);
}

#[test]
fn edge_key_matches_the_live_edge_table_derivation() {
    // The op-log derives `edge_key` through the same helper the live table uses, so an add via
    // the op-log and a direct insert content-address to the SAME key.
    let spec = edge_spec();
    let expected = memory::edge_key(
        spec.source_node_id.as_str(),
        spec.relation.as_db_str(),
        &spec.target_kind,
        &spec.target_anchor,
    );
    assert_eq!(spec.edge_key().as_str(), expected);
}

#[test]
fn payload_absent_differs_from_payload_present() {
    // The `null` vs text encoding keeps a no-payload node distinct from one with a payload.
    let mut without = content();
    without.payload = None;
    assert_ne!(encode(&node_create(without)), encode(&node_create(content())));
}

fn node_create(content: NodeContent) -> MemoryOp {
    MemoryOp::NodeCreate { node_id: NodeId::from("mem_1"), content }
}
