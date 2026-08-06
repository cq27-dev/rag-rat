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
//! After both bindings verify, each side reports the capability it granted the other. A sender
//! intersects that remote grant with its own authority before revealing inventory or entries.
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

/// The two supported service modes for admitting a connection. Local policy, never negotiated on
/// the wire — an impostor cannot assert `Open` to exempt itself from the other side's `Closed`.
/// Per-ACCOUNT, not per-endpoint: one transport node may legitimately serve several accounts, so a
/// public account being `Open` must not open a private one on the same endpoint.
///
/// The mode governs TRANSPORT-LEVEL admission and the granted [`PeerCapability`] only; fold-level
/// WRITE AUTHORITY is always roster+role gated at INGEST, mode-independent. A transport `ReadWrite`
/// grant (including the `Open` bootstrap below) is capability, not authorship — every received
/// entry still passes the store's cryptographic + authority checks — so no mode can make an
/// unverified peer's entries authoritative. (Onboarding a not-yet-roster device is a separate
/// exchange on `ENROLL_ALPN`, not an admission mode of the data path.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPolicy {
    /// Admit any dialer for reads on the account/`/3` content paths. A valid roster binding
    /// determines its capability when available; a rejected binding remains read-only. Only a
    /// dialer whose local authority is unavailable permits its explicitly selected server to
    /// send the snapshot needed to restore roster state. Does NOT reach `/5` tables: the
    /// dispatchers (`dispatch_connection` and the multi-account `dispatch_connection_multi`)
    /// pin `TABLE_SYNC_ALPN` to `Closed` regardless of the configured policy, so a table
    /// manifest is never revealed to an unverified peer.
    Open,
    /// Admit only a peer whose binding verifies against this account's roster and the connection's
    /// authenticated remote node id. The default for a private account.
    Closed,
    /// Admit any dialer exactly like [`AuthPolicy::Open`], but SERVE a fallback-admitted
    /// (anonymous) reader only the account's authenticated PUBLIC material — the mode for a
    /// public knowledge base (#407). Admission is identical to `Open`; the difference is the
    /// serve scope the dispatcher derives from the admission outcome (a verified member still
    /// gets the full account; an anonymous reader gets
    /// [`crate::session::ServeScope::PublicOnly`]). Like `Open`, does NOT reach `/5` tables.
    PublicRead,
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

    fn restricted_by(self, grant: Self) -> Self {
        match (self, grant) {
            (Self::ReadWrite, Self::ReadWrite) => Self::ReadWrite,
            _ => Self::ReadOnly,
        }
    }
}

/// The local store's verdict for a peer binding. `Unavailable` is distinct from rejection so a
/// fresh Open dialer with no account authority yet can bootstrap from its selected server without
/// letting a peer gain capability by withholding or corrupting a binding we could validate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAuthorization {
    Granted(PeerCapability),
    Rejected,
    Unavailable,
}

/// How a peer was admitted: whether its OWN binding verified against the account roster, or it was
/// admitted only by policy fallback (the `Open`/`PublicRead` read-only bootstrap). A deliberately
/// granted read-only MEMBER and an anonymous reader both hold [`PeerCapability::ReadOnly`], so
/// capability alone cannot tell them apart — the serve scope for a public account depends on this
/// distinction (a member gets the full account; only a fallback reader is narrowed to public-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAdmission {
    /// The peer's binding verified against the roster — a member device.
    Verified,
    /// Admitted by policy without a verified binding (`Open`/`PublicRead` fallback).
    Fallback,
}

/// The binding this side presents and its locally-derived data-phase capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAuth {
    pub binding: Vec<u8>,
    pub capability: PeerCapability,
}

/// Directional data-phase capabilities established around mutual authentication. `local` is this
/// side's authority intersected with the remote peer's explicit grant; `peer` is the capability
/// this side granted after verifying the remote binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCapabilities {
    pub local: PeerCapability,
    pub peer: PeerCapability,
}

impl SessionCapabilities {
    pub const fn new(local: PeerCapability, peer: PeerCapability) -> Self {
        Self { local, peer }
    }

    pub const fn bidirectional() -> Self {
        Self::new(PeerCapability::ReadWrite, PeerCapability::ReadWrite)
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
    /// `now_ms`, plus the capability our current effective role permits us to exercise. A store
    /// with no effective local role presents an empty/invalid binding and remains read-only.
    fn local_auth(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<LocalAuth>;

    /// The verdict for `binding` from a peer whose iroh-authenticated node key is `remote_node`,
    /// judged fresh against `now_ms`. `Rejected` collapses the internal failure taxonomy so the
    /// wire cannot distinguish a malformed binding from a removed device. `Unavailable` means this
    /// store has no effective account authority yet. `Err` is reserved for a real storage fault.
    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<PeerAuthorization>;
}

/// A node-authorization handshake that did not admit the connection.
#[derive(Debug)]
pub enum AuthError {
    /// The transport failed or the peer sent an unreadable frame.
    Codec(CodecError),
    /// The peer's binding did not satisfy our admission policy — the UNIFORM refusal (cause
    /// hidden).
    Unauthorized,
    /// The peer sent no expected auth-phase frame within the pre-auth deadline.
    Timeout,
    /// The peer sent something other than an auth frame to open, or a policy we cannot serve.
    Protocol(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Codec(e) => write!(f, "sync auth transport: {e}"),
            AuthError::Unauthorized => write!(f, "peer is not authorized for this account"),
            AuthError::Timeout =>
                write!(f, "peer did not complete authentication before the deadline"),
            AuthError::Protocol(m) => write!(f, "sync auth protocol violation: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// A hosted account chosen for a connection by [`run_auth_phase_selected`], from the account the
/// dialer named in its opening auth frame. Carries that account's authorizer and its PER-ACCOUNT
/// policy — a public (`Open`) account and a private (`Closed`) one may share one endpoint, so the
/// mode must come from the selection, never an endpoint-wide default.
pub struct Selected<'a> {
    pub account_id: [u8; 32],
    pub auth: &'a dyn NodeAuth,
    pub policy: AuthPolicy,
}

/// Run the mutual auth handshake. On success, returns what each side may transmit in the data
/// phase; the caller passes those capabilities to [`crate::session::run_session`] over the same
/// stream. On `Err` the caller drops the connection without revealing any inventory.
pub async fn run_auth_phase<W, R, A>(
    send: &mut W,
    recv: &mut R,
    auth: &A,
    cfg: AuthConfig,
) -> Result<(SessionCapabilities, PeerAdmission), AuthError>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    A: NodeAuth,
{
    match cfg.role {
        // Single-account acceptor: admit iff the dialer named OUR one account. Expressed as the
        // degenerate one-account selector so the acceptor path is shared with multi-account
        // hosting.
        AuthRole::Acceptor => {
            let (_account, caps, admission) =
                run_auth_phase_selected(send, recv, cfg, |peer_account| {
                    (*peer_account == cfg.account_id).then_some(Selected {
                        account_id: cfg.account_id,
                        auth,
                        policy: cfg.policy,
                    })
                })
                .await?;
            Ok((caps, admission))
        },
        // Dialer presents first (to the node it already authenticated, naming its own account),
        // then verifies the acceptor before proceeding to the data phase. The returned admission
        // describes how THIS side admitted the acceptor (unused for serve scope — only an acceptor
        // serves — but returned for symmetry).
        AuthRole::Dialer => {
            let local = send_ours(send, auth, &cfg).await?;
            let (peer, admission) = verify_peer(recv, auth, &cfg).await?;
            let granted_by_peer = exchange_grants(send, recv, peer, cfg.pre_auth_timeout).await?;
            Ok((SessionCapabilities::new(local.restricted_by(granted_by_peer), peer), admission))
        },
    }
}

/// The acceptor half of the handshake for a host that serves a BOUNDED SET of accounts on one
/// endpoint. Reads the dialer's auth frame, hands the named account to `select` to choose the
/// hosted account (its authorizer + per-account policy), verifies the dialer's binding against THAT
/// account, then reveals the acceptor's own binding for it. Returns the selected account id so the
/// caller can route the data phase to that account's stores.
///
/// An account the host does not serve is refused with the SAME uniform [`AuthError::Unauthorized`]
/// as a rejected binding — no wire signal distinguishes "account not hosted here" from "not
/// authorized", so a peer cannot probe which accounts a host holds. No inventory (not even the
/// account confirmation in the acceptor's frame) is revealed before `select` and the binding check
/// succeed.
pub async fn run_auth_phase_selected<'sel, W, R, F>(
    send: &mut W,
    recv: &mut R,
    cfg: AuthConfig,
    select: F,
) -> Result<([u8; 32], SessionCapabilities, PeerAdmission), AuthError>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    F: FnOnce(&[u8; 32]) -> Option<Selected<'sel>>,
{
    let (peer_account, binding) = read_peer_auth(recv, cfg.pre_auth_timeout).await?;
    let Some(selected) = select(&peer_account) else {
        return Err(AuthError::Unauthorized);
    };
    let (peer, admission) = authorize_binding(
        selected.auth,
        &binding,
        selected.policy,
        AuthRole::Acceptor,
        &cfg.remote_node,
        cfg.now_ms,
    )?;
    // Mint our own frame for the SELECTED account (and under its policy), not the placeholder the
    // caller passed. Everything else in `cfg` (nodes, clock, timeout) is connection-level.
    let account_cfg =
        AuthConfig { account_id: selected.account_id, policy: selected.policy, ..cfg };
    let local = send_ours(send, selected.auth, &account_cfg).await?;
    let granted_by_peer = exchange_grants(send, recv, peer, cfg.pre_auth_timeout).await?;
    Ok((
        selected.account_id,
        SessionCapabilities::new(local.restricted_by(granted_by_peer), peer),
        admission,
    ))
}

async fn exchange_grants<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    send: &mut W,
    recv: &mut R,
    peer: PeerCapability,
    timeout: Duration,
) -> Result<PeerCapability, AuthError> {
    let (_, granted_by_peer) =
        tokio::try_join!(send_grant(send, peer, timeout), receive_grant(recv, timeout),)?;
    Ok(granted_by_peer)
}

async fn send_grant<W: AsyncWrite + Unpin>(
    send: &mut W,
    peer: PeerCapability,
    timeout: Duration,
) -> Result<(), AuthError> {
    let frame = Frame::AuthGrant { can_push: peer.can_push() };
    match tokio::time::timeout(timeout, codec::write_frame(send, &frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(AuthError::Codec(e)),
        Err(_elapsed) => Err(AuthError::Timeout),
    }
}

async fn receive_grant<R: AsyncRead + Unpin>(
    recv: &mut R,
    timeout: Duration,
) -> Result<PeerCapability, AuthError> {
    let read = codec::read_frame_within(recv, MAX_AUTH_FRAME_BYTES);
    let frame = match tokio::time::timeout(timeout, read).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => return Err(AuthError::Codec(e)),
        Err(_elapsed) => return Err(AuthError::Timeout),
    };
    match frame {
        Frame::AuthGrant { can_push: true } => Ok(PeerCapability::ReadWrite),
        Frame::AuthGrant { can_push: false } => Ok(PeerCapability::ReadOnly),
        _ => Err(AuthError::Protocol("peer did not follow auth with a capability grant".into())),
    }
}

async fn send_ours<W: AsyncWrite + Unpin>(
    send: &mut W,
    auth: &dyn NodeAuth,
    cfg: &AuthConfig,
) -> Result<PeerCapability, AuthError> {
    // Derive the binding and local send capability from one account snapshot. An anonymous or
    // locally removed device may still present an empty/invalid binding under Open, but remains
    // read-only so the session sender never uploads entries.
    let local =
        auth.local_auth(&cfg.local_node, cfg.now_ms).map_err(|_| AuthError::Unauthorized)?;
    // Bound the WRITE by the same pre-auth deadline as the read: a peer that opens the stream but
    // never grants receive credit would otherwise hang this write forever, blocking the acceptor's
    // single-session accept slot — a pre-auth DoS.
    let frame = Frame::Auth { account_id: cfg.account_id, binding: local.binding };
    match tokio::time::timeout(cfg.pre_auth_timeout, codec::write_frame(send, &frame)).await {
        Ok(Ok(())) => Ok(local.capability),
        Ok(Err(e)) => Err(AuthError::Codec(e)),
        Err(_elapsed) => Err(AuthError::Timeout),
    }
}

/// Read the peer's opening `Frame::Auth` off the stream, returning the account it named and the
/// binding it presented. Split out of [`verify_peer`] so the acceptor can SELECT which hosted
/// account to authorize against (multi-account hosting) between this read and the policy check,
/// rather than only equality-checking a pre-fixed account.
async fn read_peer_auth<R: AsyncRead + Unpin>(
    recv: &mut R,
    pre_auth_timeout: Duration,
) -> Result<([u8; 32], Vec<u8>), AuthError> {
    let read = codec::read_frame_within(recv, MAX_AUTH_FRAME_BYTES);
    let frame = match tokio::time::timeout(pre_auth_timeout, read).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => return Err(AuthError::Codec(e)),
        Err(_elapsed) => return Err(AuthError::Timeout),
    };
    let Frame::Auth { account_id, binding } = frame else {
        return Err(AuthError::Protocol("peer did not open with an auth frame".into()));
    };
    Ok((account_id, binding))
}

/// Policy verdict for a peer binding, against a CHOSEN account's `auth`. The account-scope decision
/// (equality for single-account, selection for multi-account) happens in the caller BEFORE this;
/// here only the binding is judged. Every rejection is the SAME uniform error — no
/// not-on-roster / bad-sig distinction on the wire.
fn authorize_binding(
    auth: &dyn NodeAuth,
    binding: &[u8],
    policy: AuthPolicy,
    role: AuthRole,
    remote_node: &[u8; 32],
    now_ms: i64,
) -> Result<(PeerCapability, PeerAdmission), AuthError> {
    match policy {
        // Open (and PublicRead — identical admission) grant an unverified dialer READ, never WRITE.
        // A fresh dialer cannot yet verify the selected server's roster binding, so it must
        // permit that acceptor to serve the requested snapshot; ingest still verifies every
        // entry from scratch. A verified binding is a member (`Verified`); anything
        // admitted without one is `Fallback`, which is what narrows a public account's
        // serve to public-only downstream.
        AuthPolicy::Open | AuthPolicy::PublicRead =>
            match auth.authorize(binding, remote_node, now_ms) {
                Ok(PeerAuthorization::Granted(capability)) =>
                    Ok((capability, PeerAdmission::Verified)),
                Ok(PeerAuthorization::Unavailable) if role == AuthRole::Dialer =>
                    Ok((PeerCapability::ReadWrite, PeerAdmission::Fallback)),
                Ok(PeerAuthorization::Rejected | PeerAuthorization::Unavailable) | Err(_) =>
                    Ok((PeerCapability::ReadOnly, PeerAdmission::Fallback)),
            },
        AuthPolicy::Closed => {
            match auth.authorize(binding, remote_node, now_ms) {
                Ok(PeerAuthorization::Granted(capability)) =>
                    Ok((capability, PeerAdmission::Verified)),
                Ok(PeerAuthorization::Rejected | PeerAuthorization::Unavailable) =>
                    Err(AuthError::Unauthorized),
                // A real fault (DB read failed) is not the same as a rejected peer, but it still
                // means we cannot admit — surface it uniformly rather than admitting on error.
                Err(_) => Err(AuthError::Unauthorized),
            }
        },
    }
}

async fn verify_peer<R: AsyncRead + Unpin>(
    recv: &mut R,
    auth: &dyn NodeAuth,
    cfg: &AuthConfig,
) -> Result<(PeerCapability, PeerAdmission), AuthError> {
    let (peer_account, binding) = read_peer_auth(recv, cfg.pre_auth_timeout).await?;
    // Account scope is enforced regardless of policy — even an Open endpoint must not proceed to
    // the data phase (whose Hello reveals the hosted account id + inventory) for a peer that
    // named a DIFFERENT account. Open relaxes the BINDING check, never the scope. Uniform error
    // either way.
    if peer_account != cfg.account_id {
        return Err(AuthError::Unauthorized);
    }
    authorize_binding(auth, &binding, cfg.policy, cfg.role, &cfg.remote_node, cfg.now_ms)
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
        local_capability: PeerCapability,
        authorization: PeerAuthorization,
    }

    impl NodeAuth for FakeAuth {
        fn local_auth(&self, _local_node: &[u8; 32], _now_ms: i64) -> anyhow::Result<LocalAuth> {
            Ok(LocalAuth { binding: self.binding.clone(), capability: self.local_capability })
        }

        fn authorize(
            &self,
            _binding: &[u8],
            _remote_node: &[u8; 32],
            _now_ms: i64,
        ) -> anyhow::Result<PeerAuthorization> {
            Ok(self.authorization)
        }
    }

    async fn run_pair(
        dialer: FakeAuth,
        dialer_account: [u8; 32],
        dialer_policy: AuthPolicy,
        acceptor: FakeAuth,
        acceptor_policy: AuthPolicy,
    ) -> (Result<SessionCapabilities, AuthError>, Result<SessionCapabilities, AuthError>) {
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
        // Drop the admission outcome so the existing assertions keep comparing
        // `SessionCapabilities` directly; the admission surface has its own test.
        let (dialer, acceptor) = tokio::join!(dialer_side, acceptor_side);
        (dialer.map(|(caps, _)| caps), acceptor.map(|(caps, _)| caps))
    }

    fn ok_auth() -> FakeAuth {
        FakeAuth {
            binding: vec![1, 2, 3],
            local_capability: PeerCapability::ReadWrite,
            authorization: PeerAuthorization::Granted(PeerCapability::ReadWrite),
        }
    }

    /// The admission outcome distinguishes a verified member from an anonymous fallback reader —
    /// the signal the public-serve scope derives from (#407 E2b). A granted binding is
    /// `Verified` under EVERY policy; a rejected binding is admitted `Fallback` read-only under
    /// `Open`/`PublicRead` and refused outright under `Closed`.
    #[test]
    fn admission_distinguishes_verified_from_fallback() {
        let rejected = FakeAuth {
            binding: Vec::new(),
            local_capability: PeerCapability::ReadOnly,
            authorization: PeerAuthorization::Rejected,
        };
        for policy in [AuthPolicy::Open, AuthPolicy::Closed, AuthPolicy::PublicRead] {
            let (cap, admission) =
                authorize_binding(&ok_auth(), &[1, 2, 3], policy, AuthRole::Acceptor, &D_NODE, 1)
                    .unwrap();
            assert_eq!(
                admission,
                PeerAdmission::Verified,
                "a granted binding is a member ({policy:?})"
            );
            assert_eq!(cap, PeerCapability::ReadWrite);
        }
        for policy in [AuthPolicy::Open, AuthPolicy::PublicRead] {
            let (cap, admission) =
                authorize_binding(&rejected, &[], policy, AuthRole::Acceptor, &D_NODE, 1).unwrap();
            assert_eq!(
                admission,
                PeerAdmission::Fallback,
                "a rejected binding is fallback ({policy:?})"
            );
            assert_eq!(cap, PeerCapability::ReadOnly, "fallback admits read-only");
        }
        assert!(
            authorize_binding(&rejected, &[], AuthPolicy::Closed, AuthRole::Acceptor, &D_NODE, 1)
                .is_err(),
            "Closed refuses a rejected binding outright",
        );
    }

    #[tokio::test]
    async fn mutual_closed_authorization_admits_both() {
        let (d, a) =
            run_pair(ok_auth(), ACCT, AuthPolicy::Closed, ok_auth(), AuthPolicy::Closed).await;
        assert_eq!(d.unwrap(), SessionCapabilities::bidirectional());
        assert_eq!(a.unwrap(), SessionCapabilities::bidirectional());
    }

    #[tokio::test]
    async fn closed_authorization_honors_the_capability_granted_by_the_peer() {
        let acceptor = FakeAuth {
            binding: vec![1, 2, 3],
            local_capability: PeerCapability::ReadWrite,
            authorization: PeerAuthorization::Granted(PeerCapability::ReadOnly),
        };
        let (dialer, acceptor) =
            run_pair(ok_auth(), ACCT, AuthPolicy::Closed, acceptor, AuthPolicy::Closed).await;
        assert_eq!(
            dialer.unwrap(),
            SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadWrite),
        );
        assert_eq!(
            acceptor.unwrap(),
            SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
        );
    }

    #[tokio::test]
    async fn an_unauthorized_dialer_is_refused_before_the_acceptor_reveals_its_binding() {
        // The acceptor rejects the dialer. Because the acceptor verifies BEFORE sending its own
        // binding, it aborts without revealing anything — the dialer's read then fails (no Auth
        // frame arrives). This is the mutual-auth ordering that stops inventory (and here even the
        // acceptor's binding) leaking to an unauthorized peer.
        let acceptor = FakeAuth {
            binding: vec![9, 9, 9],
            local_capability: PeerCapability::ReadWrite,
            authorization: PeerAuthorization::Rejected,
        };
        let (dialer, accept) =
            run_pair(ok_auth(), ACCT, AuthPolicy::Closed, acceptor, AuthPolicy::Closed).await;
        assert!(matches!(accept, Err(AuthError::Unauthorized)), "acceptor refused: {accept:?}");
        assert!(dialer.is_err(), "dialer got no acceptor binding — nothing leaked: {dialer:?}");
    }

    #[tokio::test]
    async fn open_policy_allows_the_selected_acceptor_to_serve_an_anonymous_dialer() {
        // Neither side can verify the other's binding yet. Open admits the dialer read-only, while
        // the dialer permits the server it explicitly selected to send the requested snapshot.
        let anon_dialer = FakeAuth {
            binding: vec![],
            local_capability: PeerCapability::ReadOnly,
            authorization: PeerAuthorization::Unavailable,
        };
        let acceptor = FakeAuth {
            binding: vec![1, 2, 3],
            local_capability: PeerCapability::ReadWrite,
            authorization: PeerAuthorization::Rejected,
        };
        let (d, a) =
            run_pair(anon_dialer, ACCT, AuthPolicy::Open, acceptor, AuthPolicy::Open).await;
        assert_eq!(
            d.unwrap(),
            SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadWrite),
        );
        assert_eq!(
            a.unwrap(),
            SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
        );
    }

    #[tokio::test]
    async fn open_policy_does_not_elevate_a_rejected_selected_acceptor() {
        let dialer = FakeAuth {
            binding: vec![4, 5, 6],
            local_capability: PeerCapability::ReadWrite,
            authorization: PeerAuthorization::Rejected,
        };
        let (dialer, acceptor) =
            run_pair(dialer, ACCT, AuthPolicy::Open, ok_auth(), AuthPolicy::Open).await;
        assert_eq!(
            dialer.unwrap(),
            SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
        );
        assert_eq!(
            acceptor.unwrap(),
            SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadWrite),
        );
    }

    #[tokio::test]
    async fn open_policy_honors_the_acceptors_read_only_grant_for_a_stale_writer() {
        let acceptor = FakeAuth {
            binding: vec![1, 2, 3],
            local_capability: PeerCapability::ReadWrite,
            authorization: PeerAuthorization::Rejected,
        };
        let (dialer, acceptor) =
            run_pair(ok_auth(), ACCT, AuthPolicy::Open, acceptor, AuthPolicy::Open).await;
        assert_eq!(
            dialer.unwrap(),
            SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadWrite),
        );
        assert_eq!(
            acceptor.unwrap(),
            SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
        );
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
    async fn a_peer_that_withholds_its_capability_grant_times_out() {
        let (mut peer_send, mut recv) = tokio::io::duplex(1 << 10);
        let (mut send, _peer_recv) = tokio::io::duplex(1 << 10);
        codec::write_frame(&mut peer_send, &Frame::Auth { account_id: ACCT, binding: vec![1] })
            .await
            .unwrap();
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
        assert!(matches!(r, Err(AuthError::Timeout)), "a withheld grant times out: {r:?}");
    }

    #[tokio::test]
    async fn a_non_grant_after_the_bindings_is_a_protocol_violation() {
        let (mut peer_send, mut recv) = tokio::io::duplex(1 << 10);
        let (mut send, _peer_recv) = tokio::io::duplex(1 << 10);
        codec::write_frame(&mut peer_send, &Frame::Auth { account_id: ACCT, binding: vec![1] })
            .await
            .unwrap();
        codec::write_frame(&mut peer_send, &Frame::Hello { account_id: ACCT, have: vec![] })
            .await
            .unwrap();
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
        assert!(matches!(r, Err(AuthError::Protocol(_))), "a non-grant is refused: {r:?}");
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

    const ACCT_B: [u8; 32] = [7u8; 32];

    /// Run the acceptor via the MULTI-account `run_auth_phase_selected` against `hosted`
    /// (account_id → its authorizer + per-account policy); the dialer names `dialer_account`.
    async fn run_selected_pair(
        dialer: FakeAuth,
        dialer_account: [u8; 32],
        hosted: Vec<([u8; 32], FakeAuth, AuthPolicy)>,
    ) -> (Result<SessionCapabilities, AuthError>, Result<([u8; 32], SessionCapabilities), AuthError>)
    {
        let (mut d_send, mut a_recv) = tokio::io::duplex(1 << 16);
        let (mut a_send, mut d_recv) = tokio::io::duplex(1 << 16);
        let timeout = Duration::from_millis(300);
        let dialer_side = run_auth_phase(&mut d_send, &mut d_recv, &dialer, AuthConfig {
            role: AuthRole::Dialer,
            account_id: dialer_account,
            local_node: D_NODE,
            remote_node: A_NODE,
            policy: AuthPolicy::Open,
            now_ms: 1,
            pre_auth_timeout: timeout,
        });
        let acceptor_side = run_auth_phase_selected(
            &mut a_send,
            &mut a_recv,
            AuthConfig {
                role: AuthRole::Acceptor,
                account_id: [0u8; 32],
                local_node: A_NODE,
                remote_node: D_NODE,
                policy: AuthPolicy::Closed,
                now_ms: 1,
                pre_auth_timeout: timeout,
            },
            |peer_account| {
                hosted
                    .iter()
                    .find(|(id, _, _)| id == peer_account)
                    .map(|(id, auth, policy)| Selected { account_id: *id, auth, policy: *policy })
            },
        );
        // Drop the admission outcome to keep the existing assertions comparing
        // capabilities/account.
        let (dialer, acceptor) = tokio::join!(dialer_side, acceptor_side);
        (dialer.map(|(caps, _)| caps), acceptor.map(|(account, caps, _)| (account, caps)))
    }

    #[tokio::test]
    async fn selects_the_hosted_account_the_dialer_names() {
        // A host holding both accounts admits a dialer for EACH, resolving to the one it named.
        for named in [ACCT, ACCT_B] {
            let (dialer, acceptor) = run_selected_pair(ok_auth(), named, vec![
                (ACCT, ok_auth(), AuthPolicy::Closed),
                (ACCT_B, ok_auth(), AuthPolicy::Closed),
            ])
            .await;
            let (selected, _caps) = acceptor.unwrap();
            assert_eq!(selected, named, "the session resolves to the account the dialer named");
            assert!(dialer.is_ok());
        }
    }

    #[tokio::test]
    async fn refuses_an_unhosted_account_with_the_uniform_error() {
        // A dialer naming an account the host does not serve gets the SAME error as a rejected
        // binding — no signal reveals which accounts are hosted.
        let unhosted = [0x9u8; 32];
        let (_dialer, acceptor) =
            run_selected_pair(ok_auth(), unhosted, vec![(ACCT, ok_auth(), AuthPolicy::Closed)])
                .await;
        assert!(
            matches!(acceptor, Err(AuthError::Unauthorized)),
            "an unhosted account is refused uniformly: {acceptor:?}"
        );
    }

    #[tokio::test]
    async fn honors_the_selected_accounts_policy_not_an_endpoint_wide_one() {
        // The selected account is Open, so an unverified dialer is admitted READ-ONLY — even though
        // the acceptor's config policy is Closed. The policy comes from the SELECTION.
        let unverified_at_acceptor = FakeAuth {
            binding: vec![],
            local_capability: PeerCapability::ReadWrite,
            authorization: PeerAuthorization::Rejected,
        };
        let (_dialer, acceptor) = run_selected_pair(ok_auth(), ACCT, vec![(
            ACCT,
            unverified_at_acceptor,
            AuthPolicy::Open,
        )])
        .await;
        let (selected, caps) = acceptor.unwrap();
        assert_eq!(selected, ACCT);
        assert_eq!(
            caps.peer,
            PeerCapability::ReadOnly,
            "Open admits the unverified dialer read-only; a Closed selection would have refused it"
        );
    }
}
