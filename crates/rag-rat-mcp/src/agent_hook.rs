//! Unix-socket listener serving the Claude Code grep-augmentation PreToolUse hook.
//!
//! One listener per worktree (socket election lock); newline-delimited JSON, one request per
//! connection; per-session dedupe in memory. Read-only on the index by construction.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// One augmentation query from a hook client. `kind` selects the lane: `"grep_augment"` uses
/// `pattern` (+ `search_path`), `"read_augment"` uses `path` (#756). Unknown fields are ignored
/// (forward compat); unknown `v`/`kind` get a null-context reply rather than an error.
#[derive(Debug, Deserialize)]
pub struct HookRequest {
    pub v: u32,
    pub kind: String,
    pub session_id: String,
    /// The grep pattern (`grep_augment` only). Defaulted so a `read_augment` request without it
    /// still deserializes.
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub search_path: Option<String>,
    /// The root-relative file path being read (`read_augment` only).
    #[serde(default)]
    pub path: Option<String>,
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
    /// The request `kind` this listener actually HANDLED, echoed so a newer client can tell a
    /// genuine "nothing new to inject" (`context: null`, `kind: Some(...)`) from an OLDER listener
    /// that didn't understand the kind and fell through to the null catch-all (`kind: None`). The
    /// latter must trigger the client's direct fallback instead of silently disabling the feature
    /// across a hot upgrade (#756 review). Absent on the unknown-kind / malformed reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[cfg(unix)]
pub use listener::{socket_path_for, spawn_listener};

#[cfg(unix)]
mod listener {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use rag_rat_base::config::Config;
    use rag_rat_base::locks::{self, FileLock};
    use rag_rat_core::query::grep_augment::{self, DedupeFilter};
    use rag_rat_db::storage::IndexConnection;
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

    /// Test-only instrumentation hooks for the listener election loop. Tests can wait on the
    /// receivers to observe readiness instead of polling or sleeping through scheduling races.
    #[derive(Clone)]
    #[cfg(test)]
    pub struct ListenerHooks {
        /// Set to true when this listener is about to sleep because the election lock is held by
        /// another process.
        pub waiting: tokio::sync::watch::Receiver<bool>,
        waiting_tx: tokio::sync::watch::Sender<bool>,
        /// Set to true when this listener has won the election and bound the socket, just before
        /// entering the accept loop.
        pub bound: tokio::sync::watch::Receiver<bool>,
        bound_tx: tokio::sync::watch::Sender<bool>,
    }

    #[cfg(test)]
    impl Default for ListenerHooks {
        fn default() -> Self {
            let (waiting_tx, waiting) = tokio::sync::watch::channel(false);
            let (bound_tx, bound) = tokio::sync::watch::channel(false);
            Self { waiting, waiting_tx, bound, bound_tx }
        }
    }

    #[cfg(test)]
    impl ListenerHooks {
        fn signal_waiting(&self) {
            let _ = self.waiting_tx.send(true);
        }

        fn signal_bound(&self) {
            let _ = self.bound_tx.send(true);
        }
    }

    /// Per-session record of what was already injected. Pruned by LRU cap + TTL.
    #[derive(Default)]
    struct SessionState {
        filter: DedupeFilter,
        last_used: Option<Instant>,
    }

    /// Shared body of the listener task. In non-test builds the `$hooks` argument is compiled away.
    macro_rules! spawn_listener_task {
        ($config:expr, $hooks:expr) => {{
            let config = $config;
            tokio::spawn(async move {
                let lock_path = socket_lock_path_for(&config);
                let socket = socket_path_for(&config);
                if let Some(parent) = socket.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                // Win the socket election, then bind. The lock must live inside this task: aborting
                // the task drops it, so a surviving process's retry loop can take over
                // (election, watcher-identical). A bind that fails AFTER winning the
                // election (a transient FS/permissions hiccup) must not strand the
                // worktree serving nothing while holding the lock (#53): drop the
                // election so a sibling can try, back off, and re-elect — the same
                // die→next-process-takes-over model, made resilient for the single-process case
                // too.
                let (_lock, listener): (FileLock, UnixListener) = loop {
                    let lock = loop {
                        match FileLock::try_acquire(&lock_path) {
                            Ok(Some(lock)) => break lock,
                            _ => {
                                #[cfg(test)]
                                {
                                    let hooks = $hooks.clone();
                                    hooks.signal_waiting();
                                }
                                tokio::time::sleep(ELECTION_RETRY).await;
                            },
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
                #[cfg(test)]
                {
                    let hooks = $hooks.clone();
                    hooks.signal_bound();
                }
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
                        eprintln!("agent-hook listener: {err:#}");
                    }
                }
            })
        }};
    }

    /// Spawn the hook listener task: win the socket election (retrying forever, like the
    /// watcher), then accept hook clients until the task is dropped. Returns the JoinHandle so
    /// the server can abort it on teardown; the lock and socket release with the process.
    pub fn spawn_listener(config: Config) -> JoinHandle<()> {
        #[cfg(test)]
        {
            spawn_listener_with_hooks(config, ListenerHooks::default())
        }
        #[cfg(not(test))]
        {
            spawn_listener_task!(config, ())
        }
    }

    /// Same as [`spawn_listener`], but with test hooks that signal readiness/waiting states.
    /// Production logic is unchanged; the hooks are no-ops by default.
    #[cfg(test)]
    pub fn spawn_listener_with_hooks(config: Config, hooks: ListenerHooks) -> JoinHandle<()> {
        spawn_listener_task!(config, hooks)
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
            Ok(req)
                if req.v == PROTOCOL_VERSION
                    && matches!(req.kind.as_str(), "grep_augment" | "read_augment") =>
            {
                let filter = {
                    let state = sessions.entry(req.session_id.clone()).or_default();
                    state.last_used = Some(Instant::now());
                    // Clone ends the borrow before the spawn_blocking await below.
                    state.filter.clone()
                };
                let database = config.database.clone();
                let config_root = config.root.clone();
                let repo_id_override = config.repo_id_override.clone();
                let memory_surface = config.memory.surface;
                // Scope to the session's worktree overlay (#219). Absent cwd (older client) → the
                // config root, which resolves to the base scope. This also fixes the listener's
                // prior lack of ANY scope install: compose queries the `files` view, so without
                // this it read raw, unscoped rows.
                let cwd = req.cwd.clone().map(PathBuf::from).unwrap_or_else(|| config_root.clone());
                let kind = req.kind.clone();
                // Echoed on the reply so a newer client can distinguish "handled, nothing new" from
                // an older listener's unknown-kind null (#756 review).
                let handled_kind = req.kind.clone();
                let pattern = req.pattern.clone();
                let search_path = req.search_path.clone();
                let read_path = req.path.clone();
                // rusqlite is sync; one short read off the runtime threads.
                let composed = tokio::task::spawn_blocking(move || {
                    let conn = IndexConnection::open_read_only(&database)?;
                    // Resolve the repo dimension from this config (identity + override) so the
                    // scope binds the config's repo, not the config-blind sole
                    // repo (a sibling in a consolidated DB); an unprovable repo
                    // → empty scope, never a sibling's rows.
                    let repo_id = rag_rat_core::index::resolve_scope_repo_id(
                        conn.connection(),
                        &config_root,
                        repo_id_override.as_deref(),
                    )?
                    .unwrap_or_default();
                    rag_rat_core::index::install_worktree_scope_view(
                        conn.connection(),
                        &repo_id,
                        &config_root,
                        &cwd,
                    )?;
                    match kind.as_str() {
                        // read_augment with no path resolves to nothing to inject.
                        "read_augment" => match read_path.as_deref() {
                            Some(path) => rag_rat_core::query::read_augment::compose(
                                conn.connection(),
                                path,
                                &filter,
                                memory_surface,
                            ),
                            None => Ok(None),
                        },
                        _ => grep_augment::compose(
                            conn.connection(),
                            &pattern,
                            search_path.as_deref(),
                            &filter,
                            memory_surface,
                        ),
                    }
                })
                .await?;
                let composed = super::degrade_when_fts_corrupt(composed, || {
                    super::spawn_background_fts_heal(config.clone())
                })?;
                match composed {
                    Some(out) => {
                        // compose's returned IDs are exactly the items rendered, so the
                        // session filter grows by exactly what the model now has.
                        let state = sessions.entry(req.session_id).or_default();
                        state.filter.memory_ids.extend(out.memory_ids.iter().cloned());
                        state.filter.symbol_keys.extend(out.symbol_keys.iter().cloned());
                        HookResponse {
                            v: PROTOCOL_VERSION,
                            context: Some(out.context),
                            kind: Some(handled_kind),
                        }
                    },
                    None => HookResponse {
                        v: PROTOCOL_VERSION,
                        context: None,
                        kind: Some(handled_kind),
                    },
                }
            },
            // Unknown v/kind or malformed JSON: answer null-context with NO handled `kind`, never
            // error back. The absent `kind` tells a newer client this listener didn't handle the
            // request so it should fall back (#756 review).
            _ => HookResponse { v: PROTOCOL_VERSION, context: None, kind: None },
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

    use rag_rat_base::config::Config;
    use rag_rat_db::schema;
    use rag_rat_db::storage::IndexConnection;

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
        schema::apply(rw.connection(), &rag_rat_core::index::migration_hooks()).unwrap();
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
        // One caller of frobnicate (symbol id 1), so it ranks as load-bearing for the read-augment
        // lane. Harmless to the grep test, which only asserts the symbol name is present.
        rw.connection()
            .execute(
                "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                                   target_qualified_name, edge_kind, confidence)
                 VALUES (1, NULL, 1, 'frobnicate', 'lib::frobnicate', 'calls_name', 'exact')",
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
        assert!(
            bad.get("kind").map(|k| k.is_null()).unwrap_or(true),
            "an unhandled request echoes NO kind, so a newer client falls back (#756): {bad}",
        );
    }

    /// #756: a `read_augment` request for a file surfaces its load-bearing symbols (here
    /// `frobnicate`, which has a caller), and dedups per session like grep-augment does.
    #[tokio::test]
    async fn listener_serves_read_augment_and_dedupes_per_session() {
        let config = test_config();
        let _listener = spawn_listener(config.clone());
        let socket = socket_path_for(&config);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !socket.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let req = |sid: &str| {
            serde_json::json!({"v": 1, "kind": "read_augment", "session_id": sid,
                               "path": "src/lib.rs"})
        };
        let first = request(&socket, req("r1")).await;
        assert!(
            first["context"].as_str().unwrap().contains("lib::frobnicate"),
            "read-augment surfaces the file's load-bearing symbol: {first}",
        );
        assert_eq!(
            first["kind"], "read_augment",
            "the reply echoes the handled kind so a newer client trusts it (#756): {first}",
        );
        let second = request(&socket, req("r1")).await;
        assert!(second["context"].is_null(), "same session deduped");
        assert_eq!(
            second["kind"], "read_augment",
            "a genuine dedup-null still echoes the kind, so the client does NOT spuriously fall \
             back",
        );
        let other = request(&socket, req("r2")).await;
        assert!(
            other["context"].as_str().unwrap().contains("lib::frobnicate"),
            "a fresh session is not deduped",
        );
    }

    #[tokio::test]
    async fn second_listener_takes_over_when_winner_dies() {
        use super::listener::{ListenerHooks, spawn_listener_with_hooks};

        let config = test_config();
        let winner_hooks = ListenerHooks::default();
        let winner = spawn_listener_with_hooks(config.clone(), winner_hooks.clone());
        // Wait for the winner to have bound the socket instead of polling socket.exists().
        let mut winner_bound = winner_hooks.bound.clone();
        tokio::time::timeout(Duration::from_secs(30), winner_bound.wait_for(|b| *b))
            .await
            .expect("winner should bind")
            .expect("winner bound channel closed");
        let socket = socket_path_for(&config);

        // The loser parks in the election retry loop while the winner holds the lock.
        let loser_hooks = ListenerHooks::default();
        let mut loser_waiting = loser_hooks.waiting.clone();
        let loser = spawn_listener_with_hooks(config.clone(), loser_hooks.clone());
        tokio::time::timeout(Duration::from_secs(30), loser_waiting.wait_for(|b| *b))
            .await
            .expect("loser should reach waiting state")
            .expect("loser waiting channel closed");
        assert!(!loser.is_finished(), "loser must wait, not exit");

        // Kill the winner: its lock fd and bound socket drop with the task's process state.
        winner.abort();
        let _ = winner.await;
        // NOTE: in-process abort drops the FileLock (held by the task) but the dead socket
        // file remains — exactly the stale-socket case. The loser must unlink + re-bind.
        // Wait for explicit rebound signaling instead of polling try_request; the 30s cap is only
        // a deadlock guard and is never reached on a healthy run.
        let mut loser_bound = loser_hooks.bound.clone();
        tokio::time::timeout(Duration::from_secs(30), loser_bound.wait_for(|b| *b))
            .await
            .expect("loser should take over the socket")
            .expect("loser bound channel closed");
        let req = serde_json::json!({"v": 1, "kind": "grep_augment",
                                     "session_id": "takeover",
                                     "pattern": "frobnicate", "search_path": null,
                                     "source": "grep_tool"});
        let reply = request(&socket, req).await;
        assert!(
            reply["context"].as_str().is_some_and(|c| c.contains("lib::frobnicate")),
            "loser must serve real context after takeover"
        );
        loser.abort();
    }
}

/// A read-only hook connection cannot heal FTS corruption, and the hook is latency-critical —
/// degrade to the hook's designed null-context reply and let `on_corrupt` kick recovery. Every
/// other error propagates unchanged.
fn degrade_when_fts_corrupt<T>(
    result: anyhow::Result<Option<T>>,
    on_corrupt: impl FnOnce(),
) -> anyhow::Result<Option<T>> {
    match result {
        Err(error) if rag_rat_core::index::error_is_fts_corruption(&error) => {
            on_corrupt();
            Ok(None)
        },
        other => other,
    }
}

/// One background FTS heal at a time, off the hook's hot path, on its own WRITABLE open — the
/// listener's read-only connection cannot rebuild. Without this, a grep-hook-only session
/// (no MCP queries to trigger the query-layer self-heal) would serve null context indefinitely
/// after a torn write, even though the repair is one lossless rebuild away.
fn spawn_background_fts_heal(config: rag_rat_base::config::Config) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HEAL_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if HEAL_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let outcome = rag_rat_core::IndexDatabase::open_config(&config)
            .and_then(|db| db.heal_fts_if_corrupt());
        match outcome {
            Ok(outcome) if !outcome.healed.is_empty() || !outcome.deferred.is_empty() => {
                eprintln!(
                    "agent-hook: FTS corruption heal — rebuilt {:?}, deferred {:?}",
                    outcome.healed, outcome.deferred
                );
            },
            Ok(_) => {},
            Err(error) => eprintln!("agent-hook: FTS corruption heal failed: {error:#}"),
        }
        HEAL_IN_FLIGHT.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn fts_corruption_degrades_to_null_context_and_kicks_recovery() {
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseCorrupt,
                extended_code: 267, // SQLITE_CORRUPT_VTAB — the FTS5 shadow variant
            },
            Some("database disk image is malformed".to_string()),
        );
        let mut kicked = false;
        let degraded = super::degrade_when_fts_corrupt::<()>(Err(corrupt.into()), || kicked = true)
            .expect("corruption must not surface as a hook error");
        assert!(degraded.is_none(), "the hook's designed degrade is a null context");
        assert!(kicked, "recovery must be kicked exactly when corruption is seen");

        let mut kicked = false;
        let other = super::degrade_when_fts_corrupt::<()>(
            Err(anyhow::anyhow!("unrelated failure")),
            || kicked = true,
        );
        assert!(other.is_err(), "non-corruption errors propagate unchanged");
        assert!(!kicked);

        let value = super::degrade_when_fts_corrupt(Ok(Some(7)), || unreachable!()).unwrap();
        assert_eq!(value, Some(7));
    }

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
        // An unhandled reply (no kind) serializes without the field — the wire shape older clients
        // parse is unchanged; the `kind` echo is purely additive.
        let resp = HookResponse { v: 1, context: None, kind: None };
        assert_eq!(serde_json::to_string(&resp).unwrap(), r#"{"v":1,"context":null}"#);
        // A handled reply carries the echoed kind.
        let handled = HookResponse { v: 1, context: None, kind: Some("read_augment".to_string()) };
        assert_eq!(
            serde_json::to_string(&handled).unwrap(),
            r#"{"v":1,"context":null,"kind":"read_augment"}"#,
        );
    }
}
