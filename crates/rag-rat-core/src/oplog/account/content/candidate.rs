//! The `/3` candidate-graph substrate the acceptance refold walks (§16.2): a hash-keyed header
//! view, the `prev_hash` ancestry walk a content cut is evaluated against, cut-target binding
//! (§11.3 for a content coordinate), and branch selection.
//!
//! These are the parts of `/3` classification that need the CANDIDATE DAG rather than the account
//! log. They are pure functions of a [`HeaderView`] — storage implements it over the candidate
//! rows, tests over a small `HashMap` — so the whole substrate is testable without a database.
//!
//! The account log has the same substrate (`account::candidate`) and the rules are deliberately
//! identical: a withheld watermark parks and never flips a verdict (I11), and a forged link (one
//! that jumps chain coordinate or skips a seq slot) is not a real predecessor.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use super::acceptance::{AncestryRelation, UnknownAncestry};
use super::envelope::ContentEntryHeader;
use crate::oplog::account::AccountId;
use crate::oplog::op::DeviceFingerprint;
use crate::oplog::stream::StreamId;

type EntryHash = [u8; 32];

/// A read view over `/3` candidates keyed by `entry_hash` — the seam the walks use without
/// depending on storage.
pub(super) trait HeaderView {
    fn header(&self, entry_hash: &EntryHash) -> Option<&ContentEntryHeader>;
}

impl HeaderView for HashMap<EntryHash, ContentEntryHeader> {
    fn header(&self, entry_hash: &EntryHash) -> Option<&ContentEntryHeader> {
        self.get(entry_hash)
    }
}

/// The chain one `/3` cut bounds: a `/3` seq is dense per `(stream, author_account, device)`, so
/// that triple — not the account log's `(account, log, device)` — is the content cut coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ChainCoordinate {
    pub(super) stream_id: StreamId,
    pub(super) author_account_id: AccountId,
    pub(super) device_fingerprint: DeviceFingerprint,
}

impl ChainCoordinate {
    fn of(header: &ContentEntryHeader) -> Self {
        Self {
            stream_id: header.stream_id,
            author_account_id: header.author_account_id,
            device_fingerprint: header.device_fingerprint,
        }
    }
}

/// One `/3` candidate the refold classifies: its hash plus the header the walks read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContentCandidate {
    pub(super) entry_hash: EntryHash,
    pub(super) header: ContentEntryHeader,
}

/// Decide whether `target` lies on the branch `watermark` heads (§11) — the ancestry relation the
/// acceptance predicate evaluates a revocation cut against.
///
/// A withheld watermark or a missing mid-chain link is UNDECIDED, and says so with its cause; it
/// never flips an on/off verdict (I11), because the entries that would decide it may still arrive.
pub(super) fn ancestry(
    target: &EntryHash,
    watermark: &EntryHash,
    view: &dyn HeaderView,
) -> AncestryRelation {
    if view.header(watermark).is_none() {
        return AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget);
    }
    let mut found = false;
    let end = walk_back(watermark, view, |hash, _| {
        if hash == target {
            found = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    if found {
        return AncestryRelation::OnBranch;
    }
    match end {
        // The walk left the bounded chain, cycled, or reached the origin without passing the
        // target: the target heads a different branch, and no later arrival can change that.
        WalkEnd::Stopped | WalkEnd::Origin | WalkEnd::ForgedLink => AncestryRelation::OffBranch,
        WalkEnd::MissingLink => AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry),
    }
}

/// The outcome of validating a `/3` cut's watermark against the coordinate its register bounds
/// (§11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CutBinding {
    /// The watermark names the exact `(stream, author_account, device, seq)` the register is for.
    Ok,
    /// The watermark entry is not held yet — park, do not reject.
    TargetNotHeld,
    /// The watermark names a DIFFERENT coordinate: the cut is structurally invalid, so it may
    /// neither condemn nor pin. Fail-closed here would be backwards — a malformed cut that could
    /// condemn honest entries is exactly the laundering §11.3 forbids.
    Mismatch,
}

/// Validate that a cut's watermark names the exact `(coordinate, seq)` its register bounds (§11.3).
pub(super) fn validate_cut_target(
    seq: u64,
    watermark: &EntryHash,
    expected: &ChainCoordinate,
    view: &dyn HeaderView,
) -> CutBinding {
    let Some(header) = view.header(watermark) else {
        return CutBinding::TargetNotHeld;
    };
    if ChainCoordinate::of(header) == *expected && header.seq == seq {
        CutBinding::Ok
    } else {
        CutBinding::Mismatch
    }
}

/// A register watermark that pins one chain's accepted branch (§16.2). Sourced from the account
/// log's revocation cuts; a cut naming a currently-`forked` branch PROMOTES it on the next refold,
/// which is what makes an off-branch condemnation enforceable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BranchPin {
    pub(super) coordinate: ChainCoordinate,
    pub(super) seq: u64,
    pub(super) watermark: EntryHash,
}

/// Eligible entries indexed by the `(chain, prev_hash)` parent slot they extend; a chain root keys
/// on `None`. Several entries under one key are an equivocation — the slot selection resolves them.
type BranchChildren = HashMap<(ChainCoordinate, Option<EntryHash>), Vec<(u64, EntryHash)>>;

/// The branch-selection verdict for one refold: the entries on each chain's accepted branch, and
/// the equivocation losers (`forked` — terminal unless a later watermark selects them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct BranchSelection {
    pub(super) accepted: HashSet<EntryHash>,
    pub(super) forked: HashSet<EntryHash>,
}

/// Select one contiguous accepted chain per `(stream, author_account, device)` from the eligible
/// candidates (§16.2).
///
/// `eligible` is the caller's authority verdict — the candidates no register has condemned and no
/// citation has rejected. A condemned entry must not compete for a slot, or an attacker could mine
/// a small-hash entry beyond a cut and fork an honest sibling out of the accepted branch.
///
/// Selection walks the dense seq slots from 0, extending only from the entry that won the previous
/// slot, so the accepted set is always one hash-linked chain. At a slot with several eligible
/// children the winner is:
/// 1. the child on a pinning watermark's branch, if a register pins this chain — a cut names the
///    branch it bounds, so the register decides, not the hash order; otherwise
/// 2. the lexicographically smallest `entry_hash` — an unforced fork resolved by a rule both peers
///    compute identically.
pub(super) fn select_accepted_branch(
    candidates: &[ContentCandidate],
    eligible: &HashSet<EntryHash>,
    pins: &[BranchPin],
    view: &dyn HeaderView,
) -> BranchSelection {
    let mut children = BranchChildren::new();
    let mut chains: HashSet<ChainCoordinate> = HashSet::new();
    for candidate in candidates.iter().filter(|c| eligible.contains(&c.entry_hash)) {
        let coordinate = ChainCoordinate::of(&candidate.header);
        children
            .entry((coordinate, candidate.header.prev_hash))
            .or_default()
            .push((candidate.header.seq, candidate.entry_hash));
        chains.insert(coordinate);
    }

    let mut accepted = HashSet::new();
    for chain in chains {
        let pinned = pinned_branch(chain, pins, view);
        let mut parent: Option<EntryHash> = None;
        // A `/3` seq is dense from 0, so the chain ends at the first slot no eligible child fills.
        // Bounded by the candidate count: every step consumes one distinct entry.
        for slot in 0..candidates.len() as u64 {
            let Some(winner) = children.get(&(chain, parent)).and_then(|kids| {
                let at_slot = kids.iter().filter(|(seq, _)| *seq == slot);
                at_slot
                    .clone()
                    .find(|(_, hash)| pinned.contains(hash))
                    .or_else(|| at_slot.min_by_key(|(_, hash)| *hash))
                    .map(|(_, hash)| *hash)
            }) else {
                break;
            };
            accepted.insert(winner);
            parent = Some(winner);
        }
    }

    let forked = candidates
        .iter()
        .filter(|c| eligible.contains(&c.entry_hash) && !accepted.contains(&c.entry_hash))
        .map(|c| c.entry_hash)
        .collect();
    BranchSelection { accepted, forked }
}

/// The entries on the branch a register pins for `chain` — empty when no cut pins it, or when the
/// pinning watermark is withheld or names a foreign coordinate (neither may steer selection).
///
/// The HIGHEST admitted watermark wins: a register only ever extends forward, so the deepest cut is
/// the most recent statement about which branch is real.
///
/// TWO registers can bound one chain — the author's roster cut and the owner's grant cut — so once
/// a device equivocates they can name different watermarks at the SAME seq. Order the pins by
/// `(seq, watermark)`, a total order, so the winner never depends on the order the registers were
/// assembled in: `max_by_key` on `seq` alone hands a tie to whichever pin happens to come last, and
/// two peers holding identical entries would derive different accepted branches from it.
fn pinned_branch(
    chain: ChainCoordinate,
    pins: &[BranchPin],
    view: &dyn HeaderView,
) -> HashSet<EntryHash> {
    let Some(pin) = pins
        .iter()
        .filter(|pin| pin.coordinate == chain)
        .filter(|pin| validate_cut_target(pin.seq, &pin.watermark, &chain, view) == CutBinding::Ok)
        .max_by_key(|pin| (pin.seq, pin.watermark))
    else {
        return HashSet::new();
    };
    let mut branch = HashSet::from([pin.watermark]);
    walk_back(&pin.watermark, view, |hash, _| {
        branch.insert(*hash);
        ControlFlow::Continue(())
    });
    branch
}

/// Why a backward walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkEnd {
    /// Reached the chain origin (`prev_hash` is null).
    Origin,
    /// A link on the walk is not held — more entries could still decide this.
    MissingLink,
    /// A link left the bounded chain or skipped a seq slot: it is forged, not a real predecessor.
    ForgedLink,
    /// The visitor broke out early.
    Stopped,
}

/// Walk `prev_hash` back from `watermark`, visiting each entry on the ONE bounded chain it heads.
///
/// A signed header pins only `prev_hash` NULLITY, never that `prev` is a valid contiguous parent —
/// so the walk re-derives that: every link must stay on the watermark's `(stream, author, device)`
/// coordinate and step down exactly one seq slot. A link that jumps coordinate or skips slots is
/// forged, and the walk refuses to follow it.
///
/// Iterative with a visited guard: chain depth is attacker-controlled, and a hash cycle would need
/// a sha256 collision but a corrupt row must not spin forever either.
fn walk_back(
    watermark: &EntryHash,
    view: &dyn HeaderView,
    mut visit: impl FnMut(&EntryHash, &ContentEntryHeader) -> ControlFlow<()>,
) -> WalkEnd {
    let Some(head) = view.header(watermark) else {
        return WalkEnd::MissingLink;
    };
    let chain = ChainCoordinate::of(head);
    let mut visited: HashSet<EntryHash> = HashSet::new();
    let mut current = *watermark;
    loop {
        let Some(header) = view.header(&current) else {
            return WalkEnd::MissingLink;
        };
        // Validate the node is on the bounded chain BEFORE the visitor counts it: a forged link
        // straight to a foreign entry is not a real predecessor, so it is not on this branch.
        if ChainCoordinate::of(header) != chain {
            return WalkEnd::ForgedLink;
        }
        if visit(&current, header).is_break() {
            return WalkEnd::Stopped;
        }
        if !visited.insert(current) {
            return WalkEnd::ForgedLink;
        }
        let Some(prev) = header.prev_hash else {
            return WalkEnd::Origin;
        };
        // A dense chain is contiguous, so a held predecessor MUST be the exactly-preceding slot; a
        // link that skips slots (5 → 3) is forged, not a real parent.
        if let Some(prev_header) = view.header(&prev)
            && prev_header.seq + 1 != header.seq
        {
            return WalkEnd::ForgedLink;
        }
        current = prev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: [u8; 32] = [1; 32];
    const AUTHOR: [u8; 32] = [2; 32];
    const DEVICE: [u8; 32] = [3; 32];

    fn chain() -> ChainCoordinate {
        ChainCoordinate {
            stream_id: StreamId::from_bytes(STREAM),
            author_account_id: AccountId::from_bytes(AUTHOR),
            device_fingerprint: DeviceFingerprint::from_bytes(DEVICE),
        }
    }

    fn header(seq: u64, prev_hash: Option<EntryHash>) -> ContentEntryHeader {
        ContentEntryHeader {
            stream_id: StreamId::from_bytes(STREAM),
            author_account_id: AccountId::from_bytes(AUTHOR),
            device_fingerprint: DeviceFingerprint::from_bytes(DEVICE),
            seq,
            lamport: 0,
            prev_hash,
            grant_id: None,
            roster_ref: [9; 32],
            owner_auth_len: 1,
            author_auth_len: 1,
            crypto_suite: 0,
            key_id: None,
        }
    }

    /// A linear chain a(seq0) <- b(seq1) <- c(seq2).
    fn linear() -> HashMap<EntryHash, ContentEntryHeader> {
        HashMap::from([
            ([0x0a; 32], header(0, None)),
            ([0x0b; 32], header(1, Some([0x0a; 32]))),
            ([0x0c; 32], header(2, Some([0x0b; 32]))),
        ])
    }

    fn candidates(view: &HashMap<EntryHash, ContentEntryHeader>) -> Vec<ContentCandidate> {
        let mut rows: Vec<ContentCandidate> = view
            .iter()
            .map(|(entry_hash, header)| ContentCandidate {
                entry_hash: *entry_hash,
                header: header.clone(),
            })
            .collect();
        // Storage returns rows in an arbitrary order; selection must not depend on it.
        rows.sort_by_key(|row| row.entry_hash);
        rows
    }

    fn all(view: &HashMap<EntryHash, ContentEntryHeader>) -> HashSet<EntryHash> {
        view.keys().copied().collect()
    }

    #[test]
    fn ancestry_decides_on_and_off_branch_against_a_held_watermark() {
        let view = linear();
        assert_eq!(ancestry(&[0x0a; 32], &[0x0c; 32], &view), AncestryRelation::OnBranch);
        assert_eq!(ancestry(&[0x0b; 32], &[0x0c; 32], &view), AncestryRelation::OnBranch);
        // The watermark is its own ancestor: an entry AT the cut is within it.
        assert_eq!(ancestry(&[0x0c; 32], &[0x0c; 32], &view), AncestryRelation::OnBranch);
        assert_eq!(ancestry(&[0xff; 32], &[0x0c; 32], &view), AncestryRelation::OffBranch);
    }

    #[test]
    fn ancestry_parks_with_the_cause_that_says_what_to_refetch() {
        let view = linear();
        // A watermark we do not hold: fetch THAT entry.
        assert_eq!(
            ancestry(&[0x0a; 32], &[0x99; 32], &view),
            AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget),
        );
        // A watermark we hold whose chain has a gap: fetch the walk.
        let gapped = HashMap::from([([0x0c; 32], header(2, Some([0x0b; 32])))]);
        assert_eq!(
            ancestry(&[0x0a; 32], &[0x0c; 32], &gapped),
            AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry),
        );
    }

    #[test]
    fn ancestry_refuses_forged_links_off_the_bounded_chain() {
        // A watermark on our device whose prev_hash points at a FOREIGN device's entry: a
        // fabricated cross-chain link is not a real predecessor.
        let mut foreign = header(4, None);
        foreign.device_fingerprint = DeviceFingerprint::from_bytes([0xcc; 32]);
        let cross =
            HashMap::from([([0xf0; 32], header(5, Some([0xcc; 32]))), ([0xcc; 32], foreign)]);
        assert_eq!(ancestry(&[0xcc; 32], &[0xf0; 32], &cross), AncestryRelation::OffBranch);

        // A link that does not step down exactly one slot is forged: a seq-5 header whose prev is
        // seq 3 skips slot 4, and the skipped slots must not look on-branch.
        let skipping = HashMap::from([
            ([0x05; 32], header(5, Some([0x03; 32]))),
            ([0x03; 32], header(3, None)),
        ]);
        assert_eq!(ancestry(&[0x03; 32], &[0x05; 32], &skipping), AncestryRelation::OffBranch);

        // A same-slot "predecessor" is forged too — a chain strictly descends.
        let flat = HashMap::from([
            ([0x01; 32], header(1, Some([0x02; 32]))),
            ([0x02; 32], header(1, None)),
        ]);
        assert_eq!(ancestry(&[0x02; 32], &[0x01; 32], &flat), AncestryRelation::OffBranch);
    }

    #[test]
    fn cut_target_binding_names_the_exact_content_coordinate() {
        let view = linear();
        assert_eq!(validate_cut_target(2, &[0x0c; 32], &chain(), &view), CutBinding::Ok);
        // The entry is at seq 2, so a cut claiming it sits at seq 1 is lying about the coordinate.
        assert_eq!(validate_cut_target(1, &[0x0c; 32], &chain(), &view), CutBinding::Mismatch);
        // A cut for a DIFFERENT device's chain may not name this entry.
        let other = ChainCoordinate {
            device_fingerprint: DeviceFingerprint::from_bytes([0xcc; 32]),
            ..chain()
        };
        assert_eq!(validate_cut_target(2, &[0x0c; 32], &other, &view), CutBinding::Mismatch);
        // A withheld watermark parks; it is never a reject.
        assert_eq!(validate_cut_target(9, &[0x99; 32], &chain(), &view), CutBinding::TargetNotHeld,);
    }

    #[test]
    fn selection_takes_one_contiguous_chain_and_stops_at_the_first_empty_slot() {
        let mut view = linear();
        // A seq-4 entry with slot 3 missing: the dense chain ends at 2, so it cannot be accepted.
        view.insert([0x0e; 32], header(4, Some([0x0d; 32])));
        let rows = candidates(&view);
        let selection = select_accepted_branch(&rows, &all(&view), &[], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]),);
        assert_eq!(selection.forked, HashSet::from([[0x0e; 32]]));
    }

    #[test]
    fn an_unforced_fork_resolves_to_the_smaller_hash_and_the_loser_is_terminal() {
        let mut view = linear();
        // An equivocating sibling at seq 1, with a LARGER hash than the incumbent 0x0b, plus a
        // child of its own: losing the slot forks the whole branch, not just the one entry.
        view.insert([0x1b; 32], header(1, Some([0x0a; 32])));
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let rows = candidates(&view);
        let selection = select_accepted_branch(&rows, &all(&view), &[], &view);
        assert_eq!(
            selection.accepted,
            HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]),
            "0x0b < 0x1b decides the slot, and only its descendants can extend the chain",
        );
        assert_eq!(selection.forked, HashSet::from([[0x1b; 32], [0x1c; 32]]));
    }

    #[test]
    fn a_register_watermark_promotes_the_branch_it_names_over_the_hash_order() {
        let mut view = linear();
        // The equivocating sibling has the LARGER hash, so the unforced rule would fork it out.
        view.insert([0x1b; 32], header(1, Some([0x0a; 32])));
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let rows = candidates(&view);

        // A revocation cut naming 0x1c says THAT branch is the real one: the register decides, and
        // the branch it names is promoted even though the hash order would have lost it. This is
        // what makes an off-branch condemnation of the other branch enforceable.
        let pin = BranchPin { coordinate: chain(), seq: 2, watermark: [0x1c; 32] };
        let selection = select_accepted_branch(&rows, &all(&view), &[pin], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x1b; 32], [0x1c; 32]]),);
        assert_eq!(selection.forked, HashSet::from([[0x0b; 32], [0x0c; 32]]));
    }

    #[test]
    fn the_highest_watermark_pins_and_a_withheld_or_foreign_one_cannot_steer_selection() {
        let mut view = linear();
        view.insert([0x1b; 32], header(1, Some([0x0a; 32])));
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let rows = candidates(&view);
        let low = BranchPin { coordinate: chain(), seq: 1, watermark: [0x0b; 32] };
        let high = BranchPin { coordinate: chain(), seq: 2, watermark: [0x1c; 32] };

        // Registers only extend forward, so the deepest cut is the most recent statement.
        let selection = select_accepted_branch(&rows, &all(&view), &[low, high], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x1b; 32], [0x1c; 32]]));

        // A watermark we do not hold cannot pin anything: fall back to the unforced rule rather
        // than let a withheld entry silently re-select the branch (I11).
        let withheld = BranchPin { coordinate: chain(), seq: 2, watermark: [0x99; 32] };
        let selection = select_accepted_branch(&rows, &all(&view), &[withheld], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]));

        // Nor can a cut whose watermark names a foreign coordinate — that cut is structurally
        // invalid, and a malformed cut must not get to choose the accepted branch.
        let foreign = BranchPin {
            coordinate: ChainCoordinate {
                device_fingerprint: DeviceFingerprint::from_bytes([0xcc; 32]),
                ..chain()
            },
            seq: 2,
            watermark: [0x1c; 32],
        };
        let selection = select_accepted_branch(&rows, &all(&view), &[foreign], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]));
    }

    #[test]
    fn equal_depth_pins_naming_divergent_branches_resolve_independently_of_pin_order() {
        // One chain can be bounded by TWO registers — the author's roster cut and the owner's grant
        // cut — so an equivocating device can leave them naming different seq-2 watermarks on
        // divergent branches. Neither register outranks the other by depth, and the pins reach us
        // from unordered projection reads, so the branch must not depend on which arrived first.
        let mut view = linear();
        view.insert([0x1b; 32], header(1, Some([0x0a; 32])));
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let rows = candidates(&view);
        let roster = BranchPin { coordinate: chain(), seq: 2, watermark: [0x0c; 32] };
        let grant = BranchPin { coordinate: chain(), seq: 2, watermark: [0x1c; 32] };

        let one_way = select_accepted_branch(&rows, &all(&view), &[roster, grant], &view);
        let other_way = select_accepted_branch(&rows, &all(&view), &[grant, roster], &view);
        assert_eq!(
            one_way, other_way,
            "conflicting equal-depth pins must resolve by a total order, not by arrival",
        );
        // The total order is (seq, watermark), so the larger watermark takes the tie.
        assert_eq!(one_way.accepted, HashSet::from([[0x0a; 32], [0x1b; 32], [0x1c; 32]]));
    }

    #[test]
    fn a_condemned_entry_never_competes_for_a_slot() {
        let mut view = linear();
        // An equivocating sibling at seq 1 whose hash is SMALLER than the incumbent: it would win
        // the unforced tiebreak, so an attacker could mine one to fork honest work off the branch.
        // Being ineligible (a register condemned it) it never enters selection at all.
        view.insert([0x00; 32], header(1, Some([0x0a; 32])));
        let rows = candidates(&view);
        let mut eligible = all(&view);
        eligible.remove(&[0x00; 32]);

        let selection = select_accepted_branch(&rows, &eligible, &[], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]));
        assert!(
            !selection.forked.contains(&[0x00; 32]),
            "an ineligible entry keeps the verdict that excluded it; selection does not relabel it",
        );
    }

    #[test]
    fn selection_is_independent_of_row_order() {
        let mut view = linear();
        view.insert([0x1b; 32], header(1, Some([0x0a; 32])));
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let mut rows = candidates(&view);
        let expected = select_accepted_branch(&rows, &all(&view), &[], &view);

        // Arrival order is adversary-controlled; the derived branch is not.
        for rotation in 1..rows.len() {
            rows.rotate_left(rotation);
            let selection = select_accepted_branch(&rows, &all(&view), &[], &view);
            assert_eq!(selection, expected, "row order must not change the accepted branch");
        }
        rows.reverse();
        assert_eq!(select_accepted_branch(&rows, &all(&view), &[], &view), expected);
    }

    #[test]
    fn independent_chains_are_selected_independently() {
        let mut view = linear();
        // A second device's chain in the same stream: its own dense slots, its own branch.
        let mut other = header(0, None);
        other.device_fingerprint = DeviceFingerprint::from_bytes([0xcc; 32]);
        view.insert([0x2a; 32], other);
        let rows = candidates(&view);

        let selection = select_accepted_branch(&rows, &all(&view), &[], &view);
        assert_eq!(
            selection.accepted,
            HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32], [0x2a; 32]]),
        );
        assert!(selection.forked.is_empty());
    }

    #[test]
    fn a_deep_chain_selects_without_a_stack_overflow() {
        // Chain depth is attacker-controlled: every walk here is iterative, and this is the test
        // that keeps it that way.
        let mut view = HashMap::new();
        let mut prev: Option<EntryHash> = None;
        for seq in 0..2000u64 {
            let mut entry_hash = [0u8; 32];
            entry_hash[..8].copy_from_slice(&seq.to_be_bytes());
            view.insert(entry_hash, header(seq, prev));
            prev = Some(entry_hash);
        }
        let head = prev.expect("2000 entries");
        let rows = candidates(&view);
        let pin = BranchPin { coordinate: chain(), seq: 1999, watermark: head };

        assert_eq!(ancestry(&[0u8; 32], &head, &view), AncestryRelation::OnBranch);
        assert_eq!(select_accepted_branch(&rows, &all(&view), &[pin], &view).accepted.len(), 2000);
    }
}
