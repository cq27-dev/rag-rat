//! The symmetric sync session (phase D, #406).
//!
//! One protocol for both roles. Each peer sends [`Frame::Hello`] with the account it is syncing and
//! every account-log entry hash it holds. A write-capable side streams the entries the other lacks;
//! a read-only side suppresses that automatic upload, and both end with [`Frame::Done`]. The two
//! directions run concurrently over one bidirectional stream, so a large transfer in one direction
//! never blocks the other (the deadlock a send-then-receive ordering would cause on a bounded
//! stream). After both data directions finish, a role-ordered
//! [`Frame::Ack`] exchange proves both receivers consumed the complete peer stream before the
//! dialer closes.
//!
//! The session is transport-agnostic — generic over any [`AsyncRead`]/[`AsyncWrite`] pair — and
//! trusts nothing it receives: entries from a read-only peer are refused before ingest; every entry
//! from a read-write peer is handed to [`SyncStore::ingest`], which re-verifies it from scratch. It
//! is deliberately NOT `Send`-bound: [`SyncStore`] wraps a SQLite connection, so a caller runs one
//! session at a time on a single task (concurrent sessions are a later slice).

use std::collections::HashSet;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::auth::{self, AuthRole, SessionCapabilities};
use crate::codec::{self, CodecError};
use crate::wire::{Frame, MAX_ENTRIES_PAGE_BYTES, MAX_ENTRIES_PER_PAGE, MAX_HELLO_HASHES};

type Hash = [u8; 32];

/// How long the receiver waits for the peer's next frame before aborting the session as idle. A
/// peer that connects and never sends, or stalls mid-stream, would otherwise hold the (single-
/// session) server forever, blocking every later peer. Generous — a slow but progressing transfer
/// resets it on each frame — while still bounding a silent connection.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The most entries one session will accept from a peer before aborting. A legitimate transfer for
/// one account is bounded by that account's stored + parked capacity (a few thousand); this cap is
/// far above that so an honest sync never hits it, while still bounding a peer that streams
/// redelivered or junk entries forever. Paired with the empty-page rejection below, it turns "the
/// receive loop runs until Done" into a bounded transfer, not an open-ended one a peer can hold
/// open (#406: bounded frames, no amplification).
pub const MAX_SESSION_ENTRIES: usize = 1_000_000;

/// Cap the outgoing hello inventory to what the wire allows the peer to decode.
///
/// Advertising a SUBSET of what we hold is always correct, only ever less efficient: the peer sends
/// every entry it has that is not in the advertised set — which, past the cap, includes some
/// entries we already hold, and re-ingesting a held entry is an idempotent no-op. So an account
/// with more than [`MAX_HELLO_HASHES`] entries still converges to the union; it just pays some
/// redundant transfer. This deliberately avoids a "remainder reconcile" protocol: correctness does
/// not need one, and the accounts D targets stay well under the cap regardless.
fn bounded_inventory(hashes: impl Iterator<Item = Hash>) -> Vec<Hash> {
    hashes.take(MAX_HELLO_HASHES).collect()
}

/// How much of a store's data a session may SERVE to the connected peer (#407 E2b). Orthogonal to
/// `PeerCapability`, which only governs whether the peer may PUSH — this governs what the acceptor
/// OFFERS. Defaults to [`ServeScope::Full`] on a freshly constructed store; a dispatcher narrows it
/// to [`ServeScope::PublicOnly`] AFTER auth, before the session reads the snapshot, for an
/// anonymous (fallback-admitted) reader of a `public_read` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServeScope {
    /// The whole account: every held entry including unauthenticated parked candidates. Members,
    /// `Open` restore, and `Closed` sessions.
    #[default]
    Full,
    /// Only AUTHENTICATED, publicly-readable material — no parked/pre-verify candidates. The scope
    /// an anonymous peer receives from a fully-public account.
    PublicOnly,
}

/// The store side of a session: what a peer offers and where received entries land. Implemented
/// over the op log for production and over an in-memory map for tests.
pub trait SyncStore {
    /// The account this session is scoped to. A peer whose hello names a different account is a
    /// misdirected connection and the session aborts.
    fn account_id(&self) -> Hash;

    /// Narrow (or restore) how much this store SERVES for the rest of the session — see
    /// [`ServeScope`]. Called by the dispatcher after auth and before the snapshot is read. No
    /// default: every impl must decide, so a new store can never silently serve `Full` to an
    /// anonymous peer (fail-closed by construction). A store that only ever serves itself/members
    /// may implement it as a no-op.
    fn set_serve_scope(&mut self, scope: ServeScope);

    /// Every held account-log entry as `(dedup_key, signed_bytes)`, read ONCE at session start. The
    /// key is the SIGNED-envelope hash (`sha256(signed_bytes)`), NOT the entry_hash — two envelopes
    /// can share an entry_hash but differ in signature, and the wire must treat them as distinct or
    /// a peer holding one would suppress the other. Snapshotting up front keeps what we send
    /// independent of what we concurrently ingest, so the two session halves never contend.
    fn snapshot(&self) -> anyhow::Result<Vec<(Hash, Vec<u8>)>>;

    /// Ingest one received entry's `signed_bytes`. Must be idempotent (re-ingesting a held entry is
    /// a no-op) and must re-verify — the bytes came off the wire from an untrusted peer.
    fn ingest(&mut self, signed_bytes: &[u8]) -> anyhow::Result<Ingested>;
}

/// Whether an ingested entry was newly stored, so a session can report real transfer versus
/// redelivery without the store leaking its verdict taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingested {
    /// The entry was accepted into the store (or durably parked pending its signer).
    Stored,
    /// Already held, or refused by verification — either way nothing new landed.
    NoChange,
}

/// What one session moved. Symmetric: each peer both sends and receives.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionReport {
    pub entries_sent: usize,
    pub entries_received: usize,
    pub entries_newly_stored: usize,
    /// What this side granted the REMOTE. When `ReadOnly`, receive rejects the peer's entries, so
    /// a session that exists to RECEIVE (a pull) transferred nothing and its all-quiet round
    /// means "structurally unable to receive", not "in sync".
    pub peer_capability: crate::auth::PeerCapability,
}

/// A session that could not complete.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The transport failed or the peer sent an unreadable frame.
    #[error("sync session transport: {0}")]
    Codec(#[from] CodecError),
    /// The peer opened with something other than a hello, or named a different account.
    #[error("sync session protocol violation: {0}")]
    Protocol(String),
    /// The peer made no progress within the idle window: it sent no frame, took none of ours, or
    /// did not let the stream close. Distinct from [`SessionError::Protocol`] — a silent peer
    /// violated nothing, and a caller can tell a stall from a malformed frame.
    #[error("sync session peer made no progress within {after:?}")]
    Timeout { after: Duration },
    /// Authentication admitted the peer for reads, but it attempted to push entries.
    #[error("read-only peer attempted to push sync entries")]
    UnauthorizedPush,
    /// Reading the local entry snapshot failed.
    #[error("sync session store: {0}")]
    Store(anyhow::Error),
}

/// Run one session to completion over `send`/`recv`, syncing account entries with the peer while
/// enforcing the directional capabilities returned by the preceding auth phase.
///
/// Both halves run under `join!` on the current task — no spawn, so `store` (and its SQLite
/// connection) need not be `Send`. The sender owns an up-front snapshot of local entries; the
/// receiver holds `&mut store` to ingest. Because the sender reads only the snapshot, the two never
/// alias the store.
pub async fn run_session<S, R, W>(
    store: &mut S,
    send: W,
    recv: R,
    role: AuthRole,
    capabilities: SessionCapabilities,
) -> Result<SessionReport, SessionError>
where
    S: SyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    run_session_limited(store, send, recv, role, capabilities, SessionLimits::default()).await
}

/// The per-session serving policy of [`run_session_limited`]. The default is the unmetered
/// session [`run_session`] runs: the default idle timeout and no egress cap.
pub struct SessionLimits<F = fn() -> i64> {
    /// The receiver aborts if the peer sends no frame within this window. Tests shorten it to
    /// exercise the timeout without waiting the default.
    pub idle_timeout: Duration,
    /// The shared GLOBAL egress cap. `None` leaves the session unmetered (the dialer paths and
    /// tests).
    pub egress: Option<std::sync::Arc<std::sync::Mutex<crate::GlobalEgressLimiter>>>,
    /// The clock the egress budget refills against; read only when `egress` is `Some`. The
    /// `Default` clock always answers 0, so a caller that sets `egress` must set this too —
    /// `SessionLimits { egress: Some(..), ..Default::default() }` would meter against the epoch.
    pub now_ms: F,
    /// The most entries this session will ACCEPT from the peer before aborting. Defaults to
    /// [`MAX_SESSION_ENTRIES`]; injectable so a test can reach the ceiling without streaming a
    /// million entries, the same seam the table lane carries as `entries_per_session`.
    pub entries_per_session: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            egress: None,
            now_ms: || 0,
            entries_per_session: MAX_SESSION_ENTRIES,
        }
    }
}

/// [`run_session`] under explicit [`SessionLimits`]. When `limits.egress` is `Some`, each outgoing
/// entries page is charged against the shared [`crate::GlobalEgressLimiter`], and the sender STOPS
/// after the last page the budget allowed (sending `Done` early) — the withheld tail is re-offered
/// by the next session's inventory diff, so a throttled reader converges across retries.
pub async fn run_session_limited<S, R, W, F>(
    store: &mut S,
    mut send: W,
    mut recv: R,
    role: AuthRole,
    capabilities: SessionCapabilities,
    limits: SessionLimits<F>,
) -> Result<SessionReport, SessionError>
where
    S: SyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn() -> i64,
{
    let SessionLimits { idle_timeout, egress, now_ms, entries_per_session } = limits;
    let account_id = store.account_id();
    let snapshot = store.snapshot().map_err(SessionError::Store)?;
    let have = bounded_inventory(snapshot.iter().map(|(h, _)| *h));

    // Channel the peer's inventory from the receiver (which parses the peer hello) to the sender
    // (which needs it to decide what to stream). A oneshot: exactly one hello per session.
    let (peer_have_tx, peer_have_rx) = tokio::sync::oneshot::channel::<HashSet<Hash>>();

    let sender = async move {
        write_frame_before(&mut send, &Frame::Hello { account_id, have }, idle_timeout).await?;
        // If the receiver aborted before delivering the peer hello, there is nothing to stream.
        let Ok(peer_have) = peer_have_rx.await else {
            return Ok((send, 0usize));
        };
        let mut to_send: Vec<Vec<u8>> = if capabilities.local.can_push() {
            snapshot
                .into_iter()
                .filter(|(hash, _)| !peer_have.contains(hash))
                .map(|(_, bytes)| bytes)
                .collect()
        } else {
            Vec::new()
        };
        // Global egress cap: TRIM the set to the prefix the shared byte budget affords, then page
        // it normally — the paging below is unchanged, so every `more` flag stays correct
        // and the receiver never sees a truncated-but-`more:true` stream. The trimmed tail
        // is re-offered by the next session's inventory diff (convergence). Charged per
        // entry under ONE lock, held only here (never across an await); the balance may go
        // negative on an oversized entry, and permitting while ANY credit remains
        // guarantees forward progress. Poison-tolerant so a panic in one session task
        // cannot wedge all future serving.
        if let Some(limiter) = &egress {
            let now = now_ms();
            let mut guard = limiter.lock().unwrap_or_else(|poison| poison.into_inner());
            let kept = to_send.iter().take_while(|entry| guard.allow(entry.len(), now)).count();
            drop(guard);
            to_send.truncate(kept);
        }
        let total = to_send.len();
        // Drain into pages bounded by BOTH the entry count and the entry bytes, whichever binds
        // first — a count alone does not bound a frame, because this `Frame` serves lanes whose
        // entries differ in size by 4x.
        //
        // Always take at least one entry, or an entry above the byte budget would yield an empty
        // page and the loop would never advance. Such an entry is still SERVED: the budget leaves
        // headroom under the codec's frame cap, so a single entry between the two is written
        // normally, and only one past the frame cap is refused. No envelope that large can reach a
        // snapshot today — both lanes cap their entries far below it — so the floor is about the
        // loop's progress, not about serving oversized entries.
        let mut rest = to_send.split_off(0);
        while !rest.is_empty() {
            let mut bytes = 0usize;
            let take = rest
                .iter()
                .take(MAX_ENTRIES_PER_PAGE)
                .take_while(|entry| {
                    bytes += entry.len();
                    bytes <= MAX_ENTRIES_PAGE_BYTES
                })
                .count()
                .max(1);
            let tail = rest.split_off(take);
            let page = std::mem::replace(&mut rest, tail);
            let more = !rest.is_empty();
            write_frame_before(&mut send, &Frame::Entries { entries: page, more }, idle_timeout)
                .await?;
        }
        write_frame_before(&mut send, &Frame::Done, idle_timeout).await?;
        // Keep the send half open for the completion acknowledgement. Returning ownership lets the
        // role-ordered phase below send it only after this side has consumed the peer's `Done`.
        Ok::<(W, usize), SessionError>((send, total))
    };

    let receiver = async {
        // The peer must open with a hello for the account we are syncing.
        let hello = read_frame_before(&mut recv, idle_timeout).await?;
        let Frame::Hello { account_id: peer_account, have: peer_have } = hello else {
            return Err(SessionError::Protocol("peer did not open with a hello".into()));
        };
        if peer_account != account_id {
            return Err(SessionError::Protocol(
                "peer hello names a different account than this session".into(),
            ));
        }
        // Hand the peer's inventory to the sender; if it already gave up, we still drain the
        // stream.
        let _ = peer_have_tx.send(peer_have.into_iter().collect());

        let mut received = 0usize;
        let mut newly_stored = 0usize;
        // Page sequencing: a peer streams zero or more `Entries` pages, the last with `more:
        // false`, then `Done`. `saw_page` records that at least one page arrived;
        // `saw_final` that a `more: false` page marked the stream complete. Together they
        // reject both a `Done` after a page that declared `more: true` (truncation) and any
        // page sent AFTER the final one.
        let mut saw_page = false;
        let mut saw_final = false;
        loop {
            match read_frame_before(&mut recv, idle_timeout).await {
                Ok(Frame::Entries { entries, more }) => {
                    // Read admission never implies write authority. Reject the frame before
                    // inspecting or ingesting its payload so an anonymous/open or roster-read-only
                    // peer cannot consume storage or verification work.
                    if !capabilities.peer.can_push() {
                        return Err(SessionError::UnauthorizedPush);
                    }
                    // A page after the one that declared `more: false` contradicts the sequencing —
                    // the peer said the previous page was the last.
                    if saw_final {
                        return Err(SessionError::Protocol(
                            "peer sent an Entries page after the final page".into(),
                        ));
                    }
                    // An empty page is never sent by an honest peer (nothing to say → Done). It is
                    // the shape a flood uses to hold the session open with `more: true` forever, so
                    // reject it outright.
                    if entries.is_empty() {
                        return Err(SessionError::Protocol(
                            "peer sent an empty Entries page".into(),
                        ));
                    }
                    for bytes in entries {
                        received += 1;
                        if received > entries_per_session {
                            return Err(SessionError::Protocol(format!(
                                "peer streamed more than {entries_per_session} entries",
                            )));
                        }
                        match store.ingest(&bytes).map_err(SessionError::Store)? {
                            Ingested::Stored => newly_stored += 1,
                            Ingested::NoChange => {},
                        }
                    }
                    saw_page = true;
                    saw_final = !more;
                },
                Ok(Frame::Done) => {
                    if saw_page && !saw_final {
                        return Err(SessionError::Protocol(
                            "peer sent Done after declaring more pages would follow".into(),
                        ));
                    }
                    break;
                },
                Ok(Frame::Ack) => {
                    return Err(SessionError::Protocol(
                        "peer acknowledged before sending Done".into(),
                    ));
                },
                Ok(Frame::Hello { .. }) => {
                    return Err(SessionError::Protocol("a second hello mid-session".into()));
                },
                Ok(Frame::Auth { .. }) => {
                    // Auth belongs to the handshake the endpoint runs BEFORE `run_session`; an Auth
                    // frame in the data phase is out of sequence.
                    return Err(SessionError::Protocol("an auth frame mid-session".into()));
                },
                Ok(Frame::AuthGrant { .. }) => {
                    return Err(SessionError::Protocol("an auth grant mid-session".into()));
                },
                // `read_frame_before` has already mapped EOF (truncated transfer) and idle timeout
                // into a `SessionError`, so any error here just propagates.
                Err(e) => return Err(e),
            }
        }
        Ok::<(R, usize, usize), SessionError>((recv, received, newly_stored))
    };

    // `try_join!`, not `join!`: if either half errors, the other is cancelled immediately. Without
    // it, a peer that sends a bad frame and stops reading would leave the sender blocked on QUIC
    // flow control mid-stream, and the session would hang instead of failing.
    let ((mut send, entries_sent), (mut recv, entries_received, entries_newly_stored)) =
        tokio::try_join!(sender, receiver)?;
    role.acknowledge_in_order(
        send_ack_and_finish(&mut send, idle_timeout),
        read_ack_before(&mut recv, idle_timeout),
    )
    .await?;
    Ok(SessionReport {
        entries_sent,
        entries_received,
        entries_newly_stored,
        peer_capability: capabilities.peer,
    })
}

async fn send_ack_and_finish<W: AsyncWrite + Unpin>(
    send: &mut W,
    idle_timeout: Duration,
) -> Result<(), SessionError> {
    write_frame_before(send, &Frame::Ack, idle_timeout).await?;
    // On iroh this maps to QUIC FIN. The acceptor remains alive until the dialer closes, while the
    // dialer does not close until it has read the acceptor's acknowledgement.
    auth::within(idle_timeout, send.shutdown(), || SessionError::Timeout { after: idle_timeout })
        .await?
        .map_err(|e| SessionError::Codec(CodecError::Io(e)))
}

/// Write one frame, failing if the peer takes nothing within `idle_timeout`. The write side waits
/// on the peer as much as the read side does: QUIC flow control blocks a write while the peer's
/// window is full, so a peer that stops reading would otherwise hold the session — its serving
/// permit and database connection — for as long as it keeps the connection open.
async fn write_frame_before<W: AsyncWrite + Unpin>(
    send: &mut W,
    frame: &Frame,
    idle_timeout: Duration,
) -> Result<(), SessionError> {
    auth::within(idle_timeout, codec::write_frame(send, frame), || SessionError::Timeout {
        after: idle_timeout,
    })
    .await?
    .map_err(SessionError::Codec)
}

async fn read_ack_before<R: AsyncRead + Unpin>(
    recv: &mut R,
    idle_timeout: Duration,
) -> Result<(), SessionError> {
    match read_frame_before(recv, idle_timeout).await? {
        Frame::Ack => Ok(()),
        _ => Err(SessionError::Protocol(
            "peer sent another data-phase frame instead of the completion acknowledgement".into(),
        )),
    }
}

/// Read the next frame, failing if the peer sends nothing within `idle_timeout`. Folds a clean EOF
/// and an idle timeout into a `SessionError` — the caller propagates either as a session failure,
/// so a stalled or silent peer cannot hold the (single-session) server open indefinitely.
async fn read_frame_before<R: AsyncRead + Unpin>(
    recv: &mut R,
    idle_timeout: Duration,
) -> Result<Frame, SessionError> {
    let read = auth::within(idle_timeout, codec::read_frame(recv), || SessionError::Timeout {
        after: idle_timeout,
    });
    match read.await? {
        Ok(frame) => Ok(frame),
        Err(CodecError::Eof) => Err(SessionError::Protocol(
            "peer closed the stream before session completion — transfer truncated".into(),
        )),
        Err(e) => Err(SessionError::Codec(e)),
    }
}

#[cfg(test)]
mod tests;
