use rag_rat_db::schema;
use rag_rat_query::memory::EdgeRelation;
use rusqlite::{Connection, TransactionBehavior};

use super::*;
use crate::account::{account_ingest, ensure_owned_stream_v2_in_tx, local_account};
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

/// Both predicates must refuse a shape `op::decode` cannot read back, whatever its size — a
/// small unreadable op would otherwise be signed, accepted and forwarded, then silently skipped
/// at projection by every peer, its author included.
///
/// This pins the WIRING, not the primitive: dropping `within_wire_limits` from either predicate
/// must fail here. The sealed twin is asserted separately because it is a separate line that a
/// refactor can drop on its own.
#[test]
fn both_authorable_predicates_refuse_a_structurally_unreadable_op() {
    let anchor = |binding_id: &str| op::PortableAnchor {
        binding_kind: "symbol".to_string(),
        binding_id: binding_id.to_string(),
        path: Some("src/lib.rs".to_string()),
        start_line: Some(1),
        end_line: Some(2),
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        created_at_ms: 1,
        symbol_kind: None,
        signature_hash: None,
        moniker_tool: None,
        moniker_tool_version: None,
    };
    let anchors_op = |anchors: Vec<op::PortableAnchor>| MemoryOp::NodeAnchors {
        node_id: NodeId::from("mem_1"),
        anchors,
    };

    // Two anchors naming ONE binding: tiny, so every size bound passes.
    let duplicated = anchors_op(vec![anchor("src/lib.rs::run"), anchor("src/lib.rs::run")]);
    assert!(!content_op_is_authorable(&duplicated), "a duplicate binding identity is unreadable");
    assert!(
        !content_op_is_sealed_authorable(&duplicated),
        "sealing does not make a duplicate identity readable",
    );

    // One past the count cap, also far inside the byte bounds.
    let over_cap = anchors_op(
        (0..=op::MAX_ANCHORS_PER_OP).map(|index| anchor(&format!("id_{index:03}"))).collect(),
    );
    assert!(!content_op_is_authorable(&over_cap), "an over-cap anchor set is unreadable");
    assert!(!content_op_is_sealed_authorable(&over_cap), "sealing does not raise the anchor cap",);

    let clean = anchors_op(vec![anchor("src/lib.rs::run"), anchor("src/lib.rs::walk")]);
    assert!(content_op_is_authorable(&clean), "a well-formed anchor set is authorable");
    assert!(content_op_is_sealed_authorable(&clean), "and authorable on a sealed stream");
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
        "the reclaimed-band op signs and folds accepted — the predicate did not over-quarantine",
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
        prev_hash: Some(AccountEntryHash::from_bytes([0x33; 32])),
        grant_id: Some(GrantId::from_bytes([0x44; 32])),
        roster_ref: RosterRef::from_bytes([0x55; 32]),
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
fn author_committed(
    conn: &Connection,
    stream: StreamId,
    ops: &[MemoryOp],
) -> Vec<AccountEntryHash> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    let hashes = author_content_batch_in_tx(&tx, stream, ops, NOW).expect("author batch");
    tx.commit().unwrap();
    hashes
}

fn genesis_ref(conn: &Connection) -> AccountEntryHash {
    AccountEntryHash::from_bytes(
        conn.query_row(
            "SELECT genesis_entry_hash FROM oplog_local_account WHERE id = 0",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map(|bytes| fixed::<32>(&bytes).unwrap())
        .unwrap(),
    )
}

fn tail(conn: &Connection, stream: StreamId, account: AccountId) -> Option<ContentChainTail> {
    let fingerprint = local_device(conn, NOW).unwrap().fingerprint();
    let tx = conn.unchecked_transaction().unwrap();
    content_chain_tail(&tx, stream, account, fingerprint).unwrap()
}

fn stored_status(conn: &Connection, entry_hash: &AccountEntryHash) -> Option<String> {
    conn.query_row(
        "SELECT status FROM content_entry_status WHERE entry_hash = ?1",
        [entry_hash.as_slice()],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
}

fn header_of(conn: &Connection, entry_hash: &AccountEntryHash) -> ContentEntryHeader {
    let signed_bytes: Vec<u8> = conn
        .query_row(
            "SELECT signed_bytes FROM content_entries WHERE entry_hash = ?1",
            [entry_hash.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    envelope::decode_content_signed(&signed_bytes).unwrap().header
}

fn insert_accepted_signed(conn: &Connection, signed: &envelope::SignedContentEntry) {
    let header = &signed.header;
    conn.execute(
        "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, lamport, accepted,
                 signed_bytes, received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12, 0)",
        params![
            signed.entry_hash.as_slice(),
            header.stream_id.to_bytes().as_slice(),
            header.author_account_id.to_bytes().as_slice(),
            header.device_fingerprint.to_bytes().as_slice(),
            header.seq.to_be_bytes().as_slice(),
            header.prev_hash.as_ref().map(|hash| hash.as_slice()),
            header.grant_id.as_ref().map(|hash| hash.as_slice()),
            header.roster_ref.as_slice(),
            header.owner_auth_len.to_be_bytes().as_slice(),
            header.author_auth_len.to_be_bytes().as_slice(),
            i64::try_from(header.lamport).unwrap_or(i64::MAX),
            signed.signed_bytes.as_slice(),
        ],
    )
    .unwrap();
}

#[test]
fn a_second_owner_batch_continues_the_stream_lamport_clock() {
    // The stream-global clock (#1164): a later batch mints lamports above the stream's current
    // max, not from a reset — so a second writer authoring later always orders after. For a
    // single writer this stays equal to `seq`.
    let conn = db();
    let stream = StreamId::from_bytes(STREAM_A);
    let _account = owned_stream_account(&conn, stream);

    let first = author_committed(&conn, stream, &[node_create("n1", "a"), node_create("n2", "b")]);
    let second = author_committed(&conn, stream, &[node_create("n3", "c")]);

    assert_eq!(header_of(&conn, &first[0]).lamport, 0);
    assert_eq!(header_of(&conn, &first[1]).lamport, 1);
    let third = header_of(&conn, &second[0]);
    assert_eq!(third.lamport, 2, "the second batch continues from the stream max, not a reset");
    assert_eq!(third.seq, 2, "for a single writer lamport still tracks seq");
}

#[test]
fn a_batch_authors_owner_content_that_folds_accepted_with_dense_seqs_and_lamport_eq_seq() {
    let conn = db();
    let stream = StreamId::from_bytes(STREAM_A);
    let account = owned_stream_account(&conn, stream);

    let hashes =
        author_committed(&conn, stream, &[node_create("n1", "first"), node_update("n1", "second")]);
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
        assert_eq!(header.roster_ref, genesis_ref(&conn).into(), "roster_ref is the genesis hash");
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
    assert!(a_content_after.contains("updated"), "stream A's projection did not collide with B");
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
        prev_hash: Some(AccountEntryHash::from_bytes([0xab; 32])),
        grant_id: None,
        roster_ref: genesis_ref(&conn).into(),
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

// ── #688: the /3 projector-version two-part discipline ──

/// The stamp these tests expect after an upgrade: the CURRENT projector version, not a literal.
/// The property under test is "the store became current", so a projector bump should not have
/// to touch them.
fn current_stamp() -> String {
    content_projection::CONTENT_PROJECTOR_VERSION.to_string()
}

/// The stored `/3` content-projector stamp, as the raw string `oplog_meta` holds.
fn stored_stamp(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM oplog_meta WHERE key = 'content_projector_version'",
        [],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
}

fn projected_node_count(conn: &Connection, stream: StreamId) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM content_projected_nodes WHERE stream_id = ?1",
        params![stream.to_bytes().as_slice()],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn the_upgrade_refold_rebuilds_every_stream_then_stamps_once() {
    let conn = db();
    let stream_a = StreamId::from_bytes(STREAM_A);
    let stream_b = StreamId::from_bytes(STREAM_B);
    let account = owned_stream_account(&conn, stream_a);
    author_committed(&conn, stream_a, &[node_create("na", "on a")]);
    seed_ownership(&conn, stream_b, account);
    author_committed(&conn, stream_b, &[node_create("nb", "on b")]);
    assert_eq!(projected_node_count(&conn, stream_a), 1);
    assert_eq!(projected_node_count(&conn, stream_b), 1);

    // Simulate an old binary's fold: stale projection rows everywhere, an older stamp, plus a
    // stray row on a stream whose accepted set is now empty (it must not linger).
    conn.execute_batch(
        "DELETE FROM content_projected_nodes;
             DELETE FROM content_projected_edges;
             INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
              VALUES(X'99', 'ghost', '{}', 'active');
             INSERT INTO oplog_meta(key, value) VALUES ('content_projector_version', '1')
              ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
    )
    .unwrap();

    assert!(
        content_projection::rebuild_all_content_projections_if_stale(&conn).unwrap(),
        "a stale stamp re-folds"
    );
    assert_eq!(projected_node_count(&conn, stream_a), 1, "stream A rebuilt");
    assert_eq!(projected_node_count(&conn, stream_b), 1, "stream B rebuilt");
    let ghost: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_projected_nodes WHERE node_id = 'ghost'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ghost, 0, "the wholesale clear dropped the row with no accepted content");
    assert_eq!(stored_stamp(&conn), Some(current_stamp()), "the store-global stamp upgraded");

    assert!(
        !content_projection::rebuild_all_content_projections_if_stale(&conn).unwrap(),
        "a current stamp is a no-op"
    );
}

#[test]
fn the_per_stream_reproject_maintains_but_never_upgrades_the_stamp() {
    let conn = db();
    let stream = StreamId::from_bytes(STREAM_A);
    owned_stream_account(&conn, stream);

    // On a store the open-path trigger never ran (a raw connection), authoring reprojects the
    // stream but leaves the MISSING stamp untouched — the rebuild-all path is the only
    // upgrader.
    author_committed(&conn, stream, &[node_create("n1", "first")]);
    assert_eq!(projected_node_count(&conn, stream), 1);
    assert_eq!(stored_stamp(&conn), None, "a per-stream reproject never writes the first stamp");

    // An OLDER stamp is left untouched too: the per-stream path rebuilds this stream's rows
    // but does not mark the (possibly stale) store current.
    conn.execute(
        "INSERT INTO oplog_meta(key, value) VALUES ('content_projector_version', '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )
    .unwrap();
    conn.execute_batch("DELETE FROM content_projected_nodes;").unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    content_projection::reproject_accepted_content_stream(&tx, stream).unwrap();
    tx.commit().unwrap();
    assert_eq!(projected_node_count(&conn, stream), 1, "the stream's rows are rebuilt");
    assert_eq!(stored_stamp(&conn).as_deref(), Some("1"), "the v1 stamp is NOT upgraded");

    // The upgrade re-fold makes the store current; from then on per-stream reprojects
    // maintain the stamp.
    assert!(content_projection::rebuild_all_content_projections_if_stale(&conn).unwrap());
    assert_eq!(stored_stamp(&conn), Some(current_stamp()));
    author_committed(&conn, stream, &[node_create("n2", "second")]);
    assert_eq!(stored_stamp(&conn), Some(current_stamp()), "a current store stays current");
    assert_eq!(projected_node_count(&conn, stream), 2);
}

#[test]
fn the_per_stream_reproject_still_refuses_a_newer_stamp() {
    let conn = db();
    let stream = StreamId::from_bytes(STREAM_A);
    owned_stream_account(&conn, stream);
    author_committed(&conn, stream, &[node_create("n1", "first")]);

    // Simulate a NEWER binary having folded + stamped a higher content-projector version.
    conn.execute(
        "INSERT INTO oplog_meta(key, value) VALUES ('content_projector_version', '999')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )
    .unwrap();

    let tx = conn.unchecked_transaction().unwrap();
    assert!(
        content_projection::reproject_accepted_content_stream(&tx, stream).is_err(),
        "an older projector must not reproject a newer-folded store"
    );
    drop(tx);
    // The upgrade re-fold also leaves the newer projection intact (never downgrades).
    assert!(
        !content_projection::rebuild_all_content_projections_if_stale(&conn).unwrap(),
        "a newer stamp is not re-folded"
    );
    assert_eq!(stored_stamp(&conn).as_deref(), Some("999"), "the newer stamp stands");
}

#[test]
fn the_upgrade_refold_skips_a_store_before_the_projected_tables_exist() {
    // The pre-V070 mid-migration window: `content_projected_*` do not exist yet, so there is
    // nothing to rebuild and nothing to stamp against — skip cleanly (the #683 guard, reused).
    let conn = db();
    conn.execute_batch("DROP TABLE content_projected_nodes; DROP TABLE content_projected_edges;")
        .unwrap();
    assert!(
        !content_projection::rebuild_all_content_projections_if_stale(&conn).unwrap(),
        "a pre-V070 store is skipped"
    );
    assert_eq!(stored_stamp(&conn), None, "no stamp is written against absent tables");
}

#[test]
fn the_chain_tail_reader_reports_genesis_for_an_empty_chain_and_the_head_when_populated() {
    let conn = db();
    let stream = StreamId::from_bytes(STREAM_A);
    let account = owned_stream_account(&conn, stream);

    // Empty chain ⇒ None (the seam's genesis gate: seq 0, prev None).
    assert!(tail(&conn, stream, account).is_none(), "empty chain has no tail");

    let hashes = author_committed(&conn, stream, &[node_create("n1", "a"), node_create("n2", "b")]);

    let head = tail(&conn, stream, account).expect("populated chain has a tail");
    assert_eq!(head.seq, 1, "the tail seq is the highest authored seq");
    assert_eq!(head.entry_hash, hashes[1], "the tail names the highest-seq entry");
}

// ── C5a: the sealed suite-1 author seam ──

/// Mint the store's local account and publish an owned `/2` stream via the REAL ownership seam
/// (`ensure_owned_stream_v2_in_tx`) — the sealed mint needs a genuine effective `StreamOwn`,
/// not the direct-seeded `account_stream_ownership` shortcut the plaintext acceptance tests
/// use.
/// Author a create plus its anchor snapshot, then read the projection back through the public
/// reader — the real path (`author_content_batch` → acceptance → reproject), not a hand-built
/// `ProjectedState`, so this covers the fold, the serializer, the column and the decoder.
#[test]
fn an_authored_anchor_snapshot_projects_onto_its_node() {
    let conn = db();
    let (account, stream) = owned_v2(&conn);
    let anchors = vec![op::PortableAnchor {
        binding_kind: "symbol".to_string(),
        binding_id: "src/lib.rs::run".to_string(),
        path: Some("src/lib.rs".to_string()),
        start_line: Some(3),
        end_line: Some(9),
        commit_hash: Some("c0ffee".to_string()),
        tracker: None,
        project: None,
        item_key: None,
        created_at_ms: 42,
        symbol_kind: Some("function".to_string()),
        signature_hash: None,
        moniker_tool: None,
        moniker_tool_version: None,
    }];
    author_content_batch(
        &conn,
        stream,
        &[node_create("n1", "first"), MemoryOp::NodeAnchors {
            node_id: NodeId::from("n1"),
            anchors: anchors.clone(),
        }],
        SealPolicy::Plaintext,
        NOW,
    )
    .expect("author the create and its anchors");

    let nodes = crate::content_projection::list_projected_content_nodes(&conn, stream)
        .expect("read the projection");
    let node = nodes.iter().find(|node| node.node_id == "n1").expect("the node projects");
    assert_eq!(node.anchors.as_deref(), Some(anchors.as_slice()), "every field survives");
    assert_eq!(
        node.anchors_author,
        Some(account),
        "the projection records which account authored the winning anchor set",
    );

    // A projector rebuild re-derives the anchors from the RETAINED accepted entries rather than
    // from the projection rows it just cleared — the path an older binary's opaquely-retained
    // `node_anchors` entry takes when this binary first opens the store.
    conn.execute(
        "INSERT INTO oplog_meta(key, value) VALUES ('content_projector_version', '0')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )
    .unwrap();
    assert!(
        crate::content_projection::rebuild_all_content_projections_if_stale(&conn).unwrap(),
        "the aged stamp triggers a rebuild",
    );
    let rebuilt = crate::content_projection::list_projected_content_nodes(&conn, stream)
        .expect("read the rebuilt projection");
    let node = rebuilt.iter().find(|node| node.node_id == "n1").expect("the node still projects");
    assert_eq!(
        node.anchors.as_deref(),
        Some(anchors.as_slice()),
        "the rebuild recovers anchors from the accepted entries",
    );
    assert_eq!(node.anchors_author, Some(account), "and who authored them");

    // The other half of the truth table the nullable column exists for: an author publishing an
    // EMPTY set must read back as `Some(vec![])`, never NULL. Without this, normalizing an
    // empty vec to NULL in the writer — or a `skip_serializing_if` on the row DTO —
    // would pass the whole suite while collapsing the distinction the drain's seed rule
    // depends on.
    author_content_batch(
        &conn,
        stream,
        &[MemoryOp::NodeAnchors { node_id: NodeId::from("n1"), anchors: Vec::new() }],
        SealPolicy::Plaintext,
        NOW,
    )
    .expect("author an empty anchor set");
    let emptied = crate::content_projection::list_projected_content_nodes(&conn, stream).unwrap();
    assert_eq!(
        emptied.iter().find(|node| node.node_id == "n1").unwrap().anchors,
        Some(Vec::new()),
        "an empty set survives SQLite as Some(vec![]), never NULL",
    );
}

/// A node whose author never published an anchor set stays `None` — the projection must not
/// coerce "nobody said" into an empty set, since the drain reads that difference.
#[test]
fn a_node_with_no_anchor_op_projects_none() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    author_content_batch(&conn, stream, &[node_create("n1", "first")], SealPolicy::Plaintext, NOW)
        .expect("author the create");

    let nodes = crate::content_projection::list_projected_content_nodes(&conn, stream).unwrap();
    assert_eq!(nodes.iter().find(|node| node.node_id == "n1").unwrap().anchors, None);
}

fn owned_v2(conn: &Connection) -> (AccountId, StreamId) {
    let account = local_account(conn, NOW).expect("mint local account");
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    let stream = ensure_owned_stream_v2_in_tx(&tx, "repo-a", NOW).expect("publish owned /2 stream");
    tx.commit().unwrap();
    (account, stream)
}

/// Author a founder-signed control op (`DeviceAdd` / `DeviceRemove`) on the account's control
/// chain in its own IMMEDIATE txn, then refold — the roster-mutation shape the C4.4 rotation
/// tests use (mirrors `secrets::author`'s test helper). Returns the authored entry hash (a
/// `DeviceAdd`'s hash IS the added device's owner incarnation id).
fn author_control_op(
    conn: &Connection,
    account: AccountId,
    op: &crate::account::ops::AccountOp,
) -> AccountEntryHash {
    use crate::account::envelope::{AccountEntryHeader, VerifiedAccountEntry, sign_account_entry};
    use crate::account::fold::CONTROL_LOG;
    use crate::account::{authoring, ops as control_ops};

    let founder = local_device(conn, NOW).unwrap();
    let LocalAccountRef { genesis_hash, .. } = bootstrap::local_account_ref(conn).unwrap().unwrap();
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    let tail = authoring::account_chain_tail(&tx, account, founder.fingerprint(), CONTROL_LOG)
        .unwrap()
        .expect("control chain is non-empty post-genesis");
    let header = AccountEntryHeader {
        account_id: account,
        log_id: CONTROL_LOG,
        device_fingerprint: founder.fingerprint(),
        seq: tail.0 + 1,
        prev_hash: Some(tail.1),
        parent_ref: Some(genesis_hash),
        entry_type: control_ops::entry_type_of(op),
        op_version: 1,
        crypto_suite: 0,
        auth_len: account_storage::account_effective_count(&tx, account).unwrap(),
        key_id: None,
        authority_ref: Some(genesis_hash.into()),
    };
    let payload = control_ops::encode(op).unwrap();
    let signed = sign_account_entry(founder.secret(), &header, &payload).unwrap();
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    account_storage::insert_candidate(&tx, &verified, &signed.signed_bytes, NOW).unwrap();
    account_storage::refold_in_tx(&tx, account, NOW).unwrap();
    tx.commit().unwrap();
    verified.entry_hash
}

/// Author + refold a control op signed by an ARBITRARY device (not just the founder), citing
/// `authority_ref`, on that device's own dense control chain. The non-founder counterpart of
/// [`author_control_op`] (mirrors `secrets::author`'s test helper) — needed to drive an
/// `OwnerDemote` of the local founder by a second owner.
fn author_control_op_as(
    conn: &Connection,
    account: AccountId,
    signer: &crate::device::DeviceSecret,
    authority_ref: OwnerId,
    op: &crate::account::ops::AccountOp,
) -> AccountEntryHash {
    use crate::account::envelope::{AccountEntryHeader, VerifiedAccountEntry, sign_account_entry};
    use crate::account::fold::CONTROL_LOG;
    use crate::account::{authoring, ops as control_ops};

    let signer_fp = signer.public().fingerprint();
    let LocalAccountRef { genesis_hash, .. } = bootstrap::local_account_ref(conn).unwrap().unwrap();
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
    let (seq, prev_hash) =
        match authoring::account_chain_tail(&tx, account, signer_fp, CONTROL_LOG).unwrap() {
            Some((tail_seq, tail_hash)) => (tail_seq + 1, Some(tail_hash)),
            None => (0, None),
        };
    let header = AccountEntryHeader {
        account_id: account,
        log_id: CONTROL_LOG,
        device_fingerprint: signer_fp,
        seq,
        prev_hash,
        parent_ref: Some(genesis_hash),
        entry_type: control_ops::entry_type_of(op),
        op_version: 1,
        crypto_suite: 0,
        auth_len: account_storage::account_effective_count(&tx, account).unwrap(),
        key_id: None,
        authority_ref: Some(authority_ref),
    };
    let payload = control_ops::encode(op).unwrap();
    let signed = sign_account_entry(signer, &header, &payload).unwrap();
    let verified = VerifiedAccountEntry {
        header: signed.header,
        payload: signed.payload,
        entry_hash: signed.entry_hash,
    };
    account_storage::insert_candidate(&tx, &verified, &signed.signed_bytes, NOW).unwrap();
    account_storage::refold_in_tx(&tx, account, NOW).unwrap();
    tx.commit().unwrap();
    verified.entry_hash
}

/// The (seq, hash) tail of a device's `(account, device, log)` chain — used to place tight
/// cuts (mirrors `secrets::author`'s test helper).
fn chain_tail(
    conn: &Connection,
    account: AccountId,
    fp: DeviceFingerprint,
    log: u8,
) -> Option<(u64, AccountEntryHash)> {
    let tx = conn.unchecked_transaction().unwrap();
    crate::account::authoring::account_chain_tail(&tx, account, fp, log).unwrap()
}

/// Add a member device to the roster (folds effective) and return its fingerprint — so a mint
/// seals the current wrap to a recipient the test can later remove.
fn add_member_device(
    conn: &Connection,
    account: AccountId,
    member_x: &crate::device::DeviceX25519Secret,
) -> DeviceFingerprint {
    use crate::account::ops::{AccountOp, DeviceRole};
    use crate::device::DeviceSecret;

    let member_pub = DeviceSecret::from_seed(&[0x2c; 32]).public();
    let member_fp = member_pub.fingerprint();
    author_control_op(conn, account, &AccountOp::DeviceAdd {
        device_fingerprint: member_fp,
        ed25519_pubkey: member_pub.to_bytes(),
        x25519_pubkey: member_x.public().to_bytes(),
        role: DeviceRole::Member,
        label: None,
    });
    member_fp
}

#[test]
fn author_device_add_in_tx_enrolls_a_joiner_at_the_chosen_role() {
    use crate::account::{DeviceRole, EnrollingDevice, author_device_add_in_tx};
    use crate::device::{DeviceSecret, DeviceX25519Secret};

    for (seed, role, token) in [
        (0x60u8, DeviceRole::ReadOnly, "read_only"),
        (0x61, DeviceRole::Member, "member"),
        (0x62, DeviceRole::Owner, "owner"),
    ] {
        let conn = db();
        // Mints the store's local account; the founder (local device) is the enrolling owner.
        owned_v2(&conn);
        let joiner = DeviceSecret::from_seed(&[seed; 32]);
        let joiner_x = DeviceX25519Secret::from_seed(&[seed.wrapping_add(0x80); 32]);
        let joiner_fp = joiner.public().fingerprint();

        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        // Returns Ok only if the joiner folded roster-effective — the seam verifies the fact,
        // so a rejected DeviceAdd (non-owner author) would surface here as an error.
        author_device_add_in_tx(
            &tx,
            EnrollingDevice {
                ed25519_pubkey: joiner.public().to_bytes(),
                x25519_pubkey: joiner_x.public().to_bytes(),
                label: Some("laptop".to_string()),
            },
            role,
            NOW,
        )
        .expect("the founder owner enrolls the joiner");
        tx.commit().unwrap();

        let persisted: String = conn
            .query_row(
                "SELECT role FROM account_roster_history WHERE device_fingerprint = ?1 AND \
                 closed_at IS NULL",
                params![joiner_fp.to_bytes().as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(persisted, token, "the granted role is persisted on the roster");
    }
}

#[test]
fn author_device_add_in_tx_rejects_an_already_enrolled_device() {
    use crate::account::{DeviceRole, EnrollingDevice, author_device_add_in_tx};
    use crate::device::{DeviceSecret, DeviceX25519Secret};

    let conn = db();
    owned_v2(&conn);
    let joiner = DeviceSecret::from_seed(&[0x70; 32]);
    let joiner_x = DeviceX25519Secret::from_seed(&[0x71; 32]);
    let mk = || EnrollingDevice {
        ed25519_pubkey: joiner.public().to_bytes(),
        x25519_pubkey: joiner_x.public().to_bytes(),
        label: None,
    };

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    author_device_add_in_tx(&tx, mk(), DeviceRole::Member, NOW).expect("first enrollment");
    tx.commit().unwrap();

    // A second enrollment of the same fingerprint would fold DuplicateAdd (rejected); the seam
    // must reject it up front rather than return the prior — or a rejected — entry's hash as
    // success.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let err = author_device_add_in_tx(&tx, mk(), DeviceRole::Member, NOW)
        .expect_err("re-enrolling an existing device is rejected");
    assert!(err.to_string().contains("already enrolled"), "unexpected error: {err}");
}

#[test]
fn a_sealed_batch_authors_suite_1_entries_that_fold_accepted_locally() {
    let conn = db();
    let (account, stream) = owned_v2(&conn);
    let hashes = author_content_batch(
        &conn,
        stream,
        &[node_create("n1", "first"), node_update("n1", "second")],
        SealPolicy::Sealed,
        NOW,
    )
    .expect("seal + author");
    assert_eq!(hashes.len(), 2, "two ops author two sealed entries");
    for entry_hash in &hashes {
        assert_eq!(
            stored_status(&conn, entry_hash).as_deref(),
            Some("accepted"),
            "a sealed entry folds accepted locally (acceptance is key-independent)",
        );
        let header = header_of(&conn, entry_hash);
        assert_eq!(header.crypto_suite, 1, "authored as suite 1");
        assert!(header.key_id.is_some(), "a sealed entry carries a key_id");
    }
    // The first-key mint left an accepted content-key wrap on the secrets log.
    assert!(
        secrets::select_current_sealing_wrap(&conn, account, stream).unwrap().is_some(),
        "a content key was minted for the stream",
    );
    let projected: String = conn
        .query_row(
            "SELECT content_json FROM content_projected_nodes
                 WHERE stream_id = ?1 AND node_id = 'n1'",
            [stream.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("suite-1 content decrypts during projection");
    assert!(projected.contains("second"), "the decrypted update wins the LWW register");
}

#[test]
fn prepared_sealed_authoring_runs_inside_and_commits_with_the_caller_transaction() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    let prepared =
        prepare_content_authoring(&conn, stream, SealPolicy::Sealed, NOW).expect("prepare");
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let hashes = author_prepared_content_batch_in_tx(
        &tx,
        stream,
        &[node_create("prepared", "secret")],
        &prepared,
        NOW,
    )
    .expect("author in caller transaction");
    assert_eq!(projected_node_count(&tx, stream), 1, "projection is visible in the transaction");
    assert_eq!(stored_status(&tx, &hashes[0]).as_deref(), Some("accepted"));
    tx.commit().unwrap();

    assert_eq!(projected_node_count(&conn, stream), 1, "content and projection commit together");
    assert_eq!(header_of(&conn, &hashes[0]).crypto_suite, 1);
}

#[test]
fn rolling_back_prepared_sealed_authoring_leaves_no_content_or_projection() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    let prepared =
        prepare_content_authoring(&conn, stream, SealPolicy::Sealed, NOW).expect("prepare");
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    author_prepared_content_batch_in_tx(
        &tx,
        stream,
        &[node_create("rolled-back", "secret")],
        &prepared,
        NOW,
    )
    .expect("author in caller transaction");
    assert_eq!(projected_node_count(&tx, stream), 1, "projection is transactional");
    tx.rollback().unwrap();

    let content_rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_entries WHERE stream_id = ?1",
            [stream.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(content_rows, 0, "rollback removes the authored content");
    assert_eq!(projected_node_count(&conn, stream), 0, "rollback removes its projection");
}

#[test]
fn mixed_plaintext_and_sealed_entries_project_together() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    author_committed(&conn, stream, &[node_create("public", "plain")]);
    author_content_batch(
        &conn,
        stream,
        &[node_create("private", "sealed")],
        SealPolicy::Sealed,
        NOW,
    )
    .unwrap();
    let ids: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT node_id FROM content_projected_nodes
                     WHERE stream_id = ?1 ORDER BY node_id",
            )
            .unwrap();
        stmt.query_map([stream.to_bytes().as_slice()], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(ids, vec!["private".to_string(), "public".to_string()]);
}

#[test]
fn projection_keeps_prior_epoch_content_after_rotation() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    author_content_batch(
        &conn,
        stream,
        &[node_create("old", "epoch zero")],
        SealPolicy::Sealed,
        NOW,
    )
    .unwrap();
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    secrets::rotate_stream_key_in_tx(&tx, stream, NOW + 1).unwrap();
    tx.commit().unwrap();
    author_content_batch(
        &conn,
        stream,
        &[node_create("new", "epoch one")],
        SealPolicy::Sealed,
        NOW + 2,
    )
    .unwrap();
    assert_eq!(projected_node_count(&conn, stream), 2, "both historical keys are available");
}

#[test]
fn decrypted_nodes_follow_retro_condemn_and_accept_reprojection() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    let hashes = author_content_batch(
        &conn,
        stream,
        &[node_create("sealed", "visible")],
        SealPolicy::Sealed,
        NOW,
    )
    .unwrap();
    assert_eq!(projected_node_count(&conn, stream), 1);

    conn.execute("UPDATE content_entries SET accepted = 0 WHERE entry_hash = ?1", [
        hashes[0].as_slice()
    ])
    .unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    content_projection::reproject_accepted_content_stream(&tx, stream).unwrap();
    tx.commit().unwrap();
    assert_eq!(projected_node_count(&conn, stream), 0, "retro-condemn removes decrypted state");

    conn.execute("UPDATE content_entries SET accepted = 1 WHERE entry_hash = ?1", [
        hashes[0].as_slice()
    ])
    .unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    content_projection::reproject_accepted_content_stream(&tx, stream).unwrap();
    tx.commit().unwrap();
    assert_eq!(projected_node_count(&conn, stream), 1, "retro-accept restores decrypted state");
}

#[test]
fn projection_resolves_the_stream_owner_not_the_content_author() {
    let conn = db();
    let (owner, stream) = owned_v2(&conn);
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    secrets::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW).unwrap();
    tx.commit().unwrap();
    let device = local_device(&conn, NOW).unwrap();
    let key = match secrets::current_sealing_key(&conn, owner, stream, &device, NOW).unwrap() {
        SealingKeyOutcome::Ready(key) => key,
        _ => panic!("owner key must resolve"),
    };
    let granted_author = AccountId::from_bytes([0x91; 32]);
    let header = ContentEntryHeader {
        stream_id: stream,
        author_account_id: granted_author,
        device_fingerprint: device.fingerprint(),
        seq: 0,
        lamport: 0,
        prev_hash: None,
        grant_id: Some(GrantId::from_bytes([0x92; 32])),
        roster_ref: RosterRef::from_bytes([0x93; 32]),
        owner_auth_len: 0,
        author_auth_len: 0,
        crypto_suite: 0,
        key_id: None,
    };
    let signed = envelope::seal_and_sign_content_entry(
        device.secret(),
        &header,
        &op::encode(&node_create("granted", "writer")),
        &key,
    )
    .unwrap();
    insert_accepted_signed(&conn, &signed);
    let tx = conn.unchecked_transaction().unwrap();
    content_projection::reproject_accepted_content_stream(&tx, stream).unwrap();
    tx.commit().unwrap();
    assert_eq!(projected_node_count(&conn, stream), 1);
}

#[test]
fn malformed_tampered_and_unknown_suite_entries_do_not_suppress_valid_content() {
    let conn = db();
    let (owner, stream) = owned_v2(&conn);
    author_content_batch(
        &conn,
        stream,
        &[node_create("good", "survives")],
        SealPolicy::Sealed,
        NOW,
    )
    .unwrap();
    let device = local_device(&conn, NOW).unwrap();
    let key = match secrets::current_sealing_key(&conn, owner, stream, &device, NOW).unwrap() {
        SealingKeyOutcome::Ready(key) => key,
        _ => panic!("owner key must resolve"),
    };
    let base = ContentEntryHeader {
        stream_id: stream,
        author_account_id: AccountId::from_bytes([0xa1; 32]),
        device_fingerprint: device.fingerprint(),
        seq: 0,
        lamport: 1,
        prev_hash: None,
        grant_id: Some(GrantId::from_bytes([0xa2; 32])),
        roster_ref: RosterRef::from_bytes([0xa3; 32]),
        owner_auth_len: 0,
        author_auth_len: 0,
        crypto_suite: 1,
        key_id: Some(key.key_id().to_bytes()),
    };
    let short = envelope::sign_opaque_content_entry_for_test(
        device.secret(),
        &base,
        &[0; envelope::SEALED_NONCE_LEN],
    )
    .unwrap();
    insert_accepted_signed(&conn, &short);

    let mut tampered = envelope::seal_and_sign_content_entry(
        device.secret(),
        &ContentEntryHeader {
            author_account_id: AccountId::from_bytes([0xb1; 32]),
            grant_id: Some(GrantId::from_bytes([0xb2; 32])),
            ..base.clone()
        },
        &op::encode(&node_create("bad", "tag")),
        &key,
    )
    .unwrap();
    *tampered.payload.last_mut().unwrap() ^= 1;
    let tampered = envelope::sign_opaque_content_entry_for_test(
        device.secret(),
        &tampered.header,
        &tampered.payload,
    )
    .unwrap();
    insert_accepted_signed(&conn, &tampered);

    let unknown = envelope::sign_opaque_content_entry_for_test(
        device.secret(),
        &ContentEntryHeader {
            author_account_id: AccountId::from_bytes([0xc1; 32]),
            grant_id: Some(GrantId::from_bytes([0xc2; 32])),
            crypto_suite: 99,
            ..base
        },
        &[0xde, 0xad],
    )
    .unwrap();
    insert_accepted_signed(&conn, &unknown);

    let tx = conn.unchecked_transaction().unwrap();
    content_projection::reproject_accepted_content_stream(&tx, stream)
        .expect("bad local payloads skip without aborting the stream");
    tx.commit().unwrap();
    assert_eq!(projected_node_count(&conn, stream), 1, "the valid node remains projected");
    let accepted: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_entries WHERE stream_id = ?1 AND accepted = 1",
            [stream.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(accepted, 4, "all malformed/unknown entries remain accepted and retained");
}

#[test]
fn corrupt_stored_signed_envelope_is_a_loud_projection_error() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    let hashes = author_content_batch(
        &conn,
        stream,
        &[node_create("sealed", "valid")],
        SealPolicy::Sealed,
        NOW,
    )
    .unwrap();
    conn.execute("UPDATE content_entries SET signed_bytes = X'00' WHERE entry_hash = ?1", [hashes
        [0]
    .as_slice()])
        .unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    assert!(
        content_projection::reproject_accepted_content_stream(&tx, stream).is_err(),
        "corruption of the stored signed envelope must not be downgraded to a local skip",
    );
}

#[test]
fn a_valid_envelope_substituted_into_another_accepted_row_is_a_loud_projection_error() {
    let conn = db();
    let stream_a = StreamId::from_bytes(STREAM_A);
    let stream_b = StreamId::from_bytes(STREAM_B);
    let account = owned_stream_account(&conn, stream_a);
    seed_ownership(&conn, stream_b, account);
    let hash_a = author_committed(&conn, stream_a, &[node_create("a", "stream-a")])[0];
    let hash_b = author_committed(&conn, stream_b, &[node_create("substituted", "stream-b")])[0];
    let signed_b: Vec<u8> = conn
        .query_row(
            "SELECT signed_bytes FROM content_entries WHERE entry_hash = ?1",
            [hash_b.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute("UPDATE content_entries SET signed_bytes = ?1 WHERE entry_hash = ?2", params![
        signed_b,
        hash_a.as_slice()
    ])
    .unwrap();
    conn.execute("DELETE FROM content_projected_nodes WHERE stream_id = ?1", [stream_a
        .to_bytes()
        .as_slice()])
        .unwrap();

    let tx = conn.unchecked_transaction().unwrap();
    let err = content_projection::reproject_accepted_content_stream(&tx, stream_a)
        .expect_err("a valid envelope from another row must not project");
    drop(tx);
    assert!(err.to_string().contains("entry_hash row"), "unexpected error: {err:#}");
    assert_eq!(
        projected_node_count(&conn, stream_a),
        0,
        "the substituted stream-b op was not projected under stream A",
    );
}

#[test]
fn a_sealed_entry_folds_accepted_through_content_ingest_on_a_keyless_peer() {
    // S3 FOLD-FIREWALL TRIPWIRE: acceptance of a sealed `/3` entry is header/citation-level, so
    // a peer that replicated the account roster but never received the content key still folds
    // it `accepted`. This is the coverage the account-log and decode-only sealed tests don't
    // provide — a suite-1 entry driven through `content_ingest` + refold.
    let author = db();
    let (account, stream) = owned_v2(&author);
    let hashes = author_content_batch(
        &author,
        stream,
        &[node_create("n1", "secret")],
        SealPolicy::Sealed,
        NOW,
    )
    .expect("seal + author");
    let sealed_bytes: Vec<u8> = author
        .query_row(
            "SELECT signed_bytes FROM content_entries WHERE entry_hash = ?1",
            [hashes[0].as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        envelope::decode_content_signed(&sealed_bytes).unwrap().header.crypto_suite,
        1,
        "the authored entry is sealed",
    );

    // The author's CONTROL log (genesis + StreamOwn) is the public roster a peer replicates.
    // The secrets-log StreamKeyWrap is deliberately NOT replicated: the peer holds no
    // content key.
    let control: Vec<Vec<u8>> = {
        let mut stmt = author
            .prepare(
                "SELECT signed_bytes FROM account_entries
                     WHERE account_id = ?1 AND log_id = 0 ORDER BY seq",
            )
            .unwrap();
        stmt.query_map([account.to_bytes().as_slice()], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };

    let peer = db();
    for bytes in &control {
        account_ingest(&peer, bytes, NOW).expect("replicate the account control log");
    }
    // The peer cannot resolve the content key — no wrap was replicated.
    assert!(
        secrets::select_current_sealing_wrap(&peer, account, stream).unwrap().is_none(),
        "the keyless peer has no content key for the stream",
    );

    // Ingest the sealed content entry and settle its deferred acceptance fold.
    content_storage::content_ingest(&peer, &sealed_bytes, NOW).expect("ingest sealed entry");
    content_storage::settle_pending_content_refolds(
        &peer,
        &content_storage::ContentRefoldBudget::unbounded(),
        NOW,
    )
    .expect("settle the refold");

    let (status, accepted): (String, i64) = peer
        .query_row(
            "SELECT s.status, e.accepted FROM content_entries e
                 JOIN content_entry_status s ON s.entry_hash = e.entry_hash
                 WHERE e.entry_hash = ?1",
            [hashes[0].as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (status.as_str(), accepted),
        ("accepted", 1),
        "the sealed entry folds accepted on a peer with no content key",
    );
    assert_eq!(projected_node_count(&peer, stream), 0, "keyless accepted content is unprojected");
    let identity_rows: i64 =
        peer.query_row("SELECT count(*) FROM oplog_device_identity", [], |row| row.get(0)).unwrap();
    assert_eq!(identity_rows, 0, "projection must not mint a peer identity");
}

#[test]
fn sealing_a_stream_the_account_does_not_own_bails_and_leaks_no_row() {
    // A Sealed-intent stream the account does NOT own: NoCurrentKey → try to mint the first
    // key, but the mint is owner-gated on an effective StreamOwn, so it bails → the
    // whole sealed batch fails closed. NO plaintext-fallback, and no row leaked into
    // any projectable state.
    let conn = db();
    local_account(&conn, NOW).expect("mint local account");
    let unowned = StreamId::from_bytes([0x99; 32]);
    let result =
        author_content_batch(&conn, unowned, &[node_create("n1", "x")], SealPolicy::Sealed, NOW);
    assert!(result.is_err(), "a keyless Sealed stream fails closed");
    let entries: i64 =
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(entries, 0, "no /3 content row survives the fail-closed bail");
    let projected: i64 = conn
        .query_row("SELECT count(*) FROM content_projected_nodes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(projected, 0, "no projection row leaks from the bailed sealed batch");
}

#[test]
fn plaintext_authoring_is_refused_after_a_stream_has_sealed() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    // Sealing mints a wrap AND authors a suite-1 entry — both downgrade-ratchet inputs.
    author_content_batch(&conn, stream, &[node_create("n1", "x")], SealPolicy::Sealed, NOW)
        .expect("seal");
    let before: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_entries WHERE stream_id = ?1",
            [stream.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    // Plaintext authoring on the now-sealed stream is refused — no silent downgrade to
    // cleartext.
    let result =
        author_content_batch(&conn, stream, &[node_create("n2", "y")], SealPolicy::Plaintext, NOW);
    assert!(result.is_err(), "plaintext authoring is refused after the stream sealed");
    let after: i64 = conn
        .query_row(
            "SELECT count(*) FROM content_entries WHERE stream_id = ?1",
            [stream.to_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, after, "the refused plaintext batch stored no row");
}

#[test]
fn corrupt_accepted_sealed_envelope_aborts_plaintext_authoring_without_a_wrap() {
    let conn = db();
    let stream = StreamId::from_bytes(STREAM_A);
    let account = owned_stream_account(&conn, stream);
    let device = local_device(&conn, NOW).unwrap();
    let key = ContentKey::from_seed(&[0x40; 32]);
    let signed = envelope::seal_and_sign_content_entry(
        device.secret(),
        &ContentEntryHeader {
            stream_id: stream,
            author_account_id: account,
            device_fingerprint: device.fingerprint(),
            seq: 0,
            lamport: 0,
            prev_hash: None,
            grant_id: None,
            roster_ref: genesis_ref(&conn).into(),
            owner_auth_len: 0,
            author_auth_len: 0,
            crypto_suite: 0,
            key_id: None,
        },
        &op::encode(&node_create("sealed", "must-stay-private")),
        &key,
    )
    .unwrap();
    insert_accepted_signed(&conn, &signed);
    conn.execute("UPDATE content_entries SET signed_bytes = X'00' WHERE entry_hash = ?1", [signed
        .entry_hash
        .as_slice()])
        .unwrap();
    assert!(
        secrets::select_current_sealing_wrap(&conn, account, stream).unwrap().is_none(),
        "the corrupt accepted suite-1 row is the only ratchet input",
    );
    let before: i64 =
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();

    let err = author_content_batch(
        &conn,
        stream,
        &[node_create("plaintext", "must-not-leak")],
        SealPolicy::Plaintext,
        NOW,
    )
    .expect_err("stored envelope corruption must abort plaintext authoring");
    assert!(err.to_string().contains("sealed ratchet"), "unexpected error: {err:#}");
    let after: i64 =
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get(0)).unwrap();
    assert_eq!(before, after, "the failed plaintext batch leaked no content row");
}

#[test]
fn corrupt_accepted_wrap_aborts_prepared_plaintext_authoring_without_sealed_content() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    let wrap_hash = {
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        let wraps = secrets::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW).unwrap();
        tx.commit().unwrap();
        let [wrap_hash] = wraps[..] else { panic!("this fixture's roster fits one op") };
        wrap_hash
    };
    conn.execute("UPDATE account_entries SET signed_bytes = X'00' WHERE entry_hash = ?1", [
        wrap_hash.as_slice(),
    ])
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0,
        "the malformed accepted wrap is the only sealing evidence",
    );

    let prepared = prepare_content_authoring(&conn, stream, SealPolicy::Plaintext, NOW)
        .expect("plaintext preparation defers the ratchet check to the authoring transaction");
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let err = author_prepared_content_batch_in_tx(
        &tx,
        stream,
        &[node_create("plaintext", "must-not-leak")],
        &prepared,
        NOW,
    )
    .expect_err("accepted wrap corruption must fail closed for downgrade evidence");
    drop(tx);
    assert!(err.to_string().contains("sealed-ratchet wrap evidence"), "unexpected error: {err:#}");
    assert_eq!(
        conn.query_row("SELECT count(*) FROM content_entries", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0,
        "the failed plaintext authoring leaked no content row",
    );
}

#[test]
fn prepared_plaintext_rechecks_the_downgrade_ratchet_in_the_caller_transaction() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    let prepared = prepare_content_authoring(&conn, stream, SealPolicy::Plaintext, NOW)
        .expect("prepare plaintext while the stream is fresh");
    author_content_batch(&conn, stream, &[node_create("sealed", "first")], SealPolicy::Sealed, NOW)
        .expect("ratchet the stream after preparation");

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let err = author_prepared_content_batch_in_tx(
        &tx,
        stream,
        &[node_create("plaintext", "must-not-leak")],
        &prepared,
        NOW,
    )
    .unwrap_err();
    drop(tx);
    assert!(err.to_string().contains("ratcheted to sealed"));
    assert_eq!(projected_node_count(&conn, stream), 1, "the plaintext op was not projected");
}

#[test]
fn the_downgrade_ratchet_fires_on_an_accepted_wrap_or_an_accepted_sealed_entry() {
    let conn = db();
    let (account, stream) = owned_v2(&conn);

    // A fresh owned stream (plaintext-only) does not ratchet.
    {
        let tx = conn.unchecked_transaction().unwrap();
        assert!(
            !stream_has_sealed_ratchet(&tx, account, stream).unwrap(),
            "a fresh owned stream is not sealed",
        );
    }

    // The WRAP branch: mint a wrap only (no content) → the ratchet fires on wrap presence.
    {
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        secrets::mint_and_author_stream_key_wrap_in_tx(&tx, stream, NOW).unwrap();
        tx.commit().unwrap();
    }
    {
        let tx = conn.unchecked_transaction().unwrap();
        assert!(
            stream_has_sealed_ratchet(&tx, account, stream).unwrap(),
            "an accepted StreamKeyWrap ratchets the stream",
        );
    }

    // The SEALED-ENTRY branch, independent of any wrap: a different stream with an accepted
    // suite-1 row but NO wrap still ratchets (proves the OR clause, not just wrap-presence).
    let other = StreamId::from_bytes([0x7a; 32]);
    let device = local_device(&conn, NOW).unwrap();
    let key = ContentKey::from_seed(&[0x40; 32]);
    let header = ContentEntryHeader {
        stream_id: other,
        author_account_id: account,
        device_fingerprint: device.fingerprint(),
        seq: 0,
        lamport: 0,
        prev_hash: None,
        grant_id: None,
        roster_ref: genesis_ref(&conn).into(),
        owner_auth_len: 0,
        author_auth_len: 0,
        crypto_suite: 0,
        key_id: None,
    };
    // The nonce value is not load-bearing here (only suite-1 presence arms the ratchet), so
    // seal via the random-nonce production entry point rather than a fixed injected
    // nonce.
    let signed =
        envelope::seal_and_sign_content_entry(device.secret(), &header, &[0x01], &key).unwrap();
    conn.execute(
        "INSERT INTO content_entries(
                 entry_hash, stream_id, author_account_id, device_fingerprint, seq, prev_hash,
                 grant_id, roster_ref, owner_auth_len, author_auth_len, accepted, signed_bytes,
                 received_at_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?7, 1, ?8, 0)",
        params![
            signed.entry_hash.as_slice(),
            other.to_bytes().as_slice(),
            account.to_bytes().as_slice(),
            device.fingerprint().to_bytes().as_slice(),
            0_u64.to_be_bytes().as_slice(),
            genesis_ref(&conn).as_slice(),
            0_u64.to_be_bytes().as_slice(),
            signed.signed_bytes,
        ],
    )
    .unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    assert!(
        secrets::select_current_sealing_wrap(&tx, account, other).unwrap().is_none(),
        "the other stream has no wrap",
    );
    assert!(
        stream_has_sealed_ratchet(&tx, account, other).unwrap(),
        "an accepted sealed entry ratchets the stream even with no wrap",
    );
}

#[test]
fn plaintext_policy_authors_suite_0_on_a_fresh_stream_unchanged() {
    // `SealPolicy::Plaintext` on a never-sealed owned stream authors suite-0 through the
    // existing in-tx seam — public authoring is unchanged.
    let conn = db();
    let stream = StreamId::from_bytes(STREAM_A);
    owned_stream_account(&conn, stream);
    let hashes =
        author_content_batch(&conn, stream, &[node_create("n1", "x")], SealPolicy::Plaintext, NOW)
            .expect("plaintext author");
    assert_eq!(stored_status(&conn, &hashes[0]).as_deref(), Some("accepted"));
    let header = header_of(&conn, &hashes[0]);
    assert_eq!(header.crypto_suite, 0, "the Plaintext policy authors suite 0");
    assert!(header.key_id.is_none(), "a suite-0 entry has no key_id");
}

#[test]
fn content_op_is_sealed_authorable_reserves_the_aead_overhead() {
    // The sealed body bound is exactly 40 bytes (24 nonce + 16 tag) tighter than the plaintext
    // one, so a sealed op can never mint an un-signable entry.
    assert_eq!(CONTENT_SEALED_OP_BODY_MAX_BYTES, CONTENT_OP_BODY_MAX_BYTES - 40);
    let at = node_create_sized("mem_at", CONTENT_SEALED_OP_BODY_MAX_BYTES);
    assert_eq!(op::encode(&at).len(), CONTENT_SEALED_OP_BODY_MAX_BYTES, "sized on the nose");
    assert!(content_op_is_sealed_authorable(&at), "an op at the sealed bound is authorable");
    let over = node_create_sized("mem_over", CONTENT_SEALED_OP_BODY_MAX_BYTES + 1);
    assert!(
        !content_op_is_sealed_authorable(&over),
        "one byte over the sealed bound is quarantined",
    );

    // Drift guard: a body at the sealed bound, sealed under the WIDEST header, signs at or
    // under the §18a envelope cap — the reserve is enough (mirrors the plaintext
    // overhead guard).
    let secret = crate::device::DeviceSecret::from_seed(&[9; 32]);
    let key = ContentKey::from_seed(&[0x20; 32]);
    let header = ContentEntryHeader {
        stream_id: StreamId::from_bytes([0x11; 32]),
        author_account_id: AccountId::from_bytes([0x22; 32]),
        device_fingerprint: secret.public().fingerprint(),
        seq: u64::MAX,
        lamport: u64::MAX,
        prev_hash: Some(AccountEntryHash::from_bytes([0x33; 32])),
        grant_id: Some(GrantId::from_bytes([0x44; 32])),
        roster_ref: RosterRef::from_bytes([0x55; 32]),
        owner_auth_len: u64::MAX,
        author_auth_len: u64::MAX,
        crypto_suite: 0,
        key_id: None,
    };
    let mut op_body = vec![0x5a];
    op_body.extend_from_slice(&((CONTENT_SEALED_OP_BODY_MAX_BYTES - 5) as u32).to_be_bytes());
    op_body.resize(CONTENT_SEALED_OP_BODY_MAX_BYTES, 0);
    // The nonce value is not load-bearing here (the size is nonce-width-independent), so seal
    // via the random-nonce production entry point rather than a fixed injected nonce.
    let signed = envelope::seal_and_sign_content_entry(&secret, &header, &op_body, &key).unwrap();
    assert!(
        signed.signed_bytes.len() <= CONTENT_ENVELOPE_MAX_BYTES,
        "the sealed reserve keeps the signed envelope within the §18a cap ({} > {})",
        signed.signed_bytes.len(),
        CONTENT_ENVELOPE_MAX_BYTES,
    );
}

#[test]
fn a_rotation_in_the_resolution_window_makes_txn_b_bail_not_seal_stale() {
    // P1: the sealed author resolves the content key in an autocommit window between txn A and
    // txn B. A DeviceRemove + rotation committing a fresh higher-epoch wrap in that window
    // would otherwise let txn B seal under the STALE key — acceptance is
    // key-independent, so it folds accepted — and a device the rotation revoked could
    // decrypt this post-removal entry. `revalidate_sealing_selection` re-reads the
    // selection under txn B's write lock and bails when it no longer names the resolved
    // key. Simulated directly: resolve, rotate the accepted wrap set to a higher epoch,
    // then assert the re-validation bails on the stale baseline.
    let conn = db();
    let (account, stream) = owned_v2(&conn);
    // Establish the epoch-0 sealing wrap the resolution would name (the stale baseline).
    author_content_batch(&conn, stream, &[node_create("n1", "first")], SealPolicy::Sealed, NOW)
        .expect("mint the initial key + author");
    let baseline = secrets::select_current_sealing_wrap(&conn, account, stream)
        .unwrap()
        .expect("an epoch-0 selection");
    assert_eq!(baseline.key_epoch, 0, "the initial mint is epoch 0");

    // The concurrent rotation: commit a fresh epoch-1 wrap (what a DeviceRemove-driven rotation
    // authors), so the current selection no longer names the resolved epoch-0 key.
    {
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        secrets::rotate_stream_key_in_tx(&tx, stream, NOW).expect("rotate to epoch 1");
        tx.commit().unwrap();
    }
    let rotated = secrets::select_current_sealing_wrap(&conn, account, stream)
        .unwrap()
        .expect("an epoch-1 selection");
    assert_eq!(rotated.key_epoch, 1, "the rotation advanced the selection to epoch 1");

    // Under txn B's write lock the stale epoch-0 baseline no longer matches → bail (retry).
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let err = revalidate_sealing_selection(
        &tx,
        account,
        stream,
        &baseline,
        &secrets::RotationOutcome::Current,
    )
    .unwrap_err();
    drop(tx);
    assert!(
        err.to_string().contains("rotated between resolution and sealing"),
        "the re-validation bails on a rotation in the resolution window: {err}",
    );

    // The guard is not a blanket refusal: the fresh selection still validates.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    revalidate_sealing_selection(
        &tx,
        account,
        stream,
        &rotated,
        &secrets::RotationOutcome::Current,
    )
    .expect("the current selection re-validates");
    tx.commit().unwrap();
}

#[test]
fn prepared_sealed_authoring_rejects_a_rotation_before_the_caller_transaction() {
    let conn = db();
    let (_account, stream) = owned_v2(&conn);
    author_content_batch(
        &conn,
        stream,
        &[node_create("old", "epoch-zero")],
        SealPolicy::Sealed,
        NOW,
    )
    .unwrap();
    let prepared =
        prepare_content_authoring(&conn, stream, SealPolicy::Sealed, NOW).expect("prepare");
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    secrets::rotate_stream_key_in_tx(&tx, stream, NOW + 1).unwrap();
    tx.commit().unwrap();

    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let err = author_prepared_content_batch_in_tx(
        &tx,
        stream,
        &[node_create("stale", "must-not-leak")],
        &prepared,
        NOW + 2,
    )
    .unwrap_err();
    drop(tx);
    assert!(err.to_string().contains("rotated between resolution and sealing"));
    assert_eq!(projected_node_count(&conn, stream), 1, "no stale-key content was projected");
}

#[test]
fn prepared_sealed_authoring_rejects_a_recipient_removal_before_the_caller_transaction() {
    let conn = db();
    let (account, stream) = owned_v2(&conn);
    let member_x = crate::device::DeviceX25519Secret::from_seed(&[0x5d; 32]);
    let member = add_member_device(&conn, account, &member_x);
    author_content_batch(
        &conn,
        stream,
        &[node_create("old", "before-removal")],
        SealPolicy::Sealed,
        NOW,
    )
    .unwrap();
    let prepared =
        prepare_content_authoring(&conn, stream, SealPolicy::Sealed, NOW).expect("prepare");

    author_control_op(&conn, account, &crate::account::ops::AccountOp::DeviceRemove {
        device_fingerprint: member,
        control_cut: crate::account::cut::Cut::Empty,
        secrets_cut: crate::account::cut::Cut::Empty,
        content_cuts: Vec::new(),
        reason: "revoked after preparation".to_string(),
    });
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let err = author_prepared_content_batch_in_tx(
        &tx,
        stream,
        &[node_create("stale", "removed-recipient-must-not-decrypt")],
        &prepared,
        NOW + 1,
    )
    .unwrap_err();
    drop(tx);
    assert!(err.to_string().contains("rotation became needed under the authoring lock"));
    assert_eq!(projected_node_count(&conn, stream), 1, "no post-removal content leaked");
}

#[test]
fn a_device_removal_in_the_resolution_window_makes_txn_b_bail_not_seal_stale() {
    // P1: a BARE `DeviceRemove` of a wrap RECIPIENT — committed in the autocommit window
    // between txn A (rotation check) and txn B (seal), with NO rotation, so NO
    // higher-epoch wrap exists — leaves the current sealing selection UNCHANGED:
    // `select_current_sealing_wrap` still returns the old `(epoch, key_id)`. The
    // selection-unchanged check alone therefore PASSES, and txn B would seal under the
    // key the just-removed device still holds — a device revoked by that removal could
    // decrypt this post-removal entry (acceptance is key-independent, so
    // the batch folds accepted regardless). The rotation-need re-check under txn B's write lock
    // catches it. Contrast with `a_rotation_in_the_resolution_window_...`, which mints a
    // HIGHER-epoch wrap so the selection-unchanged check alone already bails.
    let conn = db();
    let (account, stream) = owned_v2(&conn);
    // A second roster device so the initial mint seals to ≥2 recipients — one we can remove.
    let member_x = crate::device::DeviceX25519Secret::from_seed(&[0x5c; 32]);
    let member = add_member_device(&conn, account, &member_x);

    // Establish the epoch-0 sealing wrap (sealed to founder + member) the resolution would
    // name.
    author_content_batch(&conn, stream, &[node_create("n1", "first")], SealPolicy::Sealed, NOW)
        .expect("mint the initial key + author");
    let baseline = secrets::select_current_sealing_wrap(&conn, account, stream)
        .unwrap()
        .expect("an epoch-0 selection");
    assert_eq!(baseline.key_epoch, 0, "the initial mint is epoch 0");

    // No removal yet ⇒ rotation is not needed ⇒ the baseline re-validates. Proves the fix does
    // not spuriously bail the normal case.
    {
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        revalidate_sealing_selection(
            &tx,
            account,
            stream,
            &baseline,
            &secrets::RotationOutcome::Current,
        )
        .expect("no removal ⇒ rotation not needed ⇒ the baseline re-validates");
        tx.commit().unwrap();
    }

    // The concurrent BARE removal: revoke the member WITHOUT rotating (mint no higher-epoch
    // wrap).
    author_control_op(&conn, account, &crate::account::ops::AccountOp::DeviceRemove {
        device_fingerprint: member,
        control_cut: crate::account::cut::Cut::Empty,
        secrets_cut: crate::account::cut::Cut::Empty,
        content_cuts: Vec::new(),
        reason: "revoked".to_string(),
    });
    // The selection is UNCHANGED (no rotation happened) — the stale-baseline check would
    // pass...
    let after = secrets::select_current_sealing_wrap(&conn, account, stream)
        .unwrap()
        .expect("a still-epoch-0 selection");
    assert_eq!(after.key_epoch, 0, "no rotation happened — the epoch is unchanged");
    assert_eq!(after.key_id, baseline.key_id, "the bare removal did not change the key_id");
    // ...yet rotation is now DUE (the removed member still holds the current wrap).
    assert!(
        secrets::stream_key_rotation_needed(&conn, account, stream).unwrap(),
        "the bare removal makes rotation needed without changing the selection",
    );

    // Under txn B's write lock the re-validation must now BAIL on rotation-need, not seal
    // stale. (Without the rotation-need re-check this passes and txn B seals under the
    // stale key.) The simulated txn A ran BEFORE the removal, so its outcome is `Current`
    // — an owner's retry rotates; only a StaleButNotOwner txn A exempts this check.
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let err = revalidate_sealing_selection(
        &tx,
        account,
        stream,
        &baseline,
        &secrets::RotationOutcome::Current,
    )
    .unwrap_err();
    drop(tx);
    assert!(
        err.to_string().contains("rotation became needed under the authoring lock"),
        "the re-validation bails on a bare removal in the resolution window: {err}",
    );
}

#[test]
fn a_non_owner_member_seals_under_the_current_key_when_rotation_is_needed() {
    // P2: when a remaining MEMBER (not an owner) calls SealPolicy::Sealed after a recipient
    // was removed, txn A deliberately returns StaleButNotOwner and leaves the old key
    // selected — the member cannot rotate, and `ensure_stream_key_current_in_tx`'s contract
    // is that it proceeds to seal under the current key. An UNCONDITIONAL rotation-need
    // re-check in txn B would fail that intended path forever (a retry can never make a
    // member an owner), so the txn-A outcome is carried into txn B and exempts the
    // re-check. The selection-unchanged check still guards the member against a concurrent
    // rotation.
    let conn = db();
    let (account, stream) = owned_v2(&conn);

    // A second owner whose secret we control, so it can author the founder's demotion (the
    // rotation machinery only operates on the LOCAL self-founded account, so the local
    // device must be demoted by a second owner to become a member).
    let owner_b_ed = crate::device::DeviceSecret::from_seed(&[0x2b; 32]);
    let owner_b_x = crate::device::DeviceX25519Secret::from_seed(&[0xb2; 32]);
    let owner_id_b =
        author_control_op(&conn, account, &crate::account::ops::AccountOp::DeviceAdd {
            device_fingerprint: owner_b_ed.public().fingerprint(),
            ed25519_pubkey: owner_b_ed.public().to_bytes(),
            x25519_pubkey: owner_b_x.public().to_bytes(),
            role: crate::account::ops::DeviceRole::Owner,
            label: None,
        });

    // A member that will be removed to make rotation needed.
    let member_x = crate::device::DeviceX25519Secret::from_seed(&[0x5c; 32]);
    let member = add_member_device(&conn, account, &member_x);

    // Mint the initial key via a sealed batch (the wrap seals to founder + owner_b + member).
    author_content_batch(&conn, stream, &[node_create("n1", "first")], SealPolicy::Sealed, NOW)
        .expect("mint the initial key + author");
    let baseline = secrets::select_current_sealing_wrap(&conn, account, stream)
        .unwrap()
        .expect("an epoch-0 selection");

    // The founder (still an owner) removes the member → rotation needed.
    author_control_op(&conn, account, &crate::account::ops::AccountOp::DeviceRemove {
        device_fingerprint: member,
        control_cut: crate::account::cut::Cut::Empty,
        secrets_cut: crate::account::cut::Cut::Empty,
        content_cuts: Vec::new(),
        reason: "revoked".to_string(),
    });
    assert!(
        secrets::stream_key_rotation_needed(&conn, account, stream).unwrap(),
        "the removed member is still a recipient of the current wrap",
    );

    // owner_b demotes the founder, preserving the founder's whole history via cuts at its
    // chain tails (the mint stays accepted; only the owner ROLE closes).
    let founder = local_device(&conn, NOW).unwrap();
    let founder_owner_id = account_storage::effective_owner_incarnation_for_device(
        &conn,
        account,
        founder.fingerprint(),
    )
    .unwrap()
    .expect("the founder is an owner before the demotion");
    let ctrl_tail =
        chain_tail(&conn, account, founder.fingerprint(), crate::account::fold::CONTROL_LOG)
            .expect("the founder has a control chain");
    let secrets_tail =
        chain_tail(&conn, account, founder.fingerprint(), crate::account::fold::SECRETS_LOG)
            .expect("the founder authored the mint on its secrets chain");
    author_control_op_as(
        &conn,
        account,
        &owner_b_ed,
        owner_id_b.into(),
        &crate::account::ops::AccountOp::OwnerDemote {
            device_fingerprint: founder.fingerprint(),
            owner_id: founder_owner_id,
            control_cut: crate::account::cut::Cut::At { seq: ctrl_tail.0, hash: ctrl_tail.1 },
            secrets_cut: crate::account::cut::Cut::At { seq: secrets_tail.0, hash: secrets_tail.1 },
            reason: "demote".to_string(),
        },
    );
    assert!(
        account_storage::effective_owner_incarnation_for_device(
            &conn,
            account,
            founder.fingerprint(),
        )
        .unwrap()
        .is_none(),
        "the founder is now a plain member",
    );
    assert!(
        secrets::stream_key_rotation_needed(&conn, account, stream).unwrap(),
        "rotation is still needed (the member is gone, no rotation happened)",
    );

    // The member's sealed batch SUCCEEDS under the current (stale) key: txn A returns
    // StaleButNotOwner, and txn B's rotation-need re-check exempts that classified state.
    // (Before the exemption, this call bailed on "rotation became needed under the authoring
    // lock" on every retry — sealed authoring was permanently unavailable to the member.)
    let prepared = prepare_content_authoring(&conn, stream, SealPolicy::Sealed, NOW)
        .expect("a member prepares under the current key (StaleButNotOwner)");
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    let hashes = author_prepared_content_batch_in_tx(
        &tx,
        stream,
        &[node_create("n2", "member-sealed")],
        &prepared,
        NOW,
    )
    .expect("a member seals under the current key (StaleButNotOwner must not fail txn B)");
    tx.commit().unwrap();
    assert_eq!(hashes.len(), 1, "one op authors one sealed entry");
    let header = header_of(&conn, &hashes[0]);
    assert_eq!(header.crypto_suite, 1, "sealed as suite 1");
    assert_eq!(
        header.key_id,
        Some(baseline.key_id.to_bytes()),
        "sealed under the CURRENT key — the member cannot rotate to a fresh one",
    );
    assert_eq!(
        stored_status(&conn, &hashes[0]).as_deref(),
        Some("accepted"),
        "the member's sealed entry folds accepted",
    );
    // And no rotation was authored: the selection is unchanged.
    let after = secrets::select_current_sealing_wrap(&conn, account, stream)
        .unwrap()
        .expect("a selection still exists");
    assert_eq!(after.key_epoch, baseline.key_epoch, "a member authored no rotation");
}

#[test]
fn an_empty_sealed_batch_is_a_noop_and_does_not_arm_the_ratchet() {
    // P2: an empty Sealed batch must run NO key management. Minting the stream's first
    // StreamKeyWrap would arm the downgrade ratchet, permanently refusing later plaintext
    // authoring after a call that authored nothing.
    let conn = db();
    let (account, stream) = owned_v2(&conn);
    let hashes = author_content_batch(&conn, stream, &[], SealPolicy::Sealed, NOW)
        .expect("an empty sealed batch is a no-op");
    assert!(hashes.is_empty(), "an empty batch authors no entries");
    // No wrap was minted → the stream never sealed.
    assert!(
        secrets::select_current_sealing_wrap(&conn, account, stream).unwrap().is_none(),
        "the empty sealed batch minted no content key",
    );
    // Plaintext authoring still works — the downgrade ratchet was never armed.
    let authored =
        author_content_batch(&conn, stream, &[node_create("n1", "x")], SealPolicy::Plaintext, NOW)
            .expect("plaintext authoring still works after an empty sealed batch");
    assert_eq!(authored.len(), 1, "the empty sealed batch did not arm the downgrade ratchet");
}
