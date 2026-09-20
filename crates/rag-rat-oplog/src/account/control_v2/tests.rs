use std::collections::HashSet;

use super::super::checkpoint::{self, TrustedCheckpointPin, VerifiedCheckpoint};
use super::super::cut::Cut;
use super::super::envelope::{self, AccountEntryHeader, SignedAccountEntry};
use super::super::fold;
use super::super::id::{AccountEntryHash, AccountId, OwnerId};
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

/// `distinguisher` only varies the operation's reason string, so two otherwise identical fixtures
/// hash differently. It is not a salt and nothing derives a key from it — the name matters because
/// a parameter called `salt` reads to a scanner as cryptographic material.
fn signed(
    checkpoint: &VerifiedCheckpoint,
    device: &LocalDevice,
    pre_cut_view: [u8; 32],
    distinguisher: u8,
) -> SignedAccountEntry {
    let mut op = revocation(false);
    op.checkpoint = checkpoint.pin().checkpoint_digest;
    op.pre_cut_view = Some(pre_cut_view);
    if let AccountOp::DeviceRemove { reason, .. } = &mut op.op {
        *reason = format!("revoked {distinguisher}");
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

/// A v2 operation signed by a device the frozen legacy epoch never certified — the shape a post-pin
/// v1 `DeviceAdd` leaves behind, since ingest resolves a key from any stored candidate while
/// execution certifies only the accepted epoch. Authored at the origin slot, so the ancestry walk
/// continues nothing and the entry parks on its signer rather than its chain.
fn uncertified(checkpoint: &VerifiedCheckpoint, seed: u8) -> SignedAccountEntry {
    let stranger = Dev::new(seed);
    let enrolled = Dev::new(seed.wrapping_add(1));
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
    envelope::sign_account_entry(
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
    .unwrap()
}

#[test]
fn an_entry_nothing_certifies_parks_itself_and_leaves_the_pool_applying() {
    // The pool is shared evidence, so a refusal raised INSIDE a bundle is a refusal of every
    // operation on the account. One entry signed by a key the accepted epoch never certified is
    // storable — ingest resolves keys from any stored candidate — and it must not silence the
    // operations that never needed it (#1395).
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let first = chain.add(0x31);
    let second = chain.add(0x32);
    let stranger = uncertified(&checkpoint, 0x41);

    let verdicts = executor::execute_held(
        &checkpoint,
        &bytes(&[first.clone(), second.clone(), stranger.clone()]),
        &[],
    );

    assert!(
        matches!(verdicts.get(&first.entry_hash), Some(executor::Verdict::Applied { .. })),
        "a sound operation applies beside an entry nothing certifies",
    );
    assert!(
        matches!(verdicts.get(&second.entry_hash), Some(executor::Verdict::Applied { .. })),
        "and so does every other sound operation in the pass",
    );
    assert!(
        matches!(
            verdicts.get(&stranger.entry_hash),
            Some(executor::Verdict::Parked(executor::ParkCause::Signer))
        ),
        "the uncertified entry parks on its own signer",
    );
}

#[test]
fn a_manifest_naming_another_checkpoint_leaves_the_pool_applying() {
    // `plan_replay` refuses the whole bundle over a manifest for a different checkpoint, and the
    // annex log cannot check a manifest against a pin, so one stored row would otherwise yield no
    // verdict for any operation (#1396).
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let sound = chain.add(0x33);
    let foreign =
        views::ViewManifest { checkpoint: [0xAA; 32], entries: Vec::new() }.encode().unwrap();

    let verdicts =
        executor::execute_held(&checkpoint, &bytes(std::slice::from_ref(&sound)), &[foreign]);

    assert!(
        matches!(verdicts.get(&sound.entry_hash), Some(executor::Verdict::Applied { .. })),
        "a manifest for another checkpoint is excluded, not a refusal of every operation",
    );
}

#[test]
fn one_view_held_twice_leaves_the_pool_applying() {
    // Two devices authoring cuts over the same pre-cut view produce byte-identical manifests, and
    // one manifest serving cuts from different authors is the documented property — so the
    // planner's duplicate refusal must never reach a bundle assembled from stored rows (#1396).
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let sound = chain.add(0x34);
    let view = manifest(&checkpoint, Vec::new()).encode().unwrap();

    let verdicts = executor::execute_held(&checkpoint, &bytes(std::slice::from_ref(&sound)), &[
        view.clone(),
        view,
    ]);

    assert!(
        matches!(verdicts.get(&sound.entry_hash), Some(executor::Verdict::Applied { .. })),
        "one view held twice is ordinary traffic, not a poisoned pool",
    );
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

/// A plan's evidence may not contain the very operation it is planning for.
///
/// This is bundle hygiene, not a self-citation defence — and the distinction matters, because a
/// later reader could otherwise trust this guard for a job it does not do. Self-counting is blocked
/// two layers down and independently: `execute` diverts any entry whose hash equals the plan's
/// consumer, and the credit pass builds its candidate set so the consumer appears exactly once.
/// Deleting this check changes no verdict. What it changes is whether a redundant object is an
/// error or is silently ignored, and this module's posture is that malformed input is an error.
#[test]
fn evidence_containing_its_own_consumer_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    // The same value is handed in as both the operation and its own evidence.
    let consumer = signed(&checkpoint, &device, root, 255);
    let manifests = [empty.encode().unwrap()];
    match views::plan_replay(
        &checkpoint,
        &consumer.signed_bytes,
        &manifests,
        std::slice::from_ref(&consumer.signed_bytes),
    ) {
        // The FULL message: `order_views` emits "pre-cut view contains its consumer", which shares
        // the shorter substring. That one is unreachable here only because a consumer committing to
        // a view that names it would be a hash fixed point — a property of SHA-256, not of this
        // assertion, so the assertion should not lean on it.
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("pre-cut evidence contains its consumer"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("a plan whose evidence contains its consumer must be refused"),
    }
}

/// Every candidate's `seq` must fit SQLite's signed INTEGER. A `u64` seq above `i64::MAX` decodes
/// and verifies fine but can never be stored, so it is refused at the plan boundary rather than
/// after a caller has already committed to replaying it.
#[test]
fn a_candidate_whose_seq_exceeds_sqlite_integer_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    // Everything a candidate needs, then the one field under test.
    let template = signed(&checkpoint, &device, root, 7);
    let mut header = template.header.clone();
    header.seq = i64::MAX as u64 + 1;
    let oversized =
        envelope::sign_account_entry(device.secret(), &header, &template.payload).unwrap();

    let manifests = [empty.encode().unwrap()];
    match plan_replay(
        &checkpoint,
        &device,
        root,
        &manifests,
        std::slice::from_ref(&oversized.signed_bytes),
    ) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("not a v2 control candidate"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("a seq above i64::MAX must be refused at the plan boundary"),
    }
}

/// Evidence must name THIS account. A candidate signed for another account verifies fine on
/// its own terms, so nothing downstream re-checks it — admitting one would let a plan replay a
/// foreign account's history as if it were this one's.
#[test]
fn evidence_signed_for_another_account_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    // Everything a candidate needs, then the one field under test.
    let template = signed(&checkpoint, &device, root, 7);
    let mut header = template.header.clone();
    header.account_id = AccountId::from_bytes([0xaf; 32]);
    let foreign =
        envelope::sign_account_entry(device.secret(), &header, &template.payload).unwrap();

    let manifests = [empty.encode().unwrap()];
    match plan_replay(
        &checkpoint,
        &device,
        root,
        &manifests,
        std::slice::from_ref(&foreign.signed_bytes),
    ) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("not a v2 control candidate"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("evidence for a foreign account must be refused"),
    }
}

/// Evidence must sit on the CONTROL log. An annex- or secrets-log entry carries no control
/// operation, so admitting one would put a payload the planner cannot judge into the order.
#[test]
fn evidence_on_another_log_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    // Everything a candidate needs, then the one field under test.
    let template = signed(&checkpoint, &device, root, 7);
    let mut header = template.header.clone();
    header.log_id = 3;
    let foreign =
        envelope::sign_account_entry(device.secret(), &header, &template.payload).unwrap();

    let manifests = [empty.encode().unwrap()];
    match plan_replay(
        &checkpoint,
        &device,
        root,
        &manifests,
        std::slice::from_ref(&foreign.signed_bytes),
    ) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("not a v2 control candidate"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("evidence off the control log must be refused"),
    }
}

/// Evidence must be at the control version this planner executes. A v1 entry decodes under a
/// different operation grammar, and the checkpoint is what fixes which grammar applies.
#[test]
fn evidence_at_another_op_version_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    // Everything a candidate needs, then the one field under test.
    let template = signed(&checkpoint, &device, root, 7);
    let mut header = template.header.clone();
    header.op_version = 1;
    let foreign =
        envelope::sign_account_entry(device.secret(), &header, &template.payload).unwrap();

    let manifests = [empty.encode().unwrap()];
    match plan_replay(
        &checkpoint,
        &device,
        root,
        &manifests,
        std::slice::from_ref(&foreign.signed_bytes),
    ) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("not a v2 control candidate"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("evidence at another op version must be refused"),
    }
}

/// Evidence must be plaintext, and this guard refuses on the DECLARED suite — before the payload
/// is looked at. Nothing stands behind it: the fixture below carries a conformant plaintext payload
/// under `crypto_suite = 1`, and with the suite check disabled the plan resolves, so `ops::decode`
/// is no backstop. Genuinely sealed bytes reaching it would be refused only by the accident of
/// ciphertext failing to parse as CBOR. Weakening this check does not fall through to a second line
/// of defense, because there is none.
#[test]
fn sealed_evidence_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    // Everything a candidate needs, then the one field under test.
    let template = signed(&checkpoint, &device, root, 7);
    let mut header = template.header.clone();
    header.crypto_suite = 1;
    // The envelope couples the two: a non-zero suite REQUIRES a key id, and signing refuses the
    // header outright otherwise — so without this the candidate never reaches the planner at all.
    header.key_id = Some([0x11; 32]);
    let foreign =
        envelope::sign_account_entry(device.secret(), &header, &template.payload).unwrap();

    let manifests = [empty.encode().unwrap()];
    match plan_replay(
        &checkpoint,
        &device,
        root,
        &manifests,
        std::slice::from_ref(&foreign.signed_bytes),
    ) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("not a v2 control candidate"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("sealed evidence must be refused"),
    }
}

/// The control for every refusal above. `plan_replay` decodes the CONSUMER before any evidence,
/// through the same `decode_candidate` and with the same refusal message — so if the shared
/// template ever stopped being admissible, those tests would still pass while asserting a message
/// the consumer produced and never examining their evidence at all.
///
/// It supplies the template as EVIDENCE rather than resolving a bare consumer, so it also covers
/// the evidence-side work the refusals never reach: the distinctness check against the consumer,
/// the duplicate-candidate insert, and the citation record. Ordering is NOT reachable from here —
/// the root view is empty, so the citations map is written and never read; that path is covered by
/// `diamond_dependencies_are_planned_once_in_dependency_order`, whose root names its candidates.
/// A new evidence-side refusal that the conformant template also tripped would otherwise leave
/// every test above green — each asserts the shared message, and the new failure produces it —
/// with nothing red to say the file went vacuous.
#[test]
fn the_shared_template_is_itself_admissible() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    let manifests = [empty.encode().unwrap()];
    // Distinguisher 7 against the consumer's 255: a distinct candidate, as the refusals all supply.
    let template = signed(&checkpoint, &device, root, 7);
    plan_replay(
        &checkpoint,
        &device,
        root,
        &manifests,
        std::slice::from_ref(&template.signed_bytes),
    )
    .expect("the conformant template must resolve, or the refusals above prove nothing");
}

/// One candidate must not enter the execution pool twice. The insert is the only thing that can
/// refuse this: both copies are distinct from the consumer, so the check above it passes for each,
/// and the payload decodes fine — a bundle that names the same entry twice would otherwise be
/// ordered with a candidate count that disagrees with the evidence it was built from.
///
/// `checkpoint_mismatch_and_duplicate_evidence_are_rejected` already reaches this guard, but every
/// one of its four assertions is a bare `Invalid(_)`: it goes red if the guard is DELETED and stays
/// green if the guard is replaced by any other refusal. This asserts the refusal's own message —
/// which is not the shared one — so it is attributable without a control, and it distinguishes this
/// guard from every sibling that reports through the same error variant.
#[test]
fn the_same_candidate_supplied_twice_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    let manifests = [empty.encode().unwrap()];
    let template = signed(&checkpoint, &device, root, 7);
    let twice = [template.signed_bytes.clone(), template.signed_bytes];
    match plan_replay(&checkpoint, &device, root, &manifests, &twice) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("duplicate v2 candidate"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("the same candidate supplied twice must be refused"),
    }
}

/// A candidate's PAYLOAD must name the checkpoint this plan is for. The header can be entirely
/// conformant while the operation inside commits to a different checkpoint — and since this same
/// decode is the membership test for the execution pool, admitting one would draw an operation
/// bound to another checkpoint into this account's refold.
#[test]
fn a_candidate_naming_another_checkpoint_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let root = empty.digest().unwrap();
    // A conformant header, and a payload naming a checkpoint that is not this plan's.
    let template = signed(&checkpoint, &device, root, 7);
    let mut op = revocation(false);
    op.checkpoint = [0x5c; 32];
    op.pre_cut_view = Some(root);
    let foreign =
        envelope::sign_account_entry(device.secret(), &template.header, &op.encode().unwrap())
            .unwrap();

    let manifests = [empty.encode().unwrap()];
    match plan_replay(
        &checkpoint,
        &device,
        root,
        &manifests,
        std::slice::from_ref(&foreign.signed_bytes),
    ) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("candidate checkpoint mismatch"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("a candidate naming another checkpoint must be refused"),
    }
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

/// A view must name the checkpoint its plan is for. The digest inside the manifest is the only
/// thing binding it, `decode_manifest` will decode one built for any checkpoint, and nothing
/// downstream re-checks it — so admitting one would fold another checkpoint's references into this
/// account's refold.
#[test]
fn a_manifest_for_another_checkpoint_is_refused() {
    let (checkpoint, device) = checkpoint();
    let foreign = views::ViewManifest { checkpoint: [99; 32], entries: vec![] };
    match plan_replay(
        &checkpoint,
        &device,
        foreign.digest().unwrap(),
        &[foreign.encode().unwrap()],
        &[],
    ) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("view checkpoint mismatch"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("a manifest for another checkpoint must be refused"),
    }
}

/// One view must not enter the plan twice. Views are keyed by digest, so a second copy is the same
/// key: without the refusal the duplicate is silently absorbed and the bundle's declared view count
/// stops matching what the plan folds.
#[test]
fn the_same_view_supplied_twice_is_refused() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    let bytes = empty.encode().unwrap();
    match plan_replay(&checkpoint, &device, empty.digest().unwrap(), &[bytes.clone(), bytes], &[]) {
        Err(views::PlanError::Invalid(error)) =>
            assert!(error.to_string().contains("duplicate view"), "unexpected refusal: {error}",),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("the same view supplied twice must be refused"),
    }
}

/// The checkpoint's own legacy evidence is v1 and is implicit in every view already. Handing it
/// back as v2 candidate evidence must be refused by the same decode every candidate goes through,
/// rather than quietly double-counting history the plan already carries.
#[test]
fn the_checkpoints_legacy_evidence_is_not_v2_candidate_evidence() {
    let (checkpoint, device) = checkpoint();
    let empty = manifest(&checkpoint, vec![]);
    match plan_replay(
        &checkpoint,
        &device,
        empty.digest().unwrap(),
        &[empty.encode().unwrap()],
        &checkpoint.bundle().evidence,
    ) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("not a v2 control candidate"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("legacy evidence must not be admitted as v2 candidates"),
    }
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

/// `MAX_VIEWS` is checked before ANYTHING is decoded, which is why the fixture need not be a
/// well-formed manifest — and is what makes the bound cheap to cross: a few thousand one-byte
/// objects rather than a few thousand signed entries.
///
/// The precondition is load-bearing, so it has to count what `plan_replay` counts. The byte budget
/// is checked immediately after this bound and spans the manifests, the evidence AND the consuming
/// operation, so the consumer's own length belongs in the sum; a fixture crossing both limits would
/// be refused either way and the test would pin nothing.
#[test]
fn more_views_than_the_bundle_may_carry_is_refused() {
    let (checkpoint, device) = checkpoint();
    let manifests = vec![vec![0u8; 1]; views::MAX_VIEWS + 1];
    // The same consumer the helper below builds, so the sum matches the one the guard sees.
    let consumer = signed(&checkpoint, &device, [0; 32], 255).signed_bytes.len();
    assert!(
        consumer + manifests.iter().map(Vec::len).sum::<usize>() < views::MAX_BYTES,
        "the count bound must be what refuses here, not the byte budget below it",
    );
    match plan_replay(&checkpoint, &device, [0; 32], &manifests, &[]) {
        Err(views::PlanError::Invalid(error)) =>
            assert!(error.to_string().contains("too many views"), "unexpected refusal: {error}",),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("more views than the bundle may carry must be refused"),
    }
}

/// `MAX_ENTRIES` has the same shape as the view bound above: checked before any candidate is
/// decoded, so the fixture is raw bytes rather than signed entries, and its precondition counts the
/// consuming operation alongside the evidence because the byte budget below it does.
#[test]
fn more_v2_entries_than_the_bundle_may_carry_is_refused() {
    let (checkpoint, device) = checkpoint();
    let evidence = vec![vec![0u8; 1]; views::MAX_ENTRIES + 1];
    // The same consumer the helper below builds, so the sum matches the one the guard sees.
    let consumer = signed(&checkpoint, &device, [0; 32], 255).signed_bytes.len();
    assert!(
        consumer + evidence.iter().map(Vec::len).sum::<usize>() < views::MAX_BYTES,
        "the count bound must be what refuses here, not the byte budget below it",
    );
    match plan_replay(&checkpoint, &device, [0; 32], &[], &evidence) {
        Err(views::PlanError::Invalid(error)) => assert!(
            error.to_string().contains("too many v2 entries"),
            "unexpected refusal: {error}",
        ),
        Err(other) => panic!("expected an invalid-plan refusal, got {other:?}"),
        Ok(_) => panic!("more v2 entries than the bundle may carry must be refused"),
    }
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
        let executor::Verdict::Applied { registers, credit, .. } = verdict else {
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

/// The depth-2 case, and the reason the classifier walks the citation chain instead of asking about
/// the entry an operation directly cites. The mint this operation names IS supplied; the mint THAT
/// one cites is not. Judging one link answers `Inadmissible` here — refusing for good an operation
/// that the single withheld entry authorizes.
#[test]
fn a_citation_chain_parks_on_a_withheld_link_two_mints_up() {
    let (checkpoint, device) = checkpoint();
    let mut chain = Chain::new(&checkpoint, &device);
    let first = Dev::new(81);
    let second = Dev::new(82);
    let enrol = |dev: &Dev| ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::DeviceAdd {
            device_fingerprint: dev.fp,
            ed25519_pubkey: dev.ed,
            x25519_pubkey: dev.x,
            role: DeviceRole::Member,
            label: None,
        },
    };
    let enrolments = [chain.author(enrol(&first)), chain.author(enrol(&second))];
    // The withheld link. It sits at the TIP of the founder's chain, so nothing else supplied needs
    // it to walk its own ancestry and the operation reaches the authority rule.
    let promote_first = chain.author(ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::OwnerPromote { device_fingerprint: first.fp },
    });
    let sign = |dev: &Dev, authority: AccountEntryHash, op: ops::ControlOp| {
        envelope::sign_account_entry(
            &dev.secret,
            &AccountEntryHeader {
                account_id: checkpoint.pin().account_id,
                log_id: 0,
                device_fingerprint: dev.fp,
                seq: 0,
                prev_hash: None,
                parent_ref: None,
                entry_type: legacy::entry_type_of(&op.op),
                op_version: ops::CONTROL_VERSION,
                crypto_suite: 0,
                auth_len: 1,
                key_id: None,
                authority_ref: Some(authority.into()),
            },
            &op.encode().unwrap(),
        )
        .unwrap()
    };
    // `first` promotes `second` under the incarnation the withheld entry opened, and `second` acts
    // under that one — two mints from anything the checkpoint itself holds.
    let promote_second = sign(&first, promote_first.entry_hash, ops::ControlOp {
        checkpoint: checkpoint.pin().checkpoint_digest,
        pre_cut_view: None,
        op: AccountOp::OwnerPromote { device_fingerprint: second.fp },
    });
    let operation = sign(&second, promote_second.entry_hash, enrol(&Dev::new(83)));

    let mut evidence = bytes(&enrolments);
    evidence.push(promote_second.signed_bytes);
    let verdict = executor::execute(&checkpoint, &operation.signed_bytes, &[], &evidence).unwrap();
    assert!(
        matches!(verdict, executor::Verdict::Parked(executor::ParkCause::Mint)),
        "the mint it cites is held; the mint THAT one cites is not, got {verdict:?}",
    );
    // The whole chain resolves once that one entry arrives, so no link of it was ever refusable.
    evidence.push(promote_first.signed_bytes);
    let verdict = executor::execute(&checkpoint, &operation.signed_bytes, &[], &evidence).unwrap();
    assert!(
        matches!(verdict, executor::Verdict::Applied { .. }),
        "the same operation applies once the withheld link arrives, got {verdict:?}",
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
