//! Dialing a peer through the mutual auth phase, and the multi-round reconcile loops.

use iroh::endpoint::Connection as IrohConnection;
use iroh::{Endpoint, EndpointAddr};
use tokio::time::timeout;

use super::dispatch::{SyncAlpn, SyncFailure, connect_failed};
use crate::auth::{
    AuthConfig, AuthPolicy, AuthRole, DEFAULT_PRE_AUTH_TIMEOUT, NodeAuth, SessionCapabilities,
    run_auth_phase,
};
use crate::session::{DEFAULT_IDLE_TIMEOUT, SessionReport, SyncStore, run_session};
use crate::table_session::{TableSessionReport, TableSyncStore, run_table_session};

/// Dial `peer`, authorize each other under `policy`, then run one sync session, returning what
/// moved. The auth handshake (#881) runs BEFORE any inventory: the dialer presents its binding,
/// then verifies the acceptor's before revealing anything, so a poisoned address never leaks the
/// account log to an impostor.
pub async fn connect_and_sync<S: SyncStore + NodeAuth>(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    stream: SyncAlpn,
    store: &mut S,
    policy: AuthPolicy,
    now_ms: i64,
) -> Result<SessionReport, SyncFailure> {
    // `stream` selects what the dialer wants — [`SyncAlpn::Account`] for the account log,
    // [`SyncAlpn::Content`] for the content lane — and must match `store`'s type. The acceptor
    // routes to the matching store by the negotiated ALPN.
    let account_id = store.account_id();
    let AuthedDial { conn, send, recv, capabilities } =
        dial_authed(endpoint, peer, stream, &*store, account_id, policy, now_ms).await?;
    let report = run_session(store, send, recv, AuthRole::Dialer, capabilities)
        .await
        .map_err(SyncFailure::Session)?;
    // The role-ordered completion acknowledgement proves the acceptor consumed everything we
    // pushed before replying, and we consumed its whole stream before sending our acknowledgement.
    // The dialer therefore closes only after both directions are delivered.
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// Dial the dedicated table-sync ALPN, run the existing mutual account auth under closed-roster
/// policy, then reconcile the bounded manifest intersection.
pub async fn connect_and_table_sync<S: TableSyncStore + NodeAuth>(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    store: &mut S,
    now_ms: i64,
) -> Result<TableSessionReport, SyncFailure> {
    let account_id = store.account_id();
    let AuthedDial { conn, send, recv, capabilities } = dial_authed(
        endpoint,
        peer,
        SyncAlpn::Table,
        &*store,
        account_id,
        AuthPolicy::Closed,
        now_ms,
    )
    .await?;
    let report = run_table_session(store, send, recv, AuthRole::Dialer, capabilities)
        .await
        .map_err(SyncFailure::TableSession)?;
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// A dialed connection past the mutual auth phase, its session stream open.
struct AuthedDial {
    conn: IrohConnection,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    capabilities: SessionCapabilities,
}

/// Dial `peer` on `alpn`, open the session stream, and run the dialer side of the auth phase under
/// `policy` — no inventory moves until it passes. Every peer-controlled wait is bounded explicitly
/// (mirroring `accept_and_sync`) rather than inheriting a transport dependency's idle default: a
/// dead or unreachable configured peer must fail the dial promptly — a device-side sync holds the
/// per-database session lock while it runs.
async fn dial_authed<A: NodeAuth>(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    stream: SyncAlpn,
    auth: &A,
    account_id: [u8; 32],
    policy: AuthPolicy,
    now_ms: i64,
) -> Result<AuthedDial, SyncFailure> {
    let lane = stream.dial_label();
    let conn = timeout(DEFAULT_IDLE_TIMEOUT, endpoint.connect(peer, stream.as_bytes()))
        .await
        .map_err(|_| connect_failed(format!("{lane}dial timed out")))?
        .map_err(|error| connect_failed(error.to_string()))?;
    let remote_node = *conn.remote_id().as_bytes();
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.open_bi())
        .await
        .map_err(|_| connect_failed(format!("opening a {lane}stream timed out")))?
        .map_err(|error| connect_failed(error.to_string()))?;
    let (capabilities, _admission) = run_auth_phase(&mut send, &mut recv, auth, AuthConfig {
        role: AuthRole::Dialer,
        account_id,
        local_node: *endpoint.id().as_bytes(),
        remote_node,
        policy,
        now_ms,
        pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
    })
    .await
    .map_err(SyncFailure::Auth)?;
    Ok(AuthedDial { conn, send, recv, capabilities })
}

/// The most times a dialer re-runs a sync session against ONE peer before giving up on convergence
/// for this pass. A cooperative account reaches a fixpoint in a handful of rounds; a peer still not
/// dry after this many re-syncs is withholding an authorizer or is pathologically active, so stop
/// with `converged = false` and let the next maintenance pass continue — device-side sync re-runs
/// the loop on its cadence, so a capped pass only defers the remainder, it never loses it.
pub const MAX_RECONCILE_ROUNDS: usize = 8;

/// What a multi-round reconciliation moved (aggregated across its rounds). `converged` is true when
/// the loop stopped on a fully quiet round — nothing stored, sent, or received in either direction,
/// so both peers hold the union of what each offered. It is false when the round cap was hit first:
/// the store may still be incomplete and a later pass should continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    pub rounds: usize,
    pub entries_newly_stored: usize,
    pub entries_sent: usize,
    pub converged: bool,
    /// What this side granted the remote in the LAST round — see
    /// [`SessionReport::peer_capability`]. `ReadOnly` here means the peer was never allowed to
    /// send, so `converged` says only that nothing moved, not that the stores agree.
    pub peer_capability: crate::auth::PeerCapability,
}

/// Whether reconciliation should run another round, given the round just completed (#878). A single
/// `run_session` can report `Done` while a store is still incomplete — an adversarial sender
/// streaming dependents before their authorizer overruns the pre-verify eviction budget (only the
/// survivors promote), and even a cooperative large account may not reach a fixpoint in one pass.
/// Re-running converges: each store grows monotonically and every hello advertises parked entries
/// (#877), so a re-sent dependent promotes once its authorizer is present on that side.
///
/// The fixpoint is a fully QUIET round — nothing stored, sent, OR received. All three matter:
/// `sent` catches a PUSH that made the acceptor evict (the confirmation quiet round proves the
/// re-pushed entries finally landed), `received` catches a peer that still has data for us, and
/// `stored` catches local promotion progress. This is reliable because a store under
/// [`MAX_HELLO_HASHES`] (65_536) advertises its COMPLETE inventory — parked entries included — so
/// the counters reflect real gaps, not redelivery, for every store D targets. A store OVER that cap
/// advertises only a bounded inventory, so a peer may re-offer already-held entries every round and
/// the session never fully quiets; the round cap then stops it with `converged = false`, the honest
/// outcome given the deliberate absence of a remainder-reconcile protocol for such stores.
pub(super) fn reconcile_step(moved: bool, rounds_done: usize, max_rounds: usize) -> ReconcileStep {
    if !moved {
        ReconcileStep::Stop { converged: true }
    } else if rounds_done >= max_rounds {
        // Still moving at the cap: the store may not be complete yet — a later maintenance pass
        // continues from where this left off.
        ReconcileStep::Stop { converged: false }
    } else {
        ReconcileStep::Continue
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReconcileStep {
    Continue,
    Stop { converged: bool },
}

/// Running totals across a reconciliation's rounds, shared by the account/content and table loops.
#[derive(Debug, Default)]
pub(super) struct RoundTally {
    rounds: usize,
    entries_newly_stored: usize,
    entries_sent: usize,
}

impl RoundTally {
    /// Fold one finished round in and return whether it moved anything — stored, sent, OR
    /// received; see [`reconcile_step`] for why all three count.
    pub(super) fn record(&mut self, newly_stored: usize, sent: usize, received: usize) -> bool {
        self.rounds += 1;
        self.entries_newly_stored += newly_stored;
        self.entries_sent += sent;
        newly_stored > 0 || sent > 0 || received > 0
    }

    fn report(
        &self,
        converged: bool,
        peer_capability: crate::auth::PeerCapability,
    ) -> ReconcileReport {
        ReconcileReport {
            rounds: self.rounds,
            entries_newly_stored: self.entries_newly_stored,
            entries_sent: self.entries_sent,
            converged,
            peer_capability,
        }
    }
}

/// Dial `peer` repeatedly, running one [`connect_and_sync`] session per round until the transfer
/// reaches a fixpoint or the round cap — the multi-round reconciliation #878 needs so a single
/// session's `Done` is never mistaken for a complete store. The serve acceptor already loops
/// accepting connections, so this dialer-side loop is all it takes: no wire change. `now_ms` is
/// read fresh each round so every re-dialed session stamps the time it actually ran (a long
/// reconciliation must not authorize against a pre-loop timestamp).
pub async fn connect_and_reconcile<S: SyncStore + NodeAuth>(
    endpoint: &Endpoint,
    peer: EndpointAddr,
    stream: SyncAlpn,
    store: &mut S,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64,
    max_rounds: usize,
) -> Result<ReconcileReport, SyncFailure> {
    let mut tally = RoundTally::default();
    loop {
        let report =
            connect_and_sync(endpoint, peer.clone(), stream, store, policy, now_ms()).await?;
        let moved =
            tally.record(report.entries_newly_stored, report.entries_sent, report.entries_received);
        if let ReconcileStep::Stop { converged } = reconcile_step(moved, tally.rounds, max_rounds) {
            return Ok(tally.report(converged, report.peer_capability));
        }
    }
}

/// Re-run table sessions until the same fully-quiet fixpoint used by account/content sync.
pub async fn connect_and_table_reconcile<S: TableSyncStore + NodeAuth>(
    endpoint: &Endpoint,
    peer: EndpointAddr,
    store: &mut S,
    now_ms: impl Fn() -> i64,
    max_rounds: usize,
) -> Result<ReconcileReport, SyncFailure> {
    let mut tally = RoundTally::default();
    loop {
        let report = connect_and_table_sync(endpoint, peer.clone(), store, now_ms()).await?;
        // A pending continuation is more data to move, so it keeps the loop going like any round
        // that moved entries.
        let moved =
            tally.record(report.entries_newly_stored, report.entries_sent, report.entries_received)
                || report.continuation_pending;
        if let ReconcileStep::Stop { converged } = reconcile_step(moved, tally.rounds, max_rounds) {
            // The table lane is pinned `Closed`, so a session that ran at all was
            // roster-authorized.
            return Ok(tally.report(converged, crate::auth::PeerCapability::ReadWrite));
        }
    }
}
