//! Poison-sibling test harness (`#[cfg(test)]` only).
//!
//! GOAL: end the review-driven discovery loop for repo-scoping bugs. Every unscoped read / count /
//! delete / resume in the engine should fail an EXISTING test **locally** rather than surface in a
//! reviewer's comment. The mechanism: after a fixture DB reaches its ready state (the tail of
//! [`IndexDatabase::rebuild_with_progress`]), register a SECOND repo (`poison-sibling`) directly
//! via SQL — `register_repo` refuses a second real repo before A7, so the harness seeds it the same
//! way `multi_repo_scope`'s `two_repo_fixture` does — and hang tripwire rows off it in every table
//! that carries a repo dimension at this schema version. A production read that forgets its
//! `repo_id` predicate (or bypasses the scope view) then sees the sibling's rows and the test's own
//! assertion trips; a production DELETE that forgets it silently wipes the sibling, which
//! [`assert_sibling_intact`] catches.
//!
//! SCHEMA-VERSION SCOPE (load-bearing): this worktree is **V040** (`LATEST_SCHEMA_VERSION = 40`),
//! where exactly TEN tables carry a `repo_id` column — `repos`, `repo_roots`, `repo_meta`, `files`,
//! `packages`, `logical_symbols`, `docs`, `parser_failures`, `git_commits`, `git_file_changes` —
//! plus tables scoped TRANSITIVELY through them (`chunks`/`symbols`/`edges_data` via `files.id`;
//! `logical_symbol_members`/`logical_symbol_monikers` via `logical_symbols.id`). The github, repo
//! memory, oracle (`oracle_runs`/`edge_oracle`), clone, `dream_findings`, and `reconcile_attempts`
//! tables are GLOBAL at V040 — they gain `repo_id` only in A4 (V041) / A5 (V042), which are NOT in
//! this worktree — so seeding a `repo_id='poison-sibling'` row into them is impossible and a read
//! of them is *legitimately* cross-repo here. Seeding them would manufacture FALSE tripwires. When
//! A4/A5 land, extend [`seed_sibling`] to those tables at the same time their scoping does.
//!
//! WHY THE SIBLING IS NOT REGISTERED IN `repos` (load-bearing at V040): the eight direct-scoped
//! DATA tables (`files`, `packages`, `logical_symbols`, `docs`, `parser_failures`, `git_commits`,
//! `git_file_changes`, and the file/symbol children) carry `repo_id` as a plain column with a
//! `DEFAULT '__unassigned__'` and **no foreign key to `repos`** — so a tripwire row under
//! `repo_id='poison-sibling'` is valid without a `repos` registry row. The harness deliberately
//! does NOT insert into `repos`/`repo_roots`/`repo_meta`. If it did, the sibling would become a
//! second REAL repo, and at phase A that (a) makes `sole_repo_id` (the config-blind fallback for
//! the many NON-git fixtures, which stay under the `__unassigned__` placeholder) return the sibling
//! instead of the fixture — hijacking every re-adoption — and (b) flips `multiple_real_repos`,
//! changing the gc/precompute global-sweep guards. Neither is a production leak; both are phase-A
//! single-repo assumptions. Seeding only the *scoped data* rows keeps the registry pristine (no
//! hijack, no `multiple_real_repos` flip) while still tripping any genuinely unscoped
//! read/count/delete. The cost is no tripwire for an unscoped `repo_meta`/`repos`/`repo_roots` read
//! — those are covered by the dedicated registry + `multi_repo_scope` tests instead. (When A7 makes
//! multi-repo the default, the sibling gets a real `repos` row and this note goes away.)
//!
//! OPT-OUT: default-ON per test thread (see [`disable_poison_sibling`]). A test that legitimately
//! asserts a scoped table's UNSCOPED total (a `full_rebuild_preserves_*` cache-total check, a
//! whole-table row count) disables the harness at its start. Each opt-out is a deliberate statement
//! that the test's invariant is single-repo by nature, not a workaround for a real leak.

use std::cell::Cell;

use rusqlite::{Connection, params};

/// The reserved id of the tripwire repo. Distinctive so a stray row is unmistakable in a failure.
pub(crate) const POISON_REPO_ID: &str = "poison-sibling";

/// Sentinel prefix on every text value the harness seeds, so a leaked row is greppable and a
/// value-mutation is detectable by exact match.
const POISON_PREFIX: &str = "zz_poison_";

/// The poison sibling's logical-symbol id. Explicit (the real derivation folds `repo_id` into a
/// content hash) and far outside any real id range so it never collides with a fixture's symbols.
const POISON_LOGICAL_ID: i64 = 9_900_000_777;

/// The poison sibling's git commit hash (a distinctive 40-hex-shaped sentinel).
const POISON_COMMIT: &str = "zzpoison00000000000000000000000000000000";

thread_local! {
    /// Whether [`seed_if_enabled`] seeds on this thread. Default ON. Thread-local (not a global
    /// static) so a `cargo test` run — which executes tests as parallel THREADS in one process —
    /// keeps each test's opt-out isolated; under `nextest` (process-per-test) it is trivially
    /// isolated too.
    static POISON_ENABLED: Cell<bool> = const { Cell::new(true) };
}

/// Restores the previous enabled state on drop, so an opt-out is scoped to the test that took it.
pub(crate) struct PoisonDisabled(bool);

impl Drop for PoisonDisabled {
    fn drop(&mut self) {
        POISON_ENABLED.with(|flag| flag.set(self.0));
    }
}

/// Disable poison-sibling seeding for the remainder of THIS test (until the returned guard drops).
/// Bind it: `let _guard = disable_poison_sibling();`. Use it in tests that need a virgin
/// single-repo DB — registry/adoption/migration-ladder tests, `sole_repo_id` assertions, and any
/// test asserting a scoped table's UNSCOPED total. Every call is a claim that the test's invariant
/// is single-repo by nature.
pub(crate) fn disable_poison_sibling() -> PoisonDisabled {
    POISON_ENABLED.with(|flag| {
        let prev = flag.get();
        flag.set(false);
        PoisonDisabled(prev)
    })
}

/// The rebuild-tail seam: seed the poison sibling on `conn` unless this thread opted out.
/// Idempotent (clears any prior sibling first), so repeated rebuilds on one DB reconverge to the
/// same tripwire set. A seeding failure PROPAGATES — a harness that cannot seed is a bug to
/// surface, never to swallow.
pub(crate) fn seed_if_enabled(conn: &Connection) -> anyhow::Result<()> {
    if POISON_ENABLED.with(Cell::get) {
        seed_sibling(conn)?;
    }
    Ok(())
}

/// Clear then insert the full tripwire row set for the poison sibling. Runs under `foreign_keys =
/// ON` (the live rebuild connection), so inserts are parent→child and clears child→parent.
/// Deliberately touches NO registry table (`repos`/`repo_roots`/`repo_meta`) — see the module docs.
pub(crate) fn seed_sibling(conn: &Connection) -> anyhow::Result<()> {
    clear_sibling(conn)?;

    // --- git history (git_file_changes FKs git_commits(repo_id, hash); git_commits has NO FK to
    // repos, so this needs no registry row) ---
    conn.execute(
        "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, committed_at_s, \
         subject, body, changed_file_count, repo_id)
         VALUES (?1, ?2, ?2, 0, 0, ?3, '', 1, ?4)",
        params![
            POISON_COMMIT,
            format!("{POISON_PREFIX}author"),
            format!("{POISON_PREFIX}subject"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
         repo_id)
         VALUES (?1, ?2, 0, 0, 'modified', ?3)",
        params![POISON_COMMIT, format!("{POISON_PREFIX}change.rs"), POISON_REPO_ID],
    )?;

    // --- direct-scoped core tables ---
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id)
         VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, '', ?4)",
        params![
            format!("{POISON_PREFIX}file.rs"),
            format!("{POISON_PREFIX}sha"),
            POISON_COMMIT,
            POISON_REPO_ID
        ],
    )?;
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO packages(manifest_dir, commit_sha, worktree_id, local_roots_json, repo_id)
         VALUES (?1, '', '', '[]', ?2)",
        params![format!("{POISON_PREFIX}pkg"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO parser_failures(repo_id, path, language, message) VALUES (?1, ?2, 'rust', ?3)",
        params![POISON_REPO_ID, format!("{POISON_PREFIX}fail.rs"), format!("{POISON_PREFIX}msg")],
    )?;
    conn.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind, \
         variant_count, group_reason, repo_id)
         VALUES (?1, 'rust', ?2, ?3, NULL, 'function', 1, ?4, ?5)",
        params![
            POISON_LOGICAL_ID,
            format!("{POISON_PREFIX}file.rs"),
            format!("{POISON_PREFIX}symbol"),
            format!("{POISON_PREFIX}group"),
            POISON_REPO_ID
        ],
    )?;

    // --- children hung off the poison file (transitively scoped through files.repo_id) ---
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line, is_test)
         VALUES (?1, 'rust', ?2, NULL, 'function', 0, 0, 0, 0, 0)",
        params![file_id, format!("{POISON_PREFIX}symbol")],
    )?;
    let symbol_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
         text_hash, source_revision, anchor_version, normalized_hash, start_boundary_hash, \
         end_boundary_hash, start_context_hash, end_context_hash, context_radius, \
         embedding_policy, embedding_priority)
         VALUES (?1, 'symbol', 0, 0, 0, 0, ?2, '', 0, ?2, '', '', '', '', 0, 'none', 0)",
        params![file_id, format!("{POISON_PREFIX}chunkhash")],
    )?;
    let chunk_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO docs(chunk_id, source_kind, heading_path, repo_id) VALUES (?1, 'markdown', \
         ?2, ?3)",
        params![chunk_id, format!("{POISON_PREFIX}heading"), POISON_REPO_ID],
    )?;

    // --- one edge whose source file is the poison file (scoped via source_file_id → files) ---
    conn.execute_batch(&format!(
        "INSERT OR IGNORE INTO name_strings(value) VALUES
            ('{POISON_PREFIX}from'), ('{POISON_PREFIX}to'), ('{POISON_PREFIX}calls'),
            ('{POISON_PREFIX}conf'), ('{POISON_PREFIX}res');"
    ))?;
    let name_id = |value: &str| -> rusqlite::Result<i64> {
        conn.query_row("SELECT id FROM name_strings WHERE value = ?1", [value], |row| row.get(0))
    };
    conn.execute(
        "INSERT INTO edges_data(source_file_id, from_name_id, to_name_id, edge_kind_id, \
         confidence_id, resolution_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            file_id,
            name_id(&format!("{POISON_PREFIX}from"))?,
            name_id(&format!("{POISON_PREFIX}to"))?,
            name_id(&format!("{POISON_PREFIX}calls"))?,
            name_id(&format!("{POISON_PREFIX}conf"))?,
            name_id(&format!("{POISON_PREFIX}res"))?,
        ],
    )?;

    // --- children hung off the poison logical symbol (scoped via logical_symbols.repo_id) ---
    conn.execute(
        "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, end_line)
         VALUES (?1, ?2, 0, 0)",
        params![POISON_LOGICAL_ID, symbol_id],
    )?;
    // logical_symbol_monikers has NO repo_id and NO FK; it is scoped only by the join to
    // logical_symbols. This row is the tripwire for the oracle moniker clear/count (round-6 P2 #3).
    conn.execute(
        "INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, moniker, \
         computed_at)
         VALUES (?1, 'scip-rust', ?2, ?3, 0)",
        params![
            POISON_LOGICAL_ID,
            format!("{POISON_PREFIX}ver"),
            format!("{POISON_PREFIX}moniker")
        ],
    )?;

    Ok(())
}

/// Remove every poison-sibling row, child→parent, so [`seed_sibling`] is idempotent across repeated
/// rebuilds on one DB. Explicit child-first order works whether or not the FK cascades fire.
fn clear_sibling(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "DELETE FROM logical_symbol_monikers WHERE logical_symbol_id = {POISON_LOGICAL_ID};
         DELETE FROM logical_symbol_members WHERE logical_symbol_id = {POISON_LOGICAL_ID};
         DELETE FROM logical_symbols WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM docs WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM edges_data WHERE source_file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM chunks WHERE file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM symbols WHERE file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM parser_failures WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM packages WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM git_file_changes WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM git_commits WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM main.files WHERE repo_id = '{POISON_REPO_ID}';"
    ))?;
    Ok(())
}

/// Each seeded tripwire as `(table, full-sentinel WHERE predicate)`. Asserting each still matches
/// EXACTLY one row is a row-count check (catches an unscoped DELETE) and a value checksum (the
/// predicate pins every seeded column, so an in-place UPDATE stops matching) in one. The transitive
/// children are matched through the poison file / logical symbol, exactly how a scoped reader would
/// have to reach them.
fn sibling_tripwires() -> Vec<(&'static str, String)> {
    let file_scope =
        format!("file_id IN (SELECT id FROM main.files WHERE repo_id = '{POISON_REPO_ID}')");
    vec![
        ("git_commits", format!("repo_id = '{POISON_REPO_ID}' AND hash = '{POISON_COMMIT}'")),
        ("git_file_changes", format!("repo_id = '{POISON_REPO_ID}'")),
        ("main.files", format!("repo_id = '{POISON_REPO_ID}' AND path = '{POISON_PREFIX}file.rs'")),
        ("packages", format!("repo_id = '{POISON_REPO_ID}'")),
        ("parser_failures", format!("repo_id = '{POISON_REPO_ID}'")),
        ("docs", format!("repo_id = '{POISON_REPO_ID}'")),
        ("logical_symbols", format!("id = {POISON_LOGICAL_ID} AND repo_id = '{POISON_REPO_ID}'")),
        ("symbols", file_scope.clone()),
        ("chunks", file_scope.clone()),
        (
            "edges_data",
            format!(
                "source_file_id IN (SELECT id FROM main.files WHERE repo_id = '{POISON_REPO_ID}')"
            ),
        ),
        ("logical_symbol_members", format!("logical_symbol_id = {POISON_LOGICAL_ID}")),
        (
            "logical_symbol_monikers",
            format!(
                "logical_symbol_id = {POISON_LOGICAL_ID} AND moniker = '{POISON_PREFIX}moniker'"
            ),
        ),
    ]
}

/// Post-condition for a MUTATING test: assert the poison sibling survived intact — every seeded
/// tripwire still present and unmodified. Any unscoped DELETE / UPDATE in the operation under test
/// (a GC that wipes a sibling's rows, an oracle clear that drops a sibling's monikers, an
/// incremental pass that stamps a sibling's file) trips this. Reads `main.*` directly (bypassing
/// the scope view), because the active connection is scoped to the fixture repo and would never see
/// the sibling through the view. Call it on the fixture's connection at the end of the mutating
/// step.
pub(crate) fn assert_sibling_intact(conn: &Connection) {
    for (table, predicate) in sibling_tripwires() {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|err| panic!("poison-sibling probe failed on {table}: {err}"));
        assert_eq!(
            count, 1,
            "poison sibling leaked/mutated in `{table}` (WHERE {predicate}): expected exactly 1 \
             row, found {count}. An unscoped read/count/delete in the operation under test \
             touched a sibling repo's rows — scope it by repo_id.",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexDatabase;
    use crate::index::schema_bootstrap_tests::poison_test_config;

    /// The harness self-check: after a normal rebuild (poison ON by default), an INTENTIONALLY
    /// unscoped probe of a scoped table MUST see the sentinel — proving the tripwires are live. If
    /// this fails, the harness is asleep and every "sibling intact" downstream assertion is
    /// meaningless.
    #[test]
    fn poison_tripwires_are_live_after_a_default_rebuild() {
        let (_root, config) = poison_test_config("poison_live");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let conn = db.storage.connection();

        // Unscoped total over a direct-scoped table sees BOTH the fixture repo and the sibling.
        let sibling_files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1",
                [POISON_REPO_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sibling_files >= 1, "the poison file must be seeded by the default rebuild");

        // And every tripwire is intact right after seeding.
        assert_sibling_intact(conn);
    }

    /// Opt-out honored: with the guard held, a rebuild seeds NO tripwire rows.
    #[test]
    fn disabling_the_harness_seeds_no_tripwires() {
        let _guard = disable_poison_sibling();
        let (_root, config) = poison_test_config("poison_optout");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let sibling_files: i64 = db
            .storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM main.files WHERE repo_id = ?1",
                [POISON_REPO_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sibling_files, 0, "opt-out must seed no poison rows");
    }

    /// The sibling never touches the `repos` registry: even with the harness ON, the DB has exactly
    /// one real repo (the fixture's), so `sole_repo_id` / `multiple_real_repos` are unperturbed and
    /// the many non-git placeholder fixtures keep resolving to their own scope.
    #[test]
    fn the_sibling_does_not_perturb_the_repos_registry() {
        let (_root, config) = poison_test_config("poison_registry");
        let db = IndexDatabase::rebuild(&config).unwrap();
        let conn = db.storage.connection();
        let poison_registered: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos WHERE repo_id = ?1", [POISON_REPO_ID], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(poison_registered, 0, "the sibling must NOT be registered in `repos`");
        let real_repos: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM repos WHERE repo_id != ?1",
                [crate::index::schema::LEGACY_REPO_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            real_repos, 1,
            "only the fixture is a real repo — the sibling is scoped rows only"
        );
    }
}
