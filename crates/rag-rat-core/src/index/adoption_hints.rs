//! Pre-index, read-only hints the one-shot `index` command surfaces BEFORE it registers or
//! indexes anything (issue #427): whether any file would be discovered, and whether the configured
//! root is joining an already-registered repo's scope. Detection only — never mutates.

use std::path::PathBuf;

use rag_rat_base::config::Config;
use rag_rat_base::repo_identity;
use rag_rat_db::storage::IndexConnection;

use super::schema;

/// `true` iff indexing `config` would walk at least one target file. `false` means the index would
/// register the repo with EMPTY content — the exact zero-`[target_bindings]` footgun of #427. A
/// one-shot walk is fine here: the `index` command that calls this is about to walk anyway.
pub fn would_discover_any_file(config: &Config) -> anyhow::Result<bool> {
    // Cheap short-circuit for the issue's headline case (no `[target_bindings]` → no targets):
    // skip the filesystem walk entirely.
    if config.targets.is_empty() {
        return Ok(false);
    }
    Ok(!super::prep::collect_index_files(config)?.is_empty())
}

/// Open the index database read-only IFF it exists and its schema is READABLE (current or
/// migrateable-forward); else `None`. Shared by the read-only pre-index hints so none of them abort
/// `index()` on a fresh, never-written, garbage, or unreadable-schema database — SQLite defers
/// header validation to the first page read, so a non-DB file opens fine and only faults on the
/// `status` read, which is folded to `None` here.
///
/// An `Older` (migrateable-forward) schema is accepted ONLY once the repo-registry tables it reads
/// exist. Accepting `Older` at all is deliberate: the `repos` / `repo_roots` / `repo_meta` rows the
/// `source_root` probe reads are stable across recent migrations, so rejecting a merely-behind DB
/// would misclassify an already-indexed repo on a pre-upgrade database as "never indexed" and
/// wrongly REFUSE a delete-to-empty prune as a first-time-empty registration (#427 review). But the
/// registry itself was ADDED in V038, and these read-only probes run BEFORE the normal write-open
/// migration — so a database OLDER than V038 is `Older` yet has NO `repos` table. Returning a
/// connection there would fault the probe with `no such table: repos` on `rag-rat index`/`--full`
/// instead of letting the write path migrate + index (#427 review). Gate on the table existing:
/// pre-registry `Older` DBs → `None` (treated as not-indexed / no join hint, exactly as before this
/// widening; the write path then migrates them). `Newer` / `Dirty` / `Missing` / garbage → `None`.
fn open_ro_compatible(config: &Config) -> Option<IndexConnection> {
    if !config.database.exists() {
        return None;
    }
    let storage = IndexConnection::open_read_only_blocking(&config.database).ok()?;
    let readable = match schema::status(storage.connection()) {
        Ok(status) => match status.state {
            schema::SchemaState::Compatible => true,
            // Pre-V038 databases lack the registry these probes read; fall through to migration.
            schema::SchemaState::Older =>
                schema::table_exists(storage.connection(), "repos").unwrap_or(false),
            _ => false,
        },
        Err(_) => false,
    };
    readable.then_some(storage)
}

/// The configured root is a NEW physical checkout sharing an already-registered repo's identity —
/// the same-identity clone / not-yet-anchored worktree of #427.
#[derive(Debug, Clone)]
pub struct SameIdentityJoin {
    /// The registered repo whose scope this checkout would join.
    pub repo_id: String,
    /// A representative recorded root of that repo (its earliest-registered checkout).
    pub existing_root: PathBuf,
}

/// `Some` when indexing `config` would fold its root into an ALREADY-REGISTERED repo's single scope
/// because they share a portable identity, and this root is not yet one of that repo's recorded
/// checkouts (#427). `None` — no warning — for a fresh DB, an incompatible schema, an
/// identity-less root, an unregistered identity, or a re-index of a KNOWN checkout. Read-only:
/// opens the DB `SQLITE_OPEN_READ_ONLY` and never writes. Returns `None` generously on any error
/// resolving identity (a false join warning is worse than a missed one).
pub fn same_identity_join_note(config: &Config) -> anyhow::Result<Option<SameIdentityJoin>> {
    // Fresh / never-written / garbage / incompatible DB → there is no existing repo to join.
    let Some(storage) = open_ro_compatible(config) else {
        return Ok(None);
    };
    let conn = storage.connection();
    let identity = match repo_identity::resolve_repo_identity(
        &config.root,
        config.repo_id_override.as_deref(),
    ) {
        Ok(identity) => identity,
        Err(_) => return Ok(None), // absent / rejected identity → no join hint
    };
    // Only a repo that is ALREADY registered can be "joined". A brand-new identity is a fresh repo.
    if !schema::repo_id_is_registered(conn, &identity.repo_id)? {
        return Ok(None);
    }
    // This exact checkout was already INDEXED as this repo → an ordinary re-index, not a join.
    // Via the shared indexing-only helper: recording is indexing-only, so a read-only `open_config`
    // (doctor / MCP) of a fresh same-identity clone does NOT suppress the warning — the first real
    // index from that clone still gets the `[index] repo_id` guidance (#427 review).
    if rag_rat_db::schema::repo_indexed_at_this_root(conn, &identity.repo_id, config)? {
        return Ok(None);
    }
    let existing = schema::earliest_recorded_root(conn, &identity.repo_id)?;
    Ok(existing.map(|existing_root| SameIdentityJoin {
        repo_id: identity.repo_id,
        existing_root: PathBuf::from(existing_root),
    }))
}

/// Whether THIS checkout has actually been INDEXED before — the signal that scopes the zero-file
/// refusal (#427) to FIRST-TIME empty registrations. An already-indexed root whose last file was
/// just deleted or moved must still be allowed to index, so the incremental / discover pass records
/// the now-empty set (applies the deletion plan) instead of stranding the old rows live; only a
/// brand-new checkout landing empty is the footgun worth refusing.
///
/// Judged by an INDEXING-ONLY signal — a `repo_roots` recording (git repos) or the persisted
/// `source_root` (non-git fallback) — see [`repo_indexed_at_this_root`]. Both are written only by a
/// real indexing pass, never by a read-only `open_config` (a `doctor` / MCP / query open now
/// registers identity via `register_repo_read_only`, which records NO root), so a mere read can't
/// make an un-indexed checkout — or a fresh same-identity CLONE — look indexed and let a later
/// empty `--discover` / `--full` prune the shared scope (#427 review). The per-checkout
/// `repo_roots` row (not the single-valued `source_root`) is also what keeps a same-identity
/// SIBLING clone's index from stealing this checkout's recognition. `Ok(false)` on a fresh /
/// never-written / garbage / unreadable database.
pub fn is_root_already_indexed(config: &Config) -> anyhow::Result<bool> {
    let Some(storage) = open_ro_compatible(config) else {
        return Ok(false);
    };
    rag_rat_db::schema::is_root_already_indexed_conn(storage.connection(), config)
}

/// Whether indexing `config` would FIRST-TIME-register an EMPTY repo — this checkout was not
/// indexed before AND no target files would be discovered. The #427 condition the two INDEXING
/// entry points check before they adopt: the one-shot `index` command turns it into an error
/// (unless `--allow-empty`), and the background paths (watcher, git-hook `maintenance`) DEFER on it
/// (skip, waiting for content) so `rag-rat mcp` / a hook on a misconfigured repo never silently
/// registers an empty index. The cheap already-indexed lookup runs first (via `&&` short-circuit),
/// so an established repo never pays the discovery walk. Read-only.
pub fn is_first_time_empty(config: &Config) -> anyhow::Result<bool> {
    Ok(!is_root_already_indexed(config)? && !would_discover_any_file(config)?)
}

/// [`is_first_time_empty`] against an ALREADY-OPEN connection (the incremental path's migrated
/// bare-open connection), checked BEFORE `adopt_repo_from_config` records the root.
pub fn is_first_time_empty_conn(
    conn: &rusqlite::Connection,
    config: &Config,
) -> anyhow::Result<bool> {
    Ok(!rag_rat_db::schema::is_root_already_indexed_conn(conn, config)?
        && !would_discover_any_file(config)?)
}

/// The core registration path (`rebuild_with_progress`) refused to FIRST-TIME-register an EMPTY
/// index (#427). This is the SINGLE enforcement of the empty-index invariant; callers react to it:
/// the one-shot `index` command surfaces it to the operator, while the background paths (watcher,
/// git-hook `maintenance`) discard it and wait for content. `--allow-empty` (→
/// `Config::allow_empty`) opts in, and this is then never produced.
#[derive(Debug, thiserror::Error)]
#[error(
    "0 files discovered under {root} — no `[target_bindings]` configured (or none match).\nAdd a \
     section like:\n\n    [target_bindings]\n    rust = [\"src\"]\n\nto rag-rat.toml, or pass \
     `--allow-empty` to index nothing."
)]
pub struct EmptyIndexRefused {
    pub root: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rag_rat_base::config::{Config, ResolvedTarget, TargetKind};
    use rag_rat_base::language::Language;

    use super::*;

    // `schema_bootstrap_tests::source_config` / `unique_temp_root` are private to that module and
    // not reachable from here (a sibling of `index`, not a descendant of `schema_bootstrap_tests`)
    // — build the fixtures inline instead of widening their visibility just for this test.
    /// A temp root unique across BOTH test runners. pid + millisecond is not: `cargo test` (what
    /// the coverage job runs) executes tests as THREADS IN ONE PROCESS, so two tests entering
    /// within the same millisecond derive the SAME path — and one test's `remove_dir_all` then
    /// races the other's `git init`, which fails with a bare "git [\"init\"] failed". nextest
    /// hides the race by giving each test its own process (its own pid). The atomic counter
    /// makes the name unique regardless of runner.
    fn unique_temp_root() -> PathBuf {
        static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);
        let mut root = std::env::temp_dir();
        root.push(format!(
            "rag-rat-adoption-hints-test-{}-{}-{}",
            std::process::id(),
            rag_rat_base::time::now_ms(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed),
        ));
        root
    }

    fn source_config(root: PathBuf, language: Language) -> Config {
        Config {
            trackers: Vec::new(),
            papertrail: Default::default(),
            sync: Default::default(),
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![ResolvedTarget {
                name: language.as_str().to_string(),
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
        let root = unique_temp_root();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        let mut config = source_config(root.clone(), Language::Rust);
        config.targets.clear(); // no [target_bindings] → nothing to walk
        assert!(!would_discover_any_file(&config).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn would_discover_any_file_is_true_when_a_target_matches() {
        let root = unique_temp_root();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        let config = source_config(root.clone(), Language::Rust);
        assert!(would_discover_any_file(&config).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    // Run a git command in `root`, panicking on failure — models
    // `query_api::oracle_surfacing_tests::git`, private to that module and not reachable here.
    fn git(root: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// A clone sharing a checkout's portable (root-commit-derived) identity, pointed at the SAME
    /// database (the consolidated-DB shape the #427 join hint targets), fires the note naming the
    /// original checkout; the original checkout re-indexing itself, and a config whose DB doesn't
    /// exist yet, both stay quiet.
    #[test]
    fn same_identity_join_note_fires_for_a_second_checkout_and_not_the_first() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // no git on PATH — skip rather than fail.
        }
        let root_a = unique_temp_root();
        std::fs::create_dir_all(root_a.join("src")).unwrap();
        std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
        git(&root_a, &["init", "-q"]);
        git(&root_a, &["add", "-A"]);
        git(&root_a, &["commit", "-q", "-m", "init"]);
        let config_a = source_config(root_a.clone(), Language::Rust);

        // No DB yet at all — nothing to join.
        assert!(same_identity_join_note(&config_a).unwrap().is_none());

        crate::index::IndexDatabase::rebuild(&config_a).unwrap(); // registers A at root_a

        // A's own recorded checkout re-indexing itself is an ordinary re-index, not a join.
        assert!(same_identity_join_note(&config_a).unwrap().is_none());

        // A full clone of A shares its portable (root-commit) identity but is a NEW checkout,
        // pointed at A's SAME database (config_b keeps config_a's `database`, only the `root`
        // moves) — the consolidated-DB shape the hint exists for.
        let root_b = PathBuf::from(format!("{}-clone", root_a.display()));
        let _ = std::fs::remove_dir_all(&root_b);
        git(&root_a, &["clone", "-q", ".", root_b.to_str().unwrap()]);
        let mut config_b = config_a.clone();
        config_b.root = root_b.clone();

        let expected_repo_id =
            rag_rat_base::repo_identity::resolve_repo_identity(&root_a, None).unwrap().repo_id;
        let note = same_identity_join_note(&config_b).unwrap().unwrap();
        assert_eq!(note.repo_id, expected_repo_id);
        assert_eq!(note.existing_root, root_a);

        let _ = std::fs::remove_dir_all(&root_a);
        let _ = std::fs::remove_dir_all(&root_b);
    }

    /// A garbage/non-DB file at `config.database` must yield `Ok(None)` (no hint), NEVER `Err`:
    /// SQLite defers header validation to the first page read, so `open_read_only_blocking`
    /// succeeds on junk content and `schema::status` is the first read to fault. A propagated error
    /// here would abort `index()` (the CLI calls `same_identity_join_note(config)?`) — a new
    /// failure mode the generous-`None` contract exists to prevent.
    #[test]
    fn same_identity_join_note_is_none_on_a_garbage_db_file() {
        let root = unique_temp_root();
        let config = source_config(root.clone(), Language::Rust);
        std::fs::create_dir_all(config.database.parent().unwrap()).unwrap();
        std::fs::write(&config.database, b"not a sqlite database at all\x00\xff").unwrap();
        assert!(config.database.exists());

        let result = same_identity_join_note(&config);
        assert!(result.is_ok(), "a garbage DB must not abort the index: {result:?}");
        assert!(result.unwrap().is_none(), "a garbage DB yields no join hint");
        let _ = std::fs::remove_dir_all(root);
    }

    /// `is_root_already_indexed` is `false` before indexing (no DB / fresh) and `true` after a
    /// rebuild has recorded this checkout — the signal that scopes the #427 empty-index refusal to
    /// FIRST-TIME registrations, so a later delete-to-empty is allowed to prune rather than
    /// refused. It keys off an INDEXING-ONLY signal (the recorded root / source_root), NOT the
    /// shared identity: a fresh clone sharing A's identity is never indexed, so it stays
    /// `false` and an empty clone can't prune A's shared scope.
    #[test]
    fn is_root_already_indexed_tracks_indexing_not_the_shared_identity() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // no git on PATH — skip rather than fail.
        }
        let root_a = unique_temp_root();
        std::fs::create_dir_all(root_a.join("src")).unwrap();
        std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
        git(&root_a, &["init", "-q"]);
        git(&root_a, &["add", "-A"]);
        git(&root_a, &["commit", "-q", "-m", "init"]);
        let config_a = source_config(root_a.clone(), Language::Rust);

        // No database yet → not indexed.
        assert!(!is_root_already_indexed(&config_a).unwrap());

        crate::index::IndexDatabase::rebuild(&config_a).unwrap();

        // A's recorded root → indexed (so a delete-to-empty on A would be allowed to prune).
        assert!(is_root_already_indexed(&config_a).unwrap());

        // A full clone of A shares A's portable identity but its root is NOT recorded — it must NOT
        // count as already-indexed, or an empty clone could prune A's shared scope (#427 review).
        let root_b = PathBuf::from(format!("{}-clone", root_a.display()));
        let _ = std::fs::remove_dir_all(&root_b);
        git(&root_a, &["clone", "-q", ".", root_b.to_str().unwrap()]);
        let mut config_b = config_a.clone();
        config_b.root = root_b.clone();
        assert!(
            !is_root_already_indexed(&config_b).unwrap(),
            "a same-identity clone with an unrecorded root is NOT already-indexed"
        );

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    /// An identity-less (NON-git) root gets NO `repo_roots` entry — `adopt_repo_from_config` falls
    /// back to the sole placeholder repo without recording the root — yet after indexing its repo's
    /// persisted `source_root` equals the root, so it must still count as already-indexed (#427
    /// review). Otherwise a delete-to-empty on a non-git project would be wrongly refused as
    /// first-time instead of pruning its stale rows.
    #[test]
    fn is_root_already_indexed_recognizes_an_indexed_non_git_root() {
        let root = unique_temp_root();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        let config = source_config(root.clone(), Language::Rust); // NOT a git repo

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

        let _ = std::fs::remove_dir_all(root);
    }

    /// #427 review (comment 4): a READ-ONLY open (`doctor` / MCP / query via `open_config`) adopts
    /// identity but must NOT record the checkout's root (it goes through `register_repo_read_only`)
    /// nor write `repo_meta[source_root]` — both are indexing-only. So a mere read must NOT flip
    /// `is_root_already_indexed` to `true`, or a later empty `--discover` / `--full` on that
    /// no-target repo would be waved through as "already indexed" and prune the scope. B is
    /// read-registered into A's shared DB but never indexed, so it stays first-time-empty.
    #[test]
    fn a_read_only_open_does_not_make_an_unindexed_repo_look_indexed() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // identity resolution needs git; skip rather than fail.
        }
        // Repo A: a committed git repo, actually indexed into a shared DB (persists A's
        // source_root).
        let root_a = unique_temp_root();
        std::fs::create_dir_all(root_a.join("src")).unwrap();
        std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
        git(&root_a, &["init", "-q"]);
        git(&root_a, &["add", "-A"]);
        git(&root_a, &["commit", "-q", "-m", "init"]);
        let config_a = source_config(root_a.clone(), Language::Rust);
        crate::index::IndexDatabase::rebuild(&config_a).unwrap();

        // Repo B: a DISTINCT git repo (own init → own root commit → own identity), pointed at A's
        // SAME database but with NO discoverable target files. A read-only `open_config` registers
        // B's identity WITHOUT indexing anything and WITHOUT recording its root.
        let root_b = unique_temp_root();
        std::fs::create_dir_all(root_b.join("src")).unwrap();
        std::fs::write(root_b.join("keep.txt"), "not a rust file\n").unwrap();
        git(&root_b, &["init", "-q"]);
        git(&root_b, &["add", "-A"]);
        git(&root_b, &["commit", "-q", "-m", "init"]);
        let mut config_b = source_config(root_b.clone(), Language::Rust);
        config_b.database = config_a.database.clone(); // shared DB

        let _ = crate::index::IndexDatabase::open_config(&config_b).unwrap();

        // The read recorded NO root for B (register_repo_read_only) and ran no indexing pass, so B
        // is NOT already-indexed and a zero-file index on B is still first-time-empty.
        {
            let conn = rusqlite::Connection::open(&config_b.database).unwrap();
            let recorded: i64 = conn
                .query_row(
                    "SELECT count(*) FROM repo_roots WHERE root = ?1",
                    [root_b.to_string_lossy()],
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

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    /// #427 review ("Gate Older schemas on registry availability"): a LEGACY index predating the
    /// V038 repo-registry is `Older` but has NO `repos` / `repo_meta` tables. The read-only
    /// pre-index probes run BEFORE the write-open migration, so on such a DB they must NOT fault
    /// with `no such table: repos` (which would break `rag-rat index` / `--full` on an upgrade) —
    /// they treat it as not-yet-indexed / no-join and let the write path migrate it. `Ok`, not
    /// `Err`.
    #[test]
    fn a_pre_registry_legacy_schema_does_not_fault_the_readonly_probes() {
        let root = unique_temp_root();
        let config = source_config(root.clone(), Language::Rust);
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

        let _ = std::fs::remove_dir_all(root);
    }

    /// #427 review ("Recognize legacy placeholder indexes before refusing empties"): a LEGACY index
    /// still living under the `__unassigned__` placeholder (a pre-adoption DB — simulated here by
    /// indexing the root while it was NON-git, then giving it a git identity the DB has not
    /// adopted) must count as already-indexed via the SOLE placeholder's `source_root`.
    /// Otherwise a delete-to-empty on it is refused as first-time instead of pruning on the
    /// first upgrade run.
    #[test]
    fn is_root_already_indexed_recognizes_a_legacy_placeholder_index() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // needs git for the (unregistered) identity; skip rather than fail.
        }
        let root = unique_temp_root();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
        let config = source_config(root.clone(), Language::Rust);

        // Index while NON-git: `adopt_repo_from_config` sees an ABSENT identity and adopts the sole
        // `__unassigned__` placeholder, persisting `source_root` under it — a pre-adoption-style
        // DB.
        crate::index::IndexDatabase::rebuild(&config).unwrap();

        // Now give the root a git identity the DB has NOT adopted (still under the placeholder), so
        // `resolve_config_repo_id` returns None and only the sole-repo fallback can recognize it.
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);

        assert!(
            is_root_already_indexed(&config).unwrap(),
            "a legacy placeholder index must be recognized via the sole repo's source_root"
        );
        // So a delete-to-empty prunes rather than being refused as first-time-empty.
        std::fs::remove_file(root.join("src/a.rs")).unwrap();
        assert!(!is_first_time_empty(&config).unwrap());

        let _ = std::fs::remove_dir_all(root);
    }

    /// #427 review ("Warn even when a read open touched the checkout"): a read-only `open_config`
    /// (doctor / MCP) on a fresh same-identity clone registers its identity but neither indexes it
    /// nor records its root. The same-identity-join warning must STILL fire on the first real index
    /// — suppressing it on a mere read would let the clone silently switch the shared scope with
    /// none of the `[index] repo_id` guidance.
    #[test]
    fn same_identity_join_warns_even_after_a_read_only_open_of_the_clone() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let root_a = unique_temp_root();
        std::fs::create_dir_all(root_a.join("src")).unwrap();
        std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
        git(&root_a, &["init", "-q"]);
        git(&root_a, &["add", "-A"]);
        git(&root_a, &["commit", "-q", "-m", "init"]);
        let config_a = source_config(root_a.clone(), Language::Rust);
        crate::index::IndexDatabase::rebuild(&config_a).unwrap(); // registers A into the shared DB

        // Clone B shares A's identity, pointed at A's SAME database.
        let root_b = PathBuf::from(format!("{}-clone", root_a.display()));
        let _ = std::fs::remove_dir_all(&root_b);
        git(&root_a, &["clone", "-q", ".", root_b.to_str().unwrap()]);
        let mut config_b = config_a.clone();
        config_b.root = root_b.clone();

        // A read-only open registers B's identity but records no root and indexes nothing.
        let _ = crate::index::IndexDatabase::open_config(&config_b).unwrap();

        // The join warning must still fire — the read must not suppress it.
        let note = same_identity_join_note(&config_b).unwrap();
        assert!(
            note.is_some(),
            "a read-only-opened clone must still warn it joins the shared scope"
        );
        assert_eq!(note.unwrap().existing_root, root_a);

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    /// #427 review ("Preserve pruning for earlier same-identity checkouts"): when checkout A has
    /// indexed the shared repo and a same-identity checkout B then indexes it too, B's pass does
    /// NOT steal A's already-indexed status — each checkout records its OWN `repo_roots` row
    /// (the single-valued `source_root` would be last-writer-wins, flipped to B). So A deleting
    /// its last file and re-indexing to empty is still ALLOWED to prune, not refused as
    /// first-time-empty.
    #[test]
    fn a_sibling_index_does_not_steal_an_earlier_checkouts_prune_right() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        // A: committed git repo, indexed into a shared DB.
        let root_a = unique_temp_root();
        std::fs::create_dir_all(root_a.join("src")).unwrap();
        std::fs::write(root_a.join("src/a.rs"), "fn a() {}\n").unwrap();
        git(&root_a, &["init", "-q"]);
        git(&root_a, &["add", "-A"]);
        git(&root_a, &["commit", "-q", "-m", "init"]);
        let config_a = source_config(root_a.clone(), Language::Rust);
        crate::index::IndexDatabase::rebuild(&config_a).unwrap();
        assert!(is_root_already_indexed(&config_a).unwrap(), "A is indexed after its rebuild");

        // B: a same-identity clone of A, pointed at the SAME DB, INDEXED too (has its own file).
        let root_b = PathBuf::from(format!("{}-clone", root_a.display()));
        let _ = std::fs::remove_dir_all(&root_b);
        git(&root_a, &["clone", "-q", ".", root_b.to_str().unwrap()]);
        let mut config_b = config_a.clone();
        config_b.root = root_b.clone();
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

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }
}
