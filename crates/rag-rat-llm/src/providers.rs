//! Embedding-provider surface: the `Embedder` trait, the `MockEmbedder`, endpoint log
//! sanitization, the missing-feature messages, and re-exports of every concrete backend plus the
//! ephemeral cookbook seams. The backend dispatch (`embedder_for_spec`) and active-model
//! resolution (`active_embedder`) live in rag-rat-core's `index::ai::embedder_select`.

// Ungated: the ephemeral cookbook lifecycle (#318) spawns a subprocess via std::process — no heavy
// optional dependency, so it ships unconditionally like the Ollama backend.

// EVAL-ONLY `pub` seam (#346): the `benchmark-embedding` subcommand (a separate crate)
// provisions an ephemeral box and runs its own measured sweep against it; re-exported `pub`
// under `eval` so it reaches the CLI through `index::ai`.
#[cfg(feature = "eval")]
pub use crate::cookbook::provision_box_for_benchmark;
// `verify_ephemeral_remote` is the `pub` init-wizard seam (the CLI's Remote step calls it);
// the underlying `provision_and_build` stays `pub(crate)`.
pub use crate::cookbook::{
    CookbookInput, CookbookProvisioner, ProvisionedBox, abort_active_provisioning,
    install_provision_log_sink, verify_ephemeral_remote, verify_ephemeral_remote_cancellable,
};
#[cfg(feature = "fastembed")]
pub use crate::fastembed::FastEmbedEmbedder;
pub use crate::hash::HashEmbedder;
pub use crate::model2vec::MODEL2VEC_HF_REPO;
#[cfg(feature = "model2vec")]
pub use crate::model2vec::Model2VecEmbedder;
pub use crate::openai::OpenAiEmbedder;
// The tuning sweep (index::ai::throughput_tune) builds embedders at varied concurrencies.
pub(crate) use crate::openai::ProvisionedEmbedderParams;

pub const MODEL2VEC_MISSING_FEATURE_MESSAGE: &str =
    "Model2Vec backend requested, but this binary was built without Model2Vec support.\nRebuild \
     with default features enabled:\n  cargo install rag-rat";
pub const FASTEMBED_MISSING_FEATURE_MESSAGE: &str =
    "FastEmbed backend requested, but this binary was built without default FastEmbed \
     support.\nRebuild with default features enabled:\n  cargo install rag-rat";

pub trait Embedder {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

/// Per-request timeout ceiling for the LIGHT (local `query_endpoint`) path — bounds both the route
/// probe and each single-flight incremental embed so a slow/hung local server can't stall a watcher
/// pass. The provisioned-box path is unaffected (it keeps the configured timeout).
pub const LIGHT_REQUEST_TIMEOUT_S: u64 = 30;

/// Strip credentials + path from an endpoint URL before logging it: keep `scheme://host[:port]`
/// only. A debug log is a shared, greppable on-disk artifact, and an endpoint may carry inline
/// `user:pass@` userinfo (the connect/query endpoints support it — see `endpoint_is_loopback`), so
/// the raw URL must never land in a log line.
pub fn sanitize_endpoint(url: &str) -> String {
    let host_port = crate::openai::url_authority(url);
    match url.split_once("://") {
        Some((scheme, _)) => format!("{scheme}://{host_port}"),
        None => host_port.to_string(),
    }
}

pub struct MockEmbedder {
    model_id: String,
    dim: usize,
}

impl MockEmbedder {
    pub fn new(model_id: impl Into<String>, dim: usize) -> Self {
        Self { model_id: model_id.into(), dim }
    }
}

impl Embedder for MockEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| crate::serving::hash_embed_text(text, self.dim)).collect())
    }
}
