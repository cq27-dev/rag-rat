use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::{Query, State};
use axum::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, AUTHORIZATION, ORIGIN};
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use rag_rat_base::config::{Config, ResolvedTarget, TargetKind};
use rag_rat_base::language::Language;
use rag_rat_core::IndexDatabase;
use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
use serde_json::Value;
use tower::ServiceExt;

use super::{
    FileClonesQuery, HttpState, ServeControl, ServeOptions, file_clones, hop_limit, router, run_db,
    serve,
};

#[tokio::test]
async fn serve_control_observes_a_stop_that_precedes_the_waiter() {
    let control = ServeControl::default();
    control.stop();
    tokio::time::timeout(Duration::from_millis(100), control.stopped())
        .await
        .expect("a pre-triggered stop must not lose its notification");
}

#[tokio::test]
async fn non_loopback_serving_requires_authentication() {
    let (_root, config) = test_config();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let error = serve(
        listener,
        config,
        ServeOptions::default(),
        std::future::pending::<std::io::Result<()>>(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn health_is_available_without_opening_the_database() {
    let (_root, config) = test_config();
    let response = router(config, ServeOptions::default())
        .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("access-control-allow-origin").is_none(),
        "unauthenticated loopback serving must not opt arbitrary browser origins into reading"
    );
    assert_eq!(json_body(response).await, serde_json::json!({"status": "ok"}));
}

#[tokio::test]
async fn bearer_auth_and_exact_origin_are_enforced() {
    let (_root, config) = test_config();
    let app = router(config, ServeOptions {
        auth_token: Some("test-token".into()),
        allowed_origins: vec!["https://lens.example".into()],
        ..ServeOptions::default()
    });

    let unauthorized = app
        .clone()
        .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let forbidden = app
        .clone()
        .oneshot(
            Request::get("/api/health")
                .header(AUTHORIZATION, "Bearer test-token")
                .header(ORIGIN, "https://attacker.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let allowed = app
        .oneshot(
            Request::get("/api/health")
                .header(AUTHORIZATION, "Bearer test-token")
                .header(ORIGIN, "https://lens.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("https://lens.example"))
    );
}

#[tokio::test]
async fn an_allowed_origin_sees_cors_headers_on_auth_failure() {
    let (_root, config) = test_config();
    let app = router(config, ServeOptions {
        auth_token: Some("test-token".into()),
        allowed_origins: vec!["https://lens.example".into()],
        ..ServeOptions::default()
    });
    let response = app
        .oneshot(
            Request::get("/api/health")
                .header(ORIGIN, "https://lens.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("https://lens.example")),
        "an allowed origin must read its own 401 instead of an opaque browser CORS error"
    );
}

/// A CORS preflight cannot carry the bearer token, so it must be answered BEFORE the auth check
/// or a browser extension host can never reach the API at all. Chrome additionally blocks a
/// public page from reaching loopback unless the preflight opts into Private Network Access —
/// emitted only for an exact-allowlisted origin, never by default.
#[tokio::test]
async fn cors_preflight_is_answered_before_auth_and_opts_into_private_network_access() {
    let (_root, config) = test_config();
    let app = router(config, ServeOptions {
        auth_token: Some("test-token".into()),
        allowed_origins: vec!["https://lens.example".into()],
        ..ServeOptions::default()
    });
    let private_network =
        axum::http::HeaderName::from_static("access-control-allow-private-network");

    // Allowed origin: answered without a token, with CORS headers and the PNA opt-in.
    let allowed = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/file/memories")
                .header(ORIGIN, "https://lens.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        allowed.status(),
        StatusCode::NO_CONTENT,
        "a preflight carries no credentials and must not 401"
    );
    assert_eq!(
        allowed.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("https://lens.example"))
    );
    assert_eq!(
        allowed.headers().get(&private_network),
        Some(&HeaderValue::from_static("true")),
        "Chrome blocks public-page → loopback without this opt-in"
    );

    // No origin: still answered, but nothing is opted into.
    let originless = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/file/memories")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(originless.status(), StatusCode::NO_CONTENT);
    assert!(
        originless.headers().get(&private_network).is_none(),
        "a request with no allowed origin must not widen private-network access"
    );

    // Disallowed origin: refused at the origin gate, ahead of the preflight answer.
    let forbidden = app
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/file/memories")
                .header(ORIGIN, "https://attacker.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    assert!(forbidden.headers().get(&private_network).is_none());
}

#[tokio::test]
async fn events_emits_the_current_version_as_sse() {
    let (root, config) = test_config();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn current() {}\n").unwrap();
    drop(IndexDatabase::rebuild(&config).unwrap());
    let update_config = config.clone();

    let control = ServeControl::default();
    let response =
        router(config, ServeOptions { control: control.clone(), ..ServeOptions::default() })
            .oneshot(Request::get("/api/events").body(Body::empty()).unwrap())
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
    );
    let mut body = response.into_body().into_data_stream();
    let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .expect("initial version event timeout")
        .expect("SSE body ended")
        .expect("SSE body error");
    let event = String::from_utf8(chunk.to_vec()).unwrap();
    assert!(event.contains("event: version"), "{event}");
    assert!(event.contains("\"generation\":"), "{event}");

    assert!(
        tokio::time::timeout(Duration::from_millis(2_500), body.next()).await.is_err(),
        "an unchanged version must not emit a duplicate event"
    );
    let db = IndexDatabase::open_config(&update_config).unwrap();
    db.memory_create(RepoMemoryCreate {
        kind: "Invariant".into(),
        title: "Refresh the lens".into(),
        body: "A memory-only write must notify connected clients".into(),
        confidence: "high".into(),
        created_by: Some("test".into()),
        source: None,
        payload_json: None,
        tags: Vec::new(),
        bind: RepoMemoryBindTarget {
            path: Some("src/lib.rs".into()),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap();
    drop(db);
    let memory_change = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .expect("memory change event timeout")
        .expect("SSE body ended")
        .expect("SSE body error");
    let memory_change = String::from_utf8(memory_change.to_vec()).unwrap();
    assert!(memory_change.contains("event: version"), "{memory_change}");

    fs::write(root.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
    drop(IndexDatabase::rebuild(&update_config).unwrap());
    let changed = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .expect("changed version event timeout")
        .expect("SSE body ended")
        .expect("SSE body error");
    let changed = String::from_utf8(changed.to_vec()).unwrap();
    assert!(changed.contains("event: version"), "{changed}");
    assert_ne!(changed, event, "a changed index emits a new version token");
    control.stop();
    let end = tokio::time::timeout(Duration::from_millis(100), body.next())
        .await
        .expect("SSE shutdown must not wait for the polling interval");
    assert!(end.is_none(), "SSE body must end when serving stops");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn events_ends_when_a_version_probe_can_no_longer_open_the_index() {
    let (root, config) = test_config();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn current() {}\n").unwrap();
    drop(IndexDatabase::rebuild(&config).unwrap());
    let database = config.database.clone();

    let response = router(config, ServeOptions::default())
        .oneshot(Request::get("/api/events").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let mut body = response.into_body().into_data_stream();
    tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .expect("initial version event timeout")
        .expect("SSE body ended before its initial version")
        .expect("SSE body error");

    fs::remove_file(database).unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .expect("failed version probes must close the SSE stream");
    assert!(end.is_none(), "a failed version probe must disconnect the client");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn clones_rejects_invalid_theta_and_paths() {
    let (_root, config) = test_config();
    let app = router(config, ServeOptions::default());
    for uri in [
        "/api/file/clones?path=src/a.rs&theta=nan",
        "/api/file/clones?path=src/a.rs&theta=0.2",
        "/api/file/clones?path=../secret.rs",
        "/api/file/clones?path=",
    ] {
        let response =
            app.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn new_routes_reject_missing_or_invalid_parameters() {
    let (_root, config) = test_config();
    let app = router(config, ServeOptions::default());
    for uri in [
        "/api/file/symbols",
        "/api/file/graph?path=../secret.rs",
        "/api/file/coupling",
        "/api/file/memories?path=../secret.rs",
        "/api/file/papertrail",
        "/api/symbol/callers",
        "/api/symbol/callees?qname=",
        // An empty `id` falls through to the qualified name, which is also absent.
        "/api/symbol/callers?id=",
        // Neither is a `sym_<hex>` handle, and neither may be read as one.
        "/api/symbol/callers?id=12345",
        "/api/symbol/callees?id=sym_zzz",
        "/api/chunk/text",
        "/api/chunk/text?chunk_id=nope",
        "/api/chunk/text?chunk_id=0",
    ] {
        let response =
            app.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert!(json_body(response).await["error"].is_string(), "{uri}");
    }
    assert_eq!(hop_limit(None), 50);
    assert_eq!(hop_limit(Some("invalid")), 50);
    assert_eq!(hop_limit(Some("0")), 1);
    assert_eq!(hop_limit(Some("-9")), 1);
    assert_eq!(hop_limit(Some("9999")), 500);
}

#[tokio::test]
async fn symbol_graph_hops_and_chunk_routes_preserve_endpoint_wrappers() {
    let (root, config) = test_config();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub enum Req { Upsert { id: i64 } }
pub fn target(_id: i64) {}
pub fn caller() { target(1); }
#[test]
fn target_test() { target(2); }
pub fn enqueue() { send(Req::Upsert { id: 3 }); }
fn send(_req: Req) {}
pub fn handle(req: Req) {
    match req { Req::Upsert { id } => target(id) }
}
"#,
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let target_qname = db
        .symbols("target", None, 10)
        .unwrap()
        .into_iter()
        .find(|hit| hit.name == "target")
        .unwrap()
        .qualified_name;
    let caller_qname = db
        .symbols("caller", None, 10)
        .unwrap()
        .into_iter()
        .find(|hit| hit.name == "caller")
        .unwrap()
        .qualified_name;
    let chunk_id = db
        .search("caller", 10, false)
        .unwrap()
        .into_iter()
        .find(|hit| hit.symbol_path.as_deref() == Some(caller_qname.as_str()))
        .unwrap()
        .chunk_id;
    drop(db);

    let app = router(config, ServeOptions::default());
    let symbols = get_json(&app, "/api/file/symbols?path=src/lib.rs").await;
    let symbol_rows = symbols["symbols"].as_array().unwrap();
    assert!(symbol_rows.windows(2).all(|pair| {
        pair[0]["start_line"].as_i64().unwrap() <= pair[1]["start_line"].as_i64().unwrap()
    }));
    let target = symbol_rows.iter().find(|row| row["name"] == "target").unwrap();
    for field in [
        "name",
        "qname",
        "kind",
        "start_line",
        "end_line",
        "is_test",
        "signature",
        "fan_in",
        "fan_out",
    ] {
        assert!(target.get(field).is_some(), "missing symbol field {field}: {target}");
    }

    let graph = get_json(&app, "/api/file/graph?path=src/lib.rs").await;
    let target =
        graph["symbols"].as_array().unwrap().iter().find(|row| row["name"] == "target").unwrap();
    assert!(
        target["callers"]["exact"].as_u64().unwrap()
            + target["callers"]["syntactic"].as_u64().unwrap()
            >= 3
    );
    assert!(target["callers"]["tests"].as_u64().unwrap() >= 1);
    assert!(target["callers"]["dispatch"].as_u64().unwrap() >= 1);
    assert!(target["fan_in_score"].as_f64().unwrap() >= 2.0);
    assert!(target["dispatch"].as_array().unwrap().iter().any(|row| row["direction"] == "handled"));

    let coupling = get_json(&app, "/api/file/coupling?path=src/lib.rs").await;
    assert_eq!(coupling, serde_json::json!({"coupling": []}));
    let memories = get_json(&app, "/api/file/memories?path=src/lib.rs").await;
    assert_eq!(memories, serde_json::json!({"memories": []}));
    let papertrail = get_json(&app, "/api/file/papertrail?path=src/lib.rs").await;
    assert_eq!(papertrail, serde_json::json!({"refs": [], "decisions": []}));

    let callers =
        get_json(&app, &format!("/api/symbol/callers?qname={target_qname}&limit=9999")).await;
    assert!(callers["callers"].as_array().unwrap().iter().any(|row| row["name"] == "caller"));
    assert!(callers["callers"][0].get("id").is_none());
    assert!(callers["callers"][0].get("source_file_id").is_none());

    let callees =
        get_json(&app, &format!("/api/symbol/callees?qname={caller_qname}&limit=invalid")).await;
    assert!(callees["callees"].as_array().unwrap().iter().any(|row| row["name"] == "target"));

    let chunk = get_json(&app, &format!("/api/chunk/text?chunk_id={chunk_id}")).await;
    assert_eq!(chunk["chunk_id"], chunk_id);
    assert!(chunk["text"].as_str().unwrap().contains("target(1)"));
    let missing = app
        .clone()
        .oneshot(
            Request::get("/api/chunk/text?chunk_id=9223372036854775807")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_body(missing).await, serde_json::json!({"error": "unknown chunk_id"}));

    // A chunk cached before an on-disk edit is expected editor-facing staleness, not a server
    // failure — it must surface as 404 without an error log, never 500.
    fs::write(root.join("src/lib.rs"), "pub fn replaced_entirely() {}\n").unwrap();
    let stale = app
        .oneshot(
            Request::get(format!("/api/chunk/text?chunk_id={chunk_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);
    assert!(
        json_body(stale).await["error"].as_str().unwrap().contains("stale"),
        "stale chunk must report as a 404"
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn clones_uses_the_native_lens_composition() {
    let (root, config) = test_config();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(db: Db) -> i32 { let u = db.get(20); validate(u); u + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.precompute_clone_graph(None).unwrap();
    drop(db);

    let app = router(config, ServeOptions {
        indexed_root: "crate".into(),
        case_insensitive_paths: true,
        ..ServeOptions::default()
    });
    let status = app
        .clone()
        .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status_code = status.status();
    let status = json_body(status).await;
    assert_eq!(status_code, StatusCode::OK, "{status}");
    assert_eq!(status["repo_root"], root.display().to_string());
    assert_eq!(status["indexed_root"], "crate");
    // The checkout a hosted client binds itself to: a repository's linked worktrees share one
    // `repo_id`, so without this the client cannot tell them apart. Compare against the CANONICAL
    // path, which is what the server derives it from: the platforms that hand a process a
    // non-canonical temp path — macOS resolving `/var` to `/private/var`, Windows expanding an 8.3
    // name like `RUNNER~1` — would otherwise fail this on a value that is perfectly correct.
    assert_eq!(
        status["worktree_id"],
        root.canonicalize().unwrap_or_else(|_| root.clone()).display().to_string(),
    );
    assert_eq!(status["case_insensitive_paths"], true);
    assert_eq!(status["live_file_count"], 2);

    let version = app
        .clone()
        .oneshot(Request::get("/api/version").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(version.status(), StatusCode::OK);
    assert!(json_body(version).await["max_indexed_at_ms"].as_i64().unwrap() > 0);

    let treemap = get_json(&app, "/api/treemap").await;
    assert_eq!(treemap["files"].as_array().unwrap().len(), 2);
    let first = &treemap["files"][0];
    for field in [
        "path",
        "language",
        "kind",
        "loc",
        "churn_commits",
        "churn_lines",
        "fan_in",
        "fan_out",
        "dup_partners",
        "dup_max_similarity",
        "memories",
    ] {
        assert!(first.get(field).is_some(), "missing treemap field {field}: {first}");
    }

    let response = app
        .oneshot(
            Request::get("/api/file/clones?path=SRC/A.RS&theta=0.7&min_tokens=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["clone_regions"][0]["symbol"], "load_user");
    assert_eq!(body["clone_regions"][0]["partners"][0]["path"], "src/b.rs");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn blocking_database_work_obeys_the_timeout() {
    let (root, config) = test_config();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    drop(IndexDatabase::rebuild(&config).unwrap());
    let options = ServeOptions {
        timeout: Duration::from_millis(10),
        blocking_workers: 1,
        ..ServeOptions::default()
    };
    let mut state = HttpState {
        config,
        options,
        blocking_workers: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        clone_graph_cache: std::sync::Arc::default(),
        version_cache: std::sync::Arc::default(),
        treemap_cache: std::sync::Arc::default(),
        clone_graph_gate: std::sync::Arc::default(),
    };
    let error = run_db(state.clone(), |db, _, cancelled| {
        while !cancelled.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        db.lens_treemap_with_cancel(cancelled)?;
        Ok(())
    })
    .await
    .unwrap_err();
    assert_eq!(error.into_response().status(), StatusCode::GATEWAY_TIMEOUT);
    state.options.timeout = Duration::from_secs(1);
    run_db(state, |_, _, _| Ok(()))
        .await
        .expect("a cooperatively cancelled request must release the sole worker permit");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn dropping_a_request_cancels_its_database_work() {
    let (root, config) = test_config();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    drop(IndexDatabase::rebuild(&config).unwrap());
    let state = HttpState {
        config,
        options: ServeOptions { blocking_workers: 1, ..ServeOptions::default() },
        blocking_workers: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        clone_graph_cache: std::sync::Arc::default(),
        version_cache: std::sync::Arc::default(),
        treemap_cache: std::sync::Arc::default(),
        clone_graph_gate: std::sync::Arc::default(),
    };
    let cancelled_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task_observed = std::sync::Arc::clone(&cancelled_observed);
    let mut request = Box::pin(run_db(state.clone(), move |_, _, cancelled| {
        while !cancelled.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        task_observed.store(true, Ordering::Release);
        Ok(())
    }));
    // Poll far enough for the blocking work to start, then drop the future the way axum drops a
    // request whose client disconnected — the editor aborts a file lane on every index change.
    assert!(tokio::time::timeout(Duration::from_millis(250), &mut request).await.is_err());
    drop(request);

    tokio::time::timeout(Duration::from_secs(10), async {
        while !cancelled_observed.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a dropped request must cancel the database work it started");
    run_db(state, |_, _, _| Ok(()))
        .await
        .expect("cancelling the abandoned work releases the sole worker permit");
    let _ = fs::remove_dir_all(root);
}

/// A clone read that is waiting its turn at the repository-wide graph must wait OUTSIDE the
/// database worker pool. Two visible editors are enough to produce two cold clone reads, and if
/// the loser waits on a worker it holds a permit while doing nothing — starving every independent
/// graph, memory, coupling, and papertrail lane behind it for the length of the build.
#[tokio::test]
async fn a_queued_clone_read_holds_no_database_worker() {
    let (root, config) = test_config();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    drop(IndexDatabase::rebuild(&config).unwrap());
    let state = HttpState {
        config,
        options: ServeOptions {
            timeout: Duration::from_secs(10),
            blocking_workers: 1,
            ..ServeOptions::default()
        },
        blocking_workers: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        clone_graph_cache: std::sync::Arc::default(),
        version_cache: std::sync::Arc::default(),
        treemap_cache: std::sync::Arc::default(),
        clone_graph_gate: std::sync::Arc::default(),
    };

    // Stand in for a cold clone-graph build already in flight.
    let building = std::sync::Arc::clone(&state.clone_graph_gate).lock_owned().await;
    let mut queued = Box::pin(file_clones(
        State(state.clone()),
        Query(FileClonesQuery { path: Some("src/a.rs".into()), theta: None, min_tokens: None }),
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut queued).await.is_err(),
        "the second clone read must park behind the in-flight build"
    );

    // The sole worker is what an independent lane needs, and the parked read must not be holding
    // it.
    tokio::time::timeout(Duration::from_secs(5), run_db(state, |_, _, _| Ok(())))
        .await
        .expect("a queued clone read must leave the database worker free")
        .expect("the independent read must succeed while the clone read waits");

    drop(building);
    let Json(clones) =
        queued.await.expect("the queued clone read completes once the build releases the graph");
    assert!(clones.clone_regions.is_empty(), "a one-function corpus has no clone regions");
    let _ = fs::remove_dir_all(root);
}

/// Two `run` overloads sharing `src/lib.rs::run`, each with its own caller and its own callee.
const OVERLOAD_SOURCE: &str = r#"
pub struct Alpha;
pub struct Beta;

pub fn alpha_leaf() {}
pub fn beta_leaf() {}

impl Alpha {
    pub fn run(&self) { alpha_leaf(); }
}

impl Beta {
    pub fn run(&self, extra: i64) { beta_leaf(); let _ = extra; }
}

pub fn calls_alpha(alpha: &Alpha) { alpha.run(); }
pub fn calls_beta(beta: &Beta) { beta.run(1); }
"#;

/// The whole protocol on one fixture: `/api/file/graph` hands out a handle per overload, and the
/// hop routes answer each handle with that overload's own neighbours. The `qname` fallback stays
/// reachable and says, in the response, that it covered two symbols at once.
#[tokio::test]
async fn hop_routes_prefer_the_symbol_handle_over_the_shared_qualified_name() {
    let (root, config) = test_config();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), OVERLOAD_SOURCE).unwrap();
    drop(IndexDatabase::rebuild(&config).unwrap());
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    resolve_overload_call_sites(&conn);
    drop(conn);
    let app = router(config, ServeOptions::default());

    let graph = get_json(&app, "/api/file/graph?path=src/lib.rs").await;
    let handles = graph["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "run")
        .map(|row| row["id"].as_str().expect("every graph row carries its handle").to_string())
        .collect::<Vec<_>>();
    assert_eq!(handles.len(), 2);
    assert_ne!(handles[0], handles[1], "overloads must not share one handle");
    let symbols = get_json(&app, "/api/file/symbols?path=src/lib.rs").await;
    assert_eq!(
        symbols["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["name"] == "run")
            .map(|row| row["id"].clone())
            .collect::<Vec<_>>(),
        handles.iter().map(|id| Value::String(id.clone())).collect::<Vec<_>>(),
        "both file lanes must agree on the handle"
    );

    for (handle, caller, callee) in
        [(&handles[0], "calls_alpha", "alpha_leaf"), (&handles[1], "calls_beta", "beta_leaf")]
    {
        let callers = get_json(&app, &format!("/api/symbol/callers?id={handle}")).await;
        assert_eq!(hop_names(&callers["callers"]), [caller]);
        assert_eq!(callers["resolved_by"], "id");
        assert_eq!(callers["matched_symbols"], 1);
        let callees = get_json(&app, &format!("/api/symbol/callees?id={handle}")).await;
        assert_eq!(hop_names(&callees["callees"]), [callee]);
        assert_eq!(callees["resolved_by"], "id");
    }

    let shared = get_json(&app, "/api/symbol/callers?qname=src/lib.rs::run").await;
    assert_eq!(hop_names(&shared["callers"]), ["calls_alpha", "calls_beta"]);
    assert_eq!(shared["resolved_by"], "ref");
    assert_eq!(
        shared["matched_symbols"], 2,
        "an older client must be able to see that its selector was ambiguous"
    );
    let shared = get_json(&app, "/api/symbol/callees?qname=src/lib.rs::run").await;
    assert_eq!(hop_names(&shared["callees"]), ["alpha_leaf", "beta_leaf"]);
    assert_eq!(shared["resolved_by"], "ref");

    // The fallback also answers an unqualified name, so its count has to model that resolution
    // too: `matched_symbols: 0` beside a non-empty hop list would tell a client its selector
    // matched nothing.
    let short = get_json(&app, "/api/symbol/callers?qname=alpha_leaf").await;
    assert_eq!(hop_names(&short["callers"]), ["run"]);
    assert_eq!(short["resolved_by"], "ref");
    assert_eq!(short["matched_symbols"], 1);

    // A handle wins over a qualified name sent in the same request.
    let both =
        get_json(&app, &format!("/api/symbol/callers?id={}&qname=src/lib.rs::run", handles[0]))
            .await;
    assert_eq!(hop_names(&both["callers"]), ["calls_alpha"]);
    assert_eq!(both["resolved_by"], "id");

    // A well-formed handle naming nothing here is a 404, not an empty caller list.
    for route in ["callers", "callees"] {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/symbol/{route}?id=sym_7fffffffffffffff"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_body(response).await["error"], "unknown symbol handle");
    }
    let _ = fs::remove_dir_all(root);
}

/// Sorted, de-duplicated hop names from a `callers`/`callees` payload.
fn hop_names(hops: &Value) -> Vec<String> {
    let mut names = hops
        .as_array()
        .unwrap()
        .iter()
        .map(|hop| hop["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

/// Bind each `run` call site to the overload it actually targets, as a compiler oracle pass does.
/// The heuristic resolver leaves a same-name method call unresolved on purpose, so without this
/// the caller direction has no edge that could tell the two overloads apart.
fn resolve_overload_call_sites(conn: &rusqlite::Connection) {
    for (caller, callee) in [("calls_alpha", "alpha_leaf"), ("calls_beta", "beta_leaf")] {
        let target: i64 = conn
            .query_row(
                "SELECT run.id
                 FROM symbols run
                 JOIN files ON files.id = run.file_id
                 JOIN edges body ON body.from_symbol_id = run.id
                 JOIN symbols leaf ON leaf.id = body.to_symbol_id
                 WHERE run.name = 'run' AND leaf.name = ?1",
                [callee],
                |row| row.get(0),
            )
            .unwrap();
        let edge: i64 = conn
            .query_row(
                "SELECT edges.id
                 FROM edges
                 JOIN symbols source ON source.id = edges.from_symbol_id
                 WHERE edges.edge_kind = 'calls_name' AND edges.to_name = 'run'
                   AND source.name = ?1",
                [caller],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute("UPDATE edges_data SET to_symbol_id = ?1 WHERE id = ?2", [target, edge])
            .unwrap();
    }
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get_json(app: &axum::Router, uri: &str) -> Value {
    let response =
        app.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

fn test_config() -> (PathBuf, Config) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "rag-rat-http-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut config = Config::minimal_for_database(root.join("index.sqlite"), root.clone());
    config.database_key_pinned = true;
    config.targets = vec![ResolvedTarget {
        name: "rust".into(),
        language: Language::Rust,
        directories: vec![PathBuf::from("src")],
        include: vec!["**/*.rs".into()],
        exclude: Vec::new(),
        kind: TargetKind::Source,
    }];
    (root, config)
}
