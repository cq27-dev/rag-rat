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

// The `rag-rat rm` removal-tombstone guard (#767 review) the memory mutations call inside
// their write transactions, immediately before the INSERT — defined beside the removal
// orchestration in `index::remove` (the dream + heal writers gate on it too), re-exported here
// so the `super::` call sites read unchanged.
pub(crate) use crate::index::remove::assert_repo_not_removed;
