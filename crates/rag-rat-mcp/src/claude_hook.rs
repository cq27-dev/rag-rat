//! Unix-socket listener serving the Claude Code grep-augmentation PreToolUse hook.
//!
//! One listener per worktree (socket election lock); newline-delimited JSON, one request per
//! connection; per-session dedupe in memory. Read-only on the index by construction. Spec:
//! `docs/specs/2026-06-09-grep-augment-pretooluse-hook.md`.

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
    use std::{
        collections::HashMap,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use rag_rat_core::{
        config::Config,
        locks::{self, FileLock},
        query::grep_augment::{self, DedupeFilter},
        storage::IndexConnection,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        task::JoinHandle,
    };

    use super::{HookRequest, HookResponse, PROTOCOL_VERSION};

    const ELECTION_RETRY: Duration = Duration::from_secs(5);
    const SESSION_CAP: usize = 64;
    const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

    /// The shared base dir the socket and its election lock key off: the index DB's directory
    /// (shared across a repo's worktrees), falling back to the worktree root.
    fn socket_base_dir(config: &Config) -> PathBuf {
        config
            .database
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| config.root.clone())
    }

    pub fn socket_path_for(config: &Config) -> PathBuf {
        locks::hook_socket_path(&socket_base_dir(config), &config.root)
    }

    fn socket_lock_path_for(config: &Config) -> PathBuf {
        locks::socket_lock_path(&socket_base_dir(config), &config.root)
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
            // The lock must live inside this task: aborting the task drops it, so a
            // surviving process's retry loop can take over (election, watcher-identical).
            let _lock: FileLock = loop {
                match FileLock::try_acquire(&lock_path) {
                    Ok(Some(lock)) => break lock,
                    _ => tokio::time::sleep(ELECTION_RETRY).await,
                }
            };
            let socket = socket_path_for(&config);
            if let Some(parent) = socket.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Only the lock holder ever unlinks: race-free stale-socket cleanup.
            let _ = std::fs::remove_file(&socket);
            let Ok(listener) = UnixListener::bind(&socket) else { return };
            let mut sessions: HashMap<String, SessionState> = HashMap::new();
            loop {
                let Ok((stream, _addr)) = listener.accept().await else { continue };
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
        BufReader::new(read).read_line(&mut line).await?;
        let reply = match serde_json::from_str::<HookRequest>(&line) {
            Ok(req) if req.v == PROTOCOL_VERSION && req.kind == "grep_augment" => {
                let filter = {
                    let state = sessions.entry(req.session_id.clone()).or_default();
                    state.last_used = Some(Instant::now());
                    // Clone ends the borrow before the spawn_blocking await below.
                    state.filter.clone()
                };
                let database = config.database.clone();
                let pattern = req.pattern.clone();
                let search_path = req.search_path.clone();
                // rusqlite is sync; one short read off the runtime threads.
                let composed = tokio::task::spawn_blocking(move || {
                    let conn = IndexConnection::open_read_only(&database)?;
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

    use rag_rat_core::{Config, index::schema, storage::IndexConnection};

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
        rw.connection()
            .execute(
                "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
                 VALUES ('src/lib.rs', 'rust', 'source', 'abc', 0, 0)",
                [],
            )
            .unwrap();
        rw.connection()
            .execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name, kind, start_byte,
                                     end_byte, signature, docs)
                 VALUES (1, 'rust', 'frobnicate', 'lib::frobnicate', 'function', 0, 10,
                         'fn frobnicate()', NULL)",
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
        let req = serde_json::json!({"v": 1, "kind": "grep_augment", "session_id": "takeover",
                                     "pattern": "frobnicate", "search_path": null,
                                     "source": "grep_tool"});
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            // Election retry is 5s; poll until the loser owns the socket and answers.
            if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {
                drop(stream);
                let reply = request(&socket, req.clone()).await;
                assert!(reply["context"].as_str().unwrap().contains("lib::frobnicate"));
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
