//! Routing an accepted connection to the session its ALPN names, for one account or a hosted
//! set of them.

use iroh::Endpoint;
use iroh::endpoint::Connection as IrohConnection;
use rag_rat_oplog::AccountId;
use strum::IntoEnumIterator;
use tokio::time::timeout;

use super::accept::{GRACEFUL_CLOSE_TIMEOUT, GlobalEgressLimiter, accept_connection};
use super::addr::EndpointError;
use super::enroll::{enrollment_database_matches, finish_enrollment_stream};
use crate::auth::{
    AuthConfig, AuthPolicy, AuthRole, DEFAULT_PRE_AUTH_TIMEOUT, PeerAdmission, Selected,
    SessionCapabilities, run_auth_phase, run_auth_phase_selected,
};
use crate::enrollment::{
    ENROLL_ALPN, EnrollmentAcceptorOutcome, InviteError, run_enrollment_acceptor,
};
use crate::session::{
    DEFAULT_IDLE_TIMEOUT, MAX_SESSION_ENTRIES, ServeScope, SessionError, SessionLimits,
    SessionReport, SyncStore, run_session_limited,
};
use crate::store::{OplogContentSyncStore, OplogSyncStore};
use crate::table_session::{TableSessionError, run_table_session};
use crate::table_wire::TABLE_SYNC_ALPN;
use crate::wire::{CONTENT_SYNC_ALPN, SYNC_ALPN};

/// A connection-stage failure: dialing, accepting, or opening a stream.
pub(super) fn connect_failed(message: impl Into<String>) -> SyncFailure {
    SyncFailure::Endpoint(EndpointError::Connect(message.into()))
}

/// Accept ONE inbound connection and run the session for the STREAM the peer negotiated: the
/// account log ([`SYNC_ALPN`] → `account_store`), the content lane ([`CONTENT_SYNC_ALPN`] →
/// `content_store`), the repo-scoped table lane ([`TABLE_SYNC_ALPN`]), or owner-side enrollment
/// ([`ENROLL_ALPN`] → the account store's database).
/// The auth phase is account-level for normal sync; enrollment instead authenticates the requested
/// node by the QUIC transport identity and atomically adds it to the roster before normal auth can
/// admit it.
pub async fn accept_and_dispatch<C>(
    endpoint: &Endpoint,
    account_store: &mut OplogSyncStore<'_>,
    content_store: &mut C,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64 + Copy,
) -> Result<(SyncAlpn, SessionReport), SyncFailure>
where
    C: SyncStore,
{
    let local_node = *endpoint.id().as_bytes();
    let conn = accept_connection(endpoint).await?;
    // Unmetered convenience/test wrapper — the live serve loops (resident host, `sync serve`) call
    // `dispatch_connection` directly with the shared egress limiter.
    dispatch_connection(conn, local_node, account_store, content_store, policy, now_ms, None).await
}

/// The serve scope an acceptor grants a peer (#407 E2b): narrow to [`ServeScope::PublicOnly`] iff a
/// `PublicRead` account admitted this peer by FALLBACK — i.e. an anonymous reader with no verified
/// binding. A verified member of a public account, and every `Open`/`Closed` session, serves the
/// full account. Derived from the SELECTED per-ALPN policy (tables are pinned `Closed`, so they
/// never reach public-only) and the auth admission outcome.
pub(super) fn serve_scope_for(policy: AuthPolicy, admission: PeerAdmission) -> ServeScope {
    match (policy, admission) {
        (AuthPolicy::PublicRead, PeerAdmission::Fallback) => ServeScope::PublicOnly,
        _ => ServeScope::Full,
    }
}

/// The streams this endpoint binds, named by the ALPN a connection negotiates. The byte values
/// are the frozen ALPN constants; this type is how dialers, dispatchers and their callers name a
/// stream without comparing bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum SyncAlpn {
    /// The account log, [`SYNC_ALPN`].
    Account,
    /// The content lane, [`CONTENT_SYNC_ALPN`].
    Content,
    /// The table lane, [`TABLE_SYNC_ALPN`].
    Table,
    /// Owner-side enrollment, [`ENROLL_ALPN`].
    Enroll,
}

impl SyncAlpn {
    /// The ALPN bytes this stream negotiates.
    pub fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Account => SYNC_ALPN,
            Self::Content => CONTENT_SYNC_ALPN,
            Self::Table => TABLE_SYNC_ALPN,
            Self::Enroll => ENROLL_ALPN,
        }
    }

    /// The lane name a dial's timeout messages carry; only the table lane names itself.
    pub(super) fn dial_label(self) -> &'static str {
        match self {
            Self::Table => "table-sync ",
            Self::Account | Self::Content | Self::Enroll => "",
        }
    }
}

impl TryFrom<&[u8]> for SyncAlpn {
    type Error = ();

    fn try_from(alpn: &[u8]) -> Result<Self, ()> {
        Self::iter().find(|stream| stream.as_bytes() == alpn).ok_or(())
    }
}

/// A connection that passed node authorization, ready for the session its ALPN names.
struct AuthorizedStream<'c> {
    conn: &'c IrohConnection,
    stream: SyncAlpn,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    capabilities: SessionCapabilities,
    admission: PeerAdmission,
    /// The admission policy this stream ran under (tables pinned `Closed`).
    policy: AuthPolicy,
}

/// The shared tail of both dispatchers: narrow the serve scope, run the session the negotiated
/// stream names against its store, then hold the connection until the dialer closes.
async fn run_dispatched<C: SyncStore>(
    authorized: AuthorizedStream<'_>,
    account_store: &mut OplogSyncStore<'_>,
    content_store: &mut C,
    now_ms: impl Fn() -> i64 + Copy,
    egress: Option<std::sync::Arc<std::sync::Mutex<GlobalEgressLimiter>>>,
) -> Result<SessionReport, SyncFailure> {
    let AuthorizedStream { conn, stream, send, recv, capabilities, admission, policy } = authorized;
    // Narrow the serve to public-only for an anonymous (fallback-admitted) reader of a `PublicRead`
    // account (#407); a verified member — or any Open/Closed session — serves the full account. Set
    // on both stores; only the ALPN's store actually serves, and the store re-checks fully-public.
    let scope = serve_scope_for(policy, admission);
    account_store.set_serve_scope(scope);
    content_store.set_serve_scope(scope);
    // The account-log and content lanes are the anonymous-servable paths, so their egress is
    // metered against the shared budget. Table sync is pinned `Closed` (unreachable by an
    // anonymous peer), so it carries no anonymous egress and is left unmetered here.
    let limits = SessionLimits {
        idle_timeout: DEFAULT_IDLE_TIMEOUT,
        egress,
        now_ms,
        entries_per_session: MAX_SESSION_ENTRIES,
    };
    let report = match stream {
        SyncAlpn::Account =>
            run_session_limited(account_store, send, recv, AuthRole::Acceptor, capabilities, limits)
                .await
                .map_err(SyncFailure::Session)?,
        SyncAlpn::Content =>
            run_session_limited(content_store, send, recv, AuthRole::Acceptor, capabilities, limits)
                .await
                .map_err(SyncFailure::Session)?,
        SyncAlpn::Table => {
            let mut table_store = crate::store::OplogTableSyncStore::new(
                account_store.connection(),
                AccountId::from_bytes(account_store.account_id()),
                now_ms,
            );
            let table =
                run_table_session(&mut table_store, send, recv, AuthRole::Acceptor, capabilities)
                    .await
                    .map_err(SyncFailure::TableSession)?;
            SessionReport {
                entries_sent: table.entries_sent,
                entries_received: table.entries_received,
                entries_newly_stored: table.entries_newly_stored,
                peer_capability: capabilities.peer,
            }
        },
        // Enrollment is its own exchange, routed (or refused) before authorization.
        SyncAlpn::Enroll => return Err(connect_failed("enrollment has no sync session")),
    };
    // Keep the acceptor alive until the dialer reads its final acknowledgement and closes.
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    Ok(report)
}

/// Run the ALPN-selected sync session for an already-accepted connection.
pub async fn dispatch_connection<C>(
    conn: IrohConnection,
    local_node: [u8; 32],
    account_store: &mut OplogSyncStore<'_>,
    content_store: &mut C,
    policy: AuthPolicy,
    now_ms: impl Fn() -> i64 + Copy,
    egress: Option<std::sync::Arc<std::sync::Mutex<GlobalEgressLimiter>>>,
) -> Result<(SyncAlpn, SessionReport), SyncFailure>
where
    C: SyncStore,
{
    // The two stores MUST be for the same account: the auth phase authorizes the peer against the
    // account store's account, and a content connection then runs the content store — which would
    // serve the WRONG account's content if they differed. Our callers always pass same-account
    // stores; enforce it for the public generic API before any connection is accepted.
    if account_store.account_id() != content_store.account_id() {
        return Err(connect_failed("account and content stores are for different accounts"));
    }
    let remote_node = *conn.remote_id().as_bytes();
    let alpn = conn.alpn().to_vec();
    // Reject an unroutable ALPN BEFORE opening a stream or running auth — there is no reason to
    // complete a mutual handshake for a stream we can't serve. Unreachable today (iroh's TLS
    // refuses any ALPN `build_endpoint` didn't bind, and it binds exactly the routed ones), but
    // keeping the check ahead of auth means adding another bound ALPN without a route here
    // fails cleanly here instead of after the peer has completed authorization.
    let Ok(stream) = SyncAlpn::try_from(alpn.as_slice()) else {
        conn.close(0u32.into(), b"unknown-alpn");
        return Err(connect_failed(format!("peer negotiated an unknown ALPN {alpn:?}")));
    };
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| connect_failed("peer opened no stream"))?
        .map_err(|e| connect_failed(e.to_string()))?;
    if stream == SyncAlpn::Enroll {
        let enrollment_database = account_store.connection();
        // The acceptor consumes one of the enrollment database's OWN invites and authors the
        // DeviceAdd into ITS account, so unless that database's local account is exactly the one
        // the sync stores serve, a miswired dispatcher would enroll the device into an unrelated
        // account — and report that as a successful enrollment — while sync connections keep
        // serving the stores' account. Refuse BEFORE redemption (the irreversible boundary).
        let matches = enrollment_database_matches(enrollment_database, account_store.account_id())
            .map_err(|error| connect_failed(error.to_string()))?;
        if !matches {
            conn.close(0u32.into(), b"enrollment-account-mismatch");
            return Err(connect_failed(
                "enrollment database belongs to a different account than the sync stores",
            ));
        }
        let outcome =
            run_enrollment_acceptor(&mut recv, &mut send, enrollment_database, remote_node, now_ms)
                .await
                .map_err(SyncFailure::Enrollment)?;
        finish_enrollment_stream(conn, &mut recv, &outcome).await;
        return match outcome {
            EnrollmentAcceptorOutcome::Enrolled(_, _)
            | EnrollmentAcceptorOutcome::WriterGranted(_) => Ok((stream, SessionReport::default())),
            EnrollmentAcceptorOutcome::Refused(error) => Err(SyncFailure::Enrollment(error)),
        };
    }
    // Read the clock only now that a peer has connected (see `accept_and_sync`).
    let auth_now_ms = now_ms();
    // Table streams are private account data. Open/bootstrap/public admission is only for the
    // account + content paths; a table manifest is never revealed to an unverified peer.
    let alpn_policy = if stream == SyncAlpn::Table { AuthPolicy::Closed } else { policy };
    // The auth phase is store-agnostic (the binding is account-level), so authorize with the
    // account store BEFORE any inventory — no stream leaves this peer until it passes the policy.
    let (capabilities, admission) =
        run_auth_phase(&mut send, &mut recv, &*account_store, AuthConfig {
            role: AuthRole::Acceptor,
            account_id: account_store.account_id(),
            local_node,
            remote_node,
            policy: alpn_policy,
            now_ms: auth_now_ms,
            pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
        })
        .await
        .map_err(SyncFailure::Auth)?;
    let authorized = AuthorizedStream {
        conn: &conn,
        stream,
        send,
        recv,
        capabilities,
        admission,
        policy: alpn_policy,
    };
    let report = run_dispatched(authorized, account_store, content_store, now_ms, egress).await?;
    Ok((stream, report))
}

/// One account a multi-account host serves: the account-log + content stores (BOTH for that one
/// account) and its per-account admission [`AuthPolicy`]. A host holds a bounded slice of these; a
/// connection selects one by the account the dialer names.
///
/// Constructed only through [`HostedAccount::new`], which REFUSES a `sync`/`content` pair for
/// different accounts — the same invariant [`dispatch_connection`] enforces at runtime, lifted to
/// construction so a misaligned pair (a content store for account B behind account A's log) is
/// unrepresentable and can never serve one account's content to another's authenticated peer. The
/// caller is responsible for passing DISTINCT accounts in the hosted slice; a duplicate account id
/// is served by its first entry.
pub struct HostedAccount<'a> {
    sync: OplogSyncStore<'a>,
    content: OplogContentSyncStore<'a>,
    policy: AuthPolicy,
}

impl<'a> HostedAccount<'a> {
    /// Bind an account's `sync` + `content` stores and its admission `policy` for hosting. Errors
    /// if the two stores are for different accounts — the cross-account-content isolation
    /// guard.
    pub fn new(
        sync: OplogSyncStore<'a>,
        content: OplogContentSyncStore<'a>,
        policy: AuthPolicy,
    ) -> Result<Self, SyncFailure> {
        if sync.account_id() != content.account_id() {
            return Err(connect_failed("account and content stores are for different accounts"));
        }
        Ok(Self { sync, content, policy })
    }
}

/// Accept ONE inbound connection and run its session for whichever of the host's BOUNDED SET of
/// `accounts` the dialer names in its auth frame — one endpoint fronting N accounts (one store pair
/// each). The dialer's named account selects the store pair, then the negotiated ALPN routes
/// exactly as [`dispatch_connection`] does for the single-account case.
///
/// Isolation: the selected account's own stores serve the session, and every store rejects a
/// foreign account's entries at ingest, so a session for one account never reads or writes
/// another's. An account the host does not serve — or a peer that fails the selected account's
/// policy — is refused with the SAME uniform [`AuthError`](crate::AuthError) as a rejected binding,
/// so a peer cannot probe which accounts a host holds. No inventory leaves the host before
/// selection + the binding check succeed.
///
/// Per-account policy is honored (a public `Open` account and a private `Closed` one may share the
/// endpoint); table streams are always served under `Closed` regardless of the account's mode.
/// `ENROLL_ALPN` is refused here: enrollment names its account out-of-band before any auth frame,
/// and a host onboards accounts as the DIALER, not by accepting enrollment across its hosted set.
pub async fn dispatch_connection_multi(
    conn: IrohConnection,
    local_node: [u8; 32],
    accounts: &mut [HostedAccount<'_>],
    now_ms: impl Fn() -> i64 + Copy,
    egress: Option<std::sync::Arc<std::sync::Mutex<GlobalEgressLimiter>>>,
) -> Result<(SyncAlpn, SessionReport), SyncFailure> {
    let remote_node = *conn.remote_id().as_bytes();
    let alpn = conn.alpn().to_vec();
    // The multi host serves the account-log, content, and table streams. ENROLL_ALPN (and any
    // unbound ALPN) is refused BEFORE a stream opens — no route, no handshake.
    let stream = match SyncAlpn::try_from(alpn.as_slice()) {
        Ok(stream @ (SyncAlpn::Account | SyncAlpn::Content | SyncAlpn::Table)) => stream,
        Ok(SyncAlpn::Enroll) | Err(()) => {
            conn.close(0u32.into(), b"unknown-alpn");
            return Err(connect_failed(format!("multi-account host does not serve ALPN {alpn:?}")));
        },
    };
    let (mut send, mut recv) = timeout(DEFAULT_IDLE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| connect_failed("peer opened no stream"))?
        .map_err(|e| connect_failed(e.to_string()))?;
    // Read the clock only now that a peer has connected (see `accept_and_sync`).
    let auth_now_ms = now_ms();
    let table_alpn = stream == SyncAlpn::Table;
    // Authorize FIRST, selecting the account from the dialer's frame — no inventory (not even which
    // account is served) leaves the host until selection + the binding check pass. `account_id` and
    // `policy` in the config are placeholders the selection overrides.
    let (selected_account, capabilities, admission) = run_auth_phase_selected(
        &mut send,
        &mut recv,
        AuthConfig {
            role: AuthRole::Acceptor,
            account_id: [0u8; 32],
            local_node,
            remote_node,
            policy: AuthPolicy::Closed,
            now_ms: auth_now_ms,
            pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
        },
        |peer_account| {
            accounts.iter().find(|account| account.sync.account_id() == *peer_account).map(
                |account| Selected {
                    account_id: *peer_account,
                    auth: &account.sync,
                    // Table streams are private account data — never Open, whatever the account's
                    // mode.
                    policy: if table_alpn { AuthPolicy::Closed } else { account.policy },
                },
            )
        },
    )
    .await
    .map_err(SyncFailure::Auth)?;
    // The selector's shared borrow has ended; take the selected store pair mutably for the session.
    let account = accounts
        .iter_mut()
        .find(|account| account.sync.account_id() == selected_account)
        .expect("run_auth_phase_selected returns only an account the selector accepted");
    // The serve scope comes from the selected account's policy under this ALPN — tables pinned
    // `Closed` — same as the single-account dispatcher.
    let alpn_policy = if table_alpn { AuthPolicy::Closed } else { account.policy };
    let authorized = AuthorizedStream {
        conn: &conn,
        stream,
        send,
        recv,
        capabilities,
        admission,
        policy: alpn_policy,
    };
    let report =
        run_dispatched(authorized, &mut account.sync, &mut account.content, now_ms, egress).await?;
    Ok((stream, report))
}

/// A sync attempt that failed setting up the connection, authorizing the peer, or running the
/// session.
#[derive(Debug, thiserror::Error)]
pub enum SyncFailure {
    #[error(transparent)]
    Endpoint(EndpointError),
    /// The dedicated owner-side enrollment exchange failed.
    #[error(transparent)]
    Enrollment(InviteError),
    /// The node-authorization handshake refused the peer (or we could not authorize to it).
    #[error(transparent)]
    Auth(crate::auth::AuthError),
    #[error(transparent)]
    Session(SessionError),
    #[error(transparent)]
    TableSession(TableSessionError),
}
