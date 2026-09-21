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

// Pure v2 execution is isolated until the account activation path is integrated.
#[allow(dead_code, reason = "control v2 execution is not enabled in production (#1311)")]
pub(super) mod v2;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::ControlFlow;

use super::AccountId;
use super::branch::{AncestryRelation, UnknownAncestry};
use super::candidate::{self, CutCoordinate, HeaderView, JoinResult};
use super::cut::{Cut, beyond};
use super::envelope::{AccountEntryHeader, VerifiedAccountEntry};
use super::id::{self, AccountEntryHash, GrantId, OwnerId, RosterRef};
use super::ops::{self, AccountOp, ChainKind, DecodedAccountOp, DeviceCut, DeviceRole, GrantRole};
use super::registers::RegisterKey;
use crate::cbor;
use crate::op::DeviceFingerprint;
use crate::stream::{self, StreamId};

/// The account CONTROL log the fold operates on (§11) — its authority-minting ops (genesis, adds,
/// promotes/demotes, removes, cut extends) live here. A known op on the secrets (1) or content (2)
/// log is not a control op and is retained unfolded here (its own C2/C4 fold owns it), never
/// minting control authority.
pub(super) const CONTROL_LOG: u8 = 0;
/// The account SECRETS log (§11). Control ops never fold here, but a `DeviceRemove`/`OwnerDemote`
/// on the control log CARRIES a `secrets_cut` that installs a revocation register scoped to this
/// log, and a `CutExtend { chain_kind: Secrets }` raises it — the same register machinery the
/// control chain uses, keyed at `log: SECRETS_LOG` instead of `log: CONTROL_LOG`.
pub(super) const SECRETS_LOG: u8 = 1;
/// The account ANNEX log — authority-inert bookkeeping artifacts (C6 snapshots, #609). It is 3, not
/// 2: `ChainKind::Content = 2` already names the content chain on the register/cut axis, so a
/// `CutExtend { chain_kind: Content }` means "log 2" and a second meaning for that number would be
/// ambiguous.
///
/// Nothing here ever folds. Entries on this log are stored, retained header-only, and can never
/// mint authority, shift `effective_count`, or enter control-chain branch selection — that
/// inertness is TOPOLOGICAL (the `foldable` gate below short-circuits on `log_id`, before any tag
/// dispatch), not a property some future arm has to remember to preserve. That is precisely why an
/// authority-inert artifact must not ride the control log: a never-effective entry in a control
/// chain orphans every later entry from that device (#809).
pub(super) const ANNEX_LOG: u8 = 3;
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

/// The §16.3 stored status of an account-log entry — `account_entry_status.status`, beside its
/// optional `detail` reason. These are persisted tokens: a rename needs a migration exactly like a
/// column rename. The strings are pinned by `every_fold_outcome_has_a_stable_storage_taxonomy`
/// and the secrets `status_pairs_cover_every_frozen_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum EntryStatus {
    /// The accepted-slot winner (the storage layer resolves `effective` into accepted vs forked).
    Accepted,
    /// An effective entry that lost its accepted slot to an equivocating sibling.
    Forked,
    Effective,
    RetainedUnfolded,
    Condemned,
    Parked,
    Rejected,
}

impl EntryStatus {
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value
            .parse()
            .map_err(|_| anyhow::anyhow!("unknown persisted account entry status `{value}`"))
    }
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
    pub(super) fn taxonomy(&self) -> (EntryStatus, Option<&'static str>) {
        match self {
            Outcome::Effective { .. } => (EntryStatus::Effective, None),
            Outcome::RetainedUnfolded => (EntryStatus::RetainedUnfolded, None),
            Outcome::Condemned(reason) => (EntryStatus::Condemned, Some(reason.into())),
            Outcome::Parked(reason) => (EntryStatus::Parked, Some(reason.into())),
            Outcome::Rejected(reason) => (EntryStatus::Rejected, Some(reason.into())),
        }
    }
}

/// Why an entry was killed by a revocation register (§11.2). `BeyondCut` is seq-only (I11) and
/// RECOVERABLE (a later `CutExtend` re-blesses); `OffBranch` is the permanent equivocation-loser
/// class (L2); `ClosedIncarnation` is a mint whose own authorizing incarnation was condemned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum CondemnedReason {
    BeyondCut,
    OffBranch,
    ClosedIncarnation,
}

/// A control op that will never be effective — a permanent state precondition failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
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
    /// A `StreamOwn` preimage is not the canonical owner-bound `/2` spec named by its account and
    /// `stream_id`.
    InvalidStreamSpec,
    /// A duplicate / no-op / self-referential op with no effect — incl. an `AccountReRoot` in a
    /// `Live` account (admissible only as the terminal recovery op once contested, §12).
    Ineffective,
}

/// A control op undecided until more entries arrive (a *withheld* input parks, never flips — I11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
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
    /// The entry asserts a control-fold length not yet present locally. This is recoverable:
    /// refetch missing control ancestry and refold; the counter never grants authority. A revoking
    /// cut is measured with the entries it removed credited back ([`revocation_credit`]).
    AuthLenAhead,
    /// A secrets/content cut whose target register belongs to a later phase.
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
///
/// `Clone` so a checkpoint's frozen history can be projected by a pinned refold without re-folding
/// the evidence it was derived from.
#[derive(Clone)]
pub(super) struct AccountAuthHistory {
    outcomes: HashMap<AccountEntryHash, Outcome>,
    classification: AccountClassification,
    /// In a `contested` account, the deterministic `AccountReRoot` successor a subscriber follows
    /// — the smallest `successor_account_id` by byte order among the admitted re-roots (§12).
    /// `None` when the account is `Live` or no pre-contest owner has re-rooted yet.
    contested_successor: Option<AccountId>,
    effective_count: u64,
    roster_refs: HashMap<RosterRef, RosterFact>,
    owner_incarnations: HashMap<OwnerId, OwnerIncarnationFact>,
    stream_ownership: HashMap<StreamId, StreamOwnershipFact>,
    grants: HashMap<GrantId, GrantFact>,
    grant_cuts: HashMap<GrantId, Vec<DeviceCut>>,
    /// Removed devices (I4: never re-enroll). Exported because the C6 canonical projection binds
    /// it: a snapshot that omitted tombstones would let a bootstrap re-admit a removed device.
    tombstoned: HashSet<DeviceFingerprint>,
    /// The CANONICAL root, as selected by [`find_genesis`] — not merely the first entry carrying
    /// the genesis tag. Exported because C6 signs snapshots with it as `parent_ref`: a malformed
    /// same-payload genesis (a non-null `parent_ref`, say) can be held alongside the real root and
    /// sort ahead of it by hash, and no snapshot read revalidates `parent_ref`. `None` when no
    /// valid genesis is held yet.
    genesis_hash: Option<AccountEntryHash>,
}

/// One authority fact resolved against the CURRENT fold. There is exactly one snapshot to resolve
/// against — `auth_len` selects no historical view (§7: it is never an authority input), so a fact
/// query answers from what we have folded and says nothing about the author's own control length.
/// Freshness is a separate axis ([`AuthorityFreshness`]) the caller applies in its own phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityQuery<T> {
    Effective(T),
    /// The citation names an entry our fold does not hold. Recoverable: refetch and re-evaluate.
    Unknown,
    Invalid(AuthorityInvalidReason),
}

/// The author's asserted control-fold length measured against ours (§7). `auth_len` is never an
/// authority input; it only tells us whether the author folded ops we have not. Ahead ⇒ park +
/// refetch, never a rejection — the missing ops are recoverable and may still re-bless a citation
/// our fold currently reads as ineffective (§11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityFreshness {
    /// The author cites a control log no longer than the one we hold.
    CurrentOrBehind,
    /// The author cites a control log longer than the one we hold.
    Ahead,
}

impl AuthorityFreshness {
    /// A cited control-log length measured against a held log of `held` rows.
    pub fn of(asserted_auth_len: u64, held: u64) -> Self {
        if asserted_auth_len > held { Self::Ahead } else { Self::CurrentOrBehind }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityInvalidReason {
    /// The cited entry's own header names a different subject than the citation claims. Decided by
    /// bytes we already hold, so a deeper control log can never overturn it.
    WrongSubject,
    /// The cited entry is held but our fold reads it as ineffective. This one is FOLD-DEPENDENT: a
    /// `CutExtend` we have not folded can re-bless it (§11.4), so a caller that is behind the
    /// author ([`AuthorityFreshness::Ahead`]) must park on it rather than treat it as a lie.
    ReferencedEntryNotEffective,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RosterAuthority {
    pub device_fingerprint: DeviceFingerprint,
    /// Current roster metadata, not authority for an owner-required operation. Owner authority is
    /// established only by citing a fresh `owner_id` and applying both revocation registers.
    pub current_role: DeviceRole,
}

/// The valid prefix of a cited device chain. `Closed` is the empty cut: no entry on that chain is
/// admissible. Callers must still verify ancestry for `Cut`; the hash prevents an equal/older
/// off-branch entry from laundering through a sequence-only check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityBoundary {
    Open,
    Cut { seq: u64, hash: AccountEntryHash },
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RosterContentAuthority {
    pub device_fingerprint: DeviceFingerprint,
    /// The device's roster role, carried so the content gate can reject a device that is on the
    /// roster but not permitted to author content (a `ReadOnly` device). Without this the gate
    /// would see only membership + boundary and admit a read-only device's content.
    pub role: DeviceRole,
    pub boundary: AuthorityBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerAuthority {
    pub device_fingerprint: DeviceFingerprint,
}

/// Owner-required entries are admitted by the conjunction of these two independent registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerChainAuthority {
    pub owner: OwnerAuthority,
    pub device_boundary: AuthorityBoundary,
    pub incarnation_boundary: AuthorityBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantAuthority {
    pub stream_id: StreamId,
    pub grantee_account_id: AccountId,
    pub role: GrantRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantDeviceAuthority {
    pub grant: GrantAuthority,
    pub boundary: GrantDeviceBoundary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantDeviceBoundary {
    Open,
    Cut(DeviceCut),
    /// The grant is revoked and this device was not named in its prefix-preserving cuts. A fresh
    /// or unlisted device gets the empty register: no content entry is admissible.
    Closed,
}

#[derive(Clone)]
pub(super) struct RosterFact {
    pub(super) authority: RosterAuthority,
    pub(super) effective_at: u64,
    pub(super) closed_at: Option<u64>,
    pub(super) control_boundary: AuthorityBoundary,
    pub(super) secrets_boundary: AuthorityBoundary,
    pub(super) content_boundaries: HashMap<StreamId, AuthorityBoundary>,
}

#[derive(Clone, Copy)]
pub(super) struct OwnerIncarnationFact {
    pub(super) authority: OwnerAuthority,
    pub(super) effective_at: u64,
    pub(super) closed_at: Option<u64>,
    pub(super) control_boundary: AuthorityBoundary,
    pub(super) secrets_boundary: AuthorityBoundary,
}

#[derive(Clone, Copy)]
pub(super) struct StreamOwnershipFact {
    pub(super) own_id: AccountEntryHash,
    pub(super) effective_at: u64,
}

#[derive(Clone, Copy)]
pub(super) struct GrantFact {
    pub(super) authority: GrantAuthority,
    pub(super) effective_at: u64,
    pub(super) closed_at: Option<u64>,
}

impl AccountAuthHistory {
    /// The outcome of the entry with this hash (absent ⇒ the entry was not in the folded set).
    pub(super) fn outcome(&self, entry_hash: &AccountEntryHash) -> Option<Outcome> {
        self.outcomes.get(entry_hash).copied()
    }

    /// The canonical root hash — see the field docs. Callers signing `parent_ref` MUST use this
    /// rather than scanning held entries for the genesis tag themselves.
    pub(super) fn genesis_hash(&self) -> Option<AccountEntryHash> {
        self.genesis_hash
    }

    pub(super) fn classification(&self) -> AccountClassification {
        self.classification
    }

    /// The deterministic recovery successor for a `contested` account (§12), if one exists.
    pub(super) fn contested_successor(&self) -> Option<AccountId> {
        self.contested_successor
    }

    pub(super) fn effective_count(&self) -> u64 {
        self.effective_count
    }

    pub(super) fn roster_facts(&self) -> impl Iterator<Item = (&RosterRef, &RosterFact)> {
        self.roster_refs.iter()
    }

    pub(super) fn owner_incarnation_facts(
        &self,
    ) -> impl Iterator<Item = (&OwnerId, &OwnerIncarnationFact)> {
        self.owner_incarnations.iter()
    }

    pub(super) fn stream_ownership_facts(
        &self,
    ) -> impl Iterator<Item = (&StreamId, &StreamOwnershipFact)> {
        self.stream_ownership.iter()
    }

    pub(super) fn grant_facts(&self) -> impl Iterator<Item = (&GrantId, &GrantFact)> {
        self.grants.iter()
    }

    /// Every EFFECTIVE entry with the `auth_epoch` it took, in arbitrary order. The C6 canonical
    /// projection sorts this; callers must not depend on iteration order (it is a `HashMap`).
    pub(super) fn effective_entries(&self) -> impl Iterator<Item = (AccountEntryHash, u64)> + '_ {
        self.outcomes.iter().filter_map(|(hash, outcome)| match outcome {
            Outcome::Effective { auth_epoch } => Some((*hash, *auth_epoch)),
            _ => None,
        })
    }

    /// Removed devices (I4). Arbitrary order — see [`Self::effective_entries`].
    pub(super) fn tombstoned(&self) -> impl Iterator<Item = &DeviceFingerprint> {
        self.tombstoned.iter()
    }

    pub(super) fn grant_cuts(&self) -> impl Iterator<Item = (&GrantId, &[DeviceCut])> {
        self.grant_cuts.iter().map(|(grant_id, cuts)| (grant_id, cuts.as_slice()))
    }

    /// Measure an asserted control-fold length against ours (§7). This is the ONE seam that reads
    /// `auth_len`; the fact queries below never see it, so an ahead counter can neither select a
    /// historical authority view nor pre-empt a verdict the current fold already decides.
    pub(super) fn auth_len_freshness(&self, asserted_auth_len: u64) -> AuthorityFreshness {
        if asserted_auth_len > self.effective_count {
            AuthorityFreshness::Ahead
        } else {
            AuthorityFreshness::CurrentOrBehind
        }
    }

    pub(super) fn roster_ref_effective(
        &self,
        roster_ref: RosterRef,
        device_fingerprint: DeviceFingerprint,
    ) -> AuthorityQuery<RosterAuthority> {
        query_fact(
            self.roster_refs
                .get(&roster_ref)
                .filter(|fact| fact.closed_at.is_none())
                .map(|fact| (fact.authority, fact.authority.device_fingerprint)),
            device_fingerprint,
            self.outcomes.contains_key(&roster_ref.into()),
        )
    }

    pub(super) fn roster_content_authority(
        &self,
        roster_ref: RosterRef,
        device_fingerprint: DeviceFingerprint,
        stream_id: StreamId,
    ) -> AuthorityQuery<RosterContentAuthority> {
        let fact = match resolve_fact(
            self.roster_refs.get(&roster_ref),
            self.outcomes.contains_key(&roster_ref.into()),
        ) {
            Ok(fact) => fact,
            Err(verdict) => return verdict,
        };
        if let Err(verdict) = require_subject(fact.authority.device_fingerprint, device_fingerprint)
        {
            return verdict;
        }
        let boundary = fact.content_boundaries.get(&stream_id).copied().unwrap_or_else(|| {
            if fact.closed_at.is_none() {
                AuthorityBoundary::Open
            } else {
                AuthorityBoundary::Closed
            }
        });
        AuthorityQuery::Effective(RosterContentAuthority {
            device_fingerprint: fact.authority.device_fingerprint,
            role: fact.authority.current_role,
            boundary,
        })
    }

    pub(super) fn owner_incarnation_effective(
        &self,
        owner_id: OwnerId,
        device_fingerprint: DeviceFingerprint,
    ) -> AuthorityQuery<OwnerAuthority> {
        query_fact(
            self.owner_incarnations
                .get(&owner_id)
                .filter(|fact| fact.closed_at.is_none())
                .map(|fact| (fact.authority, fact.authority.device_fingerprint)),
            device_fingerprint,
            self.outcomes.contains_key(&owner_id.into()),
        )
    }

    pub(super) fn owner_control_authority(
        &self,
        owner_id: OwnerId,
        device_fingerprint: DeviceFingerprint,
    ) -> AuthorityQuery<OwnerChainAuthority> {
        self.owner_chain_authority(owner_id, device_fingerprint, AuthorityChain::Control)
    }

    pub(super) fn owner_secrets_authority(
        &self,
        owner_id: OwnerId,
        device_fingerprint: DeviceFingerprint,
    ) -> AuthorityQuery<OwnerChainAuthority> {
        self.owner_chain_authority(owner_id, device_fingerprint, AuthorityChain::Secrets)
    }

    fn owner_chain_authority(
        &self,
        owner_id: OwnerId,
        device_fingerprint: DeviceFingerprint,
        chain: AuthorityChain,
    ) -> AuthorityQuery<OwnerChainAuthority> {
        let owner = match resolve_fact(
            self.owner_incarnations.get(&owner_id),
            self.outcomes.contains_key(&owner_id.into()),
        ) {
            Ok(fact) => fact,
            Err(verdict) => return verdict,
        };
        if let Err(verdict) =
            require_subject(owner.authority.device_fingerprint, device_fingerprint)
        {
            return verdict;
        }
        let device = self
            .roster_refs
            .values()
            .find(|fact| fact.authority.device_fingerprint == device_fingerprint)
            .map_or(AuthorityBoundary::Closed, |fact| fact.boundary(chain));
        AuthorityQuery::Effective(OwnerChainAuthority {
            owner: owner.authority,
            device_boundary: device,
            incarnation_boundary: owner.boundary(chain),
        })
    }

    pub(super) fn grant_effective(
        &self,
        grant_id: GrantId,
        stream_id: StreamId,
        grantee_account_id: AccountId,
    ) -> AuthorityQuery<GrantAuthority> {
        let fact = match resolve_fact(
            self.grants.get(&grant_id),
            self.outcomes.contains_key(&grant_id.into()),
        ) {
            Ok(fact) => fact,
            Err(verdict) => return verdict,
        };
        if let Err(verdict) = require_subject(
            (fact.authority.stream_id, fact.authority.grantee_account_id),
            (stream_id, grantee_account_id),
        ) {
            return verdict;
        }
        AuthorityQuery::Effective(fact.authority)
    }

    pub(super) fn stream_owner_effective(
        &self,
        stream_id: StreamId,
    ) -> AuthorityQuery<AccountEntryHash> {
        let Some(fact) = self.stream_ownership.get(&stream_id) else {
            return AuthorityQuery::Unknown;
        };
        AuthorityQuery::Effective(fact.own_id)
    }

    fn is_effective(&self, entry_hash: &AccountEntryHash) -> bool {
        self.outcome(entry_hash).is_some_and(|o| o.is_effective())
    }
}

fn query_fact<T: Copy, S: PartialEq>(
    fact: Option<(T, S)>,
    expected_subject: S,
    reference_is_known: bool,
) -> AuthorityQuery<T> {
    let (authority, subject) = match resolve_fact(fact.as_ref(), reference_is_known) {
        Ok(fact) => fact,
        Err(verdict) => return verdict,
    };
    if let Err(verdict) = require_subject(subject, &expected_subject) {
        return verdict;
    }
    AuthorityQuery::Effective(*authority)
}

fn resolve_fact<F, T>(fact: Option<&F>, reference_is_known: bool) -> Result<&F, AuthorityQuery<T>> {
    fact.ok_or_else(|| {
        if reference_is_known {
            AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective)
        } else {
            AuthorityQuery::Unknown
        }
    })
}

fn require_subject<T, S: PartialEq>(actual: S, expected: S) -> Result<(), AuthorityQuery<T>> {
    if actual == expected {
        Ok(())
    } else {
        Err(AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject))
    }
}

/// A structurally-valid, signature-valid candidate the fold considers: the verified entry + its
/// decoded KNOWN op. (Unknown ops classify `RetainedUnfolded` and are never folded.)
#[derive(Clone, Debug)]
pub(in crate::account) struct Candidate {
    entry: VerifiedAccountEntry,
    op: AccountOp,
}

impl Candidate {
    /// Pair a verified entry with the KNOWN op it carries. Control v2 entries wrap the same v1
    /// operation grammar, so its executor builds candidates the fold's own rules can read.
    pub(in crate::account) fn new(entry: VerifiedAccountEntry, op: AccountOp) -> Self {
        Self { entry, op }
    }

    pub(in crate::account) fn hash(&self) -> AccountEntryHash {
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
    mints: HashMap<OwnerId, &'a Candidate>,
    genesis_owner_id: OwnerId,
    /// Memoized structural depth per owner_id.
    depth: HashMap<OwnerId, Option<usize>>,
}

impl<'a> Incarnations<'a> {
    fn build(candidates: &'a [Candidate], genesis_owner_id: OwnerId) -> Self {
        let mints =
            candidates.iter().filter(|c| c.is_mint()).map(|c| (c.hash().into(), c)).collect();
        Incarnations { mints, genesis_owner_id, depth: HashMap::new() }
    }

    /// Resolve an `owner_id` to its mint candidate (account-local).
    fn candidate(&self, owner_id: &OwnerId) -> Option<&'a Candidate> {
        self.mints.get(owner_id).copied()
    }

    /// The structural depth of an incarnation: genesis = 0; else 1 + the depth of the incarnation
    /// the minting op's author cited. `None` if the citation chain is unresolvable in this
    /// account (cross-account — P3 — or not yet synced). Memoized with an in-progress guard
    /// (the DAG cannot cycle — L1 — but a corrupt set must not loop).
    fn incarnation_depth(&mut self, owner_id: OwnerId) -> Option<usize> {
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
        let mut chain: Vec<OwnerId> = Vec::new();
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
    fn author_incarnation_id(&self, e: &Candidate) -> Option<OwnerId> {
        match e.op {
            AccountOp::AccountGenesis { .. } => Some(e.hash().into()),
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
    live: HashSet<OwnerId>,
    /// Each enrolled device → the entry_hash of the DeviceAdd / genesis that enrolled it. Keyed by
    /// SOURCE (not just presence) so condemning a superseded / duplicate add for a device does not
    /// erase the enrollment a DIFFERENT, still-valid add contributed.
    roster: HashMap<DeviceFingerprint, RosterRef>,
    /// Immutable role granted by the effective enrollment entry. `OwnerPromote` is deliberately
    /// limited to authoring-capable enrollments: otherwise a later promotion could retroactively
    /// re-bless content a read-only device authored before it had write authority.
    enrollment_roles: HashMap<DeviceFingerprint, DeviceRole>,
    /// Each device holding an OPEN owner incarnation → that incarnation's `owner_id`. Keyed by
    /// incarnation (not just device) so a stale `OwnerDemote` naming a since-superseded `owner_id`
    /// cannot close a device's freshly-reopened incarnation.
    owners: HashMap<DeviceFingerprint, OwnerId>,
    /// Removed devices — never re-enroll (I4).
    tombstoned: HashSet<DeviceFingerprint>,
    /// Whether an `AccountGenesis` has been made effective.
    genesis_seen: bool,
    /// 0-based effective index assigned as `auth_epoch`.
    next_auth_epoch: u64,
    /// Effective immutable stream ownership roots, keyed by owner-bound stream id.
    stream_ownership: HashMap<StreamId, AccountEntryHash>,
    /// The owned streams whose effective `StreamOwn` spec declares `PublicRead` — the only
    /// streams a `StreamGrant` may fold on (see the grant gate). A subset of
    /// `stream_ownership`; the access mode is committed into the stream id, so membership is
    /// as immutable as ownership itself.
    public_streams: HashSet<StreamId>,
    /// Effective grant incarnations. A revoke closes exactly one id; a later grant gets a fresh
    /// hash and remains independent.
    grants: HashMap<GrantId, LiveGrant>,
}

#[derive(Clone, Copy)]
struct LiveGrant {
    stream_id: StreamId,
    grantee_account_id: AccountId,
    role: GrantRole,
    open: bool,
}

/// A hash-keyed [`HeaderView`] over ALL verified entries — the ancestry walk + cut-target binding
/// read headers through this. It spans every entry (incl. forward-compat UNKNOWN ops that are not
/// folded): a signed header is present whether or not its op decodes, so a cut may name an unknown
/// entry as its watermark, and unknown entries beyond a cut are condemnable.
struct CandidateView<'a> {
    headers: &'a HashMap<AccountEntryHash, &'a AccountEntryHeader>,
}

impl HeaderView for CandidateView<'_> {
    fn header(&self, entry_hash: &AccountEntryHash) -> Option<&AccountEntryHeader> {
        self.headers.get(entry_hash).copied()
    }
}

/// The revocation registers a cut op installs (§11). A `DeviceRemove` bounds the removed device's
/// WHOLE chain (a device-level register); an `OwnerDemote` bounds only the ops citing `owner_id`
/// (an owner-incarnation register). Each such op carries TWO watermarks — a `control_cut` on the
/// device's control chain (`log: CONTROL_LOG`) and a `secrets_cut` on its secrets chain
/// (`log: SECRETS_LOG`) — so it installs one register per chain, keyed identically apart from the
/// log. Each element is `(key, watermark, coordinate the watermark MUST name, §11.3)`. Element 0 is
/// the control register; element 1 (when present) is the secrets register. Non-cut ops return an
/// empty vec.
fn cut_op_registers(c: &Candidate) -> Vec<(RegisterKey, Cut, CutCoordinate)> {
    let account = c.header().account_id;
    // The device-level and owner-incarnation register keys for one `(log, cut)`, differing only by
    // the log — the control chain and the secrets chain share the same key shape (§11).
    let device_register = |log: u8, device: DeviceFingerprint, cut: &Cut| {
        (RegisterKey::Device { account, log, device }, cut.clone(), CutCoordinate {
            account,
            log,
            device,
        })
    };
    let owner_register = |log: u8, device: DeviceFingerprint, owner_id: OwnerId, cut: &Cut| {
        (
            RegisterKey::OwnerIncarnation { account, log, device, owner_id },
            cut.clone(),
            CutCoordinate { account, log, device },
        )
    };
    match &c.op {
        AccountOp::DeviceRemove { device_fingerprint, control_cut, secrets_cut, .. } => vec![
            device_register(CONTROL_LOG, *device_fingerprint, control_cut),
            device_register(SECRETS_LOG, *device_fingerprint, secrets_cut),
        ],
        AccountOp::OwnerDemote {
            device_fingerprint, owner_id, control_cut, secrets_cut, ..
        } => {
            vec![
                owner_register(CONTROL_LOG, *device_fingerprint, *owner_id, control_cut),
                owner_register(SECRETS_LOG, *device_fingerprint, *owner_id, secrets_cut),
            ]
        },
        _ => Vec::new(),
    }
}

/// The register a `CutExtend` raises (§10/§11.4) and the new watermark it joins in. A `CutExtend`
/// does NOT create a register — it extends one a prior `DeviceRemove` / `OwnerDemote` made — so it
/// is joined separately from the creator cut ops (it is never a cycle participant).
/// `incarnation_id` selects the owner-incarnation register; its absence selects the device-level
/// one. A `Ctrl` extend raises the control-chain register; a `Secrets` extend raises the
/// secrets-chain register — both account-log chains the fold holds. A `Content` extend binds a
/// stream chain (C2's fold), so it has no account-log register here and returns `None`.
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
    let log = match chain_kind {
        ChainKind::Ctrl => CONTROL_LOG,
        ChainKind::Secrets => SECRETS_LOG,
        ChainKind::Content => return None,
    };
    let account = *subject_account_id;
    let cut = Cut::At { seq: *new_seq, hash: *new_entry_hash };
    let coord = CutCoordinate { account, log, device: *device_fingerprint };
    let key = match incarnation_id {
        Some(owner_id) => RegisterKey::OwnerIncarnation {
            account,
            log,
            device: *device_fingerprint,
            owner_id: (*owner_id).into(),
        },
        None => RegisterKey::Device { account, log, device: *device_fingerprint },
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
            AncestryRelation::OnBranch => {}, /* within-cut on the accepted branch: this */
            // register admits it
            AncestryRelation::OffBranch => off_branch = true,
            AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget) =>
                park_unknown_target = true,
            AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry) =>
                park_incomplete = true,
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
/// ⇒ `contested`. Only a LITERAL self-edge (`i == j`, an op against itself) is excluded — DISTINCT
/// ops on the SAME device chain (e.g. a sole owner's two forked self-removals) DO condemn each
/// other and would 2-cycle here. Those are intrinsically dead (they close the sole prior-depth
/// owner) and are dropped by the intrinsic last-owner prefilter BEFORE this runs, so a cycle
/// detected here is only ever genuine cross-owner mutual condemnation.
fn has_condemn_cycle(admitted: &[AdmittedCut<'_>], view: &dyn HeaderView) -> bool {
    let n = admitted.len();
    // An edge X → Y iff ANY register X installs scopes Y's own cut op and condemns it. A secrets
    // register (`log: SECRETS_LOG`) never scopes a control-log cut op, so it adds no edges here —
    // the mutual-owner-condemnation cycle stays a property of the CONTROL chains — but iterating
    // every register keeps the detector correct as the register set grows.
    let condemns = |x: &AdmittedCut<'_>, y: &AdmittedCut<'_>| -> bool {
        x.registers.iter().any(|(key, cut)| {
            key.scopes(y.op.header())
                && (beyond(y.op.header().seq, cut)
                    || candidate::ancestry(&y.op.hash(), cut, view) == AncestryRelation::OffBranch)
        })
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

/// A same-depth cut op that passed cut-target binding + the I2 last-owner guard and so installs its
/// register(s) — the unit the cycle detector and the `⊔` join operate over. A `DeviceRemove` /
/// `OwnerDemote` carries BOTH its control-chain and secrets-chain registers here (see
/// [`cut_op_registers`]); each is `⊔`-joined independently under its own log-scoped key.
struct AdmittedCut<'a> {
    op: &'a Candidate,
    registers: Vec<(RegisterKey, Cut)>,
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

/// The join outcome for `cut` under `key` WITHOUT mutating `registers` — the read-only twin of
/// [`join_register`]. Lets a multi-chain cut op decide ALL its registers before committing any, so
/// a register that would park never gets raised alongside one that would apply.
fn join_register_peek(
    registers: &HashMap<RegisterKey, Cut>,
    key: &RegisterKey,
    cut: &Cut,
    view: &dyn HeaderView,
) -> RegisterJoin {
    match registers.get(key) {
        None => RegisterJoin::Applied,
        Some(existing) => match candidate::join_cuts(existing, cut, view) {
            JoinResult::Extended(_) => RegisterJoin::Applied,
            JoinResult::Incomparable => RegisterJoin::Contested,
            JoinResult::Unknown => RegisterJoin::Parked,
        },
    }
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
    fold_account_traced(entries, false).0
}

/// Captured only for an explicitly verified checkpoint, never allocated on ordinary v1 replay.
/// It carries the FINAL coherent pass's state, so a cut held out as authored ahead appears here
/// exactly as the fold left it: with no register installed and as no one's contributor.
pub(super) struct LegacyTrace {
    /// The revocation registers the fold installed — the only fold state v2 execution reads.
    registers: HashMap<RegisterKey, Cut>,
    /// The candidates that installed one. v1 computes a revocation credit for these alone.
    contributors: HashSet<AccountEntryHash>,
}

pub(super) fn fold_account_traced(
    entries: &[VerifiedAccountEntry],
    capture: bool,
) -> (AccountAuthHistory, Option<LegacyTrace>) {
    // Readiness is monotone: once an entry proves it was authored ahead of the locally folded
    // authority history (or depends on authority that did not survive the fold), it cannot
    // contribute a register or phase-E mutation in this fold. Re-run the frozen stratified pass
    // with those entries held out until no new readiness exclusions appear. This is not a graph
    // fixpoint: each pass still performs exactly the §11.1 one-way, per-depth register fold, and
    // exclusions only grow.
    let mut readiness_exclusions = HashMap::new();
    loop {
        let (history, discovered, trace) =
            fold_account_pass(entries, &readiness_exclusions, capture);
        let mut changed = false;
        for (hash, outcome) in discovered {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                readiness_exclusions.entry(hash)
            {
                entry.insert(outcome);
                changed = true;
            }
        }
        if !changed {
            return (history, trace);
        }
    }
}

fn fold_account_pass(
    entries: &[VerifiedAccountEntry],
    readiness_exclusions: &HashMap<AccountEntryHash, Outcome>,
    capture: bool,
) -> (AccountAuthHistory, HashMap<AccountEntryHash, Outcome>, Option<LegacyTrace>) {
    let mut outcomes: HashMap<AccountEntryHash, Outcome> = HashMap::new();

    // Decode once, then classify. `candidates` are the ops the fold actually folds; `all_headers`
    // is the ancestry / cut-binding view — it holds every STRUCTURALLY-VALID entry (a valid chain
    // link, incl. a forward-compat unknown or a sealed op), but NOT a malformed one, so invalid
    // bytes can't shape the accepted branch.
    let mut candidates: Vec<Candidate> = Vec::with_capacity(entries.len());
    let mut all_headers: HashMap<AccountEntryHash, &AccountEntryHeader> = HashMap::new();
    let mut seen: HashSet<AccountEntryHash> = HashSet::new();
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
        // retained header-only — never folded, never HARD-rejected — so it stays a valid
        // watermark/ancestry target and its own layer (C2/C4/newer) folds it.
        //
        // What retention does NOT buy: forward compatibility WITHIN log 0. Branch selection accepts
        // one contiguous chain per (log, device) over EFFECTIVE entries, so a retained entry
        // mid-chain truncates its author's accepted chain — every later entry from that device
        // forks. That is deliberate quarantine, not an oversight (#809): no binary folds such an
        // entry, so every binary truncates at the same slot and peers still converge, and a third
        // party cannot place an entry on someone else's chain. It is load-bearing only because
        // log 0's tag set is CLOSED — a new artifact class gets its own log (C6 →
        // `ANNEX_LOG`), never a new tag here. `retained_entry_on_the_control_log_quarantines_the_
        // rest_of_its_own_chain` pins this.
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
        return (
            AccountAuthHistory {
                outcomes,
                classification: AccountClassification::Live,
                contested_successor: None,
                effective_count: 0,
                roster_refs: HashMap::new(),
                owner_incarnations: HashMap::new(),
                stream_ownership: HashMap::new(),
                grants: HashMap::new(),
                grant_cuts: HashMap::new(),
                tombstoned: HashSet::new(),
                genesis_hash: None,
            },
            HashMap::new(),
            None,
        );
    };
    let genesis_owner_id = genesis.hash();
    let genesis_founder = genesis.subject_device();

    let mut incarnations = Incarnations::build(&candidates, genesis_owner_id.into());

    // Group resolvable candidates by author-depth; unresolvable citations park.
    let mut strata: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (idx, c) in candidates.iter().enumerate() {
        if let Some(outcome) = readiness_exclusions.get(&c.hash()) {
            outcomes.insert(c.hash(), *outcome);
            continue;
        }
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
    // recovery is deliberately excluded by the stratified model).
    let mut pass = DepthPass {
        candidates: &candidates,
        incarnations: &incarnations,
        view: CandidateView { headers: &all_headers },
        genesis_owner_id: genesis_owner_id.into(),
        state: FoldState::seeded(genesis_owner_id.into()),
        registers: HashMap::new(),
        verdicts: FoldVerdicts::default(),
        register_contributors: HashSet::new(),
        classification: AccountClassification::Live,
    };
    for (&depth, idxs) in &strata {
        if pass.fold_depth(depth, idxs, &mut outcomes).is_break() {
            break;
        }
    }
    let DepthPass { registers, verdicts, register_contributors, classification, .. } = pass;

    // Registers and their per-depth condemnation decisions are now final. Rebuild ONLY phase-E
    // state against those fixed verdicts so a retroactively condemned non-mint cannot leave a
    // roster, tombstone, ownership, or grant mutation behind. This is deliberately not a graph
    // fixpoint: no register is added/removed and no lower-depth condemnation is revised.
    let replay_before_depth = match classification {
        AccountClassification::Live => None,
        AccountClassification::Contested { state_before_depth } => Some(state_before_depth),
    };
    let mut state = replay_effect_state(
        &candidates,
        &strata,
        &incarnations,
        replay_before_depth,
        &verdicts,
        &mut outcomes,
    );

    // Overlay the FINAL register verdicts: a candidate effective at a shallow depth but
    // condemned / parked by a later register reflects that here (the strictest verdict
    // wins over an earlier one).
    for c in &candidates {
        if let Some(reason) = verdicts.condemned.get(&c.hash()) {
            outcomes.insert(c.hash(), Outcome::Condemned(*reason));
        } else if let Some(reason) = verdicts.parked.get(&c.hash()) {
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
                    if !verdicts.condemned.contains_key(&c.hash())
                        && matches!(
                            authority_status(c, &incarnations, &state, &verdicts.parked),
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
            outcomes.insert(c.hash(), state.take_epoch());
            contested_successor.get_or_insert(*successor);
        }
        for c in &candidates {
            outcomes.entry(c.hash()).or_insert(Outcome::Parked(ParkReason::ContestedSubject));
        }
    }

    // ponytail: one scan of the candidates per cut; index them by register key if cuts get
    // numerous.
    let credits: HashMap<AccountEntryHash, u64> = candidates
        .iter()
        .filter(|c| register_contributors.contains(&c.hash()))
        .map(|c| {
            let credit = revocation_credit(
                &candidates,
                &strata,
                &incarnations,
                &outcomes,
                c,
                CreditScope::EveryScopedEntry,
            );
            (c.hash(), credit)
        })
        .collect();
    let mut discovered = close_final_authority_dependencies(&candidates, &credits, &mut outcomes);
    if matches!(classification, AccountClassification::Contested { .. }) {
        contested_successor = candidates
            .iter()
            .filter(|candidate| outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective))
            .filter_map(|candidate| match candidate.op {
                AccountOp::AccountReRoot { successor_account_id, .. } => Some(successor_account_id),
                _ => None,
            })
            .min_by_key(|account_id| account_id.to_bytes());
    }
    let effective_count = normalize_auth_epochs(&mut outcomes);
    let vouches = vouch_table(&candidates, &credits, &outcomes);
    // Effective candidates use `effective_count - 1` inside the closure above. Also inspect
    // condemned, contested, and actual register contributors against the fully-folded count: a
    // mint can provisionally authorize the descendant cut that later condemns it; mutually-
    // condemning ahead cuts can manufacture `Contested`; and a cut may install a register before
    // phase E rejects it as ineffective (for example, removing a never-enrolled device).
    // Structurally rejected and ordinarily parked candidates retain their stronger verdict when
    // they contributed neither state nor a register. A cut keeps its revocation credit here too, so
    // a redundant concurrent cut of the same device does not park behind the ops both removed —
    // and the vouch of the other effective cuts, as in the closure.
    for candidate in &candidates {
        let credit = credits.get(&candidate.hash()).copied().unwrap_or(0);
        if !readiness_exclusions.contains_key(&candidate.hash())
            && register_contributors.contains(&candidate.hash())
            && cited_ahead(&vouches, candidate, effective_count, credit)
        {
            discovered.insert(candidate.hash(), Outcome::Parked(ParkReason::AuthLenAhead));
        }
    }
    // A cut found ahead in this pass is held out of the next one, so what it condemned or contested
    // is judged there. Measured now, an honest victim reads as ahead of a count that lost it to a
    // cut that did not hold, and is held out for the whole fold (#1294). The vouch applies here as
    // everywhere: a contested pair authored against the pre-cut view must stay contested, not be
    // held out as ahead and let the account fold `Live` around a genuine standoff.
    let cut_found_ahead = register_contributors.iter().any(|hash| discovered.contains_key(hash));
    if !cut_found_ahead {
        for candidate in &candidates {
            if !readiness_exclusions.contains_key(&candidate.hash())
                && !register_contributors.contains(&candidate.hash())
                && matches!(
                    outcomes.get(&candidate.hash()),
                    Some(Outcome::Condemned(_) | Outcome::Parked(ParkReason::ContestedSubject))
                )
                && cited_ahead(&vouches, candidate, effective_count, 0)
            {
                discovered.insert(candidate.hash(), Outcome::Parked(ParkReason::AuthLenAhead));
            }
        }
    }
    let facts = derive_authority_facts(&candidates, &outcomes, &registers);
    (
        AccountAuthHistory {
            outcomes,
            classification,
            contested_successor,
            effective_count,
            roster_refs: facts.roster_refs,
            owner_incarnations: facts.owner_incarnations,
            stream_ownership: facts.stream_ownership,
            grants: facts.grants,
            grant_cuts: facts.grant_cuts,
            tombstoned: state.tombstoned,
            genesis_hash: Some(genesis_owner_id),
        },
        discovered,
        capture.then(|| LegacyTrace { registers, contributors: register_contributors }),
    )
}

/// The state one stratified pass threads through its depths (§11.1), beside the read-only context
/// every stage consults. Each stage of a depth is a method; a stage that finds the depth contested
/// records the classification and returns `ControlFlow::Break`, which halts the pass with the real
/// registers exactly as the prior depth left them (§12 `state_before_depth`).
struct DepthPass<'a> {
    candidates: &'a [Candidate],
    incarnations: &'a Incarnations<'a>,
    view: CandidateView<'a>,
    genesis_owner_id: OwnerId,
    state: FoldState,
    /// The revocation registers accumulated so far (extend-only, joined by `⊔`).
    registers: HashMap<RegisterKey, Cut>,
    verdicts: FoldVerdicts,
    register_contributors: HashSet<AccountEntryHash>,
    classification: AccountClassification,
}

#[derive(Default)]
struct FoldVerdicts {
    /// Every entry a register condemns. Grows monotonically — a lower-depth decision is final, no
    /// oscillation.
    condemned: HashMap<AccountEntryHash, CondemnedReason>,
    /// Every entry a register parks. Rebuilt fresh each depth.
    parked: HashMap<AccountEntryHash, ParkReason>,
    /// The cut ops decided in the register pass (a binding failure / I2).
    cut_verdicts: HashMap<AccountEntryHash, Outcome>,
}

impl<'a> DepthPass<'a> {
    /// Fold one stratum: admit its cut registers, settle which install, reject contests, commit
    /// the registers, re-derive condemnation, then run the effect pass.
    fn fold_depth(
        &mut self,
        depth: usize,
        idxs: &[usize],
        outcomes: &mut HashMap<AccountEntryHash, Outcome>,
    ) -> ControlFlow<()> {
        let mut admitted = self.admit_cut_registers(idxs);
        self.reject_sole_owner_cuts(&mut admitted);
        self.stage_register_joins(depth, &mut admitted)?;
        // A same-depth mutual owner-condemnation cycle is genuine owner-key compromise (§12):
        // halt at the last cycle-free stratum. Detected BEFORE I2 so a two-owner
        // mutual removal folds contested rather than being resolved by reserving
        // one owner. Parked ops are already excluded above — a cut that installs nothing is not a
        // cycle participant.
        if has_condemn_cycle(&admitted, &self.view) {
            return self.contest(depth);
        }
        self.reserve_surviving_owner(&mut admitted);
        self.commit_registers(depth, idxs, &admitted)?;
        self.rederive_condemnation();
        self.run_effect_pass(idxs, outcomes);
        ControlFlow::Continue(())
    }

    /// Classify the account contested at `depth` and halt the pass.
    fn contest(&mut self, depth: usize) -> ControlFlow<()> {
        self.classification = AccountClassification::Contested { state_before_depth: depth };
        ControlFlow::Break(())
    }

    /// (a) REGISTER PASS. A cut op installs a register iff its author is AUTHORIZED
    /// (authority_status == Live: its cited incarnation resolves to a mint for the SIGNER
    /// and is live — the transitive-liveness gate that defeats laundering AND
    /// owner impersonation), it passes cut-target binding (§11.3), and — for
    /// OwnerDemote — its target `owner_id` names its subject device.
    fn admit_cut_registers(&mut self, idxs: &[usize]) -> Vec<AdmittedCut<'a>> {
        let candidates = self.candidates;
        let mut admitted: Vec<AdmittedCut<'a>> = Vec::new();
        for &i in idxs {
            let c = &candidates[i];
            // A condemned OR parked cut op installs nothing — parked authority (its own chain is
            // under a not-yet-decided watermark) must not have register side effects before it is
            // on a known-valid branch.
            if self.verdicts.condemned.contains_key(&c.hash())
                || self.verdicts.parked.contains_key(&c.hash())
            {
                continue;
            }
            let op_registers = cut_op_registers(c);
            if op_registers.is_empty() {
                continue; // not a cut op
            }
            if !matches!(
                authority_status(c, self.incarnations, &self.state, &self.verdicts.parked),
                AuthorityStatus::Live
            ) {
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
                match self.incarnations.candidate(owner_id) {
                    None => {
                        self.verdicts
                            .cut_verdicts
                            .insert(c.hash(), Outcome::Parked(ParkReason::UnknownOwnerRef));
                        continue;
                    },
                    Some(target) if target.subject_device() != *device_fingerprint => {
                        self.verdicts
                            .cut_verdicts
                            .insert(c.hash(), Outcome::Rejected(RejectReason::WrongDevice));
                        continue;
                    },
                    Some(_) => {},
                }
            }
            // Cut-target binding (§11.3) applies to EVERY chain the op cuts — its control cut AND
            // its secrets cut. A held watermark naming a DIFFERENT coordinate on any of them is a
            // structural reject of the WHOLE op (an owner who misbinds one chain's watermark is
            // misbehaving; extending the control-cut precedent, that condemns the op rather than
            // silently projecting the bad watermark). Held-and-correct OR not-yet-held installs the
            // register either way: its `[seq]` condemns beyond entries from seq alone (I11) even
            // before the watermark syncs; the under-cut branch decision parks until it does (a
            // withheld watermark never flips a verdict). A revoking owner is TRUSTED not to
            // misstate a watermark seq (§10) — a watermark that later resolves to a
            // different coordinate is owner misbehaviour, out of the trusted-owner
            // model.
            let misbound = op_registers.iter().any(|(_, cut, coord)| {
                candidate::validate_cut_target(cut, coord, &self.view)
                    == candidate::CutBinding::Mismatch
            });
            if misbound {
                self.verdicts
                    .cut_verdicts
                    .insert(c.hash(), Outcome::Rejected(RejectReason::CutTargetMismatch));
                continue;
            }
            admitted.push(AdmittedCut {
                op: c,
                registers: op_registers.into_iter().map(|(key, cut, _)| (key, cut)).collect(),
            });
        }
        // Deterministic order (by entry hash) so cut selection + the `⊔` join + the I2
        // reservation are arrival-independent (I9) when two same-depth cuts contend
        // for one register key.
        admitted.sort_by_key(|a| a.op.hash());
        admitted
    }

    /// INTRINSIC last-owner prefilter (order-free, §12/I2). A cut that closes the SOLE
    /// prior-depth owner can never succeed under ANY processing order — a size-1 surviving set
    /// never shrinks, so the sequential I2 sim would reject it as `LastOwner` whatever the sort
    /// — so reject it HERE and drop it BEFORE the park preflight, cycle-detection, AND
    /// the I2 sim. An intrinsically-dead cut must not park, contest, or form a
    /// condemnation cycle: this is what makes a SOLE owner's equivocating self-removals
    /// fold `Live` (each rejected `LastOwner`) instead of manufacturing a contested cut
    /// (incomparable variant) or a same-device 2-cycle (same-cut variant, which
    /// `has_condemn_cycle` WOULD flag). Keyed on [`closes_open_incarnation`], as in the I2
    /// simulation, against the prior-depth `state.owners` (empty at stratum 0, so genesis / a
    /// founder self-remove is never intrinsic here) — NEVER on "is a self-removal". The
    /// multi-owner mutual-removal case (owner set > 1) is untouched and still
    /// reaches `has_condemn_cycle` before I2, so it folds contested (§12).
    fn reject_sole_owner_cuts(&mut self, admitted: &mut Vec<AdmittedCut<'a>>) {
        if self.state.owners.len() != 1 {
            return;
        }
        admitted.retain(|a| {
            let closes_sole_owner = closes_open_incarnation(&a.op.op, &self.state.owners).is_some();
            if closes_sole_owner {
                self.verdicts
                    .cut_verdicts
                    .insert(a.op.hash(), Outcome::Rejected(RejectReason::LastOwner));
                return false;
            }
            true
        });
    }

    /// Decide which admitted cut ops will actually INSTALL registers this depth, BEFORE
    /// cycle-detection and the I2 last-owner simulation consume `admitted`. One signed op cuts
    /// BOTH the device's control chain and its secrets chain, and its registers commit
    /// ATOMICALLY: if EITHER chain's join is undecidable the WHOLE op raises NO register, so it
    /// is NOT an active cut this depth — it parks `UnknownCutTarget` and must not manufacture a
    /// mutual-condemnation cycle or reserve a surviving owner it never actually removes (the
    /// ordering bug: a would-be-parked op left in `admitted` would wrongly drive cycle/I2). An
    /// incomparable pair (→ Contested) is genuine owner-key compromise and still halts. The
    /// decision runs against a WORKING copy so a same-key same-depth op sees the prior op's
    /// would-be watermark, while the real register set stays untouched until after cycle/I2 (a
    /// Contested must leave this depth's registers uninstalled).
    fn stage_register_joins(
        &mut self,
        depth: usize,
        admitted: &mut Vec<AdmittedCut<'a>>,
    ) -> ControlFlow<()> {
        let mut working = self.registers.clone();
        let mut parked_cuts: HashSet<AccountEntryHash> = HashSet::new();
        for a in admitted.iter() {
            let mut any_parked = false;
            for (key, cut) in &a.registers {
                match join_register_peek(&working, key, cut, &self.view) {
                    RegisterJoin::Applied => {},
                    RegisterJoin::Contested => return self.contest(depth),
                    RegisterJoin::Parked => any_parked = true,
                }
            }
            if any_parked {
                self.verdicts
                    .cut_verdicts
                    .insert(a.op.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                parked_cuts.insert(a.op.hash());
            } else {
                // Apply so a same-key same-depth op joins against this op's would-be watermark.
                for (key, cut) in &a.registers {
                    join_register(&mut working, key.clone(), cut.clone(), &self.view);
                }
            }
        }
        admitted.retain(|a| !parked_cuts.contains(&a.op.hash()));
        ControlFlow::Continue(())
    }

    /// I2 last-owner protection across ALL same-depth admitted cuts: simulate the removals
    /// in deterministic order over the prior-depth owner set and reject any cut that would
    /// empty it, reserving a surviving owner. A cut counts only if it CLOSES a device's
    /// currently-open incarnation (a DeviceRemove of any owner, or an OwnerDemote naming the
    /// open `owner_id`) — a stale demote does not. (A self-cut is separately self-defeating:
    /// its own op sits beyond any watermark it can name on its chain, so it self-condemns.)
    fn reserve_surviving_owner(&mut self, admitted: &mut Vec<AdmittedCut<'a>>) {
        let mut surviving = self.state.owners.clone();
        admitted.retain(|a| {
            let closes = closes_open_incarnation(&a.op.op, &surviving);
            if let Some(dev) = closes {
                if surviving.len() == 1 {
                    self.verdicts
                        .cut_verdicts
                        .insert(a.op.hash(), Outcome::Rejected(RejectReason::LastOwner));
                    return false;
                }
                surviving.remove(&dev);
            }
            true
        });
    }

    /// Stage ALL of this depth's register changes (creator cuts + `CutExtend`s) in ONE working
    /// copy, then merge into the REAL register set only at the END of a NON-contested depth.
    /// This is the class fix for "partial register mutation on a contested stratum": every
    /// contest still reachable here (an incomparable extend) must leave the real
    /// registers EXACTLY as the prior depth left them (§12 `state_before_depth`), so a
    /// half-applied cut whose stratum then halts cannot leak its watermark into
    /// `derive_authority_facts`. (The creator-sim and cycle contests run before any
    /// real mutation; this staging covers the two remaining mutation sites — the creator commit
    /// and the extends join — which both precede the extends contest.)
    fn commit_registers(
        &mut self,
        depth: usize,
        idxs: &[usize],
        admitted: &[AdmittedCut<'a>],
    ) -> ControlFlow<()> {
        let mut depth_registers = self.registers.clone();

        // Commit the surviving admitted cut ops' registers (§11.3 `⊔`) into the staging copy. The
        // park / contested / incomparable decisions were all made above against the working copy,
        // and I2 only REMOVES ops (same-key removers are all-rejected-or-all-kept together, so a
        // kept op never loses a same-key predecessor), so every remaining register here joins
        // `Applied`.
        for a in admitted {
            for (key, cut) in &a.registers {
                join_register(&mut depth_registers, key.clone(), cut.clone(), &self.view);
            }
            self.register_contributors.insert(a.op.hash());
        }

        // Raise registers with this depth's live `CutExtend`s (§11.4 recovery). An extend is
        // EXTEND-ONLY: it may only raise a register a prior DeviceRemove/OwnerDemote created (this
        // depth's creators are already in `depth_registers`), never conjure a fresh one (else a
        // live owner could condemn a chain with a bare extend). An extend for a not-yet-established
        // register parks until the creator syncs.
        let candidates = self.candidates;
        let mut extends: Vec<(&Candidate, RegisterKey, Cut)> = Vec::new();
        for &i in idxs {
            let c = &candidates[i];
            if self.verdicts.condemned.contains_key(&c.hash())
                || self.verdicts.parked.contains_key(&c.hash())
            {
                continue;
            }
            let Some((key, cut, coord)) = cut_extend_register(c) else {
                continue;
            };
            if !matches!(
                authority_status(c, self.incarnations, &self.state, &self.verdicts.parked),
                AuthorityStatus::Live
            ) {
                continue;
            }
            if candidate::validate_cut_target(&cut, &coord, &self.view)
                == candidate::CutBinding::Mismatch
            {
                self.verdicts
                    .cut_verdicts
                    .insert(c.hash(), Outcome::Rejected(RejectReason::CutTargetMismatch));
                continue;
            }
            if !depth_registers.contains_key(&key) {
                self.verdicts
                    .cut_verdicts
                    .insert(c.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                continue;
            }
            extends.push((c, key, cut));
        }
        extends.sort_by_key(|(c, _, _)| c.hash());
        for (c, key, cut) in extends {
            // Join into the STAGING copy. A same-key same-depth extend joins against the prior
            // extend's watermark; an incomparable pair (→ Contested) is owner-key compromise and
            // halts with the REAL registers still untouched — `depth_registers` is
            // dropped, never merged, so no watermark leaks from the halted stratum.
            match join_register(&mut depth_registers, key, cut, &self.view) {
                RegisterJoin::Applied => {
                    self.register_contributors.insert(c.hash());
                },
                RegisterJoin::Contested => return self.contest(depth),
                RegisterJoin::Parked => {
                    self.verdicts
                        .cut_verdicts
                        .insert(c.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                },
            }
        }

        // No contest this depth — merge the staged changes into the real register set. The
        // condemnation scan and every later depth now see this depth's creators + extends.
        self.registers = depth_registers;
        ControlFlow::Continue(())
    }

    /// Re-derive condemnation + parking against the current registers. Condemnation grows
    /// monotonically: the frozen stratified model never lets a deeper authority revise a
    /// lower-depth decision. Parking is rebuilt because missing ancestry can arrive later.
    fn rederive_condemnation(&mut self) {
        self.verdicts.parked.clear();
        for c in self.candidates {
            // The genesis is the account's ROOT axiom — it can never be condemned, else a cut on
            // the founder's own chain (e.g. a self-DeviceRemove with an empty cut,
            // which condemns everything on that chain incl. seq 0) would leave a `Live`
            // account with no effective root. The founder's LATER entries stay
            // condemnable; only the seq-0 root is exempt.
            if c.hash() == self.genesis_owner_id.into()
                || self.verdicts.condemned.contains_key(&c.hash())
            {
                continue;
            }
            match register_verdict(c, &self.registers, &self.view) {
                RegisterVerdict::Condemned(reason) => {
                    self.verdicts.condemned.insert(c.hash(), reason);
                    // A condemned mint leaves `live` (kills dependents transitively) and, if it is
                    // the device's open incarnation, `owners`. A condemned DeviceAdd ALSO leaves
                    // the roster — its enrollment is invalidated, so a later
                    // OwnerPromote must not see the device as enrolled. (A
                    // condemned OwnerPromote leaves the roster intact —
                    // the device's separate DeviceAdd enrollment may still be valid.)
                    if c.is_mint() {
                        let state = &mut self.state;
                        state.live.remove(&c.hash().into());
                        if state.owners.get(&c.subject_device()) == Some(&c.hash().into()) {
                            state.owners.remove(&c.subject_device());
                        }
                        // Roll back the roster only if THIS DeviceAdd is the source of the current
                        // enrollment — a condemned duplicate/superseded add must not erase the
                        // enrollment a different, still-valid add contributed.
                        if matches!(c.op, AccountOp::DeviceAdd { .. })
                            && state.roster.get(&c.subject_device()) == Some(&c.hash().into())
                        {
                            state.roster.remove(&c.subject_device());
                            state.enrollment_roles.remove(&c.subject_device());
                        }
                    }
                },
                RegisterVerdict::Parked(reason) => {
                    self.verdicts.parked.insert(c.hash(), reason);
                },
                RegisterVerdict::Clear => {},
            }
        }
    }

    /// (b) EFFECT PASS over the stratum in (chain, seq, hash) order — a TOTAL order, so an
    /// equivocation (same device + seq, different content) sorts identically under every
    /// arrival permutation (I9).
    fn run_effect_pass(
        &mut self,
        idxs: &[usize],
        outcomes: &mut HashMap<AccountEntryHash, Outcome>,
    ) {
        effect_pass(
            self.candidates,
            idxs,
            self.incarnations,
            &self.verdicts,
            &mut self.state,
            outcomes,
        );
    }
}
fn effect_pass(
    candidates: &[Candidate],
    idxs: &[usize],
    incarnations: &Incarnations<'_>,
    verdicts: &FoldVerdicts,
    state: &mut FoldState,
    outcomes: &mut HashMap<AccountEntryHash, Outcome>,
) {
    let mut ordered = idxs.to_vec();
    ordered.sort_by_key(|&i| {
        let h = candidates[i].header();
        (h.device_fingerprint.to_bytes(), h.seq, candidates[i].hash())
    });
    for i in ordered {
        let c = &candidates[i];
        if let Some(reason) = verdicts.condemned.get(&c.hash()) {
            outcomes.insert(c.hash(), Outcome::Condemned(*reason));
            continue;
        }
        if let Some(reason) = verdicts.parked.get(&c.hash()) {
            outcomes.insert(c.hash(), Outcome::Parked(*reason));
            continue;
        }
        if let Some(verdict) = verdicts.cut_verdicts.get(&c.hash()) {
            outcomes.insert(c.hash(), *verdict);
            continue;
        }
        let outcome = match classify_effect(c, incarnations, state, &verdicts.parked) {
            EffectVerdict::Effective => {
                let outcome = state.take_epoch();
                apply_effect(c, state);
                outcome
            },
            EffectVerdict::Rejected(reason) => Outcome::Rejected(reason),
            EffectVerdict::Parked(reason) => Outcome::Parked(reason),
        };
        outcomes.insert(c.hash(), outcome);
    }
}

fn replay_effect_state(
    candidates: &[Candidate],
    strata: &BTreeMap<usize, Vec<usize>>,
    incarnations: &Incarnations<'_>,
    before_depth: Option<usize>,
    verdicts: &FoldVerdicts,
    outcomes: &mut HashMap<AccountEntryHash, Outcome>,
) -> FoldState {
    let mut state = FoldState::seeded(incarnations.genesis_owner_id);
    for (&depth, idxs) in strata {
        if before_depth.is_some_and(|limit| depth >= limit) {
            break;
        }
        effect_pass(candidates, idxs, incarnations, verdicts, &mut state, outcomes);
    }
    state
}

fn normalize_auth_epochs(outcomes: &mut HashMap<AccountEntryHash, Outcome>) -> u64 {
    let mut effective: Vec<(AccountEntryHash, u64)> = outcomes
        .iter()
        .filter_map(|(hash, outcome)| match outcome {
            Outcome::Effective { auth_epoch } => Some((*hash, *auth_epoch)),
            _ => None,
        })
        .collect();
    effective.sort_by_key(|(hash, old_epoch)| (*old_epoch, *hash));
    for (new_epoch, (hash, _)) in effective.iter().enumerate() {
        outcomes.insert(*hash, Outcome::Effective { auth_epoch: new_epoch as u64 });
    }
    effective.len() as u64
}

/// The entries a revoking cut took out of the effective count, credited back to that cut's own
/// freshness check (#1294). Its author cited a count that included the revoked device's ops it had
/// folded; once the cut applies they are condemned, so without the credit an honest revocation
/// reads as ahead of the log and parks behind the very ops it revokes.
///
/// Counted: candidates the cut's own register keys scope that ended condemned, plus ops left
/// without authority because a counted mint fell, transitively. Both stay inside the revoked
/// device's cone: `scopes` needs its fingerprint, which the signature binds, and a stale dependent
/// must be signed by its mint's subject (anyone else is `WrongDevice`). So a revoked key's own
/// entries can at most bring forward its own revocation, and credit never reaches any other op.
/// Dependents rejected on a state precondition (`Ineffective`) are not counted: under-crediting
/// only parks the cut, it never admits an ahead one. A `CutExtend` has no creator keys and gets
/// none.
/// Which condemned entries a cut may count toward its own freshness credit. A v1 cut counts every
/// entry its own register keys scope, because the log carries no statement of which ones its author
/// had folded. A v2 cut counts only the identities its signed pre-cut manifest nominated, so its
/// credit is by construction a SUBSET of the v1 credit over the same outcomes and register scope —
/// the same loops decide it, under a strictly narrower membership test.
enum CreditScope<'a> {
    EveryScopedEntry,
    Nominated(&'a HashSet<AccountEntryHash>),
}

impl CreditScope<'_> {
    fn admits(&self, hash: &AccountEntryHash) -> bool {
        match self {
            CreditScope::EveryScopedEntry => true,
            CreditScope::Nominated(eligible) => eligible.contains(hash),
        }
    }
}

fn revocation_credit(
    candidates: &[Candidate],
    strata: &BTreeMap<usize, Vec<usize>>,
    incarnations: &Incarnations<'_>,
    outcomes: &HashMap<AccountEntryHash, Outcome>,
    cut: &Candidate,
    scope: CreditScope<'_>,
) -> u64 {
    let keys: Vec<RegisterKey> = cut_op_registers(cut).into_iter().map(|(key, ..)| key).collect();
    let mut closed_mints: HashSet<AccountEntryHash> = HashSet::new();
    let mut credit = 0;
    // Every candidate, not just the strata: an entry held out by a readiness exclusion is still
    // condemned by the final register overlay, and its author's count included it.
    for c in candidates {
        // Never the cut itself: a self-removal condemns its own entry, which its author never
        // folded, and counting it would let an ahead self-cut pay for its own freshness.
        if c.hash() != cut.hash()
            && scope.admits(&c.hash())
            && matches!(outcomes.get(&c.hash()), Some(Outcome::Condemned(_)))
            && keys.iter().any(|key| key.scopes(c.header()))
        {
            credit += 1;
            if c.is_mint() {
                closed_mints.insert(c.hash());
            }
        }
    }
    // Ascending depth: a dependent is always deeper than the mint it cites.
    for &i in strata.values().flatten() {
        let c = &candidates[i];
        // Nor the cut itself when it condemned its own authorizing mint and went stale.
        if c.hash() != cut.hash()
            && scope.admits(&c.hash())
            && outcomes.get(&c.hash()) == Some(&Outcome::Rejected(RejectReason::StaleAuthority))
            && incarnations
                .author_incarnation_id(c)
                .is_some_and(|inc| closed_mints.contains(&inc.into()))
        {
            credit += 1;
            if c.is_mint() {
                closed_mints.insert(c.hash());
            }
        }
    }
    credit
}

/// What the effective cuts vouch for the other ops' freshness, and the effective ops each author
/// can count on its own chain — the two owner-signed inputs of [`concurrent_vouch`]. One snapshot
/// per check, taken before any verdict moves.
struct VouchTable {
    /// Each effective cut that installs a register: its hash, revocation credit and cited length.
    /// A cut extend installs no register and vouches for nothing.
    cuts: Vec<(AccountEntryHash, u64, u64)>,
    /// Every effective op by author device: chain seq and cited length.
    chains: HashMap<DeviceFingerprint, Vec<(u64, u64)>>,
}

fn vouch_table(
    candidates: &[Candidate],
    credits: &HashMap<AccountEntryHash, u64>,
    outcomes: &HashMap<AccountEntryHash, Outcome>,
) -> VouchTable {
    let effective: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| outcomes.get(&c.hash()).is_some_and(Outcome::is_effective))
        .collect();
    VouchTable {
        cuts: effective
            .iter()
            .filter(|c| !cut_op_registers(c).is_empty())
            .filter_map(|c| {
                credits.get(&c.hash()).map(|credit| (c.hash(), *credit, c.header().auth_len))
            })
            .collect(),
        chains: effective.iter().fold(HashMap::new(), |mut chains, c| {
            let h = c.header();
            chains.entry(h.device_fingerprint).or_insert_with(Vec::new).push((h.seq, h.auth_len));
            chains
        }),
    }
}

/// Whether `op` cites past `base + credit` even with what the effective cuts vouch for it. The
/// vouch is computed only for an op that is ahead without it: the chain scan behind it is
/// per-device, and an ahead op is the exception, so a refold stays linear in the ordinary case.
fn cited_ahead(table: &VouchTable, op: &Candidate, base: u64, credit: u64) -> bool {
    let measure = base.saturating_add(credit);
    op.header().auth_len > measure
        && op.header().auth_len > measure.saturating_add(concurrent_vouch(table, op, base))
}

/// What the effective cuts vouch for an op's freshness (#1301): an op authored concurrently with a
/// revoking cut counted the ops the cut condemned, and its citation exceeds the post-cut count by
/// that many. The cut's own credit ([`revocation_credit`]) covers only the cut. Every other op is
/// credited
///
/// ```text
/// min( Σ credit of the other effective cuts,  max(0, ceiling − base) )
/// ceiling = max over those cuts c of
///           N_c + the op's own effective chain predecessors cited at or past N_c
/// ```
///
/// where `N_c` is the cut's cited length and `base` the count the op is measured against. A cut's
/// citation is owner-signed and the fold accepted it, so `N_c − base` is the fewest condemned ops
/// its author must have folded; an honest op concurrent with the cuts cites at most what one of
/// their authors could have counted, plus one for each op of its own it had landed since that view.
/// Those predecessors are as owner-signed as the cuts — effective, on the op's own chain, below it
/// — and a revoked key cannot add one. Without them only the first op an owner authors after the
/// shared view clears; each later one would park until the account gained an op it did not author.
/// The maximum is taken per cut so the ceiling never falls when a cut joins the table: a cut that
/// is provisionally effective can only raise it, and the pass-level check, which sees every
/// register contributor, never parks what the closure would clear. The credit term bounds the whole
/// by what is actually condemned. A revoked key can inflate the credits (its entries past the cut
/// are all condemned), but it signs no accepted citation and adds nothing effective, so it cannot
/// raise the ceiling. One vouch per op, not one per cut: summing per-cut vouches double-counts a
/// victim two cut authors both folded, and a revoked key's junk then moves the sum.
///
/// What remains, all inside the trusted-owner model. The credit is what makes a cut effective, and
/// a revoked key can inflate it enough to clear a cut whose citation is ahead of its honest view
/// (that alone only brings the revocation forward). Such a cut then vouches for ops up to that
/// citation before the ops justifying it are held — bounded by an owner-signed citation, which a
/// live owner could reach with filler adds anyway, and undone when the missing ops arrive and the
/// fold recomputes. The credit term is the victims' to inflate too, so an owner citing high on a
/// cut of a device it padded with entries vouches for that much; two such cuts clear each other.
/// Nothing in the log separates a cut's folded victims from later junk on the same chain; a signed
/// victim-chain watermark on the cut ops would (#1311).
fn concurrent_vouch(table: &VouchTable, op: &Candidate, base: u64) -> u64 {
    let condemned: u64 = table
        .cuts
        .iter()
        .filter(|(hash, _, _)| *hash != op.hash())
        .fold(0u64, |sum, (_, credit, _)| sum.saturating_add(*credit));
    if condemned == 0 {
        return 0;
    }
    let h = op.header();
    let chain = table.chains.get(&h.device_fingerprint).map_or(&[][..], Vec::as_slice);
    let ceiling = table
        .cuts
        .iter()
        .filter(|(hash, _, _)| *hash != op.hash())
        .map(|&(_, _, cited)| {
            let own_since =
                chain.iter().filter(|&&(seq, auth_len)| seq < h.seq && auth_len >= cited).count();
            cited.saturating_add(own_since as u64)
        })
        .max()
        .unwrap_or(0);
    condemned.min(ceiling.saturating_sub(base))
}

/// Final fail-closed dependency closure after fixed-register phase-E replay. No authority fact may
/// survive without its final-effective roster / ownership / grant prerequisite. Freshness is also
/// decided against the fully folded count here — never against transient hash iteration order, and
/// never as an authority input.
///
/// Each round first removes every op whose prerequisite is not effective until nothing more falls
/// (removal only, so the fixed point does not depend on candidate order), then measures freshness
/// against that settled set and applies the parks together. Measuring earlier would let an op that
/// is about to fall — a cut whose authority is parked, say — vouch for another op or move its
/// ceiling for one round, and since an ahead park is permanent, which round it fell in would decide
/// the other op's verdict.
fn close_final_authority_dependencies(
    candidates: &[Candidate],
    credits: &HashMap<AccountEntryHash, u64>,
    outcomes: &mut HashMap<AccountEntryHash, Outcome>,
) -> HashMap<AccountEntryHash, Outcome> {
    let mut discovered = HashMap::new();
    loop {
        let measured: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective))
            .collect();
        settle_authority_dependencies(candidates, outcomes);
        let effective_count = normalize_auth_epochs(outcomes);
        let vouches = vouch_table(candidates, credits, outcomes);
        // Every op effective at the start of the round is measured against the settled fold
        // without it, the ones that just fell included: a citation ahead of that count is the
        // stronger, permanent verdict.
        let ahead: Vec<AccountEntryHash> = measured
            .into_iter()
            .filter(|candidate| {
                let credit = credits.get(&candidate.hash()).copied().unwrap_or(0);
                let still_effective =
                    outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective);
                let base = effective_count.saturating_sub(u64::from(still_effective));
                cited_ahead(&vouches, candidate, base, credit)
            })
            .map(Candidate::hash)
            .collect();
        if ahead.is_empty() {
            return discovered;
        }
        for hash in ahead {
            outcomes.insert(hash, Outcome::Parked(ParkReason::AuthLenAhead));
            discovered.insert(hash, Outcome::Parked(ParkReason::AuthLenAhead));
        }
    }
}

/// Remove every effective op whose prerequisite is not effective — its `authority_ref`, the roster
/// row an `OwnerPromote` needs, the public ownership a `StreamGrant` needs, the grant a
/// `StreamRevoke` names — until nothing more falls. Only freshness is monotone across readiness
/// passes: a stale citation or failed state precondition can recover after an ahead competing
/// effect is excluded, so these verdicts are recomputed rather than permanently held out.
fn settle_authority_dependencies(
    candidates: &[Candidate],
    outcomes: &mut HashMap<AccountEntryHash, Outcome>,
) {
    loop {
        let effective_roster: HashSet<DeviceFingerprint> = candidates
            .iter()
            .filter(|candidate| outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective))
            .filter_map(|candidate| match candidate.op {
                AccountOp::AccountGenesis { .. } => Some(candidate.header().device_fingerprint),
                AccountOp::DeviceAdd { device_fingerprint, .. } => Some(device_fingerprint),
                _ => None,
            })
            .collect();
        // PublicRead-scoped, mirroring the primary grant gate: a grant surviving closure must
        // cite an effective PUBLIC ownership root, not merely an effective one.
        let effective_public_ownership: HashSet<StreamId> = candidates
            .iter()
            .filter(|candidate| outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective))
            .filter_map(|candidate| match &candidate.op {
                AccountOp::StreamOwn { stream_id, stream_spec_bytes } =>
                    stream::decode_spec_v2(stream_spec_bytes)
                        .is_ok_and(|spec| spec.access_mode == stream::AccessMode::PublicRead)
                        .then_some(*stream_id),
                _ => None,
            })
            .collect();
        let effective_grants: HashMap<GrantId, (StreamId, AccountId)> = candidates
            .iter()
            .filter(|candidate| outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective))
            .filter_map(|candidate| match candidate.op {
                AccountOp::StreamGrant { stream_id, grantee_account_id, .. } =>
                    Some((candidate.hash().into(), (stream_id, grantee_account_id))),
                _ => None,
            })
            .collect();

        let mut changed = false;
        for candidate in candidates {
            if !outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective) {
                continue;
            }
            let replacement = if let Some(authority_ref) = candidate.header().authority_ref
                && !outcomes.get(&authority_ref.into()).is_some_and(Outcome::is_effective)
            {
                Some(match outcomes.get(&authority_ref.into()) {
                    Some(Outcome::Parked(reason)) => Outcome::Parked(*reason),
                    _ => Outcome::Rejected(RejectReason::StaleAuthority),
                })
            } else {
                match &candidate.op {
                    AccountOp::OwnerPromote { device_fingerprint }
                        if !effective_roster.contains(device_fingerprint) =>
                        Some(Outcome::Rejected(RejectReason::Ineffective)),
                    AccountOp::StreamGrant { stream_id, .. }
                        if !effective_public_ownership.contains(stream_id) =>
                        Some(Outcome::Rejected(RejectReason::Ineffective)),
                    AccountOp::StreamRevoke { stream_id, grantee_account_id, grant_id, .. }
                        if effective_grants.get(grant_id)
                            != Some(&(*stream_id, *grantee_account_id)) =>
                        Some(Outcome::Rejected(RejectReason::Ineffective)),
                    _ => None,
                }
            };
            if let Some(replacement) = replacement {
                outcomes.insert(candidate.hash(), replacement);
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}

#[derive(Default)]
struct AuthorityFacts {
    roster_refs: HashMap<RosterRef, RosterFact>,
    owner_incarnations: HashMap<OwnerId, OwnerIncarnationFact>,
    stream_ownership: HashMap<StreamId, StreamOwnershipFact>,
    grants: HashMap<GrantId, GrantFact>,
    grant_cuts: HashMap<GrantId, Vec<DeviceCut>>,
}

fn derive_authority_facts(
    candidates: &[Candidate],
    outcomes: &HashMap<AccountEntryHash, Outcome>,
    registers: &HashMap<RegisterKey, Cut>,
) -> AuthorityFacts {
    let mut effective: Vec<(&Candidate, u64)> = candidates
        .iter()
        .filter_map(|candidate| match outcomes.get(&candidate.hash()) {
            Some(Outcome::Effective { auth_epoch }) => Some((candidate, *auth_epoch)),
            _ => None,
        })
        .collect();
    effective.sort_by_key(|(candidate, epoch)| (*epoch, candidate.hash()));

    let mut facts = AuthorityFacts::default();
    let mut roster = HashMap::<DeviceFingerprint, RosterRef>::new();
    let mut owners = HashMap::<DeviceFingerprint, OwnerId>::new();
    for (candidate, epoch) in effective {
        match &candidate.op {
            AccountOp::AccountGenesis { .. } => {
                let device = candidate.subject_device();
                let hash = candidate.hash();
                facts.roster_refs.insert(hash.into(), RosterFact {
                    authority: RosterAuthority {
                        device_fingerprint: device,
                        current_role: DeviceRole::Owner,
                    },
                    effective_at: epoch,
                    closed_at: None,
                    control_boundary: AuthorityBoundary::Open,
                    secrets_boundary: AuthorityBoundary::Open,
                    content_boundaries: HashMap::new(),
                });
                facts.owner_incarnations.insert(hash.into(), OwnerIncarnationFact {
                    authority: OwnerAuthority { device_fingerprint: device },
                    effective_at: epoch,
                    closed_at: None,
                    control_boundary: AuthorityBoundary::Open,
                    secrets_boundary: AuthorityBoundary::Open,
                });
                roster.insert(device, hash.into());
                owners.insert(device, hash.into());
            },
            AccountOp::DeviceAdd { device_fingerprint, role, .. } => {
                let hash = candidate.hash();
                facts.roster_refs.insert(hash.into(), RosterFact {
                    authority: RosterAuthority {
                        device_fingerprint: *device_fingerprint,
                        current_role: *role,
                    },
                    effective_at: epoch,
                    closed_at: None,
                    control_boundary: AuthorityBoundary::Open,
                    secrets_boundary: AuthorityBoundary::Open,
                    content_boundaries: HashMap::new(),
                });
                roster.insert(*device_fingerprint, hash.into());
                if *role == DeviceRole::Owner {
                    facts.owner_incarnations.insert(hash.into(), OwnerIncarnationFact {
                        authority: OwnerAuthority { device_fingerprint: *device_fingerprint },
                        effective_at: epoch,
                        closed_at: None,
                        control_boundary: AuthorityBoundary::Open,
                        secrets_boundary: AuthorityBoundary::Open,
                    });
                    owners.insert(*device_fingerprint, hash.into());
                }
            },
            AccountOp::OwnerPromote { device_fingerprint } => {
                let hash = candidate.hash();
                // A promote of a device with no ACTIVE enrollment confers nothing. Neither fold
                // reaches this any more: `settle_authority_dependencies` rejects such a promote
                // `Ineffective`, and `v2::pinned_history` now replays the same effect pass over the
                // applied v2 operations, so a promote whose `DeviceAdd` a v2 cut condemned is
                // classified `BadPromote` rather than left standing. Kept total rather than
                // asserting, so a future composition that reaches it grants nothing instead of
                // panicking.
                let Some(roster_ref) = roster.get(device_fingerprint) else {
                    continue;
                };
                facts
                    .roster_refs
                    .get_mut(roster_ref)
                    .expect("active roster fact")
                    .authority
                    .current_role = DeviceRole::Owner;
                facts.owner_incarnations.insert(hash.into(), OwnerIncarnationFact {
                    authority: OwnerAuthority { device_fingerprint: *device_fingerprint },
                    effective_at: epoch,
                    closed_at: None,
                    control_boundary: AuthorityBoundary::Open,
                    secrets_boundary: AuthorityBoundary::Open,
                });
                owners.insert(*device_fingerprint, hash.into());
            },
            AccountOp::DeviceRemove { device_fingerprint, content_cuts, .. } => {
                if let Some(roster_ref) = roster.remove(device_fingerprint) {
                    let fact = facts.roster_refs.get_mut(&roster_ref).expect("active roster fact");
                    let account = candidate.header().account_id;
                    fact.closed_at = Some(epoch);
                    // Both boundaries are the §11.3-validated, `⊔`-joined watermark for the
                    // device's chain — NOT the raw op-field cut — so a
                    // `CutExtend` that raised either chain is reflected. An
                    // effective remove always installed its registers, so the `Closed`
                    // default is defensive.
                    fact.control_boundary = registers
                        .get(&RegisterKey::Device {
                            account,
                            log: CONTROL_LOG,
                            device: *device_fingerprint,
                        })
                        .map_or(AuthorityBoundary::Closed, boundary_from_cut);
                    fact.secrets_boundary = registers
                        .get(&RegisterKey::Device {
                            account,
                            log: SECRETS_LOG,
                            device: *device_fingerprint,
                        })
                        .map_or(AuthorityBoundary::Closed, boundary_from_cut);
                    fact.content_boundaries = content_cuts
                        .iter()
                        .map(|cut| {
                            (cut.stream_id, AuthorityBoundary::Cut { seq: cut.seq, hash: cut.hash })
                        })
                        .collect();
                }
                if let Some(owner_id) = owners.remove(device_fingerprint) {
                    facts
                        .owner_incarnations
                        .get_mut(&owner_id)
                        .expect("active owner fact")
                        .closed_at = Some(epoch);
                }
            },
            AccountOp::OwnerDemote { device_fingerprint, owner_id, .. } => {
                if closes_open_incarnation(&candidate.op, &owners).is_some() {
                    owners.remove(device_fingerprint);
                    let account = candidate.header().account_id;
                    let roster_ref = roster
                        .get(device_fingerprint)
                        .expect("demoted device has an active roster fact");
                    facts
                        .roster_refs
                        .get_mut(roster_ref)
                        .expect("active roster fact")
                        .authority
                        .current_role = DeviceRole::Member;
                    let fact =
                        facts.owner_incarnations.get_mut(owner_id).expect("active owner fact");
                    fact.closed_at = Some(epoch);
                    // Both boundaries read the §11.3-validated, `⊔`-joined owner-incarnation
                    // register for the demoted `owner_id` — not the raw op-field cut — so a
                    // `CutExtend` raising either chain is reflected.
                    let owner_register = |log: u8| RegisterKey::OwnerIncarnation {
                        account,
                        log,
                        device: *device_fingerprint,
                        owner_id: *owner_id,
                    };
                    fact.control_boundary = registers
                        .get(&owner_register(CONTROL_LOG))
                        .map_or(AuthorityBoundary::Closed, boundary_from_cut);
                    fact.secrets_boundary = registers
                        .get(&owner_register(SECRETS_LOG))
                        .map_or(AuthorityBoundary::Closed, boundary_from_cut);
                }
            },
            AccountOp::StreamOwn { stream_id, .. } => {
                facts.stream_ownership.insert(*stream_id, StreamOwnershipFact {
                    own_id: candidate.hash(),
                    effective_at: epoch,
                });
            },
            AccountOp::StreamGrant { stream_id, grantee_account_id, grant_role } => {
                facts.grants.insert(candidate.hash().into(), GrantFact {
                    authority: GrantAuthority {
                        stream_id: *stream_id,
                        grantee_account_id: *grantee_account_id,
                        role: *grant_role,
                    },
                    effective_at: epoch,
                    closed_at: None,
                });
            },
            AccountOp::StreamRevoke { grant_id, device_cuts, .. } => {
                if let Some(grant) = facts.grants.get_mut(grant_id) {
                    grant.closed_at = Some(epoch);
                    facts.grant_cuts.insert(*grant_id, device_cuts.clone());
                }
            },
            AccountOp::CutExtend { .. } | AccountOp::AccountReRoot { .. } => {},
        }
    }
    // Register creators can be retained even when their state mutation is ineffective (notably a
    // remove that arrived before enrollment). Project the fold's FINAL joined device register onto
    // every roster incarnation for that device — control from `log: CONTROL_LOG`, secrets from
    // `log: SECRETS_LOG` — so a device whose chain the fold condemns never reports an Open boundary
    // while a register bounds it. Deriving only from effective close ops would otherwise expose
    // Open on both chains for a state-ineffective creator, misstating the secrets chain as
    // unbounded.
    for fact in facts.roster_refs.values_mut() {
        let device = fact.authority.device_fingerprint;
        let device_register = |want_log: u8| {
            registers.iter().find_map(|(key, cut)| match key {
                RegisterKey::Device { log, device: reg_device, .. }
                    if *log == want_log && *reg_device == device =>
                    Some(cut),
                _ => None,
            })
        };
        if let Some(cut) = device_register(CONTROL_LOG) {
            fact.control_boundary = boundary_from_cut(cut);
        }
        if let Some(cut) = device_register(SECRETS_LOG) {
            fact.secrets_boundary = boundary_from_cut(cut);
        }
    }
    facts
}

fn boundary_from_cut(cut: &Cut) -> AuthorityBoundary {
    match cut {
        Cut::Empty => AuthorityBoundary::Closed,
        Cut::At { seq, hash } => AuthorityBoundary::Cut { seq: *seq, hash: *hash },
    }
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
                    && id::account_id_from_genesis_payload(&c.entry.payload) == h.account_id
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

fn closes_open_incarnation(
    op: &AccountOp,
    owners: &HashMap<DeviceFingerprint, OwnerId>,
) -> Option<DeviceFingerprint> {
    match op {
        AccountOp::DeviceRemove { device_fingerprint, .. } =>
            owners.contains_key(device_fingerprint).then_some(*device_fingerprint),
        AccountOp::OwnerDemote { device_fingerprint, owner_id, .. } =>
            (owners.get(device_fingerprint) == Some(owner_id)).then_some(*device_fingerprint),
        _ => None,
    }
}

/// The §"authority rule" (clauses 1 + 3): `c`'s cited incarnation must (1) resolve to a mint whose
/// SUBJECT device is the signer, and (3) be live. `AccountGenesis` acts under its own incarnation
/// (self-minted, subject = signer), so it passes trivially once seeded live. A not-live authorizer
/// that is merely PARKED (vs condemned) parks the dependent so it can recover on a later refold.
fn authority_status(
    c: &Candidate,
    incarnations: &Incarnations<'_>,
    state: &FoldState,
    parked: &HashMap<AccountEntryHash, ParkReason>,
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
    match parked.get(&author_inc.into()) {
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
    parked: &HashMap<AccountEntryHash, ParkReason>,
) -> EffectVerdict {
    // The author must act under a LIVE incarnation minted for THIS device (clauses 1 + 3). This is
    // what defeats laundering (a cut authored under a since-condemned owner is not live) AND owner
    // impersonation (a member citing another device's live incarnation — P3-adjacent).
    match authority_status(c, incarnations, state, parked) {
        AuthorityStatus::Unresolvable => return EffectVerdict::Parked(ParkReason::UnknownOwnerRef),
        AuthorityStatus::WrongDevice => return EffectVerdict::Rejected(RejectReason::WrongDevice),
        AuthorityStatus::Stale => return EffectVerdict::Rejected(RejectReason::StaleAuthority),
        // A parked authorizer parks the dependent (recoverable), never permanently stale-rejects
        // it.
        AuthorityStatus::ParkedAuthorizer(reason) => return EffectVerdict::Parked(reason),
        AuthorityStatus::Live => {},
    }
    match &c.op {
        AccountOp::AccountGenesis { .. } => {
            if state.genesis_seen {
                return EffectVerdict::Rejected(RejectReason::DuplicateGenesis);
            }
            // The self-hash was checked in `find_genesis`; a second genesis reaching here is a dup.
            if id::account_id_from_genesis_payload(&c.entry.payload) != c.header().account_id {
                return EffectVerdict::Rejected(RejectReason::GenesisSelfHash);
            }
            EffectVerdict::Effective
        },
        AccountOp::DeviceAdd { device_fingerprint, .. } => {
            if state.tombstoned.contains(device_fingerprint) {
                EffectVerdict::Rejected(RejectReason::TombstoneReAdd)
            } else if state.roster.contains_key(device_fingerprint) {
                EffectVerdict::Rejected(RejectReason::DuplicateAdd)
            } else {
                EffectVerdict::Effective
            }
        },
        AccountOp::OwnerPromote { device_fingerprint } => {
            let enrolled = state.roster.contains_key(device_fingerprint);
            let already_owner = state.owners.contains_key(device_fingerprint);
            let tombstoned = state.tombstoned.contains(device_fingerprint);
            let authoring_role = state
                .enrollment_roles
                .get(device_fingerprint)
                .is_some_and(|role| role.can_author_content());
            if enrolled && authoring_role && !already_owner && !tombstoned {
                EffectVerdict::Effective
            } else {
                EffectVerdict::Rejected(RejectReason::BadPromote)
            }
        },
        AccountOp::StreamOwn { stream_id, stream_spec_bytes } => {
            let valid = stream::decode_spec_v2(stream_spec_bytes)
                .and_then(|spec| {
                    anyhow::ensure!(spec.owner_account_id == c.header().account_id);
                    Ok(stream::derive_v2(&spec)? == *stream_id)
                })
                .unwrap_or(false);
            if !valid {
                EffectVerdict::Rejected(RejectReason::InvalidStreamSpec)
            } else if state.stream_ownership.contains_key(stream_id) {
                EffectVerdict::Rejected(RejectReason::Ineffective)
            } else {
                EffectVerdict::Effective
            }
        },
        AccountOp::StreamGrant { stream_id, grantee_account_id, grant_role } => {
            let duplicate = state.grants.values().any(|grant| {
                grant.open
                    && grant.stream_id == *stream_id
                    && grant.grantee_account_id == *grantee_account_id
                    && grant.role == *grant_role
            });
            // A grant folds ONLY on an owned `PublicRead` stream (`public_streams` ⊆
            // `stream_ownership`, so this subsumes the ownership check). This is a CONSENSUS
            // rule, not a convenience: every consumer relies on "a contribution is always on a
            // public stream", and holding it in the fold — not just in the authoring crate —
            // means a hand-crafted entry from a hostile owner cannot smuggle a grant onto a
            // private stream. Relaxing it later (private grants need key wraps) is a protocol
            // version bump: an old binary would fold such a grant Effective where a new one
            // Rejects it.
            if !state.public_streams.contains(stream_id)
                || *grantee_account_id == c.header().account_id
                || duplicate
            {
                EffectVerdict::Rejected(RejectReason::Ineffective)
            } else {
                EffectVerdict::Effective
            }
        },
        AccountOp::StreamRevoke { stream_id, grantee_account_id, grant_id, .. } => {
            let matches_open_grant = state.grants.get(grant_id).is_some_and(|grant| {
                grant.open
                    && grant.stream_id == *stream_id
                    && grant.grantee_account_id == *grantee_account_id
            });
            if state.stream_ownership.contains_key(stream_id) && matches_open_grant {
                EffectVerdict::Effective
            } else {
                EffectVerdict::Rejected(RejectReason::Ineffective)
            }
        },
        // `AccountReRoot` is admissible ONLY as the terminal recovery op once the account is
        // contested (§12) — the contested path admits it. In a `Live` account it has no effect.
        AccountOp::AccountReRoot { .. } => EffectVerdict::Rejected(RejectReason::Ineffective),
        // A DeviceRemove of a device that was never enrolled is ineffective — otherwise it would
        // tombstone a fingerprint that was never added (I4), permanently barring a future
        // legitimate DeviceAdd for it.
        AccountOp::DeviceRemove { device_fingerprint, .. } =>
            if state.roster.contains_key(device_fingerprint) {
                EffectVerdict::Effective
            } else {
                EffectVerdict::Rejected(RejectReason::Ineffective)
            },
        // An OwnerDemote reaching here is an admitted cut op (register + binding decided in the
        // register pass); it is effective.
        AccountOp::OwnerDemote { .. } => EffectVerdict::Effective,
        // A control- or secrets-chain CutExtend reaching here was admitted in the register pass
        // (its register joined against the fold's account-log chains), so it is effective. A
        // content extend binds a stream chain (C2's fold), not an account log — defer it rather
        // than mark it effective on a target this fold never validated.
        AccountOp::CutExtend { chain_kind, .. } => match chain_kind {
            ChainKind::Ctrl | ChainKind::Secrets => EffectVerdict::Effective,
            ChainKind::Content => EffectVerdict::Parked(ParkReason::DeferredStreamAuthorization),
        },
    }
}

/// Classification stays pure; only the applying pass can consume an epoch.
enum EffectVerdict {
    Effective,
    Rejected(RejectReason),
    Parked(ParkReason),
}

/// Apply an EFFECTIVE op's roster/live effect (called only after an effective verdict).
fn apply_effect(c: &Candidate, state: &mut FoldState) {
    match &c.op {
        AccountOp::AccountGenesis { .. } => {
            state.genesis_seen = true;
            state.roster.insert(c.subject_device(), c.hash().into());
            state.enrollment_roles.insert(c.subject_device(), DeviceRole::Owner);
            state.owners.insert(c.subject_device(), c.hash().into());
            state.live.insert(c.hash().into());
        },
        AccountOp::DeviceAdd { device_fingerprint, role, .. } => {
            state.roster.insert(*device_fingerprint, c.hash().into());
            state.enrollment_roles.insert(*device_fingerprint, *role);
            if *role == DeviceRole::Owner {
                state.owners.insert(*device_fingerprint, c.hash().into());
                state.live.insert(c.hash().into());
            }
        },
        AccountOp::OwnerPromote { device_fingerprint } => {
            state.owners.insert(*device_fingerprint, c.hash().into());
            state.live.insert(c.hash().into());
        },
        AccountOp::StreamOwn { stream_id, stream_spec_bytes } => {
            state.stream_ownership.insert(*stream_id, c.hash());
            // Admission already validated the spec decodes and derives this id; re-decode for
            // the access mode, which the id commits to.
            if stream::decode_spec_v2(stream_spec_bytes)
                .is_ok_and(|spec| spec.access_mode == stream::AccessMode::PublicRead)
            {
                state.public_streams.insert(*stream_id);
            }
        },
        AccountOp::StreamGrant { stream_id, grantee_account_id, grant_role } => {
            state.grants.insert(c.hash().into(), LiveGrant {
                stream_id: *stream_id,
                grantee_account_id: *grantee_account_id,
                role: *grant_role,
                open: true,
            });
        },
        AccountOp::StreamRevoke { grant_id, .. } => {
            if let Some(grant) = state.grants.get_mut(grant_id) {
                grant.open = false;
            }
        },
        // An effective removal tombstones the device (I4: never re-enroll) and drops it from the
        // roster/owner sets. The register it installed handles condemning its beyond-cut entries.
        AccountOp::DeviceRemove { device_fingerprint, .. } => {
            state.roster.remove(device_fingerprint);
            state.enrollment_roles.remove(device_fingerprint);
            state.owners.remove(device_fingerprint);
            state.tombstoned.insert(*device_fingerprint);
        },
        // A demotion closes ONLY the named incarnation: if the device has since reopened a fresh
        // one (a later OwnerPromote), a stale demote naming the old `owner_id` is a no-op.
        AccountOp::OwnerDemote { device_fingerprint, .. }
            if closes_open_incarnation(&c.op, &state.owners).is_some() =>
        {
            state.owners.remove(device_fingerprint);
        },
        _ => {},
    }
}

impl FoldState {
    /// Mint exactly one effective verdict and reserve its pre-normalization position.
    fn take_epoch(&mut self) -> Outcome {
        let auth_epoch = self.next_auth_epoch;
        self.next_auth_epoch += 1;
        Outcome::Effective { auth_epoch }
    }

    fn seeded(genesis_owner_id: OwnerId) -> Self {
        Self { live: HashSet::from([genesis_owner_id]), ..Default::default() }
    }
}

/// Which authority chain an owner-incarnation lookup reads — each has its own `{chain}_boundary`,
/// `{chain}_seq`, `{chain}_hash` column triple on both the incarnation and roster tables.
#[derive(Clone, Copy)]
pub(super) enum AuthorityChain {
    Control,
    Secrets,
}

impl AuthorityChain {
    pub(super) fn column_prefix(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Secrets => "secrets",
        }
    }
}

impl RosterFact {
    fn boundary(&self, chain: AuthorityChain) -> AuthorityBoundary {
        match chain {
            AuthorityChain::Control => self.control_boundary,
            AuthorityChain::Secrets => self.secrets_boundary,
        }
    }
}

impl OwnerIncarnationFact {
    fn boundary(&self, chain: AuthorityChain) -> AuthorityBoundary {
        match chain {
            AuthorityChain::Control => self.control_boundary,
            AuthorityChain::Secrets => self.secrets_boundary,
        }
    }
}

#[cfg(test)]
#[path = "fold/tests.rs"]
mod tests;
