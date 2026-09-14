//! Accepting inbound connections: the single-stream acceptor and the global accept and egress
//! rate limiters.

use iroh::Endpoint;
use iroh::endpoint::Connection as IrohConnection;
use tokio::time::timeout;

use super::dispatch::{SyncFailure, connect_failed};
use crate::auth::{
    AuthConfig, AuthPolicy, AuthRole, DEFAULT_PRE_AUTH_TIMEOUT, NodeAuth, run_auth_phase,
};
use crate::session::{DEFAULT_IDLE_TIMEOUT, SessionReport, SyncStore, run_session};
use crate::wire::SYNC_ALPN;

/// How long an ACCEPTOR waits for the dialer to close the connection before closing from its own
/// side. A QUIC `close()` discards in-flight stream data, so the side that STREAMED a response must
/// not close until the dialer has read it — the dialer closes once its `run_session` finished
/// reading, and this bounds the wait so a vanished dialer can never wedge the acceptor.
pub(super) const GRACEFUL_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Accept ONE inbound connection and run a session against it. D4's `sync serve` loops this;
/// keeping it single-shot here keeps the store's `!Send` connection on one task (no spawn).
///
/// Every pre-session wait a peer controls — the handshake and opening the bidirectional stream — is
/// bounded by [`DEFAULT_IDLE_TIMEOUT`]. `run_session`'s own idle timeout only starts once the
/// stream is open, so without these a peer that connects and then stalls (never opening a stream)
/// would hold this single-session server forever, blocking every later peer.
///
/// `now_ms` is a CLOCK, read once a peer has connected — never a timestamp captured before the
/// accept wait. A server may idle arbitrarily long between connections, so the auth phase must
/// stamp and verify bindings against the time the connection actually arrived (see the read below).
pub async fn accept_and_sync<S: SyncStore + NodeAuth>(
    endpoint: &Endpoint,
    store: &mut S,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64,
) -> Result<SessionReport, SyncFailure> {
    let local_node = *endpoint.id().as_bytes();
    let incoming = endpoint.accept().await.ok_or_else(|| connect_failed("endpoint closed"))?;
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, incoming)
        .await
        .map_err(|_| connect_failed("handshake timed out"))?
        .map_err(|e| connect_failed(e.to_string()))?;
    let remote_node = *conn.remote_id().as_bytes();
    // This single-stream acceptor serves ONLY the account-log ALPN. The endpoint binds the content
    // ALPN too (for `accept_and_dispatch`), so a content client could negotiate it and land here —
    // reject it rather than run a content connection against the account-log store.
    if conn.alpn() != SYNC_ALPN {
        conn.close(0u32.into(), b"wrong-alpn");
        return Err(connect_failed(format!(
            "this acceptor serves only the account-log ALPN, got {:?}",
            conn.alpn()
        )));
    }
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| connect_failed("peer opened no stream"))?
        .map_err(|e| connect_failed(e.to_string()))?;
    // Read the clock only now that a peer has connected — NOT before the accept wait above. A
    // long-idle server whose stamp/verify time predated the wait would treat a peer's freshly
    // minted binding (and its own) as future-skewed and reject the session.
    let now_ms = now_ms();
    // Authorize the dialer BEFORE run_session so no inventory (not even account confirmation)
    // leaves this peer until the remote passes our policy (#881).
    let (capabilities, _admission) = run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
        role: AuthRole::Acceptor,
        account_id: store.account_id(),
        local_node,
        remote_node,
        policy,
        now_ms,
        pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
    })
    .await
    .map_err(SyncFailure::Auth)?;
    let report = run_session(store, send, recv, AuthRole::Acceptor, capabilities)
        .await
        .map_err(SyncFailure::Session)?;
    // The acceptor sends the final completion acknowledgement. Keep the connection alive until the
    // dialer reads it and closes, bounded so a vanished dialer cannot wedge the server.
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// Accept and complete the transport handshake for one inbound connection. Kept separate from
/// [`dispatch_connection`](super::dispatch::dispatch_connection) so a resident host can keep
/// accepting while prior sessions reconcile.
pub async fn accept_connection(endpoint: &Endpoint) -> Result<IrohConnection, SyncFailure> {
    let incoming = endpoint.accept().await.ok_or_else(|| connect_failed("endpoint closed"))?;
    timeout(DEFAULT_IDLE_TIMEOUT, incoming)
        .await
        .map_err(|_| connect_failed("handshake timed out"))?
        .map_err(|e| connect_failed(e.to_string()))
}

/// Inbound connections admitted per second in steady state once the burst is spent. Sized to clear
/// legitimate multi-peer inbound (a joiner opens ~4 rapid connections — enrollment + account log +
/// content + table) while bounding a flood. Implementation-local constant, not config.
pub(super) const ACCEPT_REFILL_PER_SEC: f64 = 8.0;
/// Maximum inbound connections admitted in one instantaneous burst.
pub(super) const ACCEPT_BURST: f64 = 32.0;

/// Bytes served per second in steady state once the burst is spent — the sustained egress ceiling a
/// public serving host allows across ALL peers. A text knowledge base is small, so a legitimate
/// full pull clears the burst instantly; the ceiling bounds an anonymous peer that re-pulls to
/// drain upload bandwidth. Implementation-local constants, not config (a host can be given a knob
/// later).
const EGRESS_REFILL_BYTES_PER_SEC: f64 = 16.0 * 1024.0 * 1024.0;
/// Bytes servable in one instantaneous burst before the steady-state ceiling applies.
pub(super) const EGRESS_BURST_BYTES: f64 = 64.0 * 1024.0 * 1024.0;

/// The token bucket both global limiters share: a balance refilled by elapsed time and capped at
/// `burst`, so idle time never accrues unbounded credit. Each limiter keeps its own spend rule.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    burst: f64,
    refill_per_sec: f64,
    last_ms: Option<i64>,
}

impl TokenBucket {
    /// A bucket that starts with its whole burst available.
    fn full(burst: f64, refill_per_sec: f64) -> Self {
        Self { tokens: burst, burst, refill_per_sec, last_ms: None }
    }

    /// Credit the time elapsed since the last call (capped at `burst`) and return the balance.
    /// `now_ms` is injected so refill is deterministically testable.
    fn refill(&mut self, now_ms: i64) -> f64 {
        if let Some(last) = self.last_ms {
            let elapsed_secs = (now_ms - last).max(0) as f64 / 1000.0;
            self.tokens = (self.tokens + elapsed_secs * self.refill_per_sec).min(self.burst);
        }
        self.last_ms = Some(now_ms);
        self.tokens
    }

    fn spend(&mut self, cost: f64) {
        self.tokens -= cost;
    }
}

/// A GLOBAL inbound-connection rate limiter: one token bucket bounding total accept rate regardless
/// of peer identity. Per-peer-by-node-id limiting is the wrong lever — an iroh node id is a keypair
/// mintable in microseconds, so a flood rotates ids and evades any per-id bucket while making the
/// id map its own memory/eviction target. The Sybil-resistant bound is global and refused before
/// the handshake. In-memory and transient by design: a restart resetting the window to full burst
/// is correct for a live-traffic control (contrast the durable byte ceilings on the ingest paths).
#[derive(Debug)]
pub struct GlobalAcceptRateLimiter {
    bucket: TokenBucket,
}

impl Default for GlobalAcceptRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalAcceptRateLimiter {
    pub fn new() -> Self {
        Self { bucket: TokenBucket::full(ACCEPT_BURST, ACCEPT_REFILL_PER_SEC) }
    }

    /// Refill by the time elapsed since the last call (capped at `burst`, so idle time never
    /// accrues unbounded credit), then spend one token. Returns `false` when the bucket is
    /// empty — the connection should be refused. `now_ms` is injected so refill is
    /// deterministically testable.
    pub fn allow(&mut self, now_ms: i64) -> bool {
        if self.bucket.refill(now_ms) >= 1.0 {
            self.bucket.spend(1.0);
            true
        } else {
            false
        }
    }
}

/// A GLOBAL byte-rate limiter bounding total EGRESS (data served to peers) regardless of peer
/// identity — the anti-drain counterpart to [`GlobalAcceptRateLimiter`]. Global, not per-peer, for
/// the same Sybil reason: a per-id budget is evaded by rotating node ids. Shared across the host's
/// concurrent per-connection tasks (an `Arc<Mutex<_>>`), checked at each outgoing page. In-memory
/// and transient: a restart resetting to full burst is correct for live-traffic control.
#[derive(Debug)]
pub struct GlobalEgressLimiter {
    bucket: TokenBucket,
}

impl Default for GlobalEgressLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalEgressLimiter {
    pub fn new() -> Self {
        Self { bucket: TokenBucket::full(EGRESS_BURST_BYTES, EGRESS_REFILL_BYTES_PER_SEC) }
    }

    /// Refill by elapsed time (capped at `burst`), then, IF any credit remains, spend `bytes` (the
    /// balance may go negative for an oversized page) and permit the page; otherwise refuse so the
    /// sender stops after the pages already sent. Permitting on ANY positive credit guarantees
    /// forward progress even for a page larger than the whole burst — a reader is never wedged,
    /// only throttled, and the unsent tail is re-offered by the next session's inventory diff.
    /// `now_ms` injected for deterministic tests.
    pub fn allow(&mut self, bytes: usize, now_ms: i64) -> bool {
        if self.bucket.refill(now_ms) > 0.0 {
            self.bucket.spend(bytes as f64);
            true
        } else {
            false
        }
    }
}

/// Accept one inbound connection, refusing it BEFORE the TLS handshake when the global accept-rate
/// `limiter` is exhausted. `Ok(None)` means the connection was refused by the rate limit — the
/// caller continues its accept loop; `Ok(Some(conn))` means admitted and handshaken. Refusing at
/// the `Incoming` stage costs no handshake CPU and reveals nothing to the peer.
pub async fn accept_connection_within_rate(
    endpoint: &Endpoint,
    limiter: &mut GlobalAcceptRateLimiter,
    now_ms: impl Fn() -> i64,
) -> Result<Option<IrohConnection>, SyncFailure> {
    let incoming = endpoint.accept().await.ok_or_else(|| connect_failed("endpoint closed"))?;
    // Read the clock only NOW that a peer has connected — `accept()` can idle arbitrarily long, and
    // a timestamp taken before the wait would under-credit the bucket's refill and wrongly
    // refuse a connection arriving after a load-then-idle stretch.
    if !limiter.allow(now_ms()) {
        incoming.refuse();
        return Ok(None);
    }
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, incoming)
        .await
        .map_err(|_| connect_failed("handshake timed out"))?
        .map_err(|e| connect_failed(e.to_string()))?;
    Ok(Some(conn))
}
