use std::fs;

use super::*;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Compile scoping discovery to the whole root (the common case in these unit tests).
fn compile(root: &Path) -> IgnoreMatcher {
    IgnoreMatcher::compile(root, &[])
}

/// Initialize a real (empty) Git repo at `dir` so worktree-root resolution returns `dir`. Used
/// by the ancestor-chain test; the others don't need git (base falls back to `root`).
fn git_init(dir: &Path) {
    rag_rat_base::test_git::run(dir, &["init", "-q"]);
}

#[test]
fn floor_dirs_ignored_without_any_gitignore() {
    let (_scratch, tmp) = tempdir();
    let m = compile(&tmp);
    assert!(m.is_ignored(&tmp.join("target"), true));
    assert!(m.is_ignored(&tmp.join("target/debug/foo.rs"), false));
    assert!(m.is_ignored(&tmp.join(".rag-rat"), true));
    assert!(m.is_ignored(&tmp.join(".claude/settings.json"), false));
    assert!(m.is_ignored(&tmp.join(".codex/config.toml"), false));
    assert!(m.is_ignored(&tmp.join("node_modules/pkg/index.ts"), false));
    assert!(m.is_ignored(&tmp.join(".build/checkouts/Dep/Sources/Dep.swift"), false));
    assert!(!m.is_ignored(&tmp.join("src/lib.rs"), false));
}

#[test]
fn the_clangd_index_floor_holds_in_a_real_linked_worktree() {
    // A real `git worktree add` checkout, not merely a nested directory: a linked worktree has
    // its own root and a `.git` FILE rather than a directory, and the live oracle runs per
    // checkout — each spawning its own clangd, each writing its own `.cache/clangd`. The floor
    // is applied relative to whichever checkout is being indexed, so neither disturbs the
    // other's sources.
    //
    // This is a MATCHER-level check on two independently compiled matchers; it says nothing
    // about the rows two checkouts sharing ONE database actually persist. That is
    // `schema_bootstrap_tests::worktree_overlay::visibility::
    // clangd_index_floor_holds_for_both_checkouts_sharing_one_database`, which indexes both
    // checkouts into one database and asserts on the stored file rows.
    let (_scratch, main) = tempdir();
    git_init(&main);
    write(&main.join("src/lib.c"), "int a(void){return 0;}\n");
    rag_rat_base::test_git::run(&main, &["add", "-A"]);
    rag_rat_base::test_git::run(&main, &[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-qm",
        "seed",
    ]);
    // Derived from this scratch directory's own unique name: a fixed name in the shared
    // scratch root collides between repeated or concurrent runs, and `git worktree add`
    // fails on an existing path.
    let linked = main
        .parent()
        .expect("scratch parent")
        .join(format!("{}-linked", main.file_name().expect("scratch name").to_string_lossy(),));
    rag_rat_base::test_git::run(&main, &[
        "worktree",
        "add",
        "-q",
        "-b",
        "wt",
        &linked.to_string_lossy(),
    ]);
    assert!(linked.join(".git").is_file(), "a linked worktree carries a .git FILE");

    let linked_matcher = IgnoreMatcher::compile(&linked, &[PathBuf::from(".")]);
    assert!(linked_matcher.is_ignored(&linked.join(".cache/clangd/index/a.idx"), false));
    assert!(
        !linked_matcher.is_ignored(&linked.join("src/lib.c"), false),
        "the linked checkout's own sources still index",
    );
    assert!(
        !linked_matcher.is_ignored(&linked.join(".cache/cmake-build/gen.c"), false),
        "and the narrow floor does not swallow the rest of a tracked .cache",
    );

    // The main checkout floors its OWN index, and its sources are untouched by the sibling's
    // presence — the active-checkout/sibling separation this topology exists to check.
    let main_matcher = IgnoreMatcher::compile(&main, &[PathBuf::from(".")]);
    assert!(main_matcher.is_ignored(&main.join(".cache/clangd/index/a.idx"), false));
    assert!(!main_matcher.is_ignored(&main.join("src/lib.c"), false));
    assert!(
        !main_matcher.is_ignored(&linked.join("src/lib.c"), false),
        "a sibling checkout outside this root is not governed by its matcher at all",
    );

    rag_rat_base::test_git::run(&main, &[
        "worktree",
        "remove",
        "--force",
        &linked.to_string_lossy(),
    ]);
}

#[test]
fn clangds_own_index_is_floored_without_swallowing_every_dot_cache() {
    // The live clangd oracle makes the checkout's OWN tooling write here: clangd persists its
    // background index to `.cache/clangd/index/` and no flag relocates it. That tree is large
    // and entirely machine-written — the same category as `.rag-rat` — so it is kept out of the
    // discovery walk, and nothing source-shaped appearing under it is indexed as first-party
    // code.
    let (_scratch, tmp) = tempdir();
    let m = compile(&tmp);
    assert!(m.is_ignored(&tmp.join(".cache/clangd"), true));
    assert!(m.is_ignored(&tmp.join(".cache/clangd/index/main.c.ABC123.idx"), false));
    // …but the floor is unconditional and cannot be whitelisted back, so it must NOT swallow a
    // `.cache/` a repo genuinely tracks, nor a nested one it happens to own.
    assert!(!m.is_ignored(&tmp.join(".cache"), true));
    assert!(!m.is_ignored(&tmp.join(".cache/generated/api.ts"), false));
    assert!(!m.is_ignored(&tmp.join("src/.cache/fixtures/sample.c"), false));
    // The floor is a consecutive-component match, so a same-named pair deeper in the tree is
    // floored too (a nested checkout's clangd index), while `clangd` alone never is.
    assert!(m.is_ignored(&tmp.join("vendor/dep/.cache/clangd/index/a.idx"), false));
    assert!(!m.is_ignored(&tmp.join("src/clangd/wrapper.c"), false));
}

#[test]
fn floor_path_run_restarts_on_a_repeated_first_component() {
    let (_scratch, tmp) = tempdir();
    let m = compile(&tmp);
    // `.cache/.cache/clangd`: the second `.cache` cannot EXTEND the run the first started (the
    // floor's second element is `clangd`), but it must start a fresh run that `clangd` then
    // completes — otherwise a nested `.cache` above the index tree would un-floor it.
    assert!(m.is_ignored(&tmp.join(".cache/.cache/clangd/index/a.idx"), false));
    // The restart is still a consecutive-run match, not a "saw both names" match.
    assert!(!m.is_ignored(&tmp.join(".cache/.cache/generated/api.ts"), false));
}

// A component that is not valid UTF-8 matches no floor entry, and must BREAK the floor path's
// run of consecutive components: `.cache/<non-utf8>/clangd` is a tracked tree that merely
// happens to sit between the two floor names, and the floor cannot be whitelisted back.
#[cfg(unix)]
#[test]
fn floor_path_run_is_broken_by_a_non_utf8_component() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let (_scratch, tmp) = tempdir();
    let m = compile(&tmp);
    let non_utf8 = OsStr::from_bytes(b"\xff\xfe");
    let between = tmp.join(".cache").join(non_utf8).join("clangd/src.c");
    assert!(
        !m.is_ignored(&between, false),
        "a non-UTF-8 component between the floor names must not be spliced away",
    );
    // Control: the same path WITHOUT the intervening component is floored, so the assertion
    // above is about the broken run and not about the path escaping the floor some other way.
    assert!(m.is_ignored(&tmp.join(".cache/clangd/src.c"), false));
}

#[test]
fn python_venv_floor_dirs_ignored_but_generic_env_indexed() {
    let (_scratch, tmp) = tempdir();
    let m = compile(&tmp);
    // Dotted tooling trees can never be first-party import packages, so they're floored even
    // with no .gitignore (#181).
    assert!(m.is_ignored(&tmp.join(".tox/py311/lib/foo.py"), false));
    assert!(m.is_ignored(&tmp.join(".nox/session/foo.py"), false));
    // site-packages stays floored regardless of the enclosing venv dir's name.
    assert!(m.is_ignored(&tmp.join("env/lib/python3.11/site-packages/dep.py"), false));
    // …but bare names that CAN be a real import package are NOT floored globally: `virtualenv`
    // is itself a PyPI package (`src/virtualenv/…` is first-party, #181 review), and
    // `env`/`.env` are too generic. Their own non-site-packages files stay indexable.
    assert!(!m.is_ignored(&tmp.join("src/virtualenv/__init__.py"), false));
    assert!(!m.is_ignored(&tmp.join("env/settings.py"), false));
    assert!(!m.is_ignored(&tmp.join(".env/config.py"), false));
}

#[test]
fn root_gitignore_is_honored() {
    let (_scratch, tmp) = tempdir();
    write(&tmp.join(".gitignore"), "generated/\n*.bak\n");
    write(&tmp.join("src/lib.rs"), "fn a() {}\n");
    write(&tmp.join("generated/out.rs"), "fn b() {}\n");
    write(&tmp.join("src/old.bak"), "x\n");
    let m = compile(&tmp);
    assert!(m.is_ignored(&tmp.join("generated"), true), "gitignored dir");
    assert!(m.is_ignored(&tmp.join("generated/out.rs"), false), "file under gitignored dir");
    assert!(m.is_ignored(&tmp.join("src/old.bak"), false), "gitignored glob");
    assert!(!m.is_ignored(&tmp.join("src/lib.rs"), false), "non-ignored source");
}

#[test]
fn nested_gitignore_scopes_to_its_subtree() {
    let (_scratch, tmp) = tempdir();
    // A nested gitignore ignores `vendor.rs` only under `sub/`, not at the root.
    write(&tmp.join("sub/.gitignore"), "vendor.rs\n");
    write(&tmp.join("sub/vendor.rs"), "x\n");
    write(&tmp.join("vendor.rs"), "x\n");
    let m = compile(&tmp);
    assert!(m.is_ignored(&tmp.join("sub/vendor.rs"), false), "nested rule applies in subtree");
    assert!(!m.is_ignored(&tmp.join("vendor.rs"), false), "nested rule does NOT leak to the root",);
}

#[test]
fn whitelist_negation_unignores() {
    let (_scratch, tmp) = tempdir();
    write(&tmp.join(".gitignore"), "build/\n!build/keep.rs\n");
    write(&tmp.join("build/keep.rs"), "x\n");
    write(&tmp.join("build/drop.rs"), "x\n");
    let m = compile(&tmp);
    // NOTE: `build` is also a FLOOR dir, so the floor wins regardless of negation — assert that
    // the floor is unconditional. (A non-floor whitelisted dir is covered by the next test.)
    assert!(m.is_ignored(&tmp.join("build/keep.rs"), false), "floor dir beats gitignore negation");
}

#[test]
fn negation_unignores_whitelisted_dir_subtree() {
    // git can re-include a file under a directory ONLY if the directory itself is whitelisted.
    // `gen/` ignored + `!gen/` re-included + `gen/skip/` re-ignored: `gen/a.rs` is back,
    // `gen/skip/b.rs` is out.
    let (_scratch, tmp) = tempdir();
    write(&tmp.join(".gitignore"), "gen/\n!gen/\ngen/skip/\n");
    write(&tmp.join("gen/a.rs"), "x\n");
    write(&tmp.join("gen/skip/b.rs"), "x\n");
    let m = compile(&tmp);
    assert!(!m.is_ignored(&tmp.join("gen/a.rs"), false), "re-included dir's file un-ignored");
    assert!(m.is_ignored(&tmp.join("gen/skip/b.rs"), false), "re-ignored subdir still out");
}

#[test]
fn nested_negation_under_ignored_parent_stays_ignored() {
    // FINDING 2: a nested `.gitignore` inside a directory ignored by an OUTER rule must NOT
    // re-include files. Git stops descending at the excluded `gen/`, so `gen/.gitignore`'s
    // `!keep.rs` is never consulted — `gen/keep.rs` stays ignored.
    let (_scratch, tmp) = tempdir();
    write(&tmp.join(".gitignore"), "gen/\n");
    write(&tmp.join("gen/.gitignore"), "!keep.rs\n");
    write(&tmp.join("gen/keep.rs"), "x\n");
    write(&tmp.join("gen/drop.rs"), "x\n");
    let m = compile(&tmp);
    assert!(
        m.is_ignored(&tmp.join("gen/keep.rs"), false),
        "nested negation under an ignored parent must NOT re-include",
    );
    assert!(m.is_ignored(&tmp.join("gen/drop.rs"), false), "sibling under ignored parent out");
}

#[test]
fn flat_negation_unignores_non_floor_file() {
    // Distinct from the nested case: a SINGLE gitignore with `gen/` + `!gen/keep.rs` does NOT
    // re-include either, because git can't reach inside an excluded directory even from the
    // same file. Both stay ignored. (This is the git-correct behavior.)
    let (_scratch, tmp) = tempdir();
    write(&tmp.join(".gitignore"), "gen/\n!gen/keep.rs\n");
    write(&tmp.join("gen/keep.rs"), "x\n");
    write(&tmp.join("gen/drop.rs"), "x\n");
    let m = compile(&tmp);
    assert!(m.is_ignored(&tmp.join("gen/keep.rs"), false), "no reinclude under excluded dir");
    assert!(m.is_ignored(&tmp.join("gen/drop.rs"), false), "sibling still ignored");
}

#[test]
fn repo_root_under_floor_named_ancestor_still_indexes() {
    // FINDING 1: the repo lives under a directory named like a floor entry (`build`). The floor
    // check must run on the path RELATIVE to the root, never on the absolute ancestors — so the
    // repo's own files are still indexed.
    let (_scratch, outer_base) = tempdir();
    let outer = outer_base.join("build");
    let root = outer.join("my-repo");
    write(&root.join("src/lib.rs"), "fn a() {}\n");
    write(&root.join("target/debug/built.rs"), "fn b() {}\n");
    let m = compile(&root);
    assert!(
        !m.is_ignored(&root.join("src/lib.rs"), false),
        "repo under a floor-named ancestor still indexes its files",
    );
    // The repo's OWN `target/` (a relative floor component) is still skipped.
    assert!(m.is_ignored(&root.join("target/debug/built.rs"), false), "in-repo floor skipped");
    // A path outside the repo root is simply not governed (not ignored) by our rules.
    assert!(!m.is_ignored(&outer.join("sibling.rs"), false), "outside-root path not governed");
}

#[test]
fn worktree_root_gitignore_governs_subdirectory_config_root() {
    // FINDING 3 (round 3): `config.root` is a subdirectory (`crates`) of a larger Git worktree.
    // The worktree-root `.gitignore` rules Git would apply to paths under that subdirectory
    // must be honored — files Git ignores must not be indexed.
    let (_scratch, wt) = tempdir();
    git_init(&wt);
    // Worktree-root .gitignore ignores every `*.gen.rs` and the `vendored/` dir, repo-wide.
    write(&wt.join(".gitignore"), "*.gen.rs\nvendored/\n");
    let sub = wt.join("crates");
    write(&sub.join("lib.rs"), "fn a() {}\n");
    write(&sub.join("schema.gen.rs"), "fn g() {}\n");
    write(&sub.join("vendored/dep.rs"), "fn v() {}\n");

    // Compile with `config.root = crates` — the ancestor chain must pull in the worktree-root
    // `.gitignore` even though it sits ABOVE config.root.
    let m = IgnoreMatcher::compile(&sub, &[PathBuf::from(".")]);
    assert!(!m.is_ignored(&sub.join("lib.rs"), false), "normal source under subdir indexes");
    assert!(
        m.is_ignored(&sub.join("schema.gen.rs"), false),
        "worktree-root glob applies under the subdir config.root (finding 3)",
    );
    assert!(
        m.is_ignored(&sub.join("vendored/dep.rs"), false),
        "worktree-root dir rule applies under the subdir config.root (finding 3)",
    );
}

#[test]
fn discovery_does_not_walk_unignored_sibling_outside_targets() {
    // ROUND 3 (scoping): a large unignored sibling dir OUTSIDE the configured target trees must
    // not be scanned for nested `.gitignore`s. We assert by behavior: a nested `.gitignore` in
    // the sibling is never compiled, so it has no effect on classification — and, conversely, a
    // nested `.gitignore` INSIDE the target tree IS picked up.
    let (_scratch, root) = tempdir();
    // Target tree: `src`. Sibling tree: `huge` (not a target).
    write(&root.join("src/.gitignore"), "skip.rs\n");
    write(&root.join("src/skip.rs"), "x\n");
    // Sibling's nested gitignore would, if scanned, ignore `marker.rs`. Scoped discovery must
    // NOT read it, so `marker.rs` stays unignored.
    write(&root.join("huge/.gitignore"), "marker.rs\n");
    write(&root.join("huge/marker.rs"), "x\n");

    let m = IgnoreMatcher::compile(&root, &[PathBuf::from("src")]);
    // In-target nested gitignore is honored.
    assert!(m.is_ignored(&root.join("src/skip.rs"), false), "in-target nested gitignore honored");
    // Out-of-target sibling's nested gitignore was never compiled → no effect.
    assert!(
        !m.is_ignored(&root.join("huge/marker.rs"), false),
        "sibling outside target trees is not scanned for nested gitignores (scoping)",
    );
}

#[test]
fn target_ancestor_gitignore_governs_nested_target_without_scanning_siblings() {
    let (_scratch, root) = tempdir();
    write(&root.join("src/.gitignore"), "generated/\n");
    write(&root.join("src/generated/lib.rs"), "x\n");
    write(&root.join("src/sibling/.gitignore"), "marker.rs\n");
    write(&root.join("src/sibling/marker.rs"), "x\n");

    let m = IgnoreMatcher::compile(&root, &[PathBuf::from("src/generated")]);
    assert!(
        m.is_ignored(&root.join("src/generated/lib.rs"), false),
        "target ancestor .gitignore must govern nested target files",
    );
    assert!(
        !m.is_ignored(&root.join("src/sibling/marker.rs"), false),
        "target ancestor compilation must not recursively scan unindexed siblings",
    );
}

fn tempdir() -> (rag_rat_base::test_scratch::ScratchDir, std::path::PathBuf) {
    let guard = rag_rat_base::test_scratch::ScratchDir::new("ignore");
    // Canonicalize so the absolute paths we build match what worktree-root resolution returns
    // (macOS /tmp is a symlink to /private/tmp; git reports the canonical form). The guard
    // still removes the directory: the canonical form is the same inode.
    let root = rag_rat_base::paths::canonicalize_or_simplified(guard.path());
    (guard, root)
}

// base_under_worktree must return the worktree root in `root`'s OWN representation even when a
// symlinked segment makes `root`'s component count differ from its canonical form — a depth
// count derived from the canonical paths would misindex `root.ancestors()` and return a wrong
// base (the parent of the worktree root, here), breaking the strip_prefix contract.
#[cfg(unix)]
#[test]
fn base_under_worktree_handles_symlinked_root_segment() {
    use std::os::unix::fs::symlink;
    let (_scratch, wt) = tempdir(); // canonical worktree root
    fs::create_dir_all(wt.join("a/b")).unwrap();
    // `<wt>/link` (1 component) resolves to `<wt>/a/b` (2 components) — counts differ.
    let link = wt.join("link");
    symlink(wt.join("a/b"), &link).unwrap();
    // `root` (= the symlink) is NOT canonicalized — the case the helper must tolerate; `wt` is
    // a genuine textual ancestor of it.
    let base = base_under_worktree(&link, &wt);
    assert_eq!(base.as_deref(), Some(wt.as_path()), "base must be wt in root's representation");
    assert!(link.strip_prefix(base.unwrap()).is_ok(), "base must stay a textual prefix of root");
}
