//! Pin-aware branch selection for the secrets log (`log_id = 1`, §16.2, C4.2b, B-1).
//!
//! Uses the pin-aware selection shared with content ([`super::super::branch`]). A register pin
//! promotes its watermark's branch over hash order; see that module for why the control log uses
//! a different selection rule.
//!
//! Pins are sourced from BOTH secrets boundaries of a wrap's cited owner incarnation (the device
//! register AND the owner-incarnation register — two registers can bound one chain), revalidated
//! here via [`super::super::candidate::validate_cut_target`] at the secrets coordinate.

use std::collections::HashSet;

use super::super::AccountId;
use super::super::branch::{self, BranchSelection, Candidate, ChainLink};
use super::super::candidate::{self, CutCoordinate, HeaderView};
use super::super::cut::Cut;
use super::super::envelope::AccountEntryHeader;
use super::super::fold::SECRETS_LOG;
use crate::op::DeviceFingerprint;

type AccountEntryHash = [u8; 32];

/// The chain one secrets cut bounds: `(account, device)` on `log: SECRETS_LOG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::account) struct SecretsCoordinate {
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
pub(super) type SecretsCandidate = Candidate<AccountEntryHeader>;

/// A register watermark that pins one secrets chain's accepted branch (§16.2). Sourced from a
/// wrap's cited owner-incarnation secrets boundaries; a cut naming a currently-`forked` branch
/// PROMOTES it, which is what makes an off-branch condemnation of the other fork enforceable.
pub(super) type BranchPin = branch::BranchPin<SecretsCoordinate>;

/// An account header seen as a link on its SECRETS chain — this module only ever walks log-1
/// candidates, whose chain is `(account, device)`.
impl ChainLink for AccountEntryHeader {
    type Coordinate = SecretsCoordinate;

    fn coordinate(&self) -> SecretsCoordinate {
        SecretsCoordinate::of(self)
    }

    fn seq(&self) -> u64 {
        self.seq
    }

    fn prev_hash(&self) -> Option<AccountEntryHash> {
        self.prev_hash
    }
}

/// Select one contiguous accepted chain per `(account, device)` on the secrets log from the
/// eligible candidates — [`branch::select_accepted_branch`] with secrets pin admission.
/// `eligible` is the caller's authority verdict + the slot-eligible non-evaluable entries.
pub(super) fn select_accepted_branch(
    candidates: &[SecretsCandidate],
    eligible: &HashSet<AccountEntryHash>,
    pins: &[BranchPin],
    view: &dyn HeaderView,
) -> BranchSelection {
    branch::select_accepted_branch(candidates, eligible, |chain| {
        let coordinate = chain.cut_coordinate();
        branch::pinned_branch(
            chain,
            pins,
            |pin| {
                candidate::validate_cut_target(
                    &Cut::At { seq: pin.seq, hash: pin.watermark },
                    &coordinate,
                    view,
                ) == candidate::CutBinding::Ok
            },
            |hash| view.header(hash),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const ACCOUNT: [u8; 32] = [0xaa; 32];
    const DEVICE: [u8; 32] = [0xbb; 32];

    fn chain() -> SecretsCoordinate {
        SecretsCoordinate {
            account_id: AccountId::from_bytes(ACCOUNT),
            device_fingerprint: DeviceFingerprint::from_bytes(DEVICE),
        }
    }

    fn header(seq: u64, prev_hash: Option<AccountEntryHash>) -> AccountEntryHeader {
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

    fn linear() -> HashMap<AccountEntryHash, AccountEntryHeader> {
        HashMap::from([
            ([0x0a; 32], header(0, None)),
            ([0x0b; 32], header(1, Some([0x0a; 32]))),
            ([0x0c; 32], header(2, Some([0x0b; 32]))),
        ])
    }

    fn candidates(view: &HashMap<AccountEntryHash, AccountEntryHeader>) -> Vec<SecretsCandidate> {
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

    fn all(view: &HashMap<AccountEntryHash, AccountEntryHeader>) -> HashSet<AccountEntryHash> {
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
