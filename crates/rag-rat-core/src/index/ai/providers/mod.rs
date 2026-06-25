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
use crate::embedding_models::{Backend, EmbeddingModelSpec, spec};
use crate::index::ai::{active_embedding_model_id, model, validate_ready_model};

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
    embedder_for_spec(spec, intra_threads)
}

/// Build the embedder for a registry spec, dispatching on its `backend`. The single construction
/// site for every model — the per-model factory fns this replaced (`fastembed_embedder`,
/// `bge_small_embedder`, `jina_code_embedder`, `model2vec_embedder`) collapsed into this dispatch.
/// The `#[cfg]` gating + missing-feature bails for builds without `fastembed` / `model2vec` are
/// preserved here.
pub(crate) fn embedder_for_spec(
    spec: &'static EmbeddingModelSpec,
    intra_threads: Option<usize>,
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
        // The Ollama HTTP backend is wired in #317 task 5 (dispatch + role split). The registry
        // row + enum variant exist as of task 2 so the table is complete; constructing one bails
        // until then.
        Backend::Ollama => anyhow::bail!("ollama embedding backend not yet wired (#317 task 5)"),
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
