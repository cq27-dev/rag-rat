//! The stratified control-log fold (§11) — total, convergent, laundering-proof, account-scoped.
//!
//! `fold_account` is a PURE function of the candidate set (all sharing one `account_id`): it
//! derives every entry's classification from content-addressed CITATIONS, never from arrival/fold
//! order (I9). The structure is a well-founded recursion on incarnation DEPTH: an op's
//! `authority_ref` cites an EARLIER-hashed owner-incarnation mint (L1), so the incarnation-citation
//! graph is a DAG grounded at `AccountGenesis` — depth strata are finite and processed in order, a
//! decision at depth `d` uses only final depth-`<d` results plus same-depth registers, and
//! `condemned` only grows (no oscillation). This stage builds the depth/candidate spine + the
//! effect pass; cut ops (registers, condemnation, `contested`) land in the next stage.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::envelope::{AccountEntryHeader, VerifiedAccountEntry};
use super::id::account_id_from_genesis_payload;
use super::ops::{self, AccountOp, DecodedAccountOp, DeviceRole};
use crate::oplog::op::DeviceFingerprint;

/// The per-entry classification (§16.3 taxonomy). `RetainedUnfolded` is an unknown `entry_type`;
/// `Rejected` will never be effective; `Parked` is undecided pending more entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Effective { auth_epoch: u64 },
    Rejected(RejectReason),
    Parked(ParkReason),
    RetainedUnfolded,
}

impl Outcome {
    pub(super) fn is_effective(&self) -> bool {
        matches!(self, Outcome::Effective { .. })
    }
}

/// A control op that will never be effective — a permanent state precondition failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RejectReason {
    /// The author's cited incarnation is not live (e.g. laundered, or cross-account — P3).
    StaleAuthority,
    /// The genesis payload does not hash to the header's `account_id` (§4 self-hash).
    GenesisSelfHash,
    /// A second `AccountGenesis` in the account.
    DuplicateGenesis,
    /// `DeviceAdd` for a device already enrolled.
    DuplicateAdd,
    /// `DeviceAdd` / re-enroll for a tombstoned fingerprint (I4).
    TombstoneReAdd,
    /// `OwnerPromote` of a non-enrolled / already-owner / tombstoned device.
    BadPromote,
    /// A duplicate / no-op / self-referential op with no effect.
    Ineffective,
}

/// A control op undecided until more entries arrive (a *withheld* input parks, never flips — I11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParkReason {
    /// The cited `authority_ref` owner-incarnation is not resolvable in this account.
    UnknownOwnerRef,
}

/// The account's classification after folding (§12): `Live`, or `Contested` (owner-key compromise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountClassification {
    Live,
    Contested { state_before_depth: usize },
}

/// The derived authority history of one account: per-entry outcomes + the account classification.
/// (Registers land with the cut-op stage.)
pub(super) struct AccountAuthHistory {
    outcomes: HashMap<[u8; 32], Outcome>,
    classification: AccountClassification,
}

impl AccountAuthHistory {
    /// The outcome of the entry with this hash (absent ⇒ the entry was not in the folded set).
    pub(super) fn outcome(&self, entry_hash: &[u8; 32]) -> Option<Outcome> {
        self.outcomes.get(entry_hash).copied()
    }

    pub(super) fn classification(&self) -> AccountClassification {
        self.classification
    }

    fn is_effective(&self, entry_hash: &[u8; 32]) -> bool {
        self.outcome(entry_hash).is_some_and(|o| o.is_effective())
    }
}

/// A structurally-valid, signature-valid candidate the fold considers: the verified entry + its
/// decoded KNOWN op. (Unknown ops classify `RetainedUnfolded` and are never folded.)
struct Candidate {
    entry: VerifiedAccountEntry,
    op: AccountOp,
}

impl Candidate {
    fn hash(&self) -> [u8; 32] {
        self.entry.entry_hash
    }

    fn header(&self) -> &AccountEntryHeader {
        &self.entry.header
    }

    /// Whether this op MINTS an owner incarnation (its `entry_hash` becomes the `owner_id`).
    fn is_mint(&self) -> bool {
        matches!(
            self.op,
            AccountOp::AccountGenesis { .. }
                | AccountOp::DeviceAdd { role: DeviceRole::Owner, .. }
                | AccountOp::OwnerPromote { .. }
        )
    }

    /// The device this op names as its SUBJECT (for a mint, the device the incarnation is for).
    fn subject_device(&self) -> DeviceFingerprint {
        match &self.op {
            // Genesis's owner is the founder that authored it.
            AccountOp::AccountGenesis { .. } => self.header().device_fingerprint,
            AccountOp::DeviceAdd { device_fingerprint, .. }
            | AccountOp::OwnerPromote { device_fingerprint }
            | AccountOp::DeviceRemove { device_fingerprint, .. }
            | AccountOp::OwnerDemote { device_fingerprint, .. } => *device_fingerprint,
            _ => self.header().device_fingerprint,
        }
    }
}

/// The incarnation resolver over the candidate set — keyed by `owner_id` (a mint's `entry_hash`),
/// account-local by construction (the map only holds THIS account's mints).
struct Incarnations<'a> {
    /// owner_id -> the mint candidate.
    mints: HashMap<[u8; 32], &'a Candidate>,
    genesis_owner_id: [u8; 32],
    /// Memoized structural depth per owner_id.
    depth: HashMap<[u8; 32], Option<usize>>,
}

impl<'a> Incarnations<'a> {
    fn build(candidates: &'a [Candidate], genesis_owner_id: [u8; 32]) -> Self {
        let mints = candidates.iter().filter(|c| c.is_mint()).map(|c| (c.hash(), c)).collect();
        Incarnations { mints, genesis_owner_id, depth: HashMap::new() }
    }

    /// Resolve an `owner_id` to its mint candidate (account-local).
    fn candidate(&self, owner_id: &[u8; 32]) -> Option<&'a Candidate> {
        self.mints.get(owner_id).copied()
    }

    /// The structural depth of an incarnation: genesis = 0; else 1 + the depth of the incarnation
    /// the minting op's author cited. `None` if the citation chain is unresolvable in this
    /// account (cross-account — P3 — or not yet synced). Memoized with an in-progress guard
    /// (the DAG cannot cycle — L1 — but a corrupt set must not loop).
    fn incarnation_depth(&mut self, owner_id: [u8; 32]) -> Option<usize> {
        if let Some(cached) = self.depth.get(&owner_id) {
            return *cached;
        }
        // Mark in-progress as unresolved to break any (impossible) cycle.
        self.depth.insert(owner_id, None);
        let mint = self.candidate(&owner_id)?;
        let computed = if owner_id == self.genesis_owner_id {
            Some(0)
        } else {
            let parent = mint.header().authority_ref?;
            self.incarnation_depth(parent).map(|d| d + 1)
        };
        self.depth.insert(owner_id, computed);
        computed
    }

    /// The incarnation `e` acts under: genesis acts under its OWN incarnation; else the cited
    /// `authority_ref`.
    fn author_incarnation_id(&self, e: &Candidate) -> Option<[u8; 32]> {
        match e.op {
            AccountOp::AccountGenesis { .. } => Some(e.hash()),
            _ => e.header().authority_ref,
        }
    }

    /// The depth of the stratum `e` belongs to = the depth of its author-incarnation. `None` when
    /// the citation is unresolvable in this account.
    fn author_depth(&mut self, e: &Candidate) -> Option<usize> {
        let inc = self.author_incarnation_id(e)?;
        self.incarnation_depth(inc)
    }
}

/// Mutable roster/state threaded through the effect pass — the STATE preconditions read + update
/// it.
#[derive(Default)]
struct FoldState {
    /// Incarnations proven live so far (owner_ids). Seeded with the genesis incarnation.
    live: HashSet<[u8; 32]>,
    /// Devices currently enrolled.
    roster: HashSet<DeviceFingerprint>,
    /// Devices that currently hold an open owner incarnation.
    owners: HashSet<DeviceFingerprint>,
    /// Removed devices — never re-enroll (I4). (Populated by the cut-op stage.)
    tombstoned: HashSet<DeviceFingerprint>,
    /// Streams with an effective `StreamOwn`.
    streams_owned: HashSet<[u8; 32]>,
    /// Whether an `AccountGenesis` has been made effective.
    genesis_seen: bool,
    /// 0-based effective index assigned as `auth_epoch`.
    next_auth_epoch: u64,
}

/// Fold one account's control-log candidates into their derived classification (§11). All entries
/// MUST share `account_id` (the caller groups by account). Order-independent: the result is
/// identical under every permutation of `entries`.
pub(super) fn fold_account(entries: &[VerifiedAccountEntry]) -> AccountAuthHistory {
    let mut outcomes: HashMap<[u8; 32], Outcome> = HashMap::new();

    // Decode ops; an unknown entry_type is retained-unfolded and never participates in the fold.
    let mut candidates: Vec<Candidate> = Vec::with_capacity(entries.len());
    for entry in entries {
        match ops::decode(entry.header.entry_type, &entry.payload) {
            Ok(DecodedAccountOp::Known(op)) => {
                candidates.push(Candidate { entry: entry.clone(), op });
            },
            // A structurally-valid-but-unknown op (decode already ran at ingest); retain it
            // unfolded.
            Ok(DecodedAccountOp::Unknown { .. }) | Err(_) => {
                outcomes.insert(entry.entry_hash, Outcome::RetainedUnfolded);
            },
        }
    }

    // The genesis anchors the account: the mint whose payload hashes to the shared account_id.
    let Some(genesis) = find_genesis(&candidates) else {
        // No valid genesis yet — nothing can be authorized; everything parks on the missing root.
        for c in &candidates {
            outcomes.insert(c.hash(), Outcome::Parked(ParkReason::UnknownOwnerRef));
        }
        return AccountAuthHistory { outcomes, classification: AccountClassification::Live };
    };
    let genesis_owner_id = genesis.hash();

    let mut incarnations = Incarnations::build(&candidates, genesis_owner_id);

    // Group resolvable candidates by author-depth; unresolvable citations park.
    let mut strata: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (idx, c) in candidates.iter().enumerate() {
        match incarnations.author_depth(c) {
            Some(d) => strata.entry(d).or_default().push(idx),
            None => {
                outcomes.insert(c.hash(), Outcome::Parked(ParkReason::UnknownOwnerRef));
            },
        }
    }

    let mut state = FoldState { live: HashSet::from([genesis_owner_id]), ..Default::default() };

    for (_depth, idxs) in strata {
        // (b) EFFECT PASS over the stratum in (chain, seq) order — deterministic, order-free (I9).
        let mut ordered = idxs;
        ordered.sort_by_key(|&i| {
            let h = candidates[i].header();
            (h.device_fingerprint.to_bytes(), h.seq)
        });
        for i in ordered {
            let c = &candidates[i];
            let outcome = classify_effect(c, &incarnations, &state);
            if let Outcome::Effective { .. } = outcome {
                apply_effect(c, &mut state);
            }
            outcomes.insert(c.hash(), outcome);
        }
    }

    AccountAuthHistory { outcomes, classification: AccountClassification::Live }
}

/// The genesis candidate: an `AccountGenesis` whose payload hashes to the shared `account_id` (§4).
fn find_genesis(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates.iter().find(|c| {
        matches!(c.op, AccountOp::AccountGenesis { .. })
            && account_id_from_genesis_payload(&c.entry.payload) == c.header().account_id
    })
}

/// Classify one op in the effect pass: liveness of its author-incarnation, then the state
/// preconditions. Does NOT mutate state (that is [`apply_effect`], only on an effective verdict).
fn classify_effect(c: &Candidate, incarnations: &Incarnations<'_>, state: &FoldState) -> Outcome {
    // The author must act under a LIVE incarnation (defeats laundering — a cut authored under a
    // since-condemned owner never gets here because that incarnation is not in `live`).
    let Some(author_inc) = incarnations.author_incarnation_id(c) else {
        return Outcome::Parked(ParkReason::UnknownOwnerRef);
    };
    if !state.live.contains(&author_inc) {
        return Outcome::Rejected(RejectReason::StaleAuthority);
    }
    match &c.op {
        AccountOp::AccountGenesis { .. } => {
            if state.genesis_seen {
                return Outcome::Rejected(RejectReason::DuplicateGenesis);
            }
            // The self-hash was checked in `find_genesis`; a second genesis reaching here is a dup.
            if account_id_from_genesis_payload(&c.entry.payload) != c.header().account_id {
                return Outcome::Rejected(RejectReason::GenesisSelfHash);
            }
            effective(state)
        },
        AccountOp::DeviceAdd { device_fingerprint, .. } => {
            if state.tombstoned.contains(device_fingerprint) {
                Outcome::Rejected(RejectReason::TombstoneReAdd)
            } else if state.roster.contains(device_fingerprint) {
                Outcome::Rejected(RejectReason::DuplicateAdd)
            } else {
                effective(state)
            }
        },
        AccountOp::OwnerPromote { device_fingerprint } => {
            let enrolled = state.roster.contains(device_fingerprint);
            let already_owner = state.owners.contains(device_fingerprint);
            let tombstoned = state.tombstoned.contains(device_fingerprint);
            if enrolled && !already_owner && !tombstoned {
                effective(state)
            } else {
                Outcome::Rejected(RejectReason::BadPromote)
            }
        },
        // Cut ops + stream ops: full handling (registers, StreamOwn-before-grant, I2 last-owner)
        // lands in the next stage. For now an op whose author-incarnation is live is provisionally
        // effective so the depth/effect spine can be tested against non-cut traces.
        _ => effective(state),
    }
}

/// Mint an effective verdict, consuming the next `auth_epoch`.
fn effective(state: &FoldState) -> Outcome {
    Outcome::Effective { auth_epoch: state.next_auth_epoch }
}

/// Apply an EFFECTIVE op's roster/live effect (called only after an effective verdict).
fn apply_effect(c: &Candidate, state: &mut FoldState) {
    state.next_auth_epoch += 1;
    match &c.op {
        AccountOp::AccountGenesis { .. } => {
            state.genesis_seen = true;
            state.roster.insert(c.subject_device());
            state.owners.insert(c.subject_device());
            state.live.insert(c.hash());
        },
        AccountOp::DeviceAdd { device_fingerprint, role, .. } => {
            state.roster.insert(*device_fingerprint);
            if *role == DeviceRole::Owner {
                state.owners.insert(*device_fingerprint);
                state.live.insert(c.hash());
            }
        },
        AccountOp::OwnerPromote { device_fingerprint } => {
            state.owners.insert(*device_fingerprint);
            state.live.insert(c.hash());
        },
        AccountOp::StreamOwn { stream_id, .. } => {
            state.streams_owned.insert(stream_id.to_bytes());
        },
        _ => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::account::AccountId;
    use crate::oplog::account::envelope::{sign_account_entry, verify_account_signed};
    use crate::oplog::account::ops::{encode, entry_type, entry_type_of};
    use crate::oplog::device::{DeviceSecret, DeviceX25519Secret};

    /// A seed-deterministic test device: its ed25519 signer + fingerprint + the pubkeys a
    /// Genesis/DeviceAdd op carries.
    struct Dev {
        secret: DeviceSecret,
        fp: DeviceFingerprint,
        ed: [u8; 32],
        x: [u8; 32],
    }

    impl Dev {
        fn new(seed: u8) -> Self {
            let secret = DeviceSecret::from_seed(&[seed; 32]);
            let public = secret.public();
            let x =
                DeviceX25519Secret::from_seed(&[seed.wrapping_add(0x80); 32]).public().to_bytes();
            Dev { fp: public.fingerprint(), ed: public.to_bytes(), x, secret }
        }
    }

    /// Authors a real signed account (genesis + arbitrary ops) so the fold runs over verified
    /// entries. Tests control each op's (author, authority_ref, payload) to build exact traces; the
    /// harness threads the per-device seq/prev chains.
    struct Fixture {
        account_id: AccountId,
        genesis_hash: [u8; 32],
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
            let payload = encode(&op).unwrap();
            let account_id = account_id_from_genesis_payload(&payload);
            let header = AccountEntryHeader {
                account_id,
                log_id: 0,
                device_fingerprint: founder.fp,
                seq: 0,
                prev_hash: None,
                parent_ref: None,
                entry_type: entry_type::ACCOUNT_GENESIS,
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
            fixture.chains.insert(founder.fp.to_bytes(), (1, Some(genesis_hash)));
            fixture.entries.push(verified);
            fixture
        }

        /// Author `op` as `author` citing `authority_ref`. Returns the entry_hash.
        fn author(
            &mut self,
            author: &Dev,
            authority_ref: Option<[u8; 32]>,
            op: &AccountOp,
        ) -> [u8; 32] {
            let payload = encode(op).unwrap();
            let (seq, prev) = self.chains.get(&author.fp.to_bytes()).copied().unwrap_or((0, None));
            let header = AccountEntryHeader {
                account_id: self.account_id,
                log_id: 0,
                device_fingerprint: author.fp,
                seq,
                prev_hash: prev,
                parent_ref: Some(self.genesis_hash),
                entry_type: entry_type_of(op),
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
            self.chains.insert(author.fp.to_bytes(), (seq + 1, Some(hash)));
            self.entries.push(verified);
            hash
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

        fn effective_set(history: &AccountAuthHistory) -> HashSet<[u8; 32]> {
            history.outcomes.iter().filter(|(_, o)| o.is_effective()).map(|(h, _)| *h).collect()
        }
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

    #[test]
    fn genesis_is_effective() {
        let founder = Dev::new(1);
        let f = Fixture::genesis(&founder);
        let h = f.fold();
        assert!(h.is_effective(&f.genesis_hash), "the genesis is effective");
        assert_eq!(h.classification(), AccountClassification::Live);
    }

    #[test]
    fn depth_ordering_admits_an_op_by_a_device_the_founder_added() {
        // genesis (founder, depth-0 owner) -> founder adds B as owner (depth 0) -> B adds C (depth
        // 1). C's add is effective ONLY because B's incarnation was made live at the
        // shallower depth.
        let (founder, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let mut f = Fixture::genesis(&founder);
        let add_b = f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let add_c = f.author(&b, Some(add_b), &device_add(&c, DeviceRole::Member));
        let h = f.fold();
        assert!(h.is_effective(&f.genesis_hash));
        assert!(h.is_effective(&add_b), "founder's DeviceAdd(B, owner) is effective");
        assert!(h.is_effective(&add_c), "B's DeviceAdd(C) at depth 1 is effective");
    }

    #[test]
    fn arrival_order_does_not_change_the_result_p9() {
        let (founder, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let mut f = Fixture::genesis(&founder);
        let add_b = f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        f.author(&b, Some(add_b), &device_add(&c, DeviceRole::Member));
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
        let op = f.author(&d, Some(foreign_incarnation), &device_add(&e, DeviceRole::Member));
        let h = f.fold();
        assert!(!h.is_effective(&op), "cross-account citation is not admitted (P3)");
        assert_eq!(h.outcome(&op), Some(Outcome::Parked(ParkReason::UnknownOwnerRef)));
    }

    #[test]
    fn duplicate_device_add_is_rejected_p11() {
        let (founder, b) = (Dev::new(1), Dev::new(2));
        let mut f = Fixture::genesis(&founder);
        let add_b1 = f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let add_b2 = f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let h = f.fold();
        assert!(h.is_effective(&add_b1), "the first DeviceAdd(B) is effective");
        assert_eq!(
            h.outcome(&add_b2),
            Some(Outcome::Rejected(RejectReason::DuplicateAdd)),
            "the duplicate DeviceAdd(B) is ineffective",
        );
    }

    #[test]
    fn an_op_citing_a_rejected_incarnation_is_stale_authority() {
        // The duplicate DeviceAdd(B) is a mint candidate but is REJECTED, so its incarnation never
        // becomes live. An op citing that (dead) incarnation is stale_authority -- authority is the
        // CITED incarnation, not "is the device an owner".
        let (founder, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let mut f = Fixture::genesis(&founder);
        f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let add_b2 = f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let op = f.author(&b, Some(add_b2), &device_add(&c, DeviceRole::Member));
        let h = f.fold();
        assert_eq!(
            h.outcome(&op),
            Some(Outcome::Rejected(RejectReason::StaleAuthority)),
            "an op under a rejected incarnation is stale_authority",
        );
    }
}
