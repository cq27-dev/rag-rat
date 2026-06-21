//! Best-effort "is a newer rag-rat published on crates.io?" check, surfaced to agents and operators
//! (the SessionStart digest and the `index_status` MCP tool). This is the one HTTP call in core; it
//! is **fail-open** — offline, a 403, a parse miss, or a disabled config all yield "no info," never
//! an error — and **cached** to a small JSON file next to the index so reads are instant and
//! session start never blocks (the network refresh runs out of band; see `refresh`).
//!
//! Versions are reported as opaque strings; comparison is a lenient numeric semver (`major.minor.
//! patch`, pre-release/build stripped) — if either side won't parse, no update is claimed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The published crate this binary is built from (the lockstep workspace crate name).
pub const CRATE_NAME: &str = "rag-rat";
/// How the operator updates — printed verbatim in the digest and the structured status.
pub const UPDATE_COMMAND: &str = "cargo install rag-rat --force";
/// Default staleness window: re-check crates.io at most once a day. Reads always use the cache.
pub const DEFAULT_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// Hard cap on the crates.io request so a slow network can never wedge a refresh.
const FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// The running binary's version (lockstep across the three workspace crates, so core's
/// `CARGO_PKG_VERSION` equals the `rag-rat` binary's).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Agent/operator-facing version status. `latest_version`/`checked_at_ms` are `null` until a
/// successful crates.io check has been cached (serialized explicitly, not omitted, so the object
/// shape is stable for consumers that test `latest_version == null`); `update_available` is only
/// ever true on a confirmed newer published version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub update_command: String,
    pub checked_at_ms: Option<i64>,
}

/// The cached result of the last crates.io check (the JSON file's shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedVersion {
    pub latest_version: String,
    pub checked_at_ms: i64,
}

/// Where the version-check cache lives: next to the index DB (under the gitignored, floor-skipped
/// `.rag-rat/`), so it's per-project and never indexed.
pub fn cache_path(database: &Path) -> PathBuf {
    database.parent().unwrap_or_else(|| Path::new(".")).join("version-check.json")
}

/// Read the cached crates.io result, or `None` when absent/unreadable/corrupt (all fail-open).
pub fn read_cache(database: &Path) -> Option<CachedVersion> {
    let text = std::fs::read_to_string(cache_path(database)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist a fresh crates.io result. Best-effort: a write error is swallowed (the next refresh
/// retries), never surfaced.
fn write_cache(database: &Path, cached: &CachedVersion) {
    let path = cache_path(database);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(cached) {
        let _ = std::fs::write(path, json);
    }
}

/// Build the agent/operator-facing status from the current version and the last cached check.
pub fn build_status(current: &str, cached: Option<&CachedVersion>) -> VersionStatus {
    let latest_version = cached.map(|c| c.latest_version.clone());
    let update_available =
        latest_version.as_deref().is_some_and(|latest| is_newer(latest, current));
    VersionStatus {
        current_version: current.to_string(),
        latest_version,
        update_available,
        update_command: UPDATE_COMMAND.to_string(),
        checked_at_ms: cached.map(|c| c.checked_at_ms),
    }
}

/// The cached status for this index, or `None` when version checking is disabled in config. No
/// network — reads the cache only, so callers (digest, `index_status`) never block.
pub fn cached_status(enabled: bool, database: &Path) -> Option<VersionStatus> {
    if !enabled {
        return None;
    }
    Some(build_status(current_version(), read_cache(database).as_ref()))
}

/// Whether the cache is missing or older than `ttl_ms` — i.e. a refresh should run.
/// `now_ms`/`ttl_ms` are injected so the decision is pure and testable.
pub fn needs_refresh(cached: Option<&CachedVersion>, now_ms: i64, ttl_ms: i64) -> bool {
    match cached {
        None => true,
        Some(c) => now_ms.saturating_sub(c.checked_at_ms) >= ttl_ms,
    }
}

/// Fetch the latest published version from crates.io. `None` on any failure (offline, non-200,
/// timeout, unexpected JSON) — the check is best-effort and never an error. A `User-Agent` is
/// mandatory: crates.io 403s a request without one.
pub fn fetch_latest() -> Option<String> {
    let url = format!("https://crates.io/api/v1/crates/{CRATE_NAME}");
    // crates.io 403s a request without a User-Agent, so set it on the agent config (ureq 3).
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .user_agent(concat!("rag-rat/", env!("CARGO_PKG_VERSION"), " (version-check)"))
        .build()
        .into();
    let body = agent.get(&url).call().ok()?.body_mut().read_to_string().ok()?;
    parse_latest_response(&body)
}

/// Extract `crate.max_version` from a crates.io `/api/v1/crates/<name>` JSON body. `None` on
/// malformed JSON or a missing/non-string field — the fail-open parse half of [`fetch_latest`],
/// split out so it's testable without the network.
fn parse_latest_response(body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    json.get("crate")?.get("max_version")?.as_str().map(str::to_string)
}

/// Fetch crates.io and write the cache (network). Returns the fresh [`CachedVersion`] on success,
/// `None` fail-open. The caller gets the result directly so it can report the just-fetched latest
/// even if the cache write fails (read-only checkout, full disk). Run out of band (a long-lived
/// server's background, an explicit `version-check` command) — never on the session-start read
/// path.
pub fn refresh(database: &Path) -> Option<CachedVersion> {
    let cached = CachedVersion { latest_version: fetch_latest()?, checked_at_ms: now_ms() };
    write_cache(database, &cached);
    Some(cached)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `(major, minor, patch)`, pre-release/build metadata stripped. `None` if it doesn't parse.
fn parse_semver(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next().unwrap_or("0").trim().parse().ok()?;
    let patch = parts.next().unwrap_or("0").trim().parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `latest` is a strictly newer published version than `current`. Conservative: if either
/// side won't parse as `major.minor.patch`, returns false (never nag on a version we can't
/// compare).
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_numeric_semver() {
        assert!(is_newer("0.6.0", "0.5.0"));
        assert!(is_newer("0.5.1", "0.5.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.5.10", "0.5.9"), "numeric, not lexical");
        assert!(!is_newer("0.5.0", "0.5.0"), "equal is not newer");
        assert!(!is_newer("0.4.0", "0.5.0"), "older is not newer");
        assert!(is_newer("0.6.0-rc.1", "0.5.0"), "pre-release suffix stripped for the compare");
        assert!(!is_newer("not-a-version", "0.5.0"), "unparseable latest → no update claimed");
        assert!(!is_newer("0.6.0", "garbage"), "unparseable current → no update claimed");
    }

    #[test]
    fn build_status_flags_an_available_update() {
        let cached = CachedVersion { latest_version: "0.6.0".into(), checked_at_ms: 123 };
        let s = build_status("0.5.0", Some(&cached));
        assert_eq!(s.current_version, "0.5.0");
        assert_eq!(s.latest_version.as_deref(), Some("0.6.0"));
        assert!(s.update_available);
        assert_eq!(s.update_command, "cargo install rag-rat --force");
        assert_eq!(s.checked_at_ms, Some(123));
    }

    #[test]
    fn build_status_up_to_date_has_no_update() {
        let cached = CachedVersion { latest_version: "0.5.0".into(), checked_at_ms: 1 };
        let s = build_status("0.5.0", Some(&cached));
        assert!(!s.update_available);
        assert_eq!(s.latest_version.as_deref(), Some("0.5.0"));
    }

    #[test]
    fn build_status_without_cache_reports_unknown_latest() {
        let s = build_status("0.5.0", None);
        assert_eq!(s.latest_version, None);
        assert!(!s.update_available);
        assert_eq!(s.checked_at_ms, None);
        // The update command is always present so an agent can relay it once a latest is known.
        assert_eq!(s.update_command, "cargo install rag-rat --force");
    }

    #[test]
    fn unknown_status_serializes_null_fields_not_omitted() {
        // Stable object shape: latest_version/checked_at_ms are present as null when unknown, so
        // consumers can test `latest_version == null` rather than a missing key.
        let json = serde_json::to_value(build_status("0.5.0", None)).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("latest_version") && obj["latest_version"].is_null());
        assert!(obj.contains_key("checked_at_ms") && obj["checked_at_ms"].is_null());
        assert_eq!(obj["current_version"], "0.5.0");
        assert_eq!(obj["update_available"], false);
    }

    #[test]
    fn cached_status_is_none_when_disabled() {
        assert_eq!(cached_status(false, Path::new("/x/.rag-rat/index.sqlite")), None);
    }

    #[test]
    fn cached_status_enabled_reflects_the_cache() {
        let dir = std::env::temp_dir().join(format!("rr-vcheck-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".rag-rat")).unwrap();
        let database = dir.join(".rag-rat").join("index.sqlite");

        // No cache yet: enabled → Some, but latest unknown and no update claimed.
        let s = cached_status(true, &database).expect("enabled → Some");
        assert_eq!(s.current_version, current_version());
        assert_eq!(s.latest_version, None);
        assert!(!s.update_available);

        // A clearly-newer cached version flips update_available against the running version.
        write_cache(&database, &CachedVersion {
            latest_version: "99.0.0".into(),
            checked_at_ms: 1,
        });
        let s = cached_status(true, &database).expect("enabled → Some");
        assert_eq!(s.latest_version.as_deref(), Some("99.0.0"));
        assert!(s.update_available, "99.0.0 is newer than the running {}", current_version());
        assert_eq!(s.update_command, "cargo install rag-rat --force");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_latest_response_extracts_max_version_fail_open() {
        assert_eq!(
            parse_latest_response(r#"{"crate":{"name":"rag-rat","max_version":"0.6.0"}}"#),
            Some("0.6.0".to_string())
        );
        assert_eq!(parse_latest_response("definitely not json"), None);
        assert_eq!(parse_latest_response(r#"{"crate":{}}"#), None, "missing max_version");
        assert_eq!(parse_latest_response("{}"), None, "missing crate");
        assert_eq!(
            parse_latest_response(r#"{"crate":{"max_version":123}}"#),
            None,
            "non-string max_version is ignored, not coerced"
        );
    }

    #[test]
    fn needs_refresh_on_missing_or_stale_only() {
        let fresh = CachedVersion { latest_version: "0.5.0".into(), checked_at_ms: 1_000 };
        assert!(needs_refresh(None, 0, DEFAULT_TTL_MS), "no cache → refresh");
        assert!(!needs_refresh(Some(&fresh), 1_000 + DEFAULT_TTL_MS - 1, DEFAULT_TTL_MS), "fresh");
        assert!(needs_refresh(Some(&fresh), 1_000 + DEFAULT_TTL_MS, DEFAULT_TTL_MS), "exactly TTL");
        assert!(needs_refresh(Some(&fresh), 1_000 + DEFAULT_TTL_MS * 2, DEFAULT_TTL_MS), "stale");
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("rr-vcheck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".rag-rat")).unwrap();
        let database = dir.join(".rag-rat").join("index.sqlite");
        assert_eq!(read_cache(&database), None, "absent cache reads None");
        write_cache(&database, &CachedVersion {
            latest_version: "0.7.1".into(),
            checked_at_ms: 42,
        });
        assert_eq!(
            read_cache(&database),
            Some(CachedVersion { latest_version: "0.7.1".into(), checked_at_ms: 42 })
        );
        assert_eq!(cache_path(&database), dir.join(".rag-rat").join("version-check.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
