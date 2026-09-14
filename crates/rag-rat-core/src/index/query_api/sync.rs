//! Account, stream, and sync operations on `IndexDatabase`.

use anyhow::Context as _;
use rag_rat_query::memory;

use super::IndexDatabase;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncCatchUpReport {
    pub target: rag_rat_oplog::DeviceFingerprint,
    pub required: u64,
    pub already_covered: u64,
    pub authored: u64,
}

/// Outcome of `sync publish --seed`: whether the publish ratchet flipped this run, and how many
/// memories were imported from the seed source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishSeedReport {
    pub published: bool,
    pub imported_memories: u64,
}

impl IndexDatabase {
    /// Permanently enable sealed local memory authoring for this repo. Existing suite-0 history is
    /// retained; subsequent live and reconcile entries use suite 1.
    pub fn sync_enable(&self) -> anyhow::Result<bool> {
        crate::memory_write::enable_sealed_authoring(
            self.storage.connection(),
            rag_rat_base::time::now_ms(),
        )
    }

    /// Mark this repo's account as a public knowledge base: persist the one-way `public`
    /// access-mode intent and ensure its `PublicRead` `/2` owner stream, so subsequent memory
    /// authoring is public and the account is servable to anonymous readers. Refuses if the
    /// account already holds a private stream (publishing an existing private repo is not
    /// supported) or authors sealed.
    pub fn sync_publish(&self) -> anyhow::Result<bool> {
        crate::memory_write::enable_public_authoring(
            self.storage.connection(),
            rag_rat_base::time::now_ms(),
        )
    }

    /// Publish this repo's account as a public knowledge base AND seed it from `source`: refuse a
    /// sealed source, flip the one-way publish ratchet, then import this repo's locally-authored
    /// memories out of `source` (a separate rag-rat index) and author them onto the PublicRead
    /// owner stream. The sealed-source refusal runs BEFORE publish, so a bad source never
    /// leaves a half-published node; a failure between the publish and the import is
    /// recoverable by re-running.
    pub fn sync_publish_seed(&self, source: &std::path::Path) -> anyhow::Result<PublishSeedReport> {
        let conn = self.storage.connection();
        let repo_id = memory::memory_repo_scope(conn)?
            .context("sync publish requires an active repo scope")?;
        crate::index::consolidate::import::ensure_source_unsealed(source, &repo_id)?;
        // BEFORE publishing: publishing establishes this store's own PublicRead stream and is a
        // one-way ratchet, but a contributor's writes target the CONFIGURED owner's stream, so the
        // seeded rows would have nowhere to be authored and the fresh public stream would stay
        // empty while the CLI reported them seeded. Refuse while nothing irreversible has happened.
        crate::memory_write::ensure_not_mirroring_another_account(
            conn,
            &repo_id,
            "`sync publish --seed`",
        )?;
        let published =
            crate::memory_write::enable_public_authoring(conn, rag_rat_base::time::now_ms())?;
        let imported_memories = crate::index::consolidate::import::seed_from_index(
            conn,
            source,
            &repo_id,
            rag_rat_base::time::now_ms(),
        )
        .context(
            "the public node is published, but seeding failed; re-run `sync publish --seed \
             <path>` to complete",
        )?;
        Ok(PublishSeedReport { published, imported_memories })
    }

    /// This store's local account id as lowercase hex — the identity another owner grants with
    /// `sync grant <id>`. Mints the account if absent so a fresh contributor can report its id.
    pub fn sync_whoami(&self) -> anyhow::Result<String> {
        let account =
            rag_rat_oplog::local_account(self.storage.connection(), rag_rat_base::time::now_ms())?;
        Ok(rag_rat_base::hash::hex_lower(&account.to_bytes()))
    }

    /// Grant `grantee` (the account id from its `sync whoami`) Writer authority on
    /// the active repo's owner stream (#1164), so that identity can author memories into this
    /// repo's shared set. Owner-only; requires a published repo. Returns the grant id as hex.
    pub fn sync_grant(&self, grantee: rag_rat_oplog::AccountId) -> anyhow::Result<String> {
        let grant_id = crate::memory_write::grant_repo_writer(
            self.storage.connection(),
            grantee,
            rag_rat_base::time::now_ms(),
        )?;
        Ok(rag_rat_base::hash::hex_lower(&grant_id))
    }

    /// The active repo's grant listing (`sync grants`): every grant this owner has authored on
    /// the repo's stream, open and revoked, newest first.
    pub fn sync_grants(&self) -> anyhow::Result<Vec<crate::memory_write::RepoGrantListing>> {
        crate::memory_write::list_repo_grants(self.storage.connection())
    }

    /// Revoke the active repo's open grant to `grantee_ref` (a 64-hex account id or an
    /// unambiguous prefix of an open grantee), with reason-driven cut semantics (#1177), then
    /// settle the resulting re-judgment so the revocation's effect on materialized memories lands
    /// in the same command. Returns the report plus how many projected nodes the eviction
    /// removed.
    pub fn sync_revoke(
        &self,
        grantee_ref: &str,
        reason: rag_rat_oplog::RevokeReason,
        keep_until: Option<(rag_rat_oplog::DeviceFingerprint, u64)>,
    ) -> anyhow::Result<(crate::memory_write::RepoRevokeReport, u32)> {
        let report = crate::memory_write::revoke_repo_writer(
            self.storage.connection(),
            grantee_ref,
            reason,
            keep_until,
            rag_rat_base::time::now_ms(),
        )?;
        // The account refold queued the stream; settling here means the operator sees the
        // eviction (retro-condemned entries un-projecting) as part of the revoke, not at some
        // later maintenance pass.
        let effects = crate::drain_synced_memory(self.storage.connection())?;
        Ok((report, effects.nodes_removed))
    }

    /// Configure the active repo to contribute memories to `owner_account_hex` (paste flow, #1164):
    /// subsequent memory authoring targets that owner's stream via this account's Writer grant. The
    /// owner must `sync grant` this account (its id from `sync whoami`) and this store must sync
    /// the owner's log before authoring succeeds.
    pub fn sync_contribute(&self, owner_account_hex: &str) -> anyhow::Result<()> {
        crate::memory_write::set_contribution_owner(
            self.storage.connection(),
            owner_account_hex,
            rag_rat_base::time::now_ms(),
        )
    }

    /// Configure the active repo to MIRROR `owner_account_hex`'s published memories, read-only
    /// (#1156): this repo's memory tables materialize from the owner's stream instead of its own.
    /// No Writer grant is involved — a subscriber authors nothing onto the owner's stream — but the
    /// owner's log must reach this store (automatic sync pulls it once the owner's host is in
    /// `[sync] server_peers`, or `sync pull <owner>`) before anything materializes.
    /// Subscribe to an owner an OPERATOR named. Re-pins this repo's trust root: a human who
    /// obtained the id out of band is entitled to move it.
    pub fn sync_subscribe(&self, owner_account_hex: &str) -> anyhow::Result<()> {
        crate::memory_write::set_subscription_owner(
            self.storage.connection(),
            owner_account_hex,
            rag_rat_base::time::now_ms(),
            crate::memory_write::SubscribeTrust::Operator,
            crate::memory_write::SubscriptionRouting::default(),
        )
    }

    /// Subscribe to the owner a checked-in `.rag-rat-stream` names. Refuses when this repo is
    /// already pinned to a different owner — an editable in-repo file may establish a trust root,
    /// never move one.
    ///
    /// The locator's routing commits in the same transaction as the owner and pin.
    pub fn sync_subscribe_from_locator(
        &self,
        owner_account_hex: &str,
        peers: &[String],
        relay: Option<&str>,
    ) -> anyhow::Result<()> {
        crate::memory_write::set_subscription_owner(
            self.storage.connection(),
            owner_account_hex,
            rag_rat_base::time::now_ms(),
            crate::memory_write::SubscribeTrust::Locator,
            crate::memory_write::SubscriptionRouting { peers, relay },
        )
    }

    /// Peers recorded for subscribed owners, each with the relay its locator named.
    pub fn subscription_routing(
        &self,
        owner_hex: &str,
    ) -> anyhow::Result<Vec<(String, Option<String>)>> {
        crate::memory_write::subscription_routing(self.storage.connection(), owner_hex)
    }

    /// The owner this repo has pinned, if any — reported by `sync whoami` so an operator can see
    /// the trust root without reading repo_meta.
    pub fn stream_pin(&self) -> anyhow::Result<Option<String>> {
        crate::memory_write::stream_pin(self.storage.connection(), &self.active_repo_id)
    }

    /// Stop mirroring a subscribed owner (`sync unsubscribe`): the active repo's memories
    /// materialize from its OWN stream again. Returns whether a subscription was configured.
    ///
    /// The subscription REMOVED this account's other devices' memories (absent from the owner's
    /// projection, which the drain reads as condemned); clearing it makes the next drain
    /// re-materialize them, and removes the owner's in turn.
    pub fn sync_unsubscribe(&self) -> anyhow::Result<bool> {
        crate::memory_write::clear_subscription_owner(self.storage.connection())
    }

    /// Stop contributing to a configured owner (`sync uncontribute`): memory authoring for the
    /// active repo targets this store's own stream again. Returns whether a contribution was
    /// configured. The owner's Writer grant and the contributions already authored onto its stream
    /// are untouched.
    pub fn sync_uncontribute(&self) -> anyhow::Result<bool> {
        crate::memory_write::clear_contribution_owner(self.storage.connection())
    }

    /// The active repo's configured foreign memory owner, if any — what `sync whoami` reports
    /// alongside this store's account id.
    pub fn sync_owner_config(&self) -> anyhow::Result<crate::memory_write::RepoOwnerConfig> {
        crate::memory_write::repo_owner_config(self.storage.connection())
    }

    /// Re-wrap the active repo stream's existing live keys to an already-effective enrolled device.
    /// This authors same-key siblings only; it does not enroll, pair, transport, or rotate keys.
    pub fn sync_catch_up(
        &self,
        target: rag_rat_oplog::DeviceFingerprint,
    ) -> anyhow::Result<SyncCatchUpReport> {
        let report = crate::memory_write::catch_up_enrolled_device_keys(
            self.storage.connection(),
            target,
            rag_rat_base::time::now_ms(),
        )?;
        Ok(SyncCatchUpReport {
            target: report.target,
            required: report.authored.len() as u64,
            already_covered: report.already_covered.len() as u64,
            authored: report.authored.len() as u64,
        })
    }

    /// Materialize any accepted SYNCED `/3` content into the local memory tables — the reverse of
    /// the memory reconcile (#691 A1). Store-global (one pass per registered real repo); a repo
    /// with no minted account or no synced content is a cheap no-op. Called from the watcher's
    /// maintenance pass so a long-running process picks up content pulled AFTER open without a
    /// reopen (open and consolidate drain at their own seams). The eventual live sync-session
    /// driver should call `drain_synced_stream_for_repo` per session for immediacy; this pass
    /// is the backstop.
    pub fn drain_synced_memory(&self) -> anyhow::Result<()> {
        crate::memory_write::drain_synced_streams_for_all_repos(
            self.storage.connection(),
            rag_rat_base::time::now_ms(),
        )?;
        Ok(())
    }
}
