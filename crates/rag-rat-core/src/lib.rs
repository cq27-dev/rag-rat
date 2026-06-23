pub mod config;
pub mod dream;
#[cfg(feature = "eval")]
pub mod eval;
pub mod fleet;
pub mod index;
pub mod language;
pub mod locks;
pub mod output;
pub mod query;
pub mod search;
pub mod serde_big_id;
pub mod storage;
pub mod version_check;
pub mod watch;

pub use config::{Config, ResolvedTarget, TargetKind, WatchConfig};
pub use index::{IndexDatabase, IndexStatus};
pub use output::{OutputFormat, render};

/// Lightweight, lock-free count of active repo memories whose anchor is `gone`/`stale` — the same
/// population `memory_doctor` lists. Opens a BARE read-only connection (no git resolution, scope
/// view, or schema migration), so it is cheap enough to call on every MCP tool result to nudge the
/// agent to re-anchor drifted memories. Returns 0 on any error (missing / locked / older DB) so the
/// nudge simply doesn't show — never blocks or fails a tool call.
pub fn memory_attention_count(database: &std::path::Path) -> u64 {
    storage::IndexConnection::open_read_only(database)
        .ok()
        .and_then(|conn| query::memory::doctor_attention_count(conn.connection()).ok())
        .unwrap_or(0)
}
