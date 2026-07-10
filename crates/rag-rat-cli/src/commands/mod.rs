//! CLI command handlers, one cohesive module per domain. This `mod.rs` is the curated index: it
//! declares the sibling command modules and re-exports the `pub(crate)` surface `main` dispatches
//! to (and that `init` reaches via the crate-root `commands::*` glob). Add a new command's handler
//! to the module that owns its domain, then export it here — never grow this file into a handler
//! junk drawer again.

mod clones;
mod config_info;
mod consolidate;
mod format;
mod hooks;
mod index_ops;
mod memory;
mod models;
mod oracle;
mod runtime_env;
mod search;

pub(crate) use clones::{clones, clones_for};
pub(crate) use config_info::{dump_config, version_check};
pub(crate) use consolidate::consolidate;
pub(crate) use format::{output_format, set_output_format};
pub(crate) use hooks::{claude_hooks, hooks, papertrail};
pub(crate) use index_ops::{doctor, index, maintenance, reconcile};
pub(crate) use memory::{dream, memory};
pub(crate) use models::models;
#[cfg(feature = "eval")]
pub(crate) use models::{benchmark_embedding, eval};
pub(crate) use oracle::{oracle, with_oracle_write_lock};
pub(crate) use runtime_env::apply_embedding_runtime_env;
pub(crate) use search::{brief, clusters, important_symbols, query};
