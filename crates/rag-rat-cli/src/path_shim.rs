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
        .find(|candidate| candidate.is_file())
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
            "`rag-rat` is not on PATH. Run commands as `npx -y @rag-rat/bin <command>`, or \
             install it with `npm install -g @rag-rat/bin` (the agent plugins add a shim in \
             ~/.local/bin on their next launch)."
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

    /// The case the section exists for: the launcher made the shim, but its directory is not on
    /// PATH, so a shell still cannot run `rag-rat` — say where it is and how to fix PATH.
    #[test]
    fn a_shim_outside_path_is_reported_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dir.path().join("shims");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&shims).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(shims.join(SHIM_NAME), "stub").unwrap();

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
            fs::create_dir_all(bin).unwrap();
            fs::write(bin.join(if cfg!(windows) { "rag-rat.cmd" } else { "rag-rat" }), "stub")
                .unwrap();
        }
        let none = dir.path().join("none");
        assert_eq!(status(&path_of(&[&npm_bin]), Some(&none))["on_path"], serde_json::Value::Null);
        let report = status(&path_of(&[&npm_bin, &global_bin]), Some(&none));
        assert!(report["on_path"].as_str().is_some_and(|p| p.contains("prefix")), "{report}");
    }

    #[test]
    fn no_rag_rat_anywhere_points_at_npx() {
        let dir = tempfile::tempdir().unwrap();
        let report = status(&path_of(&[dir.path()]), Some(&dir.path().join("none")));
        let warning = report["warning"].as_str().expect("unreachable warns");
        assert!(warning.contains("npx -y @rag-rat/bin"), "{warning}");
    }
}
