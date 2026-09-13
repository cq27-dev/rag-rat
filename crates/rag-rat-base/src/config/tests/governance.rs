use super::*;

#[test]
fn config_load_resolves_main_and_linked_worktrees_to_one_database() {
    // The actual guarantee (review item 1): Config::load from the main worktree and from a
    // linked worktree of the same repo produce the *same* database path — not two DBs.
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("cfgload");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

    let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(
        from_main.database, from_linked.database,
        "main and linked worktrees must share one index database",
    );
    assert_eq!(
        from_main.database,
        crate::paths::canonicalize(&main).unwrap().join(".rag-rat/index.sqlite")
    );
    // AND the `root` anchors to the main worktree from either launch point — so every process
    // uses the same base commit for the shared index, instead of a worktree-launched one
    // rooting at the worktree (a different base → conflicting overlay writes /
    // readable-vs-tombstone races) (#218/#219).
    assert_eq!(from_main.root, from_linked.root, "main and linked configs resolve to one root");
    assert_eq!(
        from_linked.root,
        crate::paths::canonicalize(&main).unwrap(),
        "a linked worktree's config root anchors to the main worktree",
    );
}

#[test]
fn repo_id_override_is_parsed_and_does_not_change_the_database_path() {
    let tmp = scratch("repoid");
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\nrepo_id = \"  pinned-id  \
         \"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();

    let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
    assert_eq!(
        config.repo_id_override.as_deref(),
        Some("pinned-id"),
        "the [index] repo_id override is parsed and trimmed",
    );
    // Parse-only: the override must NOT influence path resolution — the explicit database stays
    // at the per-repo path beside `root`.
    assert_eq!(config.database, config.root.join(".rag-rat/index.sqlite"));
}

/// Seed a minimal COMMITTED git repo at `dir` — the identity-bearing fixture the global
/// default requires (a keyless config resolves globally only for a root with a derivable repo
/// identity).
fn git_commit_all(dir: &Path) {
    let git = |args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@e"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "seed"]);
}

/// A7 default flip: a keyless config in an IDENTITY-BEARING repo (a committed git root) with
/// no legacy `.rag-rat/index.sqlite` resolves to the consolidated GLOBAL store. Compared
/// against `global_database_path()` in the CURRENT environment (no env mutation ⇒ no
/// cross-test race); `Config::load` only RESOLVES the path, it never opens or creates the DB,
/// so this never touches a developer's real global store.
#[test]
fn config_load_without_a_database_key_resolves_to_the_global_database() {
    let _env = crate::data_dir::env_guard();
    let tmp = scratch("globaldb");
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git_commit_all(&tmp);

    let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
    let expected = crate::data_dir::global_database_path()
        .expect("a data dir resolves in the test environment (HOME is set)");
    assert_eq!(
        config.database, expected,
        "a keyless config defaults to the consolidated global database",
    );
}

/// The GOVERNING SEAM: in a linked worktree the MAIN config governs the WHOLE config, not a
/// per-key subset — a divergent branch-local file cannot fork the embedding model (or any
/// other key) even though no per-key anchoring was ever written for `[llm]`. The two loads
/// must produce the SAME resolved `Config`.
#[test]
fn config_load_in_a_linked_worktree_is_governed_wholesale_by_the_main_config() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("wholecfg");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"main.sqlite\"\n[watch]\ndebounce_ms = \
         1111\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

    // The branch config diverges on a key with NO historical per-key anchoring: `[watch]`.
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"branch.sqlite\"\n[watch]\ndebounce_ms = \
         9999\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(
        from_linked.watch.debounce_ms, from_main.watch.debounce_ms,
        "the divergent branch config is IGNORED wholesale — keys with no per-key anchoring \
         history included",
    );
    assert_eq!(from_linked.watch.debounce_ms, 1111, "main's value, not the branch's 9999");
    assert_eq!(from_linked.database, from_main.database);
    assert_eq!(from_linked.root, from_main.root);
    assert_eq!(from_linked.targets, from_main.targets);
}

/// Config-less-main fallback posture: main is resolvable but has NO `rag-rat.toml`, so the
/// linked worktree's local config governs best-effort (with a warning) — root still anchors
/// to main so the shared index keys off one base checkout.
#[test]
fn config_load_falls_back_to_the_local_config_when_main_has_none() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("nomaincfg");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

    // Only the LINKED checkout has a config (e.g. authored on a branch, not yet merged).
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"branch.sqlite\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let cfg = Config::load(linked.join("rag-rat.toml")).unwrap();
    let canonical_main = crate::paths::canonicalize(&main).unwrap();
    assert_eq!(cfg.root, canonical_main, "root anchors to main even on the fallback");
    assert_eq!(
        cfg.database,
        canonical_main.join("branch.sqlite"),
        "the local key governs (resolved against the main top) until main gains a config",
    );
    assert!(cfg.database_key_pinned);
}

/// The DISCOVERY resolver matrix (Codex batch 9): local file wins wherever it exists (the
/// seam then governs + warns), a linked checkout without one resolves to MAIN's path (even
/// when that file doesn't exist yet — hints must name where the config belongs), and
/// main/non-git checkouts stay local.
#[test]
fn discover_config_path_resolves_the_governing_checkout() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("discover");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
    let main_c = crate::paths::canonicalize(&main).unwrap();

    // Linked, no local file, main config not yet written: MAIN's path (where it belongs).
    assert_eq!(config::discover_config_path(&linked), main_c.join("rag-rat.toml"));
    // Main checkout: always local, present or not.
    assert_eq!(config::discover_config_path(&main), main.join("rag-rat.toml"));
    // Linked WITH a local (divergent) file: the local path — the load then routes through
    // the governing seam, which warns; discovery must not silently skip that.
    std::fs::write(linked.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();
    assert_eq!(config::discover_config_path(&linked), linked.join("rag-rat.toml"));
    // Non-git: local.
    let plain = tmp.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(config::discover_config_path(&plain), plain.join("rag-rat.toml"));
}

/// The ANCESTOR-WALK arm (non-worktree): a launch from a SUBDIRECTORY of a rag-rat repo
/// resolves to the repo root's `rag-rat.toml` instead of dying at the local existence
/// check, while a genuinely config-less tree still yields the local (non-existent) path for
/// the hint. Also guards the relative-path footgun the walk fixed:
/// `nearest_config_at_or_above` must resolve a `.`-style dir to ABSOLUTE before climbing —
/// a relative `parent()` is `Some("")` then `None`, so the walk would never leave the
/// starting dir.
#[test]
fn discover_config_path_walks_up_to_a_parent_repo_config() {
    let tmp = scratch("walkup");
    let repo = tmp.join("repo");
    let nested = repo.join("crates").join("cli").join("src");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(repo.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();

    // The walk returns a canonical absolute path (a found file), so compare canonically — temp
    // roots can be symlinked (macOS `/tmp` → `/private/tmp`).
    let want = crate::paths::canonicalize(repo.join("rag-rat.toml")).unwrap();
    assert_eq!(
        crate::paths::canonicalize(config::discover_config_path(&nested)).unwrap(),
        want,
        "subdir → repo cfg"
    );
    assert_eq!(
        crate::paths::canonicalize(config::discover_config_path(&repo)).unwrap(),
        want,
        "repo root → local"
    );

    // A config-less tree with NO ancestor config: the local (non-existent) path, unchanged —
    // the not-found fallback returns the original `dir/rag-rat.toml` for the hint, uncanonical.
    let bare = tmp.join("bare").join("deep");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(config::discover_config_path(&bare), bare.join("rag-rat.toml"));
}

/// The ancestor walk STOPS at the enclosing git repo root: a nested checkout / submodule with
/// no `rag-rat.toml` of its own must NOT bind to an indexed PARENT repo's config — that
/// would point searches and (worse) memory writes at the wrong repository (#611 review,
/// P2).
#[test]
fn discover_config_path_does_not_cross_a_nested_repo_boundary() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("nested");
    let parent = tmp.join("parent");
    let nested = parent.join("vendor").join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    // Parent IS a rag-rat repo (git + rag-rat.toml at its root).
    git(&parent, &["init", "-q"]);
    std::fs::write(parent.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();
    // `nested` is its OWN git repo (submodule-like), with no rag-rat.toml.
    git(&nested, &["init", "-q"]);

    // Launched from the nested repo, discovery stays WITHIN it (no toml) → its local path,
    // never the parent's config.
    let got = config::discover_config_path(&nested);
    assert_eq!(got, nested.join("rag-rat.toml"), "must not adopt the parent repo's config");
    assert_ne!(
        crate::paths::canonicalize(&got).ok(),
        crate::paths::canonicalize(parent.join("rag-rat.toml")).ok(),
        "the parent repo's rag-rat.toml must not leak across the nested boundary",
    );
}

/// A SUBDIRECTORY launch inside a LINKED worktree finds a BRANCH-LOCAL `rag-rat.toml` at the
/// worktree root (routing the load through the governing seam + divergence warning) instead of
/// jumping straight to main; with no branch-local config anywhere in the worktree it still
/// resolves to MAIN's path (the governing-seam invariant). #611 review, P2 (linked arm).
#[test]
fn discover_config_path_finds_a_branch_local_config_from_a_linked_worktree_subdir() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("wtsub");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
    let main_c = crate::paths::canonicalize(&main).unwrap();
    let sub = linked.join("src");
    std::fs::create_dir_all(&sub).unwrap();

    // No branch-local config anywhere in the worktree: a subdir launch resolves to MAIN's path.
    assert_eq!(
        config::discover_config_path(&sub),
        main_c.join("rag-rat.toml"),
        "invariant: → main"
    );

    // A branch-local config at the LINKED worktree root: the subdir launch now finds IT (never
    // climbing past the worktree root into main).
    std::fs::write(linked.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();
    assert_eq!(
        crate::paths::canonicalize(config::discover_config_path(&sub)).unwrap(),
        crate::paths::canonicalize(linked.join("rag-rat.toml")).unwrap(),
        "a subdir launch must find the branch-local config, not jump to main",
    );
}

/// The linked-ness PRIMITIVE (Codex batch 8, findings 1+3): topology-derived — the discovered
/// checkout's workdir vs the designated main — so a SUBDIRECTORY of the main worktree is NOT
/// linked (pre-fix, `init` from `main/src` falsely refused), while any path inside a linked
/// checkout (its top OR a subdir) is.
#[test]
fn linked_worktree_main_root_derives_linkedness_from_topology() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("linkpred");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
    let main_c = crate::paths::canonicalize(&main).unwrap();

    assert_eq!(config::linked_worktree_main_root(&main), None, "the main worktree is not linked");
    assert_eq!(
        config::linked_worktree_main_root(&main.join("src")),
        None,
        "a SUBDIRECTORY of main is main — not linked (the false-refusal bug)",
    );
    assert_eq!(config::linked_worktree_main_root(&linked), Some(main_c.clone()));
    assert_eq!(
        config::linked_worktree_main_root(&linked.join("src")),
        Some(main_c),
        "a subdir of a linked checkout is still linked",
    );
    let plain = tmp.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(config::linked_worktree_main_root(&plain), None, "non-git has no designated main");
}

/// Validation ORDERING (Codex batch 8, finding 2): the governing config is chosen FIRST; hard
/// validation applies only to the config actually used. A branch-local file that fails to
/// parse (or trips the `[local_ai]` rejection) in a linked worktree folds into the divergence
/// warning — it must never make every command from the linked checkout fatal, because its
/// contents are irrelevant by design when main governs.
#[test]
fn config_load_ignores_an_invalid_branch_config_when_main_governs() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("brokecfg");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"main.sqlite\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

    // Unparseable garbage on the branch: main still governs.
    std::fs::write(linked.join("rag-rat.toml"), "this is [not toml").unwrap();
    let cfg = Config::load(linked.join("rag-rat.toml"))
        .expect("a broken branch config is ignored when main governs");
    let main_c = crate::paths::canonicalize(&main).unwrap();
    assert_eq!(cfg.database, main_c.join("main.sqlite"));

    // The deprecated `[local_ai]` table on the branch: same posture (it is a VALIDATION
    // failure, not a parse failure — both fold into the warning).
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[local_ai]\nmodel = \"x\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    let cfg = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(cfg.database, main_c.join("main.sqlite"));

    // In the checkout that GOVERNS (main), the same brokenness stays fatal.
    std::fs::write(main.join("rag-rat.toml"), "this is [not toml").unwrap();
    assert!(
        Config::load(main.join("rag-rat.toml")).is_err(),
        "the governing config's validation is fatal as always",
    );
}

/// The seam's trigger is TOPOLOGY, not the root-anchoring proxy (Codex batch 8, finding 3): a
/// branch-only `[index] root` makes `anchor_root_to_main_worktree` keep the local root (the
/// dir doesn't exist in main), which under the old `anchored != local` trigger concluded
/// "not linked" and let the branch config govern database/watch/models — the exact
/// split-brain the seam prevents. Governance must be unconditional on linked-ness.
#[test]
fn config_load_governs_from_main_even_when_a_branch_only_root_defeats_anchoring() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("branchroot");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"main.sqlite\"\n[watch]\ndebounce_ms = \
         1111\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

    // The branch config points `[index] root` at a dir that exists ONLY on the branch —
    // anchoring keeps the local root (missing in main), defeating the old equality proxy.
    std::fs::create_dir_all(linked.join("branch_only/src")).unwrap();
    std::fs::write(linked.join("branch_only/src/lib.rs"), "pub fn b() {}\n").unwrap();
    assert!(!main.join("branch_only").exists(), "main never had this dir");
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \"branch_only\"\ndatabase = \"branch.sqlite\"\n[watch]\ndebounce_ms = \
         9999\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    let cfg = Config::load(linked.join("rag-rat.toml")).unwrap();
    let main_c = crate::paths::canonicalize(&main).unwrap();
    assert_eq!(
        cfg.database,
        main_c.join("main.sqlite"),
        "main's database governs — the branch-only root cannot defeat the seam",
    );
    assert_eq!(cfg.watch.debounce_ms, 1111, "main's watch config governs too");
    assert_eq!(cfg.root, main_c, "root comes from MAIN's config when main governs");
}

/// The identity gate's SECOND entrance (Codex batch 8, finding 5a): an EXPLICIT pin at the
/// consolidated global store from an identity-less root is refused at resolution — the
/// keyless gate never sees a pinned config, and letting it open the shared store would land
/// this project on adoption's sole-repo pick (a SIBLING repo). A `repo_id` pin restores the
/// identity and lifts the refusal. Compares against `global_database_path()` in the CURRENT
/// environment (no env mutation ⇒ parallel-safe); `load` only resolves, never writes there.
#[test]
fn config_load_refuses_an_identity_less_pin_at_the_global_store() {
    let _env = crate::data_dir::env_guard();
    let Some(global) = crate::data_dir::global_database_path() else {
        return; // no resolvable data dir on this platform — the gate cannot trigger
    };
    let tmp = scratch("globpin");
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    let config_path = tmp.join("rag-rat.toml");
    std::fs::write(
        &config_path,
        format!(
            "[index]\nroot = \".\"\ndatabase = \"{}\"\n[target_bindings]\nrust = [\"src\"]\n",
            // Forward-slash (path-slash): a Windows `C:\…` path has invalid TOML escapes
            // (`\U`, …); `/` is TOML-safe and `Path` treats the separators as equivalent
            // there.
            global.to_slash_lossy()
        ),
    )
    .unwrap();
    let err = Config::load(&config_path).expect_err("identity-less global pin is refused");
    assert!(
        matches!(err, ConfigError::GlobalPinWithoutIdentity),
        "the refusal names the remedy: {err}",
    );

    // A `repo_id` pin IS a resolvable identity — the same config with one loads fine.
    std::fs::write(
        &config_path,
        format!(
            "[index]\nroot = \".\"\nrepo_id = \"pinned-project\"\ndatabase = \
             \"{}\"\n[target_bindings]\nrust = [\"src\"]\n",
            // Forward-slash (path-slash): a Windows `C:\…` path has invalid TOML escapes
            // (`\U`, …); `/` is TOML-safe and `Path` treats the separators as equivalent
            // there.
            global.to_slash_lossy()
        ),
    )
    .unwrap();
    let cfg = Config::load(&config_path).expect("a repo_id pin lifts the refusal");
    assert_eq!(cfg.database, global);
}

/// The `database` decision is MAIN-WORKTREE-ANCHORED (Codex batch 7): a linked worktree's
/// branch-local config can neither UN-PIN (a branch toml omitting the key while main pins —
/// pre-fix that split the repo across the global store and main's per-repo file) nor RE-PIN
/// (a branch adding its own key) the repo's database. Main's config is authoritative, exactly
/// as it is for `repo_id`.
#[test]
fn config_load_anchors_the_database_key_to_the_main_worktree() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("dbanchor");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    // MAIN pins an explicit per-repo database.
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"custom/pinned.sqlite\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);

    // The BRANCH config omits the key (a branch predating the pin): pre-fix the keyless
    // default resolved the linked checkout to the GLOBAL store — a different DB than main's.
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(
        from_linked.database, from_main.database,
        "a branch omitting the key must not divert the linked worktree off main's pin",
    );
    assert!(from_linked.database_key_pinned, "the GOVERNING (main) key decision travels too");

    // The BRANCH config pinning its OWN key: main (keyless here) stays authoritative — a
    // branch cannot fork the repo onto a private database.
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \"branch/fork.sqlite\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(
        from_linked.database, from_main.database,
        "a branch-local pin must not fork the repo onto its own database",
    );
    assert!(!from_linked.database_key_pinned, "main keyless ⇒ governing decision is keyless");
}

/// A7 legacy interplay: a keyless config in a repo that ALREADY has a `.rag-rat/index.sqlite`
/// (indexed before the flip, or a fresh `rag-rat init` over an old checkout) keeps resolving to
/// that legacy file — never silently abandoning its memories — until `rag-rat consolidate`
/// imports and renames it, after which resolution falls through to the global store.
#[test]
fn config_load_without_a_database_key_prefers_an_existing_legacy_index() {
    let _env = crate::data_dir::env_guard();
    let tmp = scratch("legacydb");
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::create_dir_all(tmp.join(".rag-rat")).unwrap();
    std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(tmp.join(".rag-rat/index.sqlite"), b"legacy").unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git_commit_all(&tmp);

    let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
    assert_eq!(
        config.database,
        crate::paths::canonicalize(&tmp).unwrap().join(".rag-rat/index.sqlite"),
        "a pre-existing legacy index wins over the global default until consolidated",
    );

    // Once consolidated (the legacy file renamed away), the same config resolves globally.
    std::fs::rename(tmp.join(".rag-rat/index.sqlite"), tmp.join(".rag-rat/index.sqlite.imported"))
        .unwrap();
    let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
    assert_eq!(
        config.database,
        crate::data_dir::global_database_path().expect("data dir resolves"),
        "after consolidation the keyless config falls through to the global store",
    );

    // The `.imported` marker is a STAY-GLOBAL LATCH: a stray legacy file REAPPEARING beside it
    // (an old binary, a restored backup) must not silently divert the repo off the global
    // store its memories were imported into.
    std::fs::write(tmp.join(".rag-rat/index.sqlite"), b"stray").unwrap();
    let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
    assert_eq!(
        config.database,
        crate::data_dir::global_database_path().expect("data dir resolves"),
        "a stray legacy file beside the .imported marker is ignored, not adopted",
    );
}

/// The IDENTITY GATE on the global default: a keyless config at a root with NO derivable repo
/// identity (non-git, or a `git init` with an unborn HEAD) stays on its PER-ROOT legacy path —
/// in the shared global store every identity-less root would pool under the one
/// `__unassigned__` placeholder scope, so two fresh non-git projects would see and overwrite
/// each other's rows, and an unborn repo would strand its placeholder rows once its first
/// commit mints a real id. Two identity-less roots therefore NEVER share a database.
#[test]
fn config_load_without_a_database_key_stays_per_root_for_identity_less_roots() {
    let _env = crate::data_dir::env_guard();
    let keyless_config = |tag: &str| {
        let tmp = scratch(&format!("noident-{tag}"));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(
            tmp.join("rag-rat.toml"),
            "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        tmp
    };

    // Two NON-GIT roots: each resolves to its OWN per-root legacy path — never the shared
    // global store, and never each other's.
    let a = keyless_config("a");
    let b = keyless_config("b");
    let config_a = Config::load(a.join("rag-rat.toml")).unwrap();
    let config_b = Config::load(b.join("rag-rat.toml")).unwrap();
    assert_eq!(
        config_a.database,
        crate::paths::canonicalize(&a).unwrap().join(".rag-rat/index.sqlite"),
        "an identity-less root stays on its per-root legacy path",
    );
    assert_eq!(
        config_b.database,
        crate::paths::canonicalize(&b).unwrap().join(".rag-rat/index.sqlite"),
        "each identity-less root gets its own database",
    );
    assert_ne!(config_a.database, config_b.database, "identity-less roots never share scope");

    // An UNBORN repo (`git init`, no commit yet) is identity-less too: it lands per-root, so
    // its placeholder rows adopt IN THAT DB when the first commit mints a real id (the
    // existing single-repo adoption flow), instead of stranding in the global store.
    let unborn = keyless_config("unborn");
    let git = |args: &[&str]| {
        crate::test_git::run(&unborn, args);
    };
    git(&["init", "-q"]);
    let config = Config::load(unborn.join("rag-rat.toml")).unwrap();
    assert_eq!(
        config.database,
        crate::paths::canonicalize(&unborn).unwrap().join(".rag-rat/index.sqlite"),
        "an unborn repo stays per-root until its first commit mints an identity",
    );
    // A `[index] repo_id` pin IS an identity: the same root then resolves globally.
    std::fs::write(
        unborn.join("rag-rat.toml"),
        "[index]\nroot = \".\"\nrepo_id = \"pinned-project\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    let config = Config::load(unborn.join("rag-rat.toml")).unwrap();
    assert_eq!(
        config.database,
        crate::data_dir::global_database_path().expect("data dir resolves"),
        "a pinned repo_id makes the root identity-bearing, so the global default applies",
    );
}

#[test]
fn repo_id_override_absent_is_none() {
    let tmp = scratch("repoid-none");
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();

    let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
    assert_eq!(config.repo_id_override, None, "no [index] repo_id → None");
}

#[test]
fn config_load_in_a_linked_worktree_uses_main_base_targets_not_the_branch() {
    // #219 review: a linked branch can point `rag-rat.toml` at a target dir that exists ONLY in
    // that branch. `Config::load` anchors `root` to the main worktree for the shared base
    // index. Two things must hold: (1) loading the branch config must NOT fail with
    // `MissingDirectory` (the branch-only dir is validated against the linked checkout where it
    // lives); (2) the stored BASE `targets` must come from MAIN's `rag-rat.toml`, not the
    // branch's — otherwise base discovery walks main with the branch target set and tombstones
    // any main file outside it. The branch's extra target is served via the overlay, not the
    // base config.
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("cfgbranch");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    // Main's config indexes only `src`.
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    // A branch adds a NEW target dir `extra` and a config that indexes it — committed only on
    // the branch, checked out in the linked worktree.
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    std::fs::create_dir_all(linked.join("extra")).unwrap();
    std::fs::write(linked.join("extra/more.rs"), "pub fn b() {}\n").unwrap();
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\", \"extra\"]\n",
    )
    .unwrap();
    git(&linked, &["add", "-A"]);
    git(&linked, &["commit", "-qm", "branch adds extra"]);

    // `extra` does not exist in the main checkout, so validating against main would fail —
    // loading still succeeds because the branch-only dir is validated against the linked
    // checkout where it lives.
    assert!(!main.join("extra").exists(), "the branch-only dir must be absent from main");
    let from_linked = Config::load(linked.join("rag-rat.toml"))
        .expect("loading the branch config in the linked worktree must not fail (req 1)");
    // root still anchors to main (one shared base index).
    assert_eq!(
        from_linked.root,
        crate::paths::canonicalize(&main).unwrap(),
        "root anchors to the main worktree for the shared base index",
    );
    // The stored BASE targets come from MAIN's config (`src` only), NOT the branch's
    // (`src` + `extra`): base discovery must not walk main with the branch's target set (req
    // 2).
    let dirs = from_linked.target_directories();
    assert!(dirs.contains(&PathBuf::from("src")), "main's `src` target is the base: {dirs:?}");
    assert!(
        !dirs.contains(&PathBuf::from("extra")),
        "the branch-only target must NOT be a base target (it can't tombstone main): {dirs:?}",
    );
}

#[test]
fn config_load_in_a_linked_worktree_keeps_main_targets_when_the_branch_narrows_them() {
    // #219 review (3440746682): a linked branch's `rag-rat.toml` that NARROWS the target set
    // (drops a dir that still exists on main) must NOT carry that narrowed set into the BASE
    // config. The base config drives discovery over the anchored (main) root; with the branch's
    // narrowed targets, main-only files would be classified `deleted` and tombstoned in the
    // base scope — hiding committed files from main queries. The stored base targets
    // must be MAIN's.
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("cfgnarrow");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::create_dir_all(main.join("extra")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(main.join("extra/more.rs"), "pub fn b() {}\n").unwrap();
    // Main indexes BOTH `src` and `extra`.
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\", \"extra\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    // The branch NARROWS to `src` only (drops `extra`), committed on the branch.
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&linked, &["add", "-A"]);
    git(&linked, &["commit", "-qm", "branch narrows to src"]);

    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    let dirs = from_linked.target_directories();
    // Both of main's targets survive in the base config, so base discovery still walks `extra`
    // on main and never tombstones `extra/more.rs`.
    assert!(dirs.contains(&PathBuf::from("src")), "base keeps main's `src`: {dirs:?}");
    assert!(
        dirs.contains(&PathBuf::from("extra")),
        "base keeps main's `extra` even though the branch dropped it: {dirs:?}",
    );
}

#[test]
fn config_load_anchors_repo_id_override_to_main_when_the_branch_diverges() {
    // FINDING 4: repo IDENTITY is per-repo, so the `[index] repo_id` override is read from the
    // MAIN worktree's config, NOT the launching (branch-local) one. A linked worktree that pins
    // a DIFFERENT id must still resolve MAIN's — otherwise identity splits by which checkout
    // launched. This mirrors the root/database/targets anchoring above.
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("repoid-anchor");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    // Main pins a canonical repo_id.
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\nrepo_id = \"canonical-id\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    // The branch pins a DIVERGENT id, committed on the branch and checked out in the worktree.
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\nrepo_id = \"branch-divergent-id\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    git(&linked, &["add", "-A"]);
    git(&linked, &["commit", "-qm", "branch pins a different repo_id"]);

    let from_main = Config::load(main.join("rag-rat.toml")).unwrap();
    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(
        from_main.repo_id_override.as_deref(),
        Some("canonical-id"),
        "the main checkout resolves its own override",
    );
    assert_eq!(
        from_linked.repo_id_override.as_deref(),
        Some("canonical-id"),
        "a linked worktree resolves MAIN's repo_id override, not its own branch-local pin",
    );
}

#[test]
fn config_load_anchors_repo_id_override_to_main_when_main_omits_it() {
    // The strong form of FINDING 4: MAIN omits `[index] repo_id` (identity derives from the
    // root commit), but the branch pins one. The anchored value is MAIN's absence →
    // None, so identity stays derived and launch-point-independent; the branch pin is
    // NOT honored for the shared identity (honoring it would make identity depend on
    // which worktree launched).
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("repoid-mainomit");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    // Main OMITS repo_id.
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\nrepo_id = \"branch-only-id\"\n[target_bindings]\nrust = \
         [\"src\"]\n",
    )
    .unwrap();
    git(&linked, &["add", "-A"]);
    git(&linked, &["commit", "-qm", "branch pins a repo_id main lacks"]);

    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(
        from_linked.repo_id_override, None,
        "main omits the override, so the anchored identity derives — the branch pin is ignored",
    );
}

/// #427: a linked worktree's `[index] root` resolves to itself locally, but `Config::load`
/// re-anchors it to MAIN so every worktree of a repo shares one base index. The PRE-anchor
/// value (the worktree the operator actually named) would otherwise be lost after anchoring —
/// capture it so the `index` command can warn instead of silently indexing a different
/// checkout than the one named.
#[test]
fn load_records_the_pre_anchor_root_for_a_linked_worktree() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("reanchor");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@e"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
    std::fs::write(linked.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();

    let main_c = crate::paths::canonicalize(&main).unwrap();
    let linked_c = crate::paths::canonicalize(&linked).unwrap();
    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(from_linked.root, main_c, "root anchors to main (existing behavior)");
    assert_eq!(
        from_linked.source_root_reanchored_from.as_deref(),
        Some(linked_c.as_path()),
        "the pre-anchor (named) linked-worktree root is captured",
    );
}

/// The counterpart to the above: loading from a plain (non-worktree) repo redirects nothing,
/// so the field stays `None`.
#[test]
fn load_leaves_reanchor_none_for_the_main_worktree() {
    let tmp = scratch("reanchor-none");
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git_commit_all(&tmp);

    let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
    assert!(
        config.source_root_reanchored_from.is_none(),
        "no worktree redirection happened, so the field stays None",
    );
}

#[test]
fn anchor_root_preserves_subdir_and_redirects_linked_to_main() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("cfg");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("seed.txt"), "x").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);
    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "--detach", "-q", linked.to_str().unwrap()]);
    std::fs::create_dir_all(linked.join("src")).unwrap();

    let main_c = crate::paths::canonicalize(&main).unwrap();
    let linked_c = crate::paths::canonicalize(&linked).unwrap();

    // Main worktree (any root) resolves to itself.
    assert_eq!(config::anchor_root_to_main_worktree(&main_c), main_c);
    // A SUBDIR root on the main worktree is PRESERVED (not collapsed to the repo top) — the
    // #219-review regression: collapsing changed the indexed file set + failed config load.
    assert_eq!(config::anchor_root_to_main_worktree(&main_c.join("src")), main_c.join("src"));
    // Linked worktree, root=".", redirects to the main worktree → one shared base.
    assert_eq!(config::anchor_root_to_main_worktree(&linked_c), main_c);
    // Linked worktree SUBDIR root rebases under the main worktree, subdir preserved.
    assert_eq!(config::anchor_root_to_main_worktree(&linked_c.join("src")), main_c.join("src"));

    // A non-git directory falls back to itself.
    let plain = tmp.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let plain_c = crate::paths::canonicalize(&plain).unwrap();
    assert_eq!(config::anchor_root_to_main_worktree(&plain_c), plain_c);

    // A linked-worktree subdir root that does NOT exist in main must NOT anchor to a missing
    // `main/<rel>` path (#219 review): the branch created `branch_only/`, which main never had.
    // The anchored `main/branch_only` doesn't exist, so resolution keeps the linked checkout's
    // (existing) root — otherwise `Config.root` would point outside any discoverable repo path.
    let branch_only = linked.join("branch_only");
    std::fs::create_dir_all(&branch_only).unwrap();
    let branch_only_c = crate::paths::canonicalize(&branch_only).unwrap();
    assert!(!main_c.join("branch_only").exists(), "main never had this dir");
    assert_eq!(
        config::anchor_root_to_main_worktree(&branch_only_c),
        branch_only_c,
        "a branch-only root that's missing in main keeps the linked checkout's root",
    );
}

#[test]
fn for_linked_worktree_overlay_falls_back_when_branch_config_is_missing_or_invalid() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("overlay-fallback");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    let base = Config::load(main.join("rag-rat.toml")).unwrap();

    let missing = base.for_linked_worktree_overlay(&linked);
    assert_eq!(missing.targets, base.targets, "missing branch config keeps base targets");

    std::fs::write(linked.join("rag-rat.toml"), "not valid toml [[[\n").unwrap();
    let invalid = base.for_linked_worktree_overlay(&linked);
    assert_eq!(invalid.targets, base.targets, "invalid branch config keeps base targets");

    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\", \"extra\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(linked.join("extra")).unwrap();
    std::fs::write(linked.join("extra/more.rs"), "pub fn b() {}\n").unwrap();
    let branch = base.for_linked_worktree_overlay(&linked);
    let dirs = branch.target_directories();
    assert!(dirs.contains(&PathBuf::from("extra")), "valid branch config swaps targets: {dirs:?}");
}

#[test]
fn config_load_propagates_main_parse_error_from_linked_worktree() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("main-broken");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(main.join("rag-rat.toml"), "[index]\nroot = \".\"\n[local_ai]\n").unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();

    let err = Config::load(linked.join("rag-rat.toml")).unwrap_err();
    assert!(
        matches!(err, ConfigError::LocalAiTableRenamed),
        "linked checkout must inherit main's fatal parse error, got {err:?}",
    );
}

#[test]
fn config_load_rejects_reserved_papertrail_table_from_governing_main() {
    let git = |dir: &Path, args: &[&str]| {
        crate::test_git::run(dir, args);
    };
    let tmp = scratch("main-papertrail");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    std::fs::write(main.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        main.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[papertrail]\nprobe_interval_secs = 60\n",
    )
    .unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.com"]);
    git(&main, &["config", "user.name", "t"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "seed"]);

    let linked = tmp.join("wt");
    git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    std::fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();

    Config::load(linked.join("rag-rat.toml")).unwrap();
}

/// Whether `path` carries the Windows extended-length (`\\?\`) prefix — asked TEXTUALLY, because
/// that is how `git` and gix see it.
fn is_verbatim(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with(r"\\?\")
}

/// `Config::load` normalizes its root, and the rest of the system consumes that root as a `git`
/// argument and compares it against gix's `workdir()`. Both consumers reject the `\\?\C:\…`
/// verbatim spelling `std::fs::canonicalize` returns on Windows, so the normalized root must not
/// carry it (#1048).
///
/// Not `cfg`-gated: on Unix the assertions hold trivially, and the Windows leg is the one that has
/// to run them — a Unix-only probe would have caught none of this.
#[test]
fn a_loaded_config_root_is_a_spelling_git_and_gix_both_accept() {
    let tmp = scratch("root-spelling");
    let main = tmp.join("main");
    std::fs::create_dir_all(main.join("crate/src")).unwrap();
    crate::test_git::run(&main, &["init", "-q"]);
    std::fs::write(main.join("crate/src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    crate::test_git::run(&main, &["add", "."]);
    crate::test_git::run(&main, &["commit", "-qm", "seed"]);
    std::fs::write(main.join("rag-rat.toml"), "[index]\nroot = \"crate\"\n").unwrap();

    let cfg = Config::load(main.join("rag-rat.toml")).unwrap();
    assert!(
        !is_verbatim(&cfg.root),
        "Config::load must not hand out a verbatim root: {:?}",
        cfg.root,
    );

    // gix: the root strips against the workdir of the repository discovered from it, which is what
    // the worktree overlay derives its config subdir from.
    let repo = crate::repo_discover::discover_repo(&cfg.root).unwrap();
    let workdir = crate::paths::canonicalize_or_simplified(repo.workdir().unwrap());
    assert_eq!(
        cfg.root.strip_prefix(&workdir).ok(),
        Some(Path::new("crate")),
        "the loaded root {:?} must strip against the repository workdir {:?}",
        cfg.root,
        workdir,
    );

    // git: a destination derived from the loaded root is usable as a `worktree add` argument. On
    // the unfixed Windows path this fails with `could not create leading directories of
    // '//?/C:/…'`.
    let linked = cfg.root.parent().expect("the root has a repository above it").join("linked-wt");
    assert!(!is_verbatim(&linked), "the derived destination is not verbatim: {linked:?}");
    crate::test_git::run(&main, &[
        "worktree",
        "add",
        "--detach",
        "-q",
        linked.to_str().expect("a scratch path is UTF-8"),
    ]);
    assert!(linked.join("crate/src/a.rs").is_file(), "the linked checkout was created");
}
