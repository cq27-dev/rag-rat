use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use gix::object::tree::diff::{Action, Change};
use gix::revision::walk::Sorting;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::index::{delete_repo_meta, repo_meta, schema, scoped_table_row_count, set_repo_meta};
use crate::search::lexical::SearchHit;

const GIT_HISTORY_INDEXED_HEAD_META: &str = "git_history_indexed_head";
const GIT_HISTORY_INDEXED_ROOT_META: &str = "git_history_indexed_root";
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

pub(crate) fn prepare(root: &Path) -> anyhow::Result<PreparedGitHistory> {
    let Some(repo) = git_repo(root) else {
        return Ok(PreparedGitHistory {
            repo: None,
            commits: Vec::new(),
            changes: Vec::new(),
            complete: true,
            mode: PreparedGitHistoryMode::Full,
        });
    };
    // One streaming gix revwalk pinned to the captured HEAD produces both the commit records and
    // their file changes — no `git log` subprocess and no full-history stdout buffer, so memory
    // stays bounded on deep-history repos (#212). Pinning to the captured sha (not implicit HEAD)
    // keeps `prepare` atomic w.r.t. a concurrent commit, so the stored `git_history_indexed_head`
    // stays honest for the reload gate.
    let history = read_history(root, &repo.worktree_root, &repo.head);
    Ok(PreparedGitHistory {
        repo: Some(repo),
        commits: history.commits,
        changes: history.changes,
        complete: history.complete,
        mode: PreparedGitHistoryMode::Full,
    })
}

pub(crate) fn prepare_with_plan(
    root: &Path,
    plan: GitHistoryPreparePlan,
) -> anyhow::Result<PreparedGitHistory> {
    match plan {
        GitHistoryPreparePlan::Full => prepare(root),
        GitHistoryPreparePlan::Append { expected } => prepare_append(root, expected),
    }
}

fn prepare_append(root: &Path, expected: HistoryCursors) -> anyhow::Result<PreparedGitHistory> {
    let Some(repo) = git_repo(root) else {
        return Ok(PreparedGitHistory {
            repo: None,
            commits: Vec::new(),
            changes: Vec::new(),
            complete: true,
            mode: PreparedGitHistoryMode::Full,
        });
    };
    if expected.complete
        && !repo.shallow
        && root_key(root) == expected.root_key
        && is_fast_forward(root, &expected.head, &repo.head)
        && let Ok(history) =
            read_history_excluding(root, &repo.worktree_root, &repo.head, &expected.head)
        && history.complete
    {
        return Ok(PreparedGitHistory {
            repo: Some(repo),
            commits: history.commits,
            changes: history.changes,
            complete: true,
            mode: PreparedGitHistoryMode::Append { expected },
        });
    }
    prepare(root)
}

pub(crate) fn prepare_plan(conn: &Connection, root: &Path) -> GitHistoryPreparePlan {
    let Some(repo) = git_repo(root) else {
        return GitHistoryPreparePlan::Full;
    };
    if repo.shallow {
        return GitHistoryPreparePlan::Full;
    }
    let probe = || -> anyhow::Result<GitHistoryPreparePlan> {
        let repo_id = schema::active_repo_id(conn)?;
        let expected = history_cursors(conn, &repo_id)?
            .ok_or_else(|| anyhow::anyhow!("missing git history cursor"))?;
        let has_rows = scoped_table_row_count(conn, "git_commits", &repo_id)? > 0;
        if !has_rows
            || expected.head == repo.head
            || expected.root_key != root_key(root)
            || expected.shallow
            || !is_fast_forward(root, &expected.head, &repo.head)
        {
            return Ok(GitHistoryPreparePlan::Full);
        }
        Ok(GitHistoryPreparePlan::Append { expected })
    };
    probe().unwrap_or(GitHistoryPreparePlan::Full)
}

/// The history reload-gate CURSOR values (`git_history_indexed_head`/`_root`/`_shallow` plus
/// `_complete`) a row apply produces, held back for a deferred write (A6, batch-4 P2). The git rows
/// themselves are keyed, INERT data — `is_history_current` and `status` treat the CURSORS as the
/// authority, never bare row presence — so a generation-staged rebuild lands the bulky rows early
/// (Phase 2) and writes these cursors inside the terminal flip transaction, keeping "what commit is
/// this index at" consistent with the file generation the pointer publishes. `complete=false`
/// records a best-effort partial read that may be useful to inspect but must not be used as an
/// append base. `None` cursors = a non-git root (the apply cleared the tables; nothing to record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryCursors {
    head: String,
    root_key: String,
    shallow: bool,
    complete: bool,
}

/// Write the reload-gate cursors — the LAST step of a history apply, split out so the full
/// rebuild can defer it into the terminal flip transaction while incremental/recovery paths write
/// it immediately after their rows (live-in-place semantics).
pub(crate) fn record_history_cursors(
    conn: &Connection,
    cursors: &HistoryCursors,
) -> anyhow::Result<()> {
    let repo_id = schema::active_repo_id(conn)?;
    set_repo_meta(conn, &repo_id, GIT_HISTORY_INDEXED_HEAD_META, &cursors.head)?;
    set_repo_meta(conn, &repo_id, GIT_HISTORY_INDEXED_ROOT_META, &cursors.root_key)?;
    set_repo_meta(
        conn,
        &repo_id,
        GIT_HISTORY_INDEXED_SHALLOW_META,
        if cursors.shallow { "1" } else { "0" },
    )?;
    let complete = if cursors.complete { "1" } else { "0" };
    set_repo_meta(conn, &repo_id, GIT_HISTORY_INDEXED_COMPLETE_META, complete)?;
    Ok(())
}

pub(crate) fn apply_prepared(
    conn: &Connection,
    root: &Path,
    prepared: PreparedGitHistory,
) -> anyhow::Result<GitHistoryIndexStatus> {
    let (status, cursors) = apply_prepared_deferring_cursors(conn, root, prepared)?;
    if let Some(cursors) = cursors {
        record_history_cursors(conn, &cursors)?;
    }
    Ok(status)
}

/// [`apply_prepared`] minus the cursor write: lands the `git_commits`/`git_file_changes` rows +
/// the `commit_fts` resync and RETURNS the cursors for the caller to record later — the
/// generation-staged rebuild's rows-inert/cursors-last seam (A6, batch-4 P2).
pub(crate) fn apply_prepared_deferring_cursors(
    conn: &Connection,
    root: &Path,
    prepared: PreparedGitHistory,
) -> anyhow::Result<(GitHistoryIndexStatus, Option<HistoryCursors>)> {
    let Some(repo) = prepared.repo else {
        clear(conn)?;
        return Ok((status(conn, root)?, None));
    };

    let repo_id = schema::active_repo_id(conn)?;
    let mut applied_cursors = HistoryCursors {
        head: repo.head.clone(),
        root_key: root_key(root),
        shallow: repo.shallow,
        complete: prepared.complete,
    };

    match prepared.mode {
        PreparedGitHistoryMode::Append { ref expected }
            if prepared.complete
                && expected_history_cursors_match(conn, &repo_id, expected)?
                && append_commit_hashes_absent(conn, &repo_id, &prepared.commits)? =>
        {
            append_history_rows(conn, &repo_id, &prepared.commits, prepared.changes)?;
        },
        PreparedGitHistoryMode::Append { .. } => {
            if let Some(cursors) =
                current_history_cursors_at_or_after_prepared(conn, &repo_id, root, &repo)?
            {
                return Ok((status(conn, root)?, Some(cursors)));
            }
            let Some(current_repo) = git_repo(root) else {
                clear(conn)?;
                return Ok((status(conn, root)?, None));
            };
            applied_cursors = HistoryCursors {
                head: current_repo.head.clone(),
                root_key: root_key(root),
                shallow: current_repo.shallow,
                complete: true,
            };
            let history = read_history(root, &current_repo.worktree_root, &current_repo.head);
            applied_cursors.complete = history.complete;
            replace_history_rows(conn, &repo_id, &history.commits, history.changes)?;
        },
        PreparedGitHistoryMode::Full => {
            replace_history_rows(conn, &repo_id, &prepared.commits, prepared.changes)?;
        },
    }

    // Reload-gate key: head + root + shallow flag + completeness. `is_history_current` skips the
    // next reload only when the complete cursor matches and git_commits is non-empty. Returned for
    // the caller to record — immediately (`apply_prepared`) or deferred into the rebuild's terminal
    // txn.
    Ok((status(conn, root)?, Some(applied_cursors)))
}

fn replace_history_rows(
    conn: &Connection,
    repo_id: &str,
    commits: &[CommitRecord],
    changes: Vec<FileChange>,
) -> anyhow::Result<()> {
    // `git_commits` / `git_file_changes` are direct-scoped since V040, so a history reindex of ONE
    // repo must delete and re-insert only THAT repo's rows — a wholesale wipe would drop a sibling
    // repo's commits in a consolidated DB. `git_chunk_blame` is transitive via `chunks`/`files`, so
    // it is cleared through the active repo's chunks (A4): a history reindex invalidates blame for
    // this whole repo (new commits change attribution), but must not touch a sibling repo's cache.
    // `commit_fts` is external-content over `git_commits` and is resynced below.
    conn.execute("DELETE FROM git_file_changes WHERE repo_id = ?1", params![repo_id])?;
    conn.execute("DELETE FROM git_commits WHERE repo_id = ?1", params![repo_id])?;
    delete_repo_chunk_blame(conn, repo_id)?;
    insert_history_rows(conn, repo_id, commits, changes)?;

    // Resync the external-content FTS from `git_commits` with the desync-safe `'rebuild'` (#51),
    // never a `DELETE FROM commit_fts` + manual repopulate — the reinserted commit rows took new
    // rowids, so the stored mapping is stale. `'rebuild'` re-indexes all repos' commits, keeping
    // every repo searchable.
    crate::index::schema::rebuild_commit_fts(conn)?;
    Ok(())
}

fn append_history_rows(
    conn: &Connection,
    repo_id: &str,
    commits: &[CommitRecord],
    changes: Vec<FileChange>,
) -> anyhow::Result<()> {
    if commits.is_empty() {
        debug_assert!(changes.is_empty(), "append changes must belong to appended commits");
        // The history cursor may still advance for an out-of-scope fast-forward. Since existing
        // `git_commits` rows are preserved, rebuild the external-content FTS so any prior desync
        // is not carried past the new cursor.
        crate::index::schema::rebuild_commit_fts(conn)?;
        return Ok(());
    }
    // V1 keeps blame invalidation whole-repo even on append. Scoping by touched paths is a
    // follow-up because merge commits' first-parent diff paths are used for scope but are not
    // stored as `git_file_changes` rows.
    delete_repo_chunk_blame(conn, repo_id)?;
    insert_history_rows(conn, repo_id, commits, changes)?;
    // External-content FTS can be missing or stale independently of `git_commits`. A fast-forward
    // append preserves existing rows, but still has to run the desync-safe rebuild so older
    // commits do not stay unsearchable until a later full history reload.
    crate::index::schema::rebuild_commit_fts(conn)?;
    Ok(())
}

fn append_commit_hashes_absent(
    conn: &Connection,
    repo_id: &str,
    commits: &[CommitRecord],
) -> anyhow::Result<bool> {
    let mut stmt =
        conn.prepare("SELECT EXISTS(SELECT 1 FROM git_commits WHERE repo_id = ?1 AND hash = ?2)")?;
    for commit in commits {
        let exists: bool = stmt.query_row(params![repo_id, commit.hash], |row| row.get(0))?;
        if exists {
            return Ok(false);
        }
    }
    Ok(true)
}

fn insert_history_rows(
    conn: &Connection,
    repo_id: &str,
    commits: &[CommitRecord],
    changes: Vec<FileChange>,
) -> anyhow::Result<()> {
    for commit in commits {
        conn.execute(
            "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, \
             committed_at_s, subject, body, changed_file_count, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
            params![
                commit.hash,
                commit.author_name,
                commit.author_email,
                commit.authored_at_s,
                commit.committed_at_s,
                commit.subject,
                commit.body,
                repo_id,
            ],
        )?;
    }

    let mut changed_counts = BTreeMap::<String, i64>::new();
    for change in changes {
        *changed_counts.entry(change.commit_hash.clone()).or_default() += 1;
        conn.execute(
            "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
             repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                change.commit_hash,
                change.path,
                change.additions,
                change.deletions,
                change.change_kind,
                repo_id,
            ],
        )?;
    }
    for (hash, count) in changed_counts {
        conn.execute(
            "UPDATE git_commits SET changed_file_count = ?2 WHERE hash = ?1 AND repo_id = ?3",
            params![hash, count, repo_id],
        )?;
    }
    Ok(())
}

fn raw_history_cursors(conn: &Connection, repo_id: &str) -> anyhow::Result<Option<HistoryCursors>> {
    let Some(head) = repo_meta(conn, repo_id, GIT_HISTORY_INDEXED_HEAD_META)? else {
        return Ok(None);
    };
    let Some(root_key) = repo_meta(conn, repo_id, GIT_HISTORY_INDEXED_ROOT_META)? else {
        return Ok(None);
    };
    let Some(shallow) = repo_meta(conn, repo_id, GIT_HISTORY_INDEXED_SHALLOW_META)? else {
        return Ok(None);
    };
    let shallow = match shallow.as_str() {
        "0" => false,
        "1" => true,
        _ => return Ok(None),
    };
    let Some(complete) = repo_meta(conn, repo_id, GIT_HISTORY_INDEXED_COMPLETE_META)? else {
        return Ok(None);
    };
    let complete = match complete.as_str() {
        "0" => false,
        "1" => true,
        _ => return Ok(None),
    };
    Ok(Some(HistoryCursors { head, root_key, shallow, complete }))
}

fn history_cursors(conn: &Connection, repo_id: &str) -> anyhow::Result<Option<HistoryCursors>> {
    Ok(raw_history_cursors(conn, repo_id)?.filter(|cursor| cursor.complete))
}

/// The STORED git-history freshness key for `repo_id` — the exact `(head, root_key, shallow,
/// complete)` cursor snapshot `is_history_current` keys on — serialized to a stable string. A
/// derived table computed FROM `git_commits` / `git_file_changes` (change_coupling) folds this into
/// its own freshness stamp so a history REWRITE at the same HEAD (shallow deepen, subtree re-point,
/// a best-effort reload flipping `complete`) — which moves these cursors even though HEAD does not
/// — also invalidates the derived table. `None` when any cursor meta is unset (no committed history
/// snapshot yet), which a derived table treats as an empty key.
pub(crate) fn history_freshness_key(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<String>> {
    Ok(raw_history_cursors(conn, repo_id)?.map(|cursor| {
        format!(
            "{}|{}|{}|{}",
            cursor.head, cursor.root_key, cursor.shallow as u8, cursor.complete as u8
        )
    }))
}

fn expected_history_cursors_match(
    conn: &Connection,
    repo_id: &str,
    expected: &HistoryCursors,
) -> anyhow::Result<bool> {
    let has_rows = scoped_table_row_count(conn, "git_commits", repo_id)? > 0;
    Ok(has_rows && history_cursors(conn, repo_id)?.as_ref() == Some(expected))
}

fn current_history_cursors_at_or_after_prepared(
    conn: &Connection,
    repo_id: &str,
    root: &Path,
    repo: &GitRepo,
) -> anyhow::Result<Option<HistoryCursors>> {
    if scoped_table_row_count(conn, "git_commits", repo_id)? == 0 {
        return Ok(None);
    }
    let Some(current) = history_cursors(conn, repo_id)? else {
        return Ok(None);
    };
    if current.root_key != root_key(root) || current.shallow != repo.shallow {
        return Ok(None);
    }
    if current.head == repo.head || is_fast_forward(root, &repo.head, &current.head) {
        return Ok(Some(current));
    }
    Ok(None)
}

pub fn index(conn: &Connection, root: &Path) -> anyhow::Result<GitHistoryIndexStatus> {
    let prepared = prepare(root)?;
    apply_prepared(conn, root, prepared)
}

pub fn status(conn: &Connection, root: &Path) -> anyhow::Result<GitHistoryIndexStatus> {
    let repo = git_repo(root);
    // `git_commits` / `git_file_changes` are direct-scoped since V040, so status must count only
    // THIS repo's rows — a whole-table `table_row_count` would report the union across a
    // consolidated DB.
    let repo_id = schema::active_repo_id(conn)?;
    let commit_count = scoped_table_row_count(conn, "git_commits", &repo_id)?;
    let file_change_count = scoped_table_row_count(conn, "git_file_changes", &repo_id)?;
    Ok(GitHistoryIndexStatus {
        available: repo.is_some(),
        head: repo.map(|repo| repo.head),
        indexed_head: repo_meta(conn, &repo_id, GIT_HISTORY_INDEXED_HEAD_META)?,
        commit_count,
        file_change_count,
    })
}

/// One commit-replay eval case (#120): the commit message is the QUERY and the diff's changed paths
/// are the GOLD ("the diff is the gold"). Built from the indexed `git_commits` / `git_file_changes`
/// so no working-tree checkout is needed to enumerate cases.
#[derive(Debug, Clone)]
pub struct ReplayCase {
    pub hash: String,
    pub subject: String,
    pub body: String,
    /// Paths the commit's diff touched — the recall gold for this case.
    pub changed_paths: Vec<String>,
}

/// Build commit-replay eval cases from the indexed git history, newest first. Caveats designed in
/// (#120): `max_files` drops BULK/mechanical commits (renames, formatting sweeps) whose path-recall
/// is noise; merge commits are excluded (their diff is a union, not a focused change).
pub fn replay_commit_cases(
    conn: &Connection,
    limit: u32,
    max_files: u32,
) -> anyhow::Result<Vec<ReplayCase>> {
    // Direct-scoped (V040): the eval cases come only from the ACTIVE repo's history.
    let repo_id = schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT hash, subject, body
        FROM git_commits
        WHERE changed_file_count BETWEEN 1 AND ?2
          AND subject NOT LIKE 'Merge %'
          AND repo_id = ?3
        ORDER BY authored_at_s DESC
        LIMIT ?1
        ",
    )?;
    let commits: Vec<(String, String, String)> = stmt
        .query_map(params![i64::from(limit), i64::from(max_files), repo_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut paths_stmt = conn.prepare(
        "SELECT path FROM git_file_changes WHERE commit_hash = ?1 AND repo_id = ?2 ORDER BY path",
    )?;
    let mut cases = Vec::with_capacity(commits.len());
    for (hash, subject, body) in commits {
        let changed_paths: Vec<String> = paths_stmt
            .query_map(params![hash, repo_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        if changed_paths.is_empty() {
            continue;
        }
        cases.push(ReplayCase { hash, subject, body, changed_paths });
    }
    Ok(cases)
}

/// The set of paths the repo config currently indexes (live `files` rows, excluding deletion
/// tombstones). The commit-replay eval (#120) restricts its gold to this set: a path the config
/// doesn't index (`.github/**`, `tools/**`, a root manifest, …) can never be retrieved, so counting
/// it as missing gold would make recall@k track the file mix of recent commits rather than search
/// quality (#315).
pub fn indexed_path_set(conn: &Connection) -> anyhow::Result<BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT path FROM files WHERE kind != 'deleted'")?;
    let paths = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<String>>>()?;
    Ok(paths)
}

/// Distinct `chunks.symbol_path` for chunks in `path` whose line span overlaps any of `ranges`
/// (inclusive). Commit-replay (#120) uses this to derive symbol-level gold: the symbols a commit
/// touched, in the SAME `symbol_path` format the search results carry, so symbol-recall is
/// measurable. `ranges` are PARENT-side diff line ranges, queried against a PARENT-state index.
pub fn chunk_symbol_paths_in_ranges(
    conn: &Connection,
    path: &str,
    ranges: &[(i64, i64)],
) -> anyhow::Result<Vec<String>> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT chunks.symbol_path
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        WHERE files.path = ?1
          AND chunks.symbol_path IS NOT NULL
          AND chunks.start_line <= ?3
          AND chunks.end_line >= ?2
        ",
    )?;
    let mut symbols = BTreeSet::new();
    for &(start, end) in ranges {
        let rows = stmt.query_map(params![path, start, end], |row| row.get::<_, String>(0))?;
        for symbol in rows {
            symbols.insert(symbol?);
        }
    }
    Ok(symbols.into_iter().collect())
}

pub fn commit_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<CommitSearchHit>> {
    let fts_query = fts_query(query);
    // `commit_fts` is external-content over the direct-scoped `git_commits` (V040); the MATCH runs
    // over every repo's commits, so the join predicate `git_commits.repo_id = ?` is what keeps a
    // sibling repo's commits out of THIS repo's results in a consolidated DB.
    let repo_id = schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT git_commits.hash, git_commits.author_name, git_commits.author_email,
               git_commits.authored_at_s, git_commits.committed_at_s,
               git_commits.subject, git_commits.body, git_commits.changed_file_count,
               bm25(commit_fts) AS score
        FROM commit_fts
        JOIN git_commits ON git_commits.rowid = commit_fts.rowid
        WHERE commit_fts MATCH ?1 AND git_commits.repo_id = ?3
        ORDER BY score, git_commits.authored_at_s DESC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![fts_query, i64::from(limit), repo_id], |row| {
        Ok(CommitSearchHit {
            hash: row.get(0)?,
            author_name: row.get(1)?,
            author_email: row.get(2)?,
            authored_at_s: row.get(3)?,
            committed_at_s: row.get(4)?,
            subject: row.get(5)?,
            body: row.get(6)?,
            changed_file_count: row.get(7)?,
            score: row.get(8)?,
            evidence_kind: "historical",
        })
    })?;
    let mut hits = collect_rows(rows)?;
    for (rank, hit) in hits.iter_mut().enumerate() {
        hit.score = positive_rank_score(rank);
    }
    Ok(hits)
}

fn positive_rank_score(rank: usize) -> f64 {
    crate::query::round_score(1.0 / ((rank + 1) as f64).sqrt())
}

pub fn history_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<PathHistoryItem>> {
    // `git_file_changes` / `git_commits` are direct-scoped (V040): join AND filter on `repo_id` so
    // a fork sharing a commit hash can't surface a sibling repo's change rows for this path.
    let repo_id = schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT git_commits.hash, git_file_changes.path, git_file_changes.additions,
               git_file_changes.deletions, git_file_changes.change_kind,
               git_commits.author_name, git_commits.authored_at_s, git_commits.subject
        FROM git_file_changes
        JOIN git_commits ON git_commits.hash = git_file_changes.commit_hash
                        AND git_commits.repo_id = git_file_changes.repo_id
        WHERE git_file_changes.path = ?1 AND git_file_changes.repo_id = ?3
        ORDER BY git_commits.authored_at_s DESC, git_commits.hash
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![path, i64::from(limit), repo_id], path_history_row)?;
    collect_rows(rows)
}

pub fn commits_touching_query(
    conn: &Connection,
    query: &str,
    limit: u32,
    current_hits: &[SearchHit],
) -> anyhow::Result<Vec<QueryCommitHit>> {
    let mut combined = BTreeMap::<String, QueryCommitHit>::new();
    for (rank, hit) in commit_search(conn, query, limit)?.into_iter().enumerate() {
        combined.insert(hit.hash.clone(), QueryCommitHit {
            hash: hit.hash,
            author_name: hit.author_name,
            authored_at_s: hit.authored_at_s,
            subject: hit.subject,
            changed_file_count: hit.changed_file_count,
            evidence: vec!["commit_message".to_string()],
            score: rank as f64,
            evidence_kind: "historical",
        });
    }

    let mut paths = BTreeSet::new();
    for hit in current_hits {
        paths.insert(hit.path.as_str());
    }
    for path in paths {
        for item in history_for_path(conn, path, limit)? {
            let entry = combined.entry(item.hash.clone()).or_insert_with(|| QueryCommitHit {
                hash: item.hash.clone(),
                author_name: item.author_name.clone(),
                authored_at_s: item.authored_at_s,
                subject: item.subject.clone(),
                changed_file_count: 0,
                evidence: Vec::new(),
                score: f64::from(limit),
                evidence_kind: "historical",
            });
            if !entry.evidence.iter().any(|value| value == "file_change") {
                entry.evidence.push("file_change".to_string());
            }
            entry.score -= 0.25;
        }
    }

    let mut hits = combined.into_values().collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        left.score
            .partial_cmp(&right.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.authored_at_s.cmp(&left.authored_at_s))
    });
    hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(hits)
}

pub fn cached_blame(
    conn: &Connection,
    chunk_id: i64,
    source_text_hash: &str,
) -> anyhow::Result<Option<ChunkBlameSummary>> {
    conn.query_row(
        "
        SELECT chunk_id, path, start_line, end_line, source_text_hash, line_count,
               dominant_commit, dominant_commit_lines, newest_commit, newest_commit_time_s,
               oldest_commit, oldest_commit_time_s, commit_counts_json
        FROM git_chunk_blame
        WHERE chunk_id = ?1 AND source_text_hash = ?2
        ",
        params![chunk_id, source_text_hash],
        blame_row,
    )
    .optional()
    .map_err(Into::into)
}

pub fn store_blame(conn: &Connection, summary: &ChunkBlameSummary) -> anyhow::Result<()> {
    let counts = serde_json::to_string(&summary.commit_counts)?;
    conn.execute(
        "
        INSERT INTO git_chunk_blame(
            chunk_id, source_text_hash, path, start_line, end_line, line_count,
            dominant_commit, dominant_commit_lines, newest_commit, newest_commit_time_s,
            oldest_commit, oldest_commit_time_s, commit_counts_json, computed_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(chunk_id) DO UPDATE SET
            source_text_hash = excluded.source_text_hash,
            path = excluded.path,
            start_line = excluded.start_line,
            end_line = excluded.end_line,
            line_count = excluded.line_count,
            dominant_commit = excluded.dominant_commit,
            dominant_commit_lines = excluded.dominant_commit_lines,
            newest_commit = excluded.newest_commit,
            newest_commit_time_s = excluded.newest_commit_time_s,
            oldest_commit = excluded.oldest_commit,
            oldest_commit_time_s = excluded.oldest_commit_time_s,
            commit_counts_json = excluded.commit_counts_json,
            computed_at_ms = excluded.computed_at_ms
        ",
        params![
            summary.chunk_id,
            summary.source_text_hash,
            summary.path,
            summary.start_line,
            summary.end_line,
            summary.line_count,
            summary.dominant_commit,
            summary.dominant_commit_lines,
            summary.newest_commit,
            summary.newest_commit_time_s,
            summary.oldest_commit,
            summary.oldest_commit_time_s,
            counts,
            crate::index::now_ms(),
        ],
    )?;
    Ok(())
}

pub fn blame_lines(root: &Path, path: &str, start_line: i64, end_line: i64) -> Vec<BlameLine> {
    blame_lines_via_gix(root, path, start_line, end_line).unwrap_or_default()
}

/// Blame `start_line..=end_line` (1-based) of `path` via gix, one `BlameLine` per source line in
/// order, attributing each to the commit gix's blame found and that commit's author time (#212).
fn blame_lines_via_gix(
    root: &Path,
    path: &str,
    start_line: i64,
    end_line: i64,
) -> anyhow::Result<Vec<BlameLine>> {
    let repo = crate::index::git_context::discover_repo(root)?;
    let head = repo.head_id()?.detach();
    // gix blames a path relative to the WORKTREE root; the caller's path is relative to the index
    // root, which may be a subdirectory of the worktree.
    let worktree_root = repo.workdir().unwrap_or(root);
    let absolute = root.join(path);
    let relative = absolute.strip_prefix(worktree_root).unwrap_or_else(|_| Path::new(path));

    // Blame attributes lines in HEAD's version of the file. If the file is modified in the
    // worktree, its line numbers don't align with HEAD, so a HEAD blame would
    // shift/mis-attribute the chunk's lines — and `git_blame_chunk` would cache that wrong
    // result under the dirty content hash (#213 review). Skip blame for a modified file. Use
    // gix status (filter-aware: a clean file that merely differs from its blob via
    // .gitattributes / autocrlf / LFS normalization is NOT flagged) rather than a raw byte
    // compare, which would wrongly treat such clean files as dirty (#213 review). (`git blame`
    // with no revision blamed the worktree directly, marking uncommitted lines `0000000`; gix
    // blames committed objects only, so a clean-file guard is the faithful, safe equivalent.)
    if crate::index::git_context::path_is_dirty(&repo, relative) {
        return Ok(Vec::new());
    }

    let file_path = gix::path::into_bstr(relative);
    let start = u32::try_from(start_line.max(1)).unwrap_or(1);
    let end = u32::try_from(end_line.max(start_line)).unwrap_or(start);
    let options = gix::repository::blame_file::Options {
        ranges: gix::blame::BlameRanges::from_one_based_inclusive_range(start..=end)?,
        // Follow whole-file renames, like `git blame` does by default — otherwise every unchanged
        // line in a renamed file is attributed to the rename commit instead of its original author,
        // skewing the chunk's dominant/newest/oldest commit (#213 review).
        rewrites: Some(gix::diff::Rewrites::default()),
        ..Default::default()
    };
    let outcome = repo.blame_file(file_path.as_ref(), head, options)?;
    let mut entries = outcome.entries;
    entries.sort_by_key(|entry| entry.start_in_blamed_file);
    let mut author_time: std::collections::HashMap<gix::ObjectId, Option<i64>> =
        std::collections::HashMap::new();
    let mut lines = Vec::new();
    for entry in entries {
        let commit = entry.commit_id.to_hex().to_string();
        let time = *author_time.entry(entry.commit_id).or_insert_with(|| {
            repo.find_commit(entry.commit_id)
                .ok()
                .and_then(|c| c.author().ok().map(|a| a.seconds()))
        });
        for _ in 0..entry.len.get() {
            lines.push(BlameLine { commit: commit.clone(), author_time_s: time });
        }
    }
    Ok(lines)
}

#[derive(Debug, Clone)]
pub struct BlameLine {
    pub commit: String,
    pub author_time_s: Option<i64>,
}

pub fn source_text_hash(text: &str) -> String {
    hex_sha256(text.as_bytes())
}

/// Delete the cached chunk-blame rows belonging to `repo_id`'s chunks. `git_chunk_blame` carries no
/// `repo_id` (it is transitive via `chunks` → `files`), so scope the delete through the join to
/// `main.files.repo_id` — NOT the `temp.files` scope view, which would additionally narrow to the
/// active commit/worktree and leave this repo's other-context blame behind. A git-history reindex
/// invalidates blame for the whole repo, so every chunk of the active repo is cleared.
fn delete_repo_chunk_blame(conn: &Connection, repo_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM git_chunk_blame
         WHERE chunk_id IN (
             SELECT chunks.id
             FROM chunks
             JOIN main.files ON main.files.id = chunks.file_id
             WHERE main.files.repo_id = ?1
         )",
        params![repo_id],
    )?;
    Ok(())
}

fn clear(conn: &Connection) -> anyhow::Result<()> {
    // Clear only the ACTIVE repo's git history (V040 scoping) — a wholesale wipe would drop a
    // sibling repo's commits in a consolidated DB. `git_chunk_blame` is transitive via
    // `chunks`/`files`, cleared through the active repo's chunks (A4). `commit_fts` is resynced
    // from the surviving `git_commits` via the #51-safe `'rebuild'` afterward.
    let repo_id = schema::active_repo_id(conn)?;
    conn.execute("DELETE FROM git_file_changes WHERE repo_id = ?1", params![repo_id])?;
    conn.execute("DELETE FROM git_commits WHERE repo_id = ?1", params![repo_id])?;
    delete_repo_chunk_blame(conn, &repo_id)?;
    crate::index::schema::rebuild_commit_fts(conn)?;
    // The reload-gate keys moved to `repo_meta` (V039); clear them for the active repo.
    for key in [
        GIT_HISTORY_INDEXED_HEAD_META,
        GIT_HISTORY_INDEXED_ROOT_META,
        GIT_HISTORY_INDEXED_SHALLOW_META,
        GIT_HISTORY_INDEXED_COMPLETE_META,
    ] {
        delete_repo_meta(conn, &repo_id, key)?;
    }
    Ok(())
}

/// Walk the commit history reachable from `head` with gix (gitoxide), producing the commit records
/// and their per-file changes in a SINGLE streaming pass — no `git log` subprocess and no
/// full-history stdout buffer, so memory stays bounded on deep-history repos (#212). Best-effort: a
/// repo gix can't open, or a commit it can't read, yields fewer rows rather than failing the whole
/// index, but marks the result incomplete so it cannot become an append base.
fn read_history(root: &Path, worktree_root: &Path, head: &str) -> ReadGitHistory {
    let mut commits = Vec::new();
    let mut changes = Vec::new();
    let complete = read_history_inner(root, worktree_root, head, None, &mut commits, &mut changes)
        .unwrap_or(false);
    ReadGitHistory { commits, changes, complete }
}

fn read_history_excluding(
    root: &Path,
    worktree_root: &Path,
    head: &str,
    hidden_head: &str,
) -> anyhow::Result<ReadGitHistory> {
    let mut commits = Vec::new();
    let mut changes = Vec::new();
    let complete = read_history_inner(
        root,
        worktree_root,
        head,
        Some(hidden_head),
        &mut commits,
        &mut changes,
    )?;
    Ok(ReadGitHistory { commits, changes, complete })
}

fn read_history_inner(
    root: &Path,
    worktree_root: &Path,
    head: &str,
    hidden_head: Option<&str>,
    commits: &mut Vec<CommitRecord>,
    changes: &mut Vec<FileChange>,
) -> anyhow::Result<bool> {
    let mut repo = crate::index::git_context::discover_repo(root)?;
    // The by-commit-time walk + per-commit tree diffs look up each commit/tree more than once; an
    // object cache avoids repeated zlib inflation (gitoxide's own recommendation for these passes).
    repo.object_cache_size_if_unset(16 * 1024 * 1024);
    let head_id = gix::ObjectId::from_hex(head.as_bytes())?;
    // A blob-diff resource cache for per-file line counts (`change.diff`), reused across commits
    // and cleared each commit to bound its growth.
    let mut diff_cache = repo.diff_resource_cache_for_tree_diff()?;
    // When `root` is a SUBDIRECTORY of the worktree, scope history to commits/paths under that
    // subtree — what the old `git log <head> -- .` (run from `root`) covered. `None` at the
    // worktree root keeps everything (#213 review).
    let scope = scope_prefix(root, worktree_root);
    let walk = repo
        .rev_walk([head_id])
        // Newest-first, matching `git log`'s default reverse-chronological order.
        .sorting(Sorting::ByCommitTime(gix::traverse::commit::simple::CommitTimeOrder::NewestFirst));
    let walk = match hidden_head {
        Some(hidden_head) => {
            let hidden_id = gix::ObjectId::from_hex(hidden_head.as_bytes())?;
            walk.with_hidden([hidden_id])
        },
        None => walk,
    }
    .all()?;
    for info in walk {
        // A walk error (e.g. traversing past a shallow clone's boundary into absent objects) ends
        // the walk with what we have, rather than discarding the whole history (#213 review).
        let Ok(info) = info else { return Ok(false) };
        let Ok(commit) = repo.find_commit(info.id) else { return Ok(false) };
        let author = commit.author()?;
        let committer = commit.committer()?;
        let body = commit.message_raw_sloppy().to_string();
        let record = CommitRecord {
            hash: info.id.to_hex().to_string(),
            author_name: author.name.to_string(),
            author_email: author.email.to_string(),
            authored_at_s: author.seconds(),
            committed_at_s: committer.seconds(),
            subject: commit.message()?.summary().to_string(),
            // The full raw message (git `%B`), so commit FTS covers the body, not just the subject.
            body: body.trim().to_string(),
        };
        let hash = record.hash.clone();
        let parent_ids: Vec<_> = commit.parent_ids().collect();
        let new_tree = commit.tree()?;
        // First parent's tree + whether its object is PRESENT. A missing object (shallow-clone
        // boundary) → empty tree, treated as a ROOT: it records the full diff vs the empty tree
        // (matching `git log --numstat` on a shallow clone), so even a shallow MERGE tip records
        // its files instead of looking like a zero-change merge (#213 review).
        let (parent_tree, parent_present) = match parent_ids.first() {
            Some(parent) =>
                match repo.find_commit(parent.detach()).ok().and_then(|p| p.tree().ok()) {
                    Some(tree) => (tree, true),
                    None => (repo.empty_tree(), false),
                },
            None => (repo.empty_tree(), false),
        };
        // A real MERGE (>1 parent, first parent present) records NO per-file changes — `git log
        // --numstat` has no `--diff-merges` — but its first-parent diff still decides whether the
        // commit is IN SCOPE (a merge that didn't touch `root` is omitted, like `git log -- .` /
        // `-- <subtree>`). A shallow boundary (parent absent) is a root, so it DOES record its
        // diff.
        let is_merge_with_parent = parent_ids.len() > 1 && parent_present;
        diff_cache.clear_resource_cache();
        let mut commit_changes = Vec::new();
        parent_tree
            .changes()?
            // Full paths, AND rename detection ON (gix default is off; `git log --numstat` has it ON
            // unless `--no-renames`): a `git mv` becomes a single Rewrite at the destination rather
            // than a spurious delete+add that corrupts per-path churn (#213 review).
            .options(|opts| {
                opts.track_path().track_rewrites(Some(gix::diff::Rewrites::default()));
            })
            .for_each_to_obtain_tree(&new_tree, |change| {
                push_file_change(&change, &hash, scope.as_deref(), &mut diff_cache, &mut commit_changes);
                Ok::<_, std::convert::Infallible>(Action::Continue(()))
            })?;
        // No in-scope changes vs the first parent → not part of this index's history: `git log --
        // .` / `-- <subtree>` history simplification lists NEITHER empty commits (even at
        // the repo root) NOR merges/commits that didn't touch the scope (#213 review). (A
        // root/shallow-boundary commit diffs against the empty tree, so it has changes and
        // is kept; a mode-only change also counts.)
        if commit_changes.is_empty() {
            continue;
        }
        commits.push(record);
        // A real merge's first-parent diff was used ONLY for the scope decision above — do NOT
        // record it (numstat has no merge diff). Non-merges and shallow-boundary roots
        // record their changes.
        if !is_merge_with_parent {
            changes.append(&mut commit_changes);
        }
    }
    Ok(true)
}

/// Record one tree-diff change as a `FileChange` — any leaf path change (regular blob, symlink,
/// submodule gitlink, OR a rename/copy), skipping only directory (tree) entries. Symlinks and
/// gitlinks are included because `git log --numstat` records them as changed paths, and renames are
/// recorded once at the destination (rename detection is on) (#213 review).
fn push_file_change(
    change: &Change<'_, '_, '_>,
    hash: &str,
    scope: Option<&str>,
    diff_cache: &mut gix::diff::blob::Platform,
    out: &mut Vec<FileChange>,
) {
    // Keep any leaf path change; drop only TREE entries (directory nodes the diff may emit
    // alongside their leaves). A rename/copy is recorded once at the DESTINATION (the old
    // numstat parser normalized `old => new` to the destination), with counts from the
    // rewrite's content diff (`None` for a pure 100%-similar rename) (#213 review).
    // Each arm yields (change_kind, ROOT-RELATIVE path, counts) or returns. `scope_relative` maps a
    // worktree-root-relative location to the index-root-relative path (or `None` = outside `root`).
    let (change_kind, path, additions, deletions) = match change {
        Change::Addition { location, entry_mode, .. } if !entry_mode.is_tree() => {
            let Some(path) = scope_relative(scope, &location.to_string()) else { return };
            let (additions, deletions) = blob_line_counts(change, diff_cache);
            ("added", path, additions, deletions)
        },
        Change::Deletion { location, entry_mode, .. } if !entry_mode.is_tree() => {
            let Some(path) = scope_relative(scope, &location.to_string()) else { return };
            let (additions, deletions) = blob_line_counts(change, diff_cache);
            ("deleted", path, additions, deletions)
        },
        Change::Modification { location, entry_mode, .. } if !entry_mode.is_tree() => {
            let Some(path) = scope_relative(scope, &location.to_string()) else { return };
            let (additions, deletions) = blob_line_counts(change, diff_cache);
            ("modified", path, additions, deletions)
        },
        Change::Rewrite { location, source_location, entry_mode, diff, copy, .. }
            if !entry_mode.is_tree() =>
        {
            // A rename/copy can cross the index-root boundary in a subtree index, so filter EACH
            // side by scope: both inside → one entry at the destination (a rename
            // within the subtree); source-only inside → the file LEFT the subtree,
            // recorded as a deletion; dest-only inside → it ENTERED the subtree,
            // recorded as an addition. Matches what `git log --numstat -- <subtree>`
            // reported for a boundary-crossing move (#213 review).
            let source = scope_relative(scope, &source_location.to_string());
            let destination = scope_relative(scope, &location.to_string());
            let (additions, deletions) = diff.as_ref().map_or((None, None), |stats| {
                (Some(i64::from(stats.insertions)), Some(i64::from(stats.removals)))
            });
            match (source, destination) {
                (Some(_), Some(path)) =>
                    (if *copy { "copied" } else { "renamed" }, path, additions, deletions),
                // Crossing the boundary: counts are unknown (the rewrite diff is the content delta,
                // not the full add/delete) — the path + kind are what path history needs.
                (None, Some(path)) => ("added", path, None, None),
                (Some(path), None) => ("deleted", path, None, None),
                (None, None) => return,
            }
        },
        // A tree entry, or any future variant: not a leaf path change.
        _ => return,
    };
    out.push(FileChange {
        commit_hash: hash.to_string(),
        path,
        additions,
        deletions,
        change_kind: change_kind.to_string(),
    });
}

/// Map a worktree-root-relative `location` to the index-root-relative path, or `None` if it falls
/// outside the index `root`. At the worktree root (`scope` is `None`) it's the location unchanged;
/// under a subtree index the location must start with the subtree prefix (#213 review).
fn scope_relative(scope: Option<&str>, location: &str) -> Option<String> {
    match scope {
        None => Some(location.to_string()),
        Some(prefix) => location
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(str::to_string),
    }
}

/// Line counts for a blob/symlink change via gix's blob diff. `(None, None)` for a
/// binary/uncountable diff — AND for submodule gitlinks (`EntryKind::Commit`, a commit id with no
/// text content): gix's blob diff accepts only blobs/symlinks. The old `git log --numstat`
/// synthesized `1/0`,`1/1`,`0/1` for submodule pointer changes; we record the path with unknown
/// counts rather than fall back to a raw `git` invocation. Tracked in #218.
fn blob_line_counts(
    change: &Change<'_, '_, '_>,
    diff_cache: &mut gix::diff::blob::Platform,
) -> (Option<i64>, Option<i64>) {
    change
        .diff(diff_cache)
        .ok()
        .and_then(|mut platform| platform.line_counts().ok().flatten())
        .map_or((None, None), |stats| {
            (Some(i64::from(stats.insertions)), Some(i64::from(stats.removals)))
        })
}

/// The index `root`'s path WITHIN the worktree (e.g. `Some("tools/rag-rat")`), or `None` when
/// `root` IS the worktree root (or the two can't be related). Used to scope history to the subtree
/// the old `git log -- .` (run from `root`) covered. Canonicalizes both sides so a symlink / path-
/// representation mismatch doesn't spuriously scope (or fail to scope).
fn scope_prefix(root: &Path, worktree_root: &Path) -> Option<String> {
    let root = root.canonicalize().ok()?;
    let worktree_root = worktree_root.canonicalize().ok()?;
    let relative = root.strip_prefix(&worktree_root).ok()?;
    let prefix = path_string(relative);
    (!prefix.is_empty()).then_some(prefix)
}

fn path_history_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PathHistoryItem> {
    Ok(PathHistoryItem {
        hash: row.get(0)?,
        path: row.get(1)?,
        additions: row.get(2)?,
        deletions: row.get(3)?,
        change_kind: row.get(4)?,
        author_name: row.get(5)?,
        authored_at_s: row.get(6)?,
        subject: row.get(7)?,
        evidence_kind: "historical",
    })
}

fn blame_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkBlameSummary> {
    let counts_json: String = row.get(12)?;
    let commit_counts = serde_json::from_str(&counts_json).unwrap_or_default();
    Ok(ChunkBlameSummary {
        chunk_id: row.get(0)?,
        path: row.get(1)?,
        start_line: row.get(2)?,
        end_line: row.get(3)?,
        source_text_hash: row.get(4)?,
        line_count: row.get(5)?,
        dominant_commit: row.get(6)?,
        dominant_commit_lines: row.get(7)?,
        newest_commit: row.get(8)?,
        newest_commit_time_s: row.get(9)?,
        oldest_commit: row.get(10)?,
        oldest_commit_time_s: row.get(11)?,
        commit_counts,
        evidence_kind: "historical",
    })
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The enclosing Git worktree root for `root` (the directory `git rev-parse --show-toplevel`
/// reports), or `None` when `root` is not inside a Git worktree (or `git` is unavailable). This is
/// the single place the codebase shells `--show-toplevel`; reuse it rather than adding a parallel
/// git call (the ignore matcher anchors its `.gitignore` ancestor stack here — issue #62 finding 3:
/// a `config.root` that is a subdirectory of a larger worktree must honor the worktree-root rules).
pub(crate) fn worktree_root(root: &Path) -> Option<PathBuf> {
    crate::index::git_context::discover_repo(root).ok()?.workdir().map(Path::to_path_buf)
}

fn git_repo(root: &Path) -> Option<GitRepo> {
    let repo = crate::index::git_context::discover_repo(root).ok()?;
    // `workdir()` is `None` for a bare repo — there is no worktree to index, so treat it as
    // "no git" (the previous `--show-toplevel` failed there too).
    let worktree_root = repo.workdir()?.to_path_buf();
    // `head_id()` fails on an unborn HEAD (empty repo) — same as the old `rev-parse HEAD`.
    let head = repo.head_id().ok()?.to_hex().to_string();
    let shallow = repo.is_shallow();
    Some(GitRepo { worktree_root, head, shallow })
}

fn is_fast_forward(root: &Path, old_head: &str, new_head: &str) -> bool {
    let probe = || -> anyhow::Result<bool> {
        let mut repo = crate::index::git_context::discover_repo(root)?;
        repo.object_cache_size_if_unset(16 * 1024 * 1024);
        let old_id = gix::ObjectId::from_hex(old_head.as_bytes())?;
        let new_id = gix::ObjectId::from_hex(new_head.as_bytes())?;
        let merge_base = repo.merge_base(old_id, new_id)?;
        Ok(merge_base.detach() == old_id)
    };
    probe().unwrap_or(false)
}

/// Canonical serialization of the indexed root for the reload gate. The git-history row set is
/// a function of (HEAD, root) because the `-- .` pathspec runs in `current_dir(root)`, so the
/// gate stores and compares the root alongside the head sha.
fn root_key(root: &Path) -> String {
    root.display().to_string()
}

/// O(1) gate for the per-pass git-history reload (`apply_prepared` is a full `git log` re-read +
/// table wipe — O(total history); see its rewrite-safety note). Returns true only when the
/// indexed commit/file-change rows are still valid for the current repo state, so the caller may
/// skip the reload entirely. Conservative: any uncertainty returns false (reload).
///
/// HEAD sha is content-addressed over tree+parents, so any rewrite (squash/rebase/amend/
/// force-pull) moves it and forces a reload. The two cases where HEAD alone is *not* a complete
/// key — and are guarded here — are a shallow clone being deepened (history grows without moving
/// HEAD) and the DB being re-pointed at a different `root` subtree at the same HEAD.
pub(crate) fn is_history_current(conn: &Connection, root: &Path) -> bool {
    let Some(repo) = git_repo(root) else {
        // No git repo (or git failed): let apply_prepared run its clear() path.
        return false;
    };
    if repo.shallow {
        return false;
    }
    let probe = || -> anyhow::Result<bool> {
        let repo_id = schema::active_repo_id(conn)?;
        let Some(cursors) = history_cursors(conn, &repo_id)? else {
            return Ok(false);
        };
        // Guard against a torn/empty prior reload writing the meta without rows — counted per-repo
        // (V040), so a sibling repo's commits can't mask THIS repo's empty history.
        let has_rows = scoped_table_row_count(conn, "git_commits", &repo_id)? > 0;
        Ok(cursors.head == repo.head
            && cursors.root_key == root_key(root)
            && !cursors.shallow
            && has_rows)
    };
    probe().unwrap_or(false)
}

fn fts_query(query: &str) -> String {
    let terms = query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() { "\"\"".to_string() } else { terms.join(" OR ") }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ragrat-git-history-{label}-{}-{id}", std::process::id()))
    }

    fn run_git(root: &Path, args: &[&str]) {
        let status = Command::new("git").current_dir(root).args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_output(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn insert_commit_row(conn: &Connection, repo_id: &str, hash: &str) {
        conn.execute(
            "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s,
             committed_at_s, subject, body, changed_file_count, repo_id)
             VALUES (?1, 'Test', 'test@example.com', 0, 0, 'subject', '', 0, ?2)",
            params![hash, repo_id],
        )
        .unwrap();
    }

    #[test]
    fn current_history_cursor_guard_rejects_empty_incomplete_or_wrong_scope() {
        let root = temp_root("cursor-guard");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        let repo_id = "repo";
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, 'repo', 0)",
            params![repo_id],
        )
        .unwrap();
        let repo = GitRepo {
            worktree_root: root.clone(),
            head: "prepared-head".to_string(),
            shallow: false,
        };

        assert!(
            current_history_cursors_at_or_after_prepared(&conn, repo_id, &root, &repo)
                .unwrap()
                .is_none(),
            "empty history rows are never current"
        );

        insert_commit_row(&conn, repo_id, "existing-head");
        assert!(
            current_history_cursors_at_or_after_prepared(&conn, repo_id, &root, &repo)
                .unwrap()
                .is_none(),
            "rows without a complete cursor are never current"
        );

        set_repo_meta(&conn, repo_id, GIT_HISTORY_INDEXED_HEAD_META, "existing-head").unwrap();
        set_repo_meta(&conn, repo_id, GIT_HISTORY_INDEXED_ROOT_META, "other-root").unwrap();
        set_repo_meta(&conn, repo_id, GIT_HISTORY_INDEXED_SHALLOW_META, "0").unwrap();
        set_repo_meta(&conn, repo_id, GIT_HISTORY_INDEXED_COMPLETE_META, "1").unwrap();
        assert!(
            current_history_cursors_at_or_after_prepared(&conn, repo_id, &root, &repo)
                .unwrap()
                .is_none(),
            "a cursor from another root cannot satisfy a stale prepared append"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_history_excluding_reports_invalid_hidden_head() {
        let root = temp_root("invalid-hidden-head");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init"]);
        run_git(&root, &["config", "user.name", "Rag Rat"]);
        run_git(&root, &["config", "user.email", "rag@example.com"]);
        fs::write(root.join("README.md"), "tracked\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "Initial"]);
        let head = git_output(&root, &["rev-parse", "HEAD"]);

        let err = read_history_excluding(&root, &root, &head, "not-a-git-object")
            .expect_err("invalid hidden head must be reported");
        assert!(!err.to_string().is_empty());

        let _ = fs::remove_dir_all(root);
    }
}
