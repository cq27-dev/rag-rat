//! The in-tx `/3` content-author seam (sync phase C3.4b-i, #663).
//!
//! The local writer's counterpart to the `/1` trio in [`crate::store`]
//! (`author_in_tx` / `author_batch_in_tx` / `author_genesis_in_tx`): it authors a batch of
//! [`MemoryOp`]s as **owner-authored** `/3` content on one `/2` stream, inside the caller's
//! IMMEDIATE transaction, minting each entry from the local chain tail. It is the LOCAL-authoring
//! path, kept deliberately distinct from [`super::storage::content_ingest`] — the REMOTE-input path
//! — which self-transacts, is §18b quota-capped, and refolds per entry. Local authoring stays
//! linear (§16.2): a single local writer from the accepted tail, quota-free, one refold per batch.
//!
//! OWNER-AUTHORED. The store's local account (author) is also the owner of its `/2` streams, so
//! every entry carries `grant_id = None` and `roster_ref` = the account's own genesis entry hash
//! (the roster the founder device is enrolled under). `owner_auth_len == author_auth_len ==` the
//! account's current control-fold `effective_count`, read in the SAME snapshot as authoring —
//! citing our own current fold length means our entries never park `auth_len_ahead` against our own
//! fold.
//!
//! `lamport = seq`. The seq is dense and monotone under the single local writer, and it IS the
//! projection LWW key ([`crate::project`] orders on `(lamport, device)`); a non-monotone
//! value would let a `NodeUpdate` lose to its own earlier `NodeCreate`.
//!
//! VERIFY-ACCEPTED. After the single batch refold, the seam reads back each authored entry's status
//! and `bail!`s if any is not `accepted`, so the whole batch — and the mutation that triggered it —
//! rolls back. A local entry CAN park/declassify (a missing `StreamOwn`, a stale `auth_len`, a
//! contested account), and a silently-stored unaccepted entry would desync the candidate tail from
//! the accepted tail and self-fork the next author's seq. Enforcing accept-or-rollback INDUCES the
//! invariant that no unaccepted local candidate ever survives a commit — which is exactly why
//! minting from the plain candidate tail (below) is the accepted tail.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::super::bootstrap::{self, LocalAccountRef};
use super::super::limits::CONTENT_ENVELOPE_MAX_BYTES;
use super::super::{AccountId, storage as account_storage};
use super::envelope::{self, ContentEntryHeader, VerifiedContentEntry};
use super::storage as content_storage;
use crate::op::{self, DeviceFingerprint, MemoryOp};
use crate::stream::StreamId;
use crate::{content_projection, local_device};

type EntryHash = [u8; 32];

/// The PROVEN worst-case byte overhead a signed `/3` content entry adds around an op body —
/// `signed_bytes.len() - payload.len()` maximized over every header field value and every payload
/// size class, derived directly from `envelope::sign_content_entry` / `encode_header` (NOT a
/// guessed margin, so it cannot silently drift).
/// `content_entry_max_overhead_bounds_the_real_signed_envelope` pins it against the real encoders.
///
/// The sum is computed from named parts so the compiler checks the arithmetic and the breakdown is
/// legible: CBOR encodes a 32-byte bstr as a 2-byte prefix + 32; a `u64` as at most 1 + 8; an
/// `n`-byte str as its prefix + `n`. Every optional-hash field counts as PRESENT (34 B) — its
/// widest form, which upper-bounds the null form (1 B) unconditionally, so the constant holds
/// regardless of the header's nullity coupling.
const CONTENT_ENTRY_MAX_OVERHEAD_BYTES: usize = {
    // `encode_header`: the 13-part `rag-rat/entry/3` array, every field at its MAX CBOR width — an
    // unconditional upper bound on the header bytes.
    const HEADER_MAX: usize = 1                 // array(13) head
        + (1 + 15)                              // domain str "rag-rat/entry/3"
        + (2 + 32) * 3                          // stream_id, author_account_id, device_fingerprint
        + (1 + 8) * 2                           // seq, lamport
        + (2 + 32) * 2                          // prev_hash, grant_id (present ≥ null)
        + (2 + 32)                              // roster_ref
        + (1 + 8) * 3                           // owner_auth_len, author_auth_len, crypto_suite
        + (2 + 32); // key_id (present ≥ null)
    // `encode_body` = cbor([header_bytes, payload]).
    const BODY_FRAMING: usize = 1               // array(2) head
        + 3                                     // header_bytes bstr prefix (HEADER_MAX = 300 ⇒ 0x59+2)
        + 5; // payload bstr prefix (a ~256 KiB body ⇒ 0x5a+4)
    // `encode_signed` = cbor([domain, body_bytes, signature]).
    const SIGNED_FRAMING: usize = 1             // array(3) head
        + (1 + 22)                              // domain str "rag-rat/signed-entry/1"
        + 5                                     // body_bytes bstr prefix (~256 KiB ⇒ 0x5a+4)
        + (2 + 64); // signature bstr
    HEADER_MAX + BODY_FRAMING + SIGNED_FRAMING // = 300 + 9 + 95 = 404
};

/// The largest op BODY (canonical CBOR) that always fits inside a signed `/3` content entry: the
/// §18a `CONTENT_ENVELOPE_MAX_BYTES` cap minus the PROVEN worst-case envelope overhead
/// ([`CONTENT_ENTRY_MAX_OVERHEAD_BYTES`]). A body at or under this bound signs to at most exactly
/// `CONTENT_ENVELOPE_MAX_BYTES`, so it clears both of `sign_content_entry`'s size checks. The prior
/// loose 1 KiB margin here permanently quarantined rows between `CAP - 1024` and the real limit
/// that `sign_content_entry` would in fact accept — legitimate imported data left unprojected
/// forever; this exact bound is a true lower-bound-safe mirror of the sign-time check (#680).
const CONTENT_OP_BODY_MAX_BYTES: usize =
    CONTENT_ENVELOPE_MAX_BYTES - CONTENT_ENTRY_MAX_OVERHEAD_BYTES;

/// Whether `op` can be authored as a `/3` content entry without exceeding the §18a envelope cap.
///
/// The local reconcile uses this to QUARANTINE a row whose op is un-authorable (an oversized
/// raw/imported memory body or payload) — skipping it instead of `bail!`ing the whole batch — so
/// one bad row can never wedge every other memory write; the write path uses the same predicate at
/// the create/update boundary to reject oversized input before the row is persisted (#680). Checks
/// the encoded body only; the header + signature are the fixed overhead `CONTENT_OP_BODY_MAX_BYTES`
/// already reserves for.
pub fn content_op_is_authorable(op: &MemoryOp) -> bool {
    op::encode(op).len() <= CONTENT_OP_BODY_MAX_BYTES
}

/// The `/3` chain tail for one `(stream, author, device)` coordinate: its highest-`seq` entry.
struct ContentChainTail {
    seq: u64,
    entry_hash: EntryHash,
}

/// Author `ops` as owner-authored `/3` content on `stream_id` WITHIN the caller's transaction:
/// chain each entry from the current tail (genesis when the chain is empty), insert it as a
/// candidate, refold the stream ONCE, then verify every entry folded `accepted` — else `bail!` so
/// the whole batch rolls back. Neither opens nor commits the txn. Returns the authored entry hashes
/// in authoring order. Requires the store's local account to be minted already (see
/// [`bootstrap::local_account`]); the caller mints it before opening this txn (that mint
/// self-transacts and cannot nest here).
pub fn author_content_batch_in_tx(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Vec<EntryHash>> {
    // Owner-authored: the store's single local account is both author and owner. Resolve it (and
    // its genesis entry hash, the `roster_ref`) from the pointer WITHOUT minting — the account
    // must already exist.
    let LocalAccountRef { account_id, genesis_hash } = bootstrap::local_account_ref(tx)?.context(
        "cannot author /3 content before the store's local account is minted (call local_account \
         first)",
    )?;
    let device = local_device(tx, now_ms)?;
    let fingerprint = device.fingerprint();
    // The freshness seam, read in THIS snapshot (see the module header): cite our own current
    // effective control-fold length as both auth_len fields.
    let auth_len = account_storage::account_effective_count(tx, account_id)?;

    let mut authored = Vec::with_capacity(ops.len());
    for op in ops {
        // Mint from the candidate tail. Under verify-accepted+rollback the candidate tail IS the
        // accepted tail, so no separate accepted-tail read is needed; the in-txn read sees the
        // entries this loop already inserted, so each op chains off the one before it.
        let (seq, prev_hash) = match content_chain_tail(tx, stream_id, account_id, fingerprint)? {
            Some(tail) => (
                tail.seq
                    .checked_add(1)
                    .context("/3 content chain tail is at u64::MAX seq; cannot extend")?,
                Some(tail.entry_hash),
            ),
            None => (0, None),
        };
        let header = ContentEntryHeader {
            stream_id,
            author_account_id: account_id,
            device_fingerprint: fingerprint,
            seq,
            // Monotone with seq — it is the projection LWW key (see the module header).
            lamport: seq,
            prev_hash,
            // Owner-authored: author == owner, so no delegated grant.
            grant_id: None,
            roster_ref: genesis_hash,
            owner_auth_len: auth_len,
            author_auth_len: auth_len,
            crypto_suite: 0,
            key_id: None,
        };
        // The `/3` body is the op's canonical CBOR verbatim (an opaque bstr the projection later
        // `op::decode`s). No `candidate_capacity` check: that is the §18b remote-abuse budget, not
        // a local-authoring bound.
        let payload = op::encode(op);
        let signed = envelope::sign_content_entry(device.secret(), &header, &payload)?;
        let verified = VerifiedContentEntry {
            header: signed.header,
            payload: signed.payload,
            header_bytes: signed.header_bytes,
            entry_hash: signed.entry_hash,
        };
        content_storage::insert_candidate(tx, &verified, &signed.signed_bytes, now_ms)?;
        authored.push(verified.entry_hash);
    }

    // ONE authority+branch refold for the whole batch (§16.2), the only writer of `accepted = 1`.
    content_storage::refold_content_stream(tx, stream_id)?;

    // verify-accepted: an owner authoring on its own stream accepts, so anything else means an
    // authority gap (missing `StreamOwn`, stale `auth_len`, contested account). Roll the batch back
    // rather than leave an unaccepted local candidate the next author's seq would collide with.
    for entry_hash in &authored {
        match content_status(tx, entry_hash)?.as_deref() {
            Some("accepted") => {},
            other => anyhow::bail!(
                "authored /3 content entry did not fold accepted (status {other:?}); rolling back \
                 the batch",
            ),
        }
    }

    // Acceptance changed on this stream → refresh its accepted-/3 → memory projection in the same
    // txn (the memory-layer fold that decodes op bodies; the acceptance layer is body-agnostic).
    content_projection::reproject_accepted_content_stream(tx, stream_id)?;

    Ok(authored)
}

/// Whether the `/2` stream's `/3` content chain is EMPTY — no `content_entries` row on it at all.
/// Under the single local writer the store's own account+device are the only chain on the stream,
/// so "no rows for this stream" is the whole chain: the genesis case where the memory reconcile
/// elides a create-time `active` status (a fresh chain holds no stale status register to override).
/// A pure read opening no transaction, so it is safe inside the caller's IMMEDIATE txn (a
/// `&Transaction` derefs to `&Connection`).
pub fn content_stream_is_empty(conn: &Connection, stream_id: StreamId) -> anyhow::Result<bool> {
    let has_row: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM content_entries WHERE stream_id = ?1)",
        params![stream_id.to_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(!has_row)
}

/// The `(stream, author, device)` chain's highest-`seq` `/3` candidate, or `None` for an empty
/// chain (→ genesis: seq 0, no predecessor). `seq` is stored as an 8-byte big-endian blob, so a
/// blob `ORDER BY seq DESC` compares byte-wise and is numerically correct for the fixed width.
fn content_chain_tail(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    author_account_id: AccountId,
    device_fingerprint: DeviceFingerprint,
) -> anyhow::Result<Option<ContentChainTail>> {
    let row: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT seq, entry_hash FROM content_entries
             WHERE stream_id = ?1 AND author_account_id = ?2 AND device_fingerprint = ?3
             ORDER BY seq DESC LIMIT 1",
            params![
                stream_id.to_bytes().as_slice(),
                author_account_id.to_bytes().as_slice(),
                device_fingerprint.to_bytes().as_slice(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(seq, entry_hash)| {
        let seq = u64::from_be_bytes(fixed::<8>(&seq)?);
        Ok(ContentChainTail { seq, entry_hash: fixed::<32>(&entry_hash)? })
    })
    .transpose()
}

/// The current `/3` status of one entry, or `None` if the refold wrote no status row for it.
fn content_status(
    tx: &Transaction<'_>,
    entry_hash: &EntryHash,
) -> rusqlite::Result<Option<String>> {
    tx.query_row(
        "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
        [entry_hash.as_slice()],
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
    use rag_rat_query::memory::EdgeRelation;
    use rusqlite::{Connection, TransactionBehavior};

    use super::*;
    use crate::op::{EdgeSpec, NodeContent, NodeId};

    const NOW: i64 = 1_700_000_000_000;
    const STREAM_A: [u8; 32] = [0x44; 32];
    const STREAM_B: [u8; 32] = [0x55; 32];

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    /// Mint the store's local account, then seed the `StreamOwn` fact for `stream` the way the C3.3
    /// acceptance tests do — `local_account` already folds the founder's roster fact (role owner)
    /// and the `account_auth_state` freshness row, so ownership is the only fact left to seed for
    /// an owner-authored entry to accept. Returns the local `account_id`.
    fn owned_stream_account(conn: &Connection, stream: StreamId) -> AccountId {
        let account_id = bootstrap::local_account(conn, NOW).expect("mint local account");
        seed_ownership(conn, stream, account_id);
        account_id
    }

    fn seed_ownership(conn: &Connection, stream: StreamId, owner: AccountId) {
        conn.execute(
            "INSERT INTO account_stream_ownership(stream_id, account_id, own_id, effective_at)
             VALUES(?1, ?2, ?3, 1)",
            params![
                stream.to_bytes().as_slice(),
                owner.to_bytes().as_slice(),
                [0x66_u8; 32].as_slice()
            ],
        )
        .unwrap();
    }

    fn content(title: &str) -> NodeContent {
        NodeContent {
            kind: "Invariant".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            source: "agent".to_string(),
            tags: Vec::new(),
            payload: None,
        }
    }

    fn node_create(id: &str, title: &str) -> MemoryOp {
        MemoryOp::NodeCreate { node_id: NodeId::from(id), content: content(title) }
    }

    fn node_update(id: &str, title: &str) -> MemoryOp {
        MemoryOp::NodeUpdate { node_id: NodeId::from(id), content: content(title) }
    }

    #[test]
    fn content_op_is_authorable_agrees_with_the_query_side_write_caps() {
        use rag_rat_query::memory::{
            MAX_EDGE_ANCHOR_LEN, MAX_MEMORY_BODY_LEN, MAX_MEMORY_PAYLOAD_LEN, MAX_MEMORY_TITLE_LEN,
        };
        // A memory at EVERY query-side write cap must still fit the signed /3 envelope. Worst case
        // for the char-counted title/body is a 4-byte char, plus a max-byte payload — this pins the
        // two crates' caps consistent even though the dependency only flows oplog → query (#680).
        let wide = '𝄞'.to_string(); // 4 UTF-8 bytes
        let maxed = NodeContent {
            kind: "Invariant".to_string(),
            title: wide.repeat(MAX_MEMORY_TITLE_LEN),
            body: wide.repeat(MAX_MEMORY_BODY_LEN),
            confidence: "high".to_string(),
            source: "agent".to_string(),
            tags: Vec::new(),
            payload: Some("x".repeat(MAX_MEMORY_PAYLOAD_LEN)),
        };
        assert!(
            content_op_is_authorable(&MemoryOp::NodeCreate {
                node_id: NodeId::from("mem_max"),
                content: maxed,
            }),
            "a memory at every write cap must still be authorable",
        );
        // A body past the envelope cap is un-authorable — exactly what the reconcile quarantines.
        let oversized = NodeContent { body: "x".repeat(300 * 1024), ..content("t") };
        assert!(
            !content_op_is_authorable(&MemoryOp::NodeCreate {
                node_id: NodeId::from("mem_big"),
                content: oversized,
            }),
            "an oversized body exceeds the /3 envelope",
        );

        // The EDGE twin (#680): both free-form edge fields at the query-side cap — plus realistic
        // short source/owner ids — must still fit the signed /3 envelope, so `add_edge`'s write cap
        // can never mint an un-authorable EdgeAdd. This pins `MAX_EDGE_ANCHOR_LEN` consistent with
        // the oplog envelope bound even though the dependency only flows oplog → query.
        let maxed_edge = EdgeSpec {
            source_node_id: NodeId::from("mem_1700000000000_abcdef"),
            relation: EdgeRelation::DependsOn,
            target_repo_id: "r".repeat(MAX_EDGE_ANCHOR_LEN),
            target_kind: "node".to_string(),
            target_anchor: "a".repeat(MAX_EDGE_ANCHOR_LEN),
            owner_repo_id: "owner-repo-id".to_string(),
        };
        assert!(
            content_op_is_authorable(&MemoryOp::EdgeAdd { edge: maxed_edge }),
            "an edge with both free-form fields at the write cap must still be authorable",
        );
        // An anchor past the cap is un-authorable — exactly what the write cap rejects and the
        // reconcile quarantines.
        assert!(
            !content_op_is_authorable(&edge_add("mem_src", &"a".repeat(300 * 1024))),
            "an oversized edge anchor exceeds the /3 envelope",
        );
    }

    /// A `NodeCreate` on `id` whose canonical-CBOR body (`op::encode`) is EXACTLY `encoded_len`
    /// bytes — used to place an op precisely relative to the authorable bound. Within one CBOR
    /// text-length class each extra body char is exactly one extra encoded byte, so measuring the
    /// fixed op-envelope framing on a body already in the ~256 KiB (5-byte-prefix) class lets us
    /// size the real body to hit `encoded_len` on the nose.
    fn node_create_sized(id: &str, encoded_len: usize) -> MemoryOp {
        let build = |body: String| MemoryOp::NodeCreate {
            node_id: NodeId::from(id),
            content: NodeContent { body, ..content("t") },
        };
        let probe = 100_000;
        let framing = op::encode(&build("a".repeat(probe))).len() - probe;
        build("a".repeat(encoded_len - framing))
    }

    #[test]
    fn an_op_in_the_band_the_old_margin_over_quarantined_is_authorable_and_signs() {
        // #680 (P2b): the old body bound was `CAP - 1024`, but the real signed-entry overhead is
        // far smaller, so an op whose encoded body sat between `CAP - 1024` and the true
        // limit was permanently quarantined even though `sign_content_entry` would accept
        // it. Place an op squarely in that reclaimed band and prove BOTH that the predicate
        // now admits it AND that authoring it actually folds accepted — no `bail!`, no
        // wedge.
        let op = node_create_sized("mem_band", CONTENT_ENVELOPE_MAX_BYTES - 512);
        let encoded = op::encode(&op).len();
        assert_eq!(encoded, CONTENT_ENVELOPE_MAX_BYTES - 512, "the op is sized on the nose");
        assert!(
            encoded > CONTENT_ENVELOPE_MAX_BYTES - 1024,
            "the op sits in the band the old 1 KiB margin wrongly quarantined",
        );
        assert!(
            content_op_is_authorable(&op),
            "the reclaimed-band op is authorable under the exact overhead bound",
        );

        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        owned_stream_account(&conn, stream);
        let hashes = author_committed(&conn, stream, std::slice::from_ref(&op));
        assert_eq!(
            content_status(&conn.unchecked_transaction().unwrap(), &hashes[0]).unwrap().as_deref(),
            Some("accepted"),
            "the reclaimed-band op signs and folds accepted — the predicate did not \
             over-quarantine",
        );
    }

    #[test]
    fn an_op_one_byte_over_the_exact_bound_is_quarantined() {
        // Just past `CONTENT_OP_BODY_MAX_BYTES`: the predicate must return false. Returning true
        // here would let the `/3` author's §18a size check `bail!` on the whole batch — the
        // #680 wedge the quarantine exists to prevent.
        let op = node_create_sized("mem_over", CONTENT_OP_BODY_MAX_BYTES + 1);
        assert_eq!(op::encode(&op).len(), CONTENT_OP_BODY_MAX_BYTES + 1);
        assert!(!content_op_is_authorable(&op), "an op past the exact body bound is quarantined");
    }

    #[test]
    fn content_entry_max_overhead_bounds_the_real_signed_envelope() {
        // Drift guard: prove the hand-derived overhead constant against the REAL encoders. A
        // payload of exactly `CONTENT_OP_BODY_MAX_BYTES`, wrapped under the WIDEST header
        // `sign_content_entry` accepts, must sign and land at or under the §18a envelope cap — i.e.
        // the constant reserves enough headroom, and the measured overhead never exceeds it.
        let secret = crate::device::DeviceSecret::from_seed(&[9; 32]);
        // The widest signable header: crypto_suite 0 (⇒ key_id null), seq != 0 (⇒ prev_hash
        // present), a present grant_id, and all u64 fields maxed.
        let header = ContentEntryHeader {
            stream_id: StreamId::from_bytes([0x11; 32]),
            author_account_id: AccountId::from_bytes([0x22; 32]),
            device_fingerprint: secret.public().fingerprint(),
            seq: u64::MAX,
            lamport: u64::MAX,
            prev_hash: Some([0x33; 32]),
            grant_id: Some([0x44; 32]),
            roster_ref: [0x55; 32],
            owner_auth_len: u64::MAX,
            author_auth_len: u64::MAX,
            crypto_suite: 0,
            key_id: None,
        };
        // A canonical-CBOR payload of exactly `CONTENT_OP_BODY_MAX_BYTES` bytes — one bstr
        // (`0x5a` + 4-byte length + content), the same near-limit shape envelope.rs's own size test
        // uses.
        let mut payload = vec![0x5a];
        payload.extend_from_slice(&((CONTENT_OP_BODY_MAX_BYTES - 5) as u32).to_be_bytes());
        payload.resize(CONTENT_OP_BODY_MAX_BYTES, 0);
        let signed = envelope::sign_content_entry(&secret, &header, &payload)
            .expect("a body at the exact bound signs under the worst-case header");
        assert!(
            signed.signed_bytes.len() <= CONTENT_ENVELOPE_MAX_BYTES,
            "the derived overhead keeps the signed envelope within the §18a cap ({} > {})",
            signed.signed_bytes.len(),
            CONTENT_ENVELOPE_MAX_BYTES,
        );
        assert!(
            signed.signed_bytes.len() - payload.len() <= CONTENT_ENTRY_MAX_OVERHEAD_BYTES,
            "the real envelope overhead ({}) is within the derived worst-case constant ({})",
            signed.signed_bytes.len() - payload.len(),
            CONTENT_ENTRY_MAX_OVERHEAD_BYTES,
        );
    }

    fn edge_add(source: &str, anchor: &str) -> MemoryOp {
        MemoryOp::EdgeAdd {
            edge: EdgeSpec {
                source_node_id: NodeId::from(source),
                relation: EdgeRelation::DependsOn,
                target_repo_id: "repo".to_string(),
                target_kind: "node".to_string(),
                target_anchor: anchor.to_string(),
                owner_repo_id: "repo".to_string(),
            },
        }
    }

    /// Run the in-tx seam in its own IMMEDIATE txn and commit — the shape a live mutation uses.
    fn author_committed(conn: &Connection, stream: StreamId, ops: &[MemoryOp]) -> Vec<EntryHash> {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let hashes = author_content_batch_in_tx(&tx, stream, ops, NOW).expect("author batch");
        tx.commit().unwrap();
        hashes
    }

    fn genesis_ref(conn: &Connection) -> EntryHash {
        conn.query_row(
            "SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map(|bytes| fixed::<32>(&bytes).unwrap())
        .unwrap()
    }

    fn tail(conn: &Connection, stream: StreamId, account: AccountId) -> Option<ContentChainTail> {
        let fingerprint = local_device(conn, NOW).unwrap().fingerprint();
        let tx = conn.unchecked_transaction().unwrap();
        content_chain_tail(&tx, stream, account, fingerprint).unwrap()
    }

    fn stored_status(conn: &Connection, entry_hash: &EntryHash) -> Option<String> {
        conn.query_row(
            "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
            [entry_hash.as_slice()],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
    }

    fn header_of(conn: &Connection, entry_hash: &EntryHash) -> ContentEntryHeader {
        let signed_bytes: Vec<u8> = conn
            .query_row(
                "SELECT signed_bytes FROM content_entries WHERE entry_hash = ?1",
                [entry_hash.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        envelope::decode_content_signed(&signed_bytes).unwrap().header
    }

    #[test]
    fn a_batch_authors_owner_content_that_folds_accepted_with_dense_seqs_and_lamport_eq_seq() {
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        let account = owned_stream_account(&conn, stream);

        let hashes = author_committed(&conn, stream, &[
            node_create("n1", "first"),
            node_update("n1", "second"),
        ]);
        assert_eq!(hashes.len(), 2, "two ops author two entries");

        // Every authored entry folds accepted.
        for entry_hash in &hashes {
            assert_eq!(stored_status(&conn, entry_hash).as_deref(), Some("accepted"));
            let (accepted,): (i64,) = conn
                .query_row(
                    "SELECT accepted FROM content_entries WHERE entry_hash = ?1",
                    [entry_hash.as_slice()],
                    |row| Ok((row.get(0)?,)),
                )
                .unwrap();
            assert_eq!(accepted, 1, "the entry carries accepted = 1");
        }

        // Dense seqs 0, 1 with lamport == seq (the projection LWW key).
        for (ordinal, entry_hash) in hashes.iter().enumerate() {
            let header = header_of(&conn, entry_hash);
            assert_eq!(header.seq, ordinal as u64, "seqs are dense from 0");
            assert_eq!(header.lamport, header.seq, "lamport == seq");
            assert_eq!(header.grant_id, None, "owner-authored: no grant");
            assert_eq!(header.roster_ref, genesis_ref(&conn), "roster_ref is the genesis hash");
            assert_eq!(header.author_account_id, account, "authored under the local account");
        }

        // The tail advanced to the highest seq.
        let advanced = tail(&conn, stream, account).expect("non-empty chain has a tail");
        assert_eq!(advanced.seq, 1, "the tail advanced to seq 1");
        assert_eq!(advanced.entry_hash, hashes[1], "the tail names the last authored entry");
    }

    #[test]
    fn the_projection_fold_materializes_the_accepted_dag_keyed_by_stream() {
        let conn = db();
        let stream_a = StreamId::from_bytes(STREAM_A);
        let account = owned_stream_account(&conn, stream_a);

        // A NodeCreate/NodeUpdate/EdgeAdd batch on stream A: the update (seq 1, higher lamport)
        // wins the node content register over the create (seq 0).
        author_committed(&conn, stream_a, &[
            node_create("n1", "created"),
            node_update("n1", "updated"),
            edge_add("n1", "n2"),
        ]);

        let (content_json, status): (String, String) = conn
            .query_row(
                "SELECT content_json, status FROM content_projected_nodes
                 WHERE stream_id = ?1 AND node_id = ?2",
                params![stream_a.to_bytes().as_slice(), "n1"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("stream A projects node n1");
        assert!(content_json.contains("updated"), "the NodeUpdate content wins the LWW register");
        assert!(!content_json.contains("created"), "the superseded create content is gone");
        assert_eq!(status, "active", "default node status");

        let edge_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_projected_edges WHERE stream_id = ?1",
                params![stream_a.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edge_rows, 1, "the EdgeAdd projects one present edge");

        // A SECOND stream (same local owner) authors its own node n1 — the stream-keying must keep
        // the two projections from colliding.
        let stream_b = StreamId::from_bytes(STREAM_B);
        seed_ownership(&conn, stream_b, account);
        author_committed(&conn, stream_b, &[node_create("n1", "b-only")]);

        let b_content: String = conn
            .query_row(
                "SELECT content_json FROM content_projected_nodes
                 WHERE stream_id = ?1 AND node_id = ?2",
                params![stream_b.to_bytes().as_slice(), "n1"],
                |row| row.get(0),
            )
            .expect("stream B projects its own node n1");
        assert!(b_content.contains("b-only"), "stream B keeps its own content");

        // Stream A's node n1 is untouched by stream B's authoring, and stream B has no edge rows.
        let a_content_after: String = conn
            .query_row(
                "SELECT content_json FROM content_projected_nodes
                 WHERE stream_id = ?1 AND node_id = ?2",
                params![stream_a.to_bytes().as_slice(), "n1"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            a_content_after.contains("updated"),
            "stream A's projection did not collide with B"
        );
        let b_edges: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_projected_edges WHERE stream_id = ?1",
                params![stream_b.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(b_edges, 0, "stream B authored no edges");
    }

    #[test]
    fn a_withheld_stream_own_makes_the_batch_roll_back_with_no_content_stored() {
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        // Mint the account but DO NOT seed ownership: with no `StreamOwn` fact the stream
        // declassifies, the entries never accept, and verify-accepted must roll the whole batch
        // back. Without verify-accepted this would silently store unaccepted /3 candidates.
        let _account = bootstrap::local_account(&conn, NOW).expect("mint local account");

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let result = author_content_batch_in_tx(&tx, stream, &[node_create("n1", "first")], NOW);
        assert!(result.is_err(), "verify-accepted rejects an unaccepted batch");
        drop(tx); // no commit → the IMMEDIATE txn rolls back

        let stored: i64 =
            conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();
        assert_eq!(stored, 0, "the rolled-back batch stored no /3 entry");
        let projected: i64 = conn
            .query_row("SELECT count(*) FROM content_projected_nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(projected, 0, "no projection row survives the rollback");
    }

    #[test]
    fn the_projection_skips_an_accepted_row_whose_body_is_not_a_decodable_op() {
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        let account = owned_stream_account(&conn, stream);
        // A good accepted entry via the seam.
        author_committed(&conn, stream, &[node_create("n1", "good")]);

        // Inject a second accepted /3 row on the same stream whose signed ENVELOPE is valid but
        // whose BODY is canonical CBOR that is not a `MemoryOp` — the shape a foreign entry could
        // take, since the acceptance layer (§8) never decodes the body. `content_ingest` would have
        // accepted it; the projection must SKIP it, not `bail!` and crash every later local author.
        let device = local_device(&conn, NOW).unwrap();
        let header = ContentEntryHeader {
            stream_id: stream,
            author_account_id: account,
            device_fingerprint: device.fingerprint(),
            seq: 99,
            lamport: 99,
            prev_hash: Some([0xab; 32]),
            grant_id: None,
            roster_ref: genesis_ref(&conn),
            owner_auth_len: 0,
            author_auth_len: 0,
            crypto_suite: 0,
            key_id: None,
        };
        // A bare canonical CBOR integer: decodes as CBOR, but is not the op envelope ⇒ `op::decode`
        // errors (the path the fix must tolerate).
        let signed = envelope::sign_content_entry(device.secret(), &header, &[0x01]).unwrap();
        conn.execute(
            "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?8, 1, ?9, 0)",
            params![
                signed.entry_hash.as_slice(),
                stream.to_bytes().as_slice(),
                account.to_bytes().as_slice(),
                device.fingerprint().to_bytes().as_slice(),
                99_u64.to_be_bytes().as_slice(),
                [0xab_u8; 32].as_slice(),
                genesis_ref(&conn).as_slice(),
                0_u64.to_be_bytes().as_slice(),
                signed.signed_bytes,
            ],
        )
        .unwrap();

        // Reproject the stream: it must NOT error, and only the good node projects.
        let tx = conn.unchecked_transaction().unwrap();
        content_projection::reproject_accepted_content_stream(&tx, stream)
            .expect("an undecodable body is skipped, not fatal");
        tx.commit().unwrap();
        let node_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM content_projected_nodes WHERE stream_id = ?1",
                params![stream.to_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(node_count, 1, "only the decodable node projects; the bad body is skipped");
    }

    #[test]
    fn the_chain_tail_reader_reports_genesis_for_an_empty_chain_and_the_head_when_populated() {
        let conn = db();
        let stream = StreamId::from_bytes(STREAM_A);
        let account = owned_stream_account(&conn, stream);

        // Empty chain ⇒ None (the seam's genesis gate: seq 0, prev None).
        assert!(tail(&conn, stream, account).is_none(), "empty chain has no tail");

        let hashes =
            author_committed(&conn, stream, &[node_create("n1", "a"), node_create("n2", "b")]);

        let head = tail(&conn, stream, account).expect("populated chain has a tail");
        assert_eq!(head.seq, 1, "the tail seq is the highest authored seq");
        assert_eq!(head.entry_hash, hashes[1], "the tail names the highest-seq entry");
    }
}
