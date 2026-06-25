//! The Model2Vec backend. `Model2VecEmbedder` is gated behind the `model2vec` feature (it needs the
//! `model2vec-rs` dep); `MODEL2VEC_HF_REPO` is a dep-free string and stays UNCONDITIONAL so its
//! public path `index::ai::MODEL2VEC_HF_REPO` survives in hash-only / `--no-default-features`
//! builds (it was an unconditional `pub const` before the `providers/` extraction — #320/#323
//! review).

/// The Model2Vec HF repo to pull weights from. Not a registry field — a construction detail of
/// `Model2VecEmbedder`, not part of the model's index identity. Dep-free, so it's not gated.
pub const MODEL2VEC_HF_REPO: &str = "minishlab/potion-retrieval-32M";

#[cfg(feature = "model2vec")]
use super::Embedder;
#[cfg(feature = "model2vec")]
use crate::embedding_models::{MODEL2VEC_EMBEDDING_DIM, MODEL2VEC_MODEL_ID};

#[cfg(feature = "model2vec")]
pub struct Model2VecEmbedder {
    model: model2vec_rs::model::StaticModel,
}

#[cfg(feature = "model2vec")]
impl Model2VecEmbedder {
    pub fn new() -> anyhow::Result<Self> {
        // Downloads (and caches) the static model from the Hugging Face hub on first use; L2-
        // normalize so cosine similarity matches the FastEmbed path's expectations.
        let model = model2vec_rs::model::StaticModel::from_pretrained(
            MODEL2VEC_HF_REPO,
            None,
            Some(true),
            None,
        )
        .map_err(|err| anyhow::anyhow!("failed to load Model2Vec model: {err}"))?;
        Ok(Self { model })
    }
}

#[cfg(feature = "model2vec")]
impl Embedder for Model2VecEmbedder {
    fn model_id(&self) -> &str {
        MODEL2VEC_MODEL_ID
    }

    fn dim(&self) -> usize {
        MODEL2VEC_EMBEDDING_DIM
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(self.model.encode(texts))
    }
}
