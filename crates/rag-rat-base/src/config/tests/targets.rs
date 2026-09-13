use super::*;

#[test]
fn cpp_target_renders_h_in_its_default_globs_but_c_keeps_h_too() {
    // The simple-binding glob render goes through `default_include_globs`, so a `cpp` binding
    // includes `**/*.h` (the header-resolution fix) while `c` keeps it as well.
    let root = scratch("prec");
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
fn rejects_unknown_language() {
    let root = std::env::current_dir().unwrap();
    let simple = BTreeMap::from([("cobol".to_string(), vec![".".to_string()])]);

    let err = config::resolve_targets(&root, simple, Vec::new()).unwrap_err();

    assert!(err.to_string().contains("unknown language"));
}

/// A target directory that escapes `[index] root` is refused at load, not at index time.
///
/// `push_target` only ever proved the directory EXISTS, so `../shared` was accepted here and then
/// failed the whole index run in `collect_index_files`, whose `strip_prefix(&config.root)` reports
/// a bare `prefix not found` naming nothing. The containment check moves that failure to the point
/// the mistake was made, and it is what lets the live oracle treat "under the index root" as the
/// corpus boundary rather than as a convention (#1008).
#[test]
fn rejects_a_target_directory_that_escapes_the_index_root() {
    let dir = scratch("target-escapes-root");
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("shared")).unwrap();
    let simple = BTreeMap::from([("rust".to_string(), vec!["../shared".to_string()])]);

    let err = config::resolve_targets(&root, simple, Vec::new()).unwrap_err();

    let message = err.to_string();
    assert!(message.contains("../shared"), "names the offending directory: {message}");
    assert!(message.contains("[index] root"), "names the remedy: {message}");
}

/// The other escape shape, and the reason the check cannot be a `..`-count: `Path::join` DISCARDS
/// the root entirely for an absolute argument, so an absolute directory never had to traverse
/// upward to leave the root.
#[test]
fn rejects_an_absolute_target_directory_outside_the_index_root() {
    let dir = scratch("absolute-target-outside-root");
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let outside = dir.join("shared");
    std::fs::create_dir_all(&outside).unwrap();
    let simple =
        BTreeMap::from([("rust".to_string(), vec![outside.to_string_lossy().into_owned()])]);

    let err = config::resolve_targets(&root, simple, Vec::new()).unwrap_err();

    assert!(err.to_string().contains("[index] root"), "names the remedy: {err}");
}

/// Containment is judged LEXICALLY, so a root the operator spelled through a symlink still accepts
/// the directories under it. Canonicalizing instead would make the verdict depend on how the root
/// was spelled rather than on what the config says.
#[test]
fn accepts_a_contained_target_under_a_root_spelled_through_a_symlink() {
    let dir = scratch("symlinked-root-target");
    let real = dir.join("real-repo");
    std::fs::create_dir_all(real.join("src")).unwrap();
    let linked = dir.join("linked-repo");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    #[cfg(not(unix))]
    std::os::windows::fs::symlink_dir(&real, &linked).unwrap();
    let simple = BTreeMap::from([("rust".to_string(), vec!["src".to_string()])]);

    let targets = config::resolve_targets(&linked, simple, Vec::new()).unwrap();

    assert_eq!(
        targets.len(),
        1,
        "a contained directory is not refused by the spelling of the root"
    );
}

/// Containment normalizes BOTH operands, because a root can itself carry `..`.
///
/// `for_linked_worktree_overlay` builds its root as `workdir.join([index] root)` without
/// canonicalizing, unlike the main load path. Comparing a normalized target path against an
/// unnormalized root rejected every genuinely contained target — and on that path the failure is
/// silent (the caller swallows the error and falls back to base targets), so a branch's added
/// targets would simply disappear from its overlay.
#[test]
fn a_root_spelled_with_a_parent_component_still_contains_its_targets() {
    let dir = scratch("root-with-parent-component");
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let spelled = root.join("sub").join("..");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let simple = BTreeMap::from([("rust".to_string(), vec!["src".to_string()])]);

    let targets = config::resolve_targets(&spelled, simple, Vec::new()).unwrap();

    assert_eq!(targets.len(), 1, "the root's spelling must not reject its own directories");
    assert_eq!(targets[0].directories, vec![PathBuf::from("src")]);
}

/// `ResolvedTarget.directories` are root-relative by contract, so an absolute directory inside the
/// root is stored in its contained form.
///
/// An absolute directory satisfies every `root.join(directory)` (it wins outright) but can never
/// prefix-match a root-relative path, so such a target was walked and indexed while every predicate
/// over it — `target_for_path`, and the live oracle's corpus — reported its files as belonging to
/// no target at all.
#[test]
fn an_absolute_target_directory_inside_the_root_is_stored_root_relative() {
    let dir = scratch("absolute-target-inside-root");
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let absolute = root.join("src").to_string_lossy().into_owned();
    let simple = BTreeMap::from([("rust".to_string(), vec![absolute])]);

    let targets = config::resolve_targets(&root, simple, Vec::new()).unwrap();

    assert_eq!(
        targets[0].directories,
        vec![PathBuf::from("src")],
        "stored root-relative, so prefix-matching against a root-relative path works",
    );
}

/// A whole-root binding keeps its `.` spelling.
///
/// `ensure_checkout_matches_corpus` compares the stored directory strings LITERALLY against a
/// corpus profile's declared bindings, and the shipped profiles spell the whole-root binding `.`
/// (`linux-kernel` is `{ c = ["."] }`). Relativizing it to the empty path would make
/// `oracle report --corpus <id>` reject a checkout indexed exactly as its own profile specifies.
#[test]
fn a_whole_root_target_keeps_its_dot_spelling() {
    let dir = scratch("dot-target-spelling");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let simple = BTreeMap::from([("rust".to_string(), vec![".".to_string()])]);

    let targets = config::resolve_targets(&dir, simple, Vec::new()).unwrap();

    assert_eq!(targets[0].directories, vec![PathBuf::from(".")]);
}

#[test]
fn target_directories_deduplicates_across_targets() {
    let dir = scratch("cfg-parse");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("extra")).unwrap();
    let cfg = Config {
        database_key_pinned: true,
        root: dir.to_path_buf(),
        database: dir.join(".rag-rat/index.sqlite"),
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
        ..Config::default()
    };
    std::fs::create_dir_all(dir.join("docs")).unwrap();

    let dirs = cfg.target_directories();
    assert_eq!(
        dirs,
        vec![PathBuf::from("src"), PathBuf::from("extra"), PathBuf::from("docs")],
        "shared dirs appear once in stable order"
    );
}

/// A `..` that follows a SYMLINK does not mean what the path text says, and containment has to
/// notice. With `link -> <outside>`, the directory `link/../src` that `is_dir()` validates is
/// `<outside>/src` — collapsing textually would yield `<root>/src`, accepting a directory nothing
/// looked at and storing a relative path naming a different tree for the walk to index.
#[cfg(unix)]
#[test]
fn a_parent_component_after_a_symlink_is_judged_against_the_filesystem() {
    let dir = scratch("parent-after-symlink");
    let root = dir.join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let outside = dir.join("outside");
    std::fs::create_dir_all(outside.join("src")).unwrap();
    std::os::unix::fs::symlink(outside.join("project"), root.join("link")).unwrap();
    std::fs::create_dir_all(outside.join("project")).unwrap();
    let simple = BTreeMap::from([("rust".to_string(), vec!["link/../src".to_string()])]);

    let err = config::resolve_targets(&root, simple, Vec::new()).unwrap_err();

    assert!(
        err.to_string().contains("[index] root"),
        "the directory that was validated lies outside the root: {err}",
    );
}

/// A target directory that is itself a symlink keeps its CONFIGURED spelling. The indexing walk
/// enters it with `is_dir()` and yields paths under that spelling, so rewriting the stored value to
/// the link's destination would leave every predicate over the target unable to match its own
/// files.
#[cfg(unix)]
#[test]
fn a_symlinked_target_directory_keeps_its_configured_spelling() {
    let dir = scratch("symlinked-target-spelling");
    std::fs::create_dir_all(dir.join("real_sources")).unwrap();
    std::os::unix::fs::symlink(dir.join("real_sources"), dir.join("src")).unwrap();
    let simple = BTreeMap::from([("rust".to_string(), vec!["src".to_string()])]);

    let targets = config::resolve_targets(&dir, simple, Vec::new()).unwrap();

    assert_eq!(
        targets[0].directories,
        vec![PathBuf::from("src")],
        "stored as configured, because that is the spelling the walk produces",
    );
}

/// A `dir/**` glob claims what is INSIDE `dir/`, so the prefix has to end at a separator. A bare
/// `starts_with` reads a prefix of a NAME as a directory boundary, which excluded a Unix file
/// called `drafts\secret.md` (and `draftsman.md`) as though it sat in `drafts/` — undoing, at the
/// matcher, exactly what preserving a literal backslash in `path_string` accomplishes.
#[test]
fn a_directory_glob_needs_a_separator_after_its_prefix() {
    let target = ResolvedTarget {
        name: "docs".to_string(),
        language: Language::Markdown,
        directories: vec![PathBuf::from(".")],
        include: vec!["**/*.md".to_string()],
        exclude: vec!["drafts/**".to_string()],
        kind: TargetKind::Docs,
    };

    assert!(!target.globs_claim("drafts/secret.md"), "a real child of drafts/ is excluded");
    assert!(
        target.globs_claim("drafts\\secret.md"),
        "a Unix file NAMED `drafts\\secret.md` is not in drafts/ and must not be excluded"
    );
    assert!(
        target.globs_claim("draftsman.md"),
        "a sibling whose name merely starts with the prefix is not in drafts/"
    );
    assert!(target.globs_claim("notes/keep.md"), "an unrelated path is claimed by the include");
}

/// The same boundary on the INCLUDE side: `foo/**` must not claim a file merely named `foo\bar.rs`.
#[test]
fn a_directory_include_glob_does_not_claim_a_backslash_named_sibling() {
    let target = ResolvedTarget {
        name: "rust".to_string(),
        language: Language::Rust,
        directories: vec![PathBuf::from(".")],
        include: vec!["foo/**".to_string()],
        exclude: Vec::new(),
        kind: TargetKind::Source,
    };

    assert!(target.globs_claim("foo/bar.rs"), "a real child of foo/ is claimed");
    assert!(!target.globs_claim("foo\\bar.rs"), "a file NAMED `foo\\bar.rs` is not under foo/");
    assert!(!target.globs_claim("foobar.rs"), "a prefix of the NAME is not a directory boundary");
}

/// A target carrying exactly the given patterns; the language/kind are irrelevant to
/// [`ResolvedTarget::globs_claim`], which reads only `include` and `exclude`.
fn glob_target(include: &[&str], exclude: &[&str]) -> ResolvedTarget {
    ResolvedTarget {
        name: "globs".to_string(),
        language: Language::Rust,
        directories: vec![PathBuf::from(".")],
        include: include.iter().map(|pattern| (*pattern).to_string()).collect(),
        exclude: exclude.iter().map(|pattern| (*pattern).to_string()).collect(),
        kind: TargetKind::Source,
    }
}

/// A `*` is a WILDCARD over one path component, not a substring probe. The matcher used to fall
/// through to `path.contains(pattern.trim_matches('*'))`, so `*.rs` asked only whether `.rs`
/// appeared anywhere in the path — claiming a backup (`notes.rs.bak`) and a conflict leftover
/// (`src/lib.rs.orig`) as Rust sources, and a wrong answer about what gets indexed.
#[test]
fn a_star_glob_matches_a_name_instead_of_containing_a_substring() {
    let target = glob_target(&["*.rs"], &[]);

    assert!(target.globs_claim("lib.rs"), "a root-level `.rs` file is claimed");
    assert!(!target.globs_claim("notes.rs.bak"), "`.rs` inside the NAME is not a `.rs` file");
    assert!(!target.globs_claim("src/lib.rs.orig"), "nor is a conflict leftover");
    // A single `*` stops at a separator, so an unanchored `*.rs` is root-level only. `**/*.rs` (the
    // shipped default) is the pattern that reaches every depth.
    assert!(!target.globs_claim("src/lib.rs"), "one `*` does not cross a separator");
    assert!(glob_target(&["**/*.rs"], &[]).globs_claim("src/lib.rs"), "`**/` does");
}

/// The same fallthrough made a bare `*` claim the entire tree: `"*".trim_matches('*')` is the empty
/// string, and every path contains it. `*` is one component's worth of wildcard.
#[test]
fn a_bare_star_claims_one_component_not_the_whole_tree() {
    let target = glob_target(&["*"], &[]);

    assert!(target.globs_claim("README.md"), "a root-level file is claimed");
    assert!(!target.globs_claim("src/lib.rs"), "a nested file is not");
    assert!(!target.globs_claim("docs/deep/guide.md"), "nor a deeper one");
    assert!(glob_target(&["**"], &[]).globs_claim("docs/deep/guide.md"), "`**` is the tree");
}

/// A pattern with no wildcard names a PATH, not a substring of one — on both sides. The old
/// fallthrough let a literal claim any path it appeared inside, so an `exclude` of `vendor` also
/// excluded `x/vendor/dep.rs` while an `include` of `src/lib.rs` also claimed `src/lib.rs.orig`.
#[test]
fn a_literal_pattern_names_a_path_not_a_substring_of_one() {
    let include = glob_target(&["src/lib.rs", "README.md"], &[]);
    assert!(include.globs_claim("src/lib.rs"), "the literal path itself is claimed");
    assert!(!include.globs_claim("src/lib.rs.orig"), "a longer name is not that path");
    assert!(!include.globs_claim("a/src/lib.rs"), "nor is the same tail deeper in the tree");
    assert!(include.globs_claim("README.md"), "a root-level literal is claimed");
    assert!(!include.globs_claim("docs/README.md"), "a same-named file elsewhere is not");

    let exclude = glob_target(&["**/*.rs"], &["vendor"]);
    assert!(
        exclude.globs_claim("vendor/dep.rs"),
        "`vendor` excludes the FILE `vendor`, not a tree"
    );
    assert!(exclude.globs_claim("x/vendor/dep.rs"), "and certainly not one nested elsewhere");
    assert!(
        !glob_target(&["**/*.rs"], &["vendor/**"]).globs_claim("vendor/dep.rs"),
        "`vendor/**` is how a subtree is excluded",
    );
}

/// The vocabulary is now real glob syntax, not the three hand-recognized shapes. Character classes,
/// alternates, `?`, and a `**` in the MIDDLE of a pattern all used to fall through to substring
/// containment over the pattern with its outer `*`s trimmed.
#[test]
fn the_full_glob_vocabulary_is_available() {
    assert!(glob_target(&["**/*.[ch]"], &[]).globs_claim("include/lib.h"), "character class");
    assert!(!glob_target(&["**/*.[ch]"], &[]).globs_claim("include/lib.hpp"), "and it is bounded");
    assert!(glob_target(&["{lib,main}.rs"], &[]).globs_claim("main.rs"), "alternates");
    assert!(!glob_target(&["{lib,main}.rs"], &[]).globs_claim("other.rs"), "and they are bounded");
    assert!(glob_target(&["a?c.rs"], &[]).globs_claim("abc.rs"), "single-character wildcard");
    assert!(!glob_target(&["a?c.rs"], &[]).globs_claim("ac.rs"), "which matches exactly one");
    assert!(glob_target(&["src/**/*.rs"], &[]).globs_claim("src/a/b/deep.rs"), "interior `**`");
    assert!(glob_target(&["src/**/*.rs"], &[]).globs_claim("src/lib.rs"), "which spans zero dirs");
    assert!(
        !glob_target(&["**/*.rs"], &["**/generated/**"]).globs_claim("src/generated/api.rs"),
        "an interior `**` on the exclude side excludes a generated subtree at any depth",
    );
}

/// Every shipped default is a `**/*.ext` suffix ([`Language::default_include_globs`]), the one
/// shape the old cascade got right. Replacing the matcher must not move a single one of them.
#[test]
fn the_shipped_default_globs_claim_exactly_what_they_did() {
    let target = glob_target(&["**/*.rs"], &[]);

    for claimed in ["lib.rs", "src/lib.rs", "src/a/b/deep.rs", "foo\\bar.rs", ".rs"] {
        assert!(target.globs_claim(claimed), "`**/*.rs` claims {claimed}");
    }
    for unclaimed in ["lib.h", "notes.rs.bak", "src/lib.rs.orig", "my.rs.template"] {
        assert!(!target.globs_claim(unclaimed), "`**/*.rs` does not claim {unclaimed}");
    }
}

/// Config load does not reject a malformed pattern, so the matcher has to answer something for one.
/// It claims NOTHING: a typo that silently widened an `include` to the whole tree, or narrowed an
/// `exclude` away, is the failure this whole function exists to stop.
#[test]
fn a_pattern_that_is_not_a_legal_glob_claims_nothing() {
    // A dangling escape — `\` with nothing after it — is a globset compile error.
    assert!(!glob_target(&["src/lib.rs\\"], &[]).globs_claim("src/lib.rs"));
    // On the exclude side an uncompilable pattern excludes nothing, so the include still stands.
    assert!(glob_target(&["**/*.rs"], &["oops\\"]).globs_claim("src/lib.rs"));
}
