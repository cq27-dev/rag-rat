use std::fs;
use std::path::PathBuf;

use rag_rat_base::config::Config;
use rusqlite::{Connection, OptionalExtension, params};

use super::super::MEMORY_STREAM_ACCESS_MODE_META_KEY;
use super::*;
use crate::index::{IndexDatabase, schema};

/// A source legacy DB (current schema) seeded with 3 memories — one carrying a binding whose
/// LOCAL rowid columns are set alongside FULL relocation provenance (symbol_kind /
/// signature_hash / moniker trio), a tag, a call-path + edge — plus 2 embedding-cache rows and
/// the model-identity meta keys.
fn seeded_source() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    for (id, title) in [("m1", "one"), ("m2", "two"), ("m3", "three")] {
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id)
                 VALUES (?1, 'Invariant', ?2, 'body', 'high', 'active', 0, 0, 'agent', 'v1', \
             'legacy-repo')",
            params![id, title],
        )
        .unwrap();
    }
    // #465: m1 carries a polymorphic payload — it must survive import verbatim, not be NULLed.
    conn.execute(r#"UPDATE repo_memories SET payload_json = '{"priority":1}' WHERE id = 'm1'"#, [])
        .unwrap();
    // A binding on m1 with the LOCAL rowid columns populated (must be NULLed on import) and
    // EVERY portable field set (must survive verbatim), including the moniker relocation
    // provenance.
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, start_line, \
         end_line, logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, tracker, \
         project, item_key, anchor_status, created_at_ms, symbol_kind, signature_hash, \
         moniker_tool, moniker_tool_version, relocation_reason, repo_id, resolved, \
         resolved_binding_id, resolved_path, resolved_start_line, resolved_end_line, \
         resolved_symbol_kind, resolved_signature_hash, resolved_moniker_tool_version)
             VALUES ('m1', 'path', 'b1', 'src/x.rs', 10, 20, 111, 222, 333, 444, 'abc', 'github', \
         'o/r', '7', 'current', 0, 'function', 'sighash', 'scip-rust', '0.4', 'moved', \
         'legacy-repo', 1, 'b1-resolved', 'src/y.rs', 11, 21, 'method', NULL, '0.5')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('m1', 'tagalpha')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, start_logical_symbol_id, \
         end_logical_symbol_id, edge_sequence_hash, path_summary, created_at_ms)
             VALUES ('m1', 555, 666, 'h1', 'a -> b', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, to_name, edge_kind) VALUES ('m1', 'h1', 0, 'fp', 'b', 'calls')",
        [],
    )
    .unwrap();
    // #464: a node edge m1 --depends_on--> m2. Its `edge_key` is RECOMPUTED on import from the
    // (possibly remapped) endpoints, so the seed's placeholder key here is intentionally not
    // the real one.
    // Three edge shapes exercise every `copy_node_edges` branch on import: a node target that
    // IS carried (remapped, current), a github target (re-homed to the import repo, current),
    // and a node target that is NOT carried (kept as an `unresolved` cross-repo
    // reference).
    for (key, relation, target_repo, kind, anchor, node, status) in [
        ("seed-node", "depends_on", "legacy-repo", "node", "m2", "m2", "current"),
        ("seed-gh", "tracks", "legacy-repo", "github", "o/r#7", "", "current"),
        ("seed-ext", "relates_to", "other-repo", "node", "external-node", "", "unresolved"),
    ] {
        let node_id: Option<&str> = (!node.is_empty()).then_some(node);
        conn.execute(
            "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id, target_kind, target_anchor, target_node_id, anchor_status, \
             created_at_ms) VALUES (?1, 'legacy-repo', 'm1', ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            rusqlite::params![key, relation, target_repo, kind, anchor, node_id, status],
        )
        .unwrap();
    }
    for (hash, dim) in [("ih1", 384), ("ih2", 768)] {
        conn.execute(
            "INSERT INTO embedding_cache(input_hash, model_id, embedding_dim, vector_blob, \
             computed_at_ms, last_used_at_ms) VALUES (?1, 'model-a', ?2, X'00', 0, 0)",
            params![hash, dim],
        )
        .unwrap();
    }
    // Seed the meta under the placeholder (which always exists in `repos`, so the FK holds);
    // `copy_model_state` reads it by KEY regardless of the source's repo_id. Keys from both
    // classification classes: three PORTABLE keys (identity, remote config, and the
    // provisional flag — an auto-picked model, "1") and one DB-LOCAL freshness cursor (must
    // NOT copy).
    for (key, value) in [
        ("active_embedding_model", "model-a"),
        ("active_embedding_remote_config", "{\"endpoint\":\"http://ollama:11434\"}"),
        ("active_embedding_model_provisional", "1"),
        ("git_commit", "cursor-sha"),
    ] {
        conn.execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', ?1, ?2)",
            params![key, value],
        )
        .unwrap();
    }
    // The active model's READINESS row — part of the model-state unit `copy_model_state`
    // carries (an identity pointing at a MissingModel/absent row leaves active_embedder
    // refusing despite the carried cache).
    conn.execute(
        "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, disabled, \
         status, installed_at_ms) VALUES ('model-a', 'embedding', 384, 'local', 1, 0, 'Ready', 7)",
        [],
    )
    .unwrap();
    conn
}

fn fresh_target() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    // The target must hold the repo `import_from_source` stamps: `repo_meta.repo_id` has a FK
    // to `repos` (in production `register_repo` creates this row before the import runs).
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('global-repo', 'g', 0)",
        [],
    )
    .unwrap();
    conn
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// A MULTI-repo source (what a live global store looks like) for the seed path: repo
/// `global-repo` holds two local memories, one peer-`synced` memory, and — on `pm1` — one local
/// and one synced edge; repo `other-repo` holds an unrelated local memory that must NEVER reach
/// a public node. Plus machine-local model meta + embedding cache (must NOT be carried).
fn seed_source_multi() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    for (id, repo, origin) in [
        ("pm1", "global-repo", "local"),
        ("pm2", "global-repo", "local"),
        ("ps1", "global-repo", "synced"),
        ("om1", "other-repo", "local"),
    ] {
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, origin, repo_id)
                 VALUES (?1, 'Invariant', ?1, 'body', 'high', 'active', 0, 0, 'agent', 'v1', ?2, \
             ?3)",
            params![id, origin, repo],
        )
        .unwrap();
    }
    // pm1's edges span every seed-relevant shape: a local github ref (kept), a synced github
    // ref (dropped by origin), a same-repo node edge (kept, resolves through id_map), and a
    // cross-repo node edge into `other-repo` (dropped by seed — it would leak that repo).
    // (key, relation, target_repo, target_kind, target_anchor, origin)
    for (key, relation, target_repo, target_kind, target_anchor, origin) in [
        ("e-gh-local", "tracks", "global-repo", "github", "o/r#7", "local"),
        ("e-gh-synced", "relates_to", "global-repo", "github", "o/r#8", "synced"),
        ("e-node-same", "depends_on", "global-repo", "node", "pm2", "local"),
        ("e-node-xrepo", "supersedes", "other-repo", "node", "om1", "local"),
    ] {
        conn.execute(
            "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id, target_kind, target_anchor, anchor_status, created_at_ms, origin)
                 VALUES (?1, 'global-repo', 'pm1', ?2, ?3, ?4, ?5, 'current', 0, ?6)",
            params![key, relation, target_repo, target_kind, target_anchor, origin],
        )
        .unwrap();
    }
    for hash in ["ih1", "ih2"] {
        conn.execute(
            "INSERT INTO embedding_cache(input_hash, model_id, embedding_dim, vector_blob, \
             computed_at_ms, last_used_at_ms) VALUES (?1, 'model-a', 384, X'00', 0, 0)",
            params![hash],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', \
         'active_embedding_model', 'model-a')",
        [],
    )
    .unwrap();
    conn
}

#[test]
fn seed_imports_only_this_repos_local_memories() {
    let source = seed_source_multi();
    let target = fresh_target();
    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::SeedPublic).unwrap();
    // Only global-repo's two LOCAL memories: the synced one and other-repo's are excluded.
    assert_eq!(counts.memories, 2);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 2);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories WHERE id IN ('pm1','pm2')"), 2,);
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_memories WHERE id IN ('ps1','om1')"),
        0,
        "peer-synced and other-repo memories never reach a public node",
    );
}

#[test]
fn seed_keeps_only_resolvable_local_edges() {
    let source = seed_source_multi();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::SeedPublic).unwrap();
    // Kept: the local github ref (`tracks`) and the same-repo node edge (`depends_on`).
    // Dropped: the synced github ref (`relates_to`, origin) and the cross-repo node edge
    // (`supersedes`, whose target lives in another private repo — carrying it would leak that
    // repo's id onto the public op-log).
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_node_edges"), 2);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_node_edges WHERE relation='tracks'"), 1);
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_node_edges WHERE relation='depends_on'"),
        1,
    );
    assert_eq!(
        count(
            &target,
            "SELECT COUNT(*) FROM repo_node_edges WHERE relation IN ('relates_to','supersedes')",
        ),
        0,
    );
    // No other private repo's id reaches the public node through an edge target.
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_node_edges WHERE target_repo_id='other-repo'"),
        0,
        "a cross-repo node edge target never leaks another repo onto a public node",
    );
}

#[test]
fn seed_carries_no_machine_state() {
    let source = seed_source_multi();
    let target = fresh_target();
    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::SeedPublic).unwrap();
    assert_eq!(counts.embedding_cache_rows, 0, "the embedding cache is not carried");
    assert_eq!(counts.meta_keys, 0, "model-state meta is not carried");
    assert_eq!(count(&target, "SELECT COUNT(*) FROM embedding_cache"), 0);
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_meta WHERE key='active_embedding_model'"),
        0,
        "the source machine's embedder identity does not follow onto the public node",
    );
}

#[test]
fn consolidate_imports_every_repo_but_only_local_rows() {
    // Guards against the seed filters regressing the legacy path: no repo filter, machine
    // state carried. (Consolidate's real source is single-repo; this reuses the multi-repo
    // fixture only to prove every repo's rows are taken.)
    let source = seed_source_multi();
    let target = fresh_target();
    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(counts.memories, 3, "all repos, local rows only");
    assert_eq!(counts.edges, 3, "local edges: github + node, cross-repo included");
    assert_eq!(counts.embedding_cache_rows, 2, "cache carried");
    assert_eq!(counts.meta_keys, 1, "model-state meta carried");
}

/// A legacy store holds memories as `origin='synced'` rows when another device materialized
/// them. Consolidation carries the ones the source's own account created — they are its own
/// work, local edits included — and leaves those another account created, so the reconcile
/// never signs another account's memory or edge as the target's (#1284). A local edge onto a
/// left-out memory survives, re-homed under the new repo identity and unresolved until that
/// memory arrives through sync.
#[test]
fn consolidation_carries_only_what_the_source_account_created() {
    use rag_rat_oplog::{EdgeSpec, MemoryOp, NodeContent, NodeId, SealPolicy};
    let source = seed_source_multi();
    // The source's own account created `sib1` and its edge on another device.
    rag_rat_oplog::local_account(&source, 0).unwrap();
    let stream = {
        let tx =
            rusqlite::Transaction::new_unchecked(&source, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
        let stream = rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "global-repo", 0).unwrap();
        tx.commit().unwrap();
        stream
    };
    let sibling_edge = EdgeSpec {
        source_node_id: NodeId::from("sib1"),
        relation: rag_rat_query::memory::EdgeRelation::RelatesTo,
        target_repo_id: "global-repo".to_string(),
        target_kind: "github".to_string(),
        target_anchor: "o/r#9".to_string(),
        owner_repo_id: "global-repo".to_string(),
    };
    rag_rat_oplog::author_content_batch(
        &source,
        stream,
        &[
            MemoryOp::NodeCreate {
                node_id: NodeId::from("sib1"),
                content: NodeContent {
                    kind: "Invariant".into(),
                    title: "sib1".into(),
                    body: "body".into(),
                    confidence: "high".into(),
                    source: "agent".into(),
                    tags: Vec::new(),
                    payload: None,
                },
            },
            MemoryOp::EdgeAdd { edge: sibling_edge.clone() },
        ],
        SealPolicy::Plaintext,
        0,
    )
    .unwrap();
    source
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, origin, repo_id)
                 VALUES ('sib1', 'Invariant', 'sib1 edited here', 'body', 'high', 'active', 0, 0, \
             'agent', 'v1', 'synced', 'global-repo')",
            [],
        )
        .unwrap();
    source
        .execute(
            "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id, target_kind, target_anchor, anchor_status, created_at_ms, origin)
                 VALUES (?1, 'global-repo', 'sib1', 'relates_to', 'global-repo', 'github', \
             'o/r#9', 'current', 0, 'synced')",
            [sibling_edge.edge_key().as_str()],
        )
        .unwrap();
    // A local edge onto `ps1`, which another account created.
    source
        .execute(
            "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id, target_kind, target_anchor, anchor_status, created_at_ms, origin)
                 VALUES ('e-onto-synced', '__unassigned__', 'pm2', 'relates_to', '__unassigned__', \
             'node', 'ps1', 'current', 0, 'local')",
            [],
        )
        .unwrap();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    crate::memory_write::reconcile_owner_stream_for_repo(
        &target,
        "global-repo",
        rag_rat_base::time::now_ms(),
    )
    .unwrap();

    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories WHERE id = 'ps1'"), 0);
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'ps1'"),
        0
    );
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_node_edges WHERE target_anchor = 'o/r#8'"),
        0,
        "another account's edge onto a local memory is not re-authored as the target's",
    );
    assert_eq!(
        count(
            &target,
            "SELECT COUNT(*) FROM repo_memories WHERE id = 'sib1' AND title = 'sib1 edited here'"
        ),
        1,
        "the source account's own synced memory is carried with its local edit",
    );
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_node_edges WHERE target_anchor = 'o/r#9'"),
        1,
        "and so is the edge the source account added",
    );
    for node in ["pm1", "sib1"] {
        assert_eq!(
            count(
                &target,
                &format!("SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = '{node}'")
            ),
            1,
            "{node} is reconciled as the target's own",
        );
    }
    let edge: (String, String) = target
        .query_row(
            "SELECT target_repo_id, anchor_status FROM repo_node_edges
                 WHERE source_node_id = 'pm2' AND target_anchor = 'ps1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(edge, ("global-repo".to_string(), "unresolved".to_string()));
}

/// A legacy source on a sealed stream, whose own account created `pm2` as a sealed entry that
/// another device of that account would have left as a synced row.
fn sealed_source_with_own_synced_memory() -> Connection {
    let source = seed_source_multi();
    source
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms)
                 VALUES ('global-repo', 'global-repo', 0)",
            [],
        )
        .unwrap();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('global-repo', ?1, 'sealed')",
            [MEMORY_STREAM_SEAL_POLICY_META_KEY],
        )
        .unwrap();
    rag_rat_oplog::local_account(&source, 0).unwrap();
    crate::memory_write::reconcile_owner_stream_for_repo(
        &source,
        "global-repo",
        rag_rat_base::time::now_ms(),
    )
    .unwrap();
    source.execute("UPDATE repo_memories SET origin = 'synced' WHERE id = 'pm2'", []).unwrap();
    source
}

/// The ownership scan opens the source account's sealed entries with its own keyring, so a
/// synced memory that account created on a sealed stream is carried (#1284).
#[test]
fn consolidation_reads_the_source_accounts_sealed_entries() {
    let source = sealed_source_with_own_synced_memory();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories WHERE id = 'pm2'"), 1);
}

/// A source that cannot open its own account's sealed entries cannot tell which synced rows
/// are its own, so the import refuses instead of leaving them behind.
#[test]
fn consolidation_refuses_a_source_whose_own_sealed_entries_it_cannot_open() {
    let source = sealed_source_with_own_synced_memory();
    source.execute("DELETE FROM oplog_device_identity", []).unwrap();
    let target = fresh_target();
    let Err(err) =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy)
    else {
        panic!("the import refuses a source whose own sealed entries it cannot open");
    };
    assert!(err.to_string().contains("sealed"), "{err}");
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 0, "nothing was imported");
}

/// An own-account entry the source cannot even decode hides what that account created, the
/// same as a sealed one it cannot open, so the import refuses.
#[test]
fn consolidation_refuses_a_source_with_an_undecodable_own_entry() {
    let source = seed_source_multi();
    let own = rag_rat_oplog::local_account(&source, 0).unwrap();
    source
        .execute(
            "INSERT INTO content_entries(
                     entry_hash, stream_id, author_account_id, device_fingerprint, seq,
                     prev_hash, grant_id, roster_ref, owner_auth_len, author_auth_len,
                     accepted, signed_bytes, received_at_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?5, ?5, 1, x'00', 0)",
            params![
                [0x31_u8; 32].as_slice(),
                [0x41_u8; 32].as_slice(),
                own.to_bytes().as_slice(),
                [0x12_u8; 32].as_slice(),
                0_u64.to_be_bytes().as_slice(),
                [0x13_u8; 32].as_slice(),
            ],
        )
        .unwrap();
    let target = fresh_target();
    let Err(err) =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy)
    else {
        panic!("the import refuses a source with an undecodable own entry");
    };
    assert!(err.to_string().contains("cannot read"), "{err}");
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 0, "nothing was imported");
}

/// Work the source's own account did on a memory another account created cannot be carried —
/// its edge from that memory would be dropped with it — so the import refuses (#1284).
#[test]
fn consolidation_refuses_to_drop_the_source_accounts_work_on_a_foreign_memory() {
    use rag_rat_oplog::{EdgeSpec, MemoryOp, NodeId, SealPolicy};
    for signed in [true, false] {
        let source = seed_source_multi();
        rag_rat_oplog::local_account(&source, 0).unwrap();
        if signed {
            // An edge this account added from `ps1`, which another account created.
            let stream = {
                let tx = rusqlite::Transaction::new_unchecked(
                    &source,
                    rusqlite::TransactionBehavior::Immediate,
                )
                .unwrap();
                let stream =
                    rag_rat_oplog::ensure_owned_stream_v2_in_tx(&tx, "global-repo", 0).unwrap();
                tx.commit().unwrap();
                stream
            };
            rag_rat_oplog::author_content_batch(
                &source,
                stream,
                &[MemoryOp::EdgeAdd {
                    edge: EdgeSpec {
                        source_node_id: NodeId::from("ps1"),
                        relation: rag_rat_query::memory::EdgeRelation::RelatesTo,
                        target_repo_id: "global-repo".to_string(),
                        target_kind: "github".to_string(),
                        target_anchor: "o/r#10".to_string(),
                        owner_repo_id: "global-repo".to_string(),
                    },
                }],
                SealPolicy::Plaintext,
                0,
            )
            .unwrap();
        } else {
            // The same edge added here and not signed yet.
            source
                .execute(
                    "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
                     target_repo_id, target_kind, target_anchor, anchor_status, created_at_ms, \
                     origin)
                         VALUES ('e-from-foreign', 'global-repo', 'ps1', 'relates_to', \
                     'global-repo', 'github', 'o/r#10', 'current', 0, 'local')",
                    [],
                )
                .unwrap();
        }
        let target = fresh_target();
        let Err(err) =
            import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy)
        else {
            panic!("signed={signed}: the import refuses to drop the account's own work");
        };
        assert!(err.to_string().contains("another account created"), "{err}");
        assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 0);
    }
}

/// Seeding carries no synced row and archives nothing, so the legacy stranded-work guard does
/// not apply: a local edge from a synced memory in the shared source must not block a seed.
#[test]
fn seed_is_not_blocked_by_local_work_on_synced_memories() {
    let source = seed_source_multi();
    source
        .execute(
            "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id, target_kind, target_anchor, anchor_status, created_at_ms, origin)
                 VALUES ('e-from-synced', 'global-repo', 'ps1', 'relates_to', 'global-repo', \
             'github', 'o/r#11', 'current', 0, 'local')",
            [],
        )
        .unwrap();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::SeedPublic).unwrap();
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_memories"),
        2,
        "the seed still takes this repo's local memories",
    );
}

#[test]
fn ensure_source_unsealed_refuses_a_sealed_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('r', ?1, 'sealed')",
            params![MEMORY_STREAM_SEAL_POLICY_META_KEY],
        )
        .unwrap();
    }
    let err = ensure_source_unsealed(&path, "r").unwrap_err().to_string();
    assert!(err.contains("sealed"), "sealed source is refused: {err}");
}

#[test]
fn ensure_source_unsealed_allows_a_plaintext_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
            [],
        )
        .unwrap();
    }
    ensure_source_unsealed(&path, "r").unwrap();
}

#[test]
fn import_stamps_the_new_repo_id_and_nulls_local_rowids() {
    let source = seeded_source();
    let target = fresh_target();
    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();

    assert_eq!(counts.memories, 3);
    assert_eq!(counts.bindings, 1);
    assert_eq!(counts.tags, 1);
    assert_eq!(counts.call_paths, 1);
    assert_eq!(counts.call_path_edges, 1);
    assert_eq!(counts.embedding_cache_rows, 2);
    assert_eq!(
        counts.meta_keys, 3,
        "the portable model-state keys carried (identity + remote config + provisional)",
    );

    // Every memory + binding is stamped the NEW repo id, never the legacy one.
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories WHERE repo_id='global-repo'"), 3);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories WHERE repo_id='legacy-repo'"), 0);
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_memory_bindings WHERE repo_id='global-repo'"),
        1,
    );

    // The binding's LOCAL rowid columns are all NULLed (re-resolved by the validate loop).
    let nulled = count(
        &target,
        "SELECT COUNT(*) FROM repo_memory_bindings WHERE memory_id='m1' AND logical_symbol_id IS \
         NULL AND symbol_id IS NULL AND chunk_id IS NULL AND edge_id IS NULL",
    );
    assert_eq!(nulled, 1, "local rowids nulled for re-resolution");
    // Its portable fields survive.
    let (path, commit, key): (Option<String>, Option<String>, Option<String>) = target
        .query_row(
            "SELECT path, commit_hash, item_key FROM repo_memory_bindings WHERE memory_id='m1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(path.as_deref(), Some("src/x.rs"), "portable path survives");
    assert_eq!(commit.as_deref(), Some("abc"));
    assert_eq!(key.as_deref(), Some("7"));

    // Call-path logical ids are NULLed too; the edge is copied verbatim.
    let (start, end): (Option<i64>, Option<i64>) = target
        .query_row(
            "SELECT start_logical_symbol_id, end_logical_symbol_id FROM repo_memory_call_paths \
             WHERE memory_id='m1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((start, end), (None, None));

    // Both content-addressed cache rows landed, and the model meta key was carried.
    assert_eq!(count(&target, "SELECT COUNT(*) FROM embedding_cache"), 2);
    assert_eq!(
        count(
            &target,
            "SELECT COUNT(*) FROM repo_meta WHERE repo_id='global-repo' AND \
             key='active_embedding_model'"
        ),
        1,
    );
}

#[test]
fn import_rekeys_call_path_identity_for_the_destination_repo() {
    use rag_rat_base::config::{ResolvedTarget, TargetKind};
    use rag_rat_base::language::Language;
    use rag_rat_base::test_scratch::{self, ScratchDir};

    let root = ScratchDir::new("consolidate-call-path");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn callee() {}\npub fn caller() { callee(); }\n")
        .unwrap();
    let config_root = test_scratch::canonical_config_root(root.path());
    let config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        mcp: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    let source = IndexDatabase::rebuild(&config).unwrap();
    let edge_id: i64 = source
        .storage
        .connection()
        .query_row(
            "SELECT id FROM edges WHERE to_name = 'callee' AND to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let memory_id = source
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Consolidated call path".to_string(),
            body: "Its callee identity is repo-derived.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                edge_path: Some(vec![edge_id]),
                ..Default::default()
            },
        })
        .unwrap()
        .memory
        .memory_id;
    let (source_callee, source_fingerprint, source_hash): (i64, String, String) = source
        .storage
        .connection()
        .query_row(
            "SELECT callee_logical_symbol_id, edge_fingerprint, edge_sequence_hash
                   FROM repo_memory_call_path_edges WHERE memory_id = ?1",
            [&memory_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    let target = fresh_target();
    import_from_source(
        source.storage.connection(),
        &target,
        "global-repo",
        ImportMode::ConsolidateLegacy,
    )
    .unwrap();
    let (target_callee, target_fingerprint, edge_hash, path_hash, binding_hash): (
        i64,
        String,
        String,
        String,
        String,
    ) = target
        .query_row(
            "SELECT e.callee_logical_symbol_id, e.edge_fingerprint, e.edge_sequence_hash,
                        p.edge_sequence_hash, IIF(b.resolved, b.resolved_binding_id, b.binding_id)
                   FROM repo_memory_call_path_edges e
                   JOIN repo_memory_call_paths p ON p.memory_id = e.memory_id
                   JOIN repo_memory_bindings b ON b.memory_id = e.memory_id
                      AND b.binding_kind = 'call_path'
                  WHERE e.memory_id = ?1",
            [&memory_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert_ne!(target_callee, source_callee, "the destination repo derives a distinct callee");
    assert_ne!(target_fingerprint, source_fingerprint, "the imported fingerprint is re-derived");
    assert_ne!(edge_hash, source_hash, "the sequence hash follows the new fingerprint");
    assert_eq!(edge_hash, path_hash);
    assert_eq!(path_hash, binding_hash);
}

#[test]
fn consolidation_declines_a_non_unanimous_legacy_logical_group() {
    let source = Connection::open_in_memory().unwrap();
    schema::apply(&source, &crate::index::migration_hooks()).unwrap();
    source
        .execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms,
                        indexed_at_ms, commit_sha, worktree_id, repo_id, generation)
                 VALUES ('src/lib.rs', 'rust', 'source', 'sha', 0, 0, '', '',
                         '__unassigned__', 0)",
            [],
        )
        .unwrap();
    source.execute("INSERT INTO name_strings(value) VALUES ('src/lib.rs::run')", []).unwrap();
    let qualified_name_id = source.last_insert_rowid();
    for scope_path in ["Alpha::run", "Beta::run"] {
        source
            .execute(
                "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind,
                            start_byte, end_byte, signature, scope_path)
                     VALUES (1, 'rust', 'run', ?1, 'function', 0, 1, 'fn run()', ?2)",
                params![qualified_name_id, scope_path],
            )
            .unwrap();
    }
    source
        .execute(
            "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id,
                        kind, variant_count, group_reason, repo_id)
                 VALUES (91, 'rust', 'src/lib.rs', 'run', ?1, 'function', 2, 'legacy',
                         '__unassigned__')",
            [qualified_name_id],
        )
        .unwrap();
    source
        .execute_batch(
            "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line,
                        end_line) VALUES (91, 1, 1, 1);
                 INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line,
                        end_line) VALUES (91, 2, 1, 1);
                 INSERT INTO repo_memories(id, kind, title, body, confidence, status,
                        created_at_ms, updated_at_ms, source, memory_version, repo_id)
                 VALUES ('m', 'Invariant', 't', 'b', 'high', 'active', 0, 0, 'agent', 'v1',
                         '__unassigned__');
                 INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal,
                        edge_fingerprint, to_name, edge_kind, callee_logical_symbol_id,
                        callee_identity_known)
                 VALUES ('m', 'h', 0, 'fp', 'run', 'calls_name', 91, 1);",
        )
        .unwrap();

    assert_eq!(consolidation_callee_remap(&source, "destination").unwrap(), vec![(91, None)]);
}

/// The relocation-provenance columns (`symbol_kind`, `signature_hash`, `moniker_tool`,
/// `moniker_tool_version`, `relocation_reason`) and the source's resolution (`resolved` and
/// the `resolved_*` shadows) survive the import verbatim — dropping them
/// would strip imported `scip_moniker` bindings of validation (`unverified` without
/// `moniker_tool`) and of the oracle-backed relocation path (which requires both tool fields).
#[test]
fn import_preserves_moniker_binding_provenance() {
    let source = seeded_source();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();

    let provenance = |column: &str| -> Option<String> {
        target
            .query_row(
                &format!("SELECT {column} FROM repo_memory_bindings WHERE memory_id='m1'"),
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(provenance("symbol_kind").as_deref(), Some("function"));
    assert_eq!(provenance("signature_hash").as_deref(), Some("sighash"));
    assert_eq!(provenance("moniker_tool").as_deref(), Some("scip-rust"));
    assert_eq!(provenance("moniker_tool_version").as_deref(), Some("0.4"));
    assert_eq!(provenance("relocation_reason").as_deref(), Some("moved"));
    // The source's resolution comes along too: the import takes the source's local call-path
    // tables wholesale, keyed by the hash the source resolved, so the rows must keep resolving
    // to them (#1297). A cleared shadow stays cleared.
    assert_eq!(provenance("CAST(resolved AS TEXT)").as_deref(), Some("1"));
    assert_eq!(provenance("resolved_binding_id").as_deref(), Some("b1-resolved"));
    assert_eq!(provenance("resolved_path").as_deref(), Some("src/y.rs"));
    assert_eq!(provenance("CAST(resolved_start_line AS TEXT)").as_deref(), Some("11"));
    assert_eq!(provenance("resolved_symbol_kind").as_deref(), Some("method"));
    assert_eq!(provenance("resolved_signature_hash"), None);
    assert_eq!(provenance("resolved_moniker_tool_version").as_deref(), Some("0.5"));
}

/// Imported memories are reachable through KEYWORD search: `memory_search` retrieves
/// exclusively through the `repo_memory_fts` mirror, whose only other writers are
/// `upsert_memory_fts` (create/update) and the one-time V042 rebuild — so the import must
/// re-derive it, or the whole imported corpus stays invisible to search forever.
#[test]
fn imported_memories_are_findable_via_memory_fts() {
    let source = seeded_source();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();

    let hits: i64 = target
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_memory_fts MATCH 'three' AND repo_id \
             = 'global-repo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "an imported memory matches by its title words");
    // The tag derivation matches upsert_memory_fts (space-joined), so tag words match too.
    let tag_hits: i64 = target
        .query_row(
            "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_memory_fts MATCH 'tagalpha' AND \
             repo_id = 'global-repo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tag_hits, 1, "an imported memory matches by its tag");
}

#[test]
fn import_is_idempotent_and_reports_honest_counts() {
    let source = seeded_source();
    let target = fresh_target();
    let first =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(first.memories, 3);
    assert_eq!(first.edges, 3, "all three edges (node, github, cross-repo) were carried");
    // A second import (a retry after a rename that never happened) inserts no duplicates AND
    // reports ZERO copies — the summary must not claim work the `INSERT OR IGNORE`s skipped.
    let second =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(second.memories, 0, "re-run reports zero memories copied");
    assert_eq!(second.bindings, 0);
    assert_eq!(second.tags, 0);
    assert_eq!(second.call_paths, 0);
    assert_eq!(second.call_path_edges, 0);
    assert_eq!(second.edges, 0, "a no-edit re-import reports zero edges (honest count)");
    assert_eq!(second.embedding_cache_rows, 0);
    assert_eq!(second.meta_keys, 0);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 3);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memory_bindings"), 1);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM embedding_cache"), 2);
    // The FTS mirror re-derivation is convergent too — one row per memory, not accumulated.
    assert_eq!(
        count(&target, "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_id='global-repo'"),
        3
    );
    // Zero-diff content: the no-edit retry converged — the target rows still carry the
    // source content verbatim (the gated upsert wrote NOTHING, it didn't rewrite in place).
    let (title, body, payload): (String, String, Option<String>) = target
        .query_row("SELECT title, body, payload_json FROM repo_memories WHERE id='m1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!((title.as_str(), body.as_str()), ("one", "body"));
    // #465: the payload was carried through the consolidation upsert (not turned into NULL).
    assert_eq!(payload.as_deref(), Some(r#"{"priority":1}"#), "m1 payload survives import");
    // #464: all three edges carried under the global repo — a node edge to a carried target
    // (current), a github `tracks` edge re-homed to the import repo (current), and a node edge
    // whose target was NOT carried (kept `unresolved`).
    let edge = |kind: &str, anchor: &str| -> (String, String) {
        target
            .query_row(
                "SELECT anchor_status, target_repo_id FROM repo_node_edges WHERE repo_id = \
                 'global-repo' AND target_kind = ?1 AND target_anchor = ?2",
                [kind, anchor],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    assert_eq!(edge("node", "m2").0, "current", "node edge to a carried target");
    assert_eq!(edge("github", "o/r#7"), ("current".to_string(), "global-repo".to_string()));
    assert_eq!(
        edge("node", "external-node").0,
        "unresolved",
        "target not carried stays unresolved"
    );
}

/// The CRASH-CREATED divergence window (Codex batch 8, finding 4): the import txn commits,
/// the RENAME fails, the legacy file stays the LIVE store (keyless resolution keeps serving
/// it), and the user edits a memory there. The retry must carry those edits into the global
/// store — content upserted, children REPLACED (a tag removed legacy-side must not survive by
/// union) — because the legacy always wins until the rename lands. Counts stay honest (only
/// the actually-refreshed memory reports), and a further no-edit retry is a true no-op.
#[test]
fn a_retry_after_a_failed_rename_carries_legacy_edits_made_in_the_window() {
    let source = seeded_source();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    // ... the rename fails here; the legacy DB stays live and the user edits m1: new body,
    // tag set REPLACED (alpha removed, window added), binding re-anchored. Every authored
    // mutation path bumps `updated_at_ms` (update_memory / rebind_memory), mirrored here.
    source
        .execute(
            "UPDATE repo_memories SET body='edited in the window', updated_at_ms=99 WHERE id='m1'",
            [],
        )
        .unwrap();
    source.execute("DELETE FROM repo_memory_tags WHERE memory_id='m1'", []).unwrap();
    source
        .execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('m1','window-tag')", [])
        .unwrap();
    source
        .execute("UPDATE repo_memory_bindings SET path='src/y.rs' WHERE memory_id='m1'", [])
        .unwrap();

    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(counts.memories, 1, "only the edited memory reports as written");
    assert_eq!(counts.tags, 1, "the replaced tag set reports its reinserted row");
    assert_eq!(counts.bindings, 1);

    let body: String =
        target.query_row("SELECT body FROM repo_memories WHERE id='m1'", [], |r| r.get(0)).unwrap();
    assert_eq!(body, "edited in the window", "the legacy edit wins in the global store");
    let tags: Vec<String> = target
        .prepare("SELECT tag FROM repo_memory_tags WHERE memory_id='m1' ORDER BY tag")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(tags, ["window-tag"], "children are REPLACED — the removed tag is gone");
    let path: String = target
        .query_row("SELECT path FROM repo_memory_bindings WHERE memory_id='m1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(path, "src/y.rs");
    // The FTS mirror carries the refreshed body — the edit is immediately searchable.
    assert_eq!(
        count(
            &target,
            "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_id='global-repo' AND repo_memory_fts \
             MATCH 'window'"
        ),
        1
    );
    // Untouched memories reported nothing and kept their rows: convergence.
    let third =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(third.memories, 0, "a further no-edit retry is a true no-op");
    assert_eq!(third.tags, 0);
    assert_eq!(third.bindings, 0);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 3);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memory_tags"), 1);
}

/// The mirror invariant covers REMAPPED parents too (Codex batch 9): a memory whose id
/// collided with another repo's row imports under the remapped id — a window edit to it
/// legacy-side must still refresh the remapped row AND replace its children on retry. The
/// batch-8 parent-edit gate missed exactly this (it tested the SOURCE id's owner, which stays
/// foreign); the unconditional child replace closes the shape. The foreign row stays
/// untouched throughout.
#[test]
fn a_retry_carries_window_edits_to_a_remapped_memory() {
    let source = seeded_source();
    let target = fresh_target();
    // The global store already owns "m1" under a different repo — the import remaps.
    target
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('other-repo', \
             'o', 0)",
            [],
        )
        .unwrap();
    target
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id)
                 VALUES ('m1', 'Risk', 'other title', 'other body', 'low', 'active', 0, 0, \
             'agent', 'v1', 'other-repo')",
            [],
        )
        .unwrap();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    let remapped = remapped_memory_id("global-repo", "m1");

    // ... rename fails; the user edits m1 in the still-live legacy DB: body + tag replaced.
    source
        .execute(
            "UPDATE repo_memories SET body='remapped window edit', updated_at_ms=99 WHERE id='m1'",
            [],
        )
        .unwrap();
    source.execute("DELETE FROM repo_memory_tags WHERE memory_id='m1'", []).unwrap();
    source
        .execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('m1','remap-tag')", [])
        .unwrap();

    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(counts.memories, 1, "the remapped parent's refresh reports honestly");

    let body: String = target
        .query_row("SELECT body FROM repo_memories WHERE id=?1", [&remapped], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "remapped window edit", "the window edit reaches the REMAPPED row");
    let tags: Vec<String> = target
        .prepare("SELECT tag FROM repo_memory_tags WHERE memory_id=?1 ORDER BY tag")
        .unwrap()
        .query_map([&remapped], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(tags, ["remap-tag"], "the remapped parent's children are replaced, not unioned");
    // The FTS mirror covers the remapped refresh (whole-repo re-derive).
    assert_eq!(
        count(
            &target,
            "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_id='global-repo' AND repo_memory_fts \
             MATCH 'remapped'"
        ),
        1
    );
    // The foreign owner of the ORIGINAL id is untouched — content and children.
    let (other_title, other_repo): (String, String) = target
        .query_row("SELECT title, repo_id FROM repo_memories WHERE id='m1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((other_title.as_str(), other_repo.as_str()), ("other title", "other-repo"));

    // Convergence: a no-edit retry reports zeros.
    let third =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(third.memories, 0);
    assert_eq!(third.tags, 0);
}

/// The mirror invariant on the MODEL-STATE unit (Codex batch 9): a model switch made in the
/// crash-retry window — new active model, bumped freshness version, the remote config KEY
/// REMOVED (moving remote → local) — is carried whole by the retry: values upserted, the
/// absent key deleted, the NEW model's readiness restored. A no-edit retry reports zero meta
/// keys.
#[test]
fn a_retry_carries_a_window_model_switch() {
    let source = seeded_source();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();

    // ... rename fails; the user switches models in the still-live legacy DB.
    source
        .execute("UPDATE repo_meta SET value='model-b' WHERE key='active_embedding_model'", [])
        .unwrap();
    source.execute("DELETE FROM repo_meta WHERE key='active_embedding_remote_config'", []).unwrap();
    source
        .execute(
            "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
             disabled, status, installed_at_ms, last_error)
                 VALUES ('model-b', 'embedding', 512, 'fastembed', 1, 0, 'Ready', 7, NULL)",
            [],
        )
        .unwrap();

    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert!(counts.meta_keys >= 2, "the upserted model + deleted remote config both report");

    let meta = |key: &str| -> Option<String> {
        target
            .query_row(
                "SELECT value FROM repo_meta WHERE repo_id='global-repo' AND key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    };
    assert_eq!(meta("active_embedding_model").as_deref(), Some("model-b"));
    assert_eq!(
        meta("active_embedding_remote_config"),
        None,
        "the key removed legacy-side is removed here too — absence has meaning"
    );
    // The NEW model's readiness restored on the retry (re-derived from the source each run).
    let status: String = target
        .query_row("SELECT status FROM ai_models WHERE model_id='model-b'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "Ready");

    let third =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(third.meta_keys, 0, "a no-edit retry writes no meta");
}

/// A memory id already owned by a DIFFERENT repo in the global store (a pre-repo-vintage id,
/// or a copied index) is REMAPPED — never dropped, and its children never attach to the other
/// repo's memory. The remap is deterministic, so a retry converges instead of duplicating.
#[test]
fn import_remaps_a_memory_id_owned_by_another_repo() {
    let source = seeded_source();
    let target = fresh_target();
    // The global store already holds a DIFFERENT repo's memory under the id "m1" (m1 is the
    // source memory that carries the binding/tag/call-path children), with its own tag.
    target
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('other-repo', \
             'o', 0)",
            [],
        )
        .unwrap();
    target
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id)
                 VALUES ('m1', 'Risk', 'other title', 'other body', 'low', 'active', 0, 0, \
             'agent', 'v1', 'other-repo')",
            [],
        )
        .unwrap();
    target
        .execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('m1', 'other-tag')", [])
        .unwrap();

    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(counts.memories, 3, "the colliding memory is remapped, not dropped");

    // The other repo's memory is untouched — content, ownership, and its own children.
    let (other_title, other_repo): (String, String) = target
        .query_row("SELECT title, repo_id FROM repo_memories WHERE id='m1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(other_title, "other title", "the other repo's memory content is untouched");
    assert_eq!(other_repo, "other-repo");
    let other_tags: i64 = target
        .query_row("SELECT COUNT(*) FROM repo_memory_tags WHERE memory_id='m1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(other_tags, 1, "no imported child attached to the other repo's memory id");

    // The imported memory landed under the DETERMINISTIC remapped id, with its children.
    let new_id = remapped_memory_id("global-repo", "m1");
    let (title, repo): (String, String) = target
        .query_row("SELECT title, repo_id FROM repo_memories WHERE id = ?1", [&new_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(title, "one", "the source memory's content imported under the remapped id");
    assert_eq!(repo, "global-repo");
    for (table, expected) in [
        ("repo_memory_bindings", 1i64),
        ("repo_memory_tags", 1),
        ("repo_memory_call_paths", 1),
        ("repo_memory_call_path_edges", 1),
    ] {
        let rows: i64 = target
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE memory_id = ?1"),
                [&new_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, expected, "{table}: children follow the remapped id");
    }

    // RETRY: the deterministic remap converges — zero new rows, no duplicate remapped copies.
    let retry =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(retry.memories, 0, "the retry re-derives the SAME remapped id and no-ops");
    let remapped_copies: i64 = target
        .query_row(
            "SELECT COUNT(*) FROM repo_memories WHERE repo_id='global-repo' AND title='one'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remapped_copies, 1, "exactly one remapped copy across retries");
}

/// A dangling child row in the SOURCE (its memory_id absent from the source's repo_memories,
/// while a memory with that id exists in the target under ANOTHER repo) is skipped — the
/// child-ownership invariant: children only ever insert under a parent id this import owns.
#[test]
fn import_skips_orphan_children_instead_of_attaching_them_across_repos() {
    let source = seeded_source();
    // A dangling tag in the source referencing a memory the source does NOT hold. The child
    // tables carry no FK in some legacy vintages; simulate by deleting the parent after
    // inserting the tag under FK-off.
    source.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    source
        .execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('foreign-mem', 'stray')", [])
        .unwrap();

    let target = fresh_target();
    // The target holds 'foreign-mem' under ANOTHER repo — the row the orphan child would have
    // contaminated.
    target
        .execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('other-repo', \
             'o', 0)",
            [],
        )
        .unwrap();
    target
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id)
                 VALUES ('foreign-mem', 'Risk', 't', 'b', 'low', 'active', 0, 0, 'agent', 'v1', \
             'other-repo')",
            [],
        )
        .unwrap();

    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    let stray: i64 = target
        .query_row("SELECT COUNT(*) FROM repo_memory_tags WHERE memory_id='foreign-mem'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stray, 0, "an orphan source child never attaches to another repo's memory");
}

/// The mirror invariant on the model-state unit (Codex batch 9, SUPERSEDES the batch-4
/// "config-seeded value wins" posture): the legacy DB is the LIVE store for this repo until
/// the rename lands — nothing else can legitimately write this repo's meta mid-consolidate
/// (the import holds the repo's write locks; the target migrate is schema-only), so a target
/// value that differs is a STALE copy from a previous crashed run and the source must win.
#[test]
fn carried_meta_mirrors_the_source_over_a_stale_target_value() {
    let source = seeded_source();
    let target = fresh_target();
    // A previous crashed run copied an older model choice; the window changed it legacy-side.
    target
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('global-repo', \
             'active_embedding_model', 'stale-model')",
            [],
        )
        .unwrap();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    let value: String = target
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id='global-repo' AND \
             key='active_embedding_model'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(value, "model-a", "the authoritative legacy value replaces the stale copy");
}

#[test]
fn sealed_policy_survives_import_and_reconcile_authors_only_suite_1() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', ?1, 'sealed')",
            [MEMORY_STREAM_SEAL_POLICY_META_KEY],
        )
        .unwrap();
    let target = fresh_target();

    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    crate::memory_write::reconcile_owner_stream_for_repo(
        &target,
        "global-repo",
        rag_rat_base::time::now_ms(),
    )
    .unwrap();

    let policy: String = target
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = 'global-repo' AND key = ?1",
            [MEMORY_STREAM_SEAL_POLICY_META_KEY],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(policy, "sealed", "consolidation carries the explicit privacy intent");
    let mut stmt = target.prepare("SELECT signed_bytes FROM content_entries").unwrap();
    let signed: Vec<Vec<u8>> =
        stmt.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert!(!signed.is_empty(), "reconcile authored the imported memories");
    assert!(
        signed.iter().all(|bytes| {
            rag_rat_oplog::decode_content_signed(bytes).unwrap().header.crypto_suite == 1
        }),
        "every reconciled content row is suite 1",
    );
}

#[test]
fn public_access_mode_carries_through_import() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', ?1, 'public')",
            [MEMORY_STREAM_ACCESS_MODE_META_KEY],
        )
        .unwrap();
    let target = fresh_target();

    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();

    let mode: String = target
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = 'global-repo' AND key = ?1",
            [MEMORY_STREAM_ACCESS_MODE_META_KEY],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mode, "public", "consolidation carries the public access-mode intent");
}

#[test]
fn unknown_source_access_mode_aborts_import_before_any_authoring() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', ?1, \
             'future-mode')",
            [MEMORY_STREAM_ACCESS_MODE_META_KEY],
        )
        .unwrap();
    let target = fresh_target();

    let err = import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy)
        .expect_err("an unknown access mode must fail closed");
    assert!(err.to_string().contains("unknown memory stream access mode"), "got: {err}");
}

#[test]
fn unknown_source_seal_policy_aborts_import_before_any_authoring() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', ?1, \
             'future-policy')",
            [MEMORY_STREAM_SEAL_POLICY_META_KEY],
        )
        .unwrap();
    let target = fresh_target();

    let err = import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy)
        .expect_err("an unknown policy must fail closed");
    assert!(err.to_string().contains("unknown memory stream seal policy"));
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 0);
    assert_eq!(count(&target, "SELECT COUNT(*) FROM content_entries"), 0);
}

#[test]
fn sealed_target_wins_retries_and_rejects_a_conflicting_unsafe_source() {
    let source = seeded_source();
    let target = fresh_target();
    target
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('global-repo', ?1, 'sealed')",
            [MEMORY_STREAM_SEAL_POLICY_META_KEY],
        )
        .unwrap();

    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    let policy = || -> String {
        target
            .query_row(
                "SELECT value FROM repo_meta WHERE repo_id = 'global-repo' AND key = ?1",
                [MEMORY_STREAM_SEAL_POLICY_META_KEY],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(policy(), "sealed", "an absent source never clears a sealed target");

    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', ?1, 'plaintext')",
            [MEMORY_STREAM_SEAL_POLICY_META_KEY],
        )
        .unwrap();
    let err = import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy)
        .expect_err("plaintext must not override a sealed target");
    assert!(err.to_string().contains("unknown memory stream seal policy"));
    assert_eq!(policy(), "sealed", "the conflicting import cannot downgrade the target");
    assert_eq!(count(&target, "SELECT COUNT(*) FROM content_entries"), 0);
}

fn target_meta(target: &Connection, key: &str) -> Option<String> {
    target
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = 'global-repo' AND key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
}

/// The pin is the one subscription field built to outlive the subscription. Dropped on the
/// move, a changed `.rag-rat-stream` would read as first use afterward.
#[test]
fn consolidation_carries_the_trust_pin() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', \
             'memory_stream_pin', 'owner-a')",
            [],
        )
        .unwrap();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(target_meta(&target, "memory_stream_pin").as_deref(), Some("owner-a"));
}

/// A store from before the pin existed, or one still subscribed, records its trust decision
/// only as the subscription owner. The pin carries that owner; the subscription itself
/// never crosses.
#[test]
fn consolidation_carries_a_subscription_owner_as_the_pin() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', \
             'memory_subscription_owner', 'owner-a')",
            [],
        )
        .unwrap();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(target_meta(&target, "memory_stream_pin").as_deref(), Some("owner-a"));
    assert_eq!(
        target_meta(&target, "memory_subscription_owner"),
        None,
        "the subscription itself stays behind",
    );
}

/// A conflicting pin the target decided itself is a separate trust decision. Carrying either
/// would silently override the other, so the run refuses — before anything is imported.
#[test]
fn a_conflicting_target_pin_refuses_the_import() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', \
             'memory_stream_pin', 'owner-b')",
            [],
        )
        .unwrap();
    let target = fresh_target();
    target
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('global-repo', \
             'memory_stream_pin', 'owner-a')",
            [],
        )
        .unwrap();

    let err = import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy)
        .expect_err("two trust roots for one repository must not be reconciled silently");
    assert!(err.to_string().contains("consolidation refused"), "got: {err}");
    assert_eq!(target_meta(&target, "memory_stream_pin").as_deref(), Some("owner-a"));
    assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 0, "nothing imported");
}

/// Two clones of one repository hold separate legacy indexes. The pin the first one's import
/// wrote is not the second one's to replace — only a retry of the SAME source may — so a second
/// source carrying a conflicting pin refuses.
#[test]
fn a_second_source_cannot_replace_the_pin_another_source_imported() {
    let dir = tempfile::tempdir().unwrap();
    let file_source = |name: &str, pin: &str| -> Connection {
        let seed = seeded_source();
        seed.execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', \
             'memory_stream_pin', ?1)",
            [pin],
        )
        .unwrap();
        let path = dir.path().join(name);
        seed.execute("VACUUM INTO ?1", [path.to_str().unwrap()]).unwrap();
        Connection::open(&path).unwrap()
    };
    let first = file_source("first.sqlite", "owner-a");
    let second = file_source("second.sqlite", "owner-b");
    let target = fresh_target();
    import_from_source(&first, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(target_meta(&target, "memory_stream_pin").as_deref(), Some("owner-a"));

    let err = import_from_source(&second, &target, "global-repo", ImportMode::ConsolidateLegacy)
        .expect_err("another source's conflicting pin is not a retry of the first");
    assert!(err.to_string().contains("consolidation refused"), "got: {err}");
    assert_eq!(target_meta(&target, "memory_stream_pin").as_deref(), Some("owner-a"));
}

/// Until the rename lands the legacy index is the live store. A repin made there after an
/// import committed but the rename failed must replace the pin that import wrote; keeping it
/// would restore the owner the operator has just moved away from.
#[test]
fn a_retry_carries_a_repin_made_in_the_legacy_index() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', \
             'memory_stream_pin', 'owner-a')",
            [],
        )
        .unwrap();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(target_meta(&target, "memory_stream_pin").as_deref(), Some("owner-a"));

    source
        .execute("UPDATE repo_meta SET value = 'owner-b' WHERE key = 'memory_stream_pin'", [])
        .unwrap();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(
        target_meta(&target, "memory_stream_pin").as_deref(),
        Some("owner-b"),
        "the retry carries the legacy-side repin forward",
    );
}

/// A pin re-decided in the target after an import is the target's own decision again, not a
/// copy this consolidation left, so a retry carrying a different one refuses rather than
/// overwriting it.
#[test]
fn a_pin_re_decided_in_the_target_after_an_import_is_not_overwritten() {
    let source = seeded_source();
    source
        .execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', \
             'memory_stream_pin', 'owner-a')",
            [],
        )
        .unwrap();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    target
        .execute(
            "UPDATE repo_meta SET value = 'owner-c' WHERE repo_id = 'global-repo' AND key = \
             'memory_stream_pin'",
            [],
        )
        .unwrap();
    source
        .execute("UPDATE repo_meta SET value = 'owner-b' WHERE key = 'memory_stream_pin'", [])
        .unwrap();

    let err = import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy)
        .expect_err("a pin the target re-decided is not this consolidation's to replace");
    assert!(err.to_string().contains("consolidation refused"), "got: {err}");
    assert_eq!(target_meta(&target, "memory_stream_pin").as_deref(), Some("owner-c"));
}

/// The `repo_meta` classification (see [`CARRIED_META_KEYS`]): repo-PORTABLE configuration is
/// carried verbatim — including `active_embedding_remote_config`, which `active_embedder()`
/// reconstructs the remote transport from (dropping it silently rerouted post-consolidation
/// searches to the local backend until reinstall) — while DB-LOCAL state (freshness cursors,
/// transient install markers) never crosses.
#[test]
fn import_carries_portable_meta_and_leaves_db_local_state() {
    let source = seeded_source();
    let target = fresh_target();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();

    let meta = |key: &str| -> Option<String> {
        target
            .query_row(
                "SELECT value FROM repo_meta WHERE repo_id='global-repo' AND key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    };
    assert_eq!(meta("active_embedding_model").as_deref(), Some("model-a"));
    assert_eq!(
        meta("active_embedding_remote_config").as_deref(),
        Some("{\"endpoint\":\"http://ollama:11434\"}"),
        "the remote-endpoint config survives verbatim — active_embedder routes remote",
    );
    assert_eq!(meta("git_commit"), None, "freshness cursors never cross (DB-local)");
    // The provisional flag is SEMANTIC state: absent ⇒ non-provisional (config-immune explicit
    // choice), so an auto-picked "1" must cross or `seed_active_embedding_model` could no
    // longer override the model from config post-consolidation.
    assert_eq!(
        meta("active_embedding_model_provisional").as_deref(),
        Some("1"),
        "the provisional auto-pick provenance survives — config can still override",
    );
    // And the model-state unit includes the ai_models READINESS row: identity without it
    // points at MissingModel and active_embedder refuses despite the carried cache.
    let (installed, status): (i64, String) = target
        .query_row("SELECT installed, status FROM ai_models WHERE model_id='model-a'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((installed, status.as_str()), (1, "Ready"), "readiness row carried");
}

/// The readiness carry's two guarded shapes: a target row stuck at `MissingModel` (the
/// manifest's fresh-DB seed) is RESTORED to the legacy Ready state; a target row the machine
/// explicitly DISABLED is never overridden (the opt-out is shared by every repo in the global
/// DB).
#[test]
fn readiness_carry_restores_missing_model_but_respects_disabled() {
    let source = seeded_source();

    // Target seeded MissingModel (what ensure_model_manifest writes on a fresh DB) → restored.
    let target = fresh_target();
    target
        .execute(
            "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
             disabled, status) VALUES ('model-a', 'embedding', 384, 'local', 0, 0, 'MissingModel')",
            [],
        )
        .unwrap();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    let (installed, status): (i64, String) = target
        .query_row("SELECT installed, status FROM ai_models WHERE model_id='model-a'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (installed, status.as_str()),
        (1, "Ready"),
        "a MissingModel manifest seed is restored from the legacy Ready row",
    );

    // Target explicitly disabled → untouched.
    let target = fresh_target();
    target
        .execute(
            "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
             disabled, status) VALUES ('model-a', 'embedding', 384, 'local', 0, 1, 'MissingModel')",
            [],
        )
        .unwrap();
    import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    let (installed, disabled, status): (i64, i64, String) = target
        .query_row(
            "SELECT installed, disabled, status FROM ai_models WHERE model_id='model-a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (installed, disabled, status.as_str()),
        (0, 1, "MissingModel"),
        "an explicit machine-level disable is never overridden by a carry",
    );
}

// --- consolidation re-authors imported rows into the target owner stream (#541 Task 5) ---

/// A target owner stream is rooted via a REAL `create_memory` call (not raw SQL) so the chain
/// is genuinely non-empty — `fresh_target()` registers exactly one repo (`global-repo`), so
/// `memory_repo_scope`'s sole-repo fallback resolves it without any extra connection-context
/// setup.
fn seeded_target_with_rooted_chain() -> Connection {
    let target = fresh_target();
    crate::memory_write::create_memory(&target, rag_rat_query::memory::RepoMemoryCreate {
        kind: "Concept".to_string(),
        title: "seed".to_string(),
        body: "body".to_string(),
        confidence: "high".to_string(),
        created_by: None,
        source: None,
        tags: Vec::new(),
        payload_json: None,
        bind: rag_rat_query::memory::RepoMemoryBindTarget::default(),
    })
    .unwrap();
    target
}

/// Before #541 Task 5, consolidation's import never touched the op-log: the imported rows
/// landed in `repo_memories`/`repo_node_edges` but were skipped by the reconcile — a later
/// `mark_obsolete` on such a row would author an inert `NodeStatus` with no `NodeCreate` behind
/// it. This test proves the reconcile call `run` now makes closes that gap on the owner-bound
/// `/2`//3 substrate (#664): an imported memory is present in the target's accepted-`/3`
/// projection, and a follow-up `mark_obsolete` is NOT inert.
#[test]
fn consolidation_authors_imported_memories_into_target_owner_stream() {
    let source = seeded_source();
    let target = seeded_target_with_rooted_chain();

    // The count of a projected node in the accepted-`/3` projection (a test target holds one
    // repo/stream, so no stream filter is needed).
    let projected_m1 = |target: &rusqlite::Connection| -> i64 {
        target
            .query_row(
                "SELECT COUNT(*) FROM content_projected_nodes WHERE node_id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };

    // PREMISE SELF-CHECK: the seed's real `create_memory` must have rooted a non-empty `/3`
    // content chain for `global-repo`, or this test silently degrades to the (already-covered)
    // genesis path and stops exercising the per-node reconcile bypass #541 fixes.
    let content_entries: i64 =
        target.query_row("SELECT COUNT(*) FROM content_entries", [], |r| r.get(0)).unwrap();
    assert!(
        content_entries > 0,
        "the seed must root a non-empty /3 content chain, or this test degrades to genesis"
    );

    // The import itself does NOT touch the op-log (it predates #541's reconcile call) — only
    // `repo_memories`/`repo_node_edges` gain the imported rows.
    let counts =
        import_from_source(&source, &target, "global-repo", ImportMode::ConsolidateLegacy).unwrap();
    assert_eq!(counts.memories, 3, "sanity: the import still carries all 3 source memories");
    assert_eq!(
        projected_m1(&target),
        0,
        "pre-reconcile: the imported memory m1 must NOT yet be projected (the per-node anti-join \
         skipped it, the bug #541 fixes) — only the seed memory should be projected here"
    );

    // The call this task wires into `run`, immediately after `import_from_source` and before
    // the legacy-file rename.
    crate::memory_write::reconcile_owner_stream_for_repo(
        &target,
        "global-repo",
        rag_rat_base::time::now_ms(),
    )
    .unwrap();

    // An imported memory is now present in the target owner stream's accepted-`/3` projection.
    assert_eq!(
        projected_m1(&target),
        1,
        "the imported memory m1 must be present in the target owner stream's /3 projection"
    );

    // A follow-up `mark_obsolete` on the imported memory is NOT inert: it flips the projected
    // status (which requires the `NodeCreate` the reconcile just authored).
    crate::memory_write::mark_obsolete(&target, "m1").unwrap();
    let status: String = target
        .query_row("SELECT status FROM content_projected_nodes WHERE node_id = 'm1'", [], |r| {
            r.get(0)
        })
        .expect("m1 must still be projected after mark_obsolete");
    assert_eq!(status, "obsolete", "mark_obsolete on an imported memory must not be inert");
}
