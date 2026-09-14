use rag_rat_query::memory::{self, RepoMemoryBindTarget, RepoMemoryCreate, edge_key, memory_by_id};
use rusqlite::Connection;

use super::*;

const REPO: &str = "repo-a";

/// A DB with the memory schema, one registered repo, and the connection scoped to it.
fn scoped_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
        [REPO],
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [REPO],
    )
    .unwrap();
    conn
}

/// Create an unanchored `Concept` through the LIVE create path (mints the account + owner
/// stream and projects the node) — the fixture the real-path tests build on.
fn create_concept(conn: &Connection, title: &str) -> String {
    crate::memory_write::create_memory(conn, RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: title.to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget::default(),
    })
    .unwrap()
    .memory
    .memory_id
}

/// Seed one row into `content_projected_nodes` with the exact `NodeContentRow` JSON shape the
/// projector writes — the "a peer authored this and it folded accepted" fixture.
#[allow(clippy::too_many_arguments)]
fn seed_projected_node(
    conn: &Connection,
    stream: StreamId,
    node_id: &str,
    kind: &str,
    title: &str,
    body: &str,
    status: &str,
    tags: &[&str],
) {
    let content_json = serde_json::json!({
        "kind": kind,
        "title": title,
        "body": body,
        "confidence": "high",
        "source": "agent",
        "tags": tags,
        "payload": null,
    })
    .to_string();
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, ?2, ?3, ?4)",
        params![stream.to_bytes().as_slice(), node_id, content_json, status],
    )
    .unwrap();
}

/// Seed a projected node carrying an anchor snapshot. `anchors` is `(binding_kind, binding_id)`
/// per row; `None` writes SQL NULL, the "nobody published bindings" state.
fn seed_projected_node_with_anchors(
    conn: &Connection,
    stream: StreamId,
    node_id: &str,
    anchors: Option<&[(&str, &str)]>,
) {
    // Idempotent in the node row, so a test can re-run this to add a snapshot to content it
    // already seeded — the "the snapshot arrived in a later entry" shape.
    conn.execute(
        "DELETE FROM content_projected_nodes WHERE stream_id = ?1 AND node_id = ?2",
        params![stream.to_bytes().as_slice(), node_id],
    )
    .unwrap();
    seed_projected_node(conn, stream, node_id, "Invariant", "t", "b", "active", &[]);
    let anchors_json = anchors.map(|anchors| {
        let rows: Vec<serde_json::Value> = anchors
            .iter()
            .map(|(kind, id)| {
                serde_json::json!({
                    "binding_kind": kind,
                    "binding_id": id,
                    "path": "src/lib.rs",
                    "start_line": 1,
                    "end_line": 2,
                    "commit_hash": null,
                    "tracker": null,
                    "project": null,
                    "item_key": null,
                    "created_at_ms": 7,
                    "symbol_kind": null,
                    "signature_hash": null,
                    "moniker_tool": null,
                    "moniker_tool_version": null,
                })
            })
            .collect();
        serde_json::to_string(&rows).unwrap()
    });
    conn.execute(
        "UPDATE content_projected_nodes SET anchors_json = ?3
             WHERE stream_id = ?1 AND node_id = ?2",
        params![stream.to_bytes().as_slice(), node_id, anchors_json],
    )
    .unwrap();
}

fn bindings_of(conn: &Connection, memory_id: &str) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT binding_kind, binding_id FROM repo_memory_bindings
                 WHERE repo_id = ?1 AND memory_id = ?2 ORDER BY binding_kind, binding_id",
        )
        .unwrap();
    let rows =
        stmt.query_map(params![REPO, memory_id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    rows.map(Result::unwrap).collect()
}

/// The happy path: a synced memory arrives with no bindings here, so its author's snapshot
/// seeds them — portable columns carried, every checkout-local column left at its default,
/// which is the row state a `/5` apply produces.
#[test]
fn a_synced_memory_with_no_bindings_is_seeded_from_its_snapshot() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run"), ("path", "src/lib.rs")]),
    );

    drain_worker(&conn, stream, 1_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![
        ("path".to_string(), "src/lib.rs".to_string()),
        ("symbol".to_string(), "src/lib.rs::run".to_string()),
    ]);
    let (status, symbol_id): (String, Option<i64>) = conn
        .query_row(
            "SELECT anchor_status, symbol_id FROM repo_memory_bindings
                 WHERE memory_id = 'mem_peer' AND binding_kind = 'symbol'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "unverified", "local resolution state starts at its default");
    assert_eq!(symbol_id, None, "no checkout-local id is carried across the wire");
}

/// Why a snapshot is applied on CHANGE rather than on difference: the validate/relocate loop
/// re-keys `binding_id`, a PK column, so a drain comparing against the bindings would find the
/// pre-relocation identity absent and put it back beside the row the loop had moved.
#[test]
fn a_relocated_binding_survives_an_unchanged_snapshot() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 1_000);

    // Stand in for the relocation loop: the row moves to a new identity.
    conn.execute(
        "UPDATE repo_memory_bindings SET binding_id = 'src/lib.rs::run_renamed'
             WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();

    // `drain_worker` calls the in-transaction drain directly, so there is no caught-up
    // short-circuit to defeat here — the second pass re-walks every projected node
    // unconditionally. A later drain must not put the original identity back beside the
    // relocated row.
    drain_worker(&conn, stream, 2_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run_renamed".to_string()
    )]);
}

/// The reason `drain_node` computes an effect and then seeds, instead of returning early: an
/// anchor snapshot can arrive in a LATER entry than the content it describes. On that second
/// pass the content has already converged, so the node takes the `unchanged` arm — and if that
/// arm returned, the memory would never get its bindings.
///
/// Restoring the early return must fail this test; nothing else in the suite reaches that arm
/// with a seed pending.
#[test]
fn a_snapshot_arriving_after_its_content_still_seeds() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    // Pass one: content only, no snapshot yet.
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", None);
    drain_worker(&conn, stream, 1_000);
    assert!(
        bindings_of(&conn, "mem_peer").is_empty(),
        "nobody has published this memory's bindings yet",
    );

    // Pass two: the snapshot lands with the content byte-identical, so the node converges and
    // takes the `unchanged` arm.
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.nodes_written, 0, "the content did not change on this pass");
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run".to_string()
    )]);
}

/// An author publishing an EMPTY set states the memory has no bindings, which is not the `None`
/// of nobody having published — but neither seeds, and crucially neither ARMS the gate: a later
/// real snapshot must still be able to seed, which an over-eager "we have seen a snapshot" gate
/// would prevent.
#[test]
fn an_empty_snapshot_seeds_nothing_and_does_not_arm_the_gate() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[]));
    drain_worker(&conn, stream, 1_000);
    assert!(bindings_of(&conn, "mem_peer").is_empty(), "an empty set seeds nothing");

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 2_000);
    assert_eq!(
        bindings_of(&conn, "mem_peer").len(),
        1,
        "the earlier empty set did not consume this memory's one chance to be seeded",
    );
}

/// A binding kind this store cannot produce is a newer peer's vocabulary: skipped row-wise,
/// with the rest of the snapshot still seeded. `call_path` is skipped for a different
/// reason — its supporting tables are in no replication scope, so it could never resolve
/// here.
#[test]
fn an_unseedable_kind_is_skipped_without_losing_the_rest() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[
            ("symbol", "src/lib.rs::run"),
            ("call_path", "abc123"),
            ("from_the_future", "whatever"),
        ]),
    );

    drain_worker(&conn, stream, 1_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run".to_string()
    )]);
}

/// Set (or clear) the source hash a projected node's author published, on a node row a
/// previous `seed_projected_node_with_anchors` call already wrote.
fn set_projected_source_hash(
    conn: &Connection,
    stream: StreamId,
    node_id: &str,
    hash: Option<&str>,
) {
    conn.execute(
        "UPDATE content_projected_nodes SET source_text_hash = ?3
             WHERE stream_id = ?1 AND node_id = ?2",
        params![stream.to_bytes().as_slice(), node_id, hash],
    )
    .unwrap();
}

/// Record which account authored a projected node's anchor set, as the projection does from
/// the winning `NodeAnchors` entry's header.
fn set_projected_anchors_author(
    conn: &Connection,
    stream: StreamId,
    node_id: &str,
    author: &rag_rat_oplog::AccountId,
) {
    conn.execute(
        "UPDATE content_projected_nodes SET anchors_author = ?3
             WHERE stream_id = ?1 AND node_id = ?2",
        params![stream.to_bytes().as_slice(), node_id, author.to_bytes().as_slice()],
    )
    .unwrap();
}

/// Mint this store's own account, and name another one.
fn own_and_foreign_accounts(
    conn: &Connection,
) -> (rag_rat_oplog::AccountId, rag_rat_oplog::AccountId) {
    (rag_rat_oplog::local_account(conn, 1).unwrap(), rag_rat_oplog::AccountId::from_bytes([9; 32]))
}

fn source_hash_of(conn: &Connection, memory_id: &str) -> Option<String> {
    conn.query_row("SELECT source_text_hash FROM repo_memories WHERE id = ?1", [memory_id], |row| {
        row.get(0)
    })
    .unwrap()
}

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// The published hash is a claim about an anchor set, so it lands with the seed that installs
/// one — the text the author anchored to, beside the bindings it describes.
#[test]
fn a_seeded_memory_takes_the_source_hash_its_author_published() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));

    drain_worker(&conn, stream, 1_000);

    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));
}

/// A hash published for a set this store already holds — the sweep reaching a memory whose
/// anchors predate the hash op — lands WITHOUT replacing the bindings, so whatever the
/// relocation loop did to them survives.
#[test]
fn a_later_hash_is_stamped_without_replacing_the_bindings() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 1_000);
    assert_eq!(source_hash_of(&conn, "mem_peer"), None, "nothing published a hash yet");
    // The relocation loop moved it down the file; same row, same target.
    conn.execute(
        "UPDATE repo_memory_bindings SET start_line = 5, end_line = 6
             WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();

    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));
    let span: (i64, i64) = conn
        .query_row(
            "SELECT start_line, end_line FROM repo_memory_bindings WHERE memory_id = 'mem_peer'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(span, (5, 6), "the relocation loop's work survives");
}

/// The INSERT that materializes a node on first sight leaves `source_text_hash` NULL even when
/// the projection carries one, because a hash with no anchors beside it describes nothing.
/// Binding the projected hash in that INSERT — the obvious shortcut — must fail this test;
/// every other case in the suite reaches the column through the seed instead.
#[test]
fn a_first_sight_node_with_no_anchors_takes_no_source_hash() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", None);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));

    drain_worker(&conn, stream, 1_000);

    assert!(bindings_of(&conn, "mem_peer").is_empty(), "nobody published this memory's bindings");
    assert_eq!(
        source_hash_of(&conn, "mem_peer"),
        None,
        "a hash with no anchor set beside it is a claim about nothing",
    );
}

/// A set the fold could not attribute to an account (`anchors_author` NULL) may be this
/// account's own, so rows held under it with no applied set are recorded, not converged: they
/// are not the set the author published, so its hash stays off them, and only a LATER set
/// replaces them and brings its hash.
#[test]
fn bindings_held_under_an_unattributed_set_are_kept_until_the_author_publishes_a_new_set() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", None);
    drain_worker(&conn, stream, 1_000);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::moved', 'src/lib.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 2_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::moved".to_string()
    )]);
    assert_eq!(
        source_hash_of(&conn, "mem_peer"),
        None,
        "the published hash describes a set these bindings are not",
    );

    // The author rebinds.
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/other.rs::walk")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_B));
    drain_worker(&conn, stream, 3_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/other.rs::walk".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// The same record-only branch, reached with bindings that ARE the author's set — the shape
/// `anchors/1` leaves on a device of the author's own account. The rows are kept, and the hash
/// they describe is stamped beside them.
#[test]
fn bindings_matching_the_published_set_take_its_hash() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", None);
    drain_worker(&conn, stream, 1_000);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::run', 'src/lib.rs', 1, 2, 'current',
                 1)",
        [REPO],
    )
    .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));
}

/// A hash published for a set this store cannot hold — only kinds it never inserts — describes
/// nothing here, but it is still recorded as applied. A rebind made here afterwards therefore
/// keeps its own hash, rather than taking the author's on the next pass as if it were new.
#[test]
fn a_hash_applied_while_no_binding_is_held_leaves_a_later_local_rebind_alone() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("chunk", "42")]));
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);
    assert!(bindings_of(&conn, "mem_peer").is_empty(), "a chunk binding is never seeded");
    assert_eq!(source_hash_of(&conn, "mem_peer"), None);

    // A rebind made here.
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'path', 'src/local.rs', 'src/local.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();
    conn.execute("UPDATE repo_memories SET source_text_hash = ?1 WHERE id = 'mem_peer'", [HASH_B])
        .unwrap();

    drain_worker(&conn, stream, 2_000);

    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// The gap a seed-once gate left: an author who rebinds publishes a new set, and a receiver
/// that already holds the old one moves to it — bindings and hash together, whatever the
/// relocation loop had done to the old rows.
#[test]
fn an_author_rebind_replaces_a_synced_memorys_bindings_and_hash_together() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);
    conn.execute(
        "UPDATE repo_memory_bindings SET binding_id = 'src/lib.rs::run_renamed'
             WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/other.rs::walk")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_B));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/other.rs::walk".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// A kind this store never inserts — here a call path — is kept while the author still names
/// it. This is the shape a rebind made on this device leaves: the binding, its call path and
/// the snapshot it published all name one path. Clearing and re-seeding would lose it, and
/// `anchors/1` would carry the delete to every device of the account.
#[test]
fn a_named_binding_this_store_cannot_seed_is_kept() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 1_000);
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'call_path', 'seq', 'current', 1)",
        [REPO],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_paths(
                 memory_id, edge_sequence_hash, path_summary, created_at_ms)
             VALUES ('mem_peer', 'seq', 'run -> walk', 1)",
        [],
    )
    .unwrap();

    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("call_path", "seq")]));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![("call_path".to_string(), "seq".to_string())]);
    let call_paths: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_call_paths WHERE memory_id = 'mem_peer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(call_paths, 1, "the call path the set still names is kept");
}

/// A set change re-resolves every row it still names, even one whose portable columns already
/// match: `anchors/1` updates those in place and keeps the checkout-local ids, so a
/// struct-to-impl rebind reaching this device through it first leaves a row that looks like
/// the impl while its ids still name the struct. Trusting them would put the impl's hash beside
/// the struct.
#[test]
fn a_set_change_re_resolves_a_row_anchors1_already_moved() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::Run")]),
    );
    set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "struct");
    drain_worker(&conn, stream, 1_000);
    // Resolved against the struct, then moved to the impl in place by `anchors/1`.
    conn.execute(
        "UPDATE repo_memory_bindings
             SET anchor_status = 'current', symbol_id = 42, symbol_kind = 'impl'
             WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::Run")]),
    );
    set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "impl");
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 2_000);

    let row: (String, Option<i64>) = conn
        .query_row(
            "SELECT anchor_status, symbol_id FROM repo_memory_bindings
                 WHERE memory_id = 'mem_peer'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        ("unverified".to_string(), None),
        "the struct's resolution is not trusted for the impl",
    );
    let reason: Option<String> = conn
        .query_row(
            "SELECT relocation_reason FROM repo_memory_bindings WHERE memory_id = 'mem_peer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        reason.as_deref(),
        Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str()),
        "judged against the struct the last set named, not the row `anchors/1` moved",
    );
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));
}

/// A retarget is told from a republish of the same target by what the author published last,
/// not by the row: relocation refreshes a row's kind and signature to this checkout's view, so
/// after a local edit the row no longer matches the author's unchanged republish — which must
/// not mark it, or the validator follows the old signature to a same-named sibling.
#[test]
fn a_republish_is_compared_with_the_last_applied_set_not_the_row() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let publish = |path: &str, signature: &str| {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::new")]),
        );
        set_projected_anchor_field(&conn, stream, "mem_peer", "path", path);
        set_projected_anchor_field(&conn, stream, "mem_peer", "signature_hash", signature);
    };
    let reason = || -> Option<String> {
        conn.query_row(
            "SELECT relocation_reason FROM repo_memory_bindings WHERE memory_id = 'mem_peer'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    publish("src/lib.rs", "sig-author");
    drain_worker(&conn, stream, 1_000);
    // Relocation here recorded this checkout's edited signature.
    conn.execute(
        "UPDATE repo_memory_bindings SET signature_hash = 'sig-local'
             WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();

    publish("src/moved.rs", "sig-author");
    drain_worker(&conn, stream, 2_000);
    assert_eq!(reason(), None, "a republish of the same target is no retarget");

    publish("src/moved.rs", "sig-other");
    drain_worker(&conn, stream, 3_000);
    assert_eq!(
        reason().as_deref(),
        Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str()),
        "a changed signature is"
    );
}

/// Two impls of different traits for one type can agree on the kind and the captured
/// signature, so a rebind between them changes only the target's SCOPE, published beside the
/// set (#1276). A changed scope marks the row `retargeted`; the same scope does not; and a
/// scope missing on either side is no evidence, so an older author's set never marks on its
/// absence. The baseline records the scope beside the kind and signature.
#[test]
fn a_changed_published_scope_marks_a_retarget_and_a_missing_one_never_does() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let publish = |path: &str, scope: Option<&str>| {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::Twin")]),
        );
        set_projected_anchor_field(&conn, stream, "mem_peer", "path", path);
        set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "impl");
        set_projected_anchor_field(&conn, stream, "mem_peer", "signature_hash", "sig");
        if let Some(scope) = scope {
            set_projected_anchor_field(&conn, stream, "mem_peer", "scope_hash", scope);
        }
    };
    let reason = || -> Option<String> {
        conn.query_row(
            "SELECT relocation_reason FROM repo_memory_bindings WHERE memory_id = 'mem_peer'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let recorded_scope = || -> Option<String> {
        let json: Option<String> = conn
            .query_row(
                "SELECT anchors_applied_targets FROM repo_memories WHERE id = 'mem_peer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        decode_applied_targets(json.as_deref())
            .and_then(|t| t.get(&("symbol".to_string(), "src/lib.rs::Twin".to_string())).cloned())
            .and_then(|target| target.scope_hash)
    };
    let alpha = rag_rat_base::hash::hex_sha256(b"Twin as Alpha");
    let beta = rag_rat_base::hash::hex_sha256(b"Twin as Beta");

    publish("src/lib.rs", Some(&alpha));
    drain_worker(&conn, stream, 1_000);
    assert_eq!(reason(), None, "a first sight marks nothing");
    assert_eq!(recorded_scope().as_deref(), Some(alpha.as_str()), "the baseline records it");

    publish("src/moved.rs", Some(&alpha));
    drain_worker(&conn, stream, 2_000);
    assert_eq!(reason(), None, "the same scope is a republish of the same target");

    publish("src/twins.rs", Some(&beta));
    drain_worker(&conn, stream, 3_000);
    assert_eq!(
        reason().as_deref(),
        Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str()),
        "a changed scope under an unchanged kind and signature is a retarget",
    );
    assert_eq!(recorded_scope().as_deref(), Some(beta.as_str()));

    // Answered, then republished by an older author that publishes no scopes.
    conn.execute(
        "UPDATE repo_memory_bindings SET relocation_reason = NULL WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();
    publish("src/older.rs", None);
    drain_worker(&conn, stream, 4_000);
    assert_eq!(reason(), None, "a scope missing on the new side is no evidence");
    assert_eq!(recorded_scope(), None);

    publish("src/newer.rs", Some(&alpha));
    drain_worker(&conn, stream, 5_000);
    assert_eq!(reason(), None, "nor is one missing on the recorded side");

    // A malformed scope from a peer is dropped where it would be stored, never quarantined.
    publish("src/odd.rs", Some("not-a-hash"));
    drain_worker(&conn, stream, 6_000);
    assert_eq!(reason(), None);
    assert_eq!(recorded_scope(), None, "an off-shape scope is not recorded");
    assert_eq!(status_of(&conn, "mem_peer"), "active", "and the memory survives");

    // Scopes exposed for the SAME set — an upgrade re-folding retained scope ops, or an author
    // republishing an unchanged set with scopes — fill the baseline in, so the next twin rebind
    // has a recorded scope to differ from.
    set_projected_anchor_field(&conn, stream, "mem_peer", "scope_hash", &alpha);
    drain_worker(&conn, stream, 7_000);
    assert_eq!(reason(), None, "filling the baseline marks nothing");
    assert_eq!(recorded_scope().as_deref(), Some(alpha.as_str()), "the baseline gains it");
    publish("src/twins-again.rs", Some(&beta));
    drain_worker(&conn, stream, 8_000);
    assert_eq!(
        reason().as_deref(),
        Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str()),
        "and the rebind that follows is a retarget",
    );

    // A known scope that changes under an unchanged set replaces the recorded one without a
    // mark: a rebind always changes the set's bytes, so this is a change of derivation.
    conn.execute(
        "UPDATE repo_memory_bindings SET relocation_reason = NULL WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();
    let gamma = rag_rat_base::hash::hex_sha256(b"Twin as Gamma");
    set_projected_anchor_field(&conn, stream, "mem_peer", "scope_hash", &gamma);
    drain_worker(&conn, stream, 9_000);
    assert_eq!(reason(), None, "a scope moving under identical bytes marks nothing");
    assert_eq!(recorded_scope().as_deref(), Some(gamma.as_str()), "but is recorded");

    // A row relocated here since the set was applied is not the author's target any more, so
    // no scope is recorded on its behalf under an unchanged set.
    conn.execute(
        "UPDATE repo_memory_bindings SET signature_hash = 'sig-local' WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();
    let delta = rag_rat_base::hash::hex_sha256(b"Twin as Delta");
    set_projected_anchor_field(&conn, stream, "mem_peer", "scope_hash", &delta);
    drain_worker(&conn, stream, 9_500);
    assert_eq!(recorded_scope().as_deref(), Some(gamma.as_str()), "a relocated row is skipped");
    conn.execute(
        "UPDATE repo_memory_bindings SET signature_hash = 'sig' WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();
    drain_worker(&conn, stream, 9_600);
    assert_eq!(recorded_scope().as_deref(), Some(delta.as_str()), "matched again, recorded");

    // A scope withdrawn under an unchanged set is cleared without a mark, so the rebind that
    // follows compares against nothing.
    conn.execute(
        "UPDATE repo_memory_bindings SET relocation_reason = NULL WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE content_projected_nodes
             SET anchors_json = json_remove(anchors_json, '$[0].scope_hash')
             WHERE stream_id = ?1 AND node_id = 'mem_peer'",
        params![stream.to_bytes().as_slice()],
    )
    .unwrap();
    drain_worker(&conn, stream, 10_000);
    assert_eq!(reason(), None, "a withdrawal marks nothing");
    assert_eq!(recorded_scope(), None, "and clears the baseline");
    publish("src/after-withdrawal.rs", Some(&beta));
    drain_worker(&conn, stream, 11_000);
    assert_eq!(reason(), None, "the next scope compares against nothing");
}

/// A set first recorded against rows that are not its own — here a seed from before the
/// drain converged, still on the struct the author has since left — is no baseline for them:
/// the author's next republish of the impl must still mark the row, or its struct handle
/// wins every pick.
#[test]
fn a_set_recorded_over_foreign_rows_is_no_baseline_for_them() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node(&conn, stream, "mem_peer", "Invariant", "t", "b", "active", &[]);
    drain_worker(&conn, stream, 500);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, symbol_kind, logical_symbol_id,
                 anchor_status, created_at_ms)
             VALUES ((SELECT repo_id FROM repo_memories WHERE id = 'mem_peer'), 'mem_peer',
                     'symbol', 'src/lib.rs::Worker', 'struct', 42, 'current', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE repo_memories SET anchors_applied_digest = NULL, anchors_applied_targets = NULL
             WHERE id = 'mem_peer'",
        [],
    )
    .unwrap();
    let publish = |path: &str| {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::Worker")]),
        );
        set_projected_anchor_field(&conn, stream, "mem_peer", "path", path);
        set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "impl");
    };
    publish("src/lib.rs");
    drain_worker(&conn, stream, 1_000);
    publish("src/moved.rs");
    drain_worker(&conn, stream, 2_000);

    let reason: Option<String> = conn
        .query_row(
            "SELECT relocation_reason FROM repo_memory_bindings WHERE memory_id = 'mem_peer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        reason.as_deref(),
        Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str())
    );
}

/// An author cannot publish an unbinding, so a memory whose rows another device removed
/// through `anchors/1` — a quarantine on an older binary, a re-point — while the author's set
/// stays unchanged is re-seeded from that set, as a first sight would be.
#[test]
fn bindings_removed_under_an_unchanged_set_are_reseeded() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 1_000);
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    // Touch the projection without changing the set, so the stream drains again.
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run".to_string()
    )]);
}

/// An edge binding's id is its fingerprint, which names the exact edge, so a republish that
/// still names it keeps the cached `edge_id` — the only thing keeping an edge a linked worktree
/// alone holds from reading `gone` in the base checkout.
#[test]
fn a_refresh_keeps_an_edge_bindings_cached_id() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let publish = |path: &str| {
        seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("edge", "fp-1")]));
        set_projected_anchor_field(&conn, stream, "mem_peer", "path", path);
    };
    publish("src/lib.rs");
    drain_worker(&conn, stream, 1_000);
    conn.execute("UPDATE repo_memory_bindings SET edge_id = 77 WHERE memory_id = 'mem_peer'", [])
        .unwrap();

    publish("src/moved.rs");
    drain_worker(&conn, stream, 2_000);

    let (path, edge_id): (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT path, edge_id FROM repo_memory_bindings WHERE memory_id = 'mem_peer'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(path.as_deref(), Some("src/moved.rs"), "the refresh ran");
    assert_eq!(edge_id, Some(77));
}

/// A chunk binding relocates on every re-chunk without a new snapshot, and on a device of the
/// author's own account `anchors/1` delivers the relocated row. When the author's next snapshot
/// still names the pre-relocation chunk, the drain cannot insert it, so it must not delete the
/// relocated row either: the memory would be unbound here, and `anchors/1` would carry that
/// delete to every device.
#[test]
fn a_relocated_chunk_binding_the_drain_cannot_reinsert_is_kept() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 1_000);
    // The author rebinds to chunk 42 and re-chunks; `anchors/1` delivers the rebind and the
    // relocation to 43 before this device drains the new snapshot.
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'chunk', '43', 'current', 1)",
        [REPO],
    )
    .unwrap();

    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("chunk", "42")]));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![("chunk".to_string(), "43".to_string())]);
}

/// A held row is matched by TARGET, not identity alone. A struct binding kept from before the
/// upgrade shares a qualified name with the impl its author has since published: it is the
/// image of the publication the impl superseded, so on first sight the row converges on the
/// impl — marked `retargeted`, judged against its own struct — and the hash, which describes
/// the impl, is stamped beside it.
#[test]
fn a_kept_binding_whose_target_moved_converges_and_takes_the_published_hash() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", None);
    drain_worker(&conn, stream, 1_000);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 symbol_kind, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::Run', 'src/lib.rs', 1, 2, 'struct',
                 'current', 7)",
        [REPO],
    )
    .unwrap();

    // The struct was published first; the impl superseded it.
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::Run")]),
    );
    set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "struct");
    let struct_set = projected_anchors_json(&conn, stream, "mem_peer");
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::Run")]),
    );
    set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "impl");
    set_projected_superseded_anchors(&conn, stream, "mem_peer", &foreign, &[struct_set]);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 2_000);

    assert_eq!(
        binding_of(&conn, "mem_peer"),
        Some((
            Some("impl".to_string()),
            Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str().to_string())
        )),
    );
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));
}

/// Rows held with no applied set and no parked baseline — a device that joined the account,
/// or re-synced after a purge, while the memory was condemned everywhere — are the image of
/// the set the siblings last converged to. When the memory returns under a rebind, the fold
/// has superseded that set, so the rows converge on the published one at once: nothing else on
/// this device would ever move them, since every later pass sees an unchanged set (#1304).
#[test]
fn a_returning_memory_with_rows_held_and_nothing_parked_converges_on_its_rebind() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    // The rows arrived through `anchors/1`; the memory never existed here.
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 symbol_kind, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::Run', 'src/lib.rs', 1, 2, 'struct',
                 'unverified', 3)",
        [REPO],
    )
    .unwrap();
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_none());
    assert!(parked_digest(&conn, "mem_peer").is_none());

    // The publication the rows are the image of, then the rebind that superseded it.
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::Run")]),
    );
    set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "struct");
    set_projected_anchor_stamp(&conn, stream, "mem_peer", 3);
    let struct_set = projected_anchors_json(&conn, stream, "mem_peer");
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::Run")]),
    );
    set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "impl");
    set_projected_superseded_anchors(&conn, stream, "mem_peer", &foreign, &[struct_set]);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 1_000);

    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_some());
    assert_eq!(
        binding_of(&conn, "mem_peer"),
        Some((
            Some("impl".to_string()),
            Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str().to_string())
        )),
        "the held struct row is an older image and converges on the impl",
    );
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));
    let digest: Option<String> = conn
        .query_row(
            "SELECT anchors_applied_digest FROM repo_memories WHERE id = 'mem_peer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(digest.is_some(), "the set is recorded as applied");
}

/// The image is judged as a whole: a superseded image that still matches SOME of the published
/// anchors — the author rebound one of two, or added one — is converged too, so the changed
/// anchor lands and the added one is installed, while the matching row is refreshed in place.
#[test]
fn a_superseded_image_matching_part_of_the_published_set_still_converges() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    for id in ["src/lib.rs::a", "src/lib.rs::b"] {
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                     anchor_status, created_at_ms)
                 VALUES (?1, 'mem_peer', 'symbol', ?2, 'src/lib.rs', 1, 2, 'current', 3)",
            params![REPO, id],
        )
        .unwrap();
    }
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::a"), ("symbol", "src/lib.rs::b")]),
    );
    set_projected_anchor_stamp(&conn, stream, "mem_peer", 3);
    let first = projected_anchors_json(&conn, stream, "mem_peer");
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::a"), ("symbol", "src/lib.rs::c")]),
    );
    set_projected_superseded_anchors(
        &conn,
        stream,
        "mem_peer",
        &foreign,
        std::slice::from_ref(&first),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 1_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![
        ("symbol".to_string(), "src/lib.rs::a".to_string()),
        ("symbol".to_string(), "src/lib.rs::c".to_string()),
    ]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));

    // The author adds an anchor to a set this store holds the exact image of.
    conn.execute("DELETE FROM repo_memories WHERE id = 'mem_peer'", []).unwrap();
    conn.execute("DELETE FROM repo_memory_parked_baselines WHERE memory_id = 'mem_peer'", [])
        .unwrap();
    let second = projected_anchors_json(&conn, stream, "mem_peer");
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[
            ("symbol", "src/lib.rs::a"),
            ("symbol", "src/lib.rs::c"),
            ("symbol", "src/lib.rs::d"),
        ]),
    );
    set_projected_superseded_anchors(&conn, stream, "mem_peer", &foreign, &[first, second]);
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 2_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![
        ("symbol".to_string(), "src/lib.rs::a".to_string()),
        ("symbol".to_string(), "src/lib.rs::c".to_string()),
        ("symbol".to_string(), "src/lib.rs::d".to_string()),
    ]);
}

/// Held rows that are the image of no publication the fold has seen — a sibling's rebind whose
/// set has yet to fold here — are recorded, not converged: converging would republish the older
/// rows to the sibling through `anchors/1`. The set that explains them converges when it
/// arrives, as any set change does.
#[test]
fn rows_of_no_known_publication_wait_for_the_set_that_explains_them() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::newer', 'src/lib.rs', 1, 2, 'current',
                 9)",
        [REPO],
    )
    .unwrap();
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::older")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 1_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::newer".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), None, "not the author's rows for that hash");

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::newest")]),
    );
    set_projected_anchor_stamp(&conn, stream, "mem_peer", 11);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_B));
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 2_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::newest".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// The rows a sibling's OWN rebind left, received ahead of the set it published: the foreign
/// set folded here is older, but the rows match none of its superseded publications, so they
/// are kept — and when the sibling's set folds as this account's own, `anchors/1` is its
/// carrier and the rows stand.
#[test]
fn a_siblings_own_rebind_rows_are_kept_until_its_set_folds() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (own, foreign) = own_and_foreign_accounts(&conn);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::ours', 'src/lib.rs', 1, 2, 'current',
                 5)",
        [REPO],
    )
    .unwrap();
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::first")]),
    );
    set_projected_anchor_stamp(&conn, stream, "mem_peer", 3);
    let first = projected_anchors_json(&conn, stream, "mem_peer");
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::theirs")]),
    );
    set_projected_superseded_anchors(&conn, stream, "mem_peer", &foreign, &[first]);
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 1_000);
    assert_eq!(
        bindings_of(&conn, "mem_peer"),
        vec![("symbol".to_string(), "src/lib.rs::ours".to_string())],
        "rows of no known publication are not overwritten by an older foreign set",
    );

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::ours")]),
    );
    set_projected_anchor_stamp(&conn, stream, "mem_peer", 5);
    set_projected_anchors_author(&conn, stream, "mem_peer", &own);
    drain_worker(&conn, stream, 2_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::ours".to_string()
    )]);
}

/// A superseded publication THIS account made is never converged on: a sibling's later
/// republication of the unchanged set (the anchor sweep keeps the stamps) is the same image,
/// and it arrives as this account's own, which nothing converges — so rows matching the older
/// one may be that republication's, and overwriting them would stand for good.
#[test]
fn a_superseded_publication_of_this_account_is_not_converged_on() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (own, foreign) = own_and_foreign_accounts(&conn);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::ours', 'src/lib.rs', 1, 2, 'current',
                 5)",
        [REPO],
    )
    .unwrap();
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::ours")]),
    );
    set_projected_anchor_stamp(&conn, stream, "mem_peer", 5);
    let ours = projected_anchors_json(&conn, stream, "mem_peer");
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::theirs")]),
    );
    set_projected_superseded_anchors(&conn, stream, "mem_peer", &own, std::slice::from_ref(&ours));
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 1_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::ours".to_string()
    )]);

    // The same image, published by another account, IS stale history.
    conn.execute("DELETE FROM repo_memories WHERE id = 'mem_peer'", []).unwrap();
    set_projected_superseded_anchors(&conn, stream, "mem_peer", &foreign, &[ours]);
    drain_worker(&conn, stream, 2_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::theirs".to_string()
    )]);
}

/// Rows of kinds this store never installs are no image of anything: a sibling's chunk rebind
/// held alone beside an older foreign set whose predecessor is known stays as it is, rather
/// than gaining that set's symbol rows beside it.
#[test]
fn unseedable_rows_alone_are_never_a_superseded_image() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'chunk', '42', 'src/lib.rs', 1, 2, 'current', 9)",
        [REPO],
    )
    .unwrap();
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::first")]),
    );
    let first = projected_anchors_json(&conn, stream, "mem_peer");
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::second")]),
    );
    set_projected_superseded_anchors(&conn, stream, "mem_peer", &foreign, &[first]);
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 1_000);
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![("chunk".to_string(), "42".to_string())]);
}

/// A kind the drain never installs sits outside the image check as it sits outside the
/// converge: a chunk row beside the exact image of the published set does not make the image
/// stale, so the symbol row keeps its resolution — and keeps the author's hash off, as before.
#[test]
fn a_chunk_row_beside_the_published_image_does_not_make_it_stale() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::run', 'src/lib.rs', 1, 2, 'current',
                 7),
                    (?1, 'mem_peer', 'chunk', '42', 'src/lib.rs', 1, 2, 'current', 7)",
        [REPO],
    )
    .unwrap();
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 1_000);
    let status: String = conn
        .query_row(
            "SELECT anchor_status FROM repo_memory_bindings
                 WHERE memory_id = 'mem_peer' AND binding_kind = 'symbol'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "current", "the image is left as it is");
    assert_eq!(source_hash_of(&conn, "mem_peer"), None, "the chunk is not the author's");
}

/// The validate loop rewrites a binding's location in place for the SAME target — here a
/// symbol that moved down its file. Bindings kept by the record-only branch are matched on the
/// target, not its location, so the author's hash still reaches them.
#[test]
fn a_kept_binding_that_only_moved_lines_takes_the_published_hash() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", None);
    drain_worker(&conn, stream, 1_000);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::run', 'src/lib.rs', 5, 6, 'current',
                 1)",
        [REPO],
    )
    .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));
}

/// Converging never removes a kind this store cannot install, so a rebind made here onto a
/// chunk outlives an author's set that names no chunk at all. The author's new hash does not
/// describe that row, so the rebind keeps its own.
#[test]
fn a_kept_chunk_the_new_set_names_nothing_like_keeps_the_authors_hash_off() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);
    // A rebind made here, onto a chunk.
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'chunk', '43', 'current', 1)",
        [REPO],
    )
    .unwrap();
    conn.execute("UPDATE repo_memories SET source_text_hash = ?1 WHERE id = 'mem_peer'", [HASH_B])
        .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/other.rs::walk")]),
    );
    let hash_c = "c".repeat(64);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(&hash_c));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![
        ("chunk".to_string(), "43".to_string()),
        ("symbol".to_string(), "src/other.rs::walk".to_string()),
    ]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// Named rows of a kind this store cannot install keep their local resolution across a set
/// change: their checkout-local id IS that resolution, and a refresh would clear it for good.
/// A chunk binding's id is a checkout-local rowid, so a receiver's own chunk rebind can carry
/// the very id the author's chunk anchor names on the author's store. Equal ids say nothing
/// about the target: the author's hash must stay off the receiver's chunk.
#[test]
fn a_local_chunk_sharing_the_authors_chunk_id_keeps_the_authors_hash_off() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 1_000);
    // Rebound here to this store's chunk 42, with its own hash.
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, chunk_id, anchor_status,
                 created_at_ms)
             VALUES (?1, 'mem_peer', 'chunk', '42', 42, 'current', 1)",
        [REPO],
    )
    .unwrap();
    conn.execute("UPDATE repo_memories SET source_text_hash = ?1 WHERE id = 'mem_peer'", [HASH_B])
        .unwrap();

    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("chunk", "42")]));
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

#[test]
fn named_bindings_this_store_cannot_seed_keep_their_resolution() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    drain_worker(&conn, stream, 1_000);
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, chunk_id, anchor_status,
                 created_at_ms)
             VALUES (?1, 'mem_peer', 'chunk', '42', 99, 'current', 1),
                    (?1, 'mem_peer', 'call_path', 'seq', NULL, 'current', 1)",
        [REPO],
    )
    .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("chunk", "42"), ("call_path", "seq")]),
    );
    drain_worker(&conn, stream, 2_000);

    let rows: Vec<(String, Option<i64>, String)> = conn
        .prepare(
            "SELECT binding_kind, chunk_id, anchor_status FROM repo_memory_bindings
                 WHERE memory_id = 'mem_peer' ORDER BY binding_kind",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, vec![
        ("call_path".to_string(), None, "current".to_string()),
        ("chunk".to_string(), Some(99), "current".to_string()),
    ]);
}

/// The author's new hash can arrive ahead of its set — a pull that stops between the two. The
/// ownership check runs on that hash-only pass too, so a chunk rebound here keeps its own hash,
/// both then and once the set lands.
#[test]
fn a_hash_arriving_ahead_of_its_set_stays_off_a_local_rebind() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);
    // A rebind made here, onto a chunk.
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'chunk', '43', 'current', 1)",
        [REPO],
    )
    .unwrap();
    conn.execute("UPDATE repo_memories SET source_text_hash = ?1 WHERE id = 'mem_peer'", [HASH_B])
        .unwrap();

    // The author's new hash lands; its set has not yet.
    let hash_c = "c".repeat(64);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(&hash_c));
    drain_worker(&conn, stream, 2_000);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));

    // Then the set.
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/other.rs::walk")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(&hash_c));
    drain_worker(&conn, stream, 3_000);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// A chunk rebound here is not the author's chunk just because the author's set names a chunk
/// too: ownership is judged by target, never by kind. The rebind keeps its own hash.
#[test]
fn a_kept_chunk_is_not_the_authors_because_the_set_names_another_chunk() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'chunk', '43', 'current', 1)",
        [REPO],
    )
    .unwrap();
    conn.execute("UPDATE repo_memories SET source_text_hash = ?1 WHERE id = 'mem_peer'", [HASH_B])
        .unwrap();

    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("chunk", "42")]));
    let hash_c = "c".repeat(64);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(&hash_c));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![("chunk".to_string(), "43".to_string())]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// A set this store's own account authored reaches it through `anchors/1` too, which carries
/// every later rebind and relocation — so the drain converges nothing onto rows it holds. The
/// renamed symbol and the re-chunked chunk `anchors/1` delivered are newer than the snapshot
/// naming their earlier forms, and a memory holding nothing is still seeded.
#[test]
fn an_own_authored_set_writes_nothing_over_rows_the_store_holds() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (own, _) = own_and_foreign_accounts(&conn);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_peer", &own);
    drain_worker(&conn, stream, 1_000);
    assert_eq!(
        bindings_of(&conn, "mem_peer"),
        vec![("symbol".to_string(), "src/lib.rs::run".to_string())],
        "a memory holding nothing is seeded",
    );
    // `anchors/1` delivers the author's newer rows: a rename and a re-chunk.
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'symbol', 'src/lib.rs::run_renamed', 'current', 1),
                    (?1, 'mem_peer', 'chunk', '43', 'current', 1)",
        [REPO],
    )
    .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::walk"), ("chunk", "42")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_peer", &own);
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![
        ("chunk".to_string(), "43".to_string()),
        ("symbol".to_string(), "src/lib.rs::run_renamed".to_string()),
    ]);
}

/// The own path stamps the author's hash without matching the held rows to the set: they are
/// this account's own, converging to the same rebind through `anchors/1`, so the hash lands
/// even while they catch up.
#[test]
fn an_own_authored_set_stamps_its_hash_while_the_rows_catch_up() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (own, _) = own_and_foreign_accounts(&conn);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_peer", &own);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/other.rs::walk")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_peer", &own);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_B));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// Even on the own path a memory holding no binding takes no hash: a chunk-only set seeds
/// nothing here, and the hash would describe nothing.
#[test]
fn an_own_authored_set_with_no_binding_held_takes_no_hash() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (own, _) = own_and_foreign_accounts(&conn);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("chunk", "42")]));
    set_projected_anchors_author(&conn, stream, "mem_peer", &own);
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);

    assert!(bindings_of(&conn, "mem_peer").is_empty());
    assert_eq!(source_hash_of(&conn, "mem_peer"), None);
}

/// The own path still records the applied digest, so when another account publishes the next
/// set the memory converges to it instead of taking the record-only branch.
#[test]
fn a_foreign_set_after_an_own_one_converges() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (own, foreign) = own_and_foreign_accounts(&conn);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_peer", &own);
    drain_worker(&conn, stream, 1_000);

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/other.rs::walk")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/other.rs::walk".to_string()
    )]);
}

/// And the other way: once the winning set is this account's own, the drain leaves the rows
/// it converged earlier to `anchors/1`, which carries the rebind that replaced them.
#[test]
fn an_own_set_after_a_foreign_one_writes_no_bindings() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (own, foreign) = own_and_foreign_accounts(&conn);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    drain_worker(&conn, stream, 1_000);

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/other.rs::walk")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_peer", &own);
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run".to_string()
    )]);
}

/// Set one field of the first anchor in a projected node's published snapshot.
/// The projected set's JSON as stored, for handing to [`set_projected_superseded_anchors`]
/// before a later publication replaces it.
fn projected_anchors_json(conn: &Connection, stream: StreamId, node_id: &str) -> String {
    conn.query_row(
        "SELECT anchors_json FROM content_projected_nodes WHERE stream_id = ?1 AND node_id = ?2",
        params![stream.to_bytes().as_slice(), node_id],
        |row| row.get(0),
    )
    .unwrap()
}

/// Record the publications the fold superseded for a node, oldest first and all by one author,
/// as the projector writes them beside the winning set.
fn set_projected_superseded_anchors(
    conn: &Connection,
    stream: StreamId,
    node_id: &str,
    author: &rag_rat_oplog::AccountId,
    sets: &[String],
) {
    let hex = rag_rat_base::hash::hex_lower(&author.to_bytes());
    let rows: Vec<String> = sets
        .iter()
        .map(|anchors| format!("{{\"author\":\"{hex}\",\"anchors\":{anchors}}}"))
        .collect();
    conn.execute(
        "UPDATE content_projected_nodes SET superseded_anchors_json = ?3
             WHERE stream_id = ?1 AND node_id = ?2",
        params![stream.to_bytes().as_slice(), node_id, format!("[{}]", rows.join(","))],
    )
    .unwrap();
}

/// Restamp every anchor of a projected set with one publication clock, as a rebind does.
fn set_projected_anchor_stamp(conn: &Connection, stream: StreamId, node_id: &str, ms: i64) {
    conn.execute(
        "UPDATE content_projected_nodes
             SET anchors_json = (SELECT json_group_array(json_set(value, '$.created_at_ms', ?3))
                                   FROM json_each(anchors_json))
             WHERE stream_id = ?1 AND node_id = ?2",
        params![stream.to_bytes().as_slice(), node_id, ms],
    )
    .unwrap();
}

fn set_projected_anchor_field(
    conn: &Connection,
    stream: StreamId,
    node_id: &str,
    field: &str,
    value: &str,
) {
    conn.execute(
        "UPDATE content_projected_nodes
             SET anchors_json = json_set(anchors_json, '$[0].' || ?3, ?4)
             WHERE stream_id = ?1 AND node_id = ?2",
        params![stream.to_bytes().as_slice(), node_id, field, value],
    )
    .unwrap();
}

/// A new set can give an unchanged hash its first binding. A memory bound only to a chunk,
/// which this store never seeds, takes no stamp; a rebind to the symbol over the same text
/// republishes the same hash. The stamp is reconsidered on the set's change, or the memory
/// would keep a NULL hash for good.
#[test]
fn a_changed_set_that_gives_the_hash_a_binding_stamps_it() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("chunk", "42")]));
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);
    assert_eq!(source_hash_of(&conn, "mem_peer"), None, "nothing held for it to describe");

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_A.to_string()));
}

/// A binding's identity can survive a rebind whose target moved: a struct and its impl share a
/// qualified name, and only `symbol_kind` / `signature_hash` differ. The still-named row takes
/// the author's new values and drops the resolution it held for the old target, so the
/// validate loop re-resolves it instead of checking the new hash against the old text.
#[test]
fn a_still_named_binding_whose_target_moved_is_refreshed() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::Run")]),
    );
    set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "struct");
    drain_worker(&conn, stream, 1_000);
    conn.execute(
        "UPDATE repo_memory_bindings
             SET anchor_status = 'current', symbol_id = 42, logical_symbol_id = 7
             WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::Run")]),
    );
    set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "impl");
    drain_worker(&conn, stream, 2_000);

    let row: (Option<String>, String, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT symbol_kind, anchor_status, symbol_id, logical_symbol_id
                 FROM repo_memory_bindings WHERE memory_id = 'mem_peer'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (Some("impl".to_string()), "unverified".to_string(), None, Some(7)),
        "the raw ids and verdict are cleared; the logical handle — the only thing that tells two \
         same-name trait impls apart — is kept",
    );
}

/// A rebind onto a target with no text behind it publishes an EMPTY hash, so the pre-rebind
/// hash does not survive beside bindings it never described.
#[test]
fn a_retracted_hash_is_cleared_beside_the_new_bindings() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);

    seed_projected_node_with_anchors(&conn, stream, "mem_peer", Some(&[("commit", "f00d")]));
    set_projected_source_hash(&conn, stream, "mem_peer", Some(""));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_peer"), vec![("commit".to_string(), "f00d".to_string())]);
    assert_eq!(source_hash_of(&conn, "mem_peer"), None);
}

/// A rebind made HERE on a synced memory keeps its own hash while the author publishes nothing
/// new. The applied hash is recorded apart from the stamped one for exactly this: comparing
/// against the stamped column would put the author's hash back beside the local bindings.
#[test]
fn a_local_rebind_keeps_its_hash_until_the_author_publishes_again() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);
    conn.execute("UPDATE repo_memories SET source_text_hash = ?1 WHERE id = 'mem_peer'", [HASH_B])
        .unwrap();

    drain_worker(&conn, stream, 2_000);

    assert_eq!(source_hash_of(&conn, "mem_peer"), Some(HASH_B.to_string()));
}

/// A LOCAL memory's bindings are its own: a snapshot this account (or nobody known) authored
/// only ever seeds one that holds none, and never replaces one that does — even when the
/// published set changes.
#[test]
fn a_local_memory_that_holds_bindings_is_never_replaced() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_mine",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
                 memory_version, repo_id, origin)
             VALUES ('mem_mine', 'Invariant', 't', 'b', 'high', 'active', 1, 1, 'agent', 'v1', ?1,
                 'local')",
        [REPO],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_mine', 'symbol', 'src/lib.rs::keep', 'src/lib.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();
    drain_worker(&conn, stream, 1_000);

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_mine",
        Some(&[("symbol", "src/other.rs::walk")]),
    );
    set_projected_source_hash(&conn, stream, "mem_mine", Some(HASH_A));
    drain_worker(&conn, stream, 2_000);

    assert_eq!(bindings_of(&conn, "mem_mine"), vec![(
        "symbol".to_string(),
        "src/lib.rs::keep".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_mine"), None, "a local memory's hash is its own");
}

/// A contributor's rebind of a memory this account created is authored by another account, so
/// it reaches this device through the snapshot alone: the creator's local row takes it, on
/// first sight, as a synced row would.
#[test]
fn a_local_memory_takes_a_set_another_account_authored() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
                 memory_version, repo_id, origin)
             VALUES ('mem_mine', 'Invariant', 't', 'b', 'high', 'active', 1, 1, 'agent', 'v1', ?1,
                 'local')",
        [REPO],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_mine', 'symbol', 'src/lib.rs::old', 'src/lib.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();

    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_mine",
        Some(&[("symbol", "src/other.rs::new")]),
    );
    set_projected_anchors_author(&conn, stream, "mem_mine", &foreign);
    set_projected_source_hash(&conn, stream, "mem_mine", Some(HASH_A));
    drain_worker(&conn, stream, 1_000);

    assert_eq!(bindings_of(&conn, "mem_mine"), vec![(
        "symbol".to_string(),
        "src/other.rs::new".to_string()
    )]);
    assert_eq!(source_hash_of(&conn, "mem_mine"), Some(HASH_A.to_string()));
}

/// A memory created here keeps the applied-set bookkeeping under its own account's sets too, so
/// a later foreign set is judged against the set the rows actually hold: here the owner's
/// sibling rebinds the contributor's struct to the impl (rows arriving through `anchors/1`),
/// and the contributor's rebind back to the struct must still mark the row. The own set's hash
/// lands on the creating device as well, since the hash only travels in the snapshot.
#[test]
fn a_local_memory_records_its_own_sets_as_the_baseline() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (own, foreign) = own_and_foreign_accounts(&conn);
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
                 memory_version, repo_id, origin)
             VALUES ('mem_mine', 'Invariant', 't', 'b', 'high', 'active', 1, 1, 'agent', 'v1', ?1,
                 'local')",
        [REPO],
    )
    .unwrap();
    let publish = |author, kind: &str, signature: &str, hash: &str| {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_mine",
            Some(&[("symbol", "src/lib.rs::T")]),
        );
        set_projected_anchor_field(&conn, stream, "mem_mine", "symbol_kind", kind);
        set_projected_anchor_field(&conn, stream, "mem_mine", "signature_hash", signature);
        set_projected_anchors_author(&conn, stream, "mem_mine", author);
        set_projected_source_hash(&conn, stream, "mem_mine", Some(hash));
    };
    let reason = || -> Option<String> {
        conn.query_row(
            "SELECT relocation_reason FROM repo_memory_bindings WHERE memory_id = 'mem_mine'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    publish(&foreign, "struct", "s1", HASH_A);
    drain_worker(&conn, stream, 1_000);
    // The owner's sibling rebinds to the impl: `anchors/1` moves the row in place.
    conn.execute(
        "UPDATE repo_memory_bindings SET symbol_kind = 'impl', signature_hash = 's2',
                 relocation_reason = NULL
             WHERE memory_id = 'mem_mine'",
        [],
    )
    .unwrap();
    publish(&own, "impl", "s2", HASH_B);
    drain_worker(&conn, stream, 2_000);
    assert_eq!(reason(), None, "an own set is carried by `anchors/1`, never marked");
    assert_eq!(source_hash_of(&conn, "mem_mine"), Some(HASH_B.to_string()));

    publish(&foreign, "struct", "s1", HASH_A);
    drain_worker(&conn, stream, 3_000);
    assert_eq!(
        reason().as_deref(),
        Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str())
    );
}

/// The published hash crosses the same untrusted boundary as the content and lands in a column
/// nothing downstream re-derives, so a value outside the shape `hex_sha256` produces is never
/// stored. It costs the memory NOTHING else: the projection re-offers the same bad value on
/// every pass, so quarantining over it would delete the row, its bindings and its FTS shadow
/// permanently the moment one peer published a different digest shape.
#[test]
fn a_malformed_source_hash_leaves_the_memory_intact_and_unstamped() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    seed_projected_node_with_anchors(
        &conn,
        stream,
        "mem_peer",
        Some(&[("symbol", "src/lib.rs::run")]),
    );
    set_projected_source_hash(&conn, stream, "mem_peer", Some("../../etc/passwd"));

    drain_worker(&conn, stream, 1_000);

    let exists: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM repo_memories WHERE id = 'mem_peer')", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(exists, "a bad hash must not cost the peer the memory itself");
    assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
        "symbol".to_string(),
        "src/lib.rs::run".to_string()
    )]);
    assert_eq!(
        source_hash_of(&conn, "mem_peer"),
        None,
        "an unparseable source hash is not persisted",
    );
}

/// Seed one row into `content_projected_edges`. `resolved` is `(target_repo, target_node,
/// anchor_status)` when the projection carries a resolved anchor.
#[allow(clippy::too_many_arguments)]
fn seed_projected_edge(
    conn: &Connection,
    stream: StreamId,
    edge_key: &str,
    source: &str,
    relation: &str,
    target_kind: &str,
    target_anchor: &str,
    present: bool,
) {
    let spec_json = serde_json::json!({
        "source_node_id": source,
        "relation": relation,
        "target_repo_id": REPO,
        "target_kind": target_kind,
        "target_anchor": target_anchor,
        "owner_repo_id": REPO,
    })
    .to_string();
    conn.execute(
        "INSERT INTO content_projected_edges(
                 stream_id, edge_key, spec_json, resolved_json, present)
             VALUES (?1, ?2, ?3, NULL, ?4)",
        params![stream.to_bytes().as_slice(), edge_key, spec_json, present as i64],
    )
    .unwrap();
}

/// Insert a locally-authored `repo_memories` row directly (origin defaults to `local`).
fn insert_local_memory(conn: &Connection, id: &str, title: &str, body: &str, status: &str) {
    insert_local_memory_in_repo(conn, id, title, body, status, REPO);
}

/// As [`insert_local_memory`] but stamped with an explicit `repo_id` — used to plant a SIBLING
/// repo's row for the cross-repo boundary tests.
fn insert_local_memory_in_repo(
    conn: &Connection,
    id: &str,
    title: &str,
    body: &str,
    status: &str,
    repo: &str,
) {
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?2, ?3, 'high', ?4, 'agent', 100, 100, 'agent', 'h', 'v1',
                 ?5)",
        params![id, title, body, status, repo],
    )
    .unwrap();
}

/// Insert a locally-authored edge directly (origin defaults to `local`); returns its key.
fn insert_local_edge(conn: &Connection, source: &str, relation: &str, target: &str) -> String {
    let key = edge_key(source, relation, "node", target);
    conn.execute(
        "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?2, 'node', ?5, ?5, 'current', 100)",
        params![key, REPO, source, relation, target],
    )
    .unwrap();
    key
}

/// Run the in-tx drain worker over `stream` and return its outcome.
fn drain_worker(conn: &Connection, stream: StreamId, now_ms: i64) -> DrainOutcome {
    let tx = conn.unchecked_transaction().unwrap();
    let outcome = drain_synced_stream_in_tx(&tx, REPO, stream, now_ms).unwrap();
    tx.commit().unwrap();
    outcome
}

fn origin_of(conn: &Connection, id: &str) -> String {
    conn.query_row("SELECT origin FROM repo_memories WHERE id = ?1", [id], |r| r.get(0)).unwrap()
}

fn status_of(conn: &Connection, id: &str) -> String {
    conn.query_row("SELECT status FROM repo_memories WHERE id = ?1", [id], |r| r.get(0)).unwrap()
}

fn updated_at_of(conn: &Connection, id: &str) -> i64 {
    conn.query_row("SELECT updated_at_ms FROM repo_memories WHERE id = ?1", [id], |r| r.get(0))
        .unwrap()
}

fn edge_exists(conn: &Connection, key: &str) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM repo_node_edges WHERE edge_key = ?1)", [key], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap()
        != 0
}

fn origin_of_edge(conn: &Connection, key: &str) -> String {
    conn.query_row("SELECT origin FROM repo_node_edges WHERE edge_key = ?1", [key], |r| r.get(0))
        .unwrap()
}

fn fts_row_exists(conn: &Connection, id: &str) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM repo_memory_fts WHERE memory_id = ?1)", [id], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap()
        != 0
}

fn content_entry_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM content_entries", [], |r| r.get(0)).unwrap()
}

// --- Task 1: repo attribution by forward derivation (repo-identity-skew pin) ---

/// The drain derives the owner stream FORWARD from `(repo_id, account_id)` and must land on the
/// EXACT stream the authoring path projected into — the guard against repo-identity skew. The
/// derivation is a pure function of the account + repo (no device/checkout input), so it is
/// stable across calls.
#[test]
fn the_drain_derives_the_same_owner_stream_authoring_projected_into() {
    let conn = scoped_conn();
    let id = create_concept(&conn, "seed");
    let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
    let projected: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM content_projected_nodes WHERE stream_id = ?1 AND node_id = ?2",
            params![stream.to_bytes().as_slice(), id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(projected, 1, "the authored node is projected under the forward-derived stream");
    assert_eq!(
        rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap(),
        stream,
        "the derivation is stable (a pure function of account + repo)",
    );
}

// --- Task 2: node drain, happy path ---

#[test]
fn a_projected_node_materializes_as_a_synced_memory_with_tags() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_x", "Invariant", "title x", "body x", "active", &[
        "beta", "alpha",
    ]);

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.nodes_written, 1);

    let memory = memory_by_id(&conn, "mem_x").unwrap().expect("materialized as a real row");
    assert_eq!(memory.title, "title x");
    assert_eq!(memory.body, "body x");
    assert_eq!(memory.tags, vec!["alpha".to_string(), "beta".to_string()], "tags fanned out");
    assert_eq!(origin_of(&conn, "mem_x"), "synced", "the drain writes an origin='synced' row");
    let repo: String = conn
        .query_row("SELECT repo_id FROM repo_memories WHERE id = 'mem_x'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(repo, REPO, "the synced row is stamped with the drained repo");
}

// --- Task 3: converge a projected local row; leave an UNPROJECTED local row untouched ---

/// A local row that ANOTHER device changed appears in the projection (the account-wide LWW
/// winner) with the new value: the drain CONVERGES the local row to it, preserving
/// `origin='local'` (it is NOT frozen). A local row with NO projection entry is a pending,
/// not-yet-reconciled edit and is left untouched.
#[test]
fn a_projected_local_row_converges_and_an_unprojected_local_row_is_untouched() {
    let conn = scoped_conn();
    // A local row the peer updated + obsoleted: the projection holds the winning value.
    insert_local_memory(&conn, "mem_shared", "OLD title", "old body", "active");
    memory::replace_tags(&conn, "mem_shared", &["oldtag".to_string()]).unwrap();
    // A local row with NO projection entry: a pending local edit not yet reconciled.
    insert_local_memory(&conn, "mem_pending", "pending title", "pending body", "active");
    memory::replace_tags(&conn, "mem_pending", &["pendingtag".to_string()]).unwrap();

    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(
        &conn,
        stream,
        "mem_shared",
        "Decision",
        "NEW title",
        "new body",
        "obsolete",
        &["newtag"],
    );

    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.nodes_written, 1, "the projected local row converges");
    assert_eq!(outcome.nodes_removed, 0, "the unprojected local row is not removed");

    // Converged to the projection value, `origin` preserved as local.
    let (kind, title, body, status, origin): (String, String, String, String, String) = conn
        .query_row(
            "SELECT kind, title, body, status, origin FROM repo_memories WHERE id = 'mem_shared'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), title.as_str(), body.as_str(), status.as_str(), origin.as_str()),
        ("Decision", "NEW title", "new body", "obsolete", "local"),
        "a projected local row converges to the account-wide value, origin preserved",
    );
    assert_eq!(memory::tags_for_memory(&conn, "mem_shared").unwrap(), vec!["newtag".to_string()]);

    // The unprojected local row is byte-for-byte untouched.
    let (title2, origin2): (String, String) = conn
        .query_row("SELECT title, origin FROM repo_memories WHERE id = 'mem_pending'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (title2.as_str(), origin2.as_str()),
        ("pending title", "local"),
        "a local row absent from the projection is a pending edit, left untouched",
    );
    assert_eq!(memory::tags_for_memory(&conn, "mem_pending").unwrap(), vec![
        "pendingtag".to_string()
    ],);
}

/// A memory created on device A, then updated (content) and — separately — obsoleted (status)
/// by device B: A's drain converges its local row to B's value each time, and `origin`
/// stays `'local'` throughout (A remains the author; convergence is not an ownership
/// change).
#[test]
fn a_remote_update_then_obsolete_converges_the_local_row_preserving_origin() {
    let conn = scoped_conn();
    insert_local_memory(&conn, "mem_a", "v1 title", "v1 body", "active");
    let stream = StreamId::from_bytes([0x44; 32]);

    // Device B updated the content.
    seed_projected_node(&conn, stream, "mem_a", "Invariant", "v2 title", "v2 body", "active", &[]);
    let out = drain_worker(&conn, stream, 1_000);
    assert_eq!(out.nodes_written, 1);
    assert_eq!(
        memory_by_id(&conn, "mem_a").unwrap().unwrap().body,
        "v2 body",
        "A converges to B's content",
    );
    assert_eq!(origin_of(&conn, "mem_a"), "local", "convergence preserves A's authorship");

    // Device B then obsoleted it.
    conn.execute(
        "UPDATE content_projected_nodes SET status = 'obsolete' WHERE node_id = 'mem_a'",
        [],
    )
    .unwrap();
    let out = drain_worker(&conn, stream, 2_000);
    assert_eq!(out.nodes_written, 1);
    assert_eq!(status_of(&conn, "mem_a"), "obsolete", "A converges to B's status");
    assert_eq!(origin_of(&conn, "mem_a"), "local", "still A's row, still local");
}

// --- Task 4: status flip is an UPDATE, never a DELETE ---

#[test]
fn a_projected_obsolete_status_updates_the_synced_row_and_does_not_delete_it() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_x", "Invariant", "t", "b", "active", &[]);
    drain_worker(&conn, stream, 1_000);
    assert_eq!(status_of(&conn, "mem_x"), "active");

    conn.execute(
        "UPDATE content_projected_nodes SET status = 'obsolete' WHERE node_id = 'mem_x'",
        [],
    )
    .unwrap();
    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.nodes_written, 1, "the status change is a write");
    assert_eq!(outcome.nodes_removed, 0, "an obsolete status flip is never a delete");
    assert_eq!(status_of(&conn, "mem_x"), "obsolete");
    assert!(memory_by_id(&conn, "mem_x").unwrap().is_some(), "the row still exists");
}

// --- Task 5: edge drain + FK order ---

#[test]
fn a_present_edge_upserts_a_synced_edge_and_a_tombstone_removes_it() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
    seed_projected_node(&conn, stream, "mem_b", "Invariant", "b", "b", "active", &[]);
    let key = edge_key("mem_a", "relates_to", "node", "mem_b");
    seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", "mem_b", true);

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.nodes_written, 2, "both source and target nodes materialize first");
    assert_eq!(outcome.edges_written, 1);
    let (origin, source): (String, String) = conn
        .query_row(
            "SELECT origin, source_node_id FROM repo_node_edges WHERE edge_key = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((origin.as_str(), source.as_str()), ("synced", "mem_a"));

    conn.execute("UPDATE content_projected_edges SET present = 0 WHERE edge_key = ?1", [&key])
        .unwrap();
    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.edges_removed, 1, "the tombstone removes the synced edge");
    assert!(!edge_exists(&conn, &key));
}

/// A local edge that ANOTHER device removed appears in the projection as a `present=0`
/// tombstone; the drain must remove it here too — a peer's remove is the account-wide
/// winner, so the tombstone path is NOT origin-gated (the P1 convergence bug: freezing it
/// left the edge forever).
#[test]
fn a_tombstone_removes_a_local_edge_of_the_same_key() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    insert_local_memory(&conn, "mem_a", "a", "b", "active");
    let key = insert_local_edge(&conn, "mem_a", "relates_to", "mem_b");
    assert_eq!(origin_of_edge(&conn, &key), "local");
    // The projection carries a present=0 tombstone for the SAME key.
    seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", "mem_b", false);

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.edges_removed, 1, "a peer's remove converges regardless of origin");
    assert!(!edge_exists(&conn, &key), "the local edge a peer removed is removed here too");
}

/// A local edge whose durable spec (`target_repo_id`) another device changed converges to the
/// projection value, preserving `origin='local'` AND the per-device resolution triple
/// (`target_node_id` / `anchor_status`) — the read path owns resolution, the drain must not
/// wipe it.
#[test]
fn a_remote_spec_change_converges_a_local_edge_preserving_origin_and_resolution() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    insert_local_memory(&conn, "mem_a", "a", "b", "active");
    // A local, resolved edge whose stored target repo is stale (add-time snapshot).
    let key = edge_key("mem_a", "relates_to", "node", "mem_b");
    conn.execute(
        "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, 'mem_a', 'relates_to', 'stale-target-repo', 'node', 'mem_b', 'mem_b',
                 'current', 100)",
        params![key, REPO],
    )
    .unwrap();
    // The projection carries the peer's current target repo for the same key.
    conn.execute(
        "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json, \
         present)
             VALUES (?1, ?2, ?3, NULL, 1)",
        params![
            stream.to_bytes().as_slice(),
            key,
            serde_json::json!({
                "source_node_id": "mem_a",
                "relation": "relates_to",
                "target_repo_id": "current-target-repo",
                "target_kind": "node",
                "target_anchor": "mem_b",
                "owner_repo_id": REPO,
            })
            .to_string(),
        ],
    )
    .unwrap();

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.edges_written, 1, "the durable spec converges");
    let (repo_id, target_repo, target_node, anchor, origin): (
        String,
        String,
        Option<String>,
        String,
        String,
    ) = conn
        .query_row(
            "SELECT repo_id, target_repo_id, target_node_id, anchor_status, origin
                 FROM repo_node_edges WHERE edge_key = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(target_repo, "current-target-repo", "target_repo_id converges to the peer value");
    assert_eq!(repo_id, REPO, "owner repo unchanged");
    assert_eq!(origin, "local", "convergence preserves the local origin");
    assert_eq!(
        (target_node.as_deref(), anchor.as_str()),
        (Some("mem_b"), "current"),
        "the per-device resolution triple is preserved, not wiped to unresolved",
    );

    // Idempotent: a re-drain now matches the durable spec and writes nothing.
    let again = drain_worker(&conn, stream, 2_000);
    assert_eq!(again, DrainOutcome::default(), "converged edge is a no-op on re-drain");
}

// --- Task 6: retro-condemn removal ---

#[test]
fn a_condemned_synced_node_is_removed_on_re_drain_and_a_local_row_survives() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_synced", "Invariant", "s", "b", "active", &[]);
    drain_worker(&conn, stream, 1_000);
    assert!(memory_by_id(&conn, "mem_synced").unwrap().is_some());

    // A local row absent from the projection is a genuine local ghost, NOT a condemned synced
    // row — the origin gate must spare it.
    insert_local_memory(&conn, "mem_local", "l", "b", "active");
    // Retro-condemn: the synced node's projection row vanishes entirely.
    conn.execute("DELETE FROM content_projected_nodes WHERE node_id = 'mem_synced'", []).unwrap();

    assert!(fts_row_exists(&conn, "mem_synced"), "the materialized synced row has an FTS row");

    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.nodes_removed, 1);
    assert!(
        memory_by_id(&conn, "mem_synced").unwrap().is_none(),
        "the condemned synced row is removed",
    );
    assert!(
        !fts_row_exists(&conn, "mem_synced"),
        "the contentless FTS shadow (no FK) is cleaned up in the same txn, not orphaned",
    );
    assert!(
        memory_by_id(&conn, "mem_local").unwrap().is_some(),
        "a local row of an absent id survives the origin gate",
    );
}

/// Removing a condemned synced memory keeps its bindings: they replicate on `anchors/1`, and a
/// delete would publish a `Remove` to every device of the account, the ones where the memory
/// is still live included. The applied-anchor baseline the row carried is parked with them, so
/// when the memory returns a rebind its author published while it was away lands rather than
/// being recorded as already applied (#1298).
#[test]
fn a_condemned_memory_keeps_its_bindings_and_takes_a_rebind_published_while_away() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    let publish = |symbol_kind: &str| {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::Run")]),
        );
        set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", symbol_kind);
        set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
        set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    };
    publish("struct");
    drain_worker(&conn, stream, 1_000);
    assert_eq!(binding_of(&conn, "mem_peer"), Some((Some("struct".to_string()), None)));
    assert_eq!(source_hash_of(&conn, "mem_peer").as_deref(), Some(HASH_A));

    // Retro-condemned: the projection row vanishes, and the mirror goes with it.
    conn.execute("DELETE FROM content_projected_nodes WHERE node_id = 'mem_peer'", []).unwrap();
    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.nodes_removed, 1);
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_none(), "the row is removed");
    assert!(!fts_row_exists(&conn, "mem_peer"), "and its FTS shadow with it");
    assert_eq!(
        binding_of(&conn, "mem_peer"),
        Some((Some("struct".to_string()), None)),
        "the binding stays, for anchors/1 to carry",
    );
    assert!(parked_digest(&conn, "mem_peer").is_some(), "the baseline is parked");

    // The author rebinds to the impl while the memory is away; then the memory returns.
    publish("impl");
    drain_worker(&conn, stream, 3_000);
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_some(), "the memory is back");
    assert_eq!(
        binding_of(&conn, "mem_peer"),
        Some((
            Some("impl".to_string()),
            Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str().to_string())
        )),
        "the rebind published while it was away lands as a retarget",
    );
    assert_eq!(
        source_hash_of(&conn, "mem_peer").as_deref(),
        Some(HASH_A),
        "and the published hash is stamped beside it again",
    );
    assert!(parked_digest(&conn, "mem_peer").is_none(), "the parked baseline is consumed");
}

/// The quarantine removal — a peer's accepted edit failing local validation — keeps the
/// bindings and parks the baseline the same way. Here the memory returns with its set
/// UNCHANGED: nothing is retargeted, and the published hash — which the insert leaves off the
/// returning row — is stamped beside the kept binding again rather than counted as applied.
#[test]
fn a_quarantined_memory_keeps_its_bindings_and_converges_when_it_returns_valid() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    let publish = || {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::Run")]),
        );
        set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", "struct");
        set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
        set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    };
    publish();
    drain_worker(&conn, stream, 1_000);
    assert_eq!(source_hash_of(&conn, "mem_peer").as_deref(), Some(HASH_A));

    conn.execute(
        "UPDATE content_projected_nodes
             SET content_json = json_set(content_json, '$.kind', 'NotAValidKind')
             WHERE node_id = 'mem_peer'",
        [],
    )
    .unwrap();
    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.nodes_removed, 1, "the stale mirror is removed");
    assert_eq!(binding_of(&conn, "mem_peer"), Some((Some("struct".to_string()), None)));
    assert!(parked_digest(&conn, "mem_peer").is_some());

    // Valid again, the set as it was.
    publish();
    drain_worker(&conn, stream, 3_000);
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_some());
    assert_eq!(
        binding_of(&conn, "mem_peer"),
        Some((Some("struct".to_string()), None)),
        "an unchanged set retargets nothing",
    );
    assert_eq!(
        source_hash_of(&conn, "mem_peer").as_deref(),
        Some(HASH_A),
        "the hash is stamped again on a return with the set unchanged",
    );
    assert!(parked_digest(&conn, "mem_peer").is_none());
}

/// A binding relocated here — validation recorded a resolution off the authored name — still
/// IS the authored anchor: the row's identity matches the published set by target, so on its
/// return the author's hash is stamped beside it, and the resolution is left as this store's.
#[test]
fn a_relocated_binding_keeps_its_source_hash_across_removal() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    let publish = || {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::Run")]),
        );
        set_projected_source_hash(&conn, stream, "mem_peer", Some(HASH_A));
        set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    };
    publish();
    drain_worker(&conn, stream, 1_000);
    assert_eq!(source_hash_of(&conn, "mem_peer").as_deref(), Some(HASH_A));
    conn.execute(
        "UPDATE repo_memory_bindings
                SET resolved = 1, resolved_binding_id = 'src/lib.rs::Moved',
                    resolved_path = path, resolved_start_line = start_line,
                    resolved_end_line = end_line, resolved_symbol_kind = symbol_kind,
                    resolved_signature_hash = signature_hash
              WHERE memory_id = 'mem_peer'",
        [],
    )
    .unwrap();

    conn.execute("DELETE FROM content_projected_nodes WHERE node_id = 'mem_peer'", []).unwrap();
    drain_worker(&conn, stream, 2_000);
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_none());

    publish();
    drain_worker(&conn, stream, 3_000);
    let resolved: Option<String> = conn
        .query_row(
            "SELECT resolved_binding_id FROM repo_memory_bindings WHERE memory_id = 'mem_peer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("src/lib.rs::Moved"),
        "an unchanged set leaves the resolution",
    );
    assert_eq!(
        source_hash_of(&conn, "mem_peer").as_deref(),
        Some(HASH_A),
        "the authored row matches the published anchor, so the hash is stamped again",
    );
}

/// A memory can return ahead of its anchors — a revocation vacates content and set together,
/// and a new author's content can fold before their set does. The parked baseline waits for
/// the pass that carries anchors, and the rebind lands then.
#[test]
fn a_memory_returning_ahead_of_its_anchors_converges_when_they_arrive() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    let publish = |symbol_kind: &str| {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::Run")]),
        );
        set_projected_anchor_field(&conn, stream, "mem_peer", "symbol_kind", symbol_kind);
        set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    };
    publish("struct");
    drain_worker(&conn, stream, 1_000);

    conn.execute("DELETE FROM content_projected_nodes WHERE node_id = 'mem_peer'", []).unwrap();
    drain_worker(&conn, stream, 2_000);
    assert!(parked_digest(&conn, "mem_peer").is_some());

    // Content only, no anchors yet: the memory is back, the baseline stays parked.
    seed_projected_node_with_anchors(&conn, stream, "mem_peer", None);
    drain_worker(&conn, stream, 3_000);
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_some());
    assert!(parked_digest(&conn, "mem_peer").is_some(), "nothing to act on it yet");
    assert_eq!(binding_of(&conn, "mem_peer"), Some((Some("struct".to_string()), None)));

    publish("impl");
    drain_worker(&conn, stream, 4_000);
    assert_eq!(
        binding_of(&conn, "mem_peer"),
        Some((
            Some("impl".to_string()),
            Some(rag_rat_query::memory::RelocationReason::Retargeted.as_db_str().to_string())
        )),
        "the rebind lands on the pass that carries the anchors",
    );
    assert!(parked_digest(&conn, "mem_peer").is_none());
}

/// The hash comes back with the parked baseline, but not past its author: a memory returning
/// with its set unchanged and the hash withdrawn meanwhile is reconsidered and cleared, as a
/// live memory would be on the withdrawal. The content returns a pass ahead of the anchors:
/// the reconsideration belongs to the pass that carries them, whichever that is.
#[test]
fn a_hash_withdrawn_while_a_memory_was_away_does_not_return_with_it() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x44; 32]);
    let (_, foreign) = own_and_foreign_accounts(&conn);
    let publish = |hash: Option<&str>| {
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::Run")]),
        );
        set_projected_source_hash(&conn, stream, "mem_peer", hash);
        set_projected_anchors_author(&conn, stream, "mem_peer", &foreign);
    };
    publish(Some(HASH_A));
    drain_worker(&conn, stream, 1_000);
    assert_eq!(source_hash_of(&conn, "mem_peer").as_deref(), Some(HASH_A));

    conn.execute("DELETE FROM content_projected_nodes WHERE node_id = 'mem_peer'", []).unwrap();
    drain_worker(&conn, stream, 2_000);

    seed_projected_node_with_anchors(&conn, stream, "mem_peer", None);
    drain_worker(&conn, stream, 3_000);
    publish(None);
    drain_worker(&conn, stream, 4_000);
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_some());
    assert_eq!(
        source_hash_of(&conn, "mem_peer"),
        None,
        "the withdrawn hash does not come back with the baseline",
    );
}

/// `(symbol_kind, relocation_reason)` of a memory's one binding, or `None` without one.
fn binding_of(conn: &Connection, memory_id: &str) -> Option<(Option<String>, Option<String>)> {
    conn.query_row(
        "SELECT symbol_kind, relocation_reason FROM repo_memory_bindings
             WHERE memory_id = ?1",
        [memory_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .unwrap()
}

/// The digest parked for a removed memory, or `None` when nothing is parked.
fn parked_digest(conn: &Connection, memory_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT anchors_applied_digest FROM repo_memory_parked_baselines WHERE memory_id = ?1",
        [memory_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .unwrap()
    .flatten()
}

/// A projected edge carrying a PEER's `Rebind` resolution must NOT import it: the durable
/// target repo comes from the signed spec, and the per-device resolution triple is stored
/// `unresolved` for the local read path to recompute (not spliced from another device's
/// view).
#[test]
fn a_projected_rebind_anchor_is_ignored_when_materializing_an_edge() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
    seed_projected_node(&conn, stream, "mem_b", "Invariant", "b", "b", "active", &[]);
    let key = edge_key("mem_a", "relates_to", "node", "mem_b");
    conn.execute(
        "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json, \
         present)
             VALUES (?1, ?2, ?3, ?4, 1)",
        params![
            stream.to_bytes().as_slice(),
            key,
            serde_json::json!({
                "source_node_id": "mem_a",
                "relation": "relates_to",
                "target_repo_id": "spec-repo",
                "target_kind": "node",
                "target_anchor": "mem_b",
                "owner_repo_id": REPO,
            })
            .to_string(),
            serde_json::json!({
                "target_repo_id": "peer-resolved-repo",
                "target_node_id": "peer-node",
                "anchor_status": "gone",
            })
            .to_string(),
        ],
    )
    .unwrap();

    drain_worker(&conn, stream, 1_000);
    let (target_repo, target_node, anchor): (String, Option<String>, String) = conn
        .query_row(
            "SELECT target_repo_id, target_node_id, anchor_status FROM repo_node_edges
                 WHERE edge_key = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        target_repo, "spec-repo",
        "the durable signed target repo is materialized, not the peer's Rebind resolution",
    );
    assert_eq!(
        (target_node.as_deref(), anchor.as_str()),
        (None, "unresolved"),
        "the peer's per-device resolution triple is not imported",
    );
}

#[test]
fn a_condemned_synced_edge_is_removed_on_re_drain() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
    seed_projected_node(&conn, stream, "mem_b", "Invariant", "b", "b", "active", &[]);
    let key = edge_key("mem_a", "relates_to", "node", "mem_b");
    seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", "mem_b", true);
    drain_worker(&conn, stream, 1_000);
    assert!(edge_exists(&conn, &key));

    // The whole edge row (not a present=0 tombstone) vanishes from the projection.
    conn.execute("DELETE FROM content_projected_edges WHERE edge_key = ?1", [&key]).unwrap();
    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.edges_removed, 1, "a vanished synced edge is removed on re-drain");
    assert!(!edge_exists(&conn, &key));
}

// --- Hardening: robustness + cross-repo boundary against malformed peer content ---

/// The node and edge projection registers are independent, so an accepted edge can legitimately
/// reference a source node that was retro-condemned away. Its FK would abort the whole drain
/// (and every open) — the drain must SKIP it and still materialize the rest.
#[test]
fn a_projected_edge_with_a_missing_source_node_is_skipped_not_fatal() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    // A valid node + edge, plus a dangling edge whose source node is not in the projection.
    seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
    seed_projected_node(&conn, stream, "mem_b", "Invariant", "b", "b", "active", &[]);
    let good = edge_key("mem_a", "relates_to", "node", "mem_b");
    seed_projected_edge(&conn, stream, &good, "mem_a", "relates_to", "node", "mem_b", true);
    let dangling = edge_key("ghost", "relates_to", "node", "mem_b");
    seed_projected_edge(&conn, stream, &dangling, "ghost", "relates_to", "node", "mem_b", true);

    // The drain succeeds (no FK abort), materializes the good edge, and skips the dangling one.
    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.nodes_written, 2);
    assert_eq!(outcome.edges_written, 1, "only the edge with a materialized source is written");
    assert!(edge_exists(&conn, &good));
    assert!(!edge_exists(&conn, &dangling), "the dangling edge is skipped, not fatal");
}

/// Node id is a global PK. A peer stream naming an id another repo already owns must NOT
/// overwrite that sibling's row.
#[test]
fn a_projected_node_owned_by_a_sibling_repo_is_not_overwritten() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    // A sibling repo already owns `mem_shared`.
    insert_local_memory_in_repo(
        &conn,
        "mem_shared",
        "SIBLING title",
        "sibling body",
        "active",
        "repo-b",
    );
    // Our stream projects a node with the same id, different content.
    seed_projected_node(
        &conn,
        stream,
        "mem_shared",
        "Decision",
        "HOSTILE title",
        "hostile body",
        "obsolete",
        &[],
    );

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.nodes_written, 0, "a sibling repo's row is never converged");
    let (title, repo_id): (String, String) = conn
        .query_row("SELECT title, repo_id FROM repo_memories WHERE id = 'mem_shared'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (title.as_str(), repo_id.as_str()),
        ("SIBLING title", "repo-b"),
        "the sibling row is byte-for-byte unchanged",
    );
}

/// An edge's `owner_repo_id` is self-declared in the peer-signed spec. An edge in OUR stream
/// claiming a foreign owner must not be injected into that sibling repo.
#[test]
fn a_projected_edge_claiming_a_foreign_owner_repo_is_not_injected() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
    let key = edge_key("mem_a", "relates_to", "node", "mem_b");
    // A hostile edge in our stream claiming owner_repo_id = a sibling repo.
    conn.execute(
        "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json, \
         present)
             VALUES (?1, ?2, ?3, NULL, 1)",
        params![
            stream.to_bytes().as_slice(),
            key,
            serde_json::json!({
                "source_node_id": "mem_a",
                "relation": "relates_to",
                "target_repo_id": "repo-b",
                "target_kind": "node",
                "target_anchor": "mem_b",
                "owner_repo_id": "repo-b",
            })
            .to_string(),
        ],
    )
    .unwrap();

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.edges_written, 0, "an edge claiming a foreign owner is not written");
    assert!(!edge_exists(&conn, &key), "nothing is injected into the sibling repo");
}

/// Even with a truthful `owner_repo_id = repo_id`, an edge whose SOURCE node id collides with a
/// sibling repo's node must not materialize — it would attach a this-repo edge to the sibling's
/// node (node id is a global PK). The source guard requires the source to belong to this repo.
#[test]
fn a_projected_edge_whose_source_belongs_to_a_sibling_repo_is_skipped() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    // A node id owned by a SIBLING repo (not ours).
    insert_local_memory_in_repo(&conn, "mem_sibling", "sib", "b", "active", "repo-b");
    // Our stream projects an edge (owner = us) whose source is that sibling id.
    let key = edge_key("mem_sibling", "relates_to", "node", "mem_b");
    seed_projected_edge(&conn, stream, &key, "mem_sibling", "relates_to", "node", "mem_b", true);

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(
        outcome.edges_written, 0,
        "an edge whose source is a sibling repo's node is not materialized",
    );
    assert!(!edge_exists(&conn, &key));
}

/// The store-global drain runs at open BEFORE the connection scope is installed, so on a
/// multi-repo store the active-repo scope is unresolvable. The synced row's FTS shadow must
/// still carry `repo_id` (copied from the row the drain stamped) — otherwise
/// `memory_search`'s repo filter never matches it and the primary "searchable now" behavior
/// fails until some later scoped write repairs it.
#[test]
fn a_drained_synced_node_is_repo_stamped_in_fts_even_when_scope_is_unresolvable() {
    let conn = scoped_conn();
    // Reproduce the open-time, multi-repo condition: a second registered repo (so
    // `sole_repo_id` can't pick one) and no `connection_context` scope.
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b','repo-b',0)",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM temp.connection_context WHERE key = 'repo_id'", []).unwrap();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_x", "Invariant", "findable", "b", "active", &[]);

    drain_worker(&conn, stream, 1_000);

    let fts_repo: Option<String> = conn
        .query_row("SELECT repo_id FROM repo_memory_fts WHERE memory_id = 'mem_x'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        fts_repo.as_deref(),
        Some(REPO),
        "the drained row's FTS carries repo_id even when the connection scope is unresolvable",
    );
}

/// Peer content is only wire-shape-validated, so an older / compromised device could project a
/// node that violates a local content rule (here: an unknown `kind`). It must be QUARANTINED
/// (skipped, not persisted) without wedging the drain — the valid siblings still materialize.
#[test]
fn an_invalid_synced_node_is_quarantined_and_does_not_wedge_the_drain() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_ok", "Invariant", "ok", "b", "active", &[]);
    // An unknown kind — rejected by `validate_kind`, which the create path enforces.
    seed_projected_node(&conn, stream, "mem_bad", "NotAValidKind", "bad", "b", "active", &[]);
    // An oversized body — beyond `MAX_MEMORY_BODY_LEN`, which the create path caps.
    seed_projected_node(
        &conn,
        stream,
        "mem_big",
        "Invariant",
        "big",
        &"x".repeat(1_000_000),
        "active",
        &[],
    );

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.nodes_written, 1, "only the valid node is materialized");
    assert!(memory_by_id(&conn, "mem_ok").unwrap().is_some());
    assert!(
        memory_by_id(&conn, "mem_bad").unwrap().is_none(),
        "the unknown-kind node is quarantined, not persisted",
    );
    assert!(
        memory_by_id(&conn, "mem_big").unwrap().is_none(),
        "the oversized-body node is quarantined, not persisted",
    );
}

/// A peer edits an already-materialized synced memory to content that fails LOCAL validation.
/// The accepted value cannot be persisted, but the STALE prior synced row (still in the
/// projection, so retro-condemn never reaches it) must be removed — not left searchable
/// forever.
#[test]
fn a_projected_update_to_invalid_content_removes_the_stale_synced_row() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    // First drain: the node is valid and materializes as a searchable synced row.
    seed_projected_node(&conn, stream, "mem_x", "Invariant", "ok", "b", "active", &[]);
    drain_worker(&conn, stream, 1_000);
    assert!(memory_by_id(&conn, "mem_x").unwrap().is_some());
    assert!(fts_row_exists(&conn, "mem_x"), "the materialized synced row has an FTS row");

    // A peer's accepted edit changes the projected content to an unknown kind (invalid
    // locally).
    conn.execute(
        "UPDATE content_projected_nodes
             SET content_json = json_set(content_json, '$.kind', 'NotAValidKind')
             WHERE node_id = 'mem_x'",
        [],
    )
    .unwrap();

    let outcome = drain_worker(&conn, stream, 2_000);
    assert_eq!(outcome.nodes_removed, 1, "the stale synced mirror is removed, counted as removed");
    assert_eq!(outcome.nodes_written, 0, "the invalid value is never persisted");
    assert!(
        memory_by_id(&conn, "mem_x").unwrap().is_none(),
        "the stale prior synced row is not left searchable under an invalidated projection",
    );
    assert!(
        !fts_row_exists(&conn, "mem_x"),
        "the contentless FTS shadow is cleaned up in the same txn, not orphaned",
    );

    // Idempotent: a re-drain still sees the invalid projection but has nothing left to remove.
    let again = drain_worker(&conn, stream, 3_000);
    assert_eq!(again, DrainOutcome::default(), "no row to remove twice — a no-op re-drain");
}

/// The quarantine-removal is gated to `origin='synced'`: a peer's invalid edit must never
/// destroy a genuine local row of the same id (the user's own authored content).
#[test]
fn a_projected_update_to_invalid_content_spares_a_local_row() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    // A locally-authored row (origin='local'), and a projection that would converge it but is
    // invalid.
    insert_local_memory(&conn, "mem_local", "mine", "b", "active");
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_local', 'path', 'src/lib.rs', 'src/lib.rs', 'current', 0)",
        [REPO],
    )
    .unwrap();
    seed_projected_node(&conn, stream, "mem_local", "NotAValidKind", "peer", "b", "active", &[]);

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.nodes_removed, 0, "a local row is never removed by an invalid peer edit");
    assert!(
        memory_by_id(&conn, "mem_local").unwrap().is_some(),
        "the user's local content survives an invalid projected edit",
    );
    let binding_exists: bool = conn
        .query_row(
            "SELECT EXISTS(
                     SELECT 1 FROM repo_memory_bindings
                     WHERE repo_id = ?1 AND memory_id = 'mem_local')",
            [REPO],
            |row| row.get(0),
        )
        .unwrap();
    assert!(binding_exists, "the local memory's anchors survive with its content");
}

/// A projected node whose tag exceeds the local 64-byte cap must be quarantined here, not left
/// to error inside `write_node_children`/`replace_tags` and roll back (and wedge) the whole
/// drain.
#[test]
fn an_oversized_tag_on_a_synced_node_is_quarantined_not_fatal() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_ok", "Invariant", "ok", "b", "active", &["fine"]);
    let big_tag = "x".repeat(65);
    seed_projected_node(&conn, stream, "mem_tag", "Invariant", "t", "b", "active", &[&big_tag]);

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.nodes_written, 1, "only the valid node materializes");
    assert!(memory_by_id(&conn, "mem_ok").unwrap().is_some());
    assert!(
        memory_by_id(&conn, "mem_tag").unwrap().is_none(),
        "an oversized tag quarantines the node rather than wedging the whole drain",
    );
}

/// A projected edge whose `target_anchor` exceeds `MAX_EDGE_ANCHOR_LEN` must be quarantined at
/// the untrusted boundary — the same cap the local `add_edge` write path enforces.
#[test]
fn an_oversized_edge_anchor_on_a_synced_edge_is_quarantined_not_persisted() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
    let big_anchor = "x".repeat(memory::MAX_EDGE_ANCHOR_LEN + 1);
    let key = edge_key("mem_a", "relates_to", "node", &big_anchor);
    seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", &big_anchor, true);

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.edges_written, 0, "the over-cap edge is not persisted");
    assert!(!edge_exists(&conn, &key), "an oversized edge anchor is quarantined at the boundary");
}

/// An existing edge row owned by a SIBLING repo (a global-PK `edge_key` collision) must not be
/// stolen or rewritten into this repo by a converge — symmetric to the node guard.
#[test]
fn a_converge_never_steals_a_sibling_repos_edge_row() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    insert_local_memory(&conn, "mem_a", "a", "b", "active");
    let key = edge_key("mem_a", "relates_to", "node", "mem_b");
    // An existing edge row on this key owned by a sibling repo (the collision codex describes).
    conn.execute(
        "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, 'repo-b', 'mem_a', 'relates_to', 'repo-b', 'node', 'mem_b', 'mem_b',
                 'current', 100)",
        [&key],
    )
    .unwrap();
    // Our stream projects the same key (owner = us, different target repo).
    seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", "mem_b", true);

    let outcome = drain_worker(&conn, stream, 1_000);
    assert_eq!(outcome.edges_written, 0, "the sibling's edge is not converged");
    let (repo_id, target_repo): (String, String) = conn
        .query_row(
            "SELECT repo_id, target_repo_id FROM repo_node_edges WHERE edge_key = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (repo_id.as_str(), target_repo.as_str()),
        ("repo-b", "repo-b"),
        "the sibling repo's edge is left byte-for-byte unchanged, not stolen",
    );
}

// --- Task 7: idempotence ---

#[test]
fn re_running_the_drain_over_an_unchanged_projection_is_a_no_op() {
    let conn = scoped_conn();
    let stream = StreamId::from_bytes([0x33; 32]);
    seed_projected_node(&conn, stream, "mem_x", "Invariant", "t", "b", "active", &["a", "b"]);
    seed_projected_node(&conn, stream, "mem_y", "Invariant", "y", "b", "active", &[]);
    let key = edge_key("mem_x", "relates_to", "node", "mem_y");
    seed_projected_edge(&conn, stream, &key, "mem_x", "relates_to", "node", "mem_y", true);

    let first = drain_worker(&conn, stream, 1_000);
    assert_eq!(first.nodes_written, 2);
    assert_eq!(first.edges_written, 1);
    let updated_before = updated_at_of(&conn, "mem_x");

    let second = drain_worker(&conn, stream, 9_999);
    assert_eq!(
        second,
        DrainOutcome::default(),
        "a re-drain over an unchanged projection writes and removes nothing",
    );
    assert_eq!(
        updated_at_of(&conn, "mem_x"),
        updated_before,
        "updated_at_ms is not bumped on a no-op re-drain",
    );
}

// --- Task 8: no echo — the round-trip is proven end-to-end ---

/// A drained `origin='synced'` row must NOT be re-authored back into the signed `/3` log by the
/// next reconcile — the authoring-side `origin='local'` gate plus the projection anti-join make
/// the round-trip non-echoing. Exercised on the REAL path (real owner stream + public entry).
#[test]
fn a_drained_synced_row_is_not_re_authored_by_the_next_reconcile() {
    let conn = scoped_conn();
    create_concept(&conn, "seed"); // mints the account + establishes the owner stream
    let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
    // A peer's node: present in the accepted-/3 projection, not yet materialized locally.
    seed_projected_node(&conn, stream, "mem_peer", "Invariant", "peer", "body", "active", &[]);
    let entries_before = content_entry_count(&conn);

    let outcome = drain_synced_stream_for_repo(&conn, REPO, 5_000).unwrap();
    assert_eq!(outcome.nodes_written, 1, "the peer node materializes");
    assert_eq!(origin_of(&conn, "mem_peer"), "synced");
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_some(), "searchable as a local row now");

    // The reconcile runs: the synced row is excluded from re-authoring (origin gate), and it is
    // also already in the projection, so nothing is appended to the immutable /3 log.
    crate::memory_write::reconcile::backfill_memory_oplog(&conn, 6_000).unwrap();
    assert_eq!(
        content_entry_count(&conn),
        entries_before,
        "a synced row is never re-authored into the signed /3 log",
    );
    assert_eq!(origin_of(&conn, "mem_peer"), "synced", "and it is never flipped to local");
}

// --- Task 9: the store-global drain the open/migrate seam calls ---

/// The store-global entry (wired into the open/migrate seam) iterates every registered real
/// repo and materializes its synced content, while leaving locally-authored rows untouched
/// — proving the exact call the lifecycle open makes.
#[test]
fn the_store_global_drain_materializes_synced_content_and_spares_local_rows() {
    let conn = scoped_conn();
    // A real local memory mints the account + owner stream and projects itself origin='local'.
    let local_id = create_concept(&conn, "local seed");
    let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
    // A peer's node in the projection, not yet materialized locally.
    seed_projected_node(&conn, stream, "mem_peer", "Invariant", "peer", "body", "active", &[]);

    let outcome = drain_synced_streams_for_all_repos(&conn, 7_000).unwrap();
    assert_eq!(outcome.nodes_written, 1, "the peer node materializes for the registered repo");
    assert_eq!(origin_of(&conn, "mem_peer"), "synced");
    assert!(memory_by_id(&conn, "mem_peer").unwrap().is_some(), "readable as a local row");
    assert_eq!(origin_of(&conn, &local_id), "local", "the local row is left untouched");
}

#[test]
fn content_then_production_anchors_surface_a_synced_memory_by_path() {
    let source = scoped_conn();
    let memory_id = create_concept(&source, "portable anchor");
    source
        .execute(
            "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                     anchor_status, created_at_ms)
                 VALUES (?1, ?2, 'path', 'src/lib.rs', 'src/lib.rs', 3, 4, 'current', 1)",
            params![REPO, memory_id],
        )
        .unwrap();
    let account = rag_rat_oplog::local_account(&source, 1).unwrap();
    rag_rat_oplog::ensure_repo_incarnation(&source, REPO, 1).unwrap().unwrap();
    assert_eq!(rag_rat_oplog::table_sync_author_pending(&source, account, 2).unwrap(), 1);
    let route = rag_rat_oplog::table_sync_supported_streams(&source, account).unwrap().remove(0);

    let destination = scoped_conn();
    rag_rat_oplog::local_device(&destination, 0).unwrap();
    for entry in rag_rat_oplog::account_entries_for_sync(&source, account).unwrap() {
        rag_rat_oplog::account_ingest(&destination, &entry.signed_bytes, 0).unwrap();
    }
    rag_rat_oplog::adopt_local_account(
        &destination,
        account,
        rag_rat_oplog::read_local_account_genesis(&source).unwrap().unwrap(),
        0,
    )
    .unwrap();
    for entry in rag_rat_oplog::content_entries_for_sync(&source, account).unwrap() {
        rag_rat_oplog::content_ingest(&destination, &entry.signed_bytes, 1).unwrap();
    }
    let source_stream = rag_rat_oplog::owned_stream_v2_id(&source, REPO).unwrap().unwrap();
    let destination_stream = rag_rat_oplog::owned_stream_v2_id(&destination, REPO)
        .unwrap()
        .expect("account restore derives the repository's content stream");
    assert_eq!(destination_stream, source_stream);
    crate::drain_synced_memory(&destination).unwrap();
    assert!(memory_by_id(&destination, &memory_id).unwrap().is_some());
    assert!(
        memory::memories_for_path(&destination, "src/lib.rs", 10).unwrap().is_empty(),
        "content alone has no checkout-independent anchor",
    );

    let head = rag_rat_oplog::table_sync_chain_page_after(&source, account, &route, None, 10)
        .unwrap()
        .remove(0);
    for entry in rag_rat_oplog::table_sync_chain_entries(
        &source,
        account,
        &route,
        head.device_fingerprint,
        rag_rat_oplog::TableSyncEntryStart::Beginning,
        10,
    )
    .unwrap()
    {
        rag_rat_oplog::table_sync_ingest(
            &destination,
            account,
            &route,
            &rag_rat_oplog::TableSyncReceived {
                expected_device: head.device_fingerprint,
                signed_bytes: &entry.signed_bytes,
                advertised_floor: None,
                advertised_tip: None,
            },
            3,
            &Default::default(),
        )
        .unwrap();
    }
    let surfaced = memory::memories_for_path(&destination, "src/lib.rs", 10).unwrap();
    assert_eq!(surfaced.len(), 1);
    assert_eq!(surfaced[0].memory_id, memory_id);
}

/// The point of the whole train (#1180): a memory created WITH a binding reaches a peer over
/// `/3` alone and drive-by surfaces there, with no `/5` anchors leg. (Both stores adopt the
/// same account here, as its sibling does — what this isolates is the transport, not the
/// account boundary.)
///
/// Its sibling above pins the complementary case: a binding written directly to the table,
/// bypassing the authoring seam, publishes nothing and so surfaces nothing until `/5` carries
/// it. The difference between the two tests is entirely whether the binding was AUTHORED.
#[test]
fn an_authored_anchor_set_surfaces_a_synced_memory_over_content_alone() {
    let source = scoped_conn();
    let memory_id = crate::memory_write::create_memory(&source, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "portable anchor".to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget {
            path: Some("src/lib.rs".to_string()),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap()
    .memory
    .memory_id;
    let account = rag_rat_oplog::local_account(&source, 1).unwrap();

    let destination = scoped_conn();
    rag_rat_oplog::local_device(&destination, 0).unwrap();
    for entry in rag_rat_oplog::account_entries_for_sync(&source, account).unwrap() {
        rag_rat_oplog::account_ingest(&destination, &entry.signed_bytes, 0).unwrap();
    }
    rag_rat_oplog::adopt_local_account(
        &destination,
        account,
        rag_rat_oplog::read_local_account_genesis(&source).unwrap().unwrap(),
        0,
    )
    .unwrap();
    // Content ONLY — the `/5` anchors leg is deliberately never run.
    for entry in rag_rat_oplog::content_entries_for_sync(&source, account).unwrap() {
        rag_rat_oplog::content_ingest(&destination, &entry.signed_bytes, 1).unwrap();
    }
    crate::drain_synced_memory(&destination).unwrap();

    let surfaced = memory::memories_for_path(&destination, "src/lib.rs", 10).unwrap();
    assert_eq!(surfaced.len(), 1, "the authored anchor set seeded a usable binding");
    assert_eq!(surfaced[0].memory_id, memory_id);
}

/// The validate pass must never author. Relocation re-keys `binding_id` per checkout, so a
/// signed op per mechanical drift would put every device in a rebind war over one memory.
///
/// What actually guarantees this is the crate graph — `rag-rat-query`, where the loop lives,
/// cannot reach core's authoring path — so treat this as documentation of the intent rather
/// than a trap that would catch someone adding authoring on the core side of the call.
#[test]
fn a_relocation_pass_authors_no_content_op() {
    let conn = scoped_conn();
    let memory_id = crate::memory_write::create_memory(&conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "relocating".to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget {
            path: Some("src/lib.rs".to_string()),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap()
    .memory
    .memory_id;
    let account = rag_rat_oplog::local_account(&conn, 1).unwrap();
    let before = rag_rat_oplog::content_entries_for_sync(&conn, account).unwrap().len();

    memory::validate_memories(&conn, None).unwrap();

    assert_eq!(
        rag_rat_oplog::content_entries_for_sync(&conn, account).unwrap().len(),
        before,
        "a validate/relocate pass authored a content entry",
    );
    assert!(memory_by_id(&conn, &memory_id).unwrap().is_some());
}

/// Advance a stream's projection epoch WITHOUT rewriting the projection — the way to simulate
/// "the projection changed" while keeping directly-seeded poison rows in place (a real
/// reproject would rebuild them away). Mutates the internal `oplog_meta` epoch key by hand
/// on purpose.
fn bump_projection_epoch(conn: &Connection, stream: StreamId) {
    conn.execute(
        "UPDATE oplog_meta SET value = CAST(value AS INTEGER) + 1 WHERE key = \
         'content:proj-epoch:' || hex(?1)",
        params![stream.to_bytes().as_slice()],
    )
    .unwrap();
}

/// A concurrent `rag-rat rm` that commits its removal tombstone after the store-global drain
/// has snapshotted the repo list must NOT re-materialize the removed repo's synced content.
/// The in-transaction tombstone recheck skips it — without the guard, the projected peer
/// node would be resurrected into `repo_memories` after `rm` reported it gone.
#[test]
fn a_drain_skips_a_repo_with_a_removal_tombstone() {
    let conn = scoped_conn();
    create_concept(&conn, "seed");
    let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
    // A peer node in the projection that a drain WOULD materialize.
    seed_projected_node(&conn, stream, "mem_peer", "Invariant", "peer", "b", "active", &[]);
    // `rm` tombstones the repo (its purge cleared the rows; the drain must not put them back).
    rag_rat_db::schema::mark_repo_removed(&conn, REPO, 1).unwrap();

    let outcome = drain_synced_stream_for_repo(&conn, REPO, 2_000).unwrap();
    assert_eq!(outcome, DrainOutcome::default(), "a tombstoned repo's drain is a no-op");
    assert!(
        memory_by_id(&conn, "mem_peer").unwrap().is_none(),
        "the removed repo's synced content is not resurrected by the drain",
    );
}

/// The drain gate must SKIP its scan when the projection is unchanged since the last drain —
/// and the skip must be load-bearing, not luck. Poison the projection with a node the scan
/// WOULD materialize but WITHOUT advancing the epoch (a direct insert bypasses the
/// reproject that bumps it); a gated re-drain must not pick it up. Then advance the epoch
/// and confirm the SAME drain now does — proving the gate, not an empty projection, is what
/// suppressed the first pass.
#[test]
fn the_drain_gate_skips_an_unchanged_projection_then_runs_when_the_epoch_advances() {
    let conn = scoped_conn();
    // Mints the account + owner stream and reprojects the local seed (epoch -> 1).
    create_concept(&conn, "seed");
    let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();

    // First drain records the watermark at the current epoch → nothing owed afterwards.
    drain_synced_streams_for_all_repos(&conn, 1_000).unwrap();
    assert!(
        !rag_rat_oplog::content_drain_needed(&conn, stream).unwrap(),
        "nothing is owed immediately after a drain",
    );

    // Poison: a projected node the scan WOULD materialize, inserted directly so the epoch does
    // NOT move (a real peer op would reproject and bump it).
    seed_projected_node(&conn, stream, "mem_poison", "Invariant", "p", "b", "active", &[]);

    // Gated re-drain: the epoch equals the watermark and nothing is pending, so the gate skips
    // the scan — the poison stays unseen.
    let skipped = drain_synced_streams_for_all_repos(&conn, 2_000).unwrap();
    assert_eq!(skipped, DrainOutcome::default(), "the gate skipped the scan");
    assert!(
        memory_by_id(&conn, "mem_poison").unwrap().is_none(),
        "a gated skip does not materialize a projected row the scan would have",
    );

    // Advance the epoch as a real reproject would; the SAME drain now runs and materializes it.
    bump_projection_epoch(&conn, stream);
    assert!(
        rag_rat_oplog::content_drain_needed(&conn, stream).unwrap(),
        "an epoch past the watermark is owed",
    );
    let ran = drain_synced_streams_for_all_repos(&conn, 3_000).unwrap();
    assert_eq!(ran.nodes_written, 1, "the drain runs once the epoch advances");
    assert!(
        memory_by_id(&conn, "mem_poison").unwrap().is_some(),
        "and materializes the row the earlier gated pass skipped",
    );
}

/// The public entry no-ops on an unstable repo id (legacy / local-only) — such an id can never
/// root an owner stream, so there is nothing to drain and it must not touch the tables.
#[test]
fn the_public_entry_no_ops_on_an_unstable_repo_id() {
    let conn = scoped_conn();
    let legacy = rag_rat_base::repo_identity::LEGACY_REPO_ID;
    assert_eq!(
        drain_synced_stream_for_repo(&conn, legacy, 1_000).unwrap(),
        DrainOutcome::default(),
    );
    let local = format!("{}deadbeef", rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX);
    assert_eq!(
        drain_synced_stream_for_repo(&conn, &local, 1_000).unwrap(),
        DrainOutcome::default(),
    );
}
