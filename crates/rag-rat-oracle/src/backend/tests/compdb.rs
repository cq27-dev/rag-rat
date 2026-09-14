//! Compilation-database parsing and governance: which databases are usable, which configure
//! an indexed file, and when one is pinned.

use super::*;

/// The #1008 case is distinguishable from every other reason for not pinning, so the watcher can
/// give an operator the remedy that fits: a database that does not cover the indexed sources needs
/// regenerating (or the tree it describes needs binding), which is nothing like the advice for a
/// checkout that simply holds several databases.
#[test]
fn a_database_that_governs_nothing_is_reported_apart_from_the_other_reasons() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-governs-nothing-reported");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("third_party")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "build/compile_commands.json", &["third_party/dep.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    assert!(
        clangd.resolve_layout(&scope).has_database_governing_nothing_indexed(),
        "a loadable database describing nothing indexed is the case worth naming",
    );

    // Several databases is a DIFFERENT problem with different advice, and must not be reported as
    // this one.
    write_database(&dir, "other/compile_commands.json", &["src/main.c"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);
    assert!(
        !clangd.resolve_layout(&scope).has_database_governing_nothing_indexed(),
        "two databases is the multi-database case, whatever either one governs",
    );
}

/// A checkout whose database governs nothing it indexes is BLOCKED, and the block says so.
///
/// This is the primary #1008 shape, and its reporting had to move. Such a database counts for
/// neither trust level, so no document can warm the session — the checkout blocks before any
/// session exists, which means a message carried on the pass report could never reach it. The
/// generic prerequisite wording is worse than silence here: it tells an operator who HAS a
/// compilation database that none was found, and sends them to generate one.
#[test]
fn a_checkout_whose_database_governs_nothing_is_blocked_with_the_reason() {
    let (_dir_guard, dir) = checkout("clangd-blocked-governs-nothing");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("third_party")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "build/compile_commands.json", &["third_party/dep.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    let hint = crate::ToolManifest::for_tool(OracleTool::ClangdLsp)
        .prerequisite_blocked_with(&scope, None)
        .expect("a database governing nothing indexed blocks the backend");

    assert!(
        hint.contains("names no file this checkout indexes"),
        "the block must name the real cause, not deny the database exists: {hint}",
    );
    assert!(
        !hint.contains("found no compile_commands.json project"),
        "the generic wording sends an operator who HAS a database after the wrong problem: {hint}",
    );
}

/// `"file": ""` is a path this reader READ and found empty — a known non-governing entry — not one
/// it could not reconstruct. Collapsing the two would let a database naming no translation unit
/// count as `Governs::Unknown`, which warm-up accepts, so a checkout would warm a session that then
/// skips every one of its sources.
#[test]
fn an_empty_file_string_is_known_non_governance_not_an_unreadable_path() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-empty-file-string");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    std::fs::write(
        dir.join("compile_commands.json"),
        r#"[{"directory":"/x","file":"","command":"cc -c a.c"}]"#,
    )
    .unwrap();
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);
    let layout = clangd.resolve_layout(&scope);

    assert!(layout.sole_marker_dir().is_none(), "it names no translation unit, so nothing pins");
    assert!(
        layout.has_database_governing_nothing_indexed(),
        "the answer is known — an empty path is a path this reader read, not one it could not",
    );
    assert!(
        !clangd.checkout_can_signal_readiness(&scope, &layout),
        "and warm-up must not accept it either — the answer is known, not unknown",
    );
}

/// A database generated through a SYMLINKED spelling of the checkout still governs it.
///
/// The configured root is canonicalized at load, but a build run through the symlink writes entry
/// paths under the alias. Judging those lexically alone finds nothing in the corpus, and the
/// checkout's only database would be declared to govern nothing — withdrawing the pin from a setup
/// that works today. The parent is resolved once and memoized, so the cost lands only where the
/// literal spelling already failed.
#[test]
fn a_database_written_through_a_symlinked_root_still_governs_the_checkout() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-aliased-root");
    let real = dir.join("real");
    std::fs::create_dir_all(real.join("src")).unwrap();
    std::fs::write(real.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    let alias = dir.join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    #[cfg(not(unix))]
    std::os::windows::fs::symlink_dir(&real, &alias).unwrap();
    // The database names its entries through the ALIAS, as a build run from there would.
    write_database(&alias, "build/compile_commands.json", &["src/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&real, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&real, &corpus);

    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_some(),
        "the aliased entry names the same file the checkout indexes",
    );
}

/// Widening the per-file walk to the ceiling means "an ancestor holds a database" no longer implies
/// "that database is about this file's project" — so governance gates that walk too.
///
/// With a subdirectory `[index] root`, a database at the checkout top describing only an unindexed
/// sibling tree is now reachable from a first-party file. Resolving through it would let clangd
/// infer commands from that project's entries and persist definitions produced under unrelated
/// flags, which is precisely what the pin gate exists to prevent.
#[test]
fn an_ancestor_database_that_governs_nothing_does_not_configure_a_file() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-ancestor-governs-nothing");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("sub/src")).unwrap();
    std::fs::create_dir_all(checkout.join("other")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    std::fs::write(checkout.join("sub/src/main.c"), "int main(void) { return 0; }\n").unwrap();
    std::fs::write(checkout.join("other/dep.c"), "int dep(void) { return 1; }\n").unwrap();
    // The checkout's only database describes the sibling tree, which this checkout does not index.
    write_database(&checkout, "compile_commands.json", &["other/dep.c"]);
    let root = checkout.join("sub");
    let corpus = crate::test_support::PrefixCorpus::new(&root, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&root, &corpus);
    let layout = clangd.resolve_layout(&scope);

    assert!(layout.sole_marker_dir().is_none(), "it governs nothing indexed, so nothing is pinned");
    assert!(
        !clangd.session_can_resolve(&scope, "src/main.c", &layout),
        "and the per-file walk must not accept it either, merely for being an ancestor",
    );
}

/// An entry that configures nothing cannot qualify a database as governing the corpus.
///
/// Loadability and governance are counted independently, so without this a database whose only
/// indexed entry has an empty invocation — while a vendored entry carries a real command — would be
/// `Loadable` AND count as governing, and get pinned for a file clangd cannot configure.
#[test]
fn an_entry_that_configures_nothing_does_not_qualify_the_database() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-degenerate-indexed-entry");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("third_party")).unwrap();
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    let root = rag_rat_base::paths::canonicalize(&dir).unwrap();
    let database = format!(
        r#"[{{"directory":{d},"file":{indexed},"command":""}},
           {{"directory":{d},"file":{vendored},"command":"cc -c dep.c"}}]"#,
        d = serde_json::to_string(&root.to_string_lossy()).unwrap(),
        indexed = serde_json::to_string(&root.join("src/main.c").to_string_lossy()).unwrap(),
        vendored =
            serde_json::to_string(&root.join("third_party/dep.c").to_string_lossy()).unwrap(),
    );
    std::fs::write(dir.join("build/compile_commands.json"), database).unwrap();
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_none(),
        "the one entry naming an indexed file configures nothing, so the database describes \
         nothing this checkout can actually analyse",
    );
}

/// One first-party entry is enough. The threshold is ANY, not most — a real database mixes
/// vendored and first-party translation units freely, and clangd's inference from a sibling entry
/// is correct WITHIN a project. What #1008 is about is the sibling belonging to a different
/// project, not that inference happened.
#[test]
fn a_database_naming_one_indexed_file_among_vendored_ones_is_pinned() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-mixed-database");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("third_party/foo")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "build/compile_commands.json", &[
        "third_party/foo/a.c",
        "third_party/foo/b.c",
        "src/main.c",
    ]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    assert_eq!(
        clangd.resolve_layout(&scope).sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().join("build").as_path()),
        "one indexed entry qualifies the database, wherever it sits in the list",
    );
}

/// Entries carrying ABSOLUTE paths are resolved as given. A database committed from another
/// machine names paths that resolve nowhere in this checkout, so it governs nothing and is not
/// pinned — deliberately, since its include paths are wrong for this machine too.
#[test]
fn an_absolute_path_database_governs_only_when_its_paths_land_in_this_checkout() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-absolute-paths");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    // `write_database` already emits absolute `file` paths rooted at the fixture.
    write_database(&dir, "build/compile_commands.json", &["src/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);
    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_some(),
        "absolute paths that land inside the checkout govern it",
    );

    // The same database as another machine wrote it.
    std::fs::write(
        dir.join("build/compile_commands.json"),
        r#"[{"directory":"/build/agent/out","file":"/build/agent/src/main.c","command":"cc -c main.c"}]"#,
    )
    .unwrap();
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_none(),
        "paths from another machine name nothing this checkout indexes",
    );
}

/// A database that describes only files this checkout does NOT index is never pinned.
///
/// `--compile-commands-dir` is global, so pinning a vendored subtree's database hands first-party
/// sources that project's `-D` and include flags — a different preprocessor branch, and so a
/// different definition, persisted as trusted evidence (#1008).
#[test]
fn a_database_governing_nothing_indexed_is_not_pinned() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-governs-nothing");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("third_party/foo")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    std::fs::write(dir.join("third_party/foo/dep.c"), "int dep(void) { return 1; }\n").unwrap();
    write_database(&dir, "third_party/foo/compile_commands.json", &["third_party/foo/dep.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    let layout = clangd.resolve_layout(&scope);

    assert!(
        layout.sole_marker_dir().is_none(),
        "the only database describes nothing this checkout indexes, so it must not be forced on \
         the whole checkout",
    );
}

/// The mainstream layout stays pinned: an out-of-tree build directory holds the database, and the
/// sources it names are the indexed ones.
///
/// This is the regression guard for the tempting wrong predicate. Testing corpus membership of the
/// DATABASE'S OWN LOCATION would fail here — `build` is in the indexing floor and gitignored
/// besides, so a `build/compile_commands.json` is never in the corpus of any repo, and pinning
/// would be withdrawn from essentially every real single-database checkout.
#[test]
fn a_build_directory_database_governing_indexed_sources_is_pinned() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-build-dir-governs");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "build/compile_commands.json", &["src/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    // Pin the premise, so this cannot pass for the wrong reason: the database's own location is
    // NOT in the corpus, and the predicate must not care.
    assert!(
        !crate::backend::IndexedCorpus::indexes_file(
            scope.corpus(),
            &dir.join("build/compile_commands.json")
        ),
        "the database file itself is outside the corpus, as it is in every real repo",
    );

    let layout = clangd.resolve_layout(&scope);

    assert_eq!(
        layout.sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().join("build").as_path()),
        "it governs `src/`, which this checkout indexes",
    );
}

#[test]
fn an_out_of_tree_compilation_database_still_counts_as_a_project() {
    // A tsconfig DECLARES the sources beneath it; a compile_commands.json is a build artifact
    // that need not sit above anything. The standard out-of-tree CMake layout puts it under
    // `build/` with the sources in `src/` — measured, clangd resolves across translation units
    // there just fine, so requiring the marker to be an ancestor would report an ordinary
    // CMake project as Blocked.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-out-of-tree");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    assert!(
        !clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "sources with no compdb anywhere"
    );

    std::fs::write(dir.join("build/compile_commands.json"), COMPDB).unwrap();
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "a compdb ANYWHERE in the checkout makes the backend usable",
    );
    assert_eq!(
        clangd.warmup_document(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        Some(dir.join("src/main.c"))
    );
    assert!(
        clangd.open_signals_readiness(
            &scope(&dir),
            "src/main.c",
            &clangd.resolve_layout(&scope(&dir))
        ),
        "a source with no ancestor compdb still warms clangd",
    );
}

#[test]
fn clangd_is_told_where_a_compilation_database_it_could_not_find_lives() {
    // clangd searches only an opened file's ancestors and their `build/` subdirectory.
    // Measured: with the database in `out/` and no flag it emits no progress at all and
    // resolves calls to header declarations; with `--compile-commands-dir` it resolves across
    // translation units. Accepting the checkout is only honest because we pass the directory.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-compdb-dir");
    std::fs::create_dir_all(dir.join("out")).unwrap();
    std::fs::write(dir.join("out/compile_commands.json"), COMPDB).unwrap();

    let args = clangd.spawn_args(&clangd.resolve_layout(&scope(&dir)));
    assert_eq!(args[0], "--background-index", "the static argv comes first");
    assert!(
        args.contains(&compdb_arg(&dir.join("out"))),
        "the discovered database directory must be passed: {args:?}",
    );
}

#[test]
fn a_file_whose_database_the_session_cannot_reach_is_not_resolvable() {
    // The sharpest failure this backend has: with several databases the session points at
    // none, so a file whose database clangd cannot find on its own gets heuristic flags —
    // measured, that resolves a cross-unit call to the callee's HEADER DECLARATION. The live
    // pass must skip such files rather than persist the wrong answer.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-unreachable-db");
    // `proj-a` keeps its database where clangd looks (`build/`); `proj-b` does not.
    std::fs::create_dir_all(dir.join("proj-a/build")).unwrap();
    std::fs::create_dir_all(dir.join("proj-b/out")).unwrap();
    std::fs::write(dir.join("proj-a/build/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("proj-b/out/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("proj-a/main.c"), "int a(void){return 0;}\n").unwrap();
    std::fs::write(dir.join("proj-b/main.c"), "int b(void){return 0;}\n").unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));

    assert!(
        clangd.session_can_resolve(&scope(&dir), "proj-a/main.c", &layout),
        "clangd finds proj-a's database beside it",
    );
    assert!(
        !clangd.session_can_resolve(&scope(&dir), "proj-b/main.c", &layout),
        "proj-b's database is somewhere clangd will not look, and nothing points it there",
    );

    // With a SINGLE database the session is pointed at it, so every file is configured —
    // including one whose database is nowhere near it.
    let (_single_guard, single) = checkout("clangd-single-db");
    std::fs::create_dir_all(single.join("out")).unwrap();
    std::fs::create_dir_all(single.join("src")).unwrap();
    std::fs::write(single.join("out/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(single.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    let single_layout = clangd.resolve_layout(&scope(&single));
    assert!(clangd.session_can_resolve(&scope(&single), "src/main.c", &single_layout));
}

#[test]
fn a_re_resolved_layout_reports_when_the_pinned_database_changed() {
    // The session caches this layout so it does not re-walk the checkout every pass, and
    // re-resolves once it ages out. What matters on re-resolution is whether the checkout
    // still pins the SAME database: the server was spawned with an argv derived from the old
    // one, so a change cannot be corrected in place. Both directions are dangerous — losing
    // the database leaves the server pointed at a directory that no longer exists, and gaining
    // one leaves the new project's files analysed with the old project's flags.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-relayout");
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("build/compile_commands.json"), COMPDB).unwrap();
    let pinned = clangd.resolve_layout(&scope(&dir));
    assert!(
        pinned.pins_same_database_as(&clangd.resolve_layout(&scope(&dir))),
        "unchanged checkout"
    );

    // A SECOND database appears: the checkout no longer pins one, so the session must go.
    std::fs::create_dir_all(dir.join("other/build")).unwrap();
    std::fs::write(dir.join("other/build/compile_commands.json"), COMPDB).unwrap();
    assert!(
        !pinned.pins_same_database_as(&clangd.resolve_layout(&scope(&dir))),
        "a database added mid-session must invalidate a pinned layout",
    );

    // And the losing direction: the sole database is removed.
    std::fs::remove_file(dir.join("other/build/compile_commands.json")).unwrap();
    std::fs::remove_file(dir.join("build/compile_commands.json")).unwrap();
    assert!(!pinned.pins_same_database_as(&clangd.resolve_layout(&scope(&dir))));

    // A backend with no project marker pins nothing and never goes stale.
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    assert!(
        rust.resolve_layout(&scope(&dir)).pins_same_database_as(&rust.resolve_layout(&scope(&dir)))
    );
}

#[test]
fn a_nearer_empty_database_does_not_make_a_file_configured() {
    // In a multi-database checkout the session points at none, so per-file discovery decides.
    // clangd picks up the NEAREST database — if that one is empty it configures nothing, and
    // the file must not count as resolvable merely because some other project has a real one.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-nearer-empty");
    // THREE projects, so the checkout stays multi-database after one is hollowed out —
    // otherwise it would collapse to the single-database case, where the session pins the one
    // remaining database and every file is configured by it.
    for project in ["good", "also-good", "hollow"] {
        std::fs::create_dir_all(dir.join(project).join("build")).unwrap();
        std::fs::write(dir.join(project).join("build/compile_commands.json"), COMPDB).unwrap();
        std::fs::write(dir.join(project).join("main.c"), "int f(void){return 0;}\n").unwrap();
    }
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(clangd.session_can_resolve(&scope(&dir), "hollow/main.c", &layout));

    // Hollow out the nearer database; the file is no longer configured, while its sibling
    // project is untouched.
    std::fs::write(dir.join("hollow/build/compile_commands.json"), "[]").unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        !clangd.session_can_resolve(&scope(&dir), "hollow/main.c", &layout),
        "an empty nearest database configures nothing",
    );
    assert!(clangd.session_can_resolve(&scope(&dir), "good/main.c", &layout));
}

#[test]
fn a_databases_usability_is_read_once_per_layout() {
    // `session_can_resolve` runs once per worklist path per pass, and every source file in a
    // directory shares the same nearest database — so re-reading and re-parsing that database
    // per file multiplies the read by the worklist, while the maintenance pass holds the
    // repository write lock. The verdict is memoized for as long as the layout is trusted:
    // with the layout held, replacing the database with an unusable one cannot change the
    // answer, because the file is not read a second time.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-usability-memo");
    // Two databases, so the per-file discovery path is what decides — a single-database
    // checkout pins instead and never asks about a file's own database at all.
    for project in ["proj-a", "proj-b"] {
        std::fs::create_dir_all(dir.join(project).join("build")).unwrap();
        std::fs::write(dir.join(project).join("build/compile_commands.json"), COMPDB).unwrap();
        std::fs::write(dir.join(project).join("main.c"), "int f(void){return 0;}\n").unwrap();
    }
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(clangd.session_can_resolve(&scope(&dir), "proj-a/main.c", &layout));

    std::fs::write(dir.join("proj-a/build/compile_commands.json"), "[]").unwrap();
    assert!(
        clangd.session_can_resolve(&scope(&dir), "proj-a/main.c", &layout),
        "the answer must come from the memo, not from a second read of the database",
    );
    // The memo lives on the layout value, so a re-resolved layout sees the change — that is
    // what keeps LAYOUT_MAX_AGE the only staleness window this introduces.
    assert!(!clangd.session_can_resolve(
        &scope(&dir),
        "proj-a/main.c",
        &clangd.resolve_layout(&scope(&dir))
    ));
}

#[test]
fn a_broken_second_database_still_disqualifies_global_pinning() {
    // `--compile-commands-dir` is GLOBAL. With one working database and one empty one,
    // recording only the working site would look like a single-database checkout and pin it —
    // handing its flags to the files of the broken project, which clangd would otherwise
    // resolve by stopping at their own nearer database. Both are wrong for those files, but
    // only pinning also makes them look configured.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-broken-second-db");
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::create_dir_all(dir.join("sub/build")).unwrap();
    std::fs::write(dir.join("sub/build/compile_commands.json"), "[]").unwrap();
    std::fs::write(dir.join("sub/main.c"), "int s(void){return 0;}\n").unwrap();
    std::fs::write(dir.join("root.c"), "int r(void){return 0;}\n").unwrap();

    let layout = clangd.resolve_layout(&scope(&dir));
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "a second database disqualifies pinning even when it is unusable",
    );
    assert!(clangd.session_can_resolve(&scope(&dir), "root.c", &layout));
    assert!(
        !clangd.session_can_resolve(&scope(&dir), "sub/main.c", &layout),
        "files of the broken project are not resolvable by either route",
    );

    // Remove the broken one and the checkout is genuinely single-database again.
    std::fs::remove_file(dir.join("sub/build/compile_commands.json")).unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(clangd.spawn_args(&layout).contains(&compdb_arg(&dir)));
    assert!(clangd.session_can_resolve(&scope(&dir), "sub/main.c", &layout));
}

#[test]
fn an_entry_missing_a_required_field_is_not_a_usable_database() {
    // clangd rejects an entry lacking `directory` or a compiler invocation and falls back to
    // generic flags, so a well-formed entry naming only a file is not a usable database.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-entry-fields");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    let incomplete = [
        r#"[{"file":"/x/a.c"}]"#,
        r#"[{"file":"/x/a.c","command":"cc -c a.c"}]"#,
        r#"[{"file":"/x/a.c","directory":"/x"}]"#,
    ];
    for entry in incomplete {
        std::fs::write(dir.join("compile_commands.json"), entry).unwrap();
        assert!(
            !clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "{entry} is missing a field clangd requires",
        );
    }
    // Either invocation form is accepted.
    for complete in [
        r#"[{"file":"/x/a.c","directory":"/x","command":"cc -c a.c"}]"#,
        r#"[{"file":"/x/a.c","directory":"/x","arguments":["cc","-c","a.c"]}]"#,
    ] {
        std::fs::write(dir.join("compile_commands.json"), complete).unwrap();
        assert!(
            clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "{complete} is a usable database",
        );
    }
}

#[test]
fn the_nearest_database_decides_even_when_it_is_unusable() {
    // clangd loads the first database it finds walking up and falls back to generic flags if
    // it configures nothing — it does NOT continue to a farther ancestor. Skipping past an
    // unusable nearer database would declare the file configured by one clangd never
    // consults, and the pass would trust a fallback-flags answer.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-nearest-wins");
    // A usable database at the root, plus a second project so the layout stays multi-database
    // (single-database checkouts pin instead of using per-file discovery).
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::create_dir_all(dir.join("elsewhere/build")).unwrap();
    std::fs::write(dir.join("elsewhere/build/compile_commands.json"), COMPDB).unwrap();
    std::fs::create_dir_all(dir.join("sub/build")).unwrap();
    std::fs::write(dir.join("sub/main.c"), "int s(void){return 0;}\n").unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        clangd.session_can_resolve(&scope(&dir), "sub/main.c", &layout),
        "falls back to the root"
    );

    // Now `sub` has its own EMPTY database. clangd stops there, so the file is not configured
    // — even though the root database above it is perfectly good.
    std::fs::write(dir.join("sub/build/compile_commands.json"), "[]").unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        !clangd.session_can_resolve(&scope(&dir), "sub/main.c", &layout),
        "an unusable NEARER database means fallback flags, not the ancestor's database",
    );
}

#[test]
fn a_database_is_parsed_not_pattern_matched() {
    // Scanning for a token is wrong in BOTH directions: it rejects a valid database whose
    // entries are larger than the window, and accepts a hollow one that merely contains the
    // token inside an unrelated string. Parsing the file settles both.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-db-shape");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    // A database whose text merely MENTIONS the key names no translation unit.
    let hollows = ["[]", "{}", r#"[{"note":"{"}]"#, r#"[{"command":"cc \"file\" x.c"}]"#];
    for hollow in hollows {
        std::fs::write(dir.join("compile_commands.json"), hollow).unwrap();
        assert!(
            !clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "{hollow} names no translation unit",
        );
    }
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );

    // An entry that puts a large `arguments` array before `file` is still valid — a fixed-size
    // byte window over a prefix of the file would have rejected it.
    let bulky = format!(
        r#"[{{"directory":"/x","arguments":[{}],"file":"/x/a.c"}}]"#,
        (0..40_000).map(|i| format!(r#""-DBIG{i}=1""#)).collect::<Vec<_>>().join(","),
    );
    assert!(bulky.len() > 512 * 1024, "the fixture must exceed a small scan window");
    std::fs::write(dir.join("compile_commands.json"), &bulky).unwrap();
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "a valid database must not be rejected for putting `file` late in a big entry",
    );
}

#[test]
fn one_malformed_entry_anywhere_makes_the_whole_database_unusable() {
    // clangd loads a compilation database ALL-OR-NOTHING. Measured with clangd 19.1.2 on a
    // database whose first entry is complete and whose second lacks a compiler invocation:
    //   E[..] Failed to load compilation database from …: Missing key: "command" or "arguments".
    //   I[..] Failed to find compilation database for …/src/main.c
    //   I[..] Generic fallback command is: […]
    // and the fallback command drops the first entry's `-D` flags entirely. So checking only the
    // first entry would call that database usable, pin the session to it, and persist
    // fallback-flag answers — which resolve a cross-unit call to the callee's header declaration.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-later-entry");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    const GOOD: &str = r#"{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"}"#;
    let rejected = [
        // A later entry with no compiler invocation, and one with no `directory`.
        format!(r#"[{GOOD},{{"directory":"/x","file":"/x/b.c"}}]"#),
        format!(r#"[{GOOD},{{"file":"/x/b.c","command":"cc -c b.c"}}]"#),
        // A later element that is not an object at all, which clangd reports as `Expected
        // object.` and likewise refuses the whole file for.
        format!(r#"[{GOOD},7]"#),
        // Complete entries wrapped in something that is not the top-level array clangd requires
        // (`Expected array.`).
        format!(r#"{{"commands":[{GOOD}]}}"#),
    ];
    for database in &rejected {
        std::fs::write(dir.join("compile_commands.json"), database).unwrap();
        assert!(
            !clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "clangd refuses to load {database}, so it configures nothing",
        );
    }
    // The same entries, all complete, are a usable database — the rejections above are about the
    // malformed entry, not about having several.
    let accepted =
        format!(r#"[{GOOD},{{"directory":"/x","file":"/x/b.c","command":"cc -c b.c"}}]"#);
    std::fs::write(dir.join("compile_commands.json"), &accepted).unwrap();
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );
}

#[test]
fn an_entrys_shape_is_judged_the_way_clangd_judges_it() {
    // clangd reads a compilation database with clang's YAML/JSON reader, so the rule is about node
    // SHAPE, not JSON type: every field must be a scalar (of any kind), except `arguments`, which
    // must be a sequence. Both halves of that matter and both are easy to get wrong —
    //
    //   too strict → a database the server loads is reported unusable, and the checkout silently
    //   loses all live evidence;
    //   too loose  → a database the server DISCARDS looks usable, the session is pinned to it, and
    //   a fallback-flags answer resolving a cross-unit call to a header declaration is persisted
    //   as trusted evidence.
    //
    // Every case below was measured against clangd 19.1.2 with `--check` and
    // `--compile-commands-dir`; `Compile command from CDB` means loaded, `Failed to load
    // compilation database` / `Generic fallback command` means the whole file was discarded.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-entry-types");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();

    // `Expected sequence as value` / `Expected string as value` / `Missing key: …`.
    let rejected = [
        // A composite where a scalar belongs.
        r#"[{"directory":"/x","file":"/x/a.c","command":["cc","-c","a.c"]}]"#,
        r#"[{"directory":"/x","file":[],"command":"cc -c a.c"}]"#,
        r#"[{"directory":{},"file":"/x/a.c","command":"cc -c a.c"}]"#,
        // `arguments` present but not a sequence. `null` is the trap: it is a PRESENT field of the
        // wrong shape, which clangd rejects, and serde's `Option` would fold it into "absent" and
        // let the entry pass on its `command` alone.
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","arguments":null}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","arguments":"cc -c a.c"}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","arguments":7}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","arguments":{}}]"#,
        // Required keys absent.
        r#"[{"directory":"/x","command":"cc -c a.c"}]"#,
        r#"[{"file":"/x/a.c","command":"cc -c a.c"}]"#,
        r#"[{"directory":"/x","file":"/x/a.c"}]"#,
        // Invocation present but EMPTY, so it yields no command line. clangd loads the database
        // and then reports `Failed to parse command line` for that file (`--check` exits 3), so a
        // database whose only entry looks like this describes no analysable translation unit.
        r#"[{"directory":"/x","file":"/x/a.c","arguments":[]}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","arguments":[""]}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","command":""}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","command":"   "}]"#,
        // An element that is not a scalar makes clangd refuse the database outright.
        r#"[{"directory":"/x","file":"/x/a.c","arguments":[{}]}]"#,
        // `output` is part of the format, so a composite there is a shape error like any other.
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","output":{}}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","output":[]}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","arguments":[["cc"]]}]"#,
    ];
    for database in rejected {
        std::fs::write(dir.join("compile_commands.json"), database).unwrap();
        assert!(
            !clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "clangd refuses to load {database}, so it configures nothing",
        );
    }

    let accepted = [
        // Each invocation in its own correct form, including an empty argument list.
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","arguments":["cc","-c","a.c"]}]"#,
        // Any scalar is a scalar. clangd reads the node as text and never checks what kind it was:
        // it loads `"directory": 7` and runs the command in a directory literally named `7`, and
        // takes `"command": null` as the command string. Refusing these would be the false
        // negative above, so they are pinned as ACCEPTED rather than tightened "for consistency".
        r#"[{"directory":7,"file":"/x/a.c","command":"cc -c a.c"}]"#,
        r#"[{"directory":"/x","file":7,"command":"cc -c a.c"}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","command":null}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","command":7}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","command":true}]"#,
        // Elements of `arguments` are not type-checked by the server either.
        r#"[{"directory":"/x","file":"/x/a.c","arguments":[7,8]}]"#,
        // An unknown key is read and discarded, as clangd does with `output`.
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","output":"a.o"}]"#,
        // One blank word among real ones still leaves a parseable command line.
        r#"[{"directory":"/x","file":"/x/a.c","arguments":["","cc","-c","a.c"]}]"#,
        // THE CASE THAT MUST NOT OVER-CORRECT: an empty invocation is a PER-ENTRY failure, not a
        // database-wide one. Measured — with a good entry alongside it, the good entry's file is
        // still analysed with the database's own flags, and only the empty entry's file fails. A
        // real export can carry one degenerate line among a hundred thousand, and condemning the
        // whole database over it would cost the checkout every live verdict it would otherwise
        // get.
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"},
            {"directory":"/x","file":"/x/b.c","arguments":[]}]"#,
    ];
    for database in accepted {
        std::fs::write(dir.join("compile_commands.json"), database).unwrap();
        assert!(
            clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "clangd loads {database}, so this check must not refuse it",
        );
    }
}

#[test]
fn a_realistically_large_compilation_database_is_validated_in_one_pass() {
    // Checking every entry makes the cost scale with the database, and this runs while the
    // maintenance pass holds the repository write lock — so the size a large C++ project actually
    // produces is pinned here rather than assumed small. Measured on the fixture below (39 MB,
    // 120k entries): ~0.3s in a release build, ~3.3s in this unoptimized test build. The budget is
    // an order of magnitude above that, so it documents the cost without failing on a loaded
    // machine; only a change that made validation super-linear would trip it.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-large-db");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();

    // Shaped like a real Ninja/CMake export: absolute paths, one entry per translation unit, and
    // a flag list long enough that the file's size comes from the commands rather than the count.
    const FLAGS: &str = "-DNDEBUG -DUSE_AURA=1 -DCOMPONENT_BUILD -I../.. -Igen \
                         -I../../third_party/abseil-cpp -I../../third_party/boringssl/src/include \
                         -O2 -std=c++20 -fno-exceptions -fno-rtti -Wall";
    let mut database = String::from("[");
    for unit in 0..120_000 {
        if unit > 0 {
            database.push(',');
        }
        database.push_str(&format!(
            r#"{{"directory":"/w/out/Release","file":"/w/components/mod{unit}/impl.cc","#
        ));
        database.push_str(&format!(
            r#""command":"clang++ {FLAGS} -c ../../components/mod{unit}/impl.cc "#
        ));
        database.push_str(&format!(r#"-o obj/mod{unit}/impl.o"}}"#));
    }
    database.push(']');
    assert!(database.len() > 32 * 1024 * 1024, "the fixture must be tens of megabytes");
    std::fs::write(dir.join("compile_commands.json"), &database).unwrap();
    drop(database);

    let started = std::time::Instant::now();
    let layout = clangd.resolve_layout(&scope(&dir));
    let elapsed = started.elapsed();
    assert!(layout.sole_marker_dir().is_some(), "a large database is still a usable one");
    assert!(
        elapsed < Duration::from_secs(30),
        "validating one large compilation database took {elapsed:?}",
    );

    // The governance answer comes from the SAME pass — there is no second read of the file — and
    // this is its worst case: a corpus that matches nothing means every entry is captured and
    // tested, with no short-circuit. That is the direction the vendored case takes, so it is the
    // one worth budgeting; the mainstream case qualifies on entry one and never gets here.
    let nothing_indexed = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = crate::backend::CheckoutScope::resolve(&dir, &nothing_indexed);
    let started = std::time::Instant::now();
    let layout = clangd.resolve_layout(&scope);
    let elapsed = started.elapsed();
    assert!(
        layout.sole_marker_dir().is_none(),
        "its entries name another machine's tree, so it governs nothing here",
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "testing every entry against the corpus took {elapsed:?}",
    );
}

#[test]
fn the_invocation_clangd_selects_is_the_one_that_must_configure_the_file() {
    // `command` and `arguments` are not alternatives to be accepted independently. Measured with
    // clangd 19.1.2: when an entry carries both, `arguments` is used and `command` is ignored
    // ENTIRELY, whichever order the keys appear in — a good `command` beside an empty `arguments`
    // yields `Failed to parse command line` and `--check` exits 3. Validating either field on its
    // own would call that entry usable and let the checkout be pinned to a database that configures
    // nothing.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-invocation-choice");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();

    for database in [
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","arguments":[]}]"#,
        // Key order must not change the answer.
        r#"[{"directory":"/x","file":"/x/a.c","arguments":[],"command":"cc -c a.c"}]"#,
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","arguments":[""]}]"#,
    ] {
        std::fs::write(dir.join("compile_commands.json"), database).unwrap();
        assert!(
            !clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "`arguments` is what clangd uses, so {database} configures nothing",
        );
    }

    // The converse: an empty `command` beside real `arguments` is fine, because the field clangd
    // ignores is the empty one.
    std::fs::write(
        dir.join("compile_commands.json"),
        r#"[{"directory":"/x","file":"/x/a.c","command":"","arguments":["cc","-c","a.c"]}]"#,
    )
    .unwrap();
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );
}

#[test]
fn a_key_outside_the_modelled_format_is_uncertainty_rather_than_acceptance() {
    // clangd's entry schema is CLOSED. Measured with clangd 19.1.2: an unrecognised key is refused
    // with `Unknown key`, and the WHOLE database falls back to generic flags — the planted
    // `-DPROBE_OK=1` disappears from the compiler invocation, while `--check` still exits 0. So
    // swallowing unknown keys marked such a database loadable, the session was pinned to it, and
    // files it "governed" were resolved under fallback flags and persisted as trusted evidence.
    //
    // The verdict is UNKNOWN rather than not-loadable on purpose. The schema belongs to clangd, and
    // an unrecognised key means this crate's model of it may simply be behind — enough to decline
    // pinning and decline resolving through it, not enough to declare the checkout unwarmable on
    // the strength of a list that could be out of date.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-unmodelled-key");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    std::fs::write(
        dir.join("compile_commands.json"),
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","extra":"x"}]"#,
    )
    .unwrap();

    let layout = clangd.resolve_layout(&scope(&dir));
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "a database clangd would refuse must not be pinned",
    );
    assert!(
        !clangd.session_can_resolve(&scope(&dir), "src/main.c", &layout),
        "…nor may a file be resolved through it, which is how fallback flags get persisted",
    );
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &layout),
        "…but an unrecognised key is not proof the checkout has no project either",
    );

    // The modelled optional key is genuinely fine, so this is not a blanket refusal of extras.
    std::fs::write(
        dir.join("compile_commands.json"),
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","output":"a.o"}]"#,
    )
    .unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        clangd
            .spawn_args(&layout)
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with("--compile-commands-dir=")),
        "`output` is part of the format and must stay loadable",
    );
}

#[test]
fn a_database_this_crate_cannot_parse_is_not_trusted_but_does_not_block_the_backend() {
    // The two questions asked of a database have OPPOSITE costs of being wrong, so one "usable"
    // flag cannot serve both. Resolving a file through a database that turns out not to load
    // persists a WRONG verdict; declaring the checkout unwarmable means the backend never runs and
    // the checkout gets no live evidence at all.
    //
    // clangd reads compilation databases with clang's YAML reader, so it loads `#` comments,
    // trailing commas, and block syntax that `serde_json` refuses (#1016). Such a file is therefore
    // UNKNOWN rather than bad: it must not be pinned or resolved through, and it must not block.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-unreadable-db");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    // Valid YAML that clangd loads and `serde_json` cannot parse.
    std::fs::write(
        dir.join("compile_commands.json"),
        "[\n  # generated by hand\n  {\"directory\":\"/x\",\"file\":\"/x/a.c\",\"command\":\"cc \
         -c a.c\"},\n]\n",
    )
    .unwrap();

    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &layout),
        "a database this crate cannot read must not report the whole backend blocked",
    );
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "…but it is not proof of anything either, so the session is not pinned to it",
    );

    // A database we CAN read and that describes nothing is a positive finding, not an unknown —
    // it still blocks, which is what stops a session warming forever on an empty project.
    std::fs::write(dir.join("compile_commands.json"), "[]").unwrap();
    assert!(
        !clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "a database that parses and describes no translation unit is still refused",
    );
}

/// A marker path that is not a regular file is refused without opening it.
///
/// `File::open` on a FIFO with no writer blocks until one appears, and both marker callers gate on
/// existence alone — so a stray `mkfifo compile_commands.json` used to hang the scan. That scan
/// runs under the repository write lock, which turns one unusable file into a wedged maintenance
/// pass rather than a reported one.
///
/// A directory at the same path would NOT prove the guard: `File::open` already fails on one. The
/// FIFO is the case that separates "opening it failed" from "we never opened it".
#[cfg(unix)]
#[test]
fn a_marker_path_that_is_not_a_regular_file_is_refused_without_opening_it() {
    use std::os::unix::ffi::OsStrExt;

    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-fifo-marker");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();

    let marker = dir.join("compile_commands.json");
    let c_path = std::ffi::CString::new(marker.as_os_str().as_bytes()).unwrap();
    // SAFETY: a nul-terminated path this test owns, in a scratch directory nothing else writes.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0, "mkfifo must succeed");
    assert!(marker.exists(), "the FIFO passes the existence gate both callers apply");

    // Reaching this line at all is the assertion: before the guard, resolving the layout opened
    // the FIFO and never returned.
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &layout),
        "a marker that cannot be read says nothing about the project, so it must not block",
    );
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "…and it is not evidence either, so the session is not pinned to it",
    );
}

#[test]
fn a_database_clangd_can_read_is_not_refused_over_a_bom_or_trailing_bytes() {
    // Both are accepted by clangd (measured) and rejected by `serde_json`, so without handling
    // them a perfectly good database is reported unusable and the checkout silently loses all live
    // evidence — the expensive direction to be wrong in. A BOM is what a generator on Windows can
    // emit; trailing bytes are what a hand-edited file can end up with.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-db-syntax");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();

    for (label, database) in [
        ("a UTF-8 BOM", format!("\u{feff}{COMPDB}")),
        ("trailing bytes", format!("{COMPDB}\n// generated\n")),
        ("a BOM and trailing bytes", format!("\u{feff}{COMPDB}\n")),
    ] {
        std::fs::write(dir.join("compile_commands.json"), &database).unwrap();
        assert!(
            clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "clangd loads a database with {label}, so this check must not refuse it",
        );
    }

    // The prefix must not become a way to smuggle a hollow database past the check: what follows
    // it is still validated.
    for database in ["\u{feff}[]", "\u{feff}[{\"file\":\"/x/a.c\"}]"] {
        std::fs::write(dir.join("compile_commands.json"), database).unwrap();
        assert!(
            !clangd
                .checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
            "a BOM does not excuse {database}",
        );
    }
}

#[test]
fn an_empty_compilation_database_is_not_a_project() {
    // `[]` is valid JSON and a valid database file, but describes nothing to load: measured,
    // clangd emits no readiness cycle for it at all. Accepting it would report the backend
    // runnable while it could only ever sit in `Warming`.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-empty-db");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void){return 0;}\n").unwrap();
    std::fs::write(dir.join("compile_commands.json"), "[]").unwrap();
    assert!(
        !clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );

    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );
}

#[test]
fn several_compilation_databases_are_left_to_the_servers_own_per_file_lookup() {
    // `--compile-commands-dir` is GLOBAL: it overrides clangd's per-file search for every
    // document. With one database that is exactly right; with several it would force one
    // project's flags onto another's files, and wrong `-D`/include flags select a different
    // `#ifdef` branch — a wrong definition, persisted. So the flag is only passed when it is
    // unambiguous, and otherwise clangd's own per-file lookup (ancestors and their `build/`)
    // decides, which is correct by construction.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-multi-db");
    for project in ["proj-a", "proj-b"] {
        std::fs::create_dir_all(dir.join(project).join("build")).unwrap();
        std::fs::write(dir.join(project).join("build/compile_commands.json"), COMPDB).unwrap();
        std::fs::write(dir.join(project).join("main.c"), "int main(void){return 0;}\n").unwrap();
    }
    assert_eq!(
        clangd.spawn_args(&clangd.resolve_layout(&scope(&dir))),
        vec![OsString::from("--background-index")],
        "no database may be forced globally when several exist",
    );
    // Each project's own file is still fine: clangd finds `<dir>/build/` beside it.
    assert!(clangd.open_signals_readiness(
        &scope(&dir),
        "proj-a/main.c",
        &clangd.resolve_layout(&scope(&dir))
    ));
    // A file belonging to no project is not a usable warm-up document here, because nothing
    // points the session at a database on its behalf.
    std::fs::write(dir.join("stray.c"), "int stray(void){return 0;}\n").unwrap();
    assert!(!clangd.open_signals_readiness(
        &scope(&dir),
        "stray.c",
        &clangd.resolve_layout(&scope(&dir))
    ));
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "the per-project files remain warmable, so the backend is not blocked",
    );
}
