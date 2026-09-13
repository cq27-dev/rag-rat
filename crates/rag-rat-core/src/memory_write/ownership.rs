//! Which account's stream a repo's memories author onto, persisted as repo meta: the one-way
//! access-mode and seal-policy intents, the contribution and subscription owners, the stream pin,
//! and the grantee context a granted contributor authors under.

use anyhow::Context;
use rag_rat_oplog::{SealPolicy, StreamId};
use rag_rat_query::memory::memory_repo_scope;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::authoring::AuthoredDurability;

/// The repo's memory-authoring ACCESS MODE — the one-way publish INTENT, persisted in repo meta.
/// This is the AUTHORING-side seed: the live-write sites are conn-only and the first memory write
/// mints the account + authors a `/2` StreamOwn, so the mode a `PublicRead` node must author under
/// cannot come from `Config` (unreachable at those sites) nor be derived from an empty op-log — it
/// is read from here. The op-log's StreamOwn set stays the SERVE-side truth
/// (`account_is_fully_public`).
pub(super) const STREAM_ACCESS_MODE_META_KEY: &str = "memory_stream_access_mode";

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

pub(super) const STREAM_SEAL_POLICY_META_KEY: &str = "memory_stream_seal_policy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StreamSealPolicy {
    Plaintext,
    Sealed,
}

impl StreamSealPolicy {
    pub(super) fn seal_policy(self) -> SealPolicy {
        match self {
            Self::Plaintext => SealPolicy::Plaintext,
            Self::Sealed => SealPolicy::Sealed,
        }
    }
}

/// The `repo_meta` key holding the account this repo contributes memories to (the paste-flow owner
/// id, set by `sync contribute`). Absent = this store authors its own owner stream.
pub(crate) const CONTRIBUTION_OWNER_META_KEY: &str = "memory_contribution_owner";

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

/// The configured contribution-owner account for `repo_id`, or `None`. Stored as a 64-hex account
/// id.
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

/// The pin an UNFINISHED `rag-rat consolidate` run wrote into this store, and the legacy source it
/// came from. It is how a retry of that same source tells its own stale copy — replaceable, because
/// the legacy index stays the live store until the rename lands — from a pin decided here, which it
/// must not override. A subscribe decides the pin here, so it clears this.
const STREAM_PIN_IMPORTED_META_KEY: &str = "memory_stream_pin_imported";

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

/// How to reach the owner being subscribed to, as a checked-in locator supplied it. Empty for an
/// operator-named subscribe, which therefore CLEARS whatever routing a previous subscription
/// recorded rather than dialing that owner's host for a different account.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SubscriptionRouting<'a> {
    pub(crate) peers: &'a [String],
    pub(crate) relay: Option<&'a str>,
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
pub(super) fn is_contribution_mode(conn: &Connection, repo_id: &str) -> anyhow::Result<bool> {
    let Some(owner) = contribution_owner_account(conn, repo_id)? else {
        return Ok(false);
    };
    Ok(rag_rat_oplog::read_local_account(conn)? != Some(owner))
}

/// Refuse `operation` when `repo_id`'s memories materialize from ANOTHER account's stream — either
/// configuration reaches that state. Every operation that reconciles or imports rows into the repo
/// (legacy consolidation, `sync publish --seed`, memory import) needs the repo's own stream to be
/// the one [`super::drain::authoritative_content_stream`] honors: a contributor owns no such stream
/// at all, and a subscriber's is not the authority, so what the import signs there never becomes
/// the repo's memory state on any other device. All of them are irreversible enough that
/// continuing on a half-applied import is worse than stopping.
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

pub(super) struct GranteeContext {
    pub(super) owner_account: rag_rat_oplog::AccountId,
    pub(super) stream: StreamId,
    pub(super) grant_id: [u8; 32],
}

/// Resolve grantee-authoring context for `repo_id`: the configured contribution owner, its
/// `PublicRead` owner stream, and this store's effective Writer grant on it. `None` when not in
/// contribution mode. Errors (fail loud) when configured but the grant is missing — the operator
/// must `sync grant` this account from the owner and sync the owner's log first.
pub(super) fn grantee_context(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<GranteeContext>> {
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
/// [`enable_public_authoring`](super::grants::enable_public_authoring) also moves the repo's
/// authoritative stream (Private ⇒ PublicRead) without coming through here. That path is
/// watermark-safe on its own: it refuses unless the account is fully public, so the outgoing
/// Private stream is one nothing was ever authored or ingested onto and it holds no synced rows to
/// condemn.
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
    routing: SubscriptionRouting<'_>,
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

    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    anyhow::ensure!(
        memory_repo_scope(&tx)?.as_deref() == Some(repo_id.as_str()),
        "active repo scope changed while starting sync subscribe; retry"
    );
    ensure_no_contribution_configured(&tx, &repo_id)?;
    // Decided UNDER the write lock. A check made before `BEGIN IMMEDIATE` can pass against a pin
    // another connection replaces before this transaction starts, and this write would then restore
    // the owner that pin had just moved away from.
    //
    // A locator may establish this repo's trust root, never move it. Moving it is how an edited
    // checked-in file would silently re-point a subscriber onto another account's stream — and a
    // re-point is destructive, not merely redirecting: the drain removes the previous owner's
    // mirrored rows, and the local binding work on them (a `memory rebind`, any local edge) does
    // not come back on unsubscribe. The remedy names a human step rather than a command, because
    // the point of the override is that the operator obtains the id from the owner, not from the
    // file that just changed.
    if trust == SubscribeTrust::Locator
        && let Some(pinned) = effective_stream_pin(&tx, &repo_id)?
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
    repoint_authoritative_content_stream(&tx, &repo_id, StreamResolution::Strict, |tx| {
        rag_rat_db::meta::set_repo_meta(tx, &repo_id, SUBSCRIPTION_OWNER_META_KEY, &canonical)
            .map_err(Into::into)
    })?;
    // Owner, pin and routing commit together: this function is the single writer of all four
    // subscription fields. A torn write could otherwise leave a repo subscribed to an owner it
    // never pinned, or holding the previous owner's routes — which `subscription_routing` would
    // then attribute to the new account and dial on its behalf.
    rag_rat_db::meta::set_repo_meta(&tx, &repo_id, STREAM_PIN_META_KEY, &canonical)?;
    write_subscription_routing(&tx, &repo_id, &routing)?;
    // The pin is now a decision made in THIS store, no longer a copy an earlier consolidation left:
    // a retried consolidation may replace an imported pin, never this one.
    rag_rat_db::meta::delete_repo_meta(&tx, &repo_id, STREAM_PIN_IMPORTED_META_KEY)?;
    tx.commit()?;
    Ok(())
}

/// The owner this repo has pinned, if any. Survives `sync unsubscribe`.
pub(crate) fn stream_pin(conn: &Connection, repo_id: &str) -> anyhow::Result<Option<String>> {
    effective_stream_pin(conn, repo_id)
}

/// The recorded pin, or — when none was ever recorded — the owner this repo is already subscribed
/// to. A store upgraded from a release without the pin holds a live subscription and no pin, and
/// reading that as "never trusted anyone" would let a changed locator silently replace an owner an
/// operator chose. An existing subscription IS an existing trust decision.
fn effective_stream_pin(conn: &Connection, repo_id: &str) -> anyhow::Result<Option<String>> {
    Ok(match rag_rat_db::meta::repo_meta(conn, repo_id, STREAM_PIN_META_KEY)? {
        Some(pinned) => Some(pinned),
        None => rag_rat_db::meta::repo_meta(conn, repo_id, SUBSCRIPTION_OWNER_META_KEY)?,
    })
}

/// Record how to reach the subscribed owner's host, or clear it when the routing is empty. Private
/// and called only inside `set_subscription_owner`'s transaction, so there is no path that updates
/// routing apart from the owner it belongs to.
fn write_subscription_routing(
    conn: &Connection,
    repo_id: &str,
    routing: &SubscriptionRouting<'_>,
) -> anyhow::Result<()> {
    let joined = routing.peers.join("\n");
    if joined.is_empty() {
        rag_rat_db::meta::delete_repo_meta(conn, repo_id, SUBSCRIPTION_PEERS_META_KEY)?;
    } else {
        rag_rat_db::meta::set_repo_meta(conn, repo_id, SUBSCRIPTION_PEERS_META_KEY, &joined)?;
    }
    match routing.relay {
        Some(relay) if !relay.trim().is_empty() =>
            rag_rat_db::meta::set_repo_meta(conn, repo_id, SUBSCRIPTION_RELAY_META_KEY, relay)?,
        _ => rag_rat_db::meta::delete_repo_meta(conn, repo_id, SUBSCRIPTION_RELAY_META_KEY)?,
    };
    Ok(())
}

/// Every peer recorded by a subscription to `owner_hex`, each paired with the relay its locator
/// named.
///
/// Attributed to the owner because a locator describes how to reach ONE owner's host. Pooled across
/// owners, one repository's locator could name another owner's peer through a dead relay and put
/// that route in front of a live one; scoped, a pull for an account only ever sees routes toward
/// that account. Pooled across the repositories subscribing to the same owner, since the pull pass
/// pulls each account once, not once per repository.
///
/// Only live subscriptions contribute. Once a repository mirrors nobody, its host drops out of
/// every pull — an obsolete host stalls each pass behind a failing dial.
///
/// ONE statement, so ownership, peers and relay come from one snapshot. Separate reads can straddle
/// a concurrent re-subscribe — find this owner, then read the NEXT owner's peers — and hand one
/// account the routes recorded for another. The write side commits all four fields together; that
/// only protects a reader that also reads them together.
pub(crate) fn subscription_routing(
    conn: &Connection,
    owner_hex: &str,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT peers.value, relay.value
         FROM repos
         JOIN repo_meta AS owner ON owner.repo_id = repos.repo_id AND owner.key = ?2
         JOIN repo_meta AS peers ON peers.repo_id = repos.repo_id AND peers.key = ?3
         LEFT JOIN repo_meta AS relay ON relay.repo_id = repos.repo_id AND relay.key = ?4
         WHERE owner.value = ?1 AND repos.repo_id != ?5
         ORDER BY repos.repo_id",
    )?;
    let rows = stmt.query_map(
        params![
            owner_hex,
            SUBSCRIPTION_OWNER_META_KEY,
            SUBSCRIPTION_PEERS_META_KEY,
            SUBSCRIPTION_RELAY_META_KEY,
            rag_rat_base::repo_identity::LEGACY_REPO_ID,
        ],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
    )?;
    let mut routes = Vec::new();
    for row in rows {
        let (Some(recorded), relay) = row? else { continue };
        routes.extend(
            recorded
                .lines()
                .map(str::trim)
                .filter(|peer| !peer.is_empty())
                .map(|peer| (peer.to_string(), relay.clone())),
        );
    }
    Ok(routes)
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
fn clear_foreign_owner(
    conn: &Connection,
    meta_key: &str,
    command: &str,
    before_clear: impl FnOnce(&Connection, &str) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
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
    before_clear(&tx, &repo_id)?;
    repoint_authoritative_content_stream(&tx, &repo_id, StreamResolution::BestEffort, |tx| {
        rag_rat_db::meta::delete_repo_meta(tx, &repo_id, meta_key).map_err(Into::into)
    })?;
    tx.commit()?;
    Ok(true)
}

/// Stop mirroring a subscribed owner (`sync unsubscribe`).
pub(crate) fn clear_subscription_owner(conn: &Connection) -> anyhow::Result<bool> {
    clear_foreign_owner(conn, SUBSCRIPTION_OWNER_META_KEY, "sync unsubscribe", |tx, repo_id| {
        // Only the trust root outlives the subscription. If the pin was never recorded — a store
        // upgraded from before it existed, unsubscribing first — record the owner being cleared
        // now, or the next locator subscribe would read as first use and pin whatever it names.
        if rag_rat_db::meta::repo_meta(tx, repo_id, STREAM_PIN_META_KEY)?.is_none()
            && let Some(owner) =
                rag_rat_db::meta::repo_meta(tx, repo_id, SUBSCRIPTION_OWNER_META_KEY)?
        {
            rag_rat_db::meta::set_repo_meta(tx, repo_id, STREAM_PIN_META_KEY, &owner)?;
        }
        Ok(())
    })
}

/// Stop contributing to a configured owner (`sync uncontribute`). The owner's Writer grant is
/// untouched — only this store's routing changes — and the contributions already authored onto the
/// owner's stream stay there; this store keeps its own `origin='local'` copies of them.
pub(crate) fn clear_contribution_owner(conn: &Connection) -> anyhow::Result<bool> {
    clear_foreign_owner(conn, CONTRIBUTION_OWNER_META_KEY, "sync uncontribute", |_, _| Ok(()))
}

pub(super) fn explicit_stream_seal_policy(
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

pub(super) fn stream_seal_policy(
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
