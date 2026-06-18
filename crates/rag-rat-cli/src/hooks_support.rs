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
        .and_then(Result::ok)
        .map(|path| if path.is_absolute() { path.into_owned() } else { worktree_root.join(path) })
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
        "post-merge" =>
            r#"rag-rat maintenance \
    --trigger post-merge \
    --max-seconds 30 \
    "$@""#,
        "post-rewrite" =>
            r#"rag-rat maintenance \
    --trigger post-rewrite \
    --max-seconds 30 \
    "$@""#,
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
