//! The AUTHORED memory mutations: create / update / obsolete / rebind, typed-edge add/remove, and
//! the op-log authoring seam they call in-transaction. This is the write half of the repo-memory
//! surface — the read half (types, hydration, search, validation, relocation) lives in
//! `rag_rat_query::memory`. It stays in the engine beside `oplog` because every mutation here
//! authors an op-log entry in the same transaction; it moves out together with `oplog` when that
//! crate extracts.

mod api;
// The live op-log authoring seam: the `author_*` helpers the memory mutations call
// in-transaction, their preparation, and the whole-op authorability guard (#532).
mod authoring;
#[cfg(test)]
mod contributor_authoring_tests;
// The REVERSE of the authoring reconcile (#691 A1): mirror a stream's accepted synced `/3` content
// back into `repo_memories` / `repo_node_edges` as `origin='synced'` rows.
mod drain;
mod edges;
// Sharing controls for the owner stream: sealed/public enable, device-key catch-up, writer grants.
mod grants;
#[cfg(test)]
mod oracle_relocation_tests;
// Which account's stream the repo authors onto: access mode, seal policy, contribution and
// subscription owners, and the stream pin.
mod ownership;
// The per-node/edge reconcile that keeps the op log a complete signed mirror of the memory tables,
// including the full backfill the mutations run before their first live entry (#541).
mod reconcile;

pub(crate) use api::{create_memory, mark_obsolete, rebind_memory, update_memory};
// The drain's in-place refresh of a held binding, driven against a real index by the
// relocation tests.
#[cfg(test)]
pub(crate) use authoring::anchor_publication_ops;
#[cfg(test)]
pub(crate) use drain::refresh_binding;
// The synced-content drain entries (#691 A1): the per-repo drain (consolidate) and the
// store-global drain (open/migrate) that materialize accepted synced `/3` content into the
// local memory tables.
pub(crate) use drain::{drain_synced_stream_for_repo, drain_synced_streams_for_all_repos};
pub(crate) use edges::{add_edge, remove_edge};
pub(crate) use grants::{
    RepoGrantListing, RepoRevokeReport, catch_up_enrolled_device_keys, enable_public_authoring,
    enable_sealed_authoring, grant_repo_writer, list_repo_grants, published_grant_target,
    revoke_repo_writer,
};
pub(crate) use ownership::{
    RepoOwnerConfig, SubscribeTrust, SubscriptionRouting, clear_contribution_owner,
    clear_subscription_owner, contribution_targets, ensure_not_mirroring_another_account,
    repo_owner_config, set_contribution_owner, set_subscription_owner, stream_pin,
    subscription_owners, subscription_routing,
};
// The scope-READING reconcile entry (#541) as index MAINTENANCE runs it: reconciles the active
// repo's owner stream, reading the repo id from the connection scope, no-oping under an
// absent/unstable scope, and skipping the stream-establishment refusal that must not fail a
// maintenance pass. Re-exported so the index reconcile path (the idle-repo ghost backstop,
// #583) can name it across the private module.
pub(crate) use reconcile::heal_memory_oplog_ghosts;
// The scope-explicit reconcile entry (#541): `reconcile` is a PRIVATE module, so
// `index::consolidate` names this through this re-export.
pub(crate) use reconcile::reconcile_owner_stream_for_repo;

// The `rag-rat rm` removal-tombstone guard (#767 review) the memory mutations call inside
// their write transactions, immediately before the INSERT — defined beside the removal
// orchestration in `index::remove` (the dream + heal writers gate on it too), re-exported here
// so the `super::` call sites read unchanged.
pub(crate) use crate::index::remove::assert_repo_not_removed;
