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

use super::*;

/// Authenticated evidence and the FINAL coherent legacy fold, captured once during checkpoint
/// verification. Normal v1 folds neither retain this trace nor clone their result.
pub(in crate::account) struct FrozenLegacy {
    entries: Vec<VerifiedAccountEntry>,
    history: AccountAuthHistory,
    trace: LegacyTrace,
    accepted: HashSet<AccountEntryHash>,
}

impl FrozenLegacy {
    pub(in crate::account) fn new(
        entries: Vec<VerifiedAccountEntry>,
        history: AccountAuthHistory,
        trace: LegacyTrace,
        accepted: HashSet<AccountEntryHash>,
    ) -> Self {
        Self { entries, history, trace, accepted }
    }

    pub(in crate::account) fn entries(&self) -> &[VerifiedAccountEntry] {
        &self.entries
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
    /// `device`. Re-signing an op at version 2 grants no authority the legacy fold withheld.
    pub(in crate::account) fn owner_is_live(
        &self,
        incarnation: OwnerId,
        device: DeviceFingerprint,
    ) -> bool {
        matches!(
            self.history.owner_incarnation_effective(incarnation, device),
            AuthorityQuery::Effective(_)
        )
    }

    /// Whether the FINAL legacy registers already condemn `candidate`. A chain the legacy epoch cut
    /// is not reopened by continuing it at version 2.
    pub(in crate::account) fn condemns(&self, candidate: &Candidate) -> bool {
        let headers = self.headers();
        let view = CandidateView { headers: &headers };
        matches!(
            register_verdict(candidate, &self.trace.registers, &view),
            RegisterVerdict::Condemned(_)
        )
    }

    fn headers(&self) -> HashMap<AccountEntryHash, &AccountEntryHeader> {
        self.entries.iter().map(|entry| (entry.entry_hash, &entry.header)).collect()
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
    /// The v2 entries the operation's signed manifest nominates, already authenticated against a
    /// key the accepted legacy epoch certifies. An entry absent from here is never counted.
    pub(in crate::account) nominated: &'a [Candidate],
    pub(in crate::account) cut: &'a Candidate,
}

/// What an authorized operation installs, and the bounded credit its nomination earns.
pub(in crate::account) struct AppliedCut {
    pub(in crate::account) registers: Vec<(RegisterKey, Cut)>,
    pub(in crate::account) credit: u64,
}

/// Execute one authorized operation. A non-cut installs nothing and earns nothing.
pub(in crate::account) fn apply_cut(input: CutExecution<'_>) -> AppliedCut {
    let registers: Vec<(RegisterKey, Cut)> =
        cut_op_registers(input.cut).into_iter().map(|(key, cut, _)| (key, cut)).collect();
    // The nominated identities, plus the legacy entries the checkpoint accepted — legacy evidence
    // is implicit in every view, but bounded by what was actually standing when it froze.
    let mut eligible: HashSet<AccountEntryHash> =
        input.nominated.iter().map(Candidate::hash).collect();
    eligible.extend(input.frozen.accepted.iter().copied());
    let credit = credit_under(&input, CreditScope::Nominated(&eligible));
    AppliedCut { registers, credit }
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
    // A nominated entry is effective only if it held authority in this epoch. Being signed by a
    // certified key and sitting on a chain that reaches an accepted branch establishes WHO wrote it
    // and WHERE, never that it was allowed to: without this an unauthorized op — one the v1 fold
    // rejects outright, and so never counts — would enter as effective and be worth a credit as
    // soon as the cut condemned it, which is exactly the admission bypass a nomination must not
    // buy. Ascending depth, so a mint is always settled before anything citing it.
    // Every v2 candidate starts unauthorized and has to earn its place. One whose cited incarnation
    // does not even resolve is never visited below, so it simply stays out.
    let mut unauthorized: HashSet<AccountEntryHash> =
        candidates[legacy_count..].iter().map(Candidate::hash).collect();
    let mut v2_mints: HashMap<OwnerId, DeviceFingerprint> = HashMap::new();
    for &idx in strata.values().flatten() {
        if idx < legacy_count {
            continue;
        }
        let candidate = &candidates[idx];
        let signer = candidate.header().device_fingerprint;
        let cited = candidate.header().authority_ref;
        let authorized = cited.is_some_and(|incarnation| {
            input.frozen.owner_is_live(incarnation, signer)
                || v2_mints.get(&incarnation) == Some(&signer)
        });
        // The legacy epoch's registers still bound these chains, and continuing one at version 2
        // does not reopen it.
        let cut_by_legacy = matches!(
            register_verdict(candidate, &input.frozen.trace.registers, &view),
            RegisterVerdict::Condemned(_)
        );
        if authorized && !cut_by_legacy {
            unauthorized.remove(&candidate.hash());
            outcomes.insert(candidate.hash(), Outcome::Effective { auth_epoch: 0 });
            if candidate.is_mint() {
                v2_mints.insert(candidate.hash().into(), candidate.subject_device());
            }
            continue;
        }
        // Separate impersonation from a mint that simply is not live any more, exactly as
        // `authority_status` does. Only the latter is a stale dependent the credit rule's second
        // loop may count when the cut condemns the mint it cites; an entry citing a mint for
        // ANOTHER device is `WrongDevice` and is never credited.
        let impersonates = cited
            .and_then(|incarnation| incarnations.candidate(&incarnation))
            .is_some_and(|mint| mint.subject_device() != signer);
        let reason =
            if impersonates { RejectReason::WrongDevice } else { RejectReason::StaleAuthority };
        outcomes.insert(candidate.hash(), Outcome::Rejected(reason));
    }

    // Only the cut's OWN registers decide what it took away — the v1 rule. Another cut in the same
    // view never widens this one's scope; it only moves outcomes, which is precisely why the
    // guarantee is an upper bound on identities rather than a fixed number.
    for candidate in &candidates {
        // An entry that never held authority was never in the effective count, so condemning it
        // takes nothing away. The v1 overlay would promote it to `Condemned` anyway — condemnation
        // outranks a stale-authority rejection there — and hand this cut a credit for an entry its
        // own author never counted. Holding it out keeps the credit at or below the v1 rule.
        if candidate.hash() == input.cut.hash() || unauthorized.contains(&candidate.hash()) {
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
    fn frozen_trace_comes_from_final_branch_closure_not_raw_fold_counts() {
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
        assert!(frozen.trace.contributors.is_empty());
    }

    #[test]
    fn frozen_trace_preserves_ineffective_contributors_and_final_readiness_exclusions() {
        for auth_len in [1, 999] {
            let mut conn = rusqlite::Connection::open_in_memory().unwrap();
            rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
            let account = crate::local_account(&conn, 1).unwrap();
            let device = crate::local_device(&conn, 1).unwrap();
            let genesis =
                storage::account_entries_for_enrollment(&conn, account).unwrap()[0].entry_hash;
            let op = AccountOp::DeviceRemove {
                device_fingerprint: Dev::new(7).fp,
                control_cut: Cut::Empty,
                secrets_cut: Cut::Empty,
                content_cuts: vec![],
                reason: "never enrolled".into(),
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
                    auth_len,
                    key_id: None,
                    authority_ref: Some(genesis.into()),
                },
                &ops::encode(&op).unwrap(),
            )
            .unwrap();
            storage::account_ingest(&conn, &signed.signed_bytes, 1).unwrap();
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
            assert!(!frozen.accepted.contains(&signed.entry_hash));
            assert_eq!(frozen.trace.contributors.contains(&signed.entry_hash), auth_len == 1);
            assert_eq!(!frozen.trace.registers.is_empty(), auth_len == 1);
            assert_eq!(
                frozen.trace.readiness_exclusions.contains_key(&signed.entry_hash),
                auth_len == 999
            );
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
        author(founder.secret(), 2, subject_incarnation.into(), genesis.into(), &{
            AccountOp::OwnerDemote {
                device_fingerprint: subject.fp,
                owner_id: subject_incarnation,
                control_cut: Cut::At { seq: 0, hash: accepted_victim },
                secrets_cut: Cut::Empty,
                reason: "demoted".into(),
            }
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

    #[test]
    fn nominated_credit_is_a_subset_of_the_v1_credit_for_the_same_register_scope() {
        let fixture = demoted_owner();
        let (cut, nominated) = v2_cut(&fixture, false);
        let input = CutExecution {
            frozen: fixture.checkpoint.frozen_legacy(),
            nominated: &nominated,
            cut: &cut,
        };
        let unrestricted = credit_under(&input, CreditScope::EveryScopedEntry);
        let applied = apply_cut(input);
        // The v1 rule counts both of the subject's entries; the frozen accepted set admits only the
        // one the checkpoint was still counting.
        assert_eq!(unrestricted, 2, "v1 counts every entry the cut's own registers scope");
        assert_eq!(applied.credit, 1, "a legacy branch already out of the count earns nothing");
        assert!(applied.credit <= unrestricted, "nomination can only narrow the v1 credit");
        assert_eq!(applied.registers.len(), 2, "a device remove cuts control and secrets");
        assert!(
            fixture.checkpoint.frozen_legacy().accepted_at_checkpoint(&fixture.accepted_victim)
        );
        assert!(
            !fixture.checkpoint.frozen_legacy().accepted_at_checkpoint(&fixture.condemned_victim)
        );
    }

    #[test]
    fn nominating_entries_the_cut_never_took_does_not_inflate_its_credit() {
        let fixture = demoted_owner();
        let (cut, nominated) = v2_cut(&fixture, true);
        assert_eq!(nominated.len(), 1, "an entry on a chain this cut does not scope");
        let bare = v2_cut(&fixture, false);
        let baseline = apply_cut(CutExecution {
            frozen: fixture.checkpoint.frozen_legacy(),
            nominated: &bare.1,
            cut: &bare.0,
        });
        let applied = apply_cut(CutExecution {
            frozen: fixture.checkpoint.frozen_legacy(),
            nominated: &nominated,
            cut: &cut,
        });
        // Nomination is weaker than proof of loss: naming an entry earns nothing unless the cut's
        // own registers actually condemn it.
        assert_eq!(applied.credit, baseline.credit);
        assert_eq!(applied.credit, 1);
    }

    /// Author one v2 operation on the founder's own chain, citing its live incarnation.
    fn sign_v2(
        fixture: &DemotedOwner,
        seq: u64,
        prev: AccountEntryHash,
        op: v2_ops::ControlOp,
    ) -> Candidate {
        let signed = envelope::sign_account_entry(
            fixture.founder.secret(),
            &AccountEntryHeader {
                account_id: fixture.checkpoint.pin().account_id,
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
        Candidate::new(
            VerifiedAccountEntry {
                header: signed.header,
                payload: signed.payload,
                entry_hash: signed.entry_hash,
            },
            op.op,
        )
    }

    /// THE residual, demonstrated rather than argued. State preconditions are not evaluated, so an
    /// entry the v1 fold would reject `Ineffective` still earns a credit once the cut condemns it.
    /// The excess is exactly one per nominated entry the cut's own registers scope.
    #[test]
    fn a_nominated_entry_v1_would_reject_ineffective_is_still_counted() {
        let fixture = demoted_owner();
        let tip = fixture
            .checkpoint
            .continuation_heads()
            .iter()
            .find(|head| head.device_fingerprint == fixture.founder.fingerprint())
            .unwrap()
            .clone();
        // Re-enrolling a device already on the roster: `classify_effect` rejects this
        // `DuplicateAdd`, so a real v1 fold would never credit it.
        let duplicate = sign_v2(&fixture, tip.seq + 1, tip.hash, v2_ops::ControlOp {
            checkpoint: fixture.checkpoint.pin().checkpoint_digest,
            pre_cut_view: None,
            op: AccountOp::DeviceAdd {
                device_fingerprint: fixture.subject.fp,
                ed25519_pubkey: fixture.subject.ed,
                x25519_pubkey: fixture.subject.x,
                role: DeviceRole::Member,
                label: None,
            },
        });
        let cut = sign_v2(&fixture, tip.seq + 2, duplicate.hash(), v2_ops::ControlOp {
            checkpoint: fixture.checkpoint.pin().checkpoint_digest,
            pre_cut_view: Some([9; 32]),
            op: AccountOp::DeviceRemove {
                device_fingerprint: fixture.founder.fingerprint(),
                control_cut: Cut::Empty,
                secrets_cut: Cut::Empty,
                content_cuts: vec![],
                reason: "revoked".into(),
            },
        });

        let frozen = fixture.checkpoint.frozen_legacy();
        let without = apply_cut(CutExecution { frozen, nominated: &[], cut: &cut });
        let with = apply_cut(CutExecution {
            frozen,
            nominated: std::slice::from_ref(&duplicate),
            cut: &cut,
        });
        assert_eq!(
            with.credit,
            without.credit + 1,
            "an ineffective nomination the cut scopes is worth exactly one over-count",
        );
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
        let without = apply_cut(CutExecution { frozen, nominated: &[], cut: &cut });
        let with = apply_cut(CutExecution {
            frozen,
            nominated: std::slice::from_ref(&unauthorized),
            cut: &cut,
        });
        // It sits on the revoked device's chain and the cut's device register condemns everything
        // there, so it would be worth a credit the moment it were treated as effective.
        assert_eq!(with.credit, without.credit, "a nomination never substitutes for admission");
        assert_eq!(with.credit, 1);
    }

    #[test]
    fn an_operation_that_installs_no_register_earns_no_credit() {
        let fixture = demoted_owner();
        let (_, nominated) = v2_cut(&fixture, true);
        let applied = apply_cut(CutExecution {
            frozen: fixture.checkpoint.frozen_legacy(),
            nominated: &[],
            cut: &nominated[0],
        });
        assert!(applied.registers.is_empty());
        assert_eq!(applied.credit, 0);
    }
}
