//! Unix-socket listener serving the Claude Code grep-augmentation PreToolUse hook.
//!
//! One listener per worktree (socket election lock); newline-delimited JSON, one request per
//! connection; per-session dedupe in memory. Read-only on the index by construction.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// One grep-augment query from a hook client. Unknown fields are ignored (forward compat);
/// unknown `v`/`kind` get a null-context reply rather than an error.
#[derive(Debug, Deserialize)]
pub struct HookRequest {
    pub v: u32,
    pub kind: String,
    pub session_id: String,
    pub pattern: String,
    #[serde(default)]
    pub search_path: Option<String>,
    #[serde(default)]
    pub source: String,
    /// The session's working directory, so the listener scopes the augmentation to that worktree's
    /// branch overlay (#219). Absent (older client) → base scope.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HookResponse {
    pub v: u32,
    pub context: Option<String>,
}

#[cfg(unix)]
pub use listener::{socket_path_for, spawn_listener};

#[cfg(unix)]
mod listener {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use rag_rat_core::config::Config;
    use rag_rat_core::locks::{self, FileLock};
    use rag_rat_core::query::grep_augment::{self, DedupeFilter};
    use rag_rat_core::storage::IndexConnection;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::task::JoinHandle;

    use super::{HookRequest, HookResponse, PROTOCOL_VERSION};

    const ELECTION_RETRY: Duration = Duration::from_secs(5);
    const SESSION_CAP: usize = 64;
    const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    /// > client's 250 ms timeout; a stalled peer cannot wedge the serialized accept loop.
    const READ_BUDGET: Duration = Duration::from_millis(500);

    pub fn socket_path_for(config: &Config) -> PathBuf {
        locks::hook_socket_path_for(config)
    }

    fn socket_lock_path_for(config: &Config) -> PathBuf {
        locks::hook_socket_lock_path_for(config)
    }

    /// Per-session record of what was already injected. Pruned by LRU cap + TTL.
    #[derive(Default)]
    struct SessionState {
        filter: DedupeFilter,
        last_used: Option<Instant>,
    }

    /// Spawn the hook listener task: win the socket election (retrying forever, like the
    /// watcher), then accept hook clients until the task is dropped. Returns the JoinHandle so
    /// the server can abort it on teardown; the lock and socket release with the process.
    pub fn spawn_listener(config: Config) -> JoinHandle<()> {
        tokio::spawn(async move {
            let lock_path = socket_lock_path_for(&config);
            let socket = socket_path_for(&config);
            if let Some(parent) = socket.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Win the socket election, then bind. The lock must live inside this task: aborting the
            // task drops it, so a surviving process's retry loop can take over (election,
            // watcher-identical). A bind that fails AFTER winning the election (a transient
            // FS/permissions hiccup) must not strand the worktree serving nothing while holding the
            // lock (#53): drop the election so a sibling can try, back off, and re-elect — the same
            // die→next-process-takes-over model, made resilient for the single-process case too.
            let (_lock, listener): (FileLock, UnixListener) = loop {
                let lock = loop {
                    match FileLock::try_acquire(&lock_path) {
                        Ok(Some(lock)) => break lock,
                        _ => tokio::time::sleep(ELECTION_RETRY).await,
                    }
                };
                // Only the lock holder ever unlinks: race-free stale-socket cleanup.
                let _ = std::fs::remove_file(&socket);
                match UnixListener::bind(&socket) {
                    Ok(listener) => break (lock, listener),
                    Err(_) => {
                        drop(lock);
                        tokio::time::sleep(ELECTION_RETRY).await;
                    },
                }
            };
            let mut sessions: HashMap<String, SessionState> = HashMap::new();
            loop {
                let Ok((stream, _addr)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                prune_sessions(&mut sessions);
                if let Err(err) = serve_one(stream, &config, &mut sessions).await
                    && std::env::var_os("RAG_RAT_HOOK_DEBUG").is_some()
                {
                    eprintln!("claude-hook listener: {err:#}");
                }
            }
        })
    }

    /// Drop sessions idle past the TTL, then evict least-recently-used down to the cap.
    fn prune_sessions(sessions: &mut HashMap<String, SessionState>) {
        let now = Instant::now();
        sessions.retain(|_, s| s.last_used.is_some_and(|t| now.duration_since(t) < SESSION_TTL));
        while sessions.len() > SESSION_CAP {
            let oldest = sessions.iter().min_by_key(|(_, s)| s.last_used).map(|(k, _)| k.clone());
            let Some(key) = oldest else { break };
            sessions.remove(&key);
        }
    }

    /// One request per connection: read a line, compose (read-only DB, off the runtime
    /// threads), record what was injected for the session, reply with one line.
    async fn serve_one(
        stream: UnixStream,
        config: &Config,
        sessions: &mut HashMap<String, SessionState>,
    ) -> anyhow::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        match tokio::time::timeout(READ_BUDGET, BufReader::new(read).read_line(&mut line)).await {
            Err(_elapsed) => return Ok(()), // stalled peer — drop, keep loop live
            Ok(io) => {
                io?;
            }, // propagate real I/O errors as before
        }
        let reply = match serde_json::from_str::<HookRequest>(&line) {
            Ok(req) if req.v == PROTOCOL_VERSION && req.kind == "grep_augment" => {
                let filter = {
                    let state = sessions.entry(req.session_id.clone()).or_default();
                    state.last_used = Some(Instant::now());
                    // Clone ends the borrow before the spawn_blocking await below.
                    state.filter.clone()
                };
                let database = config.database.clone();
                let config_root = config.root.clone();
                // Scope to the session's worktree overlay (#219). Absent cwd (older client) → the
                // config root, which resolves to the base scope. This also fixes the listener's
                // prior lack of ANY scope install: compose queries the `files` view, so without
                // this it read raw, unscoped rows.
                let cwd = req.cwd.clone().map(PathBuf::from).unwrap_or_else(|| config_root.clone());
                let pattern = req.pattern.clone();
                let search_path = req.search_path.clone();
                // rusqlite is sync; one short read off the runtime threads.
                let composed = tokio::task::spawn_blocking(move || {
                    let conn = IndexConnection::open_read_only(&database)?;
                    rag_rat_core::index::install_worktree_scope_view(
                        conn.connection(),
                        &config_root,
                        &cwd,
                    )?;
                    grep_augment::compose(
                        conn.connection(),
                        &pattern,
                        search_path.as_deref(),
                        &filter,
                    )
                })
                .await??;
                match composed {
                    Some(out) => {
                        // compose's returned IDs are exactly the items rendered, so the
                        // session filter grows by exactly what the model now has.
                        let state = sessions.entry(req.session_id).or_default();
                        state.filter.memory_ids.extend(out.memory_ids.iter().cloned());
                        state.filter.symbol_keys.extend(out.symbol_keys.iter().cloned());
                        HookResponse { v: PROTOCOL_VERSION, context: Some(out.context) }
                    },
                    None => HookResponse { v: PROTOCOL_VERSION, context: None },
                }
            },
            // Unknown v/kind or malformed JSON: answer null-context, never error back.
            _ => HookResponse { v: PROTOCOL_VERSION, context: None },
        };
        let mut payload = serde_json::to_string(&reply)?;
        payload.push('\n');
        write.write_all(payload.as_bytes()).await?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod listener_tests {
    use std::time::Duration;

    use rag_rat_core::Config;
    use rag_rat_core::index::schema;
    use rag_rat_core::storage::IndexConnection;

    use super::*;

    /// Build a Config rooted at a fresh temp dir with a seeded on-disk index. The toml shape
    /// mirrors `mcp_hot_upgrade.rs`'s `TestEnv::setup`, minus targets (the listener never
    /// indexes). The DB is created at `config.database` *after* `Config::load` so root
    /// canonicalization can't desync the two paths.
    fn test_config() -> Config {
        let root = std::env::temp_dir().join(format!(
            "ragrat-hooksock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config_path = root.join("rag-rat.toml");
        std::fs::write(&config_path, "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.db\"\n")
            .unwrap();
        let config = Config::load(&config_path).unwrap();

        let rw = IndexConnection::open(&config.database).unwrap();
        schema::apply(rw.connection()).unwrap();
        // Seed at the scope a NON-git index uses (commit_sha '', worktree_id = the root), so the
        // listener's worktree-scoped read (#219) surfaces the file — the listener now installs the
        // scope view (resolve_worktree_scope → an absent request cwd resolves to config.root → base
        // scope), where the overlay branch keys on `worktree_id`.
        rw.connection()
            .execute(
                &format!(
                    "INSERT INTO files(path, language, kind, sha256, modified_at_ms, \
                     indexed_at_ms, worktree_id)
                     VALUES ('src/lib.rs', 'rust', 'source', 'abc', 0, 0, '{}')",
                    config.root.to_string_lossy()
                ),
                [],
            )
            .unwrap();
        rw.connection()
            .execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('lib::frobnicate')", [])
            .unwrap();
        rw.connection()
            .execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', 'frobnicate',
                         (SELECT id FROM name_strings WHERE value = 'lib::frobnicate'),
                         'function', 0, 10, 'fn frobnicate()', NULL)",
                [],
            )
            .unwrap();
        // No rebuild_fts here: external-content FTS5 rebuilds corrupt when out of sync with
        // direct seeding, and the symbol lane needs no FTS rows.
        config
    }

    async fn request(socket: &std::path::Path, body: serde_json::Value) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(format!("{body}\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// Fallible `request`: `None` on any connect/write/read failure or an empty read. During a
    /// listener takeover the connection can be reset or closed mid-handoff while the new owner is
    /// still binding (the race widens under llvm-cov instrumentation — #84), which means "not
    /// serving yet, retry," not a test failure. Callers must use a FRESH session per attempt so a
    /// half-completed attempt can't dedupe the retry to a null context.
    async fn try_request(
        socket: &std::path::Path,
        body: serde_json::Value,
    ) -> Option<serde_json::Value> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = tokio::net::UnixStream::connect(socket).await.ok()?;
        let (read, mut write) = stream.into_split();
        write.write_all(format!("{body}\n").as_bytes()).await.ok()?;
        let mut line = String::new();
        if BufReader::new(read).read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        serde_json::from_str(&line).ok()
    }

    #[tokio::test]
    async fn listener_serves_context_then_dedupes_per_session() {
        let config = test_config();
        let _listener = spawn_listener(config.clone());
        let socket = socket_path_for(&config);
        // Election + bind are async; poll for the socket.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !socket.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let req = |sid: &str| {
            serde_json::json!({"v": 1, "kind": "grep_augment", "session_id": sid,
                               "pattern": "frobnicate", "search_path": null,
                               "source": "grep_tool"})
        };
        let first = request(&socket, req("s1")).await;
        assert!(first["context"].as_str().unwrap().contains("lib::frobnicate"));
        let second = request(&socket, req("s1")).await;
        assert!(second["context"].is_null(), "same session deduped");
        let other = request(&socket, req("s2")).await;
        assert!(
            other["context"].as_str().unwrap().contains("lib::frobnicate"),
            "fresh session not deduped"
        );
        let bad = request(&socket, serde_json::json!({"v": 99, "kind": "nope"})).await;
        assert!(bad["context"].is_null(), "unknown version answered, not errored");
    }

    #[tokio::test]
    async fn second_listener_takes_over_when_winner_dies() {
        let config = test_config();
        let winner = spawn_listener(config.clone());
        let socket = socket_path_for(&config);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !socket.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The loser parks in the election retry loop while the winner holds the lock.
        let loser = spawn_listener(config.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!loser.is_finished(), "loser must wait, not exit");

        // Kill the winner: its lock fd and bound socket drop with the task's process state.
        winner.abort();
        let _ = winner.await;
        // NOTE: in-process abort drops the FileLock (held by the task) but the dead socket
        // file remains — exactly the stale-socket case. The loser must unlink + re-bind.
        // Election retry is 5s; poll until the loser owns the socket and serves a real reply. The
        // connection can be reset/closed mid-handoff while the loser is still binding, so use the
        // fallible `try_request` with a FRESH session per attempt (so a half-completed attempt
        // never dedupes the retry) rather than unwrap()-ing the first read (#84).
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let req = serde_json::json!({"v": 1, "kind": "grep_augment",
                                         "session_id": format!("takeover-{attempt}"),
                                         "pattern": "frobnicate", "search_path": null,
                                         "source": "grep_tool"});
            if let Some(reply) = try_request(&socket, req).await
                && reply["context"].as_str().is_some_and(|c| c.contains("lib::frobnicate"))
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "loser never took over the socket");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        loser.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_and_tolerates_unknown_fields() {
        let json = r#"{"v":1,"kind":"grep_augment","session_id":"s1","pattern":"foo",
                       "search_path":null,"source":"grep_tool","future_field":true}"#;
        let req: HookRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.v, 1);
        assert_eq!(req.kind, "grep_augment");
        assert_eq!(req.pattern, "foo");
        assert!(req.search_path.is_none());
    }

    #[test]
    fn response_serializes_null_context_explicitly() {
        let resp = HookResponse { v: 1, context: None };
        assert_eq!(serde_json::to_string(&resp).unwrap(), r#"{"v":1,"context":null}"#);
    }
}
