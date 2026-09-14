//! Registry migrations, adoption, identity upgrades, and checkout resolution.

use rag_rat_base::repo_identity::{LEGACY_REPO_ID, RepoIdentity, RepoIdentityClass};
use rag_rat_db::schema::{self, register_repo};

use super::*;

fn identity(repo_id: &str, display_name: &str) -> RepoIdentity {
    RepoIdentity {
        repo_id: repo_id.to_string(),
        display_name: display_name.to_string(),
        // The class is a scoping-neutral tag; ADOPTION of a placeholder / refusal of a second repo
        // ignore it — only the LocalOnly→Portable upgrade branch reads it (see `identity_local`).
        class: RepoIdentityClass::Portable,
        shallow_boundary: Vec::new(),
    }
}

/// A machine-local (`LocalOnly`) identity — a `local:`-prefixed id, as a cut shallow clone derives.
/// `register_repo` refuses one against an existing real repo (a deepened clone must not DOWNGRADE a
/// portable id), and only UPGRADES *away* from one when an incoming `Portable` id arrives — after
/// PROVING the deepened clone reaches the recorded `shallow_boundary` (pass the boundary commits a
/// later portable clone must reach; empty ⇒ no proof recorded ⇒ the upgrade is refused).
fn identity_local(
    repo_id: &str,
    display_name: &str,
    shallow_boundary: Vec<String>,
) -> RepoIdentity {
    RepoIdentity {
        repo_id: repo_id.to_string(),
        display_name: display_name.to_string(),
        class: RepoIdentityClass::LocalOnly,
        shallow_boundary,
    }
}

fn repo_row_count(conn: &rusqlite::Connection, repo_id: &str) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM repos WHERE repo_id = ?1", [repo_id], |r| r.get(0))
        .unwrap()
}

fn root_count(conn: &rusqlite::Connection, repo_id: &str) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM repo_roots WHERE repo_id = ?1", [repo_id], |r| r.get(0))
        .unwrap()
}

/// Upsert a key into a `(key, value)` k/v table (`index_meta` / `reconcile_meta`) — seeds the
/// pre-relocation state a legacy DB carries.
fn upsert_meta(conn: &rusqlite::Connection, table: &str, key: &str, value: &str) {
    conn.execute(
        &format!(
            "INSERT INTO {table}(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
        ),
        [key, value],
    )
    .unwrap();
}

/// Whether `table` still holds `key` (used to assert relocated keys are gone / retained keys stay).
fn meta_present(conn: &rusqlite::Connection, table: &str, key: &str) -> bool {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE key = ?1"), [key], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap()
        > 0
}

// --- V040: repo_id scoping on the core tables (memory-sync phase A3) ---

/// The pre-V040 (post-V039) shape of every core table
/// [`schema::migrations::apply_repo_id_core_scoping`] transforms, plus the V038 registry it
/// relocates meta under — built in ISOLATION so the migration is exercised against its own inputs,
/// not the full ladder (the directory's "assert deferred absence / rebuild behavior in isolation"
/// rule). `files`/`packages`/… carry NO `repo_id`; `parser_failures` is id-keyed; `git_commits` is
/// `hash`-PK with `commit_fts` external content and a `git_file_changes(commit_hash)` FK — exactly
/// what V040 rebuilds.
fn seed_pre_v040_core_schema(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "
        CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE files(
            id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL, language TEXT NOT NULL,
            kind TEXT NOT NULL, sha256 TEXT NOT NULL, modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0, indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT '', commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '', has_test_code INTEGER NOT NULL DEFAULT 0,
            UNIQUE(path, commit_sha, worktree_id));
        CREATE TABLE packages(
            id INTEGER PRIMARY KEY AUTOINCREMENT, manifest_dir TEXT NOT NULL,
            commit_sha TEXT NOT NULL DEFAULT '', worktree_id TEXT NOT NULL DEFAULT '',
            local_roots_json TEXT NOT NULL DEFAULT '[]',
            UNIQUE(manifest_dir, commit_sha, worktree_id)) STRICT;
        CREATE TABLE logical_symbols(
            id INTEGER PRIMARY KEY AUTOINCREMENT, language TEXT NOT NULL, path TEXT NOT NULL,
            logical_name TEXT NOT NULL, qualified_name_id INTEGER, kind TEXT NOT NULL,
            variant_count INTEGER NOT NULL, group_reason TEXT NOT NULL);
        CREATE TABLE docs(
            id INTEGER PRIMARY KEY AUTOINCREMENT, chunk_id INTEGER NOT NULL,
            source_kind TEXT NOT NULL, heading_path TEXT);
        CREATE TABLE parser_failures(
            id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL, language TEXT NOT NULL,
            message TEXT NOT NULL);
        CREATE TABLE git_commits(
            hash TEXT PRIMARY KEY, author_name TEXT NOT NULL, author_email TEXT NOT NULL,
            authored_at_s INTEGER NOT NULL, committed_at_s INTEGER NOT NULL, subject TEXT NOT NULL,
            body TEXT NOT NULL, changed_file_count INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE git_file_changes(
            id INTEGER PRIMARY KEY AUTOINCREMENT, commit_hash TEXT NOT NULL, path TEXT NOT NULL,
            additions INTEGER, deletions INTEGER, change_kind TEXT NOT NULL DEFAULT 'modified',
            FOREIGN KEY(commit_hash) REFERENCES git_commits(hash) ON DELETE CASCADE);
        CREATE VIRTUAL TABLE commit_fts USING fts5(
            subject, body, content='git_commits', content_rowid='rowid', tokenize='porter');
        -- The logical-symbol companion tables a real pre-V040 DB carries (all baseline). V040's
        -- logical-symbol id realign (repo_id fold) reads name_strings / symbols /
        -- logical_symbol_members for the hash inputs and re-points logical_symbol_monikers +
        -- repo_memory_bindings / repo_memory_call_paths; the tables must exist for that pass to
        -- PREPARE even when (as here) logical_symbols is empty, so it is a no-op.
        CREATE TABLE name_strings(id INTEGER PRIMARY KEY, value TEXT NOT NULL UNIQUE) STRICT;
        CREATE TABLE symbols(id INTEGER PRIMARY KEY AUTOINCREMENT, signature TEXT);
        CREATE TABLE logical_symbol_members(
            logical_symbol_id INTEGER NOT NULL, symbol_id INTEGER NOT NULL, cfg_expr TEXT,
            signature_hash TEXT, start_line INTEGER NOT NULL DEFAULT 0,
            end_line INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(logical_symbol_id, symbol_id),
            -- The real ON DELETE CASCADE FK: it is exactly what forces the id realign to run with \
         FK
            -- OFF (V040) or DEFERRED (adoption), so the fixture carries it to exercise those \
         paths.
            FOREIGN KEY(logical_symbol_id) REFERENCES logical_symbols(id) ON DELETE CASCADE);
        CREATE TABLE logical_symbol_monikers(
            logical_symbol_id INTEGER NOT NULL, tool TEXT NOT NULL, tool_version TEXT NOT NULL,
            moniker TEXT NOT NULL, computed_at INTEGER NOT NULL,
            PRIMARY KEY(logical_symbol_id, tool)) STRICT;
        CREATE TABLE repo_memory_bindings(
            memory_id TEXT NOT NULL, binding_kind TEXT NOT NULL, binding_id TEXT NOT NULL,
            logical_symbol_id INTEGER, PRIMARY KEY(memory_id, binding_kind, binding_id));
        CREATE TABLE repo_memory_call_paths(
            memory_id TEXT NOT NULL, start_logical_symbol_id INTEGER, end_logical_symbol_id \
         INTEGER,
            edge_sequence_hash TEXT NOT NULL, PRIMARY KEY(memory_id, edge_sequence_hash));
        ",
    )
    .unwrap();
    schema::migrations::apply_repos_registry(conn).expect("V038 registry seeds the placeholder");
}

// --- Logical-symbol id realign across the repo_id fold (A3, #413 finding #1) ---

/// Seed ONE logical symbol (`logical_id`) with a fully-recoverable key — a member symbol carrying
/// the signature, an interned qualified name — plus every kind of reference that must follow it: a
/// per-tool moniker, a memory binding, and a memory call-path (start + end). The next realign (V040
/// or adoption) re-derives the id under the folded hash and must carry ALL of them along.
fn seed_pre_v040_logical_symbol_with_a_bound_memory(conn: &rusqlite::Connection, logical_id: i64) {
    conn.execute("INSERT INTO name_strings(id, value) VALUES (1, 'mymod::my_fn')", []).unwrap();
    conn.execute("INSERT INTO symbols(id, signature) VALUES (100, 'fn my_fn()')", []).unwrap();
    conn.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind, \
         variant_count, group_reason)
         VALUES (?1, 'rust', 'src/lib.rs', 'my_fn', 1, 'function', 1, 'single')",
        [logical_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, end_line) \
         VALUES (?1, 100, 1, 3)",
        [logical_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, moniker, \
         computed_at) VALUES (?1, 'scip-rust', 'v1', 'moniker', 0)",
        [logical_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, logical_symbol_id) \
         VALUES ('mem-1', 'logical_symbol', 'b1', ?1)",
        [logical_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, start_logical_symbol_id, \
         end_logical_symbol_id, edge_sequence_hash) VALUES ('mem-1', ?1, ?1, 'h1')",
        [logical_id],
    )
    .unwrap();
}

/// The `logical_symbol_id` every reference table now points at (binding / call-path start+end /
/// moniker / member), asserted equal so a single value proves they all followed the realign.
fn bound_logical_symbol_id(conn: &rusqlite::Connection) -> i64 {
    let binding: i64 = conn
        .query_row(
            "SELECT logical_symbol_id FROM repo_memory_bindings WHERE memory_id = 'mem-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for (sql, label) in [
        ("SELECT logical_symbol_id FROM logical_symbol_members", "member"),
        ("SELECT logical_symbol_id FROM logical_symbol_monikers", "moniker"),
        ("SELECT start_logical_symbol_id FROM repo_memory_call_paths", "call-path start"),
        ("SELECT end_logical_symbol_id FROM repo_memory_call_paths", "call-path end"),
    ] {
        let other: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap();
        assert_eq!(other, binding, "the {label} reference must match the binding after realign");
    }
    // "Resolves to the SAME symbol": the id joins to a live logical symbol with the seeded content.
    let name: String = conn
        .query_row(
            "SELECT ls.logical_name FROM repo_memory_bindings b
               JOIN logical_symbols ls ON ls.id = b.logical_symbol_id
              WHERE b.memory_id = 'mem-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name, "my_fn", "the bound memory resolves to the same logical symbol");
    binding
}

mod adoption;
mod identity_upgrade;
mod resolution;
mod v038_registry;
mod v039_repo_meta;
mod v040_core_scoping;
mod v041_papertrail;
mod v042_periphery;
