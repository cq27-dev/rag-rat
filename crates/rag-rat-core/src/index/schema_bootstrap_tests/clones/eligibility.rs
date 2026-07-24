use super::*;

/// Fix 3 (#215): `clones_for_symbol` carries eligibility flags. A below-`MIN_TOKENS` function
/// RESOLVES (`symbol_resolved=true`) but is not fingerprinted (`symbol_fingerprinted=false`,
/// `class=None`); an eligible-but-unique function is fingerprinted with no class; an eligible
/// clone yields `class=Some`.
#[test]
fn clones_for_symbol_reports_eligibility() {
    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    // tiny: below MIN_TOKENS ⇒ resolves but is never fingerprinted.
    fs::write(root.join("src/tiny.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    // solo: a substantial, structurally distinct function ⇒ fingerprinted but in no clone class.
    fs::write(
        root.join("src/solo.rs"),
        "pub fn solo(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n ^= b as usize; } n }\n",
    )
    .unwrap();
    // a/b: two rename-clones ⇒ an eligible clone class.
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

    // tiny: resolves, not fingerprinted, no class.
    let tiny = db.clones_for_symbol(CloneSymbolSelector::Ref("src/tiny.rs::tiny".into())).unwrap();
    assert!(tiny.symbol_resolved, "tiny resolves to a scoped symbol");
    assert!(!tiny.symbol_fingerprinted, "tiny is below MIN_TOKENS ⇒ not fingerprinted");
    assert!(tiny.class.is_none(), "an unfingerprinted symbol is in no class");

    // solo: eligible (fingerprinted) but unique ⇒ no class.
    let solo = db.clones_for_symbol(CloneSymbolSelector::Ref("src/solo.rs::solo".into())).unwrap();
    assert!(solo.symbol_resolved, "solo resolves");
    assert!(solo.symbol_fingerprinted, "solo is substantial ⇒ fingerprinted (eligible)");
    assert!(solo.class.is_none(), "a unique eligible symbol has no clone class");

    // load_user: eligible AND a clone ⇒ class is Some.
    let clone =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    assert!(clone.symbol_resolved, "load_user resolves");
    assert!(clone.symbol_fingerprinted, "load_user is fingerprinted");
    let class = clone.class.expect("load_user is in a clone class");
    assert_eq!(class.member_count, 2, "the clone class has both rename-clones");

    let _ = fs::remove_dir_all(root);
}

/// #274 item 3a: `clones_for_symbol` reports a RICHER eligibility reason than the bare
/// `symbol_fingerprinted = false` bool, distinguishing the four "not eligible" causes:
/// `BelowMinTokens`, `NonFunctionKind`, `Generated`, and `StaleNormalizerVersion`. Each variant is
/// exercised with a symbol that triggers exactly it, plus the `Eligible` and `SymbolNotResolved`
/// verdicts and the bool/enum consistency invariant.
#[test]
fn clones_for_symbol_distinguishes_ineligibility_reasons() {
    use rag_rat_clones::NORM_VERSION;

    let root = unique_temp_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("src/generated")).unwrap();

    // tiny: a `function` below MIN_TOKENS ⇒ no fingerprint row ⇒ BelowMinTokens.
    fs::write(root.join("src/tiny.rs"), "pub fn tiny() -> i32 { 0 }\n").unwrap();
    // a struct: a non-function symbol ⇒ NonFunctionKind. (A struct never fingerprints.)
    fs::write(
        root.join("src/shape.rs"),
        "pub struct Shape { pub width: i32, pub height: i32, pub depth: i32 }\n",
    )
    .unwrap();
    // generated: a SUBSTANTIAL function (clears MIN_TOKENS) in a path-heuristic generated file.
    // It keeps `kind = source` and gets symbols, but `files.generated = 1`, so the index-time
    // fingerprint compute is skipped — Generated must win over BelowMinTokens (it clears the size
    // floor, so the only reason it is unfingerprinted is the generated flag).
    fs::write(
        root.join("src/generated/bindings.rs"),
        "pub fn shared_symbol(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n ^= b as usize; \
         } n }\n",
    )
    .unwrap();
    // stale: a substantial, structurally distinct function ⇒ normally fingerprinted (eligible). We
    // demote its fingerprint row to NORM_VERSION - 1 below so the current-version read misses it
    // while a baseline row still EXISTS ⇒ StaleNormalizerVersion.
    fs::write(
        root.join("src/stale.rs"),
        "pub fn stale_fn(v: Vec<u8>) -> usize { let mut n = 0; for b in v { n += b as usize; } n \
         }\n",
    )
    .unwrap();
    // eligible: two rename-clones so each is fingerprinted AND in a clone class ⇒ Eligible.
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

    // Demote stale_fn's baseline fingerprint to a PRIOR normalizer_version: the row still exists,
    // but the current-NORM_VERSION read filter excludes it.
    {
        let conn = db.storage.connection();
        let updated = conn
            .execute(
                "UPDATE symbol_fingerprints
                 SET normalizer_version = ?2
                 WHERE normalizer_kind = 'baseline'
                   AND normalizer_version = ?1
                   AND symbol_id IN (
                       SELECT symbols.id FROM symbols
                       JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                       WHERE ns.value = 'src/stale.rs::stale_fn'
                   )",
                rusqlite::params![NORM_VERSION, NORM_VERSION - 1],
            )
            .unwrap();
        assert_eq!(updated, 1, "exactly one stale_fn baseline row should be demoted");
    }

    // Helper: assert the bool fields stay consistent with the enum verdict (the invariant the
    // result type documents).
    let assert_consistent = |res: &crate::index::ClonesForSymbolResult| match res.eligibility {
        crate::index::CloneEligibility::SymbolNotResolved => {
            assert!(!res.symbol_resolved && !res.symbol_fingerprinted);
        },
        crate::index::CloneEligibility::Eligible => {
            assert!(res.symbol_resolved && res.symbol_fingerprinted);
        },
        crate::index::CloneEligibility::Ineligible { .. } => {
            assert!(res.symbol_resolved && !res.symbol_fingerprinted);
        },
    };

    // BelowMinTokens.
    let tiny = db.clones_for_symbol(CloneSymbolSelector::Ref("src/tiny.rs::tiny".into())).unwrap();
    assert_eq!(
        tiny.eligibility,
        crate::index::CloneEligibility::Ineligible {
            reason: crate::index::CloneIneligibilityReason::BelowMinTokens
        },
        "a below-MIN_TOKENS function reports BelowMinTokens"
    );
    assert_consistent(&tiny);

    // NonFunctionKind.
    let shape =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/shape.rs::Shape".into())).unwrap();
    assert_eq!(
        shape.eligibility,
        crate::index::CloneEligibility::Ineligible {
            reason: crate::index::CloneIneligibilityReason::NonFunctionKind
        },
        "a struct (non-function kind) reports NonFunctionKind"
    );
    assert_consistent(&shape);

    // Generated (wins over BelowMinTokens even though the body clears the size floor).
    let generated = db
        .clones_for_symbol(CloneSymbolSelector::Ref(
            "src/generated/bindings.rs::shared_symbol".into(),
        ))
        .unwrap();
    assert_eq!(
        generated.eligibility,
        crate::index::CloneEligibility::Ineligible {
            reason: crate::index::CloneIneligibilityReason::Generated
        },
        "a substantial function in a generated file reports Generated"
    );
    assert_consistent(&generated);

    // StaleNormalizerVersion (a baseline row exists, just not at the current version).
    let stale =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/stale.rs::stale_fn".into())).unwrap();
    assert_eq!(
        stale.eligibility,
        crate::index::CloneEligibility::Ineligible {
            reason: crate::index::CloneIneligibilityReason::StaleNormalizerVersion
        },
        "a symbol whose only fingerprint row is at a prior normalizer_version reports \
         StaleNormalizerVersion"
    );
    assert_consistent(&stale);

    // Eligible (fingerprinted + in a clone class).
    let eligible =
        db.clones_for_symbol(CloneSymbolSelector::Ref("src/a.rs::load_user".into())).unwrap();
    assert_eq!(
        eligible.eligibility,
        crate::index::CloneEligibility::Eligible,
        "a fingerprinted symbol reports Eligible"
    );
    assert!(eligible.class.is_some(), "load_user is in a clone class");
    assert_consistent(&eligible);

    // SymbolNotResolved (no such symbol).
    let missing = db
        .clones_for_symbol(CloneSymbolSelector::Ref("src/nope.rs::does_not_exist".into()))
        .unwrap();
    assert_eq!(
        missing.eligibility,
        crate::index::CloneEligibility::SymbolNotResolved,
        "an unresolved selector reports SymbolNotResolved"
    );
    assert_consistent(&missing);

    // The enum serializes as an internally-tagged object with the snake_case wire tokens that
    // cross the MCP/CLI boundary.
    let json = serde_json::to_value(&tiny).unwrap();
    assert_eq!(json["eligibility"]["status"], "ineligible");
    assert_eq!(json["eligibility"]["reason"], "below_min_tokens");
    let json_ok = serde_json::to_value(&eligible).unwrap();
    assert_eq!(json_ok["eligibility"]["status"], "eligible");

    // as_db_str / from_db_str round-trip for every variant.
    for reason in [
        crate::index::CloneIneligibilityReason::Generated,
        crate::index::CloneIneligibilityReason::NonFunctionKind,
        crate::index::CloneIneligibilityReason::StaleNormalizerVersion,
        crate::index::CloneIneligibilityReason::BelowMinTokens,
    ] {
        assert_eq!(
            crate::index::CloneIneligibilityReason::from_db_str(reason.as_db_str()),
            Some(reason),
            "as_db_str/from_db_str must round-trip"
        );
    }
    assert_eq!(crate::index::CloneIneligibilityReason::from_db_str("bogus"), None);

    let _ = fs::remove_dir_all(root);
}
