//! The single source of truth for every embedding model rag-rat knows about.
//!
//! Adding a model is adding ONE row to [`EMBEDDING_MODELS`] — the persisted model id (the HF path,
//! which is ALSO the toml `model = "..."` selector — NO aliases, #317), its display name,
//! dimension, reconcile-freshness version key, backend, and (for asymmetric models) a query
//! instruction prefix. Everything else — `expected_dim`, `default_model_version`, the manifest
//! upsert list, the install dispatch, the `EmbeddingBackend` config selector, the
//! operational-status reporting — reads this table instead of carrying its own hardcoded model-id
//! match arm.
//!
//! This module lives at the crate ROOT (not under `index::ai`) on purpose: [`crate::config`]
//! resolves the toml `model = "..."` selector through [`spec`] (the model_id), and config must NOT
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
    /// A remote Ollama server (`POST /api/embed`); always compiled in (the embedder uses `ureq`,
    /// already a non-optional workspace dep, so there is no heavy optional dependency to gate — no
    /// `remote-embed` feature). This is a RUNTIME value ONLY: NO `EMBEDDING_MODELS` row carries it
    /// and it is NOT a `model = "..."` selector (#317 rework). It is the EFFECTIVE runtime computed
    /// at dispatch when a `[llm.embedding.remote]` block is present — the selected model
    /// (e.g. `minilm`) is served by Ollama instead of in-process, same model_id + dim, runtime
    /// overridden. The endpoint/auth/timeout/server-side model come from the `[remote]` config
    /// block, never the static registry.
    Ollama,
}

impl Backend {
    /// The `ai_models.runtime` column value for this backend. Stable wire string — keep in sync
    /// with the persisted manifest (`upsert_model`), never reorder/rename without a migration.
    pub fn runtime(self) -> &'static str {
        match self {
            Self::Hash => "hash",
            Self::FastEmbed => "fastembed",
            Self::Model2Vec => "model2vec",
            Self::Ollama => "ollama",
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
    /// The model's maximum input length in TOKENS (its transformer context window), or `None` for
    /// models with no sequence limit (the char-hash fallback; Model2Vec, which mean-pools token
    /// vectors with no attention). A transformer embedder SILENTLY TRUNCATES input past this —
    /// content beyond it is not represented in the vector, so a short window (all-MiniLM's
    /// 256) costs precision/recall on long code chunks. Used to cap the embedding input to
    /// something the model can actually use ([`Self::max_input_chars`]) and to warn when
    /// chunks exceed it.
    pub max_tokens: Option<usize>,
    /// The reconcile freshness key (formerly `default_model_version`). Bumping it forces a
    /// re-embed of every chunk for this model. MUST be unique across the table and never the
    /// bare `"v1"` fallback — the registry test guards both.
    pub version: &'static str,
    /// Which runtime produces this model's vectors.
    pub backend: Backend,
    /// A query-side instruction prefix for ASYMMETRIC models (queries and passages embed
    /// differently). `""` for every model shipping today — they are all symmetric, so queries
    /// embed RAW (see the BGE instruction-collapse note in `index::ai`). A future asymmetric
    /// model (e.g. CodeRankEmbed) would set this; `embed_query_with` would prepend it on the
    /// query path only.
    pub query_prefix: &'static str,
}

/// Rough UPPER-BOUND chars-per-token for deriving a char cap from a token limit. Deliberately an
/// over-estimate (English prose ≈ 4, code is denser ≈ 3) so the derived char cap never truncates
/// BEFORE the model's own token limit would — it only trims input clearly beyond what any plausible
/// tokenization could fit, saving bytes without changing the embedded content.
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;

/// The char budget rag-rat allots for `tokens` tokens of embedding input (see
/// [`CHARS_PER_TOKEN_ESTIMATE`]). Shared so a token window from any source — a model's static
/// `max_tokens` or an ollama remote's `num_ctx` override — maps to chars the same way.
pub(crate) fn tokens_to_char_budget(tokens: usize) -> usize {
    tokens.saturating_mul(CHARS_PER_TOKEN_ESTIMATE)
}

impl EmbeddingModelSpec {
    /// The embedding input length (in CHARS) worth sending this model: `max_tokens × ~4`, or `None`
    /// for a model with no sequence limit. Past this the transformer truncates anyway, so a caller
    /// caps `max_embedding_chars` to `min(configured, this)` — recall-neutral (the model ignored
    /// the rest) while saving bandwidth. NOTE: an over-estimate, so it does NOT guarantee a
    /// strict server (vLLM) won't reject a pathologically dense chunk — it only narrows the
    /// margin.
    pub fn max_input_chars(&self) -> Option<usize> {
        self.max_tokens.map(tokens_to_char_budget)
    }
}

/// Locality-sensitive hash embedder id — the dependency-free fallback tier.
pub const HASH_MODEL_ID: &str = "embedding-hash";
pub const HASH_EMBEDDING_DIM: usize = 384;

/// all-MiniLM-L6-v2 (384-dim) — the default FastEmbed general-purpose backend. The model_id is the
/// HF path (also the toml `model = "..."` selector, #317): no aliases.
pub const FASTEMBED_MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
pub const FASTEMBED_DISPLAY_MODEL: &str = "sentence-transformers/all-MiniLM-L6-v2";
pub const FASTEMBED_EMBEDDING_DIM: usize = 384;

/// BGE-small-en-v1.5 (#112): a stronger general-retrieval embedder than all-MiniLM at the SAME
/// 384-dim — switching to it is a re-embed, not a schema/dim change. MIT-licensed; ships via
/// fastembed (downloads on first use). Measured against `FASTEMBED_MODEL_ID` on the replay eval.
pub const BGE_SMALL_MODEL_ID: &str = "BAAI/bge-small-en-v1.5";
pub const BGE_SMALL_DISPLAY_MODEL: &str = "BAAI/bge-small-en-v1.5";
pub const BGE_SMALL_EMBEDDING_DIM: usize = 384;

/// jina-embeddings-v2-base-code (#112): a CODE-specific embedder — 768-dim, Apache-2.0, a built-in
/// fastembed model. SYMMETRIC: queries and code both embed RAW (no query instruction, unlike
/// CodeRankEmbed), so it slots into the raw embed path with no prefix. 768-dim → a re-embed, not a
/// schema change. Measured against all-MiniLM / BGE on the commit-replay eval before it ships as
/// the code tier.
pub const JINA_CODE_MODEL_ID: &str = "jinaai/jina-embeddings-v2-base-code";
pub const JINA_CODE_DISPLAY_MODEL: &str = "jinaai/jina-embeddings-v2-base-code";
pub const JINA_CODE_EMBEDDING_DIM: usize = 768;

/// Model2Vec static-embedding backend: a token→vector lookup + mean-pool (no transformer forward
/// pass), ~100-500× faster than FastEmbed on CPU at some retrieval-quality cost. The right choice
/// for very large repos where the FastEmbed backfill is infeasible.
pub const MODEL2VEC_MODEL_ID: &str = "minishlab/potion-retrieval-32M";
pub const MODEL2VEC_DISPLAY_MODEL: &str = "minishlab/potion-retrieval-32M";
pub const MODEL2VEC_EMBEDDING_DIM: usize = 512;

// NOTE (#317 rework): there is intentionally NO `ollama-*` registry row, alias, or const. Ollama is
// a TRANSPORT, not a model — the model selector (`model = "minilm"`) names the MODEL, and a
// `[llm.embedding.remote]` block serves THAT model via Ollama (same model_id + dim, runtime
// overridden to `Backend::Ollama` at dispatch). `Backend::Ollama` therefore never appears in this
// table; it is only ever the EFFECTIVE runtime when a remote block is present.

/// The embedding-model registry. ONE row per model — adding a model is adding a row here. Pure
/// data, no feature gates; the embedder construction is gated in `index::ai`.
pub const EMBEDDING_MODELS: &[EmbeddingModelSpec] = &[
    EmbeddingModelSpec {
        model_id: HASH_MODEL_ID,
        display: "hash",
        dim: HASH_EMBEDDING_DIM,
        // A char-level locality hash, not a transformer — no token window.
        max_tokens: None,
        version: "hash-v1",
        backend: Backend::Hash,
        query_prefix: "",
    },
    EmbeddingModelSpec {
        model_id: FASTEMBED_MODEL_ID,
        display: FASTEMBED_DISPLAY_MODEL,
        dim: FASTEMBED_EMBEDDING_DIM,
        // all-MiniLM-L6-v2's context is 256 tokens — short for code; long chunks lose their tail.
        max_tokens: Some(256),
        version: "sentence-transformers/all-MiniLM-L6-v2-v1",
        backend: Backend::FastEmbed,
        query_prefix: "",
    },
    EmbeddingModelSpec {
        model_id: BGE_SMALL_MODEL_ID,
        display: BGE_SMALL_DISPLAY_MODEL,
        dim: BGE_SMALL_EMBEDDING_DIM,
        // BGE-small-en-v1.5's context is 512 tokens.
        max_tokens: Some(512),
        version: "BAAI/bge-small-en-v1.5-v1",
        backend: Backend::FastEmbed,
        query_prefix: "",
    },
    EmbeddingModelSpec {
        model_id: JINA_CODE_MODEL_ID,
        display: JINA_CODE_DISPLAY_MODEL,
        dim: JINA_CODE_EMBEDDING_DIM,
        // jina-v2-base-code handles 8192 tokens (ALiBi) — whole code chunks fit; no tail loss.
        max_tokens: Some(8192),
        version: "jinaai/jina-embeddings-v2-base-code-v1",
        backend: Backend::FastEmbed,
        query_prefix: "",
    },
    EmbeddingModelSpec {
        model_id: MODEL2VEC_MODEL_ID,
        display: MODEL2VEC_DISPLAY_MODEL,
        dim: MODEL2VEC_EMBEDDING_DIM,
        // Model2Vec mean-pools token vectors with no attention — no sequence limit to truncate at.
        max_tokens: None,
        version: "minishlab/potion-retrieval-32M-v1",
        backend: Backend::Model2Vec,
        query_prefix: "",
    },
];

/// Look up a spec by its persisted `model_id` — which is ALSO the toml `model = "..."` selector
/// (the HF path; no aliases, #317).
pub fn spec(model_id: &str) -> Option<&'static EmbeddingModelSpec> {
    EMBEDDING_MODELS.iter().find(|s| s.model_id == model_id)
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
    fn no_registry_row_uses_the_ollama_runtime() {
        // Ollama is a TRANSPORT, not a model (#317 rework): the effective runtime is computed at
        // dispatch from the presence of a `[remote]` block, never carried by a registry row. A row
        // with `Backend::Ollama` would resurrect the removed `model = "ollama"` selector.
        for spec in EMBEDDING_MODELS {
            assert_ne!(
                spec.backend,
                Backend::Ollama,
                "{} must not use the Ollama runtime in the registry",
                spec.model_id
            );
        }
    }

    #[test]
    fn ollama_is_not_a_selectable_model_id() {
        // The removed `ollama-*` ids/aliases must not resolve — selecting Ollama is done via the
        // `[remote]` block on a real model, not a model selector.
        assert_eq!(spec("ollama"), None);
        assert_eq!(spec("ollama-all-minilm"), None);
    }

    #[test]
    fn model_id_round_trips_through_spec() {
        // The model_id IS the toml `model = "..."` selector (HF path; no aliases). Loading it back
        // must resolve the same model — `init` renders the model_id, config resolves it via `spec`.
        for s in EMBEDDING_MODELS {
            assert_eq!(spec(s.model_id).map(|x| x.model_id), Some(s.model_id));
        }
    }

    #[test]
    fn transformer_models_declare_a_token_window_static_ones_do_not() {
        // A transformer embedder (FastEmbed) has a finite context window that truncates long input;
        // the char-hash and Model2Vec (mean-pool) tiers have none. `max_input_chars` is derived iff
        // there's a token limit.
        for s in EMBEDDING_MODELS {
            match s.backend {
                Backend::FastEmbed => {
                    assert!(
                        s.max_tokens.is_some(),
                        "{} (transformer) must declare max_tokens",
                        s.model_id
                    )
                },
                Backend::Hash | Backend::Model2Vec => {
                    assert_eq!(s.max_tokens, None, "{} has no token window", s.model_id)
                },
                _ => {},
            }
            assert_eq!(s.max_input_chars().is_some(), s.max_tokens.is_some());
        }
        // The default all-MiniLM is short-context (the init help warns about this); jina-code is
        // long.
        assert_eq!(spec(FASTEMBED_MODEL_ID).unwrap().max_tokens, Some(256));
        assert_eq!(spec(JINA_CODE_MODEL_ID).unwrap().max_tokens, Some(8192));
    }

    #[test]
    fn transformer_model_ids_are_hf_paths() {
        // The FastEmbed/Model2Vec model_ids are HF identifiers (contain `/`); only the internal
        // hash fallback is a bare id. Pins the user directive that selectors are full HF
        // names.
        for s in EMBEDDING_MODELS {
            match s.backend {
                Backend::Hash => assert!(!s.model_id.contains('/'), "hash id stays bare"),
                _ => assert!(s.model_id.contains('/'), "{} should be an HF path", s.model_id),
            }
        }
    }
}
