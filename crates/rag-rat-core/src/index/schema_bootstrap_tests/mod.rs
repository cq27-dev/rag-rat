use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::config::ResolvedTarget;
// The embedding-model constants moved from `index::ai` to the crate-root registry (#112).
// Import them here so the existing `HASH_MODEL_ID` / `FASTEMBED_*` references resolve to the
// new path.
use crate::embedding_models::{
    FASTEMBED_DISPLAY_MODEL, FASTEMBED_EMBEDDING_DIM, FASTEMBED_MODEL_ID, HASH_EMBEDDING_DIM,
    HASH_MODEL_ID,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A recognizable bogus row that only a git-history full replacement would remove. Surviving the
/// next incremental pass means the reload was skipped or appended; gone means the full path ran. It
/// lives in `git_file_changes` (a plain table the full path wipes) rather than `git_commits`,
/// because a stray `git_commits` row with no matching `commit_fts` entry desyncs the
/// external-content FTS5 and the reload's `commit_fts` rebuild then has to repair it — a test
/// artifact, not a real state.
const SENTINEL_PATH: &str = "__rag_rat_reload_sentinel__";

fn insert_sentinel_commit(db: &IndexDatabase) {
    let conn = db.storage.connection();
    // git_file_changes has a COMPOSITE FK `(repo_id, commit_hash) → git_commits(repo_id, hash)`
    // (V040), so reuse a real commit's BOTH keys; the sentinel marker is the path, which the reload
    // wipes along with every other change row.
    let (hash, repo_id): (String, String) = conn
        .query_row("SELECT hash, repo_id FROM git_commits LIMIT 1", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
         repo_id)
         VALUES (?1, ?2, 0, 0, 'modified', ?3)",
        rusqlite::params![hash, SENTINEL_PATH, repo_id],
    )
    .unwrap();
}

fn sentinel_commit_count(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM git_file_changes WHERE path = ?1",
            [SENTINEL_PATH],
            |row| row.get(0),
        )
        .unwrap()
}

fn git_history_targets() -> Vec<ResolvedTarget> {
    vec![
        ResolvedTarget {
            name: "markdown".to_string(),
            language: Language::Markdown,
            directories: vec![PathBuf::from("docs")],
            include: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Docs,
        },
        ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        },
    ]
}

fn rag_rat_config(root: &Path) -> Config {
    Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.to_path_buf(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: git_history_targets(),
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}

/// Init a git repo at `root` with two commits over docs/ + src/, returning its rag-rat Config.
fn git_history_test_config(root: &Path) -> Config {
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    run_git(root, &["init"]);
    run_git(root, &["config", "user.name", "Rag Rat"]);
    run_git(root, &["config", "user.email", "rag@example.com"]);
    fs::write(root.join("docs/search.md"), "# Title\nalpha token\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn tracked_symbol() {}\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-m", "Add alpha docs"]);
    fs::write(root.join("docs/search.md"), "# Title\nbeta token\n").unwrap();
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-m", "Refresh beta docs"]);
    rag_rat_config(root)
}

fn read_meta(db: &IndexDatabase, key: &str) -> Option<String> {
    // Reads the per-repo `repo_meta` (V039 relocated the singleton keys there); the only caller
    // reads `indexed_at_ms`, which moved.
    db.repo_meta(key).unwrap()
}

/// The callee identifier byte range (#67) stored on the single edge matching
/// `from LIKE %from% AND to_name LIKE %to% AND edge_kind`, as `Option<(start, end)>`.
/// `None` means the column is NULL (no callee range recorded).
fn callee_byte_range(
    db: &IndexDatabase,
    from: &str,
    to: &str,
    edge_kind: &str,
) -> Option<(i64, i64)> {
    let (start, end) = db
        .storage
        .connection()
        .query_row(
            "
                SELECT callee_start_byte, callee_end_byte
                FROM edges
                WHERE edge_kind = ?1
                  AND COALESCE(from_name, '') LIKE ?2
                  AND to_name LIKE ?3
                ",
            params![edge_kind, format!("%{from}%"), format!("%{to}%")],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .unwrap();
    match (start, end) {
        (Some(start), Some(end)) => Some((start, end)),
        _ => None,
    }
}

fn hot_module_text(revision: usize) -> String {
    let mut text = String::new();
    text.push_str("pub fn entry() -> i32 {\n");
    for i in 0..32 {
        text.push_str(&format!("    helper_{i}() +\n"));
    }
    text.push_str(&format!("    {revision}\n}}\n"));
    for i in 0..32 {
        text.push_str(&format!("pub fn helper_{i}() -> i32 {{ {i} }}\n"));
    }
    text
}

fn unique_temp_root() -> PathBuf {
    let mut root = std::env::temp_dir();
    let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    root.push(format!("rag-rat-schema-test-{}-{}-{suffix}", std::process::id(), now_ms()));
    root
}

fn fixture_temp_root(fixture: &str) -> PathBuf {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let fixture_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(fixture);
    copy_fixture_dir(&fixture_root, &root);
    root
}

fn copy_fixture_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let from_path = entry.path();
        let to_path = to.join(entry.file_name());
        if from_path.is_dir() {
            copy_fixture_dir(&from_path, &to_path);
        } else {
            fs::copy(&from_path, &to_path).unwrap();
        }
    }
}

fn markdown_config(text: &str) -> (PathBuf, Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("search.md"), text).unwrap();
    let config = markdown_config_for_root(root.clone());
    (root, config)
}

fn markdown_config_for_root(root: PathBuf) -> Config {
    Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "markdown".to_string(),
            language: Language::Markdown,
            directories: vec![PathBuf::from("docs")],
            include: vec!["**/*.md".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Docs,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}

/// GitHub context for tests: the rag-rat repo itself, never the live `gh` CLI (#60).
fn test_gh_ctx() -> github::GitHubContext {
    github::GitHubContext::new(Some("cq27-dev/rag-rat"), false)
}
// ---- #219 stage 2: linked-worktree overlay indexing ----

fn init_git_repo(root: &Path) {
    run_git(root, &["init", "-q", "-b", "main"]);
    run_git(root, &["config", "user.email", "t@e"]);
    run_git(root, &["config", "user.name", "t"]);
}

/// Symbol names visible in the ACTIVE scope for `path` — queried through the `temp.files` scope
/// view (set by `set_context`), so overlay shadowing and tombstones are reflected exactly as a real
/// query would see them.
fn names_in_scope(db: &IndexDatabase, path: &str) -> Vec<String> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT s.name FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path = ?1 \
             ORDER BY s.name",
        )
        .unwrap();
    let names = stmt.query_map([path], |row| row.get::<_, String>(0)).unwrap();
    names.filter_map(Result::ok).collect()
}

/// Whether `path` is visible at all in the active scope view (a tombstone makes this false even
/// when the base committed row exists).
fn path_in_scope(db: &IndexDatabase, path: &str) -> bool {
    db.storage
        .connection()
        .query_row("SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1)", [path], |row| row.get(0))
        .unwrap()
}

fn set_base_scope(db: &mut IndexDatabase, root: &Path) {
    let (sha, _) = resolve_git_context(root);
    db.set_context(&sha, &worktree_id_of(root)).unwrap();
}

/// The resolved target symbol id of the single `calls_name` edge whose source file is `path` — or
/// `None` when it is unresolved. Reads `edges_data` directly (the edge rows are shared across
/// scopes; `source_file_id` keys them by the scope's file row), so it can prove a shared committed
/// caller's edge is left intact by an overlay pass (#219 P1).
fn calls_edge_target(db: &IndexDatabase, path: &str) -> Option<i64> {
    db.storage
        .connection()
        .query_row(
            "SELECT d.to_symbol_id FROM edges_data d
             JOIN files f ON f.id = d.source_file_id
             JOIN name_strings ek ON ek.id = d.edge_kind_id
             WHERE f.path = ?1 AND ek.value = 'calls_name'",
            [path],
            |row| row.get::<_, Option<i64>>(0),
        )
        .unwrap()
}

/// The ACTIVE repo's `parser_failures` row count. Scoped by `repo_id` (V040 gave `parser_failures`
/// a `(repo_id, path)` PK), so a consolidated DB's other repos — including the poison-sibling test
/// tripwire — never inflate the "this repo's parse failures" count the overlay tests assert on.
fn parser_failure_total(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM parser_failures WHERE repo_id = ?1",
            [&db.active_repo_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// The lowest chunk id whose file row has `path` in the active scope (overlay row wins).
fn scoped_chunk_id(db: &IndexDatabase, path: &str) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
             WHERE f.path = ?1 ORDER BY c.id LIMIT 1",
            [path],
            |row| row.get(0),
        )
        .unwrap()
}

/// Raw count of overlay file rows (`worktree_id != ''`), bypassing the scope view — to assert GC
/// keeps/prunes overlay rows directly.
fn overlay_row_count(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM main.files WHERE worktree_id != ''", [], |row| row.get(0))
        .unwrap()
}

/// A real git repo with one committed rust file, plus its source `Config` — the fixture the
/// poison-sibling harness self-tests against. A REAL git root (not a bare temp dir) so
/// `adopt_repo_from_config` registers a portable repo id (a non-git root is `Absent` and stays
/// under the placeholder, which would leave `real_repos == 0` and defeat the opt-out self-check).
pub(crate) fn poison_test_config(tag: &str) -> (PathBuf, Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), format!("pub fn {tag}_anchor() -> i32 {{ 1 }}\n")).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "seed"]);
    let config = source_config(root.clone(), Language::Rust);
    (root, config)
}

fn source_config(root: PathBuf, language: Language) -> Config {
    Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: language.as_str().to_string(),
            language,
            directories: vec![PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}

fn assert_edge(db: &IndexDatabase, from: &str, to: &str, edge_kind: &str, confidence: &str) {
    let count = db
        .storage
        .connection()
        .query_row(
            "
                SELECT COUNT(*)
                FROM edges
                WHERE edge_kind = ?1
                  AND confidence = ?2
                  AND COALESCE(from_name, '') LIKE ?3
                  AND to_name LIKE ?4
                ",
            params![edge_kind, confidence, format!("%{from}%"), format!("%{to}%")],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert!(count > 0, "missing edge {from} -[{edge_kind}/{confidence}]-> {to}");
}

// ─── dir_tree tests ──────────────────────────────────────────────────────────

/// Shared helper: build a dir-only `RepoMemoryBindTarget`.
fn dir_bind_target(dir: Option<String>) -> crate::query::memory::RepoMemoryBindTarget {
    crate::query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        github_owner: None,
        github_repo: None,
        github_number: None,
        start_logical_symbol_id: None,
        end_logical_symbol_id: None,
        edge_sequence_hash: None,
        path_summary: None,
        edge_path: None,
        dir,
    }
}

/// Shared helper: create a minimal "dir" memory attached to the given directory path.
fn create_dir_memory(db: &IndexDatabase, title: &str, dir: Option<String>) {
    db.memory_create(crate::query::memory::RepoMemoryCreate {
        kind: "Decision".to_string(),
        title: title.to_string(),
        body: format!("Memory for {dir:?}."),
        confidence: "high".to_string(),
        created_by: Some("test".to_string()),
        source: Some("agent".to_string()),
        tags: vec![],
        payload_json: None,
        bind: dir_bind_target(dir),
    })
    .unwrap();
}

/// Shared helper: install the scope view on `conn` for the repo at `root`.
fn install_scope(conn: &rusqlite::Connection, root: &Path) {
    let (commit_sha, worktree_id) = resolve_git_context(root);
    crate::index::install_scope_view(conn, &commit_sha, &worktree_id).unwrap();
}

fn table_count(db: &IndexDatabase, table: &str) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [table], |row| row.get(0))
        .unwrap()
}

fn row_count(db: &IndexDatabase, table: &str) -> i64 {
    db.storage
        .connection()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
        .unwrap()
}

fn chunk_columns(db: &IndexDatabase) -> Vec<String> {
    table_columns(db, "chunks")
}

fn file_columns(db: &IndexDatabase) -> Vec<String> {
    table_columns(db, "files")
}

fn table_columns(db: &IndexDatabase, table: &str) -> Vec<String> {
    let mut stmt = db.storage.connection().prepare(&format!("PRAGMA table_info({table})")).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().map(Result::unwrap).collect()
}

fn conn_table_columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().map(Result::unwrap).collect()
}

fn conn_table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .optional()
    .unwrap()
    .is_some()
}

/// Delete every `schema_version` ledger row for a migration NEWER than `version`, so a freshly
/// `apply`ed in-memory DB reads as if it stopped at `version` and the next
/// `apply`/`migrate_forward` replays `version + 1` onward. `known_version` reads the MAX applied
/// version, so EVERY later row must go (leaving one would keep the schema Compatible and skip the
/// forward migrate). Migration ids are `NNN_name`, so the leading three digits are the version.
/// Migration-count-agnostic: a newly added migration is truncated automatically, so a schema bump
/// no longer means editing a growing list of named `DELETE FROM schema_version` lines (#222).
fn truncate_schema_to(conn: &rusqlite::Connection, version: u32) {
    conn.execute("DELETE FROM schema_version WHERE CAST(substr(id, 1, 3) AS INTEGER) > ?1", [
        i64::from(version),
    ])
    .expect("truncate schema_version ledger");
}

/// True when an INDEX of this name exists (sqlite_master, type='index').
fn conn_index_exists(conn: &rusqlite::Connection, index: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [index],
        |_| Ok(()),
    )
    .optional()
    .unwrap()
    .is_some()
}

fn indexed_revision_count(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM files WHERE indexed_revision != ''", [], |row| row.get(0))
        .unwrap()
}

fn chunk_source_revision_count(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT COUNT(*) FROM chunks WHERE source_revision != ''", [], |row| row.get(0))
        .unwrap()
}

fn first_chunk_id(db: &IndexDatabase) -> i64 {
    db.storage
        .connection()
        .query_row("SELECT id FROM chunks ORDER BY id LIMIT 1", [], |row| row.get(0))
        .unwrap()
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_git_with_env(root: &Path, args: &[&str], env: &[(&str, &str)]) {
    let output = Command::new("git")
        .args(args)
        .envs(env.iter().copied())
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {:?} failed", args);
}

fn git_output(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(output.status.success(), "git {:?} failed", args);
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

struct MockGitHubClient;

impl github::GitHubClient for MockGitHubClient {
    fn issue(&self, owner: &str, repo: &str, number: i64) -> anyhow::Result<github::GitHubIssue> {
        Ok(github::GitHubIssue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            html_url: format!("https://github.com/{owner}/{repo}/issues/{number}"),
            state: "open".to_string(),
            title: "Decision: keep sqlite".to_string(),
            body: "We decided sqlite is required for binary size.".to_string(),
            author: Some("octo".to_string()),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            updated_at: Some("2026-01-02T00:00:00Z".to_string()),
            is_pull_request: true,
        })
    }

    fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubComment>> {
        Ok(vec![github::GitHubComment {
            id: 4201,
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            html_url: format!("https://github.com/{owner}/{repo}/issues/{number}#comment-1"),
            body: "Rejected alternative: duckdb was too large.".to_string(),
            author: Some("octo".to_string()),
            created_at: Some("2026-01-01T01:00:00Z".to_string()),
            updated_at: Some("2026-01-01T01:00:00Z".to_string()),
        }])
    }

    fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<github::GitHubPullRequest>> {
        Ok(Some(github::GitHubPullRequest {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            html_url: format!("https://github.com/{owner}/{repo}/pull/{number}"),
            state: "open".to_string(),
            title: "Use sqlite".to_string(),
            body: "Constraint: normal queries must use cache only.".to_string(),
            author: Some("octo".to_string()),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            updated_at: Some("2026-01-02T00:00:00Z".to_string()),
            merged_at: None,
        }))
    }

    fn pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubReview>> {
        Ok(vec![github::GitHubReview {
            id: 4202,
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            html_url: Some(format!("https://github.com/{owner}/{repo}/pull/{number}#review")),
            state: "COMMENTED".to_string(),
            body: "Risk: live crawling during search would be surprising.".to_string(),
            author: Some("reviewer".to_string()),
            submitted_at: Some("2026-01-01T02:00:00Z".to_string()),
        }])
    }

    fn pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubReviewComment>> {
        Ok(vec![github::GitHubReviewComment {
            id: 4203,
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
            path: Some("docs/search.md".to_string()),
            html_url: format!("https://github.com/{owner}/{repo}/pull/{number}#discussion"),
            body: "No longer use obsolete duckdb rationale.".to_string(),
            author: Some("reviewer".to_string()),
            created_at: Some("2026-01-01T03:00:00Z".to_string()),
            updated_at: Some("2026-01-01T03:00:00Z".to_string()),
        }])
    }
}

struct PartiallyFailingGitHubClient;

impl github::GitHubClient for PartiallyFailingGitHubClient {
    fn issue(&self, owner: &str, repo: &str, number: i64) -> anyhow::Result<github::GitHubIssue> {
        if number == 404 {
            anyhow::bail!("gh: Not Found (HTTP 404)");
        }
        MockGitHubClient.issue(owner, repo, number)
    }

    fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubComment>> {
        MockGitHubClient.issue_comments(owner, repo, number)
    }

    fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<github::GitHubPullRequest>> {
        MockGitHubClient.pull(owner, repo, number)
    }

    fn pull_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubReview>> {
        MockGitHubClient.pull_reviews(owner, repo, number)
    }

    fn pull_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Vec<github::GitHubReviewComment>> {
        MockGitHubClient.pull_review_comments(owner, repo, number)
    }
}

/// A git fixture with one committed source file, configured like production (absolute DB path).
fn git_fixture_for_overlay_tests() -> (PathBuf, Config) {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    run_git(&root, &["init"]);
    run_git(&root, &["config", "user.name", "Rag Rat"]);
    run_git(&root, &["config", "user.email", "rag@example.com"]);
    fs::write(root.join("src/lib.rs"), "pub fn stable() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("src/extra.rs"), "pub fn extra() -> i32 { 2 }\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-m", "init"]);
    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    (root, config)
}

/// Insert a worktree-overlay `files` row mirroring an existing committed row's content — the
/// stale leftover a dirty-then-committed file leaves behind when its cleanup never ran (#87).
fn insert_stale_overlay_row(db: &IndexDatabase, path: &str, worktree_id: &str) -> i64 {
    let (sha, language, kind): (String, String, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT sha256, language, kind FROM main.files WHERE path = ?1 AND commit_sha != ''",
            [path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    // Stamp the ACTIVE repo id (V040): a stale overlay row is a leftover from a prior index of the
    // SAME repo, so it must carry that repo's id for the full-rebuild staging + scope view to see
    // it.
    // Stamp the ACTIVE generation (A6): a real stale overlay is a leftover from a PRIOR index of
    // the LIVE generation, so it lives at that generation and is visible through the scope view
    // exactly like a genuine leftover — inserting it at the default 0 while the index is on a
    // later generation would make it invisible to discovery and defeat the heal path the tests
    // exercise.
    db.storage
        .connection()
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id, generation) VALUES (?1, ?2, ?3, ?4, 0, 0, '', ?5, \
             ?6, ?7)",
            rusqlite::params![
                path,
                language,
                kind,
                sha,
                worktree_id,
                db.active_repo_id,
                db.active_generation
            ],
        )
        .unwrap();
    db.storage.connection().last_insert_rowid()
}

/// Every `ON DELETE CASCADE`/`RESTRICT` FK to a reindex-volatile parent, as
/// `(child_table, parent_table, on_delete)`, scanned from a FULLY-MIGRATED DB. Enumerates EVERY
/// table in `sqlite_master` (not a hand-maintained list) so a new offender is caught automatically.
fn cascading_fks_to_volatile_parents(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    };
    let mut found = Vec::new();
    for table in tables {
        // `pragma_foreign_key_list` columns: `id`, `seq`, `table` (parent), `from`, `to`,
        // `on_update`, `on_delete`, `match`.
        let mut stmt = conn
            .prepare(&format!(
                "SELECT \"table\", on_delete FROM pragma_foreign_key_list('{table}')"
            ))
            .unwrap();
        let fks: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for (parent, on_delete) in fks {
            let is_volatile_parent =
                crate::index::schema::REINDEX_VOLATILE_PARENTS.contains(&parent.as_str());
            let is_cascading = matches!(on_delete.to_uppercase().as_str(), "CASCADE" | "RESTRICT");
            if is_volatile_parent && is_cascading {
                found.push((table.clone(), parent, on_delete));
            }
        }
    }
    found
}

/// Helper: write four renamed clones (identical structure, different identifiers) across two
/// directories into `root`, returning the rebuilt index. The four functions form ONE clean,
/// high-fidelity clone class — the canonical refine fixture.
fn write_four_renamed_clones(root: &PathBuf) -> IndexDatabase {
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    for (dir, name, var) in [
        ("a", "load_user", "u"),
        ("a", "load_order", "o"),
        ("b", "load_item", "i"),
        ("b", "load_blob", "x"),
    ] {
        fs::write(
            root.join(dir).join(format!("{name}.rs")),
            format!(
                "pub fn {name}(db: Db) -> i32 {{ let {var} = db.get(1); validate({var}); {var} + \
                 1 }}\n"
            ),
        )
        .unwrap();
    }
    let config = Config {
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("a"), PathBuf::from("b")],
            include: vec!["a/".to_string(), "b/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    IndexDatabase::rebuild(&config).unwrap()
}

/// Resolve the `symbols.id` of a fingerprinted function by its qualified-name `ref` (the
/// `path::name` form `find_clones`/`clones_for_symbol` emit). Used by the refine-loader tests to
/// get the raw member ids `load_refine_members` takes.
fn fingerprinted_symbol_id_for_ref(db: &IndexDatabase, qualified_name: &str) -> i64 {
    db.storage
        .connection()
        .query_row(
            "SELECT symbols.id
             FROM symbols
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             JOIN symbol_fingerprints sf
               ON sf.symbol_id = symbols.id
               AND sf.normalizer_kind = 'baseline'
             WHERE ns.value = ?1
             ORDER BY symbols.id
             LIMIT 1",
            params![qualified_name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or_else(|e| panic!("no fingerprinted symbol id for ref {qualified_name}: {e}"))
}

mod chunk_store_migrations;
mod clones;
mod dir_memory_tree;
mod dispatch;
mod generation_rebuild;
mod git_history_reload;
mod github_papertrail;
mod graph_edges;
mod multi_repo_scope;
mod orientation_healing;
mod reconcile_embeddings;
mod repo_memory;
mod repo_registry;
mod schema_migrations;
mod symbol_search_lookup;
mod worktree_overlay;
