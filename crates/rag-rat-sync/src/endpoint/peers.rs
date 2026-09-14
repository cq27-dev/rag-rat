//! The peers one device-side sync pass dials: the configured ones plus those discovered.

use std::collections::HashSet;

use iroh::EndpointAddr;

use super::addr::{peer_addr, peer_addr_from_bytes};

/// The peers one device-side sync pass should dial, plus the configured ids it could not use.
#[derive(Debug, Default, Clone)]
pub struct DiscoveredPeers {
    /// Each dialable address paired with the node-id string naming it, for logging. Configured
    /// peers come first and win on collision, so a pass that discovers nothing behaves exactly as
    /// it did before discovery existed.
    pub peers: Vec<(String, EndpointAddr)>,
    /// Configured ids that did not parse, each already logged.
    ///
    /// Deliberately NOT recoverable as `configured.len() - peers.len()`: discovery can add peers,
    /// and two configured spellings of one node collapse into a single entry without either being
    /// unresolved. A caller that subtracts under-counts its errors and reports a healthy-looking
    /// pass over an all-typo peer list.
    pub unresolved_configured: usize,
}

/// Resolve the peers a device-side sync should dial: the explicitly configured ones, plus whatever
/// the account advertises to the peer-discovery service.
///
/// Configured peers are first-class and unchanged — discovery is purely additive, and `discovery:
/// None` reduces this to the configured resolver. A configured id that does not parse is logged
/// and counted rather than aborting the pass, so one typo cannot suppress every other peer.
///
/// `open_announcement` recovers a node id from a sealed announcement, or `None` for one this
/// device cannot read. It is a parameter rather than something this crate does itself because
/// sealing is the op-log crate's concern; passing a closure that always returns `None` reduces this
/// to the configured-peer resolver.
///
/// **Everything is compared on the raw 32 bytes, never the display string.**
/// `iroh::EndpointId::from_str` accepts 64-char lowercase hex (the `Display` form) OR standard
/// base32, and uppercases before base32-decoding, so three distinct strings can name one peer —
/// while the config layer only trims and de-duplicates literally. Comparing strings would dial such
/// a peer twice per pass (two full multi-ALPN reconciles) and double-count it in `ok`/`errors`.
pub async fn discover_peers(
    configured_peers: &[String],
    relay_url: &str,
    discovery: Option<crate::discovery::DiscoveryExchange<'_>>,
    open_announcement: &dyn Fn(&[u8]) -> Option<[u8; 32]>,
) -> DiscoveredPeers {
    let mut peers: Vec<(String, EndpointAddr)> = Vec::new();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut unresolved_configured = 0usize;

    for peer in configured_peers {
        match peer_addr(peer, relay_url) {
            Ok(addr) =>
                if seen.insert(*addr.id.as_bytes()) {
                    peers.push((peer.clone(), addr));
                } else {
                    tracing::debug!(
                        peer,
                        "skipping a configured sync peer another entry already names"
                    );
                },
            Err(error) => {
                tracing::warn!(peer, %error, "skipping a configured sync peer with an invalid node id");
                unresolved_configured += 1;
            },
        }
    }

    let Some(discovery) = discovery else {
        return DiscoveredPeers { peers, unresolved_configured };
    };
    // Read the local id BEFORE the exchange consumes the params. Self-exclusion is not optional:
    // advertising this node is precisely what puts it in the set it then fetches back.
    let local_node = *discovery.endpoint.id().as_bytes();
    let outcome = crate::discovery::exchange(discovery).await;
    if let Some(degraded) = &outcome.degraded {
        // Never fatal — the configured peers are dialed regardless. See the discovery module docs.
        tracing::warn!(
            degraded,
            "peer discovery degraded; continuing with the peers already resolved"
        );
    }
    // Announcements arrive sealed. Opening happens HERE rather than inside the exchange because it
    // needs a database connection, which is not `Sync` and so cannot cross the await inside the
    // spawned advertise loop that shares that code.
    //
    // Failures are INDIVIDUAL throughout the loop below. Failing the batch would let one bad
    // entry, which anyone able to compute the tag may publish, hide every good one.
    // The peer cap is applied HERE, to announcements that actually resolved, and not in the
    // exchange to raw payloads. Capping payloads would let anyone able to compute the tag suppress
    // every real advertiser with a handful of unopenable entries — see `MAX_ANNOUNCEMENTS`.
    let mut admitted = 0usize;
    for payload in &outcome.announcements {
        if admitted >= crate::discovery::MAX_ANNOUNCEMENTS {
            tracing::debug!(
                cap = crate::discovery::MAX_ANNOUNCEMENTS,
                "discovered the most peers one pass admits; ignoring the rest"
            );
            break;
        }
        // One that will not open is skipped without spending cap budget: it is sealed to a roster
        // this device has left, malformed, or from a newer version.
        let Some(node) = open_announcement(payload) else { continue };
        if node == local_node || !seen.insert(node) {
            continue;
        }
        match peer_addr_from_bytes(&node, relay_url) {
            // The label comes off the parsed id rather than a second fallible conversion.
            Ok(addr) => {
                peers.push((addr.id.to_string(), addr));
                admitted += 1;
            },
            // `from_bytes` rejects a non-canonical point. Dropped individually: anyone who can
            // compute the tag can publish garbage, and one bad entry must not hide the good ones.
            Err(error) =>
                tracing::warn!(%error, "dropping a discovered peer with an unusable node id"),
        }
    }
    DiscoveredPeers { peers, unresolved_configured }
}
