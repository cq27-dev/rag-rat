//! Shared HTTP router for editor-facing read APIs.

use std::convert::Infallible;
use std::future::{Future, IntoFuture};
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Query, Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    AUTHORIZATION, ORIGIN, VARY,
};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rag_rat_base::config::Config;
use rag_rat_core::IndexDatabase;
use rag_rat_core::index::LensCloneGraphCache;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_WORKERS: usize = 2;
const DEFAULT_THETA: f64 = 0.7;
const DEFAULT_MIN_TOKENS: i64 = 100;
const DEFAULT_HOP_LIMIT: u32 = 50;
const MAX_HOP_LIMIT: u32 = 500;

#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub timeout: Duration,
    pub blocking_workers: usize,
    pub workspace_root: Option<std::path::PathBuf>,
    pub indexed_root: String,
    pub case_insensitive_paths: bool,
    pub auth_token: Option<String>,
    pub allowed_origins: Vec<String>,
    pub control: ServeControl,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            blocking_workers: DEFAULT_WORKERS,
            workspace_root: None,
            indexed_root: String::new(),
            case_insensitive_paths: false,
            auth_token: None,
            allowed_origins: Vec::new(),
            control: ServeControl::default(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ServeControl {
    stopping: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl ServeControl {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub async fn stopped(&self) {
        loop {
            let notified = self.notify.notified();
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct HttpState {
    config: Config,
    options: ServeOptions,
    blocking_workers: Arc<Semaphore>,
    clone_graph_cache: Arc<LensCloneGraphCache>,
    // Bounded global freshness cache: without it every SSE client pays 16 COUNT/MAX scans every
    // 2s through a fresh read-only connection. One entry per router collapses that to ~1
    // version computation per second regardless of client count.
    version_cache: Arc<tokio::sync::Mutex<Option<(Instant, rag_rat_core::index::LensVersion)>>>,
    treemap_cache: Arc<tokio::sync::Mutex<Option<(Instant, rag_rat_core::index::LensTreemap)>>>,
    // Admission control for the repository-wide clone graph, held OUTSIDE the worker pool. The
    // graph is single-flighted by a cache shared across requests, but a waiter that blocks for it
    // on a database worker holds a permit while doing nothing: two cold clone reads — two visible
    // editors in a linked worktree is enough — would occupy both default permits, one building and
    // one waiting, and no independent graph, memory, coupling, or papertrail read could start
    // until the build finished. Serializing entry here means the waiter holds no permit and no
    // connection, so at most one worker is ever inside a clone-graph build.
    clone_graph_gate: Arc<tokio::sync::Mutex<()>>,
}

/// Build the editor API router. The state deliberately owns no database handle: every request
/// opens and drops its read-only connection on a bounded blocking worker.
pub fn router(config: Config, options: ServeOptions) -> Router {
    let workers = options.blocking_workers.max(1);
    let state = HttpState {
        config,
        options,
        blocking_workers: Arc::new(Semaphore::new(workers)),
        clone_graph_cache: Arc::default(),
        version_cache: Arc::default(),
        treemap_cache: Arc::default(),
        clone_graph_gate: Arc::default(),
    };
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/version", get(version))
        .route("/api/events", get(events))
        .route("/api/treemap", get(treemap))
        .route("/api/file/symbols", get(file_symbols))
        .route("/api/file/graph", get(file_graph))
        .route("/api/file/coupling", get(file_coupling))
        .route("/api/file/memories", get(file_memories))
        .route("/api/file/papertrail", get(file_papertrail))
        .route("/api/file/clones", get(file_clones))
        .route("/api/symbol/callers", get(symbol_callers))
        .route("/api/symbol/callees", get(symbol_callees))
        .route("/api/chunk/text", get(chunk_text))
        .layer(middleware::from_fn_with_state(state.clone(), authorize_and_apply_cors))
        .with_state(state)
}

async fn authorize_and_apply_cors(
    State(state): State<HttpState>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request.headers().get(ORIGIN).cloned();
    let allowed_origin = origin.as_ref().filter(|origin| {
        origin.to_str().ok().is_some_and(|value| {
            state.options.allowed_origins.iter().any(|allowed| allowed == value)
        })
    });
    if origin.is_some() && allowed_origin.is_none() {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse { error: "origin is not allowed".into() }),
        )
            .into_response();
    }
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        apply_cors_headers(response.headers_mut(), allowed_origin);
        if allowed_origin.is_some() {
            // Chrome's Private Network Access blocks public-page → loopback requests unless the
            // preflight opts in. Safe to emit here: `allowed_origin` is exact-allowlisted, and
            // every route still requires the bearer token.
            response.headers_mut().insert(
                axum::http::HeaderName::from_static("access-control-allow-private-network"),
                HeaderValue::from_static("true"),
            );
        }
        return response;
    }
    if let Some(expected) = state.options.auth_token.as_deref() {
        let supplied = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if !supplied.is_some_and(|value| tokens_equal(value, expected)) {
            // A disallowed-origin 403 above stays header-free; an ALLOWED origin must still see
            // CORS headers on its 401 or the browser reports an opaque network error and the
            // client cannot distinguish "re-prompt for token" from "server down".
            let mut response =
                (StatusCode::UNAUTHORIZED, Json(ErrorResponse { error: "unauthorized".into() }))
                    .into_response();
            apply_cors_headers(response.headers_mut(), allowed_origin);
            return response;
        }
    }
    let mut response = next.run(request).await;
    apply_cors_headers(response.headers_mut(), allowed_origin);
    response
}

fn apply_cors_headers(headers: &mut axum::http::HeaderMap, origin: Option<&HeaderValue>) {
    let Some(origin) = origin else { return };
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    headers.insert(ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, OPTIONS"));
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(VARY, HeaderValue::from_static("Origin"));
}

fn tokens_equal(actual: &str, expected: &str) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    actual.bytes().zip(expected.bytes()).fold(0_u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// Serve a pre-built shared router on an already-bound listener.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    options: ServeOptions,
    shutdown: impl Future<Output = std::io::Result<()>> + Send + 'static,
) -> std::io::Result<()> {
    if !listener.local_addr()?.ip().is_loopback()
        && options.auth_token.as_deref().is_none_or(str::is_empty)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "non-loopback HTTP serving requires bearer authentication",
        ));
    }
    let control = options.control.clone();
    let app = router(config, options);
    let (trigger, graceful) = tokio::sync::oneshot::channel();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = graceful.await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        result = shutdown => {
            result?;
            control.stop();
            let _ = trigger.send(());
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "HTTP shutdown timed out"))?
        },
    }
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn status(
    State(state): State<HttpState>,
) -> Result<Json<rag_rat_core::index::LensStatus>, ApiError> {
    let indexed_root = state.options.indexed_root.clone();
    let case_insensitive_paths = state.options.case_insensitive_paths;
    run_db(state, move |db, config, _| {
        Ok(db.lens_status(config, indexed_root, case_insensitive_paths)?)
    })
    .await
    .map(Json)
}

const VERSION_CACHE_TTL: Duration = Duration::from_secs(1);

async fn cached_lens_version(
    mut state: HttpState,
) -> Result<rag_rat_core::index::LensVersion, ApiError> {
    // Keep the async mutex through the bounded worker call so aligned SSE pollers share one fill,
    // and charge the wait for it against the request's own deadline exactly as the treemap does.
    // A fill whose `run_db` sits behind two occupied workers can hold this lock for the full
    // permit timeout; without the charge every queued `/api/version` and opening `/api/events`
    // would then start a FRESH full-length attempt, so latency grows by one timeout per waiting
    // client instead of staying bounded by `ServeOptions::timeout`.
    let version_cache = state.version_cache.clone();
    let (mut cache, remaining) =
        acquire_within_budget(&version_cache, state.options.timeout).await?;
    if let Some((computed_at, version)) = cache.as_ref()
        && computed_at.elapsed() < VERSION_CACHE_TTL
    {
        return Ok(version.clone());
    }
    state.options.timeout = remaining;
    let version = run_db(state, |db, _, _| Ok(db.lens_version()?)).await?;
    *cache = Some((Instant::now(), version.clone()));
    Ok(version)
}

async fn version(
    State(state): State<HttpState>,
) -> Result<Json<rag_rat_core::index::LensVersion>, ApiError> {
    cached_lens_version(state).await.map(Json)
}

async fn events(
    State(state): State<HttpState>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let initial = cached_lens_version(state.clone()).await?;
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let stream = futures_util::stream::unfold(
        (state, Some(initial), true, interval),
        |(state, mut last, emit_initial, mut interval)| async move {
            loop {
                if state.options.control.is_stopping() {
                    return None;
                }
                let version = if emit_initial {
                    last.clone().expect("initial version is present")
                } else {
                    tokio::select! {
                        () = state.options.control.stopped() => return None,
                        _ = interval.tick() => {},
                    }
                    let Ok(version) = cached_lens_version(state.clone()).await else {
                        return None;
                    };
                    if last.as_ref() == Some(&version) {
                        continue;
                    }
                    version
                };
                let Ok(event) = Event::default().event("version").json_data(&version) else {
                    continue;
                };
                last = Some(version);
                return Some((Ok::<_, Infallible>(event), (state, last, false, interval)));
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(16)).text("hb")))
}

/// Wait for `gate`, charging the wait against `budget` and returning what survived it.
///
/// Every wait a handler adds before `run_db` has to come out of the same deadline the caller is
/// holding open, exactly as `run_db` charges its own permit wait against it. Otherwise a queue
/// stacks one full timeout on the next and answers long after the caller stopped listening.
async fn acquire_within_budget<T>(
    gate: &tokio::sync::Mutex<T>,
    budget: Duration,
) -> Result<(tokio::sync::MutexGuard<'_, T>, Duration), ApiError> {
    let waiting = Instant::now();
    let Ok(guard) = tokio::time::timeout(budget, gate.lock()).await else {
        return Err(ApiError::Timeout);
    };
    let Some(remaining) = budget.checked_sub(waiting.elapsed()) else {
        return Err(ApiError::Timeout);
    };
    Ok((guard, remaining))
}

async fn treemap(
    State(mut state): State<HttpState>,
) -> Result<Json<rag_rat_core::index::LensTreemap>, ApiError> {
    // Keep the async mutex through the bounded worker call so concurrent callers share ONE fill,
    // like the version cache above. The treemap rescans the live clone graph — and derives overlay
    // metrics on a linked worktree — so a stampede after the TTL expires would occupy the database
    // workers and stall the interactive file lanes behind it.
    let treemap_cache = state.treemap_cache.clone();
    let (mut cache, budget) = acquire_within_budget(&treemap_cache, state.options.timeout).await?;
    if let Some((computed_at, treemap)) = cache.as_ref()
        && computed_at.elapsed() < VERSION_CACHE_TTL
    {
        return Ok(Json(treemap.clone()));
    }
    // A cold treemap builds the same repository-wide clone graph `/api/file/clones` does, so it
    // queues on the same gate. Lock order is cache then gate, and nothing takes them the other way
    // round.
    let clone_graph_gate = state.clone_graph_gate.clone();
    let (_clone_graph, remaining) = acquire_within_budget(&clone_graph_gate, budget).await?;
    state.options.timeout = remaining;
    let treemap =
        run_db(state, |db, _, cancelled| Ok(db.lens_treemap_with_cancel(cancelled)?)).await?;
    *cache = Some((Instant::now(), treemap.clone()));
    Ok(Json(treemap))
}

async fn file_clones(
    State(mut state): State<HttpState>,
    Query(query): Query<FileClonesQuery>,
) -> Result<Json<rag_rat_core::index::LensFileClones>, ApiError> {
    let path = query.path.ok_or_else(|| ApiError::bad_query("missing query parameter `path`"))?;
    validate_relative_path(&path)?;
    let theta = query.theta.unwrap_or(DEFAULT_THETA);
    if !theta.is_finite() || !(DEFAULT_THETA..=1.0).contains(&theta) {
        return Err(ApiError::bad_query("`theta` must be finite and in [0.7, 1.0]"));
    }
    let min_tokens = query.min_tokens.unwrap_or(DEFAULT_MIN_TOKENS);
    if min_tokens < 0 {
        return Err(ApiError::bad_query("`min_tokens` must be non-negative"));
    }
    let case_insensitive = state.options.case_insensitive_paths;
    // Queue for the clone graph BEFORE taking a database worker. A cache hit costs one vector
    // scan, so serializing the visible editors' clone lanes here is cheap; letting them queue on
    // the worker pool instead is what starves every other lane during a cold build.
    let clone_graph_gate = state.clone_graph_gate.clone();
    let (_clone_graph, remaining) =
        acquire_within_budget(&clone_graph_gate, state.options.timeout).await?;
    state.options.timeout = remaining;
    run_db(state, move |db, _, cancelled| {
        let path = canonical_file_path(db, path, case_insensitive)?;
        Ok(db.lens_file_clones_with_cancel(&path, theta, min_tokens, cancelled)?)
    })
    .await
    .map(Json)
}

async fn file_symbols(
    State(state): State<HttpState>,
    Query(query): Query<FileQuery>,
) -> Result<Json<rag_rat_core::index::LensFileSymbols>, ApiError> {
    let path = required_path(query.path)?;
    let case_insensitive = state.options.case_insensitive_paths;
    run_db(state, move |db, _, _| {
        let path = canonical_file_path(db, path, case_insensitive)?;
        Ok(db.lens_file_symbols(&path)?)
    })
    .await
    .map(Json)
}

async fn file_graph(
    State(state): State<HttpState>,
    Query(query): Query<FileQuery>,
) -> Result<Json<rag_rat_core::index::LensFileGraph>, ApiError> {
    let path = required_path(query.path)?;
    let case_insensitive = state.options.case_insensitive_paths;
    run_db(state, move |db, _, _| {
        let path = canonical_file_path(db, path, case_insensitive)?;
        Ok(db.lens_file_graph(&path)?)
    })
    .await
    .map(Json)
}

async fn file_coupling(
    State(state): State<HttpState>,
    Query(query): Query<FileQuery>,
) -> Result<Json<rag_rat_core::index::LensFileCoupling>, ApiError> {
    let path = required_path(query.path)?;
    let case_insensitive = state.options.case_insensitive_paths;
    run_db(state, move |db, _, _| {
        let path = canonical_file_path(db, path, case_insensitive)?;
        Ok(db.lens_file_coupling(&path)?)
    })
    .await
    .map(Json)
}

async fn file_memories(
    State(state): State<HttpState>,
    Query(query): Query<FileQuery>,
) -> Result<Json<rag_rat_core::index::LensFileMemories>, ApiError> {
    let path = required_path(query.path)?;
    let case_insensitive = state.options.case_insensitive_paths;
    run_db(state, move |db, _, _| {
        let path = canonical_file_path(db, path, case_insensitive)?;
        Ok(db.lens_file_memories(&path)?)
    })
    .await
    .map(Json)
}

async fn file_papertrail(
    State(state): State<HttpState>,
    Query(query): Query<FileQuery>,
) -> Result<Json<rag_rat_core::index::LensFilePapertrail>, ApiError> {
    let path = required_path(query.path)?;
    let case_insensitive = state.options.case_insensitive_paths;
    run_db(state, move |db, _, _| {
        let path = canonical_file_path(db, path, case_insensitive)?;
        Ok(db.lens_file_papertrail(&path)?)
    })
    .await
    .map(Json)
}

async fn symbol_callers(
    State(state): State<HttpState>,
    Query(query): Query<SymbolHopQuery>,
) -> Result<Json<rag_rat_core::index::LensCallers>, ApiError> {
    let selector = hop_selector(&query)?;
    let limit = hop_limit(query.limit.as_deref());
    run_db(state, move |db, _, _| Ok(db.lens_symbol_callers(&selector, limit)?))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(UNKNOWN_SYMBOL_HANDLE.into()))
}

async fn symbol_callees(
    State(state): State<HttpState>,
    Query(query): Query<SymbolHopQuery>,
) -> Result<Json<rag_rat_core::index::LensCallees>, ApiError> {
    let selector = hop_selector(&query)?;
    let limit = hop_limit(query.limit.as_deref());
    run_db(state, move |db, _, _| Ok(db.lens_symbol_callees(&selector, limit)?))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(UNKNOWN_SYMBOL_HANDLE.into()))
}

async fn chunk_text(
    State(state): State<HttpState>,
    Query(query): Query<ChunkTextQuery>,
) -> Result<Json<rag_rat_core::index::LensChunkText>, ApiError> {
    let raw =
        query.chunk_id.ok_or_else(|| ApiError::bad_query("missing query parameter `chunk_id`"))?;
    let chunk_id = raw
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| ApiError::bad_query("`chunk_id` must be a positive integer"))?;
    run_db(state, move |db, _, _| Ok(db.lens_chunk_text(chunk_id)?))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("unknown chunk_id".into()))
}

fn required_path(path: Option<String>) -> Result<String, ApiError> {
    let path = path.ok_or_else(|| ApiError::bad_query("missing query parameter `path`"))?;
    validate_relative_path(&path)?;
    Ok(path)
}

fn canonical_file_path(
    db: &IndexDatabase,
    path: String,
    case_insensitive: bool,
) -> anyhow::Result<String> {
    Ok(db.lens_canonical_file_path(&path, case_insensitive)?.unwrap_or(path))
}

const UNKNOWN_SYMBOL_HANDLE: &str = "unknown symbol handle";

/// Pick the selector a hop request is answered by, preferring the stable `sym_<hex>` handle.
///
/// `qname` stays accepted because the server and the editor extension version independently: an
/// installed extension built before the handle existed sends only a name, and answering it with a
/// 400 would break call navigation on every such install. It is a documented fallback rather than
/// an equal alternative — a qualified name is shared by every overload in a file, so the answer is
/// their union, which the response says out loud via `resolved_by` / `matched_symbols`.
///
/// A request that CARRIES `id` is answered by the handle lane or not at all — an empty or
/// malformed handle is a 400, never a fall-through to `qname`. The two lanes answer different
/// questions, so degrading between them silently is the failure this route exists to remove: a
/// client that meant one overload would get the union of every symbol sharing its name back,
/// under a `resolved_by` it never asked for and has no reason to re-read.
fn hop_selector(query: &SymbolHopQuery) -> Result<rag_rat_core::index::LensHopSelector, ApiError> {
    if let Some(id) = query.id.as_deref() {
        return rag_rat_base::serde_big_id::parse_sym_handle(id)
            .map(rag_rat_core::index::LensHopSelector::Handle)
            .ok_or_else(|| ApiError::bad_query("`id` must be a `sym_<hex>` symbol handle"));
    }
    let qname = query.qname.as_deref().filter(|value| !value.is_empty()).ok_or_else(|| {
        ApiError::bad_query("missing query parameter `id` (preferred) or `qname`")
    })?;
    Ok(rag_rat_core::index::LensHopSelector::QualifiedName(qname.to_string()))
}

fn hop_limit(raw: Option<&str>) -> u32 {
    raw.and_then(|value| value.parse::<i64>().ok())
        .map(|value| value.clamp(i64::from(1), i64::from(MAX_HOP_LIMIT)) as u32)
        .unwrap_or(DEFAULT_HOP_LIMIT)
}

fn validate_relative_path(raw: &str) -> Result<(), ApiError> {
    if raw.is_empty() {
        return Err(ApiError::bad_query("`path` must not be empty"));
    }
    let path = Path::new(raw);
    if path.components().any(|component| {
        matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    }) {
        return Err(ApiError::bad_query("`path` must be a repository-relative path"));
    }
    Ok(())
}

/// Stops the blocking database task when a request ends WITHOUT reaching its own timeout — the
/// client disconnected, or the whole request future was dropped. Cancellation is two-sided: the
/// cooperative flag ends loops between statements, and the SQLite interrupt ends a single long
/// statement. The interrupt handle is published by the task once it has opened the connection, so
/// a cancellation that arrives first waits for it on a detached task rather than being lost.
struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    interrupt: Option<tokio::sync::oneshot::Receiver<rag_rat_core::index::DatabaseInterruptHandle>>,
}

impl CancelOnDrop {
    fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let Some(mut interrupt) = self.interrupt.take() else { return };
        match interrupt.try_recv() {
            Ok(handle) => handle.interrupt(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                // Dropping a request future happens on the runtime, but never assume it: a spawn
                // outside one panics, and a lost interrupt is better than a panicking drop.
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        if let Ok(handle) = interrupt.await {
                            handle.interrupt();
                        }
                    });
                }
            },
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {},
        }
    }

    /// The task finished on its own; there is nothing left to cancel.
    fn disarm(&mut self) {
        self.interrupt = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.interrupt.is_some() {
            self.cancel();
        }
    }
}

async fn run_db<T: Send + 'static>(
    state: HttpState,
    operation: impl FnOnce(&IndexDatabase, &Config, &AtomicBool) -> Result<T, DbError> + Send + 'static,
) -> Result<T, ApiError> {
    let started = Instant::now();
    let permit = tokio::time::timeout(
        state.options.timeout,
        Arc::clone(&state.blocking_workers).acquire_owned(),
    )
    .await
    .map_err(|_| ApiError::Timeout)?
    .map_err(|_| ApiError::Unavailable("database worker pool is unavailable".into()))?;
    let Some(run_budget) = state.options.timeout.checked_sub(started.elapsed()) else {
        return Err(ApiError::Timeout);
    };
    let config = state.config;
    let workspace_root = state.options.workspace_root;
    let clone_graph_cache = state.clone_graph_cache;
    let cancelled = Arc::new(AtomicBool::new(false));
    let task_cancelled = Arc::clone(&cancelled);
    let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel();
    let mut handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if !config.database.is_file() {
            return Err(DbError::Unavailable("index database does not exist".into()));
        }
        let mut db = IndexDatabase::try_open_config_read_only(&config)
            .map_err(DbError::Internal)?
            .ok_or_else(|| {
                DbError::Unavailable("index is not ready for read-only access".into())
            })?;
        if let Some(workspace_root) = workspace_root.as_deref() {
            db.use_worktree_scope(&config.root, Some(workspace_root)).map_err(DbError::Internal)?;
        }
        db.set_lens_clone_graph_cache(clone_graph_cache);
        let _ = interrupt_tx.send(db.interrupt_handle());
        if task_cancelled.load(Ordering::Acquire) {
            return Err(DbError::Unavailable("database request was cancelled".into()));
        }
        operation(&db, &config, &task_cancelled)
    });
    // The timeout is not the only way this request ends: axum DROPS the request future when the
    // client disconnects, and the editor now aborts a file lane the moment an index change makes
    // its answer irrelevant. Dropping the `JoinHandle` only detaches the blocking task, so without
    // this guard a repository-wide clone or treemap scan would keep running — holding one of the
    // two default worker permits — for a reader that has gone away.
    let mut cancel =
        CancelOnDrop { cancelled: Arc::clone(&cancelled), interrupt: Some(interrupt_rx) };
    let deadline = tokio::time::sleep(run_budget);
    tokio::pin!(deadline);
    let result = tokio::select! {
        result = &mut handle => {
            cancel.disarm();
            Some(result)
        },
        () = &mut deadline => {
            // `spawn_blocking` aborts only while still queued. Once running, the SQLite interrupt
            // and cooperative flag stop it; while queued, abort drops the captured permit
            // immediately instead of starving later requests behind work that never started.
            cancel.cancel();
            handle.abort();
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut handle).await;
            None
        },
    };
    match result {
        None => Err(ApiError::Timeout),
        Some(Err(error)) => {
            tracing::error!(target: "rag_rat_mcp::http", %error, "database worker failed");
            Err(ApiError::Internal)
        },
        Some(Ok(Err(DbError::Unavailable(message)))) => Err(ApiError::Unavailable(message)),
        Some(Ok(Err(DbError::Internal(error)))) => {
            // Expected editor-facing staleness (a chunk whose file was edited or deleted since
            // the editor cached it) is a 404, not a server failure — editors hold stale chunk ids
            // after every save, so a 500 + error log per cached id is pure noise.
            if matches!(
                error.downcast_ref::<rag_rat_core::index::IndexError>(),
                Some(
                    rag_rat_core::index::IndexError::Gone { .. }
                        | rag_rat_core::index::IndexError::StaleChunk { .. }
                )
            ) {
                return Err(ApiError::NotFound("chunk is stale or its file was deleted".into()));
            }
            tracing::error!(target: "rag_rat_mcp::http", %error, "HTTP database read failed");
            Err(ApiError::Internal)
        },
        Some(Ok(Ok(value))) => Ok(value),
    }
}

#[derive(Debug)]
enum DbError {
    Unavailable(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for DbError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

#[derive(Debug)]
enum ApiError {
    BadQuery(String),
    NotFound(String),
    Unavailable(String),
    Timeout,
    Internal,
}

impl ApiError {
    fn bad_query(message: impl Into<String>) -> Self {
        Self::BadQuery(message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadQuery(message) => (StatusCode::BAD_REQUEST, message),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, message),
            Self::Unavailable(message) => (StatusCode::SERVICE_UNAVAILABLE, message),
            Self::Timeout => (StatusCode::GATEWAY_TIMEOUT, "database request timed out".into()),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".into()),
        };
        (status, Json(ErrorResponse { error: message })).into_response()
    }
}

#[derive(Deserialize)]
struct FileClonesQuery {
    path: Option<String>,
    theta: Option<f64>,
    min_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct FileQuery {
    path: Option<String>,
}

#[derive(Deserialize)]
struct SymbolHopQuery {
    /// The `sym_<hex>` logical-symbol handle from `/api/file/{symbols,graph}`. Preferred.
    id: Option<String>,
    /// Compatibility fallback for a client that has no handle to send.
    qname: Option<String>,
    limit: Option<String>,
}

#[derive(Deserialize)]
struct ChunkTextQuery {
    chunk_id: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[cfg(test)]
mod tests;
