use rag_rat_base::embedding_models::{HASH_EMBEDDING_DIM, HASH_MODEL_ID};

use crate::providers::Embedder;
use crate::serving::hash_embed_text;

pub struct HashEmbedder;

impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        HASH_MODEL_ID
    }

    fn dim(&self) -> usize {
        HASH_EMBEDDING_DIM
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| hash_embed_text(text, HASH_EMBEDDING_DIM)).collect())
    }
}
