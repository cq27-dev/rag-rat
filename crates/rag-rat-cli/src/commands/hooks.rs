//! Git-hook and papertrail-sync commands, split out of the `commands` god-module:
//! `hooks` (install/uninstall/status of the managed git hooks) and `papertrail` (item / refs
//! sync).
use std::fs;

use rag_rat_core::Config;
use rag_rat_core::index::papertrail::autosync;

use crate::cli::{HookAction, HooksArgs, PapertrailArgs, PapertrailCommand};
use crate::render::print_output;
use crate::{MANAGED_HOOKS, git_paths, install_hook, is_rag_rat_hook};

pub(crate) fn papertrail(config: &Config, args: &PapertrailArgs) -> anyhow::Result<()> {
    match &args.command {
        PapertrailCommand::Sync { full } => {
            // Manual sync shares the per-repo flight lock with automatic sync: two mirror runs
            // over one binding would interleave their cursor load/save cycles and clobber each
            // other's walk state. A running automatic flight is waited out (observably), never
            // substituted for the unconditional manual pass.
            let report = autosync::run_manual(config, *full, || {
                eprintln!(
                    "papertrail: an automatic sync flight is running; waiting for it to finish \
                     (Ctrl-C to abort)…"
                );
            })?;
            print_output(&report)
        },
    }
}

pub(crate) fn hooks(config: &Config, args: &HooksArgs) -> anyhow::Result<()> {
    let git = git_paths(&config.root)?;
    match args.action {
        HookAction::Install => {
            fs::create_dir_all(&git.hooks_dir)?;
            let mut installed = Vec::new();
            for hook in MANAGED_HOOKS {
                install_hook(&git.hooks_dir, hook)?;
                installed.push(*hook);
            }
            print_output(&serde_json::json!({
                "status": "installed",
                "repo_root": git.worktree_root,
                "git_dir": git.git_dir,
                "git_common_dir": git.git_common_dir,
                "hooks_dir": git.hooks_dir,
                "hooks": installed,
            }))
        },
        HookAction::Uninstall => {
            let mut removed = Vec::new();
            let mut kept = Vec::new();
            for hook in MANAGED_HOOKS {
                let path = git.hooks_dir.join(hook);
                if !path.exists() {
                    continue;
                }
                if is_rag_rat_hook(&path)? {
                    fs::remove_file(&path)?;
                    removed.push(*hook);
                } else {
                    kept.push(*hook);
                }
            }
            print_output(&serde_json::json!({
                "status": "uninstalled",
                "hooks_dir": git.hooks_dir,
                "removed": removed,
                "kept_unmanaged": kept,
            }))
        },
        HookAction::Status => {
            let hooks = MANAGED_HOOKS
                .iter()
                .map(|hook| {
                    let path = git.hooks_dir.join(hook);
                    let managed = is_rag_rat_hook(&path).unwrap_or(false);
                    serde_json::json!({
                        "name": hook,
                        "path": path,
                        "exists": path.exists(),
                        "managed": managed,
                    })
                })
                .collect::<Vec<_>>();
            print_output(&serde_json::json!({
                "repo_root": git.worktree_root,
                "git_dir": git.git_dir,
                "git_common_dir": git.git_common_dir,
                "hooks_dir": git.hooks_dir,
                "hooks": hooks,
            }))
        },
    }
}
