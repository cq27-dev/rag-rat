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
mod ollama;

use rusqlite::Connection;

pub(crate) use self::cookbook::provision_and_build;
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
// Ungated `pub` re-export (crate-public path `crate::index::ai::providers::OllamaEmbedder`):
// wired into `embedder_for_spec` in #317 task 5, so nothing constructs it yet. The `pub`
// visibility (same pattern as the other backends) exempts it from dead-code/unused-import
// analysis under `-D warnings` until the dispatch arm lands.
pub use self::ollama::OllamaEmbedder;
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
    /// Ephemeral active model on a non-provisioning pass — skip remote chunk embedding entirely.
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
/// embedder against it (the bulk path, `provision_and_build`). On a non-provisioning pass
/// (watcher), `SkipEphemeral`. CONNECT/local: the usual `active_embedder`. Provisioning happens
/// ONCE here, not per batch. `provision_remote` gates the cold-start (only an explicit `rag-rat
/// reconcile` sets it); `scan`/`options` size the pending-work check exactly like the embed loop.
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
            return ChunkEmbedder::SkipEphemeral;
        }
        // BEFORE provisioning a paid box, confirm there's actually work to do. An explicit
        // `rag-rat reconcile` on an already-current ephemeral model would otherwise cold-start +
        // tear down a GPU/pod for nothing (#330-6). `--force` makes every chunk a candidate, so a
        // forced reconcile still provisions (count > 0). A count error is non-fatal — fall through
        // to provisioning rather than skip real work on a transient query failure.
        if let Ok(0) = estimated_reconcile_jobs(conn, scan, options) {
            return ChunkEmbedder::NoEphemeralWork;
        }
        let active_model_id = match active_embedding_model_id(conn) {
            Ok(id) => id,
            Err(err) => return ChunkEmbedder::NotReady(err),
        };
        let Some(spec) = spec(&active_model_id) else {
            return ChunkEmbedder::NotReady(anyhow::anyhow!(
                "unknown active embedding model `{active_model_id}`"
            ));
        };
        return match provision_and_build(remote, spec) {
            Ok((embedder, provisioned)) => ChunkEmbedder::Ready {
                embedder: Box::new(embedder),
                provisioned: Some(provisioned),
                remote: Some(Box::new(remote.clone())),
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
        return Ok(Box::new(OllamaEmbedder::from_remote_config(remote, spec.model_id, spec.dim)?));
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
        // config builds an OllamaEmbedder for the selected spec regardless of its local backend.
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
}
