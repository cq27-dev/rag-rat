//! Bounded execution of one owner-authorized control v2 operation against a verified checkpoint.
//!
//! The evidence for a revocation is a DETACHED manifest the operation names by digest. Verification
//! requires every entry that manifest names: its signed bytes, a key the accepted legacy epoch
//! certifies, and an unbroken chain back to a branch the checkpoint accepted. Nothing is counted on
//! a peer's word — there is no entry count without the signed entry behind it.
//!
//! Incomplete evidence PARKS. A withheld manifest, a withheld chain head, a withheld mint of the
//! incarnation the operation cites, or a gap in the walk back to the legacy branch leaves the
//! operation unapplied in full: no register, no credit, and no partial effect that a later arrival
//! would have to unwind.
//!
//! Malformed input is an `Err`, never a verdict. A peer that attaches one unverifiable object to an
//! otherwise sound operation gets its bundle refused; it does not get the operation itself
//! permanently condemned, which is what a `Rejected` means.
//!
//! Every bound is the planner's declared evidence budget ([`views::MAX_ENTRIES`],
//! [`views::MAX_BYTES`]), counted over the deduplicated objects this call was handed. Every bound,
//! every commitment and every authority verdict is derived fresh on each call.
//!
//! The ONE thing a caller may share across calls is the signature check itself, through an
//! [`AuthMemo`] ([`execute_held`] does this for a refold's whole pool). A memo entry is keyed on
//! the exact `(entry_hash, signing key)` pair it was verified under, so a bundle that introduces a
//! DIFFERENT key for the same author still has to verify on its own terms — the memo can only skip
//! repeating an Ed25519 check whose two inputs are byte-identical, never substitute for one.
//!
//! **Equivocation on the consumer's own chain slot is OUT OF SCOPE here.** Two v2 entries at the
//! same `seq` off the same accepted `prev_hash` both execute, because this layer decides ONE
//! operation against a checkpoint and has no accepted-branch selection to appeal to. Choosing
//! between equivocating siblings is `select_coherent_branches`' job in the storage layer, on the
//! held candidate set, and it must happen before an operation reaches here. Saying nothing was what
//! made a reader expect the check at this layer.
//!
//! A refold of an account under a control pin this binary executes dispatches here, through
//! [`execute_held`]. Nothing else does: no CLI activates it, an UNPINNED account never reaches it
//! whatever versions its rows carry, and executing an operation flips no readiness or pin state.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::super::checkpoint::VerifiedCheckpoint;
use super::super::cut::Cut;
use super::super::envelope::{self, AccountEntryHeader, SignedAccountEntry, VerifiedAccountEntry};
use super::super::fold::{self, Candidate, RejectReason};
use super::super::id::AccountEntryHash;
use super::super::ops::AccountOp;
use super::super::registers::RegisterKey;
use super::{ops, views};
use crate::device::DevicePublic;
use crate::op::DeviceFingerprint;

/// What executing one v2 operation against the checkpoint decided.
#[derive(Debug)]
pub(in crate::account) enum Verdict {
    /// Authorized: the entry itself, the registers it installs, and the credit its signed
    /// nomination earns. The entry rides along so a consumer projecting these registers reads the
    /// very operation `apply_cut` derived them from, rather than decoding the row a second time.
    /// Boxed so an ordinary `Parked`/`Rejected` verdict stays a small value.
    Applied { entry: Box<Candidate>, registers: Vec<(RegisterKey, Cut)>, credit: u64 },
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
    /// No key the bundle carries names some entry's author. A v2 enrolment can introduce any key,
    /// so this is recoverable: the enrolment may simply have been withheld.
    Signer,
    /// A watermark the cut names belongs to an incarnation the frozen epoch does not hold.
    CutTarget,
    /// The mint of the incarnation the operation cites was not supplied. A receiver holding one
    /// bundle cannot tell an incarnation that never existed from one whose mint was withheld, and
    /// attaching that single entry authorizes the same operation — so this is recoverable.
    Mint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) enum RejectCause {
    /// The operation cites no live owner incarnation minted for its own signer, judged by the one
    /// authority resolution the credit pass also reads. Only a citation that RESOLVED lands here: a
    /// citation naming an object nothing supplied is [`ParkCause::Mint`], because no verdict over
    /// one bundle can call that permanent.
    Inadmissible,
    /// The frozen legacy registers already condemn the operation's own chain slot.
    Condemned,
    /// The cut fails a register-pass precondition and installs nothing.
    Precondition(RejectReason),
}

/// Signature checks already performed, shared across the operations of one execution pass.
///
/// Keyed on `(entry_hash, signing key)`: the entry hash commits to the header and payload, so a hit
/// means this exact object already verified under this exact key. Ed25519 verification is what an
/// execution pass actually spends, and re-running it per operation is what makes N operations over
/// one pool cost N times the pool.
pub(in crate::account) type AuthMemo = HashMap<(AccountEntryHash, [u8; 32]), V2Entry>;

/// Execute `operation` against `checkpoint`. `Err` is malformed or over-budget input; an honest
/// peer that is merely behind gets a [`Verdict::Parked`].
pub(in crate::account) fn execute(
    checkpoint: &VerifiedCheckpoint,
    operation: &[u8],
    manifests: &[Vec<u8>],
    evidence: &[Vec<u8>],
) -> anyhow::Result<Verdict> {
    execute_shared(checkpoint, operation, manifests, evidence, &mut AuthMemo::default())
}

/// [`execute`], reusing `memo`'s signature checks. Every other decision is still derived fresh.
fn execute_shared(
    checkpoint: &VerifiedCheckpoint,
    operation: &[u8],
    manifests: &[Vec<u8>],
    evidence: &[Vec<u8>],
    memo: &mut AuthMemo,
) -> anyhow::Result<Verdict> {
    let plan = match views::plan_replay(checkpoint, operation, manifests, evidence) {
        Ok(plan) => plan,
        Err(views::PlanError::MissingView(_)) => return Ok(Verdict::Parked(ParkCause::Manifest)),
        Err(views::PlanError::MissingEntry(_)) => return Ok(Verdict::Parked(ParkCause::Evidence)),
        Err(views::PlanError::Invalid(error)) => return Err(error),
    };
    let frozen = checkpoint.frozen_legacy();
    let authenticated = match authenticate(&plan, frozen, memo) {
        Ok(authenticated) => authenticated,
        // Bytes that do not verify are a bad bundle, not a bad operation.
        Err(AuthFailure::Malformed(error)) => return Err(error),
        // A v2 enrolment can introduce any key, so "no key names this author" is never provably
        // permanent — the enrolment that introduces it may simply be withheld. Parking also keeps
        // one uncertified attachment from condemning an otherwise sound operation.
        Err(AuthFailure::UnknownSigner) => return Ok(Verdict::Parked(ParkCause::Signer)),
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

    let cut = authenticated.consumer.candidate();
    // ONE authority resolution over the whole bundle, and admission reads exactly what credit
    // reads. Asking separately is how an operation gets admitted on a mint the credit pass would
    // have refused — a Member can sign its own `OwnerPromote` and cite it.
    let bundle: Vec<Candidate> = authenticated
        .entries
        .values()
        .map(V2Entry::candidate)
        .chain(std::iter::once(cut.clone()))
        .collect();
    let authority = fold::v2::V2Authority::resolve(frozen, &bundle);
    if let Some(refusal) = refuse(authority.verdict(&cut.hash())) {
        return Ok(refusal);
    }

    // Only the identities the operation's own manifest named; an ordinary operation names none.
    // Being named is not authority: each one's verdict comes from the resolution above.
    let nominated: Vec<Candidate> = plan
        .root()
        .into_iter()
        .flat_map(|view| view.entries.iter())
        .filter_map(|hash| authenticated.entries.get(hash))
        .map(V2Entry::candidate)
        .collect();
    let outcome = fold::v2::apply_cut(fold::v2::CutExecution {
        frozen,
        authority: &authority,
        nominated: &nominated,
        cut: &cut,
    });
    Ok(settle(outcome, cut))
}

/// The verdict to return for a bundle authority answer, or `None` to admit.
///
/// Variant by variant, never a wildcard — a wildcard is how a refusal that cannot support
/// permanence ends up claiming it anyway. `MintNotSupplied` is the one that must not: the cited
/// mint is simply absent from this bundle, which is withheld evidence like any other, so it parks
/// and the same operation with that mint attached is authorized.
fn refuse(verdict: fold::v2::V2Verdict) -> Option<Verdict> {
    match verdict {
        fold::v2::V2Verdict::Authorized => None,
        fold::v2::V2Verdict::Condemned => Some(Verdict::Rejected(RejectCause::Condemned)),
        fold::v2::V2Verdict::MintNotSupplied => Some(Verdict::Parked(ParkCause::Mint)),
        fold::v2::V2Verdict::Inadmissible | fold::v2::V2Verdict::WrongDevice =>
            Some(Verdict::Rejected(RejectCause::Inadmissible)),
    }
}

/// Route the register-pass outcome into the executor's verdict.
///
/// Takes `cut` by value: the caller is done with it, so the applied arm moves it into the verdict
/// and the refusal arms drop it. Nothing is cloned on any path.
fn settle(outcome: fold::v2::CutOutcome, cut: Candidate) -> Verdict {
    match outcome {
        fold::v2::CutOutcome::Applied(applied) => Verdict::Applied {
            entry: Box::new(cut),
            registers: applied.registers,
            credit: applied.credit,
        },
        fold::v2::CutOutcome::Rejected(reason) =>
            Verdict::Rejected(RejectCause::Precondition(reason)),
        fold::v2::CutOutcome::Parked(_) => Verdict::Parked(ParkCause::CutTarget),
    }
}

/// One authenticated v2 entry: the verified envelope and the inner v1 operation it carries.
#[derive(Clone)]
pub(in crate::account) struct V2Entry {
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

/// Why authentication could not complete. The two cases are never collapsed: bytes that fail under
/// a key we hold are a bad bundle and refuse it, while an author no supplied key names is only
/// evidence we do not have yet.
enum AuthFailure {
    Malformed(anyhow::Error),
    UnknownSigner,
}

/// Authenticate every supplied v2 entry under a key the ACCEPTED legacy epoch certifies, or one an
/// already-authenticated v2 enrollment introduces. An entry may introduce a key, but only once it
/// has itself authenticated: pooling unverified introductions would admit a mutually-introducing
/// cycle that a fresh receiver can never reproduce.
/// Each entry is visited once, and an enrolment wakes only the entries actually waiting on the key
/// it introduces, so the work is linear in the objects supplied rather than quadratic in their
/// worst ordering — the declared evidence budget then bounds it, as the module claims.
fn authenticate(
    plan: &views::ReplayPlan,
    frozen: &fold::v2::FrozenLegacy,
    memo: &mut AuthMemo,
) -> Result<Authenticated, AuthFailure> {
    let mut keys = frozen.device_pubkeys();
    let mut headers: HashMap<AccountEntryHash, AccountEntryHeader> =
        frozen.entries().iter().map(|entry| (entry.entry_hash, entry.header.clone())).collect();
    let mut entries: BTreeMap<AccountEntryHash, V2Entry> = BTreeMap::new();
    let mut consumer = None;
    let mut waiting: HashMap<DeviceFingerprint, Vec<&SignedAccountEntry>> = HashMap::new();
    let mut ready: Vec<&SignedAccountEntry> = Vec::new();
    for signed in plan.candidates().chain(std::iter::once(plan.consumer())) {
        if keys.contains_key(&signed.header.device_fingerprint) {
            ready.push(signed);
        } else {
            waiting.entry(signed.header.device_fingerprint).or_default().push(signed);
        }
    }
    while let Some(signed) = ready.pop() {
        let key = keys[&signed.header.device_fingerprint];
        // A key exists for this author, so the bytes now have to verify under it — unless this
        // exact object already verified under this exact key earlier in the pass. The memo is keyed
        // on both, so a different key for the same author is a different check and still runs.
        let entry = match memo.get(&(signed.entry_hash, key)) {
            Some(entry) => entry.clone(),
            None => {
                let verified = DevicePublic::from_bytes(&key)
                    .and_then(|key| envelope::verify_account_signed(&signed.signed_bytes, &key))
                    .map_err(AuthFailure::Malformed)?;
                let op = ops::decode(verified.header.entry_type, &verified.payload)
                    .map_err(AuthFailure::Malformed)?
                    .op;
                let entry = V2Entry { verified, op };
                memo.insert((signed.entry_hash, key), entry.clone());
                entry
            },
        };
        // A v2 enrolment certifies the added device's key exactly as its v1 counterpart does. It
        // does NOT confer authority — `V2Authority` judges that separately.
        if let AccountOp::DeviceAdd { device_fingerprint, ed25519_pubkey, .. } = &entry.op {
            keys.insert(*device_fingerprint, *ed25519_pubkey);
            ready.extend(waiting.remove(device_fingerprint).unwrap_or_default());
        }
        headers.insert(entry.verified.entry_hash, entry.verified.header.clone());
        if signed.entry_hash == plan.consumer().entry_hash {
            consumer = Some(entry);
        } else {
            entries.insert(signed.entry_hash, entry);
        }
    }
    if !waiting.is_empty() {
        return Err(AuthFailure::UnknownSigner);
    }
    Ok(Authenticated { consumer: consumer.ok_or(AuthFailure::UnknownSigner)?, entries, headers })
}

/// Execute every held v2 control operation for one account against `checkpoint`.
///
/// One pool, one authentication: each operation is judged against every OTHER held v2 entry as its
/// evidence, and the pass shares its signature checks through a single [`AuthMemo`]. Without that,
/// a pool of N operations costs N full verifications of the pool, on every refold.
///
/// A revocation's evidence is a DETACHED manifest, and `manifests` is what the store holds of them
/// — the annex payloads for this account, verbatim. Every operation in the pass is offered the same
/// set, because a manifest is content-addressed: which of them a given cut can use is decided by
/// the digest it signed, never by who carried the bytes. A manifest that has not arrived yet parks
/// its cut on [`ParkCause::Manifest`] until it does, and an ordinary operation names no view at
/// all.
///
/// Rows that are not v2 candidates for THIS checkpoint are excluded from the pool rather than
/// refused inside it: such a row would refuse every bundle it appeared in, not just its own. A
/// bundle that still fails to verify yields no verdict for that ONE operation and never for the
/// rest — a peer cannot silence an account's other operations by attaching one bad object.
pub(in crate::account) fn execute_held(
    checkpoint: &VerifiedCheckpoint,
    held: &[Vec<u8>],
    manifests: &[Vec<u8>],
) -> BTreeMap<AccountEntryHash, Verdict> {
    let pin = checkpoint.pin();
    let mut verdicts = BTreeMap::new();
    let mut hashes = Vec::new();
    let mut pool = Vec::new();
    for row in held {
        if let Ok((entry, _)) = views::decode_candidate(&pin, row) {
            hashes.push(entry.entry_hash);
            pool.push(row.clone());
        }
    }
    let Some(last) = pool.len().checked_sub(1) else {
        return verdicts;
    };
    let mut memo = AuthMemo::default();
    for index in 0..pool.len() {
        // Rotate the consumer to the end so the remaining prefix is exactly its evidence without
        // copying the pool once per operation — `plan_replay` refuses a bundle carrying its own
        // consumer, so the consumer has to come out of the evidence one way or another.
        hashes.swap(index, last);
        pool.swap(index, last);
        let consumer = hashes[last];
        let verdict = {
            let (evidence, operation) = pool.split_at(last);
            execute_shared(checkpoint, &operation[0], manifests, evidence, &mut memo)
        };
        hashes.swap(index, last);
        pool.swap(index, last);
        if let Ok(verdict) = verdict {
            verdicts.insert(consumer, verdict);
        }
    }
    verdicts
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
        // The budget counts the v2 links; the legacy entry the walk terminates on sits one beyond
        // it, so a chain filling the largest bundle `plan_replay` accepts still lands.
        if walked.len() > views::MAX_ENTRIES {
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
        // A device first enrolled at version 2 roots its OWN chain at seq 0, exactly as the
        // starting entry may. Reaching that origin completes the walk rather than failing it: what
        // binds such a device to the account is the authority rule, never this chain.
        let Some(prev) = header.prev_hash else {
            break;
        };
        hash = prev;
        expected_seq = expected_seq.saturating_sub(1);
    }
    reached.extend(walked);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::checkpoint::{self, TrustedCheckpointPin};

    /// A verified checkpoint over a fresh account. The account id its entries must name is
    /// `proof.pin().account_id`.
    fn checkpoint_proof() -> checkpoint::VerifiedCheckpoint {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = crate::local_account(&conn, 1).unwrap();
        let device = crate::local_device(&conn, 1).unwrap();
        let tx = conn.transaction().unwrap();
        let bundle = checkpoint::prepare_checkpoint_in_tx(&tx, account, &device).unwrap();
        checkpoint::verify_checkpoint(
            TrustedCheckpointPin {
                account_id: account,
                checkpoint_digest: bundle.certificate_digest(),
                required_control_version: 2,
            },
            &bundle,
        )
        .unwrap()
    }

    /// The synthetic chain-link hash for `seq`. Distinct per seq and never equal to a real content
    /// hash, so a checkpoint never accepts one.
    fn link(seq: u64) -> AccountEntryHash {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&seq.to_be_bytes());
        AccountEntryHash::from_bytes(hash)
    }

    /// The device every synthetic chain below is authored by.
    fn subject_device() -> DeviceFingerprint {
        DeviceFingerprint::from_bytes([0xab; 32])
    }

    /// Exercise the walk's declared budget with synthetic headers: the step bound must hold for a
    /// complete chain longer than the evidence budget, not only for a broken one.
    #[test]
    fn ancestry_longer_than_the_evidence_budget_is_refused_not_walked() {
        let proof = checkpoint_proof();
        let account = proof.pin().account_id;
        let subject = subject_device();
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

        // The identical walk lands once it is short enough to fit the budget. These links are v2,
        // so reaching the seq-0 origin COMPLETES the walk: a device first enrolled at version 2
        // roots its own chain there, and the authority rule is what binds it to the account.
        let short = headers[&link(4)].clone();
        assert!(matches!(
            chain_reaches_accepted_legacy(
                &short,
                &headers,
                proof.frozen_legacy(),
                &mut HashSet::new()
            ),
            Ok(())
        ));

        // The exact boundary the comment above the bound claims: a chain filling the largest
        // bundle still LANDS. Walking from seq `MAX_ENTRIES + 1` pushes `MAX_ENTRIES + 1` links and
        // tests the bound at most at `MAX_ENTRIES`, so `>` accepts it and `>=` would not. Without
        // this case both comparisons refuse the overlong chain above and the off-by-one is
        // invisible.
        let boundary = headers[&link(views::MAX_ENTRIES as u64 + 1)].clone();
        assert!(
            matches!(
                chain_reaches_accepted_legacy(
                    &boundary,
                    &headers,
                    proof.frozen_legacy(),
                    &mut HashSet::new()
                ),
                Ok(())
            ),
            "a chain exactly filling the evidence budget lands rather than being refused",
        );
    }

    /// A synthetic candidate. `settle` never inspects it — it only decides whether to materialize
    /// one — so a header literal is enough and no signature is needed.
    fn synthetic_candidate() -> Candidate {
        let header = AccountEntryHeader {
            account_id: checkpoint_proof().pin().account_id,
            log_id: fold::CONTROL_LOG,
            device_fingerprint: subject_device(),
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: 2,
            op_version: ops::CONTROL_VERSION,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: None,
        };
        Candidate::new(
            VerifiedAccountEntry { header, payload: Vec::new(), entry_hash: link(0) },
            AccountOp::OwnerPromote { device_fingerprint: subject_device() },
        )
    }

    /// Every authority verdict routes to exactly one answer. `refuse`'s `match` is what the
    /// compiler forces to stay exhaustive — a new `V2Verdict` variant fails to build there, not
    /// here — so this table is a convention that must be extended alongside it, not a guarantee.
    ///
    /// The distinction it exists to hold is permanence. `MintNotSupplied` is the only verdict that
    /// is not a property of the operation: the same operation with its mint attached is
    /// authorized, so it must park. Routing it to a rejection would make withheld evidence
    /// permanent.
    #[test]
    fn every_authority_verdict_routes_to_one_answer() {
        use fold::v2::V2Verdict;
        assert!(refuse(V2Verdict::Authorized).is_none());
        assert!(matches!(
            refuse(V2Verdict::Condemned),
            Some(Verdict::Rejected(RejectCause::Condemned))
        ));
        assert!(matches!(
            refuse(V2Verdict::MintNotSupplied),
            Some(Verdict::Parked(ParkCause::Mint))
        ));
        assert!(matches!(
            refuse(V2Verdict::Inadmissible),
            Some(Verdict::Rejected(RejectCause::Inadmissible))
        ));
        assert!(matches!(
            refuse(V2Verdict::WrongDevice),
            Some(Verdict::Rejected(RejectCause::Inadmissible))
        ));
    }

    /// Every register-pass outcome routes to exactly one verdict, and the applied arm carries its
    /// registers and credit through untouched — a non-empty register set, so a pass-through that
    /// dropped or replaced it fails here rather than only in the fixture tests downstream.
    #[test]
    fn every_cut_outcome_routes_to_one_verdict() {
        use fold::v2::{AppliedCut, CutOutcome};
        let key = RegisterKey::Device {
            account: checkpoint_proof().pin().account_id,
            log: fold::CONTROL_LOG,
            device: subject_device(),
        };
        let registers = vec![(key.clone(), Cut::At { seq: 3, hash: link(3) })];
        let applied = settle(
            CutOutcome::Applied(AppliedCut { registers: registers.clone(), credit: 7 }),
            synthetic_candidate(),
        );
        match applied {
            Verdict::Applied { registers: got, credit, .. } => {
                assert_eq!(credit, 7);
                assert_eq!(got, registers, "the applied arm carries its registers through");
            },
            other => panic!("expected an applied verdict, got {other:?}"),
        }

        assert!(matches!(
            settle(CutOutcome::Rejected(RejectReason::LastOwner), synthetic_candidate()),
            Verdict::Rejected(RejectCause::Precondition(RejectReason::LastOwner))
        ));
        assert!(matches!(
            settle(CutOutcome::Parked(fold::ParkReason::UnknownCutTarget), synthetic_candidate()),
            Verdict::Parked(ParkCause::CutTarget)
        ));
    }

    /// A short, complete, walkable v2 chain plus the pieces to perturb one link of it. Every step
    /// guard below starts from a chain the walk ACCEPTS, so a test that fails proves its own
    /// perturbation was the cause rather than an unwalkable fixture.
    struct Walk {
        proof: checkpoint::VerifiedCheckpoint,
        headers: HashMap<AccountEntryHash, AccountEntryHeader>,
        head: AccountEntryHeader,
    }

    fn walk_fixture() -> Walk {
        let proof = checkpoint_proof();
        let account = proof.pin().account_id;
        let mut headers = HashMap::new();
        for seq in 0..=4 {
            headers.insert(link(seq), AccountEntryHeader {
                account_id: account,
                log_id: fold::CONTROL_LOG,
                device_fingerprint: subject_device(),
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
        let head = headers[&link(4)].clone();
        Walk { proof, headers, head }
    }

    impl Walk {
        fn walk(&self) -> Result<(), WalkError> {
            self.walk_from(&mut HashSet::new())
        }

        /// The same walk against a caller-supplied `reached` set, which `execute_shared` shares
        /// across a whole pool rather than resetting per entry.
        fn walk_from(&self, reached: &mut HashSet<AccountEntryHash>) -> Result<(), WalkError> {
            chain_reaches_accepted_legacy(
                &self.head,
                &self.headers,
                self.proof.frozen_legacy(),
                reached,
            )
        }

        /// The same walk against a caller-supplied header set, for perturbing the headers without
        /// disturbing the fixture's own.
        fn walk_with(
            &self,
            headers: &HashMap<AccountEntryHash, AccountEntryHeader>,
        ) -> Result<(), WalkError> {
            chain_reaches_accepted_legacy(
                &self.head,
                headers,
                self.proof.frozen_legacy(),
                &mut HashSet::new(),
            )
        }
    }

    /// Each step must belong to the SAME device chain. Without this a walk hops onto another
    /// device's entries and a continuation inherits a chain its author never wrote.
    #[test]
    fn a_step_on_another_device_chain_parks_the_walk() {
        let mut f = walk_fixture();
        assert!(
            matches!(f.walk(), Ok(())),
            "the unperturbed chain must walk, or this proves nothing"
        );

        let hash = link(2);
        f.headers.get_mut(&hash).unwrap().device_fingerprint =
            DeviceFingerprint::from_bytes([0xcd; 32]);
        assert!(
            matches!(f.walk(), Err(WalkError::Incomplete(ParkCause::Ancestry))),
            "a step signed by a different device cannot continue this chain",
        );
    }

    /// Each step must occupy the exact predecessor slot. Without this a gapped chain walks, and the
    /// step budget stops bounding the walk because a repeat becomes reachable.
    #[test]
    fn a_step_at_the_wrong_sequence_slot_parks_the_walk() {
        let mut f = walk_fixture();
        assert!(
            matches!(f.walk(), Ok(())),
            "the unperturbed chain must walk, or this proves nothing"
        );

        let hash = link(2);
        f.headers.get_mut(&hash).unwrap().seq = 7;
        assert!(
            matches!(f.walk(), Err(WalkError::Incomplete(ParkCause::Ancestry))),
            "a step off the expected predecessor slot cannot continue this chain",
        );
    }

    /// Each step must be on the CONTROL log. Without this a secrets- or annex-log entry satisfies a
    /// control-chain step, and authority is derived from a log that never carried it.
    #[test]
    fn a_step_on_another_log_parks_the_walk() {
        let mut f = walk_fixture();
        assert!(
            matches!(f.walk(), Ok(())),
            "the unperturbed chain must walk, or this proves nothing"
        );

        let hash = link(2);
        f.headers.get_mut(&hash).unwrap().log_id = fold::ANNEX_LOG;
        assert!(
            matches!(f.walk(), Err(WalkError::Incomplete(ParkCause::Ancestry))),
            "a step on a non-control log cannot continue a control chain",
        );
    }

    /// A walk terminating on a LEGACY link lands only if the checkpoint accepted that link. Without
    /// this a v2 continuation revives a branch the checkpoint rejected — the one outcome the pin
    /// exists to prevent.
    #[test]
    fn a_walk_terminating_on_an_unaccepted_legacy_entry_parks() {
        let mut f = walk_fixture();
        assert!(
            matches!(f.walk(), Ok(())),
            "the unperturbed chain must walk, or this proves nothing"
        );

        // Make the seq-0 root a LEGACY entry. The checkpoint was prepared over an account that
        // never held this synthetic hash, so it is held-but-not-accepted.
        let root = link(0);
        f.headers.get_mut(&root).unwrap().op_version = 1;
        assert!(
            !f.proof.frozen_legacy().accepted_at_checkpoint(&root),
            "the fixture root must be unaccepted at the checkpoint or this proves nothing",
        );
        assert!(
            matches!(f.walk(), Err(WalkError::Incomplete(ParkCause::Ancestry))),
            "a legacy terminus the checkpoint did not accept is a branch loser, not a root",
        );
    }

    /// A link already in `reached` stops the walk. `execute_shared` shares one set across the whole
    /// pool, so this is not mere memoization: without it every operation re-walks their common
    /// prefix, and a long enough one tips the step budget and refuses the entire bundle.
    #[test]
    fn a_link_already_reached_stops_the_walk_instead_of_rewalking_it() {
        let mut f = walk_fixture();
        assert!(
            matches!(f.walk(), Ok(())),
            "the unperturbed chain must walk, or this proves nothing"
        );

        // A landing walk RECORDS what it walked. The read below is only meaningful because of this
        // write: `execute_shared` shares one set across a whole pool, so without it every operation
        // re-walks their common prefix and a long enough one tips the step budget, refusing the
        // whole bundle rather than applying it.
        let mut reached = HashSet::new();
        assert!(matches!(f.walk_from(&mut reached), Ok(())));
        assert!(
            reached.contains(&link(3)) && reached.contains(&link(2)),
            "a landing walk records the links it walked",
        );

        // Remove the tail of the chain so a full walk CANNOT land, then pre-reach the link just
        // above the hole. The walk must stop at the memo; without it, it reads on into the hole.
        f.headers.remove(&link(0));
        let mut parked = HashSet::new();
        assert!(
            matches!(f.walk_from(&mut parked), Err(WalkError::Incomplete(ParkCause::Ancestry))),
            "with the root missing and nothing pre-reached the walk cannot land",
        );
        // A park must leave NOTHING behind: the prefix it crossed was never proven to reach an
        // accepted branch, and the set it would poison is shared across the pool.
        assert!(
            parked.is_empty(),
            "a parked walk memoizes no part of the chain it could not prove",
        );

        let mut reached = HashSet::from([link(1)]);
        assert!(
            matches!(f.walk_from(&mut reached), Ok(())),
            "a pre-reached link completes the walk without descending past it",
        );
    }

    /// The park cause names WHICH link is missing: the entry's own chain head, or a link deeper in
    /// its ancestry. A receiver refetches on that distinction, so collapsing the two loses it.
    #[test]
    fn a_missing_chain_head_and_a_missing_ancestor_park_for_different_reasons() {
        let mut f = walk_fixture();
        assert!(
            matches!(f.walk(), Ok(())),
            "the unperturbed chain must walk, or this proves nothing"
        );

        // The head is seq 4, so its own predecessor slot is seq 3: the FIRST step.
        let mut first_gap = f.headers.clone();
        first_gap.remove(&link(3));
        assert!(
            matches!(f.walk_with(&first_gap), Err(WalkError::Incomplete(ParkCause::ChainHead))),
            "the entry's own chain head is missing",
        );

        // One step deeper: the walk took a step, so the same absence is an ancestry gap.
        f.headers.remove(&link(2));
        assert!(
            matches!(f.walk(), Err(WalkError::Incomplete(ParkCause::Ancestry))),
            "a link below the chain head is missing",
        );
    }
}
