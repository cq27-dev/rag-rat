//! Configured-peer resolution and node-id spellings.

use super::*;

/// Standard base32, no padding — one of the spellings `EndpointId::from_str` accepts for a
/// node id, alongside the 64-char lowercase hex that `Display` produces.
fn base32_nopad(bytes: &[u8; 32]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(52);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// A configured-peers-only resolve: nothing published, nothing to open.
fn no_announcements(_payload: &[u8]) -> Option<[u8; 32]> {
    None
}

#[tokio::test]
async fn discover_peers_resolves_valid_ids_and_counts_invalid_ones() {
    let valid = node_id_to_string(&node_id_from_secret([7u8; 32])).unwrap();
    let resolved = discover_peers(
        &[valid.clone(), "not-a-node-id".to_string()],
        "https://relay.example",
        None,
        &no_announcements,
    )
    .await;
    assert_eq!(resolved.peers.len(), 1, "the unparseable id is dropped, the valid one resolves");
    assert_eq!(resolved.peers[0].0, valid, "the resolved entry keeps its node-id label");
    assert_eq!(
        resolved.unresolved_configured, 1,
        "the unparseable id is COUNTED, not silently forgotten — the driver seeds its error tally \
         from this and cannot recover it by subtraction once discovery adds peers"
    );
}

/// One node written several ways must be dialed once.
///
/// `EndpointId::from_str` takes 64-char lowercase hex OR standard base32, and uppercases before
/// base32-decoding — so three strings name one node, while `[sync] server_peers` only
/// de-duplicates literally. Comparing display strings would dial this peer three times per pass
/// (each a full multi-ALPN reconcile) and triple-count it in `ok`/`errors`.
#[tokio::test]
async fn discover_peers_dedupes_configured_spellings_of_one_node() {
    let bytes = node_id_from_secret([11u8; 32]);
    let hex = node_id_to_string(&bytes).unwrap();
    let base32_upper = base32_nopad(&bytes);
    let base32_lower = base32_upper.to_ascii_lowercase();
    for spelling in [&hex, &base32_upper, &base32_lower] {
        assert_eq!(
            EndpointId::from_str(spelling).unwrap().as_bytes(),
            &bytes,
            "every spelling under test must really name this node"
        );
    }
    assert_eq!(
        [&hex, &base32_upper, &base32_lower].iter().collect::<HashSet<_>>().len(),
        3,
        "the spellings must be textually distinct or the test proves nothing"
    );

    let configured =
        [hex.clone(), base32_upper.clone(), base32_lower.clone(), base32_upper.clone()];
    let resolved =
        discover_peers(&configured, "https://relay.example", None, &no_announcements).await;
    assert_eq!(resolved.peers.len(), 1, "every spelling names one peer, dialed once");
    assert_eq!(resolved.peers[0].0, hex, "the first spelling configured wins");
    assert_eq!(
        resolved.unresolved_configured, 0,
        "a de-duplicated spelling resolved fine; it is not an error"
    );
}

#[test]
fn node_id_string_round_trips_through_bytes() {
    let bytes = node_id_from_secret([9u8; 32]);
    let text = node_id_to_string(&bytes).unwrap();
    // `peer_addr` parses the same hex form, so the string is a valid dial id, and
    // `peer_addr_from_bytes` reaches the same address from the raw bytes a ticket carries.
    assert!(peer_addr(&text, "https://relay.example").is_ok());
    assert!(peer_addr_from_bytes(&bytes, "https://relay.example").is_ok());
}
