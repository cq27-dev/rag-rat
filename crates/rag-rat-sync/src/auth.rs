//! The node-authorization handshake that gates a session (phase D, #881).
//!
//! Runs BEFORE [`crate::session::run_session`], over the same bidirectional stream, and reveals
//! nothing about the account (not even confirmation that this peer hosts it) until the remote is
//! authorized. Each side presents a signed transport-node ↔ account-device binding and verifies the
//! other's under its OWN admission policy — authorization is **mutual**, so a peer handed a
//! poisoned address never streams its inventory to an impostor.
//!
//! Ordering is asymmetric to close the metadata gap without deadlocking (the transfer phase that
//! follows stays concurrent):
//! - the **acceptor** reads the dialer's binding and verifies it BEFORE sending its own — an
//!   unauthorized dialer learns nothing, not even the acceptor's binding;
//! - the **dialer** sends its binding (to the node iroh already authenticated it dialed) then
//!   verifies the acceptor's before proceeding to the data phase.
//!
//! A failure closes with one UNIFORM error regardless of cause (wrong account / not on roster / bad
//! signature / stale) so a peer cannot probe "does this server host account A / know device D".

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::codec::{self, CodecError};
use crate::wire::Frame;

/// How long a peer may take to send its auth frame before the handshake aborts. Deliberately much
/// shorter than the data-phase idle timeout: an unauthorized peer that connects and stalls must not
/// occupy the single-session accept slot for long.
pub const DEFAULT_PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest frame accepted during the auth phase, checked against the length prefix BEFORE any
/// allocation. A valid auth frame is a ~600-byte cap (domain + tag + account id + a
/// [`crate::wire::MAX_AUTH_BINDING_BYTES`] binding); 1 KiB leaves headroom while keeping the
/// pre-auth allocation an unauthenticated peer can force to ~1 KiB, not the data-phase
/// [`crate::codec::MAX_FRAME_BYTES`].
const MAX_AUTH_FRAME_BYTES: u32 = 1024;

/// How a peer decides whether to admit a connection. Local policy, never negotiated on the wire —
/// an impostor cannot assert `Open` to exempt itself from the other side's `Closed`. Per-ACCOUNT,
/// not per-endpoint: one transport node may legitimately serve several accounts, so a public
/// account being `Open` must not open a private one on the same endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPolicy {
    /// Admit any dialer for reads. A valid roster binding determines its capability when available;
    /// otherwise an acceptor keeps the dialer read-only, while a dialer permits its explicitly
    /// selected server to send the snapshot needed to restore roster state.
    Open,
    /// Admit only a peer whose binding verifies against this account's roster and the connection's
    /// authenticated remote node id. The default for a private account.
    Closed,
    /// Admit a not-yet-roster peer via a one-time invite token — the onboarding/pairing flow. Not
    /// implemented in this slice (it needs the token issue/redeem exchange); a session configured
    /// with it fails closed rather than silently admitting.
    InviteToken,
}

/// Which end of the connection this peer is — determines the send/verify order above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRole {
    Dialer,
    Acceptor,
}

/// Whether the authenticated peer may transmit entries in the data phase. This is a transport
/// capability, not proof that the peer authored those entries: under [`AuthPolicy::Open`], a dialer
/// permits its explicitly selected server to send the snapshot needed to restore roster state.
/// Every received entry still passes the store's cryptographic and authority checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerCapability {
    ReadOnly,
    ReadWrite,
}

impl PeerCapability {
    pub(crate) fn can_push(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// The non-stream inputs to one auth handshake, bundled so [`run_auth_phase`] takes the stream
/// pair, the authorizer, and this — rather than a long argument train.
#[derive(Debug, Clone, Copy)]
pub struct AuthConfig {
    /// Which end this peer is (sets the send/verify order).
    pub role: AuthRole,
    /// The account this session is scoped to (named in our own auth frame).
    pub account_id: [u8; 32],
    /// Our own transport node id (bound into the binding we present).
    pub local_node: [u8; 32],
    /// The peer's iroh-authenticated transport node id (checked against the binding it presents).
    pub remote_node: [u8; 32],
    /// How we admit the peer.
    pub policy: AuthPolicy,
    /// The CURRENT wall-clock (ms) for this handshake — used to stamp the binding we mint and to
    /// check the peer's binding freshness. Per-handshake, NOT a store-construction timestamp: a
    /// reused store would otherwise mint stale bindings and never advance the replay window.
    pub now_ms: i64,
    /// How long we wait for the peer's auth frame before aborting.
    pub pre_auth_timeout: Duration,
}

/// The account-authorization capability the transport needs from the store: mint our own binding,
/// and verify a peer's. Both resolve against the store's account + live fold; kept separate from
/// [`crate::session::SyncStore`] so the auth phase is testable without the data-phase machinery.
pub trait NodeAuth {
    /// Our signed binding vouching that `local_node` is this account's local device, stamped
    /// `now_ms`. `Err` if this store cannot authorize (e.g. no local account/device yet — the
    /// onboarding case).
    fn local_binding(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<Vec<u8>>;

    /// The capability granted by `binding` to a peer whose iroh-authenticated node key is
    /// `remote_node`, judged fresh against `now_ms`. `None` is a uniform refusal; the store
    /// collapses its internal failure taxonomy so the wire cannot distinguish a malformed binding
    /// from a removed device. `Err` is reserved for a real fault (for example a failed DB read).
    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<Option<PeerCapability>>;
}

/// A node-authorization handshake that did not admit the connection.
#[derive(Debug)]
pub enum AuthError {
    /// The transport failed or the peer sent an unreadable frame.
    Codec(CodecError),
    /// The peer's binding did not satisfy our admission policy — the UNIFORM refusal (cause
    /// hidden).
    Unauthorized,
    /// The peer sent no auth frame within the pre-auth deadline.
    Timeout,
    /// The peer sent something other than an auth frame to open, or a policy we cannot serve.
    Protocol(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Codec(e) => write!(f, "sync auth transport: {e}"),
            AuthError::Unauthorized => write!(f, "peer is not authorized for this account"),
            AuthError::Timeout => write!(f, "peer sent no auth frame before the deadline"),
            AuthError::Protocol(m) => write!(f, "sync auth protocol violation: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Run the mutual auth handshake. On success, returns what the peer may do in the data phase; the
/// caller passes that capability to [`crate::session::run_session`] over the same stream. On `Err`
/// the caller drops the connection without revealing any inventory.
pub async fn run_auth_phase<W, R, A>(
    send: &mut W,
    recv: &mut R,
    auth: &A,
    cfg: AuthConfig,
) -> Result<PeerCapability, AuthError>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    A: NodeAuth,
{
    let capability = match cfg.role {
        // Acceptor verifies the dialer BEFORE revealing its own binding.
        AuthRole::Acceptor => {
            let capability = verify_peer(recv, auth, &cfg).await?;
            send_ours(send, auth, &cfg).await?;
            capability
        },
        // Dialer presents first (to the node it already authenticated), then verifies the acceptor
        // before proceeding to the data phase.
        AuthRole::Dialer => {
            send_ours(send, auth, &cfg).await?;
            verify_peer(recv, auth, &cfg).await?
        },
    };
    Ok(capability)
}

async fn send_ours<W: AsyncWrite + Unpin>(
    send: &mut W,
    auth: &dyn NodeAuth,
    cfg: &AuthConfig,
) -> Result<(), AuthError> {
    // A store that cannot mint a binding (no local account/device) cannot authorize — fail closed.
    let binding =
        auth.local_binding(&cfg.local_node, cfg.now_ms).map_err(|_| AuthError::Unauthorized)?;
    // Bound the WRITE by the same pre-auth deadline as the read: a peer that opens the stream but
    // never grants receive credit would otherwise hang this write forever, blocking the acceptor's
    // single-session accept slot — a pre-auth DoS.
    let frame = Frame::Auth { account_id: cfg.account_id, binding };
    match tokio::time::timeout(cfg.pre_auth_timeout, codec::write_frame(send, &frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(AuthError::Codec(e)),
        Err(_elapsed) => Err(AuthError::Timeout),
    }
}

async fn verify_peer<R: AsyncRead + Unpin>(
    recv: &mut R,
    auth: &dyn NodeAuth,
    cfg: &AuthConfig,
) -> Result<PeerCapability, AuthError> {
    let read = codec::read_frame_within(recv, MAX_AUTH_FRAME_BYTES);
    let frame = match tokio::time::timeout(cfg.pre_auth_timeout, read).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => return Err(AuthError::Codec(e)),
        Err(_elapsed) => return Err(AuthError::Timeout),
    };
    let Frame::Auth { account_id: peer_account, binding } = frame else {
        return Err(AuthError::Protocol("peer did not open with an auth frame".into()));
    };
    // Account scope is enforced regardless of policy — even an Open endpoint must not proceed to
    // the data phase (whose Hello reveals the hosted account id + inventory) for a peer that
    // named a DIFFERENT account. Open relaxes the BINDING check, never the scope. Uniform error
    // either way.
    if peer_account != cfg.account_id {
        return Err(AuthError::Unauthorized);
    }
    // Every rejection below is the SAME uniform error — no not-on-roster / bad-sig distinction on
    // the wire.
    match cfg.policy {
        // Open admission grants an unverified dialer READ, never WRITE. A fresh dialer cannot yet
        // verify the selected server's roster binding, so it must permit that acceptor to serve the
        // requested snapshot; ingest still verifies every entry from scratch.
        AuthPolicy::Open => {
            let unverified_peer_capability = match cfg.role {
                AuthRole::Dialer => PeerCapability::ReadWrite,
                AuthRole::Acceptor => PeerCapability::ReadOnly,
            };
            Ok(auth
                .authorize(&binding, &cfg.remote_node, cfg.now_ms)
                .ok()
                .flatten()
                .unwrap_or(unverified_peer_capability))
        },
        AuthPolicy::Closed => {
            match auth.authorize(&binding, &cfg.remote_node, cfg.now_ms) {
                Ok(Some(capability)) => Ok(capability),
                Ok(None) => Err(AuthError::Unauthorized),
                // A real fault (DB read failed) is not the same as a rejected peer, but it still
                // means we cannot admit — surface it uniformly rather than admitting on error.
                Err(_) => Err(AuthError::Unauthorized),
            }
        },
        AuthPolicy::InviteToken =>
            Err(AuthError::Protocol("invite-token admission is not supported yet".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCT: [u8; 32] = [1u8; 32];
    const D_NODE: [u8; 32] = [2u8; 32];
    const A_NODE: [u8; 32] = [3u8; 32];

    /// A fake authorizer that tests the PROTOCOL (ordering, mutual verification, uniform refusal)
    /// independently of the binding crypto (covered by the oplog `node_binding` tests).
    struct FakeAuth {
        binding: Vec<u8>,
        capability: Option<PeerCapability>,
    }

    impl NodeAuth for FakeAuth {
        fn local_binding(&self, _local_node: &[u8; 32], _now_ms: i64) -> anyhow::Result<Vec<u8>> {
            Ok(self.binding.clone())
        }

        fn authorize(
            &self,
            _binding: &[u8],
            _remote_node: &[u8; 32],
            _now_ms: i64,
        ) -> anyhow::Result<Option<PeerCapability>> {
            Ok(self.capability)
        }
    }

    async fn run_pair(
        dialer: FakeAuth,
        dialer_account: [u8; 32],
        dialer_policy: AuthPolicy,
        acceptor: FakeAuth,
        acceptor_policy: AuthPolicy,
    ) -> (Result<PeerCapability, AuthError>, Result<PeerCapability, AuthError>) {
        let (mut d_send, mut a_recv) = tokio::io::duplex(1 << 16);
        let (mut a_send, mut d_recv) = tokio::io::duplex(1 << 16);
        // Short: the happy path never waits, and a refusal leaves the peer's read pending (the
        // duplex send half is not dropped until this fn returns, unlike a real connection
        // that closes on abort), so a tight timeout keeps the failure-path tests fast.
        let timeout = Duration::from_millis(300);
        let dialer_side = run_auth_phase(&mut d_send, &mut d_recv, &dialer, AuthConfig {
            role: AuthRole::Dialer,
            account_id: dialer_account,
            local_node: D_NODE,
            remote_node: A_NODE,
            policy: dialer_policy,
            now_ms: 1,
            pre_auth_timeout: timeout,
        });
        let acceptor_side = run_auth_phase(&mut a_send, &mut a_recv, &acceptor, AuthConfig {
            role: AuthRole::Acceptor,
            account_id: ACCT,
            local_node: A_NODE,
            remote_node: D_NODE,
            policy: acceptor_policy,
            now_ms: 1,
            pre_auth_timeout: timeout,
        });
        tokio::join!(dialer_side, acceptor_side)
    }

    fn ok_auth() -> FakeAuth {
        FakeAuth { binding: vec![1, 2, 3], capability: Some(PeerCapability::ReadWrite) }
    }

    #[tokio::test]
    async fn mutual_closed_authorization_admits_both() {
        let (d, a) =
            run_pair(ok_auth(), ACCT, AuthPolicy::Closed, ok_auth(), AuthPolicy::Closed).await;
        assert_eq!(d.unwrap(), PeerCapability::ReadWrite);
        assert_eq!(a.unwrap(), PeerCapability::ReadWrite);
    }

    #[tokio::test]
    async fn closed_authorization_returns_the_peers_effective_capability() {
        let acceptor =
            FakeAuth { binding: vec![1, 2, 3], capability: Some(PeerCapability::ReadOnly) };
        let (dialer, acceptor) =
            run_pair(ok_auth(), ACCT, AuthPolicy::Closed, acceptor, AuthPolicy::Closed).await;
        assert_eq!(dialer.unwrap(), PeerCapability::ReadWrite);
        assert_eq!(acceptor.unwrap(), PeerCapability::ReadOnly);
    }

    #[tokio::test]
    async fn an_unauthorized_dialer_is_refused_before_the_acceptor_reveals_its_binding() {
        // The acceptor rejects the dialer. Because the acceptor verifies BEFORE sending its own
        // binding, it aborts without revealing anything — the dialer's read then fails (no Auth
        // frame arrives). This is the mutual-auth ordering that stops inventory (and here even the
        // acceptor's binding) leaking to an unauthorized peer.
        let acceptor = FakeAuth { binding: vec![9, 9, 9], capability: None };
        let (dialer, accept) =
            run_pair(ok_auth(), ACCT, AuthPolicy::Closed, acceptor, AuthPolicy::Closed).await;
        assert!(matches!(accept, Err(AuthError::Unauthorized)), "acceptor refused: {accept:?}");
        assert!(dialer.is_err(), "dialer got no acceptor binding — nothing leaked: {dialer:?}");
    }

    #[tokio::test]
    async fn open_policy_allows_the_selected_acceptor_to_serve_an_anonymous_dialer() {
        // Neither side can verify the other's binding yet. Open admits the dialer read-only, while
        // the dialer permits the server it explicitly selected to send the requested snapshot.
        let anon_dialer = FakeAuth { binding: vec![], capability: None };
        let acceptor = FakeAuth { binding: vec![1, 2, 3], capability: None };
        let (d, a) =
            run_pair(anon_dialer, ACCT, AuthPolicy::Open, acceptor, AuthPolicy::Open).await;
        assert_eq!(d.unwrap(), PeerCapability::ReadWrite);
        assert_eq!(a.unwrap(), PeerCapability::ReadOnly);
    }

    #[tokio::test]
    async fn a_dialer_naming_a_different_account_is_refused_under_closed() {
        // The dialer presents a binding scoped to a different account than the acceptor serves.
        let (_dialer, accept) = run_pair(
            ok_auth(),
            [0xee; 32], // dialer names a different account
            AuthPolicy::Closed,
            ok_auth(),
            AuthPolicy::Closed,
        )
        .await;
        assert!(
            matches!(accept, Err(AuthError::Unauthorized)),
            "the acceptor refuses a cross-account dialer: {accept:?}",
        );
    }

    #[tokio::test]
    async fn even_an_open_endpoint_refuses_a_cross_account_dialer() {
        // Open relaxes the binding check, NOT the account scope: a peer naming a different account
        // than the endpoint serves must be refused before run_session could leak the hosted
        // account's Hello + inventory.
        let (_dialer, accept) = run_pair(
            ok_auth(),
            [0xee; 32], // dialer names a different account
            AuthPolicy::Open,
            ok_auth(),
            AuthPolicy::Open,
        )
        .await;
        assert!(
            matches!(accept, Err(AuthError::Unauthorized)),
            "an open endpoint still enforces the account scope: {accept:?}",
        );
    }

    #[tokio::test]
    async fn invite_token_policy_fails_closed_until_implemented() {
        let (_d, a) =
            run_pair(ok_auth(), ACCT, AuthPolicy::Closed, ok_auth(), AuthPolicy::InviteToken).await;
        assert!(
            matches!(a, Err(AuthError::Protocol(_))),
            "invite-token is not admitted yet: {a:?}"
        );
    }

    #[tokio::test]
    async fn a_peer_that_sends_nothing_times_out() {
        // The write end stays alive but silent, so the acceptor's read pends and hits the deadline
        // (rather than seeing EOF).
        let (_silent_writer, mut recv) = tokio::io::duplex(1 << 10);
        let (mut send, _sink) = tokio::io::duplex(1 << 10);
        let r = run_auth_phase(&mut send, &mut recv, &ok_auth(), AuthConfig {
            role: AuthRole::Acceptor,
            account_id: ACCT,
            local_node: A_NODE,
            remote_node: D_NODE,
            policy: AuthPolicy::Closed,
            now_ms: 1,
            pre_auth_timeout: Duration::from_millis(100),
        })
        .await;
        assert!(matches!(r, Err(AuthError::Timeout)), "a silent peer times out: {r:?}");
    }

    #[tokio::test]
    async fn a_non_auth_first_frame_is_a_protocol_violation() {
        // The peer opens with a data-phase frame instead of an Auth frame.
        let (mut peer_send, mut recv) = tokio::io::duplex(1 << 12);
        let (mut send, _sink) = tokio::io::duplex(1 << 12);
        codec::write_frame(&mut peer_send, &Frame::Done).await.unwrap();
        let r = run_auth_phase(&mut send, &mut recv, &ok_auth(), AuthConfig {
            role: AuthRole::Acceptor,
            account_id: ACCT,
            local_node: A_NODE,
            remote_node: D_NODE,
            policy: AuthPolicy::Open,
            now_ms: 1,
            pre_auth_timeout: Duration::from_millis(200),
        })
        .await;
        assert!(matches!(r, Err(AuthError::Protocol(_))), "a non-auth opener is refused: {r:?}");
    }

    #[test]
    fn auth_errors_render() {
        // Every Display arm is reachable and non-empty.
        for e in [
            AuthError::Unauthorized,
            AuthError::Timeout,
            AuthError::Protocol("x".into()),
            AuthError::Codec(CodecError::Eof),
        ] {
            assert!(!format!("{e}").is_empty());
        }
    }
}
