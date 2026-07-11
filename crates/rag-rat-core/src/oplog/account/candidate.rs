//! The candidate-graph substrate the fold walks (§11): a hash-keyed header view, the `prev_hash`
//! ancestry walk, cut-target binding (§11.3), and the `⊔` comparable-ancestor join.
//!
//! These are the parts of cut evaluation that need the CANDIDATE DAG (not just a `[seq]`, which the
//! self-contained [`super::cut::beyond`] handles). They are pure functions of a [`HeaderView`] —
//! the fold implements it over its candidate map; tests implement it over a small `HashMap`.

use std::collections::{HashMap, HashSet};

use super::AccountId;
use super::cut::Cut;
use super::envelope::AccountEntryHeader;
use crate::oplog::op::DeviceFingerprint;

/// A read view over candidate entries keyed by `entry_hash` — the seam the ancestry walk and cut
/// binding use without depending on the fold's storage.
pub(super) trait HeaderView {
    fn header(&self, entry_hash: &[u8; 32]) -> Option<&AccountEntryHeader>;
}

impl HeaderView for HashMap<[u8; 32], AccountEntryHeader> {
    fn header(&self, entry_hash: &[u8; 32]) -> Option<&AccountEntryHeader> {
        self.get(entry_hash)
    }
}

/// Why an ancestry walk could not be decided yet (a WITHHELD watermark parks, never flips a verdict
/// — I11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnknownCause {
    /// The cut's watermark entry itself is not held.
    UnknownCutTarget,
    /// A link on the walk from the watermark toward the target is missing.
    IncompleteCutAncestry,
}

/// The result of walking `prev_hash` from a cut's watermark toward a target entry (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ancestry {
    /// The target is an ancestor of (or equal to) the watermark — within the cut's accepted branch.
    OnBranch,
    /// The watermark's branch reaches genesis without passing the target — a different branch (L2).
    OffBranch,
    /// Undecidable until more entries arrive.
    Unknown(UnknownCause),
}

/// Walk `prev_hash` from `cut`'s watermark and decide whether `target` lies on that branch (§11). A
/// withheld watermark / missing link parks (`Unknown`); it NEVER flips an on/off verdict. `Empty`
/// has no branch, so ancestry is `OffBranch` (callers test [`super::cut::beyond`] first).
pub(super) fn ancestry(target: &[u8; 32], cut: &Cut, view: &dyn HeaderView) -> Ancestry {
    let Some(watermark) = cut.hash() else {
        return Ancestry::OffBranch;
    };
    let Some(wm) = view.header(&watermark) else {
        return Ancestry::Unknown(UnknownCause::UnknownCutTarget);
    };
    // The cut bounds ONE device's log: every link on the walk must stay on the watermark's
    // `(account, log, device)` coordinate with a STRICTLY-DECREASING seq. A signed header only pins
    // `prev_hash` NULLITY, not that `prev` is a valid parent, so a forged link (jumping coordinate
    // or not decreasing seq) is not a real predecessor — the branch ends `OffBranch` there.
    let (account, log, device) = (wm.account_id, wm.log_id, wm.device_fingerprint);
    // Hash chains cannot cycle (a cycle needs a sha256 collision), but guard a corrupt input
    // against an infinite loop by refusing to revisit a hash.
    let mut visited: HashSet<[u8; 32]> = HashSet::new();
    let mut current = watermark;
    loop {
        let Some(header) = view.header(&current) else {
            return Ancestry::Unknown(UnknownCause::IncompleteCutAncestry);
        };
        // Validate the node is on the bounded chain BEFORE counting it as the target — a forged
        // link straight to a foreign / off-coordinate entry is not a real on-branch
        // predecessor.
        if header.account_id != account
            || header.log_id != log
            || header.device_fingerprint != device
        {
            return Ancestry::OffBranch;
        }
        if &current == target {
            return Ancestry::OnBranch;
        }
        if !visited.insert(current) {
            return Ancestry::OffBranch;
        }
        let Some(prev) = header.prev_hash else {
            return Ancestry::OffBranch; // reached the chain origin without hitting the target
        };
        // If the predecessor is held, its seq must be strictly lower (a real parent link).
        if let Some(prev_header) = view.header(&prev)
            && prev_header.seq >= header.seq
        {
            return Ancestry::OffBranch;
        }
        current = prev;
    }
}

/// The `(account, log, device)` coordinate a control/secrets cut's watermark MUST name (§11.3). (A
/// content cut names a `stream_id`; that binding is C2.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CutCoordinate {
    pub(super) account: AccountId,
    pub(super) log: u8,
    pub(super) device: DeviceFingerprint,
}

/// The outcome of validating a cut's watermark hash against the coordinate its register is for
/// (§11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CutBinding {
    /// `Empty`, or the watermark names the exact `(coordinate, seq)`.
    Ok,
    /// The watermark entry is not held yet — park, do not reject.
    TargetNotHeld,
    /// The watermark names a DIFFERENT coordinate — a structural reject (§11.3).
    Mismatch,
}

/// Validate a cut's watermark hash names the exact `(account, log, device, seq)` its register is
/// for (§11.3). A hash naming a different coordinate is a structural reject; a not-yet-held
/// watermark parks.
pub(super) fn validate_cut_target(
    cut: &Cut,
    expected: &CutCoordinate,
    view: &dyn HeaderView,
) -> CutBinding {
    let Cut::At { seq, hash } = cut else {
        return CutBinding::Ok;
    };
    match view.header(hash) {
        None => CutBinding::TargetNotHeld,
        Some(header) => {
            let names_coordinate = header.account_id == expected.account
                && header.log_id == expected.log
                && header.device_fingerprint == expected.device
                && header.seq == *seq;
            if names_coordinate { CutBinding::Ok } else { CutBinding::Mismatch }
        },
    }
}

/// The result of the `⊔` comparable-ancestor join of two cuts for one register key (§11.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JoinResult {
    /// The two are on one branch; the join is the higher watermark.
    Extended(Cut),
    /// Equal-seq different-hash, or divergent branches — a same-depth mutual condemnation ⇒
    /// contested.
    Incomparable,
    /// The branch relation can't be decided yet (a withheld watermark) — park.
    Unknown,
}

/// Join two cuts for one register key (§11.3): `Empty` is the bottom; equal seq requires equal hash
/// (else `Incomparable`); otherwise the higher watermark must be `OnBranch`-descended from the
/// lower.
pub(super) fn join_cuts(a: &Cut, b: &Cut, view: &dyn HeaderView) -> JoinResult {
    match (a, b) {
        (Cut::Empty, other) | (other, Cut::Empty) => JoinResult::Extended(other.clone()),
        (Cut::At { seq: seq_a, hash: hash_a }, Cut::At { seq: seq_b, hash: hash_b }) => {
            if seq_a == seq_b {
                return if hash_a == hash_b {
                    JoinResult::Extended(a.clone())
                } else {
                    JoinResult::Incomparable
                };
            }
            let (lower, higher) = if seq_a < seq_b { (a, b) } else { (b, a) };
            // `higher` must descend `lower`: walking prev_hash from higher's watermark reaches
            // lower's watermark.
            let lower_hash = lower.hash().expect("At cut has a hash");
            match ancestry(&lower_hash, higher, view) {
                Ancestry::OnBranch => JoinResult::Extended(higher.clone()),
                Ancestry::OffBranch => JoinResult::Incomparable,
                Ancestry::Unknown(_) => JoinResult::Unknown,
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a header at `(account 0xaa, log 0, device 0xbb, seq)` with `prev_hash`, keyed in
    /// `view` under `entry_hash`.
    fn insert_chain_entry(
        view: &mut HashMap<[u8; 32], AccountEntryHeader>,
        entry_hash: [u8; 32],
        seq: u64,
        prev_hash: Option<[u8; 32]>,
    ) {
        view.insert(entry_hash, AccountEntryHeader {
            account_id: AccountId::from_bytes([0xaa; 32]),
            log_id: 0,
            device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
            seq,
            prev_hash,
            parent_ref: None,
            entry_type: 3,
            op_version: 1,
            crypto_suite: 0,
            auth_len: seq,
            key_id: None,
            authority_ref: None,
        });
    }

    /// A three-entry chain g(seq0) <- m(seq1) <- t(seq2), hashes g/m/t.
    fn linear_chain() -> HashMap<[u8; 32], AccountEntryHeader> {
        let mut view = HashMap::new();
        insert_chain_entry(&mut view, [0x0a; 32], 0, None);
        insert_chain_entry(&mut view, [0x0b; 32], 1, Some([0x0a; 32]));
        insert_chain_entry(&mut view, [0x0c; 32], 2, Some([0x0b; 32]));
        view
    }

    #[test]
    fn ancestry_on_and_off_branch() {
        let view = linear_chain();
        let cut = Cut::At { seq: 2, hash: [0x0c; 32] };
        // g and m are ancestors of the seq-2 watermark; a stranger is not.
        assert_eq!(ancestry(&[0x0a; 32], &cut, &view), Ancestry::OnBranch);
        assert_eq!(ancestry(&[0x0b; 32], &cut, &view), Ancestry::OnBranch);
        assert_eq!(ancestry(&[0x0c; 32], &cut, &view), Ancestry::OnBranch);
        assert_eq!(ancestry(&[0xff; 32], &cut, &view), Ancestry::OffBranch);
    }

    #[test]
    fn ancestry_rejects_forged_links_off_the_bounded_chain() {
        let mut view = linear_chain(); // g/m/t on (account 0xaa, log 0, device 0xbb)
        // A forged watermark on device 0xbb whose prev_hash points at an entry on a DIFFERENT
        // device chain (0xcc) — a fabricated cross-device link, not a real predecessor.
        view.insert([0xf0; 32], AccountEntryHeader {
            account_id: AccountId::from_bytes([0xaa; 32]),
            log_id: 0,
            device_fingerprint: DeviceFingerprint::from_bytes([0xbb; 32]),
            seq: 5,
            prev_hash: Some([0xcc; 32]), // points off-chain
            parent_ref: None,
            entry_type: 3,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 5,
            key_id: None,
            authority_ref: None,
        });
        view.insert([0xcc; 32], AccountEntryHeader {
            account_id: AccountId::from_bytes([0xaa; 32]),
            log_id: 0,
            device_fingerprint: DeviceFingerprint::from_bytes([0xcc; 32]), // foreign device
            seq: 4,
            prev_hash: None,
            parent_ref: None,
            entry_type: 3,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 4,
            key_id: None,
            authority_ref: None,
        });
        // The foreign entry is NOT on the forged watermark's branch — the link is rejected.
        let forged = Cut::At { seq: 5, hash: [0xf0; 32] };
        assert_eq!(ancestry(&[0xcc; 32], &forged, &view), Ancestry::OffBranch);

        // A non-decreasing-seq link is likewise forged: a watermark at seq 1 whose prev points at a
        // same-device entry at seq 1 (not a lower slot).
        let mut v2 = HashMap::new();
        insert_chain_entry(&mut v2, [0x01; 32], 1, Some([0x02; 32]));
        insert_chain_entry(&mut v2, [0x02; 32], 1, None); // sibling at the SAME seq
        let flat = Cut::At { seq: 1, hash: [0x01; 32] };
        assert_eq!(ancestry(&[0x02; 32], &flat, &v2), Ancestry::OffBranch);
    }

    #[test]
    fn ancestry_parks_on_a_withheld_watermark_but_not_forever() {
        let view = linear_chain();
        // A watermark we don't hold ⇒ Unknown(UnknownCutTarget), never a flipped verdict (I11).
        let cut = Cut::At { seq: 9, hash: [0x99; 32] };
        assert_eq!(
            ancestry(&[0x0a; 32], &cut, &view),
            Ancestry::Unknown(UnknownCause::UnknownCutTarget),
        );
    }

    #[test]
    fn ancestry_parks_on_a_missing_mid_chain_link() {
        // Hold the seq-2 watermark but NOT its predecessor: the walk can't reach the target.
        let mut view = HashMap::new();
        insert_chain_entry(&mut view, [0x0c; 32], 2, Some([0x0b; 32]));
        let cut = Cut::At { seq: 2, hash: [0x0c; 32] };
        assert_eq!(
            ancestry(&[0x0a; 32], &cut, &view),
            Ancestry::Unknown(UnknownCause::IncompleteCutAncestry),
        );
    }

    #[test]
    fn validate_cut_target_binds_the_exact_coordinate() {
        let view = linear_chain();
        let coord = CutCoordinate {
            account: AccountId::from_bytes([0xaa; 32]),
            log: 0,
            device: DeviceFingerprint::from_bytes([0xbb; 32]),
        };
        // The seq-2 watermark names (aa,0,bb,2) — Ok.
        assert_eq!(
            validate_cut_target(&Cut::At { seq: 2, hash: [0x0c; 32] }, &coord, &view),
            CutBinding::Ok
        );
        // Empty is always Ok (no target).
        assert_eq!(validate_cut_target(&Cut::Empty, &coord, &view), CutBinding::Ok);
        // A watermark whose entry claims seq 2 but the cut says seq 1 ⇒ Mismatch.
        assert_eq!(
            validate_cut_target(&Cut::At { seq: 1, hash: [0x0c; 32] }, &coord, &view),
            CutBinding::Mismatch
        );
        // A different device coordinate ⇒ Mismatch.
        let other = CutCoordinate { device: DeviceFingerprint::from_bytes([0xcc; 32]), ..coord };
        assert_eq!(
            validate_cut_target(&Cut::At { seq: 2, hash: [0x0c; 32] }, &other, &view),
            CutBinding::Mismatch
        );
        // A not-yet-held watermark parks.
        assert_eq!(
            validate_cut_target(&Cut::At { seq: 5, hash: [0x55; 32] }, &coord, &view),
            CutBinding::TargetNotHeld
        );
    }

    #[test]
    fn join_extends_on_one_branch_and_flags_incomparable_forks() {
        let view = linear_chain();
        let lo = Cut::At { seq: 1, hash: [0x0b; 32] };
        let hi = Cut::At { seq: 2, hash: [0x0c; 32] };
        // hi descends lo ⇒ Extended(hi).
        assert_eq!(join_cuts(&lo, &hi, &view), JoinResult::Extended(hi.clone()));
        assert_eq!(join_cuts(&hi, &lo, &view), JoinResult::Extended(hi.clone()));
        // Empty ⊔ anything = anything.
        assert_eq!(join_cuts(&Cut::Empty, &hi, &view), JoinResult::Extended(hi.clone()));
        // Equal seq, different hash ⇒ Incomparable (the contested trigger).
        let other_at_2 = Cut::At { seq: 2, hash: [0xee; 32] };
        assert_eq!(join_cuts(&hi, &other_at_2, &view), JoinResult::Incomparable);
        // Higher seq on a FULLY-HELD divergent branch (forks at genesis) ⇒ OffBranch ⇒
        // Incomparable.
        let mut forked = view.clone();
        insert_chain_entry(&mut forked, [0xd1; 32], 1, Some([0x0a; 32])); // sibling of 0x0b off genesis
        insert_chain_entry(&mut forked, [0xd2; 32], 2, Some([0xd1; 32]));
        assert_eq!(
            join_cuts(&lo, &Cut::At { seq: 2, hash: [0xd2; 32] }, &forked),
            JoinResult::Incomparable,
        );
        // Higher seq whose branch has a WITHHELD predecessor ⇒ Unknown (park, never a flipped
        // verdict).
        let mut incomplete = view.clone();
        insert_chain_entry(&mut incomplete, [0xdd; 32], 3, Some([0xff; 32])); // 0xff is not held
        assert_eq!(
            join_cuts(&lo, &Cut::At { seq: 3, hash: [0xdd; 32] }, &incomplete),
            JoinResult::Unknown,
        );
    }
}
