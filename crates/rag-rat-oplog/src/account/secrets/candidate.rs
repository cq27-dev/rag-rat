//! Pin-aware branch selection for the secrets log (`log_id = 1`, §16.2, C4.2b, B-1).
//!
//! The account-side [`super::super::candidate`] primitives (`ancestry`, `validate_cut_target`,
//! `HeaderView`) are log-generic, but `select_coherent_branches` in [`super::super::storage`] has
//! NO watermark-pin mechanism — the control fold never needs one. The secrets acceptance loop DOES:
//! a compromised-then-removed owner device can fork its secrets chain BELOW its `DeviceRemove`
//! secrets cut, and a pin-less min-hash tiebreak would select the attacker fork (deterministic,
//! convergent, WRONG), forking the honest cut-preserved wraps off the accepted branch. So this
//! module ports the CONTENT selection's pin logic (`content::candidate`) onto `AccountEntryHeader`
//! rows: a register pin promotes the branch its watermark names over the hash order, which is what
//! makes the off-branch condemnation of the other fork enforceable.
//!
//! Pins are sourced from BOTH secrets boundaries of a wrap's cited owner incarnation (the device
//! register AND the owner-incarnation register — two registers can bound one chain), revalidated
//! here via [`super::super::candidate::validate_cut_target`] at the secrets coordinate.

use std::collections::{HashMap, HashSet};

use super::super::AccountId;
use super::super::candidate::{self, CutCoordinate, HeaderView};
use super::super::cut::Cut;
use super::super::envelope::AccountEntryHeader;
use super::super::fold::SECRETS_LOG;
use crate::op::DeviceFingerprint;

type EntryHash = [u8; 32];

/// The chain one secrets cut bounds: `(account, device)` on `log: SECRETS_LOG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct SecretsCoordinate {
    pub(super) account_id: AccountId,
    pub(super) device_fingerprint: DeviceFingerprint,
}

impl SecretsCoordinate {
    fn of(header: &AccountEntryHeader) -> Self {
        Self { account_id: header.account_id, device_fingerprint: header.device_fingerprint }
    }

    fn cut_coordinate(&self) -> CutCoordinate {
        CutCoordinate {
            account: self.account_id,
            log: SECRETS_LOG,
            device: self.device_fingerprint,
        }
    }
}

/// One log-1 candidate the refold classifies: its hash plus the header the walks read.
#[derive(Debug, Clone)]
pub(super) struct SecretsCandidate {
    pub(super) entry_hash: EntryHash,
    pub(super) header: AccountEntryHeader,
}

/// A register watermark that pins one secrets chain's accepted branch (§16.2). Sourced from a
/// wrap's cited owner-incarnation secrets boundaries; a cut naming a currently-`forked` branch
/// PROMOTES it, which is what makes an off-branch condemnation of the other fork enforceable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BranchPin {
    pub(super) coordinate: SecretsCoordinate,
    pub(super) seq: u64,
    pub(super) watermark: EntryHash,
}

/// Eligible entries indexed by the `(chain, prev_hash)` parent slot they extend; a chain root keys
/// on `None`. Several entries under one key are an equivocation — the slot selection resolves them.
type BranchChildren = HashMap<(SecretsCoordinate, Option<EntryHash>), Vec<(u64, EntryHash)>>;

/// The branch-selection verdict for one refold. The two sets do NOT partition the eligible
/// candidates: an entry stranded above a gap in the dense chain is in NEITHER (it lost nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct BranchSelection {
    pub(super) accepted: HashSet<EntryHash>,
    pub(super) forked: HashSet<EntryHash>,
}

/// Select one contiguous accepted chain per `(account, device)` on the secrets log from the
/// eligible candidates (§16.2). `eligible` is the caller's authority verdict + the slot-eligible
/// non-evaluable entries; a condemned/rejected entry must not compete for a slot. At a slot with
/// several eligible children the winner is (1) the child on a pinning watermark's branch, if a
/// register pins this chain — the register decides, not hash order — else (2) the lexicographically
/// smallest `entry_hash`.
pub(super) fn select_accepted_branch(
    candidates: &[SecretsCandidate],
    eligible: &HashSet<EntryHash>,
    pins: &[BranchPin],
    view: &dyn HeaderView,
) -> BranchSelection {
    let mut children = BranchChildren::new();
    let mut chains: HashSet<SecretsCoordinate> = HashSet::new();
    for candidate in candidates.iter().filter(|c| eligible.contains(&c.entry_hash)) {
        let coordinate = SecretsCoordinate::of(&candidate.header);
        children
            .entry((coordinate, candidate.header.prev_hash))
            .or_default()
            .push((candidate.header.seq, candidate.entry_hash));
        chains.insert(coordinate);
    }

    let mut accepted = HashSet::new();
    let mut rooted = HashSet::new();
    for chain in chains {
        let pinned = pinned_branch(chain, pins, view);
        let mut parent: Option<EntryHash> = None;
        // A secrets seq is dense from 0, so the chain ends at the first slot no eligible child
        // fills.
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

    // Only an entry that reaches its chain root through held entries can have LOST anything. One
    // stranded above a gap never entered a contest, so it is not a loser — leave it out of both
    // sets and let the caller park it until its predecessor arrives.
    let forked = rooted.difference(&accepted).copied().collect();
    BranchSelection { accepted, forked }
}

/// Every eligible entry reachable from a chain root by contiguous `prev_hash` links.
fn collect_rooted(
    chain: SecretsCoordinate,
    children: &BranchChildren,
    rooted: &mut HashSet<EntryHash>,
) {
    let mut frontier: Vec<(Option<EntryHash>, u64)> = vec![(None, 0)];
    while let Some((parent, slot)) = frontier.pop() {
        let Some(kids) = children.get(&(chain, parent)) else {
            continue;
        };
        for (seq, hash) in kids.iter().filter(|(seq, _)| *seq == slot) {
            if !rooted.insert(*hash) {
                continue;
            }
            if let Some(next) = seq.checked_add(1) {
                frontier.push((Some(*hash), next));
            }
        }
    }
}

/// The entries on the branch a register pins for `chain` — empty when no cut pins it, or when the
/// pinning watermark is withheld or names a foreign coordinate (neither may steer selection). The
/// HIGHEST admitted watermark wins, ordered by `(seq, watermark)` (a total order) so the winner
/// never depends on the order the pins were assembled in — two registers can name the SAME seq once
/// a device equivocates.
fn pinned_branch(
    chain: SecretsCoordinate,
    pins: &[BranchPin],
    view: &dyn HeaderView,
) -> HashSet<EntryHash> {
    let coordinate = chain.cut_coordinate();
    let Some(pin) = pins
        .iter()
        .filter(|pin| pin.coordinate == chain)
        .filter(|pin| {
            candidate::validate_cut_target(
                &Cut::At { seq: pin.seq, hash: pin.watermark },
                &coordinate,
                view,
            ) == candidate::CutBinding::Ok
        })
        .max_by_key(|pin| (pin.seq, pin.watermark))
    else {
        return HashSet::new();
    };
    // Walk `prev_hash` back from the watermark, collecting the branch it heads. A withheld/forged
    // link simply ends the walk — validate_cut_target already confirmed the watermark is held and
    // names this coordinate.
    let mut branch = HashSet::from([pin.watermark]);
    let mut visited: HashSet<EntryHash> = HashSet::new();
    let mut current = pin.watermark;
    while let Some(header) = view.header(&current) {
        if SecretsCoordinate::of(header) != chain {
            break; // a forged cross-coordinate link is not a real predecessor
        }
        branch.insert(current);
        if !visited.insert(current) {
            break;
        }
        let Some(prev) = header.prev_hash else {
            break; // reached the chain origin
        };
        // A dense chain is contiguous — a held predecessor must be the exactly-preceding slot.
        if let Some(prev_header) = view.header(&prev)
            && prev_header.seq.checked_add(1) != Some(header.seq)
        {
            break;
        }
        current = prev;
    }
    branch
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: [u8; 32] = [0xaa; 32];
    const DEVICE: [u8; 32] = [0xbb; 32];

    fn chain() -> SecretsCoordinate {
        SecretsCoordinate {
            account_id: AccountId::from_bytes(ACCOUNT),
            device_fingerprint: DeviceFingerprint::from_bytes(DEVICE),
        }
    }

    fn header(seq: u64, prev_hash: Option<EntryHash>) -> AccountEntryHeader {
        AccountEntryHeader {
            account_id: AccountId::from_bytes(ACCOUNT),
            log_id: SECRETS_LOG,
            device_fingerprint: DeviceFingerprint::from_bytes(DEVICE),
            seq,
            prev_hash,
            parent_ref: None,
            entry_type: 0,
            op_version: 1,
            crypto_suite: 0,
            auth_len: seq,
            key_id: None,
            authority_ref: Some([1; 32]),
        }
    }

    fn linear() -> HashMap<EntryHash, AccountEntryHeader> {
        HashMap::from([
            ([0x0a; 32], header(0, None)),
            ([0x0b; 32], header(1, Some([0x0a; 32]))),
            ([0x0c; 32], header(2, Some([0x0b; 32]))),
        ])
    }

    fn candidates(view: &HashMap<EntryHash, AccountEntryHeader>) -> Vec<SecretsCandidate> {
        let mut rows: Vec<SecretsCandidate> = view
            .iter()
            .map(|(entry_hash, header)| SecretsCandidate {
                entry_hash: *entry_hash,
                header: header.clone(),
            })
            .collect();
        rows.sort_by_key(|row| row.entry_hash);
        rows
    }

    fn all(view: &HashMap<EntryHash, AccountEntryHeader>) -> HashSet<EntryHash> {
        view.keys().copied().collect()
    }

    #[test]
    fn a_linear_chain_is_accepted_in_full() {
        let view = linear();
        let rows = candidates(&view);
        let selection = select_accepted_branch(&rows, &all(&view), &[], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]));
        assert!(selection.forked.is_empty());
    }

    #[test]
    fn an_unforced_fork_resolves_to_the_smaller_hash_and_the_loser_is_terminal() {
        let mut view = linear();
        view.insert([0x1b; 32], header(1, Some([0x0a; 32]))); // sibling of 0x0b, larger hash
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let rows = candidates(&view);
        let selection = select_accepted_branch(&rows, &all(&view), &[], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]));
        assert_eq!(selection.forked, HashSet::from([[0x1b; 32], [0x1c; 32]]));
    }

    #[test]
    fn a_register_watermark_promotes_the_branch_it_names_over_the_hash_order() {
        let mut view = linear();
        // The equivocating sibling has the LARGER hash, so the unforced rule would fork it out.
        view.insert([0x1b; 32], header(1, Some([0x0a; 32])));
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let rows = candidates(&view);
        let pin = BranchPin { coordinate: chain(), seq: 2, watermark: [0x1c; 32] };
        let selection = select_accepted_branch(&rows, &all(&view), &[pin], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x1b; 32], [0x1c; 32]]));
        assert_eq!(selection.forked, HashSet::from([[0x0b; 32], [0x0c; 32]]));
    }

    #[test]
    fn a_withheld_or_foreign_pin_cannot_steer_selection() {
        let mut view = linear();
        view.insert([0x1b; 32], header(1, Some([0x0a; 32])));
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let rows = candidates(&view);
        // A watermark we do not hold cannot pin.
        let withheld = BranchPin { coordinate: chain(), seq: 2, watermark: [0x99; 32] };
        let selection = select_accepted_branch(&rows, &all(&view), &[withheld], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]));
        // Nor a pin whose watermark names a foreign coordinate.
        let foreign = BranchPin {
            coordinate: SecretsCoordinate {
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
    fn a_condemned_entry_never_competes_for_a_slot() {
        let mut view = linear();
        // A smaller-hash equivocating sibling that WOULD win the tiebreak, but is ineligible.
        view.insert([0x00; 32], header(1, Some([0x0a; 32])));
        let rows = candidates(&view);
        let mut eligible = all(&view);
        eligible.remove(&[0x00; 32]);
        let selection = select_accepted_branch(&rows, &eligible, &[], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]));
    }

    #[test]
    fn selection_is_independent_of_row_order() {
        let mut view = linear();
        view.insert([0x1b; 32], header(1, Some([0x0a; 32])));
        view.insert([0x1c; 32], header(2, Some([0x1b; 32])));
        let mut rows = candidates(&view);
        let expected = select_accepted_branch(&rows, &all(&view), &[], &view);
        for rotation in 1..rows.len() {
            rows.rotate_left(rotation);
            assert_eq!(select_accepted_branch(&rows, &all(&view), &[], &view), expected);
        }
    }

    #[test]
    fn an_entry_stranded_above_a_gap_is_neither_accepted_nor_forked() {
        let mut view = linear();
        view.insert([0x0e; 32], header(4, Some([0x0d; 32]))); // seq-3 predecessor absent
        let rows = candidates(&view);
        let selection = select_accepted_branch(&rows, &all(&view), &[], &view);
        assert_eq!(selection.accepted, HashSet::from([[0x0a; 32], [0x0b; 32], [0x0c; 32]]));
        assert!(!selection.forked.contains(&[0x0e; 32]));
    }
}
