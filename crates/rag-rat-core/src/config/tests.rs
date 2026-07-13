use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use path_slash::PathExt;

use super::{
    self as config, Config, ConfigError, EmbeddingRuntimeConfig, LlmConfig, LogConfig, LogFormat,
    LogLevel, MemoryConfig, MemorySurface, OracleConfig, RawConfig, RawMemory, RawOracle,
    RawSearch, RawTarget, RawVersionCheck, RawWatch, RemoteBackend, RemoteDreamConfig,
    RemoteEmbeddingConfig, ResolvedTarget, SearchConfig, TargetKind, TrackerAuth,
    VersionCheckConfig, WatchConfig,
};
use crate::language::Language;

// Test-only: forward-slashes a real filesystem path so it can be embedded in a
// double-quoted TOML string value without Windows `\U`/`\R` invalid-escape parse
// errors.

static CFG_TEMP: AtomicU64 = AtomicU64::new(0);

#[test]
fn config_load_resolves_main_and_linked_worktrees_to_one_database() {
    // The actual guarantee (review item 1): Config::load from the main worktree and from a
    // linked worktree of the same repo produce the *same* database path — not two DBs.
    let git = |dir: &Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-cfgload-{}-{id}", std::process::id()));
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
    assert_eq!(from_main.database, main.canonicalize().unwrap().join(".rag-rat/index.sqlite"));
    // AND the `root` anchors to the main worktree from either launch point — so every process
    // uses the same base commit for the shared index, instead of a worktree-launched one
    // rooting at the worktree (a different base → conflicting overlay writes /
    // readable-vs-tombstone races) (#218/#219).
    assert_eq!(from_main.root, from_linked.root, "main and linked configs resolve to one root");
    assert_eq!(
        from_linked.root,
        main.canonicalize().unwrap(),
        "a linked worktree's config root anchors to the main worktree",
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn tracker_and_papertrail_config_parse_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("rag-rat.toml"),
        "[[tracker]]\nprovider = \"github\"\nproject = \"owner/repo\"\n",
    )
    .unwrap();
    let cfg = Config::load(dir.path().join("rag-rat.toml")).unwrap();
    assert_eq!(cfg.trackers.len(), 1);
    assert_eq!(cfg.trackers[0].provider, super::Tracker::Github);
    assert_eq!(cfg.trackers[0].project.as_deref(), Some("owner/repo"));
    assert_eq!(cfg.trackers[0].remote, "origin");
}

#[test]
fn jira_tracker_requires_an_explicit_project() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("rag-rat.toml"), "[[tracker]]\nprovider = \"jira\"\n").unwrap();
    assert!(matches!(
        Config::load(dir.path().join("rag-rat.toml")),
        Err(ConfigError::JiraTrackerRequiresProject)
    ));
}

#[test]
fn tracker_auth_requires_exactly_one_source() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("rag-rat.toml"),
        "[[tracker]]\nprovider = \"gitlab\"\nauth = { env = \"TOKEN\", token_command = \"glab \
         auth token\" }\n",
    )
    .unwrap();
    assert!(matches!(
        Config::load(dir.path().join("rag-rat.toml")),
        Err(ConfigError::TrackerAuthExactlyOne)
    ));
}

#[test]
fn tracker_auth_accepts_each_supported_source() {
    for (auth, expected) in [
        ("env = \"TOKEN\"", TrackerAuth::Env("TOKEN".to_string())),
        (
            "token_command = \"gh auth token\"",
            TrackerAuth::TokenCommand("gh auth token".to_string()),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rag-rat.toml"),
            format!(
                "[[tracker]]\nprovider = \"github\"\nproject = \"org/repo\"\nauth = {{ {auth} }}\n"
            ),
        )
        .unwrap();
        let config = Config::load(dir.path().join("rag-rat.toml")).unwrap();
        assert_eq!(config.trackers[0].auth.as_ref(), Some(&expected));
    }
}

#[test]
fn tracker_provider_is_required_and_closed() {
    for (body, expected) in [
        ("[[tracker]]\nproject = \"org/repo\"\n", "requires a `provider`"),
        ("[[tracker]]\nprovider = \"forgejo\"\n", "must be one of"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rag-rat.toml"), body).unwrap();
        let error = Config::load(dir.path().join("rag-rat.toml")).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn tracker_projects_are_validated_by_provider() {
    for (provider, project) in [
        ("github", "org/team/repo"),
        ("bitbucket", "workspace/repo/extra"),
        ("gitlab", "repo"),
        ("github", "org/repo?query=1"),
        ("github", "org/%2e%2e"),
        ("gitlab", "group/../repo"),
        ("jira", "Proj"),
        ("jira", "A"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rag-rat.toml"),
            format!("[[tracker]]\nprovider = \"{provider}\"\nproject = \"{project}\"\n"),
        )
        .unwrap();
        assert!(matches!(
            Config::load(dir.path().join("rag-rat.toml")),
            Err(ConfigError::InvalidTrackerProject { .. })
        ));
    }

    for (provider, project) in
        [("github", "org/repo"), ("bitbucket", "workspace/repo"), ("gitlab", "group/sub/repo")]
    {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rag-rat.toml"),
            format!("[[tracker]]\nprovider = \"{provider}\"\nproject = \"{project}\"\n"),
        )
        .unwrap();
        Config::load(dir.path().join("rag-rat.toml")).unwrap();
    }
}

#[test]
fn tracker_base_url_requires_a_nonempty_authority() {
    for base_url in [
        "https://",
        "https:///gitlab",
        "ftp://gitlab.example.com",
        "gitlab.example.com",
        "https://:8443",
        "https://gitlab example.com",
        "https://gitlab.example.com/path",
        "https://gitlab.example.com?query=1",
        "https://gitlab.example.com#fragment",
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rag-rat.toml"),
            format!("[[tracker]]\nprovider = \"gitlab\"\nbase_url = \"{base_url}\"\n"),
        )
        .unwrap();
        assert!(matches!(
            Config::load(dir.path().join("rag-rat.toml")),
            Err(ConfigError::TrackerBaseUrlNotHttp(_))
        ));
    }
}

#[test]
fn tracker_base_url_rejects_credentials_and_normalizes_trailing_slashes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("rag-rat.toml"),
        "[[tracker]]\nprovider = \"gitlab\"\nbase_url = \"https://user:token@gitlab.example.com\"\n",
    )
    .unwrap();
    assert!(matches!(
        Config::load(dir.path().join("rag-rat.toml")),
        Err(ConfigError::TrackerBaseUrlHasCredentials)
    ));

    std::fs::write(
        dir.path().join("rag-rat.toml"),
        "[[tracker]]\nprovider = \"gitlab\"\nbase_url = \"http://gitlab.example.com:8080/\"\n",
    )
    .unwrap();
    let config = Config::load(dir.path().join("rag-rat.toml")).unwrap();
    assert_eq!(config.trackers[0].base_url.as_deref(), Some("http://gitlab.example.com:8080"));
}

#[test]
fn papertrail_scheduling_table_is_rejected_until_it_is_consumed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("rag-rat.toml"), "[papertrail]\nprobe_interval_secs = 60\n")
        .unwrap();
    assert!(matches!(
        Config::load(dir.path().join("rag-rat.toml")),
        Err(ConfigError::PapertrailSchedulingNotSupported)
    ));
}

#[test]
fn papertrail_rate_limit_reserve_is_parsed_and_validated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rag-rat.toml");
    std::fs::write(&path, "[papertrail]\nrate_limit_reserve = 0.2\n").unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.papertrail.rate_limit_reserve, 0.2);

    std::fs::write(&path, "[papertrail]\nrate_limit_reserve = 0.0\n").unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.papertrail.rate_limit_reserve, 0.0, "zero disables the reserved slice");

    for invalid in ["-0.1", "1.0", "nan"] {
        std::fs::write(&path, format!("[papertrail]\nrate_limit_reserve = {invalid}\n")).unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::PapertrailRateLimitReserveOutOfRange(_))
        ));
    }
}

#[test]
fn repo_id_override_is_parsed_and_does_not_change_the_database_path() {
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-repoid-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Seed a minimal COMMITTED git repo at `dir` — the identity-bearing fixture the global
/// default requires (a keyless config resolves globally only for a root with a derivable repo
/// identity).
fn git_commit_all(dir: &Path) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
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
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-globaldb-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The GOVERNING SEAM: in a linked worktree the MAIN config governs the WHOLE config, not a
/// per-key subset — a divergent branch-local file cannot fork the embedding model (or any
/// other key) even though no per-key anchoring was ever written for `[llm]`. The two loads
/// must produce the SAME resolved `Config`.
#[test]
fn config_load_in_a_linked_worktree_is_governed_wholesale_by_the_main_config() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-wholecfg-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Config-less-main fallback posture: main is resolvable but has NO `rag-rat.toml`, so the
/// linked worktree's local config governs best-effort (with a warning) — root still anchors
/// to main so the shared index keys off one base checkout.
#[test]
fn config_load_falls_back_to_the_local_config_when_main_has_none() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-nomaincfg-{}-{id}", std::process::id()));
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
    let canonical_main = main.canonicalize().unwrap();
    assert_eq!(cfg.root, canonical_main, "root anchors to main even on the fallback");
    assert_eq!(
        cfg.database,
        canonical_main.join("branch.sqlite"),
        "the local key governs (resolved against the main top) until main gains a config",
    );
    assert!(cfg.database_key_pinned);

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The DISCOVERY resolver matrix (Codex batch 9): local file wins wherever it exists (the
/// seam then governs + warns), a linked checkout without one resolves to MAIN's path (even
/// when that file doesn't exist yet — hints must name where the config belongs), and
/// main/non-git checkouts stay local.
#[test]
fn discover_config_path_resolves_the_governing_checkout() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-discover-{}-{id}", std::process::id()));
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
    let main_c = main.canonicalize().unwrap();

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

    let _ = std::fs::remove_dir_all(&tmp);
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
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-walkup-{}-{id}", std::process::id()));
    let repo = tmp.join("repo");
    let nested = repo.join("crates").join("cli").join("src");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(repo.join("rag-rat.toml"), "[index]\nroot = \".\"\n").unwrap();

    // The walk returns a canonical absolute path (a found file), so compare canonically — temp
    // roots can be symlinked (macOS `/tmp` → `/private/tmp`).
    let want = repo.join("rag-rat.toml").canonicalize().unwrap();
    assert_eq!(
        config::discover_config_path(&nested).canonicalize().unwrap(),
        want,
        "subdir → repo cfg"
    );
    assert_eq!(
        config::discover_config_path(&repo).canonicalize().unwrap(),
        want,
        "repo root → local"
    );

    // A config-less tree with NO ancestor config: the local (non-existent) path, unchanged —
    // the not-found fallback returns the original `dir/rag-rat.toml` for the hint, uncanonical.
    let bare = tmp.join("bare").join("deep");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(config::discover_config_path(&bare), bare.join("rag-rat.toml"));

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The ancestor walk STOPS at the enclosing git repo root: a nested checkout / submodule with
/// no `rag-rat.toml` of its own must NOT bind to an indexed PARENT repo's config — that
/// would point searches and (worse) memory writes at the wrong repository (#611 review,
/// P2).
#[test]
fn discover_config_path_does_not_cross_a_nested_repo_boundary() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-nested-{}-{id}", std::process::id()));
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
        got.canonicalize().ok(),
        parent.join("rag-rat.toml").canonicalize().ok(),
        "the parent repo's rag-rat.toml must not leak across the nested boundary",
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A SUBDIRECTORY launch inside a LINKED worktree finds a BRANCH-LOCAL `rag-rat.toml` at the
/// worktree root (routing the load through the governing seam + divergence warning) instead of
/// jumping straight to main; with no branch-local config anywhere in the worktree it still
/// resolves to MAIN's path (the governing-seam invariant). #611 review, P2 (linked arm).
#[test]
fn discover_config_path_finds_a_branch_local_config_from_a_linked_worktree_subdir() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-wtsub-{}-{id}", std::process::id()));
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
    let main_c = main.canonicalize().unwrap();
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
        config::discover_config_path(&sub).canonicalize().unwrap(),
        linked.join("rag-rat.toml").canonicalize().unwrap(),
        "a subdir launch must find the branch-local config, not jump to main",
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The linked-ness PRIMITIVE (Codex batch 8, findings 1+3): topology-derived — the discovered
/// checkout's workdir vs the designated main — so a SUBDIRECTORY of the main worktree is NOT
/// linked (pre-fix, `init` from `main/src` falsely refused), while any path inside a linked
/// checkout (its top OR a subdir) is.
#[test]
fn linked_worktree_main_root_derives_linkedness_from_topology() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-linkpred-{}-{id}", std::process::id()));
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
    let main_c = main.canonicalize().unwrap();

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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Validation ORDERING (Codex batch 8, finding 2): the governing config is chosen FIRST; hard
/// validation applies only to the config actually used. A branch-local file that fails to
/// parse (or trips the `[local_ai]` rejection) in a linked worktree folds into the divergence
/// warning — it must never make every command from the linked checkout fatal, because its
/// contents are irrelevant by design when main governs.
#[test]
fn config_load_ignores_an_invalid_branch_config_when_main_governs() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-brokecfg-{}-{id}", std::process::id()));
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
    let main_c = main.canonicalize().unwrap();
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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The seam's trigger is TOPOLOGY, not the root-anchoring proxy (Codex batch 8, finding 3): a
/// branch-only `[index] root` makes `anchor_root_to_main_worktree` keep the local root (the
/// dir doesn't exist in main), which under the old `anchored != local` trigger concluded
/// "not linked" and let the branch config govern database/watch/models — the exact
/// split-brain the seam prevents. Governance must be unconditional on linked-ness.
#[test]
fn config_load_governs_from_main_even_when_a_branch_only_root_defeats_anchoring() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-branchroot-{}-{id}", std::process::id()));
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
    let main_c = main.canonicalize().unwrap();
    assert_eq!(
        cfg.database,
        main_c.join("main.sqlite"),
        "main's database governs — the branch-only root cannot defeat the seam",
    );
    assert_eq!(cfg.watch.debounce_ms, 1111, "main's watch config governs too");
    assert_eq!(cfg.root, main_c, "root comes from MAIN's config when main governs");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The identity gate's SECOND entrance (Codex batch 8, finding 5a): an EXPLICIT pin at the
/// consolidated global store from an identity-less root is refused at resolution — the
/// keyless gate never sees a pinned config, and letting it open the shared store would land
/// this project on adoption's sole-repo pick (a SIBLING repo). A `repo_id` pin restores the
/// identity and lifts the refusal. Compares against `global_database_path()` in the CURRENT
/// environment (no env mutation ⇒ parallel-safe); `load` only resolves, never writes there.
#[test]
fn config_load_refuses_an_identity_less_pin_at_the_global_store() {
    let Some(global) = crate::data_dir::global_database_path() else {
        return; // no resolvable data dir on this platform — the gate cannot trigger
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-globpin-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The `database` decision is MAIN-WORKTREE-ANCHORED (Codex batch 7): a linked worktree's
/// branch-local config can neither UN-PIN (a branch toml omitting the key while main pins —
/// pre-fix that split the repo across the global store and main's per-repo file) nor RE-PIN
/// (a branch adding its own key) the repo's database. Main's config is authoritative, exactly
/// as it is for `repo_id`.
#[test]
fn config_load_anchors_the_database_key_to_the_main_worktree() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-dbanchor-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// A7 legacy interplay: a keyless config in a repo that ALREADY has a `.rag-rat/index.sqlite`
/// (indexed before the flip, or a fresh `rag-rat init` over an old checkout) keeps resolving to
/// that legacy file — never silently abandoning its memories — until `rag-rat consolidate`
/// imports and renames it, after which resolution falls through to the global store.
#[test]
fn config_load_without_a_database_key_prefers_an_existing_legacy_index() {
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-legacydb-{}-{id}", std::process::id()));
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
        tmp.canonicalize().unwrap().join(".rag-rat/index.sqlite"),
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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The IDENTITY GATE on the global default: a keyless config at a root with NO derivable repo
/// identity (non-git, or a `git init` with an unborn HEAD) stays on its PER-ROOT legacy path —
/// in the shared global store every identity-less root would pool under the one
/// `__unassigned__` placeholder scope, so two fresh non-git projects would see and overwrite
/// each other's rows, and an unborn repo would strand its placeholder rows once its first
/// commit mints a real id. Two identity-less roots therefore NEVER share a database.
#[test]
fn config_load_without_a_database_key_stays_per_root_for_identity_less_roots() {
    let keyless_config = |tag: &str| {
        let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("ragrat-noident-{tag}-{}-{id}", std::process::id()));
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
        a.canonicalize().unwrap().join(".rag-rat/index.sqlite"),
        "an identity-less root stays on its per-root legacy path",
    );
    assert_eq!(
        config_b.database,
        b.canonicalize().unwrap().join(".rag-rat/index.sqlite"),
        "each identity-less root gets its own database",
    );
    assert_ne!(config_a.database, config_b.database, "identity-less roots never share scope");

    // An UNBORN repo (`git init`, no commit yet) is identity-less too: it lands per-root, so
    // its placeholder rows adopt IN THAT DB when the first commit mints a real id (the
    // existing single-repo adoption flow), instead of stranding in the global store.
    let unborn = keyless_config("unborn");
    let git = |args: &[&str]| {
        let out =
            std::process::Command::new("git").arg("-C").arg(&unborn).args(args).output().unwrap();
        assert!(out.status.success());
    };
    git(&["init", "-q"]);
    let config = Config::load(unborn.join("rag-rat.toml")).unwrap();
    assert_eq!(
        config.database,
        unborn.canonicalize().unwrap().join(".rag-rat/index.sqlite"),
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

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
    let _ = std::fs::remove_dir_all(&unborn);
}

#[test]
fn repo_id_override_absent_is_none() {
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-repoid-none-{}-{id}", std::process::id()));
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n",
    )
    .unwrap();

    let config = Config::load(tmp.join("rag-rat.toml")).unwrap();
    assert_eq!(config.repo_id_override, None, "no [index] repo_id → None");

    let _ = std::fs::remove_dir_all(&tmp);
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
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-cfgbranch-{}-{id}", std::process::id()));
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
        main.canonicalize().unwrap(),
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

    let _ = std::fs::remove_dir_all(&tmp);
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
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-cfgnarrow-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn config_load_anchors_repo_id_override_to_main_when_the_branch_diverges() {
    // FINDING 4: repo IDENTITY is per-repo, so the `[index] repo_id` override is read from the
    // MAIN worktree's config, NOT the launching (branch-local) one. A linked worktree that pins
    // a DIFFERENT id must still resolve MAIN's — otherwise identity splits by which checkout
    // launched. This mirrors the root/database/targets anchoring above.
    let git = |dir: &Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp =
        std::env::temp_dir().join(format!("ragrat-repoid-anchor-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn config_load_anchors_repo_id_override_to_main_when_main_omits_it() {
    // The strong form of FINDING 4: MAIN omits `[index] repo_id` (identity derives from the
    // root commit), but the branch pins one. The anchored value is MAIN's absence →
    // None, so identity stays derived and launch-point-independent; the branch pin is
    // NOT honored for the shared identity (honoring it would make identity depend on
    // which worktree launched).
    let git = |dir: &Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp =
        std::env::temp_dir().join(format!("ragrat-repoid-mainomit-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

/// #427: a linked worktree's `[index] root` resolves to itself locally, but `Config::load`
/// re-anchors it to MAIN so every worktree of a repo shares one base index. The PRE-anchor
/// value (the worktree the operator actually named) would otherwise be lost after anchoring —
/// capture it so the `index` command can warn instead of silently indexing a different
/// checkout than the one named.
#[test]
fn load_records_the_pre_anchor_root_for_a_linked_worktree() {
    let git = |dir: &Path, args: &[&str]| {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-reanchor-{}-{id}", std::process::id()));
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

    let main_c = main.canonicalize().unwrap();
    let linked_c = linked.canonicalize().unwrap();
    let from_linked = Config::load(linked.join("rag-rat.toml")).unwrap();
    assert_eq!(from_linked.root, main_c, "root anchors to main (existing behavior)");
    assert_eq!(
        from_linked.source_root_reanchored_from.as_deref(),
        Some(linked_c.as_path()),
        "the pre-anchor (named) linked-worktree root is captured",
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// The counterpart to the above: loading from a plain (non-worktree) repo redirects nothing,
/// so the field stays `None`.
#[test]
fn load_leaves_reanchor_none_for_the_main_worktree() {
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp =
        std::env::temp_dir().join(format!("ragrat-reanchor-none-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn cpp_target_renders_h_in_its_default_globs_but_c_keeps_h_too() {
    // The simple-binding glob render goes through `default_include_globs`, so a `cpp` binding
    // includes `**/*.h` (the header-resolution fix) while `c` keeps it as well.
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("ragrat-prec-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[target_bindings]\nc = [\".\"]\ncpp = [\".\"]\n",
    )
    .unwrap();
    let config = Config::load(root.join("rag-rat.toml")).unwrap();
    let cpp = config.targets.iter().find(|t| t.language == Language::Cpp).unwrap();
    assert!(cpp.include.contains(&"**/*.h".to_string()), "cpp globs: {:?}", cpp.include);
    // cpp must sort ahead of c so it wins the ambiguous `.h` (index_precedence).
    assert!(
        cpp.index_precedence()
            < config.targets.iter().find(|t| t.language == Language::C).unwrap().index_precedence(),
        "cpp must outrank c for the shared .h header"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn anchor_root_preserves_subdir_and_redirects_linked_to_main() {
    let git = |dir: &Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-cfg-{}-{id}", std::process::id()));
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

    let main_c = main.canonicalize().unwrap();
    let linked_c = linked.canonicalize().unwrap();

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
    let plain_c = plain.canonicalize().unwrap();
    assert_eq!(config::anchor_root_to_main_worktree(&plain_c), plain_c);

    // A linked-worktree subdir root that does NOT exist in main must NOT anchor to a missing
    // `main/<rel>` path (#219 review): the branch created `branch_only/`, which main never had.
    // The anchored `main/branch_only` doesn't exist, so resolution keeps the linked checkout's
    // (existing) root — otherwise `Config.root` would point outside any discoverable repo path.
    let branch_only = linked.join("branch_only");
    std::fs::create_dir_all(&branch_only).unwrap();
    let branch_only_c = branch_only.canonicalize().unwrap();
    assert!(!main_c.join("branch_only").exists(), "main never had this dir");
    assert_eq!(
        config::anchor_root_to_main_worktree(&branch_only_c),
        branch_only_c,
        "a branch-only root that's missing in main keeps the linked checkout's root",
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn parses_simple_and_expanded_targets() {
    let root = std::env::current_dir().unwrap();
    let simple = BTreeMap::from([("rust".to_string(), vec![".".to_string()])]);
    let expanded = vec![RawTarget {
        name: "generated-ts".to_string(),
        language: "typescript".to_string(),
        directories: vec![".".to_string()],
        kind: Some("generated".to_string()),
        include: Some(vec!["**/*.ts".to_string()]),
        exclude: Some(vec!["**/*.map".to_string()]),
    }];

    let targets = config::resolve_targets(&root, simple, expanded).unwrap();

    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].language, Language::Rust);
    assert_eq!(targets[1].kind, TargetKind::Generated);
}

#[test]
fn embedding_runtime_defaults_match_local_profile() {
    let runtime = EmbeddingRuntimeConfig::default();

    assert_eq!(runtime.batch_size, 64);
    assert_eq!(runtime.ort_threads, Some(4));
    assert_eq!(runtime.omp_threads, Some(1));
    assert_eq!(runtime.max_embedding_chars, 4000);
}

#[test]
fn parses_embedding_runtime_overrides() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."
            database = ".rag-rat/index.sqlite"

            [llm.embedding.runtime]
            batch_size = 128
            ort_threads = 2
            omp_threads = 1
            max_embedding_chars = 5000
            "#,
    )
    .unwrap();

    let llm = LlmConfig::try_from(raw.llm).unwrap();

    assert_eq!(llm.embedding.runtime, EmbeddingRuntimeConfig {
        batch_size: 128,
        ort_threads: Some(2),
        omp_threads: Some(1),
        max_embedding_chars: 5000,
    });
}

#[test]
fn remote_embedding_absent_is_none() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"
            "#,
    )
    .unwrap();
    let llm = LlmConfig::try_from(raw.llm).unwrap();
    assert_eq!(llm.embedding.remote, None, "no [remote] block → remote: None");
}

#[test]
fn remote_embedding_connect_happy_path_applies_defaults() {
    // CONNECT is inferred from `endpoint` being set (#318) — no `mode` field. The selector
    // names a real MODEL; the [remote] block serves it via Ollama.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            "#,
    )
    .unwrap();
    let llm = LlmConfig::try_from(raw.llm).unwrap();
    assert_eq!(
        llm.embedding.remote,
        Some(RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            endpoint: Some("http://localhost:11434".to_string()),
            cookbook: None,
            query_endpoint: None, // connect mode: no local query box
            auth_env: None,
            gpu: None,
            num_ctx: None,
            // defaults applied when omitted
            batch_size: 256,
            concurrency: 1,
            max_batch_chars: 384_000,
            request_timeout_s: 60,
        })
    );
    let remote = llm.embedding.remote.as_ref().unwrap();
    assert!(remote.is_connect() && !remote.is_ephemeral());
    // The selector still resolves to the LOCAL fastembed model — the [remote] block overrides
    // the RUNTIME, not the model identity.
    assert_eq!(llm.embedding.backend.model_id(), Some(crate::embedding_models::FASTEMBED_MODEL_ID));
}

#[test]
fn remote_embedding_ephemeral_infers_mode_and_defaults_query_endpoint() {
    // EPHEMERAL is inferred from `cookbook` being set; `query_endpoint` defaults to the local
    // Ollama when omitted (queries embed the same model → same vector space as remote chunks).
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
    assert!(remote.is_ephemeral() && !remote.is_connect());
    assert_eq!(remote.cookbook.as_deref(), Some("@rag-rat/cookbook/modal"));
    assert_eq!(remote.endpoint, None);
    assert_eq!(remote.query_endpoint.as_deref(), Some(config::DEFAULT_QUERY_ENDPOINT));
    assert_eq!(remote.concurrency, 32);
}

#[test]
fn remote_embedding_ephemeral_honors_explicit_query_endpoint() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "./recipe.mjs"
            query_endpoint = "http://127.0.0.1:11999"
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
    assert_eq!(remote.query_endpoint.as_deref(), Some("http://127.0.0.1:11999"));
}

#[test]
fn ephemeral_non_ollama_backend_requires_an_explicit_query_endpoint() {
    // The config::DEFAULT_QUERY_ENDPOINT is a local OLLAMA URL; it only fits `backend = ollama`. A
    // non-ollama ephemeral backend that omits `query_endpoint` must be REJECTED (not silently
    // defaulted), or after teardown queries embed against local Ollama with the wrong route /
    // model → silent BM25 fallback. See `RemoteQueryEndpointRequiredForBackend`.
    let build = |backend: &str, query_line: &str| {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "sentence-transformers/all-MiniLM-L6-v2"
                backend = "{backend}"
                cookbook = "@rag-rat/cookbook modal"
                {query_line}
                "#,
        ))
        .unwrap();
        LlmConfig::try_from(raw.llm).map(|l| l.embedding.remote.unwrap())
    };

    for backend in ["infinity", "vllm"] {
        let err = build(backend, "").unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::RemoteQueryEndpointRequiredForBackend { backend: b } if b == backend
            ),
            "{backend} ephemeral without query_endpoint → RemoteQueryEndpointRequiredForBackend, \
             got {err:?}",
        );
        // An explicit query_endpoint is accepted, and the backend is preserved for the query
        // path.
        let remote = build(backend, r#"query_endpoint = "http://127.0.0.1:7997""#).unwrap();
        assert_eq!(remote.query_endpoint.as_deref(), Some("http://127.0.0.1:7997"));
        assert_eq!(remote.backend.as_db_str(), backend);
    }
    // ollama still defaults (its default IS a local Ollama).
    assert_eq!(
        build("ollama", "").unwrap().query_endpoint.as_deref(),
        Some(config::DEFAULT_QUERY_ENDPOINT),
    );
}

#[test]
fn remote_embedding_ephemeral_gpu_is_parsed_and_trimmed() {
    // EPHEMERAL: `gpu` picks the GPU the cookbook recipe provisions. The value is
    // provider-specific (Modal class / RunPod gpuTypeId) and NOT validated against an
    // allow-list here — only trimmed.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            gpu = "  A10G  "
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
    assert_eq!(remote.gpu.as_deref(), Some("A10G"));
}

#[test]
fn remote_embedding_gpu_with_connect_endpoint_is_rejected() {
    // `gpu` only applies to ephemeral `cookbook` provisioning. Set alongside a connect
    // `endpoint` it is meaningless → rejected (not silently ignored).
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            gpu = "A10G"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteGpuRequiresCookbook),
        "gpu + endpoint → RemoteGpuRequiresCookbook, got {err:?}",
    );
}

#[test]
fn remote_embedding_empty_gpu_is_rejected() {
    // A present-but-empty/whitespace `gpu` is a config error — clearer than silently dropping a
    // key the user meant to set. (Omitting `gpu` entirely is fine: the recipe uses its
    // default.)
    for value in ["\"\"", "\"   \""] {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "all-minilm"
                cookbook = "@rag-rat/cookbook/modal"
                gpu = {value}
                "#,
        ))
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteGpuEmpty),
            "gpu={value} → RemoteGpuEmpty, got {err:?}",
        );
    }
}

#[test]
fn remote_embedding_overrides_batch_and_timeout() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            auth_env = "OLLAMA_TOKEN"
            num_ctx = 4096
            batch_size = 512
            concurrency = 16
            max_batch_chars = 128000
            request_timeout_s = 120
            "#,
    )
    .unwrap();
    let llm = LlmConfig::try_from(raw.llm).unwrap();
    assert_eq!(
        llm.embedding.remote,
        Some(RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: RemoteBackend::Ollama,
            endpoint: Some("http://localhost:11434".to_string()),
            cookbook: None,
            query_endpoint: None,
            auth_env: Some("OLLAMA_TOKEN".to_string()),
            gpu: None,
            num_ctx: Some(4096),
            batch_size: 512,
            concurrency: 16,
            max_batch_chars: 128_000,
            request_timeout_s: 120,
        })
    );
}

#[test]
fn remote_embedding_zero_concurrency_and_char_budget_are_clamped() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            concurrency = 0
            max_batch_chars = 0
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().embedding.remote.unwrap();
    assert_eq!(remote.concurrency, 1);
    assert_eq!(remote.max_batch_chars, 1);
}

#[test]
fn remote_embedding_rejects_oversized_concurrency() {
    let raw: RawConfig = toml::from_str(&format!(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            concurrency = {}
            "#,
        config::MAX_REMOTE_EMBEDDING_CONCURRENCY + 1
    ))
    .unwrap();

    let err = LlmConfig::try_from(raw.llm).expect_err("oversized concurrency should reject");
    assert!(matches!(
        err,
        ConfigError::RemoteEmbeddingConcurrencyTooHigh {
            value,
            max: config::MAX_REMOTE_EMBEDDING_CONCURRENCY
        } if value == config::MAX_REMOTE_EMBEDDING_CONCURRENCY + 1
    ));
}

#[test]
fn older_remote_embedding_meta_json_deserializes_with_legacy_safe_defaults() {
    let json = r#"{
            "model": "all-minilm",
            "endpoint": "http://localhost:11434",
            "cookbook": null,
            "query_endpoint": null,
            "auth_env": null,
            "gpu": null,
            "num_ctx": null,
            "batch_size": 256,
            "request_timeout_s": 60
        }"#;
    let remote: RemoteEmbeddingConfig = serde_json::from_str(json).unwrap();
    assert_eq!(remote.concurrency, 1);
    assert_eq!(remote.max_batch_chars, 384_000);
}

#[test]
fn remote_embedding_requires_exactly_one_of_endpoint_or_cookbook() {
    // Neither → no server to reach; both → ambiguous mode. Both reject with the exactly-one
    // rule.
    let neither = r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            "#;
    let both = r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "http://localhost:11434"
            cookbook = "@rag-rat/cookbook/modal"
            "#;
    for (label, toml_str) in [("neither", neither), ("both", both)] {
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteEmbeddingModeAmbiguous),
            "{label} endpoint/cookbook → RemoteEmbeddingModeAmbiguous, got {err:?}",
        );
    }
}

#[test]
fn remote_embedding_endpoint_with_credentials_is_rejected() {
    // The endpoint is persisted whole into the index meta, so a `user:token@host` URL would
    // copy the credential into the index. Reject it and direct the user to `auth_env`.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            endpoint = "https://user:token@host:11434"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteEmbeddingEndpointHasCredentials),
        "endpoint with userinfo → RemoteEmbeddingEndpointHasCredentials, got {err:?}",
    );
}

#[test]
fn remote_embedding_endpoint_without_credentials_is_accepted() {
    // A plain host and a loopback endpoint both pass the userinfo guard (and an `@` in a path
    // is not userinfo).
    for endpoint in [
        "https://host:11434",
        "http://127.0.0.1:11434",
        "http://localhost:11434/v1/embeddings?user=a@b",
    ] {
        let raw: RawConfig = toml::from_str(&format!(
            "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
             \"sentence-transformers/all-MiniLM-L6-v2\"\n\n[llm.embedding.remote]\nmodel = \
             \"all-minilm\"\nendpoint = \"{endpoint}\"\n"
        ))
        .unwrap();
        let remote = LlmConfig::try_from(raw.llm)
            .unwrap_or_else(|e| panic!("`{endpoint}` must be accepted: {e:?}"))
            .embedding
            .remote
            .expect("remote block present");
        assert_eq!(remote.endpoint.as_deref(), Some(endpoint));
    }
}

#[test]
fn endpoint_authority_has_userinfo_classifies_urls() {
    assert!(config::endpoint_authority_has_userinfo("https://user:token@host:11434"));
    assert!(config::endpoint_authority_has_userinfo("http://u@127.0.0.1"));
    assert!(!config::endpoint_authority_has_userinfo("https://host:11434"));
    assert!(!config::endpoint_authority_has_userinfo("http://127.0.0.1:11434"));
    // An `@` in the PATH/query is not userinfo.
    assert!(!config::endpoint_authority_has_userinfo("http://host:11434/path?x=a@b"));
}

#[test]
fn resolve_relative_cookbook_path_anchors_relative_recipe_paths_to_config_dir() {
    let dir = Path::new("/repo/sub");

    // A path-shaped spec resolves its FIRST token against `config_dir` and preserves any
    // trailing provider args verbatim. The resolved token carries the platform-NATIVE
    // separator (`\` on Windows), so assert it as a `Path`, not a `String`: `Path`
    // equality is separator-agnostic on Windows and normalizes a mid-path `.` on every
    // OS, so one assertion holds cross-platform without hardcoding a separator
    // rendering.
    let anchored = |spec: &str| -> (PathBuf, String) {
        let out =
            config::resolve_relative_cookbook_path(spec, dir).expect("path-shaped spec resolves");
        match out.split_once(' ') {
            Some((path, rest)) => (PathBuf::from(path), rest.to_string()),
            None => (PathBuf::from(out), String::new()),
        }
    };

    let (path, rest) = anchored("./recipes/x.mts");
    assert_eq!(path, dir.join("./recipes/x.mts"));
    assert_eq!(rest, "");

    let (path, rest) = anchored("../cookbook.mjs modal");
    assert_eq!(path, dir.join("../cookbook.mjs"));
    assert_eq!(rest, "modal");

    // A bare relative `.ts`/`.mts`/`.js` path (no `./`) is still path-shaped → resolved.
    let (path, rest) = anchored("recipe.mts");
    assert_eq!(path, dir.join("recipe.mts"));
    assert_eq!(rest, "");

    // npm package specs and a bare token are LEFT VERBATIM (None).
    assert_eq!(config::resolve_relative_cookbook_path("@rag-rat/cookbook modal", dir), None);
    assert_eq!(config::resolve_relative_cookbook_path("some-pkg", dir), None);
    // An ALREADY-ABSOLUTE recipe path is left verbatim (None). Use a platform-absolute path: a
    // bare `/abs/...` is NOT absolute on Windows (no drive), so it wouldn't reach the
    // absolute-bailout branch there.
    #[cfg(windows)]
    let abs_recipe = r"C:\abs\recipe.mjs runpod";
    #[cfg(not(windows))]
    let abs_recipe = "/abs/recipe.mjs runpod";
    assert_eq!(config::resolve_relative_cookbook_path(abs_recipe, dir), None);

    // Drive-agnostic on Windows: a NON-C drive anchors the same way (the `E:` prefix survives
    // untouched). An absolute `E:\…` recipe is still left verbatim.
    #[cfg(windows)]
    {
        let out = config::resolve_relative_cookbook_path("./r/x.mts", Path::new(r"E:\proj"))
            .expect("relative recipe on a non-C drive resolves");
        assert_eq!(PathBuf::from(out), Path::new(r"E:\proj").join("./r/x.mts"));
        assert_eq!(config::resolve_relative_cookbook_path(r"E:\abs\recipe.mjs", dir), None);
    }
}

#[test]
fn remote_embedding_query_endpoint_with_credentials_is_rejected() {
    // The query_endpoint is persisted too, so userinfo in it is rejected the same as
    // `endpoint`.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "all-minilm"
            cookbook = "@rag-rat/cookbook/modal"
            query_endpoint = "http://user:tok@127.0.0.1:11434"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteEmbeddingEndpointHasCredentials),
        "query_endpoint with userinfo → RemoteEmbeddingEndpointHasCredentials, got {err:?}",
    );
}

#[test]
fn remote_embedding_missing_model_is_rejected() {
    // The two `model` keys are distinct: `[llm.embedding] model` is the registry SELECTOR;
    // `[remote] model` is the Ollama API model name — it's the latter that's required here.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            endpoint = "http://localhost:11434"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteEmbeddingMissingModel),
        "omitted [remote] model → RemoteEmbeddingMissingModel, got {err:?}",
    );

    // A whitespace-only `[remote] model` trims to empty and is rejected the same way.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"

            [llm.embedding.remote]
            model = "   "
            endpoint = "http://localhost:11434"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::RemoteEmbeddingMissingModel),
        "whitespace-only [remote] model → RemoteEmbeddingMissingModel, got {err:?}",
    );
}

#[test]
fn remote_backend_parses_defaults_to_ollama_and_rejects_unknown() {
    let parse = |backend_line: &str| -> Result<RemoteEmbeddingConfig, ConfigError> {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.embedding]
                model = "sentence-transformers/all-MiniLM-L6-v2"

                [llm.embedding.remote]
                model = "all-minilm"
                endpoint = "http://localhost:11434"
                {backend_line}
                "#
        ))
        .unwrap();
        LlmConfig::try_from(raw.llm).map(|llm| llm.embedding.remote.unwrap())
    };
    // Omitted → ollama (back-compat with pre-selector configs).
    assert_eq!(parse("").unwrap().backend, RemoteBackend::Ollama);
    // Explicit, case-insensitive.
    assert_eq!(parse(r#"backend = "infinity""#).unwrap().backend, RemoteBackend::Infinity);
    assert_eq!(parse(r#"backend = "VLLM""#).unwrap().backend, RemoteBackend::Vllm);
    // Unknown → a clear config error naming the bad value.
    let err = parse(r#"backend = "tgi""#).unwrap_err();
    assert!(matches!(&err, ConfigError::RemoteBackendUnknown(v) if v == "tgi"), "got {err:?}");
}

#[test]
fn remote_backend_db_str_round_trips_and_matches_serde() {
    for b in [RemoteBackend::Ollama, RemoteBackend::Infinity, RemoteBackend::Vllm] {
        assert_eq!(RemoteBackend::from_db_str(b.as_db_str()), Some(b));
        // The serde repr (persisted into the index meta) MUST equal `as_db_str` (the runtime
        // marker + freshness/tune-key discriminator) so the two representations never drift.
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(json, format!("\"{}\"", b.as_db_str()));
    }
    assert_eq!(RemoteBackend::from_db_str("nope"), None);
}

#[test]
fn remote_backend_embed_path_is_per_backend() {
    // ollama + vLLM expose the OpenAI-standard route; infinity's v2 server serves `/embeddings`
    // (verified live). Same request/response shape — only the path differs.
    assert_eq!(RemoteBackend::Ollama.embed_path(), "/v1/embeddings");
    assert_eq!(RemoteBackend::Vllm.embed_path(), "/v1/embeddings");
    assert_eq!(RemoteBackend::Infinity.embed_path(), "/embeddings");
}

#[test]
fn remote_backend_provision_timeout_is_longer_for_vllm() {
    // vLLM's ~10-15 GB image needs a longer cold-start ceiling than ollama/infinity, or it
    // times out on Modal. ollama/infinity share the shorter default.
    assert_eq!(
        RemoteBackend::Ollama.provision_timeout(),
        RemoteBackend::Infinity.provision_timeout()
    );
    assert!(
        RemoteBackend::Vllm.provision_timeout() > RemoteBackend::Infinity.provision_timeout(),
        "vLLM must get a longer provisioning ceiling than infinity",
    );
}

#[test]
fn remote_block_on_a_non_transformer_model_is_rejected() {
    // #317 rework guardrail: Ollama can only serve transformer models. A [remote] block on the
    // static model2vec, the hash model, or `none` (embeddings disabled) is a misconfiguration —
    // reject at parse with a clear message rather than leaving a remote block that never
    // installs/provisions anything. Selectors are the HF-path model_ids now.
    for model in ["minishlab/potion-retrieval-32M", "embedding-hash", "none"] {
        let raw: RawConfig = toml::from_str(&format!(
                "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
                 \"{model}\"\n\n[llm.embedding.remote]\nmodel = \"all-minilm\"\nendpoint = \
                 \"http://localhost:11434\"\n"
            ))
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteEmbeddingNonTransformerModel(_)),
            "remote block + {model} → RemoteEmbeddingNonTransformerModel, got {err:?}",
        );
    }
}

#[test]
fn the_renamed_local_ai_table_is_rejected_with_a_migration_message() {
    // #317 renamed [local_ai] → [llm]. An old config's [local_ai] table must error LOUDLY:
    // serde would otherwise silently DROP it, reverting embedding settings to defaults on
    // upgrade. The error fires in Config::load before any directory resolution.
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-localai-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[local_ai.embedding]\nmodel = \"none\"\n",
    )
    .unwrap();
    let err = Config::load(tmp.join("rag-rat.toml")).unwrap_err();
    assert!(
        matches!(err, ConfigError::LocalAiTableRenamed),
        "[local_ai] table → LocalAiTableRenamed, got {err:?}",
    );
}

#[test]
fn the_legacy_dream_table_is_rejected_with_a_migration_message() {
    // The dream model config moved from [dream.model] → [llm.dream]. An old config's top-level
    // [dream] table must error LOUDLY: serde would otherwise silently DROP it, so an upgrade
    // from `[dream.model] enabled = true` would run the deterministic passes only (never the
    // model). Fires in Config::load before any directory resolution.
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-dream-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("rag-rat.toml"),
        "[index]\nroot = \".\"\n[dream.model]\nenabled = true\n",
    )
    .unwrap();
    let err = Config::load(tmp.join("rag-rat.toml")).unwrap_err();
    assert!(
        matches!(err, ConfigError::DreamTableMoved),
        "[dream] table → DreamTableMoved, got {err:?}",
    );
}

#[test]
fn remote_block_with_a_transformer_model_is_accepted() {
    // The inverse of the guardrail: the FastEmbed (transformer) HF-path models accept a
    // [remote] block.
    for model in [
        "sentence-transformers/all-MiniLM-L6-v2",
        "BAAI/bge-small-en-v1.5",
        "jinaai/jina-embeddings-v2-base-code",
    ] {
        let raw: RawConfig = toml::from_str(&format!(
                "[index]\nroot = \".\"\n\n[llm.embedding]\nmodel = \
                 \"{model}\"\n\n[llm.embedding.remote]\nmodel = \"all-minilm\"\nendpoint = \
                 \"http://localhost:11434\"\n"
            ))
        .unwrap();
        assert!(
            LlmConfig::try_from(raw.llm).is_ok(),
            "remote block + {model} (transformer) must be accepted",
        );
    }
}

#[test]
fn watch_config_defaults_on_and_parses_overrides() {
    let default: WatchConfig = RawWatch::default().into();
    assert!(default.enabled, "watcher is on by default");
    assert_eq!(default.debounce_ms, 400);
    assert_eq!(default.max_latency_ms, 2500);
    assert_eq!(default.periodic_sweep_secs, 300);

    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [watch]
            enabled = false
            debounce_ms = 750
            max_latency_ms = 4000
            periodic_sweep_secs = 0
            "#,
    )
    .unwrap();
    let watch: WatchConfig = raw.watch.into();
    assert_eq!(watch, WatchConfig {
        enabled: false,
        debounce_ms: 750,
        max_latency_ms: 4000,
        periodic_sweep_secs: 0,
    });
}

#[test]
fn version_check_defaults_on_and_parses_opt_out() {
    let default: VersionCheckConfig = RawVersionCheck::default().into();
    assert!(default.enabled, "version check is opted in by default");

    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[version_check]\nenabled = false\n").unwrap();
    let version_check: VersionCheckConfig = raw.version_check.into();
    assert!(!version_check.enabled, "[version_check] enabled = false opts out");
}

#[test]
fn search_defaults_off_and_parses_opt_in() {
    let default: SearchConfig = RawSearch::default().into();
    assert!(!default.graded_git_rerank, "graded git rerank is OFF by default");

    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[search]\ngraded_git_rerank = true\n").unwrap();
    let search: SearchConfig = raw.search.into();
    assert!(search.graded_git_rerank, "[search] graded_git_rerank = true opts in");
}

#[test]
fn dream_absent_defaults_to_off_and_local_ollama_connect() {
    // No `[llm.dream]` at all → disabled, with a local-Ollama CONNECT serving default
    // (byte-for-byte the pre-migration `[dream.model]` default).
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.embedding]
            model = "sentence-transformers/all-MiniLM-L6-v2"
            "#,
    )
    .unwrap();
    let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
    assert!(!dream.enabled, "the model pass is OFF by default");
    assert_eq!(dream.remote, RemoteDreamConfig::default());
    assert_eq!(dream.remote.backend, RemoteBackend::Ollama);
    assert_eq!(dream.remote.endpoint.as_deref(), Some("http://localhost:11434"));
    assert_eq!(dream.remote.model, "qwen3:4b-instruct");
    assert_eq!(dream.remote.request_timeout_s, 300);
    assert!(dream.remote.is_connect() && !dream.remote.is_ephemeral());
}

#[test]
fn dream_enabled_flag_without_remote_block_keeps_default_serving() {
    // `[llm.dream] enabled = true` with no `[llm.dream.remote]` still resolves to the default
    // (a local-Ollama connect) — dream has no in-process backend, so `remote` is never `None`.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true
            "#,
    )
    .unwrap();
    let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
    assert!(dream.enabled, "[llm.dream] enabled = true opts in");
    assert_eq!(dream.remote, RemoteDreamConfig::default());
}

#[test]
fn dream_remote_connect_happy_path_applies_defaults() {
    // CONNECT is inferred from `endpoint` being set.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true

            [llm.dream.remote]
            backend = "ollama"
            endpoint = "http://ollama.local:11434"
            model = "qwen3:8b"
            auth_env = "OLLAMA_TOKEN"
            request_timeout_s = 60
            "#,
    )
    .unwrap();
    let dream = LlmConfig::try_from(raw.llm).unwrap().dream;
    assert!(dream.enabled);
    assert_eq!(dream.remote, RemoteDreamConfig {
        backend: RemoteBackend::Ollama,
        endpoint: Some("http://ollama.local:11434".to_string()),
        cookbook: None,
        model: "qwen3:8b".to_string(),
        gpu: None,
        auth_env: Some("OLLAMA_TOKEN".to_string()),
        request_timeout_s: 60,
    });
    assert!(dream.remote.is_connect() && !dream.remote.is_ephemeral());
}

#[test]
fn dream_remote_ephemeral_infers_mode_and_parses_gpu() {
    // EPHEMERAL is inferred from `cookbook` being set; a vLLM backend serves chat, and `gpu` is
    // trimmed (not validated here). No `query_endpoint`/batching knobs exist for dream.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream]
            enabled = true

            [llm.dream.remote]
            backend = "vllm"
            cookbook = "@rag-rat/cookbook modal"
            gpu = "  A10G  "
            model = "Qwen/Qwen3-4B-Instruct-2507"
            request_timeout_s = 900
            "#,
    )
    .unwrap();
    let remote = LlmConfig::try_from(raw.llm).unwrap().dream.remote;
    assert!(remote.is_ephemeral() && !remote.is_connect());
    assert_eq!(remote.backend, RemoteBackend::Vllm);
    assert_eq!(remote.cookbook.as_deref(), Some("@rag-rat/cookbook modal"));
    assert_eq!(remote.gpu.as_deref(), Some("A10G"), "gpu is trimmed");
    assert_eq!(remote.model, "Qwen/Qwen3-4B-Instruct-2507");
    assert_eq!(remote.request_timeout_s, 900);
}

#[test]
fn dream_remote_requires_a_non_empty_model() {
    for model_line in ["", "model = \"  \""] {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.dream.remote]
                endpoint = "http://localhost:11434"
                {model_line}
                "#,
        ))
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::DreamRemoteMissingModel),
            "model={model_line:?} → DreamRemoteMissingModel, got {err:?}",
        );
    }
}

#[test]
fn dream_remote_infinity_backend_cannot_serve_chat() {
    // `infinity` is embed-only; a dream remote on it is rejected at parse time.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            backend = "infinity"
            endpoint = "http://localhost:7997"
            model = "some-model"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(&err, ConfigError::DreamBackendCannotServeChat(b) if b.as_str() == "infinity"),
        "infinity → DreamBackendCannotServeChat, got {err:?}",
    );
}

#[test]
fn dream_remote_requires_exactly_one_of_endpoint_or_cookbook() {
    // Neither → no server to reach; both → ambiguous mode. Both reject with the exactly-one
    // rule.
    let neither = r#"
            [index]
            root = "."

            [llm.dream.remote]
            model = "qwen3:8b"
            "#;
    let both = r#"
            [index]
            root = "."

            [llm.dream.remote]
            model = "qwen3:8b"
            endpoint = "http://localhost:11434"
            cookbook = "@rag-rat/cookbook modal"
            "#;
    for (label, toml_str) in [("neither", neither), ("both", both)] {
        let raw: RawConfig = toml::from_str(toml_str).unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::DreamRemoteModeAmbiguous),
            "{label} endpoint/cookbook → DreamRemoteModeAmbiguous, got {err:?}",
        );
    }
}

#[test]
fn dream_remote_gpu_with_connect_endpoint_is_rejected() {
    // `gpu` only applies to ephemeral `cookbook` provisioning. Set alongside a connect
    // `endpoint` it is meaningless → rejected.
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            endpoint = "http://localhost:11434"
            model = "qwen3:8b"
            gpu = "A10G"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::DreamRemoteGpuRequiresCookbook),
        "gpu + endpoint → DreamRemoteGpuRequiresCookbook, got {err:?}",
    );
}

#[test]
fn dream_remote_empty_gpu_is_rejected() {
    for value in ["\"\"", "\"   \""] {
        let raw: RawConfig = toml::from_str(&format!(
            r#"
                [index]
                root = "."

                [llm.dream.remote]
                backend = "vllm"
                cookbook = "@rag-rat/cookbook modal"
                model = "Qwen/Qwen3-4B-Instruct-2507"
                gpu = {value}
                "#,
        ))
        .unwrap();
        let err = LlmConfig::try_from(raw.llm).unwrap_err();
        assert!(
            matches!(err, ConfigError::RemoteGpuEmpty),
            "gpu={value} → RemoteGpuEmpty, got {err:?}",
        );
    }
}

#[test]
fn dream_remote_endpoint_with_credentials_is_rejected() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            endpoint = "https://user:token@host:11434"
            model = "qwen3:8b"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(err, ConfigError::DreamRemoteEndpointHasCredentials),
        "endpoint with userinfo → DreamRemoteEndpointHasCredentials, got {err:?}",
    );
}

#[test]
fn dream_remote_unknown_backend_is_rejected() {
    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [llm.dream.remote]
            backend = "tgi"
            endpoint = "http://localhost:11434"
            model = "qwen3:8b"
            "#,
    )
    .unwrap();
    let err = LlmConfig::try_from(raw.llm).unwrap_err();
    assert!(
        matches!(&err, ConfigError::RemoteBackendUnknown(b) if b.as_str() == "tgi"),
        "unknown backend → RemoteBackendUnknown, got {err:?}",
    );
}

#[test]
fn remote_backend_chat_capability_and_path() {
    assert!(RemoteBackend::Ollama.supports_chat());
    assert!(RemoteBackend::Vllm.supports_chat());
    assert!(!RemoteBackend::Infinity.supports_chat(), "infinity is embed-only");
    // The chat route is uniform across chat-capable backends; only serving differs.
    assert_eq!(RemoteBackend::Ollama.chat_path(), "/v1/chat/completions");
    assert_eq!(RemoteBackend::Vllm.chat_path(), "/v1/chat/completions");
}

#[test]
fn memory_surface_defaults_summary_and_parses_full_and_rejects_unknown() {
    let default: MemoryConfig = RawMemory::default().try_into().unwrap();
    assert_eq!(
        default.surface,
        MemorySurface::Summary,
        "the memory surface is `summary` by default (bodies deferred to `memory show`)"
    );

    let raw: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[memory]\nsurface = \"full\"\n").unwrap();
    let memory: MemoryConfig = raw.memory.try_into().unwrap();
    assert_eq!(memory.surface, MemorySurface::Full, "surface = \"full\" opts back to whole bodies");

    // Case-insensitive, and a round-trip through `as_str`.
    assert_eq!(MemorySurface::parse("SUMMARY"), Some(MemorySurface::Summary));
    assert_eq!(MemorySurface::parse("FULL"), Some(MemorySurface::Full));
    assert_eq!(MemorySurface::Summary.as_str(), "summary");
    assert_eq!(MemorySurface::Full.as_str(), "full");

    let bad: RawConfig =
        toml::from_str("[index]\nroot = \".\"\n\n[memory]\nsurface = \"digest\"\n").unwrap();
    assert!(matches!(
        MemoryConfig::try_from(bad.memory),
        Err(ConfigError::UnknownMemorySurface(_))
    ));
}

#[test]
fn oracle_defaults_off_and_parses_overrides() {
    let default: OracleConfig = RawOracle::default().into();
    assert!(!default.auto_run, "background oracle is OFF by default");
    assert_eq!(default.auto_run_quiet_period_secs, 900);
    assert_eq!(default.auto_run_min_interval_secs, 21_600);

    let raw: RawConfig = toml::from_str(
        r#"
            [index]
            root = "."

            [oracle]
            auto_run = true
            auto_run_quiet_period_secs = 60
            auto_run_min_interval_secs = 3600
            "#,
    )
    .unwrap();
    let oracle: OracleConfig = raw.oracle.into();
    assert_eq!(oracle, OracleConfig {
        auto_run: true,
        auto_run_quiet_period_secs: 60,
        auto_run_min_interval_secs: 3600,
    });
}

#[test]
fn rejects_unknown_language() {
    let root = std::env::current_dir().unwrap();
    let simple = BTreeMap::from([("cobol".to_string(), vec![".".to_string()])]);

    let err = config::resolve_targets(&root, simple, Vec::new()).unwrap_err();

    assert!(err.to_string().contains("unknown language"));
}

#[test]
fn log_config_defaults_off() {
    let raw: RawConfig = toml::from_str("").unwrap();
    let log: LogConfig = raw.log.try_into().unwrap();
    assert!(!log.enabled);
    assert_eq!(log.level, LogLevel::Info);
    assert_eq!(log.format, LogFormat::Text);
    assert_eq!(log.retention_days, 7);
    assert_eq!(log.max_files, 200);
}

#[test]
fn log_config_parses_and_rejects_unknown_level_and_format() {
    let raw: RawConfig = toml::from_str(
        "[log]\nenabled=true\nlevel=\"debug\"\nformat=\"json\"\nfilter=\"\
         rag_rat_core::index::ai=trace\"\nmax_files=10",
    )
    .unwrap();
    let log: LogConfig = raw.log.try_into().unwrap();
    assert!(log.enabled);
    assert_eq!(log.level, LogLevel::Debug);
    assert_eq!(log.format, LogFormat::Json);
    assert_eq!(log.filter.as_deref(), Some("rag_rat_core::index::ai=trace"));
    assert_eq!(log.max_files, 10);

    let bad_level: RawConfig = toml::from_str("[log]\nlevel=\"loud\"").unwrap();
    assert!(matches!(LogConfig::try_from(bad_level.log), Err(ConfigError::UnknownLogLevel(_))));
    let bad_fmt: RawConfig = toml::from_str("[log]\nformat=\"xml\"").unwrap();
    assert!(matches!(LogConfig::try_from(bad_fmt.log), Err(ConfigError::UnknownLogFormat(_))));
}

#[test]
fn log_dir_defaults_to_db_sibling_and_custom_is_config_relative() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("rag-rat.toml"), "[log]\nenabled=true\n").unwrap();
    let cfg = Config::load(dir.path().join("rag-rat.toml")).unwrap();
    assert_eq!(cfg.log.dir, cfg.database.parent().unwrap().join("logs"));
}

#[test]
fn for_linked_worktree_overlay_falls_back_when_branch_config_is_missing_or_invalid() {
    let git = |dir: &Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp =
        std::env::temp_dir().join(format!("ragrat-overlay-fallback-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn config_load_propagates_main_parse_error_from_linked_worktree() {
    let git = |dir: &Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ragrat-main-broken-{}-{id}", std::process::id()));
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

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn config_load_rejects_reserved_papertrail_table_from_governing_main() {
    let git = |dir: &Path, args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap()
    };
    let id = CFG_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp =
        std::env::temp_dir().join(format!("ragrat-main-papertrail-{}-{id}", std::process::id()));
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

    assert!(matches!(
        Config::load(linked.join("rag-rat.toml")),
        Err(ConfigError::PapertrailSchedulingNotSupported)
    ));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn target_directories_deduplicates_across_targets() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("extra")).unwrap();
    let cfg = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        root: dir.path().to_path_buf(),
        database: dir.path().join(".rag-rat/index.sqlite"),
        targets: vec![
            ResolvedTarget {
                name: "rust".to_string(),
                language: Language::Rust,
                directories: vec![PathBuf::from("src"), PathBuf::from("extra")],
                include: vec!["**/*.rs".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Source,
            },
            ResolvedTarget {
                name: "docs".to_string(),
                language: Language::Markdown,
                directories: vec![PathBuf::from("extra"), PathBuf::from("docs")],
                include: vec!["**/*.md".to_string()],
                exclude: Vec::new(),
                kind: TargetKind::Docs,
            },
        ],
        llm: LlmConfig::default(),
        watch: WatchConfig::default(),
        version_check: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();

    let dirs = cfg.target_directories();
    assert_eq!(
        dirs,
        vec![PathBuf::from("src"), PathBuf::from("extra"), PathBuf::from("docs")],
        "shared dirs appear once in stable order"
    );
}
