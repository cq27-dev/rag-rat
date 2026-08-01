//! The `/5` table-sync stream identity.
//!
//! Each `(repo_id, account_id, incarnation_ref, scope_id)` names one signed hash-chained stream — a
//! sibling of the `/3` content stream, on the same signed-entry layer. Committing the owning
//! `account_id` inside the hash makes ownership self-certifying (as `/2` does for content);
//! committing the account-authorized incarnation separates explicit repository resets, and
//! `scope_id` separates the anchors/overlay/distill logs. A row op rides its table's scope stream
//! and no other.

use minicbor::Encoder;

use crate::{AccountId, StreamId, cbor};

/// Domain tag + version for the table-sync stream identity. `/5` is a sibling of the content `/2`
/// derivation; bump the version only if the canonical rule itself changes.
const TABLE_STREAM_DOMAIN: &str = "rag-rat/stream/5";

/// Writing CBOR into a `Vec` cannot fail — mirrors `super::row_op` / `super::super::stream`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// Derive the immutable `stream_id` for a scope's table-sync log:
/// `sha256(cbor(["rag-rat/stream/5", account_id (b32), repo_id, incarnation_ref (b32),
/// scope_id]))`. Deterministic and checkout-independent, so every device of one account derives the
/// SAME id for an authorized repository incarnation and scope — that is what lets a peer's row ops
/// land on the stream this device reads.
pub(crate) fn scope_stream_id(
    repo_id: &str,
    account_id: AccountId,
    incarnation_ref: [u8; 32],
    scope_id: &str,
) -> StreamId {
    let mut buf = Vec::with_capacity(96);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(5).expect(INFALLIBLE);
        enc.str(TABLE_STREAM_DOMAIN).expect(INFALLIBLE);
        enc.bytes(&account_id.to_bytes()).expect(INFALLIBLE);
        enc.str(repo_id).expect(INFALLIBLE);
        enc.bytes(&incarnation_ref).expect(INFALLIBLE);
        enc.str(scope_id).expect(INFALLIBLE);
    }
    StreamId::from_bytes(cbor::sha256(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(seed: u8) -> AccountId {
        AccountId::from_bytes([seed; 32])
    }

    #[test]
    fn same_inputs_derive_the_same_stream() {
        let a = scope_stream_id("repo-a", account(1), [3; 32], "anchors/1");
        let b = scope_stream_id("repo-a", account(1), [3; 32], "anchors/1");
        assert_eq!(a, b, "the id is a pure function of (repo, account, incarnation, scope)");
    }

    #[test]
    fn a_different_scope_repo_or_account_derives_a_different_stream() {
        let base = scope_stream_id("repo-a", account(1), [3; 32], "anchors/1");
        assert_ne!(
            base,
            scope_stream_id("repo-a", account(1), [3; 32], "overlay/1"),
            "scope separates logs"
        );
        assert_ne!(
            base,
            scope_stream_id("repo-b", account(1), [3; 32], "anchors/1"),
            "repo separates logs"
        );
        assert_ne!(
            base,
            scope_stream_id("repo-a", account(2), [3; 32], "anchors/1"),
            "account separates logs"
        );
    }

    #[test]
    fn incarnation_separates_streams_and_wire_is_golden() {
        let first = scope_stream_id("repo-a", account(1), [3; 32], "anchors/1");
        let second = scope_stream_id("repo-a", account(1), [4; 32], "anchors/1");
        assert_ne!(first, second);
        assert_eq!(first.to_bytes(), [
            0xc7, 0x06, 0xc9, 0x8a, 0xc2, 0x40, 0x98, 0xb6, 0xf6, 0x90, 0x0a, 0xed, 0x62, 0xb8,
            0x69, 0xea, 0x9d, 0xa1, 0x3b, 0x37, 0xf5, 0x3e, 0x22, 0x85, 0xc5, 0x35, 0x56, 0x66,
            0x20, 0xe0, 0x4a, 0x5a,
        ]);
    }
}
