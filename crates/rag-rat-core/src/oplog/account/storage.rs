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
use super::ops::{self, AccountOp, DecodedAccountOp};
use super::{AccountId, fold};
use crate::oplog::cbor;
use crate::oplog::device::DevicePublic;
use crate::oplog::op::DeviceFingerprint;

type EntryHash = [u8; 32];
type BranchKey = (u8, DeviceFingerprint, Option<EntryHash>);
type BranchChild = (u64, EntryHash);
type BranchChildren = HashMap<BranchKey, Vec<BranchChild>>;

// Operational admission limits, not wire-validity limits. At the §18a envelope maximum these cap
// unauthenticated parked bytes at 4 MiB/account and 16 MiB globally. Candidate history is larger
// because it is signature-authenticated, but still finite so full refold work cannot grow without
// bound before the incremental projector exists.
const PRE_VERIFY_PER_ACCOUNT_MAX: usize = 64;
const PRE_VERIFY_GLOBAL_MAX: usize = 256;
const CANDIDATES_PER_ACCOUNT_MAX: usize = 4_096;

struct AccountProjection {
    history: fold::AccountAuthHistory,
    accepted: HashSet<EntryHash>,
    forked: HashSet<EntryHash>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateInsert {
    Inserted,
    AlreadyPresent,
    AtCapacity,
}

/// The result of ingesting one signed account entry (§16.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IngestOutcome {
    /// Structurally rejected (bad canonicity / over §18a / bad signature / fingerprint or self-hash
    /// mismatch) — NEVER stored.
    Rejected(String),
    /// The signing device is not yet resolvable (`sha256(pk)` matches no known fingerprint) — held
    /// durably in `account_pre_verify`, retried when a later DeviceAdd/AccountGenesis arrives.
    PreVerify,
    /// Stored as a candidate; `status` is its post-refold §16.3 taxonomy label.
    Ingested { status: String },
}

/// Ingest one signed account entry: structural decode → content-addressed device resolution →
/// signature verify → genesis self-hash → `INSERT OR IGNORE` into the candidate DAG → promote any
/// pre-verify rows this entry now resolves → `refold_account`. Opens its own IMMEDIATE transaction.
pub(crate) fn account_ingest(
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

    // One IMMEDIATE transaction spans resolution → park-or-store → promote → refold. The write
    // lock is what makes the park decision race-free: a concurrent ingest on another connection
    // cannot commit a resolving DeviceAdd between our device-set read and our pre-verify insert,
    // so an entry is never parked after its only promotion trigger has already passed.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    // Resolve the signer's ed25519 pubkey content-addressedly: from stored genesis/DeviceAdd
    // candidates for this account, plus THIS entry if it self-certifies (a genesis / a DeviceAdd of
    // its own signer). Unresolvable ⇒ pre-verify queue (durably, under the same lock).
    let mut pubkeys = stored_device_pubkeys(&tx, account_id)?;
    add_self_pubkey(&mut pubkeys, &signed.header, &signed.payload);
    let Some(pubkey_bytes) = pubkeys.get(&device_fp).copied() else {
        insert_pre_verify(&tx, &signed.entry_hash, account_id, device_fp, signed_bytes, now_ms)?;
        tx.commit()?;
        return Ok(IngestOutcome::PreVerify);
    };

    // Verify the signature under the resolved key (also re-binds fingerprint == sha256(pk)). A
    // failure here has written nothing, so dropping the txn rolls back cleanly.
    let Ok(pubkey) = DevicePublic::from_bytes(&pubkey_bytes) else {
        return Ok(IngestOutcome::Rejected("resolved device key is not a valid point".into()));
    };
    let verified = match envelope::verify_account_signed(signed_bytes, &pubkey) {
        Ok(verified) => verified,
        Err(err) => return Ok(IngestOutcome::Rejected(err.to_string())),
    };
    if let Err(err) = validate_authenticated_entry(&verified) {
        return Ok(IngestOutcome::Rejected(err));
    }

    if insert_candidate(&tx, &verified, signed_bytes, now_ms)? == CandidateInsert::AtCapacity {
        return Ok(IngestOutcome::Rejected("account candidate limit reached".into()));
    }
    // A DeviceAdd/genesis may resolve devices that were parked — retry their pre-verify rows.
    if is_genesis(&verified.header) || is_device_add(&verified) {
        promote_pre_verify(&tx, account_id, now_ms)?;
    }
    let status = refold_in_tx(&tx, account_id)?;
    tx.commit()?;
    Ok(IngestOutcome::Ingested {
        status: status.get(&verified.entry_hash).cloned().unwrap_or_else(|| "unknown".into()),
    })
}

/// Re-derive the whole account: fold the candidate set, resolve accepted-slot uniqueness (branch
/// selection §16.2), and rewrite `accepted` + `account_entry_status` in one IMMEDIATE transaction
/// so the `account_accepted_slot` partial unique index (I10a) never transiently double-accepts a
/// slot.
pub(super) fn refold_account(conn: &Connection, account_id: AccountId) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    refold_in_tx(&tx, account_id)?;
    tx.commit()?;
    Ok(())
}

/// The refold body (caller owns the txn). Returns each entry_hash → its projected status.
fn refold_in_tx(
    tx: &Transaction<'_>,
    account_id: AccountId,
) -> anyhow::Result<HashMap<[u8; 32], String>> {
    let rows = load_candidates(tx, account_id)?;
    let projection = derive_account_projection(&rows);

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
    Ok(statuses)
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

fn insert_candidate(
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
        return Ok(CandidateInsert::AtCapacity);
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
    let mut stmt = conn.prepare(
        "SELECT signed_bytes FROM account_entries WHERE account_id = ?1 AND entry_type IN (?2, ?3)",
    )?;
    let rows = stmt
        .query_map(
            params![
                account_id.to_bytes().as_slice(),
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

fn insert_pre_verify(
    conn: &Connection,
    entry_hash: &[u8; 32],
    account_id: AccountId,
    fingerprint: DeviceFingerprint,
    signed_bytes: &[u8],
    now_ms: i64,
) -> rusqlite::Result<()> {
    let signed_hash = cbor::sha256(signed_bytes);
    conn.execute(
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
    enforce_pre_verify_budget(conn, account_id)?;
    Ok(())
}

/// Keep the unauthenticated queue within deterministic oldest-first account and global budgets.
/// Ties use `signed_hash`, so replicas with the same rows and timestamps retain the same set.
fn enforce_pre_verify_budget(conn: &Connection, account_id: AccountId) -> rusqlite::Result<()> {
    evict_oldest_pre_verify(conn, Some(account_id), PRE_VERIFY_PER_ACCOUNT_MAX)?;
    evict_oldest_pre_verify(conn, None, PRE_VERIFY_GLOBAL_MAX)
}

fn evict_oldest_pre_verify(
    conn: &Connection,
    account_id: Option<AccountId>,
    limit: usize,
) -> rusqlite::Result<()> {
    let account_bytes = account_id.map(AccountId::to_bytes);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM account_pre_verify
         WHERE (?1 IS NULL OR claimed_account_id = ?1)",
        params![account_bytes.as_ref().map(<[u8; 32]>::as_slice)],
        |row| row.get(0),
    )?;
    if count <= limit as i64 {
        return Ok(());
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
    )?;
    Ok(())
}

/// Retry every pre-verify row for the account against the now-larger device set: a row whose signer
/// resolves is verified + promoted into `account_entries` and cleared; the rest stay parked.
fn promote_pre_verify(
    tx: &Transaction<'_>,
    account_id: AccountId,
    now_ms: i64,
) -> anyhow::Result<()> {
    // Fixpoint: a promoted genesis/DeviceAdd enlarges the resolvable device set, which can in turn
    // resolve a DEEPER parked entry (a device chain — founder→B→C — delivered before its
    // authorizers). Feed each promoted key back and re-scan until a full pass promotes nothing.
    // Snapshotting the device set once would strand depth≥2 chains forever, so two peers that
    // received the same entries in different orders would converge on different accepted sets.
    let mut pubkeys = stored_device_pubkeys(tx, account_id)?;
    loop {
        let pending: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = {
            let mut stmt = tx.prepare(
                "SELECT signed_hash, claimed_fingerprint, raw_bytes
                 FROM account_pre_verify WHERE claimed_account_id = ?1",
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
            if let Some(verified) = promoted {
                let inserted = insert_candidate(tx, &verified, &raw_bytes, now_ms)?;
                // A promoted genesis/DeviceAdd certifies a device key — feed it back so the next
                // round can resolve entries that were waiting on it.
                if inserted != CandidateInsert::AtCapacity {
                    add_self_pubkey(&mut pubkeys, &verified.header, &verified.payload);
                    promoted_any = true;
                }
            }
            // Resolved (promoted) or refuted (verify failed): clear it from the queue either way.
            tx.execute("DELETE FROM account_pre_verify WHERE signed_hash = ?1", params![
                signed_hash.as_slice()
            ])?;
        }
        // A round advances the queue only by promoting (each promotion deletes ≥1 pending row and
        // adds ≥1 key), so this terminates; a round that promotes nothing new is the fixpoint.
        if !promoted_any {
            break;
        }
    }
    Ok(())
}

fn is_genesis(header: &AccountEntryHeader) -> bool {
    header.entry_type == ops::entry_type::ACCOUNT_GENESIS
}

fn is_current_control_plaintext(header: &AccountEntryHeader) -> bool {
    header.log_id == fold::CONTROL_LOG
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

fn is_device_add(verified: &VerifiedAccountEntry) -> bool {
    verified.header.entry_type == ops::entry_type::DEVICE_ADD
}

/// A stored 32-byte column as a fixed array (errors, never panics, on a wrong-length blob).
fn fixed(bytes: &[u8]) -> anyhow::Result<[u8; 32]> {
    bytes.try_into().map_err(|_| anyhow::anyhow!("stored hash is {} bytes, not 32", bytes.len()))
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
    use super::*;
    use crate::index::schema;
    use crate::oplog::account::envelope::sign_account_entry;
    use crate::oplog::account::ops::{DeviceRole, encode, entry_type_of};
    use crate::oplog::device::{DeviceSecret, DeviceX25519Secret};

    const NOW: i64 = 1_700_000_000_000;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
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
        let payload = encode(&op).unwrap();
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
        let payload = encode(op).unwrap();
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: signer.fp,
            seq,
            prev_hash: prev,
            parent_ref: None,
            entry_type: entry_type_of(op),
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

    fn device_remove(dev: &Dev, control_cut: super::super::cut::Cut) -> AccountOp {
        AccountOp::DeviceRemove {
            device_fingerprint: dev.fp,
            control_cut,
            secrets_cut: super::super::cut::Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        }
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

    fn status(conn: &Connection, hash: &[u8; 32]) -> Option<String> {
        entry_status(conn, hash).unwrap().map(|(s, _)| s)
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
        let payload = encode(&genesis_op).unwrap();
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
        let payload = encode(&op).unwrap();
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
    }

    #[test]
    fn candidate_ceiling_rejects_new_history_before_refold_work_grows_unbounded() {
        let conn = db();
        let founder = Dev::new(1);
        let (account_id, genesis_bytes, _) = genesis(&founder);
        let verified =
            envelope::verify_account_signed(&genesis_bytes, &founder.secret.public()).unwrap();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        for ordinal in 0..CANDIDATES_PER_ACCOUNT_MAX {
            let entry_hash = cbor::sha256(&ordinal.to_be_bytes());
            tx.execute(
                "INSERT INTO account_entries(
                     entry_hash, account_id, log_id, device_fingerprint, seq, entry_type,
                     accepted, signed_bytes, received_at_ms)
                 VALUES (?1, ?2, 0, ?3, ?4, 99, 0, X'00', ?4)",
                params![
                    entry_hash.as_slice(),
                    account_id.to_bytes().as_slice(),
                    founder.fp.to_bytes().as_slice(),
                    i64::try_from(ordinal).unwrap(),
                ],
            )
            .unwrap();
        }
        assert_eq!(
            insert_candidate(&tx, &verified, &genesis_bytes, NOW).unwrap(),
            CandidateInsert::AtCapacity,
        );
        tx.commit().unwrap();
        assert_eq!(
            account_ingest(&conn, &genesis_bytes, NOW).unwrap(),
            IngestOutcome::Rejected("account candidate limit reached".into()),
        );
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
        let payload = encode(&device_add(&b, DeviceRole::Member)).unwrap();
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
        let payload = encode(&device_add(&b, DeviceRole::Member)).unwrap();
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
        let payload = encode(&device_add(&b, DeviceRole::Owner)).unwrap();
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
        assert_eq!(
            account_ingest(&conn, &signed.signed_bytes, NOW).unwrap(),
            IngestOutcome::Ingested { status: "retained_unfolded".into() },
        );
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
}
