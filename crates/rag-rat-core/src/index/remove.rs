//! `rag-rat rm <path>`: fully de-index a repo from the consolidated global store. The row-level
//! purge (which tables, in which order) lives in `rag_rat_db::schema::purge`; this module is the
//! ORCHESTRATION — resolve a filesystem path to a registered repo, count what would be deleted (the
//! confirmation preview + `--dry-run`), and drive the destructive sequence: purge in one IMMEDIATE
//! transaction, run the caller's on-disk deconfigure step, then VACUUM. The deconfigure step itself
//! (delete `rag-rat.toml`, uninstall git hooks) is a filesystem closure the CLI shim passes in;
//! [`purge_and_vacuum`] runs it at the right point in the locked sequence.
//!
//! LOCK + ATOMICITY: the repo's [`write_lock_path`](rag_rat_base::locks) flock (the same lock
//! `index` / `gc` / `consolidate` take) is held across the WHOLE sequence — purge, deconfigure, and
//! VACUUM — so no watcher/index pass can append to (or re-register) the repo mid-removal. The purge
//! is a single IMMEDIATE transaction so a failure rolls the store back rather than leaving it
//! half-removed; VACUUM runs on a fresh connection (it cannot run inside the purge transaction) but
//! still under the flock, so the repo is fully de-indexed AND deconfigured before any writer
//! resumes.

use std::path::{Path, PathBuf};

use rag_rat_base::locks::{self, WriteLock};
use rag_rat_db::schema::{self, RepoRowCounts};
use rag_rat_db::storage::IndexConnection;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::index::{FreelistReclaim, FreelistReclaimReport, reclaim_freelist_at};

/// How long the purge waits for the repo's write lock before giving up. `rm` is interactive, so a
/// bounded wait surfaces an actionable "a writer is busy" error rather than blocking forever.
const REMOVE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A registered repo resolved from a filesystem path — the target of a removal.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolvedRepo {
    pub repo_id: String,
    pub display_name: Option<String>,
    /// Every recorded working-tree root for the repo (a repo can be checked out in several
    /// worktrees), so the caller can report exactly what it is about to remove.
    pub roots: Vec<String>,
    /// The path the removal resolved through — canonicalized when it exists, else the path as
    /// given (a deleted / moved working tree resolved by its recorded root). Informational only
    /// (reported in `--dry-run`); the on-disk deconfigure targets `roots` (the recorded governing
    /// roots), not this — the arg path may be a subdirectory / linked worktree without the config.
    pub resolved_root: PathBuf,
}

/// The read-only preview of a removal: the resolved repo plus a per-table count of everything the
/// purge would delete. Drives both the confirmation summary and `--dry-run` (which stops here).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RemovePlan {
    pub repo: ResolvedRepo,
    pub counts: RepoRowCounts,
}

/// The result of an executed removal: what was purged and what VACUUM reclaimed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RemoveOutcome {
    pub repo_id: String,
    pub display_name: Option<String>,
    /// Exact rows deleted (recounted under the write lock immediately before the purge).
    pub purged_rows: i64,
    /// The VACUUM reclaim report, or `None` when VACUUM did not complete (see `vacuum_skipped`).
    pub vacuum: Option<FreelistReclaimReport>,
    /// Why VACUUM did not run (a concurrent writer held the file busy) — the purge is still fully
    /// committed; this is a best-effort space reclaim the operator can retry with `doctor
    /// --vacuum`.
    pub vacuum_skipped: Option<String>,
}

/// Resolve a filesystem `path` to a REGISTERED repo that can be removed, or `None` when the path
/// maps to no registered repo. Resolution is DELIBERATELY STRICT — it never falls back to "the sole
/// repo" the way the read-path resolver does: for a destructive `rm <path>`, resolving an arbitrary
/// unregistered path to the only registered repo would offer to delete the wrong repo. Routes, in
/// order:
///  1. by derived git IDENTITY — if `path` is a git worktree whose content-derived id is a
///     registered repo, that id (this also resolves a LINKED worktree, whose path is not the
///     recorded root but derives the same id);
///  2. by EXACT `repo_roots` match on the canonical path, then the path as given — so a repo whose
///     working tree was DELETED / moved / is non-git (identity underivable), or was registered
///     under a pinned `[index] repo_id`, still resolves by its recorded root.
///
/// Read-only. `path` is canonicalized when it exists on disk; a non-existent path (a deleted /
/// moved working tree) falls back to its ABSOLUTE lexically-normalized form
/// ([`std::path::absolute`], CWD-relative, filesystem-free) so a RELATIVE argument to a gone tree
/// (e.g. `./old-repo`) still matches the absolute root `repo_roots` recorded — not the relative
/// string, which never would.
pub fn resolve_removable_repo(
    conn: &Connection,
    path: &Path,
) -> anyhow::Result<Option<ResolvedRepo>> {
    let canonical = path
        .canonicalize()
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf());

    // Route 1: a derivable git identity that is a REGISTERED repo. The identity is content-derived
    // from the git repo AT this path, so a match is unambiguous — no sole-repo guessing. A pinned
    // `[index] repo_id` is intentionally NOT read here (we have no config for an arbitrary path);
    // route 2's recorded-root match covers a pinned registration.
    if let Ok(identity) = rag_rat_base::repo_identity::resolve_repo_identity(&canonical, None)
        && schema::repo_id_is_registered(conn, &identity.repo_id)?
    {
        return Ok(Some(load_resolved_repo(conn, identity.repo_id, canonical)?));
    }

    // Route 2: an exact recorded-root match — a physical path belongs to exactly one repo, and
    // registration always records the root (#427). Try the canonical form first (registration
    // records the canonicalized root), then the raw path as a courtesy.
    let mut seen = std::collections::BTreeSet::new();
    for candidate in [canonical.to_string_lossy().to_string(), path.to_string_lossy().to_string()] {
        if !seen.insert(candidate.clone()) {
            continue;
        }
        if let Some(repo_id) = recorded_root_owner(conn, &candidate)? {
            return Ok(Some(load_resolved_repo(conn, repo_id, canonical)?));
        }
    }
    Ok(None)
}

/// The repo that recorded `root` in `repo_roots` (a physical path belongs to exactly one repo), or
/// `None`. Read-only.
fn recorded_root_owner(conn: &Connection, root: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT repo_id FROM repo_roots WHERE root = ?1 LIMIT 1", [root], |row| {
        row.get::<_, String>(0)
    })
    .optional()
}

/// Load the registry facts (display name + recorded roots) for a resolved `repo_id`.
fn load_resolved_repo(
    conn: &Connection,
    repo_id: String,
    resolved_root: PathBuf,
) -> rusqlite::Result<ResolvedRepo> {
    let display_name: Option<String> = conn
        .query_row("SELECT display_name FROM repos WHERE repo_id = ?1", [&repo_id], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    let mut roots_stmt =
        conn.prepare("SELECT root FROM repo_roots WHERE repo_id = ?1 ORDER BY root")?;
    let roots = roots_stmt
        .query_map([&repo_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ResolvedRepo { repo_id, display_name, roots, resolved_root })
}

/// Build the read-only removal plan: the resolved repo + the per-table count of what the purge
/// would delete. Read-only, so it is safe to run for `--dry-run` and to render before prompting.
pub fn plan_remove(conn: &Connection, repo: ResolvedRepo) -> anyhow::Result<RemovePlan> {
    let counts = schema::count_repo_rows(conn, &repo.repo_id)?;
    Ok(RemovePlan { repo, counts })
}

/// Execute the removal: purge every row `repo_id` owns in ONE IMMEDIATE transaction, run the
/// caller's `deconfigure` step (delete `rag-rat.toml` / uninstall hooks), then VACUUM — the WHOLE
/// sequence under the repo's per-repo write lock. DESTRUCTIVE — the caller has already confirmed
/// (or passed `--yes`). Returns the outcome plus whatever `deconfigure` produced (the CLI's cleanup
/// report).
///
/// LOCK SPAN (the re-registration guard): the write lock is held across purge → deconfigure →
/// VACUUM, not just the purge. Deconfiguration is what makes the repo un-re-indexable (a keyless
/// `rag-rat index` needs a `rag-rat.toml`), so it must land BEFORE any writer waiting on the lock
/// can resume — otherwise a concurrent index/maintenance pass could re-register and repopulate the
/// just-purged repo during the (seconds-long) VACUUM window. Holding the lock through VACUUM also
/// keeps that window closed.
///
/// ORDER (`deconfigure` before VACUUM): the purge has already committed, so the on-disk
/// deconfiguration must run regardless of the VACUUM outcome. A busy VACUUM mutated nothing and
/// downgrades to `vacuum_skipped`; a post-VACUUM failure (commit_fts rebuild / checkpoint)
/// propagates as `Err` — but by then the purge AND the deconfiguration have both happened, so the
/// operator just reclaims/repairs later with `doctor --vacuum`.
pub fn purge_and_vacuum<C>(
    database: &Path,
    repo_id: &str,
    now_ms: i64,
    deconfigure: impl FnOnce() -> C,
) -> anyhow::Result<(RemoveOutcome, C)> {
    // One lock for the whole operation. Acquired FIRST (per-repo before the global schema lock the
    // VACUUM takes — the per-repo → global ordering rule).
    let _lock =
        WriteLock::acquire_timeout(database, repo_id, REMOVE_LOCK_TIMEOUT)?.ok_or_else(|| {
            anyhow::anyhow!(
                "timed out waiting for the repo's write lock — an index or maintenance pass is \
                 running; re-run `rag-rat rm` once it finishes"
            )
        })?;

    // ALSO exclude a concurrent papertrail autosync flight. Autosync deliberately does NOT take the
    // repo write lock (its commits are short lockless transactions serialized by SQLite), so
    // without this a flight in progress could commit `papertrail_*` rows AFTER the purge
    // deletes them — rows keyed by the removed `repo_id` that survive a "removed" result. Its
    // own flight lock waits for any in-flight sync to drain and blocks a new one; combined with
    // the deconfigure below (a new flight can't reload a deleted config), the repo's papertrail
    // stays purged. The two lock sets are disjoint (autosync never takes the write lock;
    // index/rm never take the flight lock), so acquiring both cannot deadlock.
    let _papertrail_lock = locks::FileLock::acquire_timeout(
        &locks::papertrail_lock_path(database, repo_id),
        REMOVE_LOCK_TIMEOUT,
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "timed out waiting for a papertrail sync flight to finish — re-run `rag-rat rm` once \
             it drains"
        )
    })?;

    // Recount + purge under the lock so the reported `purged_rows` is exact and nothing appends to
    // the repo between the count and the delete.
    let (purged_rows, display_name) = {
        let storage = IndexConnection::open(database)?;
        let conn = storage.connection();
        let display_name: Option<String> = conn
            .query_row("SELECT display_name FROM repos WHERE repo_id = ?1", [repo_id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        let purged_rows = schema::count_repo_rows(conn, repo_id)?.total_rows;
        // ONE IMMEDIATE transaction: all-or-nothing. The write lock is already held, and IMMEDIATE
        // takes the SQLite write lock up front rather than upgrading mid-transaction.
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        schema::purge_repo_rows(&tx, repo_id)?;
        // Tombstone the repo in the SAME transaction. The marker lives in `index_meta` (which the
        // purge sweep never touches), so it survives the removal that writes it and lets
        // `register_repo` refuse a writer that resumes after the lock releases with a stale
        // in-memory config. Only `rag-rat init` clears it.
        schema::mark_repo_removed(&tx, repo_id, now_ms)?;
        tx.commit()?;
        (purged_rows, display_name)
        // `storage` (the purge connection) drops here, releasing its SQLite handles before VACUUM
        // opens its own fresh connection — but the write lock is STILL held below.
    };

    // Deconfigure UNDER the lock, after the committed purge and BEFORE the VACUUM: this is the step
    // that makes the repo un-re-indexable, so it must land before any writer can resume, and it
    // must run regardless of the VACUUM outcome.
    let deconfigured = deconfigure();

    // VACUUM last, still under the write lock, on a fresh connection under the schema lock. A busy
    // VACUUM mutated nothing → `vacuum_skipped`; a post-VACUUM failure propagates (`?`) — the purge
    // and deconfiguration already committed, so the operator repairs/reclaims later with `doctor
    // --vacuum`.
    let (vacuum, vacuum_skipped) = match reclaim_freelist_at(database)? {
        FreelistReclaim::Reclaimed(report) => (Some(report), None),
        FreelistReclaim::BusySkipped(message) => (None, Some(message)),
    };

    Ok((
        RemoveOutcome {
            repo_id: repo_id.to_string(),
            display_name,
            purged_rows,
            vacuum,
            vacuum_skipped,
        },
        deconfigured,
    ))
}

/// The per-repo write-lock file for `repo_id` beside `database` — exposed so a caller can report
/// the lock path in a timeout diagnostic if it needs to.
pub fn remove_write_lock_path(database: &Path, repo_id: &str) -> PathBuf {
    locks::write_lock_path(database, repo_id)
}

#[cfg(test)]
mod tests {
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
    ) {
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
             input_hash, embedding_text_version, embedding_policy, embedding_priority, \
             input_chars, input_truncated, embedding_dim, vector_blob, status, attempt_count, \
             created_at_ms)
             VALUES (?1, 'm', 'v', ?2, ?2, 'etv', 'eligible', 0, 1, 0, 1, x'00', 'ready', 0, 0)",
            params![chunk_id, format!("sth_{tag}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunk_summaries(chunk_id, model_id, prompt_version, input_hash, \
             text_hash, summary, status, attempt_count)
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
    }

    /// Seed the memory- and clone-generation transitive children the poison harness does not seed,
    /// hung off the poison sibling's memory + clone generation, so purging the sibling exercises
    /// (and the orphan check pins) every transitive child.
    fn seed_memory_and_clone_children(
        conn: &rusqlite::Connection,
        memory_id: &str,
        generation: i64,
    ) {
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
    ) {
        let file_in = in_clause(files);
        let chunk_in = in_clause(chunks);
        let symbol_in = in_clause(symbols);
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
                "purge left {count} orphaned row(s) in transitive child `{table}` (WHERE {column} \
                 IN the removed repo's ids) — add it to TRANSITIVE_SCOPED_TABLES",
            );
        }
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
        seed_chunk_symbol_children(conn, "b", b_chunk, b_symbol);
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
                "SELECT id FROM symbols WHERE file_id IN (SELECT id FROM files WHERE repo_id = \
                 ?1) LIMIT 1",
                params![&repo_a],
                |row| row.get(0),
            )
            .unwrap();
        seed_chunk_symbol_children(conn, "a", a_chunk, a_symbol);

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
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
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
        let repo_a = {
            let storage = IndexConnection::open(&config.database).unwrap();
            fixture_repo_id(storage.connection())
        };
        drop(db);

        let (outcome, cleanup) =
            purge_and_vacuum(&config.database, &repo_a, 12_345, || "deconfigured".to_string())
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
        assert!(
            schema::is_repo_removed(conn, &repo_a).unwrap(),
            "the removed repo must be tombstoned"
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
        // The READ-ONLY adoption path is NOT gated (a read of a now-empty repo is harmless).
        schema::register_repo_read_only(conn, &identity, tv_root, 4, &hooks).unwrap();
        // `rag-rat init`'s clear lifts the tombstone → the indexing path works again.
        schema::clear_repo_removed(conn, &identity.repo_id).unwrap();
        assert!(!schema::is_repo_removed(conn, &identity.repo_id).unwrap());
        schema::register_repo(conn, &identity, tv_root, 5, &hooks).unwrap();
    }
}
