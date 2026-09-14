use super::*;

/// The full HEAD commit hash — a commit reachable from HEAD, used as a recorded shallow boundary.
fn head_commit_hash(root: &Path) -> String {
    rag_rat_base::test_git::output(root, &["rev-parse", "HEAD"])
}

/// A real git repo (two empty commits) for the upgrade-proof tests — its HEAD is a commit a genuine
/// deepened clone would reach.
fn real_git_repo(tag: &str) -> ScratchRoot {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["commit", "-q", "--allow-empty", "-m", &format!("{tag}-one")]);
    run_git(&root, &["commit", "-q", "--allow-empty", "-m", &format!("{tag}-two")]);
    root
}

// --- LocalOnly → Portable id upgrade (deepened shallow clone; #413 round-4 finding #4) ---

/// A DB first indexed under a machine-local `local:` id (a cut shallow clone) must UPGRADE in place
/// when the caller deepens it (`git fetch --unshallow` — our own remedy) and re-opens under a
/// portable id: every scoped row, `repo_meta`, `repo_roots`, and logical-symbol id re-points from
/// the local id to the portable one, and a bound memory survives (its `logical_symbol_id` follows
/// the realign). Without this the deepened clone would hit the "different real repo" refusal and
/// the existing index could never open again without deletion — a dead end.
#[test]
fn register_repo_upgrades_a_local_only_id_to_a_portable_id_in_place() {
    // A REAL git repo supplies the upgrade PROOF (round-6 P2 #4): the incoming deepened clone's
    // HEAD must reach the incumbent's recorded shallow boundary. The DB (in-memory) and the
    // repo are decoupled — register_repo re-points rows in `conn` while verifying ancestry
    // against `repo`.
    let repo = real_git_repo("upgrade-in-place");
    let boundary = head_commit_hash(&repo);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v040_core_schema(&conn);
    // A file + a logical symbol with a bound memory — the rows the upgrade must re-point + realign.
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms) VALUES \
         ('src/lib.rs', 'rust', 'source', 'a', 0, 0)",
        [],
    )
    .unwrap();
    seed_pre_v040_logical_symbol_with_a_bound_memory(&conn, 777_001);

    schema::migrations::apply_repo_id_core_scoping(&conn, &crate::index::migration_hooks())
        .expect("V040 applies (unadopted → placeholder id)");

    // First registration: a cut shallow clone adopts under a machine-local id AND records its
    // shallow boundary (the proof material a later upgrade verifies against).
    let local_id = "local:deadbeefcafef00d";
    register_repo(
        &conn,
        &identity_local(local_id, "shallow", vec![boundary]),
        repo.as_path(),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), local_id, "adopted under the LocalOnly id");
    let local_symbol_id = bound_logical_symbol_id(&conn);

    // Deepen + re-open: the incoming identity is now Portable and its HEAD reaches the recorded
    // boundary → PROVEN → UPGRADE, not a refusal.
    let portable_id = "0abc123root";
    register_repo(
        &conn,
        &identity(portable_id, "deepened"),
        repo.as_path(),
        2,
        &crate::index::migration_hooks(),
    )
    .expect("a PROVEN Portable id against a local: incumbent UPGRADES in place, not refused");

    // The portable id now solely owns the DB; the local id is gone.
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), portable_id, "portable id owns the DB");
    assert_eq!(repo_row_count(&conn, local_id), 0, "the machine-local repos row is gone");
    assert_eq!(repo_row_count(&conn, portable_id), 1, "exactly the portable repos row remains");
    // Scoped rows + the recorded root re-pointed off the local id.
    assert_eq!(
        conn.query_row("SELECT repo_id FROM files", [], |r| r.get::<_, String>(0)).unwrap(),
        portable_id,
        "the file row re-pointed to the portable id",
    );
    assert_eq!(root_count(&conn, local_id), 0, "no root left under the local id");
    assert_eq!(root_count(&conn, portable_id), 1, "the root moved to the portable id");
    // The logical id re-derived under the portable fold, and the bound memory followed it.
    let portable_symbol_id: i64 =
        conn.query_row("SELECT id FROM logical_symbols", [], |r| r.get(0)).unwrap();
    assert_ne!(portable_symbol_id, local_symbol_id, "the id re-derived under the portable repo_id");
    assert_eq!(
        bound_logical_symbol_id(&conn),
        portable_symbol_id,
        "the bound memory survives the upgrade, still resolving to the same symbol",
    );
    let _ = fs::remove_dir_all(&repo);
}

/// A6 batch-4 P2: the LocalOnly→Portable upgrade must SERIALIZE with a writer still holding the
/// OUTGOING `local:` discriminator's write lock — the lock identity flips with the derived id at
/// unshallow time, so without this the upgrade re-points every scoped row out from under an
/// in-flight pre-unshallow writer. (Deadlock order: the upgrade is the only multi-lock holder and
/// always acquires incoming-then-outgoing; see the comment in `register_repo`.)
#[test]
fn upgrade_blocks_until_the_outgoing_local_lock_holder_finishes() {
    let repo = real_git_repo("upgrade-lock");
    let boundary = head_commit_hash(&repo);

    // FILE-backed DB: the outgoing-lock acquisition keys off `conn.path()` (a pathless in-memory
    // DB skips it — no cross-process writer can exist there).
    let db_root = unique_temp_root();
    let _ = fs::remove_dir_all(&db_root);
    fs::create_dir_all(&db_root).unwrap();
    let db_path = db_root.join("index.sqlite");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    seed_pre_v040_core_schema(&conn);
    conn.execute(
        "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms) VALUES \
         ('src/lib.rs', 'rust', 'source', 'a', 0, 0)",
        [],
    )
    .unwrap();
    schema::migrations::apply_repo_id_core_scoping(&conn, &crate::index::migration_hooks())
        .expect("V040 applies");
    let local_id = "local:aaaa1111bbbb";
    register_repo(
        &conn,
        &identity_local(local_id, "shallow", vec![boundary]),
        repo.as_path(),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    // Writer 1: an in-flight pre-unshallow writer — holds the OUTGOING local-id lock while
    // writing two rows with a deliberate pause between them.
    let writer_db = db_path.clone();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let _lock =
            rag_rat_base::locks::WriteLock::acquire_blocking(&writer_db, "local:aaaa1111bbbb")
                .unwrap();
        let conn = rusqlite::Connection::open(&writer_db).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             repo_id) VALUES ('src/w1.rs', 'rust', 'source', 'w1', 0, 0, 'local:aaaa1111bbbb')",
            [],
        )
        .unwrap();
        held_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             repo_id) VALUES ('src/w2.rs', 'rust', 'source', 'w2', 0, 0, 'local:aaaa1111bbbb')",
            [],
        )
        .unwrap();
        // The lock releases on drop, AFTER both writes — the upgrade must not run before this.
    });
    held_rx.recv().unwrap();

    // Writer 2 triggers the upgrade (the post-unshallow open). It must BLOCK on the outgoing
    // lock until writer 1 finishes, then re-point EVERYTHING — including writer 1's second row.
    let started = std::time::Instant::now();
    register_repo(
        &conn,
        &identity("0abc123root", "deepened"),
        repo.as_path(),
        2,
        &crate::index::migration_hooks(),
    )
    .expect("the upgrade proceeds once the outgoing-lock holder finishes");
    let elapsed = started.elapsed();
    writer.join().unwrap();
    assert!(
        elapsed >= std::time::Duration::from_millis(300),
        "the upgrade must block until the outgoing local-lock holder completes, got {elapsed:?}"
    );
    // No interleaved re-point: row attribution is consistent — every row (the seed + BOTH of the
    // in-flight writer's rows) ended under the portable id, none stranded under the local id.
    let under_local: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE repo_id = ?1", [local_id], |r| r.get(0))
        .unwrap();
    let under_portable: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE repo_id = '0abc123root'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(under_local, 0, "no row left under the outgoing local id");
    assert_eq!(under_portable, 3, "seed + both in-flight writer rows re-pointed together");

    let _ = fs::remove_dir_all(&db_root);
    let _ = fs::remove_dir_all(&repo);
}

/// A6 batch-5 P2 (the fence gap): a writer whose entry lock was keyed by the STALE `local:` id
/// (derived pre-unshallow) and whose open then resolves + upgrades to the portable id must extend
/// its lock coverage to the RESOLVED id for the rest of its run — a fresh portable-lock writer
/// blocks until it completes, instead of running concurrently with a writer holding only the
/// retired local lock. RULE: a writer's held lock must match the repo id it writes under.
#[test]
fn a_writer_whose_identity_upgrades_mid_run_extends_its_lock_to_the_resolved_id() {
    let repo = real_git_repo("fence-gap");
    let boundary = head_commit_hash(&repo);
    let config = source_config(repo.clone(), Language::Rust);
    let local_id = "local:feedfacecafe";
    {
        let db = IndexDatabase::create_or_migrate(&config.database).unwrap();
        register_repo(
            db.storage.connection(),
            &identity_local(local_id, "shallow", vec![boundary]),
            repo.as_path(),
            1,
            &crate::index::migration_hooks(),
        )
        .unwrap();
    }

    // The gap writer: entry lock keyed by the stale local id (what a pre-unshallow derivation
    // gave), then the open resolves the portable id, upgrades, and — the fix — stashes the
    // resolved id's lock on the connection for its lifetime.
    let _entry_lock =
        rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, local_id).unwrap();
    let db = IndexDatabase::open_config(&config).unwrap();
    let resolved = db.active_repo_id.clone();
    assert!(!resolved.starts_with("local:"), "the open resolved + upgraded to the portable id");

    // A concurrent portable-lock writer (another thread — same-thread probes would re-enter)
    // must BLOCK while the gap writer's connection lives...
    let probe_db = config.database.clone();
    let probe_id = resolved.clone();
    let blocked = std::thread::spawn(move || {
        rag_rat_base::locks::WriteLock::acquire_timeout(
            &probe_db,
            &probe_id,
            std::time::Duration::from_millis(150),
        )
        .unwrap()
        .is_some()
    })
    .join()
    .unwrap();
    assert!(!blocked, "a portable-lock writer must block while the gap writer runs");

    // ...and proceed once it completes (dropping the connection releases the stashed lock).
    drop(db);
    let probe_db = config.database.clone();
    let free = std::thread::spawn(move || {
        rag_rat_base::locks::WriteLock::acquire_timeout(
            &probe_db,
            &resolved,
            std::time::Duration::from_millis(500),
        )
        .unwrap()
        .is_some()
    })
    .join()
    .unwrap();
    assert!(free, "the resolved id's lock frees when the gap writer's connection drops");

    let _ = fs::remove_dir_all(&repo);
}

/// PROOF gates the RE-POINT, not the registration (A7): a DB first indexed from a cut shallow clone
/// of repo X, later opened from an UNRELATED full repo Y at a NEW root, must NOT upgrade X's local
/// id onto Y — Y's HEAD reaches NONE of X's boundary commits, so re-pointing would migrate one
/// repo's data onto another. Instead Y registers as its OWN repo (the multi-repo default) and X's
/// `local:` index is left exactly as it was. The critical safety — no cross-repo data migration —
/// holds because the upgrade did not fire.
#[test]
fn register_repo_adds_an_unrelated_repo_without_upgrading_the_local_incumbent() {
    let repo_x = real_git_repo("origin-x");
    let x_boundary = head_commit_hash(&repo_x);
    let repo_y = real_git_repo("unrelated-y"); // an independent root — no shared history with X.

    let conn = fresh_conn();
    let local_id = "local:beefbeefcafe";
    register_repo(
        &conn,
        &identity_local(local_id, "shallow", vec![x_boundary]),
        repo_x.as_path(),
        1,
        &crate::index::migration_hooks(),
    )
    .expect("the shallow clone of X adopts under its local id, recording X's boundary");

    let registered = register_repo(
        &conn,
        &identity("y-portable-root", "y"),
        repo_y.as_path(),
        2,
        &crate::index::migration_hooks(),
    )
    .expect("an unrelated repo at a new root registers as its own repo — no upgrade attempted");
    assert_eq!(registered, "y-portable-root");
    // X's local incumbent is untouched (NOT re-pointed onto Y); both repos now coexist.
    assert_eq!(repo_row_count(&conn, local_id), 1, "X's local id is left as-is, never upgraded");
    assert_eq!(repo_row_count(&conn, "y-portable-root"), 1, "Y registered as a separate repo");
    assert!(schema::multiple_real_repos(&conn).unwrap(), "the DB now holds two real repos");
    let _ = fs::remove_dir_all(&repo_x);
    let _ = fs::remove_dir_all(&repo_y);
}

/// No recorded shallow boundary ⇒ no proof available ⇒ refuse. A `local:` incumbent registered
/// before the proof gate (or with an unknown boundary) cannot be upgraded on faith, even by a
/// genuine deepened clone; the actionable error points at the `[index] repo_id` pin to force it.
#[test]
fn register_repo_refuses_a_local_upgrade_without_a_recorded_boundary() {
    let repo = real_git_repo("no-boundary");
    let conn = fresh_conn();
    // Register the local incumbent with an EMPTY boundary — nothing to prove against.
    let local_id = "local:nobound00";
    register_repo(
        &conn,
        &identity_local(local_id, "shallow", vec![]),
        repo.as_path(),
        1,
        &crate::index::migration_hooks(),
    )
    .expect("a LocalOnly registration succeeds even without a boundary — the gate is at UPGRADE");

    let err = register_repo(
        &conn,
        &identity("would-be-portable", "p"),
        repo.as_path(),
        2,
        &crate::index::migration_hooks(),
    )
    .expect_err("no recorded boundary ⇒ no proof ⇒ the upgrade is refused");
    assert!(err.to_string().contains("repo_id"), "refusal names the pin remedy: {err}");
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), local_id, "the incumbent is untouched");
    let _ = fs::remove_dir_all(&repo);
}

/// A7: a second PORTABLE repo at a DIFFERENT root registers fresh (like any new repo) and NEVER
/// re-points the incumbent — the upgrade machinery is reserved for a `local:` incumbent, so a
/// Portable incoming can only ever add a new repo, never silently migrate one portable repo's rows
/// onto another (which would be data loss). The incumbent is left exactly as it was.
#[test]
fn register_repo_adds_a_second_portable_repo_without_repointing_the_incumbent() {
    let conn = fresh_conn();
    register_repo(
        &conn,
        &identity("portable-a", "a"),
        Path::new("/src/a"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    register_repo(
        &conn,
        &identity("portable-b", "b"),
        Path::new("/src/b"),
        2,
        &crate::index::migration_hooks(),
    )
    .expect("a second portable repo at an unclaimed root registers as its own repo");
    // The incumbent is untouched (still one row, its own root); portable-b is a distinct repo.
    assert_eq!(repo_row_count(&conn, "portable-a"), 1, "incumbent untouched — not re-pointed");
    assert_eq!(root_count(&conn, "portable-a"), 1);
    assert_eq!(repo_row_count(&conn, "portable-b"), 1, "the second portable repo landed");
}

/// The SECOND shallow clone of one upstream must not be stranded once the portable id exists
/// (Codex batch 5): clones A and B register under distinct `local:` ids; A deepens and claims the
/// portable id P; B deepens LATER — register_repo(P at B's root) hits the idempotent branch, whose
/// root-owner guard would refuse (and pinning P re-enters the same refusal). The LATE-upgrade
/// merge instead retires B's local id INTO P: B's AUTHORED memories move (rowid anchors nulled for
/// re-resolution), B's DERIVED rows drop (a fresh index re-derives them under P), both roots land
/// under P, and P's existing data — A's — is untouched.
#[test]
fn second_shallow_clone_late_upgrades_into_the_existing_portable_repo() {
    let root_b = real_git_repo("late-b");
    let boundary_b = head_commit_hash(&root_b);

    let conn = fresh_conn();

    // A's story already completed: the portable id P is registered at A's root (the in-place
    // upgrade path, covered by its own tests) and carries A's authored + derived data.
    register_repo(
        &conn,
        &identity("P-portable", "up"),
        Path::new("/src/clone-a"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms,          updated_at_ms, source, memory_version, repo_id)
         VALUES ('mem-a', 'Invariant', 'a title', 'a body', 'high', 'active', 0, 0, 'agent',          'v1', 'P-portable')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,          commit_sha, worktree_id, repo_id, generation)
         VALUES ('src/shared.rs', 'rust', 'source', 'a-sha', 0, 0, 'c1', '', 'P-portable', 0)",
        [],
    )
    .unwrap();

    // B registers as a shallow clone at its own (real) root, with its boundary recorded, and
    // authors a memory (full anchor set) plus a derived file row.
    register_repo(
        &conn,
        &identity_local("local:bbbb", "clone-b", vec![boundary_b]),
        root_b.as_path(),
        2,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms,          updated_at_ms, source, memory_version, repo_id)
         VALUES ('mem-b', 'Decision', 'b title', 'b body', 'high', 'active', 0, 0, 'agent',          'v1', 'local:bbbb')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path,          \
         logical_symbol_id, symbol_id, chunk_id, edge_id, anchor_status, created_at_ms, repo_id)
         VALUES ('mem-b', 'path', 'bind-b', 'src/b.rs', 11, 22, 33, 44, 'current', 0,          \
         'local:bbbb')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('mem-b', 'tag-b')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_call_paths(memory_id, start_logical_symbol_id,          \
         end_logical_symbol_id, edge_sequence_hash, path_summary, created_at_ms)
         VALUES ('mem-b', 55, 66, 'hash-b', 'x -> y', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
         VALUES ('local:bbbb', 'mem-b', 'b title', 'b body', 'Decision', 'tag-b')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,          commit_sha, worktree_id, repo_id, generation)
         VALUES ('src/shared.rs', 'rust', 'source', 'b-sha', 0, 0, 'c1', '', 'local:bbbb', 0)",
        [],
    )
    .unwrap();

    // B deepens: the incoming portable identity is the ALREADY-REGISTERED P at B's root — the
    // late-upgrade merge, not a refusal.
    let registered = register_repo(
        &conn,
        &identity("P-portable", "up"),
        root_b.as_path(),
        3,
        &crate::index::migration_hooks(),
    )
    .expect("the second clone's late upgrade must complete, never strand");
    assert_eq!(registered, "P-portable");

    // The local id is retired; BOTH roots live under P.
    assert_eq!(repo_row_count(&conn, "local:bbbb"), 0, "B's local id retired");
    for root in ["/src/clone-a", &root_b.to_string_lossy()] {
        let owned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM repo_roots WHERE repo_id = 'P-portable' AND root = ?1",
                [root],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owned, 1, "root {root} recorded under P");
    }

    // B's memory MOVED with correct attribution: repo_id = P, rowid anchors nulled, portable
    // anchor + tag + call-path + FTS row intact.
    let (repo, ls, sy, path_col): (String, Option<i64>, Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT m.repo_id, b.logical_symbol_id, b.symbol_id, b.path
             FROM repo_memories m JOIN repo_memory_bindings b ON b.memory_id = m.id
             WHERE m.id = 'mem-b'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(repo, "P-portable", "B's memory re-attributed to P");
    assert_eq!((ls, sy), (None, None), "rowid anchors nulled for re-resolution");
    assert_eq!(path_col.as_deref(), Some("src/b.rs"), "portable anchor survives");
    let (cp_start, fts_repo): (Option<i64>, String) = conn
        .query_row(
            "SELECT cp.start_logical_symbol_id, f.repo_id
             FROM repo_memory_call_paths cp, repo_memory_fts f
             WHERE cp.memory_id = 'mem-b' AND f.memory_id = 'mem-b'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cp_start, None, "call-path endpoints nulled");
    assert_eq!(fts_repo, "P-portable", "FTS mirror row follows the memory");

    // B's DERIVED row dropped (re-derived by the next index); A's data untouched.
    let b_files: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.files WHERE sha256 = 'b-sha'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(b_files, 0, "B's derived rows dropped, not migrated");
    let (a_mem, a_files): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM repo_memories WHERE id='mem-a' AND              \
             repo_id='P-portable' AND title='a title'),
                    (SELECT COUNT(*) FROM main.files WHERE sha256='a-sha' AND              \
             repo_id='P-portable')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((a_mem, a_files), (1, 1), "P's existing (A's) data untouched by the merge");

    // The flow is now plainly idempotent: P at B's root re-registers without drama.
    register_repo(
        &conn,
        &identity("P-portable", "up"),
        root_b.as_path(),
        4,
        &crate::index::migration_hooks(),
    )
    .expect("post-merge re-registration is the plain idempotent path");
    let _ = fs::remove_dir_all(&root_b);
}

/// The upgrade scan matches on (root AND boundary), never boundary alone: with TWO shallow clones
/// of the same upstream registered under distinct `local:` ids — an ordinary shape on the shared
/// global DB — a deepened checkout's HEAD reaches BOTH boundaries, and a boundary-only pick would
/// re-point whichever incumbent SORTS first, hijacking the sibling clone's index and stranding the
/// actually-deepened one. The root match pins the upgrade to the working tree that was deepened.
#[test]
fn upgrade_picks_the_root_matching_incumbent_among_two_shallow_clones() {
    let repo = real_git_repo("two-shallow"); // clone B's working tree — the one that deepens.
    let boundary = head_commit_hash(&repo);

    let conn = fresh_conn();
    // Clone A: a SIBLING shallow clone of the same upstream at a DIFFERENT root, whose id sorts
    // FIRST — the incumbent a boundary-only scan would wrongly pick (its boundary is equally
    // reachable from the deepened HEAD).
    register_repo(
        &conn,
        &identity_local("local:aaa", "clone-a", vec![boundary.clone()]),
        Path::new("/src/clone-a"),
        1,
        &crate::index::migration_hooks(),
    )
    .expect("sibling shallow clone registers fresh at its own root");
    // Clone B: the clone that will be deepened, registered at the REAL working tree.
    register_repo(
        &conn,
        &identity_local("local:bbb", "clone-b", vec![boundary]),
        repo.as_path(),
        2,
        &crate::index::migration_hooks(),
    )
    .expect("second shallow clone registers fresh at its own root");

    // B deepens (its HEAD reaches both recorded boundaries) and re-registers portable from B's
    // root: ONLY B's incumbent upgrades.
    register_repo(
        &conn,
        &identity("portable-root", "b"),
        repo.as_path(),
        3,
        &crate::index::migration_hooks(),
    )
    .expect("the deepened clone upgrades its own incumbent");
    assert_eq!(repo_row_count(&conn, "local:bbb"), 0, "B's local id upgraded in place");
    assert_eq!(repo_row_count(&conn, "portable-root"), 1);
    assert_eq!(
        repo_row_count(&conn, "local:aaa"),
        1,
        "the first-sorting SIBLING incumbent is untouched — never hijacked by boundary alone",
    );
    // And the untouched sibling is not bricked: its own re-registration stays idempotent.
    register_repo(
        &conn,
        &identity_local("local:aaa", "clone-a", vec![]),
        Path::new("/src/clone-a"),
        4,
        &crate::index::migration_hooks(),
    )
    .expect("the sibling clone keeps re-registering under its own id");
    let _ = fs::remove_dir_all(&repo);
}

/// The IDEMPOTENT path carries the root-owner guard too: a checkout whose identity changes to an
/// ALREADY-REGISTERED id (a pin switched to an existing repo, an in-place re-clone) must not
/// silently record one physical root under TWO repos — that would make `resolve_config_repo_id`'s
/// recorded-root route (`LIMIT 1` over two owners) non-deterministic.
#[test]
fn idempotent_reregistration_refuses_a_root_owned_by_another_repo() {
    let conn = fresh_conn();
    register_repo(
        &conn,
        &identity("repo-abc", "a"),
        Path::new("/src/a"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    register_repo(
        &conn,
        &identity("repo-xyz", "b"),
        Path::new("/src/b"),
        2,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    // repo-xyz (already registered) shows up at repo-abc's root — an identity change, not a new
    // worktree of xyz. Refused; the root stays mapped to exactly one repo.
    let err = register_repo(
        &conn,
        &identity("repo-xyz", "b"),
        Path::new("/src/a"),
        3,
        &crate::index::migration_hooks(),
    )
    .expect_err("an owned root must not be recorded under a second repo");
    assert!(err.to_string().contains("repo-abc"), "refusal names the owning repo: {err}");
    let owners: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT repo_id) FROM repo_roots WHERE root = '/src/a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owners, 1, "one physical root maps to exactly one repo");
    // The same repo's own root re-registration stays idempotent (self-ownership never trips).
    register_repo(
        &conn,
        &identity("repo-xyz", "b"),
        Path::new("/src/b"),
        4,
        &crate::index::migration_hooks(),
    )
    .expect("re-registering an owned root under its OWN repo is idempotent");
}

/// Two repos' concurrent FIRST registrations on one shared file DB both succeed (A7): the
/// DB-global registry lock serializes the read-decide-write sequence, so neither writer sees the
/// torn middle (`SQLITE_BUSY_SNAPSHOT` on a deferred upgrade, or a `repos`-PK constraint on the
/// same-id race). The same-id pair collapses the loser into the idempotent path.
#[test]
fn concurrent_registrations_on_a_shared_db_both_succeed() {
    use std::sync::{Arc, Barrier};

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let db_path = root.join("global.sqlite");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
    }

    // Round 1: two DIFFERENT repos race their first registration.
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["repo-one", "repo-two"]
        .into_iter()
        .map(|id| {
            let barrier = Arc::clone(&barrier);
            let db_path = db_path.clone();
            std::thread::spawn(move || {
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                conn.pragma_update(None, "foreign_keys", "ON").unwrap();
                barrier.wait();
                register_repo(
                    &conn,
                    &identity(id, id),
                    Path::new(&format!("/src/{id}")),
                    1,
                    &crate::index::migration_hooks(),
                )
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().expect("a concurrent first registration must succeed");
    }

    // Round 2: the SAME repo races itself (two worktrees' first open) — winner registers, loser
    // collapses into the idempotent path; never a PK constraint.
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let db_path = db_path.clone();
            std::thread::spawn(move || {
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                conn.pragma_update(None, "foreign_keys", "ON").unwrap();
                barrier.wait();
                register_repo(
                    &conn,
                    &identity("repo-same", "s"),
                    Path::new("/src/same"),
                    2,
                    &crate::index::migration_hooks(),
                )
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().expect("a same-id registration race must collapse idempotently");
    }

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    for id in ["repo-one", "repo-two", "repo-same"] {
        assert_eq!(repo_row_count(&conn, id), 1, "{id} registered exactly once");
    }
    let _ = fs::remove_dir_all(&root);
}

/// A `LocalOnly` incoming for a working tree ALREADY registered under a real repo is refused — a
/// re-shallowed clone must never DOWNGRADE the portable id its root already owns. Keyed on the ROOT
/// (`real_root_owner`): the same physical path resolving to a machine-local id while it is recorded
/// under a portable one is exactly the downgrade the refusal guards. (A LocalOnly incoming at a
/// NEW, unclaimed root is instead a genuinely new shallow repo and registers fresh.)
#[test]
fn register_repo_refuses_a_local_only_downgrade_of_an_owned_root() {
    let conn = fresh_conn();
    register_repo(
        &conn,
        &identity("portable-a", "a"),
        Path::new("/src/a"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    // Same root as portable-a: a re-shallowed clone of A resolving to a machine-local id.
    let err = register_repo(
        &conn,
        &identity_local("local:beef", "shallow", vec![]),
        Path::new("/src/a"),
        2,
        &crate::index::migration_hooks(),
    )
    .expect_err("a LocalOnly incoming must not downgrade the portable id its root already owns");
    assert!(err.to_string().contains("portable-a"), "refusal names the owning repo: {err}");
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "portable-a", "portable id stays");
    assert_eq!(repo_row_count(&conn, "local:beef"), 0, "the local id never landed");
}

/// End-to-end: a real cut shallow clone is indexed under a `local:` id, then `git fetch
/// --unshallow` makes the portable root reachable, and re-opening UPGRADES the existing index in
/// place — the registered id flips to the portable root hash, the `local:` id is gone, and a memory
/// bound to an indexed symbol still resolves. This is the exact path the LocalOnly warning tells
/// the user to take; it must not strand their index.
#[test]
fn unshallow_upgrades_a_shallow_clone_index_from_local_to_portable_in_place() {
    let base = unique_temp_root();
    let _ = fs::remove_dir_all(&base);
    let origin = base.join("origin");
    fs::create_dir_all(origin.join("src")).unwrap();
    fs::write(
        origin.join("src/lib.rs"),
        "pub fn shallow_anchor() {}\npub fn call_anchor() { shallow_anchor(); }\n",
    )
    .unwrap();
    run_git(&origin, &["init", "-q", "-b", "main"]);
    run_git(&origin, &["config", "user.email", "t@e"]);
    run_git(&origin, &["config", "user.name", "t"]);
    run_git(&origin, &["add", "."]);
    run_git(&origin, &["commit", "-q", "-m", "one"]);
    run_git(&origin, &["commit", "-q", "--allow-empty", "-m", "two"]);
    // The portable id the deepened clone must resolve to: origin has full history, so its identity
    // is the (Portable) root-commit hash — the exact id `git fetch --unshallow` makes reachable
    // again.
    let origin_root =
        rag_rat_base::repo_identity::resolve_repo_identity(&origin, None).unwrap().repo_id;

    // --depth 1 < history: a genuinely CUT shallow clone (root unreachable → LocalOnly id).
    let url = format!("file://{}", origin.display());
    run_git(&base, &["clone", "-q", "--depth", "1", &url, "clone"]);
    let clone_root = base.join("clone");

    let config = source_config(clone_root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).expect("index the shallow clone under a LocalOnly id");
    let local_id = db.active_repo_id.clone();
    assert!(local_id.starts_with("local:"), "shallow clone indexes under a local: id");

    // Bind a memory to an indexed logical symbol so we can prove it survives the id realign.
    let symbol_id: i64 = db
        .storage
        .connection()
        .query_row("SELECT id FROM logical_symbols LIMIT 1", [], |r| r.get(0))
        .expect("the shallow clone indexed at least one logical symbol");
    let symbol_name: String = db
        .storage
        .connection()
        .query_row("SELECT logical_name FROM logical_symbols WHERE id = ?1", [symbol_id], |r| {
            r.get(0)
        })
        .unwrap();
    // A real memory row + a binding to the indexed symbol (the FK + NOT NULL columns the actual
    // schema carries). The upgrade's realign must re-point this binding as `logical_symbols.id`
    // changes under the portable fold.
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version) VALUES ('mem-upgrade', 'Invariant', 't', 'b', \
             'high', 'active', 0, 0, 'agent', 'v1')",
            [],
        )
        .unwrap();
    db.storage
        .connection()
        .execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, \
             logical_symbol_id, anchor_status, created_at_ms) VALUES ('mem-upgrade', \
             'logical_symbol', 'b1', ?1, 'current', 0)",
            [symbol_id],
        )
        .unwrap();
    let call_edge_id: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT id FROM edges WHERE to_name = 'shallow_anchor' AND to_symbol_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("the caller resolves to shallow_anchor");
    let call_memory = db
        .memory_create(rag_rat_query::memory::RepoMemoryCreate {
            kind: "Invariant".to_string(),
            title: "Call path survives identity adoption".to_string(),
            body: "The persisted edge identity follows the logical callee id.".to_string(),
            confidence: "high".to_string(),
            created_by: Some("test-agent".to_string()),
            source: Some("agent".to_string()),
            tags: Vec::new(),
            payload_json: None,
            bind: rag_rat_query::memory::RepoMemoryBindTarget {
                edge_path: Some(vec![call_edge_id]),
                ..Default::default()
            },
        })
        .unwrap()
        .memory
        .memory_id;
    let (local_callee, local_fingerprint, local_hash): (i64, String, String) = db
        .storage
        .connection()
        .query_row(
            "SELECT callee_logical_symbol_id, edge_fingerprint, edge_sequence_hash
               FROM repo_memory_call_path_edges WHERE memory_id = ?1",
            [&call_memory],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    drop(db);

    // Deepen the clone: the real root becomes reachable, so identity is now Portable (the root
    // hash).
    run_git(&clone_root, &["fetch", "-q", "--unshallow"]);

    // Re-open the EXISTING index (not a rebuild): register_repo detects local: incumbent + Portable
    // incoming → upgrade in place.
    let reopened = IndexDatabase::open_config(&config)
        .expect("re-opening a deepened clone upgrades the index, it does not refuse");
    assert_eq!(reopened.active_repo_id, origin_root, "upgraded to the portable root-commit id");
    assert!(!reopened.active_repo_id.starts_with("local:"), "no longer a machine-local id");
    assert_eq!(
        repo_row_count(reopened.storage.connection(), &local_id),
        0,
        "the machine-local repos row is gone after the upgrade",
    );
    // The bound memory survived: its (realigned) logical_symbol_id still resolves to the same
    // symbol.
    let resolved_name: String = reopened
        .storage
        .connection()
        .query_row(
            "SELECT ls.logical_name FROM repo_memory_bindings b
               JOIN logical_symbols ls ON ls.id = b.logical_symbol_id
              WHERE b.memory_id = 'mem-upgrade'",
            [],
            |r| r.get(0),
        )
        .expect("the memory binding still resolves after the in-place upgrade");
    assert_eq!(resolved_name, symbol_name, "the memory resolves to the same symbol post-upgrade");
    let (portable_callee, portable_fingerprint, edge_hash, path_hash, binding_hash): (
        i64,
        String,
        String,
        String,
        String,
    ) = reopened
        .storage
        .connection()
        .query_row(
            "SELECT e.callee_logical_symbol_id, e.edge_fingerprint, e.edge_sequence_hash,
                    p.edge_sequence_hash, IIF(b.resolved, b.resolved_binding_id, b.binding_id)
               FROM repo_memory_call_path_edges e
               JOIN repo_memory_call_paths p ON p.memory_id = e.memory_id
               JOIN repo_memory_bindings b ON b.memory_id = e.memory_id
                  AND b.binding_kind = 'call_path'
              WHERE e.memory_id = ?1",
            [&call_memory],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert_ne!(portable_callee, local_callee, "the callee id follows the portable repo fold");
    assert_ne!(portable_fingerprint, local_fingerprint, "the fingerprint includes that callee id");
    assert_ne!(edge_hash, local_hash, "the ordered fingerprint hash is re-derived");
    assert_eq!(
        (edge_hash.as_str(), path_hash.as_str(), binding_hash.as_str()),
        (path_hash.as_str(), path_hash.as_str(), path_hash.as_str()),
        "edge rows, call-path parent, and binding stay keyed by one sequence hash",
    );
    let _ = fs::remove_dir_all(&base);
}
