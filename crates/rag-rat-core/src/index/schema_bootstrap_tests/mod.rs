use std::process::Command;

use rag_rat_base::config::ResolvedTarget;
// The embedding-model constants moved from `index::ai` to the crate-root registry (#112).
// Import them here so the existing `HASH_MODEL_ID` / `FASTEMBED_*` references resolve to the
// new path.
use rag_rat_base::embedding_models::{
    FASTEMBED_DISPLAY_MODEL, FASTEMBED_EMBEDDING_DIM, FASTEMBED_MODEL_ID, HASH_EMBEDDING_DIM,
    HASH_MODEL_ID,
};

use super::*;

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
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
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
    // Keep the live index out of the tree: a later `git add .` in this test would otherwise commit
    // .rag-rat/index.sqlite, and the checkout/squash round-trip then removes/restores (and under
    // Windows autocrlf can corrupt) that committed db, failing the subsequent index open.
    fs::write(root.join(".gitignore"), ".rag-rat/\n").unwrap();
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

pub(crate) struct ScratchRoot {
    path: PathBuf,
    _scratch: rag_rat_base::test_scratch::ScratchDir,
}

impl ScratchRoot {
    fn new(tag: &str) -> Self {
        let scratch = rag_rat_base::test_scratch::ScratchDir::new(tag);
        let path = scratch.path().to_path_buf();
        // Preserve the old helper's allocated-but-absent path contract for worktree destinations.
        let _ = fs::remove_dir_all(&path);
        Self { path, _scratch: scratch }
    }
}

impl std::ops::Deref for ScratchRoot {
    type Target = PathBuf;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for &ScratchRoot {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

fn unique_temp_root() -> ScratchRoot {
    ScratchRoot::new("rag-rat-schema-test")
}

fn fixture_temp_root(fixture: &str) -> ScratchRoot {
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

fn markdown_config(text: &str) -> (ScratchRoot, Config) {
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
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
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
fn test_gh_ctx() -> papertrail::PapertrailContext {
    papertrail::PapertrailContext::new(Some("cq27-dev/rag-rat"))
}

/// Test-side bridge over the async sync entry point, mirroring the IndexDatabase boundary.
fn sync_from_refs_blocking<C: papertrail::PapertrailClient>(
    conn: &rusqlite::Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &papertrail::PapertrailContext,
) -> anyhow::Result<papertrail::PapertrailSyncReport> {
    papertrail::block_on(papertrail::sync_from_refs(conn, root, client, offline, ctx))
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
pub(crate) fn poison_test_config(tag: &str) -> (ScratchRoot, Config) {
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
    source_config_dirs(root, language, &["src"])
}

/// `source_config` for a layout that isn't `src/` — SwiftPM puts first-party code under `Sources/`
/// and its test targets under `Tests/`.
fn source_config_dirs(root: PathBuf, language: Language, dirs: &[&str]) -> Config {
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: language.as_str().to_string(),
            language,
            directories: dirs.iter().map(PathBuf::from).collect(),
            include: dirs.iter().map(|dir| format!("{dir}/")).collect(),
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        // The #822 overlay quiet window is OFF in this shared fixture: the overlay/watch tests
        // built on it assert per-pass refresh semantics (the #577 scope matrix, delta
        // categorization, prune safety), which the window would time-gate. Tests exercising the
        // window itself opt in by setting `overlay_quiet_secs` explicitly.
        watch: rag_rat_base::config::WatchConfig { overlay_quiet_secs: 0, ..Default::default() },
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
fn dir_bind_target(dir: Option<String>) -> rag_rat_query::memory::RepoMemoryBindTarget {
    rag_rat_query::memory::RepoMemoryBindTarget {
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        path: None,
        start_line: None,
        end_line: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
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
    db.memory_create(rag_rat_query::memory::RepoMemoryCreate {
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

/// A `git` invocation with line-ending conversion forced OFF for this child process only. GitHub's
/// windows-latest runner sets `core.autocrlf=true` globally, which rewrites the LF fixtures these
/// tests write+index as CRLF on checkout — changing file bytes and thus the content sha256, which
/// defeats the HEAD-move carry (#502) sha match and corrupts the committed index db. `GIT_CONFIG_*`
/// overrides global config without touching the user's config.
fn git_command(root: &Path, args: &[&str]) -> Command {
    rag_rat_base::test_git::command(root, args)
}

fn run_git(root: &Path, args: &[&str]) {
    rag_rat_base::test_git::run(root, args);
}

fn run_git_with_env(root: &Path, args: &[&str], env: &[(&str, &str)]) {
    let output = git_command(root, args).envs(env.iter().copied()).output().unwrap();
    assert!(output.status.success(), "git {:?} failed", args);
}

fn git_output(root: &Path, args: &[&str]) -> String {
    rag_rat_base::test_git::output(root, args)
}

struct MockGitHubClient;

impl MockGitHubClient {
    fn mock_item(project: &str, key: &str) -> papertrail::PapertrailItem {
        papertrail::PapertrailItem {
            project: project.to_string(),
            item_kind: papertrail::ItemKind::ChangeRequest,
            item_key: key.to_string(),
            url: format!("https://github.com/{project}/pull/{key}"),
            state: "open".to_string(),
            title: "Decision: keep sqlite".to_string(),
            body: "We decided sqlite is required for binary size.".to_string(),
            author: Some("octo".to_string()),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            updated_at: Some("2026-01-02T00:00:00Z".to_string()),
            merged_at: None,
            closed_at: None,
            resolution: None,
            merge_commit_sha: None,
            author_kind: None,
            author_association: None,
            tags: Vec::new(),
        }
    }

    fn mock_comments(project: &str, key: &str) -> Vec<papertrail::PapertrailComment> {
        let comment = |id: &str, body: &str| papertrail::PapertrailComment {
            project: project.to_string(),
            item_kind: papertrail::ItemKind::ChangeRequest,
            item_key: key.to_string(),
            comment_id: id.to_string(),
            url: Some(format!("https://github.com/{project}/issues/{key}#comment-{id}")),
            body: body.to_string(),
            author: Some("octo".to_string()),
            author_kind: None,
            author_association: None,
            created_at: Some("2026-01-01T01:00:00Z".to_string()),
            updated_at: Some("2026-01-01T01:00:00Z".to_string()),
            review_state: None,
            anchor_path: None,
        };
        vec![
            comment("4201", "Rejected alternative: duckdb was too large."),
            comment("4204", "Constraint: normal queries must use cache only."),
            papertrail::PapertrailComment {
                url: None,
                author: Some("reviewer".to_string()),
                author_kind: None,
                author_association: None,
                review_state: Some("COMMENTED".to_string()),
                ..comment("4202", "Risk: live crawling during search would be surprising.")
            },
            papertrail::PapertrailComment {
                author: Some("reviewer".to_string()),
                author_kind: None,
                author_association: None,
                anchor_path: Some("docs/search.md".to_string()),
                ..comment("4203", "No longer use obsolete duckdb rationale.")
            },
        ]
    }
}

impl papertrail::PapertrailClient for MockGitHubClient {
    async fn item(
        &self,
        project: &str,
        _kind: papertrail::ItemKind,
        key: &str,
    ) -> anyhow::Result<papertrail::PapertrailItem> {
        Ok(Self::mock_item(project, key))
    }

    async fn item_comments(
        &self,
        project: &str,
        _kind: papertrail::ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<papertrail::PapertrailComment>> {
        Ok(Self::mock_comments(project, key))
    }

    async fn items_page(
        &self,
        project: &str,
        _cursor: &papertrail::PageCursor,
    ) -> anyhow::Result<papertrail::ItemsPage> {
        Ok(papertrail::ItemsPage {
            items: vec![Self::mock_item(project, "42")],
            next: None,
            backfill_boundary: None,
        })
    }

    async fn comments_page(
        &self,
        project: &str,
        _cursor: &papertrail::PageCursor,
    ) -> anyhow::Result<papertrail::CommentsPage> {
        Ok(papertrail::CommentsPage {
            comments: Self::mock_comments(project, "42"),
            next: None,
            frontier: None,
        })
    }

    async fn freshness_probe(
        &self,
        _project: &str,
        probe: &papertrail::FreshnessProbe,
    ) -> anyhow::Result<papertrail::FreshnessResult> {
        let latest = match probe.updated_since.as_deref() {
            Some(since) if since >= "2026-01-02T00:00:00Z" => None,
            _ => Some("2026-01-02T00:00:00Z".to_string()),
        };
        Ok(papertrail::FreshnessResult { latest, etag: None, not_modified: false })
    }
}

struct PartiallyFailingGitHubClient;

impl papertrail::PapertrailClient for PartiallyFailingGitHubClient {
    async fn item(
        &self,
        project: &str,
        kind: papertrail::ItemKind,
        key: &str,
    ) -> anyhow::Result<papertrail::PapertrailItem> {
        if key == "404" {
            anyhow::bail!("gh: Not Found (HTTP 404)");
        }
        MockGitHubClient.item(project, kind, key).await
    }

    async fn item_comments(
        &self,
        project: &str,
        kind: papertrail::ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<papertrail::PapertrailComment>> {
        MockGitHubClient.item_comments(project, kind, key).await
    }

    async fn items_page(
        &self,
        project: &str,
        cursor: &papertrail::PageCursor,
    ) -> anyhow::Result<papertrail::ItemsPage> {
        MockGitHubClient.items_page(project, cursor).await
    }

    async fn comments_page(
        &self,
        project: &str,
        cursor: &papertrail::PageCursor,
    ) -> anyhow::Result<papertrail::CommentsPage> {
        MockGitHubClient.comments_page(project, cursor).await
    }

    async fn freshness_probe(
        &self,
        project: &str,
        probe: &papertrail::FreshnessProbe,
    ) -> anyhow::Result<papertrail::FreshnessResult> {
        MockGitHubClient.freshness_probe(project, probe).await
    }
}

/// A git fixture with one committed source file, configured like production (absolute DB path).
fn git_fixture_for_overlay_tests() -> (ScratchRoot, Config) {
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
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
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
                rag_rat_db::schema::REINDEX_VOLATILE_PARENTS.contains(&parent.as_str());
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
fn write_four_renamed_clones(root: &Path) -> IndexDatabase {
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
    IndexDatabase::rebuild(&four_clone_config(root)).unwrap()
}

/// The `write_four_renamed_clones` fixture's config — a rust target over the `a/` + `b/` dirs —
/// shared with tests that write their own four-member variant (e.g. the #275 differing-callee
/// fixture) before rebuilding.
fn four_clone_config(root: &Path) -> Config {
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.to_path_buf(),
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
    }
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
mod graph_edges;
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
