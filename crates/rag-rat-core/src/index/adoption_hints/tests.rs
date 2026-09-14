use std::path::PathBuf;

use rag_rat_base::config::{Config, ResolvedTarget, TargetKind};
use rag_rat_base::language::Language;
use rag_rat_base::test_scratch::{self, ScratchDir};

use super::*;

// `schema_bootstrap_tests::source_config` is private to that module and not reachable from
// here (a sibling of `index`, not a descendant of `schema_bootstrap_tests`) — build the
// fixture `Config` inline instead of widening its visibility just for this test.
/// A created, uniquely owned scratch root that is removed on drop.
///
/// [`ScratchDir`] rather than a hand-rolled `std::env::temp_dir()` join, for two reasons. It
/// names the dir from the pid AND a process-wide counter, so it is unique under both runners
/// (`cargo test`, which the coverage job runs, executes tests as THREADS IN ONE PROCESS, where
/// a pid+millisecond name collides and one test's cleanup races another's `git init`). And it
/// hands the path out through a symlinked ancestor, so `canonical_config_root` in
/// [`source_config`] is a real normalization here — a bare `temp_dir()` root is already
/// canonical on Linux, which would leave these fixtures outside the per-PR coverage of
/// root-spelling bugs and only redden the cross-platform legs (#1027). The guard also brings
/// them under the scratch namespace's stale sweep (#726).
fn unique_temp_root(tag: &str) -> ScratchDir {
    ScratchDir::new(tag)
}

fn source_config(root: PathBuf, language: Language) -> Config {
    let config_root = test_scratch::canonical_config_root(root);
    Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: config_root.join(".rag-rat/index.sqlite"),
        root: config_root,
        targets: vec![ResolvedTarget {
            name: language.as_db_str().to_string(),
            language,
            directories: vec![PathBuf::from("src")],
            include: vec!["src/".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    }
}

#[test]
fn would_discover_any_file_is_false_when_targets_are_empty() {
    let root = unique_temp_root("adopt-no-targets");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
    let mut config = source_config(root.to_path_buf(), Language::Rust);
    config.targets.clear(); // no [target_bindings] → nothing to walk
    assert!(!would_discover_any_file(&config).unwrap());
}

#[test]
fn would_discover_any_file_is_true_when_a_target_matches() {
    let root = unique_temp_root("adopt-target-match");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
    let config = source_config(root.to_path_buf(), Language::Rust);
    assert!(would_discover_any_file(&config).unwrap());
}

// Run a git command in `root`, panicking on failure — models
// `query_api::oracle_surfacing_tests::git`, private to that module and not reachable here.
fn git(root: &std::path::Path, args: &[&str]) {
    // The shared seam keeps the #581 isolation (no ambient gitconfig, pinned HOME and
    // identity) and adds repository-routing isolation (an exported `GIT_DIR`/`GIT_WORK_TREE`
    // can never re-point a fixture at an external repo).
    rag_rat_base::test_git::run(root, args);
}

/// Clone `source` into the already-created scratch dir `dest` — a second physical checkout
/// sharing `source`'s portable identity. `git clone` accepts an existing EMPTY destination, so
/// the clone can land in a guarded scratch dir instead of an unguarded sibling path.
fn clone_into(source: &ScratchDir, dest: &ScratchDir) {
    git(source, &["clone", "-q", ".", dest.path().to_str().unwrap()]);
}

/// A clone sharing a checkout's portable (root-commit-derived) identity, pointed at the SAME
/// database (the consolidated-DB shape the #427 join hint targets), fires the note naming the
/// original checkout; the original checkout re-indexing itself, and a config whose DB doesn't
/// exist yet, both stay quiet.
#[test]
fn same_identity_join_note_fires_for_a_second_checkout_and_not_the_first() {
    if !rag_rat_base::test_git::available() {
        return; // no git on PATH — skip rather than fail.
    }
    let root_a = unique_temp_root("adopt-join-a");
    std::fs::create_dir_all(root_a.join("src")).unwrap();
    std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
    git(&root_a, &["init", "-q", "-b", "main"]);
    git(&root_a, &["add", "-A"]);
    git(&root_a, &["commit", "-q", "-m", "init"]);
    let config_a = source_config(root_a.to_path_buf(), Language::Rust);

    // No DB yet at all — nothing to join.
    assert!(same_identity_join_note(&config_a).unwrap().is_none());

    crate::index::IndexDatabase::rebuild(&config_a).unwrap(); // registers A at root_a

    // A's own recorded checkout re-indexing itself is an ordinary re-index, not a join.
    assert!(same_identity_join_note(&config_a).unwrap().is_none());

    // A full clone of A shares its portable (root-commit) identity but is a NEW checkout,
    // pointed at A's SAME database (config_b keeps config_a's `database`, only the `root`
    // moves) — the consolidated-DB shape the hint exists for.
    let root_b = unique_temp_root("adopt-join-b");
    clone_into(&root_a, &root_b);
    let mut config_b = config_a.clone();
    config_b.root = test_scratch::canonical_config_root(root_b.path());

    let expected_repo_id =
        rag_rat_base::repo_identity::resolve_repo_identity(&root_a, None).unwrap().repo_id;
    let note = same_identity_join_note(&config_b).unwrap().unwrap();
    assert_eq!(note.repo_id, expected_repo_id);
    // The recorded root is the spelling A's `Config` carries — canonical, as `Config::load`
    // produces — never the scratch spelling of the same directory (#1027).
    assert_eq!(note.existing_root, config_a.root);
}

/// A garbage/non-DB file at `config.database` must yield `Ok(None)` (no hint), NEVER `Err`:
/// SQLite defers header validation to the first page read, so `open_read_only_blocking`
/// succeeds on junk content and `schema::status` is the first read to fault. A propagated error
/// here would abort `index()` (the CLI calls `same_identity_join_note(config)?`) — a new
/// failure mode the generous-`None` contract exists to prevent.
#[test]
fn same_identity_join_note_is_none_on_a_garbage_db_file() {
    let root = unique_temp_root("adopt-garbage-db");
    let config = source_config(root.to_path_buf(), Language::Rust);
    std::fs::create_dir_all(config.database.parent().unwrap()).unwrap();
    std::fs::write(&config.database, b"not a sqlite database at all\x00\xff").unwrap();
    assert!(config.database.exists());

    let result = same_identity_join_note(&config);
    assert!(result.is_ok(), "a garbage DB must not abort the index: {result:?}");
    assert!(result.unwrap().is_none(), "a garbage DB yields no join hint");
}

/// `is_root_already_indexed` is `false` before indexing (no DB / fresh) and `true` after a
/// rebuild has recorded this checkout — the signal that scopes the #427 empty-index refusal to
/// FIRST-TIME registrations, so a later delete-to-empty is allowed to prune rather than
/// refused. It keys off an INDEXING-ONLY signal (the recorded root / source_root), NOT the
/// shared identity: a fresh clone sharing A's identity is never indexed, so it stays
/// `false` and an empty clone can't prune A's shared scope.
#[test]
fn is_root_already_indexed_tracks_indexing_not_the_shared_identity() {
    if !rag_rat_base::test_git::available() {
        return; // no git on PATH — skip rather than fail.
    }
    let root_a = unique_temp_root("adopt-indexed-a");
    std::fs::create_dir_all(root_a.join("src")).unwrap();
    std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
    git(&root_a, &["init", "-q", "-b", "main"]);
    git(&root_a, &["add", "-A"]);
    git(&root_a, &["commit", "-q", "-m", "init"]);
    let config_a = source_config(root_a.to_path_buf(), Language::Rust);

    // No database yet → not indexed.
    assert!(!is_root_already_indexed(&config_a).unwrap());

    crate::index::IndexDatabase::rebuild(&config_a).unwrap();

    // A's recorded root → indexed (so a delete-to-empty on A would be allowed to prune).
    assert!(is_root_already_indexed(&config_a).unwrap());

    // A full clone of A shares A's portable identity but its root is NOT recorded — it must NOT
    // count as already-indexed, or an empty clone could prune A's shared scope (#427 review).
    let root_b = unique_temp_root("adopt-indexed-b");
    clone_into(&root_a, &root_b);
    let mut config_b = config_a.clone();
    config_b.root = test_scratch::canonical_config_root(root_b.path());
    assert!(
        !is_root_already_indexed(&config_b).unwrap(),
        "a same-identity clone with an unrecorded root is NOT already-indexed"
    );
}

/// An identity-less (NON-git) root gets NO `repo_roots` entry — `adopt_repo_from_config` falls
/// back to the sole placeholder repo without recording the root — yet after indexing its repo's
/// persisted `source_root` equals the root, so it must still count as already-indexed (#427
/// review). Otherwise a delete-to-empty on a non-git project would be wrongly refused as
/// first-time instead of pruning its stale rows.
#[test]
fn is_root_already_indexed_recognizes_an_indexed_non_git_root() {
    let root = unique_temp_root("adopt-non-git");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
    let config = source_config(root.to_path_buf(), Language::Rust); // NOT a git repo

    // Fresh: not indexed.
    assert!(!is_root_already_indexed(&config).unwrap());

    crate::index::IndexDatabase::rebuild(&config).unwrap();

    // Indexed: recognized via the persisted source_root, despite no repo_roots entry.
    assert!(
        is_root_already_indexed(&config).unwrap(),
        "an indexed non-git root is recognized via its persisted source_root"
    );
    // And so a now-empty re-index is NOT treated as first-time (it would prune, not refuse).
    std::fs::remove_file(root.join("src/a.rs")).unwrap();
    assert!(!is_first_time_empty(&config).unwrap());
}

/// #427 review (comment 4): a READ-ONLY open (`doctor` / MCP / query via `open_config`) adopts
/// identity but must NOT record the checkout's root (it goes through `register_repo_read_only`)
/// nor write `repo_meta[source_root]` — both are indexing-only. So a mere read must NOT flip
/// `is_root_already_indexed` to `true`, or a later empty `--discover` / `--full` on that
/// no-target repo would be waved through as "already indexed" and prune the scope. B is
/// read-registered into A's shared DB but never indexed, so it stays first-time-empty.
#[test]
fn a_read_only_open_does_not_make_an_unindexed_repo_look_indexed() {
    if !rag_rat_base::test_git::available() {
        return; // identity resolution needs git; skip rather than fail.
    }
    // Repo A: a committed git repo, actually indexed into a shared DB (persists A's
    // source_root).
    let root_a = unique_temp_root("adopt-readonly-a");
    std::fs::create_dir_all(root_a.join("src")).unwrap();
    std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
    git(&root_a, &["init", "-q", "-b", "main"]);
    git(&root_a, &["add", "-A"]);
    git(&root_a, &["commit", "-q", "-m", "init"]);
    let config_a = source_config(root_a.to_path_buf(), Language::Rust);
    crate::index::IndexDatabase::rebuild(&config_a).unwrap();

    // Repo B: a DISTINCT git repo (own init → own root commit → own identity), pointed at A's
    // SAME database but with NO discoverable target files. A read-only `open_config` registers
    // B's identity WITHOUT indexing anything and WITHOUT recording its root.
    let root_b = unique_temp_root("adopt-readonly-b");
    std::fs::create_dir_all(root_b.join("src")).unwrap();
    std::fs::write(root_b.join("keep.txt"), "not a rust file\n").unwrap();
    git(&root_b, &["init", "-q", "-b", "main"]);
    git(&root_b, &["add", "-A"]);
    git(&root_b, &["commit", "-q", "-m", "init"]);
    let mut config_b = source_config(root_b.to_path_buf(), Language::Rust);
    config_b.database = config_a.database.clone(); // shared DB

    let _ = crate::index::IndexDatabase::open_config(&config_b).unwrap();

    // The read recorded NO root for B (register_repo_read_only) and ran no indexing pass, so B
    // is NOT already-indexed and a zero-file index on B is still first-time-empty.
    {
        let conn = rusqlite::Connection::open(&config_b.database).unwrap();
        let recorded: i64 = conn
            .query_row(
                "SELECT count(*) FROM repo_roots WHERE root = ?1",
                // The recorded spelling would be the one B's `Config` carries (canonical), so
                // that is what must be absent — asserting on the scratch spelling could pass
                // for the wrong reason.
                [config_b.root.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 0, "a read-only open must not record the checkout's root");
    }
    assert!(
        !is_root_already_indexed(&config_b).unwrap(),
        "a read-only open must not make an unindexed repo look already-indexed"
    );
    assert!(is_first_time_empty(&config_b).unwrap());
}

/// #427 review ("Gate Older schemas on registry availability"): a LEGACY index predating the
/// V038 repo-registry is `Older` but has NO `repos` / `repo_meta` tables. The read-only
/// pre-index probes run BEFORE the write-open migration, so on such a DB they must NOT fault
/// with `no such table: repos` (which would break `rag-rat index` / `--full` on an upgrade) —
/// they treat it as not-yet-indexed / no-join and let the write path migrate it. `Ok`, not
/// `Err`.
#[test]
fn a_pre_registry_legacy_schema_does_not_fault_the_readonly_probes() {
    let root = unique_temp_root("adopt-legacy-schema");
    let config = source_config(root.to_path_buf(), Language::Rust);
    std::fs::create_dir_all(config.database.parent().unwrap()).unwrap();
    // A legacy index: a `files` table makes `schema::status` report `Older` (not `Missing`),
    // but there is no repo registry — exactly a pre-V038 database.
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        conn.execute_batch("CREATE TABLE files(path TEXT PRIMARY KEY);").unwrap();
    }
    // Both probes must SUCCEED with a benign answer, never propagate `no such table: repos`.
    let indexed = is_root_already_indexed(&config);
    assert!(indexed.is_ok(), "a pre-registry DB must not fault the probe: {indexed:?}");
    assert!(!indexed.unwrap(), "a pre-registry DB is not already-indexed");
    let join = same_identity_join_note(&config);
    assert!(join.is_ok(), "a pre-registry DB must not fault the join hint: {join:?}");
    assert!(join.unwrap().is_none());
}

/// #427 review ("Recognize legacy placeholder indexes before refusing empties"): a LEGACY index
/// still living under the `__unassigned__` placeholder (a pre-adoption DB — simulated here by
/// indexing the root while it was NON-git, then giving it a git identity the DB has not
/// adopted) must count as already-indexed via the SOLE placeholder's `source_root`.
/// Otherwise a delete-to-empty on it is refused as first-time instead of pruning on the
/// first upgrade run.
#[test]
fn is_root_already_indexed_recognizes_a_legacy_placeholder_index() {
    if !rag_rat_base::test_git::available() {
        return; // needs git for the (unregistered) identity; skip rather than fail.
    }
    let root = unique_temp_root("adopt-placeholder");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
    let config = source_config(root.to_path_buf(), Language::Rust);

    // Index while NON-git: `adopt_repo_from_config` sees an ABSENT identity and adopts the sole
    // `__unassigned__` placeholder, persisting `source_root` under it — a pre-adoption-style
    // DB.
    crate::index::IndexDatabase::rebuild(&config).unwrap();

    // Now give the root a git identity the DB has NOT adopted (still under the placeholder), so
    // `resolve_config_repo_id` returns None and only the sole-repo fallback can recognize it.
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);

    assert!(
        is_root_already_indexed(&config).unwrap(),
        "a legacy placeholder index must be recognized via the sole repo's source_root"
    );
    // So a delete-to-empty prunes rather than being refused as first-time-empty.
    std::fs::remove_file(root.join("src/a.rs")).unwrap();
    assert!(!is_first_time_empty(&config).unwrap());
}

/// #427 review ("Warn even when a read open touched the checkout"): a read-only `open_config`
/// (doctor / MCP) on a fresh same-identity clone registers its identity but neither indexes it
/// nor records its root. The same-identity-join warning must STILL fire on the first real index
/// — suppressing it on a mere read would let the clone silently switch the shared scope with
/// none of the `[index] repo_id` guidance.
#[test]
fn same_identity_join_warns_even_after_a_read_only_open_of_the_clone() {
    if !rag_rat_base::test_git::available() {
        return;
    }
    let root_a = unique_temp_root("adopt-join-read-a");
    std::fs::create_dir_all(root_a.join("src")).unwrap();
    std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
    git(&root_a, &["init", "-q", "-b", "main"]);
    git(&root_a, &["add", "-A"]);
    git(&root_a, &["commit", "-q", "-m", "init"]);
    let config_a = source_config(root_a.to_path_buf(), Language::Rust);
    crate::index::IndexDatabase::rebuild(&config_a).unwrap(); // registers A into the shared DB

    // Clone B shares A's identity, pointed at A's SAME database.
    let root_b = unique_temp_root("adopt-join-read-b");
    clone_into(&root_a, &root_b);
    let mut config_b = config_a.clone();
    config_b.root = test_scratch::canonical_config_root(root_b.path());

    // A read-only open registers B's identity but records no root and indexes nothing.
    let _ = crate::index::IndexDatabase::open_config(&config_b).unwrap();

    // The join warning must still fire — the read must not suppress it.
    let note = same_identity_join_note(&config_b).unwrap();
    assert!(note.is_some(), "a read-only-opened clone must still warn it joins the shared scope");
    assert_eq!(note.unwrap().existing_root, config_a.root);
}

/// #427 review ("Preserve pruning for earlier same-identity checkouts"): when checkout A has
/// indexed the shared repo and a same-identity checkout B then indexes it too, B's pass does
/// NOT steal A's already-indexed status — each checkout records its OWN `repo_roots` row
/// (the single-valued `source_root` would be last-writer-wins, flipped to B). So A deleting
/// its last file and re-indexing to empty is still ALLOWED to prune, not refused as
/// first-time-empty.
#[test]
fn a_sibling_index_does_not_steal_an_earlier_checkouts_prune_right() {
    if !rag_rat_base::test_git::available() {
        return;
    }
    // A: committed git repo, indexed into a shared DB.
    let root_a = unique_temp_root("adopt-sibling-a");
    std::fs::create_dir_all(root_a.join("src")).unwrap();
    std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
    git(&root_a, &["init", "-q", "-b", "main"]);
    git(&root_a, &["add", "-A"]);
    git(&root_a, &["commit", "-q", "-m", "init"]);
    let config_a = source_config(root_a.to_path_buf(), Language::Rust);
    crate::index::IndexDatabase::rebuild(&config_a).unwrap();
    assert!(is_root_already_indexed(&config_a).unwrap(), "A is indexed after its rebuild");

    // B: a same-identity clone of A, pointed at the SAME DB, INDEXED too (has its own file).
    let root_b = unique_temp_root("adopt-sibling-b");
    clone_into(&root_a, &root_b);
    let mut config_b = config_a.clone();
    config_b.root = test_scratch::canonical_config_root(root_b.path());
    crate::index::IndexDatabase::index_discover(&config_b).unwrap();

    // B's index overwrote the shared `source_root` to B — but A's `repo_roots` row survives, so
    // A is STILL recognized as already-indexed and its delete-to-empty prunes instead
    // of refusing.
    assert!(
        is_root_already_indexed(&config_a).unwrap(),
        "A must stay already-indexed after a sibling checkout indexes the shared repo"
    );
    std::fs::remove_file(root_a.join("src/a.rs")).unwrap();
    assert!(
        !is_first_time_empty(&config_a).unwrap(),
        "A going empty must be allowed to prune, not refused as first-time-empty"
    );
}

/// The scratch spelling and `config.root` must be two names for ONE directory — the shape
/// `Config::load` produces wherever the system temp is reached through a symlink (macOS
/// `/var` → `/private/var`) or an 8.3 alias (Windows `RUNNER~1`). Rooting these fixtures at a
/// bare `temp_dir()` join degenerates `canonical_config_root` to a no-op on Linux, and the
/// recorded
/// root the probes above compare against would then be indistinguishable from the raw fixture
/// path on the only platform the per-PR matrix runs (#1027).
#[cfg(unix)]
#[test]
fn the_fixture_config_root_diverges_from_its_scratch_spelling() {
    let root = unique_temp_root("adopt-root-spelling");
    let config = source_config(root.to_path_buf(), Language::Rust);
    assert_ne!(
        config.root,
        root.path(),
        "the fixture must reach its root through a symlinked ancestor, or root-spelling bugs stay \
         invisible on the per-PR matrix",
    );
    assert_eq!(
        config.root,
        rag_rat_base::paths::canonicalize(root.path()).unwrap(),
        "both spellings name the same directory",
    );
}
