pub mod ai;
pub mod anchors;
pub mod chunker;
pub mod edges;
pub mod git_history;
pub mod github;
pub mod ignore_rules;
pub mod oracle;
pub mod parser;
pub mod schema;
pub mod symbols;
pub mod walker;

mod discovery;
mod file_index;
mod file_rows;
mod fts;
mod git_context;
mod git_meta;
mod graph_index;
mod incremental;
mod lifecycle;
mod mem_diag;
mod meta;
mod packages;
mod parser_failures;
mod prep;
mod query_api;
mod rebuild;
mod staleness;
mod util;
pub use discovery::DiscoveryStatus;
pub(crate) use discovery::*;
pub use git_context::resolve_git_context;
pub(crate) use git_context::*;
pub(crate) use lifecycle::install_scope_view;
pub(crate) use mem_diag::{maybe_set_sqlite_soft_heap_limit, mem_trace};
pub use parser_failures::ParserFailure;
pub(crate) use prep::*;
pub use query_api::{GcReport, ImportantSymbolsRequest, OracleShaSnapshots, SearchRequest};
pub(crate) use util::*;

#[cfg(test)]
mod anchor_tests;
#[cfg(test)]
mod parser_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, thread};

use gix::bstr::{BString, ByteSlice};
use gix::status::{UntrackedFiles, tree_index};
use rayon::prelude::*;
use regex::Regex;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config::{Config, TargetKind};
use crate::index::ai::{LocalAiStatus, ModelInfo, ReconcilePlan, ReconcileReport};
use crate::index::anchors::{AnchorStatus, ChunkAnchor};
use crate::index::chunker::Chunk;
use crate::index::git_history::{
    ChunkBlameSummary, CommitSearchHit, GitHistoryIndexStatus, PathHistoryItem, QueryCommitHit,
    SymbolHistoryItem,
};
use crate::index::github::{GitHubEvidence, GitHubStatus, GitHubSyncReport, Papertrail};
use crate::index::symbols::Symbol;
use crate::language::Language;
use crate::query::graph_meta::{self, GraphMetaMode};
use crate::search::lexical::SearchHit;
use crate::storage::{IndexConnection, StorageStatus};

#[derive(Debug)]
pub struct IndexDatabase {
    storage: IndexConnection,
    pub active_commit_sha: String,
    pub active_worktree_id: String,
    /// Injected GitHub repo context. Resolved from `gh` only in `open_config` (real usage);
    /// `rebuild`/`open` leave it offline, and tests set it explicitly — so the library never
    /// shells out to `gh` during tests (#60).
    github: github::GitHubContext,
}

#[derive(Debug, Clone)]
pub enum IndexProgress {
    Started {
        database: PathBuf,
        mode: IndexMode,
    },
    Discovering,
    Discovered {
        files: usize,
    },
    PreparingFile {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    },
    IndexingFile {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    },
    IndexingGitHistory,
    RebuildingLogicalSymbols,
    ResolvingGraph,
    SyncingFts,
    RebuildingFts,
    Finished {
        files: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMode {
    Changed,
    Discover,
    Full,
}

impl IndexMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Changed => "changed files",
            Self::Discover => "discovery",
            Self::Full => "full rebuild",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct IndexStatus {
    pub database: String,
    pub exists: bool,
    pub schema: schema::SchemaStatus,
    pub git_commit: Option<String>,
    pub git_dirty: Option<bool>,
    pub indexed_at_ms: Option<i64>,
    pub content_revision: String,
    pub fts_synced_at_ms: Option<i64>,
    pub fts_source_revision: Option<String>,
    pub fts_dirty: bool,
    pub fts_fresh: bool,
    pub file_count_by_language: BTreeMap<String, u64>,
    pub parser_failures: u64,
    pub parser_failure_paths: Vec<ParserFailure>,
    pub git_history: GitHistoryIndexStatus,
    pub github: GitHubStatus,
    pub local_ai: LocalAiStatus,
    pub anchor_health: AnchorHealth,
}

/// READ-only counts of active repo-memory bindings grouped by `anchor_status`.
/// Computed by a single GROUP BY query; does not run `memory_validate` or write anything.
#[derive(Debug, Default, Serialize)]
pub struct AnchorHealth {
    pub current: u64,
    pub relocated: u64,
    pub stale: u64,
    pub gone: u64,
}

#[derive(Debug, Serialize)]
pub struct HealIndexReport {
    pub checked_files: u64,
    pub healed_files: u64,
    pub removed_files: u64,
    pub skipped_files: u64,
    pub fts_fresh: bool,
    pub message: Option<String>,
}

const MAX_AUTO_HEAL_FILES_PER_CALL: usize = 4;
// Bumped whenever the resolved graph's SHAPE changes so an upgraded index re-resolves its edges on
// next open (`ensure_graph_index_current`) instead of carrying forward stale resolutions. 7: the
// per-package + module-aware import scope (#61) — re-resolve repopulates the `packages` rows and
// re-derives the dedicated `import_scope_*` edge columns, which the V022 migration only ADDED
// (never backfilled), so without the bump a migrated index keeps empty package scopes and the
// global-fallback behavior forever. The file→package mapping itself is computed at LOAD time from
// `packages` (no persisted `files.package_id`), so the re-resolve only needs the `packages` rows.
const GRAPH_INDEX_VERSION: &str = "7";

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("Gone: indexed chunk {chunk_id} no longer exists")]
    Gone { chunk_id: i64 },
    #[error("StaleChunk: chunk {chunk_id} in {path} could not be relocated after reindex")]
    StaleChunk { chunk_id: i64, path: String },
    #[error("needs_reindex: {stale_files} stale files exceeds automatic heal cap {cap}")]
    NeedsReindex { stale_files: usize, cap: usize },
}

#[derive(Debug)]
struct FileRow {
    language: Language,
    kind: TargetKind,
}

#[derive(Debug)]
struct GraphReindexFile {
    id: i64,
    path: String,
    language: Language,
    kind: TargetKind,
}

#[derive(Debug)]
struct GraphPathRow {
    language: String,
    sha256: String,
    indexed_revision: String,
}

pub(crate) fn is_generated_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/generated-web/")
        || path.ends_with(".d.ts")
        || path.ends_with("_bg.wasm.d.ts")
}

#[derive(Debug)]
struct IndexedFile {
    path: String,
    sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexFile {
    full_path: PathBuf,
    relative_path: PathBuf,
    language: Language,
    kind: TargetKind,
    commit_sha: String,
    worktree_id: String,
}

#[derive(Debug, Clone)]
struct FileScope {
    commit_sha: String,
    worktree_id: String,
}

impl FileScope {
    fn commit(commit_sha: String) -> Self {
        Self { commit_sha, worktree_id: String::new() }
    }

    fn worktree(worktree_id: String) -> Self {
        Self { commit_sha: String::new(), worktree_id }
    }
}

#[cfg(test)]
mod schema_bootstrap_tests;
