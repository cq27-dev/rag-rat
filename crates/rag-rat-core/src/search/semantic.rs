//! Semantic embedding search is implemented through `index::ai::Embedder`.
//!
//! This module remains the search-facing semantic namespace while the runtime
//! boundary lives next to model manifests and artifact reconciliation.

pub use rag_rat_llm::providers::Embedder;
