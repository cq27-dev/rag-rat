use super::Embedder;
use crate::embedding_models::{MODEL2VEC_EMBEDDING_DIM, MODEL2VEC_MODEL_ID};

/// The Model2Vec HF repo to pull weights from. Not a registry field — it is a construction detail
/// of `Model2VecEmbedder`, not part of the model's index identity.
pub const MODEL2VEC_HF_REPO: &str = "minishlab/potion-retrieval-32M";

pub struct Model2VecEmbedder {
    model: model2vec_rs::model::StaticModel,
}

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
