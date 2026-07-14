//! C2 `/3` candidate-DAG ingest and dense-chain structural classification (§16).
//!
//! This layer verifies an exact content-addressed `roster_ref`, signatures, and dense predecessor
//! coordinates. It deliberately never sets `accepted`: C3 must evaluate authority, cuts,
//! freshness, and branch selection together before content can reach the live projection.

use std::collections::VecDeque;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::super::envelope as account_envelope;
use super::super::ops::{self, AccountOp, DecodedAccountOp};
use super::envelope::{self, SignedContentEntry, VerifiedContentEntry};
use crate::oplog::cbor;
use crate::oplog::device::DevicePublic;

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

fn insert_candidate(
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
}
