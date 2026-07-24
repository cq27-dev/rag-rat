use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::apply::current_history_cursors_at_or_after_prepared;
use super::read::read_history_excluding;
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
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    let repo_id = "repo";
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, 'repo', 0)",
        params![repo_id],
    )
    .unwrap();
    let repo =
        GitRepo { worktree_root: root.clone(), head: "prepared-head".to_string(), shallow: false };

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
