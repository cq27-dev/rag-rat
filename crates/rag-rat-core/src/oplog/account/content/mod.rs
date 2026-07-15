//! Owner-bound `/3` content entries (sync phase C2).

mod acceptance;
mod author;
mod candidate;
mod envelope;
mod storage;

// The in-tx `/3` local-authoring seam (C3.4b-i, #663) — frozen until the live memory path retargets
// onto it (#664).
#[allow(unused_imports, reason = "C3.1 freezes the pure evaluator before C3.2 storage wiring")]
pub(crate) use acceptance::{
    AncestryRelation, CitedFreshness, CitedGrantAuthority, CitedOwnership, CitedRosterAuthority,
    ContentAcceptance, ContentAcceptanceInput, ContentAcceptanceInputError, ContentCondemnReason,
    ContentParkReason, ContentRejectReason, SubjectAuthorityHold, UnknownAncestry,
    evaluate_content_acceptance,
};
#[allow(
    unused_imports,
    reason = "C3.4b-i content-author seam is frozen before its caller lands"
)]
pub(crate) use author::author_content_batch_in_tx;
pub(in crate::oplog) use envelope::{
    ContentEntryHeader, SignedContentEntry, VerifiedContentEntry, decode_content_signed,
    sign_content_entry, verify_content_signed,
};
#[allow(unused_imports, reason = "C2 storage seam is frozen before C3 wiring lands")]
pub(in crate::oplog) use storage::{ContentCapacityScope, ContentIngestOutcome, content_ingest};
pub(super) use storage::{promote_pre_verify_for_account, refold_streams_for_account};
