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
mod candidate;
mod cut;
mod envelope;
mod fold;
mod id;
mod limits;
mod ops;
mod registers;
mod storage;

pub(crate) use fold::{
    AuthorityBoundary, AuthorityInvalidReason, AuthorityParkReason, AuthorityQuery, GrantAuthority,
    GrantDeviceAuthority, GrantDeviceBoundary, OwnerAuthority, OwnerChainAuthority,
    RosterContentAuthority,
};
pub(crate) use id::AccountId;
pub(crate) use ops::{DeviceCut, DeviceRole, GrantRole};
pub(crate) use storage::{
    CapacityScope, IngestOutcome, account_ingest, backfill_authority_projection,
    grant_effective_for_device, owner_control_authority, owner_secrets_authority,
    roster_content_authority, stream_owner_effective,
};
