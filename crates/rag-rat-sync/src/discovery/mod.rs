//! Account-keyed peer discovery over a shared, blind announcement service (#988).
//!
//! Devices of one account publish their node id under a tag only that account can compute, and
//! fetch the same tag to learn where their peers are. The service stores opaque bytes under opaque
//! tags.
//!
//! **What that does and does not hide.** The tag is a pseudonym the service cannot link to an
//! account: it is keyed material, not a digest of the account id, so a party holding the account id
//! — which every host a device has ever dialed does — cannot compute it.
//!
//! Announcement payloads are sealed per roster-effective device, so the service reads no node id
//! out of them. **That does not hide node ids from the service**, and believing otherwise is the
//! easy mistake here: every publish and every fetch arrives over an authenticated iroh connection,
//! whose remote id the service can read directly. It therefore learns which node ids publish under
//! a tag and which node ids ask about one — that is, the account's whole active device set — plus
//! expiry timestamps and renewal timing. Sealing is aimed at the OTHER reader, the party who can
//! compute the tag but cannot terminate the connection: a removed device, or anyone the tag leaks
//! to. Withholding node ids from the service itself would take publishing over a throwaway
//! endpoint identity, which this does not do.
//!
//! So: unlinkability of tag to account is the guarantee. Hiding device count, device identity, or
//! liveness FROM THE SERVICE is not, and code or documentation that implies otherwise is wrong.
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

/// Most discovered peers one fetch may contribute.
///
/// **Counts announcements this device could OPEN, not payloads the service returned** — the
/// distinction is the whole safety property, and applying this to raw payloads instead is a
/// silent hole. Anyone who can compute the tag may publish under it, and a device removed from the
/// roster can compute it forever (that is #1081's business, not this constant's). Capping raw
/// payloads would let such a device hide every real advertiser behind sixteen pieces of garbage —
/// far cheaper than filling the service's slots, and invisible, because the cap discards the good
/// entries before anything tries to open them. Bounding what actually resolves means garbage costs
/// an attacker one slot per suppressed peer instead of one payload.
///
/// The service caps its own per-tag storage too, but that cap is an environment-overridable
/// deployment default this client cannot observe, so it is not something to trust. Each admitted
/// peer costs a full dial with two ALPN sessions, which is what makes an unbounded list expensive
/// rather than merely untidy.
pub const MAX_ANNOUNCEMENTS: usize = 16;

/// Most raw payloads carried out of one fetch, before anything is opened.
///
/// Purely resource control — the bound that keeps a hostile response from costing unbounded work —
/// and deliberately looser than [`MAX_ANNOUNCEMENTS`], which is the security-relevant one. Set to
/// the wire decoder's own per-response ceiling so this never silently truncates a well-formed
/// answer; opening a payload is one X25519 operation per roster device, cheap enough that the real
/// protection is the frame cap the decoder already enforces.
pub const MAX_SEALED_ANNOUNCEMENTS: usize = 64;

/// Bytes of sealed announcement the service will accept in one publish.
///
/// A MIRROR of the service's `ANNOUNCEMENT_PAYLOAD_MAX_BYTES`, which this client cannot query, so
/// it can drift: the service is the authority and a publish above its real limit is refused
/// outright. Mirroring it anyway is what turns "this host quietly stopped being discoverable" into
/// a diagnosable message, because the alternative failure is genuinely silent — the refusal looks
/// like every other transient publish error and the host retries it forever.
///
/// It binds at a smaller roster than it looks. An envelope is one version byte plus 80 bytes per
/// roster-effective device, so this permits about **25 recipients**; the service's own frame budget
/// would not bite until several hundred. An account past that ceiling cannot advertise until the
/// envelope is split across announcements — see [`crate::discovery`] docs.
pub const MAX_ANNOUNCEMENT_BYTES: usize = 2048;

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
/// only on expiry. A host renews at half-life ([`RENEW_AFTER_NUM`]), so that is **2 copies per
/// host, whatever the TTL**; against a 32-slot cap the limit is therefore a limit on hosts (roughly
/// sixteen) and no TTL choice moves it.
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
/// Only ticks where a renewal is actually due cost anything — the rest compare two local values and
/// go back to sleep — so this is free to be fine, and being fine is what buys retry attempts. The
/// number of ticks between renewal falling due and the announcement expiring is how many chances a
/// host gets to recover from a timeout or the service's `RateLimited`; at an eighth-TTL tick and a
/// half-TTL renewal that is four.
///
/// It was NOT free while liveness came from a fetch, when every tick was a full round trip. That
/// coupling is gone.
const TICKS_PER_TTL: u64 = 8;

/// Renew once half the TTL has elapsed.
///
/// This fraction is a slot budget, not a comfort margin. The service APPENDS and reaps only on
/// expiry, so a host holds `ttl / renewal_interval` live copies of itself at any moment — two here,
/// whatever the TTL. Against a 32-slot per-tag cap that is roughly sixteen advertisers before hosts
/// start evicting each other. Renewing at a quarter-TTL instead would double the copies and halve
/// the accounts that fit, for one extra retry attempt that a finer tick supplies for free.
const RENEW_AFTER_NUM: u32 = 1;
const RENEW_AFTER_DEN: u32 = 2;

/// What became of this node's own announcement during a pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PublishState {
    /// This device does not advertise itself (`[sync] discoverable` is off), or the pass never got
    /// far enough to try.
    #[default]
    NotAttempted,
    /// The service acknowledged storing the announcement.
    Published,
    /// The announcement is definitely NOT stored, so a caller renewing on a cadence should try
    /// again at once. Two ways to reach here: the service responded and declined (rate-limited, or
    /// any non-`Published` answer), or the request stream never opened, so the frame never left
    /// this host. Both mean nothing landed — distinct from [`Uncertain`], where it might have.
    Refused,
    /// No acknowledgement arrived: the publish frame may have been sent but the stream errored, or
    /// the deadline passed. The announcement **may or may not** be stored — the service stamps
    /// expiry and stores on RECEIPT, before it answers,
    /// so a lost or late response says nothing about whether the write landed. A renewing caller
    /// must treat this as possibly-live and NOT re-append every tick, or a service that stores but
    /// cannot answer would drive one host to fill the whole tag. Non-fatal: the fetch half still
    /// ran.
    Uncertain,
}

/// What one discovery pass produced. Never an `Err`: see the module docs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiscoveryOutcome {
    /// The announcement payloads found under this tag: sealed envelopes, de-duplicated and capped.
    ///
    /// Returned UNOPENED. Opening needs the account roster and this device's key, which live in
    /// the op-log crate behind a connection — and a connection is not `Sync`, so an opener
    /// carried here would travel across an await inside the spawned advertise loop and make
    /// that future unspawnable. The composing caller opens them; the advertise loop discards
    /// them.
    pub announcements: Vec<Vec<u8>>,
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
    /// Ask the service who else is advertising. A serving host sets this `false`: it publishes so
    /// that peers dial IT, and has no use for the list it would pay a response frame to receive.
    pub fetch: bool,
    /// The sealed announcement to publish, or `None` to fetch only — which is what lets a machine
    /// behind NAT find a host without becoming discoverable itself.
    ///
    /// Opaque bytes here on purpose: sealing needs the account roster and the device key, both of
    /// which live in the op-log crate, so this layer carries the result rather than the
    /// ingredients.
    pub publish: Option<&'a [u8]>,
    pub ttl_seconds: u32,
}

/// Fetch the account's current advertisers, publish this node's announcement, or both.
///
/// **Whether to publish is the CALLER's decision, made from its own records** — this function does
/// what it is told. It is deliberately not inferred from the fetch, which was the obvious design
/// and is wrong: the service returns a size-bounded, randomly sampled SUBSET of a tag, so a host's
/// own live announcement is frequently absent from a response. Reading that absence as "not live"
/// makes the host republish, and because the service appends rather than replaces, each spurious
/// copy evicts some other host — a feedback loop that removes real peers from discovery precisely
/// when the tag is busy enough to sample. [`advertise`] keeps the record instead.
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
    let DiscoveryExchange { endpoint, service, tag, fetch, publish, ttl_seconds } = params;
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
    let announcements = if *fetch {
        let request = DiscoveryRequest::Fetch { tag: *tag };
        match tokio::time::timeout_at(deadline, self::request(&conn, &request)).await {
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
        }
    } else {
        Vec::new()
    };

    // Reached whatever the fetch did — the two halves are independent, and a publish the caller
    // asked for is owed regardless of whether anyone answered the other question.
    let publish_state = match publish {
        None => PublishState::NotAttempted,
        Some(envelope) => {
            let publish = DiscoveryRequest::Publish {
                tag: *tag,
                payload: envelope.to_vec(),
                ttl_seconds: *ttl_seconds,
            };
            // Whatever happens here, the peers fetched above are already in hand and are returned.
            match tokio::time::timeout_at(deadline, request(&conn, &publish)).await {
                Ok(Ok(DiscoveryResponse::Published { .. })) => PublishState::Published,
                // A response arrived and it was not `Published`: the service saw the request and
                // declined it, so nothing is stored.
                Ok(Ok(other)) => {
                    degraded.push(format!("publish refused: {other:?}"));
                    PublishState::Refused
                },
                // The bi-stream never opened, so the frame never left this host: nothing is stored,
                // exactly like a refusal, so the next tick should retry rather than assume
                // liveness.
                Ok(Err(RequestError::NotSent(error))) => {
                    degraded.push(format!("publish not sent: {error}"));
                    PublishState::Refused
                },
                // The frame may have reached the service before the stream errored. The service
                // stores on receipt before answering, so the write may already have landed — see
                // [`PublishState::Uncertain`].
                Ok(Err(RequestError::MaybeSent(error))) => {
                    degraded.push(format!("publish failed: {error}"));
                    PublishState::Uncertain
                },
                // The deadline passed. `open_bi` on an established connection resolves without a
                // round trip, so a timeout falls in the write or the wait for a reply — the frame
                // may already have reached the service, so treat it as possibly live.
                Err(_elapsed) => {
                    degraded.push(format!("publish timed out after {DISCOVERY_TIMEOUT:?}"));
                    PublishState::Uncertain
                },
            }
        },
    };

    // De-duplicating by BYTES works, and only because a publisher seals once per roster change and
    // republishes the result verbatim: its copies are byte-identical. Were the envelope re-sealed
    // per publish, each copy would carry a fresh ephemeral and none of this would collapse.
    //
    // Bounded at [`MAX_SEALED_ANNOUNCEMENTS`], NOT at the peer cap. The peer cap belongs to the
    // caller, after opening — see [`MAX_ANNOUNCEMENTS`] for why applying it to unopened payloads
    // hands a suppression primitive to anyone who can compute the tag.
    let mut seen = std::collections::HashSet::new();
    let announcements = announcements
        .into_iter()
        .map(|announcement| announcement.payload)
        .filter(|payload| seen.insert(payload.clone()))
        .take(MAX_SEALED_ANNOUNCEMENTS)
        .collect();

    DiscoveryOutcome {
        announcements,
        publish: publish_state,
        degraded: (!degraded.is_empty()).then(|| degraded.join("; ")),
    }
}

/// Whether a publish outcome means "this announcement may be live", so the advertiser should time
/// renewal from it rather than re-append on the next tick.
///
/// True for both a confirmed publish and an ambiguous one. The ambiguous case is the load-bearing
/// one: the service stores on receipt before it answers, so a lost or timed-out ack does not mean
/// the write failed, and re-appending because it went unacknowledged is how a host whose responses
/// are dropped fills the whole tag (the service appends, never replaces). A `Refused` is the
/// opposite — either the service answered and declined or the frame never left this host, so
/// nothing is stored and the next tick should retry.
///
/// The cost of treating a genuinely-failed `Uncertain` as live is one renewal interval of
/// undiscoverability before the next republish; the cost of the other error is unbounded slot
/// churn. This trades the bounded harm for the unbounded one.
fn records_liveness(state: PublishState) -> bool {
    matches!(state, PublishState::Published | PublishState::Uncertain)
}

/// A long-running host's standing advertisement — see [`advertise`].
pub struct Advertise {
    /// Owned (clone the caller's) so the loop can be spawned and outlive the call that made it.
    pub endpoint: Endpoint,
    /// The service's dialable address, resolved by the caller for the same reason
    /// [`DiscoveryExchange`] takes one: it is what makes an in-process stub testable.
    pub service: EndpointAddr,
    pub tag: [u8; TAG_LEN],
    /// The sealed announcement to publish, re-read on EVERY tick.
    ///
    /// A channel rather than a value because the roster moves under a long-running host: a device
    /// enrolled an hour after it started must become a recipient at the next renewal, not never.
    /// `None` means there is nothing to advertise — no account, or a roster holding only this
    /// device, which has no one to be discovered by.
    pub announcement: tokio::sync::watch::Receiver<Option<Vec<u8>>>,
    pub ttl_seconds: u32,
}

/// Keep `node` advertised under `tag` for as long as this future is polled. Never returns.
///
/// For a host that serves rather than syncs on a cadence. It is the node that most needs
/// announcing — the always-on peer a machine behind NAT is trying to reach — and the one node a
/// maintenance-hook cadence can never announce, because a serving host has no maintenance hook.
/// Without this the only publishers would be the only fetchers.
///
/// Ticks at an eighth of the TTL and republishes once half of it has elapsed, so a renewal lands at
/// half-life with four ticks in hand before expiry — see [`TICKS_PER_TTL`] and [`RENEW_AFTER_NUM`],
/// where the fractions are a slot budget rather than a comfort margin. The first tick fires
/// immediately, so a host is discoverable from startup rather than a fraction of a TTL later.
///
/// **Liveness is this loop's own record, not something read back from the service.** It keeps the
/// envelope it last got accepted and the monotonic instant of that acceptance, and republishes when
/// either the envelope changed (a roster move, which must take effect at the next tick) or half the
/// TTL has passed. Asking the service instead is the trap [`exchange`] documents: fetch responses
/// are a sampled subset, so a live announcement is often missing from one, and believing that
/// stacks copies until hosts evict each other. Keeping the record locally also makes a
/// not-yet-due tick completely free — no dial at all — which is what lets the tick be fine enough
/// to give renewal four attempts.
///
/// A publish that FAILS deliberately leaves the record untouched, so the very next tick retries;
/// only the service accepting one moves it forward.
///
/// Publish-only: [`DiscoveryExchange::fetch`] is off, because peers dial a serving host rather than
/// the other way round.
pub async fn advertise(params: Advertise) {
    let Advertise { endpoint, service, tag, mut announcement, ttl_seconds } = params;
    // Milliseconds, and `.max(1)`, so the period stays positive for any TTL the service's floor
    // could ever permit — `interval` panics on a zero period.
    let period = Duration::from_millis((u64::from(ttl_seconds) * 1000 / TICKS_PER_TTL).max(1));
    let renew_after =
        Duration::from_millis(u64::from(ttl_seconds * RENEW_AFTER_NUM / RENEW_AFTER_DEN) * 1000);
    let mut ticks = tokio::time::interval(period);
    // What the service last accepted from this host, and when. A MONOTONIC instant, not the wall
    // clock and not the service's `expires_at_ms`: this is a pure "how long since we published"
    // question, so it needs neither a clock that can step backwards nor a comparison across two
    // machines' clocks.
    //
    // In memory only, so a restart forgets it — and because a fresh seal is byte-distinct, the
    // restarted host cannot recognise its still-live announcement and appends another. Bounded and
    // self-healing after one restart; a crash loop or rapid redeploy is the case that bites.
    // Persisting or reusing the envelope across restart is #1086.
    let mut published: Option<(Vec<u8>, tokio::time::Instant)> = None;
    loop {
        ticks.tick().await;
        // Re-read per tick, not once at spawn: this is what makes a roster change take effect.
        //
        // Scoped so the borrow guard is dropped before the await below. A `watch::Ref` is not
        // `Send`, and a temporary living to the end of its statement would be held across the
        // suspension point, making this whole future unspawnable.
        let envelope = {
            let borrowed = announcement.borrow_and_update();
            borrowed.clone()
        };
        let Some(envelope) = envelope else {
            tracing::debug!("nothing to advertise yet; skipping this tick");
            continue;
        };
        if let Some((last, accepted_at)) = &published
            && *last == envelope
            && accepted_at.elapsed() < renew_after
        {
            tracing::trace!("this host's announcement is still live; not renewing yet");
            continue;
        }
        // Started BEFORE the request, and read again only if it succeeds. The service stamps expiry
        // when it RECEIVES the publish, so timing from the response would start this clock a round
        // trip late and renew that much nearer expiry than intended — at the ten-second exchange
        // deadline against the sixty-second TTL floor, half the retry margin. Erring early costs a
        // fraction of a renewal interval; erring late costs the margin that exists to absorb a
        // failed renewal.
        let attempted_at = tokio::time::Instant::now();
        let outcome = exchange(DiscoveryExchange {
            endpoint: &endpoint,
            service: service.clone(),
            tag,
            fetch: false,
            publish: Some(&envelope),
            ttl_seconds,
        })
        .await;
        if records_liveness(outcome.publish) {
            published = Some((envelope, attempted_at));
        }
        match outcome.degraded {
            // Never fatal: a host that cannot advertise itself still serves every peer that
            // reaches it through a configured `server_peers` entry.
            Some(reason) => tracing::warn!(reason, "advertising this host degraded"),
            None => tracing::debug!(state = ?outcome.publish, "advertised this host"),
        }
    }
}

/// Where a discovery request failed, so a publish can tell "definitely not stored" from "maybe
/// stored". A fetch does not care and treats both the same.
enum RequestError {
    /// The bi-stream never opened, so nothing reached the service — the write is definitively not
    /// stored and the caller should retry at once rather than assume it might be live.
    NotSent(anyhow::Error),
    /// The request frame may have reached the service before the failure. Since the service stores
    /// on receipt before it answers, the write may already have landed.
    MaybeSent(anyhow::Error),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSent(error) | Self::MaybeSent(error) => write!(f, "{error}"),
        }
    }
}

/// One request on its own bi-stream. The service reads a single frame, answers, and closes, so a
/// second request on the same stream would block until the timeout.
///
/// The error distinguishes a failure BEFORE any bytes were sent (`open_bi`) from one after the
/// frame may have reached the service, so a publish can classify the two differently — see
/// [`PublishState`].
async fn request(
    conn: &iroh::endpoint::Connection,
    req: &DiscoveryRequest,
) -> Result<DiscoveryResponse, RequestError> {
    let (mut send, mut recv) =
        conn.open_bi().await.map_err(|error| RequestError::NotSent(error.into()))?;
    wire::write_frame(&mut send, &req.encode())
        .await
        .map_err(|error| RequestError::MaybeSent(error.into()))?;
    send.finish().map_err(|error| RequestError::MaybeSent(error.into()))?;
    let body =
        wire::read_frame(&mut recv).await.map_err(|error| RequestError::MaybeSent(error.into()))?;
    DiscoveryResponse::decode(&body).map_err(|error| RequestError::MaybeSent(error.into()))
}
