//! Embedding/LLM serving layer for the rag-rat workspace: the `Embedder` trait and its
//! providers (fastembed, model2vec, OpenAI-compatible remote, hash fallback), the ephemeral
//! cookbook box provisioning contract, and provision-time throughput tuning. No index coupling:
//! the engine's selection glue decides WHICH provider runs; this crate knows how to run it.

pub mod chat;
pub mod providers;
pub mod throughput_tune;

mod cookbook;
#[cfg(feature = "fastembed")]
mod fastembed;
mod hash;
mod model2vec;
mod openai;

pub mod serving {
    //! Serving-side helpers shared by providers.

    use std::path::PathBuf;

    use sha2::{Digest, Sha256};

    fn split_identifier(value: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut previous_lower = false;
        for ch in value.chars() {
            if ch == '_' || ch == '-' {
                if !current.is_empty() {
                    parts.push(current.to_ascii_lowercase());
                    current.clear();
                }
                previous_lower = false;
                continue;
            }
            if previous_lower && ch.is_uppercase() && !current.is_empty() {
                parts.push(current.to_ascii_lowercase());
                current.clear();
            }
            previous_lower = ch.is_lowercase() || ch.is_ascii_digit();
            current.push(ch);
        }
        if !current.is_empty() {
            parts.push(current.to_ascii_lowercase());
        }
        parts
    }

    fn tokens(text: &str) -> Vec<String> {
        text.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .filter(|part| !part.is_empty())
            .flat_map(split_identifier)
            .filter(|part| part.len() > 1)
            .collect()
    }

    fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
        let digest = Sha256::digest(feature.as_bytes());
        let index = u16::from_le_bytes([digest[0], digest[1]]) as usize % vector.len();
        let sign = if digest[2] & 1 == 0 { 1.0 } else { -1.0 };
        vector[index] += sign * weight;
    }

    fn normalize(vector: &mut [f32]) {
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in vector {
                *value /= norm;
            }
        }
    }
    pub fn fastembed_cache_dir() -> PathBuf {
        if let Ok(cache) = std::env::var("RAG_RAT_MODEL_CACHE") {
            return PathBuf::from(cache);
        }
        if let Ok(cache) = std::env::var("XDG_CACHE_HOME") {
            return PathBuf::from(cache).join("rag-rat").join("models");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".cache").join("rag-rat").join("models");
        }
        // On Windows HOME/XDG_CACHE_HOME are usually unset; land the model cache per-user under
        // %LOCALAPPDATA% rather than per-checkout in the repo-relative fallback below.
        #[cfg(windows)]
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("rag-rat").join("models");
        }
        PathBuf::from(".rag-rat").join("models")
    }

    pub fn hash_embed_text(text: &str, dim: usize) -> Vec<f32> {
        let mut vector = vec![0.0_f32; dim];
        let tokens = tokens(text);
        for token in &tokens {
            add_feature(&mut vector, token, 1.0);
        }
        for pair in tokens.windows(2) {
            add_feature(&mut vector, &format!("{}::{}", pair[0], pair[1]), 0.6);
        }
        normalize(&mut vector);
        vector
    }
}

pub use providers::*;

/// Cookbook internals the engine's selection glue drives directly (provisioning lifecycle).
pub mod cookbook_internals {
    #[cfg(feature = "eval")]
    pub use crate::cookbook::provision_box_for_benchmark;
    pub use crate::cookbook::{TuneRequest, provision_and_build};
    pub use crate::openai::resolve_auth_header;
}
