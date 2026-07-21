//! C2 `/3` candidate-DAG ingest and dense-chain structural classification (§16).
//!
//! This layer verifies an exact content-addressed `roster_ref`, signatures, and dense predecessor
//! coordinates. It deliberately never sets `accepted`: C3 must evaluate authority, cuts,
//! freshness, and branch selection together before content can reach the live projection.

use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::super::ops::{self, AccountOp, DecodedAccountOp};
use super::super::{envelope as account_envelope, storage as account_storage};
use super::acceptance::{
    self, CitedFreshness, CitedGrantAuthority, CitedOwnership, CitedRosterAuthority,
    ContentAcceptance, ContentParkReason, SubjectAuthorityHold,
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

type EntryHash = [u8; 32];

const PENDING_REFOLD_CONTENT_CANDIDATE: i64 = 1;
const PENDING_REFOLD_ACCOUNT_CHANGE: i64 = 2;

const PRE_VERIFY_PER_AUTHOR_MAX: i64 = 64;
const PRE_VERIFY_GLOBAL_MAX: i64 = 256;
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

#[derive(Debug, Default)]
pub(in crate::account) struct ContentPromotionOutcome {
    pub(in crate::account) scope: Option<ContentCapacityScope>,
    pub(in crate::account) entry_hashes: Vec<EntryHash>,
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
    entry_hash: &EntryHash,
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
    if roster.entry_hash != content.header.roster_ref
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
    if tx
        .query_row(
            "SELECT 1 FROM content_pre_verify WHERE signed_hash = ?1",
            [signed_hash.as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
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
    enforce_pre_verify_budget(tx, signed.header.author_account_id, &signed_hash)
}

fn enforce_pre_verify_budget(
    tx: &Transaction<'_>,
    author: super::super::AccountId,
    inserted_hash: &EntryHash,
) -> rusqlite::Result<ContentIngestOutcome> {
    let mut scopes = Vec::new();
    if evict_oldest_pre_verify(tx, Some(author), PRE_VERIFY_PER_AUTHOR_MAX)? > 0 {
        scopes.push(ContentCapacityScope::PreVerifyAuthor);
    }
    if !pre_verify_contains(tx, inserted_hash)? {
        return Ok(ContentIngestOutcome::CapacityReached {
            scope: ContentCapacityScope::PreVerifyAuthor,
        });
    }
    if evict_oldest_pre_verify(tx, None, PRE_VERIFY_GLOBAL_MAX)? > 0 {
        scopes.push(ContentCapacityScope::PreVerifyGlobal);
    }
    if !pre_verify_contains(tx, inserted_hash)? {
        return Ok(ContentIngestOutcome::CapacityReached {
            scope: ContentCapacityScope::PreVerifyGlobal,
        });
    }
    if scopes.is_empty() {
        Ok(ContentIngestOutcome::PreVerify)
    } else {
        Ok(ContentIngestOutcome::PreVerifyWithEviction { scopes })
    }
}

fn pre_verify_contains(conn: &Connection, signed_hash: &EntryHash) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM content_pre_verify WHERE signed_hash = ?1)",
        [signed_hash.as_slice()],
        |row| row.get(0),
    )
}

fn evict_oldest_pre_verify(
    conn: &Connection,
    author: Option<super::super::AccountId>,
    limit: i64,
) -> rusqlite::Result<usize> {
    let author = author.map(super::super::AccountId::to_bytes);
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM content_pre_verify
         WHERE (?1 IS NULL OR claimed_author_account_id = ?1)",
        params![author.as_ref().map(<[u8; 32]>::as_slice)],
        |row| row.get(0),
    )?;
    if count <= limit {
        return Ok(0);
    }
    conn.execute(
        "DELETE FROM content_pre_verify WHERE signed_hash IN (
             SELECT signed_hash FROM content_pre_verify
             WHERE (?1 IS NULL OR claimed_author_account_id = ?1)
             ORDER BY received_at_ms, signed_hash LIMIT ?2
         )",
        params![author.as_ref().map(<[u8; 32]>::as_slice), count - limit],
    )
}

pub(in crate::account) fn promote_pre_verify_for_account(
    tx: &Transaction<'_>,
    account_id: super::super::AccountId,
    now_ms: i64,
) -> anyhow::Result<ContentPromotionOutcome> {
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
                    delete_pre_verify(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
            };
            let public = match resolve_roster_key(tx, &signed) {
                Ok(Some(public)) => public,
                Ok(None) => continue,
                Err(_) => {
                    delete_pre_verify(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
            };
            let verified = match envelope::verify_content_signed(&raw, &public) {
                Ok(verified) => verified,
                Err(_) => {
                    delete_pre_verify(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
            };
            match stored_candidate_bytes(tx, &verified.entry_hash)? {
                Some(stored) if stored != raw => {
                    delete_pre_verify(tx, &signed_hash)?;
                    progressed = true;
                    continue;
                },
                Some(_) => {},
                None if let Some(scope) = candidate_capacity(tx, &verified, raw.len())? => {
                    outcome.scope.get_or_insert(scope);
                    outcome.entry_hashes.push(verified.entry_hash);
                    delete_pre_verify(tx, &signed_hash)?;
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
            delete_pre_verify(tx, &signed_hash)?;
            progressed = true;
        }
        if !progressed {
            return Ok(outcome);
        }
    }
}

fn delete_pre_verify(tx: &Transaction<'_>, signed_hash: &[u8]) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM content_pre_verify WHERE signed_hash = ?1", [signed_hash])?;
    Ok(())
}

fn candidate_capacity(
    tx: &Transaction<'_>,
    entry: &VerifiedContentEntry,
    incoming_bytes: usize,
) -> anyhow::Result<Option<ContentCapacityScope>> {
    // Both budgets are REMOTE-abuse ceilings (#652): exclude the local device's OWN signed rows so
    // a large local history never starves foreign ingest. Key the exclusion on the local DEVICE
    // FINGERPRINT, NOT `author_account_id` — the latter is attacker-settable (a self-signed
    // DeviceAdd can store forged content under a claimed local account id), while a row can
    // carry the local fingerprint only if it was signed with the local device key
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
         WHERE author_account_id = ?1 AND (device_fingerprint != ?2 OR ?2 IS NULL)",
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
         FROM content_entries WHERE device_fingerprint != ?1 OR ?1 IS NULL",
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
             grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
             received_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, ?12)",
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
            signed_bytes,
            now_ms,
        ],
    )?;
    Ok(())
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
            set_status(tx, &child, ContentStatus::RetainedUnfolded)?;
            queue.push_back((child, child_seq));
        }
    }
    Ok(())
}

/// Every authority fact one candidate is evaluated against, resolved once from the current fold so
/// the two evaluator phases (eligibility, then the finished verdict) read one consistent snapshot.
struct ResolvedEntry {
    entry_hash: EntryHash,
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
) -> anyhow::Result<()> {
    refold_content_stream(tx, stream_id)?;
    if content_projected_tables_exist(tx)? {
        content_projection::reproject_accepted_content_stream(tx, stream_id)?;
    }
    if pending_refold_table_exists(tx)? {
        clear_pending_content_refold(tx, stream_id)?;
    }
    Ok(())
}

pub(in crate::account) fn finalize_affected_streams(
    tx: &Transaction<'_>,
    streams: &[StreamId],
) -> anyhow::Result<()> {
    for &stream_id in streams {
        refold_and_project_stream_in_tx(tx, stream_id)?;
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
    let handled: HashSet<EntryHash> = resolved.iter().map(|r| r.entry_hash).collect();
    declassify_rows_absent_from(tx, stream_id, &handled)?;
    if resolved.is_empty() {
        return Ok(());
    }
    let view: HashMap<EntryHash, ContentEntryHeader> =
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
    let mut raw: HashMap<EntryHash, ContentAcceptance> = HashMap::with_capacity(resolved.len());
    for r in &resolved {
        let selected = selection.accepted.contains(&r.entry_hash);
        let verdict = verdict_for(r, &view, selected, EvaluatorPhase::Finished)?
            .expect("the finished evaluator always returns a verdict");
        raw.insert(r.entry_hash, verdict);
    }
    let accepted = prefix_closed_accepted(&resolved, &selection, &raw);
    for r in &resolved {
        let hash = r.entry_hash;
        let verdict = if accepted.contains(&hash) {
            ContentAcceptance::Accepted
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

/// One queued stream's current settle cost. The cost comes from the V080 `content_stream_stats`
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
/// the V080 `content_stream_stats` aggregate (counts and `length(signed_bytes) + 32` per row,
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
/// calls never re-list them. The scan stays one bounded page query per call, backed by the V080
/// `content_streams_pending_refold_order` index.
fn list_pending_refold_streams_page(
    conn: &Connection,
    cursor: Option<(i64, StreamId)>,
    budget: &ContentRefoldBudget,
    limit: usize,
) -> anyhow::Result<Vec<PendingRefoldListing>> {
    #[cfg(test)]
    SETTLE_LISTING_QUERIES.fetch_add(1, Ordering::Relaxed);
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

/// Test-only work counters proving a settle call's SQL/lock work is bounded by the budget and not
/// the backlog (#798 review): listing queries (page listings plus the one targeted oversize query
/// in maintenance mode), per-stream admission probes (each an IMMEDIATE transaction), and the one
/// O(1) queue-empty completion probe. Production semantics are unchanged; nextest isolates tests
/// per process.
#[cfg(test)]
static SETTLE_LISTING_QUERIES: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static SETTLE_ADMISSION_PROBES: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static SETTLE_COMPLETION_PROBES: AtomicUsize = AtomicUsize::new(0);

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
    SETTLE_COMPLETION_PROBES.fetch_add(1, Ordering::Relaxed);
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
    SETTLE_LISTING_QUERIES.fetch_add(1, Ordering::Relaxed);
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
) -> PendingRefoldOutcome {
    #[cfg(test)]
    SETTLE_ADMISSION_PROBES.fetch_add(1, Ordering::Relaxed);
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

    if let Err(error) = refold_and_project_stream_in_tx(&tx, stream_id) {
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
/// Counts and bytes are the V080 `content_stream_stats` fold-cost units: a candidate row and
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
}

/// Fold the streams `content_ingest` deferred, oldest first, until `budget` runs out, and report
/// exactly what was settled, deferred, and retained. One refold per settled stream (O(settled
/// streams), NOT O(ingested entries)); each stream settles in its OWN IMMEDIATE transaction via
/// the shared [`refold_and_project_stream_in_tx`] finalizer, so a poisoned stream rolls back
/// alone — its queue mark is kept for retry and DEMOTED behind every currently-queued row (so the
/// next call tries other streams first instead of head-of-line blocking on the poisoned stream
/// forever), it lands in [`ContentSettleReport::failures`], and it never blocks the rest of the
/// batch. A BEGIN IMMEDIATE lock/BUSY race is NOT poison: it is retried once, then counted in
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
) -> anyhow::Result<ContentSettleReport> {
    settle_pending_content_refolds_inner(conn, budget, || {})
}

fn settle_pending_content_refolds_inner(
    conn: &Connection,
    budget: &ContentRefoldBudget,
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
    let mut listed_any = false;
    loop {
        let page = list_pending_refold_streams_page(conn, cursor, budget, batch)?;
        if let Some(hook) = after_first_listing.take() {
            hook();
        }
        if page.is_empty() {
            break;
        }
        listed_any = true;
        let page_len = page.len();
        let mut page_admitted = false;
        let mut page_vanished = false;
        for listing in page {
            cursor = Some((listing.first_enqueued_at_ms, listing.stream_id));
            let stream_id = listing.stream_id;
            if !listed_fits_remaining(budget, consumed, listing.listed_work) {
                report.deferred_budget += 1;
                continue;
            }
            let outcome = settle_one_pending_refold(conn, stream_id, budget, consumed);
            let (admitted, vanished, demote) =
                record_settle_outcome(&mut report, &mut consumed, stream_id, outcome);
            if demote {
                demote_pending_refold(conn, stream_id)?;
            }
            page_admitted |= admitted;
            page_vanished |= vanished;
        }
        if page_len < batch
            || !budget_could_fit_more(budget, consumed)
            || !(page_admitted || page_vanished)
        {
            break;
        }
    }
    // Oversize maintenance: the listing above never discovers rows that exceed a fresh budget.
    // Probe for the OLDEST such row ONLY when normal discovery listed ZERO eligible rows on its
    // first page — oversize rows are then the only work left. The `LIMIT 1` probe is oldest-first
    // via the order index but degenerates to a FULL QUEUE SCAN when no oversize row exists, so
    // running it whenever a slot remained would make every budgeted call O(queue) (#798
    // adversarial review). The admission goes through the same in-transaction revalidation, which
    // honors the intentional exceedance and the stream-slot limit.
    if budget.allow_one_oversize
        && !listed_any
        && !consumed.oversize_slot_spent
        && consumed.attempted_streams < budget.max_streams
        && let Some(stream_id) = oldest_oversize_pending_refold(conn, budget)?
    {
        let outcome = settle_one_pending_refold(conn, stream_id, budget, consumed);
        let (_, _, demote) = record_settle_outcome(&mut report, &mut consumed, stream_id, outcome);
        if demote {
            demote_pending_refold(conn, stream_id)?;
        }
    }
    report.queue_empty = !pending_refold_queue_nonempty(conn)?;
    Ok(report)
}

/// Settle ONE named stream's queued refold debt, in its own IMMEDIATE transaction — the
/// pending-fold barrier's inline drain (rag-rat-core `memory_write`). Targeted on purpose: the
/// barrier owes THIS stream's acceptance before it may read completeness, so draining the global
/// oldest-first queue (which could settle a different stream) would not unblock it. Bypasses the
/// fairness order and never demotes — a failure here keeps the row exactly where it was. Returns
/// whether the stream's queue row is gone afterwards (a vanished row counts as clear).
pub fn settle_pending_content_refold_for_stream(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<bool> {
    let outcome = settle_one_pending_refold(
        conn,
        stream_id,
        &ContentRefoldBudget::unbounded(),
        ContentSettleConsumption::default(),
    );
    match outcome {
        PendingRefoldOutcome::Settled { .. } | PendingRefoldOutcome::Missing => Ok(true),
        // The unbounded budget admits every present row, so a deferral is unreachable; a failure
        // (or a persistent begin-lock race) retains the row.
        PendingRefoldOutcome::Deferred { .. }
        | PendingRefoldOutcome::Failed { .. }
        | PendingRefoldOutcome::TransientLock { .. } =>
            Ok(!content_stream_has_pending_refold(conn, stream_id)?),
    }
}

/// Narrow the branch-selected winners to a contiguous accepted prefix per coordinate.
/// `select_accepted_branch` already returns one hash-linked chain per coordinate, but freshness
/// (evaluated after selection) can park a mid-chain winner, and the accepted projection must stay
/// prefix-closed — so keep only the run from seq 0 whose own finished verdict is `Accepted`.
fn prefix_closed_accepted(
    resolved: &[ResolvedEntry],
    selection: &candidate::BranchSelection,
    raw: &HashMap<EntryHash, ContentAcceptance>,
) -> HashSet<EntryHash> {
    let mut chains: HashMap<ChainCoordinate, Vec<(u64, EntryHash)>> = HashMap::new();
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

/// Reset the status of every stream row NOT in `handled` to the unclassified baseline. Those rows
/// could not be decoded — they are absent from the refold's per-entry writes, so without this a
/// corrupt blob (or a row written outside `content_ingest`) would keep whatever verdict a prior
/// fold derived. `accepted` is cleared separately by the caller's blanket update.
fn declassify_rows_absent_from(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    handled: &HashSet<EntryHash>,
) -> anyhow::Result<()> {
    let hashes: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare("SELECT entry_hash FROM content_entries WHERE stream_id = ?1")?;
        stmt.query_map([stream_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for bytes in hashes {
        let hash = fixed::<32>(&bytes)?;
        if !handled.contains(&hash) {
            set_status(tx, &hash, ContentStatus::RetainedUnfolded)?;
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
    view: &HashMap<EntryHash, ContentEntryHeader>,
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
    let view: HashMap<EntryHash, ContentEntryHeader> =
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
        HashMap<(AccountId, EntryHash, DeviceFingerprint), AuthorityQuery<CitedRosterAuthority>>,
    grant: HashMap<(EntryHash, AccountId, DeviceFingerprint), AuthorityQuery<CitedGrantAuthority>>,
    freshness: HashMap<(AccountId, u64), AuthorityFreshness>,
    contested: HashMap<AccountId, bool>,
}

impl AuthorityCaches {
    fn freshness(
        &mut self,
        tx: &Transaction<'_>,
        account_id: AccountId,
        asserted_auth_len: u64,
    ) -> anyhow::Result<AuthorityFreshness> {
        if let Some(cached) = self.freshness.get(&(account_id, asserted_auth_len)) {
            return Ok(*cached);
        }
        let state = account_storage::auth_len_freshness(tx, account_id, asserted_auth_len)?;
        self.freshness.insert((account_id, asserted_auth_len), state);
        Ok(state)
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
) -> anyhow::Result<Vec<(EntryHash, ContentEntryHeader)>> {
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
        let entry_hash = fixed::<32>(&entry_hash)?;
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
    let view: HashMap<EntryHash, ContentEntryHeader> =
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
fn reachable_entries(view: &HashMap<EntryHash, ContentEntryHeader>) -> HashSet<EntryHash> {
    let mut by_prev: HashMap<EntryHash, Vec<&EntryHash>> = HashMap::new();
    let mut queue: VecDeque<&EntryHash> = VecDeque::new();
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
    grant_id: EntryHash,
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
    entry_hash: &EntryHash,
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
    entry_hash: &EntryHash,
    status: ContentStatus,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO content_entry_status(entry_hash, status, detail) VALUES(?1, ?2, NULL)
         ON CONFLICT(entry_hash) DO UPDATE SET status = excluded.status, detail = NULL",
        params![entry_hash.as_slice(), status.as_db_str()],
    )?;
    Ok(())
}

fn status_for(tx: &Transaction<'_>, hash: &EntryHash) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
        [hash.as_slice()],
        |row| row.get(0),
    )
    .optional()
}

fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("expected {N} bytes, got {}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;

    use super::super::super::envelope::{AccountEntryHeader, sign_account_entry};
    use super::super::super::id::account_id_from_genesis_payload;
    use super::super::ContentEntryHeader;
    use super::*;
    use crate::account::ops::entry_type;
    use crate::device::{DeviceSecret, DeviceX25519Secret};
    use crate::stream::StreamId;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    #[test]
    fn pending_refold_guard_is_false_before_the_queue_table_exists() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(
            !content_stream_has_pending_refold(&conn, StreamId::from_bytes([0x41; 32])).unwrap()
        );
    }

    fn roster(
        conn: &Connection,
        secret: &DeviceSecret,
    ) -> (super::super::super::AccountId, EntryHash) {
        let (account_id, signed) = signed_roster(secret);
        conn.execute(
            "INSERT INTO account_entries(entry_hash, account_id, log_id, device_fingerprint, seq,
             prev_hash, parent_ref, authority_ref, entry_type, accepted, signed_bytes, \
             received_at_ms)
             VALUES(?1, ?2, 0, ?3, 0, NULL, NULL, NULL, 0, 1, ?4, 1)",
            params![
                signed.entry_hash.as_slice(),
                account_id.to_bytes().as_slice(),
                secret.public().fingerprint().to_bytes().as_slice(),
                signed.signed_bytes,
            ],
        )
        .unwrap();
        (account_id, signed.entry_hash)
    }

    fn signed_roster(
        secret: &DeviceSecret,
    ) -> (super::super::super::AccountId, super::super::super::envelope::SignedAccountEntry) {
        let x = DeviceX25519Secret::from_seed(&[0x81; 32]).public().to_bytes();
        let op = AccountOp::AccountGenesis {
            ed25519_pubkey: secret.public().to_bytes(),
            x25519_pubkey: x,
            nonce16: [0; 16],
            created_at_ms: 1,
            label: None,
        };
        let payload = ops::encode(&op).unwrap();
        let account_id = account_id_from_genesis_payload(&payload);
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: secret.public().fingerprint(),
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(secret, &header, &payload).unwrap();
        (account_id, signed)
    }

    fn signed_device_add(
        founder: &DeviceSecret,
        member: &DeviceSecret,
        account_id: super::super::super::AccountId,
        genesis_hash: EntryHash,
    ) -> super::super::super::envelope::SignedAccountEntry {
        signed_device_add_at(founder, member, account_id, 1, genesis_hash, genesis_hash, 1)
    }

    fn signed_device_add_at(
        founder: &DeviceSecret,
        member: &DeviceSecret,
        account_id: super::super::super::AccountId,
        seq: u64,
        previous: EntryHash,
        genesis_hash: EntryHash,
        auth_len: u64,
    ) -> super::super::super::envelope::SignedAccountEntry {
        let op = AccountOp::DeviceAdd {
            device_fingerprint: member.public().fingerprint(),
            ed25519_pubkey: member.public().to_bytes(),
            x25519_pubkey: DeviceX25519Secret::from_seed(&[0x82; 32]).public().to_bytes(),
            role: crate::account::DeviceRole::Member,
            label: None,
        };
        let payload = ops::encode(&op).unwrap();
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: founder.public().fingerprint(),
            seq,
            prev_hash: Some(previous),
            parent_ref: None,
            entry_type: entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 0,
            auth_len,
            key_id: None,
            authority_ref: Some(genesis_hash),
        };
        sign_account_entry(founder, &header, &payload).unwrap()
    }

    fn content(
        secret: &DeviceSecret,
        account_id: super::super::super::AccountId,
        roster_ref: EntryHash,
        seq: u64,
        previous: Option<EntryHash>,
    ) -> SignedContentEntry {
        let header = ContentEntryHeader {
            stream_id: StreamId::from_bytes([0x44; 32]),
            author_account_id: account_id,
            device_fingerprint: secret.public().fingerprint(),
            seq,
            lamport: seq.saturating_add(1),
            prev_hash: previous,
            grant_id: None,
            roster_ref,
            owner_auth_len: u64::MAX,
            author_auth_len: u64::MAX,
            crypto_suite: 0,
            key_id: None,
        };
        envelope::sign_content_entry(secret, &header, &[0xf6]).unwrap()
    }

    fn seed_content_candidates(
        conn: &Connection,
        author: super::super::super::AccountId,
        device_fingerprint: [u8; 32],
        namespace: u64,
        count: usize,
        signed_bytes_len: usize,
    ) {
        let raw = vec![0_u8; signed_bytes_len];
        for ordinal in 0..count {
            let hash = cbor::sha256(
                &[namespace.to_be_bytes().as_slice(), (ordinal as u64).to_be_bytes().as_slice()]
                    .concat(),
            );
            conn.execute(
                "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, 0, ?9, 0)",
                params![
                    hash.as_slice(),
                    [1_u8; 32].as_slice(),
                    author.to_bytes().as_slice(),
                    device_fingerprint.as_slice(),
                    (ordinal as u64).to_be_bytes().as_slice(),
                    [3_u8; 32].as_slice(),
                    [0_u8; 8].as_slice(),
                    [0_u8; 8].as_slice(),
                    raw.as_slice(),
                ],
            )
            .unwrap();
        }
    }

    /// A device fingerprint distinct from any real local device — the pre-#652 default seed used
    /// for candidates that stand in for FOREIGN, remotely-signed rows in the capacity tests.
    const FOREIGN_FP: [u8; 32] = [2_u8; 32];

    #[test]
    fn exact_roster_ref_verifies_and_full_u64_counters_persist() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[1; 32]);
        let (account, roster_ref) = roster(&conn, &secret);
        let signed = content(&secret, account, roster_ref, 0, None);
        assert_eq!(
            content_ingest(&conn, &signed.signed_bytes, 2).unwrap(),
            ContentIngestOutcome::Ingested { status: "retained_unfolded".into() }
        );
        let (seq, owner, author, accepted): (Vec<u8>, Vec<u8>, Vec<u8>, i64) = conn
            .query_row(
                "SELECT seq, owner_auth_len, author_auth_len, accepted FROM content_entries",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(seq, 0u64.to_be_bytes());
        assert_eq!(owner, u64::MAX.to_be_bytes());
        assert_eq!(author, u64::MAX.to_be_bytes());
        assert_eq!(accepted, 0, "C2 never manufactures authority acceptance");
    }

    #[test]
    fn missing_predecessor_heals_when_dense_parent_arrives() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[2; 32]);
        let (account, roster_ref) = roster(&conn, &secret);
        let genesis = content(&secret, account, roster_ref, 0, None);
        let child = content(&secret, account, roster_ref, 1, Some(genesis.entry_hash));
        assert_eq!(
            content_ingest(&conn, &child.signed_bytes, 2).unwrap(),
            ContentIngestOutcome::Ingested { status: "parked{missing_predecessor}".into() }
        );
        content_ingest(&conn, &genesis.signed_bytes, 3).unwrap();
        assert_eq!(
            status_for(&conn.unchecked_transaction().unwrap(), &child.entry_hash).unwrap(),
            Some("retained_unfolded".into())
        );
    }

    #[test]
    fn unknown_roster_parks_and_wrong_coordinate_is_order_independently_retained() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[3; 32]);
        let account = super::super::super::AccountId::from_bytes([0x55; 32]);
        let signed = content(&secret, account, [0x66; 32], 0, None);
        assert_eq!(
            content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
            ContentIngestOutcome::PreVerify
        );

        let (account, roster_ref) = roster(&conn, &secret);
        let genesis = content(&secret, account, roster_ref, 0, None);
        content_ingest(&conn, &genesis.signed_bytes, 2).unwrap();
        let mut wrong = content(&secret, account, roster_ref, 1, Some(genesis.entry_hash));
        wrong.header.stream_id = StreamId::from_bytes([0x77; 32]);
        wrong = envelope::sign_content_entry(&secret, &wrong.header, &[0xf6]).unwrap();
        assert_eq!(
            content_ingest(&conn, &wrong.signed_bytes, 3).unwrap(),
            ContentIngestOutcome::Ingested { status: "parked{missing_predecessor}".into() }
        );

        let reverse = db();
        let (reverse_account, reverse_roster) = roster(&reverse, &secret);
        let reverse_genesis = content(&secret, reverse_account, reverse_roster, 0, None);
        let mut reverse_wrong =
            content(&secret, reverse_account, reverse_roster, 1, Some(reverse_genesis.entry_hash));
        reverse_wrong.header.stream_id = StreamId::from_bytes([0x77; 32]);
        reverse_wrong =
            envelope::sign_content_entry(&secret, &reverse_wrong.header, &[0xf6]).unwrap();
        content_ingest(&reverse, &reverse_wrong.signed_bytes, 2).unwrap();
        content_ingest(&reverse, &reverse_genesis.signed_bytes, 3).unwrap();
        assert_eq!(
            reverse
                .query_row("SELECT count(*) FROM content_entries", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            reverse
                .query_row(
                    "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
                    [reverse_wrong.entry_hash.as_slice()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "parked{missing_predecessor}"
        );
    }

    #[test]
    fn account_roster_arrival_promotes_parked_content_in_the_same_transaction() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[4; 32]);
        let (account, roster) = signed_roster(&secret);
        let signed = content(&secret, account, roster.entry_hash, 0, None);
        assert_eq!(
            content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
            ContentIngestOutcome::PreVerify
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );

        super::super::super::storage::account_ingest(&conn, &roster.signed_bytes, 2).unwrap();

        let (parked, stored, status, accepted): (i64, i64, String, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM content_pre_verify),
                        (SELECT count(*) FROM content_entries), s.status, e.accepted
                 FROM content_entries e JOIN content_entry_status s USING(entry_hash)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((parked, stored), (0, 1));
        assert_eq!(status, "retained_unfolded");
        assert_eq!(accepted, 0, "promotion cannot cross the C2/C3 authority boundary");
    }

    #[test]
    fn promoting_pre_verify_content_marks_the_stream_for_settle() {
        // A pre-verify PARK does not mark the stream (the entry is not a candidate yet, and
        // `content_ingest` returns before its mark). When the roster key later arrives and
        // `account_ingest` PROMOTES the parked entry into a candidate, the promotion path must mark
        // the stream — otherwise a promoted entry the account fold later accepts would carry no
        // queue row, settle would skip it, and its /3 projection would stay stale (#652/#699).
        let conn = db();
        let secret = DeviceSecret::from_seed(&[7; 32]);
        let (account, roster) = signed_roster(&secret);
        let signed = content(&secret, account, roster.entry_hash, 0, None);

        assert_eq!(
            content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
            ContentIngestOutcome::PreVerify,
        );
        assert_eq!(pending_refold_count(&conn), 0, "a pre-verify park does not mark the stream");

        super::super::super::storage::account_ingest(&conn, &roster.signed_bytes, 2).unwrap();
        assert_eq!(
            pending_refold_count(&conn),
            1,
            "promoting the parked entry marks its stream so a later settle refolds + reprojects it",
        );
    }

    #[test]
    fn device_add_exact_roster_promotes_then_verifies_non_founder_content() {
        let conn = db();
        let founder = DeviceSecret::from_seed(&[13; 32]);
        let member = DeviceSecret::from_seed(&[14; 32]);
        let (account, genesis) = signed_roster(&founder);
        super::super::super::storage::account_ingest(&conn, &genesis.signed_bytes, 1).unwrap();
        let add = signed_device_add(&founder, &member, account, genesis.entry_hash);
        let first = content(&member, account, add.entry_hash, 0, None);
        assert_eq!(
            content_ingest(&conn, &first.signed_bytes, 2).unwrap(),
            ContentIngestOutcome::PreVerify
        );

        super::super::super::storage::account_ingest(&conn, &add.signed_bytes, 3).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let second = content(&member, account, add.entry_hash, 1, Some(first.entry_hash));
        assert_eq!(
            content_ingest(&conn, &second.signed_bytes, 4).unwrap(),
            ContentIngestOutcome::Ingested { status: "retained_unfolded".into() }
        );

        let outsider = DeviceSecret::from_seed(&[15; 32]);
        let wrong = content(&outsider, account, add.entry_hash, 0, None);
        assert!(matches!(
            content_ingest(&conn, &wrong.signed_bytes, 5).unwrap(),
            ContentIngestOutcome::Rejected(_)
        ));
    }

    #[test]
    fn duplicate_pre_verify_is_idempotent_and_does_not_consume_queue_budget() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[5; 32]);
        let account = super::super::super::AccountId::from_bytes([0x65; 32]);
        let signed = content(&secret, account, [0x75; 32], 0, None);
        for now in 1..=3 {
            assert_eq!(
                content_ingest(&conn, &signed.signed_bytes, now).unwrap(),
                ContentIngestOutcome::PreVerify
            );
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn bad_signature_and_wrong_exact_roster_are_rejected_without_storage() {
        let conn = db();
        let enrolled = DeviceSecret::from_seed(&[6; 32]);
        let attacker = DeviceSecret::from_seed(&[7; 32]);
        let (account, roster_ref) = roster(&conn, &enrolled);

        let wrong_device = content(&attacker, account, roster_ref, 0, None);
        assert!(matches!(
            content_ingest(&conn, &wrong_device.signed_bytes, 1).unwrap(),
            ContentIngestOutcome::Rejected(_)
        ));

        let mut bad_signature = content(&enrolled, account, roster_ref, 0, None).signed_bytes;
        let last = bad_signature.last_mut().unwrap();
        *last ^= 1;
        assert!(matches!(
            content_ingest(&conn, &bad_signature, 2).unwrap(),
            ContentIngestOutcome::Rejected(_)
        ));
        assert_eq!(
            conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn equivocations_remain_first_class_but_unaccepted() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[8; 32]);
        let (account, roster_ref) = roster(&conn, &secret);
        let first = content(&secret, account, roster_ref, 0, None);
        let mut second_header = first.header.clone();
        second_header.lamport += 1;
        let second = envelope::sign_content_entry(&secret, &second_header, &[0xf6]).unwrap();
        let first_child = content(&secret, account, roster_ref, 1, Some(first.entry_hash));
        let second_child = content(&secret, account, roster_ref, 1, Some(second.entry_hash));
        for (received, signed) in
            [&second_child, &first_child, &second, &first].into_iter().enumerate()
        {
            content_ingest(&conn, &signed.signed_bytes, received as i64).unwrap();
        }
        assert_eq!(
            conn.query_row("SELECT count(*), sum(accepted) FROM content_entries", [], |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?
            )),)
                .unwrap(),
            (4, 0)
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM content_entry_status WHERE status = 'retained_unfolded'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            4
        );
    }

    #[test]
    fn pre_verify_evicts_oldest_per_author_and_keeps_newest() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[9; 32]);
        let account = super::super::super::AccountId::from_bytes([0x79; 32]);
        let mut first_hash = None;
        let mut newest_hash = None;
        for seq in 0..=PRE_VERIFY_PER_AUTHOR_MAX as u64 {
            let previous = (seq > 0).then_some([seq as u8; 32]);
            let signed = content(&secret, account, [seq as u8; 32], seq, previous);
            let hash = cbor::sha256(&signed.signed_bytes);
            first_hash.get_or_insert(hash);
            newest_hash = Some(hash);
            let outcome = content_ingest(&conn, &signed.signed_bytes, seq as i64).unwrap();
            if seq == PRE_VERIFY_PER_AUTHOR_MAX as u64 {
                assert_eq!(outcome, ContentIngestOutcome::PreVerifyWithEviction {
                    scopes: vec![ContentCapacityScope::PreVerifyAuthor]
                });
            }
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            PRE_VERIFY_PER_AUTHOR_MAX
        );
        assert!(!pre_verify_contains(&conn, &first_hash.unwrap()).unwrap());
        assert!(pre_verify_contains(&conn, &newest_hash.unwrap()).unwrap());
    }

    #[test]
    fn content_pre_verify_author_cap_protects_other_authors_below_global_cap() {
        // The per-author pre-verify cap must stop ONE claimed author from evicting ANOTHER author's
        // parked rows while the global queue (PRE_VERIFY_GLOBAL_MAX) still has headroom. Park one
        // older row under author B, then flood author A with PRE_VERIFY_PER_AUTHOR_MAX + 1 rows at
        // strictly newer received_at_ms; the total (PER_AUTHOR_MAX + 1) stays far under the global
        // cap, so ONLY the per-author eviction can fire. If the per-author eviction DELETE were
        // made global-scoped (dropping `WHERE claimed_author_account_id = ?1`), it would
        // evict the globally-oldest row — author B's — instead of author A's own oldest,
        // and this test fails. The single-author test above cannot catch that regression;
        // the account layer's `pre_verify_budget_evicts_oldest_per_account_and_globally` is
        // the sibling guard.
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x5b; 32]);
        let author_a = super::super::super::AccountId::from_bytes([0x0a; 32]);
        let author_b = super::super::super::AccountId::from_bytes([0x0b; 32]);

        // Author B parks one row FIRST, at the oldest received_at_ms — the globally-oldest row, so
        // a global-scoped eviction would target exactly this one.
        let b_entry = content(&secret, author_b, [0xee; 32], 0, None);
        let b_hash = cbor::sha256(&b_entry.signed_bytes);
        assert_eq!(
            content_ingest(&conn, &b_entry.signed_bytes, 0).unwrap(),
            ContentIngestOutcome::PreVerify,
        );

        // Author A floods PER_AUTHOR_MAX + 1 rows, each strictly newer than author B's. Only the
        // (MAX+1)th trips the per-author cap, evicting author A's OWN oldest row.
        for seq in 0..=PRE_VERIFY_PER_AUTHOR_MAX as u64 {
            // A garbage but non-null predecessor for seq > 0 (prev_hash must be null iff seq == 0);
            // it keeps every row a distinct pre-verify entry and never resolves (still parked).
            let previous = (seq > 0).then_some([seq as u8; 32]);
            let a_entry = content(&secret, author_a, [0xee; 32], seq, previous);
            let received_at_ms = seq as i64 + 1; // strictly newer than author B's 0
            let outcome = content_ingest(&conn, &a_entry.signed_bytes, received_at_ms).unwrap();
            if seq == PRE_VERIFY_PER_AUTHOR_MAX as u64 {
                assert_eq!(outcome, ContentIngestOutcome::PreVerifyWithEviction {
                    scopes: vec![ContentCapacityScope::PreVerifyAuthor],
                });
            } else {
                assert_eq!(outcome, ContentIngestOutcome::PreVerify);
            }
        }

        // Author B's older row survives — author A's per-author eviction must not reach across it.
        assert!(
            pre_verify_contains(&conn, &b_hash).unwrap(),
            "author A's flood must NOT evict author B's parked row",
        );
        // Author A is held to EXACTLY the per-author cap.
        let a_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_pre_verify WHERE claimed_author_account_id = ?1",
                [author_a.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(a_count, PRE_VERIFY_PER_AUTHOR_MAX, "author A is capped at the per-author max");
        // The global total is PER_AUTHOR_MAX + 1 (author A's cap + author B's surviving row), well
        // under the global cap — proving the global eviction never fired.
        let total: i64 = conn
            .query_row("SELECT count(*) FROM content_pre_verify", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, PRE_VERIFY_PER_AUTHOR_MAX + 1);
    }

    #[test]
    fn reverse_delivery_heals_a_long_dense_chain_without_recursion() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[10; 32]);
        let (account, roster_ref) = roster(&conn, &secret);
        let mut entries = Vec::new();
        let mut previous = None;
        for seq in 0..256 {
            let signed = content(&secret, account, roster_ref, seq, previous);
            previous = Some(signed.entry_hash);
            entries.push(signed);
        }
        for (received, signed) in entries.iter().rev().enumerate() {
            content_ingest(&conn, &signed.signed_bytes, received as i64).unwrap();
        }
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM content_entry_status WHERE status = 'retained_unfolded'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            256
        );
    }

    #[test]
    fn maximum_sequence_with_a_missing_predecessor_is_stored_without_overflow() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[11; 32]);
        let (account, roster_ref) = roster(&conn, &secret);
        let signed = content(&secret, account, roster_ref, u64::MAX, Some([0xaa; 32]));
        assert_eq!(
            content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
            ContentIngestOutcome::Ingested { status: "parked{missing_predecessor}".into() }
        );
        assert_eq!(
            conn.query_row("SELECT seq FROM content_entries", [], |row| row.get::<_, Vec<u8>>(0))
                .unwrap(),
            u64::MAX.to_be_bytes()
        );
    }

    #[test]
    fn terminal_promotion_capacity_is_reported_and_clears_authenticated_queue_work() {
        let mut conn = db();
        let secret = DeviceSecret::from_seed(&[12; 32]);
        let (account, roster) = signed_roster(&secret);
        let signed = content(&secret, account, roster.entry_hash, 0, None);
        assert_eq!(
            content_ingest(&conn, &signed.signed_bytes, 1).unwrap(),
            ContentIngestOutcome::PreVerify
        );
        let tx = conn.transaction().unwrap();
        for index in 0..CANDIDATES_PER_AUTHOR_MAX {
            let mut hash = [0_u8; 32];
            hash[24..].copy_from_slice(&(index as u64).to_be_bytes());
            tx.execute(
                "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, 0, ?9, 0)",
                params![
                    hash.as_slice(),
                    [1_u8; 32].as_slice(),
                    account.to_bytes().as_slice(),
                    [2_u8; 32].as_slice(),
                    (index as u64).to_be_bytes().as_slice(),
                    [3_u8; 32].as_slice(),
                    [0_u8; 8].as_slice(),
                    [0_u8; 8].as_slice(),
                    [0_u8],
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let outcome =
            super::super::super::storage::account_ingest(&conn, &roster.signed_bytes, 2).unwrap();
        assert_eq!(
            outcome,
            super::super::super::storage::IngestOutcome::IngestedWithRejectedContentPromotions {
                status: "accepted".into(),
                scope: ContentCapacityScope::CandidateAuthor,
                entry_hashes: vec![signed.entry_hash],
            }
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM content_pre_verify", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM content_entries WHERE entry_hash = ?1",
                [signed.entry_hash.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn candidate_admission_reports_author_and_global_count_and_byte_scopes() {
        let secret = DeviceSecret::from_seed(&[16; 32]);

        let author_count = db();
        let (account, roster_ref) = roster(&author_count, &secret);
        let signed = content(&secret, account, roster_ref, 0, None);
        let verified =
            envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
        seed_content_candidates(
            &author_count,
            account,
            FOREIGN_FP,
            1,
            CANDIDATES_PER_AUTHOR_MAX as usize,
            1,
        );
        let tx = author_count.unchecked_transaction().unwrap();
        assert_eq!(
            candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
            Some(ContentCapacityScope::CandidateAuthor)
        );

        let author_bytes = db();
        let (account, roster_ref) = roster(&author_bytes, &secret);
        let signed = content(&secret, account, roster_ref, 0, None);
        let verified =
            envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
        seed_content_candidates(
            &author_bytes,
            account,
            FOREIGN_FP,
            2,
            1,
            CANDIDATE_BYTES_PER_AUTHOR_MAX as usize,
        );
        let tx = author_bytes.unchecked_transaction().unwrap();
        assert_eq!(
            candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
            Some(ContentCapacityScope::CandidateAuthorBytes)
        );

        let global_count = db();
        for author in 0..4_u8 {
            seed_content_candidates(
                &global_count,
                super::super::super::AccountId::from_bytes([author; 32]),
                FOREIGN_FP,
                10 + u64::from(author),
                CANDIDATES_PER_AUTHOR_MAX as usize,
                1,
            );
        }
        let (account, roster_ref) = roster(&global_count, &secret);
        let signed = content(&secret, account, roster_ref, 0, None);
        let verified =
            envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
        let tx = global_count.unchecked_transaction().unwrap();
        assert_eq!(
            candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
            Some(ContentCapacityScope::CandidateGlobal)
        );

        let global_bytes = db();
        for author in 0..5_u8 {
            seed_content_candidates(
                &global_bytes,
                super::super::super::AccountId::from_bytes([author; 32]),
                FOREIGN_FP,
                20 + u64::from(author),
                1,
                13 * 1024 * 1024,
            );
        }
        let (account, roster_ref) = roster(&global_bytes, &secret);
        let signed = content(&secret, account, roster_ref, 0, None);
        let verified =
            envelope::verify_content_signed(&signed.signed_bytes, &secret.public()).unwrap();
        let tx = global_bytes.unchecked_transaction().unwrap();
        assert_eq!(
            candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
            Some(ContentCapacityScope::CandidateGlobalBytes)
        );
    }

    #[test]
    fn global_pre_verify_evicts_oldest_and_exact_candidate_replay_bypasses_capacity() {
        let conn = db();
        for ordinal in 0..PRE_VERIFY_GLOBAL_MAX {
            let signed_hash = cbor::sha256(&ordinal.to_be_bytes());
            conn.execute(
                "INSERT INTO content_pre_verify(
                     signed_hash, entry_hash, claimed_stream_id, claimed_author_account_id,
                     claimed_fingerprint, roster_ref, raw_bytes, received_at_ms)
                 VALUES(?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    signed_hash.as_slice(),
                    [1_u8; 32].as_slice(),
                    [ordinal as u8; 32].as_slice(),
                    [2_u8; 32].as_slice(),
                    [3_u8; 32].as_slice(),
                    [0_u8],
                    ordinal,
                ],
            )
            .unwrap();
        }
        let oldest = cbor::sha256(&0_i64.to_be_bytes());
        let secret = DeviceSecret::from_seed(&[17; 32]);
        let unknown_account = super::super::super::AccountId::from_bytes([0xf1; 32]);
        let parked = content(&secret, unknown_account, [0xf2; 32], 0, None);
        assert_eq!(
            content_ingest(&conn, &parked.signed_bytes, PRE_VERIFY_GLOBAL_MAX + 1).unwrap(),
            ContentIngestOutcome::PreVerifyWithEviction {
                scopes: vec![ContentCapacityScope::PreVerifyGlobal]
            }
        );
        assert!(!pre_verify_contains(&conn, &oldest).unwrap());

        let replay = db();
        let (account, roster_ref) = roster(&replay, &secret);
        let signed = content(&secret, account, roster_ref, 0, None);
        let expected = content_ingest(&replay, &signed.signed_bytes, 1).unwrap();
        seed_content_candidates(
            &replay,
            account,
            FOREIGN_FP,
            99,
            CANDIDATES_PER_AUTHOR_MAX as usize - 1,
            1,
        );
        assert_eq!(content_ingest(&replay, &signed.signed_bytes, 2).unwrap(), expected);
        assert_eq!(
            replay
                .query_row(
                    "SELECT count(*) FROM content_entries WHERE author_account_id = ?1",
                    [account.to_bytes().as_slice()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            CANDIDATES_PER_AUTHOR_MAX
        );
    }

    // ---- C3.3: the acceptance refold wired into ingest ----

    const STREAM: [u8; 32] = [0x44; 32];

    /// The parts of a content entry that vary per test. Defaults are the common case: a seq-0 chain
    /// root, no grant, `auth_len` 0 (freshness `CurrentOrBehind` — the frozen `content` helper pins
    /// it at `u64::MAX`, always `Ahead`, which would park every accept path), body `0xf6`.
    #[derive(Clone, Copy)]
    struct ContentSpec {
        grant_id: Option<EntryHash>,
        seq: u64,
        previous: Option<EntryHash>,
        auth_len: u64,
        body: u8,
    }

    impl Default for ContentSpec {
        fn default() -> Self {
            Self { grant_id: None, seq: 0, previous: None, auth_len: 0, body: 0xf6 }
        }
    }

    /// Sign a content entry. `roster_ref` must name the author's real account genesis so the
    /// signing device resolves (see [`resolve_roster_key`]).
    fn authored(
        secret: &DeviceSecret,
        author: AccountId,
        roster_ref: EntryHash,
        spec: ContentSpec,
    ) -> SignedContentEntry {
        let header = ContentEntryHeader {
            stream_id: StreamId::from_bytes(STREAM),
            author_account_id: author,
            device_fingerprint: secret.public().fingerprint(),
            seq: spec.seq,
            lamport: spec.seq.saturating_add(1),
            prev_hash: spec.previous,
            grant_id: spec.grant_id,
            roster_ref,
            owner_auth_len: spec.auth_len,
            author_auth_len: spec.auth_len,
            crypto_suite: 0,
            key_id: None,
        };
        envelope::sign_content_entry(secret, &header, &[spec.body]).unwrap()
    }

    /// Sign a content entry carrying a real memory-op body, so the accepted-`/3` projection has
    /// something to fold — the `authored` helper's single-byte body never decodes to an op.
    fn authored_op(
        secret: &DeviceSecret,
        author: AccountId,
        roster_ref: EntryHash,
        spec: ContentSpec,
        memory_op: &crate::op::MemoryOp,
    ) -> SignedContentEntry {
        let header = ContentEntryHeader {
            stream_id: StreamId::from_bytes(STREAM),
            author_account_id: author,
            device_fingerprint: secret.public().fingerprint(),
            seq: spec.seq,
            lamport: spec.seq.saturating_add(1),
            prev_hash: spec.previous,
            grant_id: spec.grant_id,
            roster_ref,
            owner_auth_len: spec.auth_len,
            author_auth_len: spec.auth_len,
            crypto_suite: 0,
            key_id: None,
        };
        envelope::sign_content_entry(secret, &header, &crate::op::encode(memory_op)).unwrap()
    }

    fn node_create(id: &str) -> crate::op::MemoryOp {
        crate::op::MemoryOp::NodeCreate {
            node_id: crate::op::NodeId::from(id),
            content: crate::op::NodeContent {
                kind: "Invariant".to_string(),
                title: id.to_string(),
                body: "body".to_string(),
                confidence: "high".to_string(),
                source: "agent".to_string(),
                tags: Vec::new(),
                payload: None,
            },
        }
    }

    /// The node ids the accepted-`/3` projection holds for [`STREAM`], sorted.
    fn projected_node_ids(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT node_id FROM content_projected_nodes WHERE stream_id = ?1 ORDER BY node_id",
            )
            .unwrap();
        stmt.query_map([STREAM.as_slice()], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn seed_ownership(conn: &Connection, owner: AccountId) {
        conn.execute(
            "INSERT INTO account_stream_ownership(stream_id, account_id, own_id, effective_at)
             VALUES(?1, ?2, ?3, 1)",
            params![STREAM.as_slice(), owner.to_bytes().as_slice(), [0x66_u8; 32].as_slice()],
        )
        .unwrap();
    }

    fn seed_roster_fact(
        conn: &Connection,
        roster_ref: EntryHash,
        account: AccountId,
        device: &DeviceSecret,
        role: &str,
    ) {
        conn.execute(
            "INSERT INTO account_roster_history(
                 roster_ref, account_id, device_fingerprint, role, effective_at, closed_at)
             VALUES(?1, ?2, ?3, ?4, 1, NULL)",
            params![
                roster_ref.as_slice(),
                account.to_bytes().as_slice(),
                device.public().fingerprint().to_bytes().as_slice(),
                role,
            ],
        )
        .unwrap();
    }

    fn seed_roster_content_cut(
        conn: &Connection,
        roster_ref: EntryHash,
        account: AccountId,
        seq: u64,
        watermark: [u8; 32],
    ) {
        conn.execute(
            "INSERT INTO account_roster_content_boundaries(
                 roster_ref, account_id, stream_id, seq, entry_hash)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                roster_ref.as_slice(),
                account.to_bytes().as_slice(),
                STREAM.as_slice(),
                seq.to_be_bytes().as_slice(),
                watermark.as_slice(),
            ],
        )
        .unwrap();
    }

    fn seed_grant(
        conn: &Connection,
        grant_id: EntryHash,
        owner: AccountId,
        grantee: AccountId,
        role: &str,
    ) {
        conn.execute(
            "INSERT INTO account_stream_grants(
                 grant_id, owner_account_id, stream_id, grantee_account_id, role,
                 effective_at, closed_at)
             VALUES(?1, ?2, ?3, ?4, ?5, 1, NULL)",
            params![
                grant_id.as_slice(),
                owner.to_bytes().as_slice(),
                STREAM.as_slice(),
                grantee.to_bytes().as_slice(),
                role,
            ],
        )
        .unwrap();
    }

    /// A revoked (closed) grant plus a device cut, so `grant_effective_for_device_in_snapshot`
    /// resolves it to a `Cut` boundary — the shape whose watermark could be misused as a branch
    /// pin.
    fn seed_closed_grant_with_cut(
        conn: &Connection,
        grant_id: EntryHash,
        owner: AccountId,
        grantee: AccountId,
        role: &str,
        device: &DeviceSecret,
        watermark: [u8; 32],
    ) {
        conn.execute(
            "INSERT INTO account_stream_grants(
                 grant_id, owner_account_id, stream_id, grantee_account_id, role,
                 effective_at, closed_at)
             VALUES(?1, ?2, ?3, ?4, ?5, 1, 2)",
            params![
                grant_id.as_slice(),
                owner.to_bytes().as_slice(),
                STREAM.as_slice(),
                grantee.to_bytes().as_slice(),
                role,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO account_stream_grant_cuts(
                 grant_id, owner_account_id, device_fingerprint, seq, entry_hash)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                grant_id.as_slice(),
                owner.to_bytes().as_slice(),
                device.public().fingerprint().to_bytes().as_slice(),
                0_u64.to_be_bytes().as_slice(),
                watermark.as_slice(),
            ],
        )
        .unwrap();
    }

    fn seed_contested(conn: &Connection, account: AccountId) {
        conn.execute(
            "INSERT INTO account_auth_state(
                 account_id, classification, contested_depth, successor_account_id, \
             effective_count)
             VALUES(?1, 'contested', 1, NULL, 3)",
            [account.to_bytes().as_slice()],
        )
        .unwrap();
    }

    fn verdict(conn: &Connection, entry_hash: &EntryHash) -> (String, i64) {
        conn.query_row(
            "SELECT s.status, e.accepted FROM content_entries e
             JOIN content_entry_status s ON s.entry_hash = e.entry_hash
             WHERE e.entry_hash = ?1",
            [entry_hash.as_slice()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap()
    }

    /// Ingest then SETTLE, and read the resulting acceptance verdict. `content_ingest` now defers
    /// the acceptance fold (#652), so a test that wants the folded verdict must settle first —
    /// this helper is the "ingest and observe acceptance" shorthand the acceptance tests are
    /// written against.
    fn verdict_after_ingest(conn: &Connection, entry: &SignedContentEntry) -> (String, i64) {
        content_ingest(conn, &entry.signed_bytes, 1).unwrap();
        settle_all(conn);
        verdict(conn, &entry.entry_hash)
    }

    #[test]
    fn an_owner_authored_entry_accepts_when_authority_and_branch_are_clear() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x21; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        let entry = authored(&secret, owner, genesis, ContentSpec::default());
        // Ingest DEFERS the acceptance fold (#652), so it returns the STRUCTURAL status; the
        // acceptance verdict appears once the stream is settled.
        assert_eq!(
            content_ingest(&conn, &entry.signed_bytes, 1).unwrap(),
            ContentIngestOutcome::Ingested { status: "retained_unfolded".into() },
        );
        assert_eq!(settle_all(&conn).settled_streams, 1);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
    }

    #[test]
    fn a_contributor_with_a_writer_grant_accepts() {
        let conn = db();
        let owner_secret = DeviceSecret::from_seed(&[0x31; 32]);
        let author_secret = DeviceSecret::from_seed(&[0x32; 32]);
        let owner = roster(&conn, &owner_secret).0;
        let (author, author_genesis) = roster(&conn, &author_secret);
        let grant_id = [0x67; 32];
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, author_genesis, author, &author_secret, "member");
        seed_grant(&conn, grant_id, owner, author, "writer");

        let entry = authored(&author_secret, author, author_genesis, ContentSpec {
            grant_id: Some(grant_id),
            ..ContentSpec::default()
        });
        assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));
    }

    #[test]
    fn a_contributor_whose_grant_only_reads_is_rejected() {
        let conn = db();
        let owner_secret = DeviceSecret::from_seed(&[0x41; 32]);
        let author_secret = DeviceSecret::from_seed(&[0x42; 32]);
        let owner = roster(&conn, &owner_secret).0;
        let (author, author_genesis) = roster(&conn, &author_secret);
        let grant_id = [0x68; 32];
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, author_genesis, author, &author_secret, "member");
        seed_grant(&conn, grant_id, owner, author, "reader");

        let entry = authored(&author_secret, author, author_genesis, ContentSpec {
            grant_id: Some(grant_id),
            ..ContentSpec::default()
        });
        assert_eq!(verdict_after_ingest(&conn, &entry), ("rejected{grant_not_writer}".into(), 0));
    }

    #[test]
    fn an_equivocating_fork_accepts_the_smaller_hash_and_forks_the_loser() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x51; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // Two seq-0 entries on one coordinate — an equivocation. Both are authority-eligible, so
        // branch selection resolves the unforced fork by the smaller entry_hash; the loser is
        // terminal `forked`, never accepted.
        let a = authored(&secret, owner, genesis, ContentSpec::default());
        let b =
            authored(&secret, owner, genesis, ContentSpec { body: 0xf7, ..ContentSpec::default() });
        content_ingest(&conn, &a.signed_bytes, 1).unwrap();
        content_ingest(&conn, &b.signed_bytes, 2).unwrap();
        settle_all(&conn);

        let (winner, loser) = if a.entry_hash < b.entry_hash { (&a, &b) } else { (&b, &a) };
        assert_eq!(verdict(&conn, &winner.entry_hash), ("accepted".into(), 1));
        assert_eq!(verdict(&conn, &loser.entry_hash), ("forked".into(), 0));
    }

    #[test]
    fn a_register_cut_condemns_an_entry_beyond_its_bound_watermark() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x61; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // A real seq-0 entry, then a roster content cut bounding this coordinate AT that held
        // watermark. seq 0 is on the cut (accepted); seq 1 is beyond it → condemned.
        let s0 = authored(&secret, owner, genesis, ContentSpec::default());
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        seed_roster_content_cut(&conn, genesis, owner, 0, s0.entry_hash);
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        assert_eq!(verdict_after_ingest(&conn, &s1), ("condemned{beyond_cut}".into(), 0));
        assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));
    }

    #[test]
    fn a_beyond_cut_entry_is_condemned_even_when_the_watermark_is_withheld() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x62; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");
        // A cut bounds this coordinate at seq 3, but its watermark has not synced. I11: the `[seq]`
        // condemns a beyond-cut entry from seq alone even while the watermark is withheld — a
        // withheld watermark must NOT launder a back-dated forgery into a park (the divergence this
        // guards against; parity with the account fold's P10 beyond-cut verdict).
        seed_roster_content_cut(&conn, genesis, owner, 3, [0xcc; 32]);

        let entry = authored(&secret, owner, genesis, ContentSpec {
            seq: 5,
            previous: Some([0xaa; 32]),
            ..ContentSpec::default()
        });
        assert_eq!(verdict_after_ingest(&conn, &entry), ("condemned{beyond_cut}".into(), 0));
    }

    #[test]
    fn an_under_cut_entry_parks_while_the_watermark_is_withheld() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x64; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");
        // The same withheld cut at seq 3, but the entry is UNDER the cut (seq 0). Its on/off-branch
        // placement can't be decided until the watermark syncs, so it PARKS as `unknown_cut_target`
        // — never silently accepted, never condemned (I11: a withheld watermark never flips a
        // verdict). This is the correct under-cut park the beyond-cut fix must preserve.
        seed_roster_content_cut(&conn, genesis, owner, 3, [0xcc; 32]);

        let entry = authored(&secret, owner, genesis, ContentSpec::default());
        assert_eq!(verdict_after_ingest(&conn, &entry), ("parked{unknown_cut_target}".into(), 0));
    }

    #[test]
    fn a_cut_whose_watermark_names_a_foreign_coordinate_is_ignored() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x63; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // A dense chain seq 0 → 1, both accepted. Then a cut claims to bound seq 0 but names the
        // seq-1 entry as its watermark — a coordinate/seq mismatch. A malformed cut must not
        // condemn honest content (the §11.3 laundering guard); both stay accepted.
        let s0 = authored(&secret, owner, genesis, ContentSpec::default());
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
        seed_roster_content_cut(&conn, genesis, owner, 0, s1.entry_hash);
        run_account_trigger(&conn, owner);

        assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));
        assert_eq!(verdict(&conn, &s1.entry_hash), ("accepted".into(), 1));
    }

    #[test]
    fn a_contested_author_parks_as_contested_subject() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x71; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");
        seed_contested(&conn, owner);

        let entry = authored(&secret, owner, genesis, ContentSpec::default());
        assert_eq!(verdict_after_ingest(&conn, &entry), ("parked{contested_subject}".into(), 0));
    }

    #[test]
    fn an_author_ahead_of_our_fold_parks_for_refetch_not_rejects() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x81; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // The author cites a control-fold length we have not reached (our effective_count is 0).
        // Freshness is the LAST axis (§13): with authority and branch otherwise clear, this parks
        // for refetch rather than hardening into a rejection we would later walk back.
        let entry = authored(&secret, owner, genesis, ContentSpec {
            auth_len: 9,
            ..ContentSpec::default()
        });
        assert_eq!(verdict_after_ingest(&conn, &entry), ("parked{auth_len_ahead}".into(), 0));
    }

    /// Run the account→content trigger the way an account fold does — one IMMEDIATE txn.
    fn run_account_trigger(conn: &Connection, account: AccountId) {
        run_account_trigger_owning(conn, account, &[]);
    }

    /// The trigger with an explicit pre-rewrite owned-stream set (what the account fold captures
    /// before it rewrites the projection).
    fn run_account_trigger_owning(
        conn: &Connection,
        account: AccountId,
        previously_owned: &[[u8; 32]],
    ) {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let streams = affected_streams_for_account(&tx, account, previously_owned).unwrap();
        finalize_affected_streams(&tx, &streams).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn content_that_arrives_before_its_owner_fact_is_classified_when_the_trigger_runs() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x91; 32]);
        let (owner, genesis) = roster(&conn, &secret);

        // No `StreamOwn` fact yet: the ingest-time refold cannot evaluate authority, so the entry
        // keeps its structural `retained_unfolded` status rather than being wrongly
        // parked/rejected.
        let entry = authored(&secret, owner, genesis, ContentSpec::default());
        assert_eq!(verdict_after_ingest(&conn, &entry), ("retained_unfolded".into(), 0));

        // The owner's ownership + roster facts fold; the account→content trigger reclassifies the
        // stream in that account-refold txn, and the entry reaches its real verdict.
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");
        run_account_trigger(&conn, owner);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
    }

    #[test]
    fn a_cut_folding_later_retro_condemns_already_accepted_content() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xa1; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // A dense chain seq 0 → 1, both accepted once the ingests settle.
        let s0 = authored(&secret, owner, genesis, ContentSpec::default());
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
        settle_all(&conn);
        assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));
        assert_eq!(verdict(&conn, &s1.entry_hash), ("accepted".into(), 1));

        // A revocation bounds the coordinate at seq 0 (watermark = s0). On the next account fold
        // the trigger retro-condemns seq 1 (beyond the cut) while seq 0 stays accepted —
        // the revocation takes effect without any new content arriving (L2 enforceable).
        seed_roster_content_cut(&conn, genesis, owner, 0, s0.entry_hash);
        run_account_trigger(&conn, owner);
        assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));
        assert_eq!(verdict(&conn, &s1.entry_hash), ("condemned{beyond_cut}".into(), 0));
    }

    #[test]
    fn losing_ownership_declassifies_previously_accepted_contributor_content() {
        let conn = db();
        let owner_secret = DeviceSecret::from_seed(&[0xb1; 32]);
        let author_secret = DeviceSecret::from_seed(&[0xb2; 32]);
        let owner = roster(&conn, &owner_secret).0;
        let (author, author_genesis) = roster(&conn, &author_secret);
        let grant_id = [0x69; 32];
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, author_genesis, author, &author_secret, "member");
        seed_grant(&conn, grant_id, owner, author, "writer");

        // A contributor's entry accepts. The owner neither authored it nor (after the next step)
        // owns the stream, so only the pre-rewrite owned set can rediscover it.
        let entry = authored(&author_secret, author, author_genesis, ContentSpec {
            grant_id: Some(grant_id),
            ..ContentSpec::default()
        });
        assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));

        // The owner's `StreamOwn` fact is dropped (owner contested / branch reselection).
        conn.execute("DELETE FROM account_stream_ownership WHERE account_id = ?1", [owner
            .to_bytes()
            .as_slice()])
            .unwrap();

        // Without the pre-rewrite owned set the orphaned stream is invisible to the trigger (the
        // owner no longer owns it and never authored it), so the stale acceptance would survive —
        // exactly the hole the captured set closes.
        run_account_trigger_owning(&conn, owner, &[]);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));

        // With it, the stream is refolded, finds no owner, and declassifies the entry.
        run_account_trigger_owning(&conn, owner, &[StreamId::from_bytes(STREAM).to_bytes()]);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("retained_unfolded".into(), 0));
    }

    #[test]
    fn a_contested_owner_parks_contributor_content_even_when_the_author_is_live() {
        let conn = db();
        let owner_secret = DeviceSecret::from_seed(&[0xc1; 32]);
        let author_secret = DeviceSecret::from_seed(&[0xc2; 32]);
        let owner = roster(&conn, &owner_secret).0;
        let (author, author_genesis) = roster(&conn, &author_secret);
        let grant_id = [0x6a; 32];
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, author_genesis, author, &author_secret, "member");
        seed_grant(&conn, grant_id, owner, author, "writer");
        // The OWNER is contested while the contributor stays live. The writer grant lives in the
        // owner's log, so a compromised owner poisons it: the content must fail closed.
        seed_contested(&conn, owner);

        let entry = authored(&author_secret, author, author_genesis, ContentSpec {
            grant_id: Some(grant_id),
            ..ContentSpec::default()
        });
        assert_eq!(verdict_after_ingest(&conn, &entry), ("parked{contested_subject}".into(), 0));
    }

    #[test]
    fn a_candidate_whose_stored_bytes_go_corrupt_is_declassified_not_left_accepted() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xd1; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        let entry = authored(&secret, owner, genesis, ContentSpec::default());
        assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));

        // The stored envelope is corrupted (a torn write / bad blob). The next refold cannot decode
        // it, so it must lose `accepted` and its status — a candidate with no readable authority
        // basis must never stay live.
        conn.execute(
            "UPDATE content_entries SET signed_bytes = ?1 WHERE entry_hash = ?2",
            params![[0_u8].as_slice(), entry.entry_hash.as_slice(),],
        )
        .unwrap();
        run_account_trigger(&conn, owner);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("retained_unfolded".into(), 0));
    }

    #[test]
    fn an_orphaned_stream_whose_only_candidate_is_corrupt_still_loses_acceptance() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xe1; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        let entry = authored(&secret, owner, genesis, ContentSpec::default());
        assert_eq!(verdict_after_ingest(&conn, &entry), ("accepted".into(), 1));

        // Worst case: ownership disappears AND the sole stored envelope is corrupt, so the refold
        // resolves no owner and decodes no headers. The declassify path must STILL clear acceptance
        // — an empty header list must not early-return past the `accepted = 0` clear.
        conn.execute("DELETE FROM account_stream_ownership WHERE account_id = ?1", [owner
            .to_bytes()
            .as_slice()])
            .unwrap();
        conn.execute(
            "UPDATE content_entries SET signed_bytes = ?1 WHERE entry_hash = ?2",
            params![[0_u8].as_slice(), entry.entry_hash.as_slice()],
        )
        .unwrap();
        run_account_trigger_owning(&conn, owner, &[StreamId::from_bytes(STREAM).to_bytes()]);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("retained_unfolded".into(), 0));
    }

    #[test]
    fn a_row_whose_blob_was_swapped_for_a_different_valid_envelope_is_not_classified() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xf1; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        let a = authored(&secret, owner, genesis, ContentSpec::default());
        assert_eq!(verdict_after_ingest(&conn, &a), ("accepted".into(), 1));

        // Replace A's stored blob with a DIFFERENT (still valid) envelope, under A's row key. The
        // refold must not classify A's row under B's header just because B decodes — the decoded
        // `entry_hash` no longer matches the key, so the row is treated as absent and declassified.
        let b =
            authored(&secret, owner, genesis, ContentSpec { body: 0xf7, ..ContentSpec::default() });
        assert_ne!(a.entry_hash, b.entry_hash);
        conn.execute(
            "UPDATE content_entries SET signed_bytes = ?1 WHERE entry_hash = ?2",
            params![b.signed_bytes.as_slice(), a.entry_hash.as_slice()],
        )
        .unwrap();
        run_account_trigger(&conn, owner);
        assert_eq!(verdict(&conn, &a.entry_hash), ("retained_unfolded".into(), 0));
    }

    #[test]
    fn a_descendant_of_a_freshness_parked_entry_does_not_accept_out_of_prefix() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xd2; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // seq 0 cites a control-fold length ahead of ours (parks on freshness); seq 1 cites a
        // current length. An attacker varies `auth_len` DOWN the chain to try to slip seq 1 in as
        // accepted over a parked seq 0. The accepted set must stay a prefix from seq 0, so neither
        // accepts while seq 0 is parked.
        let s0 = authored(&secret, owner, genesis, ContentSpec {
            auth_len: 9,
            ..ContentSpec::default()
        });
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            auth_len: 0,
            ..ContentSpec::default()
        });
        content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
        settle_all(&conn);

        assert_eq!(verdict(&conn, &s0.entry_hash), ("parked{auth_len_ahead}".into(), 0));
        assert_eq!(verdict(&conn, &s1.entry_hash), ("parked{auth_len_ahead}".into(), 0));
    }

    #[test]
    fn an_eligible_child_of_an_ineligible_parent_parks_rather_than_forks() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xd3; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // seq 0 is the owner citing a grant it must not have → rejected (ineligible). seq 1 is a
        // clean owner entry building on it: authority-eligible, but with no accepted parent to
        // extend. It is stranded, not a contest loser, so it parks (recoverable) — never `forked`.
        let s0 = authored(&secret, owner, genesis, ContentSpec {
            grant_id: Some([0x6b; 32]),
            ..ContentSpec::default()
        });
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
        settle_all(&conn);

        assert_eq!(verdict(&conn, &s0.entry_hash), ("rejected{unexpected_grant}".into(), 0));
        assert_eq!(verdict(&conn, &s1.entry_hash), ("parked{missing_predecessor}".into(), 0));
    }

    #[test]
    fn the_account_trigger_is_a_noop_before_the_content_tables_exist() {
        // Mid-migration: the V064/V065 authority backfill folds every existing account before the
        // `/3` tables are created. The account→content trigger must not query `content_entries`
        // (which does not exist yet) — otherwise a populated DB fails to upgrade.
        let conn = Connection::open_in_memory().unwrap();
        let owner = signed_roster(&DeviceSecret::from_seed(&[0xe2; 32])).0;
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let streams = affected_streams_for_account(&tx, owner, &[]).unwrap();
        finalize_affected_streams(&tx, &streams).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn the_account_trigger_skips_the_reproject_before_the_projected_tables_exist() {
        // The V064/V065 authority backfill can also fold accounts AFTER the `/3` candidate tables
        // exist but BEFORE V070 creates `content_projected_*`: a content refold runs (so
        // `content_entries_exists` passes) but the reproject targets absent tables. Simulate that
        // window by dropping the V070 tables; the trigger must still refold cleanly.
        let conn = db();
        conn.execute_batch(
            "DROP TABLE content_projected_nodes; DROP TABLE content_projected_edges;",
        )
        .unwrap();
        let secret = DeviceSecret::from_seed(&[0xe3; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        let entry = authored(&secret, owner, genesis, ContentSpec::default());
        content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
        run_account_trigger(&conn, owner);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
    }

    #[test]
    fn an_account_fold_that_retro_condemns_content_drops_it_from_the_projection() {
        // #683: the account→content trigger re-derives `accepted` — and a revocation can FLIP it.
        // The reconcile's anti-join trusts `content_projected_*` to mirror `accepted`, so the
        // trigger must reproject every stream it refolds.
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xe4; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // A dense chain seq 0 → 1 carrying real ops; both accept and project on settle.
        let s0 =
            authored_op(&secret, owner, genesis, ContentSpec::default(), &node_create("mem_a"));
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        let s1 = authored_op(
            &secret,
            owner,
            genesis,
            ContentSpec { seq: 1, previous: Some(s0.entry_hash), ..ContentSpec::default() },
            &node_create("mem_b"),
        );
        content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
        settle_all(&conn);
        assert_eq!(verdict(&conn, &s1.entry_hash), ("accepted".into(), 1));
        assert_eq!(projected_node_ids(&conn), vec!["mem_a".to_string(), "mem_b".to_string()]);

        // A revocation bounds the coordinate at seq 0: the account fold retro-condemns seq 1, and
        // the same txn must drop its node from the projection — a stale row here would make the
        // reconcile's anti-join treat mem_b as still authored.
        seed_roster_content_cut(&conn, genesis, owner, 0, s0.entry_hash);
        run_account_trigger(&conn, owner);
        assert_eq!(verdict(&conn, &s1.entry_hash), ("condemned{beyond_cut}".into(), 0));
        assert_eq!(
            projected_node_ids(&conn),
            vec!["mem_a".to_string()],
            "the retro-condemned entry's node leaves the projection in the refold txn",
        );
    }

    #[test]
    fn an_account_fold_that_accepts_parked_content_projects_it() {
        // #683, the other direction: content arrives BEFORE its owner fact, so it cannot accept
        // yet; when the authority facts fold, the trigger flips it to accepted — and the node
        // must APPEAR in the projection in the same txn, or the reconcile would re-author it.
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xe5; 32]);
        let (owner, genesis) = roster(&conn, &secret);

        let entry =
            authored_op(&secret, owner, genesis, ContentSpec::default(), &node_create("mem_a"));
        assert_eq!(verdict_after_ingest(&conn, &entry), ("retained_unfolded".into(), 0));
        assert_eq!(projected_node_ids(&conn), Vec::<String>::new());

        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");
        run_account_trigger(&conn, owner);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
        assert_eq!(
            projected_node_ids(&conn),
            vec!["mem_a".to_string()],
            "the newly-accepted entry's node enters the projection in the refold txn",
        );
    }

    #[test]
    fn a_reader_grants_revoke_cut_cannot_steer_writer_content_branch_selection() {
        let conn = db();
        let owner_secret = DeviceSecret::from_seed(&[0xf3; 32]);
        let author_secret = DeviceSecret::from_seed(&[0xf4; 32]);
        let owner = roster(&conn, &owner_secret).0;
        let (author, author_genesis) = roster(&conn, &author_secret);
        let writer_grant = [0x71; 32];
        let reader_grant = [0x72; 32];
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, author_genesis, author, &author_secret, "member");
        seed_grant(&conn, writer_grant, owner, author, "writer");

        // Two writer entries at seq 0 (equivocation): the smaller entry_hash wins the unforced
        // fork.
        let a = authored(&author_secret, author, author_genesis, ContentSpec {
            grant_id: Some(writer_grant),
            body: 0xf6,
            ..ContentSpec::default()
        });
        let b = authored(&author_secret, author, author_genesis, ContentSpec {
            grant_id: Some(writer_grant),
            body: 0xf7,
            ..ContentSpec::default()
        });
        content_ingest(&conn, &a.signed_bytes, 1).unwrap();
        content_ingest(&conn, &b.signed_bytes, 2).unwrap();
        let (winner, loser) = if a.entry_hash < b.entry_hash { (&a, &b) } else { (&b, &a) };

        // A revoked READER grant on the same coordinate carries a cut naming the hash-order LOSER.
        // A reader grant never authorizes a content write, so its cut must NOT pin selection — else
        // a peer could hijack the accepted branch by storing a rejected reader-grant entry.
        seed_closed_grant_with_cut(
            &conn,
            reader_grant,
            owner,
            author,
            "reader",
            &author_secret,
            loser.entry_hash,
        );
        let reader_entry = authored(&author_secret, author, author_genesis, ContentSpec {
            grant_id: Some(reader_grant),
            body: 0xf5,
            ..ContentSpec::default()
        });
        content_ingest(&conn, &reader_entry.signed_bytes, 3).unwrap();
        settle_all(&conn);

        assert_eq!(
            verdict(&conn, &reader_entry.entry_hash),
            ("rejected{grant_not_writer}".into(), 0)
        );
        // Hash order still decides — the reader cut did not steer the writers' branch.
        assert_eq!(verdict(&conn, &winner.entry_hash), ("accepted".into(), 1));
        assert_eq!(verdict(&conn, &loser.entry_hash), ("forked".into(), 0));
    }

    // ---- #652: deferred/batch ingest refold + local-vs-global budget ----

    #[derive(Clone, Copy, PartialEq)]
    enum RefoldCadence {
        /// Settle after EVERY ingest — reproduces the pre-#652 per-entry refold cadence.
        PerEntry,
        /// Settle ONCE after the whole batch — the deferred/batch path this change introduces.
        Batch,
    }

    /// Ingest `entries` into a fresh DB seeded by `setup`, folding the deferred refold either after
    /// every entry or once after the batch, and return the full sorted (entry_hash, status,
    /// accepted) set. The acceptance fold is a pure function of the final candidate set, so both
    /// cadences MUST produce a byte-identical result — this is what proves defer/batch changed only
    /// WHEN the fold runs, not its outcome.
    fn drive_ingest(
        entries: &[SignedContentEntry],
        setup: &dyn Fn(&Connection),
        cadence: RefoldCadence,
    ) -> Vec<(Vec<u8>, String, i64)> {
        let conn = db();
        setup(&conn);
        for entry in entries {
            content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
            if cadence == RefoldCadence::PerEntry {
                settle_all(&conn);
            }
        }
        // A trailing settle folds whatever is still deferred: the whole batch under `Batch`, and
        // nothing (an empty-queue no-op) under `PerEntry`.
        settle_all(&conn);
        all_verdicts(&conn)
    }

    fn all_verdicts(conn: &Connection) -> Vec<(Vec<u8>, String, i64)> {
        let mut stmt = conn
            .prepare(
                "SELECT e.entry_hash, s.status, e.accepted
                 FROM content_entries e JOIN content_entry_status s ON s.entry_hash = e.entry_hash
                 ORDER BY e.entry_hash",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    fn pending_refold_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM content_streams_pending_refold", [], |row| row.get(0))
            .unwrap()
    }

    /// Settle with the UNBOUNDED budget — the behavior every pre-budget settle caller relied on
    /// (drain the whole queue in one call). Tests written against that contract use this helper;
    /// the budgeted tests pass an explicit [`ContentRefoldBudget`] instead.
    fn settle_all(conn: &Connection) -> ContentSettleReport {
        settle_pending_content_refolds(conn, &ContentRefoldBudget::unbounded()).unwrap()
    }

    fn pending_refold_state(conn: &Connection) -> (i64, i64, i64) {
        conn.query_row(
            "SELECT reason_mask, first_enqueued_at_ms, last_enqueued_at_ms
             FROM content_streams_pending_refold",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn deferred_settle_matches_per_entry_refold_on_an_honest_chain() {
        let secret = DeviceSecret::from_seed(&[0xa1; 32]);
        let (owner, genesis) = signed_roster(&secret);
        let genesis = genesis.entry_hash;

        // A dense honest chain seq 0 → 1 → 2, all fresh (auth_len 0) → all accept.
        let s0 = authored(&secret, owner, genesis, ContentSpec::default());
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        let s2 = authored(&secret, owner, genesis, ContentSpec {
            seq: 2,
            previous: Some(s1.entry_hash),
            ..ContentSpec::default()
        });
        let entries = [s0, s1, s2];
        let setup = |conn: &Connection| {
            roster(conn, &secret);
            seed_ownership(conn, owner);
            seed_roster_fact(conn, genesis, owner, &secret, "owner");
        };

        let per_entry = drive_ingest(&entries, &setup, RefoldCadence::PerEntry);
        let batch = drive_ingest(&entries, &setup, RefoldCadence::Batch);
        assert_eq!(
            per_entry, batch,
            "deferred batch settle equals per-entry refold, byte for byte"
        );
        // Non-trivial: the fold actually accepted the chain (not two empty sets agreeing).
        assert!(
            batch.iter().all(|(_, status, accepted)| status == "accepted" && *accepted == 1),
            "the honest chain accepts end to end: {batch:?}",
        );
    }

    #[test]
    fn deferred_settle_matches_per_entry_refold_on_an_adversarial_interleaving() {
        let secret = DeviceSecret::from_seed(&[0xa2; 32]);
        let (owner, genesis) = signed_roster(&secret);
        let genesis = genesis.entry_hash;

        // An equivocating fork at seq 0 (siblings a0/b0), plus a seq-1 descendant of a0 that cites
        // a DIFFERENT (ahead) auth_len — the varying-auth_len-down-the-chain attack.
        // Ingested OUT OF ORDER (descendant first, then one sibling, then the other) so the
        // per-entry cadence genuinely folds partial states mid-flight while the batch
        // cadence sees the whole set at once. Both must converge.
        let a0 =
            authored(&secret, owner, genesis, ContentSpec { body: 0xf6, ..ContentSpec::default() });
        let b0 =
            authored(&secret, owner, genesis, ContentSpec { body: 0xf7, ..ContentSpec::default() });
        let child = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(a0.entry_hash),
            auth_len: 9,
            ..ContentSpec::default()
        });
        let entries = [child, b0, a0];
        let setup = |conn: &Connection| {
            roster(conn, &secret);
            seed_ownership(conn, owner);
            seed_roster_fact(conn, genesis, owner, &secret, "owner");
        };

        let per_entry = drive_ingest(&entries, &setup, RefoldCadence::PerEntry);
        let batch = drive_ingest(&entries, &setup, RefoldCadence::Batch);
        assert_eq!(
            per_entry, batch,
            "deferred batch settle equals per-entry refold across an out-of-order fork with \
             varying auth_len",
        );
        // Non-trivial: the fork resolved — one seq-0 sibling accepts, the other forks.
        let statuses: Vec<&str> = batch.iter().map(|(_, status, _)| status.as_str()).collect();
        assert!(statuses.contains(&"accepted"), "a seq-0 sibling accepts: {batch:?}");
        assert!(statuses.contains(&"forked"), "the losing seq-0 sibling forks: {batch:?}");
    }

    #[test]
    fn deferred_settle_matches_per_entry_refold_across_a_cut() {
        let secret = DeviceSecret::from_seed(&[0xa3; 32]);
        let (owner, genesis) = signed_roster(&secret);
        let genesis = genesis.entry_hash;

        // seq 0 sits ON a roster content cut (accepted); seq 1 is BEYOND the bound watermark
        // (condemned). The cut exercises the condemn fold path under both cadences.
        let s0 = authored(&secret, owner, genesis, ContentSpec::default());
        let s0_hash = s0.entry_hash;
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0_hash),
            ..ContentSpec::default()
        });
        let entries = [s0, s1];
        let setup = |conn: &Connection| {
            roster(conn, &secret);
            seed_ownership(conn, owner);
            seed_roster_fact(conn, genesis, owner, &secret, "owner");
            seed_roster_content_cut(conn, genesis, owner, 0, s0_hash);
        };

        let per_entry = drive_ingest(&entries, &setup, RefoldCadence::PerEntry);
        let batch = drive_ingest(&entries, &setup, RefoldCadence::Batch);
        assert_eq!(per_entry, batch, "deferred batch settle equals per-entry refold across a cut");
        let statuses: Vec<&str> = batch.iter().map(|(_, status, _)| status.as_str()).collect();
        assert!(statuses.contains(&"accepted"), "seq 0 on the cut accepts: {batch:?}");
        assert!(
            statuses.iter().any(|status| status.starts_with("condemned")),
            "seq 1 beyond the cut is condemned: {batch:?}",
        );
    }

    #[test]
    fn ingest_defers_all_refolds_and_one_settle_folds_the_dirty_stream_once() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xb1; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        // Authority is present BEFORE ingest, so a refold — if one ran mid-ingest — WOULD accept
        // these entries. Observing them still structural after N ingests is the proof it did not.
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // A dense honest chain. Pre-#652 this ran one whole-stream refold PER entry (O(n) each,
        // O(n^2) cumulative under the writer lock).
        let mut entries = Vec::new();
        let mut previous = None;
        for seq in 0..6_u64 {
            let entry = authored(&secret, owner, genesis, ContentSpec {
                seq,
                previous,
                ..ContentSpec::default()
            });
            previous = Some(entry.entry_hash);
            entries.push(entry);
        }
        for entry in &entries {
            content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
        }

        // BEFORE/AFTER measurement: the N ingests ran ZERO refolds. The refold is the only writer
        // of a non-structural verdict, so every entry sitting at its structural
        // `retained_unfolded` baseline — with authority a refold would have consumed to
        // accept it — proves none ran.
        for entry in &entries {
            assert_eq!(
                verdict(&conn, &entry.entry_hash),
                ("retained_unfolded".into(), 0),
                "no refold runs during ingest — the entry keeps its structural status",
            );
        }
        // N ingests to one stream deduped into ONE queued refold, not N.
        assert_eq!(pending_refold_count(&conn), 1, "N ingests to one stream queue one refold");

        // One settle folds exactly one dirty stream: O(dirty streams), NOT O(entries).
        assert_eq!(settle_all(&conn).settled_streams, 1, "one refold per dirty stream");
        assert_eq!(pending_refold_count(&conn), 0, "settle drains the queue");
        for entry in &entries {
            assert_eq!(
                verdict(&conn, &entry.entry_hash),
                ("accepted".into(), 1),
                "the single settle folds the whole chain to its acceptance verdict",
            );
        }
    }

    #[test]
    fn remote_account_ingests_dedupe_account_change_and_defer_content_until_settle() {
        let conn = db();
        let founder = DeviceSecret::from_seed(&[0xb5; 32]);
        let member_a = DeviceSecret::from_seed(&[0xb6; 32]);
        let member_b = DeviceSecret::from_seed(&[0xb7; 32]);
        let (account, genesis) = roster(&conn, &founder);
        seed_ownership(&conn, account);
        seed_roster_fact(&conn, genesis, account, &founder, "owner");

        let entry = authored(&founder, account, genesis, ContentSpec::default());
        content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
        assert_eq!(settle_all(&conn).settled_streams, 1);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));

        let add_a = signed_device_add_at(&founder, &member_a, account, 1, genesis, genesis, 1);
        super::super::super::storage::account_ingest(&conn, &add_a.signed_bytes, 10).unwrap();
        let add_b =
            signed_device_add_at(&founder, &member_b, account, 2, add_a.entry_hash, genesis, 2);
        super::super::super::storage::account_ingest(&conn, &add_b.signed_bytes, 20).unwrap();

        assert_eq!(
            verdict(&conn, &entry.entry_hash),
            ("accepted".into(), 1),
            "remote account folds leave the last completed content verdict untouched",
        );
        assert_eq!(pending_refold_count(&conn), 1, "N account ingests dedupe per stream");
        assert_eq!(pending_refold_state(&conn), (PENDING_REFOLD_ACCOUNT_CHANGE, 10, 20));

        assert_eq!(settle_all(&conn).settled_streams, 1);
        assert_eq!(pending_refold_count(&conn), 0);
        assert_eq!(
            verdict(&conn, &entry.entry_hash),
            ("retained_unfolded".into(), 0),
            "one settle performs the deferred content fold once",
        );
    }

    #[test]
    fn pending_refold_merges_content_and_account_reasons_without_moving_first_timestamp() {
        let conn = db();
        let founder = DeviceSecret::from_seed(&[0xb8; 32]);
        let member = DeviceSecret::from_seed(&[0xb9; 32]);
        let (account, genesis) = roster(&conn, &founder);
        seed_ownership(&conn, account);
        seed_roster_fact(&conn, genesis, account, &founder, "owner");

        let entry = authored(&founder, account, genesis, ContentSpec::default());
        content_ingest(&conn, &entry.signed_bytes, 5).unwrap();
        assert_eq!(pending_refold_state(&conn), (PENDING_REFOLD_CONTENT_CANDIDATE, 5, 5),);

        let add = signed_device_add(&founder, &member, account, genesis);
        super::super::super::storage::account_ingest(&conn, &add.signed_bytes, 11).unwrap();
        assert_eq!(
            pending_refold_state(&conn),
            (PENDING_REFOLD_CONTENT_CANDIDATE | PENDING_REFOLD_ACCOUNT_CHANGE, 5, 11),
            "reason bits OR, first enqueue stays stable, and last enqueue refreshes",
        );
    }

    #[test]
    fn account_change_settle_reprojects_even_when_acceptance_is_unchanged() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xba; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");
        let entry = authored_op(
            &secret,
            owner,
            genesis,
            ContentSpec::default(),
            &node_create("sealed-later"),
        );
        content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
        settle_all(&conn);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
        assert_eq!(projected_node_ids(&conn), vec!["sealed-later".to_string()]);

        // Model a body that was accepted while locally unprojectable, then became projectable when
        // account-side key material arrived. ACCOUNT_CHANGE must reproject even though acceptance
        // itself remains unchanged.
        conn.execute("DELETE FROM content_projected_nodes", []).unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        queue_account_changed_streams(&tx, &[entry.header.stream_id], 2).unwrap();
        tx.commit().unwrap();

        assert_eq!(settle_all(&conn).settled_streams, 1);
        assert_eq!(verdict(&conn, &entry.entry_hash), ("accepted".into(), 1));
        assert_eq!(projected_node_ids(&conn), vec!["sealed-later".to_string()]);
    }

    #[test]
    fn a_trusted_account_fold_finalizes_and_clears_existing_queue_debt() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xb2; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // Two ingests to one stream leave exactly one queued refold (dedup), and it persists across
        // the separate ingest calls (crash-safety: the mark survives until a fold consumes it).
        let s0 = authored(&secret, owner, genesis, ContentSpec::default());
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        assert_eq!(pending_refold_count(&conn), 1, "the first ingest queues the stream");
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
        assert_eq!(
            pending_refold_count(&conn),
            1,
            "a second ingest dedups onto the same queue row"
        );

        // The trusted account-fold path finalizes the stream in this transaction: it updates
        // `accepted`, reprojects (#683/C5), and clears the already-satisfied queue debt only after
        // both duties succeed.
        run_account_trigger(&conn, owner);
        assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1), "it folds the stream");
        assert_eq!(
            pending_refold_count(&conn),
            0,
            "trusted finalization clears the queue after refold and reproject",
        );

        // The queue is already discharged, so settle has no duplicate work.
        assert_eq!(
            settle_all(&conn).settled_streams,
            0,
            "settle does not repeat trusted finalization",
        );
    }

    #[test]
    fn a_dense_continuation_of_a_settled_chain_is_retained_not_missing_predecessor() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xb4; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // Ingest seq 0 and SETTLE it — its status becomes `accepted`, no longer
        // `retained_unfolded`.
        let s0 = authored(&secret, owner, genesis, ContentSpec::default());
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        assert_eq!(settle_all(&conn).settled_streams, 1);
        assert_eq!(verdict(&conn, &s0.entry_hash), ("accepted".into(), 1));

        // A dense continuation cites the now-SETTLED predecessor. Its STRUCTURAL classification
        // must see the predecessor as present (any status but missing-predecessor), so it
        // lands `retained_unfolded` — NOT wrongly `missing_predecessor` just because s0's
        // status moved off `retained_unfolded` at settle. The RETURNED status is the
        // contract, so assert it directly.
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        assert_eq!(
            content_ingest(&conn, &s1.signed_bytes, 2).unwrap(),
            ContentIngestOutcome::Ingested { status: "retained_unfolded".into() },
            "a dense continuation of a settled chain reports the structural retained_unfolded \
             status",
        );
        assert_eq!(verdict(&conn, &s1.entry_hash), ("retained_unfolded".into(), 0));
        // And it folds to accepted on settle, extending the settled prefix.
        assert_eq!(settle_all(&conn).settled_streams, 1);
        assert_eq!(verdict(&conn, &s1.entry_hash), ("accepted".into(), 1));
    }

    #[test]
    fn settle_folds_each_dirty_stream_independently() {
        let conn = db();
        // Two distinct dirty streams. Each settles in its OWN txn: an ownerless stream declassifies
        // (and clears its mark), exercising the per-stream loop without cross-stream coupling.
        for stream in [[0x51_u8; 32], [0x52_u8; 32]] {
            conn.execute("INSERT INTO content_streams_pending_refold(stream_id) VALUES (?1)", [
                stream.as_slice(),
            ])
            .unwrap();
        }
        assert_eq!(pending_refold_count(&conn), 2);
        assert_eq!(settle_all(&conn).settled_streams, 2, "each dirty stream settles exactly once",);
        assert_eq!(pending_refold_count(&conn), 0, "both marks cleared");
    }

    #[test]
    fn local_device_history_is_excluded_from_the_global_cap_but_a_foreign_flood_still_trips() {
        let foreign_secret = DeviceSecret::from_seed(&[0xc1; 32]);

        // A LOCAL history the size of the whole global ceiling must NOT starve foreign ingest. The
        // exclusion keys on the local DEVICE FINGERPRINT (forge-proof — a row carries it only if
        // signed by the local key), NOT the attacker-settable author_account_id, so seed the
        // ceiling-sized history under the local device's own fingerprint.
        let excluded = db();
        let local_fp = crate::local_device(&excluded, 1).unwrap().fingerprint().to_bytes();
        seed_content_candidates(
            &excluded,
            AccountId::from_bytes([0xd2; 32]),
            local_fp,
            1,
            CANDIDATES_GLOBAL_MAX as usize,
            1,
        );
        let (foreign, foreign_roster) = roster(&excluded, &foreign_secret);
        let signed = content(&foreign_secret, foreign, foreign_roster, 0, None);
        let verified =
            envelope::verify_content_signed(&signed.signed_bytes, &foreign_secret.public())
                .unwrap();
        {
            let tx = excluded.unchecked_transaction().unwrap();
            assert_eq!(
                candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
                None,
                "a global-cap-sized LOCAL-DEVICE history does not trip the foreign global cap",
            );
        }

        // The SAME volume of FOREIGN-signed entries (no local device fingerprint to exclude) still
        // trips the ceiling — the anti-flood budget is intact for genuine remote abuse.
        let flooded = db();
        seed_content_candidates(
            &flooded,
            AccountId::from_bytes([0xd1; 32]),
            FOREIGN_FP,
            1,
            CANDIDATES_GLOBAL_MAX as usize,
            1,
        );
        let (foreign, foreign_roster) = roster(&flooded, &foreign_secret);
        let signed = content(&foreign_secret, foreign, foreign_roster, 0, None);
        let verified =
            envelope::verify_content_signed(&signed.signed_bytes, &foreign_secret.public())
                .unwrap();
        let tx = flooded.unchecked_transaction().unwrap();
        assert_eq!(
            candidate_capacity(&tx, &verified, signed.signed_bytes.len()).unwrap(),
            Some(ContentCapacityScope::CandidateGlobal),
            "a genuine foreign flood past the global cap still trips CandidateGlobal",
        );
    }

    #[test]
    fn settle_on_an_empty_queue_is_a_noop_and_a_second_settle_authors_nothing() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0xb3; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");

        // Empty queue: settle folds nothing and returns 0.
        assert_eq!(settle_all(&conn).settled_streams, 0, "settle on an empty queue is a no-op",);

        // Ingest, then settle drains it; a SECOND settle finds the queue empty and changes nothing.
        let entry = authored(&secret, owner, genesis, ContentSpec::default());
        content_ingest(&conn, &entry.signed_bytes, 1).unwrap();
        assert_eq!(settle_all(&conn).settled_streams, 1, "the first settle folds the dirty stream",);
        let after_first = verdict(&conn, &entry.entry_hash);
        assert_eq!(settle_all(&conn).settled_streams, 0, "the second settle authors nothing new",);
        assert_eq!(
            verdict(&conn, &entry.entry_hash),
            after_first,
            "a redundant settle leaves the verdict unchanged",
        );
    }

    // ---- #698: budgeted, resumable deferred settlement ----

    /// The V079 fold-cost unit for one synthetic candidate row: `length(signed_bytes) + 32`.
    const SYNTHETIC_ROW_BYTES: u64 = 16 + 32;

    /// Seed `count` synthetic candidates for `stream`. The bodies are deliberately NOT decodable
    /// envelopes: the refold treats them as absent rows, so the stream settles through the
    /// ownerless declassify path — enough to exercise queue admission, per-stream txns, and the
    /// stats-trigger accounting without an authority fixture.
    fn seed_synthetic_candidates(conn: &Connection, stream: [u8; 32], count: u64) {
        let first_ordinal = conn
            .query_row(
                "SELECT count(*) FROM content_entries WHERE stream_id = ?1",
                [stream.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap() as u64;
        for ordinal in first_ordinal..first_ordinal + count {
            let entry_hash = cbor::sha256(&[&stream[..], &ordinal.to_be_bytes()[..]].concat());
            conn.execute(
                "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?7, 0, ?8, 1)",
                params![
                    entry_hash.as_slice(),
                    stream.as_slice(),
                    [0xaa_u8; 32].as_slice(),
                    [0xbb_u8; 32].as_slice(),
                    ordinal.to_be_bytes().as_slice(),
                    [0xcc_u8; 32].as_slice(),
                    0_u64.to_be_bytes().as_slice(),
                    vec![0xdd_u8; 16],
                ],
            )
            .unwrap();
        }
    }

    fn enqueue_refold(conn: &Connection, stream: [u8; 32], first_enqueued_at_ms: i64) {
        conn.execute(
            "INSERT INTO content_streams_pending_refold(
                 stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
             VALUES(?1, 1, ?2, ?2)",
            params![stream.as_slice(), first_enqueued_at_ms],
        )
        .unwrap();
    }

    fn queue_contains(conn: &Connection, stream: [u8; 32]) -> bool {
        conn.query_row(
            "SELECT count(*) FROM content_streams_pending_refold WHERE stream_id = ?1",
            [stream.as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    }

    fn stream_enqueued_at(conn: &Connection, stream: [u8; 32]) -> i64 {
        conn.query_row(
            "SELECT first_enqueued_at_ms FROM content_streams_pending_refold WHERE stream_id = ?1",
            [stream.as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn stream_stats(conn: &Connection, stream: [u8; 32]) -> (i64, i64) {
        conn.query_row(
            "SELECT candidate_count, candidate_bytes FROM content_stream_stats
             WHERE stream_id = ?1",
            [stream.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    fn budget(
        max_streams: u64,
        max_candidates: u64,
        max_candidate_bytes: u64,
        allow_one_oversize: bool,
    ) -> ContentRefoldBudget {
        ContentRefoldBudget { max_streams, max_candidates, max_candidate_bytes, allow_one_oversize }
    }

    #[test]
    fn settle_budget_stops_at_the_stream_limit_and_resumes_oldest_first() {
        let conn = db();
        let oldest = [0x11_u8; 32];
        let middle = [0x12_u8; 32];
        let newest = [0x13_u8; 32];
        for (stream, enqueued_at) in [(oldest, 1), (middle, 2), (newest, 3)] {
            seed_synthetic_candidates(&conn, stream, 1);
            enqueue_refold(&conn, stream, enqueued_at);
        }
        let one_stream = budget(1, u64::MAX, u64::MAX, false);

        let first = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        assert_eq!(first.settled_streams, 1);
        assert_eq!(first.consumed_candidates, 1);
        assert_eq!(first.consumed_candidate_bytes, SYNTHETIC_ROW_BYTES);
        assert_eq!(first.deferred_budget, 2, "the rest of the queue fits a fresh budget");
        assert_eq!(first.deferred_oversize, 0);
        assert!(first.failures.is_empty());
        assert!(!first.queue_empty, "two streams remain queued");
        assert!(!queue_contains(&conn, oldest), "the oldest stream settled first");
        assert!(queue_contains(&conn, middle));
        assert!(queue_contains(&conn, newest));

        let second = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        assert_eq!(second.settled_streams, 1);
        assert!(!second.queue_empty, "the resume continues where the budget stopped");
        assert!(!queue_contains(&conn, middle));
        assert!(queue_contains(&conn, newest));

        let third = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        assert_eq!(third.settled_streams, 1);
        assert!(third.queue_empty);
        let drained = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        assert_eq!(drained.settled_streams, 0, "a drained queue is a no-op");
        assert!(drained.queue_empty);
    }

    #[test]
    fn settle_budget_stops_at_the_candidate_and_byte_limits() {
        let conn = db();
        let big = [0x21_u8; 32];
        let small = [0x22_u8; 32];
        seed_synthetic_candidates(&conn, big, 3);
        seed_synthetic_candidates(&conn, small, 1);
        enqueue_refold(&conn, big, 1);
        enqueue_refold(&conn, small, 2);

        // Candidate limit: `big` exactly fills it, so `small` no longer fits the remainder.
        let report =
            settle_pending_content_refolds(&conn, &budget(u64::MAX, 3, u64::MAX, false)).unwrap();
        assert_eq!(report.settled_streams, 1);
        assert_eq!(report.consumed_candidates, 3);
        assert_eq!(report.deferred_budget, 1);
        assert!(!queue_contains(&conn, big));
        assert!(queue_contains(&conn, small));

        // Byte limit on a fresh store: the second stream would exceed the remaining bytes.
        let conn = db();
        seed_synthetic_candidates(&conn, big, 2);
        seed_synthetic_candidates(&conn, small, 1);
        enqueue_refold(&conn, big, 1);
        enqueue_refold(&conn, small, 2);
        let byte_budget = budget(u64::MAX, u64::MAX, 2 * SYNTHETIC_ROW_BYTES, false);
        let report = settle_pending_content_refolds(&conn, &byte_budget).unwrap();
        assert_eq!(report.settled_streams, 1);
        assert_eq!(report.consumed_candidate_bytes, 2 * SYNTHETIC_ROW_BYTES);
        assert_eq!(report.deferred_budget, 1);
        assert!(queue_contains(&conn, small));
        // The resume settles the remainder with a fresh budget.
        let report = settle_pending_content_refolds(&conn, &byte_budget).unwrap();
        assert_eq!(report.settled_streams, 1);
        assert!(report.queue_empty);
    }

    #[test]
    fn admission_charges_the_stats_aggregate_not_a_row_count() {
        let conn = db();
        let stream = [0x31_u8; 32];
        seed_synthetic_candidates(&conn, stream, 3);
        enqueue_refold(&conn, stream, 1);
        assert_eq!(
            stream_stats(&conn, stream),
            (3, (3 * SYNTHETIC_ROW_BYTES) as i64),
            "the stats triggers account counts and length(signed_bytes) + 32 per row",
        );

        // Lie about the aggregate: if admission ever COUNTed the stream's rows it would see 3 and
        // admit; reading the O(1) stats row it sees 10 and the eligibility filter excludes the
        // stream from discovery entirely.
        conn.execute(
            "UPDATE content_stream_stats SET candidate_count = 10 WHERE stream_id = ?1",
            [stream.as_slice()],
        )
        .unwrap();
        let report =
            settle_pending_content_refolds(&conn, &budget(u64::MAX, 5, u64::MAX, false)).unwrap();
        assert_eq!(report.settled_streams, 0);
        assert_eq!(
            report.deferred_oversize, 0,
            "filtered from discovery by the stats row, not a COUNT(*): never listed, never counted",
        );
        assert!(!report.queue_empty, "the queue-empty probe still sees the queued stream");
        assert!(queue_contains(&conn, stream));

        // Restore the true aggregate and the same budget admits the stream.
        conn.execute("UPDATE content_stream_stats SET candidate_count = 3 WHERE stream_id = ?1", [
            stream.as_slice(),
        ])
        .unwrap();
        let report =
            settle_pending_content_refolds(&conn, &budget(u64::MAX, 5, u64::MAX, false)).unwrap();
        assert_eq!(report.settled_streams, 1);
        assert_eq!(report.consumed_candidates, 3);
        assert!(report.queue_empty);
    }

    #[test]
    fn admission_revalidates_cost_after_the_initial_listing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("revalidate-cost.sqlite");
        let conn = Connection::open(&path).unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let concurrent = Connection::open(&path).unwrap();
        let stream = [0x32_u8; 32];
        seed_synthetic_candidates(&conn, stream, 1);
        enqueue_refold(&conn, stream, 1);

        let report = settle_pending_content_refolds_inner(
            &conn,
            &budget(u64::MAX, 2, u64::MAX, false),
            || seed_synthetic_candidates(&concurrent, stream, 2),
        )
        .unwrap();

        assert_eq!(report.settled_streams, 0);
        assert_eq!(report.consumed_candidates, 0, "a skipped stream was never admitted");
        assert_eq!(report.deferred_oversize, 1, "admission saw the current three-row cost");
        assert!(!report.queue_empty);
        assert!(queue_contains(&conn, stream));
        let status_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_entry_status s
                 JOIN content_entries e ON e.entry_hash = s.entry_hash
                 WHERE e.stream_id = ?1",
                [stream.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status_rows, 0, "admission rollback made no fold writes");
    }

    #[test]
    fn queue_empty_observes_a_different_stream_enqueued_during_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current-remaining.sqlite");
        let conn = Connection::open(&path).unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let concurrent = Connection::open(&path).unwrap();
        let listed = [0x33_u8; 32];
        let newly_enqueued = [0x34_u8; 32];
        seed_synthetic_candidates(&conn, listed, 1);
        enqueue_refold(&conn, listed, 1);

        let report =
            settle_pending_content_refolds_inner(&conn, &ContentRefoldBudget::unbounded(), || {
                seed_synthetic_candidates(&concurrent, newly_enqueued, 1);
                enqueue_refold(&concurrent, newly_enqueued, 2);
            })
            .unwrap();

        assert_eq!(report.settled_streams, 1);
        assert!(!report.queue_empty, "the final queue-empty probe sees the committed enqueue");
        assert!(!queue_contains(&conn, listed));
        assert!(queue_contains(&conn, newly_enqueued));
    }

    #[test]
    fn a_queue_row_removed_after_listing_consumes_no_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vanished-queue-row.sqlite");
        let conn = Connection::open(&path).unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let concurrent = Connection::open(&path).unwrap();
        let stream = [0x35_u8; 32];
        seed_synthetic_candidates(&conn, stream, 1);
        enqueue_refold(&conn, stream, 1);

        let report = settle_pending_content_refolds_inner(
            &conn,
            &budget(1, 1, SYNTHETIC_ROW_BYTES, false),
            || {
                concurrent
                    .execute("DELETE FROM content_streams_pending_refold WHERE stream_id = ?1", [
                        stream.as_slice(),
                    ])
                    .unwrap();
            },
        )
        .unwrap();

        assert_eq!(report.settled_streams, 0);
        assert_eq!(report.consumed_candidates, 0);
        assert_eq!(report.consumed_candidate_bytes, 0);
        assert!(report.failures.is_empty());
        assert!(report.queue_empty);
    }

    #[test]
    fn normal_mode_skips_an_oversize_stream_without_blocking_smaller_ones() {
        let conn = db();
        // The OLDEST stream is oversize: head-of-line blocking would stall the whole queue here.
        let oversize = [0x41_u8; 32];
        let small = [0x42_u8; 32];
        seed_synthetic_candidates(&conn, oversize, 5);
        seed_synthetic_candidates(&conn, small, 1);
        enqueue_refold(&conn, oversize, 1);
        enqueue_refold(&conn, small, 2);

        let report =
            settle_pending_content_refolds(&conn, &budget(u64::MAX, 2, u64::MAX, false)).unwrap();
        assert_eq!(report.settled_streams, 1, "the smaller stream still settles");
        assert_eq!(report.consumed_candidates, 1);
        assert_eq!(report.deferred_budget, 0);
        assert_eq!(
            report.deferred_oversize, 0,
            "the oversize stream is filtered out of discovery, not listed and deferred",
        );
        assert!(!report.queue_empty);
        assert!(queue_contains(&conn, oversize), "the oversize stream stays queued");
        assert!(!queue_contains(&conn, small));

        // Normal mode NEVER starts it: repeated calls converge nothing further and never re-list
        // it.
        let stuck =
            settle_pending_content_refolds(&conn, &budget(u64::MAX, 2, u64::MAX, false)).unwrap();
        assert_eq!(stuck.settled_streams, 0);
        assert_eq!(stuck.deferred_oversize, 0);
        assert!(!stuck.queue_empty);
    }

    #[test]
    fn oversize_maintenance_mode_forces_one_oldest_oversize_stream_per_call() {
        let conn = db();
        let oldest_big = [0x51_u8; 32];
        let other_big = [0x52_u8; 32];
        let small = [0x53_u8; 32];
        seed_synthetic_candidates(&conn, oldest_big, 3);
        seed_synthetic_candidates(&conn, other_big, 3);
        seed_synthetic_candidates(&conn, small, 1);
        enqueue_refold(&conn, oldest_big, 1);
        enqueue_refold(&conn, other_big, 2);
        enqueue_refold(&conn, small, 3);

        let maintenance = budget(u64::MAX, 2, u64::MAX, true);
        // Call 1: normal discovery lists the small stream (eligible), so the oversize probe is
        // NOT run this call (#798 finding 2: probing while eligible work remains would degenerate
        // to a full queue scan when no oversize row exists). Only the small stream settles.
        let first = settle_pending_content_refolds(&conn, &maintenance).unwrap();
        assert_eq!(
            first.settled_streams, 1,
            "only the eligible small stream settles; the oversize slot is deferred while normal \
             discovery found work",
        );
        assert_eq!(first.consumed_candidates, 1, "no intentional exceedance yet");
        assert!(!first.queue_empty);
        assert!(!queue_contains(&conn, small));
        assert!(
            queue_contains(&conn, oldest_big),
            "oversize rows wait for a no-eligible-work call"
        );
        assert!(queue_contains(&conn, other_big));

        // Call 2: normal discovery lists ZERO eligible rows (both bigs are filtered as oversize),
        // so the oversize slot runs and settles the OLDEST oversize stream, an intentional
        // exceedance visible in the charged counters.
        let second = settle_pending_content_refolds(&conn, &maintenance).unwrap();
        assert_eq!(second.settled_streams, 1);
        assert_eq!(second.consumed_candidates, 3, "the intentional exceedance is charged");
        assert!(second.consumed_candidates > maintenance.max_candidates);
        assert_eq!(
            second.deferred_oversize, 0,
            "the second oversize stream is filtered out of discovery, not listed and deferred"
        );
        assert!(!second.queue_empty);
        assert!(!queue_contains(&conn, oldest_big));
        assert!(queue_contains(&conn, other_big), "one oversize attempt per call");

        // Call 3: the scheduled maintenance loop converges — the second big stream claims this
        // call's oversize slot.
        let third = settle_pending_content_refolds(&conn, &maintenance).unwrap();
        assert_eq!(third.settled_streams, 1);
        assert_eq!(third.consumed_candidates, 3);
        assert!(!queue_contains(&conn, other_big));
        assert!(third.queue_empty);
    }

    #[test]
    fn a_poisoned_stream_is_reported_and_retained_without_blocking_the_batch() {
        let conn = db();
        let poisoned = [0x61_u8; 32];
        let healthy = [0x62_u8; 32];
        seed_synthetic_candidates(&conn, poisoned, 1);
        seed_synthetic_candidates(&conn, healthy, 1);
        enqueue_refold(&conn, poisoned, 1);
        enqueue_refold(&conn, healthy, 2);
        // Fail only the poisoned stream's queue clear: its whole per-stream txn (refold writes
        // included) rolls back, so the queue row is deleted only after a COMMIT.
        let poison_hex: String = poisoned.iter().map(|byte| format!("{byte:02x}")).collect();
        conn.execute_batch(&format!(
            "CREATE TRIGGER poison_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             WHEN OLD.stream_id = X'{poison_hex}'
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;"
        ))
        .unwrap();

        let report = settle_all(&conn);
        assert_eq!(report.settled_streams, 1, "the healthy stream still commits");
        assert_eq!(report.consumed_candidates, 2, "both admitted attempts are charged");
        assert_eq!(report.consumed_candidate_bytes, 2 * SYNTHETIC_ROW_BYTES);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].stream_id, StreamId::from_bytes(poisoned));
        assert!(report.failures[0].error.contains("injected queue-clear failure"));
        assert!(!report.queue_empty);
        assert!(queue_contains(&conn, poisoned), "the poisoned stream keeps its queue mark");
        assert!(!queue_contains(&conn, healthy));
        // The poisoned stream's txn rolled back: no declassify status write survived either.
        let status_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_entry_status s
                 JOIN content_entries e ON e.entry_hash = s.entry_hash
                 WHERE e.stream_id = ?1",
                [poisoned.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status_rows, 0, "the failed stream's refold writes rolled back with its txn");
    }

    #[test]
    fn poisoned_attempts_consume_every_budget_axis() {
        let conn = db();
        let first = [0x63_u8; 32];
        let second = [0x64_u8; 32];
        seed_synthetic_candidates(&conn, first, 2);
        seed_synthetic_candidates(&conn, second, 1);
        enqueue_refold(&conn, first, 1);
        enqueue_refold(&conn, second, 2);
        conn.execute_batch(
            "CREATE TRIGGER poison_every_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;",
        )
        .unwrap();

        let report =
            settle_pending_content_refolds(&conn, &budget(1, 2, 2 * SYNTHETIC_ROW_BYTES, false))
                .unwrap();

        assert_eq!(report.settled_streams, 0);
        assert_eq!(report.failures.len(), 1, "the stream budget permits one attempt");
        assert_eq!(report.failures[0].stream_id, StreamId::from_bytes(first));
        assert_eq!(report.consumed_candidates, 2, "failed admitted work consumes candidates");
        assert_eq!(report.consumed_candidate_bytes, 2 * SYNTHETIC_ROW_BYTES);
        assert_eq!(report.deferred_budget, 1);
        assert!(!report.queue_empty);
        assert!(queue_contains(&conn, first));
        assert!(queue_contains(&conn, second));
    }

    #[test]
    fn oversize_mode_never_bypasses_the_stream_limit() {
        let conn = db();
        let oversize = [0x65_u8; 32];
        seed_synthetic_candidates(&conn, oversize, 3);
        enqueue_refold(&conn, oversize, 1);

        let zero_streams =
            settle_pending_content_refolds(&conn, &budget(0, 2, u64::MAX, true)).unwrap();
        assert_eq!(zero_streams.settled_streams, 0);
        assert_eq!(zero_streams.consumed_candidates, 0);
        assert!(zero_streams.failures.is_empty());
        assert!(!zero_streams.queue_empty);
        assert!(queue_contains(&conn, oversize));

        let conn = db();
        let normal = [0x66_u8; 32];
        let oversize = [0x67_u8; 32];
        seed_synthetic_candidates(&conn, normal, 1);
        seed_synthetic_candidates(&conn, oversize, 3);
        enqueue_refold(&conn, normal, 1);
        enqueue_refold(&conn, oversize, 2);

        let one_stream =
            settle_pending_content_refolds(&conn, &budget(1, 2, u64::MAX, true)).unwrap();
        assert_eq!(one_stream.settled_streams, 1);
        assert_eq!(one_stream.consumed_candidates, 1);
        assert_eq!(
            one_stream.deferred_oversize, 0,
            "the oversize stream is filtered out of discovery, not listed and deferred"
        );
        assert!(!one_stream.queue_empty);
        assert!(!queue_contains(&conn, normal));
        assert!(queue_contains(&conn, oversize), "the oversize attempt had no stream slot");
    }

    #[test]
    fn settle_admits_oldest_first_then_breaks_ties_by_stream_id() {
        let conn = db();
        let older = [0xff_u8; 32];
        let tie_low = [0x71_u8; 32];
        let tie_mid = [0x72_u8; 32];
        let tie_high = [0x73_u8; 32];
        for stream in [older, tie_high, tie_low, tie_mid] {
            seed_synthetic_candidates(&conn, stream, 1);
        }
        // Enqueue out of order; `older` has the numerically largest stream id but the earliest
        // timestamp, so first_enqueued_at_ms dominates and stream_id only breaks ties.
        enqueue_refold(&conn, tie_high, 5);
        enqueue_refold(&conn, older, 1);
        enqueue_refold(&conn, tie_mid, 5);
        enqueue_refold(&conn, tie_low, 5);

        let one_stream = budget(1, u64::MAX, u64::MAX, false);
        let mut settled_order = Vec::new();
        for expected in [older, tie_low, tie_mid, tie_high] {
            let report = settle_pending_content_refolds(&conn, &one_stream).unwrap();
            assert_eq!(report.settled_streams, 1);
            assert!(!queue_contains(&conn, expected), "expected {expected:?} to settle next");
            settled_order.push(expected);
        }
        assert_eq!(settled_order, vec![older, tie_low, tie_mid, tie_high]);
        assert_eq!(pending_refold_count(&conn), 0);
    }

    // ---- #798 review: bounded progressive candidate discovery ----

    fn reset_settle_work_counters() {
        SETTLE_LISTING_QUERIES.store(0, Ordering::SeqCst);
        SETTLE_ADMISSION_PROBES.store(0, Ordering::SeqCst);
        SETTLE_COMPLETION_PROBES.store(0, Ordering::SeqCst);
    }

    fn settle_work_counters() -> (usize, usize) {
        (
            SETTLE_LISTING_QUERIES.load(Ordering::SeqCst),
            SETTLE_ADMISSION_PROBES.load(Ordering::SeqCst),
        )
    }

    fn settle_completion_probes() -> usize {
        SETTLE_COMPLETION_PROBES.load(Ordering::SeqCst)
    }

    /// One deterministic stream id per backlog ordinal.
    fn backlog_stream(ordinal: u64) -> [u8; 32] {
        let mut stream = [0x9d_u8; 32];
        stream[..8].copy_from_slice(&ordinal.to_be_bytes());
        stream
    }

    #[test]
    fn settle_scan_work_is_bounded_by_the_budget_not_the_backlog() {
        let conn = db();
        const QUEUE: u64 = 16_384;
        {
            let tx = conn.unchecked_transaction().unwrap();
            for ordinal in 0..QUEUE {
                let stream = backlog_stream(ordinal);
                seed_synthetic_candidates(&tx, stream, 1);
                enqueue_refold(&tx, stream, i64::try_from(ordinal).unwrap() + 1);
            }
            tx.commit().unwrap();
        }
        let one_stream = budget(1, u64::MAX, u64::MAX, false);

        reset_settle_work_counters();
        let first = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        let (listings, probes) = settle_work_counters();
        assert_eq!(first.settled_streams, 1);
        assert!(
            !first.queue_empty,
            "the O(1) queue-empty EXISTS probe still observes the backlog is non-empty",
        );
        assert_eq!(
            pending_refold_count(&conn),
            i64::try_from(QUEUE - 1).unwrap(),
            "the whole 16k backlog minus the one settled stream is still queued",
        );
        assert_eq!(
            settle_completion_probes(),
            1,
            "completion is one O(1) EXISTS probe, never a COUNT(*) over the 16k backlog (#798)",
        );
        assert_eq!(listings, 1, "one bounded page listing, independent of the 16k backlog");
        assert_eq!(probes, 1, "only the admitted stream pays for an IMMEDIATE probe");
        assert_eq!(
            first.deferred_budget,
            settle_candidate_batch_size(&one_stream) - 1,
            "deferral counters classify only the discovered page, not the untouched backlog",
        );

        // A repeated call keeps the SAME per-call bound: draining stays linear in total, with no
        // per-call full scan.
        reset_settle_work_counters();
        let second = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        let (listings, probes) = settle_work_counters();
        assert_eq!(second.settled_streams, 1);
        assert!(!second.queue_empty);
        assert_eq!(pending_refold_count(&conn), i64::try_from(QUEUE - 2).unwrap());
        assert_eq!(
            settle_completion_probes(),
            1,
            "the resume also completes with a single O(1) EXISTS probe",
        );
        assert_eq!(listings, 1);
        assert_eq!(probes, 1);
    }

    #[test]
    fn vanished_rows_trigger_progressive_paging_to_find_eligible_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paged-vanish.sqlite");
        let conn = Connection::open(&path).unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        let concurrent = Connection::open(&path).unwrap();
        const QUEUE: u64 = 100;
        const VANISHED: u64 = 12;
        {
            let tx = conn.unchecked_transaction().unwrap();
            for ordinal in 0..QUEUE {
                let stream = backlog_stream(ordinal);
                seed_synthetic_candidates(&tx, stream, 1);
                enqueue_refold(&tx, stream, i64::try_from(ordinal).unwrap() + 1);
            }
            tx.commit().unwrap();
        }
        let one_stream = budget(1, u64::MAX, u64::MAX, false);

        // Delete the 12 oldest queue rows AFTER the first page listing: page 1 probes nothing but
        // vanished rows, so discovery must page onward — without ever listing the rest of the
        // queue — to reach the first eligible stream.
        reset_settle_work_counters();
        let report = settle_pending_content_refolds_inner(&conn, &one_stream, || {
            for ordinal in 0..VANISHED {
                concurrent
                    .execute("DELETE FROM content_streams_pending_refold WHERE stream_id = ?1", [
                        backlog_stream(ordinal).as_slice(),
                    ])
                    .unwrap();
            }
        })
        .unwrap();
        let (listings, probes) = settle_work_counters();

        assert_eq!(report.settled_streams, 1, "the first surviving stream settles");
        assert!(!queue_contains(&conn, backlog_stream(VANISHED)));
        assert!(!report.queue_empty, "the O(1) queue-empty probe still sees queued work");
        assert_eq!(
            pending_refold_count(&conn),
            i64::try_from(QUEUE - VANISHED - 1).unwrap(),
            "the whole-queue count minus the vanished rows and the one settled stream",
        );
        assert_eq!(listings, 2, "a full page of vanishes pages onward exactly once");
        assert_eq!(
            probes,
            settle_candidate_batch_size(&one_stream) + 1,
            "one full page of vanished probes plus the single admitted probe; no later queued row \
             was touched",
        );
    }

    #[test]
    fn an_oversize_head_does_not_block_a_smaller_later_row_in_the_same_page() {
        let conn = db();
        let oversize = [0x81_u8; 32];
        let small = [0x82_u8; 32];
        seed_synthetic_candidates(&conn, oversize, 5);
        seed_synthetic_candidates(&conn, small, 1);
        enqueue_refold(&conn, oversize, 1);
        enqueue_refold(&conn, small, 2);
        // Backlog filler behind them proves discovery stops once the budget is spent.
        const FILLER: u64 = 50;
        {
            let tx = conn.unchecked_transaction().unwrap();
            for ordinal in 0..FILLER {
                let stream = backlog_stream(ordinal);
                seed_synthetic_candidates(&tx, stream, 1);
                enqueue_refold(&tx, stream, i64::try_from(ordinal).unwrap() + 3);
            }
            tx.commit().unwrap();
        }
        let one_stream = budget(1, 2, u64::MAX, false);

        reset_settle_work_counters();
        let report = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        let (listings, probes) = settle_work_counters();

        assert_eq!(report.settled_streams, 1, "the smaller later stream settles");
        assert_eq!(
            report.deferred_oversize, 0,
            "the oversize head is filtered out of discovery entirely",
        );
        assert_eq!(
            report.deferred_budget,
            settle_candidate_batch_size(&one_stream) - 1,
            "the discovered fillers defer on the spent stream slot",
        );
        assert!(queue_contains(&conn, oversize));
        assert!(!queue_contains(&conn, small));
        assert!(!report.queue_empty);
        assert_eq!(pending_refold_count(&conn), i64::try_from(FILLER + 1).unwrap());
        assert_eq!(listings, 1);
        assert_eq!(probes, 1, "the filtered-out oversize head costs no transaction");
    }

    #[test]
    fn an_oversize_backlog_never_blocks_nor_relists_the_small_stream_behind_it() {
        let conn = db();
        // #798 Codex P1: eleven oversize streams ahead of one small stream, with a single stream
        // slot and caps every oversize stream exceeds. The old pager listed a full page of pure
        // oversize deferrals, stopped, and re-listed the SAME rows every call — the small stream
        // never settled. The eligibility-filtered listing skips them inside the query.
        const OVERSIZE: u64 = 11;
        for ordinal in 0..OVERSIZE {
            let stream = backlog_stream(ordinal);
            seed_synthetic_candidates(&conn, stream, 3);
            enqueue_refold(&conn, stream, i64::try_from(ordinal).unwrap() + 1);
        }
        let small = [0x91_u8; 32];
        seed_synthetic_candidates(&conn, small, 1);
        enqueue_refold(&conn, small, i64::try_from(OVERSIZE).unwrap() + 1);
        let one_stream = budget(1, 2, 2 * SYNTHETIC_ROW_BYTES, false);

        reset_settle_work_counters();
        let first = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        let (listings, probes) = settle_work_counters();
        assert_eq!(first.settled_streams, 1, "the small stream settles on the FIRST call");
        assert!(!queue_contains(&conn, small));
        assert!(!first.queue_empty);
        assert_eq!(pending_refold_count(&conn), i64::try_from(OVERSIZE).unwrap());
        assert_eq!(first.deferred_oversize, 0, "filtered rows are never discovered");
        assert_eq!(first.deferred_budget, 0);
        assert_eq!(listings, 1, "one eligibility-filtered page query");
        assert_eq!(probes, 1, "the oversize backlog costs zero admission probes");

        // A follow-up call over the pure-oversize queue discovers nothing and probes nothing.
        reset_settle_work_counters();
        let second = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        let (listings, probes) = settle_work_counters();
        assert_eq!(second.settled_streams, 0);
        assert!(!second.queue_empty);
        assert_eq!(pending_refold_count(&conn), i64::try_from(OVERSIZE).unwrap());
        assert_eq!(listings, 1, "one filtered listing, never a re-listed deferral page");
        assert_eq!(probes, 0);
    }

    #[test]
    fn oversize_maintenance_admits_exactly_the_oldest_oversize_row() {
        let conn = db();
        let oldest = [0xa1_u8; 32];
        let newer = [0xa2_u8; 32];
        seed_synthetic_candidates(&conn, oldest, 3);
        seed_synthetic_candidates(&conn, newer, 4);
        enqueue_refold(&conn, oldest, 1);
        enqueue_refold(&conn, newer, 2);
        // One stream slot, so normal discovery can admit nothing; the oversize slot takes the
        // OLDEST row exceeding the caps via the single targeted query.
        let maintenance = budget(1, 2, u64::MAX, true);

        reset_settle_work_counters();
        let report = settle_pending_content_refolds(&conn, &maintenance).unwrap();
        let (listings, probes) = settle_work_counters();
        assert_eq!(report.settled_streams, 1);
        assert_eq!(report.consumed_candidates, 3, "the intentional exceedance is charged");
        assert!(!queue_contains(&conn, oldest));
        assert!(queue_contains(&conn, newer), "the second oversize row waits for a later call");
        assert!(!report.queue_empty);
        assert_eq!(listings, 2, "one filtered page plus the single targeted oversize query");
        assert_eq!(probes, 1, "only the admitted oversize row pays for a transaction");

        let report = settle_pending_content_refolds(&conn, &maintenance).unwrap();
        assert_eq!(report.settled_streams, 1);
        assert!(report.queue_empty);
    }

    #[test]
    fn oversize_probe_is_not_run_while_eligible_work_remains() {
        // #798 finding 2: the oversize `LIMIT 1` probe degenerates to a full queue scan when no
        // oversize row exists, so it must run ONLY when normal discovery listed zero eligible rows.
        // Here an eligible small stream is discoverable AND a free stream slot remains, yet the
        // oversize stream must NOT be admitted this call — proving the probe was skipped.
        let conn = db();
        let small = [0xb1_u8; 32];
        let oversize = [0xb2_u8; 32];
        seed_synthetic_candidates(&conn, small, 1);
        seed_synthetic_candidates(&conn, oversize, 5);
        enqueue_refold(&conn, small, 1);
        enqueue_refold(&conn, oversize, 2);
        // Maintenance mode with plenty of stream slots: only the `!listed_any` gate keeps the
        // oversize slot from firing. The OLD unconditional probe would settle the oversize row too.
        let maintenance = budget(u64::MAX, 2, u64::MAX, true);

        let report = settle_pending_content_refolds(&conn, &maintenance).unwrap();
        assert_eq!(
            report.settled_streams, 1,
            "only the eligible small stream settles; the oversize probe did not run",
        );
        assert!(!queue_contains(&conn, small));
        assert!(
            queue_contains(&conn, oversize),
            "the oversize probe is skipped while eligible work remained, so the oversize row stays",
        );
        assert_eq!(report.consumed_candidates, 1, "no intentional oversize exceedance was charged");
        assert!(!report.queue_empty);

        // Once the eligible work is drained, a follow-up maintenance call finds zero eligible rows
        // and the probe DOES run, converging the oversize row.
        let report = settle_pending_content_refolds(&conn, &maintenance).unwrap();
        assert_eq!(report.settled_streams, 1, "with no eligible work the oversize probe runs");
        assert!(!queue_contains(&conn, oversize));
        assert!(report.queue_empty);
    }

    #[test]
    fn a_poisoned_stream_is_demoted_so_it_no_longer_head_of_line_blocks() {
        // #798 finding 3: a settle failure keeps the row QUEUED but DEMOTES it behind every
        // currently-queued row, so a persistently-poisoned oldest stream can no longer starve the
        // queue. With a one-stream budget the poisoned oldest row is admitted first each call; only
        // demote-on-failure lets the healthy stream behind it ever settle.
        let conn = db();
        let poisoned = [0xc1_u8; 32];
        let healthy = [0xc2_u8; 32];
        seed_synthetic_candidates(&conn, poisoned, 1);
        seed_synthetic_candidates(&conn, healthy, 1);
        enqueue_refold(&conn, poisoned, 1);
        enqueue_refold(&conn, healthy, 2);
        let poison_hex: String = poisoned.iter().map(|byte| format!("{byte:02x}")).collect();
        conn.execute_batch(&format!(
            "CREATE TRIGGER poison_demote_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             WHEN OLD.stream_id = X'{poison_hex}'
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;"
        ))
        .unwrap();
        let one_stream = budget(1, u64::MAX, u64::MAX, false);

        // Call 1: the poisoned stream is oldest, admitted first, and fails; its one stream slot is
        // spent so the healthy stream is deferred. The failed row is demoted behind the healthy
        // row.
        let first = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        assert_eq!(first.settled_streams, 0, "the poisoned stream took the only stream slot");
        assert_eq!(first.failures.len(), 1);
        assert_eq!(first.failures[0].stream_id, StreamId::from_bytes(poisoned));
        assert!(queue_contains(&conn, poisoned), "a failed stream keeps its queue row");
        assert!(queue_contains(&conn, healthy));
        assert!(
            stream_enqueued_at(&conn, poisoned) > stream_enqueued_at(&conn, healthy),
            "the poisoned row is demoted behind the still-queued healthy row",
        );

        // Call 2: because the poisoned row was demoted, the healthy stream is now oldest and
        // settles — without demotion the poisoned oldest row would head-of-line block it
        // forever.
        let second = settle_pending_content_refolds(&conn, &one_stream).unwrap();
        assert_eq!(second.settled_streams, 1, "the healthy stream settles once the poison demoted");
        assert!(!queue_contains(&conn, healthy));
        assert!(queue_contains(&conn, poisoned), "the poisoned stream is still queued for retry");
        assert!(!second.queue_empty);
    }

    /// Differential cut-binding parity harness (I11). The account control fold and the `/3` content
    /// fold share the "deliberately identical" cut substrate (see `candidate.rs`) — a withheld
    /// watermark condemns beyond-cut from seq alone and parks only the under-cut prefix, and a
    /// misbound watermark neither condemns nor pins. The content fold once diverged (a withheld
    /// watermark parked a beyond-cut entry the account fold condemns). This runs the SAME cut
    /// scenarios — watermark held / withheld / misbound, against an entry beyond and under the cut
    /// — through BOTH real folds and asserts the target entry reaches an IDENTICAL verdict, so
    /// any future re-divergence of the whole class (not just this instance) fails here.
    mod cut_binding_parity {
        use std::collections::HashMap;

        use super::*;
        use crate::account::cut::Cut;
        use crate::account::envelope::{
            AccountEntryHeader, VerifiedAccountEntry, sign_account_entry, verify_account_signed,
        };
        use crate::account::fold::{
            AccountAuthHistory, CondemnedReason, Outcome, ParkReason, fold_account,
        };
        use crate::account::id::account_id_from_genesis_payload;
        use crate::account::ops::{self as account_ops, AccountOp, entry_type};
        use crate::account::{AccountId, DeviceRole};
        use crate::device::{DeviceSecret, DeviceX25519Secret};
        use crate::op::DeviceFingerprint;

        /// The normalized verdict both folds must agree on — the effect the cut has on ONE target
        /// entry, projected out of each fold's own taxonomy.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum CutParity {
            CondemnedBeyondCut,
            ParkedUnknownCutTarget,
            /// The cut did not bite this entry: `accepted` (content) / `effective` (account).
            Survives,
        }

        #[derive(Debug, Clone, Copy)]
        enum Watermark {
            Held,
            Withheld,
            Misbound,
        }

        #[derive(Debug, Clone, Copy)]
        enum Target {
            BeyondCut,
            UnderCut,
        }

        /// A seed-deterministic account-fold test device.
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
                let x = DeviceX25519Secret::from_seed(&[seed.wrapping_add(0x80); 32])
                    .public()
                    .to_bytes();
                Dev { fp: public.fingerprint(), ed: public.to_bytes(), x, secret }
            }
        }

        /// A minimal account-log authoring fixture: it threads each device's `(seq, prev)` chain
        /// and signs real entries so `fold_account` runs over verified input.
        struct AccountLog {
            account_id: AccountId,
            genesis_hash: [u8; 32],
            chains: HashMap<[u8; 32], (u64, Option<[u8; 32]>)>,
            entries: Vec<VerifiedAccountEntry>,
        }

        impl AccountLog {
            fn genesis(founder: &Dev) -> Self {
                let op = AccountOp::AccountGenesis {
                    ed25519_pubkey: founder.ed,
                    x25519_pubkey: founder.x,
                    nonce16: [0u8; 16],
                    created_at_ms: 1_700_000_000_000,
                    label: None,
                };
                let payload = account_ops::encode(&op).unwrap();
                let account_id = account_id_from_genesis_payload(&payload);
                let header = AccountEntryHeader {
                    account_id,
                    log_id: 0,
                    device_fingerprint: founder.fp,
                    seq: 0,
                    prev_hash: None,
                    parent_ref: None,
                    entry_type: entry_type::ACCOUNT_GENESIS,
                    op_version: 1,
                    crypto_suite: 0,
                    auth_len: 0,
                    key_id: None,
                    authority_ref: None,
                };
                let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
                let verified =
                    verify_account_signed(&signed.signed_bytes, &founder.secret.public()).unwrap();
                let genesis_hash = verified.entry_hash;
                let mut chains = HashMap::new();
                chains.insert(founder.fp.to_bytes(), (1, Some(genesis_hash)));
                AccountLog { account_id, genesis_hash, chains, entries: vec![verified] }
            }

            fn author(
                &mut self,
                author: &Dev,
                authority_ref: Option<[u8; 32]>,
                op: &AccountOp,
            ) -> [u8; 32] {
                let payload = account_ops::encode(op).unwrap();
                let (seq, prev) =
                    self.chains.get(&author.fp.to_bytes()).copied().unwrap_or((0, None));
                let header = AccountEntryHeader {
                    account_id: self.account_id,
                    log_id: 0,
                    device_fingerprint: author.fp,
                    seq,
                    prev_hash: prev,
                    parent_ref: Some(self.genesis_hash),
                    entry_type: account_ops::entry_type_of(op),
                    op_version: 1,
                    auth_len: 1,
                    crypto_suite: 0,
                    key_id: None,
                    authority_ref,
                };
                let signed = sign_account_entry(&author.secret, &header, &payload).unwrap();
                let verified =
                    verify_account_signed(&signed.signed_bytes, &author.secret.public()).unwrap();
                let hash = verified.entry_hash;
                self.chains.insert(author.fp.to_bytes(), (seq + 1, Some(hash)));
                self.entries.push(verified);
                hash
            }
        }

        fn member_add(dev: &Dev) -> AccountOp {
            AccountOp::DeviceAdd {
                device_fingerprint: dev.fp,
                ed25519_pubkey: dev.ed,
                x25519_pubkey: dev.x,
                role: DeviceRole::Member,
                label: None,
            }
        }

        /// Drive the account control fold: founder F (owner) enrolls owner B; B authors a dense
        /// control chain b0→b1→b2; F removes B with a cut bounding B's chain at seq 1 (watermark
        /// b1). B's own chain is the cut coordinate — the account analog of the content stream
        /// chain.
        fn account_verdict(target: Target, watermark: Watermark) -> CutParity {
            let founder = Dev::new(0xF1);
            let b = Dev::new(0xB1);
            let mut log = AccountLog::genesis(&founder);
            let add_b = log.author(&founder, Some(log.genesis_hash), &AccountOp::DeviceAdd {
                device_fingerprint: b.fp,
                ed25519_pubkey: b.ed,
                x25519_pubkey: b.x,
                role: DeviceRole::Owner,
                label: None,
            });
            let b0 = log.author(&b, Some(add_b), &member_add(&Dev::new(0xD1)));
            let b1 = log.author(&b, Some(add_b), &member_add(&Dev::new(0xE1)));
            let b2 = log.author(&b, Some(add_b), &member_add(&Dev::new(0x71)));
            let control_cut = match watermark {
                // A misbound watermark names the WRONG seq on B's chain (b0 is seq 0, the cut
                // claims seq 1): the §11.3 guard rejects the whole remove, so B
                // (and b0/b2) survives.
                Watermark::Misbound => Cut::At { seq: 1, hash: b0 },
                Watermark::Held | Watermark::Withheld => Cut::At { seq: 1, hash: b1 },
            };
            log.author(&founder, Some(log.genesis_hash), &AccountOp::DeviceRemove {
                device_fingerprint: b.fp,
                control_cut,
                secrets_cut: Cut::Empty,
                content_cuts: Vec::new(),
                reason: "revoked".to_string(),
            });
            // A withheld watermark models b1 not yet synced — fold every entry EXCEPT b1.
            let history: AccountAuthHistory = match watermark {
                Watermark::Withheld => {
                    let held: Vec<VerifiedAccountEntry> =
                        log.entries.iter().filter(|e| e.entry_hash != b1).cloned().collect();
                    fold_account(&held)
                },
                _ => fold_account(&log.entries),
            };
            let target_hash = match target {
                Target::BeyondCut => b2,
                Target::UnderCut => b0,
            };
            match history.outcome(&target_hash) {
                Some(Outcome::Condemned(CondemnedReason::BeyondCut)) =>
                    CutParity::CondemnedBeyondCut,
                Some(Outcome::Parked(ParkReason::UnknownCutTarget)) =>
                    CutParity::ParkedUnknownCutTarget,
                Some(Outcome::Effective { .. }) => CutParity::Survives,
                other =>
                    panic!("account fold: unexpected {target:?}/{watermark:?} outcome {other:?}"),
            }
        }

        /// Drive the `/3` content fold over the analogous scenario: a dense stream chain s0→s1→s2
        /// on one coordinate, a roster content cut bounding it at seq 1 (watermark s1).
        fn content_verdict(target: Target, watermark: Watermark) -> CutParity {
            let conn = db();
            let secret = DeviceSecret::from_seed(&[0xC0; 32]);
            let (owner, genesis) = roster(&conn, &secret);
            seed_ownership(&conn, owner);
            seed_roster_fact(&conn, genesis, owner, &secret, "owner");
            let s0 = authored(&secret, owner, genesis, ContentSpec::default());
            let s1 = authored(&secret, owner, genesis, ContentSpec {
                seq: 1,
                previous: Some(s0.entry_hash),
                ..ContentSpec::default()
            });
            let s2 = authored(&secret, owner, genesis, ContentSpec {
                seq: 2,
                previous: Some(s1.entry_hash),
                ..ContentSpec::default()
            });
            let cut_watermark = match watermark {
                // Misbound: names s0 (seq 0) as the seq-1 watermark — a same-coordinate seq
                // mismatch.
                Watermark::Misbound => s0.entry_hash,
                Watermark::Held | Watermark::Withheld => s1.entry_hash,
            };
            seed_roster_content_cut(&conn, genesis, owner, 1, cut_watermark);
            content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
            // A withheld watermark models s1 not yet ingested — the content analog of dropping b1.
            if !matches!(watermark, Watermark::Withheld) {
                content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
            }
            content_ingest(&conn, &s2.signed_bytes, 3).unwrap();
            settle_all(&conn);
            let target_hash = match target {
                Target::BeyondCut => s2.entry_hash,
                Target::UnderCut => s0.entry_hash,
            };
            match verdict(&conn, &target_hash).0.as_str() {
                "condemned{beyond_cut}" => CutParity::CondemnedBeyondCut,
                "parked{unknown_cut_target}" => CutParity::ParkedUnknownCutTarget,
                "accepted" => CutParity::Survives,
                other => panic!("content fold: unexpected {target:?}/{watermark:?} status {other}"),
            }
        }

        #[test]
        fn account_and_content_folds_agree_on_every_cut_binding() {
            for (target, watermark, expected) in [
                (Target::BeyondCut, Watermark::Held, CutParity::CondemnedBeyondCut),
                // The exact divergence this fix closes: a withheld watermark must still condemn.
                (Target::BeyondCut, Watermark::Withheld, CutParity::CondemnedBeyondCut),
                (Target::BeyondCut, Watermark::Misbound, CutParity::Survives),
                (Target::UnderCut, Watermark::Held, CutParity::Survives),
                (Target::UnderCut, Watermark::Withheld, CutParity::ParkedUnknownCutTarget),
                (Target::UnderCut, Watermark::Misbound, CutParity::Survives),
            ] {
                let account = account_verdict(target, watermark);
                let content = content_verdict(target, watermark);
                assert_eq!(
                    account, content,
                    "folds diverged for {target:?}/{watermark:?}: account={account:?} \
                     content={content:?}",
                );
                assert_eq!(
                    account, expected,
                    "the account fold verdict for {target:?}/{watermark:?} is not the intended one",
                );
            }
        }
    }
}
