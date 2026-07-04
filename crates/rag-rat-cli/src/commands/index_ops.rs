//! Index-lifecycle commands, split out of the `commands` god-module: `index` (full / discover /
//! changed / worktree-overlay build + watch), `reconcile` (embedding backfill, plan, vector
//! re-encode), `maintenance` (the git-hook pass, coalesced), `doctor` (health snapshot), and the
//! `run_watch` / `run_maintenance_pass` helpers they drive.
use std::fs;
use std::time::Instant;

use rag_rat_core::{Config, IndexDatabase, OutputFormat};

use crate::cli::{IndexArgs, MaintenanceArgs, ReconcileArgs};
use crate::commands::output_format;
use crate::render::{
    print_output, print_reconcile_plan, render_index_progress, render_reconcile_progress,
};
use crate::{DEFAULT_MAINTENANCE_SECONDS, open_index};

pub(crate) fn index(config: &Config, args: &IndexArgs) -> anyhow::Result<()> {
    if args.watch {
        return run_watch(config.clone());
    }
    // Serialize with the background watcher / other writers OF THIS REPO (busy_timeout backstops
    // any heal on the query path). The write lock is per-repo (A6), so a rebuild here never
    // blocks an unrelated repo's writer in a shared global DB.
    let lock_repo = rag_rat_core::locks::write_lock_repo_id(config);
    let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    // `--worktree`: index a linked worktree's branch overlay on top of the existing base index
    // (#219). A distinct mode — the delta vs the base, not a base (re)build — so handle it before
    // the full/discover/changed branches.
    if let Some(worktree) = &args.worktree {
        let mut db = open_index(config)?;
        let mut progress = render_index_progress;
        // Index the overlay with the LINKED worktree's OWN target set (its branch `rag-rat.toml`),
        // not the launching process's base targets — a branch that adds/narrows targets must be
        // indexed by its own config or its overlay rows are filtered/pruned (#219 review).
        let overlay_config = config.for_linked_worktree_overlay(worktree);
        let report = db.index_worktree_overlay(&overlay_config, worktree, &mut progress)?;
        if report.worktree_id.is_empty() {
            anyhow::bail!(
                "{} is not a linked worktree of {} — nothing indexed",
                worktree.display(),
                config.root.display()
            );
        }
        eprintln!(
            "worktree overlay [{}]: {} indexed, {} tombstoned, {} pruned",
            report.worktree_id, report.indexed, report.tombstoned, report.pruned
        );
        return Ok(());
    }
    let db = if args.full {
        IndexDatabase::rebuild_with_progress(config, render_index_progress)?
    } else if args.discover {
        IndexDatabase::index_discover_with_progress(config, render_index_progress)?
    } else {
        IndexDatabase::index_changed_with_progress(config, render_index_progress)?
    };
    // Re-anchor repo memories against the freshly indexed symbols/chunks so a moved or renamed
    // binding relocates (or is flagged) instead of silently pointing at a stale row. Memory rows
    // themselves are never deleted by indexing.
    if let Err(err) = db.memory_validate() {
        eprintln!("warning: repo-memory re-validation failed: {err}");
    }
    // After validate has refreshed anchor_status values, count non-current anchors with a
    // read-only query (doctor reads persisted values; no re-validation).
    let doctor_count = db.memory_doctor().map(|entries| entries.len()).unwrap_or(0);
    if doctor_count > 0 {
        eprintln!("⚠ {doctor_count} repo memories need re-anchoring — run 'rag-rat memory doctor'");
    }
    print_output(&db.status(&config.database)?)
}

pub(crate) fn reconcile(config: &Config, args: &ReconcileArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    // INVARIANT (#312): this `--plan` early-return MUST stay ABOVE the `--reencode-vectors`
    // mutation below. `--plan` is a READ-ONLY dry run; returning here first is what keeps
    // `reconcile --plan --reencode-vectors` from mutating the index during a dry run. Do not
    // reorder.
    if args.plan {
        let plan = db.reconcile_plan()?;
        // `--plan` prints a human summary by default; the global `--json` switches to the
        // structured plan.
        if output_format() == OutputFormat::Json {
            print_output(&plan)?;
        } else {
            print_reconcile_plan(&plan);
        }
        return Ok(());
    }
    // Force the legacy-f32 → int8 vector re-encode (#312) when asked, ignoring the run-once gate —
    // for users who want it now on a huge index. Format-only, idempotent. This SHORT-CIRCUITS: it
    // re-encodes and RETURNS, ignoring the other reconcile flags (no embedding inference runs), so
    // `--reencode-vectors` is a re-encode-only action. Bounded by `--max-seconds` (resumable via
    // the persisted cursor) when given. Runs only on a real (non-`--plan`) reconcile.
    if args.reencode_vectors {
        let deadline = args.max_seconds.map(|s| Instant::now() + std::time::Duration::from_secs(s));
        let converted = db.reencode_legacy_vectors_now(deadline)?;
        let report = serde_json::json!({ "reencoded_vectors": converted });
        if output_format() == OutputFormat::Json {
            print_output(&report)?;
        } else {
            eprintln!("rag-rat: re-encoded {converted} legacy f32 vector blobs to int8");
        }
        return Ok(());
    }
    let options = rag_rat_core::index::ai::ReconcileOptions {
        limit: args.limit,
        batch_size: args.batch_size.or(Some(config.llm.embedding.runtime.batch_size)),
        force: args.force,
        until_clean: args.until_clean,
        changed_first: args.changed_first,
        max_seconds: args.max_seconds,
        max_embedding_chars: args
            .max_embedding_chars
            .unwrap_or(config.llm.embedding.runtime.max_embedding_chars),
        intra_threads: config.llm.embedding.runtime.ort_threads.map(|n| n as usize),
        // The explicit `rag-rat reconcile` is the deliberate bulk pass that MAY provision an
        // ephemeral cookbook box (#318); the watcher's incremental pass does not.
        provision_remote: true,
    };
    let report = db.reconcile_with_options_progress(options, render_reconcile_progress)?;
    // After reconciling, surface non-current memory anchors so they don't rot silently.
    // Read-only count from persisted anchor_status; does not call memory_validate.
    let non_current = db.memory_anchor_health().map(|h| h.stale + h.gone).unwrap_or(0);
    if non_current > 0 {
        eprintln!("⚠ {non_current} repo memories need re-anchoring — run 'rag-rat memory doctor'");
    }
    print_output(&report)
}

pub(crate) fn run_watch(config: Config) -> anyhow::Result<()> {
    let Some(_watcher) = rag_rat_core::watch::Watcher::spawn(config.clone()) else {
        anyhow::bail!("watcher is disabled ([watch] enabled = false or RAG_RAT_NO_WATCH set)");
    };
    eprintln!("rag-rat: watching {} for changes (Ctrl-C to stop)", config.root.display());
    // The watcher runs on its own thread; park here. Ctrl-C ends the process and the OS releases
    // the locks; the next session's startup catch-up covers any edit in flight.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

pub(crate) fn doctor(config: &Config) -> anyhow::Result<()> {
    let schema = IndexDatabase::migration_check(&config.database)?;
    let (index, discovery, storage, clone_fingerprints) =
        if schema.state == rag_rat_core::index::schema::SchemaState::Compatible {
            let db = IndexDatabase::open_config(config)?;
            let mut index_status = serde_json::to_value(db.status(&config.database)?)?;
            // Schema (incl. the migrations list) is reported once at the top level from
            // `migration_check`; drop the duplicate copy nested in `index` so `doctor` doesn't list
            // migrations twice.
            if let Some(object) = index_status.as_object_mut() {
                object.remove("schema");
            }
            (
                Some(index_status),
                Some(serde_json::to_value(db.discovery_status(config)?)?),
                Some(serde_json::to_value(db.storage_status()?)?),
                Some(serde_json::to_value(db.clone_fingerprint_health()?)?),
            )
        } else {
            (None, None, None, None)
        };
    print_output(&serde_json::json!({
        "config_root": config.root,
        "database": config.database,
        "schema": schema,
        "storage": storage,
        "discovery": discovery,
        "clone_fingerprints": clone_fingerprints,
        "targets": config.targets.iter().map(|target| serde_json::json!({
            "name": target.name,
            "language": target.language.as_str(),
            "directories": target.directories,
            "kind": target.kind.as_str(),
        })).collect::<Vec<_>>(),
        "index": index,
        "mcp": {
            "transport": "stdio",
            "tools": rag_rat_mcp::tools::TOOL_NAMES,
            "source_read_only": true,
            "index_writes": "sqlite_auto_heal"
        }
    }))
}

pub(crate) fn maintenance(config: &Config, args: &MaintenanceArgs) -> anyhow::Result<()> {
    let trigger = args.trigger.clone().unwrap_or_else(|| "manual".to_string());
    let branch_checkout = args.branch_checkout.clone();
    let old_head = args.old_head.clone();
    let new_head = args.new_head.clone();

    if trigger == "post-checkout" && branch_checkout.as_deref() == Some("0") {
        print_output(&serde_json::json!({
            "trigger": trigger,
            "status": "skipped",
            "reason": "file checkout",
            "old_head": old_head,
            "new_head": new_head,
            "branch_checkout": branch_checkout,
        }))?;
        return Ok(());
    }

    // If an MCP watcher is already live for this worktree, it runs the identical pass
    // (`watch::run_pass`) on its own schedule whenever a tracked FILE changes — so for the
    // file-changing triggers (checkout/merge) the eager hook pass here is redundant and just
    // doubles the work + memory pressure. Defer to the watcher; the query-path heal covers the
    // brief staleness gap. post-commit / post-rewrite touch only git metadata, which the
    // file-watcher can't see, so those still run (and are cheap — no file content changed).
    if matches!(trigger.as_str(), "post-checkout" | "post-merge")
        && crate::claude_hook::watcher_state(config).0
    {
        print_output(&serde_json::json!({
            "trigger": trigger,
            "status": "skipped",
            "reason": "watcher live — deferring to the watcher's pass",
            "old_head": old_head,
            "new_head": new_head,
        }))?;
        return Ok(());
    }

    // Single-flight coalescing (#267): a single amend/merge/rebase fires several git hooks
    // (post-commit + post-rewrite, post-merge + post-commit, post-rewrite + post-checkouts), each
    // backgrounding `rag-rat maintenance`. Without coalescing they serialize on the write lock and
    // each runs a full discover pass — doubling work and widening the SQLITE_BUSY window for MCP
    // reads (#220). The first trigger to take the maintenance lock runs; concurrent triggers set a
    // "rerun pending" marker and exit immediately; the runner re-checks the marker after its pass
    // and runs once more to cover a change that arrived mid-pass. The pass still takes the write
    // lock internally, so serialization with the watcher is unchanged.
    let lock_repo = rag_rat_core::locks::write_lock_repo_id(config);
    let pending = rag_rat_core::locks::maintenance_pending_path(&config.database, &lock_repo);
    let lock_path = rag_rat_core::locks::maintenance_lock_path(&config.database, &lock_repo);
    let Some(_maint) = rag_rat_core::locks::FileLock::try_acquire(&lock_path)? else {
        let _ = fs::File::create(&pending);
        return print_output(&serde_json::json!({
            "trigger": trigger,
            "status": "skipped",
            "reason": "another maintenance pass is in flight (coalesced, #267)",
            "old_head": old_head,
            "new_head": new_head,
        }));
    };

    let mut report;
    loop {
        // This pass covers the current state, so clear any prior rerun request first; a trigger
        // that fires after this point re-sets it and earns the rerun below.
        let _ = fs::remove_file(&pending);
        report = run_maintenance_pass(config, args, &trigger)?;
        if !pending.exists() {
            break;
        }
    }
    print_output(&report)
}

/// One maintenance pass: discover-index under the write lock, refresh every live linked-worktree
/// overlay, run the budgeted embedding reconcile, GC dead git contexts, and re-validate repo-memory
/// anchors. Returns the report object — the caller prints it, after a coalesced rerun if one was
/// requested mid-pass (see [`maintenance`]).
fn run_maintenance_pass(
    config: &Config,
    args: &MaintenanceArgs,
    trigger: &str,
) -> anyhow::Result<serde_json::Value> {
    let max_seconds = args.max_seconds.unwrap_or(DEFAULT_MAINTENANCE_SECONDS);
    let started = Instant::now();

    // Debug-log span for the whole pass (off unless `[log]`/`RAG_RAT_LOG`). This is the entry point
    // for "git action → maintenance → embedding"; the per-phase events below make it legible.
    let _span = tracing::info_span!("maintenance_pass", %trigger, max_seconds).entered();
    tracing::info!(target: "rag_rat_core::maintenance", "maintenance pass started");

    // Serialize with the background watcher (and other writers) OF THIS REPO. The hook backgrounds
    // this command, so blocking here never holds up the git operation; busy_timeout backstops the
    // query-path heal. Per-repo write lock (A6).
    let lock_repo = rag_rat_core::locks::write_lock_repo_id(config);
    let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    tracing::debug!(target: "rag_rat_core::maintenance", phase = "lock_acquired", elapsed_ms = started.elapsed().as_millis() as u64, "write lock acquired");

    let mut db = IndexDatabase::index_discover_with_progress(config, render_index_progress)?;
    tracing::debug!(target: "rag_rat_core::maintenance", phase = "index_discover", elapsed_ms = started.elapsed().as_millis() as u64, "phase complete");
    // One-time on upgrade: re-encode any legacy f32 vector blobs to the compact int8 format (#312).
    // Meta-gated, so this runs once and then skips the table scan cheaply on every later pass; run
    // on the BASE index (not per-overlay) before the worktree refresh re-scopes the connection.
    // Format-only (decode f32 → encode int8), so it's cheap — no model inference.
    //
    // BUDGETED, and only gets a SHARE of the budget: skipped entirely when `max_seconds == 0` (the
    // "no embedding work" cap, mirroring `budget` below), and otherwise bounded by `started +
    // max_seconds/2` — only HALF the window. Giving it the full window would let a multi-pass
    // conversion consume the whole budget every pass, so `budget.next_options()` returns None and
    // new/changed chunks go un-embedded (BM25-only) for the whole window. With the half cap the
    // embedding reconcile always gets the rest; and `max_seconds == 1` → `max_seconds/2 == 0` → an
    // already-expired deadline → the re-encode does nothing this pass (the embedding reconcile
    // wins), which is correct. Resumes from the persisted cursor across passes until complete.
    let vector_reencode = if max_seconds > 0 {
        let deadline = started + std::time::Duration::from_secs(max_seconds / 2);
        match db.reencode_legacy_vectors_if_needed(Some(deadline)) {
            Ok(converted) => Some(converted),
            Err(e) => {
                // Don't swallow it: the gate is set only on success, so a persistent error
                // (SQLITE_BUSY, disk full) would otherwise retry-and-fail invisibly every pass.
                eprintln!("rag-rat: vector re-encode pass failed (will retry): {e}");
                None
            },
        }
    } else {
        None
    };
    // ONE time budget for the whole pass — the per-overlay embedding reconciles AND the base
    // reconcile below — measured from `started` so discovery already counts against it. Without a
    // shared budget each overlay (each call starts its own `max_seconds` timer) plus the base could
    // spend the full `--max-seconds`, holding the write lock (N+1)× past the advertised limit (#219
    // review). A `0` cap means the caller asked to skip embedding work entirely.
    let budget = (max_seconds > 0).then(|| {
        rag_rat_core::watch::ReconcileBudget::new(
            rag_rat_core::index::ai::ReconcileOptions {
                limit: None,
                batch_size: Some(config.llm.embedding.runtime.batch_size),
                force: false,
                until_clean: false,
                changed_first: true,
                max_seconds: Some(max_seconds),
                max_embedding_chars: config.llm.embedding.runtime.max_embedding_chars,
                intra_threads: config.llm.embedding.runtime.ort_threads.map(|n| n as usize),
                // Maintenance is a background pass (watcher-like) — it must NOT cold-start a GPU
                // box for incremental work. Only the explicit `rag-rat reconcile`
                // provisions (#318).
                provision_remote: false,
            },
            started,
        )
    });
    // Keep every live linked worktree's branch overlay fresh (#219). The git hooks run THIS command
    // (not the foreground watcher), so without this a commit/checkout/merge in a linked worktree
    // would index the base `config.root` but leave that worktree's overlay stale until a watcher
    // pass or a manual `index --worktree`. Delta-only + idle-safe, like the watcher's pass; a
    // CHANGED overlay's embeddings are reconciled INLINE (while scoped to it) so worktree queries
    // aren't BM25-only for branch content. It restores the base scope afterward so the base
    // reconcile/gc/memory-validate below run unscoped.
    rag_rat_core::watch::refresh_worktree_overlays(&mut db, config, budget.as_ref());
    // The base reconcile gets whatever budget the overlays left; `None` → exhausted (or no cap left
    // at all), so skip it rather than start a fresh full-budget reconcile.
    let reconcile_report = match budget
        .as_ref()
        .and_then(rag_rat_core::watch::ReconcileBudget::next_options)
    {
        Some(options) => {
            let report = db.reconcile_with_options_progress(options, render_reconcile_progress)?;
            tracing::info!(target: "rag_rat_core::maintenance", phase = "reconcile", ran = true, status = %report.status, "phase complete");
            Some(report)
        },
        None => {
            // No budget left (overlays/reencode consumed it) or a 0-cap "skip embedding" pass —
            // this is a common reason the base backlog stays non-empty across hook passes.
            tracing::info!(target: "rag_rat_core::maintenance", phase = "reconcile", ran = false, skip_reason = "no_budget_remaining", "phase skipped");
            None
        },
    };
    // Prune index rows for git contexts that are no longer live (worktree-safe; keeps every
    // live worktree's HEAD). Cheap and bounded, so it runs every maintenance pass.
    let gc_report = db.garbage_collect().ok();
    // Clone-edge graph (#286): refresh the persisted graph when absent/stale with whatever budget
    // the embedding reconcile left (shared so the pass can't overrun), so the git-hook
    // maintenance keeps the graph warm too — not just the foreground watcher. Best-effort +
    // resumable across passes.
    let clone_graph_report = if db.pending_clone_graph().unwrap_or(false) {
        match budget.as_ref().and_then(rag_rat_core::watch::ReconcileBudget::next_options) {
            Some(options) => db.reconcile_clone_edges_with_budget(options.max_seconds).ok(),
            None => None,
        }
    } else {
        None
    };
    // Re-anchor repo memories: post-checkout/merge/rewrite/commit are exactly when files move,
    // rename, or change, so relocate symbol/chunk bindings (or flag them) here rather than
    // leaving stale anchors until a manual memory_validate.
    let memory_validation = db.memory_validate().ok();
    tracing::debug!(target: "rag_rat_core::maintenance", phase = "gc_clone_memory", gc = gc_report.is_some(), clone_graph = clone_graph_report.is_some(), memory_validated = memory_validation.is_some(), "post-reconcile phases complete");
    // Remaining backlog for the ACTIVE embedding model from the CHEAP persisted counts
    // (`status.embedding` / `status.artifacts`, #285), NOT `reconcile_plan` — which rebuilds +
    // re-hashes EVERY chunk's embedding input (O(repo)) on every hook pass. #378 measured that plan
    // dominating the maintenance pass (~10s on this index). Use the ACTIVE-model fields, NOT
    // `status.fastembed`: the latter reports the FastEmbed identity when the active model is
    // non-FastEmbed (Model2Vec / hash / embeddings-off), which would show a different model +
    // phantom counts (PR #380 review). Only fields the cheap counts compute EXACTLY are exported;
    // `missing`, the exact per-policy `skipped`, the failed retryable/waiting split, and the
    // by-priority/by-policy breakdown all need the O(repo) per-chunk scan — run `reconcile --plan`
    // (see the `remaining_backlog` comment for why `missing` in particular can't be trusted
    // cheaply). NOTE: these counts are still UNSCOPED (all worktrees — see #360).
    let status = db.llm_status()?;
    let embedding = &status.embedding;
    let artifacts = &status.artifacts;
    tracing::info!(
        target: "rag_rat_core::maintenance",
        elapsed_ms = started.elapsed().as_millis() as u64,
        model = %embedding.model_id,
        current = artifacts.current,
        stale = artifacts.stale,
        total_chunks = artifacts.total_chunks,
        "maintenance pass complete (remaining backlog is unscoped/cross-worktree, #360)"
    );
    Ok(serde_json::json!({
        "trigger": trigger,
        "status": "complete",
        "old_head": args.old_head,
        "new_head": args.new_head,
        "branch_checkout": args.branch_checkout,
        "max_seconds": max_seconds,
        "elapsed_seconds": started.elapsed().as_secs_f64(),
        "reconcile": reconcile_report,
        // #312: rows the legacy-f32 → int8 re-encode converted this pass, or null when it was
        // skipped (max_seconds == 0, or already done/the gate was set so the call returned 0 — note
        // a gate-skip also reports {"converted": 0}) or errored. Lets a --json consumer see progress.
        "vector_reencode": vector_reencode.map(|n| serde_json::json!({ "converted": n })),
        "clone_graph": clone_graph_report,
        "gc": gc_report,
        "memory_validation": memory_validation,
        "remaining_backlog": {
            "model": embedding.model_id,
            "current": artifacts.current,
            "stale": artifacts.stale,
            "failed": artifacts.failed,
            "blocked": artifacts.blocked,
            "total_chunks": artifacts.total_chunks,
            // `missing` is intentionally OMITTED: `artifacts.missing` is `total - current - stale -
            // failed - blocked` with policy-skipped chunks (generated / tiny) treated as zero, so it
            // would report a PERMANENT backlog even after a clean reconcile (PR #380 review) — and the
            // exact eligible-missing can't be computed without the O(repo) per-chunk scan. Coverage
            // reads off `current`/`total_chunks`; `stale`/`failed`/`blocked` are exact remaining-work
            // signals. The precise missing + per-policy `skipped` + by-priority breakdown live in
            // `reconcile --plan`, along with the failed retryable/waiting split.
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rag_rat_core::config::{ResolvedTarget, TargetKind};
    use rag_rat_core::language::Language;
    use rag_rat_core::{Config, IndexDatabase};

    static N: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn maintenance_command_refreshes_a_linked_worktree_overlay() {
        // #219 review: the git hooks invoke `rag-rat maintenance` (NOT the foreground watcher), so
        // this command — not just `watch::maintenance_pass` — must refresh every live linked
        // worktree's branch overlay. Without it, a commit/checkout/merge in a linked worktree
        // indexes the base `config.root` but leaves the worktree overlay stale.
        let git = |dir: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-maint-overlay-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let main = root.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "base"]);
        let config = Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: main.clone(),
            database: main.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
        };
        IndexDatabase::rebuild(&config).unwrap();

        let linked = root.join("wt");
        git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        std::fs::write(linked.join("src/a.rs"), "pub fn linked_fn() {}\n").unwrap();
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-qm", "branch"]);

        // Run the actual CLI maintenance command (the hook entry point).
        let args = super::MaintenanceArgs {
            trigger: Some("post-merge".to_string()),
            max_seconds: Some(0), // skip the embedding reconcile; we only assert the overlay
            branch_checkout: None,
            old_head: None,
            new_head: None,
        };
        super::maintenance(&config, &args).unwrap();

        // The worktree-scoped query now sees the branch version, populated by the maintenance pass.
        let mut db = IndexDatabase::open_config(&config).unwrap();
        db.use_worktree_scope(&config.root, Some(&linked)).unwrap();
        let names: Vec<String> =
            db.symbols("linked_fn", None, 10).unwrap().into_iter().map(|h| h.name).collect();
        assert!(
            names.contains(&"linked_fn".to_string()),
            "the maintenance command must populate the worktree overlay: {names:?}",
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn embedding_counts_scope_to_the_active_checkout() {
        // #360 regression guard: reconcile --plan / llm_status embedding coverage must count the
        // ACTIVE checkout, not the aggregate across sibling worktrees. Those surfaces open via
        // `open_config`, which base-scopes the connection (set_context, #219); a bare `open` stays
        // UNSCOPED. This pins that difference so #360 can't silently regress: with a sibling
        // overlay present, `open_config` must report FEWER missing chunks than `open`. (No
        // embedder needed — with zero embeddings every chunk is `missing`, so `missing`
        // tracks the scoped chunk count.)
        let git = |dir: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-scope360-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let main = root.join("main");
        std::fs::create_dir_all(main.join("src")).unwrap();
        std::fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-qm", "base"]);
        let config = Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: main.clone(),
            database: main.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
        };
        IndexDatabase::rebuild(&config).unwrap();

        // Linked worktree on a branch that ADDS a file → chunks that belong only to the sibling.
        let linked = root.join("wt");
        git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
        std::fs::write(linked.join("src/b.rs"), "pub fn sibling_fn() {}\n").unwrap();
        git(&linked, &["add", "-A"]);
        git(&linked, &["commit", "-qm", "sibling"]);
        let args = super::MaintenanceArgs {
            trigger: Some("post-merge".to_string()),
            max_seconds: Some(0),
            branch_checkout: None,
            old_head: None,
            new_head: None,
        };
        super::maintenance(&config, &args).unwrap(); // populates the linked overlay

        // `open` leaves the connection UNSCOPED (all worktrees); `open_config` pre-scopes to the
        // base checkout. With the sibling overlay present, base scope must count FEWER
        // missing chunks.
        let unscoped =
            IndexDatabase::open(&config.database).unwrap().llm_status().unwrap().artifacts.missing;
        let scoped =
            IndexDatabase::open_config(&config).unwrap().llm_status().unwrap().artifacts.missing;
        assert!(
            scoped < unscoped,
            "open_config must base-scope embedding counts (open does not): scoped={scoped} \
             unscoped={unscoped}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maintenance_backlog_uses_cheap_counts_not_reconcile_plan() {
        // #378: `run_maintenance_pass` must build `remaining_backlog` from the CHEAP `llm_status`
        // counts, never the O(repo) `reconcile_plan` (which rebuilds + re-hashes every chunk's
        // embedding input on every hook pass). Assert the exact cheap coverage fields are present,
        // and the approximate ones (skipped, the retryable/waiting split) + expensive per-chunk
        // breakdowns are ABSENT — those need `reconcile --plan`.
        let git = |dir: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
        };
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-maint-backlog-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "t"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        let config = Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
        };
        IndexDatabase::rebuild(&config).unwrap();

        let args = super::MaintenanceArgs {
            trigger: Some("post-commit".to_string()),
            max_seconds: Some(0), // skip the reconcile; we only assert the backlog shape
            branch_checkout: None,
            old_head: None,
            new_head: None,
        };
        let report = super::run_maintenance_pass(&config, &args, "post-commit").unwrap();
        let backlog = &report["remaining_backlog"];

        // Fields the cheap ACTIVE-model counts compute exactly.
        for key in ["current", "stale", "failed", "blocked", "total_chunks"] {
            assert!(backlog[key].is_number(), "cheap backlog count `{key}` present: {backlog}");
        }
        // Fields the cheap counts CAN'T compute exactly are omitted (reconcile --plan's job):
        // `missing` (would count policy-skipped chunks as a permanent backlog), the exact policy
        // `skipped`, the failed retryable/waiting split, and the per-chunk breakdowns (PR #380
        // review).
        for key in [
            "missing",
            "skipped",
            "failed_retryable",
            "failed_waiting",
            "missing_by_priority",
            "skipped_by_policy",
        ] {
            assert!(backlog.get(key).is_none(), "`{key}` must be omitted from the cheap backlog");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maintenance_coalesces_a_concurrent_trigger() {
        use rag_rat_core::locks::{
            FileLock, maintenance_lock_path, maintenance_pending_path, write_lock_repo_id,
        };

        // #267: a single amend/merge/rebase fires several git hooks, each backgrounding
        // `rag-rat maintenance`. A concurrent trigger must coalesce — skip its pass and set the
        // rerun marker — rather than queue a redundant discover that widens the SQLITE_BUSY window.
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-maint-coalesce-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let config = Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            log: Default::default(),
        };
        IndexDatabase::rebuild(&config).unwrap();

        let lock_repo = write_lock_repo_id(&config);
        let pending = maintenance_pending_path(&config.database, &lock_repo);
        let args = super::MaintenanceArgs {
            trigger: Some("post-rewrite".to_string()),
            max_seconds: Some(0), // skip the embedding reconcile; we only assert coalescing
            branch_checkout: None,
            old_head: None,
            new_head: None,
        };

        // Hold the coordination lock to simulate an in-flight maintenance pass.
        let held = FileLock::try_acquire(&maintenance_lock_path(&config.database, &lock_repo))
            .unwrap()
            .unwrap();
        assert!(!pending.exists());
        // A concurrent trigger coalesces: it does NOT run a pass; it sets the rerun marker.
        super::maintenance(&config, &args).unwrap();
        assert!(pending.exists(), "a coalesced trigger sets the rerun-pending marker");
        drop(held);

        // With the lock free, maintenance runs a pass and clears the marker (the rerun covers the
        // change the coalesced trigger requested).
        super::maintenance(&config, &args).unwrap();
        assert!(!pending.exists(), "the runner clears the rerun marker after its pass");

        let _ = std::fs::remove_dir_all(&root);
    }
}
