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
mod limits;
mod ops;
mod registers;
mod storage;

// The in-tx `/2`-ownership ensure seam (C3.4b-ii, #676) — the idempotent
// ensure-the-repo's-owner-stream-is-owned primitive #664 calls before authoring `/3` content.
// Frozen until that caller lands.
#[allow(
    unused_imports,
    reason = "C3.4b-ii /2-ownership ensure seam is frozen before its caller lands"
)]
pub(crate) use authoring::ensure_owned_stream_v2_in_tx;
pub(crate) use bootstrap::local_account;
// The in-tx `/3` content-author seam (C3.4b-i, #663) — frozen until #664 retargets the live
// path.
#[allow(
    unused_imports,
    reason = "C3.4b-i content-author seam is frozen before its caller lands"
)]
pub(crate) use content::author_content_batch_in_tx;
#[allow(unused_imports, reason = "C2 contract is frozen before transport wiring lands")]
pub(in crate::oplog) use content::{
    ContentCapacityScope, ContentEntryHeader, ContentIngestOutcome, SignedContentEntry,
    VerifiedContentEntry, content_ingest, decode_content_signed, sign_content_entry,
    verify_content_signed,
};
pub(crate) use fold::{
    AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason, AuthorityQuery, GrantAuthority,
    GrantDeviceAuthority, GrantDeviceBoundary, OwnerAuthority, OwnerChainAuthority,
    RosterContentAuthority,
};
pub(crate) use id::AccountId;
pub(crate) use ops::{DeviceCut, DeviceRole, GrantRole};
pub(crate) use storage::{
    CapacityScope, IngestOutcome, account_ingest, auth_len_freshness,
    backfill_authority_projection, grant_effective_for_device, owner_control_authority,
    owner_secrets_authority, roster_content_authority, stream_owner_effective,
};
