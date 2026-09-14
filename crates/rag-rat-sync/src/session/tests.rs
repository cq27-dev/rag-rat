use super::*;
use crate::auth::PeerCapability;
use crate::testing::SessionMemStore as MemStore;

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
        run_session(b, b_send, b_recv, AuthRole::Acceptor, SessionCapabilities::bidirectional(),),
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
async fn an_exhausted_egress_budget_serves_nothing_then_converges_after_refill() {
    use std::sync::{Arc, Mutex};
    let now = 1_700_000_000_000i64;
    let full: Vec<_> = (0u8..5).map(entry).collect();

    // A shared egress limiter overspent so its balance is negative at `now`.
    let egress = Arc::new(Mutex::new(crate::GlobalEgressLimiter::new()));
    egress.lock().unwrap().allow(256 * 1024 * 1024, now);

    // Round 1 at `now` (no refill): an exhausted budget serves nothing.
    {
        let mut server = MemStore::new([0xe6; 32], &full);
        let mut reader = MemStore::new([0xe6; 32], &[]);
        let (server_send, reader_recv) = tokio::io::duplex(1 << 20);
        let (reader_send, server_recv) = tokio::io::duplex(1 << 20);
        let (s, r) = tokio::join!(
            run_session_limited(
                &mut server,
                server_send,
                server_recv,
                AuthRole::Acceptor,
                SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
                SessionLimits {
                    idle_timeout: DEFAULT_IDLE_TIMEOUT,
                    egress: Some(egress.clone()),
                    now_ms: move || now,
                },
            ),
            run_session(
                &mut reader,
                reader_send,
                reader_recv,
                AuthRole::Dialer,
                SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadWrite),
            ),
        );
        s.unwrap();
        r.unwrap();
        assert!(reader.entries.is_empty(), "an exhausted egress budget serves nothing");
    }

    // Round 2 with the clock advanced past a full refill: the withheld set is served —
    // convergence across retries.
    {
        let later = now + 60_000;
        let mut server = MemStore::new([0xe6; 32], &full);
        let mut reader = MemStore::new([0xe6; 32], &[]);
        let (server_send, reader_recv) = tokio::io::duplex(1 << 20);
        let (reader_send, server_recv) = tokio::io::duplex(1 << 20);
        let (s, r) = tokio::join!(
            run_session_limited(
                &mut server,
                server_send,
                server_recv,
                AuthRole::Acceptor,
                SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
                SessionLimits {
                    idle_timeout: DEFAULT_IDLE_TIMEOUT,
                    egress: Some(egress.clone()),
                    now_ms: move || later,
                },
            ),
            run_session(
                &mut reader,
                reader_send,
                reader_recv,
                AuthRole::Dialer,
                SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadWrite),
            ),
        );
        s.unwrap();
        r.unwrap();
        assert_eq!(reader.entries.len(), full.len(), "after refill the withheld set is served");
    }
}

#[tokio::test]
async fn a_generous_egress_budget_serves_a_multi_page_transfer_intact() {
    use std::sync::{Arc, Mutex};
    // Distinct 32-byte hashes so the set exceeds one page (`entry`'s u8 seed caps at 256).
    let make = |i: usize| -> (Hash, Vec<u8>) {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&(i as u64).to_be_bytes());
        let mut bytes = vec![0u8; 40];
        bytes[..32].copy_from_slice(&hash);
        (hash, bytes)
    };
    let count = MAX_ENTRIES_PER_PAGE + 10; // spans more than one page
    let full: Vec<_> = (0..count).map(make).collect();
    let mut server = MemStore::new([0xe7; 32], &full);
    let mut reader = MemStore::new([0xe7; 32], &[]);
    // A fresh (generous) budget must not disturb a normal multi-page transfer: every page's
    // `more` flag stays correct and the receiver's truncation guard never fires.
    let egress = Arc::new(Mutex::new(crate::GlobalEgressLimiter::new()));
    let (server_send, reader_recv) = tokio::io::duplex(1 << 20);
    let (reader_send, server_recv) = tokio::io::duplex(1 << 20);
    let (s, r) = tokio::join!(
        run_session_limited(
            &mut server,
            server_send,
            server_recv,
            AuthRole::Acceptor,
            SessionCapabilities::new(PeerCapability::ReadWrite, PeerCapability::ReadOnly),
            SessionLimits {
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                egress: Some(egress),
                now_ms: || 1_700_000_000_000,
            },
        ),
        run_session(
            &mut reader,
            reader_send,
            reader_recv,
            AuthRole::Dialer,
            SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadWrite),
        ),
    );
    let server_report = s.unwrap();
    r.unwrap();
    assert_eq!(reader.entries.len(), count, "the whole multi-page set is served intact");
    assert_eq!(server_report.entries_sent, count, "entries_sent reflects the full transfer");
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

/// A peer that sends `Hello` and `Done`, then stops reading while keeping its connection open,
/// must not hold the session: the writes wait on the peer as much as the reads do.
#[tokio::test]
async fn a_peer_that_stops_reading_cannot_hold_the_session() {
    let full: Vec<_> = (0u8..64).map(entry).collect();
    let mut server = MemStore::new([6; 32], &full);
    let (mut peer_send, recv) = tokio::io::duplex(1 << 16);
    // A tiny window the peer never drains: the first large write blocks on it.
    let (send, _peer_recv_never_read) = tokio::io::duplex(64);
    codec::write_frame(&mut peer_send, &Frame::Hello { account_id: [6; 32], have: vec![] })
        .await
        .unwrap();
    codec::write_frame(&mut peer_send, &Frame::Done).await.unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_session_limited(
            &mut server,
            send,
            recv,
            AuthRole::Acceptor,
            SessionCapabilities::bidirectional(),
            SessionLimits { idle_timeout: Duration::from_millis(50), ..SessionLimits::default() },
        ),
    )
    .await
    .expect("the session must give up on a peer that stops reading, not wait on it");
    assert!(
        matches!(result, Err(SessionError::Timeout { after }) if after == Duration::from_millis(50)),
        "{result:?}",
    );
    drop(peer_send);
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
        codec::write_frame(&mut dialer_send, &Frame::Hello { account_id: [5; 32], have: vec![] })
            .await
            .unwrap();
        codec::write_frame(&mut dialer_send, &Frame::Done).await.unwrap();

        assert!(matches!(codec::read_frame(&mut dialer_recv).await, Ok(Frame::Hello { .. })));
        assert_eq!(codec::read_frame(&mut dialer_recv).await.unwrap(), Frame::Done);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), codec::read_frame(&mut dialer_recv),)
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
    let result = run_session_limited(
        &mut receiver,
        send,
        recv,
        AuthRole::Dialer,
        SessionCapabilities::bidirectional(),
        SessionLimits {
            idle_timeout: std::time::Duration::from_millis(50),
            ..SessionLimits::default()
        },
    )
    .await;
    drop(peer_send); // keep the stream alive until after the timeout fired
    match result {
        Err(SessionError::Timeout { after }) => {
            assert_eq!(after, std::time::Duration::from_millis(50));
        },
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
        write_frame(&mut peer_send, &Frame::Entries { entries: vec![], more: true }).await.unwrap();
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
        run_session(&mut a, a_send, a_recv, AuthRole::Dialer, SessionCapabilities::bidirectional(),),
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
