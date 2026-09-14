use super::*;

/// Test-local probe: is `repo_id` a registered real repo? (mirrors the private
/// `repo_id_is_registered`, kept here so the test above needn't expose it.)
fn repo_id_is_registered_probe(conn: &rusqlite::Connection, repo_id: &str) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM repos WHERE repo_id = ?1)", [repo_id], |r| r.get(0))
        .unwrap()
}

/// The read-only resolver MIRRORS `register_repo`'s root-owner refusal: an `[index] repo_id` pin
/// switched to a SIBLING's id at a root recorded under another repo must NOT resolve on the
/// read-only fast path (MCP reads / hooks would silently serve the sibling's scope while the
/// write path refuses). `None` declines the fast path so the read-write open surfaces the refusal.
#[test]
fn read_only_resolver_declines_a_pin_onto_a_root_owned_by_another_repo() {
    let conn = fresh_conn();
    register_repo(
        &conn,
        &identity("repo-a", "a"),
        Path::new("/src/a"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    register_repo(
        &conn,
        &identity("repo-b", "b"),
        Path::new("/src/b"),
        2,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    // The pin names repo-b, but /src/a is recorded under repo-a → decline (mirror of the write
    // path's mismatched-root refusal).
    let resolved =
        schema::resolve_config_repo_id(&conn, Path::new("/src/a"), Some("repo-b")).unwrap();
    assert_eq!(resolved, None, "a sibling-id pin at an owned root must not fast-path resolve");

    // Control: the OWNING repo's own pin at its root still resolves.
    let resolved =
        schema::resolve_config_repo_id(&conn, Path::new("/src/a"), Some("repo-a")).unwrap();
    assert_eq!(resolved.as_deref(), Some("repo-a"), "self-ownership resolves normally");
}

// --- Read-path repo resolution without registering (#413 round-4 findings #1 + #2) ---

/// `resolve_config_repo_id` (the read-path resolver behind the read-only open + the raw scope-view
/// hooks) binds the repo a config's ROOT is recorded under, NOT the config-blind sole repo. In a
/// consolidated DB the sole pick could be a sibling; the recorded-root route keeps a read scoped to
/// the config's own repo even for a non-git root that has no derivable identity.
#[test]
fn resolve_config_repo_id_binds_a_recorded_root_over_the_sole_pick() {
    let conn = fresh_conn();
    // Repo A adopted (sorts first). A sibling repo B seeded directly with its recorded root — the
    // A7 consolidated shape (register_repo forbids a second real repo before A7).
    register_repo(
        &conn,
        &identity("repo-a", "a"),
        Path::new("/src/a"),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('sibling-b', 'b', 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO repo_roots(repo_id, root, registered_at_ms) VALUES ('sibling-b', ?1, 0)",
        [Path::new("/src/b").to_string_lossy().as_ref()],
    )
    .unwrap();

    // The config-blind sole pick is repo-a (lexicographically smallest) — the WRONG repo for
    // /src/b.
    assert_eq!(schema::sole_repo_id(&conn).unwrap(), "repo-a");
    // The resolver binds the repo the ROOT is recorded under (a non-git root → the by-root route).
    assert_eq!(
        schema::resolve_config_repo_id(&conn, Path::new("/src/b"), None).unwrap().as_deref(),
        Some("sibling-b"),
        "a recorded root binds its own repo, not the smaller sole pick",
    );
    // A single-repo DB with an unrecorded, non-git root still falls back to the sole repo
    // (preserves the pre-A3 read fast path) — asserted here by an unknown root resolving to
    // None on this CONSOLIDATED DB (>1 real repo → cannot prove, bind nothing rather than a
    // sibling).
    assert_eq!(
        schema::resolve_config_repo_id(&conn, Path::new("/src/unknown"), None).unwrap(),
        None,
        "an unprovable root on a consolidated DB resolves to None (never a sibling)",
    );
}

/// A `Rejected` config (a reserved `[index] repo_id` pin) resolves to `None` on the read path — it
/// must NOT silently bind a repo; the read-write open surfaces the actionable error instead.
#[test]
fn resolve_config_repo_id_returns_none_for_a_rejected_pin() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["commit", "-q", "--allow-empty", "-m", "genesis"]);

    let conn = fresh_conn();
    register_repo(
        &conn,
        &identity("repo-a", "a"),
        root.as_path(),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    // A reserved pin is the Rejected class → None, even though the root is a real registered repo.
    assert_eq!(
        schema::resolve_config_repo_id(&conn, &root, Some(LEGACY_REPO_ID)).unwrap(),
        None,
        "a reserved-id pin does not resolve on the read path — it surfaces via the read-write open",
    );
    let _ = fs::remove_dir_all(&root);
}

/// #413 round-5: a NEW, unregistered `[index] repo_id` pin resolves to `None` on the read path —
/// NOT to the repo the root was previously registered under. A changed identity must adopt/surface
/// on the read-write open; the read-only path declining is what forces that, instead of silently
/// serving the old scope under the new pin.
#[test]
fn resolve_config_repo_id_returns_none_for_a_new_unregistered_pin() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["commit", "-q", "--allow-empty", "-m", "genesis"]);

    let conn = fresh_conn();
    // The root is registered (and recorded) under repo-a — so the pre-fix by-root fallback would
    // resolve the new pin to repo-a.
    register_repo(
        &conn,
        &identity("repo-a", "a"),
        root.as_path(),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();

    assert_eq!(
        schema::resolve_config_repo_id(&conn, &root, Some("brand-new-pin")).unwrap(),
        None,
        "a new unregistered pin declines on the read path — it does not bind the old (repo-a) \
         scope",
    );
    let _ = fs::remove_dir_all(&root);
}

/// #413 round-5, the shallow-upgrade sibling case: a repo registered under a `local:` id whose
/// full-history root now derives a DIFFERENT (portable) id. `resolve_config_repo_id` with no pin
/// derives the unregistered portable id and must return `None` — the LocalOnly→Portable upgrade
/// belongs on the read-write path (`register_repo`), not a silent read-path rebind to the old
/// `local:` scope.
#[test]
fn resolve_config_repo_id_returns_none_for_a_newly_portable_local_incumbent() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "t@e"]);
    run_git(&root, &["config", "user.name", "t"]);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "one"]);

    // The portable id the full-history root derives (what a deepened clone would resolve to).
    let portable_id =
        rag_rat_base::repo_identity::resolve_repo_identity(&root, None).unwrap().repo_id;
    assert!(!portable_id.starts_with("local:"), "full history → a portable root id");

    let conn = fresh_conn();
    // Incumbent: registered (and root recorded) under a machine-local id, as a prior shallow index.
    register_repo(
        &conn,
        &identity_local("local:beef", "shallow", vec![]),
        root.as_path(),
        1,
        &crate::index::migration_hooks(),
    )
    .unwrap();
    assert!(!repo_id_is_registered_probe(&conn, &portable_id), "portable id is not yet registered");

    // No pin: the identity route derives the portable id (unregistered) → None. The pre-fix by-root
    // fallback would rebind to the incumbent `local:beef`.
    assert_eq!(
        schema::resolve_config_repo_id(&conn, &root, None).unwrap(),
        None,
        "a newly-portable identity declines on the read path — upgrade happens on the write path",
    );
    let _ = fs::remove_dir_all(&root);
}

// --- Identity-resolution error classes at the open_config boundary (A3) ---

/// A pinned RESERVED `[index] repo_id` must SURFACE from `open_config` — the `Rejected` class of
/// `RepoIdentityError`. The old blanket fallback silently scoped the DB to the placeholder, hiding
/// the configuration problem and leaving every row unadopted under the legacy id.
#[test]
fn open_config_surfaces_a_reserved_repo_id_pin() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let mut config = source_config(root.clone(), Language::Rust);
    config.repo_id_override = Some(LEGACY_REPO_ID.to_string());
    // `open_config` opens an EXISTING index (a Missing schema refuses); create it first, exactly
    // like the `rag-rat index` → later plain opens sequence.
    IndexDatabase::migrate(&config.database).unwrap();

    let err = IndexDatabase::open_config(&config)
        .expect_err("a reserved-id pin is a rejection, never a silent placeholder fallback");
    assert!(err.to_string().contains("reserved"), "error names the rejection: {err}");
    let _ = fs::remove_dir_all(&root);
}

/// A cut shallow clone (its root commit unreachable, so a derived id would be depth-dependent) does
/// NOT fail through `open_config`: it adopts under a deterministic `local:`-prefixed LocalOnly id
/// and proceeds. Blocking a `--depth 1` checkout would break CI fixtures for no benefit — the id is
/// stable on this machine, only not portable across machines (the sync layer enforces that later).
#[test]
fn open_config_adopts_a_shallow_clone_under_a_local_only_id() {
    let base = unique_temp_root();
    let _ = fs::remove_dir_all(&base);
    let origin = base.join("origin");
    fs::create_dir_all(origin.join("src")).unwrap();
    fs::write(origin.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    run_git(&origin, &["init", "-q", "-b", "main"]);
    run_git(&origin, &["config", "user.email", "t@e"]);
    run_git(&origin, &["config", "user.name", "t"]);
    run_git(&origin, &["add", "."]);
    run_git(&origin, &["commit", "-q", "-m", "one"]);
    run_git(&origin, &["commit", "-q", "--allow-empty", "-m", "two"]);
    // --depth 1 < history: the clone's root commit is unreachable (a genuinely CUT shallow clone).
    let url = format!("file://{}", origin.display());
    run_git(&base, &["clone", "-q", "--depth", "1", &url, "clone"]);
    let clone_root = base.join("clone");

    let config = source_config(clone_root, Language::Rust);
    IndexDatabase::migrate(&config.database).unwrap();
    let db = IndexDatabase::open_config(&config)
        .expect("a cut shallow clone adopts under a LocalOnly id, it does not fail");
    assert!(
        db.active_repo_id.starts_with("local:"),
        "a cut shallow clone adopts under a LocalOnly id, got {}",
        db.active_repo_id
    );
    // Adopted as a real repo: the placeholder is gone and the LocalOnly id owns the registry.
    assert_eq!(repo_row_count(db.storage.connection(), LEGACY_REPO_ID), 0, "placeholder adopted");
    assert_eq!(
        repo_row_count(db.storage.connection(), &db.active_repo_id),
        1,
        "LocalOnly repo row"
    );
    let _ = fs::remove_dir_all(&base);
}

/// A NON-git root (no identity to derive at all) is the EXPECTED-absence class: `open_config`
/// still opens, scoped to the sole repo of the single-repo DB (the placeholder on a fresh one) —
/// the pre-A3 behavior every bare temp-dir index relies on.
#[test]
fn open_config_falls_back_to_the_sole_repo_on_a_non_git_root() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let config = source_config(root.clone(), Language::Rust);
    IndexDatabase::migrate(&config.database).unwrap();

    let db = IndexDatabase::open_config(&config)
        .expect("expected absence (not a git repo) falls back, it does not error");
    assert_eq!(
        db.active_repo_id, LEGACY_REPO_ID,
        "the un-adopted single-repo DB scopes to the placeholder"
    );
    let _ = fs::remove_dir_all(&root);
}

/// The STRUCTURAL BACKSTOP for the identity gate's second entrance (Codex batch 8, finding 5): an
/// identity-less root (non-git) whose explicit `database` pin lands on a MULTI-repo store must
/// never sole-pick — `sole_repo_id`'s lexicographic tiebreak would silently adopt the first
/// SIBLING repo and write this project's rows under it. The open REFUSES with the remedy, and the
/// siblings' registry state is untouched. (The global-path pin shape is already refused at
/// `Config::load`; this closes every other shared-path entrance — the same doctrine as the
/// config-less bare-open fail-fast and the healers' witness.)
#[test]
fn an_identity_less_open_refuses_to_sole_pick_on_a_multi_repo_db() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let mut config = source_config(root.clone(), Language::Rust);
    // The pin points at a SHARED store that already holds two sibling repos.
    config.database = root.join("shared.sqlite");
    IndexDatabase::migrate(&config.database).unwrap();
    {
        let conn = rusqlite::Connection::open(&config.database).unwrap();
        for repo in ["repo-alpha", "repo-beta"] {
            conn.execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
                [repo],
            )
            .unwrap();
        }
    }

    let err = IndexDatabase::open_config(&config)
        .expect_err("an identity-less root must not guess a repo on a multi-repo store");
    assert!(
        err.to_string().contains("no resolvable repo identity")
            && err.to_string().contains("repo_id"),
        "the refusal names the problem and the remedy: {err}"
    );
    // The siblings are untouched — no adoption, no placeholder re-point, no root recorded.
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    let repos: i64 = conn
        .query_row("SELECT COUNT(*) FROM repos WHERE repo_id != ?1", [LEGACY_REPO_ID], |r| r.get(0))
        .unwrap();
    assert_eq!(repos, 2, "both siblings still registered, nothing adopted");
    let roots: i64 = conn.query_row("SELECT COUNT(*) FROM repo_roots", [], |r| r.get(0)).unwrap();
    assert_eq!(roots, 0, "no root was recorded for the refused open");
    let _ = fs::remove_dir_all(&root);
}
