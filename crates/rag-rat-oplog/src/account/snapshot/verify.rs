//! Read-time verification of a snapshot's coverage claim (C6b, #609).
//!
//! A manifest asserts "I folded exactly this prefix and got this state". Checking that requires
//! HOLDING the covered history and re-folding it, which is a device-dependent capability — so it
//! happens here, at read, and never at acceptance.
//!
//! # The invariant this module exists to protect
//!
//! **Acceptance may read only frozen structural facts of the entry plus the control projection. It
//! may never read the local candidate inventory of the logs a manifest covers.** A structurally
//! valid manifest whose watermarks exist nowhere on this device must still be STORED — it simply
//! never verifies here. The moment acceptance asks "do I hold the covered entries?", two peers with
//! different sync progress reach different verdicts on the same signed entry and the fold firewall
//! is gone. Everything in this module is therefore advisory: it decides whether a snapshot may be
//! TRUSTED, never whether it may be stored.
//!
//! That is the same line C4.3b's sealing-key cross-check sits on, for the same reason.
//!
//! # Why the input is the on-branch prefix
//!
//! The fold is a pure function of the candidate SET, and the candidate DAG deliberately retains
//! both sides of an equivocation. A watermark vector names one branch head per `(log, device)` and
//! says nothing about off-branch siblings, so "everything at or below seq N" would leave the input
//! under-determined: two honest devices holding different fork evidence would compute different
//! hashes for the same claim. The verification input is therefore exactly the chains reachable by
//! walking `prev_hash` from each named watermark — the same link discipline
//! [`super::super::candidate::ancestry`] enforces (same `(account, log, device)` coordinate,
//! stepping down exactly one seq slot).
//!
//! The counterpart to that restriction: an author could otherwise omit fork evidence and publish a
//! "clean" snapshot of an account whose full fold is contested. So a snapshot whose covered input
//! does not fold `Live` is not usable, however well its hash matches.

use std::collections::HashMap;

use super::super::envelope::VerifiedAccountEntry;
use super::super::fold::{self, AccountClassification};
use super::ops::{CoveredWatermark, SnapshotTarget};
use super::projection;
use crate::op::DeviceFingerprint;

/// The account control log — the only target this binary has a projection for. A manifest may name
/// the secrets or content logs (the wire allows it so #406 needs no bump), but nothing here can
/// check those claims yet.
const CONTROL_LOG_ID: u8 = 0;

/// Why a claim could not be checked. Never a judgement about the snapshot — always a statement
/// about what this device currently holds or implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) enum Unverifiable {
    /// A named watermark entry is not held here.
    WatermarkNotHeld,
    /// A link on the walk from a watermark toward seq 0 is missing, or the chain is malformed.
    IncompleteChain,
    /// Every target names a log this binary has no canonical projection for.
    UnsupportedTargets,
}

/// The result of checking one snapshot against local history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) enum SnapshotVerdict {
    /// Every supported target's hash matched a local re-fold of its covered prefix.
    Verified,
    /// A supported target's hash disagreed with the local re-fold: the claim is FALSE. The entry
    /// remains stored — it is simply never trusted.
    Mismatch,
    /// The account does not fold `Live` over everything this device holds. Refused even on a hash
    /// match: a contested account's snapshot would let peers that trust it proceed while full peers
    /// halt.
    NotLive,
    /// The device HOLDS an entry at a covered coordinate that the claim's chain does not include —
    /// an equivocation the author either did not see or chose not to fold. The claim may be
    /// perfectly true about the branch it names and still be an incomplete view of what is known
    /// here, so it is not trusted. This is the counterpart to restricting the input to one branch:
    /// without it, an author picks the branch that suits them and the `Live` check above inspects
    /// the attacker's own selection rather than reality.
    IgnoresHeldEvidence,
    /// Undecidable here and now.
    Unverifiable(Unverifiable),
}

/// Whether this binary can actually check a target's claim.
///
/// The single source of truth for "supported", and it has a second consumer for a sharp reason:
/// selection must rank snapshots by VERIFIED coverage only. An unsupported target is skipped here
/// without affecting the verdict, so if selection counted it, an author could pad a manifest with
/// fabricated secrets or content targets and outrank an honest control-only snapshot on coverage
/// nobody validated.
pub(in crate::account) fn is_supported_target(target: &SnapshotTarget) -> bool {
    target.log_id == CONTROL_LOG_ID
}

/// Check one snapshot's targets against the entries this device holds for the account.
///
/// A pure function of `(held, targets)` — deliberately not a storage call, so it is impossible to
/// reach from acceptance by accident and trivial to test against a fold fixture. Advisory only: a
/// `Mismatch` means "do not trust this", never "reject this entry".
pub(in crate::account) fn verify_snapshot(
    held: &[VerifiedAccountEntry],
    targets: &[SnapshotTarget],
) -> SnapshotVerdict {
    let by_hash: HashMap<[u8; 32], &VerifiedAccountEntry> =
        held.iter().map(|entry| (entry.entry_hash, entry)).collect();

    // Classification duty runs over EVERYTHING this device holds, never over the claimed branch
    // alone. Restricting the fold input to one branch is what makes the hash deterministic between
    // honest peers — but it also means a branch chosen by the author folds `Live` by construction,
    // so checking `Live` over that input would be checking the author's own selection.
    if fold::fold_account(held).classification() != AccountClassification::Live {
        return SnapshotVerdict::NotLive;
    }

    let mut supported = 0usize;
    for target in targets {
        if !is_supported_target(target) {
            // A target this binary has no projection for is not a failure — a newer binary (or
            // #406, for content) will check it.
            continue;
        }
        supported += 1;
        let prefix = match on_branch_prefix(&target.covered, &by_hash) {
            Ok(prefix) => prefix,
            Err(reason) => return SnapshotVerdict::Unverifiable(reason),
        };
        if ignores_held_evidence(&prefix, held) {
            return SnapshotVerdict::IgnoresHeldEvidence;
        }
        let history = fold::fold_account(&prefix);
        if projection::folded_state_hash(&history) != target.folded_state_hash {
            return SnapshotVerdict::Mismatch;
        }
    }
    if supported == 0 {
        return SnapshotVerdict::Unverifiable(Unverifiable::UnsupportedTargets);
    }
    SnapshotVerdict::Verified
}

/// Whether this device holds an entry at a coordinate the claim's chain covers, that the chain does
/// not include — i.e. an equivocation the author did not fold.
///
/// The claim can be entirely true about the branch it names and still be an incomplete view of what
/// is known here. Since the author chooses which branch to hash, this is the check that stops that
/// choice from being the only thing verification inspects.
/// Does `held` contain an entry at a covered coordinate that the claimed chain does not include?
///
/// Shared with authoring for the same reason as [`on_branch_prefix`]: an author that could not ask
/// this question would mint snapshots its own verifier deterministically refuses. One function, two
/// callers.
pub(in crate::account) fn ignores_held_evidence(
    prefix: &[VerifiedAccountEntry],
    held: &[VerifiedAccountEntry],
) -> bool {
    let covered_slots: HashMap<(u8, DeviceFingerprint, u64), [u8; 32]> = prefix
        .iter()
        .map(|entry| {
            let h = &entry.header;
            ((h.log_id, h.device_fingerprint, h.seq), entry.entry_hash)
        })
        .collect();
    held.iter().any(|entry| {
        let h = &entry.header;
        covered_slots
            .get(&(h.log_id, h.device_fingerprint, h.seq))
            .is_some_and(|chosen| *chosen != entry.entry_hash)
    })
}

/// Collect the union of the chains reachable by walking `prev_hash` from each covered watermark.
///
/// Shared with authoring ON PURPOSE. The author hashes a projection of this prefix and a verifier
/// re-derives it from the same watermark vector, so if the two ever computed "the covered prefix"
/// differently every honest snapshot would fail verification. One function, two callers.
///
/// The walk mirrors [`super::super::candidate::ancestry`]'s link rules exactly: every step stays on
/// the watermark's own `(log, device)` coordinate and steps down precisely one seq slot, ending at
/// seq 0 with no parent. A signed header pins only `prev_hash` NULLITY — not that its parent is a
/// valid contiguous link — so a forged chain must not be walkable into the verification input.
pub(in crate::account) fn on_branch_prefix(
    covered: &[CoveredWatermark],
    by_hash: &HashMap<[u8; 32], &VerifiedAccountEntry>,
) -> Result<Vec<VerifiedAccountEntry>, Unverifiable> {
    let mut collected: Vec<&VerifiedAccountEntry> = Vec::new();
    for watermark in covered {
        let mut cursor = Some(watermark.entry_hash);
        let mut expected_seq = watermark.seq;
        let mut at_head = true;
        while let Some(hash) = cursor {
            let Some(entry) = by_hash.get(&hash).copied() else {
                // A missing HEAD is "not synced yet"; a hole further down is a claim naming a chain
                // this device cannot reconstruct. Different diagnoses, both undecidable.
                return Err(if at_head {
                    Unverifiable::WatermarkNotHeld
                } else {
                    Unverifiable::IncompleteChain
                });
            };
            let header = &entry.header;
            if header.log_id != CONTROL_LOG_ID
                || header.device_fingerprint != watermark.device_fingerprint
                || header.seq != expected_seq
            {
                return Err(Unverifiable::IncompleteChain);
            }
            collected.push(entry);
            at_head = false;
            match (header.prev_hash, header.seq) {
                // Root of the chain: seq 0 with no parent is the only valid terminus.
                (None, 0) => cursor = None,
                (None, _) | (Some(_), 0) => return Err(Unverifiable::IncompleteChain),
                (Some(prev), seq) => {
                    cursor = Some(prev);
                    expected_seq = seq - 1;
                },
            }
        }
    }
    collected.sort_unstable_by_key(|entry| entry.entry_hash);
    collected.dedup_by_key(|entry| entry.entry_hash);
    Ok(collected.into_iter().cloned().collect())
}
