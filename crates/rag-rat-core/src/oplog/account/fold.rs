//! The stratified control-log fold (§11) — total, convergent, laundering-proof, account-scoped.
//!
//! `fold_account` is a PURE function of the candidate set (all sharing one `account_id`): it
//! derives every entry's classification from content-addressed CITATIONS, never from arrival/fold
//! order (I9). The structure is a well-founded recursion on incarnation DEPTH: an op's
//! `authority_ref` cites an EARLIER-hashed owner-incarnation mint (L1), so the incarnation-citation
//! graph is a DAG grounded at `AccountGenesis` — depth strata are finite and processed in order, a
//! decision at depth `d` uses only final depth-`<d` results plus same-depth registers, and
//! `condemned` only grows (no oscillation). Each depth's live cut ops install revocation registers
//! (after cut-target binding + the I2 last-owner guard); a register condemns entries beyond its cut
//! (seq-only, I11) or off the accepted branch (L2), and parks an under-cut prefix whose watermark
//! is still withheld. A same-depth mutual owner-condemnation cycle or an incomparable-cut register
//! is genuine owner-key compromise ⇒ the account folds `contested` and halts authority mutation
//! (§12).

use std::collections::{BTreeMap, HashMap, HashSet};

use super::AccountId;
use super::candidate::{self, Ancestry, CutCoordinate, HeaderView, JoinResult, UnknownCause};
use super::cut::{Cut, beyond};
use super::envelope::{AccountEntryHeader, VerifiedAccountEntry};
use super::id::account_id_from_genesis_payload;
use super::ops::{self, AccountOp, ChainKind, DecodedAccountOp, DeviceRole};
use super::registers::RegisterKey;
use crate::oplog::op::DeviceFingerprint;

/// The per-entry classification (§16.3 taxonomy). `RetainedUnfolded` is an unknown `entry_type`;
/// `Rejected` will never be effective; `Parked` is undecided pending more entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Effective { auth_epoch: u64 },
    Condemned(CondemnedReason),
    Rejected(RejectReason),
    Parked(ParkReason),
    RetainedUnfolded,
}

impl Outcome {
    pub(super) fn is_effective(&self) -> bool {
        matches!(self, Outcome::Effective { .. })
    }
}

/// Why an entry was killed by a revocation register (§11.2). `BeyondCut` is seq-only (I11) and
/// RECOVERABLE (a later `CutExtend` re-blesses); `OffBranch` is the permanent equivocation-loser
/// class (L2); `ClosedIncarnation` is a mint whose own authorizing incarnation was condemned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CondemnedReason {
    BeyondCut,
    OffBranch,
    ClosedIncarnation,
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
    /// A cut op whose effect would close the LAST open owner incarnation (I2).
    LastOwner,
    /// A cut whose watermark names a different `(scope, log, device, seq)` than its register
    /// (§11.3).
    CutTargetMismatch,
    /// The cited incarnation's mint does not name the SIGNING device (an owner op is admissible
    /// only when its `authority_ref` resolves to a mint for the author — §"authority rule",
    /// clause 1), or an `OwnerDemote`'s `owner_id` names a mint minted for a different device
    /// than its subject.
    WrongDevice,
    /// A `StreamGrant` / `StreamRevoke` for a stream with no effective `StreamOwn` (P11).
    UnownedStream,
    /// A duplicate / no-op / self-referential op with no effect.
    Ineffective,
}

/// A control op undecided until more entries arrive (a *withheld* input parks, never flips — I11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParkReason {
    /// The cited `authority_ref` owner-incarnation is not resolvable in this account.
    UnknownOwnerRef,
    /// A cut's watermark entry is not held yet, so an under-cut entry can't be placed on/off the
    /// accepted branch (§11.3, I11 — a withheld watermark parks, never flips a verdict).
    UnknownCutTarget,
    /// A link on the walk from a cut's watermark toward the entry is missing (I11).
    IncompleteCutAncestry,
    /// The entry's author is a subject of a residue cut op in a `contested` account (§12) — parked,
    /// quota-bounded, reclassified if the account recovers.
    ContestedSubject,
}

/// The account's classification after folding (§12): `Live`, or `Contested` (owner-key compromise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountClassification {
    Live,
    Contested { state_before_depth: usize },
}

/// The derived authority history of one account: per-entry outcomes, the account classification,
/// and (only when `contested`) the deterministic recovery successor.
pub(super) struct AccountAuthHistory {
    outcomes: HashMap<[u8; 32], Outcome>,
    classification: AccountClassification,
    /// In a `contested` account, the deterministic `AccountReRoot` successor a subscriber follows
    /// — the smallest `successor_account_id` by byte order among the admitted re-roots (§12).
    /// `None` when the account is `Live` or no pre-contest owner has re-rooted yet.
    contested_successor: Option<AccountId>,
}

impl AccountAuthHistory {
    /// The outcome of the entry with this hash (absent ⇒ the entry was not in the folded set).
    pub(super) fn outcome(&self, entry_hash: &[u8; 32]) -> Option<Outcome> {
        self.outcomes.get(entry_hash).copied()
    }

    pub(super) fn classification(&self) -> AccountClassification {
        self.classification
    }

    /// The deterministic recovery successor for a `contested` account (§12), if one exists.
    pub(super) fn contested_successor(&self) -> Option<AccountId> {
        self.contested_successor
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

/// A hash-keyed [`HeaderView`] over the candidate set — the ancestry walk + cut-target binding read
/// headers through this without owning the fold's storage.
struct CandidateView<'a> {
    candidates: &'a [Candidate],
    by_hash: &'a HashMap<[u8; 32], usize>,
}

impl HeaderView for CandidateView<'_> {
    fn header(&self, entry_hash: &[u8; 32]) -> Option<&AccountEntryHeader> {
        self.by_hash.get(entry_hash).map(|&i| self.candidates[i].header())
    }
}

/// The CONTROL-log register a cut op installs (§11): `DeviceRemove` → a device-level register on
/// the removed device's control chain (scopes the whole chain); `OwnerDemote` → an
/// owner-incarnation register scoped to the ops citing `owner_id`. Returns the key, the watermark,
/// and the coordinate the watermark MUST name (§11.3). Non-cut ops and the secrets/content cuts
/// (C2/C4) return `None`.
fn control_register(c: &Candidate) -> Option<(RegisterKey, Cut, CutCoordinate)> {
    let account = c.header().account_id;
    match &c.op {
        AccountOp::DeviceRemove { device_fingerprint, control_cut, .. } => Some((
            RegisterKey::Device { account, log: 0, device: *device_fingerprint },
            control_cut.clone(),
            CutCoordinate { account, log: 0, device: *device_fingerprint },
        )),
        AccountOp::OwnerDemote { device_fingerprint, owner_id, control_cut, .. } => Some((
            RegisterKey::OwnerIncarnation {
                account,
                log: 0,
                device: *device_fingerprint,
                owner_id: *owner_id,
            },
            control_cut.clone(),
            CutCoordinate { account, log: 0, device: *device_fingerprint },
        )),
        _ => None,
    }
}

/// The CONTROL register a `CutExtend` raises (§10/§11.4) and the new watermark it joins in. A
/// `CutExtend` does NOT create a register — it extends one a prior `DeviceRemove` / `OwnerDemote`
/// made — so it is joined separately from the creator cut ops (it is never a cycle participant).
/// `incarnation_id` selects the owner-incarnation register; its absence selects the device-level
/// one. Secrets / content extends (C2/C4) return `None`.
fn cut_extend_register(c: &Candidate) -> Option<(RegisterKey, Cut, CutCoordinate)> {
    let AccountOp::CutExtend {
        chain_kind,
        incarnation_id,
        subject_account_id,
        device_fingerprint,
        new_seq,
        new_entry_hash,
        ..
    } = &c.op
    else {
        return None;
    };
    if *chain_kind != ChainKind::Ctrl {
        return None;
    }
    let account = *subject_account_id;
    let cut = Cut::At { seq: *new_seq, hash: *new_entry_hash };
    let coord = CutCoordinate { account, log: 0, device: *device_fingerprint };
    let key = match incarnation_id {
        Some(owner_id) => RegisterKey::OwnerIncarnation {
            account,
            log: 0,
            device: *device_fingerprint,
            owner_id: *owner_id,
        },
        None => RegisterKey::Device { account, log: 0, device: *device_fingerprint },
    };
    Some((key, cut, coord))
}

/// The device a cut op removes/demotes (its owner-closing SUBJECT), for the I2 last-owner
/// reservation. Non-cut ops have none.
fn cut_subject(c: &Candidate) -> Option<DeviceFingerprint> {
    match &c.op {
        AccountOp::DeviceRemove { device_fingerprint, .. }
        | AccountOp::OwnerDemote { device_fingerprint, .. } => Some(*device_fingerprint),
        _ => None,
    }
}

/// The verdict a register scoping `c` reaches — the strictest across all scoping registers governs.
enum RegisterVerdict {
    /// Beyond a cut (seq-only, I11) or off the accepted branch (L2) — never effective this fold.
    Condemned(CondemnedReason),
    /// Under a cut whose watermark/ancestry isn't held yet — undecided until it syncs (I11).
    Parked(ParkReason),
    /// No register scopes `c`, or every scoping register admits it (within-cut, on-branch).
    Clear,
}

/// Classify `c` against the accumulated registers (§11.2). The STRICTEST scoping register governs:
/// off-branch/beyond (condemned) beats a withheld-watermark park beats clear. Beyond-cut fires from
/// `[seq]` alone even when the watermark entry is withheld (I11); an under-cut entry whose branch
/// can't yet be decided PARKS (never silently accepted, never flipped later — I11).
fn register_verdict(
    c: &Candidate,
    registers: &HashMap<RegisterKey, Cut>,
    view: &dyn HeaderView,
) -> RegisterVerdict {
    let mut off_branch = false;
    let mut beyond_cut = false;
    let mut park: Option<ParkReason> = None;
    for (key, cut) in registers {
        if !key.scopes(c.header()) {
            continue;
        }
        if beyond(c.header().seq, cut) {
            beyond_cut = true;
            continue;
        }
        match candidate::ancestry(&c.hash(), cut, view) {
            Ancestry::OnBranch => {}, // within-cut on the accepted branch: this register admits it
            Ancestry::OffBranch => off_branch = true,
            Ancestry::Unknown(cause) =>
                park = Some(match cause {
                    UnknownCause::UnknownCutTarget => ParkReason::UnknownCutTarget,
                    UnknownCause::IncompleteCutAncestry => ParkReason::IncompleteCutAncestry,
                }),
        }
    }
    if off_branch {
        RegisterVerdict::Condemned(CondemnedReason::OffBranch)
    } else if beyond_cut {
        RegisterVerdict::Condemned(CondemnedReason::BeyondCut)
    } else if let Some(reason) = park {
        RegisterVerdict::Parked(reason)
    } else {
        RegisterVerdict::Clear
    }
}

/// Detect the ONE cycle that means owner-key compromise (§11.1/§12): among the same-depth cut ops
/// that install registers, `X → Y` iff X's register scopes Y's own chain AND condemns it (beyond /
/// off-branch). A cycle in this relation is two (or more) owners cutting each other simultaneously
/// ⇒ `contested`. Self-edges (a device cutting its own chain) are not mutual and are excluded.
fn has_condemn_cycle(admitted: &[AdmittedCut<'_>], view: &dyn HeaderView) -> bool {
    let n = admitted.len();
    let condemns = |x: &AdmittedCut<'_>, y: &AdmittedCut<'_>| -> bool {
        x.key.scopes(y.op.header())
            && (beyond(y.op.header().seq, &x.cut)
                || candidate::ancestry(&y.op.hash(), &x.cut, view) == Ancestry::OffBranch)
    };
    let adj: Vec<Vec<usize>> = (0..n)
        .map(|i| (0..n).filter(|&j| i != j && condemns(&admitted[i], &admitted[j])).collect())
        .collect();
    // Iterative DFS 3-colouring (0 = white, 1 = grey/on-stack, 2 = black): a grey re-visit is a
    // back edge ⇒ cycle.
    let mut colour = vec![0u8; n];
    for start in 0..n {
        if colour[start] != 0 {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        colour[start] = 1;
        while let Some((node, edge)) = stack.last().copied() {
            if edge < adj[node].len() {
                stack.last_mut().unwrap().1 += 1;
                let next = adj[node][edge];
                match colour[next] {
                    1 => return true, // back edge into the current DFS stack
                    0 => {
                        colour[next] = 1;
                        stack.push((next, 0));
                    },
                    _ => {},
                }
            } else {
                colour[node] = 2;
                stack.pop();
            }
        }
    }
    false
}

/// A same-depth cut op that passed cut-target binding + the I2 last-owner guard and so installs a
/// register — the unit the cycle detector and the `⊔` join operate over.
struct AdmittedCut<'a> {
    op: &'a Candidate,
    key: RegisterKey,
    cut: Cut,
}

/// The result of `⊔`-joining one register into the accumulated set.
enum RegisterJoin {
    /// Installed / raised the watermark.
    Applied,
    /// Two incomparable cuts for one key — owner-key compromise (§11.3).
    Contested,
    /// The branch relation can't be decided yet — leave the held register, park the newcomer.
    Parked,
}

/// `⊔`-join `cut` into `registers` under `key` (§11.3): a fresh key installs it; an existing key
/// keeps the comparable-ancestor join (the higher on-branch watermark), reports `Contested` on an
/// incomparable pair, and `Parked` when the branch relation is still undecidable.
fn join_register(
    registers: &mut HashMap<RegisterKey, Cut>,
    key: RegisterKey,
    cut: Cut,
    view: &dyn HeaderView,
) -> RegisterJoin {
    match registers.get(&key) {
        None => {
            registers.insert(key, cut);
            RegisterJoin::Applied
        },
        Some(existing) => match candidate::join_cuts(existing, &cut, view) {
            JoinResult::Extended(joined) => {
                registers.insert(key, joined);
                RegisterJoin::Applied
            },
            JoinResult::Incomparable => RegisterJoin::Contested,
            JoinResult::Unknown => RegisterJoin::Parked,
        },
    }
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
        return AccountAuthHistory {
            outcomes,
            classification: AccountClassification::Live,
            contested_successor: None,
        };
    };
    let genesis_owner_id = genesis.hash();

    // A hash → index map backs the [`CandidateView`] the ancestry walk / cut binding read through.
    let by_hash: HashMap<[u8; 32], usize> =
        candidates.iter().enumerate().map(|(i, c)| (c.hash(), i)).collect();

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

    // Two passes over the strata (§11.4 recovery): pass 0 accumulates the FINAL register set (all
    // CutExtends joined); pass 1 re-classifies against it, seeded from depth 0, so a CutExtend
    // re-blesses its condemned cone even when authored at a DEEPER incarnation depth than the
    // register's creator. Recovery is thus a pure function of the final registers — never sticky
    // per-depth condemnation.
    let base_outcomes = outcomes;
    let mut final_registers: HashMap<RegisterKey, Cut> = HashMap::new();
    for pass in 0..2 {
        let mut outcomes = base_outcomes.clone();
        let mut state = FoldState { live: HashSet::from([genesis_owner_id]), ..Default::default() };
        // The revocation registers accumulated so far (extend-only, joined by `⊔`; pass 1 seeds
        // them with the final set so every depth condemns against final watermarks), every
        // entry a register condemns / parks, and the cut ops decided in the register pass
        // (a binding failure / I2).
        let mut registers: HashMap<RegisterKey, Cut> = final_registers.clone();
        let mut condemned: HashMap<[u8; 32], CondemnedReason> = HashMap::new();
        let mut parked: HashMap<[u8; 32], ParkReason> = HashMap::new();
        let mut cut_verdicts: HashMap<[u8; 32], Outcome> = HashMap::new();
        let mut classification = AccountClassification::Live;

        'depths: for (&depth, idxs) in &strata {
            let view = CandidateView { candidates: &candidates, by_hash: &by_hash };

            // (a) REGISTER PASS. A cut op installs a register iff its author is AUTHORIZED
            // (authority_status == Live: its cited incarnation resolves to a mint for the SIGNER
            // and is live — the transitive-liveness gate that defeats laundering AND
            // owner impersonation), it passes cut-target binding (§11.3), and — for
            // OwnerDemote — its target `owner_id` names its subject device.
            let mut admitted: Vec<AdmittedCut<'_>> = Vec::new();
            for &i in idxs {
                let c = &candidates[i];
                if condemned.contains_key(&c.hash()) {
                    continue;
                }
                let Some((key, cut, coord)) = control_register(c) else {
                    continue;
                };
                if !matches!(authority_status(c, &incarnations, &state), AuthorityStatus::Live) {
                    continue; // unauthorized → the effect pass classifies it (wrong-device / stale / park)
                }
                // An OwnerDemote's `owner_id` must resolve to a mint minted for the demoted device
                // — a wrong-device binding would leave the target's real
                // incarnation unbounded.
                if let AccountOp::OwnerDemote { device_fingerprint, owner_id, .. } = &c.op {
                    match incarnations.candidate(owner_id) {
                        None => {
                            cut_verdicts
                                .insert(c.hash(), Outcome::Parked(ParkReason::UnknownOwnerRef));
                            continue;
                        },
                        Some(target) if target.subject_device() != *device_fingerprint => {
                            cut_verdicts
                                .insert(c.hash(), Outcome::Rejected(RejectReason::WrongDevice));
                            continue;
                        },
                        Some(_) => {},
                    }
                }
                match candidate::validate_cut_target(&cut, &coord, &view) {
                    // A held watermark naming a DIFFERENT coordinate is a structural reject
                    // (§11.3).
                    candidate::CutBinding::Mismatch => {
                        cut_verdicts
                            .insert(c.hash(), Outcome::Rejected(RejectReason::CutTargetMismatch));
                        continue;
                    },
                    // Held-and-correct, OR not-yet-held: install the register either way. Its
                    // `[seq]` condemns beyond entries from seq alone (I11) even
                    // before the watermark syncs; the under-cut branch decision
                    // parks until it does (a withheld watermark never flips a
                    // verdict). A revoking owner is trusted not to misstate the watermark seq
                    // (§10).
                    candidate::CutBinding::Ok | candidate::CutBinding::TargetNotHeld => {},
                }
                admitted.push(AdmittedCut { op: c, key, cut });
            }
            // Deterministic order (by entry hash) so cut selection + the `⊔` join + the I2
            // reservation are arrival-independent (I9) when two same-depth cuts contend
            // for one register key.
            admitted.sort_by_key(|a| a.op.hash());

            // A same-depth mutual owner-condemnation cycle is genuine owner-key compromise (§12):
            // halt at the last cycle-free stratum. Detected BEFORE I2 so a two-owner
            // mutual removal folds contested rather than being resolved by reserving
            // one owner.
            if has_condemn_cycle(&admitted, &view) {
                classification = AccountClassification::Contested { state_before_depth: depth };
                break 'depths;
            }

            // I2 last-owner protection across ALL same-depth admitted cuts: simulate the removals
            // in the deterministic order and reject any cut that would empty the owner
            // set, reserving a surviving owner. (The pre-depth owner count alone misses
            // concurrent same-depth removals.)
            let mut surviving_owners = state.owners.clone();
            admitted.retain(|a| {
                let Some(subject) = cut_subject(a.op) else {
                    return true;
                };
                if surviving_owners.contains(&subject) {
                    if surviving_owners.len() == 1 {
                        cut_verdicts
                            .insert(a.op.hash(), Outcome::Rejected(RejectReason::LastOwner));
                        return false;
                    }
                    surviving_owners.remove(&subject);
                }
                true
            });

            // Join each admitted creator register (§11.3 `⊔`). Two incomparable cuts for ONE key
            // (equal-seq different-hash, or divergent branches — only two owner cut ops can produce
            // this) are a same-depth mutual condemnation ⇒ contested, never a chosen hash.
            for a in &admitted {
                match join_register(&mut registers, a.key.clone(), a.cut.clone(), &view) {
                    RegisterJoin::Applied => {},
                    RegisterJoin::Contested => {
                        classification =
                            AccountClassification::Contested { state_before_depth: depth };
                        break 'depths;
                    },
                    // The branch relation can't be decided yet — keep the held register and park
                    // the newcomer (it re-joins on the next refold once its
                    // watermark syncs).
                    RegisterJoin::Parked => {
                        cut_verdicts
                            .insert(a.op.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                    },
                }
            }

            // Raise registers with this depth's live `CutExtend`s (§11.4 recovery). An extend is
            // EXTEND-ONLY: it may only raise a register a prior DeviceRemove/OwnerDemote created,
            // never conjure a fresh one (else a live owner could condemn a chain with a
            // bare extend). An extend for a not-yet-established register parks until
            // the creator syncs.
            let mut extends: Vec<(&Candidate, RegisterKey, Cut)> = Vec::new();
            for &i in idxs {
                let c = &candidates[i];
                if condemned.contains_key(&c.hash()) {
                    continue;
                }
                let Some((key, cut, coord)) = cut_extend_register(c) else {
                    continue;
                };
                if !matches!(authority_status(c, &incarnations, &state), AuthorityStatus::Live) {
                    continue;
                }
                if candidate::validate_cut_target(&cut, &coord, &view)
                    == candidate::CutBinding::Mismatch
                {
                    cut_verdicts
                        .insert(c.hash(), Outcome::Rejected(RejectReason::CutTargetMismatch));
                    continue;
                }
                if !registers.contains_key(&key) {
                    cut_verdicts.insert(c.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                    continue;
                }
                extends.push((c, key, cut));
            }
            extends.sort_by_key(|(c, _, _)| c.hash());
            for (c, key, cut) in extends {
                match join_register(&mut registers, key, cut, &view) {
                    RegisterJoin::Applied => {},
                    RegisterJoin::Contested => {
                        classification =
                            AccountClassification::Contested { state_before_depth: depth };
                        break 'depths;
                    },
                    RegisterJoin::Parked => {
                        cut_verdicts
                            .insert(c.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                    },
                }
            }

            // Re-derive condemnation + parking against the current registers. Condemnation grows
            // monotonically (a lower-depth decision is never revised); parking is rebuilt fresh (a
            // withheld watermark parks the under-cut prefix — I11). Pruning a condemned MINT from
            // `live` kills its dependents transitively (they cite an owner_id no longer live →
            // stale).
            parked.clear();
            for c in &candidates {
                if condemned.contains_key(&c.hash()) {
                    continue;
                }
                match register_verdict(c, &registers, &view) {
                    RegisterVerdict::Condemned(reason) => {
                        condemned.insert(c.hash(), reason);
                        if c.is_mint() {
                            state.live.remove(&c.hash());
                        }
                    },
                    RegisterVerdict::Parked(reason) => {
                        parked.insert(c.hash(), reason);
                    },
                    RegisterVerdict::Clear => {},
                }
            }

            // (b) EFFECT PASS over the stratum in (chain, seq, hash) order — a TOTAL order, so an
            // equivocation (same device + seq, different content) sorts identically under every
            // arrival permutation (I9).
            let mut ordered = idxs.clone();
            ordered.sort_by_key(|&i| {
                let h = candidates[i].header();
                (h.device_fingerprint.to_bytes(), h.seq, candidates[i].hash())
            });
            for i in ordered {
                let c = &candidates[i];
                if let Some(reason) = condemned.get(&c.hash()) {
                    outcomes.insert(c.hash(), Outcome::Condemned(*reason));
                    continue;
                }
                if let Some(reason) = parked.get(&c.hash()) {
                    outcomes.insert(c.hash(), Outcome::Parked(*reason));
                    continue;
                }
                if let Some(verdict) = cut_verdicts.get(&c.hash()) {
                    outcomes.insert(c.hash(), *verdict);
                    continue;
                }
                let outcome = classify_effect(c, &incarnations, &state);
                if let Outcome::Effective { .. } = outcome {
                    apply_effect(c, &mut state);
                }
                outcomes.insert(c.hash(), outcome);
            }
        }

        // Overlay the FINAL register verdicts: a candidate effective at a shallow depth but
        // condemned / parked by a later register reflects that here (the strictest verdict
        // wins over an earlier one).
        for c in &candidates {
            if let Some(reason) = condemned.get(&c.hash()) {
                outcomes.insert(c.hash(), Outcome::Condemned(*reason));
            } else if let Some(reason) = parked.get(&c.hash()) {
                outcomes.insert(c.hash(), Outcome::Parked(*reason));
            }
        }

        // In a `contested` account authority mutation halts (§12), with ONE exception:
        // `AccountReRoot` by an owner live in state_before(d). It is advisory (transfers no
        // authority — a subscriber re-decides trust against the self-certifying successor
        // genesis), so it needs no residue registers. Multiple re-roots ⇒ the deterministic
        // successor is the smallest by byte order (order-free). Every other undecided
        // candidate parks as a contested subject — fail-closed and observable, reclassified
        // if the account recovers.
        let mut contested_successor: Option<AccountId> = None;
        if matches!(classification, AccountClassification::Contested { .. }) {
            // Admit re-roots in a deterministic (successor_id, hash) order so `auth_epoch` is
            // order-free.
            let mut reroots: Vec<(&Candidate, AccountId)> = candidates
                .iter()
                .filter_map(|c| match &c.op {
                    AccountOp::AccountReRoot { successor_account_id, .. }
                        if !condemned.contains_key(&c.hash()) =>
                    {
                        let author_inc = incarnations.author_incarnation_id(c)?;
                        state.live.contains(&author_inc).then_some((c, *successor_account_id))
                    },
                    _ => None,
                })
                .collect();
            reroots.sort_by_key(|(c, successor)| (successor.to_bytes(), c.hash()));
            for (c, successor) in &reroots {
                outcomes.insert(c.hash(), effective(&state));
                state.next_auth_epoch += 1;
                contested_successor.get_or_insert(*successor);
            }
            for c in &candidates {
                outcomes.entry(c.hash()).or_insert(Outcome::Parked(ParkReason::ContestedSubject));
            }
        }

        // Pass 0 keeps only the accumulated registers, to seed pass 1; pass 1 is authoritative.
        final_registers = registers;
        if pass == 1 {
            return AccountAuthHistory { outcomes, classification, contested_successor };
        }
    }
    unreachable!("the two-pass loop always returns on pass 1")
}

/// The genesis candidate: an `AccountGenesis` whose payload hashes to the shared `account_id` (§4).
fn find_genesis(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates.iter().find(|c| {
        matches!(c.op, AccountOp::AccountGenesis { .. })
            && account_id_from_genesis_payload(&c.entry.payload) == c.header().account_id
    })
}

/// Whether `c`'s AUTHOR is presently authorized to act — the §"authority rule" preflight shared by
/// the register pass and the effect pass.
enum AuthorityStatus {
    /// The cited incarnation resolves to a mint naming the signer and is live.
    Live,
    /// The cited incarnation is unresolvable in this account (park `unknown_owner_ref`).
    Unresolvable,
    /// The cited incarnation resolves but its mint names a DIFFERENT device (reject
    /// `wrong_device`).
    WrongDevice,
    /// The cited incarnation names the signer but is not live (reject `stale_authority`).
    Stale,
}

/// The §"authority rule" (clauses 1 + 3): `c`'s cited incarnation must (1) resolve to a mint whose
/// SUBJECT device is the signer, and (3) be live. `AccountGenesis` acts under its own incarnation
/// (self-minted, subject = signer), so it passes trivially once seeded live.
fn authority_status(
    c: &Candidate,
    incarnations: &Incarnations<'_>,
    state: &FoldState,
) -> AuthorityStatus {
    let Some(author_inc) = incarnations.author_incarnation_id(c) else {
        return AuthorityStatus::Unresolvable;
    };
    let Some(mint) = incarnations.candidate(&author_inc) else {
        return AuthorityStatus::Unresolvable;
    };
    if mint.subject_device() != c.header().device_fingerprint {
        return AuthorityStatus::WrongDevice;
    }
    if !state.live.contains(&author_inc) {
        return AuthorityStatus::Stale;
    }
    AuthorityStatus::Live
}

/// Classify one op in the effect pass: authority of its author-incarnation, then the state
/// preconditions. Does NOT mutate state (that is [`apply_effect`], only on an effective verdict).
fn classify_effect(c: &Candidate, incarnations: &Incarnations<'_>, state: &FoldState) -> Outcome {
    // The author must act under a LIVE incarnation minted for THIS device (clauses 1 + 3). This is
    // what defeats laundering (a cut authored under a since-condemned owner is not live) AND owner
    // impersonation (a member citing another device's live incarnation — P3-adjacent).
    match authority_status(c, incarnations, state) {
        AuthorityStatus::Unresolvable => return Outcome::Parked(ParkReason::UnknownOwnerRef),
        AuthorityStatus::WrongDevice => return Outcome::Rejected(RejectReason::WrongDevice),
        AuthorityStatus::Stale => return Outcome::Rejected(RejectReason::StaleAuthority),
        AuthorityStatus::Live => {},
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
        // A stream can only be owned once (uniqueness); full self-certification of the spec bytes
        // (decode → `derive_v2` → owner-account binding) rides the C2 content-chain acceptance
        // predicate, where the spec decoder + grant registers live.
        AccountOp::StreamOwn { stream_id, .. } => {
            if state.streams_owned.contains(&stream_id.to_bytes()) {
                Outcome::Rejected(RejectReason::Ineffective)
            } else {
                effective(state)
            }
        },
        // A grant / revoke is ineffective until its stream has an effective `StreamOwn` (P11).
        AccountOp::StreamGrant { stream_id, .. } | AccountOp::StreamRevoke { stream_id, .. } =>
            if state.streams_owned.contains(&stream_id.to_bytes()) {
                effective(state)
            } else {
                Outcome::Rejected(RejectReason::UnownedStream)
            },
        // DeviceRemove / OwnerDemote / CutExtend reaching here are admitted cut ops (their register
        // + binding were decided in the register pass); AccountReRoot is advisory. All effective.
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
        // An effective removal tombstones the device (I4: never re-enroll) and drops it from the
        // roster/owner sets. The register it installed handles condemning its beyond-cut entries.
        AccountOp::DeviceRemove { device_fingerprint, .. } => {
            state.roster.remove(device_fingerprint);
            state.owners.remove(device_fingerprint);
            state.tombstoned.insert(*device_fingerprint);
        },
        // A demotion closes the owner incarnation (device stays enrolled as a member).
        AccountOp::OwnerDemote { device_fingerprint, .. } => {
            state.owners.remove(device_fingerprint);
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
    use crate::oplog::stream::StreamId;

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

        /// Author `op` at an EXPLICIT `(seq, prev_hash)` without advancing the device's main chain
        /// — used to forge an equivocating sibling entry (an off-branch fork) for
        /// revocation tests.
        fn author_forked(
            &mut self,
            author: &Dev,
            authority_ref: Option<[u8; 32]>,
            op: &AccountOp,
            seq: u64,
            prev_hash: Option<[u8; 32]>,
        ) -> [u8; 32] {
            let payload = encode(op).unwrap();
            let header = AccountEntryHeader {
                account_id: self.account_id,
                log_id: 0,
                device_fingerprint: author.fp,
                seq,
                prev_hash,
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

        /// Fold every entry EXCEPT `exclude` — models a watermark (or any entry) not yet synced.
        fn fold_without(&self, exclude: [u8; 32]) -> AccountAuthHistory {
            let held: Vec<VerifiedAccountEntry> =
                self.entries.iter().filter(|e| e.entry_hash != exclude).cloned().collect();
            fold_account(&held)
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

    fn device_remove(dev: &Dev, control_cut: Cut) -> AccountOp {
        AccountOp::DeviceRemove {
            device_fingerprint: dev.fp,
            control_cut,
            secrets_cut: Cut::Empty,
            content_cuts: Vec::new(),
            reason: "revoked".to_string(),
        }
    }

    fn owner_demote(dev: &Dev, owner_id: [u8; 32], control_cut: Cut) -> AccountOp {
        AccountOp::OwnerDemote {
            device_fingerprint: dev.fp,
            owner_id,
            control_cut,
            secrets_cut: Cut::Empty,
            reason: "demoted".to_string(),
        }
    }

    fn owner_promote(dev: &Dev) -> AccountOp {
        AccountOp::OwnerPromote { device_fingerprint: dev.fp }
    }

    fn account_reroot(successor: AccountId) -> AccountOp {
        AccountOp::AccountReRoot { successor_account_id: successor, note: None }
    }

    fn stream_own(stream: StreamId) -> AccountOp {
        AccountOp::StreamOwn { stream_id: stream, stream_spec_bytes: vec![0x80] }
    }

    fn stream_grant(stream: StreamId, grantee: AccountId) -> AccountOp {
        AccountOp::StreamGrant {
            stream_id: stream,
            grantee_account_id: grantee,
            grant_role: ops::GrantRole::Reader,
        }
    }

    /// A control-log `CutExtend` raising `subject`'s register (device-level when `incarnation_id`
    /// is `None`, owner-incarnation otherwise) to `[new_seq, new_entry_hash]`.
    fn cut_extend_ctrl(
        account: AccountId,
        subject: &Dev,
        incarnation_id: Option<[u8; 32]>,
        new_seq: u64,
        new_entry_hash: [u8; 32],
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
        let add_b = f.author(&a, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let b0 = f.author(&b, Some(add_b), &device_add(&d, DeviceRole::Member));
        let b1 = f.author(&b, Some(add_b), &device_add(&e, DeviceRole::Member));
        let b2 = f.author(&b, Some(add_b), &device_add(&fdev, DeviceRole::Member));
        // A forged sibling of b1: seq 1 (within the cut) but forking off b0 — off the branch b1 is
        // on.
        let forged =
            f.author_forked(&b, Some(add_b), &device_add(&g, DeviceRole::Member), 1, Some(b0));
        // A removes B, valid prefix pinned to b1 on B's control chain.
        let remove_b =
            f.author(&a, Some(f.genesis_hash), &device_remove(&b, Cut::At { seq: 1, hash: b1 }));

        let h = f.fold();
        assert!(h.is_effective(&remove_b), "the removal itself is effective");
        assert!(h.is_effective(&b0), "b0 is within the cut and on-branch — effective");
        assert!(h.is_effective(&b1), "b1 (the watermark slot) is within the cut — effective");
        assert_eq!(
            h.outcome(&b2),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
            "b2 is beyond the cut — a back-dated forgery",
        );
        assert_eq!(
            h.outcome(&forged),
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
        let add_b = f.author(&a, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let demote_b = f.author(&a, Some(f.genesis_hash), &owner_demote(&b, add_b, Cut::Empty));
        let add_c = f.author(&b, Some(add_b), &device_add(&c, DeviceRole::Owner));
        let remove_a = f.author(&c, Some(add_c), &device_remove(&a, Cut::Empty));

        let h = f.fold();
        assert!(h.is_effective(&f.genesis_hash));
        assert!(h.is_effective(&add_b), "B was a legitimately-added owner");
        assert!(h.is_effective(&demote_b), "A's demotion of B is effective");
        assert_eq!(
            h.outcome(&add_c),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
            "B's laundered owner-mint is condemned by the demotion cut",
        );
        assert_eq!(
            h.outcome(&remove_a),
            Some(Outcome::Rejected(RejectReason::StaleAuthority)),
            "C's removal of A cites an incarnation that never lived",
        );
        assert!(!h.is_effective(&remove_a), "A is not removed — laundering defeated");
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let remove_b = f.author(&a, Some(add_a), &device_remove(&b, Cut::Empty));
        let remove_a = f.author(&b, Some(add_b), &device_remove(&a, Cut::Empty));
        // Two competing recovery re-roots by a pre-contest owner (A, live in state_before(1)).
        let reroot_big = f.author(&a, Some(add_a), &account_reroot(big));
        let reroot_small = f.author(&a, Some(add_a), &account_reroot(small));

        let h = f.fold();
        assert_eq!(
            h.classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "two owners cutting each other is contested at the last cycle-free depth",
        );
        assert!(h.is_effective(&f.genesis_hash), "state_before(1) keeps the depth-0 roster");
        assert!(h.is_effective(&add_a), "A was a legitimate owner before the standoff");
        assert!(h.is_effective(&add_b), "B was a legitimate owner before the standoff");
        assert!(!h.is_effective(&remove_a), "authority mutation is halted — no removal folds");
        assert!(!h.is_effective(&remove_b), "authority mutation is halted — no removal folds");
        assert_eq!(
            h.outcome(&remove_a),
            Some(Outcome::Parked(ParkReason::ContestedSubject)),
            "the residue cut op parks, fail-closed",
        );
        // The sole admitted ops are the pre-contest owner's re-roots; the successor is
        // deterministic.
        assert!(h.is_effective(&reroot_small) && h.is_effective(&reroot_big), "re-roots admitted");
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
    fn incomparable_cuts_for_one_key_are_contested_p5() {
        // D equivocates: two entries at D's seq 0 (different content ⇒ different hashes). Two
        // owners A and B each remove D, but their control cuts name the two DIFFERENT seq-0
        // watermarks. One register key (Device{D}) with equal-seq / different-hash
        // watermarks is incomparable — the fold refuses to pick a hash and folds contested
        // (§11.3).
        let (fdr, a, b, d) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
        let (t8, t9) = (Dev::new(8), Dev::new(9));
        let mut f = Fixture::genesis(&fdr);
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let add_d = f.author(&fdr, Some(f.genesis_hash), &device_add(&d, DeviceRole::Owner));
        // D's equivocation: two seq-0 entries on D's chain with distinct content.
        let d0a = f.author_forked(&d, Some(add_d), &device_add(&t8, DeviceRole::Member), 0, None);
        let d0b = f.author_forked(&d, Some(add_d), &device_add(&t9, DeviceRole::Member), 0, None);
        assert_ne!(d0a, d0b, "the two seq-0 entries must be distinct watermarks");
        f.author(&a, Some(add_a), &device_remove(&d, Cut::At { seq: 0, hash: d0a }));
        f.author(&b, Some(add_b), &device_remove(&d, Cut::At { seq: 0, hash: d0b }));

        let h = f.fold();
        assert_eq!(
            h.classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "one register key with equal-seq different-hash cuts is contested",
        );
        assert!(h.is_effective(&add_a) && h.is_effective(&add_b) && h.is_effective(&add_d));
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let b0 = f.author(&b, Some(add_b), &device_add(&d, DeviceRole::Member));
        let b1 = f.author(&b, Some(add_b), &device_add(&e, DeviceRole::Member));
        let b2 = f.author(&b, Some(add_b), &device_add(&g, DeviceRole::Member));
        f.author(&fdr, Some(f.genesis_hash), &device_remove(&b, Cut::At { seq: 1, hash: b1 }));

        // Withheld: fold WITHOUT b1. Beyond-cut still fires; the under-cut prefix parks.
        let withheld = f.fold_without(b1);
        assert_eq!(
            withheld.outcome(&b0),
            Some(Outcome::Parked(ParkReason::UnknownCutTarget)),
            "under-cut b0 parks while the watermark is withheld",
        );
        assert_eq!(
            withheld.outcome(&b2),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
            "beyond-cut b2 is condemned from seq alone (I11) even with the watermark withheld",
        );

        // Healed: the watermark synced — the prefix is on the accepted branch and re-blesses; the
        // beyond-cut verdict is unchanged (no prior verdict flipped).
        let healed = f.fold();
        assert!(healed.is_effective(&b0), "b0 heals to effective once b1 is held");
        assert!(healed.is_effective(&b1), "b1 (the watermark slot) is within the cut");
        assert_eq!(
            healed.outcome(&b2),
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let a0 = f.author(&a, Some(add_a), &device_add(&d, DeviceRole::Member));
        let b0 = f.author(&b, Some(add_b), &device_add(&g, DeviceRole::Member));
        let b1 = f.author(&b, Some(add_b), &device_add(&hdev, DeviceRole::Member));
        // Demote B's incarnation, valid prefix pinned to b0 ⇒ b1 (seq 1) is condemned.
        f.author(
            &fdr,
            Some(f.genesis_hash),
            &owner_demote(&b, add_b, Cut::At { seq: 0, hash: b0 }),
        );
        f.author(&a, Some(add_a), &device_remove(&d, Cut::Empty));
        // An op citing an unresolvable incarnation parks.
        let foreign = f.author(&a, Some([0x77u8; 32]), &device_add(&k, DeviceRole::Member));

        let baseline = f.fold();
        // Totality: exactly one outcome per candidate, and the mixed classes are all represented.
        assert_eq!(baseline.outcomes.len(), f.entries.len(), "every candidate is classified");
        assert!(baseline.is_effective(&a0) && baseline.is_effective(&add_a));
        assert_eq!(baseline.outcome(&b1), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));
        assert_eq!(baseline.outcome(&foreign), Some(Outcome::Parked(ParkReason::UnknownOwnerRef)));

        // No oscillation: the FULL map (including auth_epoch numbering) is permutation-invariant.
        for rot in 0..f.entries.len() {
            let rotated = f.fold_rotated(rot);
            assert_eq!(rotated.classification(), AccountClassification::Live);
            assert_eq!(
                rotated.outcomes, baseline.outcomes,
                "rotation {rot} changed the outcome map"
            );
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let b0 = f.author(&b, Some(add_b), &device_add(&d, DeviceRole::Member));
        let b1 = f.author(&b, Some(add_b), &device_add(&e, DeviceRole::Member));
        let b2 = f.author(&b, Some(add_b), &device_add(&g, DeviceRole::Member));
        let remove_b =
            f.author(&fdr, Some(f.genesis_hash), &device_remove(&b, Cut::At { seq: 0, hash: b0 }));
        let extend =
            f.author(&fdr, Some(f.genesis_hash), &cut_extend_ctrl(f.account_id, &b, None, 2, b2));

        // Without the extend, the beyond-cut cone is condemned.
        let before = f.fold_without(extend);
        assert_eq!(before.outcome(&b1), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));
        assert_eq!(before.outcome(&b2), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));

        // With the extend, the cone re-blesses; the removal + extend themselves are effective.
        let after = f.fold();
        assert!(after.is_effective(&b0), "b0 stays within every watermark");
        assert!(after.is_effective(&b1), "b1 re-blessed by the extend");
        assert!(after.is_effective(&b2), "b2 re-blessed by the extend");
        assert!(after.is_effective(&remove_b) && after.is_effective(&extend));
    }

    #[test]
    fn recovery_reopens_a_fresh_incarnation_and_bars_tombstone_readd_p7() {
        // Demotion bounds the OLD incarnation via its register, but a re-PROMOTE mints a FRESH
        // owner_id with no register, so the device resumes (§11). A tombstoned fingerprint, by
        // contrast, is a permanent bar (I4): re-adding it is rejected.
        let (fdr, b, dremoved) = (Dev::new(1), Dev::new(2), Dev::new(4));
        let (t5, t6) = (Dev::new(5), Dev::new(6));
        let mut f = Fixture::genesis(&fdr);
        let g1 = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        // Demote B's incarnation g1 with an empty cut — everything B does under g1 is condemned.
        f.author(&fdr, Some(f.genesis_hash), &owner_demote(&b, g1, Cut::Empty));
        let under_g1 = f.author(&b, Some(g1), &device_add(&t5, DeviceRole::Member));
        // Re-promote B → a fresh incarnation g2 (no register); B's work under g2 accepts.
        let g2 = f.author(&fdr, Some(f.genesis_hash), &owner_promote(&b));
        let under_g2 = f.author(&b, Some(g2), &device_add(&t6, DeviceRole::Member));
        // A removed device's fingerprint is tombstoned; re-adding it is barred (I4).
        f.author(&fdr, Some(f.genesis_hash), &device_add(&dremoved, DeviceRole::Member));
        f.author(&fdr, Some(f.genesis_hash), &device_remove(&dremoved, Cut::Empty));
        let readd =
            f.author(&fdr, Some(f.genesis_hash), &device_add(&dremoved, DeviceRole::Member));

        let h = f.fold();
        assert_eq!(
            h.outcome(&under_g1),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
            "work under the demoted incarnation is condemned",
        );
        assert!(h.is_effective(&g2), "the re-promotion mints a fresh incarnation");
        assert!(h.is_effective(&under_g2), "B resumes under the fresh incarnation");
        assert_eq!(
            h.outcome(&readd),
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        f.author(&fdr, Some(f.genesis_hash), &device_add(&m, DeviceRole::Member));
        let impersonation = f.author(&m, Some(add_a), &device_add(&x, DeviceRole::Member));

        let h = f.fold();
        assert!(h.is_effective(&add_a));
        assert_eq!(
            h.outcome(&impersonation),
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let bad = f.author(&fdr, Some(f.genesis_hash), &owner_demote(&a, add_b, Cut::Empty));

        let h = f.fold();
        assert!(h.is_effective(&add_a) && h.is_effective(&add_b));
        assert_eq!(
            h.outcome(&bad),
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let b0 = f.author(&b, Some(add_b), &device_add(&d, DeviceRole::Member));
        let b1 = f.author(&b, Some(add_b), &device_add(&e, DeviceRole::Member));
        let extend =
            f.author(&fdr, Some(f.genesis_hash), &cut_extend_ctrl(f.account_id, &b, None, 0, b0));

        let h = f.fold();
        assert!(h.is_effective(&b0), "b0 effective");
        assert!(
            h.is_effective(&b1),
            "b1 (seq 1, beyond a phantom [0] watermark) is effective — the extend created no \
             register",
        );
        assert_eq!(
            h.outcome(&extend),
            Some(Outcome::Parked(ParkReason::UnknownCutTarget)),
            "a bare extend parks until a creating cut exists",
        );
    }

    #[test]
    fn a_stream_grant_before_its_stream_own_is_ineffective_p11() {
        // On F's chain a StreamGrant precedes the StreamOwn (lower seq); the grant is ineffective
        // until the stream is owned (order-free: the precondition is on effective state,
        // seq-ordered within the chain), and a later grant after the StreamOwn is
        // effective.
        let fdr = Dev::new(1);
        let stream = StreamId::from_bytes([0x33; 32]);
        let grantee = AccountId::from_bytes([0x44; 32]);
        let mut f = Fixture::genesis(&fdr);
        let early = f.author(&fdr, Some(f.genesis_hash), &stream_grant(stream, grantee));
        let own = f.author(&fdr, Some(f.genesis_hash), &stream_own(stream));
        let late = f.author(&fdr, Some(f.genesis_hash), &stream_grant(stream, grantee));

        let h = f.fold();
        assert_eq!(
            h.outcome(&early),
            Some(Outcome::Rejected(RejectReason::UnownedStream)),
            "a grant before its StreamOwn is ineffective",
        );
        assert!(h.is_effective(&own), "the StreamOwn is effective");
        assert!(h.is_effective(&late), "a grant after the StreamOwn is effective");
    }

    #[test]
    fn cut_extend_reblesses_across_incarnation_depths_p7() {
        // The cross-depth recovery case: F (depth 0) removes B pinned to seq 0, condemning b1/b2. A
        // DEEPER owner A (a depth-1 incarnation) extends B's device cut to seq 2. Because the fold
        // classifies against the FINAL accumulated registers, the cone re-blesses even though the
        // extend is authored at a deeper depth than the removal — recovery is not sticky per-depth.
        let (fdr, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let (d, e, g) = (Dev::new(5), Dev::new(6), Dev::new(7));
        let mut f = Fixture::genesis(&fdr);
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let b0 = f.author(&b, Some(add_b), &device_add(&d, DeviceRole::Member));
        let b1 = f.author(&b, Some(add_b), &device_add(&e, DeviceRole::Member));
        let b2 = f.author(&b, Some(add_b), &device_add(&g, DeviceRole::Member));
        // Creator at depth 0.
        let remove_b =
            f.author(&fdr, Some(f.genesis_hash), &device_remove(&b, Cut::At { seq: 0, hash: b0 }));
        // Extend authored by A (depth-1 incarnation) — DEEPER than the depth-0 removal.
        let extend = f.author(&a, Some(add_a), &cut_extend_ctrl(f.account_id, &b, None, 2, b2));

        let h = f.fold();
        assert!(h.is_effective(&remove_b) && h.is_effective(&extend));
        assert!(h.is_effective(&b1), "b1 re-blessed by the deeper-depth extend");
        assert!(h.is_effective(&b2), "b2 re-blessed by the deeper-depth extend");
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
