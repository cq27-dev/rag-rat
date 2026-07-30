//! Registry, layout, and warm-up-document behaviour for the live backends.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rag_rat_base::language::Language;
use rag_rat_base::test_scratch::{self, ScratchDir};

use super::documents::enclosing_project_dir;
use super::registry::LiveBackend;
use crate::OracleTool;
use crate::test_support::every_path_scope as scope;

/// A scratch checkout, paired with the CANONICAL spelling of its root — the only spelling these
/// tests may build paths from.
///
/// [`CheckoutScope::resolve`](super::CheckoutScope::resolve) canonicalizes the root it is handed
/// (as `Config::load` does), so every path a layout, marker search or warm-up document comes back
/// as is spelled through that root. Scratch paths reach their directory through a symlinked
/// ancestor, so the guard's own spelling is a SECOND name for it — comparing against that name is
/// the divergence macOS (`/var` → `/private/var`) and Windows (8.3 `RUNNER~1`) hand over by
/// default, and it is why the guard is returned opaque here (#1027).
fn checkout(tag: &str) -> (ScratchDir, PathBuf) {
    let scratch = ScratchDir::new(tag);
    let root = test_scratch::canonical_config_root(scratch.path());
    (scratch, root)
}

/// The guard's spelling of a scratch root and the canonical one [`checkout`] hands back must be
/// two names for ONE directory. Without the divergence every assertion in this module would pass
/// whether it derived its paths from the canonical root or from the guard, and the root-spelling
/// class would only redden the cross-platform legs (#1027).
#[cfg(unix)]
#[test]
fn a_scratch_checkouts_canonical_root_diverges_from_the_guards_spelling() {
    let (guard, root) = checkout("scope-root-spelling");
    assert_ne!(root, guard.path(), "the guard must reach its root through a symlinked ancestor");
    assert_eq!(
        root,
        rag_rat_base::paths::canonicalize(guard.path()).unwrap(),
        "both spellings name one directory"
    );
    assert_eq!(scope(&root).root(), root, "and the scope resolves to the canonical one");
}

/// A compilation database naming `files`, written at `dir/relative`.
fn write_database(dir: &Path, relative: &str, files: &[&str]) {
    let entries: Vec<String> = files
        .iter()
        .map(|file| {
            let absolute = dir.join(file);
            format!(
                r#"{{"directory":{},"file":{},"command":"cc -c {}"}}"#,
                serde_json::to_string(&dir.to_string_lossy()).unwrap(),
                serde_json::to_string(&absolute.to_string_lossy()).unwrap(),
                file
            )
        })
        .collect();
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("[{}]", entries.join(","))).unwrap();
}

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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

    assert!(
        clangd.resolve_layout(&scope).has_database_governing_nothing_indexed(),
        "a loadable database describing nothing indexed is the case worth naming",
    );

    // Several databases is a DIFFERENT problem with different advice, and must not be reported as
    // this one.
    write_database(&dir, "other/compile_commands.json", &["src/main.c"]);
    let scope = super::CheckoutScope::resolve(&dir, &corpus);
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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);
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
    let scope = super::CheckoutScope::resolve(&real, &corpus);

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
    let scope = super::CheckoutScope::resolve(&root, &corpus);
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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);
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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_none(),
        "paths from another machine name nothing this checkout indexes",
    );
}

/// A symlinked build directory pointing at a SIBLING of the index root is followed: it is still
/// inside the checkout, and the database behind it is genuinely this checkout's.
///
/// The containment bound used to be the index root, so `sub/build -> ../out` pointed "out of
/// scope" and the database was never seen. The bound is the checkout now, which is what makes this
/// ordinary layout work while still refusing a link that leaves the checkout entirely.
#[test]
fn a_symlinked_build_directory_inside_the_checkout_is_followed() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-symlink-sibling");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("sub/src")).unwrap();
    std::fs::create_dir_all(checkout.join("out")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    std::fs::write(checkout.join("sub/src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&checkout, "out/compile_commands.json", &["sub/src/main.c"]);
    #[cfg(unix)]
    std::os::unix::fs::symlink("../out", checkout.join("sub/build")).unwrap();
    #[cfg(not(unix))]
    std::os::windows::fs::symlink_dir(checkout.join("out"), checkout.join("sub/build")).unwrap();
    let root = checkout.join("sub");
    let corpus = crate::test_support::PrefixCorpus::new(&root, &["src"]);
    let scope = super::CheckoutScope::resolve(&root, &corpus);

    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_some(),
        "a link to a sibling of the index root stays inside the checkout",
    );
}

/// Outside a git checkout nothing widens: the ceiling is the configured root, and every answer is
/// the one the pre-ceiling code gave.
#[test]
fn a_non_git_checkout_behaves_exactly_as_before() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-non-git");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "compile_commands.json", &["src/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

    assert_eq!(scope.ceiling(), scope.root(), "no checkout ⇒ no widening");
    assert_eq!(
        clangd.resolve_layout(&scope).sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().as_path()),
    );
}

/// A checkout whose indexed sources live only under a HIDDEN directory can still warm a session.
///
/// The warm-up document search excluded every dot-directory, so such a checkout had a usable
/// database and no findable document, and the prerequisite gate reported the backend `Blocked` for
/// a configuration that would have worked (#1011). The indexed corpus is the authority on where
/// this checkout's sources are; the search now asks it instead of guessing from the name.
#[test]
fn sources_under_a_hidden_directory_can_warm_the_session() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-hidden-sources");
    std::fs::create_dir_all(dir.join(".cache/generated")).unwrap();
    std::fs::write(dir.join(".cache/generated/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "build/compile_commands.json", &[".cache/generated/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &[".cache/generated"]);
    let scope = super::CheckoutScope::resolve(&dir, &corpus);
    let layout = clangd.resolve_layout(&scope);

    assert_eq!(
        clangd.warmup_document(&scope, &layout),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().join(".cache/generated/main.c")),
        "a hidden directory the checkout indexes is an ordinary source location",
    );
    assert!(clangd.checkout_can_signal_readiness(&scope, &layout), "so it is not blocked");
}

/// The blanket dot-rule was right about tooling state, and dropping it must not lose that: a
/// document is refused because the checkout does not index it, not because of its name.
#[test]
fn a_document_the_checkout_does_not_index_is_never_warmed_on() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-unindexed-documents");
    std::fs::create_dir_all(dir.join(".cache/clangd")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    // Source-shaped files inside a machine-written tree, and one real source.
    std::fs::write(dir.join(".cache/clangd/stale.c"), "int stale(void) { return 0; }\n").unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&dir, "build/compile_commands.json", &["src/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = super::CheckoutScope::resolve(&dir, &corpus);
    let layout = clangd.resolve_layout(&scope);

    assert_eq!(
        clangd.warmup_document(&scope, &layout),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().join("src/main.c")),
        "the indexed source is chosen, never the one under clangd's own index",
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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

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
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

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

/// A database ABOVE `[index] root` still governs the sources under it, and clangd finds it for them
/// by its own ancestor search — so the layout has to see it too.
///
/// The marker walk was bounded below by the index root, so a `compile_commands.json` at the
/// worktree top was invisible: the layout came back empty and the backend reported the checkout
/// `Blocked` for a configuration it could have served (#1008).
#[test]
fn a_database_above_the_index_root_is_found_by_the_ancestor_leg() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-db-above-index-root");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("sub/src")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    std::fs::write(checkout.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::write(checkout.join("sub/src/main.c"), "int main(void) { return 0; }\n").unwrap();
    let root = checkout.join("sub");

    let layout = clangd.resolve_layout(&scope(&root));

    assert_eq!(
        layout.sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(checkout).unwrap().as_path()),
        "the ancestor leg reaches the checkout top",
    );
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&root), &layout),
        "and the checkout is no longer reported blocked",
    );
}

/// The per-file discovery walk stops at the CEILING, not at the index root — it exists to mirror
/// clangd's own ancestor search, and clangd has no notion of an index root.
#[test]
fn a_file_resolves_through_a_database_above_the_index_root() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-resolve-above-root");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("sub/src")).unwrap();
    std::fs::create_dir_all(checkout.join("sub/other")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    std::fs::write(checkout.join("compile_commands.json"), COMPDB).unwrap();
    // A second database keeps the session UNPINNED, so the per-file discovery walk is what decides
    // — which is the path this test is about. It sits INSIDE the index root on purpose: a database
    // in a sibling subtree of the root is not searched at all, because it can govern no indexed
    // source (see `ProjectLayout::complete`), so it would not disqualify pinning.
    std::fs::write(checkout.join("sub/other/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(checkout.join("sub/src/main.c"), "int main(void) { return 0; }\n").unwrap();
    let root = checkout.join("sub");
    let scope = scope(&root);
    let layout = clangd.resolve_layout(&scope);

    assert!(layout.sole_marker_dir().is_none(), "two databases ⇒ nothing is pinned");
    assert!(
        clangd.session_can_resolve(&scope, "src/main.c", &layout),
        "the file's own ancestor chain reaches the database above the index root",
    );
}

/// The ceiling is the enclosing CHECKOUT, not the configured root.
///
/// `[index] root` may legitimately sit below the checkout root, and clangd searches a file's
/// ancestors without any notion of an index root. When the two were the same path, a
/// `compile_commands.json` at the worktree top was invisible and the backend reported `Blocked`
/// for a checkout it could have served (#1008).
#[test]
fn the_ceiling_is_the_enclosing_checkout_not_the_index_root() {
    let (_dir_guard, dir) = checkout("scope-ceiling-subdir");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("sub")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    let root = checkout.join("sub");

    let scope = scope(&root);

    assert_eq!(
        scope.ceiling(),
        rag_rat_base::paths::canonicalize(checkout).unwrap(),
        "the ceiling is the checkout"
    );
    assert_eq!(
        scope.root(),
        rag_rat_base::paths::canonicalize(&root).unwrap(),
        "the root is left where it was"
    );
}

/// Outside a git checkout there is no boundary but the configured one, and inventing a wider one
/// would let a walk wander into unrelated trees.
#[test]
fn a_root_outside_a_git_checkout_is_its_own_ceiling() {
    let (_dir_guard, dir) = checkout("scope-ceiling-no-git");
    std::fs::create_dir_all(dir.join("plain")).unwrap();
    let root = dir.join("plain");

    let scope = scope(&root);

    assert_eq!(scope.ceiling(), scope.root(), "no checkout ⇒ the root is the ceiling");
}

/// A scope resolved inside a LINKED worktree gets that worktree as its ceiling, never the main
/// checkout. Each linked worktree is a different source tree, so a ceiling pointing at main would
/// admit databases and ancestors belonging to another checkout entirely — and it is the property
/// per-checkout live sessions (#1010) will need, pinned now so it cannot regress before then.
#[test]
fn a_linked_worktree_is_its_own_ceiling_not_the_main_checkout() {
    let (_dir_guard, dir) = checkout("scope-ceiling-linked");
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    rag_rat_base::test_git::run(&main, &["init"]);
    std::fs::write(main.join("seed.txt"), "seed\n").unwrap();
    rag_rat_base::test_git::run(&main, &["add", "."]);
    rag_rat_base::test_git::run(&main, &["commit", "-m", "seed"]);
    let linked = dir.join("linked");
    rag_rat_base::test_git::run(&main, &[
        "worktree",
        "add",
        linked.to_str().unwrap(),
        "-b",
        "branch",
    ]);

    let scope = scope(&linked);

    assert_eq!(
        scope.ceiling(),
        rag_rat_base::paths::canonicalize(&linked).unwrap(),
        "a linked worktree is its own checkout, not a subdirectory of main",
    );
}

#[test]
fn live_backends_are_exactly_the_non_batch_tools() {
    // The two must stay in lockstep: a live tool with no backend entry would be enumerated by
    // the watcher and spawn nothing, and a backend for a batch tool would double-write edges
    // the batch pass already owns authoritatively.
    for &tool in OracleTool::ALL {
        assert_eq!(
            LiveBackend::for_tool(tool).is_some(),
            !tool.batch_capable(),
            "{} disagrees about being a live backend",
            tool.as_db_str()
        );
    }
}

#[test]
fn every_live_backend_copies_monikers_from_a_batch_tool_for_its_own_language() {
    // A live verdict's `scip_symbol` is its batch counterpart's moniker verbatim. If the two
    // resolved different languages the copy would be meaningless, so the pairing is asserted
    // rather than assumed.
    for backend in LiveBackend::all() {
        let source = backend
            .tool
            .batch_moniker_source()
            .unwrap_or_else(|| panic!("{} has no moniker source", backend.tool.as_db_str()));
        assert!(source.batch_capable(), "a moniker source must be a batch tool");
        let batch_languages = crate::ToolManifest::for_tool(source).languages;
        for language in backend.languages {
            assert!(
                batch_languages.contains(language),
                "{} resolves {language} but copies monikers from {}, which indexes \
                 {batch_languages:?}",
                backend.tool.as_db_str(),
                source.as_db_str(),
            );
        }
    }
}

#[test]
fn every_live_backend_declares_ids_for_the_extensions_its_language_claims() {
    // `claims_path` admits a file to the worklist and `language_id_for` decides how it is
    // opened; a gap between them means a file gets resolved under a fallback id.
    for backend in LiveBackend::all() {
        for extension in backend.languages.iter().flat_map(|l| l.target_extensions()) {
            let path = format!("src/file.{extension}");
            assert!(backend.claims_path(&path), "{path} must be claimed");
            assert!(
                backend.language_ids.iter().any(|(ext, _)| ext == extension),
                "{} claims .{extension} but declares no languageId for it",
                backend.tool.as_db_str(),
            );
        }
    }
}

#[test]
fn typescript_opens_tsx_as_typescriptreact() {
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    assert_eq!(ts.language_id_for("src/main.ts"), "typescript");
    assert_eq!(ts.language_id_for("src/App.tsx"), "typescriptreact");
    // An extension the table doesn't name still opens under the backend's fallback rather
    // than an empty id the server would reject.
    assert_eq!(ts.language_id_for("src/no-extension"), "typescript");
    assert!(!ts.claims_path("src/lib.rs"), "another language's file never enters the worklist");
}

/// The `--compile-commands-dir` argument for `dir`, built the way production builds it.
fn compdb_arg(dir: &Path) -> OsString {
    let mut arg = OsString::from("--compile-commands-dir=");
    arg.push(dir.as_os_str());
    arg
}

/// A compilation database with one real entry. `[]` is syntactically valid but describes no
/// project, and clangd emits no readiness cycle for it — writing that in a fixture would
/// assert the very bug `marker_is_usable` exists to catch.
const COMPDB: &str = r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"}]"#;

/// A TypeScript project at `relative_dir` holding one `main.ts`.
fn write_project(root: &Path, relative_dir: &str) {
    let dir = root.join(relative_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
    std::fs::write(dir.join("main.ts"), "export function greet() {}\n").unwrap();
}

#[test]
fn enclosing_tsconfig_walks_up_to_the_nearest_project_and_stops_at_the_root() {
    // This is how tsserver assigns a file to a project, and it decides whether opening the
    // file produces an observable load. A file under no project opens as an inferred project
    // SILENTLY, so warming on it teaches the session nothing.
    let (_dir_guard, dir) = checkout("ts-lsp-enclosing");
    std::fs::create_dir_all(dir.join("packages/app/src")).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("packages/app/tsconfig.json"), "{}").unwrap();

    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("packages/app/src/main.ts"), &[
            "tsconfig.json"
        ]),
        Some(dir.join("packages/app")),
        "the nearest enclosing project wins",
    );
    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("scripts/tool.ts"), &["tsconfig.json"]),
        None,
        "a file under no project has none",
    );
}

#[test]
fn every_declared_marker_name_is_usable() {
    // `MarkerKind::Parsed` guarantees a parsed marker has exactly ONE name — that part of the old
    // invariant is now in the type. What the type does not say is that a name is non-empty, and an
    // empty one is worse than useless: `dir.join("")` is the directory itself, so a nameless
    // sentinel would match every directory in the checkout, and an empty pin would hand the server
    // a bare `=<dir>`. Every reader fails closed on it, but nothing should ever declare one.
    for backend in LiveBackend::all() {
        let Some(model) = backend.project_model else {
            continue;
        };
        let names = model.files();
        assert!(!names.is_empty(), "{:?} declares a marker with no name", backend.tool);
        for name in names {
            assert!(!name.is_empty(), "{:?} declares an empty marker name", backend.tool);
        }
        if let super::registry::MarkerKind::Parsed { pin, .. } = model.kind {
            assert!(!pin.is_empty(), "{:?} declares an empty marker pin flag", backend.tool);
        }
    }
}

#[test]
fn only_a_backend_that_declares_a_pin_receives_one() {
    // The marker pin is a property of the MARKER, not of spawning: it is meaningless without the
    // file it points at. Writing `--compile-commands-dir` into the shared argv builder meant any
    // backend whose layout happened to find a sole marker directory would receive clangd's flag —
    // harmless only because clangd is the one backend that produces such a layout today (#1042).
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("pin-is-declared");
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("main.c"), "int m(void){return 0;}\n").unwrap();

    // A layout that DOES name a sole marker directory.
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(layout.sole_marker_dir().is_some(), "the fixture really pins a directory");

    assert!(
        clangd.spawn_args(&layout).contains(&compdb_arg(&dir)),
        "the backend that declares a pin gets it: {:?}",
        clangd.spawn_args(&layout),
    );
    assert_eq!(
        ts.spawn_args(&layout),
        vec![OsString::from("--stdio")],
        "a backend whose marker is a sentinel declares no pin, so it receives none even from a \
         layout that has one",
    );
}

#[test]
fn any_declared_marker_name_identifies_a_project() {
    // A build system often accepts several spellings of the same declaration — Gradle takes
    // `build.gradle.kts` or `build.gradle`. With one name per marker a checkout using the other
    // spelling reads as having no project at all: the readiness signal never fires, so the session
    // can only sit in `Warming` while the prerequisite gate reports the project missing (#1042).
    let (_dir_guard, dir) = checkout("marker-alternates");
    std::fs::create_dir_all(dir.join("app/src")).unwrap();
    std::fs::create_dir_all(dir.join("lib/src")).unwrap();
    // One module declares itself with the first name, the other with the second.
    std::fs::write(dir.join("app/build.gradle.kts"), "").unwrap();
    std::fs::write(dir.join("lib/build.gradle"), "").unwrap();
    let names = &["build.gradle.kts", "build.gradle"];

    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("app/src/Main.kt"), names),
        Some(dir.join("app")),
        "the first name identifies its project",
    );
    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("lib/src/Lib.kt"), names),
        Some(dir.join("lib")),
        "and so does any other declared name — this is what one name per marker could not do",
    );
    assert_eq!(
        enclosing_project_dir(&scope(&dir), &dir.join("src/Stray.kt"), names),
        None,
        "a file under none of them still has no project",
    );
}

#[test]
fn a_warmup_document_is_found_under_a_project_declared_by_any_name() {
    // The second widened path. The warm-up search is what makes a backend usable on a checkout
    // whose changed files all sit outside a project, so a name it does not recognise there costs
    // the same thing as one the ancestor walk misses: the session never warms (#1042).
    let (_dir_guard, dir) = checkout("warmup-marker-alternates");
    std::fs::create_dir_all(dir.join("lib/src")).unwrap();
    // Only the SECOND declared name, and the document sits beneath it.
    std::fs::write(dir.join("lib/build.gradle"), "").unwrap();
    std::fs::write(dir.join("lib/src/main.ts"), "export function greet() {}\n").unwrap();

    let found = super::documents::find_document_in_project(
        &scope(&dir),
        &dir,
        &[Language::TypeScript],
        &["build.gradle.kts", "build.gradle"],
        false,
    );

    assert_eq!(
        found,
        Some(dir.join("lib/src/main.ts")),
        "a project declared by any name yields a warm-up document",
    );
}

#[test]
fn the_prerequisite_hint_names_every_spelling_that_would_satisfy_the_marker() {
    // The third widened path. An operator reading the hint must not be sent to create the one
    // spelling the backend happened to list first when another would have done (#1042).
    use crate::manifest::hint_marker_names;

    assert_eq!(
        hint_marker_names(&["build.gradle.kts", "build.gradle"]),
        "build.gradle.kts or build.gradle",
        "every declared name appears",
    );
    assert_eq!(
        hint_marker_names(&["tsconfig.json"]),
        "tsconfig.json",
        "and a single-name marker reads exactly as it did before",
    );
    assert_eq!(
        hint_marker_names(&[]),
        "project",
        "a marker declaring no name falls back to the generic wording rather than naming nothing",
    );

    // And through the real hint, for a shipped single-name backend: the rendering above is the
    // one an operator actually reads.
    let (_dir_guard, dir) = checkout("hint-single-name");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.ts"), "export function greet() {}\n").unwrap();
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let hint = crate::ToolManifest::for_tool(OracleTool::TsLsp)
        .prerequisite_blocked_with(&super::CheckoutScope::resolve(&dir, &corpus), None)
        .expect("a checkout with no tsconfig blocks the TypeScript backend");
    assert!(
        hint.contains("found no tsconfig.json project"),
        "the shipped single-name hint is unchanged: {hint}",
    );
}

#[test]
fn a_warmup_document_is_found_at_any_depth_and_only_inside_a_project() {
    // A project can sit arbitrarily deep in a monorepo; a depth limit would silently disable
    // those checkouts entirely, which is worse than the walk it saves.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("ts-lsp-warmup-doc");
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/tool.ts"), "export function x() {}\n").unwrap();
    assert_eq!(
        ts.warmup_document(&scope(&dir), &ts.resolve_layout(&scope(&dir))),
        None,
        "a TypeScript file outside every project is not a warm-up document",
    );
    assert!(!ts.checkout_can_signal_readiness(&scope(&dir), &ts.resolve_layout(&scope(&dir))));

    write_project(&dir, "services/teams/foo/web");
    assert_eq!(
        ts.warmup_document(&scope(&dir), &ts.resolve_layout(&scope(&dir))),
        Some(dir.join("services/teams/foo/web/main.ts")),
        "a deeply nested project is still found",
    );
    assert!(ts.checkout_can_signal_readiness(&scope(&dir), &ts.resolve_layout(&scope(&dir))));
}

#[test]
fn the_warmup_search_refuses_a_document_the_checkout_does_not_index() {
    // `node_modules` ships thousands of tsconfigs describing DEPENDENCIES. Warming on one would
    // report the checkout usable while none of ITS files ever resolve.
    //
    // The test supplies a REAL corpus rather than the permissive one: the refusal is now the
    // corpus's, not a directory-name test's, so a corpus that claimed everything would assert
    // nothing. That is the point of the change — the indexer's own answer decides, so the warm-up
    // search cannot disagree with what the pass will actually resolve.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("ts-lsp-vendored-warmup");
    write_project(&dir, "node_modules/some-dep");
    write_project(&dir, ".cache/tooling");
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &["src"]);
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

    assert_eq!(ts.warmup_document(&scope, &ts.resolve_layout(&scope)), None);
    assert!(!ts.checkout_can_signal_readiness(&scope, &ts.resolve_layout(&scope)));

    // And a hidden directory this checkout DOES index is an ordinary source location — the case the
    // blanket dot-rule got wrong (#1011).
    write_project(&dir, ".cache/generated");
    let corpus = crate::test_support::PrefixCorpus::new(&dir, &[".cache/generated"]);
    let scope = super::CheckoutScope::resolve(&dir, &corpus);

    assert_eq!(
        ts.warmup_document(&scope, &ts.resolve_layout(&scope)),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().join(".cache/generated/main.ts")),
    );
}

#[test]
fn a_server_status_backend_needs_no_warmup_document_and_is_never_blocked_on_one() {
    // rust-analyzer reports quiescence for any checkout, so the whole notion is TS-specific
    // and must not leak into the other backend's gating.
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    let (_dir_guard, dir) = checkout("ra-lsp-warmup-doc");
    assert_eq!(rust.warmup_document(&scope(&dir), &rust.resolve_layout(&scope(&dir))), None);
    assert!(
        rust.checkout_can_signal_readiness(&scope(&dir), &rust.resolve_layout(&scope(&dir))),
        "an empty checkout still signals"
    );
    assert!(
        rust.open_signals_readiness(&scope(&dir), "src/lib.rs", &rust.resolve_layout(&scope(&dir))),
        "any document will do"
    );
    assert!(rust.project_model.is_none(), "session-level readiness needs no project");
}

#[test]
fn clangd_serves_c_and_cpp_from_one_backend() {
    // The first backend whose language set is not a singleton. Both languages must reach its
    // worklist, or half its files would silently never be resolved.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert!(clangd.resolves_language(Language::C));
    assert!(clangd.resolves_language(Language::Cpp));
    assert!(!clangd.resolves_language(Language::Rust));
    for path in ["src/a.c", "src/a.h", "src/a.cpp", "src/a.cc", "src/a.hpp"] {
        assert!(clangd.claims_path(path), "{path} must be claimed");
    }
    assert!(!clangd.claims_path("src/a.rs"));
    assert!(!clangd.claims_path("src/a.ts"));
}

#[test]
fn clangd_opens_each_dialect_under_its_own_language_id() {
    // A C++ file opened as `c` parses under the wrong dialect, so the extension decides.
    // `.h` follows the language registry's default owner (C), which clangd copes with.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert_eq!(clangd.language_id_for("src/a.c"), "c");
    assert_eq!(clangd.language_id_for("src/a.h"), "c");
    for path in ["src/a.cc", "src/a.cpp", "src/a.cxx", "src/a.hpp", "src/a.hh"] {
        assert_eq!(clangd.language_id_for(path), "cpp", "{path}");
    }
}

#[test]
fn a_backends_project_marker_is_the_file_its_prerequisite_looks_for() {
    // The warm-up search and the prerequisite gate must ask the SAME question, or a checkout
    // could pass the gate and still have nothing to warm on (or vice versa).
    let (_dir_guard, dir) = checkout("clangd-marker");
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert_eq!(
        clangd.project_model.and_then(|m| m.parsed_file()),
        Some("compile_commands.json"),
        "the compilation database is PARSED, so it declares exactly one name",
    );
    assert!(
        !clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "no compdb ⇒ no signal possible"
    );

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    assert!(
        !clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        "sources alone are not a project"
    );
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    assert_eq!(
        clangd.warmup_document(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        Some(dir.join("src/main.c"))
    );
    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );
    // The two live backends' project markers can coexist in one checkout, so a document
    // qualifies only if THIS backend could open it — not merely because a project contains it.
    assert!(clangd.open_signals_readiness(
        &scope(&dir),
        "src/main.c",
        &clangd.resolve_layout(&scope(&dir))
    ));
    assert!(
        !clangd.open_signals_readiness(
            &scope(&dir),
            "src/app.ts",
            &clangd.resolve_layout(&scope(&dir))
        ),
        "another language's file is not a clangd warm-up document, project or not",
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
    let scope = super::CheckoutScope::resolve(&dir, &nothing_indexed);
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
fn a_symlinked_build_directory_is_still_searched() {
    // `build -> cmake-build-debug` is an ordinary layout, and the database is reachable
    // through the checkout path — but a symlink is not a directory to `DirEntry::file_type`.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-symlinked-build");
    std::fs::create_dir_all(dir.join("cmake-build-debug")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("cmake-build-debug/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("cmake-build-debug"), dir.join("build")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(dir.join("cmake-build-debug"), dir.join("build")).unwrap();

    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );
}

#[cfg(unix)]
#[test]
fn one_database_reached_through_a_symlink_alias_is_not_two_databases() {
    // `out/` plus a `current-build -> out` convenience symlink is ONE database reachable by two
    // paths. The walk follows directory symlinks on purpose — that is what makes a symlinked build
    // directory discoverable — so without collapsing aliases the checkout looks multi-database:
    // `--compile-commands-dir` is dropped, and a source outside clangd's own ancestor/`build/`
    // search stops being resolvable even though the checkout is perfectly ordinary.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-aliased-db");
    std::fs::create_dir_all(dir.join("out")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("out/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    std::os::unix::fs::symlink(dir.join("out"), dir.join("current-build")).unwrap();

    let layout = clangd.resolve_layout(&scope(&dir));
    let args = clangd.spawn_args(&layout);
    assert!(
        args.contains(&compdb_arg(&dir.join("out")))
            || args.contains(&compdb_arg(&dir.join("current-build"))),
        "either path reaches the one database, but the session must be pointed at it: {args:?}",
    );
    assert!(
        clangd.session_can_resolve(&scope(&dir), "src/main.c", &layout),
        "a source clangd could not find the database for is resolvable only because we pin it",
    );

    // A genuinely different second database still disqualifies pinning: aliases collapse,
    // projects do not.
    std::fs::create_dir_all(dir.join("other/build")).unwrap();
    std::fs::write(dir.join("other/build/compile_commands.json"), COMPDB).unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "two distinct databases are still two",
    );
    // Including when the second one is unusable — it is what clangd would load for its own
    // project's files, so pinning the working one would hand them the wrong flags.
    std::fs::write(dir.join("other/build/compile_commands.json"), "[]").unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "an unusable second database disqualifies pinning too",
    );
}

#[test]
fn a_nested_checkout_makes_the_layout_unprovable_rather_than_simply_smaller() {
    // A linked worktree or submodule kept INSIDE the checkout (this repo does it under
    // `.claude/worktrees/`) carries `.git` as a FILE, so excluding the NAME `.git` never sees it
    // and the walk counts the sibling's database as this checkout's.
    //
    // Not descending is only half the answer, and the other half is the one that matters: the
    // index walker DOES descend an ordinary directory whatever `.git` file it holds, so a
    // submodule's sources can be indexed here while its database is invisible to this scan. Pinning
    // the parent's database would then analyse those sources under unrelated defines and include
    // paths — a wrong definition, persisted. Whether a nested checkout is inside the indexed corpus
    // is not a question this crate can answer (#1008), so the scan reports itself INCOMPLETE and
    // pinning is declined. clangd's own per-file lookup takes over, which is correct by
    // construction.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-nested-checkout");
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("build/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();

    // Control FIRST: with no nested checkout the scan is complete, so this pins.
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        clangd.spawn_args(&layout).contains(&compdb_arg(&dir.join("build"))),
        "a checkout with one database and nothing hidden is pinned",
    );

    // Now add a nested checkout. Its database must not be counted as a second database of THIS
    // checkout — but its presence means the scan can no longer prove there is only one.
    std::fs::create_dir_all(dir.join("worktrees/feature/out")).unwrap();
    std::fs::write(
        dir.join("worktrees/feature/.git"),
        "gitdir: /elsewhere/.git/worktrees/feature\n",
    )
    .unwrap();
    std::fs::write(dir.join("worktrees/feature/out/compile_commands.json"), COMPDB).unwrap();

    let layout = clangd.resolve_layout(&scope(&dir));
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "a hidden nested checkout makes global pinning unprovable, so it is declined",
    );
    // The file's own database is still in an ancestor `build/`, which clangd finds unaided — so
    // declining to pin costs this file nothing.
    assert!(
        clangd.session_can_resolve(&scope(&dir), "src/main.c", &layout),
        "clangd's own ancestor/build lookup still configures the file",
    );
}

#[test]
fn a_scan_that_could_not_look_everywhere_never_reports_a_sole_database() {
    // The general invariant behind the nested-checkout case: `--compile-commands-dir` is GLOBAL, so
    // "there is exactly one database" has to be a proof rather than an observation. Every way the
    // walk can stop early — the depth bound here — can hide the database that governs half the
    // sources, and pinning would then hand them another project's flags. A truncated scan therefore
    // yields no sole database, whatever it happened to find.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-deep-tree");
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::write(dir.join("build/compile_commands.json"), COMPDB).unwrap();

    // A shallow database alone pins.
    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        clangd.spawn_args(&layout).contains(&compdb_arg(&dir.join("build"))),
        "the control must pin, or this test proves nothing about truncation",
    );

    // A tree deeper than the search bound: the walk stops with subdirectories left unexplored, so
    // whether a second database exists down there is unknown — and unknown is not "exactly one".
    let deep: std::path::PathBuf =
        (0..40).fold(dir.join("nested"), |path, level| path.join(format!("l{level}")));
    std::fs::create_dir_all(&deep).unwrap();

    let layout = clangd.resolve_layout(&scope(&dir));
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "a scan that hit its depth bound cannot claim the database it found is the only one",
    );
}

/// The ancestor leg never climbs ABOVE the checkout, including when the index root is already the
/// checkout top — the common case, where there is nothing to climb at all.
#[test]
fn the_ancestor_leg_never_leaves_the_checkout() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-climb-stops-at-checkout");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    std::fs::write(checkout.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&checkout, "build/compile_commands.json", &["src/main.c"]);
    // A database OUTSIDE the checkout, on the path the climb would take if it did not stop.
    write_database(&dir, "compile_commands.json", &["src/main.c"]);
    let corpus = crate::test_support::PrefixCorpus::new(&checkout, &["src"]);
    let scope = super::CheckoutScope::resolve(&checkout, &corpus);

    assert_eq!(
        clangd.resolve_layout(&scope).sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(checkout).unwrap().join("build").as_path()),
        "a database above the checkout is not this checkout's, and must not disqualify the pin",
    );
}

/// The nested-checkout case against a REAL linked worktree, not a synthetic `.git` file.
///
/// This repo keeps its own linked worktrees under `.claude/worktrees/`, so a checkout containing
/// another checkout is the ordinary case here, not an exotic one. `git worktree add` writes `.git`
/// as a FILE, which is exactly what a name-based exclusion cannot see — and the sibling carries its
/// own database, which must never be adopted as this checkout's.
///
/// The main checkout and the linked worktree share one database in this repository's model, so the
/// scan's answer has to hold from either side: the main checkout declines to pin because it can no
/// longer prove there is one database, and the linked worktree resolves its OWN layout with its own
/// ceiling.
#[test]
fn a_real_linked_worktree_inside_the_checkout_is_not_this_checkouts_project() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-real-linked-worktree");
    let main = dir.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    rag_rat_base::test_git::run(&main, &["init"]);
    std::fs::write(main.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&main, "build/compile_commands.json", &["src/main.c"]);
    rag_rat_base::test_git::run(&main, &["add", "."]);
    rag_rat_base::test_git::run(&main, &["commit", "-m", "seed"]);
    let corpus = crate::test_support::PrefixCorpus::new(&main, &["src"]);

    // Control FIRST: with no nested checkout this pins, or the assertion below proves nothing.
    let scope = super::CheckoutScope::resolve(&main, &corpus);
    assert_eq!(
        clangd.resolve_layout(&scope).sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(&main).unwrap().join("build").as_path()),
        "one database and nothing nested ⇒ pinned",
    );

    // A linked worktree INSIDE the main checkout, carrying its own database.
    let linked = main.join(".claude/worktrees/feature");
    std::fs::create_dir_all(main.join(".claude/worktrees")).unwrap();
    rag_rat_base::test_git::run(&main, &[
        "worktree",
        "add",
        linked.to_str().unwrap(),
        "-b",
        "feature",
    ]);
    write_database(&linked, "build/compile_commands.json", &["src/main.c"]);

    let scope = super::CheckoutScope::resolve(&main, &corpus);
    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_none(),
        "a nested checkout's `.git` is a FILE — the scan can no longer prove this checkout has \
         exactly one database, so it declines to pin rather than adopting the sibling's",
    );

    // From the linked worktree's own side, the ceiling is that worktree — never the main checkout
    // it happens to sit inside.
    let linked_corpus = crate::test_support::PrefixCorpus::new(&linked, &["src"]);
    let linked_scope = super::CheckoutScope::resolve(&linked, &linked_corpus);
    assert_eq!(
        linked_scope.ceiling(),
        rag_rat_base::paths::canonicalize(&linked).unwrap(),
        "the linked worktree is its own checkout, not a subdirectory of main",
    );
    assert_eq!(
        clangd.resolve_layout(&linked_scope).sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(&linked).unwrap().join("build").as_path()),
        "and it resolves its own database, not main's",
    );
}

/// What the scan proves is now "every database that could GOVERN an indexed file", not "every
/// database under the root" — and both halves of that need pinning, or the redefinition lives only
/// in a comment.
#[test]
fn completeness_covers_the_ancestor_chain_and_ignores_sibling_subtrees() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-completeness-scope");
    let checkout = dir.join("repo");
    std::fs::create_dir_all(checkout.join("sub/src")).unwrap();
    std::fs::create_dir_all(checkout.join("elsewhere/deep")).unwrap();
    rag_rat_base::test_git::run(&checkout, &["init"]);
    std::fs::write(checkout.join("sub/src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&checkout, "sub/build/compile_commands.json", &["sub/src/main.c"]);
    let root = checkout.join("sub");
    let corpus = crate::test_support::PrefixCorpus::new(&root, &["src"]);

    // A database in a SIBLING subtree of the index root neither counts nor spoils the proof: no
    // indexed file lies under it, so it cannot be the one that governs half the sources.
    write_database(&checkout, "elsewhere/deep/compile_commands.json", &["elsewhere/deep/x.c"]);
    let scope = super::CheckoutScope::resolve(&root, &corpus);
    assert_eq!(
        clangd.resolve_layout(&scope).sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(&root).unwrap().join("build").as_path()),
        "a sibling subtree of the index root is outside the question being asked",
    );

    // One on the ANCESTOR chain is a different matter: it governs the indexed sources, so it is
    // found, counted, and the checkout therefore has two — which disqualifies pinning.
    write_database(&checkout, "compile_commands.json", &["sub/src/main.c"]);
    let scope = super::CheckoutScope::resolve(&root, &corpus);
    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_none(),
        "the ancestor chain is searched, so this checkout has two databases, not one",
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

#[cfg(unix)]
#[test]
fn the_marker_search_does_not_follow_a_symlink_out_of_the_checkout() {
    // Following directory symlinks is what makes a symlinked `build/` discoverable, but a link
    // pointing OUT of the checkout (`sdk -> /opt/sdk`, `external -> ..`) is not part of it. Walking
    // through one costs an unrelated tree's traversal while the maintenance pass holds the
    // repository write lock, and — worse — counts a database found out there as this checkout's,
    // which flips the pinning decision for files that have nothing to do with it.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_outside_guard, outside) = checkout("clangd-outside-tree");
    std::fs::create_dir_all(outside.join("vendor/build")).unwrap();
    std::fs::write(outside.join("vendor/build/compile_commands.json"), COMPDB).unwrap();

    let (_dir_guard, dir) = checkout("clangd-escaping-link");
    std::fs::create_dir_all(dir.join("build")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("build/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();
    // The escape: a link to a tree that holds its own database.
    std::os::unix::fs::symlink(outside.as_path(), dir.join("sdk")).unwrap();

    let layout = clangd.resolve_layout(&scope(&dir));
    assert!(
        clangd.spawn_args(&layout).contains(&compdb_arg(&dir.join("build"))),
        "the checkout has exactly one database; a link out of it must not make that two",
    );

    // Control: the same shape INSIDE the checkout is a second database and does disqualify
    // pinning — so the assertion above cannot pass by the walk simply never descending.
    std::fs::create_dir_all(dir.join("inside/build")).unwrap();
    std::fs::write(dir.join("inside/build/compile_commands.json"), COMPDB).unwrap();
    let layout = clangd.resolve_layout(&scope(&dir));
    assert_eq!(
        clangd.spawn_args(&layout),
        vec![OsString::from("--background-index")],
        "a second database inside the checkout still counts",
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_cycle_cannot_hang_the_marker_search() {
    // Following directory symlinks is what makes the case above work, and it is also what
    // makes a cycle possible. The search must terminate rather than recurse forever.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-symlink-cycle");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::os::unix::fs::symlink(dir.as_path(), dir.join("nested/loop")).unwrap();
    // Terminates; the checkout has no database, so it reports none.
    assert!(clangd.resolve_layout(&scope(&dir)).sole_marker_dir().is_none());
}

#[cfg(unix)]
#[test]
fn two_symlink_cycles_cannot_make_the_marker_search_explode() {
    // ONE cycle costs the depth bound in visits; TWO links pointing at the same ancestor make
    // the walk branch at every level, so the number of paths through them is exponential in
    // that bound — replaying this traversal without a visited set over symlink targets did not
    // finish in 60 seconds on a tree of this shape. `resolve_layout` runs on every spawn
    // attempt while the maintenance pass holds the repository write lock, so that is a wedged
    // watcher, not a slow scan.
    //
    // No database anywhere, deliberately: the search stops at two sites, and a database
    // reachable through the links would supply the second one and mask the explosion.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-two-symlink-cycles");
    std::os::unix::fs::symlink(dir.as_path(), dir.join("loop-a")).unwrap();
    std::os::unix::fs::symlink(dir.as_path(), dir.join("loop-b")).unwrap();

    // Run the search off-thread with a bounded wait, so a regression fails this test in
    // seconds instead of hanging the suite for as long as CI allows.
    let root = dir.as_path().to_path_buf();
    let (done, resolved) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pinned = clangd.resolve_layout(&scope(&root)).sole_marker_dir().map(Path::to_path_buf);
        let _ = done.send(pinned);
    });
    let pinned = resolved
        .recv_timeout(Duration::from_secs(20))
        .expect("the marker search must terminate with two symlink cycles present");
    assert_eq!(pinned, None, "the checkout holds no database, through the links or otherwise");
}

#[test]
fn a_hidden_build_under_dot_cache_is_still_a_database() {
    // Only clangd's OWN index is off-limits under `.cache` — excluding the whole subtree would
    // contradict supporting hidden build directories at all.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-dot-cache-build");
    std::fs::create_dir_all(dir.join(".cache/cmake-build")).unwrap();
    std::fs::create_dir_all(dir.join(".cache/clangd/index")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join(".cache/cmake-build/compile_commands.json"), COMPDB).unwrap();
    // clangd's own index directory must never be mistaken for a project of ours.
    std::fs::write(dir.join(".cache/clangd/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int m(void){return 0;}\n").unwrap();

    let layout = clangd.resolve_layout(&scope(&dir));
    assert_eq!(
        layout.sole_marker_dir(),
        Some(dir.join(".cache/cmake-build").as_path()),
        "the hidden build counts, and clangd's own index does not",
    );
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
fn a_hidden_build_directory_still_counts_as_a_compilation_database() {
    // A build directory may legitimately be hidden (`.build/`), and a database there is as
    // real as one in `build/`. The DOCUMENT search still skips dot-directories — those hold
    // tooling state, not this checkout's sources — so the two searches differ on purpose.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-hidden-build");
    std::fs::create_dir_all(dir.join(".build")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join(".build/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void){return 0;}\n").unwrap();

    assert!(
        clangd.checkout_can_signal_readiness(&scope(&dir), &clangd.resolve_layout(&scope(&dir)))
    );
    assert!(
        clangd
            .spawn_args(&clangd.resolve_layout(&scope(&dir)))
            .contains(&compdb_arg(&dir.join(".build"))),
    );
    // The warm-up document still comes from the visible tree.
    assert_eq!(
        clangd.warmup_document(&scope(&dir), &clangd.resolve_layout(&scope(&dir))),
        Some(dir.join("src/main.c"))
    );
}

#[test]
fn a_vendored_or_vcs_database_is_never_mistaken_for_the_checkouts_own() {
    // Counting a stray database would be worse than missing one: it would flip a working
    // single-database checkout into the multi-database mode and drop the flag that makes it
    // resolvable.
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-vendored-db");
    std::fs::create_dir_all(dir.join("node_modules/dep")).unwrap();
    std::fs::create_dir_all(dir.join(".git/weird")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("node_modules/dep/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join(".git/weird/compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("compile_commands.json"), COMPDB).unwrap();
    std::fs::write(dir.join("src/main.c"), "int main(void){return 0;}\n").unwrap();

    assert!(
        clangd
            .spawn_args(&clangd.resolve_layout(&scope(&dir)))
            .contains(&OsString::from(format!("--compile-commands-dir={}", dir.display()))),
        "the checkout's own database is still the single unambiguous one",
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

#[test]
fn a_backend_with_no_checkout_scoped_marker_gets_only_its_static_argv() {
    // The dynamic argument is clangd-shaped; the other backends must not acquire a stray flag
    // their server would reject.
    let (_dir_guard, dir) = checkout("static-argv");
    std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    assert_eq!(ts.spawn_args(&ts.resolve_layout(&scope(&dir))), vec![OsString::from("--stdio")]);
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    assert!(rust.spawn_args(&rust.resolve_layout(&scope(&dir))).is_empty());
    // And with no database anywhere, clangd gets no directory to point at either.
    let (_empty_guard, empty) = checkout("static-argv-empty");
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    assert_eq!(clangd.spawn_args(&clangd.resolve_layout(&scope(&empty))), vec![OsString::from(
        "--background-index"
    )],);
}

#[test]
fn a_typescript_project_still_has_to_enclose_its_documents() {
    // The other scope, asserted alongside so the two cannot be conflated: a tsconfig sibling
    // of the sources governs nothing, because tsserver resolves a file's project by walking UP
    // from the file.
    let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
    let (_dir_guard, dir) = checkout("ts-sibling-config");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::write(dir.join("src/main.ts"), "export const x = 1;\n").unwrap();
    std::fs::write(dir.join("config/tsconfig.json"), "{}").unwrap();
    assert!(
        !ts.open_signals_readiness(&scope(&dir), "src/main.ts", &ts.resolve_layout(&scope(&dir))),
        "a config in a SIBLING directory governs nothing under src/",
    );
    assert_eq!(
        ts.warmup_document(&scope(&dir), &ts.resolve_layout(&scope(&dir))),
        None,
        "and there is nothing to warm on"
    );
}

#[test]
fn rust_claims_only_rust_paths() {
    let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
    assert_eq!(rust.language_id_for("src/lib.rs"), "rust");
    assert!(rust.claims_path("src/lib.rs"));
    assert!(!rust.claims_path("src/main.ts"));
    assert!(!rust.claims_path("Cargo.toml"));
}
