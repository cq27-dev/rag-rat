//! Translating persisted memories into signed op-log entries, and the per-node/edge reconcile that
//! keeps the log a COMPLETE signed mirror of `repo_memories` / `repo_node_edges` (#524, #541,
//! #664).
//!
//! This bridges `repo_memories` / `repo_node_edges` (owned by this module) and the op-log MINTING
//! primitives ([`crate::oplog`]) — a ONE-WAY dependency, so `oplog` never depends back on the
//! memory subsystem (a reverse call would cycle the build).
//!
//! OWNER-BOUND `/2`//3 substrate (#664). The live path authors owner-bound `/3` content on the
//! repo's owner-bound `/2` stream, under the store's single local account (minted once, store-
//! global). Each reconcile/mutation ensures the repo's `/2` stream is owned (publishing a
//! `StreamOwn` account op) and authors its ops as owner-authored `/3` content
//! ([`rag_rat_oplog::author_prepared_content_batch_in_tx`]), which verify-accepts and reprojects
//! into `content_projected_nodes` / `content_projected_edges`. The completeness predicate is an
//! anti-join against that accepted-`/3` projection; the pre-existing `/1` history is retained but
//! no longer written by the live path (existing `/1` rows are adopted into `/3` by the reconcile).
//!
//! WIRED into the live write path (#532): the memory mutations call [`backfill_memory_oplog`] once
//! (before the first live entry) and the `author_*` seams below INSIDE their own transaction, so
//! the op-append and the table write commit — or roll back — together (strict-atomic). Authoring is
//! a NO-OP under an unstable scope ([`stable_owner_stream`]) or before the local account is minted,
//! leaving scope-less callers untouched.
//!
//! [`backfill_memory_oplog`] is a per-node/edge RECONCILE (#541), not a per-chain gate: it authors
//! every table row MISSING from the accepted-`/3` projection, so a row that entered the tables
//! outside the wired path (a pre-#532 binary, a raw writer, a consolidation import, or pre-existing
//! `/1` history) is signed on the next mutation and no later lifecycle op on it is ever inert.
//! Genesis is just the empty-`/3`-chain case where every row is missing.

use std::collections::HashSet;

use anyhow::Context;
use rag_rat_query::memory::hydrate::tags_for_memory;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

/// Scoped durability bump for an AUTHORED write (#560). The index connection runs
/// `synchronous = NORMAL` — the right policy for the high-frequency, fully reconstructable
/// derived-index writes, where skipping the per-commit WAL fsync is a throughput win and the only
/// cost is that the last committed transaction can roll back on power loss (a re-index recovers
/// it).
///
/// Authored memory / op-log mutations are the OPPOSITE class: irreplaceable, low-frequency, and
/// they return success to the caller. They must not acknowledge under a mode that can silently lose
/// the last commit, so they raise `synchronous = FULL` (fsync the WAL on commit) for the duration
/// of their transaction and restore `NORMAL` on drop. The guard is held ACROSS the authored
/// `BEGIN .. COMMIT` and dropped after, so the commit fsyncs; restore runs on every path (including
/// error/panic), so a shared connection is never stranded at FULL — and a stray failure could only
/// leave it on the *safer*, slower setting, never a less durable one.
pub(super) struct AuthoredDurability<'a> {
    conn: &'a Connection,
}

impl<'a> AuthoredDurability<'a> {
    /// Raise `synchronous = FULL`. MUST be called OUTSIDE a transaction (SQLite only applies a
    /// `synchronous` change to subsequent transactions), i.e. immediately before the authored
    /// `BEGIN`/`unchecked_transaction`.
    pub(super) fn begin(conn: &'a Connection) -> anyhow::Result<Self> {
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        Ok(Self { conn })
    }
}

impl Drop for AuthoredDurability<'_> {
    fn drop(&mut self) {
        // Best-effort restore of the connection default (see the struct doc for why swallowing is
        // safe). Runs after the authored txn has committed/rolled back, so no transaction is open.
        let _ = self.conn.execute_batch("PRAGMA synchronous = NORMAL;");
    }
}
use rag_rat_oplog::{
    EdgeKey, EdgeSpec, MemoryOp, NodeContent, NodeId, NodeStatus, PreparedContentAuthoring,
    SealPolicy, StreamId,
};
use rag_rat_query::memory::{EdgeRelation, NodeEdge, RepoMemory, memory_repo_scope};

/// One memory's projectable content — the columns the op model carries (NOT the identity / anchor /
/// dedup bookkeeping). Read in bulk so the backfill makes one pass over `repo_memories`.
struct MemoryRow {
    memory_id: String,
    kind: String,
    title: String,
    body: String,
    confidence: String,
    status: String,
    source: String,
    payload_json: Option<String>,
    tags: Vec<String>,
}

/// The repo's memory-authoring ACCESS MODE — the one-way publish INTENT, persisted in repo meta.
/// This is the AUTHORING-side seed: the live-write sites are conn-only and the first memory write
/// mints the account + authors a `/2` StreamOwn, so the mode a `PublicRead` node must author under
/// cannot come from `Config` (unreachable at those sites) nor be derived from an empty op-log — it
/// is read from here. The op-log's StreamOwn set stays the SERVE-side truth
/// (`account_is_fully_public`).
const STREAM_ACCESS_MODE_META_KEY: &str = "memory_stream_access_mode";

/// The persisted access-mode intent for `repo_id`: `public` → `PublicRead`; absent → `Private` (the
/// default); any other token refuses to author (a malformed one-way ratchet must not silently
/// downgrade to a public or private write).
pub(crate) fn owner_stream_access_mode(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<rag_rat_oplog::AccessMode> {
    match rag_rat_db::meta::repo_meta(conn, repo_id, STREAM_ACCESS_MODE_META_KEY)?.as_deref() {
        Some("public") => return Ok(rag_rat_oplog::AccessMode::PublicRead),
        Some(other) => anyhow::bail!(
            "repo `{repo_id}` has unknown memory stream access mode `{other}`; refusing to author"
        ),
        None => {},
    }
    // Derived one-way ratchet (mirrors the seal policy's `content_stream_has_sealed_ratchet`): if
    // the account ALREADY owns this repo's `PublicRead` `/2` stream, stay public even when the
    // intent row is absent (deleted / a meta bug). Otherwise a write would resolve `Private`,
    // find the (distinct) Private-mode stream unowned, and author a SECOND `Private` StreamOwn
    // — permanently mixing the account (unservable forever; the control log is append-only).
    // Uses only plain reads (no nested transaction), so it is safe at every caller, including
    // those inside an open txn.
    if let Some(public_id) = rag_rat_oplog::owned_stream_v2_id_with_mode(
        conn,
        repo_id,
        rag_rat_oplog::AccessMode::PublicRead,
    )? && rag_rat_oplog::stream_owner_account(conn, public_id)?.is_some()
    {
        return Ok(rag_rat_oplog::AccessMode::PublicRead);
    }
    Ok(rag_rat_oplog::AccessMode::Private)
}

const STREAM_SEAL_POLICY_META_KEY: &str = "memory_stream_seal_policy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamSealPolicy {
    Plaintext,
    Sealed,
}

impl StreamSealPolicy {
    fn seal_policy(self) -> SealPolicy {
        match self {
            Self::Plaintext => SealPolicy::Plaintext,
            Self::Sealed => SealPolicy::Sealed,
        }
    }
}

pub(crate) struct PreparedOwnerAuthoring {
    repo_id: String,
    /// The `/2` stream to author onto — this store's own owned stream in `Owner` mode, the
    /// CONFIGURED owner's stream in `Grantee` mode.
    stream: StreamId,
    role: AuthoringRole,
}

/// How this store authors `/3` content for a repo. `Owner` is the default — the local account owns
/// the stream. `Grantee` (#1164) is a granted contributor authoring onto ANOTHER account's stream.
enum AuthoringRole {
    Owner { policy: StreamSealPolicy, prepared: PreparedContentAuthoring },
    Grantee { owner_account: rag_rat_oplog::AccountId, grant_id: [u8; 32] },
}

impl PreparedOwnerAuthoring {
    /// The owner-mode prepared `/3` batch. The reconcile paths build and use Owner-role handles
    /// only (a contributor skips reconcile via `backfill_memory_oplog`), so a Grantee handle
    /// here is a programming error, not a runtime condition.
    fn owner_prepared(&self) -> anyhow::Result<&PreparedContentAuthoring> {
        match &self.role {
            AuthoringRole::Owner { prepared, .. } => Ok(prepared),
            AuthoringRole::Grantee { .. } => anyhow::bail!(
                "reconcile is owner-only, but a grantee-role prepared handle reached it"
            ),
        }
    }
}

// A granted contributor's rows stay `origin='local'`, which is their AUTHORSHIP: this store wrote
// them, and `origin` is what `ImportMode::SeedPublic` reads to decide whose memories a public seed
// carries. It is deliberately NOT re-purposed to mean "under the drain's removal authority" — those
// two meanings diverge for exactly these rows (locally authored, yet projected on another account's
// stream), and one column cannot carry both.
//
// The consequence is bounded and accepted: when an authority refold condemns a contribution (a
// revoked grant, a device cut ordered before it), the owner's stream stops accepting and serving
// it, but the contributor keeps its own copy of its own writing. Content this store RECEIVED is
// `origin='synced'` and the drain's anti-join does remove it, so a revoke never leaves another
// account's condemned content readable here. Separating the two would need a third `origin` value
// (a CHECK rewrite on both tables) — worth it only once a case appears where a contributor must
// forget what it authored itself.

/// The `repo_meta` key holding the account this repo contributes memories to (the paste-flow owner
/// id, set by `sync contribute`). Absent = this store authors its own owner stream.
pub(crate) const CONTRIBUTION_OWNER_META_KEY: &str = "memory_contribution_owner";

/// The configured contribution-owner account for `repo_id`, or `None`. Stored as a 64-hex account
/// id.
/// Every `(repo_id, owner)` this store is configured to contribute to. Small by construction — one
/// entry per contributing repo — and the input to both the serve predicate (which grant matters)
/// and the private-stream guard (whether ANY repo is contributing).
pub(crate) fn contribution_targets(
    conn: &Connection,
) -> anyhow::Result<Vec<(String, rag_rat_oplog::AccountId)>> {
    let mut out = Vec::new();
    for repo_id in rag_rat_db::schema::real_repo_ids(conn)? {
        if let Some(owner) = contribution_owner_account(conn, &repo_id)? {
            out.push((repo_id, owner));
        }
    }
    Ok(out)
}

/// A configured owner key that will not parse — the ONE stream-resolution failure
/// [`repoint_authoritative_content_stream`] tolerates. Attached as context so the parse error's own
/// message survives in the chain, and so a genuine read failure (which carries no such context)
/// stays distinguishable from it.
#[derive(Debug, thiserror::Error)]
#[error("repo `{repo_id}`'s `{meta_key}` is not a 64-hex account id")]
struct UnparseableOwnerKey {
    repo_id: String,
    meta_key: &'static str,
}

pub(super) fn contribution_owner_account(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<rag_rat_oplog::AccountId>> {
    let Some(hex) = rag_rat_db::meta::repo_meta(conn, repo_id, CONTRIBUTION_OWNER_META_KEY)? else {
        return Ok(None);
    };
    let owner = rag_rat_oplog::AccountId::from_hex(&hex).map_err(|err| {
        err.context(UnparseableOwnerKey {
            repo_id: repo_id.to_string(),
            meta_key: CONTRIBUTION_OWNER_META_KEY,
        })
    })?;
    Ok(Some(owner))
}

/// The `repo_meta` key holding the account this repo MIRRORS read-only (set by `sync subscribe`,
/// #1156). Absent = this repo mirrors its own account's stream.
///
/// Read-only is the whole difference from [`CONTRIBUTION_OWNER_META_KEY`]: a subscriber authors
/// nothing onto the owner's stream, so it needs no Writer grant, and it is never pulled FROM, so
/// its own streams may stay private. Its own memories keep going to its OWN stream — only the drain
/// re-points.
const SUBSCRIPTION_OWNER_META_KEY: &str = "memory_subscription_owner";

/// The owner this repo has ever been told to trust, kept SEPARATELY from the live subscription so
/// it outlives `sync unsubscribe`.
///
/// Trust-on-first-use only works if the "first use" cannot be replayed. Were the pin the live
/// subscription key, the refusal below would be defeated by the very sequence its message would
/// otherwise suggest — unsubscribe, subscribe — and that sequence is what an agent handed a
/// fail-closed error will try. So the pin is written on the first locator-driven subscribe, left
/// behind by unsubscribe, and re-written ONLY when an operator names an account id themselves.
const STREAM_PIN_META_KEY: &str = "memory_stream_pin";

/// Routing to reach the subscribed owner's host, as the locator supplied it.
///
/// Persisted rather than merely echoed because the point of the locator is a clone that has NO
/// `[sync] server_peers`: a subscriber cannot discover a foreign account's host — that account's
/// discovery tag derives from its own secret — so without somewhere to keep these, the repo records
/// a subscription it can never fetch. Both the manual pull and the automatic cross-account pass
/// read them.
///
/// They carry NO authority and are safe to persist from an untrusted file: every entry pulled is
/// verified against the pinned account's signature chain, so a hostile entry here can waste a dial,
/// never forge content. One node id per line; the relay is a single URL.
const SUBSCRIPTION_PEERS_META_KEY: &str = "memory_subscription_peers";
const SUBSCRIPTION_RELAY_META_KEY: &str = "memory_subscription_relay";

/// Who chose the account being subscribed to — the whole basis of the pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscribeTrust {
    /// An operator passed the id on the command line, having obtained it out of band. A human
    /// asserting a trust root: it re-pins.
    Operator,
    /// A checked-in `.rag-rat-stream` supplied it. Untrusted input — anyone who can land a commit
    /// can change it — so it may establish a pin but never move one.
    Locator,
}

pub(super) fn subscription_owner_account(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<rag_rat_oplog::AccountId>> {
    let Some(hex) = rag_rat_db::meta::repo_meta(conn, repo_id, SUBSCRIPTION_OWNER_META_KEY)? else {
        return Ok(None);
    };
    let owner = rag_rat_oplog::AccountId::from_hex(&hex).map_err(|err| {
        err.context(UnparseableOwnerKey {
            repo_id: repo_id.to_string(),
            meta_key: SUBSCRIPTION_OWNER_META_KEY,
        })
    })?;
    Ok(Some(owner))
}

/// Every owner account this store SUBSCRIBES to. Unlike [`contribution_targets`] the repo id is not
/// carried: a subscription authors nothing and is never served, so the account is all the
/// foreign-pull enumeration — its only consumer — needs.
pub(crate) fn subscription_owners(
    conn: &Connection,
) -> anyhow::Result<Vec<rag_rat_oplog::AccountId>> {
    let mut out = Vec::new();
    for repo_id in rag_rat_db::schema::real_repo_ids(conn)? {
        out.extend(subscription_owner_account(conn, &repo_id)?);
    }
    Ok(out)
}

/// The active repo's configured FOREIGN memory owner, for `sync whoami`. At most one field is set —
/// contribution and subscription both re-point the repo's one authoritative stream, so configuring
/// the second is refused.
#[derive(Debug, Default)]
pub struct RepoOwnerConfig {
    pub contribution_owner_account_id: Option<String>,
    pub subscription_owner_account_id: Option<String>,
}

pub(crate) fn repo_owner_config(conn: &Connection) -> anyhow::Result<RepoOwnerConfig> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(RepoOwnerConfig::default());
    };
    let hex =
        |account: rag_rat_oplog::AccountId| rag_rat_base::hash::hex_lower(&account.to_bytes());
    Ok(RepoOwnerConfig {
        contribution_owner_account_id: contribution_owner_account(conn, &repo_id)?.map(hex),
        subscription_owner_account_id: subscription_owner_account(conn, &repo_id)?.map(hex),
    })
}

/// Whether this repo authors as a granted CONTRIBUTOR — an owner is configured and it is not this
/// store's own account. A light check (no grant lookup) so `backfill_memory_oplog` can cheaply skip
/// the owner-only establish/reconcile path; the grant is required (and its absence errors) later,
/// in [`grantee_context`] at prepare/author time.
fn is_contribution_mode(conn: &Connection, repo_id: &str) -> anyhow::Result<bool> {
    let Some(owner) = contribution_owner_account(conn, repo_id)? else {
        return Ok(false);
    };
    Ok(rag_rat_oplog::read_local_account(conn)? != Some(owner))
}

/// Refuse `operation` when `repo_id`'s memories materialize from ANOTHER account's stream — either
/// configuration reaches that state. Every operation that reconciles or imports rows into the repo
/// (legacy consolidation, `sync publish --seed`, memory import) needs the repo's own stream to be
/// the one [`super::drain::authoritative_content_stream`] honors: a contributor owns no such stream
/// at all, and a subscriber's is not the authority, so the imported `origin='synced'` rows are
/// condemned by the very next drain. All of them are irreversible enough that continuing on a
/// half-applied import is worse than stopping.
pub(crate) fn ensure_not_mirroring_another_account(
    conn: &Connection,
    repo_id: &str,
    operation: &str,
) -> anyhow::Result<()> {
    let local = rag_rat_oplog::read_local_account(conn)?;
    if let Some(owner) = contribution_owner_account(conn, repo_id)?
        && local != Some(owner)
    {
        anyhow::bail!(
            "repo `{repo_id}` is configured to contribute memories to account {}, so it owns no \
             memory stream of its own — {operation} is not supported in contribution mode",
            rag_rat_base::hash::hex_lower(&owner.to_bytes()),
        );
    }
    if let Some(owner) = subscription_owner_account(conn, repo_id)?
        && local != Some(owner)
    {
        anyhow::bail!(
            "repo `{repo_id}` mirrors account {}'s memories read-only, so its memory tables \
             materialize from that account's stream and rows imported here would be removed by \
             the next drain — {operation} is not supported while subscribed. Run `sync \
             unsubscribe` first",
            rag_rat_base::hash::hex_lower(&owner.to_bytes()),
        );
    }
    Ok(())
}

struct GranteeContext {
    owner_account: rag_rat_oplog::AccountId,
    stream: StreamId,
    grant_id: [u8; 32],
}

/// Resolve grantee-authoring context for `repo_id`: the configured contribution owner, its
/// `PublicRead` owner stream, and this store's effective Writer grant on it. `None` when not in
/// contribution mode. Errors (fail loud) when configured but the grant is missing — the operator
/// must `sync grant` this account from the owner and sync the owner's log first.
fn grantee_context(conn: &Connection, repo_id: &str) -> anyhow::Result<Option<GranteeContext>> {
    let Some(owner_account) = contribution_owner_account(conn, repo_id)? else {
        return Ok(None);
    };
    let local = rag_rat_oplog::read_local_account(conn)?;
    if local == Some(owner_account) {
        return Ok(None);
    }
    let local = local.context(
        "contribution mode requires a local account; index or sync this repo before authoring",
    )?;
    // v1 contribution targets a published (public_read) owner stream — no content-key wraps needed.
    let stream = rag_rat_oplog::owner_stream_v2_id_for_account(
        repo_id,
        owner_account,
        rag_rat_oplog::AccessMode::PublicRead,
    )?;
    let grant_id = rag_rat_oplog::effective_writer_grant(conn, owner_account, stream, local)?
        .with_context(|| {
            format!(
                "no effective writer grant for this account on repo `{repo_id}`'s owner stream — \
                 the owner must `sync grant` this account (its id from `sync whoami`), and this \
                 store needs the owner's log: automatic sync pulls it once the owner's host is in \
                 [sync] server_peers, or run `rag-rat sync pull <owner-account>` now"
            )
        })?;
    Ok(Some(GranteeContext { owner_account, stream, grant_id }))
}

/// Refuse a `sync contribute` on a repo that already subscribes.
///
/// Run TWICE by each setter: once before the transaction, so this specific conflict is what the
/// operator is told about rather than a later, blunter guard; and again INSIDE it, because the two
/// setters read what the other writes — checked only outside, two concurrent configures each
/// observe the other's absence and commit, leaving BOTH keys set, which neither setter could then
/// correct (each refuses on account of the other) and which the drain resolves silently in
/// contribution's favor. Both setters open `BEGIN IMMEDIATE`, so the in-transaction read serializes
/// them; the outside one only buys the better message.
fn ensure_no_subscription_configured(conn: &Connection, repo_id: &str) -> anyhow::Result<()> {
    if let Some(subscribed) = subscription_owner_account(conn, repo_id)? {
        anyhow::bail!(
            "repo `{repo_id}` already subscribes to account {} — subscription and contribution \
             both re-point the ONE stream that materializes this repo's memories, so only one can \
             be configured. Run `sync unsubscribe` first, or contribute from a separate index",
            rag_rat_base::hash::hex_lower(&subscribed.to_bytes()),
        );
    }
    Ok(())
}

/// Refuse a `sync subscribe` on a repo that already contributes — the twin of
/// [`ensure_no_subscription_configured`], run at the same two points and for the same reasons.
///
/// Refuse rather than supersede: silently clearing a contribution would discard the Writer-grant
/// setup behind it and leave already-authored contributions looking unconfigured, which the
/// operator has to notice rather than have decided for them.
fn ensure_no_contribution_configured(conn: &Connection, repo_id: &str) -> anyhow::Result<()> {
    if let Some(contributing) = contribution_owner_account(conn, repo_id)? {
        anyhow::bail!(
            "repo `{repo_id}` already contributes its memories to account {} — contribution and \
             subscription both re-point the ONE stream that materializes this repo's memories, so \
             only one can be configured. Run `sync uncontribute` first, or subscribe from a \
             separate index to mirror a different owner",
            rag_rat_base::hash::hex_lower(&contributing.to_bytes()),
        );
    }
    Ok(())
}

/// Configure the ACTIVE repo to contribute memories to `owner_account_hex` (paste flow, #1164):
/// record the owner id so subsequent memory authoring targets the owner's stream via this account's
/// Writer grant. Mints this store's local account (the identity the owner grants). Requires a
/// stable repo scope and an owner id distinct from this account. The grant itself must be issued by
/// the owner (`sync grant`) and synced before authoring succeeds.
pub(crate) fn set_contribution_owner(
    conn: &Connection,
    owner_account_hex: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    let repo_id =
        memory_repo_scope(conn)?.context("sync contribute requires an active repo scope")?;
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        anyhow::bail!("sync contribute requires a stable repo identity (not legacy or local-only)");
    }
    ensure_no_subscription_configured(conn, &repo_id)?;
    let owner = rag_rat_oplog::AccountId::from_hex(owner_account_hex)?;
    let local = rag_rat_oplog::local_account(conn, now_ms)?;
    anyhow::ensure!(
        owner != local,
        "cannot contribute to your own account — the owner is a SEPARATE identity (its id from \
         the owner's `sync whoami`)"
    );
    ensure_contributor_account_is_servable(conn, local)?;

    let canonical = rag_rat_base::hash::hex_lower(&owner.to_bytes());

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    anyhow::ensure!(
        memory_repo_scope(&tx)?.as_deref() == Some(repo_id.as_str()),
        "active repo scope changed while starting sync contribute; retry"
    );
    ensure_no_subscription_configured(&tx, &repo_id)?;
    // Re-read UNDER the write lock, like the conflict guard above and for the same reason: read
    // only outside, a memory write in a SECOND repo of this index can establish that repo's
    // private stream (`ensure_owner_stream` takes its own IMMEDIATE lock) between this check and
    // the commit, and contribute would win the race into exactly the contributing-plus-private
    // state `ensure_owner_stream` exists to forbid.
    ensure_contributor_account_is_servable(&tx, local)?;
    repoint_authoritative_content_stream(&tx, &repo_id, StreamResolution::Strict, |tx| {
        rag_rat_db::meta::set_repo_meta(tx, &repo_id, CONTRIBUTION_OWNER_META_KEY, &canonical)
            .map_err(Into::into)
    })?;
    tx.commit()?;
    Ok(())
}

/// A contributor is reachable only while its account is publicly servable: content is served by its
/// AUTHOR account and the owner is not enrolled here, so if this account holds ANY private stream
/// the owner can never pull what this store authors — the contributions would be authored, accepted
/// on the owner's stream, and permanently unreachable.
///
/// Refuse at configure time rather than let it fail invisibly later. A control log cannot be served
/// as a subset (it is one hash chain), so there is no way to expose the roster while withholding
/// the private stream metadata; a dedicated store is the only correct answer. `sync publish` guards
/// the same property for OWNERS, but a contributor never publishes, so that check never runs on
/// this path.
fn ensure_contributor_account_is_servable(
    conn: &Connection,
    local: rag_rat_oplog::AccountId,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        rag_rat_oplog::account_is_fully_public(conn, local)?,
        "this account owns private memory streams (from other repos in this index), so a granted \
         owner could never fetch what you contribute — an account is servable to a peer only when \
         all of its streams are public, and a control log cannot be served in part. Contribute \
         from a dedicated index instead: `rag-rat init --database <path-to-a-fresh-index>` in \
         this checkout, then run `sync contribute` there"
    );
    Ok(())
}

/// How a re-point resolves the streams whose drain watermarks it forgets.
enum StreamResolution {
    /// A SETTER: an unreadable owner key is a state the configure must not write over silently.
    Strict,
    /// A CLEAR: the unreadable owner key is precisely what is being removed, and a side that
    /// cannot resolve had no stream to drain in the first place. Tolerating THAT — and only that,
    /// see [`UnparseableOwnerKey`] — is what keeps the recovery command usable in the one state
    /// that needs it.
    BestEffort,
}

/// Apply `repoint` — the CONFIGURATION write that changes which stream
/// [`super::drain::authoritative_content_stream`] names for `repo_id` — and forget the drain
/// watermark of the stream on BOTH sides of it.
///
/// Only a FULL drain pass runs the removal anti-joins, and a watermark that is still current
/// short-circuits the pass entirely. So the INCOMING stream's watermark must go, or the new
/// authority materializes nothing; and the OUTGOING one's must go too, or the rows the re-point
/// condemns are gone for good — re-pointing back would find its watermark current and restore
/// nothing. Nothing drains a stream while it is not the authority, so clearing its watermark costs
/// only the one full pass that a re-point back needs anyway.
///
/// Both sides resolve through the drain's own helper, so the two can never disagree about which
/// stream the re-point moved away from.
///
/// [`enable_public_authoring`] also moves the repo's authoritative stream (Private ⇒ PublicRead)
/// without coming through here. That path is watermark-safe on its own: it refuses unless the
/// account is fully public, so the outgoing Private stream is one nothing was ever authored or
/// ingested onto and it holds no synced rows to condemn.
fn repoint_authoritative_content_stream(
    tx: &Transaction<'_>,
    repo_id: &str,
    resolution: StreamResolution,
    repoint: impl FnOnce(&Transaction<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let resolve = |tx: &Transaction<'_>| -> anyhow::Result<Option<StreamId>> {
        match super::drain::authoritative_content_stream(tx, repo_id) {
            Ok(stream) => Ok(stream),
            // Tolerate the ONE failure the unset is itself the cure for. Every other error is real
            // and must roll the command back: skipping a watermark clear on a read failure loses
            // the clear silently, and on the INCOMING side that watermark is the stream the repo
            // moves BACK to — the clear that makes the memories the re-point removed reappear.
            Err(err)
                if matches!(resolution, StreamResolution::BestEffort)
                    && err.downcast_ref::<UnparseableOwnerKey>().is_some() =>
            {
                tracing::warn!(
                    repo_id,
                    error = format!("{err:#}"),
                    "skipping a drain-watermark clear: the configured owner key does not parse",
                );
                Ok(None)
            },
            Err(err) => Err(err),
        }
    };
    if let Some(outgoing) = resolve(tx)? {
        rag_rat_oplog::clear_content_drain_watermark(tx, outgoing)?;
    }
    repoint(tx)?;
    if let Some(incoming) = resolve(tx)? {
        rag_rat_oplog::clear_content_drain_watermark(tx, incoming)?;
    }
    Ok(())
}

/// Configure the ACTIVE repo to MIRROR `owner_account_hex`'s published memories, read-only
/// (`sync subscribe`, #1156): record the owner id so this repo's memory tables materialize from the
/// owner's stream instead of its own.
///
/// The two guards `sync contribute` carries deliberately do NOT apply here, because both exist only
/// because a contributor AUTHORS content the owner must later fetch. A subscriber writes nothing to
/// the owner's stream, so it needs no Writer grant; and it is only ever the puller, never pulled
/// from, so whether its own streams are private is irrelevant. What it shares with contribution is
/// the re-point itself: the owner's stream REPLACES this repo's own as its one authoritative
/// content stream (see `drain::authoritative_content_stream`), so while subscribed this repo stops
/// draining its own account's stream and sibling-device sync for it pauses. This store's own
/// memories keep being authored onto its OWN stream and stay `origin='local'`, which the drain's
/// synced-only removal anti-joins spare.
pub(crate) fn set_subscription_owner(
    conn: &Connection,
    owner_account_hex: &str,
    now_ms: i64,
    trust: SubscribeTrust,
) -> anyhow::Result<()> {
    let repo_id =
        memory_repo_scope(conn)?.context("sync subscribe requires an active repo scope")?;
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        anyhow::bail!("sync subscribe requires a stable repo identity (not legacy or local-only)");
    }
    ensure_no_contribution_configured(conn, &repo_id)?;
    let owner = rag_rat_oplog::AccountId::from_hex(owner_account_hex)?;
    let local = rag_rat_oplog::local_account(conn, now_ms)?;
    anyhow::ensure!(
        owner != local,
        "cannot subscribe to your own account — this repo already mirrors its own stream (the \
         owner is a SEPARATE identity, its id from the owner's `sync whoami`)"
    );

    let canonical = rag_rat_base::hash::hex_lower(&owner.to_bytes());

    // A locator may establish this repo's trust root, never move it. Moving it is how an edited
    // checked-in file would silently re-point a subscriber onto another account's stream — and a
    // re-point is destructive, not merely redirecting: the drain removes the previous owner's
    // mirrored rows, and the local binding work on them (a `memory rebind`, any local edge) does
    // not come back on unsubscribe. The remedy names a human step rather than a command, because
    // the point of the override is that the operator obtains the id from the owner, not from the
    // file that just changed.
    if trust == SubscribeTrust::Locator
        && let Some(pinned) = rag_rat_db::meta::repo_meta(conn, &repo_id, STREAM_PIN_META_KEY)?
        && pinned != canonical
    {
        anyhow::bail!(
            "`{}` names owner {canonical}, but this repo is pinned to {pinned}.\n\nA checked-in \
             locator cannot re-point a subscription. Confirm the new id with the stream's owner \
             out of band, then subscribe to it explicitly:\n\n    rag-rat sync subscribe \
             {canonical}",
            rag_rat_base::stream_locator::STREAM_LOCATOR_FILE,
        );
    }

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    anyhow::ensure!(
        memory_repo_scope(&tx)?.as_deref() == Some(repo_id.as_str()),
        "active repo scope changed while starting sync subscribe; retry"
    );
    ensure_no_contribution_configured(&tx, &repo_id)?;
    repoint_authoritative_content_stream(&tx, &repo_id, StreamResolution::Strict, |tx| {
        rag_rat_db::meta::set_repo_meta(tx, &repo_id, SUBSCRIPTION_OWNER_META_KEY, &canonical)
            .map_err(Into::into)
    })?;
    // Written under the same transaction as the subscription it authorizes, so a torn write cannot
    // leave a repo subscribed to an owner it never pinned.
    rag_rat_db::meta::set_repo_meta(&tx, &repo_id, STREAM_PIN_META_KEY, &canonical)?;
    tx.commit()?;
    Ok(())
}

/// The owner this repo has pinned, if any. Survives `sync unsubscribe`.
pub(crate) fn stream_pin(conn: &Connection, repo_id: &str) -> anyhow::Result<Option<String>> {
    Ok(rag_rat_db::meta::repo_meta(conn, repo_id, STREAM_PIN_META_KEY)?)
}

/// Record how to reach the subscribed owner's host. Empty peers CLEAR the record rather than
/// leaving a previous owner's routing behind to be dialed for a different account.
pub(crate) fn set_subscription_routing(
    conn: &Connection,
    peers: &[String],
    relay: Option<&str>,
) -> anyhow::Result<()> {
    let repo_id =
        memory_repo_scope(conn)?.context("recording subscription routing requires a repo scope")?;
    let joined = peers.join("\n");
    if joined.is_empty() {
        rag_rat_db::meta::delete_repo_meta(conn, &repo_id, SUBSCRIPTION_PEERS_META_KEY)?;
    } else {
        rag_rat_db::meta::set_repo_meta(conn, &repo_id, SUBSCRIPTION_PEERS_META_KEY, &joined)?;
    }
    match relay {
        Some(relay) if !relay.trim().is_empty() =>
            rag_rat_db::meta::set_repo_meta(conn, &repo_id, SUBSCRIPTION_RELAY_META_KEY, relay)?,
        _ => rag_rat_db::meta::delete_repo_meta(conn, &repo_id, SUBSCRIPTION_RELAY_META_KEY)?,
    };
    Ok(())
}

/// Peers and relay recorded for the subscribed owner, across every repo in this store.
///
/// Store-wide rather than repo-scoped because the cross-account pull pass is store-wide: it pulls
/// each foreign account once, not once per repo, so it needs every repo's routing pooled.
pub(crate) fn subscription_routing(
    conn: &Connection,
) -> anyhow::Result<(Vec<String>, Option<String>)> {
    let mut peers = Vec::new();
    let mut relay = None;
    for repo_id in rag_rat_db::schema::real_repo_ids(conn)? {
        if let Some(recorded) =
            rag_rat_db::meta::repo_meta(conn, &repo_id, SUBSCRIPTION_PEERS_META_KEY)?
        {
            peers.extend(
                recorded.lines().map(str::trim).filter(|p| !p.is_empty()).map(String::from),
            );
        }
        if relay.is_none() {
            relay = rag_rat_db::meta::repo_meta(conn, &repo_id, SUBSCRIPTION_RELAY_META_KEY)?;
        }
    }
    peers.sort_unstable();
    peers.dedup();
    Ok((peers, relay))
}

/// Stop mirroring another account (`sync unsubscribe` / `sync uncontribute`): drop the configured
/// owner so this repo's memories materialize from its OWN stream again. Returns whether a
/// configuration was actually cleared.
///
/// The re-point that configured the owner REMOVED every `origin='synced'` row that arrived from
/// this account's other devices — they are absent from the owner's projection, and the drain reads
/// that as condemned. Going through [`repoint_authoritative_content_stream`] is what makes the
/// removal recoverable: the own stream's watermark is forgotten, so the next drain makes a full
/// pass and re-materializes them from its projection. What does NOT come back is checkout-local
/// binding work — `drain::seed_node_anchors` seeds only the author's published anchors, and only
/// for a memory this store holds no bindings for, so a `memory rebind` made here is lost with the
/// row, as is any `origin='local'` edge that FK'd it.
///
/// Stream resolution here is BEST-EFFORT, unlike the setters': an owner key that will not parse is
/// exactly what this command removes, and a side that cannot resolve had no stream to drain — a
/// strict resolution would make the recovery command unusable in the state that most needs it.
fn clear_foreign_owner(conn: &Connection, meta_key: &str, command: &str) -> anyhow::Result<bool> {
    let repo_id = memory_repo_scope(conn)?
        .with_context(|| format!("{command} requires an active repo scope"))?;

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    anyhow::ensure!(
        memory_repo_scope(&tx)?.as_deref() == Some(repo_id.as_str()),
        "active repo scope changed while starting {command}; retry"
    );
    if rag_rat_db::meta::repo_meta(&tx, &repo_id, meta_key)?.is_none() {
        return Ok(false);
    }
    repoint_authoritative_content_stream(&tx, &repo_id, StreamResolution::BestEffort, |tx| {
        rag_rat_db::meta::delete_repo_meta(tx, &repo_id, meta_key).map_err(Into::into)
    })?;
    tx.commit()?;
    Ok(true)
}

/// Stop mirroring a subscribed owner (`sync unsubscribe`).
pub(crate) fn clear_subscription_owner(conn: &Connection) -> anyhow::Result<bool> {
    clear_foreign_owner(conn, SUBSCRIPTION_OWNER_META_KEY, "sync unsubscribe")
}

/// Stop contributing to a configured owner (`sync uncontribute`). The owner's Writer grant is
/// untouched — only this store's routing changes — and the contributions already authored onto the
/// owner's stream stay there; this store keeps its own `origin='local'` copies of them.
pub(crate) fn clear_contribution_owner(conn: &Connection) -> anyhow::Result<bool> {
    clear_foreign_owner(conn, CONTRIBUTION_OWNER_META_KEY, "sync uncontribute")
}

fn explicit_stream_seal_policy(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<StreamSealPolicy>> {
    match rag_rat_db::meta::repo_meta(conn, repo_id, STREAM_SEAL_POLICY_META_KEY)?.as_deref() {
        None => Ok(None),
        Some("sealed") => Ok(Some(StreamSealPolicy::Sealed)),
        Some(other) => anyhow::bail!(
            "repo `{repo_id}` has unknown memory stream seal policy `{other}`; refusing to author"
        ),
    }
}

fn stream_seal_policy(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<StreamSealPolicy> {
    if explicit_stream_seal_policy(conn, repo_id)? == Some(StreamSealPolicy::Sealed)
        || rag_rat_oplog::content_stream_has_sealed_ratchet(conn, stream)?
    {
        Ok(StreamSealPolicy::Sealed)
    } else {
        Ok(StreamSealPolicy::Plaintext)
    }
}

/// Build the op model's content register from a memory's projectable columns — shared by the
/// reconcile ([`node_ops`]) and the live create/update authors, so all three agree byte-for-byte.
fn node_content(
    kind: &str,
    title: &str,
    body: &str,
    confidence: &str,
    source: &str,
    tags: &[String],
    payload_json: Option<&str>,
) -> NodeContent {
    NodeContent {
        kind: kind.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        confidence: confidence.to_string(),
        source: source.to_string(),
        tags: tags.to_vec(),
        payload: payload_json.map(str::to_string),
    }
}

/// A memory's NODE ops: a `NodeCreate` for content, then a `NodeStatus`.
///
/// `elide_active_status = true` (GENESIS on an empty chain, no stale registers) emits the status op
/// ONLY when non-active — the fold's create-time default handles `active`, so genesis stays
/// byte-identical to the pre-#541 backfill. `false` (INCREMENTAL heal on a non-empty chain) ALWAYS
/// emits `NodeStatus`, so a healed node's status wins its register at the new, higher Lamport even
/// if an inert `NodeStatus` from an old binary left a stale value in it (the fold's status register
/// is independent of existence — a `NodeCreate` never touches it — so authoring only the create
/// would let that stale register surface; see decision 6 of #541).
///
/// An unrecognized status token FAILS — a signed op cannot be corrected, and coercing to `active`
/// would permanently mint the wrong status into the immutable history. Code-anchor BINDINGS are
/// excluded — per-device derived resolution state, never part of the shared node graph.
fn node_ops(row: &MemoryRow, elide_active_status: bool) -> anyhow::Result<Vec<MemoryOp>> {
    let node_id = NodeId::from(row.memory_id.as_str());
    let mut ops = vec![MemoryOp::NodeCreate {
        node_id: node_id.clone(),
        content: node_content(
            &row.kind,
            &row.title,
            &row.body,
            &row.confidence,
            &row.source,
            &row.tags,
            row.payload_json.as_deref(),
        ),
    }];
    let is_active = row.status == NodeStatus::default().as_db_str();
    if !(elide_active_status && is_active) {
        let status = NodeStatus::from_db_str(&row.status).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot author memory `{}`: unknown status token `{}` (a newer binary must author \
                 this history)",
                row.memory_id,
                row.status
            )
        })?;
        ops.push(MemoryOp::NodeStatus { node_id, status });
    }
    Ok(ops)
}

/// One `EdgeAdd` — presence + the durable, RE-RESOLVED spec only. `edge.target_repo_id` is already
/// current: `unauthored_edges`'s `reresolve_on_read` repaired the add-time snapshot before the
/// reconcile signs it (a signed op cannot be corrected later). Deliberately NO `Rebind`: the
/// `Rebind` op's resolved dimension (`target_node_id`, `anchor_status`) is PER-DEVICE derived state
/// recomputed on every read by `reresolve_on_read`, so signing it would bake one device's view into
/// the immutable shared history — excluded for the same reason code-anchor BINDINGS are.
fn edge_add_op(edge: &NodeEdge, owner_repo_id: &str) -> anyhow::Result<MemoryOp> {
    Ok(MemoryOp::EdgeAdd {
        edge: EdgeSpec {
            source_node_id: NodeId::from(edge.source_node_id.as_str()),
            relation: EdgeRelation::from_db_str(&edge.relation)?,
            target_repo_id: edge.target_repo_id.clone(),
            target_kind: edge.target_kind.clone(),
            target_anchor: edge.target_anchor.clone(),
            owner_repo_id: owner_repo_id.to_string(),
        },
    })
}

/// The ordered reconcile batch. Missing edges are grouped by `source_node_id`; for each missing
/// memory in `(created_at_ms, id)` order it emits [`node_ops`] then that memory's missing edges (in
/// the `edge_key` order [`unauthored_edges`] returned), then a final pass for edges whose source is
/// an already-authored node. On an EMPTY projection with `elide_active_status = true` this is
/// byte-identical to today's genesis sequence: every source memory is missing (`FK ON DELETE
/// CASCADE` on `source_node_id` rules out an orphan edge), so the final pass is empty and each
/// memory's edges follow its `NodeCreate`/`NodeStatus` in `edge_key` order.
fn build_reconcile_ops(
    conn: &Connection,
    missing_nodes: &[MemoryRow],
    missing_edges: &[NodeEdge],
    anchor_backfill_ops: &[MemoryOp],
    owner_repo_id: &str,
    policy: StreamSealPolicy,
    elide_active_status: bool,
) -> anyhow::Result<Vec<MemoryOp>> {
    use std::collections::BTreeMap;
    let mut by_source: BTreeMap<&str, Vec<&NodeEdge>> = BTreeMap::new();
    for edge in missing_edges {
        by_source.entry(edge.source_node_id.as_str()).or_default().push(edge);
    }
    let mut ops = Vec::new();
    for row in missing_nodes {
        ops.extend(node_ops(row, elide_active_status)?);
        // The backfill leg has to publish anchors too, or every memory that predates the anchor op
        // replicates with none and a peer seeds nothing for it — permanently, since this anti-join
        // never revisits a node once it exists. That covers the existing corpus on every upgraded
        // store, and `sync publish --seed`, which reconciles a whole index onto a public stream.
        //
        // An unpublishable set is dropped rather than quarantining the node: anchors are
        // decoration, and losing them must never cost a peer the memory itself.
        if let Some(op) = anchors_op(conn, &row.memory_id)?
            && content_op_is_authorable(&op, policy)
        {
            ops.push(op);
        }
        if let Some(op) = source_hash_op(conn, &row.memory_id)?
            && content_op_is_authorable(&op, policy)
        {
            ops.push(op);
        }
        if let Some(group) = by_source.remove(row.memory_id.as_str()) {
            for edge in group {
                ops.push(edge_add_op(edge, owner_repo_id)?);
            }
        }
    }
    // Lone ghost edges whose source node was already authored (absent on a genesis projection).
    for (_source, group) in by_source {
        for edge in group {
            ops.push(edge_add_op(edge, owner_repo_id)?);
        }
    }
    // Anchors for memories whose node was authored before this op kind existed — the corpus the
    // node anti-join above can never revisit. Already partitioned by authorability at read time,
    // so an unpublishable set was warned about there and never reaches this batch.
    ops.extend(anchor_backfill_ops.iter().cloned());
    Ok(ops)
}

/// Whether `row`'s `NodeCreate` fits the signed `/3` content envelope. A normal rag-rat memory
/// (title ≤ 160 chars, body ≤ 8 000 chars, payload capped by [`validate_payload`]) always fits;
/// only a raw writer / import / pre-cap ghost with an oversized body or payload can fail this, and
/// such a row is QUARANTINED rather than allowed to wedge the whole batch (#680). The status/edge
/// ops are tiny and never oversized, so the `NodeCreate` alone decides a node's authorability.
fn node_is_authorable(row: &MemoryRow, policy: StreamSealPolicy) -> bool {
    let op = MemoryOp::NodeCreate {
        node_id: NodeId::from(row.memory_id.as_str()),
        content: node_content(
            &row.kind,
            &row.title,
            &row.body,
            &row.confidence,
            &row.source,
            &row.tags,
            row.payload_json.as_deref(),
        ),
    };
    content_op_is_authorable(&op, policy)
}

/// Whether `edge`'s `EdgeAdd` fits the signed `/3` content envelope. The edge twin of
/// [`node_is_authorable`] (#680): a normal edge (short ids + a resolved node/github anchor) always
/// fits, and the write path now caps `target_anchor` / `target_repo_id`, so only a raw writer /
/// pre-cap import / consolidation-remapped ghost with an oversized free-form field can fail this —
/// and such an edge is QUARANTINED rather than allowed to make the whole reconcile `bail!` and
/// wedge every other memory write. An edge whose relation TOKEN this binary can't map is a
/// DIFFERENT (forward-compat) failure — [`build_reconcile_ops`] surfaces it loudly via
/// [`edge_add_op`], exactly as an unknown node status does — so a build error counts as authorable
/// here and defers to that path rather than silently quarantining it.
fn edge_is_authorable(edge: &NodeEdge, owner_repo_id: &str, policy: StreamSealPolicy) -> bool {
    match edge_add_op(edge, owner_repo_id) {
        Ok(op) => content_op_is_authorable(&op, policy),
        Err(_) => true,
    }
}

fn content_op_is_authorable(op: &MemoryOp, policy: StreamSealPolicy) -> bool {
    match policy {
        StreamSealPolicy::Plaintext => rag_rat_oplog::content_op_is_authorable(op),
        StreamSealPolicy::Sealed => rag_rat_oplog::content_op_is_sealed_authorable(op),
    }
}

/// The reconcile's missing set, split into what CAN be signed and what must be QUARANTINED (#680).
/// `live_edges` already excludes any edge whose SOURCE node is quarantined — an `EdgeAdd` with no
/// authored `NodeCreate` for its source would project a dangling edge — AND any edge whose OWN
/// `EdgeAdd` is oversized (those go to `quarantined_edges`).
struct ReconcileWork {
    authorable_nodes: Vec<MemoryRow>,
    live_edges: Vec<NodeEdge>,
    quarantined_nodes: Vec<MemoryRow>,
    quarantined_edges: Vec<NodeEdge>,
    /// Snapshots for already-authored memories still owed one — see [`read_anchor_backfill_ids`].
    anchor_backfill_ops: Vec<MemoryOp>,
    /// Memories the sweep selected whose snapshot will not fit a signed entry. Like a quarantined
    /// node, these are deliberately NOT work: re-selecting them forever is what would spin the
    /// slow path.
    quarantined_anchor_ids: Vec<String>,
}

impl ReconcileWork {
    /// Any AUTHORABLE work remaining. A quarantined row is intentionally NOT work: it never becomes
    /// authorable on its own, so counting it would spin the reconcile's slow path forever (#680).
    fn has_authorable_work(&self) -> bool {
        !self.authorable_nodes.is_empty()
            || !self.live_edges.is_empty()
            || !self.anchor_backfill_ops.is_empty()
    }

    /// Surface every quarantined node, edge, and anchor set as a warning naming the repo + the
    /// row's id — the per-row failure the caller can act on, in place of the old fail-loud that
    /// wedged the store.
    fn warn_quarantined(&self, repo_id: &str) {
        for row in &self.quarantined_nodes {
            tracing::warn!(
                repo_id,
                memory_id = %row.memory_id,
                "quarantining an un-authorable memory row: its signed /3 envelope exceeds the §18a \
                 256 KiB cap, so it is skipped to keep the memory-write path live; shrink or delete \
                 it through the public API to recover it (#680)",
            );
        }
        for memory_id in &self.quarantined_anchor_ids {
            tracing::warn!(
                repo_id,
                memory_id = %memory_id,
                "not publishing a memory's anchor set: it exceeds the anchor-count or signed-entry \
                 cap, so the memory replicates without its bindings and a peer cannot seed them; \
                 reduce or shorten its bindings through the public API to recover it",
            );
        }
        for edge in &self.quarantined_edges {
            tracing::warn!(
                repo_id,
                edge_key = %edge.edge_key,
                source_node_id = %edge.source_node_id,
                "quarantining an un-authorable node-edge: its signed /3 envelope exceeds the §18a \
                 256 KiB cap, so it is skipped to keep the memory-write path live; remove it \
                 through the public API to recover it (#680)",
            );
        }
    }
}

/// The pending-fold barrier (#698): memory completeness may not be read while the owner stream
/// owes a deferred content refold, because the accepted-`/3` projection is stale until settle.
///
/// The debt is settled INSIDE the caller's own IMMEDIATE transaction, immediately before the
/// authoritative re-read, and never on an autocommit connection beforehand. A foreign `/3`
/// candidate may target the LOCAL owner stream, so a remote peer can re-enqueue debt on the
/// largest stream in the store at will; draining it ahead of the transaction meant an unbudgeted
/// refold of the whole local memory history on the interactive write path, and a trip observed
/// inside an already-open transaction could only hard-error (#798 adversarial findings 2 and 5).
/// Settling in-transaction bounds the cost to ONE fold per local write — work the write's own
/// authoring performs anyway — and lets a mid-write enqueue self-heal. A fold failure propagates
/// and rolls the write back, so the barrier stays fail-closed.
fn settle_owner_stream_in_tx(
    tx: &Transaction<'_>,
    stream: StreamId,
    now_ms: i64,
) -> anyhow::Result<()> {
    rag_rat_oplog::settle_pending_content_refold_for_stream_in_tx(tx, stream, now_ms)
        .context("settling the owner stream's pending content refold before reading completeness")
}

/// How many anchor snapshots one reconcile pass PUBLISHES. The pass rides every authored write, so
/// an unbounded sweep would make the first write after an upgrade pay for the whole corpus;
/// bounded, it converges over successive writes and each pass stays cheap.
const ANCHOR_BACKFILL_PER_PASS: usize = 64;

/// How many candidates one pass EXAMINES to find that many publishable ones.
///
/// The two differ because a quarantined memory never leaves the match set — `anchors_json` stays
/// NULL by design — and it sorts oldest-first, which is exactly where the window is: an over-cap
/// set can only be a legacy row, since the live write path refuses one. Taking the window as the
/// publish budget would let enough of them permanently occupy it and stall the backfill for the
/// rest of the corpus. Examining wider and stopping at the publish budget means quarantined rows
/// cost a slot in the scan, never one in the batch.
const ANCHOR_BACKFILL_SCAN_PER_PASS: i64 = 512;

/// Memories whose node is ALREADY authored but whose anchors were never published — the corpus
/// that predates the anchor op on any store that was already syncing.
///
/// This is the other half of the backfill. The node anti-join only revisits memories missing from
/// the projection, so a memory authored before the op existed is never reconsidered by it, and
/// without this its bindings would never reach a peer.
///
/// Idempotent by construction: authoring the snapshot refolds the stream in the same transaction,
/// so `anchors_json` stops being NULL and the row drops out of this query. A memory with no
/// bindings never matches at all, so an unanchored memory is not re-examined forever.
///
/// Note what the column tracks: PRESENCE, not currency. This heals `none -> some` exactly once. It
/// cannot heal `some -> different`, and the relocation engine does rewrite portable anchor identity
/// outside any authoring path, so a renamed symbol leaves a peer holding the pre-rename set until
/// an explicit rebind re-authors it. Republish-on-drift is a separate mechanism, not this one.
///
/// The match set is `anchors_json IS NULL` only. A memory whose anchors were published BEFORE the
/// source-hash op existed has left it for good, so its hash is never swept and the peer holds the
/// anchors unmarked. Widening this to also select on `source_text_hash IS NULL` would not fix it:
/// the receiver applies a hash only where it seeds the anchors (`stamp_seeded_source_hash`), and a
/// peer that already holds them seeds nothing. Healing the pair needs a republish path that
/// re-seeds both together. No released version authored anchors, so the exposure is stores that ran
/// an unreleased build of the anchor op.
fn read_anchor_backfill_ids(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        // `origin = 'local'` for the same load-bearing reason as the node anti-join above: a
        // synced row is a peer's to publish, never this device's to author.
        "SELECT m.id
         FROM repo_memories m
         JOIN content_projected_nodes p ON p.stream_id = ?2 AND p.node_id = m.id
         WHERE m.repo_id = ?1
           AND m.origin = 'local'
           AND p.anchors_json IS NULL
           AND EXISTS (
                 SELECT 1 FROM repo_memory_bindings b
                 WHERE b.memory_id = m.id AND b.repo_id = m.repo_id)
         ORDER BY m.created_at_ms, m.id
         LIMIT ?3",
    )?;
    let ids = stmt
        .query_map(
            params![repo_id, stream.to_bytes().as_slice(), ANCHOR_BACKFILL_SCAN_PER_PASS],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Read the repo's unauthored nodes + edges and partition out the un-authorable (#680): the fast
/// path calls it to decide whether real work remains, the slow path to build the batch from the
/// AUTHORABLE half. Scope-independent like its two readers, so it runs on either an autocommit
/// `Connection` (fast path) or the reconcile `Transaction` (slow path, via deref).
///
/// Callers inside a transaction MUST call [`settle_owner_stream_in_tx`] first: this reads the
/// accepted-`/3` projection, which is stale while the stream owes a deferred refold. The
/// autocommit fast path cannot settle, so it treats outstanding debt as "work may exist" rather
/// than trusting an empty read (see `sync_owner_stream`).
fn read_reconcile_work(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
    policy: StreamSealPolicy,
) -> anyhow::Result<ReconcileWork> {
    let (authorable_nodes, quarantined_nodes): (Vec<MemoryRow>, Vec<MemoryRow>) =
        read_unauthored_memory_rows(conn, repo_id, stream)?
            .into_iter()
            .partition(|row| node_is_authorable(row, policy));
    // An edge whose source node is quarantined has no authored `NodeCreate` to hang off, so drop it
    // WITH its source (the source node's quarantine warning is the actionable one — it never
    // reaches `quarantined_edges`).
    let quarantined_node_ids: HashSet<&str> =
        quarantined_nodes.iter().map(|row| row.memory_id.as_str()).collect();
    // Partition the remaining edges by authorability the SAME way nodes are (#680): an edge whose
    // OWN `EdgeAdd` is oversized (a raw / imported / pre-cap ghost carrying an oversized free-form
    // field) is QUARANTINED rather than left in `live_edges` to make `author_content_batch_in_tx`
    // `bail!` and wedge the write path — the exact failure mode the node quarantine removes,
    // reached via an edge instead of a node.
    let (live_edges, quarantined_edges): (Vec<NodeEdge>, Vec<NodeEdge>) =
        super::edges::unauthored_edges(conn, repo_id, stream)?
            .into_iter()
            .filter(|edge| !quarantined_node_ids.contains(edge.source_node_id.as_str()))
            .partition(|edge| edge_is_authorable(edge, repo_id, policy));
    // Partition the anchor sweep the SAME way nodes and edges are, and for the same reason. This
    // leg selects on `anchors_json IS NULL`, a condition authoring is only USUALLY able to clear:
    // an over-cap or oversized set is dropped, never folds, stays NULL, and would be re-selected on
    // every pass — reporting authorable work forever and spinning the reconcile's slow path, the
    // exact #680 property `has_authorable_work` documents. Building the op here keeps
    // `has_authorable_work` implying a non-empty batch.
    let mut anchor_backfill_ops = Vec::new();
    let mut quarantined_anchor_ids = Vec::new();
    let mut swept = 0;
    for memory_id in read_anchor_backfill_ids(conn, repo_id, stream)? {
        // Stop once the pass has its publish budget. Candidates past this point are simply not this
        // pass's work — they are neither authored nor quarantined, and the next pass reaches them.
        // Counted in MEMORIES, not ops, so the source hash riding along below cannot halve it.
        if swept >= ANCHOR_BACKFILL_PER_PASS {
            break;
        }
        match anchors_op(conn, &memory_id)? {
            Some(op) if content_op_is_authorable(&op, policy) => {
                anchor_backfill_ops.push(op);
                swept += 1;
                // The hash describes exactly the anchors this sweep is publishing, and a receiver
                // applies it only where it seeds them — so it has to ride the same batch, or every
                // memory in the corpus this leg exists to reach lands on a peer unmarked forever.
                if let Some(op) = source_hash_op(conn, &memory_id)?
                    && content_op_is_authorable(&op, policy)
                {
                    anchor_backfill_ops.push(op);
                }
            },
            // `None` is unreachable (the query requires a binding), but counting it as quarantined
            // keeps the partition total rather than silently dropping.
            _ => quarantined_anchor_ids.push(memory_id),
        }
    }
    Ok(ReconcileWork {
        authorable_nodes,
        live_edges,
        quarantined_nodes,
        quarantined_edges,
        anchor_backfill_ops,
        quarantined_anchor_ids,
    })
}

/// Reconcile the repo's owner-bound `/2` stream against its tables: establish ownership (mint the
/// local account + publish a `StreamOwn` if needed) and author every `repo_memories` /
/// `repo_node_edges` row MISSING from the accepted-`/3` projection as owner-authored `/3` content.
/// Genesis (empty `/3` chain) authors the full history; a populated chain authors only the ghosts.
/// Idempotent and scope-gated (LEGACY / `local:` ids never root an immutable stream).
/// Scope-EXPLICIT — `repo_id` is passed, and the readers + re-resolution are scope-independent, so
/// the consolidation importer's unscoped connection can call it. Concurrency: two racing callers
/// serialize on the IMMEDIATE lock; the loser re-reads under the lock and authors only what the
/// winner left missing.
fn sync_owner_stream(conn: &Connection, repo_id: &str, now_ms: i64) -> anyhow::Result<()> {
    // Only a STABLE id may root an IMMUTABLE owner stream. Two ids get re-pointed later, which
    // would strand a stream signed under the old id: the legacy `__unassigned__` placeholder
    // (an unadopted DB, re-pointed on adoption) and a machine-local `local:` shallow-clone id
    // (upgraded to a portable id when the clone is deepened). No-op until a stable id is active
    // — as if unscoped.
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(());
    }
    // Mint the store's local account BEFORE the durability guard: the mint
    // self-transacts and holds its OWN durability guard that restores `synchronous = NORMAL` on
    // drop — beginning our guard first would let the mint's drop downgrade our authored commit
    // below NORMAL, silently losing the #560 durability. The mint is store-global and
    // idempotent (a re-mint returns the same account).
    rag_rat_oplog::local_account(conn, now_ms)?;
    let stream = ensure_owner_stream(conn, repo_id, now_ms)?;
    let policy = stream_seal_policy(conn, repo_id, stream)?;
    // While the owner stream owes a deferred refold, the accepted-`/3` projection is stale and the
    // completeness readers refuse to run at all (they are the fail-closed barrier) — so the
    // autocommit fast path is SKIPPED entirely rather than consulted and disbelieved. The
    // transaction below settles the debt first and then reads authoritatively. Preparation does not
    // depend on the op set (only the authorability pre-check does, and the in-transaction read
    // quarantines un-authorable rows by construction), so a sentinel drives it exactly as the
    // sealed-enable path does. A false positive costs one otherwise-idle transaction: authoring an
    // empty batch is already skipped below.
    let prepared = if rag_rat_oplog::content_stream_has_pending_refold(conn, stream)? {
        let sentinel =
            MemoryOp::EdgeRemove { edge_key: EdgeKey::from("pending-refold-settle-preparation") };
        prepare_owner_authoring(conn, repo_id, stream, policy, &[sentinel], now_ms)?
    } else {
        let work = read_reconcile_work(conn, repo_id, stream, policy)?;
        if !work.has_authorable_work() {
            work.warn_quarantined(repo_id);
            return Ok(());
        }
        let ops = build_reconcile_ops(
            conn,
            &work.authorable_nodes,
            &work.live_edges,
            &work.anchor_backfill_ops,
            repo_id,
            policy,
            rag_rat_oplog::content_stream_is_empty(conn, stream)?,
        )?;
        prepare_owner_authoring(conn, repo_id, stream, policy, &ops, now_ms)?
    };

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Settle the owner stream's deferred refold debt HERE, inside the write's own transaction and
    // before the authoritative re-read, so completeness is read against a current projection.
    settle_owner_stream_in_tx(&tx, stream, now_ms)?;
    anyhow::ensure!(
        stream_seal_policy(&tx, repo_id, stream)? == policy,
        "memory stream seal policy changed while preparing reconcile; retry"
    );
    // Authoritative re-read UNDER the write lock (TOCTOU): a concurrent author may have healed or
    // added rows between the probe and the lock, so re-read the missing set and re-derive `genesis`
    // here. `genesis` decides the status-elision: an empty `/3` content chain ⇒ no stale registers
    // ⇒ elide `active` (byte-identical to the pre-#541 genesis). This equivalence holds ONLY
    // under the single-local-writer owner stream (see the module header); phase D (foreign
    // devices can populate the stream) must revisit whether local-chain-empty still implies
    // register-clean.
    let genesis = rag_rat_oplog::content_stream_is_empty(&tx, stream)?;
    // Quarantine un-authorable rows (#680): partition the oversized ones out so a single row whose
    // signed `/3` envelope exceeds the §18a cap cannot make the whole batch `bail!` and wedge every
    // other memory write. The authorable rows are signed; the quarantined ones are logged and left
    // for the public API to shrink or delete.
    let work = read_reconcile_work(&tx, repo_id, stream, policy)?;
    work.warn_quarantined(repo_id);
    let ops = build_reconcile_ops(
        &tx,
        &work.authorable_nodes,
        &work.live_edges,
        &work.anchor_backfill_ops,
        repo_id,
        policy,
        genesis,
    )?;
    // Skip the author when there is nothing to author: a fresh repo whose anti-join was empty only
    // needed ownership established (done above), and authoring an empty batch would still refold +
    // reproject for no change.
    if !ops.is_empty() {
        let prepared =
            prepared.as_ref().context("reconcile work unexpectedly prepared as an empty batch")?;
        // Every op here is authorable — `read_reconcile_work` already quarantined any oversized row
        // (#680), so the `/3` author's §18a size check cannot fire on this batch. `with_context`
        // still names the repo so any OTHER authoring failure (a stale `auth_len`, a contested
        // account) is attributable rather than surfacing as a bare rollback.
        rag_rat_oplog::author_prepared_content_batch_in_tx(
            &tx,
            stream,
            &ops,
            prepared.owner_prepared()?,
            now_ms,
        )
        .with_context(|| {
            format!(
                "reconciling the /3 owner log for repo `{repo_id}` failed while authoring {} \
                 pre-existing memory op(s)",
                ops.len()
            )
        })?;
    }
    tx.commit()?;
    Ok(())
}

fn ensure_owner_stream(conn: &Connection, repo_id: &str, now_ms: i64) -> anyhow::Result<StreamId> {
    // Resolve the /2 stream under the repo's persisted access-mode intent, so live-write, drain,
    // reconcile, and catch-up all target the SAME stream id (a `PublicRead` stream has a distinct
    // id from the `Private` one). Absent intent = Private, today's behavior.
    let mode = owner_stream_access_mode(conn, repo_id)?;
    if let Some(stream) = rag_rat_oplog::established_owned_stream_v2_with_mode(conn, repo_id, mode)?
    {
        return Ok(stream);
    }
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Establishing a PRIVATE stream here would silently un-serve every repo this store contributes
    // to: an account is servable to a peer only when ALL of its streams are public, content is
    // served by its AUTHOR, and the owner is not enrolled here — so the contributions this store
    // has already authored, and any it authors later, become permanently unreachable.
    //
    // The configure-time check in `set_contribution_owner` cannot cover this: it runs once, and the
    // conflicting stream is created later by ordinary authoring in a DIFFERENT repo. Enforce it
    // where the conflict is actually created, inside the same transaction that would create it.
    //
    // Yes, this means memory authoring in an unrelated private repo fails while this index
    // contributes — or has ever contributed, since the authored entries outlive the configuration.
    // That is the honest ordering: the alternative is authoring memories nobody can ever fetch and
    // discovering it much later. The error names both escapes, and nothing is committed on the way
    // out, so re-running `sync contribute` or publishing the repo unblocks it.
    if mode != rag_rat_oplog::AccessMode::PublicRead
        && let Some(cause) = private_stream_strands_contributions(&tx)?
    {
        return Err(PrivateStreamRefusal(format!(
            "repo `{repo_id}` would need a PRIVATE memory stream, but this index {cause} — and an \
             account is fetchable by a peer only while all of its streams are public, so this \
             would strand those contributions unreachable. Index `{repo_id}` in a separate \
             database, or publish it with `rag-rat sync publish`"
        ))
        .into());
    }
    let stream = rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(&tx, repo_id, mode, now_ms)?;
    tx.commit()?;
    Ok(stream)
}

/// The refusal [`ensure_owner_stream`] raises rather than establish a PRIVATE stream that would
/// strand contributions. Typed, not a bare `bail!`, because this is a stream-establishment POLICY:
/// it belongs on the paths where a user is asking for a memory write, and the INDEX-MAINTENANCE
/// seam that shares the same reconcile ([`heal_memory_oplog_ghosts`]) recognizes it and skips.
/// Left as an opaque error there, an ordinary `sync uncontribute` would fail `rag-rat reconcile`,
/// every watcher pass, and `rag-rat index` — with no ghost to heal in the first place.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct PrivateStreamRefusal(String);

/// Why establishing a PRIVATE stream in this index would strand contributions, phrased as the
/// middle of the refusal sentence — or `None` when nothing is at stake.
///
/// EVIDENCE outranks configuration for the streams this account has ALREADY authored onto: `sync
/// uncontribute` (or any other path that drops the meta row) clears the configured target, but
/// those entries stay on the owner's PublicRead stream, and the owner can fetch them only while
/// this account owns no private stream. Keyed on configuration alone, the unset would open a door
/// that a `StreamOwn` — append-only, never un-authorable — then closes forever.
///
/// Each authored stream is put through the same servability check the serving side applies
/// ([`crate::sync_driver::contribution_stream_is_servable`]), so the refusal fires exactly when
/// something real is at stake. Authorship the owner has since revoked strands nothing — its pull
/// already cannot reach this account for that stream — and blocking on it would be a permanent
/// refusal with no recourse.
fn private_stream_strands_contributions(conn: &Connection) -> anyhow::Result<Option<String>> {
    if let Some((contributing_repo, owner)) = contribution_targets(conn)?.first() {
        return Ok(Some(format!(
            "contributes repo `{contributing_repo}`'s memories to account {}",
            rag_rat_base::hash::hex_lower(&owner.to_bytes()),
        )));
    }
    // No local account ⇒ nothing was ever authored anywhere ⇒ nothing to strand.
    let Some(account) = rag_rat_oplog::read_local_account(conn)? else {
        return Ok(None);
    };
    let mut still_servable = 0usize;
    for stream in rag_rat_oplog::authored_foreign_streams(conn, account)? {
        let Some(owner) = rag_rat_oplog::stream_owner_account(conn, stream)? else {
            continue;
        };
        if crate::sync_driver::contribution_stream_is_servable(conn, owner, stream, account)? {
            still_servable += 1;
        }
    }
    if still_servable == 0 {
        return Ok(None);
    }
    Ok(Some(format!(
        "has already authored memories onto {still_servable} stream(s) another account owns and \
         can still serve"
    )))
}

fn prepare_owner_authoring(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
    policy: StreamSealPolicy,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Option<PreparedOwnerAuthoring>> {
    if ops.is_empty() {
        return Ok(None);
    }
    for op in ops {
        reject_unauthorable_content_op(op, policy)?;
    }
    let prepared =
        rag_rat_oplog::prepare_content_authoring(conn, stream, policy.seal_policy(), now_ms)?;
    Ok(Some(PreparedOwnerAuthoring {
        repo_id: repo_id.to_string(),
        stream,
        role: AuthoringRole::Owner { policy, prepared },
    }))
}

pub(crate) fn prepare_live_authoring(
    conn: &Connection,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Option<PreparedOwnerAuthoring>> {
    if ops.is_empty() {
        return Ok(None);
    }
    let Some(repo_id) = memory_repo_scope(conn)? else {
        for op in ops {
            reject_unauthorable_content_op(op, StreamSealPolicy::Plaintext)?;
        }
        return Ok(None);
    };
    // Grantee mode (#1164): a granted contributor authors onto the CONFIGURED owner's stream via
    // its grant. Grants target public plaintext streams, so the authorability guard uses
    // Plaintext.
    if let Some(ctx) = grantee_context(conn, &repo_id)? {
        for op in ops {
            reject_unauthorable_content_op(op, StreamSealPolicy::Plaintext)?;
        }
        return Ok(Some(PreparedOwnerAuthoring {
            repo_id,
            stream: ctx.stream,
            role: AuthoringRole::Grantee {
                owner_account: ctx.owner_account,
                grant_id: ctx.grant_id,
            },
        }));
    }
    let Some(stream) = stable_owner_stream_for_repo(conn, &repo_id)? else {
        for op in ops {
            reject_unauthorable_content_op(op, StreamSealPolicy::Plaintext)?;
        }
        return Ok(None);
    };
    let policy = stream_seal_policy(conn, &repo_id, stream)?;
    prepare_owner_authoring(conn, &repo_id, stream, policy, ops, now_ms)
}

pub(crate) fn prepare_live_content_authoring(
    conn: &Connection,
    now_ms: i64,
) -> anyhow::Result<Option<PreparedOwnerAuthoring>> {
    let sentinel = MemoryOp::EdgeRemove { edge_key: EdgeKey::from("live-authoring-preparation") };
    prepare_live_authoring(conn, &[sentinel], now_ms)
}

pub(crate) fn enable_sealed_authoring(conn: &Connection, now_ms: i64) -> anyhow::Result<bool> {
    let repo_id = memory_repo_scope(conn)?.context("sync enable requires an active repo scope")?;
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        anyhow::bail!("sync enable requires a stable repo identity (not legacy or local-only)");
    }
    // Publish and sealing are mutually-exclusive one-way intents (a public reader cannot unwrap
    // sealed bytes); the reverse guard lives in `enable_public_authoring`.
    if owner_stream_access_mode(conn, &repo_id)? == rag_rat_oplog::AccessMode::PublicRead {
        anyhow::bail!(
            "repo `{repo_id}` is a published public knowledge base; sealing is incompatible with \
             public authoring"
        );
    }
    rag_rat_oplog::local_account(conn, now_ms)?;
    let stream = ensure_owner_stream(conn, &repo_id, now_ms)?;
    let was_enabled =
        explicit_stream_seal_policy(conn, &repo_id)? == Some(StreamSealPolicy::Sealed);

    // A non-empty sentinel is required because preparation deliberately makes empty batches
    // side-effect-free. The sentinel is never authored; it only drives the three-transaction key
    // protocol before the final intent+reconcile transaction.
    let sentinel = MemoryOp::EdgeRemove { edge_key: EdgeKey::from("sync-enable-key-preparation") };
    let prepared = prepare_owner_authoring(
        conn,
        &repo_id,
        stream,
        StreamSealPolicy::Sealed,
        &[sentinel],
        now_ms,
    )?
    .context("sealed enable preparation unexpectedly returned empty")?;

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Unknown or malformed persisted policy was already rejected above. The only writable token is
    // `sealed`; there is deliberately no plaintext writer, and the derived ratchet keeps this
    // one-way even if external tooling deletes the intent row.
    anyhow::ensure!(
        stream_seal_policy(&tx, &repo_id, stream)? == StreamSealPolicy::Sealed,
        "sealed enable key preparation did not arm the stream ratchet"
    );
    rag_rat_db::meta::set_repo_meta(&tx, &repo_id, STREAM_SEAL_POLICY_META_KEY, "sealed")?;
    // Same barrier discipline as the reconcile path: settle inside this transaction so the sealed
    // re-authoring below reads completeness against a current accepted-`/3` projection.
    settle_owner_stream_in_tx(&tx, stream, now_ms)?;
    let work = read_reconcile_work(&tx, &repo_id, stream, StreamSealPolicy::Sealed)?;
    work.warn_quarantined(&repo_id);
    let ops = build_reconcile_ops(
        &tx,
        &work.authorable_nodes,
        &work.live_edges,
        &work.anchor_backfill_ops,
        &repo_id,
        StreamSealPolicy::Sealed,
        rag_rat_oplog::content_stream_is_empty(&tx, stream)?,
    )?;
    if !ops.is_empty() {
        rag_rat_oplog::author_prepared_content_batch_in_tx(
            &tx,
            stream,
            &ops,
            prepared.owner_prepared()?,
            now_ms,
        )?;
    }
    tx.commit()?;
    Ok(!was_enabled)
}

/// Mark the active repo's account as a PUBLIC knowledge base: persist the one-way `public`
/// access-mode intent and ensure its `PublicRead` `/2` owner stream. Thereafter every conn-only
/// writer authors public (via [`owner_stream_access_mode`]), and serving selects
/// `AuthPolicy::PublicRead`. Returns whether this call newly enabled it (idempotent). Refuses —
/// rather than brick the account — when: the repo id is legacy/local-only; sealing is intended
/// (publish and sealing are mutually-exclusive one-way intents; a public reader could never unwrap
/// sealed bytes); or the account already holds any private stream (`account_is_fully_public`
/// false), since a mixed account can never be served public and the private StreamOwn can never be
/// un-authored — publishing an existing private repo is not supported (start a fresh public index).
pub(crate) fn enable_public_authoring(conn: &Connection, now_ms: i64) -> anyhow::Result<bool> {
    let repo_id = memory_repo_scope(conn)?.context("sync publish requires an active repo scope")?;
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        anyhow::bail!("sync publish requires a stable repo identity (not legacy or local-only)");
    }
    if explicit_stream_seal_policy(conn, &repo_id)? == Some(StreamSealPolicy::Sealed) {
        anyhow::bail!(
            "repo `{repo_id}` authors sealed memories; publishing requires plaintext (a public \
             reader cannot unwrap sealed content)"
        );
    }
    let account = rag_rat_oplog::local_account(conn, now_ms)?;
    anyhow::ensure!(
        rag_rat_oplog::account_is_fully_public(conn, account)?,
        "repo `{repo_id}`'s account already owns a private stream; a public knowledge base must \
         be a fresh index (publishing an existing private repo is not supported)"
    );
    let was_enabled =
        owner_stream_access_mode(conn, &repo_id)? == rag_rat_oplog::AccessMode::PublicRead;

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Persist intent FIRST so the ensure below (and every later writer) resolves the PublicRead
    // stream; the one-way ratchet holds even if external tooling deletes the intent row, because
    // the op-log's PublicRead StreamOwn then makes `account_is_fully_public` true and
    // re-authoring private is refused by this guard.
    rag_rat_db::meta::set_repo_meta(&tx, &repo_id, STREAM_ACCESS_MODE_META_KEY, "public")?;
    rag_rat_oplog::ensure_owned_stream_v2_with_mode_in_tx(
        &tx,
        &repo_id,
        rag_rat_oplog::AccessMode::PublicRead,
        now_ms,
    )?;
    tx.commit()?;
    Ok(!was_enabled)
}

pub(crate) fn catch_up_enrolled_device_keys(
    conn: &Connection,
    target: rag_rat_oplog::DeviceFingerprint,
    now_ms: i64,
) -> anyhow::Result<rag_rat_oplog::CatchUpReport> {
    let repo_id =
        memory_repo_scope(conn)?.context("sync catch-up requires an active repo scope")?;
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        anyhow::bail!("sync catch-up requires a stable repo identity (not legacy or local-only)");
    }
    let mode = owner_stream_access_mode(conn, &repo_id)?;
    let stream = rag_rat_oplog::established_owned_stream_v2_with_mode(conn, &repo_id, mode)?
        .with_context(|| {
            format!(
                "sync catch-up requires an established owner stream for active repo `{repo_id}`"
            )
        })?;

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    anyhow::ensure!(
        memory_repo_scope(&tx)?.as_deref() == Some(repo_id.as_str()),
        "active repo scope changed while starting sync catch-up; retry"
    );
    anyhow::ensure!(
        rag_rat_oplog::owned_stream_v2_id_with_mode(&tx, &repo_id, mode)? == Some(stream),
        "active repo owner stream changed while starting sync catch-up; retry"
    );
    let report =
        rag_rat_oplog::catch_up_stream_keys_for_device_in_tx(&tx, target, &[stream], now_ms)?;
    tx.commit()?;
    Ok(report)
}

/// Grant `grantee_account_id` Writer authority on the ACTIVE repo's owner stream (#1164), so a
/// separate identity can author memories into this repo's shared set. Owner-only: authoring the
/// grant verifies it became the effective fact, which a non-owner device cannot produce. v1
/// requires a PUBLISHED (public_read) repo — a grant on a private stream would need stream-key
/// wraps to the grantee, which is out of scope. Returns the grant id. Mirrors the catch-up seam's
/// resolve-then- re-check-under-lock discipline.
/// Resolve the active repo's published grant target — the checks every grant-shaped operation
/// (`sync grant`, `sync invite-writer`) shares: a stable repo identity, the publish ratchet
/// flipped, and an established `PublicRead` owner stream.
pub(crate) fn published_grant_target(
    conn: &Connection,
    operation: &str,
) -> anyhow::Result<(String, rag_rat_oplog::StreamId)> {
    let repo_id = memory_repo_scope(conn)?
        .with_context(|| format!("{operation} requires an active repo scope"))?;
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        anyhow::bail!("{operation} requires a stable repo identity (not legacy or local-only)");
    }
    let mode = owner_stream_access_mode(conn, &repo_id)?;
    anyhow::ensure!(
        mode == rag_rat_oplog::AccessMode::PublicRead,
        "{operation} requires a published repo — run `sync publish` first (granting a writer on a \
         private stream is not yet supported: the grantee would need stream-key wraps)"
    );
    let stream = rag_rat_oplog::established_owned_stream_v2_with_mode(conn, &repo_id, mode)?
        .with_context(|| {
            format!("{operation} requires an established owner stream for active repo `{repo_id}`")
        })?;
    Ok((repo_id, stream))
}

pub(crate) fn grant_repo_writer(
    conn: &Connection,
    grantee_account_id: rag_rat_oplog::AccountId,
    now_ms: i64,
) -> anyhow::Result<[u8; 32]> {
    let (repo_id, stream) = published_grant_target(conn, "sync grant")?;
    let mode = owner_stream_access_mode(conn, &repo_id)?;

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    anyhow::ensure!(
        memory_repo_scope(&tx)?.as_deref() == Some(repo_id.as_str()),
        "active repo scope changed while starting sync grant; retry"
    );
    anyhow::ensure!(
        rag_rat_oplog::owned_stream_v2_id_with_mode(&tx, &repo_id, mode)? == Some(stream),
        "active repo owner stream changed while starting sync grant; retry"
    );
    let grant_id = rag_rat_oplog::author_stream_grant_in_tx(
        &tx,
        stream,
        grantee_account_id,
        rag_rat_oplog::GrantRole::Writer,
        now_ms,
    )?;
    tx.commit()?;
    Ok(grant_id)
}

/// One row of the owner-facing grant listing (`sync grants`), hex-rendered for display.
#[derive(Debug, Clone)]
pub struct RepoGrantListing {
    pub grantee_account_id: String,
    pub role: String,
    pub open: bool,
    pub grant_id: String,
}

/// Every grant the local account has authored on the active repo's owner stream, open and
/// revoked, newest first — an owner otherwise has no way to see who holds access. Empty (not an
/// error) when the repo has no owner stream or the store has no account yet.
pub(crate) fn list_repo_grants(conn: &Connection) -> anyhow::Result<Vec<RepoGrantListing>> {
    let repo_id = memory_repo_scope(conn)?.context("sync grants requires an active repo scope")?;
    let Some(owner) = rag_rat_oplog::read_local_account(conn)? else {
        return Ok(Vec::new());
    };
    let mode = owner_stream_access_mode(conn, &repo_id)?;
    let Some(stream) = rag_rat_oplog::owned_stream_v2_id_with_mode(conn, &repo_id, mode)? else {
        return Ok(Vec::new());
    };
    Ok(rag_rat_oplog::stream_grants_for_owner(conn, owner, stream)?
        .into_iter()
        .map(|grant| RepoGrantListing {
            grantee_account_id: rag_rat_base::hash::hex_lower(&grant.grantee_account_id.to_bytes()),
            role: grant.role,
            open: grant.open,
            grant_id: rag_rat_base::hash::hex_lower(&grant.grant_id),
        })
        .collect())
}

/// The authored revocation, hex-rendered for the CLI report.
#[derive(Debug, Clone)]
pub struct RepoRevokeReport {
    pub grantee_account_id: String,
    pub grant_ids: Vec<String>,
    pub revoke_ids: Vec<String>,
    pub reason: String,
    /// `(device_fingerprint_hex, kept_through_seq)` — the chain prefixes that stay valid.
    pub cuts: Vec<(String, u64)>,
}

/// Revoke the active repo's open grant to `grantee_ref` — a full 64-hex account id or an
/// unambiguous prefix of one, matched against the stream's OPEN grantees (git-style, so the
/// operator can name who they see in `sync grants`). The cut semantics follow `reason`; see
/// [`rag_rat_oplog::author_stream_revoke_in_tx`]. Mirrors [`grant_repo_writer`]'s
/// resolve-then-re-check-under-lock discipline.
pub(crate) fn revoke_repo_writer(
    conn: &Connection,
    grantee_ref: &str,
    reason: rag_rat_oplog::RevokeReason,
    keep_until: Option<(rag_rat_oplog::DeviceFingerprint, u64)>,
    now_ms: i64,
) -> anyhow::Result<RepoRevokeReport> {
    let repo_id = memory_repo_scope(conn)?.context("sync revoke requires an active repo scope")?;
    let owner = rag_rat_oplog::read_local_account(conn)?
        .context("sync revoke requires this store's account — nothing has been granted yet")?;
    let mode = owner_stream_access_mode(conn, &repo_id)?;
    let stream = rag_rat_oplog::owned_stream_v2_id_with_mode(conn, &repo_id, mode)?
        .context("sync revoke requires the repo's owner stream")?;
    let grantee = resolve_grantee_ref(conn, owner, stream, grantee_ref)?;

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    anyhow::ensure!(
        memory_repo_scope(&tx)?.as_deref() == Some(repo_id.as_str()),
        "active repo scope changed while starting sync revoke; retry"
    );
    let revocation = rag_rat_oplog::author_stream_revoke_in_tx(
        &tx, stream, grantee, reason, keep_until, now_ms,
    )?;
    tx.commit()?;
    Ok(RepoRevokeReport {
        grantee_account_id: rag_rat_base::hash::hex_lower(&grantee.to_bytes()),
        grant_ids: revocation
            .grant_ids
            .iter()
            .map(|id| rag_rat_base::hash::hex_lower(id))
            .collect(),
        revoke_ids: revocation
            .revoke_ids
            .iter()
            .map(|id| rag_rat_base::hash::hex_lower(id))
            .collect(),
        reason: reason.as_db_str().to_string(),
        cuts: revocation
            .cuts
            .iter()
            .map(|cut| (rag_rat_base::hash::hex_lower(&cut.device_fingerprint.to_bytes()), cut.seq))
            .collect(),
    })
}

/// Resolve a full 64-hex account id, or an unambiguous hex prefix of an open WRITER grantee on the
/// stream. Prefixes shorter than 4 characters are refused outright — with one grantee even a
/// single character would match, and an id that short in an operator's history is more likely a
/// typo than an intent.
fn resolve_grantee_ref(
    conn: &Connection,
    owner: rag_rat_oplog::AccountId,
    stream: rag_rat_oplog::StreamId,
    grantee_ref: &str,
) -> anyhow::Result<rag_rat_oplog::AccountId> {
    let reference = grantee_ref.trim();
    if reference.len() == 64 {
        return rag_rat_oplog::AccountId::from_hex(reference);
    }
    anyhow::ensure!(
        reference.len() >= 4 && reference.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "`{reference}` is not an account id or a hex prefix of at least 4 characters"
    );
    let reference = reference.to_ascii_lowercase();
    let mut matches: Vec<rag_rat_oplog::AccountId> =
        rag_rat_oplog::stream_grants_for_owner(conn, owner, stream)?
            .into_iter()
            .filter(|grant| grant.open && grant.role == "writer")
            .map(|grant| grant.grantee_account_id)
            .filter(|grantee| {
                rag_rat_base::hash::hex_lower(&grantee.to_bytes()).starts_with(&reference)
            })
            .collect();
    matches.dedup();
    match matches.as_slice() {
        [grantee] => Ok(*grantee),
        [] => anyhow::bail!(
            "no open writer grant matches `{reference}` on this repo's stream — `sync grants` \
             lists them"
        ),
        _ => anyhow::bail!(
            "`{reference}` is ambiguous between {} open writer grantees — give more characters",
            matches.len()
        ),
    }
}

/// Reconcile the ACTIVE repo's owner stream (scope read from the connection) — the idempotent call
/// every live memory/edge mutation makes before authoring (#532), now self-healing per node/edge (a
/// ghost row is authored on the next mutation, so no later lifecycle op on it is inert). A no-op on
/// an unscoped DB.
pub(crate) fn backfill_memory_oplog(conn: &Connection, now_ms: i64) -> anyhow::Result<()> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(());
    };
    // A granted contributor does not own the stream: there is no `StreamOwn` to establish and no
    // local owner history to reconcile — its live authoring goes straight to the owner's stream via
    // the grant (#1164). The owner-only establish/reconcile below would try to author under local
    // ownership and is skipped entirely.
    if is_contribution_mode(conn, &repo_id)? {
        return Ok(());
    }
    sync_owner_stream(conn, &repo_id, now_ms)
}

/// [`backfill_memory_oplog`] as INDEX MAINTENANCE runs it — after an embedding reconcile, on every
/// watcher pass, on every `rag-rat index` — where nobody asked for a memory write.
///
/// The one difference is the stream-establishment refusal ([`PrivateStreamRefusal`]): an
/// ex-contributor still owes the owner a public account, so it provably has no owner stream and
/// never will until it publishes or re-contributes. Propagating that here would fail the whole
/// pass, with zero ghosts required — the refusal belongs on the authoring paths, which keep it.
/// Any OTHER error is a real failure and still propagates.
pub(crate) fn heal_memory_oplog_ghosts(conn: &Connection, now_ms: i64) -> anyhow::Result<()> {
    match backfill_memory_oplog(conn, now_ms) {
        Err(err) if err.downcast_ref::<PrivateStreamRefusal>().is_some() => {
            tracing::warn!(
                error = format!("{err:#}"),
                "skipping the memory op-log ghost heal; memory authoring in this repo stays \
                 refused until it is published or contributing again",
            );
            Ok(())
        },
        other => other,
    }
}

/// Reconcile a SPECIFIC repo's owner stream independent of connection scope — the seam
/// consolidation uses to author freshly-imported (remapped) rows into the TARGET's owner stream
/// under the TARGET's identity (#541). The source's pre-remap signed entries are intentionally NOT
/// carried (they are signed under the source device over pre-remap ids). Wired into consolidation
/// by [`crate::index::consolidate::run`] (#541 Task 5), immediately after the import commits and
/// before the legacy file is renamed away.
pub(crate) fn reconcile_owner_stream_for_repo(
    conn: &Connection,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    // A granted contributor owns no stream for this repo, so this reconcile cannot run: it would
    // establish one and author the imported rows onto a stream nobody reads, while the configured
    // owner — where this repo's memories actually live — never receives them.
    //
    // FAIL, do not skip. Both callers (legacy consolidation, `sync publish --seed`) call this
    // specifically to author freshly-IMPORTED rows. Reporting success without authoring would
    // strand them with no `NodeCreate`, leaving every later update or status op on them inert.
    // Authoring them as a grantee is the real feature; until it exists, say so.
    //
    // This is the BACKSTOP, not the gate. By the time control reaches here the import has already
    // committed, so failing leaves the very half-applied state the refusal exists to prevent —
    // which is why both callers refuse BEFORE their irreversible step (`consolidate::run_inner`
    // before importing, `sync_publish_seed` before publishing). Keep this arm so a future third
    // caller fails loudly instead of silently skipping, and give it the same pre-check.
    //
    // (The live-write path skips silently instead, and correctly: `backfill_memory_oplog` has
    // nothing to reconcile because each mutation already authors onto the owner's stream.)
    ensure_not_mirroring_another_account(conn, repo_id, "importing memories into this repo")?;
    sync_owner_stream(conn, repo_id, now_ms)
}

/// The active repo's owner-bound `/2` stream, but ONLY when the scope is a STABLE identity to root
/// an IMMUTABLE stream on — the SAME gate the backfill uses: `Some`, not the `__unassigned__`
/// placeholder, not a `local:` shallow-clone id (both get re-pointed later) — AND the store's local
/// account is minted (the `/2` id is derived under it). `None` otherwise, and the `author_*` seams
/// SKIP authoring on `None`, so a scope-less mutation (most tests) or a store whose account is not
/// yet minted never touches the log. Derivation-only (no fact check), so it is safe inside the
/// caller's open txn — unlike the reconcile's autocommit `established_owned_stream_v2` probe.
fn stable_owner_stream(conn: &Connection) -> anyhow::Result<Option<StreamId>> {
    let Some(repo_id) = memory_repo_scope(conn)? else {
        return Ok(None);
    };
    stable_owner_stream_for_repo(conn, &repo_id)
}

fn stable_owner_stream_for_repo(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<StreamId>> {
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(None);
    }
    let mode = owner_stream_access_mode(conn, repo_id)?;
    rag_rat_oplog::owned_stream_v2_id_with_mode(conn, repo_id, mode)
}

/// Reject a live content op the `/3` log cannot carry — either a shape `op::decode` would refuse
/// at any size, or an ASSEMBLED signed envelope over the §18a 256 KiB cap (#680). The
/// AUTHORITATIVE whole-op write-boundary guard. The cheap per-field caps
/// (`validate_payload`'s payload byte cap, `validate_edge_len`'s edge-anchor cap, the title/body
/// char caps) fast-fail a single pathological field, but they cannot see an AGGREGATE — most
/// reachably an arbitrary NUMBER of individually-valid tags on a `NodeCreate`/`NodeUpdate`, or a
/// max-ish payload + a long body + many tags TOGETHER — nor a FUTURE uncapped field. This one
/// check, built on the SAME [`rag_rat_oplog::content_op_is_authorable`] the reconcile quarantine
/// uses, rejects every such op before it is signed, so a write the guard accepts is exactly one the
/// reconcile can later author. Without it an "otherwise valid" create/update assembles an
/// un-authorable op that the #680 reconcile quarantine then SILENTLY skips — the user never learns
/// at write time.
fn reject_unauthorable_content_op(op: &MemoryOp, policy: StreamSealPolicy) -> anyhow::Result<()> {
    if content_op_is_authorable(op, policy) {
        return Ok(());
    }
    // Two different failures reach here and they want different remedies, so name the right one.
    // A structural refusal is not a size problem: 65 tiny anchors are nowhere near the byte cap,
    // and a binding named twice is a dedupe fix, not a "shorten it" one.
    if !rag_rat_oplog::within_wire_limits(op) {
        match op {
            MemoryOp::NodeAnchors { node_id, anchors } => anyhow::bail!(
                "memory `{}` cannot store its {} anchors: an anchor set holds at most {} bindings \
                 and must not name one binding twice",
                node_id.as_str(),
                anchors.len(),
                rag_rat_oplog::MAX_ANCHORS_PER_OP
            ),
            // No other op kind has a structural limit today; this arm keeps the branch total.
            _ => anyhow::bail!("this memory operation has a shape the op log cannot encode"),
        }
    }
    // Past the structural gate the culprit is size: an aggregate the per-field caps cannot see (or
    // a future uncapped field). Name what to shrink rather than surfacing a bare envelope-overflow
    // rollback.
    match op {
        MemoryOp::NodeCreate { node_id, .. } | MemoryOp::NodeUpdate { node_id, .. } =>
            anyhow::bail!(
                "memory `{}` is too large to store: even with each field within its own limit, \
                 its title, body, payload and tags together exceed the 256 KiB signed-entry cap — \
                 reduce the number of tags, or shrink the body/payload",
                node_id.as_str()
            ),
        MemoryOp::EdgeAdd { edge } => anyhow::bail!(
            "the edge from `{}` is too large to store: its assembled fields exceed the 256 KiB \
             signed-entry cap — shorten the target anchor / target repo id",
            edge.source_node_id.as_str()
        ),
        MemoryOp::NodeAnchors { node_id, anchors } => anyhow::bail!(
            "memory `{}` is too large to store: its {} anchors exceed the 256 KiB signed-entry \
             cap — shorten their paths, or bind the memory to fewer places",
            node_id.as_str(),
            anchors.len()
        ),
        // NodeStatus / EdgeRemove / Rebind / NodeSourceHash carry no unbounded free-form field a
        // caller controls — a source hash is a fixed-width digest — so they cannot exceed the cap;
        // this arm keeps the guard total over the op vocabulary.
        _ => anyhow::bail!(
            "this memory operation is too large to store: its assembled /3 content envelope \
             exceeds the 256 KiB signed-entry cap"
        ),
    }
}

/// Author `ops` as owner-authored `/3` content on the active repo's owner-bound `/2` stream WITHIN
/// the caller's mutation txn — the strict-atomic live seam.
/// [`rag_rat_oplog::author_prepared_content_batch_in_tx`] inserts, refolds, and verify-accepts the
/// batch (no open/commit), so an authoring error propagates via `?` and the caller's txn rolls the
/// table write back with it. A NO-OP under an unstable scope or before the local account is minted.
/// The caller MUST have run `backfill_memory_oplog` first, so the store's account plus `StreamOwn`
/// are established (else the batch's verify-accepted rolls back) and the pre-existing history
/// precedes this live entry.
fn author_in_owner_stream(
    tx: &Transaction<'_>,
    ops: &[MemoryOp],
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    // Whole-op write-boundary guard (#680): every live mutation
    // (`create_memory`/`update_memory`/`rebind_memory`/`add_edge`/`remove_edge`) funnels its
    // authored ops through
    // this ONE seam, so rejecting an un-authorable op here — before it is signed — is the single
    // authoritative catch-all for the aggregate no per-field cap sees (e.g. thousands of
    // individually-valid tags) and any future uncapped field. Runs BEFORE the scope gate so an
    // un-authorable op is rejected consistently even on a not-yet-owned stream (the reconcile does
    // NOT pass through here, so its pre-cap/imported-row quarantine is unaffected).
    // Skip a no-op mutation: an empty batch would still refold + reproject the whole stream for no
    // change. (The four live seams only reach here with non-empty ops today, but the guard keeps a
    // change-free `author_update` from doing O(chain) work.)
    if ops.is_empty() {
        return Ok(());
    }
    let Some(prepared) = prepared else {
        // Scope-less and unstable-scope callers intentionally do not author.
        anyhow::ensure!(stable_owner_stream(tx)?.is_none(), "missing prepared owner authoring");
        return Ok(());
    };
    anyhow::ensure!(
        memory_repo_scope(tx)?.as_deref() == Some(prepared.repo_id.as_str()),
        "prepared /3 authoring belongs to a different repo scope"
    );
    // The whole-op authorability guard runs for both roles; a grant targets a public plaintext
    // stream, so its guard policy is Plaintext.
    let guard_policy = match &prepared.role {
        AuthoringRole::Owner { policy, .. } => *policy,
        AuthoringRole::Grantee { .. } => StreamSealPolicy::Plaintext,
    };
    for op in ops {
        reject_unauthorable_content_op(op, guard_policy)?;
    }
    match &prepared.role {
        AuthoringRole::Owner { policy, prepared: content } => {
            anyhow::ensure!(
                stream_seal_policy(tx, &prepared.repo_id, prepared.stream)? == *policy,
                "memory stream seal policy changed while preparing live authoring; retry"
            );
            rag_rat_oplog::author_prepared_content_batch_in_tx(
                tx,
                prepared.stream,
                ops,
                content,
                now_ms,
            )?;
        },
        // Grantee: author onto the OWNER's stream citing the grant (#1164). No seal-policy recheck
        // — the stream is the owner's and v1 grants are plaintext-public.
        AuthoringRole::Grantee { owner_account, grant_id } => {
            rag_rat_oplog::author_grantee_content_batch_in_tx(
                tx,
                prepared.stream,
                *owner_account,
                *grant_id,
                ops,
                now_ms,
            )?;
        },
    }
    Ok(())
}

/// Author a live memory CREATE (`NodeCreate`) inside the caller's mutation txn. A fresh memory has
/// no node-edges yet, so this is a single op.
pub(crate) fn author_create(
    tx: &Transaction<'_>,
    memory: &RepoMemory,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let node_id = NodeId::from(memory.memory_id.as_str());
    let mut ops =
        vec![MemoryOp::NodeCreate { node_id: node_id.clone(), content: content_of(memory) }];
    ops.extend(anchors_op(tx, &memory.memory_id)?);
    ops.extend(source_hash_op(tx, &memory.memory_id)?);
    author_in_owner_stream(tx, &ops, prepared, now_ms)
}

/// Author the memory's CURRENT anchor set, for a caller that just changed which code it points at.
/// A full-set snapshot, so the op says what the bindings are now rather than how they got there.
pub(crate) fn author_anchors(
    tx: &Transaction<'_>,
    memory_id: &str,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let mut ops: Vec<MemoryOp> = anchors_op(tx, memory_id)?.into_iter().collect();
    // A rebind re-stamps `source_text_hash` in the same transaction, so the published hash has to
    // move with the anchors or a peer keeps comparing against the pre-rebind text.
    ops.extend(source_hash_op(tx, memory_id)?);
    author_in_owner_stream(tx, &ops, prepared, now_ms)
}

/// The `NodeSourceHash` op for a memory's stamped source hash, or `None` when it has none.
///
/// Like the anchor snapshot, an absent hash authors NOTHING rather than a sentinel: a receiver
/// treats "nobody published one" as no evidence of drift, so spending a signed entry to say it
/// would tell that peer nothing it can act on.
///
/// That silence has no retraction, so the published register can outlive the hash it was taken
/// from — an author who rebinds onto a target that carries none (tracker / dir / commit / call
/// path) nulls the column and publishes nothing. A receiver applies the hash only where it also
/// installs the anchors it describes, which is what keeps the pair from contradicting each other.
fn source_hash_op(conn: &Connection, memory_id: &str) -> anyhow::Result<Option<MemoryOp>> {
    let mut stmt = conn.prepare("SELECT source_text_hash FROM repo_memories WHERE id = ?1")?;
    let hash: Option<String> = stmt
        .query_map(params![memory_id], |row| row.get::<_, Option<String>>(0))?
        .next()
        .transpose()?
        .flatten();
    Ok(hash.map(|source_text_hash| MemoryOp::NodeSourceHash {
        node_id: NodeId::from(memory_id),
        source_text_hash,
    }))
}

/// The `NodeAnchors` op for a memory's current bindings, or `None` when it has none.
///
/// An unanchored memory authors NOTHING rather than an empty set. The two are different facts to a
/// receiver — nobody published bindings, versus the author saying there are none — but neither
/// seeds anything, so publishing the empty case would cost a signed entry per unanchored memory to
/// tell a peer something it cannot act on. The projection keeps the distinction because a future op
/// that RETRACTS a binding set will need it.
///
/// Deliberately unfiltered: the author publishes every portable fact it holds, including kinds this
/// binary's own drain declines to seed. Which anchors are usable is the receiver's judgment, and
/// filtering here would destroy information a later receiver could use.
fn anchors_op(conn: &Connection, memory_id: &str) -> anyhow::Result<Option<MemoryOp>> {
    let anchors = portable_anchors_of(conn, memory_id)?;
    if anchors.is_empty() {
        return Ok(None);
    }
    Ok(Some(MemoryOp::NodeAnchors { node_id: NodeId::from(memory_id), anchors }))
}

/// Read a memory's bindings as the portable facts the wire carries — every replicated column, and
/// no checkout-local resolution state.
fn portable_anchors_of(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Vec<rag_rat_oplog::PortableAnchor>> {
    let mut stmt = conn.prepare(
        "SELECT binding_kind, binding_id, path, start_line, end_line, commit_hash, tracker,
                project, item_key, created_at_ms, symbol_kind, signature_hash, moniker_tool,
                moniker_tool_version
         FROM repo_memory_bindings
         WHERE memory_id = ?1
           AND repo_id = (SELECT repo_id FROM repo_memories WHERE id = ?1)
         ORDER BY binding_kind, binding_id",
    )?;
    let rows = stmt.query_map(params![memory_id], |row| {
        Ok(rag_rat_oplog::PortableAnchor {
            binding_kind: row.get(0)?,
            binding_id: row.get(1)?,
            path: row.get(2)?,
            start_line: row.get(3)?,
            end_line: row.get(4)?,
            commit_hash: row.get(5)?,
            tracker: row.get(6)?,
            project: row.get(7)?,
            item_key: row.get(8)?,
            created_at_ms: row.get(9)?,
            symbol_kind: row.get(10)?,
            signature_hash: row.get(11)?,
            moniker_tool: row.get(12)?,
            moniker_tool_version: row.get(13)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Author a live memory UPDATE inside the caller's mutation txn: a `NodeUpdate` ONLY when the
/// content actually changed, plus a `NodeStatus` ONLY when the status changed (even to `active`,
/// since the fold needs an explicit op to override a prior non-active status). Content and status
/// are INDEPENDENT LWW registers, so a status-only change must NOT emit a `NodeUpdate` — in a
/// synced multi-writer stream that lifecycle op would re-assert this device's content snapshot at a
/// new Lamport and could revert a concurrent body/title edit from another device. An unknown new
/// status token errors (the write path validates status first, so this is defensive).
pub(crate) fn author_update(
    tx: &Transaction<'_>,
    memory: &RepoMemory,
    content_changed: bool,
    status_changed: bool,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let node_id = NodeId::from(memory.memory_id.as_str());
    let mut ops = Vec::new();
    if content_changed {
        ops.push(MemoryOp::NodeUpdate { node_id: node_id.clone(), content: content_of(memory) });
    }
    if status_changed {
        let status = NodeStatus::from_db_str(&memory.status).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown status token `{}` (a newer binary must author this)",
                memory.status
            )
        })?;
        ops.push(MemoryOp::NodeStatus { node_id, status });
    }
    author_in_owner_stream(tx, &ops, prepared, now_ms)
}

/// Author a live edge ADD (`EdgeAdd`) inside the caller's mutation txn — presence + the durable
/// spec only (no `Rebind`; edge resolution is per-device, recomputed on read).
#[allow(clippy::too_many_arguments)]
pub(crate) fn author_edge_add(
    tx: &Transaction<'_>,
    source_node_id: &str,
    relation: EdgeRelation,
    target_repo_id: &str,
    target_kind: &str,
    target_anchor: &str,
    owner_repo_id: &str,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let op = MemoryOp::EdgeAdd {
        edge: EdgeSpec {
            source_node_id: NodeId::from(source_node_id),
            relation,
            target_repo_id: target_repo_id.to_string(),
            target_kind: target_kind.to_string(),
            target_anchor: target_anchor.to_string(),
            owner_repo_id: owner_repo_id.to_string(),
        },
    };
    author_in_owner_stream(tx, &[op], prepared, now_ms)
}

/// Author a live edge REMOVE (`EdgeRemove` tombstone) inside the caller's mutation txn.
pub(crate) fn author_edge_remove(
    tx: &Transaction<'_>,
    edge_key: &str,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    author_in_owner_stream(
        tx,
        &[MemoryOp::EdgeRemove { edge_key: EdgeKey::from(edge_key) }],
        prepared,
        now_ms,
    )
}

/// The op-model content register for a persisted memory.
fn content_of(memory: &RepoMemory) -> NodeContent {
    node_content(
        &memory.kind,
        &memory.title,
        &memory.body,
        &memory.confidence,
        &memory.source,
        &memory.tags,
        memory.payload_json.as_deref(),
    )
}

/// The repo's memories with NO projected node on `stream` in the accepted-`/3` projection — the
/// rows the signed log is MISSING — in deterministic `(created_at_ms, id)` order, tags attached. On
/// an EMPTY projection this is every memory (genesis); on a populated one, the ghosts a raw writer,
/// an old binary, or pre-existing `/1` history left behind (#541, #664). Reuses the memory
/// subsystem's own tag reader (the op encoder sorts + dedupes anyway).
///
/// This trusts `content_projected_nodes` to mirror the `accepted` flag exactly. Every writer of
/// `accepted` refreshes the projection in the same txn: local authoring reprojects, trusted/local
/// account folds finalize each affected stream immediately, and deferred remote content/account
/// work reprojects at settle before clearing its mark. A future acceptance writer must uphold the
/// same coupling or this anti-join re-authors/skips rows.
fn read_unauthored_memory_rows(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<Vec<MemoryRow>> {
    anyhow::ensure!(
        !rag_rat_oplog::content_stream_has_pending_refold(conn, stream)?,
        "owner stream has a pending content refold; settle pending content refolds before reading \
         memory completeness"
    );
    let mut stmt = conn.prepare(
        // `origin = 'local'` is load-bearing (#691 A-pre): a SYNCED row (projected from a
        // sibling's /3) must never be re-authored as local /3 — even if its acceptance is
        // later revoked and its projection row vanishes — or the local device would forge
        // authorship of, and re-legitimize, content the account revoked. Only
        // locally-authored rows are the reconcile's to complete.
        "SELECT m.id, m.kind, m.title, m.body, m.confidence, m.status, m.source, m.payload_json
         FROM repo_memories m
         WHERE m.repo_id = ?1
           AND m.origin = 'local'
           AND NOT EXISTS (
                 SELECT 1 FROM content_projected_nodes p
                 WHERE p.stream_id = ?2 AND p.node_id = m.id)
         ORDER BY m.created_at_ms, m.id",
    )?;
    let mut rows = stmt
        .query_map(params![repo_id, stream.to_bytes().as_slice()], |row| {
            Ok(MemoryRow {
                memory_id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                body: row.get(3)?,
                confidence: row.get(4)?,
                status: row.get(5)?,
                source: row.get(6)?,
                payload_json: row.get(7)?,
                tags: Vec::new(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for row in &mut rows {
        row.tags = tags_for_memory(conn, &row.memory_id)?;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const REPO: &str = "repo-a";

    /// A conn with the local account minted and the repo's owner stream published, so a test can
    /// plant projected rows against a REAL stream id — the `else { return }` shape silently skips
    /// and proves nothing.
    fn conn_with_stream() -> (Connection, rag_rat_oplog::StreamId) {
        let conn = scoped_conn();
        rag_rat_oplog::local_account(&conn, 1_000).unwrap();
        let stream =
            crate::memory_write::create_memory(&conn, rag_rat_query::memory::RepoMemoryCreate {
                kind: "Concept".to_string(),
                title: "seed".to_string(),
                body: "b".to_string(),
                confidence: "high".to_string(),
                created_by: None,
                source: None,
                tags: Vec::new(),
                payload_json: None,
                bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
            })
            .map(|_| rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap())
            .unwrap();
        (conn, stream)
    }

    /// The sweep must not select a memory whose snapshot is legitimately absent, or it re-examines
    /// it on every authored write forever. Asserted on the QUERY: "no snapshot appeared" is true
    /// whether or not the memory was selected, so it proves nothing.
    #[test]
    fn the_anchor_sweep_skips_a_memory_with_no_bindings() {
        let (conn, stream) = conn_with_stream();
        insert_memory(&conn, "mem_bare", "active", 1);
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_bare', '{}', 'active')",
            params![stream.to_bytes().as_slice()],
        )
        .unwrap();

        let swept = read_anchor_backfill_ids(&conn, REPO, stream).unwrap();
        assert!(!swept.contains(&"mem_bare".to_string()), "swept a memory with no bindings");
    }

    /// The `origin = 'local'` gate the node anti-join also carries: a SYNCED row is a peer's to
    /// publish, and re-authoring one here would forge authorship of content the account may have
    /// revoked.
    #[test]
    fn the_anchor_sweep_never_selects_a_synced_memory() {
        let (conn, stream) = conn_with_stream();
        insert_memory(&conn, "mem_peer", "active", 1);
        conn.execute("UPDATE repo_memories SET origin = 'synced' WHERE id = 'mem_peer'", [])
            .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'path', 'src/lib.rs', 'src/lib.rs', 'current', 1)",
            [REPO],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_peer', '{}', 'active')",
            params![stream.to_bytes().as_slice()],
        )
        .unwrap();

        let swept = read_anchor_backfill_ids(&conn, REPO, stream).unwrap();
        assert!(
            !swept.contains(&"mem_peer".to_string()),
            "a synced memory is the peer's to publish"
        );
    }

    /// The #680 property, for the anchor leg. A snapshot that will never fit a signed entry must
    /// be quarantined rather than left selectable: it never folds, so `anchors_json` stays NULL and
    /// the sweep would re-select it on every pass, reporting work forever and spinning the
    /// reconcile's slow path — which `has_authorable_work` is documented never to do.
    #[test]
    fn an_unpublishable_anchor_set_is_quarantined_not_reported_as_work() {
        let (conn, stream) = conn_with_stream();
        insert_memory(&conn, "mem_fat", "active", 1);
        // Past MAX_ANCHORS_PER_OP, so the op cannot be encoded at any size.
        for index in 0..=rag_rat_oplog::MAX_ANCHORS_PER_OP {
            conn.execute(
                "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, anchor_status,
                     created_at_ms)
                 VALUES (?1, 'mem_fat', 'path', ?2, 'src/lib.rs', 'current', 1)",
                params![REPO, format!("src/lib.rs:{index:04}")],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_fat', '{}', 'active')",
            params![stream.to_bytes().as_slice()],
        )
        .unwrap();

        let work = read_reconcile_work(&conn, REPO, stream, StreamSealPolicy::Plaintext).unwrap();
        assert!(
            work.quarantined_anchor_ids.contains(&"mem_fat".to_string()),
            "an unpublishable set must be quarantined",
        );
        assert!(work.anchor_backfill_ops.is_empty(), "and must not reach the batch",);
        assert!(
            !work.has_authorable_work(),
            "reporting it as work is what spins the slow path forever",
        );
    }

    /// A quarantined memory must not block the ones behind it. It never leaves the match set by
    /// design, and it sorts oldest-first — an over-cap set can only be a legacy row — so if the
    /// scan window doubled as the publish budget, enough of them at the head would stall the
    /// backfill for the rest of the corpus.
    #[test]
    fn a_quarantined_memory_does_not_block_a_publishable_one_behind_it() {
        let (conn, stream) = conn_with_stream();
        // Oldest, and unpublishable: past the per-op anchor cap.
        insert_memory(&conn, "mem_fat", "active", 1);
        for index in 0..=rag_rat_oplog::MAX_ANCHORS_PER_OP {
            conn.execute(
                "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, anchor_status,
                     created_at_ms)
                 VALUES (?1, 'mem_fat', 'path', ?2, 'src/lib.rs', 'current', 1)",
                params![REPO, format!("src/lib.rs:{index:04}")],
            )
            .unwrap();
        }
        // Newer, and perfectly publishable — with a stamped hash, so the sweep emits BOTH of the
        // ops a swept memory owes.
        insert_memory(&conn, "mem_ok", "active", 2);
        set_source_hash(&conn, "mem_ok");
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_ok', 'path', 'src/ok.rs', 'src/ok.rs', 'current', 1)",
            [REPO],
        )
        .unwrap();
        for id in ["mem_fat", "mem_ok"] {
            conn.execute(
                "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
                 VALUES (?1, ?2, '{}', 'active')",
                params![stream.to_bytes().as_slice(), id],
            )
            .unwrap();
        }

        let work = read_reconcile_work(&conn, REPO, stream, StreamSealPolicy::Plaintext).unwrap();
        assert_eq!(work.quarantined_anchor_ids, vec!["mem_fat".to_string()]);
        assert_eq!(
            work.anchor_backfill_ops.len(),
            2,
            "the publishable one is still authored, anchors and the hash describing them",
        );
        assert!(work.has_authorable_work(), "progress is available despite the quarantined head");
    }

    /// The publish budget counts MEMORIES, not ops. Each swept memory owes up to two ops — its
    /// anchors and the hash describing them — so counting ops would halve the budget the moment
    /// hashes exist, and the pass would reach 32 memories instead of 64.
    #[test]
    fn the_publish_budget_counts_memories_not_the_ops_they_owe() {
        let (conn, stream) = conn_with_stream();
        // One more than the budget, all publishable and all hashed, oldest first.
        for index in 0..=ANCHOR_BACKFILL_PER_PASS {
            let id = format!("mem_{index:04}");
            insert_memory(&conn, &id, "active", index as i64);
            set_source_hash(&conn, &id);
            conn.execute(
                "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, anchor_status,
                     created_at_ms)
                 VALUES (?1, ?2, 'path', ?3, 'src/lib.rs', 'current', 1)",
                params![REPO, id, format!("src/lib.rs:{index:04}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
                 VALUES (?1, ?2, '{}', 'active')",
                params![stream.to_bytes().as_slice(), id],
            )
            .unwrap();
        }

        let work = read_reconcile_work(&conn, REPO, stream, StreamSealPolicy::Plaintext).unwrap();

        let swept: BTreeSet<String> = work
            .anchor_backfill_ops
            .iter()
            .map(|op| match op {
                MemoryOp::NodeAnchors { node_id, .. }
                | MemoryOp::NodeSourceHash { node_id, .. } => node_id.as_str().to_string(),
                other => panic!("the sweep authors anchors and hashes only, got {other:?}"),
            })
            .collect();
        assert_eq!(swept.len(), ANCHOR_BACKFILL_PER_PASS, "a full budget of memories");
        assert_eq!(
            work.anchor_backfill_ops.len(),
            ANCHOR_BACKFILL_PER_PASS * 2,
            "each swept memory owes its anchors and the hash describing them",
        );
        assert!(
            !swept.contains(&format!("mem_{ANCHOR_BACKFILL_PER_PASS:04}")),
            "the one past the budget is the next pass's work",
        );
    }

    /// Stamp a memory with a source hash of the shape the local write path produces.
    fn set_source_hash(conn: &Connection, id: &str) {
        conn.execute("UPDATE repo_memories SET source_text_hash = ?2 WHERE id = ?1", params![
            id,
            rag_rat_base::hash::hex_sha256(id.as_bytes())
        ])
        .unwrap();
    }

    /// The positive control for both gates above: a LOCAL memory with a binding and no snapshot IS
    /// swept, so neither test can pass by the query simply returning nothing.
    #[test]
    fn the_anchor_sweep_selects_a_local_memory_owed_a_snapshot() {
        let (conn, stream) = conn_with_stream();
        insert_memory(&conn, "mem_owed", "active", 1);
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_owed', 'path', 'src/lib.rs', 'src/lib.rs', 'current', 1)",
            [REPO],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_owed', '{}', 'active')",
            params![stream.to_bytes().as_slice()],
        )
        .unwrap();

        let swept = read_anchor_backfill_ids(&conn, REPO, stream).unwrap();
        assert!(swept.contains(&"mem_owed".to_string()), "the sweep must select what it is for");
    }

    /// A DB with the memory schema, one registered repo, and the connection scoped to it — the
    /// minimal setup `memory_repo_scope` needs to resolve an active repo.
    fn scoped_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
            [REPO],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [REPO],
        )
        .unwrap();
        conn
    }

    fn insert_memory(conn: &Connection, id: &str, status: &str, created_at_ms: i64) {
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?1, 'body', 'high', ?2, 'agent', ?3, ?3, 'agent', 'h', 'v1',
                 ?4)",
            params![id, status, created_at_ms, REPO],
        )
        .unwrap();
    }

    /// Insert an active memory with a CUSTOM body — used to plant an adversarial oversized body
    /// that a normal rag-rat memory (body ≤ 8 KiB) could never carry.
    fn insert_memory_with_body(conn: &Connection, id: &str, body: &str) {
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?1, ?2, 'high', 'active', 'agent', 100, 100, 'agent', 'h',
                 'v1', ?3)",
            params![id, body, REPO],
        )
        .unwrap();
    }

    /// Point a freshly-opened connection at `repo` via the per-connection TEMP `connection_context`
    /// (temp tables are connection-local, so each thread of the race test scopes its own).
    fn set_scope(conn: &Connection, repo: &str) {
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [repo],
        )
        .unwrap();
    }

    /// The projected `/3` status of `node_id` on the store's owner stream — the completeness mirror
    /// the reconcile heals into. A test DB holds one repo/stream, so no stream filter is needed;
    /// panics if the node is not projected (callers assert presence).
    fn projected_node_status(conn: &Connection, node_id: &str) -> String {
        conn.query_row(
            "SELECT status FROM content_projected_nodes WHERE node_id = ?1",
            [node_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Whether `node_id` has a projected `/3` node on the store's owner stream — the completeness
    /// mirror. Unlike [`projected_node_status`], returns `false` (not a panic) when absent, so a
    /// quarantined ghost can be asserted un-projected (#680).
    fn is_projected(conn: &Connection, node_id: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM content_projected_nodes WHERE node_id = ?1)",
            [node_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            != 0
    }

    /// Insert a node-edge by RAW SQL, bypassing the wired `add_edge` author — a "ghost edge" that
    /// exists in `repo_node_edges` but was never signed into the op-log.
    fn insert_raw_node_edge(conn: &Connection, source: &str, relation: &str, target: &str) {
        let key = rag_rat_query::memory::edge_key(source, relation, "node", target);
        conn.execute(
            "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id,
                 target_kind, target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?2, 'node', ?5, ?5, 'current', 100)",
            params![key, REPO, source, relation, target],
        )
        .unwrap();
    }

    fn queue_pending_refold(conn: &Connection, stream: StreamId) {
        conn.execute("INSERT INTO content_streams_pending_refold(stream_id) VALUES (?1)", [stream
            .to_bytes()
            .as_slice()])
            .unwrap();
    }

    /// Author a BARE `/3` NodeStatus for a node with NO `NodeCreate` — the `/3` analog of the INERT
    /// op a pre-fix (#532) binary authored when it `mark_obsolete`'d a still-ghost memory. The
    /// shared projector emits no node without content, so this leaves a stale status register
    /// with no projected node. Requires the store's account + owner stream already established
    /// (via a prior live create); it authors through the real `/3` seam so the register truly
    /// lands.
    fn author_inert_status_op(conn: &Connection, node_id: &str, status: NodeStatus) {
        let stream = rag_rat_oplog::owned_stream_v2_id(conn, REPO)
            .unwrap()
            .expect("account minted by a prior live create");
        let tx = conn.unchecked_transaction().unwrap();
        rag_rat_oplog::author_content_batch_in_tx(
            &tx,
            stream,
            &[MemoryOp::NodeStatus { node_id: NodeId::from(node_id), status }],
            100,
        )
        .unwrap();
        tx.commit().unwrap();
    }

    /// The `/3` content chain length — one entry per authored op, the retarget's signed op-log
    /// (account genesis + `StreamOwn` live in `account_entries`, not counted here).
    fn entry_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_entries", [], |r| r.get(0)).unwrap()
    }

    fn content_suites(conn: &Connection) -> Vec<u64> {
        let mut stmt = conn
            .prepare("SELECT signed_bytes FROM content_entries WHERE accepted = 1 ORDER BY rowid")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|bytes| {
                rag_rat_oplog::decode_content_signed(&bytes.unwrap()).unwrap().header.crypto_suite
            })
            .collect()
    }

    fn projected_node_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_projected_nodes", [], |r| r.get(0)).unwrap()
    }

    /// The number of local accounts minted (the single-row pointer table): 0 before any established
    /// reconcile, 1 after — proves a no-op / scope-gated path mints NO account.
    fn local_account_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM oplog_local_account", [], |r| r.get(0)).unwrap()
    }

    /// The number of folded `StreamOwn` ownership facts — one per owned `/2` stream.
    fn owned_stream_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM account_stream_ownership", [], |r| r.get(0)).unwrap()
    }

    /// #560 durability split: an authored write commits under `synchronous = FULL`, and the guard
    /// restores the connection's `NORMAL` default on drop so derived-index writes are unaffected.
    /// Uses a file-backed index connection (WAL + NORMAL, like every real open) because an
    /// in-memory database ignores the `synchronous` setting and would not report the change.
    #[test]
    fn authored_durability_raises_full_then_restores_normal() {
        let dir = rag_rat_base::test_scratch::ScratchDir::new("authdur");
        let storage = rag_rat_db::storage::IndexConnection::open(&dir.join("index.db")).unwrap();
        let conn = storage.connection();
        let synchronous = |c: &Connection| -> i64 {
            c.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap()
        };

        assert_eq!(synchronous(conn), 1, "an index connection defaults to synchronous=NORMAL (=1)");
        {
            let _durability = AuthoredDurability::begin(conn).unwrap();
            assert_eq!(
                synchronous(conn),
                2,
                "an authored write must raise synchronous=FULL (=2) for its commit"
            );
        }
        assert_eq!(
            synchronous(conn),
            1,
            "the authored-durability guard must restore synchronous=NORMAL (=1) on drop"
        );
    }

    /// A `MemoryRow` with the given status, no payload, one tag — the fixture the ported op-split
    /// tests translate.
    fn op_row(status: &str) -> MemoryRow {
        MemoryRow {
            memory_id: "mem_a".to_string(),
            kind: "Invariant".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            confidence: "high".to_string(),
            status: status.to_string(),
            source: "agent".to_string(),
            payload_json: None,
            tags: vec!["x".to_string()],
        }
    }

    #[test]
    fn node_ops_and_edge_add_op_translate_content_status_and_an_edge() {
        // GENESIS (elide=true) on a non-active memory: NodeCreate then a NodeStatus (obsolete is
        // not the active default). `edge_add_op` yields one EdgeAdd and DELIBERATELY no
        // Rebind — the per-device resolved dimension (target_node_id / anchor_status) is
        // recomputed on read, never signed into the log.
        let ops = node_ops(&op_row("obsolete"), true).unwrap();
        assert_eq!(ops.len(), 2);
        assert!(
            matches!(&ops[0], MemoryOp::NodeCreate { node_id, .. } if node_id.as_str() == "mem_a")
        );
        assert!(
            matches!(&ops[1], MemoryOp::NodeStatus { status, .. } if status.as_db_str() == "obsolete")
        );
        let edge = NodeEdge {
            edge_key: "k1".to_string(),
            source_node_id: "mem_a".to_string(),
            relation: "relates_to".to_string(),
            target_repo_id: REPO.to_string(),
            target_kind: "node".to_string(),
            target_anchor: "mem_b".to_string(),
            target_node_id: Some("mem_b".to_string()),
            anchor_status: "current".to_string(),
        };
        let edge_op = edge_add_op(&edge, REPO).unwrap();
        assert!(matches!(&edge_op, MemoryOp::EdgeAdd { .. }));
        let all_ops: Vec<MemoryOp> = ops.into_iter().chain(std::iter::once(edge_op)).collect();
        assert!(
            !all_ops.iter().any(|op| matches!(op, MemoryOp::Rebind { .. })),
            "the reconcile omits the per-device resolved dimension"
        );
    }

    #[test]
    fn genesis_node_ops_for_an_active_memory_emit_no_status_op() {
        // elide=true (genesis, no stale registers): an active, edgeless memory is just its
        // NodeCreate — the fold's create-time default handles `active`.
        let ops = node_ops(&op_row("active"), true).unwrap();
        assert_eq!(ops.len(), 1, "an active memory on genesis is just its NodeCreate");
        assert!(matches!(&ops[0], MemoryOp::NodeCreate { .. }));
    }

    #[test]
    fn incremental_node_ops_for_an_active_memory_do_emit_an_explicit_status() {
        // elide=false (incremental heal on a non-empty chain): ALWAYS emit NodeStatus, even
        // `active`, so a healed node's status wins its register at the new Lamport and overrides
        // any stale register a prior inert op left behind (decision 6 of #541).
        let ops = node_ops(&op_row("active"), false).unwrap();
        assert_eq!(ops.len(), 2, "an active memory on a heal emits an explicit NodeStatus");
        assert!(matches!(&ops[0], MemoryOp::NodeCreate { .. }));
        assert!(
            matches!(&ops[1], MemoryOp::NodeStatus { status, .. } if status.as_db_str() == "active"),
            "the incremental branch emits NodeStatus{{active}} to win the register"
        );
    }

    #[test]
    fn node_ops_fails_on_an_unknown_status() {
        // A status token this binary can't map must FAIL, not silently default the signed history
        // to `active`. Holds in either branch — an unknown token is never the active
        // default.
        let err = node_ops(&op_row("future_status_from_a_newer_binary"), true).unwrap_err();
        assert!(err.to_string().contains("unknown status"), "an unknown status fails authoring");
    }

    #[test]
    fn sealed_authorability_reserves_the_aead_overhead() {
        let mut row = op_row("active");
        let mut found = false;
        for len in 261_000..262_500 {
            row.body = "x".repeat(len);
            if node_is_authorable(&row, StreamSealPolicy::Plaintext)
                && !node_is_authorable(&row, StreamSealPolicy::Sealed)
            {
                found = true;
                break;
            }
        }
        assert!(found, "the sealed policy rejects an op in the 40-byte AEAD overhead window");
    }

    #[test]
    fn empty_preparation_mints_no_key_and_arms_no_policy() {
        let conn = scoped_conn();
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert!(prepare_live_authoring(&conn, &[], 2_000).unwrap().is_none());
        let secret_entries: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_entries WHERE log_id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(secret_entries, 0);
        assert_eq!(
            rag_rat_db::meta::repo_meta(&conn, REPO, STREAM_SEAL_POLICY_META_KEY).unwrap(),
            None
        );
    }

    /// #541/#664: the reconcile's memory reader anti-joins `repo_memories` against the accepted-`/3`
    /// projection `content_projected_nodes` — only a row with no projected node (never signed)
    /// comes back. `stream` is an opaque `StreamId` here (the anti-join only needs seed/query
    /// agreement).
    #[test]
    fn read_unauthored_memory_rows_returns_only_rows_absent_from_the_projection() {
        let conn = scoped_conn();
        insert_memory(&conn, "mem_live", "active", 100);
        insert_memory(&conn, "mem_ghost", "active", 200);
        let stream = StreamId::from_bytes([0x11; 32]);
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_live', '{}', 'active')",
            params![stream.to_bytes().as_slice()],
        )
        .unwrap();
        let missing = read_unauthored_memory_rows(&conn, REPO, stream).unwrap();
        assert_eq!(missing.iter().map(|r| r.memory_id.as_str()).collect::<Vec<_>>(), ["mem_ghost"]);
    }

    /// A SYNCED memory is never the reconcile's to author — even absent from the projection (its
    /// acceptance was revoked) — or the local device would forge authorship of, and re-legitimize,
    /// content the account revoked (#691 A-pre, Trace 2). A local ghost in the same position WOULD
    /// be authored.
    #[test]
    fn a_synced_memory_is_never_re_authored() {
        let conn = scoped_conn();
        insert_memory(&conn, "mem_synced", "active", 100);
        conn.execute("UPDATE repo_memories SET origin = 'synced' WHERE id = 'mem_synced'", [])
            .unwrap();
        let stream = StreamId::from_bytes([0x22; 32]);
        assert!(
            read_unauthored_memory_rows(&conn, REPO, stream).unwrap().is_empty(),
            "a synced memory is not re-authored even when absent from the projection",
        );
    }

    // --- the per-node/edge self-healing reconcile (#541) ---

    #[test]
    fn a_ghost_memory_is_authored_on_the_next_reconcile() {
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap(); // roots the chain via genesis
        insert_memory(&conn, "mem_ghost", "obsolete", 500); // raw, un-authored ghost
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(
            projected_node_status(&conn, "mem_ghost"),
            NodeStatus::Obsolete.as_db_str(),
            "the ghost is now authored with its create-time status",
        );
    }

    #[test]
    fn heal_overrides_a_stale_status_register_left_by_an_inert_op() {
        // The decision-6 divergence: an inert NodeStatus{obsolete} exists, the row is now active,
        // the heal must author an explicit NodeStatus{active} so the projection matches the table.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap(); // establishes the account + owner stream
        // Author an inert `/3` NodeStatus for a NOT-yet-created ghost (the /3 analog of the old
        // binary's inert op): a bare status register with no projected node.
        let ghost = "mem_ghost";
        author_inert_status_op(&conn, ghost, NodeStatus::Obsolete);
        insert_memory(&conn, ghost, "active", 500); // the table says active
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(
            projected_node_status(&conn, ghost),
            NodeStatus::Active.as_db_str(),
            "explicit NodeStatus{{active}} overrode the stale register",
        );
    }

    #[test]
    fn a_ghost_edge_on_a_live_node_is_authored_on_the_next_reconcile() {
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        insert_raw_node_edge(&conn, &a, "relates_to", &b); // writes repo_node_edges directly
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(projected_edge_count(&conn), 1, "ghost edge now signed");
    }

    #[test]
    fn reconcile_is_idempotent_and_a_clean_repo_authors_nothing() {
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap();
        let before = entry_count(&conn);
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(entry_count(&conn), before, "no ghost → no new /3 entry");
    }

    /// Abort the queue-clear DELETE for `stream`, poisoning its settle so an inline barrier settle
    /// cannot clear the mark — the row's refold debt is retained and the barrier stays tripped.
    fn poison_owner_stream_settle(conn: &Connection, stream: StreamId) {
        let hex: String = stream.to_bytes().iter().map(|byte| format!("{byte:02x}")).collect();
        conn.execute_batch(&format!(
            "CREATE TRIGGER poison_owner_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             WHEN OLD.stream_id = X'{hex}'
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;"
        ))
        .unwrap();
    }

    #[test]
    fn pending_owner_refold_inline_settles_on_the_mutation_path() {
        // #798 finding 5: a tripped barrier on the AUTOCOMMIT mutation path no longer hard-fails —
        // it attempts ONE inline, targeted settle of the owner stream and, when that clears the
        // debt, PROCEEDS. A remote ingest enqueuing owner-stream debt must not wedge every local
        // mutation behind a settle no local caller schedules.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap();
        let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
        insert_memory(&conn, "mem_ghost", "active", 500);
        queue_pending_refold(&conn, stream);

        // The reconcile trips the barrier, inline-settles the settle-able owner stream, and then
        // authors the ghost — no manual settle needed.
        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert!(is_projected(&conn, "mem_ghost"), "the inline-settled reconcile authors the ghost");
        assert!(
            !rag_rat_oplog::content_stream_has_pending_refold(&conn, stream).unwrap(),
            "the inline settle cleared the owner stream's refold debt",
        );

        // A subsequent mutation on the same clean stream also succeeds and authors no duplicates.
        create_concept(&conn, "after inline settle").unwrap();
        let entries_after = entry_count(&conn);
        backfill_memory_oplog(&conn, 10_000).unwrap();
        assert_eq!(entry_count(&conn), entries_after, "the retry authors no duplicates");
    }

    #[test]
    fn a_still_pending_owner_refold_errors_after_a_failed_in_tx_settle() {
        // #798 finding 5: the barrier self-heals only when the in-transaction settle actually
        // clears the debt. A stream whose settle keeps failing (poisoned) rolls the whole write
        // back, so the barrier stays FAIL-CLOSED rather than reading a stale accepted-/3
        // projection — the debt survives and nothing is authored.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap();
        let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
        insert_memory(&conn, "mem_ghost", "active", 500);
        queue_pending_refold(&conn, stream);
        poison_owner_stream_settle(&conn, stream);
        let entries_before = entry_count(&conn);

        // The failure now surfaces from the settle itself (it is attempted inside the write's own
        // transaction) rather than from a barrier that refused to try.
        let reconcile_err = format!("{:#}", backfill_memory_oplog(&conn, 9_000).unwrap_err());
        assert!(
            reconcile_err.contains("pending content refold"),
            "the rolled-back write names the unsettled owner stream: {reconcile_err}",
        );
        let mutation_err = format!("{:#}", create_concept(&conn, "blocked mutation").unwrap_err());
        assert!(
            mutation_err.contains("pending content refold"),
            "a live mutation fails closed the same way: {mutation_err}",
        );
        assert_eq!(entry_count(&conn), entries_before, "the still-tripped barrier authors nothing");
        assert!(
            rag_rat_oplog::content_stream_has_pending_refold(&conn, stream).unwrap(),
            "the poisoned settle retained the refold debt",
        );
        assert!(
            !is_projected(&conn, "mem_ghost"),
            "the ghost is not authored while the barrier trips",
        );
    }

    #[test]
    fn pending_owner_refold_inline_settles_on_the_edge_reconcile_path() {
        // #798 finding 5, edge path: the same inline settle unblocks a ghost EDGE reconcile.
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        insert_raw_node_edge(&conn, &a, "relates_to", &b);
        let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
        queue_pending_refold(&conn, stream);

        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert_eq!(projected_edge_count(&conn), 1, "the inline-settled reconcile authors the edge");
        assert!(
            !rag_rat_oplog::content_stream_has_pending_refold(&conn, stream).unwrap(),
            "the inline settle cleared the owner stream's refold debt",
        );
        let entries_after = entry_count(&conn);
        backfill_memory_oplog(&conn, 11_000).unwrap();
        assert_eq!(entry_count(&conn), entries_after, "the retry authors no duplicate edge");
    }

    #[test]
    fn pending_unrelated_stream_does_not_block_owner_reconcile() {
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap();
        insert_memory(&conn, "mem_ghost", "active", 500);
        let unrelated = StreamId::from_bytes([0x51; 32]);
        queue_pending_refold(&conn, unrelated);

        backfill_memory_oplog(&conn, 9_000).unwrap();
        assert!(is_projected(&conn, "mem_ghost"));
        assert!(rag_rat_oplog::content_stream_has_pending_refold(&conn, unrelated).unwrap());
    }

    #[test]
    fn an_unreadable_status_ghost_fails_the_mutation_path_loudly() {
        // Blast-radius pin: a ghost carrying a status token THIS binary cannot decode makes the
        // whole reconcile (hence the mutation that triggered it) fail, rather than silently minting
        // `active`.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap();
        insert_memory(&conn, "mem_future", "some_future_status", 500);
        assert!(backfill_memory_oplog(&conn, 9_000).is_err());
    }

    #[test]
    fn backfill_authors_every_memory_and_is_idempotent() {
        let conn = scoped_conn();
        insert_memory(&conn, "mem_a", "active", 100);
        insert_memory(&conn, "mem_b", "active", 200);
        insert_memory(&conn, "mem_c", "active", 300);
        // Insert a typed edge FROM mem_b DIRECTLY (not via the now-live `add_edge`, which would
        // author it eagerly and defeat this isolation test), THEN mark mem_b obsolete — so the
        // explicit backfill below is the SOLE authoring path and must still capture the edge from a
        // now-non-live source (the complete-history guard: the live reader hides it).
        let ek = rag_rat_query::memory::edge_key("mem_b", "relates_to", "node", "mem_c");
        conn.execute(
            "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, 'mem_b', 'relates_to', ?2, 'node', 'mem_c', 'mem_c', 'current', 0)",
            params![ek, REPO],
        )
        .unwrap();
        conn.execute("UPDATE repo_memories SET status = 'obsolete' WHERE id = 'mem_b'", [])
            .unwrap();

        backfill_memory_oplog(&conn, 1_000).unwrap();
        // 3 NodeCreate + 1 NodeStatus (mem_b obsolete) + 1 EdgeAdd (from obsolete mem_b) = 5 `/3`
        // content entries; 3 projected nodes.
        assert_eq!(entry_count(&conn), 5);
        assert_eq!(projected_node_count(&conn), 3);
        assert_eq!(projected_node_status(&conn, "mem_b"), "obsolete");
        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM content_projected_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 1);

        // A second backfill is a no-op — the atomic batch already completed (chain non-empty).
        backfill_memory_oplog(&conn, 2_000).unwrap();
        assert_eq!(entry_count(&conn), 5, "re-running backfill authors nothing more");
    }

    #[test]
    fn backfill_is_a_noop_on_the_placeholder_repo() {
        // An unadopted DB scoped to the legacy `__unassigned__` placeholder: backfilling would sign
        // an immutable owner stream that adoption can never re-point, so it must no-op even with
        // memories present.
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [rag_rat_base::repo_identity::LEGACY_REPO_ID],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES ('mem_a', 'Invariant', 'mem_a', 'body', 'high', 'active', 'agent', 1, 1,
                 'agent', 'h', 'v1', ?1)",
            [rag_rat_base::repo_identity::LEGACY_REPO_ID],
        )
        .unwrap();

        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0, "the placeholder repo is not backfilled");
        assert_eq!(
            local_account_count(&conn),
            0,
            "the scope gate no-ops BEFORE the mint — a placeholder repo mints no account",
        );
    }

    #[test]
    fn backfill_is_a_noop_on_a_local_only_repo() {
        // A machine-local `local:` shallow-clone id is upgraded to a portable id when the clone is
        // deepened, re-pointing the rows — so an immutable owner stream must not be rooted on it.
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        let local_id = format!("{}deadbeef", rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX);
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [&local_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES ('mem_a', 'Invariant', 'mem_a', 'body', 'high', 'active', 'agent', 1, 1,
                 'agent', 'h', 'v1', ?1)",
            [&local_id],
        )
        .unwrap();

        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0, "a local: repo is not backfilled");
        assert_eq!(local_account_count(&conn), 0, "a local: repo mints no account");
    }

    #[test]
    fn backfill_is_a_noop_on_an_unscoped_db() {
        // No repos row, no connection scope → memory_repo_scope is None → nothing to root a stream.
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0);
        assert_eq!(local_account_count(&conn), 0, "an unscoped DB mints no account");
    }

    #[test]
    fn backfill_of_an_empty_scoped_repo_establishes_ownership_but_authors_no_content() {
        // A fresh scoped repo with no memories: the reconcile falls through the fast-path probe
        // (ownership not yet established), mints the account, and publishes the `/2` StreamOwn —
        // but authors NO `/3` content (nothing is missing). So the content chain stays
        // empty while ownership is now live (the first live op will chain off an empty
        // content chain = genesis).
        let conn = scoped_conn();
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(entry_count(&conn), 0, "no memories ⇒ no /3 content entries");
        assert_eq!(local_account_count(&conn), 1, "a scoped repo mints the store's local account");
        assert_eq!(owned_stream_count(&conn), 1, "and publishes exactly one /2 StreamOwn");
    }

    // --- live write-path wiring (#532) ---

    /// Count LIVE projected edges (`present = 1`). A removed edge is retained as a tombstone
    /// (`present = 0`, #691 A-pre) rather than deleted, so "how many edges are present" filters
    /// them.
    fn projected_edge_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_projected_edges WHERE present = 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    /// Create an unanchored `Concept` (needs no code binding) through the LIVE `create_memory`.
    fn create_concept(
        conn: &Connection,
        title: &str,
    ) -> anyhow::Result<rag_rat_query::memory::RepoMemoryCreateResult> {
        crate::memory_write::create_memory(conn, rag_rat_query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
    }

    #[test]
    fn create_memory_authors_a_projected_node() {
        let conn = scoped_conn();
        let r = create_concept(&conn, "t1").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = ?1",
                [&r.memory.memory_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the created memory is a projected /3 node");
        assert_eq!(projected_node_count(&conn), 1);
        assert_eq!(content_suites(&conn), [0], "plaintext remains the default for existing repos");
    }

    #[test]
    fn enable_is_idempotent_and_subsequent_live_authoring_is_sealed_and_projected() {
        let conn = scoped_conn();
        assert!(enable_sealed_authoring(&conn, 1_000).unwrap());
        assert!(!enable_sealed_authoring(&conn, 2_000).unwrap());
        assert_eq!(
            rag_rat_db::meta::repo_meta(&conn, REPO, STREAM_SEAL_POLICY_META_KEY)
                .unwrap()
                .as_deref(),
            Some("sealed")
        );
        let accepted_wraps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_entries WHERE log_id = 1 AND accepted = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(accepted_wraps, 1, "enable establishes exactly one accepted first-key wrap");

        let first = create_concept(&conn, "sealed one").unwrap().memory.memory_id;
        let second = create_concept(&conn, "sealed two").unwrap().memory.memory_id;
        assert_eq!(content_suites(&conn), [1, 1]);
        assert!(is_projected(&conn, &first));
        assert!(is_projected(&conn, &second));

        crate::memory_write::update_memory(&conn, rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: first.clone(),
            kind: None,
            title: None,
            body: Some("sealed update".to_string()),
            confidence: None,
            status: Some("obsolete".to_string()),
            tags: None,
            payload_json: None,
        })
        .unwrap();
        let edge = crate::memory_write::add_edge(
            &conn,
            &second,
            EdgeRelation::RelatesTo,
            &rag_rat_query::memory::EdgeTarget::Node { repo_id: None, node_id: first },
        )
        .unwrap();
        crate::memory_write::remove_edge(&conn, &edge.edge_key).unwrap();
        assert!(content_suites(&conn).into_iter().all(|suite| suite == 1));
    }

    #[test]
    fn publish_is_idempotent_makes_the_account_public_and_authors_public() {
        let conn = scoped_conn();
        assert!(enable_public_authoring(&conn, 1_000).unwrap());
        assert!(!enable_public_authoring(&conn, 2_000).unwrap());
        assert_eq!(
            rag_rat_db::meta::repo_meta(&conn, REPO, STREAM_ACCESS_MODE_META_KEY)
                .unwrap()
                .as_deref(),
            Some("public")
        );
        let account = rag_rat_oplog::local_account(&conn, 1_000).unwrap();
        assert!(rag_rat_oplog::account_is_fully_public(&conn, account).unwrap());

        // Live-write, drain, and reconcile all resolve the SAME public stream id — distinct from
        // the Private-mode id, so nothing desyncs.
        let public_id = rag_rat_oplog::owned_stream_v2_id_with_mode(
            &conn,
            REPO,
            rag_rat_oplog::AccessMode::PublicRead,
        )
        .unwrap()
        .unwrap();
        let private_id = rag_rat_oplog::owned_stream_v2_id_with_mode(
            &conn,
            REPO,
            rag_rat_oplog::AccessMode::Private,
        )
        .unwrap()
        .unwrap();
        assert_ne!(public_id, private_id, "the public stream has a distinct identity");
        assert_eq!(stable_owner_stream_for_repo(&conn, REPO).unwrap(), Some(public_id));

        // A created memory authors onto the public stream and the account stays fully public.
        let m = create_concept(&conn, "public one").unwrap().memory.memory_id;
        assert!(is_projected(&conn, &m));
        assert!(rag_rat_oplog::account_is_fully_public(&conn, account).unwrap());
    }

    #[test]
    fn publish_refuses_an_account_that_already_has_private_memories() {
        let conn = scoped_conn();
        // Authors a Private `/2` StreamOwn — the account can never become fully public thereafter.
        create_concept(&conn, "private history").unwrap();
        let err = enable_public_authoring(&conn, 2_000).unwrap_err().to_string();
        assert!(err.contains("private stream") || err.contains("fresh index"), "got: {err}");
    }

    #[test]
    fn publish_and_seal_are_mutually_exclusive_both_directions() {
        let sealed_first = scoped_conn();
        enable_sealed_authoring(&sealed_first, 1_000).unwrap();
        assert!(
            enable_public_authoring(&sealed_first, 2_000).is_err(),
            "a sealed repo cannot be published"
        );

        let public_first = scoped_conn();
        enable_public_authoring(&public_first, 1_000).unwrap();
        assert!(
            enable_sealed_authoring(&public_first, 2_000).is_err(),
            "a published repo cannot be sealed"
        );
    }

    #[test]
    fn publish_access_mode_ratchet_survives_a_deleted_intent_row() {
        let conn = scoped_conn();
        enable_public_authoring(&conn, 1_000).unwrap();
        let account = rag_rat_oplog::local_account(&conn, 1_000).unwrap();
        // Simulate intent-row loss (external tooling / a meta bug): the op-log's PublicRead
        // StreamOwn must keep the mode public, or the next write authors a second (Private)
        // StreamOwn and permanently mixes the account.
        conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", rusqlite::params![
            REPO,
            STREAM_ACCESS_MODE_META_KEY
        ])
        .unwrap();
        assert_eq!(
            owner_stream_access_mode(&conn, REPO).unwrap(),
            rag_rat_oplog::AccessMode::PublicRead,
            "the derived op-log fact keeps the ratchet public after intent-row loss"
        );
        create_concept(&conn, "after intent loss").unwrap();
        assert!(
            rag_rat_oplog::account_is_fully_public(&conn, account).unwrap(),
            "a lost intent row must not let a Private StreamOwn mix the published account"
        );
    }

    #[test]
    fn catch_up_is_idempotent_for_an_already_covered_effective_device() {
        let conn = scoped_conn();
        enable_sealed_authoring(&conn, 1_000).unwrap();
        let target = rag_rat_oplog::local_device(&conn, 1_000).unwrap().fingerprint();

        let first = catch_up_enrolled_device_keys(&conn, target, 2_000).unwrap();
        assert!(first.authored.is_empty());
        assert_eq!(first.already_covered.len(), 1);
        let second = catch_up_enrolled_device_keys(&conn, target, 3_000).unwrap();
        assert!(second.authored.is_empty());
        assert_eq!(second.already_covered, first.already_covered);
    }

    #[test]
    fn catch_up_rejects_a_non_effective_target_without_partial_rows() {
        let conn = scoped_conn();
        enable_sealed_authoring(&conn, 1_000).unwrap();
        let before = entry_count(&conn);

        let err = catch_up_enrolled_device_keys(
            &conn,
            rag_rat_oplog::DeviceFingerprint::from_bytes([0xff; 32]),
            2_000,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not currently roster-effective"));
        assert_eq!(entry_count(&conn), before);
        let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
        assert_eq!(synchronous, 1, "the durability guard restores NORMAL after rollback");
    }

    #[test]
    fn catch_up_only_reports_the_active_repos_owner_stream() {
        let conn = scoped_conn();
        enable_sealed_authoring(&conn, 1_000).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b', \
             'repo-b', 0)",
            [],
        )
        .unwrap();
        set_scope(&conn, "repo-b");
        enable_sealed_authoring(&conn, 2_000).unwrap();
        let target = rag_rat_oplog::local_device(&conn, 2_000).unwrap().fingerprint();

        set_scope(&conn, REPO);
        let report = catch_up_enrolled_device_keys(&conn, target, 3_000).unwrap();
        assert!(report.authored.is_empty());
        assert_eq!(report.already_covered.len(), 1, "repo-b's live key is outside repo-a scope");
    }

    #[test]
    fn sealed_reconcile_authors_a_ghost_once() {
        let conn = scoped_conn();
        enable_sealed_authoring(&conn, 1_000).unwrap();
        insert_memory(&conn, "sealed-ghost", "active", 100);
        backfill_memory_oplog(&conn, 2_000).unwrap();
        assert_eq!(content_suites(&conn), [1]);
        assert!(is_projected(&conn, "sealed-ghost"));
        backfill_memory_oplog(&conn, 3_000).unwrap();
        assert_eq!(
            content_suites(&conn),
            [1],
            "repeat reconcile does not duplicate sealed history"
        );
    }

    #[test]
    fn sealed_policy_is_repo_scoped_and_the_ratchet_survives_deleted_intent() {
        let conn = scoped_conn();
        enable_sealed_authoring(&conn, 1_000).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b', \
             'repo-b', 0)",
            [],
        )
        .unwrap();
        set_scope(&conn, "repo-b");
        let b = create_concept(&conn, "repo b plaintext").unwrap().memory.memory_id;
        assert!(is_projected(&conn, &b));
        assert_eq!(content_suites(&conn), [0], "a sibling repo remains plaintext by default");

        set_scope(&conn, REPO);
        conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", params![
            REPO,
            STREAM_SEAL_POLICY_META_KEY
        ])
        .unwrap();
        create_concept(&conn, "ratcheted sealed").unwrap();
        assert_eq!(content_suites(&conn), [0, 1], "a wrap prevents plaintext downgrade");
    }

    #[test]
    fn policy_revalidation_failure_rolls_back_the_table_mutation() {
        let conn = scoped_conn();
        backfill_memory_oplog(&conn, 1_000).unwrap();
        let prepared = prepare_live_content_authoring(&conn, 2_000).unwrap().unwrap();
        rag_rat_db::meta::set_repo_meta(&conn, REPO, STREAM_SEAL_POLICY_META_KEY, "sealed")
            .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        insert_memory(&tx, "must-roll-back", "active", 100);
        let memory = rag_rat_query::memory::memory_by_id(&tx, "must-roll-back").unwrap().unwrap();
        assert!(author_create(&tx, &memory, Some(&prepared), 2_000).is_err());
        drop(tx);
        assert!(rag_rat_query::memory::memory_by_id(&conn, "must-roll-back").unwrap().is_none());
    }

    #[test]
    fn a_scope_less_create_authors_nothing() {
        // No repos row / no active-repo context → the scope gate skips authoring entirely.
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        create_concept(&conn, "t1").unwrap();
        assert_eq!(entry_count(&conn), 0, "a scope-less create never touches the log");
        assert_eq!(local_account_count(&conn), 0, "a scope-less create mints no account");
    }

    #[test]
    fn update_memory_authors_node_update_and_a_status_change() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
        crate::memory_write::update_memory(&conn, rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: id.clone(),
            kind: None,
            title: None,
            body: Some("a new body".to_string()),
            confidence: None,
            status: Some("obsolete".to_string()),
            tags: None,
            payload_json: None,
        })
        .unwrap();
        let (content_json, status): (String, String) = conn
            .query_row(
                "SELECT content_json, status FROM content_projected_nodes WHERE node_id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(content_json.contains("a new body"), "the NodeUpdate replaced the content");
        assert_eq!(status, "obsolete", "the status change authored a NodeStatus");
    }

    #[test]
    fn mark_obsolete_authors_only_a_status_op_no_node_update() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
        // The NodeCreate is the sole entry so far.
        assert_eq!(entry_count(&conn), 1);
        crate::memory_write::mark_obsolete(&conn, &id).unwrap();
        assert_eq!(projected_node_status(&conn, &id), "obsolete");
        // A status-only change authors EXACTLY ONE op (a NodeStatus) — NOT a NodeUpdate, which in a
        // synced stream could revert a concurrent content edit (content/status are independent
        // LWW).
        assert_eq!(
            entry_count(&conn),
            2,
            "status-only update authors one NodeStatus, no NodeUpdate"
        );
    }

    #[test]
    fn a_no_op_update_authors_nothing() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
        assert_eq!(entry_count(&conn), 1);
        // An update that changes neither content nor status is a complete no-op in the log.
        crate::memory_write::update_memory(&conn, rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: id,
            kind: None,
            title: None,
            body: None,
            confidence: None,
            status: None,
            tags: None,
            payload_json: None,
        })
        .unwrap();
        assert_eq!(entry_count(&conn), 1, "a change-free update authors no op");
    }

    #[test]
    fn add_and_remove_edge_author_edge_presence() {
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        let edge = crate::memory_write::add_edge(&conn, &a, EdgeRelation::RelatesTo, &{
            rag_rat_query::memory::EdgeTarget::Node { repo_id: None, node_id: b }
        })
        .unwrap();
        assert_eq!(projected_edge_count(&conn), 1, "add_edge authored an EdgeAdd");
        assert!(crate::memory_write::remove_edge(&conn, &edge.edge_key).unwrap());
        assert_eq!(projected_edge_count(&conn), 0, "remove_edge authored an EdgeRemove tombstone");
    }

    #[test]
    fn add_edge_rejects_an_oversized_target_anchor_at_write_validation() {
        // #680 (prevention): the write-boundary cap rejects an oversized `target_anchor` BEFORE the
        // row is persisted, so the normal API can never mint the un-authorable edge the reconcile
        // would otherwise have to quarantine. An EXPLICIT cross-repo target to a not-yet-indexed
        // repo is the one path that stores the caller's raw anchor verbatim, so it exercises the
        // cap.
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let oversized = "x".repeat(rag_rat_query::memory::MAX_EDGE_ANCHOR_LEN + 1);
        let err = crate::memory_write::add_edge(&conn, &a, EdgeRelation::RelatesTo, &{
            rag_rat_query::memory::EdgeTarget::Node {
                repo_id: Some("some-unindexed-repo".to_string()),
                node_id: oversized,
            }
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("over the"),
            "the byte cap rejects an oversized edge anchor: {err}",
        );
        let edges: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_node_edges", [], |r| r.get(0)).unwrap();
        assert_eq!(edges, 0, "no edge row is stored when the anchor is over the cap");
    }

    #[test]
    fn a_node_with_too_many_tags_is_rejected_at_write_validation() {
        // #680 (P2): title/body are char-capped and payload is byte-capped, but the NUMBER of
        // tags is not — each tag is individually validated (≤ 64 chars) with no limit on how many.
        // Enough individually-valid tags overflow the signed `/3` envelope even with a tiny body
        // and no payload, so an "otherwise valid" create assembles an un-authorable
        // `NodeCreate`. Before the whole-op write-boundary guard that op was minted and
        // then SILENTLY quarantined by the reconcile; now it is rejected at write time with
        // an actionable error and no row persists.
        let conn = scoped_conn();
        // ~5000 unique 64-char tags: safely past the ~4000 the envelope admits, each within the
        // per-tag cap so it is INDIVIDUALLY valid (i.e. would have been accepted before this fix).
        let tags: Vec<String> = (0..5000).map(|i| format!("tag-{i:060}")).collect();
        for tag in &tags {
            assert_eq!(tag.chars().count(), 64, "each tag is exactly the 64-char per-tag cap");
            rag_rat_query::memory::validate_len("tag", tag, 64)
                .expect("each tag is individually valid — only the aggregate is un-authorable");
        }
        let err =
            crate::memory_write::create_memory(&conn, rag_rat_query::memory::RepoMemoryCreate {
                kind: "Concept".to_string(),
                title: "too many tags".to_string(),
                body: "body".to_string(),
                confidence: "high".to_string(),
                created_by: None,
                source: None,
                tags,
                payload_json: None,
                bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("too large"),
            "the whole-op guard rejects the tag aggregate at write time: {err}",
        );
        // Rejected at the write boundary — the row never persists (not accepted-then-quarantined).
        let rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 0, "the un-authorable create rolled back — nothing was persisted");
    }

    #[test]
    fn the_whole_op_guard_rejects_an_aggregate_no_single_field_cap_catches() {
        // #680 (the point of the ROOT fix): an op can overflow the signed `/3` envelope from
        // the SUM of fields that are EACH within their own cap — a max-ish payload + a max body +
        // many tags together. No single per-field cap (payload ≤ 128 KiB, body ≤ 8000 chars, tag ≤
        // 64 chars) rejects it; only the whole-op guard, checking the ASSEMBLED op, does.
        let conn = scoped_conn();
        // Payload just under the 128 KiB cap, as a valid JSON object — so `validate_payload`'s
        // object / canonical checks pass and only its byte cap could fire (and it does not).
        let filler = "x".repeat(rag_rat_query::memory::MAX_MEMORY_PAYLOAD_LEN - 16);
        let payload = format!("{{\"v\":\"{filler}\"}}");
        assert!(
            payload.len() <= rag_rat_query::memory::MAX_MEMORY_PAYLOAD_LEN,
            "the payload is within its own byte cap",
        );
        // Body exactly at the char cap.
        let body = "b".repeat(rag_rat_query::memory::MAX_MEMORY_BODY_LEN);
        // A control create with the payload + body but NO tags SUCCEEDS — proving neither field,
        // nor the two together, is over the envelope on its own, so it is specifically the
        // tag aggregate (below) that trips the whole-op guard, not any single field.
        crate::memory_write::create_memory(&conn, rag_rat_query::memory::RepoMemoryCreate {
            kind: "Task".to_string(),
            title: "aggregate control".to_string(),
            body: body.clone(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: Some(payload.clone()),
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .expect("payload + body alone are within the envelope — no single cap is exceeded");
        // The SAME payload + body PLUS many individually-valid tags tips the assembled op over the
        // envelope — caught ONLY by the whole-op guard, not by any per-field cap.
        let tags: Vec<String> = (0..2500).map(|i| format!("tag-{i:060}")).collect();
        let err =
            crate::memory_write::create_memory(&conn, rag_rat_query::memory::RepoMemoryCreate {
                kind: "Task".to_string(),
                title: "aggregate over cap".to_string(),
                body,
                confidence: "high".to_string(),
                created_by: None,
                source: None,
                tags,
                payload_json: Some(payload),
                bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("too large"),
            "the aggregate op is rejected by the whole-op guard: {err}",
        );
        // Only the authorable control row persisted; the aggregate create rolled back.
        let rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "only the authorable control create persisted");
    }

    // --- live mutation seam self-heals a ghost end to end (#541 Task 4) ---

    #[test]
    fn mark_obsolete_on_a_ghost_authors_a_create_not_an_inert_status() {
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap(); // roots the chain
        insert_memory(&conn, "mem_ghost", "active", 500); // raw, un-authored ghost
        // mark_obsolete reconciles first (heals NodeCreate + NodeStatus{active}), THEN authors the
        // obsolete NodeStatus — so it is NOT inert and the node projects obsolete.
        crate::memory_write::mark_obsolete(&conn, "mem_ghost").unwrap();
        assert_eq!(
            projected_node_status(&conn, "mem_ghost"),
            NodeStatus::Obsolete.as_db_str(),
            "ghost healed then obsoleted",
        );
    }

    #[test]
    fn remove_edge_on_a_ghost_edge_heals_then_tombstones_not_an_inert_remove() {
        // The EdgeRemove path: `remove_edge` calls backfill (edges.rs) BEFORE its delete txn, so a
        // raw ghost edge is first healed (EdgeAdd authored), then the delete authors EdgeRemove —
        // the signed history is add→remove (complete), and the projection ends with the
        // edge ABSENT (not an inert tombstone with no matching add).
        //
        // `remove_edge` authors its `EdgeRemove` unconditionally once the raw row is deleted
        // (edges.rs gates it on `n > 0`, NOT on whether an `EdgeAdd` was ever signed) — so
        // `edges.is_empty()` alone is satisfied whether or not the heal ran (a
        // never-authored edge and a healed-then-removed edge both project empty). The
        // `entry_count` delta is what actually distinguishes them: it is +2 (heal's
        // `EdgeAdd` + `remove_edge`'s own `EdgeRemove`) only when the reconcile fired; a
        // disabled reconcile would author just the bare `EdgeRemove` (+1).
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        insert_raw_node_edge(&conn, &a, "relates_to", &b);
        let key = rag_rat_query::memory::edge_key(&a, "relates_to", "node", &b);
        let before = entry_count(&conn);
        crate::memory_write::remove_edge(&conn, &key).unwrap();
        assert_eq!(
            entry_count(&conn),
            before + 2,
            "the heal's EdgeAdd + remove_edge's own EdgeRemove — not a bare, inert tombstone"
        );
        assert_eq!(projected_edge_count(&conn), 0, "healed then tombstoned → edge absent");
    }

    #[test]
    fn a_failed_author_rolls_back_the_memory_write() {
        let conn = scoped_conn();
        // One good create so the account + owner stream are established and the second create's
        // backfill fast-paths, isolating the failure to the live author's reproject.
        create_concept(&conn, "first").unwrap();
        let before: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        // Poison the `/3` projector guard: pretend a NEWER binary already folded this store's `/3`
        // projection, so `reproject_accepted_content_stream`'s `assert_content_projector_not_newer`
        // errors and the second create's `/3` author fails (this doubles as the #664 `/3`
        // projector-stamp poison test). UPSERT: on this raw-connection store the stamp may be
        // absent — the per-stream reproject only MAINTAINS an already-current stamp (#688); the
        // open-path trigger (`rebuild_all_content_projections_if_stale`) is what writes it first.
        conn.execute(
            "INSERT INTO oplog_meta(key, value) VALUES ('content_projector_version', '999')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        assert!(create_concept(&conn, "second").is_err(), "the authoring failure fails the create");
        let after: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        assert_eq!(after, before, "strict-atomic: the failed create's row rolled back with it");
    }

    #[test]
    fn a_live_create_backfills_pre_existing_memories_first() {
        let conn = scoped_conn();
        // A memory inserted by RAW SQL (never authored) — the pre-existing history.
        insert_memory(&conn, "old", "active", 100);
        // The first LIVE create backfills `old` (a NodeCreate) BEFORE authoring the new memory.
        let new_id = create_concept(&conn, "new").unwrap().memory.memory_id;
        assert_eq!(
            projected_node_count(&conn),
            2,
            "old (backfilled) + new (live) are both projected"
        );
        let old_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_present, 1, "the pre-existing memory was backfilled");
        assert_ne!(new_id, "old");
    }

    /// Create an unanchored `Concept` with tags through the LIVE `create_memory`.
    fn create_concept_tagged(conn: &Connection, title: &str, tags: Vec<String>) -> String {
        crate::memory_write::create_memory(conn, rag_rat_query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags,
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap()
        .memory
        .memory_id
    }

    fn update_tags(conn: &Connection, id: &str, tags: Vec<String>) {
        crate::memory_write::update_memory(conn, rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: id.to_string(),
            kind: None,
            title: None,
            body: None,
            confidence: None,
            status: None,
            tags: Some(tags),
            payload_json: None,
        })
        .unwrap();
    }

    #[test]
    fn a_normalization_only_tag_change_authors_nothing() {
        let conn = scoped_conn();
        let id = create_concept_tagged(&conn, "t", vec!["x".to_string()]);
        let before = entry_count(&conn);
        // Tags that normalize to the SAME set: trailing space, duplicate, and an empty string.
        update_tags(&conn, &id, vec!["x ".to_string(), "x".to_string(), String::new()]);
        assert_eq!(
            entry_count(&conn),
            before,
            "a whitespace/duplicate-only re-tag is not a content change → no NodeUpdate"
        );
    }

    #[test]
    fn a_real_tag_change_authors_a_node_update() {
        let conn = scoped_conn();
        let id = create_concept_tagged(&conn, "t", vec!["x".to_string()]);
        let before = entry_count(&conn);
        update_tags(&conn, &id, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(entry_count(&conn), before + 1, "adding a real tag authors a NodeUpdate");
    }

    #[test]
    fn re_adding_an_edge_authors_no_duplicate() {
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let b = create_concept(&conn, "b").unwrap().memory.memory_id;
        let target = |node: &str| rag_rat_query::memory::EdgeTarget::Node {
            repo_id: None,
            node_id: node.to_string(),
        };
        crate::memory_write::add_edge(&conn, &a, EdgeRelation::RelatesTo, &target(&b)).unwrap();
        let after_first = entry_count(&conn);
        assert_eq!(projected_edge_count(&conn), 1);
        // Re-adding the SAME edge is an idempotent resolution-refresh — it must NOT author a second
        // EdgeAdd (which could resurrect a concurrent remove under sync).
        crate::memory_write::add_edge(&conn, &a, EdgeRelation::RelatesTo, &target(&b)).unwrap();
        assert_eq!(
            entry_count(&conn),
            after_first,
            "an idempotent edge re-add authors no duplicate EdgeAdd"
        );
        assert_eq!(projected_edge_count(&conn), 1);
    }

    // --- owner-bound /2//3 retarget (#664) ---

    #[test]
    fn a_fresh_repo_first_create_mints_the_account_publishes_ownership_and_projects_the_node() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "first").unwrap().memory.memory_id;
        // The first create on a fresh scoped repo mints exactly one local account and folds exactly
        // one `/2` StreamOwn effective — the ownership the owner-authored `/3` content accepts
        // under.
        assert_eq!(local_account_count(&conn), 1, "the first create mints one local account");
        assert_eq!(owned_stream_count(&conn), 1, "exactly one /2 StreamOwn folds effective");
        // And the memory is an accepted, projected `/3` node.
        assert_eq!(projected_node_count(&conn), 1, "the created memory is a projected /3 node");
        assert_eq!(projected_node_status(&conn, &id), "active", "a fresh create projects active");
    }

    #[test]
    fn a_second_repo_first_create_establishes_its_own_ownership_despite_the_shared_account() {
        // Multi-repo store, the distinguishing case for the fast-path probe: repo A's create mints
        // the STORE-GLOBAL account and establishes A's ownership. Repo B is then fresh with ZERO
        // memories. B's first create must STILL publish B's own `/2` StreamOwn — the account
        // already being minted is NOT enough. This is why the probe checks
        // `established_owned_stream_v2` (StreamOwn folded EFFECTIVE) and not merely
        // "account minted": with the weaker check, B's empty anti-join would early-return,
        // B would author `/3` content under an unowned stream, and verify-accepted would
        // roll the create back (Risk trap #1). A single-repo test cannot catch this — there
        // the account is unminted, so both checks agree.
        let conn = scoped_conn(); // repo-a: registered + scoped
        create_concept(&conn, "a1").unwrap();
        assert_eq!(local_account_count(&conn), 1, "repo-a's create mints the store account");
        assert_eq!(owned_stream_count(&conn), 1, "repo-a owns its /2 stream");

        // Register a SECOND repo in the same store and scope the connection to it.
        const REPO_B: &str = "repo-b";
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
            [REPO_B],
        )
        .unwrap();
        set_scope(&conn, REPO_B);

        // B's first create: the account is already minted, B has no StreamOwn and no memories.
        let id_b = create_concept(&conn, "b1").unwrap().memory.memory_id;
        assert_eq!(
            local_account_count(&conn),
            1,
            "the account is store-global — still exactly one"
        );
        assert_eq!(
            owned_stream_count(&conn),
            2,
            "repo-b established its OWN /2 StreamOwn rather than riding repo-a's",
        );
        assert_eq!(
            projected_node_status(&conn, &id_b),
            "active",
            "b1 accepted under repo-b's freshly-published ownership",
        );
    }

    #[test]
    fn pre_existing_memories_are_adopted_into_v3_and_the_v1_tables_are_left_untouched() {
        let conn = scoped_conn();
        // Pre-existing history a pre-#664 binary / raw writer left in the tables (never signed).
        insert_memory(&conn, "old_a", "active", 100);
        insert_memory(&conn, "old_b", "obsolete", 200);
        insert_memory(&conn, "old_c", "active", 300);
        // One live mutation triggers the reconcile: the three pre-existing rows are authored into
        // `/3` as the genesis batch, then the new memory is authored live.
        create_concept(&conn, "trigger").unwrap();
        for id in ["old_a", "old_b", "old_c"] {
            let projected: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(projected, 1, "pre-existing memory {id} was adopted into /3");
        }
        assert_eq!(projected_node_status(&conn, "old_b"), "obsolete", "the adopted status carried");
        // The retained `/1` tables are UNTOUCHED — the live path no longer writes them (issue J1).
        let v1_entries: i64 =
            conn.query_row("SELECT COUNT(*) FROM oplog_entries", [], |r| r.get(0)).unwrap();
        let v1_nodes: i64 =
            conn.query_row("SELECT COUNT(*) FROM oplog_projected_nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(v1_entries, 0, "the /1 entry log is not written by the retargeted live path");
        assert_eq!(v1_nodes, 0, "the /1 shadow projection is not written by the retarget");
    }

    #[test]
    fn racing_backfills_converge_on_one_stream_own_and_no_duplicate_content() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db");
        // Setup once: schema, the registered repo, and one pre-existing memory so there is content
        // for the reconcile to author (the racers must converge on authoring it exactly once).
        let setup = Connection::open(&path).unwrap();
        setup.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        rag_rat_db::schema::apply(&setup, &crate::index::migration_hooks()).unwrap();
        setup
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
                [REPO],
            )
            .unwrap();
        set_scope(&setup, REPO);
        insert_memory(&setup, "old", "active", 100);
        drop(setup);

        let barrier = Arc::new(Barrier::new(2));
        let spawn = || {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                set_scope(&conn, REPO);
                barrier.wait();
                backfill_memory_oplog(&conn, 9_000).unwrap();
            })
        };
        let a = spawn();
        let b = spawn();
        a.join().unwrap();
        b.join().unwrap();

        let conn = Connection::open(&path).unwrap();
        assert_eq!(local_account_count(&conn), 1, "the racers converge on one local account");
        assert_eq!(owned_stream_count(&conn), 1, "exactly one /2 StreamOwn survives the race");
        assert_eq!(entry_count(&conn), 1, "the pre-existing memory is authored exactly once");
        let old_nodes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_nodes, 1, "the adopted memory projects exactly once, no duplicate");
    }

    #[test]
    fn an_oversized_ghost_is_quarantined_and_does_not_wedge_the_write_path() {
        // #680: a raw/imported memory whose signed /3 envelope exceeds the §18a 256 KiB cap is
        // un-authorable. The reconcile must QUARANTINE it (skip it) rather than `bail!` — the old
        // fail-loud posture made EVERY subsequent mutation fail at its pre-write backfill, wedging
        // the whole memory-write path with no recovery.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap(); // roots the chain
        let oversized = "x".repeat(300 * 1024); // > 256 KiB ⇒ the signed envelope exceeds the cap
        insert_memory_with_body(&conn, "mem_big", &oversized); // raw, un-authorable ghost

        // The reconcile no longer errors — the poison ghost is quarantined, not fatal.
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert!(
            !is_projected(&conn, "mem_big"),
            "the oversized ghost is quarantined, never signed into the /3 log",
        );
        // The write path stays LIVE: a fresh mutation still succeeds instead of wedging.
        let live = create_concept(&conn, "still alive").unwrap().memory.memory_id;
        assert!(is_projected(&conn, &live), "other writes stay live despite the poison ghost");
    }

    #[test]
    fn an_oversized_ghost_can_be_recovered_via_the_public_api() {
        // #680: before the fix an oversized ghost wedged every write with no way out. Now the write
        // path stays live, so the ghost is RECOVERABLE through the public API — shrink its body
        // under the cap and the next reconcile signs it like any other healed ghost.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap();
        let oversized = "x".repeat(300 * 1024);
        insert_memory_with_body(&conn, "mem_big", &oversized);

        // Recover it: a plain `update_memory` shrinking the body under the cap — the mutation the
        // wedge would have blocked — now succeeds.
        crate::memory_write::update_memory(&conn, rag_rat_query::memory::RepoMemoryUpdate {
            memory_id: "mem_big".to_string(),
            kind: None,
            title: None,
            body: Some("shrunk".to_string()),
            confidence: None,
            status: None,
            tags: None,
            payload_json: None,
        })
        .unwrap();
        // The row is authorable now, so the next reconcile (any mutation) signs it.
        create_concept(&conn, "next").unwrap();
        assert_eq!(
            projected_node_status(&conn, "mem_big"),
            NodeStatus::Active.as_db_str(),
            "the shrunk row is signed on the next reconcile — fully recovered",
        );
    }

    #[test]
    fn an_oversized_ghost_edge_is_quarantined_and_does_not_wedge_the_write_path() {
        // #680: the node quarantine's EDGE twin. A raw/imported edge whose signed /3 `EdgeAdd`
        // exceeds the §18a 256 KiB cap (an oversized `target_anchor`) is un-authorable. The
        // reconcile must QUARANTINE it (skip it) rather than `bail!` — otherwise that ONE edge
        // makes EVERY subsequent mutation fail at its pre-write backfill, wedging the whole
        // memory-write path exactly as an oversized node would (the gap the node-only
        // quarantine left open).
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id; // live, authorable source node
        let oversized_anchor = "x".repeat(300 * 1024); // > 256 KiB ⇒ the EdgeAdd envelope exceeds cap
        insert_raw_node_edge(&conn, &a, "relates_to", &oversized_anchor); // raw un-authorable ghost

        // The reconcile no longer errors — the poison edge is quarantined, not fatal.
        backfill_memory_oplog(&conn, 1_000).unwrap();
        assert_eq!(
            projected_edge_count(&conn),
            0,
            "the oversized ghost edge is quarantined, never signed into the /3 log",
        );
        assert!(
            is_projected(&conn, &a),
            "the authorable source node is unaffected — only its oversized edge is quarantined",
        );
        // The write path stays LIVE: a fresh mutation still succeeds instead of wedging.
        let live = create_concept(&conn, "still alive").unwrap().memory.memory_id;
        assert!(is_projected(&conn, &live), "other writes stay live despite the poison edge");
    }

    #[test]
    fn an_oversized_ghost_edge_can_be_recovered_via_the_public_api() {
        // #680: because the write path stays live, the quarantined edge is RECOVERABLE — an edge
        // has no "shrink" (its anchor is its identity), so recovery is `remove_edge`
        // (memory_edge_remove): it deletes the un-authorable ghost by its short, hashed `edge_key`
        // (unaffected by the oversized anchor) and authors a tiny EdgeRemove, leaving the reconcile
        // with nothing un-authorable.
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id;
        let oversized_anchor = "x".repeat(300 * 1024);
        let key = rag_rat_query::memory::edge_key(&a, "relates_to", "node", &oversized_anchor);
        insert_raw_node_edge(&conn, &a, "relates_to", &oversized_anchor);

        // Remove it through the public API — the mutation the wedge would have blocked now
        // succeeds.
        assert!(
            crate::memory_write::remove_edge(&conn, &key).unwrap(),
            "the oversized ghost edge is removed through the public API",
        );
        let remaining: i64 =
            conn.query_row("SELECT COUNT(*) FROM repo_node_edges", [], |r| r.get(0)).unwrap();
        assert_eq!(remaining, 0, "the un-authorable edge row is gone after recovery");
        // The write path is clean: the next reconcile has nothing to quarantine.
        let live = create_concept(&conn, "next").unwrap().memory.memory_id;
        assert!(is_projected(&conn, &live), "the write path is clean after recovery");
    }

    /// A `MakeWriter` that appends every formatted log line into a shared buffer, so a test can
    /// assert on emitted `tracing` events (`rag_rat_base::logging` uses the same `with_writer`
    /// shape).
    #[derive(Clone)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn the_fast_path_warns_on_the_quarantined_row_it_skips() {
        // #680 (P2a): once ownership is established and the ONLY pending row is un-authorable, the
        // reconcile takes the lock-free fast path and early-returns without authoring. It must
        // still emit the per-row quarantine warning from that path — otherwise the
        // oversized row is silently skipped on every reconcile, defeating the
        // actionable-warning contract that replaced the old fail-loud wedge. Before the fix
        // the fast path returned with no warning.
        let conn = scoped_conn();
        create_concept(&conn, "seed").unwrap(); // establishes ownership → the fast path is reachable
        let oversized = "x".repeat(300 * 1024);
        insert_memory_with_body(&conn, "mem_big", &oversized); // the only pending row, un-authorable

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(CaptureWriter(std::sync::Arc::clone(&buf)))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            // Ownership established + nothing AUTHORABLE missing ⇒ the fast path handles this.
            backfill_memory_oplog(&conn, 2_000).unwrap();
        });

        let logged = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("quarantining an un-authorable memory row"),
            "the fast path emitted the per-row quarantine warning; got: {logged:?}",
        );
        assert!(
            logged.contains("mem_big"),
            "the warning names the skipped memory id; got: {logged:?}",
        );
        // The row stays unprojected — the warning is emitted IN PLACE OF authoring it, not
        // alongside.
        assert!(
            !is_projected(&conn, "mem_big"),
            "the quarantined row is still skipped, not signed"
        );
    }

    #[test]
    fn the_fast_path_warns_on_the_quarantined_edge_it_skips() {
        // #680 (P2a, edge twin): once ownership is established and the ONLY pending row is an
        // un-authorable EDGE, the reconcile takes the lock-free fast path and early-returns without
        // authoring. It must STILL emit the per-edge quarantine warning from that path — otherwise
        // the oversized edge is silently skipped on every reconcile, with no signal at all.
        let conn = scoped_conn();
        let a = create_concept(&conn, "a").unwrap().memory.memory_id; // establishes ownership
        let oversized_anchor = "x".repeat(300 * 1024);
        insert_raw_node_edge(&conn, &a, "relates_to", &oversized_anchor); // the only pending row

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(CaptureWriter(std::sync::Arc::clone(&buf)))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            // Ownership established + nothing AUTHORABLE missing ⇒ the fast path handles this.
            backfill_memory_oplog(&conn, 2_000).unwrap();
        });

        let logged = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("quarantining an un-authorable node-edge"),
            "the fast path emitted the per-edge quarantine warning; got: {logged:?}",
        );
        assert_eq!(
            projected_edge_count(&conn),
            0,
            "the quarantined edge is still skipped, not signed",
        );
    }

    // --- read path is unchanged by the retarget (#665) ---

    #[test]
    fn local_reads_come_from_repo_memories_not_the_v3_stream() {
        // The retarget moved AUTHORING onto /3, but local reads must be unchanged: they come from
        // repo_memories, never the /3 stream or its content_projected_* shadow (whose only consumer
        // is the reconcile's completeness anti-join). Prove it: author via the live path, then WIPE
        // the entire /3 substrate and confirm a read returns the same memories. A read that
        // consulted any /3 table would change here.
        let conn = scoped_conn();
        let created = create_concept(&conn, "readable").unwrap().memory.memory_id;

        let read_ids = |c: &Connection| -> Vec<String> {
            rag_rat_query::memory::list_memories(c, None)
                .unwrap()
                .into_iter()
                .map(|m| m.memory_id)
                .collect()
        };
        let before = read_ids(&conn);
        assert!(before.contains(&created), "the authored memory lists before the wipe");

        // Wipe every /3 table the live path writes (FKs off so delete order is irrelevant).
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             DELETE FROM content_projected_nodes;
             DELETE FROM content_projected_edges;
             DELETE FROM content_entry_status;
             DELETE FROM content_entries;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();

        assert_eq!(
            read_ids(&conn),
            before,
            "reads are identical with the whole /3 substrate wiped — they never consult it",
        );
    }
}
