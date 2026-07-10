//! Git-hook, Claude-hook, and GitHub-sync commands, split out of the `commands` god-module:
//! `hooks` (install/uninstall/status of the managed git hooks, or delegate to `claude_hooks`),
//! `claude_hooks` (the Claude Code settings.json hook entries), and `github` (issue / refs sync).
use std::fs;

use rag_rat_core::Config;

use crate::cli::{GithubArgs, GithubCommand, HookAction, HooksArgs};
use crate::render::{print_output, render_papertrail_sync_progress};
use crate::{MANAGED_HOOKS, claude_settings, git_paths, install_hook, is_rag_rat_hook, open_index};

pub(crate) fn papertrail(config: &Config, args: &GithubArgs) -> anyhow::Result<()> {
    match &args.command {
        GithubCommand::Sync { from_refs, issue, offline } => {
            let db = open_index(config)?;
            let report = if let Some(issue) = issue {
                db.papertrail_sync_issue(issue, *offline)?
            } else if *from_refs {
                db.papertrail_sync_from_refs_with_progress(*offline, render_papertrail_sync_progress)?
            } else {
                anyhow::bail!("github sync needs --from-refs or --issue <owner/repo#number>");
            };
            print_output(&report)
        },
    }
}

pub(crate) fn hooks(config: &Config, args: &HooksArgs) -> anyhow::Result<()> {
    if args.claude {
        return claude_hooks(config, args.action.as_str(), args.global);
    }
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

pub(crate) fn claude_hooks(config: &Config, subcommand: &str, global: bool) -> anyhow::Result<()> {
    let path = claude_settings::settings_path(&config.root, global)?;
    let mut settings = claude_settings::read_settings(&path)?;
    match subcommand {
        "install" => {
            let changed = claude_settings::merge_hook_entries(&mut settings);
            if changed {
                claude_settings::write_settings(&path, &settings)?;
            }
            print_output(&serde_json::json!({
                "status": if changed { "installed" } else { "already_installed" },
                "settings_path": path,
                "matchers": ["Grep", "Bash"],
            }))
        },
        "uninstall" => {
            let changed = claude_settings::remove_hook_entries(&mut settings);
            if changed {
                claude_settings::write_settings(&path, &settings)?;
            }
            print_output(&serde_json::json!({
                "status": if changed { "uninstalled" } else { "not_installed" },
                "settings_path": path,
            }))
        },
        "status" => {
            let status = claude_settings::hook_status(&settings);
            print_output(&serde_json::json!({
                "settings_path": path,
                "pretooluse_installed": status.pretooluse,
                "session_start_installed": status.session_start,
            }))
        },
        other => anyhow::bail!("unknown hooks subcommand `{other}`"),
    }
}
