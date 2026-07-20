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
use super::id::account_id_from_genesis_payload;
use super::ops::{self, AccountOp, DecodedAccountOp, DeviceCut, DeviceRole, GrantRole};
use super::{AccountId, content, fold, secrets};
use crate::cbor;
use crate::device::{DevicePublic, DeviceX25519Public};
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;

type EntryHash = [u8; 32];
type BranchKey = (u8, DeviceFingerprint, Option<EntryHash>);
type BranchChild = (u64, EntryHash);
type BranchChildren = HashMap<BranchKey, Vec<BranchChild>>;

// Operational admission limits, not wire-validity limits. At the §18a envelope maximum these cap
// unauthenticated parked bytes at 4 MiB/account and 16 MiB globally. Candidate admission has both
// count and aggregate-byte limits: refolding materializes every candidate, so a count-only ceiling
// would still permit hundreds of MiB in one writer transaction. Admission rejects rather than
// evicts: deleting grow-only history would break replica convergence.
const PRE_VERIFY_PER_ACCOUNT_MAX: usize = 64;
const PRE_VERIFY_GLOBAL_MAX: usize = 256;
const CANDIDATES_PER_ACCOUNT_MAX: usize = 4_096;
const CANDIDATES_GLOBAL_MAX: usize = 16_384;
const CANDIDATE_BYTES_PER_ACCOUNT_MAX: usize = 16 * 1024 * 1024;
const CANDIDATE_BYTES_GLOBAL_MAX: usize = 64 * 1024 * 1024;

struct AccountProjection {
    history: fold::AccountAuthHistory,
    accepted: HashSet<EntryHash>,
    forked: HashSet<EntryHash>,
}

struct AccountStateFold {
    statuses: HashMap<EntryHash, String>,
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

#[derive(Debug, Default, Eq, PartialEq)]
struct PromotionOutcome {
    scope: Option<CapacityScope>,
    entry_hashes: Vec<EntryHash>,
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
    /// Stored as a candidate; `status` is its post-refold §16.3 taxonomy label.
    Ingested { status: String },
    /// This entry was stored, but valid parked entries hit terminal grow-only candidate capacity.
    /// They were removed from the unauthenticated queue; request these hashes again if capacity is
    /// raised or storage is rebuilt under a larger operational budget.
    IngestedWithRejectedPromotions {
        status: String,
        scope: CapacityScope,
        entry_hashes: Vec<EntryHash>,
    },
    IngestedWithRejectedContentPromotions {
        status: String,
        scope: content::ContentCapacityScope,
        entry_hashes: Vec<EntryHash>,
    },
    IngestedWithRejectedAccountAndContentPromotions {
        status: String,
        account_scope: CapacityScope,
        account_entry_hashes: Vec<EntryHash>,
        content_scope: content::ContentCapacityScope,
        content_entry_hashes: Vec<EntryHash>,
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
        return Ok(IngestOutcome::Ingested { status });
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
    let verified = if let Some(verified) = optimistic_verified {
        verified
    } else {
        let mut pubkeys = stored_device_pubkeys(&tx, account_id)?;
        add_self_pubkey(&mut pubkeys, &signed.header, &signed.payload);
        let Some(pubkey_bytes) = pubkeys.get(&device_fp).copied() else {
            let parked = insert_pre_verify(
                &tx,
                &signed.entry_hash,
                account_id,
                device_fp,
                signed_bytes,
                now_ms,
            )?;
            tx.commit()?;
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

    match insert_candidate(&tx, &verified, signed_bytes, now_ms)? {
        CandidateInsert::AtCapacity(scope) => {
            return Ok(IngestOutcome::CapacityReached { scope });
        },
        CandidateInsert::AlreadyPresent => {
            if let Some((status, _)) = entry_status(&tx, &verified.entry_hash)? {
                tx.commit()?;
                return Ok(IngestOutcome::Ingested { status });
            }
        },
        CandidateInsert::Inserted => {},
    }
    // A DeviceAdd/genesis may resolve devices that were parked — retry their pre-verify rows.
    let rejected_promotions = if is_genesis(&verified.header) || is_device_add(&verified) {
        promote_pre_verify(&tx, account_id, now_ms)?
    } else {
        PromotionOutcome::default()
    };
    let state = refold_untrusted_ingest_in_tx(&tx, account_id, now_ms)?;
    tx.commit()?;
    let status =
        state.statuses.get(&verified.entry_hash).cloned().unwrap_or_else(|| "unknown".into());
    Ok(match (rejected_promotions.scope, state.rejected_content_promotions.scope) {
        (Some(account_scope), Some(content_scope)) =>
            IngestOutcome::IngestedWithRejectedAccountAndContentPromotions {
                status,
                account_scope,
                account_entry_hashes: rejected_promotions.entry_hashes,
                content_scope,
                content_entry_hashes: state.rejected_content_promotions.entry_hashes,
            },
        (Some(scope), None) => IngestOutcome::IngestedWithRejectedPromotions {
            status,
            scope,
            entry_hashes: rejected_promotions.entry_hashes,
        },
        (None, Some(scope)) => IngestOutcome::IngestedWithRejectedContentPromotions {
            status,
            scope,
            entry_hashes: state.rejected_content_promotions.entry_hashes,
        },
        (None, None) => IngestOutcome::Ingested { status },
    })
}

fn authenticate_entry(
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
    entry_hash: &EntryHash,
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

/// Populate V064's derived authority tables from every account already present in the V059
/// candidate DAG. The migration owns `tx`; replaying all accounts and recording V064 therefore
/// commits as one writer-locked unit, with no source-history gap between the scan and ledger.
pub fn backfill_authority_projection(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    let account_ids = {
        let mut stmt =
            tx.prepare("SELECT DISTINCT account_id FROM account_entries ORDER BY account_id")?;
        stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for account_bytes in account_ids {
        let result = fixed(&account_bytes)
            .map(AccountId::from_bytes)
            .and_then(|account_id| refold_in_tx(tx, account_id, 0));
        if let Err(err) = result {
            return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                format!("V064 authority backfill failed: {err:#}"),
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
    roster_ref: EntryHash,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::RosterAuthority>> {
    let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let conn: &Connection = &read_tx;
    let row: Option<(Vec<u8>, String, i64, Option<i64>)> = conn
        .query_row(
            "SELECT device_fingerprint, role, effective_at, closed_at
             FROM account_roster_history WHERE account_id = ?1 AND roster_ref = ?2",
            params![account_id.to_bytes().as_slice(), roster_ref.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((device, role, effective_at, closed_at)) = row else {
        return missing_reference(conn, account_id, &roster_ref);
    };
    let authority = fold::RosterAuthority {
        device_fingerprint: DeviceFingerprint::from_bytes(fixed(&device)?),
        current_role: DeviceRole::from_db_str(&role)?,
    };
    if authority.device_fingerprint != device_fingerprint {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    validated_open_fact(authority, effective_at, closed_at)
}

pub fn roster_content_authority(
    conn: &Connection,
    account_id: AccountId,
    roster_ref: EntryHash,
    device_fingerprint: DeviceFingerprint,
    stream_id: StreamId,
) -> anyhow::Result<fold::AuthorityQuery<fold::RosterContentAuthority>> {
    let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    roster_content_authority_in_snapshot(
        &read_tx,
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
    roster_ref: EntryHash,
    device_fingerprint: DeviceFingerprint,
    stream_id: StreamId,
) -> anyhow::Result<fold::AuthorityQuery<fold::RosterContentAuthority>> {
    let row: Option<(Vec<u8>, String, i64, Option<i64>)> = conn
        .query_row(
            "SELECT device_fingerprint, role, effective_at, closed_at
             FROM account_roster_history WHERE account_id = ?1 AND roster_ref = ?2",
            params![account_id.to_bytes().as_slice(), roster_ref.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((device, role, effective_at, closed_at)) = row else {
        return missing_reference(conn, account_id, &roster_ref);
    };
    let roster = fold::RosterAuthority {
        device_fingerprint: DeviceFingerprint::from_bytes(fixed(&device)?),
        current_role: DeviceRole::from_db_str(&role)?,
    };
    if roster.device_fingerprint != device_fingerprint {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
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
            seq: u64::from_be_bytes(fixed(&seq)?),
            hash: fixed(&hash)?,
        },
        None if closed_at.is_none() => fold::AuthorityBoundary::Open,
        None => fold::AuthorityBoundary::Closed,
    };
    Ok(fold::AuthorityQuery::Effective(fold::RosterContentAuthority {
        device_fingerprint: roster.device_fingerprint,
        boundary,
    }))
}

pub fn owner_control_authority(
    conn: &Connection,
    account_id: AccountId,
    owner_id: EntryHash,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    owner_chain_authority(conn, account_id, owner_id, device_fingerprint, "control")
}

pub fn owner_secrets_authority(
    conn: &Connection,
    account_id: AccountId,
    owner_id: EntryHash,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    owner_chain_authority(conn, account_id, owner_id, device_fingerprint, "secrets")
}

/// The body of [`owner_secrets_authority`], reading whatever snapshot `conn` is already in — the
/// secrets refold (C4.2b) resolves every wrap's owner-incarnation authority inside its own txn and
/// cannot call the wrapper above, which would try to `BEGIN` a second one (S1; mirrors
/// [`stream_owner_effective_in_snapshot`]).
pub fn owner_secrets_authority_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    owner_id: EntryHash,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    owner_chain_authority_in_snapshot(conn, account_id, owner_id, device_fingerprint, "secrets")
}

fn owner_chain_authority(
    conn: &Connection,
    account_id: AccountId,
    owner_id: EntryHash,
    device_fingerprint: DeviceFingerprint,
    chain: &str,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
    let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    owner_chain_authority_in_snapshot(&read_tx, account_id, owner_id, device_fingerprint, chain)
}

/// The body of [`owner_chain_authority`], reading whatever snapshot `conn` is already in.
fn owner_chain_authority_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    owner_id: EntryHash,
    device_fingerprint: DeviceFingerprint,
    chain: &str,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerChainAuthority>> {
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
        return missing_reference(conn, account_id, &owner_id);
    };
    let owner =
        fold::OwnerAuthority { device_fingerprint: DeviceFingerprint::from_bytes(fixed(&device)?) };
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

pub fn owner_incarnation_effective(
    conn: &Connection,
    account_id: AccountId,
    owner_id: EntryHash,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::OwnerAuthority>> {
    let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let conn: &Connection = &read_tx;
    let row: Option<(Vec<u8>, i64, Option<i64>)> = conn
        .query_row(
            "SELECT device_fingerprint, effective_at, closed_at
             FROM account_owner_incarnations WHERE account_id = ?1 AND owner_id = ?2",
            params![account_id.to_bytes().as_slice(), owner_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((device, effective_at, closed_at)) = row else {
        return missing_reference(conn, account_id, &owner_id);
    };
    let authority =
        fold::OwnerAuthority { device_fingerprint: DeviceFingerprint::from_bytes(fixed(&device)?) };
    if authority.device_fingerprint != device_fingerprint {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    validated_open_fact(authority, effective_at, closed_at)
}

type StoredGrantRow = (Vec<u8>, Vec<u8>, String, i64, Option<i64>);

pub fn grant_effective(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: EntryHash,
    stream_id: StreamId,
    grantee_account_id: AccountId,
) -> anyhow::Result<fold::AuthorityQuery<fold::GrantAuthority>> {
    let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let conn: &Connection = &read_tx;
    let row: Option<StoredGrantRow> = conn
        .query_row(
            "SELECT stream_id, grantee_account_id, role, effective_at, closed_at
             FROM account_stream_grants WHERE owner_account_id = ?1 AND grant_id = ?2",
            params![owner_account_id.to_bytes().as_slice(), grant_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((stored_stream, stored_grantee, role, effective_at, closed_at)) = row else {
        return missing_reference(conn, owner_account_id, &grant_id);
    };
    let authority = fold::GrantAuthority {
        stream_id: StreamId::from_bytes(fixed(&stored_stream)?),
        grantee_account_id: AccountId::from_bytes(fixed(&stored_grantee)?),
        role: GrantRole::from_db_str(&role)?,
    };
    if authority.stream_id != stream_id || authority.grantee_account_id != grantee_account_id {
        return Ok(fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject));
    }
    validated_fact(authority, effective_at, closed_at)
}

/// Resolve a grant and the requesting device's revoke cut as ONE authorization decision. C2 must
/// use this combined seam when admitting content: two independent calls could otherwise straddle
/// a refold and combine an old effective grant with a new (or absent) cut projection.
pub fn grant_effective_for_device(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: EntryHash,
    stream_id: StreamId,
    grantee_account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::GrantDeviceAuthority>> {
    let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    grant_effective_for_device_in_snapshot(
        &read_tx,
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
    grant_id: EntryHash,
    stream_id: StreamId,
    grantee_account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<fold::GrantDeviceAuthority>> {
    let row: Option<StoredGrantRow> = conn
        .query_row(
            "SELECT stream_id, grantee_account_id, role, effective_at, closed_at
             FROM account_stream_grants WHERE owner_account_id = ?1 AND grant_id = ?2",
            params![owner_account_id.to_bytes().as_slice(), grant_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((stored_stream, stored_grantee, role, effective_at, closed_at)) = row else {
        return missing_reference(conn, owner_account_id, &grant_id);
    };
    let grant = fold::GrantAuthority {
        stream_id: StreamId::from_bytes(fixed(&stored_stream)?),
        grantee_account_id: AccountId::from_bytes(fixed(&stored_grantee)?),
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
) -> anyhow::Result<fold::AuthorityQuery<EntryHash>> {
    let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    stream_owner_effective_in_snapshot(&read_tx, account_id, stream_id)
}

/// The body of [`stream_owner_effective`], reading whatever snapshot `conn` is already in — see
/// [`roster_content_authority_in_snapshot`] for why the `/3` refold needs this shape.
pub fn stream_owner_effective_in_snapshot(
    conn: &Connection,
    account_id: AccountId,
    stream_id: StreamId,
) -> anyhow::Result<fold::AuthorityQuery<EntryHash>> {
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
    validated_fact(fixed(&own_id)?, effective_at, None)
}

/// Keyed per-device cut lookup for C2 content authorization. The grant's final effectiveness and
/// the optional cut are read from one SQLite snapshot, so a concurrent refold cannot mix rounds.
pub(super) fn grant_device_cut(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: EntryHash,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<fold::AuthorityQuery<Option<DeviceCut>>> {
    let read_tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let conn: &Connection = &read_tx;
    let grant_exists: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM account_stream_grants
             WHERE owner_account_id = ?1 AND grant_id = ?2
         )",
        params![owner_account_id.to_bytes().as_slice(), grant_id.as_slice()],
        |row| row.get(0),
    )?;
    if !grant_exists {
        return missing_reference(conn, owner_account_id, &grant_id);
    }
    let cut = load_grant_device_cut(conn, owner_account_id, grant_id, device_fingerprint)?;
    Ok(fold::AuthorityQuery::Effective(cut))
}

fn load_grant_device_cut(
    conn: &Connection,
    owner_account_id: AccountId,
    grant_id: EntryHash,
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
            seq: u64::from_be_bytes(fixed(&seq)?),
            hash: fixed(&hash)?,
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
    let owner: Option<Vec<u8>> = conn
        .query_row(
            "SELECT account_id FROM account_stream_ownership WHERE stream_id = ?1",
            [stream_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    owner.map(|bytes| Ok(AccountId::from_bytes(fixed(&bytes)?))).transpose()
}

/// Whether an account has folded to `contested` — a genuine owner-key-compromise / equivocation
/// event (§12), which HALTS authority mutation. Content authorized by a contested account is
/// fail-closed: parked (quota-bounded), never accepted, and reclassified if the account recovers.
pub fn account_is_contested(conn: &Connection, account_id: AccountId) -> anyhow::Result<bool> {
    let classification: Option<String> = conn
        .query_row(
            "SELECT classification FROM account_auth_state WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(classification.is_some_and(|state| state == "contested"))
}

/// Measure an asserted control-fold length against our own folded view (§7) — the ONE seam that
/// reads `auth_len`. Keeping it out of the fact queries above is what stops a counter from acting
/// as an authority input: facts always answer from the current fold, and the caller applies this
/// verdict as its own phase, where an `Ahead` author parks rather than pre-empting a decision the
/// fold has already made. An account we hold nothing for has folded zero effective ops (its facts
/// resolve `Unknown` long before freshness is consulted).
pub fn auth_len_freshness(
    conn: &Connection,
    account_id: AccountId,
    asserted_auth_len: u64,
) -> anyhow::Result<fold::AuthorityFreshness> {
    let effective_count: Option<i64> = conn
        .query_row(
            "SELECT effective_count FROM account_auth_state WHERE account_id = ?1",
            [account_id.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let effective_count = effective_count.map(u64::try_from).transpose()?.unwrap_or_default();
    Ok(if asserted_auth_len > effective_count {
        fold::AuthorityFreshness::Ahead
    } else {
        fold::AuthorityFreshness::CurrentOrBehind
    })
}

/// This account's current effective control-fold length — the raw `effective_count`
/// [`auth_len_freshness`] compares against, read from whatever snapshot `conn` is already in. The
/// in-tx content-author seam stamps it as the `owner_auth_len`/`author_auth_len` it cites so its
/// own entries never park `auth_len_ahead` against its own fold; it MUST be read in the SAME
/// snapshot as the authoring txn, or a concurrent control-fold advance would let the citation
/// straddle two folds. Zero for an account we hold nothing for (its facts resolve `Unknown` long
/// before freshness).
pub fn account_effective_count(conn: &Connection, account_id: AccountId) -> anyhow::Result<u64> {
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
) -> anyhow::Result<Option<EntryHash>> {
    let owner_id: Option<Vec<u8>> = conn
        .query_row(
            "SELECT owner_id FROM account_owner_incarnations
             WHERE account_id = ?1 AND device_fingerprint = ?2 AND closed_at IS NULL
             ORDER BY effective_at DESC, owner_id LIMIT 1",
            params![account_id.to_bytes().as_slice(), device_fingerprint.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    owner_id.map(|bytes| fixed(&bytes)).transpose()
}

fn missing_reference<T>(
    conn: &Connection,
    account_id: AccountId,
    reference: &EntryHash,
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
        Some(stored) if fixed(&stored)? != account_id.to_bytes() =>
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
        fold::AuthorityBoundary::Cut { seq, hash } => ("cut", Some(seq.to_be_bytes()), Some(hash)),
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
            seq: u64::from_be_bytes(fixed(&seq)?),
            hash: fixed(&hash)?,
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

/// The refold body (caller owns the txn). Returns each entry_hash → its projected status.
/// `pub(super)` so [`super::bootstrap`] can fold its freshly-inserted local-account genesis inside
/// the same mint transaction, rather than nesting the self-transacting [`refold_account`].
pub(super) fn refold_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<HashMap<[u8; 32], String>> {
    let state = fold_account_state_in_tx(tx, account_id, now_ms)?;
    super::content::finalize_affected_streams(tx, &state.affected_streams)?;
    Ok(state.statuses)
}

/// Remote account ingest commits account/authority/secrets state and durable content wakeups, but
/// leaves content acceptance and projection at the last completed finalization until settle.
fn refold_untrusted_ingest_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<AccountStateFold> {
    let state = fold_account_state_in_tx(tx, account_id, now_ms)?;
    super::content::queue_account_changed_streams(tx, &state.affected_streams, now_ms)?;
    Ok(state)
}

/// Fold account-owned state and return the exact content streams whose acceptance or projection
/// may depend on it. Content finalization is deliberately absent: trusted/local and untrusted
/// remote wrappers choose immediate finalization or durable queueing structurally.
fn fold_account_state_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<AccountStateFold> {
    let rows = load_candidates(tx, account_id)?;
    let projection = derive_account_projection(&rows);

    // Streams this account owns BEFORE the projection rewrite: a fold that drops a `StreamOwn` fact
    // must still refold that stream so its content is declassified, but the ownership row is gone
    // after the rewrite — so capture it now and hand it to the content trigger below.
    let previously_owned = owned_streams(tx, account_id)?;

    // Rewrite atomically so the partial unique index never observes two accepted rows at a slot.
    tx.execute("UPDATE account_entries SET accepted = 0 WHERE account_id = ?1", params![
        account_id.to_bytes().as_slice()
    ])?;

    let mut statuses: HashMap<[u8; 32], String> = HashMap::new();
    for row in rows {
        let accepted = projection.accepted.contains(&row.entry_hash);
        let (status, detail): (String, Option<String>) = if accepted {
            ("accepted".to_string(), None)
        } else if projection.forked.contains(&row.entry_hash) {
            ("forked".to_string(), None)
        } else {
            match projection.history.outcome(&row.entry_hash) {
                Some(outcome) => {
                    let (s, d) = outcome.taxonomy();
                    (s.to_string(), d.map(str::to_string))
                },
                // Every non-forked candidate participates in the final fold.
                None => ("retained_unfolded".to_string(), None),
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
            params![row.entry_hash.as_slice(), status, detail],
        )?;
        statuses.insert(row.entry_hash, status);
    }
    rewrite_authority_projection(tx, account_id, &projection.history)?;
    // Re-derive secrets-log (log 1) acceptance from the just-rewritten authority projection, in
    // this SAME txn (§15, C4.2b). The main loop above wrote `retained_unfolded` for every log-1
    // row (the declassify baseline); this pass OVERWRITES that — in both `account_entry_status`
    // and the returned `statuses` map (S4) — for the wraps it classifies. Same-txn placement is
    // what makes a control fold that condemns a device's secrets chain retro-condemn its wraps
    // atomically.
    super::secrets::refold_secrets_log(tx, account_id, &mut statuses)?;
    let rejected_content_promotions =
        super::content::promote_pre_verify_for_account(tx, account_id, now_ms)?;
    let affected_streams =
        super::content::affected_streams_for_account(tx, account_id, &previously_owned)?;
    Ok(AccountStateFold { statuses, affected_streams, rejected_content_promotions })
}

/// The streams this account currently owns, per the projection — captured before a refold rewrites
/// it so a dropped `StreamOwn` still triggers its stream's content declassification.
fn owned_streams(tx: &Transaction<'_>, account_id: AccountId) -> anyhow::Result<Vec<[u8; 32]>> {
    let mut stmt =
        tx.prepare("SELECT stream_id FROM account_stream_ownership WHERE account_id = ?1")?;
    let rows = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.iter().map(|bytes| fixed(bytes)).collect()
}

/// Replace every query-ready authority fact for this account. The caller's IMMEDIATE refold txn
/// also owns accepted/status, so readers can never observe authority from a different fold round.
fn rewrite_authority_projection(
    tx: &Transaction<'_>,
    account_id: AccountId,
    history: &fold::AccountAuthHistory,
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

/// Derive an author-chain-coherent, authority-closed projection without touching storage.
fn derive_account_projection(rows: &[CandidateRow]) -> AccountProjection {
    let mut forked = HashSet::new();
    loop {
        let entries: Vec<VerifiedAccountEntry> = rows
            .iter()
            .filter(|row| !forked.contains(&row.entry_hash))
            .map(|row| row.verified.clone())
            .collect();
        let history = fold::fold_account(&entries);
        let effective: HashSet<EntryHash> = rows
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
        let newly_forked: Vec<EntryHash> = effective.difference(&accepted).copied().collect();
        if newly_forked.is_empty() {
            return AccountProjection { history, accepted, forked };
        }
        // Monotone elimination is both the termination argument and the security boundary: once
        // an effective candidate loses its author branch or its cited authority branch, neither it
        // nor any effect it produced may participate in a later fold round.
        forked.extend(newly_forked);
    }
}

/// Select one contiguous effective hash-chain per `(log_id, device)` (§16.2).
fn select_coherent_branches(
    rows: &[CandidateRow],
    effective: &HashSet<EntryHash>,
) -> HashSet<EntryHash> {
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
        let mut parent: Option<[u8; 32]> = None;
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
    mut selected: HashSet<EntryHash>,
) -> HashSet<EntryHash> {
    loop {
        let invalid: Vec<EntryHash> = rows
            .iter()
            .filter(|row| selected.contains(&row.entry_hash))
            .filter(|row| {
                row.verified
                    .header
                    .authority_ref
                    .is_some_and(|authority| !selected.contains(&authority))
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
    entry_hash: [u8; 32],
    log_id: u8,
    device_fingerprint: DeviceFingerprint,
    seq: u64,
    verified: VerifiedAccountEntry,
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
            entry_hash: fixed(&hash)?,
            log_id: signed.header.log_id,
            device_fingerprint: DeviceFingerprint::from_bytes(fixed(&fp)?),
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
    let candidate_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM account_entries WHERE account_id = ?1",
        params![h.account_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if candidate_count >= CANDIDATES_PER_ACCOUNT_MAX as i64 {
        return Ok(CandidateInsert::AtCapacity(CapacityScope::CandidateAccount));
    }
    let candidate_bytes: i64 = tx.query_row(
        "SELECT COALESCE(SUM(length(signed_bytes)), 0)
         FROM account_entries WHERE account_id = ?1",
        params![h.account_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if candidate_bytes.saturating_add(signed_bytes.len() as i64)
        > CANDIDATE_BYTES_PER_ACCOUNT_MAX as i64
    {
        return Ok(CandidateInsert::AtCapacity(CapacityScope::CandidateAccountBytes));
    }
    let global_candidate_count: i64 =
        tx.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0))?;
    if global_candidate_count >= CANDIDATES_GLOBAL_MAX as i64 {
        return Ok(CandidateInsert::AtCapacity(CapacityScope::CandidateGlobal));
    }
    let global_candidate_bytes: i64 = tx.query_row(
        "SELECT COALESCE(SUM(length(signed_bytes)), 0) FROM account_entries",
        [],
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
            h.prev_hash.map(|p| p.to_vec()),
            h.parent_ref.map(|p| p.to_vec()),
            h.authority_ref.map(|p| p.to_vec()),
            h.entry_type,
            signed_bytes,
            now_ms,
        ],
    )?;
    Ok(if inserted == 1 { CandidateInsert::Inserted } else { CandidateInsert::AlreadyPresent })
}

/// The `(fingerprint → ed25519_pubkey)` map from every stored genesis / DeviceAdd for the account —
/// the only ops that carry a device's key. Fold-status-independent (§16.2): a key resolves from ANY
/// stored candidate carrying it, whether or not it is currently accepted.
fn stored_device_pubkeys(
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

/// Add the `(fingerprint → pubkey)` an entry itself certifies: a genesis certifies its founder key
/// (the SIGNER); a DeviceAdd certifies the ADDED device's key. Both bind `sha256(pk) ==
/// fingerprint` (genesis: the header device; DeviceAdd: the op's `device_fingerprint`, enforced at
/// decode).
fn add_self_pubkey(
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
        let fingerprint = DeviceFingerprint::from_bytes(fixed(&fp)?);
        let roster_ref: EntryHash = fixed(&roster_ref)?;
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
        .map(|roster_ref| enrollment_x25519(conn, account_id, &fixed(&roster_ref)?, fingerprint))
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
    let mut stmt = conn.prepare(
        "SELECT DISTINCT device_fingerprint FROM account_roster_history
         WHERE account_id = ?1 AND closed_at IS NULL
         ORDER BY device_fingerprint",
    )?;
    let rows = stmt
        .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|fp| Ok(DeviceFingerprint::from_bytes(fixed(&fp)?))).collect()
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
    roster_ref: &EntryHash,
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
    entry_hash: &[u8; 32],
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
    enforce_pre_verify_budget(conn, account_id, &signed_hash)
}

fn pre_verify_contains(conn: &Connection, signed_hash: &EntryHash) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM account_pre_verify WHERE signed_hash = ?1)",
        params![signed_hash.as_slice()],
        |row| row.get(0),
    )
}

/// Keep the unauthenticated queue within deterministic oldest-first account and global budgets.
/// Ties use `signed_hash`, so replicas with the same rows and timestamps retain the same set.
fn enforce_pre_verify_budget(
    conn: &Connection,
    account_id: AccountId,
    inserted_signed_hash: &EntryHash,
) -> rusqlite::Result<PreVerifyInsert> {
    let mut evicted = Vec::new();
    if evict_oldest_pre_verify(conn, Some(account_id), PRE_VERIFY_PER_ACCOUNT_MAX)? > 0 {
        evicted.push(CapacityScope::PreVerifyAccount);
    }
    if !pre_verify_contains(conn, inserted_signed_hash)? {
        return Ok(PreVerifyInsert::AtCapacity(CapacityScope::PreVerifyAccount));
    }
    if evict_oldest_pre_verify(conn, None, PRE_VERIFY_GLOBAL_MAX)? > 0 {
        evicted.push(CapacityScope::PreVerifyGlobal);
    }
    if !pre_verify_contains(conn, inserted_signed_hash)? {
        return Ok(PreVerifyInsert::AtCapacity(CapacityScope::PreVerifyGlobal));
    }
    Ok(PreVerifyInsert::Parked { evicted })
}

fn evict_oldest_pre_verify(
    conn: &Connection,
    account_id: Option<AccountId>,
    limit: usize,
) -> rusqlite::Result<usize> {
    let account_bytes = account_id.map(AccountId::to_bytes);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM account_pre_verify
         WHERE (?1 IS NULL OR claimed_account_id = ?1)",
        params![account_bytes.as_ref().map(<[u8; 32]>::as_slice)],
        |row| row.get(0),
    )?;
    if count <= limit as i64 {
        return Ok(0);
    }
    let excess = count - limit as i64;
    conn.execute(
        "DELETE FROM account_pre_verify WHERE signed_hash IN (
             SELECT signed_hash FROM account_pre_verify
             WHERE (?1 IS NULL OR claimed_account_id = ?1)
             ORDER BY received_at_ms, signed_hash
             LIMIT ?2
         )",
        params![account_bytes.as_ref().map(<[u8; 32]>::as_slice), excess,],
    )
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
            let fp = DeviceFingerprint::from_bytes(fixed(&fp_bytes)?);
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
                delete_pre_verify(tx, &signed_hash)?;
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
                    delete_pre_verify(tx, &signed_hash)?;
                },
                CandidateInsert::Inserted | CandidateInsert::AlreadyPresent => {
                    // A promoted genesis/DeviceAdd certifies a device key — feed it back so the
                    // next round can resolve entries that were waiting on it.
                    add_self_pubkey(&mut pubkeys, &verified.header, &verified.payload);
                    promoted_any = true;
                    delete_pre_verify(tx, &signed_hash)?;
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

fn delete_pre_verify(conn: &Connection, signed_hash: &[u8]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM account_pre_verify WHERE signed_hash = ?1", params![signed_hash])?;
    Ok(())
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

fn validate_storable_header_payload(
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
    if account_id_from_genesis_payload(payload) != header.account_id {
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

/// A stored fixed-width blob as an array (errors, never panics, on a wrong-length value).
fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("stored blob is {} bytes, not {N}", bytes.len()))
}

/// The projected status of a single entry (§16.3), or `None` if the entry isn't stored (e.g. it is
/// still in the pre-verify queue). A read helper for queries.
pub(super) fn entry_status(
    conn: &Connection,
    entry_hash: &[u8; 32],
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
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use rag_rat_db::schema;

    use super::*;
    use crate::account::content::{
        ContentEntryHeader, ContentRefoldBudget, content_ingest, settle_pending_content_refolds,
        sign_content_entry,
    };
    use crate::account::envelope::sign_account_entry;
    use crate::account::ops::{ContentCut, DeviceCut, DeviceRole, GrantRole};
    use crate::device::{DeviceSecret, DeviceX25519Secret};
    use crate::stream::{self, StreamSpec, StreamSpecV2};

    const NOW: i64 = 1_700_000_000_000;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    #[test]
    fn effective_owner_incarnation_resolves_the_open_incarnation_not_a_closed_one() {
        // Reverse lookup by device: a device can hold a CLOSED prior incarnation and an OPEN
        // current one (demote-then-repromote). The StreamKeyWrap author cites the OPEN
        // owner_id as authority_ref — returning the closed one would cite an ineffective
        // incarnation and roll the mint back. The closed row here has the LATER
        // effective_at, so a query missing the `closed_at IS NULL` filter would wrongly
        // return it: the test bites that omission.
        let conn = db();
        let account = AccountId::from_bytes([0xa1; 32]);
        let device = DeviceFingerprint::from_bytes([0xd2; 32]);
        let open_owner = [0x22u8; 32];
        let closed_owner = [0x11u8; 32];
        conn.execute(
            "INSERT INTO account_owner_incarnations(
                 owner_id, account_id, device_fingerprint, effective_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, NULL)",
            params![
                open_owner.as_slice(),
                account.to_bytes().as_slice(),
                device.to_bytes().as_slice(),
                10_i64,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO account_owner_incarnations(
                 owner_id, account_id, device_fingerprint, effective_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                closed_owner.as_slice(),
                account.to_bytes().as_slice(),
                device.to_bytes().as_slice(),
                30_i64, // LATER than the open one — a missing closed_at filter would pick this
                40_i64,
            ],
        )
        .unwrap();

        assert_eq!(
            effective_owner_incarnation_for_device(&conn, account, device).unwrap(),
            Some(open_owner),
            "resolves the OPEN incarnation, never the closed prior one",
        );
        // A device with no open incarnation resolves None.
        let stranger = DeviceFingerprint::from_bytes([0xee; 32]);
        assert_eq!(
            effective_owner_incarnation_for_device(&conn, account, stranger).unwrap(),
            None,
            "a device with no open owner incarnation resolves None",
        );
    }

    struct Dev {
        secret: DeviceSecret,
        fp: DeviceFingerprint,
        ed: [u8; 32],
        x: [u8; 32],
    }

    impl Dev {
        fn new(seed: u8) -> Self {
            let secret = DeviceSecret::from_seed(&[seed; 32]);
            let public = secret.public();
            let x =
                DeviceX25519Secret::from_seed(&[seed.wrapping_add(0x80); 32]).public().to_bytes();
            Dev { fp: public.fingerprint(), ed: public.to_bytes(), x, secret }
        }
    }

    fn genesis(founder: &Dev) -> (AccountId, Vec<u8>, [u8; 32]) {
        let op = AccountOp::AccountGenesis {
            ed25519_pubkey: founder.ed,
            x25519_pubkey: founder.x,
            nonce16: [0u8; 16],
            created_at_ms: NOW as u64,
            label: None,
        };
        let payload = ops::encode(&op).unwrap();
        let account_id = account_id_from_genesis_payload(&payload);
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: ops::entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
        (account_id, signed.signed_bytes, signed.entry_hash)
    }

    #[allow(clippy::too_many_arguments)]
    fn op(
        account_id: AccountId,
        signer: &Dev,
        seq: u64,
        prev: Option<[u8; 32]>,
        authority_ref: Option<[u8; 32]>,
        op: &AccountOp,
    ) -> (Vec<u8>, [u8; 32]) {
        let payload = ops::encode(op).unwrap();
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: signer.fp,
            seq,
            prev_hash: prev,
            parent_ref: None,
            entry_type: ops::entry_type_of(op),
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref,
        };
        let signed = sign_account_entry(&signer.secret, &header, &payload).unwrap();
        (signed.signed_bytes, signed.entry_hash)
    }

    fn device_add(dev: &Dev, role: DeviceRole) -> AccountOp {
        AccountOp::DeviceAdd {
            device_fingerprint: dev.fp,
            ed25519_pubkey: dev.ed,
            x25519_pubkey: dev.x,
            role,
            label: None,
        }
    }

    fn stream_own(account_id: AccountId) -> (crate::stream::StreamId, AccountOp) {
        let spec = StreamSpecV2 {
            owner_account_id: account_id,
            policy: StreamSpec {
                repo_set: vec!["repo-a".to_string()],
                kind_allow_list: None,
                relation_policy: None,
                node_overrides: Vec::new(),
            },
        };
        let stream_id = stream::derive_v2(&spec).unwrap();
        let stream_spec_bytes = stream::canonical_spec_v2_bytes(&spec).unwrap();
        (stream_id, AccountOp::StreamOwn { stream_id, stream_spec_bytes })
    }

    fn device_remove(dev: &Dev, control_cut: super::super::cut::Cut) -> AccountOp {
        AccountOp::DeviceRemove {
            device_fingerprint: dev.fp,
            control_cut,
            secrets_cut: super::super::cut::Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        }
    }

    fn projected_nodes(conn: &Connection, stream_id: StreamId) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT node_id FROM content_projected_nodes
                 WHERE stream_id = ?1 ORDER BY node_id",
            )
            .unwrap();
        stmt.query_map([stream_id.to_bytes().as_slice()], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn content_verdict(conn: &Connection, entry_hash: &[u8; 32]) -> (String, i64) {
        conn.query_row(
            "SELECT s.status, e.accepted FROM content_entries e
             JOIN content_entry_status s ON s.entry_hash = e.entry_hash
             WHERE e.entry_hash = ?1",
            [entry_hash.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    fn signed_member_content(
        member: &Dev,
        account_id: AccountId,
        stream_id: StreamId,
        roster_ref: [u8; 32],
        auth_len: u64,
    ) -> crate::account::content::SignedContentEntry {
        let header = ContentEntryHeader {
            stream_id,
            author_account_id: account_id,
            device_fingerprint: member.fp,
            seq: 0,
            lamport: 1,
            prev_hash: None,
            grant_id: None,
            roster_ref,
            owner_auth_len: auth_len,
            author_auth_len: auth_len,
            crypto_suite: 0,
            key_id: None,
        };
        let op = crate::op::MemoryOp::NodeCreate {
            node_id: crate::op::NodeId::from("remote-node"),
            content: crate::op::NodeContent {
                kind: "Invariant".into(),
                title: "remote node".into(),
                body: "body".into(),
                confidence: "high".into(),
                source: "agent".into(),
                tags: Vec::new(),
                payload: None,
            },
        };
        sign_content_entry(&member.secret, &header, &crate::op::encode(&op)).unwrap()
    }

    fn owner_demote(dev: &Dev, owner_id: EntryHash) -> AccountOp {
        AccountOp::OwnerDemote {
            device_fingerprint: dev.fp,
            owner_id,
            control_cut: super::super::cut::Cut::Empty,
            secrets_cut: super::super::cut::Cut::Empty,
            reason: "demoted".to_string(),
        }
    }

    fn cut_extend_ctrl(
        account_id: AccountId,
        subject: &Dev,
        new_seq: u64,
        new_entry_hash: EntryHash,
    ) -> AccountOp {
        AccountOp::CutExtend {
            chain_kind: super::super::ops::ChainKind::Ctrl,
            stream_id: None,
            incarnation_id: None,
            subject_account_id: account_id,
            device_fingerprint: subject.fp,
            new_seq,
            new_entry_hash,
        }
    }

    fn status(conn: &Connection, hash: &[u8; 32]) -> Option<String> {
        entry_status(conn, hash).unwrap().map(|(s, _)| s)
    }

    fn seed_candidate_rows(
        conn: &Connection,
        account_id: AccountId,
        fingerprint: DeviceFingerprint,
        fixture_namespace: u64,
        count: usize,
    ) {
        for ordinal in 0..count {
            let entry_hash = cbor::sha256(
                &[
                    fixture_namespace.to_be_bytes().as_slice(),
                    u64::try_from(ordinal).unwrap().to_be_bytes().as_slice(),
                ]
                .concat(),
            );
            conn.execute(
                "INSERT INTO account_entries(
                     entry_hash, account_id, log_id, device_fingerprint, seq, entry_type,
                     accepted, signed_bytes, received_at_ms)
                 VALUES (?1, ?2, 0, ?3, ?4, 99, 0, X'00', ?4)",
                params![
                    entry_hash.as_slice(),
                    account_id.to_bytes().as_slice(),
                    fingerprint.to_bytes().as_slice(),
                    i64::try_from(ordinal).unwrap(),
                ],
            )
            .unwrap();
        }
    }

    fn seed_global_candidate_rows(conn: &Connection, mut count: usize, namespace_start: u64) {
        let fingerprint = Dev::new(9).fp;
        let mut account_ordinal = 0u64;
        while count > 0 {
            let fixture_namespace = namespace_start + account_ordinal;
            let account_id = AccountId::from_bytes(cbor::sha256(&fixture_namespace.to_be_bytes()));
            let account_count = count.min(CANDIDATES_PER_ACCOUNT_MAX);
            seed_candidate_rows(conn, account_id, fingerprint, fixture_namespace, account_count);
            count -= account_count;
            account_ordinal += 1;
        }
    }

    #[test]
    fn a_genesis_ingests_and_is_accepted() {
        let conn = db();
        let (_acct, bytes, gh) = genesis(&Dev::new(1));
        let out = account_ingest(&conn, &bytes, NOW).unwrap();
        assert_eq!(out, IngestOutcome::Ingested { status: "accepted".into() });
        assert_eq!(status(&conn, &gh).as_deref(), Some("accepted"));
    }

    #[test]
    fn refold_projects_stream_authority_and_revoke_cuts_for_keyed_queries() {
        let conn = db();
        let founder = Dev::new(1);
        let grantee_device = Dev::new(2);
        let grantee = AccountId::from_bytes([0x44; 32]);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();

        let (stream_id, own_op) = stream_own(account_id);
        let (own_bytes, own_hash) =
            op(account_id, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own_op);
        account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
        let grant_op = AccountOp::StreamGrant {
            stream_id,
            grantee_account_id: grantee,
            grant_role: GrantRole::Writer,
        };
        let (grant_bytes, grant_id) =
            op(account_id, &founder, 2, Some(own_hash), Some(genesis_hash), &grant_op);
        account_ingest(&conn, &grant_bytes, NOW + 2).unwrap();
        assert!(matches!(
            grant_effective_for_device(
                &conn,
                account_id,
                grant_id,
                stream_id,
                grantee,
                grantee_device.fp,
            )
            .unwrap(),
            fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
                boundary: fold::GrantDeviceBoundary::Open,
                ..
            }),
        ));
        let cut_hash = [0x99; 32];
        let revoke_op = AccountOp::StreamRevoke {
            stream_id,
            grantee_account_id: grantee,
            grant_id,
            device_cuts: vec![DeviceCut {
                device_fingerprint: grantee_device.fp,
                seq: u64::MAX,
                hash: cut_hash,
            }],
            reason: "access ended".to_string(),
        };
        let (revoke_bytes, _) =
            op(account_id, &founder, 3, Some(grant_id), Some(genesis_hash), &revoke_op);
        account_ingest(&conn, &revoke_bytes, NOW + 3).unwrap();

        let state: (String, i64) = conn
            .query_row(
                "SELECT classification, effective_count FROM account_auth_state
                 WHERE account_id = ?1",
                [account_id.to_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, ("live".to_string(), 4));
        let grant: (String, i64, Option<i64>) = conn
            .query_row(
                "SELECT role, effective_at, closed_at FROM account_stream_grants
                 WHERE grant_id = ?1",
                [grant_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(grant, ("writer".to_string(), 2, Some(3)));
        let cut: (Vec<u8>, Vec<u8>, Vec<u8>) = conn
            .query_row(
                "SELECT device_fingerprint, seq, entry_hash
                 FROM account_stream_grant_cuts WHERE grant_id = ?1",
                [grant_id.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            cut,
            (
                grantee_device.fp.to_bytes().to_vec(),
                u64::MAX.to_be_bytes().to_vec(),
                cut_hash.to_vec(),
            ),
        );
        assert_eq!(
            stream_owner_effective(&conn, account_id, stream_id).unwrap(),
            fold::AuthorityQuery::Effective(own_hash),
        );
        assert_eq!(
            grant_device_cut(&conn, account_id, grant_id, grantee_device.fp).unwrap(),
            fold::AuthorityQuery::Effective(Some(DeviceCut {
                device_fingerprint: grantee_device.fp,
                seq: u64::MAX,
                hash: cut_hash,
            })),
        );
        assert_eq!(
            grant_effective_for_device(
                &conn,
                account_id,
                grant_id,
                stream_id,
                grantee,
                grantee_device.fp,
            )
            .unwrap(),
            fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
                grant: fold::GrantAuthority {
                    stream_id,
                    grantee_account_id: grantee,
                    role: GrantRole::Writer,
                },
                boundary: fold::GrantDeviceBoundary::Cut(DeviceCut {
                    device_fingerprint: grantee_device.fp,
                    seq: u64::MAX,
                    hash: cut_hash,
                }),
            }),
        );
        assert_eq!(
            grant_device_cut(&conn, account_id, grant_id, Dev::new(3).fp).unwrap(),
            fold::AuthorityQuery::Effective(None),
        );
        assert!(matches!(
            grant_effective_for_device(
                &conn,
                account_id,
                grant_id,
                stream_id,
                grantee,
                Dev::new(3).fp,
            )
            .unwrap(),
            fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
                boundary: fold::GrantDeviceBoundary::Closed,
                ..
            }),
        ));
        // A citation that files a grant we DO hold under an account it does not belong to is
        // refuted by the entry's own bytes, so it is a wrong subject — not an unknown reference.
        // (The account-wide `auth_len` preflight used to mask this as `Unknown` whenever we
        // happened to hold no authority state for the claimed account.)
        assert_eq!(
            grant_device_cut(
                &conn,
                AccountId::from_bytes([0x66; 32]),
                grant_id,
                grantee_device.fp,
            )
            .unwrap(),
            fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject),
        );
        // A grant we hold nothing about stays recoverable: refetch and re-evaluate.
        assert_eq!(
            grant_device_cut(
                &conn,
                AccountId::from_bytes([0x66; 32]),
                [0x77; 32],
                grantee_device.fp,
            )
            .unwrap(),
            fold::AuthorityQuery::Unknown,
        );
        assert!(matches!(
            grant_effective(&conn, account_id, grant_id, stream_id, grantee).unwrap(),
            fold::AuthorityQuery::Effective(fold::GrantAuthority { role: GrantRole::Writer, .. })
        ));
        assert_eq!(
            grant_effective(&conn, account_id, grant_id, stream_id, grantee).unwrap(),
            fold::AuthorityQuery::Effective(fold::GrantAuthority {
                stream_id,
                grantee_account_id: grantee,
                role: GrantRole::Writer,
            }),
        );
        assert!(matches!(
            roster_ref_effective(&conn, account_id, genesis_hash, founder.fp).unwrap(),
            fold::AuthorityQuery::Effective(fold::RosterAuthority {
                current_role: DeviceRole::Owner,
                ..
            })
        ));
        assert!(matches!(
            owner_incarnation_effective(&conn, account_id, genesis_hash, founder.fp).unwrap(),
            fold::AuthorityQuery::Effective(_)
        ));
        assert_eq!(
            grant_effective(&conn, account_id, [0xaa; 32], stream_id, grantee).unwrap(),
            fold::AuthorityQuery::Unknown,
        );

        // Corrupt projection state must fail closed: an open grant cannot legitimately retain a
        // revoke cut from an older projection round.
        conn.execute(
            "UPDATE account_stream_grants SET closed_at = NULL
             WHERE owner_account_id = ?1 AND grant_id = ?2",
            params![account_id.to_bytes().as_slice(), grant_id.as_slice()],
        )
        .unwrap();
        let error = grant_effective_for_device(
            &conn,
            account_id,
            grant_id,
            stream_id,
            grantee,
            grantee_device.fp,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("open grant unexpectedly has a persisted device cut"),
            "unexpected corrupt-projection error: {error:#}",
        );
    }

    #[test]
    fn combined_grant_query_never_mixes_projection_rounds_across_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grant-snapshot.db");
        let setup = Connection::open(&path).unwrap();
        schema::apply(&setup, &crate::test_hooks()).unwrap();
        setup.execute_batch("PRAGMA journal_mode = WAL;").unwrap();

        let founder = Dev::new(1);
        let grantee_device = Dev::new(2);
        let grantee = AccountId::from_bytes([0x44; 32]);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&setup, &genesis_bytes, NOW).unwrap();
        let (stream_id, own_op) = stream_own(account_id);
        let (own_bytes, own_hash) =
            op(account_id, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own_op);
        account_ingest(&setup, &own_bytes, NOW + 1).unwrap();
        let grant_op = AccountOp::StreamGrant {
            stream_id,
            grantee_account_id: grantee,
            grant_role: GrantRole::Writer,
        };
        let (grant_bytes, grant_id) =
            op(account_id, &founder, 2, Some(own_hash), Some(genesis_hash), &grant_op);
        account_ingest(&setup, &grant_bytes, NOW + 2).unwrap();
        let cut_hash = [0x99; 32];
        let revoke_op = AccountOp::StreamRevoke {
            stream_id,
            grantee_account_id: grantee,
            grant_id,
            device_cuts: vec![DeviceCut {
                device_fingerprint: grantee_device.fp,
                seq: 7,
                hash: cut_hash,
            }],
            reason: "access ended".to_string(),
        };
        let (revoke_bytes, _) =
            op(account_id, &founder, 3, Some(grant_id), Some(genesis_hash), &revoke_op);
        account_ingest(&setup, &revoke_bytes, NOW + 3).unwrap();
        drop(setup);

        let reader = Connection::open(&path).unwrap();
        let writer = Connection::open(&path).unwrap();
        let snapshot = Transaction::new_unchecked(&reader, TransactionBehavior::Deferred).unwrap();
        assert_eq!(
            auth_len_freshness(&snapshot, account_id, 4).unwrap(),
            fold::AuthorityFreshness::CurrentOrBehind,
        );

        let write_tx = Transaction::new_unchecked(&writer, TransactionBehavior::Immediate).unwrap();
        write_tx
            .execute(
                "DELETE FROM account_stream_grant_cuts
                 WHERE owner_account_id = ?1 AND grant_id = ?2",
                params![account_id.to_bytes().as_slice(), grant_id.as_slice()],
            )
            .unwrap();
        write_tx
            .execute(
                "UPDATE account_stream_grants SET closed_at = NULL
                 WHERE owner_account_id = ?1 AND grant_id = ?2",
                params![account_id.to_bytes().as_slice(), grant_id.as_slice()],
            )
            .unwrap();
        write_tx.commit().unwrap();

        let old_round = grant_effective_for_device_in_snapshot(
            &snapshot,
            account_id,
            grant_id,
            stream_id,
            grantee,
            grantee_device.fp,
        )
        .unwrap();
        assert!(matches!(
            old_round,
            fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
                boundary: fold::GrantDeviceBoundary::Cut(DeviceCut { seq: 7, hash, .. }),
                ..
            }) if hash == cut_hash
        ));
        drop(snapshot);

        assert!(matches!(
            grant_effective_for_device(
                &reader,
                account_id,
                grant_id,
                stream_id,
                grantee,
                grantee_device.fp,
            )
            .unwrap(),
            fold::AuthorityQuery::Effective(fold::GrantDeviceAuthority {
                boundary: fold::GrantDeviceBoundary::Open,
                ..
            }),
        ));
    }

    #[test]
    fn authority_projection_failure_rolls_back_candidate_status_and_prior_shadow_state() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_stream_authority_projection
             BEFORE INSERT ON account_stream_ownership
             BEGIN SELECT RAISE(ABORT, 'injected authority projection failure'); END;",
        )
        .unwrap();
        let (_, own_op) = stream_own(account_id);
        let (own_bytes, own_hash) =
            op(account_id, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own_op);

        assert!(account_ingest(&conn, &own_bytes, NOW + 1).is_err());
        let candidate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_entries WHERE account_id = ?1",
                [account_id.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(candidate_count, 1, "the newly inserted candidate rolled back");
        assert_eq!(status(&conn, &own_hash), None, "its projected status rolled back");
        let effective_count: i64 = conn
            .query_row(
                "SELECT effective_count FROM account_auth_state WHERE account_id = ?1",
                [account_id.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(effective_count, 1, "the prior shadow projection survived intact");
    }

    #[test]
    fn remote_revoke_updates_authority_now_but_content_and_projection_only_at_settle() {
        let conn = db();
        let founder = Dev::new(0x31);
        let member = Dev::new(0x32);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();

        let (stream_id, own) = stream_own(account_id);
        let (own_bytes, own_hash) =
            op(account_id, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own);
        account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
        let (add_bytes, add_hash) = op(
            account_id,
            &founder,
            2,
            Some(own_hash),
            Some(genesis_hash),
            &device_add(&member, DeviceRole::Member),
        );
        account_ingest(&conn, &add_bytes, NOW + 2).unwrap();

        let content = signed_member_content(&member, account_id, stream_id, add_hash, 3);
        content_ingest(&conn, &content.signed_bytes, NOW + 3).unwrap();
        assert_eq!(
            settle_pending_content_refolds(&conn, &ContentRefoldBudget::unbounded())
                .unwrap()
                .settled_streams,
            1
        );
        assert_eq!(content_verdict(&conn, &content.entry_hash), ("accepted".into(), 1));
        assert_eq!(projected_nodes(&conn, stream_id), vec!["remote-node".to_string()]);

        let remove = device_remove(&member, super::super::cut::Cut::Empty);
        let (remove_bytes, _) =
            op(account_id, &founder, 3, Some(add_hash), Some(genesis_hash), &remove);
        account_ingest(&conn, &remove_bytes, NOW + 4).unwrap();

        assert!(matches!(
            roster_ref_effective(&conn, account_id, add_hash, member.fp).unwrap(),
            fold::AuthorityQuery::Invalid(_),
        ));
        assert_eq!(
            content_verdict(&conn, &content.entry_hash),
            ("accepted".into(), 1),
            "remote account ingest leaves content at the last completed fold",
        );
        assert_eq!(projected_nodes(&conn, stream_id), vec!["remote-node".to_string()]);
        let reason: i64 = conn
            .query_row(
                "SELECT reason_mask FROM content_streams_pending_refold WHERE stream_id = ?1",
                [stream_id.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, 2, "the revoke queues ACCOUNT_CHANGE");

        assert_eq!(
            settle_pending_content_refolds(&conn, &ContentRefoldBudget::unbounded())
                .unwrap()
                .settled_streams,
            1
        );
        let (status, accepted) = content_verdict(&conn, &content.entry_hash);
        assert_eq!(accepted, 0);
        assert!(status.starts_with("condemned{"), "unexpected settled verdict: {status}");
        assert!(projected_nodes(&conn, stream_id).is_empty());
    }

    #[test]
    fn trusted_revoke_projection_failure_rolls_back_authority_content_and_projection() {
        let conn = db();
        let founder = Dev::new(0x33);
        let member = Dev::new(0x34);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let (stream_id, own) = stream_own(account_id);
        let (own_bytes, own_hash) =
            op(account_id, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own);
        account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
        let (add_bytes, add_hash) = op(
            account_id,
            &founder,
            2,
            Some(own_hash),
            Some(genesis_hash),
            &device_add(&member, DeviceRole::Member),
        );
        account_ingest(&conn, &add_bytes, NOW + 2).unwrap();
        let content = signed_member_content(&member, account_id, stream_id, add_hash, 3);
        content_ingest(&conn, &content.signed_bytes, NOW + 3).unwrap();
        settle_pending_content_refolds(&conn, &ContentRefoldBudget::unbounded()).unwrap();

        conn.execute_batch(
            "CREATE TRIGGER fail_content_reproject
             BEFORE DELETE ON content_projected_nodes
             BEGIN SELECT RAISE(ABORT, 'injected content projection failure'); END;",
        )
        .unwrap();
        let remove = device_remove(&member, super::super::cut::Cut::Empty);
        let (remove_bytes, remove_hash) =
            op(account_id, &founder, 3, Some(add_hash), Some(genesis_hash), &remove);
        let signed =
            envelope::verify_account_signed(&remove_bytes, &founder.secret.public()).unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        insert_candidate(&tx, &signed, &remove_bytes, NOW + 4).unwrap();
        assert!(refold_in_tx(&tx, account_id, NOW + 4).is_err());
        drop(tx);

        assert_eq!(status(&conn, &remove_hash), None, "the trusted candidate rolled back");
        assert!(matches!(
            roster_ref_effective(&conn, account_id, add_hash, member.fp).unwrap(),
            fold::AuthorityQuery::Effective(_),
        ));
        assert_eq!(content_verdict(&conn, &content.entry_hash), ("accepted".into(), 1));
        assert_eq!(projected_nodes(&conn, stream_id), vec!["remote-node".to_string()]);
    }

    #[test]
    fn v064_forward_migration_backfills_populated_account_histories() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let (stream_id, own_op) = stream_own(account_id);
        let (own_bytes, own_hash) =
            op(account_id, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own_op);
        account_ingest(&conn, &own_bytes, NOW + 1).unwrap();

        conn.execute("DELETE FROM schema_version WHERE id = '064_account_authority_projection'", [
        ])
        .unwrap();
        conn.execute_batch(
            "DROP TABLE account_stream_grant_cuts;
             DROP TABLE account_stream_grants;
             DROP TABLE account_stream_ownership;
             DROP TABLE account_owner_incarnations;
             DROP TABLE account_roster_history;
             DROP TABLE account_auth_state;",
        )
        .unwrap();

        schema::migrate_forward(&conn, &crate::test_hooks()).unwrap();
        assert_eq!(
            stream_owner_effective(&conn, account_id, stream_id).unwrap(),
            fold::AuthorityQuery::Effective(own_hash),
        );
        assert!(matches!(
            roster_ref_effective(&conn, account_id, genesis_hash, founder.fp).unwrap(),
            fold::AuthorityQuery::Effective(fold::RosterAuthority {
                current_role: DeviceRole::Owner,
                ..
            })
        ));
        assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn v064_backfill_failure_rolls_back_ddl_projection_and_ledger_together() {
        let conn = db();
        conn.execute("DELETE FROM schema_version WHERE id = '064_account_authority_projection'", [
        ])
        .unwrap();
        conn.execute_batch(
            "DROP TABLE account_stream_grant_cuts;
             DROP TABLE account_stream_grants;
             DROP TABLE account_stream_ownership;
             DROP TABLE account_owner_incarnations;
             DROP TABLE account_roster_history;
             DROP TABLE account_auth_state;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO account_entries(
                 entry_hash, account_id, log_id, device_fingerprint, seq, entry_type,
                 accepted, signed_bytes, received_at_ms
             ) VALUES (?1, ?2, 0, ?1, 0, 99, 0, X'00', 0)",
            params![[0x11u8; 32].as_slice(), [0x22u8; 31].as_slice()],
        )
        .unwrap();

        assert!(schema::migrate_forward(&conn, &crate::test_hooks()).is_err());
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'account_auth_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0, "V064 DDL rolled back with the failed backfill");
        let ledger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_version
                 WHERE id = '064_account_authority_projection'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ledger_count, 0, "a failed projection never claims V064 was applied");
    }

    #[test]
    fn authority_queries_fail_closed_on_subject_mismatch_ahead_state_and_corrupt_rows() {
        let conn = db();
        let founder = Dev::new(1);
        let grantee = AccountId::from_bytes([0x44; 32]);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let (stream_id, own_op) = stream_own(account_id);
        let (own_bytes, own_hash) =
            op(account_id, &founder, 1, Some(genesis_hash), Some(genesis_hash), &own_op);
        account_ingest(&conn, &own_bytes, NOW + 1).unwrap();
        let (grant_bytes, grant_id) = op(
            account_id,
            &founder,
            2,
            Some(own_hash),
            Some(genesis_hash),
            &AccountOp::StreamGrant {
                stream_id,
                grantee_account_id: grantee,
                grant_role: GrantRole::Reader,
            },
        );
        account_ingest(&conn, &grant_bytes, NOW + 2).unwrap();
        let (self_grant_bytes, self_grant_id) = op(
            account_id,
            &founder,
            3,
            Some(grant_id),
            Some(genesis_hash),
            &AccountOp::StreamGrant {
                stream_id,
                grantee_account_id: account_id,
                grant_role: GrantRole::Reader,
            },
        );
        account_ingest(&conn, &self_grant_bytes, NOW + 3).unwrap();

        // Freshness is its own seam: an ahead assertion is measured against the fold, and it does
        // NOT reach into the fact queries — the grant resolves from the current fold either way, so
        // an ahead counter can neither hide nor manufacture an authority verdict.
        assert_eq!(
            auth_len_freshness(&conn, account_id, 99).unwrap(),
            fold::AuthorityFreshness::Ahead,
        );
        assert_eq!(
            auth_len_freshness(&conn, account_id, 3).unwrap(),
            fold::AuthorityFreshness::CurrentOrBehind,
        );
        assert!(matches!(
            grant_effective(&conn, account_id, grant_id, stream_id, grantee).unwrap(),
            fold::AuthorityQuery::Effective(_),
        ));
        assert_eq!(
            auth_len_freshness(&conn, AccountId::from_bytes([0x7e; 32]), 1).unwrap(),
            fold::AuthorityFreshness::Ahead,
            "an account we hold nothing for has folded zero effective ops",
        );
        assert_eq!(
            grant_effective(
                &conn,
                account_id,
                grant_id,
                crate::stream::StreamId::from_bytes([0x55; 32]),
                grantee,
            )
            .unwrap(),
            fold::AuthorityQuery::Invalid(fold::AuthorityInvalidReason::WrongSubject),
        );
        assert_eq!(
            grant_effective(&conn, account_id, self_grant_id, stream_id, account_id).unwrap(),
            fold::AuthorityQuery::Invalid(
                fold::AuthorityInvalidReason::ReferencedEntryNotEffective,
            ),
        );

        conn.execute("UPDATE account_stream_grants SET role = 'admin' WHERE grant_id = ?1", [
            grant_id.as_slice(),
        ])
        .unwrap();
        assert!(grant_effective(&conn, account_id, grant_id, stream_id, grantee).is_err());
        conn.execute(
            "UPDATE account_stream_grants SET role = 'reader', effective_at = -1
             WHERE grant_id = ?1",
            [grant_id.as_slice()],
        )
        .unwrap();
        assert!(grant_effective(&conn, account_id, grant_id, stream_id, grantee).is_err());
        conn.execute("UPDATE account_stream_grants SET effective_at = 2 WHERE grant_id = ?1", [
            grant_id.as_slice(),
        ])
        .unwrap();
        conn.execute(
            "UPDATE account_stream_ownership SET own_id = ?2 WHERE stream_id = ?1",
            params![stream_id.to_bytes().as_slice(), [0u8; 31].as_slice()],
        )
        .unwrap();
        assert!(stream_owner_effective(&conn, account_id, stream_id).is_err());
        conn.execute("UPDATE account_auth_state SET effective_count = -1 WHERE account_id = ?1", [
            account_id.to_bytes().as_slice(),
        ])
        .unwrap();
        assert!(auth_len_freshness(&conn, account_id, 0).is_err());
    }

    #[test]
    fn malformed_envelopes_and_wrong_genesis_self_hashes_never_touch_storage() {
        let conn = db();
        assert!(matches!(account_ingest(&conn, &[0xff], NOW).unwrap(), IngestOutcome::Rejected(_)));

        let founder = Dev::new(1);
        let genesis_op = AccountOp::AccountGenesis {
            ed25519_pubkey: founder.ed,
            x25519_pubkey: founder.x,
            nonce16: [0u8; 16],
            created_at_ms: NOW as u64,
            label: None,
        };
        let payload = ops::encode(&genesis_op).unwrap();
        let wrong_account = AccountId::from_bytes([0x55; 32]);
        assert_ne!(wrong_account, account_id_from_genesis_payload(&payload));
        let header = AccountEntryHeader {
            account_id: wrong_account,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: ops::entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
        assert_eq!(
            account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
            IngestOutcome::Rejected("genesis payload does not hash to its account_id".into()),
        );
        let stored: i64 =
            conn.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
        assert_eq!(stored, 0, "structural rejects never create candidate rows");
    }

    #[test]
    fn a_forged_re_signed_genesis_is_rejected_not_stored() {
        // A non-owner re-signs the victim's genesis payload under its own key: the fold binds the
        // founder key, so ingest verifies but the fold classifies the forgery non-effective. (The
        // signature verifies under the attacker key AND fingerprint matches — the takeover defence
        // is the fold's founder binding, exercised end-to-end through ingest.)
        let conn = db();
        let (acct, real_bytes, real_gh) = genesis(&Dev::new(1));
        account_ingest(&conn, &real_bytes, NOW).unwrap();
        // The attacker re-signs the SAME payload (same account_id) under its own key.
        let attacker = Dev::new(9);
        let victim = Dev::new(1);
        let op = AccountOp::AccountGenesis {
            ed25519_pubkey: victim.ed,
            x25519_pubkey: victim.x,
            nonce16: [0u8; 16],
            created_at_ms: NOW as u64,
            label: None,
        };
        let payload = ops::encode(&op).unwrap();
        let header = AccountEntryHeader {
            account_id: acct,
            log_id: 0,
            device_fingerprint: attacker.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: ops::entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(&attacker.secret, &header, &payload).unwrap();
        account_ingest(&conn, &signed.signed_bytes, NOW).unwrap();
        assert_eq!(
            status(&conn, &real_gh).as_deref(),
            Some("accepted"),
            "the real founder holds it"
        );
        assert_ne!(
            status(&conn, &signed.entry_hash).as_deref(),
            Some("accepted"),
            "the forged re-signed genesis never becomes the accepted root",
        );
    }

    #[test]
    fn an_owner_added_after_genesis_is_accepted() {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        let b = Dev::new(2);
        let (add_bytes, add_hash) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));
        let out = account_ingest(&conn, &add_bytes, NOW).unwrap();
        assert_eq!(out, IngestOutcome::Ingested { status: "accepted".into() });
        assert_eq!(status(&conn, &add_hash).as_deref(), Some("accepted"));
    }

    #[test]
    fn an_entry_whose_device_is_unknown_is_pre_verified_then_promoted() {
        // Ingest a founder-signed DeviceAdd BEFORE the genesis: the founder's key isn't resolvable,
        // so it parks in pre-verify. The genesis arrival resolves the founder and promotes it.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        let b = Dev::new(2);
        let (add_bytes, add_hash) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));

        // The add arrives first — unresolvable device → pre-verify.
        assert_eq!(account_ingest(&conn, &add_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
        assert_eq!(status(&conn, &add_hash), None, "not yet a stored candidate");

        // The genesis resolves the founder and promotes the queued add.
        account_ingest(&conn, &gbytes, NOW).unwrap();
        assert_eq!(status(&conn, &gh).as_deref(), Some("accepted"));
        assert_eq!(
            status(&conn, &add_hash).as_deref(),
            Some("accepted"),
            "the queued add promoted"
        );
        let pending: i64 =
            conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |r| r.get(0)).unwrap();
        assert_eq!(pending, 0, "the pre-verify queue is drained");
    }

    #[test]
    fn pre_verify_budget_evicts_oldest_per_account_and_globally() {
        let conn = db();
        let account_a = AccountId::from_bytes([0xa1; 32]);
        for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX + 2 {
            let raw = ordinal.to_be_bytes();
            insert_pre_verify(
                &conn,
                &cbor::sha256(&raw),
                account_a,
                Dev::new(2).fp,
                &raw,
                ordinal as i64,
            )
            .unwrap();
        }
        let account_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_pre_verify WHERE claimed_account_id = ?1",
                params![account_a.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(account_count, PRE_VERIFY_PER_ACCOUNT_MAX as i64);
        let oldest_remaining: i64 = conn
            .query_row(
                "SELECT MIN(received_at_ms) FROM account_pre_verify
                 WHERE claimed_account_id = ?1",
                params![account_a.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(oldest_remaining, 2, "the oldest account rows are evicted first");

        for account_byte in 0xb0..0xb4 {
            let account = AccountId::from_bytes([account_byte; 32]);
            for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX {
                let raw = [account_byte, ordinal as u8];
                insert_pre_verify(
                    &conn,
                    &cbor::sha256(&raw),
                    account,
                    Dev::new(3).fp,
                    &raw,
                    1_000 + i64::try_from(ordinal).unwrap(),
                )
                .unwrap();
            }
        }
        let global_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0))
            .unwrap();
        assert_eq!(global_count, PRE_VERIFY_GLOBAL_MAX as i64);
        let account_a_remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_pre_verify WHERE claimed_account_id = ?1",
                params![account_a.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(account_a_remaining, 0, "global eviction removes the globally oldest rows");

        let evicted_raw = [0xfe, 0xed];
        assert_eq!(
            insert_pre_verify(
                &conn,
                &cbor::sha256(&evicted_raw),
                AccountId::from_bytes([0xcc; 32]),
                Dev::new(4).fp,
                &evicted_raw,
                -1,
            )
            .unwrap(),
            PreVerifyInsert::AtCapacity(CapacityScope::PreVerifyGlobal),
            "the insert result reports which budget evicted the new row",
        );
    }

    #[test]
    fn pre_verify_oldest_ties_are_broken_by_signed_hash() {
        let conn = db();
        let account_id = AccountId::from_bytes([0xd1; 32]);
        let mut expected_hashes = Vec::new();
        for ordinal in 0..=PRE_VERIFY_PER_ACCOUNT_MAX {
            let raw = ordinal.to_be_bytes();
            let signed_hash = cbor::sha256(&raw);
            expected_hashes.push(signed_hash);
            insert_pre_verify(
                &conn,
                &cbor::sha256(&signed_hash),
                account_id,
                Dev::new(2).fp,
                &raw,
                NOW,
            )
            .unwrap();
        }
        expected_hashes.sort_unstable();
        expected_hashes.remove(0);
        let mut retained = conn
            .prepare(
                "SELECT signed_hash FROM account_pre_verify
                 WHERE claimed_account_id = ?1 ORDER BY signed_hash",
            )
            .unwrap()
            .query_map(params![account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|row| fixed(&row.unwrap()).unwrap())
            .collect::<Vec<_>>();
        retained.sort_unstable();
        assert_eq!(retained, expected_hashes);
    }

    #[test]
    fn account_ingest_reports_the_budget_that_evicted_its_pre_verify_row() {
        let founder = Dev::new(1);
        let (account_id, _genesis_bytes, genesis_hash) = genesis(&founder);
        let added = Dev::new(2);
        let (pending_bytes, _) = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&added, DeviceRole::Owner),
        );

        let per_account = db();
        for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX {
            let raw = ordinal.to_be_bytes();
            assert_eq!(
                insert_pre_verify(
                    &per_account,
                    &cbor::sha256(&raw),
                    account_id,
                    Dev::new(3).fp,
                    &raw,
                    NOW + 1,
                )
                .unwrap(),
                PreVerifyInsert::Parked { evicted: Vec::new() },
            );
        }
        assert_eq!(
            account_ingest(&per_account, &pending_bytes, NOW).unwrap(),
            IngestOutcome::CapacityReached { scope: CapacityScope::PreVerifyAccount },
        );

        let global = db();
        for account_byte in 0..PRE_VERIFY_GLOBAL_MAX / PRE_VERIFY_PER_ACCOUNT_MAX {
            let parked_account = AccountId::from_bytes([account_byte as u8; 32]);
            for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX {
                let raw = [account_byte as u8, ordinal as u8];
                assert_eq!(
                    insert_pre_verify(
                        &global,
                        &cbor::sha256(&raw),
                        parked_account,
                        Dev::new(4).fp,
                        &raw,
                        NOW + 1,
                    )
                    .unwrap(),
                    PreVerifyInsert::Parked { evicted: Vec::new() },
                );
            }
        }
        assert_eq!(
            account_ingest(&global, &pending_bytes, NOW).unwrap(),
            IngestOutcome::CapacityReached { scope: CapacityScope::PreVerifyGlobal },
        );
    }

    #[test]
    fn newer_pre_verify_admission_surfaces_collateral_oldest_eviction() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, _genesis_bytes, genesis_hash) = genesis(&founder);
        let added = Dev::new(2);
        let (pending_bytes, _) = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&added, DeviceRole::Owner),
        );
        let mut oldest_signed_hash = None;
        for ordinal in 0..PRE_VERIFY_PER_ACCOUNT_MAX {
            let raw = ordinal.to_be_bytes();
            oldest_signed_hash.get_or_insert_with(|| cbor::sha256(&raw));
            assert_eq!(
                insert_pre_verify(
                    &conn,
                    &cbor::sha256(&raw),
                    account_id,
                    Dev::new(3).fp,
                    &raw,
                    NOW + i64::try_from(ordinal).unwrap(),
                )
                .unwrap(),
                PreVerifyInsert::Parked { evicted: Vec::new() },
            );
        }

        assert_eq!(
            account_ingest(&conn, &pending_bytes, NOW + 1_000).unwrap(),
            IngestOutcome::PreVerifyWithEviction { scopes: vec![CapacityScope::PreVerifyAccount] },
        );
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_pre_verify WHERE claimed_account_id = ?1",
                params![account_id.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, PRE_VERIFY_PER_ACCOUNT_MAX as i64);
        assert!(pre_verify_contains(&conn, &cbor::sha256(&pending_bytes)).unwrap());
        assert!(!pre_verify_contains(&conn, &oldest_signed_hash.unwrap()).unwrap());
    }

    #[test]
    fn candidate_ceiling_rejects_new_history_before_refold_work_grows_unbounded() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, _) = genesis(&founder);
        let verified =
            envelope::verify_account_signed(&genesis_bytes, &founder.secret.public()).unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        seed_candidate_rows(&tx, account_id, founder.fp, 0, CANDIDATES_PER_ACCOUNT_MAX);
        assert_eq!(
            insert_candidate(&tx, &verified, &genesis_bytes, NOW).unwrap(),
            CandidateInsert::AtCapacity(CapacityScope::CandidateAccount),
        );
        tx.commit().unwrap();
        assert_eq!(
            account_ingest(&conn, &genesis_bytes, NOW).unwrap(),
            IngestOutcome::CapacityReached { scope: CapacityScope::CandidateAccount },
        );
    }

    #[test]
    fn candidate_byte_budgets_bound_refold_materialization_even_below_count_limits() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, _) = genesis(&founder);
        let verified =
            envelope::verify_account_signed(&genesis_bytes, &founder.secret.public()).unwrap();

        seed_candidate_rows(&conn, account_id, founder.fp, 0, 1);
        conn.execute("UPDATE account_entries SET signed_bytes = zeroblob(?1)", [i64::try_from(
            CANDIDATE_BYTES_PER_ACCOUNT_MAX - genesis_bytes.len() + 1,
        )
        .unwrap()])
            .unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        assert_eq!(
            insert_candidate(&tx, &verified, &genesis_bytes, NOW).unwrap(),
            CandidateInsert::AtCapacity(CapacityScope::CandidateAccountBytes),
        );
        tx.rollback().unwrap();

        let conn = db();
        let other_account = AccountId::from_bytes([0x55; 32]);
        seed_candidate_rows(&conn, other_account, founder.fp, 0, 1);
        conn.execute("UPDATE account_entries SET signed_bytes = zeroblob(?1)", [i64::try_from(
            CANDIDATE_BYTES_GLOBAL_MAX - genesis_bytes.len() + 1,
        )
        .unwrap()])
            .unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        assert_eq!(
            insert_candidate(&tx, &verified, &genesis_bytes, NOW).unwrap(),
            CandidateInsert::AtCapacity(CapacityScope::CandidateGlobalBytes),
        );
        tx.rollback().unwrap();
    }

    #[test]
    fn global_candidate_ceiling_bounds_many_attacker_created_accounts() {
        let conn = db();
        seed_global_candidate_rows(&conn, CANDIDATES_GLOBAL_MAX, 0);

        let founder = Dev::new(1);
        let (_account_id, genesis_bytes, _) = genesis(&founder);
        assert_eq!(
            account_ingest(&conn, &genesis_bytes, NOW).unwrap(),
            IngestOutcome::CapacityReached { scope: CapacityScope::CandidateGlobal },
        );
        let stored: i64 =
            conn.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
        assert_eq!(stored, CANDIDATES_GLOBAL_MAX as i64);
    }

    #[test]
    fn concurrent_candidate_admission_cannot_overshoot_the_global_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account-cap.db");
        let setup = Connection::open(&path).unwrap();
        schema::apply(&setup, &crate::test_hooks()).unwrap();
        let tx = Transaction::new_unchecked(&setup, TransactionBehavior::Immediate).unwrap();
        seed_global_candidate_rows(&tx, CANDIDATES_GLOBAL_MAX - 1, 0);
        tx.commit().unwrap();
        drop(setup);

        let first = genesis(&Dev::new(1)).1;
        let second = genesis(&Dev::new(2)).1;
        let barrier = Arc::new(Barrier::new(2));
        let handles = [first, second].map(|bytes| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(Duration::from_secs(5)).unwrap();
                barrier.wait();
                account_ingest(&conn, &bytes, NOW).unwrap()
            })
        });
        let outcomes = handles.map(|handle| handle.join().unwrap());
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, IngestOutcome::Ingested { .. }))
                .count(),
            1,
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, IngestOutcome::CapacityReached {
                    scope: CapacityScope::CandidateGlobal,
                }))
                .count(),
            1,
        );
        let conn = Connection::open(path).unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM account_entries", [], |row| row.get(0)).unwrap();
        assert_eq!(count, CANDIDATES_GLOBAL_MAX as i64);
    }

    #[test]
    fn a_bad_known_key_signature_is_rejected_without_waiting_for_the_writer_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known-key.db");
        let setup = Connection::open(&path).unwrap();
        schema::apply(&setup, &crate::test_hooks()).unwrap();
        let founder = Dev::new(1);
        let (_account_id, mut forged, _) = genesis(&founder);
        account_ingest(&setup, &forged, NOW).unwrap();
        *forged.last_mut().unwrap() ^= 1;

        let holder = Connection::open(&path).unwrap();
        let tx = Transaction::new_unchecked(&holder, TransactionBehavior::Immediate).unwrap();
        let contender = Connection::open(&path).unwrap();
        contender.busy_timeout(Duration::ZERO).unwrap();
        assert!(matches!(
            account_ingest(&contender, &forged, NOW + 1).unwrap(),
            IngestOutcome::Rejected(_),
        ));
        tx.rollback().unwrap();
    }

    #[test]
    fn ingest_reports_valid_promotions_rejected_at_terminal_global_capacity() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();

        let pending_device = Dev::new(2);
        let (pending_bytes, pending_hash) = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&pending_device, DeviceRole::Owner),
        );
        assert_eq!(
            insert_pre_verify(&conn, &pending_hash, account_id, founder.fp, &pending_bytes, NOW,)
                .unwrap(),
            PreVerifyInsert::Parked { evicted: Vec::new() },
        );

        // Leave exactly one global slot. The trigger occupies it, so promotion must remove and
        // identify the valid row that cannot enter terminal grow-only candidate storage.
        seed_global_candidate_rows(&conn, CANDIDATES_GLOBAL_MAX - 2, 100);
        let trigger_device = Dev::new(3);
        let (trigger_bytes, trigger_hash) = op(
            account_id,
            &founder,
            2,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&trigger_device, DeviceRole::Member),
        );
        let outcome = account_ingest(&conn, &trigger_bytes, NOW + 1).unwrap();
        assert_eq!(outcome, IngestOutcome::IngestedWithRejectedPromotions {
            status: "forked".into(),
            scope: CapacityScope::CandidateGlobal,
            entry_hashes: vec![pending_hash],
        },);
        assert!(!pre_verify_contains(&conn, &cbor::sha256(&pending_bytes)).unwrap());
        assert_eq!(status(&conn, &trigger_hash), Some("forked".into()));
        assert_eq!(status(&conn, &pending_hash), None, "the rejected row was not half-promoted");
    }

    #[test]
    fn exact_candidate_redelivery_is_idempotent_at_capacity_but_a_re_signature_is_verified() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        assert_eq!(account_ingest(&conn, &genesis_bytes, NOW).unwrap(), IngestOutcome::Ingested {
            status: "accepted".into()
        },);
        seed_candidate_rows(&conn, account_id, founder.fp, 1, CANDIDATES_PER_ACCOUNT_MAX - 1);
        assert_eq!(
            account_ingest(&conn, &genesis_bytes, NOW + 1).unwrap(),
            IngestOutcome::Ingested { status: "accepted".into() },
        );

        let mut forged_envelope = genesis_bytes.clone();
        *forged_envelope.last_mut().unwrap() ^= 1;
        let forged = envelope::decode_account_signed(&forged_envelope).unwrap();
        assert_eq!(forged.entry_hash, genesis_hash, "signature bytes are outside the entry hash");
        assert!(matches!(
            account_ingest(&conn, &forged_envelope, NOW + 2).unwrap(),
            IngestOutcome::Rejected(_)
        ));
    }

    #[test]
    fn exact_redelivery_repairs_a_missing_projection_row() {
        let conn = db();
        let founder = Dev::new(1);
        let (_account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        conn.execute("DELETE FROM account_entry_status WHERE entry_hash = ?1", params![
            genesis_hash.as_slice()
        ])
        .unwrap();

        assert_eq!(
            account_ingest(&conn, &genesis_bytes, NOW + 1).unwrap(),
            IngestOutcome::Ingested { status: "accepted".into() },
        );
        assert_eq!(status(&conn, &genesis_hash), Some("accepted".into()));
    }

    #[test]
    fn capacity_blocked_promotion_returns_redelivery_hash_and_clears_terminal_queue_state() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let added = Dev::new(2);
        let (pending_bytes, pending_hash) = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&added, DeviceRole::Owner),
        );
        assert_eq!(
            insert_pre_verify(&conn, &pending_hash, account_id, founder.fp, &pending_bytes, NOW,)
                .unwrap(),
            PreVerifyInsert::Parked { evicted: Vec::new() },
        );
        seed_candidate_rows(&conn, account_id, founder.fp, 2, CANDIDATES_PER_ACCOUNT_MAX - 1);

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        assert_eq!(promote_pre_verify(&tx, account_id, NOW + 1).unwrap(), PromotionOutcome {
            scope: Some(CapacityScope::CandidateAccount),
            entry_hashes: vec![pending_hash],
        },);
        tx.commit().unwrap();
        assert!(!pre_verify_contains(&conn, &cbor::sha256(&pending_bytes)).unwrap());
        assert_eq!(status(&conn, &pending_hash), None, "the blocked row was not half-promoted");
    }

    #[test]
    fn promotion_at_the_last_slot_is_stable_across_opposite_arrival_orders() {
        let run = |reverse: bool| {
            let conn = db();
            let founder = Dev::new(1);
            let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
            account_ingest(&conn, &genesis_bytes, NOW).unwrap();
            let a = Dev::new(2);
            let b = Dev::new(3);
            let first = op(
                account_id,
                &founder,
                1,
                Some(genesis_hash),
                Some(genesis_hash),
                &device_add(&a, DeviceRole::Member),
            );
            let second = op(
                account_id,
                &founder,
                2,
                Some(genesis_hash),
                Some(genesis_hash),
                &device_add(&b, DeviceRole::Member),
            );
            let rows = if reverse { [&second, &first] } else { [&first, &second] };
            for (bytes, hash) in rows {
                assert_eq!(
                    insert_pre_verify(&conn, hash, account_id, founder.fp, bytes, NOW).unwrap(),
                    PreVerifyInsert::Parked { evicted: Vec::new() },
                );
            }
            seed_candidate_rows(&conn, account_id, founder.fp, 50, CANDIDATES_PER_ACCOUNT_MAX - 2);
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
            let outcome = promote_pre_verify(&tx, account_id, NOW + 1).unwrap();
            tx.commit().unwrap();
            let admitted = [first.1, second.1]
                .into_iter()
                .find(|hash| {
                    conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM account_entries WHERE entry_hash = ?1)",
                        params![hash.as_slice()],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap()
                })
                .unwrap();
            (admitted, outcome)
        };

        let forward = run(false);
        let reverse = run(true);
        assert_eq!(forward, reverse);
        assert_eq!(forward.1.scope, Some(CapacityScope::CandidateAccount));
        assert_eq!(forward.1.entry_hashes.len(), 1);
    }

    #[test]
    fn a_depth_two_pre_verify_chain_promotes_to_a_fixpoint_when_the_root_arrives() {
        // A device chain delivered before its authorizers: founder→B (DeviceAdd signed by the
        // founder) →C (DeviceAdd signed by B). Deliver in REVERSE so both the C-add and the B-add
        // park. The genesis arrival resolves the founder and promotes the B-add; the newly added B
        // key must then resolve the parked C-add in the SAME drain (the fixpoint). A one-shot
        // device-set snapshot would strand the C-add forever — a peer that received the three
        // entries in forward order would accept it, so the two peers would diverge.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        let b = Dev::new(2);
        let (add_b_bytes, add_b) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));
        // C added by owner B on B's own log (seq 0), authorized by the entry that made B an owner.
        let c = Dev::new(3);
        let (add_c_bytes, add_c) =
            op(acct, &b, 0, None, Some(add_b), &device_add(&c, DeviceRole::Member));

        // Reverse delivery: the C-add parks (B unknown), then the B-add parks (founder unknown).
        assert_eq!(account_ingest(&conn, &add_c_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
        assert_eq!(account_ingest(&conn, &add_b_bytes, NOW).unwrap(), IngestOutcome::PreVerify);

        // The genesis resolves the founder → promotes add_b → the fed-back B key promotes add_c.
        account_ingest(&conn, &gbytes, NOW).unwrap();
        assert_eq!(status(&conn, &gh).as_deref(), Some("accepted"), "genesis accepted");
        assert_eq!(
            status(&conn, &add_b).as_deref(),
            Some("accepted"),
            "the B-add promoted (depth 1)"
        );
        assert_eq!(
            status(&conn, &add_c).as_deref(),
            Some("accepted"),
            "the C-add promoted transitively (depth 2 — the fixpoint drain)",
        );
        let pending: i64 =
            conn.query_row("SELECT COUNT(*) FROM account_pre_verify", [], |r| r.get(0)).unwrap();
        assert_eq!(pending, 0, "the pre-verify queue is fully drained");
    }

    #[test]
    fn a_cut_promotes_its_named_branch_regardless_of_arrival_order_p8() {
        // B equivocates at seq 0 (two heads b0a/b0b). A DeviceRemove(B) names b0b as its watermark:
        // the register condemns the OFF-branch head (b0a) and keeps b0b — promoting the cut's
        // chosen branch over the unforced-fork hash tiebreak. The verdict is identical
        // whichever head (or the cut) arrives first (P8 arrival-independence, I10a holds
        // throughout).
        for order in 0..3u8 {
            let conn = db();
            let founder = Dev::new(1);
            let (acct, gbytes, gh) = genesis(&founder);
            let b = Dev::new(2);
            let (add_bytes, add_b) =
                op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));
            // B's two equivocating seq-0 heads (each adds a throwaway member so they differ).
            let (t8, t9) = (Dev::new(8), Dev::new(9));
            let (b0a_bytes, b0a) =
                op(acct, &b, 0, None, Some(add_b), &device_add(&t8, DeviceRole::Member));
            let (b0b_bytes, b0b) =
                op(acct, &b, 0, None, Some(add_b), &device_add(&t9, DeviceRole::Member));
            // The founder removes B, watermark = b0b (keep b0b's branch).
            let (rm_bytes, _rm) = op(
                acct,
                &founder,
                2,
                Some(add_b),
                Some(gh),
                &device_remove(&b, super::super::cut::Cut::At { seq: 0, hash: b0b }),
            );

            // Ingest genesis + add_b first (so B resolves), then the three in a rotated order.
            account_ingest(&conn, &gbytes, NOW).unwrap();
            account_ingest(&conn, &add_bytes, NOW).unwrap();
            let mut rest = [&b0a_bytes, &b0b_bytes, &rm_bytes];
            rest.rotate_left(order as usize);
            for bytes in rest {
                account_ingest(&conn, bytes, NOW).unwrap();
            }

            assert_eq!(
                status(&conn, &b0b).as_deref(),
                Some("accepted"),
                "the cut-named branch b0b is accepted (order {order})",
            );
            assert_eq!(
                status(&conn, &b0a).as_deref(),
                Some("condemned"),
                "the off-branch head b0a is condemned (order {order})",
            );
        }
    }

    #[test]
    fn late_cut_and_extend_atomically_remove_then_restore_authority_shadow_rows() {
        let conn = db();
        let (founder, owner, d, e, g) =
            (Dev::new(1), Dev::new(2), Dev::new(5), Dev::new(6), Dev::new(7));
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let (add_owner_bytes, add_owner) = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&owner, DeviceRole::Owner),
        );
        account_ingest(&conn, &add_owner_bytes, NOW + 1).unwrap();
        let (b0_bytes, b0) =
            op(account_id, &owner, 0, None, Some(add_owner), &device_add(&d, DeviceRole::Member));
        let (b1_bytes, b1) = op(
            account_id,
            &owner,
            1,
            Some(b0),
            Some(add_owner),
            &device_add(&e, DeviceRole::Member),
        );
        let (b2_bytes, b2) = op(
            account_id,
            &owner,
            2,
            Some(b1),
            Some(add_owner),
            &device_add(&g, DeviceRole::Member),
        );
        for (offset, bytes) in [&b0_bytes, &b1_bytes, &b2_bytes].into_iter().enumerate() {
            account_ingest(&conn, bytes, NOW + 2 + i64::try_from(offset).unwrap()).unwrap();
        }
        assert!(matches!(
            roster_ref_effective(&conn, account_id, b2, g.fp).unwrap(),
            fold::AuthorityQuery::Effective(_)
        ));

        let (remove_bytes, remove_hash) = op(
            account_id,
            &founder,
            2,
            Some(add_owner),
            Some(genesis_hash),
            &device_remove(&owner, super::super::cut::Cut::At { seq: 0, hash: b0 }),
        );
        account_ingest(&conn, &remove_bytes, NOW + 5).unwrap();
        assert_eq!(status(&conn, &b1).as_deref(), Some("condemned"));
        assert_eq!(status(&conn, &b2).as_deref(), Some("condemned"));
        assert_eq!(
            roster_ref_effective(&conn, account_id, b2, g.fp).unwrap(),
            fold::AuthorityQuery::Invalid(
                fold::AuthorityInvalidReason::ReferencedEntryNotEffective
            ),
        );

        let (extend_bytes, _) = op(
            account_id,
            &founder,
            3,
            Some(remove_hash),
            Some(genesis_hash),
            &cut_extend_ctrl(account_id, &owner, 2, b2),
        );
        account_ingest(&conn, &extend_bytes, NOW + 6).unwrap();
        assert_eq!(status(&conn, &b1).as_deref(), Some("accepted"));
        assert_eq!(status(&conn, &b2).as_deref(), Some("accepted"));
        assert!(matches!(
            roster_ref_effective(&conn, account_id, b2, g.fp).unwrap(),
            fold::AuthorityQuery::Effective(_)
        ));
        assert_eq!(
            owner_control_authority(&conn, account_id, add_owner, owner.fp).unwrap(),
            fold::AuthorityQuery::Effective(fold::OwnerChainAuthority {
                owner: fold::OwnerAuthority { device_fingerprint: owner.fp },
                device_boundary: fold::AuthorityBoundary::Cut { seq: 2, hash: b2 },
                incarnation_boundary: fold::AuthorityBoundary::Open,
            }),
            "the query exposes the final joined device register and the independent incarnation",
        );

        // Simulate an already-populated V064 projection upgrading: additive columns begin at
        // their legacy-safe Open default, then V065's same-transaction refold must replace them
        // before its ledger stamp commits.
        conn.execute("DELETE FROM schema_version WHERE id = '065_account_authority_boundaries'", [
        ])
        .unwrap();
        conn.execute(
            "UPDATE account_roster_history
             SET control_boundary = 'open', control_seq = NULL, control_hash = NULL
             WHERE roster_ref = ?1",
            [add_owner.as_slice()],
        )
        .unwrap();
        schema::migrate_forward(&conn, &crate::test_hooks()).unwrap();
        assert!(matches!(
            owner_control_authority(&conn, account_id, add_owner, owner.fp).unwrap(),
            fold::AuthorityQuery::Effective(fold::OwnerChainAuthority {
                device_boundary: fold::AuthorityBoundary::Cut { seq: 2, hash },
                ..
            }) if hash == b2
        ));
    }

    #[test]
    fn contested_fold_persists_the_state_before_depth_and_halts_authority_mutation() {
        let conn = db();
        let (founder, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let (add_a_bytes, add_a) = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&a, DeviceRole::Owner),
        );
        let (add_b_bytes, add_b) = op(
            account_id,
            &founder,
            2,
            Some(add_a),
            Some(genesis_hash),
            &device_add(&b, DeviceRole::Owner),
        );
        account_ingest(&conn, &add_a_bytes, NOW + 1).unwrap();
        account_ingest(&conn, &add_b_bytes, NOW + 2).unwrap();
        let (remove_b_bytes, remove_b) = op(
            account_id,
            &a,
            0,
            None,
            Some(add_a),
            &device_remove(&b, super::super::cut::Cut::Empty),
        );
        let (remove_a_bytes, remove_a) = op(
            account_id,
            &b,
            0,
            None,
            Some(add_b),
            &device_remove(&a, super::super::cut::Cut::Empty),
        );
        account_ingest(&conn, &remove_b_bytes, NOW + 3).unwrap();
        account_ingest(&conn, &remove_a_bytes, NOW + 4).unwrap();

        let state: (String, Option<i64>, Option<Vec<u8>>, i64) = conn
            .query_row(
                "SELECT classification, contested_depth, successor_account_id, effective_count
                 FROM account_auth_state WHERE account_id = ?1",
                [account_id.to_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, ("contested".to_string(), Some(1), None, 3));
        assert_eq!(status(&conn, &remove_a).as_deref(), Some("parked"));
        assert_eq!(status(&conn, &remove_b).as_deref(), Some("parked"));
        let owner_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_owner_incarnations WHERE account_id = ?1",
                [account_id.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner_count, 3, "only state-before owner incarnations are projected");
    }

    #[test]
    fn owner_query_preserves_and_extends_both_registers_independently() {
        let conn = db();
        let founder = Dev::new(1);
        let owner = Dev::new(2);
        let (account, genesis_bytes, genesis) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let (add_bytes, owner_id) = op(
            account,
            &founder,
            1,
            Some(genesis),
            Some(genesis),
            &device_add(&owner, DeviceRole::Owner),
        );
        account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
        let mut prev = None;
        let mut heads = Vec::new();
        for (seq, seed) in [(0, 10), (1, 11), (2, 12)] {
            let member = Dev::new(seed);
            let (bytes, hash) = op(
                account,
                &owner,
                seq,
                prev,
                Some(owner_id),
                &device_add(&member, DeviceRole::Member),
            );
            account_ingest(&conn, &bytes, NOW + 2 + i64::try_from(seq).unwrap()).unwrap();
            prev = Some(hash);
            heads.push(hash);
        }
        let demote = AccountOp::OwnerDemote {
            device_fingerprint: owner.fp,
            owner_id,
            control_cut: super::super::cut::Cut::At { seq: 0, hash: heads[0] },
            secrets_cut: super::super::cut::Cut::Empty,
            reason: "demote".to_string(),
        };
        let (demote_bytes, demote_hash) =
            op(account, &founder, 2, Some(owner_id), Some(genesis), &demote);
        account_ingest(&conn, &demote_bytes, NOW + 5).unwrap();
        let (remove_bytes, remove_hash) = op(
            account,
            &founder,
            3,
            Some(demote_hash),
            Some(genesis),
            &device_remove(&owner, super::super::cut::Cut::At { seq: 1, hash: heads[1] }),
        );
        account_ingest(&conn, &remove_bytes, NOW + 6).unwrap();
        let expected = |device_boundary, incarnation_boundary| {
            fold::AuthorityQuery::Effective(fold::OwnerChainAuthority {
                owner: fold::OwnerAuthority { device_fingerprint: owner.fp },
                device_boundary,
                incarnation_boundary,
            })
        };
        assert_eq!(
            owner_control_authority(&conn, account, owner_id, owner.fp).unwrap(),
            expected(
                fold::AuthorityBoundary::Cut { seq: 1, hash: heads[1] },
                fold::AuthorityBoundary::Cut { seq: 0, hash: heads[0] },
            ),
        );
        let extend_incarnation = AccountOp::CutExtend {
            chain_kind: super::super::ops::ChainKind::Ctrl,
            stream_id: None,
            incarnation_id: Some(owner_id),
            subject_account_id: account,
            device_fingerprint: owner.fp,
            new_seq: 2,
            new_entry_hash: heads[2],
        };
        let (bytes, extend_hash) =
            op(account, &founder, 4, Some(remove_hash), Some(genesis), &extend_incarnation);
        account_ingest(&conn, &bytes, NOW + 7).unwrap();
        assert_eq!(
            owner_control_authority(&conn, account, owner_id, owner.fp).unwrap(),
            expected(
                fold::AuthorityBoundary::Cut { seq: 1, hash: heads[1] },
                fold::AuthorityBoundary::Cut { seq: 2, hash: heads[2] },
            ),
            "extending the incarnation register leaves the device register unchanged",
        );
        let (bytes, _) = op(
            account,
            &founder,
            5,
            Some(extend_hash),
            Some(genesis),
            &cut_extend_ctrl(account, &owner, 2, heads[2]),
        );
        account_ingest(&conn, &bytes, NOW + 8).unwrap();
        assert_eq!(
            owner_control_authority(&conn, account, owner_id, owner.fp).unwrap(),
            expected(
                fold::AuthorityBoundary::Cut { seq: 2, hash: heads[2] },
                fold::AuthorityBoundary::Cut { seq: 2, hash: heads[2] },
            ),
            "extending the device register leaves the incarnation register unchanged",
        );
    }

    #[test]
    fn an_equivocation_accepts_one_head_and_forks_the_other() {
        // The founder equivocates: two DIFFERENT seq-1 entries on its own chain. Both fold
        // effective, but only one can occupy the (device, seq) slot (I10a) — the smaller
        // entry_hash wins, the other is `forked`. The partial unique index would blow up if
        // refold accepted both.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        let (b, c) = (Dev::new(2), Dev::new(3));
        let (b_bytes, b_hash) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Member));
        let (c_bytes, c_hash) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&c, DeviceRole::Member));
        account_ingest(&conn, &b_bytes, NOW).unwrap();
        account_ingest(&conn, &c_bytes, NOW).unwrap();

        let (winner, loser) = if b_hash < c_hash { (b_hash, c_hash) } else { (c_hash, b_hash) };
        assert_eq!(
            status(&conn, &winner).as_deref(),
            Some("accepted"),
            "smaller hash wins the slot"
        );
        assert_eq!(
            status(&conn, &loser).as_deref(),
            Some("forked"),
            "the equivocation loser forks"
        );
        // I10a: exactly one accepted entry at the (founder, seq 1) slot.
        let accepted_at_slot: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_entries
                 WHERE account_id = ?1 AND device_fingerprint = ?2 AND seq = 1 AND accepted = 1",
                params![acct.to_bytes().as_slice(), founder.fp.to_bytes().as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(accepted_at_slot, 1, "one accepted head per slot (I10a)");
    }

    #[test]
    fn losing_mint_authority_is_pruned_transitively_in_every_arrival_order() {
        // The founder equivocates between minting B and W. Whichever mint loses by hash must not
        // authorize an independent B chain: B mints C, then C mints D. Content-addressed key
        // discovery deliberately knows all three keys, so signature verification alone cannot
        // hide an authority-closure bug. The winning device also authors a child to prove pruning
        // is scoped to the losing incarnation rather than all newly minted devices.
        for rotation in 0..4 {
            let conn = db();
            let founder = Dev::new(1);
            let (acct, gbytes, gh) = genesis(&founder);
            account_ingest(&conn, &gbytes, NOW).unwrap();

            let (b, w, c, d, survivor_child) =
                (Dev::new(2), Dev::new(3), Dev::new(4), Dev::new(5), Dev::new(6));
            let (add_b_bytes, add_b) =
                op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));
            let (add_w_bytes, add_w) =
                op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&w, DeviceRole::Owner));

            let (loser, loser_add_bytes, loser_add, winner, winner_add_bytes, winner_add) =
                if add_b < add_w {
                    (&w, &add_w_bytes, add_w, &b, &add_b_bytes, add_b)
                } else {
                    (&b, &add_b_bytes, add_b, &w, &add_w_bytes, add_w)
                };
            let (add_c_bytes, add_c) =
                op(acct, loser, 0, None, Some(loser_add), &device_add(&c, DeviceRole::Owner));
            let (add_d_bytes, add_d) =
                op(acct, &c, 0, None, Some(add_c), &device_add(&d, DeviceRole::Member));
            let (winner_child_bytes, winner_child) = op(
                acct,
                winner,
                0,
                None,
                Some(winner_add),
                &device_add(&survivor_child, DeviceRole::Member),
            );

            let mut arrivals = [loser_add_bytes, winner_add_bytes, &add_c_bytes, &add_d_bytes];
            arrivals.rotate_left(rotation);
            for bytes in arrivals {
                account_ingest(&conn, bytes, NOW).unwrap();
            }
            account_ingest(&conn, &winner_child_bytes, NOW).unwrap();

            assert_eq!(status(&conn, &winner_add).as_deref(), Some("accepted"));
            assert_eq!(status(&conn, &winner_child).as_deref(), Some("accepted"));
            assert_eq!(status(&conn, &loser_add).as_deref(), Some("forked"));
            assert_eq!(status(&conn, &add_c).as_deref(), Some("forked"));
            assert_eq!(status(&conn, &add_d).as_deref(), Some("forked"));
        }
    }

    #[test]
    fn roster_and_owner_projection_tracks_role_changes_and_closures() {
        let conn = db();
        let founder = Dev::new(1);
        let member = Dev::new(2);
        let (account_id, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();

        let (add_bytes, add) = op(
            account_id,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&member, DeviceRole::Member),
        );
        account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
        let (promote_bytes, promote) =
            op(account_id, &founder, 2, Some(add), Some(genesis_hash), &AccountOp::OwnerPromote {
                device_fingerprint: member.fp,
            });
        account_ingest(&conn, &promote_bytes, NOW + 2).unwrap();
        assert_eq!(
            roster_ref_effective(&conn, account_id, add, member.fp).unwrap(),
            fold::AuthorityQuery::Effective(fold::RosterAuthority {
                device_fingerprint: member.fp,
                current_role: DeviceRole::Owner,
            }),
        );

        let (demote_bytes, demote) = op(
            account_id,
            &founder,
            3,
            Some(promote),
            Some(genesis_hash),
            &owner_demote(&member, promote),
        );
        account_ingest(&conn, &demote_bytes, NOW + 3).unwrap();
        assert_eq!(
            roster_ref_effective(&conn, account_id, add, member.fp).unwrap(),
            fold::AuthorityQuery::Effective(fold::RosterAuthority {
                device_fingerprint: member.fp,
                current_role: DeviceRole::Member,
            }),
        );
        assert_eq!(
            owner_incarnation_effective(&conn, account_id, promote, member.fp).unwrap(),
            fold::AuthorityQuery::Invalid(
                fold::AuthorityInvalidReason::ReferencedEntryNotEffective,
            ),
        );

        let (remove_bytes, _) = op(
            account_id,
            &founder,
            4,
            Some(demote),
            Some(genesis_hash),
            &device_remove(&member, super::super::cut::Cut::Empty),
        );
        account_ingest(&conn, &remove_bytes, NOW + 4).unwrap();
        assert_eq!(
            roster_ref_effective(&conn, account_id, add, member.fp).unwrap(),
            fold::AuthorityQuery::Invalid(
                fold::AuthorityInvalidReason::ReferencedEntryNotEffective,
            ),
        );
    }

    #[test]
    fn final_fold_does_not_keep_control_effects_from_a_losing_mint() {
        // The losing founder branch mints owner B. A demotion of B sits on the WINNING founder
        // branch, so the first all-candidate fold can resolve B's owner_id and treat the demotion
        // as effective. After branch elimination, the final fold must run without the
        // losing mint: the demotion can no longer resolve its target and must park without leaving
        // a control register or an accepted status behind. A post-hoc accepted-set filter would
        // fail this assertion.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        let b = Dev::new(2);
        let (add_b_bytes, add_b) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));
        // Find a deterministic rival whose hash is smaller, making the owner mint the losing
        // branch without depending on an assumed ordering of test keys.
        let (winner_bytes, winner_hash) = (3u8..=u8::MAX)
            .find_map(|seed| {
                let rival = Dev::new(seed);
                let candidate = op(
                    acct,
                    &founder,
                    1,
                    Some(gh),
                    Some(gh),
                    &device_add(&rival, DeviceRole::Member),
                );
                (candidate.1 < add_b).then_some(candidate)
            })
            .expect("the deterministic fixture set contains a lower-hash rival");
        let (demote_bytes, demote) =
            op(acct, &founder, 2, Some(winner_hash), Some(gh), &owner_demote(&b, add_b));

        account_ingest(&conn, &add_b_bytes, NOW).unwrap();
        account_ingest(&conn, &winner_bytes, NOW).unwrap();
        account_ingest(&conn, &demote_bytes, NOW).unwrap();

        assert_eq!(status(&conn, &add_b).as_deref(), Some("forked"));
        assert_eq!(status(&conn, &winner_hash).as_deref(), Some("accepted"));
        assert_eq!(status(&conn, &demote).as_deref(), Some("parked"));
    }

    #[test]
    fn an_entry_that_chains_from_a_forked_head_is_not_accepted() {
        // B equivocates at seq 0 (b0a/b0b), then authors a seq-1 entry whose prev_hash chains from
        // the LOSING head. The fold marks that seq-1 entry effective (its own authority is valid)
        // and it is the ONLY candidate at (B, seq 1) — yet accepting it would leave an accepted
        // entry whose parent is `forked`, a broken chain. Branch selection must fork the descendant
        // of a losing head, not just the losing head itself.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        let b = Dev::new(2);
        let (add_b_bytes, add_b) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));
        account_ingest(&conn, &add_b_bytes, NOW).unwrap();
        // B's two equivocating seq-0 heads (each adds a distinct throwaway member so they differ).
        let (t8, t9) = (Dev::new(8), Dev::new(9));
        let (b0a_bytes, b0a) =
            op(acct, &b, 0, None, Some(add_b), &device_add(&t8, DeviceRole::Member));
        let (b0b_bytes, b0b) =
            op(acct, &b, 0, None, Some(add_b), &device_add(&t9, DeviceRole::Member));
        let ((win, win_bytes), (lose, lose_bytes)) = if b0a < b0b {
            ((b0a, &b0a_bytes), (b0b, &b0b_bytes))
        } else {
            ((b0b, &b0b_bytes), (b0a, &b0a_bytes))
        };
        // B continues at seq 1 from the LOSING head.
        let t7 = Dev::new(7);
        let (b1_bytes, b1) =
            op(acct, &b, 1, Some(lose), Some(add_b), &device_add(&t7, DeviceRole::Member));

        account_ingest(&conn, win_bytes, NOW).unwrap();
        account_ingest(&conn, lose_bytes, NOW).unwrap();
        account_ingest(&conn, &b1_bytes, NOW).unwrap();

        assert_eq!(status(&conn, &win).as_deref(), Some("accepted"), "min-hash seq-0 head wins");
        assert_eq!(status(&conn, &lose).as_deref(), Some("forked"), "the losing seq-0 head forks");
        assert_eq!(
            status(&conn, &b1).as_deref(),
            Some("forked"),
            "a seq-1 entry chaining from the forked head is off-branch, not accepted",
        );
        // No accepted entry has a forked parent: nothing is accepted on B's dead branch at seq 1.
        let accepted_seq1: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_entries
                 WHERE account_id = ?1 AND device_fingerprint = ?2 AND seq = 1 AND accepted = 1",
                params![acct.to_bytes().as_slice(), b.fp.to_bytes().as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(accepted_seq1, 0, "no accepted seq-1 entry on B's forked branch");
    }

    #[test]
    fn a_malformed_op_payload_is_rejected_not_stored() {
        // A founder-signed DEVICE_ADD whose plaintext op payload is not decodable CBOR. The
        // envelope + signature verify (the payload rides opaque inside the envelope), but the op is
        // garbage — a structural reject that must never enter the grow-only DAG. Only ops::decode
        // catches it, so ingest must gate on it before storing.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        let header = AccountEntryHeader {
            account_id: acct,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: 1,
            prev_hash: Some(gh),
            parent_ref: None,
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(gh),
        };
        let signed = sign_account_entry(&founder.secret, &header, &[0xff, 0xff, 0xff]).unwrap();
        let out = account_ingest(&conn, &signed.signed_bytes, NOW).unwrap();
        assert!(
            matches!(out, IngestOutcome::Rejected(_)),
            "a malformed op payload is rejected: {out:?}"
        );
        assert_eq!(status(&conn, &signed.entry_hash), None, "and is never stored");
    }

    #[test]
    fn sealed_and_future_version_payloads_remain_opaque_through_ingest() {
        // C1 does not understand sealed ciphertext or future op versions. Both must remain valid
        // ancestry/watermark targets instead of being decoded with today's plaintext schema.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();

        let opaque = [0xff, 0x00, 0xff]; // deliberately not a current DeviceAdd payload
        let sealed_header = AccountEntryHeader {
            account_id: acct,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: 1,
            prev_hash: Some(gh),
            parent_ref: None,
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 1,
            auth_len: 1,
            key_id: Some([0x44; 32]),
            authority_ref: Some(gh),
        };
        let sealed = sign_account_entry(&founder.secret, &sealed_header, &opaque).unwrap();
        assert_eq!(
            account_ingest(&conn, &sealed.signed_bytes, NOW).unwrap(),
            IngestOutcome::Ingested { status: "retained_unfolded".into() },
        );

        let future_header = AccountEntryHeader {
            seq: 2,
            prev_hash: Some(sealed.entry_hash),
            op_version: 2,
            crypto_suite: 0,
            key_id: None,
            ..sealed_header
        };
        let future = sign_account_entry(&founder.secret, &future_header, &opaque).unwrap();
        assert_eq!(
            account_ingest(&conn, &future.signed_bytes, NOW).unwrap(),
            IngestOutcome::Ingested { status: "retained_unfolded".into() },
        );
        assert_eq!(status(&conn, &sealed.entry_hash).as_deref(), Some("retained_unfolded"));
        assert_eq!(status(&conn, &future.entry_hash).as_deref(), Some("retained_unfolded"));
    }

    #[test]
    fn malformed_payload_is_rejected_before_it_can_enter_pre_verify() {
        // Payload structure is independent of signer resolution. Reject it before parking so the
        // unauthenticated queue never contradicts the "structural rejects are never stored" rule.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, _gbytes, gh) = genesis(&founder);
        let b = Dev::new(2);
        let malformed_header = AccountEntryHeader {
            account_id: acct,
            log_id: 0,
            device_fingerprint: b.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(gh),
        };
        let malformed =
            sign_account_entry(&b.secret, &malformed_header, &[0xff, 0xff, 0xff]).unwrap();
        assert!(matches!(
            account_ingest(&conn, &malformed.signed_bytes, NOW).unwrap(),
            IngestOutcome::Rejected(_)
        ));
        assert_eq!(status(&conn, &malformed.entry_hash), None);
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pending, 0, "malformed bytes are never parked");
    }

    #[test]
    fn branch_selection_requires_contiguous_sequence_numbers() {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        let b = Dev::new(2);
        let payload = ops::encode(&device_add(&b, DeviceRole::Member)).unwrap();
        let gap_header = AccountEntryHeader {
            account_id: acct,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: 2,
            prev_hash: Some(gh),
            parent_ref: None,
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(gh),
        };
        let gap = sign_account_entry(&founder.secret, &gap_header, &payload).unwrap();
        account_ingest(&conn, &gap.signed_bytes, NOW).unwrap();
        assert_eq!(status(&conn, &gap.entry_hash).as_deref(), Some("forked"));

        let c = Dev::new(3);
        let (next_bytes, next) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&c, DeviceRole::Member));
        account_ingest(&conn, &next_bytes, NOW).unwrap();
        assert_eq!(status(&conn, &next).as_deref(), Some("accepted"));
        assert_eq!(
            status(&conn, &gap.entry_hash).as_deref(),
            Some("forked"),
            "a seq-2 sibling of seq-1 cannot extend the accepted chain",
        );
    }

    #[test]
    fn sequence_values_outside_sqlite_integer_are_rejected() {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, _gbytes, gh) = genesis(&founder);
        let b = Dev::new(2);
        let payload = ops::encode(&device_add(&b, DeviceRole::Member)).unwrap();
        let header = AccountEntryHeader {
            account_id: acct,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: i64::MAX as u64 + 1,
            prev_hash: Some(gh),
            parent_ref: None,
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(gh),
        };
        let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
        assert_eq!(
            account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
            IngestOutcome::Rejected("account seq exceeds SQLite INTEGER range".into()),
        );
    }

    #[test]
    fn competing_pre_verify_signatures_are_retained_until_one_verifies() {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        let b = Dev::new(2);
        let (valid_bytes, entry_hash) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));
        let mut bad_bytes = valid_bytes.clone();
        *bad_bytes.last_mut().unwrap() ^= 1; // signature byte; body/entry_hash stays identical

        assert_eq!(account_ingest(&conn, &bad_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
        assert_eq!(account_ingest(&conn, &valid_bytes, NOW).unwrap(), IngestOutcome::PreVerify);
        let parked: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0))
            .unwrap();
        assert_eq!(parked, 2, "both signed envelopes for one entry body are retained");

        account_ingest(&conn, &gbytes, NOW).unwrap();
        assert_eq!(status(&conn, &entry_hash).as_deref(), Some("accepted"));
        let parked: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0))
            .unwrap();
        assert_eq!(parked, 0, "both the refuted and promoted envelopes are drained");
    }

    #[test]
    fn forged_genesis_is_rejected_before_pre_verify() {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, real_bytes, _gh) = genesis(&founder);
        let attacker = Dev::new(9);
        let genesis_payload = envelope::decode_account_signed(&real_bytes).unwrap().payload;
        let forged_header = AccountEntryHeader {
            account_id: acct,
            log_id: 0,
            device_fingerprint: attacker.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: ops::entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let forged =
            sign_account_entry(&attacker.secret, &forged_header, &genesis_payload).unwrap();
        assert!(matches!(
            account_ingest(&conn, &forged.signed_bytes, NOW).unwrap(),
            IngestOutcome::Rejected(_)
        ));
        assert_eq!(status(&conn, &forged.entry_hash), None, "founder binding refutes the forgery");
        let parked: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_pre_verify", [], |row| row.get(0))
            .unwrap();
        assert_eq!(parked, 0);
    }

    #[test]
    fn opaque_device_add_cannot_self_resolve_or_seed_key_discovery() {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, _gbytes, gh) = genesis(&founder);
        let b = Dev::new(2);
        let payload = ops::encode(&device_add(&b, DeviceRole::Owner)).unwrap();
        let header = AccountEntryHeader {
            account_id: acct,
            log_id: 0,
            device_fingerprint: b.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: 2,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(gh),
        };
        let opaque = sign_account_entry(&b.secret, &header, &payload).unwrap();
        assert_eq!(
            account_ingest(&conn, &opaque.signed_bytes, NOW).unwrap(),
            IngestOutcome::PreVerify
        );
        assert_eq!(status(&conn, &opaque.entry_hash), None);
    }

    #[test]
    fn non_control_payload_is_retained_without_control_schema_decode() {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        // A log-1 entry carrying the control DEVICE_ADD tag: on the SECRETS log that number is an
        // unknown secrets tag, so the control DEVICE_ADD schema is never applied. C4.2b's secrets
        // twin DOES validate log-1 plaintext at ingest, but only as one canonical CBOR item (the
        // same opaque-retention rule as an unknown control tag) — a canonical payload is retained
        // `retained_unfolded`, never decoded against any op schema.
        let header = AccountEntryHeader {
            account_id: acct,
            log_id: 1,
            device_fingerprint: founder.fp,
            seq: 1,
            prev_hash: Some(gh),
            parent_ref: None,
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(gh),
        };
        let signed = sign_account_entry(&founder.secret, &header, &[0x80]).unwrap();
        assert_eq!(
            account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
            IngestOutcome::Ingested { status: "retained_unfolded".into() },
        );
    }

    #[test]
    fn a_non_canonical_secrets_plaintext_payload_is_rejected_at_ingest() {
        // The secrets twin (C4.2b) rejects a non-canonical log-1 plaintext payload at ingest,
        // exactly as an unknown control tag with a non-canonical payload is rejected — a
        // cross-version consensus split on a signed log is the hazard the canonicity rule
        // closes.
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        let header = AccountEntryHeader {
            account_id: acct,
            log_id: 1,
            device_fingerprint: founder.fp,
            seq: 1,
            prev_hash: Some(gh),
            parent_ref: None,
            entry_type: ops::entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(gh),
        };
        let signed = sign_account_entry(&founder.secret, &header, &[0xff, 0x00]).unwrap();
        assert!(
            matches!(
                account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
                IngestOutcome::Rejected(_)
            ),
            "a non-canonical secrets plaintext payload is a structural reject",
        );
        assert_eq!(status(&conn, &signed.entry_hash), None, "a rejected entry is not stored");
    }

    #[test]
    fn refold_is_idempotent_and_preserves_the_selected_branch() {
        let conn = db();
        let founder = Dev::new(1);
        let (acct, gbytes, gh) = genesis(&founder);
        account_ingest(&conn, &gbytes, NOW).unwrap();
        let b = Dev::new(2);
        let (add_bytes, add_hash) =
            op(acct, &founder, 1, Some(gh), Some(gh), &device_add(&b, DeviceRole::Owner));
        account_ingest(&conn, &add_bytes, NOW).unwrap();

        refold_account(&conn, acct).unwrap();
        refold_account(&conn, acct).unwrap();

        assert_eq!(status(&conn, &gh).as_deref(), Some("accepted"));
        assert_eq!(status(&conn, &add_hash).as_deref(), Some("accepted"));
        let accepted: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_entries WHERE account_id = ?1 AND accepted = 1",
                params![acct.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(accepted, 2, "repeated clear-and-set refolds keep the same accepted chain");
    }

    #[test]
    fn removed_roster_citation_keeps_only_its_per_stream_prefix() {
        let conn = db();
        let founder = Dev::new(1);
        let member = Dev::new(2);
        let (account, genesis_bytes, genesis_hash) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        let (add_bytes, roster_ref) = op(
            account,
            &founder,
            1,
            Some(genesis_hash),
            Some(genesis_hash),
            &device_add(&member, DeviceRole::Member),
        );
        account_ingest(&conn, &add_bytes, NOW + 1).unwrap();
        let listed = StreamId::from_bytes([0x41; 32]);
        let unlisted = StreamId::from_bytes([0x42; 32]);
        let remove = AccountOp::DeviceRemove {
            device_fingerprint: member.fp,
            control_cut: super::super::cut::Cut::Empty,
            secrets_cut: super::super::cut::Cut::Empty,
            content_cuts: vec![ContentCut { stream_id: listed, seq: u64::MAX, hash: [0xa5; 32] }],
            reason: "revoked".to_string(),
        };
        let (remove_bytes, _) =
            op(account, &founder, 2, Some(roster_ref), Some(genesis_hash), &remove);
        account_ingest(&conn, &remove_bytes, NOW + 2).unwrap();

        assert_eq!(
            roster_content_authority(&conn, account, roster_ref, member.fp, listed).unwrap(),
            fold::AuthorityQuery::Effective(fold::RosterContentAuthority {
                device_fingerprint: member.fp,
                boundary: fold::AuthorityBoundary::Cut { seq: u64::MAX, hash: [0xa5; 32] },
            }),
        );
        assert_eq!(
            roster_content_authority(&conn, account, roster_ref, member.fp, unlisted).unwrap(),
            fold::AuthorityQuery::Effective(fold::RosterContentAuthority {
                device_fingerprint: member.fp,
                boundary: fold::AuthorityBoundary::Closed,
            }),
            "an omitted content chain is the empty cut, never open",
        );
    }

    #[test]
    fn malformed_persisted_owner_boundary_fails_closed() {
        let conn = db();
        let founder = Dev::new(1);
        let (account, genesis_bytes, owner_id) = genesis(&founder);
        account_ingest(&conn, &genesis_bytes, NOW).unwrap();
        conn.execute(
            "UPDATE account_owner_incarnations
             SET control_boundary = 'cut', control_seq = NULL, control_hash = NULL
             WHERE owner_id = ?1",
            [owner_id.as_slice()],
        )
        .unwrap();
        assert!(
            owner_control_authority(&conn, account, owner_id, founder.fp).is_err(),
            "a partial cut tuple must never become open authority",
        );
        conn.execute(
            "UPDATE account_owner_incarnations
             SET control_boundary = 'open', effective_at = -1
             WHERE owner_id = ?1",
            [owner_id.as_slice()],
        )
        .unwrap();
        assert!(
            owner_control_authority(&conn, account, owner_id, founder.fp).is_err(),
            "negative fact epochs fail closed",
        );
        conn.execute(
            "UPDATE account_owner_incarnations SET effective_at = 0, closed_at = 2
             WHERE owner_id = ?1",
            [owner_id.as_slice()],
        )
        .unwrap();
        conn.execute("UPDATE account_roster_history SET closed_at = 2 WHERE roster_ref = ?1", [
            owner_id.as_slice(),
        ])
        .unwrap();
        assert!(
            owner_control_authority(&conn, account, owner_id, founder.fp).is_err(),
            "a closed roster and owner cannot jointly retain Open/Open authority",
        );
    }
}
