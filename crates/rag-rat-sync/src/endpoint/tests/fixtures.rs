//! Shared database and loopback endpoint fixtures.

use super::*;

pub(super) const NOW: i64 = 1_700_000_000_000;

pub(super) fn database() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
    conn
}

pub(super) fn test_entry(seed: u8) -> ([u8; 32], Vec<u8>) {
    ([seed; 32], vec![seed; 40])
}

/// Two iroh endpoints on loopback UDP with the relay disabled — no network, no relay, just a
/// real QUIC transport between in-process endpoints.
pub(super) async fn loopback_endpoints() -> (Endpoint, Endpoint) {
    let bind = |seed: [u8; 32]| async move {
        Endpoint::builder(presets::Minimal)
            .alpns(vec![
                SYNC_ALPN.to_vec(),
                CONTENT_SYNC_ALPN.to_vec(),
                TABLE_SYNC_ALPN.to_vec(),
                ENROLL_ALPN.to_vec(),
            ])
            .relay_mode(RelayMode::Disabled)
            .secret_key(SecretKey::from_bytes(&seed))
            .bind()
            .await
            .unwrap()
    };
    (bind([0x11; 32]).await, bind([0x12; 32]).await)
}

pub(super) fn direct_addr(endpoint: &Endpoint) -> EndpointAddr {
    let port = endpoint
        .addr()
        .ip_addrs()
        .next()
        .expect("a bound endpoint advertises at least one socket address")
        .port();
    EndpointAddr::new(endpoint.id())
        .with_ip_addr(std::net::SocketAddr::from(([127, 0, 0, 1], port)))
}
