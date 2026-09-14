use std::collections::BTreeSet;

use rag_rat_oplog::NodeContent;
use rag_rat_query::memory::{EdgeRelation, NodeEdge};

use super::super::grants::{
    catch_up_enrolled_device_keys, enable_public_authoring, enable_sealed_authoring,
};
use super::super::ownership::{
    STREAM_ACCESS_MODE_META_KEY, STREAM_SEAL_POLICY_META_KEY, owner_stream_access_mode,
};
use super::super::reconcile::{
    ANCHOR_BACKFILL_PER_PASS, MemoryRow, backfill_memory_oplog, edge_add_op,
    node_content_of_memory, node_content_of_row, node_is_authorable, node_ops,
    read_anchor_backfill_ids, read_reconcile_work, read_unauthored_memory_rows,
};
use super::*;

const REPO: &str = "repo-a";

/// A conn with the local account minted and the repo's owner stream published, so a test can
/// plant projected rows against a REAL stream id — the `else { return }` shape silently skips
/// and proves nothing.
fn conn_with_stream() -> (Connection, rag_rat_oplog::StreamId) {
    let conn = scoped_conn();
    rag_rat_oplog::local_account(&conn, 1_000).unwrap();
    let stream =
        crate::memory_write::create_memory(&conn, rag_rat_query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: "seed".to_string(),
            body: "b".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
        })
        .map(|_| rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap())
        .unwrap();
    (conn, stream)
}

/// The sweep must not select a memory whose snapshot is legitimately absent, or it re-examines
/// it on every authored write forever. Asserted on the QUERY: "no snapshot appeared" is true
/// whether or not the memory was selected, so it proves nothing.
#[test]
fn the_anchor_sweep_skips_a_memory_with_no_bindings() {
    let (conn, stream) = conn_with_stream();
    insert_memory(&conn, "mem_bare", "active", 1);
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_bare', '{}', 'active')",
        params![stream.to_bytes().as_slice()],
    )
    .unwrap();

    let swept = read_anchor_backfill_ids(&conn, REPO, stream).unwrap();
    assert!(!swept.contains(&"mem_bare".to_string()), "swept a memory with no bindings");
}

/// The `origin = 'local'` gate the node anti-join also carries: a SYNCED row is a peer's to
/// publish, and re-authoring one here would forge authorship of content the account may have
/// revoked.
#[test]
fn the_anchor_sweep_never_selects_a_synced_memory() {
    let (conn, stream) = conn_with_stream();
    insert_memory(&conn, "mem_peer", "active", 1);
    conn.execute("UPDATE repo_memories SET origin = 'synced' WHERE id = 'mem_peer'", []).unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_peer', 'path', 'src/lib.rs', 'src/lib.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_peer', '{}', 'active')",
        params![stream.to_bytes().as_slice()],
    )
    .unwrap();

    let swept = read_anchor_backfill_ids(&conn, REPO, stream).unwrap();
    assert!(!swept.contains(&"mem_peer".to_string()), "a synced memory is the peer's to publish");
}

/// The #680 property, for the anchor leg. A snapshot that will never fit a signed entry must
/// be quarantined rather than left selectable: it never folds, so `anchors_json` stays NULL and
/// the sweep would re-select it on every pass, reporting work forever and spinning the
/// reconcile's slow path — which `has_authorable_work` is documented never to do.
#[test]
fn an_unpublishable_anchor_set_is_quarantined_not_reported_as_work() {
    let (conn, stream) = conn_with_stream();
    insert_memory(&conn, "mem_fat", "active", 1);
    // Past MAX_ANCHORS_PER_OP, so the op cannot be encoded at any size.
    for index in 0..=rag_rat_oplog::MAX_ANCHORS_PER_OP {
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, anchor_status,
                     created_at_ms)
                 VALUES (?1, 'mem_fat', 'path', ?2, 'src/lib.rs', 'current', 1)",
            params![REPO, format!("src/lib.rs:{index:04}")],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_fat', '{}', 'active')",
        params![stream.to_bytes().as_slice()],
    )
    .unwrap();

    let work = read_reconcile_work(&conn, REPO, stream, StreamSealPolicy::Plaintext).unwrap();
    assert!(
        work.quarantined_anchor_ids.contains(&"mem_fat".to_string()),
        "an unpublishable set must be quarantined",
    );
    assert!(work.anchor_backfill_ops.is_empty(), "and must not reach the batch",);
    assert!(
        !work.has_authorable_work(),
        "reporting it as work is what spins the slow path forever",
    );
}

/// A quarantined memory must not block the ones behind it. It never leaves the match set by
/// design, and it sorts oldest-first — an over-cap set can only be a legacy row — so if the
/// scan window doubled as the publish budget, enough of them at the head would stall the
/// backfill for the rest of the corpus.
#[test]
fn a_quarantined_memory_does_not_block_a_publishable_one_behind_it() {
    let (conn, stream) = conn_with_stream();
    // Oldest, and unpublishable: past the per-op anchor cap.
    insert_memory(&conn, "mem_fat", "active", 1);
    for index in 0..=rag_rat_oplog::MAX_ANCHORS_PER_OP {
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, anchor_status,
                     created_at_ms)
                 VALUES (?1, 'mem_fat', 'path', ?2, 'src/lib.rs', 'current', 1)",
            params![REPO, format!("src/lib.rs:{index:04}")],
        )
        .unwrap();
    }
    // Newer, and perfectly publishable — with a stamped hash, so the sweep emits BOTH of the
    // ops a swept memory owes.
    insert_memory(&conn, "mem_ok", "active", 2);
    set_source_hash(&conn, "mem_ok");
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_ok', 'path', 'src/ok.rs', 'src/ok.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();
    for id in ["mem_fat", "mem_ok"] {
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
                 VALUES (?1, ?2, '{}', 'active')",
            params![stream.to_bytes().as_slice(), id],
        )
        .unwrap();
    }

    let work = read_reconcile_work(&conn, REPO, stream, StreamSealPolicy::Plaintext).unwrap();
    assert_eq!(work.quarantined_anchor_ids, vec!["mem_fat".to_string()]);
    assert_eq!(
        work.anchor_backfill_ops.len(),
        3,
        "the publishable one is still authored: anchors, the hash describing them, scopes",
    );
    assert!(work.has_authorable_work(), "progress is available despite the quarantined head");
}

/// A memory whose projected set predates the scope op — a symbol anchor with no `scope_hash`
/// — is swept again once its binding can supply one, so an upgraded author republishes its
/// unchanged set with scopes beside it. A set that already carries the scope, or a binding
/// that no longer resolves one, is left alone, so the sweep converges; a set another account
/// authored, or one the local rows no longer match by target, is never re-signed as this
/// device's own.
#[test]
fn the_anchor_sweep_republishes_a_projected_set_whose_symbol_anchor_lacks_a_scope() {
    let (conn, stream) = conn_with_stream();
    let own = rag_rat_oplog::read_local_account(&conn).unwrap().unwrap().to_bytes();
    conn.execute(
        "INSERT INTO files(repo_id, path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES (?1, 'src/lib.rs', 'rust', 'code', 'sha', 1, 1)",
        [REPO],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('src/lib.rs::Twin')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, scope_path, kind,
                    start_byte, end_byte, start_line, end_line)
             VALUES (?1, 'rust', 'Twin', (SELECT id FROM name_strings WHERE value = \
         'src/lib.rs::Twin'),
                     'Twin as Beta', 'impl', 0, 1, 1, 1)",
        [file_id],
    )
    .unwrap();
    let symbol_id = conn.last_insert_rowid();
    let seed = |id: &str, symbol_id: Option<i64>, scope_hash: Option<&str>| {
        insert_memory(&conn, id, "active", 1);
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, symbol_id, symbol_kind,
                     anchor_status, created_at_ms)
                 VALUES (?1, ?2, 'symbol', 'src/lib.rs::Twin', ?3, 'impl', 'current', 1)",
            params![REPO, id, symbol_id],
        )
        .unwrap();
        let anchors = serde_json::json!([{
            "binding_kind": "symbol", "binding_id": "src/lib.rs::Twin",
            "path": "src/lib.rs", "start_line": 1, "end_line": 1,
            "commit_hash": null, "tracker": null, "project": null, "item_key": null,
            "created_at_ms": 1, "symbol_kind": "impl", "signature_hash": null,
            "moniker_tool": null, "moniker_tool_version": null, "scope_hash": scope_hash,
        }]);
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status, \
             anchors_json, anchors_author)
                 VALUES (?1, ?2, '{}', 'active', ?3, ?4)",
            params![stream.to_bytes().as_slice(), id, anchors.to_string(), own.as_slice()],
        )
        .unwrap();
    };
    seed("mem_unscoped", Some(symbol_id), None);
    seed("mem_scoped", Some(symbol_id), Some("already"));
    seed("mem_dead", None, None);
    // A set another account authored on this memory — a contributor's rebind — is that
    // account's to publish: re-signing it here would outlive the contributor's revocation.
    seed("mem_foreign", Some(symbol_id), None);
    conn.execute(
        "UPDATE content_projected_nodes SET anchors_author = ?1 WHERE node_id = 'mem_foreign'",
        [[9u8; 32].as_slice()],
    )
    .unwrap();
    // A local row that no longer matches the projected set by target — a sibling device's
    // newer rebind the rows have yet to catch up with — must not be republished over it.
    seed("mem_behind", Some(symbol_id), None);
    conn.execute(
        "UPDATE repo_memory_bindings SET signature_hash = 'newer' WHERE memory_id = 'mem_behind'",
        [],
    )
    .unwrap();
    // The same, when the sibling's rebind was between twins the target columns cannot tell
    // apart: only the rebind stamp says the local row is not the published one.
    seed("mem_sibling", Some(symbol_id), None);
    conn.execute(
        "UPDATE repo_memory_bindings SET created_at_ms = 2 WHERE memory_id = 'mem_sibling'",
        [],
    )
    .unwrap();

    let work = read_reconcile_work(&conn, REPO, stream, StreamSealPolicy::Plaintext).unwrap();
    let swept: BTreeSet<String> = work
        .anchor_backfill_ops
        .iter()
        .map(|op| match op {
            MemoryOp::NodeAnchors { node_id, .. }
            | MemoryOp::NodeSourceHash { node_id, .. }
            | MemoryOp::NodeAnchorScopes { node_id, .. } => node_id.as_str().to_string(),
            other => panic!("unexpected sweep op {other:?}"),
        })
        .collect();
    assert_eq!(swept, BTreeSet::from(["mem_unscoped".to_string()]));
    let scopes = work.anchor_backfill_ops.iter().find_map(|op| match op {
        MemoryOp::NodeAnchorScopes { scopes, .. } => Some(scopes.clone()),
        _ => None,
    });
    assert_eq!(
        scopes.map(|scopes| scopes.into_iter().map(|s| s.scope_hash).collect::<Vec<_>>()),
        Some(vec![rag_rat_base::hash::hex_sha256(b"Twin as Beta")]),
    );
}

/// A binding this store relocated — its handle now names a symbol under a different qualified
/// name, recorded as the resolution — still publishes its scope under the authored anchor:
/// the handle guard compares against the name the store resolved the binding to, not the
/// authored one, or every relocated anchor would lose the scope that tells twins apart. A
/// handle whose row answers to neither name is stale, and publishes none.
#[test]
fn a_relocated_binding_still_publishes_the_scope_its_handle_resolves_to() {
    let (conn, _stream) = conn_with_stream();
    conn.execute(
        "INSERT INTO files(repo_id, path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES (?1, 'src/lib.rs', 'rust', 'code', 'sha', 1, 1)",
        [REPO],
    )
    .unwrap();
    let file_id = conn.last_insert_rowid();
    conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES ('src/lib.rs::Twin')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, scope_path, kind,
                    start_byte, end_byte, start_line, end_line)
             VALUES (?1, 'rust', 'Twin', (SELECT id FROM name_strings WHERE value = \
         'src/lib.rs::Twin'),
                     'Twin as Beta', 'impl', 0, 1, 1, 1)",
        [file_id],
    )
    .unwrap();
    let symbol_id = conn.last_insert_rowid();
    let seed = |id: &str, resolved_to: Option<&str>| {
        insert_memory(&conn, id, "active", 1);
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, symbol_id, symbol_kind,
                     anchor_status, created_at_ms, resolved, resolved_binding_id)
                 VALUES (?1, ?2, 'symbol', 'src/lib.rs::OldTwin', ?3, 'impl', 'current', 1,
                         ?4, ?5)",
            params![REPO, id, symbol_id, resolved_to.map(|_| 1), resolved_to],
        )
        .unwrap();
    };
    seed("mem_relocated", Some("src/lib.rs::Twin"));
    seed("mem_stale_handle", None);
    let scopes = |id: &str| match anchor_scopes_op(&conn, id).unwrap() {
        MemoryOp::NodeAnchorScopes { scopes, .. } =>
            scopes.into_iter().map(|scope| (scope.binding_id, scope.scope_hash)).collect::<Vec<_>>(),
        other => panic!("unexpected op {other:?}"),
    };
    assert_eq!(
        scopes("mem_relocated"),
        vec![("src/lib.rs::OldTwin".to_string(), rag_rat_base::hash::hex_sha256(b"Twin as Beta"))],
        "the scope is published under the AUTHORED anchor, found through the resolution",
    );
    assert!(scopes("mem_stale_handle").is_empty(), "a handle answering to neither name is stale");
}

/// The publish budget counts MEMORIES, not ops. Each swept memory owes up to two ops — its
/// anchors and the hash describing them — so counting ops would halve the budget the moment
/// hashes exist, and the pass would reach 32 memories instead of 64.
#[test]
fn the_publish_budget_counts_memories_not_the_ops_they_owe() {
    let (conn, stream) = conn_with_stream();
    // One more than the budget, all publishable and all hashed, oldest first.
    for index in 0..=ANCHOR_BACKFILL_PER_PASS {
        let id = format!("mem_{index:04}");
        insert_memory(&conn, &id, "active", index as i64);
        set_source_hash(&conn, &id);
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, anchor_status,
                     created_at_ms)
                 VALUES (?1, ?2, 'path', ?3, 'src/lib.rs', 'current', 1)",
            params![REPO, id, format!("src/lib.rs:{index:04}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
                 VALUES (?1, ?2, '{}', 'active')",
            params![stream.to_bytes().as_slice(), id],
        )
        .unwrap();
    }

    let work = read_reconcile_work(&conn, REPO, stream, StreamSealPolicy::Plaintext).unwrap();

    let swept: BTreeSet<String> = work
        .anchor_backfill_ops
        .iter()
        .map(|op| match op {
            MemoryOp::NodeAnchors { node_id, .. }
            | MemoryOp::NodeSourceHash { node_id, .. }
            | MemoryOp::NodeAnchorScopes { node_id, .. } => node_id.as_str().to_string(),
            other => panic!("the sweep authors anchors, hashes and scopes only, got {other:?}"),
        })
        .collect();
    assert_eq!(swept.len(), ANCHOR_BACKFILL_PER_PASS, "a full budget of memories");
    assert_eq!(
        work.anchor_backfill_ops.len(),
        ANCHOR_BACKFILL_PER_PASS * 3,
        "each swept memory owes its anchors, the hash describing them and their scopes",
    );
    assert!(
        !swept.contains(&format!("mem_{ANCHOR_BACKFILL_PER_PASS:04}")),
        "the one past the budget is the next pass's work",
    );
}

/// Stamp a memory with a source hash of the shape the local write path produces.
fn set_source_hash(conn: &Connection, id: &str) {
    conn.execute("UPDATE repo_memories SET source_text_hash = ?2 WHERE id = ?1", params![
        id,
        rag_rat_base::hash::hex_sha256(id.as_bytes())
    ])
    .unwrap();
}

/// The positive control for both gates above: a LOCAL memory with a binding and no snapshot IS
/// swept, so neither test can pass by the query simply returning nothing.
#[test]
fn the_anchor_sweep_selects_a_local_memory_owed_a_snapshot() {
    let (conn, stream) = conn_with_stream();
    insert_memory(&conn, "mem_owed", "active", 1);
    conn.execute(
        "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_owed', 'path', 'src/lib.rs', 'src/lib.rs', 'current', 1)",
        [REPO],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_owed', '{}', 'active')",
        params![stream.to_bytes().as_slice()],
    )
    .unwrap();

    let swept = read_anchor_backfill_ids(&conn, REPO, stream).unwrap();
    assert!(swept.contains(&"mem_owed".to_string()), "the sweep must select what it is for");
}

/// A DB with the memory schema, one registered repo, and the connection scoped to it — the
/// minimal setup `memory_repo_scope` needs to resolve an active repo.
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

fn insert_memory(conn: &Connection, id: &str, status: &str, created_at_ms: i64) {
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?1, 'body', 'high', ?2, 'agent', ?3, ?3, 'agent', 'h', 'v1',
                 ?4)",
        params![id, status, created_at_ms, REPO],
    )
    .unwrap();
}

/// Insert an active memory with a CUSTOM body — used to plant an adversarial oversized body
/// that a normal rag-rat memory (body ≤ 8 KiB) could never carry.
fn insert_memory_with_body(conn: &Connection, id: &str, body: &str) {
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?1, ?2, 'high', 'active', 'agent', 100, 100, 'agent', 'h',
                 'v1', ?3)",
        params![id, body, REPO],
    )
    .unwrap();
}

/// Point a freshly-opened connection at `repo` via the per-connection TEMP `connection_context`
/// (temp tables are connection-local, so each thread of the race test scopes its own).
fn set_scope(conn: &Connection, repo: &str) {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [repo],
    )
    .unwrap();
}

/// The projected `/3` status of `node_id` on the store's owner stream — the completeness mirror
/// the reconcile heals into. A test DB holds one repo/stream, so no stream filter is needed;
/// panics if the node is not projected (callers assert presence).
fn projected_node_status(conn: &Connection, node_id: &str) -> String {
    conn.query_row(
        "SELECT status FROM content_projected_nodes WHERE node_id = ?1",
        [node_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// Whether `node_id` has a projected `/3` node on the store's owner stream — the completeness
/// mirror. Unlike [`projected_node_status`], returns `false` (not a panic) when absent, so a
/// quarantined ghost can be asserted un-projected (#680).
fn is_projected(conn: &Connection, node_id: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM content_projected_nodes WHERE node_id = ?1)",
        [node_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        != 0
}

/// Insert a node-edge by RAW SQL, bypassing the wired `add_edge` author — a "ghost edge" that
/// exists in `repo_node_edges` but was never signed into the op-log.
fn insert_raw_node_edge(conn: &Connection, source: &str, relation: &str, target: &str) {
    let key = rag_rat_query::memory::edge_key(source, relation, "node", target);
    conn.execute(
        "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, target_repo_id,
                 target_kind, target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?2, 'node', ?5, ?5, 'current', 100)",
        params![key, REPO, source, relation, target],
    )
    .unwrap();
}

fn queue_pending_refold(conn: &Connection, stream: StreamId) {
    conn.execute("INSERT INTO content_streams_pending_refold(stream_id) VALUES (?1)", [stream
        .to_bytes()
        .as_slice()])
        .unwrap();
}

/// Author a BARE `/3` NodeStatus for a node with NO `NodeCreate` — the `/3` analog of the INERT
/// op a pre-fix (#532) binary authored when it `mark_obsolete`'d a still-ghost memory. The
/// shared projector emits no node without content, so this leaves a stale status register
/// with no projected node. Requires the store's account + owner stream already established
/// (via a prior live create); it authors through the real `/3` seam so the register truly
/// lands.
fn author_inert_status_op(conn: &Connection, node_id: &str, status: NodeStatus) {
    let stream = rag_rat_oplog::owned_stream_v2_id(conn, REPO)
        .unwrap()
        .expect("account minted by a prior live create");
    let tx = conn.unchecked_transaction().unwrap();
    rag_rat_oplog::author_content_batch_in_tx(
        &tx,
        stream,
        &[MemoryOp::NodeStatus { node_id: NodeId::from(node_id), status }],
        100,
    )
    .unwrap();
    tx.commit().unwrap();
}

/// The `/3` content chain length — one entry per authored op, the retarget's signed op-log
/// (account genesis + `StreamOwn` live in `account_entries`, not counted here).
fn entry_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM content_entries", [], |r| r.get(0)).unwrap()
}

fn content_suites(conn: &Connection) -> Vec<u64> {
    let mut stmt = conn
        .prepare("SELECT signed_bytes FROM content_entries WHERE accepted = 1 ORDER BY rowid")
        .unwrap();
    stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|bytes| {
            rag_rat_oplog::decode_content_signed(&bytes.unwrap()).unwrap().header.crypto_suite
        })
        .collect()
}

fn projected_node_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM content_projected_nodes", [], |r| r.get(0)).unwrap()
}

/// The number of local accounts minted (the single-row pointer table): 0 before any established
/// reconcile, 1 after — proves a no-op / scope-gated path mints NO account.
fn local_account_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM oplog_local_account", [], |r| r.get(0)).unwrap()
}

/// The number of folded `StreamOwn` ownership facts — one per owned `/2` stream.
fn owned_stream_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM account_stream_ownership", [], |r| r.get(0)).unwrap()
}

/// #560 durability split: an authored write commits under `synchronous = FULL`, and the guard
/// restores the connection's `NORMAL` default on drop so derived-index writes are unaffected.
/// Uses a file-backed index connection (WAL + NORMAL, like every real open) because an
/// in-memory database ignores the `synchronous` setting and would not report the change.
#[test]
fn authored_durability_raises_full_then_restores_normal() {
    let dir = rag_rat_base::test_scratch::ScratchDir::new("authdur");
    let storage = rag_rat_db::storage::IndexConnection::open(&dir.join("index.db")).unwrap();
    let conn = storage.connection();
    let synchronous = |c: &Connection| -> i64 {
        c.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap()
    };

    assert_eq!(synchronous(conn), 1, "an index connection defaults to synchronous=NORMAL (=1)");
    {
        let _durability = AuthoredDurability::begin(conn).unwrap();
        assert_eq!(
            synchronous(conn),
            2,
            "an authored write must raise synchronous=FULL (=2) for its commit"
        );
    }
    assert_eq!(
        synchronous(conn),
        1,
        "the authored-durability guard must restore synchronous=NORMAL (=1) on drop"
    );
}

/// Both sources of signed `NodeContent` carry each column into its OWN register. `confidence` and
/// `source` are adjacent short tokens signed verbatim into an append-only log, so the fixtures give
/// every register a distinct value: a transposition fails here instead of signing the wrong bytes.
#[test]
fn node_content_maps_every_column_to_its_own_register() {
    let row = MemoryRow {
        memory_id: "mem_a".to_string(),
        kind: "Decision".to_string(),
        title: "the title".to_string(),
        body: "the body".to_string(),
        confidence: "low".to_string(),
        status: "active".to_string(),
        source: "human".to_string(),
        payload_json: Some("{\"schema_version\":1}".to_string()),
        tags: vec!["tag".to_string()],
    };
    assert_eq!(node_content_of_row(&row), NodeContent {
        kind: "Decision".to_string(),
        title: "the title".to_string(),
        body: "the body".to_string(),
        confidence: "low".to_string(),
        source: "human".to_string(),
        tags: vec!["tag".to_string()],
        payload: Some("{\"schema_version\":1}".to_string()),
    });

    let conn = scoped_conn();
    insert_memory(&conn, "mem_b", "active", 100);
    let memory = rag_rat_query::memory::memory_by_id(&conn, "mem_b").unwrap().unwrap();
    assert_eq!(node_content_of_memory(&memory), NodeContent {
        kind: "Invariant".to_string(),
        title: "mem_b".to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        source: "agent".to_string(),
        tags: Vec::new(),
        payload: None,
    });
}

/// A `MemoryRow` with the given status, no payload, one tag — the fixture the ported op-split
/// tests translate.
fn op_row(status: &str) -> MemoryRow {
    MemoryRow {
        memory_id: "mem_a".to_string(),
        kind: "Invariant".to_string(),
        title: "t".to_string(),
        body: "b".to_string(),
        confidence: "high".to_string(),
        status: status.to_string(),
        source: "agent".to_string(),
        payload_json: None,
        tags: vec!["x".to_string()],
    }
}

#[test]
fn node_ops_and_edge_add_op_translate_content_status_and_an_edge() {
    // GENESIS (elide=true) on a non-active memory: NodeCreate then a NodeStatus (obsolete is
    // not the active default). `edge_add_op` yields one EdgeAdd and DELIBERATELY no
    // Rebind — the per-device resolved dimension (target_node_id / anchor_status) is
    // recomputed on read, never signed into the log.
    let ops = node_ops(&op_row("obsolete"), true).unwrap();
    assert_eq!(ops.len(), 2);
    assert!(matches!(&ops[0], MemoryOp::NodeCreate { node_id, .. } if node_id.as_str() == "mem_a"));
    assert!(
        matches!(&ops[1], MemoryOp::NodeStatus { status, .. } if status.as_db_str() == "obsolete")
    );
    let edge = NodeEdge {
        edge_key: "k1".to_string(),
        source_node_id: "mem_a".to_string(),
        relation: "relates_to".to_string(),
        target_repo_id: REPO.to_string(),
        target_kind: "node".to_string(),
        target_anchor: "mem_b".to_string(),
        target_node_id: Some("mem_b".to_string()),
        anchor_status: "current".to_string(),
    };
    let edge_op = edge_add_op(&edge, REPO).unwrap();
    assert!(matches!(&edge_op, MemoryOp::EdgeAdd { .. }));
    let all_ops: Vec<MemoryOp> = ops.into_iter().chain(std::iter::once(edge_op)).collect();
    assert!(
        !all_ops.iter().any(|op| matches!(op, MemoryOp::Rebind { .. })),
        "the reconcile omits the per-device resolved dimension"
    );
}

#[test]
fn genesis_node_ops_for_an_active_memory_emit_no_status_op() {
    // elide=true (genesis, no stale registers): an active, edgeless memory is just its
    // NodeCreate — the fold's create-time default handles `active`.
    let ops = node_ops(&op_row("active"), true).unwrap();
    assert_eq!(ops.len(), 1, "an active memory on genesis is just its NodeCreate");
    assert!(matches!(&ops[0], MemoryOp::NodeCreate { .. }));
}

#[test]
fn incremental_node_ops_for_an_active_memory_do_emit_an_explicit_status() {
    // elide=false (incremental heal on a non-empty chain): ALWAYS emit NodeStatus, even
    // `active`, so a healed node's status wins its register at the new Lamport and overrides
    // any stale register a prior inert op left behind (decision 6 of #541).
    let ops = node_ops(&op_row("active"), false).unwrap();
    assert_eq!(ops.len(), 2, "an active memory on a heal emits an explicit NodeStatus");
    assert!(matches!(&ops[0], MemoryOp::NodeCreate { .. }));
    assert!(
        matches!(&ops[1], MemoryOp::NodeStatus { status, .. } if status.as_db_str() == "active"),
        "the incremental branch emits NodeStatus{{active}} to win the register"
    );
}

#[test]
fn node_ops_fails_on_an_unknown_status() {
    // A status token this binary can't map must FAIL, not silently default the signed history
    // to `active`. Holds in either branch — an unknown token is never the active
    // default.
    let err = node_ops(&op_row("future_status_from_a_newer_binary"), true).unwrap_err();
    assert!(err.to_string().contains("unknown status"), "an unknown status fails authoring");
}

#[test]
fn sealed_authorability_reserves_the_aead_overhead() {
    let mut row = op_row("active");
    let mut found = false;
    for len in 261_000..262_500 {
        row.body = "x".repeat(len);
        if node_is_authorable(&row, StreamSealPolicy::Plaintext)
            && !node_is_authorable(&row, StreamSealPolicy::Sealed)
        {
            found = true;
            break;
        }
    }
    assert!(found, "the sealed policy rejects an op in the 40-byte AEAD overhead window");
}

#[test]
fn empty_preparation_mints_no_key_and_arms_no_policy() {
    let conn = scoped_conn();
    backfill_memory_oplog(&conn, 1_000).unwrap();
    assert!(prepare_live_authoring(&conn, &[], 2_000).unwrap().is_none());
    let secret_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM account_entries WHERE log_id = 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(secret_entries, 0);
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, REPO, STREAM_SEAL_POLICY_META_KEY).unwrap(),
        None
    );
}

/// #541/#664: the reconcile's memory reader anti-joins `repo_memories` against the accepted-`/3`
/// projection `content_projected_nodes` — only a row with no projected node (never signed)
/// comes back. `stream` is an opaque `StreamId` here (the anti-join only needs seed/query
/// agreement).
#[test]
fn read_unauthored_memory_rows_returns_only_rows_absent_from_the_projection() {
    let conn = scoped_conn();
    insert_memory(&conn, "mem_live", "active", 100);
    insert_memory(&conn, "mem_ghost", "active", 200);
    let stream = StreamId::from_bytes([0x11; 32]);
    conn.execute(
        "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, 'mem_live', '{}', 'active')",
        params![stream.to_bytes().as_slice()],
    )
    .unwrap();
    let missing = read_unauthored_memory_rows(&conn, REPO, stream).unwrap();
    assert_eq!(missing.iter().map(|r| r.memory_id.as_str()).collect::<Vec<_>>(), ["mem_ghost"]);
}

/// A SYNCED memory is never the reconcile's to author — even absent from the projection (its
/// acceptance was revoked) — or the local device would forge authorship of, and re-legitimize,
/// content the account revoked (#691 A-pre, Trace 2). A local ghost in the same position WOULD
/// be authored.
#[test]
fn a_synced_memory_is_never_re_authored() {
    let conn = scoped_conn();
    insert_memory(&conn, "mem_synced", "active", 100);
    conn.execute("UPDATE repo_memories SET origin = 'synced' WHERE id = 'mem_synced'", []).unwrap();
    let stream = StreamId::from_bytes([0x22; 32]);
    assert!(
        read_unauthored_memory_rows(&conn, REPO, stream).unwrap().is_empty(),
        "a synced memory is not re-authored even when absent from the projection",
    );
}

// --- the per-node/edge self-healing reconcile (#541) ---

#[test]
fn a_ghost_memory_is_authored_on_the_next_reconcile() {
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap(); // roots the chain via genesis
    insert_memory(&conn, "mem_ghost", "obsolete", 500); // raw, un-authored ghost
    backfill_memory_oplog(&conn, 9_000).unwrap();
    assert_eq!(
        projected_node_status(&conn, "mem_ghost"),
        NodeStatus::Obsolete.as_db_str(),
        "the ghost is now authored with its create-time status",
    );
}

#[test]
fn heal_overrides_a_stale_status_register_left_by_an_inert_op() {
    // The decision-6 divergence: an inert NodeStatus{obsolete} exists, the row is now active,
    // the heal must author an explicit NodeStatus{active} so the projection matches the table.
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap(); // establishes the account + owner stream
    // Author an inert `/3` NodeStatus for a NOT-yet-created ghost (the /3 analog of the old
    // binary's inert op): a bare status register with no projected node.
    let ghost = "mem_ghost";
    author_inert_status_op(&conn, ghost, NodeStatus::Obsolete);
    insert_memory(&conn, ghost, "active", 500); // the table says active
    backfill_memory_oplog(&conn, 9_000).unwrap();
    assert_eq!(
        projected_node_status(&conn, ghost),
        NodeStatus::Active.as_db_str(),
        "explicit NodeStatus{{active}} overrode the stale register",
    );
}

#[test]
fn a_ghost_edge_on_a_live_node_is_authored_on_the_next_reconcile() {
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id;
    let b = create_concept(&conn, "b").unwrap().memory.memory_id;
    insert_raw_node_edge(&conn, &a, "relates_to", &b); // writes repo_node_edges directly
    backfill_memory_oplog(&conn, 9_000).unwrap();
    assert_eq!(projected_edge_count(&conn), 1, "ghost edge now signed");
}

#[test]
fn reconcile_is_idempotent_and_a_clean_repo_authors_nothing() {
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap();
    let before = entry_count(&conn);
    backfill_memory_oplog(&conn, 9_000).unwrap();
    assert_eq!(entry_count(&conn), before, "no ghost → no new /3 entry");
}

/// Abort the queue-clear DELETE for `stream`, poisoning its settle so an inline barrier settle
/// cannot clear the mark — the row's refold debt is retained and the barrier stays tripped.
fn poison_owner_stream_settle(conn: &Connection, stream: StreamId) {
    let hex: String = rag_rat_base::hash::hex_lower(&stream.to_bytes());
    conn.execute_batch(&format!(
        "CREATE TRIGGER poison_owner_queue_clear
             BEFORE DELETE ON content_streams_pending_refold
             WHEN OLD.stream_id = X'{hex}'
             BEGIN SELECT RAISE(ABORT, 'injected queue-clear failure'); END;"
    ))
    .unwrap();
}

#[test]
fn pending_owner_refold_inline_settles_on_the_mutation_path() {
    // #798 finding 5: a tripped barrier on the AUTOCOMMIT mutation path no longer hard-fails —
    // it attempts ONE inline, targeted settle of the owner stream and, when that clears the
    // debt, PROCEEDS. A remote ingest enqueuing owner-stream debt must not wedge every local
    // mutation behind a settle no local caller schedules.
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap();
    let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
    insert_memory(&conn, "mem_ghost", "active", 500);
    queue_pending_refold(&conn, stream);

    // The reconcile trips the barrier, inline-settles the settle-able owner stream, and then
    // authors the ghost — no manual settle needed.
    backfill_memory_oplog(&conn, 9_000).unwrap();
    assert!(is_projected(&conn, "mem_ghost"), "the inline-settled reconcile authors the ghost");
    assert!(
        !rag_rat_oplog::content_stream_has_pending_refold(&conn, stream).unwrap(),
        "the inline settle cleared the owner stream's refold debt",
    );

    // A subsequent mutation on the same clean stream also succeeds and authors no duplicates.
    create_concept(&conn, "after inline settle").unwrap();
    let entries_after = entry_count(&conn);
    backfill_memory_oplog(&conn, 10_000).unwrap();
    assert_eq!(entry_count(&conn), entries_after, "the retry authors no duplicates");
}

#[test]
fn a_still_pending_owner_refold_errors_after_a_failed_in_tx_settle() {
    // #798 finding 5: the barrier self-heals only when the in-transaction settle actually
    // clears the debt. A stream whose settle keeps failing (poisoned) rolls the whole write
    // back, so the barrier stays FAIL-CLOSED rather than reading a stale accepted-/3
    // projection — the debt survives and nothing is authored.
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap();
    let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
    insert_memory(&conn, "mem_ghost", "active", 500);
    queue_pending_refold(&conn, stream);
    poison_owner_stream_settle(&conn, stream);
    let entries_before = entry_count(&conn);

    // The failure now surfaces from the settle itself (it is attempted inside the write's own
    // transaction) rather than from a barrier that refused to try.
    let reconcile_err = format!("{:#}", backfill_memory_oplog(&conn, 9_000).unwrap_err());
    assert!(
        reconcile_err.contains("pending content refold"),
        "the rolled-back write names the unsettled owner stream: {reconcile_err}",
    );
    let mutation_err = format!("{:#}", create_concept(&conn, "blocked mutation").unwrap_err());
    assert!(
        mutation_err.contains("pending content refold"),
        "a live mutation fails closed the same way: {mutation_err}",
    );
    assert_eq!(entry_count(&conn), entries_before, "the still-tripped barrier authors nothing");
    assert!(
        rag_rat_oplog::content_stream_has_pending_refold(&conn, stream).unwrap(),
        "the poisoned settle retained the refold debt",
    );
    assert!(!is_projected(&conn, "mem_ghost"), "the ghost is not authored while the barrier trips",);
}

#[test]
fn pending_owner_refold_inline_settles_on_the_edge_reconcile_path() {
    // #798 finding 5, edge path: the same inline settle unblocks a ghost EDGE reconcile.
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id;
    let b = create_concept(&conn, "b").unwrap().memory.memory_id;
    insert_raw_node_edge(&conn, &a, "relates_to", &b);
    let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
    queue_pending_refold(&conn, stream);

    backfill_memory_oplog(&conn, 9_000).unwrap();
    assert_eq!(projected_edge_count(&conn), 1, "the inline-settled reconcile authors the edge");
    assert!(
        !rag_rat_oplog::content_stream_has_pending_refold(&conn, stream).unwrap(),
        "the inline settle cleared the owner stream's refold debt",
    );
    let entries_after = entry_count(&conn);
    backfill_memory_oplog(&conn, 11_000).unwrap();
    assert_eq!(entry_count(&conn), entries_after, "the retry authors no duplicate edge");
}

#[test]
fn pending_unrelated_stream_does_not_block_owner_reconcile() {
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap();
    insert_memory(&conn, "mem_ghost", "active", 500);
    let unrelated = StreamId::from_bytes([0x51; 32]);
    queue_pending_refold(&conn, unrelated);

    backfill_memory_oplog(&conn, 9_000).unwrap();
    assert!(is_projected(&conn, "mem_ghost"));
    assert!(rag_rat_oplog::content_stream_has_pending_refold(&conn, unrelated).unwrap());
}

#[test]
fn an_unreadable_status_ghost_fails_the_mutation_path_loudly() {
    // Blast-radius pin: a ghost carrying a status token THIS binary cannot decode makes the
    // whole reconcile (hence the mutation that triggered it) fail, rather than silently minting
    // `active`.
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap();
    insert_memory(&conn, "mem_future", "some_future_status", 500);
    assert!(backfill_memory_oplog(&conn, 9_000).is_err());
}

#[test]
fn backfill_authors_every_memory_and_is_idempotent() {
    let conn = scoped_conn();
    insert_memory(&conn, "mem_a", "active", 100);
    insert_memory(&conn, "mem_b", "active", 200);
    insert_memory(&conn, "mem_c", "active", 300);
    // Insert a typed edge FROM mem_b DIRECTLY (not via the now-live `add_edge`, which would
    // author it eagerly and defeat this isolation test), THEN mark mem_b obsolete — so the
    // explicit backfill below is the SOLE authoring path and must still capture the edge from a
    // now-non-live source (the complete-history guard: the live reader hides it).
    let ek = rag_rat_query::memory::edge_key("mem_b", "relates_to", "node", "mem_c");
    conn.execute(
        "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, 'mem_b', 'relates_to', ?2, 'node', 'mem_c', 'mem_c', 'current', 0)",
        params![ek, REPO],
    )
    .unwrap();
    conn.execute("UPDATE repo_memories SET status = 'obsolete' WHERE id = 'mem_b'", []).unwrap();

    backfill_memory_oplog(&conn, 1_000).unwrap();
    // 3 NodeCreate + 1 NodeStatus (mem_b obsolete) + 1 EdgeAdd (from obsolete mem_b) = 5 `/3`
    // content entries; 3 projected nodes.
    assert_eq!(entry_count(&conn), 5);
    assert_eq!(projected_node_count(&conn), 3);
    assert_eq!(projected_node_status(&conn, "mem_b"), "obsolete");
    let edges: i64 =
        conn.query_row("SELECT COUNT(*) FROM content_projected_edges", [], |r| r.get(0)).unwrap();
    assert_eq!(edges, 1);

    // A second backfill is a no-op — the atomic batch already completed (chain non-empty).
    backfill_memory_oplog(&conn, 2_000).unwrap();
    assert_eq!(entry_count(&conn), 5, "re-running backfill authors nothing more");
}

#[test]
fn backfill_is_a_noop_on_the_placeholder_repo() {
    // An unadopted DB scoped to the legacy `__unassigned__` placeholder: backfilling would sign
    // an immutable owner stream that adoption can never re-point, so it must no-op even with
    // memories present.
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [rag_rat_base::repo_identity::LEGACY_REPO_ID],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES ('mem_a', 'Invariant', 'mem_a', 'body', 'high', 'active', 'agent', 1, 1,
                 'agent', 'h', 'v1', ?1)",
        [rag_rat_base::repo_identity::LEGACY_REPO_ID],
    )
    .unwrap();

    backfill_memory_oplog(&conn, 1_000).unwrap();
    assert_eq!(entry_count(&conn), 0, "the placeholder repo is not backfilled");
    assert_eq!(
        local_account_count(&conn),
        0,
        "the scope gate no-ops BEFORE the mint — a placeholder repo mints no account",
    );
}

#[test]
fn backfill_is_a_noop_on_a_local_only_repo() {
    // A machine-local `local:` shallow-clone id is upgraded to a portable id when the clone is
    // deepened, re-pointing the rows — so an immutable owner stream must not be rooted on it.
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    let local_id = format!("{}deadbeef", rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX);
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
        [&local_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES ('mem_a', 'Invariant', 'mem_a', 'body', 'high', 'active', 'agent', 1, 1,
                 'agent', 'h', 'v1', ?1)",
        [&local_id],
    )
    .unwrap();

    backfill_memory_oplog(&conn, 1_000).unwrap();
    assert_eq!(entry_count(&conn), 0, "a local: repo is not backfilled");
    assert_eq!(local_account_count(&conn), 0, "a local: repo mints no account");
}

#[test]
fn backfill_is_a_noop_on_an_unscoped_db() {
    // No repos row, no connection scope → memory_repo_scope is None → nothing to root a stream.
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    backfill_memory_oplog(&conn, 1_000).unwrap();
    assert_eq!(entry_count(&conn), 0);
    assert_eq!(local_account_count(&conn), 0, "an unscoped DB mints no account");
}

#[test]
fn backfill_of_an_empty_scoped_repo_establishes_ownership_but_authors_no_content() {
    // A fresh scoped repo with no memories: the reconcile falls through the fast-path probe
    // (ownership not yet established), mints the account, and publishes the `/2` StreamOwn —
    // but authors NO `/3` content (nothing is missing). So the content chain stays
    // empty while ownership is now live (the first live op will chain off an empty
    // content chain = genesis).
    let conn = scoped_conn();
    backfill_memory_oplog(&conn, 1_000).unwrap();
    assert_eq!(entry_count(&conn), 0, "no memories ⇒ no /3 content entries");
    assert_eq!(local_account_count(&conn), 1, "a scoped repo mints the store's local account");
    assert_eq!(owned_stream_count(&conn), 1, "and publishes exactly one /2 StreamOwn");
}

// --- live write-path wiring (#532) ---

/// Count LIVE projected edges (`present = 1`). A removed edge is retained as a tombstone
/// (`present = 0`, #691 A-pre) rather than deleted, so "how many edges are present" filters
/// them.
fn projected_edge_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM content_projected_edges WHERE present = 1", [], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Create an unanchored `Concept` (needs no code binding) through the LIVE `create_memory`.
fn create_concept(
    conn: &Connection,
    title: &str,
) -> anyhow::Result<rag_rat_query::memory::RepoMemoryCreateResult> {
    crate::memory_write::create_memory(conn, rag_rat_query::memory::RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: title.to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    })
}

#[test]
fn create_memory_authors_a_projected_node() {
    let conn = scoped_conn();
    let r = create_concept(&conn, "t1").unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = ?1",
            [&r.memory.memory_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "the created memory is a projected /3 node");
    assert_eq!(projected_node_count(&conn), 1);
    assert_eq!(content_suites(&conn), [0], "plaintext remains the default for existing repos");
}

#[test]
fn enable_is_idempotent_and_subsequent_live_authoring_is_sealed_and_projected() {
    let conn = scoped_conn();
    assert!(enable_sealed_authoring(&conn, 1_000).unwrap());
    assert!(!enable_sealed_authoring(&conn, 2_000).unwrap());
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, REPO, STREAM_SEAL_POLICY_META_KEY).unwrap().as_deref(),
        Some("sealed")
    );
    let accepted_wraps: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM account_entries WHERE log_id = 1 AND accepted = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(accepted_wraps, 1, "enable establishes exactly one accepted first-key wrap");

    let first = create_concept(&conn, "sealed one").unwrap().memory.memory_id;
    let second = create_concept(&conn, "sealed two").unwrap().memory.memory_id;
    assert_eq!(content_suites(&conn), [1, 1]);
    assert!(is_projected(&conn, &first));
    assert!(is_projected(&conn, &second));

    crate::memory_write::update_memory(&conn, rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: first.clone(),
        kind: None,
        title: None,
        body: Some("sealed update".to_string()),
        confidence: None,
        status: Some("obsolete".to_string()),
        tags: None,
        payload_json: None,
    })
    .unwrap();
    let edge = crate::memory_write::add_edge(
        &conn,
        &second,
        EdgeRelation::RelatesTo,
        &rag_rat_query::memory::EdgeTarget::Node { repo_id: None, node_id: first },
    )
    .unwrap();
    crate::memory_write::remove_edge(&conn, &edge.edge_key).unwrap();
    assert!(content_suites(&conn).into_iter().all(|suite| suite == 1));
}

#[test]
fn publish_is_idempotent_makes_the_account_public_and_authors_public() {
    let conn = scoped_conn();
    assert!(enable_public_authoring(&conn, 1_000).unwrap());
    assert!(!enable_public_authoring(&conn, 2_000).unwrap());
    assert_eq!(
        rag_rat_db::meta::repo_meta(&conn, REPO, STREAM_ACCESS_MODE_META_KEY).unwrap().as_deref(),
        Some("public")
    );
    let account = rag_rat_oplog::local_account(&conn, 1_000).unwrap();
    assert!(rag_rat_oplog::account_is_fully_public(&conn, account).unwrap());

    // Live-write, drain, and reconcile all resolve the SAME public stream id — distinct from
    // the Private-mode id, so nothing desyncs.
    let public_id = rag_rat_oplog::owned_stream_v2_id_with_mode(
        &conn,
        REPO,
        rag_rat_oplog::AccessMode::PublicRead,
    )
    .unwrap()
    .unwrap();
    let private_id = rag_rat_oplog::owned_stream_v2_id_with_mode(
        &conn,
        REPO,
        rag_rat_oplog::AccessMode::Private,
    )
    .unwrap()
    .unwrap();
    assert_ne!(public_id, private_id, "the public stream has a distinct identity");
    assert_eq!(stable_owner_stream_for_repo(&conn, REPO).unwrap(), Some(public_id));

    // A created memory authors onto the public stream and the account stays fully public.
    let m = create_concept(&conn, "public one").unwrap().memory.memory_id;
    assert!(is_projected(&conn, &m));
    assert!(rag_rat_oplog::account_is_fully_public(&conn, account).unwrap());
}

#[test]
fn publish_refuses_an_account_that_already_has_private_memories() {
    let conn = scoped_conn();
    // Authors a Private `/2` StreamOwn — the account can never become fully public thereafter.
    create_concept(&conn, "private history").unwrap();
    let err = enable_public_authoring(&conn, 2_000).unwrap_err().to_string();
    assert!(err.contains("private stream") || err.contains("fresh index"), "got: {err}");
}

#[test]
fn publish_and_seal_are_mutually_exclusive_both_directions() {
    let sealed_first = scoped_conn();
    enable_sealed_authoring(&sealed_first, 1_000).unwrap();
    assert!(
        enable_public_authoring(&sealed_first, 2_000).is_err(),
        "a sealed repo cannot be published"
    );

    let public_first = scoped_conn();
    enable_public_authoring(&public_first, 1_000).unwrap();
    assert!(
        enable_sealed_authoring(&public_first, 2_000).is_err(),
        "a published repo cannot be sealed"
    );
}

#[test]
fn publish_access_mode_ratchet_survives_a_deleted_intent_row() {
    let conn = scoped_conn();
    enable_public_authoring(&conn, 1_000).unwrap();
    let account = rag_rat_oplog::local_account(&conn, 1_000).unwrap();
    // Simulate intent-row loss (external tooling / a meta bug): the op-log's PublicRead
    // StreamOwn must keep the mode public, or the next write authors a second (Private)
    // StreamOwn and permanently mixes the account.
    conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", rusqlite::params![
        REPO,
        STREAM_ACCESS_MODE_META_KEY
    ])
    .unwrap();
    assert_eq!(
        owner_stream_access_mode(&conn, REPO).unwrap(),
        rag_rat_oplog::AccessMode::PublicRead,
        "the derived op-log fact keeps the ratchet public after intent-row loss"
    );
    create_concept(&conn, "after intent loss").unwrap();
    assert!(
        rag_rat_oplog::account_is_fully_public(&conn, account).unwrap(),
        "a lost intent row must not let a Private StreamOwn mix the published account"
    );
}

#[test]
fn catch_up_is_idempotent_for_an_already_covered_effective_device() {
    let conn = scoped_conn();
    enable_sealed_authoring(&conn, 1_000).unwrap();
    let target = rag_rat_oplog::local_device(&conn, 1_000).unwrap().fingerprint();

    let first = catch_up_enrolled_device_keys(&conn, target, 2_000).unwrap();
    assert!(first.authored.is_empty());
    assert_eq!(first.already_covered.len(), 1);
    let second = catch_up_enrolled_device_keys(&conn, target, 3_000).unwrap();
    assert!(second.authored.is_empty());
    assert_eq!(second.already_covered, first.already_covered);
}

#[test]
fn catch_up_rejects_a_non_effective_target_without_partial_rows() {
    let conn = scoped_conn();
    enable_sealed_authoring(&conn, 1_000).unwrap();
    let before = entry_count(&conn);

    let err = catch_up_enrolled_device_keys(
        &conn,
        rag_rat_oplog::DeviceFingerprint::from_bytes([0xff; 32]),
        2_000,
    )
    .unwrap_err();
    assert!(err.to_string().contains("not currently roster-effective"));
    assert_eq!(entry_count(&conn), before);
    let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
    assert_eq!(synchronous, 1, "the durability guard restores NORMAL after rollback");
}

#[test]
fn catch_up_only_reports_the_active_repos_owner_stream() {
    let conn = scoped_conn();
    enable_sealed_authoring(&conn, 1_000).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b', 'repo-b', 0)",
        [],
    )
    .unwrap();
    set_scope(&conn, "repo-b");
    enable_sealed_authoring(&conn, 2_000).unwrap();
    let target = rag_rat_oplog::local_device(&conn, 2_000).unwrap().fingerprint();

    set_scope(&conn, REPO);
    let report = catch_up_enrolled_device_keys(&conn, target, 3_000).unwrap();
    assert!(report.authored.is_empty());
    assert_eq!(report.already_covered.len(), 1, "repo-b's live key is outside repo-a scope");
}

#[test]
fn sealed_reconcile_authors_a_ghost_once() {
    let conn = scoped_conn();
    enable_sealed_authoring(&conn, 1_000).unwrap();
    insert_memory(&conn, "sealed-ghost", "active", 100);
    backfill_memory_oplog(&conn, 2_000).unwrap();
    assert_eq!(content_suites(&conn), [1]);
    assert!(is_projected(&conn, "sealed-ghost"));
    backfill_memory_oplog(&conn, 3_000).unwrap();
    assert_eq!(content_suites(&conn), [1], "repeat reconcile does not duplicate sealed history");
}

#[test]
fn sealed_policy_is_repo_scoped_and_the_ratchet_survives_deleted_intent() {
    let conn = scoped_conn();
    enable_sealed_authoring(&conn, 1_000).unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('repo-b', 'repo-b', 0)",
        [],
    )
    .unwrap();
    set_scope(&conn, "repo-b");
    let b = create_concept(&conn, "repo b plaintext").unwrap().memory.memory_id;
    assert!(is_projected(&conn, &b));
    assert_eq!(content_suites(&conn), [0], "a sibling repo remains plaintext by default");

    set_scope(&conn, REPO);
    conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", params![
        REPO,
        STREAM_SEAL_POLICY_META_KEY
    ])
    .unwrap();
    create_concept(&conn, "ratcheted sealed").unwrap();
    assert_eq!(content_suites(&conn), [0, 1], "a wrap prevents plaintext downgrade");
}

#[test]
fn policy_revalidation_failure_rolls_back_the_table_mutation() {
    let conn = scoped_conn();
    backfill_memory_oplog(&conn, 1_000).unwrap();
    let prepared = prepare_live_content_authoring(&conn, 2_000).unwrap().unwrap();
    rag_rat_db::meta::set_repo_meta(&conn, REPO, STREAM_SEAL_POLICY_META_KEY, "sealed").unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    insert_memory(&tx, "must-roll-back", "active", 100);
    let memory = rag_rat_query::memory::memory_by_id(&tx, "must-roll-back").unwrap().unwrap();
    assert!(author_create(&tx, &memory, Some(&prepared), 2_000).is_err());
    drop(tx);
    assert!(rag_rat_query::memory::memory_by_id(&conn, "must-roll-back").unwrap().is_none());
}

#[test]
fn a_scope_less_create_authors_nothing() {
    // No repos row / no active-repo context → the scope gate skips authoring entirely.
    let conn = Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    create_concept(&conn, "t1").unwrap();
    assert_eq!(entry_count(&conn), 0, "a scope-less create never touches the log");
    assert_eq!(local_account_count(&conn), 0, "a scope-less create mints no account");
}

#[test]
fn update_memory_authors_node_update_and_a_status_change() {
    let conn = scoped_conn();
    let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
    crate::memory_write::update_memory(&conn, rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: id.clone(),
        kind: None,
        title: None,
        body: Some("a new body".to_string()),
        confidence: None,
        status: Some("obsolete".to_string()),
        tags: None,
        payload_json: None,
    })
    .unwrap();
    let (content_json, status): (String, String) = conn
        .query_row(
            "SELECT content_json, status FROM content_projected_nodes WHERE node_id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(content_json.contains("a new body"), "the NodeUpdate replaced the content");
    assert_eq!(status, "obsolete", "the status change authored a NodeStatus");
}

#[test]
fn mark_obsolete_authors_only_a_status_op_no_node_update() {
    let conn = scoped_conn();
    let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
    // The NodeCreate is the sole entry so far.
    assert_eq!(entry_count(&conn), 1);
    crate::memory_write::mark_obsolete(&conn, &id).unwrap();
    assert_eq!(projected_node_status(&conn, &id), "obsolete");
    // A status-only change authors EXACTLY ONE op (a NodeStatus) — NOT a NodeUpdate, which in a
    // synced stream could revert a concurrent content edit (content/status are independent
    // LWW).
    assert_eq!(entry_count(&conn), 2, "status-only update authors one NodeStatus, no NodeUpdate");
}

#[test]
fn a_no_op_update_authors_nothing() {
    let conn = scoped_conn();
    let id = create_concept(&conn, "t1").unwrap().memory.memory_id;
    assert_eq!(entry_count(&conn), 1);
    // An update that changes neither content nor status is a complete no-op in the log.
    crate::memory_write::update_memory(&conn, rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: id,
        kind: None,
        title: None,
        body: None,
        confidence: None,
        status: None,
        tags: None,
        payload_json: None,
    })
    .unwrap();
    assert_eq!(entry_count(&conn), 1, "a change-free update authors no op");
}

#[test]
fn add_and_remove_edge_author_edge_presence() {
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id;
    let b = create_concept(&conn, "b").unwrap().memory.memory_id;
    let edge = crate::memory_write::add_edge(&conn, &a, EdgeRelation::RelatesTo, &{
        rag_rat_query::memory::EdgeTarget::Node { repo_id: None, node_id: b }
    })
    .unwrap();
    assert_eq!(projected_edge_count(&conn), 1, "add_edge authored an EdgeAdd");
    assert!(crate::memory_write::remove_edge(&conn, &edge.edge_key).unwrap());
    assert_eq!(projected_edge_count(&conn), 0, "remove_edge authored an EdgeRemove tombstone");
}

#[test]
fn add_edge_rejects_an_oversized_target_anchor_at_write_validation() {
    // #680 (prevention): the write-boundary cap rejects an oversized `target_anchor` BEFORE the
    // row is persisted, so the normal API can never mint the un-authorable edge the reconcile
    // would otherwise have to quarantine. An EXPLICIT cross-repo target to a not-yet-indexed
    // repo is the one path that stores the caller's raw anchor verbatim, so it exercises the
    // cap.
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id;
    let oversized = "x".repeat(rag_rat_query::memory::MAX_EDGE_ANCHOR_LEN + 1);
    let err = crate::memory_write::add_edge(&conn, &a, EdgeRelation::RelatesTo, &{
        rag_rat_query::memory::EdgeTarget::Node {
            repo_id: Some("some-unindexed-repo".to_string()),
            node_id: oversized,
        }
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("over the"),
        "the byte cap rejects an oversized edge anchor: {err}",
    );
    let edges: i64 =
        conn.query_row("SELECT COUNT(*) FROM repo_node_edges", [], |r| r.get(0)).unwrap();
    assert_eq!(edges, 0, "no edge row is stored when the anchor is over the cap");
}

#[test]
fn a_node_with_too_many_tags_is_rejected_at_write_validation() {
    // #680 (P2): title/body are char-capped and payload is byte-capped, but the NUMBER of
    // tags is not — each tag is individually validated (≤ 64 chars) with no limit on how many.
    // Enough individually-valid tags overflow the signed `/3` envelope even with a tiny body
    // and no payload, so an "otherwise valid" create assembles an un-authorable
    // `NodeCreate`. Before the whole-op write-boundary guard that op was minted and
    // then SILENTLY quarantined by the reconcile; now it is rejected at write time with
    // an actionable error and no row persists.
    let conn = scoped_conn();
    // ~5000 unique 64-char tags: safely past the ~4000 the envelope admits, each within the
    // per-tag cap so it is INDIVIDUALLY valid (i.e. would have been accepted before this fix).
    let tags: Vec<String> = (0..5000).map(|i| format!("tag-{i:060}")).collect();
    for tag in &tags {
        assert_eq!(tag.chars().count(), 64, "each tag is exactly the 64-char per-tag cap");
        rag_rat_query::memory::validate_len("tag", tag, 64)
            .expect("each tag is individually valid — only the aggregate is un-authorable");
    }
    let err = crate::memory_write::create_memory(&conn, rag_rat_query::memory::RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: "too many tags".to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags,
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("too large"),
        "the whole-op guard rejects the tag aggregate at write time: {err}",
    );
    // Rejected at the write boundary — the row never persists (not accepted-then-quarantined).
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 0, "the un-authorable create rolled back — nothing was persisted");
}

#[test]
fn the_whole_op_guard_rejects_an_aggregate_no_single_field_cap_catches() {
    // #680 (the point of the ROOT fix): an op can overflow the signed `/3` envelope from
    // the SUM of fields that are EACH within their own cap — a max-ish payload + a max body +
    // many tags together. No single per-field cap (payload ≤ 128 KiB, body ≤ 8000 chars, tag ≤
    // 64 chars) rejects it; only the whole-op guard, checking the ASSEMBLED op, does.
    let conn = scoped_conn();
    // Payload just under the 128 KiB cap, as a valid JSON object — so `validate_payload`'s
    // object / canonical checks pass and only its byte cap could fire (and it does not).
    let filler = "x".repeat(rag_rat_query::memory::MAX_MEMORY_PAYLOAD_LEN - 16);
    let payload = format!("{{\"v\":\"{filler}\"}}");
    assert!(
        payload.len() <= rag_rat_query::memory::MAX_MEMORY_PAYLOAD_LEN,
        "the payload is within its own byte cap",
    );
    // Body exactly at the char cap.
    let body = "b".repeat(rag_rat_query::memory::MAX_MEMORY_BODY_LEN);
    // A control create with the payload + body but NO tags SUCCEEDS — proving neither field,
    // nor the two together, is over the envelope on its own, so it is specifically the
    // tag aggregate (below) that trips the whole-op guard, not any single field.
    crate::memory_write::create_memory(&conn, rag_rat_query::memory::RepoMemoryCreate {
        kind: "Task".to_string(),
        title: "aggregate control".to_string(),
        body: body.clone(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: Some(payload.clone()),
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    })
    .expect("payload + body alone are within the envelope — no single cap is exceeded");
    // The SAME payload + body PLUS many individually-valid tags tips the assembled op over the
    // envelope — caught ONLY by the whole-op guard, not by any per-field cap.
    let tags: Vec<String> = (0..2500).map(|i| format!("tag-{i:060}")).collect();
    let err = crate::memory_write::create_memory(&conn, rag_rat_query::memory::RepoMemoryCreate {
        kind: "Task".to_string(),
        title: "aggregate over cap".to_string(),
        body,
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags,
        payload_json: Some(payload),
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("too large"),
        "the aggregate op is rejected by the whole-op guard: {err}",
    );
    // Only the authorable control row persisted; the aggregate create rolled back.
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "only the authorable control create persisted");
}

// --- live mutation seam self-heals a ghost end to end (#541 Task 4) ---

#[test]
fn mark_obsolete_on_a_ghost_authors_a_create_not_an_inert_status() {
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap(); // roots the chain
    insert_memory(&conn, "mem_ghost", "active", 500); // raw, un-authored ghost
    // mark_obsolete reconciles first (heals NodeCreate + NodeStatus{active}), THEN authors the
    // obsolete NodeStatus — so it is NOT inert and the node projects obsolete.
    crate::memory_write::mark_obsolete(&conn, "mem_ghost").unwrap();
    assert_eq!(
        projected_node_status(&conn, "mem_ghost"),
        NodeStatus::Obsolete.as_db_str(),
        "ghost healed then obsoleted",
    );
}

#[test]
fn remove_edge_on_a_ghost_edge_heals_then_tombstones_not_an_inert_remove() {
    // The EdgeRemove path: `remove_edge` calls backfill (edges.rs) BEFORE its delete txn, so a
    // raw ghost edge is first healed (EdgeAdd authored), then the delete authors EdgeRemove —
    // the signed history is add→remove (complete), and the projection ends with the
    // edge ABSENT (not an inert tombstone with no matching add).
    //
    // `remove_edge` authors its `EdgeRemove` unconditionally once the raw row is deleted
    // (edges.rs gates it on `n > 0`, NOT on whether an `EdgeAdd` was ever signed) — so
    // `edges.is_empty()` alone is satisfied whether or not the heal ran (a
    // never-authored edge and a healed-then-removed edge both project empty). The
    // `entry_count` delta is what actually distinguishes them: it is +2 (heal's
    // `EdgeAdd` + `remove_edge`'s own `EdgeRemove`) only when the reconcile fired; a
    // disabled reconcile would author just the bare `EdgeRemove` (+1).
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id;
    let b = create_concept(&conn, "b").unwrap().memory.memory_id;
    insert_raw_node_edge(&conn, &a, "relates_to", &b);
    let key = rag_rat_query::memory::edge_key(&a, "relates_to", "node", &b);
    let before = entry_count(&conn);
    crate::memory_write::remove_edge(&conn, &key).unwrap();
    assert_eq!(
        entry_count(&conn),
        before + 2,
        "the heal's EdgeAdd + remove_edge's own EdgeRemove — not a bare, inert tombstone"
    );
    assert_eq!(projected_edge_count(&conn), 0, "healed then tombstoned → edge absent");
}

#[test]
fn a_failed_author_rolls_back_the_memory_write() {
    let conn = scoped_conn();
    // One good create so the account + owner stream are established and the second create's
    // backfill fast-paths, isolating the failure to the live author's reproject.
    create_concept(&conn, "first").unwrap();
    let before: i64 =
        conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
    // Poison the `/3` projector guard: pretend a NEWER binary already folded this store's `/3`
    // projection, so `reproject_accepted_content_stream`'s `assert_content_projector_not_newer`
    // errors and the second create's `/3` author fails (this doubles as the #664 `/3`
    // projector-stamp poison test). UPSERT: on this raw-connection store the stamp may be
    // absent — the per-stream reproject only MAINTAINS an already-current stamp (#688); the
    // open-path trigger (`rebuild_all_content_projections_if_stale`) is what writes it first.
    conn.execute(
        "INSERT INTO oplog_meta(key, value) VALUES ('content_projector_version', '999')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )
    .unwrap();
    assert!(create_concept(&conn, "second").is_err(), "the authoring failure fails the create");
    let after: i64 =
        conn.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
    assert_eq!(after, before, "strict-atomic: the failed create's row rolled back with it");
}

#[test]
fn a_live_create_backfills_pre_existing_memories_first() {
    let conn = scoped_conn();
    // A memory inserted by RAW SQL (never authored) — the pre-existing history.
    insert_memory(&conn, "old", "active", 100);
    // The first LIVE create backfills `old` (a NodeCreate) BEFORE authoring the new memory.
    let new_id = create_concept(&conn, "new").unwrap().memory.memory_id;
    assert_eq!(projected_node_count(&conn), 2, "old (backfilled) + new (live) are both projected");
    let old_present: i64 = conn
        .query_row("SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'old'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(old_present, 1, "the pre-existing memory was backfilled");
    assert_ne!(new_id, "old");
}

/// Create an unanchored `Concept` with tags through the LIVE `create_memory`.
fn create_concept_tagged(conn: &Connection, title: &str, tags: Vec<String>) -> String {
    crate::memory_write::create_memory(conn, rag_rat_query::memory::RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: title.to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags,
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    })
    .unwrap()
    .memory
    .memory_id
}

fn update_tags(conn: &Connection, id: &str, tags: Vec<String>) {
    crate::memory_write::update_memory(conn, rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: id.to_string(),
        kind: None,
        title: None,
        body: None,
        confidence: None,
        status: None,
        tags: Some(tags),
        payload_json: None,
    })
    .unwrap();
}

#[test]
fn a_normalization_only_tag_change_authors_nothing() {
    let conn = scoped_conn();
    let id = create_concept_tagged(&conn, "t", vec!["x".to_string()]);
    let before = entry_count(&conn);
    // Tags that normalize to the SAME set: trailing space, duplicate, and an empty string.
    update_tags(&conn, &id, vec!["x ".to_string(), "x".to_string(), String::new()]);
    assert_eq!(
        entry_count(&conn),
        before,
        "a whitespace/duplicate-only re-tag is not a content change → no NodeUpdate"
    );
}

#[test]
fn a_real_tag_change_authors_a_node_update() {
    let conn = scoped_conn();
    let id = create_concept_tagged(&conn, "t", vec!["x".to_string()]);
    let before = entry_count(&conn);
    update_tags(&conn, &id, vec!["x".to_string(), "y".to_string()]);
    assert_eq!(entry_count(&conn), before + 1, "adding a real tag authors a NodeUpdate");
}

#[test]
fn re_adding_an_edge_authors_no_duplicate() {
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id;
    let b = create_concept(&conn, "b").unwrap().memory.memory_id;
    let target = |node: &str| rag_rat_query::memory::EdgeTarget::Node {
        repo_id: None,
        node_id: node.to_string(),
    };
    crate::memory_write::add_edge(&conn, &a, EdgeRelation::RelatesTo, &target(&b)).unwrap();
    let after_first = entry_count(&conn);
    assert_eq!(projected_edge_count(&conn), 1);
    // Re-adding the SAME edge is an idempotent resolution-refresh — it must NOT author a second
    // EdgeAdd (which could resurrect a concurrent remove under sync).
    crate::memory_write::add_edge(&conn, &a, EdgeRelation::RelatesTo, &target(&b)).unwrap();
    assert_eq!(
        entry_count(&conn),
        after_first,
        "an idempotent edge re-add authors no duplicate EdgeAdd"
    );
    assert_eq!(projected_edge_count(&conn), 1);
}

// --- owner-bound /2//3 retarget (#664) ---

#[test]
fn a_fresh_repo_first_create_mints_the_account_publishes_ownership_and_projects_the_node() {
    let conn = scoped_conn();
    let id = create_concept(&conn, "first").unwrap().memory.memory_id;
    // The first create on a fresh scoped repo mints exactly one local account and folds exactly
    // one `/2` StreamOwn effective — the ownership the owner-authored `/3` content accepts
    // under.
    assert_eq!(local_account_count(&conn), 1, "the first create mints one local account");
    assert_eq!(owned_stream_count(&conn), 1, "exactly one /2 StreamOwn folds effective");
    // And the memory is an accepted, projected `/3` node.
    assert_eq!(projected_node_count(&conn), 1, "the created memory is a projected /3 node");
    assert_eq!(projected_node_status(&conn, &id), "active", "a fresh create projects active");
}

#[test]
fn a_second_repo_first_create_establishes_its_own_ownership_despite_the_shared_account() {
    // Multi-repo store, the distinguishing case for the fast-path probe: repo A's create mints
    // the STORE-GLOBAL account and establishes A's ownership. Repo B is then fresh with ZERO
    // memories. B's first create must STILL publish B's own `/2` StreamOwn — the account
    // already being minted is NOT enough. This is why the probe checks
    // `established_owned_stream_v2` (StreamOwn folded EFFECTIVE) and not merely
    // "account minted": with the weaker check, B's empty anti-join would early-return,
    // B would author `/3` content under an unowned stream, and verify-accepted would
    // roll the create back (Risk trap #1). A single-repo test cannot catch this — there
    // the account is unminted, so both checks agree.
    let conn = scoped_conn(); // repo-a: registered + scoped
    create_concept(&conn, "a1").unwrap();
    assert_eq!(local_account_count(&conn), 1, "repo-a's create mints the store account");
    assert_eq!(owned_stream_count(&conn), 1, "repo-a owns its /2 stream");

    // Register a SECOND repo in the same store and scope the connection to it.
    const REPO_B: &str = "repo-b";
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
        [REPO_B],
    )
    .unwrap();
    set_scope(&conn, REPO_B);

    // B's first create: the account is already minted, B has no StreamOwn and no memories.
    let id_b = create_concept(&conn, "b1").unwrap().memory.memory_id;
    assert_eq!(local_account_count(&conn), 1, "the account is store-global — still exactly one");
    assert_eq!(
        owned_stream_count(&conn),
        2,
        "repo-b established its OWN /2 StreamOwn rather than riding repo-a's",
    );
    assert_eq!(
        projected_node_status(&conn, &id_b),
        "active",
        "b1 accepted under repo-b's freshly-published ownership",
    );
}

#[test]
fn pre_existing_memories_are_adopted_into_v3_and_the_v1_tables_are_left_untouched() {
    let conn = scoped_conn();
    // Pre-existing history a pre-#664 binary / raw writer left in the tables (never signed).
    insert_memory(&conn, "old_a", "active", 100);
    insert_memory(&conn, "old_b", "obsolete", 200);
    insert_memory(&conn, "old_c", "active", 300);
    // One live mutation triggers the reconcile: the three pre-existing rows are authored into
    // `/3` as the genesis batch, then the new memory is authored live.
    create_concept(&conn, "trigger").unwrap();
    for id in ["old_a", "old_b", "old_c"] {
        let projected: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(projected, 1, "pre-existing memory {id} was adopted into /3");
    }
    assert_eq!(projected_node_status(&conn, "old_b"), "obsolete", "the adopted status carried");
    // The retained `/1` tables are UNTOUCHED — the live path no longer writes them (issue J1).
    let v1_entries: i64 =
        conn.query_row("SELECT COUNT(*) FROM oplog_entries", [], |r| r.get(0)).unwrap();
    let v1_nodes: i64 =
        conn.query_row("SELECT COUNT(*) FROM oplog_projected_nodes", [], |r| r.get(0)).unwrap();
    assert_eq!(v1_entries, 0, "the /1 entry log is not written by the retargeted live path");
    assert_eq!(v1_nodes, 0, "the /1 shadow projection is not written by the retarget");
}

#[test]
fn racing_backfills_converge_on_one_stream_own_and_no_duplicate_content() {
    use std::sync::{Arc, Barrier};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.db");
    // Setup once: schema, the registered repo, and one pre-existing memory so there is content
    // for the reconcile to author (the racers must converge on authoring it exactly once).
    let setup = Connection::open(&path).unwrap();
    setup.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    rag_rat_db::schema::apply(&setup, &crate::index::migration_hooks()).unwrap();
    setup
        .execute("INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)", [
            REPO,
        ])
        .unwrap();
    set_scope(&setup, REPO);
    insert_memory(&setup, "old", "active", 100);
    drop(setup);

    let barrier = Arc::new(Barrier::new(2));
    let spawn = || {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let conn = Connection::open(path).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
            set_scope(&conn, REPO);
            barrier.wait();
            backfill_memory_oplog(&conn, 9_000).unwrap();
        })
    };
    let a = spawn();
    let b = spawn();
    a.join().unwrap();
    b.join().unwrap();

    let conn = Connection::open(&path).unwrap();
    assert_eq!(local_account_count(&conn), 1, "the racers converge on one local account");
    assert_eq!(owned_stream_count(&conn), 1, "exactly one /2 StreamOwn survives the race");
    assert_eq!(entry_count(&conn), 1, "the pre-existing memory is authored exactly once");
    let old_nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'old'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(old_nodes, 1, "the adopted memory projects exactly once, no duplicate");
}

#[test]
fn an_oversized_ghost_is_quarantined_and_does_not_wedge_the_write_path() {
    // #680: a raw/imported memory whose signed /3 envelope exceeds the §18a 256 KiB cap is
    // un-authorable. The reconcile must QUARANTINE it (skip it) rather than `bail!` — the old
    // fail-loud posture made EVERY subsequent mutation fail at its pre-write backfill, wedging
    // the whole memory-write path with no recovery.
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap(); // roots the chain
    let oversized = "x".repeat(300 * 1024); // > 256 KiB ⇒ the signed envelope exceeds the cap
    insert_memory_with_body(&conn, "mem_big", &oversized); // raw, un-authorable ghost

    // The reconcile no longer errors — the poison ghost is quarantined, not fatal.
    backfill_memory_oplog(&conn, 1_000).unwrap();
    assert!(
        !is_projected(&conn, "mem_big"),
        "the oversized ghost is quarantined, never signed into the /3 log",
    );
    // The write path stays LIVE: a fresh mutation still succeeds instead of wedging.
    let live = create_concept(&conn, "still alive").unwrap().memory.memory_id;
    assert!(is_projected(&conn, &live), "other writes stay live despite the poison ghost");
}

#[test]
fn an_oversized_ghost_can_be_recovered_via_the_public_api() {
    // #680: before the fix an oversized ghost wedged every write with no way out. Now the write
    // path stays live, so the ghost is RECOVERABLE through the public API — shrink its body
    // under the cap and the next reconcile signs it like any other healed ghost.
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap();
    let oversized = "x".repeat(300 * 1024);
    insert_memory_with_body(&conn, "mem_big", &oversized);

    // Recover it: a plain `update_memory` shrinking the body under the cap — the mutation the
    // wedge would have blocked — now succeeds.
    crate::memory_write::update_memory(&conn, rag_rat_query::memory::RepoMemoryUpdate {
        memory_id: "mem_big".to_string(),
        kind: None,
        title: None,
        body: Some("shrunk".to_string()),
        confidence: None,
        status: None,
        tags: None,
        payload_json: None,
    })
    .unwrap();
    // The row is authorable now, so the next reconcile (any mutation) signs it.
    create_concept(&conn, "next").unwrap();
    assert_eq!(
        projected_node_status(&conn, "mem_big"),
        NodeStatus::Active.as_db_str(),
        "the shrunk row is signed on the next reconcile — fully recovered",
    );
}

#[test]
fn an_oversized_ghost_edge_is_quarantined_and_does_not_wedge_the_write_path() {
    // #680: the node quarantine's EDGE twin. A raw/imported edge whose signed /3 `EdgeAdd`
    // exceeds the §18a 256 KiB cap (an oversized `target_anchor`) is un-authorable. The
    // reconcile must QUARANTINE it (skip it) rather than `bail!` — otherwise that ONE edge
    // makes EVERY subsequent mutation fail at its pre-write backfill, wedging the whole
    // memory-write path exactly as an oversized node would (the gap the node-only
    // quarantine left open).
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id; // live, authorable source node
    let oversized_anchor = "x".repeat(300 * 1024); // > 256 KiB ⇒ the EdgeAdd envelope exceeds cap
    insert_raw_node_edge(&conn, &a, "relates_to", &oversized_anchor); // raw un-authorable ghost

    // The reconcile no longer errors — the poison edge is quarantined, not fatal.
    backfill_memory_oplog(&conn, 1_000).unwrap();
    assert_eq!(
        projected_edge_count(&conn),
        0,
        "the oversized ghost edge is quarantined, never signed into the /3 log",
    );
    assert!(
        is_projected(&conn, &a),
        "the authorable source node is unaffected — only its oversized edge is quarantined",
    );
    // The write path stays LIVE: a fresh mutation still succeeds instead of wedging.
    let live = create_concept(&conn, "still alive").unwrap().memory.memory_id;
    assert!(is_projected(&conn, &live), "other writes stay live despite the poison edge");
}

#[test]
fn an_oversized_ghost_edge_can_be_recovered_via_the_public_api() {
    // #680: because the write path stays live, the quarantined edge is RECOVERABLE — an edge
    // has no "shrink" (its anchor is its identity), so recovery is `remove_edge`
    // (memory_edge_remove): it deletes the un-authorable ghost by its short, hashed `edge_key`
    // (unaffected by the oversized anchor) and authors a tiny EdgeRemove, leaving the reconcile
    // with nothing un-authorable.
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id;
    let oversized_anchor = "x".repeat(300 * 1024);
    let key = rag_rat_query::memory::edge_key(&a, "relates_to", "node", &oversized_anchor);
    insert_raw_node_edge(&conn, &a, "relates_to", &oversized_anchor);

    // Remove it through the public API — the mutation the wedge would have blocked now
    // succeeds.
    assert!(
        crate::memory_write::remove_edge(&conn, &key).unwrap(),
        "the oversized ghost edge is removed through the public API",
    );
    let remaining: i64 =
        conn.query_row("SELECT COUNT(*) FROM repo_node_edges", [], |r| r.get(0)).unwrap();
    assert_eq!(remaining, 0, "the un-authorable edge row is gone after recovery");
    // The write path is clean: the next reconcile has nothing to quarantine.
    let live = create_concept(&conn, "next").unwrap().memory.memory_id;
    assert!(is_projected(&conn, &live), "the write path is clean after recovery");
}

/// A `MakeWriter` that appends every formatted log line into a shared buffer, so a test can
/// assert on emitted `tracing` events (`rag_rat_base::logging` uses the same `with_writer`
/// shape).
#[derive(Clone)]
struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn the_fast_path_warns_on_the_quarantined_row_it_skips() {
    // #680 (P2a): once ownership is established and the ONLY pending row is un-authorable, the
    // reconcile takes the lock-free fast path and early-returns without authoring. It must
    // still emit the per-row quarantine warning from that path — otherwise the
    // oversized row is silently skipped on every reconcile, defeating the
    // actionable-warning contract that replaced the old fail-loud wedge. Before the fix
    // the fast path returned with no warning.
    let conn = scoped_conn();
    create_concept(&conn, "seed").unwrap(); // establishes ownership → the fast path is reachable
    let oversized = "x".repeat(300 * 1024);
    insert_memory_with_body(&conn, "mem_big", &oversized); // the only pending row, un-authorable

    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(CaptureWriter(std::sync::Arc::clone(&buf)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        // Ownership established + nothing AUTHORABLE missing ⇒ the fast path handles this.
        backfill_memory_oplog(&conn, 2_000).unwrap();
    });

    let logged = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        logged.contains("quarantining an un-authorable memory row"),
        "the fast path emitted the per-row quarantine warning; got: {logged:?}",
    );
    assert!(logged.contains("mem_big"), "the warning names the skipped memory id; got: {logged:?}",);
    // The row stays unprojected — the warning is emitted IN PLACE OF authoring it, not
    // alongside.
    assert!(!is_projected(&conn, "mem_big"), "the quarantined row is still skipped, not signed");
}

#[test]
fn the_fast_path_warns_on_the_quarantined_edge_it_skips() {
    // #680 (P2a, edge twin): once ownership is established and the ONLY pending row is an
    // un-authorable EDGE, the reconcile takes the lock-free fast path and early-returns without
    // authoring. It must STILL emit the per-edge quarantine warning from that path — otherwise
    // the oversized edge is silently skipped on every reconcile, with no signal at all.
    let conn = scoped_conn();
    let a = create_concept(&conn, "a").unwrap().memory.memory_id; // establishes ownership
    let oversized_anchor = "x".repeat(300 * 1024);
    insert_raw_node_edge(&conn, &a, "relates_to", &oversized_anchor); // the only pending row

    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(CaptureWriter(std::sync::Arc::clone(&buf)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        // Ownership established + nothing AUTHORABLE missing ⇒ the fast path handles this.
        backfill_memory_oplog(&conn, 2_000).unwrap();
    });

    let logged = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        logged.contains("quarantining an un-authorable node-edge"),
        "the fast path emitted the per-edge quarantine warning; got: {logged:?}",
    );
    assert_eq!(projected_edge_count(&conn), 0, "the quarantined edge is still skipped, not signed",);
}

// --- read path is unchanged by the retarget (#665) ---

#[test]
fn local_reads_come_from_repo_memories_not_the_v3_stream() {
    // The retarget moved AUTHORING onto /3, but local reads must be unchanged: they come from
    // repo_memories, never the /3 stream or its content_projected_* shadow (whose only consumer
    // is the reconcile's completeness anti-join). Prove it: author via the live path, then WIPE
    // the entire /3 substrate and confirm a read returns the same memories. A read that
    // consulted any /3 table would change here.
    let conn = scoped_conn();
    let created = create_concept(&conn, "readable").unwrap().memory.memory_id;

    let read_ids = |c: &Connection| -> Vec<String> {
        rag_rat_query::memory::list_memories(c, None)
            .unwrap()
            .into_iter()
            .map(|m| m.memory_id)
            .collect()
    };
    let before = read_ids(&conn);
    assert!(before.contains(&created), "the authored memory lists before the wipe");

    // Wipe every /3 table the live path writes (FKs off so delete order is irrelevant).
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
             DELETE FROM content_projected_nodes;
             DELETE FROM content_projected_edges;
             DELETE FROM content_entry_status;
             DELETE FROM content_entries;
             PRAGMA foreign_keys = ON;",
    )
    .unwrap();

    assert_eq!(
        read_ids(&conn),
        before,
        "reads are identical with the whole /3 substrate wiped — they never consult it",
    );
}
