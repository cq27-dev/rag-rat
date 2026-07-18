//! The account secrets log (`log_id = 1`, sync phase C4.2b): the `StreamKeyWrap` op wire and the
//! owner-gated acceptance evaluator that classifies it.
//!
//! The secrets log is a SECOND roster-only account log on the shared account-entry envelope. Its
//! ops ride as the opaque `payload` bstr; the fold ([`super::fold`]) is CONTROL-only, so secrets
//! entries are CONSUMERS of the authority projection (like `/3` content), classified here by a
//! content-style acceptance loop over the log-generic candidate primitives. C4.2b ships
//! C4.2b shipped `StreamKeyWrap` ACCEPTANCE; C4.3a ([`author`]) adds the owner-side content-key
//! mint + wrap AUTHORING (verify-accepted-or-rollback). The `key_id` adoption cross-check, the
//! derived sealing-key projection, and `sync enable` are C4.3b.

mod acceptance;
mod author;
mod candidate;
mod ops;
mod sealing;
mod security_event;
mod storage;

// The in-tx content-key mint + `StreamKeyWrap` author seam (C4.3a) + the C4.4 lazy rotation entry
// points (`rotate_stream_key_in_tx` / `ensure_stream_key_current_in_tx` / `RotationOutcome`):
// re-exported up through `account` and the crate root for the seal path (C5) to reach (`pub` here
// so `account::mod` can re-export it — the private `mod secrets` keeps it crate-scoped regardless).
pub use author::{
    RotationOutcome, ensure_stream_key_current_in_tx, mint_and_author_stream_key_wrap_in_tx,
    rotate_stream_key_in_tx,
};
// The ingest-time structural validation twin (mirrors the control-plaintext arm) and the
// refold pass wired into `refold_in_tx`.
pub(in crate::account) use ops::validate_storable_secrets_payload;
// The C4.3b READ side: derive-on-read sealing-key selection + the key_id adoption cross-check.
// `current_sealing_key` is what C5's seal path calls; `select_current_sealing_wrap` is also
// the CLI "what key is current" surface. Nothing calls them in C4.3b (machinery ships one
// slice ahead of its consumer).
pub use sealing::{
    SealingKeyOutcome, SelectedWrap, current_sealing_key, select_current_sealing_wrap,
    stream_key_rotation_needed,
};
pub(in crate::account) use storage::refold_secrets_log;
