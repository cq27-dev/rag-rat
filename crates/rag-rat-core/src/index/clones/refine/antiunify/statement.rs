use super::super::RefineMember;
use super::alignment::{align_to_anchor_with_budget, resolve_anchor_idx};
use super::budget::CellBudget;
use super::build::anti_unify_with_budget;
use super::spans::direct_children;
use super::types::{ClassAlignment, EmittedSpan};
use crate::index::clones::normalize::NodeSpan;

/// `true` for a statement-like node kind — the granularity an inserted/removed statement snaps to.
/// Punctuation children of a block (`{`, `}`) are NOT statements, so they never become a snap unit.
fn is_statement_kind(kind: &str) -> bool {
    matches!(
        kind,
        "expression_statement"
            | "let_declaration"
            | "let_statement"
            | "return_statement"
            | "macro_invocation"
            | "if_expression"
            | "match_expression"
            | "for_expression"
            | "while_expression"
            | "loop_expression"
            | "item"
            | "use_declaration"
    )
}

/// The statement-like direct children of the anchor container `[lo..=hi]`, each as a
/// `(stmt_lo, stmt_hi)` column span. Non-statement children (the `{` / `}` punctuation) are
/// dropped.
fn anchor_statement_children(anchor: &RefineMember, lo: usize, hi: usize) -> Vec<(usize, usize)> {
    direct_children(anchor, lo, hi)
        .into_iter()
        .filter(|&(clo, _chi)| is_statement_kind(anchor.node_spans[clo].kind))
        .collect()
}

/// dropping leaf identity) plus its real source slice.
///
/// The skeleton — NOT the struct-hash — is the match key. A struct-hash includes alpha-rename ID
/// numbering and literal buckets, so two Type-2 variant statements (`a.map(x, y, x)` vs
/// `a.map(x, y, y)`, or `foo()` vs `bar()`) get DIFFERENT struct-hashes and would (wrongly) read as
/// an insert+delete instead of one matched, substituted statement. The kind-skeleton is leaf-blind:
/// same shape ⟹ matched, so the statement-snap only gaps GENUINELY inserted/removed statements.
struct MemberStatement {
    skeleton: String,
    source: String,
    /// The statement's token range in the MEMBER's own `seq`/`node_spans`: `[token_start ..
    /// token_start + token_len)`. Used by the matched-statement re-descent (Fix 2): a matched
    /// statement is anti-unified on JUST its own token sub-range, so an inner value/callee/type/
    /// literal diff inside a matched statement still surfaces as a classified VP.
    token_start: usize,
    token_len: usize,
}

/// The structural KIND-skeleton of a statement's token subsequence: the node kinds joined, leaf
/// identity dropped (an `ID<n>` leaf or `LIT_<KIND>` leaf folds to its node kind via the parallel
/// `node_spans`, so alpha-rename and literal-value differences don't change the skeleton). This is
/// the statement match key (see [`MemberStatement`]).
fn statement_skeleton(spans: &[NodeSpan]) -> String {
    let mut s = String::new();
    for sp in spans {
        s.push_str(sp.kind);
        s.push('\u{1}');
    }
    s
}

/// Extract a member's statement-like nodes within ITS block that corresponds to the anchor
/// container column `block_col`. Recovers the member's block node via the matched token
/// (`col_map[m][block_col]`), then walks the member's `node_spans` for the statement-kind direct
/// children of that block (byte-span nesting; one level deep). Each statement carries its
/// kind-skeleton (match key) and source slice — the inputs to the statement-level structural
/// alignment. `None` when the member did not match the block column (can't locate its block).
fn member_statements(
    member: &RefineMember,
    alignment: &ClassAlignment,
    m_idx: usize,
    block_col: usize,
) -> Option<Vec<MemberStatement>> {
    let block_j = alignment.col_map[m_idx][block_col]?;
    let block_end = member.node_spans[block_j].end_byte;

    let mut stmts = Vec::new();
    let mut j = block_j + 1;
    while j < member.node_spans.len() && member.node_spans[j].start_byte < block_end {
        let sp = &member.node_spans[j];
        if is_statement_kind(sp.kind) {
            // The statement subtree is the contiguous run of tokens inside this statement's span.
            let stmt_byte_end = sp.end_byte;
            let stmt_start = j;
            let mut k = j + 1;
            while k < member.node_spans.len() && member.node_spans[k].start_byte < stmt_byte_end {
                k += 1;
            }
            let source = member
                .text
                .get(sp.start_byte..sp.end_byte)
                .map(str::to_string)
                .unwrap_or_else(|| member.seq[stmt_start..k].join(" "));
            stmts.push(MemberStatement {
                skeleton: statement_skeleton(&member.node_spans[stmt_start..k]),
                source,
                token_start: stmt_start,
                token_len: k - stmt_start,
            });
            j = k;
        } else {
            j += 1;
        }
    }
    Some(stmts)
}

/// A member's statement matched (by kind-skeleton) to one anchor statement — its real source slice
/// plus its token range in the member's own `seq`/`node_spans`. The token range feeds the
/// matched-statement re-descent (Fix 2): anti-unifying the matched statement on JUST this
/// sub-range.
struct MatchedStmt {
    source: String,
    token_start: usize,
    token_len: usize,
}

/// Outcome of statement-level structural alignment for one member against the anchor statements.
struct MemberStmtAlign {
    /// `matched[i]` = the member's statement matched to anchor statement `i` (`Some` when the
    /// member has a structurally-equal statement; `None` when the member gaps it). Carries the
    /// source slice (the gapped-statement value) AND the member's token range (the re-descent
    /// sub-range, Fix 2).
    matched: Vec<Option<MatchedStmt>>,
    /// Member statements with no structural match in the anchor — pure member-only inserts, in
    /// member order. Each is `(after_anchor_stmt_idx, source)`: the inserted statement follows
    /// anchor statement index `after_anchor_stmt_idx` (`0` when it leads all statements). Used to
    /// attach a gapped metavar at the right anchor position.
    inserts: Vec<(usize, String)>,
}

/// Align a member's statements to the anchor statements by kind-skeleton LCS (clean: a whole
/// statement matches structurally or it doesn't — no closing-punctuation tangle; leaf-blind so a
/// Type-2 substituted statement still matches). Returns, per anchor statement, the member's
/// matching source (or `None` = gapped), plus the member-only inserted statements with the anchor
/// position they follow.
///
/// KNOWN LIMITATION — leftmost-tie attribution (residual; #235 GumTree is the real fix, NOT a
/// blocker). The match is by kind-SKELETON ([`lcs_pairs`]), and `lcs_pairs` resolves ties LEFTMOST.
/// When sibling statements share a skeleton — the COMMON bare-call case, where `a();`, `b();`,
/// `c();` all skeletonize identically — the LCS cannot tell WHICH statement a member has vs gaps,
/// so it mis-attributes the position. Concretely, `a(); c();` vs `a(); b(); c();` renders
/// `a(); c(); ⟨m0?⟩` with values `["", "c();"]` and coverage 1.0: the inserted MIDDLE `b();` is
/// dropped from all values and the coverage is falsely 1.0. Statements identical under
/// normalization (`a();`/`b();`/`c();` all normalize to `ID0();`) are genuinely indistinguishable
/// to LCS; the real fix is move-aware tree diff (GumTree, #235) — DO NOT attempt to fix it here,
/// and do not relax the indel gate or revert to the raw-straddle path to paper over it.
///
/// WHY THIS IS NON-BLOCKING (display fidelity only; never a scoring over-claim):
/// 1. `lcs_ratio` (the NiCad class fidelity) is computed INDEPENDENTLY from the raw `m.seq` token
///    sequences in `cache.rs` (`class_lcs_ratio`), NOT from this template/coverage — so a dropped
///    statement still depresses the pairwise LCS.
/// 2. A gapped VP is still emitted, and `confidence_v2`/`refactorability_v2` ALWAYS downgrade on
///    `gapped > 0`. So a false coverage 1.0 can never escalate confidence to High or
///    refactorability to full — it only fails to ADD an extra downgrade it arguably should have.
///    The damage is confined to which statement renders as fixed text, the gap value, and the
///    coverage column.
///
/// The test `indel_sibling_skeleton_tie_is_balanced_but_attribution_is_approximate` pins this
/// boundary.
fn align_member_statements(
    anchor_skeletons: &[String],
    member_stmts: &[MemberStatement],
) -> MemberStmtAlign {
    let a: Vec<&str> = anchor_skeletons.iter().map(String::as_str).collect();
    let b: Vec<&str> = member_stmts.iter().map(|s| s.skeleton.as_str()).collect();
    let pairs = lcs_pairs(&a, &b);

    let mut matched: Vec<Option<MatchedStmt>> = (0..anchor_skeletons.len()).map(|_| None).collect();
    let mut matched_b: Vec<bool> = vec![false; member_stmts.len()];
    for &(ai, bj) in &pairs {
        matched[ai] = Some(MatchedStmt {
            source: member_stmts[bj].source.clone(),
            token_start: member_stmts[bj].token_start,
            token_len: member_stmts[bj].token_len,
        });
        matched_b[bj] = true;
    }
    // Member-only inserts: each unmatched member statement, attributed after the most recent
    // matched anchor statement at or before it (0 when none precedes). Walk member statements
    // in order, tracking the running "after" position from the matched pairs.
    let mut inserts = Vec::new();
    let mut after = 0usize;
    let mut pair_iter = pairs.iter().peekable();
    for (bj, stmt) in member_stmts.iter().enumerate() {
        while let Some(&&(ai, pbj)) = pair_iter.peek() {
            if pbj <= bj {
                after = ai + 1;
                pair_iter.next();
                if pbj == bj {
                    break;
                }
            } else {
                break;
            }
        }
        if !matched_b[bj] {
            inserts.push((after, stmt.source.clone()));
        }
    }
    MemberStmtAlign { matched, inserts }
}

/// LCS over two `&str` slices → the matched `(ai, bj)` index pairs, ascending. A tiny DP (statement
/// counts are small — ≤ a function body's statement count). Deterministic.
///
/// Tie-break is LEFTMOST (the `>=` in the backtrack). When several elements are equal (skeleton-LCS
/// over sibling bare calls) the leftmost match wins, which mis-attributes WHICH statement a member
/// gaps — see the KNOWN LIMITATION note on [`align_member_statements`] (display-fidelity residual,
/// GumTree #235 is the real fix, non-blocking for scoring).
fn lcs_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] =
                if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Statement-snap a block-level Type-3 indel (rule 5). Returns `true` when it handled the node
/// (emitted clean statement metavars / left matched statements fixed), `false` when there is no
/// statement-level indel here (every anchor statement matched by every aligned member, no
/// member-only insert) — the caller then falls through to the generic decomposable path (rule 6)
/// for plain substitutions.
///
/// The headline correctness fix (#215 Plan 4b): an inserted statement whose neighbour ends in `();`
/// tangles the flat token LCS (`( ) ;` matches across the statement boundary), so the raw col_map
/// produces mangled, ordinal-scrambled per-member values and leaks single-member content into fixed
/// template text. Here we re-derive at STATEMENT granularity via kind-skeleton LCS, so:
/// - a matched statement stays FIXED (its tangled within-statement col_map is NOT trusted; a
///   structurally-equal statement has no genuine sub-variation, and a co-occurring literal/callee
///   diff in a matched neighbour is a documented simplification — Fix-2/differing-callee still fire
///   in non-indel blocks, which take rule 6),
/// - a statement some member gaps becomes ONE gapped metavar over its WHOLE span with balanced,
///   ordinal-correct values (`""` for the absent member, the whole statement for the present one),
/// - a member-only inserted statement becomes ONE zero-width gapped metavar attributed at the
///   anchor position it follows.
#[allow(clippy::too_many_arguments)] // threads the shared cell budget + sample flag for re-descent.
pub(super) fn emit_block_statement_indel(
    members: &[RefineMember],
    alignment: &ClassAlignment,
    anchor: &RefineMember,
    _is_fixed: &[bool],
    lo: usize,
    hi: usize,
    budget: &mut CellBudget,
    redescent_sampled: &mut bool,
    out: &mut Vec<EmittedSpan>,
) -> bool {
    let stmt_children = anchor_statement_children(anchor, lo, hi);
    if stmt_children.is_empty() {
        return false;
    }
    let anchor_skeletons: Vec<String> = stmt_children
        .iter()
        .map(|&(slo, shi)| statement_skeleton(&anchor.node_spans[slo..=shi]))
        .collect();

    // Gather each ALIGNED non-anchor member's own statements. A member whose block we can't locate
    // (didn't match the block column) is excluded — it can't witness a clean statement indel.
    // Skipped (cost-capped) members are likewise excluded.
    let mut member_stmt_lists: Vec<Option<Vec<MemberStatement>>> =
        Vec::with_capacity(members.len());
    for (m_idx, _member) in members.iter().enumerate() {
        if !alignment.aligned[m_idx] || m_idx == alignment.anchor_idx {
            member_stmt_lists.push(None);
        } else {
            member_stmt_lists.push(member_statements(&members[m_idx], alignment, m_idx, lo));
        }
    }

    // INDEL GATE: only snap when an aligned member's statement COUNT differs from the anchor's —
    // the honest signal of an inserted/removed statement. When every aligned member has the
    // same count, each statement is a 1:1 positional SUBSTITUTION (e.g. `g(process(x))` vs
    // `g(y.trim())` — one statement, different internal structure), NOT an indel: defer to rule
    // 6 so the substitution is classified normally. This is what keeps the snap from
    // mis-reading a substituted statement as a gapped insert+delete.
    let anchor_count = stmt_children.len();
    let count_differs = member_stmt_lists
        .iter()
        .enumerate()
        .any(|(m, l)| alignment.aligned[m] && l.as_ref().is_some_and(|s| s.len() != anchor_count));
    if !count_differs {
        return false;
    }

    // Per aligned member, align its statements to the anchor statements (kind-skeleton LCS).
    let mut per_member: Vec<Option<MemberStmtAlign>> = Vec::with_capacity(members.len());
    for (m_idx, list) in member_stmt_lists.into_iter().enumerate() {
        if !alignment.aligned[m_idx] {
            per_member.push(None);
            continue;
        }
        if m_idx == alignment.anchor_idx {
            // The anchor matches every statement by identity (its own columns); no inserts.
            per_member.push(Some(MemberStmtAlign {
                matched: stmt_children
                    .iter()
                    .map(|&(slo, shi)| {
                        anchor
                            .text
                            .get(anchor.node_spans[slo].start_byte..anchor.node_spans[shi].end_byte)
                            .map(|source| MatchedStmt {
                                source: source.to_string(),
                                token_start: slo,
                                token_len: shi - slo + 1,
                            })
                    })
                    .collect(),
                inserts: Vec::new(),
            }));
            continue;
        }
        per_member.push(list.map(|stmts| align_member_statements(&anchor_skeletons, &stmts)));
    }

    // Is there a genuine statement-level indel? (some anchor statement gapped by an aligned member,
    // OR some member-only inserted statement). The count gate above already implies one, but a
    // skeleton mismatch could in principle align everything 1:1 — re-check to be safe.
    let any_gap = per_member
        .iter()
        .flatten()
        .any(|a| a.matched.iter().any(Option::is_none) || !a.inserts.is_empty());
    if !any_gap {
        return false;
    }

    // One EmittedSpan per anchor statement that some member gaps; matched statements stay FIXED
    // (a gapped statement's tangled within-statement col_map is not trusted), but a MATCHED
    // statement is RE-DESCENDED (Fix 2): it is anti-unified on JUST its own token sub-range so an
    // inner value/callee/type/literal diff inside it still surfaces as a classified VP.
    for (si, &(slo, shi)) in stmt_children.iter().enumerate() {
        let gapped_by_some = per_member.iter().enumerate().any(|(m, a)| {
            alignment.aligned[m] && a.as_ref().is_some_and(|a| a.matched[si].is_none())
        });
        if gapped_by_some {
            // Whole-statement gapped metavar: ordinal-correct, balanced (whole statement or "").
            let per_member_values: Vec<String> = (0..members.len())
                .map(|m| match per_member[m].as_ref() {
                    Some(a) => a.matched[si].as_ref().map(|s| s.source.clone()).unwrap_or_default(),
                    None => String::new(),
                })
                .collect();
            out.push(EmittedSpan::Statement {
                lo: slo,
                hi: shi,
                per_member_values,
                zero_width: false,
            });
        } else {
            // MATCHED by every aligned member (Fix 2): re-descend into the statement's own token
            // sub-range so an inner diff (`p(1)` vs `p(2)`, a differing callee, a type swap) is
            // anti-unified and emitted as a classified VP — instead of being silently dropped by
            // leaving the whole matched statement fixed. The re-descent draws from the SHARED
            // per-class `budget`: once it is exhausted it leaves the statement whole-fixed (no
            // extra exact DP) and latches `redescent_sampled` so the class is honestly
            // flagged.
            emit_matched_statement_redescent(
                members,
                alignment.anchor_idx,
                &per_member,
                si,
                slo,
                shi,
                budget,
                redescent_sampled,
                out,
            );
        }
    }

    // Member-only inserted statements (anchor lacks them entirely) → zero-width gapped metavars.
    emit_member_only_inserts(&stmt_children, &per_member, members.len(), out);
    true
}

/// Re-descend a MATCHED anchor statement `si` (anchor columns `[slo..=shi]`) that every aligned
/// member realises with a structurally-equal statement — anti-unify it on JUST its own token
/// sub-range so an inner value/callee/type/literal diff still surfaces as a classified VP (Fix 2,
/// #215 Plan 4b Codex round-5).
///
/// WHY a fresh sub-alignment, not the global col_map: the statement-snap engages precisely because
/// the flat token LCS is TANGLED across statement boundaries (closing `();`/`}` matches the wrong
/// statement), so the global `alignment.col_map` cannot be trusted inside an indel block. WITHIN a
/// matched statement the structure is clean, so we re-extract each member's statement as a
/// sub-member (a slice of its `seq`/`node_spans` keeping the same ABSOLUTE byte offsets, so
/// recovered source and rendered text stay correct) and run the SAME `align_to_anchor` +
/// `anti_unify` pipeline on the sub-members. Each sub-template VP is then translated back into the
/// parent anchor's column space (sub-column `c` becomes parent column `slo + c`) and full member
/// ordinality (a member that gaps this statement, or is cost-skipped, contributes `""`), then
/// emitted as a pre-classified `EmittedSpan::Classified`.
///
/// SUB-ANCHOR = THE PARENT ANCHOR'S OWN STATEMENT (Fix 1, Codex round-7). The translation
/// `parent_lo = slo + sub_col` is exact ONLY when the sub-template's columns ARE
/// `anchor.node_spans[slo..=shi]` — i.e. the sub-alignment is anchored on the PARENT anchor's
/// (medoid's) statement slice, not on the canonical-first matched member. When the medoid is not
/// canonical member 0, anchoring on the first sub-member would spine the sub-alignment on a
/// different member's statement and shift `slo + sub_col` onto the wrong medoid columns. We pass
/// `parent_anchor_idx` (`alignment.anchor_idx`) and use the sub-member it built as the sub-anchor.
#[allow(clippy::too_many_arguments)] // threads the shared cell budget + sample flag.
fn emit_matched_statement_redescent(
    members: &[RefineMember],
    parent_anchor_idx: usize,
    per_member: &[Option<MemberStmtAlign>],
    si: usize,
    slo: usize,
    shi: usize,
    budget: &mut CellBudget,
    redescent_sampled: &mut bool,
    out: &mut Vec<EmittedSpan>,
) {
    // Aggregate cell budget (shared with the parent star-align): once the per-class budget is
    // exhausted, STOP re-descending matched statements — leave this one whole-fixed (the documented
    // pre-Fix-2 behavior: an inner diff inside it is not surfaced as a VP) instead of running
    // another exact sub-DP. Latch `redescent_sampled` so the class is honestly flagged sampled.
    // This is what bounds the WHOLE per-class anti-unify (parent align + all re-descents) by
    // ONE budget.
    if budget.exhausted {
        *redescent_sampled = true;
        return;
    }
    // Build a sub-member per member that matched this statement: a slice of its own seq/node_spans
    // for the statement's token range (same ABSOLUTE byte offsets). `sub_to_full[k]` maps the k-th
    // sub-member back to its full member ordinal, so the sub-template's per_member_values can be
    // re-expanded. Sub-members preserve the parent's canonical order, so the sub-anchor is stable.
    //
    // `sub_anchor_pos` records WHERE in `sub_members` the PARENT anchor's (medoid's) own statement
    // landed — it is the sub-anchor (Fix 1, #215 Plan 4b Codex round-7). The translation below maps
    // a sub-column `c` to parent column `slo + c`, which is exact ONLY when the sub-anchor's
    // `node_spans` ARE `anchor.node_spans[slo..=shi]` — i.e. the sub-anchor is the parent anchor's
    // own statement slice. Using the canonical-first sub-member instead would, when the medoid is
    // not canonical member 0, anchor the sub-alignment on a DIFFERENT member's statement spine, so
    // `slo + sub_col` would index the medoid anchor's columns at the wrong positions → wrong
    // rendered/classified source. The parent anchor always matches its own statement (it is the
    // anchor; `per_member[anchor_idx].matched[si]` is always `Some`), so it is always present here.
    let mut sub_members: Vec<RefineMember> = Vec::new();
    let mut sub_to_full: Vec<usize> = Vec::new();
    let mut sub_anchor_pos: Option<usize> = None;
    for (m_idx, member) in members.iter().enumerate() {
        let Some(matched) = per_member[m_idx].as_ref().and_then(|a| a.matched[si].as_ref()) else {
            continue;
        };
        let start = matched.token_start;
        let end = start + matched.token_len;
        if end > member.seq.len() || end > member.node_spans.len() || start >= end {
            // Defensive: a malformed range can't be re-descended — bail on the whole statement
            // rather than emit a partial/incorrect sub-template (the statement stays fixed).
            return;
        }
        if m_idx == parent_anchor_idx {
            sub_anchor_pos = Some(sub_members.len());
        }
        sub_members.push(RefineMember {
            symbol_id: member.symbol_id,
            lang: member.lang,
            struct_hash: member.struct_hash.clone(),
            seq: member.seq[start..end].to_vec(),
            node_spans: member.node_spans[start..end].to_vec(),
            text: member.text.clone(),
        });
        sub_to_full.push(m_idx);
    }
    // Need ≥2 sub-members to have any variation; with <2 there is nothing to anti-unify.
    if sub_members.len() < 2 {
        return;
    }
    // The sub-anchor MUST be the parent anchor's own statement slice (Fix 1): only then are the
    // sub-template's columns genuinely `anchor.node_spans[slo + c]`, making `parent_lo = slo +
    // sub_col` exact. Defensive fall back to the canonical-first sub-member if the parent anchor's
    // statement is somehow absent (it never is — the anchor matches every statement by identity).
    let sub_anchor_idx = sub_anchor_pos.unwrap_or_else(|| resolve_anchor_idx(&sub_members, None));

    // Anti-unify the sub-members with the SAME pipeline (the reuse the prompt sanctions), threading
    // the SHARED per-class `budget` so the sub-align's cells and any deeper (nested-statement)
    // re-descent draw from the one budget — NOT a fresh one.
    let sub_alignment = align_to_anchor_with_budget(&sub_members, sub_anchor_idx, budget);
    // The sub-align may itself have hit the budget (a member skipped past the cutover) → fold its
    // sampling into the class flag; the budget-aware `anti_unify_with_budget` uses the same shared
    // budget for any deeper re-descent (no re-seed — the sub-alignment's cells are already
    // charged).
    *redescent_sampled |= sub_alignment.sampled;
    let sub_template = anti_unify_with_budget(&sub_members, &sub_alignment, budget);
    *redescent_sampled |= sub_template.sampled;

    // Translate each sub-VP into the parent: shift each occurrence column by `slo` (the
    // sub-anchor's node_spans are exactly `anchor.node_spans[slo..=shi]`, so sub-column c is
    // parent column slo+c) and re-expand per_member_values to full member ordinality. Each VP
    // is emitted as a pre-classified span carrying its kind/values verbatim (the col_map for
    // the parent's tangled indel block is NOT consulted — the sub-template already classified
    // cleanly).
    for vp in &sub_template.variation_points {
        // Re-expand sub-ordinal values to full member ordinality: a member absent from the
        // sub-class (gapped this statement, or cost-skipped) contributes "".
        let mut full_values = vec![String::new(); members.len()];
        for (sub_idx, &full_idx) in sub_to_full.iter().enumerate() {
            if let Some(v) = vp.per_member_values.get(sub_idx) {
                full_values[full_idx] = v.clone();
            }
        }
        for &sub_col in &vp.occurrences {
            let parent_lo = slo + sub_col;
            if parent_lo > shi {
                continue;
            }
            // Carry the sub-VP's ACTUAL snapped span + zero-width flag (Fix 2, Codex round-7) from
            // `occurrence_spans` rather than re-deriving `hi` from the parent subtree at
            // `parent_lo`. Re-derivation TRUNCATED a straddling multi-subtree hole (a span wider
            // than the one subtree at its start column — e.g. a differing `a + b` vs `c * d` that
            // snapped wider than `parent_lo`'s subtree) and turned a zero-width member-only insert
            // (a statement absent from the anchor's matched statement) into a CONSUMING hole. The
            // sub-template's columns ARE `anchor.node_spans[slo..]` (sub-anchor = parent anchor's
            // statement, Fix 1), so `slo + sub_hi` is the correct parent hi; bound by `shi`.
            // Defensive fall back to a single-leaf span if the occurrence is somehow missing.
            let occ = sub_template.occurrence_spans.get(&sub_col).copied();
            let parent_hi = occ.map(|o| slo + o.hi).unwrap_or(parent_lo).min(shi);
            let zero_width = occ.is_some_and(|o| o.zero_width);
            out.push(EmittedSpan::Classified {
                lo: parent_lo,
                hi: parent_hi,
                per_member_values: full_values.clone(),
                kind: vp.kind,
                type_hint: vp.type_hint.clone(),
                confidence: vp.confidence,
                differing_callee: vp.differing_callee,
                zero_width,
            });
        }
    }
}

/// Emit zero-width gapped metavars for member-only inserted statements — statements present in some
/// members' source but ABSENT from the anchor (anchor is the shorter member). Each distinct
/// follow-position becomes one metavar attributed at the END column of the anchor statement it
/// follows (or the first statement's start when it leads), with per-member values = the inserted
/// source (or `""`).
fn emit_member_only_inserts(
    stmt_children: &[(usize, usize)],
    per_member: &[Option<MemberStmtAlign>],
    member_count: usize,
    out: &mut Vec<EmittedSpan>,
) {
    let mut positions: Vec<usize> = per_member
        .iter()
        .flatten()
        .flat_map(|a| a.inserts.iter().map(|&(after, _)| after))
        .collect();
    positions.sort_unstable();
    positions.dedup();

    for after in positions {
        let per_member_values: Vec<String> = (0..member_count)
            .map(|m| match per_member[m].as_ref() {
                Some(a) => a
                    .inserts
                    .iter()
                    .filter(|&&(p, _)| p == after)
                    .map(|(_, s)| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => String::new(),
            })
            .collect();
        // Attach at the END column of the statement this insert follows (renders right after it),
        // or the first statement's lo when it leads. `after` is a statement INDEX;
        // `after-1` precedes.
        let attach = if after == 0 {
            stmt_children.first().map(|&(slo, _)| slo).unwrap_or(0)
        } else {
            stmt_children
                .get(after - 1)
                .map(|&(_, shi)| shi)
                .or_else(|| stmt_children.last().map(|&(_, shi)| shi))
                .unwrap_or(0)
        };
        out.push(EmittedSpan::Statement {
            lo: attach,
            hi: attach,
            per_member_values,
            zero_width: true,
        });
    }
}

#[cfg(test)]
mod coverage_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::language::Language;

    fn span(start_byte: usize, end_byte: usize, kind: &'static str, is_leaf: bool) -> NodeSpan {
        NodeSpan { start_byte, end_byte, kind, is_leaf }
    }

    fn fixture_member(
        symbol_id: i64,
        text: &str,
        seq: Vec<&str>,
        node_spans: Vec<NodeSpan>,
    ) -> RefineMember {
        RefineMember {
            symbol_id,
            lang: Language::Rust,
            struct_hash: seq.join("\u{1}"),
            seq: seq.into_iter().map(str::to_string).collect(),
            node_spans,
            text: Arc::from(text),
        }
    }

    fn statement_parts(span: &EmittedSpan) -> Option<(usize, usize, Vec<String>, bool)> {
        match span {
            EmittedSpan::Statement { lo, hi, per_member_values, zero_width } =>
                Some((*lo, *hi, per_member_values.clone(), *zero_width)),
            _ => None,
        }
    }

    #[test]
    fn statement_alignment_and_member_insert_edges_are_covered() {
        let anchor_skeletons = vec!["A".to_string(), "B".to_string()];
        let member_stmts = vec![
            MemberStatement {
                skeleton: "A".to_string(),
                source: "a();".to_string(),
                token_start: 0,
                token_len: 1,
            },
            MemberStatement {
                skeleton: "X".to_string(),
                source: "x();".to_string(),
                token_start: 1,
                token_len: 1,
            },
            MemberStatement {
                skeleton: "B".to_string(),
                source: "b();".to_string(),
                token_start: 2,
                token_len: 1,
            },
        ];

        let aligned = align_member_statements(&anchor_skeletons, &member_stmts);
        assert_eq!(aligned.matched.len(), 2);
        assert_eq!(aligned.matched[0].as_ref().map(|s| s.source.as_str()), Some("a();"));
        assert_eq!(aligned.matched[1].as_ref().map(|s| s.source.as_str()), Some("b();"));
        assert_eq!(aligned.inserts, vec![(1, "x();".to_string())]);

        let per_member = vec![
            None,
            Some(MemberStmtAlign {
                matched: Vec::new(),
                inserts: vec![(0, "lead();".to_string()), (2, "tail();".to_string())],
            }),
        ];
        let mut emitted = Vec::new();
        emit_member_only_inserts(&[(10, 12), (20, 22)], &per_member, 2, &mut emitted);

        assert_eq!(statement_parts(&EmittedSpan::Raw(0, 0)), None);
        assert_eq!(emitted.len(), 2);
        assert_eq!(
            statement_parts(&emitted[0]),
            Some((10, 10, vec!["".to_string(), "lead();".to_string()], true))
        );
        assert_eq!(
            statement_parts(&emitted[1]),
            Some((22, 22, vec!["".to_string(), "tail();".to_string()], true))
        );
    }

    #[test]
    fn block_statement_indel_handles_empty_and_unaligned_members() {
        let lone_leaf = fixture_member(1, "x", vec!["ID0"], vec![span(0, 1, "identifier", true)]);
        let one_member_alignment = ClassAlignment {
            anchor_idx: 0,
            sampled: false,
            aligned: vec![true],
            col_map: vec![vec![Some(0)]],
            member_inserts: vec![BTreeMap::new()],
            spent_cells: 0,
        };
        let mut budget = CellBudget::new(100);
        let mut sampled = false;
        let mut emitted = Vec::new();
        assert!(!emit_block_statement_indel(
            std::slice::from_ref(&lone_leaf),
            &one_member_alignment,
            &lone_leaf,
            &[true],
            0,
            0,
            &mut budget,
            &mut sampled,
            &mut emitted,
        ));

        let anchor =
            fixture_member(10, "{a;}", vec!["block", "expression_statement", "ID0", ";"], vec![
                span(0, 4, "block", false),
                span(1, 3, "expression_statement", false),
                span(1, 2, "identifier", true),
                span(2, 3, ";", true),
            ]);
        let member_without_statement =
            fixture_member(11, "{}", vec!["block"], vec![span(0, 2, "block", false)]);
        let unaligned_member =
            fixture_member(12, "{b;}", vec!["block", "expression_statement", "ID0", ";"], vec![
                span(0, 4, "block", false),
                span(1, 3, "expression_statement", false),
                span(1, 2, "identifier", true),
                span(2, 3, ";", true),
            ]);
        let members = vec![anchor, member_without_statement, unaligned_member];
        let alignment = ClassAlignment {
            anchor_idx: 0,
            sampled: false,
            aligned: vec![true, true, false],
            col_map: vec![
                vec![Some(0), Some(1), Some(2), Some(3)],
                vec![Some(0), None, None, None],
                vec![None, None, None, None],
            ],
            member_inserts: vec![BTreeMap::new(), BTreeMap::new(), BTreeMap::new()],
            spent_cells: 0,
        };
        let mut budget = CellBudget::new(100);
        let mut sampled = false;
        let mut emitted = Vec::new();

        assert!(emit_block_statement_indel(
            &members,
            &alignment,
            &members[0],
            &[true, true, true, true],
            0,
            3,
            &mut budget,
            &mut sampled,
            &mut emitted,
        ));

        assert_eq!(emitted.len(), 1);
        assert_eq!(
            statement_parts(&emitted[0]),
            Some((1, 3, vec!["a;".to_string(), String::new(), String::new()], false))
        );
    }

    #[test]
    fn matched_statement_redescent_defensive_returns_are_covered() {
        let short = fixture_member(20, "a", vec!["ID0"], vec![span(0, 1, "identifier", true)]);
        let malformed = vec![Some(MemberStmtAlign {
            matched: vec![Some(MatchedStmt {
                source: "a".to_string(),
                token_start: 0,
                token_len: 2,
            })],
            inserts: Vec::new(),
        })];
        let mut budget = CellBudget::new(100);
        let mut sampled = false;
        let mut emitted = Vec::new();
        emit_matched_statement_redescent(
            std::slice::from_ref(&short),
            0,
            &malformed,
            0,
            0,
            0,
            &mut budget,
            &mut sampled,
            &mut emitted,
        );
        assert!(emitted.is_empty());

        let sparse_anchor =
            fixture_member(20, "a", vec!["ID0"], vec![span(0, 1, "identifier", true)]);
        let skipped_member =
            fixture_member(21, "b", vec!["ID0"], vec![span(0, 1, "identifier", true)]);
        let sparse_members = vec![sparse_anchor, skipped_member];
        let sparse = vec![
            Some(MemberStmtAlign {
                matched: vec![Some(MatchedStmt {
                    source: "a".to_string(),
                    token_start: 0,
                    token_len: 1,
                })],
                inserts: Vec::new(),
            }),
            None,
        ];
        let mut budget = CellBudget::new(100);
        let mut sampled = false;
        emit_matched_statement_redescent(
            &sparse_members,
            0,
            &sparse,
            0,
            0,
            0,
            &mut budget,
            &mut sampled,
            &mut emitted,
        );
        assert!(emitted.is_empty());

        let parent_anchor = fixture_member(30, "ab", vec!["fixed", "A"], vec![
            span(0, 1, "identifier", true),
            span(1, 2, "identifier", true),
        ]);
        let other = fixture_member(31, "ac", vec!["fixed", "B"], vec![
            span(0, 1, "identifier", true),
            span(1, 2, "identifier", true),
        ]);
        let parent_members = vec![parent_anchor, other];
        let per_member = vec![
            Some(MemberStmtAlign {
                matched: vec![Some(MatchedStmt {
                    source: "ab".to_string(),
                    token_start: 0,
                    token_len: 2,
                })],
                inserts: Vec::new(),
            }),
            Some(MemberStmtAlign {
                matched: vec![Some(MatchedStmt {
                    source: "ac".to_string(),
                    token_start: 0,
                    token_len: 2,
                })],
                inserts: Vec::new(),
            }),
        ];
        let mut budget = CellBudget::new(100);
        let mut sampled = false;
        emit_matched_statement_redescent(
            &parent_members,
            0,
            &per_member,
            0,
            0,
            0,
            &mut budget,
            &mut sampled,
            &mut emitted,
        );
        assert!(
            emitted.is_empty(),
            "a sub-template occurrence beyond the parent span is defensively ignored"
        );
    }
}
