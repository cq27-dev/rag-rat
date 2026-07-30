use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use gix::object::tree::diff::{Action, Change};
use gix::revision::walk::Sorting;
use rag_rat_base::hash::hex_sha256;
use rag_rat_db::meta::{
    bump_lens_enrichment_revision, delete_repo_meta, repo_meta, scoped_table_row_count,
    set_repo_meta,
};
use rag_rat_db::schema;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::search::lexical::SearchHit;

mod apply;
mod blame;
mod prepare;
mod query;
mod read;

pub(crate) use apply::{
    HistoryCursors, apply_prepared, apply_prepared_deferring_cursors, history_freshness_key,
    record_history_cursors,
};
pub use blame::{BlameLine, cached_blame, source_text_hash, store_blame};
pub(crate) use prepare::{
    is_history_current, prepare, prepare_plan, prepare_with_plan, worktree_root,
};
pub use query::{
    ReplayCase, chunk_symbol_paths_in_ranges, commit_search, commits_touching_query,
    history_for_path, indexed_path_set, replay_commit_cases,
};

const GIT_HISTORY_INDEXED_HEAD_META: &str = "git_history_indexed_head";
/// The one cursor of the four whose value is a PATH (`root_key` = `config.root`'s spelling), so the
/// V096 path-spelling rekey has to rewrite it. Declared in `rag-rat-db` and referenced here rather
/// than spelled twice — the migration lives below this crate and a drifted literal there is a
/// silent miss, not a compile error.
const GIT_HISTORY_INDEXED_ROOT_META: &str = rag_rat_db::meta::GIT_HISTORY_INDEXED_ROOT_META;
const GIT_HISTORY_INDEXED_SHALLOW_META: &str = "git_history_indexed_shallow";
const GIT_HISTORY_INDEXED_COMPLETE_META: &str = "git_history_indexed_complete";

#[derive(Debug, Clone, Serialize)]
pub struct GitHistoryIndexStatus {
    pub available: bool,
    pub head: Option<String>,
    pub indexed_head: Option<String>,
    pub commit_count: u64,
    pub file_change_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitSearchHit {
    pub hash: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at_s: i64,
    pub committed_at_s: i64,
    pub subject: String,
    pub body: String,
    pub changed_file_count: i64,
    pub score: f64,
    pub evidence_kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathHistoryItem {
    pub hash: String,
    pub path: String,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub change_kind: String,
    pub author_name: String,
    pub authored_at_s: i64,
    pub subject: String,
    pub evidence_kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolHistoryItem {
    pub symbol: String,
    pub qualified_name: String,
    pub path: String,
    pub start_byte: i64,
    pub end_byte: i64,
    pub commit: PathHistoryItem,
    pub evidence_kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryCommitHit {
    pub hash: String,
    pub author_name: String,
    pub authored_at_s: i64,
    pub subject: String,
    pub changed_file_count: i64,
    pub evidence: Vec<String>,
    pub score: f64,
    pub evidence_kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkBlameSummary {
    pub chunk_id: i64,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub source_text_hash: String,
    pub line_count: i64,
    pub dominant_commit: Option<String>,
    pub dominant_commit_lines: i64,
    pub newest_commit: Option<String>,
    pub newest_commit_time_s: Option<i64>,
    pub oldest_commit: Option<String>,
    pub oldest_commit_time_s: Option<i64>,
    pub commit_counts: BTreeMap<String, i64>,
    pub evidence_kind: &'static str,
}

#[derive(Debug)]
struct GitRepo {
    worktree_root: PathBuf,
    head: String,
    /// True for a `--depth`-limited clone. A shallow clone can be deepened/unshallowed
    /// without moving HEAD, so its reachable history is not pinned by the HEAD sha — the
    /// reload gate must never skip while shallow. See [`is_history_current`].
    shallow: bool,
}

#[derive(Debug)]
struct CommitRecord {
    hash: String,
    author_name: String,
    author_email: String,
    authored_at_s: i64,
    committed_at_s: i64,
    subject: String,
    body: String,
}

#[derive(Debug)]
struct FileChange {
    commit_hash: String,
    path: String,
    additions: Option<i64>,
    deletions: Option<i64>,
    change_kind: String,
}

#[derive(Debug)]
struct ReadGitHistory {
    commits: Vec<CommitRecord>,
    changes: Vec<FileChange>,
    complete: bool,
}

#[derive(Debug)]
pub(crate) struct PreparedGitHistory {
    repo: Option<GitRepo>,
    commits: Vec<CommitRecord>,
    changes: Vec<FileChange>,
    complete: bool,
    mode: PreparedGitHistoryMode,
}

#[derive(Debug)]
enum PreparedGitHistoryMode {
    Full,
    Append { expected: HistoryCursors },
}

#[derive(Debug, Clone)]
pub(crate) enum GitHistoryPreparePlan {
    Full,
    Append { expected: HistoryCursors },
}

pub use apply::{index, status};
pub use blame::blame_lines;

#[cfg(test)]
mod tests;
