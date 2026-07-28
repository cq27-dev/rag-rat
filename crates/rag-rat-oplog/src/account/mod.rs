//! The account identity/crypto layer (sync phase C) — self-sovereign device rosters, roles,
//! cross-account grants, revocation, and the depth-stratified control fold, layered ABOVE the
//! phase-B op-log (layer 1) and reusing its canonical-CBOR discipline ([`super::cbor`]), device
//! keys ([`super::device`]), and [`super::stream::StreamId`].
//!
//! An **account** is a principal (a person or org): a self-sovereign device roster managed only by
//! its own hash-chained, signed logs. Account entries are a SEPARATE signed wire layer
//! (`rag-rat/account-entry/1`) that never touches [`super::store::append`] — they get their own
//! candidate-DAG storage and a pure `fold_account` that derives ALL authority from
//! content-addressed citations (a cited grant / owner-incarnation / cut), never from ordering. See
//! the frozen design note `sync-phase-c-design.md` (v5) for the whole-phase contract; C1 builds the
//! spine.
//!
//! C1 modules (this slice):
//! - [`id`]: [`AccountId`] + the §4 genesis commitment.
//! - [`limits`]: §18a protocol-validity constants + the account-layer domain strings.
//! - [`envelope`]: the 13-part signed account-entry envelope (§6).
mod authoring;
mod bootstrap;
mod candidate;
mod content;
mod cut;
mod envelope;
mod fold;
mod id;
// C4.1 content-key crypto primitives (#607). C4.3a (`secrets::author`) consumes the seal/unwrap +
// key-id path; the remaining primitives (deterministic `from_seed` / seed-injected seal) are still
// test-only or await C5 content sealing, so the module keeps the dead-code allowance.
#[allow(dead_code, reason = "some C4.1 primitives still precede their C5 consumers (#607)")]
mod keywrap;
mod limits;
mod node_binding;
mod ops;
mod registers;
// C4.2b: the account secrets log (`log_id = 1`) — the `StreamKeyWrap` op + owner-gated acceptance
// evaluator, consuming the control fold's authority projection (#607).
mod secrets;
mod snapshot;
mod storage;

// The in-tx `/2`-ownership ensure seam + the two read-only `/2`-stream resolvers (C3.4b-ii, #676):
// `owned_stream_v2_id` (pure derivation — the live seam's stream resolver) and
// `established_owned_stream_v2` (derivation + effective-ownership fact — the reconcile's fast-path
// probe). #664 wires all three into `query::memory`, so they are plain re-exports.
pub use authoring::{
    EnrollingDevice, author_device_add_in_tx, author_enrollment_device_add_in_tx,
    enrollment_authoring_fits, enrollment_authoring_requirements, ensure_owned_stream_v2_in_tx,
    established_owned_stream_v2, owned_stream_v2_id, retry_enrollment_pre_verify,
    validate_device_add_label,
};
pub use bootstrap::{
    AuthoredDurability, ENROLLMENT_HELD_ENTRY_HASHES_MAX, EnrollmentBootstrap, EnrollmentBudget,
    adopt_enrollment_bootstrap, adopt_local_account, enrollment_budget, held_account_entry_hashes,
    local_account, prune_account_candidate_reservations_in_tx, read_local_account,
    release_account_candidate_reservation_in_tx, upsert_account_candidate_reservation_in_tx,
};
#[allow(unused_imports, reason = "C2 contract is frozen before transport wiring lands")]
pub use content::{
    ContentCapacityScope, ContentEntryHeader, ContentIngestOutcome, ContentRefoldBudget,
    ContentSettleReport, ContentStreamSettleFailure, SignedContentEntry, VerifiedContentEntry,
    content_ingest, content_stream_has_pending_refold, decode_content_signed,
    settle_pending_content_refold_for_stream_in_tx, settle_pending_content_refolds,
};
// The envelope sign/verify primitives take `&DeviceSecret`/`&DevicePublic` (`pub(crate)`
// types), so they stay off the crate-root glob — account-internal consumers reach them through
// `content`.

// The C5a sealed-authoring surface (#608): the envelope-layer seal
// (`sign_sealed_content_entry` + its OS-nonce wrapper `seal_and_sign_content_entry`), the
// policy-aware prepared authoring surface, sealed-op size predicate, and downgrade-ratchet
// reader.
pub use content::{
    PreparedContentAuthoring, SealPolicy, author_content_batch,
    author_prepared_content_batch_in_tx, content_op_is_sealed_authorable,
    content_stream_has_sealed_ratchet, prepare_content_authoring,
};
// The phase-D (#406) content sync read seams, consumed by the transport crate — plain
// re-exports.
pub use content::{
    SyncContentEntry, content_entries_for_sync, content_entry_ref, content_signed_entry_exists,
    content_signed_hash,
};
// The in-tx `/3` content-author seam + its genesis-detection reader (C3.4b-i, #663): #664
// retargets the live memory path onto them, so they are plain re-exports.
pub use content::{author_content_batch_in_tx, content_op_is_authorable, content_stream_is_empty};
// The V070 projection-table guard, reused by the memory-layer content projection's upgrade
// re-fold (#688).
pub(crate) use content::{content_projected_tables_exist, open_sealed_payload};
#[allow(unused_imports, reason = "envelope tests consume these crate-internal signing seams")]
pub(in crate::account) use content::{seal_and_sign_content_entry, sign_sealed_content_entry};
pub use fold::{
    AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason, AuthorityQuery, GrantAuthority,
    GrantDeviceAuthority, GrantDeviceBoundary, OwnerAuthority, OwnerChainAuthority,
    RosterContentAuthority,
};
pub use id::AccountId;
// C4.1 content-key primitives the C4.3b sealing surface exposes: `ContentKey` is the `Ready`
// payload C5's seal path consumes; `KeyId` is the selection identity (#607).
pub use keywrap::{ContentKey, KeyId};
// The phase-D (#881) node-authorization seam: mint + verify a signed transport-node ↔
// account-device binding. Consumed by the transport crate's auth handshake.
pub use node_binding::{NodeAuthError, sign_local_node_binding, verify_node_binding};
pub use ops::{DeviceCut, DeviceRole, GrantRole};
// The in-tx content-key mint + owner-gated `StreamKeyWrap` author seam (C4.3a), the C4.3b READ
// side (derive-on-read sealing-key selection + the key_id adoption cross-check), and the C4.4
// lazy rotation-on-removal entry points (#607).
pub use secrets::{
    CatchUpReport, ContentKeyring, LiveKeyEpoch, LiveKeyTargets, RotationOutcome,
    SealingKeyOutcome, SelectedWrap, catch_up_stream_keys_for_device_in_tx, current_sealing_key,
    enroll_stream_keys_for_device_in_tx, ensure_stream_key_current_in_tx,
    historical_content_keyring, live_stream_key_targets_for_device,
    mint_and_author_stream_key_wrap_in_tx, rotate_stream_key_in_tx, select_current_sealing_wrap,
    stream_key_rotation_needed,
};
// The C6 snapshot-authoring seam (#609). No caller fires it yet, and that is deliberate rather
// than an oversight: a snapshot is only worth minting once something reads one. #406 owns both
// halves — it is what consumes manifests ("window pruning only against verified snapshot
// manifests") and it owns the maintenance path device-side sync piggybacks. Authoring on a
// cadence before then would write entries nothing reads into a capacity-bounded candidate
// store that cannot yet prune them, since the tombstone horizon is still outstanding on #609.
#[allow(unused_imports, reason = "C6 authoring seam is frozen before transport wiring lands")]
pub use snapshot::author::{SnapshotAuthorOutcome, author_snapshot_in_tx};
pub(crate) use storage::stream_owner_account;
pub use storage::{
    CapacityScope, IngestOutcome, SyncAccountEntry, account_effective_count,
    account_entries_for_enrollment, account_entries_for_sync, account_entry_ref, account_ingest,
    account_signed_entry_exists, account_signed_hash, auth_len_freshness,
    backfill_authority_projection, grant_effective_for_device, owned_streams_for_account,
    owner_control_authority, owner_control_authority_in_snapshot, owner_secrets_authority,
    roster_content_authority, stream_owner_effective, verify_enrollment_device_add,
};
