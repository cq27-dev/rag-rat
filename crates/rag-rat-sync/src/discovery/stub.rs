//! An in-process peer-discovery service for the tests, over real iroh and the real wire.
//!
//! The client half of discovery is only as trustworthy as what it was tested against, so this stub
//! is deliberately literal: it speaks the same CBOR the counterpart does (through the test-only
//! service-side codec in [`super::wire`], which is itself pinned to the measured golden vectors)
//! and it reproduces the two service behaviours that shape this client's design — announcements are
//! APPENDED under a tag and never replaced, and a tag holds at most [`PER_TAG_CAP`] live entries,
//! REJECTING rather than evicting once full.
//!
//! [`Behaviour`] covers the failure modes the client must survive. A stub that only ever answered
//! correctly would leave every fail-open path untested, and those paths are the whole reason
//! discovery is safe to run inside the sync session lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMode};

use super::PEER_DISCOVERY_ALPN;
use super::wire::{
    DecodedErrorCode, DiscoveryErrorCode, DiscoveryRequest, DiscoveryResponse, TAG_LEN,
    WireAnnouncement, read_frame, write_frame,
};

/// The real service's per-tag cap. Mirrored so the exhaustion behaviour under test is the real one.
pub(crate) const PER_TAG_CAP: usize = 8;

/// How the stub answers. Each variant is a failure the client must absorb without harming the
/// configured-peer path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Behaviour {
    /// Answer both requests correctly.
    Serve,
    /// Accept the bi-stream and never answer. The shape a transport error CANNOT produce, and the
    /// only thing the client's timeout defends against — an unreachable address fails fast, so a
    /// test using one would pass with the timeout deleted.
    BlackHole,
    /// Refuse every publish, answer fetches normally: a device that is rate-limited for writing
    /// must still learn where its peers are.
    RefusePublish,
    /// Answer fetches with one unusable payload alongside the real ones.
    GarbageAmongTheGood,
}

pub(crate) struct StubService {
    endpoint: Endpoint,
    tags: Arc<Mutex<HashMap<[u8; TAG_LEN], Vec<WireAnnouncement>>>>,
    /// The stub's own clock, advanced by the test. Owned per-instance rather than read from the
    /// system so expiry and reaping are exact, and so nothing is shared between tests.
    now_ms: Arc<AtomicI64>,
}

impl StubService {
    /// Bind the stub and start serving from `now_ms`, which the test advances with
    /// [`Self::advance_to`].
    pub(crate) async fn start(now_ms: i64, behaviour: Behaviour) -> Self {
        let endpoint = Endpoint::builder(presets::Minimal)
            .alpns(vec![PEER_DISCOVERY_ALPN.to_vec()])
            // No relay: the tests dial the direct address, so nothing leaves the machine.
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .expect("bind the stub discovery service");
        let tags = Arc::new(Mutex::new(HashMap::new()));
        let now = Arc::new(AtomicI64::new(now_ms));
        let service =
            Self { endpoint: endpoint.clone(), tags: Arc::clone(&tags), now_ms: Arc::clone(&now) };
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let tags = Arc::clone(&tags);
                let now = Arc::clone(&now);
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    // Swallowed streams are PARKED, not dropped: dropping a send stream closes it,
                    // which the client sees as an immediate error — the opposite of a black hole,
                    // and enough to make the timeout test pass with the timeout deleted.
                    let mut parked = Vec::new();
                    // One request per bi-stream, exactly like the service: it reads a single frame,
                    // answers, and returns. A client that wrote two requests to one stream would
                    // hang here rather than getting a second answer.
                    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                        let Ok(body) = read_frame(&mut recv).await else { return };
                        let Ok(request) = DiscoveryRequest::decode(&body) else { return };
                        if behaviour == Behaviour::BlackHole {
                            // Accepted, never answered, held open.
                            parked.push((send, recv));
                            continue;
                        }
                        let response =
                            answer(&tags, &request, behaviour, now.load(Ordering::Relaxed));
                        if write_frame(&mut send, &response.encode()).await.is_err() {
                            return;
                        }
                        let _ = send.finish();
                    }
                });
            }
        });
        service
    }

    /// Move the stub's clock forward, so entries published under an earlier pass can expire.
    pub(crate) fn advance_to(&self, now_ms: i64) {
        self.now_ms.store(now_ms, Ordering::Relaxed);
    }

    /// The stub's dialable address — direct, no relay.
    pub(crate) fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// The payloads LIVE under `tag` at the stub's current clock, in insertion order. Live rather
    /// than stored, because the cap the client works against counts live entries only.
    pub(crate) fn stored(&self, tag: &[u8; TAG_LEN]) -> Vec<Vec<u8>> {
        let now = self.now_ms.load(Ordering::Relaxed);
        self.tags
            .lock()
            .unwrap()
            .get(tag)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| entry.expires_at_ms > now)
                    .map(|entry| entry.payload.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Seed an announcement directly, bypassing the wire — for setting up a tag's prior state.
    pub(crate) fn seed(&self, tag: [u8; TAG_LEN], payload: Vec<u8>, expires_at_ms: i64) {
        self.tags
            .lock()
            .unwrap()
            .entry(tag)
            .or_default()
            .push(WireAnnouncement { payload, expires_at_ms });
    }
}

fn answer(
    tags: &Mutex<HashMap<[u8; TAG_LEN], Vec<WireAnnouncement>>>,
    request: &DiscoveryRequest,
    behaviour: Behaviour,
    now_ms: i64,
) -> DiscoveryResponse {
    let mut tags = tags.lock().unwrap();
    match request {
        DiscoveryRequest::Publish { tag, payload, ttl_seconds } => {
            if behaviour == Behaviour::RefusePublish {
                return refusal(DiscoveryErrorCode::RateLimited, "slow down");
            }
            let entries = tags.entry(*tag).or_default();
            // Reap expired, then REJECT when full — the service never evicts to make room, which is
            // why a tag an attacker fills holds zero real peers rather than eight bad ones.
            entries.retain(|entry| entry.expires_at_ms > now_ms);
            if entries.len() >= PER_TAG_CAP {
                return refusal(DiscoveryErrorCode::PerTagCapExceeded, "tag is full");
            }
            // APPEND, never replace: republishing the same node id adds a second live copy.
            entries.push(WireAnnouncement {
                payload: payload.clone(),
                expires_at_ms: now_ms + i64::from(*ttl_seconds) * 1000,
            });
            DiscoveryResponse::Published { withdraw_token: vec![0xaa; 8] }
        },
        DiscoveryRequest::Fetch { tag } => {
            let mut announcements = tags
                .get(tag)
                .map(|entries| {
                    entries.iter().filter(|e| e.expires_at_ms > now_ms).cloned().collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if behaviour == Behaviour::GarbageAmongTheGood {
                // A payload that is not 32 bytes. Anyone who can compute the tag can publish this.
                announcements.insert(0, WireAnnouncement {
                    payload: vec![0xff; 7],
                    expires_at_ms: now_ms + 60_000,
                });
            }
            DiscoveryResponse::Fetched { announcements }
        },
        DiscoveryRequest::Withdraw { .. } =>
            refusal(DiscoveryErrorCode::WithdrawUnknownTag, "the stub does not withdraw"),
    }
}

fn refusal(code: DiscoveryErrorCode, message: &str) -> DiscoveryResponse {
    DiscoveryResponse::Error { code: DecodedErrorCode::Known(code), message: message.to_string() }
}
