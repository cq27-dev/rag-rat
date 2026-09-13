//! The purge-completeness TRIPWIRE. It builds a real fixture repo A alongside the
//! poison-sibling repo B (seeded across every repo-scoped table by the rebuild-tail harness),
//! seeds A's derived transitive children too, then purges A and asserts — at the CLASS level,
//! by enumerating the schema rather than a hand-listed subset — that (a) ZERO rows remain for A
//! in every `repo_id`-bearing table and no orphan survives in any transitive child, and (b) the
//! poison sibling B is byte-for-byte untouched. A future migration that adds a new repo-scoped
//! table is swept automatically (the class-level `repo_id` enumeration), and this test proves
//! an unscoped purge would wipe B (via `assert_sibling_intact`) — so a scoping regression
//! fails here instead of in production.

use rag_rat_base::repo_identity::LEGACY_REPO_ID;
use rag_rat_db::schema::{self, purge_repo_rows, repo_scoped_table_names};
use rag_rat_db::storage::IndexConnection;
use rusqlite::params;

use super::purge_and_vacuum;
use crate::index::IndexDatabase;
use crate::index::poison_sibling::{POISON_REPO_ID, assert_sibling_intact};
use crate::index::schema_bootstrap_tests::poison_test_config;

#[test]
fn missing_path_fallback_lexically_normalizes_parent_components() {
    let base = std::path::absolute(".").unwrap();
    let spelled_with_parent = base.join("gone").join("..").join("old");
    assert_eq!(
        super::lexically_normalize(&spelled_with_parent),
        base.join("old"),
        "a missing checkout path must match the canonical repo_roots spelling"
    );
}

#[test]
fn identity_resolution_accepts_the_worktree_top_but_rejects_an_arbitrary_child() {
    let (root, config) = poison_test_config("rm_identity_target");
    let db = IndexDatabase::rebuild(&config).unwrap();
    drop(db);
    let storage = IndexConnection::open(&config.database).unwrap();
    let conn = storage.connection();

    let top = super::resolve_removable_repo(conn, &root)
        .unwrap()
        .expect("the worktree top resolves by identity");
    let child = root.join("src");
    assert!(
        super::resolve_removable_repo(conn, &child).unwrap().is_none(),
        "an arbitrary existing child must not select its enclosing repo"
    );

    // A configured `[index] root = "src"` records that exact subroot. Route 2 must continue to
    // accept it even though route 1 correctly rejects child-path identity discovery.
    conn.execute("UPDATE repo_roots SET root = ?1 WHERE repo_id = ?2", rusqlite::params![
        child.to_string_lossy(),
        top.repo_id
    ])
    .unwrap();
    assert_eq!(
        super::resolve_removable_repo(conn, &child).unwrap().unwrap().repo_id,
        top.repo_id,
        "an exact recorded config root remains a valid destructive target"
    );
    drop(storage);
    let _ = std::fs::remove_dir_all(&root);
}

/// The one REAL fixture repo (neither the `__unassigned__` placeholder nor the poison sibling).
fn fixture_repo_id(conn: &rusqlite::Connection) -> String {
    conn.query_row(
        "SELECT repo_id FROM repos WHERE repo_id != ?1 AND repo_id != ?2",
        params![LEGACY_REPO_ID, POISON_REPO_ID],
        |row| row.get(0),
    )
    .expect("a real fixture repo is registered on a git fixture")
}

/// A comma-joined `IN (...)` body for a captured id set (empty → `NULL`, matching nothing).
fn in_clause(ids: &[i64]) -> String {
    if ids.is_empty() {
        "NULL".to_string()
    } else {
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    }
}

/// Seed one sentinel row into each DERIVED chunk/symbol transitive child the poison harness
/// does not itself seed, hung off the given chunk + symbol. `tag` keeps rows for two
/// different repos (the removed repo and the sibling) collision-free. So purging has
/// something to delete in those tables and the orphan check is not vacuous.
fn seed_chunk_symbol_children(
    conn: &rusqlite::Connection,
    tag: &str,
    chunk_id: i64,
    symbol_id: i64,
) -> i64 {
    // OR IGNORE: a real fixture chunk already carries a chunk_text row (leave it); the poison
    // chunk carries none (seed one so the purge has a chunk_text row to remove).
    conn.execute(
        "INSERT OR IGNORE INTO chunk_text(chunk_id, blob, raw_len, dict_version) VALUES (?1, \
         x'00', 1, 0)",
        params![chunk_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO chunk_embeddings(chunk_id, model_id, model_version, source_text_hash, \
         input_hash, embedding_text_version, embedding_policy, embedding_priority, input_chars, \
         input_truncated, embedding_dim, vector_blob, status, attempt_count, created_at_ms)
             VALUES (?1, 'm', 'v', ?2, ?2, 'etv', 'eligible', 0, 1, 0, 1, x'00', 'ready', 0, 0)",
        params![chunk_id, format!("sth_{tag}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO chunk_summaries(chunk_id, model_id, prompt_version, input_hash, text_hash, \
         summary, status, attempt_count)
             VALUES (?1, 'm', 'pv', ?2, 'th', 'summary', 'ready', 0)",
        params![chunk_id, format!("ih_{tag}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO git_chunk_blame(chunk_id, source_text_hash, path, start_line, end_line, \
         line_count, dominant_commit_lines, commit_counts_json, computed_at_ms)
             VALUES (?1, ?2, 'src/a.rs', 0, 0, 1, 0, '{}', 0)",
        params![chunk_id, format!("sth_{tag}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO symbol_facts(symbol_id, fact_kind, fact_value) VALUES (?1, ?2, 'test')",
        params![symbol_id, format!("attr_{tag}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO symbol_fingerprints(symbol_id, normalizer_kind, normalizer_version, \
         struct_hash, token_len, created_at_ms)
             VALUES (?1, ?2, 1, 'sh', 1, 0)",
        params![symbol_id, format!("baseline_{tag}")],
    )
    .unwrap();
    // #782: a legacy/malformed edge with no source_file_id. Normal index writers stamp the
    // source file, but the nullable schema permits this shape; purge must reach it through a
    // victim symbol endpoint. Reusing the symbol's interned qualified-name id satisfies every
    // required interned-id column without adding unrelated fixture state.
    conn.execute(
        "INSERT INTO edges_data(source_file_id, from_symbol_id, to_symbol_id, from_name_id, \
         to_name_id, edge_kind_id, confidence_id, resolution_id) SELECT NULL, s.id, s.id, n.id, \
         n.id, n.id, n.id, n.id FROM symbols s CROSS JOIN (SELECT id FROM name_strings ORDER BY \
         id LIMIT 1) n WHERE s.id = ?1",
        params![symbol_id],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// Seed the memory- and clone-generation transitive children the poison harness does not seed,
/// hung off the poison sibling's memory + clone generation, so purging the sibling exercises
/// (and the orphan check pins) every transitive child.
fn seed_memory_and_clone_children(conn: &rusqlite::Connection, memory_id: &str, generation: i64) {
    conn.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, edge_sequence_hash, path_summary, \
         created_at_ms) VALUES (?1, 'esh', 'summary', 0)",
        params![memory_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
         edge_fingerprint, to_name, edge_kind) VALUES (?1, 'esh', 0, 'fp', 'callee', 'calls')",
        params![memory_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_edges(build_generation, a_path, a_start_byte, a_file_sha, b_path, \
         b_start_byte, b_file_sha, overlap, a_token_len, b_token_len, similarity, edge_source)
             VALUES (?1, 'a.rs', 0, 'sa', 'b.rs', 0, 'sb', 1, 1, 1, 1.0, 'exact')",
        params![generation],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_subblock_postings(build_generation, token_hash, path, start_byte, \
         file_sha) VALUES (?1, 1, 'a.rs', 0, 'sa')",
        params![generation],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_df_epoch(build_generation, token_hash, df) VALUES (?1, 1, 1)",
        params![generation],
    )
    .unwrap();
}

/// Seed one table-sync stream directory row for `repo_id` plus one entry on that stream, and
/// return the stream id. The entry log carries no `repo_id` and is reached ONLY through the
/// directory (#1004), so without this the stream-keyed half of the purge is unobservable — the
/// class-level assertion below would pass on an empty table and prove nothing.
fn seed_table_sync_stream(conn: &rusqlite::Connection, repo_id: &str, seed: u8) -> Vec<u8> {
    let stream_id = vec![seed; 32];
    conn.execute(
        "INSERT INTO table_sync_streams(
                 stream_id, repo_id, account_id, incarnation_ref, scope_id
             ) VALUES (?1, ?2, ?3, ?4, 'demo/1')",
        params![stream_id, repo_id, vec![seed; 32], vec![seed ^ 0x55; 32]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO table_sync_entries(entry_hash, stream_id, device_fingerprint, lamport, \
         prev_hash, signed_bytes, received_at_ms) VALUES (?1, ?2, ?3, 1, NULL, ?4, 0)",
        params![vec![seed; 32], stream_id, vec![seed; 32], vec![seed; 8]],
    )
    .unwrap();
    // The gapped half of the log is swept through the same directory, so it needs its own seed
    // row or the assertion below passes on an empty table.
    conn.execute(
        "INSERT INTO table_sync_gapped_entries(entry_hash, stream_id, device_fingerprint, \
         lamport, prev_hash, signed_bytes, gapped_at_ms) VALUES (?1, ?2, ?3, 2, ?4, ?5, 0)",
        params![vec![seed ^ 0xff; 32], stream_id, vec![seed; 32], vec![seed; 32], vec![seed; 8]],
    )
    .unwrap();
    // The re-adoption tables are swept through the same directory (#997): a pending repair and
    // its provenance both name the stream, so each needs a seed row for the orphan check to
    // observe.
    conn.execute(
        "INSERT INTO table_sync_readoption_work(account_id, device_fingerprint, stream_id, \
         roster_ref, removed_at_epoch, enqueued_at_ms) VALUES (?1, ?2, ?3, ?4, 1, 0)",
        params![vec![seed; 32], vec![seed; 32], stream_id, vec![seed ^ 0x11; 32]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO table_sync_readoption_audit(account_id, removed_fingerprint, \
         adopter_fingerprint, stream_id, repo_id, scope_id, table_name, row_pk, original_lamport, \
         original_entry_hash, adopted_entry_hash, adopted_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, \
         'demo/1', 't', 'aa', 1, ?6, ?7, 0)",
        params![
            vec![seed; 32],
            vec![seed; 32],
            vec![seed ^ 0x22; 32],
            stream_id,
            repo_id,
            vec![seed; 32],
            vec![seed ^ 0x33; 32]
        ],
    )
    .unwrap();
    // The retained floor bounds the same accepted log (#1127) and is swept with it.
    conn.execute(
        "INSERT INTO table_sync_retained_floors(stream_id, device_fingerprint, lamport, \
         entry_hash, compacted_at_ms) VALUES (?1, ?2, 1, ?3, 0)",
        params![stream_id, vec![seed; 32], vec![seed; 32]],
    )
    .unwrap();
    stream_id
}

/// A blob as a SQL `X'..'` literal, for the stream-keyed orphan check (the other id sets are
/// integers or text).
fn blob_literal(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("X'{hex}'")
}

/// Assert NO orphan survives the purge in ANY transitive child — checked against the parent id
/// sets CAPTURED BEFORE the purge, so a row whose parent was deleted but which itself was
/// missed (the exact escape a hand-list allows) is still caught. `NULL` id sets match
/// nothing (a repo with no memory / clone generation).
#[allow(clippy::too_many_arguments)]
fn assert_no_transitive_orphans(
    conn: &rusqlite::Connection,
    files: &[i64],
    chunks: &[i64],
    symbols: &[i64],
    memory_id: &str,
    generation: Option<i64>,
    stream_id: &[u8],
) {
    let file_in = in_clause(files);
    let chunk_in = in_clause(chunks);
    let symbol_in = in_clause(symbols);
    let null_edge_symbol_in = symbol_in.clone();
    let generation_in = generation.map(|g| g.to_string()).unwrap_or_else(|| "NULL".to_string());
    // (table, keyed column, IN-body) for every transitive child + the contentless chunk_fts.
    let checks: Vec<(&str, &str, String)> = vec![
        ("symbols", "file_id", file_in.clone()),
        ("chunks", "file_id", file_in.clone()),
        ("edges_data", "source_file_id", file_in),
        ("chunk_fts", "rowid", chunk_in.clone()),
        ("chunk_text", "chunk_id", chunk_in.clone()),
        ("chunk_embeddings", "chunk_id", chunk_in.clone()),
        ("chunk_summaries", "chunk_id", chunk_in.clone()),
        ("git_chunk_blame", "chunk_id", chunk_in),
        ("symbol_facts", "symbol_id", symbol_in.clone()),
        ("symbol_fingerprints", "symbol_id", symbol_in.clone()),
        ("logical_symbol_members", "symbol_id", symbol_in),
        ("repo_memory_tags", "memory_id", format!("'{memory_id}'")),
        ("repo_memory_call_paths", "memory_id", format!("'{memory_id}'")),
        ("repo_memory_call_path_edges", "memory_id", format!("'{memory_id}'")),
        ("clone_edges", "build_generation", generation_in.clone()),
        ("clone_subblock_postings", "build_generation", generation_in.clone()),
        ("clone_df_epoch", "build_generation", generation_in),
        ("table_sync_entries", "stream_id", blob_literal(stream_id)),
        ("table_sync_gapped_entries", "stream_id", blob_literal(stream_id)),
        ("table_sync_readoption_work", "stream_id", blob_literal(stream_id)),
        ("table_sync_readoption_audit", "stream_id", blob_literal(stream_id)),
        ("table_sync_retained_floors", "stream_id", blob_literal(stream_id)),
    ];
    for (table, column, in_body) in checks {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column} IN ({in_body})"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "purge left {count} orphaned row(s) in transitive child `{table}` (WHERE {column} IN \
             the removed repo's ids) — add it to TRANSITIVE_SCOPED_TABLES",
        );
    }
    let null_source_edges: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM edges_data WHERE source_file_id IS NULL AND (from_symbol_id \
                 IN ({null_edge_symbol_in}) OR to_symbol_id IN ({null_edge_symbol_in}))"
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        null_source_edges, 0,
        "purge left NULL-source edge row(s) attached to the removed repo's symbols"
    );
}

#[test]
fn purge_removes_every_scoped_row_for_the_repo_and_leaves_the_sibling_intact() {
    let (_root, config) = poison_test_config("rm_purge_tripwire");
    // Rebuild the fixture (indexes repo A, seeds the poison sibling B across ~37 tables), then
    // operate through a BARE connection exactly as the production purge does — the scoped
    // `IndexDatabase` connection installs a `temp.files` view that hides `repo_id` and shadows
    // the base tables.
    let _db = IndexDatabase::rebuild(&config).unwrap();
    let storage = IndexConnection::open(&config.database).unwrap();
    let conn = storage.connection();

    // We PURGE the poison sibling B (seeded across ~37 repo_id tables — far broader than the
    // fixture, so a table the sweep misses is caught) and PROTECT the real fixture A.
    let repo_a = fixture_repo_id(conn);
    let scalar_i64 = |sql: &str, param: i64| -> i64 {
        conn.query_row(sql, params![param], |row| row.get(0)).unwrap()
    };

    // B's poison ids (captured BEFORE any delete so the orphan check is exact).
    let b_file: i64 = conn
        .query_row(
            "SELECT id FROM files WHERE repo_id = ?1 AND path LIKE 'zz_poison_file%' LIMIT 1",
            params![POISON_REPO_ID],
            |row| row.get(0),
        )
        .unwrap();
    let b_chunk = scalar_i64("SELECT id FROM chunks WHERE file_id = ?1 LIMIT 1", b_file);
    let b_symbol = scalar_i64("SELECT id FROM symbols WHERE file_id = ?1 LIMIT 1", b_file);
    let b_memory: String = conn
        .query_row(
            "SELECT id FROM repo_memories WHERE repo_id = ?1 LIMIT 1",
            params![POISON_REPO_ID],
            |row| row.get(0),
        )
        .unwrap();
    let b_generation: i64 = conn
        .query_row(
            "SELECT generation FROM clone_graph_generations WHERE repo_id = ?1 LIMIT 1",
            params![POISON_REPO_ID],
            |row| row.get(0),
        )
        .unwrap();
    let b_files = vec![b_file];
    let b_chunks = vec![b_chunk];
    let b_symbols = vec![b_symbol];

    // Seed B's derived + memory + clone transitive children (the ones the poison harness does
    // not seed) so purging B exercises — and the orphan check pins — every transitive child.
    let b_null_source_edge = seed_chunk_symbol_children(conn, "b", b_chunk, b_symbol);
    seed_memory_and_clone_children(conn, &b_memory, b_generation);

    // Seed the fixture A's chunk/symbol children too, so A HAS rows in those transitive tables
    // — then a scoping bug that deletes them (keyed by A's ids, which the B purge must
    // not touch) would drop A's counts and fail the sibling-integrity check below.
    let a_chunk: i64 = conn
        .query_row(
            "SELECT id FROM chunks WHERE file_id IN (SELECT id FROM files WHERE repo_id = ?1) \
             LIMIT 1",
            params![&repo_a],
            |row| row.get(0),
        )
        .unwrap();
    let a_symbol: i64 = conn
        .query_row(
            "SELECT id FROM symbols WHERE file_id IN (SELECT id FROM files WHERE repo_id = ?1) \
             LIMIT 1",
            params![&repo_a],
            |row| row.get(0),
        )
        .unwrap();
    let a_null_source_edge = seed_chunk_symbol_children(conn, "a", a_chunk, a_symbol);

    // A table-sync stream + one entry on it for each repo. Seeded BEFORE the counts below so
    // A's entry is inside the parity check: the entry log has no `repo_id`, so a purge that
    // deleted it by anything other than B's captured stream ids would show up as A losing rows.
    let b_stream = seed_table_sync_stream(conn, POISON_REPO_ID, 0xbb);
    let a_stream = seed_table_sync_stream(conn, &repo_a, 0xaa);

    // The multi-checkout dimension of this purge is covered where it can be exercised for real:
    // `schema_bootstrap_tests::worktree_purge` builds a main checkout and a linked worktree
    // sharing one database, resolves the removal through the linked one, and asserts a sibling
    // repo's stream-keyed log survives. This tripwire stays about class-level completeness.

    // The full class-level table set, captured from the LIVE schema. A subset here would
    // silently narrow the guarantee, so pin a floor on its breadth.
    let repo_id_tables = repo_scoped_table_names(conn).unwrap();
    assert!(
        repo_id_tables.len() >= 40,
        "expected the class-level sweep to see the whole schema (>= 40 repo_id tables), saw {}",
        repo_id_tables.len()
    );

    // Pre-purge fact base: B is seeded across many tables; A has real rows.
    let a_before = schema::count_repo_rows(conn, &repo_a).unwrap();
    let b_before = schema::count_repo_rows(conn, POISON_REPO_ID).unwrap();
    assert!(a_before.total_rows > 0, "the fixture sibling must have rows to protect");
    assert!(
        b_before.by_table.len() >= 30,
        "the poison sibling must be seeded across ~37 tables (saw {})",
        b_before.by_table.len()
    );

    // Purge B inside one IMMEDIATE transaction, exactly as production does.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    purge_repo_rows(&tx, POISON_REPO_ID).unwrap();
    tx.commit().unwrap();

    // (a) CLASS-LEVEL COMPLETENESS: zero rows remain for B in EVERY repo_id-bearing table.
    for table in &repo_id_tables {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{table}\" WHERE repo_id = ?1"),
                params![POISON_REPO_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "purge left {count} row(s) for the removed repo in `{table}` — the class-level \
             repo_id sweep must reach it",
        );
    }
    // …and no orphan in any transitive child (checked against the ids captured before purge).
    assert_no_transitive_orphans(
        conn,
        &b_files,
        &b_chunks,
        &b_symbols,
        &b_memory,
        Some(b_generation),
        &b_stream,
    );
    // The stream-keyed half of #1004 explicitly: the directory row is gone via the class sweep,
    // and the entries that only it could place are gone with it. Retaining them would let a
    // same-incarnation re-registration of the repo — whose stream id is a pure function of
    // `(repo_id, account_id, incarnation_ref, scope_id)` — replay the removed repo's operations
    // back into it.
    let b_streams_left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM table_sync_streams WHERE repo_id = ?1",
            params![POISON_REPO_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(b_streams_left, 0, "the removed repo's stream directory rows are swept");
    let a_entries: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM table_sync_entries WHERE stream_id = {}",
                blob_literal(&a_stream)
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(a_entries, 1, "the sibling's stream-keyed entries survive the scoped purge");
    let a_gapped: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM table_sync_gapped_entries WHERE stream_id = {}",
                blob_literal(&a_stream)
            ),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(a_gapped, 1, "the sibling's entries awaiting a predecessor survive it too");
    let edge_exists = |id: i64| -> bool {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM edges_data WHERE id = ?1)", [id], |row| {
            row.get(0)
        })
        .unwrap()
    };
    assert!(
        !edge_exists(b_null_source_edge),
        "the victim's exact NULL-source edge row must be deleted, not merely endpoint-nulled"
    );
    assert!(
        edge_exists(a_null_source_edge),
        "the sibling's NULL-source edge row must survive the scoped purge"
    );
    // The read-side counter agrees: nothing left for B.
    assert_eq!(schema::count_repo_rows(conn, POISON_REPO_ID).unwrap().total_rows, 0);

    // (b) SIBLING INTEGRITY: the fixture A is untouched — same rows in every table, before and
    // after (the purge only DELETEs, so a per-table row-count parity catches any leak).
    let a_after = schema::count_repo_rows(conn, &repo_a).unwrap();
    assert_eq!(
        a_before.by_table, a_after.by_table,
        "the purge changed the fixture sibling's row counts — an unscoped delete leaked",
    );
}

#[test]
fn dry_run_style_count_does_not_mutate_the_store() {
    let (_root, config) = poison_test_config("rm_count_readonly");
    let _db = IndexDatabase::rebuild(&config).unwrap();
    let storage = IndexConnection::open(&config.database).unwrap();
    let conn = storage.connection();
    let repo_a = fixture_repo_id(conn);

    let before = schema::count_repo_rows(conn, &repo_a).unwrap();
    // Counting twice must be a pure read: identical result, sibling untouched.
    let again = schema::count_repo_rows(conn, &repo_a).unwrap();
    assert_eq!(before.by_table, again.by_table);
    assert_eq!(before.total_rows, again.total_rows);
    assert!(before.total_rows > 0);
    assert_sibling_intact(conn);
}

/// The public entry point removes the repo AND threads the deconfigure closure's result back:
/// after `purge_and_vacuum`, the repo is unregistered, no `repo_id` row survives, and the
/// closure ran (its value is returned). Exercises the restructured lock → purge → deconfigure →
/// VACUUM flow end to end.
#[test]
fn purge_and_vacuum_removes_the_repo_and_threads_the_deconfigure_result() {
    let (_root, config) = poison_test_config("rm_purge_and_vacuum");
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Read the fixture repo id, then DROP the rebuilt db so it releases the repo's write lock
    // before `purge_and_vacuum` tries to acquire it.
    let (repo_a, removal_generation) = {
        let storage = IndexConnection::open(&config.database).unwrap();
        let repo_id = fixture_repo_id(storage.connection());
        let generation = schema::repo_removal_generation(storage.connection(), &repo_id).unwrap();
        (repo_id, generation)
    };
    drop(db);

    let (outcome, cleanup) =
        purge_and_vacuum(&config.database, &repo_a, removal_generation, 12_345, || {
            "deconfigured".to_string()
        })
        .unwrap();
    assert!(outcome.purged_rows > 0, "the fixture repo had rows to purge");
    assert_eq!(cleanup, "deconfigured", "the deconfigure closure result must thread back");

    // The repo is gone from the registry and every repo_id table.
    let storage = IndexConnection::open(&config.database).unwrap();
    let conn = storage.connection();
    let still_registered: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM repos WHERE repo_id = ?1)",
            params![&repo_a],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!still_registered, "the purged repo must be unregistered");
    // …and it is TOMBSTONED so a stale-config writer can't silently re-register it.
    assert!(schema::is_repo_removed(conn, &repo_a).unwrap(), "the removed repo must be tombstoned");
    assert_eq!(
        schema::repo_removal_generation(conn, &repo_a).unwrap(),
        removal_generation + 1,
        "a completed purge advances the durable removal generation"
    );
    for table in repo_scoped_table_names(conn).unwrap() {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{table}\" WHERE repo_id = ?1"),
                params![&repo_a],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "rows for the purged repo remain in `{table}`");
    }
}

#[test]
fn purge_reports_and_waits_for_a_running_papertrail_flight() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (_root, config) = poison_test_config("rm_waits_for_papertrail");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let (repo_id, removal_generation) = {
        let storage = IndexConnection::open(&config.database).unwrap();
        let repo_id = fixture_repo_id(storage.connection());
        let generation = schema::repo_removal_generation(storage.connection(), &repo_id).unwrap();
        (repo_id, generation)
    };
    drop(db);

    let lock_path =
        rag_rat_base::locks::FlightKind::Papertrail.lock_path(&config.database, &repo_id);
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _lock = rag_rat_base::locks::FileLock::acquire_blocking(&lock_path).unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let database = config.database.clone();
    let (wait_tx, wait_rx) = mpsc::channel();
    let remover = std::thread::spawn(move || {
        super::purge_and_vacuum_with_wait(
            &database,
            &repo_id,
            removal_generation,
            12_345,
            || wait_tx.send(()).unwrap(),
            || (),
        )
    });

    wait_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the contention callback runs before blocking");
    assert!(!remover.is_finished(), "purge must wait until the flight lock drains");
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    remover.join().unwrap().unwrap();
}

#[test]
fn purge_refuses_a_plan_from_before_an_intervening_remove_and_readd() {
    use std::cell::Cell;

    let (root, config) = poison_test_config("rm_stale_plan");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let (repo_id, planned_generation) = {
        let storage = IndexConnection::open(&config.database).unwrap();
        let repo_id = fixture_repo_id(storage.connection());
        let generation = schema::repo_removal_generation(storage.connection(), &repo_id).unwrap();
        (repo_id, generation)
    };
    drop(db);

    purge_and_vacuum(&config.database, &repo_id, planned_generation, 10, || ()).unwrap();
    {
        let storage = IndexConnection::open(&config.database).unwrap();
        let conn = storage.connection();
        schema::clear_repo_removed(conn, &repo_id).unwrap();
        let identity = rag_rat_base::repo_identity::resolve_repo_identity(&root, None).unwrap();
        schema::register_repo(conn, &identity, &root, 11, &crate::index::migration_hooks())
            .unwrap();
    }

    let deconfigured = Cell::new(false);
    let err = purge_and_vacuum(&config.database, &repo_id, planned_generation, 12, || {
        deconfigured.set(true);
    })
    .expect_err("a plan from before remove/re-add must be rejected");
    assert!(err.to_string().contains("stale"), "unexpected refusal: {err}");
    assert!(!deconfigured.get(), "stale removal must not deconfigure the re-added repo");
    let storage = IndexConnection::open(&config.database).unwrap();
    assert!(
        schema::repo_id_is_registered(storage.connection(), &repo_id).unwrap(),
        "the newly re-added repo must survive the stale removal attempt"
    );
    drop(storage);
    let _ = std::fs::remove_dir_all(&root);
}

/// The removal tombstone gates re-registration: once a repo is marked removed, the indexing
/// (root-recording) path refuses it — the durable stop for a stale-config writer that queued
/// behind the lock — while the read-only adoption path stays open, and `rag-rat init`'s clear
/// lifts the gate.
#[test]
fn register_refuses_a_removed_repo_until_it_is_cleared() {
    use rag_rat_base::repo_identity::{RepoIdentity, RepoIdentityClass};

    let (_root, config) = poison_test_config("rm_tombstone_register");
    let _db = IndexDatabase::rebuild(&config).unwrap();
    let storage = IndexConnection::open(&config.database).unwrap();
    let conn = storage.connection();
    let hooks = crate::index::migration_hooks();
    let identity = RepoIdentity {
        repo_id: "tombstone-victim".to_string(),
        display_name: "tombstone-victim".to_string(),
        class: RepoIdentityClass::Portable,
        shallow_boundary: Vec::new(),
    };
    let tv_root = std::path::Path::new("/src/tombstone-victim");

    // A fresh registration works.
    schema::register_repo(conn, &identity, tv_root, 1, &hooks).unwrap();
    // Tombstone it → the indexing (root-recording) path refuses.
    schema::mark_repo_removed(conn, &identity.repo_id, 2).unwrap();
    let err = schema::register_repo(conn, &identity, tv_root, 3, &hooks)
        .expect_err("a tombstoned repo must refuse re-registration");
    assert!(
        format!("{err}").contains("rag-rat rm") || format!("{err}").contains("removed"),
        "the refusal must name the removal remedy, got: {err}"
    );
    // The read-only adoption path is ALSO gated: it is write-capable (a stale `papertrail sync`
    // registers read-only, then commits rows), so a tombstoned repo must refuse there too.
    schema::register_repo_read_only(conn, &identity, tv_root, 4, &hooks)
        .expect_err("a tombstoned repo must refuse read-only re-registration too");
    // `rag-rat init`'s clear lifts the tombstone → the indexing path works again.
    schema::clear_repo_removed(conn, &identity.repo_id).unwrap();
    assert!(!schema::is_repo_removed(conn, &identity.repo_id).unwrap());
    schema::register_repo(conn, &identity, tv_root, 5, &hooks).unwrap();
}
