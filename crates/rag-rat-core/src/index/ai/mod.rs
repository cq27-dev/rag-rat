mod helpers;
mod policy;
mod reconcile;
mod status;
mod store;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub(crate) use helpers::*;
pub(crate) use policy::*;
pub(crate) use reconcile::*;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::Serialize;
use sha2::{Digest, Sha256};
pub(crate) use status::*;
pub(crate) use store::*;

#[cfg(feature = "fastembed")]
use crate::embedding_models::{BGE_SMALL_MODEL_ID, JINA_CODE_MODEL_ID};
use crate::embedding_models::{HASH_EMBEDDING_DIM, HASH_MODEL_ID};
#[cfg(feature = "model2vec")]
use crate::embedding_models::{MODEL2VEC_EMBEDDING_DIM, MODEL2VEC_MODEL_ID};
use crate::index::now_ms;
use crate::language::Language;

/// The Model2Vec HF repo to pull weights from. Not a registry field — it is a construction detail
/// of `Model2VecEmbedder`, not part of the model's index identity.
pub const MODEL2VEC_HF_REPO: &str = "minishlab/potion-retrieval-32M";
pub const MODEL2VEC_MISSING_FEATURE_MESSAGE: &str =
    "Model2Vec backend requested, but this binary was built without Model2Vec support.\nRebuild \
     with default features enabled:\n  cargo install rag-rat";
pub const FASTEMBED_MISSING_FEATURE_MESSAGE: &str =
    "FastEmbed backend requested, but this binary was built without default FastEmbed \
     support.\nRebuild with default features enabled:\n  cargo install rag-rat";
const ACTIVE_EMBEDDING_MODEL_META: &str = "active_embedding_model";
const ACTIVE_EMBEDDING_MODEL_VERSION_META: &str = "embedding_active_model_version";
const LAST_EMBEDDING_RECONCILE_STARTED_META: &str = "last_embedding_reconcile_started_at_ms";
const LAST_EMBEDDING_RECONCILE_FINISHED_META: &str = "last_embedding_reconcile_finished_at_ms";
const DEFAULT_BATCH_SIZE: usize = 64;
pub const DEFAULT_MAX_EMBEDDING_CHARS: usize = 4_000;
const MIN_EMBEDDING_CHARS: usize = 80;
pub const EMBEDDING_TEXT_VERSION: &str = "embedding-text-v2";
const LEGACY_MODEL_IDS: &[&str] = &["embedding-small"];
#[cfg(feature = "fastembed")]
const FASTEMBED_HF_CACHE_REPO_DIR: &str = "models--Qdrant--all-MiniLM-L6-v2-onnx";

pub trait Embedder {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

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
        Ok(texts.iter().map(|text| hash_embed_text(text, self.dim)).collect())
    }
}

#[cfg(feature = "fastembed")]
pub struct FastEmbedEmbedder {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
    model_id: &'static str,
    dim: usize,
}

#[cfg(feature = "fastembed")]
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

#[cfg(feature = "fastembed")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ArtifactStatus {
    Current,
    Missing,
    Stale,
    Failed,
    Blocked,
    Disabled,
}

impl ArtifactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "Current",
            Self::Missing => "Missing",
            Self::Stale => "Stale",
            Self::Failed => "Failed",
            Self::Blocked => "Blocked",
            Self::Disabled => "Disabled",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalAiStatus {
    pub embedding: CapabilityStatus,
    pub artifacts: ArtifactCounts,
    pub fastembed: FastEmbedOperationalStatus,
    pub last_reconcile: Option<LastReconcileStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityStatus {
    pub capability: String,
    pub model_id: String,
    pub state: String,
    pub installed: bool,
    pub disabled: bool,
    pub current_artifacts: u64,
    pub stale_artifacts: u64,
    pub failed_artifacts: u64,
    pub blocked_artifacts: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FastEmbedOperationalStatus {
    pub backend: String,
    pub build_feature_enabled: bool,
    pub model_id: String,
    pub model: String,
    pub dim: usize,
    pub cache: String,
    pub installed: bool,
    pub active: bool,
    pub status: String,
    pub current_embeddings: u64,
    pub eligible_embeddings: u64,
    pub skipped_embeddings: u64,
    pub stale_embeddings: u64,
    pub missing_embeddings: u64,
    pub failed_embeddings: u64,
    pub failed_retryable_embeddings: u64,
    pub failed_waiting_embeddings: u64,
    pub message: Option<String>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactCounts {
    pub total_chunks: u64,
    pub eligible_chunks: u64,
    pub skipped_chunks: u64,
    pub current: u64,
    pub missing: u64,
    pub stale: u64,
    pub failed: u64,
    pub blocked: u64,
    pub disabled: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LastReconcileStatus {
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub batch_size: u64,
    pub processed_chunks: u64,
    pub embeddings_written: u64,
    pub blocked_chunks: u64,
    pub elapsed_ms: u64,
    pub input_chars: u64,
    pub chunks_per_sec: f64,
    pub chars_per_sec: f64,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub model_id: String,
    pub capability: String,
    pub embedding_dim: Option<i64>,
    pub runtime: String,
    pub installed: bool,
    pub disabled: bool,
    pub status: String,
    pub installed_at_ms: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileReport {
    pub processed_chunks: u64,
    pub embeddings_written: u64,
    pub skipped_chunks: u64,
    pub failed_chunks: u64,
    pub blocked_chunks: u64,
    pub model_id: String,
    pub model_version: String,
    pub embedding_dim: usize,
    pub batch_size: usize,
    pub max_embedding_chars: usize,
    pub forced: bool,
    pub changed_first: bool,
    pub until_clean: bool,
    pub max_seconds: Option<u64>,
    pub work_reasons: BTreeMap<String, u64>,
    pub skipped_by_policy: BTreeMap<String, u64>,
    pub input_chars: u64,
    pub truncated_inputs: u64,
    pub elapsed_ms: u64,
    pub chunks_per_sec: f64,
    pub chars_per_sec: f64,
    pub avg_chars_per_chunk: f64,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconcilePlan {
    pub embeddings: EmbeddingReconcilePlan,
    pub summaries: SummaryReconcilePlan,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingReconcilePlan {
    pub model_id: String,
    pub model_version: String,
    pub dim: usize,
    pub available: bool,
    pub current: u64,
    pub missing: u64,
    pub stale: u64,
    pub model_changed: u64,
    pub dim_changed: u64,
    pub failed_retryable: u64,
    pub failed_waiting: u64,
    pub blocked: u64,
    pub disabled: u64,
    pub skipped_total: u64,
    pub skipped_by_policy: BTreeMap<String, u64>,
    pub missing_by_priority: BTreeMap<String, u64>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryReconcilePlan {
    pub enabled: bool,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReconcileReason {
    Missing,
    SourceChanged,
    InputChanged,
    ModelChanged,
    DimChanged,
    RetryAfterFailure,
    Forced,
}

impl ReconcileReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "Missing",
            Self::SourceChanged => "SourceChanged",
            Self::InputChanged => "InputChanged",
            Self::ModelChanged => "ModelChanged",
            Self::DimChanged => "DimChanged",
            Self::RetryAfterFailure => "RetryAfterFailure",
            Self::Forced => "Forced",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReconcileOptions {
    pub limit: Option<u32>,
    pub batch_size: Option<u32>,
    pub force: bool,
    pub until_clean: bool,
    pub changed_first: bool,
    pub max_seconds: Option<u64>,
    pub max_embedding_chars: usize,
    /// ONNX Runtime intra-op thread cap (`ort_threads`). `None` lets the backend pick (all cores).
    pub intra_threads: Option<usize>,
}

impl Default for ReconcileOptions {
    fn default() -> Self {
        Self {
            limit: None,
            batch_size: None,
            force: false,
            until_clean: false,
            changed_first: false,
            max_seconds: None,
            max_embedding_chars: DEFAULT_MAX_EMBEDDING_CHARS,
            intra_threads: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingPolicyDecision {
    pub policy: String,
    pub priority: i64,
    pub eligible: bool,
}

#[derive(Debug, Clone, Serialize)]
pub enum ReconcileProgress {
    Started { model_id: String, total_chunks: u64, batch_size: usize },
    Batch { processed_chunks: u64, total_chunks: u64, embeddings_written: u64, blocked_chunks: u64 },
    Finished { processed_chunks: u64, embeddings_written: u64, blocked_chunks: u64 },
}

#[derive(Debug, Clone)]
pub(crate) struct CurrentChunk {
    id: i64,
    path: String,
    language: String,
    file_kind: String,
    chunk_kind: String,
    symbol_path: Option<String>,
    text: String,
    text_hash: String,
    embedding_status: Option<String>,
    source_text_hash: Option<String>,
    model_version: Option<String>,
    embedding_dim: Option<i64>,
    input_hash: Option<String>,
    embedding_text_version: Option<String>,
    next_retry_after_ms: Option<i64>,
    reason: ReconcileReason,
}

#[derive(Debug)]
pub(crate) struct PreparedEmbeddingJob {
    id: i64,
    text_hash: String,
    input_text: String,
    input_hash: String,
    input_chars: usize,
    input_truncated: bool,
    policy: String,
    priority: i64,
    reason: ReconcileReason,
}

pub(crate) struct SelectedBatch {
    jobs: Vec<PreparedEmbeddingJob>,
}

impl CurrentChunk {
    fn reason(
        &self,
        model_version: &str,
        dim: usize,
        now_ms: i64,
        _max_embedding_chars: usize,
    ) -> ReconcileReason {
        if self.reason == ReconcileReason::Forced {
            return ReconcileReason::Forced;
        }
        if self.embedding_status.is_none() {
            return ReconcileReason::Missing;
        }
        if self.source_text_hash.as_deref() != Some(self.text_hash.as_str()) {
            return ReconcileReason::SourceChanged;
        }
        if self.input_hash.as_deref().is_none_or(str::is_empty) {
            return ReconcileReason::InputChanged;
        }
        if self.model_version.as_deref() != Some(model_version)
            || self.embedding_text_version.as_deref() != Some(EMBEDDING_TEXT_VERSION)
        {
            return ReconcileReason::ModelChanged;
        }
        if self.embedding_dim != Some(i64::try_from(dim).unwrap_or(i64::MAX)) {
            return ReconcileReason::DimChanged;
        }
        if self.embedding_status.as_deref() == Some("Failed")
            && self.next_retry_after_ms.unwrap_or(0) <= now_ms
        {
            return ReconcileReason::RetryAfterFailure;
        }
        ReconcileReason::Missing
    }
}

/// The embedding-model identity for one reconcile run. Stable across every batch, so it
/// travels as a context struct rather than as repeated positional args (rust-modern-style:
/// separate context from per-call command).
pub(crate) struct EmbeddingScan<'a> {
    model_id: &'a str,
    model_version: &'a str,
    dim: usize,
    max_embedding_chars: usize,
}

pub(crate) struct EmbeddingInput {
    text: String,
    chars: usize,
    truncated: bool,
}

pub struct QueryEmbedding {
    pub model_id: String,
    pub dim: usize,
    pub vector: Vec<f32>,
}
