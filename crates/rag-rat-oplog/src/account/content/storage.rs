//! C2 `/3` candidate-DAG ingest and dense-chain structural classification (§16).
//!
//! This layer verifies an exact content-addressed `roster_ref`, signatures, and dense predecessor
//! coordinates. It deliberately never sets `accepted`: C3 must evaluate authority, cuts,
//! freshness, and branch selection together before content can reach the live projection.

use std::collections::{HashMap, HashSet, VecDeque};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::super::branch::{BranchSelection, CitedFreshness};
#[cfg(test)]
use super::super::id::OwnerId;
use super::super::id::{AccountEntryHash, GrantId, RosterRef, SignedHash};
use super::super::ops::{self, AccountOp, DecodedAccountOp};
use super::super::pre_verify::{BudgetOutcome, PreVerifyQueue, QueueBudget};
use super::super::{envelope as account_envelope, storage as account_storage};
use super::acceptance::{
    self, CitedGrantAuthority, CitedOwnership, CitedRosterAuthority, ContentAcceptance,
    ContentParkReason, SubjectAuthorityHold,
};
use super::candidate::{
    self, BranchPin, ChainCoordinate, ContentCandidate, CutBinding, HeaderView,
};
use super::envelope::{self, ContentEntryHeader, SignedContentEntry, VerifiedContentEntry};
use crate::account::{
    AccountId, AuthorityBoundary, AuthorityFreshness, AuthorityQuery, GrantDeviceBoundary,
    GrantRole,
};
use crate::device::DevicePublic;
use crate::op::DeviceFingerprint;
use crate::stream::StreamId;
use crate::{cbor, content_projection, identity};

/// A stored fixed-width blob as an array. The content layer keeps its own wrong-length wording
/// (`expected N bytes, got M`), distinct from the account layer's [`crate::account::id::fixed`].
pub(super) fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("expected {N} bytes, got {}", bytes.len()))
}

const PENDING_REFOLD_CONTENT_CANDIDATE: i64 = 1;
const PENDING_REFOLD_ACCOUNT_CHANGE: i64 = 2;

const PRE_VERIFY_PER_AUTHOR_MAX: usize = 64;
const PRE_VERIFY_GLOBAL_MAX: usize = 256;
const PRE_VERIFY: PreVerifyQueue =
    PreVerifyQueue { table: "content_pre_verify", owner_column: "claimed_author_account_id" };
const CANDIDATES_PER_AUTHOR_MAX: i64 = 4_096;
const CANDIDATES_GLOBAL_MAX: i64 = 16_384;
const CANDIDATE_BYTES_PER_AUTHOR_MAX: i64 = 16 * 1024 * 1024;
const CANDIDATE_BYTES_GLOBAL_MAX: i64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentStatus {
    MissingPredecessor,
    RetainedUnfolded,
}

impl ContentStatus {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::MissingPredecessor => "parked{missing_predecessor}",
            Self::RetainedUnfolded => "retained_unfolded",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentCapacityScope {
    PreVerifyAuthor,
    PreVerifyGlobal,
    CandidateAuthor,
    CandidateGlobal,
    CandidateAuthorBytes,
    CandidateGlobalBytes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentIngestOutcome {
    Rejected(String),
    PreVerify,
    PreVerifyWithEviction { scopes: Vec<ContentCapacityScope> },
    CapacityReached { scope: ContentCapacityScope },
    Ingested { status: String },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContentPromotionOutcome {
    pub scope: Option<ContentCapacityScope>,
    pub entry_hashes: Vec<AccountEntryHash>,
}

/// Ingest one REMOTE, untrusted `/3` content envelope: resolve its roster key, verify the
/// signature, store the candidate under the §18b anti-abuse budgets, and classify it structurally
/// against the dense chain.
///
/// The whole-stream acceptance fold is DEFERRED (#652). Running it inline on every ingested entry
/// re-evaluates the O(stream) authority + branch-selection pass once per entry, so building an
/// n-entry stream one candidate at a time is O(n^2) under the writer lock — and an attacker varying
/// cited `auth_len` down the chain defeats the per-refold freshness cache, keeping each pass O(n).
/// Instead this marks the stream owing a refold; [`settle_pending_content_refolds`] runs it ONCE.
/// The returned `Ingested { status }` therefore reports
/// the STRUCTURAL verdict, not the acceptance verdict: a caller that needs foreign acceptance MUST
/// settle first. Nothing reads foreign acceptance before transport lands (#691), so the deferral is
/// invisible today; the local author path is unaffected (it never routes through here).
pub fn content_ingest(
    conn: &Connection,
    signed_bytes: &[u8],
    now_ms: i64,
) -> anyhow::Result<ContentIngestOutcome> {
    let signed = match envelope::decode_content_signed(signed_bytes) {
        Ok(signed) => signed,
        Err(error) => return Ok(ContentIngestOutcome::Rejected(error.to_string())),
    };
    // Reserve the protocol lamport ceiling BEFORE anything is stored — including the pre-verify
    // park below. `promote_pre_verify_for_account` re-inserts parked bytes without re-entering
    // this function, so a check any later is bypassed by parking a near-ceiling entry behind a
    // withheld roster. The rejection is a drop, never durable: nothing is written, so a later
    // session may re-offer the envelope. Authoring caps at the same ceiling, so no honest peer's
    // entry can ever trip this.
    if signed.header.lamport >= crate::entry::MAX_ENTRY_LAMPORT {
        return Ok(ContentIngestOutcome::Rejected(format!(
            "entry lamport {} exceeds the protocol ceiling",
            signed.header.lamport
        )));
    }
    if let Some(status) = stored_status_for_exact_envelope(conn, &signed, signed_bytes)? {
        return Ok(ContentIngestOutcome::Ingested { status });
    }

    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let public = match resolve_roster_key(&tx, &signed) {
        Ok(Some(public)) => public,
        Ok(None) => {
            let outcome = park_pre_verify(&tx, &signed, signed_bytes, now_ms)?;
            tx.commit()?;
            return Ok(outcome);
        },
        Err(error) => return Ok(ContentIngestOutcome::Rejected(error.to_string())),
    };
    let verified = match envelope::verify_content_signed(signed_bytes, &public) {
        Ok(verified) => verified,
        Err(error) => return Ok(ContentIngestOutcome::Rejected(error.to_string())),
    };
    // Bounded advance at ingest, as a DROP — nothing is stored, so a stale-max race or a stream
    // whose accepted clock later catches up is repaired by the next session re-offering the
    // envelope. This is what keeps the V113 upgrade repair stable: without it, a not-yet-purged
    // replica re-sends a purged poison (or the honest tail that inherited its clock), the entry
    // re-parks as its author's candidate chain tail, and local authoring wedges again.
    //
    // The check runs only AFTER signature verification: the accepted-clock read decodes every
    // accepted envelope, and pre-verification placement would hand any peer able to spray forged
    // high-lamport envelopes a repeatable O(stream) scan that no capacity budget throttles
    // (nothing gets stored). The O(1) ceiling check above stays pre-verification; an
    // unknown-roster envelope parks pre-verify UNSCANNED (bounded by the pre-verify budgets) and
    // promotion re-applies this gate before it can become a candidate. The `MAX_LAMPORT_ADVANCE`
    // pre-check keeps the scan off the honest path entirely: a legitimate lamport counts real ops
    // and cannot approach 2^32 under the candidate caps, so only an authenticated writer's
    // suspicious entries pay it — and they are dropped, not stored. The catch-up trap (rejecting
    // an honest writer against a stale max) is unreachable at these constants: it would take a
    // >4-billion-entry backlog against a 16k candidate cap. The fold's clamp stays authoritative
    // for whatever is already stored; this gate is advisory and only ever refuses what the fold
    // would park anyway.
    if verified.header.lamport > crate::entry::MAX_LAMPORT_ADVANCE {
        let stream_max =
            super::author::stream_max_content_lamport(&tx, verified.header.stream_id)?.unwrap_or(0);
        if verified.header.lamport > stream_max.saturating_add(crate::entry::MAX_LAMPORT_ADVANCE) {
            return Ok(ContentIngestOutcome::Rejected(format!(
                "entry lamport {} jumps more than {} past the accepted stream clock {stream_max}",
                verified.header.lamport,
                crate::entry::MAX_LAMPORT_ADVANCE
            )));
        }
    }
    match stored_candidate_bytes(&tx, &verified.entry_hash)? {
        Some(stored) if stored != signed_bytes => {
            return Ok(ContentIngestOutcome::Rejected(
                "entry hash collides with a different stored envelope".into(),
            ));
        },
        Some(_) => {},
        None =>
            if let Some(scope) = candidate_capacity(&tx, &verified, signed_bytes.len())? {
                return Ok(ContentIngestOutcome::CapacityReached { scope });
            },
    }
    insert_candidate(&tx, &verified, signed_bytes, now_ms)?;
    reclassify_chain(&tx, &verified)?;
    // Structural classification is done; the authority + branch-selection fold (the pass that sets
    // `accepted`) is deferred off this per-entry path (#652) by marking the stream as owing a
    // refold. `settle_pending_content_refolds` folds it once. So the returned status is the
    // STRUCTURAL verdict, not the acceptance verdict.
    mark_stream_pending_refold(
        &tx,
        verified.header.stream_id,
        PENDING_REFOLD_CONTENT_CANDIDATE,
        now_ms,
    )?;
    let status = status_for(&tx, &verified.entry_hash)?
        .unwrap_or_else(|| ContentStatus::RetainedUnfolded.as_db_str().to_string());
    tx.commit()?;
    Ok(ContentIngestOutcome::Ingested { status })
}

fn stored_candidate_bytes(
    conn: &Connection,
    entry_hash: &AccountEntryHash,
) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT signed_bytes FROM content_entries WHERE entry_hash = ?1",
        [entry_hash.as_slice()],
        |row| row.get(0),
    )
    .optional()
}

fn stored_status_for_exact_envelope(
    conn: &Connection,
    signed: &SignedContentEntry,
    signed_bytes: &[u8],
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT s.status FROM content_entries e
         JOIN content_entry_status s ON s.entry_hash = e.entry_hash
         WHERE e.entry_hash = ?1 AND e.signed_bytes = ?2",
        params![signed.entry_hash.as_slice(), signed_bytes],
        |row| row.get(0),
    )
    .optional()
}

fn resolve_roster_key(
    conn: &Connection,
    content: &SignedContentEntry,
) -> anyhow::Result<Option<DevicePublic>> {
    let raw: Option<Vec<u8>> = conn
        .query_row(
            "SELECT signed_bytes FROM account_entries
             WHERE entry_hash = ?1 AND account_id = ?2",
            params![
                content.header.roster_ref.as_slice(),
                content.header.author_account_id.to_bytes().as_slice(),
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let roster = account_envelope::decode_account_signed(&raw)?;
    if roster.entry_hash != content.header.roster_ref.into()
        || roster.header.account_id != content.header.author_account_id
        || roster.header.log_id != 0
        || roster.header.op_version != 1
        || roster.header.crypto_suite != 0
    {
        anyhow::bail!("roster_ref does not name a current plaintext control candidate");
    }
    let DecodedAccountOp::Known(op) = ops::decode(roster.header.entry_type, &roster.payload)?
    else {
        anyhow::bail!("roster_ref names an unknown account operation");
    };
    let public_bytes = match op {
        AccountOp::AccountGenesis { ed25519_pubkey, .. }
            if roster.header.device_fingerprint == content.header.device_fingerprint =>
            ed25519_pubkey,
        AccountOp::DeviceAdd { device_fingerprint, ed25519_pubkey, .. }
            if device_fingerprint == content.header.device_fingerprint =>
            ed25519_pubkey,
        _ => anyhow::bail!("roster_ref does not enroll the content signing device"),
    };
    let public = DevicePublic::from_bytes(&public_bytes)?;
    if public.fingerprint() != content.header.device_fingerprint {
        anyhow::bail!("roster_ref public key does not match the content signing device");
    }
    Ok(Some(public))
}

fn park_pre_verify(
    tx: &Transaction<'_>,
    signed: &SignedContentEntry,
    raw: &[u8],
    now_ms: i64,
) -> rusqlite::Result<ContentIngestOutcome> {
    let signed_hash = cbor::sha256(raw);
    if PRE_VERIFY.contains(tx, &signed_hash.into())? {
        return Ok(ContentIngestOutcome::PreVerify);
    }
    let author = signed.header.author_account_id.to_bytes();
    tx.execute(
        "INSERT OR IGNORE INTO content_pre_verify(
             signed_hash, entry_hash, claimed_stream_id, claimed_author_account_id,
             claimed_fingerprint, roster_ref, raw_bytes, received_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            signed_hash.as_slice(),
            signed.entry_hash.as_slice(),
            signed.header.stream_id.to_bytes().as_slice(),
            author.as_slice(),
            signed.header.device_fingerprint.to_bytes().as_slice(),
            signed.header.roster_ref.as_slice(),
            raw,
            now_ms,
        ],
    )?;
    enforce_pre_verify_budget(tx, signed.header.author_account_id, &signed_hash.into())
}

fn enforce_pre_verify_budget(
    tx: &Transaction<'_>,
    author: super::super::AccountId,
    inserted_hash: &SignedHash,
) -> rusqlite::Result<ContentIngestOutcome> {
    let outcome = PRE_VERIFY.enforce_budget(
        tx,
        author,
        inserted_hash,
        QueueBudget {
            max: PRE_VERIFY_PER_AUTHOR_MAX,
            scope: ContentCapacityScope::PreVerifyAuthor,
        },
        QueueBudget { max: PRE_VERIFY_GLOBAL_MAX, scope: ContentCapacityScope::PreVerifyGlobal },
    )?;
    Ok(match outcome {
        BudgetOutcome::Parked { evicted } if evicted.is_empty() => ContentIngestOutcome::PreVerify,
        BudgetOutcome::Parked { evicted } =>
            ContentIngestOutcome::PreVerifyWithEviction { scopes: evicted },
        BudgetOutcome::AtCapacity(scope) => ContentIngestOutcome::CapacityReached { scope },
    })
}

// Test-only: how many times the parked-content promotion sweep actually ran. The gate that keeps
// it off every non-resolving account entry is a COST invariant, not an outcome one — the sweep is
// a no-op whenever nothing new can resolve, so a parked-row-count assertion alone would pass just
// as happily with the gate deleted. Thread-local for the same reason as the settle counters.
#[cfg(test)]
thread_local! {
    static PRE_VERIFY_CONTENT_SWEEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(in crate::account) fn reset_pre_verify_content_sweeps() {
    PRE_VERIFY_CONTENT_SWEEPS.with(|c| c.set(0));
}

#[cfg(test)]
pub(in crate::account) fn pre_verify_content_sweeps() -> usize {
    PRE_VERIFY_CONTENT_SWEEPS.with(std::cell::Cell::get)
}

pub(in crate::account) fn promote_pre_verify_for_account(
    tx: &Transaction<'_>,
    account_id: super::super::AccountId,
    now_ms: i64,
) -> anyhow::Result<ContentPromotionOutcome> {
    #[cfg(test)]
    PRE_VERIFY_CONTENT_SWEEPS.with(|c| c.set(c.get() + 1));
    // V064/V065 authority backfill predates the V066 content tables. Account-state folding is also
    // used by that migration, where there cannot be content pre-verify work yet.
    if !content_entries_exists(tx)? {
        return Ok(ContentPromotionOutcome::default());
    }
    let mut outcome = ContentPromotionOutcome::default();
    loop {
        let rows = {
            let mut stmt = tx.prepare(
                "SELECT signed_hash, raw_bytes FROM content_pre_verify
                 WHERE claimed_author_account_id = ?1 ORDER BY received_at_ms, signed_hash",
            )?;
            stmt.query_map([account_id.to_bytes().as_slice()], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut progressed = false;
        for (signed_hash, raw) in rows {
            let signed = match envelope::decode_content_signed(&raw) {
                Ok(signed) => signed,
                Err(_) => {
                    PRE_VERIFY.delete(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
            };
            // The ingest-time lamport gates (ceiling here, bounded advance below), re-applied
            // because promotion inserts candidates WITHOUT re-entering `content_ingest`: a
            // pre-verify row stored by a binary that predates the gates can violate either, and
            // promoting it would make the poison durable and relayable. Dropping the row keeps
            // the drop-before-storage contract; a legitimately re-offered envelope re-parks and
            // gets re-judged with a fresher clock.
            if signed.header.lamport >= crate::entry::MAX_ENTRY_LAMPORT {
                PRE_VERIFY.delete(tx, &signed_hash)?;
                progressed = true;
                continue;
            }
            let public = match resolve_roster_key(tx, &signed) {
                Ok(Some(public)) => public,
                Ok(None) => continue,
                Err(_) => {
                    PRE_VERIFY.delete(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
            };
            let verified = match envelope::verify_content_signed(&raw, &public) {
                Ok(verified) => verified,
                Err(_) => {
                    PRE_VERIFY.delete(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
            };
            // The bounded-advance gate, AFTER signature verification for the same reason as at
            // ingest: the accepted-clock read is O(stream), so only an authenticated envelope may
            // trigger it. Rows here are already capacity-bounded, but the ordering discipline is
            // one rule, not two.
            if verified.header.lamport > crate::entry::MAX_LAMPORT_ADVANCE {
                let stream_max =
                    super::author::stream_max_content_lamport(tx, verified.header.stream_id)?
                        .unwrap_or(0);
                if verified.header.lamport
                    > stream_max.saturating_add(crate::entry::MAX_LAMPORT_ADVANCE)
                {
                    PRE_VERIFY.delete(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                }
            }
            match stored_candidate_bytes(tx, &verified.entry_hash)? {
                Some(stored) if stored != raw => {
                    PRE_VERIFY.delete(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
                Some(_) => {},
                None if let Some(scope) = candidate_capacity(tx, &verified, raw.len())? => {
                    outcome.scope.get_or_insert(scope);
                    outcome.entry_hashes.push(verified.entry_hash);
                    PRE_VERIFY.delete(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
                None => {},
            }
            insert_candidate(tx, &verified, &raw, now_ms)?;
            reclassify_chain(tx, &verified)?;
            mark_stream_pending_refold(
                tx,
                verified.header.stream_id,
                PENDING_REFOLD_CONTENT_CANDIDATE,
                now_ms,
            )?;
            PRE_VERIFY.delete(tx, &signed_hash)?;
            progressed = true;
        }
        if !progressed {
            return Ok(outcome);
        }
    }
}

fn candidate_capacity(
    tx: &Transaction<'_>,
    entry: &VerifiedContentEntry,
    incoming_bytes: usize,
) -> anyhow::Result<Option<ContentCapacityScope>> {
    // Both budgets are REMOTE-abuse ceilings (#652) on UNRESOLVED candidates, so they count only
    // rows the acceptance fold has not accepted: accepted content is authorized — signed by a
    // device on its author's roster, or under a grant — and counting it would make an author's
    // history a wall, refusing its 4,097th legitimate entry forever, and a store's whole foreign
    // history a wall for every author. A burst above a budget still stalls only until the settle
    // accepts it; the rest is re-offered by the next session. Forged, parked, and later
    // condemned rows stay `accepted = 0` and keep counting. The local device's OWN signed rows are
    // excluded too, so a large local history never starves foreign ingest. Key the exclusion on the
    // local DEVICE FINGERPRINT, NOT `author_account_id` — the latter is attacker-settable (a
    // self-signed DeviceAdd can store forged content under a claimed local account id), while a
    // row can carry the local fingerprint only if it was signed with the local device key
    // (`verify_content_signed` binds the signature to `header.device_fingerprint`). `None` (no
    // local device minted yet) excludes nothing; the nullable `?local_fp` parameter selects the
    // branch in-SQL.
    let local_fp = identity::local_device_fingerprint(tx)?.map(DeviceFingerprint::to_bytes);
    let local_fp = local_fp.as_ref().map(|fp| fp.as_slice());

    // The per-author budget scopes to the INCOMING (foreign) author. Excluding the local device
    // here is forward-compat for a second local device syncing under the same account;
    // pre-transport it is a no-op, since no foreign author owns locally-signed rows.
    let author = entry.header.author_account_id.to_bytes();
    let (count, bytes): (i64, i64) = tx.query_row(
        "SELECT count(*), coalesce(sum(length(signed_bytes)), 0)
         FROM content_entries
         WHERE author_account_id = ?1 AND accepted = 0
           AND (device_fingerprint != ?2 OR ?2 IS NULL)",
        params![author.as_slice(), local_fp],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count >= CANDIDATES_PER_AUTHOR_MAX {
        return Ok(Some(ContentCapacityScope::CandidateAuthor));
    }
    if bytes.saturating_add(incoming_bytes as i64) > CANDIDATE_BYTES_PER_AUTHOR_MAX {
        return Ok(Some(ContentCapacityScope::CandidateAuthorBytes));
    }
    // The global budget is the remote-flood ceiling; the local device's own signed rows are not
    // remote abuse and are excluded on the same forge-proof key.
    let (count, bytes): (i64, i64) = tx.query_row(
        "SELECT count(*), coalesce(sum(length(signed_bytes)), 0)
         FROM content_entries WHERE accepted = 0 AND (device_fingerprint != ?1 OR ?1 IS NULL)",
        params![local_fp],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count >= CANDIDATES_GLOBAL_MAX {
        return Ok(Some(ContentCapacityScope::CandidateGlobal));
    }
    if bytes.saturating_add(incoming_bytes as i64) > CANDIDATE_BYTES_GLOBAL_MAX {
        return Ok(Some(ContentCapacityScope::CandidateGlobalBytes));
    }
    Ok(None)
}

/// Insert one verified `/3` entry into the candidate DAG under the caller's txn (`accepted = 0`;
/// authority acceptance is the refold's job). `pub(super)` so the in-tx content-author seam
/// [`super::author`] can store its freshly-signed owner-authored entries through the same seam the
/// ingest path uses — it MUST NOT go through the self-transacting [`content_ingest`] (it authors
/// inside the caller's IMMEDIATE txn) and it deliberately skips [`candidate_capacity`] (the §18b
/// remote-abuse budget, not a local-authoring bound).
pub(super) fn insert_candidate(
    tx: &Transaction<'_>,
    entry: &VerifiedContentEntry,
    signed_bytes: &[u8],
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO content_entries(
             entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
             grant_id, roster_ref, owner_auth_len, author_auth_len, lamport, accepted,
             signed_bytes, received_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?13)",
        params![
            entry.entry_hash.as_slice(),
            entry.header.stream_id.to_bytes().as_slice(),
            entry.header.author_account_id.to_bytes().as_slice(),
            entry.header.device_fingerprint.to_bytes().as_slice(),
            entry.header.seq.to_be_bytes().as_slice(),
            entry.header.prev_hash.as_ref().map(|hash| hash.as_slice()),
            entry.header.grant_id.as_ref().map(|hash| hash.as_slice()),
            entry.header.roster_ref.as_slice(),
            entry.header.owner_auth_len.to_be_bytes().as_slice(),
            entry.header.author_auth_len.to_be_bytes().as_slice(),
            stored_lamport(entry.header.lamport),
            signed_bytes,
            now_ms,
        ],
    )?;
    Ok(())
}

/// The `content_entries.lamport` column value for a header lamport. The ingest ceiling keeps
/// every gated value below `1 << 62`, so the clamp to `i64::MAX` only ever fires for legacy junk
/// written around the gates — rows that can never be accepted and so never reach the accepted
/// `MAX` the column exists to serve.
fn stored_lamport(lamport: u64) -> i64 {
    i64::try_from(lamport).unwrap_or(i64::MAX)
}

fn reclassify_chain(tx: &Transaction<'_>, entry: &VerifiedContentEntry) -> anyhow::Result<()> {
    let reachable = match entry.header.prev_hash {
        None => entry.header.seq == 0,
        Some(previous) => {
            let Some(expected) = entry.header.seq.checked_sub(1).map(u64::to_be_bytes) else {
                return Ok(());
            };
            // Reachability is STRUCTURAL: the predecessor must be present at the adjacent seq AND
            // its own chain must not itself be broken. The predicate is "status is NOT
            // `parked{missing_predecessor}`", NOT "status IS `retained_unfolded`" — once a
            // predecessor is settled its status becomes `accepted`/`forked`/`condemned{…}`/etc., so
            // testing for `retained_unfolded` exactly would misclassify a dense continuation of an
            // already-folded chain as `missing_predecessor` (the per-entry refold used to mask
            // this; the deferred refold unmasks it). Both the structural
            // missing-predecessor state and the acceptance-layer
            // `parked{missing_predecessor}` render to the same db string, so the single
            // `!=` covers both.
            tx.query_row(
                "SELECT EXISTS(
                         SELECT 1 FROM content_entries p
                         JOIN content_entry_status s ON s.entry_hash = p.entry_hash
                         WHERE p.entry_hash = ?1 AND p.stream_id = ?2
                           AND p.author_account_id = ?3 AND p.device_fingerprint = ?4
                           AND p.seq = ?5 AND s.status != ?6)",
                params![
                    previous.as_slice(),
                    entry.header.stream_id.to_bytes().as_slice(),
                    entry.header.author_account_id.to_bytes().as_slice(),
                    entry.header.device_fingerprint.to_bytes().as_slice(),
                    expected.as_slice(),
                    ContentStatus::MissingPredecessor.as_db_str(),
                ],
                |row| row.get(0),
            )?
        },
    };
    set_status(
        tx,
        &entry.entry_hash,
        if reachable { ContentStatus::RetainedUnfolded } else { ContentStatus::MissingPredecessor },
    )?;
    if !reachable {
        return Ok(());
    }

    let mut queue = VecDeque::new();
    queue.push_back((entry.entry_hash, entry.header.seq));
    while let Some((parent, parent_seq)) = queue.pop_front() {
        let Some(expected) = parent_seq.checked_add(1) else {
            continue;
        };
        let children = {
            let mut stmt = tx.prepare(
                "SELECT entry_hash, seq FROM content_entries
                 WHERE prev_hash = ?1 AND stream_id = ?2 AND author_account_id = ?3
                   AND device_fingerprint = ?4",
            )?;
            stmt.query_map(
                params![
                    parent.as_slice(),
                    entry.header.stream_id.to_bytes().as_slice(),
                    entry.header.author_account_id.to_bytes().as_slice(),
                    entry.header.device_fingerprint.to_bytes().as_slice(),
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (child, child_seq) in children {
            let child = fixed::<32>(&child)?;
            let child_seq = u64::from_be_bytes(fixed::<8>(&child_seq)?);
            if child_seq != expected {
                continue;
            }
            set_status(tx, &child.into(), ContentStatus::RetainedUnfolded)?;
            queue.push_back((AccountEntryHash::from_bytes(child), child_seq));
        }
    }
    Ok(())
}

/// Every authority fact one candidate is evaluated against, resolved once from the current fold so
/// the two evaluator phases (eligibility, then the finished verdict) read one consistent snapshot.
struct ResolvedEntry {
    entry_hash: AccountEntryHash,
    header: ContentEntryHeader,
    owner_account_id: AccountId,
    dense_predecessor_reachable: bool,
    ownership: AuthorityQuery<CitedOwnership>,
    roster: AuthorityQuery<CitedRosterAuthority>,
    grant: Option<AuthorityQuery<CitedGrantAuthority>>,
    owner_freshness: CitedFreshness,
    author_freshness: CitedFreshness,
    subject_hold: SubjectAuthorityHold,
}

/// Compute the streams whose `/3` acceptance may depend on `account_id`'s fold: the
/// owned-BEFORE ∪ owned-AFTER ∪ AUTHORED union. Purely a READ — it writes no `accepted` flag
/// itself; the caller refolds each returned stream (which is what writes `accepted`, §13).
///
/// `previously_owned` is the owned-before half: a fold that DROPS a `StreamOwn` fact must still
/// refold that stream (to declassify its now-authority-less content), but the ownership row is
/// already gone from the projection, so the caller passes the pre-rewrite set to union in. The
/// owned-after and authored halves come from the just-rewritten projection and `content_entries`.
/// The union covers every cross-account case — a `StreamRevoke` folds in the OWNER's log and
/// reaches the grantee's content through the ownership branch; a roster change folds in the
/// AUTHOR's log and reaches it through the author branch.
pub(in crate::account) fn affected_streams_for_account(
    tx: &Transaction<'_>,
    account_id: AccountId,
    previously_owned: &[[u8; 32]],
) -> anyhow::Result<Vec<StreamId>> {
    // The `/3` tables are created by a LATER migration than the account authority projection, and
    // the V064/V065 authority backfill folds every existing account inside its own migration — so
    // this runs before `content_entries` exists on an upgrading database. There is no content to
    // classify then; skip until the table is present.
    if !content_entries_exists(tx)? {
        return Ok(Vec::new());
    }
    // `previously_owned` is the account's owned-stream set captured BEFORE this fold rewrote the
    // projection. A fold that DROPS a `StreamOwn` fact must still refold that stream (to declassify
    // its now-authority-less content) — but the ownership row is already gone from the query below,
    // so the caller passes the pre-rewrite set and we union it in.
    let mut streams: HashSet<[u8; 32]> = previously_owned.iter().copied().collect();
    {
        let mut stmt = tx.prepare(
            "SELECT stream_id FROM account_stream_ownership WHERE account_id = ?1
             UNION
             SELECT stream_id FROM content_entries WHERE author_account_id = ?1",
        )?;
        let rows = stmt
            .query_map([account_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for stream_bytes in rows {
            streams.insert(fixed::<32>(&stream_bytes)?);
        }
    }
    let mut streams = streams.into_iter().map(StreamId::from_bytes).collect::<Vec<_>>();
    streams.sort_by_key(|stream| stream.to_bytes());
    Ok(streams)
}

/// Finish one stream after either trusted/local account-state work or deferred remote work.
/// Reprojection is mandatory even when the accepted set did not change: an account change can make
/// a previously unprojectable sealed body projectable (C5). The queue row is cleared only after all
/// current finalization duties succeed. Add the future transport notification hook (#691) here,
/// before the clear, so a failed hook cannot lose the wakeup.
pub(super) fn refold_and_project_stream_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<()> {
    refold_content_stream(tx, stream_id)?;
    if content_projected_tables_exist(tx)? {
        content_projection::reproject_accepted_content_stream(tx, stream_id)?;
    }
    if pending_refold_table_exists(tx)? {
        clear_pending_content_refold(tx, stream_id)?;
    }
    // Accepted suite-1 content pins its sealing key as a live enrollment catch-up target, so
    // settling content can grow an outstanding invite's mandatory redemption cost without any
    // account fold — refresh reservations here, at the content-acceptance choke point (#945).
    super::super::storage::refresh_enrollment_reservations_for_stream_in_tx(tx, stream_id, now_ms)?;
    Ok(())
}

pub(in crate::account) fn finalize_affected_streams(
    tx: &Transaction<'_>,
    streams: &[StreamId],
    now_ms: i64,
) -> anyhow::Result<()> {
    for &stream_id in streams {
        refold_and_project_stream_in_tx(tx, stream_id, now_ms)?;
    }
    Ok(())
}

pub(in crate::account) fn queue_account_changed_streams(
    tx: &Transaction<'_>,
    streams: &[StreamId],
    now_ms: i64,
) -> rusqlite::Result<()> {
    for &stream_id in streams {
        mark_stream_pending_refold(tx, stream_id, PENDING_REFOLD_ACCOUNT_CHANGE, now_ms)?;
    }
    Ok(())
}

/// Whether `content_entries` exists yet — false while an upgrading DB is mid-migration, before the
/// `/3` tables are created. `sqlite_master` is served from SQLite's in-memory schema, so this is a
/// cheap guard, not a table scan.
fn content_entries_exists(tx: &Transaction<'_>) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'content_entries'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
}

/// Re-derive `/3` acceptance for one stream from the current fold (the only writer of
/// `accepted = 1`). `pub(super)` so the in-tx content-author seam [`super::author`] can fold the
/// batch of owner-authored entries it just inserted — ONE refold per batch, then it verifies each
/// entry came back `accepted` inside the same txn or rolls the whole batch back.
pub(super) fn refold_content_stream(
    tx: &Transaction<'_>,
    stream_id: StreamId,
) -> anyhow::Result<()> {
    // This body only derives acceptance. Callers that finalize observable content must pair it with
    // reprojection; [`refold_and_project_stream_in_tx`] is the shared queue-discharging seam. The
    // owner is inside the stream identity
    // (`stream_id = sha256(cbor([.., owner,
    // ..]))`, §14) but not invertible, so it is resolved through the owner's `StreamOwn` fact.
    // No fact ⇒ authority cannot be evaluated: the entries revert to their structural state.
    // This is a DECLASSIFY, not a skip — if ownership was dropped by a later fold (owner
    // contested / branch reselection), previously accepted content must lose `accepted` here,
    // or it would stay live with no current authority basis.
    let Some(owner_account_id) = account_storage::stream_owner_account(tx, stream_id)? else {
        declassify_stream_to_structural(tx, stream_id)?;
        store_stream_clock(tx, stream_id, 0)?;
        return Ok(());
    };

    let resolved = resolve_stream_authority(tx, stream_id, owner_account_id)?;

    // Clear every `accepted` on the stream up front — so the `content_accepted_slot` partial-unique
    // index never transiently sees two accepted rows at one `(stream, author, device, seq)` (I10a),
    // and so a row we could NOT decode (absent from `resolved`) cannot keep a stale `accepted`. Its
    // status is reset too, or a corrupt blob would retain a stale `accepted{…}` verdict.
    tx.execute("UPDATE content_entries SET accepted = 0 WHERE stream_id = ?1", [stream_id
        .to_bytes()
        .as_slice()])?;
    let handled: HashSet<AccountEntryHash> = resolved.iter().map(|r| r.entry_hash).collect();
    declassify_rows_absent_from(tx, stream_id, &handled)?;
    if resolved.is_empty() {
        store_stream_clock(tx, stream_id, 0)?;
        return Ok(());
    }
    let view: HashMap<AccountEntryHash, ContentEntryHeader> =
        resolved.iter().map(|r| (r.entry_hash, r.header.clone())).collect();

    // Phase 1 — eligibility. An entry the authority pass condemns or rejects must NOT compete for a
    // dense seq slot (§16.2): a small-hash entry mined beyond a cut would otherwise win the
    // unforced tiebreak and fork an honest sibling off the accepted branch.
    let mut eligible = HashSet::new();
    for r in &resolved {
        if verdict_for(r, &view, false, EvaluatorPhase::Eligibility)?.is_none() {
            eligible.insert(r.entry_hash);
        }
    }

    // The register watermarks that pin a branch are the same cut boundaries the authority pass
    // resolved: §16 pins the accepted branch to the highest admitted watermark. `pinned_branch`
    // re-validates each against its coordinate, so over-collecting is safe.
    let pins = branch_pins(&resolved);
    let candidates: Vec<ContentCandidate> = resolved
        .iter()
        .map(|r| ContentCandidate { entry_hash: r.entry_hash, header: r.header.clone() })
        .collect();
    let selection = candidate::select_accepted_branch(&candidates, &eligible, &pins, &view);

    // Phase 2 — the finished verdict, made prefix-closed, then the write (`accepted` was cleared
    // above). The raw per-entry verdict needs two corrections before it is the truth:
    //  - Freshness is evaluated AFTER branch selection (§13), so a selected entry can still park
    //    `auth_len_ahead`. The accepted set is a contiguous prefix from seq 0 — a descendant must
    //    not stay accepted over a parked ancestor — so each chain is truncated at its first
    //    non-accepted winner. (An attacker varying cited `auth_len` down a chain is the trigger.)
    //  - `select_accepted_branch` leaves an entry stranded above an ineligible/unselected parent in
    //    NEITHER `accepted` nor `forked` — it lost no contest. Passing `branch_selected = false`
    //    would make the evaluator call it `Forked`, a terminal loser state it is not; only the real
    //    losers in `selection.forked` fork, and a stranded entry parks (recoverable).
    let mut raw: HashMap<AccountEntryHash, ContentAcceptance> =
        HashMap::with_capacity(resolved.len());
    for r in &resolved {
        let selected = selection.accepted.contains(&r.entry_hash);
        let verdict = verdict_for(r, &view, selected, EvaluatorPhase::Finished)?
            .expect("the finished evaluator always returns a verdict");
        raw.insert(r.entry_hash, verdict);
    }
    let accepted = prefix_closed_accepted(&resolved, &selection, &raw);
    // The condemned rows form the CLOCK BASIS alongside the accepted set (see
    // [`bounded_advance_walk`]): a revoked writer's condemned entries stop projecting, but an
    // honest dependent that minted against them while they were accepted must not park — or
    // revocation, the designed repair path, would itself wedge the dependent chain.
    let condemned: Vec<(AccountEntryHash, &ContentEntryHeader)> = resolved
        .iter()
        .filter(|r| matches!(raw.get(&r.entry_hash), Some(ContentAcceptance::Condemned(_))))
        .map(|r| (r.entry_hash, &r.header))
        .collect();
    let (accepted, lamport_parked, clock) =
        lamport_advance_clamped(&resolved, accepted, &condemned);
    // Persist the floor for the O(1) ingest-gate and authoring-mint clock reads.
    store_stream_clock(tx, stream_id, clock)?;
    for r in &resolved {
        let hash = r.entry_hash;
        let verdict = if accepted.contains(&hash) {
            ContentAcceptance::Accepted
        } else if lamport_parked.contains(&hash) {
            ContentAcceptance::Parked(ContentParkReason::LamportAhead)
        } else if selection.forked.contains(&hash) {
            ContentAcceptance::Forked
        } else {
            match raw[&hash] {
                // Selected + fresh, but truncated by a parked ancestor: blocked on that ancestor
                // catching up, so it parks with the same freshness reason instead of accepting.
                ContentAcceptance::Accepted =>
                    ContentAcceptance::Parked(ContentParkReason::AuthorAuthLenAhead),
                // Eligible but stranded above an unselected parent — recoverable, not a terminal
                // fork. It has no accepted predecessor to build on, so it parks like one missing.
                ContentAcceptance::Forked =>
                    ContentAcceptance::Parked(ContentParkReason::MissingPredecessor),
                other => other,
            }
        };
        write_verdict(tx, &hash, verdict)?;
    }
    Ok(())
}

/// Merge deferred work for one stream. `IMMEDIATE` transaction serialization plus the single UPSERT
/// prevents a concurrent settle/enqueue lost wakeup: reasons accumulate, the first timestamp is
/// stable, and the latest enqueue refreshes the last timestamp.
fn mark_stream_pending_refold(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    reason_mask: i64,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO content_streams_pending_refold(
             stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
         VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(stream_id) DO UPDATE SET
             reason_mask = content_streams_pending_refold.reason_mask | excluded.reason_mask,
             last_enqueued_at_ms = excluded.last_enqueued_at_ms",
        params![stream_id.to_bytes().as_slice(), reason_mask, now_ms],
    )?;
    Ok(())
}

/// Drop a stream's deferred-refold mark after shared finalization completed all duties.
fn clear_pending_content_refold(tx: &Transaction<'_>, stream_id: StreamId) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM content_streams_pending_refold WHERE stream_id = ?1", [stream_id
        .to_bytes()
        .as_slice()])?;
    Ok(())
}

/// One queued stream's current settle cost. The cost comes from the V082 `content_stream_stats`
/// aggregate (counts and `length(signed_bytes) + 32` per row, trigger-maintained) — never a
/// `COUNT(*)`/`SUM` over the stream's candidate rows, which would make admission itself
/// attacker-triggered O(stream).
#[derive(Clone, Copy, Debug)]
struct PendingRefoldWork {
    candidate_count: u64,
    candidate_bytes: u64,
}

/// One row of a BOUNDED fairness-ordered queue page: the stream identity, its position in the
/// fairness order (the keyset cursor for the next page), and its LISTED fold cost joined in from
/// the V082 `content_stream_stats` aggregate (counts and `length(signed_bytes) + 32` per row,
/// trigger-maintained). The listing query filters eligibility in SQL, so every listed row fits a
/// FRESH candidate/byte budget; the listed cost then classifies rows that no longer fit the
/// REMAINING budget without a transaction, and anything that can still be admitted is re-read
/// inside the admission transaction (a concurrent ingest can only GROW the cost).
#[derive(Clone, Copy, Debug)]
struct PendingRefoldListing {
    first_enqueued_at_ms: i64,
    stream_id: StreamId,
    listed_work: PendingRefoldWork,
}

/// The number of queue rows one listing page holds: proportional to the stream-slot axis (the only
/// budget axis that bounds ADMISSIONS before per-stream costs are known — candidate/byte costs
/// cannot size a listing), with slack for races/vanishes and a ceiling so an unbounded budget
/// still pages instead of materializing the whole queue. Never O(queue).
fn settle_candidate_batch_size(budget: &ContentRefoldBudget) -> usize {
    const SLACK: u64 = 8;
    const MAX_BATCH: u64 = 512;
    let size = budget.max_streams.saturating_mul(2).saturating_add(SLACK).min(MAX_BATCH);
    usize::try_from(size).unwrap_or(usize::MAX)
}

/// Fetch one bounded page of dirty streams, oldest first (`first_enqueued_at_ms, stream_id`),
/// strictly after the keyset `cursor`. The LEFT JOIN brings each row's O(1) fold cost along in the
/// SAME paged read — never a whole-queue join and never a `COUNT(*)`/`SUM` over candidate rows,
/// which would make listing itself attacker-triggered O(queue).
///
/// Eligibility is filtered INSIDE the query (#798 Codex P1): only rows whose stored stats fit a
/// FRESH candidate/byte budget are returned (a missing stats row is zero cost), so oversize rows
/// are never listed — they cannot head-of-line block the smaller rows behind them, and later
/// calls never re-list them.
///
/// The `LIMIT` bounds the rows RETURNED, not the rows examined: the eligibility predicates are
/// applied before it, so SQLite walks the `content_streams_pending_refold_order` index and probes
/// `content_stream_stats` per row until it collects a full page. A queue that is entirely oversize
/// therefore costs one index walk, not one page. That stays bounded in practice by the global
/// candidate/account caps, but it is a walk, not a point read.
fn list_pending_refold_streams_page(
    conn: &Connection,
    cursor: Option<(i64, StreamId)>,
    budget: &ContentRefoldBudget,
    limit: usize,
) -> anyhow::Result<Vec<PendingRefoldListing>> {
    #[cfg(test)]
    SETTLE_LISTING_QUERIES.with(|c| c.set(c.get() + 1));
    let (after_ms, after_stream) = match &cursor {
        Some((ms, stream)) => (Some(*ms), Some(stream.to_bytes().to_vec())),
        None => (None, None),
    };
    // The stats columns are non-negative i64; a cap above i64::MAX (an unbounded budget) admits
    // every stored value.
    let max_candidates = i64::try_from(budget.max_candidates).unwrap_or(i64::MAX);
    let max_candidate_bytes = i64::try_from(budget.max_candidate_bytes).unwrap_or(i64::MAX);
    let rows = {
        let mut stmt = conn.prepare(
            "SELECT q.first_enqueued_at_ms, q.stream_id,
                    COALESCE(s.candidate_count, 0), COALESCE(s.candidate_bytes, 0)
             FROM content_streams_pending_refold q
             LEFT JOIN content_stream_stats s ON s.stream_id = q.stream_id
             WHERE (?1 IS NULL OR (q.first_enqueued_at_ms, q.stream_id) > (?1, ?2))
               AND COALESCE(s.candidate_count, 0) <= ?4
               AND COALESCE(s.candidate_bytes, 0) <= ?5
             ORDER BY q.first_enqueued_at_ms, q.stream_id
             LIMIT ?3",
        )?;
        stmt.query_map(
            params![
                after_ms,
                after_stream,
                i64::try_from(limit)?,
                max_candidates,
                max_candidate_bytes
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    rows.iter()
        .map(|(ms, stream, count, bytes)| {
            Ok(PendingRefoldListing {
                first_enqueued_at_ms: *ms,
                stream_id: StreamId::from_bytes(fixed::<32>(stream)?),
                listed_work: PendingRefoldWork {
                    // The stats columns carry `CHECK(... >= 0)`, so the narrowing casts cannot
                    // wrap.
                    candidate_count: *count as u64,
                    candidate_bytes: *bytes as u64,
                },
            })
        })
        .collect()
}

/// Whether the LISTED cost still fits the REMAINING budget. The listing query already filtered to
/// rows that fit a FRESH budget, so a listed miss is a remaining-budget deferral — consumption and
/// concurrent ingestion only grow costs within the call, making the classification stable enough
/// to skip without a transaction. A listed fit must still be PROBED (revalidated in the IMMEDIATE
/// transaction, since a concurrent ingest can grow the cost after the listing).
fn listed_fits_remaining(
    budget: &ContentRefoldBudget,
    consumed: ContentSettleConsumption,
    listed_work: PendingRefoldWork,
) -> bool {
    consumed.attempted_streams < budget.max_streams
        && consumed.candidates.saturating_add(listed_work.candidate_count) <= budget.max_candidates
        && consumed.candidate_bytes.saturating_add(listed_work.candidate_bytes)
            <= budget.max_candidate_bytes
}

/// Whether ANY further admission could still fit the budget. Once this is false, later queued rows
/// can only be deferred, so the settle stops paging without touching them. An accounting axis
/// exactly at its cap ends discovery (only a zero-cost stream could still fit); the caller's
/// `remaining`-driven loop picks such residue up with a fresh budget.
fn budget_could_fit_more(budget: &ContentRefoldBudget, consumed: ContentSettleConsumption) -> bool {
    consumed.attempted_streams < budget.max_streams
        && consumed.candidates < budget.max_candidates
        && consumed.candidate_bytes < budget.max_candidate_bytes
}

// Test-only work counters proving a settle call's SQL/lock work is bounded by the budget and not
// the backlog (#798 review): listing queries (page listings plus the one targeted oversize query
// in maintenance mode), per-stream admission probes (each an IMMEDIATE transaction), and the one
// O(1) queue-empty completion probe. Production semantics are unchanged.
//
// THREAD-LOCAL, not `static`: the CI coverage job runs `cargo llvm-cov` over `cargo test`, which
// shares ONE process per test binary and runs tests on concurrent threads, so process-global
// counters read whatever sibling settle tests happened to add in the window. `cargo test` gives
// each test its own thread and the settle path is synchronous rusqlite, so per-thread counting is
// exact under both that runner and nextest. A future test that settles on a SPAWNED thread would
// read 0 here — assert counters only on the thread that called settle.
#[cfg(test)]
thread_local! {
    static SETTLE_LISTING_QUERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SETTLE_ADMISSION_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SETTLE_COMPLETION_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Revalidate both queue membership and O(1) fold cost under the transaction that may fold it.
fn pending_refold_work_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
) -> anyhow::Result<Option<PendingRefoldWork>> {
    tx.query_row(
        "SELECT COALESCE(s.candidate_count, 0), COALESCE(s.candidate_bytes, 0)
         FROM content_streams_pending_refold q
         LEFT JOIN content_stream_stats s ON s.stream_id = q.stream_id
         WHERE q.stream_id = ?1",
        [stream_id.to_bytes().as_slice()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )
    .optional()?
    .map(|(count, bytes)| {
        Ok(PendingRefoldWork {
            // The stats columns carry `CHECK(... >= 0)`, so the narrowing casts cannot wrap.
            candidate_count: count as u64,
            candidate_bytes: bytes as u64,
        })
    })
    .transpose()
}

/// Whether ANY queue row remains, as an O(1) `EXISTS` probe (#798 Codex P2 / adversarial review):
/// callers need only drained-vs-not (plus the progress counters), so an exact `COUNT(*)` over the
/// whole pending queue — O(queue) per call, quadratic across a max_streams=1 drain — is
/// deliberately NOT taken.
fn pending_refold_queue_nonempty(conn: &Connection) -> anyhow::Result<bool> {
    #[cfg(test)]
    SETTLE_COMPLETION_PROBES.with(|c| c.set(c.get() + 1));
    let nonempty =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM content_streams_pending_refold)", [], |row| {
            row.get::<_, i64>(0)
        })?;
    Ok(nonempty != 0)
}

/// Move a FAILED stream's queue row behind every currently-queued row, so the next call tries
/// other streams first instead of head-of-line blocking on a poisoned stream forever (#798
/// adversarial review). The row stays QUEUED — its refold debt is real and a later call retries
/// it; only its fairness position changes. `MAX(...) + 1` is index-backed
/// (`content_streams_pending_refold_order`), never a scan.
///
/// This MUST NOT run INSIDE the paging loop (#798 adversarial finding F1): the bump moves the row
/// to `MAX(first_enqueued_at_ms) + 1`, AHEAD of the advancing keyset cursor, so a persistently
/// failing OLDEST stream would re-list and re-fold on every later page of the SAME settle call
/// (duplicate `failures`, redundant folds, premature budget consumption that starves healthy
/// later-page rows). Callers collect the failed stream ids during the loop and apply them once,
/// after paging, via [`demote_pending_refolds`].
fn demote_pending_refold(conn: &Connection, stream_id: StreamId) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE content_streams_pending_refold
         SET first_enqueued_at_ms = (SELECT COALESCE(MAX(first_enqueued_at_ms), 0) + 1
                                     FROM content_streams_pending_refold)
         WHERE stream_id = ?1",
        [stream_id.to_bytes().as_slice()],
    )?;
    Ok(())
}

/// Apply the settle call's collected fairness demotions in ONE committed IMMEDIATE transaction,
/// AFTER the paging loop finished (#798 adversarial finding F1). Demoting inside the loop moved a
/// failed row ahead of the keyset cursor and re-listed it on later pages; deferring every demotion
/// to this single post-loop pass keeps each failed stream attempted exactly once per call while
/// still bumping it behind every currently-queued row for the NEXT call. The pass is committed
/// independently of (and survives) the rolled-back per-stream txns. Demotions are applied in
/// collection order (oldest failed stream first); each row lands at `MAX + 1` of the queue as it
/// stands at that point, so the bumped rows keep their relative order at the back.
fn demote_pending_refolds(conn: &Connection, stream_ids: &[StreamId]) -> anyhow::Result<()> {
    if stream_ids.is_empty() {
        return Ok(());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    for &stream_id in stream_ids {
        demote_pending_refold(&tx, stream_id)?;
    }
    tx.commit()?;
    Ok(())
}

/// Find the OLDEST queued stream whose stored stats exceed a FRESH candidate/byte budget — the
/// rows the eligibility-filtered listing never returns. Oversize maintenance mode only, and only
/// when normal discovery listed ZERO eligible rows on its first page (oversize rows are then the
/// only remaining work): the `LIMIT 1` is NOT a point read — the
/// `content_streams_pending_refold_order` index gives oldest-first order, but with no oversize row
/// present SQLite scans the whole queue to prove absence, so running it whenever a slot remained
/// would be an O(queue) probe on every budgeted call (#798 adversarial review). Missing stats are
/// zero cost, so a stats-less row is never oversize; never per-page work and never a Rust-side scan
/// of the queue.
fn oldest_oversize_pending_refold(
    conn: &Connection,
    budget: &ContentRefoldBudget,
) -> anyhow::Result<Option<StreamId>> {
    #[cfg(test)]
    SETTLE_LISTING_QUERIES.with(|c| c.set(c.get() + 1));
    let max_candidates = i64::try_from(budget.max_candidates).unwrap_or(i64::MAX);
    let max_candidate_bytes = i64::try_from(budget.max_candidate_bytes).unwrap_or(i64::MAX);
    conn.query_row(
        "SELECT q.stream_id
         FROM content_streams_pending_refold q
         LEFT JOIN content_stream_stats s ON s.stream_id = q.stream_id
         WHERE COALESCE(s.candidate_count, 0) > ?1
            OR COALESCE(s.candidate_bytes, 0) > ?2
         ORDER BY q.first_enqueued_at_ms, q.stream_id
         LIMIT 1",
        params![max_candidates, max_candidate_bytes],
        |row| row.get::<_, Vec<u8>>(0),
    )
    .optional()?
    .map(|stream| Ok(StreamId::from_bytes(fixed::<32>(&stream)?)))
    .transpose()
}

/// Whether the V070 `content_projected_*` tables exist yet — false on a DB upgrading past a
/// pre-V070 ledger, where the reproject would target absent tables. `sqlite_master` is served from
/// SQLite's in-memory schema, so this is cheap; mirrors [`content_entries_exists`]. Takes a
/// `&Connection` (a `&Transaction` derefs to it) so the open-path upgrade re-fold
/// ([`content_projection::rebuild_all_content_projections_if_stale`], #688) shares the one guard.
pub(crate) fn content_projected_tables_exist(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'content_projected_nodes'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
}

fn pending_refold_table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master
         WHERE type = 'table' AND name = 'content_streams_pending_refold'",
        [],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
}

/// Whether this exact stream still owes deferred acceptance folding and reprojection. Stores being
/// upgraded from before the queue table was introduced have no deferred debt, so they return false.
pub fn content_stream_has_pending_refold(
    conn: &Connection,
    stream_id: StreamId,
) -> rusqlite::Result<bool> {
    if !pending_refold_table_exists(conn)? {
        return Ok(false);
    }
    conn.query_row(
        "SELECT 1 FROM content_streams_pending_refold WHERE stream_id = ?1",
        [stream_id.to_bytes().as_slice()],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
}

#[derive(Clone, Copy, Debug, Default)]
struct ContentSettleConsumption {
    attempted_streams: u64,
    candidates: u64,
    candidate_bytes: u64,
    oversize_slot_spent: bool,
}

enum PendingRefoldOutcome {
    Missing,
    Deferred {
        oversize: bool,
    },
    Settled {
        work: PendingRefoldWork,
        oversize: bool,
    },
    Failed {
        admitted: Option<(PendingRefoldWork, bool)>,
        error: anyhow::Error,
    },
    /// BEGIN IMMEDIATE failed twice (a lock/BUSY race): NOT stream poison — no budget charge, no
    /// failure entry, no demotion. Counted separately in [`ContentSettleReport::lock_failures`].
    TransientLock {
        error: anyhow::Error,
    },
}

/// Open the per-stream IMMEDIATE transaction, retrying a begin failure ONCE: a lock/BUSY race at
/// BEGIN says nothing about the stream's foldability and must not be classified (or charged, or
/// demoted) as stream poison (#798 adversarial review).
fn begin_immediate_tx(conn: &Connection) -> Result<Transaction<'_>, PendingRefoldOutcome> {
    match Transaction::new_unchecked(conn, TransactionBehavior::Immediate) {
        Ok(tx) => Ok(tx),
        Err(first) => match Transaction::new_unchecked(conn, TransactionBehavior::Immediate) {
            Ok(tx) => Ok(tx),
            Err(second) => Err(PendingRefoldOutcome::TransientLock {
                error: anyhow::anyhow!("begin immediate failed twice ({first}): {second}"),
            }),
        },
    }
}

/// Revalidate and, if admitted, settle ONE dirty stream in the SAME IMMEDIATE transaction. A
/// failure rolls back only this stream and leaves its queue mark intact for a later retry.
fn settle_one_pending_refold(
    conn: &Connection,
    stream_id: StreamId,
    budget: &ContentRefoldBudget,
    consumed: ContentSettleConsumption,
    now_ms: i64,
) -> PendingRefoldOutcome {
    #[cfg(test)]
    SETTLE_ADMISSION_PROBES.with(|c| c.set(c.get() + 1));
    let tx = match begin_immediate_tx(conn) {
        Ok(tx) => tx,
        Err(outcome) => return outcome,
    };
    let work = match pending_refold_work_in_tx(&tx, stream_id) {
        Ok(Some(work)) => work,
        Ok(None) => return PendingRefoldOutcome::Missing,
        Err(error) => return PendingRefoldOutcome::Failed { admitted: None, error },
    };
    let stream_slot_remaining = consumed.attempted_streams < budget.max_streams;
    let fits_fresh_budget = work.candidate_count <= budget.max_candidates
        && work.candidate_bytes <= budget.max_candidate_bytes;
    let fits_remaining_budget = stream_slot_remaining
        && consumed.candidates.saturating_add(work.candidate_count) <= budget.max_candidates
        && consumed.candidate_bytes.saturating_add(work.candidate_bytes)
            <= budget.max_candidate_bytes;
    let oversize = if fits_remaining_budget {
        false
    } else if !fits_fresh_budget {
        if !(stream_slot_remaining && budget.allow_one_oversize && !consumed.oversize_slot_spent) {
            return PendingRefoldOutcome::Deferred { oversize: true };
        }
        true
    } else {
        return PendingRefoldOutcome::Deferred { oversize: false };
    };

    if let Err(error) = refold_and_project_stream_in_tx(&tx, stream_id, now_ms) {
        return PendingRefoldOutcome::Failed { admitted: Some((work, oversize)), error };
    }
    match tx.commit() {
        Ok(()) => PendingRefoldOutcome::Settled { work, oversize },
        Err(error) =>
            PendingRefoldOutcome::Failed { admitted: Some((work, oversize)), error: error.into() },
    }
}

/// Fold one probe outcome into the report and the running consumption: an ADMITTED attempt (a
/// settle or a failure after admission) charges every budget axis whether it commits or not.
/// Returns `(admitted, vanished, demote)` so the paging loop knows whether the page made progress
/// and whether the stream's queue row must be demoted (any post-begin failure is treated as
/// poison for scheduling; a begin/lock failure is transient and never demotes).
fn record_settle_outcome(
    report: &mut ContentSettleReport,
    consumed: &mut ContentSettleConsumption,
    stream_id: StreamId,
    outcome: PendingRefoldOutcome,
) -> (bool, bool, bool) {
    let admitted_work = match &outcome {
        PendingRefoldOutcome::Settled { work, oversize }
        | PendingRefoldOutcome::Failed { admitted: Some((work, oversize)), .. } =>
            Some((*work, *oversize)),
        _ => None,
    };
    if let Some((work, oversize)) = admitted_work {
        consumed.attempted_streams = consumed.attempted_streams.saturating_add(1);
        consumed.candidates = consumed.candidates.saturating_add(work.candidate_count);
        consumed.candidate_bytes = consumed.candidate_bytes.saturating_add(work.candidate_bytes);
        consumed.oversize_slot_spent |= oversize;
        report.consumed_candidates = consumed.candidates;
        report.consumed_candidate_bytes = consumed.candidate_bytes;
    }
    let vanished = matches!(outcome, PendingRefoldOutcome::Missing);
    let demote = matches!(outcome, PendingRefoldOutcome::Failed { .. });
    match outcome {
        PendingRefoldOutcome::Missing => {},
        PendingRefoldOutcome::Deferred { oversize: true } => report.deferred_oversize += 1,
        PendingRefoldOutcome::Deferred { oversize: false } => report.deferred_budget += 1,
        PendingRefoldOutcome::Settled { .. } => {
            report.settled_streams += 1;
        },
        PendingRefoldOutcome::Failed { error, .. } => report
            .failures
            .push(ContentStreamSettleFailure { stream_id, error: format!("{error:#}") }),
        // A begin-lock race is transient and not stream poison: the crate has no logging
        // facility, so the diagnostic surfaces via the `lock_failures` counter on the report
        // rather than a log line, and the underlying error is intentionally discarded.
        PendingRefoldOutcome::TransientLock { error: _ } => {
            report.lock_failures += 1;
        },
    }
    (admitted_work.is_some(), vanished, demote)
}

/// The work one [`settle_pending_content_refolds`] call may start. Streams are admitted oldest
/// first (`first_enqueued_at_ms, stream_id`); a stream is admitted only while it fits the
/// REMAINING budget on every axis, so one call's cost stays bounded and the queue resumes where
/// the budget ran out.
///
/// Counts and bytes are the V082 `content_stream_stats` fold-cost units: a candidate row and
/// `length(signed_bytes) + 32` bytes per row (the payload a full refold's `load_stream_headers`
/// copies out of SQLite).
///
/// A stream that could never fit a FRESH budget (its candidates or bytes alone exceed the cap) is
/// OVERSIZE. Normal mode (`allow_one_oversize: false`) never starts it — the eligibility-filtered
/// listing never even returns it, so it stays queued while smaller streams still settle (no
/// head-of-line blocking) — so a normal-mode caller with a persistent oversize stream never
/// converges. Oversize maintenance mode (`allow_one_oversize: true`) settles at most ONE oldest
/// oversize stream per call in its own transaction, an intentional budget exceedance visible in
/// the report's consumed counters: that is the scheduled-maintenance convergence path. The
/// oversize probe runs only when normal discovery listed ZERO eligible rows on its first page —
/// oversize rows are then the only work left — because the `LIMIT 1` probe degenerates to a full
/// queue scan when no oversize row exists (#798 adversarial review).
/// Hard-bounded partial folds are deliberately out of scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentRefoldBudget {
    pub max_streams: u64,
    pub max_candidates: u64,
    pub max_candidate_bytes: u64,
    pub allow_one_oversize: bool,
}

impl ContentRefoldBudget {
    /// No limit: every queued stream is admitted in one call. For internal callers (tests,
    /// trusted-path maintenance) that must keep the original "settle everything" behavior;
    /// transport-facing callers take a hard budget instead.
    pub const fn unbounded() -> Self {
        Self {
            max_streams: u64::MAX,
            max_candidates: u64::MAX,
            max_candidate_bytes: u64::MAX,
            allow_one_oversize: false,
        }
    }
}

/// A stream whose settle failed. Its queue row is RETAINED (the per-stream txn rolled back) and
/// DEMOTED behind every currently-queued row (see [`ContentSettleReport::failures`]), so the next
/// call tries other streams first; the batch is not aborted and the error never blocks other
/// streams.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentStreamSettleFailure {
    pub stream_id: StreamId,
    pub error: String,
}

/// What one [`settle_pending_content_refolds`] call did. `queue_empty` is an O(1) `EXISTS`
/// observation after the pass — an exact queue magnitude is deliberately NOT reported: computing
/// it costs a `COUNT(*)` over the whole pending queue per call (#798 Codex P2). The counters
/// relate to the queue in BOTH inequality directions: they classify only the DISCOVERED
/// candidates (the bounded pages this call actually read), so the untouched backlog, concurrent
/// enqueues, and SQL-filtered oversize rows can keep the queue non-empty while every counter is
/// zero; and a `failures` entry names a stream whose row could concurrently vanish, so the
/// counters can also describe rows the queue no longer holds. In particular, rows the
/// eligibility-filtered listing excludes (they exceed a fresh budget) are never discovered — see
/// [`ContentSettleReport::deferred_oversize`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContentSettleReport {
    /// Streams fully settled (fold + reproject + queue clear committed) this call.
    pub settled_streams: usize,
    /// Fold-cost candidates charged by ADMITTED attempts, including failures. May exceed the
    /// budget's `max_candidates` ONLY by the one intentional oversize attempt.
    pub consumed_candidates: u64,
    /// Fold-cost bytes charged by admitted attempts (same oversize-exceedance caveat).
    pub consumed_candidate_bytes: u64,
    /// Discovered streams never started because they did not fit the REMAINING budget; each fits
    /// a fresh budget, so the next call makes progress on them.
    pub deferred_budget: usize,
    /// Streams the IN-TRANSACTION revalidation observed exceeding a FRESH budget (their cost grew
    /// past the caps between the paged listing and admission) and could not take the oversize
    /// slot. Rows that exceed the caps at listing time are filtered out in SQL and never
    /// discovered, so they are NOT counted here — a caller that makes no progress while
    /// `queue_empty` stays false should schedule an oversize-maintenance pass. Only oversize
    /// maintenance mode converges such rows.
    pub deferred_oversize: usize,
    /// Streams started but rolled back; their queue rows are retained for retry and demoted
    /// behind all currently-queued rows, so later calls try other streams first.
    pub failures: Vec<ContentStreamSettleFailure>,
    /// BEGIN IMMEDIATE lock/BUSY races that survived one retry. NOT stream poison: no budget was
    /// charged, the row keeps its fairness position, and the stream appears in no other counter.
    pub lock_failures: usize,
    /// Whether the queue was empty at this O(1) post-pass observation. The transport caller loops
    /// while this is false AND the last call made progress; it never learns the exact backlog.
    pub queue_empty: bool,
    /// Queue rows the paged listing actually returned this call. Zero with `queue_empty == false`
    /// is the ONLY signal distinguishing "the remaining work is all SQL-filtered oversize rows"
    /// (schedule an oversize-maintenance pass) from a poisoned stream, lock contention, or a
    /// listing that raced empty — every one of which otherwise reports identical all-zero counters
    /// (#798 adversarial finding 6). Costs nothing: it is the page lengths already read.
    pub discovered_rows: usize,
}

/// Fold the streams `content_ingest` deferred, oldest first, until `budget` runs out, and report
/// exactly what was settled, deferred, and retained. One refold per settled stream (O(settled
/// streams), NOT O(ingested entries)); each stream settles in its OWN IMMEDIATE transaction via
/// the shared [`refold_and_project_stream_in_tx`] finalizer, so a poisoned stream rolls back
/// alone — its queue mark is kept for retry and DEMOTED behind every currently-queued row (so the
/// next call tries other streams first instead of head-of-line blocking on the poisoned stream
/// forever), it lands in [`ContentSettleReport::failures`], and it never blocks the rest of the
/// batch. The demotions are COLLECTED during paging and applied once AFTER the loop (a single
/// committed pass, [`demote_pending_refolds`]): bumping a failed row mid-loop moves it ahead of
/// the keyset cursor, so a persistently failing oldest stream would re-list and re-fold on every
/// later page of the SAME call (#798 adversarial finding F1) — deferring the bump keeps each
/// failed stream attempted exactly once per call. A BEGIN IMMEDIATE lock/BUSY race is NOT poison:
/// it is retried once, then counted in
/// [`ContentSettleReport::lock_failures`] with no budget charge and no demotion. Only store/setup
/// failures (e.g. the queue snapshot itself) make the overall call `Err`.
///
/// Admission revalidates queue membership and charges the O(1) `content_stream_stats` aggregate
/// INSIDE the same IMMEDIATE transaction that will fold — counting a targeted stream's rows would
/// itself be attacker-triggered O(n), while reading before writer serialization would admit stale
/// costs. Every admitted attempt consumes stream/candidate/byte budget even if it rolls back.
/// Normal mode skips oversize streams without blocking smaller ones;
/// [`ContentRefoldBudget::allow_one_oversize`] permits one oldest oversize attempt only while a
/// stream slot remains AND normal discovery listed zero eligible rows.
///
/// Candidate discovery is BOUNDED and progressive: the fairness-ordered listing is paged (O(budget)
/// rows per page, keyset on `first_enqueued_at_ms, stream_id`) and filters eligibility INSIDE the
/// query — a row whose stored stats exceed a fresh candidate/byte budget is never listed, so
/// oversize rows cannot head-of-line block the smaller rows behind them and are never re-listed
/// by later calls. A listed row that no longer fits the REMAINING budget is deferred without a
/// transaction, and a further page is fetched only while more budget could still fit AND the last
/// page made progress (an admission or a race-vanished row). A call whose budget is exhausted
/// therefore touches O(budget) rows — never the whole backlog. Completion is reported as the O(1)
/// [`ContentSettleReport::queue_empty`] EXISTS probe, never a `COUNT(*)` over the queue (#798
/// Codex P2).
///
/// Oversize maintenance mode ([`ContentRefoldBudget::allow_one_oversize`]) runs AFTER normal
/// discovery, only when normal discovery listed ZERO eligible rows on its first page (oversize
/// rows are then the only remaining work — the probe degenerates to a full queue scan when no
/// oversize row exists, so it must not run on every call): ONE targeted `LIMIT 1` query finds the
/// oldest queued row exceeding the fresh caps, admitted through the same in-transaction
/// revalidation. Normal-mode callers learn about persistent oversize rows via `queue_empty`
/// staying false with no progress (`deferred_oversize` counts only rows the in-transaction
/// revalidation caught growing past the caps).
///
/// This is the transport-facing settle seam (#406): after transport drains a batch of foreign
/// ingests it calls this with a HARD budget and LOOPS WHILE PROGRESS — the batching contract is
/// "drain, then loop settle while the last call made progress and `queue_empty` is false;
/// RESCHEDULE otherwise", never settle per entry, and never claim convergence while queued work
/// remains. Progress means `settled_streams > 0` (failures/deferred rows alone are not progress);
/// demote-on-failure keeps a poisoned stream from starving the queue, but a call that made no
/// progress with `queue_empty == false` (persistent oversize rows, lock contention, or a poisoned
/// stream that just demoted past everything) must be RESCHEDULED, not immediately retried —
/// schedule an oversize-maintenance pass to converge filtered oversize rows. A non-empty
/// `failures` (or `lock_failures`) means acceptance for those streams is still not observable.
///
/// Remote account ingests enqueue the same settle debt with `ACCOUNT_CHANGE`; trusted/local account
/// folds still finalize immediately in their caller's transaction.
pub fn settle_pending_content_refolds(
    conn: &Connection,
    budget: &ContentRefoldBudget,
    now_ms: i64,
) -> anyhow::Result<ContentSettleReport> {
    settle_pending_content_refolds_inner(conn, budget, now_ms, || {})
}

fn settle_pending_content_refolds_inner(
    conn: &Connection,
    budget: &ContentRefoldBudget,
    now_ms: i64,
    after_first_listing: impl FnOnce(),
) -> anyhow::Result<ContentSettleReport> {
    let batch = settle_candidate_batch_size(budget);
    let mut after_first_listing = Some(after_first_listing);
    let mut cursor: Option<(i64, StreamId)> = None;
    let mut report = ContentSettleReport::default();
    let mut consumed = ContentSettleConsumption::default();
    // Bounded progressive discovery, with eligibility filtered INSIDE the paged query (#798 Codex
    // P1): a page holds only rows whose stored stats fit a FRESH candidate/byte budget, so
    // oversize rows are never listed — they cannot head-of-line block the smaller rows behind
    // them, and later calls never re-list them. A further page is fetched only while more budget
    // could still fit AND the previous page made progress (an admission consumed budget, or a
    // listed row vanished to a race) — so a normal settle with an exhausted budget never touches
    // later queued rows (#798 Codex P2: the old whole-queue snapshot made budgeted drains
    // quadratic). A listed row that fit the fresh budget but not the REMAINING budget is deferred
    // without a transaction; the rare row that GREW past the budget between listing and admission
    // is deferred by the in-transaction revalidation instead.
    // Whether the paging loop ended because ELIGIBLE work ran out (as opposed to the budget running
    // out with eligible rows still queued behind the cursor). Only the former licenses the oversize
    // probe below — see its comment for why "listed nothing at all" is the wrong test.
    let eligible_work_drained;
    // Fairness demotions are COLLECTED here and applied once after the paging loop (#798
    // adversarial finding F1): demoting a failed row inside the loop moves it to
    // `MAX(first_enqueued_at_ms) + 1`, ahead of the advancing keyset cursor, so a persistently
    // failing oldest stream would re-list and re-fold on every later page of THIS call. Deferring
    // the bump keeps a failed row behind the cursor (never re-listed), so it is attempted exactly
    // once per call and never starves the healthy later-page rows.
    let mut deferred_demotions: Vec<StreamId> = Vec::new();
    loop {
        let page = list_pending_refold_streams_page(conn, cursor, budget, batch)?;
        if let Some(hook) = after_first_listing.take() {
            hook();
        }
        if page.is_empty() {
            eligible_work_drained = true;
            break;
        }
        let page_len = page.len();
        report.discovered_rows += page_len;
        let mut page_admitted = false;
        let mut page_vanished = false;
        for listing in page {
            cursor = Some((listing.first_enqueued_at_ms, listing.stream_id));
            let stream_id = listing.stream_id;
            if !listed_fits_remaining(budget, consumed, listing.listed_work) {
                report.deferred_budget += 1;
                continue;
            }
            let outcome = settle_one_pending_refold(conn, stream_id, budget, consumed, now_ms);
            let (admitted, vanished, demote) =
                record_settle_outcome(&mut report, &mut consumed, stream_id, outcome);
            if demote {
                deferred_demotions.push(stream_id);
            }
            page_admitted |= admitted;
            page_vanished |= vanished;
        }
        // A SHORT page is the only exit that proves the eligible queue is exhausted: the other two
        // mean the budget ran out while eligible rows remain behind the cursor. Rows this call
        // deferred for remaining budget are still owed eligible work, so they veto it too.
        let last_page = page_len < batch;
        if last_page
            || !budget_could_fit_more(budget, consumed)
            || !(page_admitted || page_vanished)
        {
            eligible_work_drained = last_page && report.deferred_budget == 0;
            break;
        }
    }
    // Oversize maintenance: the listing above never discovers rows that exceed a fresh budget.
    // Probe for the OLDEST such row only once ELIGIBLE work is drained for this budget. The
    // `LIMIT 1` probe is oldest-first via the order index but degenerates to a FULL QUEUE SCAN when
    // no oversize row exists, so running it whenever a slot remained would make every budgeted call
    // O(queue) (#798 Codex review) — hence the gate. It must NOT be "the loop listed nothing at
    // all", though: that starves maintenance forever whenever the queue keeps producing any
    // listable row (a persistently failing small stream, or just steady remote ingest), so the
    // oversize row's acceptance and projection freeze permanently (#798 adversarial finding 1).
    // Draining eligible work first keeps the scan off the hot path while still guaranteeing that a
    // maintenance caller which reaches a quiet queue converges. The admission goes through the same
    // in-transaction revalidation, which honors the intentional exceedance and the stream-slot
    // limit.
    if budget.allow_one_oversize
        && eligible_work_drained
        && !consumed.oversize_slot_spent
        && consumed.attempted_streams < budget.max_streams
        && let Some(stream_id) = oldest_oversize_pending_refold(conn, budget)?
    {
        let outcome = settle_one_pending_refold(conn, stream_id, budget, consumed, now_ms);
        let (_, _, demote) = record_settle_outcome(&mut report, &mut consumed, stream_id, outcome);
        if demote {
            deferred_demotions.push(stream_id);
        }
    }
    // Apply every collected fairness demotion once, after paging, in its own committed txn — see
    // `deferred_demotions` above and [`demote_pending_refolds`]. Runs before the queue-empty probe
    // so the report reflects the final queue, though the O(1) EXISTS observation is position-blind
    // (a failed row still leaves `queue_empty == false` either way).
    demote_pending_refolds(conn, &deferred_demotions)?;
    report.queue_empty = !pending_refold_queue_nonempty(conn)?;
    Ok(report)
}

/// Settle ONE named stream's queued refold debt INSIDE the caller's already-open IMMEDIATE
/// transaction — the pending-fold barrier's drain (rag-rat-core `memory_write`).
///
/// The barrier owes THIS stream's acceptance before it may read completeness, so it settles the
/// stream as part of the write it is already performing rather than draining it beforehand on an
/// autocommit connection: a foreign `/3` candidate can target the LOCAL owner stream (C2 targeting
/// is unconstrained), so a remote peer can re-enqueue debt on the most expensive stream in the
/// store at will. Folding inside the caller's transaction bounds that to ONE fold per local write —
/// work the write's own authoring would do anyway — instead of an unbudgeted refold of the entire
/// local memory history run before the transaction opens (#798 adversarial finding 2). It also
/// means a barrier trip inside an open transaction can self-heal instead of hard-erroring, which
/// previously turned a remote enqueue landing mid-write into an unactionable failure for the user
/// (finding 5). A fold failure propagates and rolls the caller's write back — fail-closed.
pub fn settle_pending_content_refold_for_stream_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    now_ms: i64,
) -> anyhow::Result<()> {
    if !content_stream_has_pending_refold(tx, stream_id)? {
        return Ok(());
    }
    refold_and_project_stream_in_tx(tx, stream_id, now_ms)
}

/// Narrow the branch-selected winners to a contiguous accepted prefix per coordinate.
/// `select_accepted_branch` already returns one hash-linked chain per coordinate, but freshness
/// (evaluated after selection) can park a mid-chain winner, and the accepted projection must stay
/// prefix-closed — so keep only the run from seq 0 whose own finished verdict is `Accepted`.
fn prefix_closed_accepted(
    resolved: &[ResolvedEntry],
    selection: &BranchSelection,
    raw: &HashMap<AccountEntryHash, ContentAcceptance>,
) -> HashSet<AccountEntryHash> {
    let mut chains: HashMap<ChainCoordinate, Vec<(u64, AccountEntryHash)>> = HashMap::new();
    for r in resolved {
        if selection.accepted.contains(&r.entry_hash) {
            let coordinate = ChainCoordinate {
                stream_id: r.header.stream_id,
                author_account_id: r.header.author_account_id,
                device_fingerprint: r.header.device_fingerprint,
            };
            chains.entry(coordinate).or_default().push((r.header.seq, r.entry_hash));
        }
    }
    let mut accepted = HashSet::new();
    for mut winners in chains.into_values() {
        winners.sort_by_key(|(seq, _)| *seq);
        for (_, hash) in winners {
            if raw.get(&hash) == Some(&ContentAcceptance::Accepted) {
                accepted.insert(hash);
            } else {
                break; // prefix broken: every later winner on this chain is not accepted
            }
        }
    }
    accepted
}

/// The lamport discipline at the acceptance seam: demote any would-be-accepted entry whose
/// lamport jumps more than [`MAX_LAMPORT_ADVANCE`] past the highest lamport the accepted set
/// below it establishes, or whose chain's lamport fails to strictly increase. The header lamport
/// is attacker-controlled and decides projection LWW `(lamport, device)`, so without the bound
/// one granted writer's entry near the ceiling wins every register permanently AND bricks
/// authoring (the next mint overflows the ceiling). Enforcing at the fold rather than at ingest
/// inherits the predecessor gate for free — parked entries never fold, so an honest partitioned
/// writer's chain folds in order and each step stays within the bound — and the verdict is
/// re-derived every refold, so a demotion is never durable.
///
/// Two passes, O(n log n) total — the work is bounded even against an adversarial chain shape,
/// because this runs inside the stream's IMMEDIATE writer transaction:
///
/// 1. **Per-chain monotonicity.** Honest authoring mints a strictly increasing lamport along a
///    chain (`max accepted + 1` per entry), so a chain that ticks backwards is forged; it truncates
///    at the first non-increase. Sound here because the accepted set holds at most one entry per
///    `(chain, seq)` (branch selection already resolved forks), and it is what makes the single
///    walk below exact: within a monotone chain, ascending-lamport order IS seq order, so an
///    entry's chain cut is always discovered before the entries it demotes — no fixed-point restart
///    for an attacker to inflate.
/// 2. **Bounded advance** — [`bounded_advance_demotions`].
fn lamport_advance_clamped(
    resolved: &[ResolvedEntry],
    mut accepted: HashSet<AccountEntryHash>,
    condemned: &[(AccountEntryHash, &ContentEntryHeader)],
) -> (HashSet<AccountEntryHash>, HashSet<AccountEntryHash>, u64) {
    let entries: Vec<(AccountEntryHash, &ContentEntryHeader)> = resolved
        .iter()
        .filter(|r| accepted.contains(&r.entry_hash))
        .map(|r| (r.entry_hash, &r.header))
        .collect();
    let (cut, clock) = bounded_advance_walk(&entries, condemned, monotonicity_cuts(&entries));
    let parked = chain_cut_demotions(&entries, &cut);
    accepted.retain(|hash| !parked.contains(hash));
    (accepted, parked, clock)
}

/// Pass 1 of the `/3` lamport clamp: each chain's cut at its first non-increasing lamport step.
/// The input must hold at most ONE entry per `(chain, seq)` — the accepted population, where
/// branch selection (or the `content_accepted_slot` index, for stored legacy rows) has already
/// resolved forks — or a same-seq fork sibling would misread as a backwards tick.
fn monotonicity_cuts(
    entries: &[(AccountEntryHash, &ContentEntryHeader)],
) -> HashMap<ChainCoordinate, u64> {
    let mut chains: HashMap<ChainCoordinate, Vec<&ContentEntryHeader>> = HashMap::new();
    for (_, header) in entries {
        chains.entry(ChainCoordinate::of(header)).or_default().push(header);
    }
    let mut cut = HashMap::new();
    for (coordinate, mut members) in chains {
        members.sort_by_key(|header| header.seq);
        for pair in members.windows(2) {
            if pair[1].lamport <= pair[0].lamport {
                cut.insert(coordinate, pair[1].seq);
                break;
            }
        }
    }
    cut
}

/// The bounded-advance walk of the `/3` lamport clamp: one ascending `(lamport, entry_hash)` pass
/// over `entries` with a running max. A demoted entry — jumping past the bound, or sitting
/// at/above its chain's cut — never advances the running max (a poison entry must not legitimize
/// the next one) and cuts its chain at its seq, so the surviving set stays a dense prefix per
/// chain. `cut` primes chain truncations the caller already knows (the fold's monotonicity cuts).
///
/// `floor` rows are the CONDEMNED clock basis: they prop the running max when themselves within
/// bound, but are never cut and never demoted. This is what keeps revocation from wedging a
/// dependent chain — an honest writer that minted `basis + 1` while the basis was accepted must
/// not park when that basis is later condemned (condemnation already evicts the basis's LWW
/// damage; its lamport magnitude is not damage). A condemned entry whose own lamport jumps the
/// bound props nothing, so a straight poison cannot inflate the floor, and an in-bound condemned
/// ladder inflates it only rung by rung — bounded by the candidate caps, with the ceiling far
/// out of reach.
///
/// Shared by the fold ([`lamport_advance_clamped`]) and the V113 upgrade purge
/// ([`purge_legacy_lamport_violators`]). Returns the chain cuts (apply with
/// [`chain_cut_demotions`]) and the final running max — the stream's clock floor, which the fold
/// persists for the O(1) ingest-gate and authoring-mint reads.
fn bounded_advance_walk(
    entries: &[(AccountEntryHash, &ContentEntryHeader)],
    floor: &[(AccountEntryHash, &ContentEntryHeader)],
    mut cut: HashMap<ChainCoordinate, u64>,
) -> (HashMap<ChainCoordinate, u64>, u64) {
    let mut rows: Vec<(u64, AccountEntryHash, &ContentEntryHeader, bool)> = entries
        .iter()
        .map(|(hash, header)| (header.lamport, *hash, *header, true))
        .chain(floor.iter().map(|(hash, header)| (header.lamport, *hash, *header, false)))
        .collect();
    rows.sort_by_key(|(lamport, hash, _, _)| (*lamport, *hash));
    let mut running_max = 0u64;
    for (lamport, _, header, demotable) in rows {
        if !demotable {
            if lamport <= running_max.saturating_add(crate::entry::MAX_LAMPORT_ADVANCE) {
                running_max = running_max.max(lamport);
            }
            continue;
        }
        let coordinate = ChainCoordinate::of(header);
        if cut.get(&coordinate).is_some_and(|&seq| header.seq >= seq) {
            continue; // demoted below a cut this walk already discovered
        }
        if lamport > running_max.saturating_add(crate::entry::MAX_LAMPORT_ADVANCE) {
            let seq = cut.entry(coordinate).or_insert(header.seq);
            *seq = (*seq).min(header.seq);
        } else {
            running_max = running_max.max(lamport);
        }
    }
    (cut, running_max)
}

/// The accepted chain tails a SOFT revoke cuts at: for each of `author`'s devices holding
/// entries this store has ACCEPTED on `stream`, the highest such `(seq, entry_hash)`. The owner's
/// own store is the witness — prior work stays valid exactly as far as the owner has accepted it,
/// which the revoked device cannot rewrite — and a device this store never accepted work from
/// gets no cut (nothing to vouch for; the fold quarantines it). Sorted by fingerprint, the wire's
/// canonical cut order.
pub(in crate::account) fn accepted_chain_tails(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    author_account_id: AccountId,
) -> anyhow::Result<Vec<ops::DeviceCut>> {
    let mut stmt = tx.prepare(
        "SELECT device_fingerprint, seq, entry_hash FROM content_entries
         WHERE stream_id = ?1 AND author_account_id = ?2 AND accepted = 1",
    )?;
    let rows = stmt.query_map(
        params![stream_id.to_bytes().as_slice(), author_account_id.to_bytes().as_slice()],
        |row| {
            Ok((row.get::<_, [u8; 32]>(0)?, row.get::<_, [u8; 8]>(1)?, row.get::<_, [u8; 32]>(2)?))
        },
    )?;
    let mut tails: HashMap<[u8; 32], (u64, [u8; 32])> = HashMap::new();
    for row in rows {
        let (fingerprint, seq, entry_hash) = row?;
        let seq = u64::from_be_bytes(seq);
        let tail = tails.entry(fingerprint).or_insert((seq, entry_hash));
        if seq >= tail.0 {
            *tail = (seq, entry_hash);
        }
    }
    let mut cuts: Vec<ops::DeviceCut> = tails
        .into_iter()
        .map(|(fingerprint, (seq, hash))| ops::DeviceCut {
            device_fingerprint: DeviceFingerprint::from_bytes(fingerprint),
            seq,
            hash: AccountEntryHash::from_bytes(hash),
        })
        .collect();
    cuts.sort_by_key(|cut| cut.device_fingerprint.to_bytes());
    Ok(cuts)
}

/// The entry hash this store holds ACCEPTED at exactly `(stream, author, device, seq)`, or `None`
/// — the `--keep-until` witness check: a hard revoke may vouch for a prefix only as far as the
/// owner's own store has accepted it.
pub(in crate::account) fn accepted_entry_at(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    author_account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
    seq: u64,
) -> anyhow::Result<Option<AccountEntryHash>> {
    Ok(tx
        .query_row(
            "SELECT entry_hash FROM content_entries
             WHERE stream_id = ?1 AND author_account_id = ?2 AND device_fingerprint = ?3
               AND seq = ?4 AND accepted = 1",
            params![
                stream_id.to_bytes().as_slice(),
                author_account_id.to_bytes().as_slice(),
                device_fingerprint.to_bytes().as_slice(),
                seq.to_be_bytes().as_slice(),
            ],
            |row| row.get::<_, [u8; 32]>(0),
        )
        .optional()?
        .map(AccountEntryHash::from_bytes))
}

/// Persist (or clear) one stream's lamport clock floor, as derived by the refold's
/// [`bounded_advance_walk`]. Only positive floors are stored — a zero floor (an empty or
/// entirely-unaccepted stream) deletes the row, so the clock readers' `None` keeps meaning "no
/// clock yet" and a fresh mint still starts at lamport 0. The row is refold-owned state: written
/// only here, read by the ingest gate and the authoring mint.
fn store_stream_clock(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    clock: u64,
) -> rusqlite::Result<()> {
    if clock == 0 {
        tx.execute("DELETE FROM content_stream_clocks WHERE stream_id = ?1", [stream_id
            .to_bytes()
            .as_slice()])?;
    } else {
        tx.execute(
            "INSERT INTO content_stream_clocks(stream_id, clock) VALUES(?1, ?2)
             ON CONFLICT(stream_id) DO UPDATE SET clock = excluded.clock",
            params![stream_id.to_bytes().as_slice(), stored_lamport(clock)],
        )?;
    }
    Ok(())
}

/// Every entry of `entries` sitting at or above its chain's cut.
fn chain_cut_demotions(
    entries: &[(AccountEntryHash, &ContentEntryHeader)],
    cut: &HashMap<ChainCoordinate, u64>,
) -> HashSet<AccountEntryHash> {
    let mut demoted = HashSet::new();
    for (hash, header) in entries {
        if cut.get(&ChainCoordinate::of(header)).is_some_and(|&seq| header.seq >= seq) {
            demoted.insert(*hash);
        }
    }
    demoted
}

/// One-time upgrade repair, run as the V113 migration hook: DELETE every stored `/3` candidate
/// the bounded-advance walk demotes — so a pre-clamp poison AND every same-chain dependent above
/// it (an honest tail minted at `poison + 1` while the poison was still accepted) retire
/// together, re-rooting the author's chain at the surviving prefix. Parking alone cannot repair
/// this: a parked candidate stays the chain tail, every continuation mints a lower lamport from
/// the (now sane) accepted clock, and the monotonicity rule parks each one — the stream would be
/// permanently unauthorable. Deleting also stops the store re-advertising envelopes upgraded
/// peers refuse before storage (an over-ceiling lamport), which would otherwise retransmit on
/// every sync forever — over-ceiling rows are cut REGARDLESS of acceptance state, since a
/// rejected/forked/parked one is just as protocol-invalid and just as advertised. Over-ceiling
/// `content_pre_verify` rows are dropped for the same reason — ingest and promotion refuse them
/// now, but a legacy row would sit there unpromotable indefinitely.
///
/// The judgment mirrors the fold exactly, on the ACCEPTED rows only — monotonicity cuts (safe
/// there: the `content_accepted_slot` index guarantees one accepted row per `(chain, seq)`, so no
/// fork sibling can misread as a backwards tick, and without this pass a poisoned ancestor whose
/// backwards descendant sorts first in the walk would shield itself), then the bounded-advance
/// walk. Judging the clock over the full candidate set instead would let junk the fold never
/// accepted (an ungranted author's high-lamport candidate) advance the running max and shield a
/// genuinely accepted poison from deletion; the queued refold would then park the poison but
/// leave it stored as a wedging chain tail.
///
/// Deletion then follows HASH branches, never seq ranges: the demoted accepted rows plus every
/// over-ceiling row (any acceptance state), closed over stored `prev_hash` descendants. A
/// seq-keyed sweep would also delete a VALID sibling that shares a violating fork loser's
/// `(chain, seq)` — irreversible loss of accepted content — while the hash closure retires
/// exactly the poisoned branch and keeps each surviving branch dense.
///
/// Runtime folds keep PARKING rather than deleting. Deletion at this seam stays stable because
/// the ingest-time bounded-advance gate refuses the purged envelopes if a not-yet-upgraded peer
/// re-offers them — without that gate, one resend would re-park the tail and re-wedge the chain.
/// Undecodable envelopes are left alone (the refold declassifies them).
pub fn purge_legacy_lamport_violators(conn: &Connection) -> rusqlite::Result<()> {
    let tables_exist = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'content_entries'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !tables_exist {
        return Ok(());
    }
    struct LegacyRow {
        entry_hash: AccountEntryHash,
        header: ContentEntryHeader,
        accepted: bool,
        condemned: bool,
    }
    let mut streams: HashMap<[u8; 32], Vec<LegacyRow>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT e.entry_hash, e.signed_bytes, e.accepted, COALESCE(s.status, '')
             FROM content_entries e
             LEFT JOIN content_entry_status s ON s.entry_hash = e.entry_hash",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, [u8; 32]>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (entry_hash, signed_bytes, accepted, status) = row?;
            if let Ok(signed) = envelope::decode_content_signed(&signed_bytes) {
                streams.entry(signed.header.stream_id.to_bytes()).or_default().push(LegacyRow {
                    entry_hash: AccountEntryHash::from_bytes(entry_hash),
                    header: signed.header,
                    accepted,
                    condemned: status.starts_with("condemned"),
                });
            }
        }
    }
    for members in streams.into_values() {
        let accepted: Vec<(AccountEntryHash, &ContentEntryHeader)> = members
            .iter()
            .filter(|row| row.accepted)
            .map(|row| (row.entry_hash, &row.header))
            .collect();
        // Condemned rows prop the clock exactly as they do at the fold (see
        // [`bounded_advance_walk`]): without them, a store whose poison basis was already
        // condemned would purge the honest dependents that minted against it.
        let condemned: Vec<(AccountEntryHash, &ContentEntryHeader)> = members
            .iter()
            .filter(|row| row.condemned)
            .map(|row| (row.entry_hash, &row.header))
            .collect();
        // The fold-mirroring judgment over the accepted rows: what would park under the clamp is
        // what deletes here.
        let (cut, _) = bounded_advance_walk(&accepted, &condemned, monotonicity_cuts(&accepted));
        let mut doomed = chain_cut_demotions(&accepted, &cut);
        // Over-ceiling rows are protocol-invalid regardless of acceptance state — upgraded peers
        // refuse the envelope before storage, so a rejected/forked/parked one left behind would
        // still be advertised and resent on every reconciliation forever.
        for row in &members {
            if row.header.lamport >= crate::entry::MAX_ENTRY_LAMPORT {
                doomed.insert(row.entry_hash);
            }
        }
        // Close over stored hash descendants: a row chained onto a doomed row can never regain a
        // stored predecessor, so it retires too — but ONLY the doomed branch; a valid sibling at
        // the same (chain, seq) is untouched.
        let mut children: HashMap<AccountEntryHash, Vec<AccountEntryHash>> = HashMap::new();
        for row in &members {
            if let Some(previous) = row.header.prev_hash {
                children.entry(previous).or_default().push(row.entry_hash);
            }
        }
        let mut frontier: Vec<AccountEntryHash> = doomed.iter().copied().collect();
        while let Some(parent) = frontier.pop() {
            for child in children.get(&parent).into_iter().flatten() {
                if doomed.insert(*child) {
                    frontier.push(*child);
                }
            }
        }
        for hash in doomed {
            conn.execute("DELETE FROM content_entries WHERE entry_hash = ?1", [hash.as_slice()])?;
            conn.execute("DELETE FROM content_entry_status WHERE entry_hash = ?1", [
                hash.as_slice()
            ])?;
        }
    }
    let legacy_ceiling_rows: Vec<Vec<u8>> = {
        let mut stmt = conn.prepare("SELECT signed_hash, raw_bytes FROM content_pre_verify")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)))?;
        let mut over_ceiling = Vec::new();
        for row in rows {
            let (signed_hash, raw) = row?;
            if envelope::decode_content_signed(&raw)
                .is_ok_and(|signed| signed.header.lamport >= crate::entry::MAX_ENTRY_LAMPORT)
            {
                over_ceiling.push(signed_hash);
            }
        }
        over_ceiling
    };
    for signed_hash in legacy_ceiling_rows {
        conn.execute("DELETE FROM content_pre_verify WHERE signed_hash = ?1", [
            signed_hash.as_slice()
        ])?;
    }
    Ok(())
}

/// The distinct streams `account` holds ACCEPTED self-authored `/3` entries on, excluding the
/// ones it owns — durable evidence of past contribution, which outlives the mutable
/// configuration that produced it. The servability probe (`account_is_public_kb`) reads this per
/// inbound connection BEFORE authentication, so it rides the V117 author-leading partial index;
/// the ownership exclusion is an indexed subquery.
pub fn authored_foreign_streams(
    conn: &Connection,
    account: AccountId,
) -> anyhow::Result<Vec<StreamId>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT stream_id FROM content_entries
         WHERE author_account_id = ?1 AND accepted = 1
           AND stream_id NOT IN (
               SELECT stream_id FROM account_stream_ownership WHERE account_id = ?1
           )
         ORDER BY stream_id",
    )?;
    let rows = stmt
        .query_map([account.to_bytes().as_slice()], |row| row.get::<_, [u8; 32]>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().map(StreamId::from_bytes).collect())
}

/// The V114 backfill hook: fill the denormalized `content_entries.lamport` column from each
/// stored signed envelope, once. Insert sites write the column from then on; an undecodable blob
/// keeps NULL, which `MAX` ignores — the same treatment the decoding scan gave it. Idempotent
/// (`WHERE lamport IS NULL`), so a ladder replay re-decodes nothing already filled.
/// Paged by entry-hash keyset, never buffered whole: envelopes run up to 256 KiB and locally
/// authored rows sit outside the remote candidate-byte ceilings, so collecting every
/// `signed_bytes` first would scale peak heap with the entire content log during a required
/// migration. The keyset (not a bare `LIMIT`) is what makes an undecodable row — which stays
/// NULL — unable to pin the loop in place.
pub fn backfill_content_lamport(conn: &Connection) -> rusqlite::Result<()> {
    const PAGE: i64 = 256;
    let mut cursor: Vec<u8> = Vec::new();
    loop {
        let rows: Vec<(Vec<u8>, Vec<u8>)> = {
            let mut stmt = conn.prepare(
                "SELECT entry_hash, signed_bytes FROM content_entries
                 WHERE lamport IS NULL AND entry_hash > ?1
                 ORDER BY entry_hash LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![cursor.as_slice(), PAGE], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let Some((last, _)) = rows.last() else {
            return Ok(());
        };
        cursor = last.clone();
        for (entry_hash, signed_bytes) in &rows {
            if let Ok(signed) = envelope::decode_content_signed(signed_bytes) {
                conn.execute(
                    "UPDATE content_entries SET lamport = ?1 WHERE entry_hash = ?2",
                    params![stored_lamport(signed.header.lamport), entry_hash.as_slice()],
                )?;
            }
        }
    }
}

/// Reset the status of every stream row NOT in `handled` to the unclassified baseline. Those rows
/// could not be decoded — they are absent from the refold's per-entry writes, so without this a
/// corrupt blob (or a row written outside `content_ingest`) would keep whatever verdict a prior
/// fold derived. `accepted` is cleared separately by the caller's blanket update.
fn declassify_rows_absent_from(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    handled: &HashSet<AccountEntryHash>,
) -> anyhow::Result<()> {
    let hashes: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare("SELECT entry_hash FROM content_entries WHERE stream_id = ?1")?;
        stmt.query_map([stream_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for bytes in hashes {
        let hash = fixed::<32>(&bytes)?;
        if !handled.contains(&hash.into()) {
            set_status(tx, &hash.into(), ContentStatus::RetainedUnfolded)?;
        }
    }
    Ok(())
}

enum EvaluatorPhase {
    /// Only the authority pass: `Some` = decided pre-DAG (rejected/parked/condemned), `None` =
    /// eligible to contest a slot.
    Eligibility,
    /// The whole predicate, including branch selection and freshness — always decides.
    Finished,
}

/// Build the per-entry evaluator input from resolved facts and run the requested phase. The
/// ancestry closure walks `view`, so the input never outlives it; both callers evaluate inline.
fn verdict_for(
    r: &ResolvedEntry,
    view: &HashMap<AccountEntryHash, ContentEntryHeader>,
    branch_selected: bool,
    phase: EvaluatorPhase,
) -> anyhow::Result<Option<ContentAcceptance>> {
    let input = acceptance::ContentAcceptanceInput {
        header: &r.header,
        entry_hash: r.entry_hash,
        owner_account_id: r.owner_account_id,
        dense_predecessor_reachable: r.dense_predecessor_reachable,
        branch_selected,
        ownership: r.ownership,
        roster: r.roster,
        grant: r.grant.clone(),
        owner_freshness: r.owner_freshness,
        author_freshness: r.author_freshness,
        subject_hold: r.subject_hold,
        ancestry: |target, watermark| {
            candidate::ancestry(&target, &watermark, view as &dyn HeaderView)
        },
    };
    let verdict = match phase {
        EvaluatorPhase::Eligibility => acceptance::authority_verdict(&input),
        EvaluatorPhase::Finished => acceptance::evaluate_content_acceptance(&input).map(Some),
    };
    verdict.map_err(|error| {
        anyhow::anyhow!("content refold built inconsistent freshness provenance: {error:?}")
    })
}

/// Read every candidate on the stream and resolve its authority facts against the current fold.
fn resolve_stream_authority(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    owner_account_id: AccountId,
) -> anyhow::Result<Vec<ResolvedEntry>> {
    let headers = load_stream_headers(tx, stream_id)?;
    let view: HashMap<AccountEntryHash, ContentEntryHeader> =
        headers.iter().map(|(hash, header)| (*hash, header.clone())).collect();
    let reachable = reachable_entries(&view);

    // Every entry on one author's chain shares its author/roster/device/grant and (usually) its
    // cited `auth_len`s, so resolving the authority facts once per DISTINCT key instead of once per
    // entry collapses O(n) authority queries to O(distinct keys) — near O(1) for a linear chain,
    // the difference between a bounded and a quadratic refold cost. All caches are scoped to
    // this one snapshot; nothing persists across refolds.
    let mut caches = AuthorityCaches::default();
    // The owner is the same for every entry on the stream, so resolve its contested state once. A
    // contested OWNER fails content closed just as a contested author does (§12): a contributor's
    // writer grant is minted in the owner's log, so if that account is compromised the grant can no
    // longer be trusted — the content parks until the owner recovers.
    let owner_contested = account_storage::account_is_contested(tx, owner_account_id)?;
    let mut resolved = Vec::with_capacity(headers.len());
    for (entry_hash, header) in headers {
        let ownership = AuthorityQuery::Effective(CitedOwnership {
            owner_account_id,
            stream_id: header.stream_id,
        });
        let roster_key = (header.author_account_id, header.roster_ref, header.device_fingerprint);
        let roster = match caches.roster.get(&roster_key) {
            Some(cached) => *cached,
            None => {
                let resolved = map_roster(
                    account_storage::roster_content_authority_in_snapshot(
                        tx,
                        header.author_account_id,
                        header.roster_ref,
                        header.device_fingerprint,
                        header.stream_id,
                    )?,
                    &header,
                );
                caches.roster.insert(roster_key, resolved);
                resolved
            },
        };
        let grant = match header.grant_id {
            None => None,
            Some(grant_id) => {
                let grant_key = (grant_id, header.author_account_id, header.device_fingerprint);
                let resolved = match caches.grant.get(&grant_key) {
                    Some(cached) => cached.clone(),
                    None => {
                        let resolved = map_grant(
                            account_storage::grant_effective_for_device_in_snapshot(
                                tx,
                                owner_account_id,
                                grant_id,
                                header.stream_id,
                                header.author_account_id,
                                header.device_fingerprint,
                            )?,
                            owner_account_id,
                            grant_id,
                        );
                        caches.grant.insert(grant_key, resolved.clone());
                        resolved
                    },
                };
                Some(resolved)
            },
        };
        // A content cut's watermark is a CONTENT candidate the account fold could not validate (it
        // only holds the account log), so bind it against the content DAG HERE, exactly as
        // `pinned_branch` does. A cut naming a DIFFERENT coordinate/seq is malformed → ignored
        // (Open, the §11.3 laundering guard); a held-and-correct OR not-yet-held watermark keeps
        // the cut intact so `combine_boundaries` still condemns `beyond_cut` from seq alone (I11)
        // and `candidate::ancestry` parks only the genuinely under-cut prefix (a withheld watermark
        // never flips a verdict).
        let coordinate = ChainCoordinate {
            stream_id: header.stream_id,
            author_account_id: header.author_account_id,
            device_fingerprint: header.device_fingerprint,
        };
        let roster = bind_roster_cut(roster, &coordinate, &view);
        let grant = bind_grant_cut(grant, &coordinate, &view);
        let owner_freshness = CitedFreshness {
            account_id: owner_account_id,
            asserted_auth_len: header.owner_auth_len,
            state: caches.freshness(tx, owner_account_id, header.owner_auth_len)?,
        };
        let author_freshness = CitedFreshness {
            account_id: header.author_account_id,
            asserted_auth_len: header.author_auth_len,
            state: caches.freshness(tx, header.author_account_id, header.author_auth_len)?,
        };
        // §12: a contested account halts authority mutation, so content that depends on it fails
        // closed as `contested_subject` (quota-bounded, reclassified on recovery). Either the
        // author (its roster enrollment) or the owner (its grant) being contested poisons
        // the citation. This over-approximates the spec's "device is a subject of a residue
        // cut" to the whole account — safe (fail-closed); the residue-subject precision is
        // a tracked follow-up.
        let subject_hold = if owner_contested || caches.contested(tx, header.author_account_id)? {
            SubjectAuthorityHold::Contested
        } else {
            SubjectAuthorityHold::Clear
        };
        let dense_predecessor_reachable = reachable.contains(&entry_hash);
        resolved.push(ResolvedEntry {
            entry_hash,
            header,
            owner_account_id,
            dense_predecessor_reachable,
            ownership,
            roster,
            grant,
            owner_freshness,
            author_freshness,
            subject_hold,
        });
    }
    Ok(resolved)
}

/// Per-refold memoization of the authority facts, keyed by exactly what each query depends on, so a
/// stream of many entries sharing one author/roster/grant resolves each fact once. Snapshot-scoped:
/// a fresh instance per `resolve_stream_authority`, never shared across refolds.
#[derive(Default)]
struct AuthorityCaches {
    roster:
        HashMap<(AccountId, RosterRef, DeviceFingerprint), AuthorityQuery<CitedRosterAuthority>>,
    grant: HashMap<(GrantId, AccountId, DeviceFingerprint), AuthorityQuery<CitedGrantAuthority>>,
    held_control_log: HashMap<AccountId, u64>,
    contested: HashMap<AccountId, bool>,
}

impl AuthorityCaches {
    fn freshness(
        &mut self,
        tx: &Transaction<'_>,
        account_id: AccountId,
        asserted_auth_len: u64,
    ) -> anyhow::Result<AuthorityFreshness> {
        let held = match self.held_control_log.get(&account_id) {
            Some(held) => *held,
            None => {
                let held = account_storage::held_control_log_len(tx, account_id)?;
                self.held_control_log.insert(account_id, held);
                held
            },
        };
        Ok(AuthorityFreshness::of(asserted_auth_len, held))
    }

    fn contested(&mut self, tx: &Transaction<'_>, account_id: AccountId) -> anyhow::Result<bool> {
        if let Some(cached) = self.contested.get(&account_id) {
            return Ok(*cached);
        }
        let contested = account_storage::account_is_contested(tx, account_id)?;
        self.contested.insert(account_id, contested);
        Ok(contested)
    }
}

/// Load `(entry_hash, header)` for every candidate on the stream, decoding the header from the
/// stored — already signature-verified — bytes. The refold re-derives authority, never re-verifies.
fn load_stream_headers(
    tx: &Transaction<'_>,
    stream_id: StreamId,
) -> anyhow::Result<Vec<(AccountEntryHash, ContentEntryHeader)>> {
    let mut stmt = tx.prepare(
        "SELECT entry_hash, signed_bytes FROM content_entries WHERE stream_id = ?1
         ORDER BY entry_hash", // deterministic load order (selection is order-free regardless)
    )?;
    let rows = stmt
        .query_map([stream_id.to_bytes().as_slice()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (entry_hash, signed_bytes) in rows {
        let entry_hash = AccountEntryHash::from_bytes(fixed::<32>(&entry_hash)?);
        // `content_ingest` only ever stores verified, decodable envelopes, so a decode failure here
        // means the row was written by some other path (or the blob is corrupt). Skip it rather
        // than abort the whole account fold on one bad row — an undecodable candidate cannot belong
        // to any valid chain, so treating it as absent is the sanest handling.
        let Ok(signed) = envelope::decode_content_signed(&signed_bytes) else {
            continue;
        };
        // The decode recomputes `entry_hash = sha256(header body)`, so if it does not match the
        // row's key the blob is not this row's entry — a
        // swapped/corrupted-into-a-different-valid-envelope blob. The hash covers the whole
        // coordinate (stream, author, device, seq), so this one check also pins the stream.
        // Skip it too: the refold must never classify a row under a header that is not its
        // own.
        if signed.entry_hash != entry_hash {
            continue;
        }
        out.push((entry_hash, signed.header));
    }
    Ok(out)
}

/// Revert every entry on a stream to its structural classification (`retained_unfolded` if its
/// dense chain is held, else `parked{missing_predecessor}`) and clear `accepted`. Used when the
/// stream has no resolvable owner: whatever a prior fold decided has lost its authority basis, so
/// the entries return to "held, not yet folded" — the same status a fresh candidate carries.
fn declassify_stream_to_structural(
    tx: &Transaction<'_>,
    stream_id: StreamId,
) -> anyhow::Result<()> {
    let headers = load_stream_headers(tx, stream_id)?;
    let view: HashMap<AccountEntryHash, ContentEntryHeader> =
        headers.iter().map(|(hash, header)| (*hash, header.clone())).collect();
    let reachable = reachable_entries(&view);
    // Clear `accepted` for the WHOLE stream first — including any undecodable row absent from
    // `headers`, which must lose acceptance too (an orphaned + corrupt stream is exactly the case a
    // bare `headers.is_empty()` early return would leave stale).
    tx.execute("UPDATE content_entries SET accepted = 0 WHERE stream_id = ?1", [stream_id
        .to_bytes()
        .as_slice()])?;
    let mut handled = HashSet::with_capacity(headers.len());
    for (entry_hash, _) in &headers {
        let status = if reachable.contains(entry_hash) {
            ContentStatus::RetainedUnfolded
        } else {
            ContentStatus::MissingPredecessor
        };
        set_status(tx, entry_hash, status)?;
        handled.insert(*entry_hash);
    }
    declassify_rows_absent_from(tx, stream_id, &handled)?;
    Ok(())
}

/// The entries whose dense chain is fully held back to seq 0, computed in ONE O(n) forward pass
/// from the chain roots. A per-entry backward walk to seq 0 would make the whole refold O(n²), and
/// at the per-author candidate cap (thousands) a long peer-supplied chain could burn tens of
/// millions of lookups on a single ingest — an availability footgun. Here every entry is enqueued
/// once and every `prev_hash` edge is followed once.
fn reachable_entries(
    view: &HashMap<AccountEntryHash, ContentEntryHeader>,
) -> HashSet<AccountEntryHash> {
    let mut by_prev: HashMap<AccountEntryHash, Vec<&AccountEntryHash>> = HashMap::new();
    let mut queue: VecDeque<&AccountEntryHash> = VecDeque::new();
    for (hash, header) in view {
        match header.prev_hash {
            // A chain root: seq 0 with no predecessor. A `prev_hash`-less entry at seq > 0 is
            // structurally impossible to reach and simply never gets enqueued.
            None if header.seq == 0 => queue.push_back(hash),
            None => {},
            Some(prev) => by_prev.entry(prev).or_default().push(hash),
        }
    }
    let mut reachable = HashSet::new();
    while let Some(hash) = queue.pop_front() {
        if !reachable.insert(*hash) {
            continue;
        }
        let parent = &view[hash];
        let Some(children) = by_prev.get(hash) else {
            continue;
        };
        for child_hash in children {
            let child = &view[*child_hash];
            // Contiguous + same coordinate: a link that skips a slot or jumps chain is not a real
            // predecessor. `seq` is peer-supplied, so guard the `+ 1` against a `u64::MAX` row.
            if parent.seq.checked_add(1) == Some(child.seq)
                && child.stream_id == parent.stream_id
                && child.author_account_id == parent.author_account_id
                && child.device_fingerprint == parent.device_fingerprint
            {
                queue.push_back(child_hash);
            }
        }
    }
    reachable
}

/// The register watermarks pinning a branch: the cut boundaries the authority pass already
/// resolved.
fn branch_pins(resolved: &[ResolvedEntry]) -> Vec<BranchPin> {
    let mut pins = Vec::new();
    for r in resolved {
        let coordinate = ChainCoordinate {
            stream_id: r.header.stream_id,
            author_account_id: r.header.author_account_id,
            device_fingerprint: r.header.device_fingerprint,
        };
        if let AuthorityQuery::Effective(roster) = &r.roster
            && let AuthorityBoundary::Cut { seq, hash } = roster.authority.boundary
        {
            pins.push(BranchPin { coordinate, seq, watermark: hash });
        }
        // Only a WRITER grant's cut may pin content: a reader grant never authorizes a content
        // write, so its revoke watermark must not steer the accepted branch for writer-grant
        // content on the same coordinate (a peer could otherwise store a rejected
        // reader-grant entry purely to hijack selection).
        if let Some(AuthorityQuery::Effective(grant)) = &r.grant
            && grant.authority.grant.role == GrantRole::Writer
            && let GrantDeviceBoundary::Cut(cut) = &grant.authority.boundary
        {
            pins.push(BranchPin { coordinate, seq: cut.seq, watermark: cut.hash });
        }
    }
    pins
}

/// Bind a roster content cut's watermark to this coordinate against the content DAG (§11.3). A
/// held-and-correct OR a not-yet-held watermark keeps the cut INTACT: its `[seq]` condemns
/// beyond-cut entries from seq alone (I11) even before the watermark syncs, and the under-cut
/// prefix parks — via `candidate::ancestry` yielding `Unknown(UnknownCutTarget)` — until it does.
/// A withheld watermark must never flip a verdict nor launder a beyond-cut forgery into a park.
/// ONLY a watermark naming a DIFFERENT coordinate/seq is malformed and drops to `Open` (the §11.3
/// laundering guard: a misbound cut may neither condemn nor pin). This mirrors the account fold's
/// cut-target binding.
fn bind_roster_cut(
    roster: AuthorityQuery<CitedRosterAuthority>,
    coordinate: &ChainCoordinate,
    view: &dyn HeaderView,
) -> AuthorityQuery<CitedRosterAuthority> {
    let AuthorityQuery::Effective(mut fact) = roster else {
        return roster;
    };
    if let AuthorityBoundary::Cut { seq, hash } = fact.authority.boundary
        && candidate::validate_cut_target(seq, &hash, coordinate, view) == CutBinding::Mismatch
    {
        fact.authority.boundary = AuthorityBoundary::Open;
    }
    AuthorityQuery::Effective(fact)
}

/// Bind a grant device cut's watermark to this coordinate against the content DAG — the grant-side
/// mirror of [`bind_roster_cut`].
fn bind_grant_cut(
    grant: Option<AuthorityQuery<CitedGrantAuthority>>,
    coordinate: &ChainCoordinate,
    view: &dyn HeaderView,
) -> Option<AuthorityQuery<CitedGrantAuthority>> {
    let Some(AuthorityQuery::Effective(mut fact)) = grant else {
        return grant;
    };
    let cut = match &fact.authority.boundary {
        GrantDeviceBoundary::Cut(cut) => Some((cut.seq, cut.hash)),
        _ => None,
    };
    if let Some((seq, hash)) = cut
        && candidate::validate_cut_target(seq, &hash, coordinate, view) == CutBinding::Mismatch
    {
        fact.authority.boundary = GrantDeviceBoundary::Open;
    }
    Some(AuthorityQuery::Effective(fact))
}

fn map_roster(
    query: AuthorityQuery<crate::account::RosterContentAuthority>,
    header: &ContentEntryHeader,
) -> AuthorityQuery<CitedRosterAuthority> {
    match query {
        AuthorityQuery::Effective(authority) => AuthorityQuery::Effective(CitedRosterAuthority {
            account_id: header.author_account_id,
            roster_ref: header.roster_ref,
            stream_id: header.stream_id,
            authority,
        }),
        AuthorityQuery::Unknown => AuthorityQuery::Unknown,
        AuthorityQuery::Invalid(reason) => AuthorityQuery::Invalid(reason),
    }
}

fn map_grant(
    query: AuthorityQuery<crate::account::GrantDeviceAuthority>,
    owner_account_id: AccountId,
    grant_id: GrantId,
) -> AuthorityQuery<CitedGrantAuthority> {
    match query {
        AuthorityQuery::Effective(authority) =>
            AuthorityQuery::Effective(CitedGrantAuthority { owner_account_id, grant_id, authority }),
        AuthorityQuery::Unknown => AuthorityQuery::Unknown,
        AuthorityQuery::Invalid(reason) => AuthorityQuery::Invalid(reason),
    }
}

/// Write one entry's verdict: the taxonomy status string, and `accepted = 1` only for `Accepted`.
fn write_verdict(
    tx: &Transaction<'_>,
    entry_hash: &AccountEntryHash,
    verdict: ContentAcceptance,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO content_entry_status(entry_hash, status, detail) VALUES(?1, ?2, NULL)
         ON CONFLICT(entry_hash) DO UPDATE SET status = excluded.status, detail = NULL",
        params![entry_hash.as_slice(), verdict.as_db_str()],
    )?;
    if verdict == ContentAcceptance::Accepted {
        tx.execute("UPDATE content_entries SET accepted = 1 WHERE entry_hash = ?1", [
            entry_hash.as_slice()
        ])?;
    }
    Ok(())
}

fn set_status(
    tx: &Transaction<'_>,
    entry_hash: &AccountEntryHash,
    status: ContentStatus,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO content_entry_status(entry_hash, status, detail) VALUES(?1, ?2, NULL)
         ON CONFLICT(entry_hash) DO UPDATE SET status = excluded.status, detail = NULL",
        params![entry_hash.as_slice(), status.as_db_str()],
    )?;
    Ok(())
}

fn status_for(tx: &Transaction<'_>, hash: &AccountEntryHash) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
        [hash.as_slice()],
        |row| row.get(0),
    )
    .optional()
}

/// One `/3` content entry as it goes on the wire (phase D, #406): its chain coordinates plus the
/// exact stored `signed_bytes`. The bytes are opaque here — a peer re-runs [`content_ingest`] over
/// them, which re-resolves the roster key and re-verifies the signature from scratch, so the sender
/// is never trusted. Every HELD candidate is offered, accepted or not: acceptance is a per-refold
/// verdict (#652) each peer recomputes locally, so peers must see the same candidate set to reach
/// the same accepted set.
#[derive(Debug, Clone)]
pub struct SyncContentEntry {
    pub stream_id: StreamId,
    pub author_account_id: AccountId,
    pub seq: u64,
    pub entry_hash: AccountEntryHash,
    pub signed_bytes: Vec<u8>,
}

/// Peek the `(stream_id, author_account_id, entry_hash)` a signed content entry claims, WITHOUT
/// ingesting it. The sync layer uses this to refuse an entry whose claimed author is a DIFFERENT
/// account than the session is scoped to, before it reaches [`content_ingest`].
///
/// The claimed author is attacker-settable header material, so this is a session-scope PRE-FILTER
/// only, NEVER a trust boundary: [`content_ingest`] is the boundary — it re-resolves the roster key
/// for the claimed `(stream, author, roster_ref)` and rejects anything not signed by a device in
/// that roster, so a forged author claim cannot land a candidate. A decode failure means the bytes
/// are not a well-formed content entry — the session treats that as a peer to distrust.
pub fn content_entry_ref(
    signed_bytes: &[u8],
) -> anyhow::Result<(StreamId, AccountId, AccountEntryHash)> {
    let signed = envelope::decode_content_signed(signed_bytes)?;
    Ok((signed.header.stream_id, signed.header.author_account_id, signed.entry_hash))
}

/// The wire dedup key for a signed content entry: `sha256(signed_bytes)`, the SAME hash
/// `content_pre_verify` keys its rows by. Distinguishes competing SIGNATURES of one body — two
/// envelopes can share an `entry_hash` yet differ in signature — so the sync layer treats them as
/// distinct. Never diff content sync inventory by `entry_hash`.
pub fn content_signed_hash(signed_bytes: &[u8]) -> SignedHash {
    SignedHash::from_bytes(cbor::sha256(signed_bytes))
}

/// Whether `account_id` already holds this EXACT signed content envelope — as a stored candidate
/// (matched by its bytes) OR a durably parked pre-verify row (matched by `signed_hash`). Scoped to
/// the account's OWN authored content, the same scope [`content_entries_for_sync`] offers.
/// Signed-envelope precise, not `entry_hash` precise: a distinct signature that happens to share an
/// entry_hash is a different entry the peer may still need.
pub fn content_signed_entry_exists(
    conn: &Connection,
    account_id: AccountId,
    signed_bytes: &[u8],
) -> anyhow::Result<bool> {
    let signed_hash = cbor::sha256(signed_bytes);
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM content_entries
             WHERE author_account_id = ?1 AND signed_bytes = ?2
             UNION ALL
             SELECT 1 FROM content_pre_verify
             WHERE claimed_author_account_id = ?1 AND signed_hash = ?3
             LIMIT 1",
            params![account_id.to_bytes().as_slice(), signed_bytes, signed_hash.as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Every `/3` content entry authored under `account_id` that a peer may need — the held candidates
/// AND the ones durably PARKED in `content_pre_verify` awaiting their roster key — plus the
/// contributions this account relays ([`relayed_content_entries`], #1280).
///
/// The account's OWN content restores a device's own memories onto a fresh sibling. Content
/// authored by OTHER accounts is offered only on a stream this account owns and granted to that
/// author, so a peer that syncs only the owner receives its contributors' memories while a stranger
/// can never use the owner's session to reach a peer.
///
/// Held candidates come first, ordered `(stream_id, seq, entry_hash)` — a causal-leaning per-stream
/// order so a cooperative receiver folds predecessors before successors and avoids
/// park-then-promote churn. Relayed contributions follow, then parked rows, since they depend on
/// roster material in the held set. It is NOT a topological guarantee against an adversarial sender
/// (that is a reconciliation concern at the caller, #878); it makes the honest restore converge in
/// one session.
pub fn content_entries_for_sync(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<SyncContentEntry>> {
    let mut out = Vec::new();

    // `seq` is stored big-endian (`u64::to_be_bytes`), so ordering the BLOB column directly yields
    // numeric order.
    let mut held = conn.prepare(
        "SELECT entry_hash, stream_id, seq, signed_bytes
         FROM content_entries WHERE author_account_id = ?1
         ORDER BY stream_id, seq, entry_hash",
    )?;
    let held_rows = held
        .query_map(params![account_id.to_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (hash, stream, seq, signed_bytes) in held_rows {
        out.push(SyncContentEntry {
            stream_id: StreamId::from_bytes(fixed::<32>(&stream)?),
            author_account_id: account_id,
            seq: u64::from_be_bytes(fixed::<8>(&seq)?),
            entry_hash: AccountEntryHash::from_bytes(fixed::<32>(&hash)?),
            signed_bytes,
        });
    }
    out.extend(relayed_content_entries(conn, account_id)?);

    // Parked rows carry raw signed bytes; decode the header for the stream/seq the wire records
    // (informational — the session diffs on the signed hash). A parked row that no longer decodes
    // is skipped rather than failing the whole read.
    let mut parked = conn.prepare(
        "SELECT entry_hash, raw_bytes
         FROM content_pre_verify WHERE claimed_author_account_id = ?1
         ORDER BY entry_hash",
    )?;
    let parked_rows = parked
        .query_map(params![account_id.to_bytes().as_slice()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (hash, signed_bytes) in parked_rows {
        let (stream_id, seq) = match envelope::decode_content_signed(&signed_bytes) {
            Ok(signed) => (signed.header.stream_id, signed.header.seq),
            Err(_) => continue,
        };
        out.push(SyncContentEntry {
            stream_id,
            author_account_id: account_id,
            seq,
            entry_hash: AccountEntryHash::from_bytes(fixed::<32>(&hash)?),
            signed_bytes,
        });
    }

    Ok(out)
}

/// The public-serve variant of [`content_entries_for_sync`] (#407): AUTHENTICATED `content_entries`
/// rows only — the parked `content_pre_verify` candidates are EXCLUDED, because those are
/// unauthenticated bytes from arbitrary peers and a public server must not relay forged candidates
/// to anonymous readers. Relayed contributions (#1280) are served only from an author that is
/// itself fully public: the account session relays a grantee's log to anonymous readers under the
/// same rule, and a contribution whose author's log is withheld could never verify.
///
/// EVERY row is filtered by ITS OWN stream's access mode, not by a caller-level "this account is
/// public" gate. That gate is `account_is_fully_public`, which inspects only streams the account
/// OWNS — and a granted CONTRIBUTOR (#1164) owns none while authoring onto other accounts' streams,
/// possibly a public one AND a private one. A single public grant is enough to make such an account
/// servable, so an account-level gate would ship its private-stream contributions to anonymous
/// readers alongside the public ones. `stream_access_mode` fails closed to `Private`, so a stream
/// whose ownership fact is not folded here is withheld rather than leaked.
pub fn content_entries_for_public_sync(
    conn: &Connection,
    account_id: AccountId,
) -> anyhow::Result<Vec<SyncContentEntry>> {
    // Resolved once per distinct stream: an account's rows span very few streams, and each miss
    // costs an ownership lookup plus a StreamOwn decode.
    let mut public: std::collections::HashMap<StreamId, bool> = std::collections::HashMap::new();
    let mut stream_is_public = |conn: &Connection, stream: StreamId| -> anyhow::Result<bool> {
        if let Some(known) = public.get(&stream) {
            return Ok(*known);
        }
        let verdict = match crate::account::storage::stream_owner_account(conn, stream)? {
            Some(owner) =>
                crate::account::storage::stream_access_mode(conn, owner, stream)?
                    == crate::stream::AccessMode::PublicRead,
            // No owner fact folded here: fail closed rather than serve an unattributable stream.
            None => false,
        };
        public.insert(stream, verdict);
        Ok(verdict)
    };
    let mut held = conn.prepare(
        "SELECT entry_hash, stream_id, seq, signed_bytes
         FROM content_entries WHERE author_account_id = ?1
         ORDER BY stream_id, seq, entry_hash",
    )?;
    let rows = held
        .query_map(params![account_id.to_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (hash, stream, seq, signed_bytes) in rows {
        let stream_id = StreamId::from_bytes(fixed::<32>(&stream)?);
        if !stream_is_public(conn, stream_id)? {
            continue;
        }
        out.push(SyncContentEntry {
            stream_id,
            author_account_id: account_id,
            seq: u64::from_be_bytes(fixed::<8>(&seq)?),
            entry_hash: AccountEntryHash::from_bytes(fixed::<32>(&hash)?),
            signed_bytes,
        });
    }
    let mut public_authors: std::collections::HashMap<AccountId, bool> =
        std::collections::HashMap::new();
    for entry in relayed_content_entries(conn, account_id)? {
        let author = entry.author_account_id;
        let author_is_public = match public_authors.get(&author) {
            Some(known) => *known,
            None => {
                let verdict = crate::account::storage::account_is_fully_public(conn, author)?;
                public_authors.insert(author, verdict);
                verdict
            },
        };
        if author_is_public && stream_is_public(conn, entry.stream_id)? {
            out.push(entry);
        }
    }
    Ok(out)
}

/// The held content that accounts `owner_account_id` granted a stream authored on that stream —
/// the contributions an owner relays (#1280), ordered `(stream_id, seq, entry_hash)`.
///
/// Held, NOT only accepted. A receiver never takes the sender's verdict: it refolds from the same
/// authority facts, and condemns what the owner condemned. Two things break if acceptance filters
/// the relay. First, a condemned entry can be the target a device cut names, and without it a fresh
/// receiver cannot verify the accepted entries before it. Second, this function also supplies the
/// receiver's inventory: it holds a relayed row unaccepted until its refold after the session, so
/// an acceptance filter would leave the row out of what it advertises and the sender would resend
/// it every round.
///
/// But only rows a device of the author's roster signed, then or now. `author_account_id` is
/// attacker-settable: a self-signed `DeviceAdd` candidate lets anyone store content claiming a
/// granted author's name, and the fold never makes that device a roster member. Relaying such rows
/// would let a stranger reach the owner's peers through the owner.
fn relayed_content_entries(
    conn: &Connection,
    owner_account_id: AccountId,
) -> anyhow::Result<Vec<SyncContentEntry>> {
    let mut stmt = conn.prepare(
        "SELECT e.entry_hash, e.stream_id, e.author_account_id, e.seq, e.signed_bytes
         FROM content_entries e
         JOIN account_stream_ownership o ON o.stream_id = e.stream_id AND o.account_id = ?1
         WHERE e.author_account_id != ?1
           AND EXISTS (
               SELECT 1 FROM account_stream_grants g
               WHERE g.owner_account_id = ?1 AND g.stream_id = e.stream_id
                 AND g.grantee_account_id = e.author_account_id
           )
           AND EXISTS (
               SELECT 1 FROM account_roster_history r
               WHERE r.account_id = e.author_account_id
                 AND r.device_fingerprint = e.device_fingerprint
           )
         ORDER BY e.stream_id, e.seq, e.entry_hash",
    )?;
    let rows = stmt
        .query_map(params![owner_account_id.to_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(hash, stream, author, seq, signed_bytes)| {
            Ok(SyncContentEntry {
                stream_id: StreamId::from_bytes(fixed::<32>(&stream)?),
                author_account_id: AccountId::from_bytes(fixed::<32>(&author)?),
                seq: u64::from_be_bytes(fixed::<8>(&seq)?),
                entry_hash: AccountEntryHash::from_bytes(fixed::<32>(&hash)?),
                signed_bytes,
            })
        })
        .collect()
}

/// What one account created in the `/3` content this store accepted: the node ids its
/// `NodeCreate` ops name and the edge keys its `EdgeAdd` ops add. A memory belongs to the account
/// that created it, whichever device materialized it here, so this is what a consolidation checks
/// before signing a synced row as the target account's own (#1284). Sealed entries are opened with
/// this store's historical keyring for their stream, the same way the projection opens them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CreatedContent {
    pub nodes: std::collections::HashSet<String>,
    pub edges: std::collections::HashSet<String>,
    /// Every node an op by this author touches: the nodes its `NodeCreate`, `NodeUpdate`,
    /// `NodeStatus`, `NodeAnchors` and `NodeSourceHash` ops name, and the source of each edge its
    /// `EdgeAdd` ops add. A caller that drops a node can tell whether it drops this author's work.
    pub touched: std::collections::HashSet<String>,
    /// Entries by this author that this store cannot read: sealed with no key for them here, or
    /// undecodable. Their ops are unknown, so a caller that must account for everything the author
    /// created refuses on a nonzero count rather than guess.
    pub unreadable: usize,
}

/// The [`CreatedContent`] of `author_account_id` in this store's accepted `/3` content.
pub fn content_created_by(
    conn: &Connection,
    author_account_id: AccountId,
) -> anyhow::Result<CreatedContent> {
    let mut stmt = conn.prepare(
        "SELECT signed_bytes FROM content_entries WHERE author_account_id = ?1 AND accepted = 1",
    )?;
    let rows = stmt
        .query_map(params![author_account_id.to_bytes().as_slice()], |row| {
            row.get::<_, Vec<u8>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let device = crate::load_local_device(conn)?;
    let mut keyrings: HashMap<StreamId, Option<crate::account::ContentKeyring>> = HashMap::new();
    let mut created = CreatedContent::default();
    for signed_bytes in rows {
        let Ok(entry) = envelope::decode_content_signed(&signed_bytes) else {
            created.unreadable += 1;
            continue;
        };
        let plaintext;
        let op_bytes = match entry.header.crypto_suite {
            0 => entry.payload.as_slice(),
            1 => {
                let stream = entry.header.stream_id;
                if let std::collections::hash_map::Entry::Vacant(slot) = keyrings.entry(stream) {
                    let owner = account_storage::stream_owner_account(conn, stream)?;
                    slot.insert(match (owner, device.as_ref()) {
                        (Some(owner), Some(device)) =>
                            Some(crate::account::historical_content_keyring(
                                conn, owner, stream, device,
                            )?),
                        _ => None,
                    });
                }
                let opened = entry
                    .header
                    .key_id
                    .and_then(|key_id| {
                        keyrings[&stream].as_ref()?.get(crate::KeyId::from_bytes(key_id))
                    })
                    .and_then(|key| {
                        envelope::open_sealed_payload(key, &entry.payload, &entry.header_bytes).ok()
                    });
                let Some(opened) = opened else {
                    created.unreadable += 1;
                    continue;
                };
                plaintext = opened;
                plaintext.as_slice()
            },
            _ => {
                created.unreadable += 1;
                continue;
            },
        };
        match crate::op::decode(op_bytes) {
            Ok(crate::op::DecodedOp::Known(crate::op::MemoryOp::NodeCreate {
                node_id, ..
            })) => {
                created.touched.insert(node_id.as_str().to_string());
                created.nodes.insert(node_id.as_str().to_string());
            },
            Ok(crate::op::DecodedOp::Known(
                crate::op::MemoryOp::NodeUpdate { node_id, .. }
                | crate::op::MemoryOp::NodeStatus { node_id, .. }
                | crate::op::MemoryOp::NodeAnchors { node_id, .. }
                | crate::op::MemoryOp::NodeSourceHash { node_id, .. },
            )) => {
                created.touched.insert(node_id.as_str().to_string());
            },
            Ok(crate::op::DecodedOp::Known(crate::op::MemoryOp::EdgeAdd { edge })) => {
                created.touched.insert(edge.source_node_id.as_str().to_string());
                created.edges.insert(edge.edge_key().as_str().to_string());
            },
            Ok(_) => {},
            Err(_) => created.unreadable += 1,
        }
    }
    Ok(created)
}

#[cfg(test)]
#[path = "storage/tests.rs"]
mod tests;
