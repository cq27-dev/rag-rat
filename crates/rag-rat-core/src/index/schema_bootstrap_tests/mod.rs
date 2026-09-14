use std::process::Command;

use rag_rat_base::config::ResolvedTarget;
// The embedding-model constants moved from `index::ai` to the crate-root registry (#112).
// Import them here so the existing `HASH_MODEL_ID` / `FASTEMBED_*` references resolve to the
// new path.
use rag_rat_base::embedding_models::{
    FASTEMBED_DISPLAY_MODEL, FASTEMBED_EMBEDDING_DIM, FASTEMBED_MODEL_ID, HASH_EMBEDDING_DIM,
    HASH_MODEL_ID,
};
use rag_rat_base::test_scratch;

use super::*;

mod support;
pub(crate) use support::poison_test_config;
use support::*;

// Unix-only: the fixture needs a file whose NAME contains `\`, which Windows forbids. Gating the
// module — rather than each item inside it — keeps its helpers from becoming `dead_code` on a
// platform where none of its tests compile in.
#[cfg(unix)]
mod backslash_file_names;
mod change_coupling;
mod chunk_store_migrations;
mod clones;
mod content_digest;
mod dir_memory_tree;
mod dispatch;
mod embedding_policy_fast_path;
mod fts_corruption;
mod generation_rebuild;
mod git_history_reload;
mod go_corpus;
mod graph_edges;
mod graph_heal_robustness;
mod head_move_carry;
mod index_paths;
mod lens_clones;
mod migration_gate_wiring;
mod multi_repo_scope;
mod orientation_healing;
mod papertrail_tests;
mod reconcile_embeddings;
mod repo_memory;
mod repo_registry;
mod schema_migrations;
mod swift_corpus;
mod symbol_search_lookup;
mod watch_placement;
mod worktree_overlay;
mod worktree_purge;
