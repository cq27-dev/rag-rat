use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, CreateKind, EventKind, Flag, ModifyKind};
use notify::{Event, RecursiveMode, Watcher as _, recommended_watcher};
use rag_rat_base::config::{
    Config, LlmConfig, RemoteBackend, RemoteEmbeddingConfig, ResolvedTarget, TargetKind,
    WatchConfig,
};
use rag_rat_base::embedding_models::{FASTEMBED_MODEL_ID, HASH_MODEL_ID, spec};
use rag_rat_base::language::Language;

use super::*;
use crate::IndexDatabase;
use crate::index::ai::ReconcileOptions;
use crate::index::ignore_rules::IgnoreMatcher;

fn mutation_event(path: PathBuf) -> Event {
    Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path)
}

/// Fresh throwaway placement counters for tests that assert on WHICH directories get watched, not
/// on the failure count (the live watcher owns one long-lived instance per the #658 hardening).
/// Keeps those call sites terse; the counting test uses its own named instances.
fn placement_counters() -> WatchPlacementCounters {
    WatchPlacementCounters::default()
}

/// A single-Rust-target `Config` rooted at `root` watching `target_dirs` — the inline builder
/// the real-watcher placement tests share so they can call `watch_created_dirs` (which needs a
/// `&Config` for the target-relation gate, #332).
fn whole_root_config(root: &Path, target_dirs: &[PathBuf]) -> Config {
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.to_path_buf(),
        database: root.join(".rag-rat/index.sqlite"),
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
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}

/// #427: a maintenance/watch pass on a first-time-empty config DEFERS — the core refuses the
/// empty registration and `run_pass` swallows `EmptyIndexRefused`, returning `Ok(())` and
/// registering nothing, rather than erroring into the watcher loop. Covers the in-process defer
/// path the subprocess CLI guards exercise out-of-process (so it doesn't count toward
/// coverage).
#[test]
fn maintenance_pass_defers_on_a_first_time_empty_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    // A single rust target with NO directories → discovers nothing → first-time-empty.
    let config = whole_root_config(tmp.path(), &[]);
    let result = maintenance_pass(&config, false);
    assert!(result.is_ok(), "an empty first-time config must defer, not error: {result:?}");
    assert!(!config.database.exists(), "deferring must register no empty index");
}

fn ephemeral_remote(query_endpoint: Option<&str>) -> RemoteEmbeddingConfig {
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

fn activate_ephemeral_model(config: &Config, repo_id: &str, query_endpoint: Option<&str>) {
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
struct RecordingWatcher {
    watched: Vec<(PathBuf, RecursiveMode)>,
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

    fn unwatch(&mut self, _path: &Path) -> notify::Result<()> {
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
struct FailingWatcher;

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

/// A failed watch placement is COUNTED (so `index_status` can surface silent degradation), and the
/// existing subtree-skip behavior is unchanged: when the top dir's watch fails, `watch_tree_pruned`
/// does not descend, so exactly one failure is recorded for a whole failed subtree — while a
/// succeeding watcher walks every directory. The counters are OWNED per watcher, so each half of
/// this test reads its own instance — no dependence on other tests' placements (the point of the
/// #658 hardening).
#[test]
fn watch_placement_failures_are_counted_and_the_subtree_is_still_skipped() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("child/grandchild")).unwrap();
    let ignore = crate::index::ignore_rules::IgnoreMatcher::compile(root, &[root.to_path_buf()]);

    // A succeeding watcher places a watch on every directory and records no failure.
    let ok_counters = WatchPlacementCounters::default();
    let mut ok = RecordingWatcher::default();
    super::placement::watch_tree_pruned(&mut ok, &ok_counters, root, &ignore);
    let (ok_attempts, ok_failures) = ok_counters.counts();
    assert_eq!(ok_failures, 0, "no failures for a succeeding watcher");
    assert!(ok_attempts >= 3, "succeeding watcher attempts every directory: {ok_attempts}");
    assert!(ok.watched.len() >= 3, "succeeding watcher descends: {:?}", ok.watched);
    assert!(ok_counters.newly_warnable_failures().is_none(), "no failures → never warnable");

    // A failing watcher fails on the top dir; the subtree is not walked, so exactly ONE failure is
    // counted for the whole subtree (the pre-existing coverage gap this observability surfaces).
    let fail_counters = WatchPlacementCounters::default();
    let mut failing = FailingWatcher;
    super::placement::watch_tree_pruned(&mut failing, &fail_counters, root, &ignore);
    let (_, fail_failures) = fail_counters.counts();
    assert_eq!(
        fail_failures, 1,
        "one failure recorded for the top dir; descent stops, so the subtree is not re-attempted"
    );

    // Warning coalescing: a fresh failure is warnable once, then not (the pass would emit exactly
    // one log line for this batch, never one per directory).
    assert_eq!(fail_counters.newly_warnable_failures(), Some(1), "new failures warn once");
    assert!(
        fail_counters.newly_warnable_failures().is_none(),
        "already-warned failures do not re-warn without new ones"
    );
}

/// A watch that fails because the directory does NOT exist is an expected miss (a not-yet-created
/// configured target, a branch-specific subdir, a worktree registry that appears later) — the
/// ancestor/bootstrap/created-dir watches cover it — so it must NOT be counted as a
/// silently-dropped watch. Otherwise, because the persisted value is a never-lowered high-water
/// mark, an optional target dir would peg `index_status` at "degraded" forever and emit a spurious
/// ENOSPC warning (#658 review).
#[test]
fn a_watch_failure_on_an_absent_directory_is_not_counted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let ignore = IgnoreMatcher::compile(root, &[root.to_path_buf()]);
    let counters = WatchPlacementCounters::default();

    // A failing watcher pointed at a directory that does not exist: the watch errors, but the path
    // is absent, so nothing is counted.
    let mut failing = FailingWatcher;
    let absent = root.join("not-created-yet");
    watch_tree_pruned(&mut failing, &counters, &absent, &ignore);
    assert_eq!(counters.counts().1, 0, "an absent target dir is an expected miss, not a drop");

    // The SAME failing watcher on a directory that DOES exist is a genuine dropped watch — counted.
    std::fs::create_dir_all(root.join("present")).unwrap();
    watch_tree_pruned(&mut failing, &counters, &root.join("present"), &ignore);
    assert_eq!(counters.counts().1, 1, "a failed watch on an existing dir is a real drop");
}

/// The post-pass linked-worktree sync places watches AFTER the pass persisted the counter, so the
/// watcher flushes the failure high-water mark at shutdown — otherwise a drop introduced by that
/// sync, on a periodic-sweep-disabled watcher with no further events, would never reach
/// `index_status` (#658 review). Covers the flush persistence directly (the shutdown wiring in
/// `watcher_main` uses the real notify watcher and isn't unit-drivable).
#[test]
fn shutdown_flush_persists_watch_placement_failures() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let config = whole_root_config(root, &[PathBuf::from("src")]);
    IndexDatabase::rebuild(&config).unwrap();

    // A fresh index has recorded nothing; a flush with zero failures stays a no-op.
    let empty = WatchPlacementCounters::default();
    flush_watch_placement_failures(&config, &empty, Duration::from_secs(3));
    let db = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(db.status(&config.database).unwrap().watch_placement_failures, 0);
    drop(db);

    // Record genuine failures (failing watcher on an existing target), then flush at shutdown.
    let counters = WatchPlacementCounters::default();
    let ignore = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    let (_, failures) = counters.counts();
    assert!(failures > 0, "a failing watcher on an existing dir records a failure");

    flush_watch_placement_failures(&config, &counters, Duration::from_secs(3));
    let db = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(
        db.status(&config.database).unwrap().watch_placement_failures,
        failures,
        "the shutdown flush persists the count so index_status surfaces it"
    );
}

/// The post-resync flush on the event loop uses a NON-blocking (`Duration::ZERO`) lock acquire so
/// it never stalls event classification (#658 review). Prove that path still persists when the lock
/// is free — i.e. `Duration::ZERO` is a real "try once and take it if free", not "never acquire" —
/// and that it routes through the lightweight config-scoped persist (no `open_config` heals).
#[test]
fn a_nonblocking_flush_persists_the_count_when_the_write_lock_is_free() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let config = whole_root_config(root, &[PathBuf::from("src")]);
    IndexDatabase::rebuild(&config).unwrap();

    let counters = WatchPlacementCounters::default();
    let ignore = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    let (_, failures) = counters.counts();
    assert!(failures > 0);

    flush_watch_placement_failures(&config, &counters, Duration::ZERO);
    let db = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(
        db.status(&config.database).unwrap().watch_placement_failures,
        failures,
        "a non-blocking flush persists when the lock is free"
    );
}

/// The flush must stay SIDE-EFFECT-FREE on a first-time-empty checkout: a watcher that placed a
/// watch which failed but never built an index (nothing to discover yet) must NOT leave a
/// schemaless `.rag-rat/index.sqlite` behind — that would poison the friendly no-index read path
/// (#658 review).
#[test]
fn a_flush_on_a_checkout_without_an_index_creates_no_db_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // A source file exists (so the later rebuild has something to index), but no index has been
    // built yet — a source file on disk does not create the `.rag-rat/index.sqlite`.
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let config = whole_root_config(root, &[PathBuf::from("src")]);
    assert!(!config.database.exists(), "precondition: no index file yet");

    // Record real placement failures, then flush — as the shutdown path would.
    let counters = WatchPlacementCounters::default();
    let ignore = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    assert!(counters.counts().1 > 0);

    flush_watch_placement_failures(&config, &counters, Duration::from_secs(3));
    assert!(
        !config.database.exists(),
        "flushing on a checkout with no index must not create a schemaless DB file"
    );

    // ...but once the index IS created (as the shutdown discover does when content arrives in the
    // last debounce window), a subsequent flush persists the count — which is why the shutdown
    // flush runs AFTER `shutdown_discover`, not before (#658 review).
    IndexDatabase::rebuild(&config).unwrap();
    flush_watch_placement_failures(&config, &counters, Duration::from_secs(3));
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        counters.counts().1,
        "a flush after the index is created writes the count into the freshly-created index"
    );
}

/// The non-blocking flush must SKIP (not block, not error) when another SQLite writer holds the
/// database — the event loop must never stall on classification/fleet triggers (#658 review). A
/// second connection holding a write transaction stands in for another repo's writer in a
/// consolidated DB; `busy_timeout = 0` turns the contended write into an immediate SKIP.
#[test]
fn a_nonblocking_flush_skips_a_busy_database_instead_of_blocking() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let config = whole_root_config(root, &[PathBuf::from("src")]);
    IndexDatabase::rebuild(&config).unwrap();

    // Persist an initial count (lock free) so we can prove a later busy flush does NOT overwrite
    // it.
    let counters = WatchPlacementCounters::default();
    let ignore = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    let first = counters.counts().1;
    assert!(first > 0);
    flush_watch_placement_failures(&config, &counters, Duration::from_secs(3));
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        first
    );

    // Grow the in-memory count, then hold the SQLite write lock from a second connection and
    // attempt a non-blocking flush of the higher count. `BEGIN IMMEDIATE` reserves the WAL
    // writer; the flush's `busy_timeout = 0` write must fail fast and SKIP, leaving the stored
    // value untouched.
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore);
    let grown = counters.counts().1;
    assert!(grown > first, "the second placement raised the in-memory count");
    let blocker = rusqlite::Connection::open(&config.database).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    flush_watch_placement_failures(&config, &counters, Duration::ZERO);
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        first,
        "a flush against a busy DB skips rather than blocking or overwriting"
    );

    // With the writer gone, the same flush persists the grown count — proving it was a skip, not a
    // loss.
    flush_watch_placement_failures(&config, &counters, Duration::ZERO);
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        grown,
        "once the DB is free the flush persists the higher count"
    );
}

/// End-to-end: the event loop's between-pass DRAIN surfaces a placement failure in `index_status`
/// WHILE the watcher runs, with the periodic sweep disabled and no pass dispatched — the exact case
/// the post-resync flush exists for (#658 review). Records a failure into the loop's counters (as a
/// resync would, after the pass persisted), wakes the loop, and asserts the drain persisted it.
#[test]
fn the_event_loop_drain_persists_a_placement_failure_while_running() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    config.watch.periodic_sweep_secs = 0; // the exact case the drain must cover.
    IndexDatabase::rebuild(&config).unwrap();

    // A failure recorded into the loop's OWN counters — as the post-pass resync would, after the
    // pass worker already persisted. Nothing is in `repo_meta` yet (no pass wrote it).
    let counters = WatchPlacementCounters::default();
    let ignore_m = IgnoreMatcher::compile(&config.root, &config.target_directories());
    let mut failing = FailingWatcher;
    watch_tree_pruned(&mut failing, &counters, &root.join("src"), &ignore_m);
    let failures = counters.counts().1;
    assert!(failures > 0);
    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        0,
        "precondition: no pass has persisted the count yet"
    );

    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel::<PassRequest>();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        // Wake the loop so an iteration runs the tail drain, give it a moment, then stop.
        tx.send(LoopMsg::Wake).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        let _ = pass_rx;
        handle.join().unwrap();
    });

    assert_eq!(
        IndexDatabase::open_config(&config)
            .unwrap()
            .status(&config.database)
            .unwrap()
            .watch_placement_failures,
        failures,
        "the between-pass drain surfaces the drop in index_status while running (sweep disabled, \
         no pass)"
    );
}

#[test]
fn gitignore_rule_watch_dirs_include_target_ancestors() {
    let root = PathBuf::from("repo");
    let dirs =
        gitignore_rule_watch_dirs(&root, &[PathBuf::from("src/generated"), PathBuf::from(".")]);
    assert!(dirs.contains(&root), "the config root itself is watched");
    assert!(
        dirs.contains(&root.join("src")),
        "a target's parent can carry a .gitignore governing files below it",
    );
    assert!(
        dirs.contains(&root.join("src/generated")),
        "the target root can carry its own .gitignore",
    );
    let unique = dirs.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(dirs.len(), unique.len(), "watch directories are de-duplicated");

    let rejected = gitignore_rule_watch_dirs(&root, &[
        PathBuf::from("../outside"),
        PathBuf::from("/absolute"),
    ]);
    assert_eq!(rejected, vec![root], "non-relative target components are ignored");
}

#[test]
fn recording_watcher_trait_methods_are_covered() {
    let mut watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    watcher.watch(Path::new("repo/src"), RecursiveMode::NonRecursive).unwrap();
    watcher.unwatch(Path::new("repo/src")).unwrap();
    assert_eq!(<RecordingWatcher as notify::Watcher>::kind(), notify::WatcherKind::NullWatcher,);
    assert_eq!(watcher.watched.len(), 1);
}

#[test]
fn startup_catchup_does_not_force_the_expensive_tail() {
    assert!(
        !should_run_base_tail(false, STARTUP_CATCHUP_RUN_GC, false, false, false),
        "an unchanged startup catch-up must not run reconcile/gc/memory validation",
    );
    assert!(
        should_run_base_tail(true, STARTUP_CATCHUP_RUN_GC, false, false, false),
        "real base content changes still run the maintenance tail",
    );
    assert!(
        should_run_base_tail(false, true, false, false, false),
        "scheduled GC passes still force the maintenance tail",
    );
    assert!(
        should_run_base_tail(false, STARTUP_CATCHUP_RUN_GC, true, false, false),
        "a bounded shutdown discover marks base reconcile owed for the next startup pass",
    );
    assert!(
        should_run_base_tail(false, STARTUP_CATCHUP_RUN_GC, false, true, false),
        "startup catch-up retries an already-indexed base embedding backlog",
    );
    assert!(
        should_run_base_tail(false, STARTUP_CATCHUP_RUN_GC, false, false, true),
        "a quiet-elapsed clone-graph backlog forces the otherwise-idle tail (#472)",
    );
}

#[test]
fn overlay_changes_do_not_force_the_base_tail() {
    // #817: a changed overlay's embeddings are reconciled inline by the overlay stage, so running
    // the corpus-scale base reconcile/clone stages on every overlay keystroke would treadmill.
    // Overlay rows DO move the GLOBAL `content_revision()` (they are `main.files` rows), but that
    // digest move and any base work it implies are picked up on the next content/gc/backlog pass,
    // not forced here. The base tail is forced by base-side state only; `run_pass` still runs
    // memory_validate on overlay-changed passes (covered by
    // `overlay_only_pass_skips_base_tail_but_still_validates_memories`).
    assert!(
        !base_tail_forced_by_state(false, false, false),
        "no base-side state forces the base tail — overlay changes are not in the force set",
    );
    assert!(
        base_tail_forced_by_state(true, false, false),
        "a base content change forces the base tail",
    );
    assert!(base_tail_forced_by_state(false, true, false), "the gc cadence forces the base tail");
    assert!(
        base_tail_forced_by_state(false, false, true),
        "a shutdown-owed base reconcile forces the base tail",
    );
}

#[test]
fn overlay_scope_merge_unions_roots_and_all_absorbs() {
    // #577: hints accumulated while the debounce is armed must union attributable roots, and
    // an unattributable hint (rescan, registry change) must widen the whole pass to All.
    let a = OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/a")]));
    let b = OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/b")]));
    assert_eq!(
        a.clone().merge(b.clone()),
        OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/a"), PathBuf::from("/wt/b")])),
        "linked roots union"
    );
    assert_eq!(a.clone().merge(OverlayScope::All), OverlayScope::All, "All absorbs");
    assert_eq!(OverlayScope::All.merge(b), OverlayScope::All, "All absorbs from either side");
    assert_eq!(
        a.clone().merge(OverlayScope::Linked(BTreeSet::new())),
        a,
        "a base-only contribution adds no roots"
    );
}

#[test]
fn scheduler_dispatch_carries_scope_and_a_gc_pass_forces_all() {
    // #577: the event-accumulated scope rides the PassRequest; the 1-in-GC_EVERY_PASSES gc
    // pass forces All (gc's worktree-liveness sweep wants the full picture anyway).
    let mut scheduler = PassScheduler::new();
    let scoped = OverlayScope::Linked(BTreeSet::from([PathBuf::from("/wt/a")]));
    for pass in 1..GC_EVERY_PASSES {
        let request = scheduler.dispatch(scoped.clone()).expect("no pass in flight");
        assert_eq!(
            request,
            PassRequest::Maintenance { run_gc: false, overlay_scope: scoped.clone() },
            "pass {pass} carries the event scope"
        );
        scheduler.on_done();
    }
    assert_eq!(
        scheduler.dispatch(scoped).expect("no pass in flight"),
        PassRequest::Maintenance { run_gc: true, overlay_scope: OverlayScope::All },
        "the gc-cadence pass widens to All"
    );
}

#[test]
fn changed_overlay_skips_backlog_probe() {
    let budget =
        ReconcileBudget::new(ReconcileOptions::default(), Instant::now() - Duration::from_secs(1));
    let needs_embed = overlay_needs_embed(true, false, Some(&budget), |_| false);

    assert!(needs_embed, "a changed overlay still embeds inline");
}

#[test]
fn unchanged_overlay_on_a_scoped_pass_never_probes_the_backlog() {
    // #577: the per-worktree backlog probe (an O(scope) candidate scan) belongs to the `All`
    // sweep only. On an event-scoped pass an unchanged worktree must pay NOTHING.
    let budget =
        ReconcileBudget::new(ReconcileOptions::default(), Instant::now() - Duration::from_secs(1));
    let needs_embed = overlay_needs_embed(false, false, Some(&budget), |_| {
        panic!("the backlog probe must not run on an event-scoped pass")
    });

    assert!(!needs_embed, "unchanged + scoped pass: no embed work");
}

#[test]
fn unchanged_overlay_on_a_sweep_probes_and_retries_a_backlog() {
    let budget =
        ReconcileBudget::new(ReconcileOptions::default(), Instant::now() - Duration::from_secs(1));
    assert!(
        overlay_needs_embed(false, true, Some(&budget), |_| true),
        "a sweep retries a pending overlay backlog (a Partial drain heals within one sweep)"
    );
    assert!(
        !overlay_needs_embed(false, true, Some(&budget), |_| false),
        "a sweep with no backlog does no embed work"
    );
}

#[test]
fn forced_tail_skips_base_backlog_probe() {
    let budget =
        ReconcileBudget::new(ReconcileOptions::default(), Instant::now() - Duration::from_secs(1));
    let needs_tail = base_embedding_backlog_needs_tail(true, true, &budget, |_| true);

    assert!(!needs_tail, "another tail trigger already guarantees reconcile");
}

#[test]
fn maintenance_pass_or_skip_runs_when_lock_is_available() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-maintenance-skip-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn maintenance_target() {}\n").unwrap();
    let root = root.canonicalize().unwrap();
    let config = whole_root_config(&root, &[PathBuf::from("src")]);

    assert!(
        maintenance_pass_or_skip(&config, false).unwrap(),
        "an available writer lock should run the maintenance pass"
    );
    let db = IndexDatabase::open_config(&config).unwrap();
    assert!(db.status(&config.database).unwrap().file_count_by_language.values().sum::<u64>() > 0);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn startup_catchup_retries_existing_base_embedding_backlog() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-startup-backlog-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn pending_startup_embedding(input: i32) -> i32 {
    let doubled = input * 2;
    let shifted = doubled + 13;
    shifted + 7
}
",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();

    let mut config = whole_root_config(&root, &[PathBuf::from("src")]);
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();

    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    assert!(
        db.pending_embedding_jobs().unwrap() > 0,
        "fixture starts with indexed chunks but no embeddings"
    );
    drop(db);

    startup_catchup_pass(&config, None).unwrap();
    let db = IndexDatabase::open_config(&config).unwrap();
    assert_eq!(
        db.pending_embedding_jobs().unwrap(),
        0,
        "unchanged startup catch-up retried and embedded the existing base backlog"
    );
    assert!(
        db.current_embedding_count(HASH_MODEL_ID).unwrap() > 0,
        "startup retry wrote hash embeddings"
    );

    let _ = std::fs::remove_dir_all(root);
}

/// #817 end-to-end: a maintenance pass whose only change is a dirty LINKED worktree skips the
/// base tail (reconcile / clone delta / clone rebuild) but still validates memory anchors.
/// Two persisted traces pin the split:
/// - the clone quiet gate stays UNARMED across overlay-only passes (the pass carries no probe
///   permission, so the pending-forever clone graph is neither probed nor armed — the intended
///   trade-off: it rides overlay churn until a content, gc, or backlog pass), and a base content
///   change then arms it on its own pass (#472 arming preserved);
/// - a memory bound to a path that exists nowhere reaches `anchor_status = "gone"`, which the #492
///   hysteresis only permits after TWO consecutive `memory_validate` passes — proof that both
///   overlay-only passes ran the validate stage.
#[test]
fn overlay_only_pass_skips_base_tail_but_still_validates_memories() {
    use std::sync::atomic::{AtomicU64, Ordering};

    use rag_rat_query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let main =
        std::env::temp_dir().join(format!("ragrat-watch-overlay-only-{}-{id}", std::process::id()));
    let linked = std::env::temp_dir()
        .join(format!("ragrat-watch-overlay-only-linked-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&main);
    let _ = std::fs::remove_dir_all(&linked);
    std::fs::create_dir_all(main.join("src")).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let status =
            std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    };
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("src/lib.rs"), "pub fn base_target(x: i32) -> i32 { x + 1 }\n")
        .unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked_arg = linked.to_string_lossy().into_owned();
    git(&main, &["worktree", "add", "-q", "-b", "feature", &linked_arg]);

    let main_root = main.canonicalize().unwrap();
    let config = whole_root_config(&main_root, &[PathBuf::from("src")]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let created = db
        .memory_create(RepoMemoryCreate {
            kind: "Risk".to_string(),
            title: "ghost anchor".to_string(),
            body: "observed gone by each memory_validate pass".to_string(),
            confidence: "low".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: RepoMemoryBindTarget {
                path: Some("src/ghost.rs".to_string()),
                ..Default::default()
            },
        })
        .unwrap();
    let memory_id = created.memory.memory_id.clone();
    drop(db);

    // Two overlay-only passes: only the linked worktree is dirtied, base content never changes.
    std::fs::write(linked.join("src/lib.rs"), "pub fn base_target(x: i32) -> i32 { x + 2 }\n")
        .unwrap();
    maintenance_pass(&config, false).unwrap();
    std::fs::write(linked.join("src/lib.rs"), "pub fn base_target(x: i32) -> i32 { x + 3 }\n")
        .unwrap();
    maintenance_pass(&config, false).unwrap();
    {
        let db = IndexDatabase::open_config(&config).unwrap();
        assert!(
            !db.clone_graph_quiet_candidate_armed(),
            "overlay-only passes neither probe nor arm the clone quiet gate — the pending clone \
             graph rides overlay churn until a content, gc, or backlog pass (#817)"
        );
        let memory = db.memory_get(&memory_id).unwrap().expect("memory persists");
        let path_binding =
            memory.bindings.iter().find(|b| b.binding_kind == "path").expect("path binding");
        assert_eq!(
            path_binding.anchor_status, "gone",
            "memory_validate ran on both overlay-only passes: the #492 hysteresis needs two \
             consecutive gone observations to persist the downgrade"
        );
    }

    // A base content change carries probe permission, so its pass ARMS the quiet window for the
    // (still absent) clone graph — the #472 arming path is untouched by #817.
    std::fs::write(main_root.join("src/lib.rs"), "pub fn base_target(x: i32) -> i32 { x + 4 }\n")
        .unwrap();
    maintenance_pass(&config, false).unwrap();
    {
        let db = IndexDatabase::open_config(&config).unwrap();
        assert!(
            db.clone_graph_quiet_candidate_armed(),
            "a base content change arms the clone quiet gate on its own pass"
        );
    }

    git(&main_root, &["worktree", "remove", "-f", &linked_arg]);
    std::fs::remove_dir_all(&main_root).ok();
    std::fs::remove_dir_all(&linked).ok();
}

#[test]
fn startup_catchup_skips_ephemeral_backlog_scan_without_query_endpoint() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-ephemeral-backlog-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn pending_ephemeral_startup(input: i32) -> i32 {
    let doubled = input * 2;
    let shifted = doubled + 13;
    shifted + 7
}
",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();

    let mut config = whole_root_config(&root, &[PathBuf::from("src")]);
    config.llm.embedding.backend = FASTEMBED_MODEL_ID.parse().unwrap();

    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = db.active_repo_id.clone();
    drop(db);
    activate_ephemeral_model(&config, &repo_id, None);

    let db = IndexDatabase::open_config(&config).unwrap();
    assert!(
        db.pending_embedding_jobs().unwrap() > 0,
        "fixture has indexed chunks missing embeddings for the active ephemeral model"
    );
    drop(db);

    crate::index::ai::reset_estimated_reconcile_job_calls();
    startup_catchup_pass(&config, None).unwrap();
    assert_eq!(
        crate::index::ai::estimated_reconcile_job_calls(),
        0,
        "startup must not scan chunks before the ephemeral light endpoint is known usable"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn startup_catchup_reconciles_shutdown_discovered_content() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-shutdown-reconcile-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn initial_value(input: i32) -> i32 {
    let doubled = input * 2;
    let shifted = doubled + 13;
    shifted + 7
}
",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();

    let mut config = whole_root_config(&root, &[PathBuf::from("src")]);
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();

    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    db.reconcile_with_options_progress(ReconcileOptions::default(), |_| {}).unwrap();
    assert!(
        db.current_embedding_count(HASH_MODEL_ID).unwrap() > 0,
        "fixture must produce at least one embeddable chunk"
    );
    assert_eq!(db.pending_embedding_jobs().unwrap(), 0, "fixture starts fully reconciled");
    assert!(
        !shutdown_discover(&config).unwrap(),
        "shutdown discover without source edits has no reconcile marker to set"
    );
    drop(db);

    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn changed_value(input: i32) -> i32 {
    let tripled = input * 3;
    let shifted = tripled + 21;
    shifted - 4
}
",
    )
    .unwrap();
    assert!(shutdown_discover(&config).unwrap(), "shutdown discover indexed the edit");

    let db = IndexDatabase::open_config(&config).unwrap();
    assert!(
        db.watch_shutdown_reconcile_pending().unwrap(),
        "shutdown-discovered content leaves a startup reconcile marker"
    );
    assert!(
        db.pending_embedding_jobs().unwrap() > 0,
        "the discover-only shutdown pass leaves changed chunks without embeddings"
    );
    drop(db);

    maintenance_pass(&config, STARTUP_CATCHUP_RUN_GC).unwrap();
    let db = IndexDatabase::open_config(&config).unwrap();
    assert!(
        !db.watch_shutdown_reconcile_pending().unwrap(),
        "successful startup reconcile clears the shutdown marker"
    );
    assert_eq!(db.pending_embedding_jobs().unwrap(), 0, "startup catch-up embedded the backlog");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn startup_catchup_keeps_shutdown_marker_when_reconcile_is_blocked() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-shutdown-blocked-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn pending_embedding(input: i32) -> i32 {
    let doubled = input * 2;
    let shifted = doubled + 13;
    shifted + 7
}
",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();

    let config = whole_root_config(&root, &[PathBuf::from("src")]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    db.mark_watch_shutdown_reconcile_pending().unwrap();
    drop(db);

    maintenance_pass(&config, STARTUP_CATCHUP_RUN_GC).unwrap();
    let db = IndexDatabase::open_config(&config).unwrap();
    assert!(
        db.watch_shutdown_reconcile_pending().unwrap(),
        "a blocked startup reconcile must keep the shutdown marker for a later retry"
    );
    assert_eq!(
        db.pending_embedding_jobs().unwrap(),
        0,
        "not-ready models report no pending jobs, so marker clearing must key off status"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn wal_checkpoint_runs_at_every_pass_terminal_gated_by_size_alone() {
    // #818: with `wal_autocheckpoint = 0` on every read-write connection, the pass-terminal
    // checkpoint is the ONLY thing folding the WAL back into the main file — so it must fire on
    // churn passes too (the pre-#818 quiet-only gate would let the sidecar grow without bound
    // under sustained editing, where quiet passes almost never happen). The size threshold is
    // the one remaining gate.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-wal-checkpoint-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "pub fn wal_probe() -> i32 { 1 }\n").unwrap();
    let config = whole_root_config(&root, &[PathBuf::from("src")]);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Put frames in the WAL so there is something to truncate (any meta write serves).
    db.mark_watch_shutdown_reconcile_pending().unwrap();
    db.clear_watch_shutdown_reconcile_pending().unwrap();
    assert!(db.database_file_health().unwrap().wal_bytes > 0);

    maybe_checkpoint_wal(&db, u64::MAX);
    assert!(
        db.database_file_health().unwrap().wal_bytes > 0,
        "an under-threshold WAL is left alone — the probe is a bare stat"
    );

    maybe_checkpoint_wal(&db, 1);
    assert_eq!(
        db.database_file_health().unwrap().wal_bytes,
        0,
        "an oversized WAL is truncated with no quiet-pass precondition"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn initial_watch_state_places_base_gitignore_and_fleet_surfaces() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-initial-state-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/kept")).unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let root = root.canonicalize().unwrap();

    let config = whole_root_config(&root, &[PathBuf::from("src")]);
    let target_dirs = config.target_directories();
    let ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let (linked_worktrees, registry) = place_initial_watch_state(
        &mut watcher,
        &placement_counters(),
        &config,
        &target_dirs,
        &ignore,
        Some(&root.join("bin/rag-rat")),
    );

    assert!(linked_worktrees.states.is_empty());
    assert!(registry.is_none(), "non-git fixtures have no worktree registry");
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root.join("src") && *mode == RecursiveMode::NonRecursive),
        "configured target roots are placed through the initial state helper",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root && *mode == RecursiveMode::NonRecursive),
        "the config root is watched for root .gitignore edits",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root.join("bin") && *mode == RecursiveMode::NonRecursive),
        "fleet hot-upgrade watches the installed binary directory",
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn event_maintenance_helpers_place_dirs_recompile_and_refresh_linked_state() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("ragrat-watch-maint-helper-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/fresh")).unwrap();
    let root = root.canonicalize().unwrap();

    let config = whole_root_config(&root, &[PathBuf::from("src")]);
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut watcher = RecordingWatcher::default();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.join("src/fresh"));

    assert_eq!(
        event_requests_maintenance(
            &mut watcher,
            &placement_counters(),
            &create,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
            None,
        ),
        Some(OverlayScope::Linked(BTreeSet::new())),
        "placing a newly-created BASE target dir must request a base-only maintenance pass",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root.join("src/fresh")
                && *mode == RecursiveMode::NonRecursive),
        "event helper delegates created-directory placement",
    );

    let before_recompile = watcher.watched.len();
    recompile_ignore_and_place_watches(
        &mut watcher,
        &placement_counters(),
        &config,
        &target_dirs,
        &mut ignore,
        &mut linked_worktrees,
    );
    assert!(
        watcher.watched.len() > before_recompile,
        "gitignore recompiles also re-place base target watches",
    );

    sync_linked_worktrees_after_pass(
        &mut watcher,
        &placement_counters(),
        &config,
        &mut linked_worktrees,
    );
    assert!(linked_worktrees.states.is_empty());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn event_maintenance_helper_requests_pass_for_relevant_and_registry_events() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-maint-branches-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let root = root.canonicalize().unwrap();

    let config = whole_root_config(&root, &[PathBuf::from("src")]);
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut watcher = RecordingWatcher::default();
    let relevant_file = mutation_event(root.join("src/lib.rs"));

    assert_eq!(
        event_requests_maintenance(
            &mut watcher,
            &placement_counters(),
            &relevant_file,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
            None,
        ),
        Some(OverlayScope::Linked(BTreeSet::new())),
        "a base target edit fires a base-only pass",
    );

    let registry = root.join(".git/worktrees");
    let registry_event = mutation_event(registry.join("feature/HEAD"));
    assert_eq!(
        event_requests_maintenance(
            &mut watcher,
            &placement_counters(),
            &registry_event,
            &config,
            &target_dirs,
            &mut ignore,
            &mut linked_worktrees,
            Some(&registry),
        ),
        Some(OverlayScope::All),
        "a worktree-registry change is unattributable, so the pass sweeps every overlay",
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn initial_watch_state_places_worktree_registry() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let main =
        std::env::temp_dir().join(format!("ragrat-watch-registry-{}-{id}", std::process::id()));
    let linked = std::env::temp_dir()
        .join(format!("ragrat-watch-registry-linked-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&main);
    let _ = std::fs::remove_dir_all(&linked);
    std::fs::create_dir_all(main.join("src")).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let status =
            std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    };
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked_arg = linked.to_string_lossy().into_owned();
    git(&main, &["worktree", "add", "-q", "-b", "feature", &linked_arg]);

    let main = main.canonicalize().unwrap();
    let config = whole_root_config(&main, &[PathBuf::from("src")]);
    let target_dirs = config.target_directories();
    let ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let (linked_worktrees, registry) = place_initial_watch_state(
        &mut watcher,
        &placement_counters(),
        &config,
        &target_dirs,
        &ignore,
        None,
    );
    let registry = registry.expect("git worktree repo exposes a registry directory");

    assert!(
        !linked_worktrees.states.is_empty(),
        "the linked checkout should receive watcher state",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &registry && *mode == RecursiveMode::NonRecursive),
        "the worktree registry must be watched so add/remove events schedule maintenance",
    );

    git(&main, &["worktree", "remove", "-f", &linked_arg]);
    std::fs::remove_dir_all(&main).ok();
    std::fs::remove_dir_all(&linked).ok();
}

#[test]
fn watch_created_dirs_ignores_non_appearance_events() {
    let root = PathBuf::from("/repo");
    let target_dirs = vec![PathBuf::from("src")];
    let config = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let access = Event::new(EventKind::Access(AccessKind::Any)).add_path(root.join("src/fresh"));

    assert!(!watch_created_dirs(
        &mut watcher,
        &placement_counters(),
        &access,
        &config,
        &target_dirs,
        &mut ignore,
        None
    ));
    assert!(watcher.watched.is_empty());
}

#[test]
fn missing_config_root_bootstrap_dirs_use_existing_ancestor_chain() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let checkout =
        std::env::temp_dir().join(format!("ragrat-bootstrap-chain-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&checkout);
    std::fs::create_dir_all(checkout.join("packages")).unwrap();
    let checkout = checkout.canonicalize().unwrap();
    let packages = checkout.join("packages");
    let config_root = packages.join("crate");

    assert_eq!(
        missing_config_root_bootstrap_dirs(&config_root, &checkout),
        vec![checkout.clone(), packages.clone()],
        "the deepest existing ancestor must be watched so its missing child creation is delivered",
    );
    assert!(
        missing_config_root_bootstrap_dirs(&config_root, &checkout.join("sibling")).is_empty(),
        "unrelated bootstrap roots must not gain watches",
    );
    std::fs::create_dir_all(&config_root).unwrap();
    assert!(
        missing_config_root_bootstrap_dirs(&config_root, &checkout).is_empty(),
        "no bootstrap is needed once the config root exists",
    );

    std::fs::remove_dir_all(&checkout).ok();
}

#[test]
fn created_dir_placement_classifies_target_ancestors_and_subtrees() {
    let root = PathBuf::from("/repo");
    let nested = vec![PathBuf::from("src/generated")];
    let config = whole_root_config(&root, &nested);

    assert_eq!(
        created_dir_placement(&config, &nested, &PathBuf::from("/elsewhere/src"), None),
        CreatedDirPlacement::OutsideTargets,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root.join("vendor"), None),
        CreatedDirPlacement::OutsideTargets,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root, None),
        CreatedDirPlacement::TargetAncestor,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root.join("src"), None),
        CreatedDirPlacement::TargetAncestor,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root.join("src/generated"), None),
        CreatedDirPlacement::TargetSubtree,
    );
    assert_eq!(
        created_dir_placement(&config, &nested, &root.join("src/generated/pkg"), None),
        CreatedDirPlacement::TargetSubtree,
    );

    let whole_root = vec![PathBuf::from(".")];
    let whole_config = whole_root_config(&root, &whole_root);
    assert_eq!(
        created_dir_placement(&whole_config, &whole_root, &root.join("anything"), None),
        CreatedDirPlacement::TargetSubtree,
    );

    let checkout = PathBuf::from("/checkout");
    let subdir_root = checkout.join("packages/crate");
    let subdir_config = whole_root_config(&subdir_root, &nested);
    assert_eq!(
        created_dir_placement(&subdir_config, &nested, &checkout.join("packages"), Some(&checkout)),
        CreatedDirPlacement::TargetAncestor,
    );
    assert_eq!(
        created_dir_placement(&subdir_config, &nested, &subdir_root, Some(&checkout)),
        CreatedDirPlacement::TargetAncestor,
    );
    assert_eq!(
        created_dir_placement(&subdir_config, &nested, &checkout.join("vendor"), Some(&checkout)),
        CreatedDirPlacement::OutsideTargets,
    );
}

#[test]
fn event_touches_worktree_matches_checkout_targets_and_registry() {
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: PathBuf::from("/main"),
        database: PathBuf::from("/main/.rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let worktree = PathBuf::from("/wt/feat");
    let registry = PathBuf::from("/main/.git/worktrees");
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    // A target file in a linked worktree fires (its overlay needs refreshing).
    assert!(
        event_touches_worktree(
            &mutation_event(worktree.join("src/a.rs")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A non-target file in the worktree does not.
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join("README.md")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A change in the worktree registry (a `git worktree add`/`remove`) fires.
    assert!(
        event_touches_worktree(
            &mutation_event(registry.join("feat/HEAD")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A `.gitignore` edit in the linked checkout fires (it changes the overlay's ignored set),
    // mirroring the base classifier (#219 review).
    assert!(
        event_touches_worktree(
            &mutation_event(worktree.join(".gitignore")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A `.gitignore` OUTSIDE any watched checkout does not.
    assert!(
        !event_touches_worktree(
            &mutation_event(PathBuf::from("/elsewhere/.gitignore")),
            &worktrees,
            Some(&registry),
        )
        .fires()
    );
    // A read event never fires (anti-feedback, same as the base watcher).
    let read = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
        .add_path(worktree.join("src/a.rs"));
    assert!(!event_touches_worktree(&read, &worktrees, Some(&registry)).fires());
    // A backend rescan fires when there is linked-worktree or registry state to refresh.
    let rescan = Event::new(EventKind::Other).set_flag(Flag::Rescan);
    assert!(event_touches_worktree(&rescan, &worktrees, None).fires());
    assert!(
        event_touches_worktree(&rescan, &LinkedWorktreeWatches::default(), Some(&registry),)
            .fires()
    );
    assert!(!event_touches_worktree(&rescan, &LinkedWorktreeWatches::default(), None).fires());
    // No worktrees and no registry → nothing fires.
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join("src/a.rs")),
            &LinkedWorktreeWatches::default(),
            None,
        )
        .fires()
    );
}

#[test]
fn event_touches_worktree_attributes_the_touched_checkout_roots() {
    // #577: the hint names WHICH linked checkouts an event implicates, so the dispatched pass
    // refreshes those overlays instead of sweeping the fleet; registry changes and rescans are
    // unattributable and widen to AllWorktrees.
    let config = whole_root_config(&PathBuf::from("/main"), &[PathBuf::from("src")]);
    let wt_a = PathBuf::from("/wt/a");
    let wt_b = PathBuf::from("/wt/b");
    let registry = PathBuf::from("/main/.git/worktrees");
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        wt_a.clone(),
        wt_b.clone(),
    ]);

    assert_eq!(
        event_touches_worktree(&mutation_event(wt_a.join("src/a.rs")), &worktrees, None),
        WorktreeEventHint::Roots(BTreeSet::from([wt_a.clone()])),
        "a target edit is attributed to its own checkout only"
    );
    assert_eq!(
        event_touches_worktree(&mutation_event(wt_b.join("src/b.rs")), &worktrees, None),
        WorktreeEventHint::Roots(BTreeSet::from([wt_b.clone()])),
    );
    assert_eq!(
        event_touches_worktree(&mutation_event(wt_a.join("README.md")), &worktrees, None),
        WorktreeEventHint::None,
        "a non-target path implicates nothing"
    );
    assert_eq!(
        event_touches_worktree(
            &mutation_event(registry.join("feat/HEAD")),
            &worktrees,
            Some(&registry),
        ),
        WorktreeEventHint::AllWorktrees,
        "a registry change (worktree add/remove) is unattributable"
    );
    let rescan = Event::new(EventKind::Other).set_flag(Flag::Rescan);
    assert_eq!(
        event_touches_worktree(&rescan, &worktrees, None),
        WorktreeEventHint::AllWorktrees,
        "a rescan means events were dropped — refresh everything"
    );
    // A branch-local `rag-rat.toml` edit changes the checkout's TARGET SET without moving
    // either HEAD (#577 review): like a `.gitignore` edit, it must fire and be attributed to
    // its checkout so the overlay is refreshed with the new branch config.
    assert_eq!(
        event_touches_worktree(&mutation_event(wt_b.join("rag-rat.toml")), &worktrees, None),
        WorktreeEventHint::Roots(BTreeSet::from([wt_b.clone()])),
        "a linked checkout's config edit fires for that checkout"
    );
    assert_eq!(
        event_touches_worktree(
            &mutation_event(PathBuf::from("/elsewhere/rag-rat.toml")),
            &worktrees,
            None
        ),
        WorktreeEventHint::None,
        "a config file outside any watched checkout does not fire"
    );
}

#[test]
fn linked_worktree_events_honor_its_ignore_rules() {
    // A linked worktree can be watched for a whole-root target, but ignored subtrees must still
    // be dropped before they fire an overlay refresh. This is the classification half of the
    // linked-watch fix: without the per-worktree IgnoreMatcher, `ignored_dir/out.rs` and
    // `target/debug/build.rs` both matched `**/*.rs` and armed the debounce.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = std::env::temp_dir().join(format!("ragrat-wt-ign-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::create_dir_all(worktree.join("ignored_dir")).unwrap();
    std::fs::create_dir_all(worktree.join("target/debug")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "ignored_dir/\ntarget/\n").unwrap();

    let config = whole_root_config(&worktree, &[PathBuf::from(".")]);
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    assert!(
        event_touches_worktree(&mutation_event(worktree.join("src/lib.rs")), &worktrees, None)
            .fires(),
        "an unignored linked target file still fires",
    );
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join("ignored_dir/out.rs")),
            &worktrees,
            None
        )
        .fires(),
        "a linked worktree gitignored source-looking path must not fire",
    );
    assert!(
        !event_touches_worktree(
            &mutation_event(worktree.join("target/debug/build.rs")),
            &worktrees,
            None
        )
        .fires(),
        "a linked worktree floor/gitignored build path must not fire",
    );
    assert!(
        event_touches_worktree(&mutation_event(worktree.join(".gitignore")), &worktrees, None)
            .fires(),
        "a linked worktree .gitignore edit still fires so rules can be recompiled",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_worktree_watch_placement_uses_configured_pruned_targets() {
    // Placement half of the linked-watch fix: linked checkouts used to be subscribed with one
    // `Recursive` watch on the checkout root, which descended into `target/` and any ignored
    // dependency/build tree. They should get the same non-recursive, gitignore-pruned target
    // placement as the main checkout.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree =
        std::env::temp_dir().join(format!("ragrat-wt-place-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src/kept")).unwrap();
    std::fs::create_dir_all(worktree.join("src/ignored_dir")).unwrap();
    std::fs::create_dir_all(worktree.join("target/debug")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "src/ignored_dir/\ntarget/\n").unwrap();

    let config = whole_root_config(&worktree, &[PathBuf::from("src")]);
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);
    let state = &worktrees.states[0];

    assert_eq!(state.config.root, worktree);
    assert!(
        watcher.watched.iter().any(|(path, mode)| path == &state.config.root.join("src")
            && *mode == RecursiveMode::NonRecursive),
        "the configured target root must be watched non-recursively",
    );
    assert!(
        watcher.watched.iter().any(|(path, mode)| path == &state.config.root.join("src/kept")
            && *mode == RecursiveMode::NonRecursive),
        "non-ignored target subdirs must be watched",
    );
    assert!(
        watcher.watched.iter().all(|(_, mode)| *mode == RecursiveMode::NonRecursive),
        "linked worktrees must not receive a recursive checkout watch: {:?}",
        watcher.watched,
    );
    assert!(
        watcher.watched.iter().all(|(path, _)| !path.starts_with(state.config.root.join("target"))
            && !path.starts_with(state.config.root.join("src/ignored_dir"))),
        "ignored or non-target build trees must not be watched: {:?}",
        watcher.watched,
    );

    std::fs::remove_dir_all(&state.config.root).ok();
}

#[test]
fn linked_worktree_watch_set_sync_rebuilds_existing_root_state() {
    // A linked checkout can keep the same filesystem path while switching to a branch whose
    // local config has different targets. The pass reconciliation must rebuild state for every
    // current root, not only add brand-new roots.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = std::env::temp_dir().join(format!("ragrat-wt-sync-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::create_dir_all(worktree.join("extra")).unwrap();
    let worktree = worktree.canonicalize().unwrap();

    let src_config = whole_root_config(&worktree, &[PathBuf::from("src")]);
    let extra_config = whole_root_config(&worktree, &[PathBuf::from("extra")]);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = LinkedWorktreeWatches::default();
    worktrees.sync(&mut watcher, &placement_counters(), &src_config, vec![worktree.clone()]);
    assert_eq!(worktrees.states[0].target_dirs, vec![PathBuf::from("src")]);

    worktrees.sync(&mut watcher, &placement_counters(), &extra_config, vec![worktree.clone()]);
    assert_eq!(worktrees.states.len(), 1);
    assert_eq!(worktrees.states[0].target_dirs, vec![PathBuf::from("extra")]);
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &worktree.join("extra")),
        "sync should place watches for the refreshed target set",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_worktree_watch_set_handles_created_dirs_and_recompile() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree = std::env::temp_dir().join(format!("ragrat-wt-set-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "").unwrap();

    let config = whole_root_config(&worktree, &[PathBuf::from("src")]);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    let fresh = worktree.join("src/fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(fresh.clone());
    assert_eq!(
        worktrees.watch_created_dirs(&mut watcher, &placement_counters(), &create),
        BTreeSet::from([worktree.clone()]),
        "created target dirs should request a maintenance pass scoped to their checkout",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &fresh),
        "created target dirs are watched through the centralized linked-worktree state",
    );

    std::fs::write(worktree.join(".gitignore"), "src/fresh/\n").unwrap();
    worktrees.recompile_ignore_and_place_watches(&mut watcher, &placement_counters());
    assert!(
        worktrees.states[0].ignore.is_ignored(&fresh, true),
        "recompile refreshes the state's matcher",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_worktree_watch_set_handles_created_target_ancestors() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree =
        std::env::temp_dir().join(format!("ragrat-wt-ancestor-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&worktree).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "").unwrap();

    let target_dirs = vec![PathBuf::from("src/generated")];
    let config = whole_root_config(&worktree, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    watcher.watched.clear();
    let ancestor = worktree.join("src");
    std::fs::create_dir_all(&ancestor).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(ancestor.clone());
    assert_eq!(
        worktrees.watch_created_dirs(&mut watcher, &placement_counters(), &create),
        BTreeSet::from([worktree.clone()]),
        "created target ancestors should request a maintenance pass after placing watches",
    );

    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &ancestor && *mode == RecursiveMode::NonRecursive),
        "a newly-created linked target ancestor must be watched non-recursively",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &worktree.join("src/generated")),
        "created ancestors should re-place configured target watches in case the target already \
         exists",
    );
    assert!(
        watcher.watched.iter().all(|(_, mode)| *mode == RecursiveMode::NonRecursive),
        "ancestor handling must not reintroduce recursive checkout watches",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_worktree_target_ancestor_gitignore_is_compiled() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree =
        std::env::temp_dir().join(format!("ragrat-wt-ancestor-ignore-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src/generated")).unwrap();
    std::fs::create_dir_all(worktree.join("src/sibling")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join("src/.gitignore"), "generated/\n").unwrap();
    std::fs::write(worktree.join("src/sibling/.gitignore"), "marker.rs\n").unwrap();

    let target_dirs = vec![PathBuf::from("src/generated")];
    let config = whole_root_config(&worktree, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);
    let ignore = &worktrees.states[0].ignore;

    assert!(
        ignore.is_ignored(&worktree.join("src/generated/lib.rs"), false),
        "target ancestor .gitignore rules must govern nested linked targets",
    );
    assert!(
        !ignore.is_ignored(&worktree.join("src/sibling/marker.rs"), false),
        "compiling target ancestors must not scan unindexed siblings below that ancestor",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_subdir_root_watch_placement_keeps_checkout_root_when_config_root_missing() {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let repo = std::env::temp_dir()
        .join(format!("ragrat-wt-missing-root-main-{}-{id}", std::process::id()));
    let checkout = std::env::temp_dir()
        .join(format!("ragrat-wt-missing-root-linked-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&checkout);
    std::fs::create_dir_all(repo.join("packages/crate/src")).unwrap();
    std::fs::write(repo.join("packages/crate/src/lib.rs"), "fn lib() {}\n").unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@e"],
        vec!["config", "user.name", "t"],
        vec!["add", "."],
        vec!["commit", "-q", "-m", "base"],
    ] {
        let output = Command::new("git").args(&args).current_dir(&repo).output().unwrap();
        assert!(output.status.success(), "git {args:?} failed: {output:?}");
    }
    std::fs::create_dir_all(checkout.join("packages")).unwrap();
    let checkout = checkout.canonicalize().unwrap();
    let config_root = repo.join("packages/crate").canonicalize().unwrap();
    let target_dirs = vec![PathBuf::from("src")];
    let config = whole_root_config(&config_root, &target_dirs);

    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        checkout.clone(),
    ]);
    let linked_root = checkout.join("packages/crate");

    assert_eq!(worktrees.states[0].config.root, linked_root);
    assert!(!linked_root.exists(), "the linked branch has not created the configured root yet");
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &checkout && *mode == RecursiveMode::NonRecursive),
        "a missing linked subdir-root needs a non-recursive checkout-root bootstrap watch",
    );
    assert!(
        watcher.watched.iter().any(|(path, mode)| path == &checkout.join("packages")
            && *mode == RecursiveMode::NonRecursive),
        "an existing parent of the missing linked root must be watched for the final component",
    );
    assert!(
        watcher.watched.iter().all(|(_, mode)| *mode == RecursiveMode::NonRecursive),
        "missing-root bootstrapping must not restore recursive checkout watches",
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&checkout).ok();
}

#[test]
fn watch_created_dirs_reinstalls_watches_for_recreated_config_root() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-recreated-root-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();
    let root = root.canonicalize().unwrap();
    let target_dirs = vec![PathBuf::from("src")];
    let config = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.clone());

    assert!(
        watch_created_dirs(
            &mut watcher,
            &placement_counters(),
            &create,
            &config,
            &target_dirs,
            &mut ignore,
            None
        ),
        "recreated config roots should re-place target watches and request maintenance",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root && *mode == RecursiveMode::NonRecursive),
        "the recreated config root itself should stay watched non-recursively",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &root.join("src") && *mode == RecursiveMode::NonRecursive),
        "configured targets below the recreated root should be watched again",
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn watch_created_dirs_bootstraps_missing_linked_subdir_root_ancestors() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let checkout = std::env::temp_dir()
        .join(format!("ragrat-watch-linked-ancestor-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&checkout);
    std::fs::create_dir_all(checkout.join("packages")).unwrap();
    std::fs::create_dir_all(checkout.join("vendor")).unwrap();
    let checkout = checkout.canonicalize().unwrap();
    let packages = checkout.join("packages");
    let target_dirs = vec![PathBuf::from("src")];
    let config = whole_root_config(&packages.join("crate"), &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let create_packages =
        Event::new(EventKind::Create(CreateKind::Folder)).add_path(packages.clone());

    assert!(
        watch_created_dirs(
            &mut watcher,
            &placement_counters(),
            &create_packages,
            &config,
            &target_dirs,
            &mut ignore,
            Some(&checkout),
        ),
        "an intermediate ancestor of a missing linked config root must keep the bootstrap moving",
    );
    assert!(
        watcher
            .watched
            .iter()
            .any(|(path, mode)| path == &packages && *mode == RecursiveMode::NonRecursive),
        "the appeared ancestor itself must be watched for the next path component",
    );
    assert!(
        watcher.watched.iter().all(|(_, mode)| *mode == RecursiveMode::NonRecursive),
        "missing linked-root ancestors must not reintroduce recursive checkout watches",
    );

    watcher.watched.clear();
    let vendor = checkout.join("vendor");
    let create_vendor = Event::new(EventKind::Create(CreateKind::Folder)).add_path(vendor.clone());
    assert!(
        !watch_created_dirs(
            &mut watcher,
            &placement_counters(),
            &create_vendor,
            &config,
            &target_dirs,
            &mut ignore,
            Some(&checkout),
        ),
        "sibling directories under the checkout are outside the missing config root",
    );
    assert!(watcher.watched.is_empty(), "outside siblings should not gain watches");

    std::fs::remove_dir_all(&checkout).ok();
}

#[test]
fn linked_created_target_dir_requests_maintenance_when_directory_event_is_not_relevant() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let worktree =
        std::env::temp_dir().join(format!("ragrat-wt-create-pass-{}-{id}", std::process::id()));
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    std::fs::write(worktree.join(".gitignore"), "").unwrap();

    let target_dirs = vec![PathBuf::from("src")];
    let config = whole_root_config(&worktree, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        worktree.clone(),
    ]);

    watcher.watched.clear();
    let pkg = worktree.join("src/pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("lib.rs"), "fn pkg() {}\n").unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(pkg.clone());

    assert!(
        !event_touches_worktree(&create, &worktrees, None).fires(),
        "extensionless directory events are not target-file events",
    );
    assert_eq!(
        worktrees.watch_created_dirs(&mut watcher, &placement_counters(), &create),
        BTreeSet::from([worktree.clone()]),
        "placing a linked target-dir watch must request a maintenance pass",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &pkg),
        "the linked target directory is still watched for subsequent edits",
    );

    std::fs::remove_dir_all(&worktree).ok();
}

#[test]
fn linked_created_dir_watch_signal_does_not_short_circuit_state_updates() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let first =
        std::env::temp_dir().join(format!("ragrat-wt-create-all-a-{}-{id}", std::process::id()));
    let second =
        std::env::temp_dir().join(format!("ragrat-wt-create-all-b-{}-{id}", std::process::id()));
    for root in [&first, &second] {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "").unwrap();
    }
    let first = first.canonicalize().unwrap();
    let second = second.canonicalize().unwrap();

    let target_dirs = vec![PathBuf::from("src")];
    let config = whole_root_config(&first, &target_dirs);
    let mut watcher = RecordingWatcher::default();
    let mut worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        first.clone(),
        second.clone(),
    ]);

    watcher.watched.clear();
    let first_pkg = first.join("src/pkg");
    let second_pkg = second.join("src/pkg");
    std::fs::create_dir_all(&first_pkg).unwrap();
    std::fs::create_dir_all(&second_pkg).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder))
        .add_path(first_pkg.clone())
        .add_path(second_pkg.clone());

    assert_eq!(
        worktrees.watch_created_dirs(&mut watcher, &placement_counters(), &create),
        BTreeSet::from([first.clone(), second.clone()]),
        "BOTH linked states place their watch and are reported (no short-circuit)",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &first_pkg),
        "the first linked state should still be updated",
    );
    assert!(
        watcher.watched.iter().any(|(path, _)| path == &second_pkg),
        "the second linked state should still be updated after the first returns true",
    );

    std::fs::remove_dir_all(&first).ok();
    std::fs::remove_dir_all(&second).ok();
}

#[test]
fn watch_created_dirs_skips_dirs_ignored_before_or_after_recompile() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("ragrat-watch-created-ignore-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("src/already_ignored")).unwrap();
    std::fs::create_dir_all(root.join("src/newly_ignored")).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "src/already_ignored/\n").unwrap();

    let target_dirs = vec![PathBuf::from("src")];
    let config = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    let mut watcher = RecordingWatcher::default();

    let already = root.join("src/already_ignored");
    let create_already =
        Event::new(EventKind::Create(CreateKind::Folder)).add_path(already.clone());
    watch_created_dirs(
        &mut watcher,
        &placement_counters(),
        &create_already,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    assert!(
        watcher.watched.iter().all(|(path, _)| path != &already),
        "a dir ignored before recompile should not be watched",
    );

    std::fs::write(root.join(".gitignore"), "src/already_ignored/\nsrc/newly_ignored/\n").unwrap();
    let newly = root.join("src/newly_ignored");
    let create_newly = Event::new(EventKind::Create(CreateKind::Folder)).add_path(newly.clone());
    watch_created_dirs(
        &mut watcher,
        &placement_counters(),
        &create_newly,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    assert!(
        watcher.watched.iter().all(|(path, _)| path != &newly),
        "a dir ignored only after recompile should not be watched",
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn event_touches_worktree_rebases_subdir_rooted_config() {
    // #219 review: when `config.root` is a repo SUBDIR (`<repo>/crate`), a linked checkout's
    // edit arrives as `<checkout>/crate/src/a.rs`. Stripping only the checkout root leaves
    // `crate/src/a.rs`, which `target_for_path` (config-root-relative, expecting `src/a.rs`)
    // rejects — so the subdir prefix must be stripped too.
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let repo = std::env::temp_dir().join(format!("ragrat-wt-subdir-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("crate/src")).unwrap();
    std::fs::write(repo.join("crate/src/a.rs"), "fn a() {}\n").unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@e"],
        vec!["config", "user.name", "t"],
        vec!["add", "."],
        vec!["commit", "-q", "-m", "base"],
    ] {
        Command::new("git").args(&args).current_dir(&repo).output().unwrap();
    }
    // `config.root` is the `crate` SUBDIR of the repo.
    let config_root = repo.join("crate").canonicalize().unwrap();
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: config_root,
        database: repo.join("crate/.rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    // A linked checkout mirrors the layout: `<checkout>/crate/src/a.rs`.
    let checkout =
        std::env::temp_dir().join(format!("ragrat-wt-subdir-co-{}-{id}", std::process::id()));
    let mut watcher = RecordingWatcher::default();
    let worktrees = watch_linked_worktrees(&mut watcher, &placement_counters(), &config, vec![
        checkout.clone(),
    ]);
    assert!(
        event_touches_worktree(&mutation_event(checkout.join("crate/src/a.rs")), &worktrees, None,)
            .fires(),
        "a subdir-rooted config must fire on a linked edit under <checkout>/<subdir>/<target>"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn event_is_relevant_skips_gitignored_paths_consistently_with_walker() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-watchev-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("crates")).unwrap();
    std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();

    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("crates")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let ignore = IgnoreMatcher::compile(&root, &[]);

    // A real source edit under the target fires.
    let src = root.join("crates/lib.rs");
    assert!(event_is_relevant(&config, &ignore, &mutation_event(src)), "source edit fires");

    // A floor dir (target/) never fires, even though it would be language-matched.
    let built = root.join("target/debug/foo.rs");
    assert!(!event_is_relevant(&config, &ignore, &mutation_event(built)), "floor dir skipped");

    // A gitignored dir under root never fires.
    let generated = root.join("gen/out.rs");
    assert!(!event_is_relevant(&config, &ignore, &mutation_event(generated)), "gitignored skipped",);

    // A read of a watched source file never fires (anti-feedback gate), even if not ignored.
    let read = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
        .add_path(root.join("crates/lib.rs"));
    assert!(!event_is_relevant(&config, &ignore, &read), "reads never fire");

    // A creation under the target fires.
    let created =
        Event::new(EventKind::Create(CreateKind::File)).add_path(root.join("crates/new.rs"));
    assert!(event_is_relevant(&config, &ignore, &created), "new source file fires");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn gitignore_edit_is_relevant_and_recompile_reflects_new_rules() {
    // EARLIER-ROUND FINDINGS (kept correct): a `.gitignore` mutation must fire a pass even
    // though `.gitignore` is not a target language, AND recompiling the matcher must make
    // subsequent classification honor the new rules — so a now-ignored file stops firing and a
    // now-unignored file resumes.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-watchgi-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("crates")).unwrap();
    // Initially nothing is gitignored.
    std::fs::write(root.join(".gitignore"), "").unwrap();

    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: root.clone(),
        database: root.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("crates")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };

    let ignore = IgnoreMatcher::compile(&root, &[]);
    let secret = root.join("crates/secret.rs");
    // Before the rule edit: a normal source edit fires.
    assert!(event_is_relevant(&config, &ignore, &mutation_event(secret.clone())), "fires pre");

    // A `.gitignore` mutation is itself relevant (finding 4) — even a root `.gitignore`, and
    // even a nested one that has no target language.
    let gi_edit = mutation_event(root.join(".gitignore"));
    assert!(event_is_relevant(&config, &ignore, &gi_edit), "gitignore edit fires a pass");
    let nested_gi = mutation_event(root.join("crates/.gitignore"));
    assert!(event_is_relevant(&config, &ignore, &nested_gi), "nested gitignore edit fires");

    // Now the user adds `secret.rs` to `.gitignore`; recompiling must make the classifier drop
    // it.
    std::fs::write(root.join(".gitignore"), "secret.rs\n").unwrap();
    let ignore = IgnoreMatcher::compile(&root, &[]);
    assert!(
        !event_is_relevant(&config, &ignore, &mutation_event(secret)),
        "recompiled matcher honors the new ignore rule (now-ignored file stops firing)",
    );
    // A different, still-unignored source file keeps firing.
    let other = root.join("crates/keep.rs");
    assert!(event_is_relevant(&config, &ignore, &mutation_event(other)), "unignored still fires");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn worktree_root_gitignore_edit_recompiles_for_subdir_config_root() {
    // FINDING 1 + 3 combined (test d for the subdirectory case): `config.root` is a subdir of a
    // Git worktree. A live edit to the WORKTREE-ROOT `.gitignore` (above config.root) must,
    // after recompiling the shared matcher, drop a now-ignored file under the subdir and keep
    // an unrelated one firing — proving ancestor rules are honored AND the recompile
    // takes effect.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let wt = std::env::temp_dir().join(format!("ragrat-wtgi-{}-{id}", std::process::id()));
    std::fs::create_dir_all(wt.join("crates")).unwrap();
    let wt = wt.canonicalize().unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&wt)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init failed (git must be on PATH)");
    std::fs::write(wt.join(".gitignore"), "").unwrap();

    let sub = wt.join("crates"); // config.root is the subdirectory.
    let target_dirs = vec![PathBuf::from(".")];
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: sub.clone(),
        database: sub.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: target_dirs.clone(),
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };

    // Before: a source file under the subdir fires.
    let ignore = IgnoreMatcher::compile(&sub, &target_dirs);
    let secret = sub.join("secret.rs");
    assert!(event_is_relevant(&config, &ignore, &mutation_event(secret.clone())), "fires pre");

    // Edit the WORKTREE-ROOT `.gitignore` to ignore `secret.rs` repo-wide, then recompile.
    std::fs::write(wt.join(".gitignore"), "secret.rs\n").unwrap();
    let ignore = IgnoreMatcher::compile(&sub, &target_dirs);
    assert!(
        !event_is_relevant(&config, &ignore, &mutation_event(secret)),
        "worktree-root rule (above config.root) drops the file after recompile (finding 1 + 3)",
    );
    assert!(
        event_is_relevant(&config, &ignore, &mutation_event(sub.join("keep.rs"))),
        "an unrelated source file under the subdir still fires",
    );

    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn debounce_fires_after_quiet_window() {
    let mut d = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
    let t0 = Instant::now();
    d.on_event(t0);
    assert!(!d.should_fire(t0 + Duration::from_millis(399)), "fires before quiet window");
    assert!(d.should_fire(t0 + Duration::from_millis(400)), "fires at quiet window");
}

#[test]
fn debounce_max_latency_cap_beats_sustained_events() {
    let debounce = Duration::from_millis(400);
    let max = Duration::from_millis(2500);
    let mut d = Debounce::new(debounce, max);
    let t0 = Instant::now();
    d.on_event(t0);
    // A steady stream of events every 200ms keeps the quiet window from ever elapsing...
    let mut now = t0;
    for _ in 0..100 {
        now += Duration::from_millis(200);
        d.on_event(now);
        if now >= t0 + max {
            break;
        }
        assert!(!d.should_fire(now), "should not fire mid-stream before the cap");
    }
    // ...but the max-latency cap forces a fire at first + max_latency regardless.
    assert!(d.should_fire(t0 + max), "max-latency cap must force a pass under sustained writes");
}

#[test]
fn reads_are_not_mutations_but_writes_are() {
    use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
    // Reads must never fire — this is the anti-feedback-loop gate.
    assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Open(AccessMode::Read))));
    assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
    assert!(!kind_is_mutation(&EventKind::Access(AccessKind::Any)));
    // Real content changes must fire.
    assert!(kind_is_mutation(&EventKind::Create(CreateKind::File)));
    assert!(kind_is_mutation(&EventKind::Remove(RemoveKind::File)));
    assert!(kind_is_mutation(&EventKind::Modify(ModifyKind::Data(DataChange::Any))));
    assert!(kind_is_mutation(&EventKind::Access(AccessKind::Close(AccessMode::Write))));
}

#[test]
fn debounce_idle_has_no_deadline() {
    let d = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
    assert!(d.due_in(Instant::now()).is_none());
    assert!(!d.should_fire(Instant::now()));
}

#[test]
fn scoped_passes_do_not_postpone_the_periodic_all_sweep() {
    // #577 review (PR): the periodic backstop measures time since the last ALL-scoped pass
    // COMPLETED — event-scoped passes don't perform sweep duties (unlisted-worktree refresh,
    // overlay embed-backlog retries), so a steady drip of scoped passes must escalate the
    // next pass to `All` once the interval elapses, not keep postponing it.
    let start = Instant::now();
    let interval = Duration::from_secs(300);
    let mut clock = SweepClock::new(Some(interval), start);

    // The startup catch-up (an All pass) is in flight at construction; its completion resets.
    clock.on_pass_done(start + Duration::from_secs(5));
    assert!(!clock.due(start + Duration::from_secs(300)), "counts from startup COMPLETION");

    // Scoped passes churn away past the interval — none of them reset the sweep clock.
    for i in 0..10 {
        clock.on_dispatch(false);
        clock.on_pass_done(start + Duration::from_secs(6 + i * 60));
    }
    assert!(
        clock.due(start + Duration::from_secs(5) + interval),
        "scoped passes do not postpone the sweep"
    );
    assert_eq!(
        clock.due_in(start + Duration::from_secs(6)),
        Some(Duration::from_secs(299)),
        "the wait deadline also measures from the last ALL completion"
    );

    // An ALL pass (periodic or gc-widened) resets the clock at its COMPLETION.
    clock.on_dispatch(true);
    let sweep_done = start + Duration::from_secs(700);
    clock.on_pass_done(sweep_done);
    assert!(!clock.due(sweep_done + interval - Duration::from_secs(1)));
    assert!(clock.due(sweep_done + interval));
}

#[test]
fn a_disabled_periodic_sweep_is_never_due() {
    let start = Instant::now();
    let mut clock = SweepClock::new(None, start);
    clock.on_pass_done(start + Duration::from_secs(1));
    clock.on_dispatch(false);
    clock.on_pass_done(start + Duration::from_secs(2));
    assert!(!clock.due(start + Duration::from_secs(1_000_000)));
    assert_eq!(clock.due_in(start), None);
}

#[test]
fn scheduler_coalesces_fire_requests_while_a_pass_is_in_flight() {
    let base_only = || OverlayScope::Linked(BTreeSet::new());
    let mut scheduler = PassScheduler::new();
    assert_eq!(
        scheduler.dispatch(base_only()),
        Some(PassRequest::Maintenance { run_gc: false, overlay_scope: base_only() })
    );
    assert!(scheduler.in_flight());
    assert_eq!(scheduler.dispatch(base_only()), None, "a fire while a pass runs must coalesce");
    scheduler.on_done();
    assert_eq!(
        scheduler.dispatch(base_only()),
        Some(PassRequest::Maintenance { run_gc: false, overlay_scope: base_only() }),
        "the coalesced fire dispatches once the pass completes",
    );
}

#[test]
fn the_pass_cooldown_holds_the_coalesced_follow_up_until_it_elapses() {
    // #823: a debounce armed by mid-pass events survives the whole (minutes-long) pass — it
    // resets only on dispatch — so at PassDone it is long past due and, unchecked, the coalesced
    // follow-up dispatches back-to-back (107 passes in ~5 h measured). The cooldown gates that
    // dispatch until it has elapsed since the pass COMPLETED.
    let t0 = Instant::now();
    let mut debounce = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
    let mut cooldown = PassCooldown::new(Some(Duration::from_secs(60)));

    // Before any pass completes there is nothing to cool down from.
    assert!(cooldown.ready(t0), "no completed pass yet — dispatch is not held back");

    // An event lands while a pass is in flight; the debounce stays armed across the pass.
    debounce.on_event(t0);
    let pass_done = t0 + Duration::from_secs(120);
    cooldown.on_pass_done(pass_done);

    // Immediately after PassDone the debounce is long past due — the #823 precondition...
    assert!(debounce.should_fire(pass_done), "the armed debounce elapsed during the pass");
    // ...but the cooldown holds the dispatch until it elapses from the COMPLETION instant.
    assert!(!cooldown.ready(pass_done), "no immediate back-to-back redispatch");
    assert!(!cooldown.ready(pass_done + Duration::from_secs(59)));
    assert!(cooldown.ready(pass_done + Duration::from_secs(60)), "ready once elapsed");
}

#[test]
fn a_zero_pass_cooldown_preserves_immediate_redispatch() {
    // `pass_cooldown_secs = 0` disables the gate entirely: the follow-up may dispatch at the
    // very instant the pass completes — exactly the pre-#823 behavior (the loop-level pin is
    // `a_pass_in_flight_does_not_starve_events_or_the_fleet_trigger`, which runs cooldown-free).
    let mut cooldown = PassCooldown::new(None);
    let t0 = Instant::now();
    cooldown.on_pass_done(t0);
    assert!(cooldown.ready(t0), "a disabled cooldown never holds dispatch back");
    let mut armed = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
    armed.on_event(t0);
    assert_eq!(
        cooldown.gate_debounce_wait(&armed, t0),
        Some(Duration::from_millis(400)),
        "a disabled cooldown is transparent to the recv-wait — the raw debounce deadline stands",
    );
}

#[test]
fn a_held_back_debounce_sleeps_out_the_cooldown_instead_of_spinning() {
    // #823 spin-avoidance: after PassDone the armed debounce is already past due while dispatch
    // is held back, so the raw debounce deadline of zero would wake the loop every iteration.
    // The recv-wait slot must be the cooldown's REMAINING time — the loop sleeps until dispatch
    // is actually allowed (the injected-clock analogue of counting loop iterations).
    let t0 = Instant::now();
    let mut debounce = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
    let mut cooldown = PassCooldown::new(Some(Duration::from_secs(60)));
    debounce.on_event(t0);
    cooldown.on_pass_done(t0 + Duration::from_secs(120));

    let mid_cooldown = t0 + Duration::from_secs(130);
    assert_eq!(debounce.due_in(mid_cooldown), Some(Duration::ZERO), "debounce is past due");
    assert_eq!(
        cooldown.gate_debounce_wait(&debounce, mid_cooldown),
        Some(Duration::from_secs(50)),
        "the wait is the cooldown remainder, not a zero-length spin",
    );
    assert_eq!(
        cooldown.gate_debounce_wait(&debounce, t0 + Duration::from_secs(180)),
        Some(Duration::ZERO),
        "once the cooldown elapses the past-due debounce fires without further delay",
    );
    // An idle debounce has no deadline: the cooldown alone never wakes the loop (there is
    // nothing to dispatch when it elapses).
    let idle = Debounce::new(Duration::from_millis(400), Duration::from_millis(2500));
    assert_eq!(cooldown.gate_debounce_wait(&idle, mid_cooldown), None);
}

#[test]
fn scheduler_gc_cadence_counts_maintenance_passes_only() {
    let mut scheduler = PassScheduler::new();
    assert_eq!(scheduler.dispatch_startup(), PassRequest::StartupCatchup);
    assert!(scheduler.in_flight(), "the startup catch-up occupies the in-flight slot");
    scheduler.on_done();
    for pass in 1..=GC_EVERY_PASSES {
        let request = scheduler.dispatch(OverlayScope::All).expect("no pass is in flight");
        assert_eq!(
            request,
            PassRequest::Maintenance {
                run_gc: pass == GC_EVERY_PASSES,
                overlay_scope: OverlayScope::All
            },
            "gc runs on pass {GC_EVERY_PASSES}, not on pass {pass}",
        );
        scheduler.on_done();
    }
}

/// #506: the worker runs requests in order, answers each with `PassDone` on the loop channel,
/// and exits when the request channel closes.
#[test]
fn pass_worker_runs_requests_in_order_and_reports_completion() {
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
    let worker = spawn_pass_worker(pass_rx, done_tx, {
        let ran = Arc::clone(&ran);
        move |request: &PassRequest| ran.lock().unwrap().push(request.clone())
    })
    .expect("worker thread spawns");
    pass_tx.send(PassRequest::StartupCatchup).unwrap();
    pass_tx
        .send(PassRequest::Maintenance { run_gc: true, overlay_scope: OverlayScope::All })
        .unwrap();
    for _ in 0..2 {
        match done_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(LoopMsg::PassDone) => {},
            other => panic!("expected PassDone, got {other:?}"),
        }
    }
    drop(pass_tx);
    worker.join().unwrap();
    assert_eq!(*ran.lock().unwrap(), vec![PassRequest::StartupCatchup, PassRequest::Maintenance {
        run_gc: true,
        overlay_scope: OverlayScope::All
    },]);
}

/// The #506 regression: while a maintenance pass is in flight, the event loop must keep
/// classifying events and must fire the fleet hot-upgrade trigger — the exact window where a
/// `cargo install` used to land unseen. The test plays the pass worker itself and withholds
/// the completion until the end.
#[test]
fn a_pass_in_flight_does_not_starve_events_or_the_fleet_trigger() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let fleet_bin = root.join("rag-rat-506-test-bin");
    std::fs::write(&fleet_bin, b"binary").unwrap();

    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    config.watch.debounce_ms = 10;
    config.watch.max_latency_ms = 50;
    config.watch.periodic_sweep_secs = 0;
    // This test pins #506 coalescing, not pacing: with the cooldown DISABLED the coalesced
    // follow-up must dispatch immediately on PassDone — exactly the pre-#823 behavior.
    config.watch.pass_cooldown_secs = 0;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let (fleet_tx, fleet_rx) = std::sync::mpsc::channel();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = move |bin: &Path| {
        let _ = fleet_tx.send(bin.to_path_buf());
    };

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: Some(&fleet_bin),
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let loop_thread = scope.spawn(move || event_loop.run());

        // A relevant BASE edit dispatches a base-only pass to the worker (played by this
        // test) — no linked checkout is implicated, so no overlay is swept (#577).
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
        );

        // While that pass is in flight (no PassDone), a new binary landing must still fire
        // the fleet trigger...
        tx.send(LoopMsg::Fs(Ok(mutation_event(fleet_bin.clone())))).unwrap();
        assert_eq!(
            fleet_rx.recv_timeout(Duration::from_secs(5)),
            Ok(fleet_bin.clone()),
            "the fleet trigger must fire during a pass, not after it",
        );
        // ...and a further edit is classified, coalescing into the armed debounce instead of
        // dispatching a concurrent pass.
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_millis(300)),
            Err(RecvTimeoutError::Timeout),
            "no second pass may dispatch while one is in flight",
        );

        // Completing the pass dispatches the coalesced follow-up.
        tx.send(LoopMsg::PassDone).unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
        );

        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        let final_refresh_owed = loop_thread.join().unwrap();
        assert!(!final_refresh_owed, "every observed edit was consumed by a dispatched pass",);
    });
}

/// The #823 regression, end-to-end: events arriving during a pass leave the debounce armed and
/// long-elapsed at `PassDone`, and the loop used to dispatch the coalesced follow-up immediately —
/// back-to-back passes for as long as editing continued. With a cooldown configured, the follow-up
/// must wait it out (counted from pass COMPLETION) and then dispatch.
#[test]
fn events_during_a_pass_do_not_redispatch_until_the_cooldown_elapses() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();

    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    config.watch.debounce_ms = 10;
    config.watch.max_latency_ms = 50;
    config.watch.periodic_sweep_secs = 0;
    config.watch.pass_cooldown_secs = 2;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let loop_thread = scope.spawn(move || event_loop.run());

        // A relevant edit dispatches the first pass (this test plays the worker).
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
        );

        // More edits land while the pass is in flight — they coalesce into the armed debounce.
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        // Let the debounce (10 ms) and even the max-latency cap (50 ms) elapse BEFORE the pass
        // completes — the production shape: the pass runs minutes, so at PassDone the armed
        // debounce is already past due and the loop's own PassDone iteration is where the
        // back-to-back redispatch used to happen.
        std::thread::sleep(Duration::from_millis(100));
        tx.send(LoopMsg::PassDone).unwrap();

        // The pre-#823 behavior was an immediate redispatch here. The follow-up must instead
        // wait out the cooldown (2 s; the 700 ms probe leaves ample CI-jitter margin)...
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_millis(700)),
            Err(RecvTimeoutError::Timeout),
            "the coalesced follow-up must not dispatch before the cooldown elapses",
        );
        // ...and then dispatch, carrying the coalesced scope.
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(10)),
            Ok(PassRequest::Maintenance {
                run_gc: false,
                overlay_scope: OverlayScope::Linked(BTreeSet::new())
            }),
            "the follow-up dispatches once the cooldown elapses",
        );

        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        let final_refresh_owed = loop_thread.join().unwrap();
        assert!(!final_refresh_owed, "the held-back edit was consumed by the delayed follow-up");
    });
}

/// #823: the periodic sweep is the missed-event backstop and must never be starved by the
/// cooldown. With an absurdly long cooldown armed by a completed pass, a due sweep still
/// dispatches on time.
#[test]
fn a_due_periodic_sweep_overrides_the_pass_cooldown() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();

    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    // No debounce fires (nothing event-driven); the sweep is due every second while the
    // cooldown would hold event-driven dispatch for an hour.
    config.watch.debounce_ms = 60_000;
    config.watch.max_latency_ms = 60_000;
    config.watch.periodic_sweep_secs = 1;
    config.watch.pass_cooldown_secs = 3_600;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());

        // The first periodic sweep dispatches; completing it arms the hour-long cooldown.
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance { run_gc: false, overlay_scope: OverlayScope::All }),
        );
        tx.send(LoopMsg::PassDone).unwrap();

        // The next sweep falls due one second later — deep inside the cooldown — and must
        // dispatch anyway: the backstop bypasses the cooldown.
        assert_eq!(
            pass_rx.recv_timeout(Duration::from_secs(5)),
            Ok(PassRequest::Maintenance { run_gc: false, overlay_scope: OverlayScope::All }),
            "a due periodic sweep must never be starved by the pass cooldown",
        );

        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        let _ = handle.join();
    });
}

#[test]
fn reconcile_budget_is_shared_across_overlays_and_base() {
    // #219 review: each overlay reconcile (and the base) starts its OWN `max_seconds` timer, so
    // handing every one the same options lets the pass spend (N+1)× the advertised budget.
    // `next_options` recomputes `max_seconds` from the time remaining in the shared budget.
    let options = ReconcileOptions { max_seconds: Some(30), ..ReconcileOptions::default() };
    // A budget whose clock STARTED 30s ago is already exhausted → skip the reconcile.
    let spent =
        ReconcileBudget::new(options.clone(), Instant::now() - std::time::Duration::from_secs(30));
    assert!(spent.next_options().is_none(), "an exhausted budget yields no reconcile");

    // A fresh budget yields options whose `max_seconds` is at most the total (the remaining
    // time), never a fresh full budget per call.
    let fresh = ReconcileBudget::new(options, Instant::now());
    let next = fresh.next_options().expect("a fresh budget has time left");
    assert!(
        next.max_seconds.is_some_and(|s| s <= 30),
        "the per-call budget is bounded by the time remaining, not a fresh full budget: {:?}",
        next.max_seconds,
    );

    // An uncapped budget (`max_seconds: None`) always yields the base options.
    let uncapped = ReconcileBudget::new(ReconcileOptions::default(), Instant::now());
    assert_eq!(uncapped.next_options().and_then(|o| o.max_seconds), None);
}

#[test]
fn worktree_watch_targets_excludes_the_main_checkout_for_a_subdir_config_root() {
    // #219 review: when `config.root` is a repo SUBDIR (`<repo>/crate`),
    // `live_worktree_contexts` reports the main checkout as `<repo>` (its workdir), but
    // filtering by `worktree_id_of(config.root)` (`<repo>/crate`) wouldn't match — so
    // the main checkout would be misread as a LINKED worktree and the watcher would
    // recursively subscribe to the whole repo root. The base id must be the ENCLOSING
    // worktree root.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let main = std::env::temp_dir().join(format!("ragrat-wwt-{}-{id}", std::process::id()));
    std::fs::create_dir_all(main.join("crate/src")).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap()
    };
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("crate/src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    let sub = main.join("crate").canonicalize().unwrap(); // config.root is the subdir.
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: sub.clone(),
        database: sub.join(".rag-rat/index.sqlite"),
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };

    let (roots, _registry) = worktree_watch_targets(&config);
    let main_id = crate::index::worktree_id_of(&main.canonicalize().unwrap());
    assert!(
        !roots.iter().any(|r| crate::index::worktree_id_of(r) == main_id),
        "the main checkout must NOT be watched as a linked worktree: {roots:?}",
    );

    std::fs::remove_dir_all(&main).ok();
}

#[test]
fn gitignore_watch_dirs_includes_worktree_root_for_subdir_config_root() {
    // FINDING 1 (round 3): when `config.root` is a subdirectory of a Git worktree, the watcher
    // must also subscribe to the ancestor chain up to the worktree root so a root-`.gitignore`
    // edit (which lives ABOVE the recursively-watched target dirs) is delivered.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let wt = std::env::temp_dir().join(format!("ragrat-wdirs-{}-{id}", std::process::id()));
    std::fs::create_dir_all(wt.join("crates/app")).unwrap();
    let wt = wt.canonicalize().unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&wt)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init failed (git must be on PATH)");

    let sub = wt.join("crates/app");
    let dirs = gitignore_watch_dirs(&sub);
    // The chain from the worktree root down to config.root, inclusive.
    assert_eq!(dirs.first(), Some(&wt), "worktree root is watched (finding 1)");
    assert!(dirs.contains(&wt.join("crates")), "intermediate ancestor watched");
    assert_eq!(dirs.last(), Some(&sub), "config.root itself is watched");

    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn gitignore_watch_dirs_non_git_tree_is_just_root() {
    // Outside a Git worktree the chain collapses to just `config.root` (already covered by the
    // recursive target watches) — no ancestor sweep above an un-versioned directory.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-wdirs-ng-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    // Best-effort: only meaningful when /tmp isn't itself inside a git worktree. If it is,
    // skip.
    if crate::index::git_history::worktree_root(&root).is_some() {
        std::fs::remove_dir_all(&root).ok();
        return;
    }
    assert_eq!(gitignore_watch_dirs(&root), vec![root.clone()]);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn root_gitignore_edit_is_delivered_to_a_real_watcher() {
    // FINDING 1 end-to-end (test d): a live edit to the worktree-root `.gitignore` — which sits
    // ABOVE the target dirs — must actually be *delivered* by the notify watcher once we
    // subscribe to `gitignore_watch_dirs`. We spawn a real recommended_watcher over exactly the
    // dirs the watcher subscribes to (target dir + ancestor chain) and assert the edit arrives.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let wt = std::env::temp_dir().join(format!("ragrat-deliv-{}-{id}", std::process::id()));
    std::fs::create_dir_all(wt.join("crates")).unwrap();
    let wt = wt.canonicalize().unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&wt)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init failed (git must be on PATH)");
    std::fs::write(wt.join(".gitignore"), "").unwrap();

    let sub = wt.join("crates"); // config.root is the subdirectory.
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&wt).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    // Subscribe exactly as watcher_main does: the gitignore-pruned target subtree (issue #331)
    // + the ancestor gitignore chain. The root `.gitignore` edit is delivered by the chain
    // watch, not the target subtree, so the pruned placement doesn't weaken this assertion.
    let ignore = IgnoreMatcher::compile(&sub, &[PathBuf::from(".")]);
    watch_tree_pruned(&mut w, &placement_counters(), &sub, &ignore);
    for dir in gitignore_watch_dirs(&sub) {
        let _ = w.watch(&dir, RecursiveMode::NonRecursive);
    }

    // Edit the worktree-root `.gitignore` (above config.root).
    std::fs::write(wt.join(".gitignore"), "secret.rs\n").unwrap();

    // Drain events for up to ~3s; assert at least one references the root `.gitignore`.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut delivered = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(event)) =>
                if event.paths.iter().any(|p| is_gitignore_path(p)) {
                    delivered = true;
                    break;
                },
            Ok(Err(_)) => {},
            Err(RecvTimeoutError::Timeout) => {},
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(w);
    std::fs::remove_dir_all(&wt).ok();
    assert!(delivered, "root .gitignore edit above config.root must be delivered (finding 1)");
}

#[cfg(target_os = "linux")]
#[test]
fn watcher_main_routes_gitignore_mutations_through_central_helpers() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-watch-loop-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let status =
            std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    };
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "t@e"]);
    git(&root, &["config", "user.name", "t"]);
    std::fs::write(root.join(".gitignore"), "").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "seed"]);
    let root = root.canonicalize().unwrap();

    let mut config = whole_root_config(&root, &[PathBuf::from("src")]);
    config.watch.debounce_ms = 20;
    config.watch.max_latency_ms = 50;
    config.watch.periodic_sweep_secs = 0;
    let watcher = Watcher::spawn(config).expect("real watcher should start");
    let db = root.join(".rag-rat/index.sqlite");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !db.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(db.exists(), "startup maintenance pass should create the index");

    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(watcher);
    std::fs::remove_dir_all(&root).ok();
}

/// Drain notify events for up to `secs` seconds; return whether any event references a path
/// under `needle`. Shared by the issue-#331 placement tests below.
fn drain_until_path_under(
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
fn drain_until_quiet(
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

// Linux/inotify only: this asserts the watch-PLACEMENT optimization (an ignored subtree gets no
// watch, so its edits are never delivered) — the mitigation for inotify `max_user_watches`
// exhaustion that motivated #331/#332. inotify places one NON-recursive watch per directory, so
// a dir that is never watched delivers nothing. The other backends coalesce differently:
// `ReadDirectoryChangesW` (Windows) and FSEvents (macOS) report the ignored DIRECTORY entry's
// mtime bump from a nested write on the parent's watch, so placement can't suppress delivery
// and the outcome is timing-dependent (this test fails on Windows and flakes on macOS).
// That is harmless — the CLASSIFICATION filter (`event_is_relevant`) drops the ignored path
// before any indexing, and THAT guarantee is verified on every OS by
// `event_is_relevant_skips_gitignored_paths_consistently_with_walker`. See #446.
#[cfg(target_os = "linux")]
#[test]
fn gitignored_subdir_under_a_target_is_not_watched() {
    // ISSUE #331: a gitignored directory under a target dir must NOT receive an inotify watch
    // (that's how a recursive watch exhausted `fs.inotify.max_user_watches`). End-to-end: an
    // edit inside the ignored subtree is never delivered, while an edit to a non-ignored
    // sibling is — proving placement, not just classification, honors `.gitignore`.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-331ign-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("ignored_dir/nested")).unwrap();
    std::fs::create_dir_all(root.join("kept_dir/nested")).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "ignored_dir/\n").unwrap();
    // Seed a file in each so the dirs exist before the watch is placed.
    std::fs::write(root.join("ignored_dir/nested/a.rs"), "// a\n").unwrap();
    std::fs::write(root.join("kept_dir/nested/b.rs"), "// b\n").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    // Place watches exactly as watcher_main does: the gitignore-pruned target subtree.
    let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from(".")]);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    drain_until_quiet(&rx, 100, 1000);

    // A write inside the gitignored subtree must NOT be delivered (the dir was never watched).
    let ignored_probe = root.join("ignored_dir/nested");
    std::fs::write(ignored_probe.join("a.rs"), "// a edited\n").unwrap();
    let ignored_seen = drain_until_path_under(&rx, &ignored_probe, 2);

    // A write to a non-ignored sibling under the same target MUST be delivered.
    let kept_probe = root.join("kept_dir/nested");
    std::fs::write(kept_probe.join("b.rs"), "// b edited\n").unwrap();
    let kept_seen = drain_until_path_under(&rx, &kept_probe, 3);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(!ignored_seen, "an edit inside a gitignored subtree must not be delivered (#331)");
    assert!(kept_seen, "an edit in a non-ignored sibling must still be delivered");
}

#[test]
fn newly_created_non_ignored_dir_gets_watched() {
    // ISSUE #331: target dirs are watched NON-recursively, so a directory created AFTER the
    // watch is placed needs an explicit pruned watch (`watch_created_dirs`), or edits inside it
    // would never fire. End-to-end: create a dir post-spawn, run the create-event handling,
    // then write a file inside it and assert the change is delivered.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-331new-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "ignored_dir/\n").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from(".")];
    let config = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);

    // Create a NEW non-ignored directory after the initial placement.
    let fresh = root.join("fresh_dir");
    std::fs::create_dir_all(&fresh).unwrap();
    // Feed the create event through the same handler watcher_main runs, which places the watch.
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(fresh.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &create,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );

    // A write inside the freshly-watched dir must now be delivered.
    std::fs::write(fresh.join("c.rs"), "// c\n").unwrap();
    let seen = drain_until_path_under(&rx, &fresh, 3);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(seen, "an edit in a newly-created non-ignored dir must be delivered (#331)");
}

#[test]
fn a_bare_directory_create_is_not_relevant_so_placement_must_be_unconditional() {
    // ISSUE #332 (P1): a new subdir under a NonRecursive-watched target (#331) needs its own
    // watch via `watch_created_dirs`. But a bare `mkdir src/foo` is NOT a relevant event — a
    // directory is extensionless, so it matches no `**/*.rs` target glob — which is exactly why
    // `watch_created_dirs` must run UNCONDITIONALLY in the loop, NOT gated behind
    // `event_is_relevant`. The original bug gated it, so new dirs were never watched and their
    // files stayed invisible until the periodic sweep.
    use std::sync::atomic::{AtomicU64, Ordering};

    use notify::event::CreateKind;
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-332rel-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
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
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from("src")]);
    let dir_create =
        Event::new(EventKind::Create(CreateKind::Folder)).add_path(root.join("src/foo"));
    let file_create =
        Event::new(EventKind::Create(CreateKind::File)).add_path(root.join("src/foo/lib.rs"));
    std::fs::remove_dir_all(&root).ok();
    assert!(
        !event_is_relevant(&config, &ignore, &dir_create),
        "a bare directory create must NOT be relevant — so watch_created_dirs must be \
         unconditional (#332 P1)",
    );
    assert!(
        event_is_relevant(&config, &ignore, &file_create),
        "the FILE under it IS relevant — but its event only arrives if src/foo was watched first",
    );
}

#[test]
fn a_directory_moved_into_a_target_is_watched() {
    // ISSUE #332: moving a directory INTO a watched target (`mv /tmp/pkg src/pkg`) is reported
    // as a name Modify (`RenameMode::To`), not a Create — `watch_created_dirs` must handle it
    // too, or edits under the moved dir are missed (the parent is NonRecursive, #331).
    use std::sync::atomic::{AtomicU64, Ordering};

    use notify::event::{ModifyKind, RenameMode};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-332mv-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from(".")];
    let config = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    // Simulate `mv` landing a directory into the target: create it, then feed a rename-To
    // event.
    let moved = root.join("moved_pkg");
    std::fs::create_dir_all(&moved).unwrap();
    let rename =
        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To))).add_path(moved.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &rename,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    std::fs::write(moved.join("d.rs"), "// d\n").unwrap();
    let seen = drain_until_path_under(&rx, &moved, 3);
    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(seen, "an edit in a directory MOVED into a target must be delivered (#332)");
}

#[test]
fn relaxing_an_ignore_rule_re_places_watches_on_the_unignored_subtree() {
    // ISSUE #332: pruned watches are placed at startup against the then-current rules. If a
    // user REMOVES an ignore rule for an existing subtree, re-placing watches (after
    // recompiling the matcher) must add a watch for it — otherwise edits inside it
    // never fire a pass.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-332re-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("formerly_ignored")).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "formerly_ignored/\n").unwrap();
    std::fs::write(root.join("formerly_ignored/e.rs"), "// e\n").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    // Startup placement against the original rules: the dir is ignored → NOT watched.
    let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from(".")]);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    // Relax the rule, then recompile + RE-PLACE (what the loop now does on a `.gitignore`
    // edit).
    std::fs::write(root.join(".gitignore"), "").unwrap();
    let ignore = IgnoreMatcher::compile(&root, &[PathBuf::from(".")]);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    // An edit in the formerly-ignored (now eligible) subtree must now be delivered.
    std::fs::write(root.join("formerly_ignored/e.rs"), "// e edited\n").unwrap();
    let seen = drain_until_path_under(&rx, &root.join("formerly_ignored"), 3);
    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(
        seen,
        "after relaxing an ignore rule, re-placement must watch the unignored subtree (#332)",
    );
}

#[test]
#[cfg(unix)]
fn a_symlink_to_a_directory_is_not_followed_into_watches() {
    // ISSUE #332 (P2): `watch_created_dirs` must NOT follow a symlink-to-dir. A symlink created
    // (or moved) under a target pointing at a huge tree OUTSIDE config.root (a dep cache,
    // another checkout) would, if followed, make `watch_tree_pruned` recurse through it
    // and place watches outside the indexed root → re-exhaust inotify.
    // `symlink_metadata` reports the link as a link (`is_dir() == false`), so the path
    // is skipped.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-332sym-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();
    // A real directory OUTSIDE the target, with a file in it — the symlink's target.
    let outside = std::env::temp_dir().join(format!("ragrat-332symtgt-{}-{id}", id));
    std::fs::create_dir_all(&outside).unwrap();
    let outside = outside.canonicalize().unwrap();
    std::fs::write(outside.join("f.rs"), "// f\n").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from(".")];
    let config = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);

    // Symlink the outside dir UNDER the target, then feed its create event.
    let link = root.join("linked");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let create = Event::new(EventKind::Create(CreateKind::Folder)).add_path(link.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &create,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );

    // An edit to the file INSIDE the link target must NOT be delivered (the link wasn't
    // watched, and the outside dir is not under config.root at all).
    std::fs::write(outside.join("f.rs"), "// f edited\n").unwrap();
    let followed = drain_until_path_under(&rx, &outside, 2);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&outside).ok();
    assert!(!followed, "a symlink-to-dir must not be followed into watches (#332)");
}

#[test]
fn a_non_target_top_level_dir_is_not_watched() {
    // ISSUE #332 (P2): config.root is watched NON-recursively (the gitignore-chain ancestor
    // watches), so a create of a top-level dir OUTSIDE any target (`vendor/`, a sibling of the
    // `src` target) is delivered to the loop too. `watch_created_dirs` must gate on the
    // target relation so it never watches such a dir — it can't be indexed and would just burn
    // inotify watches. A new subdir UNDER the target still gets watched.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-332nt-{}-{id}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from("src")];
    let config = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    // Watch the target subtree + (mirroring watcher_main) config.root itself non-recursively,
    // so a top-level create is delivered here exactly as it would be in production.
    watch_tree_pruned(&mut w, &placement_counters(), &root.join("src"), &ignore);
    let _ = w.watch(&root, RecursiveMode::NonRecursive);

    // A NON-target top-level dir: created + its event fed → must NOT be watched. Probe a file
    // two levels deep (`vendor/sub/v.rs`) — a delivery there could ONLY come from a watch on
    // `vendor` (or below), never from the root's own NON-recursive watch, which sees the
    // top-level `vendor` entry but not its contents. (Probing `vendor/v.rs` would falsely match
    // the root watch's delivery of the direct `vendor` child.)
    let vendor = root.join("vendor");
    let vendor_sub = vendor.join("sub");
    std::fs::create_dir_all(&vendor_sub).unwrap();
    let vendor_ev = Event::new(EventKind::Create(CreateKind::Folder)).add_path(vendor.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &vendor_ev,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    std::fs::write(vendor_sub.join("v.rs"), "// v\n").unwrap();
    let vendor_seen = drain_until_path_under(&rx, &vendor_sub, 2);

    // A new dir UNDER the target: must be watched.
    let pkg = root.join("src/pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    let pkg_ev = Event::new(EventKind::Create(CreateKind::Folder)).add_path(pkg.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &pkg_ev,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    std::fs::write(pkg.join("p.rs"), "// p\n").unwrap();
    let pkg_seen = drain_until_path_under(&rx, &pkg, 3);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(!vendor_seen, "a non-target top-level dir must not be watched (#332)");
    assert!(pkg_seen, "a new dir under the target must still be watched");
}

// Linux/inotify only — same rationale as `gitignored_subdir_under_a_target_is_not_watched`:
// this asserts watch PLACEMENT (a nested-ignored moved-in subdir gets no watch). On
// Windows/macOS the nested write bumps the ignored dir entry's mtime and the parent watch
// reports it; classification still drops it. See #446.
#[cfg(target_os = "linux")]
#[test]
fn a_moved_in_dir_with_a_nested_gitignore_prunes_against_it() {
    // ISSUE #332 (P2): a dir MOVED into a target carrying its OWN nested `.gitignore` must be
    // pruned against that nested rule. The long-lived matcher was compiled before the subtree
    // existed, so it doesn't know the nested rule; `watch_created_dirs` recompiles before
    // walking so `watch_tree_pruned` skips the nested-ignored subdir.
    use std::sync::atomic::{AtomicU64, Ordering};

    use notify::event::{ModifyKind, RenameMode};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-332nest-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "").unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(mut w) = recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) else {
        std::fs::remove_dir_all(&root).ok();
        return; // no watcher backend available (sandboxed CI) — nothing to assert.
    };
    let target_dirs = vec![PathBuf::from(".")];
    let config = whole_root_config(&root, &target_dirs);
    let mut ignore = IgnoreMatcher::compile(&root, &target_dirs);
    watch_tree_pruned(&mut w, &placement_counters(), &root, &ignore);
    drain_until_quiet(&rx, 100, 1000);

    // Build the moved-in dir with a NESTED `.gitignore` ignoring `ignored_sub/`, plus a kept
    // sibling — all created BEFORE feeding the rename event (so the matcher was stale to them).
    let pkg = root.join("pkg");
    std::fs::create_dir_all(pkg.join("ignored_sub/deep")).unwrap();
    std::fs::create_dir_all(pkg.join("kept_sub/deep")).unwrap();
    std::fs::write(pkg.join(".gitignore"), "ignored_sub/\n").unwrap();
    std::fs::write(pkg.join("ignored_sub/deep/x.rs"), "// x\n").unwrap();
    std::fs::write(pkg.join("kept_sub/deep/y.rs"), "// y\n").unwrap();
    let rename =
        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To))).add_path(pkg.clone());
    watch_created_dirs(
        &mut w,
        &placement_counters(),
        &rename,
        &config,
        &target_dirs,
        &mut ignore,
        None,
    );
    drain_until_quiet(&rx, 100, 1000);

    // The nested-ignored subdir must NOT be watched; the kept sibling MUST be.
    let ignored_probe = pkg.join("ignored_sub/deep");
    std::fs::write(ignored_probe.join("x.rs"), "// x edited\n").unwrap();
    let ignored_seen = drain_until_path_under(&rx, &ignored_probe, 2);
    let kept_probe = pkg.join("kept_sub/deep");
    std::fs::write(kept_probe.join("y.rs"), "// y edited\n").unwrap();
    let kept_seen = drain_until_path_under(&rx, &kept_probe, 3);

    drop(w);
    std::fs::remove_dir_all(&root).ok();
    assert!(!ignored_seen, "a moved-in nested-.gitignore-ignored subdir must not be watched");
    assert!(kept_seen, "the kept sibling under the moved-in dir must be watched (#332)");
}

#[test]
fn watcher_spawn_is_disabled_when_watch_is_off_or_env_opt_out_is_set() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    config.watch.enabled = false;
    assert!(Watcher::spawn(config).is_none(), "disabled watch config must not spawn a thread");

    let mut enabled = whole_root_config(root, &[PathBuf::from("src")]);
    enabled.watch.enabled = true;
    // SAFETY: this test is the only one touching RAG_RAT_NO_WATCH in this process.
    unsafe {
        std::env::set_var("RAG_RAT_NO_WATCH", "1");
    }
    assert!(Watcher::spawn(enabled).is_none(), "RAG_RAT_NO_WATCH must suppress the watcher");
    unsafe {
        std::env::remove_var("RAG_RAT_NO_WATCH");
    }
}

#[test]
fn event_loop_ignores_fs_errors_and_exits_on_disconnect() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    config.watch.debounce_ms = 50;
    config.watch.max_latency_ms = 200;
    config.watch.periodic_sweep_secs = 0;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel::<PassRequest>();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };

    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        tx.send(LoopMsg::Fs(Err(notify::Error::generic("disk full")))).unwrap();
        tx.send(LoopMsg::Wake).unwrap();
        drop(tx);
        drop(pass_tx);
        let _ = pass_rx;
        let final_refresh_owed = handle.join().unwrap();
        assert!(!final_refresh_owed, "ignored errors and disconnect should not arm a refresh");
    });
}

#[test]
fn periodic_sweep_dispatches_all_overlay_scope() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    config.watch.debounce_ms = 60_000;
    config.watch.max_latency_ms = 60_000;
    config.watch.periodic_sweep_secs = 1;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: None,
        papertrail_interval: None,
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };

    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        let request = pass_rx.recv_timeout(Duration::from_secs(5)).expect("periodic sweep pass");
        assert_eq!(
            request,
            PassRequest::Maintenance { run_gc: false, overlay_scope: OverlayScope::All },
            "the periodic backstop must refresh every overlay"
        );
        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        let _ = handle.join();
    });
}

#[test]
fn shutdown_discover_skips_when_write_lock_is_held() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let config = whole_root_config(root, &[PathBuf::from("src")]);
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(&config);
    let holder_config = config.clone();
    let holder_repo = lock_repo.clone();
    let release = Arc::new(AtomicBool::new(false));
    let release_for_holder = Arc::clone(&release);
    let holder = std::thread::spawn(move || {
        let _held =
            rag_rat_base::locks::WriteLock::acquire_blocking(&holder_config.database, &holder_repo)
                .unwrap();
        while !release_for_holder.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    assert!(
        !shutdown_discover(&config).unwrap(),
        "shutdown discover must skip when another thread holds the write lock"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "shutdown discover should time out waiting for the lock, not block until it is released"
    );
    release.store(true, Ordering::Relaxed);
    holder.join().unwrap();
}

#[test]
fn papertrail_clock_is_never_due_without_an_interval_and_rearms_on_tick() {
    let start = Instant::now();
    let disabled = PapertrailClock::new(None, start);
    assert!(!disabled.due(start + Duration::from_secs(86_400)));
    assert_eq!(disabled.due_in(start), None);

    let mut clock = PapertrailClock::new(Some(Duration::from_secs(900)), start);
    assert!(!clock.due(start + Duration::from_secs(899)));
    assert!(clock.due(start + Duration::from_secs(900)));
    clock.on_tick(start + Duration::from_secs(900));
    assert!(!clock.due(start + Duration::from_secs(1_799)));
    assert!(clock.due(start + Duration::from_secs(1_800)));

    // A cadence that overflows Instant arithmetic is a deadline that never arrives — it must
    // not panic the watcher's wait computation.
    let oversized = PapertrailClock::new(Some(Duration::from_secs(u64::MAX)), start);
    assert!(!oversized.due(start + Duration::from_secs(86_400)));
    assert_eq!(oversized.due_in(start), None);
}

#[test]
fn papertrail_scheduler_single_flights_and_coalesces_max_wins() {
    use rag_rat_papertrail::AutosyncRequest;
    let mut scheduler = PapertrailScheduler::new();
    // Idle → dispatch immediately.
    assert_eq!(scheduler.admit(AutosyncRequest::Evaluate), Some(AutosyncRequest::Evaluate));
    // In flight → any number of requests coalesce into ONE pending follow-up, strongest wins —
    // and a later weaker request must not weaken it.
    assert_eq!(scheduler.admit(AutosyncRequest::Incremental), None);
    assert_eq!(scheduler.admit(AutosyncRequest::Full), None);
    assert_eq!(scheduler.admit(AutosyncRequest::Evaluate), None);
    // Completion dispatches the coalesced follow-up (the scheduler is in flight again)...
    assert_eq!(scheduler.on_done(), Some(AutosyncRequest::Full));
    // ...and the next completion, with nothing queued, dispatches nothing.
    assert_eq!(scheduler.on_done(), None);
    assert_eq!(scheduler.admit(AutosyncRequest::Evaluate), Some(AutosyncRequest::Evaluate));
}

#[test]
fn papertrail_tick_interval_requires_bindings_and_takes_the_tightest_cadence() {
    let tmp = tempfile::TempDir::new().unwrap();
    // No `[[tracker]]` bindings and no git remote to auto-detect one from → disabled.
    let mut config = whole_root_config(tmp.path(), &[PathBuf::from("src")]);
    assert_eq!(papertrail_tick_interval(&config), None);

    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        base_url: None,
        auth: None,
        tags: Vec::new(),
    }];
    assert_eq!(papertrail_tick_interval(&config), Some(Duration::from_secs(900)));
    // The daily full-walk backstop shares the wake-up: a tighter full interval tightens it.
    config.papertrail.full_sync_interval_secs = 600;
    assert_eq!(papertrail_tick_interval(&config), Some(Duration::from_secs(600)));
}

#[test]
fn idle_watcher_enqueues_papertrail_evaluation_without_filesystem_activity() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    config.watch.debounce_ms = 60_000;
    config.watch.max_latency_ms = 60_000;
    config.watch.periodic_sweep_secs = 0;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, _pass_rx) = std::sync::mpsc::channel();
    let (papertrail_tx, papertrail_rx) = std::sync::mpsc::channel();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx,
        scheduler: &mut scheduler,
        papertrail_tx: Some(&papertrail_tx),
        papertrail_interval: Some(Duration::from_millis(50)),
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());
        // No filesystem events at all: the deadline alone must enqueue an evaluation.
        assert_eq!(
            papertrail_rx.recv_timeout(Duration::from_secs(5)),
            Ok(rag_rat_papertrail::AutosyncRequest::Evaluate),
        );
        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        let _ = handle.join();
    });
}

#[test]
fn papertrail_deadline_fires_during_an_in_flight_pass_and_ticks_coalesce() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let mut config = whole_root_config(root, &[PathBuf::from("src")]);
    config.watch.debounce_ms = 10;
    config.watch.max_latency_ms = 50;
    config.watch.periodic_sweep_secs = 0;
    let target_dirs = config.target_directories();
    let mut ignore = IgnoreMatcher::compile(&config.root, &target_dirs);
    let mut linked_worktrees = LinkedWorktreeWatches::default();
    let mut notify_watcher =
        <RecordingWatcher as notify::Watcher>::new(|_| {}, notify::Config::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (pass_tx, pass_rx) = std::sync::mpsc::channel();
    let pass_tx_for_loop = pass_tx.clone();
    let (papertrail_tx, papertrail_rx) = std::sync::mpsc::channel();
    let mut scheduler = PassScheduler::new();
    let stop = AtomicBool::new(false);
    let mut fleet_trigger = |_: &Path| {};

    let counters = placement_counters();
    let event_loop = EventLoop {
        config: &config,
        target_dirs: &target_dirs,
        fleet_bin: None,
        notify_watcher: &mut notify_watcher,
        counters: &counters,
        ignore: &mut ignore,
        linked_worktrees: &mut linked_worktrees,
        worktree_registry: None,
        rx,
        pass_tx: &pass_tx_for_loop,
        scheduler: &mut scheduler,
        papertrail_tx: Some(&papertrail_tx),
        papertrail_interval: Some(Duration::from_millis(100)),
        stop: &stop,
        fleet_trigger: &mut fleet_trigger,
    };
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || event_loop.run());

        // Put an ordinary maintenance pass in flight (this test plays the worker and does NOT
        // complete it).
        tx.send(LoopMsg::Fs(Ok(mutation_event(root.join("src/lib.rs"))))).unwrap();
        assert!(pass_rx.recv_timeout(Duration::from_secs(5)).is_ok());

        // The papertrail deadline still fires — an in-flight pass must not postpone it.
        assert_eq!(
            papertrail_rx.recv_timeout(Duration::from_secs(5)),
            Ok(rag_rat_papertrail::AutosyncRequest::Evaluate),
            "the papertrail deadline must fire during an in-flight maintenance pass",
        );

        // Several more deadline ticks elapse while the papertrail flight is in the air (no
        // PapertrailDone): they must coalesce, not queue.
        std::thread::sleep(Duration::from_millis(350));
        assert!(
            papertrail_rx.try_recv().is_err(),
            "ticks during an in-flight papertrail run must coalesce into one follow-up",
        );

        // Completing the flight dispatches exactly the one coalesced follow-up.
        tx.send(LoopMsg::PapertrailDone).unwrap();
        assert_eq!(
            papertrail_rx.recv_timeout(Duration::from_secs(5)),
            Ok(rag_rat_papertrail::AutosyncRequest::Evaluate),
        );

        stop.store(true, Ordering::Relaxed);
        // Best-effort wake: a timeout tick may have already observed `stop` and exited the loop,
        // in which case `rx` is gone and this send fails — that is success, not an error.
        let _ = tx.send(LoopMsg::Wake);
        drop(tx);
        drop(pass_tx);
        let _ = handle.join();
    });
}
