use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::index::clones::normalize::normalize_baseline_spanned;
use crate::index::clones::tokens;
use crate::index::parser;
use crate::language::Language;

/// Build a `RefineMember` from a Rust snippet, mirroring `load_refine_members`: parse, descend
/// to the first `function` symbol, span-normalize, compute the faithfulness struct_hash.
fn member(symbol_id: i64, src: &str) -> RefineMember {
    let text: Arc<str> = Arc::from(src);
    let parsed = parser::parse_file(Path::new("t.rs"), Language::Rust, &text).expect("parse");
    let func = parsed.symbols.iter().find(|s| s.kind == "function").expect("a function symbol");
    let node =
        parsed.root().descendant_for_byte_range(func.start_byte, func.end_byte).expect("node");
    let (seq, node_spans) = normalize_baseline_spanned(node, &text, Language::Rust);
    let struct_hash = tokens::struct_hash(&seq);
    RefineMember {
        callee_monikers: Default::default(),
        symbol_id,
        lang: Language::Rust,
        struct_hash,
        seq,
        node_spans,
        text,
    }
}

/// Build a `RefineMember` from a TypeScript snippet — the TS analogue of [`member`]. Picks the
/// target symbol by MAX normalized-token count (the function body), exactly as the production
/// loader / the `normalize` tests' `target_node_for` do, so it works for TS `function`,
/// `const`/arrow declarators, etc. Used by the template-literal / TS-string tests (#254 #274).
fn member_ts(symbol_id: i64, src: &str) -> RefineMember {
    let text: Arc<str> = Arc::from(src);
    let parsed = parser::parse_file(Path::new("t.ts"), Language::TypeScript, &text).expect("parse");
    let node = parsed
        .symbols
        .iter()
        .filter_map(|s| {
            let n = parsed.root().descendant_for_byte_range(s.start_byte, s.end_byte)?;
            Some((normalize_baseline_spanned(n, &text, Language::TypeScript).0.len(), n))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, n)| n)
        .expect("a body symbol");
    let (seq, node_spans) = normalize_baseline_spanned(node, &text, Language::TypeScript);
    let struct_hash = tokens::struct_hash(&seq);
    RefineMember {
        callee_monikers: Default::default(),
        symbol_id,
        lang: Language::TypeScript,
        struct_hash,
        seq,
        node_spans,
        text,
    }
}

/// Sort members into the canonical order the loader guarantees. Production keys on the
/// REINDEX-STABLE `(struct_hash, path, start_byte)` (see `canonical_member_order_key` /
/// `refine_member_order_is_reindex_stable`). `RefineMember` (a test fixture here) carries no
/// `path`/`start_byte`, so this helper sorts `struct_hash` then `symbol_id` — the test members
/// assign `symbol_id` to coincide with `(path, start_byte)`, so the two keys produce the SAME
/// order on these fixtures; the production guard is the reindex-stable unit test, not this
/// sort.
fn canonical(mut members: Vec<RefineMember>) -> Vec<RefineMember> {
    members.sort_by(|a, b| {
        a.struct_hash.cmp(&b.struct_hash).then_with(|| a.symbol_id.cmp(&b.symbol_id))
    });
    members
}

fn run(members: Vec<RefineMember>) -> (Vec<RefineMember>, Template) {
    let members = canonical(members);
    let anchor_idx = resolve_anchor_idx(&members, None);
    let alignment = align_to_anchor(&members, anchor_idx);
    let template = anti_unify(&members, &alignment);
    (members, template)
}

#[test]
fn two_renamed_clones_differ_only_in_literal_one_value_param() {
    // Same structure, differ only in the literal KIND (int vs float) — alpha-renaming makes
    // identifiers identical, so the single differing column is the literal leaf.
    let a = member(1, "fn f() { let x = 10; }");
    let b = member(2, "fn f() { let y = 1.5; }");
    let (_members, template) = run(vec![a, b]);

    assert_eq!(template.variation_points.len(), 1, "expected exactly one variation point");
    let vp = &template.variation_points[0];
    assert_eq!(vp.kind, MetavarKind::ValueParam, "literal hole must be a value_param");
    assert_eq!(vp.extraction_role, "value_param");
    assert_eq!(vp.confidence, Confidence::High);
    // per-member values are the real source literals.
    let mut vals = vp.per_member_values.clone();
    vals.sort();
    assert_eq!(vals, vec!["1.5".to_string(), "10".to_string()]);
}

#[test]
fn differing_called_function_is_closure_param_low() {
    // The argument subtree differs: `g(process(x))` vs `g(x.trim())`. The inner call subtree is
    // a differing call/method head — the differing-callee guard forces closure_param Low, NEVER
    // a high-confidence value_param (the Plan-3 SCIP seam).
    let a = member(1, "fn f() { let v = g(process(x)); }");
    let b = member(2, "fn f() { let v = g(y.trim()); }");
    let (_members, template) = run(vec![a, b]);

    // There is at least one variation point and the one covering the differing call is a
    // closure_param, not a value_param.
    assert!(!template.variation_points.is_empty(), "must have a variation point");
    let any_closure =
        template.variation_points.iter().any(|vp| vp.kind == MetavarKind::ClosureParam);
    assert!(any_closure, "differing callee subtree must classify as closure_param");
    // The guard: NO variation point covering the differing callee may be a high-confidence
    // value_param.
    for vp in &template.variation_points {
        assert_ne!(
            (vp.kind, vp.confidence),
            (MetavarKind::ValueParam, Confidence::High),
            "differing callee must NOT be a high-confidence value_param"
        );
    }
    // The closure_param bands at most Medium (here Low).
    let closure =
        template.variation_points.iter().find(|vp| vp.kind == MetavarKind::ClosureParam).unwrap();
    assert!(
        matches!(closure.confidence, Confidence::Medium | Confidence::Low),
        "closure_param must band Medium/Low, got {:?}",
        closure.confidence
    );
}

#[test]
fn inserted_statement_is_gapped_type3() {
    // b has an extra statement (`bar();`) the others lack. The inserted statement's `();`
    // tangles the flat token LCS across the `foo();`/`bar();` boundary; the statement-snap
    // (rule 5) must recover the WHOLE inserted statement BALANCED — one gapped metavar whose
    // values are exactly `["bar();", ""]` (ordinal-correct, no leading `(`/`)`/`;` fragment) —
    // and keep `foo();` FIXED in the template (no single-member leak).
    let a = member(1, "fn f() { let a = 1; foo(); }");
    let b = member(2, "fn f() { let a = 1; foo(); bar(); }");
    let (members, template) = run(vec![a, b]);

    // Coverage drops below 1.0 (the inserted statement is non-fixed material).
    assert!(template.anti_unify_coverage < 1.0, "an inserted statement must drop coverage");

    // EXACTLY one variation point, gapped Low.
    assert_eq!(
        template.variation_points.len(),
        1,
        "the inserted statement is one gapped metavar, got {:?}",
        template.variation_points
    );
    let gapped = &template.variation_points[0];
    assert_eq!(gapped.kind, MetavarKind::Gapped);
    assert_eq!(gapped.extraction_role, "gapped");
    assert_eq!(gapped.confidence, Confidence::Low);

    // BALANCED + ORDINAL-CORRECT: exactly one value is the WHOLE statement `bar();` (no
    // `(); bar`-style fragment), the other is the empty gap.
    assert_eq!(gapped.per_member_values.len(), members.len());
    let mut vals = gapped.per_member_values.clone();
    vals.sort();
    assert_eq!(
        vals,
        vec!["".to_string(), "bar();".to_string()],
        "the inserted statement must be recovered whole + balanced, got {:?}",
        gapped.per_member_values
    );
    // Ordinal correctness: the member that HAS bar(); carries `bar();`; the one lacking it
    // gaps.
    for (i, m) in members.iter().enumerate() {
        let has_bar = m.text.contains("bar();");
        assert_eq!(
            gapped.per_member_values[i] == "bar();",
            has_bar,
            "per_member_values[{i}] must align to member symbol_id {} (has_bar={has_bar})",
            m.symbol_id
        );
    }

    // `foo();` (and `let a = 1;`) are FIXED in the template — no single-member content leaks,
    // no `(...)` punctuation fragment renders as fixed text.
    assert!(
        template.text.contains("foo();"),
        "foo(); must be fixed text (no statement-boundary leak), got {:?}",
        template.text
    );
    assert!(
        template.text.contains("let a = 1;"),
        "the shared prefix statement must be fixed, got {:?}",
        template.text
    );
}

#[test]
fn indel_neighbour_call_not_scrambled() {
    // `p(1);` vs `p(1); q(2);` — the inserted `q(2);`'s `(` `)` `;` LCS-matches the neighbour
    // `p(1);`'s, which used to scramble ordinals (member 1's `p(1);` was mislabelled `q(2);`)
    // and leak `(2);` into the fixed template. The statement-snap must keep `p(1);` FIXED in
    // BOTH members and surface `q(2);` as ONE gapped metavar with balanced values
    // `["q(2);",""]`.
    let a = member(1, "fn f(){ p(1); }");
    let b = member(2, "fn f(){ p(1); q(2); }");
    let (members, template) = run(vec![a, b]);

    // Exactly one gapped metavar = the inserted statement.
    assert_eq!(
        template.variation_points.len(),
        1,
        "only the inserted statement varies, got {:?}",
        template.variation_points
    );
    let vp = &template.variation_points[0];
    assert_eq!(vp.kind, MetavarKind::Gapped, "the inserted statement must be gapped");

    // Balanced + ordinal-correct: `q(2);` whole, paired with "". NO member mislabelled, no
    // `(1);`/`(2);` fragment.
    assert_eq!(vp.per_member_values.len(), members.len());
    let mut vals = vp.per_member_values.clone();
    vals.sort();
    assert_eq!(
        vals,
        vec!["".to_string(), "q(2);".to_string()],
        "inserted statement must be whole + balanced (no scramble), got {:?}",
        vp.per_member_values
    );
    for (i, m) in members.iter().enumerate() {
        let has_q = m.text.contains("q(2);");
        assert_eq!(
            vp.per_member_values[i] == "q(2);",
            has_q,
            "per_member_values[{i}] must align to member symbol_id {}",
            m.symbol_id
        );
    }

    // `p(1);` is FIXED text (present in both members) and `(2);` does NOT leak as fixed text.
    assert!(template.text.contains("p(1);"), "p(1); must be fixed, got {:?}", template.text);
    assert!(
        !template.text.contains("(2)"),
        "the inserted statement's `(2);` must NOT leak into fixed text, got {:?}",
        template.text
    );
}

#[test]
fn matched_statements_after_indel_are_anti_unified() {
    // Fix 2 (#215 Plan 4b Codex round-5): after the statement-snap handles a statement-COUNT
    // indel, MATCHED statement pairs that still differ INSIDE must be anti-unified — not left
    // whole-fixed. The OLD path emitted only the gapped (inserted) statement and DROPPED the
    // `1`/`2` change in the matched `p(1)`/`p(2)` (template hard-coded `p(1)`, member-2's `2`
    // missing from per_member_values). The fix RE-DESCENDS each matched statement on its own
    // token sub-range via the existing column pipeline.
    //
    // `p(1); q();` vs `p(2); let z = 0; q();`: the inserted `let z = 0;` is a distinct skeleton
    // (a let_declaration, NOT a bare call), so it does not collide with `q()` — the
    // leftmost-tie residual (a SEPARATE limitation, #235) is sidestepped here.
    // Expected: value_param 1/2 for the matched `p()` + gapped `let z = 0;` + `q()`
    // fixed.
    let a = member(1, "fn f(){ p(1); q(); }");
    let b = member(2, "fn f(){ p(2); let z = 0; q(); }");
    let (members, template) = run(vec![a, b]);

    // The matched `p(N)` statement's inner literal diff is a value_param 1/2 — NOT dropped.
    let value_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.retain(|v| !v.is_empty());
            vals.sort();
            vals == vec!["1".to_string(), "2".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "the matched p() statement's inner 1/2 diff must be anti-unified (Fix 2), got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        value_vp.kind,
        MetavarKind::ValueParam,
        "the inner literal diff in a matched statement must be a value_param, got {:?}",
        value_vp
    );
    assert_eq!(value_vp.confidence, Confidence::High);
    assert_eq!(value_vp.per_member_values.len(), members.len());
    // Ordinal correctness: each member's value is its own literal.
    for (i, m) in members.iter().enumerate() {
        let expected = if m.text.contains("p(1)") { "1" } else { "2" };
        assert_eq!(
            value_vp.per_member_values[i], expected,
            "per_member_values[{i}] must align to member symbol_id {}",
            m.symbol_id
        );
    }

    // The inserted statement is still a gapped metavar.
    let gapped = template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::Gapped)
        .expect("the inserted `let z = 0;` must be a gapped metavar");
    let mut gvals = gapped.per_member_values.clone();
    gvals.sort();
    assert_eq!(
        gvals,
        vec!["".to_string(), "let z = 0;".to_string()],
        "the inserted statement must be whole + balanced, got {:?}",
        gapped.per_member_values
    );

    // The shared `p(` and `q();` are FIXED text — and member-2's `2` is NOT hidden (the old
    // bug: a hard-coded `p(1)` with no hole). The matched p() renders a hole.
    assert!(
        template.text.contains("p(⟨"),
        "the matched p() must render an inner hole (not a hard-coded p(1)), got {:?}",
        template.text
    );
    assert!(template.text.contains("q();"), "q(); must be fixed text, got {:?}", template.text);
    // Coverage reflects BOTH the inner value diff and the inserted statement.
    assert!(
        template.anti_unify_coverage < 1.0,
        "coverage must drop for the inner diff + the indel, got {}",
        template.anti_unify_coverage
    );
}

#[test]
fn matched_statement_differing_callee_after_indel_is_closure_param() {
    // Fix 2 companion: a differing CALLEE inside a matched statement (within an indel block)
    // re-descends to a closure_param with differing_callee=true (the Plan-3 SCIP seam), exactly
    // as it would outside an indel block.
    //
    // `a(); a(); let z = 0;` vs `a(); b(); let z = 0; w();`: the inserted `w();` triggers the
    // statement indel; the matched SECOND statement differs by callee. Alpha-rename numbers by
    // first occurrence, so the second callee is a REUSED id (`a` = ID0) in member a but a FRESH
    // id (`b` = ID1) in member b → the callee column genuinely differs within the matched
    // statement, and the differing-callee guard fires on the re-descended sub-statement.
    let a = member(1, "fn f(){ a(); a(); let z = 0; }");
    let b = member(2, "fn f(){ a(); b(); let z = 0; w(); }");
    let (_members, template) = run(vec![a, b]);

    // The matched second statement's differing callee is a closure_param (NEVER value_param).
    let callee_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.retain(|v| !v.is_empty());
            vals.sort();
            vals == vec!["a".to_string(), "b".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "the matched statement's differing callee must be anti-unified (Fix 2), got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        callee_vp.kind,
        MetavarKind::ClosureParam,
        "a differing callee in a matched statement must be closure_param, got {:?}",
        callee_vp
    );
    assert!(
        callee_vp.differing_callee,
        "the re-descended differing callee must set differing_callee=true, got {:?}",
        callee_vp
    );
    assert_ne!(
        callee_vp.kind,
        MetavarKind::ValueParam,
        "a differing callee must NEVER be a value_param"
    );
    // The inserted `w();` is still gapped.
    assert!(
        template.variation_points.iter().any(|vp| vp.kind == MetavarKind::Gapped),
        "the inserted w(); must be a gapped metavar, got {:?}",
        template.variation_points
    );
}

#[test]
fn indel_sibling_skeleton_tie_is_balanced_but_attribution_is_approximate() {
    // HONEST boundary test for the kind-skeleton-LCS leftmost-tie residual (see the KNOWN
    // LIMITATION note on `align_member_statements`; real fix is GumTree #235). `a(); c();` vs
    // `a(); b(); c();` inserts a MIDDLE statement `b();` whose skeleton (`ID0();`) is identical
    // to its siblings, so the LCS cannot tell which statement the longer member added. The
    // leftmost-tie resolves the match WRONG: anchor stmt 1 `c()` pairs with member stmt 1
    // `b()`, and member stmt 2 `c()` reads as the insert.
    //
    // Codex round-6 INTERACTION: the matched-column callee reopen now fires on that
    // wrongly-matched pair. The Fix-2 matched-statement re-descent anti-unifies `c()` vs `b()`
    // and (correctly, given the wrong upstream pairing) surfaces the differing callee as a
    // closure_param — the very under-reporting the reopen closes. So the output is now TWO VPs:
    // a closure_param `["c","b"]` (the mis-paired callee) + a gapped insert. We deliberately do
    // NOT assert the attribution is correct — this pins the SAFE invariants that DO hold and
    // proves the approximation never corrupts scoring, NOT that the rendered template is ideal.
    let a = member(1, "fn f(){ a(); c(); }");
    let b = member(2, "fn f(){ a(); b(); c(); }");
    let (members, template) = run(vec![a, b]);

    // No panic reaching here. The insert STILL surfaces as a Gapped metavar (Low confidence) —
    // so the gapped-downgrade in confidence_v2/refactorability_v2 always fires regardless of
    // the (mis-attributed) coverage. This is the load-bearing non-blocking guard: the
    // residual can never escalate confidence, only mis-place the hole.
    let gapped = template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::Gapped)
        .expect("the insert must still surface as a Gapped metavar (the scoring downgrade)");
    assert_eq!(
        gapped.confidence,
        Confidence::Low,
        "a Gapped indel is Low confidence (the gapped downgrade always fires), got {:?}",
        gapped.confidence
    );

    // Every VP's per_member_values are BALANCED + ordinal-aligned to members, and every entry
    // is either empty or a CLEAN token/whole statement (ends in `;` for a statement, a
    // bare name for a callee) — no straddled fragment (`(`-prefix), no token scramble,
    // no cross-member leak. The attribution may be wrong, but the VALUES are always
    // safe.
    for vp in &template.variation_points {
        assert_eq!(
            vp.per_member_values.len(),
            members.len(),
            "values must stay ordinal-aligned to members, got {:?}",
            vp.per_member_values
        );
        for v in &vp.per_member_values {
            assert!(
                !v.starts_with('('),
                "no value may be a straddled `(`-prefixed fragment, got {v:?}"
            );
        }
    }
}

#[test]
fn inserted_statement_does_not_spuriously_flag_differing_callee() {
    // `foo();` vs `foo(); extra_tail(99);` — the trailing inserted statement is a member-only
    // insert (anchor is the shorter member). The OLD path keyed the insert's tokens onto
    // `foo`'s call subtree, producing a bogus closure_param with differing_callee=true
    // and an unbalanced `["foo(", "foo(); extra_tail(99"]`. The statement-snap must
    // instead surface the inserted statement as ONE gapped metavar — never a
    // differing-callee closure_param.
    let a = member(1, "fn f(){ foo(); }");
    let b = member(2, "fn f(){ foo(); extra_tail(99); }");
    let (members, template) = run(vec![a, b]);

    // No variation point may be flagged as a differing callee (the spurious flag).
    assert!(
        template.variation_points.iter().all(|vp| !vp.differing_callee),
        "an inserted statement must NOT spuriously flag differing_callee, got {:?}",
        template.variation_points
    );
    // No closure_param either — the inserted statement is gapped, not a closure head.
    assert!(
        template.variation_points.iter().all(|vp| vp.kind != MetavarKind::ClosureParam),
        "an inserted statement must NOT be a closure_param, got {:?}",
        template.variation_points
    );

    // It IS a gapped metavar with balanced values: `extra_tail(99);` whole, paired with "".
    let gapped = template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::Gapped)
        .expect("the inserted statement must be a gapped metavar");
    assert_eq!(gapped.per_member_values.len(), members.len());
    let mut vals = gapped.per_member_values.clone();
    vals.sort();
    assert_eq!(
        vals,
        vec!["".to_string(), "extra_tail(99);".to_string()],
        "the inserted statement must be whole + balanced, got {:?}",
        gapped.per_member_values
    );
    // `foo();` stays fixed; no `foo(`-style unbalanced fragment leaks.
    assert!(template.text.contains("foo();"), "foo(); must be fixed, got {:?}", template.text);
}

#[test]
fn recurrence_collapse_keeps_distinct_roles() {
    // P2 (Codex #6): the recurrence-collapse key must include the classified ROLE +
    // differing_callee, not just (values, snapped_kind). The SAME values `foo`/`bar` can recur
    // once as a call HEAD (closure_param, differing_callee=true) and once as a plain VALUE
    // (value_param). Collapsing on values+kind alone donated the first occurrence's role to the
    // second, mislabelling it. Keying on the role keeps the two metavars distinct.
    //
    // The differing-callee-via-ID-collapse case has no real-parse fixture (a consistent
    // alpha-rename makes `foo`/`bar` identical tokens — see the module differing-callee note),
    // so drive `collapse_recurring` directly with the two candidates the classifier would emit.
    let values = vec!["foo".to_string(), "bar".to_string()];
    let candidates = vec![
        // Occurrence 1: `foo`/`bar` as a call head → closure_param, differing_callee=true.
        RunMetavar {
            lo: 3,
            snapped_kind: "identifier",
            per_member_values: values.clone(),
            kind: MetavarKind::ClosureParam,
            type_hint: None,
            confidence: Confidence::Medium,
            differing_callee: true,
            type_context: None,
        },
        // Occurrence 2: the SAME `foo`/`bar` as a plain value → value_param High.
        RunMetavar {
            lo: 9,
            snapped_kind: "identifier",
            per_member_values: values.clone(),
            kind: MetavarKind::ValueParam,
            type_hint: None,
            confidence: Confidence::High,
            differing_callee: false,
            type_context: None,
        },
    ];
    let vps = collapse_recurring(candidates);

    // Must NOT collapse to one metavar — the roles differ.
    assert_eq!(vps.len(), 2, "same values in different roles must stay TWO metavars, got {vps:?}");
    // Both roles are preserved distinctly (not both donated to the first occurrence's role).
    let callee = vps
        .iter()
        .find(|vp| vp.kind == MetavarKind::ClosureParam)
        .expect("the call-head occurrence must stay a closure_param");
    assert!(callee.differing_callee, "the call-head occurrence keeps differing_callee=true");
    let value = vps
        .iter()
        .find(|vp| vp.kind == MetavarKind::ValueParam)
        .expect("the plain-value occurrence must stay a value_param");
    assert!(!value.differing_callee, "the plain-value occurrence keeps differing_callee=false");
    assert_eq!(value.confidence, Confidence::High, "the value_param keeps its High band");
    // Each carries the same values but its own correct role.
    assert_eq!(callee.per_member_values, values);
    assert_eq!(value.per_member_values, values);
}

#[test]
fn recurring_local_is_one_metavar_with_two_occurrences() {
    // A literal that recurs at two spine positions and co-varies across members collapses to
    // one metavar with two occurrences. `let x = 10; let y = 10;` vs float at both
    // positions.
    let a = member(1, "fn f() { let p = 10; let q = 10; }");
    let b = member(2, "fn f() { let p = 2.5; let q = 2.5; }");
    let (_members, template) = run(vec![a, b]);

    // Exactly one metavar (the two literal columns co-vary identically → collapsed).
    assert_eq!(
        template.variation_points.len(),
        1,
        "co-varying recurring literal must collapse to one metavar, got {:?}",
        template.variation_points
    );
    let vp = &template.variation_points[0];
    assert_eq!(vp.occurrences.len(), 2, "the collapsed metavar must record both occurrences");
    // Occurrences are ascending by spine column.
    assert!(vp.occurrences[0] < vp.occurrences[1], "occurrences must be column-ascending");
}

#[test]
fn differing_subtree_snaps_to_one_metavar_not_per_token() {
    // The differing material is a whole multi-token subtree (`a + b + c` vs `m * n`). It must
    // snap to ONE metavar covering the subtree, not one metavar per differing leaf.
    let a = member(1, "fn f() { let r = a + b + c; }");
    let b = member(2, "fn f() { let r = m * n; }");
    let (_members, template) = run(vec![a, b]);

    // One variation point covering the differing expression subtree (not 3+ per-token holes).
    assert_eq!(
        template.variation_points.len(),
        1,
        "a differing subtree must snap to one metavar, got {} points: {:?}",
        template.variation_points.len(),
        template.variation_points
    );
    let vp = &template.variation_points[0];
    // The recovered values are the whole differing subtrees.
    let mut vals = vp.per_member_values.clone();
    vals.sort();
    assert_eq!(vals, vec!["a + b + c".to_string(), "m * n".to_string()]);
}

#[test]
fn per_member_values_ordinal_aligned_to_sorted_members() {
    // Three members; per_member_values[i] corresponds to the i-th canonical member. Use
    // distinct literal kinds so each member's value is its own real source.
    let a = member(10, "fn f() { let x = 1; }");
    let b = member(20, "fn f() { let x = 2.0; }");
    let c = member(30, "fn f() { let x = 3; }");
    let (members, template) = run(vec![a, b, c]);

    assert_eq!(template.variation_points.len(), 1);
    let vp = &template.variation_points[0];
    assert_eq!(vp.per_member_values.len(), members.len());
    // For each canonical member, its value is the real literal from ITS source.
    for (i, m) in members.iter().enumerate() {
        let lit = m.text.get(..).unwrap();
        // The member's literal is one of "1","2.0","3"; assert the ordinal value is that
        // source.
        let expected = if lit.contains("2.0") {
            "2.0"
        } else if lit.contains("= 1;") {
            "1"
        } else {
            "3"
        };
        assert_eq!(
            vp.per_member_values[i], expected,
            "per_member_values[{i}] must align to member symbol_id {}",
            m.symbol_id
        );
    }
}

#[test]
fn coverage_is_fixed_over_total_and_1_0_for_identical() {
    // Structurally identical members → coverage 1.0, no variation points. Alpha-renaming
    // (`f`/`g`, `x`/`y`) is the intended Type-2 equivalence; the literal VALUE must be the SAME
    // (`10`/`10`) — a differing literal value is now a value_param variation (Fix 2), so it
    // would (correctly) drop coverage below 1.0.
    let a = member(1, "fn f() { let x = 10; sink(x); }");
    let b = member(2, "fn g() { let y = 10; sink(y); }");
    let (_members, template) = run(vec![a, b]);
    assert!(
        (template.anti_unify_coverage - 1.0).abs() < 1e-12,
        "structurally identical members → coverage 1.0, got {}",
        template.anti_unify_coverage
    );
    assert!(
        template.variation_points.is_empty(),
        "identical members → no variation points, got {:?}",
        template.variation_points
    );

    // A single differing column → coverage strictly between 0 and 1.
    let c = member(1, "fn f() { let x = 10; }");
    let d = member(2, "fn f() { let x = 1.5; }");
    let (_m2, t2) = run(vec![c, d]);
    assert!(
        t2.anti_unify_coverage > 0.0 && t2.anti_unify_coverage < 1.0,
        "one differing column → fractional coverage, got {}",
        t2.anti_unify_coverage
    );
}

#[test]
fn metavar_ids_ascending_by_spine_column_deterministic() {
    // Two independent differing literals at different positions → m0 before m1, ascending by
    // spine column. Run twice and assert identical output (determinism).
    let make = || {
        vec![
            member(1, "fn f() { let x = 1; let y = 2; }"),
            member(2, "fn f() { let x = 3.0; let y = true; }"),
        ]
    };
    let (_m1, t1) = run(make());
    let (_m2, t2) = run(make());

    // Two distinct metavars (the two literals don't co-vary identically: int→float vs
    // int→bool).
    assert_eq!(t1.variation_points.len(), 2, "expected two independent metavars");
    // metavar_ids are m0, m1 ascending by spine column.
    assert_eq!(t1.variation_points[0].metavar_id, "m0");
    assert_eq!(t1.variation_points[1].metavar_id, "m1");
    let c0 = t1.variation_points[0].occurrences[0];
    let c1 = t1.variation_points[1].occurrences[0];
    assert!(c0 < c1, "m0 must occupy an earlier spine column than m1");

    // Determinism: same input twice → identical template text + same metavar layout.
    assert_eq!(t1.text, t2.text, "template render must be deterministic");
    assert_eq!(
        t1.variation_points.len(),
        t2.variation_points.len(),
        "variation-point count must be deterministic"
    );
    for (vp1, vp2) in t1.variation_points.iter().zip(t2.variation_points.iter()) {
        assert_eq!(vp1.metavar_id, vp2.metavar_id);
        assert_eq!(vp1.occurrences, vp2.occurrences);
        assert_eq!(vp1.per_member_values, vp2.per_member_values);
    }
}

#[test]
fn wrap_plus_trailing_calls_no_spurious_identical_metavars() {
    // C1: the ONLY real difference is the `g(...)` wrap inside `h(...)`. A stray surplus-insert
    // (an LCS tie peeling `(` etc.) must NOT promote the trailing `k()`/`m()` calls — whose
    // recovered values are byte-identical across members — to spurious metavars.
    let a = member(1, "pub fn a(x: T) -> T { let r = h(g(x)); k(); m() }");
    let b = member(2, "pub fn b(x: T) -> T { let r = h(x); k(); m() }");
    let (_members, template) = run(vec![a, b]);

    // C1 invariant: NO surviving metavar has all-identical (non-gap) per-member values — the
    // `k`/`m` calls (recovered byte-identical across members) must NOT be promoted to holes.
    for vp in &template.variation_points {
        let all_equal = vp.per_member_values.iter().all(|v| *v == vp.per_member_values[0]);
        assert!(
            vp.kind == MetavarKind::Gapped || !all_equal,
            "a non-gapped metavar must have ≥2 distinct values, got {:?}",
            vp
        );
    }

    // Every surviving metavar is the real g-wrap difference: exactly one member's value carries
    // the `g` wrap; the other does not. (No spurious `k`/`m` metavars survive.)
    for vp in &template.variation_points {
        assert!(
            vp.per_member_values.iter().any(|v| v.contains('g')),
            "the only real difference is the g-wrap; spurious metavar survived: {:?}",
            vp
        );
    }

    // `k()`/`m()` are fixed in the template (not rendered as holes) — coverage reflects only
    // the real difference.
    assert!(template.text.contains("k()"), "k() must be fixed text, got {:?}", template.text);
    assert!(template.text.contains("m()"), "m() must be fixed text, got {:?}", template.text);
    assert!(
        template.anti_unify_coverage > 0.0 && template.anti_unify_coverage < 1.0,
        "coverage must reflect the single real difference, got {}",
        template.anti_unify_coverage
    );
}

#[test]
fn c1_guard_ignores_skipped_member_no_spurious_metavar() {
    // C1 + skipped-member residual (the 8th un-gated site, #215 Plan 4b Codex round-4): the C1
    // all-identical-drop must compare only ALIGNED members' values. Reuse the
    // `wrap_plus_trailing_calls` shape — two aligned members whose ONLY real difference is the
    // `g(...)` wrap, with byte-identical trailing `k(); m()` calls — PLUS one synthetic
    // cost-skipped (>LCS_MAX_SEQ_TOKENS) member. The skipped member contributes `""` to the
    // `k`/`m` runs, so the recovered values are `["k","k",""]` / `["m","m",""]`. The OLD
    // (un-gated) C1 check `all(v == values[0])` was FALSE (the skipped `""` differs from `k`),
    // so the spurious `k()`/`m()` ValueParams SURVIVED — inflating params, depressing coverage,
    // un-fixing `k()`/`m()` in the template. With the aligned-only gate they are dropped.
    let cap = super::align::LCS_MAX_SEQ_TOKENS;

    let a = member(1, "pub fn a(x: T) -> T { let r = h(g(x)); k(); m() }");
    let b = member(2, "pub fn b(x: T) -> T { let r = h(x); k(); m() }");
    // Synthetic long member (struct_hash "z" sorts LAST → never the anchor, always skipped).
    let c = synthetic_member(3, "z", cap + 50);

    let members = canonical(vec![a, b, c]);
    let anchor_idx = resolve_anchor_idx(&members, None);
    let alignment = align_to_anchor(&members, anchor_idx);
    let template = anti_unify(&members, &alignment);

    // Precondition: the long member is skipped and the alignment is sampled (honest).
    let long_idx = members.iter().position(|m| m.seq.len() > cap).unwrap();
    assert!(!alignment.aligned[long_idx], "the long member must be excluded from alignment");
    assert!(alignment.sampled, "skipping a long member must set the sampled flag");

    // C1 invariant (the FIX): NO surviving non-gapped metavar is all-identical among ALIGNED
    // members. The `k()`/`m()` runs (recovered byte-identical across the two aligned members)
    // must NOT survive as spurious ValueParams just because the skipped member's `""` differs.
    for vp in &template.variation_points {
        let aligned_all_equal = vp
            .per_member_values
            .iter()
            .enumerate()
            .filter(|&(m, _)| alignment.aligned[m])
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
            .windows(2)
            .all(|w| w[0] == w[1]);
        assert!(
            vp.kind == MetavarKind::Gapped || !aligned_all_equal,
            "a skipped member's \"\" must NOT resurrect an all-identical (among aligned) metavar, \
             got {vp:?}"
        );
    }

    // No spurious `k`/`m` metavar: every surviving VP is the real g-wrap difference (one
    // aligned member carries the `g` wrap, the other does not).
    for vp in &template.variation_points {
        assert!(
            vp.per_member_values.iter().any(|v| v.contains('g')),
            "the only real difference is the g-wrap; a spurious k()/m() metavar survived: {vp:?}"
        );
    }

    // `k()`/`m()` stay FIXED in the template (not rendered as holes).
    assert!(template.text.contains("k()"), "k() must be fixed text, got {:?}", template.text);
    assert!(template.text.contains("m()"), "m() must be fixed text, got {:?}", template.text);

    // Coverage is NOT depressed by spurious holes — it reflects only the single real difference
    // (the g-wrap), exactly as the clean 2-member base case.
    assert!(
        template.anti_unify_coverage > 0.0 && template.anti_unify_coverage < 1.0,
        "coverage must reflect only the real g-wrap difference, got {}",
        template.anti_unify_coverage
    );
}

#[test]
fn statement_head_differing_callee_is_closure_param_not_value() {
    // C2: the 2nd callee differs (`foo` vs `bar`) and surfaces as a bare `identifier` LEAF in
    // call-head position (not the `call_expression` node). The differing-callee guard MUST
    // still fire → closure_param Medium/Low, NEVER a high-confidence value_param.
    let a = member(1, "pub fn a() { foo(); foo() }");
    let b = member(2, "pub fn b() { foo(); bar() }");
    let (_members, template) = run(vec![a, b]);

    assert!(!template.variation_points.is_empty(), "must have a variation point");
    // The differing-callee metavar carries the two callee names.
    let callee_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["bar".to_string(), "foo".to_string()]
        })
        .expect("a metavar covering the differing callee");
    assert_eq!(
        callee_vp.kind,
        MetavarKind::ClosureParam,
        "a differing callee must be closure_param (Plan-3 SCIP seam), got {:?}",
        callee_vp
    );
    assert!(
        matches!(callee_vp.confidence, Confidence::Medium | Confidence::Low),
        "closure_param must band Medium/Low, got {:?}",
        callee_vp.confidence
    );
    // The guard: NO variation point covering the differing callee may be a value_param.
    assert_ne!(
        callee_vp.kind,
        MetavarKind::ValueParam,
        "a differing callee must NEVER be a value_param"
    );
}

#[test]
fn plain_differing_local_stays_value_param() {
    // Confirm the C2 widening does NOT misclassify a non-callee identifier: a plain differing
    // local var (an operand of a binary expression — NOT in call-head position) must stay
    // value_param High. Alpha-renaming numbers identifiers by first occurrence, so the trailing
    // operand differs: member `a` re-uses `x` (ID0), member `b` uses `y` (ID1).
    let a = member(1, "pub fn a() { let r = foo(x, y) + x; }");
    let b = member(2, "pub fn b() { let r = foo(x, y) + y; }");
    let (members, template) = run(vec![a, b]);

    // The differing trailing operand is a value-leaf NOT in call-head position → value_param.
    let local_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["x".to_string(), "y".to_string()]
        })
        .unwrap_or_else(|| {
            panic!("a metavar covering the differing operand, got {:?}", template.variation_points)
        });
    assert_eq!(
        local_vp.kind,
        MetavarKind::ValueParam,
        "a plain differing local operand must stay value_param, got {:?}",
        local_vp
    );
    assert_eq!(local_vp.confidence, Confidence::High);
    assert_eq!(local_vp.per_member_values.len(), members.len());
}

#[test]
fn differing_free_callee_at_matched_column_is_closure_param() {
    // Codex round-6 P2 (the THIRD matched-column reopen position, after literal + type-id):
    // a SINGLE-USE free callee `foo()` vs `bar()` BOTH alpha-rename to the same `ID<n>`, so LCS
    // matches the callee column → the OLD fixedness marked it FIXED → no variation run → the
    // differing-callee guard (which only runs on a VARIATION run) never fires → the template
    // hard-coded the anchor `foo()` with coverage 1.0, silently losing the callee-only clone
    // difference. The matched-column reopen (`matched_column_reopen` → `ReopenRole::Callee`)
    // must flip the column to a variation and route it through the differing-callee guard:
    // ONE closure_param VP, differing_callee=true, per_member_values ["foo","bar"],
    // coverage<1.0 — NOT a hard-coded `foo` at coverage 1.0.
    let a = member(1, "fn a(){ foo(); }");
    let b = member(2, "fn b(){ bar(); }");
    let (members, template) = run(vec![a, b]);

    // EXACTLY one variation point, covering the differing callee.
    assert_eq!(
        template.variation_points.len(),
        1,
        "the differing single-use callee is one variation point, got {:?}",
        template.variation_points
    );
    let callee_vp = &template.variation_points[0];
    assert_eq!(
        callee_vp.kind,
        MetavarKind::ClosureParam,
        "a differing callee at a matched column must be closure_param (Plan-3 SCIP seam), got {:?}",
        callee_vp
    );
    assert!(
        callee_vp.differing_callee,
        "the reopened callee column must set differing_callee=true (route through the guard), got \
         {:?}",
        callee_vp
    );
    assert_ne!(
        callee_vp.kind,
        MetavarKind::ValueParam,
        "a differing callee must NEVER be a value_param"
    );
    // per_member_values carry the two callee names (the reopened source values).
    assert_eq!(callee_vp.per_member_values.len(), members.len());
    let mut vals = callee_vp.per_member_values.clone();
    vals.sort();
    assert_eq!(vals, vec!["bar".to_string(), "foo".to_string()]);
    // Coverage drops below 1.0 — the callee is no longer hard-coded as fixed text.
    assert!(
        template.anti_unify_coverage < 1.0,
        "a differing callee must drop coverage below 1.0 (not the old hard-coded 1.0), got {}",
        template.anti_unify_coverage
    );
    // The anchor callee `foo` is NOT rendered as fixed text — it renders a hole.
    assert!(
        template.text.contains(&format!("⟨{}⟩", callee_vp.metavar_id)),
        "the differing callee must render a hole, not a hard-coded `foo`, got {:?}",
        template.text
    );
}

/// Attach a SCIP callee moniker for the `nth` (0-based) occurrence of `name` in the member's
/// source — the span-exact map `current_callee_monikers` would build in scip refine mode (#275).
fn with_callee_moniker(
    mut member: RefineMember,
    name: &str,
    nth: usize,
    moniker: &str,
) -> RefineMember {
    let mut from = 0;
    let mut start = None;
    for _ in 0..=nth {
        let at = member.text[from..].find(name).expect("callee occurrence present") + from;
        start = Some(at);
        from = at + name.len();
    }
    let start = start.expect("nth occurrence");
    member.callee_monikers.insert((start, start + name.len()), moniker.to_string());
    member
}

#[test]
fn same_moniker_callee_at_matched_column_stays_fixed() {
    // #275 (Plan 3): the SAME fixture as `differing_free_callee_at_matched_column_is_closure_param`
    // — `foo()` vs `bar()` at an LCS-matched callee column — but the oracle proves both spellings
    // resolve to ONE moniker (a moved / aliased / re-exported same function). The reopen must NOT
    // fire: the column stays FIXED (the anchor's spelling renders as template text — finding 4's
    // "a collapsed callee becomes fixed template text"), there is NO variation point, NO
    // `differing_callee`, and coverage stays 1.0 — the Type-2 lift.
    let a = with_callee_moniker(member(1, "fn a(){ foo(); }"), "foo", 0, "rust cr 1.0 m/foo().");
    let b = with_callee_moniker(member(2, "fn b(){ bar(); }"), "bar", 0, "rust cr 1.0 m/foo().");
    let (_members, template) = run(vec![a, b]);

    assert!(
        template.variation_points.is_empty(),
        "a moniker-proven same-symbol callee must not open a variation point, got {:?}",
        template.variation_points
    );
    assert!(
        (template.anti_unify_coverage - 1.0).abs() < 1e-12,
        "a collapsed callee keeps coverage 1.0, got {}",
        template.anti_unify_coverage
    );
    assert!(
        template.text.contains("foo"),
        "the collapsed column renders the anchor's spelling as fixed text, got {:?}",
        template.text
    );
}

#[test]
fn differing_moniker_callee_still_reopens_as_closure_param() {
    // #275: monikers attached but DIFFERENT — genuinely different functions. The collapse must
    // not fire; the reopen routes through the differing-callee guard exactly as with no oracle.
    let a = with_callee_moniker(member(1, "fn a(){ foo(); }"), "foo", 0, "rust cr 1.0 m/foo().");
    let b = with_callee_moniker(member(2, "fn b(){ bar(); }"), "bar", 0, "rust cr 1.0 m/bar().");
    let (_members, template) = run(vec![a, b]);

    assert_eq!(template.variation_points.len(), 1, "got {:?}", template.variation_points);
    let vp = &template.variation_points[0];
    assert_eq!(vp.kind, MetavarKind::ClosureParam);
    assert!(vp.differing_callee, "different monikers keep the differing-callee verdict");
}

#[test]
fn missing_moniker_on_one_member_vetoes_the_collapse() {
    // #275: only ONE member carries oracle evidence (stale coverage for the other file, a drifted
    // member, an unresolved callee). No proof ⇒ no collapse — the conservative reopen stands.
    let a = with_callee_moniker(member(1, "fn a(){ foo(); }"), "foo", 0, "rust cr 1.0 m/foo().");
    let b = member(2, "fn b(){ bar(); }");
    let (_members, template) = run(vec![a, b]);

    assert_eq!(template.variation_points.len(), 1, "got {:?}", template.variation_points);
    let vp = &template.variation_points[0];
    assert_eq!(vp.kind, MetavarKind::ClosureParam);
    assert!(vp.differing_callee, "a member without a moniker must veto the collapse");
}

#[test]
fn same_moniker_callee_in_matched_statement_redescent_is_not_differing() {
    // #275, the VARIATION-RUN site (`run_callees_differ` guard): the
    // `matched_statement_differing_callee_after_indel_is_closure_param` fixture — the matched
    // second statement's callee genuinely differs in token space (reused `a` = ID0 vs fresh `b` =
    // ID1, so LCS makes it a variation run, not a matched column) — but the oracle proves both
    // resolve to ONE moniker. The guard is skipped: the run classifies through the normal ladder
    // (a single `ID` leaf ⇒ value_param), NEVER `differing_callee`. This also pins that the
    // moniker map survives the statement re-descent (the sub-members share the parent's absolute
    // offsets).
    let a = with_callee_moniker(
        member(1, "fn f(){ a(); a(); let z = 0; }"),
        "a",
        // Occurrences of "a" in the source: the fn param list has none; `fn f(){ a(); a(); …` —
        // occurrence 0 is the first callee, 1 the second (the compared statement's callee).
        // Attach BOTH so whichever statement the alignment compares carries evidence.
        0,
        "rust cr 1.0 m/same().",
    );
    let a = with_callee_moniker(a, "a", 1, "rust cr 1.0 m/same().");
    let b = with_callee_moniker(
        member(2, "fn f(){ a(); b(); let z = 0; w(); }"),
        "b",
        0,
        "rust cr 1.0 m/same().",
    );
    // Member b's FIRST callee `a` matches member a's — equal values never reopen, but attach the
    // moniker anyway (the production map covers every resolved call site).
    let b = with_callee_moniker(b, "a", 0, "rust cr 1.0 m/same().");
    let (_members, template) = run(vec![a, b]);

    // The a/b callee variation must NOT be a differing callee any more…
    assert!(
        template.variation_points.iter().all(|vp| !vp.differing_callee),
        "a moniker-proven same-symbol callee must not be flagged differing_callee, got {:?}",
        template.variation_points
    );
    // …it classifies through the ladder as a plain single-leaf value hole instead.
    let collapsed_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.retain(|v| !v.is_empty());
            vals.sort();
            vals == vec!["a".to_string(), "b".to_string()]
        })
        .expect("the a/b callee column still surfaces as a variation point");
    assert_eq!(
        collapsed_vp.kind,
        MetavarKind::ValueParam,
        "the guard-skipped single-leaf run falls through to value_param, got {:?}",
        collapsed_vp
    );
    // The inserted `w();` stays gapped — the collapse never touches indel handling.
    assert!(
        template.variation_points.iter().any(|vp| vp.kind == MetavarKind::Gapped),
        "the inserted w(); must stay a gapped metavar, got {:?}",
        template.variation_points
    );
}

#[test]
fn differing_scoped_path_callee_tail_at_matched_column_is_closure_param() {
    // #235 item 18: `m::foo()` vs `m::bar()` differ only in the FINAL path segment. Both
    // alpha-rename to the same normalized seq (`m`→ID0, `foo`/`bar`→ID1), so LCS matches the
    // callee column. The call's callee child is the `scoped_identifier` `m::foo`, whose final
    // segment `foo` is a plain `identifier` that does NOT share the call's start byte — only
    // the module `m` does. Before the fix this asymmetry meant a differing module
    // reopened but a differing function name did not (coverage 1.0, callee hard-coded);
    // `run_in_callee_position` now recognizes the scoped-path tail, so it reopens like a free
    // callee.
    let a = member(1, "fn a(){ m::foo(); }");
    let b = member(2, "fn b(){ m::bar(); }");
    let (members, template) = run(vec![a, b]);

    assert_eq!(
        template.variation_points.len(),
        1,
        "the differing scoped-path callee tail is one variation point, got {:?}",
        template.variation_points
    );
    let vp = &template.variation_points[0];
    assert_eq!(
        vp.kind,
        MetavarKind::ClosureParam,
        "a differing scoped-path callee tail must be closure_param, got {vp:?}"
    );
    assert!(
        vp.differing_callee,
        "the reopened scoped callee must set differing_callee=true, got {vp:?}"
    );
    assert_eq!(vp.per_member_values.len(), members.len());
    let mut vals = vp.per_member_values.clone();
    vals.sort();
    assert_eq!(vals, vec!["bar".to_string(), "foo".to_string()]);
    assert!(
        template.anti_unify_coverage < 1.0,
        "a differing scoped callee tail must drop coverage below 1.0, got {}",
        template.anti_unify_coverage
    );
    assert!(
        template.text.contains(&format!("⟨{}⟩", vp.metavar_id)),
        "the differing callee tail must render a hole, got {:?}",
        template.text
    );
}

#[test]
fn differing_method_name_at_matched_column_is_closure_param() {
    // Companion to the free-callee reopen: a differing METHOD-NAME head at a matched column
    // (`x.foo()` vs `x.bar()`, same receiver) must reopen as a closure_param differing_callee
    // too — the method name is the callee head, not a value. The receiver `x` is alpha-renamed
    // identically (same ID), so only the method-name column genuinely differs; both `foo`/`bar`
    // method names alpha-rename to the same `ID<n>` field name → matched/fixed without the
    // reopen. `run_in_callee_position` recognises the `field_identifier` method-name head.
    let a = member(1, "fn a(){ let x = q(); x.foo(); }");
    let b = member(2, "fn b(){ let x = q(); x.bar(); }");
    let (members, template) = run(vec![a, b]);

    // The differing method-name head is a closure_param differing_callee VP carrying foo/bar.
    let callee_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.retain(|v| !v.is_empty());
            vals.sort();
            vals == vec!["bar".to_string(), "foo".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "a metavar covering the differing method name foo/bar, got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        callee_vp.kind,
        MetavarKind::ClosureParam,
        "a differing method-name head at a matched column must be closure_param, got {:?}",
        callee_vp
    );
    assert!(
        callee_vp.differing_callee,
        "the reopened method-name head must set differing_callee=true, got {:?}",
        callee_vp
    );
    assert_ne!(
        callee_vp.kind,
        MetavarKind::ValueParam,
        "a differing method name must NEVER be a value_param"
    );
    assert_eq!(callee_vp.per_member_values.len(), members.len());
}

#[test]
fn differing_macro_name_at_matched_column_is_closure_param() {
    // Codex round-6 audit: a differing MACRO name (`foo!()` vs `bar!()`) is covered by the
    // callee branch — the differing-callee guard treats `macro_invocation` as a call head and a
    // macro callee leaf in head position satisfies `run_in_callee_position`. The macro name
    // must reopen as a closure_param differing_callee (never a value_param / never
    // hard-coded `foo`), exactly like a free-fn callee. (`assert_eq!`-shaped macros
    // would carry extra structure; `foo!()` keeps the test focused on the bare
    // macro-name head.)
    let a = member(1, "fn a(){ foo!(); }");
    let b = member(2, "fn b(){ bar!(); }");
    let (_members, template) = run(vec![a, b]);

    assert!(!template.variation_points.is_empty(), "must have a variation point");
    // No variation point covering the differing macro name may be a value_param.
    for vp in &template.variation_points {
        assert_ne!(
            vp.kind,
            MetavarKind::ValueParam,
            "a differing macro name must NEVER be a value_param, got {vp:?}"
        );
    }
    // The differing macro name surfaces as a closure_param with differing_callee=true.
    let callee_vp = template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::ClosureParam && vp.differing_callee)
        .unwrap_or_else(|| {
            panic!(
                "a closure_param differing_callee metavar for the macro name, got {:?}",
                template.variation_points
            )
        });
    // The recovered values include the two macro names.
    assert!(
        callee_vp.per_member_values.iter().any(|v| v.contains("foo"))
            && callee_vp.per_member_values.iter().any(|v| v.contains("bar")),
        "the macro-name hole must carry foo/bar, got {:?}",
        callee_vp.per_member_values
    );
}

#[test]
fn same_callee_stays_fixed() {
    // Reopen NEGATIVE (the C1 all-identical drop composes): the SAME callee in both members
    // (`foo()` / `foo()`) recovers byte-equal source values, so `matched_column_reopen` returns
    // None (nothing to reopen) → the column stays fixed → no VP, coverage 1.0. The reopen must
    // not over-fire on equal callees.
    let a = member(1, "fn a(){ foo(); }");
    let b = member(2, "fn b(){ foo(); }");
    let (_members, template) = run(vec![a, b]);
    assert!(
        template.variation_points.is_empty(),
        "the same callee in both members → no variation point, got {:?}",
        template.variation_points
    );
    assert!(
        (template.anti_unify_coverage - 1.0).abs() < 1e-12,
        "the same callee → coverage 1.0, got {}",
        template.anti_unify_coverage
    );
}

#[test]
fn value_position_local_not_reopened_as_callee() {
    // Reopen NEGATIVE — scope guard: a differing VALUE-position local (NOT in callee position)
    // must NOT be reopened as a callee. The matched-column callee reopen is scoped to
    // callee/method-name positions only; a consistently alpha-renamed value local is a Type-2
    // equivalent (no VP), and a differing value-position operand is a value_param (via the
    // normal variation-run path), NEVER a closure_param callee.
    //
    // (a) consistently alpha-renamed value local (`x`→`y`, both `ID0`) at a matched column: the
    //     callee reopen must NOT fire (it is not a callee leaf) → no VP, coverage 1.0.
    let a = member(1, "fn f() { let x = 10; sink(x); }");
    let b = member(2, "fn g() { let y = 10; sink(y); }");
    let (_members, template) = run(vec![a, b]);
    assert!(
        template.variation_points.is_empty(),
        "a consistently alpha-renamed value local must NOT be reopened as a callee, got {:?}",
        template.variation_points
    );
    assert!(
        (template.anti_unify_coverage - 1.0).abs() < 1e-12,
        "alpha-renamed value local → coverage 1.0, got {}",
        template.anti_unify_coverage
    );

    // (b) a differing value-position operand (`+ x` vs `+ y`, a binary operand NOT in call-head
    //     position) stays value_param High — never reopened/misrouted to a closure_param
    // callee.
    let c = member(1, "pub fn a() { let r = foo(x, y) + x; }");
    let d = member(2, "pub fn b() { let r = foo(x, y) + y; }");
    let (_m2, t2) = run(vec![c, d]);
    let local_vp = t2
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["x".to_string(), "y".to_string()]
        })
        .unwrap_or_else(|| {
            panic!("a metavar covering the differing operand, got {:?}", t2.variation_points)
        });
    assert_eq!(
        local_vp.kind,
        MetavarKind::ValueParam,
        "a differing value-position operand must stay value_param, got {:?}",
        local_vp
    );
    assert!(
        !local_vp.differing_callee,
        "a value-position operand must NOT be flagged differing_callee, got {:?}",
        local_vp
    );
}

#[test]
fn custom_type_hole_is_type_param() {
    // P2a: a differing CUSTOM type in type position. A `type_identifier` normalizes to `ID<n>`
    // (is_identifier_kind matches `*identifier`), so without the type-position guard it would
    // be promoted to a value_param leaf in classify_run's step (2) BEFORE the
    // type_param check (3). The fix gates step (2) on `!is_type_position(anchor_kind)`
    // so a type-position leaf falls through to type_param.
    //
    // Forcing a type-name variation column: a custom type alpha-renames to `ID<n>`, so a single
    // `let x: Foo` vs `let x: Bar` produces NO column (both → the same positional `ID`). Re-use
    // a prior type in member A but introduce a NEW type in member B: at the SECOND `let`,
    // member A re-uses `T`'s id while member B gets `U`'s fresh id → a differing column
    // at the second type position (per-member values `T`/`U`).
    let a = member(1, "fn f() { let a: T = id(); let b: T = id(); }");
    let b = member(2, "fn g() { let a: T = id(); let b: U = id(); }");
    let (_members, template) = run(vec![a, b]);

    // The hole covering the differing custom type name carries `T`/`U` and is type_param.
    let type_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["T".to_string(), "U".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "a metavar covering the differing type name, got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        type_vp.kind,
        MetavarKind::TypeParam,
        "a custom type-position hole must be type_param, not value_param, got {:?}",
        type_vp
    );
}

#[test]
fn outer_type_node_is_type_param() {
    // Fix 4 (#215 Plan 4b Codex round-5): a variation that snaps to an OUTER composite type
    // node (`reference_type` `&Foo` / `generic_type` `Box<Foo>` / `array_type` /
    // `tuple_type` / …) must classify as type_param. The OLD `is_type_position`
    // enumerated only the bare-name kinds (`type_identifier`/`generic_type`/
    // `scoped_type_identifier`/`primitive_type`), so an outer `reference_type` fell
    // through to `closure_param` (a wrong, opaque `impl Fn()` slot). The shared
    // `is_rust_type_kind` predicate now covers the full set for BOTH the anti-unify
    // classifier and the signature recoverer.
    //
    // `&Foo` is a `reference_type`; `Box<Foo>` is a `generic_type`. The outer node STRUCTURES
    // differ across members, so the whole type annotation snaps to one metavar at the outer
    // type node — exactly the position the old enumeration mishandled for `reference_type`.
    let a = member(1, "fn f() { let x: &Foo = g(); }");
    let b = member(2, "fn g() { let x: Box<Foo> = g(); }");
    let (_members, template) = run(vec![a, b]);

    // The hole covering the differing outer type is type_param, NOT closure_param.
    let type_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["&Foo".to_string(), "Box<Foo>".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "a metavar covering the differing outer type, got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        type_vp.kind,
        MetavarKind::TypeParam,
        "an outer type node (&Foo / Box<Foo>) must be type_param, not closure_param, got {:?}",
        type_vp
    );
    // NEVER a closure_param (the old misclassification).
    assert_ne!(
        type_vp.kind,
        MetavarKind::ClosureParam,
        "an outer type node must NOT fall through to closure_param"
    );
}

#[test]
fn differing_type_identifier_is_type_param() {
    // Fix 1 (#215 Plan 4b Codex round-5): a differing TYPE NAME in type position. A custom type
    // `type_identifier` alpha-renames to `ID<n>` exactly like a value local, so `let x: Foo` vs
    // `let x: Bar` produce IDENTICAL tokens → LCS matches the column → the OLD fixedness marked
    // it FIXED → the template hard-coded the anchor's `Foo` with NO per_member_values for
    // `Bar`. The fix flips a matched TYPE-POSITION-identifier column whose RECOVERED
    // source values differ to a VARIATION; it then classifies as type_param (rendered
    // as a generic), NOT value_param — type names are NOT consistently-alpha-renamed
    // Type-2 equivalents.
    let a = member(1, "fn f(){ let x: Foo = g(); }");
    let b = member(2, "fn f(){ let x: Bar = g(); }");
    let (members, template) = run(vec![a, b]);

    // EXACTLY one variation point: the differing type name, carrying Foo/Bar.
    let type_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["Bar".to_string(), "Foo".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "a metavar covering the differing type name, got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        type_vp.kind,
        MetavarKind::TypeParam,
        "a differing type-position identifier must be type_param, not value_param, got {:?}",
        type_vp
    );
    assert_ne!(
        type_vp.kind,
        MetavarKind::ValueParam,
        "a type name is NOT a value_param (consistent alpha-rename is Type-2 equivalence; a \
         differing TYPE name is a type_param)"
    );
    assert_eq!(type_vp.per_member_values.len(), members.len());
    // The varying type is rendered as a generic in the proposed signature.
    let anchor_idx = resolve_anchor_idx(&members, None);
    let sig = super::super::signature::propose_signature(&template, &members, anchor_idx);
    assert!(
        sig.generic_params.iter().any(|g| g == "T0"),
        "the differing type name must be promoted to a generic, got {:?}",
        sig.generic_params
    );
    // Coverage drops below 1.0 (the type name is now non-fixed material).
    assert!(
        template.anti_unify_coverage < 1.0,
        "a differing type name must drop coverage below 1.0, got {}",
        template.anti_unify_coverage
    );
}

#[test]
fn value_position_local_not_spuriously_type_param() {
    // Fix 1 NEGATIVE: the matched-column type-identifier flip is SCOPED STRICTLY to type
    // positions. A value-position local consistently alpha-renamed across members
    // (`x` → `y`, same value `10`) is a Type-2 equivalent — it must NOT become any variation
    // point at all. The flip gates on the leaf's NODE KIND (`is_type_position`), which is
    // `identifier` (not a type position) for a value local, so it is never flipped; the column
    // stays fixed.
    let a = member(1, "fn f() { let x = 10; sink(x); }");
    let b = member(2, "fn g() { let y = 10; sink(y); }");
    let (_members, template) = run(vec![a, b]);

    // Alpha-rename equivalence: no variation points, coverage 1.0. The value local is NEVER
    // promoted to a type_param (nor any other VP) by the type-identifier flip.
    assert!(
        template.variation_points.is_empty(),
        "a consistently alpha-renamed value local must NOT become a variation point, got {:?}",
        template.variation_points
    );
    assert!(
        (template.anti_unify_coverage - 1.0).abs() < 1e-12,
        "alpha-renamed value locals → coverage 1.0, got {}",
        template.anti_unify_coverage
    );
}

/// Build a synthetic `RefineMember` whose `node_spans` model a `method_call_expression`
/// `recv.name(arg)` so the P2b guard can be exercised directly. Rust/TS tree-sitter actually
/// emit `call_expression` + `field_expression`/`member_expression` for method calls, so the
/// `method_call_expression` branch of `run_in_callee_position` (the over-broad one C2 widened)
/// has no real-parse fixture — this synthesizes the node shape the branch reasons about.
/// Columns: 0 method_call_expression, 1 recv (identifier), 2 name (field_identifier),
/// 3 arguments, 4 arg (identifier). Bytes pin `recv.name(arg)`.
fn synthetic_method_call() -> RefineMember {
    // bytes:  recv=0..4 "recv"  .=4..5  name=5..9 "name"  (=9..10  arg=10..13 "arg"  )=13..14
    let node_spans = vec![
        NodeSpan { start_byte: 0, end_byte: 14, kind: "method_call_expression", is_leaf: false },
        NodeSpan { start_byte: 0, end_byte: 4, kind: "identifier", is_leaf: true },
        NodeSpan { start_byte: 5, end_byte: 9, kind: "field_identifier", is_leaf: true },
        NodeSpan { start_byte: 9, end_byte: 14, kind: "arguments", is_leaf: false },
        NodeSpan { start_byte: 10, end_byte: 13, kind: "identifier", is_leaf: true },
    ];
    let seq = vec![
        "method_call_expression".to_string(),
        "ID0".to_string(),
        "ID1".to_string(),
        "arguments".to_string(),
        "ID2".to_string(),
    ];
    RefineMember {
        callee_monikers: Default::default(),
        symbol_id: 1,
        lang: Language::Rust,
        struct_hash: "synthetic".to_string(),
        seq,
        node_spans,
        text: Arc::from("recv.name(arg)"),
    }
}

#[test]
fn method_call_differing_arg_is_value_param_not_closure() {
    // P2b: the method-call callee guard was too broad — it treated ANY differing identifier
    // inside a `method_call_expression` as a differing callee, so the ARGUMENT of `obj.map(x)`
    // vs `obj.map(y)` was misclassified as a closure_param. The fix restricts the
    // `method_call_expression` branch to the METHOD-NAME head.

    // (a) Synthetic-spans unit check: the ARGUMENT leaf (column 4, inside `arguments`) is NOT a
    // callee position; the METHOD-NAME head (column 2) IS.
    let m = synthetic_method_call();
    assert!(
        !super::run_in_callee_position(&m, 4, 4),
        "a method-call ARGUMENT must NOT count as a callee position"
    );
    assert!(
        super::run_in_callee_position(&m, 2, 2),
        "the method-NAME head must count as a callee position"
    );

    // (b) Real-Rust end-to-end: a differing trailing method-call argument (`a.map(x, y, x)` vs
    // `a.map(x, y, y)`) classifies the differing arg as a value_param High, never a
    // closure_param. (Rust emits `call_expression` for this, but the property — args stay
    // value_param — is the same one the synthetic guard pins for `method_call_expression`.)
    let a = member(1, "fn f() { a.map(x, y, x); }");
    let b = member(2, "fn g() { a.map(x, y, y); }");
    let (_members, template) = run(vec![a, b]);
    let arg_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["x".to_string(), "y".to_string()]
        })
        .unwrap_or_else(|| {
            panic!("a metavar covering the differing arg, got {:?}", template.variation_points)
        });
    assert_eq!(
        arg_vp.kind,
        MetavarKind::ValueParam,
        "a differing method-call argument must stay value_param, got {:?}",
        arg_vp
    );
    assert_eq!(arg_vp.confidence, Confidence::High);
}

#[test]
fn literal_value_difference_is_value_param() {
    // Fix 2: members differing only in a SAME-KIND literal VALUE (`let x = 10` vs `let x = 20`)
    // normalize to identical `LIT_INTEGER_LITERAL` tokens, so LCS matches the literal column
    // and the OLD fixedness marked it FIXED → the template hard-coded the anchor's 10
    // with NO per_member_values for 20. The fix: a value-erased literal column whose
    // RECOVERED source values differ is a VARIATION (a value_param), not fixed.
    let a = member(1, "fn a() -> i32 { let x = 10; x }");
    let b = member(2, "fn b() -> i32 { let x = 20; x }");
    let (members, template) = run(vec![a, b]);

    // Exactly one variation point: the differing literal.
    let lit_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["10".to_string(), "20".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "a value_param metavar with per_member_values [10, 20], got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        lit_vp.kind,
        MetavarKind::ValueParam,
        "an erased same-kind literal value difference must be a value_param, got {:?}",
        lit_vp
    );
    // Uniform integer literal → type_hint is the LIT bucket (keeps the typedness path).
    assert_eq!(lit_vp.type_hint.as_deref(), Some("LIT_INTEGER_LITERAL"));
    assert_eq!(lit_vp.per_member_values.len(), members.len());
    // The template renders a hole where the literal was, and coverage is < 1.0 (NOT the old
    // 1.0).
    assert!(
        template.text.contains(&format!("⟨{}⟩", lit_vp.metavar_id)),
        "template must render the hole, got {:?}",
        template.text
    );
    assert!(
        template.anti_unify_coverage > 0.0 && template.anti_unify_coverage < 1.0,
        "a now-variation literal column must drop coverage below 1.0, got {}",
        template.anti_unify_coverage
    );
}

#[test]
fn string_literal_value_difference_is_value_param() {
    // Fix 2 + Fix 3, string-literal variant: two different string literals (same kind) →
    // value_param. tree-sitter Rust models a string literal as `string_literal` →
    // `string_content`; the value-erased leaf is the inner content (`hello`/`world`). Fix 3
    // WIDENS the hole to the enclosing `string_literal` so per_member_values include the QUOTES
    // (`"hello"`/`"world"`) and the template renders a bare `⟨m0⟩` — a valid `&str`
    // substitution (the old narrow `hello` hole produced `"⟨m0⟩"`, invalid because the
    // quotes sat outside).
    let a = member(1, r#"fn a() { let s = "hello"; sink(s); }"#);
    let b = member(2, r#"fn b() { let s = "world"; sink(s); }"#);
    let (_members, template) = run(vec![a, b]);

    let str_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["\"hello\"".to_string(), "\"world\"".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "a value_param metavar with the two WHOLE string literals (quotes included), got \
                 {:?}",
                template.variation_points
            )
        });
    assert_eq!(str_vp.kind, MetavarKind::ValueParam);
    assert_eq!(str_vp.type_hint.as_deref(), Some("LIT_STRING_CONTENT"));
}

#[test]
fn string_content_hole_widens_to_string_literal() {
    // Fix 3 (#215 Plan 4b Codex round-5): a differing `string_content` leaf must widen its hole
    // to the enclosing `string_literal` node. The OLD narrow hole covered only the inner text
    // (`hello`), leaving the `"` quotes as FIXED template text → `let s = "⟨m0⟩";` with
    // `arg0: &str` — INVALID Rust (substituting a `&str` VALUE there double-quotes it). After
    // widening the hole is the WHOLE `"hello"`: template `let s = ⟨m0⟩;`, per_member_values
    // WITH quotes, NO bare `"⟨m0⟩"`, so `arg0: &str` is a valid value substitution.
    let a = member(1, r#"fn a() { let s = "hello"; }"#);
    let b = member(2, r#"fn b() { let s = "world"; }"#);
    let (members, template) = run(vec![a, b]);

    let str_vp = template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::ValueParam)
        .unwrap_or_else(|| {
            panic!("a value_param metavar for the string hole, got {:?}", template.variation_points)
        });

    // per_member_values are the WHOLE string literals (quotes INCLUDED).
    let mut vals = str_vp.per_member_values.clone();
    vals.sort();
    assert_eq!(
        vals,
        vec!["\"hello\"".to_string(), "\"world\"".to_string()],
        "per_member_values must be the whole `\"hello\"`/`\"world\"` (quotes included), got {:?}",
        str_vp.per_member_values
    );
    assert_eq!(str_vp.per_member_values.len(), members.len());

    // The template renders a BARE hole — NO surrounding quotes (no `"⟨m0⟩"`).
    let label = format!("⟨{}⟩", str_vp.metavar_id);
    assert!(
        template.text.contains(&label),
        "template must render the hole, got {:?}",
        template.text
    );
    assert!(
        !template.text.contains(&format!("\"{label}\"")),
        "the quotes must be INSIDE the hole, not fixed text around it (no `\"⟨m0⟩\"`), got {:?}",
        template.text
    );

    // The signature recovers `arg0: &str` (the LIT_STRING_CONTENT bucket → &str), a valid value
    // substitution.
    assert_eq!(str_vp.type_hint.as_deref(), Some("LIT_STRING_CONTENT"));
    let anchor_idx = resolve_anchor_idx(&members, None);
    let sig = super::super::signature::propose_signature(&template, &members, anchor_idx);
    assert!(
        sig.params.iter().any(|p| p.type_text.as_deref() == Some("&str")),
        "the widened string hole must recover `&str`, got {:?}",
        sig.params
    );
}

#[test]
fn same_literal_value_stays_fixed_no_metavar() {
    // Fix 2 NEGATIVE: the SAME literal in both members (`let x = 10` / `let x = 10`) stays
    // fixed (recovered values are byte-equal) — no metavar, coverage 1.0. The
    // fixedness/C1 path handles it; the literal-value extension must not over-fire on
    // equal values.
    let a = member(1, "fn a() { let x = 10; sink(x); }");
    let b = member(2, "fn b() { let x = 10; sink(x); }");
    let (_members, template) = run(vec![a, b]);
    assert!(
        template.variation_points.is_empty(),
        "identical literal values → no variation point, got {:?}",
        template.variation_points
    );
    assert!(
        (template.anti_unify_coverage - 1.0).abs() < 1e-12,
        "identical members → coverage 1.0, got {}",
        template.anti_unify_coverage
    );
}

#[test]
fn ts_template_literal_hole_widens_to_whole_template_string() {
    // #254: a differing `string_fragment` inside a TS/JS `template_string` (`` `hi` `` vs
    // `` `lo` ``) must widen its hole to the WHOLE `` `hi` `` (backticks included), exactly
    // like the `"hello"`/`"world"` `string` case. Before the fix, `is_string_node_kind` knew
    // only `string_literal`/`string`, so the enclosing `template_string` was not recognised and
    // the backticks rendered as FIXED text around a bare-fragment hole (`` `⟨m0⟩` ``). After:
    // the hole is the whole literal → bare `⟨m0⟩`, per_member_values carry the backticks,
    // ValueParam High &str.
    let a = member_ts(1, "function a() { const s = `hi`; sink(s); }");
    let b = member_ts(2, "function b() { const s = `lo`; sink(s); }");
    let (members, template) = run(vec![a, b]);

    let str_vp = template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::ValueParam)
        .unwrap_or_else(|| {
            panic!(
                "a value_param metavar for the template-literal hole, got {:?}",
                template.variation_points
            )
        });

    // per_member_values are the WHOLE template literals (backticks INCLUDED).
    let mut vals = str_vp.per_member_values.clone();
    vals.sort();
    assert_eq!(
        vals,
        vec!["`hi`".to_string(), "`lo`".to_string()],
        "per_member_values must be the whole `` `hi` ``/`` `lo` `` (backticks included), got {:?}",
        str_vp.per_member_values
    );
    assert_eq!(str_vp.per_member_values.len(), members.len());
    assert_eq!(str_vp.type_hint.as_deref(), Some("LIT_STRING_CONTENT"));

    // The template renders a BARE hole — the backticks are INSIDE it, NOT fixed text around it
    // (no `` `⟨m0⟩` ``).
    let label = format!("⟨{}⟩", str_vp.metavar_id);
    assert!(
        template.text.contains(&label),
        "template must render the hole, got {:?}",
        template.text
    );
    assert!(
        !template.text.contains(&format!("`{label}`")),
        "the backticks must be INSIDE the hole, not fixed text around it (no `` `⟨m0⟩` ``), got \
         {:?}",
        template.text
    );
}

#[test]
fn ts_template_literal_interpolation_is_not_swallowed_by_widen() {
    // #254 CAUTION: an INTERPOLATED template (`` `hi${x}lo` `` vs `` `aa${x}bb` ``) carries a
    // `template_substitution` child. Widening a fragment run to the WHOLE `template_string`
    // there would SWALLOW the `${x}` interpolation into the hole — an over-widen. The widen
    // gate must REFUSE to cross the substitution: the `${x}` stays FIXED template text
    // and each differing fragment is its own (un-widened, bare-fragment) hole. Never an
    // over-claim across the interpolation boundary.
    let a = member_ts(1, "function a() { const s = `hi${x}lo`; sink(s); }");
    let b = member_ts(2, "function b() { const s = `aa${x}bb`; sink(s); }");
    let (_members, template) = run(vec![a, b]);

    // The `${x}` interpolation survives verbatim in the template (NOT inside any hole).
    assert!(
        template.text.contains("${x}"),
        "the `${{x}}` interpolation must stay fixed, not be swallowed into a hole, got {:?}",
        template.text
    );

    // The two differing fragments surface as their OWN holes with the fragment values — the
    // widen did NOT collapse the whole template into one hole spanning the interpolation.
    let all_vals: Vec<String> = template
        .variation_points
        .iter()
        .flat_map(|vp| vp.per_member_values.iter().cloned())
        .collect();
    assert!(
        all_vals.iter().all(|v| !v.contains("${")),
        "no per-member value may contain the interpolation `${{` (widen must stop at it), got \
         {all_vals:?}",
    );
    for v in ["hi", "lo", "aa", "bb"] {
        assert!(
            all_vals.iter().any(|val| val == v),
            "fragment {v:?} must surface as its own hole value, got {all_vals:?}",
        );
    }
}

#[test]
fn empty_string_vs_nonempty_widens_no_stray_quote() {
    // #274 item 16: an empty `""` has NO `string_content` leaf — only the two `"` quote leaves
    // — so a `""`-vs-`"x"` diff surfaces the variation run on a QUOTE leaf, not a
    // `string_content`. Before the fix, `widen_string_content_run` gated only on the body leaf,
    // so the quote run was NOT widened: the template rendered a stray trailing `"`
    // (`let s = ⟨m0⟩";`) and the hole classified ClosureParam Low with values `["\"", "\"x"]`.
    // After: the quote run widens to the whole `string_literal` → bare `⟨m0⟩`, values are the
    // WHOLE `""`/`"x"`, ValueParam High &str. Distinct from #254 (this is the empty-literal
    // delimiter run, not the template-string node kind).
    let a = member(1, "fn a() { let s = \"\"; sink(s); }");
    let b = member(2, "fn b() { let s = \"x\"; sink(s); }");
    let (members, template) = run(vec![a, b]);

    let str_vp = template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::ValueParam)
        .unwrap_or_else(|| {
            panic!(
                "the empty-vs-nonempty string hole must be a value_param, got {:?}",
                template.variation_points
            )
        });

    // per_member_values are the WHOLE literals (quotes included), NOT the broken `["\"",
    // "\"x"]`.
    let mut vals = str_vp.per_member_values.clone();
    vals.sort();
    assert_eq!(
        vals,
        vec!["\"\"".to_string(), "\"x\"".to_string()],
        "per_member_values must be the whole `\"\"`/`\"x\"` (quotes included), got {:?}",
        str_vp.per_member_values
    );
    assert_eq!(str_vp.per_member_values.len(), members.len());
    assert_eq!(str_vp.type_hint.as_deref(), Some("LIT_STRING_CONTENT"));

    // NO stray quote: the template renders a BARE hole with no `"` adjacent to the label.
    let label = format!("⟨{}⟩", str_vp.metavar_id);
    assert!(
        template.text.contains(&label),
        "template must render the hole, got {:?}",
        template.text
    );
    assert!(
        !template.text.contains(&format!("{label}\""))
            && !template.text.contains(&format!("\"{label}")),
        "no stray quote may sit adjacent to the hole (the quotes are INSIDE it), got {:?}",
        template.text
    );
}

/// Build a synthetic `RefineMember` with `token_count` parallel leaf tokens/spans. Mirrors the
/// `long_member` helper in cache.rs — used to exceed [`super::align::LCS_MAX_SEQ_TOKENS`]
/// without parsing a multi-thousand-token real source.
fn synthetic_member(symbol_id: i64, struct_hash: &str, token_count: usize) -> RefineMember {
    let seq: Vec<String> = (0..token_count).map(|i| format!("t{i}")).collect();
    let text: String = (0..token_count).map(|_| "x ").collect();
    let node_spans: Vec<NodeSpan> = (0..token_count)
        .map(|i| NodeSpan {
            start_byte: i * 2,
            end_byte: i * 2 + 1,
            kind: "identifier",
            is_leaf: true,
        })
        .collect();
    RefineMember {
        callee_monikers: Default::default(),
        symbol_id,
        lang: Language::Rust,
        struct_hash: struct_hash.to_string(),
        seq,
        node_spans,
        text: Arc::from(text.as_str()),
    }
}

#[test]
fn p1_long_member_is_skipped_and_sampled_no_huge_dp() {
    // P1 (OOM guard): a class with one SHORT anchor + one member whose seq EXCEEDS
    // LCS_MAX_SEQ_TOKENS must refine WITHOUT calling exact lcs_align on the long pair (which
    // would allocate the (n+1)·(m+1) DP table = hundreds of MB). The long member is SKIPPED
    // from the alignment (excluded from the aligned set) and the alignment is marked sampled.
    let cap = super::align::LCS_MAX_SEQ_TOKENS;
    // Anchor sorts FIRST canonically (struct_hash "a" < "b") and is short → spine is bounded.
    let short = synthetic_member(1, "a", 8);
    let long = synthetic_member(2, "b", cap + 50);
    let members = canonical(vec![short, long]);
    let anchor_idx = resolve_anchor_idx(&members, None);
    // The anchor must be the short member (bounded spine) — the long one is the skipped member.
    assert!(members[anchor_idx].seq.len() <= cap, "anchor spine must be bounded");

    // Time-box the alignment: if it allocated the GB DP this would hang. A bounded skip is
    // effectively instant.
    let start = std::time::Instant::now();
    let alignment = align_to_anchor(&members, anchor_idx);
    let template = anti_unify(&members, &alignment);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "long-member refine must be fast (no huge DP alloc), took {elapsed:?}"
    );

    // The cost guard fired → sampled flag set.
    assert!(alignment.sampled, "skipping a long member must set the sampled flag");
    // The long member is excluded from the aligned set; the short anchor stays aligned.
    let long_idx = members.iter().position(|m| m.seq.len() > cap).unwrap();
    assert!(!alignment.aligned[long_idx], "the long member must be excluded from alignment");
    assert!(alignment.aligned[anchor_idx], "the anchor stays aligned");
    // Excluding the long member, the only aligned member is the anchor → no genuine variation;
    // coverage is conservative (the skipped member can't manufacture spurious gaps).
    assert!(
        (template.anti_unify_coverage - 1.0).abs() < 1e-12,
        "a single aligned member → fixed spine, coverage 1.0, got {}",
        template.anti_unify_coverage
    );
}

#[test]
fn p1_degraded_anchor_when_anchor_seq_too_long() {
    // P1 (OOM guard): when the ANCHOR seq itself exceeds LCS_MAX_SEQ_TOKENS the template can't
    // be computed bounded at all → DEGRADE: every non-anchor member is skipped, no
    // exact lcs_align is run, the result is an empty-variation-point template with the
    // sampled flag set.
    let cap = super::align::LCS_MAX_SEQ_TOKENS;
    // Both long; the canonical-first member is the (long) anchor.
    let a = synthetic_member(1, "a", cap + 30);
    let b = synthetic_member(2, "a", cap + 40);
    let members = canonical(vec![a, b]);
    let anchor_idx = resolve_anchor_idx(&members, None);
    assert!(members[anchor_idx].seq.len() > cap, "anchor spine exceeds the cap (degraded)");

    let start = std::time::Instant::now();
    let alignment = align_to_anchor(&members, anchor_idx);
    let template = anti_unify(&members, &alignment);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "degraded-anchor refine must be fast (no huge DP alloc), took {elapsed:?}"
    );

    assert!(alignment.sampled, "a degraded (too-long) anchor must set the sampled flag");
    // Only the anchor is aligned; non-anchor members are all skipped.
    for (m, &al) in alignment.aligned.iter().enumerate() {
        assert_eq!(al, m == anchor_idx, "only the anchor is aligned in the degraded path");
    }
    // Degraded → no variation points (no member was aligned to diff against the anchor).
    assert!(
        template.variation_points.is_empty(),
        "degraded template has no variation points, got {:?}",
        template.variation_points
    );
}

#[test]
fn metavar_profile_differing_callee_only_for_real_callees() {
    use super::super::score::metavar_profile;

    // (a) A differing binary_expression subtree (`a + b + c` vs `m * n`) is a closure_param at
    // MEDIUM confidence — but it is NOT a differing callee. Fix 5: differing_callee must be
    // false (the OLD band-derived heuristic wrongly set it true for any Medium ClosureParam).
    let a = member(1, "fn f() { let r = a + b + c; }");
    let b = member(2, "fn g() { let r = m * n; }");
    let (_m, bin_template) = run(vec![a, b]);
    let bin_vp = bin_template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::ClosureParam)
        .expect("a closure_param over the differing binary subtree");
    assert_eq!(bin_vp.confidence, Confidence::Medium, "binary subtree closure is Medium");
    assert!(
        !bin_vp.differing_callee,
        "a binary_expression closure_param is NOT a differing callee, got differing_callee=true"
    );
    assert!(
        !metavar_profile(&bin_template).differing_callee,
        "metavar_profile must NOT flag differing_callee for a binary closure_param"
    );

    // (b) A REAL differing callee (`foo()` vs `bar()` at the statement head) sets the flag.
    let c = member(1, "pub fn a() { foo(); foo() }");
    let d = member(2, "pub fn b() { foo(); bar() }");
    let (_m2, callee_template) = run(vec![c, d]);
    let callee_vp = callee_template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["bar".to_string(), "foo".to_string()]
        })
        .expect("a metavar covering the differing callee");
    assert!(
        callee_vp.differing_callee,
        "a real differing callee must set differing_callee, got {:?}",
        callee_vp
    );
    assert!(
        metavar_profile(&callee_template).differing_callee,
        "metavar_profile must flag differing_callee for a real differing callee"
    );
}

#[test]
fn method_call_differing_receiver_is_value_param() {
    // Fix 3: a method-call RECEIVER differs but the method is the SAME (`x.map(a)` vs
    // `y.map(a)`). In Rust tree-sitter this is a `call_expression` whose function is a
    // `field_expression` (`x.map`); the receiver `x`/`y` is the value-child and shares the
    // call's start_byte. The OLD `run_in_callee_position` first arm (`call.start_byte ==
    // run_start => true`) caught the receiver and misclassified it as a closure_param callee,
    // even though the method head (`.map`) is unchanged. The fix: the run must BE the
    // callee/method-name child, not the receiver. A differing receiver is a value_param.
    //
    // Alpha-renaming numbers identifiers by first occurrence, so a bare `x.map` vs `y.map`
    // would both normalize the receiver to ID0 → no differing column. Pin the numbering with a
    // prior `let p = x;` so the receiver is a REUSED id in `a` (`x` = ID1) but a FRESH id in
    // `b` (`y` = ID2) → the receiver column genuinely differs (per-member values `x`/`y`).
    let a = member(1, "fn a() { let p = x; x.map(a); }");
    let b = member(2, "fn b() { let p = x; y.map(a); }");
    let (_members, template) = run(vec![a, b]);

    let recv_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.sort();
            vals == vec!["x".to_string(), "y".to_string()]
        })
        .unwrap_or_else(|| {
            panic!("a metavar covering the differing receiver, got {:?}", template.variation_points)
        });
    assert_eq!(
        recv_vp.kind,
        MetavarKind::ValueParam,
        "a differing method-call receiver (same method) must be a value_param, got {:?}",
        recv_vp
    );
    assert_ne!(
        recv_vp.kind,
        MetavarKind::ClosureParam,
        "a differing receiver must NOT be a closure_param callee"
    );

    // Synthetic-spans unit check: the RECEIVER leaf (column 1) is NOT a callee position; the
    // METHOD-NAME head (column 2) IS.
    let m = synthetic_method_call();
    assert!(
        !super::run_in_callee_position(&m, 1, 1),
        "the receiver must NOT count as a callee position"
    );
    assert!(
        super::run_in_callee_position(&m, 2, 2),
        "the method-NAME head must count as a callee position"
    );
}

#[test]
fn method_call_differing_method_name_is_closure_param() {
    // P2b companion: when the METHOD NAME (not an argument) differs, the
    // `method_call_expression` callee branch DOES fire → closure_param (the differing-callee
    // guard, the Plan-3 SCIP seam). Synthetic spans: two members whose method-name head differs
    // (`recv.foo(arg)` vs `recv.bar(arg)`).
    let head = synthetic_method_call();
    // run_in_callee_position pins the method-name head as a callee position (the structural
    // half). The full classify_run path then bands it closure_param when the values differ.
    assert!(
        super::run_in_callee_position(&head, 2, 2),
        "the differing method-NAME head must be a callee position → closure_param"
    );
}

#[test]
fn skipped_member_does_not_demote_value_param_to_gapped() {
    // P1 correctness regression: a cost-skipped (too-long) member's `""` in per_member_values
    // is "value unknown", NOT a genuine indel gap. The gapped check in `classify_run` must
    // only consider ALIGNED members — a skipped member's empty entry must not force
    // `kind=Gapped` on a column where the aligned members show a genuine value difference.
    //
    // Setup: 3-member class.
    //   - member A: `fn a() -> i32 { let x = 10; x }` (short, aligned)
    //   - member B: `fn b() -> i32 { let x = 20; x }` (short, aligned)
    //   - member C: synthetic with >LCS_MAX_SEQ_TOKENS tokens (skipped, not aligned)
    //
    // The two short aligned members differ only by the literal (10 vs 20) → should be a
    // clean `ValueParam`. Without the fix, C's `""` in per_member_values triggers the old
    // `any(|v| v.is_empty())` check and classifies it `Gapped` instead.
    let cap = super::align::LCS_MAX_SEQ_TOKENS;

    let a = member(1, "fn a() -> i32 { let x = 10; x }");
    let b = member(2, "fn b() -> i32 { let x = 20; x }");
    // Synthetic long member: struct_hash "z" sorts LAST → it is never the anchor.
    let c = synthetic_member(3, "z", cap + 50);

    let members = canonical(vec![a, b, c]);
    let anchor_idx = resolve_anchor_idx(&members, None);
    let alignment = align_to_anchor(&members, anchor_idx);
    let template = anti_unify(&members, &alignment);

    // The long member must be skipped and the alignment marked sampled.
    let long_idx = members.iter().position(|m| m.seq.len() > cap).unwrap();
    assert!(!alignment.aligned[long_idx], "the long member must be excluded from alignment");
    assert!(alignment.sampled, "skipping a long member must set the sampled flag");

    // The literal column (10 vs 20) must be a ValueParam — the skipped member's "" must NOT
    // force it to Gapped.
    let lit_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.retain(|v| !v.is_empty()); // exclude the skipped member's ""
            vals.sort();
            vals == vec!["10".to_string(), "20".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a value_param metavar with values [10, 20, \"\"], got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        lit_vp.kind,
        MetavarKind::ValueParam,
        "a skipped member's \"\" must NOT demote a genuine literal column to Gapped; got kind={:?}",
        lit_vp.kind
    );
    assert_eq!(lit_vp.confidence, Confidence::High);

    // per_member_values stays length == members.len() (ordinal-aligned); the skipped
    // member's slot is "" (honest "unknown"), not removed.
    assert_eq!(
        lit_vp.per_member_values.len(),
        members.len(),
        "per_member_values must be ordinal-aligned to all members (skipped member keeps \"\")"
    );
    assert!(
        lit_vp.per_member_values.iter().any(|v| v.is_empty()),
        "the skipped member must still contribute \"\" to per_member_values (honest unknown)"
    );
}

#[test]
fn zero_width_insert_sharing_column_is_rendered() {
    // Fix 4 (#215 Plan 4b Codex round-4): when statement snapping emits BOTH a CONSUMING gapped
    // span AND a ZERO-WIDTH member-only insert at the SAME anchor column (one member deletes
    // the first anchor statement while another inserts a leading statement before it),
    // the old `render_template` emitted the consuming hole and jumped to `occ.hi + 1`,
    // so the zero-width VP at that `lo` was NEVER rendered — yet it stayed in
    // `variation_points` JSON (a metavar with no placeholder). The fix renders
    // zero-width holes across the consumed range too, so EVERY VP has a placeholder (VP
    // count == placeholder count).
    //
    // Drive `render_template` directly with the exact shape (the real-parse path that produces
    // it depends on the leftmost-tie skeleton-LCS attribution, which is
    // non-deterministic to force): a 3-leaf anchor, a consuming gapped VP over cols
    // [1..=2], and a zero-width gapped VP also attached at col 1.
    let anchor = RefineMember {
        callee_monikers: Default::default(),
        symbol_id: 1,
        lang: Language::Rust,
        struct_hash: "synthetic".to_string(),
        // bytes: A=0..1, B=2..3, C=4..5 (single-char leaves separated by a space).
        seq: vec!["A".to_string(), "B".to_string(), "C".to_string()],
        node_spans: vec![
            NodeSpan { start_byte: 0, end_byte: 1, kind: "identifier", is_leaf: true },
            NodeSpan { start_byte: 2, end_byte: 3, kind: "identifier", is_leaf: true },
            NodeSpan { start_byte: 4, end_byte: 5, kind: "identifier", is_leaf: true },
        ],
        text: Arc::from("A B C"),
    };

    // m0: a CONSUMING gapped span over cols [1..=2] (a deleted-first anchor statement).
    // m1: a ZERO-WIDTH member-only inserted statement attached at the SAME col 1.
    let variation_points = vec![
        VariationPoint {
            metavar_id: "m0".to_string(),
            kind: MetavarKind::Gapped,
            occurrences: vec![1],
            per_member_values: vec!["B C".to_string(), String::new()],
            extraction_role: MetavarKind::Gapped.as_db_str(),
            type_hint: None,
            confidence: Confidence::Low,
            differing_callee: false,
        },
        VariationPoint {
            metavar_id: "m1".to_string(),
            kind: MetavarKind::Gapped,
            occurrences: vec![1],
            per_member_values: vec![String::new(), "leading();".to_string()],
            extraction_role: MetavarKind::Gapped.as_db_str(),
            type_hint: None,
            confidence: Confidence::Low,
            differing_callee: false,
        },
    ];
    let lo_to_hi: BTreeMap<usize, usize> = [(1usize, 2usize)].into_iter().collect();
    let zero_width_cols: std::collections::BTreeSet<usize> = [1usize].into_iter().collect();

    let text = render_template(&anchor, &variation_points, &lo_to_hi, &zero_width_cols);

    // BOTH placeholders are present: the consuming hole ⟨m0?⟩ and the zero-width hole ⟨m1?⟩.
    assert!(text.contains("⟨m0?⟩"), "the consuming gapped hole must render, got {text:?}");
    assert!(
        text.contains("⟨m1?⟩"),
        "the zero-width insert sharing the consumed column must render, got {text:?}"
    );

    // VP count == placeholder count: every VP in the JSON has exactly one rendered placeholder.
    let placeholder_count = text.matches('⟨').count();
    assert_eq!(
        placeholder_count,
        variation_points.len(),
        "every variation point must have exactly one placeholder in the template (got {} \
         placeholders for {} VPs): {text:?}",
        placeholder_count,
        variation_points.len()
    );
}

#[test]
fn uniform_literal_bucket_ignores_skipped_member() {
    // Fix 1 (#215 Plan 4b Codex round-4): `uniform_literal_bucket` iterated ALL members. A
    // cost-skipped member has an all-gap col_map and no insert at the literal column, so the
    // `found?` short-circuit returned `None` → NO type_hint, even when EVERY aligned member is
    // the same integer-literal kind. The fix excludes `!alignment.aligned[m]` members so the
    // stable integer-bucket hint still emits over the aligned subset.
    let cap = super::align::LCS_MAX_SEQ_TOKENS;

    // Two short aligned members differing only in a SAME-KIND integer literal (10 vs 20) → a
    // clean value_param whose uniform bucket is LIT_INTEGER_LITERAL.
    let a = member(1, "fn a() -> i32 { let x = 10; x }");
    let b = member(2, "fn b() -> i32 { let x = 20; x }");
    // Synthetic long member (struct_hash "z" sorts LAST → never the anchor, always skipped).
    let c = synthetic_member(3, "z", cap + 50);

    let members = canonical(vec![a, b, c]);
    let anchor_idx = resolve_anchor_idx(&members, None);
    let alignment = align_to_anchor(&members, anchor_idx);
    let template = anti_unify(&members, &alignment);

    // Precondition: the long member is skipped and the alignment is sampled.
    let long_idx = members.iter().position(|m| m.seq.len() > cap).unwrap();
    assert!(!alignment.aligned[long_idx], "the long member must be excluded from alignment");
    assert!(alignment.sampled, "skipping a long member must set the sampled flag");

    // The literal value_param (10 vs 20, "" for the skipped member) must carry the INTEGER
    // bucket type hint — NOT None (which would later mint a bare generic in propose_signature).
    let lit_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.retain(|v| !v.is_empty()); // drop the skipped member's ""
            vals.sort();
            vals == vec!["10".to_string(), "20".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a value_param metavar with values [10, 20, \"\"], got {:?}",
                template.variation_points
            )
        });
    assert_eq!(lit_vp.kind, MetavarKind::ValueParam);
    assert_eq!(
        lit_vp.type_hint.as_deref(),
        Some("LIT_INTEGER_LITERAL"),
        "a skipped member must NOT erase the uniform integer-bucket type hint, got {:?}",
        lit_vp.type_hint
    );
}

/// AGGREGATE cell budget on the TEMPLATE lane (the round-4 fidelity-lane budget mirrored into
/// `align_to_anchor`). A class of MANY members each LARGE but UNDER
/// [`align::LCS_MAX_SEQ_TOKENS`] (so the per-member length cap never fires) must NOT run an
/// exact O(n·m) DP for every member — once the running `Σ |anchor|·|member|` exceeds the
/// budget the remaining members are SKIPPED (`aligned[m] = false`, all-gap col_map) and the
/// alignment is `sampled`. Without the budget this class would run all N−1 large
/// star-aligns (the unbudgeted template-lane cliff).
#[test]
fn align_to_anchor_aggregate_budget_caps_template_lane() {
    // Same invariant the production budget enforces, exercised with a TINY INJECTED budget on
    // SMALL seqs so it runs in milliseconds (the production budget is 100M cells; tripping it
    // for real needs huge members — the cutover arithmetic is identical at any budget
    // value, so a small budget hits the SAME code path far cheaper).
    //
    // 20 members, each 10 tokens. The anchor sorts first (struct_hash "a"); every non-anchor is
    // 10 tokens → 10·10 = 100 cells per star-align. With a 250-cell budget the running Σ
    // exceeds it after 3 charged aligns (300 > 250), so the rest are skipped.
    let per_member = 10usize;
    let member_count = 20usize;
    let tiny_budget: u64 = 250;
    // Anchor "a" sorts first → it is the spine; the rest are non-anchor star-align members.
    let mut members = vec![synthetic_member(1, "a", per_member)];
    for m in 1..member_count {
        members.push(synthetic_member(1 + m as i64, &format!("b{m:02}"), per_member));
    }
    let members = canonical(members);
    let anchor_idx = resolve_anchor_idx(&members, None);

    // Sanity: every member is under the per-pair length cap, so the ONLY bound that can fire is
    // the (injected) aggregate budget.
    assert!(
        members.iter().all(|m| m.seq.len() <= align::LCS_MAX_SEQ_TOKENS),
        "members must be under the per-pair length cap so only the aggregate budget bounds cost"
    );

    let start = std::time::Instant::now();
    let mut budget = CellBudget::new(tiny_budget);
    let alignment = align_to_anchor_with_budget(&members, anchor_idx, &mut budget);
    let elapsed = start.elapsed();

    // The budget tripped → sampled flag set (a budget-truncated class is never reported exact).
    assert!(alignment.sampled, "aggregate-budget truncation must set sampled=true");

    // Only a bounded HANDFUL of non-anchor members actually aligned exactly — far below all
    // N−1. The anchor is always aligned (identity), so count the non-anchor aligned
    // members.
    let aligned_non_anchor =
        alignment.aligned.iter().enumerate().filter(|&(m, &a)| a && m != anchor_idx).count();
    let non_anchor_total = member_count - 1;
    assert!(
        aligned_non_anchor < non_anchor_total,
        "aggregate budget must cap exact aligns below all {non_anchor_total} non-anchor members, \
         ran {aligned_non_anchor}"
    );
    // Budget-implied bound: ⌈budget / cells_per_align⌉ + 1 (the align that trips it still
    // runs). cells_per_align = per_member² = 100.
    let max_aligned = tiny_budget / ((per_member as u64) * (per_member as u64)) + 1;
    assert!(
        aligned_non_anchor as u64 <= max_aligned,
        "exact aligns ({aligned_non_anchor}) must not exceed the budget-implied bound \
         ({max_aligned})"
    );
    // The skipped members read as all-gap (excluded from fixedness/indel) — verify one.
    let skipped = alignment
        .aligned
        .iter()
        .enumerate()
        .find(|&(m, &a)| !a && m != anchor_idx)
        .map(|(m, _)| m)
        .expect("at least one non-anchor member must be skipped past the budget");
    assert!(
        alignment.col_map[skipped].iter().all(Option::is_none),
        "a budget-skipped member must have an all-gap col_map"
    );
    // Fast: the tiny budget means only ~3 small DPs ran, the rest are O(1) skips.
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "budget-capped template align must be fast, took {elapsed:?}"
    );
}

/// The matched-statement re-descent draws from the SAME per-class budget as the parent
/// star-align: once that shared budget is exhausted, a remaining matched statement is NOT
/// re-descended (no extra exact DP — it is left whole-fixed) and `Template::sampled` latches.
///
/// The parent star-align ALIGNS the member (so the indel snap engages and the re-descent path
/// is reached), but the SHARED budget is already exhausted by the time the re-descent runs
/// — exactly the production state when the parent star-align of a huge class spends the
/// whole budget before `anti_unify` re-seeds it (`budget.spent = alignment.spent_cells`).
/// We reproduce that precise precondition (member aligned, budget exhausted) by running the
/// parent align under a generous budget, then driving `anti_unify_with_budget` with a
/// budget marked exhausted. Verified against the SAME fixture under a generous budget
/// (re-descent fires, the inner 1/2 VP appears) to prove the budget is what suppresses it.
#[test]
fn redescent_shares_parent_align_budget() {
    // `p(1); q();` vs `p(2); let z = 0; q();`: the inserted `let z = 0;` is the statement-count
    // indel that engages the snap; `p(1)`/`p(2)` is the matched statement whose inner 1/2 diff
    // the re-descent would surface as a value_param VP (see
    // `matched_statements_after_indel_are_anti_unified`).
    let mk = || {
        canonical(vec![
            member(1, "fn f(){ p(1); q(); }"),
            member(2, "fn f(){ p(2); let z = 0; q(); }"),
        ])
    };
    let inner_value_present = |tpl: &Template| {
        tpl.variation_points.iter().any(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.retain(|v| !v.is_empty());
            vals.sort();
            vals == vec!["1".to_string(), "2".to_string()]
        })
    };

    // GENEROUS budget (production default): the re-descent fires → the inner 1/2 value_param VP
    // is emitted and the template is NOT sampled.
    let generous = mk();
    let anchor_idx = resolve_anchor_idx(&generous, None);
    let generous_align = align_to_anchor(&generous, anchor_idx);
    let generous_tpl = anti_unify(&generous, &generous_align);
    assert!(
        inner_value_present(&generous_tpl),
        "under a generous budget the re-descent must surface the inner 1/2 value_param, got {:?}",
        generous_tpl.variation_points
    );
    assert!(
        !generous_align.sampled && !generous_tpl.sampled,
        "an under-budget class must NOT be sampled"
    );

    // STARVED at the re-descent: align the member with a generous budget (so the indel snap
    // engages and `aligned[member] = true`, NOT sampled), then run the re-descent with the
    // SHARED budget already exhausted. The re-descent must SKIP the matched statement (no inner
    // VP) and latch `Template::sampled`.
    let starved = mk();
    let anchor_idx = resolve_anchor_idx(&starved, None);
    let mut shared = CellBudget::new(ALIGN_AGGREGATE_CELLS_BUDGET);
    let starved_align = align_to_anchor_with_budget(&starved, anchor_idx, &mut shared);
    assert!(
        !starved_align.sampled,
        "the parent star-align under a generous budget must align the member (not sampled)"
    );
    assert!(starved_align.aligned.iter().all(|&a| a), "every member must be aligned");
    // The parent has now spent the budget down to `shared.spent`; force the exhausted state the
    // re-descent would see when a huge parent star-align consumes the whole per-class budget.
    shared.exhausted = true;
    let starved_tpl = anti_unify_with_budget(&starved, &starved_align, &mut shared);
    assert!(
        !inner_value_present(&starved_tpl),
        "with the budget exhausted the matched statement must NOT be re-descended (no inner VP), \
         got {:?}",
        starved_tpl.variation_points
    );
    // The skipped re-descent latches Template::sampled (the parent align was NOT sampled here,
    // so this is the ONLY honest signal that the class was cost-degraded).
    assert!(
        starved_tpl.sampled,
        "a budget-skipped re-descent must latch Template::sampled (the class is cost-degraded)"
    );
    // The inserted statement is still gapped (the snap itself does not need exact DP).
    assert!(
        starved_tpl.variation_points.iter().any(|vp| vp.kind == MetavarKind::Gapped),
        "the inserted statement must still be a gapped metavar even when the budget is exhausted"
    );
}

/// An UNDER-budget normal class is byte-identical (template text, variation points, coverage)
/// to the same class with NO budget threading — the budget counter only triggers
/// degradation PAST the budget, so the common case is untouched. (Determinism: the
/// production budget is never tripped here, so the output matches the pre-budget behavior
/// exactly.)
#[test]
fn under_budget_class_is_byte_identical_to_unbudgeted() {
    // A representative class exercising several pipeline paths: a literal value_param, a
    // matched statement re-descent (the indel + inner 1/2 diff), and fixed text.
    let mk = || {
        canonical(vec![
            member(1, "fn f(){ let x = 10; p(1); q(); }"),
            member(2, "fn f(){ let x = 20; p(2); let z = 0; q(); }"),
        ])
    };

    // Production path (fresh budget at the lane const — never tripped for this small class).
    let prod = mk();
    let prod_anchor = resolve_anchor_idx(&prod, None);
    let prod_align = align_to_anchor(&prod, prod_anchor);
    let prod_tpl = anti_unify(&prod, &prod_align);

    // Explicit huge-budget path (a budget so large it can never trip) — must produce the SAME
    // output, proving the budget machinery does not perturb the under-budget result.
    let huge = mk();
    let huge_anchor = resolve_anchor_idx(&huge, None);
    let mut huge_budget = CellBudget::new(u64::MAX);
    let huge_align = align_to_anchor_with_budget(&huge, huge_anchor, &mut huge_budget);
    let huge_tpl = anti_unify_with_budget(&huge, &huge_align, &mut huge_budget);

    assert_eq!(prod_tpl.text, huge_tpl.text, "template text must be budget-invariant");
    assert_eq!(
        prod_tpl.anti_unify_coverage, huge_tpl.anti_unify_coverage,
        "coverage must be budget-invariant"
    );
    // Variation points compared via their serialized contract (the persisted shape).
    let prod_vps = serde_json::to_string(&prod_tpl.variation_points).unwrap();
    let huge_vps = serde_json::to_string(&huge_tpl.variation_points).unwrap();
    assert_eq!(prod_vps, huge_vps, "variation points must be budget-invariant");
    // Neither is sampled — the under-budget common case.
    assert!(!prod_align.sampled && !prod_tpl.sampled, "under-budget class must not be sampled");
    assert!(!huge_align.sampled && !huge_tpl.sampled, "huge-budget class must not be sampled");
}

// ─────────────────────── kk adversarial probes (round-6 callee reopen) ──────────────────────
fn dump(label: &str, template: &Template) {
    eprintln!("=== {label} ===");
    eprintln!("  text: {:?}", template.text);
    eprintln!("  coverage: {}", template.anti_unify_coverage);
    for vp in &template.variation_points {
        eprintln!(
            "  VP {}: kind={:?} conf={:?} differing_callee={} vals={:?} type_hint={:?}",
            vp.metavar_id,
            vp.kind,
            vp.confidence,
            vp.differing_callee,
            vp.per_member_values,
            vp.type_hint
        );
    }
}

#[test]
fn kk_probe_value_position_locals() {
    // (1) renamed locals load_user/load_order — must NOT explode into closure_param.
    let a = member(1, "fn f() { let u = load_user(id); sink(u); }");
    let b = member(2, "fn g() { let o = load_order(id); sink(o); }");
    let (_m, t) = run(vec![a, b]);
    dump("renamed-callee load_user/load_order", &t);

    // operand x/y in a+x vs a+y stays value_param.
    let c = member(1, "fn f() { let r = a + x; }");
    let d = member(2, "fn g() { let r = a + y; }");
    let (_m2, t2) = run(vec![c, d]);
    dump("operand a+x / a+y", &t2);
}

#[test]
fn kk_probe_field_access_non_call() {
    // (2) plain field access x.foo vs x.bar (NON-call) — documented non-reopen.
    let a = member(1, "fn f() { let v = x.foo; }");
    let b = member(2, "fn g() { let v = x.bar; }");
    let (_m, t) = run(vec![a, b]);
    dump("field access x.foo / x.bar (non-call)", &t);

    // struct field names in a struct literal.
    let c = member(1, "fn f() { let v = S { foo: 1 }; }");
    let d = member(2, "fn g() { let v = S { bar: 1 }; }");
    let (_m2, t2) = run(vec![c, d]);
    dump("struct field foo: / bar:", &t2);

    // labels.
    let e = member(1, "fn f() { 'outer: loop { break 'outer; } }");
    let g = member(2, "fn h() { 'inner: loop { break 'inner; } }");
    let (_m3, t3) = run(vec![e, g]);
    dump("labels 'outer / 'inner", &t3);
}

#[test]
fn kk_probe_235_interaction() {
    // (3) a(); c(); vs a(); b(); c(); — spurious closure_param ["c","b"] + gapped insert.
    let a = member(1, "fn f(){ a(); c(); }");
    let b = member(2, "fn f(){ a(); b(); c(); }");
    let (_m, t) = run(vec![a, b]);
    dump("235 interaction a;c vs a;b;c", &t);
    eprintln!("  template.sampled={}", t.sampled);
    let gapped = t.variation_points.iter().filter(|v| v.kind == MetavarKind::Gapped).count();
    let closure = t.variation_points.iter().filter(|v| v.kind == MetavarKind::ClosureParam).count();
    eprintln!("  gapped_count={gapped} closure_count={closure}");
}

#[test]
fn kk_probe_4th_position_candidates() {
    // (6) candidate erased-difference positions NOT in the reopen set — do they leak?
    // (a) enum variant path head in a call: Foo::Bar(x) vs Foo::Baz(x).
    let a = member(1, "fn f() { let v = E::Bar(x); }");
    let b = member(2, "fn g() { let v = E::Baz(x); }");
    let (_m, t) = run(vec![a, b]);
    dump("enum-variant-call E::Bar / E::Baz", &t);

    // (b) struct literal name (type position): S1 { } vs S2 { }.
    let c = member(1, "fn f() { let v = S1 { a: 1 }; }");
    let d = member(2, "fn g() { let v = S2 { a: 1 }; }");
    let (_m2, t2) = run(vec![c, d]);
    dump("struct-literal-name S1 / S2", &t2);

    // (c) tuple-struct / unit path expr (no call): just E::Bar vs E::Baz.
    let e = member(1, "fn f() { let v = E::Bar; }");
    let g = member(2, "fn h() { let v = E::Baz; }");
    let (_m3, t3) = run(vec![e, g]);
    dump("path-expr E::Bar / E::Baz (no call)", &t3);

    // (d) macro with args (assert_eq! shaped).
    let h = member(1, "fn f() { foo!(x, y); }");
    let i = member(2, "fn g() { bar!(x, y); }");
    let (_m4, t4) = run(vec![h, i]);
    dump("macro-with-args foo!(x,y) / bar!(x,y)", &t4);

    // (e) lifetime in a type position '&'a vs '&'b.
    let j = member(1, "fn f<'a>(x: &'a u32) -> &'a u32 { x }");
    let k = member(2, "fn g<'b>(x: &'b u32) -> &'b u32 { x }");
    let (_m5, t5) = run(vec![j, k]);
    dump("lifetime 'a / 'b", &t5);
}

#[test]
fn kk_probe_determinism() {
    // (5) run the callee case twice with reversed input order; output must match.
    let a1 = member(1, "fn a(){ foo(); }");
    let b1 = member(2, "fn b(){ bar(); }");
    let (_m1, t1) = run(vec![a1, b1]);
    let a2 = member(1, "fn a(){ foo(); }");
    let b2 = member(2, "fn b(){ bar(); }");
    let (_m2, t2) = run(vec![b2, a2]); // reversed input
    assert_eq!(
        serde_json::to_string(&t1.variation_points).unwrap(),
        serde_json::to_string(&t2.variation_points).unwrap(),
        "callee reopen must be input-order deterministic"
    );
    eprintln!("determinism OK: {:?}", t1.text);
}

#[test]
fn kk_probe_scoped_callee() {
    // scoped_identifier callee a::foo() vs a::bar() — is_callee_leaf_kind includes
    // scoped_identifier.
    let a = member(1, "fn f() { m::foo(); }");
    let b = member(2, "fn g() { m::bar(); }");
    let (_m, t) = run(vec![a, b]);
    dump("scoped callee m::foo / m::bar", &t);
}

/// Build + anti-unify a class with an EXPLICIT medoid (parent anchor), mirroring `run` but
/// threading `Some(medoid_symbol_id)` so the spine is a chosen member, not canonical member 0.
fn run_with_medoid(
    members: Vec<RefineMember>,
    medoid_symbol_id: i64,
) -> (Vec<RefineMember>, Template, usize) {
    let members = canonical(members);
    let anchor_idx = resolve_anchor_idx(&members, Some(medoid_symbol_id));
    let alignment = align_to_anchor(&members, anchor_idx);
    let template = anti_unify(&members, &alignment);
    (members, template, anchor_idx)
}

#[test]
fn redescent_uses_parent_medoid_anchor() {
    // Fix 1 (Codex round-7): the matched-statement re-descent anchors its sub-alignment on the
    // PARENT anchor's (medoid's) own statement slice — not the canonical-first matched
    // sub-member. `parent_lo = slo + sub_col` is exact only when the sub-anchor's `node_spans`
    // ARE `anchor.node_spans[slo..]`, i.e. the sub-anchor IS the parent anchor's statement.
    // Anchoring on sub-member 0 relied IMPLICITLY on the statement-snap matching only
    // skeleton-EQUAL statements (identical column layouts → the shift is coincidentally right);
    // the fix makes that literal, removing the fragile dependence.
    //
    // End-to-end correctness probe with the medoid NOT at canonical member 0: an inner literal
    // diff `p(N)` in a MATCHED statement + a statement-count indel (`let z = 0;`) forces the
    // re-descent. Values + the rendered hole must be ordinal-correct for ALL members. (HONEST
    // NOTE: skeleton-equality means the medoid path stays correct even pre-fix — the bug could
    // not be made to RENDER wrong source; this verifies the principled fix is
    // behavior-preserving with a non-zero medoid.)
    let m0 = member(1, "fn f(){ p(10); q(); }");
    let m1 = member(2, "fn f(){ p(20); let z = 0; q(); }");
    let m2 = member(3, "fn f(){ p(30); let z = 0; q(); }");
    // Pick the medoid as the member that is NOT canonical-first.
    let members_sorted = canonical(vec![
        member(1, "fn f(){ p(10); q(); }"),
        member(2, "fn f(){ p(20); let z = 0; q(); }"),
        member(3, "fn f(){ p(30); let z = 0; q(); }"),
    ]);
    let first_sym = members_sorted[0].symbol_id;
    let medoid = members_sorted
        .iter()
        .map(|m| m.symbol_id)
        .find(|&s| s != first_sym)
        .expect("a non-first member");
    let (members, template, anchor_idx) = run_with_medoid(vec![m0, m1, m2], medoid);
    assert_ne!(anchor_idx, 0, "the medoid must not be canonical member 0 for this probe");

    // The inner literal hole must surface a value_param with ordinal-correct per-member values:
    // each member's OWN literal at the hole (the medoid's source at the hole, members' values
    // ordinal-correct), NOT shifted.
    let value_vp = template
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::ValueParam)
        .unwrap_or_else(|| {
            panic!(
                "the matched p(N) inner diff must be a value_param, got {:?}",
                template.variation_points
            )
        });
    assert_eq!(
        value_vp.per_member_values.len(),
        members.len(),
        "values stay ordinal to all members"
    );
    for (i, m) in members.iter().enumerate() {
        let expected = if m.text.contains("p(10)") {
            "10"
        } else if m.text.contains("p(20)") {
            "20"
        } else {
            "30"
        };
        assert_eq!(
            value_vp.per_member_values[i], expected,
            "per_member_values[{i}] must be member symbol_id {}'s own literal (not shifted)",
            m.symbol_id
        );
    }
    // The rendered hole sits INSIDE the matched p() call — `p(⟨…⟩)` — at the medoid's anchor
    // source, never a shifted column that would render e.g. a bare hole or the wrong token.
    assert!(
        template.text.contains("p(⟨"),
        "the inner literal hole must render inside the matched p() call, got {:?}",
        template.text
    );
    // The inserted statement is still a gapped metavar (the indel that triggered the
    // re-descent).
    assert!(
        template.variation_points.iter().any(|vp| vp.kind == MetavarKind::Gapped),
        "the inserted `let z = 0;` must remain a gapped metavar, got {:?}",
        template.variation_points
    );
}

#[test]
fn redescent_preserves_straddling_and_zero_width_spans() {
    // Fix 2 (Codex round-7): the re-descent translates a sub-VP using the sub-template's REAL
    // occurrence span (`occurrence_spans`: lo AND hi, plus the zero-width flag), shifted by
    // `slo` — it NO LONGER re-derives `hi` from `subtree_token_count(anchor, parent_lo)`. The
    // re-derivation was correct only for a single-subtree snap; it would TRUNCATE a sub-VP that
    // snapped WIDER than the subtree at its start column (a straddling multi-subtree hole) and
    // would turn a zero-width member-only insert into a CONSUMING hole.
    //
    // A MULTI-TOKEN inner snap exercises the carried-span path: `p("x")` vs `p("yy")` inside a
    // matched statement (the `let z = 0;` / `w();` indel forces the re-descent). The differing
    // string argument widens to the WHOLE `string_literal` subtree (quotes included) — a
    // multi-token hole, NOT a single leaf. The carried span must cover the whole `"x"` so the
    // per-member values include the quotes and the template renders ONE hole replacing the
    // whole literal — not a truncated fragment.
    let a = member(1, "fn f(){ p(\"x\"); let z = 0; }");
    let b = member(2, "fn f(){ p(\"yy\"); let z = 0; w(); }");
    let c = member(3, "fn f(){ p(\"zzz\"); let z = 0; w(); }");
    let (members, template) = run(vec![a, b, c]);

    // The inner string hole keeps its FULL widened span: per-member values are the WHOLE quoted
    // literals (`"x"` / `"yy"` / `"zzz"`), recovered across ALL members ordinal-correct — a
    // truncated hi would drop a quote / yield a fragment.
    let str_vp = template
        .variation_points
        .iter()
        .find(|vp| {
            let mut vals = vp.per_member_values.clone();
            vals.retain(|v| !v.is_empty());
            vals.sort();
            vals == vec!["\"x\"".to_string(), "\"yy\"".to_string(), "\"zzz\"".to_string()]
        })
        .unwrap_or_else(|| {
            panic!(
                "the inner string literal must keep its FULL widened span (whole quoted literal), \
                 got {:?}",
                template.variation_points
            )
        });
    assert_eq!(str_vp.kind, MetavarKind::ValueParam, "a string literal hole is a value_param");
    assert_eq!(str_vp.per_member_values.len(), members.len(), "values stay ordinal to members");
    for (i, m) in members.iter().enumerate() {
        let expected = if m.text.contains("\"x\"") {
            "\"x\""
        } else if m.text.contains("\"yy\"") {
            "\"yy\""
        } else {
            "\"zzz\""
        };
        assert_eq!(str_vp.per_member_values[i], expected, "value[{i}] full quoted + ordinal");
    }
    // The template renders ONE hole INSIDE the matched `p(…)` call, with the quotes consumed
    // (not left dangling) — a truncated span would render `p(⟨…⟩")` or similar.
    assert!(
        template.text.contains("p(⟨"),
        "the inner string hole must render inside p() as one placeholder, got {:?}",
        template.text
    );
    assert!(
        !template.text.contains("p(⟨m0⟩\""),
        "the hole must consume the WHOLE quoted literal (no dangling quote), got {:?}",
        template.text
    );

    // TEMPLATE/VP PARITY: every variation point's occurrence renders exactly one placeholder in
    // the template — the carried span never drops or duplicates a hole. Count `⟨` openers and
    // require it equals the total occurrence count.
    let total_occ: usize = template.variation_points.iter().map(|vp| vp.occurrences.len()).sum();
    let placeholders = template.text.matches('⟨').count();
    assert_eq!(
        placeholders, total_occ,
        "every VP occurrence must render exactly one placeholder (carried-span parity), got \
         template {:?} vps {:?}",
        template.text, template.variation_points
    );
}

#[test]
fn recurrence_collapse_separates_distinct_type_contexts() {
    // Fix 3 (Codex round-7): the recurrence-collapse key must include the syntactic type
    // context (the recovered `: T` annotation), not just (values, role,
    // differing_callee). Same value tuple `[1→2]`, same role, but DIFFERENT annotations
    // (`let a: i32 = 1; let b: u8 = 1` vs both → `2`) must stay TWO metavars —
    // collapsing them makes `propose_signature` recover the first occurrence's `i32`
    // and reuse one param across the `i32` AND `u8` slots (an invalid typed signature).
    let a = member(1, "fn f(){ let a: i32 = 1; let b: u8 = 1; }");
    let b = member(2, "fn g(){ let a: i32 = 2; let b: u8 = 2; }");
    let (_m, t) = run(vec![a, b]);
    let value_vps: Vec<_> =
        t.variation_points.iter().filter(|vp| vp.kind == MetavarKind::ValueParam).collect();
    assert_eq!(
        value_vps.len(),
        2,
        "distinct type contexts (i32 vs u8) must stay TWO metavars, got {:?}",
        t.variation_points
    );
    // Each metavar is its own [1→2] occupying ONE position (NOT one metavar with two
    // occurrences).
    for vp in &value_vps {
        let mut vals = vp.per_member_values.clone();
        vals.sort();
        assert_eq!(vals, vec!["1".to_string(), "2".to_string()], "each metavar is [1→2]");
        assert_eq!(
            vp.occurrences.len(),
            1,
            "a distinct-context metavar must NOT collapse to multiple occurrences, got {:?}",
            vp.occurrences
        );
    }

    // REGRESSION GUARD: the SAME type context at two positions STILL collapses. Two type
    // positions `Foo`/`Bar` (no `: T` annotation — they ARE the types, so type_context is None
    // for both) recur as ONE recurrence-collapsed TypeParam with TWO occurrences.
    let c = member(1, "fn f(){ let a: Foo = d(); let b: Foo = d(); }");
    let e = member(2, "fn g(){ let a: Bar = d(); let b: Bar = d(); }");
    let (_m2, t2) = run(vec![c, e]);
    let type_vps: Vec<_> =
        t2.variation_points.iter().filter(|vp| vp.kind == MetavarKind::TypeParam).collect();
    assert_eq!(
        type_vps.len(),
        1,
        "same-context recurring type positions must COLLAPSE to one metavar, got {:?}",
        t2.variation_points
    );
    assert_eq!(
        type_vps[0].occurrences.len(),
        2,
        "the collapsed type metavar must carry both occurrences, got {:?}",
        type_vps[0].occurrences
    );
}

#[test]
fn generic_type_head_diff_widens_to_whole_type() {
    // Fix 4 (Codex round-7): when the differing type leaf is the HEAD of an enclosing
    // `generic_type` (`Vec<i32>` vs `Option<i32>`), reopening just the head leaf would render
    // `⟨m0⟩<i32>` (and a signature `-> T0<i32>`) — invalid Rust that also hard-codes the
    // anchor's type args. The hole must WIDEN to the whole `generic_type` so the
    // per-member values are the WHOLE types and the template renders a single `⟨m0⟩`.
    let a = member(1, "fn f() -> Vec<i32> { todo!() }");
    let b = member(2, "fn g() -> Option<i32> { todo!() }");
    let (_m, t) = run(vec![a, b]);
    let vp = t.variation_points.iter().find(|vp| vp.kind == MetavarKind::TypeParam).unwrap_or_else(
        || {
            panic!(
                "a differing generic-type head must be a type_param, got {:?}",
                t.variation_points
            )
        },
    );
    let mut vals = vp.per_member_values.clone();
    vals.sort();
    assert_eq!(
        vals,
        vec!["Option<i32>".to_string(), "Vec<i32>".to_string()],
        "the WHOLE generic type must be the metavar value (head widened), got {:?}",
        vp.per_member_values
    );
    assert!(
        t.text.contains("-> ⟨") && !t.text.contains("⟨m0⟩<"),
        "the template must render one whole-type hole (not `⟨m0⟩<i32>`), got {:?}",
        t.text
    );

    // let-binding variant — same widening.
    let c = member(1, "fn f() { let v: Vec<i32> = d(); }");
    let e = member(2, "fn g() { let v: Option<i32> = d(); }");
    let (_m2, t2) = run(vec![c, e]);
    let vp2 = t2
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::TypeParam)
        .expect("let-binding generic-type head must be a type_param");
    let mut vals2 = vp2.per_member_values.clone();
    vals2.sort();
    assert_eq!(
        vals2,
        vec!["Option<i32>".to_string(), "Vec<i32>".to_string()],
        "let-binding: the WHOLE generic type must be the metavar value, got {:?}",
        vp2.per_member_values
    );

    // REGRESSION GUARD: an INNER-arg-only diff (`Vec<i32>`/`Vec<u8>`) stays the inner-leaf case
    // (head unchanged) → `Vec<⟨m0⟩>`, NOT widened to the whole type.
    let g = member(1, "fn f() -> Vec<i32> { todo!() }");
    let h = member(2, "fn g() -> Vec<u8> { todo!() }");
    let (_m3, t3) = run(vec![g, h]);
    let vp3 = t3
        .variation_points
        .iter()
        .find(|vp| vp.kind == MetavarKind::TypeParam)
        .expect("inner-arg diff is still a type_param");
    let mut vals3 = vp3.per_member_values.clone();
    vals3.sort();
    assert_eq!(
        vals3,
        vec!["i32".to_string(), "u8".to_string()],
        "inner-arg diff must stay the inner leaf (wrapper preserved), got {:?}",
        vp3.per_member_values
    );
    assert!(
        t3.text.contains("Vec<⟨"),
        "inner-arg diff must keep the `Vec<…>` wrapper in the template, got {:?}",
        t3.text
    );
}

#[test]
fn defensive_member_sample_cap_marks_tail_members_unaligned() {
    let member_count = super::align::LCS_MEMBER_SAMPLE + 1;
    let members: Vec<RefineMember> =
        (0..member_count).map(|i| synthetic_member(i as i64, &format!("m{i:03}"), 1)).collect();

    let alignment = align_to_anchor(&members, 0);

    assert!(alignment.sampled, "tail members beyond the defensive sample cap mark sampling");
    assert!(alignment.aligned[0], "the anchor remains aligned");
    assert!(alignment.aligned[super::align::LCS_MEMBER_SAMPLE - 1]);
    assert!(!alignment.aligned[super::align::LCS_MEMBER_SAMPLE]);
    assert_eq!(alignment.col_map[super::align::LCS_MEMBER_SAMPLE], vec![None]);
    assert!(alignment.member_inserts[super::align::LCS_MEMBER_SAMPLE].is_empty());
}

#[test]
fn call_node_differing_callee_classifies_low_with_flag() {
    let members = vec![
        RefineMember {
            callee_monikers: Default::default(),
            symbol_id: 1,
            lang: Language::Rust,
            struct_hash: "call".to_string(),
            seq: vec!["call_expression".to_string()],
            node_spans: vec![NodeSpan {
                start_byte: 0,
                end_byte: 5,
                kind: "call_expression",
                is_leaf: false,
            }],
            text: Arc::from("foo()"),
        },
        RefineMember {
            callee_monikers: Default::default(),
            symbol_id: 2,
            lang: Language::Rust,
            struct_hash: "call".to_string(),
            seq: vec!["call_expression".to_string()],
            node_spans: vec![NodeSpan {
                start_byte: 0,
                end_byte: 5,
                kind: "call_expression",
                is_leaf: false,
            }],
            text: Arc::from("bar()"),
        },
    ];
    let alignment = ClassAlignment {
        anchor_idx: 0,
        sampled: false,
        aligned: vec![true, true],
        col_map: vec![vec![Some(0)], vec![Some(0)]],
        member_inserts: vec![BTreeMap::new(), BTreeMap::new()],
        spent_cells: 0,
    };

    let class = classify_run(&members, &alignment, &members[0], 0, 0, &[
        "foo()".to_string(),
        "bar()".to_string(),
    ]);

    assert_eq!(class.kind, MetavarKind::ClosureParam);
    assert_eq!(class.confidence, Confidence::Low);
    assert!(class.differing_callee);
    assert!(class.type_hint.is_none());
}

#[test]
fn split_helper_defensive_paths_are_covered() {
    assert!((coverage_from_mask(&[]) - 1.0).abs() < f64::EPSILON);

    let one_member_alignment = ClassAlignment {
        anchor_idx: 0,
        sampled: false,
        aligned: vec![true],
        col_map: vec![vec![Some(0)]],
        member_inserts: vec![BTreeMap::new()],
        spent_cells: 0,
    };
    assert!(!any_member_inserts_within(&one_member_alignment, 2, 1));

    let raw = EmittedSpan::Raw(2, 4);
    let statement =
        EmittedSpan::Statement { lo: 1, hi: 6, per_member_values: Vec::new(), zero_width: false };
    let classified = EmittedSpan::Classified {
        lo: 1,
        hi: 8,
        per_member_values: Vec::new(),
        kind: MetavarKind::ValueParam,
        type_hint: None,
        confidence: Confidence::High,
        differing_callee: false,
        zero_width: false,
    };
    assert_eq!(raw.hi(), 4);
    assert_eq!(statement.hi(), 6);
    assert_eq!(classified.hi(), 8);

    let invalid_utf8_slice = RefineMember {
        callee_monikers: Default::default(),
        symbol_id: 1,
        lang: Language::Rust,
        struct_hash: "utf8".to_string(),
        seq: vec!["ID0".to_string()],
        node_spans: vec![NodeSpan {
            start_byte: 1,
            end_byte: 2,
            kind: "identifier",
            is_leaf: true,
        }],
        text: Arc::from("é"),
    };
    assert_eq!(recover_values(&[invalid_utf8_slice], &one_member_alignment, 0, 0), vec![
        "ID0".to_string()
    ]);

    let empty_anchor = RefineMember {
        callee_monikers: Default::default(),
        symbol_id: 2,
        lang: Language::Rust,
        struct_hash: "empty".to_string(),
        seq: Vec::new(),
        node_spans: Vec::new(),
        text: Arc::from(""),
    };
    assert_eq!(annotation_type_context(&empty_anchor, 0), None);

    let invalid_type_slice = RefineMember {
        callee_monikers: Default::default(),
        symbol_id: 3,
        lang: Language::Rust,
        struct_hash: "bad-type".to_string(),
        seq: vec![":".to_string(), "ID0".to_string()],
        node_spans: vec![
            NodeSpan { start_byte: 0, end_byte: 1, kind: ":", is_leaf: true },
            NodeSpan { start_byte: 2, end_byte: 3, kind: "type_identifier", is_leaf: true },
        ],
        text: Arc::from(":é"),
    };
    assert_eq!(annotation_type_context(&invalid_type_slice, 1), None);

    let colon_without_type = RefineMember {
        callee_monikers: Default::default(),
        symbol_id: 4,
        lang: Language::Rust,
        struct_hash: "no-type".to_string(),
        seq: vec![":".to_string(), "ID0".to_string()],
        node_spans: vec![
            NodeSpan { start_byte: 0, end_byte: 1, kind: ":", is_leaf: true },
            NodeSpan { start_byte: 1, end_byte: 2, kind: "identifier", is_leaf: true },
        ],
        text: Arc::from(":x"),
    };
    assert_eq!(annotation_type_context(&colon_without_type, 1), None);

    let interpolated_template = RefineMember {
        callee_monikers: Default::default(),
        symbol_id: 5,
        lang: Language::TypeScript,
        struct_hash: "template".to_string(),
        seq: vec![
            "template_string".to_string(),
            "`".to_string(),
            "string_fragment".to_string(),
            "template_substitution".to_string(),
            "`".to_string(),
        ],
        node_spans: vec![
            NodeSpan { start_byte: 0, end_byte: 7, kind: "template_string", is_leaf: false },
            NodeSpan { start_byte: 0, end_byte: 1, kind: "`", is_leaf: true },
            NodeSpan { start_byte: 1, end_byte: 2, kind: "string_fragment", is_leaf: true },
            NodeSpan { start_byte: 2, end_byte: 6, kind: "template_substitution", is_leaf: false },
            NodeSpan { start_byte: 6, end_byte: 7, kind: "`", is_leaf: true },
        ],
        text: Arc::from("`a${b}`"),
    };
    assert_eq!(widen_string_content_run(&interpolated_template, 2, 2), (2, 2));

    let duplicate_string_candidates = RefineMember {
        callee_monikers: Default::default(),
        symbol_id: 6,
        lang: Language::Rust,
        struct_hash: "duplicate-strings".to_string(),
        seq: vec![
            "string_literal".to_string(),
            "string_literal".to_string(),
            "string_content".to_string(),
        ],
        node_spans: vec![
            NodeSpan { start_byte: 0, end_byte: 4, kind: "string_literal", is_leaf: false },
            NodeSpan { start_byte: 0, end_byte: 4, kind: "string_literal", is_leaf: false },
            NodeSpan { start_byte: 1, end_byte: 2, kind: "string_content", is_leaf: true },
        ],
        text: Arc::from("\"a\""),
    };
    assert_eq!(widen_string_content_run(&duplicate_string_candidates, 2, 2), (0, 2));
}
