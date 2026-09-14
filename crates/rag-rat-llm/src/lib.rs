//! Embedding/LLM serving layer for the rag-rat workspace: the `Embedder` trait and its
//! providers (fastembed, model2vec, OpenAI-compatible remote, hash fallback), the ephemeral
//! cookbook box provisioning contract, and provision-time throughput tuning. No index coupling:
//! the engine's selection glue decides WHICH provider runs; this crate knows how to run it.

pub mod chat;
pub mod providers;
pub mod serving;
pub mod throughput_tune;

mod cookbook;
#[cfg(feature = "fastembed")]
mod fastembed;
mod hash;
mod http;
mod model2vec;
mod openai;

// The crate-root surface: every public item of `providers`, re-exported so callers can name it
// without the module path. Three entries exist only under their cargo feature.
#[cfg(feature = "fastembed")]
pub use providers::FastEmbedEmbedder;
#[cfg(feature = "model2vec")]
pub use providers::Model2VecEmbedder;
#[cfg(feature = "eval")]
pub use providers::provision_box_for_benchmark;
pub use providers::{
    CookbookCapability, CookbookInput, CookbookProvisioner, Embedder,
    FASTEMBED_MISSING_FEATURE_MESSAGE, HashEmbedder, LIGHT_REQUEST_TIMEOUT_S, MODEL2VEC_HF_REPO,
    MODEL2VEC_MISSING_FEATURE_MESSAGE, MockEmbedder, OpenAiEmbedder, ProvisionedBox,
    abort_active_provisioning, install_provision_log_sink, sanitize_endpoint,
    verify_ephemeral_remote, verify_ephemeral_remote_cancellable,
};

/// Cookbook internals the engine's selection glue drives directly (provisioning lifecycle).
pub mod cookbook_internals {
    #[cfg(feature = "eval")]
    pub use crate::cookbook::provision_box_for_benchmark;
    pub use crate::cookbook::{ProvisionedEmbedding, TuneRequest, provision_and_build};
}
