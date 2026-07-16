//! Embedding-provider layer: the `Embedder` trait, the backend dispatch (`embedder_for_spec`),
//! the active-model resolution (`active_embedder`), and one concrete backend per submodule.
//! This module is the single construction site for every embedder — callers reach it through the
//! curated re-exports in `index::ai`.

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
#[cfg(feature = "fastembed")]
pub use crate::fastembed::FastEmbedEmbedder;
pub use crate::hash::HashEmbedder;
pub use crate::model2vec::MODEL2VEC_HF_REPO;
#[cfg(feature = "model2vec")]
#[cfg(feature = "model2vec")]
pub use crate::model2vec::Model2VecEmbedder;
// Ungated `pub` re-export (crate-public path `crate::index::ai::providers::OpenAiEmbedder`):
// wired into `embedder_for_spec` in #317 task 5, so nothing constructs it yet. The `pub`
// visibility (same pattern as the other backends) exempts it from dead-code/unused-import
// analysis under `-D warnings` until the dispatch arm lands.
pub use crate::openai::OpenAiEmbedder;
// The tuning sweep (index::ai::throughput_tune) builds embedders at varied concurrencies.
pub(crate) use crate::openai::ProvisionedEmbedderParams;
// The shared connect-mode auth resolver — reused by the dream verdict client
// (`dream/model.rs`) so a configured-but-unresolved `auth_env` errors identically for
// embeddings and the dream model.

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

/// Acquire the CHUNK-embed embedder for a reconcile. EPHEMERAL active model: on a provisioning
/// reconcile, FIRST check for pending candidate chunks — if none, `NoEphemeralWork` (never
/// provision a paid box for zero work, #330-6); otherwise provision the cookbook box + build an
/// embedder against it (the bulk path, `provision_and_build`). On a non-provisioning pass (watcher
/// / maintenance): embed the changed chunks LOCALLY against `query_endpoint` when a probe embed on
/// it SUCCEEDS (the light/incremental path — no cold-start, single-flight, same vector space as the
/// box); `SkipEphemeral` when there is no local query server or the probe fails. CONNECT/local: the
/// usual `active_embedder`. Provisioning happens ONCE here, not per batch. `provision_remote` gates
/// the cold-start (only an explicit `rag-rat reconcile` sets it); `scan`/`options` size the
/// provision-path pending-work check.
/// Strip credentials + path from an endpoint URL before logging it: keep `scheme://host[:port]`
/// only. A debug log is a shared, greppable on-disk artifact, and an endpoint may carry inline
/// `user:pass@` userinfo (the connect/query endpoints support it — see `endpoint_is_loopback`), so
/// the raw URL must never land in a log line.
pub fn sanitize_endpoint(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    // Authority is up to the first path/query/fragment delimiter; drop any `user:pass@` userinfo.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host_port)| host_port);
    match scheme {
        Some(scheme) => format!("{scheme}://{host_port}"),
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
