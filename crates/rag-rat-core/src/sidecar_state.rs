//! The MCP server's per-project sidecar state: a small JSON file next to the index DB (under the
//! gitignored `.rag-rat/`) that persists low-frequency, cross-process signals which don't belong in
//! the index. One file combines what used to be several: the crates.io version-check cache and the
//! stale-memory rebind-nudge throttle (#752).
//!
//! Every access is best-effort and FAIL-OPEN — a missing/corrupt/unwritable file yields defaults,
//! never an error. Writes are read-modify-write under a NON-BLOCKING [`FileLock`] so two MCP
//! processes (one per session) updating DIFFERENT sections don't clobber each other; on contention
//! the write is skipped rather than waited on (see [`update`]).

use std::path::{Path, PathBuf};

use rag_rat_base::locks::FileLock;
use serde::{Deserialize, Serialize};

use crate::version_check::CachedVersion;

/// The combined sidecar file's shape. Every section is optional so an older/newer file (or a
/// partial write) still deserializes; unknown fields are ignored by serde's default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SidecarState {
    /// The last crates.io version-check result (was `version-check.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_check: Option<CachedVersion>,
    /// The stale-memory rebind-nudge throttle (#752): when the fleet last showed the nudge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_nudge: Option<MemoryNudgeState>,
}

/// Throttle state for the stale-anchor rebind nudge (#752): the fleet shows it at most once per
/// window (a memory `create`/`update` forces it regardless), so it never rides every tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryNudgeState {
    pub last_shown_at_ms: i64,
}

/// Where the sidecar state lives: next to the index DB, per-project, never indexed.
pub fn state_path(database: &Path) -> PathBuf {
    sidecar_dir(database).join("mcp-state.json")
}

fn sidecar_dir(database: &Path) -> &Path {
    database.parent().unwrap_or_else(|| Path::new("."))
}

/// The legacy pre-#752 version-check file. Read once as a migration fallback so an upgrade doesn't
/// throw away a still-fresh version-check cache (which would force a spurious crates.io re-fetch).
fn legacy_version_cache_path(database: &Path) -> PathBuf {
    sidecar_dir(database).join("version-check.json")
}

/// Read the whole sidecar state, or defaults when absent/unreadable/corrupt (fail-open). Migrates a
/// pre-#752 `version-check.json` into the `version_check` section when the combined file is absent.
pub fn read(database: &Path) -> SidecarState {
    if let Ok(text) = std::fs::read_to_string(state_path(database))
        && let Ok(state) = serde_json::from_str::<SidecarState>(&text)
    {
        return state;
    }
    // Migration fallback: no combined file yet — salvage the legacy version-check cache if present.
    let version_check = std::fs::read_to_string(legacy_version_cache_path(database))
        .ok()
        .and_then(|text| serde_json::from_str::<CachedVersion>(&text).ok());
    SidecarState { version_check, memory_nudge: None }
}

/// Read-modify-write the sidecar state under the file lock, so a concurrent MCP process updating a
/// different section can't clobber this write. Returns `Some(result)` when the update ran (lock
/// held); `None` when the lock was CONTENDED and the update was skipped — the caller falls back to
/// a safe default.
///
/// The lock is NON-BLOCKING (`try_acquire`), deliberately (#752 review): the stale-nudge path
/// enters this on nearly every tool call, so a blocking wait would let one stuck/paused peer
/// process freeze every MCP session for the repo. A single try-lock instead — on contention we
/// skip. That's safe because this state is advisory and self-correcting: a skipped nudge claim
/// re-evaluates on the next call, a skipped version-cache write re-persists on the next refresh.
/// Never stalling a tool call is worth losing a rare, benign write.
fn update<T>(database: &Path, mutate: impl FnOnce(&mut SidecarState) -> T) -> Option<T> {
    let dir = sidecar_dir(database);
    let _ = std::fs::create_dir_all(dir);
    let _lock = FileLock::try_acquire(&dir.join("mcp-state.lock")).ok().flatten()?;
    let mut state = read(database);
    let out = mutate(&mut state);
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = std::fs::write(state_path(database), json);
    }
    Some(out)
}

/// The cached crates.io result, or `None` when never checked (fail-open).
pub fn read_version_cache(database: &Path) -> Option<CachedVersion> {
    read(database).version_check
}

/// Persist a fresh crates.io result into the combined file (RMW, preserving other sections).
/// Best-effort: a lock-contended skip just means the next refresh re-persists.
pub fn write_version_cache(database: &Path, cached: &CachedVersion) {
    let _ = update(database, |state| state.version_check = Some(cached.clone()));
}

/// Claim the rebind-nudge slot: return whether the nudge should show NOW and, if so, record it as
/// shown (atomically under the lock, so concurrent processes don't both show). It shows when
/// `force` (a memory create/update just ran) OR the throttle window has elapsed since the last
/// show.
pub fn take_memory_nudge_slot(database: &Path, now_ms: i64, ttl_ms: i64, force: bool) -> bool {
    // On lock contention the slot can't be claimed atomically, so default to NOT showing: fewer
    // tokens (the goal of #752) and the next call re-evaluates.
    update(database, |state| {
        let elapsed = state
            .memory_nudge
            .is_none_or(|nudge| now_ms.saturating_sub(nudge.last_shown_at_ms) >= ttl_ms);
        let show = force || elapsed;
        if show {
            state.memory_nudge = Some(MemoryNudgeState { last_shown_at_ms: now_ms });
        }
        show
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rr-sidecar-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".rag-rat")).unwrap();
        dir.join(".rag-rat").join("index.sqlite")
    }

    const TTL: i64 = 30 * 60 * 1000;

    #[test]
    fn nudge_slot_throttles_to_the_window_but_forces_on_demand() {
        let db = temp_db("nudge");
        // First ever show: nothing recorded → the window has "elapsed", so it shows and records.
        assert!(take_memory_nudge_slot(&db, 1_000, TTL, false), "first show");
        // Within the window, non-forced: throttled.
        assert!(
            !take_memory_nudge_slot(&db, 1_000 + TTL - 1, TTL, false),
            "throttled inside window"
        );
        // Force (a memory create/update) shows regardless AND resets the window.
        assert!(take_memory_nudge_slot(&db, 1_000 + TTL - 1, TTL, true), "forced show");
        // Now the reset window throttles again just after the forced show.
        assert!(!take_memory_nudge_slot(&db, 1_000 + TTL, TTL, false), "reset window throttles");
        // Once the full window elapses from the last show, it shows again.
        assert!(take_memory_nudge_slot(&db, 1_000 + 2 * TTL, TTL, false), "window elapsed → show");
        let _ = std::fs::remove_dir_all(db.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn nudge_and_version_cache_coexist_in_one_file() {
        let db = temp_db("coexist");
        write_version_cache(&db, &CachedVersion {
            latest_version: "9.9.9".into(),
            checked_at_ms: 7,
        });
        // Claiming the nudge slot must PRESERVE the version-check section (RMW, not overwrite).
        assert!(take_memory_nudge_slot(&db, 5_000, TTL, false));
        let state = read(&db);
        assert_eq!(state.version_check.unwrap().latest_version, "9.9.9");
        assert_eq!(state.memory_nudge.unwrap().last_shown_at_ms, 5_000);
        let _ = std::fs::remove_dir_all(db.parent().unwrap().parent().unwrap());
    }
}
