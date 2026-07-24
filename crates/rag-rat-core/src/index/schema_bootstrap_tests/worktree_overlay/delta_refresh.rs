use super::*;

/// The overlay's `(path, is_tombstone)` rows for `worktree_id` at the live scope — the observable
/// the #825 tree-diff-skip equivalence is asserted on (tombstones included: pruning one would
/// un-shadow a branch-deleted base file).
fn overlay_rows(db: &IndexDatabase, worktree_id: &str) -> Vec<(String, bool)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT path, kind = 'deleted' FROM main.files WHERE worktree_id = ?1 AND commit_sha \
             = '' ORDER BY path",
        )
        .unwrap();
    let rows = stmt
        .query_map([worktree_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)))
        .unwrap();
    rows.filter_map(Result::ok).collect()
}

#[test]
fn heads_unchanged_refresh_reuses_the_committed_delta_and_still_sees_dirty_edits() {
    // #825: when both heads equal the recorded basis, the refresh seeds its committed candidates
    // from the current overlay rows instead of walking the base↔linked tree diff; the status walk
    // still runs. Equivalence on a heads-unchanged fixture: the committed rows (a modified file's
    // overlay row, a deleted file's tombstone) come out identical, and a fresh dirty edit is
    // still picked up. The PROOF the tree walk was actually skipped is the seed path's one
    // accepted divergence: a previously-dirty file REVERTED to base content keeps its
    // (content-identical) overlay row — the full diff would have pruned it — until the next
    // HEAD move runs the full diff and does.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_a() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn base_b() {}\n").unwrap();
    fs::write(main.join("src/c.rs"), "pub fn base_c() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Committed branch delta: modify a.rs, delete b.rs (→ tombstone).
    fs::write(linked.join("src/a.rs"), "pub fn branch_a() {}\n").unwrap();
    fs::remove_file(linked.join("src/b.rs")).unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch delta"]);
    // Dirty edit that will be REVERTED before the seeded pass.
    fs::write(linked.join("src/c.rs"), "pub fn dirty_c() {}\n").unwrap();

    // Refresh maintaining the basis with REAL heads, exactly as the watcher does.
    let refresh = |db: &mut IndexDatabase| {
        let base_sha = crate::index::head_sha(&main);
        let linked_head = crate::index::head_sha(&linked);
        let tail = crate::index::OverlayRefreshTail {
            logical_rebuild: crate::index::OverlayLogicalRebuild::Inline,
            basis: Some(crate::index::OverlayBasisUpdate {
                base_sha: &base_sha,
                linked_head_sha: &linked_head,
            }),
        };
        db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap()
    };
    // Pass 1: no recorded basis → the full tree diff.
    let first = refresh(&mut db);
    assert!(first.status_complete);
    assert_eq!(first.indexed, 2, "branch a.rs + dirty c.rs");
    assert_eq!(first.tombstoned, 1, "deleted b.rs shadows its base row");
    let worktree_id = first.worktree_id.clone();
    let committed_rows = overlay_rows(&db, &worktree_id);

    // Heads unchanged; revert c.rs to base content and add a NEW dirty file d.rs.
    fs::write(linked.join("src/c.rs"), "pub fn base_c() {}\n").unwrap();
    fs::write(linked.join("src/d.rs"), "pub fn dirty_d() {}\n").unwrap();
    let second = refresh(&mut db);
    assert!(second.status_complete);
    let expected = {
        let mut expected = committed_rows.clone();
        expected.push(("src/d.rs".to_string(), false));
        expected.sort();
        expected
    };
    assert_eq!(
        overlay_rows(&db, &worktree_id),
        expected,
        "the seeded pass reproduces the committed delta exactly (a.rs row, b.rs tombstone), still \
         catches the new dirty d.rs via the status walk, and keeps the reverted c.rs row — the \
         divergence that proves the tree diff was skipped (a full diff prunes it)"
    );
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(names_in_scope(&db, "src/a.rs"), vec!["branch_a".to_string()]);
    assert!(!path_in_scope(&db, "src/b.rs"), "the tombstone still shadows the base row");
    assert_eq!(names_in_scope(&db, "src/d.rs"), vec!["dirty_d".to_string()]);

    // A HEAD move runs the full diff again, which prunes the lingering reverted row.
    set_base_scope(&mut db, &main);
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "commit the dirty work"]);
    let third = refresh(&mut db);
    assert!(third.status_complete);
    assert!(
        !overlay_rows(&db, &worktree_id).iter().any(|(path, _)| path == "src/c.rs"),
        "the next tree-diff refresh prunes the content-identical reverted row"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn a_dirty_gitignore_edit_forces_the_full_diff_on_matching_heads() {
    // #825: the one committed-side case the row seed provably cannot see — an ignore flip making
    // a committed branch-only file indexable for the FIRST time (it produced no row, and it is
    // tracked + clean so it is not in status). A dirty `.gitignore` among the status candidates
    // must force the full tree diff even though the heads match the basis.
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_a() {}\n").unwrap();
    fs::write(main.join("src/.gitignore"), "gen.rs\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // The branch COMMITS an ignored file (forced add): a tree-diff candidate on the first pass,
    // but not indexable → no overlay row.
    fs::write(linked.join("src/gen.rs"), "pub fn generated_fn() {}\n").unwrap();
    run_git(&linked, &["add", "-f", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds ignored file"]);

    let refresh = |db: &mut IndexDatabase| {
        let base_sha = crate::index::head_sha(&main);
        let linked_head = crate::index::head_sha(&linked);
        let tail = crate::index::OverlayRefreshTail {
            logical_rebuild: crate::index::OverlayLogicalRebuild::Inline,
            basis: Some(crate::index::OverlayBasisUpdate {
                base_sha: &base_sha,
                linked_head_sha: &linked_head,
            }),
        };
        db.index_worktree_overlay_with_tail(&config, &linked, tail, &mut |_| {}).unwrap()
    };
    let first = refresh(&mut db);
    assert!(first.status_complete);
    assert_eq!(first.indexed, 0, "the committed file is ignored — no overlay row yet");

    // Heads unchanged; a DIRTY `.gitignore` edit unignores it. Without the forced full diff the
    // seeded pass could never surface gen.rs: it has no row and is absent from status.
    fs::write(linked.join("src/.gitignore"), "").unwrap();
    let second = refresh(&mut db);
    assert!(second.status_complete);
    db.use_worktree_scope(&main, Some(&linked)).unwrap();
    assert_eq!(
        names_in_scope(&db, "src/gen.rs"),
        vec!["generated_fn".to_string()],
        "the dirty .gitignore edit forced the tree diff, surfacing the newly-unignored file"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #826: adding a function changes a file's logical-key set (a non-#820-relink change) — which used
/// to pay the whole-repo `rebuild_logical_symbols` on every incremental pass. It now re-derives
/// ONLY the changed path's groups. Observable: the base incremental pass runs ZERO whole-repo
/// logical rebuilds, yet the grouping is the wholesale rebuild's FIXED POINT (byte-identical rows,
/// ids, variant counts, member spans and hashes).
#[test]
fn a_key_set_change_scopes_the_logical_rederive_to_changed_paths() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn b_one() {}\npub fn b_two() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    drop(IndexDatabase::rebuild(&config).unwrap());

    // Add a function to a.rs (dirty, uncommitted) — a key-set change routes to the rebuild branch.
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\npub fn a_two() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();

    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a scoped re-derive runs ZERO whole-repo logical rebuilds"
    );
    assert!(logical_symbol_named(&db, "a_two"), "the added function is grouped");
    assert!(logical_symbol_named(&db, "b_one"), "the unchanged file's grouping is intact");

    // The scoped output is the wholesale rebuild's FIXED POINT — forcing the rebuild changes
    // nothing.
    let scoped = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(
        logical_grouping_snapshot(&db),
        scoped,
        "the scoped re-derive output must equal the whole-repo rebuild's output"
    );

    let _ = fs::remove_dir_all(&main);
}

/// #826 (the fast path must disagree with the fallback): POISON an UNCHANGED file's grouping row with
/// a wrong `variant_count`. A scoped re-derive editing a DIFFERENT file must leave the poison
/// intact — a whole-repo rebuild would correct it, so its survival proves the scoped pass never
/// re-derived the unchanged path (i.e. work scales with changed files, not repo size).
#[test]
fn the_scoped_logical_rederive_leaves_unchanged_paths_untouched() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\n").unwrap();
    fs::write(main.join("src/b.rs"), "pub fn b_one() {}\npub fn b_two() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    let repo_id = db.active_repo_id.clone();
    // Poison b.rs's grouping rows — each b_* function is its own group with variant_count 1.
    db.storage
        .connection()
        .execute(
            "UPDATE main.logical_symbols SET variant_count = 999 WHERE repo_id = ?1 AND path = ?2",
            rusqlite::params![repo_id, "src/b.rs"],
        )
        .unwrap();
    drop(db);

    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\npub fn a_two() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the pass took the scoped re-derive, not a whole-repo rebuild"
    );

    let poisoned: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT variant_count FROM main.logical_symbols WHERE repo_id = ?1 AND path = ?2 \
             LIMIT 1",
            rusqlite::params![repo_id, "src/b.rs"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        poisoned, 999,
        "the UNCHANGED file's poisoned grouping row survives — the scoped pass never touched b.rs"
    );
    assert!(logical_symbol_named(&db, "a_two"), "the edited file WAS regrouped");

    // A whole-repo rebuild DOES correct the poison — proving it was live and fixable, so the scoped
    // pass's decision to skip b.rs is what preserved it.
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    let corrected: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT variant_count FROM main.logical_symbols WHERE repo_id = ?1 AND path = ?2 \
             LIMIT 1",
            rusqlite::params![repo_id, "src/b.rs"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(corrected, 1, "a whole-repo rebuild corrects the poisoned variant_count");

    let _ = fs::remove_dir_all(&main);
}

/// #826 over the OVERLAY finalize (Inline): a key-set change on a LINKED WORKTREE re-derives only
/// that worktree's changed paths' groups, not the whole repo. ZERO whole-repo rebuilds, and the
/// grouping is the wholesale rebuild's fixed point — the scoped re-derive regroups the changed path
/// across ALL its scopes (raw `main.files`), so base + overlay members stay correct.
#[test]
fn a_linked_worktree_overlay_scopes_the_logical_rederive() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() -> i32 {\n    1\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(logical_symbol_named(&db, "one_fn"), "the added overlay file is grouped");

    // A KEY-SET change on the linked worktree (add two_fn to one.rs, uncommitted → dirty overlay).
    fs::write(
        linked.join("src/one.rs"),
        "pub fn one_fn() -> i32 {\n    1\n}\npub fn two_fn() {}\n",
    )
    .unwrap();
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        0,
        "the overlay finalize's scoped re-derive runs ZERO whole-repo rebuilds"
    );
    assert!(logical_symbol_named(&db, "two_fn"), "the added overlay function is grouped");

    let scoped = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(
        logical_grouping_snapshot(&db),
        scoped,
        "the scoped overlay re-derive output must equal the whole-repo rebuild's output"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #826 gate: a lagging `logical_key_version` owes the #493 drift heal, which ONLY the whole-repo
/// `rebuild_logical_symbols` performs. The scoped re-derive must NOT be used — the pass keeps the
/// full rebuild.
#[test]
fn a_stale_logical_key_version_keeps_the_whole_repo_rebuild() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();
    // Force a version lag exactly as the #493 drift-heal tests do.
    db.storage
        .connection()
        .execute("DELETE FROM repo_meta WHERE key = 'logical_key_version'", [])
        .unwrap();
    drop(db);

    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\npub fn a_two() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "a lagging logical_key_version keeps the whole-repo rebuild (for the #493 drift heal), \
         never the scoped re-derive"
    );
    assert!(logical_symbol_named(&db, "a_two"), "the edit is still grouped");

    let _ = fs::remove_dir_all(&main);
}

/// #826: the scoped re-derive DROPS a group whose symbol is gone from every live scope. Adding a
/// function to a dirty file then removing it leaves that function in NO scope (it was never
/// committed), so the scoped DELETE + regroup must drop its `logical_symbols` row — with ZERO
/// whole-repo rebuilds. (A single dirty deletion of a COMMITTED symbol is deliberately NOT tested
/// here: `logical_symbols` is scope-independent, so the committed symbol's group correctly survives
/// until the deletion is committed — the same for a scoped re-derive and a whole-repo rebuild.)
#[test]
fn the_scoped_rederive_drops_a_removed_symbols_group() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    drop(IndexDatabase::rebuild(&config).unwrap());

    // Add a_two (dirty overlay) — a_two now exists ONLY in the overlay scope.
    fs::write(main.join("src/a.rs"), "pub fn a_one() {}\npub fn a_two() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert!(logical_symbol_named(&db, "a_two"), "the added overlay symbol is grouped");
    drop(db);

    // Remove a_two again (the `// edited` line keeps a.rs DIRTY vs the commit, so this stays a
    // scoped dirty re-index rather than a stale-overlay heal). a_two is now gone from EVERY live
    // scope — it was only ever in the overlay.
    fs::write(main.join("src/a.rs"), "// edited\npub fn a_one() {}\n").unwrap();
    let db = IndexDatabase::index_changed(&config).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "dropping a symbol takes the scoped re-derive, not a whole-repo rebuild"
    );
    assert!(!logical_symbol_named(&db, "a_two"), "the removed symbol's group is dropped");
    assert!(logical_symbol_named(&db, "a_one"), "the surviving symbol's group is intact");

    // Byte-identity: the drop leaves exactly what a whole-repo rebuild would.
    let scoped = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(logical_grouping_snapshot(&db), scoped, "the drop equals the whole-repo rebuild");

    let _ = fs::remove_dir_all(&main);
}

fn rust_symbol_selector(name: &str) -> rag_rat_query::symbol::SymbolSelector {
    rag_rat_query::symbol::SymbolSelector {
        logical_symbol_id: None,
        symbol_id: None,
        symbol_path: None,
        symbol: Some(name.to_string()),
        language: Some(Language::Rust),
        allow_ambiguous: true,
        limit: 10,
    }
}

/// #897: a symbol present in BOTH the base commit and a linked worktree's overlay (same key) has a
/// STORED `variant_count` of 2 (`scope_replica`), but under the linked worktree's scope only ONE
/// member is visible. `symbol_lookup` must report the SCOPE-VISIBLE count (1) / `single` — matching
/// the member list it returns — not the corpus-level stored total.
#[test]
fn overlay_scope_symbol_lookup_reports_scope_visible_variant_count() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn foo() -> i32 {\n    1\n}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Body-only edit keeps foo's key line, so foo is a 2-member scope_replica group (base +
    // overlay).
    fs::write(linked.join("src/a.rs"), "pub fn foo() -> i32 {\n    2\n}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch body edit"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // index_worktree_overlay leaves the connection in the overlay scope — symbol_lookup sees ONE
    // foo.
    let hit = db.symbol_candidates(&rust_symbol_selector("foo"), false).unwrap();
    assert!(!hit.candidates.is_empty(), "foo resolves in the overlay scope");
    assert_eq!(
        hit.candidates[0].logical_variant_count,
        Some(1),
        "overlay scope sees ONE foo — variant_count must be the scope-visible count, not the \
         corpus total (2)"
    );
    assert_eq!(hit.candidates[0].logical_group_reason.as_deref(), Some("single"));

    // The graph/impact variant LIST must be scoped too, or it would disagree with the count and
    // leak the hidden base member (#897 review): one visible member in the overlay scope, not two.
    let logical_id = hit.candidates[0].logical_symbol_id.expect("logical id");
    let members =
        rag_rat_query::symbol::logical_members(db.storage.connection(), logical_id).unwrap();
    assert_eq!(members.len(), 1, "the scoped variant list matches the scope-visible count (1)");

    // The STORED corpus-level count is still 2: the fix is at surfacing time, not in the table.
    let stored: i64 = db
        .storage
        .connection()
        .query_row(
            "SELECT variant_count FROM main.logical_symbols WHERE repo_id = ?1 AND logical_name = \
             ?2",
            rusqlite::params![db.active_repo_id, "foo"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, 2, "the stored corpus-level variant_count remains 2");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #897 (rename half): under the overlay scope, `symbol_lookup` reflects the BRANCH's renamed symbol
/// and NOT the pre-rename name; the base scope is the reverse — per-branch symbol visibility.
#[test]
fn overlay_scope_symbol_lookup_reflects_a_branch_rename() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn target_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/a.rs"), "pub fn renamed_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "rename"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    let resolves = |db: &IndexDatabase, name: &str| {
        !db.symbol_candidates(&rust_symbol_selector(name), false).unwrap().candidates.is_empty()
    };
    // Overlay scope (left installed by index_worktree_overlay):
    assert!(resolves(&db, "renamed_fn"), "overlay scope sees the branch rename");
    assert!(!resolves(&db, "target_fn"), "overlay scope does not see the pre-rename name");
    // Base scope:
    set_base_scope(&mut db, &main);
    assert!(resolves(&db, "target_fn"), "base scope keeps the original name");
    assert!(!resolves(&db, "renamed_fn"), "base scope does not see the branch rename");

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #898: with TWO simultaneously-live worktree overlays holding divergent branch-only content,
/// removing exactly ONE worktree and running gc prunes only that worktree's `files` rows — the
/// other live worktree's overlay rows remain intact.
#[test]
fn gc_prunes_only_the_removed_worktrees_overlay_with_two_live_overlays() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/base.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let wt_a = unique_temp_root();
    let _ = fs::remove_dir_all(&wt_a);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-a", wt_a.to_str().unwrap()]);
    fs::write(wt_a.join("src/a_only.rs"), "pub fn a_only() {}\n").unwrap();
    run_git(&wt_a, &["add", "."]);
    run_git(&wt_a, &["commit", "-q", "-m", "branch a"]);
    let wt_b = unique_temp_root();
    let _ = fs::remove_dir_all(&wt_b);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat-b", wt_b.to_str().unwrap()]);
    fs::write(wt_b.join("src/b_only.rs"), "pub fn b_only() {}\n").unwrap();
    run_git(&wt_b, &["add", "."]);
    run_git(&wt_b, &["commit", "-q", "-m", "branch b"]);

    db.index_worktree_overlay(&config, &wt_a, &mut |_| {}).unwrap();
    db.index_worktree_overlay(&config, &wt_b, &mut |_| {}).unwrap();
    let a_id = worktree_id_of(&wt_a);
    let b_id = worktree_id_of(&wt_b);
    assert!(
        overlay_rows(&db, &a_id).iter().any(|(path, _)| path == "src/a_only.rs"),
        "worktree A's branch-only file is in its overlay"
    );
    assert!(
        overlay_rows(&db, &b_id).iter().any(|(path, _)| path == "src/b_only.rs"),
        "worktree B's branch-only file is in its overlay"
    );

    // Remove ONLY worktree B, then gc.
    run_git(&main, &["worktree", "remove", "--force", wt_b.to_str().unwrap()]);
    set_base_scope(&mut db, &main);
    db.garbage_collect().unwrap();
    assert!(overlay_rows(&db, &b_id).is_empty(), "gc pruned the removed worktree B's overlay rows");
    assert!(
        overlay_rows(&db, &a_id).iter().any(|(path, _)| path == "src/a_only.rs"),
        "the still-live worktree A's overlay rows are untouched"
    );
    crate::index::poison_sibling::assert_sibling_intact(db.storage.connection());

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&wt_a);
}

/// #899: a linked worktree that DROPS a symbol via `index_worktree_overlay` (the Inline #826 route,
/// NOT the Deferred batch fallback): the dropped symbol's group is removed by the scoped re-derive,
/// the surviving symbol is kept, ZERO whole-repo rebuilds, and the result is the full rebuild's
/// fixed point. Covers the #826 scoped path for a linked-worktree rename/removal (previously only
/// the addition case exercised the Inline route).
#[test]
fn a_linked_worktree_symbol_drop_scopes_the_logical_rederive() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    fs::write(main.join("src/a.rs"), "pub fn base_fn() {}\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() {}\n").unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "branch adds one_fn"]);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();

    // Dirty-add two_fn — it exists ONLY in the overlay (never committed).
    fs::write(linked.join("src/one.rs"), "pub fn one_fn() {}\npub fn two_fn() {}\n").unwrap();
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(logical_symbol_named(&db, "two_fn"), "the dirty-added overlay symbol is grouped");

    // Dirty-remove two_fn (the `// edited` line keeps one.rs dirty vs the feat commit, so this
    // stays the Inline scoped route, not a stale-overlay heal). two_fn is now gone from EVERY
    // live scope.
    fs::write(linked.join("src/one.rs"), "// edited\npub fn one_fn() {}\n").unwrap();
    let before = db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed);
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert_eq!(
        db.logical_symbol_rebuilds.load(std::sync::atomic::Ordering::Relaxed) - before,
        0,
        "the drop is served by the Inline scoped re-derive, not a whole-repo rebuild"
    );
    assert!(
        !logical_symbol_named(&db, "two_fn"),
        "the removed overlay symbol's group is dropped via the scoped path"
    );
    assert!(logical_symbol_named(&db, "one_fn"), "the surviving symbol's group is intact");

    let scoped = logical_grouping_snapshot(&db);
    db.rebuild_logical_symbols(crate::index::graph_index::KeyVersionStamp::Defer).unwrap();
    assert_eq!(
        logical_grouping_snapshot(&db),
        scoped,
        "the scoped drop equals the whole-repo rebuild"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}
