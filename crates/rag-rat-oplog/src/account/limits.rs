//! Frozen wire constants for the account layer: the domain tags and the §18a protocol-VALIDITY
//! limits.
//!
//! Two flavours of constant live here, and the distinction is load-bearing:
//! - **Domain tags** version the wire. An old binary rejects a bumped tag rather than misreading
//!   it.
//! - **§18a validity limits** are frozen protocol constants: a violation is a STRUCTURAL REJECT in
//!   *every* implementation, and changing one is a deliberate wire bump — NOT a tunable. (The
//!   local, tunable quotas of §18b — park budgets, fold caps — are implementation-local and live
//!   with the code that enforces them, never here.)
//!
//! CBOR nesting depth reuses [`super::super::cbor::MAX_CBOR_DEPTH`] (= 32), the same floor the
//! phase-B op wire enforces — an account object is not allowed to nest deeper than an op entry.

/// Domain tag for the account-entry BODY header (the bytes the signature and `entry_hash` cover).
pub(super) const ACCOUNT_ENTRY_DOMAIN: &str = "rag-rat/account-entry/1";
/// Domain tag for the outer signed account-entry transport envelope (body + signature).
pub(super) const ACCOUNT_SIGNED_DOMAIN: &str = "rag-rat/account-signed/1";
/// Domain tag for the `/3` content-entry header (§8).
pub(super) const CONTENT_ENTRY_DOMAIN: &str = "rag-rat/entry/3";
/// Domain tag shared by signed content-entry transport envelopes.
pub(super) const CONTENT_SIGNED_DOMAIN: &str = "rag-rat/signed-entry/1";
/// Domain tag committed into the `account_id` genesis hash (§4).
pub(super) const ACCOUNT_ID_DOMAIN: &str = "rag-rat/account/1";

/// §18a — max encoded size of an account-log entry wire. A larger envelope is a structural reject.
pub(super) const ACCOUNT_ENVELOPE_MAX_BYTES: usize = 64 * 1024;
/// §18a — max encoded size of a `/3` content entry wire. Consumed by C2 (the `/3` envelope); pinned
/// here now so the whole §18a set is frozen together at C1.
pub(super) const CONTENT_ENVELOPE_MAX_BYTES: usize = 256 * 1024;
/// §18a — max `device_cuts` entries in a `DeviceRemove` / `StreamRevoke` payload. Consumed by the
/// control-op decoders (Phase 3, `ops.rs`).
pub(super) const DEVICE_CUTS_MAX: usize = 256;
/// §18a — max `content_cuts` entries in a `DeviceRemove` payload. Consumed by the control-op
/// decoders (Phase 3, `ops.rs`).
pub(super) const CONTENT_CUTS_MAX: usize = 1024;
/// §18a — max wrap recipients in a `StreamKeyWrap`. Consumed by C4 (key wraps); pinned here now so
/// the whole §18a set is frozen together at C1.
pub(super) const WRAP_RECIPIENTS_MAX: usize = 1024;
/// §18a — max coverage targets in a snapshot manifest. Consumed by C6. A decoder bound has to be a
/// VALIDITY limit, not a local quota: two implementations that bound the same array differently
/// would accept different entries on the same signed log.
pub(super) const SNAPSHOT_TARGETS_MAX: usize = 256;
/// §18a — max covered watermarks within ONE snapshot target. Bounded by roster size, so it matches
/// [`WRAP_RECIPIENTS_MAX`]; the 64 KiB envelope is the binding constraint in practice.
pub(super) const SNAPSHOT_COVERED_MAX: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_tags_are_frozen() {
        // A change to any tag is a wire-version bump — an old binary must reject the new tag, not
        // misread it. These strings are the security boundary; pin them exactly.
        assert_eq!(ACCOUNT_ENTRY_DOMAIN, "rag-rat/account-entry/1");
        assert_eq!(ACCOUNT_SIGNED_DOMAIN, "rag-rat/account-signed/1");
        assert_eq!(CONTENT_ENTRY_DOMAIN, "rag-rat/entry/3");
        assert_eq!(CONTENT_SIGNED_DOMAIN, "rag-rat/signed-entry/1");
        assert_eq!(ACCOUNT_ID_DOMAIN, "rag-rat/account/1");
    }

    #[test]
    fn validity_limits_are_frozen_wire_constants() {
        // §18a: a violation is a structural reject in every impl, and changing one of these is a
        // deliberate wire bump. Pin the exact values so a drift breaks the build, not a live peer.
        assert_eq!(ACCOUNT_ENVELOPE_MAX_BYTES, 65_536);
        assert_eq!(SNAPSHOT_TARGETS_MAX, 256);
        assert_eq!(SNAPSHOT_COVERED_MAX, 1024);
        assert_eq!(CONTENT_ENVELOPE_MAX_BYTES, 262_144);
        assert_eq!(DEVICE_CUTS_MAX, 256);
        assert_eq!(CONTENT_CUTS_MAX, 1_024);
        assert_eq!(WRAP_RECIPIENTS_MAX, 1_024);
        // CBOR depth is shared with the op wire, not re-declared here.
        assert_eq!(crate::cbor::MAX_CBOR_DEPTH, 32);
    }
}
