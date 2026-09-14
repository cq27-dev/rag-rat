//! The global accept-rate and egress limiters.

use super::*;

#[test]
fn accept_rate_admits_a_burst_then_denies() {
    let mut limiter = GlobalAcceptRateLimiter::new();
    // The full burst is admitted at one instant...
    for i in 0..ACCEPT_BURST as usize {
        assert!(limiter.allow(NOW), "burst connection {i} within capacity");
    }
    // ...and the next one at the same instant is denied.
    assert!(!limiter.allow(NOW), "the connection past the burst is refused");
}

#[test]
fn egress_bounds_bytes_then_refills() {
    let mut limiter = GlobalEgressLimiter::new();
    // A page is permitted while any credit remains, even one larger than the whole burst
    // (forward progress), driving the balance to zero-or-below.
    assert!(limiter.allow(EGRESS_BURST_BYTES as usize, NOW), "the burst is servable");
    assert!(!limiter.allow(1, NOW), "a further page at the same instant is refused (no credit)");
    // One second refills `EGRESS_REFILL_BYTES_PER_SEC`, so serving resumes.
    assert!(limiter.allow(1, NOW + 1000), "refilled credit permits serving again after 1s");
}

#[test]
fn accept_rate_refills_over_time() {
    let mut limiter = GlobalAcceptRateLimiter::new();
    while limiter.allow(NOW) {} // drain the burst
    assert!(!limiter.allow(NOW), "drained");
    // One second later, exactly `ACCEPT_REFILL_PER_SEC` tokens are available again.
    let later = NOW + 1000;
    for i in 0..ACCEPT_REFILL_PER_SEC as usize {
        assert!(limiter.allow(later), "refilled token {i} available after 1s");
    }
    assert!(!limiter.allow(later), "no more than the per-second refill accrues in 1s");
}

#[test]
fn accept_rate_refill_is_capped_at_the_burst() {
    let mut limiter = GlobalAcceptRateLimiter::new();
    while limiter.allow(NOW) {} // drain
    // A long idle must not accrue unbounded credit — only up to the burst.
    let long_idle = NOW + 1_000_000;
    for i in 0..ACCEPT_BURST as usize {
        assert!(limiter.allow(long_idle), "capped-refill token {i}");
    }
    assert!(!limiter.allow(long_idle), "idle time accrues at most one burst, not more");
}

#[test]
fn accept_rate_never_denies_traffic_under_the_rate() {
    let mut limiter = GlobalAcceptRateLimiter::new();
    // One connection every 250ms = 4/s, well under the 8/s refill — never denied over a long
    // run.
    for tick in 0..200 {
        let now = NOW + tick * 250;
        assert!(limiter.allow(now), "steady sub-rate traffic at tick {tick} is admitted");
    }
}

#[tokio::test]
async fn a_drained_accept_rate_refuses_a_connection_before_the_handshake() {
    let (listener, dialer) = loopback_endpoints().await;
    let mut limiter = GlobalAcceptRateLimiter::new();
    while limiter.allow(NOW) {} // drain so the next accept is refused

    let server = accept_connection_within_rate(&listener, &mut limiter, || NOW);
    let client = async {
        // The refused `Incoming` makes the dial fail rather than establish a session.
        timeout(DEFAULT_IDLE_TIMEOUT, dialer.connect(direct_addr(&listener), SYNC_ALPN)).await
    };
    let (server_result, client_result) = tokio::join!(server, client);

    assert!(
        matches!(server_result, Ok(None)),
        "a drained limiter refuses at the Incoming stage: {server_result:?}"
    );
    assert!(
        matches!(client_result, Ok(Err(_)) | Err(_)),
        "the dialer's connection does not establish"
    );
}

#[tokio::test]
async fn the_accept_rate_clock_is_read_when_the_peer_connects_not_before_the_wait() {
    // Drain the bucket, then let the connection arrive an HOUR later. The limiter must refill
    // against the CONNECT time — read via the closure after `accept()` resolves — not a
    // timestamp captured before the idle wait. With an eagerly-captured clock this
    // would wrongly refuse a legitimate connection after any load-then-idle stretch;
    // the closure makes it admit.
    let (listener, dialer) = loopback_endpoints().await;
    let mut limiter = GlobalAcceptRateLimiter::new();
    while limiter.allow(NOW) {}
    let connect_at = NOW + 3_600_000;

    let server = accept_connection_within_rate(&listener, &mut limiter, move || connect_at);
    let client = async {
        timeout(DEFAULT_IDLE_TIMEOUT, dialer.connect(direct_addr(&listener), SYNC_ALPN)).await
    };
    let (server_result, client_result) = tokio::join!(server, client);

    assert!(
        matches!(server_result, Ok(Some(_))),
        "the bucket refilled to the connect time admits the delayed connection: {server_result:?}"
    );
    assert!(matches!(client_result, Ok(Ok(_))), "the dialer connects: {client_result:?}");
}
