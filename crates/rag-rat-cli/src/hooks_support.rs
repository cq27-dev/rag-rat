use std::fs;
use std::path::{Path, PathBuf};

use crate::fs_atomic::write_atomic;
use crate::{DEFAULT_MAINTENANCE_SECONDS, HOOK_MARKER};

// Variant names mirror Git’s installed trigger tokens, including their shared `post-` prefix.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub(crate) enum ManagedHook {
    PostCheckout,
    PostMerge,
    PostRewrite,
    PostCommit,
}

impl ManagedHook {
    pub(crate) const ALL: &[Self] =
        &[Self::PostCheckout, Self::PostMerge, Self::PostRewrite, Self::PostCommit];

    pub(crate) fn as_trigger(self) -> &'static str {
        self.into()
    }

    pub(crate) fn from_trigger(trigger: &str) -> Option<Self> {
        trigger.parse().ok()
    }

    pub(crate) fn changes_files(self) -> bool {
        matches!(self, Self::PostCheckout | Self::PostMerge)
    }
}

#[derive(Debug)]
pub(crate) struct GitPaths {
    worktree_root: PathBuf,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
    hooks_dir: PathBuf,
}

impl GitPaths {
    pub(crate) fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }
    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }
    pub(crate) fn git_common_dir(&self) -> &Path {
        &self.git_common_dir
    }
    pub(crate) fn hooks_dir(&self) -> &Path {
        &self.hooks_dir
    }
}

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
pub(crate) fn install_hook(hooks_dir: &Path, hook: ManagedHook) -> anyhow::Result<()> {
    let path = hooks_dir.join(hook.as_trigger());
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
/// Install every managed hook, or none. Refuses before writing anything when any slot holds a hook
/// rag-rat does not manage, naming every such slot: installing hook by hook stopped at the first
/// conflict and left a partial set that kept some git operations fresh and not others.
pub(crate) fn install_managed_hooks(hooks_dir: &Path) -> anyhow::Result<Vec<ManagedHook>> {
    let mut foreign = Vec::new();
    for &hook in ManagedHook::ALL {
        let path = hooks_dir.join(hook.as_trigger());
        if path.exists() && !is_rag_rat_hook(&path)? {
            foreign.push(path.display().to_string());
        }
    }
    anyhow::ensure!(
        foreign.is_empty(),
        "no hooks were installed: {} already exist and are not managed by rag-rat; move them \
         aside or merge them manually, then re-run",
        foreign.join(", ")
    );
    fs::create_dir_all(hooks_dir)?;
    for &hook in ManagedHook::ALL {
        install_hook(hooks_dir, hook)?;
    }
    Ok(ManagedHook::ALL.to_vec())
}

pub(crate) fn is_rag_rat_hook(path: &Path) -> anyhow::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read_to_string(path)?.contains(HOOK_MARKER))
}
/// The npm package a hook runs, pinned to this binary's version.
const PINNED_PACKAGE: &str = concat!("@rag-rat/bin@", env!("CARGO_PKG_VERSION"));

/// How every managed hook invokes `rag-rat maintenance {args}`: through npx, pinned to the version
/// that wrote the hook, falling back to `rag-rat` on PATH.
///
/// npx first because the server runs from the npx package, so a machine can have a working install
/// with no `rag-rat` on PATH at all; a bare invocation then fails in the background on every git
/// operation while the index silently goes stale. Pinned, not `@latest`: a newer binary migrates
/// the shared index forward, and a server pinned to the older release then refuses to open it. The
/// pin follows plugin updates through [`refresh_managed_hooks`].
///
/// The PATH fallback keeps installs the npm package cannot serve — `cargo install` without Node,
/// and the source-only platforms — maintained as before. It also runs when a pinned run genuinely
/// fails; maintenance is idempotent, so that costs one retry, not a second effect.
///
/// A group, so the one redirect in the hook covers both attempts.
pub(crate) fn maintenance_invocation(args: &str) -> String {
    format!("{{\n  npx -y {PINNED_PACKAGE} maintenance {args} ||\n  rag-rat maintenance {args}\n}}")
}

/// Whether the hook at `path` is exactly what this binary would install for `hook`: a managed hook
/// written by another version, or by an older generator, is not.
pub(crate) fn hook_is_current(path: &Path, hook: ManagedHook) -> bool {
    fs::read_to_string(path).is_ok_and(|script| script == hook_script(hook))
}

/// A release version as comparable numbers. `None` for anything that is not plain `X.Y.Z`.
pub(crate) fn release_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|part| part.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// The version a managed hook script pins, if it pins one.
fn pinned_version(script: &str) -> Option<&str> {
    let rest = script.split_once("npx -y @rag-rat/bin@")?.1;
    rest.split_whitespace().next()
}

/// Rewrite this repo's managed hooks that pin an OLDER rag-rat than this binary, or none at all.
///
/// The plugin updates itself, so the server moves to a new release and migrates the shared index
/// while hooks written earlier still pin the old one — which then refuses the newer schema, and
/// the index goes silently stale with nobody re-running `hooks install`. The server calls this at
/// startup, so an update reaches the hooks the next time it starts.
///
/// Upgrade-only: two servers on different versions (one per agent's plugin) must not rewrite the
/// hooks back and forth, and the higher version is the one the index can be opened by. Only hooks
/// rag-rat already manages are touched — this never installs a hook the user did not ask for — and
/// a hook on a version this binary cannot order (a pre-release, a local build) is left alone.
///
/// The read, compare and rewrite run under one lock in the hooks directory: two servers starting
/// together could otherwise both read an old hook, and the older one write last, downgrading it.
/// A lock not free within a few seconds skips the refresh; the next start retries.
pub(crate) fn refresh_managed_hooks(hooks_dir: &Path) -> anyhow::Result<Vec<ManagedHook>> {
    let Some(_lock) = rag_rat_base::locks::FileLock::acquire_timeout(
        &hooks_dir.join(".rag-rat-hooks.lock"),
        std::time::Duration::from_secs(5),
    )?
    else {
        return Ok(Vec::new());
    };
    let own = release_version(env!("CARGO_PKG_VERSION"));
    let mut refreshed = Vec::new();
    for &hook in ManagedHook::ALL {
        let path = hooks_dir.join(hook.as_trigger());
        if !is_rag_rat_hook(&path)? {
            continue;
        }
        let script = fs::read_to_string(&path)?;
        let stale = match pinned_version(&script) {
            None => true,
            Some(pinned) => match (release_version(pinned), own) {
                (Some(pinned), Some(own)) => pinned < own,
                _ => false,
            },
        };
        if stale {
            install_hook(hooks_dir, hook)?;
            refreshed.push(hook);
        }
    }
    Ok(refreshed)
}

pub(crate) fn hook_script(hook: ManagedHook) -> String {
    let trigger = hook.as_trigger();
    let args = match hook {
        ManagedHook::PostCheckout => format!(
            r#"--trigger {trigger} --old-head "$1" --new-head "$2" --branch-checkout "$3" --max-seconds {DEFAULT_MAINTENANCE_SECONDS}"#
        ),
        // No positional args: git passes post-merge a squash flag (0/1) and post-rewrite the
        // command (amend/rebase); maintenance needs only the trigger. post-commit passes none.
        ManagedHook::PostMerge | ManagedHook::PostRewrite | ManagedHook::PostCommit =>
            format!("--trigger {trigger} --max-seconds {DEFAULT_MAINTENANCE_SECONDS}"),
    };
    let command = maintenance_invocation(&args);
    let hook = trigger;
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

# Exported, not a command prefix: a prefix would cover only the first of the two attempts.
RAG_RAT_HOOK_DISABLE=1
export RAG_RAT_HOOK_DISABLE
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
        for &managed in ManagedHook::ALL {
            let hook = managed.as_trigger();
            let script = hook_script(managed);
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
            let invoke = script.find("npx -y @rag-rat/bin@").expect("hook invokes maintenance");
            assert!(unset < invoke, "{hook}: clears git env AFTER invoking rag-rat");
        }
    }
}

#[cfg(test)]
mod current_tests {
    use super::*;

    /// A hook this binary would write is current; one from the bare-`rag-rat` generation is not,
    /// which is how `hooks status` surfaces a hook that fails wherever `rag-rat` is not on PATH.
    #[test]
    fn only_the_script_this_binary_writes_is_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("post-commit");
        install_hook(dir.path(), ManagedHook::PostCommit).unwrap();
        assert!(hook_is_current(&path, ManagedHook::PostCommit));

        let bare =
            hook_script(ManagedHook::PostCommit).replace(&format!("npx -y {PINNED_PACKAGE} "), "");
        fs::write(&path, bare).unwrap();
        assert!(is_rag_rat_hook(&path).unwrap(), "still managed");
        assert!(!hook_is_current(&path, ManagedHook::PostCommit), "but not current");
    }
}

#[cfg(test)]
mod install_tests {
    use super::*;

    /// One foreign hook blocks the whole install, and nothing is written: a partial set would keep
    /// some git operations fresh and silently not others.
    #[test]
    fn a_foreign_hook_in_any_slot_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join(ManagedHook::PostRewrite.as_trigger());
        fs::write(&foreign, "#!/bin/sh\necho mine\n").unwrap();

        let err = install_managed_hooks(dir.path()).expect_err("a foreign hook refuses");
        assert!(err.to_string().contains("post-rewrite"), "names the slot: {err}");
        for &hook in ManagedHook::ALL {
            if hook != ManagedHook::PostRewrite {
                assert!(!dir.path().join(hook.as_trigger()).exists(), "{hook:?} was written");
            }
        }
        assert_eq!(fs::read_to_string(&foreign).unwrap(), "#!/bin/sh\necho mine\n");

        fs::remove_file(&foreign).unwrap();
        assert_eq!(install_managed_hooks(dir.path()).unwrap(), ManagedHook::ALL.to_vec());
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;

    fn pinned_to(version: &str) -> String {
        hook_script(ManagedHook::PostCommit)
            .replace(PINNED_PACKAGE, &format!("@rag-rat/bin@{version}"))
    }

    /// A plugin update moves the server to a new release; hooks pinned to an older one (or to none)
    /// are rewritten, and nothing else is touched.
    #[test]
    fn refresh_rewrites_only_managed_hooks_pinned_older_than_this_binary() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path();
        let path = |hook: ManagedHook| hooks.join(hook.as_trigger());

        fs::write(path(ManagedHook::PostCommit), pinned_to("0.0.1")).unwrap();
        let bare =
            hook_script(ManagedHook::PostMerge).replace(&format!("npx -y {PINNED_PACKAGE} "), "");
        fs::write(path(ManagedHook::PostMerge), &bare).unwrap();
        let newer = pinned_to("999.0.0");
        fs::write(path(ManagedHook::PostRewrite), &newer).unwrap();
        // post-checkout is absent: refresh must not install it.

        let refreshed = refresh_managed_hooks(hooks).unwrap();
        assert_eq!(refreshed, vec![ManagedHook::PostMerge, ManagedHook::PostCommit]);
        assert!(hook_is_current(&path(ManagedHook::PostCommit), ManagedHook::PostCommit));
        assert!(hook_is_current(&path(ManagedHook::PostMerge), ManagedHook::PostMerge));
        assert_eq!(
            fs::read_to_string(path(ManagedHook::PostRewrite)).unwrap(),
            newer,
            "no downgrade"
        );
        assert!(!path(ManagedHook::PostCheckout).exists(), "never installs");

        let foreign = "#!/bin/sh\necho mine\n";
        fs::write(path(ManagedHook::PostCommit), foreign).unwrap();
        refresh_managed_hooks(hooks).unwrap();
        assert_eq!(fs::read_to_string(path(ManagedHook::PostCommit)).unwrap(), foreign);
    }

    /// A refresh that cannot take the lock rewrites nothing, rather than racing the holder.
    #[test]
    fn refresh_skips_while_another_refresh_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ManagedHook::PostCommit.as_trigger());
        let old = pinned_to("0.0.1");
        fs::write(&path, &old).unwrap();
        let _held =
            rag_rat_base::locks::FileLock::try_acquire(&dir.path().join(".rag-rat-hooks.lock"))
                .unwrap()
                .expect("free");

        assert!(refresh_managed_hooks(dir.path()).unwrap().is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), old);
    }

    #[test]
    fn only_plain_release_versions_order() {
        assert_eq!(release_version("0.23.2"), Some((0, 23, 2)));
        assert!(release_version("0.23.10") > release_version("0.23.9"));
        assert_eq!(release_version("0.24.0-rc.1"), None);
        assert_eq!(release_version("0.24"), None);
        assert_eq!(pinned_version(&pinned_to("1.2.3")), Some("1.2.3"));
    }
}

/// Run a generated hook against stub `npx` and `rag-rat` binaries and read what each was called
/// with.
#[cfg(all(test, unix))]
mod fallback_tests {
    use std::process::Command;

    use super::*;

    fn run_hook(npx_exit: u8) -> (String, String) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        assert!(Command::new("git").args(["init", "-q"]).arg(&repo).status().unwrap().success());
        for (name, exit) in [("npx", npx_exit), ("rag-rat", 0)] {
            let stub = bin.join(name);
            fs::write(
                &stub,
                format!("#!/bin/sh\necho \"$*\" >> \"$STUB_LOG/{name}\"\nexit {exit}\n"),
            )
            .unwrap();
            make_executable(&stub).unwrap();
        }
        let hook = repo.join("hook");
        fs::write(&hook, hook_script(ManagedHook::PostCommit)).unwrap();
        make_executable(&hook).unwrap();
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("{} && wait", hook.display()))
            .current_dir(&repo)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("STUB_LOG", dir.path())
            .env("TMPDIR", dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        // The hook backgrounds the work; poll for it to land.
        let read = |name: &str| fs::read_to_string(dir.path().join(name)).unwrap_or_default();
        for _ in 0..100 {
            if !read("npx").is_empty() && (npx_exit == 0 || !read("rag-rat").is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        (read("npx"), read("rag-rat"))
    }

    #[test]
    fn a_hook_runs_the_pinned_npx_package_and_falls_back_to_path_only_when_it_fails() {
        let args = format!("--trigger post-commit --max-seconds {DEFAULT_MAINTENANCE_SECONDS}");

        let (npx, path) = run_hook(0);
        assert_eq!(npx.trim(), format!("-y {PINNED_PACKAGE} maintenance {args}"));
        assert!(path.is_empty(), "no fallback when npx succeeds: {path}");

        let (npx, path) = run_hook(1);
        assert!(!npx.is_empty(), "npx is tried first");
        assert_eq!(path.trim(), format!("maintenance {args}"), "then rag-rat on PATH");
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn hook_tokens_and_scripts_are_stable() {
        let fixtures = [
            ("post-checkout", include_str!("hook_fixtures/post-checkout.sh")),
            ("post-merge", include_str!("hook_fixtures/post-merge.sh")),
            ("post-rewrite", include_str!("hook_fixtures/post-rewrite.sh")),
            ("post-commit", include_str!("hook_fixtures/post-commit.sh")),
        ];
        for (&hook, (token, script)) in ManagedHook::ALL.iter().zip(fixtures) {
            assert_eq!(hook.as_trigger(), token);
            assert_eq!(ManagedHook::from_trigger(token), Some(hook));
            let script = script.replace("@VERSION@", env!("CARGO_PKG_VERSION"));
            assert_eq!(hook_script(hook), script);
            assert_eq!(hook.changes_files(), token == "post-checkout" || token == "post-merge");
        }
        assert_eq!(ManagedHook::from_trigger("manual"), None);
        assert_eq!(ManagedHook::from_trigger("POST-COMMIT"), None);
    }
}
