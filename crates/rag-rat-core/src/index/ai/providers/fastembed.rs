use super::Embedder;
use crate::embedding_models::{BGE_SMALL_MODEL_ID, JINA_CODE_MODEL_ID};
use crate::index::ai::fastembed_cache_dir;

pub struct FastEmbedEmbedder {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
    model_id: &'static str,
    dim: usize,
}

impl FastEmbedEmbedder {
    /// Construct the FastEmbed embedder for a registered model id. This is the ONLY place that maps
    /// a `model_id` to a `fastembed::EmbeddingModel` enum — every fastembed model the registry
    /// knows about is selected here, defaulting to all-MiniLM for the (registry-guaranteed)
    /// MiniLM id. The `dim` comes from the registry spec so the embedder reports the right
    /// dimension without a second source of truth.
    ///
    /// All current fastembed models are SYMMETRIC (queries and code embed raw); fastembed applies
    /// each model's Mean pooling + L2-normalize internally, so nothing is overridden here. See the
    /// BGE instruction-collapse note in `embedding_models`.
    pub fn for_model_id(
        model_id: &'static str,
        dim: usize,
        intra_threads: Option<usize>,
    ) -> anyhow::Result<Self> {
        let model = match model_id {
            BGE_SMALL_MODEL_ID => fastembed::EmbeddingModel::BGESmallENV15,
            JINA_CODE_MODEL_ID => fastembed::EmbeddingModel::JinaEmbeddingsV2BaseCode,
            _ => fastembed::EmbeddingModel::AllMiniLML6V2,
        };
        Self::with_model(model, model_id, dim, intra_threads)
    }

    fn with_model(
        model: fastembed::EmbeddingModel,
        model_id: &'static str,
        dim: usize,
        intra_threads: Option<usize>,
    ) -> anyhow::Result<Self> {
        use fastembed::{InitOptions, TextEmbedding};
        let mut options = InitOptions::new(model)
            .with_cache_dir(fastembed_cache_dir())
            .with_show_download_progress(true);
        // `ort_threads` caps the ONNX Runtime intra-op thread pool. Microsoft's prebuilt ORT
        // binaries (what fastembed downloads) are OpenMP-based, where this has no effect and
        // OMP_NUM_THREADS (set from `omp_threads`) is the lever instead — see docs/config.md.
        // We still apply it so non-OpenMP builds honor the configured cap.
        if let Some(threads) = intra_threads.filter(|threads| *threads > 0) {
            options = options.with_intra_threads(threads);
        }
        Ok(Self { model: std::sync::Mutex::new(TextEmbedding::try_new(options)?), model_id, dim })
    }
}

impl Embedder for FastEmbedEmbedder {
    fn model_id(&self) -> &str {
        self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let documents = texts.iter().map(String::as_str).collect::<Vec<_>>();
        let mut model =
            self.model.lock().map_err(|_| anyhow::anyhow!("fastembed model lock poisoned"))?;
        model.embed(documents, None)
    }
}
