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
// C4.1 content-key crypto primitives (#607). They land ahead of their consumers — C4.3 (`key_wrap`
// authoring) and C5 (content sealing) — so nothing in a non-test build references them yet.
#[allow(dead_code, reason = "C4.1 primitives precede their C4.3/C5 consumers (#607)")]
mod keywrap;
mod limits;
mod ops;
mod registers;
// C4.2b: the account secrets log (`log_id = 1`) — the `StreamKeyWrap` op + owner-gated acceptance
// evaluator, consuming the control fold's authority projection (#607).
mod secrets;
mod storage;

// The in-tx `/2`-ownership ensure seam + the two read-only `/2`-stream resolvers (C3.4b-ii, #676):
// `owned_stream_v2_id` (pure derivation — the live seam's stream resolver) and
// `established_owned_stream_v2` (derivation + effective-ownership fact — the reconcile's fast-path
// probe). #664 wires all three into `query::memory`, so they are plain re-exports.
pub use authoring::{
    ensure_owned_stream_v2_in_tx, established_owned_stream_v2, owned_stream_v2_id,
};
pub use bootstrap::local_account;
#[allow(unused_imports, reason = "C2 contract is frozen before transport wiring lands")]
pub use content::{
    ContentCapacityScope, ContentEntryHeader, ContentIngestOutcome, SignedContentEntry,
    VerifiedContentEntry, content_ingest, decode_content_signed, settle_pending_content_refolds,
    sign_content_entry, verify_content_signed,
};
// The in-tx `/3` content-author seam + its genesis-detection reader (C3.4b-i, #663): #664
// retargets the live memory path onto them, so they are plain re-exports.
pub use content::{author_content_batch_in_tx, content_stream_is_empty};
pub use fold::{
    AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason, AuthorityQuery, GrantAuthority,
    GrantDeviceAuthority, GrantDeviceBoundary, OwnerAuthority, OwnerChainAuthority,
    RosterContentAuthority,
};
pub use id::AccountId;
pub use ops::{DeviceCut, DeviceRole, GrantRole};
pub use storage::{
    CapacityScope, IngestOutcome, account_ingest, auth_len_freshness,
    backfill_authority_projection, grant_effective_for_device, owner_control_authority,
    owner_secrets_authority, roster_content_authority, stream_owner_effective,
};
