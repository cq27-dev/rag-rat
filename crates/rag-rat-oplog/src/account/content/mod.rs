//! Owner-bound `/3` content entries (sync phase C2).

mod acceptance;
mod author;
mod candidate;
mod envelope;
mod storage;

// The in-tx `/3` local-authoring seam (C3.4b-i, #663) — frozen until the live memory path retargets
// onto it (#664).
#[allow(unused_imports, reason = "C3.1 freezes the pure evaluator before C3.2 storage wiring")]
pub use acceptance::{
    AncestryRelation, CitedFreshness, CitedGrantAuthority, CitedOwnership, CitedRosterAuthority,
    ContentAcceptance, ContentAcceptanceInput, ContentAcceptanceInputError, ContentCondemnReason,
    ContentParkReason, ContentRejectReason, SubjectAuthorityHold, UnknownAncestry,
    evaluate_content_acceptance,
};
// The in-tx `/3` local-authoring seam + its genesis-detection reader (C3.4b-i, #663): live
// callers in `query::memory` land with #664, so these are plain (un-frozen) re-exports.
pub use author::{author_content_batch_in_tx, content_op_is_authorable, content_stream_is_empty};
pub use envelope::{
    ContentEntryHeader, SignedContentEntry, VerifiedContentEntry, decode_content_signed,
    sign_content_entry, verify_content_signed,
};
#[allow(unused_imports, reason = "C2 storage seam is frozen before C3 wiring lands")]
pub use storage::{
    ContentCapacityScope, ContentIngestOutcome, content_ingest, settle_pending_content_refolds,
};
pub(super) use storage::{promote_pre_verify_for_account, refold_streams_for_account};
