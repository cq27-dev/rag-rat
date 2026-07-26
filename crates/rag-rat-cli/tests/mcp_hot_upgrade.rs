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

    fn send_sigusr1(&self) {
        let pid = self.child.id();
        let status = Command::new("kill").arg("-USR1").arg(pid.to_string()).status().unwrap();
        assert!(status.success(), "failed to deliver SIGUSR1 to {pid}");
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
