//! The per-machine data directory for rag-rat's consolidated global store. The cascade mirrors
//! `index::ai::helpers::fastembed_cache_dir`, but targets the XDG *data* dir (durable, authored
//! state) rather than the *cache* dir (disposable, re-derivable) — memories and the op log live
//! here and must survive `git clean -fdx` of any checkout.

use std::path::{Path, PathBuf};

/// The per-checkout workspace directory rag-rat keeps beside a repo: the legacy per-repo index,
/// the default log dir, the lens discovery sockets.
pub const WORKSPACE_DIR: &str = ".rag-rat";

/// The legacy per-repo index file's name inside [`WORKSPACE_DIR`].
pub const LEGACY_DATABASE_FILE: &str = "index.sqlite";

/// The suffix `rag-rat consolidate` appends to a database file's name once it has been imported;
/// the marker's presence is the stay-global latch keyless database resolution reads.
pub const IMPORTED_MARKER_SUFFIX: &str = ".imported";

/// The legacy per-repo index under `base` (the main worktree top): `<base>/.rag-rat/index.sqlite`.
/// Spelled with a `/` inside the joined component on every platform, exactly as the path has
/// always been rendered.
pub fn legacy_database_path(base: &Path) -> PathBuf {
    base.join(format!("{WORKSPACE_DIR}/{LEGACY_DATABASE_FILE}"))
}

/// `<database>.imported` — the name the legacy file is renamed to after a successful import.
pub fn imported_marker_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(IMPORTED_MARKER_SUFFIX);
    PathBuf::from(name)
}

/// The rag-rat data directory, resolved by env cascade. An env var set to the empty string is
/// treated as unset (XDG semantics), so the cascade falls through:
///
/// 1. `RAG_RAT_DATA_DIR` — explicit override, honored verbatim.
/// 2. `$XDG_DATA_HOME/rag-rat`.
/// 3. `$HOME/.local/share/rag-rat` (the XDG default when `XDG_DATA_HOME` is unset).
/// 4. (Windows) `%APPDATA%/rag-rat`.
///
/// Returns `None` when none resolve. There is deliberately NO repo-relative fallback: the global
/// DB is machine-scoped, and silently landing it inside a checkout would defeat its purpose — the
/// caller decides what to do without a data dir (e.g. keep using the per-repo `.rag-rat/` path).
pub fn data_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("RAG_RAT_DATA_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    if let Ok(data_home) = std::env::var("XDG_DATA_HOME")
        && !data_home.is_empty()
    {
        return Some(PathBuf::from(data_home).join("rag-rat"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join(".local").join("share").join("rag-rat"));
    }
    #[cfg(windows)]
    if let Ok(appdata) = std::env::var("APPDATA")
        && !appdata.is_empty()
    {
        return Some(PathBuf::from(appdata).join("rag-rat"));
    }
    None
}

/// The consolidated global database path: [`data_dir`]`/"rag-rat.sqlite"`. `None` when no data dir
/// resolves (see [`data_dir`]).
pub fn global_database_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("rag-rat.sqlite"))
}

/// Serializes every test that reads or writes the data-dir cascade variables. `set_var` /
/// `remove_var` mutate PROCESS-global state, so under a thread-based runner (`cargo test`) a test
/// that reads [`data_dir`] while another rewrites the variables flakes; nextest's
/// process-per-test isolation hides it, and the lock keeps both runners honest. Hold the guard for
/// the whole test.
#[cfg(test)]
pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `body` with the four cascade vars forced to `values` (`None` = removed), restoring the
    /// prior environment afterward so tests never leak state into each other.
    fn with_env(values: &[(&str, Option<&str>)], body: impl FnOnce()) {
        const KEYS: [&str; 4] = ["RAG_RAT_DATA_DIR", "XDG_DATA_HOME", "HOME", "APPDATA"];
        let _guard = env_guard();
        let saved: Vec<(&str, Option<String>)> =
            KEYS.iter().map(|&key| (key, std::env::var(key).ok())).collect();
        // SAFETY: env access is serialized by `env_guard` for the duration of this call.
        unsafe {
            for &key in &KEYS {
                std::env::remove_var(key);
            }
            for &(key, value) in values {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        body();
        // SAFETY: still under ENV_LOCK; restore exactly what was there before.
        unsafe {
            for (key, value) in saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// These name files on users' disks — a respelling strands an existing index or its marker.
    #[test]
    fn workspace_file_names_are_pinned() {
        // Compared as the rendered `OsStr`, against a single `join` of the literal tokens: that is
        // the historical spelling on every platform, including the `/` inside the joined
        // component on Windows (where `join` itself inserts a `\` before it).
        let base = Path::new("/repo");
        let legacy = legacy_database_path(base);
        assert_eq!(legacy.as_os_str(), base.join(".rag-rat/index.sqlite").as_os_str());
        assert_eq!(
            imported_marker_path(&legacy).as_os_str(),
            base.join(".rag-rat/index.sqlite.imported").as_os_str()
        );
    }

    #[test]
    fn rag_rat_data_dir_override_wins() {
        with_env(
            &[
                ("RAG_RAT_DATA_DIR", Some("/custom/data")),
                ("XDG_DATA_HOME", Some("/xdg")),
                ("HOME", Some("/home/u")),
            ],
            || {
                assert_eq!(data_dir(), Some(PathBuf::from("/custom/data")));
                assert_eq!(
                    global_database_path(),
                    Some(PathBuf::from("/custom/data/rag-rat.sqlite"))
                );
            },
        );
    }

    #[test]
    fn xdg_data_home_is_next() {
        with_env(&[("XDG_DATA_HOME", Some("/xdg")), ("HOME", Some("/home/u"))], || {
            assert_eq!(data_dir(), Some(PathBuf::from("/xdg/rag-rat")));
        });
    }

    #[test]
    fn home_is_the_xdg_default() {
        with_env(&[("HOME", Some("/home/u"))], || {
            assert_eq!(data_dir(), Some(PathBuf::from("/home/u/.local/share/rag-rat")));
        });
    }

    #[test]
    fn empty_var_is_treated_as_unset() {
        // An empty XDG_DATA_HOME must fall through to HOME (XDG spec), not resolve to `/rag-rat`.
        with_env(&[("XDG_DATA_HOME", Some("")), ("HOME", Some("/home/u"))], || {
            assert_eq!(data_dir(), Some(PathBuf::from("/home/u/.local/share/rag-rat")));
        });
    }

    #[cfg(not(windows))]
    #[test]
    fn none_when_nothing_resolves() {
        // On non-Windows the cascade ends at HOME; with every var unset there is no data dir.
        with_env(&[], || {
            assert_eq!(data_dir(), None);
            assert_eq!(global_database_path(), None);
        });
    }
}
