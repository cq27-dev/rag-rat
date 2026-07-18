pub mod distill;
#[cfg(feature = "eval")]
pub mod eval;
pub mod fleet;
pub mod index;
pub(crate) mod memory_write;
pub mod output;
pub mod query;
pub mod search;
pub mod sidecar_state;
pub mod version_check;
pub mod watch;

pub use index::{IndexDatabase, IndexStatus};
pub use output::{OutputFormat, render};

/// Lightweight, lock-free count of active repo memories whose anchor is `gone`/`stale` — the same
/// population `memory_doctor` lists. Opens a BARE read-only connection (no git resolution, scope
/// view, or schema migration), so it is cheap enough to call on every MCP tool result to nudge the
/// agent to re-anchor drifted memories. Returns 0 on any error (missing / locked / older DB) so the
/// nudge simply doesn't show — never blocks or fails a tool call.
pub fn memory_attention_count(database: &std::path::Path) -> u64 {
    rag_rat_db::storage::IndexConnection::open_read_only(database)
        .ok()
        .and_then(|conn| rag_rat_query::memory::doctor_attention_count(conn.connection()).ok())
        .unwrap_or(0)
}
