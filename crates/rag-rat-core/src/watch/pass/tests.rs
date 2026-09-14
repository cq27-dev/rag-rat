use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rag_rat_base::embedding_models::{FASTEMBED_MODEL_ID, HASH_MODEL_ID};

use crate::IndexDatabase;
use crate::index::ai::ReconcileOptions;
use crate::watch::tests::support::*;
use crate::watch::*;

/// #427: a maintenance/watch pass on a first-time-empty config DEFERS — the core refuses the
/// empty registration and `run_pass` swallows `EmptyIndexRefused`, returning `Ok(())` and
/// registering nothing, rather than erroring into the watcher loop. Covers the in-process defer
/// path the subprocess CLI guards exercise out-of-process (so it doesn't count toward
/// coverage).
#[test]
fn maintenance_pass_defers_on_a_first_time_empty_config() {
    let scratch = scratch_root("watch-empty-first-time");
    std::fs::create_dir_all(scratch.as_path()).unwrap();
    // A single rust target with NO directories → discovers nothing → first-time-empty.
    let (config, _) = whole_root_config(&scratch, &[]);
    let result = maintenance_pass(&config, false);
    assert!(result.is_ok(), "an empty first-time config must defer, not error: {result:?}");
    assert!(!config.database.exists(), "deferring must register no empty index");
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
    let root = scratch_root(format!("ragrat-watch-maintenance-skip-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn maintenance_target() {}\n").unwrap();
    let root = root.canonicalize().unwrap();
    let (config, root) = whole_root_config(&root, &[PathBuf::from("src")]);

    assert!(
        maintenance_pass_or_skip(&config, false).unwrap(),
        "an available writer lock should run the maintenance pass"
    );
    let db = IndexDatabase::open_config(&config).unwrap();
    assert!(db.status(&config.database).unwrap().file_count_by_language.values().sum::<u64>() > 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn startup_catchup_retries_existing_base_embedding_backlog() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-startup-backlog-{}-{id}", std::process::id()));
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

    let (mut config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
    config.llm.embedding.backend = HASH_MODEL_ID.parse().unwrap();

    let db = IndexDatabase::rebuild(&config).unwrap();
    db.install_model(HASH_MODEL_ID, None).unwrap();
    assert!(
        db.pending_embedding_jobs().unwrap() > 0,
        "fixture starts with indexed chunks but no embeddings"
    );
    drop(db);

    startup_catchup_pass(&config, None, None).unwrap();
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

    let _ = std::fs::remove_dir_all(&root);
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
    let main = scratch_root(format!("ragrat-watch-overlay-only-{}-{id}", std::process::id()));
    let linked =
        scratch_root(format!("ragrat-watch-overlay-only-linked-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&main);
    let _ = std::fs::remove_dir_all(&linked);
    std::fs::create_dir_all(main.join("src")).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        rag_rat_base::test_git::run(dir, args);
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
    let (config, main_root) = whole_root_config(&main_root, &[PathBuf::from("src")]);
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
    let root = scratch_root(format!("ragrat-watch-ephemeral-backlog-{}-{id}", std::process::id()));
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

    let (mut config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
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
    startup_catchup_pass(&config, None, None).unwrap();
    assert_eq!(
        crate::index::ai::estimated_reconcile_job_calls(),
        0,
        "startup must not scan chunks before the ephemeral light endpoint is known usable"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn startup_catchup_reconciles_shutdown_discovered_content() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-shutdown-reconcile-{}-{id}", std::process::id()));
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

    let (mut config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
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

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn startup_catchup_keeps_shutdown_marker_when_reconcile_is_blocked() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root(format!("ragrat-watch-shutdown-blocked-{}-{id}", std::process::id()));
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

    let (config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
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

    let _ = std::fs::remove_dir_all(&root);
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
    let root = scratch_root(format!("ragrat-watch-wal-checkpoint-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "pub fn wal_probe() -> i32 { 1 }\n").unwrap();
    let (config, root) = whole_root_config(&root, &[PathBuf::from("src")]);
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
        move |request: &PassRequest| {
            ran.lock().unwrap().push(request.clone());
            None
        }
    })
    .expect("worker thread spawns");
    pass_tx.send(PassRequest::StartupCatchup).unwrap();
    pass_tx
        .send(PassRequest::Maintenance { run_gc: true, overlay_scope: OverlayScope::All })
        .unwrap();
    for _ in 0..2 {
        match done_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(LoopMsg::PassDone { live_oracle_wake_in: None }) => {},
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

#[test]
fn papertrail_clock_is_never_due_without_an_interval_and_rearms_on_tick() {
    let start = Instant::now();
    let disabled = IntervalClock::new(None, start);
    assert!(!disabled.due(start + Duration::from_secs(86_400)));
    assert_eq!(disabled.due_in(start), None);

    let mut clock = IntervalClock::new(Some(Duration::from_secs(900)), start);
    assert!(!clock.due(start + Duration::from_secs(899)));
    assert!(clock.due(start + Duration::from_secs(900)));
    clock.on_tick(start + Duration::from_secs(900));
    assert!(!clock.due(start + Duration::from_secs(1_799)));
    assert!(clock.due(start + Duration::from_secs(1_800)));

    // A cadence that overflows Instant arithmetic is a deadline that never arrives — it must
    // not panic the watcher's wait computation.
    let oversized = IntervalClock::new(Some(Duration::from_secs(u64::MAX)), start);
    assert!(!oversized.due(start + Duration::from_secs(86_400)));
    assert_eq!(oversized.due_in(start), None);
}
