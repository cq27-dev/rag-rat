//! The account secrets log (`log_id = 1`, sync phase C4.2b): the `StreamKeyWrap` op wire and the
//! owner-gated acceptance evaluator that classifies it.
//!
//! The secrets log is a SECOND roster-only account log on the shared account-entry envelope. Its
//! ops ride as the opaque `payload` bstr; the fold ([`super::fold`]) is CONTROL-only, so secrets
//! entries are CONSUMERS of the authority projection (like `/3` content), classified here by a
//! content-style acceptance loop over the log-generic candidate primitives. C4.2b ships
//! `StreamKeyWrap` ACCEPTANCE only — content-key minting + wrap AUTHORING + the `key_id` adoption
//! cross-check are C4.3.

mod acceptance;
mod candidate;
mod ops;
mod storage;

// The ingest-time structural validation twin (mirrors the control-plaintext arm) and the refold
// pass wired into `refold_in_tx`.
pub(in crate::account) use ops::validate_storable_secrets_payload;
pub(in crate::account) use storage::refold_secrets_log;
