//! Shared HTTP substrate for the papertrail provider clients (#589): an async `reqwest`/rustls
//! transport on the workspace tokio, a per-(provider, host, token) rate governor that keeps a
//! user reserve (default 35%) of header-reported quota untouched, and secret-free auth
//! resolution (env var / `token_command`). NO provider logic lives here — URL building,
//! pagination, and payload mapping belong to the per-provider clients built on top (#591+).
//!
//! The transport's futures are driven through the papertrail module's `block_on` bridge at the
//! synchronous entry points, exactly like the `PapertrailClient` trait methods.

mod auth;
mod client;
mod governor;

pub(crate) use auth::*;
pub(crate) use client::*;
pub(crate) use governor::*;

// Not test-gated: the engine crate's autosync tests drive mirror flights through this stub, so
// it ships as ordinary (never-in-production-paths) test support.
pub mod stub;

#[cfg(test)]
mod pagination_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::stub::{StubResponse, spawn_script_stub};
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    /// Fake wall clock: every governor decision reads it, so pause/resume runs deterministically
    /// with zero real sleeping.
    fn fake_clock(now: &Arc<AtomicI64>) -> Arc<dyn Fn() -> i64 + Send + Sync> {
        let now = Arc::clone(now);
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    fn drive<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    /// The generic paginated-fetch shape the provider clients will use: fetch pages from the
    /// cursor, collecting items, until the stream ends — or the governor pauses, in which case
    /// the cursor is KEPT where it stands so the next window resumes mid-pagination.
    fn fetch_from_cursor(
        transport: &Transport,
        base: &str,
        cursor: &mut u32,
        collected: &mut Vec<i64>,
    ) -> Result<(), TransportError> {
        drive(async {
            loop {
                let response = transport.get(&format!("{base}/items?page={cursor}"), &[]).await?;
                let page: serde_json::Value =
                    serde_json::from_str(&response.body).expect("stub page json");
                collected.extend(
                    page["items"].as_array().expect("items").iter().map(|v| v.as_i64().unwrap()),
                );
                match page["next"].as_u64() {
                    Some(next) => *cursor = next as u32,
                    None => return Ok(()),
                }
            }
        })
    }

    // The issue's acceptance scenario end-to-end: a paginated fetch whose second page's quota
    // headers fall to the reserve pauses BEFORE the third request, keeps its cursor, and the
    // next window resumes mid-pagination without dropping or duplicating a single item.
    #[test]
    fn paginated_fetch_pauses_at_the_reserve_and_resumes_without_loss_or_duplication() {
        let reset_epoch_s = T0 / 1000 + 60;
        let (url, handle) = spawn_script_stub(vec![
            // Page 1: plenty of quota left (80 > 35 = 0.35 × 100).
            StubResponse::ok_with_quota(r#"{"items":[1,2],"next":2}"#, 100, 80, reset_epoch_s),
            // Page 2: the response itself succeeds, but its headers report the reserve reached
            // (30 <= 35) — the pause lands before the NEXT request.
            StubResponse::ok_with_quota(r#"{"items":[3,4],"next":3}"#, 100, 30, reset_epoch_s),
            // Page 3, served in the next window: quota replenished, stream ends.
            StubResponse::ok_with_quota(r#"{"items":[5,6]}"#, 100, 95, reset_epoch_s + 3600),
        ]);
        let now = Arc::new(AtomicI64::new(T0));
        let registry = GovernorRegistry::default();
        let transport = Transport::with_clock(
            TransportParams {
                provider: "github",
                lane: "core",
                host: "127.0.0.1",
                auth: None,
                registry: &registry,
                options: TransportOptions::default(),
            },
            fake_clock(&now),
        )
        .expect("transport");

        let mut cursor = 1;
        let mut collected = Vec::new();
        let err = fetch_from_cursor(&transport, &url, &mut cursor, &mut collected)
            .expect_err("must pause at the reserve");
        let (resume_at_ms, reason) = err.pause_details().expect("expected Paused");
        assert_eq!(reason, PauseReason::QuotaReserve);
        assert_eq!(resume_at_ms, reset_epoch_s * 1000, "resume anchors to the provider reset");
        assert_eq!(collected, vec![1, 2, 3, 4], "both delivered pages are kept");
        assert_eq!(cursor, 3, "the cursor stays on the unfetched page");

        // Retrying within the same window stays paused — and never touches the network.
        let mut early_cursor = cursor;
        let mut early_items = collected.clone();
        assert!(matches!(
            fetch_from_cursor(&transport, &url, &mut early_cursor, &mut early_items),
            Err(TransportError::Paused { .. })
        ));
        assert_eq!(early_cursor, 3, "a paused retry moves nothing");

        // The next window: resume from the KEPT cursor.
        now.store(resume_at_ms + 1, Ordering::SeqCst);
        fetch_from_cursor(&transport, &url, &mut cursor, &mut collected).expect("resumes cleanly");
        assert_eq!(collected, vec![1, 2, 3, 4, 5, 6], "no item dropped, none duplicated");

        // The wire agrees: each page was fetched exactly once, in order.
        let pages: Vec<String> = handle
            .join()
            .unwrap()
            .iter()
            .map(|head| head.lines().next().unwrap().to_string())
            .collect();
        assert_eq!(pages, vec![
            "GET /items?page=1 HTTP/1.1",
            "GET /items?page=2 HTTP/1.1",
            "GET /items?page=3 HTTP/1.1",
        ]);
    }
}
