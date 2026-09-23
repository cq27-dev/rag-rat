use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use notify::event::{EventKind, ModifyKind};
use notify::{Event, RecursiveMode};
use rag_rat_base::config::{
    Config, LlmConfig, RemoteBackend, RemoteEmbeddingConfig, ResolvedTarget, TargetKind,
    WatchConfig,
};
use rag_rat_base::embedding_models::{FASTEMBED_MODEL_ID, spec};
use rag_rat_base::language::Language;

use crate::watch::*;

#[derive(Debug)]
pub(crate) struct ScratchRoot {
    pub(crate) path: PathBuf,
    pub(crate) _scratch: rag_rat_base::test_scratch::ScratchDir,
}

impl ScratchRoot {
    pub(crate) fn new(tag: impl AsRef<str>) -> Self {
        let scratch = rag_rat_base::test_scratch::ScratchDir::new(tag.as_ref());
        let path = scratch.path().to_path_buf();
        // Watch fixtures historically allocate an absent path, including Git worktree targets.
        let _ = std::fs::remove_dir_all(&path);
        Self { path, _scratch: scratch }
    }

    pub(crate) fn canonicalize(mut self) -> std::io::Result<Self> {
        self.path = rag_rat_base::paths::canonicalize(self.path)?;
        Ok(self)
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

pub(crate) fn scratch_root(tag: impl AsRef<str>) -> ScratchRoot {
    ScratchRoot::new(tag)
}

pub(crate) fn mutation_event(path: PathBuf) -> Event {
    Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path)
}

/// Fresh throwaway placement counters for tests that assert on WHICH directories get watched, not
/// on the failure count (the live watcher owns one long-lived instance per the #658 hardening).
/// Keeps those call sites terse; the counting test uses its own named instances.
pub(crate) fn placement_counters() -> WatchPlacementCounters {
    WatchPlacementCounters::default()
}

/// A single-Rust-target `Config` rooted at `root` watching `target_dirs` — the inline builder
/// the real-watcher placement tests share so they can call `watch_created_dirs` (which needs a
/// `&Config` for the target-relation gate, #332) — paired with the CANONICAL spelling of that
/// root the returned `Config` actually carries.
///
/// Handing the canonical root back is the point of the pair, not a convenience. `Config::load`
/// canonicalizes its root, and the watcher classifies an event by stripping `config.root` off the
/// event path, so a fixture that keeps deriving event paths, ignore matchers and expectations from
/// its OWN spelling of the root is comparing against a root production never produces: the strip
/// misses and no pass is ever dispatched. Linux was the one platform where the two spellings
/// coincided — macOS resolves `/var` → `/private/var`, Windows expands 8.3 names, and the scratch
/// namespace now hands paths out through a symlinked ancestor so Linux carries the divergence too
/// (#1027). Bind the second element and derive every other path in the test from it.
pub(crate) fn whole_root_config(root: &Path, target_dirs: &[PathBuf]) -> (Config, PathBuf) {
    let config_root = rag_rat_base::test_scratch::canonical_config_root(root.to_path_buf());
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: target_dirs.to_vec(),
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        mcp: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let canonical_root = config.root.clone();
    (config, canonical_root)
}

/// The prologue almost every event-loop test shares: a scratch checkout holding one Rust source
/// file under `src/`, its single-target [`whole_root_config`], and the CANONICAL root spelling
/// that `Config` carries. The scratch guard comes back so the caller can keep the directory alive;
/// nothing hands the caller the fixture's own spelling of the root, which is the point.
///
/// Rooted on the scratch guard rather than `tempfile::TempDir` deliberately. A `tempfile` root is
/// already canonical on Linux, so a test that keeps its own copy of the root compares against the
/// same string `Config::load` would produce and stays green — while the identical fixture on macOS
/// (`/var` → `/private/var`) or Windows (8.3 names) diverges and the watcher's `config.root` strip
/// never matches the event path. Scratch paths reach their directory through a symlinked ancestor,
/// so the per-PR Linux matrix carries that divergence too (#1027).
pub(crate) fn src_checkout_config(tag: &str) -> (ScratchRoot, Config, PathBuf) {
    let scratch = scratch_root(tag);
    std::fs::create_dir_all(scratch.join("src")).unwrap();
    std::fs::write(scratch.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let (config, root) = whole_root_config(&scratch, &[PathBuf::from("src")]);
    (scratch, config, root)
}

/// The fixture's own spelling of the root and `config.root` must be two names for ONE directory —
/// the shape `Config::load` produces wherever the system temp is reached through a symlink (macOS
/// `/var` → `/private/var`) or an 8.3 alias (Windows `RUNNER~1`). Rooting the event-loop fixtures
/// at a bare `tempfile::TempDir` degenerates the canonicalization to a no-op on Linux, and a test
/// that kept its own copy of the root would then be indistinguishable from one deriving every path
/// from `config.root` on the only platform the per-PR matrix runs — which is how a mis-scoped
/// root reached the cross-platform legs unnoticed (#1027).
#[cfg(unix)]
#[test]
fn the_watch_fixture_config_root_diverges_from_its_scratch_spelling() {
    let (scratch, config, root) = src_checkout_config("watch-root-spelling");
    assert_ne!(
        config.root,
        scratch.as_path(),
        "the fixture must reach its root through a symlinked ancestor, or root-spelling bugs stay \
         invisible on the per-PR matrix",
    );
    assert_eq!(
        config.root,
        rag_rat_base::paths::canonicalize(scratch.as_path()).unwrap(),
        "both spellings name the same directory",
    );
    assert_eq!(root, config.root, "the returned root is the one the Config carries");
}

pub(crate) fn ephemeral_remote(query_endpoint: Option<&str>) -> RemoteEmbeddingConfig {
    RemoteEmbeddingConfig {
        model: "all-minilm".to_string(),
        backend: RemoteBackend::Ollama,
        endpoint: None,
        cookbook: Some("@rag-rat/cookbook/modal".to_string()),
        query_endpoint: query_endpoint.map(str::to_string),
        auth_env: None,
        gpu: None,
        num_ctx: None,
        batch_size: 256,
        concurrency: 32,
        max_batch_chars: 384_000,
        request_timeout_s: 5,
    }
}

pub(crate) fn activate_ephemeral_model(
    config: &Config,
    repo_id: &str,
    query_endpoint: Option<&str>,
) {
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    let remote = ephemeral_remote(query_endpoint);
    let model_spec = spec(FASTEMBED_MODEL_ID).unwrap();
    conn.execute(
        "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
         'ollama', last_error = NULL
             WHERE model_id = ?1",
        rusqlite::params![FASTEMBED_MODEL_ID, i64::try_from(model_spec.dim).unwrap()],
    )
    .unwrap();
    rag_rat_db::meta::set_repo_meta(&conn, repo_id, "active_embedding_model", FASTEMBED_MODEL_ID)
        .unwrap();
    rag_rat_db::meta::set_repo_meta(
        &conn,
        repo_id,
        "active_embedding_remote_config",
        &serde_json::to_string(&remote).unwrap(),
    )
    .unwrap();
    rag_rat_db::meta::set_repo_meta(
        &conn,
        repo_id,
        "embedding_active_model_version",
        &crate::index::ai::remote_freshness_version(model_spec, &remote),
    )
    .unwrap();
}

#[derive(Debug, Default)]
pub(crate) struct RecordingWatcher {
    pub(crate) watched: Vec<(PathBuf, RecursiveMode)>,
    pub(crate) unwatched: Vec<PathBuf>,
}

impl notify::Watcher for RecordingWatcher {
    fn new<F: notify::EventHandler>(
        _event_handler: F,
        _config: notify::Config,
    ) -> notify::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self::default())
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        self.watched.push((path.to_path_buf(), recursive_mode));
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.unwatched.push(path.to_path_buf());
        Ok(())
    }

    fn kind() -> notify::WatcherKind
    where
        Self: Sized,
    {
        notify::WatcherKind::NullWatcher
    }
}

/// A watcher whose every `watch()` fails — stands in for `ENOSPC` (inotify `max_user_watches`
/// exhausted) so the placement-failure counting is testable without exhausting the real kernel
/// limit.
#[derive(Debug, Default)]
pub(crate) struct FailingWatcher;

impl notify::Watcher for FailingWatcher {
    fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
        Ok(Self)
    }
    fn watch(&mut self, _: &Path, _: RecursiveMode) -> notify::Result<()> {
        Err(notify::Error::generic("forced test failure"))
    }
    fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
        Ok(())
    }
    fn kind() -> notify::WatcherKind {
        notify::WatcherKind::NullWatcher
    }
}

/// Drain notify events for up to `secs` seconds; return whether any event references a path
/// under `needle`. Shared by the issue-#331 placement tests below.
pub(crate) fn drain_until_path_under(
    rx: &std::sync::mpsc::Receiver<notify::Result<Event>>,
    needle: &Path,
    secs: u64,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(event)) =>
                if event.paths.iter().any(|p| p.starts_with(needle)) {
                    return true;
                },
            Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {},
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
    false
}

/// Drain real-watcher setup noise until the channel stays quiet for `quiet_ms`, capped by
/// `max_ms`, so negative placement probes only observe events from the mutation under test.
#[cfg(target_os = "linux")]
pub(crate) fn drain_until_quiet(
    rx: &std::sync::mpsc::Receiver<notify::Result<Event>>,
    quiet_ms: u64,
    max_ms: u64,
) {
    let quiet = Duration::from_millis(quiet_ms);
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(quiet.min(remaining)) {
            Ok(_) => {},
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
}
