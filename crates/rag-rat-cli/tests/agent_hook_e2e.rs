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
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

fn run_hook(stdin_body: &str, cwd: &std::path::Path) -> (String, std::process::ExitStatus) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
        .arg("agent-hook")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.stdin.as_mut().unwrap().write_all(stdin_body.as_bytes());
    let out = child.wait_with_output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status)
}

#[test]
fn no_rag_rat_toml_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-noindex-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir, "hook_event_name": "PreToolUse",
        "tool_name": "Grep", "tool_input": {"pattern": "anything"}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success());
    assert!(stdout.is_empty(), "must print nothing without an index, got: {stdout}");
}

#[test]
fn garbage_stdin_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-garbage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (stdout, status) = run_hook("this is not json", &dir);
    assert!(status.success());
    assert!(stdout.is_empty());
}

#[test]
fn non_search_tool_means_silent_exit_zero() {
    let dir = std::env::temp_dir().join(format!("ragrat-hook-read-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = serde_json::json!({
        "session_id": "s1", "cwd": dir, "hook_event_name": "PreToolUse",
        "tool_name": "Read", "tool_input": {"path": "/x"}
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success());
    assert!(stdout.is_empty());
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

    repo.cleanup();
}

#[cfg(unix)]
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
struct TestRepo {
    root: PathBuf,
    config_path: PathBuf,
    config: rag_rat_base::config::Config,
}

#[cfg(unix)]
impl TestRepo {
    /// A temp repo with one Rust source file carrying a distinctive symbol, indexed in-process via
    /// `IndexDatabase::rebuild` (the same path `mcp_stdio`/`mcp_hot_upgrade` use to populate a real
    /// index). Unique per run so parallel tests never collide on the socket election lock.
    fn indexed_with_symbol() -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("ragrat-hook-e2e-{}-{id}", std::process::id()));
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
            "session_id": session_id, "cwd": self.root, "hook_event_name": "PreToolUse",
            "tool_name": "Grep", "tool_input": {"pattern": "frobnicate_xyz"}
        });
        let (stdout, status) = run_hook(&input.to_string(), &self.root);
        assert!(status.success(), "agent-hook must exit zero on every path");
        stdout
    }

    fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.root);
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
    let dir = std::env::temp_dir().join(format!("ragrat-hook-newer-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
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
        "session_id": "s-skew", "cwd": dir, "hook_event_name": "SessionStart",
        "source": "startup"
    });
    let (stdout, status) = run_hook(&input.to_string(), &dir);
    assert!(status.success(), "the hook must never block session start");
    assert!(
        stdout.contains("newer rag-rat") && stdout.contains("upgrade rag-rat"),
        "expected an actionable version-skew notice, got: {stdout:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
