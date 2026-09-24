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
#[derive(Clone, Copy, PartialEq, Eq, Debug, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
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
    /// The `ai_models.runtime` column value for this backend. Stable wire string — never rename a
    /// variant without a migration.
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// The exact inverse of [`Self::as_db_str`]; `None` for any other stored text.
    pub fn from_db_str(token: &str) -> Option<Self> {
        token.parse().ok()
    }
}

/// One row in the embedding-model registry: everything the rest of the codebase needs to know about
/// a model, in one place. See the module docs for why this is the single source of truth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EmbeddingModelSpec {
    /// The persisted `ai_models.model_id` — the stable identity used everywhere in the index.
    pub model_id: &'static str,
    /// Human-facing name for status output when it differs from [`Self::model_id`] (only the hash
    /// tier's). Read through [`Self::display`].
    pub display_name: Option<&'static str>,
    /// Embedding dimension. A change here is a re-embed (new vectors), not a schema migration.
    pub dim: usize,
    /// The model's maximum input length in TOKENS (its transformer context window), or `None` for
    /// models with no sequence limit (the char-hash fallback; Model2Vec, which mean-pools token
    /// vectors with no attention). A transformer embedder SILENTLY TRUNCATES input past this —
    /// content beyond it is not represented in the vector, so a short window (all-MiniLM's
    /// 256) costs precision/recall on long code chunks. Drives [`Self::max_input_chars`], which
    /// `rag-rat init` + `models install` use to WARN when a short-context model is picked for a
    /// code repo (steering to a long-context model like jina-code).
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
    /// Cosine similarity, in thousandths, at which `memory_create` reports an existing memory as a
    /// possible restatement of the new one, or `None` to skip that check. Cosine scales differ per
    /// model — a threshold that separates restatements from merely related notes on one model
    /// floods or goes silent on another — so only a model measured on a real memory set has one.
    pub near_duplicate_permille: Option<u16>,
}

/// Rough UPPER-BOUND chars-per-token for deriving a char cap from a token limit. Deliberately an
/// over-estimate (English prose ≈ 4, code is denser ≈ 3) so the derived char cap never truncates
/// BEFORE the model's own token limit would — it only trims input clearly beyond what any plausible
/// tokenization could fit, saving bytes without changing the embedded content.
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;

impl EmbeddingModelSpec {
    /// Human-facing model name for status output: the upstream HF path, which is the model id,
    /// unless the row names another.
    pub fn display(&self) -> &'static str {
        self.display_name.unwrap_or(self.model_id)
    }

    /// The embedding input length (in CHARS) this model can actually use: `max_tokens × ~4`, or
    /// `None` for a model with no sequence limit. A rough over-estimate (see
    /// [`CHARS_PER_TOKEN_ESTIMATE`]) used to judge whether a model is "short-context" for typical
    /// chunks — `rag-rat init` + `models install` warn when this falls below the default
    /// chunk-embed budget, steering code repos to a long-context model. NOT used to truncate
    /// reconcile input: the model truncates past its own window anyway, and forcing a smaller
    /// cap there interacts badly with the `SkipTooLarge` policy threshold (`max_embedding_chars
    /// × 4`) and an ollama `num_ctx` override.
    pub fn max_input_chars(&self) -> Option<usize> {
        self.max_tokens.map(|t| t.saturating_mul(CHARS_PER_TOKEN_ESTIMATE))
    }
}

/// Locality-sensitive hash embedder id — the dependency-free fallback tier.
pub const HASH_MODEL_ID: &str = "embedding-hash";
pub const HASH_EMBEDDING_DIM: usize = 384;

/// all-MiniLM-L6-v2 (384-dim) — the default FastEmbed general-purpose backend. The model_id is the
/// HF path (also the toml `model = "..."` selector, #317): no aliases.
pub const FASTEMBED_MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
pub const FASTEMBED_DISPLAY_MODEL: &str = FASTEMBED_MODEL_ID;
pub const FASTEMBED_EMBEDDING_DIM: usize = 384;

/// BGE-small-en-v1.5 (#112): a stronger general-retrieval embedder than all-MiniLM at the SAME
/// 384-dim — switching to it is a re-embed, not a schema/dim change. MIT-licensed; ships via
/// fastembed (downloads on first use). Measured against `FASTEMBED_MODEL_ID` on the replay eval.
pub const BGE_SMALL_MODEL_ID: &str = "BAAI/bge-small-en-v1.5";

/// jina-embeddings-v2-base-code (#112): a CODE-specific embedder — 768-dim, Apache-2.0, a built-in
/// fastembed model. SYMMETRIC: queries and code both embed RAW (no query instruction, unlike
/// CodeRankEmbed), so it slots into the raw embed path with no prefix. 768-dim → a re-embed, not a
/// schema change. Measured against all-MiniLM / BGE on the commit-replay eval before it ships as
/// the code tier.
pub const JINA_CODE_MODEL_ID: &str = "jinaai/jina-embeddings-v2-base-code";

/// Model2Vec static-embedding backend: a token→vector lookup + mean-pool (no transformer forward
/// pass), ~100-500× faster than FastEmbed on CPU at some retrieval-quality cost. The right choice
/// for very large repos where the FastEmbed backfill is infeasible.
pub const MODEL2VEC_MODEL_ID: &str = "minishlab/potion-retrieval-32M";
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
        display_name: Some("hash"),
        dim: HASH_EMBEDDING_DIM,
        // A char-level locality hash, not a transformer — no token window.
        max_tokens: None,
        version: "hash-v1",
        backend: Backend::Hash,
        query_prefix: "",
        // Token-overlap vectors: 0.86 flags only near-verbatim restatements.
        near_duplicate_permille: Some(860),
    },
    EmbeddingModelSpec {
        model_id: FASTEMBED_MODEL_ID,
        display_name: None,
        dim: FASTEMBED_EMBEDDING_DIM,
        // all-MiniLM-L6-v2's context is 256 tokens — short for code; long chunks lose their tail.
        max_tokens: Some(256),
        version: "sentence-transformers/all-MiniLM-L6-v2-v1",
        backend: Backend::FastEmbed,
        query_prefix: "",
        near_duplicate_permille: None,
    },
    EmbeddingModelSpec {
        model_id: BGE_SMALL_MODEL_ID,
        display_name: None,
        dim: 384,
        // BGE-small-en-v1.5's context is 512 tokens.
        max_tokens: Some(512),
        version: "BAAI/bge-small-en-v1.5-v1",
        backend: Backend::FastEmbed,
        query_prefix: "",
        near_duplicate_permille: None,
    },
    EmbeddingModelSpec {
        model_id: JINA_CODE_MODEL_ID,
        display_name: None,
        dim: 768,
        // jina-v2-base-code handles 8192 tokens (ALiBi) — whole code chunks fit; no tail loss.
        max_tokens: Some(8192),
        version: "jinaai/jina-embeddings-v2-base-code-v1",
        backend: Backend::FastEmbed,
        query_prefix: "",
        // Measured on 816 memories: the closest pair scored 0.908, restatements of one rule
        // 0.865-0.91, and 0.86 flagged 13 pairs.
        near_duplicate_permille: Some(860),
    },
    EmbeddingModelSpec {
        model_id: MODEL2VEC_MODEL_ID,
        display_name: None,
        dim: MODEL2VEC_EMBEDDING_DIM,
        // Model2Vec mean-pools token vectors with no attention — no sequence limit to truncate at.
        max_tokens: None,
        version: "minishlab/potion-retrieval-32M-v1",
        backend: Backend::Model2Vec,
        query_prefix: "",
        near_duplicate_permille: None,
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

    /// `ai_models.runtime` holds these tokens: pinned byte-for-byte, and the DB side is exact.
    #[test]
    fn runtime_tokens_are_pinned_and_round_trip() {
        for (backend, token) in [
            (Backend::Hash, "hash"),
            (Backend::FastEmbed, "fastembed"),
            (Backend::Model2Vec, "model2vec"),
            (Backend::Ollama, "ollama"),
        ] {
            assert_eq!(backend.as_db_str(), token);
            assert_eq!(Backend::from_db_str(token), Some(backend));
        }
        assert_eq!(Backend::from_db_str("FastEmbed"), None);
    }

    /// Status output shows the HF path for every model but the hash tier.
    #[test]
    fn display_is_the_model_id_except_for_the_hash_tier() {
        for s in EMBEDDING_MODELS {
            let expected = if s.backend == Backend::Hash { "hash" } else { s.model_id };
            assert_eq!(s.display(), expected);
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
    fn near_duplicate_thresholds_are_cosines() {
        // Thousandths of a cosine: a value past 1000 (a slipped digit) could never be reached and
        // would silently switch the `memory_create` near-duplicate warning off.
        for s in EMBEDDING_MODELS {
            assert!(s.near_duplicate_permille.is_none_or(|p| p <= 1000), "{}", s.model_id);
        }
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
                Backend::Ollama => unreachable!("no registry row is served by Ollama"),
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
                Backend::FastEmbed | Backend::Model2Vec | Backend::Ollama => {
                    assert!(s.model_id.contains('/'), "{} should be an HF path", s.model_id)
                },
            }
        }
    }
}
