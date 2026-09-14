//! The reconcile loop's stop rule and table reconciliation over iroh.

use super::*;

async fn accept_test_table_sync(
    endpoint: &Endpoint,
    store: &mut TableTestStore,
) -> TableSessionReport {
    let incoming = endpoint.accept().await.unwrap();
    let conn = incoming.await.unwrap();
    assert_eq!(conn.alpn(), TABLE_SYNC_ALPN);
    let local_node = *endpoint.id().as_bytes();
    let remote_node = *conn.remote_id().as_bytes();
    let (mut send, mut recv) = conn.accept_bi().await.unwrap();
    let (capabilities, _admission) = run_auth_phase(&mut send, &mut recv, &*store, AuthConfig {
        role: AuthRole::Acceptor,
        account_id: store.account_id(),
        local_node,
        remote_node,
        policy: AuthPolicy::Closed,
        now_ms: NOW,
        pre_auth_timeout: DEFAULT_PRE_AUTH_TIMEOUT,
    })
    .await
    .unwrap();
    let report =
        run_table_session(store, send, recv, AuthRole::Acceptor, capabilities).await.unwrap();
    let _ = timeout(GRACEFUL_CLOSE_TIMEOUT, conn.closed()).await;
    conn.close(0u32.into(), b"done");
    report
}

async fn reconcile_test_tables(
    listener: &Endpoint,
    dialer: &Endpoint,
    source: &mut TableTestStore,
    destination: &mut TableTestStore,
) -> ReconcileReport {
    let client = connect_and_table_reconcile(
        dialer,
        direct_addr(listener),
        destination,
        || NOW,
        MAX_RECONCILE_ROUNDS,
    );
    tokio::pin!(client);
    loop {
        tokio::select! {
            report = &mut client => break report.unwrap(),
            _ = accept_test_table_sync(listener, source) => {},
        }
    }
}

fn table_item(repo: &str, stream: u8) -> crate::table_wire::ManifestItem {
    crate::table_wire::ManifestItem {
        repo_id: repo.into(),
        incarnation_ref: [1; 32],
        scope_id: "anchors/1".into(),
        stream_id: [stream; 32],
    }
}

#[test]
fn reconcile_step_loops_until_a_fully_quiet_round() {
    let moved =
        |newly_stored, sent, received| RoundTally::default().record(newly_stored, sent, received);
    // Nothing moved in either direction — the fixpoint.
    let quiet = moved(0, 0, 0);
    assert_eq!(reconcile_step(quiet, 1, 8), ReconcileStep::Stop { converged: true });
    // Each direction of movement, on its own, keeps the loop going under the cap: `stored`
    // (local promotion), `received` (peer still has data), and `sent` (our push may have made
    // the acceptor evict — a quiet confirmation round must prove the re-push landed).
    let stored = moved(2, 0, 0);
    let received = moved(0, 0, 5);
    let sent = moved(0, 3, 0);
    for round in [stored, received, sent] {
        assert_eq!(reconcile_step(round, 1, 8), ReconcileStep::Continue);
    }
    // Still moving at the cap stops UN-converged so a later maintenance pass continues.
    assert_eq!(reconcile_step(sent, 8, 8), ReconcileStep::Stop { converged: false });
    // A quiet round at the cap is still the converged fixpoint.
    assert_eq!(reconcile_step(quiet, 8, 8), ReconcileStep::Stop { converged: true });
}

#[tokio::test]
async fn table_reconcile_transfers_only_the_scoped_intersection_over_iroh() {
    let account = [0xa3; 32];
    let shared = table_item("repo-shared", 1);
    let source_only = table_item("repo-source", 2);
    let destination_only = table_item("repo-destination", 3);
    let shared_entry = test_entry(11);
    let private_entry = test_entry(12);
    let dialer_entry = test_entry(13);
    let mut source = TableTestStore::new(account, vec![source_only.clone(), shared.clone()], [
        (shared.stream_id, shared_entry.clone()),
        (source_only.stream_id, private_entry),
    ]);
    let mut destination = TableTestStore::new(account, vec![shared.clone(), destination_only], [(
        shared.stream_id,
        dialer_entry.clone(),
    )]);
    let (listener, dialer) = loopback_endpoints().await;

    let report = reconcile_test_tables(&listener, &dialer, &mut source, &mut destination).await;
    assert_eq!(report.rounds, 2, "one round transfers, one confirms the fixpoint");
    assert!(report.converged);
    assert_eq!(report.entries_newly_stored, 1);
    assert_eq!(report.entries_sent, 1);
    assert_eq!(destination.entries[&shared.stream_id][&shared_entry.0], shared_entry.1);
    assert_eq!(
        source.entries[&shared.stream_id][&dialer_entry.0], dialer_entry.1,
        "the acceptor stores the dialer's push before the dialer closes",
    );
    assert!(
        !destination.entries.contains_key(&source_only.stream_id),
        "a repo outside the manifest intersection never crosses the connection",
    );

    let again = reconcile_test_tables(&listener, &dialer, &mut source, &mut destination).await;
    assert_eq!(again.rounds, 1, "an idempotent replay is immediately quiet");
    assert!(again.converged);
    assert_eq!(again.entries_newly_stored, 0);
    assert_eq!(again.entries_sent, 0);
}
