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
pub mod sync_driver;
pub mod version_check;
pub mod watch;

pub use index::{IndexDatabase, IndexStatus};
pub use output::{OutputFormat, render};

/// Settle and materialize accepted `/3` memory content before dependent `/5` anchor rows are
/// reconciled.
pub fn drain_synced_memory(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    let now = rag_rat_base::time::now_ms();
    rag_rat_oplog::settle_pending_content_refolds(
        conn,
        &rag_rat_oplog::ContentRefoldBudget::unbounded(),
        now,
    )?;
    memory_write::drain_synced_streams_for_all_repos(conn, now)?;
    Ok(())
}

/// Re-resolve synced SYMBOL distill anchors against the active repo's local index — call at a
/// table-sync (`/5`) settle point so a replicated anchor surfaces as drive-by in the SAME session
/// it arrived, without waiting for the next index open. A device never receives an anchor's
/// device-local `logical_symbol_id`/`resolved` (they are `local_columns`); this derives them from
/// the anchor's portable `(name, file_path)`. Idempotent — a no-op when nothing resolved anew.
pub fn resolve_synced_distill_anchors(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    index::resolve_synced_distill_anchors(conn)
}

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
