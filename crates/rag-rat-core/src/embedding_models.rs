//! The single source of truth for every embedding model rag-rat knows about.
//!
//! Adding a model is adding ONE row to [`EMBEDDING_MODELS`] — the persisted model id, its display
//! name, dimension, reconcile-freshness version key, backend, the toml `model = "..."` aliases that
//! select it, and (for asymmetric models) a query instruction prefix. Everything else —
//! `expected_dim`, `default_model_version`, the manifest upsert list, the install dispatch, the
//! `EmbeddingBackend` config selector, the operational-status reporting — reads this table instead
//! of carrying its own hardcoded model-id match arm.
//!
//! This module lives at the crate ROOT (not under `index::ai`) on purpose: [`crate::config`]
//! resolves the toml `model = "..."` selector through [`spec_for_alias`], and config must NOT
//! depend on `index`. Pure data only — no feature gates here, so the table compiles on every
//! feature set; the embedder CONSTRUCTION (which needs `fastembed` / `model2vec`) is gated in
//! `index::ai`.

/// The runtime that actually produces vectors for a model. Maps 1:1 to the `ai_models.runtime`
/// column persisted in the index, and selects which embedder `index::ai` constructs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// The dependency-free locality-sensitive hash embedder — always available, the fallback tier.
    Hash,
    /// A FastEmbed (ONNX) transformer model; gated behind the `fastembed` feature.
    FastEmbed,
    /// A Model2Vec static token→vector lookup; gated behind the `model2vec` feature.
    Model2Vec,
}

impl Backend {
    /// The `ai_models.runtime` column value for this backend. Stable wire string — keep in sync
    /// with the persisted manifest (`upsert_model`), never reorder/rename without a migration.
    pub fn runtime(self) -> &'static str {
        match self {
            Self::Hash => "hash",
            Self::FastEmbed => "fastembed",
            Self::Model2Vec => "model2vec",
        }
    }
}

/// One row in the embedding-model registry: everything the rest of the codebase needs to know about
/// a model, in one place. See the module docs for why this is the single source of truth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EmbeddingModelSpec {
    /// The persisted `ai_models.model_id` — the stable identity used everywhere in the index.
    pub model_id: &'static str,
    /// Human-facing model name (the upstream HF repo / canonical name) for status output.
    pub display: &'static str,
    /// Embedding dimension. A change here is a re-embed (new vectors), not a schema migration.
    pub dim: usize,
    /// The reconcile freshness key (formerly `default_model_version`). Bumping it forces a
    /// re-embed of every chunk for this model. MUST be unique across the table and never the
    /// bare `"v1"` fallback — the registry test guards both.
    pub version: &'static str,
    /// Which runtime produces this model's vectors.
    pub backend: Backend,
    /// The toml `model = "..."` selectors that pick this model. The FIRST alias is the canonical
    /// one `init` renders back into `rag-rat.toml`, so it must be a valid, stable spelling.
    pub aliases: &'static [&'static str],
    /// A query-side instruction prefix for ASYMMETRIC models (queries and passages embed
    /// differently). `""` for every model shipping today — they are all symmetric, so queries
    /// embed RAW (see the BGE instruction-collapse note in `index::ai`). A future asymmetric
    /// model (e.g. CodeRankEmbed) would set this; `embed_query_with` would prepend it on the
    /// query path only.
    pub query_prefix: &'static str,
}

/// Locality-sensitive hash embedder id — the dependency-free fallback tier.
pub const HASH_MODEL_ID: &str = "embedding-hash";
pub const HASH_EMBEDDING_DIM: usize = 384;

/// all-MiniLM-L6-v2 (384-dim) — the default FastEmbed general-purpose backend.
pub const FASTEMBED_MODEL_ID: &str = "fastembed-all-minilm-l6-v2";
pub const FASTEMBED_DISPLAY_MODEL: &str = "sentence-transformers/all-MiniLM-L6-v2";
pub const FASTEMBED_EMBEDDING_DIM: usize = 384;

/// BGE-small-en-v1.5 (#112): a stronger general-retrieval embedder than all-MiniLM at the SAME
/// 384-dim — switching to it is a re-embed, not a schema/dim change. MIT-licensed; ships via
/// fastembed (downloads on first use). Measured against `FASTEMBED_MODEL_ID` on the replay eval.
pub const BGE_SMALL_MODEL_ID: &str = "fastembed-bge-small-en-v1.5";
pub const BGE_SMALL_DISPLAY_MODEL: &str = "BAAI/bge-small-en-v1.5";
pub const BGE_SMALL_EMBEDDING_DIM: usize = 384;

/// jina-embeddings-v2-base-code (#112): a CODE-specific embedder — 768-dim, Apache-2.0, a built-in
/// fastembed model. SYMMETRIC: queries and code both embed RAW (no query instruction, unlike
/// CodeRankEmbed), so it slots into the raw embed path with no prefix. 768-dim → a re-embed, not a
/// schema change. Measured against all-MiniLM / BGE on the commit-replay eval before it ships as
/// the code tier.
pub const JINA_CODE_MODEL_ID: &str = "fastembed-jina-v2-base-code";
pub const JINA_CODE_DISPLAY_MODEL: &str = "jinaai/jina-embeddings-v2-base-code";
pub const JINA_CODE_EMBEDDING_DIM: usize = 768;

/// Model2Vec static-embedding backend: a token→vector lookup + mean-pool (no transformer forward
/// pass), ~100-500× faster than FastEmbed on CPU at some retrieval-quality cost. The right choice
/// for very large repos where the FastEmbed backfill is infeasible.
pub const MODEL2VEC_MODEL_ID: &str = "model2vec-potion-retrieval-32m";
pub const MODEL2VEC_DISPLAY_MODEL: &str = "minishlab/potion-retrieval-32M";
pub const MODEL2VEC_EMBEDDING_DIM: usize = 512;

/// The embedding-model registry. ONE row per model — adding a model is adding a row here. Pure
/// data, no feature gates; the embedder construction is gated in `index::ai`.
pub const EMBEDDING_MODELS: &[EmbeddingModelSpec] = &[
    EmbeddingModelSpec {
        model_id: HASH_MODEL_ID,
        display: "hash",
        dim: HASH_EMBEDDING_DIM,
        version: "hash-v1",
        backend: Backend::Hash,
        aliases: &["hash"],
        query_prefix: "",
    },
    EmbeddingModelSpec {
        model_id: FASTEMBED_MODEL_ID,
        display: FASTEMBED_DISPLAY_MODEL,
        dim: FASTEMBED_EMBEDDING_DIM,
        version: "fastembed-all-minilm-l6-v2-v1",
        backend: Backend::FastEmbed,
        aliases: &["minilm", "fastembed", "minilm-l6"],
        query_prefix: "",
    },
    EmbeddingModelSpec {
        model_id: BGE_SMALL_MODEL_ID,
        display: BGE_SMALL_DISPLAY_MODEL,
        dim: BGE_SMALL_EMBEDDING_DIM,
        version: "fastembed-bge-small-en-v1.5-v1",
        backend: Backend::FastEmbed,
        aliases: &["bge", "bge-small"],
        query_prefix: "",
    },
    EmbeddingModelSpec {
        model_id: JINA_CODE_MODEL_ID,
        display: JINA_CODE_DISPLAY_MODEL,
        dim: JINA_CODE_EMBEDDING_DIM,
        version: "fastembed-jina-v2-base-code-v1",
        backend: Backend::FastEmbed,
        aliases: &["jina", "jina-code"],
        query_prefix: "",
    },
    EmbeddingModelSpec {
        model_id: MODEL2VEC_MODEL_ID,
        display: MODEL2VEC_DISPLAY_MODEL,
        dim: MODEL2VEC_EMBEDDING_DIM,
        version: "model2vec-potion-retrieval-32m-v1",
        backend: Backend::Model2Vec,
        aliases: &["model2vec", "potion", "static"],
        query_prefix: "",
    },
];

/// Look up a spec by its persisted `model_id`.
pub fn spec(model_id: &str) -> Option<&'static EmbeddingModelSpec> {
    EMBEDDING_MODELS.iter().find(|s| s.model_id == model_id)
}

/// Look up a spec by a toml `model = "..."` alias.
pub fn spec_for_alias(alias: &str) -> Option<&'static EmbeddingModelSpec> {
    EMBEDDING_MODELS.iter().find(|s| s.aliases.contains(&alias))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_spec_has_a_unique_non_default_version() {
        // The version is the reconcile freshness key: a duplicate would make two models share a
        // freshness state, and the bare "v1" fallback means the model-version match silently fell
        // through. Both are wrong, so guard against them at the table.
        let mut seen = HashSet::new();
        for spec in EMBEDDING_MODELS {
            assert_ne!(spec.version, "v1", "{} fell back to the default version", spec.model_id);
            assert!(
                seen.insert(spec.version),
                "duplicate version {} (shared by {})",
                spec.version,
                spec.model_id
            );
        }
    }

    #[test]
    fn first_alias_round_trips_to_its_model_id() {
        // `init` renders the canonical (first) alias into the toml; loading it back must resolve
        // the same model. A regression here breaks `init` → config round-trips.
        for spec in EMBEDDING_MODELS {
            let first = spec.aliases.first().expect("every model needs at least one alias");
            assert_eq!(
                spec_for_alias(first).map(|s| s.model_id),
                Some(spec.model_id),
                "alias {first} did not round-trip to {}",
                spec.model_id
            );
        }
    }
}
