//! Embedding-provider layer: the `Embedder` trait, the backend dispatch (`embedder_for_spec`),
//! the active-model resolution (`active_embedder`), and one concrete backend per submodule.
//! This module is the single construction site for every embedder — callers reach it through the
//! curated re-exports in `index::ai`.

// Ungated: the ephemeral cookbook lifecycle (#318) spawns a subprocess via std::process — no heavy
// optional dependency, so it ships unconditionally like the Ollama backend.
mod cookbook;
#[cfg(feature = "fastembed")]
mod fastembed;
mod hash;
// Ungated: the module compiles in all builds so its dep-free `MODEL2VEC_HF_REPO` const stays
// available; only `Model2VecEmbedder` (which needs `model2vec-rs`) is feature-gated inside it.
mod model2vec;
// Ungated: the Ollama backend uses `ureq` (already a non-optional workspace dep — see the crates.io
// version check), so there is no heavy optional dependency to gate. No `remote-embed` feature.
mod openai;

use rusqlite::Connection;

pub(crate) use self::cookbook::provision_and_build;
// EVAL-ONLY `pub` seam (#346): the `benchmark-embedding` subcommand (a separate crate)
// provisions an ephemeral box and runs its own measured sweep against it; re-exported `pub`
// under `eval` so it reaches the CLI through `index::ai`.
#[cfg(feature = "eval")]
pub use self::cookbook::provision_box_for_benchmark;
// `verify_ephemeral_remote` is the `pub` init-wizard seam (the CLI's Remote step calls it);
// the underlying `provision_and_build` stays `pub(crate)`.
pub use self::cookbook::{
    CookbookInput, CookbookProvisioner, ProvisionedBox, abort_active_provisioning,
    install_provision_log_sink, verify_ephemeral_remote, verify_ephemeral_remote_cancellable,
};
#[cfg(feature = "fastembed")]
pub use self::fastembed::FastEmbedEmbedder;
pub use self::hash::HashEmbedder;
pub use self::model2vec::MODEL2VEC_HF_REPO;
#[cfg(feature = "model2vec")]
pub use self::model2vec::Model2VecEmbedder;
// Ungated `pub` re-export (crate-public path `crate::index::ai::providers::OpenAiEmbedder`):
// wired into `embedder_for_spec` in #317 task 5, so nothing constructs it yet. The `pub`
// visibility (same pattern as the other backends) exempts it from dead-code/unused-import
// analysis under `-D warnings` until the dispatch arm lands.
pub use self::openai::OpenAiEmbedder;
// The tuning sweep (index::ai::throughput_tune) builds embedders at varied concurrencies.
pub(crate) use self::openai::ProvisionedEmbedderParams;
use crate::config::RemoteEmbeddingConfig;
use crate::embedding_models::{Backend, EmbeddingModelSpec, spec};
use crate::index::ai::{
    EmbeddingScan, ReconcileOptions, active_embedding_model_id, active_remote_config,
    estimated_reconcile_jobs, model, validate_ready_model,
};

pub const MODEL2VEC_MISSING_FEATURE_MESSAGE: &str =
    "Model2Vec backend requested, but this binary was built without Model2Vec support.\nRebuild \
     with default features enabled:\n  cargo install rag-rat";
pub const FASTEMBED_MISSING_FEATURE_MESSAGE: &str =
    "FastEmbed backend requested, but this binary was built without default FastEmbed \
     support.\nRebuild with default features enabled:\n  cargo install rag-rat";

pub trait Embedder {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

pub(crate) fn active_embedder(
    conn: &Connection,
    intra_threads: Option<usize>,
) -> anyhow::Result<Box<dyn Embedder>> {
    let model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &model_id)?;
    validate_ready_model(&model)?;
    let spec = spec(&model.model_id)
        .ok_or_else(|| anyhow::anyhow!("unknown active embedding model `{}`", model.model_id))?;
    // The remote config (persisted at install) flips the effective runtime to Ollama for the active
    // model — same `spec`, only the transport changes. `active_embedder` is the CONNECT-chunk +
    // QUERY-embed path (NOT the ephemeral-CHUNK path — that's the reconcile provisioner, which
    // constructs its own embedder against the provisioned box):
    // - CONNECT → the configured `endpoint`.
    // - EPHEMERAL → the LOCAL `query_endpoint` (queries embed the same model → same GGUF vector
    //   space as the remote-embedded chunks; we never cold-start a GPU box just to embed a query).
    // - local → the local backend.
    let remote = active_remote_config(conn)?;
    let query_remote = remote.as_ref().map(query_embed_config);
    embedder_for_spec(spec, intra_threads, query_remote.as_ref())
}

/// Map a persisted remote config to the one `active_embedder` should embed QUERIES with. Connect →
/// the config unchanged (its `endpoint`). Ephemeral → a connect-shaped config pointed at the LOCAL
/// `query_endpoint`, so the query embeds against the local box (same model) rather than
/// provisioning a remote GPU. Used only on the query/connect-chunk path; the ephemeral CHUNK path
/// provisions.
///
/// The `backend` is PRESERVED (via `..remote.clone()`) — the query embedder uses the same route +
/// server-side model name as the chunk backend — so `query_endpoint` must point at a LOCAL server
/// running that backend + model. Config parsing enforces this for non-ollama backends (an ephemeral
/// infinity/vLLM config must set `query_endpoint` explicitly; there is no ollama-shaped default —
/// see `ConfigError::RemoteQueryEndpointRequiredForBackend`).
fn query_embed_config(remote: &RemoteEmbeddingConfig) -> RemoteEmbeddingConfig {
    if remote.is_ephemeral() {
        RemoteEmbeddingConfig {
            endpoint: remote.query_endpoint.clone(),
            cookbook: None,
            query_endpoint: None,
            // PRESERVE `auth_env` (R5): a user can point `query_endpoint` at an authenticated
            // Ollama and name the bearer-token env var. Dropping it → 401 on every
            // query embed → `embed_query` returns None → silent, PERMANENT BM25.
            // `auth_env` is a secret-free NAME.
            auth_env: remote.auth_env.clone(),
            // DROP `gpu`: this is now a CONNECT-shaped config (it has `endpoint`), and `gpu` is
            // cookbook-only. The query path never reads it, but keep the "gpu ⟹ ephemeral"
            // invariant true for derived configs too.
            gpu: None,
            ..remote.clone()
        }
    } else {
        remote.clone()
    }
}

/// Per-request timeout ceiling for the LIGHT (local `query_endpoint`) path — bounds both the route
/// probe and each single-flight incremental embed so a slow/hung local server can't stall a watcher
/// pass. The provisioned-box path is unaffected (it keeps the configured timeout).
const LIGHT_REQUEST_TIMEOUT_S: u64 = 30;

/// The remote config for a LIGHT (watcher / incremental) embed against the local `query_endpoint`:
/// `query_embed_config` (connect-shaped, local endpoint, backend preserved) but clamped to
/// SINGLE-FLIGHT concurrency and a SHORT request timeout. The query server is a user's local box
/// that may not have been launched with the cookbook's parallelism, so a background watcher edit
/// must not fan out concurrent requests at it (connect endpoints are single-flight for the same
/// reason); the short timeout bounds the route probe + each incremental request so a slow/hung
/// local server can't stall the watcher. Only this local light path is clamped; the provisioned-box
/// path keeps its tuned concurrency + timeout.
fn light_incremental_config(remote: &RemoteEmbeddingConfig) -> RemoteEmbeddingConfig {
    let transport = query_embed_config(remote);
    RemoteEmbeddingConfig {
        concurrency: 1,
        request_timeout_s: transport.request_timeout_s.min(LIGHT_REQUEST_TIMEOUT_S),
        ..transport
    }
}

/// Build the active model's embedder against a caller-supplied connect-shaped `transport` — used by
/// the light path to point at the local `query_endpoint` at clamped concurrency. Like
/// `active_embedder`, but the transport is the passed config, not `query_embed_config` of the
/// persisted remote.
fn light_embedder(
    conn: &Connection,
    intra_threads: Option<usize>,
    transport: &RemoteEmbeddingConfig,
) -> anyhow::Result<Box<dyn Embedder>> {
    let model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &model_id)?;
    validate_ready_model(&model)?;
    let spec = spec(&model.model_id)
        .ok_or_else(|| anyhow::anyhow!("unknown active embedding model `{}`", model.model_id))?;
    embedder_for_spec(spec, intra_threads, Some(transport))
}

/// The outcome of acquiring the CHUNK-embed embedder for a reconcile. `Ready` carries the optional
/// `ProvisionedBox` guard so the caller keeps it alive for the whole embed loop (its `Drop` is the
/// box teardown). This is the ONE place that branches on `is_ephemeral()` for chunk embedding — the
/// reconcile loop just calls [`acquire_chunk_embedder`] and matches.
pub(crate) enum ChunkEmbedder {
    /// An embedder is ready. `provisioned` is `Some` only for an ephemeral box; `None` for
    /// connect/local. `remote` is the already-read active remote config, if any, so reconcile does
    /// not need to re-read potentially malformed meta outside the NotReady path.
    Ready {
        embedder: Box<dyn Embedder>,
        provisioned: Option<ProvisionedBox>,
        remote: Option<Box<RemoteEmbeddingConfig>>,
    },
    /// Ephemeral active model on a non-provisioning pass whose local `query_endpoint` is absent or
    /// fails a probe embed (down, a wrong service on the port, the model not pulled, a dim
    /// mismatch) — defer incremental embedding to an explicit provisioning `rag-rat reconcile`.
    /// When a `query_endpoint` IS set and a probe embed SUCCEEDS, the light path embeds locally
    /// against it instead (returns `Ready` with `provisioned: None`) — see
    /// [`acquire_chunk_embedder`].
    SkipEphemeral,
    /// Ephemeral active model on a PROVISIONING reconcile, but there is NOTHING to embed (the model
    /// is already current). We return this INSTEAD of provisioning so a no-op `rag-rat reconcile`
    /// doesn't cold-start (and immediately tear down) a paid GPU box for zero work (#330-6). The
    /// reconcile maps it to a clean "Current" report.
    NoEphemeralWork,
    /// The model isn't ready (not installed / dim mismatch / provisioning failed). The error is for
    /// diagnostics; the caller reports a generic "model not ready".
    NotReady(anyhow::Error),
}

/// Acquire the CHUNK-embed embedder for a reconcile. EPHEMERAL active model: on a provisioning
/// reconcile, FIRST check for pending candidate chunks — if none, `NoEphemeralWork` (never
/// provision a paid box for zero work, #330-6); otherwise provision the cookbook box + build an
/// embedder against it (the bulk path, `provision_and_build`). On a non-provisioning pass (watcher
/// / maintenance): embed the changed chunks LOCALLY against `query_endpoint` when a probe embed on
/// it SUCCEEDS (the light/incremental path — no cold-start, single-flight, same vector space as the
/// box); `SkipEphemeral` when there is no local query server or the probe fails. CONNECT/local: the
/// usual `active_embedder`. Provisioning happens ONCE here, not per batch. `provision_remote` gates
/// the cold-start (only an explicit `rag-rat reconcile` sets it); `scan`/`options` size the
/// provision-path pending-work check.
/// Strip credentials + path from an endpoint URL before logging it: keep `scheme://host[:port]`
/// only. A debug log is a shared, greppable on-disk artifact, and an endpoint may carry inline
/// `user:pass@` userinfo (the connect/query endpoints support it — see `endpoint_is_loopback`), so
/// the raw URL must never land in a log line.
pub(crate) fn sanitize_endpoint(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    // Authority is up to the first path/query/fragment delimiter; drop any `user:pass@` userinfo.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host_port)| host_port);
    match scheme {
        Some(scheme) => format!("{scheme}://{host_port}"),
        None => host_port.to_string(),
    }
}

pub(crate) fn acquire_chunk_embedder(
    conn: &Connection,
    intra_threads: Option<usize>,
    scan: &EmbeddingScan<'_>,
    options: &ReconcileOptions,
) -> ChunkEmbedder {
    let remote = match active_remote_config(conn) {
        Ok(remote) => remote,
        Err(err) => return ChunkEmbedder::NotReady(err),
    };
    if let Some(remote) = remote.as_ref().filter(|r| r.is_ephemeral()) {
        if !options.provision_remote {
            // LIGHT / incremental pass (watcher, maintenance): NEVER cold-start a paid box. Embed
            // the changed chunks against the local `query_endpoint` — but ONLY after PROVING the
            // configured model can actually embed on it. Build the single-flight light embedder,
            // then probe the real embeddings route with a tiny request:
            //  - no `query_endpoint`, or the probe fails (server down, a DIFFERENT service on the
            //    port, the model not pulled, a dim mismatch) → `SkipEphemeral` (defer to an
            //    explicit provisioning reconcile). A refused connect returns at once, so the common
            //    no-local-server pass stays cheap (BEFORE any O(repo) candidate scan), and a broken
            //    or wrong endpoint never persists `Failed` chunk_embeddings — a bare TCP connect
            //    would only prove SOMETHING listens, so the actual embed probe is what makes this
            //    safe.
            //  - probe OK → embed locally, same backend + model as the box, so the vectors share
            //    ONE space (freshness is endpoint-independent) and semantic search stays current
            //    between big reindexes.
            // NO work-count gate here on purpose: `estimated_reconcile_jobs` decompresses the whole
            // repo, so it is NOT a cheap idle-pass guard — the fast-failing probe is. A reachable
            // server's no-work pass then costs the same policy summary a connect-mode pass already
            // pays. The probe (and every light embed) is bounded by the transport's clamped
            // timeout, so a hung server can't stall the watcher.
            if remote.query_endpoint.is_none() {
                tracing::debug!(target: "rag_rat_core::index::ai::providers", path = "skip_ephemeral", reason = "no_query_endpoint", "light reconcile: no local query_endpoint, deferring to explicit reconcile");
                return ChunkEmbedder::SkipEphemeral;
            }
            let transport = light_incremental_config(remote);
            let embedder = match light_embedder(conn, intra_threads, &transport) {
                Ok(embedder) => embedder,
                // Construction shouldn't fail for a Ready model, but on any error defer rather than
                // fail the watcher pass.
                Err(_) => return ChunkEmbedder::SkipEphemeral,
            };
            if embedder.embed_batch(&["ping".to_string()]).is_err() {
                tracing::debug!(target: "rag_rat_core::index::ai::providers", path = "skip_ephemeral", reason = "local_probe_failed", "light reconcile: query_endpoint probe failed, deferring");
                return ChunkEmbedder::SkipEphemeral;
            }
            // The #356 light path: this is the "local embedding after a git action" the maintenance
            // hook triggers — embeds changed chunks against the LOCAL query_endpoint, no paid box.
            tracing::debug!(target: "rag_rat_core::index::ai::providers", path = "local_query_endpoint", endpoint = %sanitize_endpoint(remote.query_endpoint.as_deref().unwrap_or("")), "light/incremental reconcile embeds locally against query_endpoint");
            return ChunkEmbedder::Ready {
                embedder,
                provisioned: None,
                remote: Some(Box::new(transport)),
            };
        }
        // BEFORE provisioning a paid box, confirm there's actually work to do. An explicit
        // `rag-rat reconcile` on an already-current ephemeral model would otherwise cold-start +
        // tear down a GPU/pod for nothing (#330-6). `--force` makes every chunk a candidate, so a
        // forced reconcile still provisions (count > 0). A count error is non-fatal — fall through
        // to provisioning rather than skip real work on a transient query failure. The estimate is
        // reused below to decide whether a concurrency sweep is worthwhile.
        let estimated_jobs = match estimated_reconcile_jobs(conn, scan, options) {
            Ok(0) => {
                tracing::debug!(target: "rag_rat_core::index::ai::providers", path = "no_ephemeral_work", "explicit reconcile: no candidate work, skipping paid-box provisioning");
                return ChunkEmbedder::NoEphemeralWork;
            },
            Ok(n) => Some(n),
            Err(_) => None,
        };
        let active_model_id = match active_embedding_model_id(conn) {
            Ok(id) => id,
            Err(err) => return ChunkEmbedder::NotReady(err),
        };
        let Some(spec) = spec(&active_model_id) else {
            return ChunkEmbedder::NotReady(anyhow::anyhow!(
                "unknown active embedding model `{active_model_id}`"
            ));
        };
        // Tune in Rust against the box before the bulk reconcile: this is the ONLY provision path
        // with the DB `conn` (for the tune cache) + the configured chunk size, so it's where the
        // sweep runs. The install probe / wizard verify pass `None` (throwaway boxes — no sweep).
        // Only sweep when the live run will actually fan out: a `--max-seconds` / maintenance pass
        // isn't widened (`remote_reconcile_batch_size`), and a tiny `--limit` / few-chunk run sends
        // one request — tuning either would just burn paid-box time the loop can't use. Use
        // `scan.max_embedding_chars` (already clamped to MIN_EMBEDDING_CHARS), NOT the raw option,
        // so the probe texts + tune cache key match the size reconcile actually embeds.
        // ALWAYS pass a TuneRequest so the tune cache is consulted (a prior tuned knee beats the
        // raw cap even on a bounded/tiny pass); `allow_sweep` gates only a fresh sweep —
        // off for a bounded `--max-seconds` run or one too small to fan out
        // (`sweep_is_worthwhile`).
        let tune = self::cookbook::TuneRequest {
            conn,
            max_embedding_chars: scan.max_embedding_chars,
            allow_sweep: crate::index::ai::throughput_tune::sweep_is_worthwhile(
                options.max_seconds,
                estimated_jobs,
                remote.bounded_concurrency(),
                remote.batch_size,
                remote.max_batch_chars,
                scan.max_embedding_chars,
            ),
        };
        tracing::info!(target: "rag_rat_core::index::ai::providers", path = "provision_ephemeral", estimated_jobs = ?estimated_jobs, "explicit reconcile: provisioning ephemeral embedding box");
        return match provision_and_build(remote, spec, Some(tune)) {
            Ok((embedder, provisioned, effective_remote, window_concurrency)) => {
                // Size the reconcile selection window by the EMBEDDER's real fan-out (the tuned
                // knee), not the user's cap: `remote_reconcile_batch_size`
                // multiplies by `concurrency`, so a 128-cap config with a knee of 4
                // would otherwise load a 32x-too-wide window the embedder only
                // drains 4-at-a-time. This `remote` is window-sizing only (NOT persisted
                // — the active-config meta is written from the cap by `install_remote_model`).
                let mut window_remote = effective_remote;
                window_remote.concurrency = window_concurrency;
                ChunkEmbedder::Ready {
                    embedder: Box::new(embedder),
                    provisioned: Some(provisioned),
                    remote: Some(Box::new(window_remote)),
                }
            },
            Err(err) => ChunkEmbedder::NotReady(err),
        };
    }
    // Connect / local: the usual single construction site.
    match active_embedder(conn, intra_threads) {
        Ok(embedder) =>
            ChunkEmbedder::Ready { embedder, provisioned: None, remote: remote.map(Box::new) },
        Err(err) => ChunkEmbedder::NotReady(err),
    }
}

/// Build the embedder for a registry spec. The EFFECTIVE runtime is `remote.is_some() ? Ollama :
/// spec.backend` (#317 rework): a `[llm.embedding.remote]` block serves the SELECTED model
/// (`spec`) via Ollama instead of in-process — same `model_id` + `dim`, transport overridden. The
/// single construction site for every model; the `#[cfg]` gating + missing-feature bails for builds
/// without `fastembed` / `model2vec` apply only on the local path (Ollama is unconditional).
pub(crate) fn embedder_for_spec(
    spec: &'static EmbeddingModelSpec,
    intra_threads: Option<usize>,
    remote: Option<&RemoteEmbeddingConfig>,
) -> anyhow::Result<Box<dyn Embedder>> {
    // Remote present → serve the SELECTED model over Ollama, regardless of its local
    // `spec.backend`. `spec.dim`/`spec.model_id` are the selected model's — the embedder
    // reports that id (so chunks key by the model, not the runtime) and validates the server's
    // vectors against that dim.
    if let Some(remote) = remote {
        let _ = intra_threads;
        return Ok(Box::new(OpenAiEmbedder::from_remote_config(remote, spec.model_id, spec.dim)?));
    }
    // No remote block → in-process embedder, dispatched on the model's local backend. `Ollama` is a
    // transport-only runtime (no registry row carries it), so it cannot appear here.
    match spec.backend {
        Backend::Hash => Ok(Box::new(HashEmbedder)),
        Backend::FastEmbed => {
            #[cfg(feature = "fastembed")]
            {
                Ok(Box::new(FastEmbedEmbedder::for_model_id(
                    spec.model_id,
                    spec.dim,
                    intra_threads,
                )?))
            }
            #[cfg(not(feature = "fastembed"))]
            {
                let _ = intra_threads;
                anyhow::bail!("{}", FASTEMBED_MISSING_FEATURE_MESSAGE)
            }
        },
        Backend::Model2Vec => {
            #[cfg(feature = "model2vec")]
            {
                let _ = intra_threads;
                Ok(Box::new(Model2VecEmbedder::new()?))
            }
            #[cfg(not(feature = "model2vec"))]
            {
                let _ = intra_threads;
                anyhow::bail!("{}", MODEL2VEC_MISSING_FEATURE_MESSAGE)
            }
        },
        // `Backend::Ollama` is a transport-only runtime value — no registry row carries it, so a
        // local-path dispatch can never reach it. Serving via Ollama goes through the `remote`
        // branch above, not here.
        Backend::Ollama => anyhow::bail!(
            "internal error: Backend::Ollama is a transport, not a selectable local model"
        ),
    }
}

#[cfg(test)]
pub struct MockEmbedder {
    model_id: String,
    dim: usize,
}

#[cfg(test)]
impl MockEmbedder {
    pub fn new(model_id: impl Into<String>, dim: usize) -> Self {
        Self { model_id: model_id.into(), dim }
    }
}

#[cfg(test)]
impl Embedder for MockEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| crate::index::ai::hash_embed_text(text, self.dim)).collect())
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::embedding_models::FASTEMBED_MODEL_ID;
    use crate::index::ai::set_active_remote_config;

    /// An in-memory index with the schema applied + the manifest seeded, with `model_id` forced
    /// Ready and made the active embedding model. Mirrors how a real install leaves the DB, so
    /// `active_embedder` resolves the active spec exactly as in production.
    fn conn_with_active_model(model_id: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        crate::index::ai::ensure_model_manifest(&conn).unwrap();
        let spec = spec(model_id).unwrap();
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2
             WHERE model_id = ?1",
            rusqlite::params![model_id, i64::try_from(spec.dim).unwrap()],
        )
        .unwrap();
        crate::index::ai::set_meta(&conn, "active_embedding_model", model_id).unwrap();
        conn
    }

    fn remote_at(endpoint: &str) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: crate::config::RemoteBackend::Ollama,
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

    #[test]
    fn remote_block_flips_a_local_model_to_an_ollama_embedder_keeping_the_model_id() {
        // #317 rework: the SELECTED model (fastembed all-minilm) is active, and a persisted remote
        // config flips its runtime to Ollama. Construction doesn't connect (a closed port is fine),
        // so we assert the resolved embedder reports the SELECTED model's id + dim — NOT a
        // hardcoded ollama id. chunk_embeddings key by the selected model regardless of
        // runtime.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let conn = conn_with_active_model(FASTEMBED_MODEL_ID);
        set_active_remote_config(&conn, &remote_at(&format!("http://127.0.0.1:{port}"))).unwrap();

        let embedder = active_embedder(&conn, None).expect("ollama embedder constructs");
        assert_eq!(embedder.model_id(), FASTEMBED_MODEL_ID, "keeps the selected model's id");
        assert_eq!(embedder.dim(), spec(FASTEMBED_MODEL_ID).unwrap().dim);
    }

    #[test]
    fn embedder_for_spec_with_remote_serves_any_model_over_ollama() {
        // The effective runtime is `remote.is_some() ? Ollama : spec.backend`: passing a remote
        // config builds an OpenAiEmbedder for the selected spec regardless of its local backend.
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        let embedder =
            embedder_for_spec(spec, None, Some(&remote_at("http://127.0.0.1:1"))).unwrap();
        assert_eq!(embedder.model_id(), FASTEMBED_MODEL_ID);
    }

    #[test]
    fn embedder_for_spec_without_remote_uses_the_local_backend() {
        // No remote → dispatch on the model's local backend. Hash is always available, so it's the
        // feature-independent assertion.
        let spec = spec(crate::embedding_models::HASH_MODEL_ID).unwrap();
        let embedder = embedder_for_spec(spec, None, None).unwrap();
        assert_eq!(embedder.model_id(), crate::embedding_models::HASH_MODEL_ID);
    }

    fn ephemeral_at(query_endpoint: &str, auth_env: Option<&str>) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: crate::config::RemoteBackend::Ollama,
            endpoint: None,
            cookbook: Some("@rag-rat/cookbook/modal".to_string()),
            query_endpoint: Some(query_endpoint.to_string()),
            auth_env: auth_env.map(str::to_string),
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: 5,
        }
    }

    #[test]
    fn query_embed_config_for_ephemeral_points_at_the_local_query_endpoint_keeping_auth() {
        // The QUERY path for an ephemeral active config embeds against the LOCAL query box (same
        // model → same vector space), NOT the cookbook. `query_embed_config` rewrites the config to
        // a connect-shaped one pointed at `query_endpoint`, dropping the cookbook — but PRESERVING
        // `auth_env` (R5), since the query box may be an authenticated Ollama. This is a pure
        // config mapping (no embedder construction), so the named env var need not exist.
        let q = query_embed_config(&ephemeral_at("http://127.0.0.1:11434", Some("OLLAMA_TOKEN")));
        assert!(q.is_connect() && !q.is_ephemeral());
        assert_eq!(q.endpoint.as_deref(), Some("http://127.0.0.1:11434"));
        assert_eq!(q.cookbook, None);
        assert_eq!(q.auth_env.as_deref(), Some("OLLAMA_TOKEN"), "auth_env preserved for query box");
    }

    #[test]
    fn active_embedder_for_ephemeral_builds_against_the_query_endpoint() {
        // An ephemeral active model: `active_embedder` (the query/connect path) must build against
        // the LOCAL query_endpoint — never provision a cookbook box just to embed a query.
        // Construction doesn't connect, so a closed port is fine.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let conn = conn_with_active_model(FASTEMBED_MODEL_ID);
        // No `auth_env` here: this test BUILDS the embedder (which would try to resolve a named env
        // var); the auth-preservation behavior is covered by the `query_embed_config` mapping test.
        set_active_remote_config(&conn, &ephemeral_at(&format!("http://127.0.0.1:{port}"), None))
            .unwrap();

        let embedder = active_embedder(&conn, None).expect("query embedder constructs locally");
        assert_eq!(embedder.model_id(), FASTEMBED_MODEL_ID);
        assert_eq!(embedder.dim(), spec(FASTEMBED_MODEL_ID).unwrap().dim);
    }

    #[test]
    fn query_embed_config_for_connect_is_unchanged() {
        let connect = remote_at("http://box:11434");
        let q = query_embed_config(&connect);
        assert_eq!(q, connect, "connect query config is the config unchanged");
    }

    #[test]
    fn sanitize_endpoint_strips_credentials_and_path() {
        assert_eq!(sanitize_endpoint("http://u:p@h:7997/embeddings"), "http://h:7997");
        assert_eq!(sanitize_endpoint("http://localhost:7997"), "http://localhost:7997");
        assert_eq!(
            sanitize_endpoint("https://user:secret@gpu.host/v1/embeddings"),
            "https://gpu.host"
        );
        assert_eq!(sanitize_endpoint(""), "");
    }
}
