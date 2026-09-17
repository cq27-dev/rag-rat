//! Execution over a permanent legacy epoch. The checkpoint finalizes legacy register decisions;
//! v2 may close current authority but cannot replay old cut admission or revive old branch losers.
//!
//! **What a nomination guarantees.** A cut's signed manifest fixes an UPPER BOUND on the identities
//! it may ever count — the entries it named, plus the legacy entries the checkpoint accepted. It is
//! not a frozen number and not a claim of causation: the numeric credit may still fall as other
//! authorized cuts change those identities' outcomes, and nothing here establishes that this cut
//! was historically effective.
//!
//! **Credit is the v1 rule over a CONSTRUCTED outcome map.** The counting is
//! [`super::revocation_credit`] itself under a narrower membership test, so against one fixed
//! outcome map it can only ever count fewer entries than v1 would. The map is where the two part
//! company: a legacy entry's outcome is the checkpoint's own, but a nominated v2 entry's is
//! constructed here rather than folded. So this is NOT a ceiling underneath a real v1 fold, and a
//! reader must not treat it as one — the residual below says exactly where it can exceed one.
//!
//! **What is and is not evaluated.** A nominated entry is effective only if it held authority: a
//! live owner incarnation minted for its OWN signer, with no legacy register already cutting its
//! chain. Authentication and the chain walk establish who wrote an entry and where it sits, never
//! that it was allowed to, so neither substitutes for that check. A cited mint naming a different
//! device is `WrongDevice`, distinct from `StaleAuthority`, because the two are credited
//! differently: the second loop of the credit rule counts stale dependents of a condemned mint and
//! must not count an impersonator.
//!
//! That check makes this pass STRICTER than v1. It is a deliberate tightening, NOT the repair of a
//! violated bound — no bound was ever broken here, and a reader should not go looking for one. v1
//! does credit such an entry: its condemnation overlay overwrites the effect pass's
//! `Rejected(StaleAuthority)`, so an unauthorized op on the revoked chain still ends `Condemned`
//! and still counts. It is held out here because it was never in the effective count, so crediting
//! its removal would count something its own author never counted. Under-crediting only parks a
//! cut, it never admits one that is ahead.
//!
//! **Register-pass preconditions ARE enforced.** Before a cut installs anything it passes the same
//! I2 last-owner guard, `OwnerDemote` owner_id ↔ subject binding and §11.3 watermark binding that
//! v1's register pass applies, by calling v1's own predicates rather than restating them. Those are
//! not state preconditions and are NOT part of the residual below: a cut v1 refuses to admit
//! installs nothing here either, which is what keeps a sole owner from removing itself and leaving
//! the account with no authority at all.
//!
//! Those two lookups resolve differently on purpose. The `OwnerDemote` binding also resolves an
//! incarnation an authorized v2 mint in the bundle opened, since it is a LOOKUP and widening it
//! only lets a correct operation through — resolving the frozen epoch alone would park a demote of
//! a v2-promoted owner forever, on evidence already supplied. The last-owner guard counts only the
//! FROZEN open owners, because it is a COUNT: widening it on the strength of entries whose effects
//! this operation does not install would weaken the one invariant stopping an ownerless account.
//! The cost is that a cut relying on a v2-promoted co-owner to escape the guard is refused until
//! the checkpoint advances past that promotion.
//!
//! **The residual, in the other direction.** State preconditions are NOT evaluated. A nominated
//! entry the v1 fold would reject `Ineffective` — re-enrolling a device already on the roster,
//! promoting one that is not enrolled, granting on a stream that is not publicly owned — is counted
//! here once the cut condemns it, and a real v1 fold would not count it. Evaluating them needs the
//! running effect-pass state, which the frozen facts do not carry, and a second copy of that rule
//! table would be free to drift from the one in [`super::classify_effect`].
//!
//! The excess is bounded by the nominated entries the cut's OWN register keys scope, plus the
//! transitive stale dependents of any mint among them — the same closure v1 credits, and the reason
//! the second loop needs the `WrongDevice` split above. Nothing a peer merely asserts widens it:
//! every counted entry is a signed object this call authenticated.
//! `a_nominated_entry_v1_would_reject_ineffective_is_still_counted` demonstrates the worst case
//! rather than leaving it to argument.
//!
//! **What the checkpoint froze.** Legacy outcomes are read from the verified fold verbatim. A v2
//! cut may condemn what the legacy epoch left standing, but it cannot revive a branch loser, awaken
//! a legacy parked cut, or undo a legacy tombstone — and a legacy entry that was already out of the
//! frozen effective count earns no credit for being removed a second time.
//!
//! **Composition.** [`pinned_history`] extends that frozen history with the register effects of the
//! operations a refold applied. The only operations installing a register are revocations, and a
//! revocation names a detached pre-cut manifest — an ordinary annex entry the refold supplies from
//! held rows. While that manifest is not held the cut parks and composes nothing; storing it is
//! what lets the refold hand the executor its evidence and project the register. What no production
//! path does is AUTHOR a v2 cut or a manifest: installing a pin is test-only.

use super::*;

/// Authenticated evidence and the FINAL coherent legacy fold, captured once during checkpoint
/// verification. Only the fold state execution actually reads is retained: of the trace, that is
/// the registers alone. Normal v1 folds allocate no trace at all.
pub(in crate::account) struct FrozenLegacy {
    entries: Vec<VerifiedAccountEntry>,
    history: AccountAuthHistory,
    registers: HashMap<RegisterKey, Cut>,
    accepted: HashSet<AccountEntryHash>,
}

impl FrozenLegacy {
    pub(in crate::account) fn new(
        entries: Vec<VerifiedAccountEntry>,
        history: AccountAuthHistory,
        trace: LegacyTrace,
        accepted: HashSet<AccountEntryHash>,
    ) -> Self {
        Self { entries, history, registers: trace.registers, accepted }
    }

    pub(in crate::account) fn entries(&self) -> &[VerifiedAccountEntry] {
        &self.entries
    }

    /// The FINAL legacy fold the checkpoint committed to. A pinned refold projects this verbatim
    /// rather than re-deriving it: the certificate's `projection_hash` is a commitment to exactly
    /// this history, so re-folding the same evidence could only reproduce it or disagree with the
    /// pin.
    pub(in crate::account) fn history(&self) -> &AccountAuthHistory {
        &self.history
    }

    pub(in crate::account) fn accepted_entries(
        &self,
    ) -> impl Iterator<Item = AccountEntryHash> + '_ {
        self.accepted.iter().copied()
    }

    /// Whether the checkpoint accepted this legacy entry. This is the eligibility ceiling for
    /// legacy identities: a forked, parked or condemned entry contributed nothing to the frozen
    /// effective count, so a later cut removing it again has taken nothing away.
    pub(in crate::account) fn accepted_at_checkpoint(&self, hash: &AccountEntryHash) -> bool {
        self.accepted.contains(hash)
    }

    /// The device keys the ACCEPTED legacy epoch certifies. A branch loser never introduces one.
    pub(in crate::account) fn device_pubkeys(&self) -> HashMap<DeviceFingerprint, [u8; 32]> {
        let mut keys = HashMap::new();
        for entry in self.entries.iter().filter(|e| self.accepted.contains(&e.entry_hash)) {
            crate::account::storage::add_self_pubkey(&mut keys, &entry.header, &entry.payload);
        }
        keys
    }

    /// Whether `incarnation` is an owner incarnation the frozen epoch still holds open for
    /// `device`. Private on purpose: every caller goes through [`V2Authority`], so admission and
    /// credit cannot end up asking different questions.
    fn owner_is_live(&self, incarnation: OwnerId, device: DeviceFingerprint) -> bool {
        matches!(
            self.history.owner_incarnation_effective(incarnation, device),
            AuthorityQuery::Effective(_)
        )
    }

    /// The devices holding an OPEN owner incarnation, shaped for the I2 last-owner guard.
    fn open_owners(&self) -> HashMap<DeviceFingerprint, OwnerId> {
        self.history
            .owner_incarnation_facts()
            .filter(|(_, fact)| fact.closed_at.is_none())
            .map(|(id, fact)| (fact.authority.device_fingerprint, *id))
            .collect()
    }

    /// The device an incarnation was minted for, if the frozen epoch holds it at all.
    fn incarnation_subject(&self, incarnation: OwnerId) -> Option<DeviceFingerprint> {
        self.history
            .owner_incarnation_facts()
            .find(|(id, _)| **id == incarnation)
            .map(|(_, fact)| fact.authority.device_fingerprint)
    }

    /// The legacy entries the control fold actually folds — the immutable baseline a v2 cut
    /// executes over. Mirrors the `foldable` gate the v1 pass applies to the same evidence.
    fn foldable(&self) -> Vec<Candidate> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.header.log_id == CONTROL_LOG
                    && entry.header.op_version == SUPPORTED_OP_VERSION
                    && entry.header.crypto_suite == 0
            })
            .filter_map(|entry| match ops::decode(entry.header.entry_type, &entry.payload) {
                Ok(DecodedAccountOp::Known(op)) => Some(Candidate::new(entry.clone(), op)),
                _ => None,
            })
            .collect()
    }
}

/// One authenticated v2 operation resolved against the frozen epoch and the view it signed.
pub(in crate::account) struct CutExecution<'a> {
    pub(in crate::account) frozen: &'a FrozenLegacy,
    /// Authority for every authenticated entry in the bundle, resolved ONCE by
    /// [`V2Authority::resolve`] so admission and credit read the same answer.
    pub(in crate::account) authority: &'a V2Authority,
    /// The v2 entries the operation's signed manifest nominates. Being named here is not
    /// authority: each one's verdict still comes from `authority`.
    pub(in crate::account) nominated: &'a [Candidate],
    pub(in crate::account) cut: &'a Candidate,
}

impl CutExecution<'_> {
    fn headers(&self) -> HashMap<AccountEntryHash, &AccountEntryHeader> {
        self.frozen
            .entries
            .iter()
            .map(|entry| (entry.entry_hash, &entry.header))
            .chain(self.nominated.iter().map(|c| (c.hash(), c.header())))
            .chain(std::iter::once((self.cut.hash(), self.cut.header())))
            .collect()
    }
}

/// What an authorized operation installs, and the bounded credit its nomination earns.
pub(in crate::account) struct AppliedCut {
    pub(in crate::account) registers: Vec<(RegisterKey, Cut)>,
    pub(in crate::account) credit: u64,
}

/// What executing one operation decided. A cut failing a register precondition installs nothing.
pub(in crate::account) enum CutOutcome {
    Applied(AppliedCut),
    Rejected(RejectReason),
    Parked(ParkReason),
}

/// The authority verdict for one control v2 entry against the frozen epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::account) enum V2Verdict {
    Authorized,
    /// The cited incarnation is not a live owner minted for this signer.
    Inadmissible,
    /// The cited mint names a different device — impersonation.
    WrongDevice,
    /// The frozen legacy registers already cut this chain.
    Condemned,
    /// The citation chain dies at an object this call was never handed. Alone among these verdicts
    /// it is not a property of the operation: the same operation with that mint attached may be
    /// authorized, so a consumer must route it to a park and never to a permanent refusal.
    MintNotSupplied,
}

/// THE control v2 authority rule, resolved once per bundle.
///
/// A mint a signer cites must ITSELF have been authorized, transitively, back to an incarnation the
/// frozen epoch holds open. Asking that question in two places is exactly how an operation gets
/// admitted on a mint the credit pass would have refused, so there is deliberately one resolution
/// and no second way to ask: a Member device can sign a self-serving `OwnerPromote` and cite it,
/// and only the transitive check refuses both.
pub(in crate::account) struct V2Authority {
    verdicts: HashMap<AccountEntryHash, V2Verdict>,
    /// The incarnations AUTHORIZED v2 mints opened, and the device each was minted for.
    mints: HashMap<OwnerId, DeviceFingerprint>,
}

impl V2Authority {
    pub(in crate::account) fn resolve(frozen: &FrozenLegacy, bundle: &[Candidate]) -> Self {
        let mut verdicts = HashMap::new();
        let Some(genesis) = frozen.history.genesis_hash() else {
            // No genesis in the frozen epoch: no citation resolves against it and no arrival
            // changes a checkpoint, so every entry falls to the permanent default below.
            return Self { verdicts, mints: HashMap::new() };
        };
        let legacy = frozen.foldable();
        let legacy_count = legacy.len();
        let mut candidates = legacy;
        candidates.extend(bundle.iter().cloned());
        let mut headers: HashMap<AccountEntryHash, &AccountEntryHeader> =
            frozen.entries.iter().map(|entry| (entry.entry_hash, &entry.header)).collect();
        for candidate in &candidates[legacy_count..] {
            headers.insert(candidate.hash(), candidate.header());
        }
        let view = CandidateView { headers: &headers };
        let mut incarnations = Incarnations::build(&candidates, genesis.into());
        let mut strata: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (idx, candidate) in candidates.iter().enumerate() {
            if let Some(depth) = incarnations.author_depth(candidate) {
                strata.entry(depth).or_default().push(idx);
            }
        }
        // Ascending depth, so a mint is settled before anything citing it. An entry whose cited
        // incarnation does not resolve is never reached here; the pass after this one classifies
        // why, because the reason decides whether its refusal can ever be cleared.
        let mut mints: HashMap<OwnerId, DeviceFingerprint> = HashMap::new();
        for &idx in strata.values().flatten() {
            if idx < legacy_count {
                continue;
            }
            let candidate = &candidates[idx];
            let signer = candidate.header().device_fingerprint;
            let cited = candidate.header().authority_ref;
            let live = cited.is_some_and(|incarnation| {
                frozen.owner_is_live(incarnation, signer)
                    || mints.get(&incarnation) == Some(&signer)
            });
            let verdict = if !live {
                // Separate impersonation from a mint that is merely no longer live: only the
                // latter leaves a stale dependent the credit rule's second loop may count.
                let impersonates = cited
                    .and_then(|incarnation| incarnations.candidate(&incarnation))
                    .is_some_and(|mint| mint.subject_device() != signer);
                if impersonates { V2Verdict::WrongDevice } else { V2Verdict::Inadmissible }
            } else if matches!(
                register_verdict(candidate, &frozen.registers, &view),
                RegisterVerdict::Condemned(_)
            ) {
                // A chain the legacy epoch cut is not reopened by continuing it at version 2.
                V2Verdict::Condemned
            } else {
                // Only an AUTHORIZED mint may certify anything citing it.
                if candidate.is_mint() {
                    mints.insert(candidate.hash().into(), candidate.subject_device());
                }
                V2Verdict::Authorized
            };
            verdicts.insert(candidate.hash(), verdict);
        }
        // The entries the walk never reached are refused too, but not alike. A citation chain that
        // dies at an object NOTHING SUPPLIED is refused for want of evidence — the same bundle plus
        // that mint authorizes the same operation — while one that dies at an object we hold, or
        // that cites nothing at all, is refused on its own merits. The frozen epoch never changes,
        // so only the first can ever be cleared, and only the first may park.
        //
        // One answer per incarnation rather than per entry: every link of a chain shares the
        // terminal that answers it, so a bundle whose entries all cite one long unresolvable chain
        // walks it once. That keeps this pass bounded by the incarnations the declared evidence
        // budget admits, as the rest of the engine is.
        let mut unsupplied: HashMap<OwnerId, bool> = HashMap::new();
        for candidate in bundle {
            if verdicts.contains_key(&candidate.hash()) {
                continue;
            }
            let withheld = incarnations.author_incarnation_id(candidate).is_some_and(|cited| {
                cites_unsupplied_mint(&incarnations, &headers, &mut unsupplied, cited)
            });
            let verdict =
                if withheld { V2Verdict::MintNotSupplied } else { V2Verdict::Inadmissible };
            verdicts.insert(candidate.hash(), verdict);
        }
        Self { verdicts, mints }
    }

    /// Every entry of the resolved bundle carries a verdict, including the ones the stratum walk
    /// never reached. Absence therefore means the caller is asking about an entry this resolution
    /// was never given: it holds no authority, and it never parks on that account.
    pub(in crate::account) fn verdict(&self, hash: &AccountEntryHash) -> V2Verdict {
        self.verdicts.get(hash).copied().unwrap_or(V2Verdict::Inadmissible)
    }

    /// The device an AUTHORIZED v2 mint in this bundle opened `incarnation` for.
    fn mint_subject(&self, incarnation: OwnerId) -> Option<DeviceFingerprint> {
        self.mints.get(&incarnation).copied()
    }
}

/// Whether the citations from `cited` run out at an object this call was never handed. That is the
/// ONE way a citation fails to resolve recoverably: the mint may simply have been withheld, and
/// attaching it authorizes the very same operation. A citation landing on an object we DO hold is
/// answered for good — a held entry that is not a mint never becomes one, and a mint citing nothing
/// never gains a citation. This walks the chain [`Incarnations::incarnation_depth`] walks, because
/// that walk is what decides whether an entry is reached at all.
fn cites_unsupplied_mint(
    incarnations: &Incarnations<'_>,
    headers: &HashMap<AccountEntryHash, &AccountEntryHeader>,
    unsupplied: &mut HashMap<OwnerId, bool>,
    cited: OwnerId,
) -> bool {
    let mut walked = HashSet::new();
    let mut node = cited;
    let answer = loop {
        if let Some(&answered) = unsupplied.get(&node) {
            break answered;
        }
        // A cycle resolves nothing, and nothing that arrives later breaks it.
        if !walked.insert(node) {
            break false;
        }
        let Some(mint) = incarnations.candidate(&node) else {
            let hash: AccountEntryHash = node.into();
            break !headers.contains_key(&hash);
        };
        match mint.header().authority_ref {
            None => break false,
            Some(parent) => node = parent,
        }
    };
    // Every link walked ends at the terminal that answered it, so they all take that answer.
    for link in walked {
        unsupplied.insert(link, answer);
    }
    answer
}

/// Execute one authorized operation. A non-cut installs nothing and earns nothing.
pub(in crate::account) fn apply_cut(input: CutExecution<'_>) -> CutOutcome {
    // The one resolution decides THIS operation too. Keeping the check here, and not only in the
    // caller, is the whole point of resolving once: an authority answer a second caller has to
    // remember to re-apply is an authority answer that eventually goes unapplied. Spelled out
    // variant by variant for the same reason: a verdict added later must be routed deliberately,
    // not swept into a permanent refusal by a wildcard.
    match input.authority.verdict(&input.cut.hash()) {
        V2Verdict::Authorized => {},
        V2Verdict::WrongDevice => return CutOutcome::Rejected(RejectReason::WrongDevice),
        // Refused for want of an entry, not on its merits: the cited mint would authorize this very
        // operation, so the refusal cannot claim permanence.
        V2Verdict::MintNotSupplied => return CutOutcome::Parked(ParkReason::UnknownOwnerRef),
        V2Verdict::Inadmissible | V2Verdict::Condemned =>
            return CutOutcome::Rejected(RejectReason::StaleAuthority),
    }
    let proposed = cut_op_registers(input.cut);
    if proposed.is_empty() {
        return CutOutcome::Applied(AppliedCut { registers: Vec::new(), credit: 0 });
    }
    if let Some(refused) = register_precondition(&input, &proposed) {
        return refused;
    }
    let registers = proposed.into_iter().map(|(key, cut, _)| (key, cut)).collect();
    // The nominated identities, plus the legacy entries the checkpoint accepted — legacy evidence
    // is implicit in every view, but bounded by what was actually standing when it froze.
    let mut eligible: HashSet<AccountEntryHash> =
        input.nominated.iter().map(Candidate::hash).collect();
    eligible.extend(input.frozen.accepted.iter().copied());
    let credit = credit_under(&input, CreditScope::Nominated(&eligible));
    CutOutcome::Applied(AppliedCut { registers, credit })
}

/// The register-pass checks a cut must pass before it installs anything: the I2 last-owner guard,
/// the `OwnerDemote` owner_id ↔ subject binding, and §11.3 watermark binding. These call v1's own
/// predicates rather than restating them — a cut v1 refuses to admit must install nothing here too,
/// or an account can be left with no owner at all.
fn register_precondition(
    input: &CutExecution<'_>,
    proposed: &[(RegisterKey, Cut, CutCoordinate)],
) -> Option<CutOutcome> {
    let owners = input.frozen.open_owners();
    if owners.len() == 1 && closes_open_incarnation(&input.cut.op, &owners).is_some() {
        return Some(CutOutcome::Rejected(RejectReason::LastOwner));
    }
    if let AccountOp::OwnerDemote { device_fingerprint, owner_id, .. } = &input.cut.op {
        // An incarnation an authorized v2 mint opened is as real as a frozen one. Resolving only
        // the frozen epoch would park a correct demote of a v2-promoted owner forever, because the
        // evidence that would clear the park is already supplied.
        let subject = input
            .frozen
            .incarnation_subject(*owner_id)
            .or_else(|| input.authority.mint_subject(*owner_id));
        match subject {
            None => return Some(CutOutcome::Parked(ParkReason::UnknownOwnerRef)),
            Some(subject) if subject != *device_fingerprint =>
                return Some(CutOutcome::Rejected(RejectReason::WrongDevice)),
            Some(_) => {},
        }
    }
    let headers = input.headers();
    let view = CandidateView { headers: &headers };
    // A watermark naming a DIFFERENT coordinate rejects the whole op; one not yet held still
    // installs, exactly as v1 treats it.
    let misbound = proposed.iter().any(|(_, cut, coord)| {
        candidate::validate_cut_target(cut, coord, &view) == candidate::CutBinding::Mismatch
    });
    misbound.then_some(CutOutcome::Rejected(RejectReason::CutTargetMismatch))
}

/// One v2 operation the executor applied, carried together with the registers [`apply_cut`]
/// installed for it.
///
/// The registers travel WITH the entry rather than being re-derived from its op at the projection.
/// A second derivation is exactly how a roster and a credit end up disagreeing about what a cut
/// took away; both now read the one `apply_cut` result.
pub(in crate::account) struct AppliedOperation<'a> {
    pub(in crate::account) entry: &'a Candidate,
    pub(in crate::account) registers: &'a [(RegisterKey, Cut)],
}

/// The authority history of an account under a control pin this binary EXECUTES: the checkpoint's
/// frozen legacy history, extended by the register effects of the v2 operations the executor
/// applied.
///
/// Derived, never mutated — the frozen history is immutable and the certificate commits to it.
/// Replaying the legacy epoch through [`derive_authority_facts`] reproduces the frozen facts
/// exactly, because that is the function the frozen fold built them with; the cuts then take effect
/// on top, at epochs above every legacy one.
///
/// A refold reaches this with cuts to apply. The only operations that install a register are
/// revocations; `execute_held` is handed the view manifests the account holds, so a revocation
/// whose detached pre-cut manifest is stored applies and composes here, and one whose manifest is
/// missing parks and composes nothing
/// (`a_v2_revocation_parks_for_want_of_its_manifest_and_applies_once_it_is_stored`). What no
/// production path does is AUTHOR a v2 cut or a manifest: installing a pin is test-only.
///
/// With no cut applied the composed registers ARE the frozen ones, and **no frozen register scopes
/// a forked entry** — which is what makes the composition exact. `forked` only ever grows from
/// entries that were `effective` in some round (`newly_forked = effective.difference(&accepted)`),
/// and a register-condemned entry is never effective, so a register-scoped entry cannot reach the
/// fork stage at all. That leaves a forked entry as the only candidate in
/// [`FrozenLegacy::foldable`] the frozen history has no outcome for: a parked or readiness-excluded
/// entry carries its own, and an unfoldable entry never enters `foldable`.
///
/// The invariant depends on registers not GROWING across elimination rounds, and the residual is
/// that converse direction alone: removing a forked entry can let a previously-ineffective cut
/// become effective and install a register scoping an already-forked entry, and the overlay then
/// hands it a `Condemned` the frozen history lacks. That has not been constructed and is not worth
/// chasing — the effective set and the projection hash are untouched, so the only observable is an
/// `account_entry_status` label moving from `retained_unfolded` to `condemned` for such a row.
///
/// **Only a register-installing cut projects**, which splits an applied operation three ways.
///
/// A REVOCATION (`DeviceRemove` / `OwnerDemote`) carries registers and projects. That is the whole
/// reason the composition exists.
///
/// An operation that GRANTS authority — a v2 enrolment, promotion, ownership or grant — is
/// deliberately left out. Execution evaluates no state preconditions (see this module's header), so
/// projecting one unjudged would re-enroll a tombstoned device or promote an unenrolled one:
/// authority this account's own history never conferred. Omitting it can only UNDER-grant, which is
/// why the operational gates stay shut; what it cannot do is under-revoke.
///
/// A `CutExtend` falls out too, and silently: [`cut_op_registers`] returns nothing for it, so an
/// applied one reports `Applied` while raising no register here. v1 raises it separately through
/// [`cut_extend_register`], which this composition never calls, so a §11.4 re-blessing is dropped
/// and the register stays LOWER than its author meant. That over-revokes rather than under-revokes,
/// so the shut gates cover it — but do not read `Applied` as "took effect".
pub(in crate::account) fn pinned_history(
    frozen: &FrozenLegacy,
    applied: &[AppliedOperation<'_>],
) -> AccountAuthHistory {
    let mut cuts: Vec<&AppliedOperation<'_>> =
        applied.iter().filter(|op| !op.registers.is_empty()).collect();
    // Total and deterministic, and causal within one device's chain — the only order that matters,
    // since a cut's effect never depends on a cut authored on someone else's chain.
    cuts.sort_by_key(|op| (op.entry.header().seq, op.entry.hash()));

    let mut candidates = frozen.foldable();
    candidates.extend(cuts.iter().map(|op| op.entry.clone()));
    let headers: HashMap<AccountEntryHash, &AccountEntryHeader> = frozen
        .entries()
        .iter()
        .map(|entry| (entry.entry_hash, &entry.header))
        .chain(candidates.iter().map(|c| (c.hash(), c.header())))
        .collect();
    let view = CandidateView { headers: &headers };

    let mut registers = frozen.registers.clone();
    // Keys whose chain is closed. `Cut::Empty` alone cannot carry that state: it is the join's
    // BOTTOM (`join_cuts(Empty, other) -> Extended(other)`, §11.3), so ANY later pair for the same
    // key absorbs it and reinstates a watermark — the under-revocation closure exists to prevent.
    // Closure is a property of the KEY, not a value the register can hold.
    //
    // Seeded from the frozen set, not just from failures here: a legacy cut naming no entry freezes
    // its register at `Cut::Empty`, and without the seed ONE authorized pair carrying a fabricated
    // watermark would join `Applied` against it and reopen a chain the checkpoint closed. Seeding
    // is safe in this composition specifically because it raises no `CutExtend` register at all
    // (see this function's header), so there is no legitimate §11.4 re-blessing for it to
    // block. What it CAN block is a further revocation of a chain already at the bottom, where
    // `beyond` already holds for every seq — strictly weaker than what stands.
    //
    // The seed covers keys HELD at `Cut::Empty`, which is not the same set as "keys whose fact
    // reads Closed": `derive_authority_facts` also closes a fact whose key is ABSENT, and an absent
    // key is not seeded. That gap is unreachable while an effective remove always installs its
    // registers and an op with an undecidable register parks whole.
    let mut closed_keys: HashSet<RegisterKey> = registers
        .iter()
        .filter(|(_, cut)| matches!(cut, Cut::Empty))
        .map(|(key, _)| key.clone())
        .collect();
    for op in &cuts {
        for (key, cut) in op.registers {
            // A closed chain admits nothing further. This also swallows a `Contested` join on an
            // already-closed key; nothing reads one here today, and anything that starts to must
            // evaluate it before this skip.
            if closed_keys.contains(key) {
                continue;
            }
            // A join that is not `Applied` leaves the HELD register standing and drops the
            // newcomer's watermark — an under-revocation. Two authorized cuts disagreeing about one
            // chain's valid prefix is the compromise case, so close the chain instead: the empty
            // cut is the one answer that cannot admit an entry either author meant to
            // cut.
            if !matches!(
                join_register(&mut registers, key.clone(), cut.clone(), &view),
                RegisterJoin::Applied
            ) {
                registers.insert(key.clone(), Cut::Empty);
                closed_keys.insert(key.clone());
            }
        }
    }

    let mut outcomes = frozen.history.outcomes.clone();
    // The frozen epochs are normalized to `0..effective_count`, so the cuts take the slots above
    // them and `derive_authority_facts` replays the whole legacy epoch before any of them.
    for (offset, op) in cuts.iter().enumerate() {
        outcomes.insert(op.entry.hash(), Outcome::Effective {
            auth_epoch: frozen.history.effective_count + offset as u64,
        });
    }
    // A v2 cut may condemn what the legacy epoch left standing. This only ever downgrades, so it
    // can revive no branch loser, and the genesis is exempt for the reason `rederive_condemnation`
    // exempts it: a self-removal on the founder's chain would otherwise leave the account with no
    // effective root.
    for candidate in &candidates {
        if Some(candidate.hash()) == frozen.history.genesis_hash {
            continue;
        }
        if let RegisterVerdict::Condemned(reason) = register_verdict(candidate, &registers, &view) {
            outcomes.insert(candidate.hash(), Outcome::Condemned(reason));
        }
    }
    let effective_count = normalize_auth_epochs(&mut outcomes);

    let mut tombstoned = frozen.history.tombstoned.clone();
    for op in &cuts {
        // An effective removal tombstones the device (I4: never re-enroll), as `apply_effect` does
        // for v1. A cut its own registers condemned removed nothing.
        if let AccountOp::DeviceRemove { device_fingerprint, .. } = &op.entry.op
            && outcomes.get(&op.entry.hash()).is_some_and(Outcome::is_effective)
        {
            tombstoned.insert(*device_fingerprint);
        }
    }

    let facts = derive_authority_facts(&candidates, &outcomes, &registers);
    AccountAuthHistory {
        outcomes,
        classification: frozen.history.classification,
        contested_successor: frozen.history.contested_successor,
        effective_count,
        roster_refs: facts.roster_refs,
        owner_incarnations: facts.owner_incarnations,
        stream_ownership: facts.stream_ownership,
        grants: facts.grants,
        grant_cuts: facts.grant_cuts,
        tombstoned,
        genesis_hash: frozen.history.genesis_hash,
    }
}

/// The credit `input.cut` earns under `scope`, over one shared baseline. Production only ever asks
/// for the nomination scope; the unrestricted scope is what the v1 rule would have counted over
/// identical inputs, which is how the subset property is checked rather than asserted.
fn credit_under(input: &CutExecution<'_>, scope: CreditScope<'_>) -> u64 {
    let Some(genesis) = input.frozen.history.genesis_hash() else {
        return 0;
    };
    let registers: HashMap<RegisterKey, Cut> =
        cut_op_registers(input.cut).into_iter().map(|(key, cut, _)| (key, cut)).collect();
    if registers.is_empty() {
        return 0;
    }

    let legacy = input.frozen.foldable();
    let legacy_count = legacy.len();
    let mut candidates = legacy;
    candidates.extend(input.nominated.iter().cloned());
    candidates.push(input.cut.clone());

    let mut headers: HashMap<AccountEntryHash, &AccountEntryHeader> =
        input.frozen.entries.iter().map(|entry| (entry.entry_hash, &entry.header)).collect();
    for candidate in &candidates[legacy_count..] {
        headers.insert(candidate.hash(), candidate.header());
    }
    let view = CandidateView { headers: &headers };

    let mut incarnations = Incarnations::build(&candidates, genesis.into());
    let mut strata: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (idx, candidate) in candidates.iter().enumerate() {
        if let Some(depth) = incarnations.author_depth(candidate) {
            strata.entry(depth).or_default().push(idx);
        }
    }

    // Legacy outcomes come from the checkpoint verbatim.
    let mut outcomes: HashMap<AccountEntryHash, Outcome> = HashMap::new();
    for candidate in &candidates[..legacy_count] {
        if let Some(outcome) = input.frozen.history.outcome(&candidate.hash()) {
            outcomes.insert(candidate.hash(), outcome);
        }
    }
    // A nominated entry is effective only if the ONE authority resolution admitted it. Being signed
    // by a certified key and sitting on a chain that reaches an accepted branch establishes WHO
    // wrote it and WHERE, never that it was allowed to.
    let mut unauthorized: HashSet<AccountEntryHash> = HashSet::new();
    for candidate in &candidates[legacy_count..] {
        let verdict = input.authority.verdict(&candidate.hash());
        if verdict == V2Verdict::Authorized {
            outcomes.insert(candidate.hash(), Outcome::Effective { auth_epoch: 0 });
            continue;
        }
        unauthorized.insert(candidate.hash());
        // Only a mint that is merely no longer live leaves a stale dependent the credit rule's
        // second loop may count; an impersonator never does.
        let reason = if verdict == V2Verdict::WrongDevice {
            RejectReason::WrongDevice
        } else {
            RejectReason::StaleAuthority
        };
        outcomes.insert(candidate.hash(), Outcome::Rejected(reason));
    }

    // Only the cut's OWN registers decide what it took away — the v1 rule. Another cut in the same
    // view never widens this one's scope; it only moves outcomes, which is precisely why the
    // guarantee is an upper bound on identities rather than a fixed number.
    for candidate in &candidates {
        // The genesis is the account's ROOT axiom and is never condemnable. v1 exempts it in
        // `rederive_condemnation` for the same reason, and without the exemption a self-removal on
        // the founder's chain credits the account's own root — over-crediting, the unsafe
        // direction, by an entry that is neither nominated nor a stale dependent.
        //
        // An entry that never held authority was never in the effective count, so condemning it
        // takes nothing away. The v1 overlay would promote it to `Condemned` anyway — condemnation
        // outranks a stale-authority rejection there — and hand this cut a credit for an entry its
        // own author never counted. Holding it out keeps the credit at or below the v1 rule.
        if candidate.hash() == genesis
            || candidate.hash() == input.cut.hash()
            || unauthorized.contains(&candidate.hash())
        {
            continue;
        }
        if let RegisterVerdict::Condemned(reason) = register_verdict(candidate, &registers, &view) {
            outcomes.insert(candidate.hash(), Outcome::Condemned(reason));
        }
    }

    // Ascending depth: a dependent is always deeper than the mint it cites, so one pass settles the
    // ops left without authority by a mint this cut condemned.
    for &idx in strata.values().flatten() {
        let candidate = &candidates[idx];
        if !outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective) {
            continue;
        }
        let stale = incarnations.author_incarnation_id(candidate).is_some_and(|incarnation| {
            matches!(
                outcomes.get(&incarnation.into()),
                Some(Outcome::Condemned(_) | Outcome::Rejected(RejectReason::StaleAuthority))
            )
        });
        if stale {
            outcomes.insert(candidate.hash(), Outcome::Rejected(RejectReason::StaleAuthority));
        }
    }

    revocation_credit(&candidates, &strata, &incarnations, &outcomes, input.cut, scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::checkpoint::{self, TrustedCheckpointPin, VerifiedCheckpoint};
    use crate::account::control_v2::ops as v2_ops;
    use crate::account::test_support::Dev;
    use crate::account::{envelope, storage};
    use crate::identity::LocalDevice;

    #[test]
    fn frozen_legacy_comes_from_final_branch_closure_not_raw_fold_counts() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = crate::local_account(&conn, 1).unwrap();
        let device = crate::local_device(&conn, 1).unwrap();
        let genesis =
            storage::account_entries_for_enrollment(&conn, account).unwrap()[0].entry_hash;
        for seed in [2, 3] {
            let other = Dev::new(seed);
            let op = AccountOp::DeviceAdd {
                device_fingerprint: other.fp,
                ed25519_pubkey: other.ed,
                x25519_pubkey: other.x,
                role: DeviceRole::Member,
                label: None,
            };
            let signed = envelope::sign_account_entry(
                device.secret(),
                &AccountEntryHeader {
                    account_id: account,
                    log_id: 0,
                    device_fingerprint: device.fingerprint(),
                    seq: 1,
                    prev_hash: Some(genesis),
                    parent_ref: Some(genesis),
                    entry_type: ops::entry_type_of(&op),
                    op_version: 1,
                    crypto_suite: 0,
                    auth_len: 1,
                    key_id: None,
                    authority_ref: Some(genesis.into()),
                },
                &ops::encode(&op).unwrap(),
            )
            .unwrap();
            storage::account_ingest(&conn, &signed.signed_bytes, 1).unwrap();
        }
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
        let frozen = proof.frozen_legacy();
        assert_eq!(fold_account(&frozen.entries).effective_count(), 3);
        assert_eq!(frozen.history.effective_count(), 2);
        assert_eq!(frozen.accepted.len(), 2);
        assert_eq!(proof.forked_legacy_entries().count(), 1);
    }

    /// The frozen registers are the only fold state v2 execution reads, and they come from the
    /// FINAL coherent pass. At `auth_len` 1 an ineffective legacy removal still installs one;
    /// at 999 the same removal is held out as authored ahead of the fold and installs none.
    /// What separates the two folds is therefore a verdict on a v2 continuation of the cut
    /// chain — the same entry either way — and not a captured value only this test would ever
    /// read.
    #[test]
    fn a_frozen_register_condemns_a_v2_continuation_of_the_cut_chain() {
        for (auth_len, expected) in [(1, V2Verdict::Condemned), (999, V2Verdict::Authorized)] {
            let mut conn = rusqlite::Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            let account = crate::local_account(&conn, 1).unwrap();
            let device = crate::local_device(&conn, 1).unwrap();
            let genesis =
                storage::account_entries_for_enrollment(&conn, account).unwrap()[0].entry_hash;
            let subject = Dev::new(7);
            let op = AccountOp::DeviceRemove {
                device_fingerprint: subject.fp,
                control_cut: Cut::Empty,
                secrets_cut: Cut::Empty,
                content_cuts: vec![],
                reason: "never enrolled".into(),
            };
            let removal = envelope::sign_account_entry(
                device.secret(),
                &AccountEntryHeader {
                    account_id: account,
                    log_id: 0,
                    device_fingerprint: device.fingerprint(),
                    seq: 1,
                    prev_hash: Some(genesis),
                    parent_ref: Some(genesis),
                    entry_type: ops::entry_type_of(&op),
                    op_version: 1,
                    crypto_suite: 0,
                    auth_len,
                    key_id: None,
                    authority_ref: Some(genesis.into()),
                },
                &ops::encode(&op).unwrap(),
            )
            .unwrap();
            storage::account_ingest(&conn, &removal.signed_bytes, 1).unwrap();
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
            let frozen = proof.frozen_legacy();
            // Removing a device that was never enrolled is ineffective, so neither fold accepts the
            // removal itself: the register it left behind is the whole difference.
            assert!(!frozen.accepted.contains(&removal.entry_hash));

            // v2 mints the subject a live incarnation. State preconditions are not evaluated here,
            // so this promote is authorized whether or not the subject is on the roster — which
            // leaves the frozen register as the only thing that can keep its chain out.
            let promote = author_on_founder_chain(
                &proof,
                &device,
                genesis.into(),
                2,
                removal.entry_hash,
                &AccountOp::OwnerPromote { device_fingerprint: subject.fp },
                v2_ops::CONTROL_VERSION,
            );
            let other = Dev::new(8);
            let enrol = v2_ops::ControlOp {
                checkpoint: proof.pin().checkpoint_digest,
                pre_cut_view: None,
                op: AccountOp::DeviceAdd {
                    device_fingerprint: other.fp,
                    ed25519_pubkey: other.ed,
                    x25519_pubkey: other.x,
                    role: DeviceRole::Member,
                    label: None,
                },
            };
            let signed = envelope::sign_account_entry(
                &subject.secret,
                &AccountEntryHeader {
                    account_id: account,
                    log_id: 0,
                    device_fingerprint: subject.fp,
                    seq: 0,
                    prev_hash: None,
                    parent_ref: None,
                    entry_type: ops::entry_type_of(&enrol.op),
                    op_version: v2_ops::CONTROL_VERSION,
                    crypto_suite: 0,
                    auth_len: 1,
                    key_id: None,
                    authority_ref: Some(promote.hash().into()),
                },
                &enrol.encode().unwrap(),
            )
            .unwrap();
            let continuation = Candidate::new(
                VerifiedAccountEntry {
                    header: signed.header,
                    payload: signed.payload,
                    entry_hash: signed.entry_hash,
                },
                enrol.op,
            );

            let authority = V2Authority::resolve(frozen, &[promote.clone(), continuation.clone()]);
            assert_eq!(
                authority.verdict(&promote.hash()),
                V2Verdict::Authorized,
                "the mint itself is on a chain no register scopes",
            );
            assert_eq!(authority.verdict(&continuation.hash()), expected);
        }
    }

    /// A legacy epoch in which a demoted owner's chain already lost an entry: `B` authored two ops
    /// under the incarnation the founder then demoted at `B`'s seq 0, so `B`'s seq 1 is condemned
    /// and NOT accepted while its seq 0 survives. A later whole-device cut of `B` therefore scopes
    /// one entry the checkpoint was still counting and one it was not.
    struct DemotedOwner {
        checkpoint: VerifiedCheckpoint,
        founder: LocalDevice,
        /// The founder's live owner incarnation.
        incarnation: OwnerId,
        subject: Dev,
        accepted_victim: AccountEntryHash,
        condemned_victim: AccountEntryHash,
    }

    fn demoted_owner() -> DemotedOwner {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = crate::local_account(&conn, 1).unwrap();
        let founder = crate::local_device(&conn, 1).unwrap();
        let genesis =
            storage::account_entries_for_enrollment(&conn, account).unwrap()[0].entry_hash;
        let subject = Dev::new(11);

        let author = |signer: &crate::device::DeviceSecret,
                      seq: u64,
                      prev: AccountEntryHash,
                      authority: OwnerId,
                      op: &AccountOp| {
            let signed = envelope::sign_account_entry(
                signer,
                &AccountEntryHeader {
                    account_id: account,
                    log_id: 0,
                    device_fingerprint: signer.public().fingerprint(),
                    seq,
                    prev_hash: (seq != 0).then_some(prev),
                    parent_ref: Some(genesis),
                    entry_type: ops::entry_type_of(op),
                    op_version: 1,
                    crypto_suite: 0,
                    auth_len: 1,
                    key_id: None,
                    authority_ref: Some(authority),
                },
                &ops::encode(op).unwrap(),
            )
            .unwrap();
            storage::account_ingest(&conn, &signed.signed_bytes, 1).unwrap();
            signed.entry_hash
        };

        let enroll = AccountOp::DeviceAdd {
            device_fingerprint: subject.fp,
            ed25519_pubkey: subject.ed,
            x25519_pubkey: subject.x,
            role: DeviceRole::Owner,
            label: None,
        };
        let subject_incarnation: OwnerId =
            author(founder.secret(), 1, genesis, genesis.into(), &enroll).into();
        let member = |seed: u8| {
            let other = Dev::new(seed);
            AccountOp::DeviceAdd {
                device_fingerprint: other.fp,
                ed25519_pubkey: other.ed,
                x25519_pubkey: other.x,
                role: DeviceRole::Member,
                label: None,
            }
        };
        let accepted_victim = author(&subject.secret, 0, genesis, subject_incarnation, &member(12));
        let condemned_victim =
            author(&subject.secret, 1, accepted_victim, subject_incarnation, &member(13));
        let demote = author(founder.secret(), 2, subject_incarnation.into(), genesis.into(), &{
            AccountOp::OwnerDemote {
                device_fingerprint: subject.fp,
                owner_id: subject_incarnation,
                control_cut: Cut::At { seq: 0, hash: accepted_victim },
                secrets_cut: Cut::Empty,
                reason: "demoted".into(),
            }
        });
        // A second open owner, so a later cut of the founder is not the I2 last-owner case. It sits
        // on the founder's chain, which the cut fixtures below do not scope.
        let second = Dev::new(41);
        author(founder.secret(), 3, demote, genesis.into(), &AccountOp::DeviceAdd {
            device_fingerprint: second.fp,
            ed25519_pubkey: second.ed,
            x25519_pubkey: second.x,
            role: DeviceRole::Owner,
            label: None,
        });

        let tx = conn.transaction().unwrap();
        let bundle = checkpoint::prepare_checkpoint_in_tx(&tx, account, &founder).unwrap();
        let checkpoint = checkpoint::verify_checkpoint(
            TrustedCheckpointPin {
                account_id: account,
                checkpoint_digest: bundle.certificate_digest(),
                required_control_version: 2,
            },
            &bundle,
        )
        .unwrap();
        drop(tx);
        DemotedOwner {
            checkpoint,
            founder,
            incarnation: genesis.into(),
            subject,
            accepted_victim,
            condemned_victim,
        }
    }

    /// The whole-device v2 cut of `B`, plus a nominated v2 entry on the founder's own chain that
    /// the cut's registers do not scope.
    fn v2_cut(fixture: &DemotedOwner, nominate_unrelated: bool) -> (Candidate, Vec<Candidate>) {
        let checkpoint = &fixture.checkpoint;
        let tip = checkpoint
            .continuation_heads()
            .iter()
            .find(|head| head.device_fingerprint == fixture.founder.fingerprint())
            .unwrap()
            .clone();
        let sign = |seq: u64, prev: AccountEntryHash, op: v2_ops::ControlOp| {
            let signed = envelope::sign_account_entry(
                fixture.founder.secret(),
                &AccountEntryHeader {
                    account_id: checkpoint.pin().account_id,
                    log_id: 0,
                    device_fingerprint: fixture.founder.fingerprint(),
                    seq,
                    prev_hash: Some(prev),
                    parent_ref: Some(prev),
                    entry_type: ops::entry_type_of(&op.op),
                    op_version: v2_ops::CONTROL_VERSION,
                    crypto_suite: 0,
                    auth_len: 1,
                    key_id: None,
                    authority_ref: Some(fixture.incarnation),
                },
                &op.encode().unwrap(),
            )
            .unwrap();
            let verified = VerifiedAccountEntry {
                header: signed.header,
                payload: signed.payload,
                entry_hash: signed.entry_hash,
            };
            Candidate::new(verified, op.op)
        };
        let unrelated = Dev::new(21);
        let extra = sign(tip.seq + 1, tip.hash, v2_ops::ControlOp {
            checkpoint: checkpoint.pin().checkpoint_digest,
            pre_cut_view: None,
            op: AccountOp::DeviceAdd {
                device_fingerprint: unrelated.fp,
                ed25519_pubkey: unrelated.ed,
                x25519_pubkey: unrelated.x,
                role: DeviceRole::Member,
                label: None,
            },
        });
        let cut = sign(tip.seq + 2, extra.hash(), v2_ops::ControlOp {
            checkpoint: checkpoint.pin().checkpoint_digest,
            pre_cut_view: Some([7; 32]),
            op: AccountOp::DeviceRemove {
                device_fingerprint: fixture.subject.fp,
                control_cut: Cut::Empty,
                secrets_cut: Cut::Empty,
                content_cuts: vec![],
                reason: "revoked".into(),
            },
        });
        (cut, if nominate_unrelated { vec![extra] } else { Vec::new() })
    }

    /// Execute `cut` exactly as the executor does: ONE authority resolution over the bundle, then
    /// `apply_cut` reading that same resolution. A test must not be able to hand `apply_cut` an
    /// authority the executor would never have derived.
    fn execute(frozen: &FrozenLegacy, nominated: &[Candidate], cut: &Candidate) -> CutOutcome {
        let bundle: Vec<Candidate> =
            nominated.iter().cloned().chain(std::iter::once(cut.clone())).collect();
        let authority = V2Authority::resolve(frozen, &bundle);
        apply_cut(CutExecution { frozen, authority: &authority, nominated, cut })
    }

    fn applied(frozen: &FrozenLegacy, nominated: &[Candidate], cut: &Candidate) -> AppliedCut {
        match execute(frozen, nominated, cut) {
            CutOutcome::Applied(applied) => applied,
            CutOutcome::Rejected(reason) => panic!("unexpectedly rejected: {reason:?}"),
            CutOutcome::Parked(reason) => panic!("unexpectedly parked: {reason:?}"),
        }
    }

    fn founder_tip(fixture: &DemotedOwner) -> DeviceCut {
        fixture
            .checkpoint
            .continuation_heads()
            .iter()
            .find(|head| head.device_fingerprint == fixture.founder.fingerprint())
            .unwrap()
            .clone()
    }

    /// Author `op` on the founder's chain at `version`. v2 wraps the operation in the control v2
    /// payload; v1 encodes it directly, which is what a real fold can judge.
    fn author_on_founder_chain(
        checkpoint: &VerifiedCheckpoint,
        founder: &LocalDevice,
        incarnation: OwnerId,
        seq: u64,
        prev: AccountEntryHash,
        op: &AccountOp,
        version: u32,
    ) -> Candidate {
        let revocation =
            matches!(op, AccountOp::DeviceRemove { .. } | AccountOp::OwnerDemote { .. });
        let payload = if version == v2_ops::CONTROL_VERSION {
            v2_ops::ControlOp {
                checkpoint: checkpoint.pin().checkpoint_digest,
                pre_cut_view: revocation.then_some([9; 32]),
                op: op.clone(),
            }
            .encode()
            .unwrap()
        } else {
            ops::encode(op).unwrap()
        };
        let signed = envelope::sign_account_entry(
            founder.secret(),
            &AccountEntryHeader {
                account_id: checkpoint.pin().account_id,
                log_id: 0,
                device_fingerprint: founder.fingerprint(),
                seq,
                prev_hash: Some(prev),
                parent_ref: Some(prev),
                entry_type: ops::entry_type_of(op),
                op_version: version,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: Some(incarnation),
            },
            &payload,
        )
        .unwrap();
        Candidate::new(
            VerifiedAccountEntry {
                header: signed.header,
                payload: signed.payload,
                entry_hash: signed.entry_hash,
            },
            op.clone(),
        )
    }

    /// What a REAL v1 fold credits this cut: the same counting loops, but over the outcome map
    /// `fold_account` derives rather than the one execution constructs. This is the oracle — where
    /// the documented bound and the code diverge, it shows up here as a number.
    fn v1_credit(frozen: &FrozenLegacy, twin: &Candidate) -> u64 {
        let mut entries = frozen.entries().to_vec();
        entries.push(twin.entry.clone());
        let (history, trace) = fold_account_traced(&entries, true);
        // v1 computes a credit only for the cuts that actually installed a register. A cut it
        // refused is not a contributor and is credited nothing at all, so gating on that set is
        // what makes this the credit v1 GIVES rather than what the rule would compute if asked.
        if !trace.is_some_and(|trace| trace.contributors.contains(&twin.hash())) {
            return 0;
        }
        let Some(genesis) = history.genesis_hash() else {
            return 0;
        };
        let candidates: Vec<Candidate> = entries
            .iter()
            .filter(|entry| {
                entry.header.log_id == CONTROL_LOG
                    && entry.header.op_version == SUPPORTED_OP_VERSION
                    && entry.header.crypto_suite == 0
            })
            .filter_map(|entry| match ops::decode(entry.header.entry_type, &entry.payload) {
                Ok(DecodedAccountOp::Known(op)) => Some(Candidate::new(entry.clone(), op)),
                _ => None,
            })
            .collect();
        let mut incarnations = Incarnations::build(&candidates, genesis.into());
        let mut strata: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (idx, candidate) in candidates.iter().enumerate() {
            if let Some(depth) = incarnations.author_depth(candidate) {
                strata.entry(depth).or_default().push(idx);
            }
        }
        let outcomes: HashMap<AccountEntryHash, Outcome> = candidates
            .iter()
            .filter_map(|c| history.outcome(&c.hash()).map(|outcome| (c.hash(), outcome)))
            .collect();
        revocation_credit(
            &candidates,
            &strata,
            &incarnations,
            &outcomes,
            twin,
            CreditScope::EveryScopedEntry,
        )
    }

    #[test]
    fn executed_credit_never_exceeds_a_real_v1_fold_of_the_same_operation() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (cut, nominated) = v2_cut(&fixture, false);
        let executed = applied(frozen, &nominated, &cut);

        let tip = founder_tip(&fixture);
        let twin = author_on_founder_chain(
            &fixture.checkpoint,
            &fixture.founder,
            fixture.incarnation,
            tip.seq + 1,
            tip.hash,
            &cut.op,
            SUPPORTED_OP_VERSION,
        );
        let oracle = v1_credit(frozen, &twin);

        assert_eq!(oracle, 2, "v1 counts both of the subject's entries");
        assert_eq!(executed.credit, 1, "a legacy branch already out of the count earns nothing");
        assert!(executed.credit <= oracle, "execution must not out-credit a real v1 fold");
        assert_eq!(executed.registers.len(), 2, "a device remove cuts control and secrets");
        assert!(frozen.accepted_at_checkpoint(&fixture.accepted_victim));
        assert!(!frozen.accepted_at_checkpoint(&fixture.condemned_victim));
    }

    /// The I2 last-owner guard and the genesis root axiom together. A sole owner removing itself
    /// installs nothing: v1 rejects it `LastOwner`, never makes it a register contributor, and so
    /// credits it nothing — including nothing for the account's own genesis, which its empty cut
    /// would otherwise condemn.
    #[test]
    fn a_sole_owners_self_removal_installs_nothing_as_in_v1() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = crate::local_account(&conn, 1).unwrap();
        let founder = crate::local_device(&conn, 1).unwrap();
        let genesis =
            storage::account_entries_for_enrollment(&conn, account).unwrap()[0].entry_hash;
        let tx = conn.transaction().unwrap();
        let bundle = checkpoint::prepare_checkpoint_in_tx(&tx, account, &founder).unwrap();
        let proof = checkpoint::verify_checkpoint(
            TrustedCheckpointPin {
                account_id: account,
                checkpoint_digest: bundle.certificate_digest(),
                required_control_version: 2,
            },
            &bundle,
        )
        .unwrap();
        drop(tx);

        let op = AccountOp::DeviceRemove {
            device_fingerprint: founder.fingerprint(),
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: vec![],
            reason: "self".into(),
        };
        let frozen = proof.frozen_legacy();
        let cut = author_on_founder_chain(
            &proof,
            &founder,
            genesis.into(),
            1,
            genesis,
            &op,
            v2_ops::CONTROL_VERSION,
        );
        assert!(matches!(
            execute(frozen, &[], &cut),
            CutOutcome::Rejected(RejectReason::LastOwner)
        ));
        let twin = author_on_founder_chain(
            &proof,
            &founder,
            genesis.into(),
            1,
            genesis,
            &op,
            SUPPORTED_OP_VERSION,
        );
        assert_eq!(v1_credit(frozen, &twin), 0, "v1 credits a refused cut nothing");
    }

    /// THE residual, demonstrated rather than argued. State preconditions are not evaluated, so an
    /// entry the v1 fold would reject `Ineffective` still earns a credit once the cut condemns it.
    /// The excess is exactly one per nominated entry the cut's own registers scope.
    #[test]
    fn a_nominated_entry_v1_would_reject_ineffective_is_still_counted() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let tip = founder_tip(&fixture);
        // Re-enrolling a device already on the roster: `classify_effect` rejects this
        // `DuplicateAdd`, so a real v1 fold would never credit it.
        let duplicate = author_on_founder_chain(
            &fixture.checkpoint,
            &fixture.founder,
            fixture.incarnation,
            tip.seq + 1,
            tip.hash,
            &AccountOp::DeviceAdd {
                device_fingerprint: fixture.subject.fp,
                ed25519_pubkey: fixture.subject.ed,
                x25519_pubkey: fixture.subject.x,
                role: DeviceRole::Member,
                label: None,
            },
            v2_ops::CONTROL_VERSION,
        );
        let cut = author_on_founder_chain(
            &fixture.checkpoint,
            &fixture.founder,
            fixture.incarnation,
            tip.seq + 2,
            duplicate.hash(),
            &AccountOp::DeviceRemove {
                device_fingerprint: fixture.founder.fingerprint(),
                control_cut: Cut::Empty,
                secrets_cut: Cut::Empty,
                content_cuts: vec![],
                reason: "revoked".into(),
            },
            v2_ops::CONTROL_VERSION,
        );

        let without = applied(frozen, &[], &cut);
        let with = applied(frozen, std::slice::from_ref(&duplicate), &cut);
        assert_eq!(
            with.credit,
            without.credit + 1,
            "an ineffective nomination the cut scopes is worth exactly one over-count",
        );
    }

    #[test]
    fn nominating_entries_the_cut_never_took_does_not_inflate_its_credit() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (cut, nominated) = v2_cut(&fixture, true);
        assert_eq!(nominated.len(), 1, "an entry on a chain this cut does not scope");
        let bare = v2_cut(&fixture, false);
        let baseline = applied(frozen, &bare.1, &bare.0);
        let with = applied(frozen, &nominated, &cut);
        // Nomination is weaker than proof of loss: naming an entry earns nothing unless the cut's
        // own registers actually condemn it.
        assert_eq!(with.credit, baseline.credit);
        assert_eq!(with.credit, 1);
    }

    /// The transitive half of the authority rule, and the only guard on it. This mint cites the
    /// founder's LIVE incarnation, so `author_depth` resolves and the mint is genuinely VISITED —
    /// unlike a mint citing nothing, which `verdict`'s absent-entry default refuses whether or not
    /// the rule exists. `WrongDevice` is a verdict that default cannot produce, so this fails the
    /// moment a mint is allowed to certify before it is itself authorized.
    #[test]
    fn a_visited_mint_certifies_nothing_until_it_is_itself_authorized() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let member = Dev::new(12);
        let sign =
            |seq: u64, prev: Option<AccountEntryHash>, incarnation: OwnerId, op: AccountOp| {
                let revocation = matches!(op, AccountOp::DeviceRemove { .. });
                let payload = v2_ops::ControlOp {
                    checkpoint: fixture.checkpoint.pin().checkpoint_digest,
                    pre_cut_view: revocation.then_some([9; 32]),
                    op: op.clone(),
                }
                .encode()
                .unwrap();
                let signed = envelope::sign_account_entry(
                    &member.secret,
                    &AccountEntryHeader {
                        account_id: fixture.checkpoint.pin().account_id,
                        log_id: 0,
                        device_fingerprint: member.fp,
                        seq,
                        prev_hash: prev,
                        parent_ref: prev,
                        entry_type: ops::entry_type_of(&op),
                        op_version: v2_ops::CONTROL_VERSION,
                        crypto_suite: 0,
                        auth_len: 1,
                        key_id: None,
                        authority_ref: Some(incarnation),
                    },
                    &payload,
                )
                .unwrap();
                Candidate::new(
                    VerifiedAccountEntry {
                        header: signed.header,
                        payload: signed.payload,
                        entry_hash: signed.entry_hash,
                    },
                    op,
                )
            };
        let mint = sign(0, None, fixture.incarnation, AccountOp::OwnerPromote {
            device_fingerprint: member.fp,
        });
        let cut = sign(1, Some(mint.hash()), mint.hash().into(), AccountOp::DeviceRemove {
            device_fingerprint: fixture.founder.fingerprint(),
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: vec![],
            reason: "seized".into(),
        });

        let authority = V2Authority::resolve(frozen, &[mint.clone(), cut.clone()]);
        // Refused on its own merits rather than by the absent-entry default: the incarnation it
        // cites was minted for the founder, not for this signer.
        assert_eq!(authority.verdict(&mint.hash()), V2Verdict::WrongDevice);
        // And a mint that was itself refused certifies nothing that cites it.
        assert_eq!(authority.verdict(&cut.hash()), V2Verdict::Inadmissible);
        // `apply_cut` enforces that same answer itself rather than trusting its caller to.
        assert!(matches!(
            apply_cut(CutExecution { frozen, authority: &authority, nominated: &[], cut: &cut }),
            CutOutcome::Rejected(_)
        ));
    }

    /// Being certified and on an accepted branch is not authority. The subject's key is certified
    /// by the accepted legacy epoch and its chain reaches an accepted entry, so it passes both the
    /// signature and the ancestry gate — but it holds no live incarnation, and an entry it signs
    /// citing the founder's incarnation is impersonation the fold rejects outright.
    #[test]
    fn a_nomination_does_not_buy_admission_for_an_unauthorized_entry() {
        let fixture = demoted_owner();
        let (cut, _) = v2_cut(&fixture, false);
        let enrol = {
            let other = Dev::new(31);
            AccountOp::DeviceAdd {
                device_fingerprint: other.fp,
                ed25519_pubkey: other.ed,
                x25519_pubkey: other.x,
                role: DeviceRole::Member,
                label: None,
            }
        };
        let op = v2_ops::ControlOp {
            checkpoint: fixture.checkpoint.pin().checkpoint_digest,
            pre_cut_view: None,
            op: enrol,
        };
        // Cites the FOUNDER's live incarnation, which no legacy register bounds, so the only thing
        // that can keep this out of the count is the mint naming a different device than the
        // signer.
        let signed = envelope::sign_account_entry(
            &fixture.subject.secret,
            &AccountEntryHeader {
                account_id: fixture.checkpoint.pin().account_id,
                log_id: 0,
                device_fingerprint: fixture.subject.fp,
                seq: 2,
                prev_hash: Some(fixture.condemned_victim),
                parent_ref: Some(fixture.condemned_victim),
                entry_type: ops::entry_type_of(&op.op),
                op_version: v2_ops::CONTROL_VERSION,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: Some(fixture.incarnation),
            },
            &op.encode().unwrap(),
        )
        .unwrap();
        let unauthorized = Candidate::new(
            VerifiedAccountEntry {
                header: signed.header,
                payload: signed.payload,
                entry_hash: signed.entry_hash,
            },
            op.op,
        );

        let frozen = fixture.checkpoint.frozen_legacy();
        let without = applied(frozen, &[], &cut);
        let with = applied(frozen, std::slice::from_ref(&unauthorized), &cut);
        // It sits on the revoked device's chain and the cut's device register condemns everything
        // there, so it would be worth a credit the moment it were treated as effective.
        assert_eq!(with.credit, without.credit, "a nomination never substitutes for admission");
        assert_eq!(with.credit, 1);
    }

    /// The two ways a citation fails to resolve, told apart on ONE entry: the same operation citing
    /// the same hash, differing only in whether that object was supplied. An entry we hold that is
    /// not a mint never becomes one, so that refusal is final; an object nothing supplied may
    /// simply be withheld, which a single bundle cannot distinguish from never having existed.
    #[test]
    fn an_unresolvable_citation_parks_only_when_the_object_it_names_is_not_held() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (_, nominated) = v2_cut(&fixture, true);
        let non_mint = nominated[0].clone();
        let other = Dev::new(51);
        let citing = author_on_founder_chain(
            &fixture.checkpoint,
            &fixture.founder,
            non_mint.hash().into(),
            founder_tip(&fixture).seq + 2,
            non_mint.hash(),
            &AccountOp::DeviceAdd {
                device_fingerprint: other.fp,
                ed25519_pubkey: other.ed,
                x25519_pubkey: other.x,
                role: DeviceRole::Member,
                label: None,
            },
            v2_ops::CONTROL_VERSION,
        );

        // The cited entry is here, and it is a `DeviceAdd` of a Member: no evidence makes it a
        // mint, so nothing about this operation is outstanding.
        let held = V2Authority::resolve(frozen, &[non_mint, citing.clone()]);
        assert_eq!(held.verdict(&citing.hash()), V2Verdict::Inadmissible);
        assert!(matches!(
            apply_cut(CutExecution { frozen, authority: &held, nominated: &[], cut: &citing }),
            CutOutcome::Rejected(RejectReason::StaleAuthority)
        ));

        let withheld = V2Authority::resolve(frozen, std::slice::from_ref(&citing));
        assert_eq!(withheld.verdict(&citing.hash()), V2Verdict::MintNotSupplied);
        assert!(matches!(
            apply_cut(CutExecution { frozen, authority: &withheld, nominated: &[], cut: &citing }),
            CutOutcome::Parked(ParkReason::UnknownOwnerRef)
        ));
    }

    /// The roster fact the history holds for `device`, whatever its state.
    fn roster_fact_for(
        history: &AccountAuthHistory,
        device: DeviceFingerprint,
    ) -> Option<(&RosterRef, &RosterFact)> {
        history.roster_facts().find(|(_, fact)| fact.authority.device_fingerprint == device)
    }

    fn projection_hash(history: &AccountAuthHistory) -> [u8; 32] {
        crate::account::annex::projection::folded_state_hash(history)
    }

    /// Composing NO v2 operation must reproduce the frozen fold exactly, byte for byte in the
    /// canonical projection. `derive_authority_facts` is the function the frozen fold built its own
    /// facts with, so replaying the legacy epoch through it can only agree — if this fails, the
    /// composition is re-deriving something the checkpoint already decided.
    #[test]
    fn composing_no_v2_operation_reproduces_the_frozen_projection() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let composed = pinned_history(frozen, &[]);
        assert_eq!(projection_hash(&composed), projection_hash(frozen.history()));
        // The canonical projection encodes only `effective_entries()`, so the hash alone would not
        // notice a candidate gaining or losing a NON-effective outcome. Compare the map too.
        assert_eq!(
            composed.outcomes,
            frozen.history().outcomes,
            "every candidate keeps the exact outcome the checkpoint decided for it",
        );
    }

    /// The genesis is the account's ROOT AXIOM and survives a cut over its own chain. A founder
    /// removal installs a device register with an empty cut, which puts EVERY seq on the founder's
    /// chain beyond the watermark — seq 0 included. Without the exemption the account folds `Live`
    /// with no effective root, which is why `rederive_condemnation` carries the same carve-out.
    #[test]
    fn a_cut_on_the_founders_own_chain_never_condemns_the_genesis_root() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let genesis = frozen.history().genesis_hash().expect("the fixture has a root");
        let tip = founder_tip(&fixture);
        // A second owner is open, so the I2 last-owner guard does not refuse this.
        let cut = author_on_founder_chain(
            &fixture.checkpoint,
            &fixture.founder,
            fixture.incarnation,
            tip.seq + 1,
            tip.hash,
            &AccountOp::DeviceRemove {
                device_fingerprint: fixture.founder.fingerprint(),
                control_cut: Cut::Empty,
                secrets_cut: Cut::Empty,
                content_cuts: vec![],
                reason: "revoked".into(),
            },
            v2_ops::CONTROL_VERSION,
        );
        let outcome = applied(frozen, &[], &cut);
        let history = pinned_history(frozen, &[AppliedOperation {
            entry: &cut,
            registers: &outcome.registers,
        }]);
        // Non-vacuous: the register really does scope the founder's own chain.
        assert!(
            matches!(history.outcome(&tip.hash), Some(Outcome::Condemned(_))),
            "the founder's later entries are condemned by its own removal",
        );
        assert!(
            history.outcome(&genesis).is_some_and(|o| o.is_effective()),
            "the genesis root axiom is never condemnable",
        );
    }

    /// A register-scoped equivocation is condemned by the FOLD, before branch selection runs, so
    /// its siblings never reach the fork stage at all.
    ///
    /// This demonstrates that STRUCTURE; it is NOT coverage of the outcome-less forked candidate.
    /// Both siblings here end `Condemned(BeyondCut)`, so neither is forked and the fold's `forked`
    /// set stays empty. By the same structure that path cannot be built this way — an entry a
    /// register condemns is never forked — so reaching it needs a register admitted LATER than the
    /// round the entry forked in. [`pinned_history`] states why that residue is harmless.
    #[test]
    fn composing_no_v2_operation_agrees_over_a_register_scoped_equivocation() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        let account = crate::local_account(&conn, 1).unwrap();
        let founder = crate::local_device(&conn, 1).unwrap();
        let genesis =
            storage::account_entries_for_enrollment(&conn, account).unwrap()[0].entry_hash;
        let subject = Dev::new(23);

        let author = |signer: &crate::device::DeviceSecret,
                      seq: u64,
                      prev: Option<AccountEntryHash>,
                      authority: OwnerId,
                      op: &AccountOp| {
            let signed = envelope::sign_account_entry(
                signer,
                &AccountEntryHeader {
                    account_id: account,
                    log_id: 0,
                    device_fingerprint: signer.public().fingerprint(),
                    seq,
                    prev_hash: prev,
                    parent_ref: Some(genesis),
                    entry_type: ops::entry_type_of(op),
                    op_version: 1,
                    crypto_suite: 0,
                    auth_len: 1,
                    key_id: None,
                    authority_ref: Some(authority),
                },
                &ops::encode(op).unwrap(),
            )
            .unwrap();
            storage::account_ingest(&conn, &signed.signed_bytes, 1).unwrap();
            signed.entry_hash
        };
        let member = |seed: u8| {
            let other = Dev::new(seed);
            AccountOp::DeviceAdd {
                device_fingerprint: other.fp,
                ed25519_pubkey: other.ed,
                x25519_pubkey: other.x,
                role: DeviceRole::Member,
                label: None,
            }
        };

        let incarnation: OwnerId = author(founder.secret(), 1, Some(genesis), genesis.into(), &{
            AccountOp::DeviceAdd {
                device_fingerprint: subject.fp,
                ed25519_pubkey: subject.ed,
                x25519_pubkey: subject.x,
                role: DeviceRole::Owner,
                label: None,
            }
        })
        .into();
        // The subject EQUIVOCATES at seq 0: one sibling is accepted, the other forks.
        let sibling_a = author(&subject.secret, 0, None, incarnation, &member(24));
        let sibling_b = author(&subject.secret, 0, None, incarnation, &member(25));
        // And the founder then cuts the subject's whole chain, scoping BOTH siblings.
        author(founder.secret(), 2, Some(incarnation.into()), genesis.into(), &{
            AccountOp::DeviceRemove {
                device_fingerprint: subject.fp,
                control_cut: Cut::Empty,
                secrets_cut: Cut::Empty,
                content_cuts: vec![],
                reason: "revoked".into(),
            }
        });

        let tx = conn.transaction().unwrap();
        let bundle = checkpoint::prepare_checkpoint_in_tx(&tx, account, &founder).unwrap();
        let proof = checkpoint::verify_checkpoint(
            TrustedCheckpointPin {
                account_id: account,
                checkpoint_digest: bundle.certificate_digest(),
                required_control_version: 2,
            },
            &bundle,
        )
        .unwrap();
        drop(tx);

        let frozen = proof.frozen_legacy();
        let composed = pinned_history(frozen, &[]);
        for hash in [sibling_a, sibling_b] {
            assert!(
                !frozen.accepted_at_checkpoint(&hash),
                "both equivocating siblings are out of the accepted set",
            );
            assert!(
                matches!(frozen.history().outcome(&hash), Some(Outcome::Condemned(_))),
                "the register condemned them in the FOLD, so neither is merely forked",
            );
            assert_eq!(
                composed.outcome(&hash),
                frozen.history().outcome(&hash),
                "and the composition repeats that verdict exactly",
            );
        }
        assert_eq!(
            composed.outcomes,
            frozen.history().outcomes,
            "no candidate gains or loses an outcome the checkpoint did not decide",
        );
    }

    /// THE under-revocation this composition closes. Without the register effects the roster fact
    /// stays open with both chains unbounded, so a device an authorized v2 cut removed still reads
    /// as a live member whose entries are still effective.
    #[test]
    fn an_applied_v2_device_remove_revokes_the_subject_in_the_rebuilt_projection() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (cut, nominated) = v2_cut(&fixture, false);
        let outcome = applied(frozen, &nominated, &cut);
        let subject = fixture.subject.fp;

        // The checkpoint's own history is the baseline: the subject is an open roster member and
        // its seq-0 entry is effective, which is exactly what an authorized cut has to be
        // able to change.
        let (before_ref, before) = roster_fact_for(frozen.history(), subject).expect("enrolled");
        assert!(before.closed_at.is_none(), "the frozen epoch leaves the subject enrolled");
        assert!(matches!(
            frozen.history().roster_ref_effective(*before_ref, subject),
            AuthorityQuery::Effective(_)
        ));
        assert!(
            frozen
                .history()
                .outcome(&fixture.accepted_victim)
                .is_some_and(|outcome| outcome.is_effective())
        );

        let history = pinned_history(frozen, &[AppliedOperation {
            entry: &cut,
            registers: &outcome.registers,
        }]);

        let (roster_ref, fact) = roster_fact_for(&history, subject).expect("the fact is kept");
        assert!(fact.closed_at.is_some(), "the removal closes the subject's roster fact");
        assert_eq!(fact.control_boundary, AuthorityBoundary::Closed, "control chain bounded");
        assert_eq!(fact.secrets_boundary, AuthorityBoundary::Closed, "secrets chain bounded");
        assert!(history.tombstoned().any(|d| *d == subject), "I4: a removal tombstones");
        // Not a member,
        assert!(
            !matches!(
                history.roster_ref_effective(*roster_ref, subject),
                AuthorityQuery::Effective(_)
            ),
            "a revoked device is not an effective roster member",
        );
        // and not a writer: the device register condemns what was still standing on its chain.
        assert!(
            matches!(history.outcome(&fixture.accepted_victim), Some(Outcome::Condemned(_))),
            "a revoked device's surviving entry is condemned",
        );
    }

    /// The projection and the credit view must not disagree about what a cut took away. Both read
    /// the ONE `apply_cut` result — the credit it returned and the registers it installed — so the
    /// entries the rebuilt history newly drops out of the effective count are exactly what that cut
    /// was credited for.
    #[test]
    fn the_rebuilt_projection_and_the_cuts_credit_agree_on_what_it_removed() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (cut, nominated) = v2_cut(&fixture, false);
        let outcome = applied(frozen, &nominated, &cut);
        let history = pinned_history(frozen, &[AppliedOperation {
            entry: &cut,
            registers: &outcome.registers,
        }]);
        let newly_removed = frozen
            .entries()
            .iter()
            .filter(|entry| {
                frozen.history().outcome(&entry.entry_hash).is_some_and(|o| o.is_effective())
                    && !history.outcome(&entry.entry_hash).is_some_and(|o| o.is_effective())
            })
            .count() as u64;
        assert_eq!(outcome.credit, 1, "one of the subject's entries was still in the frozen count");
        assert_eq!(
            newly_removed, outcome.credit,
            "the projection removed exactly what the cut was credited for",
        );
    }

    /// An operation that installs NO register is deliberately not projected. Execution evaluates no
    /// state preconditions, so projecting a v2 enrolment unjudged would put a device on the roster
    /// this account's own history never admitted — a tombstoned one included. Omitting it can only
    /// under-GRANT, which the operational gates cover; what it can never do is under-revoke.
    #[test]
    fn a_v2_operation_that_installs_no_register_grants_no_authority() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (_, nominated) = v2_cut(&fixture, true);
        let enrolment = &nominated[0];
        let outcome = applied(frozen, &[], enrolment);
        assert!(outcome.registers.is_empty(), "an enrolment installs no register");
        let history = pinned_history(frozen, &[AppliedOperation {
            entry: enrolment,
            registers: &outcome.registers,
        }]);
        assert_eq!(
            projection_hash(&history),
            projection_hash(frozen.history()),
            "a v2 enrolment leaves the frozen projection untouched",
        );
    }

    /// A register join the evidence cannot decide — an incomparable pair, or a watermark naming an
    /// entry nothing holds — must not leave the HELD register standing. That would silently drop
    /// the newcomer's watermark and admit entries a cut meant to condemn. The chain closes
    /// instead: the empty cut is the one answer neither author can be under-served by, and a
    /// later refold recomputes it once the evidence arrives.
    #[test]
    fn a_register_join_the_evidence_cannot_decide_closes_the_chain() {
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (cut, _) = v2_cut(&fixture, false);
        let key = RegisterKey::Device {
            account: fixture.checkpoint.pin().account_id,
            log: CONTROL_LOG,
            device: fixture.subject.fp,
        };
        let registers = [
            (key.clone(), Cut::At { seq: 1, hash: fixture.condemned_victim }),
            // A watermark on a chain nothing in this view holds: the join is undecidable.
            (key.clone(), Cut::At { seq: 9, hash: AccountEntryHash::from_bytes([0x5c; 32]) }),
        ];
        let history =
            pinned_history(frozen, &[AppliedOperation { entry: &cut, registers: &registers }]);
        let (_, fact) = roster_fact_for(&history, fixture.subject.fp).expect("enrolled");
        assert_eq!(
            fact.control_boundary,
            AuthorityBoundary::Closed,
            "an undecidable join closes the chain rather than keeping one side's watermark",
        );
    }

    #[test]
    fn a_closed_chain_stays_closed_when_a_later_register_could_decide() {
        // Closure must be a property of the KEY, not of the value the register holds. `Cut::Empty`
        // cannot carry it: it is the join's bottom, so a decidable watermark arriving afterwards
        // absorbs it and reinstates the very prefix the failed join refused to choose between.
        //
        // Three registers are the minimum that shows it. The first installs on a fresh key without
        // joining anything; the second fails its join and closes the chain; only the THIRD has an
        // `Empty` standing to absorb. With two, both orders close and the property is invisible.
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (cut, _) = v2_cut(&fixture, false);
        let key = RegisterKey::Device {
            account: fixture.checkpoint.pin().account_id,
            log: CONTROL_LOG,
            device: fixture.subject.fp,
        };
        let watermark = Cut::At { seq: 1, hash: fixture.condemned_victim };
        let registers = [
            // Installs on a fresh key — no join runs.
            (key.clone(), watermark.clone()),
            // Undecidable against it: a watermark on a chain nothing in this view holds. Closes.
            (key.clone(), Cut::At { seq: 9, hash: AccountEntryHash::from_bytes([0x5c; 32]) }),
            // Decidable, and the one that would absorb the closure's `Empty`.
            (key.clone(), watermark.clone()),
        ];
        let history =
            pinned_history(frozen, &[AppliedOperation { entry: &cut, registers: &registers }]);
        let (_, fact) = roster_fact_for(&history, fixture.subject.fp).expect("enrolled");
        assert_eq!(
            fact.control_boundary,
            AuthorityBoundary::Closed,
            "a chain closed by an undecidable join stays closed against a later decidable one",
        );
    }

    #[test]
    fn the_composed_boundary_is_a_function_of_the_register_multiset_not_its_order() {
        // Closure being absorbing is what makes the boundary order-free. Replicas holding different
        // evidence subsets disagree about WHICH pair is undecidable, so without it they would
        // derive different boundaries from the same registers and diverge. Three of these six
        // orders previously yielded a watermark instead of `Closed`.
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (cut, _) = v2_cut(&fixture, false);
        let key = RegisterKey::Device {
            account: fixture.checkpoint.pin().account_id,
            log: CONTROL_LOG,
            device: fixture.subject.fp,
        };
        let a = Cut::At { seq: 0, hash: fixture.accepted_victim };
        let b = Cut::At { seq: 1, hash: fixture.condemned_victim };
        // A watermark on a chain nothing in this view holds: the join cannot be decided.
        let u = Cut::At { seq: 9, hash: AccountEntryHash::from_bytes([0x5c; 32]) };

        for order in
            [[&a, &b, &u], [&a, &u, &b], [&b, &a, &u], [&b, &u, &a], [&u, &a, &b], [&u, &b, &a]]
        {
            let registers: Vec<_> = order.iter().map(|cut| (key.clone(), (*cut).clone())).collect();
            let history =
                pinned_history(frozen, &[AppliedOperation { entry: &cut, registers: &registers }]);
            let (_, fact) = roster_fact_for(&history, fixture.subject.fp).expect("enrolled");
            assert_eq!(
                fact.control_boundary,
                AuthorityBoundary::Closed,
                "every order of the same registers must close the chain; this one did not",
            );
        }
    }

    #[test]
    fn a_frozen_closed_chain_is_not_reopened_by_one_authorized_register() {
        // The checkpoint froze this chain closed: a legacy cut naming no entry leaves `Cut::Empty`
        // standing in the frozen registers. Because `Empty` is the join's bottom, a SINGLE pair
        // naming any watermark would otherwise join `Applied` against it and reopen the chain —
        // no ordering trick and no evidence required. The fixture's own `OwnerDemote` carries
        // `secrets_cut: Cut::Empty`, so the frozen slot is already occupied here, which is what
        // the three-register test cannot exercise.
        let fixture = demoted_owner();
        let frozen = fixture.checkpoint.frozen_legacy();
        let (cut, _) = v2_cut(&fixture, false);
        // An `OwnerIncarnation` register governs the OWNER INCARNATION fact, not the roster device
        // fact: `derive_authority_facts` projects onto a roster fact only from
        // `RegisterKey::Device` registers. Read the surface this key actually reaches.
        let (key, _) = frozen
            .registers
            .iter()
            .find(|(key, cut)| {
                matches!(key, RegisterKey::OwnerIncarnation { .. }) && matches!(cut, Cut::Empty)
            })
            .expect("the legacy OwnerDemote closed the secrets chain with an empty cut");
        let owner_id = match key {
            RegisterKey::OwnerIncarnation { owner_id, .. } => *owner_id,
            other => panic!("expected an owner-incarnation key, got {other:?}"),
        };
        let secrets_boundary = |history: &AccountAuthHistory| {
            history
                .owner_incarnation_facts()
                .find(|(id, _)| **id == owner_id)
                .map(|(_, fact)| fact.secrets_boundary)
        };
        let before = secrets_boundary(frozen.history());

        let registers =
            [(key.clone(), Cut::At { seq: 42, hash: AccountEntryHash::from_bytes([0xfa; 32]) })];
        let history =
            pinned_history(frozen, &[AppliedOperation { entry: &cut, registers: &registers }]);

        assert_eq!(
            before,
            Some(AuthorityBoundary::Closed),
            "the fixture must start from a frozen-closed chain or this proves nothing",
        );
        assert_eq!(
            secrets_boundary(&history),
            Some(AuthorityBoundary::Closed),
            "a chain the checkpoint froze closed stays closed against a later authorized register",
        );
    }

    #[test]
    fn an_operation_that_installs_no_register_earns_no_credit() {
        let fixture = demoted_owner();
        let (_, nominated) = v2_cut(&fixture, true);
        let outcome = applied(fixture.checkpoint.frozen_legacy(), &[], &nominated[0]);
        assert!(outcome.registers.is_empty());
        assert_eq!(outcome.credit, 0);
    }
}
