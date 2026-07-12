//! Embedding reconcile: model-manifest lifecycle, install/activate, status/plan, and the core
//! embed loop. Split into cohesive siblings; this `mod.rs` is the curated index that the parent
//! `index::ai` module re-exports (`pub(crate) use reconcile::*`) so the crate-facing paths
//! (`ai::install_model`, `ai::reconcile_with_options_progress`, …) are unchanged.
//!
//! - [`manifest`] — model-manifest lifecycle (`ensure_model_manifest`, `model_manifest_is_current`,
//!   `remove_legacy_models`, `normalize_embedding_model_versions`, `upsert_model`).
//! - [`model_lifecycle`] — cached-model recovery + install/activate (`install_model`, `models`,
//!   `activate_model_with_version`, `recover_cached_fastembed_model*`, …).
//! - [`status`] — status/plan reads (`status`, `pending_embedding_jobs`, `reconcile_plan`,
//!   `embedding_reconcile_plan`, `last_reconcile_status`).
//! - [`embed_loop`] — the reconcile/embed orchestration loop.
//! - [`batch_write`] — embed-batch grouping, remote-scoped writes, and retry policy.
//! - [`policy_scan`] — embedding-policy skip summaries and column self-heal.

mod batch_write;
mod embed_loop;
mod manifest;
mod model_lifecycle;
mod policy_scan;
mod status;

pub(crate) use embed_loop::*;
pub(crate) use manifest::*;
pub(crate) use model_lifecycle::*;
pub(crate) use policy_scan::embedding_policy_skip_summary;
pub(crate) use status::*;
