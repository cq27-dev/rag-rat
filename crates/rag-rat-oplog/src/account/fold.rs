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
                    ParkReason::AuthLenAhead => "auth_len_ahead",
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
                    RejectReason::InvalidStreamSpec => "invalid_stream_spec",
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
    /// A `StreamOwn` preimage is not the canonical owner-bound `/2` spec named by its account and
    /// `stream_id`.
    InvalidStreamSpec,
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
    /// The entry asserts a control-fold length not yet present locally. This is recoverable:
    /// refetch missing control ancestry and refold; the counter never grants authority.
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
pub(super) struct AccountAuthHistory {
    outcomes: HashMap<[u8; 32], Outcome>,
    classification: AccountClassification,
    /// In a `contested` account, the deterministic `AccountReRoot` successor a subscriber follows
    /// — the smallest `successor_account_id` by byte order among the admitted re-roots (§12).
    /// `None` when the account is `Live` or no pre-contest owner has re-rooted yet.
    contested_successor: Option<AccountId>,
    effective_count: u64,
    roster_refs: HashMap<[u8; 32], RosterFact>,
    owner_incarnations: HashMap<[u8; 32], OwnerIncarnationFact>,
    stream_ownership: HashMap<StreamId, StreamOwnershipFact>,
    grants: HashMap<[u8; 32], GrantFact>,
    grant_cuts: HashMap<[u8; 32], Vec<DeviceCut>>,
    /// Removed devices (I4: never re-enroll). Exported because the C6 canonical projection binds
    /// it: a snapshot that omitted tombstones would let a bootstrap re-admit a removed device.
    tombstoned: HashSet<DeviceFingerprint>,
    /// The CANONICAL root, as selected by [`find_genesis`] — not merely the first entry carrying
    /// the genesis tag. Exported because C6 signs snapshots with it as `parent_ref`: a malformed
    /// same-payload genesis (a non-null `parent_ref`, say) can be held alongside the real root and
    /// sort ahead of it by hash, and no snapshot read revalidates `parent_ref`. `None` when no
    /// valid genesis is held yet.
    genesis_hash: Option<[u8; 32]>,
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
    /// The author cites effective ops we have not folded yet.
    Ahead,
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
    Cut { seq: u64, hash: [u8; 32] },
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
    pub(super) own_id: [u8; 32],
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
    pub(super) fn outcome(&self, entry_hash: &[u8; 32]) -> Option<Outcome> {
        self.outcomes.get(entry_hash).copied()
    }

    /// The canonical root hash — see the field docs. Callers signing `parent_ref` MUST use this
    /// rather than scanning held entries for the genesis tag themselves.
    pub(super) fn genesis_hash(&self) -> Option<[u8; 32]> {
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

    pub(super) fn roster_facts(&self) -> impl Iterator<Item = (&[u8; 32], &RosterFact)> {
        self.roster_refs.iter()
    }

    pub(super) fn owner_incarnation_facts(
        &self,
    ) -> impl Iterator<Item = (&[u8; 32], &OwnerIncarnationFact)> {
        self.owner_incarnations.iter()
    }

    pub(super) fn stream_ownership_facts(
        &self,
    ) -> impl Iterator<Item = (&StreamId, &StreamOwnershipFact)> {
        self.stream_ownership.iter()
    }

    pub(super) fn grant_facts(&self) -> impl Iterator<Item = (&[u8; 32], &GrantFact)> {
        self.grants.iter()
    }

    /// Every EFFECTIVE entry with the `auth_epoch` it took, in arbitrary order. The C6 canonical
    /// projection sorts this; callers must not depend on iteration order (it is a `HashMap`).
    pub(super) fn effective_entries(&self) -> impl Iterator<Item = ([u8; 32], u64)> + '_ {
        self.outcomes.iter().filter_map(|(hash, outcome)| match outcome {
            Outcome::Effective { auth_epoch } => Some((*hash, *auth_epoch)),
            _ => None,
        })
    }

    /// Removed devices (I4). Arbitrary order — see [`Self::effective_entries`].
    pub(super) fn tombstoned(&self) -> impl Iterator<Item = &DeviceFingerprint> {
        self.tombstoned.iter()
    }

    pub(super) fn grant_cuts(&self) -> impl Iterator<Item = (&[u8; 32], &[DeviceCut])> {
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
        roster_ref: [u8; 32],
        device_fingerprint: DeviceFingerprint,
    ) -> AuthorityQuery<RosterAuthority> {
        query_fact(
            self.roster_refs
                .get(&roster_ref)
                .filter(|fact| fact.closed_at.is_none())
                .map(|fact| (fact.authority, fact.authority.device_fingerprint)),
            device_fingerprint,
            self.outcomes.contains_key(&roster_ref),
        )
    }

    pub(super) fn roster_content_authority(
        &self,
        roster_ref: [u8; 32],
        device_fingerprint: DeviceFingerprint,
        stream_id: StreamId,
    ) -> AuthorityQuery<RosterContentAuthority> {
        let Some(fact) = self.roster_refs.get(&roster_ref) else {
            return if self.outcomes.contains_key(&roster_ref) {
                AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective)
            } else {
                AuthorityQuery::Unknown
            };
        };
        if fact.authority.device_fingerprint != device_fingerprint {
            return AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject);
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
        owner_id: [u8; 32],
        device_fingerprint: DeviceFingerprint,
    ) -> AuthorityQuery<OwnerAuthority> {
        query_fact(
            self.owner_incarnations
                .get(&owner_id)
                .filter(|fact| fact.closed_at.is_none())
                .map(|fact| (fact.authority, fact.authority.device_fingerprint)),
            device_fingerprint,
            self.outcomes.contains_key(&owner_id),
        )
    }

    pub(super) fn owner_control_authority(
        &self,
        owner_id: [u8; 32],
        device_fingerprint: DeviceFingerprint,
    ) -> AuthorityQuery<OwnerChainAuthority> {
        self.owner_chain_authority(
            owner_id,
            device_fingerprint,
            |fact| fact.control_boundary,
            |fact| fact.control_boundary,
        )
    }

    pub(super) fn owner_secrets_authority(
        &self,
        owner_id: [u8; 32],
        device_fingerprint: DeviceFingerprint,
    ) -> AuthorityQuery<OwnerChainAuthority> {
        self.owner_chain_authority(
            owner_id,
            device_fingerprint,
            |fact| fact.secrets_boundary,
            |fact| fact.secrets_boundary,
        )
    }

    fn owner_chain_authority(
        &self,
        owner_id: [u8; 32],
        device_fingerprint: DeviceFingerprint,
        device_boundary: impl Fn(&RosterFact) -> AuthorityBoundary,
        incarnation_boundary: impl Fn(&OwnerIncarnationFact) -> AuthorityBoundary,
    ) -> AuthorityQuery<OwnerChainAuthority> {
        let Some(owner) = self.owner_incarnations.get(&owner_id) else {
            return if self.outcomes.contains_key(&owner_id) {
                AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective)
            } else {
                AuthorityQuery::Unknown
            };
        };
        if owner.authority.device_fingerprint != device_fingerprint {
            return AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject);
        }
        let device = self
            .roster_refs
            .values()
            .find(|fact| fact.authority.device_fingerprint == device_fingerprint)
            .map_or(AuthorityBoundary::Closed, device_boundary);
        AuthorityQuery::Effective(OwnerChainAuthority {
            owner: owner.authority,
            device_boundary: device,
            incarnation_boundary: incarnation_boundary(owner),
        })
    }

    pub(super) fn grant_effective(
        &self,
        grant_id: [u8; 32],
        stream_id: StreamId,
        grantee_account_id: AccountId,
    ) -> AuthorityQuery<GrantAuthority> {
        let Some(fact) = self.grants.get(&grant_id) else {
            return if self.outcomes.contains_key(&grant_id) {
                AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective)
            } else {
                AuthorityQuery::Unknown
            };
        };
        if fact.authority.stream_id != stream_id
            || fact.authority.grantee_account_id != grantee_account_id
        {
            return AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject);
        }
        AuthorityQuery::Effective(fact.authority)
    }

    pub(super) fn stream_owner_effective(&self, stream_id: StreamId) -> AuthorityQuery<[u8; 32]> {
        let Some(fact) = self.stream_ownership.get(&stream_id) else {
            return AuthorityQuery::Unknown;
        };
        AuthorityQuery::Effective(fact.own_id)
    }

    fn is_effective(&self, entry_hash: &[u8; 32]) -> bool {
        self.outcome(entry_hash).is_some_and(|o| o.is_effective())
    }
}

fn query_fact<T: Copy, S: PartialEq>(
    fact: Option<(T, S)>,
    expected_subject: S,
    reference_is_known: bool,
) -> AuthorityQuery<T> {
    let Some((authority, subject)) = fact else {
        return if reference_is_known {
            AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective)
        } else {
            AuthorityQuery::Unknown
        };
    };
    if subject != expected_subject {
        return AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject);
    }
    AuthorityQuery::Effective(authority)
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
    /// Immutable role granted by the effective enrollment entry. `OwnerPromote` is deliberately
    /// limited to authoring-capable enrollments: otherwise a later promotion could retroactively
    /// re-bless content a read-only device authored before it had write authority.
    enrollment_roles: HashMap<DeviceFingerprint, DeviceRole>,
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
    /// Effective immutable stream ownership roots, keyed by owner-bound stream id.
    stream_ownership: HashMap<StreamId, [u8; 32]>,
    /// Effective grant incarnations. A revoke closes exactly one id; a later grant gets a fresh
    /// hash and remains independent.
    grants: HashMap<[u8; 32], LiveGrant>,
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
    headers: &'a HashMap<[u8; 32], &'a AccountEntryHeader>,
}

impl HeaderView for CandidateView<'_> {
    fn header(&self, entry_hash: &[u8; 32]) -> Option<&AccountEntryHeader> {
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
    let owner_register = |log: u8, device: DeviceFingerprint, owner_id: [u8; 32], cut: &Cut| {
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
            owner_id: *owner_id,
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
                    || candidate::ancestry(&y.op.hash(), cut, view) == Ancestry::OffBranch)
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
    // Readiness is monotone: once an entry proves it was authored ahead of the locally folded
    // authority history (or depends on authority that did not survive the fold), it cannot
    // contribute a register or phase-E mutation in this fold. Re-run the frozen stratified pass
    // with those entries held out until no new readiness exclusions appear. This is not a graph
    // fixpoint: each pass still performs exactly the §11.1 one-way, per-depth register fold, and
    // exclusions only grow.
    let mut readiness_exclusions = HashMap::new();
    loop {
        let (history, discovered) = fold_account_pass(entries, &readiness_exclusions);
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
            return history;
        }
    }
}

fn fold_account_pass(
    entries: &[VerifiedAccountEntry],
    readiness_exclusions: &HashMap<[u8; 32], Outcome>,
) -> (AccountAuthHistory, HashMap<[u8; 32], Outcome>) {
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
        );
    };
    let genesis_owner_id = genesis.hash();
    let genesis_founder = genesis.subject_device();

    let mut incarnations = Incarnations::build(&candidates, genesis_owner_id);

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
    let mut state = FoldState { live: HashSet::from([genesis_owner_id]), ..Default::default() };
    // The revocation registers accumulated so far (extend-only, joined by `⊔`), every entry a
    // register condemns (grows monotonically — a lower-depth decision is final, no oscillation) /
    // parks (rebuilt fresh each depth), and the cut ops decided in the register pass (a binding
    // failure / I2).
    let mut registers: HashMap<RegisterKey, Cut> = HashMap::new();
    let mut condemned: HashMap<[u8; 32], CondemnedReason> = HashMap::new();
    let mut parked: HashMap<[u8; 32], ParkReason> = HashMap::new();
    let mut cut_verdicts: HashMap<[u8; 32], Outcome> = HashMap::new();
    let mut register_contributors: HashSet<[u8; 32]> = HashSet::new();
    let mut classification = AccountClassification::Live;

    'depths: for (&depth, idxs) in &strata {
        let view = CandidateView { headers: &all_headers };

        // (a) REGISTER PASS. A cut op installs a register iff its author is AUTHORIZED
        // (authority_status == Live: its cited incarnation resolves to a mint for the SIGNER
        // and is live — the transitive-liveness gate that defeats laundering AND
        // owner impersonation), it passes cut-target binding (§11.3), and — for
        // OwnerDemote — its target `owner_id` names its subject device.
        let mut admitted: Vec<AdmittedCut<'_>> = Vec::new();
        for &i in idxs {
            let c = &candidates[i];
            // A condemned OR parked cut op installs nothing — parked authority (its own chain is
            // under a not-yet-decided watermark) must not have register side effects before it is
            // on a known-valid branch.
            if condemned.contains_key(&c.hash()) || parked.contains_key(&c.hash()) {
                continue;
            }
            let op_registers = cut_op_registers(c);
            if op_registers.is_empty() {
                continue; // not a cut op
            }
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
                candidate::validate_cut_target(cut, coord, &view) == candidate::CutBinding::Mismatch
            });
            if misbound {
                cut_verdicts.insert(c.hash(), Outcome::Rejected(RejectReason::CutTargetMismatch));
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

        // INTRINSIC last-owner prefilter (order-free, §12/I2). A cut that closes the SOLE
        // prior-depth owner can never succeed under ANY processing order — a size-1 surviving set
        // never shrinks, so the sequential I2 sim would reject it as `LastOwner` whatever the sort
        // — so reject it HERE and drop it BEFORE the park preflight, cycle-detection, AND
        // the I2 sim. An intrinsically-dead cut must not park, contest, or form a
        // condemnation cycle: this is what makes a SOLE owner's equivocating self-removals
        // fold `Live` (each rejected `LastOwner`) instead of manufacturing a contested cut
        // (incomparable variant) or a same-device 2-cycle (same-cut variant, which
        // `has_condemn_cycle` WOULD flag). Keyed on the SAME `closes` predicate the I2 sim
        // uses, against the prior-depth `state.owners` (empty at stratum 0, so genesis / a
        // founder self-remove is never intrinsic here) — NEVER on "is a self-removal". The
        // multi-owner mutual-removal case (owner set > 1) is untouched and still
        // reaches `has_condemn_cycle` before I2, so it folds contested (§12).
        if state.owners.len() == 1 {
            admitted.retain(|a| {
                let closes_sole_owner = match &a.op.op {
                    AccountOp::DeviceRemove { device_fingerprint, .. } =>
                        state.owners.contains_key(device_fingerprint),
                    AccountOp::OwnerDemote { device_fingerprint, owner_id, .. } =>
                        state.owners.get(device_fingerprint) == Some(owner_id),
                    _ => false,
                };
                if closes_sole_owner {
                    cut_verdicts.insert(a.op.hash(), Outcome::Rejected(RejectReason::LastOwner));
                    return false;
                }
                true
            });
        }

        // Decide which admitted cut ops will actually INSTALL registers this depth, BEFORE
        // cycle-detection and the I2 last-owner simulation consume `admitted`. One signed op cuts
        // BOTH the device's control chain and its secrets chain, and its registers commit
        // ATOMICALLY: if EITHER chain's join is undecidable the WHOLE op raises NO register, so it
        // is NOT an active cut this depth — it parks `UnknownCutTarget` and must not manufacture a
        // mutual-condemnation cycle or reserve a surviving owner it never actually removes (the
        // ordering bug: a would-be-parked op left in `admitted` would wrongly drive cycle/I2). An
        // incomparable pair (→ Contested) is genuine owner-key compromise and still halts. The
        // decision runs against a WORKING copy so a same-key same-depth op sees the prior op's
        // would-be watermark, while the real register set stays untouched until after cycle/I2 (a
        // Contested must leave this depth's registers uninstalled).
        {
            let mut working = registers.clone();
            let mut parked: HashSet<[u8; 32]> = HashSet::new();
            for a in &admitted {
                let mut any_parked = false;
                for (key, cut) in &a.registers {
                    match join_register_peek(&working, key, cut, &view) {
                        RegisterJoin::Applied => {},
                        RegisterJoin::Contested => {
                            classification =
                                AccountClassification::Contested { state_before_depth: depth };
                            break 'depths;
                        },
                        RegisterJoin::Parked => any_parked = true,
                    }
                }
                if any_parked {
                    cut_verdicts.insert(a.op.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                    parked.insert(a.op.hash());
                } else {
                    // Apply so a same-key same-depth op joins against this op's would-be watermark.
                    for (key, cut) in &a.registers {
                        join_register(&mut working, key.clone(), cut.clone(), &view);
                    }
                }
            }
            admitted.retain(|a| !parked.contains(&a.op.hash()));
        }

        // A same-depth mutual owner-condemnation cycle is genuine owner-key compromise (§12):
        // halt at the last cycle-free stratum. Detected BEFORE I2 so a two-owner
        // mutual removal folds contested rather than being resolved by reserving
        // one owner. Parked ops are already excluded above — a cut that installs nothing is not a
        // cycle participant.
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

        // Stage ALL of this depth's register changes (creator cuts + `CutExtend`s) in ONE working
        // copy, then merge into the REAL register set only at the END of a NON-contested depth.
        // This is the class fix for "partial register mutation on a contested stratum": every
        // `break 'depths` still reachable below (an incomparable extend) must leave the real
        // registers EXACTLY as the prior depth left them (§12 `state_before_depth`), so a
        // half-applied cut whose stratum then halts cannot leak its watermark into
        // `derive_authority_facts`. (The creator-sim and cycle breaks above already run before any
        // real mutation; this staging covers the two remaining mutation sites — the creator commit
        // and the extends join — which both precede the extends contest break.)
        let mut depth_registers = registers.clone();

        // Commit the surviving admitted cut ops' registers (§11.3 `⊔`) into the staging copy. The
        // park / contested / incomparable decisions were all made above against the working copy,
        // and I2 only REMOVES ops (same-key removers are all-rejected-or-all-kept together, so a
        // kept op never loses a same-key predecessor), so every remaining register here joins
        // `Applied`.
        for a in &admitted {
            for (key, cut) in &a.registers {
                join_register(&mut depth_registers, key.clone(), cut.clone(), &view);
            }
            register_contributors.insert(a.op.hash());
        }

        // Raise registers with this depth's live `CutExtend`s (§11.4 recovery). An extend is
        // EXTEND-ONLY: it may only raise a register a prior DeviceRemove/OwnerDemote created (this
        // depth's creators are already in `depth_registers`), never conjure a fresh one (else a
        // live owner could condemn a chain with a bare extend). An extend for a not-yet-established
        // register parks until the creator syncs.
        let mut extends: Vec<(&Candidate, RegisterKey, Cut)> = Vec::new();
        for &i in idxs {
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
            if !depth_registers.contains_key(&key) {
                cut_verdicts.insert(c.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                continue;
            }
            extends.push((c, key, cut));
        }
        extends.sort_by_key(|(c, _, _)| c.hash());
        for (c, key, cut) in extends {
            // Join into the STAGING copy. A same-key same-depth extend joins against the prior
            // extend's watermark; an incomparable pair (→ Contested) is owner-key compromise and
            // breaks 'depths with the REAL registers still untouched — `depth_registers` is
            // dropped, never merged, so no watermark leaks from the halted stratum.
            match join_register(&mut depth_registers, key, cut, &view) {
                RegisterJoin::Applied => {
                    register_contributors.insert(c.hash());
                },
                RegisterJoin::Contested => {
                    classification = AccountClassification::Contested { state_before_depth: depth };
                    break 'depths;
                },
                RegisterJoin::Parked => {
                    cut_verdicts.insert(c.hash(), Outcome::Parked(ParkReason::UnknownCutTarget));
                },
            }
        }

        // No contest this depth — merge the staged changes into the real register set. The
        // condemnation scan below and every later depth now see this depth's creators + extends.
        registers = depth_registers;

        // Re-derive condemnation + parking against the current registers. Condemnation grows
        // monotonically: the frozen stratified model never lets a deeper authority revise a
        // lower-depth decision. Parking is rebuilt because missing ancestry can arrive later.
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
                            state.enrollment_roles.remove(&c.subject_device());
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
            let outcome = classify_effect(c, &incarnations, &state, &parked);
            if let Outcome::Effective { .. } = outcome {
                apply_effect(c, &mut state);
            }
            outcomes.insert(c.hash(), outcome);
        }
    }

    // Registers and their per-depth condemnation decisions are now final. Rebuild ONLY phase-E
    // state against those fixed verdicts so a retroactively condemned non-mint cannot leave a
    // roster, tombstone, ownership, or grant mutation behind. This is deliberately not a graph
    // fixpoint: no register is added/removed and no lower-depth condemnation is revised.
    let replay_before_depth = match classification {
        AccountClassification::Live => None,
        AccountClassification::Contested { state_before_depth } => Some(state_before_depth),
    };
    state = replay_effect_state(
        &candidates,
        &strata,
        &incarnations,
        genesis_owner_id,
        replay_before_depth,
        &condemned,
        &parked,
        &cut_verdicts,
        &mut outcomes,
    );

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

    let mut discovered = close_final_authority_dependencies(&candidates, &mut outcomes);
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
    // Effective candidates use `effective_count - 1` inside the closure above. Also inspect
    // condemned, contested, and actual register contributors against the fully-folded count: a
    // mint can provisionally authorize the descendant cut that later condemns it; mutually-
    // condemning ahead cuts can manufacture `Contested`; and a cut may install a register before
    // phase E rejects it as ineffective (for example, removing a never-enrolled device).
    // Structurally rejected and ordinarily parked candidates retain their stronger verdict when
    // they contributed neither state nor a register.
    for candidate in &candidates {
        if !readiness_exclusions.contains_key(&candidate.hash())
            && (register_contributors.contains(&candidate.hash())
                || matches!(
                    outcomes.get(&candidate.hash()),
                    Some(Outcome::Condemned(_) | Outcome::Parked(ParkReason::ContestedSubject))
                ))
            && candidate.header().auth_len > effective_count
        {
            discovered.insert(candidate.hash(), Outcome::Parked(ParkReason::AuthLenAhead));
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
    )
}

#[expect(clippy::too_many_arguments, reason = "fixed fold verdicts are explicit replay inputs")]
fn replay_effect_state(
    candidates: &[Candidate],
    strata: &BTreeMap<usize, Vec<usize>>,
    incarnations: &Incarnations<'_>,
    genesis_owner_id: [u8; 32],
    before_depth: Option<usize>,
    condemned: &HashMap<[u8; 32], CondemnedReason>,
    parked: &HashMap<[u8; 32], ParkReason>,
    cut_verdicts: &HashMap<[u8; 32], Outcome>,
    outcomes: &mut HashMap<[u8; 32], Outcome>,
) -> FoldState {
    let mut state = FoldState { live: HashSet::from([genesis_owner_id]), ..Default::default() };
    for (&depth, idxs) in strata {
        if before_depth.is_some_and(|limit| depth >= limit) {
            break;
        }
        let mut ordered = idxs.clone();
        ordered.sort_by_key(|&i| {
            let header = candidates[i].header();
            (header.device_fingerprint.to_bytes(), header.seq, candidates[i].hash())
        });
        for i in ordered {
            let candidate = &candidates[i];
            let outcome = if let Some(reason) = condemned.get(&candidate.hash()) {
                Outcome::Condemned(*reason)
            } else if let Some(reason) = parked.get(&candidate.hash()) {
                Outcome::Parked(*reason)
            } else if let Some(verdict) = cut_verdicts.get(&candidate.hash()) {
                *verdict
            } else {
                classify_effect(candidate, incarnations, &state, parked)
            };
            if outcome.is_effective() {
                apply_effect(candidate, &mut state);
            }
            outcomes.insert(candidate.hash(), outcome);
        }
    }
    state
}

fn normalize_auth_epochs(outcomes: &mut HashMap<[u8; 32], Outcome>) -> u64 {
    let mut effective: Vec<([u8; 32], u64)> = outcomes
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

/// Final fail-closed dependency closure after fixed-register phase-E replay. No authority fact may
/// survive without its final-effective roster / ownership / grant prerequisite. Freshness is also
/// decided against the fully folded count here — never against transient hash iteration order, and
/// never as an authority input.
fn close_final_authority_dependencies(
    candidates: &[Candidate],
    outcomes: &mut HashMap<[u8; 32], Outcome>,
) -> HashMap<[u8; 32], Outcome> {
    let mut discovered = HashMap::new();
    loop {
        let effective_count = normalize_auth_epochs(outcomes);
        let effective_roster: HashSet<DeviceFingerprint> = candidates
            .iter()
            .filter(|candidate| outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective))
            .filter_map(|candidate| match candidate.op {
                AccountOp::AccountGenesis { .. } => Some(candidate.header().device_fingerprint),
                AccountOp::DeviceAdd { device_fingerprint, .. } => Some(device_fingerprint),
                _ => None,
            })
            .collect();
        let effective_ownership: HashSet<StreamId> = candidates
            .iter()
            .filter(|candidate| outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective))
            .filter_map(|candidate| match candidate.op {
                AccountOp::StreamOwn { stream_id, .. } => Some(stream_id),
                _ => None,
            })
            .collect();
        let effective_grants: HashMap<[u8; 32], (StreamId, AccountId)> = candidates
            .iter()
            .filter(|candidate| outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective))
            .filter_map(|candidate| match candidate.op {
                AccountOp::StreamGrant { stream_id, grantee_account_id, .. } =>
                    Some((candidate.hash(), (stream_id, grantee_account_id))),
                _ => None,
            })
            .collect();

        let mut changed = false;
        for candidate in candidates {
            if !outcomes.get(&candidate.hash()).is_some_and(Outcome::is_effective) {
                continue;
            }
            let replacement = if candidate.header().auth_len > effective_count.saturating_sub(1) {
                Some(Outcome::Parked(ParkReason::AuthLenAhead))
            } else if let Some(authority_ref) = candidate.header().authority_ref
                && !outcomes.get(&authority_ref).is_some_and(Outcome::is_effective)
            {
                Some(match outcomes.get(&authority_ref) {
                    Some(Outcome::Parked(reason)) => Outcome::Parked(*reason),
                    _ => Outcome::Rejected(RejectReason::StaleAuthority),
                })
            } else {
                match &candidate.op {
                    AccountOp::OwnerPromote { device_fingerprint }
                        if !effective_roster.contains(device_fingerprint) =>
                        Some(Outcome::Rejected(RejectReason::Ineffective)),
                    AccountOp::StreamGrant { stream_id, .. }
                        if !effective_ownership.contains(stream_id) =>
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
                // Only freshness is monotone across readiness passes. A stale citation or failed
                // state precondition can recover after an ahead competing effect is excluded, so
                // those verdicts must be recomputed rather than permanently held out.
                if replacement == Outcome::Parked(ParkReason::AuthLenAhead) {
                    discovered.insert(candidate.hash(), replacement);
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    discovered
}

#[derive(Default)]
struct AuthorityFacts {
    roster_refs: HashMap<[u8; 32], RosterFact>,
    owner_incarnations: HashMap<[u8; 32], OwnerIncarnationFact>,
    stream_ownership: HashMap<StreamId, StreamOwnershipFact>,
    grants: HashMap<[u8; 32], GrantFact>,
    grant_cuts: HashMap<[u8; 32], Vec<DeviceCut>>,
}

fn derive_authority_facts(
    candidates: &[Candidate],
    outcomes: &HashMap<[u8; 32], Outcome>,
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
    let mut roster = HashMap::<DeviceFingerprint, [u8; 32]>::new();
    let mut owners = HashMap::<DeviceFingerprint, [u8; 32]>::new();
    for (candidate, epoch) in effective {
        match &candidate.op {
            AccountOp::AccountGenesis { .. } => {
                let device = candidate.subject_device();
                let hash = candidate.hash();
                facts.roster_refs.insert(hash, RosterFact {
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
                facts.owner_incarnations.insert(hash, OwnerIncarnationFact {
                    authority: OwnerAuthority { device_fingerprint: device },
                    effective_at: epoch,
                    closed_at: None,
                    control_boundary: AuthorityBoundary::Open,
                    secrets_boundary: AuthorityBoundary::Open,
                });
                roster.insert(device, hash);
                owners.insert(device, hash);
            },
            AccountOp::DeviceAdd { device_fingerprint, role, .. } => {
                let hash = candidate.hash();
                facts.roster_refs.insert(hash, RosterFact {
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
                roster.insert(*device_fingerprint, hash);
                if *role == DeviceRole::Owner {
                    facts.owner_incarnations.insert(hash, OwnerIncarnationFact {
                        authority: OwnerAuthority { device_fingerprint: *device_fingerprint },
                        effective_at: epoch,
                        closed_at: None,
                        control_boundary: AuthorityBoundary::Open,
                        secrets_boundary: AuthorityBoundary::Open,
                    });
                    owners.insert(*device_fingerprint, hash);
                }
            },
            AccountOp::OwnerPromote { device_fingerprint } => {
                let hash = candidate.hash();
                let roster_ref = roster
                    .get(device_fingerprint)
                    .expect("promoted device has an active roster fact");
                facts
                    .roster_refs
                    .get_mut(roster_ref)
                    .expect("active roster fact")
                    .authority
                    .current_role = DeviceRole::Owner;
                facts.owner_incarnations.insert(hash, OwnerIncarnationFact {
                    authority: OwnerAuthority { device_fingerprint: *device_fingerprint },
                    effective_at: epoch,
                    closed_at: None,
                    control_boundary: AuthorityBoundary::Open,
                    secrets_boundary: AuthorityBoundary::Open,
                });
                owners.insert(*device_fingerprint, hash);
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
                if owners.get(device_fingerprint) == Some(owner_id) {
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
                facts.grants.insert(candidate.hash(), GrantFact {
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
            let authoring_role = state
                .enrollment_roles
                .get(device_fingerprint)
                .is_some_and(|role| role.can_author_content());
            if enrolled && authoring_role && !already_owner && !tombstoned {
                effective(state)
            } else {
                Outcome::Rejected(RejectReason::BadPromote)
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
                Outcome::Rejected(RejectReason::InvalidStreamSpec)
            } else if state.stream_ownership.contains_key(stream_id) {
                Outcome::Rejected(RejectReason::Ineffective)
            } else {
                effective(state)
            }
        },
        AccountOp::StreamGrant { stream_id, grantee_account_id, grant_role } => {
            let duplicate = state.grants.values().any(|grant| {
                grant.open
                    && grant.stream_id == *stream_id
                    && grant.grantee_account_id == *grantee_account_id
                    && grant.role == *grant_role
            });
            if !state.stream_ownership.contains_key(stream_id)
                || *grantee_account_id == c.header().account_id
                || duplicate
            {
                Outcome::Rejected(RejectReason::Ineffective)
            } else {
                effective(state)
            }
        },
        AccountOp::StreamRevoke { stream_id, grantee_account_id, grant_id, .. } => {
            let matches_open_grant = state.grants.get(grant_id).is_some_and(|grant| {
                grant.open
                    && grant.stream_id == *stream_id
                    && grant.grantee_account_id == *grantee_account_id
            });
            if state.stream_ownership.contains_key(stream_id) && matches_open_grant {
                effective(state)
            } else {
                Outcome::Rejected(RejectReason::Ineffective)
            }
        },
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
        // A control- or secrets-chain CutExtend reaching here was admitted in the register pass
        // (its register joined against the fold's account-log chains), so it is effective. A
        // content extend binds a stream chain (C2's fold), not an account log — defer it rather
        // than mark it effective on a target this fold never validated.
        AccountOp::CutExtend { chain_kind, .. } => match chain_kind {
            ChainKind::Ctrl | ChainKind::Secrets => effective(state),
            ChainKind::Content => Outcome::Parked(ParkReason::DeferredStreamAuthorization),
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
            state.enrollment_roles.insert(c.subject_device(), DeviceRole::Owner);
            state.owners.insert(c.subject_device(), c.hash());
            state.live.insert(c.hash());
        },
        AccountOp::DeviceAdd { device_fingerprint, role, .. } => {
            state.roster.insert(*device_fingerprint, c.hash());
            state.enrollment_roles.insert(*device_fingerprint, *role);
            if *role == DeviceRole::Owner {
                state.owners.insert(*device_fingerprint, c.hash());
                state.live.insert(c.hash());
            }
        },
        AccountOp::OwnerPromote { device_fingerprint } => {
            state.owners.insert(*device_fingerprint, c.hash());
            state.live.insert(c.hash());
        },
        AccountOp::StreamOwn { stream_id, .. } => {
            state.stream_ownership.insert(*stream_id, c.hash());
        },
        AccountOp::StreamGrant { stream_id, grantee_account_id, grant_role } => {
            state.grants.insert(c.hash(), LiveGrant {
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
    use crate::account::envelope::{sign_account_entry, verify_account_signed};
    use crate::account::{AccountId, ops as account_ops, snapshot};
    use crate::device::{DeviceSecret, DeviceX25519Secret};
    use crate::stream::{StreamId, StreamSpec, StreamSpecV2};

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
            let payload = account_ops::encode(&op).unwrap();
            let account_id = account_id_from_genesis_payload(&payload);
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
            self.author_at_auth_len(author, authority_ref, op, 1)
        }

        fn author_at_auth_len(
            &mut self,
            author: &Dev,
            authority_ref: Option<[u8; 32]>,
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
                prev_hash: prev,
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
            hash
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
            prev_hash: Option<[u8; 32]>,
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
        f.author(&founder, Some(g), &device_add(&owner_b, DeviceRole::Owner));
        f.author(&founder, Some(g), &device_add(&member, DeviceRole::Member));
        f.author(&founder, Some(g), &device_add(&removed, DeviceRole::Member));
        let (stream_id, own) = stream_own(f.account_id);
        f.author(&founder, Some(g), &own);
        f.author(&founder, Some(g), &AccountOp::StreamGrant {
            stream_id,
            grantee_account_id: AccountId::from_bytes([0x9a; 32]),
            grant_role: GrantRole::Writer,
        });
        // `Cut::Empty` because `removed` never authored an entry: a `Cut::At` would name a
        // coordinate on its chain that does not exist, and the remove would park
        // `unknown_cut_target` instead of taking effect — leaving the fixture with no tombstone at
        // all, which is exactly what `a_different_covered_prefix_projects_differently` caught.
        f.author(&founder, Some(g), &AccountOp::DeviceRemove {
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
            let slot = heads.entry(h.device_fingerprint).or_insert((h.seq, entry.entry_hash));
            if h.seq >= slot.0 {
                *slot = (h.seq, entry.entry_hash);
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
                    entry_hash,
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
            Some(g),
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
        assert_eq!(
            hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "f6cace33757ebd07c34e076bc6078233321e857292c422e3b2b16940cbe7cb52",
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
        roster.author(&Dev::new(1), Some(g), &device_add(&Dev::new(7), DeviceRole::Member));
        assert_ne!(
            snapshot::projection::folded_state_hash(&roster.fold()),
            base,
            "an added device must change the hash",
        );

        // A tombstone change (I4's set is bound, so a bootstrap cannot re-admit a removed device).
        let mut tombstone = projection_fixture();
        let g = tombstone.genesis_hash;
        let victim = Dev::new(3);
        tombstone.author(&Dev::new(1), Some(g), &AccountOp::DeviceRemove {
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
        let (stream_id, _) = stream_own(grant.account_id);
        grant.author(&Dev::new(1), Some(g), &AccountOp::StreamGrant {
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
            snapshot::projection::folded_state_hash(&f.fold_without(withheld)),
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

    fn owner_demote(dev: &Dev, owner_id: [u8; 32], control_cut: Cut) -> AccountOp {
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
        owner_id: [u8; 32],
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

    fn stream_own(account_id: AccountId) -> (StreamId, AccountOp) {
        let spec = StreamSpecV2 {
            owner_account_id: account_id,
            policy: StreamSpec {
                repo_set: vec!["repo-a".to_string()],
                kind_allow_list: None,
                relation_policy: None,
                node_overrides: Vec::new(),
            },
            access_mode: crate::stream::AccessMode::Private,
        };
        let stream_id = stream::derive_v2(&spec).unwrap();
        let stream_spec_bytes = stream::canonical_spec_v2_bytes(&spec).unwrap();
        (stream_id, AccountOp::StreamOwn { stream_id, stream_spec_bytes })
    }

    fn stream_grant(stream: StreamId, grantee: AccountId) -> AccountOp {
        AccountOp::StreamGrant {
            stream_id: stream,
            grantee_account_id: grantee,
            grant_role: ops::GrantRole::Reader,
        }
    }

    fn stream_revoke(stream: StreamId, grantee: AccountId, grant_id: [u8; 32]) -> AccountOp {
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

    /// A `CutExtend` raising `subject`'s SECRETS-chain register (device-level when `incarnation_id`
    /// is `None`, owner-incarnation otherwise) to `[new_seq, new_entry_hash]`.
    fn cut_extend_secrets(
        account: AccountId,
        subject: &Dev,
        incarnation_id: Option<[u8; 32]>,
        new_seq: u64,
        new_entry_hash: [u8; 32],
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
            Some(f.genesis_hash),
            &device_add(&ahead_device, DeviceRole::Member),
            10,
        );
        let behind = f.author_at_auth_len(
            &founder,
            Some(f.genesis_hash),
            &device_add(&behind_device, DeviceRole::Member),
            0,
        );

        let h = f.fold();
        assert_eq!(h.outcome(&ahead), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
        assert_eq!(
            h.roster_ref_effective(ahead, ahead_device.fp),
            AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
            "an ahead candidate must not leave a projected roster mutation",
        );
        assert!(h.is_effective(&behind), "a stale count is informational, never authority");
    }

    #[test]
    fn a_candidate_cannot_satisfy_its_own_auth_len() {
        let (founder, device) = (Dev::new(1), Dev::new(2));
        let mut f = Fixture::genesis(&founder);
        let ahead = f.author_at_auth_len(
            &founder,
            Some(f.genesis_hash),
            &device_add(&device, DeviceRole::Member),
            2,
        );

        let h = f.fold();
        assert_eq!(h.effective_count(), 1, "only genesis preceded the candidate");
        assert_eq!(h.outcome(&ahead), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
    }

    #[test]
    fn an_auth_len_ahead_cut_has_no_register_side_effect() {
        let (founder, b, member) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let mut f = Fixture::genesis(&founder);
        let add_b = f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let ahead_remove = f.author_at_auth_len(
            &founder,
            Some(f.genesis_hash),
            &device_remove(&b, Cut::Empty),
            u64::MAX,
        );
        let add_member = f.author(&b, Some(add_b), &device_add(&member, DeviceRole::Member));

        let h = f.fold();
        assert_eq!(h.outcome(&ahead_remove), Some(Outcome::Parked(ParkReason::AuthLenAhead)),);
        assert!(h.is_effective(&add_member), "a parked cut must not condemn B's chain");
        assert_eq!(h.classification(), AccountClassification::Live);
    }

    #[test]
    fn an_ahead_ineffective_cut_cannot_poison_a_later_enrollment() {
        let (founder, device, member) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let mut f = Fixture::genesis(&founder);
        let ahead_remove = f.author_at_auth_len(
            &founder,
            Some(f.genesis_hash),
            &device_remove(&device, Cut::Empty),
            u64::MAX,
        );
        let add_device =
            f.author(&founder, Some(f.genesis_hash), &device_add(&device, DeviceRole::Owner));
        let add_member =
            f.author(&device, Some(add_device), &device_add(&member, DeviceRole::Member));

        let h = f.fold();
        assert_eq!(
            h.outcome(&ahead_remove),
            Some(Outcome::Parked(ParkReason::AuthLenAhead)),
            "freshness dominates the phase-E ineffective verdict for a register contributor",
        );
        assert!(h.is_effective(&add_device));
        assert!(
            h.is_effective(&add_member),
            "the rejected ahead cut must leave no empty register on the enrolled device",
        );
    }

    #[test]
    fn an_auth_len_ahead_owner_mint_cannot_authorize_a_descendant_cut() {
        let (founder, b) = (Dev::new(1), Dev::new(2));
        let mut f = Fixture::genesis(&founder);
        let ahead_add = f.author_at_auth_len(
            &founder,
            Some(f.genesis_hash),
            &device_add(&b, DeviceRole::Owner),
            u64::MAX,
        );
        let remove_founder = f.author(&b, Some(ahead_add), &device_remove(&founder, Cut::Empty));

        let h = f.fold();
        assert_eq!(h.outcome(&ahead_add), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
        assert_eq!(
            h.outcome(&remove_founder),
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
            Some(f.genesis_hash),
            &device_add(&device, DeviceRole::Member),
            u64::MAX,
        );
        let valid =
            f.author(&founder, Some(f.genesis_hash), &device_add(&device, DeviceRole::Member));
        let promote = f.author(&founder, Some(f.genesis_hash), &owner_promote(&device));

        let h = f.fold();
        assert_eq!(h.outcome(&ahead), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
        assert!(h.is_effective(&valid), "the parked add must not cause DuplicateAdd");
        assert!(
            h.is_effective(&promote),
            "the promotion must recover with the valid enrollment on the next readiness pass",
        );
        assert_eq!(
            h.roster_ref_effective(valid, device.fp),
            AuthorityQuery::Effective(RosterAuthority {
                device_fingerprint: device.fp,
                current_role: DeviceRole::Owner,
            }),
        );
        assert_eq!(
            h.owner_incarnation_effective(promote, device.fp),
            AuthorityQuery::Effective(OwnerAuthority { device_fingerprint: device.fp }),
        );
    }

    #[test]
    fn a_dependent_grant_recovers_after_an_ahead_ownership_competitor_is_excluded() {
        let founder = Dev::new(1);
        let grantee = AccountId::from_bytes([0x44; 32]);
        let mut f = Fixture::genesis(&founder);
        let (stream, own) = stream_own(f.account_id);
        let ahead_own = f.author_at_auth_len(&founder, Some(f.genesis_hash), &own, u64::MAX);
        let valid_own = f.author(&founder, Some(f.genesis_hash), &own);
        let grant = f.author(&founder, Some(f.genesis_hash), &stream_grant(stream, grantee));

        let h = f.fold();
        assert_eq!(h.outcome(&ahead_own), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
        assert!(h.is_effective(&valid_own), "the valid StreamOwn must replace its ahead twin");
        assert!(
            h.is_effective(&grant),
            "a state-dependent grant must be recomputed, not permanently readiness-excluded",
        );
    }

    #[test]
    fn mutually_condemning_ahead_cuts_do_not_manufacture_contested() {
        let (founder, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let mut f = Fixture::genesis(&founder);
        let add_a = f.author(&founder, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let remove_b =
            f.author_at_auth_len(&a, Some(add_a), &device_remove(&b, Cut::Empty), u64::MAX);
        let remove_a =
            f.author_at_auth_len(&b, Some(add_b), &device_remove(&a, Cut::Empty), u64::MAX);

        let h = f.fold();
        assert_eq!(h.classification(), AccountClassification::Live);
        assert_eq!(h.outcome(&remove_a), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
        assert_eq!(h.outcome(&remove_b), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
        assert!(h.is_effective(&add_a) && h.is_effective(&add_b));
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
    fn an_auth_len_ahead_reroot_cannot_select_the_contested_successor() {
        let (founder, a, b) = (Dev::new(1), Dev::new(2), Dev::new(3));
        let (ahead_small, valid_big) =
            (AccountId::from_bytes([0x11; 32]), AccountId::from_bytes([0x22; 32]));
        let mut f = Fixture::genesis(&founder);
        let add_a = f.author(&founder, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        f.author(&a, Some(add_a), &device_remove(&b, Cut::Empty));
        f.author(&b, Some(add_b), &device_remove(&a, Cut::Empty));
        let ahead = f.author_at_auth_len(&a, Some(add_a), &account_reroot(ahead_small), u64::MAX);
        let valid = f.author(&a, Some(add_a), &account_reroot(valid_big));

        let h = f.fold();
        assert_eq!(h.outcome(&ahead), Some(Outcome::Parked(ParkReason::AuthLenAhead)));
        assert!(h.is_effective(&valid));
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
    fn stream_ownership_grant_and_revoke_are_folded_as_citation_authority() {
        let fdr = Dev::new(1);
        let grantee = AccountId::from_bytes([0x44; 32]);
        let mut f = Fixture::genesis(&fdr);
        let (stream, own_op) = stream_own(f.account_id);
        let own = f.author(&fdr, Some(f.genesis_hash), &own_op);
        let grant = f.author(&fdr, Some(f.genesis_hash), &stream_grant(stream, grantee));
        let revoke = f.author(&fdr, Some(f.genesis_hash), &stream_revoke(stream, grantee, grant));

        let h = f.fold();
        assert!(h.is_effective(&own));
        assert!(h.is_effective(&grant));
        assert!(h.is_effective(&revoke));
        assert_eq!(h.effective_count(), 4);
        // The exact citation resolves against the fold we hold, and NOTHING else: revocation bounds
        // content through cuts, so no assertion the author makes about its own control length can
        // reopen, close, or deny this grant. That counter is a separate, purely informational axis.
        assert_eq!(
            h.grant_effective(grant, stream, grantee),
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
        assert_eq!(h.stream_owner_effective(stream), AuthorityQuery::Effective(own));
    }

    #[test]
    fn stream_preconditions_fail_closed_without_minting_authority_p11() {
        let fdr = Dev::new(1);
        let grantee = AccountId::from_bytes([0x44; 32]);
        let mut f = Fixture::genesis(&fdr);
        let (stream, own_op) = stream_own(f.account_id);
        let early_grant = f.author(&fdr, Some(f.genesis_hash), &stream_grant(stream, grantee));
        let own = f.author(&fdr, Some(f.genesis_hash), &own_op);
        let duplicate_own = f.author(&fdr, Some(f.genesis_hash), &own_op);
        let self_grant = f.author(&fdr, Some(f.genesis_hash), &stream_grant(stream, f.account_id));
        let grant = f.author(&fdr, Some(f.genesis_hash), &stream_grant(stream, grantee));
        let duplicate_grant = f.author(&fdr, Some(f.genesis_hash), &stream_grant(stream, grantee));
        let wrong_revoke = f.author(
            &fdr,
            Some(f.genesis_hash),
            &stream_revoke(stream, AccountId::from_bytes([0x55; 32]), grant),
        );

        let h = f.fold();
        assert!(h.is_effective(&own));
        assert!(h.is_effective(&grant));
        assert_eq!(
            h.grant_effective(early_grant, stream, grantee),
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
                h.outcome(&hash),
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
            f.author(&founder, Some(f.genesis_hash), &device_add(&owner, DeviceRole::Owner));
        let (stream, own_op) = stream_own(f.account_id);
        let own = f.author(&founder, Some(f.genesis_hash), &own_op);
        let remove_founder = f.author(
            &owner,
            Some(add_owner),
            &device_remove(&founder, Cut::At { seq: 1, hash: add_owner }),
        );
        let grant = f.author(&owner, Some(add_owner), &stream_grant(stream, grantee));

        let expected = f.fold();
        assert_eq!(expected.outcome(&own), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));
        assert!(expected.is_effective(&remove_founder));
        assert_eq!(expected.outcome(&grant), Some(Outcome::Rejected(RejectReason::Ineffective)));
        assert_eq!(
            expected.grant_effective(grant, stream, grantee),
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
            f.author(&founder, Some(f.genesis_hash), &device_add(&owner, DeviceRole::Owner));
        let add_member =
            f.author(&founder, Some(f.genesis_hash), &device_add(&member, DeviceRole::Member));
        let promote = f.author(&owner, Some(add_owner), &owner_promote(&member));
        let cut = f.author(
            &owner,
            Some(add_owner),
            &device_remove(&founder, Cut::At { seq: 1, hash: add_owner }),
        );

        let expected = f.fold();
        assert!(expected.is_effective(&cut));
        assert_eq!(
            expected.outcome(&add_member),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        );
        assert!(!expected.is_effective(&promote));
        assert_eq!(
            expected.owner_incarnation_effective(promote, member.fp,),
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
            f.author(&founder, Some(f.genesis_hash), &device_add(&owner, DeviceRole::Owner));
        let add_member =
            f.author(&founder, Some(f.genesis_hash), &device_add(&member, DeviceRole::Member));
        let remove_member =
            f.author(&founder, Some(f.genesis_hash), &device_remove(&member, Cut::Empty));
        f.author(
            &owner,
            Some(add_owner),
            &device_remove(&founder, Cut::At { seq: 2, hash: add_member }),
        );
        let promote = f.author(&owner, Some(add_owner), &owner_promote(&member));

        let expected = f.fold();
        assert_eq!(
            expected.outcome(&remove_member),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
        );
        assert!(
            expected.is_effective(&promote),
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
            f.author(&founder, Some(f.genesis_hash), &device_add(&owner, DeviceRole::Owner));
        let (stream, own_op) = stream_own(f.account_id);
        let own = f.author(&founder, Some(f.genesis_hash), &own_op);
        let grant = f.author(&founder, Some(f.genesis_hash), &stream_grant(stream, grantee));
        let remove_founder = f.author(
            &owner,
            Some(add_owner),
            &device_remove(&founder, Cut::At { seq: 2, hash: own }),
        );
        let revoke = f.author(&owner, Some(add_owner), &stream_revoke(stream, grantee, grant));

        let expected = f.fold();
        assert!(expected.is_effective(&own));
        assert_eq!(expected.outcome(&grant), Some(Outcome::Condemned(CondemnedReason::BeyondCut)));
        assert!(expected.is_effective(&remove_founder));
        assert_eq!(expected.outcome(&revoke), Some(Outcome::Rejected(RejectReason::Ineffective)));
        assert_eq!(
            expected.grant_effective(grant, stream, grantee),
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
                f.author(&founder, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
            let add_b =
                f.author(&founder, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
            let (e_author, e_ref, x_author, x_ref) =
                if e_is_a { (&a, add_a, &b, add_b) } else { (&b, add_b, &a, add_a) };
            saw_orderings.insert(e_author.fp < x_author.fp);
            let x = f.author_at_auth_len(
                x_author,
                Some(x_ref),
                &device_add(&Dev::new(4), DeviceRole::Member),
                3,
            );
            let e = f.author_at_auth_len(
                e_author,
                Some(e_ref),
                &device_add(&Dev::new(5), DeviceRole::Member),
                4,
            );

            let expected = f.fold();
            assert!(expected.is_effective(&x));
            assert!(expected.is_effective(&e));
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
        let (stream, valid) = stream_own(f.account_id);
        let AccountOp::StreamOwn { stream_spec_bytes, .. } = valid else { unreachable!() };
        let wrong_hash = f.author(&fdr, Some(f.genesis_hash), &AccountOp::StreamOwn {
            stream_id: StreamId::from_bytes([0x77; 32]),
            stream_spec_bytes: stream_spec_bytes.clone(),
        });
        let (_, wrong_owner_op) = stream_own(AccountId::from_bytes([0x66; 32]));
        let wrong_owner = f.author(&fdr, Some(f.genesis_hash), &wrong_owner_op);
        let malformed = f.author(&fdr, Some(f.genesis_hash), &AccountOp::StreamOwn {
            stream_id: stream,
            stream_spec_bytes: vec![0x80],
        });

        let h = f.fold();
        for hash in [wrong_hash, wrong_owner, malformed] {
            assert_eq!(h.outcome(&hash), Some(Outcome::Rejected(RejectReason::InvalidStreamSpec)),);
        }
    }

    #[test]
    fn roster_and_owner_queries_are_citation_time_and_subject_bound() {
        let fdr = Dev::new(1);
        let member = Dev::new(2);
        let mut f = Fixture::genesis(&fdr);
        let add = f.author(&fdr, Some(f.genesis_hash), &device_add(&member, DeviceRole::Member));
        let promote = f.author(&fdr, Some(f.genesis_hash), &owner_promote(&member));
        let h = f.fold();

        assert_eq!(
            h.roster_ref_effective(add, member.fp),
            AuthorityQuery::Effective(RosterAuthority {
                device_fingerprint: member.fp,
                current_role: DeviceRole::Owner,
            }),
        );
        assert_eq!(
            h.roster_ref_effective(add, fdr.fp),
            AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject),
        );
        assert_eq!(
            h.owner_incarnation_effective(promote, member.fp),
            AuthorityQuery::Effective(OwnerAuthority { device_fingerprint: member.fp }),
            "a behind auth_len does not deny an exact owner citation",
        );
        assert_eq!(
            h.owner_incarnation_effective(promote, member.fp),
            AuthorityQuery::Effective(OwnerAuthority { device_fingerprint: member.fp }),
        );

        f.author(&fdr, Some(f.genesis_hash), &owner_demote(&member, promote, Cut::Empty));
        let h = f.fold();
        assert_eq!(
            h.roster_ref_effective(add, member.fp),
            AuthorityQuery::Effective(RosterAuthority {
                device_fingerprint: member.fp,
                current_role: DeviceRole::Member,
            }),
        );
        assert_eq!(
            h.owner_incarnation_effective(promote, member.fp),
            AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
        );

        f.author(&fdr, Some(f.genesis_hash), &device_remove(&member, Cut::Empty));
        let h = f.fold();
        assert_eq!(
            h.roster_ref_effective(add, member.fp),
            AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
        );
    }

    #[test]
    fn a_read_only_enrollment_cannot_be_promoted_and_re_bless_prior_content() {
        let founder = Dev::new(1);
        let read_only = Dev::new(2);
        let stream = StreamId::from_bytes([0x44; 32]);
        let mut f = Fixture::genesis(&founder);
        let add =
            f.author(&founder, Some(f.genesis_hash), &device_add(&read_only, DeviceRole::ReadOnly));
        let promote = f.author(&founder, Some(f.genesis_hash), &owner_promote(&read_only));

        let h = f.fold();
        assert_eq!(
            h.outcome(&promote),
            Some(Outcome::Rejected(RejectReason::BadPromote)),
            "promotion cannot turn a read-only enrollment into retroactive write authority",
        );
        assert_eq!(
            h.roster_content_authority(add, read_only.fp, stream),
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
        let payload = account_ops::encode(&op).unwrap();
        let account_id = account_id_from_genesis_payload(&payload);
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
        let fact = h.roster_refs.get(&add).expect("later enrollment has a roster fact");
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
            g = f.author(&devs[(k - 1) as usize], Some(g), &device_add(&dev, DeviceRole::Owner));
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
        let payload = account_ops::encode(&op).unwrap();
        let account_id = account_id_from_genesis_payload(&payload);
        let header = AccountEntryHeader {
            account_id,
            log_id: 0,
            device_fingerprint: founder.fp,
            seq: 0,
            prev_hash: None,
            parent_ref: Some([0x01; 32]), // a root has no parent
            entry_type: account_ops::entry_type::ACCOUNT_GENESIS,
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
            entry_type: account_ops::entry_type::DEVICE_ADD,
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
            entry_type: account_ops::entry_type::DEVICE_ADD,
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
            new_entry_hash: [0x44; 32],
        };
        let extend = f.author(&fdr, Some(f.genesis_hash), &op);

        let h = f.fold();
        assert_eq!(
            h.outcome(&extend),
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
            Some(f.genesis_hash),
            &cut_extend_secrets(f.account_id, &fdr, None, 3, [0x44; 32]),
        );

        let h = f.fold();
        assert_eq!(
            h.outcome(&extend),
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        // A second owner so removing B is not the last-owner reject.
        f.author(&fdr, Some(f.genesis_hash), &device_add(&c, DeviceRole::Owner));
        // A held watermark on B's secrets chain (log 1) so §11.3 binding is Ok, not TargetNotHeld.
        let s0 = f.author_secrets_entry(&b, &b, 0, None);
        let remove_b = f.author(
            &fdr,
            Some(f.genesis_hash),
            &device_remove_with_secrets(&b, Cut::Empty, Cut::At { seq: 0, hash: s0 }),
        );

        let h = f.fold();
        assert!(h.is_effective(&remove_b), "the remove is admitted — both cuts bind");
        let fact = h.roster_refs.get(&add_b).expect("B has a roster fact");
        assert_eq!(
            fact.secrets_boundary,
            AuthorityBoundary::Cut { seq: 0, hash: s0 },
            "secrets_boundary is the joined log-1 register",
        );
        assert_eq!(
            fact.control_boundary,
            AuthorityBoundary::Closed,
            "the control chain is bounded independently by its own (empty) cut",
        );
        match h.owner_secrets_authority(add_b, b.fp) {
            AuthorityQuery::Effective(auth) => assert_eq!(
                auth.device_boundary,
                AuthorityBoundary::Cut { seq: 0, hash: s0 },
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        f.author(&fdr, Some(f.genesis_hash), &device_add(&c, DeviceRole::Owner));
        let s0 = f.author_secrets_entry(&b, &b, 0, None);
        let demote_b = f.author(
            &fdr,
            Some(f.genesis_hash),
            &owner_demote_with_secrets(&b, add_b, Cut::Empty, Cut::At { seq: 0, hash: s0 }),
        );

        let h = f.fold();
        assert!(h.is_effective(&demote_b), "the demote is admitted — both cuts bind");
        match h.owner_secrets_authority(add_b, b.fp) {
            AuthorityQuery::Effective(auth) => assert_eq!(
                auth.incarnation_boundary,
                AuthorityBoundary::Cut { seq: 0, hash: s0 },
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        f.author(&fdr, Some(f.genesis_hash), &device_add(&c, DeviceRole::Owner));
        // B's secrets chain s0 <- s1 <- s2 (log 1), so the extend's watermark descends the
        // remove's.
        let s0 = f.author_secrets_entry(&b, &b, 0, None);
        let s1 = f.author_secrets_entry(&b, &b, 1, Some(s0));
        let s2 = f.author_secrets_entry(&b, &b, 2, Some(s1));
        let remove_b = f.author(
            &fdr,
            Some(f.genesis_hash),
            &device_remove_with_secrets(&b, Cut::Empty, Cut::At { seq: 0, hash: s0 }),
        );
        let extend = f.author(
            &fdr,
            Some(f.genesis_hash),
            &cut_extend_secrets(f.account_id, &b, None, 2, s2),
        );

        // Without the extend, the boundary is the original seq-0 watermark.
        let before = f.fold_without(extend);
        assert_eq!(
            before.roster_refs.get(&add_b).unwrap().secrets_boundary,
            AuthorityBoundary::Cut { seq: 0, hash: s0 },
        );

        // With the extend, the register joins to seq 2 and the extend is effective.
        let after = f.fold();
        assert!(after.is_effective(&extend), "the secrets extend re-blesses and is effective");
        assert!(after.is_effective(&remove_b));
        assert_eq!(
            after.roster_refs.get(&add_b).unwrap().secrets_boundary,
            AuthorityBoundary::Cut { seq: 2, hash: s2 },
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let add_c = f.author(&fdr, Some(f.genesis_hash), &device_add(&c, DeviceRole::Owner));
        let remove_b = f.author(
            &fdr,
            Some(f.genesis_hash),
            // add_c is a held CONTROL-log entry — the wrong coordinate for a log-1 watermark on B.
            &device_remove_with_secrets(&b, Cut::Empty, Cut::At { seq: 1, hash: add_c }),
        );

        let h = f.fold();
        assert_eq!(
            h.outcome(&remove_b),
            Some(Outcome::Rejected(RejectReason::CutTargetMismatch)),
            "a misbound secrets_cut rejects the whole remove",
        );
        assert!(h.is_effective(&add_b), "B stays enrolled — the misbound remove never took effect");
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let add_d = f.author(&fdr, Some(f.genesis_hash), &device_add(&d, DeviceRole::Owner));
        // Control cuts Empty (comparable); secrets cuts at the same seq with distinct hashes.
        f.author(
            &a,
            Some(add_a),
            &device_remove_with_secrets(&d, Cut::Empty, Cut::At { seq: 0, hash: [0xaa; 32] }),
        );
        f.author(
            &b,
            Some(add_b),
            &device_remove_with_secrets(&d, Cut::Empty, Cut::At { seq: 0, hash: [0xbb; 32] }),
        );

        let h = f.fold();
        assert_eq!(
            h.classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "incomparable secrets cuts for one key fold contested",
        );
        assert!(h.is_effective(&add_a) && h.is_effective(&add_b) && h.is_effective(&add_d));
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
        let add_g = f.author(&fdr, Some(f.genesis_hash), &device_add(&g, DeviceRole::Owner));
        let add_d = f.author(&fdr, Some(f.genesis_hash), &device_add(&d, DeviceRole::Owner));
        f.author(&fdr, Some(f.genesis_hash), &device_add(&e, DeviceRole::Owner)); // spare owner
        // Founder removes d, installing d's control register (Empty) and secrets register (At{0}).
        let remove_d = f.author(
            &fdr,
            Some(f.genesis_hash),
            &device_remove_with_secrets(&d, Cut::Empty, Cut::At { seq: 0, hash: [0x50; 32] }),
        );
        // g removes d again: its control cut WOULD raise d's control register (Empty ⊔ At{0} =
        // At{0}), but its secrets cut is undecidable — the higher watermark [0x52] is not held, so
        // the branch relation against the founder's At{0} secrets register can't be decided → the
        // secrets join parks.
        let remove_d_by_g = f.author(
            &g,
            Some(add_g),
            &device_remove_with_secrets(&d, Cut::At { seq: 0, hash: [0xc0; 32] }, Cut::At {
                seq: 2,
                hash: [0x52; 32],
            }),
        );

        let h = f.fold();
        // g's op parks (one chain undecidable) — NOT effective.
        assert_eq!(
            h.outcome(&remove_d_by_g),
            Some(Outcome::Parked(ParkReason::UnknownCutTarget)),
            "the op parks because one chain's cut is undecidable",
        );
        assert!(h.is_effective(&remove_d), "the founder's remove is unaffected");
        let fact = h.roster_refs.get(&add_d).expect("d has a roster fact");
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
            AuthorityBoundary::Cut { seq: 0, hash: [0x50; 32] },
            "a parked op must not advance the secrets register",
        );
        // Arrival-order-free: strata are content-derived, so the parked op raises no register under
        // any rotation (I9).
        for rot in 0..f.entries.len() {
            let rotated = f.fold_rotated(rot);
            assert_eq!(
                rotated.roster_refs.get(&add_d).unwrap().control_boundary,
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
        let add_p = f.author(&fdr, Some(f.genesis_hash), &device_add(&p, DeviceRole::Owner));
        f.author(&fdr, Some(f.genesis_hash), &device_add(&s, DeviceRole::Owner)); // spare owner
        // P (a depth-1 owner) mints A and D at depth 2 — so A's/D's own ops sit at stratum 2.
        let add_a = f.author(&p, Some(add_p), &device_add(&a, DeviceRole::Owner));
        let add_d = f.author(&p, Some(add_p), &device_add(&d, DeviceRole::Owner));
        // D removes A (a plain, valid mutual-removal partner: both cuts empty).
        let d_removes_a =
            f.author(&d, Some(add_d), &device_remove_with_secrets(&a, Cut::Empty, Cut::Empty));
        // F's early remove of D: at stratum 0 (before D is enrolled at stratum 1) it is
        // state-ineffective, but installs D's `Device{log:1}` secrets register (At{0}) and a
        // `Device{log:0}` control register whose cut names D's OWN op, so it condemns nothing D
        // does.
        f.author(
            &fdr,
            Some(f.genesis_hash),
            &device_remove_with_secrets(&d, Cut::At { seq: 0, hash: d_removes_a }, Cut::At {
                seq: 0,
                hash: [0x50; 32],
            }),
        );
        // A removes D: its control cut WOULD condemn D's op (the A→D cycle edge), but its secrets
        // cut is undecidable against the pre-existing At{0} secrets register (higher watermark
        // [0x52] not held) → A parks, and must NOT drive cycle-detection.
        let a_removes_d = f.author(
            &a,
            Some(add_a),
            &device_remove_with_secrets(&d, Cut::Empty, Cut::At { seq: 2, hash: [0x52; 32] }),
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
        assert!(h.is_effective(&d_removes_a), "D's valid removal of A takes effect");
        assert!(!h.is_effective(&a_removes_d), "A's own removal never goes effective (it parked)");
        assert!(
            matches!(h.owner_incarnation_effective(add_a, a.fp), AuthorityQuery::Invalid(_)),
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
        let add_p = f.author(&fdr, Some(f.genesis_hash), &device_add(&p, DeviceRole::Owner));
        let add_d = f.author(&fdr, Some(f.genesis_hash), &device_add(&d, DeviceRole::Owner));
        // D's secrets chain: s0, then an EQUIVOCATION at seq 1 (two distinct siblings off s0).
        let s0 = f.author_secrets_entry(&d, &d, 0, None);
        let e1a = f.author_secrets_entry(&d, &t8, 1, Some(s0));
        let e1b = f.author_secrets_entry(&d, &t9, 1, Some(s0));
        assert_ne!(e1a, e1b, "the two seq-1 secrets watermarks must be distinct");
        // Founder removes D (stratum 0, effective): installs D's secrets register at At{0, s0}.
        f.author(
            &fdr,
            Some(f.genesis_hash),
            &device_remove_with_secrets(&d, Cut::Empty, Cut::At { seq: 0, hash: s0 }),
        );
        // Two stratum-1 extends of D's secrets register to the two incomparable seq-1 watermarks.
        f.author(&p, Some(add_p), &cut_extend_secrets(f.account_id, &d, None, 1, e1a));
        f.author(&p, Some(add_p), &cut_extend_secrets(f.account_id, &d, None, 1, e1b));

        let h = f.fold();
        assert_eq!(
            h.classification(),
            AccountClassification::Contested { state_before_depth: 1 },
            "incomparable same-depth secrets extends fold contested at their stratum",
        );
        // DISCRIMINATOR: the contested stratum leaked NO watermark — D's secrets_boundary is still
        // the founder's original cut (At{0, s0}), NOT the first extend's raised At{1, e1a}.
        let fact =
            h.roster_refs.get(&add_d).expect("D has a roster fact from the stratum-0 remove");
        assert_eq!(
            fact.secrets_boundary,
            AuthorityBoundary::Cut { seq: 0, hash: s0 },
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
                rotated.roster_refs.get(&add_d).unwrap().secrets_boundary,
                AuthorityBoundary::Cut { seq: 0, hash: s0 },
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        let remove_b = f.author(&fdr, Some(f.genesis_hash), &device_remove(&b, Cut::Empty));

        let h = f.fold();
        assert_eq!(h.classification(), AccountClassification::Live);
        assert!(h.is_effective(&remove_b), "removing one of several owners is effective");
        assert!(h.is_effective(&add_a) && h.is_effective(&add_b), "both devices were enrolled");
        assert!(
            matches!(h.owner_incarnation_effective(add_b, b.fp), AuthorityQuery::Invalid(_)),
            "B's owner incarnation is closed by the removal",
        );
        assert!(
            matches!(h.owner_incarnation_effective(add_a, a.fp), AuthorityQuery::Effective(_)),
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        // A removes the founder (A's seq 0), then equivocates its OWN removal at A's seq 1.
        let remove_f =
            f.author(&a, Some(add_a), &device_remove(&fdr, Cut::At { seq: 1, hash: add_a }));
        let self_x = f.author_forked(
            &a,
            Some(add_a),
            &device_remove(&a, Cut::At { seq: 5, hash: [0xc1; 32] }),
            1,
            Some(remove_f),
        );
        let self_y = f.author_forked(
            &a,
            Some(add_a),
            &device_remove(&a, Cut::At { seq: 5, hash: [0xc2; 32] }),
            1,
            Some(remove_f),
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
            (Outcome::Parked(ParkReason::AuthLenAhead), ("parked", Some("auth_len_ahead"))),
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
            (
                Outcome::Rejected(RejectReason::InvalidStreamSpec),
                ("rejected", Some("invalid_stream_spec")),
            ),
            (Outcome::Rejected(RejectReason::Ineffective), ("rejected", Some("ineffective"))),
        ];
        for (outcome, expected) in cases {
            assert_eq!(outcome.taxonomy(), expected, "taxonomy drift for {outcome:?}");
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
        let add_x = f.author(&fdr, Some(f.genesis_hash), &device_add(&x, DeviceRole::Owner));
        // X (depth-1 owner) mints Y as a deeper owner, removes the founder, and demotes Y with a
        // cut that COVERS Y's later removal op — so Y's incarnation stays live and its removal is
        // admitted (not condemned) at depth 2.
        let add_y = f.author(&x, Some(add_x), &device_add(&y, DeviceRole::Owner)); // X seq 0
        let remove_x = f.author(&y, Some(add_y), &device_remove(&x, Cut::Empty)); // Y seq 0 (depth 2)
        f.author(&x, Some(add_x), &owner_demote(&y, add_y, Cut::At { seq: 0, hash: remove_x })); // X seq 1
        f.author(&x, Some(add_x), &device_remove(&fdr, Cut::At { seq: 1, hash: add_x })); // X seq 2

        let h = f.fold();
        assert_eq!(
            h.outcome(&remove_x),
            Some(Outcome::Rejected(RejectReason::LastOwner)),
            "removing the sole remaining owner is reserved LastOwner by the intrinsic prefilter",
        );
        assert_eq!(h.classification(), AccountClassification::Live, "the account stays live");
        assert!(!h.is_effective(&remove_x), "the sole-owner removal does not fold");
        assert!(
            matches!(h.owner_incarnation_effective(add_x, x.fp), AuthorityQuery::Effective(_)),
            "X's owner incarnation stays open — the owner set is never emptied",
        );
        // Arrival order cannot change the verdict (I9): the intrinsic prefilter is order-free.
        for rot in 0..f.entries.len() {
            let r = f.fold_rotated(rot);
            assert_eq!(r.classification(), AccountClassification::Live, "rotation {rot} class");
            assert_eq!(
                r.outcome(&remove_x),
                Some(Outcome::Rejected(RejectReason::LastOwner)),
                "rotation {rot}: still reserved LastOwner",
            );
            assert!(
                matches!(r.owner_incarnation_effective(add_x, x.fp), AuthorityQuery::Effective(_)),
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
        let add_a = f.author(&fdr, Some(f.genesis_hash), &device_add(&a, DeviceRole::Owner));
        // A (depth-1 owner) mints B and C as deeper owners, then removes F and demotes C — leaving
        // the prior-depth owner set at depth 2 exactly {A, B}. C's demotion cut COVERS both of C's
        // later removals, so C's incarnation stays live and both cuts are admitted at depth 2.
        let add_b = f.author(&a, Some(add_a), &device_add(&b, DeviceRole::Owner)); // A seq 0
        let add_c = f.author(&a, Some(add_a), &device_add(&c, DeviceRole::Owner)); // A seq 1
        let remove_a = f.author(&c, Some(add_c), &device_remove(&a, Cut::Empty)); // C seq 0 (depth 2)
        let remove_b = f.author(&c, Some(add_c), &device_remove(&b, Cut::Empty)); // C seq 1 (depth 2)
        f.author(&a, Some(add_a), &owner_demote(&c, add_c, Cut::At { seq: 1, hash: remove_b })); // A seq 2
        f.author(&a, Some(add_a), &device_remove(&fdr, Cut::At { seq: 1, hash: add_a })); // A seq 3

        let h = f.fold();
        assert_eq!(h.classification(), AccountClassification::Live, "the standoff stays live");
        // Exactly one of the two same-depth removals folds; the other is reserved LastOwner.
        let a_removed = h.is_effective(&remove_a);
        let b_removed = h.is_effective(&remove_b);
        assert!(a_removed ^ b_removed, "exactly one owner removal folds — the other is reserved");
        let (effective, reserved) =
            if a_removed { (remove_a, remove_b) } else { (remove_b, remove_a) };
        assert!(h.is_effective(&effective), "the first-in-order removal is effective");
        assert_eq!(
            h.outcome(&reserved),
            Some(Outcome::Rejected(RejectReason::LastOwner)),
            "the removal that would empty the owner set is reserved LastOwner",
        );
        // One owner incarnation stays open (the reserved survivor); the other is closed.
        let a_open =
            matches!(h.owner_incarnation_effective(add_a, a.fp), AuthorityQuery::Effective(_));
        let b_open =
            matches!(h.owner_incarnation_effective(add_b, b.fp), AuthorityQuery::Effective(_));
        assert!(a_open ^ b_open, "exactly one owner incarnation stays open");
        // Arrival order (I9): the SAME reserved survivor and verdict under every rotation.
        for rot in 0..f.entries.len() {
            let r = f.fold_rotated(rot);
            assert_eq!(r.classification(), AccountClassification::Live, "rotation {rot} class");
            assert_eq!(
                r.outcome(&reserved),
                Some(Outcome::Rejected(RejectReason::LastOwner)),
                "rotation {rot}: the same removal is reserved LastOwner",
            );
            assert!(r.is_effective(&effective), "rotation {rot}: the same removal folds");
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
        let add_b = f.author(&fdr, Some(f.genesis_hash), &device_add(&b, DeviceRole::Owner));
        // B's control chain (per-device seq numbering starts at 0 on B's own chain — add_b lives on
        // F's chain, not B's): StreamOwn (seq 0), StreamGrant (seq 1), a member add BEYOND the cut
        // (seq 2).
        let (stream, own_op) = stream_own(f.account_id);
        let own = f.author(&b, Some(add_b), &own_op); // B seq 0
        let grant = f.author(&b, Some(add_b), &stream_grant(stream, grantee)); // B seq 1
        let beyond = f.author(&b, Some(add_b), &device_add(&x, DeviceRole::Member)); // B seq 2
        // F removes B with the valid prefix pinned AT the grant (B seq 1) — own + grant are within.
        let remove_b = f.author(
            &fdr,
            Some(f.genesis_hash),
            &device_remove(&b, Cut::At { seq: 1, hash: grant }),
        );

        let h = f.fold();
        assert!(h.is_effective(&remove_b), "the removal of B is effective");
        assert!(h.is_effective(&own), "B's within-cut StreamOwn survives the removal (no cascade)");
        assert!(
            h.is_effective(&grant),
            "B's within-cut StreamGrant survives the removal (no cascade)",
        );
        assert_eq!(
            h.grant_effective(grant, stream, grantee),
            AuthorityQuery::Effective(GrantAuthority {
                stream_id: stream,
                grantee_account_id: grantee,
                role: GrantRole::Reader,
            }),
            "the grant stays queryable authority after its author is removed",
        );
        assert_eq!(
            h.outcome(&beyond),
            Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
            "B's op beyond the cut is condemned — the prefix is bounded, the within-cut grant is \
             not",
        );
        // Arrival order (I9): same no-cascade result under every rotation.
        for rot in 0..f.entries.len() {
            let r = f.fold_rotated(rot);
            assert!(r.is_effective(&grant), "rotation {rot}: grant survives");
            assert_eq!(
                r.grant_effective(grant, stream, grantee),
                AuthorityQuery::Effective(GrantAuthority {
                    stream_id: stream,
                    grantee_account_id: grantee,
                    role: GrantRole::Reader,
                }),
                "rotation {rot}: grant stays effective",
            );
            assert_eq!(
                r.outcome(&beyond),
                Some(Outcome::Condemned(CondemnedReason::BeyondCut)),
                "rotation {rot}: beyond-cut op condemned",
            );
        }
    }
}
