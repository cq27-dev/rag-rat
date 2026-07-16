//! The per-machine data directory for rag-rat's consolidated global store. The cascade mirrors
//! `index::ai::helpers::fastembed_cache_dir`, but targets the XDG *data* dir (durable, authored
//! state) rather than the *cache* dir (disposable, re-derivable) — memories and the op log live
//! here and must survive `git clean -fdx` of any checkout.

use std::path::PathBuf;

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

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};

    use super::*;

    /// Serializes the env-mutating tests. `set_var`/`remove_var` mutate PROCESS-global state, so
    /// under a thread-based runner (`cargo test`) two of these racing would flake; nextest's
    /// process-per-test isolation makes it moot there, but the mutex keeps both runners honest.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Run `body` with the four cascade vars forced to `values` (`None` = removed), restoring the
    /// prior environment afterward so tests never leak state into each other.
    fn with_env(values: &[(&str, Option<&str>)], body: impl FnOnce()) {
        const KEYS: [&str; 4] = ["RAG_RAT_DATA_DIR", "XDG_DATA_HOME", "HOME", "APPDATA"];
        let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let saved: Vec<(&str, Option<String>)> =
            KEYS.iter().map(|&key| (key, std::env::var(key).ok())).collect();
        // SAFETY: env access is serialized by ENV_LOCK for the duration of this call.
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
