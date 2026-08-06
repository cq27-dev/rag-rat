//! Memory op-log: the op model, a deterministic projection fold, and the signed hash-chained entry
//! envelope (phase B, layer 1).
//!
//! A pure, in-memory primitive frozen in isolation — the op-log's ordering + integrity semantics
//! without any transport or storage. Parts:
//! - [`op`]: the frozen [`op::MemoryOp`] set, its canonical CBOR wire form
//!   ([`op::encode`]/[`op::decode`]), and the known/unknown split ([`op::DecodedOp`]) that keeps a
//!   forward-version op opaque-but-retained.
//! - [`project`]: the LWW [`project::project`] fold from a Lamport/device-ordered `[op::Entry]` to
//!   a converged [`project::ProjectedState`].
//! - [`device`]: the ed25519 device identity — [`device::DeviceSecret`] signs,
//!   [`device::DevicePublic`] verifies (`verify_strict`) and derives the op model's opaque
//!   `DeviceFingerprint` = `sha256(pubkey)`.
//! - [`entry`]: the signed, hash-chained entry envelope over an OPAQUE op — sign / verify /
//!   per-`(stream, device)` chain-verify. The signature covers the canonical body `[domain,
//!   stream_id, prev_hash, lamport, device_fingerprint, op_bytes]`; `entry_hash = sha256(body)`
//!   links the chain, so an UNKNOWN op still verifies + chains.
//! - [`cbor`]: the shared canonical-CBOR discipline (definite lengths, minimal headers, sorted map
//!   keys, no trailing) both wire layers enforce.
//!
//! - [`store`]: the durable SQLite seam — the layer-1 opaque signed entry log (verified,
//!   chain-continuous, idempotent [`store::append`]) plus the layer-2 full-replay projection into
//!   oplog-owned shadow tables, kept in sync atomically.
//! - [`stream`]: the immutable, content-derived stream identity (#509) — one signed chain,
//!   watermark, and projection exists PER [`stream::StreamId`]; the entry body binds it, so stream
//!   membership is signature-protected.
//! - [`identity`]: the store's ONE persisted ed25519 device identity (#513) —
//!   [`identity::local_device`] mints it from OS entropy on first use and returns it stably
//!   thereafter, so every authored entry signs under one machine fingerprint.
//!
//! Nothing here is wired into the live write path yet (later increments add the append-on-mutation
//! seam, roster/epochs, and transport) — this mirrors the `content_hash` freeze: pin the semantic
//! primitive first, in isolation-testable form.

// The authoring half is wired into the memory write path (#532), but the SYNC-TRANSPORT half —
// `append` (receiving a foreign signed entry), the fork quarantine, `AppendOutcome`, and the C4/C5
// authority primitives that precede their consumers (#607) — is still unconsumed frozen
// scaffolding, so the crate keeps `allow(dead_code)` (carried over from the module's own
// suppression before the #706-phase-8 extraction).
#![allow(dead_code)]

/// Migration hooks for oplog's own tests. The only migration hook oplog's schema uses is its OWN
/// `backfill_authority_projection` (the all-account refold for V064/V065/V099); the other domains'
/// hooks are irrelevant here, so they stay noop. Wiring the crate's own function means the tests
/// need no dev-dependency on rag-rat-core's `migration_hooks()`.
#[cfg(test)]
pub(crate) fn test_hooks() -> rag_rat_db::MigrationHooks {
    rag_rat_db::MigrationHooks {
        backfill_authority_projection: account::backfill_authority_projection,
        ..rag_rat_db::MigrationHooks::noop()
    }
}

mod account;
mod cbor;
mod content_projection;
mod device;
mod entry;
mod identity;
mod op;
mod project;
mod store;
mod stream;
mod table_sync;
// C1's curated authority seam for the C2 `/3` content envelope and candidate DAG. The account
// implementation stays private; only typed ingest results and snapshot-consistent point
// queries cross the phase boundary.
//
// The C3.4 local-authoring surface `query::memory` reaches for once #664 retargets the live
// memory path onto the owner-bound `/2`//3 substrate:
// - `local_account` (C3.4a, #662): the store-global principal owner-bound `/3` content authors
//   under.
// - `ensure_owned_stream_v2_in_tx` (C3.4b-ii, #676): publish + resolve a repo's owned `/2` stream.
// - `owned_stream_v2_id` / `established_owned_stream_v2` (C3.4b-ii, #676): the pure stream resolver
//   and the effective-ownership fast-path probe.
// - `author_content_batch_in_tx` (C3.4b-i, #663): author a batch of ops as owner-authored `/3`
//   content, verify-accepted in the caller's txn.
// - `content_stream_is_empty` (C3.4b-i, #663): the `/3` genesis-detection reader.
// - `mint_and_author_stream_key_wrap_in_tx` (C4.3a, #607): mint a per-stream content key and author
//   an owner-gated `StreamKeyWrap` sealing it to every effective device, verify-accepted in the
//   caller's txn.
// - `rotate_stream_key_in_tx` / `ensure_stream_key_current_in_tx` / `stream_key_rotation_needed`
//   (C4.4, #607): lazy content-key rotation on device removal — re-seal a fresh higher-epoch key to
//   the remaining roster when a removed device still holds the current key.
// `prepare_content_authoring` / `author_prepared_content_batch_in_tx` / `SealPolicy` (C5,
// #608): policy-aware `/3` authoring prepared before, then executed inside, a caller-owned
// transaction; `author_content_batch` is the self-transacting convenience composition. The
// envelope-layer seal primitives (`sign_sealed_content_entry` / `seal_and_sign_content_entry`)
// take a `&DeviceSecret`, so — like `sign_content_entry` — they stay account-crate-internal.
// Keep this crate-root API explicit: adding an account implementation seam must not silently
// make it a public semver commitment.
/// Sealing a discovery announcement to the account's roster-effective devices. Both halves
/// live in the op-log crate because neither the device X25519 secret nor the roster's public
/// keys may leave it.
pub use account::discovery;
pub use account::{
    AccountId, AuthoredDurability, AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason,
    AuthorityQuery, CapacityScope, CatchUpReport, ContentCapacityScope, ContentEntryHeader,
    ContentIngestOutcome, ContentKey, ContentKeyring, ContentRefoldBudget, ContentSettleReport,
    ContentStreamSettleFailure, DeviceCut, DeviceRole, ENROLLMENT_HELD_ENTRY_HASHES_MAX,
    EnrollingDevice, EnrollmentBootstrap, EnrollmentBudget, GrantAuthority, GrantDeviceAuthority,
    GrantDeviceBoundary, GrantRole, IngestOutcome, KeyId, LiveKeyEpoch, LiveKeyTargets,
    NodeAuthError, OwnerAuthority, OwnerChainAuthority, PreparedContentAuthoring, RepoIncarnation,
    RepoIncarnationState, RosterContentAuthority, RotationOutcome, SealPolicy, SealingKeyOutcome,
    SelectedWrap, SignedContentEntry, SnapshotAuthorOutcome, SyncAccountEntry, SyncContentEntry,
    VerifiedContentEntry, account_effective_count, account_entries_for_enrollment,
    account_entries_for_sync, account_entry_ref, account_ingest, account_signed_entry_exists,
    account_signed_hash, adopt_enrollment_bootstrap, adopt_local_account,
    advance_repo_incarnation_in_tx, auth_len_freshness, author_content_batch,
    author_content_batch_in_tx, author_device_add_in_tx, author_enrollment_device_add_in_tx,
    author_prepared_content_batch_in_tx, author_snapshot_in_tx, backfill_authority_projection,
    catch_up_stream_keys_for_device_in_tx, content_entries_for_public_sync,
    content_entries_for_sync, content_entry_ref, content_ingest, content_op_is_authorable,
    content_op_is_sealed_authorable, content_signed_entry_exists, content_signed_hash,
    content_stream_has_pending_refold, content_stream_has_sealed_ratchet, content_stream_is_empty,
    current_sealing_key, decode_content_signed, enroll_stream_keys_for_device_in_tx,
    enrollment_authoring_fits, enrollment_authoring_requirements, enrollment_budget,
    ensure_owned_stream_v2_in_tx, ensure_owned_stream_v2_with_mode_in_tx, ensure_repo_incarnation,
    ensure_stream_key_current_in_tx, established_owned_stream_v2, grant_effective_for_device,
    held_account_entry_hashes, historical_content_keyring, live_stream_key_targets_for_device,
    local_account, mint_and_author_stream_key_wrap_in_tx, owned_stream_v2_id,
    owned_streams_for_account, owner_control_authority, owner_control_authority_in_snapshot,
    owner_secrets_authority, prepare_content_authoring, prune_account_candidate_reservations_in_tx,
    read_local_account, read_local_account_genesis, release_account_candidate_reservation_in_tx,
    repo_incarnation_state, retry_enrollment_pre_verify, roster_content_authority,
    rotate_stream_key_in_tx, select_current_sealing_wrap,
    settle_pending_content_refold_for_stream_in_tx, settle_pending_content_refolds,
    sign_local_node_binding, stream_access_mode, stream_key_rotation_needed, stream_owner_account,
    stream_owner_effective, upsert_account_candidate_reservation_in_tx, validate_device_add_label,
    verify_enrollment_device_add, verify_node_binding,
};
// The `/3` content projection's store-global upgrade re-fold (#688): wired into the index
// open/migrate seam by rag-rat-core, so a stale store is rebuilt (every stream, then the one
// stamp) before any per-stream write.
pub use content_projection::rebuild_all_content_projections_if_stale;
// The projection READ seam (#691 A1): decode a `/2` stream's projected nodes/edges back into
// the op model. The memory drain reads these to mirror accepted synced content into
// `repo_memories` / `repo_node_edges` as `origin='synced'` rows — the reverse of the local
// reconcile. Read-only over the projection tables, keeping the private row DTOs inside this
// crate.
pub use content_projection::{
    ProjectedContentEdge, ProjectedContentNode, list_projected_content_edges,
    list_projected_content_nodes,
};
// The drain-gate seam (#902): a per-stream projection epoch + last-drained watermark (both in
// `oplog_meta`) so the memory drain skips its O(projection) scan when nothing changed since it
// last ran — the store-global watcher pass would otherwise re-scan every stream every pass.
pub use content_projection::{
    content_drain_needed, content_projection_epoch, record_content_drained,
};
// The op-log's first crate-internal API surface (#524): the MINTING primitives + the op
// vocabulary the memory subsystem needs to author + backfill entries. Every submodule above is
// otherwise private, so this curated re-export is the ONE seam `query::memory` reaches through
// — and the only direction of the dependency (`oplog` never depends back on `query::memory`).
pub use identity::{LocalDevice, load_local_device, local_device};
pub use op::{
    DeviceFingerprint, EdgeKey, EdgeSpec, MemoryOp, NodeContent, NodeId, NodeStatus,
    ParseDeviceFingerprintError, ResolvedAnchor,
};
// The `/1` shadow-projection read seams (`ProjectedState` / `load_projection`) and the
// standalone (own-txn) `/1` authoring wrappers (`author_batch` / `author_op`) — test-only
// scaffolding for the retained `/1` store. The live memory path now authors owner-bound `/3`
// content (#664), so these `/1` seams have no non-test caller; the re-exports stay `allow`'d
// rather than removed because the `/1` store itself is retained (its history is not migrated,
// per J1).
pub use project::ProjectedState;
pub use store::{author_batch, author_op, load_projection};
pub use stream::{AccessMode, StreamId};
// The table-sync forward-compat seam (#1001): replay entries retained but not projected when
// they arrived. Belongs at store open, before producing — see the module docs.
pub use table_sync::{
    TABLE_SYNC_ENTRY_MAX_BYTES, TableSyncChainEntry, TableSyncChainHead, TableSyncEntryStart,
    TableSyncFrontier, TableSyncIngestOutcome, TableSyncStream,
    refold_stale_table_sync_projections, scope_retention_budget, table_sync_author_pending,
    table_sync_chain_entries, table_sync_chain_frontier, table_sync_chain_page_after,
    table_sync_compact_overdue, table_sync_ingest, table_sync_supported_streams,
    table_sync_validate_stream,
};
