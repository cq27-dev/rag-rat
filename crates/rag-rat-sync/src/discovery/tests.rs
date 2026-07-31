use std::time::{Duration, Instant};

use iroh::endpoint::presets;
use iroh::{Endpoint, RelayMode};

use super::stub::{Behaviour, PER_TAG_CAP, StubService};
use super::{
    DISCOVERY_TAG_DOMAIN, DISCOVERY_TIMEOUT, DiscoveryExchange, DiscoveryOutcome,
    PEER_DISCOVERY_ALPN, PublishState, account_tag, exchange, publish_ttl_seconds,
};

/// A fixed instant: the announcement arithmetic under test is about durations, and a real clock
/// would make the near-expiry boundary flaky rather than exact.
const NOW_MS: i64 = 1_700_000_000_000;

/// A client endpoint with no relay — it reaches the stub by direct address, so nothing leaves the
/// machine and the tests do not depend on a relay being up.
async fn client() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .expect("bind a client endpoint")
}

fn local_node(endpoint: &Endpoint) -> [u8; 32] {
    *endpoint.id().as_bytes()
}

/// These tests publish a bare node id as the announcement payload and read it straight back.
///
/// Real announcements are sealed envelopes, but sealing is the op-log crate's concern and is
/// covered there; what this module tests is the transport — publish, fetch, renew, bound, fail
/// open — and it should not have to build a roster to do so. The opener is the seam that lets the
/// two be tested separately.
fn as_node_id(payload: &[u8]) -> Option<[u8; 32]> {
    <[u8; 32]>::try_from(payload).ok()
}

/// The node ids in an outcome, as the composing caller would recover them.
fn opened(outcome: &DiscoveryOutcome) -> Vec<[u8; 32]> {
    outcome.announcements.iter().filter_map(|payload| as_node_id(payload)).collect()
}

/// Poll `ready` until it holds, up to a few seconds. Returns whether it ever did.
///
/// Used instead of sleeping a fixed interval: a fixed sleep is either flaky (too short) or slow
/// (too long), and both are worse than asking the question repeatedly.
async fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

// ---------------------------------------------------------------- tag derivation

/// The tag must depend on the secret, or every account shares one tag and discovery becomes a
/// global address book.
#[test]
fn the_tag_is_keyed_by_the_secret() {
    assert_ne!(account_tag(&[1; 32]), account_tag(&[2; 32]));
}

/// Pinned: the derivation IS the contract between two devices of one account. If it moves, peers
/// stop finding each other and nothing reports an error — they simply see an empty account.
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

// ---------------------------------------------------------------- TTL arithmetic

/// Two cadences per announcement, clamped to what the service accepts.
///
/// The number that matters is `ceil(ttl / cadence)` — how many copies of ITSELF one device holds —
/// because the service appends and never replaces, and rejects once a tag holds 8. At the service
/// maximum on the default cadence that is 3 copies, which is what broke a three-device account.
#[test]
fn the_published_ttl_is_two_cadences_clamped_to_what_the_service_accepts() {
    assert_eq!(
        publish_ttl_seconds(300),
        600,
        "the default cadence yields 2 live copies per device"
    );
    assert_eq!(publish_ttl_seconds(0), 60, "clamped up to the service minimum");
    assert_eq!(publish_ttl_seconds(30), 60, "clamped up to the service minimum");
    assert_eq!(publish_ttl_seconds(600), 900, "clamped down to the service maximum");
    assert_eq!(publish_ttl_seconds(u64::MAX), 900, "no overflow, still clamped");
    for cadence in [60u64, 120, 300, 450] {
        let copies = publish_ttl_seconds(cadence).div_ceil(cadence as u32);
        assert_eq!(copies, 2, "cadence {cadence}s must hold 2 copies, not more");
    }
}

// ---------------------------------------------------------------- the exchange

/// The base case: what one device publishes, another fetches.
#[tokio::test]
async fn a_published_node_id_is_fetched_back_by_another_device() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[3; 32]);

    let publisher = client().await;
    let published = local_node(&publisher);
    let out = exchange(DiscoveryExchange {
        endpoint: &publisher,
        service: service.addr(),
        tag,
        publish: Some(&published),
        ttl_seconds: 600,
        fetch: true,
    })
    .await;
    assert_eq!(out.publish, PublishState::Published, "degraded: {:?}", out.degraded);

    let fetcher = client().await;
    let out = exchange(DiscoveryExchange {
        endpoint: &fetcher,
        service: service.addr(),
        tag,
        publish: None,
        ttl_seconds: 600,
        fetch: true,
    })
    .await;
    assert_eq!(opened(&out), vec![published], "the fetcher sees the publisher");
    assert_eq!(
        out.publish,
        PublishState::NotAttempted,
        "a device that has not opted in must publish nothing"
    );
    assert_eq!(service.stored(&tag).len(), 1, "and must leave no trace on the service");
}

/// Fetching is not gated on publishing. This is what lets a machine behind NAT find an always-on
/// host without becoming discoverable itself.
#[tokio::test]
async fn a_device_that_does_not_advertise_itself_still_learns_its_peers() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[4; 32]);
    let peer = [0x5a; 32];
    service.seed(tag, peer.to_vec(), NOW_MS + 600_000);

    let endpoint = client().await;
    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        publish: None,
        ttl_seconds: 600,
        fetch: true,
    })
    .await;
    assert_eq!(opened(&out), vec![peer]);
    assert_eq!(service.stored(&tag).len(), 1, "nothing of ours was added");
}

/// A publish that fails must not discard peers already fetched — otherwise one device's
/// rate-limiting blinds it to peers it can reach perfectly well.
#[tokio::test]
async fn a_refused_publish_does_not_suppress_the_fetch() {
    let service = StubService::start(NOW_MS, Behaviour::RefusePublish).await;
    let tag = account_tag(&[5; 32]);
    let peer = [0x6b; 32];
    service.seed(tag, peer.to_vec(), NOW_MS + 600_000);

    let endpoint = client().await;
    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        publish: Some(&local_node(&endpoint)),
        ttl_seconds: 600,
        fetch: true,
    })
    .await;
    assert_eq!(out.publish, PublishState::Refused, "the service answered and declined");
    assert_eq!(opened(&out), vec![peer], "the fetched peers survive the publish failure");
    assert!(out.degraded.is_some(), "and the failure is reported for logging");
}

/// A publish that STALLS must not discard the peers already fetched.
///
/// The failure mode a refusal cannot stand in for: a refusal returns promptly and leaves the
/// fetched peers untouched by construction, whereas a stall has to be cut off by a deadline — and a
/// single timeout wrapped around the whole exchange cancels the future that is already holding
/// those peers, turning "the publish is stuck" into "there are no peers". A device would then skip
/// every discovered sync target for the pass because of a write it did not need.
#[tokio::test]
async fn a_stalled_publish_does_not_discard_the_peers_already_fetched() {
    let service = StubService::start(NOW_MS, Behaviour::StallPublish).await;
    let tag = account_tag(&[18; 32]);
    let peer = crate::node_id_from_secret([0x51; 32]);
    service.seed(tag, peer.to_vec(), NOW_MS + 600_000);

    let endpoint = client().await;
    let started = Instant::now();
    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        publish: Some(&local_node(&endpoint)),
        ttl_seconds: 600,
        fetch: true,
    })
    .await;

    assert_eq!(opened(&out), vec![peer], "the fetched peer survives the stalled publish");
    assert_eq!(
        out.publish,
        PublishState::Uncertain,
        "a timed-out publish is ambiguous, not refused"
    );
    assert!(out.degraded.is_some_and(|d| d.contains("timed out")), "and the stall is reported");
    assert!(
        started.elapsed() >= DISCOVERY_TIMEOUT,
        "the deadline is what ends the stall, so it must actually be reached"
    );
    assert!(
        started.elapsed() < DISCOVERY_TIMEOUT * 2,
        "the phases SHARE one deadline; they must not each get a full timeout"
    );
}

/// One unusable payload must not hide the good ones. Anyone who can compute the tag can publish
/// garbage, so failing the batch would hand them a cheap way to blind the whole account.
#[tokio::test]
async fn a_malformed_announcement_is_dropped_individually() {
    let service = StubService::start(NOW_MS, Behaviour::GarbageAmongTheGood).await;
    let tag = account_tag(&[6; 32]);
    let good = [0x7c; 32];
    service.seed(tag, good.to_vec(), NOW_MS + 600_000);

    let endpoint = client().await;
    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        publish: None,
        ttl_seconds: 600,
        fetch: true,
    })
    .await;
    assert_eq!(opened(&out), vec![good], "the well-formed peer survives its malformed neighbour");
}

/// The timeout's whole reason for existing.
///
/// A service that ACCEPTS and then stalls produces no transport error at all, so nothing but the
/// bound recovers from it — and discovery runs inside the per-database sync session lock, so an
/// unbounded wait there stalls every local writer, not merely this pass. Note the stub must be a
/// black hole rather than an unreachable address: an unreachable address fails fast, and a test
/// using one stays green with the timeout deleted.
#[tokio::test]
async fn a_service_that_accepts_and_never_answers_does_not_stall_the_pass() {
    let service = StubService::start(NOW_MS, Behaviour::BlackHole).await;
    let endpoint = client().await;

    let started = Instant::now();
    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag: account_tag(&[7; 32]),
        publish: Some(&local_node(&endpoint)),
        ttl_seconds: 600,
        fetch: true,
    })
    .await;
    let elapsed = started.elapsed();

    assert!(opened(&out).is_empty(), "a stalled service yields no peers");
    assert!(out.degraded.is_some_and(|d| d.contains("timed out")), "and says why");
    assert!(
        elapsed >= DISCOVERY_TIMEOUT,
        "the bound must actually be reached, not short-circuited: {elapsed:?}"
    );
    assert!(
        elapsed < DISCOVERY_TIMEOUT + Duration::from_secs(5),
        "and must not run appreciably past it: {elapsed:?}"
    );
}

/// An unreachable service is the OTHER failure, and it is fail-open rather than timeout-bounded:
/// the dial errors and the caller proceeds with its configured peers, having lost nothing.
///
/// The address here names a node with no relay and no direct address — nothing to dial — so the
/// error arrives promptly instead of by exhausting the bound. That is deliberately a DIFFERENT
/// path from `a_service_that_accepts_and_never_answers_does_not_stall_the_pass`: a dial that fails
/// is self-announcing, while one that succeeds and then stalls is not, and only the latter needs
/// the timeout. Conflating them is how a timeout test ends up proving nothing.
#[tokio::test]
async fn an_unreachable_service_fails_open() {
    let nowhere = iroh::EndpointAddr::new(
        iroh::EndpointId::from_bytes(&local_node(&client().await)).unwrap(),
    );

    let endpoint = client().await;
    let started = Instant::now();
    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: nowhere,
        tag: account_tag(&[8; 32]),
        publish: None,
        ttl_seconds: 600,
        fetch: true,
    })
    .await;
    assert!(opened(&out).is_empty());
    assert!(out.degraded.is_some(), "the failure is reported, never propagated as an error");
    assert!(
        started.elapsed() < DISCOVERY_TIMEOUT,
        "an undialable address must fail on its own, not by hitting the bound"
    );
}

// ---------------------------------------------------------------- cap exhaustion

/// Steady state on the default cadence: three devices, six passes, no publish ever refused.
///
/// The original design — publish at the service maximum every pass, never skipping — gives each
/// device `ceil(900/300) = 3` live copies, so three devices need 9 slots against a cap of 8, the
/// ninth publish is REJECTED (the service never evicts to make room), and an ordinary three-device
/// account breaks its own discovery on default configuration.
///
/// Honest about what this test can and cannot kill. It exercises the TTL clamp and the eviction
/// policy; it cannot exercise the renewal skip, which is no longer part of an exchange at all —
/// `advertise` owns it, and its killing case is
/// `a_running_host_publishes_once_per_renewal_interval_however_often_it_ticks`. The clamp's
/// arithmetic is pinned by `the_published_ttl_is_two_cadences_clamped_to_what_the_service_accepts`.
#[tokio::test]
async fn a_three_device_account_on_the_default_cadence_stays_under_the_per_tag_cap() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[11; 32]);
    let ttl = publish_ttl_seconds(300);

    let devices = [client().await, client().await, client().await];
    let ids: Vec<[u8; 32]> = devices.iter().map(local_node).collect();

    // Six passes at the default cadence, each device publishing once — the cadence a host on the
    // half-life renewal actually produces. Long enough for entries to accumulate and for the first
    // ones to age out, which is where a leak would show.
    for pass in 0..6i64 {
        let pass_now_ms = NOW_MS + pass * 300_000;
        // Advance the SERVICE's clock before the pass, not after: the stub reaps on publish, so a
        // service still living in the previous cadence never expires anything and the tag fills up
        // for reasons that have nothing to do with the client's arithmetic.
        service.advance_to(pass_now_ms);
        for endpoint in &devices {
            let out = exchange(DiscoveryExchange {
                endpoint,
                service: service.addr(),
                tag,
                fetch: true,
                publish: Some(&local_node(endpoint)),
                ttl_seconds: ttl,
            })
            .await;
            assert_eq!(
                out.publish,
                PublishState::Published,
                "pass {pass}: a publish did not succeed — the tag filled up ({:?})",
                out.degraded
            );
        }
        let live = service.stored(&tag).len();
        assert!(
            live <= PER_TAG_CAP,
            "pass {pass}: {live} live announcements exceeds {PER_TAG_CAP}"
        );
        assert!(
            live <= 2 * devices.len(),
            "pass {pass}: {live} live is more than two copies per device"
        );
    }

    // Every device is still discoverable at the end.
    let observer = client().await;
    let out = exchange(DiscoveryExchange {
        endpoint: &observer,
        service: service.addr(),
        tag,
        fetch: true,
        publish: None,
        ttl_seconds: ttl,
    })
    .await;
    for id in &ids {
        assert!(opened(&out).contains(id), "a device stopped being discoverable");
    }
}

/// A caller that asks for a publish gets one — the exchange does not second-guess it by fetching.
///
/// The removed design decided liveness here, from the fetch, and that is the bug this pins shut:
/// the service answers with a size-bounded random SAMPLE, so a host's own live announcement is
/// often missing from a response, and reading that absence as "not live" republishes. Because the
/// service appends rather than replaces, every spurious copy evicts another host — the tag degrades
/// fastest exactly when it is busy enough to be sampled.
#[tokio::test]
async fn a_publish_is_not_conditioned_on_finding_ourselves_in_the_fetch() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[9; 32]);
    let endpoint = client().await;
    let node = local_node(&endpoint);
    // Already live with almost all of a 600s TTL left. Under the removed rule this was the
    // canonical "skip it" case; the decision is no longer the exchange's to make.
    service.seed(tag, node.to_vec(), NOW_MS + 590_000);

    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        fetch: true,
        publish: Some(&node),
        ttl_seconds: 600,
    })
    .await;
    assert_eq!(out.publish, PublishState::Published);
    assert_eq!(service.stored(&tag).len(), 2, "the publish the caller asked for was made");
}

/// `fetch: false` means no fetch request reaches the service at all.
///
/// Not a micro-optimisation. A serving host publishes so that peers dial IT and has no use for the
/// list, and paying for a response frame per renewal is the smaller half; the larger half is that
/// a fetch here invites exactly the inference the test above forbids. Asserted on the REQUEST
/// COUNT, because an ignored response is indistinguishable from an unsent request in every
/// assertion about stored announcements.
#[tokio::test]
async fn a_publish_only_exchange_never_asks_the_service_for_peers() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[10; 32]);
    let endpoint = client().await;
    let node = local_node(&endpoint);

    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        fetch: false,
        publish: Some(&node),
        ttl_seconds: 600,
    })
    .await;
    assert_eq!(out.publish, PublishState::Published);
    assert!(out.announcements.is_empty(), "nothing was asked for, so nothing came back");
    assert_eq!(service.fetches(), 0, "a publish-only exchange fetched anyway");
}

/// A large account is never refused, and its fetches still fit a frame.
///
/// This replaces a test that asserted the opposite. The service used to refuse a publish into a
/// full tag, so an account past roughly four advertisers lost discovery outright; it now evicts the
/// oldest entry instead, and bounds what a fetch returns. The observable flips: publishing always
/// succeeds, and it is the RESPONSE that is limited.
///
/// The payload size here is deliberate — a sealed envelope for a sixteen-device roster, about 1,281
/// bytes. Thirty-two of those is roughly 78 KiB once the wire's integer-array encoding is counted,
/// comfortably past the 64 KiB frame, so this exercises the bound rather than passing under it.
#[tokio::test]
async fn a_large_account_is_never_refused_and_its_fetches_fit_one_frame() {
    use super::wire::{DiscoveryResponse, MAX_FRAME_LEN, WireAnnouncement};

    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[13; 32]);
    let realistic_envelope = |seed: u8| vec![seed.wrapping_add(100); 1 + 16 * 80];

    let endpoint = client().await;
    for seed in 0..32u8 {
        let out = exchange(DiscoveryExchange {
            endpoint: &endpoint,
            service: service.addr(),
            tag,
            publish: Some(&realistic_envelope(seed)),
            ttl_seconds: 600,
            fetch: true,
        })
        .await;
        assert_eq!(
            out.publish,
            PublishState::Published,
            "publish {seed} was refused; a full tag must evict, not refuse ({:?})",
            out.degraded
        );
    }

    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        publish: None,
        ttl_seconds: 600,
        fetch: true,
    })
    .await;
    assert!(!out.announcements.is_empty(), "an over-full tag must still answer");
    assert!(
        out.announcements.len() < 32,
        "and must answer with a SUBSET, not everything: {}",
        out.announcements.len()
    );

    // The returned set must be one the service could actually have sent.
    let response = DiscoveryResponse::Fetched {
        announcements: out
            .announcements
            .iter()
            .map(|payload| WireAnnouncement { payload: payload.clone(), expires_at_ms: NOW_MS })
            .collect(),
    };
    let encoded = response.encode().len();
    assert!(encoded <= MAX_FRAME_LEN, "response of {encoded} bytes exceeds the frame cap");
}

// ---------------------------------------------------------------- composition

/// `discover_peers` composes configured and discovered peers, and must exclude THIS node.
///
/// Self-exclusion is not a nicety: advertising ourselves is exactly what puts us in the set we then
/// fetch back, so without it every discoverable device dials itself once per pass — a full
/// two-ALPN reconcile against its own endpoint — and counts the result in `ok`/`errors`.
#[tokio::test]
async fn discovered_peers_join_the_configured_ones_without_this_node_or_duplicates() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[14; 32]);
    let endpoint = client().await;
    let me = local_node(&endpoint);

    // Real node ids: unlike `exchange`, which passes announcement payloads through untouched,
    // `discover_peers` PARSES each one into a dialable address, so an arbitrary byte pattern would
    // be dropped as a non-canonical key rather than composed.
    let configured_peer = crate::node_id_from_secret([0x21; 32]);
    let discovered_only = crate::node_id_from_secret([0x22; 32]);
    for peer in [&configured_peer, &discovered_only] {
        service.seed(tag, peer.to_vec(), NOW_MS + 600_000);
    }

    let configured = [crate::node_id_to_string(&configured_peer).unwrap()];
    let resolved = crate::discover_peers(
        &configured,
        "https://relay.example",
        Some(DiscoveryExchange {
            endpoint: &endpoint,
            service: service.addr(),
            tag,
            publish: Some(&me),
            ttl_seconds: 600,
            fetch: true,
        }),
        &as_node_id,
    )
    .await;

    let ids: Vec<[u8; 32]> = resolved.peers.iter().map(|(_, addr)| *addr.id.as_bytes()).collect();
    assert!(!ids.contains(&me), "this node must never be dialed as its own peer");
    assert!(ids.contains(&discovered_only), "a discovered peer joins the set");
    assert_eq!(
        ids.iter().filter(|id| **id == configured_peer).count(),
        1,
        "a peer that is both configured and discovered is dialed once"
    );
    assert_eq!(ids.len(), 2, "exactly the two real peers: {ids:?}");
    assert_eq!(
        resolved.peers[0].0, configured[0],
        "configured peers keep their place at the front"
    );
    assert_eq!(resolved.unresolved_configured, 0);
}

/// Discovery is additive: when it finds nothing, the pass is exactly what it was before discovery
/// existed. This is what makes the feature safe to turn on.
#[tokio::test]
async fn an_empty_discovery_result_leaves_the_configured_peers_untouched() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let endpoint = client().await;
    let configured = [crate::node_id_to_string(&crate::node_id_from_secret([0x31; 32])).unwrap()];

    let resolved = crate::discover_peers(
        &configured,
        "https://relay.example",
        Some(DiscoveryExchange {
            endpoint: &endpoint,
            service: service.addr(),
            tag: account_tag(&[15; 32]),
            publish: None,
            ttl_seconds: 600,
            fetch: true,
        }),
        &as_node_id,
    )
    .await;
    assert_eq!(resolved.peers.len(), 1);
    assert_eq!(resolved.peers[0].0, configured[0]);
}

/// The peer cap counts announcements this device could OPEN, not payloads the service returned.
///
/// The killing case for a suppression attack that costs an attacker almost nothing. Anyone who can
/// compute the tag may publish under it, and a removed device can compute it forever. If the cap
/// were applied to raw payloads, `MAX_ANNOUNCEMENTS` pieces of garbage would discard every real
/// advertiser BEFORE anything tried to open them — cheaper than filling the service's slots, and
/// undetectable, because the good entries are dropped by the client's own bookkeeping.
///
/// So: a full cap's worth of unopenable payloads, then one real peer behind them.
#[tokio::test]
async fn unopenable_announcements_do_not_consume_the_peer_cap() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[21; 32]);
    let endpoint = client().await;

    // Not 32 bytes, so the opener rejects them — the shape of an envelope sealed to a roster this
    // device is not on, or simply of junk.
    for i in 0..crate::discovery::MAX_ANNOUNCEMENTS {
        service.seed(tag, vec![i as u8; 7], NOW_MS + 600_000);
    }
    let real_peer = crate::node_id_from_secret([0x44; 32]);
    service.seed(tag, real_peer.to_vec(), NOW_MS + 600_000);

    let resolved = crate::discover_peers(
        &[],
        "https://relay.example",
        Some(DiscoveryExchange {
            endpoint: &endpoint,
            service: service.addr(),
            tag,
            fetch: true,
            publish: None,
            ttl_seconds: 600,
        }),
        &as_node_id,
    )
    .await;
    let ids: Vec<[u8; 32]> = resolved.peers.iter().map(|(_, addr)| *addr.id.as_bytes()).collect();
    assert_eq!(
        ids,
        vec![real_peer],
        "a real advertiser was hidden behind a cap's worth of garbage"
    );
}

/// The cap still binds — on peers that resolved.
#[tokio::test]
async fn the_peer_cap_bounds_how_many_discovered_peers_one_pass_admits() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[22; 32]);
    let endpoint = client().await;

    let over = crate::discovery::MAX_ANNOUNCEMENTS + 4;
    for i in 0..over {
        let peer = crate::node_id_from_secret([i as u8; 32]);
        service.seed(tag, peer.to_vec(), NOW_MS + 600_000);
    }

    let resolved = crate::discover_peers(
        &[],
        "https://relay.example",
        Some(DiscoveryExchange {
            endpoint: &endpoint,
            service: service.addr(),
            tag,
            fetch: true,
            publish: None,
            ttl_seconds: 600,
        }),
        &as_node_id,
    )
    .await;
    assert_eq!(resolved.peers.len(), crate::discovery::MAX_ANNOUNCEMENTS);
}

// ---------------------------------------------------------------- the serving host's advertisement

/// A serving host advertises itself from startup, and KEEPS renewing.
///
/// `sync serve` is the node a machine behind NAT is trying to reach, and it is the one node the
/// maintenance-hook cadence can never announce — it has no maintenance hook. Delete the advertise
/// loop and the always-on host is invisible to every device that does not already have it in
/// `server_peers`.
///
/// The renewal half is asserted separately from the first announcement, because it is a different
/// mechanism and the obvious test misses it entirely: waiting only for the FIRST announcement stays
/// green with the `loop` reduced to a single iteration, with the tick period set to something
/// absurd (an `interval`'s first tick is immediate whatever its period), and with the renewal
/// check stubbed to "still live" forever. Requiring a SECOND announcement kills all three.
///
/// Real elapsed time, unlike the frozen-clock tests above: renewal is a question about elapsed
/// time, and a frozen clock can only ever answer it one way — which is exactly how a zero-margin
/// renewal went unnoticed.
#[tokio::test]
async fn a_serving_host_advertises_itself_immediately_and_keeps_renewing() {
    fn wall_clock_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_millis() as i64
    }

    // A four-second TTL ticks twice a second and renews at two seconds, so the whole
    // publish-then-renew cycle fits inside a test.
    const TTL: u32 = 4;
    let service = StubService::start(wall_clock_ms(), Behaviour::Serve).await;
    let tag = account_tag(&[16; 32]);
    let host = client().await;
    let node = local_node(&host);

    let (envelopes, announcement) = tokio::sync::watch::channel(Some(node.to_vec()));
    let advertiser = tokio::spawn(super::advertise(super::Advertise {
        endpoint: host.clone(),
        service: service.addr(),
        tag,
        announcement,
        ttl_seconds: TTL,
    }));

    let announced = wait_for(|| !service.stored(&tag).is_empty()).await;
    assert!(announced, "the host did not advertise itself at startup");

    // A second announcement can only come from a further pass through the loop that judged the
    // first no longer fresh enough.
    let renewed = wait_for(|| service.stored(&tag).len() >= 2).await;
    let stored = service.stored(&tag);
    advertiser.abort();
    assert!(
        renewed,
        "the host advertised once and then stopped renewing ({} stored)",
        stored.len()
    );
    assert!(
        stored.iter().all(|payload| payload.as_slice() == node.as_slice()),
        "every announcement under this tag is this host's own node id"
    );
    drop(envelopes);
}

/// The advertiser must re-read the watch on EVERY tick, not once at spawn.
///
/// This is what makes a roster change reach a long-running host: a device enrolled an hour after it
/// started becomes a recipient at the next renewal rather than never. Reading once at spawn passes
/// the renewal test above — which only ever sees one value — so the property needs its own case
/// that actually changes the value.
#[tokio::test]
async fn a_new_envelope_reaches_a_running_advertiser_at_the_next_tick() {
    fn wall_clock_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_millis() as i64
    }

    const TTL: u32 = 4;
    let service = StubService::start(wall_clock_ms(), Behaviour::Serve).await;
    let tag = account_tag(&[19; 32]);
    let host = client().await;

    let before = vec![0xa1u8; 48];
    let after = vec![0xb2u8; 48];
    let (envelopes, announcement) = tokio::sync::watch::channel(Some(before.clone()));
    let advertiser = tokio::spawn(super::advertise(super::Advertise {
        endpoint: host.clone(),
        service: service.addr(),
        tag,
        announcement,
        ttl_seconds: TTL,
    }));

    assert!(
        wait_for(|| service.stored(&tag).contains(&before)).await,
        "the first envelope is advertised"
    );

    // Swap it, as a roster change does.
    envelopes.send(Some(after.clone())).unwrap();
    let switched = wait_for(|| service.stored(&tag).contains(&after)).await;
    advertiser.abort();
    assert!(
        switched,
        "the replacement was never published; the watch is being read once, not per tick"
    );
}

/// A host publishes once per renewal interval however many times it ticks — and never asks the
/// service whether it is still live.
///
/// The killing case for renewal being LOCAL state. `ForgetfulFetch` answers every fetch with an
/// empty list, which is the real service's size-bounded random sample at its most adversarial: a
/// host's own live announcement is routinely missing from a response. Any implementation that
/// decides "am I live?" from a fetch sees "no" on every tick here, republishes on every tick, and
/// — because the service appends rather than replaces — evicts a real peer each time. That is a
/// feedback loop, not a wasted request: it is worst exactly when the tag is busy enough to sample.
///
/// Three assertions, because they fail for different reasons. The stored count kills republishing;
/// the fetch count kills a fetch whose answer is merely ignored, which no assertion about stored
/// announcements can see; and requiring at least one publish kills a loop that does nothing at all.
#[tokio::test]
async fn a_running_host_publishes_once_per_renewal_interval_however_often_it_ticks() {
    // Eight seconds ticks every second and renews at four, so the window below spans several ticks
    // and stops short of the first renewal.
    const TTL: u32 = 8;
    let service = StubService::start(0, Behaviour::ForgetfulFetch).await;
    let tag = account_tag(&[23; 32]);
    let host = client().await;
    let node = local_node(&host);

    let (envelopes, announcement) = tokio::sync::watch::channel(Some(node.to_vec()));
    let advertiser = tokio::spawn(super::advertise(super::Advertise {
        endpoint: host.clone(),
        service: service.addr(),
        tag,
        announcement,
        ttl_seconds: TTL,
    }));

    assert!(wait_for(|| !service.stored(&tag).is_empty()).await, "the host never advertised");
    // Well past three further ticks, still inside the four-second renewal interval.
    tokio::time::sleep(Duration::from_millis(3_200)).await;
    let stored = service.stored(&tag).len();
    let fetches = service.fetches();
    advertiser.abort();
    drop(envelopes);

    assert_eq!(stored, 1, "the host republished on ticks where its announcement was still live");
    assert_eq!(fetches, 0, "the advertiser asked the service about its own liveness");
}

/// Renewal is timed from when the publish was SENT, not from when the answer came back.
///
/// The service stamps expiry on receipt, so a client that starts its clock on the response is a
/// round trip behind the entry it is tracking, and renews that much nearer expiry. It is silent —
/// every renewal still succeeds — until one fails, and by then the margin that was supposed to
/// absorb it has been spent. At the exchange's ten-second deadline against the sixty-second TTL
/// floor it halves, from four attempts to two.
///
/// The stub stores the announcement and only then delays its response, which is what a loaded
/// service does. With an eight-second TTL renewal is due four seconds after the publish is sent,
/// and six after the response is read; the assertion falls between the two, so it can only pass
/// for a client timing from the send.
#[tokio::test]
async fn renewal_is_timed_from_the_publish_being_sent_not_answered() {
    const TTL: u32 = 8;
    let service = StubService::start(0, Behaviour::SlowPublish).await;
    let tag = account_tag(&[24; 32]);
    let host = client().await;
    let node = local_node(&host);

    let (envelopes, announcement) = tokio::sync::watch::channel(Some(node.to_vec()));
    let started = tokio::time::Instant::now();
    let advertiser = tokio::spawn(super::advertise(super::Advertise {
        endpoint: host.clone(),
        service: service.addr(),
        tag,
        announcement,
        ttl_seconds: TTL,
    }));

    assert!(wait_for(|| !service.stored(&tag).is_empty()).await, "the host never advertised");
    // Wait for the renewal up to a deadline that sits BETWEEN the two candidate schedules: a
    // send-timed clock renews at four seconds, a response-timed one no earlier than six. Polling to
    // a deadline rather than sampling at a fixed instant keeps the dial latency inside each
    // exchange from deciding the result.
    let deadline = started + Duration::from_millis(5_500);
    let mut renewed = false;
    while tokio::time::Instant::now() < deadline {
        if service.stored(&tag).len() >= 2 {
            renewed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    advertiser.abort();
    drop(envelopes);

    assert!(
        renewed,
        "renewal ran late; the clock is being started when the response arrives, not when the \
         publish is sent"
    );
}

/// A publish the service stored but never acknowledged is reported as ambiguous, not failed.
///
/// The distinction the renewal fix rests on. The service stamps expiry and stores on receipt,
/// before it answers, so a lost or late ack says nothing about whether the write landed — and the
/// stub proves exactly that by storing the announcement and then withholding the response.
/// Reporting this as `Refused` (definitely not stored) would make a renewing host re-append it
/// every tick.
#[tokio::test]
async fn a_stored_publish_with_a_lost_ack_is_uncertain_not_refused() {
    let service = StubService::start(NOW_MS, Behaviour::AckLostAfterStore).await;
    let tag = account_tag(&[25; 32]);
    let endpoint = client().await;
    let node = local_node(&endpoint);
    let started = Instant::now();

    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        fetch: false,
        publish: Some(&node),
        ttl_seconds: 600,
    })
    .await;

    assert_eq!(out.publish, PublishState::Uncertain, "a stored-but-unacked publish is ambiguous");
    assert_eq!(service.stored(&tag).len(), 1, "and it really was stored");
    assert!(
        started.elapsed() >= DISCOVERY_TIMEOUT,
        "the deadline is what ends the wait for the missing ack"
    );
}

/// The renewal-recording rule: possibly-live outcomes are recorded, a refusal is not.
///
/// This is the pure core of the lost-ack fix, unit-tested because exercising it through `advertise`
/// would cost a full `DISCOVERY_TIMEOUT` stall per tick. `Uncertain` records BECAUSE the write may
/// have landed — re-appending it every tick is how a host whose acks are dropped fills the tag.
/// `Refused` does not, because the service answered and stored nothing, so the next tick must
/// retry.
#[test]
fn only_possibly_live_publishes_reset_the_renewal_clock() {
    assert!(super::records_liveness(PublishState::Published), "a confirmed publish is live");
    assert!(
        super::records_liveness(PublishState::Uncertain),
        "an unacknowledged publish may be live and must not be re-appended every tick"
    );
    assert!(
        !super::records_liveness(PublishState::Refused),
        "a refusal stored nothing, so the next tick should retry"
    );
    assert!(
        !super::records_liveness(PublishState::NotAttempted),
        "nothing was published, so there is nothing to renew"
    );
}
