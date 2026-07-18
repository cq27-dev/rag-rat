//! `rm`: remove a repo from the consolidated global index and clean its on-disk footprint. The
//! database purge (which rows, under which lock, in one transaction, then VACUUM) lives in
//! `rag_rat_core::index::remove`; this shim resolves the path argument to a registered repo,
//! renders the confirmation summary, prompts (unless `--yes`), and passes the on-disk deconfigure
//! closure (delete `rag-rat.toml` + uninstall git hooks over the repo's governing roots) that
//! `purge_and_vacuum` runs UNDER the write lock — best-effort: a missing file/hook is a no-op, and
//! a cleanup failure WARNS rather than failing an already-committed purge.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use dialoguer::Confirm;
use rag_rat_base::config::Config;
use rag_rat_core::index::remove::{self, RemovePlan};
use rag_rat_db::schema;
use rag_rat_db::storage::IndexConnection;

use crate::cli::RmArgs;
use crate::render::print_output;
use crate::{MANAGED_HOOKS, ensure_index_exists, git_paths, is_rag_rat_hook};

pub(crate) fn rm(config: &Config, args: &RmArgs) -> anyhow::Result<()> {
    // Nothing to remove without an index — the friendly hint before any open.
    ensure_index_exists(config)?;

    // Bring the store to the CURRENT schema before querying purge tables. `count_repo_rows` reads a
    // hand-listed set of transitive tables (e.g. `clone_df_epoch`, V051), so a database written by
    // an OLDER rag-rat that predates one of them would otherwise fail with `no such table`.
    // Schema-only + additive; the purge then runs on the current schema. (The write commands `init`
    // / `consolidate` migrate first for the same reason.)
    let migration = rag_rat_core::index::IndexDatabase::migrate_schema_only(&config.database)?;
    if migration.state != schema::SchemaState::Compatible {
        anyhow::bail!("{}", migration.message);
    }

    // Resolve the path to a registered repo + count what removal would delete, on a read
    // connection.
    let plan = {
        let storage = IndexConnection::open(&config.database)?;
        let conn = storage.connection();
        let Some(repo) = remove::resolve_removable_repo(conn, &args.path)? else {
            anyhow::bail!("{}", unregistered_path_message(&storage, &args.path)?);
        };
        remove::plan_remove(conn, repo)?
    };

    // Human-readable per-category summary to stderr; the structured plan/outcome goes to stdout.
    eprint!("{}", render_plan_summary(&plan));

    if args.dry_run {
        eprintln!("rm: --dry-run — nothing was deleted.");
        return print_output(&serde_json::json!({
            "status": "dry_run",
            "repo_id": plan.repo.repo_id,
            "display_name": plan.repo.display_name,
            "roots": plan.repo.roots,
            "resolved_root": plan.repo.resolved_root,
            "total_rows": plan.counts.total_rows,
            "by_table": plan.counts.by_table,
        }));
    }

    if !args.yes && !confirm_removal(&plan)? {
        eprintln!("rm: aborted — nothing was deleted.");
        return print_output(&serde_json::json!({
            "status": "aborted",
            "repo_id": plan.repo.repo_id,
        }));
    }

    // The destructive step, all under the repo's write lock: purge in one transaction, deconfigure
    // the repo on disk (delete rag-rat.toml + uninstall hooks over its recorded governing roots),
    // then VACUUM. The deconfiguration runs INSIDE the lock, before it is released, so a writer
    // waiting on the lock cannot re-register the repo before it is made un-re-indexable.
    // Cleanup candidates: the recorded governing roots PLUS the checkout the removal resolved
    // through. `resolved_root` matters when the repo was registered ONLY via the read-only path
    // (no `repo_roots` entry, so `roots` is empty) — without it `rm` would report success while
    // deleting nothing on disk. It is deduped against `roots` and still passes the ownership guard,
    // so adding it never widens what gets deleted to a foreign repo.
    let mut cleanup_dirs = plan.repo.roots.clone();
    cleanup_dirs.push(plan.repo.resolved_root.to_string_lossy().into_owned());
    let (outcome, cleanup) = remove::purge_and_vacuum(
        &config.database,
        &plan.repo.repo_id,
        rag_rat_base::time::now_ms(),
        || clean_on_disk_footprint(&cleanup_dirs, &plan.repo.repo_id),
    )?;

    print_output(&serde_json::json!({
        "status": "removed",
        "repo_id": outcome.repo_id,
        "display_name": outcome.display_name,
        "purged_rows": outcome.purged_rows,
        "vacuum": outcome.vacuum,
        "vacuum_skipped": outcome.vacuum_skipped,
        "configs_removed": cleanup.configs_removed,
        "hooks_removed": cleanup.hooks_removed,
        "cleanup_warnings": cleanup.warnings,
        "next": "run `rag-rat init` in the repo to index it again",
    }))
}

/// Group the per-table counts into a small set of human categories and render a summary block. The
/// grouping is display-only (by table-name family) — the authoritative figure is `total_rows`, so a
/// table the grouping does not recognize still lands in `other` and is still counted.
fn render_plan_summary(plan: &RemovePlan) -> String {
    let mut categories: BTreeMap<&'static str, i64> = BTreeMap::new();
    for (table, count) in &plan.counts.by_table {
        *categories.entry(category_of(table)).or_insert(0) += count;
    }
    let mut out = String::new();
    let name = plan.repo.display_name.as_deref().unwrap_or("(unnamed)");
    out.push_str(&format!("rm: about to remove `{name}` [{}]\n", plan.repo.repo_id));
    for root in &plan.repo.roots {
        out.push_str(&format!("      root: {root}\n"));
    }
    if plan.counts.total_rows == 0 {
        out.push_str("      (no rows in the index — registry entry only)\n");
    } else {
        for (category, count) in &categories {
            out.push_str(&format!("      {category:<20} {count:>8}\n"));
        }
    }
    out.push_str(&format!("      {:<20} {:>8}\n", "TOTAL rows", plan.counts.total_rows));
    out
}

/// The display category for a table name, by family prefix. Order matters: `repo_memory*` /
/// `repo_node_edges` are memory tables and must be matched before the `repos` / `repo_roots` /
/// `repo_meta` registry check.
fn category_of(table: &str) -> &'static str {
    if table.starts_with("papertrail") {
        "papertrail"
    } else if table.starts_with("clone") {
        "clones"
    } else if table.starts_with("git") {
        "git history"
    } else if table.starts_with("repo_memory")
        || table.starts_with("memory_")
        || table == "repo_node_edges"
    {
        "memories"
    } else if table == "repos" || table == "repo_roots" || table == "repo_meta" {
        "registry"
    } else if table.starts_with("chunk") {
        "text & embeddings"
    } else if table.starts_with("oracle") || table == "edge_oracle" || table == "external_symbols" {
        "oracle"
    } else if table.starts_with("dream") || table.starts_with("reconcile") {
        "dream & reconcile"
    } else {
        // files, symbols, chunks, edges_data, logical_symbols/_members/_monikers, docs, packages,
        // parser_failures — the tree-sitter code graph.
        "code graph"
    }
}

/// Prompt for confirmation, defaulting to NO (a destructive op must not proceed on a bare Enter).
/// Refuses non-interactively with an actionable hint to pass `--yes` rather than erroring obscurely
/// inside dialoguer.
fn confirm_removal(plan: &RemovePlan) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing to remove `{}` without confirmation on a non-interactive stdin — re-run \
             with --yes (or --dry-run to preview)",
            plan.repo.repo_id
        );
    }
    Ok(Confirm::new()
        .with_prompt(format!(
            "Permanently remove this repo ({} rows) from the global index?",
            plan.counts.total_rows
        ))
        .default(false)
        .interact()?)
}

/// The outcome of the best-effort on-disk cleanup: the config/hook PATHS actually deleted, and any
/// non-fatal failures as warnings.
#[derive(Default)]
struct CleanupResult {
    configs_removed: Vec<String>,
    hooks_removed: Vec<String>,
    warnings: Vec<String>,
}

/// Delete the repo's GOVERNING `rag-rat.toml`(s) and uninstall its rag-rat-managed git hooks across
/// every directory whose config governs — or could govern — the repo: each recorded root (from
/// `repo_roots`, where the repo was indexed from) PLUS every git worktree linked to it, with each
/// dir's config resolved via `discover_config_path` (the same seam `Config::load` uses). Resolving
/// the config rather than assuming `<dir>/rag-rat.toml` matters three ways: (1) `rm` from a
/// subdirectory / linked worktree resolves the repo by git identity, but that path is not where the
/// governing config sits; (2) an `[index] root = "src"` layout keeps the toml at the worktree top
/// while `repo_roots` records `<top>/src`; (3) a linked worktree can carry its OWN branch-local
/// `rag-rat.toml`, which `Config::load`'s config-less-main fallback would promote to governing once
/// the main config is gone — re-adopting the repo on the next keyless `rag-rat index`. Every step
/// is best-effort: a missing file/hook is a silent no-op, and any error is a warning rather than a
/// failure — the database purge has already committed (and tombstoned the repo, the durable stop
/// against re-registration), so the repo is removed regardless of whether its on-disk config could
/// be cleaned.
fn clean_on_disk_footprint(roots: &[String], removed_repo_id: &str) -> CleanupResult {
    let mut result = CleanupResult::default();

    // Candidate dirs: each recorded root plus every git worktree linked to it, deduplicated…
    let mut seen_dirs = std::collections::BTreeSet::new();
    let dirs: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| worktree_dirs_for(Path::new(root)))
        .filter(|dir| seen_dirs.insert(dir.clone()))
        // …then the OWNERSHIP GUARD: only touch a dir that STILL belongs to the repo being removed —
        // a PRESENT git worktree whose content-derived identity equals `removed_repo_id`. This is
        // what makes the config discovery safe:
        //  * a GONE tree (the recorded root was deleted — rm's own use case) is skipped, so
        //    `discover_config_path`'s upward walk can never climb out of the missing git boundary
        //    into an UNRELATED parent repo and delete ITS governing config;
        //  * a recorded root now REUSED by a different repo derives a different id and is skipped, so
        //    rm never deletes the new occupant's config/hooks.
        // Skipping only UNDER-deletes (a stray config/hook left behind), which is harmless: the
        // removal tombstone refuses re-registration regardless of a surviving config.
        .filter(|dir| dir_belongs_to_removed_repo(dir, removed_repo_id))
        .collect();

    // The GOVERNING config file for each dir — resolved the way `Config::load` does
    // (`discover_config_path`): an `[index] root = "src"` layout keeps the toml at the worktree top
    // while `repo_roots` records `<top>/src`, and a linked worktree may carry a branch-local config
    // or defer to main's. A blind `<dir>/rag-rat.toml` would miss the first and mis-handle the
    // second; discovery lands on the file that actually governs. Deduped: several dirs can resolve
    // to one config.
    let mut seen_configs = std::collections::BTreeSet::new();
    for dir in &dirs {
        let config_path = rag_rat_base::config::discover_config_path(dir);
        if !seen_configs.insert(config_path.clone()) {
            continue;
        }
        match std::fs::remove_file(&config_path) {
            Ok(()) => result.configs_removed.push(config_path.display().to_string()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
            Err(err) =>
                result.warnings.push(format!("could not delete {}: {err}", config_path.display())),
        }
    }

    // Hooks live in the SHARED git dir (one `.git/hooks` across all worktrees), so dedup hook paths
    // across dirs and remove each managed hook once.
    let mut seen_hooks = std::collections::BTreeSet::new();
    for dir in &dirs {
        // Git hooks — uninstall exactly the rag-rat-managed ones (never a user's own hook),
        // mirroring `hooks uninstall`. `git_paths` fails when `dir` is not a git worktree
        // (deleted / non-git), in which case there is nothing to uninstall (a
        // non-git-worktree `Err` is a silent no-op).
        if let Ok(git) = git_paths(dir) {
            for hook in MANAGED_HOOKS {
                let path = git.hooks_dir.join(hook);
                if !seen_hooks.insert(path.clone()) {
                    continue;
                }
                match is_rag_rat_hook(&path) {
                    Ok(true) => match std::fs::remove_file(&path) {
                        Ok(()) => result.hooks_removed.push(path.display().to_string()),
                        Err(err) => result
                            .warnings
                            .push(format!("could not remove hook {}: {err}", path.display())),
                    },
                    // Absent or not rag-rat-managed: leave it.
                    Ok(false) => {},
                    Err(err) => result
                        .warnings
                        .push(format!("could not inspect hook {}: {err}", path.display())),
                }
            }
        }
    }

    for warning in &result.warnings {
        eprintln!("rm: warning: {warning}");
    }
    result
}

/// Whether `dir` is a PRESENT git worktree whose content-derived identity is the repo being
/// removed — the ownership gate for [`clean_on_disk_footprint`]. A gone / reused / foreign /
/// non-git dir returns false and is left untouched (an under-deletion the removal tombstone
/// backstops). `None` override: identity comes purely from the git content at `dir`, never a pinned
/// id.
fn dir_belongs_to_removed_repo(dir: &Path, removed_repo_id: &str) -> bool {
    dir.is_dir()
        && rag_rat_base::repo_identity::resolve_repo_identity(dir, None)
            .map(|identity| identity.repo_id == removed_repo_id)
            .unwrap_or(false)
}

/// `root` itself plus every git worktree linked to it (`git worktree list --porcelain`).
/// Best-effort: a non-git / missing / erroring `root` yields just `[root]`. Deconfiguration walks
/// these so a linked worktree's branch-local `rag-rat.toml` is removed too, not only the main
/// worktree's.
fn worktree_dirs_for(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "list", "--porcelain"])
        .output();
    if let Ok(out) = output
        && out.status.success()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                dirs.push(PathBuf::from(path));
            }
        }
    }
    dirs
}

/// The actionable error when a path resolves to no registered repo: name the path and list the
/// repos that ARE registered, so the operator can pick a valid one.
fn unregistered_path_message(storage: &IndexConnection, path: &Path) -> anyhow::Result<String> {
    let registered = schema::registered_repos(storage.connection())?;
    let mut message = format!(
        "`{}` is not a registered rag-rat repo in the global index — nothing to remove.",
        path.display()
    );
    if registered.is_empty() {
        message.push_str("\nThe global index has no registered repos.");
    } else {
        message.push_str("\nRegistered repos (remove one by its path):");
        for repo in registered {
            let roots = if repo.roots.is_empty() {
                "(no recorded root)".to_string()
            } else {
                repo.roots.join(", ")
            };
            message.push_str(&format!("\n  {} [{}] — {roots}", repo.display_name, repo.repo_id));
        }
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The P1 data-loss guard: removing a repo whose tree is GONE must NOT let
    /// `discover_config_path` climb out of the missing git boundary and delete an UNRELATED
    /// parent project's `rag-rat.toml`. The ownership gate skips the gone dir before any
    /// discovery runs. Without the guard, the old code discovered and deleted the parent
    /// config, so this test fails on a regression.
    #[test]
    fn cleanup_of_a_gone_tree_does_not_climb_into_a_parent_repos_config() {
        let base = std::env::temp_dir().join(format!(
            "ragrat-rm-guard-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let parent = base.join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let parent_config = parent.join("rag-rat.toml");
        std::fs::write(&parent_config, "# unrelated parent project config\n").unwrap();
        // A removed child repo nested under the parent, whose working tree no longer exists.
        let gone_child = parent.join("vendor").join("gone-child");

        let result = clean_on_disk_footprint(
            &[gone_child.to_string_lossy().to_string()],
            "removed-child-id",
        );

        assert!(
            parent_config.exists(),
            "removing a gone child must NOT delete the parent project's governing config"
        );
        assert!(
            result.configs_removed.is_empty(),
            "a gone tree owns no reachable config to remove"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
