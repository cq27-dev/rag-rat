//! The AUTHORED memory mutations: create / update / obsolete / rebind, typed-edge add/remove, and
//! the op-log authoring seam they call in-transaction. This is the write half of the repo-memory
//! surface — the read half (types, hydration, search, validation, relocation) lives in
//! `rag_rat_query::memory`. It stays in the engine beside `oplog` because every mutation here
//! authors an op-log entry in the same transaction; it moves out together with `oplog` when that
//! crate extracts.

mod api;
// The op-log authoring seam: row→op translation, the one-time full backfill, and the live
// `author_*` helpers the memory mutations call in-transaction (#532).
mod authoring;
mod edges;
#[cfg(test)]
mod oracle_relocation_tests;

pub(crate) use api::{create_memory, mark_obsolete, rebind_memory, update_memory};
// The scope-READING reconcile entry (#541): reconciles the active repo's owner stream, reading
// the repo id from the connection scope and no-oping under an absent/unstable scope.
// Re-exported so the index reconcile path (the idle-repo ghost backstop, #583) can name it
// across the private module.
pub(crate) use authoring::backfill_memory_oplog;
// The scope-explicit reconcile entry (#541): `authoring` is a PRIVATE module, so
// `index::consolidate` names this through this re-export (Task 5 of #541).
pub(crate) use authoring::reconcile_owner_stream_for_repo;
pub(crate) use edges::{add_edge, remove_edge};

/// #767 review: fail a repo-scoped memory mutation CLOSED when the active repo was removed by
/// `rag-rat rm`. The removal tombstone is normally enforced at connection-registration time, but
/// an MCP connection that opened (and resolved its active repo scope) BEFORE `rm` acquired the
/// repo lock keeps that stale scope; without this re-check its `create_memory` / `add_edge` would
/// INSERT fresh `repo_memories` / `repo_node_edges` rows stamped with the removed `repo_id` (and
/// author op-log state) AFTER `rm`'s purge committed and reported success. Call INSIDE the write
/// transaction, immediately before the INSERT, so the tombstone read shares the transaction's
/// snapshot: an `rm` that committed first is seen (fail closed); an `rm` that commits after purges
/// the just-written row itself (also consistent). The reverse-order hazard is the one this closes.
pub(crate) fn assert_repo_not_removed(
    conn: &rusqlite::Connection,
    repo_id: &str,
) -> anyhow::Result<()> {
    if rag_rat_db::schema::is_repo_removed(conn, repo_id)? {
        anyhow::bail!(
            "repo {repo_id} was removed with `rag-rat rm` — refusing the write; run `rag-rat \
             init` in the repo to re-add it"
        );
    }
    Ok(())
}
