//! The op-log-backed [`SyncStore`] (phase D, #406).
//!
//! Adapts the phase-C ingest and read seams to the transport's [`SyncStore`] trait: entries offered
//! to a peer come from [`account_entries_for_sync`], and entries received from a peer go straight
//! into [`account_ingest`], which re-verifies signature, canonicity, and chain continuity. The
//! transport therefore adds no trust — a synced entry passes exactly the checks a local write does.

use rag_rat_oplog::{
    AccessMode, AccountId, ContentIngestOutcome, DeviceRole, IngestOutcome, NodeAuthError,
    account_effective_count, account_entries_for_enrollment, account_entries_for_sync,
    account_entry_ref, account_ingest, account_signed_entry_exists, account_signed_hash,
    content_entries_for_public_sync, content_entries_for_sync, content_entry_ref, content_ingest,
    content_signed_entry_exists, content_signed_hash, sign_local_node_binding, stream_access_mode,
    stream_owner_account, verify_node_binding,
};
use rusqlite::Connection;

use crate::auth::{LocalAuth, NodeAuth, PeerAuthorization, PeerCapability};
use crate::session::{Ingested, ServeScope, SyncStore};
use crate::table_session::{ChainEntry, ChainStart, TableSyncStore};
use crate::table_wire::{ChainHead, FrontierState, ManifestItem};

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

/// The verdict for a peer's binding to `account_id`, given its authenticated `remote_node`.
/// Binding failures collapse to `Rejected`; only a valid binding whose roster cannot be checked
/// because this store has no effective account fold is `Unavailable`. The transport still renders
/// every refusal uniformly; a propagated error here is a real DB fault.
fn authorize_binding(
    conn: &Connection,
    account_id: AccountId,
    binding: &[u8],
    remote_node: &[u8; 32],
    now_ms: i64,
) -> anyhow::Result<PeerAuthorization> {
    Ok(match verify_node_binding(conn, account_id, binding, remote_node, now_ms)? {
        Ok(role) => PeerAuthorization::Granted(capability_for_role(role)),
        Err(NodeAuthError::NotRosterDevice) if account_effective_count(conn, account_id)? == 0 =>
            PeerAuthorization::Unavailable,
        Err(_) => PeerAuthorization::Rejected,
    })
}

fn capability_for_role(role: DeviceRole) -> PeerCapability {
    match role {
        DeviceRole::ReadOnly => PeerCapability::ReadOnly,
        DeviceRole::Member | DeviceRole::Owner => PeerCapability::ReadWrite,
    }
}

fn local_auth(
    conn: &Connection,
    account_id: AccountId,
    local_node: &[u8; 32],
    now_ms: i64,
) -> anyhow::Result<LocalAuth> {
    let binding = sign_binding(conn, account_id, local_node, now_ms)?;
    let capability = match authorize_binding(conn, account_id, &binding, local_node, now_ms)? {
        PeerAuthorization::Granted(capability) => capability,
        PeerAuthorization::Rejected | PeerAuthorization::Unavailable => PeerCapability::ReadOnly,
    };
    Ok(LocalAuth { binding, capability })
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
    /// How much of the account log this session serves (#407 E2b). `Full` by default; a dispatcher
    /// narrows it to `PublicOnly` post-auth for an anonymous reader of a public account.
    serve_scope: ServeScope,
}

impl<'a> OplogSyncStore<'a> {
    pub fn new(conn: &'a Connection, account_id: AccountId, now_fn: fn() -> i64) -> Self {
        Self { conn, account_id, now_fn, serve_scope: ServeScope::Full }
    }

    pub(crate) fn connection(&self) -> &'a Connection {
        self.conn
    }
}

impl SyncStore for OplogSyncStore<'_> {
    fn account_id(&self) -> [u8; 32] {
        self.account_id.to_bytes()
    }

    fn set_serve_scope(&mut self, scope: ServeScope) {
        self.serve_scope = scope;
    }

    fn snapshot(&self) -> anyhow::Result<Vec<([u8; 32], Vec<u8>)>> {
        // Key by the SIGNED-envelope hash, not `entry_hash`: two envelopes can share an entry_hash
        // but differ in signature (pre-verify keeps competing signatures for exactly this reason),
        // and diffing by entry_hash would let a peer holding the valid signature suppress it.
        //
        // `PublicOnly` serves the AUTHENTICATED account log only (`account_entries_for_enrollment`,
        // which omits the unauthenticated parked `account_pre_verify` rows) — a public server must
        // not relay forged candidates to anonymous readers. For a fully-public account (the only
        // account served `PublicOnly`) this is the whole control + secrets log, exactly what a
        // subscriber needs to verify the content.
        let entries = match self.serve_scope {
            ServeScope::Full => account_entries_for_sync(self.conn, self.account_id)?,
            ServeScope::PublicOnly => account_entries_for_enrollment(self.conn, self.account_id)?,
        };
        Ok(entries
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
/// memories onto a fresh sibling. Content authored by OTHER accounts is admitted only on a
/// `public_read` stream (#407) — private foreign content is still refused here. Received bytes go
/// through [`content_ingest`], which re-resolves the roster key and re-verifies the signature from
/// scratch — the transport adds no trust.
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
    /// How much content this session serves (#407 E2b) — see [`OplogSyncStore`]'s field.
    serve_scope: ServeScope,
}

impl<'a> OplogContentSyncStore<'a> {
    pub fn new(conn: &'a Connection, account_id: AccountId, now_fn: fn() -> i64) -> Self {
        Self { conn, account_id, now_fn, serve_scope: ServeScope::Full }
    }
}

impl SyncStore for OplogContentSyncStore<'_> {
    fn account_id(&self) -> [u8; 32] {
        self.account_id.to_bytes()
    }

    fn set_serve_scope(&mut self, scope: ServeScope) {
        self.serve_scope = scope;
    }

    fn snapshot(&self) -> anyhow::Result<Vec<([u8; 32], Vec<u8>)>> {
        // Key by the SIGNED-envelope hash, not `entry_hash`: two content envelopes can share an
        // entry_hash but differ in signature (content_pre_verify keeps competing signatures for
        // exactly this reason), and diffing by entry_hash would let a peer holding the valid
        // signature suppress it against one holding only an invalid variant.
        //
        // `PublicOnly` serves only AUTHENTICATED content (`content_entries_for_public_sync`, which
        // omits the unauthenticated parked `content_pre_verify` candidates) — a public server must
        // not relay forged candidates to anonymous readers.
        let entries = match self.serve_scope {
            ServeScope::Full => content_entries_for_sync(self.conn, self.account_id)?,
            ServeScope::PublicOnly => content_entries_for_public_sync(self.conn, self.account_id)?,
        };
        Ok(entries
            .into_iter()
            .map(|e| (content_signed_hash(&e.signed_bytes), e.signed_bytes))
            .collect())
    }

    fn ingest(&mut self, signed_bytes: &[u8]) -> anyhow::Result<Ingested> {
        // Admit this account's OWN content, plus foreign content on a `public_read` stream, before
        // it reaches `content_ingest`. This is a session-scope PRE-FILTER, not a trust boundary:
        // the claimed author is attacker-settable, so `content_ingest` re-resolves the roster key
        // and rejects anything not signed by a device in the author's roster regardless — the
        // pre-filter only widens WHICH foreign candidates reach that verifier, never how they are
        // trusted. Private (and not-yet-known-owner) foreign content is dropped so it never parks
        // in this account's pre-verify table through a session that never named the other account.
        // An undecodable entry is a peer to distrust — dropped as NoChange, not a session-fatal
        // error.
        let Ok((stream, entry_account, _entry_hash)) = content_entry_ref(signed_bytes) else {
            return Ok(Ingested::NoChange);
        };
        if entry_account != self.account_id {
            // Resolve the access mode from the STREAM's owner, never from the attacker-settable
            // claimed author — that also correctly admits grant-gated contributor content
            // (grantee != owner) on a public stream, which `content_ingest` then gates on the
            // grant. A dropped entry is re-offered by the next session's snapshot diff, so once the
            // owner's account log is folded locally the admission converges (E2b orders owner log
            // before content on delivery, so first-session convergence is the norm). ANY failure to
            // resolve the owner/mode (owner not yet synced, a corrupt ownership fact) is treated as
            // "not public" and drops the entry as NoChange — fail-closed AND never session-fatal,
            // matching the undecodable-entry posture above rather than aborting the whole session.
            let public =
                stream_owner_account(self.conn, stream).ok().flatten().is_some_and(|owner| {
                    stream_access_mode(self.conn, owner, stream).ok()
                        == Some(AccessMode::PublicRead)
                });
            if !public {
                return Ok(Ingested::NoChange);
            }
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
    fn local_auth(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<LocalAuth> {
        local_auth(self.conn, self.account_id, local_node, now_ms)
    }

    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<PeerAuthorization> {
        authorize_binding(self.conn, self.account_id, binding, remote_node, now_ms)
    }
}

impl NodeAuth for OplogContentSyncStore<'_> {
    fn local_auth(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<LocalAuth> {
        local_auth(self.conn, self.account_id, local_node, now_ms)
    }

    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<PeerAuthorization> {
        authorize_binding(self.conn, self.account_id, binding, remote_node, now_ms)
    }
}

/// Production adapter for current repo-scoped `/5` table streams.
pub struct OplogTableSyncStore<'a, F = fn() -> i64> {
    conn: &'a Connection,
    account_id: AccountId,
    now_fn: F,
}

impl<'a, F: Fn() -> i64> OplogTableSyncStore<'a, F> {
    pub fn new(conn: &'a Connection, account_id: AccountId, now_fn: F) -> Self {
        Self { conn, account_id, now_fn }
    }

    /// Whether this binary currently supports any table stream for this account.
    pub fn has_streams(&self) -> anyhow::Result<bool> {
        Ok(!rag_rat_oplog::table_sync_supported_streams(self.conn, self.account_id)?.is_empty())
    }
}

fn to_manifest_item(stream: rag_rat_oplog::TableSyncStream) -> ManifestItem {
    ManifestItem {
        repo_id: stream.repo_id,
        incarnation_ref: stream.incarnation_ref,
        scope_id: stream.scope_id,
        stream_id: stream.stream_id,
    }
}

fn to_oplog_stream(item: &ManifestItem) -> rag_rat_oplog::TableSyncStream {
    rag_rat_oplog::TableSyncStream {
        repo_id: item.repo_id.clone(),
        incarnation_ref: item.incarnation_ref,
        scope_id: item.scope_id.clone(),
        stream_id: item.stream_id,
    }
}

impl<F: Fn() -> i64> TableSyncStore for OplogTableSyncStore<'_, F> {
    fn account_id(&self) -> [u8; 32] {
        self.account_id.to_bytes()
    }

    fn prepare(&mut self) -> anyhow::Result<()> {
        // Author local edits FIRST, then compact: the retention docs require the authoring pass to
        // have run so a winner restamp never disowns an unsent local edit. `prepare` runs only when
        // the local device can push, so a read-only peer never compacts. Compaction is a
        // re-runnable steady-state no-op — bounded scopes (overlay/1) trim to their budget,
        // fully-retained scopes (anchors/1) are inert.
        let now_ms = (self.now_fn)();
        rag_rat_oplog::table_sync_author_pending(self.conn, self.account_id, now_ms)?;
        rag_rat_oplog::table_sync_compact_overdue(
            self.conn,
            self.account_id,
            now_ms,
            &rag_rat_oplog::scope_retention_budget,
        )?;
        Ok(())
    }

    fn supported_streams(&self) -> anyhow::Result<Vec<ManifestItem>> {
        Ok(rag_rat_oplog::table_sync_supported_streams(self.conn, self.account_id)?
            .into_iter()
            .map(to_manifest_item)
            .collect())
    }

    fn validates(&self, item: &ManifestItem) -> anyhow::Result<bool> {
        rag_rat_oplog::table_sync_validate_stream(
            self.conn,
            self.account_id,
            &to_oplog_stream(item),
        )
    }

    fn chain_page(
        &self,
        item: &ManifestItem,
        after_device: Option<[u8; 32]>,
        limit: usize,
    ) -> anyhow::Result<Vec<ChainHead>> {
        Ok(rag_rat_oplog::table_sync_chain_page_after(
            self.conn,
            self.account_id,
            &to_oplog_stream(item),
            after_device,
            limit,
        )?
        .into_iter()
        .map(|chain| ChainHead {
            device_fingerprint: chain.device_fingerprint,
            lamport: chain.lamport,
            entry_hash: chain.entry_hash,
            floor: chain.floor,
        })
        .collect())
    }

    fn frontier(&self, item: &ManifestItem, device: [u8; 32]) -> anyhow::Result<FrontierState> {
        Ok(
            match rag_rat_oplog::table_sync_chain_frontier(
                self.conn,
                self.account_id,
                &to_oplog_stream(item),
                device,
            )? {
                rag_rat_oplog::TableSyncFrontier::Empty => FrontierState::Empty,
                rag_rat_oplog::TableSyncFrontier::Accepted { lamport, entry_hash } =>
                    FrontierState::Accepted { lamport, entry_hash },
                rag_rat_oplog::TableSyncFrontier::Restore { lamport, entry_hash } =>
                    FrontierState::Restore { lamport, entry_hash },
            },
        )
    }

    fn entries(
        &self,
        item: &ManifestItem,
        device: [u8; 32],
        start: ChainStart,
        limit: usize,
    ) -> anyhow::Result<Vec<ChainEntry>> {
        let start = match start {
            ChainStart::Beginning => rag_rat_oplog::TableSyncEntryStart::Beginning,
            ChainStart::After { lamport, entry_hash } =>
                rag_rat_oplog::TableSyncEntryStart::After { lamport, entry_hash },
            ChainStart::At { lamport, entry_hash } =>
                rag_rat_oplog::TableSyncEntryStart::At { lamport, entry_hash },
        };
        Ok(rag_rat_oplog::table_sync_chain_entries(
            self.conn,
            self.account_id,
            &to_oplog_stream(item),
            device,
            start,
            limit,
        )?
        .into_iter()
        .map(|entry| ChainEntry {
            lamport: entry.lamport,
            entry_hash: entry.entry_hash,
            signed_bytes: entry.signed_bytes,
        })
        .collect())
    }

    fn ingest(
        &mut self,
        item: &ManifestItem,
        expected_device: [u8; 32],
        signed_bytes: &[u8],
        advertised_floor: Option<(u64, [u8; 32])>,
    ) -> anyhow::Result<Ingested> {
        Ok(
            match rag_rat_oplog::table_sync_ingest(
                self.conn,
                self.account_id,
                &to_oplog_stream(item),
                expected_device,
                signed_bytes,
                (self.now_fn)(),
                advertised_floor,
            )? {
                rag_rat_oplog::TableSyncIngestOutcome::Stored => Ingested::Stored,
                rag_rat_oplog::TableSyncIngestOutcome::NoChange => Ingested::NoChange,
            },
        )
    }
}

impl<F> NodeAuth for OplogTableSyncStore<'_, F> {
    fn local_auth(&self, local_node: &[u8; 32], now_ms: i64) -> anyhow::Result<LocalAuth> {
        local_auth(self.conn, self.account_id, local_node, now_ms)
    }

    fn authorize(
        &self,
        binding: &[u8],
        remote_node: &[u8; 32],
        now_ms: i64,
    ) -> anyhow::Result<PeerAuthorization> {
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

    #[test]
    fn table_store_maps_routes_and_skips_them_without_current_incarnations() {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::hooks::MigrationHooks::noop()).unwrap();
        let account_id = AccountId::from_bytes([1; 32]);
        let stream = rag_rat_oplog::TableSyncStream {
            repo_id: "repo-a".into(),
            incarnation_ref: [2; 32],
            scope_id: "anchors/1".into(),
            stream_id: [3; 32],
        };
        let item = to_manifest_item(stream.clone());
        assert_eq!(to_oplog_stream(&item), stream);

        let mut store = OplogTableSyncStore::new(&conn, account_id, || 7);
        assert_eq!(store.account_id(), account_id.to_bytes());
        assert!(!store.has_streams().unwrap());
        assert!(store.supported_streams().unwrap().is_empty());
        assert!(!store.validates(&item).unwrap());
        assert!(store.chain_page(&item, None, 1).unwrap().is_empty());
        assert_eq!(store.frontier(&item, [4; 32]).unwrap(), FrontierState::Empty);
        assert!(store.entries(&item, [4; 32], ChainStart::Beginning, 1).unwrap().is_empty());
        assert_eq!(store.ingest(&item, [4; 32], &[0], None).unwrap(), Ingested::NoChange);
    }
}
