//! The op-log-backed [`SyncStore`] (phase D, #406).
//!
//! Adapts the phase-C ingest and read seams to the transport's [`SyncStore`] trait: entries offered
//! to a peer come from [`account_entries_for_sync`], and entries received from a peer go straight
//! into [`account_ingest`], which re-verifies signature, canonicity, and chain continuity. The
//! transport therefore adds no trust — a synced entry passes exactly the checks a local write does.

use rag_rat_oplog::{
    AccountId, IngestOutcome, account_entries_for_sync, account_entry_ref, account_ingest,
    account_signed_entry_exists, account_signed_hash,
};
use rusqlite::Connection;

use crate::session::{Ingested, SyncStore};

/// A [`SyncStore`] over one account's op log on a live connection. Scoped to a single account: a
/// session syncs one account, and the hello handshake refuses a peer naming a different one.
pub struct OplogSyncStore<'a> {
    conn: &'a Connection,
    account_id: AccountId,
    now_ms: i64,
}

impl<'a> OplogSyncStore<'a> {
    pub fn new(conn: &'a Connection, account_id: AccountId, now_ms: i64) -> Self {
        Self { conn, account_id, now_ms }
    }
}

impl SyncStore for OplogSyncStore<'_> {
    fn account_id(&self) -> [u8; 32] {
        self.account_id.to_bytes()
    }

    fn snapshot(&self) -> anyhow::Result<Vec<([u8; 32], Vec<u8>)>> {
        // Key by the SIGNED-envelope hash, not `entry_hash`: two envelopes can share an entry_hash
        // but differ in signature (pre-verify keeps competing signatures for exactly this reason),
        // and diffing by entry_hash would let a peer holding the valid signature suppress it.
        Ok(account_entries_for_sync(self.conn, self.account_id)?
            .into_iter()
            .map(|e| (account_signed_hash(&e.signed_bytes), e.signed_bytes))
            .collect())
    }

    fn ingest(&mut self, signed_bytes: &[u8]) -> anyhow::Result<Ingested> {
        // Refuse an entry for a DIFFERENT account before it reaches `account_ingest`. This session
        // is scoped to one account; `account_ingest` would happily store a valid entry for any
        // account (it is not account-scoped), so a peer could otherwise inject and grow other
        // accounts through a session that never named them. A structurally undecodable entry is a
        // peer to distrust, not a session-fatal error — drop it as NoChange.
        let Ok((entry_account, _entry_hash)) = account_entry_ref(signed_bytes) else {
            return Ok(Ingested::NoChange);
        };
        if entry_account != self.account_id {
            return Ok(Ingested::NoChange);
        }
        // Skip an entry already held — matched by the EXACT signed envelope, not entry_hash: a
        // distinct signature of the same body is a different entry the peer may need, so it must
        // still ingest. `account_ingest`'s fast path re-reports `Ingested` for an exact replay, so
        // without this an idempotent redelivery would inflate "newly stored".
        if account_signed_entry_exists(self.conn, self.account_id, signed_bytes)? {
            return Ok(Ingested::NoChange);
        }
        // `account_ingest` is the SAME entry point a local write uses: it re-verifies from scratch,
        // so a forged or malformed frame is rejected here exactly as a bad local write would be.
        // A structurally rejected entry is NOT an error — a peer may legitimately offer something
        // this binary refuses (e.g. over a future cap) — so map it to NoChange, not a failure that
        // would abort the whole session.
        match account_ingest(self.conn, signed_bytes, self.now_ms)? {
            // Newly durable: stored, or durably parked pending its signer. Every `Ingested*`
            // variant added state (the `RejectedPromotions` suffixes report collateral pre-verify
            // eviction of OTHER parked rows, not a failure of THIS entry).
            IngestOutcome::PreVerify
            | IngestOutcome::PreVerifyWithEviction { .. }
            | IngestOutcome::Ingested { .. }
            | IngestOutcome::IngestedWithRejectedPromotions { .. }
            | IngestOutcome::IngestedWithRejectedContentPromotions { .. }
            | IngestOutcome::IngestedWithRejectedAccountAndContentPromotions { .. } =>
                Ok(Ingested::Stored),
            // Already held / structurally refused / capacity-blocked: nothing new landed. A refusal
            // is not a session error — a peer may legitimately offer what this binary declines.
            IngestOutcome::Rejected(_) | IngestOutcome::CapacityReached { .. } =>
                Ok(Ingested::NoChange),
        }
    }
}
