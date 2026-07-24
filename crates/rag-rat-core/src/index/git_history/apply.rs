use super::blame::delete_repo_chunk_blame;
use super::prepare::{git_repo, is_fast_forward, root_key};
use super::read::read_history;
use super::*;

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
    pub(super) head: String,
    pub(super) root_key: String,
    pub(super) shallow: bool,
    pub(super) complete: bool,
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
    rag_rat_db::schema::rebuild_commit_fts(conn)?;
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
        rag_rat_db::schema::rebuild_commit_fts(conn)?;
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
    rag_rat_db::schema::rebuild_commit_fts(conn)?;
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

pub(super) fn history_cursors(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<HistoryCursors>> {
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

pub(super) fn current_history_cursors_at_or_after_prepared(
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

fn clear(conn: &Connection) -> anyhow::Result<()> {
    // Clear only the ACTIVE repo's git history (V040 scoping) — a wholesale wipe would drop a
    // sibling repo's commits in a consolidated DB. `git_chunk_blame` is transitive via
    // `chunks`/`files`, cleared through the active repo's chunks (A4). `commit_fts` is resynced
    // from the surviving `git_commits` via the #51-safe `'rebuild'` afterward.
    let repo_id = schema::active_repo_id(conn)?;
    conn.execute("DELETE FROM git_file_changes WHERE repo_id = ?1", params![repo_id])?;
    conn.execute("DELETE FROM git_commits WHERE repo_id = ?1", params![repo_id])?;
    delete_repo_chunk_blame(conn, &repo_id)?;
    rag_rat_db::schema::rebuild_commit_fts(conn)?;
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
