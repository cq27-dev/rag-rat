//! The `/4` table-sync stream identity.
//!
//! Each `(repo_id, account_id, scope_id)` names one signed hash-chained stream — a sibling of the
//! `/3` content stream, on the same signed-entry layer. Committing the owning `account_id` inside
//! the hash makes ownership self-certifying (as `/2` does for content); committing `scope_id`
//! separates the anchors/overlay/distill logs so their per-column LWW clocks never compare across
//! scopes. A row op rides its table's scope stream and no other.

use minicbor::Encoder;

use crate::{AccountId, StreamId, cbor};

/// Domain tag + version for the table-sync stream identity. `/4` is a sibling of the content `/2`
/// derivation; bump the version only if the canonical rule itself changes.
const TABLE_STREAM_DOMAIN: &str = "rag-rat/stream/4";

/// Writing CBOR into a `Vec` cannot fail — mirrors `super::row_op` / `super::super::stream`.
const INFALLIBLE: &str = "encoding CBOR to a Vec is infallible";

/// Derive the immutable `stream_id` for a scope's table-sync log:
/// `sha256(cbor(["rag-rat/stream/4", account_id (b32), repo_id, scope_id]))`. Deterministic and
/// checkout-independent, so every device of one account derives the SAME id for a repo+scope — that
/// is what lets a peer's row ops land on the stream this device reads.
pub(crate) fn scope_stream_id(repo_id: &str, account_id: AccountId, scope_id: &str) -> StreamId {
    let mut buf = Vec::with_capacity(96);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(4).expect(INFALLIBLE);
        enc.str(TABLE_STREAM_DOMAIN).expect(INFALLIBLE);
        enc.bytes(&account_id.to_bytes()).expect(INFALLIBLE);
        enc.str(repo_id).expect(INFALLIBLE);
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
        let a = scope_stream_id("repo-a", account(1), "anchors/1");
        let b = scope_stream_id("repo-a", account(1), "anchors/1");
        assert_eq!(a, b, "the id is a pure function of (repo, account, scope)");
    }

    #[test]
    fn a_different_scope_repo_or_account_derives_a_different_stream() {
        let base = scope_stream_id("repo-a", account(1), "anchors/1");
        assert_ne!(
            base,
            scope_stream_id("repo-a", account(1), "overlay/1"),
            "scope separates logs"
        );
        assert_ne!(base, scope_stream_id("repo-b", account(1), "anchors/1"), "repo separates logs");
        assert_ne!(
            base,
            scope_stream_id("repo-a", account(2), "anchors/1"),
            "account separates logs"
        );
    }
}
