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

/// Hard cap on the all-pairs O(n²) work inside a single component. A component LARGER than this is
/// returned WHOLE as a single class (no member loss — see Fix 3, #215 Plan 4a): huge components are
/// near-universally fully-coherent exact-clone classes, and precise clique-cover splitting of
/// over-cap components is a follow-up. The caller already sets `metrics_sampled = true` for
/// components larger than `METRIC_SAMPLE_CAP` (200); this guard matches that cap so the split never
/// exceeds the same pairwise bound. For the overwhelmingly common case (all existing tests) this
/// cap is never reached.
const SPLIT_MAX: usize = 200;

/// Split an over-merged union-find component into internally-coherent clone classes: every pair
/// within a returned class has pairwise similarity >= theta.  Union-find over-merges transitive
/// chains (A~B, B~C, A!~C ⇒ {A,B,C}); this returns coherent sub-classes instead, via a greedy
/// maximal clique cover that keeps EVERY coherent group (so a member shared by two incompatible
/// peers, like B in chain A~B / B~C / A!~C, appears in both {A,B} and {B,C}).
///
/// `similarity(a, b)` returns the pairwise overlap/max similarity in \[0,1\] (the caller supplies
/// it from the token bags). Deterministic: members are processed in ascending-id order, group
/// members are sorted ascending, and returned classes are in stable order (by their lowest member
/// id). Classes of size < 2 are dropped (a clone class needs ≥ 2 members).
///
/// # Algorithm (greedy maximal clique cover)
///
/// 1. Sort members ascending by id (determinism).
/// 2. If the component exceeds `SPLIT_MAX`, return it WHOLE as one class — no members dropped (Fix
///    3, #215). `build_class`'s `METRIC_SAMPLE_CAP` bounds the pairwise metric cost downstream.
/// 3. Collect all coherent edges `(a, b)` with `a < b` and `similarity(a, b) >= theta`.
/// 4. For each coherent edge not already fully contained in an emitted group, grow a maximal
///    coherent group from `{a, b}` by adding any remaining member (in id order) that is ≥ θ to
///    every current group member.
/// 5. Drop groups that are a strict subset of another (keep only maximal cliques), de-duplicate
///    identical groups, drop singletons, and sort by lowest member id.
///
/// # Postcondition guaranteed by construction
///
/// Every returned class is internally coherent (all pairs ≥ theta): a member is only added to a
/// group after it has passed the coherence check against every existing member, so the all-pairs
/// property holds across all insertions.
pub(crate) fn coherence_split(
    component: &[i64],
    similarity: impl Fn(i64, i64) -> f64,
    theta: f64,
) -> Vec<Vec<i64>> {
    // 1. Sort ascending (determinism).
    let mut members: Vec<i64> = component.to_vec();
    members.sort_unstable();

    // 2. Huge component: return the whole thing as one class (no member loss — Fix 3, #215).
    // build_class's METRIC_SAMPLE_CAP bounds the pairwise cost. Huge components are typically
    // fully-coherent exact-clone classes; precise splitting of >SPLIT_MAX-member components is a
    // follow-up.
    if members.len() > SPLIT_MAX {
        return vec![members];
    }

    // 3. Collect all coherent edges (pairs with similarity >= theta), in (a, b) order a < b.
    let n = members.len();
    let mut coherent_edges: Vec<(i64, i64)> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            if similarity(members[i], members[j]) >= theta {
                coherent_edges.push((members[i], members[j]));
            }
        }
    }

    // 4. Greedy maximal clique cover: for each coherent edge not already fully inside some emitted
    // group, grow a maximal coherent group from {a, b} by adding any remaining member (in id order)
    // that is coherent with every current group member. Emit the group.
    let mut groups: Vec<Vec<i64>> = Vec::new();

    'edges: for (a, b) in &coherent_edges {
        // Skip this edge if some existing group already contains both endpoints.
        for g in &groups {
            if g.contains(a) && g.contains(b) {
                continue 'edges;
            }
        }
        // Grow a maximal coherent group from {a, b}.
        let mut group: Vec<i64> = vec![*a, *b];
        for &m in &members {
            if group.contains(&m) {
                continue;
            }
            // m is coherent with every current group member?
            if group.iter().all(|&g| similarity(m, g) >= theta) {
                group.push(m);
            }
        }
        group.sort_unstable(); // deterministic order within group
        groups.push(group);
    }

    // 5. Keep only maximal groups (drop any that is a strict subset of another).
    let mut maximal: Vec<Vec<i64>> = Vec::new();
    for g in &groups {
        let is_subset =
            groups.iter().any(|other| other.len() > g.len() && g.iter().all(|x| other.contains(x)));
        if !is_subset {
            maximal.push(g.clone());
        }
    }

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

        let split = coherence_split(&[a, b, c], &sim, theta);

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

        let split = coherence_split(&[a, b, c, d], &sim, theta);

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

        let split = coherence_split(&[a, b], &sim, theta);

        assert!(split.is_empty(), "expected no class (both singletons), got {:?}", split);
    }

    /// The result must be identical regardless of the order the component slice is supplied in.
    #[test]
    fn coherence_split_is_deterministic() {
        let (a, b, c) = (10_i64, 20_i64, 30_i64);
        let theta = 0.70;
        let pairs = [((a, b), 0.74), ((b, c), 0.86), ((a, c), 0.67)];
        let sim = sim_from_pairs(&pairs);

        // Supply in three different orderings.
        let r1 = coherence_split(&[a, b, c], &sim, theta);
        let r2 = coherence_split(&[c, b, a], &sim, theta);
        let r3 = coherence_split(&[b, a, c], &sim, theta);

        assert_eq!(r1, r2, "result differs between [a,b,c] and [c,b,a]");
        assert_eq!(r1, r3, "result differs between [a,b,c] and [b,a,c]");
    }

    /// Fix 3 (#215): a component LARGER than `SPLIT_MAX` is returned WHOLE as one class — NO
    /// members dropped. Even though here every pair is fully coherent, the point is the size
    /// guard returns the entire member set rather than truncating to `SPLIT_MAX` (the prior
    /// `members.truncate` silently lost the tail).
    #[test]
    fn coherence_split_huge_component_returns_all_members() {
        let count = SPLIT_MAX + 1;
        let members: Vec<i64> = (0..count as i64).collect();
        // All pairs above theta (fully coherent).
        let sim = |_a: i64, _b: i64| 1.0_f64;
        let split = coherence_split(&members, sim, 0.70);
        // Must return exactly one group with ALL members (not truncated).
        assert_eq!(split.len(), 1, "huge component returns one group: {} groups", split.len());
        assert_eq!(split[0].len(), count, "all {count} members must be present, not truncated");
    }

    /// Fix 3 (#215), companion: a >`SPLIT_MAX` component is returned as ONE class regardless of
    /// internal coherence — the size guard fires BEFORE any pairwise math, so even a component with
    /// no coherent pairs at all keeps every member (precise splitting of huge components is a
    /// follow-up; member loss is not acceptable in the interim).
    #[test]
    fn coherence_split_huge_component_returned_as_one_class() {
        let count = SPLIT_MAX + 1;
        let members: Vec<i64> = (0..count as i64).collect();
        // No coherent pairs (similarity 0.0 everywhere) — yet the size guard still returns one
        // whole class because it fires before the edge collection.
        let sim = |_a: i64, _b: i64| 0.0_f64;
        let split = coherence_split(&members, sim, 0.70);
        assert_eq!(split.len(), 1, "a >SPLIT_MAX component is one class regardless of coherence");
        assert_eq!(split[0].len(), count, "no members are dropped past SPLIT_MAX");
    }
}
