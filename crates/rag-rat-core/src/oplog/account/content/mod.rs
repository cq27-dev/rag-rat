//! Owner-bound `/3` content entries (sync phase C2).

mod envelope;
mod storage;

pub(in crate::oplog) use envelope::{
    ContentEntryHeader, SignedContentEntry, VerifiedContentEntry, decode_content_signed,
    sign_content_entry, verify_content_signed,
};
pub(super) use storage::promote_pre_verify_for_account;
#[allow(unused_imports, reason = "C2 storage seam is frozen before C3 wiring lands")]
pub(in crate::oplog) use storage::{ContentCapacityScope, ContentIngestOutcome, content_ingest};
