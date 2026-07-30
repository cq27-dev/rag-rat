//! #828 — the incrementally-maintained `content_revision` digest: schema tip, trigger contract,
//! seam parity, multiset semantics, the fast-path-disagrees poison pin, fail-closed skew, and the
//! migration re-stamp. The raw-schema tests drive `main.files` directly (fast, isolates the
//! trigger mechanics against the full ladder's `files` shape); the real-`IndexDatabase` tests drive
//! the production seams (rebuild, gc) end to end.

use rag_rat_db::content_digest::{content_row_hash, encode_state, fold_row};

use super::*;

// ---- raw-schema helpers (in-memory, full ladder) ----

fn apply_schema() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    conn
}

fn insert_file(conn: &rusqlite::Connection, path: &str, sha: &str, kind: &str, generation: i64) {
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation)
         VALUES (?1, 'rust', ?2, ?3, 0, 0, '', '', 'r', ?4)",
        rusqlite::params![path, kind, sha, generation],
    )
    .unwrap();
}

/// The independent from-scratch fold the triggers must always agree with.
fn digest_scan(conn: &rusqlite::Connection) -> (String, i64) {
    let mut state = [0u64; 4];
    let mut count = 0i64;
    let mut stmt =
        conn.prepare("SELECT path, sha256 FROM main.files WHERE kind != 'deleted'").unwrap();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        let path: String = row.get(0).unwrap();
        let sha256: String = row.get(1).unwrap();
        fold_row(&mut state, &content_row_hash(&path, &sha256), true);
        count += 1;
    }
    (encode_state(&state), count)
}

fn digest_stored(conn: &rusqlite::Connection) -> (String, i64) {
    conn.query_row("SELECT state, rows_folded FROM content_digest_state WHERE id = 1", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .unwrap()
}

fn assert_trigger_parity(conn: &rusqlite::Connection) {
    assert_eq!(
        digest_stored(conn),
        digest_scan(conn),
        "trigger-maintained content_digest_state must equal a from-scratch scan fold"
    );
}

fn object_exists(conn: &rusqlite::Connection, kind: &str, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
        rusqlite::params![kind, name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

const FOLD_TRIGGERS: [&str; 3] =
    ["files_content_digest_ai", "files_content_digest_ad", "files_content_digest_au"];

// ---- schema tip + trigger contract ----

/// V086 creates `content_digest_state` (seeded empty on a fresh ladder) and the three fold
/// triggers. The absolute schema-tip pin has moved forward to the newest migration's test
/// (V088, `migration_088_caches_the_generation_posting_row_count`); this keeps only the symbolic
/// "schema at LATEST after apply" check per the ladder convention.
#[test]
fn content_digest_state_has_live_fold_triggers() {
    let conn = apply_schema();
    assert_eq!(schema::status(&conn).unwrap().current_version, schema::LATEST_SCHEMA_VERSION);

    let (state, rows) = digest_stored(&conn);
    assert_eq!(state, "0".repeat(64), "a fresh empty corpus seeds the all-zero state");
    assert_eq!(rows, 0);
    for trigger in FOLD_TRIGGERS {
        assert!(object_exists(&conn, "trigger", trigger), "{trigger} exists after the full ladder");
    }
}

/// §8/§9.2(4) recreation contract: a `files`-table REBUILD (the V040/V043 create/copy/drop/rename
/// pattern) silently drops the row triggers — the tripwire that pins why a future rebuild migration
/// MUST call `ensure_content_digest` again.
#[test]
fn a_files_table_rebuild_drops_the_fold_triggers_and_ensure_recreates_them() {
    let conn = apply_schema();
    insert_file(&conn, "a.rs", "aa", "source", 1);
    for trigger in FOLD_TRIGGERS {
        assert!(object_exists(&conn, "trigger", trigger));
    }

    // V040-style rebuild. `DROP TABLE files` fires no row triggers but drops the triggers ON it.
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE files_new AS SELECT * FROM files;
         DROP TABLE files;
         ALTER TABLE files_new RENAME TO files;",
    )
    .unwrap();
    for trigger in FOLD_TRIGGERS {
        assert!(
            !object_exists(&conn, "trigger", trigger),
            "{trigger} is dropped by a files-table rebuild — the hazard the contract addresses"
        );
    }

    // The shared helper a rebuild migration must call restores them.
    rag_rat_db::content_digest::ensure_content_digest(&conn).unwrap();
    for trigger in FOLD_TRIGGERS {
        assert!(object_exists(&conn, "trigger", trigger), "{trigger} recreated by ensure");
    }
}

// ---- seam parity + multiset semantics (raw schema) ----

#[test]
fn triggers_hold_parity_through_insert_tombstone_flip_delete_and_purge() {
    let conn = apply_schema();
    insert_file(&conn, "a.rs", "aa", "source", 1);
    insert_file(&conn, "b.rs", "bb", "docs", 1);
    assert_trigger_parity(&conn);
    assert_eq!(digest_stored(&conn).1, 2);

    // Tombstone insert (seam #2 insert arm) is excluded.
    insert_file(&conn, "c.rs", "", "deleted", 1);
    assert_trigger_parity(&conn);
    assert_eq!(digest_stored(&conn).1, 2);

    // sha change (incremental reindex ≈ delete+reinsert).
    conn.execute("UPDATE main.files SET sha256 = 'aa2' WHERE path = 'a.rs'", []).unwrap();
    assert_trigger_parity(&conn);

    // Tombstone flip both arms (seam #2 upsert DO UPDATE): source -> deleted -> source.
    conn.execute("UPDATE main.files SET kind = 'deleted', sha256 = '' WHERE path = 'b.rs'", [])
        .unwrap();
    assert_trigger_parity(&conn);
    assert_eq!(digest_stored(&conn).1, 1);
    conn.execute("UPDATE main.files SET kind = 'docs', sha256 = 'bb3' WHERE path = 'b.rs'", [])
        .unwrap();
    assert_trigger_parity(&conn);
    assert_eq!(digest_stored(&conn).1, 2);

    // generated-flag flip (seam #5) is digest-neutral (UPDATE OF path,sha256,kind skips it).
    let before = digest_stored(&conn);
    conn.execute("UPDATE main.files SET generated = 1 WHERE path = 'a.rs'", []).unwrap();
    assert_eq!(digest_stored(&conn), before, "a generated-flag flip does not touch the digest");
    assert_trigger_parity(&conn);

    // Delete a real row (seam #3), then delete a tombstone (digest-neutral).
    conn.execute("DELETE FROM main.files WHERE path = 'a.rs'", []).unwrap();
    assert_trigger_parity(&conn);
    conn.execute("DELETE FROM main.files WHERE path = 'c.rs'", []).unwrap();
    assert_trigger_parity(&conn);

    // The rag-rat-db dynamic purge sweep (seam #9) — the seam a Rust-seam design would miss.
    rag_rat_db::schema::purge_repo_rows(&conn, "r").unwrap();
    assert_trigger_parity(&conn);
    assert_eq!(digest_stored(&conn).1, 0, "purging the repo removes every member");
    assert_eq!(digest_stored(&conn).0, "0".repeat(64));
}

/// Multiset pins: two identical `(path, sha256)` rows at different generations do NOT cancel (the
/// XOR-regression pin) and add+remove of the same pair returns exactly to the prior value (the
/// counter/content-stability pin). Digest is invariant under insert order / rowid.
#[test]
fn digest_is_a_multiset_not_a_set_and_is_order_invariant() {
    let conn = apply_schema();
    insert_file(&conn, "dup.rs", "same", "source", 1);
    let one = digest_stored(&conn);

    // A second identical (path, sha256) row at a different generation (the staged-rebuild shape).
    insert_file(&conn, "dup.rs", "same", "source", 2);
    let two = digest_stored(&conn);
    assert_ne!(two, one, "a duplicate pair moves the digest (XOR would cancel it to the prior)");
    assert_ne!(two.0, "0".repeat(64), "two identical rows are NOT the empty state (XOR pin)");
    assert_trigger_parity(&conn);

    // Remove the duplicate: back to the single-copy digest exactly.
    conn.execute("DELETE FROM main.files WHERE path = 'dup.rs' AND generation = 2", []).unwrap();
    assert_eq!(digest_stored(&conn), one, "add+remove of the same pair is a no-op on the digest");

    // Order / rowid invariance: a second DB inserting the same multiset in the opposite order and
    // at different rowids lands on the identical digest.
    let other = apply_schema();
    insert_file(&other, "z.rs", "zz", "source", 7);
    insert_file(&other, "a.rs", "aa", "source", 3);
    let reverse = apply_schema();
    insert_file(&reverse, "a.rs", "aa", "source", 9);
    insert_file(&reverse, "z.rs", "zz", "source", 4);
    assert_eq!(
        digest_stored(&other).0,
        digest_stored(&reverse).0,
        "identical row multisets give identical digests regardless of order/rowid/generation"
    );
}

/// §9.2(5) fail-closed skew: a `files` write on a connection WITHOUT the fold registered errors
/// loudly and rolls back, so version skew cannot silently drift the digest. (The db-level test
/// covers this against the minimal table; this one is against the full ladder.)
#[test]
fn a_files_write_without_the_fold_function_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("skew.sqlite");
    {
        let setup = rag_rat_db::storage::IndexConnection::open(&path).unwrap();
        rag_rat_db::schema::apply(setup.connection(), &crate::index::migration_hooks()).unwrap();
    }
    // A bare connection that never registered the fold: the delete/insert trigger's function
    // reference is unresolved.
    let unregistered = rusqlite::Connection::open(&path).unwrap();
    // Full column set, so the only thing that can fail is the trigger's unresolved function (the
    // trigger program is compiled at statement-prepare, before NOT NULL checks).
    let err = unregistered
        .execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id, generation)
             VALUES ('a.rs', 'rust', 'source', 'aa', 0, 0, '', '', 'r', 1)",
            [],
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("no such function"),
        "an unregistered writer must fail closed, got: {err}"
    );
    let count: i64 = unregistered
        .query_row("SELECT rows_folded FROM content_digest_state WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "the rolled-back INSERT left the digest untouched");
}

// ---- real IndexDatabase seams (rebuild / gc / poison / migration re-stamp) ----

/// Build a tiny committed git repo and return its config (the base for the real-DB seam tests).
fn digest_repo_config(tag: &str) -> (ScratchRoot, Config) {
    let root = ScratchRoot::new(tag);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "init"]);
    let config = source_config(root.clone(), Language::Rust);
    (root, config)
}

#[test]
fn content_revision_is_content_stable_across_rebuilds_and_moves_on_edits() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let (root, config) = digest_repo_config("digest-stable");

    let db = IndexDatabase::rebuild(&config).unwrap();
    let d1 = db.content_revision().unwrap();
    assert!(d1.starts_with("ms1-"), "rendered digest is ms1-prefixed: {d1}");
    assert_eq!(d1, db.content_revision_from_scan().unwrap(), "O(1) read == from-scratch scan");
    drop(db);

    // Content-identical rebuild: mid-window the staged generation and the still-live old one
    // double the multiset (the digest correctly reflects that — a full-table scan does too), so
    // the stability property is asserted AFTER gc sweeps the dead generation. Once swept, the
    // byte-identical corpus lands back on the exact prior digest (the counter-regression pin — a
    // version counter would bump on every rebuild and spuriously invalidate FTS/clone stamps).
    let db = IndexDatabase::rebuild(&config).unwrap();
    // gc runs the DeadGeneration + DeadContext sweeps and the parity self-check; parity holds
    // throughout.
    assert_eq!(db.content_revision().unwrap(), db.content_revision_from_scan().unwrap());
    db.garbage_collect().unwrap();
    assert_eq!(db.content_revision().unwrap(), db.content_revision_from_scan().unwrap());
    assert_eq!(
        db.content_revision().unwrap(),
        d1,
        "after gc sweeps the dead generation, a content-identical rebuild is digest-stable"
    );
    drop(db);

    // A content edit moves the digest.
    fs::write(root.join("src/lib.rs"), "pub fn a() -> u8 { 1 }\n").unwrap();
    let db = IndexDatabase::rebuild(&config).unwrap();
    let d2 = db.content_revision().unwrap();
    assert_ne!(d2, d1, "a content edit moves the digest");
    assert_eq!(d2, db.content_revision_from_scan().unwrap());

    let _ = fs::remove_dir_all(&root);
}

/// The repo's fast-path-disagrees-with-fallback pin (§9.2(3)): poison the O(1) state and assert
/// `content_revision()` returns the POISON (proving it reads the state, not the scan), then the
/// parity self-check heals it in place.
#[test]
fn poisoned_state_is_read_verbatim_then_parity_heals() {
    let _poison = crate::index::poison_sibling::disable_poison_sibling();
    let (root, config) = digest_repo_config("digest-poison");
    let db = IndexDatabase::rebuild(&config).unwrap();
    let real = db.content_revision().unwrap();

    // A valid-format but WRONG state (a scan would never produce it).
    let poison = "1".repeat(64);
    db.storage
        .connection()
        .execute("UPDATE content_digest_state SET state = ?1 WHERE id = 1", [&poison])
        .unwrap();
    assert_eq!(
        db.content_revision().unwrap(),
        format!("ms1-{poison}"),
        "content_revision() returns the poisoned state — proving the O(1) read, not the scan"
    );
    assert_ne!(db.content_revision().unwrap(), real);

    db.verify_content_digest_parity().unwrap();
    assert_eq!(db.content_revision().unwrap(), real, "parity reseeded the drifted state");
    assert_eq!(db.content_revision().unwrap(), db.content_revision_from_scan().unwrap());

    let _ = fs::remove_dir_all(&root);
}

/// §9.2(6) migration re-stamp: a stamp equal to the FROZEN legacy digest is pointed at the new
/// rendered digest (no first-use FTS/clone rebuild); a stamp that was already stale is left for the
/// normal freshness machinery. Driven on a raw connection (no scoped temp `files` view — the shape
/// the migration actually runs under) by re-running the idempotent applier.
#[test]
fn migration_restamps_fresh_legacy_stamps_but_leaves_stale_ones() {
    let conn = apply_schema();
    // repo_meta.repo_id REFERENCES repos (files.repo_id does not), so register the repo before
    // arming a quiet candidate for it.
    conn.execute(
        "INSERT OR IGNORE INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', '', 0)",
        [],
    )
    .unwrap();
    insert_file(&conn, "a.rs", "aa", "source", 1);
    insert_file(&conn, "b.rs", "bb", "source", 1);

    // The frozen pre-#828 legacy digest over the current corpus.
    let legacy_concat: String = conn
        .query_row(
            "SELECT COALESCE(group_concat(pv, ','), '') FROM (SELECT path || ':' || sha256 AS pv \
             FROM main.files WHERE kind != 'deleted' ORDER BY path)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let legacy = rag_rat_base::hash::hex_sha256(legacy_concat.as_bytes());
    // The new rendered digest the applier seeds/stamps: "ms1-" + the from-scratch scan state.
    let new_digest = format!("ms1-{}", digest_scan(&conn).0);

    // Simulate a pre-#828 store: FRESH stamps sit at the legacy digest (a clone generation and an
    // armed quiet candidate too); plus deliberately STALE controls that must NOT be re-stamped.
    conn.execute(
        "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('fts_source_revision', ?1)",
        [&legacy],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO index_meta(key, value) VALUES ('content_revision', ?1)",
        [&legacy],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_graph_generations(generation, status, theta_floor, normalizer_kind, \
         normalizer_version, source_revision, started_at_ms)
         VALUES (990, 'Complete', 0.7, 'baseline', 1, ?1, 0)",
        [&legacy],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO clone_graph_generations(generation, status, theta_floor, normalizer_kind, \
         normalizer_version, source_revision, started_at_ms)
         VALUES (991, 'Complete', 0.7, 'baseline', 1, 'already-stale-rev', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO repo_meta(repo_id, key, value)
         VALUES ('r', 'clone_graph_quiet_candidate_revision', ?1)",
        [&legacy],
    )
    .unwrap();

    // Re-run the applier (idempotent): re-seeds (files unchanged) and re-stamps legacy -> new.
    rag_rat_db::schema::migrations::apply_content_digest_state(&conn).unwrap();

    let meta = |key: &str| -> String {
        conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |r| r.get(0)).unwrap()
    };
    assert_eq!(meta("fts_source_revision"), new_digest, "a fresh legacy FTS stamp is re-stamped");
    assert_eq!(meta("content_revision"), new_digest, "the stored content_revision is re-stamped");
    let gen_rev = |g: i64| -> String {
        conn.query_row(
            "SELECT source_revision FROM clone_graph_generations WHERE generation = ?1",
            [g],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(gen_rev(990), new_digest, "a clone generation at the legacy digest is re-stamped");
    assert_eq!(
        gen_rev(991),
        "already-stale-rev",
        "an already-stale stamp is left for the freshness machinery"
    );
    let quiet: String = conn
        .query_row(
            "SELECT value FROM repo_meta WHERE key = 'clone_graph_quiet_candidate_revision'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(quiet, new_digest, "the armed quiet-window candidate survives the upgrade");
}
