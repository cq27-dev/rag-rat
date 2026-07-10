//! Index-lifecycle commands, split out of the `commands` god-module: `index` (full / discover /
//! changed / worktree-overlay build + watch), `reconcile` (embedding backfill, plan, vector
//! re-encode), `maintenance` (the git-hook pass, coalesced), `doctor` (health snapshot), and the
//! `run_watch` / `run_maintenance_pass` helpers they drive.
use std::fs;
use std::time::Instant;

use rag_rat_core::{Config, IndexDatabase, OutputFormat};

use crate::cli::{DoctorArgs, IndexArgs, MaintenanceArgs, ReconcileArgs};
use crate::commands::output_format;
use crate::render::{
    print_output, print_reconcile_plan, render_index_progress, render_reconcile_progress,
};
use crate::{DEFAULT_MAINTENANCE_SECONDS, open_index};

pub(crate) fn index(config: &Config, args: &IndexArgs) -> anyhow::Result<()> {
    // The empty-index refusal is enforced ONCE, in the core `rebuild_with_progress` (#427); the CLI
    // only threads the `--allow-empty` opt-in via `Config::allow_empty`, so every mode below
    // (full / discover / changed / watch) inherits the same policy with no per-mode guard.
    let config = &Config { allow_empty: args.allow_empty, ..config.clone() };
    if args.watch {
        // Surface the loud-adoption warnings (re-anchor / same-identity join) for watch mode too —
        // the watcher's catch-up pass indexes this scope, so a worktree/clone starting `--watch`
        // must still get the `--worktree` / `[index] repo_id` guidance (#427 review). `run_watch`
        // refuses a NO-`[target_bindings]` config up front (it could never index anything); a
        // config with targets that currently match zero files instead starts and defers via
        // the watcher's own rebuild, which refuses the first-time-empty registration and
        // `let _ =`-discards it (waiting for content). `--allow-empty` registers a
        // deliberately empty index either way.
        surface_adoption_warnings(config)?;
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
    surface_adoption_warnings(config)?;
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
    // read-only query (doctor reads persisted values; no re-validation). `pending` entries are
    // excluded — alive on an in-flight worktree branch (#492), they need no re-anchoring — but
    // everything else doctor lists (gone/stale bindings AND placeholder-scoped memories) stays
    // actionable and must keep tripping this warning.
    let doctor_count = db
        .memory_doctor()
        .map(|entries| entries.iter().filter(|e| e.anchor_status != "pending").count())
        .unwrap_or(0);
    if doctor_count > 0 {
        eprintln!("⚠ {doctor_count} repo memories need re-anchoring — run 'rag-rat memory doctor'");
    }
    print_output(&db.status(&config.database)?)
}

/// #427: emit the loud-adoption warnings for a one-shot or watch `index`. Non-fatal (stderr), and
/// deliberately NOT run for `--worktree` overlays (that is an explicit, correct overlay op). The
/// re-anchor message leads the more generic same-identity-join message (worktree is the more
/// specific diagnosis).
fn surface_adoption_warnings(config: &Config) -> anyhow::Result<()> {
    // The configured `[index] root` named a LINKED worktree; `Config::load` re-anchored it to the
    // main checkout (worktrees share one base index). Say so, and point at the two ways to get the
    // branch's own content: the overlay, or a distinct pinned identity.
    if let Some(worktree_root) = &config.source_root_reanchored_from {
        eprintln!(
            "warning: `[index] root` names a linked worktree ({}); indexing the MAIN checkout {} \
             instead — worktrees share one base index.\n  Index this branch's delta with `rag-rat \
             index --worktree {}`.\n  To index {} as its own repo, pin `[index] repo_id`.",
            worktree_root.display(),
            config.root.display(),
            worktree_root.display(),
            worktree_root.display(),
        );
    }
    // Warn — don't block — when this root shares an already-registered repo's identity and would
    // merge into that ONE scope (a fresh clone, or a worktree not yet anchored). The user may want
    // that (re-index) or may have meant an independent index; name the remedy either way.
    if let Some(join) = rag_rat_core::index::same_identity_join_note(config)? {
        eprintln!(
            "warning: this checkout ({}) shares the identity of already-indexed repo `{}` \
             (recorded at {}); indexing merges it into that one scope.\n  To index {} as its own \
             repo, add `[index] repo_id = \"...\"` to rag-rat.toml.",
            config.root.display(),
            join.repo_id,
            join.existing_root.display(),
            config.root.display(),
        );
    }
    Ok(())
}

pub(crate) fn reconcile(config: &Config, args: &ReconcileArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    // INVARIANT (#312): this `--plan` early-return MUST stay ABOVE the `--reencode-vectors`
    // mutation below. `--plan` is a READ-ONLY dry run; returning here first is what keeps
    // `reconcile --plan --reencode-vectors` from mutating the index during a dry run. Do not
    // reorder.
    if args.plan {
        // Same cap the actual reconcile below resolves (`args` override else config), so `--plan`
        // classifies against exactly what the run it previews will use.
        let plan = db.reconcile_plan_with_cap(
            args.max_embedding_chars.unwrap_or(config.llm.embedding.runtime.max_embedding_chars),
        )?;
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
    // #427 (review): a config with no watchable target DIRECTORIES can discover nothing, EVER. Its
    // watcher would place zero source watches and then sit forever — a silent, unrecoverable state
    // (it never re-reads config, so adding targets later doesn't help until restart). Keyed on
    // `target_directories()`, NOT `targets.is_empty()`: a target with an empty directory list
    // (`[target_bindings] rust = []`) leaves `targets` non-empty yet watches nothing, the same dead
    // end. Unlike a config WITH target dirs that currently match zero files — that legitimately
    // starts and DEFERS, because a file appearing under a watched dir wakes it — the
    // no-watchable-dir case is a misconfiguration, so refuse it up front with the actionable
    // message, the same policy as the one-shot `index`. `--allow-empty` opts in to an empty watch.
    if !config.allow_empty && config.target_directories().is_empty() {
        return Err(rag_rat_core::index::EmptyIndexRefused {
            root: config.root.display().to_string(),
        }
        .into());
    }
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

pub(crate) fn doctor(config: &Config, args: &DoctorArgs) -> anyhow::Result<()> {
    if args.vacuum {
        return vacuum(config);
    }
    let schema = IndexDatabase::migration_check(&config.database)?;
    let (index, discovery, storage, clone_fingerprints, file_health) =
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
                Some(serde_json::to_value(db.database_file_health()?)?),
            )
        } else {
            (None, None, None, None, None)
        };
    print_output(&serde_json::json!({
        "config_root": config.root,
        "database": config.database,
        "schema": schema,
        "storage": storage,
        "file_health": file_health,
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

/// `doctor --vacuum`: reclaim dead space by rewriting the database (#574). Explicit operator action
/// — VACUUM takes the global schema lock and rewrites the whole file, so it's never automatic.
fn vacuum(config: &Config) -> anyhow::Result<()> {
    let db = IndexDatabase::open_config(config)?;
    let report = db.reclaim_freelist()?;
    let reclaimed_bytes = report.main_bytes_before.saturating_sub(report.main_bytes_after);
    // VACUUM cleared the freelist, but a live reader can pin the WAL so the post-VACUUM checkpoint
    // can't truncate — the file then stays large on disk. Say so, actionably, rather than report a
    // silent "success".
    let note = (!report.wal_truncated).then_some(
        "dead space was cleared but the compacted image is staged in the WAL — a live reader \
         pinned it, so the file has not shrunk on disk; stop agents/watchers/MCP servers and \
         re-run `rag-rat doctor --vacuum`",
    );
    print_output(&serde_json::json!({
        "vacuum": report,
        "reclaimed_bytes": reclaimed_bytes,
        "note": note,
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

    // #427: the core refuses a first-time-empty registration (a post-commit/checkout hook on a repo
    // with no `[target_bindings]` or no matching files). The hook-driven maintenance pass treats
    // that as "nothing to index yet" and DEFERS — a later pass registers once content appears —
    // rather than surfacing an error into the git hook. A recorded root going empty still prunes
    // (it is not first-time, so the core does not refuse it).
    let mut db = match IndexDatabase::index_discover_with_progress(config, render_index_progress) {
        Ok(db) => db,
        Err(err) if err.downcast_ref::<rag_rat_core::index::EmptyIndexRefused>().is_some() => {
            tracing::info!(target: "rag_rat_core::maintenance", "deferred: no discoverable files (first-time empty index)");
            return Ok(serde_json::json!({
                "trigger": trigger,
                "status": "deferred",
                "reason": "no discoverable files (first-time empty index)",
            }));
        },
        Err(err) => return Err(err),
    };
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
    rag_rat_core::watch::refresh_worktree_overlays(
        &mut db,
        config,
        budget.as_ref(),
        &rag_rat_core::watch::OverlayScope::All,
    );
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
    // Clone-edge graph (#286/#473): try the cheap IN-PLACE delta first — it settles an ordinary
    // commit's changes on this very hook pass. The FULL rebuild runs only when the delta could
    // not settle freshness (absent generation, normalizer bump, cap crossing, huge delta, error)
    // or the generation's accumulated df drift owes a refresh — and it stays behind the #472
    // quiet window (shared with the watcher via per-repo meta), so a stream of commits can't
    // treadmill full rebuilds. Best-effort + resumable across passes.
    let clone_delta = db.apply_clone_graph_delta(rag_rat_core::index::CLONE_DELTA_MAX_FILES).ok();
    let clone_full_rebuild_owed = match &clone_delta {
        Some(delta) if delta.status == "Applied" || delta.status == "Noop" =>
            delta.full_rebuild_owed,
        _ => true,
    };
    let clone_graph_report = if clone_full_rebuild_owed
        && db
            .clone_graph_rebuild_due(rag_rat_core::watch::CLONE_GRAPH_QUIET_MS, true)
            .unwrap_or(false)
    {
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
    tracing::debug!(target: "rag_rat_core::maintenance", phase = "gc_clone_memory", gc = gc_report.is_some(), clone_delta = clone_delta.as_ref().map_or("error", |d| d.status.as_str()), clone_graph = clone_graph_report.is_some(), memory_validated = memory_validation.is_some(), "post-reconcile phases complete");
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

    /// #574: `doctor --vacuum` opens the store, reclaims dead space under the schema lock, and
    /// leaves the freelist empty. (Reclamation-under-load correctness is unit-tested in
    /// `db_file_health`; this pins the CLI wiring — dispatch → open_config → VACUUM → report.)
    #[test]
    fn doctor_vacuum_runs_and_leaves_no_freelist() {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-vacuum-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.md"), "# Title\nalpha token\n").unwrap();
        let config = Config {
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: "markdown".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
        };
        IndexDatabase::rebuild(&config).unwrap();

        super::doctor(&config, &crate::cli::DoctorArgs { vacuum: true }).unwrap();

        let db = IndexDatabase::open_config(&config).unwrap();
        assert_eq!(
            db.database_file_health().unwrap().freelist_pages,
            0,
            "a vacuum leaves no reclaimable freelist"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

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
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
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
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
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
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
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
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
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

    /// The hook-driven maintenance tail shares the #472 quiet-window gate with the watcher: a
    /// pass observing a fresh content revision ARMS the window and DEFERS the clone-graph
    /// rebuild; once the armed candidate has sat past the window, the next pass completes it.
    #[test]
    fn maintenance_defers_the_clone_graph_rebuild_inside_the_quiet_window() {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-clone-quiet-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/b.rs"),
            "pub fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }\n",
        )
        .unwrap();
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
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
        };
        IndexDatabase::rebuild(&config).unwrap();

        // A content change lands, then the hook pass runs inside the window: it must arm the
        // gate and defer the rebuild, not discard-and-rebuild the generation.
        std::fs::write(root.join("src/c.rs"), "pub fn freshly_edited(x: i32) -> i32 { x * 3 }\n")
            .unwrap();
        let args = super::MaintenanceArgs {
            trigger: Some("post-commit".to_string()),
            max_seconds: None,
            branch_checkout: None,
            old_head: None,
            new_head: None,
        };
        super::maintenance(&config, &args).unwrap();

        let conn = rusqlite::Connection::open(&config.database).unwrap();
        let generations: i64 = conn
            .query_row("SELECT COUNT(*) FROM clone_graph_generations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(generations, 0, "a hook pass inside the quiet window defers the clone rebuild");

        // Backdate the armed candidate past the window; the next hook pass completes the owed
        // rebuild.
        conn.execute(
            "UPDATE repo_meta SET value = '1' WHERE key = 'clone_graph_quiet_candidate_since_ms'",
            [],
        )
        .unwrap();
        drop(conn);
        super::maintenance(&config, &args).unwrap();

        let conn = rusqlite::Connection::open(&config.database).unwrap();
        let complete: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM clone_graph_generations WHERE status = 'Complete'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(complete, 1, "the quiet-elapsed hook pass builds the graph to completion");

        let _ = std::fs::remove_dir_all(&root);
    }
}
