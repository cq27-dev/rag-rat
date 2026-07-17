//! What remains of the engine-side query layer after the `rag-rat-query` extraction: the two
//! orientation surfaces still coupled to engine internals (git history, change-coupling writer).
//! Everything else — graph/impact/symbol/tree reads, repo-memory reads, pagerank — lives in the
//! `rag-rat-query` crate.

pub mod clusters;
pub mod grep_augment;
pub mod orientation;
