//! `index --paths` — the explicit-path scoped reconcile (#659). Reconciles exactly the supplied
//! candidate paths (content-hash decides staleness), leaving everything else untouched; ignored /
//! out-of-target / unchanged paths are no-ops; a supplied path that no longer exists is tombstoned;
//! and a first-time-empty repo defers (#427) exactly like the other modes.

use super::*;

/// Whether a symbol with `name` is currently indexed. POSITIVE queries only — `symbol_candidates`
/// lazily heals a zero-hit, so a test that asserted a symbol's ABSENCE could self-heal it into
/// existence and pass vacuously. Every assertion below checks a symbol that SHOULD be present.
fn symbol_present(db: &IndexDatabase, name: &str) -> bool {
    let selector = rag_rat_query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some(name.to_string()),
        language: None,
        allow_ambiguous: true,
        limit: 10,
    };
    !db.symbol_candidates(&selector, false).unwrap().candidates.is_empty()
}

#[test]
fn index_paths_reconciles_only_the_supplied_path() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn beta_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap(), 2);

    // Edit the supplied file AND drop a brand-new target file that is NOT supplied. Reconciling
    // only a.rs must re-index it while leaving c.rs undiscovered — proving the pass touches
    // EXACTLY the supplied set, not "whatever is on disk". (Heal-proof: `indexed_file_count` is
    // a plain count, and the alpha_v2 lookup is a positive hit on a now-clean file, so no
    // lazy/stale heal fires.)
    fs::write(root.join("src/a.rs"), "pub fn alpha_v2() {}\n").unwrap();
    fs::write(root.join("src/c.rs"), "pub fn gamma_v1() {}\n").unwrap();
    let db = IndexDatabase::index_paths(&config, &[root.join("src/a.rs")]).unwrap();

    assert!(symbol_present(&db, "alpha_v2"), "the supplied path was reconciled to its new content");
    assert_eq!(
        db.indexed_file_count().unwrap(),
        2,
        "a new file that was NOT supplied is not indexed — only the supplied path is touched",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_is_a_noop_for_ignored_out_of_target_and_unchanged_paths() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    // `secret.rs` is gitignored (the `ignore` crate honors `.gitignore` without a git repo), so the
    // rebuild never indexes it; `README.md` is outside the `src` target.
    fs::write(root.join(".gitignore"), "secret.rs\n").unwrap();
    fs::write(root.join("src/secret.rs"), "pub fn secret_v1() {}\n").unwrap();
    fs::write(root.join("README.md"), "# readme\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    let files_before = IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap();

    // Supply an unchanged target file, an ignored file, and an out-of-target file — all no-ops.
    let db = IndexDatabase::index_paths(&config, &[
        root.join("src/a.rs"),      // unchanged
        root.join("src/secret.rs"), // ignored
        root.join("README.md"),     // out of target
    ])
    .unwrap();

    assert_eq!(
        db.indexed_file_count().unwrap(),
        files_before,
        "ignored / out-of-target / unchanged paths add nothing",
    );
    assert!(symbol_present(&db, "alpha_v1"), "the unchanged target file is intact");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_tombstones_a_supplied_path_that_no_longer_exists() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn beta_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap(), 2);

    // Delete b.rs on disk, then supply it — a vanished path is tombstoned.
    fs::remove_file(root.join("src/b.rs")).unwrap();
    let db = IndexDatabase::index_paths(&config, &[root.join("src/b.rs")]).unwrap();

    assert_eq!(db.indexed_file_count().unwrap(), 1, "the deleted supplied path is tombstoned");
    assert!(symbol_present(&db, "alpha_v1"), "the un-supplied file is untouched by the tombstone");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reindex_paths_routes_a_linked_worktree_path_to_its_overlay_not_the_base() {
    // The routing decision (#659): a path under the base checkout stays a base path; a path under a
    // LINKED worktree routes to that checkout's overlay. This is load-bearing — `config.root` is
    // the main checkout, so a base pass would `strip_prefix` the linked path away and silently
    // drop the edit. White-boxes the partition the `reindex_paths` orchestration drives.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_marker() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn branch_marker() {}\n").unwrap();

    let partition = crate::watch::partition_paths_by_worktree(&config, &[
        main.join("src/a.rs"),
        linked.join("src/a.rs"),
    ]);
    assert_eq!(
        partition.base_paths,
        vec![main.join("src/a.rs")],
        "only the base-checkout path is a base path — the linked path must NOT fall through to \
         the base pass (which would drop it)",
    );
    assert_eq!(partition.linked.len(), 1, "the linked path routes to exactly its checkout overlay",);
    let (root, root_paths) = partition.linked.iter().next().unwrap();
    assert!(
        rag_rat_base::paths::canonicalize(&linked).is_ok_and(|canonical| *root == canonical),
        "the routed root is the canonical linked checkout: {root:?}",
    );
    assert_eq!(
        root_paths,
        &vec![linked.join("src/a.rs")],
        "the checkout's supplied paths are bucketed under it for a path-scoped overlay pass (#679)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #679: a linked-worktree `index --paths` reconciles EXACTLY the supplied path's overlay row — a
/// single-file edit does NOT pull in the OTHER in-flight changes in the same worktree (path-scoped,
/// mirroring the base `Paths` semantics on the linked route). Row-count assertions only: no
/// `symbol_present`, which lazily heals a zero-hit and would mask the b.rs absence.
#[test]
fn reindex_paths_scopes_a_linked_edit_to_just_the_supplied_path() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_base() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn b_base() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // BOTH files are dirty in the linked checkout, but only a.rs is supplied.
    fs::write(linked.join("src/a.rs"), "pub fn a_branch() {}\n").unwrap();
    fs::write(linked.join("src/b.rs"), "pub fn b_branch() {}\n").unwrap();

    crate::watch::reindex_paths(&config, &[linked.join("src/a.rs")], |_| {}).unwrap();

    assert_eq!(
        overlay_rows(&config, "src/a.rs"),
        1,
        "the supplied linked path gets its overlay row (its edit is reflected)",
    );
    assert_eq!(
        overlay_rows(&config, "src/b.rs"),
        0,
        "the OTHER in-flight linked edit is NOT pulled in — the pass is path-scoped (#679)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #679: a DELETED supplied linked path tombstones exactly that path's overlay row (shadowing the
/// base row), still without touching the other in-flight change.
#[test]
fn reindex_paths_tombstones_only_the_supplied_deleted_linked_path() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_base() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn b_base() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::remove_file(linked.join("src/a.rs")).unwrap(); // delete the supplied path
    fs::write(linked.join("src/b.rs"), "pub fn b_branch() {}\n").unwrap(); // other dirty edit, not supplied

    crate::watch::reindex_paths(&config, &[linked.join("src/a.rs")], |_| {}).unwrap();

    assert_eq!(
        deleted_overlay_rows(&config, "src/a.rs"),
        1,
        "the deleted supplied path is tombstoned in the overlay (shadows the base row)",
    );
    assert_eq!(
        overlay_rows(&config, "src/b.rs"),
        0,
        "the other in-flight linked edit is untouched — still path-scoped (#679)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #679 review: a supplied linked path the branch `.gitignore` IGNORES must not be indexed into the
/// overlay — the path-scoped route honors the linked checkout's ignore rules, exactly like the base
/// walker and the whole-delta overlay (else `index --paths` could create an overlay row for a file
/// discovery would never index).
#[test]
fn reindex_paths_does_not_index_an_ignored_linked_path() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_base() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch ignores `gen.rs`; the file sits in a target dir but is gitignored.
    fs::write(linked.join(".gitignore"), "gen.rs\n").unwrap();
    fs::write(linked.join("src/gen.rs"), "pub fn generated() {}\n").unwrap();

    crate::watch::reindex_paths(&config, &[linked.join("src/gen.rs")], |_| {}).unwrap();

    assert_eq!(
        overlay_rows(&config, "src/gen.rs"),
        0,
        "an ignored linked path is not indexed into the overlay (#679 review)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #679 review: a supplied linked path that CROSSES A SYMLINK (the leaf is a symlink) must not be
/// indexed — the walker skips symlink entries, so indexing one would write an overlay row under a
/// spelling a full/discover pass never produces (and could read content outside the checkout).
#[cfg(unix)]
#[test]
fn reindex_paths_does_not_index_a_symlinked_linked_path() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_base() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/real.rs"), "pub fn real_target() {}\n").unwrap();
    std::os::unix::fs::symlink(linked.join("src/real.rs"), linked.join("src/link.rs")).unwrap();

    crate::watch::reindex_paths(&config, &[linked.join("src/link.rs")], |_| {}).unwrap();

    assert_eq!(
        overlay_rows(&config, "src/link.rs"),
        0,
        "a symlink-crossing linked path is not indexed into the overlay (#679 review)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #1027: the path-scoped linked overlay rebases a supplied path spelled through a DIFFERENT
/// symlink than the resolved source root (macOS `/var` vs `/private/var`, a symlinked editor
/// `$PWD`, a watcher event carrying the watched spelling) — and that rebase must keep the LEAF
/// verbatim. A branch-only file REPLACED by a symlink still has to lose its stale overlay row;
/// resolving the leaf hands the pass the link's TARGET instead, so the replaced file is never
/// classified at all and its row survives indefinitely (this pass skips the global prune).
#[cfg(unix)]
#[test]
fn overlay_paths_rebases_a_symlink_replaced_branch_file_without_following_its_leaf() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_base() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Two BRANCH-ONLY files (absent from base), so each carries an overlay row with no base row
    // behind it — the removal branch, not the tombstone branch.
    fs::write(linked.join("src/branch.rs"), "pub fn branch_fn() {}\n").unwrap();
    fs::write(linked.join("src/other.rs"), "pub fn other_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        overlay_rows(&config, "src/branch.rs"),
        1,
        "the branch-only file is overlay-indexed"
    );

    // Replace the branch-only file with a symlink to its sibling, then supply it through an ALIAS
    // spelling of the worktree — the second spelling of one directory that macOS and Windows hand
    // over for free.
    fs::remove_file(linked.join("src/branch.rs")).unwrap();
    std::os::unix::fs::symlink(linked.join("src/other.rs"), linked.join("src/branch.rs")).unwrap();
    let alias = unique_temp_root();
    let _ = fs::remove_dir_all(&alias);
    std::os::unix::fs::symlink(linked.as_path(), alias.as_path()).unwrap();

    db.index_worktree_overlay_paths(
        &config,
        &linked,
        &[alias.join("src/branch.rs")],
        crate::index::OverlayLogicalRebuild::Inline,
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(
        overlay_rows(&config, "src/branch.rs"),
        0,
        "the symlink-replaced branch-only file's stale overlay row is removed, not left behind",
    );
    assert_eq!(
        overlay_rows(&config, "src/other.rs"),
        1,
        "the link's target is untouched — it was never the supplied path",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #679 review: a BRANCH-ONLY file (absent from base) that was overlay-indexed and is then deleted
/// must have its stale overlay row REMOVED by a path-scoped pass — there is no base row to shadow
/// (so a tombstone is wrong) and this pass skips the global prune, so it must remove the row per
/// supplied path or the dead row + its symbols would linger indefinitely.
#[test]
fn reindex_paths_removes_a_stale_branch_only_overlay_row() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_base() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // A branch-only file (never in base), overlay-indexed by a scoped pass.
    fs::write(linked.join("src/only.rs"), "pub fn only_branch() {}\n").unwrap();
    crate::watch::reindex_paths(&config, &[linked.join("src/only.rs")], |_| {}).unwrap();
    assert_eq!(overlay_rows(&config, "src/only.rs"), 1, "the branch-only file is overlay-indexed");

    // Delete it and reindex the SAME path — the stale overlay row must be removed, not tombstoned.
    fs::remove_file(linked.join("src/only.rs")).unwrap();
    crate::watch::reindex_paths(&config, &[linked.join("src/only.rs")], |_| {}).unwrap();
    assert_eq!(
        overlay_rows(&config, "src/only.rs"),
        0,
        "the stale branch-only overlay row is removed"
    );
    assert_eq!(
        deleted_overlay_rows(&config, "src/only.rs"),
        0,
        "and NOT tombstoned — there is no base row to shadow (#679 review)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #679 review: a supplied linked `rag-rat.toml` edit routes to the WHOLE-delta overlay pass (not
/// the path-scoped one), so a branch target-config change is reconciled immediately — a file the
/// branch now targets (but the base never indexed) appears in the overlay. The path-scoped route
/// alone would no-op on the config file (not a source target) and leave the drift for a later
/// sweep.
#[test]
fn reindex_paths_reconciles_target_drift_on_a_linked_config_edit() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::create_dir_all(main.join("extra")).unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    fs::write(main.join("extra/tool.rs"), "pub fn tool_marker() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    // Base config targets ONLY `src`, so extra/tool.rs is never base-indexed.
    let base_config = source_config_dirs(main.clone(), Language::Rust, &["src"]);
    let _ = IndexDatabase::rebuild(&base_config).unwrap();
    assert_eq!(overlay_rows(&base_config, "extra/tool.rs"), 0, "extra/ is not a base target");

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch edits its config to ADD `extra` as a target (read on-disk by the overlay pass).
    fs::write(
        linked.join("rag-rat.toml"),
        "[index]\nroot = \".\"\ndatabase = \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = \
         [\"src\", \"extra\"]\n",
    )
    .unwrap();

    // Supply the CONFIG file — must route to the whole-delta drift reconcile, not the path-scoped
    // route (which would no-op on rag-rat.toml).
    crate::watch::reindex_paths(&base_config, &[linked.join("rag-rat.toml")], |_| {}).unwrap();

    assert!(
        overlay_rows(&base_config, "extra/tool.rs") >= 1,
        "a branch config edit that re-targets extra/ is reconciled in the overlay (#679 review)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #679 review: a supplied linked `.gitignore` edit routes to the WHOLE-delta overlay pass, so an
/// ignore-rule change is reconciled immediately — a base file the branch now ignores is tombstoned
/// in the overlay. The path-scoped route alone would no-op on `.gitignore` (not a source target)
/// and leave the flipped files stale until a later sweep.
#[test]
fn reindex_paths_reconciles_an_ignore_flip_on_a_linked_gitignore_edit() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_base() {}\n").unwrap();
    fs::write(main.join("src/gen.rs"), "pub fn gen_base() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch adds a `.gitignore` that now ignores the base-indexed gen.rs.
    fs::write(linked.join(".gitignore"), "gen.rs\n").unwrap();

    crate::watch::reindex_paths(&config, &[linked.join(".gitignore")], |_| {}).unwrap();

    assert!(
        deleted_overlay_rows(&config, "src/gen.rs") >= 1,
        "a base file the branch newly ignores is tombstoned in the overlay (#679 review)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn index_paths_rejects_a_path_that_escapes_the_repo_root() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    let before = IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap();

    // A file OUTSIDE the repo, reached by `..` traversal that STARTS with a configured target dir
    // (`src/`) so the lexical target filter would accept the spelling — the exact escape #659's
    // review flagged. `<root>/src/../../<sibling>/outside.rs` normalizes to the intruder, but the
    // `..` guard rejects it before anything outside `config.root` is read or tombstoned.
    let intruder_dir = root
        .parent()
        .unwrap()
        .join(format!("{}-intruder", root.file_name().unwrap().to_string_lossy()));
    fs::create_dir_all(&intruder_dir).unwrap();
    fs::write(intruder_dir.join("outside.rs"), "pub fn intruder() {}\n").unwrap();
    let escaping = root
        .join("src")
        .join("..")
        .join("..")
        .join(intruder_dir.file_name().unwrap())
        .join("outside.rs");
    assert!(escaping.is_file(), "the escaping spelling really does resolve to the outside file");

    let db = IndexDatabase::index_paths(&config, &[escaping]).unwrap();
    assert_eq!(
        db.indexed_file_count().unwrap(),
        before,
        "a path escaping the root via `..` is dropped — nothing outside config.root is indexed",
    );

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&intruder_dir);
}

#[test]
fn reindex_paths_routes_a_deleted_linked_path_whose_parent_dir_is_also_gone() {
    // A linked-worktree DELETION may remove the file AND its parent directory in one edit. Routing
    // must still send it to the linked overlay (to tombstone), which means canonicalizing the
    // NEAREST SURVIVING ancestor — not the immediate (now-absent) parent, whose lexical spelling
    // could mis-compare against the canonical checkout root and drop the tombstone (#659 review).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/keep.rs"), "pub fn keep() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // `linked/src/gone/` never existed on disk, so a deleted file under it has BOTH its file and
    // its immediate parent absent; only `linked/src` (and up) survive.
    let deleted = linked.join("src/gone/x.rs");
    assert!(!deleted.parent().unwrap().exists(), "the deleted file's parent is absent");

    let partition = crate::watch::partition_paths_by_worktree(&config, &[deleted]);
    assert!(
        partition.base_paths.is_empty(),
        "the deleted linked path must not fall through to the base pass"
    );
    assert_eq!(
        partition.linked.len(),
        1,
        "it routes to the linked overlay via the surviving ancestor",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// Count `main.files` rows for `path` matching a `commit_sha` predicate — `commit_sha = ''` is a
/// working-tree/overlay (dirty) row; `commit_sha != ''` is a committed-scope row.
fn file_rows(config: &Config, path: &str, commit_sha_pred: &str) -> i64 {
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    conn.query_row(
        &format!("SELECT COUNT(*) FROM main.files WHERE path = ?1 AND {commit_sha_pred}"),
        [path],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

/// Count non-deleted OVERLAY rows (`worktree_id != ''`) for `path` — a linked worktree's overlay
/// scope. Zero ⇒ the base row shows through (the path was not shadowed).
fn overlay_rows(config: &Config, path: &str) -> i64 {
    file_rows(config, path, "worktree_id != '' AND kind != 'deleted'")
}

/// Count OVERLAY tombstone rows (`worktree_id != ''`, `kind = 'deleted'`) for `path`.
fn deleted_overlay_rows(config: &Config, path: &str) -> i64 {
    file_rows(config, path, "worktree_id != '' AND kind = 'deleted'")
}

#[test]
fn index_paths_completes_the_base_scope_after_a_commit_advances_head() {
    // A commit advances HEAD but the committed rows are still keyed to the OLD sha, so the active
    // (new-HEAD) scope is incomplete. A scoped Paths pass over one file must NOT leave every
    // unchanged file orphaned at the old sha (queries would lose most of the repo) — it promotes to
    // a discovery that restamps the committed rows onto the new HEAD (#659 review, mirrors
    // Changed).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn beta() {}\n").unwrap();
    fs::write(root.join("src/c.rs"), "pub fn gamma() {}\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "A"]);
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap(); // indexed at commit A (3 committed files)

    // Advance HEAD: edit a.rs and commit. The committed rows are now stale (keyed to A).
    fs::write(root.join("src/a.rs"), "pub fn alpha_v2() {}\n").unwrap();
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "B"]);

    let db = IndexDatabase::index_paths(&config, &[root.join("src/a.rs")]).unwrap();
    assert_eq!(
        db.indexed_file_count().unwrap(),
        3,
        "a HEAD move promotes the scoped pass to complete the scope — b.rs and c.rs are NOT \
         orphaned",
    );
    assert!(symbol_present(&db, "alpha_v2"), "the committed edit is indexed at the new HEAD");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_keeps_a_clean_committed_file_in_the_committed_scope() {
    // Supplying a CLEAN committed file (scope complete, HEAD unchanged) must reindex it in the
    // COMMITTED scope, not shadow it with a working-tree overlay row —
    // `explicit_index_files_and_changes` cross-references git status so a non-dirty path is
    // left out of the dirty set (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "A"]);
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(
        file_rows(&config, "src/a.rs", "commit_sha = ''"),
        0,
        "clean baseline: no overlay row"
    );

    // a.rs is clean and committed; supplying it must not create a dirty overlay shadow.
    let _ = IndexDatabase::index_paths(&config, &[root.join("src/a.rs")]).unwrap();
    assert_eq!(
        file_rows(&config, "src/a.rs", "commit_sha = ''"),
        0,
        "a clean committed file must NOT gain a working-tree overlay row",
    );
    assert!(
        file_rows(&config, "src/a.rs", "commit_sha != ''") >= 1,
        "the clean file stays committed-scoped",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
#[cfg(unix)]
fn index_paths_rejects_a_path_through_a_symlink_that_escapes_the_root() {
    // A supplied path can also escape the checkout through an IN-REPO SYMLINK — `src/link/x.rs`
    // where `link` points OUTSIDE — which a lexical prefix/`..` check accepts but resolves outside.
    // The canonicalized containment check rejects it, so nothing external is read or indexed (#659
    // review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    let before = IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap();

    let outside = root
        .parent()
        .unwrap()
        .join(format!("{}-escape", root.file_name().unwrap().to_string_lossy()));
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("intruder.rs"), "pub fn intruder() {}\n").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("src/link")).unwrap();
    let escaping = root.join("src/link/intruder.rs");
    assert!(escaping.is_file(), "the symlink really does resolve to the external file");

    let db = IndexDatabase::index_paths(&config, &[escaping]).unwrap();
    assert_eq!(
        db.indexed_file_count().unwrap(),
        before,
        "a path escaping the root through a symlink indexes nothing outside the checkout",
    );

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}

/// The `id` of the (single) committed row for `path`, for churn assertions.
fn committed_row_id(config: &Config, path: &str) -> i64 {
    let conn = rusqlite::Connection::open(&config.database).unwrap();
    conn.query_row(
        "SELECT id FROM main.files WHERE path = ?1 AND commit_sha != '' LIMIT 1",
        [path],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

#[test]
fn index_paths_does_not_churn_an_unchanged_files_row() {
    // Reconciling a CLEAN/unchanged supplied file must be a true no-op — not a remove+reinsert that
    // churns the row id and cascade-drops its chunk embeddings (#659 review). The row id is stable.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "A"]);
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    let id_before = committed_row_id(&config, "src/a.rs");

    // a.rs is clean; reconciling it changes nothing.
    let _ = IndexDatabase::index_paths(&config, &[root.join("src/a.rs")]).unwrap();
    assert_eq!(
        committed_row_id(&config, "src/a.rs"),
        id_before,
        "an unchanged supplied file keeps its row id — no remove+reinsert churn",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_normalizes_internal_parent_dir_components() {
    // A supplied path with an INTERNAL `..` must be normalized before target/dirty/persistence use:
    // `src/../src/a.rs` collapses to `src/a.rs` (reindexed under the real key, not a duplicate),
    // and `src/../outside.rs` collapses to `outside.rs` and is dropped as out-of-target (#659
    // review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    fs::write(root.join("outside.rs"), "pub fn outsider() {}\n").unwrap(); // root-level, out of `src`
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    let files_before = IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap();

    fs::write(root.join("src/a.rs"), "pub fn alpha_v2() {}\n").unwrap();
    let db = IndexDatabase::index_paths(&config, &[
        root.join("src/../src/a.rs"),   // → src/a.rs
        root.join("src/../outside.rs"), // → outside.rs (out of target)
    ])
    .unwrap();

    assert!(symbol_present(&db, "alpha_v2"), "the collapsed src/a.rs path was reconciled");
    assert_eq!(
        file_rows(&config, "src/../src/a.rs", "1 = 1"),
        0,
        "no row is persisted under the un-normalized `src/../src/a.rs` key",
    );
    assert_eq!(
        db.indexed_file_count().unwrap(),
        files_before,
        "the out-of-target `outside.rs` (via `src/../`) is not indexed",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_does_not_tombstone_a_never_indexed_missing_path() {
    // A missing supplied path that was NEVER indexed (a typo, an out-of-target temp file) must not
    // get a spurious `kind='deleted'` overlay row — that row would shadow a real committed file
    // that later appears at the path (#659 review). Only indexed / git-deleted paths are
    // tombstoned.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    // `src/typo.rs` never existed and was never indexed.
    let _ = IndexDatabase::index_paths(&config, &[root.join("src/typo.rs")]).unwrap();
    assert_eq!(
        file_rows(&config, "src/typo.rs", "1 = 1"),
        0,
        "a never-indexed missing path gets no tombstone row",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_signals_a_package_refresh_for_a_supplied_cargo_toml() {
    // A `Cargo.toml` is not itself a target file, so it never reaches `files`; but naming it on the
    // command line must still SIGNAL a package-map refresh (crate names / path-deps must update
    // even for a clean/committed manifest) — the builder carries that signal out separately
    // (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"widget\"\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    let (files, _changes, manifest_signal) =
        crate::index::explicit_index_files_and_changes(&db, &config, &[root.join("Cargo.toml")])
            .unwrap();
    assert!(manifest_signal, "a supplied Cargo.toml sets the package-refresh signal");
    assert!(files.is_empty(), "the manifest is not a target file, so it is not itself indexed");

    // A supplied source file alone does not signal a package refresh.
    let (_, _, no_signal) =
        crate::index::explicit_index_files_and_changes(&db, &config, &[root.join("src/lib.rs")])
            .unwrap();
    assert!(!no_signal, "a supplied source file alone does not signal a package refresh");

    // A DELETED manifest still signals — a non-git / untracked `Cargo.toml` that was scanned into
    // `packages` and then removed is reported by neither git status nor a `files` row, but naming
    // it must still refresh the package map (drop its stale rows). Not gated on a git-confirmed
    // deletion (#659 review).
    fs::remove_file(root.join("Cargo.toml")).unwrap();
    let (_, _, deleted_signal) =
        crate::index::explicit_index_files_and_changes(&db, &config, &[root.join("Cargo.toml")])
            .unwrap();
    assert!(deleted_signal, "a supplied but DELETED Cargo.toml still signals a package refresh");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_defers_on_a_first_time_empty_repo() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    // A `src` target with no matching files → first-time-empty.
    fs::create_dir_all(root.join("src")).unwrap();
    let config = source_config(root.clone(), Language::Rust);

    let result = IndexDatabase::index_paths(&config, &[root.join("src/new.rs")]);
    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|err| err.downcast_ref::<crate::index::EmptyIndexRefused>().is_some()),
        "a scoped pass on a not-yet-registered repo surfaces #427 (the CLI defers on it): \
         {result:?}",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_defers_instead_of_rebuilding_an_uninitialized_non_empty_repo() {
    // A scoped Paths pass must NEVER fall through to a full rebuild: on a repo with content but no
    // index yet, `index --paths a.rs` would otherwise index the WHOLE repository instead of the one
    // supplied path. It defers (`EmptyIndexRefused`) — the CLI reports "run `rag-rat index` first".
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn beta() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    assert!(!config.database.exists(), "precondition: no index built yet");

    let result = IndexDatabase::index_paths(&config, &[root.join("src/a.rs")]);
    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|err| err.downcast_ref::<crate::index::EmptyIndexRefused>().is_some()),
        "an uninitialized index DEFERS a scoped pass rather than full-rebuilding: {result:?}",
    );
    assert!(!config.database.exists(), "deferring must not build any index");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_defers_for_a_repo_with_files_but_no_rows_in_a_shared_db() {
    // On a SHARED/global DB, the DB file + schema already exist (another repo created them), so the
    // missing-DB guard is false; and this repo has files on disk, so the #427 first-time-empty
    // check lets it through. A scoped pass then reaches the `repo_generation_file_count == 0`
    // bootstrap fallback — which for the sweep modes is a FULL rebuild. `Paths` must DEFER there
    // instead, or `index --paths src/a.rs` would index the WHOLE unindexed repo (#659 review).
    let repo_b = unique_temp_root();
    let _ = fs::remove_dir_all(&repo_b);
    fs::create_dir_all(repo_b.join("src")).unwrap();
    fs::write(repo_b.join("src/lib.rs"), "pub fn beta() {}\n").unwrap();
    let config_b = source_config(repo_b.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config_b).unwrap(); // owns the shared DB

    // repo A: files on disk, never indexed, sharing B's database.
    let repo_a = unique_temp_root();
    let _ = fs::remove_dir_all(&repo_a);
    fs::create_dir_all(repo_a.join("src")).unwrap();
    fs::write(repo_a.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    fs::write(repo_a.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
    let mut config_a = source_config(repo_a.clone(), Language::Rust);
    config_a.database = config_b.database.clone();

    let result = IndexDatabase::index_paths(&config_a, &[repo_a.join("src/a.rs")]);
    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|err| err.downcast_ref::<crate::index::EmptyIndexRefused>().is_some()),
        "a scoped pass on an unindexed repo in a shared DB DEFERS rather than full-rebuilding it: \
         {result:?}",
    );

    let _ = fs::remove_dir_all(&repo_a);
    let _ = fs::remove_dir_all(&repo_b);
}

#[test]
#[cfg(unix)]
fn index_paths_skips_a_supplied_symlink_to_an_in_repo_file() {
    // The base walker (`walker::walk_dir`) SKIPS symlinks, so a supplied path that is itself a
    // symlink — even to an in-repo source file — must be skipped too. Otherwise `index --paths
    // src/link.rs` writes an index row under the symlink path that a full/discover pass would never
    // produce: a duplicate of the real file's row (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap(), 1);

    // `src/link.rs` is a symlink to the real `src/a.rs`; supplying it must not add a second row.
    std::os::unix::fs::symlink(root.join("src/a.rs"), root.join("src/link.rs")).unwrap();
    let db = IndexDatabase::index_paths(&config, &[root.join("src/link.rs")]).unwrap();
    assert_eq!(
        db.indexed_file_count().unwrap(),
        1,
        "a supplied symlink is skipped — no duplicate index row under the symlink path",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
#[cfg(unix)]
fn index_paths_skips_a_path_through_a_symlinked_ancestor_dir() {
    // The symlink skip must reject a symlink at ANY component, not just the leaf: `src/link/b.rs`
    // where `src/link` is a symlink to an in-repo dir has a REGULAR leaf, but the walker skips the
    // `src/link` entry and never descends, so indexing it by path writes a row under a spelling a
    // full/discover pass never produces (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    // `other/` is OUTSIDE the `src` target, so its file is never indexed under its real spelling —
    // isolating the symlink-spelling row as the only thing that could appear.
    fs::create_dir_all(root.join("other")).unwrap();
    fs::write(root.join("other/b.rs"), "pub fn beta_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap(), 1);

    // `src/link` → `other`, so `src/link/b.rs` resolves to the real (regular) `other/b.rs`.
    std::os::unix::fs::symlink(root.join("other"), root.join("src/link")).unwrap();
    let db = IndexDatabase::index_paths(&config, &[root.join("src/link/b.rs")]).unwrap();
    assert_eq!(
        db.indexed_file_count().unwrap(),
        1,
        "a path crossing a symlinked ANCESTOR dir is skipped — no row under the symlink spelling",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
#[cfg(unix)]
fn index_paths_indexes_an_absolute_path_through_a_symlinked_root_spelling() {
    // An editor/git hook may hand `--paths` an absolute path spelled through a DIFFERENT symlink
    // than the canonical checkout root (macOS `/tmp` vs `/private/tmp`, a symlinked `$PWD`). A
    // LEXICAL `strip_prefix(config.root)` fails on that spelling; the builder must canonicalize
    // before giving up, or the valid in-repo edit is silently dropped (#659 review).
    let realroot = unique_temp_root();
    let _ = fs::remove_dir_all(&realroot);
    fs::create_dir_all(realroot.join("src")).unwrap();
    fs::write(realroot.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    // config.root is the CANONICAL root (as `Config::load` would normalize it).
    let canonical_root = rag_rat_base::paths::canonicalize(&realroot).unwrap();
    let config = source_config(canonical_root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    // A sibling symlink that is an alias of the canonical root.
    let link_root = realroot
        .parent()
        .unwrap()
        .join(format!("{}-alias", realroot.file_name().unwrap().to_string_lossy()));
    std::os::unix::fs::symlink(&canonical_root, &link_root).unwrap();

    // Edit the file, then reindex it via the SYMLINKED root spelling.
    fs::write(realroot.join("src/a.rs"), "pub fn alpha_v2() {}\n").unwrap();
    let db = IndexDatabase::index_paths(&config, &[link_root.join("src/a.rs")]).unwrap();
    assert!(
        symbol_present(&db, "alpha_v2"),
        "an absolute path through a symlinked root spelling is rebased canonically and indexed, \
         not dropped",
    );

    let _ = fs::remove_dir_all(&realroot);
    let _ = fs::remove_file(&link_root);
}

/// Total `packages` rows across all scopes — a manifest refresh in a fresh overlay scope adds one.
fn package_count(config: &Config) -> i64 {
    rusqlite::Connection::open(&config.database)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM packages", [], |row| row.get::<_, i64>(0))
        .unwrap()
}

#[test]
fn reindex_paths_refreshes_linked_overlay_packages_for_a_dirty_manifest() {
    // A manifest-only edit in a LINKED worktree (dirty `Cargo.toml`, no source-row change) routes
    // to `index_worktree_overlay`, whose refresh is delta-gated on indexed/tombstoned/pruned counts
    // — all zero here. The base `Paths` flow refreshes packages on its manifest signal even with
    // zero indexed files, and the overlay must match, or the branch resolves imports against a
    // stale package map (#659 review). Observable: the overlay scope gains its own `packages` row.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("Cargo.toml"), "[package]\nname = \"widget\"\n").unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    let base_packages = package_count(&config);
    assert!(base_packages >= 1, "base rebuild wrote the widget package row");

    // Linked worktree; dirty its Cargo.toml with NO source change (so only the manifest signal can
    // drive a package refresh).
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("Cargo.toml"), "[package]\nname = \"widget\"\n# edited\n").unwrap();

    let _ = crate::watch::reindex_paths(&config, &[linked.join("Cargo.toml")], |_| {}).unwrap();
    assert_eq!(
        package_count(&config),
        base_packages + 1,
        "the manifest-only linked edit refreshes the overlay scope's package map (a new row)",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn reindex_paths_refreshes_linked_overlay_packages_for_a_committed_manifest() {
    // The overlay's own `manifest_changed` is status-derived (dirty-only), so a CLEAN/committed
    // linked `Cargo.toml` produces no source rows and no dirty signal. The SUPPLIED-manifest signal
    // (routed via `WorktreePartition.manifest_roots` → `refresh_worktree_overlay_packages`) must
    // still refresh the overlay package map, so `index --paths` honors "also sees committed
    // changes" for the linked route (#659 review).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("Cargo.toml"), "[package]\nname = \"widget\"\n").unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    let base_packages = package_count(&config);

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Edit AND COMMIT the manifest on the branch → clean working tree, a committed manifest change.
    fs::write(linked.join("Cargo.toml"), "[package]\nname = \"widget\"\n# committed\n").unwrap();
    run_git(&linked, &["add", "Cargo.toml"]);
    run_git(&linked, &["commit", "-q", "-m", "manifest"]);

    let _ = crate::watch::reindex_paths(&config, &[linked.join("Cargo.toml")], |_| {}).unwrap();
    assert_eq!(
        package_count(&config),
        base_packages + 1,
        "a supplied CLEAN/committed linked manifest still refreshes the overlay package map",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn index_paths_tombstones_a_deleted_indexed_file_under_a_now_ignored_dir() {
    // A previously-indexed path that is now BOTH deleted AND under a newly-ignored directory must
    // still be tombstoned by `index --paths` — the early ignore filter used to `continue` before
    // the deletion branch, leaving the stale row visible until a full discover pass (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/generated")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    fs::write(root.join("src/generated/gen.rs"), "pub fn gen_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap(); // gen.rs indexed (no ignore rule yet)
    assert_eq!(IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap(), 2);

    // Now ignore `generated/` AND delete the indexed file under it.
    fs::write(root.join(".gitignore"), "generated/\n").unwrap();
    fs::remove_file(root.join("src/generated/gen.rs")).unwrap();
    let db = IndexDatabase::index_paths(&config, &[root.join("src/generated/gen.rs")]).unwrap();
    assert_eq!(
        db.indexed_file_count().unwrap(),
        1,
        "a deleted indexed file under a now-ignored dir is tombstoned, not skipped by the filter",
    );
    assert!(symbol_present(&db, "alpha_v1"), "the un-supplied file is untouched");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn index_paths_reindexes_on_stored_target_identity_drift_with_unchanged_bytes() {
    // The no-op skip must compare (sha256, language, kind), not sha alone: a target-identity change
    // with UNCHANGED bytes (e.g. an extension-precedence upgrade re-languages a path) must still
    // reindex, matching discovery's staleness. Simulate the drift by POISONING the stored row's
    // `language` — the config/target fingerprint is unchanged, so the pass stays in Paths mode (not
    // promoted to Discover) and the fix must be what triggers the reindex (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();

    // Read the LIVE row (max generation) — the schema-bootstrap harness seeds a dead poison-sibling
    // row at generation 0 that scoped queries must never observe.
    let stored_language = |config: &Config| -> String {
        rusqlite::Connection::open(&config.database)
            .unwrap()
            .query_row(
                "SELECT language FROM main.files WHERE path = 'src/a.rs' AND kind != 'deleted' \
                 ORDER BY generation DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    assert_eq!(stored_language(&config), "rust", "baseline: indexed as rust");

    // Poison the LIVE row's language to simulate a target-identity drift with identical bytes.
    rusqlite::Connection::open(&config.database)
        .unwrap()
        .execute(
            "UPDATE main.files SET language = 'python' WHERE path = 'src/a.rs' AND generation = \
             (SELECT MAX(generation) FROM main.files WHERE path = 'src/a.rs')",
            [],
        )
        .unwrap();

    // Reconcile the (byte-identical) path: a sha-only skip would leave the poisoned language; the
    // identity comparison must reindex it back to rust.
    let _ = IndexDatabase::index_paths(&config, &[root.join("src/a.rs")]).unwrap();
    assert_eq!(
        stored_language(&config),
        "rust",
        "a stored target-identity drift is reindexed, not sha-skipped",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_overlay_shadows_a_byte_identical_file_re_languaged_by_branch_config() {
    // A branch whose config RE-LANGUAGES a byte-identical file — a `.h` header the base indexes as
    // C and the branch, via a `cpp` target, indexes as C++ (both bindings claim `.h`) — is
    // invisible to the content delta (same bytes, clean tree). The overlay must still shadow
    // the base row with the branch's (language, kind): target-identity drift, mirroring
    // discovery's staleness (#659 review). Without it queries fall through to the stale base C
    // parse.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/widget.h"), "int widget(void);\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let base_config = source_config_dirs(main.clone(), Language::C, &["src"]);
    let mut db = IndexDatabase::rebuild(&base_config).unwrap(); // widget.h indexed as C

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // widget.h is byte-identical on the branch; only the branch CONFIG re-languages it to C++. The
    // overlay config keeps the base (main) root and swaps in the branch's targets, exactly as
    // `for_linked_worktree_overlay` produces.
    let branch_config = source_config_dirs(main.clone(), Language::Cpp, &["src"]);
    db.index_worktree_overlay(&branch_config, &linked, &mut |_| {}).unwrap();

    let cpp_overlay_rows: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM main.files
             WHERE path = 'src/widget.h' AND language = 'cpp' AND kind != 'deleted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        cpp_overlay_rows >= 1,
        "the overlay shadows the base C row with the branch's C++ parse of the identical header",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn overlay_target_drift_scan_is_gated_on_the_config_fingerprint() {
    // The per-file drift scan is O(base-files); it must be SKIPPED when the branch config's targets
    // match the base's (fingerprint equal → no file can re-language), so a no-divergent-config
    // worktree does not re-scan every base file on every overlay refresh (#577 event-scoping). Only
    // a genuinely divergent branch config may drift (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/widget.h"), "int widget(void);\n").unwrap();
    init_git_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "base"]);
    let base_config = source_config_dirs(root.clone(), Language::C, &["src"]);
    let db = IndexDatabase::rebuild(&base_config).unwrap(); // records the base-scope target marker

    assert!(
        !db.overlay_targets_may_drift(&base_config.targets).unwrap(),
        "matching targets → the drift scan is skipped",
    );
    let cpp_config = source_config_dirs(root.clone(), Language::Cpp, &["src"]);
    assert!(
        db.overlay_targets_may_drift(&cpp_config.targets).unwrap(),
        "a divergent branch config (C→C++) → the drift scan runs",
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
#[cfg(unix)]
fn index_paths_tombstones_a_regular_file_replaced_by_a_symlink() {
    // An indexed regular file that is then REPLACED by a symlink must be tombstoned by `index
    // --paths`, not left as a stale no-op — the walker skips the symlink, so its row must go. This
    // needs BOTH the prep-side routing (symlink → deletion branch) and the fs-deletion revalidation
    // using `symlink_metadata` (a symlink is not a "restored" regular file) (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn beta_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap(), 2);

    // Replace the indexed src/a.rs with a symlink (its target b.rs IS a file, so `is_file` follows
    // to true — the exact case a sha/is_file check would mis-treat as "restored").
    fs::remove_file(root.join("src/a.rs")).unwrap();
    std::os::unix::fs::symlink(root.join("src/b.rs"), root.join("src/a.rs")).unwrap();
    // Spelled from the RAW scratch root, which is a different (non-canonical) name for the same
    // directory than the canonicalized `config.root` — exactly what a caller hands over on macOS
    // (`/var` vs `/private/var`) or through a symlinked `$PWD`. That drives the non-canonical-root
    // retry in `explicit_index_files_and_changes`, which must rebase the path WITHOUT resolving
    // the symlink LEAF: resolving it would turn this into `src/b.rs` and the stale row for
    // `src/a.rs` would never be tombstoned (#1027).
    let db = IndexDatabase::index_paths(&config, &[root.join("src/a.rs")]).unwrap();

    assert_eq!(
        db.indexed_file_count().unwrap(),
        1,
        "the symlink-replaced file's stale row is tombstoned, not left as a no-op",
    );
    assert!(symbol_present(&db, "beta_v1"), "the un-supplied file is untouched");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_overlay_indexes_a_file_newly_targeted_by_the_branch_config() {
    // A branch that ADDS a target dir (via its config) for files that already exist in the base
    // tree but were never base-indexed must index them in the overlay. The content delta never
    // sees them (unchanged bytes), and a base-rows scan can't (no base row) — the config-aware
    // walk covers it (#659 review).
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::create_dir_all(main.join("extra")).unwrap();
    fs::write(main.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    fs::write(main.join("extra/tool.rs"), "pub fn tool() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    // Base config targets ONLY `src`, so extra/tool.rs is never base-indexed.
    let base_config = source_config_dirs(main.clone(), Language::Rust, &["src"]);
    let mut db = IndexDatabase::rebuild(&base_config).unwrap();
    let rows_for = |db: &IndexDatabase, path: &str| -> i64 {
        db.storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM main.files WHERE path = ?1 AND kind != 'deleted'",
                [path],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(rows_for(&db, "extra/tool.rs"), 0, "extra/ is not a base target — no base row");

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Branch config ADDS `extra` as a target (same root, superset targets).
    let branch_config = source_config_dirs(main.clone(), Language::Rust, &["src", "extra"]);
    db.index_worktree_overlay(&branch_config, &linked, &mut |_| {}).unwrap();

    assert!(
        rows_for(&db, "extra/tool.rs") >= 1,
        "a file newly targeted by the branch config is indexed in the overlay",
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
#[cfg(unix)]
fn index_paths_tombstones_a_regular_file_replaced_by_an_escaping_symlink() {
    // A regular file that was indexed and is then replaced by a symlink pointing OUTSIDE the repo
    // must STILL be tombstoned — the containment check must not short-circuit the deletion branch —
    // and (critically) the external target is NEVER read/indexed (#659 review).
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha_v1() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn beta_v1() {}\n").unwrap();
    let config = source_config(root.clone(), Language::Rust);
    let _ = IndexDatabase::rebuild(&config).unwrap();
    assert_eq!(IndexDatabase::open(&config.database).unwrap().indexed_file_count().unwrap(), 2);

    // Replace the indexed src/a.rs with a symlink to an EXTERNAL file (outside the repo root).
    let outside =
        root.parent().unwrap().join(format!("{}-ext", root.file_name().unwrap().to_string_lossy()));
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("evil.rs"), "pub fn evil_marker() {}\n").unwrap();
    fs::remove_file(root.join("src/a.rs")).unwrap();
    std::os::unix::fs::symlink(outside.join("evil.rs"), root.join("src/a.rs")).unwrap();

    // Spelled from the RAW scratch root for the same reason as the in-repo symlink case above:
    // the non-canonical spelling drives the rebase retry, which must not follow the symlink LEAF —
    // following it lands OUTSIDE the root and the path is dropped before the deletion branch ever
    // runs, silently leaving the stale row behind (#1027).
    let db = IndexDatabase::index_paths(&config, &[root.join("src/a.rs")]).unwrap();
    // Count 1 proves BOTH: a.rs's stale row is tombstoned AND the external evil.rs is not indexed
    // (an external read would have reindexed src/a.rs, keeping the count at 2).
    assert_eq!(
        db.indexed_file_count().unwrap(),
        1,
        "the stale row is tombstoned and the escaping symlink's external target is not indexed",
    );
    assert!(symbol_present(&db, "beta_v1"), "the un-supplied file is untouched");

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
}
