use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rag_rat_base::config::RemoteEmbeddingConfig;
use rag_rat_base::embedding_models::{FASTEMBED_MODEL_ID, HASH_MODEL_ID, spec};

use super::{batch_write, *};

/// One-shot HTTP/1.1 stub replying to the install probe's `/api/embed` with a `dim`-wide
/// vector.
fn spawn_embed_stub(dim: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Fully drain the request (not a one-shot read): a partial drain leaves unread
            // bytes that make Windows do an abortive RST close, surfacing to
            // the client as a transport error instead of the response. See
            // `read_request_body`.
            let _ = read_request_body(&mut stream);
            let nums = vec!["0.1"; dim].join(",");
            let body = format!("{{\"data\":[{{\"embedding\":[{nums}],\"index\":0}}]}}");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

fn fastembed_dim() -> usize {
    spec(FASTEMBED_MODEL_ID).unwrap().dim
}

fn schema_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    ensure_model_manifest(&conn).unwrap();
    conn
}

fn reset_estimated_reconcile_job_calls() {
    crate::index::ai::store::ESTIMATED_RECONCILE_JOBS_CALLS.with(|calls| calls.set(0));
}

fn estimated_reconcile_job_calls() -> usize {
    crate::index::ai::store::ESTIMATED_RECONCILE_JOBS_CALLS.with(std::cell::Cell::get)
}

fn remote_at(endpoint: &str) -> RemoteEmbeddingConfig {
    RemoteEmbeddingConfig {
        model: "all-minilm".to_string(),
        backend: rag_rat_base::config::RemoteBackend::Ollama,
        endpoint: Some(endpoint.to_string()),
        cookbook: None,
        query_endpoint: None,
        auth_env: None,
        gpu: None,
        num_ctx: None,
        batch_size: 256,
        concurrency: 32,
        max_batch_chars: 384_000,
        request_timeout_s: 5,
    }
}

fn activate_remote_fastembed(conn: &Connection, remote: &RemoteEmbeddingConfig) {
    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    set_active_remote_config(conn, remote).unwrap();
    conn.execute(
        "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
         'ollama'
             WHERE model_id = ?1",
        params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
    )
    .unwrap();
    set_repo_meta(conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
    set_repo_meta(
        conn,
        ACTIVE_EMBEDDING_MODEL_VERSION_META,
        &remote_freshness_version(spec, remote),
    )
    .unwrap();
}

fn reconcile_attempt_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM reconcile_attempts", [], |row| row.get(0)).unwrap()
}

fn reconcile_meta_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM reconcile_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()
        .unwrap()
}

struct TimeoutsThenOkEmbedder {
    calls: AtomicUsize,
    dim: usize,
    failures: usize,
}

impl Embedder for TimeoutsThenOkEmbedder {
    fn model_id(&self) -> &str {
        FASTEMBED_MODEL_ID
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.failures {
            anyhow::bail!("request timed out");
        }
        Ok(vec![vec![0.1; self.dim]; texts.len()])
    }
}

struct RecordingEmbedder {
    calls: AtomicUsize,
    request_sizes: Mutex<Vec<usize>>,
    dim: usize,
}

impl Embedder for RecordingEmbedder {
    fn model_id(&self) -> &str {
        FASTEMBED_MODEL_ID
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.request_sizes.lock().unwrap().push(texts.len());
        Ok(vec![vec![0.1; self.dim]; texts.len()])
    }
}

fn read_request_body(stream: &mut TcpStream) -> String {
    // Force the accepted stream to blocking so `set_read_timeout` (SO_RCVTIMEO) governs the
    // reads: on macOS/BSD an accepted socket INHERITS a non-blocking listener's `O_NONBLOCK`
    // (Linux/Windows do not inherit), and on a non-blocking socket `read` returns
    // `WouldBlock` the instant no bytes are buffered — which the body loop below treats as
    // `Err → break`, TRUNCATING a request whose body spans multiple TCP segments (the
    // 1005-item batch). The stubs' listeners have been blocking since #697; the reset stays
    // as a guard.
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n");
        if let Some(end) = header_end {
            let headers = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
            let content_len = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = end + 4;
            while raw.len() < body_start + content_len {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            return String::from_utf8_lossy(&raw[body_start..body_start + content_len]).to_string();
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return String::new(),
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
}

fn raise_max(max_seen: &AtomicUsize, value: usize) {
    let mut current = max_seen.load(Ordering::SeqCst);
    while value > current {
        match max_seen.compare_exchange(current, value, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

fn spawn_reconcile_embed_stub(
    dim: usize,
    max_conns: usize,
    delay: Duration,
) -> (String, thread::JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::clone(&max_in_flight);
    let handle = thread::spawn(move || {
        let mut workers = Vec::new();
        // Accept EXACTLY `max_conns` connections on a blocking listener — the stub's lifetime
        // is bounded by the test's request count (an event), not by the wall clock. The
        // previous loop polled a non-blocking listener under a 5s overall / 500ms idle cap:
        // under host load the reconcile's pre-request work delayed its connect past those
        // caps, the listener dropped, and every chunk failed (#697). A loaded box can only
        // make this loop WAIT longer, never exit early. `handle.join()` in the test therefore
        // guarantees every accepted request has fully drained before the assertions run.
        for _ in 0..max_conns {
            let Ok((mut stream, _)) = listener.accept() else { break };
            let in_flight = Arc::clone(&in_flight);
            let max_seen = Arc::clone(&max_seen);
            workers.push(thread::spawn(move || {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                raise_max(&max_seen, now);
                let body = read_request_body(&mut stream);
                thread::sleep(delay);
                let inputs = body.matches("path: ").count().max(1);
                let vector = vec!["0.1"; dim].join(",");
                let rows = (0..inputs)
                    .map(|i| format!("{{\"embedding\":[{vector}],\"index\":{i}}}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let response_body = format!("{{\"data\":[{rows}]}}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for worker in workers {
            let _ = worker.join();
        }
    });
    (format!("http://127.0.0.1:{port}"), handle, max_in_flight)
}

fn spawn_selective_failure_embed_stub(
    dim: usize,
    max_conns: usize,
    fail_marker: &'static str,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let mut workers = Vec::new();
        // Same event-bounded accept as `spawn_reconcile_embed_stub` (#697): block until each
        // of the `max_conns` expected connections arrives; no wall-clock caps that a loaded
        // host can beat. The retries the scoped-retry path makes are SEQUENTIAL, so the old
        // 500ms idle cap could fire between one request and its follow-up.
        for _ in 0..max_conns {
            let Ok((mut stream, _)) = listener.accept() else { break };
            workers.push(thread::spawn(move || {
                let body = read_request_body(&mut stream);
                let response = if body.contains(fail_marker) {
                    let response_body = "{\"error\":\"transient\"}";
                    format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: \
                         application/json\r\nContent-Length: {}\r\nConnection: \
                         close\r\n\r\n{response_body}",
                        response_body.len()
                    )
                } else {
                    let inputs = body.matches("path: ").count().max(1);
                    let vector = vec!["0.1"; dim].join(",");
                    let rows = (0..inputs)
                        .map(|i| format!("{{\"embedding\":[{vector}],\"index\":{i}}}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    let response_body = format!("{{\"data\":[{rows}]}}");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                         {}\r\nConnection: close\r\n\r\n{response_body}",
                        response_body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }));
        }
        for worker in workers {
            let _ = worker.join();
        }
    });
    (format!("http://127.0.0.1:{port}"), handle)
}

fn seed_embedding_chunk(conn: &Connection, i: i64) -> i64 {
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES (?1, 'rust', 'source', ?2, 0, 0)",
        params![format!("src/file_{i}.rs"), format!("sha-{i}")],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    let text = format!(
        "pub fn item_{i}(input: usize) -> usize {{\n    let mut value = input + {i};\n    for \
         step in 0..16 {{ value = value.saturating_add(step); }}\n    value\n}}"
    );
    conn.execute(
        "INSERT INTO chunks(
                 file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line, end_line,
                 text_hash, source_revision
             )
             VALUES (?1, 'code', ?2, 0, ?3, 1, 3, ?4, ?5)",
        params![
            file_id,
            format!("crate::item_{i}"),
            i64::try_from(text.len()).unwrap(),
            format!("hash-{i}"),
            format!("rev-{i}")
        ],
    )
    .unwrap();
    let chunk_id = conn.last_insert_rowid();
    rag_rat_db::chunk_text_store::seed_chunk_text(conn, chunk_id, &text).unwrap();
    chunk_id
}

fn prepared_job(chunk_id: i64, i: i64) -> PreparedEmbeddingJob {
    let input_text = format!("path: src/file_{i}.rs\n\npub fn item_{i}() {{}}");
    PreparedEmbeddingJob {
        id: chunk_id,
        text_hash: format!("hash-{i}"),
        input_hash: format!("input-hash-{i}"),
        input_chars: input_text.chars().count(),
        input_text,
        input_truncated: false,
        policy: EmbeddingPolicy::Embed,
        priority: 0,
        reason: ReconcileReason::Missing,
    }
}

fn active_version(conn: &Connection) -> String {
    repo_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META).unwrap().unwrap()
}

#[test]
fn install_with_remote_toggles_the_row_to_ollama_and_stamps_the_remote_key() {
    // #317 rework: a remote block serves the SELECTED model over Ollama — the SAME ai_models
    // row toggles its runtime to `ollama`, and the freshness key is the remote (not
    // local) version.
    let (url, handle) = spawn_embed_stub(fastembed_dim());
    let conn = schema_conn();
    let remote = remote_at(&url);

    let model = install_model(&conn, FASTEMBED_MODEL_ID, Some(&remote)).expect("ollama installs");
    handle.join().unwrap();

    assert_eq!(model.runtime, "ollama", "the row's runtime toggled to ollama");
    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    assert_eq!(active_version(&conn), remote_freshness_version(spec, &remote));
    assert_ne!(active_version(&conn), spec.version, "remote key differs from the local version");
}

#[test]
fn remote_reconcile_accumulates_enough_work_to_fill_concurrent_embedder_window() {
    let conn = schema_conn();
    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    for i in 0..4 {
        seed_embedding_chunk(&conn, i);
    }
    let (url, handle, max_in_flight) =
        spawn_reconcile_embed_stub(spec.dim, 4, Duration::from_millis(150));
    let mut remote = remote_at(&url);
    remote.batch_size = 1;
    remote.concurrency = 4;
    set_active_remote_config(&conn, &remote).unwrap();
    conn.execute(
        "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
         'ollama'
             WHERE model_id = ?1",
        params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
    )
    .unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
    set_repo_meta(
        &conn,
        ACTIVE_EMBEDDING_MODEL_VERSION_META,
        &remote_freshness_version(spec, &remote),
    )
    .unwrap();

    let report = reconcile_with_options_progress(
        &conn,
        ReconcileOptions { batch_size: Some(1), ..ReconcileOptions::default() },
        |_| {},
    )
    .unwrap();
    handle.join().unwrap();

    assert_eq!(report.batch_size, 1, "public/report batch size is preserved");
    assert_eq!(report.embeddings_written, 4);
    assert_eq!(report.failed_chunks, 0);
    assert!(
        max_in_flight.load(Ordering::SeqCst) > 1,
        "remote reconcile should hand multiple ordered texts to one concurrent embedder call"
    );
}

#[test]
fn remote_reconcile_chunks_large_selection_below_sqlite_bind_limit() {
    let conn = schema_conn();
    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    for i in 0..1005 {
        seed_embedding_chunk(&conn, i);
    }
    let (url, handle, _) = spawn_reconcile_embed_stub(spec.dim, 1, Duration::ZERO);
    let mut remote = remote_at(&url);
    remote.batch_size = 4096;
    remote.concurrency = 32;
    set_active_remote_config(&conn, &remote).unwrap();
    conn.execute(
        "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
         'ollama'
             WHERE model_id = ?1",
        params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
    )
    .unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
    set_repo_meta(
        &conn,
        ACTIVE_EMBEDDING_MODEL_VERSION_META,
        &remote_freshness_version(spec, &remote),
    )
    .unwrap();

    let report = reconcile_with_options_progress(
        &conn,
        ReconcileOptions { batch_size: Some(64), ..ReconcileOptions::default() },
        |_| {},
    )
    .unwrap();
    handle.join().unwrap();

    assert_eq!(report.embeddings_written, 1005);
    assert_eq!(report.failed_chunks, 0);
}

#[test]
fn automatic_noop_reconcile_does_not_write_attempt() {
    let conn = schema_conn();
    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    seed_embedding_chunk(&conn, 1);
    let (url, handle, _) = spawn_reconcile_embed_stub(spec.dim, 1, Duration::ZERO);
    let remote = remote_at(&url);
    activate_remote_fastembed(&conn, &remote);

    reset_estimated_reconcile_job_calls();
    let report = reconcile_with_options_progress(
        &conn,
        ReconcileOptions {
            max_seconds: Some(30),
            provision_remote: false,
            ..ReconcileOptions::default()
        },
        std::mem::drop,
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(report.embeddings_written, 1);
    assert_eq!(
        estimated_reconcile_job_calls(),
        1,
        "non-empty automatic reconcile must reuse the no-op preflight estimate"
    );
    let attempts_after_work = reconcile_attempt_count(&conn);
    let started_meta_after_work =
        reconcile_meta_value(&conn, LAST_EMBEDDING_RECONCILE_STARTED_META)
            .expect("working reconcile writes started meta");

    reset_estimated_reconcile_job_calls();
    let report = reconcile_with_options_progress(
        &conn,
        ReconcileOptions {
            max_seconds: Some(30),
            provision_remote: false,
            ..ReconcileOptions::default()
        },
        std::mem::drop,
    )
    .unwrap();

    assert_eq!(report.status, ReconcileStatus::Current);
    assert_eq!(report.processed_chunks, 0);
    assert_eq!(report.embeddings_written, 0);
    assert_eq!(
        estimated_reconcile_job_calls(),
        1,
        "automatic no-op reconcile needs only the preflight estimate"
    );
    assert_eq!(
        reconcile_attempt_count(&conn),
        attempts_after_work,
        "automatic no-op reconcile must not append write-heavy attempt rows"
    );
    assert_eq!(
        reconcile_meta_value(&conn, LAST_EMBEDDING_RECONCILE_STARTED_META),
        Some(started_meta_after_work),
        "automatic no-op reconcile must not dirty reconcile meta"
    );
}

#[test]
fn remote_reconcile_scopes_request_failure_to_remote_request_batch() {
    let conn = schema_conn();
    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    for i in 0..4 {
        seed_embedding_chunk(&conn, i);
    }
    let (url, handle) = spawn_selective_failure_embed_stub(spec.dim, 8, "item_2");
    let mut remote = remote_at(&url);
    remote.batch_size = 1;
    remote.concurrency = 4;
    set_active_remote_config(&conn, &remote).unwrap();
    conn.execute(
        "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
         'ollama'
             WHERE model_id = ?1",
        params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
    )
    .unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
    set_repo_meta(
        &conn,
        ACTIVE_EMBEDDING_MODEL_VERSION_META,
        &remote_freshness_version(spec, &remote),
    )
    .unwrap();

    let report = reconcile_with_options_progress(
        &conn,
        ReconcileOptions { batch_size: Some(1), ..ReconcileOptions::default() },
        |_| {},
    )
    .unwrap();
    handle.join().unwrap();

    assert_eq!(report.embeddings_written, 3);
    assert_eq!(report.failed_chunks, 1);
    let failed_rows: i64 = conn
        .query_row("SELECT count(*) FROM chunk_embeddings WHERE status = 'Failed'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let current_rows: i64 = conn
        .query_row("SELECT count(*) FROM chunk_embeddings WHERE status = 'Current'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(failed_rows, 1);
    assert_eq!(current_rows, 3);
}

#[test]
fn remote_reconcile_keeps_caller_batch_size_when_time_bounded() {
    let mut remote = remote_at("http://localhost:11434");
    remote.batch_size = 256;
    remote.concurrency = 32;

    assert_eq!(batch_write::remote_reconcile_batch_size(&remote, 8, Some(1)), 8);
    assert_eq!(batch_write::remote_reconcile_batch_size(&remote, 8, None), 8192);
}

#[test]
fn remote_scoped_retry_classifies_endpoint_failures() {
    for error in ["connection refused", "failed to connect", "connect error"] {
        assert_eq!(
            batch_write::classify_remote_scoped_retry_error(error),
            batch_write::RemoteScopedRetryError::AbortImmediately,
            "{error}"
        );
    }
    for error in [
        "request timed out",
        "timeout",
        "connection reset",
        "connection closed",
        "http status 504: gateway timeout",
    ] {
        assert_eq!(
            batch_write::classify_remote_scoped_retry_error(error),
            batch_write::RemoteScopedRetryError::EndpointFailure,
            "{error}"
        );
    }
    for error in ["http status 500: transient", "embedder model returned 2 vectors for 3 texts"] {
        assert_eq!(
            batch_write::classify_remote_scoped_retry_error(error),
            batch_write::RemoteScopedRetryError::Other,
            "{error}"
        );
    }
}

#[test]
fn remote_scoped_retry_keeps_later_ranges_after_one_timeout() {
    let conn = schema_conn();
    let jobs = (0..3)
        .map(|i| {
            let chunk_id = seed_embedding_chunk(&conn, i);
            prepared_job(chunk_id, i)
        })
        .collect::<Vec<_>>();
    let mut remote = remote_at("http://localhost:11434");
    remote.batch_size = 1;
    remote.max_batch_chars = usize::MAX;
    let embedder = TimeoutsThenOkEmbedder {
        calls: AtomicUsize::new(0),
        dim: spec(FASTEMBED_MODEL_ID).unwrap().dim,
        failures: 1,
    };
    let groups = batch_write::group_embedding_jobs_by_input_hash(jobs);

    let (written, failed) = batch_write::write_remote_scoped_or_failed(
        &conn,
        &embedder,
        "test-version",
        &groups,
        Some(&remote),
        "initial failure",
    )
    .unwrap();

    assert_eq!(written, 2);
    assert_eq!(failed, 1);
    assert_eq!(
        embedder.calls.load(Ordering::SeqCst),
        3,
        "a single timeout should not skip later request ranges"
    );
    let failed_rows: i64 = conn
        .query_row("SELECT count(*) FROM chunk_embeddings WHERE status = 'Failed'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let current_rows: i64 = conn
        .query_row("SELECT count(*) FROM chunk_embeddings WHERE status = 'Current'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(failed_rows, 1);
    assert_eq!(current_rows, 2);
}

#[test]
fn remote_scoped_retry_aborts_after_repeated_endpoint_failures() {
    let conn = schema_conn();
    let jobs = (0..4)
        .map(|i| {
            let chunk_id = seed_embedding_chunk(&conn, i);
            prepared_job(chunk_id, i)
        })
        .collect::<Vec<_>>();
    let mut remote = remote_at("http://localhost:11434");
    remote.batch_size = 1;
    remote.max_batch_chars = usize::MAX;
    let embedder = TimeoutsThenOkEmbedder {
        calls: AtomicUsize::new(0),
        dim: spec(FASTEMBED_MODEL_ID).unwrap().dim,
        failures: usize::MAX,
    };
    let groups = batch_write::group_embedding_jobs_by_input_hash(jobs);

    let (written, failed) = batch_write::write_remote_scoped_or_failed(
        &conn,
        &embedder,
        "test-version",
        &groups,
        Some(&remote),
        "initial failure",
    )
    .unwrap();

    assert_eq!(written, 0);
    assert_eq!(failed, 4);
    assert_eq!(
        embedder.calls.load(Ordering::SeqCst),
        batch_write::REMOTE_SCOPED_RETRY_CONSECUTIVE_ENDPOINT_FAILURE_LIMIT,
        "repeated timeout-like failures should stop before serially retrying every range"
    );
}

#[test]
fn embed_and_write_jobs_reuses_same_window_duplicate_input_hashes() {
    let conn = schema_conn();
    let first_chunk_id = seed_embedding_chunk(&conn, 0);
    let duplicate_chunk_id = seed_embedding_chunk(&conn, 1);
    let first_job = prepared_job(first_chunk_id, 0);
    let mut duplicate_job = prepared_job(duplicate_chunk_id, 1);
    duplicate_job.input_hash = first_job.input_hash.clone();
    duplicate_job.input_text = first_job.input_text.clone();
    let embedder = RecordingEmbedder {
        calls: AtomicUsize::new(0),
        request_sizes: Mutex::new(Vec::new()),
        dim: spec(FASTEMBED_MODEL_ID).unwrap().dim,
    };
    let mut remote = remote_at("http://localhost:11434");
    remote.batch_size = 1;
    remote.concurrency = 4;

    let (written, failed) = batch_write::embed_and_write_jobs(
        &conn,
        &embedder,
        "test-version",
        vec![first_job, duplicate_job],
        Some(&remote),
    )
    .unwrap();

    assert_eq!(written, 2);
    assert_eq!(failed, 0);
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *embedder.request_sizes.lock().unwrap(),
        vec![1],
        "duplicate input_hashes in one reconcile window should issue one embed text"
    );
    let current_rows: i64 = conn
        .query_row("SELECT count(*) FROM chunk_embeddings WHERE status = 'Current'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(current_rows, 2);
}

#[test]
fn embedding_cache_reuse_survives_chunk_deletion_and_gc_respects_grace() {
    // #357: the content-addressed embedding_cache decouples the vector from chunk_id, so a
    // reindex/branch-switch that deletes a chunk does NOT lose its vector.
    let conn = schema_conn();
    let dim = spec(FASTEMBED_MODEL_ID).unwrap().dim;
    let embedder = RecordingEmbedder {
        calls: AtomicUsize::new(0),
        request_sizes: Mutex::new(Vec::new()),
        dim,
    };
    let mut remote = remote_at("http://localhost:11434");
    remote.batch_size = 8;

    // Embed a chunk → writes chunk_embeddings AND the content-addressed embedding_cache.
    let chunk_id = seed_embedding_chunk(&conn, 0);
    let job = prepared_job(chunk_id, 0);
    let input_hash = job.input_hash.clone();
    let (written, failed) =
        batch_write::embed_and_write_jobs(&conn, &embedder, "v", vec![job], Some(&remote)).unwrap();
    assert_eq!((written, failed), (1, 0));
    assert!(
        find_existing_embedding(&conn, embedder.model_id(), &input_hash, dim).unwrap().is_some(),
        "embedding_cache is populated on write"
    );

    // REINDEX: deleting the chunk cascade-deletes its chunk_embeddings row (the pre-fix
    // behavior that lost the vector). The content-addressed cache is NOT chunk-scoped.
    conn.execute("DELETE FROM chunks WHERE id = ?1", params![chunk_id]).unwrap();
    let live: i64 =
        conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |r| r.get(0)).unwrap();
    assert_eq!(live, 0, "chunk deletion cascade-deleted the embedding");
    assert!(
        find_existing_embedding(&conn, embedder.model_id(), &input_hash, dim).unwrap().is_some(),
        "reuse survives reindex: the vector is still found in the durable cache"
    );

    // GC keeps a recently-used vector even with no live chunk (fast branch switch-back)...
    assert_eq!(
        prune_embedding_cache_unreferenced(&conn).unwrap(),
        0,
        "recently-used unreferenced entry is kept within the grace"
    );
    // ...and prunes it once it is past the grace with no live chunk referencing it.
    conn.execute("UPDATE embedding_cache SET last_used_at_ms = 0", []).unwrap();
    assert_eq!(
        prune_embedding_cache_unreferenced(&conn).unwrap(),
        1,
        "stale unreferenced entry is pruned"
    );
    assert!(
        find_existing_embedding(&conn, embedder.model_id(), &input_hash, dim).unwrap().is_none(),
        "pruned entry is no longer reusable"
    );
}

#[test]
fn remote_reconcile_malformed_remote_meta_finishes_blocked_attempt() {
    let conn = schema_conn();
    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    conn.execute(
        "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
         'ollama'
             WHERE model_id = ?1",
        params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
    )
    .unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_VERSION_META, spec.version).unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META, "{not valid json").unwrap();
    assert!(
        !batch_write::automatic_reconcile_can_skip_noop(&conn, &ReconcileOptions {
            max_seconds: Some(1),
            ..ReconcileOptions::default()
        },),
        "malformed remote meta fails the automatic no-op preflight closed"
    );

    let report =
        reconcile_with_options_progress(&conn, ReconcileOptions::default(), |_| {}).unwrap();

    assert_eq!(report.status, ReconcileStatus::Blocked);
    let attempt_status: String = conn
        .query_row("SELECT status FROM reconcile_attempts ORDER BY id DESC LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(attempt_status, "Blocked");
}

#[test]
fn automatic_reconcile_continues_when_preflight_estimate_errors() {
    let conn = schema_conn();
    install_model(&conn, HASH_MODEL_ID, None).expect("hash installs");
    conn.execute("DROP TABLE chunks", []).unwrap();

    let err = reconcile_with_options_progress(
        &conn,
        ReconcileOptions { max_seconds: Some(1), ..ReconcileOptions::default() },
        std::mem::drop,
    )
    .expect_err("the later reconcile estimate should report the broken schema");
    assert!(err.to_string().contains("chunks"));
}

#[test]
fn install_rejects_an_unknown_model_id() {
    // No aliases (#317): an unrecognized selector (e.g. the old `minilm` alias) is rejected —
    // the arg must be a registered model_id (the HF path).
    let conn = schema_conn();
    let err = install_model(&conn, "minilm", None).expect_err("alias is no longer accepted");
    assert!(err.to_string().contains("unknown embedding model"), "{err}");
}

#[test]
fn install_rejects_a_remote_block_for_a_non_transformer_target() {
    // A remote block serves the model over Ollama (transformers only). `models install
    // embedding-hash` with a remote block present must be rejected BEFORE any probe — else the
    // hash row would be marked runtime='ollama' with the served model's vectors under its id.
    let conn = schema_conn();
    let remote = remote_at("http://127.0.0.1:1"); // guard fires before any connection attempt
    let err =
        install_model(&conn, HASH_MODEL_ID, Some(&remote)).expect_err("hash + remote rejected");
    assert!(err.to_string().contains("requires a transformer model"), "{err}");
}

#[test]
fn local_install_clears_a_stale_remote_config_meta() {
    // After an Ollama install persists a remote config, re-installing the model LOCALLY must
    // DELETE that meta — otherwise active_embedder keeps building an OpenAiEmbedder against the
    // dead endpoint. Uses the hash model so the local install is feature-free.
    let conn = schema_conn();
    set_active_remote_config(&conn, &remote_at("http://box:11434")).unwrap();
    assert!(active_remote_config(&conn).unwrap().is_some(), "precondition: remote meta set");
    install_model(&conn, HASH_MODEL_ID, None).unwrap();
    assert!(
        active_remote_config(&conn).unwrap().is_none(),
        "a local install must clear the stale remote-config meta",
    );
}

#[test]
fn legacy_active_ollama_model_is_cleaned_on_manifest_ensure() {
    // An index that had the pre-#317 REMOTE id `ollama-all-minilm` installed + active keeps its
    // `ai_models` row + active-model meta + remote-config meta. That id is gone from the
    // registry (Ollama is now a transport), so without legacy cleanup `active_embedder` bails
    // with "unknown active embedding model" — breaking search/reconcile.
    // `ensure_model_manifest` must drop ALL THREE (row, active meta, remote config) and
    // fall back to hash. Feature-free: the legacy row is seeded by raw SQL (no
    // fastembed/model2vec install), and the fallback is the always-available hash
    // embedder.
    const LEGACY_OLLAMA_ID: &str = "ollama-all-minilm";
    let conn = schema_conn();

    // Mirror a real pre-#317 remote install's DB state: a Ready, active `ai_models` row for the
    // removed id, the active-model meta, and a persisted (secret-free) remote config.
    conn.execute(
        "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, disabled, \
         status, installed_at_ms) VALUES (?1, 'embedding', 384, 'ollama', 1, 0, 'Ready', 1)",
        params![LEGACY_OLLAMA_ID],
    )
    .unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, LEGACY_OLLAMA_ID).unwrap();
    set_active_remote_config(&conn, &remote_at("http://box:11434")).unwrap();
    assert!(!model_manifest_is_current(&conn).unwrap(), "a lingering legacy active id is work");

    ensure_model_manifest(&conn).unwrap();

    // The row, the active-model meta, and the remote config are all gone.
    let row_present: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM ai_models WHERE model_id = ?1)",
            params![LEGACY_OLLAMA_ID],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!row_present, "the legacy ai_models row is removed");
    assert_eq!(repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META).unwrap(), None, "active meta cleared");
    assert!(
        active_remote_config(&conn).unwrap().is_none(),
        "the stale remote config is cleared so no OpenAiEmbedder is reconstructed",
    );

    // With no active model + no remote config, the active model falls back to hash. Mark the
    // (always-feature-free) hash row Ready as a normal index would, and assert
    // `active_embedder` resolves it WITHOUT the "unknown active embedding model" error
    // the stale legacy row caused.
    install_model(&conn, HASH_MODEL_ID, None).expect("hash installs");
    let embedder = active_embedder(&conn, None).expect("falls back to hash, no unknown-model err");
    assert_eq!(embedder.model_id(), HASH_MODEL_ID, "active embedder falls back to hash");
}

#[test]
fn legacy_active_model_cleanup_clears_the_stale_version_meta() {
    // R3a: a pre-#317 legacy-active model (id removed from the registry) had its freshness
    // version meta stamped. `remove_legacy_models` must clear
    // `ACTIVE_EMBEDDING_MODEL_VERSION_META` too when the legacy id was active — else
    // `active_embedding_model_version(HASH)` would inherit the legacy key (it reads the
    // meta for the active model) and bake new hash embeddings under the wrong
    // `model_version`.
    const LEGACY_OLLAMA_ID: &str = "ollama-all-minilm";
    let conn = schema_conn();
    conn.execute(
        "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, disabled, \
         status, installed_at_ms) VALUES (?1, 'embedding', 384, 'ollama', 1, 0, 'Ready', 1)",
        params![LEGACY_OLLAMA_ID],
    )
    .unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, LEGACY_OLLAMA_ID).unwrap();
    set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_VERSION_META, "ollama-all-minilm-v1-deadbeef")
        .unwrap();

    ensure_model_manifest(&conn).unwrap();

    // The stale version meta is gone. With the active model now the hash fallback,
    // `active_embedding_model_version(HASH)` falls back to the hash spec's static version — NOT
    // the legacy key.
    assert_eq!(
        repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_VERSION_META).unwrap(),
        None,
        "the legacy freshness-version meta is cleared",
    );
    assert_eq!(
        active_embedding_model_version(&conn, HASH_MODEL_ID).unwrap(),
        spec(HASH_MODEL_ID).unwrap().version,
        "hash fallback gets its OWN version, not the stale legacy key",
    );
}

#[test]
fn activate_model_with_version_writes_all_metas() {
    // R3b centralization: the helper every activation site goes through stamps the active
    // model, its version, AND its provenance — so no site can activate without any of
    // them (the recovery bug for the version; the #394 masquerade bug for provenance).
    let conn = schema_conn();
    activate_model_with_version(&conn, HASH_MODEL_ID, "hash-v1", ActiveModelProvenance::Confirmed)
        .unwrap();
    assert_eq!(
        repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META).unwrap().as_deref(),
        Some(HASH_MODEL_ID)
    );
    assert_eq!(active_version(&conn), "hash-v1");
    assert!(!active_embedding_model_is_provisional(&conn).unwrap(), "Confirmed ⇒ non-provisional");
}

// Needs a real fastembed install (the no-default-features CI build bails without the feature);
// HF-path id resolution is also exercised by the rejection + registry tests, which run
// everywhere.
#[cfg(feature = "fastembed")]
#[test]
fn install_activates_a_model_by_its_hf_path_id() {
    let conn = schema_conn();
    let model = install_model(&conn, FASTEMBED_MODEL_ID, None).expect("hf-path id installs");
    assert_eq!(model.model_id, FASTEMBED_MODEL_ID);
}

// The LOCAL re-install is a real fastembed install — gated for the no-default-features build.
// (`remote_freshness_version` itself + the endpoint-independence are unit-tested feature-free.)
#[cfg(feature = "fastembed")]
#[test]
fn flipping_remote_to_local_resets_the_freshness_version_to_the_static_version() {
    // Install the model over Ollama (remote key), then re-install it LOCALLY (no remote). The
    // active freshness key must flip to the static `spec.version` — a local↔remote flip is a
    // re-embed, and the meta is the single source the reconcile/search path reads.
    let (url, handle) = spawn_embed_stub(fastembed_dim());
    let conn = schema_conn();
    let remote = remote_at(&url);

    install_model(&conn, FASTEMBED_MODEL_ID, Some(&remote)).expect("ollama installs");
    handle.join().unwrap();
    let remote_version = active_version(&conn);

    // Re-install the SAME model locally (no remote).
    install_model(&conn, FASTEMBED_MODEL_ID, None).expect("local re-install");

    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    assert_eq!(active_version(&conn), spec.version, "flip to local resets to static version");
    assert_ne!(active_version(&conn), remote_version, "must NOT keep the stale remote key");
}

#[test]
fn the_remote_freshness_key_is_endpoint_independent_across_installs() {
    // Two installs at DIFFERENT endpoints (same server model) must stamp the SAME freshness key
    // — an ephemeral box's per-run URL must not re-embed the whole repo.
    let conn = schema_conn();
    let (url_a, h_a) = spawn_embed_stub(fastembed_dim());
    install_model(&conn, FASTEMBED_MODEL_ID, Some(&remote_at(&url_a))).unwrap();
    h_a.join().unwrap();
    let version_a = active_version(&conn);

    let (url_b, h_b) = spawn_embed_stub(fastembed_dim());
    install_model(&conn, FASTEMBED_MODEL_ID, Some(&remote_at(&url_b))).unwrap();
    h_b.join().unwrap();
    let version_b = active_version(&conn);

    assert_eq!(version_a, version_b, "different endpoints → same freshness key (no re-embed)");
}

#[test]
fn installing_a_local_model_stamps_its_static_version() {
    let conn = schema_conn();
    install_model(&conn, HASH_MODEL_ID, None).expect("hash installs");
    assert_eq!(active_version(&conn), spec(HASH_MODEL_ID).unwrap().version);
}

/// Activate an ephemeral remote config WITHOUT provisioning (mark the model Ready + persist a
/// cookbook remote config + freshness meta), mirroring a real ephemeral install's DB state.
fn activate_ephemeral(conn: &Connection) {
    activate_ephemeral_with_query_endpoint(conn, Some("http://localhost:11434"));
}

/// Activate an ephemeral (`cookbook`) remote config with the given local `query_endpoint`.
/// `None` models the "no local query server" case whose light/watcher pass defers
/// (`SkipEphemeral`); `Some(..)` models the local light-embed path.
fn activate_ephemeral_with_query_endpoint(conn: &Connection, query_endpoint: Option<&str>) {
    let spec = spec(FASTEMBED_MODEL_ID).unwrap();
    conn.execute(
        "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
         'ollama'
             WHERE model_id = ?1",
        params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
    )
    .unwrap();
    set_repo_meta(conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
    let remote = RemoteEmbeddingConfig {
        model: "all-minilm".to_string(),
        backend: rag_rat_base::config::RemoteBackend::Ollama,
        endpoint: None,
        cookbook: Some("@rag-rat/cookbook/modal".to_string()),
        query_endpoint: query_endpoint.map(str::to_string),
        auth_env: None,
        gpu: None,
        num_ctx: None,
        batch_size: 256,
        concurrency: 32,
        max_batch_chars: 384_000,
        request_timeout_s: 5,
    };
    set_active_remote_config(conn, &remote).unwrap();
    set_repo_meta(
        conn,
        ACTIVE_EMBEDDING_MODEL_VERSION_META,
        &remote_freshness_version(spec, &remote),
    )
    .unwrap();
}

#[test]
fn reconcile_skips_ephemeral_chunk_embed_without_provision_remote() {
    // The watcher/maintenance pass (`provision_remote: false`) on an ephemeral model with NO
    // local `query_endpoint` server must NOT cold-start a cookbook box — it returns Blocked
    // with a "needs explicit reconcile" message and never spawns a subprocess. (No
    // cookbook is actually runnable here, so a provisioning attempt would error/hang;
    // the skip is what keeps this test fast + offline.) With a `query_endpoint` set,
    // the light path embeds locally instead — see
    // `light_pass_with_query_endpoint_embeds_locally_without_provisioning`.
    let conn = schema_conn();
    activate_ephemeral_with_query_endpoint(&conn, None);

    let report = reconcile_with_options_progress(
        &conn,
        ReconcileOptions { provision_remote: false, ..ReconcileOptions::default() },
        |_| {},
    )
    .expect("reconcile returns a report (skips, does not error)");

    assert_eq!(report.status, ReconcileStatus::Blocked);
    assert_eq!(report.embeddings_written, 0);
    assert!(
        report.message.as_deref().unwrap_or_default().contains("explicit `rag-rat reconcile`"),
        "skip message: {:?}",
        report.message
    );
}

/// Build the light-path acquire for an ephemeral active model (mirrors the scan
/// `reconcile_with_options_progress` builds), on a non-provisioning (watcher) pass.
fn ephemeral_light_acquire(conn: &Connection) -> ChunkEmbedder {
    let model_id = active_embedding_model_id(conn).unwrap();
    let dim = spec(&model_id).unwrap().dim;
    let version = active_version(conn);
    let scan = EmbeddingScan {
        model_id: &model_id,
        model_version: &version,
        dim,
        max_embedding_chars: 4000,
        stamped_policy: false,
    };
    acquire_chunk_embedder(conn, None, &scan, &ReconcileOptions {
        provision_remote: false,
        ..ReconcileOptions::default()
    })
}

#[test]
fn light_pass_without_query_endpoint_defers_to_explicit_reconcile() {
    // Ephemeral watcher pass with NO local query server → SkipEphemeral (defer), even with
    // pending work: the "watcher never cold-starts a paid box" guarantee is preserved.
    let conn = schema_conn();
    activate_ephemeral_with_query_endpoint(&conn, None);
    seed_embedding_chunk(&conn, 1);
    assert!(matches!(ephemeral_light_acquire(&conn), ChunkEmbedder::SkipEphemeral));
}

#[test]
fn light_pass_with_reachable_query_endpoint_embeds_locally_single_flight() {
    // Ephemeral watcher pass, `query_endpoint` answers a probe embed → Ready with NO
    // provisioned box: the light path builds the LOCAL query-endpoint embedder (same
    // vector space as the cookbook box, no cold-start), the probe embed succeeds, and
    // concurrency is clamped to SINGLE-FLIGHT so a background edit can't overload the
    // local server.
    let conn = schema_conn();
    let (endpoint, _stub) = spawn_embed_stub(fastembed_dim());
    activate_ephemeral_with_query_endpoint(&conn, Some(&endpoint));
    seed_embedding_chunk(&conn, 1);
    match ephemeral_light_acquire(&conn) {
        ChunkEmbedder::Ready { provisioned: None, remote: Some(r), .. } => {
            assert_eq!(r.concurrency, 1, "light path must be single-flight");
            assert_eq!(
                r.endpoint.as_deref(),
                Some(endpoint.as_str()),
                "embeds against the query_endpoint"
            );
        },
        _ => panic!("expected a local Ready with provisioned=None"),
    }
}

#[test]
fn light_pass_with_unreachable_query_endpoint_defers() {
    // Ephemeral watcher pass, `query_endpoint` set to port zero (which no server listens on) →
    // the probe embed's connect is refused, so defer (SkipEphemeral), NOT embed-and-fail into
    // `Failed` chunk_embeddings, and without paying an O(repo) candidate scan first.
    let conn = schema_conn();
    activate_ephemeral_with_query_endpoint(&conn, Some("http://127.0.0.1:0"));
    seed_embedding_chunk(&conn, 1);
    assert!(matches!(ephemeral_light_acquire(&conn), ChunkEmbedder::SkipEphemeral));
}

#[test]
fn automatic_noop_scan_is_not_used_for_ephemeral_query_endpoint() {
    let conn = schema_conn();
    activate_ephemeral_with_query_endpoint(&conn, Some("http://127.0.0.1:9"));

    assert!(
        !batch_write::automatic_reconcile_can_skip_noop(&conn, &ReconcileOptions {
            max_seconds: Some(1),
            provision_remote: false,
            ..ReconcileOptions::default()
        }),
        "ephemeral light passes must probe query_endpoint before any O(repo) no-op scan"
    );
}

#[test]
fn light_pass_with_wrong_model_on_the_endpoint_defers() {
    // The port ACCEPTS connections but the embeddings route returns the WRONG dim (a different
    // service, or the configured model not pulled) — a bare TCP connect would pass, but the
    // probe embed catches the dim mismatch and defers (SkipEphemeral) instead of persisting
    // `Failed` chunk rows.
    let conn = schema_conn();
    let (endpoint, _stub) = spawn_embed_stub(fastembed_dim() + 1); // wrong dim on the route
    activate_ephemeral_with_query_endpoint(&conn, Some(&endpoint));
    seed_embedding_chunk(&conn, 1);
    assert!(matches!(ephemeral_light_acquire(&conn), ChunkEmbedder::SkipEphemeral));
}

#[test]
fn reconcile_does_not_provision_when_an_ephemeral_model_is_already_current() {
    // #330-6: an explicit `rag-rat reconcile` (`provision_remote: true`) on an ephemeral active
    // model that has NOTHING pending must NOT cold-start (and immediately tear down) a paid GPU
    // box. The repo here has ZERO chunks, so there are zero candidates. The cookbook spec
    // (`@rag-rat/cookbook/modal`) is NOT runnable in the test env, so IF provisioning were
    // attempted it would fail → a "Blocked" / error report. A clean "Current" report with no
    // embeddings is the proof that `acquire_chunk_embedder` short-circuited to
    // `NoEphemeralWork` BEFORE provisioning. (Contrast the `provision_remote: false`
    // skip test above, which returns "Blocked".)
    let conn = schema_conn();
    activate_ephemeral(&conn);

    let report = reconcile_with_options_progress(
        &conn,
        ReconcileOptions { provision_remote: true, ..ReconcileOptions::default() },
        |_| {},
    )
    .expect("reconcile returns a report (no provision attempt, no error)");

    assert_eq!(
        report.status,
        ReconcileStatus::Current,
        "no pending work → Current, not Blocked: {report:?}"
    );
    assert_eq!(report.embeddings_written, 0);
    assert_eq!(report.processed_chunks, 0);
    assert!(
        report.message.is_none(),
        "no-op reconcile carries no failure message: {:?}",
        report.message
    );
}
