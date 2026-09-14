use std::path::{Path, PathBuf};

use rag_rat_base::repo_identity::{self, RepoIdentity, RepoIdentityClass};
use rag_rat_base::test_git;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use super::register_repo;
use crate::hooks::MigrationHooks;
use crate::{meta, schema};

const LIVE_TABLES: &[&str] = &["memory_reality", "memory_note_summaries", "memory_model_failures"];
const STATE_TABLES: &[&str] = &[
    "repos",
    "repo_roots",
    "repo_meta",
    "repo_memories",
    "files",
    "memory_reality",
    "memory_note_summaries",
    "memory_model_failures",
    "memory_summaries",
    "table_sync_streams",
    "sync_published_rows",
    "sync_row_clocks",
    "sync_row_tombstones",
];

const SYNC_TABLES: &[&str] =
    &["table_sync_streams", "sync_published_rows", "sync_row_clocks", "sync_row_tombstones"];

fn git(root: &Path, args: &[&str]) {
    let output = test_git::command(root, args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn identity(root: &Path) -> RepoIdentity {
    repo_identity::resolve_repo_identity(root, None).unwrap()
}

fn register(conn: &Connection, identity: &RepoIdentity, root: &Path) -> rusqlite::Result<String> {
    register_repo(conn, identity, root, 10, &MigrationHooks::noop())
}

struct Fixture {
    _temp: tempfile::TempDir,
    conn: Connection,
    source: PathBuf,
    linked: PathBuf,
    target: PathBuf,
    sibling_root: PathBuf,
    local: RepoIdentity,
    portable: RepoIdentity,
    sibling: RepoIdentity,
}

impl Fixture {
    fn new(occupied: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("upstream");
        std::fs::create_dir(&target).unwrap();
        git(&target, &["init", "--initial-branch=main"]);
        git(&target, &["commit", "--allow-empty", "-m", "root"]);
        git(&target, &["commit", "--allow-empty", "-m", "tip"]);
        let source = temp.path().join("shallow");
        git(temp.path(), &[
            "clone",
            "--depth=1",
            "--no-local",
            target.to_str().unwrap(),
            source.to_str().unwrap(),
        ]);
        let linked = temp.path().join("linked");
        git(&source, &["worktree", "add", "--detach", linked.to_str().unwrap()]);
        let local = identity(&source);
        assert_eq!(local.class, RepoIdentityClass::LocalOnly);
        assert!(!local.shallow_boundary.is_empty());
        assert_eq!(identity(&linked).repo_id, local.repo_id);
        let portable = identity(&target);
        assert_eq!(portable.class, RepoIdentityClass::Portable);
        assert_ne!(portable.repo_id, local.repo_id);

        let sibling_root = temp.path().join("unrelated");
        std::fs::create_dir(&sibling_root).unwrap();
        git(&sibling_root, &["init", "--initial-branch=main"]);
        git(&sibling_root, &["commit", "--allow-empty", "-m", "unrelated root"]);
        let sibling = identity(&sibling_root);
        assert_ne!(sibling.repo_id, portable.repo_id);
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &MigrationHooks::noop()).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        register(&conn, &local, &source).unwrap();
        register(&conn, &local, &linked).unwrap();
        if occupied {
            register(&conn, &portable, &target).unwrap();
            seed(&conn, &portable.repo_id);
        }
        register(&conn, &sibling, &sibling_root).unwrap();
        seed(&conn, &local.repo_id);
        seed(&conn, &sibling.repo_id);
        Self {
            _temp: temp,
            conn,
            source,
            linked,
            target,
            sibling_root,
            local,
            portable,
            sibling,
        }
    }

    fn deepen(&self) {
        git(&self.source, &["fetch", "--unshallow"]);
        for root in [&self.source, &self.linked] {
            let resolved = identity(root);
            assert_eq!(resolved.class, RepoIdentityClass::Portable);
            assert_eq!(resolved.repo_id, self.portable.repo_id);
            assert!(resolved.shallow_boundary.is_empty());
        }
    }

    fn assert_roots(&self, source_owner: &str, occupied: bool) {
        for (root, owner) in [
            (&self.source, source_owner),
            (&self.linked, source_owner),
            (&self.sibling_root, self.sibling.repo_id.as_str()),
        ] {
            assert_root(&self.conn, root, owner);
        }
        if occupied {
            assert_root(&self.conn, &self.target, &self.portable.repo_id);
        }
    }
}

fn assert_root(conn: &Connection, root: &Path, owner: &str) {
    let owners: Vec<String> = conn
        .prepare("SELECT repo_id FROM repo_roots WHERE root = ?1")
        .unwrap()
        .query_map([root.to_str().unwrap()], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(owners, [owner], "root {} has exactly one owner", root.display());
}

fn seed(conn: &Connection, repo: &str) {
    seed_sync(conn, repo);
    conn.execute(
        "INSERT INTO repo_memories(id, repo_id, kind, title, body, confidence, status,
             created_at_ms, updated_at_ms, source, memory_version)
         VALUES (?1, ?1, 'Invariant', 'title', 'body', 'high', 'active', 0, 0, 'agent', 'v1')",
        [repo],
    )
    .unwrap();
    // Both a colliding key and a source-only key: deleting must neither replace the target's
    // winner nor carry the otherwise non-colliding row onto the target.
    for memory in ["shared", repo] {
        conn.execute(
            "INSERT INTO memory_reality(repo_id, memory_id, content_hash, checked_at_ms)
             VALUES (?1, ?2, ?1, 11)",
            params![repo, memory],
        )
        .unwrap();
        for table in ["memory_note_summaries", "memory_summaries"] {
            conn.execute(
                &format!(
                    "INSERT INTO {table}(repo_id, memory_id, content_hash, summary, \
                     generated_at_ms)
                 VALUES (?1, ?2, 'same-hash', ?1, 12)"
                ),
                params![repo, memory],
            )
            .unwrap();
        }
        for pass in ["verify", "compact"] {
            conn.execute(
                "INSERT INTO memory_model_failures(repo_id, memory_id, pass, content_hash,
                     model_id, prompt_version, reason, failed_at_ms)
                 VALUES (?1, ?2, ?3, ?1, 'model', 'v1', 'invalid_output', 13)",
                params![repo, memory, pass],
            )
            .unwrap();
        }
    }
    for worktree in ["", "linked"] {
        conn.execute(
            "INSERT INTO files(repo_id, path, language, kind, sha256, modified_at_ms,
                 indexed_at_ms, commit_sha, worktree_id, generation)
             VALUES (?1, 'shared.rs', 'rust', 'source', ?1, 0, 0, 'head', ?2, 0)",
            params![repo, worktree],
        )
        .unwrap();
    }
}

// Tiny canonical CBOR string/byte-string encoder for the directory and PK fixtures; no signed
// log or account authority is needed to exercise the DB registry's preservation contract.
fn cbor_string(bytes: &mut Vec<u8>, major: u8, value: &[u8]) {
    let len = u8::try_from(value.len()).unwrap();
    if len < 24 {
        bytes.push(major | len);
    } else {
        bytes.extend([major | 24, len]);
    }
    bytes.extend(value);
}

fn seed_sync(conn: &Connection, repo: &str) {
    let account = [7_u8; 32];
    let incarnation = [8_u8; 32];
    let mut context = vec![0x85];
    cbor_string(&mut context, 0x60, b"rag-rat/stream/5");
    cbor_string(&mut context, 0x40, &account);
    cbor_string(&mut context, 0x60, repo.as_bytes());
    cbor_string(&mut context, 0x40, &incarnation);
    cbor_string(&mut context, 0x60, b"overlay/1");
    let stream: [u8; 32] = Sha256::digest(context).into();
    conn.execute(
        "INSERT INTO table_sync_streams(stream_id, repo_id, account_id, incarnation_ref, scope_id)
         VALUES (?1, ?2, ?3, ?4, 'overlay/1')",
        params![stream.as_slice(), repo, account.as_slice(), incarnation.as_slice()],
    )
    .unwrap();
    for table in ["memory_reality", "memory_note_summaries"] {
        for memory in ["shared", repo] {
            let mut pk = vec![0x82];
            cbor_string(&mut pk, 0x60, repo.as_bytes());
            cbor_string(&mut pk, 0x60, memory.as_bytes());
            let pk = rag_rat_base::hash::hex_lower(&pk);
            conn.execute(
                "INSERT INTO sync_published_rows(stream_id, repo_id, table_name, row_pk,
                     synced_hash, spec_version) VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                params![stream.as_slice(), repo, table, pk, "ab".repeat(32)],
            )
            .unwrap();
            // An older delete may coexist with a newer live winner. Both identities must survive.
            for (bookkeeping, lamport) in [("sync_row_clocks", 20), ("sync_row_tombstones", 10)] {
                conn.execute(
                    &format!(
                        "INSERT INTO {bookkeeping}(stream_id, repo_id, table_name, row_pk,
                         lamport, device_fingerprint) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                    ),
                    params![stream.as_slice(), repo, table, pk, lamport, "cd".repeat(32)],
                )
                .unwrap();
            }
        }
    }
}

fn revision(conn: &Connection, repo: &str) -> i64 {
    meta::repo_meta(conn, repo, meta::LENS_MEMORIES_REVISION_META)
        .unwrap()
        .unwrap_or_else(|| "0".into())
        .parse()
        .unwrap()
}

// Compare every column, including payloads and timestamps, rather than only row counts.
fn rows(conn: &Connection, table: &str, repo: Option<&str>) -> Vec<String> {
    let filter = if repo.is_some() { " WHERE repo_id = ?1" } else { "" };
    let mut stmt = conn.prepare(&format!("SELECT * FROM {table}{filter}")).unwrap();
    let columns = stmt.column_count();
    let mut cursor = stmt.query(rusqlite::params_from_iter(repo)).unwrap();
    let mut result = Vec::new();
    while let Some(row) = cursor.next().unwrap() {
        let values: Vec<rusqlite::types::Value> =
            (0..columns).map(|column| row.get(column).unwrap()).collect();
        result.push(format!("{values:?}"));
    }
    result.sort();
    result
}

fn state(conn: &Connection, repo: Option<&str>) -> Vec<Vec<String>> {
    STATE_TABLES.iter().map(|table| rows(conn, table, repo)).collect()
}

#[test]
fn late_merge_discards_live_verification_and_preserves_target_and_sibling() {
    let f = Fixture::new(true);
    let target = state(&f.conn, Some(&f.portable.repo_id));
    let sibling = state(&f.conn, Some(&f.sibling.repo_id));
    let retired = rows(&f.conn, "memory_summaries", None);
    let sync: Vec<_> = SYNC_TABLES.iter().map(|table| rows(&f.conn, table, None)).collect();
    let target_revision = revision(&f.conn, &f.portable.repo_id);
    let sibling_revision = revision(&f.conn, &f.sibling.repo_id);
    f.deepen();
    // Enter through the linked checkout, whose shallow boundary was shared with the main root.
    register(&f.conn, &identity(&f.linked), &f.linked).unwrap();
    assert!(revision(&f.conn, &f.portable.repo_id) > target_revision);
    assert_eq!(revision(&f.conn, &f.sibling.repo_id), sibling_revision);
    for (table, before) in SYNC_TABLES.iter().zip(sync) {
        assert!(!before.is_empty(), "{table} fixture is populated");
        assert_eq!(rows(&f.conn, table, None), before, "{table} survives retirement verbatim");
    }
    for table in LIVE_TABLES {
        assert!(rows(&f.conn, table, Some(&f.local.repo_id)).is_empty(), "{table}");
        let index = STATE_TABLES.iter().position(|name| name == table).unwrap();
        assert_eq!(rows(&f.conn, table, Some(&f.portable.repo_id)), target[index], "{table}");
    }
    assert_eq!(rows(&f.conn, "files", Some(&f.portable.repo_id)), target[4]);
    assert_eq!(state(&f.conn, Some(&f.sibling.repo_id)), sibling);
    assert_eq!(rows(&f.conn, "memory_summaries", None), retired);
    assert!(rows(&f.conn, "repos", Some(&f.local.repo_id)).is_empty());
    let moved: String = f
        .conn
        .query_row("SELECT repo_id FROM repo_memories WHERE id = ?1", [&f.local.repo_id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(moved, f.portable.repo_id);
    f.assert_roots(&f.portable.repo_id, true);
    let after = state(&f.conn, None);
    register(&f.conn, &identity(&f.source), &f.source).unwrap();
    register(&f.conn, &identity(&f.linked), &f.linked).unwrap();
    assert_eq!(state(&f.conn, None), after);
    schema::apply(&f.conn, &MigrationHooks::noop()).unwrap();
    for table in LIVE_TABLES {
        assert!(
            rows(&f.conn, table, Some(&f.local.repo_id)).is_empty(),
            "schema replay must not reseed {table} under the retired id"
        );
    }
    assert_eq!(rows(&f.conn, "memory_summaries", None), retired);
}

#[test]
fn late_merge_verification_cleanup_rolls_back_then_retries() {
    let f = Fixture::new(true);
    f.deepen();
    let before = state(&f.conn, None);
    // This runs after all derived DELETEs, root moves, and authored moves, exercising rollback
    // of the complete merge rather than merely refusing before the first write.
    f.conn
        .execute_batch(
            "CREATE TEMP TRIGGER fail_retirement BEFORE DELETE ON main.repos
         WHEN OLD.repo_id LIKE 'local:%'
         BEGIN SELECT RAISE(ABORT, 'injected retirement failure'); END;",
        )
        .unwrap();
    let error = register(&f.conn, &identity(&f.source), &f.source).unwrap_err();
    assert!(error.to_string().contains("injected retirement failure"), "{error}");
    assert_eq!(state(&f.conn, None), before);
    f.assert_roots(&f.local.repo_id, true);
    f.conn.execute_batch("DROP TRIGGER fail_retirement;").unwrap();
    register(&f.conn, &identity(&f.source), &f.source).unwrap();
    for table in LIVE_TABLES {
        assert!(rows(&f.conn, table, Some(&f.local.repo_id)).is_empty(), "{table}");
    }
    f.assert_roots(&f.portable.repo_id, true);
    let after = state(&f.conn, None);
    register(&f.conn, &identity(&f.source), &f.source).unwrap();
    assert_eq!(state(&f.conn, None), after);
}

#[test]
fn in_place_adoption_carries_live_verification_and_leaves_retired_summaries() {
    let f = Fixture::new(false);
    let sibling = state(&f.conn, Some(&f.sibling.repo_id));
    let retired = rows(&f.conn, "memory_summaries", None);
    let before: Vec<_> =
        LIVE_TABLES.iter().map(|table| rows(&f.conn, table, Some(&f.local.repo_id))).collect();
    f.deepen();
    register(&f.conn, &identity(&f.source), &f.source).unwrap();
    for (table, original) in LIVE_TABLES.iter().zip(before) {
        assert!(rows(&f.conn, table, Some(&f.local.repo_id)).is_empty(), "{table}");
        // Normalize only the repo_id column; payloads deliberately contain the old id too.
        let mut stmt =
            f.conn.prepare(&format!("SELECT * FROM {table} WHERE repo_id = ?1")).unwrap();
        let repo_column = stmt.column_index("repo_id").unwrap();
        let columns = stmt.column_count();
        let mut normalized: Vec<String> = stmt
            .query_map([&f.portable.repo_id], |row| {
                let mut values = (0..columns)
                    .map(|column| row.get(column))
                    .collect::<rusqlite::Result<Vec<rusqlite::types::Value>>>()?;
                values[repo_column] = f.local.repo_id.clone().into();
                Ok(format!("{values:?}"))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        normalized.sort();
        assert_eq!(normalized, original, "{table} preserves every non-scope column");
    }
    assert_eq!(state(&f.conn, Some(&f.sibling.repo_id)), sibling);
    assert_eq!(rows(&f.conn, "memory_summaries", None), retired);
    f.assert_roots(&f.portable.repo_id, false);
}
