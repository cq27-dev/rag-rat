//! Coherence split: break an over-merged union-find component into internally-coherent clone
//! classes.
//!
//! # Why a clique cover, not greedy first-fit or connected-components of the θ-graph
//!
//! Union-find over-merges transitive chains (A~B edge, B~C edge, A!~C → {A,B,C} component).
//! Connected components of the θ-graph suffer the same flaw: a chain A–B–C is one connected
//! component even when A and C are below θ, so re-running connected components on the θ-graph
//! just reproduces the problem. We need **internally-coherent** classes — every pair within a
//! returned class must be ≥ θ.
//!
//! Greedy FIRST-FIT (the prior implementation) achieves coherence but LOSES classes: in chain
//! A~B, B~C, A!~C the member B joins the first class {A,B}, then C cannot join {A,B} (A!~C) and is
//! dropped as a singleton — the perfectly valid {B,C} pair is never emitted. The fix (#215 Plan 4a)
//! is a greedy MAXIMAL CLIQUE COVER: every coherent edge seeds a maximal coherent group, so B lands
//! in BOTH {A,B} and {B,C} (overlap is correct — a member can be coherent with two otherwise-
//! incompatible peers). Each emitted group is still internally coherent by construction (a member
//! is added only when it is ≥ θ to EVERY current group member).
//!
//! # Scalability: the caller threads in the θ-verified edge list (#256)
//!
//! The clique cover seeds a group from every θ-edge. The edges are already known at the call site
//! (the `candidate_pairs_from_bags` output, restricted to the component), so this module takes
//! them as `edges` instead of rediscovering them with an O(n²) all-pairs scan. That all-pairs scan
//! was the ONLY reason a `SPLIT_MAX` member cap existed — and that cap returned any giant
//! (>200-member) component WHOLE, so a 2,575-member transitive-chain blob never split and ranked
//! #1 with coverage 0.00 (#256). With edge-fed seeding the cover is O(edges) to seed + O(group ×
//! members) to grow each clique, so the member cap is gone: a sparse chained giant breaks into its
//! real small cliques, while a genuinely dense ≥200-member clique (every pair ≥ θ) still collapses
//! to one class. NO member is lost — every θ-edge endpoint lands in at least its own seed clique.

/// Budget on the number of GROWN maximal cliques before the cover stops growing and falls back to
/// emitting the remaining θ-edges as ungrown 2-member cliques (#256). It bounds the superlinear
/// maximal-subset-removal pass (O(grown² × n)) — to a sub-second budget (256² × n) on a
/// pathologically tangled component (e.g. dense-minus-perfect-matching, which yields O(n²) maximal
/// cliques). The per-edge "already covered" containment test is no longer superlinear: it is a
/// small-list intersection over each endpoint's group-index list (`member_groups`) — O(1) in the
/// dense case where every member is in exactly one group (the old `Vec::contains` scan over every
/// group made a DENSE clique O(n³), #256).
///
/// CRUCIAL (#256): tripping the budget does NOT drop members and does NOT return the over-merged
/// component (the old behavior that resurrected the giant). Once the budget trips, the tail emits a
/// COVERING SUBSET of the remaining θ-edges — a 2-member clique only for an edge that covers a
/// still-uncovered member (#282) — so EVERY member that has a θ-edge still lands in at least one
/// coherent class. A long transitive CHAIN (n−1 edges, far more than 256 grown cliques) therefore
/// keeps all members: each member is covered by a grown clique or by the first tail edge that
/// reaches it. Only the (rare) GROWN-clique expansion is bounded, never coverage.
///
/// #282: the OLD tail emitted EVERY remaining edge, so a dense over-merged giant (cargo's 3.1k-node
/// component: ~92k edges) shattered into O(edges) ≈ 87k bare 2-member classes — `build_class`-ing
/// all of them was the dominant cold `find_clones` cost. The covering subset emits ≤ O(uncovered
/// members) pairs (each one covers ≥1 new member), bounding the tail to the component size while
/// keeping symbol-level recall identical: a skipped edge has BOTH endpoints already in a class, so
/// it is a redundant clone PAIR, never a lost member. For the normal case (all existing tests) far
/// fewer than 256 grown cliques are ever produced and the tail is empty.
const MAX_SPLIT_GROUPS: usize = 256;

/// Split an over-merged union-find component into internally-coherent clone classes: every pair
/// within a returned class has pairwise similarity >= theta.  Union-find over-merges transitive
/// chains (A~B, B~C, A!~C ⇒ {A,B,C}); this returns coherent sub-classes instead, via a greedy
/// maximal clique cover that keeps EVERY coherent group (so a member shared by two incompatible
/// peers, like B in chain A~B / B~C / A!~C, appears in both {A,B} and {B,C}).
///
/// `edges` is the θ-verified candidate-pair subset for THIS component (a `(a, b)` per pair, in any
/// order/orientation — the caller bucketed `candidate_pairs_from_bags` per component, #256). Every
/// edge endpoint must be a member of `component`. The clique cover seeds a group from each edge, so
/// feeding the precomputed edges makes seeding O(edges) instead of the old O(n²) all-pairs scan
/// (the reason the now-removed `SPLIT_MAX` member cap existed). The GROW step decides whether a
/// candidate member coheres with a group member by EDGE ADJACENCY against `edges` (an O(1) HashSet
/// lookup, #258), since every edge IS a ≥ θ pair (the forward direction is unconditional) and on a
/// recall-safe corpus `edges` IS exactly the ≥ θ pair set for this component — no overlap
/// recompute. θ itself is therefore no longer a parameter: the edge set fully encodes the
/// threshold the caller verified at, so the split needs only the edges (coherence) and `similarity`
/// (seed ranking). `similarity(a, b)` returns the pairwise overlap/max similarity in \[0,1\]; it is
/// consulted ONLY to rank the seed order (descending similarity, O(edges)), never per-(m, g) in the
/// grow — so a dense (near-)clique component is O(n²) lookups, not O(n² · token_len) recomputes.
///
/// Deterministic: edges are canonicalized to `a < b` and sorted, members are processed in
/// ascending-id order, group members are sorted ascending, and returned classes are in stable order
/// (by their lowest member id). Classes of size < 2 are dropped (a clone class needs ≥ 2 members).
///
/// # Algorithm (greedy maximal clique cover)
///
/// 1. Sort members ascending by id (determinism).
/// 2. Canonicalize + dedup `edges`, then order the seed list by DESCENDING pairwise similarity
///    (tie-break `(a, b)`) so tight real cliques grow before `MAX_SPLIT_GROUPS` trips (#256 R-A).
/// 3. For each coherent edge not already fully contained in an emitted group, grow a maximal
///    coherent group from `{a, b}` by adding any remaining member (in id order) that has a θ-edge
///    to every current group member (an edge-adjacency check against `edges`, #258 —
///    output-identical to the old `≥ θ` recompute when `edges` is exactly the ≥ θ set; see the
///    parity caveat on the `adjacency` block re: #271's hot-token cap). (The grow scans members in
///    id order; only the SEED order is similarity-ranked.) The cover is greedy and therefore
///    seed-order-dependent, but: no member is ever dropped (the union of classes is order-invariant
///    — post-budget endpoints become bare pairs), and on a single connected component (the only
///    shape this receives — callers iterate per union-find component) descending-sim cover quality
///    is ≥ id-order, never worse.
/// 4. Drop groups that are a strict subset of another (keep only maximal cliques), de-duplicate
///    identical groups, drop singletons, and sort by lowest member id.
///
/// # Postcondition guaranteed by construction
///
/// Every returned class is internally coherent (all pairs ≥ theta): a member is only added to a
/// group after it has passed the coherence check against every existing member, so the all-pairs
/// property holds across all insertions.
pub(crate) fn coherence_split(
    component: &[i64],
    edges: &[(i64, i64)],
    similarity: impl Fn(i64, i64) -> f64,
) -> Vec<Vec<i64>> {
    // 1. Sort ascending (determinism).
    let mut members: Vec<i64> = component.to_vec();
    members.sort_unstable();
    // `HashSet` (O(1) `contains`), not `BTreeSet`: this is the defensive membership filter for the
    // edge list below, hit twice per edge — on a dense giant that is ~2·n² lookups, where O(log n)
    // per lookup (BTreeSet) is needlessly slow (#256). Membership-only, never iterated, so
    // determinism is unaffected.
    let member_set: std::collections::HashSet<i64> = members.iter().copied().collect();

    // 2. Coherent-edge seed list from the caller's θ-verified pairs (#256): canonicalize each to
    // `a < b`, keep only edges whose BOTH endpoints are members of this component (defensive — the
    // caller buckets per component), sort + dedup for a deterministic seed order. These ARE the
    // ≥ θ edges (verified by `candidate_pairs_from_bags` at the call site), so there is no
    // all-pairs similarity scan here — that scan was the reason for the removed `SPLIT_MAX`
    // member cap.
    let mut coherent_edges: Vec<(i64, i64)> = edges
        .iter()
        .filter_map(|&(a, b)| {
            if a == b || !member_set.contains(&a) || !member_set.contains(&b) {
                return None;
            }
            Some((a.min(b), a.max(b)))
        })
        .collect();
    coherent_edges.sort_unstable();
    coherent_edges.dedup();

    // 2a. Adjacency set over the canonical θ-edges, for the O(1) GROW coherence check (#258).
    //
    // The GROW step below adds a candidate member `m` to a group only when `m` coheres with EVERY
    // current group member `g`. The pre-#258 check RE-CALLED `similarity(m, g) >= theta` per (m,
    // g), recomputing `overlap`/`max_len` (an O(token_len) token-bag merge) — so on a dense
    // (near-)clique component the grow cost was O(n² · token_len). But `coherent_edges` ALREADY
    // IS the set of pairs with similarity ≥ θ for this component: the caller threads in
    // `candidate_pairs_from_bags` restricted to the component, i.e. every `verified_clone(a, b,
    // θ)` pair (`overlap ≥ ceil(θ·max_len)`) plus every struct-hash pair (identical bags ⇒
    // similarity 1.0). So "`m` coheres with `g`" ⟺ "the canonical edge `(min, max)` is in
    // `coherent_edges`" — an O(1) HashSet lookup with NO overlap recompute. Seeding (step 2b)
    // still consults `similarity` once per edge (O(edges)) to rank tight cliques first; only the
    // per-(m, g) GROW recompute is eliminated.
    //
    // PARITY (when this is output-identical to the old per-(m, g) `similarity >= θ` grow):
    //   FORWARD (edge ⟹ similarity ≥ θ) is UNCONDITIONAL — `verified_clone` gates `overlap ≥
    //   ceil(θ·max_len)`, which is bit-for-bit `overlap/max_len ≥ θ` (`overlap` is a non-negative
    //   integer, so the smallest integer ≥ θ·max_len is `ceil(θ·max_len)`, and both sites compute
    //   the identical f64 `(θ·max_len).ceil()`); struct-hash edges have `overlap == max_len` ⇒
    //   similarity 1.0. So every edge in `adjacency` is genuinely a ≥ θ pair: the grow NEVER
    //   admits a member the old code would have rejected.
    //   REVERSE (similarity ≥ θ ⟹ edge) is SourcererCC sub-block admissibility, valid EXCEPT
    //   where #271's `HOT_TOKEN_POSTINGS_CAP` drops a θ-pair whose only shared sub-block tokens are
    //   all hot-capped (> cap postings): that pair is never generated as a candidate, so it is not
    //   an edge even though similarity ≥ θ. In that (recall-unsafe) case the OLD grow would admit
    //   the member and this edge-adjacency grow rejects it — i.e. the output diverges. But that
    //   scenario means #271 has ALREADY altered which pairs exist, so #258 does NOT introduce a new
    //   behavior class: it inherits exactly #271's accepted recall approximation, which is gated
    //   separately by `clones --recall-symbols`. On a recall-safe corpus (#271 drops no θ-pair) the
    //   two grows are byte-identical — the production invariant the issue assumed, and what the
    //   `coherence_split_edge_adjacency_equals_similarity_recompute_when_edges_are_theta_set` test
    //   pins on `edges == exactly the ≥ θ set`.
    let adjacency: std::collections::HashSet<(i64, i64)> = coherent_edges.iter().copied().collect();

    // 2b. Seed high-similarity edges FIRST so tight real cliques grow WITHIN the MAX_SPLIT_GROUPS
    // budget instead of falling into the bare-pair tail (#256 R-A). On the dogfood a true 7-member
    // `collect_rows` clique (all pairs sim ≥ 0.959) was fragmented to 2-member pairs because its
    // edges sorted late (by id) inside a 2,599-member sparse transitive component glued by generic
    // low-`df` tokens — the budget tripped on 256 noise cliques before collect_rows was reached.
    // Ordering by descending similarity grows the tight cliques first; the low-similarity
    // transitive "glue" edges sort last and become the bare pairs after the trip — correct,
    // since they are the over-merge noise, not real clones. Similarity is computed ONCE per
    // edge here (O(edges)), NOT in the comparator. The descending-sim + `(a, b)` tie-break
    // keeps the seed order deterministic.
    let mut seeds: Vec<(f64, i64, i64)> =
        coherent_edges.iter().map(|&(a, b)| (similarity(a, b), a, b)).collect();
    // `total_cmp` (not `partial_cmp(...).unwrap_or(Equal)`): a guaranteed TOTAL order so the sort
    // can never panic on a non-total comparator. similarity() cannot produce NaN today (both call
    // sites guard `max_len == 0 → 1.0`), but total_cmp removes that footgun for any future caller.
    seeds.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| (x.1, x.2).cmp(&(y.1, y.2))));

    // 3. Greedy maximal clique cover: for each coherent edge not already fully inside some emitted
    // group, grow a maximal coherent group from {a, b} by adding any remaining member (in id order)
    // that is coherent with every current group member. Emit the group.
    //
    // The "already inside some emitted group" test is the EXACT predicate the old per-group scan
    // computed — `(a, b)` is covered iff some single emitted group contains BOTH endpoints — but
    // implemented without the superlinear scan (#256). We keep a `member_groups: HashMap<i64,
    // Vec<usize>>` mapping each member to the indices of the emitted groups it belongs to; the
    // per-edge check is "do `a` and `b` share a group index?" — an intersection of two small lists,
    // NOT a `Vec::contains` over a giant group. The old `for g in &groups { g.contains(&a) &&
    // g.contains(&b) }` was O(groups × members) per edge; on a DENSE clique the first edge grows
    // one group of all n members and each of the ~n²/2 remaining edges then re-scanned that
    // giant group → O(n³) (a 2,575-member dense blob hung, #256). Why this is exact AND fast:
    // clique overlap is RARE (a member lands in two groups only when it coheres with two
    // mutually-incompatible peers — the chain case), so each member's group-index list is tiny
    // (length 1 in the dense case), and the intersection is O(1) there. It avoids the O(n²)
    // cost of recording every intra-group PAIR (the obvious covered-edge-set), which is ~0.5M
    // HashSet ops at n=1000 and blows the sub-second debug budget. `member_groups` is
    // membership bookkeeping only (never iterated for output), so determinism is unaffected —
    // the returned class order comes solely from the canonicalized `coherent_edges` + `groups`
    // order.
    //
    // Past `MAX_SPLIT_GROUPS` GROWN cliques the component is pathologically tangled; we stop
    // growing (the grow is the remaining superlinear pass) and emit every remaining uncovered edge
    // as a bare 2-member clique. This bounds work WITHOUT dropping any member: a long chain's tail
    // edges become pairs rather than being lost (#256).
    let mut groups: Vec<Vec<i64>> = Vec::new();
    let mut member_groups: std::collections::HashMap<i64, Vec<usize>> =
        std::collections::HashMap::new();
    // Members already in some emitted group (a grown clique or a bare pair). Drives the
    // covering-subset emission below (#282): the over-budget tail only needs to COVER each
    // still-uncovered member, not emit every edge.
    let mut covered: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut budget_tripped = false;

    for &(_sim, a, b) in &seeds {
        if budget_tripped {
            // Over budget: emit this θ-edge as a 2-member clique ONLY if it covers a
            // still-uncovered member (#282). Emitting EVERY remaining edge (the old
            // behavior) shattered a dense giant into O(edges) bare pairs — ~87k classes
            // on cargo's 3.1k-node giant, the dominant `find_clones` cost (building
            // them all). A covering subset emits ≤ O(uncovered members) pairs and KEEPS
            // the #256 invariant: every member with a θ-edge that isn't in a grown
            // clique gets a bare pair the first time one of its edges is reached here (it is
            // uncovered then), so no member is dropped and symbol-level recall is unchanged. An
            // edge whose endpoints are BOTH already covered is a redundant clone PAIR,
            // not a lost member, so skipping it cannot drop recall — it only removes a
            // duplicate 2-member class. Still internally coherent (it is a θ-edge); the
            // later dedup/subset pass stays skipped.
            if !covered.contains(&a) || !covered.contains(&b) {
                groups.push(vec![a, b]);
                covered.insert(a);
                covered.insert(b);
            }
            continue;
        }
        // Skip this edge if some existing group already contains BOTH endpoints — i.e. `a` and `b`
        // share a group index. This is the EXACT old predicate (a common group), via a small-list
        // intersection instead of a giant-group `Vec::contains`.
        if let (Some(ga), Some(gb)) = (member_groups.get(&a), member_groups.get(&b))
            && ga.iter().any(|gi| gb.contains(gi))
        {
            continue;
        }
        // Grow a maximal coherent group from {a, b}. `group_set` mirrors `group` for O(1)
        // membership (the old `group.contains(&m)` was O(group) per candidate → O(n²) on a
        // dense clique, #256).
        let mut group: Vec<i64> = vec![a, b];
        let mut group_set: std::collections::HashSet<i64> = group.iter().copied().collect();
        for &m in &members {
            if group_set.contains(&m) {
                continue;
            }
            // m is coherent with every current group member? Check EDGE ADJACENCY against the
            // pre-verified θ-edge set (#258) — an O(1) HashSet lookup per (m, g) — instead of
            // recomputing `similarity(m, g) >= theta` (an O(token_len) overlap recompute). The two
            // are output-identical: `(min, max)` is in `adjacency` iff the pair is a θ-edge iff
            // `similarity(m, g) >= theta` (see the `adjacency` construction above). This removes
            // the O(token_len) factor from the grow, making a dense (near-)clique
            // component O(n²) lookups instead of O(n² · token_len) overlap recomputes.
            if group.iter().all(|&g| adjacency.contains(&(m.min(g), m.max(g)))) {
                group.push(m);
                group_set.insert(m);
            }
        }
        group.sort_unstable(); // deterministic order within group
        // Record this group's index against each of its members so a later edge fully inside this
        // group is skipped via the small-list intersection above — the exact predicate the old
        // per-group scan computed. O(group), not O(group²).
        let gi = groups.len();
        for &m in &group {
            member_groups.entry(m).or_default().push(gi);
            // A grown-clique member is covered → the over-budget tail won't re-emit a bare pair
            // for it (#282 covering-subset).
            covered.insert(m);
        }
        groups.push(group);
        // Budget guard (#256): bound the superlinear passes. From here on, remaining edges are
        // emitted as bare pairs (above) — coverage is preserved, work is bounded.
        if groups.len() > MAX_SPLIT_GROUPS {
            budget_tripped = true;
        }
    }

    // 5. Keep only maximal groups (drop any that is a strict subset of another). SKIP this
    // O(groups² × n) pass when the budget tripped — on a pathologically tangled component it is the
    // expensive step, and the bare-pair tail is already minimal (2-member edges). Redundant subset
    // groups in the over-budget output are harmless to recall (they never drop a member).
    let mut maximal: Vec<Vec<i64>> = if budget_tripped {
        groups
    } else {
        let mut kept: Vec<Vec<i64>> = Vec::new();
        for g in &groups {
            let is_subset = groups
                .iter()
                .any(|other| other.len() > g.len() && g.iter().all(|x| other.contains(x)));
            if !is_subset {
                kept.push(g.clone());
            }
        }
        kept
    };

    // 6. De-duplicate identical groups (same set).
    maximal.sort_unstable();
    maximal.dedup();

    // 7. Drop singletons (shouldn't happen since we start from edges, but be safe).
    maximal.retain(|g| g.len() >= 2);

    // 8. Sort by lowest member id (determinism).
    maximal.sort_by_key(|g| g[0]);

    maximal
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One parity fixture for `coherence_split_edge_adjacency_equals_similarity_recompute_*`:
    /// `(shape_name, component members, explicit symmetric similarity pairs)`.
    type ParityCase = (&'static str, Vec<i64>, Vec<((i64, i64), f64)>);

    /// Verify that every pair within `class` has similarity ≥ theta via `sim`.
    fn all_pairs_meet_theta(class: &[i64], sim: impl Fn(i64, i64) -> f64, theta: f64) -> bool {
        for (i, &a) in class.iter().enumerate() {
            for &b in &class[i + 1..] {
                if sim(a, b) < theta {
                    return false;
                }
            }
        }
        true
    }

    /// Build a symmetric similarity closure from an explicit pair list.
    /// Any unlisted pair defaults to 0.0 (below any reasonable theta).
    fn sim_from_pairs(pairs: &[((i64, i64), f64)]) -> impl Fn(i64, i64) -> f64 + '_ {
        move |a, b| {
            for &((x, y), v) in pairs {
                if (a == x && b == y) || (a == y && b == x) {
                    return v;
                }
            }
            0.0
        }
    }

    /// Derive the θ-verified edge list (the seed `edges` the production call site buckets per
    /// component) from an explicit pair list: every pair whose similarity is ≥ theta is an edge,
    /// canonicalized to `(a.min(b), a.max(b))`. Mirrors `candidate_pairs_from_bags` restricted to
    /// the component.
    fn edges_from_pairs(pairs: &[((i64, i64), f64)], theta: f64) -> Vec<(i64, i64)> {
        pairs
            .iter()
            .filter(|&&(_, v)| v >= theta)
            .map(|&((a, b), _)| (a.min(b), a.max(b)))
            .collect()
    }

    /// Transitive chain A~B (0.74), B~C (0.86), A!~C (0.67) at theta=0.70.
    /// The greedy clique cover yields BOTH coherent pairs — {A,B} and {B,C} — with B in both (the
    /// overlap is correct: B coheres with two peers that are themselves incompatible). No returned
    /// class may contain all three, and every returned class must be internally coherent.
    #[test]
    fn coherence_split_breaks_a_transitive_chain_into_coherent_classes() {
        let (a, b, c) = (10_i64, 20_i64, 30_i64);
        let theta = 0.70;
        let pairs = [((a, b), 0.74), ((b, c), 0.86), ((a, c), 0.67)];
        let sim = sim_from_pairs(&pairs);
        let edges = edges_from_pairs(&pairs, theta);

        let split = coherence_split(&[a, b, c], &edges, &sim);

        // Chain A~B, B~C, A!~C → both {A,B} and {B,C} are returned (B in both — overlap is
        // correct).
        assert_eq!(split.len(), 2, "chain yields {{A,B}} and {{B,C}}: {split:?}");
        let has_ab = split.iter().any(|g| g.contains(&a) && g.contains(&b) && !g.contains(&c));
        let has_bc = split.iter().any(|g| g.contains(&b) && g.contains(&c) && !g.contains(&a));
        assert!(has_ab, "must contain {{A,B}}: {split:?}");
        assert!(has_bc, "must contain {{B,C}}: {split:?}");
        // Every returned class must be internally coherent (all pairs ≥ theta).
        for class in &split {
            assert!(
                all_pairs_meet_theta(class, &sim, theta),
                "class {:?} contains a pair below theta={}",
                class,
                theta
            );
        }
    }

    /// A fully coherent 4-member component (all pairs ≥ theta) must stay as one class.
    #[test]
    fn coherence_split_keeps_a_fully_coherent_component_as_one_class() {
        let (a, b, c, d) = (1_i64, 2_i64, 3_i64, 4_i64);
        let theta = 0.70;
        // All pairs are 0.80 — well above theta.
        let pairs = [
            ((a, b), 0.80),
            ((a, c), 0.80),
            ((a, d), 0.80),
            ((b, c), 0.80),
            ((b, d), 0.80),
            ((c, d), 0.80),
        ];
        let sim = sim_from_pairs(&pairs);
        let edges = edges_from_pairs(&pairs, theta);

        let split = coherence_split(&[a, b, c, d], &edges, &sim);

        assert_eq!(split.len(), 1, "expected one class, got {:?}", split);
        let mut returned = split[0].clone();
        returned.sort_unstable();
        assert_eq!(returned, vec![a, b, c, d]);
    }

    /// Two members below theta → both become singletons → no class returned.
    #[test]
    fn coherence_split_drops_singletons() {
        let (a, b) = (1_i64, 2_i64);
        let theta = 0.70;
        let pairs = [((a, b), 0.50)]; // below theta
        let sim = sim_from_pairs(&pairs);
        let edges = edges_from_pairs(&pairs, theta); // below θ → no edge

        let split = coherence_split(&[a, b], &edges, &sim);

        assert!(split.is_empty(), "expected no class (both singletons), got {:?}", split);
    }

    /// The result must be identical regardless of the order the component slice is supplied in.
    #[test]
    fn coherence_split_is_deterministic() {
        let (a, b, c) = (10_i64, 20_i64, 30_i64);
        let pairs = [((a, b), 0.74), ((b, c), 0.86), ((a, c), 0.67)];
        let sim = sim_from_pairs(&pairs);
        // Supply the edges in a scrambled order too — the split canonicalizes + sorts them.
        let edges = vec![(b, c), (a, b)];

        // Supply in three different orderings.
        let r1 = coherence_split(&[a, b, c], &edges, &sim);
        let r2 = coherence_split(&[c, b, a], &[(c, b), (b, a)], &sim);
        let r3 = coherence_split(&[b, a, c], &[(a, b), (c, b)], &sim);

        assert_eq!(r1, r2, "result differs between [a,b,c] and [c,b,a]");
        assert_eq!(r1, r3, "result differs between [a,b,c] and [b,a,c]");
    }

    /// #256 scalability pin: a DENSE clique of n = 1000 (every pair ≥ θ, ~500K edges) must split to
    /// ONE class with all n members AND complete fast. This is the case the O(n³) per-edge
    /// containment scan hung on — the first edge grows one group of all n members, then each of the
    /// ~n²/2 remaining edges re-scanned that giant group via `Vec::contains` (O(n) each) → O(n³):
    /// ~5.9s at n=1000 / ~44s at n=2000 in debug. `MAX_SPLIT_GROUPS` does NOT save it (a full
    /// clique = exactly ONE grown group, so the budget never trips). The #256 fix collapses
    /// every per-edge pass to O(1): the `member_groups` small-list-intersection containment
    /// check, a `HashSet` member filter, and an O(1)-membership grow loop — the whole dense
    /// clique is now O(edges).
    ///
    /// Timing: standalone this completes in ~0.45s — well under a second, vs ~5.9s pre-fix (a >10×
    /// speedup; the n=1000 / ~5.9s figure is the exact pre-fix data point this pin A/Bs against).
    /// The assertion is a HANG-DETECTOR, not a microbenchmark: the full suite runs this O(n²)
    /// test concurrently with ~1000 others (some multi-second) on 8 cores, so its WALL time
    /// inflates to a few seconds under that saturation — a scheduler artifact, not algorithmic.
    /// The 4s ceiling sits firmly between the post-fix contended time (~2-3s) and the pre-fix
    /// regression (5.9s standalone → tens of seconds under the same contention), so it catches
    /// an O(n³) reintroduction without flaking on scheduler noise.
    #[test]
    fn coherence_split_dense_clique_scales() {
        let count: i64 = 1500; // O(n³) regression ~40-60s+ under load; O(edges) cover ~10s.
        let members: Vec<i64> = (0..count).collect();
        // Every pair is coherent at 0.90 — a genuinely dense full clique above θ.
        let sim = |_a: i64, _b: i64| 0.90_f64;
        // The θ-verified edge list is the complete graph (every pair ≥ θ): ~500K edges, each of
        // which hit the old O(n) containment scan.
        let mut edges: Vec<(i64, i64)> = Vec::with_capacity(((count * (count - 1)) / 2) as usize);
        for i in 0..count {
            for j in (i + 1)..count {
                edges.push((i, j));
            }
        }

        let start = std::time::Instant::now();
        let split = coherence_split(&members, &edges, sim);
        let elapsed = start.elapsed();

        // ONE coherent class with all n members — zero member loss.
        assert_eq!(split.len(), 1, "a dense clique stays ONE class, got {} classes", split.len());
        assert_eq!(split[0].len(), count as usize, "all {count} members must survive");
        let union: std::collections::BTreeSet<i64> = split.iter().flatten().copied().collect();
        let expected: std::collections::BTreeSet<i64> = members.iter().copied().collect();
        assert_eq!(union, expected, "the union of class members must equal the input members");
        // COARSE hang-detector, NOT a microbenchmark — the durable guard is the correctness
        // assertion above. A wall-clock ceiling under nextest's per-core parallelism is inherently
        // noisy: the old 4s ceiling at n=1000 flaked at ~4.25s under full-suite CPU saturation. So
        // this uses generous headroom — at n=1500 the O(edges) cover is ~10s under load while an
        // O(n³) reintroduction is ~40-60s+; the 30s ceiling sits ~3x clear of the pass case (no
        // scheduler-noise flake) yet still reddens on a cubic regression.
        assert!(
            elapsed.as_secs() < 30,
            "dense clique of {count} must split in ~O(edges), not O(n³); took {elapsed:?}"
        );
    }

    /// #256 R-A regression: a true high-similarity clique buried in a giant transitive component
    /// (whose generic low-`df` "glue" trips `MAX_SPLIT_GROUPS`) must still be GROWN, not fragmented
    /// to bare pairs. Pre-fix the clique's edges sorted late (by id) into the post-budget bare-pair
    /// tail — on the dogfood the real 7-copy `collect_rows` clique came back as 2-member pairs.
    /// Seeding by descending similarity grows the tight clique first.
    #[test]
    fn coherence_split_grows_tight_clique_before_budget_trips() {
        // A 7-member tight clique (high ids, sim 1.0) connected to a 300-leaf star hub (sim 0.70);
        // the star's 301 disjoint 2-cliques trip MAX_SPLIT_GROUPS (256) before any id-ordered pass
        // would reach the high-id clique edges.
        let clique: Vec<i64> = (1000..=1006).collect();
        let clique_set: std::collections::HashSet<i64> = clique.iter().copied().collect();
        let mut members: Vec<i64> = vec![0];
        members.extend(1..=300);
        members.extend(clique.iter().copied());

        let sim = |a: i64, b: i64| -> f64 {
            if clique_set.contains(&a) && clique_set.contains(&b) {
                1.0 // tight clique: every pair maximally coherent
            } else if (a == 0 && ((1..=300).contains(&b) || b == 1000))
                || (b == 0 && ((1..=300).contains(&a) || a == 1000))
            {
                0.70 // hub↔leaf (and the hub↔clique connector that joins the component)
            } else {
                0.0 // leaves don't cohere with each other or the clique → small cliques
            }
        };

        let mut edges: Vec<(i64, i64)> = Vec::new();
        for i in 1..=300 {
            edges.push((0, i)); // star: 300 disjoint 2-cliques
        }
        edges.push((0, 1000)); // connector: clique shares the star's component
        for i in 0..clique.len() {
            for j in (i + 1)..clique.len() {
                edges.push((clique[i], clique[j])); // 21 tight-clique pairs, high ids
            }
        }

        let split = coherence_split(&members, &edges, sim);

        // The tight 7-clique is GROWN as ONE group (descending-sim seed), not 21 bare pairs.
        assert!(
            split.iter().any(|g| {
                let s: std::collections::HashSet<i64> = g.iter().copied().collect();
                s == clique_set
            }),
            "the tight high-similarity clique must be grown as ONE group, not fragmented: \
             {split:?}"
        );
        // Zero member loss across the whole (budget-tripping) component.
        let union: std::collections::BTreeSet<i64> = split.iter().flatten().copied().collect();
        let expected: std::collections::BTreeSet<i64> = members.iter().copied().collect();
        assert_eq!(union, expected, "no member may be dropped");
    }

    /// #256 pin (a): a 250-member LOOSE clique — every pair coherent at exactly 0.80 (> θ), well
    /// past the old `SPLIT_MAX = 200` cap that would have returned it whole anyway but for the
    /// wrong reason (the cap, not coherence). With the cap removed and the full edge list fed
    /// in, the clique cover keeps it as ONE coherent class with **zero member loss** — a
    /// genuinely dense large clone class must NOT be fragmented.
    #[test]
    fn coherence_split_keeps_loose_clique() {
        let count: i64 = 250; // > the old SPLIT_MAX (200)
        let members: Vec<i64> = (0..count).collect();
        // Every pair is coherent at 0.80 — a full clique above θ.
        let sim = |_a: i64, _b: i64| 0.80_f64;
        // The θ-verified edge list is the complete graph (every pair ≥ θ).
        let mut edges: Vec<(i64, i64)> = Vec::new();
        for i in 0..count {
            for j in (i + 1)..count {
                edges.push((i, j));
            }
        }

        let split = coherence_split(&members, &edges, sim);

        // One coherent class with all 250 members — none dropped.
        assert_eq!(split.len(), 1, "a full clique stays ONE class, got {} classes", split.len());
        assert_eq!(split[0].len(), count as usize, "all {count} members must survive");
        let union: std::collections::BTreeSet<i64> = split.iter().flatten().copied().collect();
        let expected: std::collections::BTreeSet<i64> = members.iter().copied().collect();
        assert_eq!(union, expected, "the union of class members must equal the input members");
    }

    /// #256 pin (b): a 300-member transitive CHAIN (`m0~m1~m2~…`, each adjacent pair ≥ θ, non-
    /// adjacent pairs below θ) — the exact over-merge shape that produced the 2,575-member giant.
    /// It must break into many small coherent cliques (the adjacent pairs / runs), NOT one giant
    /// class, with NO member dropped (every member is an endpoint of at least one edge).
    #[test]
    fn coherence_split_breaks_chain() {
        let theta = 0.70;
        let count: i64 = 300; // > the old SPLIT_MAX (200)
        let members: Vec<i64> = (0..count).collect();
        // Adjacent members cohere (0.85); everything else is below θ (a pure chain).
        let sim = |a: i64, b: i64| -> f64 { if (a - b).abs() == 1 { 0.85 } else { 0.0 } };
        // θ-verified edges = the chain edges only.
        let edges: Vec<(i64, i64)> = (0..count - 1).map(|i| (i, i + 1)).collect();

        let split = coherence_split(&members, &edges, sim);

        // Must NOT be one giant class — the chain breaks into its adjacent cliques.
        assert!(
            split.len() > 1,
            "a 300-member chain must break into many cliques, got {} classes",
            split.len()
        );
        // No member dropped: every member is an endpoint of a chain edge, so it lands in a clique.
        let union: std::collections::BTreeSet<i64> = split.iter().flatten().copied().collect();
        let expected: std::collections::BTreeSet<i64> = members.iter().copied().collect();
        assert_eq!(union, expected, "no member may be dropped from a chain split");
        // Every returned class is internally coherent (all pairs ≥ θ): a chain edge {i, i+1} is a
        // maximal clique because no third member coheres with both endpoints.
        for class in &split {
            assert!(
                all_pairs_meet_theta(class, sim, theta),
                "class {:?} contains a pair below theta={}",
                class,
                theta
            );
            assert!(class.len() <= 2, "chain cliques are adjacent pairs: {class:?}");
        }
    }

    /// #256: a dense-minus-perfect-matching component at n≈200 is the pathological case for the
    /// greedy clique-cover (every non-matched pair is an edge → O(n²) maximal cliques). The
    /// `MAX_SPLIT_GROUPS` budget trips early and the cover STOPS SEEDING new cliques, keeping the
    /// cliques collected so far (#256 changed this from "return the whole component" — the old
    /// behavior that resurrected the giant — to "emit groups-so-far"). The result completes fast
    /// and drops NO member: in such a dense graph every member is covered by an early seed clique.
    #[test]
    fn coherence_split_pathological_dense_minus_matching_returns_fast() {
        // n=200 dense-minus-perfect-matching: all pairs above theta EXCEPT the perfect matching
        // (0,1), (2,3), (4,5), … This is the worst case for maximal-clique enumeration because
        // every non-matched pair is an edge, yielding O(n²) maximal cliques.
        let n: i64 = 200;
        let members: Vec<i64> = (0..n).collect();
        // Perfect matching: pair (2k, 2k+1) is BELOW theta; all other pairs are ABOVE theta.
        let sim = |a: i64, b: i64| -> f64 {
            // If a and b are a matched pair (same "bucket"), below theta.
            let matched = (a / 2 == b / 2) && (a % 2 != b % 2);
            if matched { 0.5 } else { 0.9 }
        };
        let theta = 0.70;
        // θ-verified edges = every NON-matched pair (the dense complement of the matching).
        let mut edges: Vec<(i64, i64)> = Vec::new();
        for a in 0..n {
            for b in (a + 1)..n {
                if sim(a, b) >= theta {
                    edges.push((a, b));
                }
            }
        }

        let start = std::time::Instant::now();
        let result = coherence_split(&members, &edges, sim);
        let elapsed = start.elapsed();

        // COARSE hang-detector, NOT a microbenchmark — the durable guards are the correctness
        // asserts below (no member dropped, every class coherent, O(members) output). A wall-clock
        // ceiling under nextest's per-core parallelism is inherently noisy: in isolation this case
        // runs ~1.0s, but under full-suite CPU saturation it was observed at ~2.03s against the old
        // `< 2` ceiling (a scheduler-noise flake, not a regression — the #258 edge-adjacency grow
        // is strictly FASTER than the old per-(m, g) similarity recompute). The generous `<
        // 10` ceiling mirrors the `coherence_split_dense_clique_scales` rationale: it sits
        // well clear of the pass case so it never flakes on contention, yet still reddens
        // on an O(n³) reintroduction (which would be tens of seconds on this n=200
        // dense-minus-matching worst case).
        assert!(
            elapsed.as_secs() < 10,
            "coherence_split must not hang on n=200 pathological input: took {elapsed:?}"
        );

        // Budget tripped (or resolved) → no member dropped. In a dense graph every member appears
        // in an early seed clique, so the groups-so-far cover still covers every member.
        let all_members: std::collections::BTreeSet<i64> = members.iter().copied().collect();
        let returned_members: std::collections::BTreeSet<i64> =
            result.iter().flatten().copied().collect();
        assert!(
            returned_members.is_superset(&all_members),
            "no member may be dropped: missing {:?}",
            all_members.difference(&returned_members).collect::<Vec<_>>()
        );
        // The budget bounds WORK (the superlinear subset-removal is skipped once tripped), not the
        // output count: a pathologically tangled component emits its tail edges as bare 2-member
        // cliques rather than dropping members. So the result may have many classes — what matters
        // is that it completed fast (asserted above) and every class is internally coherent.
        for class in &result {
            assert!(
                all_pairs_meet_theta(class, sim, theta),
                "class {class:?} contains a pair below theta={theta}"
            );
        }
        // #282 covering subset: the dense giant's grown cliques already cover every member, so the
        // over-budget tail emits ~no redundant pairs — the output is O(grown cliques), NOT
        // O(edges). The OLD "emit every remaining edge" behavior produced ~n²/2 ≈ 20k bare
        // 2-member classes here (build_class-ing all of them was the dominant cold
        // find_clones cost, #282).
        assert!(
            result.len() <= 2 * n as usize,
            "#282: covering-subset tail must bound output to O(members), not O(edges) — got {} \
             classes for {} edges",
            result.len(),
            edges.len(),
        );
    }

    #[test]
    fn coherence_split_covering_subset_keeps_all_members_when_budget_trips() {
        // A graph that TRIPS the budget AND leaves members for the tail to cover: many disjoint
        // tight triangles (each grows one clique) plus a long sparse chain whose members are NOT in
        // any triangle. With > MAX_SPLIT_GROUPS triangles the budget trips, and the chain members
        // must still be covered by the tail — by a COVERING SUBSET, not every chain edge.
        let theta = 0.70;
        let mut edges: Vec<(i64, i64)> = Vec::new();
        let mut hi: Vec<((i64, i64), f64)> = Vec::new();
        // 300 disjoint triangles (ids 0..900): trips MAX_SPLIT_GROUPS=256.
        for t in 0..300i64 {
            let (a, b, c) = (t * 3, t * 3 + 1, t * 3 + 2);
            for &(x, y) in &[(a, b), (b, c), (a, c)] {
                edges.push((x, y));
                hi.push(((x, y), 0.9));
            }
        }
        // A sparse chain 1000-1001-1002-…-1009 (each adjacent pair a θ-edge; non-adjacent below θ).
        for k in 0..9i64 {
            edges.push((1000 + k, 1000 + k + 1));
            hi.push(((1000 + k, 1000 + k + 1), 0.8));
        }
        let members: Vec<i64> = (0..900).chain(1000..1010).collect();
        let sim = sim_from_pairs(&hi);

        let result = coherence_split(&members, &edges, &sim);

        // #256 invariant: NO member with a θ-edge is dropped (every triangle member + every chain
        // member appears in some class).
        let returned: std::collections::BTreeSet<i64> = result.iter().flatten().copied().collect();
        for &m in &members {
            assert!(returned.contains(&m), "#256: member {m} was dropped after the budget tripped");
        }
        // #282: the chain's 10 members are covered by ≤ their count of tail pairs (a covering
        // subset), and every emitted class is internally coherent.
        for class in &result {
            assert!(all_pairs_meet_theta(class, &sim, theta), "incoherent class {class:?}");
        }
    }

    /// #258 pin: the GROW step decides coherence by EDGE ADJACENCY against the passed-in `edges`, not
    /// by recomputing `similarity(m, g) >= theta`. In production the two are equivalent (`edges` IS
    /// the set of `similarity >= theta` pairs — `candidate_pairs_from_bags` restricted to the
    /// component), so the output is byte-identical to the pre-#258 similarity-recompute version.
    /// This test asserts the grow follows `edges`: a fully-coherent 4-clique whose `edges` set
    /// is missing ONE pair (a,d) must NOT grow into a single 4-member group — d cannot join
    /// {a,b,c} because (a,d) is not an edge — so the output reflects edge adjacency, exactly as
    /// the candidate verify would have produced. (If the grow still recomputed `similarity`, it
    /// would IGNORE the missing edge and wrongly emit one 4-clique.) The seed `similarity`
    /// closure still returns the true values; only the EDGE SET drives grow membership.
    #[test]
    fn coherence_split_grows_by_edge_adjacency_not_recomputed_similarity() {
        let (a, b, c, d) = (1_i64, 2_i64, 3_i64, 4_i64);
        // All six pairs are ABOVE theta by similarity …
        let pairs = [
            ((a, b), 0.90),
            ((a, c), 0.90),
            ((a, d), 0.90),
            ((b, c), 0.90),
            ((b, d), 0.90),
            ((c, d), 0.90),
        ];
        let sim = sim_from_pairs(&pairs);
        // … but the θ-verified EDGE SET omits (a, d) (e.g. the candidate verify rejected it). The
        // grow must honor the edge set, so d cannot join a group containing a.
        let edges: Vec<(i64, i64)> = vec![(a, b), (a, c), (b, c), (b, d), (c, d)];

        let split = coherence_split(&[a, b, c, d], &edges, &sim);

        // No returned class may contain BOTH a and d — there is no (a, d) edge, so they are not
        // mutually coherent under the edge-adjacency grow.
        for class in &split {
            assert!(
                !(class.contains(&a) && class.contains(&d)),
                "a and d share no edge; the edge-adjacency grow must not co-class them: {split:?}"
            );
        }
        // {b, c, d} IS a clique in the edge set → grown as one group; {a, b, c} likewise.
        let has_bcd = split.iter().any(|g| {
            let s: std::collections::HashSet<i64> = g.iter().copied().collect();
            s == std::collections::HashSet::from([b, c, d])
        });
        let has_abc = split.iter().any(|g| {
            let s: std::collections::HashSet<i64> = g.iter().copied().collect();
            s == std::collections::HashSet::from([a, b, c])
        });
        assert!(has_bcd, "{{b,c,d}} is an edge-clique and must be one grown group: {split:?}");
        assert!(has_abc, "{{a,b,c}} is an edge-clique and must be one grown group: {split:?}");
    }

    /// Reference reimplementation of `coherence_split` that reproduces the PRE-#258 grow exactly:
    /// the per-(m, g) coherence check recomputes `similarity(m, g) >= theta` instead of the O(1)
    /// edge-adjacency lookup. Every OTHER step (member sort, canonicalize+dedup edges,
    /// descending-sim seed order with `(a, b)` tie-break, the `member_groups` already-covered
    /// skip, the budget trip + covering-subset tail, subset removal, dedup, singleton drop,
    /// final sort) is byte-for-byte the shipped algorithm. The ONLY divergence point is the
    /// grow predicate, so when this and the shipped `coherence_split` are run on the SAME
    /// fixture and `edges == exactly {(a,b): sim>=θ}`, any output difference is attributable
    /// solely to the #258 substitution — which is what the parity test below asserts CANNOT
    /// happen. (Kept in lockstep with the production body above.)
    fn reference_split_by_similarity_recompute(
        component: &[i64],
        edges: &[(i64, i64)],
        similarity: impl Fn(i64, i64) -> f64,
        theta: f64,
    ) -> Vec<Vec<i64>> {
        let mut members: Vec<i64> = component.to_vec();
        members.sort_unstable();
        let member_set: std::collections::HashSet<i64> = members.iter().copied().collect();

        let mut coherent_edges: Vec<(i64, i64)> = edges
            .iter()
            .filter_map(|&(a, b)| {
                if a == b || !member_set.contains(&a) || !member_set.contains(&b) {
                    return None;
                }
                Some((a.min(b), a.max(b)))
            })
            .collect();
        coherent_edges.sort_unstable();
        coherent_edges.dedup();

        let mut seeds: Vec<(f64, i64, i64)> =
            coherent_edges.iter().map(|&(a, b)| (similarity(a, b), a, b)).collect();
        seeds.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| (x.1, x.2).cmp(&(y.1, y.2))));

        let mut groups: Vec<Vec<i64>> = Vec::new();
        let mut member_groups: std::collections::HashMap<i64, Vec<usize>> =
            std::collections::HashMap::new();
        let mut covered: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut budget_tripped = false;

        for &(_sim, a, b) in &seeds {
            if budget_tripped {
                if !covered.contains(&a) || !covered.contains(&b) {
                    groups.push(vec![a, b]);
                    covered.insert(a);
                    covered.insert(b);
                }
                continue;
            }
            if let (Some(ga), Some(gb)) = (member_groups.get(&a), member_groups.get(&b))
                && ga.iter().any(|gi| gb.contains(gi))
            {
                continue;
            }
            let mut group: Vec<i64> = vec![a, b];
            let mut group_set: std::collections::HashSet<i64> = group.iter().copied().collect();
            for &m in &members {
                if group_set.contains(&m) {
                    continue;
                }
                // The PRE-#258 grow predicate: recompute similarity per (m, g).
                if group.iter().all(|&g| similarity(m, g) >= theta) {
                    group.push(m);
                    group_set.insert(m);
                }
            }
            group.sort_unstable();
            let gi = groups.len();
            for &m in &group {
                member_groups.entry(m).or_default().push(gi);
                covered.insert(m);
            }
            groups.push(group);
            if groups.len() > MAX_SPLIT_GROUPS {
                budget_tripped = true;
            }
        }

        let mut maximal: Vec<Vec<i64>> = if budget_tripped {
            groups
        } else {
            let mut kept: Vec<Vec<i64>> = Vec::new();
            for g in &groups {
                let is_subset = groups
                    .iter()
                    .any(|other| other.len() > g.len() && g.iter().all(|x| other.contains(x)));
                if !is_subset {
                    kept.push(g.clone());
                }
            }
            kept
        };
        maximal.sort_unstable();
        maximal.dedup();
        maximal.retain(|g| g.len() >= 2);
        maximal.sort_by_key(|g| g[0]);
        maximal
    }

    /// #258 PARITY PIN (the byte-identical bar the perf change must clear): when `edges` is EXACTLY
    /// the ≥ θ pair set (the production invariant on a recall-safe corpus), the shipped
    /// edge-adjacency grow must produce output IDENTICAL to the old per-(m, g) `similarity >=
    /// theta` recompute. The salvage's `..._not_recomputed_similarity` test only pins the
    /// DIVERGENT direction (edges omit a pair sim calls ≥ θ → honor edges); this pins the
    /// production direction. Several component shapes (chain, dense clique, loose clique,
    /// dense-minus-matching) are each built with `edges = edges_from_pairs(pairs, θ) = {(a,b):
    /// sim(a,b) >= θ}` and asserted to give `coherence_split(...) ==
    /// reference_split_by_similarity_recompute(...)`.
    #[test]
    fn coherence_split_edge_adjacency_equals_similarity_recompute_when_edges_are_theta_set() {
        let theta = 0.70;

        // Build (component, pairs) for each shape, with the FULL symmetric pair list so
        // `edges_from_pairs` yields exactly the ≥ θ edge set.
        let mut cases: Vec<ParityCase> = Vec::new();

        // (1) Chain 0-1-2-3-4: adjacent pairs ≥ θ, non-adjacent below θ (default 0.0 via
        // sim_from_pairs).
        {
            let members: Vec<i64> = (0..5).collect();
            let pairs: Vec<((i64, i64), f64)> =
                (0..4).map(|i| ((i, i + 1), 0.80 + 0.01 * i as f64)).collect();
            cases.push(("chain", members, pairs));
        }

        // (2) Dense clique of 8: every pair ≥ θ.
        {
            let members: Vec<i64> = (0..8).collect();
            let mut pairs: Vec<((i64, i64), f64)> = Vec::new();
            for a in 0..8 {
                for b in (a + 1)..8 {
                    pairs.push(((a, b), 0.95));
                }
            }
            cases.push(("dense_clique", members, pairs));
        }

        // (3) Loose clique of 12: every pair ≥ θ but only just (0.71), distinct values to exercise
        // the descending-sim seed order with ties broken by id.
        {
            let members: Vec<i64> = (0..12).collect();
            let mut pairs: Vec<((i64, i64), f64)> = Vec::new();
            for a in 0..12 {
                for b in (a + 1)..12 {
                    // Spread similarities across [0.71, 0.79] deterministically so the seed sort
                    // is exercised (every value is still ≥ θ → every pair an edge).
                    let v = 0.71 + (((a * 7 + b) % 9) as f64) * 0.01;
                    pairs.push(((a, b), v));
                }
            }
            cases.push(("loose_clique", members, pairs));
        }

        // (4) Dense-minus-perfect-matching at small n=10: all pairs ≥ θ EXCEPT the matching
        // (2k, 2k+1), which is below θ (and so NOT an edge). The worst-case maximal-clique shape.
        {
            let n: i64 = 10;
            let members: Vec<i64> = (0..n).collect();
            let mut pairs: Vec<((i64, i64), f64)> = Vec::new();
            for a in 0..n {
                for b in (a + 1)..n {
                    let matched = (a / 2 == b / 2) && (a % 2 != b % 2);
                    pairs.push(((a, b), if matched { 0.50 } else { 0.90 }));
                }
            }
            cases.push(("dense_minus_matching", members, pairs));
        }

        for (name, members, pairs) in &cases {
            let sim = sim_from_pairs(pairs);
            // edges == exactly {(a,b): sim(a,b) >= θ} — the production invariant.
            let edges = edges_from_pairs(pairs, theta);

            let shipped = coherence_split(members, &edges, &sim);
            let reference = reference_split_by_similarity_recompute(members, &edges, &sim, theta);

            assert_eq!(
                shipped, reference,
                "#258 parity: shape `{name}` — edge-adjacency grow must equal the old \
                 similarity-recompute grow when edges are exactly the ≥ θ set.\n  shipped:   \
                 {shipped:?}\n  reference: {reference:?}"
            );
            // Sanity: the equivalence is non-trivial — at least one real (≥ 2-member) class.
            assert!(
                !shipped.is_empty(),
                "shape `{name}` should produce at least one coherent class"
            );
        }
    }
}
