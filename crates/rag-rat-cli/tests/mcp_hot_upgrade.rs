//! Acceptance tests for the `SIGUSR1` hot-upgrade of the stdio MCP server (Unix only).
//!
//! These spawn the real `rag-rat mcp` binary, drive a JSON-RPC session over its stdio pipes, send
//! `SIGUSR1`, and assert the documented end-to-end behavior: a transparent in-place `exec` that
//! resumes the session without a fresh `initialize`, and a clean non-zero exit when the configured
//! upgrade binary can't be `exec`'d.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rag_rat_base::config::Config;
use serde_json::{Value, json};

mod common;

use common::{ScratchRoot, unique_dir};

/// SIGUSR1: drain at a boundary, hand off, `exec` the new binary in place, and resume serving the
/// same session — the next tool call succeeds with no intervening `initialize`.
#[test]
fn sigusr1_hot_upgrade_resumes_session_in_place() {
    let env = TestEnv::setup();
    // The "new binary" is a wrapper that records that it ran, then `exec`s the real binary with the
    // original argv — standing in for a freshly `cargo install`ed copy.
    let sentinel = env.root.join("upgraded.marker");
    let wrapper = env.write_upgrade_wrapper(&sentinel);

    let mut session = env.spawn_mcp(&[("RAG_RAT_UPGRADE_BIN", wrapper.to_str().unwrap())]);
    session.initialize();

    let before = session.call_semantic_search();
    assert!(before.contains("chunk_id"), "tool call works before upgrade: {before}");

    session.send_sigusr1();
    wait_for(&sentinel, Duration::from_secs(20)); // the wrapper ran ⇒ we `exec`'d the new binary.

    // No re-`initialize`: a cold server would reject this. A successful result proves the resumed
    // process skipped the handshake via `serve_directly`.
    let after = session.call_semantic_search();
    assert!(after.contains("chunk_id"), "session resumed transparently after exec: {after}");

    // The grep-augment hook socket must answer *after* the in-place `exec`: the old process's lock
    // fd and bound socket died with the exec'd image, so the resumed process has to re-run the
    // socket election and re-bind the listener (the `AbortOnDrop` guard + the election retry loop).
    // Probing it proves `spawn_listener` runs on the handoff-resume path, not just cold start.
    let socket = env.hook_socket_path();
    let reply = hook_roundtrip(&socket, "sqlite", Duration::from_secs(15));
    assert_eq!(reply["v"], 1, "hook socket re-binds and speaks the protocol after hot-upgrade");

    session.stop();
}

/// When the configured upgrade binary can't be `exec`'d, the process tears down and exits non-zero
/// (the client then sees EOF and relaunches) rather than wedging.
#[test]
fn sigusr1_exec_failure_exits_nonzero() {
    let env = TestEnv::setup();
    let missing = env.root.join("does-not-exist-rag-rat");

    let mut session = env.spawn_mcp(&[("RAG_RAT_UPGRADE_BIN", missing.to_str().unwrap())]);
    session.initialize();
    let _ = session.call_semantic_search();

    session.send_sigusr1();
    let status = session.wait_for_exit(Duration::from_secs(20));
    assert!(!status.success(), "exec failure must exit non-zero, got {status:?}");
}

/// `fleet::trigger` selects its targets by reading other processes' environ: a `rag-rat mcp`
/// carrying `RAG_RAT_UPGRADE_BIN` is taken as proof that a `SIGUSR1` handler is installed, and is
/// signaled on that basis. A server carrying that variable must therefore NEVER leave the signal on
/// its default disposition, which terminates the process — mid-request, and with no diagnostic —
/// instead of upgrading it.
///
/// This covers the two states where there is nothing to upgrade *to*: before the client has said
/// anything at all, and after it opened a session through the stateless lifecycle (protocol
/// 2026-07-28 drops `initialize` and carries the version + capabilities in each request's `_meta`,
/// so the server never learns the peer info an `exec` handoff would need). Both must absorb the
/// signal and keep serving.
#[test]
fn sigusr1_is_survivable_before_and_without_an_initialize_handshake() {
    let env = TestEnv::setup();
    let sentinel = env.root.join("must-not-run.marker");
    let wrapper = env.write_upgrade_wrapper(&sentinel);

    let mut session = env.spawn_mcp(&[("RAG_RAT_UPGRADE_BIN", wrapper.to_str().unwrap())]);

    // (1) Signal arrives while the server is still blocked waiting for the client's first message.
    // `ping` is the one request MCP permits before `initialize`, so it proves the server is up and
    // serving without moving it out of the pre-handshake state the signal has to survive.
    session.ping();
    session.send_sigusr1();
    session.assert_still_running("pre-handshake SIGUSR1 must not terminate the server");

    // (2) The client then opens a session WITHOUT `initialize`. Serving must be unaffected.
    let text = session.call_semantic_search_stateless();
    assert!(text.contains("chunk_id"), "stateless session serves tool calls: {text}");

    // (3) Signal again, now that the server knows the session has no handoff state.
    session.send_sigusr1();
    session.assert_still_running("SIGUSR1 on a stateless session must not terminate the server");
    let text = session.call_semantic_search_stateless();
    assert!(text.contains("chunk_id"), "stateless session still serves after SIGUSR1: {text}");

    assert!(!sentinel.exists(), "a session with no handoff state must not `exec` the new binary");
    session.stop();
}

/// The dormant server (launched outside any indexed repo) never hot-upgrades — with no config
/// there is no handoff directory. It is still a `rag-rat mcp` process that a globally-registered
/// launcher hands `RAG_RAT_UPGRADE_BIN`, so it is a fleet target like any other and must absorb
/// `SIGUSR1` rather than die on it.
#[test]
fn sigusr1_does_not_kill_a_dormant_server() {
    let elsewhere = unique_dir("hot-upgrade-dormant");
    fs::create_dir_all(&*elsewhere).unwrap();
    let installed = elsewhere.join("rag-rat-installed");
    fs::write(&installed, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();

    let mut session = Session::spawn_dormant(&elsewhere, &installed);
    let notice = session.call_semantic_search_stateless();
    assert!(notice.contains("no_index"), "server is dormant: {notice}");

    session.send_sigusr1();
    session.assert_still_running("SIGUSR1 must not terminate a dormant server");
    let notice = session.call_semantic_search_stateless();
    assert!(notice.contains("no_index"), "dormant server still serves after SIGUSR1: {notice}");
    session.stop();
}

struct TestEnv {
    root: ScratchRoot,
    config_path: PathBuf,
    config: Config,
}

impl TestEnv {
    fn setup() -> Self {
        let root = unique_dir("hot-upgrade");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/search.md"), "# Search\n\nSemantic recall uses sqlite.\n")
            .unwrap();
        let config_path = root.join("rag-rat.toml");
        fs::write(
            &config_path,
            "[index]\nroot = \".\"\ndatabase = \
             \".rag-rat/index.sqlite\"\n\n[target_bindings]\nmarkdown = [\"docs\"]\n",
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        rag_rat_core::IndexDatabase::rebuild(&config).unwrap();
        Self { root, config_path, config }
    }

    /// The deterministic hook-socket path for this config — the same derivation the elected
    /// listener and the CLI client use (`hook_socket_path_for` handles the XDG/temp budget
    /// cascade), so the probe can't drift from where the server actually binds.
    fn hook_socket_path(&self) -> PathBuf {
        rag_rat_base::locks::hook_socket_path_for(&self.config)
    }

    /// A `/bin/sh` wrapper that touches `sentinel` then `exec`s the real binary with our argv —
    /// the handoff env (`MCP_HANDOFF_PATH`) is inherited across both `exec`s.
    fn write_upgrade_wrapper(&self, sentinel: &Path) -> PathBuf {
        let real_binary = env!("CARGO_BIN_EXE_rag-rat");
        let wrapper = self.root.join("rag-rat-upgraded");
        fs::write(
            &wrapper,
            format!("#!/bin/sh\ntouch '{}'\nexec '{}' \"$@\"\n", sentinel.display(), real_binary),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        wrapper
    }

    fn spawn_mcp(&self, envs: &[(&str, &str)]) -> Session {
        let binary = env!("CARGO_BIN_EXE_rag-rat");
        let mut command = Command::new(binary);
        command
            .arg("mcp")
            .arg("--config")
            .arg(&self.config_path)
            // Disable the background watcher so its passes don't perturb the upgrade timing; we are
            // exercising the SIGUSR1 self-upgrade path, not the fleet trigger.
            .env("RAG_RAT_NO_WATCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        Session { child, stdin, reader, next_id: 1 }
    }
}

struct Session {
    child: Child,
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Session {
    fn initialize(&mut self) {
        let id = self.take_id();
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "rag-rat-upgrade-test", "version": "0.1"}
            }
        }));
        let response = self.recv();
        assert_eq!(response["id"], id, "initialize response");
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}));
    }

    /// Issue a `semantic_search` tool call and return its raw text result. MCP results are rendered
    /// as TOON (not JSON), so callers assert on the text rather than parsing it as JSON.
    fn call_semantic_search(&mut self) -> String {
        let id = self.take_id();
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "semantic_search", "arguments": {"query": "search", "limit": 1}}
        }));
        let response = self.recv();
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool call had no text result: {response}"))
            .to_string()
    }

    /// Issue a `semantic_search` through the STATELESS lifecycle — no `initialize`, with the
    /// protocol version and client capabilities riding in the request's own `_meta`. A server
    /// reached this way never learns its peer's `initialize` params.
    fn call_semantic_search_stateless(&mut self) -> String {
        let id = self.take_id();
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo":
                        {"name": "rag-rat-upgrade-test", "version": "0.1"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                },
                "name": "semantic_search",
                "arguments": {"query": "search", "limit": 1}
            }
        }));
        let response = self.recv();
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool call had no text result: {response}"))
            .to_string()
    }

    /// A dormant server: no `--config`, and a working directory with no config to discover, so the
    /// server activates nothing and every `tools/call` returns the `no_index` notice.
    fn spawn_dormant(cwd: &Path, install_path: &Path) -> Session {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rag-rat"))
            .arg("mcp")
            .current_dir(cwd)
            .env("RAG_RAT_UPGRADE_BIN", install_path)
            .env("RAG_RAT_NO_WATCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        Session { child, stdin, reader, next_id: 1 }
    }

    /// The one request MCP permits before `initialize`. Used to prove the server is up and
    /// serving while still in the pre-handshake state.
    fn ping(&mut self) {
        let id = self.take_id();
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "ping", "params": {}}));
        let response = self.recv();
        assert_eq!(response["id"], id, "ping response");
    }

    fn send_sigusr1(&self) {
        let pid = self.child.id();
        let status = Command::new("kill").arg("-USR1").arg(pid.to_string()).status().unwrap();
        assert!(status.success(), "failed to deliver SIGUSR1 to {pid}");
    }

    /// Give the signal time to be delivered and acted on, then assert the process is still up.
    /// A default-disposition `SIGUSR1` kills within microseconds, so a short settle is enough;
    /// the follow-up tool call is what proves it is still *serving*, not merely un-reaped.
    fn assert_still_running(&mut self, what: &str) {
        std::thread::sleep(Duration::from_millis(750));
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "{what} (exited with {:?})",
            self.child.try_wait().unwrap()
        );
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("process did not exit within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn send(&mut self, value: Value) {
        writeln!(self.stdin, "{}", serde_json::to_string(&value).unwrap()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).unwrap();
        assert!(read > 0, "mcp server closed stdout");
        serde_json::from_str(&line).unwrap()
    }

    fn take_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Connect to the hook socket and exchange one `grep_augment` request/reply, polling until a bound
/// listener answers (the post-`exec` process must win the socket election before it re-binds, which
/// the 5s election retry can stretch out). Returns the decoded reply envelope; the caller asserts
/// on `v`. We deliberately do not couple to the `context` payload — the point is "the socket is
/// alive and speaks the protocol after exec", not what it found for `pattern`.
fn hook_roundtrip(socket: &Path, pattern: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut writer = stream.try_clone().unwrap();
            let request = json!({
                "v": 1, "kind": "grep_augment", "session_id": "upgrade-probe",
                "pattern": pattern, "search_path": null, "source": "grep_tool",
            });
            if writeln!(writer, "{request}").is_ok() {
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_ok() && !line.is_empty() {
                    return serde_json::from_str(&line).unwrap();
                }
            }
        }
        assert!(Instant::now() < deadline, "hook socket never answered after upgrade: {socket:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("expected {} within {timeout:?}", path.display());
}
