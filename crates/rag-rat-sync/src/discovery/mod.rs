//! Account-keyed peer discovery over a shared, blind announcement service (#988).
//!
//! Devices of one account publish their node id under a tag only that account can compute, and
//! fetch the same tag to learn where their peers are. The service stores opaque bytes under opaque
//! tags — it never learns which account a tag belongs to, how many devices an account has, or when
//! they are online.
//!
//! **Discovery is routing advice, never authority.** A discovered address is dialed exactly like a
//! configured one and still passes the full mutual roster auth before a single byte of inventory is
//! exchanged. A forged announcement therefore cannot inject data; the worst it does is waste a
//! dial. That is why announcements carry no signature — one would gate nothing the auth phase does
//! not already gate.
//!
//! **Every failure here is non-fatal.** Discovery runs inside the per-database session lock, ahead
//! of the peer loop, so anything it does badly it does to every configured peer as well. A service
//! that is unreachable, slow, rate-limiting, or answering with garbage must leave the caller
//! exactly as well off as if discovery did not exist — see [`DiscoveryOutcome`].

pub mod wire;

use std::time::Duration;

use iroh::{Endpoint, EndpointAddr};
use sha2::{Digest, Sha256};

use self::wire::{DiscoveryRequest, DiscoveryResponse, TAG_LEN};

/// The ALPN of the shared peer-discovery service.
///
/// Operator-namespaced rather than named for this project: the service is shared infrastructure
/// that rag-rat CONSUMES and does not own or deploy, and naming a shared service after one of its
/// clients is backwards. Contrast `rag-rat/sync/4` and `rag-rat/content/3`, which name protocols
/// rag-rat defines and serves.
pub const PEER_DISCOVERY_ALPN: &[u8] = b"dev.cq27.peer-discovery/1";

/// The domain string folded into every tag.
///
/// DELIBERATELY not the ALPN, and not derived from it. The service keeps ONE tag namespace across
/// every ALPN it serves, so this constant is the only thing separating this project's tags from
/// another consumer's — and if it were the ALPN string, renaming the ALPN would silently move every
/// tag and partition old clients from new ones with no error anywhere. The two are independent on
/// purpose; changing this one is a hard protocol break.
pub const DISCOVERY_TAG_DOMAIN: &[u8] = b"rag-rat/discovery-tag/1";

/// How long the whole discovery exchange may take before the caller gives up on it.
///
/// Bounded for the same reason every other dial on this path is: a device-side sync holds the
/// per-database session lock while it runs, so an unbounded wait here stalls every local writer,
/// not just this pass. Deliberately tighter than the peer dial budget — discovery is an
/// optimisation, and spending a peer's worth of patience on it before dialing any peer is the wrong
/// trade.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Most announcements accepted from one fetch.
///
/// The service caps this too, but its cap is an environment-overridable deployment default that
/// this client cannot observe — so it is not something to trust. Each accepted announcement costs a
/// full dial with two ALPN sessions, which is what makes an unbounded list expensive rather than
/// merely untidy.
pub const MAX_ANNOUNCEMENTS: usize = 16;

/// The tag an account publishes and fetches under.
///
/// Computable only from `secret`, which only roster-effective devices hold. It is deliberately NOT
/// derived from the account id: the account id is sent as the dialer's FIRST frame, before the peer
/// has proved anything, so every host a device has ever dialed — including a decommissioned or
/// hostile one — holds it permanently. A tag derived from it would let any of them enumerate the
/// account's devices and watch when they come online, which is exactly the observability this
/// design exists to withhold.
pub fn account_tag(secret: &[u8; 32]) -> [u8; TAG_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(DISCOVERY_TAG_DOMAIN);
    hasher.update(secret);
    hasher.finalize().into()
}

/// What one discovery pass produced. Never an `Err`: see the module docs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiscoveryOutcome {
    /// Node ids advertised for this account, already filtered to well-formed ones.
    pub peers: Vec<[u8; 32]>,
    /// Whether the publish half succeeded, when one was attempted.
    pub published: bool,
    /// Why the pass produced less than it might have — for logging only. A caller that branches on
    /// this is treating routing advice as authority.
    pub degraded: Option<String>,
}

/// Publish `local_node` under this account's tag and fetch the account's current advertisers.
///
/// Publish and fetch are INDEPENDENT: a publish failure must not suppress the fetch, or one
/// device's rate-limiting would blind it to peers it could otherwise reach. They travel on separate
/// bi-streams because the service answers exactly one request per stream.
///
/// `publish` is opt-in; a device that does not advertise itself still learns about others, which is
/// what lets a machine behind NAT reach a server without becoming discoverable itself.
pub async fn exchange(
    endpoint: &Endpoint,
    service: EndpointAddr,
    tag: [u8; TAG_LEN],
    local_node: Option<[u8; 32]>,
    ttl_seconds: u32,
) -> DiscoveryOutcome {
    match tokio::time::timeout(
        DISCOVERY_TIMEOUT,
        exchange_inner(endpoint, service, tag, local_node, ttl_seconds),
    )
    .await
    {
        Ok(outcome) => outcome,
        // The bound exists precisely for a service that accepts and then stalls, which no transport
        // error would ever surface.
        Err(_elapsed) => DiscoveryOutcome {
            degraded: Some(format!("discovery timed out after {DISCOVERY_TIMEOUT:?}")),
            ..Default::default()
        },
    }
}

async fn exchange_inner(
    endpoint: &Endpoint,
    service: EndpointAddr,
    tag: [u8; TAG_LEN],
    local_node: Option<[u8; 32]>,
    ttl_seconds: u32,
) -> DiscoveryOutcome {
    let conn = match endpoint.connect(service, PEER_DISCOVERY_ALPN).await {
        Ok(conn) => conn,
        // Includes an ALPN the service does not know — the shape a client deployed ahead of the
        // service sees. Fail open: the configured peers are unaffected.
        Err(error) => {
            return DiscoveryOutcome {
                degraded: Some(format!("discovery service unreachable: {error}")),
                ..Default::default()
            };
        },
    };

    let mut degraded = Vec::new();
    let mut published = false;
    if let Some(node) = local_node {
        match request(&conn, &DiscoveryRequest::Publish {
            tag,
            payload: node.to_vec(),
            ttl_seconds,
        })
        .await
        {
            Ok(DiscoveryResponse::Published { .. }) => published = true,
            Ok(other) => degraded.push(format!("publish refused: {other:?}")),
            Err(error) => degraded.push(format!("publish failed: {error}")),
        }
    }

    // Reached even when the publish failed above — the two halves are independent.
    let peers = match request(&conn, &DiscoveryRequest::Fetch { tag }).await {
        Ok(DiscoveryResponse::Fetched { announcements }) => announcements
            .into_iter()
            .take(MAX_ANNOUNCEMENTS)
            // A malformed payload is dropped INDIVIDUALLY. Failing the batch would let one bad
            // entry — which any party that can compute the tag may publish — hide every good one.
            .filter_map(|announcement| <[u8; 32]>::try_from(announcement.payload.as_slice()).ok())
            .collect(),
        Ok(other) => {
            degraded.push(format!("fetch refused: {other:?}"));
            Vec::new()
        },
        Err(error) => {
            degraded.push(format!("fetch failed: {error}"));
            Vec::new()
        },
    };

    DiscoveryOutcome {
        peers,
        published,
        degraded: (!degraded.is_empty()).then(|| degraded.join("; ")),
    }
}

/// One request on its own bi-stream. The service reads a single frame, answers, and closes, so a
/// second request on the same stream would block until the timeout.
async fn request(
    conn: &iroh::endpoint::Connection,
    req: &DiscoveryRequest,
) -> anyhow::Result<DiscoveryResponse> {
    let (mut send, mut recv) = conn.open_bi().await?;
    wire::write_frame(&mut send, &req.encode()).await?;
    send.finish()?;
    let body = wire::read_frame(&mut recv).await?;
    Ok(DiscoveryResponse::decode(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag must depend on the secret, or every account shares one tag and discovery becomes a
    /// global address book.
    #[test]
    fn the_tag_is_keyed_by_the_secret() {
        assert_ne!(account_tag(&[1; 32]), account_tag(&[2; 32]));
    }

    /// Pinned: the derivation IS the contract between two devices of one account. If it moves,
    /// peers stop finding each other and nothing reports an error — they simply see an empty
    /// account.
    #[test]
    fn the_tag_derivation_is_pinned() {
        let tag = account_tag(&[0xab; 32]);
        let hex: String = tag.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "27da05bee4ecf160b800de8f223835cda93e73d3d883b578966a5f82229fcaaf");
    }

    /// The domain must be textually independent of the ALPN, so renaming the ALPN cannot silently
    /// re-partition the tag space. This is a source-level guard because the failure it prevents is
    /// invisible at runtime.
    #[test]
    fn the_tag_domain_is_not_the_alpn() {
        assert_ne!(DISCOVERY_TAG_DOMAIN, PEER_DISCOVERY_ALPN);
        assert_eq!(DISCOVERY_TAG_DOMAIN, b"rag-rat/discovery-tag/1");
        assert_eq!(PEER_DISCOVERY_ALPN, b"dev.cq27.peer-discovery/1");
    }
}
