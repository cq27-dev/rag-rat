//! Embedding-provider layer: the `Embedder` trait, the backend dispatch (`embedder_for_spec`),
//! the active-model resolution (`active_embedder`), and one concrete backend per submodule.
//! This module is the single construction site for every embedder — callers reach it through the
//! curated re-exports in `index::ai`.

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
    active_embedding_model_id, active_remote_config, model, validate_ready_model,
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
    // The remote config (persisted at install) is what FLIPS the effective runtime to Ollama for
    // the active model — the model is the same (`spec`), only the transport changes. Read it once
    // here so this single construction site serves both callers (reconcile's chunk-embed and
    // `embed_query`'s query-embed): connect mode has no chunk/query split (the mirror is deferred
    // to ephemeral, #318). A non-remote active model gets `None` → its local backend.
    let remote = active_remote_config(conn)?;
    embedder_for_spec(spec, intra_threads, remote.as_ref())
}

/// Build the embedder for a registry spec. The EFFECTIVE runtime is `remote.is_some() ? Ollama :
/// spec.backend` (#317 rework): a `[local_ai.embedding.remote]` block serves the SELECTED model
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
    use crate::config::RemoteMode;
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
            mode: RemoteMode::Connect,
            model: "all-minilm".to_string(),
            endpoint: Some(endpoint.to_string()),
            auth_env: None,
            batch_size: 256,
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
}
