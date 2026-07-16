//! C2 `/3` candidate-DAG ingest and dense-chain structural classification (§16).
//!
//! This layer verifies an exact content-addressed `roster_ref`, signatures, and dense predecessor
//! coordinates. It deliberately never sets `accepted`: C3 must evaluate authority, cuts,
//! freshness, and branch selection together before content can reach the live projection.

use std::collections::{HashMap, HashSet, VecDeque};

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
use crate::oplog::account::{
    AccountId, AuthorityBoundary, AuthorityFreshness, AuthorityQuery, GrantDeviceBoundary,
    GrantRole,
};
use crate::oplog::cbor;
use crate::oplog::device::DevicePublic;
use crate::oplog::op::DeviceFingerprint;
use crate::oplog::stream::StreamId;

type EntryHash = [u8; 32];

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
pub(crate) enum ContentCapacityScope {
    PreVerifyAuthor,
    PreVerifyGlobal,
    CandidateAuthor,
    CandidateGlobal,
    CandidateAuthorBytes,
    CandidateGlobalBytes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::oplog) enum ContentIngestOutcome {
    Rejected(String),
    PreVerify,
    PreVerifyWithEviction { scopes: Vec<ContentCapacityScope> },
    CapacityReached { scope: ContentCapacityScope },
    Ingested { status: String },
}

#[derive(Debug, Default)]
pub(in crate::oplog::account) struct ContentPromotionOutcome {
    pub(in crate::oplog::account) scope: Option<ContentCapacityScope>,
    pub(in crate::oplog::account) entry_hashes: Vec<EntryHash>,
}

pub(in crate::oplog) fn content_ingest(
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
    // Structural classification is done; now fold authority + branch selection over the whole
    // stream (§13) — the pass that can set `accepted`. With the owner's `StreamOwn` fact absent
    // this is a no-op and the entry stays `retained_unfolded` until the account→content trigger
    // refolds it; with authority present the entry lands on its real verdict in this same txn.
    refold_content_stream(&tx, verified.header.stream_id)?;
    // TODO(phase D): project a newly-ACCEPTED FOREIGN entry into `repo_memories` /
    // `repo_node_edges` here (and from the account→content retro-trigger that retro-accepts
    // foreign content), so a remote memory surfaces in the local read APIs — those read ONLY
    // the memory tables, never this `/3` stream or its projection. Deferrable until transport
    // lands because no foreign entry can exist before then; the local author path needs no such
    // write-back (it authors `/3` content FROM `repo_memories`, so the reader's row is already
    // present). Tracked in #691.
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

pub(in crate::oplog::account) fn promote_pre_verify_for_account(
    tx: &Transaction<'_>,
    account_id: super::super::AccountId,
    now_ms: i64,
) -> anyhow::Result<ContentPromotionOutcome> {
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
) -> rusqlite::Result<Option<ContentCapacityScope>> {
    let author = entry.header.author_account_id.to_bytes();
    let (count, bytes): (i64, i64) = tx.query_row(
        "SELECT count(*), coalesce(sum(length(signed_bytes)), 0)
         FROM content_entries WHERE author_account_id = ?1",
        [author.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count >= CANDIDATES_PER_AUTHOR_MAX {
        return Ok(Some(ContentCapacityScope::CandidateAuthor));
    }
    if bytes.saturating_add(incoming_bytes as i64) > CANDIDATE_BYTES_PER_AUTHOR_MAX {
        return Ok(Some(ContentCapacityScope::CandidateAuthorBytes));
    }
    let (count, bytes): (i64, i64) = tx.query_row(
        "SELECT count(*), coalesce(sum(length(signed_bytes)), 0) FROM content_entries",
        [],
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
            tx.query_row(
                "SELECT EXISTS(
                         SELECT 1 FROM content_entries p
                         JOIN content_entry_status s ON s.entry_hash = p.entry_hash
                         WHERE p.entry_hash = ?1 AND p.stream_id = ?2
                           AND p.author_account_id = ?3 AND p.device_fingerprint = ?4
                           AND p.seq = ?5 AND s.status = ?6)",
                params![
                    previous.as_slice(),
                    entry.header.stream_id.to_bytes().as_slice(),
                    entry.header.author_account_id.to_bytes().as_slice(),
                    entry.header.device_fingerprint.to_bytes().as_slice(),
                    expected.as_slice(),
                    ContentStatus::RetainedUnfolded.as_db_str(),
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

/// Re-derive `/3` acceptance for every candidate on `stream_id` from the CURRENT account fold and
/// write each entry's `accepted` flag + status (§13). This is the only writer of `accepted = 1`.
///
/// Runs entirely inside the caller's `IMMEDIATE` transaction. §13 orders the passes structural →
/// authority+registers → branch → freshness, and every authority fact is read in this one snapshot:
/// a concurrent account refold committing mid-evaluation could otherwise pair an old grant with a
/// new cut. Nothing is ever mutated in the log — a late control or ancestry arrival simply refolds
/// the same candidates to a new classification (retro-condemn or re-bless), never rewrites history.
/// Re-evaluate every content stream whose acceptance THIS account's authority can change, after its
/// control fold rewrote the authority projection. This is the account→content trigger: a grant
/// revocation, roster cut, ownership arrival, or contested flip retro-classifies already-stored
/// content in the SAME account-refold txn — otherwise a revocation would not take effect until an
/// unrelated content entry happened to land on the stream (L2/I5 would be unenforceable).
///
/// A stream is reachable from this account two ways: the account OWNS it (its ownership/grant/cut
/// facts bound the stream), or the account AUTHORS content on it (its roster/contested state gates
/// that content). The union covers every cross-account case — a `StreamRevoke` folds in the OWNER's
/// log and reaches the grantee's content through the ownership branch; a roster change folds in the
/// AUTHOR's log and reaches it through the author branch.
pub(in crate::oplog::account) fn refold_streams_for_account(
    tx: &Transaction<'_>,
    account_id: AccountId,
    previously_owned: &[[u8; 32]],
) -> anyhow::Result<()> {
    // The `/3` tables are created by a LATER migration than the account authority projection, and
    // the V064/V065 authority backfill folds every existing account inside its own migration — so
    // this runs before `content_entries` exists on an upgrading database. There is no content to
    // classify then; skip until the table is present.
    if !content_entries_exists(tx)? {
        return Ok(());
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
    // This re-derives `accepted` but deliberately does NOT refresh the accepted-`/3` → memory
    // projection (`content_projected_*`). That is safe ONLY while no path here can FLIP an accepted
    // set: pre-transport the reconcile publishes a stream's `StreamOwn` before authoring its
    // content (no retro-accept) and there is no local revoke/demote op (no retro-condemn), so
    // this loop re-derives the identical accepted set the authoring path already reprojected.
    // When transport (or any retro-accept/declassify op) lands, a flip here would leave
    // `content_projected_*` stale and the memory reconcile's anti-join would mass-duplicate or
    // skip rows — so phase-D must call `content_projection::reproject_accepted_content_stream`
    // for each stream below, GUARDED on the V070 `content_projected_*` tables existing (this fn
    // also runs in the pre-V070 V064/V065 authority backfill; mirror the
    // `content_entries_exists` guard above). Tracked in #683.
    for stream in streams {
        refold_content_stream(tx, StreamId::from_bytes(stream))?;
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
    // The owner is inside the stream identity (`stream_id = sha256(cbor([.., owner, ..]))`, §14)
    // but not invertible, so it is resolved through the owner's `StreamOwn` fact. No fact ⇒
    // authority cannot be evaluated: the entries revert to their structural state. This is a
    // DECLASSIFY, not a skip — if ownership was dropped by a later fold (owner contested / branch
    // reselection), previously accepted content must lose `accepted` here, or it would stay live
    // with no current authority basis.
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
        // `pinned_branch` does — otherwise `combine_boundaries` would condemn `beyond_cut` off a
        // watermark that names a foreign coordinate or is not held (§11.3 laundering). A cut naming
        // a different coordinate is malformed → ignored (Open); an unheld watermark parks the whole
        // coordinate (`unknown_cut_target`, I11) rather than condemning.
        let coordinate = ChainCoordinate {
            stream_id: header.stream_id,
            author_account_id: header.author_account_id,
            device_fingerprint: header.device_fingerprint,
        };
        let mut cut_target_unknown = false;
        let roster = bind_roster_cut(roster, &coordinate, &view, &mut cut_target_unknown);
        let grant = bind_grant_cut(grant, &coordinate, &view, &mut cut_target_unknown);
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
        } else if cut_target_unknown {
            SubjectAuthorityHold::UnknownCutTarget
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

/// Bind a roster content cut's watermark to this coordinate against the content DAG (§11.3). An
/// `Ok` binding keeps the cut; a watermark naming a foreign coordinate is malformed and drops to
/// `Open`; an unheld watermark drops to `Open` and flags `unknown_cut_target` so the caller parks
/// the coordinate instead of condemning off an unverifiable cut.
fn bind_roster_cut(
    roster: AuthorityQuery<CitedRosterAuthority>,
    coordinate: &ChainCoordinate,
    view: &dyn HeaderView,
    cut_target_unknown: &mut bool,
) -> AuthorityQuery<CitedRosterAuthority> {
    let AuthorityQuery::Effective(mut fact) = roster else {
        return roster;
    };
    if let AuthorityBoundary::Cut { seq, hash } = fact.authority.boundary {
        match candidate::validate_cut_target(seq, &hash, coordinate, view) {
            CutBinding::Ok => {},
            CutBinding::TargetNotHeld => {
                *cut_target_unknown = true;
                fact.authority.boundary = AuthorityBoundary::Open;
            },
            CutBinding::Mismatch => fact.authority.boundary = AuthorityBoundary::Open,
        }
    }
    AuthorityQuery::Effective(fact)
}

/// Bind a grant device cut's watermark to this coordinate against the content DAG — the grant-side
/// mirror of [`bind_roster_cut`].
fn bind_grant_cut(
    grant: Option<AuthorityQuery<CitedGrantAuthority>>,
    coordinate: &ChainCoordinate,
    view: &dyn HeaderView,
    cut_target_unknown: &mut bool,
) -> Option<AuthorityQuery<CitedGrantAuthority>> {
    let Some(AuthorityQuery::Effective(mut fact)) = grant else {
        return grant;
    };
    let cut = match &fact.authority.boundary {
        GrantDeviceBoundary::Cut(cut) => Some((cut.seq, cut.hash)),
        _ => None,
    };
    if let Some((seq, hash)) = cut {
        match candidate::validate_cut_target(seq, &hash, coordinate, view) {
            CutBinding::Ok => {},
            CutBinding::TargetNotHeld => {
                *cut_target_unknown = true;
                fact.authority.boundary = GrantDeviceBoundary::Open;
            },
            CutBinding::Mismatch => fact.authority.boundary = GrantDeviceBoundary::Open,
        }
    }
    Some(AuthorityQuery::Effective(fact))
}

fn map_roster(
    query: AuthorityQuery<crate::oplog::account::RosterContentAuthority>,
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
    query: AuthorityQuery<crate::oplog::account::GrantDeviceAuthority>,
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
    use super::super::super::envelope::{AccountEntryHeader, sign_account_entry};
    use super::super::super::id::account_id_from_genesis_payload;
    use super::super::ContentEntryHeader;
    use super::*;
    use crate::index::schema;
    use crate::oplog::account::ops::entry_type;
    use crate::oplog::device::{DeviceSecret, DeviceX25519Secret};
    use crate::oplog::stream::StreamId;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
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
        let op = AccountOp::DeviceAdd {
            device_fingerprint: member.public().fingerprint(),
            ed25519_pubkey: member.public().to_bytes(),
            x25519_pubkey: DeviceX25519Secret::from_seed(&[0x82; 32]).public().to_bytes(),
            role: crate::oplog::account::DeviceRole::Member,
            label: None,
        };
        let payload = ops::encode(&op).unwrap();
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: founder.public().fingerprint(),
            seq: 1,
            prev_hash: Some(genesis_hash),
            parent_ref: None,
            entry_type: entry_type::DEVICE_ADD,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 1,
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
                    [2_u8; 32].as_slice(),
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
        seed_content_candidates(&author_count, account, 1, CANDIDATES_PER_AUTHOR_MAX as usize, 1);
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
        seed_content_candidates(&replay, account, 99, CANDIDATES_PER_AUTHOR_MAX as usize - 1, 1);
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

    fn verdict_after_ingest(conn: &Connection, entry: &SignedContentEntry) -> (String, i64) {
        content_ingest(conn, &entry.signed_bytes, 1).unwrap();
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
        assert_eq!(
            content_ingest(&conn, &entry.signed_bytes, 1).unwrap(),
            ContentIngestOutcome::Ingested { status: "accepted".into() },
        );
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
    fn a_cut_whose_watermark_is_not_held_parks_rather_than_condemning() {
        let conn = db();
        let secret = DeviceSecret::from_seed(&[0x62; 32]);
        let (owner, genesis) = roster(&conn, &secret);
        seed_ownership(&conn, owner);
        seed_roster_fact(&conn, genesis, owner, &secret, "owner");
        // A malformed/late cut names a watermark we do not hold. It must NOT condemn an honest
        // beyond-`seq` entry (§11.3 / I11 — an unheld watermark parks, never flips a verdict); a
        // later `CutExtend` or the watermark's arrival re-evaluates it.
        seed_roster_content_cut(&conn, genesis, owner, 3, [0xcc; 32]);

        let entry = authored(&secret, owner, genesis, ContentSpec {
            seq: 5,
            previous: Some([0xaa; 32]),
            ..ContentSpec::default()
        });
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
        refold_streams_for_account(&tx, account, previously_owned).unwrap();
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

        // A dense chain seq 0 → 1, both accepted at ingest.
        let s0 = authored(&secret, owner, genesis, ContentSpec::default());
        content_ingest(&conn, &s0.signed_bytes, 1).unwrap();
        let s1 = authored(&secret, owner, genesis, ContentSpec {
            seq: 1,
            previous: Some(s0.entry_hash),
            ..ContentSpec::default()
        });
        content_ingest(&conn, &s1.signed_bytes, 2).unwrap();
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
        refold_streams_for_account(&tx, owner, &[]).unwrap();
        tx.commit().unwrap();
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

        assert_eq!(
            verdict(&conn, &reader_entry.entry_hash),
            ("rejected{grant_not_writer}".into(), 0)
        );
        // Hash order still decides — the reader cut did not steer the writers' branch.
        assert_eq!(verdict(&conn, &winner.entry_hash), ("accepted".into(), 1));
        assert_eq!(verdict(&conn, &loser.entry_hash), ("forked".into(), 0));
    }
}
