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
mod file_lifecycle;
mod fts;
mod git_context;
mod git_meta;
mod graph_index;
mod incremental;
mod lifecycle;
mod meta;
mod packages;
mod parser_failures;
mod prep;
mod query_api;
mod rebuild;
mod staleness;
mod text_compare;
mod util;
pub(crate) use discovery::*;
pub use git_context::resolve_git_context;
pub(crate) use git_context::*;
pub(crate) use lifecycle::install_scope_view;
pub(crate) use prep::*;
pub use query_api::ImportantSymbolsRequest;
pub(crate) use text_compare::*;
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
use crate::search::lexical::{SearchHit, SearchOptions};
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

#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    pub files_pruned: u64,
    pub chunks_pruned: u64,
    pub files_remaining: u64,
    pub chunks_remaining: u64,
    /// True when no live context could be determined and pruning was skipped (nothing deleted).
    pub skipped: bool,
}

#[derive(Debug, Serialize)]
pub struct ParserFailure {
    pub path: String,
    pub language: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryStatus {
    pub discovered_files: usize,
    pub indexed_files: usize,
    pub unindexed_files: usize,
    pub unindexed_source_files: usize,
    pub changed_indexed_files: usize,
    pub removed_indexed_files: usize,
    pub unindexed_sample: Vec<String>,
    pub warning: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LogicalSymbolKey {
    language: String,
    path: String,
    name: String,
    qualified_name: String,
    kind: String,
    // Signature is part of the identity so that two distinct same-named symbols in one file (e.g.
    // `new` on two different impls — same `qualified_name`, different signatures) do NOT collapse
    // into one logical symbol. Genuine cfg variants share a signature, so they still group.
    signature: Option<String>,
}

impl LogicalSymbolKey {
    fn from(row: &LogicalSymbolMemberRow) -> Self {
        Self {
            language: row.language.clone(),
            path: row.path.clone(),
            name: row.name.clone(),
            qualified_name: row.qualified_name.clone(),
            kind: row.kind.clone(),
            signature: row.signature.clone(),
        }
    }

    /// Deterministic logical-symbol id derived from the key, so it is **stable across reindex**
    /// (the table is fully rebuilt each pass; an autoincrement rowid would churn the id every
    /// time, breaking any cached id or logical-symbol-bound memory). A 63-bit truncation of the
    /// key's SHA-256 — collisions are astronomically unlikely across a repo's symbols, and a
    /// collision would surface as a loud primary-key error on rebuild rather than silent merging.
    fn stable_id(&self) -> i64 {
        let canonical = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.language,
            self.path,
            self.name,
            self.qualified_name,
            self.kind,
            self.signature.as_deref().unwrap_or(""),
        );
        let digest = Sha256::digest(canonical.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) >> 1) as i64
    }
}

#[derive(Debug, Clone)]
struct LogicalSymbolMemberRow {
    symbol_id: i64,
    path: String,
    language: String,
    name: String,
    qualified_name: String,
    kind: String,
    signature: Option<String>,
    start_line: i64,
    end_line: i64,
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

/// Diagnostic: when `RAG_RAT_SQLITE_SOFT_HEAP_LIMIT_MB` is set, cap SQLite's soft heap limit
/// (process-wide) for this run. Distinguishes a *discretionary* SQLite buffer spike (clamps with
/// little wall cost — the buffer just flushes earlier) from a *load-bearing* allocation (wall
/// blows up under the cap because the work genuinely needs the memory). No-op unless set.
pub(crate) fn maybe_set_sqlite_soft_heap_limit() {
    let Ok(raw) = std::env::var("RAG_RAT_SQLITE_SOFT_HEAP_LIMIT_MB") else {
        return;
    };
    let Ok(mb) = raw.trim().parse::<i64>() else {
        return;
    };
    if mb <= 0 {
        return;
    }
    let bytes = mb.saturating_mul(1024 * 1024);
    // SAFETY: sqlite3_soft_heap_limit64 is a thread-safe configuration accessor with no
    // preconditions; it returns the prior limit.
    let prev = unsafe { rusqlite::ffi::sqlite3_soft_heap_limit64(bytes) };
    eprintln!("MEMTRACE soft_heap_limit set to {mb} MB (was {} MB)", prev / 1024 / 1024);
}

/// Diagnostic peak-RSS probe: when `RAG_RAT_MEM_TRACE` is set, print process resident set
/// (`/proc/self/status` VmRSS) and SQLite's outstanding allocation (`sqlite3_memory_used`,
/// process-wide) at a labelled point. The two together localize a full-rebuild memory spike to a
/// phase AND attribute it to Rust heap vs SQLite. Off by default — a single env check, zero cost in
/// normal runs; it only reads counters and prints to stderr, so it never affects index output.
pub(crate) fn mem_trace(label: &str) {
    // Off unless the env var is set to a truthy value — empty / "0" / "false" count as off so a
    // workflow that always passes the var (possibly empty) doesn't trace by accident.
    match std::env::var("RAG_RAT_MEM_TRACE").as_deref() {
        Ok("" | "0" | "false") | Err(_) => return,
        Ok(_) => {},
    }
    let vmrss_kb = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
        })
        .and_then(|kb| kb.parse::<i64>().ok())
        .unwrap_or(0);
    // SAFETY: both are thread-safe libsqlite3 accessors with no preconditions. `highwater(1)`
    // returns the peak SQLite allocation since the previous probe and RESETS it, so each line's
    // `sqlite_peak` is the high-water of the phase that just ran — this catches an in-statement
    // spike (e.g. an FTS5 'rebuild') that has already freed by the time we read `used` at the
    // boundary. A high `sqlite_peak` with flat `rss` ⇒ SQLite-allocator; a high `rss` jump with
    // flat `sqlite_peak` ⇒ the spike is outside SQLite's allocator (glibc arena / temp / mmap).
    let sqlite_bytes = unsafe { rusqlite::ffi::sqlite3_memory_used() };
    let sqlite_peak = unsafe { rusqlite::ffi::sqlite3_memory_highwater(1) };
    // Elapsed since the first probe — phase deltas reveal which trailing phase is the long one that
    // the 1 s sampler's spike window falls inside.
    static FIRST: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let elapsed = FIRST.get_or_init(std::time::Instant::now).elapsed().as_secs_f64();
    eprintln!(
        "MEMTRACE {label}: t+{elapsed:.0}s rss={:.2}GB sqlite={:.2}GB sqlite_peak={:.2}GB",
        vmrss_kb as f64 / 1024.0 / 1024.0,
        sqlite_bytes as f64 / 1_073_741_824.0,
        sqlite_peak as f64 / 1_073_741_824.0,
    );
}

#[cfg(test)]
mod schema_bootstrap_tests;
