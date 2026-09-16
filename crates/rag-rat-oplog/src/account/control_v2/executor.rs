//! Bounded execution of one owner-authorized control v2 operation against a verified checkpoint.
//!
//! The evidence for a revocation is a DETACHED manifest the operation names by digest. Verification
//! requires every entry that manifest names: its signed bytes, a key the accepted legacy epoch
//! certifies, and an unbroken chain back to a branch the checkpoint accepted. Nothing is counted on
//! a peer's word — there is no entry count without the signed entry behind it.
//!
//! Incomplete evidence PARKS. A withheld manifest, a withheld chain head, or a gap in the walk back
//! to the legacy branch leaves the operation unapplied in full: no register, no credit, and no
//! partial effect that a later arrival would have to unwind.
//!
//! Malformed input is an `Err`, never a verdict. A peer that attaches one unverifiable object to an
//! otherwise sound operation gets its bundle refused; it does not get the operation itself
//! permanently condemned, which is what a `Rejected` means.
//!
//! Every bound is the planner's declared evidence budget ([`views::MAX_ENTRIES`],
//! [`views::MAX_BYTES`]), counted over the deduplicated objects this call was handed. Verification
//! is performed fresh here on every call; no cache stands in for it.
//!
//! This engine is not wired to production. The v1 fold dispatches nothing here, no CLI activates
//! it, and executing an operation flips no readiness or pin state.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::super::checkpoint::VerifiedCheckpoint;
use super::super::cut::Cut;
use super::super::envelope::{self, AccountEntryHeader, SignedAccountEntry, VerifiedAccountEntry};
use super::super::fold::{self, Candidate};
use super::super::id::AccountEntryHash;
use super::super::ops::{AccountOp, DeviceRole};
use super::super::registers::RegisterKey;
use super::{ops, views};
use crate::device::DevicePublic;
use crate::op::DeviceFingerprint;

/// What executing one v2 operation against the checkpoint decided.
pub(in crate::account) enum Verdict {
    /// Authorized: the registers it installs and the credit its signed nomination earns.
    Applied { registers: Vec<(RegisterKey, Cut)>, credit: u64 },
    /// Evidence is incomplete. NOTHING is applied — no register, no credit — and the operation is
    /// reconsidered when the missing objects arrive.
    Parked(ParkCause),
    /// Never admissible under this checkpoint, whatever else arrives.
    Rejected(RejectCause),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) enum ParkCause {
    /// A cited pre-cut view manifest was not supplied.
    Manifest,
    /// A manifest names an entry whose signed bytes were not supplied.
    Evidence,
    /// The chain slot an entry continues from is not held.
    ChainHead,
    /// A link between an entry and the accepted legacy branch is not held.
    Ancestry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) enum RejectCause {
    /// No key the accepted legacy epoch certifies names the author of some supplied entry. This is
    /// about the absence of a certifying key, never about bytes that fail to verify under one.
    Unauthenticated,
    /// The operation cites no live owner incarnation minted for its own signer.
    Inadmissible,
    /// The frozen legacy registers already condemn the operation's own chain slot.
    Condemned,
}

/// Execute `operation` against `checkpoint`. `Err` is malformed or over-budget input; an honest
/// peer that is merely behind gets a [`Verdict::Parked`].
pub(in crate::account) fn execute(
    checkpoint: &VerifiedCheckpoint,
    operation: &[u8],
    manifests: &[Vec<u8>],
    evidence: &[Vec<u8>],
) -> anyhow::Result<Verdict> {
    let plan = match views::plan_replay(checkpoint, operation, manifests, evidence) {
        Ok(plan) => plan,
        Err(views::PlanError::MissingView(_)) => return Ok(Verdict::Parked(ParkCause::Manifest)),
        Err(views::PlanError::MissingEntry(_)) => return Ok(Verdict::Parked(ParkCause::Evidence)),
        Err(views::PlanError::Invalid(error)) => return Err(error),
    };
    let frozen = checkpoint.frozen_legacy();
    let authenticated = match authenticate(&plan, frozen) {
        Ok(authenticated) => authenticated,
        // Bytes that do not verify are a bad bundle, not a bad operation.
        Err(AuthFailure::Malformed(error)) => return Err(error),
        Err(AuthFailure::UnknownSigner) =>
            return Ok(Verdict::Rejected(RejectCause::Unauthenticated)),
    };

    // Ancestry before authority: an operation whose branch we cannot yet walk is behind, not wrong.
    // The consumer is walked first, then the rest in hash order, so which gap a caller is told
    // about is a property of the evidence and not of map iteration order.
    let mut reached = HashSet::new();
    let walk = std::iter::once(&authenticated.consumer).chain(authenticated.entries.values());
    for entry in walk {
        let header = &entry.verified.header;
        match chain_reaches_accepted_legacy(header, &authenticated.headers, frozen, &mut reached) {
            Ok(()) => {},
            Err(WalkError::Incomplete(cause)) => return Ok(Verdict::Parked(cause)),
            Err(WalkError::OverBudget) =>
                anyhow::bail!("control chain ancestry exceeds the declared evidence budget"),
        }
    }

    let consumer = &authenticated.consumer;
    let header = &consumer.verified.header;
    let Some(incarnation) = header.authority_ref else {
        return Ok(Verdict::Rejected(RejectCause::Inadmissible));
    };
    let live = frozen.owner_is_live(incarnation, header.device_fingerprint)
        || authenticated
            .entries
            .get(&incarnation.into())
            .is_some_and(|mint| mints_owner_for(mint, header.device_fingerprint));
    if !live {
        return Ok(Verdict::Rejected(RejectCause::Inadmissible));
    }
    let cut = consumer.candidate();
    if frozen.condemns(&cut) {
        return Ok(Verdict::Rejected(RejectCause::Condemned));
    }

    // Only the identities the operation's own manifest named; an ordinary operation names none.
    // Each one's own authority is judged inside the execution, never assumed from having been
    // named here.
    let nominated: Vec<Candidate> = plan
        .root()
        .into_iter()
        .flat_map(|view| view.entries.iter())
        .filter_map(|hash| authenticated.entries.get(hash))
        .map(V2Entry::candidate)
        .collect();
    let applied =
        fold::v2::apply_cut(fold::v2::CutExecution { frozen, nominated: &nominated, cut: &cut });
    Ok(Verdict::Applied { registers: applied.registers, credit: applied.credit })
}

/// One authenticated v2 entry: the verified envelope and the inner v1 operation it carries.
struct V2Entry {
    verified: VerifiedAccountEntry,
    op: AccountOp,
}

impl V2Entry {
    fn candidate(&self) -> Candidate {
        Candidate::new(self.verified.clone(), self.op.clone())
    }
}

struct Authenticated {
    consumer: V2Entry,
    /// Ordered by hash so every derived verdict is arrival-independent.
    entries: BTreeMap<AccountEntryHash, V2Entry>,
    /// The frozen legacy headers plus every authenticated v2 header — the ancestry walk's view.
    headers: HashMap<AccountEntryHash, AccountEntryHeader>,
}

/// Why authentication could not complete. The two cases carry different consequences, so they are
/// never collapsed: absent certification is a property of the account, unverifiable bytes are a
/// property of the bundle.
enum AuthFailure {
    Malformed(anyhow::Error),
    UnknownSigner,
}

/// Authenticate every supplied v2 entry under a key the ACCEPTED legacy epoch certifies, or one an
/// already-authenticated v2 enrollment introduces. An entry may introduce a key, but only once it
/// has itself authenticated: pooling unverified introductions would admit a mutually-introducing
/// cycle that a fresh receiver can never reproduce.
fn authenticate(
    plan: &views::ReplayPlan,
    frozen: &fold::v2::FrozenLegacy,
) -> Result<Authenticated, AuthFailure> {
    let mut keys = frozen.device_pubkeys();
    let mut headers: HashMap<AccountEntryHash, AccountEntryHeader> =
        frozen.entries().iter().map(|entry| (entry.entry_hash, entry.header.clone())).collect();
    let mut entries: BTreeMap<AccountEntryHash, V2Entry> = BTreeMap::new();
    let mut consumer = None;
    let mut pending: Vec<&SignedAccountEntry> =
        plan.candidates().chain(std::iter::once(plan.consumer())).collect();
    while !pending.is_empty() {
        let before = pending.len();
        let mut remaining = Vec::new();
        for signed in pending {
            let Some(key) = keys.get(&signed.header.device_fingerprint).copied() else {
                remaining.push(signed);
                continue;
            };
            // A key exists for this author, so the bytes now have to verify under it.
            let verified = DevicePublic::from_bytes(&key)
                .and_then(|key| envelope::verify_account_signed(&signed.signed_bytes, &key))
                .map_err(AuthFailure::Malformed)?;
            let op = ops::decode(verified.header.entry_type, &verified.payload)
                .map_err(AuthFailure::Malformed)?
                .op;
            // A v2 enrollment certifies the added device's key exactly as its v1 counterpart does.
            if let AccountOp::DeviceAdd { device_fingerprint, ed25519_pubkey, .. } = &op {
                keys.insert(*device_fingerprint, *ed25519_pubkey);
            }
            headers.insert(verified.entry_hash, verified.header.clone());
            let entry = V2Entry { verified, op };
            if signed.entry_hash == plan.consumer().entry_hash {
                consumer = Some(entry);
            } else {
                entries.insert(signed.entry_hash, entry);
            }
        }
        if remaining.len() == before {
            return Err(AuthFailure::UnknownSigner);
        }
        pending = remaining;
    }
    Ok(Authenticated { consumer: consumer.ok_or(AuthFailure::UnknownSigner)?, entries, headers })
}

/// Whether `entry` mints an owner incarnation for `device`. A v2 entry's hash becomes the
/// `owner_id` exactly as a v1 mint's does.
fn mints_owner_for(entry: &V2Entry, device: DeviceFingerprint) -> bool {
    match &entry.op {
        AccountOp::DeviceAdd { device_fingerprint, role: DeviceRole::Owner, .. }
        | AccountOp::OwnerPromote { device_fingerprint } => *device_fingerprint == device,
        _ => false,
    }
}

/// Why an ancestry walk did not land.
enum WalkError {
    /// Evidence is missing — the operation parks and is reconsidered when it arrives.
    Incomplete(ParkCause),
    /// The chain is longer than the declared evidence budget can verify.
    OverBudget,
}

/// Walk an entry's own device chain back to a branch the checkpoint ACCEPTED. A withheld link parks
/// rather than rejects — it is recoverable — while a walk that lands on a legacy entry the
/// checkpoint did not accept is a branch loser no v2 continuation revives.
///
/// Each step demands the exact predecessor sequence, so a repeat is already unreachable; the step
/// budget is the same defensive posture the view scheduler takes against a cycle it likewise cannot
/// construct, and it ties the work to the declared evidence budget instead of to a chain length a
/// header can claim.
fn chain_reaches_accepted_legacy(
    entry: &AccountEntryHeader,
    headers: &HashMap<AccountEntryHash, AccountEntryHeader>,
    frozen: &fold::v2::FrozenLegacy,
    reached: &mut HashSet<AccountEntryHash>,
) -> Result<(), WalkError> {
    // An origin slot continues nothing: a device first enrolled at version 2 starts here.
    let Some(mut hash) = entry.prev_hash else {
        return Ok(());
    };
    let mut expected_seq = entry.seq.saturating_sub(1);
    // Only record the walk once it lands, so a park never memoizes an unproven chain.
    let mut walked = Vec::new();
    loop {
        if reached.contains(&hash) {
            break;
        }
        if walked.len() >= views::MAX_ENTRIES {
            return Err(WalkError::OverBudget);
        }
        let missing = if walked.is_empty() { ParkCause::ChainHead } else { ParkCause::Ancestry };
        let header = headers.get(&hash).ok_or(WalkError::Incomplete(missing))?;
        if header.device_fingerprint != entry.device_fingerprint
            || header.seq != expected_seq
            || header.log_id != fold::CONTROL_LOG
        {
            return Err(WalkError::Incomplete(ParkCause::Ancestry));
        }
        walked.push(hash);
        if header.op_version != ops::CONTROL_VERSION {
            if !frozen.accepted_at_checkpoint(&hash) {
                return Err(WalkError::Incomplete(ParkCause::Ancestry));
            }
            break;
        }
        hash = header.prev_hash.ok_or(WalkError::Incomplete(ParkCause::Ancestry))?;
        expected_seq = expected_seq.saturating_sub(1);
    }
    reached.extend(walked);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::checkpoint::{self, TrustedCheckpointPin};

    /// Exercise the walk's declared budget with synthetic headers: the step bound must hold for a
    /// complete chain longer than the evidence budget, not only for a broken one.
    #[test]
    fn ancestry_longer_than_the_evidence_budget_is_refused_not_walked() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = crate::local_account(&conn, 1).unwrap();
        let device = crate::local_device(&conn, 1).unwrap();
        let tx = conn.transaction().unwrap();
        let bundle = checkpoint::prepare_checkpoint_in_tx(&tx, account, &device).unwrap();
        let proof = checkpoint::verify_checkpoint(
            TrustedCheckpointPin {
                account_id: account,
                checkpoint_digest: bundle.certificate_digest(),
                required_control_version: 2,
            },
            &bundle,
        )
        .unwrap();

        let subject = DeviceFingerprint::from_bytes([0xab; 32]);
        let link = |seq: u64| {
            let mut hash = [0u8; 32];
            hash[..8].copy_from_slice(&seq.to_be_bytes());
            AccountEntryHash::from_bytes(hash)
        };
        let overlong = views::MAX_ENTRIES as u64 + 4;
        let mut headers = HashMap::new();
        for seq in 0..=overlong {
            headers.insert(link(seq), AccountEntryHeader {
                account_id: account,
                log_id: fold::CONTROL_LOG,
                device_fingerprint: subject,
                seq,
                prev_hash: (seq != 0).then(|| link(seq - 1)),
                parent_ref: None,
                entry_type: 2,
                op_version: ops::CONTROL_VERSION,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: None,
            });
        }
        let head = headers[&link(overlong)].clone();
        assert!(matches!(
            chain_reaches_accepted_legacy(
                &head,
                &headers,
                proof.frozen_legacy(),
                &mut HashSet::new()
            ),
            Err(WalkError::OverBudget)
        ));

        // The identical walk lands once it is short enough to fit the budget.
        let short = headers[&link(4)].clone();
        assert!(matches!(
            chain_reaches_accepted_legacy(
                &short,
                &headers,
                proof.frozen_legacy(),
                &mut HashSet::new()
            ),
            // Reaching seq 0 without meeting a legacy entry is a chain rooted outside the
            // checkpoint, which parks rather than spinning.
            Err(WalkError::Incomplete(ParkCause::Ancestry))
        ));
    }
}
