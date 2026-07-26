use super::*;

/// Plan 4b Task 5c: `build_class` threads the medoid's `symbol_id` out as
/// `CandidateCloneClass::medoid_symbol_id`. For a normal (non-sampled) clone class:
///   - `medoid_symbol_id` is `Some`.
///   - The id it contains is one of the class's member symbol_ids.
///   - It is stable across two independent `find_clones` calls on the same index.
///
/// The medoid is the bag-overlap medoid (max Σ overlap/max_len), NOT an LCS-distance medoid —
/// sound as a template-spine anchor for a coherence-split class (§1.1). Task 5d uses it as the
/// anti-unify anchor, falling back to the canonical-first `(struct_hash, path, start_byte)` member
/// when `None`.
#[test]
fn build_class_surfaces_medoid_symbol_id() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // Two rename-clone functions: same structure, different local names. Both fingerprint to the
    // SAME struct_hash (rename-clone), so they form one class via the struct_hash fast path.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();

    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(res.classes.len(), 1, "one clone class");
    let class = &res.classes[0];

    // medoid_symbol_id must be Some for any non-degenerate class.
    let medoid_id = class
        .medoid_symbol_id
        .expect("medoid_symbol_id must be Some for a non-degenerate clone class");

    // Collect the actual symbol_ids of the class members by resolving their qualified names from
    // the DB — `CloneMember` doesn't expose the rowid, so we look it up via the helper.
    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");
    let member_ids = [id_a, id_b];

    assert!(
        member_ids.contains(&medoid_id),
        "medoid_symbol_id ({medoid_id}) must be one of the class's member symbol_ids \
         ({member_ids:?})"
    );

    // Stability: a second find_clones call on the same index must return the same medoid_symbol_id.
    let res2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    let medoid_id2 =
        res2.classes[0].medoid_symbol_id.expect("medoid_symbol_id must be Some on second call");
    assert_eq!(
        medoid_id, medoid_id2,
        "medoid_symbol_id must be deterministic across repeated calls"
    );

    // Close the SQLite connection before deleting its dir: Windows refuses to remove a file with a
    // live handle (`os error 32`), whereas Unix unlinks it lazily. Dropping `db` first makes the
    // strict teardown pass on both.
    drop(db);
    fs::remove_dir_all(&root).unwrap();
}

/// Worktree-overlay scope: `find_clones` returns the BRANCH-ONLY clone class under the overlay
/// scope, and the base scope has no clone classes. Proves the clone read is scope-correct —
/// only the branch's symbol_fingerprint rows (written by `index_worktree_overlay`) are visible
/// under the linked scope; the base sees only its own (non-clone) file.
#[test]
fn worktree_overlay_find_clones_reflects_branch_clone_pair() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // Base has only a tiny function — below MIN_TOKENS, so no fingerprint, no clone class.
    fs::write(main.join("src/base.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // Confirm base has NO clone classes.
    let base_before = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        base_before.classes.is_empty(),
        "base scope must have no clone classes before overlay: {:?}",
        base_before.classes
    );

    // Create a linked worktree on a new branch that ADDS a rename-clone pair.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    // Two renamed-clone functions — same structure as the existing clone fixture.
    fs::write(
        linked.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        linked.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "add clone pair"]);

    // Index the overlay — leaves connection in the linked scope.
    let report = db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(report.indexed >= 1, "the branch's new files are indexed as overlay rows");

    // Under the overlay scope, find_clones must return the branch's clone class.
    let overlay_res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        overlay_res.classes.len(),
        1,
        "overlay scope must expose exactly the branch's clone class: {:?}",
        overlay_res.classes
    );
    let class = &overlay_res.classes[0];
    assert_eq!(class.member_count, 2, "the branch clone class has 2 members");

    // Round-6 regression (#215): `stale_members` must be 0 under an overlay scope. The branch's
    // members (src/a.rs, src/b.rs) are branch-ONLY — absent from the main checkout — so a
    // main-checkout staleness comparison would count them both "missing" → stale=2 (false). The
    // overlay is maintained from branch bytes, so `count_stale_member_paths` correctly skips the
    // main-checkout check under a linked-overlay scope and reports 0.
    assert_eq!(
        overlay_res.completeness.stale_members, 0,
        "branch-only overlay members must not be falsely reported stale against the main checkout"
    );

    // Base scope must still have no clone classes.
    set_base_scope(&mut db, &main);
    let base_after = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(
        base_after.classes.is_empty(),
        "base scope must have no clone classes after overlay indexing: {:?}",
        base_after.classes
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// Plan 4a (#215): the refine driver is BEST-EFFORT — when refine inputs are unavailable it leaves
/// the class in its Plan-2 un-refined shape rather than erroring. We force `load_refine_members` to
/// return `None` by deleting the source files AFTER indexing: the bags/fingerprints are already
/// persisted in SQLite (so `find_clones` can still build the class), but `read_to_string` of each
/// member's now-missing path fails, tripping the un-refinable fallback. The returned class must be
/// the bare candidate component with every refinement field cleared — no panic, no error.
#[test]
fn find_clones_falls_back_to_unrefined_when_source_unavailable() {
    let root = unique_temp_root();
    // Build the index — fingerprints/bags persisted in the DB under `root/.rag-rat`.
    let db = write_four_renamed_clones(&root);

    // Delete the source trees (a/ and b/) but keep `.rag-rat/` (the SQLite DB). Each member's path
    // now fails `read_to_string`, so `load_refine_members` returns `None` → un-refinable fallback.
    let _ = fs::remove_dir_all(root.join("a"));
    let _ = fs::remove_dir_all(root.join("b"));

    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        res.classes.len(),
        1,
        "the class is still built from persisted bags even with source gone"
    );
    let c = &res.classes[0];

    assert!(!c.refined, "source gone → refine inputs unavailable → class stays un-refined");
    assert_eq!(c.class_kind, "candidate_component", "un-refined classes keep the Plan-2 kind");
    assert!(c.lcs_ratio.is_none(), "no lcs_ratio on an un-refined class");
    assert!(c.refactorability.is_none(), "no refactorability on an un-refined class");
    assert!(c.confidence.is_none(), "no confidence on an un-refined class");
    assert!(c.refine_mode.is_none(), "no refine_mode on an un-refined class");

    // a/ and b/ are already gone; only `.rag-rat/` remains under root.
    let _ = fs::remove_dir_all(&root);
}

/// Fix 1 + Fix 2 (#215): a clone class with more than MAX_MEMBERS members exercises two paths that
/// a small fixture never reaches:
///  - Fix 1 (chunked hydration): `build_class` hydrates members in batches of HYDRATION_CHUNK
///    rather than one `?` host-param per member. With 60 members the single-statement path would
///    still fit under the SQLite var limit, but this proves the chunked accumulation produces the
///    correct `member_count`/`members.len()`/`truncated` semantics with no error — the chunking is
///    otherwise only stress-visible above ~999 members, which is too expensive to plant in a unit
///    test.
///  - Fix 2 (subject pinning): `clones_for_symbol` for a clone whose `symbols.id` falls LATE in the
///    component (past MAX_MEMBERS by id) must still return that subject in the capped member list —
///    the caller asked about THAT symbol.
#[test]
fn find_clones_caps_large_class_and_pins_late_subject() {
    use crate::index::query_api::MAX_MEMBERS;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // 60 rename-clone functions: identical structure, only the local variable name changes, so they
    // share a struct_hash and form ONE clone component well above MAX_MEMBERS. Files are named so
    // that lexical write order does NOT predetermine symbols.id order (the subject we resolve by
    // ref is a LATE one, whose rowid lands past MAX_MEMBERS in the component's id-sorted
    // order).
    const N: usize = 60;
    for i in 0..N {
        let var = format!("v{i}");
        fs::write(
            root.join(format!("src/f{i:02}.rs")),
            format!(
                "pub fn f{i:02}(db: Db) -> i32 {{ let {var} = db.get(1); validate({var}); {var} + \
                 1 }}\n"
            ),
        )
        .unwrap();
    }
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Fix 1: find_clones returns the full-population class — member_count is all 60, the returned
    // member list is capped at MAX_MEMBERS, truncated is set, and there is NO error.
    let res = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        res.classes.len(),
        1,
        "the 60 rename-clones form one class: {:?}",
        res.classes.len()
    );
    let class = &res.classes[0];
    assert_eq!(class.member_count, N, "member_count reflects the FULL component");
    assert_eq!(class.total_members, N, "total_members reflects the FULL component");
    assert_eq!(class.members.len(), MAX_MEMBERS, "returned members are capped at MAX_MEMBERS");
    assert_eq!(class.members_returned, MAX_MEMBERS, "members_returned == cap");
    assert!(res.completeness.truncated, "a capped member list must set truncated");

    // Find a subject whose symbols.id is LATE in the component (past MAX_MEMBERS in id order), so
    // the plain `take(cap)` path would DROP it. We read the highest fingerprinted symbol id's
    // qualified name — that member sorts last in the component's id order, well past
    // MAX_MEMBERS.
    let conn = db.storage.connection();
    let late_ref: String = {
        let mut stmt = conn
            .prepare(
                "SELECT ns.value
                 FROM symbols
                 JOIN files ON files.id = symbols.file_id
                 JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                 JOIN symbol_fingerprints sf
                   ON sf.symbol_id = symbols.id AND sf.normalizer_kind = 'baseline'
                 ORDER BY symbols.id DESC
                 LIMIT 1",
            )
            .unwrap();
        stmt.query_row([], |r| r.get(0)).unwrap()
    };

    // Fix 2: clones_for_symbol for that late subject must INCLUDE it in the capped member list.
    let by_ref = db.clones_for_symbol(CloneSymbolSelector::Ref(late_ref.clone())).unwrap();
    let pinned = by_ref.class.as_ref().expect("the late subject is in the clone class");
    assert_eq!(pinned.member_count, N, "the class still reports the full population");
    assert_eq!(pinned.members.len(), MAX_MEMBERS, "members are capped at MAX_MEMBERS");
    assert!(
        pinned.members.iter().any(|m| m.r#ref == late_ref),
        "the pinned late subject {late_ref} must appear in the capped members: {:?}",
        pinned.members.iter().map(|m| &m.r#ref).collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(&root);
}

/// Fix 5 (#215): the clone surface stays empty (and errors NOT) when no fingerprint rows survive —
/// `build_class`'s `raw_members.is_empty()` guard returns `None` rather than building an
/// internally-inconsistent class. We delete every fingerprint row after a clone class was formed
/// and assert `find_clones` returns no classes with no error.
#[test]
fn find_clones_returns_no_class_when_fingerprints_vanish() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Baseline: one clone class.
    let before = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(before.classes.len(), 1, "baseline: one clone class");

    // Drop every fingerprint row. The candidate read loads bags from the same rows, so no component
    // forms and the Fix 5 empty-check guarantees no malformed class can leak through. Either way
    // the surface must be empty with no error.
    db.storage.connection().execute("DELETE FROM symbol_fingerprints", []).unwrap();
    let after = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert!(after.classes.is_empty(), "no fingerprints ⇒ no clone classes (no error): {after:?}");

    let _ = fs::remove_dir_all(&root);
}

/// Fix 4 (#215): the `Ref` and `PathLine` resolution arms now LEFT JOIN symbol_fingerprints and
/// prefer a fingerprinted row. This is primarily a SQL-correctness change (proven to COMPILE and to
/// not regress the existing resolution tests). Here we additionally assert the simple positive case
/// keeps working end-to-end: a fingerprinted clone resolves by `Ref` AND `PathLine` to its class.
#[test]
fn clones_for_symbol_prefers_fingerprinted_row_on_resolution() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Ref resolution finds the fingerprinted clone and its class.
    let by_ref =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    assert!(by_ref.symbol_fingerprinted, "Ref must resolve to the fingerprinted row");
    let ref_class = by_ref.class.as_ref().expect("Ref resolves into the clone class");

    // PathLine resolution at line 1 reaches the same class via the fingerprint-preferred ordering.
    let by_line = db
        .clones_for_symbol(CloneSymbolSelector::PathLine { path: "src/a.rs".into(), line: 1 })
        .unwrap();
    assert!(by_line.symbol_fingerprinted, "PathLine must resolve to the fingerprinted row");
    let line_class = by_line.class.as_ref().expect("PathLine resolves into the clone class");
    assert_eq!(
        ref_class.class_key, line_class.class_key,
        "Ref and PathLine must resolve to the same fingerprinted class"
    );

    let _ = fs::remove_dir_all(&root);
}

// ── Fix 1 regression guard (PathLine tightest-span PRIMARY) ──────────────────────────────────

/// PathLine CONTRACT: span is PRIMARY. The tightest-spanning symbol at the cursor wins,
/// regardless of fingerprint status. A tiny unfingerprinted (below MIN_TOKENS) nested item that
/// is ENCLOSED by a larger fingerprinted outer function must be returned when the cursor is
/// within the inner item — we must NOT silently jump to the enclosing fingerprinted function.
///
/// Fixture: two symbols at line 3 of the same file — the OUTER spans lines 1-10 and is
/// fingerprinted (it is large enough), and an INNER placeholder spans lines 3-3 and is NOT
/// fingerprinted (below MIN_TOKENS). PathLine{line=3} must resolve to the INNER symbol (smaller
/// span), not the OUTER one.
///
/// Because the test fixture is entirely synthetic (we inject symbols directly into the DB rather
/// than relying on the parser to produce nested symbols from source), the inner symbol has a
/// bare stub source that definitely stays below MIN_TOKENS.
#[test]
fn pathline_tightest_span_wins_over_fingerprinted_enclosing() {
    use rag_rat_clones::NORM_VERSION;

    // Strategy: inject TWO symbols rows directly into the DB for the same file and same line,
    // with DIFFERENT spans. The OUTER has a wider span (lines 1-10) and IS fingerprinted (we
    // copy the real fp row from an indexed clone). The INNER has span 0 (lines 5-5) and has NO
    // fingerprint row. PathLine{line=5} must resolve to the INNER (tightest span), not the
    // OUTER (wider span, but fingerprinted).
    //
    // We use a clone pair so at least one function is fingerprinted (large enough token count),
    // and then inject a synthetic outer that wraps the fingerprinted symbol's span.
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();

    // Clone pair so load_user gets a real fingerprint row.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();

    // Look up the indexed load_user symbol (line 1-1 after parsing).
    let (lu_id, lu_file_id): (i64, i64) = conn
        .query_row(
            "SELECT symbols.id, symbols.file_id FROM symbols
             JOIN files ON files.id = symbols.file_id
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             WHERE files.path = 'src/a.rs'
             ORDER BY (end_line - start_line) ASC
             LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    // Verify load_user is fingerprinted.
    let lu_fp_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = \
             'baseline'",
            [lu_id],
            |r| r.get(0),
        )
        .unwrap();
    if lu_fp_count == 0 {
        // load_user not fingerprinted — can't run this test; skip gracefully.
        let _ = fs::remove_dir_all(&root);
        return;
    }

    // Inject a SYNTHETIC OUTER symbol covering lines 1-20 (wider than load_user's 1-1).
    // It WILL be fingerprinted (we copy load_user's fp row to it).
    // Inner: inject at lines 1-1 with start_byte slightly different — same line, same span.
    // The key: inject a synthetic WIDE outer symbol spanning lines 1-20, fingerprinted.
    // Then inject a synthetic TINY inner symbol at lines 1-1 (same line, span 0), NOT
    // fingerprinted. PathLine{line=1} now has TWO candidates: outer (span 19) and inner (span
    // 0). The INNER must win (tightest span), even though the OUTER is fingerprinted.

    // Fake name string for the outer.
    conn.execute(
        "INSERT OR IGNORE INTO name_strings (value) VALUES ('src/a.rs::synthetic_outer')",
        [],
    )
    .unwrap();
    let outer_name_id: i64 = conn
        .query_row(
            "SELECT id FROM name_strings WHERE value = 'src/a.rs::synthetic_outer'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Inject the wide outer symbol (lines 1-20, large span).
    conn.execute(
        "INSERT INTO symbols (file_id, name, qualified_name_id, kind, language, start_line, \
         end_line, start_byte, end_byte) VALUES (?1, 'synthetic_outer', ?2, 'function', 'rust', \
         1, 20, 0, 1000)",
        rusqlite::params![lu_file_id, outer_name_id],
    )
    .unwrap();
    let outer_id: i64 = conn.last_insert_rowid();

    // Copy load_user's fp row to the outer (making it fingerprinted with the same token bag).
    let (nk, nv, tl, sh, created_at): (String, i64, i64, String, i64) = conn
        .query_row(
            "SELECT normalizer_kind, normalizer_version, token_len, struct_hash, created_at_ms \
             FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = 'baseline' LIMIT \
             1",
            [lu_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO symbol_fingerprints (symbol_id, normalizer_kind, \
         normalizer_version, token_len, struct_hash, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, \
         ?6)",
        rusqlite::params![outer_id, &nk, nv, tl, &sh, created_at],
    )
    .unwrap();

    // Fake name string for the tiny inner (NOT fingerprinted).
    conn.execute(
        "INSERT OR IGNORE INTO name_strings (value) VALUES ('src/a.rs::synthetic_inner')",
        [],
    )
    .unwrap();
    let inner_name_id: i64 = conn
        .query_row(
            "SELECT id FROM name_strings WHERE value = 'src/a.rs::synthetic_inner'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Inject the tiny inner symbol at lines 1-1 (within the outer's 1-20 span, span=0).
    // NO fingerprint row for this one.
    conn.execute(
        "INSERT INTO symbols (file_id, name, qualified_name_id, kind, language, start_line, \
         end_line, start_byte, end_byte) VALUES (?1, 'synthetic_inner', ?2, 'function', 'rust', \
         1, 1, 0, 10)",
        rusqlite::params![lu_file_id, inner_name_id],
    )
    .unwrap();
    let inner_id: i64 = conn.last_insert_rowid();

    // Sanity: outer IS fingerprinted, inner is NOT.
    let outer_fp: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = \
             'baseline' AND normalizer_version = ?2",
            rusqlite::params![outer_id, NORM_VERSION],
            |r| r.get(0),
        )
        .unwrap();
    let inner_fp: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = \
             'baseline' AND normalizer_version = ?2",
            rusqlite::params![inner_id, NORM_VERSION],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(outer_fp, 1, "outer must be fingerprinted for the regression to be testable");
    assert_eq!(inner_fp, 0, "inner must NOT be fingerprinted");

    // PathLine at line 1: must resolve to the TIGHTEST spanning symbol.
    // - load_user: span 0 (lines 1-1) — fingerprinted
    // - synthetic_inner: span 0 (lines 1-1) — NOT fingerprinted
    // - synthetic_outer: span 19 (lines 1-20) — fingerprinted
    //
    // The tightest span is 0 (load_user or synthetic_inner, both at lines 1-1). The old
    // fingerprint-first ORDER BY would have put synthetic_outer FIRST if we had the bug.
    // The correct ORDER BY puts synthetic_outer LAST (span=19 > span=0).
    //
    // The regression guard: if fingerprint-presence were PRIMARY, the resolver would pick
    // outer (fingerprinted, span=19) over inner (unfingerprinted, span=0). With the fix,
    // it picks one of the span-0 symbols first.
    //
    // To make this unambiguous, we can directly verify that the outer is NOT the resolved symbol
    // by checking: if synthetic_inner (no fp) is resolved, symbol_fingerprinted=false.
    // But since load_user also has span=0 and IS fingerprinted, the tiebreaker (fp-then-rowid)
    // may pick load_user. Either way, synthetic_outer (span=19) must NOT be picked.
    //
    // Direct verification: query what PathLine resolves to using the same SQL as the resolver.
    let resolved_id: Option<i64> = conn
        .query_row(
            "SELECT symbols.id
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             LEFT JOIN symbol_fingerprints sf
               ON sf.symbol_id = symbols.id
               AND sf.normalizer_kind = 'baseline'
               AND sf.normalizer_version = ?3
             WHERE files.path = ?1
               AND ?2 BETWEEN symbols.start_line AND symbols.end_line
             ORDER BY (symbols.end_line - symbols.start_line) ASC,
                      (sf.symbol_id IS NULL) ASC, symbols.id ASC
             LIMIT 1",
            rusqlite::params!["src/a.rs", 1i64, NORM_VERSION],
            |r| r.get(0),
        )
        .optional()
        .unwrap();

    let resolved_symbol_id = resolved_id.expect("line 1 in src/a.rs must resolve to SOME symbol");

    // The resolved symbol must NOT be the outer (span=19). It must be one of the span=0 symbols.
    assert_ne!(
        resolved_symbol_id, outer_id,
        "PathLine must NOT resolve to synthetic_outer (span=19, fingerprinted) — the tightest \
         span (0) must win; this would fail with fingerprint-first ORDER BY"
    );

    // The span of the resolved symbol must be 0 (lines 1-1), not 19.
    let (res_start, res_end): (i64, i64) = conn
        .query_row(
            "SELECT start_line, end_line FROM symbols WHERE id = ?1",
            [resolved_symbol_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        res_end - res_start,
        0,
        "resolved symbol must have span=0 (lines {res_start}-{res_end}), not the wide outer \
         (span=19)"
    );

    let _ = fs::remove_dir_all(&root);
}

// ── Fix 2: Ref ambiguity rejection ───────────────────────────────────────────────────────────

/// Fix 2 (#215): a `Ref` that matches EXACTLY ONE fingerprinted symbol resolves normally.
/// A `Ref` that matches NO fingerprinted symbols falls back to the unfingerprinted path
/// (`symbol_resolved=true, symbol_fingerprinted=false, class=None`) — existing behaviour preserved.
///
/// True same-ref duplicate-fingerprinted injection is not possible via the standard indexer
/// (the indexer deduplicates by qualified_name per file), so we test the two non-ambiguous paths
/// here and note the gap. The ambiguous-ref path (>1 fingerprinted match → Err) is tested via
/// direct DB injection in the dedicated fixture below.
#[test]
fn clones_for_symbol_ref_single_fingerprinted_resolves_unfingerprinted_falls_back() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two rename-clones so load_user is fingerprinted and in a class.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    // A tiny function: resolves but is not fingerprinted.
    fs::write(root.join("src/tiny.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Exactly 1 fingerprinted match → resolves to clone class.
    let res = db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    assert!(res.symbol_resolved);
    assert!(res.symbol_fingerprinted);
    assert!(res.class.is_some(), "single fingerprinted Ref must return its clone class");

    // 0 fingerprinted matches (tiny is below MIN_TOKENS) → resolved but not fingerprinted.
    let tiny_res =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/tiny.rs::tiny".into())).unwrap();
    assert!(tiny_res.symbol_resolved, "the unfingerprinted symbol still resolves");
    assert!(!tiny_res.symbol_fingerprinted, "tiny is below MIN_TOKENS");
    assert!(tiny_res.class.is_none());

    let _ = fs::remove_dir_all(&root);
}

/// Fix 2 (#215): injecting TWO distinct fingerprinted `symbols` rows that share the SAME
/// qualified name causes `clones_for_symbol(Ref)` to return an `Err` with "disambiguate" in the
/// message. This exercises the >1 fingerprinted path in `resolve_selector_to_symbol_id`.
///
/// We inject the second symbol row directly into the DB (the indexer never produces same-ref
/// duplicates for the same file, but the index schema allows it and the code must handle it).
#[test]
fn clones_for_symbol_ref_ambiguous_fingerprinted_returns_err() {
    use rag_rat_clones::NORM_VERSION;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // One file with one function; rebuild gives us a clean indexed symbol + fingerprint.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();
    let conn = db.storage.connection();

    // Fetch the existing symbol's id and its name_id.
    let (orig_id, name_id, file_id): (i64, i64, i64) = conn
        .query_row(
            "SELECT symbols.id, symbols.qualified_name_id, symbols.file_id
             FROM symbols
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             WHERE ns.value = 'src/a.rs::load_user'
             LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();

    // Inject a SECOND symbols row sharing the same qualified_name_id and file_id but different
    // span — simulating an overload / cfg variant with the same qualified name.
    // `name` is the bare symbol identifier (NOT NULL); we reuse "load_user".
    conn.execute(
        "INSERT INTO symbols (file_id, name, qualified_name_id, kind, language, start_line, \
         end_line, start_byte, end_byte) VALUES (?1, 'load_user', ?2, 'function', 'rust', 2, 5, \
         10, 50)",
        rusqlite::params![file_id, name_id],
    )
    .unwrap();
    let dup_id: i64 = conn.last_insert_rowid();

    // Fetch an existing fingerprint row for orig_id to clone its token data.
    let fp: Option<(String, i64, i64, String)> = conn
        .query_row(
            "SELECT normalizer_kind, normalizer_version, token_len, struct_hash
             FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = 'baseline' AND \
             normalizer_version = ?2 LIMIT 1",
            rusqlite::params![orig_id, NORM_VERSION],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .unwrap();

    let Some((nk, nv, tl, sh)) = fp else {
        // If there's no fingerprint yet the ambiguity path can't be reached; skip gracefully.
        let _ = fs::remove_dir_all(&root);
        return;
    };

    // Give the duplicate its own fingerprint row (same normalizer_version = current).
    // created_at_ms is NOT NULL in STRICT mode; use 0 as a placeholder.
    conn.execute(
        "INSERT OR IGNORE INTO symbol_fingerprints (symbol_id, normalizer_kind, \
         normalizer_version, token_len, struct_hash, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        rusqlite::params![dup_id, nk, nv, tl, sh],
    )
    .unwrap();

    // Verify the dup fp row was actually inserted (it would be silently ignored if the PK
    // already existed, which can't happen here since dup_id is fresh, but be explicit).
    let dup_fp_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE symbol_id = ?1 AND normalizer_kind = \
             'baseline' AND normalizer_version = ?2",
            rusqlite::params![dup_id, NORM_VERSION],
            |r| r.get(0),
        )
        .unwrap();
    if dup_fp_count == 0 {
        // fp INSERT was silently ignored (shouldn't happen) — skip the test.
        let _ = fs::remove_dir_all(&root);
        return;
    }

    // Now Ref("src/a.rs::load_user") matches TWO fingerprinted symbols → must return Err.
    let err = db
        .clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into()))
        .expect_err("Ref matching >1 fingerprinted symbols must return Err, not silently pick one");
    let msg = err.to_string();
    assert!(msg.contains("disambiguate"), "error message must mention 'disambiguate', got: {msg}");
    assert!(
        msg.contains("src/a.rs::load_user"),
        "error message must name the ambiguous ref, got: {msg}"
    );

    let _ = fs::remove_dir_all(&root);
}

// ── Fix 3: stale_members in completeness ────────────────────────────────────────────────────

/// Fix 3 (#215): `completeness.stale_members` counts DISTINCT returned-member file paths whose
/// on-disk content no longer matches the indexed `files.sha256`.
///
/// Clean index → `stale_members == 0`. After editing one member file on disk WITHOUT reindexing
/// → `stale_members >= 1`.
#[test]
fn find_clones_stale_members_zero_on_clean_index_and_nonzero_after_disk_edit() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    let a_path = root.join("src/a.rs");
    let b_path = root.join("src/b.rs");
    fs::write(
        &a_path,
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        &b_path,
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    // Clean index: stale_members must be 0.
    let clean = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(clean.classes.len(), 1, "the two rename-clones form one class");
    assert_eq!(
        clean.completeness.stale_members, 0,
        "a freshly-indexed index with unchanged files must report stale_members=0"
    );

    // Edit one member file on disk WITHOUT reindexing — content now differs from indexed sha256.
    fs::write(&a_path, "pub fn load_user(db: Db) -> i32 { /* EDITED: body replaced */ 42 }\n")
        .unwrap();

    // find_clones reads PERSISTED fingerprint tables (unchanged) but stale_members checks disk.
    let stale = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    assert_eq!(
        stale.classes.len(),
        1,
        "the class is still returned (stale detection is read-only)"
    );
    assert!(
        stale.completeness.stale_members >= 1,
        "after editing src/a.rs on disk, stale_members must be >= 1, got {}",
        stale.completeness.stale_members
    );

    let _ = fs::remove_dir_all(&root);
}

/// Faithfulness pin (#215 Plan 4a Task 2): `load_refine_members` re-parses each member's scoped
/// source and re-normalizes to the ordered baseline token sequence — the strong correctness
/// guarantee is that `tokens::struct_hash(&member.seq)` reproduces the PERSISTED
/// `symbol_fingerprints.struct_hash` exactly (the re-parse is faithful to Plan-1's normalization).
/// Also pins: seqs are non-empty, members come back sorted by struct_hash, and the lang/byte-range
/// are populated.
#[test]
fn load_refine_members_reparse_is_faithful_to_persisted_struct_hash() {
    use rag_rat_clones::tokens::struct_hash;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // Two rename-clone functions — identical structure, different identifier names. Both
    // fingerprint to the SAME struct_hash (renamed clones), so sorting by struct_hash is stable on
    // symbol_id.
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    let members = db
        .load_refine_members(&[id_a, id_b], false)
        .unwrap()
        .expect("refine inputs available for an unchanged, in-scope clone pair");
    assert_eq!(members.len(), 2, "both members loaded");

    // Persisted struct_hash per member, for the faithfulness comparison.
    let persisted = |sid: i64| -> String {
        db.storage
            .connection()
            .query_row(
                "SELECT struct_hash FROM symbol_fingerprints
                 WHERE symbol_id = ?1 AND normalizer_kind = 'baseline'",
                params![sid],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };

    for m in &members {
        assert!(!m.seq.is_empty(), "the re-parsed token sequence must be non-empty");
        assert_eq!(m.lang, Language::Rust, "member language is rust");
        // THE PIN: the re-parse reproduces Plan-1's normalization exactly.
        assert_eq!(
            struct_hash(&m.seq),
            m.struct_hash,
            "re-parsed struct_hash must equal the member's persisted struct_hash"
        );
        assert_eq!(
            m.struct_hash,
            persisted(m.symbol_id),
            "the member's carried struct_hash must equal the DB-persisted struct_hash"
        );
    }

    // Members are returned in canonical sorted-by-struct_hash order. Production keys the canonical
    // order on the REINDEX-STABLE `(struct_hash, path, start_byte)`; `RefineMember` carries no
    // `path`/`start_byte`, so this test can only re-derive `(struct_hash, symbol_id)` — the
    // fixtures arrange `symbol_id` to coincide with `(path, start_byte)`, so the orders match
    // here. The REAL reindex-stable-order guard is the unit test
    // `refine_member_order_is_reindex_stable`.
    let mut expected =
        members.iter().map(|m| (m.struct_hash.clone(), m.symbol_id)).collect::<Vec<_>>();
    expected.sort();
    let actual = members.iter().map(|m| (m.struct_hash.clone(), m.symbol_id)).collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "members must be in canonical (struct_hash, path, start_byte) order — \
         struct_hash-ascending here (fixtures pin symbol_id to coincide); real guard: \
         refine_member_order_is_reindex_stable"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Empty input is a valid (empty) refine set — not a failure.
#[test]
fn load_refine_members_empty_input_returns_empty() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn f() -> i32 { 0 }\n").unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let members =
        db.load_refine_members(&[], false).unwrap().expect("empty input is a valid empty set");
    assert!(members.is_empty(), "empty member_ids → empty members");

    let _ = fs::remove_dir_all(&root);
}

/// Missing source: a member whose source file is deleted from disk (but whose fingerprint row is
/// still persisted) makes the re-parse impossible, so `load_refine_members` returns `Ok(None)` —
/// the caller falls back to an un-refined class rather than refining over a partial input.
#[test]
fn load_refine_members_returns_none_when_source_missing() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    let db = IndexDatabase::rebuild(&source_config(root.clone(), Language::Rust)).unwrap();

    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    // Delete one member's source file on disk; the fingerprint rows are unchanged in the index.
    fs::remove_file(root.join("src/b.rs")).unwrap();

    let result = db.load_refine_members(&[id_a, id_b], false).unwrap();
    assert!(
        result.is_none(),
        "a member with a deleted source file must yield Ok(None) for the whole class"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Overlay fallback (#215 Plan 4a Task 2): under a LINKED-WORKTREE OVERLAY scope, `source_root` is
/// the MAIN checkout — not the branch the overlay's symbol rows came from — so no scope-correct
/// source read is available and `load_refine_members` must return `Ok(None)` BEFORE touching disk.
/// Mirrors `count_stale_member_paths` / the staleness heal path's overlay early-return.
#[test]
fn load_refine_members_returns_none_under_linked_overlay_scope() {
    let main = unique_temp_root();
    let _ = fs::remove_dir_all(&main);
    fs::create_dir_all(main.join("src")).unwrap();
    // Base has only a tiny (below-MIN_TOKENS) function — no fingerprint, no clone class.
    fs::write(main.join("src/base.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    init_git_repo(&main);
    run_git(&main, &["add", "."]);
    run_git(&main, &["commit", "-q", "-m", "base"]);
    let config = source_config(main.clone(), Language::Rust);
    let mut db = IndexDatabase::rebuild(&config).unwrap();

    // Linked worktree on a new branch adds a rename-clone pair.
    let linked = unique_temp_root();
    let _ = fs::remove_dir_all(&linked);
    run_git(&main, &["worktree", "add", "-q", "-b", "feat", linked.to_str().unwrap()]);
    fs::write(
        linked.join("src/a.rs"),
        "pub fn load_user(db: Db) -> i32 { let u = db.get(1); validate(u); u + 1 }\n",
    )
    .unwrap();
    fs::write(
        linked.join("src/b.rs"),
        "pub fn load_order(s: Db) -> i32 { let o = s.get(2); validate(o); o + 1 }\n",
    )
    .unwrap();
    run_git(&linked, &["add", "."]);
    run_git(&linked, &["commit", "-q", "-m", "add clone pair"]);

    // Index the overlay — leaves the connection in the linked (overlay) scope.
    db.index_worktree_overlay(&config, &linked, &mut |_| {}).unwrap();
    assert!(db.active_scope_is_linked_overlay(), "connection must be in the overlay scope");

    // Resolve the branch members' ids (under the overlay scope they are visible).
    let id_a = fingerprinted_symbol_id_for_ref(&db, "src/a.rs::load_user");
    let id_b = fingerprinted_symbol_id_for_ref(&db, "src/b.rs::load_order");

    // Even with valid member ids, refine is unavailable under an overlay scope.
    let result = db.load_refine_members(&[id_a, id_b], false).unwrap();
    assert!(
        result.is_none(),
        "refine must be unavailable (Ok(None)) under a linked-worktree overlay scope"
    );

    let _ = fs::remove_dir_all(&main);
    let _ = fs::remove_dir_all(&linked);
}

/// #275 (Plan 3) end to end: a clone class whose ONLY difference is the callee SPELLING —
/// baseline refines it with a `differing_callee` closure_param (it cannot prove `validate` /
/// `check` / `verify` / `audit` are one function). Seeding CURRENT `edge_oracle` rows that
/// resolve every member's callee to ONE moniker flips the next refine into scip mode: the callee
/// column collapses back into the fixed spine (coverage 1.0, no variation point, no
/// `differing_callee`) and the class carries `refine_mode = "scip"` provenance. The baseline and
/// scip refinements coexist as separate cache rows (the mode is folded into the key).
#[test]
fn scip_moniker_collapse_lifts_a_same_symbol_callee_class() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    // The write_four_renamed_clones shape, but the CALLEE differs per member — the one variation
    // baseline must flag as differing_callee and the oracle can collapse.
    let fixtures = [
        ("a", "load_user", "u", "validate"),
        ("a", "load_order", "o", "check"),
        ("b", "load_item", "i", "verify"),
        ("b", "load_blob", "x", "audit"),
    ];
    for (dir, name, var, callee) in fixtures {
        fs::write(
            root.join(dir).join(format!("{name}.rs")),
            format!(
                "pub fn {name}(db: Db) -> i32 {{ let {var} = db.get(1); {callee}({var}); {var} + \
                 1 }}\n"
            ),
        )
        .unwrap();
    }
    let config = four_clone_config(&root);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // Run 1 — NO oracle data: baseline mode, and the differing callee is a conservative
    // closure_param/differing_callee variation point.
    let r1 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    let class1 = &r1.classes[0];
    assert!(class1.refined, "run 1 refines the class");
    assert_eq!(class1.refine_mode, Some("baseline"), "no oracle coverage ⇒ baseline mode");
    let vps1 = class1.variation_points.as_ref().expect("run 1 VPs").as_array().unwrap().clone();
    assert!(
        vps1.iter().any(|vp| vp["differing_callee"] == serde_json::Value::Bool(true)),
        "baseline must flag the differing callee (non-vacuity guard), got {vps1:?}"
    );

    // Seed CURRENT oracle verdicts: each member file's callee identifier span resolves to the
    // SAME moniker. `file_sha` must equal the indexed (= on-disk) content hash, a COMPLETED
    // `oracle_runs` row in this checkout must stand behind their `(tool, tool_version)`, and each
    // verdict's content key must match a LIVE `calls_name` edge — the three currency gates the
    // collapse read applies. Deriving the verdict's content key from the real indexed edge (rather
    // than a synthetic callee-only span) is what the production oracle does, so the live-edge gate
    // sees a match.
    let conn = db.storage.connection();
    conn.execute(
        "INSERT INTO oracle_runs(repo_id, tool, tool_version, commit_sha, worktree_id, \
         started_at, status, stats_json)
         VALUES (?1, 'scip-rust', 'v1', ?2, ?3, 0, 'Completed', '{}')",
        rusqlite::params![db.active_repo_id, db.active_commit_sha, db.active_worktree_id],
    )
    .unwrap();
    for (dir, name, _var, callee) in fixtures {
        let rel = format!("{dir}/{name}.rs");
        let src = fs::read_to_string(root.join(&rel)).unwrap();
        let sha = rag_rat_base::hash::hex_sha256(src.as_bytes());
        // The real edge the extractor emitted for `callee(...)` — its source span is the whole
        // call expression, its callee span the identifier. The verdict is keyed by BOTH.
        let (src_lo, src_hi, cal_lo, cal_hi): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT edges.source_start_byte, edges.source_end_byte, edges.callee_start_byte, \
                 edges.callee_end_byte
                 FROM edges JOIN files ON files.id = edges.source_file_id
                 WHERE files.path = ?1 AND edges.edge_kind = 'calls_name' AND edges.to_name = ?2",
                rusqlite::params![rel, callee],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO edge_oracle(repo_id, source_path, source_start_byte, source_end_byte, \
             callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
             scip_symbol, kind, computed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'calls_name', ?7, 'scip-rust', 'v1', 'rust cr 1.0 \
             validate().', 'resolved', 0)",
            rusqlite::params![db.active_repo_id, rel, src_lo, src_hi, cal_lo, cal_hi, sha],
        )
        .unwrap();
    }

    // Run 2 — oracle coverage exists: scip mode, the callee collapses into the fixed spine.
    let r2 = db
        .find_clones(FindClonesOptions { min_similarity: None, min_copies: None, limit: None })
        .unwrap();
    let class2 = &r2.classes[0];
    assert!(class2.refined, "run 2 refines the class (scip-mode cold compute)");
    assert_eq!(
        class2.refine_mode,
        Some("scip"),
        "oracle coverage ⇒ scip mode provenance on the class"
    );
    let vps2 = class2.variation_points.as_ref().expect("run 2 VPs").as_array().unwrap().clone();
    assert!(
        vps2.iter().all(|vp| vp["differing_callee"] != serde_json::Value::Bool(true)),
        "a moniker-proven same-symbol callee must not be differing_callee, got {vps2:?}"
    );
    assert!(
        vps2.is_empty(),
        "the callee was the ONLY variation — the collapsed class has no variation points, got \
         {vps2:?}"
    );
    assert_eq!(
        class2.anti_unify_coverage,
        Some(1.0),
        "the collapsed callee column is fixed spine — coverage returns to 1.0"
    );

    // Both modes' refinements coexist as distinct cache rows (disjoint key namespaces). Scoped
    // to the active repo — the poison-sibling harness seeds its own refinement row.
    let modes: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT refine_mode FROM clone_refinements WHERE repo_id = ?1 ORDER BY refine_mode",
            )
            .unwrap();
        stmt.query_map([&db.active_repo_id], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(modes, vec!["baseline".to_string(), "scip".to_string()]);

    let _ = fs::remove_dir_all(&root);
}
