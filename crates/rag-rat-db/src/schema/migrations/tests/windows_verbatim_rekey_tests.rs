use super::*;

/// The stand-in spelling rule: a blind prefix strip. Used for the ROW-WALKING assertions —
/// which columns and which meta keys the sweep reaches — so those stay legible against a rule
/// with one obvious answer per input, and so the two halves fail separately. Which spellings
/// the PRODUCTION rule declines is `paths`' concern and is asserted there;
/// `the_production_pass_rekeys_a_windows_store_on_any_host` covers the two composed.
fn strip_verbatim(stored: &str) -> Option<String> {
    stored.strip_prefix(r"\\?\").map(str::to_string)
}

/// A store shaped like a pre-V097 Windows index: every scope key and recorded root spelled the
/// way `std::fs::canonicalize` used to answer.
fn poisoned_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        r"
            CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT NOT NULL, commit_sha TEXT NOT NULL
                DEFAULT '', worktree_id TEXT NOT NULL DEFAULT '',
                UNIQUE(path, commit_sha, worktree_id));
            CREATE TABLE packages(manifest_dir TEXT NOT NULL, commit_sha TEXT NOT NULL DEFAULT '',
                worktree_id TEXT NOT NULL DEFAULT '');
            CREATE TABLE oracle_runs(id INTEGER PRIMARY KEY, worktree_id TEXT NOT NULL DEFAULT '');
            CREATE TABLE external_symbols(name TEXT NOT NULL, worktree_id TEXT NOT NULL);
            CREATE TABLE repo_roots(repo_id TEXT NOT NULL, root TEXT NOT NULL,
                PRIMARY KEY(repo_id, root));
            CREATE TABLE repo_meta(repo_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
                PRIMARY KEY(repo_id, key));
            CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);

            INSERT INTO files(path, commit_sha, worktree_id)
                VALUES ('src/a.rs', 'headsha', '\\?\C:\repo'),
                       ('src/b.rs', '', '\\?\C:\linked');
            INSERT INTO packages VALUES ('crate', 'headsha', '\\?\C:\repo');
            INSERT INTO oracle_runs(worktree_id) VALUES ('\\?\C:\repo');
            INSERT INTO external_symbols VALUES ('Ext', '\\?\C:\repo');
            INSERT INTO repo_roots VALUES ('repo-1', '\\?\C:\repo');
            INSERT INTO repo_meta VALUES ('repo-1', 'source_root', '\\?\C:\repo');
            INSERT INTO repo_meta VALUES ('repo-1', 'git_history_indexed_root', '\\?\C:\repo');
            -- The reload cursor's SIBLING key, given a value the rule would happily rewrite so a
            -- blanket value-sweep would visibly corrupt it. In a real store this holds a commit
            -- hash; the sweep must be scoped to the keys that carry a path.
            INSERT INTO repo_meta VALUES ('repo-1', 'git_history_indexed_head', '\\?\C:\repo');
            INSERT INTO repo_meta VALUES
                ('repo-1', 'worktree_overlay_basis:\\?\C:\linked',
                 'base' || char(10) || 'linked' || char(10) || '42');
            INSERT INTO index_meta VALUES ('source_root', '\\?\C:\repo');
            ",
    )
    .unwrap();
    conn
}

fn scalar(conn: &Connection, sql: &str) -> String {
    conn.query_row(sql, [], |row| row.get::<_, String>(0)).unwrap()
}

/// The whole class in one pass: every table whose `worktree_id` names a checkout, the recorded
/// root behind the empty-index guard, the `source_root` fallback beside it, the git-history
/// reload cursor's root, and the overlay basis whose worktree identity lives in the KEY — plus
/// the negative half, a meta key that is NOT a path staying put.
///
/// Against the unfixed state (no migration at all) every one of these still reads the verbatim
/// spelling, which is precisely the state where the active scope selects nothing and GC prunes
/// the rows as dead.
#[test]
fn the_rekey_covers_every_persisted_path_spelling() {
    let conn = poisoned_store();
    rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();

    for (table, column) in [
        ("files", "worktree_id"),
        ("packages", "worktree_id"),
        ("oracle_runs", "worktree_id"),
        ("external_symbols", "worktree_id"),
    ] {
        let remaining: i64 = conn
            .query_row(
                &format!(r"SELECT COUNT(*) FROM {table} WHERE substr({column}, 1, 4) = '\\?\'"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "{table}.{column} still holds a verbatim spelling");
    }
    assert_eq!(
        scalar(&conn, "SELECT worktree_id FROM files WHERE path = 'src/a.rs'"),
        r"C:\repo",
        "the base index's scope key is rekeyed, not just the overlay's",
    );
    assert_eq!(
        scalar(&conn, "SELECT worktree_id FROM files WHERE path = 'src/b.rs'"),
        r"C:\linked",
    );
    assert_eq!(scalar(&conn, "SELECT root FROM repo_roots"), r"C:\repo");
    assert_eq!(scalar(&conn, "SELECT value FROM repo_meta WHERE key = 'source_root'"), r"C:\repo",);
    assert_eq!(scalar(&conn, "SELECT value FROM index_meta WHERE key = 'source_root'"), r"C:\repo",);
    assert_eq!(
        scalar(&conn, "SELECT value FROM repo_meta WHERE key = 'git_history_indexed_root'"),
        r"C:\repo",
        "the git-history reload cursor is a ROOT PATH, not a commit hash — left stale it fails \
         the freshness comparison and forces a full revwalk plus a blame-cache wipe",
    );
    assert_eq!(
        scalar(&conn, "SELECT value FROM repo_meta WHERE key = 'git_history_indexed_head'"),
        r"\\?\C:\repo",
        "a meta key that does not carry a path is left alone — the sweep is key-scoped, not a \
         blanket rewrite of every value that happens to start with those bytes",
    );
    assert_eq!(
        scalar(&conn, "SELECT key FROM repo_meta WHERE key LIKE 'worktree_overlay_basis:%'",),
        r"worktree_overlay_basis:C:\linked",
        "the basis key carries the worktree identity, so the KEY must move with it",
    );
    assert_eq!(
        scalar(&conn, "SELECT value FROM repo_meta WHERE key LIKE 'worktree_overlay_basis:%'",),
        "base\nlinked\n42",
        "the basis payload rides the key move intact",
    );
}

/// Re-running the pass changes nothing — the ladder can replay it, and a store already written
/// by a fixed binary must come through untouched.
#[test]
fn the_rekey_is_idempotent() {
    let conn = poisoned_store();
    rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();
    let snapshot = scalar(&conn, "SELECT group_concat(worktree_id, '|') FROM files ORDER BY id");
    rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();
    assert_eq!(
        scalar(&conn, "SELECT group_concat(worktree_id, '|') FROM files ORDER BY id"),
        snapshot,
    );
}

/// A spelling the rule declines to rewrite is LEFT ALONE. This is the half a blind `\\?\`
/// strip would get wrong: verbatim form is still produced for UNC shares and reserved names,
/// and rewriting one would break the very match the migration exists to preserve.
#[test]
fn a_spelling_the_rule_declines_is_untouched() {
    let conn = poisoned_store();
    rekey_persisted_path_spellings(&conn, |_| None).unwrap();
    assert_eq!(
        scalar(&conn, "SELECT worktree_id FROM files WHERE path = 'src/a.rs'"),
        r"\\?\C:\repo",
        "a declined rewrite must not be applied anyway",
    );
    assert_eq!(scalar(&conn, "SELECT root FROM repo_roots"), r"\\?\C:\repo");
}

/// A mixed-binary store already holds the plain spelling for one of the rows being rekeyed.
/// The pass must not abort the upgrade on the unique constraint, and must not cascade the live
/// plain-spelled row away: it keeps that row and leaves the superseded verbatim duplicate for
/// GC.
#[test]
fn a_colliding_plain_row_survives_the_rekey() {
    let conn = poisoned_store();
    conn.execute(
            r"INSERT INTO files(path, commit_sha, worktree_id) VALUES ('src/a.rs', 'headsha', 'C:\repo')",
            [],
        )
        .unwrap();
    rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();
    let live: i64 = conn
        .query_row(
            r"SELECT COUNT(*) FROM files WHERE path = 'src/a.rs' AND worktree_id = 'C:\repo'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(live, 1, "the row production just wrote is intact, not replaced or cascaded");
}

/// The real entry point, end-to-end, on WHICHEVER platform runs it — deliberately not
/// `cfg`-gated.
///
/// Which spellings a store holds is a property of the store, not of the host that opens it: one
/// repository directory reachable from both a Windows path and a WSL/container mount is one
/// SQLite file, and whichever binary opens first is the one that runs the ladder. A pass that
/// converted only on Windows would let a non-Windows opener stamp V097 as applied without
/// converting anything, and the forward-only ladder would never revisit it — leaving the
/// Windows binary with exactly the spellings whose rows its next GC prunes as a dead checkout.
///
/// So this asserts the conversion on Unix too, where the host-gated version returned before
/// opening the transaction and left every value below verbatim.
#[test]
fn the_production_pass_rekeys_a_windows_store_on_any_host() {
    let conn = poisoned_store();
    apply_windows_verbatim_path_rekey(&conn).unwrap();
    assert_eq!(scalar(&conn, "SELECT worktree_id FROM files WHERE path = 'src/a.rs'"), r"C:\repo",);
    assert_eq!(scalar(&conn, "SELECT root FROM repo_roots"), r"C:\repo");
    assert_eq!(
        scalar(&conn, "SELECT value FROM repo_meta WHERE key = 'git_history_indexed_root'"),
        r"C:\repo",
    );
    assert_eq!(
        scalar(&conn, "SELECT key FROM repo_meta WHERE key LIKE 'worktree_overlay_basis:%'"),
        r"worktree_overlay_basis:C:\linked",
    );
}

/// A store that predates a covered table (a partial-schema bootstrap fixture) must not abort
/// the ladder — every sweep is guarded on the column actually being there.
#[test]
fn a_missing_table_is_skipped_rather_than_failing() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        r"CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT NOT NULL,
                  worktree_id TEXT NOT NULL DEFAULT '');
              INSERT INTO files(path, worktree_id) VALUES ('a.rs', '\\?\C:\repo');",
    )
    .unwrap();
    rekey_persisted_path_spellings(&conn, strip_verbatim).unwrap();
    assert_eq!(scalar(&conn, "SELECT worktree_id FROM files"), r"C:\repo");
}
