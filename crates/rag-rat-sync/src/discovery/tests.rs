use std::time::{Duration, Instant};

use iroh::endpoint::presets;
use iroh::{Endpoint, RelayMode};

use super::stub::{Behaviour, PER_TAG_CAP, StubService};
use super::{
    DISCOVERY_TAG_DOMAIN, DISCOVERY_TIMEOUT, DiscoveryExchange, PEER_DISCOVERY_ALPN, PublishState,
    account_tag, exchange, publish_ttl_seconds,
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
        publish: Some(published),
        ttl_seconds: 600,
        now_ms: NOW_MS,
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
        now_ms: NOW_MS,
    })
    .await;
    assert_eq!(out.peers, vec![published], "the fetcher sees the publisher");
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
        now_ms: NOW_MS,
    })
    .await;
    assert_eq!(out.peers, vec![peer]);
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
        publish: Some(local_node(&endpoint)),
        ttl_seconds: 600,
        now_ms: NOW_MS,
    })
    .await;
    assert_eq!(out.publish, PublishState::Failed);
    assert_eq!(out.peers, vec![peer], "the fetched peers survive the publish failure");
    assert!(out.degraded.is_some(), "and the failure is reported for logging");
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
        now_ms: NOW_MS,
    })
    .await;
    assert_eq!(out.peers, vec![good], "the well-formed peer survives its malformed neighbour");
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
        publish: Some(local_node(&endpoint)),
        ttl_seconds: 600,
        now_ms: NOW_MS,
    })
    .await;
    let elapsed = started.elapsed();

    assert!(out.peers.is_empty(), "a stalled service yields no peers");
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
        now_ms: NOW_MS,
    })
    .await;
    assert!(out.peers.is_empty());
    assert!(out.degraded.is_some(), "the failure is reported, never propagated as an error");
    assert!(
        started.elapsed() < DISCOVERY_TIMEOUT,
        "an undialable address must fail on its own, not by hitting the bound"
    );
}

// ---------------------------------------------------------------- cap exhaustion

/// A live announcement with room to spare means this pass spends no slot on another copy.
#[tokio::test]
async fn a_still_fresh_announcement_is_not_republished() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[9; 32]);
    let endpoint = client().await;
    let node = local_node(&endpoint);
    // Published a moment ago under a 600s TTL: well over half its life remains.
    service.seed(tag, node.to_vec(), NOW_MS + 590_000);

    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        publish: Some(node),
        ttl_seconds: 600,
        now_ms: NOW_MS,
    })
    .await;
    assert_eq!(out.publish, PublishState::AlreadyLive);
    assert_eq!(service.stored(&tag).len(), 1, "no second copy of ourselves was added");
}

/// Close to expiry it IS republished — skipping here would let the entry lapse between passes and
/// silently stop advertising this device.
#[tokio::test]
async fn an_announcement_near_expiry_is_republished() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[10; 32]);
    let endpoint = client().await;
    let node = local_node(&endpoint);
    // Under half of a 600s TTL remains, i.e. less than one cadence of headroom.
    service.seed(tag, node.to_vec(), NOW_MS + 200_000);

    let out = exchange(DiscoveryExchange {
        endpoint: &endpoint,
        service: service.addr(),
        tag,
        publish: Some(node),
        ttl_seconds: 600,
        now_ms: NOW_MS,
    })
    .await;
    assert_eq!(out.publish, PublishState::Published);
    assert_eq!(service.stored(&tag).len(), 2, "the fresh copy joins the one about to lapse");
}

/// Steady state on the default cadence: three devices, six passes, no publish ever refused.
///
/// The original design — publish at the service maximum every pass, never skipping — gives each
/// device `ceil(900/300) = 3` live copies, so three devices need 9 slots against a cap of 8, the
/// ninth publish is REJECTED (the service never evicts to make room), and an ordinary three-device
/// account breaks its own discovery on default configuration.
///
/// Honest about what this test can and cannot kill: at this cadence the TTL clamp and the
/// fetch-before-publish skip are REDUNDANT — either alone keeps three devices inside the cap, so
/// reverting one of them does not turn this red. It is a regression test on the combination. The
/// skip has its own killing case in
/// `rapid_passes_do_not_fill_the_tag_with_copies_of_one_device`, and the clamp's arithmetic is
/// pinned by `the_published_ttl_is_two_cadences_clamped_to_what_the_service_accepts`.
#[tokio::test]
async fn a_three_device_account_on_the_default_cadence_stays_under_the_per_tag_cap() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[11; 32]);
    let ttl = publish_ttl_seconds(300);

    let devices = [client().await, client().await, client().await];
    let ids: Vec<[u8; 32]> = devices.iter().map(local_node).collect();

    // Six passes at the default cadence: long enough for entries to accumulate and for the first
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
                publish: Some(local_node(endpoint)),
                ttl_seconds: ttl,
                now_ms: pass_now_ms,
            })
            .await;
            assert_ne!(
                out.publish,
                PublishState::Failed,
                "pass {pass}: a publish was refused — the tag filled up ({:?})",
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
        publish: None,
        ttl_seconds: ttl,
        now_ms: NOW_MS + 1_500_000,
    })
    .await;
    for id in &ids {
        assert!(out.peers.contains(id), "a device stopped being discoverable");
    }
}

/// The fetch-before-publish skip's killing case.
///
/// `push_interval_secs = 0` runs a pass on every trigger, and several git hooks fire per action —
/// so passes land seconds apart while the TTL floor is 60s. Publishing blind each time stacks a
/// device's own announcements until the tag is full and EVERY device's publishes start failing.
/// The skip is the only thing standing between that and a self-inflicted outage; delete it and this
/// test goes red.
#[tokio::test]
async fn rapid_passes_do_not_fill_the_tag_with_copies_of_one_device() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[12; 32]);
    let ttl = publish_ttl_seconds(0);
    assert_eq!(ttl, 60, "the floor is what makes rapid passes dangerous");

    let devices = [client().await, client().await, client().await];
    // Twelve passes one second apart: well inside a single TTL, so nothing expires to make room.
    for pass in 0..12i64 {
        let pass_now_ms = NOW_MS + pass * 1_000;
        service.advance_to(pass_now_ms);
        for endpoint in &devices {
            let out = exchange(DiscoveryExchange {
                endpoint,
                service: service.addr(),
                tag,
                publish: Some(local_node(endpoint)),
                ttl_seconds: ttl,
                now_ms: pass_now_ms,
            })
            .await;
            assert_ne!(
                out.publish,
                PublishState::Failed,
                "pass {pass}: a publish was refused — the tag filled with our own copies ({:?})",
                out.degraded
            );
        }
    }
    assert_eq!(
        service.stored(&tag).len(),
        devices.len(),
        "one live announcement per device, however many passes ran"
    );
}

/// The residual, pinned so it is a known limit rather than a surprise.
///
/// The service caps a tag at 8 live announcements and REJECTS rather than evicting once full, so an
/// account large enough to need more slots than that has publishes refused no matter how carefully
/// this client rations them. Two live copies per device is the steady state, so the ceiling is
/// around four devices. Fetching is unaffected — a refused device simply stops being discoverable
/// itself while still finding everyone else — and explicit `server_peers` remain first-class, which
/// is why this is a limit and not a blocker. Lifting it needs the SERVICE to evict oldest instead
/// of refusing.
#[tokio::test]
async fn beyond_roughly_four_devices_the_service_starts_refusing_publishes() {
    let service = StubService::start(NOW_MS, Behaviour::Serve).await;
    let tag = account_tag(&[13; 32]);
    let ttl = publish_ttl_seconds(300);

    let mut devices = Vec::new();
    for _ in 0..5 {
        devices.push(client().await);
    }

    let mut refused = false;
    for pass in 0..4i64 {
        let pass_now_ms = NOW_MS + pass * 300_000;
        service.advance_to(pass_now_ms);
        for endpoint in &devices {
            let out = exchange(DiscoveryExchange {
                endpoint,
                service: service.addr(),
                tag,
                publish: Some(local_node(endpoint)),
                ttl_seconds: ttl,
                now_ms: pass_now_ms,
            })
            .await;
            if out.publish == PublishState::Failed {
                refused = true;
                // The point of the limit being survivable: a device that cannot advertise ITSELF
                // still learns where everyone else is. (A refusal only happens once the tag is
                // full, so there is certainly something to have fetched.)
                assert!(!out.peers.is_empty(), "a refused publish must not cost us the fetch");
            }
        }
    }
    assert!(
        refused,
        "five devices are expected to hit the per-tag cap; if this stops happening the service \
         changed its cap or its eviction policy and the limit above should be re-measured"
    );
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
            publish: Some(me),
            ttl_seconds: 600,
            now_ms: NOW_MS,
        }),
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
            now_ms: NOW_MS,
        }),
    )
    .await;
    assert_eq!(resolved.peers.len(), 1);
    assert_eq!(resolved.peers[0].0, configured[0]);
}
