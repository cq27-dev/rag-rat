//! The op-log-backed [`SyncStore`] (phase D, #406).
//!
//! Adapts the phase-C ingest and read seams to the transport's [`SyncStore`] trait: entries offered
//! to a peer come from [`account_entries_for_sync`], and entries received from a peer go straight
//! into [`account_ingest`], which re-verifies signature, canonicity, and chain continuity. The
//! transport therefore adds no trust — a synced entry passes exactly the checks a local write does.

use rag_rat_oplog::{
    AccountId, ContentIngestOutcome, DeviceRole, IngestOutcome, account_entries_for_sync,
    account_entry_ref, account_ingest, account_signed_entry_exists, account_signed_hash,
    content_entries_for_sync, content_entry_ref, content_ingest, content_signed_entry_exists,
    content_signed_hash, sign_local_node_binding, verify_node_binding,
};
use rusqlite::Connection;

use crate::auth::{NodeAuth, PeerCapability};
use crate::session::{Ingested, SyncStore};

/// Mint this account's signed node binding for `local_node`. A store with no local device yet (a
/// fresh peer being onboarded) has nothing to prove, so it returns an EMPTY binding rather than
/// failing: an `Open` peer admits it as read-only, while a `Closed` peer fails to verify it (an
/// empty binding decodes to nothing) and correctly refuses. Shared by both op-log stores — the
/// binding is account-level, identical whether the session moves account entries or content.
fn sign_binding(
    conn: &Connection,
    account_id: AccountId,
    local_node: &[u8; 32],
    now_ms: i64,
) -> anyhow::Result<Vec<u8>> {
    match sign_local_node_binding(conn, account_id, local_node, now_ms)? {
        Ok(bytes) => Ok(bytes),
        // No local device to sign with — send an anonymous (empty) binding. Never authorizes under
        // `Closed`; harmless under `Open`.
        Err(_no_local_device) => Ok(Vec::new()),
    }
}

/// The capability a peer's binding grants for `account_id`, given its authenticated `remote_node`.
/// Collapses the internal failure taxonomy to `None` so the transport's wire refusal stays uniform;
/// a `?`-propagated error here is a real DB fault, not a rejected peer.
fn authorize_binding(
    conn: &Connection,
    account_id: AccountId,
    binding: &[u8],
    remote_node: &[u8; 32],
    now_ms: i64,
) -> anyhow::Result<Option<PeerCapability>> {
    Ok(verify_node_binding(conn, account_id, binding, remote_node, now_ms)?
        .ok()
        .map(capability_for_role))
}

fn capability_for_role(role: DeviceRole) -> PeerCapability {
    match role {
        DeviceRole::ReadOnly => PeerCapability::ReadOnly,
        DeviceRole::Member | DeviceRole::Owner => PeerCapability::ReadWrite,
    }
}

/// A [`SyncStore`] over one account's op log on a live connection. Scoped to a single account: a
/// session syncs one account, and the hello handshake refuses a peer naming a different one.
pub struct OplogSyncStore<'a> {
    conn: &'a Connection,
    account_id: AccountId,
    /// A CLOCK, not a captured instant: `ingest` reads it when an entry actually arrives, so a
    /// long-idle acceptor stamps `received_at_ms` (and drives pre-verify eviction ordering) with
    /// the receipt time, not a stale construction time.
    now_fn: fn() -> i64,
}

impl<'a> OplogSyncStore<'a> {
    pub fn new(conn: &'a Connection, account_id: AccountId, now_fn: fn() -> i64) -> Self {
        Self { conn, account_id, now_fn }
    }

    pub(crate) fn connection(&self) -> &'a Connection {
        self.conn
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
        match account_ingest(self.conn, signed_bytes, (self.now_fn)())? {
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

/// A [`SyncStore`] over one account's OWN `/3` content on a live connection (phase D, #406) — the
/// memories themselves, where [`OplogSyncStore`] moves the account log that authorizes them.
///
/// Scoped to a single account, like the account-log store: a session restores the account's own
/// memories onto a fresh sibling. Content authored by OTHER accounts (shared streams) is a later
/// slice (#407) and is refused here. Received bytes go through [`content_ingest`], which
/// re-resolves the roster key and re-verifies the signature from scratch — the transport adds no
/// trust.
///
/// Run this AFTER an account-log session in the same restore: `content_ingest` needs the roster and
/// grant material the account log carries to ACCEPT (rather than park) a candidate, so the account
/// authority must be in place first. A content session run before authority lands still transfers
/// every byte — it just parks candidates that a later settle promotes once authority arrives.
pub struct OplogContentSyncStore<'a> {
    conn: &'a Connection,
    account_id: AccountId,
    /// A CLOCK read at ingest time (see [`OplogSyncStore::now_fn`]), not a captured instant, so a
    /// long-idle acceptor stamps received content with its receipt time.
    now_fn: fn() -> i64,
}

impl<'a> OplogContentSyncStore<'a> {
    pub fn new(conn: &'a Connection, account_id: AccountId, now_fn: fn() -> i64) -> Self {
        Self { conn, account_id, now_fn }
    }
}

impl SyncStore for OplogContentSyncStore<'_> {
    fn account_id(&self) -> [u8; 32] {
        self.account_id.to_bytes()
    }

    fn snapshot(&self) -> anyhow::Result<Vec<([u8; 32], Vec<u8>)>> {
        // Key by the SIGNED-envelope hash, not `entry_hash`: two content envelopes can share an
        // entry_hash but differ in signature (content_pre_verify keeps competing signatures for
        // exactly this reason), and diffing by entry_hash would let a peer holding the valid
        // signature suppress it against one holding only an invalid variant.
        Ok(content_entries_for_sync(self.conn, self.account_id)?
            .into_iter()
            .map(|e| (content_signed_hash(&e.signed_bytes), e.signed_bytes))
            .collect())
    }

    fn ingest(&mut self, signed_bytes: &[u8]) -> anyhow::Result<Ingested> {
        // Refuse content whose CLAIMED author is a DIFFERENT account than this session before it
        // reaches `content_ingest`. This is a session-scope PRE-FILTER, not a trust boundary: the
        // claimed author is attacker-settable, so `content_ingest` re-resolves the roster key and
        // rejects anything not signed by a device in the account's roster regardless. The
        // pre-filter keeps an honestly-labeled foreign entry from parking in this account's
        // pre-verify table through a session that never named the other account. An
        // undecodable entry is a peer to distrust — dropped as NoChange, not a
        // session-fatal error.
        let Ok((_stream, entry_account, _entry_hash)) = content_entry_ref(signed_bytes) else {
            return Ok(Ingested::NoChange);
        };
        if entry_account != self.account_id {
            return Ok(Ingested::NoChange);
        }
        // Skip content already held — matched by the EXACT signed envelope, not entry_hash: a
        // distinct signature of the same body is a different entry the peer may need, so it must
        // still ingest. `content_ingest` re-reports `Ingested` for an exact replay, so without this
        // an idempotent redelivery would inflate "newly stored".
        if content_signed_entry_exists(self.conn, self.account_id, signed_bytes)? {
            return Ok(Ingested::NoChange);
        }
        // `content_ingest` is the SAME entry point untrusted content takes: it re-resolves the
        // roster key, re-verifies the signature, and stores the candidate under the §18b anti-abuse
        // budgets. A structural refusal (Rejected) or a capacity block is NOT a session error — a
        // peer may legitimately offer what this binary declines (e.g. content over the remote-flood
        // cap) — so map both to NoChange rather than aborting the whole session.
        match content_ingest(self.conn, signed_bytes, (self.now_fn)())? {
            // Newly durable: stored as a candidate, or durably parked pending its roster key. The
            // `Eviction` suffix reports collateral pre-verify eviction of OTHER parked rows, not a
            // failure of THIS entry.
            ContentIngestOutcome::PreVerify
            | ContentIngestOutcome::PreVerifyWithEviction { .. }
            | ContentIngestOutcome::Ingested { .. } => Ok(Ingested::Stored),
            // Already held / structurally refused / capacity-blocked: nothing new landed.
            ContentIngestOutcome::Rejected(_) | ContentIngestOutcome::CapacityReached { .. } =>
                Ok(Ingested::NoChange),
        }
    }
}

// Both op-log stores carry the same account-level node-authorization capability (the binding is
// about the account + transport node, independent of whether the session moves account entries or
// content), so both delegate to the shared helpers above.
// Both auth methods take `now_ms` per HANDSHAKE (distinct from the store's `now_fn` ingest clock):
// binding freshness must track the live clock, or a reused store would mint stale bindings and
// never advance the replay window.
impl NodeAuth for OplogSyncStore<'_> {
    fn local_binding(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<Vec<u8>> {
        sign_binding(self.conn, self.account_id, local_node, now_ms)
    }

    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<Option<PeerCapability>> {
        authorize_binding(self.conn, self.account_id, binding, remote_node, now_ms)
    }
}

impl NodeAuth for OplogContentSyncStore<'_> {
    fn local_binding(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<Vec<u8>> {
        sign_binding(self.conn, self.account_id, local_node, now_ms)
    }

    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<Option<PeerCapability>> {
        authorize_binding(self.conn, self.account_id, binding, remote_node, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_read_only_roster_role_lacks_push_capability() {
        assert_eq!(capability_for_role(DeviceRole::ReadOnly), PeerCapability::ReadOnly);
        assert_eq!(capability_for_role(DeviceRole::Member), PeerCapability::ReadWrite);
        assert_eq!(capability_for_role(DeviceRole::Owner), PeerCapability::ReadWrite);
    }
}
