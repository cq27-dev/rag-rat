//! Sharing controls for a repo's owner stream: the one-way sealed and public enables,
//! enrolled-device key catch-up, and writer grants.

use anyhow::Context;
use rag_rat_oplog::{EdgeKey, MemoryOp};
use rag_rat_query::memory::memory_repo_scope;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::authoring::{AuthoredDurability, prepare_owner_authoring};
use super::ownership::{
    STREAM_ACCESS_MODE_META_KEY, STREAM_SEAL_POLICY_META_KEY, StreamSealPolicy,
    explicit_stream_seal_policy, owner_stream_access_mode, stream_seal_policy,
};
use super::reconcile::{
    build_reconcile_ops, ensure_owner_stream, read_reconcile_work, settle_owner_stream_in_tx,
};

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
    let sealed = StreamSealPolicy::Sealed
        .as_db_str()
        .context("the sealed policy always has a persisted token")?;
    rag_rat_db::meta::set_repo_meta(&tx, &repo_id, STREAM_SEAL_POLICY_META_KEY, sealed)?;
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
    let public = super::ownership::access_mode_db_str(rag_rat_oplog::AccessMode::PublicRead)
        .context("the public access mode always has a persisted token")?;
    rag_rat_db::meta::set_repo_meta(&tx, &repo_id, STREAM_ACCESS_MODE_META_KEY, public)?;
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

/// Remove an enrolled device from the local account (owner-only) — the recovery for a device whose
/// chain forked (#1417), or one that was lost. Account-scoped, not repo-scoped: the roster spans
/// every repo. Stream keys the removed device held rotate at the next seal. Returns the device.
pub(crate) fn remove_account_device(
    conn: &Connection,
    device: &str,
    reason: &str,
    now_ms: i64,
) -> anyhow::Result<rag_rat_oplog::RosterDevice> {
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let subject = rag_rat_oplog::resolve_roster_device(&tx, device)?;
    rag_rat_oplog::author_device_remove_in_tx(&tx, subject.fingerprint, reason, now_ms)?;
    tx.commit()?;
    Ok(subject)
}

/// Grant an enrolled member device owner authority on the local account (owner-only), so a sole
/// owner can hand removal authority to another device before its own store is retired.
pub(crate) fn promote_account_device(
    conn: &Connection,
    device: &str,
    now_ms: i64,
) -> anyhow::Result<rag_rat_oplog::RosterDevice> {
    let _durability = AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let subject = rag_rat_oplog::resolve_roster_device(&tx, device)?;
    rag_rat_oplog::author_owner_promote_in_tx(&tx, subject.fingerprint, now_ms)?;
    let promoted = rag_rat_oplog::resolve_roster_device(&tx, &subject.fingerprint.to_string())?;
    tx.commit()?;
    Ok(promoted)
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
    Ok(grant_id.into())
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
            grant_id: rag_rat_base::hash::hex_lower(grant.grant_id.as_slice()),
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
            .map(|id| rag_rat_base::hash::hex_lower(id.as_slice()))
            .collect(),
        revoke_ids: revocation
            .revoke_ids
            .iter()
            .map(|id| rag_rat_base::hash::hex_lower(id.as_slice()))
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
