//! Coherence split: break an over-merged union-find component into internally-coherent clone
//! classes.
//!
//! # Why greedy-coherent, not connected-components of the θ-graph
//!
//! Union-find over-merges transitive chains (A~B edge, B~C edge, A!~C → {A,B,C} component).
//! Connected components of the θ-graph suffer the same flaw: a chain A–B–C is one connected
//! component even when A and C are below θ, so re-running connected components on the θ-graph
//! just reproduces the problem. We need **internally-coherent** classes — every pair within a
//! returned class must be ≥ θ. The greedy clustering below achieves this: each new member joins
//! only a class where it is coherent with EVERY existing member, guaranteeing the all-pairs
//! postcondition by construction.

/// Hard cap on the all-pairs O(n²) work inside a single component. When a component exceeds this
/// count, only the first `SPLIT_MAX` members (by ascending id) are processed. The caller already
/// sets `metrics_sampled = true` for components larger than `METRIC_SAMPLE_CAP` (200); this guard
/// matches that cap so the split never exceeds the same bound. For the overwhelmingly common case
/// (all existing tests) this cap is never reached.
const SPLIT_MAX: usize = 200;

/// Split an over-merged union-find component into internally-coherent clone classes: every pair
/// within a returned class has pairwise similarity >= theta.  Union-find over-merges transitive
/// chains (A~B, B~C, A!~C ⇒ {A,B,C}); this returns coherent sub-classes instead.
///
/// `similarity(a, b)` returns the pairwise overlap/max similarity in \[0,1\] (the caller supplies
/// it from the token bags). Deterministic: members are processed in ascending-id order and returned
/// classes are in stable order (by their lowest member id). Classes of size < 2 are dropped (a
/// clone class needs ≥ 2 members).
///
/// # Algorithm
///
/// 1. Sort members ascending by id (determinism).
/// 2. Cap at `SPLIT_MAX` (coarse guard for huge components — see doc above).
/// 3. For each member in id order, scan the existing classes and find the FIRST class (lowest
///    anchor id) where the candidate is coherent with EVERY current member (similarity ≥ theta). If
///    found, append it there. Otherwise start a new singleton class.
///
/// This greedy-first-fit strategy means a member joins the earliest class it fully coheres with,
/// which is tie-break deterministic by class-creation order (which is itself id-order-driven).
///
/// # Postcondition guaranteed by construction
///
/// Every returned class is internally coherent (all pairs ≥ theta): a member is only appended to
/// a class after it has passed the coherence check against every existing member in that class, so
/// the check is transitively maintained across all insertions.
pub(crate) fn coherence_split(
    component: &[i64],
    similarity: impl Fn(i64, i64) -> f64,
    theta: f64,
) -> Vec<Vec<i64>> {
    // 1. Sorted ascending (determinism).
    let mut members: Vec<i64> = component.to_vec();
    members.sort_unstable();

    // 2. Cap (coarse guard for huge components).
    members.truncate(SPLIT_MAX);

    // 3. Greedy first-fit coherence clustering.
    // `classes` grows as we discover members that don't fit any existing class.
    // Each class is internally coherent by construction: we only append when the candidate is
    // ≥ theta to every current member.
    let mut classes: Vec<Vec<i64>> = Vec::new();

    'member: for &m in &members {
        for class in &mut classes {
            // Check coherence of `m` with every existing member of this class.
            let fully_coherent = class.iter().all(|&existing| similarity(m, existing) >= theta);
            if fully_coherent {
                class.push(m);
                continue 'member;
            }
        }
        // No existing class accepted `m` — start a new singleton.
        classes.push(vec![m]);
    }

    // Drop singletons (a clone class needs ≥ 2 members).
    // Classes are already in creation order (anchored by the lowest member id that started each
    // class), which is a stable order derived from the id-sorted member stream.
    classes.retain(|c| c.len() >= 2);

    classes
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
    /// The component {A, B, C} must split — no returned class may contain all three.
    /// Every returned class must be internally coherent (all pairs ≥ theta).
    #[test]
    fn coherence_split_breaks_a_transitive_chain_into_coherent_classes() {
        let (a, b, c) = (10_i64, 20_i64, 30_i64);
        let theta = 0.70;
        let pairs = [((a, b), 0.74), ((b, c), 0.86), ((a, c), 0.67)];
        let sim = sim_from_pairs(&pairs);

        let split = coherence_split(&[a, b, c], &sim, theta);

        // Every returned class must be internally coherent.
        for class in &split {
            assert!(
                all_pairs_meet_theta(class, &sim, theta),
                "class {:?} contains a pair below theta={}",
                class,
                theta
            );
        }
        // The chain must NOT be returned as one 3-member class.
        assert!(
            split.iter().all(|c| c.len() < 3),
            "expected the chain to be split but got {:?}",
            split
        );
        // There must be at least one returned class (e.g. {A,B} or {B,C}).
        assert!(!split.is_empty(), "expected at least one coherent class, got none");
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
}
