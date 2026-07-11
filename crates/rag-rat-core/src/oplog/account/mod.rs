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
//!
//! **Build-time lint posture.** The account subsystem is built bottom-up over the C1 slices: a
//! lower layer (envelope, ops, cut, registers) is consumed only by a higher one (fold, storage) or
//! by a later phase (C2–C5), so a freshly-landed layer reads as `dead_code` until its consumer
//! arrives. This blanket allow spans the build; it is REMOVED at Phase 6 (surface wire-up), where
//! the final `-D warnings` clippy gate + the reviewer catch anything genuinely unreachable, and the
//! handful of truly C2–C5-deferred seams get precise per-item allows.
#![allow(dead_code)]

mod cut;
mod envelope;
mod id;
mod limits;
mod ops;

pub(crate) use id::AccountId;
