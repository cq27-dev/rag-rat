//! Choosing which verified snapshot to use (C6b, #609).
//!
//! # Ordering by coverage, not by an asserted counter
//!
//! An earlier design gave the manifest a monotonic "view epoch" so snapshots could be totally
//! ordered. It was dropped: the epoch's true value is derivable only from the fold it claims to
//! order, so it is a second source that can disagree with the first — the exact disease the
//! `folded_state_hash` design refused when it kept the authority frontier *inside* the hash rather
//! than beside it. It also had no answer for two owner devices concurrently and legitimately
//! claiming the same epoch, and retro-condemnation would renumber every later one.
//!
//! Coverage is derivable, so it is used directly: one snapshot **dominates** another when it covers
//! every target the other does and reaches at least as far on every device. That is a partial
//! order, which is enough — a selector needs a deterministic *pick*, never a total order. Dominated
//! snapshots are discarded and the survivors are broken by `entry_hash`, so two devices holding the
//! same usable set choose the same snapshot.
//!
//! # Usability is a control-fold question
//!
//! Registers are minted per log and `ChainKind` has no annex variant, so **no control op can cut an
//! annex chain**: a revoked device's snapshots cannot be condemned by watermark the way its control
//! entries are. The rule that needs no wire change is therefore incarnation-scoped — a snapshot is
//! usable only while the owner incarnation it cites is still open, and closing that incarnation
//! (demote or remove) kills every snapshot authored under it, including ones authored long before
//! the revocation.
//!
//! That is coarser than a beyond-cut boundary and deliberately so. For this artifact class it is
//! also stronger: a compromised-then-removed owner's snapshots all become unusable at once, rather
//! than staying trusted up to a cut. The cost is availability only — snapshots are re-derivable,
//! and every peer still holds the full history this phase.

use super::ops::SnapshotTarget;

/// The key identifying which log (or content stream) a target covers.
type TargetKey = (u8, Option<[u8; 32]>, Option<[u8; 32]>);

fn target_key(target: &SnapshotTarget) -> TargetKey {
    (
        target.log_id,
        target.stream_id.map(|s| s.to_bytes()),
        target.subject_account_id.map(|a| a.to_bytes()),
    )
}

/// Whether `a` covers everything `b` does, reaching at least as far on every device.
///
/// Deliberately not a comparison of "how much" — a snapshot that covers a different set of logs is
/// incomparable, not lesser. Incomparable pairs are resolved by the caller's `entry_hash` tiebreak,
/// which is why this returns a plain bool rather than an `Ordering` it could not honestly produce.
pub(in crate::account) fn dominates(a: &[SnapshotTarget], b: &[SnapshotTarget]) -> bool {
    b.iter().all(|target| {
        let Some(mine) = a.iter().find(|candidate| target_key(candidate) == target_key(target))
        else {
            return false;
        };
        target.covered.iter().all(|watermark| {
            mine.covered
                .iter()
                .find(|m| m.device_fingerprint == watermark.device_fingerprint)
                .is_some_and(|m| m.seq >= watermark.seq)
        })
    })
}

/// One candidate for selection: its identity and the coverage it claims. A small caller-built view
/// keeps this module free of any knowledge of how snapshots are stored.
#[derive(Debug, Clone, Copy)]
pub(in crate::account) struct Candidate<'a> {
    pub(in crate::account) entry_hash: [u8; 32],
    pub(in crate::account) targets: &'a [SnapshotTarget],
}

/// Pick the snapshot to use from the usable set: discard every strictly dominated candidate, then
/// break the remaining (mutually incomparable) ones by `entry_hash`. Returns the chosen identity.
///
/// Deterministic given the same usable set, which is what matters — the set itself is
/// device-dependent, because verification depends on what a device holds, and that is fine for a
/// read-time selector. Nothing here may influence acceptance.
pub(in crate::account) fn select(usable: &[Candidate<'_>]) -> Option<[u8; 32]> {
    usable
        .iter()
        .filter(|candidate| {
            // Strictly dominated: someone else covers everything this does, and this does not
            // return the favour. Mutual domination means EQUAL coverage — neither is discarded, and
            // the hash tiebreak settles it.
            !usable.iter().any(|other| {
                other.entry_hash != candidate.entry_hash
                    && dominates(other.targets, candidate.targets)
                    && !dominates(candidate.targets, other.targets)
            })
        })
        .map(|candidate| candidate.entry_hash)
        .min()
}

#[cfg(test)]
mod tests {
    use super::super::ops::CoveredWatermark;
    use super::*;
    use crate::op::DeviceFingerprint;

    fn watermark(device: u8, seq: u64) -> CoveredWatermark {
        CoveredWatermark {
            device_fingerprint: DeviceFingerprint::from_bytes([device; 32]),
            seq,
            entry_hash: [device; 32],
        }
    }

    fn control(covered: Vec<CoveredWatermark>) -> SnapshotTarget {
        SnapshotTarget {
            log_id: 0,
            stream_id: None,
            subject_account_id: None,
            folded_state_hash: [0x2a; 32],
            covered,
        }
    }

    fn candidate<'a>(id: u8, targets: &'a [SnapshotTarget]) -> Candidate<'a> {
        Candidate { entry_hash: [id; 32], targets }
    }

    #[test]
    fn reaching_further_on_every_device_dominates() {
        let ahead = [control(vec![watermark(1, 9), watermark(2, 4)])];
        let behind = [control(vec![watermark(1, 5), watermark(2, 4)])];
        assert!(dominates(&ahead, &behind));
        assert!(!dominates(&behind, &ahead));
    }

    #[test]
    fn covering_a_device_the_other_omits_dominates() {
        let more = [control(vec![watermark(1, 5), watermark(2, 5)])];
        let fewer = [control(vec![watermark(1, 5)])];
        assert!(dominates(&more, &fewer));
        assert!(!dominates(&fewer, &more), "the shorter claim cannot cover a device it omits");
    }

    #[test]
    fn different_coverage_is_incomparable_not_lesser() {
        // Neither is "bigger": each reaches further on a device the other does not. A total order
        // would have to invent a winner here, which is why dominance is deliberately partial.
        let a = [control(vec![watermark(1, 9), watermark(2, 1)])];
        let b = [control(vec![watermark(1, 1), watermark(2, 9)])];
        assert!(!dominates(&a, &b));
        assert!(!dominates(&b, &a));
    }

    #[test]
    fn a_target_the_other_lacks_makes_it_incomparable() {
        let account_only = [control(vec![watermark(1, 5)])];
        let with_secrets = [control(vec![watermark(1, 5)]), SnapshotTarget {
            log_id: 1,
            ..control(vec![watermark(1, 3)])
        }];
        assert!(dominates(&with_secrets, &account_only));
        assert!(!dominates(&account_only, &with_secrets));
    }

    #[test]
    fn selection_prefers_coverage_and_breaks_ties_by_hash() {
        let ahead = [control(vec![watermark(1, 9)])];
        let behind = [control(vec![watermark(1, 2)])];
        // The dominated candidate loses even though its hash sorts first, so this cannot pass by
        // accident of ordering.
        let chosen = select(&[candidate(0x01, &behind), candidate(0xff, &ahead)]);
        assert_eq!(chosen, Some([0xff; 32]), "greater coverage wins over a smaller hash");

        // Equal coverage is mutual domination: neither is discarded, and the hash decides.
        let same_a = [control(vec![watermark(1, 5)])];
        let same_b = [control(vec![watermark(1, 5)])];
        assert_eq!(select(&[candidate(0xbb, &same_a), candidate(0x11, &same_b)]), Some([0x11; 32]),);
    }

    #[test]
    fn selection_is_order_independent() {
        // Two devices holding the same usable set must choose the same snapshot regardless of the
        // order they happen to enumerate it in.
        let a = [control(vec![watermark(1, 9), watermark(2, 1)])];
        let b = [control(vec![watermark(1, 1), watermark(2, 9)])];
        let c = [control(vec![watermark(1, 1)])];
        let forward = select(&[candidate(0x30, &a), candidate(0x20, &b), candidate(0x10, &c)]);
        let reversed = select(&[candidate(0x10, &c), candidate(0x20, &b), candidate(0x30, &a)]);
        assert_eq!(forward, reversed);
        assert_eq!(forward, Some([0x20; 32]), "c is dominated; a and b tie and 0x20 sorts first");
    }

    #[test]
    fn nothing_usable_selects_nothing() {
        assert_eq!(select(&[]), None);
    }
}
