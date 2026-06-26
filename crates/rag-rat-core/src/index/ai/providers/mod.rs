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
    // The remote config is read from the DB meta (persisted at install) ONLY for an Ollama-backed
    // active model — every other backend ignores it. This is the single construction site, so both
    // callers (reconcile's chunk-embed and `embed_query`'s query-embed) transparently get an
    // `OllamaEmbedder` pointed at the same endpoint + model: connect mode has no chunk/query split
    // (the mirror is deferred to ephemeral, #318).
    let remote = match spec.backend {
        Backend::Ollama => active_remote_config(conn)?,
        _ => None,
    };
    embedder_for_spec(spec, intra_threads, remote.as_ref())
}

/// Build the embedder for a registry spec, dispatching on its `backend`. The single construction
/// site for every model — the per-model factory fns this replaced (`fastembed_embedder`,
/// `bge_small_embedder`, `jina_code_embedder`, `model2vec_embedder`) collapsed into this dispatch.
/// The `#[cfg]` gating + missing-feature bails for builds without `fastembed` / `model2vec` are
/// preserved here.
pub(crate) fn embedder_for_spec(
    spec: &'static EmbeddingModelSpec,
    intra_threads: Option<usize>,
    remote: Option<&RemoteEmbeddingConfig>,
) -> anyhow::Result<Box<dyn Embedder>> {
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
        // The Ollama HTTP backend (#317 task 5). `registry_dim` is `spec.dim` — the dim parity
        // contract the embedder checks every batch against. The remote connection params come from
        // the persisted meta (read in `active_embedder`), not from config threading; absence means
        // an Ollama model was activated without its config being written — a corrupted/half-done
        // install — so bail with the recovery hint rather than constructing a half-formed embedder.
        Backend::Ollama => {
            let _ = intra_threads;
            let remote = remote.ok_or_else(|| {
                anyhow::anyhow!(
                    "ollama backend active but no remote config persisted — run `rag-rat \
                     reconcile`/install to re-activate the model"
                )
            })?;
            Ok(Box::new(OllamaEmbedder::from_remote_config(remote, spec.dim)?))
        },
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
    use crate::embedding_models::{OLLAMA_ALL_MINILM_EMBEDDING_DIM, OLLAMA_ALL_MINILM_MODEL_ID};
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
    fn active_embedder_yields_an_ollama_embedder_when_remote_config_is_persisted() {
        // Construction does not connect (from_remote_config only parses the endpoint), so a closed
        // port is fine — we assert the resolved embedder's identity + dim, the construction path.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let conn = conn_with_active_model(OLLAMA_ALL_MINILM_MODEL_ID);
        set_active_remote_config(&conn, &remote_at(&format!("http://127.0.0.1:{port}"))).unwrap();

        let embedder = active_embedder(&conn, None).expect("ollama embedder constructs");
        assert_eq!(embedder.model_id(), OLLAMA_ALL_MINILM_MODEL_ID);
        assert_eq!(embedder.dim(), OLLAMA_ALL_MINILM_EMBEDDING_DIM);
    }

    #[test]
    fn active_embedder_bails_with_a_clear_message_when_remote_config_is_missing() {
        // Ollama active but NO remote-config meta → the dispatch must refuse with the recovery
        // hint, not panic or silently construct a half-formed embedder.
        let conn = conn_with_active_model(OLLAMA_ALL_MINILM_MODEL_ID);
        // `Box<dyn Embedder>` is not `Debug`, so `expect_err` won't compile — match the result.
        let msg = match active_embedder(&conn, None) {
            Ok(_) => panic!("must bail without persisted remote cfg"),
            Err(err) => err.to_string(),
        };
        assert!(msg.contains("no remote config persisted"), "clear message: {msg}");
    }

    #[test]
    fn embedder_for_spec_ignores_remote_for_the_hash_backend() {
        // A non-ollama backend ignores `remote` entirely — passing Some or None is identical.
        let spec = spec(crate::embedding_models::HASH_MODEL_ID).unwrap();
        let with_none = embedder_for_spec(spec, None, None).unwrap();
        assert_eq!(with_none.model_id(), crate::embedding_models::HASH_MODEL_ID);
    }
}
