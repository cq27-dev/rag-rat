use std::collections::HashSet;

use super::super::checkpoint::{self, TrustedCheckpointPin, VerifiedCheckpoint};
use super::super::cut::Cut;
use super::super::envelope::{self, AccountEntryHeader, SignedAccountEntry};
use super::super::fold;
use super::super::id::AccountEntryHash;
use super::super::ops::{self as legacy, AccountOp, DeviceCut};
use super::{ops, views};
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
    ops::ControlOp {
        checkpoint: [1; 32],
        pre_cut_view: Some([2; 32]),
        op,
        credit_frontier: Some(vec![]),
    }
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

#[test]
fn revocations_bind_both_view_and_frontier_without_changing_v1_bytes() {
    for demote in [false, true] {
        let mut op = revocation(demote);
        op.credit_frontier = Some(vec![DeviceCut {
            device_fingerprint: DeviceFingerprint::from_bytes([7; 32]),
            seq: 4,
            hash: [8; 32].into(),
        }]);
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
fn view_and_frontier_slots_are_present_exactly_for_a_revocation() {
    let mut op = revocation(false);
    op.credit_frontier = None;
    assert!(op.encode().is_err(), "a revocation carries a frontier");
    op.credit_frontier = Some(vec![]);
    op.pre_cut_view = None;
    assert!(op.encode().is_err(), "a revocation nominates a view");
    op.pre_cut_view = Some([2; 32]);
    let head = DeviceCut {
        device_fingerprint: DeviceFingerprint::from_bytes([3; 32]),
        seq: 0,
        hash: [4; 32].into(),
    };
    op.credit_frontier = Some(vec![head.clone(), head.clone()]);
    assert!(op.encode().is_err());
    op.credit_frontier = Some(vec![DeviceCut { seq: u64::MAX, ..head.clone() }]);
    assert!(op.encode().is_err());
    op.credit_frontier = Some(vec![head; ops::FRONTIER_MAX + 1]);
    assert!(op.encode().is_err());

    // An ordinary operation carries neither, and needs no historical evidence to be read.
    op.op = AccountOp::OwnerPromote { device_fingerprint: DeviceFingerprint::from_bytes([3; 32]) };
    op.credit_frontier = Some(vec![]);
    assert!(op.encode().is_err());
    op.credit_frontier = None;
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
fn no_lifetime_limit_is_stricter_than_the_declared_evidence_budget() {
    // Every view and every reference has to be supplied as bytes, so the storage budget already
    // bounds the work. A tighter view or reference cap would only limit how long an account may
    // keep revoking, which is not a bound on anything a receiver spends.
    assert_eq!(views::MAX_VIEWS, views::MAX_ENTRIES);
    assert_eq!(views::MAX_REFERENCES, views::MAX_BYTES / 32);
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
