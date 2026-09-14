use super::super::test_support;
use super::*;
use crate::account::envelope::{sign_account_entry, verify_account_signed};
use crate::account::test_support::Dev;
use crate::account::{AccountId, ops as account_ops, snapshot};
use crate::device::{DeviceSecret, DeviceX25519Secret};
use crate::stream::StreamId;

impl Dev {
    /// A distinct device from a wide index (`new` only spans a `u8`) — for building long
    /// chains.
    fn seeded(i: u32) -> Self {
        let mut ed_seed = [0u8; 32];
        ed_seed[..4].copy_from_slice(&i.to_le_bytes());
        let secret = DeviceSecret::from_seed(&ed_seed);
        let public = secret.public();
        let mut x_seed = ed_seed;
        x_seed[8] = 0x80;
        let x = DeviceX25519Secret::from_seed(&x_seed).public().to_bytes();
        Dev { fp: public.fingerprint(), ed: public.to_bytes(), x, secret }
    }
}

/// Authors a real signed account (genesis + arbitrary ops) so the fold runs over verified
/// entries. Tests control each op's (author, authority_ref, payload) to build exact traces; the
/// harness threads the per-device seq/prev chains.
#[derive(Clone)]
struct Fixture {
    account_id: AccountId,
    genesis_hash: AccountEntryHash,
    chains: HashMap<[u8; 32], (u64, Option<[u8; 32]>)>,
    entries: Vec<VerifiedAccountEntry>,
}

impl Fixture {
    fn genesis(founder: &Dev) -> Self {
        let op = AccountOp::AccountGenesis {
            ed25519_pubkey: founder.ed,
            x25519_pubkey: founder.x,
            nonce16: [0u8; 16],
            created_at_ms: 1_700_000_000_000,
            label: None,
        };
        let payload = account_ops::encode(&op).unwrap();
        let account_id = id::account_id_from_genesis_payload(&payload);
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: None,
            entry_type: account_ops::entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            crypto_suite: 0,
            auth_len: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
        let verified =
            verify_account_signed(&signed.signed_bytes, &founder.secret.public()).unwrap();
        let genesis_hash = verified.entry_hash;
        let mut fixture =
            Fixture { account_id, genesis_hash, chains: HashMap::new(), entries: Vec::new() };
        fixture.chains.insert(founder.fp.to_bytes(), (1, Some(genesis_hash.into())));
        fixture.entries.push(verified);
        fixture
    }

    /// Author `op` as `author` citing `authority_ref`. Returns the entry_hash.
    fn author(&mut self, author: &Dev, authority_ref: Option<OwnerId>, op: &AccountOp) -> [u8; 32] {
        self.author_at_auth_len(author, authority_ref, op, 1)
    }

    fn author_at_auth_len(
        &mut self,
        author: &Dev,
        authority_ref: Option<OwnerId>,
        op: &AccountOp,
        auth_len: u64,
    ) -> [u8; 32] {
        let payload = account_ops::encode(op).unwrap();
        let (seq, prev) = self.chains.get(&author.fp.to_bytes()).copied().unwrap_or((0, None));
        let header = AccountEntryHeader {
            account_id: self.account_id,
            log_id: 0,
            device_fingerprint: author.fp,
            seq,
            prev_hash: prev.map(Into::into),
            parent_ref: Some(self.genesis_hash),
            entry_type: account_ops::entry_type_of(op),
            op_version: 1,
            auth_len,
            crypto_suite: 0,
            key_id: None,
            authority_ref,
        };
        let signed = sign_account_entry(&author.secret, &header, &payload).unwrap();
        let verified =
            verify_account_signed(&signed.signed_bytes, &author.secret.public()).unwrap();
        let hash = verified.entry_hash;
        self.chains.insert(author.fp.to_bytes(), (seq + 1, Some(hash.into())));
        self.entries.push(verified);
        hash.into()
    }

    /// Author `op` at an EXPLICIT `(seq, prev_hash)` without advancing the device's main chain
    /// — used to forge an equivocating sibling entry (an off-branch fork) for
    /// revocation tests.
    fn author_forked(
        &mut self,
        author: &Dev,
        authority_ref: Option<OwnerId>,
        op: &AccountOp,
        seq: u64,
        prev_hash: Option<AccountEntryHash>,
    ) -> [u8; 32] {
        let payload = account_ops::encode(op).unwrap();
        let header = AccountEntryHeader {
            account_id: self.account_id,
            log_id: 0,
            device_fingerprint: author.fp,
            seq,
            prev_hash,
            parent_ref: Some(self.genesis_hash),
            entry_type: account_ops::entry_type_of(op),
            op_version: 1,
            auth_len: 1,
            crypto_suite: 0,
            key_id: None,
            authority_ref,
        };
        let signed = sign_account_entry(&author.secret, &header, &payload).unwrap();
        let verified =
            verify_account_signed(&signed.signed_bytes, &author.secret.public()).unwrap();
        let hash = verified.entry_hash;
        self.entries.push(verified);
        hash.into()
    }

    /// Author a raw log-1 (secrets) entry at an explicit `(seq, prev_hash)` on `author`'s
    /// secrets chain. The control fold never FOLDS a log-1 entry — it only reads its HEADER for
    /// cut-target binding + ancestry — so the payload is opaque here; any verifiable entry
    /// serves as a secrets-chain watermark / ancestry target. `filler` only varies the opaque
    /// payload, so two entries at the SAME `(seq, prev_hash)` with DIFFERENT `filler`s are
    /// distinct-hash siblings (an equivocation at one slot). (A `DeviceAdd` payload is a handy
    /// opaque body; on log 1 it is retained-unfolded, never interpreted as a control op.)
    fn author_secrets_entry(
        &mut self,
        author: &Dev,
        filler: &Dev,
        seq: u64,
        prev_hash: Option<AccountEntryHash>,
    ) -> [u8; 32] {
        let op = device_add(filler, DeviceRole::Member);
        let payload = account_ops::encode(&op).unwrap();
        let header = AccountEntryHeader {
            account_id: self.account_id,
            log_id: SECRETS_LOG,
            device_fingerprint: author.fp,
            seq,
            prev_hash,
            parent_ref: Some(self.genesis_hash),
            entry_type: account_ops::entry_type_of(&op),
            op_version: 1,
            auth_len: 1,
            crypto_suite: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(&author.secret, &header, &payload).unwrap();
        let verified =
            verify_account_signed(&signed.signed_bytes, &author.secret.public()).unwrap();
        let hash = verified.entry_hash;
        self.entries.push(verified);
        hash.into()
    }

    fn fold(&self) -> AccountAuthHistory {
        fold_account(&self.entries)
    }

    /// Fold the entries in a rotated order — arrival order must not change the result (I9).
    fn fold_rotated(&self, rot: usize) -> AccountAuthHistory {
        let mut e = self.entries.clone();
        let n = e.len().max(1);
        e.rotate_left(rot % n);
        fold_account(&e)
    }

    /// Fold every entry EXCEPT `exclude` — models a watermark (or any entry) not yet synced.
    fn fold_without(&self, exclude: [u8; 32]) -> AccountAuthHistory {
        let held: Vec<VerifiedAccountEntry> = self
            .entries
            .iter()
            .filter(|e| e.entry_hash != AccountEntryHash::from_bytes(exclude))
            .cloned()
            .collect();
        fold_account(&held)
    }

    fn effective_set(history: &AccountAuthHistory) -> HashSet<AccountEntryHash> {
        history.outcomes.iter().filter(|(_, o)| o.is_effective()).map(|(h, _)| *h).collect()
    }
}

// ---- C6b: the canonical projection (#609) ----

/// A fixture exercising every collection the projection binds: two owners, a member, a removed
/// device (tombstone + cuts), stream ownership, and a grant.
fn projection_fixture() -> Fixture {
    let founder = Dev::new(1);
    let owner_b = Dev::new(2);
    let member = Dev::new(3);
    let removed = Dev::new(4);
    let mut f = Fixture::genesis(&founder);
    let g = f.genesis_hash;
    f.author(&founder, Some(g.into()), &device_add(&owner_b, DeviceRole::Owner));
    f.author(&founder, Some(g.into()), &device_add(&member, DeviceRole::Member));
    f.author(&founder, Some(g.into()), &device_add(&removed, DeviceRole::Member));
    let (stream_id, own) = test_support::stream_own_public(f.account_id);
    f.author(&founder, Some(g.into()), &own);
    f.author(&founder, Some(g.into()), &AccountOp::StreamGrant {
        stream_id,
        grantee_account_id: AccountId::from_bytes([0x9a; 32]),
        grant_role: GrantRole::Writer,
    });
    // `Cut::Empty` because `removed` never authored an entry: a `Cut::At` would name a
    // coordinate on its chain that does not exist, and the remove would park
    // `unknown_cut_target` instead of taking effect — leaving the fixture with no tombstone at
    // all, which is exactly what `a_different_covered_prefix_projects_differently` caught.
    f.author(&founder, Some(g.into()), &AccountOp::DeviceRemove {
        device_fingerprint: removed.fp,
        control_cut: Cut::Empty,
        secrets_cut: Cut::Empty,
        content_cuts: Vec::new(),
        reason: "revoked".to_string(),
    });
    f
}

/// THE determinism tripwire. Every collection the projection reads is a `HashMap`/`HashSet`,
/// whose iteration order varies run to run and between peers. Arrival order must not change one
/// byte — this is what makes `folded_state_hash` a claim two honest devices can both compute.
///
/// It fails the moment any collection is iterated straight into the encoder instead of being
/// sorted first.
#[test]
fn a_shuffled_fold_produces_identical_projection_bytes() {
    let f = projection_fixture();
    let baseline = snapshot::projection::encoded(&f.fold());
    assert!(baseline.len() > 200, "the fixture must exercise a non-trivial projection");
    for rot in 0..f.entries.len() {
        assert_eq!(
            snapshot::projection::encoded(&f.fold_rotated(rot)),
            baseline,
            "arrival order rotated by {rot} changed the canonical bytes",
        );
    }
}

// ---- C6b-ii: read-time verification (#609) ----

/// Build the manifest target a HONEST author would publish for this fixture: every device's
/// control-chain head, and the hash its own fold produces.
fn honest_target(f: &Fixture) -> snapshot::ops::SnapshotTarget {
    let history = f.fold();
    let mut heads: HashMap<DeviceFingerprint, (u64, [u8; 32])> = HashMap::new();
    for entry in &f.entries {
        let h = &entry.header;
        let slot = heads.entry(h.device_fingerprint).or_insert((h.seq, entry.entry_hash.into()));
        if h.seq >= slot.0 {
            *slot = (h.seq, entry.entry_hash.into());
        }
    }
    snapshot::ops::SnapshotTarget {
        log_id: 0,
        stream_id: None,
        subject_account_id: None,
        folded_state_hash: snapshot::projection::folded_state_hash(&history),
        covered: heads
            .into_iter()
            .map(|(device_fingerprint, (seq, entry_hash))| snapshot::ops::CoveredWatermark {
                device_fingerprint,
                seq,
                entry_hash: AccountEntryHash::from_bytes(entry_hash),
            })
            .collect(),
    }
}

#[test]
fn an_honest_snapshot_verifies_against_a_local_refold() {
    let f = projection_fixture();
    assert_eq!(
        snapshot::verify::verify_snapshot(&f.entries, &[honest_target(&f)]),
        snapshot::verify::SnapshotVerdict::Verified,
    );
}

#[test]
fn a_false_coverage_claim_is_a_mismatch() {
    // The point of the hash: a claim that does not match the covered prefix is detectable.
    let f = projection_fixture();
    let mut lying = honest_target(&f);
    lying.folded_state_hash = [0xff; 32];
    assert_eq!(
        snapshot::verify::verify_snapshot(&f.entries, &[lying]),
        snapshot::verify::SnapshotVerdict::Mismatch,
    );
}

/// THE FIREWALL PROPERTY. Verification is device-dependent by nature, so it must be advisory:
/// a device that lacks the covered history reports `Unverifiable`, NEVER a judgement. If this
/// ever returned `Mismatch` for missing history, two peers at different sync progress would
/// disagree about the same signed entry.
#[test]
fn a_device_lacking_the_covered_history_reports_unverifiable_not_a_judgement() {
    let f = projection_fixture();
    let target = honest_target(&f);

    // Hold nothing at all: the heads themselves are absent.
    assert_eq!(
        snapshot::verify::verify_snapshot(&[], std::slice::from_ref(&target)),
        snapshot::verify::SnapshotVerdict::Unverifiable(
            snapshot::verify::Unverifiable::WatermarkNotHeld
        ),
    );

    // Hold the heads but not a link beneath them: a claim this device cannot reconstruct.
    let heads: Vec<_> = f
        .entries
        .iter()
        .filter(|e| target.covered.iter().any(|w| w.entry_hash == e.entry_hash))
        .cloned()
        .collect();
    assert!(heads.len() < f.entries.len(), "the fixture must have interior entries");
    assert_eq!(
        snapshot::verify::verify_snapshot(&heads, &[target]),
        snapshot::verify::SnapshotVerdict::Unverifiable(
            snapshot::verify::Unverifiable::IncompleteChain
        ),
    );
}

/// The attack the branch restriction would otherwise enable. Verification folds only the chain
/// a watermark names — that is what makes the hash deterministic between honest peers — but it
/// also means the AUTHOR picks which branch gets hashed. An author who equivocates and then
/// snapshots the clean side produces a claim that is perfectly true about that branch.
///
/// So a device that HOLDS the sibling must refuse it. Without this, checking `Live` over the
/// covered input would be inspecting the author's own selection: that fold is `Live` by
/// construction, because the evidence contradicting it was left out.
#[test]
fn a_snapshot_of_one_branch_is_refused_by_a_device_holding_the_equivocation() {
    let founder = Dev::new(1);
    let sibling_target = Dev::new(8);
    let mut f = projection_fixture();
    let target = honest_target(&f);

    // Without the sibling, the claim verifies — the branch it names is real.
    assert_eq!(
        snapshot::verify::verify_snapshot(&f.entries, std::slice::from_ref(&target)),
        snapshot::verify::SnapshotVerdict::Verified,
    );

    // Now equivocate at a covered coordinate: a second entry at a `(device, seq)` the claim's
    // chain already occupies. The claim is unchanged and still true about its own branch.
    let covered = target.covered.iter().find(|w| w.device_fingerprint == founder.fp).cloned();
    let covered = covered.expect("the founder's chain is covered");
    let held_entry = f
        .entries
        .iter()
        .find(|e| e.entry_hash == covered.entry_hash)
        .expect("the covered head is held");
    let (seq, prev) = (held_entry.header.seq, held_entry.header.prev_hash);
    let g = f.genesis_hash;
    f.author_forked(
        &founder,
        Some(g.into()),
        &device_add(&sibling_target, DeviceRole::Member),
        seq,
        prev,
    );

    assert_eq!(
        snapshot::verify::verify_snapshot(&f.entries, &[target]),
        snapshot::verify::SnapshotVerdict::IgnoresHeldEvidence,
        "a device holding the sibling must not trust a snapshot that folded only one branch",
    );
}

#[test]
fn a_target_naming_an_unsupported_log_is_unverifiable_not_failed() {
    // The wire admits secrets/content targets so #406 needs no bump; this binary has no
    // projection for them and must say so rather than call the snapshot wrong.
    let f = projection_fixture();
    let secrets_target = snapshot::ops::SnapshotTarget { log_id: 1, ..honest_target(&f) };
    assert_eq!(
        snapshot::verify::verify_snapshot(&f.entries, &[secrets_target]),
        snapshot::verify::SnapshotVerdict::Unverifiable(
            snapshot::verify::Unverifiable::UnsupportedTargets
        ),
    );
}

#[test]
fn a_forged_chain_link_is_not_walkable_into_the_verification_input() {
    // A signed header pins `prev_hash` NULLITY, not that its parent is a contiguous link on the
    // same coordinate. A watermark whose seq disagrees with the entry it names must not fold.
    let f = projection_fixture();
    let mut forged = honest_target(&f);
    forged.covered[0].seq = forged.covered[0].seq.wrapping_add(7);
    assert_eq!(
        snapshot::verify::verify_snapshot(&f.entries, &[forged]),
        snapshot::verify::SnapshotVerdict::Unverifiable(
            snapshot::verify::Unverifiable::IncompleteChain
        ),
    );
}

/// The projection must be ONE complete, canonical CBOR item — not a well-formed prefix followed
/// by trailing values. A golden hash alone cannot catch that: it freezes whatever bytes the
/// encoder produces, malformed or not, which is exactly how a short top-level array survived
/// into a pinned vector. This checks the shape rather than the digest.
#[test]
fn the_projection_is_one_complete_canonical_cbor_item() {
    let bytes = snapshot::projection::encoded(&projection_fixture().fold());
    crate::cbor::require_canonical_cbor(&bytes)
        .expect("the canonical projection must decode as exactly one canonical CBOR item");
}

/// The canonical encoding is a FROZEN WIRE: two honest devices must agree byte-for-byte, and a
/// hash computed under one encoding is meaningless under another. Determinism and sensitivity
/// tests both keep passing if the encoding silently changes shape, so pin the bytes.
///
/// If this fails, you changed what `folded_state_hash` covers — that is a
/// `SNAPSHOT_STATE_FORMAT_V1` bump, not a refactor.
#[test]
fn golden_projection_pins_the_canonical_encoding() {
    let hash = snapshot::projection::folded_state_hash(&projection_fixture().fold());
    // Recomputed when the fixture's `stream_own` moved to `PublicRead` (grants fold only on
    // public streams): the ENCODING is unchanged — the fixture's stream id and spec bytes
    // are what moved. A change to what `folded_state_hash` covers is still a
    // `SNAPSHOT_STATE_FORMAT_V1` bump.
    assert_eq!(
        rag_rat_base::hash::hex_lower(&hash),
        "5c8aee143bb180044289c4aeee5ad8279a7cb66e51091120d57614e7545604b0",
    );
}

/// The hash must actually depend on the state it claims to bind. Without this, a projection
/// that silently dropped a collection would still pass the determinism test above.
#[test]
fn the_projection_hash_moves_with_every_bound_collection() {
    let base = snapshot::projection::folded_state_hash(&projection_fixture().fold());

    // A roster/effective-set change.
    let mut roster = projection_fixture();
    let g = roster.genesis_hash;
    roster.author(&Dev::new(1), Some(g.into()), &device_add(&Dev::new(7), DeviceRole::Member));
    assert_ne!(
        snapshot::projection::folded_state_hash(&roster.fold()),
        base,
        "an added device must change the hash",
    );

    // A tombstone change (I4's set is bound, so a bootstrap cannot re-admit a removed device).
    let mut tombstone = projection_fixture();
    let g = tombstone.genesis_hash;
    let victim = Dev::new(3);
    tombstone.author(&Dev::new(1), Some(g.into()), &AccountOp::DeviceRemove {
        device_fingerprint: victim.fp,
        control_cut: Cut::Empty,
        secrets_cut: Cut::Empty,
        content_cuts: Vec::new(),
        reason: "revoked".to_string(),
    });
    assert_ne!(
        snapshot::projection::folded_state_hash(&tombstone.fold()),
        base,
        "a tombstoned device must change the hash",
    );

    // A grant change.
    let mut grant = projection_fixture();
    let g = grant.genesis_hash;
    let (stream_id, _) = test_support::stream_own_public(grant.account_id);
    grant.author(&Dev::new(1), Some(g.into()), &AccountOp::StreamGrant {
        stream_id,
        grantee_account_id: AccountId::from_bytes([0xbe; 32]),
        grant_role: GrantRole::Reader,
    });
    assert_ne!(
        snapshot::projection::folded_state_hash(&grant.fold()),
        base,
        "an added grant must change the hash",
    );
}

/// A held-back entry changes the fold, so it must change the projection — this is what makes a
/// coverage claim meaningful rather than a constant.
#[test]
fn a_different_covered_prefix_projects_differently() {
    let f = projection_fixture();
    let full = snapshot::projection::folded_state_hash(&f.fold());
    let withheld = f.entries.last().expect("fixture has entries").entry_hash;
    assert_ne!(
        snapshot::projection::folded_state_hash(&f.fold_without(withheld.into())),
        full,
        "folding a shorter prefix must project differently",
    );
}

fn device_add(dev: &Dev, role: DeviceRole) -> AccountOp {
    AccountOp::DeviceAdd {
        device_fingerprint: dev.fp,
        ed25519_pubkey: dev.ed,
        x25519_pubkey: dev.x,
        role,
        label: None,
    }
}

fn device_remove(dev: &Dev, control_cut: Cut) -> AccountOp {
    AccountOp::DeviceRemove {
        device_fingerprint: dev.fp,
        control_cut,
        secrets_cut: Cut::Empty,
        content_cuts: Vec::new(),
        reason: "revoked".to_string(),
    }
}

fn owner_demote(dev: &Dev, owner_id: OwnerId, control_cut: Cut) -> AccountOp {
    AccountOp::OwnerDemote {
        device_fingerprint: dev.fp,
        owner_id,
        control_cut,
        secrets_cut: Cut::Empty,
        reason: "demoted".to_string(),
    }
}

/// A `DeviceRemove` that cuts BOTH the device's control chain and its secrets chain — the
/// vehicle for exercising the secrets-chain register.
fn device_remove_with_secrets(dev: &Dev, control_cut: Cut, secrets_cut: Cut) -> AccountOp {
    AccountOp::DeviceRemove {
        device_fingerprint: dev.fp,
        control_cut,
        secrets_cut,
        content_cuts: Vec::new(),
        reason: "revoked".to_string(),
    }
}

/// An `OwnerDemote` that cuts BOTH the incarnation's control chain and its secrets chain.
fn owner_demote_with_secrets(
    dev: &Dev,
    owner_id: OwnerId,
    control_cut: Cut,
    secrets_cut: Cut,
) -> AccountOp {
    AccountOp::OwnerDemote {
        device_fingerprint: dev.fp,
        owner_id,
        control_cut,
        secrets_cut,
        reason: "demoted".to_string(),
    }
}

fn owner_promote(dev: &Dev) -> AccountOp {
    AccountOp::OwnerPromote { device_fingerprint: dev.fp }
}

fn account_reroot(successor: AccountId) -> AccountOp {
    AccountOp::AccountReRoot { successor_account_id: successor, note: None }
}

fn stream_grant(stream: StreamId, grantee: AccountId) -> AccountOp {
    AccountOp::StreamGrant {
        stream_id: stream,
        grantee_account_id: grantee,
        grant_role: ops::GrantRole::Reader,
    }
}

fn stream_revoke(stream: StreamId, grantee: AccountId, grant_id: GrantId) -> AccountOp {
    AccountOp::StreamRevoke {
        stream_id: stream,
        grantee_account_id: grantee,
        grant_id,
        device_cuts: Vec::new(),
        reason: "access ended".to_string(),
    }
}

/// A control-log `CutExtend` raising `subject`'s register (device-level when `incarnation_id`
/// is `None`, owner-incarnation otherwise) to `[new_seq, new_entry_hash]`.
fn cut_extend_ctrl(
    account: AccountId,
    subject: &Dev,
    incarnation_id: Option<AccountEntryHash>,
    new_seq: u64,
    new_entry_hash: AccountEntryHash,
) -> AccountOp {
    AccountOp::CutExtend {
        chain_kind: ops::ChainKind::Ctrl,
        stream_id: None,
        incarnation_id,
        subject_account_id: account,
        device_fingerprint: subject.fp,
        new_seq,
        new_entry_hash,
    }
}

/// A `CutExtend` raising `subject`'s SECRETS-chain register (device-level when `incarnation_id`
/// is `None`, owner-incarnation otherwise) to `[new_seq, new_entry_hash]`.
fn cut_extend_secrets(
    account: AccountId,
    subject: &Dev,
    incarnation_id: Option<AccountEntryHash>,
    new_seq: u64,
    new_entry_hash: AccountEntryHash,
) -> AccountOp {
    AccountOp::CutExtend {
        chain_kind: ops::ChainKind::Secrets,
        stream_id: None,
        incarnation_id,
        subject_account_id: account,
        device_fingerprint: subject.fp,
        new_seq,
        new_entry_hash,
    }
}

#[test]
fn genesis_is_effective() {
    let founder = Dev::new(1);
    let f = Fixture::genesis(&founder);
    let h = f.fold();
    assert!(h.is_effective(&f.genesis_hash), "the genesis is effective");
    assert_eq!(h.classification(), AccountClassification::Live);
}

#[test]
fn auth_len_ahead_parks_but_a_behind_assertion_never_grants_or_denies_authority() {
    let founder = Dev::new(1);
    let ahead_device = Dev::new(2);
    let behind_device = Dev::new(3);
    let mut f = Fixture::genesis(&founder);
    let ahead = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_add(&ahead_device, DeviceRole::Member),
        10,
    );
    let behind = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_add(&behind_device, DeviceRole::Member),
        0,
    );

    let h = f.fold();
    assert_eq!(h.outcome(&ahead.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert_eq!(
        h.roster_ref_effective(RosterRef::from_bytes(ahead), ahead_device.fp),
        AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
        "an ahead candidate must not leave a projected roster mutation",
    );
    assert!(h.is_effective(&behind.into()), "a stale count is informational, never authority");
}

#[test]
fn a_candidate_cannot_satisfy_its_own_auth_len() {
    let (founder, device) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&founder);
    let ahead = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_add(&device, DeviceRole::Member),
        2,
    );

    let h = f.fold();
    assert_eq!(h.effective_count(), 1, "only genesis preceded the candidate");
    assert_eq!(h.outcome(&ahead.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
}

#[test]
fn an_auth_len_ahead_cut_has_no_register_side_effect() {
    let (founder, b, member) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let add_b = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let ahead_remove = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_remove(&b, Cut::Empty),
        u64::MAX,
    );
    let add_member =
        f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&member, DeviceRole::Member));

    let h = f.fold();
    assert_eq!(h.outcome(&ahead_remove.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)),);
    assert!(h.is_effective(&add_member.into()), "a parked cut must not condemn B's chain");
    assert_eq!(h.classification(), AccountClassification::Live);
}

#[test]
fn an_ahead_ineffective_cut_cannot_poison_a_later_enrollment() {
    let (founder, device, member) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let ahead_remove = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_remove(&device, Cut::Empty),
        u64::MAX,
    );
    let add_device =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&device, DeviceRole::Owner));
    let add_member = f.author(
        &device,
        Some(OwnerId::from_bytes(add_device)),
        &device_add(&member, DeviceRole::Member),
    );

    let h = f.fold();
    assert_eq!(
        h.outcome(&ahead_remove.into()),
        Some(Outcome::Parked(ParkReason::AuthLenAhead)),
        "freshness dominates the phase-E ineffective verdict for a register contributor",
    );
    assert!(h.is_effective(&add_device.into()));
    assert!(
        h.is_effective(&add_member.into()),
        "the rejected ahead cut must leave no empty register on the enrolled device",
    );
}

#[test]
fn an_auth_len_ahead_owner_mint_cannot_authorize_a_descendant_cut() {
    let (founder, b) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&founder);
    let ahead_add = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_add(&b, DeviceRole::Owner),
        u64::MAX,
    );
    let remove_founder =
        f.author(&b, Some(OwnerId::from_bytes(ahead_add)), &device_remove(&founder, Cut::Empty));

    let h = f.fold();
    assert_eq!(h.outcome(&ahead_add.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert_eq!(
        h.outcome(&remove_founder.into()),
        Some(Outcome::Rejected(RejectReason::StaleAuthority)),
        "an excluded mint cannot launder authority into its descendant",
    );
    assert_eq!(h.classification(), AccountClassification::Live);
    assert!(h.is_effective(&f.genesis_hash));
}

#[test]
fn an_auth_len_ahead_effect_cannot_poison_a_valid_successor_or_its_dependent() {
    let (founder, device) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&founder);
    let ahead = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_add(&device, DeviceRole::Member),
        u64::MAX,
    );
    let valid =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&device, DeviceRole::Member));
    let promote = f.author(&founder, Some(f.genesis_hash.into()), &owner_promote(&device));

    let h = f.fold();
    assert_eq!(h.outcome(&ahead.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert!(h.is_effective(&valid.into()), "the parked add must not cause DuplicateAdd");
    assert!(
        h.is_effective(&promote.into()),
        "the promotion must recover with the valid enrollment on the next readiness pass",
    );
    assert_eq!(
        h.roster_ref_effective(RosterRef::from_bytes(valid), device.fp),
        AuthorityQuery::Effective(RosterAuthority {
            device_fingerprint: device.fp,
            current_role: DeviceRole::Owner,
        }),
    );
    assert_eq!(
        h.owner_incarnation_effective(OwnerId::from_bytes(promote), device.fp),
        AuthorityQuery::Effective(OwnerAuthority { device_fingerprint: device.fp }),
    );
}

#[test]
fn a_dependent_grant_recovers_after_an_ahead_ownership_competitor_is_excluded() {
    let founder = Dev::new(1);
    let grantee = AccountId::from_bytes([0x44; 32]);
    let mut f = Fixture::genesis(&founder);
    let (stream, own) = test_support::stream_own_public(f.account_id);
    let ahead_own = f.author_at_auth_len(&founder, Some(f.genesis_hash.into()), &own, u64::MAX);
    let valid_own = f.author(&founder, Some(f.genesis_hash.into()), &own);
    let grant = f.author(&founder, Some(f.genesis_hash.into()), &stream_grant(stream, grantee));

    let h = f.fold();
    assert_eq!(h.outcome(&ahead_own.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert!(h.is_effective(&valid_own.into()), "the valid StreamOwn must replace its ahead twin");
    assert!(
        h.is_effective(&grant.into()),
        "a state-dependent grant must be recomputed, not permanently readiness-excluded",
    );
}

#[test]
fn mutually_condemning_ahead_cuts_do_not_manufacture_contested() {
    let (founder, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let add_a = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let remove_b = f.author_at_auth_len(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove(&b, Cut::Empty),
        u64::MAX,
    );
    let remove_a = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove(&a, Cut::Empty),
        u64::MAX,
    );

    let h = f.fold();
    assert_eq!(h.classification(), AccountClassification::Live);
    assert_eq!(h.outcome(&remove_a.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert_eq!(h.outcome(&remove_b.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert!(h.is_effective(&add_a.into()) && h.is_effective(&add_b.into()));
}

/// The founder adds owner O, and O authors `k` member adds. Every op cites the count its author
/// had folded, so this is the history an honest founder has seen when it revokes O. Returns
/// `(fixture, founder, O, O's mint, O's adds)`.
fn owner_with_history(k: u8) -> (Fixture, Dev, Dev, [u8; 32], Vec<[u8; 32]>) {
    let (founder, o) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&founder);
    let add_o = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&o, DeviceRole::Owner));
    let adds = (0..k)
        .map(|i| {
            let member = device_add(&Dev::new(10 + i), DeviceRole::Member);
            f.author_at_auth_len(&o, Some(OwnerId::from_bytes(add_o)), &member, 2 + u64::from(i))
        })
        .collect();
    (f, founder, o, add_o, adds)
}

/// #1294: the founder's cut cites the count that includes O's three ops. Applying it condemns
/// them, and it must be measured with them credited back or it parks behind the ops it revokes.
/// O's third op cites 4, past the post-cut count of 3, so a later readiness pass holds it out
/// as ahead; its credit must survive that.
#[test]
fn a_cut_condemning_ops_its_author_folded_takes_effect() {
    let (mut f, founder, o, _, adds) = owner_with_history(3);
    let cut = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_remove(&o, Cut::Empty),
        5,
    );

    for rot in 0..f.entries.len() {
        let h = f.fold_rotated(rot);
        assert!(
            h.is_effective(&cut.into()),
            "rotation {rot}: the cut parked behind the ops it revokes"
        );
        for add in &adds {
            assert_eq!(
                h.outcome(&(*(add)).into()),
                Some(Outcome::Condemned(CondemnedReason::BeyondCut))
            );
        }
        assert_eq!(h.effective_count(), 3, "genesis, the add of O, and the cut");
    }
}

#[test]
fn a_cut_one_past_its_honest_count_still_parks() {
    let (mut f, founder, o, _, adds) = owner_with_history(2);
    let cut = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_remove(&o, Cut::Empty),
        5,
    );

    let h = f.fold();
    assert_eq!(h.outcome(&cut.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert!(
        adds.iter().all(|add| h.is_effective(&(*(add)).into())),
        "a parked cut condemns nothing"
    );
}

#[test]
fn a_cut_condemning_a_removed_owners_dependents_takes_effect() {
    let (founder, o, e, member) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&founder);
    let g = f.genesis_hash;
    let add_o = f.author(&founder, Some(g.into()), &device_add(&o, DeviceRole::Owner));
    let add_e = f.author_at_auth_len(
        &o,
        Some(OwnerId::from_bytes(add_o)),
        &device_add(&e, DeviceRole::Owner),
        2,
    );
    let add_member = f.author_at_auth_len(
        &e,
        Some(OwnerId::from_bytes(add_e)),
        &device_add(&member, DeviceRole::Member),
        3,
    );
    let cut = f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o, Cut::Empty), 4);

    let h = f.fold();
    assert!(h.is_effective(&cut.into()), "E's add, left without authority, is credited to the cut");
    assert_eq!(h.outcome(&add_e.into()), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));
    assert_eq!(
        h.outcome(&add_member.into()),
        Some(Outcome::Rejected(RejectReason::StaleAuthority))
    );
}

/// V1 compatibility: a direct victim-chain head does not bound the transitive credit.
/// O's chain stays unchanged while a device O minted appends an entry after the cut.
#[test]
fn v1_descendant_entries_can_credit_a_cut_without_advancing_the_victim_chain() {
    for demote in [false, true] {
        let (founder, owner, descendant, member) =
            (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
        let mut f = Fixture::genesis(&founder);
        let genesis = f.genesis_hash;
        let add_owner =
            f.author(&founder, Some(genesis.into()), &device_add(&owner, DeviceRole::Owner));
        let add_descendant = f.author_at_auth_len(
            &owner,
            Some(add_owner.into()),
            &device_add(&descendant, DeviceRole::Owner),
            2,
        );
        let victim_head = f.chains[&owner.fp.to_bytes()];
        let revocation = if demote {
            owner_demote(&owner, add_owner.into(), Cut::Empty)
        } else {
            device_remove(&owner, Cut::Empty)
        };
        let cut = f.author_at_auth_len(&founder, Some(genesis.into()), &revocation, 4);
        assert_eq!(f.fold().outcome(&cut.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));

        let later = f.author_at_auth_len(
            &descendant,
            Some(add_descendant.into()),
            &device_add(&member, DeviceRole::Member),
            3,
        );
        assert_eq!(f.chains[&owner.fp.to_bytes()], victim_head);
        for rotation in 0..f.entries.len() {
            let history = f.fold_rotated(rotation);
            assert!(history.is_effective(&cut.into()));
            assert_eq!(
                history.outcome(&later.into()),
                Some(Outcome::Rejected(RejectReason::StaleAuthority))
            );
        }
    }
}

/// The observable consequence of the v1 gap: descendant traffic supplies enough credit to
/// activate an ahead cut and its concurrent-op vouch, without moving the direct victim head.
#[test]
fn v1_descendant_credit_can_activate_an_ahead_cuts_concurrent_vouch() {
    for demote in [false, true] {
        let (founder, owner, descendant, concurrent_owner) =
            (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
        let mut f = Fixture::genesis(&founder);
        let g = f.genesis_hash;
        let add_owner = f.author(&founder, Some(g.into()), &device_add(&owner, DeviceRole::Owner));
        let add_concurrent_owner = f.author_at_auth_len(
            &founder,
            Some(g.into()),
            &device_add(&concurrent_owner, DeviceRole::Owner),
            2,
        );
        let add_descendant = f.author_at_auth_len(
            &owner,
            Some(add_owner.into()),
            &device_add(&descendant, DeviceRole::Owner),
            3,
        );
        assert_eq!(f.fold().effective_count(), 4);
        let victim_head = f.chains[&owner.fp.to_bytes()];
        let revocation = if demote {
            owner_demote(&owner, add_owner.into(), Cut::Empty)
        } else {
            device_remove(&owner, Cut::Empty)
        };
        let cut = f.author_at_auth_len(&founder, Some(g.into()), &revocation, 5);
        assert_eq!(f.fold().outcome(&cut.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
        let later = f.author_at_auth_len(
            &descendant,
            Some(add_descendant.into()),
            &device_add(&Dev::new(6), DeviceRole::Member),
            4,
        );
        assert_eq!(f.chains[&owner.fp.to_bytes()], victim_head);
        let after = f.fold();
        assert!(after.is_effective(&cut.into()));
        assert_eq!(
            after.outcome(&later.into()),
            Some(Outcome::Rejected(RejectReason::StaleAuthority))
        );
        let concurrent = f.author_at_auth_len(
            &concurrent_owner,
            Some(add_concurrent_owner.into()),
            &device_add(&Dev::new(5), DeviceRole::Member),
            5,
        );
        for rotation in 0..f.entries.len() {
            let history = f.fold_rotated(rotation);
            assert!(history.is_effective(&cut.into()), "demote={demote}, rotation={rotation}");
            assert!(
                history.is_effective(&concurrent.into()),
                "demote={demote}, rotation={rotation}"
            );
            assert_eq!(
                history.outcome(&later.into()),
                Some(Outcome::Rejected(RejectReason::StaleAuthority))
            );
        }
    }
}

/// A revoked device can pile up entries past the cut, all condemned, but they carry no other
/// op past the largest citation an effective cut made. Here the pre-cut view is 5 (genesis, the
/// adds of O and Q, O's two adds): an op cited at 5 is concurrent with the cut and takes
/// effect, one cited at 6 — a plain add, or a cut of a different device — parks whatever O
/// appends.
#[test]
fn a_revoked_devices_condemned_entries_credit_no_op_past_the_cuts_citation() {
    for remove_q in [false, true] {
        let (mut f, founder, o, add_o, _) = owner_with_history(2);
        let g = f.genesis_hash;
        let (q, b) = (Dev::new(20), Dev::new(22));
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&q, DeviceRole::Member), 4);
        let add_b =
            f.author_at_auth_len(&founder, Some(g.into()), &device_add(&b, DeviceRole::Owner), 5);
        let cut = f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o, Cut::Empty), 6);
        for i in 0..10 {
            let junk = device_add(&Dev::new(30 + i), DeviceRole::Member);
            f.author_at_auth_len(&o, Some(OwnerId::from_bytes(add_o)), &junk, 6);
        }
        let other = |n: u8| {
            if remove_q {
                device_remove(&q, Cut::Empty)
            } else {
                device_add(&Dev::new(40 + n), DeviceRole::Member)
            }
        };
        // B folded the pre-cut view (6, B's own add included) and never saw the cut. The op one
        // past it lives in its own fixture, so a second removal of Q is not merely redundant.
        let mut g2 = f.clone();
        let ahead = g2.author_at_auth_len(&b, Some(OwnerId::from_bytes(add_b)), &other(1), 7);
        let concurrent = f.author_at_auth_len(&b, Some(OwnerId::from_bytes(add_b)), &other(0), 6);

        let h = f.fold();
        assert!(h.is_effective(&cut.into()), "remove_q={remove_q}");
        assert!(h.is_effective(&concurrent.into()), "remove_q={remove_q}: concurrent with the cut");
        let h2 = g2.fold();
        assert_eq!(
            h2.outcome(&ahead.into()),
            Some(Outcome::Parked(ParkReason::AuthLenAhead)),
            "remove_q={remove_q}: O's condemned entries carried an op past the cut's citation",
        );
    }
}

/// #1301: B and the founder both folded 5 ops. The founder cuts O citing 5; B concurrently adds
/// a device citing 5. After the cut the count is 5 with B's add, so B's add is one past the
/// fold without it — and one is exactly what the cut's citation vouches for.
/// Order-independent.
#[test]
fn an_op_concurrent_with_a_revoking_cut_takes_effect() {
    let (founder, b, o) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let g = f.genesis_hash;
    let add_b = f.author(&founder, Some(g.into()), &device_add(&b, DeviceRole::Owner));
    let add_o =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o, DeviceRole::Owner), 2);
    for i in 0..2 {
        let member = device_add(&Dev::new(10 + i), DeviceRole::Member);
        f.author_at_auth_len(&o, Some(OwnerId::from_bytes(add_o)), &member, 3 + u64::from(i));
    }
    let cut = f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o, Cut::Empty), 5);
    let concurrent = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&Dev::new(20), DeviceRole::Member),
        5,
    );
    // B keeps authoring before it sees the cut: each op counts the one before it. The cuts'
    // citations alone cap at 5; B's own landed ops raise what B could honestly cite.
    let second = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&Dev::new(21), DeviceRole::Member),
        6,
    );
    let third = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&Dev::new(22), DeviceRole::Member),
        7,
    );

    for rot in 0..f.entries.len() {
        let h = f.fold_rotated(rot);
        assert!(h.is_effective(&cut.into()), "rotation {rot}");
        for (name, op) in [("first", &concurrent), ("second", &second), ("third", &third)] {
            assert!(
                h.is_effective(&(*(op)).into()),
                "rotation {rot}: B's {name} op parked behind the cut"
            );
        }
        assert_eq!(h.effective_count(), 7, "rotation {rot}");
    }
}

/// The same with an `OwnerDemote` as the vouching cut.
#[test]
fn an_op_concurrent_with_a_demoting_cut_takes_effect() {
    let (founder, b, o) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let g = f.genesis_hash;
    let add_b = f.author(&founder, Some(g.into()), &device_add(&b, DeviceRole::Owner));
    let add_o =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o, DeviceRole::Owner), 2);
    for i in 0..2 {
        let member = device_add(&Dev::new(10 + i), DeviceRole::Member);
        f.author_at_auth_len(&o, Some(OwnerId::from_bytes(add_o)), &member, 3 + u64::from(i));
    }
    let demote = f.author_at_auth_len(
        &founder,
        Some(g.into()),
        &owner_demote(&o, OwnerId::from_bytes(add_o), Cut::Empty),
        5,
    );
    let concurrent = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&Dev::new(20), DeviceRole::Member),
        5,
    );

    let h = f.fold();
    assert!(h.is_effective(&demote.into()));
    assert!(h.is_effective(&concurrent.into()));
}

/// Two owners removing each other, both citing the view a concurrent revocation of O has since
/// cut down, are a genuine standoff. Measured without the vouch they read as ahead, are held
/// out, and the account folds `Live` around them; with it they stay contested.
#[test]
fn a_mutual_removal_concurrent_with_a_revocation_stays_contested() {
    let (founder, b, d, o) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&founder);
    let g = f.genesis_hash;
    let add_b = f.author(&founder, Some(g.into()), &device_add(&b, DeviceRole::Owner));
    let add_d =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&d, DeviceRole::Owner), 2);
    let add_o =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o, DeviceRole::Owner), 3);
    for i in 0..2 {
        let member = device_add(&Dev::new(10 + i), DeviceRole::Member);
        f.author_at_auth_len(&o, Some(OwnerId::from_bytes(add_o)), &member, 4 + u64::from(i));
    }
    let cut = f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o, Cut::Empty), 6);
    let remove_d = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove(&d, Cut::Empty),
        6,
    );
    let remove_b = f.author_at_auth_len(
        &d,
        Some(OwnerId::from_bytes(add_d)),
        &device_remove(&b, Cut::Empty),
        6,
    );

    for rot in 0..f.entries.len() {
        let h = f.fold_rotated(rot);
        assert!(h.is_effective(&cut.into()), "rotation {rot}");
        assert!(
            matches!(h.classification(), AccountClassification::Contested { .. }),
            "rotation {rot}: the standoff was held out as ahead instead of contesting",
        );
        for op in [&remove_d, &remove_b] {
            assert_eq!(
                h.outcome(&(*(op)).into()),
                Some(Outcome::Parked(ParkReason::ContestedSubject)),
                "rotation {rot}"
            );
        }
    }
}

/// A cut whose authority is parked falls in the closure; which round it falls in must not
/// decide anything else. Here B's cut of O1 cites high on an enrolment cited at `u64::MAX`,
/// and the founder's add counts its own ops since the founder's low-citing cut of O2. With the
/// doomed cut in the table the add's ceiling is measured from the high citation and it reads
/// ahead; without it, from the low one, and it clears. Every rotation must agree.
#[test]
fn a_cut_that_falls_for_its_authority_never_moves_another_ops_verdict() {
    let (founder, b, o1, o2) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&founder);
    let g = f.genesis_hash;
    let add_b = f.author_at_auth_len(
        &founder,
        Some(g.into()),
        &device_add(&b, DeviceRole::Owner),
        u64::MAX,
    );
    f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o1, DeviceRole::Owner), 1);
    let add_o2 =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o2, DeviceRole::Owner), 2);
    for i in 0..2 {
        let member = device_add(&Dev::new(30 + i), DeviceRole::Member);
        f.author_at_auth_len(&o2, Some(OwnerId::from_bytes(add_o2)), &member, 3 + u64::from(i));
    }
    for i in 0..8 {
        let member = device_add(&Dev::new(10 + i), DeviceRole::Member);
        f.author_at_auth_len(&founder, Some(g.into()), &member, 6);
    }
    let low = f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o2, Cut::Empty), 6);
    let high = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove(&o1, Cut::Empty),
        13,
    );
    let mut g2 = f.clone();
    let x = f.author_at_auth_len(
        &founder,
        Some(g.into()),
        &device_add(&Dev::new(50), DeviceRole::Member),
        14,
    );
    let beyond = g2.author_at_auth_len(
        &founder,
        Some(g.into()),
        &device_add(&Dev::new(51), DeviceRole::Member),
        15,
    );

    for rot in 0..f.entries.len() {
        let h = f.fold_rotated(rot);
        assert!(!h.is_effective(&high.into()), "rotation {rot}: its enrolment is parked");
        assert!(h.is_effective(&low.into()), "rotation {rot}");
        assert!(h.is_effective(&x.into()), "rotation {rot}: parked by a cut that fell");
        assert_eq!(h.effective_count(), 13, "rotation {rot}");
    }
    for rot in 0..g2.entries.len() {
        let h = g2.fold_rotated(rot);
        assert_eq!(
            h.outcome(&beyond.into()),
            Some(Outcome::Parked(ParkReason::AuthLenAhead)),
            "rotation {rot}"
        );
    }
}

/// The bound: two cuts of two devices, each citing the pre-cut view plus BOTH victim counts,
/// clear each other (the credit term is the victims' to fill) and vouch for that much.
/// Owner-signed on both sides; recorded, not defended.
#[test]
fn two_cuts_citing_past_the_view_clear_each_other_up_to_the_condemned_count() {
    let (founder, b, c, o1, o2) = (Dev::new(1), Dev::new(2), Dev::new(5), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&founder);
    let g = f.genesis_hash;
    let add_b = f.author(&founder, Some(g.into()), &device_add(&b, DeviceRole::Owner));
    let add_c =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&c, DeviceRole::Owner), 2);
    let add_o1 =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o1, DeviceRole::Owner), 3);
    let add_o2 =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o2, DeviceRole::Owner), 4);
    for i in 0..2 {
        let member = device_add(&Dev::new(10 + i), DeviceRole::Member);
        f.author_at_auth_len(&o1, Some(OwnerId::from_bytes(add_o1)), &member, 5 + u64::from(i));
    }
    for i in 0..2 {
        let member = device_add(&Dev::new(12 + i), DeviceRole::Member);
        f.author_at_auth_len(&o2, Some(OwnerId::from_bytes(add_o2)), &member, 7 + u64::from(i));
    }
    // The pre-cut view is 9; the fold with both cuts is 7. Each cut cites 6 + 2 + 2: the
    // fold without it plus its own credit plus the other's. C, with nothing landed since the
    // view, is vouched for up to that citation and no further.
    let cut1 = f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o1, Cut::Empty), 10);
    let cut2 = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove(&o2, Cut::Empty),
        10,
    );
    let mut g2 = f.clone();
    let past = f.author_at_auth_len(
        &c,
        Some(OwnerId::from_bytes(add_c)),
        &device_add(&Dev::new(20), DeviceRole::Member),
        10,
    );
    let beyond = g2.author_at_auth_len(
        &c,
        Some(OwnerId::from_bytes(add_c)),
        &device_add(&Dev::new(21), DeviceRole::Member),
        11,
    );

    let h = f.fold();
    assert!(h.is_effective(&cut1.into()) && h.is_effective(&cut2.into()));
    assert!(h.is_effective(&past.into()), "vouched up to the condemned count");
    assert_eq!(g2.fold().outcome(&beyond.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
}

/// Two owners revoke two DIFFERENT devices over one history of 8 ops, each citing 8. Each cut's
/// author counted the other's victims, which its own credit does not cover; the other cut
/// vouches for them. Both take effect, every victim is condemned, and an op cited one past the
/// shared view still parks — whatever a revoked device appends.
#[test]
fn concurrent_cuts_of_two_devices_both_take_effect() {
    for junk in [0u8, 3] {
        let (founder, b, o1, o2) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
        let mut f = Fixture::genesis(&founder);
        let g = f.genesis_hash;
        let add_b = f.author(&founder, Some(g.into()), &device_add(&b, DeviceRole::Owner));
        let add_o1 =
            f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o1, DeviceRole::Owner), 2);
        let add_o2 =
            f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o2, DeviceRole::Owner), 3);
        let mut victims = Vec::new();
        for i in 0..2 {
            let member = device_add(&Dev::new(10 + i), DeviceRole::Member);
            victims.push(f.author_at_auth_len(
                &o1,
                Some(OwnerId::from_bytes(add_o1)),
                &member,
                4 + u64::from(i),
            ));
        }
        for i in 0..2 {
            let member = device_add(&Dev::new(12 + i), DeviceRole::Member);
            victims.push(f.author_at_auth_len(
                &o2,
                Some(OwnerId::from_bytes(add_o2)),
                &member,
                6 + u64::from(i),
            ));
        }
        let cut1 =
            f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o1, Cut::Empty), 8);
        let cut2 = f.author_at_auth_len(
            &b,
            Some(OwnerId::from_bytes(add_b)),
            &device_remove(&o2, Cut::Empty),
            8,
        );
        for i in 0..junk {
            let entry = device_add(&Dev::new(30 + i), DeviceRole::Member);
            f.author_at_auth_len(&o1, Some(OwnerId::from_bytes(add_o1)), &entry, 8);
        }
        let concurrent = f.author_at_auth_len(
            &b,
            Some(OwnerId::from_bytes(add_b)),
            &device_add(&Dev::new(20), DeviceRole::Member),
            8,
        );
        // B's own cut and add landed since the view, so B may cite two past it, not three.
        let mut g2 = f.clone();
        let next = g2.author_at_auth_len(
            &b,
            Some(OwnerId::from_bytes(add_b)),
            &device_add(&Dev::new(21), DeviceRole::Member),
            10,
        );
        let ahead = g2.author_at_auth_len(
            &b,
            Some(OwnerId::from_bytes(add_b)),
            &device_add(&Dev::new(22), DeviceRole::Member),
            12,
        );

        for rot in 0..f.entries.len() {
            let h = f.fold_rotated(rot);
            assert!(
                h.is_effective(&cut1.into()) && h.is_effective(&cut2.into()),
                "junk={junk} rotation {rot}"
            );
            assert!(h.is_effective(&concurrent.into()), "junk={junk} rotation {rot}");
            for victim in &victims {
                assert_eq!(
                    h.outcome(&(*(victim)).into()),
                    Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
                    "junk={junk} rotation {rot}"
                );
            }
        }
        let h2 = g2.fold();
        assert!(h2.is_effective(&next.into()), "junk={junk}: B's own landed ops raise its ceiling");
        assert_eq!(
            h2.outcome(&ahead.into()),
            Some(Outcome::Parked(ParkReason::AuthLenAhead)),
            "junk={junk}: admitted past the largest citation any effective cut made",
        );
    }
}

/// A cut that parks as ahead vouches for nothing: an op concurrent with it parks too.
#[test]
fn a_parked_cut_vouches_for_nothing() {
    let (mut f, founder, o, _, _) = owner_with_history(2);
    let g = f.genesis_hash;
    let b = Dev::new(22);
    let add_b =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&b, DeviceRole::Owner), 4);
    // Past the honest count of 5 (genesis, the adds of O and B, O's two adds) AND past the two
    // condemned ops the cut's own credit covers: 7 parks it.
    let cut = f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o, Cut::Empty), 7);
    let concurrent = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&Dev::new(20), DeviceRole::Member),
        7,
    );

    let h = f.fold();
    assert_eq!(h.outcome(&cut.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert_eq!(h.outcome(&concurrent.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
}

/// An unenrolled device signing entries that cite O's incarnation folds `WrongDevice`; they are
/// not on O's chain, so they cannot move the credit of a cut that revokes O.
#[test]
fn a_strangers_entries_cannot_inflate_a_cuts_credit() {
    let (mut f, founder, o, add_o, adds) = owner_with_history(1);
    let stranger = Dev::new(40);
    for i in 0..10 {
        let junk = device_add(&Dev::new(50 + i), DeviceRole::Member);
        f.author_at_auth_len(&stranger, Some(OwnerId::from_bytes(add_o)), &junk, 3);
    }
    // One past the honest count of 3: genesis, the add of O, and O's one add.
    let cut = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &device_remove(&o, Cut::Empty),
        4,
    );

    let h = f.fold();
    assert_eq!(h.outcome(&cut.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert!(h.is_effective(&adds[0].into()));
}

/// Two owners revoking O over the same history both cite the pre-cut count. The cuts share O's
/// register key, so each is credited O's condemned ops: one applies, the other is a redundant
/// remove, and neither parks as ahead.
#[test]
fn concurrent_revocations_of_one_device_both_clear_freshness() {
    let (founder, b, o) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let g = f.genesis_hash;
    let add_b = f.author(&founder, Some(g.into()), &device_add(&b, DeviceRole::Owner));
    let add_o =
        f.author_at_auth_len(&founder, Some(g.into()), &device_add(&o, DeviceRole::Owner), 2);
    for i in 0..2 {
        let member = device_add(&Dev::new(10 + i), DeviceRole::Member);
        f.author_at_auth_len(&o, Some(OwnerId::from_bytes(add_o)), &member, 3 + u64::from(i));
    }
    let by_founder =
        f.author_at_auth_len(&founder, Some(g.into()), &device_remove(&o, Cut::Empty), 5);
    let by_b = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove(&o, Cut::Empty),
        5,
    );

    for rot in 0..f.entries.len() {
        let h = f.fold_rotated(rot);
        assert!(h.is_effective(&by_founder.into()), "rotation {rot}");
        assert_eq!(
            h.outcome(&by_b.into()),
            Some(Outcome::Rejected(RejectReason::Ineffective)),
            "rotation {rot}: the redundant cut must not park as ahead",
        );
    }
}

#[test]
fn an_owner_demote_condemning_folded_ops_takes_effect() {
    let (mut f, founder, o, add_o, adds) = owner_with_history(2);
    let demote = f.author_at_auth_len(
        &founder,
        Some(f.genesis_hash.into()),
        &owner_demote(&o, OwnerId::from_bytes(add_o), Cut::Empty),
        4,
    );

    let h = f.fold();
    assert!(h.is_effective(&demote.into()));
    for add in &adds {
        assert_eq!(
            h.outcome(&(*(add)).into()),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut))
        );
    }
}

/// A self-removal condemns its own entry. Counting that toward its credit would let an ahead
/// self-cut pay for its own freshness and install its register.
#[test]
fn a_self_removal_cannot_credit_its_own_entry() {
    let (founder, o) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&founder);
    let add_o = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&o, DeviceRole::Owner));
    // One past the honest count of 2: genesis and the add of O.
    let cut = f.author_at_auth_len(
        &o,
        Some(OwnerId::from_bytes(add_o)),
        &device_remove(&o, Cut::Empty),
        3,
    );

    let h = f.fold();
    assert_eq!(h.outcome(&cut.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
}

/// B's removal of the founder condemns the founder's add of B, B's own mint, so the cut itself
/// goes stale. Only the add is B's folded history; the stale cut must not count as its own
/// dependent.
#[test]
fn a_cut_that_strands_its_own_authority_cannot_credit_itself() {
    let (founder, b) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&founder);
    let add_b = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    // One past the honest count of 2: genesis and the add of B.
    let cut = f.author_at_auth_len(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove(&founder, Cut::Empty),
        3,
    );

    let h = f.fold();
    assert_eq!(h.outcome(&cut.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert!(h.is_effective(&add_b.into()), "an excluded cut condemns nothing");
}

#[test]
fn depth_ordering_admits_an_op_by_a_device_the_founder_added() {
    // genesis (founder, depth-0 owner) -> founder adds B as owner (depth 0) -> B adds C (depth
    // 1). C's add is effective ONLY because B's incarnation was made live at the
    // shallower depth.
    let (founder, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let add_b = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let add_c = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&c, DeviceRole::Member));
    let h = f.fold();
    assert!(h.is_effective(&f.genesis_hash));
    assert!(h.is_effective(&add_b.into()), "founder's DeviceAdd(B, owner) is effective");
    assert!(h.is_effective(&add_c.into()), "B's DeviceAdd(C) at depth 1 is effective");
}

#[test]
fn arrival_order_does_not_change_the_result_p9() {
    let (founder, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let add_b = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&c, DeviceRole::Member));
    let baseline = Fixture::effective_set(&f.fold());
    for rot in 0..f.entries.len() {
        assert_eq!(
            Fixture::effective_set(&f.fold_rotated(rot)),
            baseline,
            "rotation {rot} changed the effective set",
        );
    }
}

#[test]
fn cross_account_citation_is_not_admitted_p3() {
    // An op in this account citing an owner_id that is NOT a mint here (an owner incarnation
    // from another account) is unresolvable -> parked, never effective. The (account,
    // owner_id) recursion never leaves this account.
    let (founder, d, e) = (Dev::new(1), Dev::new(9), Dev::new(10));
    let mut f = Fixture::genesis(&founder);
    let foreign_incarnation = [0x77u8; 32];
    let op = f.author(
        &d,
        Some(OwnerId::from_bytes(foreign_incarnation)),
        &device_add(&e, DeviceRole::Member),
    );
    let h = f.fold();
    assert!(!h.is_effective(&op.into()), "cross-account citation is not admitted (P3)");
    assert_eq!(h.outcome(&op.into()), Some(Outcome::Parked(ParkReason::UnknownOwnerRef)));
}

#[test]
fn duplicate_device_add_is_rejected_p11() {
    let (founder, b) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&founder);
    let add_b1 =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let add_b2 =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let h = f.fold();
    assert!(h.is_effective(&add_b1.into()), "the first DeviceAdd(B) is effective");
    assert_eq!(
        h.outcome(&add_b2.into()),
        Some(Outcome::Rejected(RejectReason::DuplicateAdd)),
        "the duplicate DeviceAdd(B) is ineffective",
    );
}

#[test]
fn revocation_is_sound_beyond_within_and_off_branch_p6() {
    // Founder A adds owner B; B authors a chain b0 <- b1 (<- b2). A then removes B with a cut
    // pinned to b1. Soundness (§11 valid-prefix, L2):
    //   * b0, b1 (within the cut, on the accepted branch) stay EFFECTIVE — a removal bounds the
    //     valid prefix, it does not erase legitimate history.
    //   * b2 (seq beyond the watermark) is Condemned{BeyondCut} — a back-dated forgery.
    //   * a forged sibling of b1 (same seq, different branch) is Condemned{OffBranch} — the
    //     equivocation loser, caught by ancestry even though its seq is within the cut.
    let (a, b) = (Dev::new(1), Dev::new(2));
    let (d, e, fdev, g) = (Dev::new(4), Dev::new(5), Dev::new(6), Dev::new(7));
    let mut f = Fixture::genesis(&a);
    let add_b = f.author(&a, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let b0 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&d, DeviceRole::Member));
    let b1 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&e, DeviceRole::Member));
    let b2 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&fdev, DeviceRole::Member));
    // A forged sibling of b1: seq 1 (within the cut) but forking off b0 — off the branch b1 is
    // on.
    let forged = f.author_forked(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_add(&g, DeviceRole::Member),
        1,
        Some(AccountEntryHash::from_bytes(b0)),
    );
    // A removes B, valid prefix pinned to b1 on B's control chain.
    let remove_b = f.author(
        &a,
        Some(f.genesis_hash.into()),
        &device_remove(&b, Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(b1) }),
    );

    let h = f.fold();
    assert!(h.is_effective(&remove_b.into()), "the removal itself is effective");
    assert!(h.is_effective(&b0.into()), "b0 is within the cut and on-branch — effective");
    assert!(h.is_effective(&b1.into()), "b1 (the watermark slot) is within the cut — effective");
    assert_eq!(
        h.outcome(&b2.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        "b2 is beyond the cut — a back-dated forgery",
    );
    assert_eq!(
        h.outcome(&forged.into()),
        Some(Outcome::Condemned(CondemnedReason::OffBranch)),
        "the forged sibling of b1 is off the accepted branch",
    );
    // Order-independence holds with revocation in play (I9).
    let baseline = Fixture::effective_set(&h);
    for rot in 0..f.entries.len() {
        assert_eq!(Fixture::effective_set(&f.fold_rotated(rot)), baseline, "rotation {rot}");
    }
}

#[test]
fn a_demoted_owner_cannot_launder_authority_p1() {
    // A (founder) demotes owner B (owner_id = add_b) with an EMPTY control cut — nothing under
    // B's incarnation is valid henceforth. B, beyond that cut, tries to mint a new owner C
    // (DeviceAdd C as owner). C then tries to remove A. No-laundering (P1):
    //   * A's demotion of B is effective.
    //   * B's post-cut DeviceAdd(C) is Condemned{BeyondCut} (scoped by the owner-incarnation
    //     register, beyond an empty cut) — so C's incarnation never becomes live.
    //   * C's DeviceRemove(A) is StaleAuthority — it cites an incarnation that never lived.
    //   * The account is NOT contested — there is no mutual, same-depth revocation.
    let (a, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&a);
    let add_b = f.author(&a, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let demote_b = f.author(
        &a,
        Some(f.genesis_hash.into()),
        &owner_demote(&b, OwnerId::from_bytes(add_b), Cut::Empty),
    );
    let add_c = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&c, DeviceRole::Owner));
    let remove_a = f.author(&c, Some(OwnerId::from_bytes(add_c)), &device_remove(&a, Cut::Empty));

    let h = f.fold();
    assert!(h.is_effective(&f.genesis_hash));
    assert!(h.is_effective(&add_b.into()), "B was a legitimately-added owner");
    assert!(h.is_effective(&demote_b.into()), "A's demotion of B is effective");
    assert_eq!(
        h.outcome(&add_c.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        "B's laundered owner-mint is condemned by the demotion cut",
    );
    assert_eq!(
        h.outcome(&remove_a.into()),
        Some(Outcome::Rejected(RejectReason::StaleAuthority)),
        "C's removal of A cites an incarnation that never lived",
    );
    assert!(!h.is_effective(&remove_a.into()), "A is not removed — laundering defeated");
    assert_eq!(
        h.classification(),
        AccountClassification::Live,
        "no mutual same-depth cut — the account is not contested",
    );
}

#[test]
fn mutual_owner_removal_is_contested_p4() {
    // Founder F adds two owners A and B (both depth-0-authored ⇒ depth-1 incarnations). A and B
    // then remove EACH OTHER at the same depth — a same-depth mutual condemnation cycle, the
    // genuine owner-key-compromise signal (§12). The fold fails closed to state_before(1) and
    // halts: neither removal folds, and the residue cut ops park as contested subjects.
    let (fdr, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let (small, big) = (AccountId::from_bytes([0x11; 32]), AccountId::from_bytes([0x22; 32]));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let remove_b = f.author(&a, Some(OwnerId::from_bytes(add_a)), &device_remove(&b, Cut::Empty));
    let remove_a = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_remove(&a, Cut::Empty));
    // Two competing recovery re-roots by a pre-contest owner (A, live in state_before(1)).
    let reroot_big = f.author(&a, Some(OwnerId::from_bytes(add_a)), &account_reroot(big));
    let reroot_small = f.author(&a, Some(OwnerId::from_bytes(add_a)), &account_reroot(small));

    let h = f.fold();
    assert_eq!(
        h.classification(),
        AccountClassification::Contested { state_before_depth: 1 },
        "two owners cutting each other is contested at the last cycle-free depth",
    );
    assert!(h.is_effective(&f.genesis_hash), "state_before(1) keeps the depth-0 roster");
    assert!(h.is_effective(&add_a.into()), "A was a legitimate owner before the standoff");
    assert!(h.is_effective(&add_b.into()), "B was a legitimate owner before the standoff");
    assert!(!h.is_effective(&remove_a.into()), "authority mutation is halted — no removal folds");
    assert!(!h.is_effective(&remove_b.into()), "authority mutation is halted — no removal folds");
    assert_eq!(
        h.outcome(&remove_a.into()),
        Some(Outcome::Parked(ParkReason::ContestedSubject)),
        "the residue cut op parks, fail-closed",
    );
    // The sole admitted ops are the pre-contest owner's re-roots; the successor is
    // deterministic.
    assert!(
        h.is_effective(&reroot_small.into()) && h.is_effective(&reroot_big.into()),
        "re-roots admitted"
    );
    assert_eq!(
        h.contested_successor(),
        Some(small),
        "the deterministic successor is the smallest account_id by byte order",
    );
    // Order-independence holds through a contested fold (I9): same standoff, same successor.
    for rot in 0..f.entries.len() {
        let r = f.fold_rotated(rot);
        assert_eq!(
            r.classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "rotation {rot} must reach the same contested verdict",
        );
        assert_eq!(r.contested_successor(), Some(small), "rotation {rot} successor");
    }
}

#[test]
fn an_auth_len_ahead_reroot_cannot_select_the_contested_successor() {
    let (founder, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let (ahead_small, valid_big) =
        (AccountId::from_bytes([0x11; 32]), AccountId::from_bytes([0x22; 32]));
    let mut f = Fixture::genesis(&founder);
    let add_a = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    f.author(&a, Some(OwnerId::from_bytes(add_a)), &device_remove(&b, Cut::Empty));
    f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_remove(&a, Cut::Empty));
    let ahead = f.author_at_auth_len(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &account_reroot(ahead_small),
        u64::MAX,
    );
    let valid = f.author(&a, Some(OwnerId::from_bytes(add_a)), &account_reroot(valid_big));

    let h = f.fold();
    assert_eq!(h.outcome(&ahead.into()), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    assert!(h.is_effective(&valid.into()));
    assert_eq!(h.contested_successor(), Some(valid_big));
}

#[test]
fn incomparable_cuts_for_one_key_are_contested_p5() {
    // D equivocates: two entries at D's seq 0 (different content ⇒ different hashes). Two
    // owners A and B each remove D, but their control cuts name the two DIFFERENT seq-0
    // watermarks. One register key (Device{D}) with equal-seq / different-hash
    // watermarks is incomparable — the fold refuses to pick a hash and folds contested
    // (§11.3).
    let (fdr, a, b, d) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let (t8, t9) = (Dev::new(8), Dev::new(9));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let add_d = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&d, DeviceRole::Owner));
    // D's equivocation: two seq-0 entries on D's chain with distinct content.
    let d0a = f.author_forked(
        &d,
        Some(OwnerId::from_bytes(add_d)),
        &device_add(&t8, DeviceRole::Member),
        0,
        None,
    );
    let d0b = f.author_forked(
        &d,
        Some(OwnerId::from_bytes(add_d)),
        &device_add(&t9, DeviceRole::Member),
        0,
        None,
    );
    assert_ne!(d0a, d0b, "the two seq-0 entries must be distinct watermarks");
    f.author(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove(&d, Cut::At { seq: 0, hash: AccountEntryHash::from_bytes(d0a) }),
    );
    f.author(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove(&d, Cut::At { seq: 0, hash: AccountEntryHash::from_bytes(d0b) }),
    );

    let h = f.fold();
    assert_eq!(
        h.classification(),
        AccountClassification::Contested { state_before_depth: 1 },
        "one register key with equal-seq different-hash cuts is contested",
    );
    assert!(
        h.is_effective(&add_a.into())
            && h.is_effective(&add_b.into())
            && h.is_effective(&add_d.into())
    );
    // The verdict is arrival-order-free: the incomparable join is symmetric (I9).
    for rot in 0..f.entries.len() {
        assert_eq!(
            f.fold_rotated(rot).classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "rotation {rot} must reach the same contested verdict",
        );
    }
}

#[test]
fn a_withheld_watermark_parks_the_under_cut_prefix_but_beyond_still_fires_p10() {
    // Founder F removes B with a cut pinned to B's seq-1 entry (b1). I11: beyond-cut
    // condemnation fires from `[seq]` alone even while b1 is withheld, but the under-cut prefix
    // (b0, b1) can't be placed on/off the accepted branch yet, so it PARKS — never silently
    // accepted, never a flipped verdict. When b1 later syncs, the prefix heals to effective.
    let (fdr, b) = (Dev::new(1), Dev::new(2));
    let (d, e, g) = (Dev::new(5), Dev::new(6), Dev::new(7));
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let b0 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&d, DeviceRole::Member));
    let b1 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&e, DeviceRole::Member));
    let b2 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&g, DeviceRole::Member));
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove(&b, Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(b1) }),
    );

    // Withheld: fold WITHOUT b1. Beyond-cut still fires; the under-cut prefix parks.
    let withheld = f.fold_without(b1);
    assert_eq!(
        withheld.outcome(&b0.into()),
        Some(Outcome::Parked(ParkReason::UnknownCutTarget)),
        "under-cut b0 parks while the watermark is withheld",
    );
    assert_eq!(
        withheld.outcome(&b2.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        "beyond-cut b2 is condemned from seq alone (I11) even with the watermark withheld",
    );

    // Healed: the watermark synced — the prefix is on the accepted branch and re-blesses; the
    // beyond-cut verdict is unchanged (no prior verdict flipped).
    let healed = f.fold();
    assert!(healed.is_effective(&b0.into()), "b0 heals to effective once b1 is held");
    assert!(healed.is_effective(&b1.into()), "b1 (the watermark slot) is within the cut");
    assert_eq!(
        healed.outcome(&b2.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        "b2 stays condemned — healing never flips the beyond-cut verdict",
    );
}

#[test]
fn interleaved_control_set_folds_totally_and_without_oscillation_p2() {
    // A control set mixing adds, an owner-incarnation demotion (condemning beyond-cut work), a
    // device removal, and an unresolvable citation. P2: the effect pass classifies EVERY
    // candidate (totality), and the full outcome map is byte-identical under every arrival
    // permutation (no oscillation, no order dependence — I9).
    let (fdr, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let (d, g, hdev, k) = (Dev::new(4), Dev::new(5), Dev::new(6), Dev::new(7));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let a0 = f.author(&a, Some(OwnerId::from_bytes(add_a)), &device_add(&d, DeviceRole::Member));
    let b0 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&g, DeviceRole::Member));
    let b1 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&hdev, DeviceRole::Member));
    // Demote B's incarnation, valid prefix pinned to b0 ⇒ b1 (seq 1) is condemned.
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &owner_demote(&b, OwnerId::from_bytes(add_b), Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes(b0),
        }),
    );
    f.author(&a, Some(OwnerId::from_bytes(add_a)), &device_remove(&d, Cut::Empty));
    // An op citing an unresolvable incarnation parks.
    let foreign =
        f.author(&a, Some(OwnerId::from_bytes([0x77u8; 32])), &device_add(&k, DeviceRole::Member));

    let baseline = f.fold();
    // Totality: exactly one outcome per candidate, and the mixed classes are all represented.
    assert_eq!(baseline.outcomes.len(), f.entries.len(), "every candidate is classified");
    assert!(baseline.is_effective(&a0.into()) && baseline.is_effective(&add_a.into()));
    assert_eq!(baseline.outcome(&b1.into()), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));
    assert_eq!(
        baseline.outcome(&foreign.into()),
        Some(Outcome::Parked(ParkReason::UnknownOwnerRef))
    );

    // No oscillation: the FULL map (including auth_epoch numbering) is permutation-invariant.
    for rot in 0..f.entries.len() {
        let rotated = f.fold_rotated(rot);
        assert_eq!(rotated.classification(), AccountClassification::Live);
        assert_eq!(rotated.outcomes, baseline.outcomes, "rotation {rot} changed the outcome map");
    }
}

#[test]
fn cut_extend_reblesses_a_condemned_cone_p7() {
    // F removes B pinned to seq 0, condemning B's later work (b1, b2). F then extends B's
    // device cut to seq 2 (§11.4). Because condemnation is recomputed against the RAISED
    // watermark, b1/b2 re-bless on the same fold — recovery is a pure refold, never sticky gate
    // state. The extend is authored at the same incarnation depth as the removal, so the `⊔`
    // join lands before the condemnation scan.
    let (fdr, b) = (Dev::new(1), Dev::new(2));
    let (d, e, g) = (Dev::new(5), Dev::new(6), Dev::new(7));
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let b0 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&d, DeviceRole::Member));
    let b1 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&e, DeviceRole::Member));
    let b2 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&g, DeviceRole::Member));
    let remove_b = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove(&b, Cut::At { seq: 0, hash: AccountEntryHash::from_bytes(b0) }),
    );
    let extend = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &cut_extend_ctrl(f.account_id, &b, None, 2, AccountEntryHash::from_bytes(b2)),
    );

    // Without the extend, the beyond-cut cone is condemned.
    let before = f.fold_without(extend);
    assert_eq!(before.outcome(&b1.into()), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));
    assert_eq!(before.outcome(&b2.into()), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));

    // With the extend, the cone re-blesses; the removal + extend themselves are effective.
    let after = f.fold();
    assert!(after.is_effective(&b0.into()), "b0 stays within every watermark");
    assert!(after.is_effective(&b1.into()), "b1 re-blessed by the extend");
    assert!(after.is_effective(&b2.into()), "b2 re-blessed by the extend");
    assert!(after.is_effective(&remove_b.into()) && after.is_effective(&extend.into()));
}

#[test]
fn recovery_reopens_a_fresh_incarnation_and_bars_tombstone_readd_p7() {
    // Demotion bounds the OLD incarnation via its register, but a re-PROMOTE mints a FRESH
    // owner_id with no register, so the device resumes (§11). A tombstoned fingerprint, by
    // contrast, is a permanent bar (I4): re-adding it is rejected.
    let (fdr, b, dremoved) = (Dev::new(1), Dev::new(2), Dev::new(4));
    let (t5, t6) = (Dev::new(5), Dev::new(6));
    let mut f = Fixture::genesis(&fdr);
    let g1 = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    // Demote B's incarnation g1 with an empty cut — everything B does under g1 is condemned.
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &owner_demote(&b, OwnerId::from_bytes(g1), Cut::Empty),
    );
    let under_g1 =
        f.author(&b, Some(OwnerId::from_bytes(g1)), &device_add(&t5, DeviceRole::Member));
    // Re-promote B → a fresh incarnation g2 (no register); B's work under g2 accepts.
    let g2 = f.author(&fdr, Some(f.genesis_hash.into()), &owner_promote(&b));
    let under_g2 =
        f.author(&b, Some(OwnerId::from_bytes(g2)), &device_add(&t6, DeviceRole::Member));
    // A removed device's fingerprint is tombstoned; re-adding it is barred (I4).
    f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&dremoved, DeviceRole::Member));
    f.author(&fdr, Some(f.genesis_hash.into()), &device_remove(&dremoved, Cut::Empty));
    let readd =
        f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&dremoved, DeviceRole::Member));

    let h = f.fold();
    assert_eq!(
        h.outcome(&under_g1.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        "work under the demoted incarnation is condemned",
    );
    assert!(h.is_effective(&g2.into()), "the re-promotion mints a fresh incarnation");
    assert!(h.is_effective(&under_g2.into()), "B resumes under the fresh incarnation");
    assert_eq!(
        h.outcome(&readd.into()),
        Some(Outcome::Rejected(RejectReason::TombstoneReAdd)),
        "a tombstoned fingerprint can never re-enroll (I4)",
    );
}

#[test]
fn an_owner_op_citing_another_devices_incarnation_is_wrong_device() {
    // Founder F adds owner A and member M. M signs an owner op but cites A's live incarnation.
    // The authority rule (clause 1: the cited mint must name the SIGNER) rejects it — a device
    // cannot borrow another device's incarnation, even within the same account.
    let (fdr, a, m, x) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&m, DeviceRole::Member));
    let impersonation =
        f.author(&m, Some(OwnerId::from_bytes(add_a)), &device_add(&x, DeviceRole::Member));

    let h = f.fold();
    assert!(h.is_effective(&add_a.into()));
    assert_eq!(
        h.outcome(&impersonation.into()),
        Some(Outcome::Rejected(RejectReason::WrongDevice)),
        "citing another device's incarnation is not admitted",
    );
}

#[test]
fn owner_demote_naming_a_wrong_device_incarnation_is_rejected() {
    // An OwnerDemote names device A but supplies B's incarnation as `owner_id`. If admitted it
    // would drop A from the owner set while leaving A's REAL incarnation unbounded — so the
    // target binding (owner_id resolves to a mint for the demoted device) rejects it.
    let (fdr, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let bad = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &owner_demote(&a, OwnerId::from_bytes(add_b), Cut::Empty),
    );

    let h = f.fold();
    assert!(h.is_effective(&add_a.into()) && h.is_effective(&add_b.into()));
    assert_eq!(
        h.outcome(&bad.into()),
        Some(Outcome::Rejected(RejectReason::WrongDevice)),
        "an OwnerDemote whose owner_id names a different device is rejected",
    );
}

#[test]
fn a_cut_extend_without_a_creating_register_installs_nothing() {
    // A `CutExtend` is EXTEND-ONLY. With no prior DeviceRemove/OwnerDemote it must not conjure
    // a register — otherwise a live owner could condemn a chain with a bare extend. B's
    // entries stay effective (no phantom `Device{B}` register condemns the beyond-`[0]`
    // slot); the extend parks.
    let (fdr, b) = (Dev::new(1), Dev::new(2));
    let (d, e) = (Dev::new(5), Dev::new(6));
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let b0 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&d, DeviceRole::Member));
    let b1 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&e, DeviceRole::Member));
    let extend = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &cut_extend_ctrl(f.account_id, &b, None, 0, AccountEntryHash::from_bytes(b0)),
    );

    let h = f.fold();
    assert!(h.is_effective(&b0.into()), "b0 effective");
    assert!(
        h.is_effective(&b1.into()),
        "b1 (seq 1, beyond a phantom [0] watermark) is effective — the extend created no register",
    );
    assert_eq!(
        h.outcome(&extend.into()),
        Some(Outcome::Parked(ParkReason::UnknownCutTarget)),
        "a bare extend parks until a creating cut exists",
    );
}

#[test]
fn a_grant_on_a_private_stream_folds_rejected() {
    // "grants ⇒ public" is a CONSENSUS rule held by the fold itself, not just by the
    // authoring crate's check — a hand-crafted grant on a private stream must reject even
    // though the ownership is effective.
    let fdr = Dev::new(1);
    let grantee = AccountId::from_bytes([0x44; 32]);
    let mut f = Fixture::genesis(&fdr);
    let (stream, own_op) = test_support::stream_own_private(f.account_id);
    let own = f.author(&fdr, Some(f.genesis_hash.into()), &own_op);
    let grant = f.author(&fdr, Some(f.genesis_hash.into()), &stream_grant(stream, grantee));

    let h = f.fold();
    assert!(h.is_effective(&own.into()), "private ownership itself is effective");
    assert_eq!(
        h.outcome(&grant.into()),
        Some(Outcome::Rejected(RejectReason::Ineffective)),
        "a grant on a private stream never folds effective",
    );
}

#[test]
fn stream_ownership_grant_and_revoke_are_folded_as_citation_authority() {
    let fdr = Dev::new(1);
    let grantee = AccountId::from_bytes([0x44; 32]);
    let mut f = Fixture::genesis(&fdr);
    let (stream, own_op) = test_support::stream_own_public(f.account_id);
    let own = f.author(&fdr, Some(f.genesis_hash.into()), &own_op);
    let grant = f.author(&fdr, Some(f.genesis_hash.into()), &stream_grant(stream, grantee));
    let revoke = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &stream_revoke(stream, grantee, GrantId::from_bytes(grant)),
    );

    let h = f.fold();
    assert!(h.is_effective(&own.into()));
    assert!(h.is_effective(&grant.into()));
    assert!(h.is_effective(&revoke.into()));
    assert_eq!(h.effective_count(), 4);
    // The exact citation resolves against the fold we hold, and NOTHING else: revocation bounds
    // content through cuts, so no assertion the author makes about its own control length can
    // reopen, close, or deny this grant. That counter is a separate, purely informational axis.
    assert_eq!(
        h.grant_effective(GrantId::from_bytes(grant), stream, grantee),
        AuthorityQuery::Effective(GrantAuthority {
            stream_id: stream,
            grantee_account_id: grantee,
            role: GrantRole::Reader,
        }),
    );
    assert_eq!(h.auth_len_freshness(3), AuthorityFreshness::CurrentOrBehind);
    assert_eq!(h.auth_len_freshness(4), AuthorityFreshness::CurrentOrBehind);
    assert_eq!(
        h.auth_len_freshness(5),
        AuthorityFreshness::Ahead,
        "an author citing more effective ops than we folded is a refetch signal, not a verdict",
    );
    assert_eq!(h.stream_owner_effective(stream), AuthorityQuery::Effective(own.into()));
}

#[test]
fn stream_preconditions_fail_closed_without_minting_authority_p11() {
    let fdr = Dev::new(1);
    let grantee = AccountId::from_bytes([0x44; 32]);
    let mut f = Fixture::genesis(&fdr);
    let (stream, own_op) = test_support::stream_own_public(f.account_id);
    let early_grant = f.author(&fdr, Some(f.genesis_hash.into()), &stream_grant(stream, grantee));
    let own = f.author(&fdr, Some(f.genesis_hash.into()), &own_op);
    let duplicate_own = f.author(&fdr, Some(f.genesis_hash.into()), &own_op);
    let self_grant =
        f.author(&fdr, Some(f.genesis_hash.into()), &stream_grant(stream, f.account_id));
    let grant = f.author(&fdr, Some(f.genesis_hash.into()), &stream_grant(stream, grantee));
    let duplicate_grant =
        f.author(&fdr, Some(f.genesis_hash.into()), &stream_grant(stream, grantee));
    let wrong_revoke = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &stream_revoke(stream, AccountId::from_bytes([0x55; 32]), GrantId::from_bytes(grant)),
    );

    let h = f.fold();
    assert!(h.is_effective(&own.into()));
    assert!(h.is_effective(&grant.into()));
    assert_eq!(
        h.grant_effective(GrantId::from_bytes(early_grant), stream, grantee),
        AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
        "a held but ineffective grant is invalid, not parked as if it were missing",
    );
    for (hash, label) in [
        (early_grant, "grant before ownership"),
        (duplicate_own, "duplicate ownership"),
        (self_grant, "self grant"),
        (duplicate_grant, "duplicate same-role grant"),
        (wrong_revoke, "revoke with mismatched grantee"),
    ] {
        assert_eq!(
            h.outcome(&hash.into()),
            Some(Outcome::Rejected(RejectReason::Ineffective)),
            "{label}",
        );
    }
}

#[test]
fn retroactive_ownership_condemnation_transitively_invalidates_a_grant() {
    let (founder, owner) = (Dev::new(1), Dev::new(2));
    let grantee = AccountId::from_bytes([0x44; 32]);
    let mut f = Fixture::genesis(&founder);
    let add_owner =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&owner, DeviceRole::Owner));
    let (stream, own_op) = test_support::stream_own_public(f.account_id);
    let own = f.author(&founder, Some(f.genesis_hash.into()), &own_op);
    let remove_founder = f.author(
        &owner,
        Some(OwnerId::from_bytes(add_owner)),
        &device_remove(&founder, Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(add_owner) }),
    );
    let grant =
        f.author(&owner, Some(OwnerId::from_bytes(add_owner)), &stream_grant(stream, grantee));

    let expected = f.fold();
    assert_eq!(expected.outcome(&own.into()), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));
    assert!(expected.is_effective(&remove_founder.into()));
    assert_eq!(expected.outcome(&grant.into()), Some(Outcome::Rejected(RejectReason::Ineffective)));
    assert_eq!(
        expected.grant_effective(GrantId::from_bytes(grant), stream, grantee),
        AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
    );
    assert_eq!(expected.stream_owner_effective(stream), AuthorityQuery::Unknown);
    for rotation in 1..f.entries.len() {
        assert_eq!(f.fold_rotated(rotation).outcomes, expected.outcomes);
    }
}

#[test]
fn retroactive_enrollment_condemnation_removes_a_promotion_side_effect() {
    let (founder, owner, member) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let add_owner =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&owner, DeviceRole::Owner));
    let add_member =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&member, DeviceRole::Member));
    let promote = f.author(&owner, Some(OwnerId::from_bytes(add_owner)), &owner_promote(&member));
    let cut = f.author(
        &owner,
        Some(OwnerId::from_bytes(add_owner)),
        &device_remove(&founder, Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(add_owner) }),
    );

    let expected = f.fold();
    assert!(expected.is_effective(&cut.into()));
    assert_eq!(
        expected.outcome(&add_member.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
    );
    assert!(!expected.is_effective(&promote.into()));
    assert_eq!(
        expected.owner_incarnation_effective(OwnerId::from_bytes(promote), member.fp,),
        AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
        "a promotion cannot survive solely through a condemned enrollment mutation",
    );
    for rotation in 1..f.entries.len() {
        assert_eq!(f.fold_rotated(rotation).outcomes, expected.outcomes);
    }
}

#[test]
fn fixed_register_replay_removes_a_condemned_tombstone_side_effect() {
    let (founder, owner, member) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    let add_owner =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&owner, DeviceRole::Owner));
    let add_member =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&member, DeviceRole::Member));
    let remove_member =
        f.author(&founder, Some(f.genesis_hash.into()), &device_remove(&member, Cut::Empty));
    f.author(
        &owner,
        Some(OwnerId::from_bytes(add_owner)),
        &device_remove(&founder, Cut::At {
            seq: 2,
            hash: AccountEntryHash::from_bytes(add_member),
        }),
    );
    let promote = f.author(&owner, Some(OwnerId::from_bytes(add_owner)), &owner_promote(&member));

    let expected = f.fold();
    assert_eq!(
        expected.outcome(&remove_member.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
    );
    assert!(
        expected.is_effective(&promote.into()),
        "phase-E replay restores the enrolled member after its tombstone op is condemned",
    );
    for rotation in 1..f.entries.len() {
        assert_eq!(f.fold_rotated(rotation).outcomes, expected.outcomes);
    }
}

#[test]
fn retroactive_grant_condemnation_invalidates_revoke_without_stale_cut_facts() {
    let (founder, owner) = (Dev::new(1), Dev::new(2));
    let grantee = AccountId::from_bytes([0x44; 32]);
    let mut f = Fixture::genesis(&founder);
    let add_owner =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&owner, DeviceRole::Owner));
    let (stream, own_op) = test_support::stream_own_public(f.account_id);
    let own = f.author(&founder, Some(f.genesis_hash.into()), &own_op);
    let grant = f.author(&founder, Some(f.genesis_hash.into()), &stream_grant(stream, grantee));
    let remove_founder = f.author(
        &owner,
        Some(OwnerId::from_bytes(add_owner)),
        &device_remove(&founder, Cut::At { seq: 2, hash: AccountEntryHash::from_bytes(own) }),
    );
    let revoke = f.author(
        &owner,
        Some(OwnerId::from_bytes(add_owner)),
        &stream_revoke(stream, grantee, GrantId::from_bytes(grant)),
    );

    let expected = f.fold();
    assert!(expected.is_effective(&own.into()));
    assert_eq!(
        expected.outcome(&grant.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut))
    );
    assert!(expected.is_effective(&remove_founder.into()));
    assert_eq!(
        expected.outcome(&revoke.into()),
        Some(Outcome::Rejected(RejectReason::Ineffective))
    );
    assert_eq!(
        expected.grant_effective(GrantId::from_bytes(grant), stream, grantee),
        AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
    );
    assert!(expected.grant_cuts.is_empty());
    for rotation in 1..f.entries.len() {
        assert_eq!(f.fold_rotated(rotation).outcomes, expected.outcomes);
    }
}

#[test]
fn final_auth_len_closure_is_independent_of_device_order() {
    let mut saw_orderings = HashSet::new();
    for e_is_a in [true, false] {
        let (founder, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let mut f = Fixture::genesis(&founder);
        let add_a =
            f.author(&founder, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
        let add_b =
            f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
        let (e_author, e_ref, x_author, x_ref) =
            if e_is_a { (&a, add_a, &b, add_b) } else { (&b, add_b, &a, add_a) };
        saw_orderings.insert(e_author.fp < x_author.fp);
        let x = f.author_at_auth_len(
            x_author,
            Some(OwnerId::from_bytes(x_ref)),
            &device_add(&Dev::new(4), DeviceRole::Member),
            3,
        );
        let e = f.author_at_auth_len(
            e_author,
            Some(OwnerId::from_bytes(e_ref)),
            &device_add(&Dev::new(5), DeviceRole::Member),
            4,
        );

        let expected = f.fold();
        assert!(expected.is_effective(&x.into()));
        assert!(expected.is_effective(&e.into()));
        assert_eq!(expected.effective_count(), 5);
        for rotation in 1..f.entries.len() {
            assert_eq!(f.fold_rotated(rotation).outcomes, expected.outcomes);
        }
    }
    assert_eq!(saw_orderings, HashSet::from([false, true]));
}

#[test]
fn stream_own_rejects_wrong_owner_wrong_hash_and_malformed_preimages() {
    let fdr = Dev::new(1);
    let mut f = Fixture::genesis(&fdr);
    let (stream, valid) = test_support::stream_own_public(f.account_id);
    let AccountOp::StreamOwn { stream_spec_bytes, .. } = valid else { unreachable!() };
    let wrong_hash = f.author(&fdr, Some(f.genesis_hash.into()), &AccountOp::StreamOwn {
        stream_id: StreamId::from_bytes([0x77; 32]),
        stream_spec_bytes: stream_spec_bytes.clone(),
    });
    let (_, wrong_owner_op) = test_support::stream_own_public(AccountId::from_bytes([0x66; 32]));
    let wrong_owner = f.author(&fdr, Some(f.genesis_hash.into()), &wrong_owner_op);
    let malformed = f.author(&fdr, Some(f.genesis_hash.into()), &AccountOp::StreamOwn {
        stream_id: stream,
        stream_spec_bytes: vec![0x80],
    });

    let h = f.fold();
    for hash in [wrong_hash, wrong_owner, malformed] {
        assert_eq!(
            h.outcome(&hash.into()),
            Some(Outcome::Rejected(RejectReason::InvalidStreamSpec)),
        );
    }
}

#[test]
fn roster_and_owner_queries_are_citation_time_and_subject_bound() {
    let fdr = Dev::new(1);
    let member = Dev::new(2);
    let mut f = Fixture::genesis(&fdr);
    let add = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&member, DeviceRole::Member));
    let promote = f.author(&fdr, Some(f.genesis_hash.into()), &owner_promote(&member));
    let h = f.fold();

    assert_eq!(
        h.roster_ref_effective(RosterRef::from_bytes(add), member.fp),
        AuthorityQuery::Effective(RosterAuthority {
            device_fingerprint: member.fp,
            current_role: DeviceRole::Owner,
        }),
    );
    assert_eq!(
        h.roster_ref_effective(RosterRef::from_bytes(add), fdr.fp),
        AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject),
    );
    assert_eq!(
        h.owner_incarnation_effective(OwnerId::from_bytes(promote), member.fp),
        AuthorityQuery::Effective(OwnerAuthority { device_fingerprint: member.fp }),
        "a behind auth_len does not deny an exact owner citation",
    );
    assert_eq!(
        h.owner_incarnation_effective(OwnerId::from_bytes(promote), member.fp),
        AuthorityQuery::Effective(OwnerAuthority { device_fingerprint: member.fp }),
    );

    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &owner_demote(&member, OwnerId::from_bytes(promote), Cut::Empty),
    );
    let h = f.fold();
    assert_eq!(
        h.roster_ref_effective(RosterRef::from_bytes(add), member.fp),
        AuthorityQuery::Effective(RosterAuthority {
            device_fingerprint: member.fp,
            current_role: DeviceRole::Member,
        }),
    );
    assert_eq!(
        h.owner_incarnation_effective(OwnerId::from_bytes(promote), member.fp),
        AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
    );

    f.author(&fdr, Some(f.genesis_hash.into()), &device_remove(&member, Cut::Empty));
    let h = f.fold();
    assert_eq!(
        h.roster_ref_effective(RosterRef::from_bytes(add), member.fp),
        AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
    );
}

#[test]
fn a_read_only_enrollment_cannot_be_promoted_and_re_bless_prior_content() {
    let founder = Dev::new(1);
    let read_only = Dev::new(2);
    let stream = StreamId::from_bytes([0x44; 32]);
    let mut f = Fixture::genesis(&founder);
    let add = f.author(
        &founder,
        Some(f.genesis_hash.into()),
        &device_add(&read_only, DeviceRole::ReadOnly),
    );
    let promote = f.author(&founder, Some(f.genesis_hash.into()), &owner_promote(&read_only));

    let h = f.fold();
    assert_eq!(
        h.outcome(&promote.into()),
        Some(Outcome::Rejected(RejectReason::BadPromote)),
        "promotion cannot turn a read-only enrollment into retroactive write authority",
    );
    assert_eq!(
        h.roster_content_authority(RosterRef::from_bytes(add), read_only.fp, stream),
        AuthorityQuery::Effective(RosterContentAuthority {
            device_fingerprint: read_only.fp,
            role: DeviceRole::ReadOnly,
            boundary: AuthorityBoundary::Open,
        }),
        "the cited roster fact remains read-only after the rejected promotion",
    );
}

#[test]
fn a_cross_depth_cut_extend_does_not_re_bless_within_a_fold() {
    // §11.1 is per-depth monotone: condemnation grows depth by depth and a lower-depth decision
    // is final. A CutExtend re-blesses its cone ONLY when joined at the CREATOR's depth (the
    // same-depth `⊔`, covered by `cut_extend_reblesses_a_condemned_cone_p7`). Here F removes B
    // at depth 0 (condemning b1/b2) and a DEEPER owner A (depth 1) extends the cut — because
    // A's extend lands a stratum later than the depth-0 condemnation, the cone stays
    // condemned in this fold. A global graph fixpoint is forbidden by the frozen v5 model
    // because it can preserve register power for an already-condemned creator.
    let (fdr, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let (d, e, g) = (Dev::new(5), Dev::new(6), Dev::new(7));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let b0 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&d, DeviceRole::Member));
    let b1 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&e, DeviceRole::Member));
    let b2 = f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&g, DeviceRole::Member));
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove(&b, Cut::At { seq: 0, hash: AccountEntryHash::from_bytes(b0) }),
    );
    let extend = f.author(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &cut_extend_ctrl(f.account_id, &b, None, 2, AccountEntryHash::from_bytes(b2)),
    );

    let h = f.fold();
    assert!(h.is_effective(&b0.into()), "the within-cut prefix is effective");
    assert!(h.is_effective(&extend.into()), "the extend itself is a valid owner op");
    assert_eq!(
        h.outcome(&b1.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        "a deeper-depth extend does not revise a shallower depth's final condemnation",
    );
}

#[test]
fn account_reroot_in_a_live_account_is_ineffective() {
    // AccountReRoot is admissible ONLY as the terminal recovery op once contested (§12). In a
    // Live account it must not fold effective — else it consumes an auth_epoch and enters the
    // effective history, and could be auto-selected if a contest later appears.
    let fdr = Dev::new(1);
    let mut f = Fixture::genesis(&fdr);
    let reroot = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &account_reroot(AccountId::from_bytes([0x55; 32])),
    );

    let h = f.fold();
    assert_eq!(h.classification(), AccountClassification::Live);
    assert_eq!(h.outcome(&reroot.into()), Some(Outcome::Rejected(RejectReason::Ineffective)));
    assert_eq!(h.contested_successor(), None);
}

#[test]
fn a_device_removing_its_own_chain_is_self_defeating() {
    // A self-cut cannot strand the account: the removal op sits at a higher seq than any
    // watermark it can name on its own chain, so the register it installs condemns the removal
    // itself. The founder therefore stays an owner — no zero-owner state, no I2 needed.
    let fdr = Dev::new(1);
    let mut f = Fixture::genesis(&fdr);
    let remove_self = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove(&fdr, Cut::At { seq: 0, hash: f.genesis_hash }),
    );

    let h = f.fold();
    assert!(h.is_effective(&f.genesis_hash), "genesis stands — the founder remains an owner");
    assert!(!h.is_effective(&remove_self.into()), "a device cannot remove its own chain");
}

#[test]
fn a_stale_owner_demote_does_not_close_a_reopened_incarnation() {
    // B is demoted (g1), then re-promoted (g2). A stale OwnerDemote naming the OLD incarnation
    // g1 must not close B's fresh g2 — the demote is scoped to its exact incarnation.
    let (fdr, b, t) = (Dev::new(1), Dev::new(2), Dev::new(5));
    let mut f = Fixture::genesis(&fdr);
    let g1 = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &owner_demote(&b, OwnerId::from_bytes(g1), Cut::Empty),
    );
    let g2 = f.author(&fdr, Some(f.genesis_hash.into()), &owner_promote(&b));
    // A late, stale demote of the OLD incarnation g1.
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &owner_demote(&b, OwnerId::from_bytes(g1), Cut::Empty),
    );
    // B authors under the fresh incarnation g2.
    let under_g2 = f.author(&b, Some(OwnerId::from_bytes(g2)), &device_add(&t, DeviceRole::Member));

    let h = f.fold();
    assert!(h.is_effective(&g2.into()), "the re-promotion mints a fresh incarnation");
    assert!(
        h.is_effective(&under_g2.into()),
        "B's fresh-incarnation work survives the stale demote of the old incarnation",
    );
}

#[test]
fn a_contested_reroot_by_a_non_owner_is_not_admitted() {
    // Owners A and B cut each other ⇒ contested. A MEMBER M then signs an AccountReRoot citing
    // A's incarnation. Because the signer is not A, the recovery admission (the FULL authority
    // check, not just liveness) rejects it — a non-owner cannot select the successor.
    let (fdr, a, b, m) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&m, DeviceRole::Member));
    f.author(&a, Some(OwnerId::from_bytes(add_a)), &device_remove(&b, Cut::Empty));
    f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_remove(&a, Cut::Empty));
    let bad_reroot = f.author(
        &m,
        Some(OwnerId::from_bytes(add_a)),
        &account_reroot(AccountId::from_bytes([0x11; 32])),
    );

    let h = f.fold();
    assert!(matches!(h.classification(), AccountClassification::Contested { .. }));
    assert!(
        !h.is_effective(&bad_reroot.into()),
        "a non-owner cannot select the recovery successor"
    );
    assert_eq!(h.contested_successor(), None, "no owner re-rooted ⇒ no successor");
}

#[test]
fn a_demoted_former_owner_cannot_select_the_contested_successor() {
    // A is demoted (its incarnation leaves `owners` but stays in `live` — its under-cut history
    // is still valid). Owners B and C then cut each other ⇒ contested. Both a CURRENT owner (F)
    // and the demoted A submit re-roots; only F's is admitted. Even though A picked the smaller
    // successor id, the deterministic successor is F's — a former owner cannot select it.
    let (fdr, a, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let (succ_a, succ_f) = (AccountId::from_bytes([0x11; 32]), AccountId::from_bytes([0x22; 32]));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let add_c = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&c, DeviceRole::Owner));
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &owner_demote(&a, OwnerId::from_bytes(add_a), Cut::Empty),
    );
    f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_remove(&c, Cut::Empty));
    f.author(&c, Some(OwnerId::from_bytes(add_c)), &device_remove(&b, Cut::Empty));
    let reroot_f = f.author(&fdr, Some(f.genesis_hash.into()), &account_reroot(succ_f));
    let reroot_a = f.author(&a, Some(OwnerId::from_bytes(add_a)), &account_reroot(succ_a));

    let h = f.fold();
    assert!(matches!(h.classification(), AccountClassification::Contested { .. }));
    assert!(h.is_effective(&reroot_f.into()), "a current owner's re-root is admitted");
    assert!(!h.is_effective(&reroot_a.into()), "a demoted former owner's re-root is not admitted");
    assert_eq!(
        h.contested_successor(),
        Some(succ_f),
        "the successor is the current owner's, not the demoted owner's smaller id",
    );
}

#[test]
fn a_forged_genesis_re_signed_by_a_non_owner_cannot_take_over_the_account() {
    // `account_id` commits to the founder pubkey inside the (public) genesis payload, but not
    // to the SIGNER. An attacker copies the victim's genesis payload verbatim and
    // re-signs it under its OWN device key. The fold must bind the founder device to
    // the committed pubkey — else, when the forgery is folded first, the attacker
    // becomes founder-owner — and must pick the genesis deterministically regardless of
    // arrival order (I9).
    let (victim, attacker, x) = (Dev::new(1), Dev::new(9), Dev::new(3));
    let op = AccountOp::AccountGenesis {
        ed25519_pubkey: victim.ed,
        x25519_pubkey: victim.x,
        nonce16: [0u8; 16],
        created_at_ms: 1_700_000_000_000,
        label: None,
    };
    let payload = account_ops::encode(&op).unwrap();
    let account_id = id::account_id_from_genesis_payload(&payload);
    let genesis_header = |signer: &Dev| AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: signer.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: None,
        entry_type: account_ops::entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        crypto_suite: 0,
        auth_len: 0,
        key_id: None,
        authority_ref: None,
    };
    let signed = |signer: &Dev, hdr: &AccountEntryHeader, pl: &[u8]| {
        let s = sign_account_entry(&signer.secret, hdr, pl).unwrap();
        verify_account_signed(&s.signed_bytes, &signer.secret.public()).unwrap()
    };
    // Both genesis entries carry the SAME (victim) payload; only the signer differs.
    let real = signed(&victim, &genesis_header(&victim), &payload);
    let forged = signed(&attacker, &genesis_header(&attacker), &payload);
    // The attacker adds itself as owner, citing its forged genesis.
    let add_op = device_add(&x, DeviceRole::Owner);
    let add_payload = account_ops::encode(&add_op).unwrap();
    let add_header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: attacker.fp,
        seq: 1,
        prev_hash: Some(forged.entry_hash),
        parent_ref: Some(forged.entry_hash),
        entry_type: account_ops::entry_type_of(&add_op),
        op_version: 1,
        auth_len: 1,
        crypto_suite: 0,
        key_id: None,
        authority_ref: Some(forged.entry_hash.into()),
    };
    let attacker_add = signed(&attacker, &add_header, &add_payload);

    // Arrival order must not matter — the forgery must lose in both.
    for order in [vec![real.clone(), forged.clone(), attacker_add.clone()], vec![
        forged.clone(),
        attacker_add.clone(),
        real.clone(),
    ]] {
        let h = fold_account(&order);
        assert!(h.is_effective(&real.entry_hash), "the founder-signed genesis is effective");
        assert!(
            !h.is_effective(&forged.entry_hash),
            "a genesis re-signed by a non-owner is not a valid root",
        );
        assert!(
            !h.is_effective(&attacker_add.entry_hash),
            "an op citing the forged genesis gains no authority",
        );
        assert_eq!(h.classification(), AccountClassification::Live);
    }
}

#[test]
fn the_genesis_root_is_never_condemned() {
    // A DeviceRemove(founder, empty-cut) installs Device{founder} = ∅, which would condemn the
    // WHOLE founder chain — including the seq-0 genesis. The root is EXEMPT, so the account
    // keeps an effective genesis instead of folding `Live` with no root. (The
    // self-removal is itself self-defeating; the point is the root survives.)
    let fdr = Dev::new(1);
    let mut f = Fixture::genesis(&fdr);
    f.author(&fdr, Some(f.genesis_hash.into()), &device_remove(&fdr, Cut::Empty));

    let h = f.fold();
    assert!(h.is_effective(&f.genesis_hash), "the genesis root is exempt from condemnation");
    assert_eq!(h.classification(), AccountClassification::Live);
}

#[test]
fn a_known_op_on_a_non_control_log_is_not_folded() {
    // The fold operates on the control log (0); the registers are control-log scoped. A
    // DeviceAdd(owner) on log 1 must NOT mint control authority — it is retained unfolded.
    let (fdr, x) = (Dev::new(1), Dev::new(2));
    let f = Fixture::genesis(&fdr);
    let op = device_add(&x, DeviceRole::Owner);
    let payload = account_ops::encode(&op).unwrap();
    let header = AccountEntryHeader {
        account_id: f.account_id,
        log_id: 1, // secrets log — not the control log
        device_fingerprint: fdr.fp,
        seq: 1,
        prev_hash: Some(f.genesis_hash),
        parent_ref: Some(f.genesis_hash),
        entry_type: account_ops::entry_type_of(&op),
        op_version: 1,
        auth_len: 1,
        crypto_suite: 0,
        key_id: None,
        authority_ref: Some(f.genesis_hash.into()),
    };
    let signed = sign_account_entry(&fdr.secret, &header, &payload).unwrap();
    let entry = verify_account_signed(&signed.signed_bytes, &fdr.secret.public()).unwrap();
    let mut entries = f.entries.clone();
    entries.push(entry.clone());

    let h = fold_account(&entries);
    assert_eq!(
        h.outcome(&entry.entry_hash),
        Some(Outcome::RetainedUnfolded),
        "a non-control-log op is retained, never folded as control authority",
    );
}

#[test]
fn a_known_op_at_an_unsupported_version_is_retained() {
    // A known entry_type at a future op_version may reuse the tag with different semantics — it
    // must be retained unfolded, not folded as today's op.
    let (fdr, x) = (Dev::new(1), Dev::new(2));
    let f = Fixture::genesis(&fdr);
    let op = device_add(&x, DeviceRole::Owner);
    let payload = account_ops::encode(&op).unwrap();
    let header = AccountEntryHeader {
        account_id: f.account_id,
        log_id: 0,
        device_fingerprint: fdr.fp,
        seq: 1,
        prev_hash: Some(f.genesis_hash),
        parent_ref: Some(f.genesis_hash),
        entry_type: account_ops::entry_type_of(&op),
        op_version: 2, // future version
        auth_len: 1,
        crypto_suite: 0,
        key_id: None,
        authority_ref: Some(f.genesis_hash.into()),
    };
    let signed = sign_account_entry(&fdr.secret, &header, &payload).unwrap();
    let entry = verify_account_signed(&signed.signed_bytes, &fdr.secret.public()).unwrap();
    let mut entries = f.entries.clone();
    entries.push(entry.clone());

    let h = fold_account(&entries);
    assert_eq!(h.outcome(&entry.entry_hash), Some(Outcome::RetainedUnfolded));
}

#[test]
fn removing_a_never_enrolled_device_is_ineffective() {
    // DeviceRemove of a device that was never added must be ineffective — else it tombstones
    // the fingerprint and permanently bars a later legitimate DeviceAdd (I4).
    let (fdr, ghost) = (Dev::new(1), Dev::new(7));
    let mut f = Fixture::genesis(&fdr);
    let remove = f.author(&fdr, Some(f.genesis_hash.into()), &device_remove(&ghost, Cut::Empty));
    let add = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&ghost, DeviceRole::Member));

    let h = f.fold();
    assert_eq!(
        h.outcome(&remove.into()),
        Some(Outcome::Rejected(RejectReason::Ineffective)),
        "removing a never-enrolled device is ineffective",
    );
    assert!(
        h.is_effective(&add.into()),
        "the device is not pre-tombstoned, so it can still be added"
    );
    let fact = h.roster_refs.get(&add.into()).expect("later enrollment has a roster fact");
    assert_eq!(fact.control_boundary, AuthorityBoundary::Closed);
}

#[test]
fn a_deep_incarnation_chain_folds_without_a_stack_overflow() {
    // `incarnation_depth` walks the authority_ref chain ITERATIVELY. Its depth is computed for
    // every candidate BEFORE any authority check, and chain length is adversary-controlled, so
    // a deep chain must fold to a classification, never recurse to a stack overflow.
    // Delivered DEEPEST-FIRST (the crash-inducing order) and folded on a SMALL stack,
    // so a recursive regression would abort loudly here.
    const N: u32 = 2500;
    let founder = Dev::seeded(0);
    let mut f = Fixture::genesis(&founder);
    let genesis_hash = f.genesis_hash;
    let mut g = genesis_hash;
    let mut devs = vec![founder];
    for k in 1..=N {
        let dev = Dev::seeded(k);
        g = AccountEntryHash::from_bytes(f.author(
            &devs[(k - 1) as usize],
            Some(g.into()),
            &device_add(&dev, DeviceRole::Owner),
        ));
        devs.push(dev);
    }
    let mut entries = f.entries.clone();
    entries.reverse(); // deepest-first — memoization can't keep the recursion shallow

    // A 256 KiB stack comfortably fits the iterative fold's constant call depth (a few frames)
    // yet is overflowed several times over by an N-deep recursion (2500 frames). 64 KiB was too
    // small for Windows, whose per-thread overhead and wider (shadow-space) frames need more
    // headroom to run even the iterative fold — so the stack is sized above that floor, and N
    // keeps the recursion-vs-iterative margin (a regression aborts here) while staying under
    // the 60 s slow-test budget (the fold is O(N²): deepest-first delivery defeats
    // memoization).
    let effective = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || fold_account(&entries).is_effective(&genesis_hash))
        .unwrap()
        .join()
        .unwrap();
    assert!(effective, "a deep delegation chain folds (genesis effective), no overflow");
}

#[test]
fn duplicate_entries_are_folded_once() {
    // Folding is a function of the entry SET, not the multiset — a duplicated verified entry
    // must classify once and apply its state transition once (order-independence).
    let (fdr, b) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let mut doubled = f.entries.clone();
    doubled.extend(f.entries.clone());

    let h = fold_account(&doubled);
    assert!(h.is_effective(&f.genesis_hash));
    assert!(
        h.is_effective(&add_b.into()),
        "the duplicated add is effective once, not overwritten as a DuplicateAdd",
    );
}

#[test]
fn a_genesis_with_non_root_header_fields_is_not_selected_as_root() {
    // The canonical root has no parent_ref and auth_len 0 (§6). A malformed same-payload
    // genesis with a non-null parent_ref must be EXCLUDED from root selection (not
    // merely lose the min-hash tiebreak), so the real root is always chosen and its
    // descendants authorize.
    let founder = Dev::new(1);
    let real = Fixture::genesis(&founder);
    let op = AccountOp::AccountGenesis {
        ed25519_pubkey: founder.ed,
        x25519_pubkey: founder.x,
        nonce16: [0u8; 16],
        created_at_ms: 1_700_000_000_000,
        label: None,
    };
    let payload = account_ops::encode(&op).unwrap();
    let account_id = id::account_id_from_genesis_payload(&payload);
    let header = AccountEntryHeader {
        account_id,
        log_id: 0,
        device_fingerprint: founder.fp,
        seq: 0,
        prev_hash: None,
        parent_ref: Some(AccountEntryHash::from_bytes([0x01; 32])), // a root has no parent
        entry_type: account_ops::entry_type::ACCOUNT_GENESIS,
        op_version: 1,
        auth_len: 0,
        crypto_suite: 0,
        key_id: None,
        authority_ref: None,
    };
    let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
    let malformed = verify_account_signed(&signed.signed_bytes, &founder.secret.public()).unwrap();
    let mut entries = real.entries.clone();
    entries.push(malformed.clone());

    let h = fold_account(&entries);
    assert!(h.is_effective(&real.genesis_hash), "the canonical root is selected regardless");
    assert!(
        !h.is_effective(&malformed.entry_hash),
        "a genesis with non-root header fields is not the root",
    );
}

#[test]
fn a_second_seq0_entry_on_the_founder_chain_is_rejected() {
    // The founder's seq-0 origin slot is the genesis alone; a second founder-signed seq-0 op is
    // an origin equivocation with the root — rejected, so it cannot take auth_epoch 0 ahead of
    // genesis.
    let (fdr, x) = (Dev::new(1), Dev::new(2));
    let f = Fixture::genesis(&fdr);
    let op = device_add(&x, DeviceRole::Owner);
    let payload = account_ops::encode(&op).unwrap();
    let header = AccountEntryHeader {
        account_id: f.account_id,
        log_id: 0,
        device_fingerprint: fdr.fp, // the founder's device
        seq: 0,
        prev_hash: None,
        parent_ref: Some(f.genesis_hash),
        entry_type: account_ops::entry_type_of(&op),
        op_version: 1,
        auth_len: 1,
        crypto_suite: 0,
        key_id: None,
        authority_ref: Some(f.genesis_hash.into()),
    };
    let signed = sign_account_entry(&fdr.secret, &header, &payload).unwrap();
    let orphan = verify_account_signed(&signed.signed_bytes, &fdr.secret.public()).unwrap();
    let mut entries = f.entries.clone();
    entries.push(orphan.clone());

    let h = fold_account(&entries);
    assert!(h.is_effective(&f.genesis_hash), "genesis is the founder's only origin slot");
    assert_eq!(
        h.outcome(&orphan.entry_hash),
        Some(Outcome::Rejected(RejectReason::NonGenesisOrigin)),
    );
}

#[test]
fn a_sealed_entry_with_unparseable_bytes_is_retained_not_malformed() {
    // A non-foldable (sealed) entry is retained header-only regardless of whether its
    // ciphertext parses — it is NEVER hard-rejected as Malformed (which would drop its
    // header from the ancestry view and hard-reject a forward-compatible entry).
    let fdr = Dev::new(1);
    let f = Fixture::genesis(&fdr);
    let header = AccountEntryHeader {
        account_id: f.account_id,
        log_id: 0,
        device_fingerprint: fdr.fp,
        seq: 1,
        prev_hash: Some(f.genesis_hash),
        parent_ref: Some(f.genesis_hash),
        entry_type: account_ops::entry_type::DEVICE_ADD,
        op_version: 1,
        auth_len: 1,
        crypto_suite: 1,          // sealed
        key_id: Some([0x33; 32]), // required when crypto_suite != 0
        authority_ref: Some(f.genesis_hash.into()),
    };
    let bad = vec![0xff, 0xff]; // not valid CBOR / not a DeviceAdd
    let signed = sign_account_entry(&fdr.secret, &header, &bad).unwrap();
    let entry = verify_account_signed(&signed.signed_bytes, &fdr.secret.public()).unwrap();
    let mut entries = f.entries.clone();
    entries.push(entry.clone());

    let h = fold_account(&entries);
    assert_eq!(
        h.outcome(&entry.entry_hash),
        Some(Outcome::RetainedUnfolded),
        "a sealed entry is retained, not hard-rejected, even if its bytes don't parse",
    );
}

#[test]
fn a_malformed_current_control_op_is_rejected_not_retained() {
    // A control-log, supported-version entry whose payload does NOT decode as its entry_type is
    // a hard reject — not RetainedUnfolded — so a malformed entry's header never becomes a
    // valid cut-ancestry watermark.
    let fdr = Dev::new(1);
    let f = Fixture::genesis(&fdr);
    let header = AccountEntryHeader {
        account_id: f.account_id,
        log_id: 0,
        device_fingerprint: fdr.fp,
        seq: 1,
        prev_hash: Some(f.genesis_hash),
        parent_ref: Some(f.genesis_hash),
        entry_type: account_ops::entry_type::DEVICE_ADD,
        op_version: 1,
        auth_len: 1,
        crypto_suite: 0,
        key_id: None,
        authority_ref: Some(f.genesis_hash.into()),
    };
    let bad_payload = vec![0xa0]; // a CBOR empty map — not the DeviceAdd array shape
    let signed = sign_account_entry(&fdr.secret, &header, &bad_payload).unwrap();
    let entry = verify_account_signed(&signed.signed_bytes, &fdr.secret.public()).unwrap();
    let mut entries = f.entries.clone();
    entries.push(entry.clone());

    let h = fold_account(&entries);
    assert_eq!(
        h.outcome(&entry.entry_hash),
        Some(Outcome::Rejected(RejectReason::Malformed)),
        "a malformed current op is a hard reject, not retained",
    );
}

#[test]
fn a_sealed_payload_is_not_folded_as_a_plaintext_op() {
    // `crypto_suite != 0` means the payload is sealed (C4); the fold must not decode it as a
    // plaintext control op even if the ciphertext happens to parse as one.
    let (fdr, x) = (Dev::new(1), Dev::new(2));
    let f = Fixture::genesis(&fdr);
    let op = device_add(&x, DeviceRole::Owner);
    let payload = account_ops::encode(&op).unwrap(); // valid DeviceAdd bytes, but the header marks it sealed
    let header = AccountEntryHeader {
        account_id: f.account_id,
        log_id: 0,
        device_fingerprint: fdr.fp,
        seq: 1,
        prev_hash: Some(f.genesis_hash),
        parent_ref: Some(f.genesis_hash),
        entry_type: account_ops::entry_type_of(&op),
        op_version: 1,
        auth_len: 1,
        crypto_suite: 1,
        key_id: Some([0x33; 32]), // required when crypto_suite != 0
        authority_ref: Some(f.genesis_hash.into()),
    };
    let signed = sign_account_entry(&fdr.secret, &header, &payload).unwrap();
    let entry = verify_account_signed(&signed.signed_bytes, &fdr.secret.public()).unwrap();
    let mut entries = f.entries.clone();
    entries.push(entry.clone());

    let h = fold_account(&entries);
    assert_eq!(
        h.outcome(&entry.entry_hash),
        Some(Outcome::RetainedUnfolded),
        "a sealed-payload op is deferred, never folded as a plaintext control op",
    );
}

#[test]
fn a_content_cut_extend_is_deferred_not_effective() {
    // A CONTENT `CutExtend` binds a stream chain (C2's fold), so the account fold has no
    // register for it and must defer it, never marking it effective on a target this fold never
    // validated. (A secrets extend, by contrast, IS an account-log chain — see
    // `a_secrets_cut_extend_reblesses_beyond_a_secrets_cut`.)
    let fdr = Dev::new(1);
    let mut f = Fixture::genesis(&fdr);
    let op = AccountOp::CutExtend {
        chain_kind: ops::ChainKind::Content,
        stream_id: Some(StreamId::from_bytes([0x55; 32])),
        incarnation_id: None,
        subject_account_id: f.account_id,
        device_fingerprint: fdr.fp,
        new_seq: 3,
        new_entry_hash: AccountEntryHash::from_bytes([0x44; 32]),
    };
    let extend = f.author(&fdr, Some(f.genesis_hash.into()), &op);

    let h = f.fold();
    assert_eq!(
        h.outcome(&extend.into()),
        Some(Outcome::Parked(ParkReason::DeferredStreamAuthorization)),
        "a content CutExtend is deferred, not effective",
    );
}

#[test]
fn a_secrets_cut_extend_without_a_creating_register_parks_not_defers() {
    // A secrets `CutExtend` is now admissible on the account log (it extends the device's
    // secrets-chain register), but it is EXTEND-ONLY: with no prior DeviceRemove/OwnerDemote to
    // create that register it parks `unknown_cut_target` (re-joining once the creator syncs) —
    // the gap B1 closes, where it used to park `deferred_stream_authorization` forever.
    let fdr = Dev::new(1);
    let mut f = Fixture::genesis(&fdr);
    let extend = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &cut_extend_secrets(f.account_id, &fdr, None, 3, AccountEntryHash::from_bytes([0x44; 32])),
    );

    let h = f.fold();
    assert_eq!(
        h.outcome(&extend.into()),
        Some(Outcome::Parked(ParkReason::UnknownCutTarget)),
        "a secrets CutExtend with no creating register parks, extend-only",
    );
}

#[test]
fn a_device_remove_installs_a_queryable_secrets_register() {
    // A DeviceRemove carrying a secrets_cut installs a log-1 device register bound at
    // CutCoordinate{log: SECRETS_LOG} (§11.3), so the removed device's secrets_boundary is the
    // validated, joined watermark — queryable via owner_secrets_authority's device_boundary —
    // NOT a raw op-field copy and NOT Open/Closed. The empty control cut proves the two chains
    // are bounded independently under one op.
    let (fdr, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    // A second owner so removing B is not the last-owner reject.
    f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&c, DeviceRole::Owner));
    // A held watermark on B's secrets chain (log 1) so §11.3 binding is Ok, not TargetNotHeld.
    let s0 = f.author_secrets_entry(&b, &b, 0, None);
    let remove_b = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove_with_secrets(&b, Cut::Empty, Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes(s0),
        }),
    );

    let h = f.fold();
    assert!(h.is_effective(&remove_b.into()), "the remove is admitted — both cuts bind");
    let fact = h.roster_refs.get(&add_b.into()).expect("B has a roster fact");
    assert_eq!(
        fact.secrets_boundary,
        AuthorityBoundary::Cut { seq: 0, hash: AccountEntryHash::from_bytes(s0) },
        "secrets_boundary is the joined log-1 register",
    );
    assert_eq!(
        fact.control_boundary,
        AuthorityBoundary::Closed,
        "the control chain is bounded independently by its own (empty) cut",
    );
    match h.owner_secrets_authority(OwnerId::from_bytes(add_b), b.fp) {
        AuthorityQuery::Effective(auth) => assert_eq!(
            auth.device_boundary,
            AuthorityBoundary::Cut { seq: 0, hash: AccountEntryHash::from_bytes(s0) },
            "owner_secrets_authority device_boundary reflects the secrets register",
        ),
        other => panic!("expected an effective owner-secrets authority, got {other:?}"),
    }
}

#[test]
fn an_owner_demote_secrets_cut_bounds_the_incarnation() {
    // An OwnerDemote's secrets_cut installs an owner-incarnation register on the demoted
    // incarnation's secrets chain, so owner_secrets_authority's incarnation_boundary is the
    // validated, joined watermark. (Mirrors the control owner-incarnation boundary.)
    let (fdr, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&c, DeviceRole::Owner));
    let s0 = f.author_secrets_entry(&b, &b, 0, None);
    let demote_b = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &owner_demote_with_secrets(&b, OwnerId::from_bytes(add_b), Cut::Empty, Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes(s0),
        }),
    );

    let h = f.fold();
    assert!(h.is_effective(&demote_b.into()), "the demote is admitted — both cuts bind");
    match h.owner_secrets_authority(OwnerId::from_bytes(add_b), b.fp) {
        AuthorityQuery::Effective(auth) => assert_eq!(
            auth.incarnation_boundary,
            AuthorityBoundary::Cut { seq: 0, hash: AccountEntryHash::from_bytes(s0) },
            "owner_secrets_authority incarnation_boundary reflects the secrets register",
        ),
        other => panic!("expected an effective owner-secrets authority, got {other:?}"),
    }
}

#[test]
fn a_secrets_cut_extend_reblesses_beyond_a_secrets_cut() {
    // Mirror the control cut-extend re-bless (p7) on the SECRETS chain. F removes B pinned to
    // B's secrets seq 0, then a CutExtend{Secrets} raises that register to seq 2. The secrets
    // register is `⊔`-joined, so B's secrets_boundary reflects the RAISED watermark (seq 2) and
    // the extend is effective — the gap B1 closes (a secrets extend used to park forever).
    let (fdr, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&c, DeviceRole::Owner));
    // B's secrets chain s0 <- s1 <- s2 (log 1), so the extend's watermark descends the
    // remove's.
    let s0 = f.author_secrets_entry(&b, &b, 0, None);
    let s1 = f.author_secrets_entry(&b, &b, 1, Some(AccountEntryHash::from_bytes(s0)));
    let s2 = f.author_secrets_entry(&b, &b, 2, Some(AccountEntryHash::from_bytes(s1)));
    let remove_b = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove_with_secrets(&b, Cut::Empty, Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes(s0),
        }),
    );
    let extend = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &cut_extend_secrets(f.account_id, &b, None, 2, AccountEntryHash::from_bytes(s2)),
    );

    // Without the extend, the boundary is the original seq-0 watermark.
    let before = f.fold_without(extend);
    assert_eq!(
        before.roster_refs.get(&add_b.into()).unwrap().secrets_boundary,
        AuthorityBoundary::Cut { seq: 0, hash: AccountEntryHash::from_bytes(s0) },
    );

    // With the extend, the register joins to seq 2 and the extend is effective.
    let after = f.fold();
    assert!(after.is_effective(&extend.into()), "the secrets extend re-blesses and is effective");
    assert!(after.is_effective(&remove_b.into()));
    assert_eq!(
        after.roster_refs.get(&add_b.into()).unwrap().secrets_boundary,
        AuthorityBoundary::Cut { seq: 2, hash: AccountEntryHash::from_bytes(s2) },
        "secrets_boundary reflects the extend-raised (joined) watermark, not the raw cut",
    );
}

#[test]
fn a_misbound_secrets_cut_rejects_the_whole_cut_op() {
    // A held secrets_cut watermark naming a DIFFERENT coordinate (here a control-log entry, not
    // a log-1 entry on B's chain) fails §11.3 binding. Extending the control-cut precedent,
    // that REJECTS the whole cut op (cut_target_mismatch) — it no longer projects the
    // bad watermark silently. The control cut is a valid Empty, so the rejection is due
    // to the secrets cut.
    let (fdr, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let add_c = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&c, DeviceRole::Owner));
    let remove_b = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        // add_c is a held CONTROL-log entry — the wrong coordinate for a log-1 watermark on B.
        &device_remove_with_secrets(&b, Cut::Empty, Cut::At {
            seq: 1,
            hash: AccountEntryHash::from_bytes(add_c),
        }),
    );

    let h = f.fold();
    assert_eq!(
        h.outcome(&remove_b.into()),
        Some(Outcome::Rejected(RejectReason::CutTargetMismatch)),
        "a misbound secrets_cut rejects the whole remove",
    );
    assert!(
        h.is_effective(&add_b.into()),
        "B stays enrolled — the misbound remove never took effect"
    );
}

#[test]
fn incomparable_secrets_cuts_for_one_key_are_contested() {
    // Two owners A and B each remove D, but their SECRETS cuts name two different seq-0
    // watermarks on D's secrets chain. One register key (Device{log: SECRETS_LOG, D}) with
    // equal-seq / different-hash cuts is incomparable — the fold folds contested (§11.3),
    // exactly as an incomparable CONTROL pair does. (Equal-seq/different-hash is incomparable
    // without an ancestry lookup, so the watermarks need not be held.)
    let (fdr, a, b, d) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let add_d = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&d, DeviceRole::Owner));
    // Control cuts Empty (comparable); secrets cuts at the same seq with distinct hashes.
    f.author(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove_with_secrets(&d, Cut::Empty, Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes([0xaa; 32]),
        }),
    );
    f.author(
        &b,
        Some(OwnerId::from_bytes(add_b)),
        &device_remove_with_secrets(&d, Cut::Empty, Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes([0xbb; 32]),
        }),
    );

    let h = f.fold();
    assert_eq!(
        h.classification(),
        AccountClassification::Contested { state_before_depth: 1 },
        "incomparable secrets cuts for one key fold contested",
    );
    assert!(
        h.is_effective(&add_a.into())
            && h.is_effective(&add_b.into())
            && h.is_effective(&add_d.into())
    );
    // Arrival-order-free: the incomparable join is symmetric (I9).
    for rot in 0..f.entries.len() {
        assert_eq!(
            f.fold_rotated(rot).classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "rotation {rot} must reach the same contested verdict",
        );
    }
}

#[test]
fn a_cut_op_that_parks_on_one_chain_raises_neither_register() {
    // Register-join ATOMICITY. One signed cut op cuts BOTH the device's control chain and its
    // secrets chain (unlike the single-register control precedent). If EITHER chain's join is
    // undecidable, the WHOLE op parks and NEITHER register is raised — a parked (non-effective)
    // op must never advance a boundary. Here g's control cut WOULD extend d's control register
    // (Empty ⊔ At = At), but its secrets cut is undecidable against the already-installed
    // secrets register (its higher watermark is not in the view), so the whole op parks
    // and d's control boundary stays at the founder's value. A commit-as-you-go join
    // would advance the control register while the secrets one parks — this test's
    // control-boundary assertion is the discriminator that fails without the atomicity
    // guard.
    let (fdr, g, d, e) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&fdr);
    // g is a DEEPER incarnation than the founder, so its cut sits at a strictly LATER stratum —
    // it deterministically joins AFTER the founder's remove installs d's registers.
    let add_g = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&g, DeviceRole::Owner));
    let add_d = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&d, DeviceRole::Owner));
    f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&e, DeviceRole::Owner)); // spare owner
    // Founder removes d, installing d's control register (Empty) and secrets register (At{0}).
    let remove_d = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove_with_secrets(&d, Cut::Empty, Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes([0x50; 32]),
        }),
    );
    // g removes d again: its control cut WOULD raise d's control register (Empty ⊔ At{0} =
    // At{0}), but its secrets cut is undecidable — the higher watermark [0x52] is not held, so
    // the branch relation against the founder's At{0} secrets register can't be decided → the
    // secrets join parks.
    let remove_d_by_g = f.author(
        &g,
        Some(OwnerId::from_bytes(add_g)),
        &device_remove_with_secrets(
            &d,
            Cut::At { seq: 0, hash: AccountEntryHash::from_bytes([0xc0; 32]) },
            Cut::At { seq: 2, hash: AccountEntryHash::from_bytes([0x52; 32]) },
        ),
    );

    let h = f.fold();
    // g's op parks (one chain undecidable) — NOT effective.
    assert_eq!(
        h.outcome(&remove_d_by_g.into()),
        Some(Outcome::Parked(ParkReason::UnknownCutTarget)),
        "the op parks because one chain's cut is undecidable",
    );
    assert!(h.is_effective(&remove_d.into()), "the founder's remove is unaffected");
    let fact = h.roster_refs.get(&add_d.into()).expect("d has a roster fact");
    // DISCRIMINATOR: g parked, so it raised NEITHER register. d's control boundary stays at the
    // founder's Empty cut (Closed) — a commit-as-you-go join would have advanced it to
    // Cut{seq:0, hash:0xc0}.
    assert_eq!(
        fact.control_boundary,
        AuthorityBoundary::Closed,
        "a parked op must not advance the control register",
    );
    // And the secrets register stays at the founder's watermark too.
    assert_eq!(
        fact.secrets_boundary,
        AuthorityBoundary::Cut { seq: 0, hash: AccountEntryHash::from_bytes([0x50; 32]) },
        "a parked op must not advance the secrets register",
    );
    // Arrival-order-free: strata are content-derived, so the parked op raises no register under
    // any rotation (I9).
    for rot in 0..f.entries.len() {
        let rotated = f.fold_rotated(rot);
        assert_eq!(
            rotated.roster_refs.get(&add_d.into()).unwrap().control_boundary,
            AuthorityBoundary::Closed,
            "rotation {rot}: the parked op still raises no register",
        );
    }
}

#[test]
fn a_secrets_park_does_not_manufacture_a_contested_cycle() {
    // ORDERING invariant: an op that will PARK (its registers don't all join) must be excluded
    // from cycle-detection and the I2 last-owner simulation — it installs nothing, so it is not
    // an active cut and must not manufacture a mutual-condemnation cycle. A cuts D and D cuts A
    // at the same stratum (a control-chain mutual removal), but A's SECRETS cut is undecidable
    // against a pre-existing secrets register on D's chain, so A parks. With the park decided
    // BEFORE cycle-detection, A is excluded, no cycle exists, and D's removal of A takes effect
    // — the account stays Live. If the park were decided AFTER cycle-detection (the bug), A's
    // would-be control register would form an A↔D cycle → wrongly `contested`.
    //
    // Deeper nesting puts A and D at stratum 2 (so their removals are same-stratum) while F's
    // early remove of D — processed at stratum 0, BEFORE D is enrolled at stratum 1, hence
    // state-ineffective — installs the pre-existing `Device{log:1, D}` secrets register A's cut
    // is undecidable against, WITHOUT tombstoning D (its control cut names D's own later op, so
    // it condemns nothing D authors).
    let (fdr, p, s) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let (a, d) = (Dev::new(4), Dev::new(5));
    let mut f = Fixture::genesis(&fdr);
    let add_p = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&p, DeviceRole::Owner));
    f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&s, DeviceRole::Owner)); // spare owner
    // P (a depth-1 owner) mints A and D at depth 2 — so A's/D's own ops sit at stratum 2.
    let add_a = f.author(&p, Some(OwnerId::from_bytes(add_p)), &device_add(&a, DeviceRole::Owner));
    let add_d = f.author(&p, Some(OwnerId::from_bytes(add_p)), &device_add(&d, DeviceRole::Owner));
    // D removes A (a plain, valid mutual-removal partner: both cuts empty).
    let d_removes_a = f.author(
        &d,
        Some(OwnerId::from_bytes(add_d)),
        &device_remove_with_secrets(&a, Cut::Empty, Cut::Empty),
    );
    // F's early remove of D: at stratum 0 (before D is enrolled at stratum 1) it is
    // state-ineffective, but installs D's `Device{log:1}` secrets register (At{0}) and a
    // `Device{log:0}` control register whose cut names D's OWN op, so it condemns nothing D
    // does.
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove_with_secrets(
            &d,
            Cut::At { seq: 0, hash: AccountEntryHash::from_bytes(d_removes_a) },
            Cut::At { seq: 0, hash: AccountEntryHash::from_bytes([0x50; 32]) },
        ),
    );
    // A removes D: its control cut WOULD condemn D's op (the A→D cycle edge), but its secrets
    // cut is undecidable against the pre-existing At{0} secrets register (higher watermark
    // [0x52] not held) → A parks, and must NOT drive cycle-detection.
    let a_removes_d = f.author(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove_with_secrets(&d, Cut::Empty, Cut::At {
            seq: 2,
            hash: AccountEntryHash::from_bytes([0x52; 32]),
        }),
    );

    let h = f.fold();
    // DISCRIMINATOR: the parked op is excluded from cycle-detection, so the account is NOT
    // contested and D's removal of A takes effect. (With the park decided after cycle/I2, this
    // would be `Contested` and A would survive.)
    assert_eq!(
        h.classification(),
        AccountClassification::Live,
        "a parked op must not manufacture a contested cycle",
    );
    assert!(h.is_effective(&d_removes_a.into()), "D's valid removal of A takes effect");
    assert!(
        !h.is_effective(&a_removes_d.into()),
        "A's own removal never goes effective (it parked)"
    );
    assert!(
        matches!(
            h.owner_incarnation_effective(OwnerId::from_bytes(add_a), a.fp),
            AuthorityQuery::Invalid(_)
        ),
        "A's incarnation is closed by D's removal",
    );
    // Order-independent (I9).
    for rot in 0..f.entries.len() {
        assert_eq!(
            f.fold_rotated(rot).classification(),
            AccountClassification::Live,
            "rotation {rot}: still not contested",
        );
    }
}

#[test]
fn a_contested_extend_stratum_leaks_no_register_watermark() {
    // A contested stratum must leave the register set EXACTLY as the prior depth left it (§12
    // `state_before_depth`) — no half-applied cut may leak its watermark. Two same-depth
    // `CutExtend{Secrets}` raise D's ONE secrets register with equal-seq / different-hash
    // watermarks: the first joins `Applied`, the second is incomparable → contested. Because
    // the depth's register changes are STAGED and only merged at
    // end-of-non-contested-depth, the first extend's raised watermark never reaches the
    // real registers — D's secrets_boundary stays the founder's original cut. (With an
    // in-place join the first extend would mutate the real registers before the second
    // contests, leaking its watermark into `derive_authority_facts`.)
    let (fdr, p, d) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let (t8, t9) = (Dev::new(8), Dev::new(9));
    let mut f = Fixture::genesis(&fdr);
    // P is a depth-1 owner, so its extends sit at stratum 1 — AFTER the founder's stratum-0
    // remove installs D's secrets register (so the removal stays effective and D keeps a roster
    // fact whose secrets_boundary we can inspect on the contested fold).
    let add_p = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&p, DeviceRole::Owner));
    let add_d = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&d, DeviceRole::Owner));
    // D's secrets chain: s0, then an EQUIVOCATION at seq 1 (two distinct siblings off s0).
    let s0 = f.author_secrets_entry(&d, &d, 0, None);
    let e1a = f.author_secrets_entry(&d, &t8, 1, Some(AccountEntryHash::from_bytes(s0)));
    let e1b = f.author_secrets_entry(&d, &t9, 1, Some(AccountEntryHash::from_bytes(s0)));
    assert_ne!(e1a, e1b, "the two seq-1 secrets watermarks must be distinct");
    // Founder removes D (stratum 0, effective): installs D's secrets register at At{0, s0}.
    f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove_with_secrets(&d, Cut::Empty, Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes(s0),
        }),
    );
    // Two stratum-1 extends of D's secrets register to the two incomparable seq-1 watermarks.
    f.author(
        &p,
        Some(OwnerId::from_bytes(add_p)),
        &cut_extend_secrets(f.account_id, &d, None, 1, AccountEntryHash::from_bytes(e1a)),
    );
    f.author(
        &p,
        Some(OwnerId::from_bytes(add_p)),
        &cut_extend_secrets(f.account_id, &d, None, 1, AccountEntryHash::from_bytes(e1b)),
    );

    let h = f.fold();
    assert_eq!(
        h.classification(),
        AccountClassification::Contested { state_before_depth: 1 },
        "incomparable same-depth secrets extends fold contested at their stratum",
    );
    // DISCRIMINATOR: the contested stratum leaked NO watermark — D's secrets_boundary is still
    // the founder's original cut (At{0, s0}), NOT the first extend's raised At{1, e1a}.
    let fact =
        h.roster_refs.get(&add_d.into()).expect("D has a roster fact from the stratum-0 remove");
    assert_eq!(
        fact.secrets_boundary,
        AuthorityBoundary::Cut { seq: 0, hash: AccountEntryHash::from_bytes(s0) },
        "a contested extend stratum must not leak the first extend's watermark",
    );
    // Order-independent (I9): same verdict + same non-leaked boundary under any rotation.
    for rot in 0..f.entries.len() {
        let rotated = f.fold_rotated(rot);
        assert_eq!(
            rotated.classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "rotation {rot}: same contested verdict",
        );
        assert_eq!(
            rotated.roster_refs.get(&add_d.into()).unwrap().secrets_boundary,
            AuthorityBoundary::Cut { seq: 0, hash: AccountEntryHash::from_bytes(s0) },
            "rotation {rot}: still no leaked watermark",
        );
    }
}

#[test]
fn removing_one_of_several_owners_is_effective_and_keeps_the_rest() {
    // Basic I2 behavior (there was no behavioral last-owner test before this): with more than
    // one owner, removing one is EFFECTIVE and does not empty the owner set — the last-owner
    // guard only reserves when a removal WOULD empty it, and the intrinsic prefilter only fires
    // at `state.owners.len() == 1`. Guards against over-rejection by either.
    let (fdr, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let remove_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_remove(&b, Cut::Empty));

    let h = f.fold();
    assert_eq!(h.classification(), AccountClassification::Live);
    assert!(h.is_effective(&remove_b.into()), "removing one of several owners is effective");
    assert!(
        h.is_effective(&add_a.into()) && h.is_effective(&add_b.into()),
        "both devices were enrolled"
    );
    assert!(
        matches!(
            h.owner_incarnation_effective(OwnerId::from_bytes(add_b), b.fp),
            AuthorityQuery::Invalid(_)
        ),
        "B's owner incarnation is closed by the removal",
    );
    assert!(
        matches!(
            h.owner_incarnation_effective(OwnerId::from_bytes(add_a), a.fp),
            AuthorityQuery::Effective(_)
        ),
        "A's owner incarnation stays open — the owner set is not emptied",
    );
}

#[test]
fn an_owner_equivocated_self_removal_is_contested_deterministically() {
    // A signs TWO conflicting self-removals (incomparable `Device{A}` cuts) while a PEER owner
    // (the founder) remains — owner-key equivocation ⇒ the account folds `contested` (§12). The
    // incomparable-cut detection is order-free (symmetric), so the verdict is identical under
    // EVERY arrival order — there is no "vacuous-survivor lottery" where the sort decides which
    // op wins. (The concurrent removal of the founder never gets to matter.) This is the
    // multi-owner sibling of the intrinsic-prefilter case: with a peer owner present the
    // equivocating device is NOT the sole owner, so its self-removals are real cuts and their
    // equivocation is genuine compromise.
    let (fdr, a) = (Dev::new(1), Dev::new(2));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    // A removes the founder (A's seq 0), then equivocates its OWN removal at A's seq 1.
    let remove_f = f.author(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove(&fdr, Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(add_a) }),
    );
    let self_x = f.author_forked(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove(&a, Cut::At { seq: 5, hash: AccountEntryHash::from_bytes([0xc1; 32]) }),
        1,
        Some(AccountEntryHash::from_bytes(remove_f)),
    );
    let self_y = f.author_forked(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove(&a, Cut::At { seq: 5, hash: AccountEntryHash::from_bytes([0xc2; 32]) }),
        1,
        Some(AccountEntryHash::from_bytes(remove_f)),
    );
    let _ = (self_x, self_y);

    let h = f.fold();
    assert_eq!(
        h.classification(),
        AccountClassification::Contested { state_before_depth: 1 },
        "an owner's self-removal equivocation is owner-key compromise (§12)",
    );
    for rot in 0..f.entries.len() {
        assert_eq!(
            f.fold_rotated(rot).classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "rotation {rot}: deterministic — no vacuous-survivor lottery (I9)",
        );
    }
}

#[test]
fn an_op_citing_a_rejected_incarnation_is_stale_authority() {
    // The duplicate DeviceAdd(B) is a mint candidate but is REJECTED, so its incarnation never
    // becomes live. An op citing that (dead) incarnation is stale_authority -- authority is the
    // CITED incarnation, not "is the device an owner".
    let (founder, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&founder);
    f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let add_b2 =
        f.author(&founder, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    let op = f.author(&b, Some(OwnerId::from_bytes(add_b2)), &device_add(&c, DeviceRole::Member));
    let h = f.fold();
    assert_eq!(
        h.outcome(&op.into()),
        Some(Outcome::Rejected(RejectReason::StaleAuthority)),
        "an op under a rejected incarnation is stale_authority",
    );
}

#[test]
fn every_fold_outcome_has_a_stable_storage_taxonomy() {
    // §16.3 is persisted API, not display text. Pin every closed-enum token so adding or
    // renaming a fold reason cannot silently drift existing database rows or query behavior.
    let cases = [
        (Outcome::Effective { auth_epoch: 7 }, ("effective", None)),
        (Outcome::RetainedUnfolded, ("retained_unfolded", None)),
        (Outcome::Condemned(CondemnedReason::BeyondCut), ("condemned", Some("beyond_cut"))),
        (Outcome::Condemned(CondemnedReason::OffBranch), ("condemned", Some("off_branch"))),
        (
            Outcome::Condemned(CondemnedReason::ClosedIncarnation),
            ("condemned", Some("closed_incarnation")),
        ),
        (Outcome::Parked(ParkReason::UnknownOwnerRef), ("parked", Some("unknown_owner_ref"))),
        (Outcome::Parked(ParkReason::UnknownCutTarget), ("parked", Some("unknown_cut_target"))),
        (
            Outcome::Parked(ParkReason::IncompleteCutAncestry),
            ("parked", Some("incomplete_cut_ancestry")),
        ),
        (Outcome::Parked(ParkReason::ContestedSubject), ("parked", Some("contested_subject"))),
        (Outcome::Parked(ParkReason::AuthLenAhead), ("parked", Some("auth_len_ahead"))),
        (
            Outcome::Parked(ParkReason::DeferredStreamAuthorization),
            ("parked", Some("deferred_stream_authorization")),
        ),
        (Outcome::Rejected(RejectReason::StaleAuthority), ("rejected", Some("stale_authority"))),
        (Outcome::Rejected(RejectReason::GenesisSelfHash), ("rejected", Some("genesis_self_hash"))),
        (
            Outcome::Rejected(RejectReason::DuplicateGenesis),
            ("rejected", Some("duplicate_genesis")),
        ),
        (Outcome::Rejected(RejectReason::DuplicateAdd), ("rejected", Some("duplicate_add"))),
        (Outcome::Rejected(RejectReason::TombstoneReAdd), ("rejected", Some("tombstone_re_add"))),
        (Outcome::Rejected(RejectReason::BadPromote), ("rejected", Some("bad_promote"))),
        (Outcome::Rejected(RejectReason::LastOwner), ("rejected", Some("last_owner"))),
        (
            Outcome::Rejected(RejectReason::CutTargetMismatch),
            ("rejected", Some("cut_target_mismatch")),
        ),
        (Outcome::Rejected(RejectReason::WrongDevice), ("rejected", Some("wrong_device"))),
        (Outcome::Rejected(RejectReason::Malformed), ("rejected", Some("malformed"))),
        (
            Outcome::Rejected(RejectReason::NonGenesisOrigin),
            ("rejected", Some("non_genesis_origin")),
        ),
        (
            Outcome::Rejected(RejectReason::InvalidStreamSpec),
            ("rejected", Some("invalid_stream_spec")),
        ),
        (Outcome::Rejected(RejectReason::Ineffective), ("rejected", Some("ineffective"))),
    ];
    for (outcome, expected) in cases {
        let (status, detail) = outcome.taxonomy();
        assert_eq!((status.as_db_str(), detail), expected, "taxonomy drift for {outcome:?}");
        assert_eq!(EntryStatus::from_db_str(status.as_db_str()).unwrap(), status);
    }
}

#[test]
fn removing_the_sole_owner_is_rejected_last_owner_and_keeps_the_account_live() {
    // The INTRINSIC last-owner prefilter (§12/I2): when the prior-depth owner set is a
    // SINGLETON, a cut that closes that sole owner can never succeed under any order, so it is
    // reserved `LastOwner` before it can park, contest, or install a register — the account
    // stays `Live` with its one owner intact, never a permanently-unrecoverable zero-owner
    // state. This is the prefilter, not the multi-cut I2 sim: only one cut acts at the depth,
    // against a size-1 prior-depth owner set.
    //
    // A genuine singleton owner set needs a NON-founder sole owner — the founder's own self-cut
    // sits at stratum 0, where the prefilter is deliberately skipped (the prior-depth set is
    // empty there). X becomes sole once F is removed and Y is demoted, both at depth 1. Y is
    // demoted but its incarnation stays live WITHIN the demotion cut, so Y can still author an
    // admitted cut at depth 2 — where the prior-depth owner set is exactly {X}. Y removing X is
    // NOT self-defeating (the removal op lives on Y's chain, so X's watermark cannot cover it);
    // only the prefilter stops it.
    let (fdr, x, y) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let mut f = Fixture::genesis(&fdr);
    let add_x = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&x, DeviceRole::Owner));
    // X (depth-1 owner) mints Y as a deeper owner, removes the founder, and demotes Y with a
    // cut that COVERS Y's later removal op — so Y's incarnation stays live and its removal is
    // admitted (not condemned) at depth 2.
    let add_y = f.author(&x, Some(OwnerId::from_bytes(add_x)), &device_add(&y, DeviceRole::Owner)); // X seq 0
    let remove_x = f.author(&y, Some(OwnerId::from_bytes(add_y)), &device_remove(&x, Cut::Empty)); // Y seq 0 (depth 2)
    f.author(
        &x,
        Some(OwnerId::from_bytes(add_x)),
        &owner_demote(&y, OwnerId::from_bytes(add_y), Cut::At {
            seq: 0,
            hash: AccountEntryHash::from_bytes(remove_x),
        }),
    ); // X seq 1
    f.author(
        &x,
        Some(OwnerId::from_bytes(add_x)),
        &device_remove(&fdr, Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(add_x) }),
    ); // X seq 2

    let h = f.fold();
    assert_eq!(
        h.outcome(&remove_x.into()),
        Some(Outcome::Rejected(RejectReason::LastOwner)),
        "removing the sole remaining owner is reserved LastOwner by the intrinsic prefilter",
    );
    assert_eq!(h.classification(), AccountClassification::Live, "the account stays live");
    assert!(!h.is_effective(&remove_x.into()), "the sole-owner removal does not fold");
    assert!(
        matches!(
            h.owner_incarnation_effective(OwnerId::from_bytes(add_x), x.fp),
            AuthorityQuery::Effective(_)
        ),
        "X's owner incarnation stays open — the owner set is never emptied",
    );
    // Arrival order cannot change the verdict (I9): the intrinsic prefilter is order-free.
    for rot in 0..f.entries.len() {
        let r = f.fold_rotated(rot);
        assert_eq!(r.classification(), AccountClassification::Live, "rotation {rot} class");
        assert_eq!(
            r.outcome(&remove_x.into()),
            Some(Outcome::Rejected(RejectReason::LastOwner)),
            "rotation {rot}: still reserved LastOwner",
        );
        assert!(
            matches!(
                r.owner_incarnation_effective(OwnerId::from_bytes(add_x), x.fp),
                AuthorityQuery::Effective(_)
            ),
            "rotation {rot}: X stays the sole open owner",
        );
    }
}

#[test]
fn last_of_two_same_depth_owner_removals_is_reserved_order_independently() {
    // The multi-cut I2 simulation (§12/I2): across ALL same-depth admitted cuts, the removals
    // are simulated in deterministic (hash) order over the prior-depth owner set, and any cut
    // that would empty it is reserved `LastOwner`. Here the prior-depth owner set at depth 2 is
    // exactly {A, B}; a demoted-but-live owner C authors TWO same-depth cuts, one closing A and
    // one closing B (on independent target chains, so no mutual-condemnation cycle — the fold
    // stays Live, not Contested). The first effective removal shrinks the set to a singleton;
    // the second is reserved LastOwner, leaving one owner open. The reserved survivor is chosen
    // by op hash, so it is identical under every arrival order (I9) — never a
    // vacuous-survivor lottery decided by the sort.
    let (fdr, a, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
    let mut f = Fixture::genesis(&fdr);
    let add_a = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&a, DeviceRole::Owner));
    // A (depth-1 owner) mints B and C as deeper owners, then removes F and demotes C — leaving
    // the prior-depth owner set at depth 2 exactly {A, B}. C's demotion cut COVERS both of C's
    // later removals, so C's incarnation stays live and both cuts are admitted at depth 2.
    let add_b = f.author(&a, Some(OwnerId::from_bytes(add_a)), &device_add(&b, DeviceRole::Owner)); // A seq 0
    let add_c = f.author(&a, Some(OwnerId::from_bytes(add_a)), &device_add(&c, DeviceRole::Owner)); // A seq 1
    let remove_a = f.author(&c, Some(OwnerId::from_bytes(add_c)), &device_remove(&a, Cut::Empty)); // C seq 0 (depth 2)
    let remove_b = f.author(&c, Some(OwnerId::from_bytes(add_c)), &device_remove(&b, Cut::Empty)); // C seq 1 (depth 2)
    f.author(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &owner_demote(&c, OwnerId::from_bytes(add_c), Cut::At {
            seq: 1,
            hash: AccountEntryHash::from_bytes(remove_b),
        }),
    ); // A seq 2
    f.author(
        &a,
        Some(OwnerId::from_bytes(add_a)),
        &device_remove(&fdr, Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(add_a) }),
    ); // A seq 3

    let h = f.fold();
    assert_eq!(h.classification(), AccountClassification::Live, "the standoff stays live");
    // Exactly one of the two same-depth removals folds; the other is reserved LastOwner.
    let a_removed = h.is_effective(&remove_a.into());
    let b_removed = h.is_effective(&remove_b.into());
    assert!(a_removed ^ b_removed, "exactly one owner removal folds — the other is reserved");
    let (effective, reserved) = if a_removed { (remove_a, remove_b) } else { (remove_b, remove_a) };
    assert!(h.is_effective(&effective.into()), "the first-in-order removal is effective");
    assert_eq!(
        h.outcome(&reserved.into()),
        Some(Outcome::Rejected(RejectReason::LastOwner)),
        "the removal that would empty the owner set is reserved LastOwner",
    );
    // One owner incarnation stays open (the reserved survivor); the other is closed.
    let a_open = matches!(
        h.owner_incarnation_effective(OwnerId::from_bytes(add_a), a.fp),
        AuthorityQuery::Effective(_)
    );
    let b_open = matches!(
        h.owner_incarnation_effective(OwnerId::from_bytes(add_b), b.fp),
        AuthorityQuery::Effective(_)
    );
    assert!(a_open ^ b_open, "exactly one owner incarnation stays open");
    // Arrival order (I9): the SAME reserved survivor and verdict under every rotation.
    for rot in 0..f.entries.len() {
        let r = f.fold_rotated(rot);
        assert_eq!(r.classification(), AccountClassification::Live, "rotation {rot} class");
        assert_eq!(
            r.outcome(&reserved.into()),
            Some(Outcome::Rejected(RejectReason::LastOwner)),
            "rotation {rot}: the same removal is reserved LastOwner",
        );
        assert!(r.is_effective(&effective.into()), "rotation {rot}: the same removal folds");
    }
}

#[test]
fn a_removed_owners_within_cut_grant_survives_no_cascade_i5() {
    // I5 no-cascade (§20): removing a legitimate owner bounds its valid prefix through the cut,
    // it does NOT retroactively undo the authorizations that owner issued WITHIN the cut — only
    // an explicit revoke closes a grant. Founder F adds owner B; B authors a StreamOwn and a
    // StreamGrant (its within-cut authorizations) then one more op BEYOND the cut. F removes B,
    // pinning the control cut AT the grant's seq: the own + grant stay effective (the grant
    // stays queryable authority), while B's beyond-cut op is condemned. The removal bounds the
    // prefix; it does not cascade to the grant.
    let (fdr, b, x) = (Dev::new(1), Dev::new(2), Dev::new(3));
    let grantee = AccountId::from_bytes([0x44; 32]);
    let mut f = Fixture::genesis(&fdr);
    let add_b = f.author(&fdr, Some(f.genesis_hash.into()), &device_add(&b, DeviceRole::Owner));
    // B's control chain (per-device seq numbering starts at 0 on B's own chain — add_b lives on
    // F's chain, not B's): StreamOwn (seq 0), StreamGrant (seq 1), a member add BEYOND the cut
    // (seq 2).
    let (stream, own_op) = test_support::stream_own_public(f.account_id);
    let own = f.author(&b, Some(OwnerId::from_bytes(add_b)), &own_op); // B seq 0
    let grant = f.author(&b, Some(OwnerId::from_bytes(add_b)), &stream_grant(stream, grantee)); // B seq 1
    let beyond =
        f.author(&b, Some(OwnerId::from_bytes(add_b)), &device_add(&x, DeviceRole::Member)); // B seq 2
    // F removes B with the valid prefix pinned AT the grant (B seq 1) — own + grant are within.
    let remove_b = f.author(
        &fdr,
        Some(f.genesis_hash.into()),
        &device_remove(&b, Cut::At { seq: 1, hash: AccountEntryHash::from_bytes(grant) }),
    );

    let h = f.fold();
    assert!(h.is_effective(&remove_b.into()), "the removal of B is effective");
    assert!(
        h.is_effective(&own.into()),
        "B's within-cut StreamOwn survives the removal (no cascade)"
    );
    assert!(
        h.is_effective(&grant.into()),
        "B's within-cut StreamGrant survives the removal (no cascade)",
    );
    assert_eq!(
        h.grant_effective(GrantId::from_bytes(grant), stream, grantee),
        AuthorityQuery::Effective(GrantAuthority {
            stream_id: stream,
            grantee_account_id: grantee,
            role: GrantRole::Reader,
        }),
        "the grant stays queryable authority after its author is removed",
    );
    assert_eq!(
        h.outcome(&beyond.into()),
        Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        "B's op beyond the cut is condemned — the prefix is bounded, the within-cut grant is not",
    );
    // Arrival order (I9): same no-cascade result under every rotation.
    for rot in 0..f.entries.len() {
        let r = f.fold_rotated(rot);
        assert!(r.is_effective(&grant.into()), "rotation {rot}: grant survives");
        assert_eq!(
            r.grant_effective(GrantId::from_bytes(grant), stream, grantee),
            AuthorityQuery::Effective(GrantAuthority {
                stream_id: stream,
                grantee_account_id: grantee,
                role: GrantRole::Reader,
            }),
            "rotation {rot}: grant stays effective",
        );
        assert_eq!(
            r.outcome(&beyond.into()),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
            "rotation {rot}: beyond-cut op condemned",
        );
    }
}
