//! Whether `rag-rat` is reachable from a shell, for `doctor` (#1427).
//!
//! The agent plugins keep their binary in a private per-version cache and expose it through a
//! `rag-rat` shim in `~/.local/bin` (`plugin/scripts/launch.js`, `ensurePathShim`). Like Claude
//! Code's native installer, the launcher never edits shell rc files or the Windows user PATH, so
//! when that directory is not on PATH the shim exists and still nothing runs — this is where that
//! gets said, with the fix.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The shim file name the launcher writes: a symlink on Unix, a `.cmd` wrapper on Windows.
const SHIM_NAME: &str = if cfg!(windows) { "rag-rat.cmd" } else { "rag-rat" };

/// The directory the launcher puts its shim in; `RAG_RAT_SHIM_DIR` overrides it there too.
fn shim_dir() -> Option<PathBuf> {
    std::env::var_os("RAG_RAT_SHIM_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".local").join("bin")))
}

/// Whether `dir` is an npm package's `node_modules/.bin`. npm puts those on the PATH of the process
/// it runs, so under `npx -y @rag-rat/bin doctor` — the documented fallback — its temporary wrapper
/// would look like an install the user's own shell can reach. A real `npm install -g` lands in the
/// prefix's `bin`, which this does not match.
fn is_npm_injected(dir: &Path) -> bool {
    dir.file_name() == Some(OsStr::new(".bin"))
        && dir.parent().and_then(Path::file_name) == Some(OsStr::new("node_modules"))
}

/// The first `rag-rat` the user's shell would run from `path_var`.
fn find_on_path(path_var: &OsStr) -> Option<PathBuf> {
    let names: &[&str] =
        if cfg!(windows) { &["rag-rat.exe", "rag-rat.cmd", "rag-rat.bat"] } else { &["rag-rat"] };
    std::env::split_paths(path_var)
        .filter(|dir| !is_npm_injected(dir))
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| is_runnable(candidate))
}

/// A file the shell would run: on Unix it needs an execute bit, or the shell skips it for a later
/// PATH entry. (Windows runs these by extension.)
fn is_runnable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// The `cli` section of `doctor`, computed from `path_var` and the launcher's shim directory.
fn status(path_var: &OsStr, shim_dir: Option<&Path>) -> serde_json::Value {
    let on_path = find_on_path(path_var);
    let shim = shim_dir.map(|dir| dir.join(SHIM_NAME)).filter(|shim| shim.is_file());
    let shim_dir_on_path =
        shim_dir.is_some_and(|dir| std::env::split_paths(path_var).any(|p| p == dir));
    let warning = match (&on_path, &shim) {
        (Some(_), _) => None,
        (None, Some(shim)) => {
            // The advice names the directory the shim is actually in, which `RAG_RAT_SHIM_DIR`
            // may have moved away from ~/.local/bin.
            let dir = shim.parent().unwrap_or(shim).display();
            Some(format!(
                "`rag-rat` is installed at {} but {dir} is not on PATH. {}",
                shim.display(),
                if cfg!(windows) {
                    format!(
                        "Add it with: [Environment]::SetEnvironmentVariable('Path', '{dir};' + \
                         [Environment]::GetEnvironmentVariable('Path', 'User'), 'User'), then \
                         open a new terminal."
                    )
                } else {
                    format!(
                        "Add `export PATH=\"{dir}:$PATH\"` to your shell profile (~/.zshrc, \
                         ~/.bashrc or ~/.profile), then open a new terminal."
                    )
                }
            ))
        },
        (None, None) => Some(
            "`rag-rat` is not on PATH. With an agent plugin it appears in ~/.local/bin once the \
             plugin's MCP server starts; otherwise install it with `npm install -g @rag-rat/bin`. \
             As a last resort run `npx -y @rag-rat/bin@<version> <command>`, pinned to your MCP \
             server's version (an unpinned run can migrate the index past what it can open)."
                .to_string(),
        ),
    };
    serde_json::json!({
        "on_path": on_path,
        "shim": shim,
        "shim_dir_on_path": shim_dir_on_path,
        "warning": warning,
    })
}

/// The marker line of a Windows `rag-rat.cmd` shim. Plain words only: cmd.exe parses redirection
/// even on a `rem` line, so a `>` here could truncate a file every time the shim runs.
const CMD_MARKER: &str = "rem rag-rat plugin shim";

/// Where the plugins cache the binary: the launcher's managed cache and npm's `npx` cache.
struct Caches {
    /// `<XDG_CACHE_HOME|~/.cache>/rag-rat/bin`, whose children are named by version.
    managed: PathBuf,
}

impl Caches {
    fn from_env() -> Option<Self> {
        let cache_home = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|home| home.join(".cache")))?;
        Some(Self { managed: std::path::absolute(cache_home).ok()?.join("rag-rat").join("bin") })
    }

    /// A binary the plugins put there — the only kind the shim ever points at, or replaces.
    fn holds(&self, binary: &Path) -> bool {
        let name = if cfg!(windows) { "rag-rat.exe" } else { "rag-rat" };
        let npx = Path::new("@rag-rat").join("bin").join("node_modules").join(".bin_real");
        binary.file_name() == Some(OsStr::new(name))
            && (binary.starts_with(&self.managed)
                || binary.parent().is_some_and(|dir| dir.ends_with(&npx)))
    }

    /// The release a cached binary is: its managed-cache directory names it; an npx one is asked.
    /// `None` for a binary that is gone or does not answer.
    fn version_of(&self, binary: &Path) -> Option<String> {
        if !binary.is_file() {
            return None;
        }
        if let Ok(rest) = binary.strip_prefix(&self.managed) {
            return rest.iter().next().map(|v| v.to_string_lossy().into_owned());
        }
        let out = std::process::Command::new(binary).arg("--version").output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace().nth(1).map(str::to_owned)
    }
}

/// What the shim at `shim` points at: `Ok(None)` when there is no shim, `Err(())` when something
/// is there that the plugins did not create (a user's own install, a foreign link).
fn shim_target(shim: &Path, caches: &Caches) -> Result<Option<PathBuf>, ()> {
    let Ok(meta) = shim.symlink_metadata() else {
        return Ok(None);
    };
    let target = if cfg!(windows) {
        let text = std::fs::read_to_string(shim).map_err(|_| ())?;
        let mut lines = text.lines();
        if !lines.any(|line| line.trim() == CMD_MARKER) {
            return Err(());
        }
        let exec = text.lines().find_map(|line| line.strip_suffix(" %*")).ok_or(())?;
        PathBuf::from(exec.trim_matches('"'))
    } else {
        if !meta.file_type().is_symlink() {
            return Err(());
        }
        std::fs::read_link(shim).map_err(|_| ())?
    };
    if caches.holds(&target) { Ok(Some(target)) } else { Err(()) }
}

/// Point the PATH shim at `binary` when it is a plugin-cached binary and the shim is missing,
/// dangling, or pointing at an older release. `Ok(true)` when the shim was (re)written.
///
/// The plugins cache their binary in a private per-version directory, so without this the
/// documented `rag-rat <command>` does not work for anyone who installed only a plugin. Every
/// harness starts `rag-rat mcp`, so doing it here covers them all, from the first session. Like
/// Claude Code's native installer it never edits a shell profile or the Windows user PATH —
/// `doctor` says when the directory is not on PATH. It never replaces something it did not create,
/// and only moves forward, so plugins for two agents on different versions do not fight over it; a
/// dev build or a `cargo install` binary is never linked.
fn refresh_at(binary: &Path, version: &str, caches: &Caches, dir: &Path) -> anyhow::Result<bool> {
    if !caches.holds(binary) {
        return Ok(false);
    }
    let shim = dir.join(SHIM_NAME);
    let current = match shim_target(&shim, caches) {
        Err(()) => return Ok(false),
        Ok(current) => current,
    };
    if current.as_deref() == Some(binary) {
        return Ok(false);
    }
    if let Some(current) = &current {
        let newer = match (
            crate::hooks_support::release_version(version),
            caches.version_of(current).as_deref().and_then(crate::hooks_support::release_version),
        ) {
            (Some(ours), Some(theirs)) => ours > theirs,
            (Some(_), None) => true, // dangling or unreadable: anything replaces it
            (None, _) => false,
        };
        if !newer {
            return Ok(false);
        }
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".rag-rat-shim-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(binary, &tmp)?;
    #[cfg(not(unix))]
    std::fs::write(&tmp, format!("@echo off\r\n{CMD_MARKER}\r\n\"{}\" %*\r\n", binary.display()))?;
    std::fs::rename(&tmp, &shim)?; // atomic replace
    Ok(true)
}

/// [`refresh_at`] for this process: the running binary, this release, the default caches and
/// shim directory. `RAG_RAT_NO_PATH_SHIM=1` turns it off.
pub(crate) fn refresh_path_shim() -> anyhow::Result<Option<PathBuf>> {
    if std::env::var_os("RAG_RAT_NO_PATH_SHIM").is_some_and(|v| v == "1") {
        return Ok(None);
    }
    let (Some(caches), Some(dir)) = (Caches::from_env(), shim_dir()) else {
        return Ok(None);
    };
    let binary = std::env::current_exe()?;
    Ok(refresh_at(&binary, env!("CARGO_PKG_VERSION"), &caches, &dir)?.then(|| dir.join(SHIM_NAME)))
}

/// The `cli` section of `doctor` for this process's environment.
pub(crate) fn doctor_status() -> serde_json::Value {
    status(&std::env::var_os("PATH").unwrap_or_default(), shim_dir().as_deref())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use super::*;

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    /// An executable stub `rag-rat` in `dir`, named the way this platform looks it up.
    fn install_stub(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let stub = dir.join(if cfg!(windows) { "rag-rat.cmd" } else { "rag-rat" });
        fs::write(&stub, "stub").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        stub
    }

    /// The case the section exists for: the launcher made the shim, but its directory is not on
    /// PATH, so a shell still cannot run `rag-rat` — say where it is and how to fix PATH.
    #[test]
    fn a_shim_outside_path_is_reported_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dir.path().join("shims");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        install_stub(&shims);

        let report = status(&path_of(&[&elsewhere]), Some(&shims));
        assert_eq!(report["on_path"], serde_json::Value::Null);
        assert_eq!(report["shim_dir_on_path"], false);
        let warning = report["warning"].as_str().expect("a shim off PATH warns");
        let fix = if cfg!(windows) {
            format!("'{};'", shims.display())
        } else {
            format!("\"{}:$PATH\"", shims.display())
        };
        assert!(warning.contains(&fix), "the fix names the shim's own directory: {warning}");

        let report = status(&path_of(&[&elsewhere, &shims]), Some(&shims));
        assert_eq!(report["shim_dir_on_path"], true);
        assert_eq!(report["warning"], serde_json::Value::Null, "reachable, nothing to say");
    }

    /// Under `npx`, npm's own `node_modules/.bin` wrapper is on PATH; it is not an install.
    #[test]
    fn an_npm_injected_wrapper_does_not_count_as_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let npm_bin = dir.path().join("_npx/abc/node_modules/.bin");
        let global_bin = dir.path().join("prefix/bin");
        for bin in [&npm_bin, &global_bin] {
            install_stub(bin);
        }
        let none = dir.path().join("none");
        assert_eq!(status(&path_of(&[&npm_bin]), Some(&none))["on_path"], serde_json::Value::Null);
        let report = status(&path_of(&[&npm_bin, &global_bin]), Some(&none));
        assert!(report["on_path"].as_str().is_some_and(|p| p.contains("prefix")), "{report}");
    }

    /// A `rag-rat` without an execute bit is not something the shell can run; the search moves on.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_rag_rat_is_skipped_for_a_later_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let broken = install_stub(&dir.path().join("broken"));
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o644)).unwrap();
        let working = install_stub(&dir.path().join("working"));
        let report = status(
            &path_of(&[&dir.path().join("broken"), &dir.path().join("working")]),
            Some(&dir.path().join("none")),
        );
        assert_eq!(report["on_path"], serde_json::json!(working));
    }

    /// The shim cases, against stub binaries in a scratch cache (Unix: the shim is a symlink
    /// there).
    #[cfg(unix)]
    mod shim {
        use std::os::unix::fs::PermissionsExt;

        use super::*;

        struct Fixture {
            _dir: tempfile::TempDir,
            caches: Caches,
            shims: PathBuf,
            npx: PathBuf,
        }

        fn fixture() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let caches = Caches { managed: dir.path().join("cache/rag-rat/bin") };
            let shims = dir.path().join("shims");
            let npx =
                dir.path().join("npm/_npx/abc/node_modules/@rag-rat/bin/node_modules/.bin_real");
            Fixture { caches, shims, npx, _dir: dir }
        }

        /// A stub `rag-rat` at `dir` that reports `version`.
        fn stub(dir: &Path, version: &str) -> PathBuf {
            fs::create_dir_all(dir).unwrap();
            let bin = dir.join("rag-rat");
            fs::write(&bin, format!("#!/bin/sh\necho \"rag-rat {version}\"\n")).unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            bin
        }

        impl Fixture {
            fn managed(&self, version: &str) -> PathBuf {
                stub(&self.caches.managed.join(version), version)
            }
            fn refresh(&self, binary: &Path, version: &str) -> bool {
                refresh_at(binary, version, &self.caches, &self.shims).unwrap()
            }
            fn target(&self) -> Option<PathBuf> {
                fs::read_link(self.shims.join(SHIM_NAME)).ok()
            }
        }

        #[test]
        fn created_then_moved_forward_never_back() {
            let f = fixture();
            let (v1, v2) = (f.managed("1.2.0"), f.managed("1.3.0"));
            assert!(f.refresh(&v1, "1.2.0"), "created");
            assert_eq!(f.target(), Some(v1.clone()));
            assert!(!f.refresh(&v1, "1.2.0"), "already right: untouched");
            assert!(f.refresh(&v2, "1.3.0"), "a newer release moves it");
            assert!(!f.refresh(&v1, "1.2.0"), "an older one does not");
            assert_eq!(f.target(), Some(v2.clone()));

            fs::remove_file(&v2).unwrap();
            assert!(f.refresh(&v1, "1.2.0"), "a dangling link is replaced by anything");
            assert_eq!(f.target(), Some(v1));
        }

        #[test]
        fn links_an_npx_cached_binary() {
            let f = fixture();
            let bin = stub(&f.npx, "1.5.0");
            assert!(f.refresh(&bin, "1.5.0"));
            assert_eq!(f.target(), Some(bin));
        }

        /// A dev build or a `cargo install` binary is not the plugin's to expose.
        #[test]
        fn never_links_a_binary_outside_the_plugin_caches() {
            let f = fixture();
            let dev = stub(&f._dir.path().join("target/debug"), "9.9.9");
            assert!(!f.refresh(&dev, "9.9.9"));
            assert_eq!(f.target(), None);
        }

        #[test]
        fn never_replaces_what_it_did_not_create() {
            let f = fixture();
            let ours = f.managed("2.0.0");
            fs::create_dir_all(&f.shims).unwrap();
            let shim = f.shims.join(SHIM_NAME);

            std::os::unix::fs::symlink("/usr/bin/true", &shim).unwrap();
            assert!(!f.refresh(&ours, "2.0.0"), "a foreign symlink stays");
            assert_eq!(f.target(), Some(PathBuf::from("/usr/bin/true")));

            fs::remove_file(&shim).unwrap();
            fs::write(&shim, "#!/bin/sh\necho mine\n").unwrap();
            assert!(!f.refresh(&ours, "2.0.0"), "a user's own file stays");
            assert_eq!(fs::read_to_string(&shim).unwrap(), "#!/bin/sh\necho mine\n");
        }
    }

    #[test]
    fn no_rag_rat_anywhere_points_at_npx() {
        let dir = tempfile::tempdir().unwrap();
        let report = status(&path_of(&[dir.path()]), Some(&dir.path().join("none")));
        let warning = report["warning"].as_str().expect("unreachable warns");
        assert!(warning.contains("npx -y @rag-rat/bin"), "{warning}");
    }
}
