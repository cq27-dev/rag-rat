use std::collections::HashSet;

use super::super::checkpoint::{self, TrustedCheckpointPin, VerifiedCheckpoint};
use super::super::cut::Cut;
use super::super::envelope::{self, AccountEntryHeader, SignedAccountEntry};
use super::super::fold;
use super::super::id::{AccountEntryHash, OwnerId};
use super::super::ops::{self as legacy, AccountOp, DeviceRole};
use super::super::test_support::Dev;
use super::{executor, ops, views};
use crate::identity::LocalDevice;
use crate::op::DeviceFingerprint;

fn checkpoint() -> (VerifiedCheckpoint, LocalDevice) {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
    let account = crate::local_account(&conn, 1).unwrap();
    let device = crate::local_device(&conn, 1).unwrap();
    let tx = conn.transaction().unwrap();
    let bundle = checkpoint::prepare_checkpoint_in_tx(&tx, account, &device).unwrap();
    let expected = TrustedCheckpointPin {
        account_id: account,
        checkpoint_digest: bundle.certificate_digest(),
        required_control_version: 2,
    };
    (checkpoint::verify_checkpoint(expected, &bundle).unwrap(), device)
}

fn revocation(demote: bool) -> ops::ControlOp {
    let device_fingerprint = DeviceFingerprint::from_bytes([4; 32]);
    let op = if demote {
        AccountOp::OwnerDemote {
            device_fingerprint,
            owner_id: [5; 32].into(),
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            reason: "revoked".into(),
        }
    } else {
        AccountOp::DeviceRemove {
            device_fingerprint,
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: vec![],
            reason: "revoked".into(),
        }
    };
    ops::ControlOp { checkpoint: [1; 32], pre_cut_view: Some([2; 32]), op }
}

fn manifest(
    checkpoint: &VerifiedCheckpoint,
    entries: Vec<AccountEntryHash>,
) -> views::ViewManifest {
    views::ViewManifest { checkpoint: checkpoint.pin().checkpoint_digest, entries }
}

fn signed(
    checkpoint: &VerifiedCheckpoint,
    device: &LocalDevice,
    pre_cut_view: [u8; 32],
    salt: u8,
) -> SignedAccountEntry {
    let mut op = revocation(false);
    op.checkpoint = checkpoint.pin().checkpoint_digest;
    op.pre_cut_view = Some(pre_cut_view);
    if let AccountOp::DeviceRemove { reason, .. } = &mut op.op {
        *reason = format!("revoked {salt}");
    }
    let tip = &checkpoint.continuation_heads()[0];
    envelope::sign_account_entry(
        device.secret(),
        &AccountEntryHeader {
            account_id: checkpoint.pin().account_id,
            log_id: 0,
            device_fingerprint: device.fingerprint(),
            seq: tip.seq + 1,
            prev_hash: Some(tip.hash),
            parent_ref: Some(tip.hash),
            entry_type: legacy::entry_type_of(&op.op),
            op_version: ops::CONTROL_VERSION,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(tip.hash.into()),
        },
        &op.encode().unwrap(),
    )
    .unwrap()
}

/// Authors v2 operations on the checkpoint signer's own chain, threading `(seq, prev_hash)`. The
/// cited incarnation stays the one the legacy epoch minted — only the chain moves — so a long run
/// of operations exercises chain length rather than a run of fresh authority.
struct Chain<'a> {
    checkpoint: &'a VerifiedCheckpoint,
    device: &'a LocalDevice,
    seq: u64,
    prev: AccountEntryHash,
    incarnation: OwnerId,
}

impl<'a> Chain<'a> {
    fn new(checkpoint: &'a VerifiedCheckpoint, device: &'a LocalDevice) -> Self {
        let tip = checkpoint
            .continuation_heads()
            .iter()
            .find(|head| head.device_fingerprint == device.fingerprint())
            .expect("the checkpoint signer has an accepted control head");
        // The fixture account holds only its genesis, so the founder's head IS the mint its
        // incarnation is named by.
        Chain {
            checkpoint,
            device,
            seq: tip.seq + 1,
            prev: tip.hash,
            incarnation: tip.hash.into(),
        }
    }

    fn author(&mut self, op: ops::ControlOp) -> SignedAccountEntry {
        let signed = envelope::sign_account_entry(
            self.device.secret(),
            &AccountEntryHeader {
                account_id: self.checkpoint.pin().account_id,
                log_id: 0,
                device_fingerprint: self.device.fingerprint(),
                seq: self.seq,
                prev_hash: Some(self.prev),
                parent_ref: Some(self.prev),
                entry_type: legacy::entry_type_of(&op.op),
                op_version: ops::CONTROL_VERSION,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: Some(self.incarnation),
            },
            &op.encode().unwrap(),
        )
        .unwrap();
        self.seq += 1;
        self.prev = signed.entry_hash;
        signed
    }

    /// An ordinary operation: it nominates no view and needs no historical evidence at all.
    fn add(&mut self, seed: u8) -> SignedAccountEntry {
        let enrolled = Dev::new(seed);
        self.author(ops::ControlOp {
            checkpoint: self.checkpoint.pin().checkpoint_digest,
            pre_cut_view: None,
            op: AccountOp::DeviceAdd {
                device_fingerprint: enrolled.fp,
                ed25519_pubkey: enrolled.ed,
                x25519_pubkey: enrolled.x,
                role: DeviceRole::Member,
                label: None,
            },
        })
    }

    fn remove(&mut self, pre_cut_view: [u8; 32], index: u32) -> SignedAccountEntry {
        let mut target = [0u8; 32];
        target[..4].copy_from_slice(&index.to_le_bytes());
        self.author(ops::ControlOp {
            checkpoint: self.checkpoint.pin().checkpoint_digest,
            pre_cut_view: Some(pre_cut_view),
            op: AccountOp::DeviceRemove {
                device_fingerprint: DeviceFingerprint::from_bytes(target),
                control_cut: Cut::Empty,
                secrets_cut: Cut::Empty,
                content_cuts: vec![],
                reason: format!("revoked {index}"),
            },
        })
    }
}

fn bytes(entries: &[SignedAccountEntry]) -> Vec<Vec<u8>> {
    entries.iter().map(|entry| entry.signed_bytes.clone()).collect()
}

#[test]
fn revocations_bind_their_pre_cut_view_without_changing_v1_bytes() {
    for demote in [false, true] {
        let mut op = revocation(demote);
        let tag = legacy::entry_type_of(&op.op);
        let old = legacy::encode(&op.op).unwrap();
        let bytes = op.encode().unwrap();
        assert_eq!(ops::decode(tag, &bytes).unwrap(), op);
        assert!(legacy::decode(tag, &bytes).is_err());
        assert!(ops::decode(tag, &old).is_err());
        assert_eq!(legacy::encode(&op.op).unwrap(), old);
        op.pre_cut_view = Some([9; 32]);
        assert_ne!(op.encode().unwrap(), bytes);
        assert_eq!(fold::SUPPORTED_OP_VERSION, 1);
    }
}

#[test]
fn unknown_v2_tags_are_explicitly_unsupported_by_the_isolated_decoder() {
    let bytes = revocation(false).encode().unwrap();
    let error = ops::decode(99, &bytes).unwrap_err();
    assert!(error.to_string().contains("unsupported v2 control operation"));
    assert!(matches!(
        legacy::decode(99, &[0x80]).unwrap(),
        legacy::DecodedAccountOp::Unknown { .. }
    ));
}

#[test]
fn payload_fit_does_not_bypass_the_complete_signed_envelope_limit() {
    use super::super::limits::{ACCOUNT_ENVELOPE_MAX_BYTES, ACCOUNT_SIGNED_DOMAIN};
    use crate::cbor::VecEncoderExt;

    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let template = signed(&checkpoint, &device, empty.digest().unwrap(), 0);
    let mut op = ops::decode(template.header.entry_type, &template.payload).unwrap();
    if let AccountOp::DeviceRemove { reason, .. } = &mut op.op {
        *reason = "r".repeat(ACCOUNT_ENVELOPE_MAX_BYTES - 256);
    }
    let payload = op.encode().unwrap();
    assert!(payload.len() < ACCOUNT_ENVELOPE_MAX_BYTES);
    assert!(envelope::sign_account_entry(device.secret(), &template.header, &payload).is_err());

    // Construct a correctly signed envelope directly to test the receiving decoder as well.
    let mut body = Vec::new();
    let mut e = minicbor::Encoder::new(&mut body);
    e.put_array(2);
    e.put_bytes(&template.header_bytes);
    e.put_bytes(&payload);
    let signature = device.secret().sign(&body);
    let mut wire = Vec::new();
    let mut e = minicbor::Encoder::new(&mut wire);
    e.put_array(3);
    e.put_str(ACCOUNT_SIGNED_DOMAIN);
    e.put_bytes(&body);
    e.put_bytes(&signature);
    assert!(wire.len() > ACCOUNT_ENVELOPE_MAX_BYTES);
    let error = envelope::decode_account_signed(&wire).unwrap_err();
    assert!(error.to_string().contains("limit"));
    assert!(matches!(
        views::plan_replay(&checkpoint, &wire, &[empty.encode().unwrap()], &[]),
        Err(views::PlanError::Invalid(_))
    ));
}

#[test]
fn the_pre_cut_view_is_the_only_slot_and_is_present_exactly_for_a_revocation() {
    let mut op = revocation(false);
    op.pre_cut_view = None;
    assert!(op.encode().is_err(), "a revocation nominates a view");
    op.pre_cut_view = Some([2; 32]);
    let revoking = op.encode().unwrap();
    assert_eq!(ops::decode(legacy::entry_type_of(&op.op), &revoking).unwrap(), op);

    // An ordinary operation nominates none, and needs no historical evidence to be read. The
    // payload carries no other slot: credit evidence is the detached manifest alone.
    op.op = AccountOp::OwnerPromote { device_fingerprint: DeviceFingerprint::from_bytes([3; 32]) };
    assert!(op.encode().is_err(), "an ordinary operation nominates no view");
    op.pre_cut_view = None;
    let bytes = op.encode().unwrap();
    assert_eq!(ops::decode(legacy::entry_type_of(&op.op), &bytes).unwrap(), op);
}

#[test]
fn v2_stays_retained_by_the_production_v1_fold() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let entry = signed(&checkpoint, &device, empty.digest().unwrap(), 0);
    let verified =
        envelope::verify_account_signed(&entry.signed_bytes, &device.secret().public()).unwrap();
    assert!(matches!(
        fold::fold_account(&[verified]).outcome(&entry.entry_hash),
        Some(fold::Outcome::RetainedUnfolded)
    ));
}

#[test]
fn diamond_dependencies_are_planned_once_in_dependency_order() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let a = signed(&checkpoint, &device, empty.digest().unwrap(), 1);
    let b = signed(&checkpoint, &device, empty.digest().unwrap(), 2);
    let left = manifest(&checkpoint, vec![a.entry_hash]);
    let right = manifest(&checkpoint, vec![b.entry_hash]);
    let c = signed(&checkpoint, &device, left.digest().unwrap(), 3);
    let d = signed(&checkpoint, &device, right.digest().unwrap(), 4);
    let root = manifest(&checkpoint, vec![a.entry_hash, b.entry_hash, c.entry_hash, d.entry_hash]);
    let mut manifests = [&empty, &left, &right, &root].map(|view| view.encode().unwrap());
    let mut evidence = [a, b, c, d].map(|entry| entry.signed_bytes);
    let plan =
        plan_replay(&checkpoint, &device, root.digest().unwrap(), &manifests, &evidence).unwrap();
    let order = plan.views().map(|(hash, _)| hash).collect::<Vec<_>>();
    assert_eq!(order.len(), 4);
    assert_eq!(order.iter().collect::<HashSet<_>>().len(), 4);
    assert_eq!(order[0], empty.digest().unwrap());
    assert_eq!(order[3], root.digest().unwrap());
    for (_, view) in plan.views() {
        for hash in &view.entries {
            assert_eq!(plan.candidate(hash).entry_hash, *hash);
        }
    }
    manifests.reverse();
    evidence.reverse();
    let reordered =
        plan_replay(&checkpoint, &device, root.digest().unwrap(), &manifests, &evidence).unwrap();
    assert_eq!(order, reordered.views().map(|(hash, _)| hash).collect::<Vec<_>>());
}

#[test]
fn missing_dependencies_never_return_a_partial_plan() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let entry = signed(&checkpoint, &device, empty.digest().unwrap(), 0);
    let root = manifest(&checkpoint, vec![entry.entry_hash]);
    let manifests = [root.encode().unwrap()];
    assert!(
        matches!(plan_replay(&checkpoint, &device, root.digest().unwrap(), &manifests, &[]), Err(views::PlanError::MissingEntry(hash)) if hash == entry.entry_hash)
    );
    assert!(
        matches!(plan_replay(&checkpoint, &device, root.digest().unwrap(), &manifests, &[entry.signed_bytes]), Err(views::PlanError::MissingView(hash)) if hash == empty.digest().unwrap())
    );
}

#[test]
fn checkpoint_mismatch_and_duplicate_evidence_are_rejected() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let bytes = empty.encode().unwrap();
    let entry = signed(&checkpoint, &device, empty.digest().unwrap(), 0);
    let duplicates = [entry.signed_bytes.clone(), entry.signed_bytes];
    assert!(matches!(
        plan_replay(
            &checkpoint,
            &device,
            empty.digest().unwrap(),
            std::slice::from_ref(&bytes),
            &duplicates
        ),
        Err(views::PlanError::Invalid(_))
    ));
    assert!(matches!(
        plan_replay(&checkpoint, &device, empty.digest().unwrap(), &[bytes.clone(), bytes], &[]),
        Err(views::PlanError::Invalid(_))
    ));
    let foreign = views::ViewManifest { checkpoint: [99; 32], entries: vec![] };
    assert!(matches!(
        plan_replay(
            &checkpoint,
            &device,
            foreign.digest().unwrap(),
            &[foreign.encode().unwrap()],
            &[]
        ),
        Err(views::PlanError::Invalid(_))
    ));
    assert!(matches!(
        plan_replay(
            &checkpoint,
            &device,
            empty.digest().unwrap(),
            &[empty.encode().unwrap()],
            &checkpoint.bundle().evidence
        ),
        Err(views::PlanError::Invalid(_))
    ));
}

#[test]
fn aggregate_manifests_are_bounded_by_the_declared_evidence_byte_budget() {
    let (checkpoint, device) = checkpoint();
    let chunk = views::MAX_BYTES / 128;
    let manifests = vec![vec![0u8; chunk]; 200];
    assert!(manifests.iter().all(|bytes| bytes.len() < views::MAX_BYTES));
    let result = plan_replay(&checkpoint, &device, [0; 32], &manifests, &[]);
    assert!(
        matches!(result, Err(views::PlanError::Invalid(error)) if error.to_string().contains("byte limit")),
        "individually small manifests still have to fit the aggregate budget",
    );
}

#[test]
fn ordinary_operations_execute_at_any_chain_length_with_no_historical_view() {
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let operations: Vec<SignedAccountEntry> = (0..40u8).map(|seed| chain.add(seed)).collect();
    assert!(operations.len() > 32, "the run must exceed any plausible depth cap");
    for (index, operation) in operations.iter().enumerate() {
        // No manifest at all: an ordinary operation nominates nothing, so there is no view to
        // supply and nothing for a depth cap to measure.
        let held = bytes(&operations[..index]);
        let verdict = executor::execute(&checkpoint, &operation.signed_bytes, &[], &held).unwrap();
        let executor::Verdict::Applied { registers, credit } = verdict else {
            panic!("operation {index} did not execute");
        };
        assert!(registers.is_empty(), "an enrollment installs no register");
        assert_eq!(credit, 0, "only a revocation earns credit");
    }
}

#[test]
fn independent_revocation_manifests_commit_cumulative_prior_candidates() {
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let mut manifests = Vec::new();
    let mut cuts: Vec<SignedAccountEntry> = Vec::new();
    // Each cut commits to every candidate before it: 96 distinct views, not one long chain of
    // single-entry views, and each one independent of the others' contents.
    for index in 0..96u32 {
        let view = manifest(&checkpoint, cuts.iter().map(|cut| cut.entry_hash).collect());
        manifests.push(view.encode().unwrap());
        cuts.push(chain.remove(view.digest().unwrap(), index));
    }
    let (last, earlier) = cuts.split_last().unwrap();
    let evidence = bytes(earlier);
    let plan = views::plan_replay(&checkpoint, &last.signed_bytes, &manifests, &evidence).unwrap();
    assert_eq!(plan.views().count(), 96, "each distinct view is scheduled exactly once");
    assert_eq!(plan.root().unwrap().entries.len(), 95);
    let verdict =
        executor::execute(&checkpoint, &last.signed_bytes, &manifests, &evidence).unwrap();
    let executor::Verdict::Applied { registers, .. } = verdict else {
        panic!("the deepest cut did not execute");
    };
    assert_eq!(registers.len(), 2, "a device remove cuts control and secrets");
}

#[test]
fn incomplete_evidence_parks_without_applying_registers_or_credit() {
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let empty = manifest(&checkpoint, vec![]);
    let cut0 = chain.remove(empty.digest().unwrap(), 0);
    let view1 = manifest(&checkpoint, vec![cut0.entry_hash]);
    let cut1 = chain.remove(view1.digest().unwrap(), 1);
    // Deliberately nominates only `cut0`, so `cut1` may be withheld without the manifest noticing.
    let view2 = manifest(&checkpoint, vec![cut0.entry_hash]);
    let cut2 = chain.remove(view2.digest().unwrap(), 2);
    let view3 = manifest(&checkpoint, vec![cut0.entry_hash, cut2.entry_hash]);
    let cut3 = chain.remove(view3.digest().unwrap(), 3);
    let (m0, m1) = (empty.encode().unwrap(), view1.encode().unwrap());
    let (m2, m3) = (view2.encode().unwrap(), view3.encode().unwrap());

    let cases = [
        // A cited view is withheld.
        (
            &cut1,
            vec![m1.clone()],
            bytes(std::slice::from_ref(&cut0)),
            executor::ParkCause::Manifest,
        ),
        // A nominated identity's signed bytes are withheld.
        (&cut1, vec![m0.clone(), m1], Vec::new(), executor::ParkCause::Evidence),
        // The slot the operation continues from is withheld.
        (
            &cut2,
            vec![m0.clone(), m2.clone()],
            bytes(std::slice::from_ref(&cut0)),
            executor::ParkCause::ChainHead,
        ),
        // A link further back along the walk to the legacy branch is withheld.
        (
            &cut3,
            vec![m0.clone(), m2.clone(), m3.clone()],
            bytes(&[cut0.clone(), cut2.clone()]),
            executor::ParkCause::Ancestry,
        ),
    ];
    for (operation, manifests, evidence, expected) in cases {
        let verdict =
            executor::execute(&checkpoint, &operation.signed_bytes, &manifests, &evidence).unwrap();
        // A park carries no register and no credit: the variant itself withholds both, so there is
        // no partial application for a later arrival to unwind.
        assert!(
            matches!(verdict, executor::Verdict::Parked(cause) if cause == expected),
            "expected {expected:?}",
        );
    }

    // The same operation applies once nothing is missing, so the parks above withheld real effect.
    let complete = bytes(&[cut0, cut1, cut2]);
    let verdict =
        executor::execute(&checkpoint, &cut3.signed_bytes, &[m0, m2, m3], &complete).unwrap();
    assert!(
        matches!(verdict, executor::Verdict::Applied { registers, .. } if registers.len() == 2)
    );
}

#[test]
fn unverifiable_evidence_is_a_bad_bundle_not_a_permanently_rejected_operation() {
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let first = chain.add(1);
    let second = chain.add(2);
    // The signature rides outside the hashed body, so flipping it leaves a structurally valid
    // object with the same identity that simply does not verify.
    let mut tampered = first.signed_bytes.clone();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(
        executor::execute(&checkpoint, &second.signed_bytes, &[], &[tampered]).is_err(),
        "an attachment that does not verify refuses the bundle",
    );
    // The operation itself was never at fault: it executes once the attachment is the real thing.
    assert!(matches!(
        executor::execute(&checkpoint, &second.signed_bytes, &[], &bytes(&[first])).unwrap(),
        executor::Verdict::Applied { .. }
    ));
}

#[test]
fn an_author_no_supplied_key_names_parks_because_its_enrolment_may_be_withheld() {
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let operation = chain.add(1);
    let stranger = Dev::new(200);
    let enrolled = Dev::new(201);
    let op = ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::DeviceAdd {
            device_fingerprint: enrolled.fp,
            ed25519_pubkey: enrolled.ed,
            x25519_pubkey: enrolled.x,
            role: DeviceRole::Member,
            label: None,
        },
    };
    // Perfectly well-formed and correctly self-signed. Nothing supplied certifies its key — but a
    // v2 enrolment can introduce any key, so the receiver cannot tell "never enrolled" from
    // "enrolment withheld", and must not condemn a sound operation over an attachment either way.
    let foreign = envelope::sign_account_entry(
        &stranger.secret,
        &AccountEntryHeader {
            account_id: checkpoint.pin().account_id,
            log_id: 0,
            device_fingerprint: stranger.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: legacy::entry_type_of(&op.op),
            op_version: ops::CONTROL_VERSION,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: None,
        },
        &op.encode().unwrap(),
    )
    .unwrap();
    assert!(matches!(
        executor::execute(&checkpoint, &operation.signed_bytes, &[], &[foreign.signed_bytes])
            .unwrap(),
        executor::Verdict::Parked(executor::ParkCause::Signer)
    ));
    // Without the attachment the very same operation applies, so the park withheld nothing of its
    // own and one uncertified object cannot condemn it.
    assert!(matches!(
        executor::execute(&checkpoint, &operation.signed_bytes, &[], &[]).unwrap(),
        executor::Verdict::Applied { .. }
    ));
}

#[test]
fn a_member_cannot_promote_itself_into_the_authority_it_then_cites() {
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let member = Dev::new(77);
    // The founder enrols the member, so the accepted epoch certifies its KEY. That is all it
    // certifies: the roster says nothing about authority to act.
    let enrolment = chain.author(ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::DeviceAdd {
            device_fingerprint: member.fp,
            ed25519_pubkey: member.ed,
            x25519_pubkey: member.x,
            role: DeviceRole::Member,
            label: None,
        },
    });
    let sign = |seq: u64, prev: Option<AccountEntryHash>, op: ops::ControlOp| {
        envelope::sign_account_entry(
            &member.secret,
            &AccountEntryHeader {
                account_id: checkpoint.pin().account_id,
                log_id: 0,
                device_fingerprint: member.fp,
                seq,
                prev_hash: prev,
                parent_ref: prev,
                entry_type: legacy::entry_type_of(&op.op),
                op_version: ops::CONTROL_VERSION,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: (seq != 0).then(|| prev.unwrap().into()),
            },
            &op.encode().unwrap(),
        )
        .unwrap()
    };
    // A self-serving mint, then a cut of the founder's chain citing it.
    let mint = sign(0, None, ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::OwnerPromote { device_fingerprint: member.fp },
    });
    let view = manifest(&checkpoint, vec![]);
    let cut = sign(1, Some(mint.entry_hash), ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: Some(view.digest().unwrap()),
        op: AccountOp::DeviceRemove {
            device_fingerprint: device.fingerprint(),
            control_cut: Cut::Empty,
            secrets_cut: Cut::Empty,
            content_cuts: vec![],
            reason: "seized".into(),
        },
    });
    // Everything else about this bundle is in order: both entries authenticate under keys the
    // account certifies, both chain to a root this bundle supplies, and the view the cut names is
    // present. Only the authority rule stands between a Member and the founder's chain.
    let manifests = [view.encode().unwrap()];
    let evidence = bytes(&[enrolment, mint]);
    let verdict = executor::execute(&checkpoint, &cut.signed_bytes, &manifests, &evidence).unwrap();
    assert!(
        matches!(verdict, executor::Verdict::Rejected(executor::RejectCause::Inadmissible)),
        "a member's self-minted authority must be refused, got {verdict:?}",
    );
}

/// The mirror of the member's self-promotion: here the FOUNDER mints the incarnation, so the very
/// same operation is authorized the moment that one entry arrives. Withholding it must therefore
/// park — a receiver holding one bundle cannot tell an incarnation that never existed from one
/// whose mint it was not given, and a refusal that asking can clear is not a property of the
/// operation.
#[test]
fn a_cut_citing_a_mint_that_was_not_supplied_parks_until_that_mint_arrives() {
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let member = Dev::new(78);
    let enrolment = chain.author(ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::DeviceAdd {
            device_fingerprint: member.fp,
            ed25519_pubkey: member.ed,
            x25519_pubkey: member.x,
            role: DeviceRole::Member,
            label: None,
        },
    });
    let mint = chain.author(ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::OwnerPromote { device_fingerprint: member.fp },
    });
    let enrolled = Dev::new(79);
    let op = ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::DeviceAdd {
            device_fingerprint: enrolled.fp,
            ed25519_pubkey: enrolled.ed,
            x25519_pubkey: enrolled.x,
            role: DeviceRole::Member,
            label: None,
        },
    };
    // The member acts under the incarnation the founder minted for it, on its own origin slot.
    let operation = envelope::sign_account_entry(
        &member.secret,
        &AccountEntryHeader {
            account_id: checkpoint.pin().account_id,
            log_id: 0,
            device_fingerprint: member.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: legacy::entry_type_of(&op.op),
            op_version: ops::CONTROL_VERSION,
            crypto_suite: 0,
            auth_len: 1,
            key_id: None,
            authority_ref: Some(mint.entry_hash.into()),
        },
        &op.encode().unwrap(),
    )
    .unwrap();

    // The enrolment alone certifies the member's KEY, so nothing but its authority is outstanding.
    let evidence = bytes(&[enrolment, mint]);
    let verdict =
        executor::execute(&checkpoint, &operation.signed_bytes, &[], &evidence[..1]).unwrap();
    assert!(
        matches!(verdict, executor::Verdict::Parked(executor::ParkCause::Mint)),
        "a citation whose mint was not supplied cannot be refused for good, got {verdict:?}",
    );
    // Supplying that one entry authorizes the identical operation, which is exactly why the
    // refusal above could never have claimed permanence.
    let verdict = executor::execute(&checkpoint, &operation.signed_bytes, &[], &evidence).unwrap();
    assert!(
        matches!(verdict, executor::Verdict::Applied { .. }),
        "the same operation applies once its mint arrives, got {verdict:?}",
    );
}

fn plan_replay(
    checkpoint: &VerifiedCheckpoint,
    device: &LocalDevice,
    root: [u8; 32],
    manifests: &[Vec<u8>],
    evidence: &[Vec<u8>],
) -> Result<views::ReplayPlan, views::PlanError> {
    let consumer = signed(checkpoint, device, root, 255);
    views::plan_replay(checkpoint, &consumer.signed_bytes, manifests, evidence)
}
