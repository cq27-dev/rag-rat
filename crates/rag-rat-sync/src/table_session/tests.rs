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
    assert_eq!(sent.unwrap(), (1, false));
    assert_eq!(received.unwrap(), (1, 1, false));

    let (source_report, destination_report) = pair(&mut source, &mut destination).await;
    assert_eq!(source_report.entries_sent + destination_report.entries_sent, 0);
    assert_eq!(source_report.entries_newly_stored + destination_report.entries_newly_stored, 0);
}

#[test]
fn peer_frontiers_must_be_provable_prefixes_and_restore_debt_stays_pending() {
    let local =
        ChainHead { device_fingerprint: [1; 32], lamport: 3, entry_hash: [3; 32], floor: None };
    assert!(matches!(
        chain_plan(&local, FrontierState::Accepted { lamport: 3, entry_hash: [4; 32] }),
        Err(TableSessionError::Protocol(_))
    ));
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
