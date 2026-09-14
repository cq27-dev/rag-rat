use rag_rat_query::memory::{EdgeRelation, EdgeTarget, RepoMemoryBindTarget, RepoMemoryCreate};
use rusqlite::Connection;

use super::{create_memory, rebind_memory};
use crate::memory_write::add_edge;

const REPO: &str = "repo-a";

/// A DB with the memory schema, one registered repo, and the connection scoped to it — the
/// minimal setup `memory_repo_scope` needs to resolve an active repo. Mirrors
/// `authoring::tests::scoped_conn` (each module's test scaffolding is self-contained).
fn scoped_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    scope_to_repo(&conn);
    conn
}

fn bound_create(conn: &Connection, path: &str) -> String {
    create_memory(conn, RepoMemoryCreate {
        kind: "Invariant".to_string(),
        title: "t".to_string(),
        body: "b".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget {
            path: Some(path.to_string()),
            ..RepoMemoryBindTarget::default()
        },
    })
    .unwrap()
    .memory
    .memory_id
}

/// An indexed file, so a `path` binding over it RESOLVES to a source hash. Without one
/// `resolve_path_binding` reads no `files` row, stamps `source_text_hash` NULL, and every
/// source-hash assertion below would pass against an op that was never built.
fn indexed_file(conn: &Connection, path: &str, sha256: &str) {
    conn.execute(
        "INSERT INTO files(repo_id, path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES (?1, ?2, 'rust', 'code', ?3, 1, 1)",
        rusqlite::params![REPO, path, sha256],
    )
    .unwrap();
}

/// The 64-char lowercase hex the indexer writes — the shape `source_text_hash` carries.
fn sha_of(seed: &str) -> String {
    rag_rat_base::hash::hex_sha256(seed.as_bytes())
}

/// The anchor set the PROJECTION holds for `memory_id` — the authored op as a peer would see
/// it, after acceptance and the fold, rather than as raw entry bytes. `None` means no
/// `node_anchors` op was authored for it at all.
fn projected_anchors(conn: &Connection, memory_id: &str) -> Option<Vec<String>> {
    let stream = rag_rat_oplog::owned_stream_v2_id(conn, REPO).unwrap().unwrap();
    rag_rat_oplog::list_projected_content_nodes(conn, stream)
        .unwrap()
        .into_iter()
        .find(|node| node.node_id == memory_id)
        .expect("the memory projects as a node")
        .anchors
        .map(|anchors| anchors.into_iter().map(|anchor| anchor.binding_id).collect())
}

/// The source hash the PROJECTION holds for `memory_id` — the twin of [`projected_anchors`].
/// `None` means no `node_source_hash` op was authored for it at all.
fn projected_source_hash(conn: &Connection, memory_id: &str) -> Option<String> {
    let stream = rag_rat_oplog::owned_stream_v2_id(conn, REPO).unwrap().unwrap();
    rag_rat_oplog::list_projected_content_nodes(conn, stream)
        .unwrap()
        .into_iter()
        .find(|node| node.node_id == memory_id)
        .expect("the memory projects as a node")
        .source_text_hash
}

/// A bound create publishes its anchors beside the node, so a peer can seed them.
#[test]
fn a_bound_create_authors_its_anchor_set() {
    let conn = scoped_conn();
    let memory_id = bound_create(&conn, "src/lib.rs");

    let anchors = projected_anchors(&conn, &memory_id).expect("the create published a set");
    assert!(anchors.contains(&"src/lib.rs".to_string()), "the path binding is published");
}

/// An unanchored memory authors NO anchor op. The empty set is a different fact from "nobody
/// published", but neither seeds anything, so spending a signed entry to say it would buy a
/// peer nothing it can act on.
#[test]
fn an_unanchored_create_authors_no_anchor_op() {
    let conn = scoped_conn();
    let memory_id = create_memory(&conn, RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: "t".to_string(),
        body: "b".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget::default(),
    })
    .unwrap()
    .memory
    .memory_id;

    assert_eq!(projected_anchors(&conn, &memory_id), None);
}

/// The backfill leg must publish anchors too. A memory whose bindings predate the anchor op —
/// every memory in an existing store, and everything `sync publish --seed` reconciles onto a
/// public stream — is authored by the reconcile anti-join, not by `create_memory`. If that leg
/// skipped the snapshot, those memories would replicate with no anchors permanently, since the
/// anti-join never revisits a node once it exists.
#[test]
fn the_reconcile_backfill_publishes_anchors_for_a_memory_that_predates_the_op() {
    let conn = scoped_conn();
    // A memory + binding written the way a pre-anchor store holds them: rows only, no op.
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
                 memory_version, repo_id, origin)
             VALUES ('mem_old', 'Invariant', 't', 'b', 'high', 'active', 1, 1, 'agent', 'v1', ?1,
                 'local')",
        [REPO],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_old', 'path', 'src/old.rs', 'src/old.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();

    // Any authored write drives the backfill, which reconciles the un-authored node.
    bound_create(&conn, "src/lib.rs");

    let anchors =
        projected_anchors(&conn, "mem_old").expect("the backfill published the old anchors");
    assert!(anchors.contains(&"src/old.rs".to_string()));
}

/// The half the node anti-join cannot reach: a memory whose `NodeCreate` was ALREADY authored
/// before the anchor op existed. It is not missing from the projection, so the node backfill
/// never revisits it — without a second sweep its bindings would never reach a peer, which is
/// the state every store that was already syncing upgrades into.
#[test]
fn the_backfill_publishes_anchors_for_an_already_authored_memory() {
    let conn = scoped_conn();
    // The pre-anchor state, built honestly: author the node with NO snapshot (an unanchored
    // create emits none), then give it a binding the way a pre-anchor store holds one — rows
    // only. Nulling the projection column instead would prove nothing, because the refold
    // restores it from the retained op.
    let memory_id = create_memory(&conn, RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: "t".to_string(),
        body: "b".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget::default(),
    })
    .unwrap()
    .memory
    .memory_id;
    assert_eq!(projected_anchors(&conn, &memory_id), None, "authored, with no snapshot");
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, ?2, 'path', 'src/old.rs', 'src/old.rs', 'current', 1)",
        rusqlite::params![REPO, memory_id],
    )
    .unwrap();

    // Any authored write drives the reconcile, which now sweeps these too.
    bound_create(&conn, "src/other.rs");

    let anchors =
        projected_anchors(&conn, &memory_id).expect("the sweep published the missing snapshot");
    assert!(anchors.contains(&"src/old.rs".to_string()));
}

/// A rebind is an explicit choice of a new anchor and now mints a signed op for it — the
/// full-set snapshot names where the memory points NOW, not how it got there.
#[test]
fn a_rebind_authors_the_new_anchor_set() {
    let conn = scoped_conn();
    let memory_id = bound_create(&conn, "src/lib.rs");
    rebind_memory(&conn, &memory_id, RepoMemoryBindTarget {
        path: Some("src/other.rs".to_string()),
        ..RepoMemoryBindTarget::default()
    })
    .unwrap();

    let anchors = projected_anchors(&conn, &memory_id).expect("the rebind published a set");
    assert!(anchors.contains(&"src/other.rs".to_string()), "the new anchor is published");
    assert!(
        !anchors.contains(&"src/lib.rs".to_string()),
        "a full-set snapshot retires the anchor the memory no longer points at",
    );
}

/// A bound create publishes the hash of the text it anchored to, beside the anchors it
/// describes. Without it a receiver has nothing to compare its own checkout against, and the
/// hash-relocation fallback — which reads exactly this column — can never fire for a synced
/// memory.
#[test]
fn a_bound_create_publishes_the_source_hash_it_anchored_to() {
    let conn = scoped_conn();
    let sha = sha_of("lib");
    indexed_file(&conn, "src/lib.rs", &sha);

    let memory_id = bound_create(&conn, "src/lib.rs");

    assert_eq!(projected_source_hash(&conn, &memory_id), Some(sha));
}

/// A memory anchored to something with no text behind it publishes an EMPTY hash beside its
/// anchors, never the set alone: a lone op can win one register against a concurrent writer's
/// pair and lose the other. A receiver reads the empty hash as no evidence of drift.
#[test]
fn a_create_over_an_unindexed_path_publishes_an_empty_source_hash() {
    let conn = scoped_conn();
    let memory_id = bound_create(&conn, "src/lib.rs");

    assert_eq!(projected_source_hash(&conn, &memory_id), Some(String::new()));
}

/// A rebind re-stamps `source_text_hash` in the same transaction, so the published hash has to
/// move with the anchors or a peer keeps comparing against the pre-rebind text.
#[test]
fn a_rebind_republishes_the_hash_of_the_text_it_now_points_at() {
    let conn = scoped_conn();
    let before = sha_of("lib");
    let after = sha_of("other");
    indexed_file(&conn, "src/lib.rs", &before);
    indexed_file(&conn, "src/other.rs", &after);
    let memory_id = bound_create(&conn, "src/lib.rs");
    assert_eq!(projected_source_hash(&conn, &memory_id), Some(before));

    rebind_memory(&conn, &memory_id, RepoMemoryBindTarget {
        path: Some("src/other.rs".to_string()),
        ..RepoMemoryBindTarget::default()
    })
    .unwrap();

    assert_eq!(projected_source_hash(&conn, &memory_id), Some(after));
}

/// A rebind onto a target with no text behind it — a commit, a tracker ref, a directory — nulls
/// the local column and publishes an EMPTY hash. The register has no other retraction, and a
/// receiver applies the hash on its own change, so staying silent would leave the pre-rebind
/// value standing beside the new anchors.
#[test]
fn a_rebind_onto_a_hashless_target_retracts_the_published_hash() {
    let conn = scoped_conn();
    let before = sha_of("lib");
    indexed_file(&conn, "src/lib.rs", &before);
    let memory_id = bound_create(&conn, "src/lib.rs");

    rebind_memory(&conn, &memory_id, RepoMemoryBindTarget {
        commit_hash: Some("f".repeat(40)),
        ..RepoMemoryBindTarget::default()
    })
    .unwrap();

    let local: Option<String> = conn
        .query_row(
            "SELECT source_text_hash FROM repo_memories WHERE id = ?1",
            [&memory_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(local, None, "the rebind nulled the local hash");
    assert_eq!(
        projected_source_hash(&conn, &memory_id),
        Some(String::new()),
        "and retracted the pre-rebind value from the register",
    );
}

/// A memory's hash and anchor set are separate entries on one chain, and a peer accepts a chain
/// in order. The hash goes first, so a pull that stops between them never pairs the new
/// bindings with the previous target's hash; a rebind onto a hashless target leads with the
/// empty retraction.
#[test]
fn the_hash_is_published_ahead_of_the_anchor_set() {
    use rag_rat_oplog::MemoryOp;
    let conn = scoped_conn();
    indexed_file(&conn, "src/lib.rs", &sha_of("lib"));
    let hashed = bound_create(&conn, "src/lib.rs");
    let unhashed = bound_create(&conn, "src/other.rs");
    let publication = |memory_id: &str| {
        crate::memory_write::authoring::anchor_publication_ops(&conn, memory_id).unwrap()
    };

    let ops = publication(&hashed);
    assert!(
        matches!(ops.as_slice(), [
            MemoryOp::NodeSourceHash { .. },
            MemoryOp::NodeAnchorScopes { .. },
            MemoryOp::NodeAnchors { .. }
        ]),
        "{ops:?}"
    );
    let ops = publication(&unhashed);
    assert!(
        matches!(
            ops.as_slice(),
            [
                MemoryOp::NodeSourceHash { source_text_hash, .. },
                MemoryOp::NodeAnchorScopes { scopes, .. },
                MemoryOp::NodeAnchors { .. }
            ] if source_text_hash.is_empty() && scopes.is_empty()
        ),
        "{ops:?}"
    );
}

/// The backfill leg must publish the hash too, for the same reason it must publish anchors: a
/// memory whose bindings predate the op is authored by the reconcile anti-join, not by
/// `create_memory`, and the anti-join never revisits a node once it exists.
#[test]
fn the_reconcile_backfill_publishes_the_source_hash_for_a_memory_that_predates_the_op() {
    let conn = scoped_conn();
    let sha = sha_of("old");
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
                 source_text_hash, memory_version, repo_id, origin)
             VALUES ('mem_old', 'Invariant', 't', 'b', 'high', 'active', 1, 1, 'agent', ?2, 'v1',
                 ?1, 'local')",
        rusqlite::params![REPO, sha],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_old', 'path', 'src/old.rs', 'src/old.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();

    // Any authored write drives the backfill, which reconciles the un-authored node.
    bound_create(&conn, "src/lib.rs");

    assert_eq!(projected_source_hash(&conn, "mem_old"), Some(sha));
}

/// The half the node anti-join cannot reach: a memory whose `NodeCreate` was ALREADY authored
/// before the op existed. The anchor sweep is the only leg that reaches it, and the hash has to
/// ride that batch — a receiver applies a hash only where it seeds the anchors it describes, so
/// a sweep that published anchors alone would leave this whole corpus unmarked on every peer.
#[test]
fn the_anchor_sweep_publishes_the_source_hash_for_an_already_authored_memory() {
    let conn = scoped_conn();
    let sha = sha_of("old");
    // The pre-anchor state, built honestly: author the node with NO snapshot (an unanchored
    // create emits none), then give it a binding and a hash the way a pre-anchor store holds
    // them — rows only.
    let memory_id = create_memory(&conn, RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: "t".to_string(),
        body: "b".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget::default(),
    })
    .unwrap()
    .memory
    .memory_id;
    assert_eq!(projected_source_hash(&conn, &memory_id), None, "authored, with no hash");
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, ?2, 'path', 'src/old.rs', 'src/old.rs', 'current', 1)",
        rusqlite::params![REPO, memory_id],
    )
    .unwrap();
    conn.execute(
        "UPDATE repo_memories SET source_text_hash = ?2 WHERE id = ?1",
        rusqlite::params![memory_id, sha],
    )
    .unwrap();

    // Any authored write drives the reconcile, which now sweeps these too.
    bound_create(&conn, "src/other.rs");

    assert_eq!(projected_source_hash(&conn, &memory_id), Some(sha));
}

fn scope_to_repo(conn: &Connection) {
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    rag_rat_db::schema::apply(conn, &crate::index::migration_hooks()).unwrap();
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
}

/// A rebind whose backfill has work of its own must still commit at `synchronous = FULL`
/// (#560). The backfill self-transacts under its own durability guard, whose drop restores
/// NORMAL, so a rebind guard raised before the backfill is downgraded before the rebind's
/// commit. A temp trigger on the rebind's own UPDATE reads the level from inside the authored
/// transaction. File-backed because an in-memory database does not report `synchronous`.
#[test]
fn a_rebind_commits_durably_after_a_backfill_that_authored() {
    let dir = rag_rat_base::test_scratch::ScratchDir::new("rebind-durability");
    let storage = rag_rat_db::storage::IndexConnection::open(&dir.join("index.db")).unwrap();
    let conn = storage.connection();
    scope_to_repo(conn);
    // A ghost: a row no op was ever authored for, so the backfill has authorable work (and, on
    // this fresh store, mints the local account first).
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_at_ms, updated_at_ms, source,
                 memory_version, repo_id, origin)
             VALUES ('mem_ghost', 'Concept', 't', 'b', 'high', 'active', 1, 1, 'agent', 'v1', ?1,
                 'local')",
        [REPO],
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE sync_probe(level INTEGER);
             CREATE TEMP TRIGGER sync_probe_on_rebind
             AFTER UPDATE OF source_text_hash ON main.repo_memories
             BEGIN
                 INSERT INTO sync_probe SELECT synchronous FROM pragma_synchronous;
             END;",
    )
    .unwrap();

    rebind_memory(conn, "mem_ghost", RepoMemoryBindTarget {
        commit_hash: Some("f".repeat(40)),
        ..Default::default()
    })
    .unwrap();

    let level: i64 = conn
        .query_row("SELECT level FROM sync_probe ORDER BY rowid DESC LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(level, 2, "the rebind's authored commit must run at synchronous=FULL (=2)");
}

/// Create an unanchored `Concept` (needs no code binding) through the LIVE `create_memory`.
fn create_concept(conn: &Connection, title: &str) -> anyhow::Result<String> {
    Ok(create_memory(conn, RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: title.to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: RepoMemoryBindTarget::default(),
    })?
    .memory
    .memory_id)
}

/// #767 review: a connection that resolved its active repo scope BEFORE `rag-rat rm` ran must
/// fail closed at write time — the removal tombstone is revalidated inside the write
/// transaction, so a post-purge `create_memory` cannot stamp the removed `repo_id` onto a fresh
/// row (and its op-log entry) after `rm` reported success.
#[test]
fn create_memory_refuses_a_tombstoned_repo_until_it_is_cleared() {
    let conn = scoped_conn();
    create_concept(&conn, "before removal").unwrap();

    rag_rat_db::schema::mark_repo_removed(&conn, REPO, 1).unwrap();
    let err = create_concept(&conn, "after removal")
        .expect_err("a tombstoned repo must refuse a memory create");
    assert!(
        err.to_string().contains("rag-rat rm"),
        "the refusal must name the removal remedy, got: {err}"
    );

    rag_rat_db::schema::clear_repo_removed(&conn, REPO).unwrap();
    create_concept(&conn, "after re-add").unwrap();
}

/// The same gate covers the edge INSERT path: `add_edge` stamps the source node's owner
/// `repo_id` onto the new row, so it revalidates the tombstone in-transaction too.
#[test]
fn add_edge_refuses_a_tombstoned_repo() {
    let conn = scoped_conn();
    let source = create_concept(&conn, "source").unwrap();
    let target = create_concept(&conn, "target").unwrap();

    rag_rat_db::schema::mark_repo_removed(&conn, REPO, 1).unwrap();
    let err = add_edge(&conn, &source, EdgeRelation::RelatesTo, &EdgeTarget::Node {
        repo_id: None,
        node_id: target,
    })
    .expect_err("a tombstoned repo must refuse an edge add");
    assert!(
        err.to_string().contains("rag-rat rm"),
        "the refusal must name the removal remedy, got: {err}"
    );
}

/// A NON-insert mutation is unaffected by the gate: `rebind_memory` writes no new repo-stamped
/// rows (and post-purge it has no row to find anyway), so it must not trip the tombstone check
/// on a still-populated store.
#[test]
fn rebind_memory_is_not_blocked_by_the_tombstone_gate() {
    let conn = scoped_conn();
    let id = create_concept(&conn, "rebind me").unwrap();

    rag_rat_db::schema::mark_repo_removed(&conn, REPO, 1).unwrap();
    rebind_memory(&conn, &id, RepoMemoryBindTarget {
        path: Some("src/lib.rs".to_string()),
        ..Default::default()
    })
    .unwrap();
}
