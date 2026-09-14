//! Pin-aware branch selection over a dense, hash-linked candidate chain (§16.2) — the one rule the
//! `/3` content refold ([`super::content`]) and the secrets log refold ([`super::secrets`]) both
//! apply. Each log supplies only what differs: its chain coordinate ([`ChainLink`]) and how a pin's
//! watermark is admitted against that coordinate.
//!
//! A divergence between two copies of this rule would be a convergence bug, so it lives once: the
//! slot walk, the pinned-over-min-hash tie-break, the rooted-minus-accepted fork rule, and the
//! backward walk that re-derives a real predecessor from a signed `prev_hash`.

//! Control-log selection in [`super::storage`] instead consumes post-fold effective entries:
//! registers have already condemned off-branch authority, so it needs no watermark pins. Its
//! forked set is effective-relative, including entries stranded above gaps; that projection is
//! rebuilt on every read, allowing a late predecessor to heal the stranded entry. Content and
//! secrets selection here leave unrooted entries undecided and use pins to preserve the branch
//! named by a revocation watermark, even if an attacker produces a smaller-hash fork below it.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::ControlFlow;

use super::fold::AuthorityFreshness;
use super::id::AccountId;

/// Why an ancestry walk against a cut watermark could not be decided (a withheld watermark parks,
/// and never flips a verdict — I11).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownAncestry {
    /// The cut's watermark entry itself is not held.
    UnknownCutTarget,
    /// A link on the walk from the watermark toward the entry is missing.
    IncompleteCutAncestry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AncestryRelation {
    /// The entry is the watermark itself or an ancestor reached walking backward from it.
    OnBranch,
    OffBranch,
    Unknown(UnknownAncestry),
}

/// One freshness observation, bound to the exact query it answers. The pair (account, asserted
/// length) is carried so a result computed for the owner cannot be read as the author's, and a
/// result computed for a shorter assertion cannot stand in for the header's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CitedFreshness {
    pub account_id: AccountId,
    pub asserted_auth_len: u64,
    pub state: AuthorityFreshness,
}

type AccountEntryHash = [u8; 32];

/// The header fields the branch walks read: which dense chain an entry extends, its slot on that
/// chain, and the predecessor it names.
pub(in crate::account) trait ChainLink {
    type Coordinate: Copy + Eq + Hash;

    fn coordinate(&self) -> Self::Coordinate;
    fn seq(&self) -> u64;
    fn prev_hash(&self) -> Option<AccountEntryHash>;
}

/// One candidate the refold classifies: its hash plus the header the walks read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::account) struct Candidate<H> {
    pub(in crate::account) entry_hash: AccountEntryHash,
    pub(in crate::account) header: H,
}

/// A register watermark that pins one chain's accepted branch (§16.2). Sourced from the account
/// log's revocation cuts; a cut naming a currently-`forked` branch PROMOTES it on the next refold,
/// which is what makes an off-branch condemnation enforceable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) struct BranchPin<C> {
    pub(in crate::account) coordinate: C,
    pub(in crate::account) seq: u64,
    pub(in crate::account) watermark: AccountEntryHash,
}

/// The branch-selection verdict for one refold.
///
/// The two sets do NOT partition the eligible candidates, and that is the point. An entry is
/// `forked` only if it reaches its chain root through held entries and still lost a slot — a real
/// equivocation loser, terminal unless a later watermark selects it. An entry stranded above a gap
/// in the dense chain (its predecessor has not arrived) is in NEITHER set: it lost nothing, and the
/// arrival of the missing predecessor can make it contiguous. Calling that `forked` would discard
/// valid work for being late; the acceptance predicate parks it as `missing_predecessor` instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::account) struct BranchSelection {
    pub(in crate::account) accepted: HashSet<AccountEntryHash>,
    pub(in crate::account) forked: HashSet<AccountEntryHash>,
}

/// Eligible entries indexed by the `(chain, prev_hash)` parent slot they extend; a chain root keys
/// on `None`. Several entries under one key are an equivocation — the slot selection resolves them.
type BranchChildren<C> = HashMap<(C, Option<AccountEntryHash>), Vec<(u64, AccountEntryHash)>>;

/// Select one contiguous accepted chain per coordinate from the eligible candidates (§16.2).
///
/// `eligible` is the caller's authority verdict — the candidates no register has condemned and no
/// citation has rejected. A condemned entry must not compete for a slot, or an attacker could mine
/// a small-hash entry beyond a cut and fork an honest sibling out of the accepted branch.
///
/// Selection walks the dense seq slots from 0, extending only from the entry that won the previous
/// slot, so the accepted set is always one hash-linked chain. At a slot with several eligible
/// children the winner is:
/// 1. the child on the chain's pinned branch (`pinned_branch`, see [`pinned_branch`]), if a
///    register pins this chain — a cut names the branch it bounds, so the register decides, not the
///    hash order; otherwise
/// 2. the lexicographically smallest `entry_hash` — an unforced fork resolved by a rule both peers
///    compute identically.
pub(in crate::account) fn select_accepted_branch<H: ChainLink>(
    candidates: &[Candidate<H>],
    eligible: &HashSet<AccountEntryHash>,
    pinned_branch: impl Fn(H::Coordinate) -> HashSet<AccountEntryHash>,
) -> BranchSelection {
    let mut children = BranchChildren::new();
    let mut chains: HashSet<H::Coordinate> = HashSet::new();
    for candidate in candidates.iter().filter(|c| eligible.contains(&c.entry_hash)) {
        let coordinate = candidate.header.coordinate();
        children
            .entry((coordinate, candidate.header.prev_hash()))
            .or_default()
            .push((candidate.header.seq(), candidate.entry_hash));
        chains.insert(coordinate);
    }

    let mut accepted = HashSet::new();
    let mut rooted = HashSet::new();
    for chain in chains {
        let pinned = pinned_branch(chain);
        let mut parent: Option<AccountEntryHash> = None;
        // A seq is dense from 0, so the chain ends at the first slot no eligible child fills.
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
        collect_rooted(chain, &children, &mut rooted);
    }

    // Only an entry that reaches its chain root through held entries can have LOST anything: it had
    // a slot to contest. One stranded above a gap never entered a contest, so it is not a loser —
    // leave it out of both sets and let the caller park it until its predecessor arrives.
    let forked = rooted.difference(&accepted).copied().collect();
    BranchSelection { accepted, forked }
}

/// Every eligible entry reachable from a chain root by contiguous `prev_hash` links — the entries
/// that are dense-complete back to seq 0, and so are decidable at all.
fn collect_rooted<C: Copy + Eq + Hash>(
    chain: C,
    children: &BranchChildren<C>,
    rooted: &mut HashSet<AccountEntryHash>,
) {
    // Breadth-first from the roots (`prev_hash` null at seq 0), stepping exactly one slot per link,
    // so a gap in the chain simply strands everything above it.
    let mut frontier: Vec<(Option<AccountEntryHash>, u64)> = vec![(None, 0)];
    while let Some((parent, slot)) = frontier.pop() {
        let Some(kids) = children.get(&(chain, parent)) else {
            continue;
        };
        for (seq, hash) in kids.iter().filter(|(seq, _)| *seq == slot) {
            if !rooted.insert(*hash) {
                continue;
            }
            // A dense chain cannot reach past `u64::MAX`; there is no next slot to walk to.
            if let Some(next) = seq.checked_add(1) {
                frontier.push((Some(*hash), next));
            }
        }
    }
}

/// The entries on the branch a register pins for `chain` — empty when no pin `admits`, i.e. when
/// the pinning watermark is withheld or names a foreign coordinate (neither may steer selection).
///
/// The HIGHEST admitted watermark wins: a register only ever extends forward, so the deepest cut is
/// the most recent statement about which branch is real.
///
/// TWO registers can bound one chain, so once a device equivocates they can name different
/// watermarks at the SAME seq. Order the pins by `(seq, watermark)`, a total order, so the winner
/// never depends on the order the registers were assembled in: `max_by_key` on `seq` alone hands a
/// tie to whichever pin happens to come last, and two peers holding identical entries would derive
/// different accepted branches from it.
pub(in crate::account) fn pinned_branch<'v, H: ChainLink + 'v>(
    chain: H::Coordinate,
    pins: &[BranchPin<H::Coordinate>],
    admits: impl Fn(&BranchPin<H::Coordinate>) -> bool,
    lookup: impl Fn(&AccountEntryHash) -> Option<&'v H>,
) -> HashSet<AccountEntryHash> {
    let Some(pin) = pins
        .iter()
        .filter(|pin| pin.coordinate == chain)
        .filter(|pin| admits(pin))
        .max_by_key(|pin| (pin.seq, pin.watermark))
    else {
        return HashSet::new();
    };
    let mut branch = HashSet::from([pin.watermark]);
    walk_back(&pin.watermark, lookup, |hash, _| {
        branch.insert(*hash);
        ControlFlow::Continue(())
    });
    branch
}

/// Why a backward walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) enum WalkEnd {
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
/// so the walk re-derives that: every link must stay on the watermark's coordinate and step down
/// exactly one seq slot. A link that jumps coordinate or skips slots is forged, and the walk
/// refuses to follow it.
///
/// Iterative with a visited guard: chain depth is attacker-controlled, and a hash cycle would need
/// a sha256 collision but a corrupt row must not spin forever either.
pub(in crate::account) fn walk_back<'v, H: ChainLink + 'v>(
    watermark: &AccountEntryHash,
    lookup: impl Fn(&AccountEntryHash) -> Option<&'v H>,
    mut visit: impl FnMut(&AccountEntryHash, &H) -> ControlFlow<()>,
) -> WalkEnd {
    let Some(head) = lookup(watermark) else {
        return WalkEnd::MissingLink;
    };
    let chain = head.coordinate();
    let mut visited: HashSet<AccountEntryHash> = HashSet::new();
    let mut current = *watermark;
    loop {
        let Some(header) = lookup(&current) else {
            return WalkEnd::MissingLink;
        };
        // Validate the node is on the bounded chain BEFORE the visitor counts it: a forged link
        // straight to a foreign entry is not a real predecessor, so it is not on this branch.
        if header.coordinate() != chain {
            return WalkEnd::ForgedLink;
        }
        if visit(&current, header).is_break() {
            return WalkEnd::Stopped;
        }
        if !visited.insert(current) {
            return WalkEnd::ForgedLink;
        }
        let Some(prev) = header.prev_hash() else {
            return WalkEnd::Origin;
        };
        // A dense chain is contiguous, so a held predecessor MUST be the exactly-preceding slot; a
        // link that skips slots (5 → 3) is forged, not a real parent. `checked_add` because `seq`
        // is a peer-supplied `u64`: a candidate parked at `u64::MAX` is reachable by any peer, and
        // `+ 1` would panic in a debug build rather than reject the link it is meant to reject.
        if let Some(prev_header) = lookup(&prev)
            && prev_header.seq().checked_add(1) != Some(header.seq())
        {
            return WalkEnd::ForgedLink;
        }
        current = prev;
    }
}
