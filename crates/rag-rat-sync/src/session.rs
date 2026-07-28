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

use crate::auth::{AuthRole, SessionCapabilities};
use crate::codec::{self, CodecError};
use crate::wire::{Frame, MAX_ENTRIES_PER_PAGE, MAX_HELLO_HASHES};

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

/// The store side of a session: what a peer offers and where received entries land. Implemented
/// over the op log for production and over an in-memory map for tests.
pub trait SyncStore {
    /// The account this session is scoped to. A peer whose hello names a different account is a
    /// misdirected connection and the session aborts.
    fn account_id(&self) -> Hash;

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
}

/// A session that could not complete.
#[derive(Debug)]
pub enum SessionError {
    /// The transport failed or the peer sent an unreadable frame.
    Codec(CodecError),
    /// The peer opened with something other than a hello, or named a different account.
    Protocol(String),
    /// Authentication admitted the peer for reads, but it attempted to push entries.
    UnauthorizedPush,
    /// Reading the local entry snapshot failed.
    Store(anyhow::Error),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Codec(e) => write!(f, "sync session transport: {e}"),
            SessionError::Protocol(m) => write!(f, "sync session protocol violation: {m}"),
            SessionError::UnauthorizedPush => {
                write!(f, "read-only peer attempted to push sync entries")
            },
            SessionError::Store(e) => write!(f, "sync session store: {e}"),
        }
    }
}

impl std::error::Error for SessionError {}

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
    run_session_with_idle_timeout(store, send, recv, role, capabilities, DEFAULT_IDLE_TIMEOUT).await
}

/// [`run_session`] with an explicit idle timeout — the receiver aborts if the peer sends no frame
/// within `idle_timeout`. Exposed so tests can exercise the timeout without waiting the default.
pub async fn run_session_with_idle_timeout<S, R, W>(
    store: &mut S,
    mut send: W,
    mut recv: R,
    role: AuthRole,
    capabilities: SessionCapabilities,
    idle_timeout: Duration,
) -> Result<SessionReport, SessionError>
where
    S: SyncStore,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let account_id = store.account_id();
    let snapshot = store.snapshot().map_err(SessionError::Store)?;
    let have = bounded_inventory(snapshot.iter().map(|(h, _)| *h));

    // Channel the peer's inventory from the receiver (which parses the peer hello) to the sender
    // (which needs it to decide what to stream). A oneshot: exactly one hello per session.
    let (peer_have_tx, peer_have_rx) = tokio::sync::oneshot::channel::<HashSet<Hash>>();

    let sender = async move {
        codec::write_frame(&mut send, &Frame::Hello { account_id, have })
            .await
            .map_err(SessionError::Codec)?;
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
        let total = to_send.len();
        // Drain into fixed pages so no single frame exceeds the per-page cap.
        let mut rest = to_send.split_off(0);
        while !rest.is_empty() {
            let tail = rest.split_off(rest.len().min(MAX_ENTRIES_PER_PAGE));
            let page = std::mem::replace(&mut rest, tail);
            let more = !rest.is_empty();
            codec::write_frame(&mut send, &Frame::Entries { entries: page, more })
                .await
                .map_err(SessionError::Codec)?;
        }
        codec::write_frame(&mut send, &Frame::Done).await.map_err(SessionError::Codec)?;
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
                        if received > MAX_SESSION_ENTRIES {
                            return Err(SessionError::Protocol(format!(
                                "peer streamed more than {MAX_SESSION_ENTRIES} entries",
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
    complete_session(&mut send, &mut recv, role, idle_timeout).await?;
    Ok(SessionReport { entries_sent, entries_received, entries_newly_stored })
}

/// Prove both data streams were consumed before the dialer may close the connection. The ordering
/// is deliberate: the dialer acknowledges first, the acceptor reads that proof before replying,
/// and the dialer waits for the reply. The acceptor endpoint then keeps the connection alive until
/// the dialer closes, so its final acknowledgement cannot be truncated in flight.
async fn complete_session<R, W>(
    send: &mut W,
    recv: &mut R,
    role: AuthRole,
    idle_timeout: Duration,
) -> Result<(), SessionError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match role {
        AuthRole::Dialer => {
            send_ack_and_finish(send).await?;
            read_ack_before(recv, idle_timeout).await
        },
        AuthRole::Acceptor => {
            read_ack_before(recv, idle_timeout).await?;
            send_ack_and_finish(send).await
        },
    }
}

async fn send_ack_and_finish<W: AsyncWrite + Unpin>(send: &mut W) -> Result<(), SessionError> {
    codec::write_frame(send, &Frame::Ack).await.map_err(SessionError::Codec)?;
    // On iroh this maps to QUIC FIN. The acceptor remains alive until the dialer closes, while the
    // dialer does not close until it has read the acceptor's acknowledgement.
    send.shutdown().await.map_err(|e| SessionError::Codec(CodecError::Io(e)))
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
    match tokio::time::timeout(idle_timeout, codec::read_frame(recv)).await {
        Ok(Ok(frame)) => Ok(frame),
        Ok(Err(CodecError::Eof)) => Err(SessionError::Protocol(
            "peer closed the stream before session completion — transfer truncated".into(),
        )),
        Ok(Err(e)) => Err(SessionError::Codec(e)),
        Err(_elapsed) => Err(SessionError::Protocol(format!(
            "peer sent no frame within {idle_timeout:?} — session aborted as idle"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::auth::PeerCapability;

    /// An in-memory store: entries are `(hash, bytes)`, ingest inserts if absent. Enough to
    /// exercise the protocol without a database — the DB-backed store has its own integration
    /// test.
    struct MemStore {
        account: Hash,
        entries: HashMap<Hash, Vec<u8>>,
    }

    impl MemStore {
        fn new(account: Hash, entries: &[(Hash, Vec<u8>)]) -> Self {
            Self { account, entries: entries.iter().cloned().collect() }
        }
    }

    impl SyncStore for MemStore {
        fn account_id(&self) -> Hash {
            self.account
        }
        fn snapshot(&self) -> anyhow::Result<Vec<(Hash, Vec<u8>)>> {
            let mut v: Vec<_> = self.entries.iter().map(|(h, b)| (*h, b.clone())).collect();
            v.sort_by_key(|(h, _)| *h);
            Ok(v)
        }
        fn ingest(&mut self, signed_bytes: &[u8]) -> anyhow::Result<Ingested> {
            // The test's "hash" is the first 32 bytes of the payload it authored below.
            let hash: Hash = signed_bytes[..32].try_into().unwrap();
            match self.entries.entry(hash) {
                std::collections::hash_map::Entry::Occupied(_) => Ok(Ingested::NoChange),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(signed_bytes.to_vec());
                    Ok(Ingested::Stored)
                },
            }
        }
    }

    fn entry(seed: u8) -> (Hash, Vec<u8>) {
        let mut bytes = vec![seed; 40];
        bytes[..32].copy_from_slice(&[seed; 32]);
        ([seed; 32], bytes)
    }

    async fn sync_pair(a: &mut MemStore, b: &mut MemStore) -> (SessionReport, SessionReport) {
        let (a_send, b_recv) = tokio::io::duplex(1 << 20);
        let (b_send, a_recv) = tokio::io::duplex(1 << 20);
        let (ra, rb) = tokio::join!(
            run_session(a, a_send, a_recv, AuthRole::Dialer, SessionCapabilities::bidirectional()),
            run_session(
                b,
                b_send,
                b_recv,
                AuthRole::Acceptor,
                SessionCapabilities::bidirectional(),
            ),
        );
        (ra.unwrap(), rb.unwrap())
    }

    #[tokio::test]
    async fn a_peer_with_nothing_restores_the_full_set_from_the_other() {
        let full: Vec<_> = (0u8..5).map(entry).collect();
        let mut a = MemStore::new([0xac; 32], &full);
        let mut b = MemStore::new([0xac; 32], &[]);
        let (ra, rb) = sync_pair(&mut a, &mut b).await;

        assert_eq!(ra.entries_sent, 5, "the full peer sends all five");
        assert_eq!(rb.entries_newly_stored, 5, "the empty peer stores all five");
        assert_eq!(a.entries.len(), 5, "the full peer is unchanged");
        assert_eq!(b.entries.len(), 5, "the empty peer is now complete");
        assert_eq!(a.entries, b.entries, "both hold the same set — restore-from-peer");
    }

    #[tokio::test]
    async fn a_read_only_peer_can_pull_without_uploading_local_entries() {
        let full: Vec<_> = (0u8..5).map(entry).collect();
        let mut server = MemStore::new([0xad; 32], &full);
        let reader_only = entry(9);
        let mut reader = MemStore::new([0xad; 32], std::slice::from_ref(&reader_only));
        let (server_send, reader_recv) = tokio::io::duplex(1 << 20);
        let (reader_send, server_recv) = tokio::io::duplex(1 << 20);

        let (server_report, reader_report) = tokio::join!(
            run_session(
                &mut server,
                server_send,
                server_recv,
                AuthRole::Acceptor,
                SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
            ),
            run_session(
                &mut reader,
                reader_send,
                reader_recv,
                AuthRole::Dialer,
                SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadWrite),
            ),
        );
        let server_report = server_report.unwrap();
        let reader_report = reader_report.unwrap();
        assert_eq!(server_report.entries_sent, full.len());
        assert_eq!(server_report.entries_received, 0);
        assert_eq!(reader_report.entries_sent, 0);
        assert_eq!(reader_report.entries_newly_stored, full.len());
        assert_eq!(server.entries.len(), full.len());
        assert_eq!(reader.entries.len(), full.len() + 1);
        assert!(reader.entries.contains_key(&reader_only.0));
    }

    #[tokio::test]
    async fn a_read_only_peers_entries_are_rejected_before_ingest() {
        let mut reader = MemStore::new([0xae; 32], &[entry(1)]);
        let mut server = MemStore::new([0xae; 32], &[]);
        let (reader_send, server_recv) = tokio::io::duplex(1 << 20);
        let (server_send, reader_recv) = tokio::io::duplex(1 << 20);

        let (reader_result, server_result) = tokio::join!(
            run_session(
                &mut reader,
                reader_send,
                reader_recv,
                AuthRole::Dialer,
                SessionCapabilities::bidirectional(),
            ),
            run_session(
                &mut server,
                server_send,
                server_recv,
                AuthRole::Acceptor,
                SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
            ),
        );
        assert!(reader_result.is_err(), "the peer observes the refused session");
        assert!(matches!(server_result, Err(SessionError::UnauthorizedPush)));
        assert!(server.entries.is_empty(), "the read-only frame reached no ingest call");
    }

    #[tokio::test]
    async fn disjoint_peers_converge_to_the_union_both_directions() {
        let mut a = MemStore::new([1; 32], &[entry(1), entry(2), entry(3)]);
        let mut b = MemStore::new([1; 32], &[entry(3), entry(4), entry(5)]);
        let (ra, rb) = sync_pair(&mut a, &mut b).await;

        // Each sends only what the other lacks; the shared entry(3) is sent by neither... actually
        // both send their non-shared entries. a lacks 4,5; b lacks 1,2.
        assert_eq!(rb.entries_newly_stored, 2, "b gains 1 and 2");
        assert_eq!(ra.entries_newly_stored, 2, "a gains 4 and 5");
        let union: HashSet<Hash> = (1u8..=5).map(|s| [s; 32]).collect();
        assert_eq!(a.entries.keys().copied().collect::<HashSet<_>>(), union);
        assert_eq!(b.entries.keys().copied().collect::<HashSet<_>>(), union);
    }

    #[tokio::test]
    async fn already_in_sync_transfers_nothing() {
        let same: Vec<_> = (10u8..13).map(entry).collect();
        let mut a = MemStore::new([2; 32], &same);
        let mut b = MemStore::new([2; 32], &same);
        let (ra, rb) = sync_pair(&mut a, &mut b).await;
        assert_eq!(ra.entries_sent, 0);
        assert_eq!(rb.entries_sent, 0);
        assert_eq!(ra.entries_newly_stored, 0);
        assert_eq!(rb.entries_newly_stored, 0);
    }

    #[tokio::test]
    async fn completion_ack_is_required_after_done() {
        let mut receiver = MemStore::new([3; 32], &[]);
        let (mut peer_send, recv) = tokio::io::duplex(1 << 16);
        let (send, _peer_recv) = tokio::io::duplex(1 << 16);
        let feeder = tokio::spawn(async move {
            codec::write_frame(&mut peer_send, &Frame::Hello { account_id: [3; 32], have: vec![] })
                .await
                .unwrap();
            codec::write_frame(&mut peer_send, &Frame::Done).await.unwrap();
            // Drop without Ack: Done terminates the data phase, not the delivery handshake.
        });

        let result = run_session(
            &mut receiver,
            send,
            recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
        )
        .await;
        feeder.await.unwrap();
        assert!(
            matches!(result, Err(SessionError::Protocol(ref message)) if message.contains("completion")),
            "Done without Ack must not report a delivered session: {result:?}",
        );
    }

    #[tokio::test]
    async fn ack_before_done_is_rejected() {
        let mut receiver = MemStore::new([4; 32], &[]);
        let (mut peer_send, recv) = tokio::io::duplex(1 << 16);
        let (send, _peer_recv) = tokio::io::duplex(1 << 16);
        let feeder = tokio::spawn(async move {
            codec::write_frame(&mut peer_send, &Frame::Hello { account_id: [4; 32], have: vec![] })
                .await
                .unwrap();
            codec::write_frame(&mut peer_send, &Frame::Ack).await.unwrap();
        });

        let result = run_session(
            &mut receiver,
            send,
            recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
        )
        .await;
        feeder.await.unwrap();
        assert!(
            matches!(result, Err(SessionError::Protocol(ref message)) if message.contains("before sending Done")),
            "an early Ack cannot skip the data phase: {result:?}",
        );
    }

    #[tokio::test]
    async fn acceptor_replies_only_after_the_dialer_ack() {
        let mut acceptor = MemStore::new([5; 32], &[]);
        let (acceptor_send, mut dialer_recv) = tokio::io::duplex(1 << 16);
        let (mut dialer_send, acceptor_recv) = tokio::io::duplex(1 << 16);

        let dialer = async move {
            codec::write_frame(&mut dialer_send, &Frame::Hello {
                account_id: [5; 32],
                have: vec![],
            })
            .await
            .unwrap();
            codec::write_frame(&mut dialer_send, &Frame::Done).await.unwrap();

            assert!(matches!(codec::read_frame(&mut dialer_recv).await, Ok(Frame::Hello { .. })));
            assert_eq!(codec::read_frame(&mut dialer_recv).await.unwrap(), Frame::Done);
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(20),
                    codec::read_frame(&mut dialer_recv),
                )
                .await
                .is_err(),
                "the acceptor must wait for the dialer acknowledgement before replying",
            );

            codec::write_frame(&mut dialer_send, &Frame::Ack).await.unwrap();
            assert_eq!(codec::read_frame(&mut dialer_recv).await.unwrap(), Frame::Ack);
        };
        let session = run_session(
            &mut acceptor,
            acceptor_send,
            acceptor_recv,
            AuthRole::Acceptor,
            SessionCapabilities::bidirectional(),
        );
        let ((), report) = tokio::join!(dialer, session);
        report.unwrap();
    }

    /// A stream that ends after a `more: true` page — a truncated transfer — must FAIL, not report
    /// success, or the caller would treat a partial account as complete.
    #[tokio::test]
    async fn a_truncated_transfer_fails_rather_than_reporting_success() {
        use crate::codec::write_frame;
        let mut receiver = MemStore::new([5; 32], &[]);
        // Feed the receiver a hello then one page claiming more follows, then close abruptly.
        let (mut peer_send, recv) = tokio::io::duplex(1 << 16);
        let (send, _peer_recv) = tokio::io::duplex(1 << 16);
        let feeder = tokio::spawn(async move {
            write_frame(&mut peer_send, &Frame::Hello { account_id: [5; 32], have: vec![] })
                .await
                .unwrap();
            write_frame(&mut peer_send, &Frame::Entries {
                entries: vec![entry(7).1],
                more: true, // a page CLAIMING more will follow …
            })
            .await
            .unwrap();
            // … then drop without Done: a truncated stream.
        });
        let result = run_session(
            &mut receiver,
            send,
            recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
        )
        .await;
        feeder.await.unwrap();
        assert!(
            matches!(result, Err(SessionError::Protocol(_))),
            "EOF before Done is a truncated transfer, not success: {result:?}",
        );
    }

    /// A peer that sends `Done` right after a `more: true` page declared an incomplete transfer
    /// and then stopped — the receiver must reject it, not report success.
    #[tokio::test]
    async fn done_after_a_more_true_page_is_rejected() {
        use crate::codec::write_frame;
        let mut receiver = MemStore::new([6; 32], &[]);
        let (mut peer_send, recv) = tokio::io::duplex(1 << 16);
        let (send, _peer_recv) = tokio::io::duplex(1 << 16);
        let feeder = tokio::spawn(async move {
            write_frame(&mut peer_send, &Frame::Hello { account_id: [6; 32], have: vec![] })
                .await
                .unwrap();
            write_frame(&mut peer_send, &Frame::Entries { entries: vec![entry(1).1], more: true })
                .await
                .unwrap();
            write_frame(&mut peer_send, &Frame::Done).await.unwrap();
        });
        let result = run_session(
            &mut receiver,
            send,
            recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
        )
        .await;
        feeder.await.unwrap();
        assert!(
            matches!(result, Err(SessionError::Protocol(_))),
            "Done after more:true is a declared-incomplete transfer: {result:?}",
        );
    }

    /// An empty Entries page is the shape a flood uses to keep a session open forever; the receiver
    /// rejects it rather than looping.
    /// A peer that connects, sends a valid hello, then goes silent must not hold the session open:
    /// the receiver aborts after the idle timeout. Uses a tiny timeout so the test is fast.
    #[tokio::test]
    async fn a_silent_peer_times_out() {
        use crate::codec::write_frame;
        let mut receiver = MemStore::new([11; 32], &[]);
        // The peer sends a hello then never sends again and keeps the stream OPEN (holds
        // `peer_send` for the whole test rather than dropping it, so there is no EOF — only
        // silence).
        let (mut peer_send, recv) = tokio::io::duplex(1 << 16);
        let (send, _peer_recv) = tokio::io::duplex(1 << 16);
        write_frame(&mut peer_send, &Frame::Hello { account_id: [11; 32], have: vec![] })
            .await
            .unwrap();
        let result = run_session_with_idle_timeout(
            &mut receiver,
            send,
            recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
            std::time::Duration::from_millis(50),
        )
        .await;
        drop(peer_send); // keep the stream alive until after the timeout fired
        match result {
            Err(SessionError::Protocol(m)) => assert!(m.contains("idle"), "{m}"),
            other => panic!("expected an idle-timeout abort: {other:?}"),
        }
    }

    /// A page after the one that declared `more: false` contradicts the sequencing and is rejected.
    #[tokio::test]
    async fn a_page_after_the_final_page_is_rejected() {
        use crate::codec::write_frame;
        let mut receiver = MemStore::new([12; 32], &[]);
        let (mut peer_send, recv) = tokio::io::duplex(1 << 16);
        let (send, _peer_recv) = tokio::io::duplex(1 << 16);
        let feeder = tokio::spawn(async move {
            write_frame(&mut peer_send, &Frame::Hello { account_id: [12; 32], have: vec![] })
                .await
                .unwrap();
            write_frame(&mut peer_send, &Frame::Entries { entries: vec![entry(1).1], more: false })
                .await
                .unwrap();
            // A page after the final one contradicts `more: false`.
            write_frame(&mut peer_send, &Frame::Entries { entries: vec![entry(2).1], more: false })
                .await
                .unwrap();
        });
        let result = run_session(
            &mut receiver,
            send,
            recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
        )
        .await;
        feeder.await.unwrap();
        match result {
            Err(SessionError::Protocol(m)) => assert!(m.contains("after the final page"), "{m}"),
            other => panic!("expected the after-final-page guard: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_entries_page_is_rejected() {
        use crate::codec::write_frame;
        let mut receiver = MemStore::new([8; 32], &[]);
        let (mut peer_send, recv) = tokio::io::duplex(1 << 16);
        let (send, _peer_recv) = tokio::io::duplex(1 << 16);
        let feeder = tokio::spawn(async move {
            write_frame(&mut peer_send, &Frame::Hello { account_id: [8; 32], have: vec![] })
                .await
                .unwrap();
            write_frame(&mut peer_send, &Frame::Entries { entries: vec![], more: true })
                .await
                .unwrap();
        });
        let result = run_session(
            &mut receiver,
            send,
            recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
        )
        .await;
        feeder.await.unwrap();
        // Assert the SPECIFIC guard fired — a dropped feeder also trips the EOF-before-Done guard,
        // so a bare `Protocol` match would not distinguish the empty-page rejection from it.
        match result {
            Err(SessionError::Protocol(m)) => assert!(m.contains("empty Entries page"), "{m}"),
            other => panic!("expected the empty-page guard: {other:?}"),
        }
    }

    #[test]
    fn the_outgoing_inventory_is_capped_to_the_wire_limit() {
        let over = MAX_HELLO_HASHES + 100;
        let hashes = (0..over).map(|i| {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&(i as u64).to_be_bytes());
            h
        });
        let bounded = bounded_inventory(hashes);
        assert_eq!(bounded.len(), MAX_HELLO_HASHES, "never advertises more than the peer decodes");
        // And the frame it produces is decodable (would be rejected as over-cap otherwise).
        let frame = Frame::Hello { account_id: [0; 32], have: bounded };
        assert!(Frame::decode(&frame.encode()).is_ok());
    }

    #[tokio::test]
    async fn a_mismatched_account_aborts_the_session() {
        let mut a = MemStore::new([1; 32], &[entry(1)]);
        let mut b = MemStore::new([2; 32], &[entry(2)]);
        let (a_send, b_recv) = tokio::io::duplex(1 << 16);
        let (b_send, a_recv) = tokio::io::duplex(1 << 16);
        let (ra, rb) = tokio::join!(
            run_session(
                &mut a,
                a_send,
                a_recv,
                AuthRole::Dialer,
                SessionCapabilities::bidirectional(),
            ),
            run_session(
                &mut b,
                b_send,
                b_recv,
                AuthRole::Acceptor,
                SessionCapabilities::bidirectional(),
            ),
        );
        assert!(matches!(ra, Err(SessionError::Protocol(_))));
        assert!(matches!(rb, Err(SessionError::Protocol(_))));
    }
}
