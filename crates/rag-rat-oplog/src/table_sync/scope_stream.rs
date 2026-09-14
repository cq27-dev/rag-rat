//! The `/5` table-sync stream identity.
//!
//! Each `(repo_id, account_id, incarnation_ref, scope_id)` names one signed hash-chained stream — a
//! sibling of the `/3` content stream, on the same signed-entry layer. Committing the owning
//! `account_id` inside the hash makes ownership self-certifying (as `/2` does for content);
//! committing the account-authorized incarnation separates explicit repository resets, and
//! `scope_id` separates the anchors/overlay/distill logs. A row op rides its table's scope stream
//! and no other.

use minicbor::Encoder;

use crate::cbor::{self, VecEncoderExt};
use crate::{AccountId, StreamId};

/// Domain tag + version for the table-sync stream identity. `/5` is a sibling of the content `/2`
/// derivation; bump the version only if the canonical rule itself changes.
const TABLE_STREAM_DOMAIN: &str = "rag-rat/stream/5";

/// The `/5` scope a table rides — the last element of its stream's hashed identity, so the token
/// is wire identity: a respelled token derives a different stream, and every op stored under the
/// old one stops resolving to its table. Production code names only the three consts;
/// [`Self::from_db_str`] returns `None` for any other token, so an unrecognized stored or received
/// scope stays a soft miss.
///
/// A newtype over the token rather than a closed enum: the engine is generic over its registry, and
/// tests register scopes of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ScopeId(&'static str);

impl ScopeId {
    /// Durable memory anchors.
    pub(crate) const ANCHORS: Self = Self("anchors/1");
    /// Regenerated memory overlays (summaries, reality verdicts).
    pub(crate) const OVERLAY: Self = Self("overlay/1");
    /// Papertrail distill records and their enrichment children.
    pub(crate) const DISTILL: Self = Self("distill/1");

    /// A scope outside the built-in set, for a custom registry.
    pub(crate) const fn new(token: &'static str) -> Self {
        Self(token)
    }

    /// The token as hashed into the stream id and stored in `table_sync_streams.scope_id`.
    pub(crate) fn as_db_str(self) -> &'static str {
        self.0
    }

    /// The production scope `token` names, or `None` for any other token.
    pub(crate) fn from_db_str(token: &str) -> Option<Self> {
        [Self::ANCHORS, Self::OVERLAY, Self::DISTILL].into_iter().find(|scope| scope.0 == token)
    }
}

/// Derive the immutable `stream_id` for a scope's table-sync log:
/// `sha256(cbor(["rag-rat/stream/5", account_id (b32), repo_id, incarnation_ref (b32),
/// scope_id]))`. Deterministic and checkout-independent, so every device of one account derives the
/// SAME id for an authorized repository incarnation and scope — that is what lets a peer's row ops
/// land on the stream this device reads.
pub(crate) fn scope_stream_id(
    repo_id: &str,
    account_id: AccountId,
    incarnation_ref: [u8; 32],
    scope_id: ScopeId,
) -> StreamId {
    let mut buf = Vec::with_capacity(96);
    {
        let mut enc = Encoder::new(&mut buf);
        enc.put_array(5);
        enc.put_str(TABLE_STREAM_DOMAIN);
        enc.put_bytes(&account_id.to_bytes());
        enc.put_str(repo_id);
        enc.put_bytes(&incarnation_ref);
        enc.put_str(scope_id.as_db_str());
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
        let a = scope_stream_id("repo-a", account(1), [3; 32], ScopeId::ANCHORS);
        let b = scope_stream_id("repo-a", account(1), [3; 32], ScopeId::ANCHORS);
        assert_eq!(a, b, "the id is a pure function of (repo, account, incarnation, scope)");
    }

    #[test]
    fn a_different_scope_repo_or_account_derives_a_different_stream() {
        let base = scope_stream_id("repo-a", account(1), [3; 32], ScopeId::ANCHORS);
        assert_ne!(
            base,
            scope_stream_id("repo-a", account(1), [3; 32], ScopeId::OVERLAY),
            "scope separates logs"
        );
        assert_ne!(
            base,
            scope_stream_id("repo-b", account(1), [3; 32], ScopeId::ANCHORS),
            "repo separates logs"
        );
        assert_ne!(
            base,
            scope_stream_id("repo-a", account(2), [3; 32], ScopeId::ANCHORS),
            "account separates logs"
        );
    }

    #[test]
    fn every_production_scope_token_is_pinned_into_its_stream_id() {
        // The token sits inside the sha256 preimage: a respelled const derives a different stream,
        // and every stored op for its tables would stop resolving.
        assert_eq!(ScopeId::ANCHORS.as_db_str(), "anchors/1");
        assert_eq!(ScopeId::OVERLAY.as_db_str(), "overlay/1");
        assert_eq!(ScopeId::DISTILL.as_db_str(), "distill/1");
        for (scope, want) in [
            (ScopeId::ANCHORS, "c706c98ac24098b6f6900aed62b869ea9da13b37f53e2285c535566620e04a5a"),
            (ScopeId::OVERLAY, "4aaacdce878d3628b97c1ce01458be69fb62d1d1ddd76e76ef477fdd8d2297e4"),
            (ScopeId::DISTILL, "38965e3e8710ca587314c5803e4998fbc6b2dad784af6ca11f76beaf82994b01"),
        ] {
            let id = scope_stream_id("repo-a", account(1), [3; 32], scope);
            assert_eq!(rag_rat_base::hash::hex_lower(&id.to_bytes()), want, "{scope:?}");
            assert_eq!(ScopeId::from_db_str(scope.as_db_str()), Some(scope));
        }
        assert_eq!(ScopeId::from_db_str("demo/1"), None, "only the production tokens resolve");
    }

    #[test]
    fn incarnation_separates_streams_and_wire_is_golden() {
        let first = scope_stream_id("repo-a", account(1), [3; 32], ScopeId::ANCHORS);
        let second = scope_stream_id("repo-a", account(1), [4; 32], ScopeId::ANCHORS);
        assert_ne!(first, second);
        assert_eq!(first.to_bytes(), [
            0xc7, 0x06, 0xc9, 0x8a, 0xc2, 0x40, 0x98, 0xb6, 0xf6, 0x90, 0x0a, 0xed, 0x62, 0xb8,
            0x69, 0xea, 0x9d, 0xa1, 0x3b, 0x37, 0xf5, 0x3e, 0x22, 0x85, 0xc5, 0x35, 0x56, 0x66,
            0x20, 0xe0, 0x4a, 0x5a,
        ]);
    }
}
