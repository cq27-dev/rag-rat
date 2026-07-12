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
use crate::oplog::cbor;
use crate::oplog::op::DeviceFingerprint;

/// The account CONTROL log the fold operates on (§11) — its registers are control-log scoped
/// (`log: 0`). A known op on the secrets (1) or content (2) log is not a control op and is retained
/// unfolded here (its own C2/C4 fold owns it), never minting control authority.
pub(super) const CONTROL_LOG: u8 = 0;
/// The account-op version this fold understands. A known `entry_type` at a different version may
/// reuse the tag with new semantics, so it is retained-unfolded rather than folded as today's op.
pub(super) const SUPPORTED_OP_VERSION: u32 = 1;

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

    /// The §16.3 stored-taxonomy `(status, detail)` for this outcome. `Effective` maps to
    /// `("effective", None)` — the storage layer resolves accepted vs `forked` per slot (I10a) —
    /// and a fold-semantic `Rejected` maps to `("rejected", reason)`; structural ingest rejects
    /// are never folded (they are not stored). Kept beside the enum so the projection can't
    /// drift.
    pub(super) fn taxonomy(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Outcome::Effective { .. } => ("effective", None),
            Outcome::RetainedUnfolded => ("retained_unfolded", None),
            Outcome::Condemned(reason) => (
                "condemned",
                Some(match reason {
                    CondemnedReason::BeyondCut => "beyond_cut",
                    CondemnedReason::OffBranch => "off_branch",
                    CondemnedReason::ClosedIncarnation => "closed_incarnation",
                }),
            ),
            Outcome::Parked(reason) => (
                "parked",
                Some(match reason {
                    ParkReason::UnknownOwnerRef => "unknown_owner_ref",
                    ParkReason::UnknownCutTarget => "unknown_cut_target",
                    ParkReason::IncompleteCutAncestry => "incomplete_cut_ancestry",
                    ParkReason::ContestedSubject => "contested_subject",
                    ParkReason::DeferredStreamAuthorization => "deferred_stream_authorization",
                }),
            ),
            Outcome::Rejected(reason) => (
                "rejected",
                Some(match reason {
                    RejectReason::StaleAuthority => "stale_authority",
                    RejectReason::GenesisSelfHash => "genesis_self_hash",
                    RejectReason::DuplicateGenesis => "duplicate_genesis",
                    RejectReason::DuplicateAdd => "duplicate_add",
                    RejectReason::TombstoneReAdd => "tombstone_re_add",
                    RejectReason::BadPromote => "bad_promote",
                    RejectReason::LastOwner => "last_owner",
                    RejectReason::CutTargetMismatch => "cut_target_mismatch",
                    RejectReason::WrongDevice => "wrong_device",
                    RejectReason::Malformed => "malformed",
                    RejectReason::NonGenesisOrigin => "non_genesis_origin",
                    RejectReason::Ineffective => "ineffective",
                }),
            ),
        }
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
    /// A known control op at the supported version whose payload does not decode (malformed CBOR /
    /// invalid enum token). Ingest structurally rejects these; this is the fold's defensive
    /// backstop — a hard reject, NOT retained, so its header never shapes cut ancestry.
    Malformed,
    /// A seq-0 origin entry on the FOUNDER's chain that is not the genesis — a second seq-0 slot
    /// competing with the root (equivocation). The founder's origin slot is the genesis alone.
    NonGenesisOrigin,
    /// A duplicate / no-op / self-referential op with no effect — incl. an `AccountReRoot` in a
    /// `Live` account (admissible only as the terminal recovery op once contested, §12).
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
    /// A `StreamOwn` / `StreamGrant` / `StreamRevoke`: stream ownership + grant authority is folded
    /// by the C2 content-chain acceptance predicate (which owns the `/2` spec decoder and the
    /// grant registers), not the C1 control fold. Parked here so C1 trusts NOTHING from an
    /// unvalidated stream op; C2 reclassifies it.
    DeferredStreamAuthorization,
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
        // ITERATIVE walk of the `authority_ref` chain — NOT recursion. Chain depth is
        // adversary-controlled (a non-owner can mint a long citation chain; its depth is computed
        // here, before any authority check, so a deep chain must never overflow the stack — §18b
        // caps chain LENGTH, not our frames). Walk to a base with a known depth (genesis / cached /
        // unresolvable), collecting the path, then assign depths back up. The in-progress `None`
        // marker also breaks an (impossible) cycle: a revisit reads `None` and resolves
        // unresolvable.
        let mut chain: Vec<[u8; 32]> = Vec::new();
        let mut node = owner_id;
        let base: Option<usize> = loop {
            if let Some(cached) = self.depth.get(&node) {
                break *cached;
            }
            self.depth.insert(node, None);
            let Some(mint) = self.candidate(&node) else {
                break None; // no mint in this account — unresolvable (cross-account P3 / unsynced)
            };
            if node == self.genesis_owner_id {
                break Some(0);
            }
            match mint.header().authority_ref {
                None => break None, // a non-genesis mint with no cited incarnation is unresolvable
                Some(parent) => {
                    chain.push(node);
                    node = parent;
                },
            }
        };
        // The terminal `node`'s depth is `base` (correct the in-progress marker for genesis);
        // each earlier link is one deeper. `None` propagates up an unresolvable chain.
        self.depth.insert(node, base);
        let mut d = base;
        for &link in chain.iter().rev() {
            d = d.map(|x| x + 1);
            self.depth.insert(link, d);
        }
        self.depth.get(&owner_id).copied().flatten()
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
    /// Each enrolled device → the entry_hash of the DeviceAdd / genesis that enrolled it. Keyed by
    /// SOURCE (not just presence) so condemning a superseded / duplicate add for a device does not
    /// erase the enrollment a DIFFERENT, still-valid add contributed.
    roster: HashMap<DeviceFingerprint, [u8; 32]>,
    /// Each device holding an OPEN owner incarnation → that incarnation's `owner_id`. Keyed by
    /// incarnation (not just device) so a stale `OwnerDemote` naming a since-superseded `owner_id`
    /// cannot close a device's freshly-reopened incarnation.
    owners: HashMap<DeviceFingerprint, [u8; 32]>,
    /// Removed devices — never re-enroll (I4).
    tombstoned: HashSet<DeviceFingerprint>,
    /// Whether an `AccountGenesis` has been made effective.
    genesis_seen: bool,
    /// 0-based effective index assigned as `auth_epoch`.
    next_auth_epoch: u64,
}

/// A hash-keyed [`HeaderView`] over ALL verified entries — the ancestry walk + cut-target binding
/// read headers through this. It spans every entry (incl. forward-compat UNKNOWN ops that are not
/// folded): a signed header is present whether or not its op decodes, so a cut may name an unknown
/// entry as its watermark, and unknown entries beyond a cut are condemnable.
struct CandidateView<'a> {
    headers: &'a HashMap<[u8; 32], &'a AccountEntryHeader>,
}

impl HeaderView for CandidateView<'_> {
    fn header(&self, entry_hash: &[u8; 32]) -> Option<&AccountEntryHeader> {
        self.headers.get(entry_hash).copied()
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
    // Track the park CAUSES as flags (not a last-write-wins var) — `registers` iterates in random
    // HashMap order, so a fixed precedence keeps the observable ParkReason deterministic (I9).
    let mut park_unknown_target = false;
    let mut park_incomplete = false;
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
            Ancestry::Unknown(UnknownCause::UnknownCutTarget) => park_unknown_target = true,
            Ancestry::Unknown(UnknownCause::IncompleteCutAncestry) => park_incomplete = true,
        }
    }
    // Precedence: off-branch/beyond (condemned) > a missing watermark entry > a missing mid-chain
    // link > clear.
    if off_branch {
        RegisterVerdict::Condemned(CondemnedReason::OffBranch)
    } else if beyond_cut {
        RegisterVerdict::Condemned(CondemnedReason::BeyondCut)
    } else if park_unknown_target {
        RegisterVerdict::Parked(ParkReason::UnknownCutTarget)
    } else if park_incomplete {
        RegisterVerdict::Parked(ParkReason::IncompleteCutAncestry)
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

    // Decode once, then classify. `candidates` are the ops the fold actually folds; `all_headers`
    // is the ancestry / cut-binding view — it holds every STRUCTURALLY-VALID entry (a valid chain
    // link, incl. a forward-compat unknown or a sealed op), but NOT a malformed one, so invalid
    // bytes can't shape the accepted branch.
    let mut candidates: Vec<Candidate> = Vec::with_capacity(entries.len());
    let mut all_headers: HashMap<[u8; 32], &AccountEntryHeader> = HashMap::new();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    for entry in entries {
        // Dedup the entry SET by hash — the fold classifies each entry once; a duplicated entry
        // must not apply its state transition (or overwrite its outcome) twice
        // (order-independence).
        if !seen.insert(entry.entry_hash) {
            continue;
        }
        // Fold only a KNOWN op on the control log (§11), at the supported version, with a PLAINTEXT
        // payload (`crypto_suite == 0`). A NON-foldable entry (unknown type / other log / future
        // version / sealed `crypto_suite != 0` ciphertext that could spuriously parse) is always
        // retained header-only — never folded, never HARD-rejected — so a forward-compatible entry
        // stays a valid watermark/ancestry target and its own layer (C2/C4/newer) folds it.
        let foldable = entry.header.log_id == CONTROL_LOG
            && entry.header.op_version == SUPPORTED_OP_VERSION
            && entry.header.crypto_suite == 0;
        if !foldable {
            all_headers.insert(entry.entry_hash, &entry.header);
            outcomes.insert(entry.entry_hash, Outcome::RetainedUnfolded);
            continue;
        }
        match ops::decode(entry.header.entry_type, &entry.payload) {
            // A malformed CURRENT-version control op is a hard reject and does NOT become a chain
            // link (ingest structurally rejects these; this is the fold's defensive backstop).
            Err(_) => {
                outcomes.insert(entry.entry_hash, Outcome::Rejected(RejectReason::Malformed));
            },
            Ok(DecodedAccountOp::Known(op)) => {
                all_headers.insert(entry.entry_hash, &entry.header);
                candidates.push(Candidate { entry: entry.clone(), op });
            },
            // A control-log, current-version, unknown-entry_type op: forward-compat, retained
            // header-only.
            Ok(DecodedAccountOp::Unknown { .. }) => {
                all_headers.insert(entry.entry_hash, &entry.header);
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
    let genesis_founder = genesis.subject_device();

    let mut incarnations = Incarnations::build(&candidates, genesis_owner_id);

    // Group resolvable candidates by author-depth; unresolvable citations park.
    let mut strata: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (idx, c) in candidates.iter().enumerate() {
        // The founder's seq-0 origin slot is the genesis ALONE. A second seq-0 entry on the
        // founder's chain is an origin equivocation with the root — reject it (else, sorting before
        // genesis by hash, it could take auth_epoch 0 and mutate state before the root is applied).
        if c.header().seq == 0
            && c.header().device_fingerprint == genesis_founder
            && c.hash() != genesis_owner_id
        {
            outcomes.insert(c.hash(), Outcome::Rejected(RejectReason::NonGenesisOrigin));
            continue;
        }
        match incarnations.author_depth(c) {
            Some(d) => strata.entry(d).or_default().push(idx),
            None => {
                outcomes.insert(c.hash(), Outcome::Parked(ParkReason::UnknownOwnerRef));
            },
        }
    }

    // A single per-depth pass over the strata (§11.1): each depth admits its live cut ops'
    // registers, detects contested, condemns/parks against the registers so far, then runs the
    // effect pass. A CutExtend re-blesses its cone via the same-depth `⊔` join (cross-depth
    // recovery is the deferred fixpoint's job — a global recompute could preserve a register
    // whose creator the fold condemns).
    let mut state = FoldState { live: HashSet::from([genesis_owner_id]), ..Default::default() };
    // The revocation registers accumulated so far (extend-only, joined by `⊔`), every entry a
    // register condemns (grows monotonically — a lower-depth decision is final, no oscillation) /
    // parks (rebuilt fresh each depth), and the cut ops decided in the register pass (a binding
    // failure / I2). Per-depth monotone, matching §11.1 — a CutExtend re-blesses its cone via the
    // same-depth `⊔` join, not a global recompute (a global seed could preserve a register whose
    // creator the fold later condemns).
    let mut registers: HashMap<RegisterKey, Cut> = HashMap::new();
    let mut condemned: HashMap<[u8; 32], CondemnedReason> = HashMap::new();
    let mut parked: HashMap<[u8; 32], ParkReason> = HashMap::new();
    let mut cut_verdicts: HashMap<[u8; 32], Outcome> = HashMap::new();
    let mut classification = AccountClassification::Live;

    'depths: for (depth, idxs) in strata {
        let view = CandidateView { headers: &all_headers };

        // (a) REGISTER PASS. A cut op installs a register iff its author is AUTHORIZED
        // (authority_status == Live: its cited incarnation resolves to a mint for the SIGNER
        // and is live — the transitive-liveness gate that defeats laundering AND
        // owner impersonation), it passes cut-target binding (§11.3), and — for
        // OwnerDemote — its target `owner_id` names its subject device.
        let mut admitted: Vec<AdmittedCut<'_>> = Vec::new();
        for &i in &idxs {
            let c = &candidates[i];
            // A condemned OR parked cut op installs nothing — parked authority (its own chain is
            // under a not-yet-decided watermark) must not have register side effects before it is
            // on a known-valid branch.
            if condemned.contains_key(&c.hash()) || parked.contains_key(&c.hash()) {
                continue;
            }
            let Some((key, cut, coord)) = control_register(c) else {
                continue;
            };
            if !matches!(authority_status(c, &incarnations, &state, &parked), AuthorityStatus::Live)
            {
                continue; // unauthorized → the effect pass classifies it (wrong-device / stale / park)
            }
            // NOTE: the register pass can't gate a DeviceRemove on the target being enrolled — at
            // this depth the roster does not yet reflect same-depth mints (genesis, DeviceAdd), so
            // gating here would wrongly skip a founder self-removal / a same-depth remove and lose
            // the self-condemnation that keeps the account rooted. The effect pass rejects a remove
            // of a never-enrolled device `Ineffective` (no tombstone); a lingering register for a
            // genuinely never-enrolled device is revocation persisting across a re-add — the owner
            // revoked that chain, which the trusted-owner model treats as intended.
            //
            // An OwnerDemote's `owner_id` must resolve to a mint minted for the demoted device
            // — a wrong-device binding would leave the target's real
            // incarnation unbounded.
            if let AccountOp::OwnerDemote { device_fingerprint, owner_id, .. } = &c.op {
                match incarnations.candidate(owner_id) {
                    None => {
                        cut_verdicts.insert(c.hash(), Outcome::Parked(ParkReason::UnknownOwnerRef));
                        continue;
                    },
                    Some(target) if target.subject_device() != *device_fingerprint => {
                        cut_verdicts.insert(c.hash(), Outcome::Rejected(RejectReason::WrongDevice));
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
                // `[seq]` condemns beyond entries from seq alone (I11) even before the watermark
                // syncs; the under-cut branch decision parks until it does (a withheld watermark
                // never flips a verdict). A revoking owner is TRUSTED not to misstate the watermark
                // seq (§10) — a watermark that later resolves to a different coordinate (flipping
                // this to `CutTargetMismatch`) is owner misbehaviour, out of the trusted-owner
                // model.
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
        // in deterministic order over the prior-depth owner set and reject any cut that would
        // empty it, reserving a surviving owner. A cut counts only if it CLOSES a device's
        // currently-open incarnation (a DeviceRemove of any owner, or an OwnerDemote naming the
        // open `owner_id`) — a stale demote does not. (A self-cut is separately self-defeating:
        // its own op sits beyond any watermark it can name on its chain, so it self-condemns.)
        let mut surviving = state.owners.clone();
        admitted.retain(|a| {
            let closes = match &a.op.op {
                AccountOp::DeviceRemove { device_fingerprint, .. } =>
                    surviving.contains_key(device_fingerprint).then_some(*device_fingerprint),
                AccountOp::OwnerDemote { device_fingerprint, owner_id, .. } =>
                    (surviving.get(device_fingerprint) == Some(owner_id))
                        .then_some(*device_fingerprint),
                _ => None,
            };
            if let Some(dev) = closes {
                if surviving.len() == 1 {
                    cut_verdicts.insert(a.op.hash(), Outcome::Rejected(RejectReason::LastOwner));
                    return false;
                }
                surviving.remove(&dev);
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
                    classification = AccountClassification::Contested { state_before_depth: depth };
                    break 'depths;
                },
                // The branch relation can't be decided yet — keep the held register and park
                // the newcomer (it re-joins on the next refold once its
                // watermark syncs).
                RegisterJoin::Parked => {
                    cut_verdicts.insert(a.op.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                },
            }
        }

        // Raise registers with this depth's live `CutExtend`s (§11.4 recovery). An extend is
        // EXTEND-ONLY: it may only raise a register a prior DeviceRemove/OwnerDemote created,
        // never conjure a fresh one (else a live owner could condemn a chain with a
        // bare extend). An extend for a not-yet-established register parks until
        // the creator syncs.
        let mut extends: Vec<(&Candidate, RegisterKey, Cut)> = Vec::new();
        for &i in &idxs {
            let c = &candidates[i];
            if condemned.contains_key(&c.hash()) || parked.contains_key(&c.hash()) {
                continue;
            }
            let Some((key, cut, coord)) = cut_extend_register(c) else {
                continue;
            };
            if !matches!(authority_status(c, &incarnations, &state, &parked), AuthorityStatus::Live)
            {
                continue;
            }
            if candidate::validate_cut_target(&cut, &coord, &view)
                == candidate::CutBinding::Mismatch
            {
                cut_verdicts.insert(c.hash(), Outcome::Rejected(RejectReason::CutTargetMismatch));
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
                    classification = AccountClassification::Contested { state_before_depth: depth };
                    break 'depths;
                },
                RegisterJoin::Parked => {
                    cut_verdicts.insert(c.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                },
            }
        }

        // Re-derive condemnation + parking against the current registers. Condemnation grows
        // monotonically (a lower-depth decision is never revised); parking is rebuilt fresh (a
        // withheld watermark parks the under-cut prefix — I11). Pruning a condemned MINT from
        // `live` kills its dependents transitively (they cite an owner_id no longer live →
        // stale).
        //
        // BOUNDARY (per-depth model): a register that retroactively condemns an already-effective
        // NON-mint entry (e.g. a member DeviceAdd) is reflected in that entry's outcome (the final
        // overlay), but its roster/tombstone side effect is not rolled back, and a same-depth
        // dependent may read the stale roster. Reaching that state requires an OWNER surgically
        // cutting a co-owner's incarnation to strand a member it is concurrently promoting —
        // Byzantine owner behaviour, which the trusted-owner threat model puts out of automated
        // resolution (owner-key compromise ⇒ `contested`, never silently reconciled). A full
        // effect-state rebuild + register-provenance fixpoint would close it and is the natural
        // home for cross-depth `CutExtend` recovery too; both are deferred with that model in
        // force.
        parked.clear();
        for c in &candidates {
            // The genesis is the account's ROOT axiom — it can never be condemned, else a cut on
            // the founder's own chain (e.g. a self-DeviceRemove with an empty cut,
            // which condemns everything on that chain incl. seq 0) would leave a `Live`
            // account with no effective root. The founder's LATER entries stay
            // condemnable; only the seq-0 root is exempt.
            if c.hash() == genesis_owner_id || condemned.contains_key(&c.hash()) {
                continue;
            }
            match register_verdict(c, &registers, &view) {
                RegisterVerdict::Condemned(reason) => {
                    condemned.insert(c.hash(), reason);
                    // A condemned mint leaves `live` (kills dependents transitively) and, if it is
                    // the device's open incarnation, `owners`. A condemned DeviceAdd ALSO leaves
                    // the roster — its enrollment is invalidated, so a later
                    // OwnerPromote must not see the device as enrolled. (A
                    // condemned OwnerPromote leaves the roster intact —
                    // the device's separate DeviceAdd enrollment may still be valid.)
                    if c.is_mint() {
                        state.live.remove(&c.hash());
                        if state.owners.get(&c.subject_device()) == Some(&c.hash()) {
                            state.owners.remove(&c.subject_device());
                        }
                        // Roll back the roster only if THIS DeviceAdd is the source of the current
                        // enrollment — a condemned duplicate/superseded add must not erase the
                        // enrollment a different, still-valid add contributed.
                        if matches!(c.op, AccountOp::DeviceAdd { .. })
                            && state.roster.get(&c.subject_device()) == Some(&c.hash())
                        {
                            state.roster.remove(&c.subject_device());
                        }
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
        let mut ordered = idxs;
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
            let outcome = classify_effect(c, &incarnations, &state, &parked);
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
                // A recovery re-root must be signed by a CURRENT pre-contest owner.
                // `authority_status == Live` binds the signer to the cited mint,
                // but liveness alone is too weak: a demoted owner's incarnation
                // stays in `live` (its under-cut ops remain valid) while
                // leaving `owners`. Require the cited incarnation to be the device's OPEN one — a
                // former owner cannot select the successor.
                // Admissible iff signed by a CURRENT pre-contest owner: the cited incarnation must
                // be live (authority_status), name the signer, AND be the device's OPEN incarnation
                // — a demoted former owner cannot select the successor. The op is advisory (the
                // subscriber re-decides trust against the successor genesis), so — per §12's
                // literal rule — it counts regardless of whether it was authored
                // before or after the contest surfaced; the account's transition to
                // `contested` is what makes it admissible.
                AccountOp::AccountReRoot { successor_account_id, .. }
                    if !condemned.contains_key(&c.hash())
                        && matches!(
                            authority_status(c, &incarnations, &state, &parked),
                            AuthorityStatus::Live
                        )
                        && incarnations.author_incarnation_id(c).is_some_and(|inc| {
                            state.owners.get(&c.header().device_fingerprint) == Some(&inc)
                        }) =>
                    Some((c, *successor_account_id)),
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

    AccountAuthHistory { outcomes, classification, contested_successor }
}

/// The genesis candidate: an `AccountGenesis` whose payload hashes to the shared `account_id` (§4)
/// AND whose SIGNER is the founder the id commits to. The header `device_fingerprint` (the signer,
/// [`Candidate::subject_device`] for genesis) MUST equal `sha256(ed25519_pubkey)` from the payload.
///
/// This binding is load-bearing: `account_id` commits to the founder pubkey inside the genesis
/// payload, but the payload is public. Without this check a non-owner could copy the victim's
/// genesis payload verbatim, re-sign it under its OWN device key (so a different
/// `device_fingerprint` still verifies), and be taken for the founder — an account takeover.
/// `DeviceAdd` enforces the same `fingerprint == sha256(pubkey)` binding at decode; genesis carries
/// no fingerprint field, so the fold binds it here. Among valid candidates, pick the smallest
/// `entry_hash` — deterministic, never arrival order (I9) — so two would-be roots can never split
/// consensus.
fn find_genesis(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| match &c.op {
            AccountOp::AccountGenesis { ed25519_pubkey, .. } => {
                let h = c.header();
                // Canonical root header shape (§6): the seq-0 origin of the control chain with NO
                // predecessor / parent / authority and a zero auth_len (genesis is
                // self-authorizing). A malformed same-payload genesis (e.g. a
                // non-null parent_ref) must be excluded, so it can't win the
                // min-hash tiebreak and get the canonical root NonGenesisOrigin'd.
                h.seq == 0
                    && h.log_id == CONTROL_LOG
                    && h.prev_hash.is_none()
                    && h.parent_ref.is_none()
                    && h.authority_ref.is_none()
                    && h.auth_len == 0
                    && account_id_from_genesis_payload(&c.entry.payload) == h.account_id
                    && h.device_fingerprint.to_bytes() == cbor::sha256(ed25519_pubkey)
            },
            _ => false,
        })
        .min_by_key(|c| c.hash())
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
    /// The cited incarnation names the signer but is not live because it was CONDEMNED / rejected —
    /// a permanent `stale_authority`.
    Stale,
    /// The cited incarnation names the signer but is itself PARKED (its own watermark not yet
    /// held). The dependent parks on the same reason, never permanently stale — it heals when
    /// the mint does (I11).
    ParkedAuthorizer(ParkReason),
}

/// The §"authority rule" (clauses 1 + 3): `c`'s cited incarnation must (1) resolve to a mint whose
/// SUBJECT device is the signer, and (3) be live. `AccountGenesis` acts under its own incarnation
/// (self-minted, subject = signer), so it passes trivially once seeded live. A not-live authorizer
/// that is merely PARKED (vs condemned) parks the dependent so it can recover on a later refold.
fn authority_status(
    c: &Candidate,
    incarnations: &Incarnations<'_>,
    state: &FoldState,
    parked: &HashMap<[u8; 32], ParkReason>,
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
    if state.live.contains(&author_inc) {
        return AuthorityStatus::Live;
    }
    match parked.get(&author_inc) {
        Some(reason) => AuthorityStatus::ParkedAuthorizer(*reason),
        None => AuthorityStatus::Stale,
    }
}

/// Classify one op in the effect pass: authority of its author-incarnation, then the state
/// preconditions. Does NOT mutate state (that is [`apply_effect`], only on an effective verdict).
fn classify_effect(
    c: &Candidate,
    incarnations: &Incarnations<'_>,
    state: &FoldState,
    parked: &HashMap<[u8; 32], ParkReason>,
) -> Outcome {
    // The author must act under a LIVE incarnation minted for THIS device (clauses 1 + 3). This is
    // what defeats laundering (a cut authored under a since-condemned owner is not live) AND owner
    // impersonation (a member citing another device's live incarnation — P3-adjacent).
    match authority_status(c, incarnations, state, parked) {
        AuthorityStatus::Unresolvable => return Outcome::Parked(ParkReason::UnknownOwnerRef),
        AuthorityStatus::WrongDevice => return Outcome::Rejected(RejectReason::WrongDevice),
        AuthorityStatus::Stale => return Outcome::Rejected(RejectReason::StaleAuthority),
        // A parked authorizer parks the dependent (recoverable), never permanently stale-rejects
        // it.
        AuthorityStatus::ParkedAuthorizer(reason) => return Outcome::Parked(reason),
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
            } else if state.roster.contains_key(device_fingerprint) {
                Outcome::Rejected(RejectReason::DuplicateAdd)
            } else {
                effective(state)
            }
        },
        AccountOp::OwnerPromote { device_fingerprint } => {
            let enrolled = state.roster.contains_key(device_fingerprint);
            let already_owner = state.owners.contains_key(device_fingerprint);
            let tombstoned = state.tombstoned.contains(device_fingerprint);
            if enrolled && !already_owner && !tombstoned {
                effective(state)
            } else {
                Outcome::Rejected(RejectReason::BadPromote)
            }
        },
        // Stream ownership + grant authority (self-certifying `/2` spec, grant registers, content
        // chains) is folded by the C2 predicate — C1 defers it rather than trust an unvalidated
        // stream op.
        AccountOp::StreamOwn { .. }
        | AccountOp::StreamGrant { .. }
        | AccountOp::StreamRevoke { .. } =>
            Outcome::Parked(ParkReason::DeferredStreamAuthorization),
        // `AccountReRoot` is admissible ONLY as the terminal recovery op once the account is
        // contested (§12) — the contested path admits it. In a `Live` account it has no effect.
        AccountOp::AccountReRoot { .. } => Outcome::Rejected(RejectReason::Ineffective),
        // A DeviceRemove of a device that was never enrolled is ineffective — otherwise it would
        // tombstone a fingerprint that was never added (I4), permanently barring a future
        // legitimate DeviceAdd for it.
        AccountOp::DeviceRemove { device_fingerprint, .. } =>
            if state.roster.contains_key(device_fingerprint) {
                effective(state)
            } else {
                Outcome::Rejected(RejectReason::Ineffective)
            },
        // An OwnerDemote reaching here is an admitted cut op (register + binding decided in the
        // register pass); it is effective.
        AccountOp::OwnerDemote { .. } => effective(state),
        // A CONTROL-log CutExtend reaching here was admitted in the register pass. A
        // secrets/content extend has no control register (its binding is the C2/C4 fold's)
        // — defer it rather than mark it effective on an unvalidated target.
        AccountOp::CutExtend { chain_kind, .. } =>
            if *chain_kind == ChainKind::Ctrl {
                effective(state)
            } else {
                Outcome::Parked(ParkReason::DeferredStreamAuthorization)
            },
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
            state.roster.insert(c.subject_device(), c.hash());
            state.owners.insert(c.subject_device(), c.hash());
            state.live.insert(c.hash());
        },
        AccountOp::DeviceAdd { device_fingerprint, role, .. } => {
            state.roster.insert(*device_fingerprint, c.hash());
            if *role == DeviceRole::Owner {
                state.owners.insert(*device_fingerprint, c.hash());
                state.live.insert(c.hash());
            }
        },
        AccountOp::OwnerPromote { device_fingerprint } => {
            state.owners.insert(*device_fingerprint, c.hash());
            state.live.insert(c.hash());
        },
        // An effective removal tombstones the device (I4: never re-enroll) and drops it from the
        // roster/owner sets. The register it installed handles condemning its beyond-cut entries.
        AccountOp::DeviceRemove { device_fingerprint, .. } => {
            state.roster.remove(device_fingerprint);
            state.owners.remove(device_fingerprint);
            state.tombstoned.insert(*device_fingerprint);
        },
        // A demotion closes ONLY the named incarnation: if the device has since reopened a fresh
        // one (a later OwnerPromote), a stale demote naming the old `owner_id` is a no-op.
        AccountOp::OwnerDemote { device_fingerprint, owner_id, .. }
            if state.owners.get(device_fingerprint) == Some(owner_id) =>
        {
            state.owners.remove(device_fingerprint);
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
    fn stream_ops_are_deferred_to_c2_not_folded_as_authority() {
        // Stream ownership + grant authority (the self-certifying `/2` spec, grant registers,
        // content chains) is folded by the C2 predicate, not the C1 control fold. So StreamOwn /
        // StreamGrant establish NO authority here — they park, and C1 trusts nothing from an
        // unvalidated stream op (P11's floor: a grant is never effective before its owner because
        // neither is ever effective in C1).
        let fdr = Dev::new(1);
        let stream = StreamId::from_bytes([0x33; 32]);
        let grantee = AccountId::from_bytes([0x44; 32]);
        let mut f = Fixture::genesis(&fdr);
        let own = f.author(&fdr, Some(f.genesis_hash), &stream_own(stream));
        let grant = f.author(&fdr, Some(f.genesis_hash), &stream_grant(stream, grantee));

        let h = f.fold();
        for (op, label) in [(own, "StreamOwn"), (grant, "StreamGrant")] {
            assert_eq!(
                h.outcome(&op),
                Some(Outcome::Parked(ParkReason::DeferredStreamAuthorization)),
                "{label} is deferred to C2, never authority-bearing in C1",
            );
        }
    }

    #[test]
    fn a_cross_depth_cut_extend_does_not_re_bless_within_a_fold() {
        // §11.1 is per-depth monotone: condemnation grows depth by depth and a lower-depth decision
        // is final. A CutExtend re-blesses its cone ONLY when joined at the CREATOR's depth (the
        // same-depth `⊔`, covered by `cut_extend_reblesses_a_condemned_cone_p7`). Here F removes B
        // at depth 0 (condemning b1/b2) and a DEEPER owner A (depth 1) extends the cut — because
        // A's extend lands a stratum later than the depth-0 condemnation, the cone stays
        // condemned in this fold. (A global recompute would fix this but could preserve a
        // register whose creator the fold later condemns — a laundering hole — so we stay
        // faithful to the per-depth model.)
        let (fdr, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let (d, e, g) = (Dev::new(5), Dev::new(6), Dev::new(7));
        let mut f = Fixture::genesis(&fdr);
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let b0 = f.author(&b, Some(add_b), &device_add(&d, DeviceRole::Member));
        let b1 = f.author(&b, Some(add_b), &device_add(&e, DeviceRole::Member));
        let b2 = f.author(&b, Some(add_b), &device_add(&g, DeviceRole::Member));
        f.author(&fdr, Some(f.genesis_hash), &device_remove(&b, Cut::At { seq: 0, hash: b0 }));
        let extend = f.author(&a, Some(add_a), &cut_extend_ctrl(f.account_id, &b, None, 2, b2));

        let h = f.fold();
        assert!(h.is_effective(&b0), "the within-cut prefix is effective");
        assert!(h.is_effective(&extend), "the extend itself is a valid owner op");
        assert_eq!(
            h.outcome(&b1),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
            "a deeper-depth extend does not re-bless a shallower depth's condemnation",
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
            Some(f.genesis_hash),
            &account_reroot(AccountId::from_bytes([0x55; 32])),
        );

        let h = f.fold();
        assert_eq!(h.classification(), AccountClassification::Live);
        assert_eq!(h.outcome(&reroot), Some(Outcome::Rejected(RejectReason::Ineffective)));
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
            Some(f.genesis_hash),
            &device_remove(&fdr, Cut::At { seq: 0, hash: f.genesis_hash }),
        );

        let h = f.fold();
        assert!(h.is_effective(&f.genesis_hash), "genesis stands — the founder remains an owner");
        assert!(!h.is_effective(&remove_self), "a device cannot remove its own chain");
    }

    #[test]
    fn a_stale_owner_demote_does_not_close_a_reopened_incarnation() {
        // B is demoted (g1), then re-promoted (g2). A stale OwnerDemote naming the OLD incarnation
        // g1 must not close B's fresh g2 — the demote is scoped to its exact incarnation.
        let (fdr, b, t) = (Dev::new(1), Dev::new(2), Dev::new(5));
        let mut f = Fixture::genesis(&fdr);
        let g1 = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        f.author(&fdr, Some(f.genesis_hash), &owner_demote(&b, g1, Cut::Empty));
        let g2 = f.author(&fdr, Some(f.genesis_hash), &owner_promote(&b));
        // A late, stale demote of the OLD incarnation g1.
        f.author(&fdr, Some(f.genesis_hash), &owner_demote(&b, g1, Cut::Empty));
        // B authors under the fresh incarnation g2.
        let under_g2 = f.author(&b, Some(g2), &device_add(&t, DeviceRole::Member));

        let h = f.fold();
        assert!(h.is_effective(&g2), "the re-promotion mints a fresh incarnation");
        assert!(
            h.is_effective(&under_g2),
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        f.author(&fdr, Some(f.genesis_hash), &device_add(&m, DeviceRole::Member));
        f.author(&a, Some(add_a), &device_remove(&b, Cut::Empty));
        f.author(&b, Some(add_b), &device_remove(&a, Cut::Empty));
        let bad_reroot =
            f.author(&m, Some(add_a), &account_reroot(AccountId::from_bytes([0x11; 32])));

        let h = f.fold();
        assert!(matches!(h.classification(), AccountClassification::Contested { .. }));
        assert!(!h.is_effective(&bad_reroot), "a non-owner cannot select the recovery successor");
        assert_eq!(h.contested_successor(), None, "no owner re-rooted ⇒ no successor");
    }

    #[test]
    fn a_demoted_former_owner_cannot_select_the_contested_successor() {
        // A is demoted (its incarnation leaves `owners` but stays in `live` — its under-cut history
        // is still valid). Owners B and C then cut each other ⇒ contested. Both a CURRENT owner (F)
        // and the demoted A submit re-roots; only F's is admitted. Even though A picked the smaller
        // successor id, the deterministic successor is F's — a former owner cannot select it.
        let (fdr, a, b, c) = (Dev::new(1), Dev::new(2), Dev::new(3), Dev::new(4));
        let (succ_a, succ_f) =
            (AccountId::from_bytes([0x11; 32]), AccountId::from_bytes([0x22; 32]));
        let mut f = Fixture::genesis(&fdr);
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let add_c = f.author(&fdr, Some(f.genesis_hash), &device_add(&c, DeviceRole::Owner));
        f.author(&fdr, Some(f.genesis_hash), &owner_demote(&a, add_a, Cut::Empty));
        f.author(&b, Some(add_b), &device_remove(&c, Cut::Empty));
        f.author(&c, Some(add_c), &device_remove(&b, Cut::Empty));
        let reroot_f = f.author(&fdr, Some(f.genesis_hash), &account_reroot(succ_f));
        let reroot_a = f.author(&a, Some(add_a), &account_reroot(succ_a));

        let h = f.fold();
        assert!(matches!(h.classification(), AccountClassification::Contested { .. }));
        assert!(h.is_effective(&reroot_f), "a current owner's re-root is admitted");
        assert!(!h.is_effective(&reroot_a), "a demoted former owner's re-root is not admitted");
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
        let payload = encode(&op).unwrap();
        let account_id = account_id_from_genesis_payload(&payload);
        let genesis_header = |signer: &Dev| AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: signer.fp,
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
        let signed = |signer: &Dev, hdr: &AccountEntryHeader, pl: &[u8]| {
            let s = sign_account_entry(&signer.secret, hdr, pl).unwrap();
            verify_account_signed(&s.signed_bytes, &signer.secret.public()).unwrap()
        };
        // Both genesis entries carry the SAME (victim) payload; only the signer differs.
        let real = signed(&victim, &genesis_header(&victim), &payload);
        let forged = signed(&attacker, &genesis_header(&attacker), &payload);
        // The attacker adds itself as owner, citing its forged genesis.
        let add_op = device_add(&x, DeviceRole::Owner);
        let add_payload = encode(&add_op).unwrap();
        let add_header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: attacker.fp,
            seq: 1,
            prev_hash: Some(forged.entry_hash),
            parent_ref: Some(forged.entry_hash),
            entry_type: entry_type_of(&add_op),
            op_version: 1,
            auth_len: 1,
            crypto_suite: 0,
            key_id: None,
            authority_ref: Some(forged.entry_hash),
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
        f.author(&fdr, Some(f.genesis_hash), &device_remove(&fdr, Cut::Empty));

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
        let payload = encode(&op).unwrap();
        let header = AccountEntryHeader {
            account_id: f.account_id,
            log_id: 1, // secrets log — not the control log
            device_fingerprint: fdr.fp,
            seq: 1,
            prev_hash: Some(f.genesis_hash),
            parent_ref: Some(f.genesis_hash),
            entry_type: entry_type_of(&op),
            op_version: 1,
            auth_len: 1,
            crypto_suite: 0,
            key_id: None,
            authority_ref: Some(f.genesis_hash),
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
        let payload = encode(&op).unwrap();
        let header = AccountEntryHeader {
            account_id: f.account_id,
            log_id: 0,
            device_fingerprint: fdr.fp,
            seq: 1,
            prev_hash: Some(f.genesis_hash),
            parent_ref: Some(f.genesis_hash),
            entry_type: entry_type_of(&op),
            op_version: 2, // future version
            auth_len: 1,
            crypto_suite: 0,
            key_id: None,
            authority_ref: Some(f.genesis_hash),
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
        let remove = f.author(&fdr, Some(f.genesis_hash), &device_remove(&ghost, Cut::Empty));
        let add = f.author(&fdr, Some(f.genesis_hash), &device_add(&ghost, DeviceRole::Member));

        let h = f.fold();
        assert_eq!(
            h.outcome(&remove),
            Some(Outcome::Rejected(RejectReason::Ineffective)),
            "removing a never-enrolled device is ineffective",
        );
        assert!(h.is_effective(&add), "the device is not pre-tombstoned, so it can still be added");
    }

    #[test]
    fn a_deep_incarnation_chain_folds_without_a_stack_overflow() {
        // `incarnation_depth` walks the authority_ref chain ITERATIVELY. Its depth is computed for
        // every candidate BEFORE any authority check, and chain length is adversary-controlled, so
        // a deep chain must fold to a classification, never recurse to a stack overflow.
        // Delivered DEEPEST-FIRST (the crash-inducing order) and folded on a SMALL stack,
        // so a recursive regression would abort loudly here.
        const N: u32 = 800;
        let founder = Dev::seeded(0);
        let mut f = Fixture::genesis(&founder);
        let genesis_hash = f.genesis_hash;
        let mut g = genesis_hash;
        let mut devs = vec![founder];
        for k in 1..=N {
            let dev = Dev::seeded(k);
            g = f.author(&devs[(k - 1) as usize], Some(g), &device_add(&dev, DeviceRole::Owner));
            devs.push(dev);
        }
        let mut entries = f.entries.clone();
        entries.reverse(); // deepest-first — memoization can't keep the recursion shallow

        // A 64 KiB stack fits the iterative fold's constant call depth but would overflow an
        // N-deep recursion many times over.
        let effective = std::thread::Builder::new()
            .stack_size(64 * 1024)
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let mut doubled = f.entries.clone();
        doubled.extend(f.entries.clone());

        let h = fold_account(&doubled);
        assert!(h.is_effective(&f.genesis_hash));
        assert!(
            h.is_effective(&add_b),
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
        let payload = encode(&op).unwrap();
        let account_id = account_id_from_genesis_payload(&payload);
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: Some([0x01; 32]), // a root has no parent
            entry_type: entry_type::ACCOUNT_GENESIS,
            op_version: 1,
            auth_len: 0,
            crypto_suite: 0,
            key_id: None,
            authority_ref: None,
        };
        let signed = sign_account_entry(&founder.secret, &header, &payload).unwrap();
        let malformed =
            verify_account_signed(&signed.signed_bytes, &founder.secret.public()).unwrap();
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
        let payload = encode(&op).unwrap();
        let header = AccountEntryHeader {
            account_id: f.account_id,
            log_id: 0,
            device_fingerprint: fdr.fp, // the founder's device
            seq: 0,
            prev_hash: None,
            parent_ref: Some(f.genesis_hash),
            entry_type: entry_type_of(&op),
            op_version: 1,
            auth_len: 1,
            crypto_suite: 0,
            key_id: None,
            authority_ref: Some(f.genesis_hash),
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
            entry_type: entry_type::DEVICE_ADD,
            op_version: 1,
            auth_len: 1,
            crypto_suite: 1,          // sealed
            key_id: Some([0x33; 32]), // required when crypto_suite != 0
            authority_ref: Some(f.genesis_hash),
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
            entry_type: entry_type::DEVICE_ADD,
            op_version: 1,
            auth_len: 1,
            crypto_suite: 0,
            key_id: None,
            authority_ref: Some(f.genesis_hash),
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
        let payload = encode(&op).unwrap(); // valid DeviceAdd bytes, but the header marks it sealed
        let header = AccountEntryHeader {
            account_id: f.account_id,
            log_id: 0,
            device_fingerprint: fdr.fp,
            seq: 1,
            prev_hash: Some(f.genesis_hash),
            parent_ref: Some(f.genesis_hash),
            entry_type: entry_type_of(&op),
            op_version: 1,
            auth_len: 1,
            crypto_suite: 1,
            key_id: Some([0x33; 32]), // required when crypto_suite != 0
            authority_ref: Some(f.genesis_hash),
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
    fn a_non_control_cut_extend_is_deferred_not_effective() {
        // A secrets/content `CutExtend` has no control register (its binding is the C2/C4 fold's),
        // so it must be deferred, never marked effective on an unvalidated target.
        let fdr = Dev::new(1);
        let mut f = Fixture::genesis(&fdr);
        let op = AccountOp::CutExtend {
            chain_kind: ops::ChainKind::Secrets,
            stream_id: None,
            incarnation_id: None,
            subject_account_id: f.account_id,
            device_fingerprint: fdr.fp,
            new_seq: 3,
            new_entry_hash: [0x44; 32],
        };
        let extend = f.author(&fdr, Some(f.genesis_hash), &op);

        let h = f.fold();
        assert_eq!(
            h.outcome(&extend),
            Some(Outcome::Parked(ParkReason::DeferredStreamAuthorization)),
            "a non-control CutExtend is deferred, not effective",
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
            (
                Outcome::Parked(ParkReason::DeferredStreamAuthorization),
                ("parked", Some("deferred_stream_authorization")),
            ),
            (
                Outcome::Rejected(RejectReason::StaleAuthority),
                ("rejected", Some("stale_authority")),
            ),
            (
                Outcome::Rejected(RejectReason::GenesisSelfHash),
                ("rejected", Some("genesis_self_hash")),
            ),
            (
                Outcome::Rejected(RejectReason::DuplicateGenesis),
                ("rejected", Some("duplicate_genesis")),
            ),
            (Outcome::Rejected(RejectReason::DuplicateAdd), ("rejected", Some("duplicate_add"))),
            (
                Outcome::Rejected(RejectReason::TombstoneReAdd),
                ("rejected", Some("tombstone_re_add")),
            ),
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
            (Outcome::Rejected(RejectReason::Ineffective), ("rejected", Some("ineffective"))),
        ];
        for (outcome, expected) in cases {
            assert_eq!(outcome.taxonomy(), expected, "taxonomy drift for {outcome:?}");
        }
    }
}
