use super::*;

pub(crate) fn git_paths(root: &Path) -> anyhow::Result<GitPaths> {
    // Env-aware discovery (honors GIT_DIR / GIT_WORK_TREE) so `hooks install/status/uninstall`
    // works in a bare-dir + external-worktree checkout, matching the old `git -C root
    // rev-parse` (#213 review).
    let repo = gix::discover_with_environment_overrides(root)?;
    let worktree_root =
        repo.workdir().ok_or_else(|| anyhow::anyhow!("not inside a git worktree"))?.to_path_buf();
    // gix may report the git dir relative to the discovery root; mirror the old `rev-parse`
    // absolutize (relative -> `root.join`).
    let absolutize = |path: &Path| {
        if path.is_absolute() { path.to_path_buf() } else { root.join(path) }
    };
    let git_dir = absolutize(repo.git_dir());
    let git_common_dir = absolutize(repo.common_dir());
    // `core.hooksPath` overrides the default (git resolves a relative value against the worktree);
    // otherwise the default is the COMMON hooks dir — a linked worktree shares `<main>/.git/hooks`,
    // NOT its private `<git-dir>/worktrees/<name>/hooks`, so installing there would write hooks git
    // never runs (#213 review). For the main worktree git_dir == common_dir, so this is correct
    // there too.
    let hooks_dir = repo
        .config_snapshot()
        .trusted_path("core.hooksPath")
        .ok()
        .flatten()
        .map(|path| if path.is_absolute() { path } else { worktree_root.join(path) })
        .unwrap_or_else(|| git_common_dir.join("hooks"));
    Ok(GitPaths { worktree_root, git_dir, git_common_dir, hooks_dir })
}
pub(crate) fn install_hook(hooks_dir: &Path, hook: &str) -> anyhow::Result<()> {
    let path = hooks_dir.join(hook);
    if path.exists() && !is_rag_rat_hook(&path)? {
        anyhow::bail!(
            "{} already exists and is not managed by rag-rat; move it aside or merge manually",
            path.display()
        );
    }
    write_atomic(&path, hook_script(hook).as_bytes())?;
    make_executable(&path)?;
    Ok(())
}
pub(crate) fn is_rag_rat_hook(path: &Path) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read_to_string(path)?.contains(HOOK_MARKER))
}
pub(crate) fn hook_script(hook: &str) -> String {
    let command = match hook {
        "post-checkout" =>
            r#"rag-rat maintenance \
    --trigger post-checkout \
    --old-head "$1" \
    --new-head "$2" \
    --branch-checkout "$3" \
    --max-seconds 30"#,
        // No positional args: git passes post-merge a squash flag (0/1) and post-rewrite the
        // command (amend/rebase); `rag-rat maintenance` takes no positionals, so forwarding
        // them ("$@") made the hook abort with `unexpected argument`. The trigger flag is
        // all maintenance needs — it re-discovers either way.
        "post-merge" =>
            r#"rag-rat maintenance \
    --trigger post-merge \
    --max-seconds 30"#,
        "post-rewrite" =>
            r#"rag-rat maintenance \
    --trigger post-rewrite \
    --max-seconds 30"#,
        // git passes no positional args to post-commit; HEAD has already advanced, so the
        // maintenance discover-index re-keys the just-committed files under the new commit.
        "post-commit" =>
            r#"rag-rat maintenance \
    --trigger post-commit \
    --max-seconds 30"#,
        _ => unreachable!("unknown managed hook"),
    };
    format!(
        r#"#!/bin/sh
{HOOK_MARKER} Edit rag-rat config, not this hook.

if [ "${{RAG_RAT_HOOK_DISABLE:-}}" = "1" ]; then
  exit 0
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
cd "$repo_root" || exit 0

# Run rag-rat in a CLEAN git environment. Git exports GIT_DIR / GIT_WORK_TREE / GIT_INDEX_FILE / ...
# to every hook, pointing at the worktree the operation ran in. rag-rat must resolve the repo from
# its OWN config root (anchored to the main worktree), not the launching git env — otherwise a hook
# fired in a linked worktree mis-scopes the one shared index (the base + every overlay resolve to
# that worktree, collapsing deltas and pruning rows). Belt-and-suspenders with discover_repo's
# path-first resolution; also drops the transient GIT_INDEX_FILE the in-progress git op set.
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_PREFIX GIT_NAMESPACE \
  GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES

RAG_RAT_HOOK_DISABLE=1 \
  {command} >"${{TMPDIR:-/tmp}}/rag-rat-{hook}.log" 2>&1 &

exit 0
"#
    )
}
#[cfg(unix)]
pub(crate) fn make_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}
#[cfg(not(unix))]
pub(crate) fn make_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_hooks_clear_git_env_and_forward_no_positionals() {
        for hook in ["post-checkout", "post-commit", "post-merge", "post-rewrite"] {
            let script = hook_script(hook);
            assert!(script.contains(HOOK_MARKER), "{hook}: missing marker");
            // git passes post-merge a squash flag (0/1) and post-rewrite a command (amend/rebase);
            // `rag-rat maintenance` takes no positionals, so forwarding "$@" aborted the hook.
            assert!(
                !script.contains("\"$@\""),
                "{hook}: forwards git's positional args to maintenance"
            );
            // The hook must clear git's inherited env BEFORE invoking rag-rat, so a hook fired in a
            // linked worktree can't hijack the shared index's repo resolution via GIT_DIR/etc.
            let unset = script.find("unset GIT_DIR").expect("hook clears GIT_DIR");
            assert!(script.contains("GIT_WORK_TREE") && script.contains("GIT_INDEX_FILE"));
            let invoke = script.find("rag-rat maintenance").expect("hook invokes maintenance");
            assert!(unset < invoke, "{hook}: clears git env AFTER invoking rag-rat");
        }
    }
}
