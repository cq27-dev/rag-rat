use super::*;
use crate::testing::TableMemStore as MemStore;

fn item(repo: &str, stream: u8) -> ManifestItem {
    ManifestItem {
        repo_id: repo.into(),
        incarnation_ref: [1; 32],
        scope_id: "anchors/1".into(),
        stream_id: [stream; 32],
    }
}

async fn pair(a: &mut MemStore, b: &mut MemStore) -> (TableSessionReport, TableSessionReport) {
    pair_with_limits(a, b, TableSessionLimits::default()).await
}

async fn pair_with_limits(
    a: &mut MemStore,
    b: &mut MemStore,
    limits: TableSessionLimits,
) -> (TableSessionReport, TableSessionReport) {
    let (a, b) = try_pair_with_limits(a, b, limits).await;
    (a.unwrap(), b.unwrap())
}

async fn try_pair_with_limits(
    a: &mut MemStore,
    b: &mut MemStore,
    limits: TableSessionLimits,
) -> (Result<TableSessionReport, TableSessionError>, Result<TableSessionReport, TableSessionError>)
{
    let (a_send, b_recv) = tokio::io::duplex(1 << 20);
    let (b_send, a_recv) = tokio::io::duplex(1 << 20);
    tokio::join!(
        run_table_session_with_limits(
            a,
            a_send,
            a_recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
            limits,
        ),
        run_table_session_with_limits(
            b,
            b_send,
            b_recv,
            AuthRole::Acceptor,
            SessionCapabilities::bidirectional(),
            limits,
        ),
    )
}

#[tokio::test]
async fn only_the_multi_repo_manifest_intersection_reconciles() {
    let shared = item("repo-b", 2);
    let mut a = MemStore::new(vec![item("repo-a", 1), shared.clone()]);
    let mut b = MemStore::new(vec![shared.clone(), item("repo-c", 3)]);
    a.insert([1; 32], 10);
    a.insert(shared.stream_id, 20);
    b.insert([3; 32], 30);
    a.forbid_snapshot([1; 32]);
    b.forbid_snapshot([3; 32]);

    let (a_report, b_report) = pair(&mut a, &mut b).await;
    assert_eq!(a_report.streams, 1);
    assert_eq!(b_report.entries_newly_stored, 1);
    assert_eq!(b.entries[&shared.stream_id].len(), 1);
    assert!(!b.entries.contains_key(&[1; 32]), "repo-a never crosses into repo-c's peer");
    assert!(!a.entries.contains_key(&[3; 32]), "repo-c never crosses into repo-a's peer");

    let (again_a, again_b) = pair(&mut a, &mut b).await;
    assert_eq!(again_a.entries_sent + again_b.entries_sent, 0);
    assert_eq!(again_a.entries_newly_stored + again_b.entries_newly_stored, 0);
}

#[tokio::test]
async fn empty_manifests_are_a_clean_no_op() {
    let mut a = MemStore::new(Vec::new());
    let mut b = MemStore::new(Vec::new());
    let (a, b) = pair(&mut a, &mut b).await;
    assert_eq!(a, TableSessionReport::default());
    assert_eq!(b, TableSessionReport::default());
}

#[tokio::test]
async fn read_only_sessions_do_not_prepare_local_table_authorship() {
    use crate::auth::PeerCapability;

    let mut a = MemStore::new(Vec::new());
    let mut b = MemStore::new(Vec::new());
    let (a_send, b_recv) = tokio::io::duplex(1024);
    let (b_send, a_recv) = tokio::io::duplex(1024);
    let capabilities = SessionCapabilities::new(PeerCapability::ReadOnly, PeerCapability::ReadOnly);
    let (a_result, b_result) = tokio::join!(
        run_table_session_with_limits(
            &mut a,
            a_send,
            a_recv,
            AuthRole::Dialer,
            capabilities,
            TableSessionLimits::default(),
        ),
        run_table_session_with_limits(
            &mut b,
            b_send,
            b_recv,
            AuthRole::Acceptor,
            capabilities,
            TableSessionLimits::default(),
        ),
    );
    a_result.unwrap();
    b_result.unwrap();
    assert_eq!(a.prepare_count, 0);
    assert_eq!(b.prepare_count, 0);
}

#[tokio::test]
async fn capped_sessions_advance_from_durable_frontiers_until_quiet() {
    let shared = item("repo-a", 1);
    let mut source = MemStore::new(vec![shared.clone()]);
    let mut destination = MemStore::new(vec![shared.clone()]);
    for seed in 1..=5 {
        source.insert_chain(shared.stream_id, 7, u64::from(seed), seed);
    }
    let limits = TableSessionLimits {
        chains_per_page: 2,
        chains_per_session: 8,
        entries_per_page: 1,
        entries_per_session: 2,
        ..Default::default()
    };

    let mut moved = Vec::new();
    let mut pending = Vec::new();
    for _ in 0..4 {
        let (source_report, destination_report) =
            pair_with_limits(&mut source, &mut destination, limits).await;
        moved.push(source_report.entries_sent);
        pending.push(source_report.continuation_pending);
        assert_eq!(source_report.entries_sent, destination_report.entries_newly_stored);
    }
    assert_eq!(moved, [2, 2, 1, 0]);
    assert_eq!(pending, [true, true, false, false]);
    assert_eq!(destination.entries[&shared.stream_id].len(), 5);
}

/// Two stores that hold different entries at one point of a chain (the chain was signed twice
/// there, #1417): each is `device 5` at lamports 1..=2, sharing lamport 1.
fn diverged_pair(stream: &ManifestItem) -> (MemStore, MemStore) {
    let mut source = MemStore::new(vec![stream.clone()]);
    let mut destination = MemStore::new(vec![stream.clone()]);
    source.insert_chain(stream.stream_id, 5, 1, 10);
    source.insert_chain(stream.stream_id, 5, 2, 11);
    source.insert_chain(stream.stream_id, 5, 3, 12);
    destination.insert_chain(stream.stream_id, 5, 1, 10);
    destination.insert_chain(stream.stream_id, 5, 2, 20);
    (source, destination)
}

/// One chain whose copies diverged must not cost the rest of the session: the sender skips it,
/// every other chain and stream still converges, and the skip does not keep the stream pending
/// (#1480).
#[tokio::test]
async fn a_diverged_chain_is_skipped_and_the_rest_of_the_session_converges() {
    let shared = item("repo-a", 1);
    let other = item("repo-b", 2);
    let (mut source, mut destination) = diverged_pair(&shared);
    for store in [&mut source, &mut destination] {
        store.supported.push(other.clone());
    }
    source.insert_chain(shared.stream_id, 6, 4, 30);
    source.insert_chain(other.stream_id, 7, 5, 40);

    for (dialer_is_source, round) in [(true, 0), (false, 1)] {
        let (source_report, destination_report) = if dialer_is_source {
            pair(&mut source, &mut destination).await
        } else {
            let (d, s) = pair(&mut destination, &mut source).await;
            (s, d)
        };
        assert_eq!(source_report.chains_skipped, 1, "round {round}: the diverged chain alone");
        assert!(!source_report.continuation_pending && !destination_report.continuation_pending);
        if round == 1 {
            assert_eq!(source_report.entries_sent, 0, "the second round is quiet");
        }
    }
    assert!(destination.entries[&shared.stream_id].contains_key(&[30; 32]), "healthy chain");
    assert!(destination.entries[&other.stream_id].contains_key(&[40; 32]), "healthy stream");
    assert!(
        !destination.entries[&shared.stream_id].contains_key(&[12; 32]),
        "nothing of the diverged chain is sent past the divergence",
    );
}

/// Copies that diverged exactly at the tip (the same lamport, different entries) are recognised
/// while planning, before any entry is read, and skipped the same way in both directions.
#[tokio::test]
async fn copies_diverged_at_the_tip_are_skipped_in_both_directions() {
    let shared = item("repo-a", 1);
    let mut left = MemStore::new(vec![shared.clone()]);
    let mut right = MemStore::new(vec![shared.clone()]);
    left.insert_chain(shared.stream_id, 5, 1, 10);
    left.insert_chain(shared.stream_id, 5, 2, 11);
    right.insert_chain(shared.stream_id, 5, 1, 10);
    right.insert_chain(shared.stream_id, 5, 2, 20);
    let (left_report, right_report) = pair(&mut left, &mut right).await;
    assert_eq!((left_report.chains_skipped, right_report.chains_skipped), (1, 1));
    assert_eq!(left_report.entries_sent + right_report.entries_sent, 0);
    assert!(!left_report.continuation_pending && !right_report.continuation_pending);
}

/// Only a diverged cursor is skipped. Any other store failure — a busy database — still fails the
/// session, so a transient error is retried as a whole rather than silently dropping a chain.
#[tokio::test]
async fn any_other_store_error_still_fails_the_session() {
    let shared = item("repo-a", 1);
    let mut source = MemStore::new(vec![shared.clone()]);
    let mut destination = MemStore::new(vec![shared.clone()]);
    source.insert_chain(shared.stream_id, 5, 1, 10);
    source.fail_entries = true;
    let (source_result, _) =
        try_pair_with_limits(&mut source, &mut destination, TableSessionLimits::default()).await;
    assert!(matches!(source_result, Err(TableSessionError::Store(_))), "{source_result:?}");
}

#[tokio::test]
async fn lost_completion_ack_does_not_consume_or_repeat_progress() {
    let shared = item("repo-a", 1);
    let mut source = MemStore::new(vec![shared.clone()]);
    let mut destination = MemStore::new(vec![shared.clone()]);
    source.insert(shared.stream_id, 1);
    let limits = TableSessionLimits::default();
    let (mut source_send, mut destination_recv) = tokio::io::duplex(4096);
    let (mut destination_send, mut source_recv) = tokio::io::duplex(4096);
    let streams = vec![shared];
    let (sent, received) = tokio::join!(
        send_direction(
            &source,
            &streams,
            &mut source_send,
            &mut source_recv,
            PeerCapability::ReadWrite,
            limits,
        ),
        receive_direction(
            &mut destination,
            &streams,
            &mut destination_send,
            &mut destination_recv,
            PeerCapability::ReadWrite,
            limits,
        ),
    );
    assert_eq!(sent.unwrap(), Sent { entries: 1, pending: false, chains_skipped: 0 });
    assert_eq!(received.unwrap(), (1, 1, false));

    let (source_report, destination_report) = pair(&mut source, &mut destination).await;
    assert_eq!(source_report.entries_sent + destination_report.entries_sent, 0);
    assert_eq!(source_report.entries_newly_stored + destination_report.entries_newly_stored, 0);
}

#[test]
fn peer_frontiers_must_be_provable_prefixes_and_restore_debt_stays_pending() {
    let local =
        ChainHead { device_fingerprint: [1; 32], lamport: 3, entry_hash: [3; 32], floor: None };
    assert_eq!(
        chain_plan(&local, FrontierState::Accepted { lamport: 3, entry_hash: [4; 32] }).unwrap(),
        ChainPlan::Diverged,
        "a different entry at our tip is a diverged chain, skipped rather than fatal",
    );
    assert_eq!(
        chain_plan(&local, FrontierState::Accepted { lamport: 4, entry_hash: [4; 32] }).unwrap(),
        ChainPlan::Complete
    );
    assert_eq!(
        chain_plan(&local, FrontierState::Accepted { lamport: 2, entry_hash: [2; 32] }).unwrap(),
        ChainPlan::Send(ChainStart::After { lamport: 2, entry_hash: [2; 32] })
    );
    assert_eq!(
        chain_plan(&local, FrontierState::Restore { lamport: 4, entry_hash: [4; 32] }).unwrap(),
        ChainPlan::Pending
    );
}

#[test]
fn a_tip_below_the_sender_floor_plans_a_reroot_not_a_suffix() {
    let local = ChainHead {
        device_fingerprint: [1; 32],
        lamport: 8,
        entry_hash: [8; 32],
        floor: Some((4, [4; 32])),
    };
    assert_eq!(
        chain_plan(&local, FrontierState::Accepted { lamport: 2, entry_hash: [2; 32] }).unwrap(),
        ChainPlan::Send(ChainStart::At { lamport: 4, entry_hash: [4; 32] }),
        "the receiver re-roots onto the floor instead of parking on compacted predecessors"
    );
    // A tip AT or above the floor keeps the ordinary suffix plan.
    assert_eq!(
        chain_plan(&local, FrontierState::Accepted { lamport: 6, entry_hash: [6; 32] }).unwrap(),
        ChainPlan::Send(ChainStart::After { lamport: 6, entry_hash: [6; 32] })
    );
}

/// A purge-restored receiver asks for its witness. A sender that compacted past the witness no
/// longer holds it or its direct successor, so it re-roots the receiver onto its floor, exactly
/// as it does for an accepted tip below the floor (#1481).
#[test]
fn a_restore_witness_below_the_sender_floor_plans_a_reroot() {
    let local = ChainHead {
        device_fingerprint: [1; 32],
        lamport: 8,
        entry_hash: [8; 32],
        floor: Some((4, [4; 32])),
    };
    assert_eq!(
        chain_plan(&local, FrontierState::Restore { lamport: 2, entry_hash: [2; 32] }).unwrap(),
        ChainPlan::Send(ChainStart::At { lamport: 4, entry_hash: [4; 32] }),
    );
    // A witness at or above the floor is still served from the witness itself.
    assert_eq!(
        chain_plan(&local, FrontierState::Restore { lamport: 6, entry_hash: [6; 32] }).unwrap(),
        ChainPlan::Send(ChainStart::At { lamport: 6, entry_hash: [6; 32] }),
    );
}

#[tokio::test]
async fn local_chain_inventory_enforces_the_exact_session_ceiling() {
    let shared = item("repo-a", 1);
    let mut source = MemStore::new(vec![shared.clone()]);
    let mut destination = MemStore::new(vec![shared.clone()]);
    source.insert_chain(shared.stream_id, 1, 0, 1);
    source.insert_chain(shared.stream_id, 2, 0, 2);
    let limits = TableSessionLimits {
        chains_per_page: 1,
        chains_per_session: 2,
        entries_per_page: 1,
        entries_per_session: 3,
        ..Default::default()
    };

    let (source_report, destination_report) =
        pair_with_limits(&mut source, &mut destination, limits).await;
    assert_eq!(source_report.entries_sent, 2);
    assert_eq!(destination_report.entries_newly_stored, 2);

    source.insert_chain(shared.stream_id, 3, 0, 3);
    let (source_result, peer_result) =
        try_pair_with_limits(&mut source, &mut destination, limits).await;
    assert!(matches!(
        source_result,
        Err(TableSessionError::Store(error)) if error.to_string().contains("ceiling")
    ));
    assert!(peer_result.is_err());
}

#[tokio::test]
async fn peer_chain_inventory_must_advance_order_and_respect_the_session_ceiling() {
    for devices in [vec![2, 2], vec![1, 2, 3]] {
        let shared = item("repo-a", 1);
        let streams = vec![shared.clone()];
        let mut store = MemStore::new(streams.clone());
        let limits = TableSessionLimits {
            chains_per_page: 1,
            chains_per_session: 2,
            entries_per_page: 1,
            entries_per_session: 2,
            ..Default::default()
        };
        let (mut receiver_send, mut peer_recv) = tokio::io::duplex(4096);
        let (mut peer_send, mut receiver_recv) = tokio::io::duplex(4096);
        let peer = async move {
            for device in &devices[..devices.len() - 1] {
                table_codec::write_frame(&mut peer_send, &TableFrame::ChainInventory {
                    stream_id: shared.stream_id,
                    chains: vec![ChainHead {
                        device_fingerprint: [*device; 32],
                        lamport: 0,
                        entry_hash: [*device; 32],
                        floor: None,
                    }],
                })
                .await
                .unwrap();
                assert!(matches!(
                    table_codec::read_frame(&mut peer_recv).await.unwrap(),
                    TableFrame::ChainFrontiers { .. }
                ));
                table_codec::write_frame(&mut peer_send, &TableFrame::InventoryDone {
                    stream_id: shared.stream_id,
                })
                .await
                .unwrap();
            }
            let device = devices[devices.len() - 1];
            table_codec::write_frame(&mut peer_send, &TableFrame::ChainInventory {
                stream_id: shared.stream_id,
                chains: vec![ChainHead {
                    device_fingerprint: [device; 32],
                    lamport: 0,
                    entry_hash: [device; 32],
                    floor: None,
                }],
            })
            .await
            .unwrap();
        };
        let receiver = receive_direction(
            &mut store,
            &streams,
            &mut receiver_send,
            &mut receiver_recv,
            PeerCapability::ReadWrite,
            limits,
        );
        let (result, ()) = tokio::join!(receiver, peer);
        assert!(
            matches!(result, Err(TableSessionError::Protocol(message)) if message.contains("cap"))
        );
    }
}

#[tokio::test]
async fn entry_pages_must_name_a_chain_in_the_current_inventory() {
    let shared = item("repo-a", 1);
    let streams = vec![shared.clone()];
    let mut store = MemStore::new(streams.clone());
    let (mut receiver_send, mut peer_recv) = tokio::io::duplex(4096);
    let (mut peer_send, mut receiver_recv) = tokio::io::duplex(4096);
    let peer = async move {
        table_codec::write_frame(&mut peer_send, &TableFrame::ChainInventory {
            stream_id: shared.stream_id,
            chains: vec![ChainHead {
                device_fingerprint: [1; 32],
                lamport: 0,
                entry_hash: [1; 32],
                floor: None,
            }],
        })
        .await
        .unwrap();
        assert!(matches!(
            table_codec::read_frame(&mut peer_recv).await.unwrap(),
            TableFrame::ChainFrontiers { .. }
        ));
        table_codec::write_frame(&mut peer_send, &TableFrame::Entries {
            stream_id: shared.stream_id,
            device_fingerprint: [2; 32],
            entries: vec![vec![0; 41]],
        })
        .await
        .unwrap();
    };
    let receiver = receive_direction(
        &mut store,
        &streams,
        &mut receiver_send,
        &mut receiver_recv,
        PeerCapability::ReadWrite,
        TableSessionLimits::default(),
    );
    let (result, ()) = tokio::join!(receiver, peer);
    assert!(matches!(result, Err(TableSessionError::Protocol(_))));
    assert!(store.entries.is_empty());
}

#[tokio::test]
async fn a_peer_that_stops_reading_cannot_block_writes_forever() {
    let shared = item("repo-a", 1);
    let mut store = MemStore::new(vec![shared.clone()]);
    for seed in 1..=32 {
        store.insert(shared.stream_id, seed);
    }
    let (send, _peer_recv) = tokio::io::duplex(64);
    let (mut peer_send, recv) = tokio::io::duplex(4096);
    let peer = async move {
        for frame in [
            TableFrame::Manifest(Manifest::new(vec![shared.clone()]).unwrap()),
            TableFrame::ChainFrontiers {
                stream_id: shared.stream_id,
                frontiers: vec![ChainFrontier {
                    device_fingerprint: [1; 32],
                    state: FrontierState::Empty,
                }],
            },
            TableFrame::StreamDone { stream_id: shared.stream_id, continuation_pending: false },
            TableFrame::Done,
        ] {
            table_codec::write_frame(&mut peer_send, &frame).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let (result, ()) = tokio::join!(
        run_table_session_with_limits(
            &mut store,
            send,
            recv,
            AuthRole::Dialer,
            SessionCapabilities::bidirectional(),
            TableSessionLimits { idle_timeout: Duration::from_millis(20), ..Default::default() },
        ),
        peer,
    );
    assert!(matches!(
        result,
        Err(TableSessionError::Timeout { after }) if after == Duration::from_millis(20)
    ));
}

#[tokio::test]
async fn partial_suffix_sources_report_pending_and_recover_in_either_session_role() {
    for reverse in [false, true] {
        let scope = item("repo", 1);
        let mut a = MemStore::new(vec![scope.clone()]);
        let mut b = MemStore::new(vec![scope.clone()]);
        a.insert_chain(scope.stream_id, 7, 120, 1);
        b.insert_chain(scope.stream_id, 7, 120, 1);
        b.insert_chain(scope.stream_id, 7, 150, 2);
        for store in [&mut a, &mut b] {
            store.owed_tips.insert((scope.stream_id, [7; 32]), (200, [3; 32]));
        }
        let (ar, br) =
            if reverse { pair(&mut b, &mut a).await } else { pair(&mut a, &mut b).await };
        assert!(ar.continuation_pending && br.continuation_pending);
        assert_eq!(a.frontier(&scope, [7; 32]).unwrap(), FrontierState::Accepted {
            lamport: 150,
            entry_hash: [2; 32]
        });
        let mut full = MemStore::new(vec![scope.clone()]);
        full.insert_chain(scope.stream_id, 7, 120, 1);
        full.insert_chain(scope.stream_id, 7, 150, 2);
        full.insert_chain(scope.stream_id, 7, 200, 3);
        pair(&mut a, &mut full).await;
        pair(&mut a, &mut b).await;
        let (ar, br) = pair(&mut a, &mut b).await;
        assert!(!ar.continuation_pending && !br.continuation_pending);
        assert_eq!(b.frontier(&scope, [7; 32]).unwrap(), FrontierState::Accepted {
            lamport: 200,
            entry_hash: [3; 32]
        });
    }
}
