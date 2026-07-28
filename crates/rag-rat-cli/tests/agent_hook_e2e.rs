//! End-to-end tests for `rag-rat agent-hook` (Unix only for socket paths; the no-op
//! contract tests run everywhere).

use std::io::Write;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Child,
    time::{Duration, Instant},
};

mod common;

use common::unique_dir;
#[cfg(unix)]
use common::{ScratchRoot, git, git_commit};
#[cfg(unix)]
use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};

fn run_hook(stdin_body: &str, cwd: &std::path::Path) -> (String, std::process::ExitStatus) {
    run_hook_for(None, stdin_body, cwd)
}

fn run_hook_for(
    harness: Option<&str>,
    stdin_body: &str,
    cwd: &std::path::Path,
) -> (String, std::process::ExitStatus) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .arg("agent-hook")
        .args(harness)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Capture (do not discard) stderr so a non-zero exit — e.g. a startup abort before the
        // hook's own always-Ok handler runs — surfaces its reason in the failing test's output
        // instead of a bare `status.success()` assertion.
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.as_mut().unwrap().write_all(stdin_body.as_bytes());
    let out = child.wait_with_output().unwrap();
    if !out.status.success() {
        eprintln!(
            "run_hook: `rag-rat agent-hook` exited unsuccessfully: {:?}\n--- captured stderr \
             ---\n{}\n--- end stderr ---",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status)
}

#[test]
fn no_rag_rat_toml_means_silent_exit_zero() {
    let dir = unique_dir("hook-noindex");
    std::fs::create_dir_all(&dir).unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir.as_path(), "hook_event_name": "PreToolUse",
        "tool_name": "Grep", "tool_input": {"pattern": "anything"}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success());
    assert!(stdout.is_empty(), "must print nothing without an index, got: {stdout}");
}

#[test]
fn garbage_stdin_means_silent_exit_zero() {
    let dir = unique_dir("hook-garbage");
    std::fs::create_dir_all(&dir).unwrap();
    let (stdout, status) = run_hook("this is not json", &dir);
    assert!(status.success());
    assert!(stdout.is_empty());
}

#[test]
fn cursor_and_vscode_missing_state_are_silent() {
    let dir = unique_dir("hook-adapter-noindex");
    std::fs::create_dir_all(&dir).unwrap();
    let cursor = serde_json::json!({
        "hook_event_name": "sessionStart",
        "conversation_id": "cursor-session",
        "workspace_roots": [dir.as_path()],
    });
    let vscode = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "vscode-session",
        "cwd": dir.as_path(),
        "tool_name": "run_in_terminal",
        "tool_input": {"command": "rg anything"},
    });
    for (harness, input) in [("cursor", cursor), ("vscode", vscode)] {
        let (stdout, status) = run_hook_for(Some(harness), &input.to_string(), &dir);
        assert!(status.success(), "{harness} must fail open");
        assert!(stdout.is_empty(), "{harness} must be silent without rag-rat state: {stdout}");
    }
}

#[test]
fn non_search_tool_means_silent_exit_zero() {
    let dir = unique_dir("hook-read");
    std::fs::create_dir_all(&dir).unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir.as_path(), "hook_event_name": "PreToolUse",
        "tool_name": "Read", "tool_input": {"path": "/x"}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success());
    assert!(stdout.is_empty());
}

/// #661: a PostToolUse edit with a config but no built index must no-op silently AND must not
/// create the index (the non-creating DB-absent gate — a stray empty DB would defeat later
/// `database.exists()` hints).
#[test]
fn posttooluse_without_an_index_is_silent_and_non_creating() {
    let dir = unique_dir("hook-post-noindex");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir.as_path(), "hook_event_name": "PostToolUse",
        "tool_name": "Write", "tool_input": {"file_path": dir.join("src/new.rs")}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success(), "PostToolUse must exit 0 even with no index");
    assert!(stdout.is_empty(), "PostToolUse prints nothing, got: {stdout}");
    assert!(!dir.join(".rag-rat/index.sqlite").is_file(), "the hook must not create the index");
}

/// Full path: a live `rag-rat mcp` elects the hook listener, the `agent-hook` client reaches it
/// over the Unix socket and gets the indexed symbol, a repeat in the same session is deduped to
/// nothing, a fresh session sees it again, and once the server dies the client falls back to a
/// direct read-only SQLite query (no dedupe). Proves listener + client + dedupe + fallback wiring.
#[cfg(unix)]
#[test]
fn socket_path_serves_dedupes_and_falls_back() {
    let repo = TestRepo::indexed_with_symbol();
    let mut server = repo.spawn_mcp_initialized();

    // The listener binds asynchronously after election; poll the exact path the client computes
    // (the `sockets/`→XDG→temp budget cascade means globbing one dir would miss a divert).
    let socket = repo.socket_path();
    wait_for(&socket, Duration::from_secs(10));

    // 1. First hook in session s1: the listener answers with the indexed symbol + its file.
    let first = repo.run_hook_session("s1");
    let context = additional_context(&first)
        .unwrap_or_else(|| panic!("listener gave no additionalContext, got: {first}"));
    assert!(
        context.contains("frobnicate_xyz") && context.contains("src/lib.rs"),
        "listener context must name the indexed symbol and file, got: {context}"
    );

    // 2. Same session s1 again: the listener already injected the symbol, so it dedupes to a null
    //    context and the client prints nothing at all.
    let repeat = repo.run_hook_session("s1");
    assert!(
        repeat.is_empty(),
        "same-session repeat must be deduped to empty stdout, got: {repeat}"
    );

    // 3. A different session s2 while the server is still alive: not deduped, sees it again.
    let other = repo.run_hook_session("s2");
    let other_context = additional_context(&other)
        .unwrap_or_else(|| panic!("fresh session got no additionalContext, got: {other}"));
    assert!(
        other_context.contains("frobnicate_xyz"),
        "fresh session must not be deduped, got: {other_context}"
    );

    // 4. Kill the server: the socket goes away and the client takes the stateless fallback path
    //    (direct read-only SQLite, no dedupe), so even a previously-injected session gets context.
    server.kill_and_wait();
    // Listener teardown releases the socket asynchronously; poll until the connect would fail so
    // the run below provably exercises fallback, not a lingering listener.
    wait_until_gone(&socket, Duration::from_secs(10));
    let fallback = repo.run_hook_session("s1");
    let fallback_context = additional_context(&fallback).unwrap_or_else(|| {
        panic!("fallback path gave no additionalContext after server death, got: {fallback}")
    });
    assert!(
        fallback_context.contains("frobnicate_xyz") && fallback_context.contains("src/lib.rs"),
        "fallback must compose from SQLite directly, got: {fallback_context}"
    );
}

#[cfg(unix)]
struct TestRepo {
    root: ScratchRoot,
    config_path: PathBuf,
    config: rag_rat_base::config::Config,
}

#[cfg(unix)]
impl TestRepo {
    /// A temp repo with one Rust source file carrying a distinctive symbol, indexed in-process via
    /// `IndexDatabase::rebuild` (the same path `mcp_stdio`/`mcp_hot_upgrade` use to populate a real
    /// index). Unique per run so parallel tests never collide on the socket election lock.
    fn indexed_with_symbol() -> Self {
        let root = unique_dir("hook-e2e");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn frobnicate_xyz() {}\n").unwrap();
        let config_path = root.join("rag-rat.toml");
        fs::write(
            &config_path,
            "[index]\nroot = \".\"\ndatabase = \
             \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let config = rag_rat_base::config::Config::load(&config_path).unwrap();
        rag_rat_core::IndexDatabase::rebuild(&config).unwrap();
        // Guard against a vacuous test: the symbol lane only fires if the symbol is really indexed.
        assert_symbol_indexed(&config, "frobnicate_xyz");
        Self { root, config_path, config }
    }

    /// Same client computation as the CLI: the deterministic socket path for this config.
    fn socket_path(&self) -> PathBuf {
        rag_rat_base::locks::hook_socket_path_for(&self.config)
    }

    fn add_file_memory(&self, marker: &str) {
        rag_rat_core::IndexDatabase::open_config(&self.config)
            .unwrap()
            .memory_create(RepoMemoryCreate {
                kind: "Invariant".to_string(),
                title: marker.to_string(),
                body: "The read hook must surface this memory title.".to_string(),
                confidence: "high".to_string(),
                created_by: Some("agent-hook-e2e".to_string()),
                source: Some("agent".to_string()),
                tags: Vec::new(),
                payload_json: None,
                bind: RepoMemoryBindTarget {
                    path: Some("src/lib.rs".to_string()),
                    ..Default::default()
                },
            })
            .unwrap();
    }

    /// Spawn `rag-rat mcp` and drive `initialize` so the server is fully up and `run_stdio_unix`
    /// has spawned the hook listener.
    fn spawn_mcp_initialized(&self) -> McpServer {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
            .arg("mcp")
            .arg("--config")
            .arg(&self.config_path)
            .env("RAG_RAT_NO_WATCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        writeln!(
            stdin,
            "{}",
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": {"name": "rag-rat-hook-e2e", "version": "0.1"}
                }
            })
        )
        .unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("\"id\":1"), "initialize response, got: {line}");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
        )
        .unwrap();
        stdin.flush().unwrap();
        // Keep stdin/stdout owned so the server's pipes stay open for the life of the test.
        McpServer { child, _stdin: stdin, _reader: reader }
    }

    /// Run the hook client with a Grep `tool_input` for the indexed symbol under the given session,
    /// cwd = repo root (so `find_config` walks up to our `rag-rat.toml`).
    fn run_hook_session(&self, session_id: &str) -> String {
        let input = serde_json::json!({
            "session_id": session_id, "cwd": self.root.as_path(), "hook_event_name": "PreToolUse",
            "tool_name": "Grep", "tool_input": {"pattern": "frobnicate_xyz"}
        });
        let (stdout, status) = run_hook(&input.to_string(), &self.root);
        assert!(status.success(), "agent-hook must exit zero on every path");
        stdout
    }
}

#[cfg(unix)]
struct LinkedTestRepo {
    main: ScratchRoot,
    linked: ScratchRoot,
    config: rag_rat_base::config::Config,
}

#[cfg(unix)]
impl LinkedTestRepo {
    fn indexed() -> Self {
        let main = unique_dir("hook-linked-main");
        fs::create_dir_all(main.join("src")).unwrap();
        fs::write(main.join("src/lib.rs"), "pub fn main_checkout_xyz() {}\n").unwrap();
        fs::write(main.join(".gitignore"), ".rag-rat/\nrag-rat.toml\n").unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git_commit(&main, &["-q", "-m", "base"]);

        let config_path = main.join("rag-rat.toml");
        fs::write(
            &config_path,
            "[index]\nroot = \".\"\ndatabase = \
             \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let config = rag_rat_base::config::Config::load(&config_path).unwrap();
        rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

        let linked = unique_dir("hook-linked-worktree");
        git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
        assert!(!linked.join("rag-rat.toml").exists());
        assert!(symbol_indexed_in_checkout(&config, &main, "main_checkout_xyz"));
        Self { main, linked, config }
    }
}

#[cfg(unix)]
struct McpServer {
    child: Child,
    _stdin: std::process::ChildStdin,
    _reader: BufReader<std::process::ChildStdout>,
}

#[cfg(unix)]
impl McpServer {
    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Reap the spawned `mcp` process even if a test panics between spawn and the explicit
// `kill_and_wait` — otherwise a mid-test assert failure orphans the process (and its bound
// socket/lock) until the OS reaps it. Idempotent with `kill_and_wait`.
#[cfg(unix)]
impl Drop for McpServer {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

/// Decode the client's `hookSpecificOutput` JSON and pull out `additionalContext` (None when the
/// client printed nothing, i.e. a deduped/empty response).
#[cfg(unix)]
fn additional_context(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).expect("client emitted valid JSON");
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    value["hookSpecificOutput"]["additionalContext"].as_str().map(str::to_string)
}

/// Independently confirm the symbol is in the index, so a missing symbol can't make the e2e pass
/// vacuously (an empty context would otherwise look like a deduped no-op).
#[cfg(unix)]
fn assert_symbol_indexed(config: &rag_rat_base::config::Config, symbol: &str) {
    use rag_rat_db::storage::IndexConnection;
    let conn = IndexConnection::open_read_only(&config.database).unwrap();
    let count: i64 = conn
        .connection()
        .query_row("SELECT COUNT(*) FROM symbols WHERE name = ?1", [symbol], |row| row.get(0))
        .unwrap();
    assert!(count > 0, "index must contain the `{symbol}` symbol or the e2e is vacuous");
}

#[cfg(unix)]
fn symbol_indexed(config: &rag_rat_base::config::Config, symbol: &str) -> bool {
    use rag_rat_db::storage::IndexConnection;
    let Ok(conn) = IndexConnection::open_read_only(&config.database) else { return false };
    let count: i64 = conn
        .connection()
        .query_row("SELECT COUNT(*) FROM symbols WHERE name = ?1", [symbol], |row| row.get(0))
        .unwrap_or(0);
    count > 0
}

#[cfg(unix)]
fn wait_for_symbol(config: &rag_rat_base::config::Config, symbol: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if symbol_indexed(config, symbol) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("edited symbol `{symbol}` not indexed within {timeout:?}");
}

/// #661: the `edit-reindex` runner reconciles exactly the edited file — a deterministic check of the
/// scoped pass (run to completion, no detach) so the edit provably lands (new symbol in, old out).
#[cfg(unix)]
#[test]
fn edit_reindex_reconciles_the_edited_file() {
    let repo = TestRepo::indexed_with_symbol();
    fs::write(repo.root.join("src/lib.rs"), "pub fn added_after_edit_qrs() {}\n").unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .arg("edit-reindex")
        .arg("--cwd")
        .arg(&*repo.root)
        .arg("--paths")
        .arg(repo.root.join("src/lib.rs"))
        .current_dir(&repo.root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        symbol_indexed(&repo.config, "added_after_edit_qrs"),
        "the edit's new symbol is indexed"
    );
    assert!(!symbol_indexed(&repo.config, "frobnicate_xyz"), "the replaced symbol is gone");
}

/// #661 end-to-end: a PostToolUse edit hook returns immediately (exit 0, silent) and backgrounds a
/// DETACHED scoped reindex that eventually lands the edit. No watcher is live, so the hook does the
/// job itself.
#[cfg(unix)]
#[test]
fn posttooluse_backgrounds_a_scoped_reindex() {
    let repo = TestRepo::indexed_with_symbol();
    fs::write(repo.root.join("src/lib.rs"), "pub fn added_via_hook_tuv() {}\n").unwrap();
    let input = serde_json::json!({
        "session_id": "s-post", "cwd": repo.root.as_path(), "hook_event_name": "PostToolUse",
        "tool_name": "Edit", "tool_input": {"file_path": repo.root.join("src/lib.rs")}
    });
    let (stdout, status) = run_hook(&input.to_string(), &repo.root);
    assert!(status.success(), "the hook must exit 0");
    assert!(stdout.is_empty(), "PostToolUse prints nothing, got: {stdout}");
    // The reindex runs in a detached child; poll until the edit lands.
    wait_for_symbol(&repo.config, "added_via_hook_tuv", Duration::from_secs(30));
}

#[cfg(unix)]
#[test]
fn cursor_and_vscode_payloads_emit_only_documented_context() {
    let repo = TestRepo::indexed_with_symbol();
    let cursor = serde_json::json!({
        "hook_event_name": "postToolUse",
        "conversation_id": "cursor-e2e",
        "cursor_version": "1.7.2",
        "workspace_roots": [repo.root.as_path()],
        "tool_name": "Shell",
        "tool_input": {"command": "rg frobnicate_xyz", "private": "RAW-PAYLOAD-SENTINEL"},
        "tool_output": "PRIVATE TOOL OUTPUT",
    });
    let (cursor_stdout, cursor_status) =
        run_hook_for(Some("cursor"), &cursor.to_string(), &repo.root);
    assert!(cursor_status.success());
    let cursor_output: serde_json::Value = serde_json::from_str(cursor_stdout.trim()).unwrap();
    assert!(cursor_output["additional_context"].as_str().unwrap().contains("frobnicate_xyz"));
    assert!(!cursor_stdout.contains("RAW-PAYLOAD-SENTINEL"));
    assert!(!cursor_stdout.contains("PRIVATE TOOL OUTPUT"));

    let vscode = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "vscode-e2e",
        "cwd": repo.root.as_path(),
        "tool_name": "run_in_terminal",
        "tool_input": {"command": "rg frobnicate_xyz", "private": "RAW-PAYLOAD-SENTINEL"},
        "transcript_path": "/private/transcript.jsonl",
    });
    let (vscode_stdout, vscode_status) =
        run_hook_for(Some("vscode"), &vscode.to_string(), &repo.root);
    assert!(vscode_status.success());
    let vscode_output: serde_json::Value = serde_json::from_str(vscode_stdout.trim()).unwrap();
    assert!(
        vscode_output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("frobnicate_xyz")
    );
    assert!(!vscode_stdout.contains("RAW-PAYLOAD-SENTINEL"));
    assert!(!vscode_stdout.contains("transcript.jsonl"));
}

#[cfg(unix)]
#[test]
fn cursor_and_vscode_read_payloads_emit_bounded_augmentation() {
    let repo = TestRepo::indexed_with_symbol();
    repo.add_file_memory("READ-AUGMENT-MARKER");
    let cursor = serde_json::json!({
        "hook_event_name": "postToolUse",
        "conversation_id": "cursor-read",
        "workspace_roots": [repo.root.as_path()],
        "tool_name": "Read",
        "tool_input": {"file_path": repo.root.join("src/lib.rs")},
    });
    let (stdout, status) = run_hook_for(Some("cursor"), &cursor.to_string(), &repo.root);
    assert!(status.success());
    let output: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(output["additional_context"].as_str().unwrap().contains("READ-AUGMENT-MARKER"));

    let vscode = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "session_id": "vscode-read",
        "cwd": repo.root.as_path(),
        "tool_name": "read_file",
        "tool_input": {"filePath": repo.root.join("src/lib.rs")},
    });
    let (stdout, status) = run_hook_for(Some("vscode"), &vscode.to_string(), &repo.root);
    assert!(status.success());
    let output: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("READ-AUGMENT-MARKER")
    );
}

#[cfg(unix)]
#[test]
fn cursor_and_vscode_session_start_wrap_the_orientation_digest() {
    let repo = TestRepo::indexed_with_symbol();
    let cursor = serde_json::json!({
        "hook_event_name": "sessionStart",
        "conversation_id": "cursor-start",
        "workspace_roots": [repo.root.as_path()],
        "composer_mode": "agent",
    });
    let (stdout, status) = run_hook_for(Some("cursor"), &cursor.to_string(), &repo.root);
    assert!(status.success());
    let output: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(output["additional_context"].as_str().unwrap().contains("rag-rat repo intelligence"));

    let vscode = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "vscode-start",
        "cwd": repo.root.as_path(),
        "source": "new",
    });
    let (stdout, status) = run_hook_for(Some("vscode"), &vscode.to_string(), &repo.root);
    assert!(status.success());
    let output: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("rag-rat repo intelligence")
    );

    let resumed = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "vscode-resume",
        "cwd": repo.root.as_path(),
        "source": "resume",
    });
    let (stdout, status) = run_hook_for(Some("vscode"), &resumed.to_string(), &repo.root);
    assert!(status.success());
    assert!(stdout.is_empty(), "resumed sessions must not receive another orientation digest");
}

#[cfg(unix)]
#[test]
fn cursor_after_file_edit_backgrounds_a_scoped_reindex() {
    let repo = TestRepo::indexed_with_symbol();
    fs::write(repo.root.join("src/lib.rs"), "pub fn cursor_edit_xyz() {}\n").unwrap();
    let input = serde_json::json!({
        "hook_event_name": "afterFileEdit",
        "conversation_id": "cursor-edit",
        "workspace_roots": ["/unrelated-workspace", repo.root.as_path()],
        "file_path": repo.root.join("src/lib.rs"),
        "edits": [{"old_string": "frobnicate_xyz", "new_string": "cursor_edit_xyz"}],
    });
    let (stdout, status) = run_hook_for(Some("cursor"), &input.to_string(), &repo.root);
    assert!(status.success());
    assert!(stdout.is_empty(), "edit reindex must not add AI context: {stdout}");
    wait_for_symbol(&repo.config, "cursor_edit_xyz", Duration::from_secs(30));
}

#[cfg(unix)]
#[test]
fn vscode_post_edit_backgrounds_a_scoped_reindex() {
    let repo = TestRepo::indexed_with_symbol();
    fs::write(repo.root.join("src/lib.rs"), "pub fn vscode_edit_xyz() {}\n").unwrap();
    let input = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "vscode-edit",
        "cwd": repo.root.as_path(),
        "tool_name": "replace_string_in_file",
        "tool_input": {"filePath": repo.root.join("src/lib.rs")},
        "tool_response": "File edited successfully",
    });
    let (stdout, status) = run_hook_for(Some("vscode"), &input.to_string(), &repo.root);
    assert!(status.success());
    assert!(stdout.is_empty(), "edit reindex must not add AI context: {stdout}");
    wait_for_symbol(&repo.config, "vscode_edit_xyz", Duration::from_secs(30));
}

#[cfg(unix)]
#[test]
fn cursor_and_vscode_reindex_only_the_active_linked_worktree() {
    let repo = LinkedTestRepo::indexed();
    let linked_file = repo.linked.join("src/lib.rs");

    fs::write(&linked_file, "pub fn cursor_linked_xyz() {}\n").unwrap();
    let cursor = serde_json::json!({
        "hook_event_name": "afterFileEdit",
        "conversation_id": "cursor-linked-edit",
        "workspace_roots": [repo.main.as_path(), repo.linked.as_path()],
        "file_path": &linked_file,
        "edits": [{"new_string": "pub fn cursor_linked_xyz() {}"}],
    });
    let (stdout, status) = run_hook_for(Some("cursor"), &cursor.to_string(), &repo.linked);
    assert!(status.success());
    assert!(stdout.is_empty());
    wait_for_checkout_symbol(
        &repo.config,
        &repo.linked,
        "cursor_linked_xyz",
        Duration::from_secs(30),
    );
    assert!(symbol_indexed_in_checkout(&repo.config, &repo.main, "main_checkout_xyz"));
    assert!(!symbol_indexed_in_checkout(&repo.config, &repo.main, "cursor_linked_xyz"));

    fs::write(&linked_file, "pub fn vscode_linked_xyz() {}\n").unwrap();
    let vscode = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "session_id": "vscode-linked-edit",
        "cwd": repo.linked.as_path(),
        "tool_name": "multi_replace_string_in_file",
        "tool_input": {
            "replacements": [{
                "filePath": &linked_file,
                "newString": "pub fn vscode_linked_xyz() {}",
            }],
        },
    });
    let (stdout, status) = run_hook_for(Some("vscode"), &vscode.to_string(), &repo.linked);
    assert!(status.success());
    assert!(stdout.is_empty());
    wait_for_checkout_symbol(
        &repo.config,
        &repo.linked,
        "vscode_linked_xyz",
        Duration::from_secs(30),
    );
    assert!(!symbol_indexed_in_checkout(&repo.config, &repo.linked, "cursor_linked_xyz"));
    assert!(symbol_indexed_in_checkout(&repo.config, &repo.main, "main_checkout_xyz"));
    assert!(!symbol_indexed_in_checkout(&repo.config, &repo.main, "vscode_linked_xyz"));
}

#[cfg(unix)]
fn symbol_indexed_in_checkout(
    config: &rag_rat_base::config::Config,
    checkout: &Path,
    symbol: &str,
) -> bool {
    let mut db = rag_rat_core::IndexDatabase::open_config(config).unwrap();
    db.use_worktree_scope(&config.root, Some(checkout)).unwrap();
    db.symbols(symbol, None, 10).unwrap().iter().any(|candidate| candidate.name == symbol)
}

#[cfg(unix)]
fn wait_for_checkout_symbol(
    config: &rag_rat_base::config::Config,
    checkout: &Path,
    symbol: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if symbol_indexed_in_checkout(config, checkout, symbol) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("edited symbol `{symbol}` not indexed for {} within {timeout:?}", checkout.display());
}

#[cfg(unix)]
fn wait_for(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("expected socket {} within {timeout:?}", path.display());
}

#[cfg(unix)]
fn wait_until_gone(path: &Path, timeout: Duration) {
    use std::os::unix::net::UnixStream;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // A bound, accepting listener answers connect(); once the server dies the socket file may
        // linger but connect() refuses — that is when the client provably falls back.
        if UnixStream::connect(path).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("listener at {} still accepting after {timeout:?}", path.display());
}

/// #484: when the index schema was created by a newer rag-rat (one upgraded agent migrated the
/// shared DB; this hook binary — and therefore this session's MCP server — is older), the
/// session-start digest must become an actionable version-skew notice instead of going silent.
#[test]
fn session_start_warns_when_the_index_schema_is_newer() {
    let dir = unique_dir("hook-newer-schema");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn skew_probe() {}\n").unwrap();
    std::fs::write(
        dir.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let config = rag_rat_base::config::Config::load(dir.join("rag-rat.toml")).unwrap();
    rag_rat_core::IndexDatabase::rebuild(&config).unwrap();

    // Make the schema read as created-by-a-future-rag-rat: an unrecognized migration id.
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    conn.execute_batch(
        "INSERT INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES ('999_future_schema', 1, 'sha256:future', 'future schema');",
    )
    .unwrap();
    drop(conn);

    let input = serde_json::json!({
        "session_id": "s-skew", "cwd": dir.as_path(), "hook_event_name": "SessionStart",
        "source": "startup"
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success(), "the hook must never block session start");
    assert!(
        stdout.contains("newer rag-rat") && stdout.contains("upgrade rag-rat"),
        "expected an actionable version-skew notice, got: {stdout:?}"
    );
}
