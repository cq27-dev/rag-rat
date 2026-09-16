//! §16 account candidate-DAG storage: ingest, content-addressed device resolution, the pre-verify
//! queue, and `refold_account` — the pure [`super::fold::fold_account`] plus branch selection
//! (§16.2) projected onto the `accepted` flag + the §16.3 status taxonomy.
//!
//! The candidate table (`account_entries`) is grow-only and holds EVERY
//! structurally/signature-valid entry, all branches of an equivocating chain first-class (no
//! seq-uniqueness). `accepted` is DERIVED — rewritten atomically by every refold, gated by the
//! `account_accepted_slot` partial unique index (I10a) — never authored. Nothing here touches
//! [`super::super::store::append`]; the account layer is a separate signed wire layer with its own
//! DAG.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::envelope::{self, AccountEntryHeader, VerifiedAccountEntry};
use super::fold::{self, AuthorityChain, EntryStatus};
use super::id::{self, AccountEntryHash, GrantId, OwnerId, RosterRef, SignedHash};
use super::ops::{self, AccountOp, DecodedAccountOp, DeviceCut, DeviceRole, GrantRole};
use super::pre_verify::{BudgetOutcome, PreVerifyQueue, QueueBudget};
use super::{AccountId, annex, content, secrets};
use crate::cbor;
use crate::device::{DevicePublic, DeviceX25519Public};
use crate::op::DeviceFingerprint;
use crate::stream::{AccessMode, StreamId};

type BranchKey = (u8, DeviceFingerprint, Option<AccountEntryHash>);
type BranchChild = (u64, AccountEntryHash);
type BranchChildren = HashMap<BranchKey, Vec<BranchChild>>;

// Operational admission limits, not wire-validity limits. At the §18a envelope maximum these cap
// unauthenticated parked bytes at 4 MiB/account and 16 MiB globally. Candidate admission has both
// count and aggregate-byte limits: refolding materializes every candidate, so a count-only ceiling
// would still permit hundreds of MiB in one writer transaction. Admission rejects rather than
// evicts: deleting grow-only history would break replica convergence.
pub(super) const PRE_VERIFY_PER_ACCOUNT_MAX: usize = 64;
const PRE_VERIFY_GLOBAL_MAX: usize = 256;
const PRE_VERIFY: PreVerifyQueue =
    PreVerifyQueue { table: "account_pre_verify", owner_column: "claimed_account_id" };
pub(super) const CANDIDATES_PER_ACCOUNT_MAX: usize = 4_096;
const CANDIDATES_GLOBAL_MAX: usize = 16_384;
pub(super) const CANDIDATE_BYTES_PER_ACCOUNT_MAX: usize = 16 * 1024 * 1024;
const CANDIDATE_BYTES_GLOBAL_MAX: usize = 64 * 1024 * 1024;
/// The slice of the per-account budget reachable ONLY by a control-v2 view manifest.
///
/// A cut's evidence is a DETACHED manifest that competes for the same grow-only budget as ordinary
/// traffic, and the cut can land first. Without a floor an insider can fill the budget and leave
/// every honest manifest permanently unadmitted — which parks the cuts naming them forever, so the
/// devices those cuts revoke stay un-revoked. Capacity never drains, so that state is terminal.
///
/// A manifest naming the maximum `control_v2::views::MAX_VIEW_ENTRIES` identities signs to roughly
/// 64 KiB, so the byte floor holds sixteen of the largest an account can produce and hundreds of
/// ordinary ones. The floor is per-ACCOUNT only: a globally exhausted store is an operator-level
/// condition no per-account arithmetic can rescue.
const VIEW_MANIFEST_FLOOR_ENTRIES: usize = 64;
const VIEW_MANIFEST_FLOOR_BYTES: usize = 1024 * 1024;
/// The per-account caps an ORDINARY candidate may reach — everything above the manifest floor.
pub(super) const ORDINARY_CANDIDATES_PER_ACCOUNT_MAX: usize =
    CANDIDATES_PER_ACCOUNT_MAX - VIEW_MANIFEST_FLOOR_ENTRIES;
pub(super) const ORDINARY_CANDIDATE_BYTES_PER_ACCOUNT_MAX: usize =
    CANDIDATE_BYTES_PER_ACCOUNT_MAX - VIEW_MANIFEST_FLOOR_BYTES;

pub(super) struct AccountProjection {
    pub(super) history: fold::AccountAuthHistory,
    pub(super) accepted: HashSet<AccountEntryHash>,
    pub(super) forked: HashSet<AccountEntryHash>,
}

#[derive(Default)]
struct AccountStateFold {
    statuses: HashMap<AccountEntryHash, EntryStatus>,
    affected_streams: Vec<StreamId>,
    rejected_content_promotions: content::ContentPromotionOutcome,
}

/// The outcome of an `INSERT OR IGNORE` into the candidate DAG. `pub(super)` so the account
/// [`super::bootstrap`] seam can reuse [`insert_candidate`] directly when minting the local-account
/// genesis (it MUST NOT go through the self-transacting [`account_ingest`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CandidateInsert {
    Inserted,
    AlreadyPresent,
    AtCapacity(CapacityScope),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PreVerifyInsert {
    Parked { evicted: Vec<CapacityScope> },
    AtCapacity(CapacityScope),
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct PromotionOutcome {
    pub scope: Option<CapacityScope>,
    pub entry_hashes: Vec<AccountEntryHash>,
}

/// The operational admission budget that prevented an otherwise valid ingest from being stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityScope {
    PreVerifyAccount,
    PreVerifyGlobal,
    CandidateAccount,
    CandidateGlobal,
    CandidateAccountBytes,
    CandidateGlobalBytes,
}

/// The result of ingesting one signed account entry (§16.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Structurally rejected (bad canonicity / over §18a / bad signature / fingerprint or self-hash
    /// mismatch) — NEVER stored.
    Rejected(String),
    /// The signing device is not yet resolvable (`sha256(pk)` matches no known fingerprint) — held
    /// durably in `account_pre_verify`, retried when a later DeviceAdd/AccountGenesis arrives.
    PreVerify,
    /// Parked successfully, but admitting it displaced older parked work at these queue budgets.
    PreVerifyWithEviction { scopes: Vec<CapacityScope> },
    /// Structurally valid input could not be retained within an operational admission budget.
    CapacityReached { scope: CapacityScope },
    /// Stored as a candidate; `status` is its post-refold §16.3 taxonomy label — an
    /// [`EntryStatus`] token when this refold classified it, the stored token verbatim on a
    /// redelivery (the column is unconstrained TEXT, so a token this build does not know still
    /// reports).
    Ingested {
        status: String,
        /// Parked account entries rejected at candidate capacity; retry after capacity increases.
        account_promotions: PromotionOutcome,
        /// Parked content entries rejected at candidate capacity; retry after capacity increases.
        content_promotions: content::ContentPromotionOutcome,
    },
}

/// Ingest one signed account entry: structural decode → content-addressed device resolution →
/// signature verify → genesis self-hash → `INSERT OR IGNORE` into the candidate DAG → promote any
/// pre-verify rows this entry now resolves → fold account state and queue affected content streams.
/// Opens its own IMMEDIATE transaction; content finalization is deferred to settle.
pub fn account_ingest(
    conn: &Connection,
    signed_bytes: &[u8],
    now_ms: i64,
) -> anyhow::Result<IngestOutcome> {
    // Structure + §18a size + canonicity (never stored on failure). No DB touch yet.
    let signed = match envelope::decode_account_signed(signed_bytes) {
        Ok(signed) => signed,
        Err(err) => return Ok(IngestOutcome::Rejected(err.to_string())),
    };
    let account_id = signed.header.account_id;
    let device_fp = signed.header.device_fingerprint;
    if let Err(err) = validate_storable_header_payload(&signed.header, &signed.payload) {
        return Ok(IngestOutcome::Rejected(err));
    }

    // An exact byte-for-byte replay was already signature-checked on first insert. Return its
    // durable projection without taking the writer lock, rediscovering keys, or refolding up to the
    // entire account. A different envelope for the same entry hash still follows full verification.
    if let Some(status) = stored_status_for_exact_envelope(conn, &signed.entry_hash, signed_bytes)?
    {
        return Ok(IngestOutcome::Ingested {
            status,
            account_promotions: PromotionOutcome::default(),
            content_promotions: content::ContentPromotionOutcome::default(),
        });
    }

    // Known-key signatures can be rejected before taking SQLite's process-wide writer lock. Stored
    // candidates are grow-only, so a key resolved by this optimistic read cannot disappear. The
    // unresolved path is re-read under IMMEDIATE below to preserve the park/promotion lost-wakeup
    // invariant.
    let mut optimistic_pubkeys = stored_device_pubkeys(conn, account_id)?;
    add_self_pubkey(&mut optimistic_pubkeys, &signed.header, &signed.payload);
    let optimistic_verified = optimistic_pubkeys
        .get(&device_fp)
        .copied()
        .map(|pubkey_bytes| authenticate_entry(signed_bytes, &pubkey_bytes))
        .transpose();
    let optimistic_verified = match optimistic_verified {
        Ok(verified) => verified,
        Err(err) => return Ok(IngestOutcome::Rejected(err)),
    };

    // One IMMEDIATE transaction spans the race-sensitive resolution → park-or-store decision,
    // promotion, and refold. Count/byte checks and insertion are therefore one serialized unit.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let outcome =
        account_ingest_decoded_in_tx(&tx, signed, signed_bytes, now_ms, optimistic_verified)?;
    tx.commit()?;
    Ok(outcome)
}

/// Ingest one signed account entry inside the caller's transaction. Enrollment uses this to make
/// its complete bootstrap and local-account pointer one atomic state transition.
pub(super) fn account_ingest_in_tx(
    tx: &Transaction<'_>,
    signed_bytes: &[u8],
    now_ms: i64,
) -> anyhow::Result<IngestOutcome> {
    let signed = match envelope::decode_account_signed(signed_bytes) {
        Ok(signed) => signed,
        Err(err) => return Ok(IngestOutcome::Rejected(err.to_string())),
    };
    if let Err(err) = validate_storable_header_payload(&signed.header, &signed.payload) {
        return Ok(IngestOutcome::Rejected(err));
    }
    account_ingest_decoded_in_tx(tx, signed, signed_bytes, now_ms, None)
}

/// Authenticate and stage one enrollment receipt entry without refolding projections or retrying
/// pre-existing parked work. The caller stages the complete receipt before one final
/// reconciliation, so adoption remains linear in the account-history size and receipt rows win
/// candidate admission.
pub(super) fn stage_enrollment_bootstrap_entry_in_tx(
    tx: &Transaction<'_>,
    signed_bytes: &[u8],
    resolved_signer: Option<[u8; 32]>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let signed = envelope::decode_account_signed(signed_bytes)
        .map_err(|err| anyhow::anyhow!("enrollment bootstrap entry is malformed: {err}"))?;
    validate_storable_header_payload(&signed.header, &signed.payload)
        .map_err(|err| anyhow::anyhow!("enrollment bootstrap entry is invalid: {err}"))?;

    let device_fp = signed.header.device_fingerprint;
    let self_certified_signer = || {
        let mut pubkeys = HashMap::new();
        add_self_pubkey(&mut pubkeys, &signed.header, &signed.payload);
        pubkeys.get(&device_fp).copied()
    };
    let pubkey_bytes = resolved_signer.or_else(self_certified_signer).ok_or_else(|| {
        anyhow::anyhow!("enrollment bootstrap entry has no candidate-certified signer")
    })?;
    let verified = authenticate_entry(signed_bytes, &pubkey_bytes).map_err(|err| {
        anyhow::anyhow!("enrollment bootstrap entry failed authentication: {err}")
    })?;
    match insert_candidate(tx, &verified, signed_bytes, now_ms)? {
        CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => Ok(()),
        CandidateInsert::AtCapacity(scope) => {
            anyhow::bail!("enrollment bootstrap entry reached candidate capacity at {scope:?}")
        },
    }
}

/// Project the staged enrollment receipt in the caller's transaction — receipt candidates and
/// already-authenticated local candidates ONLY. Pre-existing parked rows are deliberately NOT
/// retried here: a newly resolvable parked sibling (say, a competing `DeviceAdd` for the joining
/// fingerprint, signed by a key the receipt certifies) would enter the same one-time fold and
/// could make the acknowledged `DeviceAdd` lose deterministic branch selection, rolling the
/// adoption back after the owner already consumed the nonce — and every exact replay would fail
/// identically. The parked queues are retried in a separate transaction after adoption commits.
pub(super) fn finish_enrollment_bootstrap_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<()> {
    let _state = refold_untrusted_ingest_in_tx(tx, account_id, now_ms, PreVerifyPromotion::Skip)?;
    Ok(())
}

fn account_ingest_decoded_in_tx(
    tx: &Transaction<'_>,
    signed: envelope::SignedAccountEntry,
    signed_bytes: &[u8],
    now_ms: i64,
    optimistic_verified: Option<VerifiedAccountEntry>,
) -> anyhow::Result<IngestOutcome> {
    let account_id = signed.header.account_id;
    let device_fp = signed.header.device_fingerprint;
    let verified = if let Some(verified) = optimistic_verified {
        verified
    } else {
        let mut pubkeys = stored_device_pubkeys(tx, account_id)?;
        add_self_pubkey(&mut pubkeys, &signed.header, &signed.payload);
        let Some(pubkey_bytes) = pubkeys.get(&device_fp).copied() else {
            let parked = insert_pre_verify(
                tx,
                &signed.entry_hash,
                account_id,
                device_fp,
                signed_bytes,
                now_ms,
            )?;
            return Ok(match parked {
                PreVerifyInsert::Parked { evicted } if evicted.is_empty() =>
                    IngestOutcome::PreVerify,
                PreVerifyInsert::Parked { evicted } =>
                    IngestOutcome::PreVerifyWithEviction { scopes: evicted },
                PreVerifyInsert::AtCapacity(scope) => IngestOutcome::CapacityReached { scope },
            });
        };
        match authenticate_entry(signed_bytes, &pubkey_bytes) {
            Ok(verified) => verified,
            Err(err) => return Ok(IngestOutcome::Rejected(err)),
        }
    };

    match insert_candidate(tx, &verified, signed_bytes, now_ms)? {
        CandidateInsert::AtCapacity(scope) => {
            return Ok(IngestOutcome::CapacityReached { scope });
        },
        CandidateInsert::AlreadyPresent => {
            if let Some((status, _)) = entry_status(tx, &verified.entry_hash)? {
                return Ok(IngestOutcome::Ingested {
                    status,
                    account_promotions: PromotionOutcome::default(),
                    content_promotions: content::ContentPromotionOutcome::default(),
                });
            }
        },
        CandidateInsert::Inserted => {},
    }
    // A DeviceAdd/genesis may resolve devices that were parked — retry their pre-verify rows.
    let introduces_device = is_genesis(&verified.header) || is_device_add(&verified);
    let rejected_promotions = if introduces_device {
        promote_pre_verify(tx, account_id, now_ms)?
    } else {
        PromotionOutcome::default()
    };
    let promotion =
        if introduces_device { PreVerifyPromotion::Retry } else { PreVerifyPromotion::Skip };
    let state = refold_untrusted_ingest_in_tx(tx, account_id, now_ms, promotion)?;
    let status = state
        .statuses
        .get(&verified.entry_hash)
        .map_or_else(|| "unknown".to_string(), |status| status.as_db_str().to_string());
    Ok(IngestOutcome::Ingested {
        status,
        account_promotions: rejected_promotions,
        content_promotions: state.rejected_content_promotions,
    })
}

pub(super) fn authenticate_entry(
    signed_bytes: &[u8],
    pubkey_bytes: &[u8; 32],
) -> Result<VerifiedAccountEntry, String> {
    let pubkey = DevicePublic::from_bytes(pubkey_bytes)
        .map_err(|_| "resolved device key is not a valid point".to_string())?;
    let verified =
        envelope::verify_account_signed(signed_bytes, &pubkey).map_err(|err| err.to_string())?;
    validate_authenticated_entry(&verified)?;
    Ok(verified)
}

fn stored_status_for_exact_envelope(
    conn: &Connection,
    entry_hash: &AccountEntryHash,
    signed_bytes: &[u8],
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT s.status
         FROM account_entries e
         JOIN account_entry_status s ON s.entry_hash = e.entry_hash
         WHERE e.entry_hash = ?1 AND e.signed_bytes = ?2",
        params![entry_hash.as_slice(), signed_bytes],
        |row| row.get(0),
    )
    .optional()
}

/// Re-derive the whole account: fold the candidate set, resolve accepted-slot uniqueness (branch
/// selection §16.2), and rewrite `accepted` + `account_entry_status` in one IMMEDIATE transaction
/// so the `account_accepted_slot` partial unique index (I10a) never transiently double-accepts a
/// slot.
pub fn refold_account(conn: &Connection, account_id: AccountId) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let now_ms = tx.query_row(
        "SELECT coalesce(max(received_at_ms), 0) FROM account_entries WHERE account_id = ?1",
        [account_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    refold_in_tx(&tx, account_id, now_ms)?;
    tx.commit()?;
    Ok(())
}

/// Rebuild account-derived projections from every account in the candidate DAG. V064/V065 use this
/// for authority tables and V099 uses it for repository incarnations learned from formerly opaque
/// secrets entries. The migration owns `tx`, so the source scan, projection, and ledger stamp
/// commit as one writer-locked unit.
pub fn backfill_authority_projection(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    let account_ids = {
        let mut stmt =
            tx.prepare("SELECT DISTINCT account_id FROM account_entries ORDER BY account_id")?;
        stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for account_bytes in account_ids {
        let result = id::fixed(&account_bytes)
            .map(AccountId::from_bytes)
            .and_then(|account_id| refold_in_tx(tx, account_id, 0));
        if let Err(err) = result {
            return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                format!("account projection backfill failed: {err:#}"),
            ))));
        }
    }
    Ok(())
}

/// Exact roster citation lookup over the V064 shadow projection. This is the hot-path seam for
/// `/3` ingest: a keyed read, never a candidate-DAG replay. It resolves against the current fold —
/// the only authority snapshot there is (§7) — and says nothing about the citing author's own
/// control length; that is [`auth_len_freshness`]'s separate job.
pub fn roster_ref_effective(
    conn: &Connection,
    account_id: AccountId,
    roster_ref: RosterRef,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::RosterAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    roster_ref_effective_in_snapshot(conn, account_id, roster_ref, device_fingerprint)
}

fn roster_ref_effective_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    roster_ref: RosterRef,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::RosterAuthority>> {
    let Some((authority, effective_at, closed_at)) =
        load_roster_fact(conn, account_id, &roster_ref)?
    else {
        return missing_reference(conn, account_id, &roster_ref.into());
    };
    if authority.device_fingerprint != device_fingerprint {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    validated_open_fact(authority, effective_at, closed_at)
}

/// The roster fact `roster_ref` minted, as its authority plus the raw `(effective_at, closed_at)`
/// window, or `None` when this store holds no such fact.
fn load_roster_fact(
    conn: &Connection,
    account_id: AccountId,
    roster_ref: &RosterRef,
) -> anyhow::Result<Option<(fold::RosterAuthority, i64, Option<i64>)>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let row: Option<(Vec<u8>, String, i64, Option<i64>)> = conn
        .query_row(
            "SELECT device_fingerprint, role, effective_at, closed_at
             FROM account_roster_history WHERE account_id = ?1 AND roster_ref = ?2",
            params![account_id.to_bytes().as_slice(), roster_ref.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((device, role, effective_at, closed_at)) = row else {
        return Ok(None);
    };
    let authority = fold::RosterAuthority {
        device_fingerprint: DeviceFingerprint::from_bytes(id::fixed(&device)?),
        current_role: DeviceRole::from_db_str(&role)?,
    };
    Ok(Some((authority, effective_at, closed_at)))
}

pub fn roster_content_authority(
    conn: &Connection,
    account_id: AccountId,
    roster_ref: RosterRef,
    device_fingerprint: DeviceFingerprint,
    stream_id: StreamId,
) -> anyhow::Result<fold::AuthorityQuery<fold::RosterContentAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    roster_content_authority_in_snapshot(
        conn,
        account_id,
        roster_ref,
        device_fingerprint,
        stream_id,
    )
}

/// The body of [`roster_content_authority`], reading whatever snapshot `conn` is already in.
///
/// The `/3` refold resolves EVERY authority fact for an entry — ownership, roster, grant, both
/// freshness verdicts — and must see one consistent snapshot across all of them, or a refold
/// committing mid-evaluation could pair an old grant with a new cut. It therefore reads inside its
/// own transaction and cannot call the wrapper above, which would try to `BEGIN` a second one.
pub fn roster_content_authority_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    roster_ref: RosterRef,
    device_fingerprint: DeviceFingerprint,
    stream_id: StreamId,
) -> anyhow::Result<fold::AuthorityQuery<fold::RosterContentAuthority>> {
    let Some((roster, effective_at, closed_at)) = load_roster_fact(conn, account_id, &roster_ref)?
    else {
        return missing_reference(conn, account_id, &roster_ref.into());
    };
    if roster.device_fingerprint != device_fingerprint {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    // Range-validate the stored window: both bounds must be non-negative (a u64 fold clock). The
    // values themselves are not needed here — the per-stream cut row carries the boundary.
    let _ = (u64::try_from(effective_at)?, closed_at.map(u64::try_from).transpose()?);
    let cut: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT seq, entry_hash FROM account_roster_content_boundaries
             WHERE account_id = ?1 AND roster_ref = ?2 AND stream_id = ?3",
            params![
                account_id.to_bytes().as_slice(),
                roster_ref.as_slice(),
                stream_id.to_bytes().as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let boundary = match cut {
        Some((seq, hash)) => fold::AuthorityBoundary::Cut {
            seq: u64::from_be_bytes(id::fixed(&seq)?),
            hash: AccountEntryHash::from_bytes(id::fixed(&hash)?),
        },
        None if closed_at.is_none() => fold::AuthorityBoundary::Open,
        None => fold::AuthorityBoundary::Closed,
    };
    Ok(fold::AuthorityQuery::Effective(fold::RosterContentAuthority {
        device_fingerprint: roster.device_fingerprint,
        role: roster.current_role,
        boundary,
    }))
}

pub fn owner_control_authority(
    conn: &Connection,
    account_id: AccountId,
    owner_id: OwnerId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    owner_chain_authority(conn, account_id, owner_id, device_fingerprint, AuthorityChain::Control)
}

/// Snapshot-safe counterpart of [`owner_control_authority`]. Callers that already own a
/// transaction use this to keep an authority check and the authored mutation in one snapshot
/// without attempting to open a nested read transaction.
pub fn owner_control_authority_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    owner_id: OwnerId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    owner_chain_authority_in_snapshot(
        conn,
        account_id,
        owner_id,
        device_fingerprint,
        AuthorityChain::Control,
    )
}

pub fn owner_secrets_authority(
    conn: &Connection,
    account_id: AccountId,
    owner_id: OwnerId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    owner_chain_authority(conn, account_id, owner_id, device_fingerprint, AuthorityChain::Secrets)
}

/// The body of [`owner_secrets_authority`], reading whatever snapshot `conn` is already in — the
/// secrets refold (C4.2b) resolves every wrap's owner-incarnation authority inside its own txn and
/// cannot call the wrapper above, which would try to `BEGIN` a second one (S1; mirrors
/// [`stream_owner_effective_in_snapshot`]).
pub fn owner_secrets_authority_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    owner_id: OwnerId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    owner_chain_authority_in_snapshot(
        conn,
        account_id,
        owner_id,
        device_fingerprint,
        AuthorityChain::Secrets,
    )
}

fn owner_chain_authority(
    conn: &Connection,
    account_id: AccountId,
    owner_id: OwnerId,
    device_fingerprint: DeviceFingerprint,
    chain: AuthorityChain,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    owner_chain_authority_in_snapshot(conn, account_id, owner_id, device_fingerprint, chain)
}

/// The body of [`owner_chain_authority`], reading whatever snapshot `conn` is already in.
fn owner_chain_authority_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    owner_id: OwnerId,
    device_fingerprint: DeviceFingerprint,
    chain: AuthorityChain,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let chain = chain.column_prefix();
    let sql = format!(
        "SELECT o.device_fingerprint, o.effective_at, o.closed_at,
                o.{chain}_boundary, o.{chain}_seq, o.{chain}_hash,
                r.effective_at, r.closed_at, r.{chain}_boundary, r.{chain}_seq, r.{chain}_hash
         FROM account_owner_incarnations o
         LEFT JOIN account_roster_history r
           ON r.account_id = o.account_id AND r.device_fingerprint = o.device_fingerprint
         WHERE o.account_id = ?1 AND o.owner_id = ?2"
    );
    type BoundaryRow = (
        Vec<u8>,
        i64,
        Option<i64>,
        String,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
    );
    let row: Option<BoundaryRow> = conn
        .query_row(&sql, params![account_id.to_bytes().as_slice(), owner_id.as_slice()], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
            ))
        })
        .optional()?;
    let Some((
        device,
        owner_effective,
        owner_closed,
        owner_kind,
        owner_seq,
        owner_hash,
        device_effective,
        device_closed,
        device_kind,
        device_seq,
        device_hash,
    )) = row
    else {
        return missing_reference(conn, account_id, &owner_id.into());
    };
    let owner = fold::OwnerAuthority {
        device_fingerprint: DeviceFingerprint::from_bytes(id::fixed(&device)?),
    };
    if owner.device_fingerprint != device_fingerprint {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    let _ = (
        u64::try_from(owner_effective)?,
        owner_closed.map(u64::try_from).transpose()?,
        device_effective.map(u64::try_from).transpose()?,
        device_closed.map(u64::try_from).transpose()?,
    );
    let device_boundary = match device_kind {
        Some(kind) =>
            decode_stored_boundary(&kind, device_seq, device_hash, device_closed, false, true)?,
        None => fold::AuthorityBoundary::Closed,
    };
    let incarnation_boundary = decode_stored_boundary(
        &owner_kind,
        owner_seq,
        owner_hash,
        owner_closed,
        device_boundary != fold::AuthorityBoundary::Open,
        false,
    )?;
    Ok(fold::AuthorityQuery::Effective(fold::OwnerChainAuthority {
        owner,
        device_boundary,
        incarnation_boundary,
    }))
}

/// Verify every snapshot this device holds for `account_id` against the account history it holds.
///
/// The read/query surface for [`annex::verify`], and deliberately READ-ONLY: it returns verdicts
/// and changes nothing. A `Mismatch` here does not delete, condemn, or unaccept the entry — a
/// snapshot whose claim is false stays stored and is simply never trusted. Nothing may feed a
/// verdict back into acceptance, because verifying consults the local candidate inventory and an
/// acceptance rule that did so would make the verdict device-dependent.
///
/// Entries that are not current-version plaintext snapshots on the annex log are skipped, and a
/// manifest this binary cannot interpret (a future state format) simply yields no verdict rather
/// than a failure.
pub(in crate::account) fn verify_stored_snapshots(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<(AccountEntryHash, annex::verify::SnapshotVerdict)>> {
    let rows = load_candidates(conn, account_id)?;
    let held: Vec<envelope::VerifiedAccountEntry> =
        rows.iter().map(|row| row.verified.clone()).collect();
    let mut verdicts = Vec::new();
    for row in &rows {
        let header = &row.verified.header;
        if header.log_id != fold::ANNEX_LOG
            || header.crypto_suite != 0
            || header.op_version != fold::SUPPORTED_OP_VERSION
        {
            continue;
        }
        let Ok(annex::ops::DecodedAnnexOp::Known(annex::ops::AnnexOp::Snapshot {
            targets, ..
        })) = annex::ops::decode(header.entry_type, &row.verified.payload)
        else {
            // An unknown tag or a future state format is retained and uninterpretable here — not a
            // verdict, and not an error.
            continue;
        };
        verdicts.push((row.entry_hash, annex::verify::verify_snapshot(&held, &targets)));
    }
    verdicts.sort_unstable_by_key(|(hash, _)| *hash);
    Ok(verdicts)
}

/// A snapshot this device may act on: it verified against local history, and the owner incarnation
/// it cites is still open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct UsableSnapshot {
    pub(in crate::account) entry_hash: AccountEntryHash,
    pub(in crate::account) targets: Vec<annex::ops::SnapshotTarget>,
}

/// Every stored snapshot that verified AND whose author's cited incarnation is still open.
///
/// The incarnation gate is the revocation story for this artifact class. No control op can cut an
/// annex chain — registers are minted per log and `ChainKind` has no annex variant — so a revoked
/// device's snapshots cannot be condemned by watermark the way its control entries are. Scoping
/// usability to the cited incarnation gives a rule that needs no wire change and is derived purely
/// from the control fold, so it is the same for every device holding the same control history.
///
/// A snapshot citing no authority is never usable: an unauthorized claim about folded state is not
/// something to weigh, it is something to ignore.
pub(in crate::account) fn usable_snapshots(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<UsableSnapshot>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let rows = load_candidates(conn, account_id)?;
    let held: Vec<envelope::VerifiedAccountEntry> =
        rows.iter().map(|row| row.verified.clone()).collect();
    let mut usable = Vec::new();
    for row in &rows {
        let header = &row.verified.header;
        if header.log_id != fold::ANNEX_LOG
            || header.crypto_suite != 0
            || header.op_version != fold::SUPPORTED_OP_VERSION
        {
            continue;
        }
        let Ok(annex::ops::DecodedAnnexOp::Known(annex::ops::AnnexOp::Snapshot {
            targets, ..
        })) = annex::ops::decode(header.entry_type, &row.verified.payload)
        else {
            continue;
        };
        let Some(owner_id) = header.authority_ref else {
            continue;
        };
        if !matches!(
            owner_incarnation_effective(conn, account_id, owner_id, header.device_fingerprint)?,
            fold::AuthorityQuery::Effective(_)
        ) {
            continue;
        }
        if annex::verify::verify_snapshot(&held, &targets)
            != annex::verify::SnapshotVerdict::Verified
        {
            continue;
        }
        // Keep ONLY the targets verification actually checked. Selection ranks by coverage, so
        // carrying unverified targets would let an author pad a manifest with fabricated secrets or
        // content coverage and outrank an honest snapshot on claims nobody validated.
        let verified_targets: Vec<_> =
            targets.into_iter().filter(annex::verify::is_supported_target).collect();
        usable.push(UsableSnapshot { entry_hash: row.entry_hash, targets: verified_targets });
    }
    usable.sort_unstable_by_key(|snapshot| snapshot.entry_hash);
    Ok(usable)
}

/// The snapshot this device would act on, if any: the maximal-coverage usable one, `entry_hash`
/// breaking ties. Read-only and advisory, like everything on this path.
pub(in crate::account) fn selected_snapshot(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Option<UsableSnapshot>> {
    let usable = usable_snapshots(conn, account_id)?;
    let candidates: Vec<annex::select::Candidate<'_>> = usable
        .iter()
        .map(|s| annex::select::Candidate { entry_hash: s.entry_hash, targets: &s.targets })
        .collect();
    let Some(chosen) = annex::select::select(&candidates) else {
        return Ok(None);
    };
    Ok(usable.into_iter().find(|s| s.entry_hash == chosen))
}

pub fn owner_incarnation_effective(
    conn: &Connection,
    account_id: AccountId,
    owner_id: OwnerId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let row: Option<(Vec<u8>, i64, Option<i64>)> = conn
        .query_row(
            "SELECT device_fingerprint, effective_at, closed_at
             FROM account_owner_incarnations WHERE account_id = ?1 AND owner_id = ?2",
            params![account_id.to_bytes().as_slice(), owner_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((device, effective_at, closed_at)) = row else {
        return missing_reference(conn, account_id, &owner_id.into());
    };
    let authority = fold::OwnerAuthority {
        device_fingerprint: DeviceFingerprint::from_bytes(id::fixed(&device)?),
    };
    if authority.device_fingerprint != device_fingerprint {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    validated_open_fact(authority, effective_at, closed_at)
}

type StoredGrantRow = (Vec<u8>, Vec<u8>, String, i64, Option<i64>);

pub fn grant_effective(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: GrantId,
    stream_id: StreamId,
    grantee_account_id: AccountId,
) -> anyhow::Result<fold::AuthorityQuery<fold::GrantAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    grant_effective_in_snapshot(conn, owner_account_id, grant_id, stream_id, grantee_account_id)
}

/// The body of [`grant_effective`], reading whatever snapshot `conn` is already in — so an
/// owner authoring a grant can verify the resulting FACT inside its own IMMEDIATE txn (mirrors
/// [`grant_effective_for_device_in_snapshot`] and the DeviceAdd post-author fact check).
pub fn grant_effective_in_snapshot(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: GrantId,
    stream_id: StreamId,
    grantee_account_id: AccountId,
) -> anyhow::Result<fold::AuthorityQuery<fold::GrantAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, owner_account_id)?;
    super::control_policy::require_supported_account_control(conn, grantee_account_id)?;
    let row: Option<StoredGrantRow> = conn
        .query_row(
            "SELECT stream_id, grantee_account_id, role, effective_at, closed_at
             FROM account_stream_grants WHERE owner_account_id = ?1 AND grant_id = ?2",
            params![owner_account_id.to_bytes().as_slice(), grant_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((stored_stream, stored_grantee, role, effective_at, closed_at)) = row else {
        return missing_reference(conn, owner_account_id, &grant_id.into());
    };
    let authority = fold::GrantAuthority {
        stream_id: StreamId::from_bytes(id::fixed(&stored_stream)?),
        grantee_account_id: AccountId::from_bytes(id::fixed(&stored_grantee)?),
        role: GrantRole::from_db_str(&role)?,
    };
    if authority.stream_id != stream_id || authority.grantee_account_id != grantee_account_id {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    validated_fact(authority, effective_at, closed_at)
}

/// The `grant_id` of an effective (not-closed) Writer grant for `grantee_account_id` on
/// `(owner_account_id, stream_id)`, or `None`. A granted contributor uses this to cite its grant
/// when authoring onto the owner's stream (#1164): the grant lives in the OWNER's control log and
/// is synced to the contributor, so this reads the projected `account_stream_grants` row. Unlike
/// [`grant_effective`] (which VERIFIES a known `grant_id`), this SEARCHES for the contributor's
/// grant.
pub fn effective_writer_grant(
    conn: &Connection,
    owner_account_id: AccountId,
    stream_id: StreamId,
    grantee_account_id: AccountId,
) -> anyhow::Result<Option<GrantId>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, owner_account_id)?;
    super::control_policy::require_supported_account_control(conn, grantee_account_id)?;
    let grant_id: Option<Vec<u8>> = conn
        .query_row(
            "SELECT grant_id FROM account_stream_grants
             WHERE owner_account_id = ?1 AND stream_id = ?2 AND grantee_account_id = ?3
               AND role = ?4 AND closed_at IS NULL",
            params![
                owner_account_id.to_bytes().as_slice(),
                stream_id.to_bytes().as_slice(),
                grantee_account_id.to_bytes().as_slice(),
                GrantRole::Writer.as_db_str(),
            ],
            |row| row.get(0),
        )
        .optional()?;
    grant_id.map(|bytes| id::fixed(&bytes).map(GrantId::from_bytes)).transpose()
}

/// Every OPEN (not-closed) WRITER grant `owner_account_id` holds for `grantee_account_id` on
/// `stream_id`, newest first — the owner-side resolver behind `sync revoke`, which names the
/// grantee while the wire op names a grant. Writer-scoped on purpose: revoking write access must
/// target the grants that CONFER writing — a grantee also holding a Reader grant would otherwise
/// have the reader row closed while it keeps authoring. Plural on purpose: double-granting
/// authors two effective grant ids, and closing only one would leave the grantee writing while
/// the command reports success.
pub fn open_writer_grants(
    conn: &Connection,
    owner_account_id: AccountId,
    stream_id: StreamId,
    grantee_account_id: AccountId,
) -> anyhow::Result<Vec<GrantId>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, owner_account_id)?;
    super::control_policy::require_supported_account_control(conn, grantee_account_id)?;
    let mut stmt = conn.prepare(
        "SELECT grant_id FROM account_stream_grants
         WHERE owner_account_id = ?1 AND stream_id = ?2 AND grantee_account_id = ?3
           AND role = ?4 AND closed_at IS NULL
         ORDER BY effective_at DESC, grant_id",
    )?;
    let rows = stmt.query_map(
        params![
            owner_account_id.to_bytes().as_slice(),
            stream_id.to_bytes().as_slice(),
            grantee_account_id.to_bytes().as_slice(),
            GrantRole::Writer.as_db_str(),
        ],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    let mut grants = Vec::new();
    for row in rows {
        grants.push(GrantId::from_bytes(id::fixed(&row?)?));
    }
    Ok(grants)
}

/// One row of the owner-facing grant listing (`sync grants`).
#[derive(Debug, Clone)]
pub struct StreamGrantListing {
    pub grant_id: GrantId,
    pub grantee_account_id: AccountId,
    /// The projected role token (`reader`/`writer`).
    pub role: String,
    /// Still effective — not revoked.
    pub open: bool,
}

/// Every grant `owner_account_id` has authored on `stream_id`, open and revoked, newest first —
/// what `sync grants` shows an owner, who otherwise has no way to see who holds access.
pub fn stream_grants_for_owner(
    conn: &Connection,
    owner_account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<Vec<StreamGrantListing>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, owner_account_id)?;
    let mut stmt = conn.prepare(
        "SELECT grant_id, grantee_account_id, role, closed_at IS NULL
         FROM account_stream_grants
         WHERE owner_account_id = ?1 AND stream_id = ?2
         ORDER BY effective_at DESC, grant_id",
    )?;
    let rows = stmt.query_map(
        params![owner_account_id.to_bytes().as_slice(), stream_id.to_bytes().as_slice()],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        },
    )?;
    let mut listings = Vec::new();
    for row in rows {
        let (grant_id, grantee, role, open) = row?;
        listings.push(StreamGrantListing {
            grant_id: GrantId::from_bytes(id::fixed(&grant_id)?),
            grantee_account_id: AccountId::from_bytes(id::fixed(&grantee)?),
            role,
            open,
        });
    }
    Ok(listings)
}

/// The distinct accounts holding an effective (not-closed) Writer grant on any stream owned by
/// `owner_account_id`. Automatic cross-account sync (#1175) pulls each grantee's own account —
/// content is offered by AUTHOR, so a contributor's entries on the owner's stream travel only when
/// the OWNER dials the contributor and syncs the CONTRIBUTOR's account. Reader grants never
/// author, so they are excluded. Owner-leading, so the scan rides the `(owner, stream, grantee)`
/// index.
pub fn effective_writer_grantees(
    conn: &Connection,
    owner_account_id: AccountId,
) -> anyhow::Result<Vec<AccountId>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, owner_account_id)?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT grantee_account_id FROM account_stream_grants
         WHERE owner_account_id = ?1 AND role = ?2 AND closed_at IS NULL
         ORDER BY grantee_account_id",
    )?;
    let rows = stmt.query_map(
        params![owner_account_id.to_bytes().as_slice(), GrantRole::Writer.as_db_str()],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    let mut grantees = Vec::new();
    for row in rows {
        grantees.push(AccountId::from_bytes(id::fixed(&row?)?));
    }
    Ok(grantees)
}

/// Whether `owner_account_id` has ever granted `grantee_account_id` a stream — open or closed, any
/// role (#1280). The relay admission question: an owner's session may carry the logs of the
/// accounts it granted. A closed grant still counts, because its cut leaves the history before it
/// accepted and a peer must still be able to verify that history.
pub fn owner_ever_granted(
    conn: &Connection,
    owner_account_id: AccountId,
    grantee_account_id: AccountId,
) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_stream_grants
             WHERE owner_account_id = ?1 AND grantee_account_id = ?2
         )",
        params![owner_account_id.to_bytes().as_slice(), grantee_account_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?)
}

/// Every account `owner_account_id` has ever granted a stream, open or closed, any role — the logs
/// an owner relays (#1280). See [`owner_ever_granted`] for why a closed grant still counts.
pub fn ever_granted_accounts(
    conn: &Connection,
    owner_account_id: AccountId,
) -> anyhow::Result<Vec<AccountId>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT grantee_account_id FROM account_stream_grants
         WHERE owner_account_id = ?1 AND grantee_account_id != ?1
         ORDER BY grantee_account_id",
    )?;
    let rows = stmt
        .query_map(params![owner_account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.iter().map(|bytes| Ok(AccountId::from_bytes(id::fixed(bytes)?))).collect()
}

/// Whether `grantee_account_id` holds an effective (not-closed) Writer grant on a stream that
/// resolves **`PublicRead`**. Unlike [`effective_writer_grant`], which answers "may this account
/// write to THIS stream", this answers "is this account a public contributor at all" — the question
/// the serve policy asks when deciding whether an account owning no stream of its own is
/// nonetheless a deliberately-participating identity worth serving (#1164), rather than a fresh
/// empty account that must stay unexposed.
///
/// AVAILABILITY CAVEAT. Servability tracks CURRENT grant effectiveness, so closing a contributor's
/// last public grant also closes the only door through which its account is reachable — content is
/// served by its AUTHOR account, and the owner is not enrolled in the contributor's account. Cut
/// semantics deliberately keep entries at or below the cut accepted, so a pre-revocation
/// contribution that was never replicated stays VALID but becomes UNREACHABLE. Automatic
/// cross-account sync (#1175) shrinks that window to "unreplicated at the moment of revocation";
/// closing it properly is revoke's problem (#1177) — replicate before closing, or serve a bounded
/// tail afterwards.
///
/// The access-mode check is load-bearing and cannot be assumed away: the fold does not yet require
/// `PublicRead` for a `StreamGrant` (#1178), so a grant on a PRIVATE stream is representable, and
/// counting it would let a private relationship justify exposing an account log to anonymous
/// readers. [`stream_access_mode`] fails closed to `Private` when the ownership fact is missing, so
/// a grant this store cannot verify does not qualify.
pub fn account_holds_effective_public_writer_grant(
    conn: &Connection,
    grantee_account_id: AccountId,
) -> anyhow::Result<bool> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, grantee_account_id)?;
    let mut stmt = conn.prepare(
        "SELECT owner_account_id, stream_id FROM account_stream_grants
         WHERE grantee_account_id = ?1 AND role = ?2 AND closed_at IS NULL",
    )?;
    let grants = stmt
        .query_map(
            params![grantee_account_id.to_bytes().as_slice(), GrantRole::Writer.as_db_str()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (owner, stream) in grants {
        let owner = AccountId::from_bytes(id::fixed(&owner)?);
        let stream = StreamId::from_bytes(id::fixed(&stream)?);
        if stream_access_mode(conn, owner, stream)? == crate::stream::AccessMode::PublicRead {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Resolve a grant and the requesting device's revoke cut as ONE authorization decision. C2 must
/// use this combined seam when admitting content: two independent calls could otherwise straddle
/// a refold and combine an old effective grant with a new (or absent) cut projection.
pub fn grant_effective_for_device(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: GrantId,
    stream_id: StreamId,
    grantee_account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::GrantDeviceAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    grant_effective_for_device_in_snapshot(
        conn,
        owner_account_id,
        grant_id,
        stream_id,
        grantee_account_id,
        device_fingerprint,
    )
}

/// The body of [`grant_effective_for_device`], reading whatever snapshot `conn` is already in — see
/// [`roster_content_authority_in_snapshot`] for why the `/3` refold needs this shape.
pub fn grant_effective_for_device_in_snapshot(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: GrantId,
    stream_id: StreamId,
    grantee_account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::GrantDeviceAuthority>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, owner_account_id)?;
    super::control_policy::require_supported_account_control(conn, grantee_account_id)?;
    let row: Option<StoredGrantRow> = conn
        .query_row(
            "SELECT stream_id, grantee_account_id, role, effective_at, closed_at
             FROM account_stream_grants WHERE owner_account_id = ?1 AND grant_id = ?2",
            params![owner_account_id.to_bytes().as_slice(), grant_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((stored_stream, stored_grantee, role, effective_at, closed_at)) = row else {
        return missing_reference(conn, owner_account_id, &grant_id.into());
    };
    let grant = fold::GrantAuthority {
        stream_id: StreamId::from_bytes(id::fixed(&stored_stream)?),
        grantee_account_id: AccountId::from_bytes(id::fixed(&stored_grantee)?),
        role: GrantRole::from_db_str(&role)?,
    };
    if grant.stream_id != stream_id || grant.grantee_account_id != grantee_account_id {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    let _ = validated_fact(grant, effective_at, closed_at)?;
    let device_cut = load_grant_device_cut(conn, owner_account_id, grant_id, device_fingerprint)?;
    let boundary = match (closed_at, device_cut) {
        (None, None) => fold::GrantDeviceBoundary::Open,
        (None, Some(_)) => anyhow::bail!("open grant unexpectedly has a persisted device cut"),
        (Some(_), Some(cut)) => fold::GrantDeviceBoundary::Cut(cut),
        (Some(_), None) => fold::GrantDeviceBoundary::Closed,
    };
    Ok(fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority { grant, boundary }))
}

/// Resolve the owner-bound `StreamOwn` fact from the current fold. A missing ownership fact is
/// recoverable: the citing author may simply hold control ops we have not folded yet.
pub fn stream_owner_effective(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<fold::AuthorityQuery<AccountEntryHash>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    stream_owner_effective_in_snapshot(conn, account_id, stream_id)
}

/// The body of [`stream_owner_effective`], reading whatever snapshot `conn` is already in — see
/// [`roster_content_authority_in_snapshot`] for why the `/3` refold needs this shape.
pub fn stream_owner_effective_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<fold::AuthorityQuery<AccountEntryHash>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let row: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT own_id, effective_at FROM account_stream_ownership
             WHERE account_id = ?1 AND stream_id = ?2",
            params![account_id.to_bytes().as_slice(), stream_id.to_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((own_id, effective_at)) = row else {
        return Ok(fold::AuthorityQuery::Unknown);
    };
    validated_fact(AccountEntryHash::from_bytes(id::fixed(&own_id)?), effective_at, None)
}

/// Keyed per-device cut lookup for C2 content authorization. The grant's final effectiveness and
/// the optional cut are read from one SQLite snapshot, so a concurrent refold cannot mix rounds.
pub(super) fn grant_device_cut(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: GrantId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<Option<DeviceCut>>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, owner_account_id)?;
    let grant_exists: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_stream_grants
             WHERE owner_account_id = ?1 AND grant_id = ?2
         )",
        params![owner_account_id.to_bytes().as_slice(), grant_id.as_slice()],
        |row| row.get(0),
    )?;
    if !grant_exists {
        return missing_reference(conn, owner_account_id, &grant_id.into());
    }
    let cut = load_grant_device_cut(conn, owner_account_id, grant_id, device_fingerprint)?;
    Ok(fold::AuthorityQuery::Effective(cut))
}

fn load_grant_device_cut(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: GrantId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<Option<DeviceCut>> {
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT seq, entry_hash FROM account_stream_grant_cuts
             WHERE owner_account_id = ?1 AND grant_id = ?2 AND device_fingerprint = ?3",
            params![
                owner_account_id.to_bytes().as_slice(),
                grant_id.as_slice(),
                device_fingerprint.to_bytes().as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(seq, hash)| -> anyhow::Result<DeviceCut> {
        Ok(DeviceCut {
            device_fingerprint,
            seq: u64::from_be_bytes(id::fixed(&seq)?),
            hash: AccountEntryHash::from_bytes(id::fixed(&hash)?),
        })
    })
    .transpose()
}

/// The account that owns `stream_id`, per the current fold.
///
/// A `/3` header never names its owner: `stream_id` is `sha256(cbor([..., owner_account_id,
/// ...]))`, so ownership is INSIDE the identity and two accounts claiming one stream is
/// cryptographically impossible (§14). The preimage is not invertible, though, so the owner is
/// resolved through the `StreamOwn` fact the owner published. No fact ⇒ we do not know who owns
/// this stream yet, and nothing on it can be authorized — that is recoverable, never a rejection.
pub fn stream_owner_account(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<Option<AccountId>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_stream_control(conn, stream_id)?;
    stream_owner_account_unchecked(conn, stream_id)
}

/// [`stream_owner_account`] for the paths that RETRACT rather than act: a stream routed to a
/// pinned account reads as owned by nobody, which is the declassify-to-structural verdict the
/// refold and the projector already give a stream whose owner is contested. The signed candidates
/// stay stored for a binary that can execute the pin; only their acceptance and projection go.
pub(in crate::account) fn stream_owner_account_for_cleanup(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<Option<AccountId>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    if super::control_policy::stream_control_pinned(conn, stream_id)? {
        return Ok(None);
    }
    stream_owner_account_unchecked(conn, stream_id)
}

fn stream_owner_account_unchecked(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<Option<AccountId>> {
    let owner: Option<Vec<u8>> = conn
        .query_row(
            "SELECT account_id FROM account_stream_ownership WHERE stream_id = ?1",
            [stream_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    owner.map(|bytes| Ok(AccountId::from_bytes(id::fixed(&bytes)?))).transpose()
}

/// The [`AccessMode`] of `stream_id` as declared in `owner_account_id`'s effective `StreamOwn`
/// fact.
///
/// Decode-on-read: `account_stream_ownership` records only the owning entry hash (`own_id`), so the
/// mode is read back from that `StreamOwn` entry's spec bytes — no dedicated column. Returns
/// [`AccessMode::Private`] (FAIL-CLOSED) whenever no effective ownership fact is folded locally: an
/// unknown or not-yet-synced owner is treated as private, never public, so admission callers open
/// nothing on a stream whose owner authority has not yet arrived. Resolve the owner FROM THE STREAM
/// via [`stream_owner_account`] before calling — never from an attacker-settable claimed author.
pub fn stream_access_mode(
    conn: &Connection,
    owner_account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<AccessMode> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, owner_account_id)?;
    let own_id: Option<Vec<u8>> = conn
        .query_row(
            "SELECT own_id FROM account_stream_ownership
             WHERE account_id = ?1 AND stream_id = ?2",
            params![owner_account_id.to_bytes().as_slice(), stream_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(own_id) = own_id else {
        return Ok(AccessMode::Private);
    };
    let raw: Option<Vec<u8>> = conn
        .query_row(
            "SELECT signed_bytes FROM account_entries
             WHERE entry_hash = ?1 AND account_id = ?2",
            params![own_id.as_slice(), owner_account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        // Ownership projected effective but its source entry is not present — fail closed.
        return Ok(AccessMode::Private);
    };
    let entry = envelope::decode_account_signed(&raw)?;
    let DecodedAccountOp::Known(AccountOp::StreamOwn { stream_id: owned, stream_spec_bytes }) =
        ops::decode(entry.header.entry_type, &entry.payload)?
    else {
        anyhow::bail!("ownership fact does not name a StreamOwn operation");
    };
    let spec = crate::stream::decode_spec_v2(&stream_spec_bytes)?;
    // The fold makes a StreamOwn effective only after checking `derive_v2(spec) == stream_id`, so
    // this must already hold; re-assert to document the dependency this decode-on-read leans on.
    anyhow::ensure!(
        owned == stream_id && crate::stream::derive_v2(&spec)? == stream_id,
        "ownership fact spec does not derive its stream id",
    );
    Ok(spec.access_mode)
}

/// Whether EVERY `StreamOwn` this account holds declares `PublicRead` — the fully-public gate for
/// anonymous serving (#407 E2b). Scans RAW `StreamOwn` candidate rows in `account_entries`
/// (effective AND non-effective/forked), NOT just effective ownership: the authenticated account
/// serve ([`account_entries_for_enrollment`]) ships every candidate, so a single `Private` (or
/// undecodable) `StreamOwn` anywhere means the account has private material and must never be
/// served to an anonymous reader. FAIL-CLOSED: any decode anomaly returns `false`. Vacuously `true`
/// for an account with no `StreamOwn`. A store serving [`crate::stream::AccessMode`]-scoped
/// `PublicOnly` content gates on this so a mis-flagged account leaks nothing, independent of how
/// its policy was selected.
pub fn account_is_fully_public(conn: &Connection, account_id: AccountId) -> anyhow::Result<bool> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let mut stmt = conn.prepare(
        "SELECT signed_bytes FROM account_entries
         WHERE account_id = ?1 AND entry_type = ?2 AND log_id = ?3",
    )?;
    let rows = stmt
        .query_map(
            params![
                account_id.to_bytes().as_slice(),
                ops::entry_type::STREAM_OWN,
                fold::CONTROL_LOG,
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for raw in rows {
        let Ok(entry) = envelope::decode_account_signed(&raw) else {
            return Ok(false);
        };
        let Ok(DecodedAccountOp::Known(AccountOp::StreamOwn { stream_spec_bytes, .. })) =
            ops::decode(entry.header.entry_type, &entry.payload)
        else {
            return Ok(false);
        };
        let is_public = crate::stream::decode_spec_v2(&stream_spec_bytes)
            .map(|spec| spec.access_mode == crate::stream::AccessMode::PublicRead)
            .unwrap_or(false);
        if !is_public {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether an account has folded to `contested` — a genuine owner-key-compromise / equivocation
/// event (§12), which HALTS authority mutation. Content authorized by a contested account is
/// fail-closed: parked (quota-bounded), never accepted, and reclassified if the account recovers.
pub fn account_is_contested(conn: &Connection, account_id: AccountId) -> anyhow::Result<bool> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let classification: Option<String> = conn
        .query_row(
            "SELECT classification FROM account_auth_state WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(classification.is_some_and(|state| state == "contested"))
}

/// Measure an asserted control-fold length against the control log we HOLD (§7) — the ONE seam
/// that reads `auth_len`. Keeping it out of the fact queries above is what stops a counter from
/// acting as an authority input: facts always answer from the current fold, and the caller applies
/// this verdict as its own phase, where an `Ahead` author parks rather than pre-empting a decision
/// the fold has already made. An account we hold nothing for holds zero rows (its facts resolve
/// `Unknown` long before freshness is consulted).
///
/// Held rows, NOT the effective count (#1282). A cut can condemn ops an author had counted, so the
/// effective count can drop below a length that author legitimately cited; measured against it,
/// accepted content would park `auth_len_ahead` with nothing left to fetch, and the drain would
/// remove its memories. Held rows are never deleted, so this measure never shrinks, and it is never
/// below the effective count an author cites, so a store's own content never parks against it.
/// Anyone can add candidate rows to an account's log (bounded per account), which can only end an
/// `Ahead` park early: freshness grants no authority, and every content refold re-decides each
/// entry, so an early verdict corrects itself when the missing ops arrive.
pub fn auth_len_freshness(
    conn: &Connection,
    account_id: AccountId,
    asserted_auth_len: u64,
) -> anyhow::Result<fold::AuthorityFreshness> {
    Ok(fold::AuthorityFreshness::of(asserted_auth_len, held_control_log_len(conn, account_id)?))
}

/// The control-log rows held for `account_id` — what [`auth_len_freshness`] measures a cited
/// length against. A refold that checks many entries reads it once per account and compares each
/// citation with [`fold::AuthorityFreshness::of`], rather than counting the log per citation.
/// Held rows are never deleted, so it also versions what [`device_ever_enrolled_as_writer`]
/// derives from: an answer computed at one length stays exact while the length is unchanged.
pub fn held_control_log_len(conn: &Connection, account_id: AccountId) -> anyhow::Result<u64> {
    let held: i64 = conn.query_row(
        "SELECT COUNT(*) FROM account_entries WHERE account_id = ?1 AND log_id = ?2",
        params![account_id.to_bytes().as_slice(), fold::CONTROL_LOG],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(held)?)
}

/// This account's current effective control-fold length, read from whatever snapshot `conn` is
/// already in. The in-tx content-author seam stamps it as the `owner_auth_len`/`author_auth_len`
/// it cites; [`auth_len_freshness`] measures against the held control log, which is never shorter,
/// so its own entries never park `auth_len_ahead` against its own store; it MUST be read in the
/// SAME snapshot as the authoring txn, or a concurrent control-fold advance would let the citation
/// straddle two folds. Zero for an account we hold nothing for (its facts resolve `Unknown` long
/// before freshness).
pub fn account_effective_count(conn: &Connection, account_id: AccountId) -> anyhow::Result<u64> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let effective_count: Option<i64> = conn
        .query_row(
            "SELECT effective_count FROM account_auth_state WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(effective_count.map(u64::try_from).transpose()?.unwrap_or_default())
}

/// The `owner_id` of the device's CURRENTLY-LIVE owner incarnation — the `entry_hash` of the still
/// -open genesis / `OwnerPromote` that put it in the owner role — or `None` when the device holds
/// no open owner incarnation. A REVERSE lookup by device, distinct from
/// [`owner_incarnation_effective`] (which self-opens a Deferred txn and VALIDATES a known
/// `owner_id`); this reads whatever snapshot `conn` is already in, so the in-tx author seam calls
/// it with its own `tx`. Normally exactly one open incarnation exists per device; `ORDER BY
/// effective_at DESC, owner_id` keeps the pick deterministic if more than one ever coexisted. The
/// `StreamKeyWrap` author cites this as `authority_ref`: for the founder it resolves to the genesis
/// hash (a founder's `owner_id` IS its genesis), but a demoted-then-repromoted or non-founder owner
/// gets its CURRENT incarnation, so a hard-coded genesis would cite a CLOSED incarnation and roll
/// every mint back.
pub(in crate::account) fn effective_owner_incarnation_for_device(
    conn: &Connection,
    account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<Option<OwnerId>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let owner_id: Option<Vec<u8>> = conn
        .query_row(
            "SELECT owner_id FROM account_owner_incarnations
             WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NULL
             ORDER BY effective_at DESC, owner_id LIMIT 1",
            params![account_id.to_bytes().as_slice(), device_fingerprint.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    owner_id.map(|bytes| id::fixed(&bytes).map(OwnerId::from_bytes)).transpose()
}

fn missing_reference<T>(
    conn: &Connection,
    account_id: AccountId,
    reference: &AccountEntryHash,
) -> anyhow::Result<fold::AuthorityQuery<T>> {
    let stored_account: Option<Vec<u8>> = conn
        .query_row(
            "SELECT account_id FROM account_entries WHERE entry_hash = ?1",
            [reference.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match stored_account {
        None => fold::AuthorityQuery::Unknown,
        Some(stored) if id::fixed(&stored)? != account_id.to_bytes() =>
            fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject),
        Some(_) =>
            fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::ReferencedEntryNotEffective),
    })
}

fn validated_fact<T>(
    authority: T,
    effective_at: i64,
    closed_at: Option<i64>,
) -> anyhow::Result<fold::AuthorityQuery<T>> {
    let _ = (u64::try_from(effective_at)?, closed_at.map(u64::try_from).transpose()?);
    Ok(fold::AuthorityQuery::Effective(authority))
}

fn validated_open_fact<T>(
    authority: T,
    effective_at: i64,
    closed_at: Option<i64>,
) -> anyhow::Result<fold::AuthorityQuery<T>> {
    let fact = validated_fact(authority, effective_at, closed_at)?;
    Ok(if closed_at.is_some() {
        fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::ReferencedEntryNotEffective)
    } else {
        fact
    })
}

fn stored_boundary(
    boundary: fold::AuthorityBoundary,
) -> (&'static str, Option<[u8; 8]>, Option<[u8; 32]>) {
    match boundary {
        fold::AuthorityBoundary::Open => ("open", None, None),
        fold::AuthorityBoundary::Closed => ("closed", None, None),
        fold::AuthorityBoundary::Cut { seq, hash } =>
            ("cut", Some(seq.to_be_bytes()), Some(hash.into())),
    }
}

fn decode_stored_boundary(
    kind: &str,
    seq: Option<Vec<u8>>,
    hash: Option<Vec<u8>>,
    closed_at: Option<i64>,
    allow_closed_open: bool,
    allow_open_bounded: bool,
) -> anyhow::Result<fold::AuthorityBoundary> {
    let boundary = match (kind, seq, hash) {
        ("open", None, None) => fold::AuthorityBoundary::Open,
        ("closed", None, None) => fold::AuthorityBoundary::Closed,
        ("cut", Some(seq), Some(hash)) => fold::AuthorityBoundary::Cut {
            seq: u64::from_be_bytes(id::fixed(&seq)?),
            hash: AccountEntryHash::from_bytes(id::fixed(&hash)?),
        },
        _ => anyhow::bail!("malformed persisted authority boundary"),
    };
    match (closed_at.is_some(), boundary) {
        (false, fold::AuthorityBoundary::Open)
        | (true, fold::AuthorityBoundary::Cut { .. } | fold::AuthorityBoundary::Closed) =>
            Ok(boundary),
        (true, fold::AuthorityBoundary::Open) if allow_closed_open => Ok(boundary),
        (false, fold::AuthorityBoundary::Cut { .. } | fold::AuthorityBoundary::Closed)
            if allow_open_bounded =>
            Ok(boundary),
        _ => anyhow::bail!("authority closure and boundary disagree"),
    }
}

/// Re-derive everything a freshly installed pin changes, in the install transaction. Routed through
/// the ordinary refold so the install and every later fold take the SAME dispatch: a pin this
/// binary executes rebuilds its projection from the checkpoint, one it cannot execute retracts it.
pub(super) fn refold_after_pin_install_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
) -> anyhow::Result<()> {
    // The WALL clock, not the last entry's arrival. Installing a pin is something happening now,
    // and anything downstream that compares a stored expiry against this clock (invite reservations
    // are `expires_at_ms > now_ms`) reads a long-expired row as outstanding when the "now" it is
    // handed is really the age of the newest entry.
    refold_in_tx(tx, account_id, rag_rat_base::time::now_ms())?;
    Ok(())
}

/// The refold body (caller owns the txn). Returns each entry_hash → its projected status.
/// `pub(super)` so [`super::bootstrap`] can fold its freshly-inserted local-account genesis inside
/// the same mint transaction, rather than nesting the self-transacting [`refold_account`].
pub(super) fn refold_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<HashMap<AccountEntryHash, EntryStatus>> {
    let state = fold_account_state_in_tx(tx, account_id, now_ms, PreVerifyPromotion::Skip)?;
    super::content::finalize_affected_streams(tx, &state.affected_streams, now_ms)?;
    // `state.rejected_content_promotions` is deliberately DISCARDED here: this trusted/local path
    // has no remote caller to signal evicted promotions to (the ingest outcome variants carry it
    // only on the untrusted remote path), and the evicted set is bounded by the per-author
    // pre-verify cap, so it can never grow into unbounded silent loss.
    Ok(state.statuses)
}

/// Retry rows unlocked by a locally authored `DeviceAdd`, then immediately finalize content in the
/// same trusted transaction. The ordinary local refold skips these sweeps because no other local
/// account op introduces a signing key.
pub(super) fn promote_after_local_device_add_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<()> {
    let _rejected_account_promotions = promote_pre_verify(tx, account_id, now_ms)?;
    let state = fold_account_state_in_tx(tx, account_id, now_ms, PreVerifyPromotion::Retry)?;
    let _rejected_content_promotions = state.rejected_content_promotions;
    super::content::finalize_affected_streams(tx, &state.affected_streams, now_ms)?;
    Ok(())
}

/// Remote account ingest commits account/authority/secrets state and durable content wakeups, but
/// leaves content acceptance and projection at the last completed finalization until settle.
fn refold_untrusted_ingest_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
    promotion: PreVerifyPromotion,
) -> anyhow::Result<AccountStateFold> {
    let state = fold_account_state_in_tx(tx, account_id, now_ms, promotion)?;
    super::content::queue_account_changed_streams(tx, &state.affected_streams, now_ms)?;
    Ok(state)
}

/// Whether this fold should re-attempt content rows parked before their author's device was
/// resolvable. ONLY a genesis or a `DeviceAdd` can make a previously unresolvable roster key
/// resolve, so every other entry skips the sweep — it would otherwise decode up to the per-author
/// pre-verify cap of parked envelopes on EVERY account entry, reintroducing exactly the per-entry
/// amplification this path exists to remove.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreVerifyPromotion {
    /// A genesis or `DeviceAdd`: retry the parked rows for this account.
    Retry,
    /// Any other entry, and every trusted/local refold: nothing new can resolve.
    Skip,
}

/// Fold account-owned state and return the exact content streams whose acceptance or projection
/// may depend on it. Content finalization is deliberately absent: trusted/local and untrusted
/// remote wrappers choose immediate finalization or durable queueing structurally.
fn fold_account_state_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
    promotion: PreVerifyPromotion,
) -> anyhow::Result<AccountStateFold> {
    let policy = super::control_policy::account_control_policy(tx, account_id)?;
    // A pin this binary cannot execute retracts instead of folding: acceptance and every derived
    // projection go, and the signed candidates stay for a binary that can execute it.
    if matches!(policy, super::control_policy::AccountControlPolicy::UnsupportedVersion(_)) {
        clear_unsupported_authority_in_tx(tx, account_id)?;
        return Ok(AccountStateFold::default());
    }
    let rows = load_candidates(tx, account_id)?;
    let projection = match policy {
        super::control_policy::AccountControlPolicy::ControlV2(_) => {
            let checkpoint = super::control_policy::verified_checkpoint(tx, account_id)?;
            let control_log = load_control_log_bytes(tx, account_id)?;
            derive_pinned_projection(&rows, &checkpoint, &control_log)
        },
        // An unpinned account folds v1, whatever versions its rows carry.
        super::control_policy::AccountControlPolicy::LegacyV1
        | super::control_policy::AccountControlPolicy::UnsupportedVersion(_) =>
            derive_account_projection(&rows),
    };

    // Streams this account owns BEFORE the projection rewrite: a fold that drops a `StreamOwn` fact
    // must still refold that stream so its content is declassified, but the ownership row is gone
    // after the rewrite — so capture it now and hand it to the content trigger below.
    let previously_owned = owned_stream_bytes(tx, account_id)?;

    // Rewrite atomically so the partial unique index never observes two accepted rows at a slot.
    tx.execute("UPDATE account_entries SET accepted = 0 WHERE account_id = ?1", params![
        account_id.to_bytes().as_slice()
    ])?;

    let mut statuses: HashMap<AccountEntryHash, EntryStatus> = HashMap::new();
    for row in rows {
        let accepted = projection.accepted.contains(&row.entry_hash);
        let (status, detail) = if accepted {
            (EntryStatus::Accepted, None)
        } else if projection.forked.contains(&row.entry_hash) {
            (EntryStatus::Forked, None)
        } else {
            match projection.history.outcome(&row.entry_hash) {
                Some(outcome) => outcome.taxonomy(),
                // Every non-forked candidate participates in the final fold.
                None => (EntryStatus::RetainedUnfolded, None),
            }
        };
        if accepted {
            tx.execute("UPDATE account_entries SET accepted = 1 WHERE entry_hash = ?1", params![
                row.entry_hash.as_slice()
            ])?;
        }
        tx.execute(
            "INSERT INTO account_entry_status(entry_hash, status, detail) VALUES (?1, ?2, ?3)
             ON CONFLICT(entry_hash) DO UPDATE SET status = excluded.status, detail = \
             excluded.detail",
            params![row.entry_hash.as_slice(), status.as_db_str(), detail],
        )?;
        statuses.insert(row.entry_hash, status);
    }
    rewrite_authority_projection(tx, account_id, &projection.history, now_ms)?;
    // Re-derive secrets-log (log 1) acceptance from the just-rewritten authority projection, in
    // this SAME txn (§15, C4.2b). The main loop above wrote `retained_unfolded` for every log-1
    // row (the declassify baseline); this pass OVERWRITES that — in both `account_entry_status`
    // and the returned `statuses` map (S4) — for the wraps it classifies. Same-txn placement is
    // what makes a control fold that condemns a device's secrets chain retro-condemn its wraps
    // atomically.
    super::secrets::refold_secrets_log(tx, account_id, &mut statuses)?;
    // Runs AFTER the projection rewrite and the secrets refold, so a `DeviceAdd`'s own key is
    // already visible to the roster resolution the promotion performs.
    let rejected_content_promotions = match promotion {
        PreVerifyPromotion::Retry =>
            super::content::promote_pre_verify_for_account(tx, account_id, now_ms)?,
        PreVerifyPromotion::Skip => Default::default(),
    };
    let unpinned = matches!(policy, super::control_policy::AccountControlPolicy::LegacyV1);
    // Content is retracted under EVERY pin, including one this binary folds: see
    // [`retract_pinned_content_in_tx`]. A pinned account therefore reports no affected streams, so
    // the ordinary finalize never walks the gated stream-authority chain.
    let affected_streams = if unpinned {
        super::content::affected_streams_for_account(tx, account_id, &previously_owned)?
    } else {
        retract_pinned_content_in_tx(tx, account_id)?;
        Vec::new()
    };
    // Every fold path — trusted, untrusted, and the DeviceAdd promotion sweep — can grow the
    // live key-target set (a locally minted key, a remotely synced `StreamOwn`/wrap, or a parked
    // wrap a promoted DeviceAdd just certified). Top outstanding invite reservations up to the
    // new mandatory redemption cost inside the same transaction (#945).
    //
    // Skipped under EVERY pin. There is nothing to reserve capacity for on an account no
    // enrollment can redeem against, and the top-up resolves the account's streams through the
    // GATED `owned_streams_for_account` — so running it here would fail the whole fold, including
    // the pin install itself, and blame a version mismatch that is not the cause. Reaching for the
    // ungated `owned_stream_bytes` instead would work mechanically and weaken the gate.
    if unpinned {
        top_up_account_candidate_reservations_in_tx(tx, account_id, now_ms)?;
    }
    Ok(AccountStateFold { statuses, affected_streams, rejected_content_promotions })
}

/// The streams this account currently owns, per the projection — captured before a refold rewrites
/// it so a dropped `StreamOwn` still triggers its stream's content declassification.
pub fn owned_streams_for_account(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<StreamId>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    owned_stream_bytes(conn, account_id)
        .map(|streams| streams.into_iter().map(StreamId::from_bytes).collect())
}

fn owned_stream_bytes(conn: &Connection, account_id: AccountId) -> anyhow::Result<Vec<[u8; 32]>> {
    let mut stmt = conn.prepare(
        "SELECT stream_id FROM account_stream_ownership
          WHERE account_id = ?1 ORDER BY stream_id",
    )?;
    let rows = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.iter().map(|bytes| id::fixed(bytes)).collect()
}

/// Every stream a pinned account's retraction touches: the streams its content currently reaches,
/// plus the streams the pin's routing table still names once the ownership rows are suppressed.
/// Read BEFORE any retraction, because both inputs are things a retraction removes.
fn pinned_affected_streams(
    tx: &Transaction<'_>,
    account_id: AccountId,
) -> anyhow::Result<Vec<StreamId>> {
    let mut affected = content::affected_streams_for_account(tx, account_id, &[])?;
    let pinned_streams = {
        let mut stmt =
            tx.prepare("SELECT stream_id FROM account_control_pin_streams WHERE account_id=?1")?;
        stmt.query_map([account_id.to_bytes().as_slice()], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for bytes in pinned_streams {
        affected.push(StreamId::from_bytes(id::fixed(&bytes)?));
    }
    affected.sort_unstable();
    affected.dedup();
    Ok(affected)
}

/// Retract the content acceptance of a pinned account's streams and re-project them through the
/// CLEANUP path.
///
/// Content is retracted under EVERY pin, including one this binary executes and folds. The content
/// acceptance path resolves stream authority through gated reads — [`account_is_contested`] and the
/// ownership/access-mode lookups — and those refuse for any pin, so there is no evaluation to
/// project. A pinned fold therefore retracts here and reports NO affected streams, rather than
/// handing the ordinary finalize a stream whose gated chain would fail the fold.
fn retract_pinned_content_in_tx(tx: &Transaction<'_>, account_id: AccountId) -> anyhow::Result<()> {
    let affected = pinned_affected_streams(tx, account_id)?;
    let flipped = tx.execute(
        "UPDATE content_entries SET accepted=0 WHERE accepted=1 AND (author_account_id=?1 OR \
         stream_id IN (SELECT stream_id FROM account_control_pin_streams WHERE account_id=?1))",
        [account_id.to_bytes().as_slice()],
    )?;
    // Once nothing is accepted there is nothing left to retract, so only a fold that flipped
    // something pays for a re-projection.
    if flipped > 0 {
        for stream in affected {
            content::refold_and_project_for_cleanup_in_tx(tx, stream)?;
        }
    }
    Ok(())
}

/// Replace every query-ready authority fact for this account. The caller's IMMEDIATE refold txn
/// also owns accepted/status, so readers can never observe authority from a different fold round.
pub(super) fn clear_unsupported_authority_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
) -> anyhow::Result<()> {
    // Capture routes before removing ownership. Refold/reprojection is a declassification path:
    // it must run even though operational authority for this account is unsupported.
    let affected = pinned_affected_streams(tx, account_id)?;
    // What the retraction below still has to retract. Every fold of a pinned account comes
    // through here — install, and each later local ingest for the account — and a re-projection
    // is O(all content on the affected streams) plus a Lens-visible epoch bump; once nothing is
    // accepted there is nothing left to retract, so only a fold that flipped something re-projects.
    let mut flipped = tx.execute(
        "UPDATE content_entries SET accepted=0 WHERE accepted=1 AND (author_account_id=?1 OR \
         stream_id IN (SELECT stream_id FROM account_control_pin_streams WHERE account_id=?1))",
        [account_id.to_bytes().as_slice()],
    )?;
    flipped += tx
        .execute("UPDATE account_entries SET accepted=0 WHERE accepted=1 AND account_id=?1", [
            account_id.to_bytes().as_slice(),
        ])?;
    tx.execute(
        "UPDATE account_entry_status SET status='retained_unfolded', detail='unsupported_version' \
         WHERE entry_hash IN (SELECT entry_hash FROM account_entries WHERE account_id=?1)",
        [account_id.to_bytes().as_slice()],
    )?;
    let account = account_id.to_bytes();
    for table in [
        "account_roster_content_boundaries",
        "account_roster_history",
        "account_owner_incarnations",
        "account_stream_ownership",
        "account_stream_grants",
        "account_stream_grant_cuts",
        "account_auth_state",
        // The eighth is the SECRETS-log (log 1) projection that `refold_secrets_log` rebuilds, not
        // one of the seven `rewrite_authority_projection` owns — the pin short-circuits both
        // passes, so both projections have to go.
        "account_repo_incarnation_current",
    ] {
        let account_column = match table {
            "account_stream_grants" | "account_stream_grant_cuts" => "owner_account_id",
            _ => "account_id",
        };
        tx.execute(&format!("DELETE FROM {table} WHERE {account_column} = ?1"), [
            account.as_slice()
        ])?;
    }

    if flipped > 0 {
        for stream in affected {
            content::refold_and_project_for_cleanup_in_tx(tx, stream)?;
        }
    }
    Ok(())
}

fn rewrite_authority_projection(
    tx: &Transaction<'_>,
    account_id: AccountId,
    history: &fold::AccountAuthHistory,
    now_ms: i64,
) -> anyhow::Result<()> {
    let account = account_id.to_bytes();
    for table in [
        "account_roster_content_boundaries",
        "account_roster_history",
        "account_owner_incarnations",
        "account_stream_ownership",
        "account_stream_grants",
        "account_stream_grant_cuts",
        "account_auth_state",
    ] {
        let account_column = match table {
            "account_stream_grants" | "account_stream_grant_cuts" => "owner_account_id",
            _ => "account_id",
        };
        tx.execute(&format!("DELETE FROM {table} WHERE {account_column} = ?1"), [
            account.as_slice()
        ])?;
    }

    let (classification, contested_depth) = match history.classification() {
        fold::AccountClassification::Live => ("live", None),
        fold::AccountClassification::Contested { state_before_depth } =>
            ("contested", Some(i64::try_from(state_before_depth)?)),
    };
    let successor = history.contested_successor().map(AccountId::to_bytes);
    tx.execute(
        "INSERT INTO account_auth_state(
             account_id, classification, contested_depth, successor_account_id, effective_count
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            account.as_slice(),
            classification,
            contested_depth,
            successor.as_ref().map(<[u8; 32]>::as_slice),
            i64::try_from(history.effective_count())?,
        ],
    )?;

    for (roster_ref, fact) in history.roster_facts() {
        let (control_kind, control_seq, control_hash) = stored_boundary(fact.control_boundary);
        let (secrets_kind, secrets_seq, secrets_hash) = stored_boundary(fact.secrets_boundary);
        if let Some(closed_at) = fact.closed_at {
            enqueue_readoption_for_closed_fact(
                tx, account_id, roster_ref, fact, closed_at, now_ms,
            )?;
        }
        tx.execute(
            "INSERT INTO account_roster_history(
                 roster_ref, account_id, device_fingerprint, role, effective_at, closed_at,
                 control_boundary, control_seq, control_hash,
                 secrets_boundary, secrets_seq, secrets_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                roster_ref.as_slice(),
                account.as_slice(),
                fact.authority.device_fingerprint.to_bytes().as_slice(),
                fact.authority.current_role.as_db_str(),
                i64::try_from(fact.effective_at)?,
                fact.closed_at.map(i64::try_from).transpose()?,
                control_kind,
                control_seq.as_ref().map(<[u8; 8]>::as_slice),
                control_hash.as_ref().map(<[u8; 32]>::as_slice),
                secrets_kind,
                secrets_seq.as_ref().map(<[u8; 8]>::as_slice),
                secrets_hash.as_ref().map(<[u8; 32]>::as_slice),
            ],
        )?;
        for (stream_id, boundary) in &fact.content_boundaries {
            let fold::AuthorityBoundary::Cut { seq, hash } = boundary else {
                continue;
            };
            tx.execute(
                "INSERT INTO account_roster_content_boundaries(
                     roster_ref, account_id, stream_id, seq, entry_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    roster_ref.as_slice(),
                    account.as_slice(),
                    stream_id.to_bytes().as_slice(),
                    seq.to_be_bytes().as_slice(),
                    hash.as_slice(),
                ],
            )?;
        }
    }
    for (owner_id, fact) in history.owner_incarnation_facts() {
        let (control_kind, control_seq, control_hash) = stored_boundary(fact.control_boundary);
        let (secrets_kind, secrets_seq, secrets_hash) = stored_boundary(fact.secrets_boundary);
        tx.execute(
            "INSERT INTO account_owner_incarnations(
                 owner_id, account_id, device_fingerprint, effective_at, closed_at,
                 control_boundary, control_seq, control_hash,
                 secrets_boundary, secrets_seq, secrets_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                owner_id.as_slice(),
                account.as_slice(),
                fact.authority.device_fingerprint.to_bytes().as_slice(),
                i64::try_from(fact.effective_at)?,
                fact.closed_at.map(i64::try_from).transpose()?,
                control_kind,
                control_seq.as_ref().map(<[u8; 8]>::as_slice),
                control_hash.as_ref().map(<[u8; 32]>::as_slice),
                secrets_kind,
                secrets_seq.as_ref().map(<[u8; 8]>::as_slice),
                secrets_hash.as_ref().map(<[u8; 32]>::as_slice),
            ],
        )?;
    }
    for (stream_id, fact) in history.stream_ownership_facts() {
        tx.execute(
            "INSERT INTO account_stream_ownership(stream_id, account_id, own_id, effective_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                stream_id.to_bytes().as_slice(),
                account.as_slice(),
                fact.own_id.as_slice(),
                i64::try_from(fact.effective_at)?,
            ],
        )?;
    }
    for (grant_id, fact) in history.grant_facts() {
        tx.execute(
            "INSERT INTO account_stream_grants(
                 grant_id, owner_account_id, stream_id, grantee_account_id, role, effective_at,
                 closed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                grant_id.as_slice(),
                account.as_slice(),
                fact.authority.stream_id.to_bytes().as_slice(),
                fact.authority.grantee_account_id.to_bytes().as_slice(),
                fact.authority.role.as_db_str(),
                i64::try_from(fact.effective_at)?,
                fact.closed_at.map(i64::try_from).transpose()?,
            ],
        )?;
    }
    for (grant_id, cuts) in history.grant_cuts() {
        for cut in cuts {
            tx.execute(
                "INSERT INTO account_stream_grant_cuts(
                     grant_id, owner_account_id, device_fingerprint, seq, entry_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    grant_id.as_slice(),
                    account.as_slice(),
                    cut.device_fingerprint.to_bytes().as_slice(),
                    cut.seq.to_be_bytes().as_slice(),
                    cut.hash.as_slice(),
                ],
            )?;
        }
    }
    Ok(())
}

/// Enqueue table-sync re-adoption for one roster fact the fold just closed (#997).
///
/// This runs inside the authority projection rewrite, before the roster row itself is inserted,
/// because the fold is the one place that knows the removal is effective. One durable work item is
/// recorded per stream that currently has table-sync context for this account; a stream that
/// first learns of the removal before its directory row exists gets the work attached when the
/// first entry is authored or ingested. `roster_ref` is intentionally not a FK: this table rewrites
/// roster history wholesale on every fold, while the worklist must survive that rewrite.
fn enqueue_readoption_for_closed_fact(
    tx: &Transaction<'_>,
    account_id: AccountId,
    roster_ref: &RosterRef,
    fact: &fold::RosterFact,
    closed_at: u64,
    now_ms: i64,
) -> anyhow::Result<()> {
    let mut stmt = tx.prepare(
        "SELECT stream_id FROM table_sync_streams WHERE account_id = ?1 ORDER BY stream_id",
    )?;
    let streams = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if streams.is_empty() {
        // A stream whose first local contact happens after this removal still owes repair. Park
        // the removal under the pre-context placeholder until the first authored/ingested entry
        // records the real context.
        crate::table_sync::enqueue_readoption_work(
            tx,
            account_id,
            fact.authority.device_fingerprint,
            StreamId::PRECONTEXT,
            (*roster_ref).into(),
            closed_at,
            now_ms,
        )?;
        return Ok(());
    }
    for stream in streams {
        crate::table_sync::enqueue_readoption_work(
            tx,
            account_id,
            fact.authority.device_fingerprint,
            StreamId::from_bytes(id::fixed::<32>(&stream)?),
            (*roster_ref).into(),
            closed_at,
            now_ms,
        )?;
    }
    Ok(())
}

/// Derive an author-chain-coherent, authority-closed projection without touching storage.
pub(super) fn project_verified_checkpoint_evidence(
    entries: &[VerifiedAccountEntry],
) -> AccountProjection {
    let rows = entries
        .iter()
        .map(|entry| CandidateRow {
            entry_hash: entry.entry_hash,
            log_id: entry.header.log_id,
            device_fingerprint: entry.header.device_fingerprint,
            seq: entry.header.seq,
            verified: entry.clone(),
        })
        .collect::<Vec<_>>();
    derive_account_projection(&rows)
}

pub(super) fn project_checkpoint_with_trace(
    entries: &[VerifiedAccountEntry],
) -> (AccountProjection, Option<fold::LegacyTrace>) {
    let rows = entries
        .iter()
        .map(|entry| CandidateRow {
            entry_hash: entry.entry_hash,
            log_id: entry.header.log_id,
            device_fingerprint: entry.header.device_fingerprint,
            seq: entry.header.seq,
            verified: entry.clone(),
        })
        .collect::<Vec<_>>();
    derive_account_projection_traced(&rows, true)
}

/// Derive an author-chain-coherent, authority-closed projection without touching storage.
fn derive_account_projection(rows: &[CandidateRow]) -> AccountProjection {
    derive_account_projection_traced(rows, false).0
}

fn derive_account_projection_traced(
    rows: &[CandidateRow],
    capture: bool,
) -> (AccountProjection, Option<fold::LegacyTrace>) {
    let mut forked = HashSet::new();
    loop {
        let entries: Vec<VerifiedAccountEntry> = rows
            .iter()
            .filter(|row| !forked.contains(&row.entry_hash))
            .map(|row| row.verified.clone())
            .collect();
        let (history, trace) = fold::fold_account_traced(&entries, capture);
        let effective: HashSet<AccountEntryHash> = rows
            .iter()
            .filter(|row| {
                !forked.contains(&row.entry_hash)
                    && history
                        .outcome(&row.entry_hash)
                        .is_some_and(|outcome| outcome.is_effective())
            })
            .map(|row| row.entry_hash)
            .collect();
        let selected = select_coherent_branches(rows, &effective);
        let accepted = close_selection_over_authority(rows, selected);
        // `forked` here is effective-relative, not rooted-relative, and a stranded entry is
        // re-derived on the next read, so a late predecessor can heal it.
        let newly_forked: Vec<AccountEntryHash> =
            effective.difference(&accepted).copied().collect();
        if newly_forked.is_empty() {
            return (AccountProjection { history, accepted, forked }, trace);
        }
        // Monotone elimination is both the termination argument and the security boundary: once
        // an effective candidate loses its author branch or its cited authority branch, neither it
        // nor any effect it produced may participate in a later fold round.
        forked.extend(newly_forked);
    }
}

/// The stored signed bytes of every control-log row for this account, in a deterministic order.
/// Deliberately the WHOLE log: the executor decides which rows are v2 candidates for the pin, so a
/// row it refuses is left out of the pool rather than refusing the pool.
fn load_control_log_bytes(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut stmt = conn.prepare(
        "SELECT signed_bytes FROM account_entries
          WHERE account_id = ?1 AND log_id = ?2 ORDER BY entry_hash",
    )?;
    Ok(stmt
        .query_map(params![account_id.to_bytes().as_slice(), fold::CONTROL_LOG], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<Vec<u8>>>>()?)
}

/// The detached pre-cut manifests this store holds for the account: annex payloads at the
/// view-manifest tag, VERBATIM. A cut names its evidence by `sha256` of exactly those bytes.
///
/// Deliberately NOT filtered the way [`usable_snapshots`] filters a snapshot, and the divergence is
/// the point. A snapshot is a CLAIM its author asserts, so it is usable only while the owner
/// incarnation it cites is still open. A manifest asserts nothing — its integrity is the digest the
/// cut signed, which holds whoever carried the bytes. Gating it on its carrier's live authority
/// would make a revoked author's manifest vanish and RE-PARK a cut that had already applied,
/// un-revoking a device with the very revocation that removed its author.
fn held_view_manifests(rows: &[CandidateRow]) -> Vec<Vec<u8>> {
    rows.iter()
        .filter(|row| is_view_manifest(&row.verified.header))
        .map(|row| row.verified.payload.clone())
        .collect()
}

/// The projection of an account under a control pin THIS binary executes.
///
/// The checkpoint fixes the legacy epoch, so the legacy fold is never RE-DERIVED here: the
/// certificate commits to exactly that history, so re-folding the same evidence could only
/// reproduce it or disagree with the pin. What the frozen history does not carry is the effect of
/// the account's own v2 operations, which [`fold::v2::pinned_history`] composes on top of it — so a
/// device a v2 cut revoked leaves the roster projection and its entries stop being accepted. Two
/// rules do the rest.
///
/// **A control-log v1 entry participates IFF the checkpoint's evidence carries it**
/// ([`fold::v2::FrozenLegacy::entries`] is exactly that set). Account ingest is not pin-gated, so
/// v1 rows keep arriving after the pin. Letting one into the effective set hands it to a selection
/// walk that re-derives acceptance from seq 0 over the LIVE candidates: a post-pin sibling with a
/// smaller entry hash wins the min-hash tiebreak, demotes the checkpoint's own winner to forked and
/// collapses that device's accepted chain — while the executor keeps admitting v2 continuations on
/// the strength of `accepted_at_checkpoint`. That revives a frozen branch loser out of ordinary v1
/// traffic, which is the one thing the pin exists to prevent. With the rule, selection reproduces
/// the checkpoint's accepted set exactly, and `select_coherent_branches` needs no change.
///
/// **A v2 entry enters the effective set only when the executor APPLIED it**, and never at a slot
/// the checkpoint already decided. Selection promotes nothing: the ordinary coherence walk picks
/// one sibling per `(log_id, device, seq)` by entry hash, and monotone elimination drops the loser
/// — so v2 equivocation resolves through the very machinery v1 equivocation does.
fn derive_pinned_projection(
    rows: &[CandidateRow],
    checkpoint: &super::checkpoint::VerifiedCheckpoint,
    control_log: &[Vec<u8>],
) -> AccountProjection {
    let frozen = checkpoint.frozen_legacy();
    let accepted_at_checkpoint: HashSet<AccountEntryHash> = frozen.accepted_entries().collect();
    // The slots the checkpoint already decided. A v2 entry may EXTEND a device's accepted chain but
    // never contest a slot at or below its tip: the min-hash tiebreak is symmetric, so without this
    // an authorized v2 sibling with a smaller entry hash would displace a checkpoint-accepted entry
    // — selecting a different historical branch, which the pin forbids however the entry was
    // authorized.
    let frozen_slots: HashSet<(u8, DeviceFingerprint, u64)> = frozen
        .entries()
        .iter()
        .filter(|entry| accepted_at_checkpoint.contains(&entry.entry_hash))
        .map(|entry| (entry.header.log_id, entry.header.device_fingerprint, entry.header.seq))
        .collect();
    let verdicts = super::control_v2::executor::execute_held(
        checkpoint,
        control_log,
        &held_view_manifests(rows),
    );
    // An entry at a slot the checkpoint already decided contributes NOTHING — not its acceptance
    // and not its registers. Being authorized is not permission to rewrite what the pin froze.
    let contests_frozen_slot: HashSet<AccountEntryHash> = rows
        .iter()
        .filter(|row| frozen_slots.contains(&(row.log_id, row.device_fingerprint, row.seq)))
        .map(|row| row.entry_hash)
        .collect();

    let mut forked: HashSet<AccountEntryHash> = HashSet::new();
    loop {
        // A forked entry lost its slot, so its registers must revoke nothing either — which is why
        // the history is composed inside the loop rather than once above it.
        let applied: Vec<fold::v2::AppliedOperation<'_>> = verdicts
            .iter()
            .filter(|(hash, _)| !forked.contains(hash) && !contests_frozen_slot.contains(hash))
            .filter_map(|(_, verdict)| match verdict {
                super::control_v2::executor::Verdict::Applied { entry, registers, .. } =>
                    Some(fold::v2::AppliedOperation { entry, registers }),
                _ => None,
            })
            .collect();
        let history = fold::v2::pinned_history(frozen, &applied);
        // The checkpoint's accepted set plus the applied entries, MINUS whatever the composed
        // history explicitly condemns. An entry the composition does not model at all — every
        // applied operation that installs no register — keeps its acceptance; acceptance is a
        // question about branch selection, and only a v2 REGISTER takes it away.
        let still_effective =
            |hash: &AccountEntryHash| history.outcome(hash).is_none_or(|o| o.is_effective());
        let mut effective: HashSet<AccountEntryHash> = accepted_at_checkpoint
            .iter()
            .filter(|hash| !forked.contains(*hash) && still_effective(hash))
            .copied()
            .collect();
        effective.extend(applied.iter().map(|op| op.entry.hash()).filter(still_effective));
        let selected = select_coherent_branches(rows, &effective);
        let accepted = close_selection_over_authority(rows, selected);
        let newly_forked: Vec<AccountEntryHash> =
            effective.difference(&accepted).copied().collect();
        if newly_forked.is_empty() {
            return AccountProjection { history, accepted, forked };
        }
        forked.extend(newly_forked);
    }
}

/// Select one contiguous effective hash-chain per `(log_id, device)` (§16.2).
/// Unlike content/secrets selection in [`super::branch`], this consumes the post-fold effective
/// set: registers have already condemned off-branch authority, so no watermark pins are needed.
/// Content/secrets must preserve the watermark's branch against smaller-hash forks below it.
fn select_coherent_branches(
    rows: &[CandidateRow],
    effective: &HashSet<AccountEntryHash>,
) -> HashSet<AccountEntryHash> {
    // Effective entries indexed by the (log, device, prev_hash) parent slot they chain from; a
    // chain root keys on `None`.
    let mut children = BranchChildren::new();
    let mut groups: HashSet<(u8, DeviceFingerprint)> = HashSet::new();
    for row in rows {
        if effective.contains(&row.entry_hash) {
            children
                .entry((row.log_id, row.device_fingerprint, row.verified.header.prev_hash))
                .or_default()
                .push((row.seq, row.entry_hash));
            groups.insert((row.log_id, row.device_fingerprint));
        }
    }
    let mut accepted_set = HashSet::new();
    for (log_id, device) in groups {
        let mut parent: Option<AccountEntryHash> = None;
        // Bounded by the candidate count; only the exact next sequence slot may extend a branch.
        for expected_seq in 0..=rows.len() {
            let Some((_, winner)) = children
                .get(&(log_id, device, parent))
                .and_then(|kids| {
                    kids.iter()
                        .filter(|(seq, _)| *seq == expected_seq as u64)
                        .min_by_key(|(_, hash)| hash)
                })
                .copied()
            else {
                break;
            };
            accepted_set.insert(winner);
            parent = Some(winner);
        }
    }
    accepted_set
}

/// Remove selected entries whose cited incarnation is not itself selected. Iterate because an
/// invalid mint can authorize another mint, so authority loss must propagate through the whole
/// incarnation DAG rather than only one edge.
fn close_selection_over_authority(
    rows: &[CandidateRow],
    mut selected: HashSet<AccountEntryHash>,
) -> HashSet<AccountEntryHash> {
    loop {
        let invalid: Vec<AccountEntryHash> = rows
            .iter()
            .filter(|row| selected.contains(&row.entry_hash))
            .filter(|row| {
                row.verified
                    .header
                    .authority_ref
                    .is_some_and(|authority| !selected.contains(&authority.into()))
            })
            .map(|row| row.entry_hash)
            .collect();
        if invalid.is_empty() {
            return selected;
        }
        for hash in invalid {
            selected.remove(&hash);
        }
    }
}

/// A stored candidate row, with its verified entry reconstituted from the trusted stored bytes (the
/// signature was checked at ingest; the local DB is the trust boundary, so we re-decode STRUCTURE
/// only rather than re-verify — which would need the device set we are loading).
struct CandidateRow {
    entry_hash: AccountEntryHash,
    log_id: u8,
    device_fingerprint: DeviceFingerprint,
    seq: u64,
    verified: VerifiedAccountEntry,
}

/// One read of an account's candidate DAG, carrying BOTH facts a snapshot author needs: everything
/// held, and which of it the store's own branch selection accepted.
///
/// The fields are private on purpose. These two sets differ exactly when a device has equivocated,
/// and that is precisely the case where confusing them is harmful: a coverage claim built from
/// "everything held" can name the LOSING side of a same-sequence fork (highest seq wins, and both
/// forks share a seq), producing a snapshot that is internally consistent yet describes a branch
/// this store rejected. So the head computation is a method here rather than a free function over a
/// slice a caller supplies — there is no wrong set to hand it.
pub(in crate::account) struct AccountEntriesView {
    held: Vec<VerifiedAccountEntry>,
    accepted: HashSet<AccountEntryHash>,
}

impl AccountEntriesView {
    /// Everything held, accepted or not. Classification must fold THIS: a verifier folds the full
    /// held set before trusting a snapshot, so an author that folded only its accepted branch would
    /// call an account live that its peers can see is contested.
    pub(in crate::account) fn held(&self) -> &[VerifiedAccountEntry] {
        &self.held
    }

    /// Each device's ACCEPTED control-chain head — what a coverage claim names.
    ///
    /// Accepted rather than merely held: the accepted branch is the coherent one, and a watermark
    /// pointing at a forked head would name a branch the receiving verifier cannot reconcile with
    /// its own view of that device's chain.
    pub(in crate::account) fn accepted_control_heads(&self) -> Vec<annex::ops::CoveredWatermark> {
        let mut heads: HashMap<DeviceFingerprint, (u64, AccountEntryHash)> = HashMap::new();
        for entry in &self.held {
            if entry.header.log_id != fold::CONTROL_LOG
                || !self.accepted.contains(&entry.entry_hash)
            {
                continue;
            }
            let slot = heads
                .entry(entry.header.device_fingerprint)
                .or_insert((entry.header.seq, entry.entry_hash));
            if entry.header.seq >= slot.0 {
                *slot = (entry.header.seq, entry.entry_hash);
            }
        }
        let mut covered: Vec<annex::ops::CoveredWatermark> = heads
            .into_iter()
            .map(|(device_fingerprint, (seq, entry_hash))| annex::ops::CoveredWatermark {
                device_fingerprint,
                seq,
                entry_hash,
            })
            .collect();
        covered.sort_unstable_by_key(|w| w.device_fingerprint.to_bytes());
        covered
    }
}

/// Read the account's candidate DAG once and derive its accepted branch with the SAME selection the
/// store persists — [`derive_account_projection`], never a second implementation that could drift.
pub(in crate::account) fn account_entries_view(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<AccountEntriesView> {
    let rows = load_candidates(conn, account_id)?;
    let accepted = derive_account_projection(&rows).accepted;
    Ok(AccountEntriesView { held: rows.into_iter().map(|row| row.verified).collect(), accepted })
}

fn load_candidates(conn: &Connection, account_id: AccountId) -> anyhow::Result<Vec<CandidateRow>> {
    let mut stmt = conn.prepare(
        "SELECT entry_hash, device_fingerprint, seq, signed_bytes
         FROM account_entries WHERE account_id = ?1
         ORDER BY entry_hash", // deterministic load order (the fold is order-free regardless)
    )?;
    let rows = stmt
        .query_map(params![account_id.to_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (hash, fp, seq, signed_bytes) in rows {
        let signed = envelope::decode_account_signed(&signed_bytes)
            .map_err(|err| anyhow::anyhow!("stored candidate re-decode failed: {err}"))?;
        out.push(CandidateRow {
            entry_hash: AccountEntryHash::from_bytes(id::fixed(&hash)?),
            log_id: signed.header.log_id,
            device_fingerprint: DeviceFingerprint::from_bytes(id::fixed(&fp)?),
            seq: seq as u64,
            verified: VerifiedAccountEntry {
                header: signed.header,
                payload: signed.payload,
                entry_hash: signed.entry_hash,
            },
        });
    }
    Ok(out)
}

/// One account-log entry as it goes on the wire (phase D, #406): its chain coordinates plus the
/// exact stored `signed_bytes`. The bytes are opaque here — a peer re-runs [`account_ingest`] over
/// them, which re-verifies the signature and canonicity from scratch, so the sender is never
/// trusted. Every HELD entry is offered, accepted or not: the fold is grow-only (I8) and
/// order-free, so a peer must see equivocation branches too to reach the same accepted set.
#[derive(Debug, Clone)]
pub struct SyncAccountEntry {
    pub device_fingerprint: DeviceFingerprint,
    pub log_id: u8,
    pub seq: u64,
    pub entry_hash: AccountEntryHash,
    pub signed_bytes: Vec<u8>,
}

/// Peek the `(account_id, entry_hash)` a signed account entry claims, WITHOUT ingesting it. The
/// sync layer uses this to refuse an entry for a different account before it reaches
/// [`account_ingest`] (an account-scoped session must not let a peer inject entries for other
/// accounts), and to skip re-offering one it already holds. Structure only — a full verify still
/// happens in `account_ingest`; a decode failure here just means the bytes are not a well-formed
/// account entry, which the session treats as a peer to distrust.
pub fn account_entry_ref(signed_bytes: &[u8]) -> anyhow::Result<(AccountId, AccountEntryHash)> {
    let signed = envelope::decode_account_signed(signed_bytes)?;
    Ok((signed.header.account_id, signed.entry_hash))
}

/// Verify that an enrollment bootstrap contains a founder-signed `DeviceAdd` for the exact
/// account, entry hash, and joiner keys the caller requested. This is deliberately independent of
/// transport identity: the inviter's QUIC key routes the exchange, while the account founder key
/// carried by the self-certifying genesis authorizes enrollment.
pub fn verify_enrollment_device_add(
    account_entries: &[Vec<u8>],
    expected_account: AccountId,
    expected_hash: AccountEntryHash,
    expected_signed: &[u8],
    expected_ed25519_pubkey: [u8; 32],
    expected_x25519_pubkey: [u8; 32],
) -> anyhow::Result<AccountEntryHash> {
    let mut entries = Vec::with_capacity(account_entries.len());
    let mut device_add = None;
    for bytes in account_entries {
        let signed = envelope::decode_account_signed(bytes)?;
        anyhow::ensure!(
            signed.header.account_id == expected_account,
            "enrollment bootstrap contains an entry for another account"
        );
        if signed.entry_hash == expected_hash {
            anyhow::ensure!(
                bytes.as_slice() == expected_signed,
                "bootstrap DeviceAdd bytes differ from the receipt"
            );
            anyhow::ensure!(
                device_add.replace(signed.clone()).is_none(),
                "duplicate enrollment DeviceAdd entry"
            );
        }
        entries.push(signed);
    }
    // Select the same canonical, current-version root the account fold accepts. A parked opaque
    // future-version row may preserve genesis-looking header coordinates, but it is not an
    // authoritative root and must not poison an otherwise valid durable receipt.
    let candidates = entries
        .iter()
        .map(|signed| VerifiedAccountEntry {
            header: signed.header.clone(),
            payload: signed.payload.clone(),
            entry_hash: signed.entry_hash,
        })
        .collect::<Vec<_>>();
    let genesis_hash = fold::fold_account(&candidates)
        .genesis_hash()
        .ok_or_else(|| anyhow::anyhow!("enrollment bootstrap has no accepted account genesis"))?;
    let genesis = entries
        .into_iter()
        .find(|signed| signed.entry_hash == genesis_hash)
        .expect("the fold's genesis hash names an entry in its input");
    // The per-entry check above compares ATTACKER-CONTROLLED header bytes; bind the genesis to
    // the expected account the way ingest does (§4): its payload must self-hash to
    // `expected_account`, or an impostor's own founder-signed genesis + DeviceAdd (stamped with
    // the victim's account_id in their headers) would pass every signature check below.
    anyhow::ensure!(
        id::account_id_from_genesis_payload(&genesis.payload) == expected_account,
        "enrollment genesis payload does not hash to the expected account"
    );
    let DecodedAccountOp::Known(AccountOp::AccountGenesis { ed25519_pubkey: founder_key, .. }) =
        ops::decode(genesis.header.entry_type, &genesis.payload)?
    else {
        anyhow::bail!("enrollment bootstrap genesis payload is not AccountGenesis");
    };
    authenticate_entry(&genesis.signed_bytes, &founder_key).map_err(anyhow::Error::msg)?;

    let device_add = device_add
        .ok_or_else(|| anyhow::anyhow!("enrollment bootstrap has no acknowledged DeviceAdd"))?;
    anyhow::ensure!(
        device_add.header.authority_ref == Some(genesis.entry_hash.into()),
        "enrollment DeviceAdd does not cite the founder incarnation"
    );
    let verified =
        authenticate_entry(&device_add.signed_bytes, &founder_key).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        verified.entry_hash == expected_hash,
        "enrollment DeviceAdd hash does not match the receipt"
    );
    let DecodedAccountOp::Known(AccountOp::DeviceAdd { ed25519_pubkey, x25519_pubkey, .. }) =
        ops::decode(verified.header.entry_type, &verified.payload)?
    else {
        anyhow::bail!("acknowledged enrollment entry is not a DeviceAdd");
    };
    anyhow::ensure!(
        ed25519_pubkey == expected_ed25519_pubkey,
        "enrollment DeviceAdd names a different ed25519 key"
    );
    anyhow::ensure!(
        x25519_pubkey == expected_x25519_pubkey,
        "enrollment DeviceAdd names a different x25519 key"
    );
    Ok(genesis.entry_hash)
}

/// The wire dedup key for a signed account entry: `sha256(signed_bytes)`, the SAME hash
/// `account_pre_verify` keys its rows by. This distinguishes competing SIGNATURES of one entry —
/// two envelopes can share an `entry_hash` (same body) yet differ in signature, and the sync layer
/// must treat them as distinct, or a peer holding the valid signature would suppress it against a
/// peer holding only an invalid one. Never diff sync inventory by `entry_hash`.
pub fn account_signed_hash(signed_bytes: &[u8]) -> SignedHash {
    SignedHash::from_bytes(cbor::sha256(signed_bytes))
}

/// Whether `account_id` already holds this EXACT signed envelope — as a stored candidate (matched
/// by its bytes) OR a durably parked pre-verify row (matched by `signed_hash`). Signed-envelope
/// precise, not `entry_hash` precise: skipping a distinct signature that happens to share an
/// entry_hash would drop a valid variant on the floor. Used so the sync layer reports real transfer
/// versus redelivery.
pub fn account_signed_entry_exists(
    conn: &Connection,
    account_id: AccountId,
    signed_bytes: &[u8],
) -> anyhow::Result<bool> {
    let signed_hash = cbor::sha256(signed_bytes);
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM account_entries WHERE account_id = ?1 AND signed_bytes = ?2
             UNION ALL
             SELECT 1 FROM account_pre_verify WHERE claimed_account_id = ?1 AND signed_hash = ?3
             LIMIT 1",
            params![account_id.to_bytes().as_slice(), signed_bytes, signed_hash.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Every authenticated account candidate for an enrollment bootstrap. Unlike normal sync, this
/// excludes unauthenticated pre-verify rows: enrollment is an authority bootstrap, not a vehicle
/// for speculative queue work.
pub fn account_entries_for_enrollment(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<SyncAccountEntry>> {
    let mut out = Vec::new();
    let mut held = conn.prepare(
        "SELECT entry_hash, device_fingerprint, seq, log_id, signed_bytes
         FROM account_entries WHERE account_id = ?1
         ORDER BY log_id, seq, entry_hash",
    )?;
    let held_rows = held
        .query_map(params![account_id.to_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (hash, fp, seq, log_id, signed_bytes) in held_rows {
        out.push(SyncAccountEntry {
            device_fingerprint: DeviceFingerprint::from_bytes(id::fixed(&fp)?),
            log_id: u8::try_from(log_id)
                .map_err(|_| anyhow::anyhow!("stored log_id {log_id} out of range"))?,
            seq: seq as u64,
            entry_hash: AccountEntryHash::from_bytes(id::fixed(&hash)?),
            signed_bytes,
        });
    }
    Ok(out)
}

/// Every account-log entry for `account_id` that a peer may need — the held entries AND the ones
/// durably PARKED in `account_pre_verify` awaiting their signer's introduction.
///
/// Parked entries must be offered too: a valid entry whose signing device is not yet known here is
/// still real, and a peer that holds the authorizing `DeviceAdd` can promote it. Omitting them
/// would let a session complete with one peer holding a dependent entry the other never receives —
/// a silent divergence.
///
/// Held entries come first, ordered `(log_id, seq, entry_hash)` — a causal-leaning order (the
/// account genesis at seq 0, then the `DeviceAdd`s that introduce signers, then later ops) so a
/// cooperative receiver folds authorizers before dependents and avoids parking-then-eviction. It is
/// NOT a full topological guarantee against an adversarial sender (that needs a reconciliation pass
/// at the caller); it makes the honest restore converge in one session. Parked entries follow,
/// since they depend on authorizers in the held set.
pub fn account_entries_for_sync(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<SyncAccountEntry>> {
    let mut out = account_entries_for_enrollment(conn, account_id)?;

    // Parked rows carry the raw signed bytes but not decoded coordinates; decode the header for the
    // log/seq the wire records (informational — the session diffs on entry_hash). A parked row that
    // no longer decodes is skipped rather than failing the whole read.
    let mut parked = conn.prepare(
        "SELECT entry_hash, claimed_fingerprint, raw_bytes
         FROM account_pre_verify WHERE claimed_account_id = ?1
         ORDER BY entry_hash",
    )?;
    let parked_rows = parked
        .query_map(params![account_id.to_bytes().as_slice()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (hash, fp, signed_bytes) in parked_rows {
        let (log_id, seq) = match envelope::decode_account_signed(&signed_bytes) {
            Ok(signed) => (signed.header.log_id, signed.header.seq),
            Err(_) => continue,
        };
        out.push(SyncAccountEntry {
            device_fingerprint: DeviceFingerprint::from_bytes(id::fixed(&fp)?),
            log_id,
            seq,
            entry_hash: AccountEntryHash::from_bytes(id::fixed(&hash)?),
            signed_bytes,
        });
    }

    Ok(out)
}

/// Insert one verified entry into the candidate DAG under the caller's txn, enforcing the
/// operational admission budgets. `pub(super)` so [`super::bootstrap`] can store its local-account
/// genesis through the same seam the ingest path uses.
pub(super) fn insert_candidate(
    tx: &Transaction<'_>,
    verified: &VerifiedAccountEntry,
    signed_bytes: &[u8],
    now_ms: i64,
) -> rusqlite::Result<CandidateInsert> {
    let h = &verified.header;
    let already_present = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM account_entries WHERE entry_hash = ?1)",
        params![verified.entry_hash.as_slice()],
        |row| row.get::<_, bool>(0),
    )?;
    if already_present {
        return Ok(CandidateInsert::AlreadyPresent);
    }
    // A view manifest reaches the whole per-account budget; everything else stops at the floor that
    // keeps a cut's evidence admissible (see [`VIEW_MANIFEST_FLOOR_ENTRIES`]).
    let (account_entries_max, account_bytes_max) = if is_view_manifest(h) {
        (CANDIDATES_PER_ACCOUNT_MAX, CANDIDATE_BYTES_PER_ACCOUNT_MAX)
    } else {
        (ORDINARY_CANDIDATES_PER_ACCOUNT_MAX, ORDINARY_CANDIDATE_BYTES_PER_ACCOUNT_MAX)
    };
    // Outstanding enrollment invites reserve their mandatory DeviceAdd + wraps in these SAME
    // counters until they are consumed or expire (#945): candidate capacity is grow-only, so
    // without charging reservations here, ordinary ingest or a second invite could consume
    // headroom an already-minted ticket was measured against and strand it permanently.
    let candidate_count: i64 = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM account_entries WHERE account_id = ?1)
              + (SELECT COALESCE(SUM(reserved_entries), 0)
                   FROM account_candidate_reservations
                  WHERE account_id = ?1 AND expires_at_ms > ?2)",
        params![h.account_id.to_bytes().as_slice(), now_ms],
        |row| row.get(0),
    )?;
    if candidate_count >= account_entries_max as i64 {
        return Ok(CandidateInsert::AtCapacity(CapacityScope::CandidateAccount));
    }
    let candidate_bytes: i64 = tx.query_row(
        "SELECT (SELECT COALESCE(SUM(length(signed_bytes)), 0)
                   FROM account_entries WHERE account_id = ?1)
              + (SELECT COALESCE(SUM(reserved_bytes), 0)
                   FROM account_candidate_reservations
                  WHERE account_id = ?1 AND expires_at_ms > ?2)",
        params![h.account_id.to_bytes().as_slice(), now_ms],
        |row| row.get(0),
    )?;
    if candidate_bytes.saturating_add(signed_bytes.len() as i64) > account_bytes_max as i64 {
        return Ok(CandidateInsert::AtCapacity(CapacityScope::CandidateAccountBytes));
    }
    let global_candidate_count: i64 = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM account_entries)
              + (SELECT COALESCE(SUM(reserved_entries), 0)
                   FROM account_candidate_reservations
                  WHERE expires_at_ms > ?1)",
        [now_ms],
        |row| row.get(0),
    )?;
    if global_candidate_count >= CANDIDATES_GLOBAL_MAX as i64 {
        return Ok(CandidateInsert::AtCapacity(CapacityScope::CandidateGlobal));
    }
    let global_candidate_bytes: i64 = tx.query_row(
        "SELECT (SELECT COALESCE(SUM(length(signed_bytes)), 0) FROM account_entries)
              + (SELECT COALESCE(SUM(reserved_bytes), 0)
                   FROM account_candidate_reservations
                  WHERE expires_at_ms > ?1)",
        [now_ms],
        |row| row.get(0),
    )?;
    if global_candidate_bytes.saturating_add(signed_bytes.len() as i64)
        > CANDIDATE_BYTES_GLOBAL_MAX as i64
    {
        return Ok(CandidateInsert::AtCapacity(CapacityScope::CandidateGlobalBytes));
    }
    // INSERT OR IGNORE on the entry_hash PK: idempotent, and the candidate table has NO
    // seq-uniqueness — an equivocation head at an already-occupied slot is a first-class candidate.
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO account_entries(
             entry_hash, account_id, log_id, device_fingerprint, seq, prev_hash, parent_ref,
             authority_ref, entry_type, accepted, signed_bytes, received_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11)",
        params![
            verified.entry_hash.as_slice(),
            h.account_id.to_bytes().as_slice(),
            h.log_id,
            h.device_fingerprint.to_bytes().as_slice(),
            i64::try_from(h.seq).expect("seq range validated before candidate insert"),
            h.prev_hash.map(|p| p.as_slice().to_vec()),
            h.parent_ref.map(|p| p.as_slice().to_vec()),
            h.authority_ref.map(|p| p.as_slice().to_vec()),
            h.entry_type,
            signed_bytes,
            now_ms,
        ],
    )?;
    Ok(if inserted == 1 { CandidateInsert::Inserted } else { CandidateInsert::AlreadyPresent })
}

/// The grow-only candidate-admission budget still available to `account_id` and to the store
/// globally — the exact counters [`insert_candidate`] enforces, exposed so an authoring PREFLIGHT
/// (enrollment invite minting, #945) can refuse a write that redemption could never recover:
/// candidate capacity never drains on its own.
pub(crate) struct CandidateCapacityHeadroom {
    pub(crate) account_entries_remaining: i64,
    pub(crate) account_bytes_remaining: i64,
    pub(crate) global_entries_remaining: i64,
    pub(crate) global_bytes_remaining: i64,
}

pub(crate) fn candidate_capacity_headroom(
    conn: &Connection,
    account_id: AccountId,
    now_ms: i64,
) -> rusqlite::Result<CandidateCapacityHeadroom> {
    let account_count: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM account_entries WHERE account_id = ?1)
              + (SELECT COALESCE(SUM(reserved_entries), 0)
                   FROM account_candidate_reservations
                  WHERE account_id = ?1 AND expires_at_ms > ?2)",
        params![account_id.to_bytes().as_slice(), now_ms],
        |row| row.get(0),
    )?;
    let account_bytes: i64 = conn.query_row(
        "SELECT (SELECT COALESCE(SUM(length(signed_bytes)), 0)
                   FROM account_entries WHERE account_id = ?1)
              + (SELECT COALESCE(SUM(reserved_bytes), 0)
                   FROM account_candidate_reservations
                  WHERE account_id = ?1 AND expires_at_ms > ?2)",
        params![account_id.to_bytes().as_slice(), now_ms],
        |row| row.get(0),
    )?;
    let global_count: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM account_entries)
              + (SELECT COALESCE(SUM(reserved_entries), 0)
                   FROM account_candidate_reservations
                  WHERE expires_at_ms > ?1)",
        [now_ms],
        |row| row.get(0),
    )?;
    let global_bytes: i64 = conn.query_row(
        "SELECT (SELECT COALESCE(SUM(length(signed_bytes)), 0) FROM account_entries)
              + (SELECT COALESCE(SUM(reserved_bytes), 0)
                   FROM account_candidate_reservations
                  WHERE expires_at_ms > ?1)",
        [now_ms],
        |row| row.get(0),
    )?;
    Ok(CandidateCapacityHeadroom {
        // The ORDINARY caps: an enrollment receipt is ordinary traffic, so a preflight measured
        // against the whole budget would promise a ticket the admission path then refuses.
        account_entries_remaining: (ORDINARY_CANDIDATES_PER_ACCOUNT_MAX as i64) - account_count,
        account_bytes_remaining: (ORDINARY_CANDIDATE_BYTES_PER_ACCOUNT_MAX as i64) - account_bytes,
        global_entries_remaining: (CANDIDATES_GLOBAL_MAX as i64) - global_count,
        global_bytes_remaining: (CANDIDATE_BYTES_GLOBAL_MAX as i64) - global_bytes,
    })
}

/// Top outstanding enrollment invite reservations up to the CURRENT live key-target count after
/// a fold (#945). Every mandatory redemption wraps every live target to its joiner, so when a
/// fold grows the target set — a local key mint, or a REMOTELY synced `StreamOwn`/wrap the local
/// authoring hooks never see — each outstanding invite's reservation must grow by the same delta,
/// or ordinary candidate writes could consume headroom a minted ticket needs and strand it.
/// Called from both refold wrappers; costs one indexed EXISTS in the common no-invites case.
/// A bump that would exceed the grow-only caps fails the fold that caused the growth instead of
/// silently stranding the ticket.
pub(super) fn top_up_account_candidate_reservations_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<()> {
    // Migration replays (e.g. V064's authority backfill) refold before V090 exists.
    let table_exists: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
              WHERE type = 'table' AND name = 'account_candidate_reservations')",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(());
    }
    let any_outstanding: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_candidate_reservations
              WHERE account_id = ?1 AND expires_at_ms > ?2)",
        params![account_id.to_bytes().as_slice(), now_ms],
        |row| row.get(0),
    )?;
    if !any_outstanding {
        return Ok(());
    }
    let streams = owned_streams_for_account(tx, account_id)?;
    let targets =
        super::secrets::recoverable_live_stream_key_target_count(tx, account_id, &streams)?;
    let targets = i64::try_from(targets)?;
    let wrap_bytes = i64::try_from(super::secrets::single_recipient_wrap_envelope_bytes())?;
    // Bidirectional: a fold that SHRINKS the live target set (say, a competing control branch
    // makes a previously owned stream ineffective) reclaims the obsolete reservation in the same
    // update, so a long-lived invite near a cap cannot keep capacity it no longer needs.
    let grew: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_candidate_reservations
              WHERE account_id = ?1 AND expires_at_ms > ?2 AND reserved_targets < ?3)",
        params![account_id.to_bytes().as_slice(), now_ms, targets],
        |row| row.get(0),
    )?;
    tx.execute(
        "UPDATE account_candidate_reservations
            SET reserved_entries = reserved_entries + (?3 - reserved_targets),
                reserved_bytes   = reserved_bytes + (?3 - reserved_targets) * ?4,
                reserved_targets = ?3
          WHERE account_id = ?1 AND expires_at_ms > ?2 AND reserved_targets != ?3",
        params![account_id.to_bytes().as_slice(), now_ms, targets, wrap_bytes],
    )?;
    if !grew {
        return Ok(());
    }
    let headroom = candidate_capacity_headroom(tx, account_id, now_ms)?;
    anyhow::ensure!(
        headroom.account_entries_remaining >= 0
            && headroom.account_bytes_remaining >= 0
            && headroom.global_entries_remaining >= 0
            && headroom.global_bytes_remaining >= 0,
        "the new mandatory enrollment key targets do not fit the candidate store alongside the \
         outstanding invite reservations",
    );
    Ok(())
}

/// Refresh outstanding invite reservations after a `/3` content stream's acceptance settles
/// (#945). Accepted suite-1 content pins its sealing key as a live enrollment catch-up target
/// (`live_stream_key_epochs` reads `content_entries.accepted`), so content finalization can grow
/// the mandatory redemption cost without any account fold. Derives the stream's owner account
/// (if any) and runs the same bidirectional top-up the account fold uses.
pub(in crate::account) fn refresh_enrollment_reservations_for_stream_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<()> {
    // Content refolds can replay during migrations that predate the authority/reservation tables.
    let tables_ready: bool = tx.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
              WHERE type = 'table' AND name = 'account_stream_ownership')
           AND EXISTS(
             SELECT 1 FROM sqlite_master
              WHERE type = 'table' AND name = 'account_candidate_reservations')",
        [],
        |row| row.get(0),
    )?;
    if !tables_ready {
        return Ok(());
    }
    let owner: Option<Vec<u8>> = tx
        .query_row(
            "SELECT account_id FROM account_stream_ownership WHERE stream_id = ?1 LIMIT 1",
            [stream_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(owner) = owner else { return Ok(()) };
    let account_id = AccountId::from_bytes(id::fixed(&owner)?);
    top_up_account_candidate_reservations_in_tx(tx, account_id, now_ms)
}

/// The `(fingerprint → ed25519_pubkey)` map from every stored genesis / DeviceAdd for the account —
/// the only ops that carry a device's key. Fold-status-independent (§16.2): a key resolves from ANY
/// stored candidate carrying it, whether or not it is currently accepted.
pub(crate) fn stored_device_pubkeys(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<HashMap<DeviceFingerprint, [u8; 32]>> {
    // Gated on `log_id == CONTROL_LOG` (S3): only control-log genesis / DeviceAdd carry device
    // keys. A secrets-log tag reusing the 0/1 numbers must not be decoded here as a key
    // certificate.
    let mut stmt = conn.prepare(
        "SELECT signed_bytes FROM account_entries
         WHERE account_id = ?1 AND log_id = ?2 AND entry_type IN (?3, ?4)",
    )?;
    let rows = stmt
        .query_map(
            params![
                account_id.to_bytes().as_slice(),
                fold::CONTROL_LOG,
                ops::entry_type::ACCOUNT_GENESIS,
                ops::entry_type::DEVICE_ADD,
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = HashMap::new();
    for signed_bytes in rows {
        if let Ok(signed) = envelope::decode_account_signed(&signed_bytes) {
            add_self_pubkey(&mut out, &signed.header, &signed.payload);
        }
    }
    Ok(out)
}

/// Whether the entry certifies its OWN signer key with no prior state — the genesis arm of
/// [`add_self_pubkey`]. The enrollment bootstrap's causal ingest order treats exactly these as
/// worklist roots (a DeviceAdd certifies the ADDED key, never its signer, so it is not a root).
pub(super) fn self_certifies_signer(header: &AccountEntryHeader, payload: &[u8]) -> bool {
    let mut map = HashMap::new();
    add_self_pubkey(&mut map, header, payload);
    map.contains_key(&header.device_fingerprint)
}

/// Add the `(fingerprint → pubkey)` an entry itself certifies: a genesis certifies its founder key
/// (the SIGNER); a DeviceAdd certifies the ADDED device's key. Both bind `sha256(pk) ==
/// fingerprint` (genesis: the header device; DeviceAdd: the op's `device_fingerprint`, enforced at
/// decode).
pub(super) fn add_self_pubkey(
    map: &mut HashMap<DeviceFingerprint, [u8; 32]>,
    header: &AccountEntryHeader,
    payload: &[u8],
) {
    if !is_current_control_plaintext(header) {
        return;
    }
    if let Ok(DecodedAccountOp::Known(op)) = ops::decode(header.entry_type, payload) {
        match op {
            AccountOp::AccountGenesis { ed25519_pubkey, .. } => {
                if DevicePublic::from_bytes(&ed25519_pubkey)
                    .is_ok_and(|key| key.fingerprint() == header.device_fingerprint)
                {
                    map.insert(header.device_fingerprint, ed25519_pubkey);
                }
            },
            AccountOp::DeviceAdd { device_fingerprint, ed25519_pubkey, .. } => {
                map.insert(device_fingerprint, ed25519_pubkey);
            },
            _ => {},
        }
    }
}

/// The `(fingerprint, x25519_pubkey)` recipients a fresh `StreamKeyWrap` seals to: every
/// ROSTER-EFFECTIVE device (`account_roster_history.closed_at IS NULL`), each keyed to the x25519
/// of the EXACT accepted enrollment that put it on the roster. Unlike [`stored_device_pubkeys`] —
/// which is fold-independent and returns REMOVED devices too — this reads the fold-projected
/// effective set, so a fresh key is NEVER sealed to a removed device (that would re-grant read
/// access and defeat rotation-on-removal, C4.4). Every effective role is a recipient (Member AND
/// Owner): the roster gate is read access, not authoring authority. Acceptors do NOT re-derive the
/// recipient set, so sealing to only effective devices is a local-honesty obligation of the
/// authoring owner.
///
/// The recipient x25519 is bound to the enrollment via `roster_ref` — the enrolling entry's own
/// `entry_hash` (fold `roster_refs.insert(hash, RosterFact)`: a genesis roster row → the genesis
/// hash, a DeviceAdd roster row → that DeviceAdd's entry hash). Resolving the key from THAT single
/// entry, never by collecting x25519 by fingerprint across all candidates, is what stops a
/// rejected/forked sibling `DeviceAdd` (same fingerprint, attacker-chosen x25519, a DIFFERENT
/// entry_hash) from shadowing the effective device's real key — a member could otherwise be sealed
/// a key it can't decrypt.
pub(super) fn list_effective_roster_x25519_pubkeys(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<(DeviceFingerprint, DeviceX25519Public)>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    // The effective set + the enrolling entry each device's key must come from. DISTINCT because
    // one device can (in principle) key more than one `roster_ref` row.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT device_fingerprint, roster_ref FROM account_roster_history
         WHERE account_id = ?1 AND closed_at IS NULL
         ORDER BY device_fingerprint",
    )?;
    let rows = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (fp, roster_ref) in rows {
        let fingerprint = DeviceFingerprint::from_bytes(id::fixed(&fp)?);
        let roster_ref: RosterRef = RosterRef::from_bytes(id::fixed(&roster_ref)?);
        out.push((fingerprint, enrollment_x25519(conn, account_id, &roster_ref, fingerprint)?));
    }
    Ok(out)
}

/// The x25519 key certified by the exact enrollment that currently makes `fingerprint`
/// roster-effective. `None` means the device is not currently effective; rejected and forked
/// enrollment candidates are never consulted because the lookup follows the projected
/// `roster_ref` into one accepted entry.
pub(super) fn effective_roster_x25519_pubkey(
    conn: &Connection,
    account_id: AccountId,
    fingerprint: DeviceFingerprint,
) -> anyhow::Result<Option<DeviceX25519Public>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let roster_ref: Option<Vec<u8>> = conn
        .query_row(
            "SELECT roster_ref FROM account_roster_history
             WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NULL
             ORDER BY roster_ref LIMIT 1",
            params![account_id.to_bytes().as_slice(), fingerprint.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    roster_ref
        .map(|roster_ref| {
            enrollment_x25519(
                conn,
                account_id,
                &RosterRef::from_bytes(id::fixed(&roster_ref)?),
                fingerprint,
            )
        })
        .transpose()
}

/// The FINGERPRINTS of every roster-effective device on `account_id` — the cheap counterpart to
/// [`list_effective_roster_x25519_pubkeys`] for a per-seal boolean (the C4.4 rotation-needed
/// predicate in `secrets::sealing`). `DISTINCT` because one device can key more than one open
/// `roster_ref` row. Unlike the x25519 reader it decodes NO enrollment (no `signed_bytes` fetch, no
/// small-order blocklist) and does NOT fail loud on a corrupt projection: the predicate only asks
/// "is this fingerprint still effective?", so the per-recipient enrollment cross-checks are dead
/// weight here.
pub(super) fn list_effective_roster_fingerprints(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<DeviceFingerprint>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT device_fingerprint FROM account_roster_history
         WHERE account_id = ?1 AND closed_at IS NULL
         ORDER BY device_fingerprint",
    )?;
    let rows = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|fp| Ok(DeviceFingerprint::from_bytes(id::fixed(&fp)?))).collect()
}

/// The effective enrollment entry and current role for `fingerprint`, read in the caller's
/// snapshot. This is the exact fact an enrollment author verifies after refolding: the returned
/// `roster_ref` identifies which `DeviceAdd` won, not merely that some enrollment exists.
pub(super) fn effective_roster_entry_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    fingerprint: DeviceFingerprint,
) -> anyhow::Result<Option<(RosterRef, ops::DeviceRole)>> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    let row: Option<(Vec<u8>, String)> = conn
        .query_row(
            "SELECT roster_ref, role FROM account_roster_history
             WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NULL",
            params![account_id.to_bytes().as_slice(), fingerprint.to_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(roster_ref, role)| {
        Ok((RosterRef::from_bytes(id::fixed(&roster_ref)?), ops::DeviceRole::from_db_str(&role)?))
    })
    .transpose()
}

/// Whether `fingerprint` is currently a roster-effective WRITER (`Member`/`Owner`, `closed_at IS
/// NULL`) of `account_id` — the exact admission fact the table-sync ingest gate needs (#935). An
/// order-free `EXISTS`: one fingerprint can hold several open roster rows (siblings that all won at
/// the same tail), so a single-row `SELECT role` would be nondeterministic, but "does an effective
/// writer row exist" is not. `role` is the immutable ENROLLMENT role — Owner *promotion* lives in
/// `account_owner_incarnations` and is irrelevant here (Member and Owner both write; a `ReadOnly`
/// enrollment cannot be promoted, per §9), so this must NOT become a join with the incarnation
/// table. A device effective in a DIFFERENT account resolves false, so wrong-account is covered.
pub(crate) fn device_is_effective_writer(
    conn: &Connection,
    account_id: AccountId,
    fingerprint: DeviceFingerprint,
) -> anyhow::Result<bool> {
    let _snapshot = super::control_policy::read_snapshot(conn)?;
    super::control_policy::require_supported_account_control(conn, account_id)?;
    Ok(conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_roster_history
             WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NULL
               AND role IN ('member', 'owner')
         )",
        params![account_id.to_bytes().as_slice(), fingerprint.to_bytes().as_slice()],
        |row| row.get::<_, bool>(0),
    )?)
}

/// Whether `fingerprint` was EVER enrolled on `account_id` as a writer (`Member`/`Owner`) — the
/// fact the table-sync unsent-work guard keys on. That guard decides something that cannot be
/// undone (a received row is applied over local state on the strength of "nothing here could ever
/// be authored"), so its answer must never flip back to false once true. Nothing in the roster
/// projection is monotone: every fold rebuilds it from scratch, and a contested fold drops the
/// device's enrolment outright rather than closing it. The projection is consulted first because
/// it is indexed and answers the common case; when it is silent the STORED account log decides —
/// a genesis this device authored, or a `DeviceAdd` naming it `Member`/`Owner`, whatever the fold
/// currently makes of either. Stored entries are never removed, so the answer stays true. A
/// read-only enrolment resolves false on both paths; a device re-enrolled read-only after being a
/// writer resolves true, which is the right side to err on (its earlier edits may be unpublished).
/// So does a stored-but-rejected `DeviceAdd` naming the device — that only keeps the guard on a
/// device it need not protect (rows park, nothing is lost), never the reverse. What the log has
/// not seen it cannot vouch for: a device enrolled AFTER its local rows were applied over was
/// not a writer at that moment, and that decision is not revisited.
///
/// Retained control entries (a future `op_version`, a sealed `crypto_suite`, an unknown tag) are
/// stored undecodable by design and skipped, as `verify_stored_snapshots` skips them: they never
/// fold into the roster, so none of them could have made the device a writer.
pub(crate) fn device_ever_enrolled_as_writer(
    conn: &Connection,
    account_id: AccountId,
    fingerprint: DeviceFingerprint,
) -> anyhow::Result<bool> {
    let projected: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_roster_history
             WHERE account_id = ?1 AND device_fingerprint = ?2 AND role IN ('member', 'owner')
         )",
        params![account_id.to_bytes().as_slice(), fingerprint.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if projected {
        return Ok(true);
    }
    // Entry-type tags are per log: the secrets log reuses tag 1, so gate on the control log.
    let mut stmt = conn.prepare(
        "SELECT entry_type, device_fingerprint, signed_bytes FROM account_entries
         WHERE account_id = ?1 AND log_id = ?2 AND entry_type IN (?3, ?4)",
    )?;
    let rows = stmt.query_map(
        params![
            account_id.to_bytes().as_slice(),
            fold::CONTROL_LOG,
            ops::entry_type::ACCOUNT_GENESIS,
            ops::entry_type::DEVICE_ADD
        ],
        |row| Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?)),
    )?;
    for row in rows {
        let (entry_type, author, signed_bytes) = row?;
        // A genesis enrols its author as the founding owner; the header names that device.
        if entry_type == ops::entry_type::ACCOUNT_GENESIS {
            if author.as_slice() == fingerprint.to_bytes() {
                return Ok(true);
            }
            continue;
        }
        let entry = envelope::decode_account_signed(&signed_bytes)?;
        if !is_current_control_plaintext(&entry.header) {
            continue;
        }
        if let Ok(DecodedAccountOp::Known(AccountOp::DeviceAdd {
            device_fingerprint, role, ..
        })) = ops::decode(entry.header.entry_type, &entry.payload)
            && device_fingerprint == fingerprint
            && matches!(role, DeviceRole::Member | DeviceRole::Owner)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The x25519 key the ONE accepted enrollment entry at `roster_ref` certifies for `fingerprint`
/// (genesis → its founder / header device; DeviceAdd → the ADDED device), routed through the
/// small-order / identity blocklist. Bound to that exact `entry_hash`, so a rejected/forked sibling
/// enrollment for the same fingerprint is never consulted. Fails LOUD on a corrupt projection — a
/// missing/undecodable enrollment, one that certifies no valid x25519, or one whose certified
/// device disagrees with its roster row — rather than dropping the recipient (a dropped recipient
/// silently loses read access; a wrong key breaks the member's decryption).
fn enrollment_x25519(
    conn: &Connection,
    account_id: AccountId,
    roster_ref: &RosterRef,
    fingerprint: DeviceFingerprint,
) -> anyhow::Result<DeviceX25519Public> {
    let signed_bytes: Vec<u8> = conn
        .query_row(
            "SELECT signed_bytes FROM account_entries WHERE account_id = ?1 AND entry_hash = ?2",
            params![account_id.to_bytes().as_slice(), roster_ref.as_slice()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "roster-effective device names an enrollment entry absent from account_entries \
                 (corrupt projection)",
            )
        })?;
    let signed = envelope::decode_account_signed(&signed_bytes)
        .map_err(|err| anyhow::anyhow!("effective enrollment entry does not decode: {err}"))?;
    let (certified_fp, pubkey) = enrollment_certified_x25519(&signed.header, &signed.payload)
        .ok_or_else(|| {
            anyhow::anyhow!("effective enrollment entry certifies no valid x25519 key")
        })?;
    // The enrollment must certify the very device its roster row names, or the projection and the
    // signed op disagree — a corrupt state, not a recipient to seal to under the wrong key.
    anyhow::ensure!(
        certified_fp == fingerprint,
        "effective enrollment entry certifies a different device than its roster row",
    );
    Ok(pubkey)
}

/// The `(fingerprint, x25519_pubkey)` a single genesis / DeviceAdd enrollment certifies: a genesis
/// certifies its founder (the header device); a DeviceAdd certifies the ADDED device (the payload's
/// derived fingerprint). The key is routed through [`DeviceX25519Public::from_bytes`] (the
/// small-order / identity blocklist). `None` for any other op, a non-current-control-plaintext
/// header, or an invalid key.
fn enrollment_certified_x25519(
    header: &AccountEntryHeader,
    payload: &[u8],
) -> Option<(DeviceFingerprint, DeviceX25519Public)> {
    if !is_current_control_plaintext(header) {
        return None;
    }
    let DecodedAccountOp::Known(op) = ops::decode(header.entry_type, payload).ok()? else {
        return None;
    };
    match op {
        AccountOp::AccountGenesis { x25519_pubkey, .. } =>
            Some((header.device_fingerprint, DeviceX25519Public::from_bytes(&x25519_pubkey).ok()?)),
        AccountOp::DeviceAdd { device_fingerprint, x25519_pubkey, .. } =>
            Some((device_fingerprint, DeviceX25519Public::from_bytes(&x25519_pubkey).ok()?)),
        _ => None,
    }
}

fn insert_pre_verify(
    conn: &Connection,
    entry_hash: &AccountEntryHash,
    account_id: AccountId,
    fingerprint: DeviceFingerprint,
    signed_bytes: &[u8],
    now_ms: i64,
) -> rusqlite::Result<PreVerifyInsert> {
    let signed_hash = cbor::sha256(signed_bytes);
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO account_pre_verify(
             signed_hash, entry_hash, claimed_account_id, claimed_fingerprint, raw_bytes,
             received_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            signed_hash.as_slice(),
            entry_hash.as_slice(),
            account_id.to_bytes().as_slice(),
            fingerprint.to_bytes().as_slice(),
            signed_bytes,
            now_ms,
        ],
    )?;
    if inserted == 0 {
        return Ok(PreVerifyInsert::Parked { evicted: Vec::new() });
    }
    enforce_pre_verify_budget(conn, account_id, &signed_hash.into())
}

/// Keep the unauthenticated queue within its per-account and global budgets.
fn enforce_pre_verify_budget(
    conn: &Connection,
    account_id: AccountId,
    inserted_signed_hash: &SignedHash,
) -> rusqlite::Result<PreVerifyInsert> {
    let outcome = PRE_VERIFY.enforce_budget(
        conn,
        account_id,
        inserted_signed_hash,
        QueueBudget { max: PRE_VERIFY_PER_ACCOUNT_MAX, scope: CapacityScope::PreVerifyAccount },
        QueueBudget { max: PRE_VERIFY_GLOBAL_MAX, scope: CapacityScope::PreVerifyGlobal },
    )?;
    Ok(match outcome {
        BudgetOutcome::Parked { evicted } => PreVerifyInsert::Parked { evicted },
        BudgetOutcome::AtCapacity(scope) => PreVerifyInsert::AtCapacity(scope),
    })
}

/// Retry every pre-verify row for the account against the now-larger device set: a row whose signer
/// resolves is verified + promoted into `account_entries` and cleared; the rest stay parked.
fn promote_pre_verify(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<PromotionOutcome> {
    // Fixpoint: a promoted genesis/DeviceAdd enlarges the resolvable device set, which can in turn
    // resolve a DEEPER parked entry (a device chain — founder→B→C — delivered before its
    // authorizers). Feed each promoted key back and re-scan until a full pass promotes nothing.
    // Snapshotting the device set once would strand depth≥2 chains forever, so two peers that
    // received the same entries in different orders would converge on different accepted sets.
    let mut pubkeys = stored_device_pubkeys(tx, account_id)?;
    let mut outcome = PromotionOutcome::default();
    loop {
        let pending: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = {
            let mut stmt = tx.prepare(
                "SELECT signed_hash, claimed_fingerprint, raw_bytes
                 FROM account_pre_verify WHERE claimed_account_id = ?1
                 ORDER BY signed_hash",
            )?;
            stmt.query_map(params![account_id.to_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut promoted_any = false;
        for (signed_hash, fp_bytes, raw_bytes) in pending {
            let fp = DeviceFingerprint::from_bytes(id::fixed(&fp_bytes)?);
            let Some(pk_bytes) = pubkeys.get(&fp).copied() else {
                continue; // still unresolvable — may resolve in a later round
            };
            let promoted = DevicePublic::from_bytes(&pk_bytes)
                .ok()
                .and_then(|pk| envelope::verify_account_signed(&raw_bytes, &pk).ok())
                .filter(|v| {
                    validate_storable_header_payload(&v.header, &v.payload).is_ok()
                        && validate_authenticated_entry(v).is_ok()
                });
            let Some(verified) = promoted else {
                // The signer resolved but the signature or authenticated payload was invalid.
                PRE_VERIFY.delete(tx, &signed_hash)?;
                continue;
            };
            match insert_candidate(tx, &verified, &raw_bytes, now_ms)? {
                CandidateInsert::AtCapacity(scope) => {
                    // Candidate history is grow-only, so this capacity state cannot recover by
                    // itself. Remove the row from the unauthenticated queue and return its entry
                    // hash so the transport can request redelivery if the operational budget is
                    // later raised or the store is rebuilt. Retaining it would only strand it until
                    // a later park-budget eviction silently discarded it.
                    outcome.scope.get_or_insert(scope);
                    outcome.entry_hashes.push(verified.entry_hash);
                    PRE_VERIFY.delete(tx, &signed_hash)?;
                },
                CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {
                    // A promoted genesis/DeviceAdd certifies a device key — feed it back so the
                    // next round can resolve entries that were waiting on it.
                    add_self_pubkey(&mut pubkeys, &verified.header, &verified.payload);
                    promoted_any = true;
                    PRE_VERIFY.delete(tx, &signed_hash)?;
                },
            }
        }
        // A round advances the queue only by promoting (each promotion deletes ≥1 pending row and
        // adds ≥1 key), so this terminates; a round that promotes nothing new is the fixpoint.
        if !promoted_any {
            break;
        }
    }
    Ok(outcome)
}

/// A genesis op — gated on `log_id == CONTROL_LOG` (S3) so a secrets-log tag reusing the 0 number
/// can never be mistaken for `AccountGenesis` (pre-verify / device-key promotion).
fn is_genesis(header: &AccountEntryHeader) -> bool {
    header.log_id == fold::CONTROL_LOG && header.entry_type == ops::entry_type::ACCOUNT_GENESIS
}

fn is_current_control_plaintext(header: &AccountEntryHeader) -> bool {
    header.log_id == fold::CONTROL_LOG
        && header.crypto_suite == 0
        && header.op_version == fold::SUPPORTED_OP_VERSION
}

/// The secrets-log analog of [`is_current_control_plaintext`]: a current-version plaintext entry on
/// `log_id == SECRETS_LOG`. Its payload is a secrets op, structurally validated by the secrets
/// twin.
fn is_current_secrets_plaintext(header: &AccountEntryHeader) -> bool {
    header.log_id == fold::SECRETS_LOG
        && header.crypto_suite == 0
        && header.op_version == fold::SUPPORTED_OP_VERSION
}

/// The annex-plaintext predicate (C6): a current-version, plaintext entry on the annex log.
fn is_current_annex_plaintext(header: &AccountEntryHeader) -> bool {
    header.log_id == fold::ANNEX_LOG
        && header.crypto_suite == 0
        && header.op_version == fold::SUPPORTED_OP_VERSION
}

/// A SEALED snapshot is a contradiction, not a forward-version entry to retain: the manifest's
/// entire §4.7 value is that a peer can verify coverage WITHOUT the plaintext, so a snapshot whose
/// manifest is ciphertext can never serve its one purpose. Refuse it rather than storing opaque
/// bytes that no binary — present or future — could interpret as a coverage claim.
///
/// Scoped to the snapshot TAG, not the annex log: `crypto_suite` is in the clear header, and a
/// later annex artifact class may legitimately be sealed. Blanket-rejecting every sealed annex
/// entry would spend that option for nothing.
///
/// Deliberately NOT scoped to the current `op_version`. "A snapshot is plaintext" is a property of
/// the artifact class, not of one version's encoding, so a future-version sealed snapshot is just
/// as uninterpretable as a current one. If a later version ever wants a sealed coverage artifact it
/// is a different class and takes a different annex tag — tags 1.. are free, and that is cheaper
/// than leaving a grow-only hole here that no verifier could ever evaluate.
/// A current-version, plaintext control-v2 view manifest on the annex log — the exact shape a
/// pinned refold can read back and hand to the planner as a cut's evidence.
fn is_view_manifest(header: &AccountEntryHeader) -> bool {
    is_current_annex_plaintext(header) && header.entry_type == annex::ops::entry_type::VIEW_MANIFEST
}

fn is_sealed_snapshot(header: &AccountEntryHeader) -> bool {
    header.log_id == fold::ANNEX_LOG
        && header.entry_type == annex::ops::entry_type::SNAPSHOT
        && header.crypto_suite != 0
}

pub(super) fn validate_storable_header_payload(
    header: &AccountEntryHeader,
    payload: &[u8],
) -> Result<(), String> {
    i64::try_from(header.seq)
        .map_err(|_| "account seq exceeds SQLite INTEGER range".to_string())?;
    if is_current_control_plaintext(header) {
        ops::decode(header.entry_type, payload)
            .map_err(|err| format!("op payload decode failed: {err}"))?;
        validate_genesis_binding(header, payload)?;
    } else if is_current_secrets_plaintext(header) {
        // The secrets-plaintext twin (C4.2b): a known secrets tag is fully validated, an unknown
        // tag is retained opaque. A future-version / sealed secrets entry falls through
        // unvalidated (it is slot-eligible, folded by a newer binary).
        secrets::validate_storable_secrets_payload(header.entry_type, payload)
            .map_err(|err| format!("secrets op payload decode failed: {err}"))?;
    } else if is_sealed_snapshot(header) {
        return Err("snapshot manifests must be plaintext-signed (crypto_suite 0)".to_string());
    } else if is_current_annex_plaintext(header) {
        // The annex-plaintext twin (C6): a known annex tag is fully validated so a garbage manifest
        // can never chain; an unknown tag is retained opaque. This is STRUCTURAL only — whether the
        // manifest's coverage claim is true is a read-time question, and asking it here would make
        // storage depend on what this device happens to hold.
        annex::ops::validate_storable_annex_payload(header.entry_type, payload)
            .map_err(|err| format!("annex op payload decode failed: {err}"))?;
    }
    Ok(())
}

fn validate_authenticated_entry(entry: &VerifiedAccountEntry) -> Result<(), String> {
    validate_genesis_binding(&entry.header, &entry.payload)
}

fn validate_genesis_binding(header: &AccountEntryHeader, payload: &[u8]) -> Result<(), String> {
    if !is_current_control_plaintext(header) || !is_genesis(header) {
        return Ok(());
    }
    if id::account_id_from_genesis_payload(payload) != header.account_id {
        return Err("genesis payload does not hash to its account_id".into());
    }
    match ops::decode(header.entry_type, payload) {
        Ok(DecodedAccountOp::Known(AccountOp::AccountGenesis { ed25519_pubkey, .. }))
            if DevicePublic::from_bytes(&ed25519_pubkey)
                .is_ok_and(|key| key.fingerprint() == header.device_fingerprint) =>
            Ok(()),
        _ => Err("genesis founder key does not match signer fingerprint".into()),
    }
}

/// A DeviceAdd op — gated on `log_id == CONTROL_LOG` (S3) so a secrets-log tag reusing the 1 number
/// can never spuriously trigger the device-key promotion path.
fn is_device_add(verified: &VerifiedAccountEntry) -> bool {
    verified.header.log_id == fold::CONTROL_LOG
        && verified.header.entry_type == ops::entry_type::DEVICE_ADD
}

/// The projected status of a single entry (§16.3), or `None` if the entry isn't stored (e.g. it is
/// still in the pre-verify queue). A read helper for queries.
pub(super) fn entry_status(
    conn: &Connection,
    entry_hash: &AccountEntryHash,
) -> anyhow::Result<Option<(String, Option<String>)>> {
    Ok(conn
        .query_row(
            "SELECT status, detail FROM account_entry_status WHERE entry_hash = ?1",
            params![entry_hash.as_slice()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?)
}

#[cfg(test)]
#[path = "storage/tests.rs"]
mod tests;
