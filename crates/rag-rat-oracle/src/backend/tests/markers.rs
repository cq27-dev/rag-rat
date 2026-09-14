//! The project-marker search: where it looks, the checkout ceiling it never crosses, and the
//! symlinks it follows or refuses.

use super::*;

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
    let scope = crate::backend::CheckoutScope::resolve(&root, &corpus);

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
    let scope = crate::backend::CheckoutScope::resolve(&dir, &corpus);

    assert_eq!(scope.ceiling(), scope.root(), "no checkout ⇒ no widening");
    assert_eq!(
        clangd.resolve_layout(&scope).sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(&dir).unwrap().as_path()),
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
    let scope = crate::backend::CheckoutScope::resolve(&checkout, &corpus);

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
    let scope = crate::backend::CheckoutScope::resolve(&main, &corpus);
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

    let scope = crate::backend::CheckoutScope::resolve(&main, &corpus);
    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_none(),
        "a nested checkout's `.git` is a FILE — the scan can no longer prove this checkout has \
         exactly one database, so it declines to pin rather than adopting the sibling's",
    );

    // From the linked worktree's own side, the ceiling is that worktree — never the main checkout
    // it happens to sit inside.
    let linked_corpus = crate::test_support::PrefixCorpus::new(&linked, &["src"]);
    let linked_scope = crate::backend::CheckoutScope::resolve(&linked, &linked_corpus);
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

/// Losing every compilation database is scoped to the checkout that lost it.
///
/// A linked worktree and its main checkout share one index in this repository's model, so an
/// absence read at repository scope would end the main checkout's session over a deletion next
/// door, and one read only at main would leave the linked worktree warming against a project it
/// no longer has. The scan answers per `CheckoutScope`, and this pins that from both sides.
///
/// The main checkout's own scan is INCOMPLETE here — a nested checkout's `.git` is a file it
/// cannot classify — which is the second half: an incomplete scan must never report an absence,
/// because it cannot prove there is nothing left to find.
#[test]
fn a_linked_worktree_losing_its_database_does_not_take_the_sibling_with_it() {
    let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
    let (_dir_guard, dir) = checkout("clangd-worktree-database-loss");
    let main = dir.join("main");
    std::fs::create_dir_all(main.join("src")).unwrap();
    rag_rat_base::test_git::run(&main, &["init"]);
    std::fs::write(main.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    write_database(&main, "build/compile_commands.json", &["src/main.c"]);
    rag_rat_base::test_git::run(&main, &["add", "."]);
    rag_rat_base::test_git::run(&main, &["commit", "-m", "seed"]);

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

    let main_corpus = crate::test_support::PrefixCorpus::new(&main, &["src"]);
    let linked_corpus = crate::test_support::PrefixCorpus::new(&linked, &["src"]);
    let main_layout =
        || clangd.resolve_layout(&crate::backend::CheckoutScope::resolve(&main, &main_corpus));
    let linked_layout =
        || clangd.resolve_layout(&crate::backend::CheckoutScope::resolve(&linked, &linked_corpus));

    // Control: with both databases present neither side reports an absence, or the assertions
    // below prove nothing.
    assert!(!main_layout().has_no_database(), "control: main holds a database");
    assert!(!linked_layout().has_no_database(), "control: so does the linked worktree");

    // The linked worktree loses its database. Its own layout is the one that says so.
    std::fs::remove_file(linked.join("build/compile_commands.json")).unwrap();
    assert!(
        linked_layout().has_no_database(),
        "the checkout that lost its database reports the absence, under its own ceiling",
    );
    let sibling = main_layout();
    assert!(
        !sibling.has_no_database(),
        "and the sibling, which still holds one, is not ended for a deletion next door",
    );
    assert!(!sibling.is_empty(), "its own database is still found");

    // The other direction: main's scan cannot classify the nested checkout, so it is incomplete
    // and must decline to claim an absence even once its own database is gone.
    std::fs::remove_file(main.join("build/compile_commands.json")).unwrap();
    assert!(
        !main_layout().has_no_database(),
        "an incomplete scan has not established there is nothing left to find, so it must not \
         report an absence that would tear a working session down",
    );
    assert!(
        linked_layout().has_no_database(),
        "while the linked worktree, whose own scan finished, still reports its own absence",
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
    let scope = crate::backend::CheckoutScope::resolve(&root, &corpus);
    assert_eq!(
        clangd.resolve_layout(&scope).sole_marker_dir(),
        Some(rag_rat_base::paths::canonicalize(&root).unwrap().join("build").as_path()),
        "a sibling subtree of the index root is outside the question being asked",
    );

    // One on the ANCESTOR chain is a different matter: it governs the indexed sources, so it is
    // found, counted, and the checkout therefore has two — which disqualifies pinning.
    write_database(&checkout, "compile_commands.json", &["sub/src/main.c"]);
    let scope = crate::backend::CheckoutScope::resolve(&root, &corpus);
    assert!(
        clangd.resolve_layout(&scope).sole_marker_dir().is_none(),
        "the ancestor chain is searched, so this checkout has two databases, not one",
    );
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
