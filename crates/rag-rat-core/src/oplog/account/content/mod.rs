//! Owner-bound `/3` content entries (sync phase C2).

mod envelope;

pub(in crate::oplog) use envelope::{
    ContentEntryHeader, SignedContentEntry, VerifiedContentEntry, decode_content_signed,
    sign_content_entry, verify_content_signed,
};
