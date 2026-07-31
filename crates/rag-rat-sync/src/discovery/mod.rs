//! Account-keyed peer discovery over a shared, blind announcement service (#988).
//!
//! Devices of one account publish their node id under a tag only that account can compute, and
//! fetch the same tag to learn where their peers are. The service stores opaque bytes under opaque
//! tags.
//!
//! **What that does and does not hide.** The tag is a pseudonym the service cannot link to an
//! account: it is keyed material, not a digest of the account id, so a party holding the account id
//! — which every host a device has ever dialed does — cannot compute it. What the service DOES see,
//! under that pseudonym, is the node ids advertised beneath a tag, their expiry timestamps, and the
//! refreshes that renew them. It can therefore count how many nodes advertise under one tag and
//! watch when they start and stop renewing. Unlinkability is the guarantee; hiding node count and
//! liveness from the service is NOT, and code or documentation that implies otherwise is wrong.
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

#[cfg(test)]
mod stub;
#[cfg(test)]
mod tests;

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

/// The key material [`account_tag`] is computed under, or `None` when this store has no account
/// (nothing to discover: a restoring-from-zero device holds a ticket with an explicit address).
///
/// A SINGLE SEAM ON PURPOSE. The account's genesis entry hash is used because it is stable forever
/// and present on every enrolled device — unlike the stream key, which rotates and whose sealing
/// selection may fall back to a lower epoch, so two devices could compute different tags at the
/// same moment.
///
/// **The limitation, which bites on an ordinary operation and not only on a leak: REMOVING A DEVICE
/// DOES NOT REVOKE ITS DISCOVERY ACCESS.** The genesis hash is immutable and already sits in that
/// device's database, so a removed device computes this account's tag forever. Roster auth stops it
/// syncing, but nothing stops it enumerating the account's advertised hosts and their liveness, or
/// filling the service's per-tag announcement slots with garbage so no legitimate host can publish.
/// Device removal cannot reach a database it no longer controls. A leaked hash (a log, a debug
/// dump, a pasted diagnostic) has the same permanent effect.
///
/// This is still strictly better than the account id, which is broadcast to every dialed host by
/// design, and it is a bounded exposure — routing advice and denial of service, never data. The fix
/// is purpose-built discovery key material sealed to roster-effective devices and rotated on
/// removal (#1080); routing every caller through this one function is what keeps that change from
/// touching the wire, the tag domain, the client, or the driver.
pub fn discovery_secret(conn: &rusqlite::Connection) -> anyhow::Result<Option<[u8; 32]>> {
    rag_rat_oplog::read_local_account_genesis(conn)
}

/// How long an announcement should live.
///
/// **The only publisher is a serving host** ([`advertise`]); the device-side pass fetches and never
/// publishes. So this is a policy knob, not a cadence: it is scaled off `push_interval_secs` purely
/// because that is the one interval an operator has already tuned, and a host renews on its own
/// schedule ([`TICKS_PER_TTL`]) regardless of it.
///
/// The number that matters for the service's per-tag limit is `ttl / renewal_interval` — how many
/// live copies of ITSELF one host holds, since the service appends rather than replacing and reaps
/// only on expiry. A host renews at half-life, so that is **2 copies per host, whatever the TTL**;
/// the limit is therefore a limit on hosts (roughly four) and no TTL choice moves it.
///
/// What the TTL does change is how long a host that has died lingers in the tag, costing whoever
/// discovers it one failed dial. Shorter is fresher; the floor and ceiling are the service's.
pub fn publish_ttl_seconds(push_interval_secs: u64) -> u32 {
    const MIN_TTL: u64 = 60;
    const MAX_TTL: u64 = 900;
    push_interval_secs.saturating_mul(2).clamp(MIN_TTL, MAX_TTL) as u32
}

/// How often [`advertise`] wakes, as a divisor of the TTL.
///
/// Must be strictly finer than one minus [`RENEWAL_THRESHOLD`]: renewal becomes due once
/// `1 - threshold` of the TTL has elapsed, and the number of ticks between that moment and expiry
/// is how many attempts a host gets. At a quarter-TTL tick and a three-quarter threshold that is
/// three attempts.
const TICKS_PER_TTL: u64 = 4;

/// Renew once less than three quarters of the TTL remains — see [`is_live`] for why this is
/// deliberately not the tick period.
const RENEWAL_THRESHOLD_NUM: i64 = 3;
const RENEWAL_THRESHOLD_DEN: i64 = 4;

/// What became of this node's own announcement during a pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PublishState {
    /// This device does not advertise itself (`[sync] discoverable` is off), or the pass never got
    /// far enough to try.
    #[default]
    NotAttempted,
    /// A live announcement for this node was already present with comfortable time left, so the
    /// pass did not spend a slot on another copy.
    AlreadyLive,
    /// This pass wrote a fresh announcement.
    Published,
    /// The service refused or the write failed. Non-fatal: the fetch half still ran.
    Failed,
}

/// What one discovery pass produced. Never an `Err`: see the module docs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiscoveryOutcome {
    /// Node ids advertised for this account, already filtered to well-formed ones. Includes this
    /// node when it advertises itself — self-exclusion belongs to the composing caller, which
    /// needs the raw set to decide whether its own announcement is still live.
    pub peers: Vec<[u8; 32]>,
    /// What happened to this node's own announcement.
    pub publish: PublishState,
    /// Why the pass produced less than it might have — for logging only. A caller that branches on
    /// this is treating routing advice as authority.
    pub degraded: Option<String>,
}

/// One discovery pass: what to ask, and who is asking.
pub struct DiscoveryExchange<'a> {
    pub endpoint: &'a Endpoint,
    /// The service's dialable address. A PARAMETER, not a constant, so an in-process stub is
    /// testable against the real code path.
    pub service: EndpointAddr,
    pub tag: [u8; TAG_LEN],
    /// `Some(node)` advertises `node`; `None` fetches only — which is what lets a machine behind
    /// NAT find a server without becoming discoverable itself.
    pub publish: Option<[u8; 32]>,
    pub ttl_seconds: u32,
    /// "Now" as a VALUE, not a clock: the exchange reads it exactly once, to judge whether this
    /// node's existing announcement has enough life left to skip republishing.
    pub now_ms: i64,
}

/// Fetch the account's current advertisers and, when asked, make sure this node is among them.
///
/// **Fetch first, then publish.** The service APPENDS and never replaces, so publishing blind every
/// pass accumulates copies of this node until the per-tag cap rejects everyone — see
/// [`publish_ttl_seconds`]. Fetching first lets the pass skip a publish it does not need, and the
/// fetch was going to happen anyway.
///
/// The two halves stay INDEPENDENT in the failure direction: a publish that fails does not discard
/// the peers already fetched, or one device's rate-limiting would blind it to peers it can reach.
/// They travel on separate bi-streams because the service answers exactly one request per stream.
pub async fn exchange(params: DiscoveryExchange<'_>) -> DiscoveryOutcome {
    exchange_inner(&params, tokio::time::Instant::now() + DISCOVERY_TIMEOUT).await
}

/// One shared deadline, applied per phase rather than once around the whole exchange.
///
/// A single outer `timeout` would bound the pass correctly but discard PARTIAL results: a service
/// that answers the fetch and then stalls on the publish stream would cancel the whole future and
/// throw away peers already in hand, so one stuck write would blind the device to every peer it had
/// just learned about. That is exactly the independence the fetch/publish split exists to provide,
/// and a stall is the one failure a `Result` never reports.
///
/// Per-phase deadlines keep the total bounded by [`DISCOVERY_TIMEOUT`] — they share one instant, so
/// three phases cannot add up to three timeouts — while letting each phase's result survive a later
/// phase running out of time.
async fn exchange_inner(
    params: &DiscoveryExchange<'_>,
    deadline: tokio::time::Instant,
) -> DiscoveryOutcome {
    let DiscoveryExchange { endpoint, service, tag, publish, ttl_seconds, now_ms } = params;
    let connecting = endpoint.connect(service.clone(), PEER_DISCOVERY_ALPN);
    let conn = match tokio::time::timeout_at(deadline, connecting).await {
        Ok(Ok(conn)) => conn,
        // Includes an ALPN the service does not know — the shape a client deployed ahead of the
        // service sees. Fail open: the configured peers are unaffected.
        Ok(Err(error)) => {
            return DiscoveryOutcome {
                degraded: Some(format!("discovery service unreachable: {error}")),
                ..Default::default()
            };
        },
        Err(_elapsed) => {
            return DiscoveryOutcome {
                degraded: Some(format!("discovery dial timed out after {DISCOVERY_TIMEOUT:?}")),
                ..Default::default()
            };
        },
    };

    let mut degraded = Vec::new();
    let fetch = DiscoveryRequest::Fetch { tag: *tag };
    let announcements = match tokio::time::timeout_at(deadline, request(&conn, &fetch)).await {
        Ok(Ok(DiscoveryResponse::Fetched { announcements })) => announcements,
        Ok(Ok(other)) => {
            degraded.push(format!("fetch refused: {other:?}"));
            Vec::new()
        },
        Ok(Err(error)) => {
            degraded.push(format!("fetch failed: {error}"));
            Vec::new()
        },
        Err(_elapsed) => {
            degraded.push(format!("fetch timed out after {DISCOVERY_TIMEOUT:?}"));
            Vec::new()
        },
    };

    // Reached even when the fetch failed above — an empty announcement list then reads as "this
    // node is not live", which republishes. Publishing a copy we did not need is far cheaper than
    // silently ceasing to advertise.
    let publish_state = match publish {
        None => PublishState::NotAttempted,
        Some(node) if is_live(&announcements, node, *ttl_seconds, *now_ms) =>
            PublishState::AlreadyLive,
        Some(node) => {
            let publish = DiscoveryRequest::Publish {
                tag: *tag,
                payload: node.to_vec(),
                ttl_seconds: *ttl_seconds,
            };
            // Whatever happens here, the peers fetched above are already in hand and are returned.
            match tokio::time::timeout_at(deadline, request(&conn, &publish)).await {
                Ok(Ok(DiscoveryResponse::Published { .. })) => PublishState::Published,
                Ok(Ok(other)) => {
                    degraded.push(format!("publish refused: {other:?}"));
                    PublishState::Failed
                },
                Ok(Err(error)) => {
                    degraded.push(format!("publish failed: {error}"));
                    PublishState::Failed
                },
                Err(_elapsed) => {
                    degraded.push(format!("publish timed out after {DISCOVERY_TIMEOUT:?}"));
                    PublishState::Failed
                },
            }
        },
    };

    let peers = announcements
        .into_iter()
        // A malformed payload is dropped INDIVIDUALLY. Failing the batch would let one bad entry —
        // which any party that can compute the tag may publish — hide every good one.
        .filter_map(|announcement| <[u8; 32]>::try_from(announcement.payload.as_slice()).ok())
        .take(MAX_ANNOUNCEMENTS)
        .collect();

    DiscoveryOutcome {
        peers,
        publish: publish_state,
        degraded: (!degraded.is_empty()).then(|| degraded.join("; ")),
    }
}

/// A long-running host's standing advertisement — see [`advertise`].
pub struct Advertise {
    /// Owned (clone the caller's) so the loop can be spawned and outlive the call that made it.
    pub endpoint: Endpoint,
    /// The service's dialable address, resolved by the caller for the same reason
    /// [`DiscoveryExchange`] takes one: it is what makes an in-process stub testable.
    pub service: EndpointAddr,
    pub tag: [u8; TAG_LEN],
    /// The node id to advertise — this host's own.
    pub node: [u8; 32],
    pub ttl_seconds: u32,
    /// A clock, not an instant: unlike a single [`exchange`], this loop runs for the host's
    /// lifetime and must read the time afresh on every tick.
    pub now_ms: fn() -> i64,
}

/// Keep `node` advertised under `tag` for as long as this future is polled. Never returns.
///
/// For a host that serves rather than syncs on a cadence. It is the node that most needs
/// announcing — the always-on peer a machine behind NAT is trying to reach — and the one node a
/// maintenance-hook cadence can never announce, because a serving host has no maintenance hook.
/// Without this the only publishers would be the only fetchers.
///
/// Ticks at a QUARTER of the TTL and renews once less than [`RENEWAL_THRESHOLD`] of it remains, so
/// a renewal lands at roughly half-life with the other half still in hand — see those constants for
/// why the margin is the whole point. The first tick fires immediately, so a host is discoverable
/// from startup rather than a fraction of a TTL later. Each tick is a full [`exchange`], so the
/// fetch-before-publish skip applies and a long-running host does not stack copies of itself.
///
/// Publish-only in effect: the fetched peers are discarded, because peers dial a serving host
/// rather than the other way round.
pub async fn advertise(params: Advertise) {
    let Advertise { endpoint, service, tag, node, ttl_seconds, now_ms } = params;
    // `.max(4)` keeps the interval positive even if a future TTL floor drops below 4s; a zero
    // period would make `interval` panic.
    let mut ticks =
        tokio::time::interval(Duration::from_secs(u64::from(ttl_seconds).max(4) / TICKS_PER_TTL));
    loop {
        ticks.tick().await;
        let outcome = exchange(DiscoveryExchange {
            endpoint: &endpoint,
            service: service.clone(),
            tag,
            publish: Some(node),
            ttl_seconds,
            now_ms: now_ms(),
        })
        .await;
        match outcome.degraded {
            // Never fatal: a host that cannot advertise itself still serves every peer that
            // reaches it through a configured `server_peers` entry.
            Some(reason) => tracing::warn!(reason, "advertising this host degraded"),
            None => tracing::debug!(state = ?outcome.publish, "advertised this host"),
        }
    }
}

/// Whether `node` already has an announcement with comfortably more than [`RENEWAL_THRESHOLD`] of
/// its TTL left — i.e. one that does not need renewing yet.
///
/// **The margin is the point, and getting it wrong is silent.** `expires_at_ms` is stamped by the
/// SERVICE when it receives the publish, not by this client when it sends one, so an entry written
/// at tick `t` expires at `t + round_trip + ttl`. If the threshold equalled the tick period, every
/// tick at which renewal was due would find `round_trip` MORE than the threshold remaining, skip,
/// and defer renewal to the following tick — by which point only `round_trip` milliseconds of life
/// are left. A single failed renewal there (a timeout, or the `RateLimited` code the service has)
/// would then leave the host unadvertised for a whole tick period. With a threshold of three
/// quarters against a quarter-TTL tick, renewal happens at half-life and two further ticks remain
/// before expiry, so one failure costs nothing.
///
/// The comparison also mixes clocks — client `now_ms` against service `expires_at_ms` — which is
/// tolerable only because the margin dwarfs any plausible skew. It would not be at zero margin.
fn is_live(
    announcements: &[wire::WireAnnouncement],
    node: &[u8; 32],
    ttl_seconds: u32,
    now_ms: i64,
) -> bool {
    let headroom_ms = i64::from(ttl_seconds) * 1000 * RENEWAL_THRESHOLD_NUM / RENEWAL_THRESHOLD_DEN;
    announcements.iter().any(|announcement| {
        announcement.payload.as_slice() == node.as_slice()
            && announcement.expires_at_ms.saturating_sub(now_ms) > headroom_ms
    })
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
