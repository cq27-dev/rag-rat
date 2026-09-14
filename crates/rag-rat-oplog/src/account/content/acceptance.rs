//! Pure C3 `/3` acceptance predicate (§13).
//!
//! Persistence resolves every authority citation against the CURRENT fold — the only authority
//! snapshot there is (§7: `auth_len` never selects a historical view) — and must read them all in
//! ONE snapshot, so a refold committing mid-evaluation cannot combine an old grant with a new cut.
//! Late control or ancestry arrival re-evaluates the same candidates; history is never mutated.
//!
//! Freshness is a separate axis from authority, applied LAST, so an author who cites control ops we
//! have not folded cannot mask a condemnation or a fork the content DAG already decides. Its one
//! reach backward is the rejection gate: a citation our fold reads as ineffective is fold-dependent
//! (a `CutExtend` we have not folded re-blesses it, §11.4), so while we are behind the author that
//! parks as `auth_len_ahead` instead of hardening into a rejection.

use super::super::branch::{AncestryRelation, CitedFreshness, UnknownAncestry};
use super::super::id::{AccountEntryHash, GrantId, RosterRef};
use super::ContentEntryHeader;
use crate::account::{
    AccountId, AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason, AuthorityQuery,
    GrantDeviceAuthority, GrantDeviceBoundary, GrantRole, RosterContentAuthority,
};
use crate::stream::StreamId;

/// A whole-coordinate authority hold that fails content closed regardless of the revocation
/// registers. A withheld cut watermark is NOT one of these: it is bound at the register (the cut
/// stays intact), so beyond-cut still condemns from seq alone and only the genuinely under-cut
/// prefix parks via `combine_boundaries`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubjectAuthorityHold {
    Clear,
    Contested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CitedOwnership {
    pub owner_account_id: AccountId,
    pub stream_id: StreamId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CitedRosterAuthority {
    pub account_id: AccountId,
    pub roster_ref: RosterRef,
    pub stream_id: StreamId,
    pub authority: RosterContentAuthority,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CitedGrantAuthority {
    pub owner_account_id: AccountId,
    pub grant_id: GrantId,
    pub authority: GrantDeviceAuthority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentParkReason {
    MissingPredecessor,
    UnknownOwner,
    UnknownRosterRef,
    UnknownGrant,
    OwnerAuthLenAhead,
    AuthorAuthLenAhead,
    IncompleteCutAncestry,
    UnknownCutTarget,
    ContestedSubject,
    /// The entry violates the lamport discipline the fold enforces (the fold-seam half of the
    /// `/3` lamport clamp): its lamport jumps implausibly far past the accepted stream clock
    /// (bounded advance), or its chain's lamport fails to strictly increase (honest authoring
    /// mints `max accepted + 1` per entry). Assigned by the refold, not this evaluator — both
    /// rules need the whole accepted set, which a per-entry pass cannot see. Recoverable by
    /// construction: verdicts are re-derived every refold, so a bounded-advance park accepts if
    /// the stream's accepted clock ever legitimately catches up.
    LamportAhead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentCondemnReason {
    BeyondCut,
    OffBranch,
    ClosedIncarnation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentRejectReason {
    OwnerReferenceInvalid,
    RosterReferenceInvalid,
    GrantReferenceInvalid,
    GrantRequired,
    UnexpectedGrant,
    GrantDoesNotPermitWrite,
    /// The signing device is on the roster but its role forbids authoring content (a `ReadOnly`
    /// device). The fold-level backstop for the read-only capability.
    RoleForbidsAuthoring,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentAcceptanceInputError {
    FreshnessProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentAcceptance {
    Accepted,
    Forked,
    Parked(ContentParkReason),
    Condemned(ContentCondemnReason),
    Rejected(ContentRejectReason),
}

impl ContentAcceptance {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Forked => "forked",
            Self::Parked(ContentParkReason::MissingPredecessor) => "parked{missing_predecessor}",
            Self::Parked(ContentParkReason::UnknownOwner) => "parked{unknown_account}",
            Self::Parked(ContentParkReason::UnknownRosterRef) => "parked{unknown_roster_ref}",
            Self::Parked(ContentParkReason::UnknownGrant) => "parked{unknown_grant}",
            Self::Parked(ContentParkReason::OwnerAuthLenAhead)
            | Self::Parked(ContentParkReason::AuthorAuthLenAhead) => "parked{auth_len_ahead}",
            Self::Parked(ContentParkReason::IncompleteCutAncestry) =>
                "parked{incomplete_cut_ancestry}",
            Self::Parked(ContentParkReason::UnknownCutTarget) => "parked{unknown_cut_target}",
            Self::Parked(ContentParkReason::ContestedSubject) => "parked{contested_subject}",
            Self::Parked(ContentParkReason::LamportAhead) => "parked{lamport_ahead}",
            Self::Condemned(ContentCondemnReason::BeyondCut) => "condemned{beyond_cut}",
            Self::Condemned(ContentCondemnReason::OffBranch) => "condemned{off_branch}",
            Self::Condemned(ContentCondemnReason::ClosedIncarnation) =>
                "condemned{closed_incarnation}",
            Self::Rejected(ContentRejectReason::OwnerReferenceInvalid) => "rejected{invalid_owner}",
            Self::Rejected(ContentRejectReason::RosterReferenceInvalid) =>
                "rejected{invalid_roster_ref}",
            Self::Rejected(ContentRejectReason::GrantReferenceInvalid) => "rejected{invalid_grant}",
            Self::Rejected(ContentRejectReason::GrantRequired) => "rejected{grant_required}",
            Self::Rejected(ContentRejectReason::UnexpectedGrant) => "rejected{unexpected_grant}",
            Self::Rejected(ContentRejectReason::GrantDoesNotPermitWrite) =>
                "rejected{grant_not_writer}",
            Self::Rejected(ContentRejectReason::RoleForbidsAuthoring) =>
                "rejected{role_forbids_authoring}",
        }
    }
}

#[derive(Clone)]
pub struct ContentAcceptanceInput<'a, F>
where
    F: Fn(AccountEntryHash, AccountEntryHash) -> AncestryRelation,
{
    pub header: &'a ContentEntryHeader,
    pub entry_hash: AccountEntryHash,
    pub owner_account_id: AccountId,
    pub dense_predecessor_reachable: bool,
    pub branch_selected: bool,
    pub ownership: AuthorityQuery<CitedOwnership>,
    pub roster: AuthorityQuery<CitedRosterAuthority>,
    pub grant: Option<AuthorityQuery<CitedGrantAuthority>>,
    pub owner_freshness: CitedFreshness,
    pub author_freshness: CitedFreshness,
    pub subject_hold: SubjectAuthorityHold,
    pub ancestry: F,
}

pub fn evaluate_content_acceptance<F>(
    input: &ContentAcceptanceInput<'_, F>,
) -> Result<ContentAcceptance, ContentAcceptanceInputError>
where
    F: Fn(AccountEntryHash, AccountEntryHash) -> AncestryRelation,
{
    if let Some(verdict) = authority_verdict(input)? {
        return Ok(verdict);
    }
    if !input.dense_predecessor_reachable {
        return Ok(ContentAcceptance::Parked(ContentParkReason::MissingPredecessor));
    }
    if !input.branch_selected {
        return Ok(ContentAcceptance::Forked);
    }
    // Freshness last (§13): an author citing control ops we have not folded parks for refetch, but
    // only once every verdict the current fold CAN decide — condemnation, fork — has been ruled
    // out.
    if input.owner_freshness.state == AuthorityFreshness::Ahead {
        return Ok(ContentAcceptance::Parked(ContentParkReason::OwnerAuthLenAhead));
    }
    if input.author_freshness.state == AuthorityFreshness::Ahead {
        return Ok(ContentAcceptance::Parked(ContentParkReason::AuthorAuthLenAhead));
    }
    Ok(ContentAcceptance::Accepted)
}

/// Everything §13 decides BEFORE the content DAG has a say: the structural grant coupling, the
/// authority citations, every applicable revocation register, and the subject holds. `None` means
/// the entry is authorized — it is then eligible to contest a slot in branch selection, and the
/// caller finishes the verdict with [`evaluate_content_acceptance`].
///
/// This is the phase split the refold needs: an entry that is condemned or rejected must NOT
/// compete for its dense seq slot (a small-hash entry mined beyond a cut would otherwise fork an
/// honest sibling off the accepted branch), so eligibility has to be decided before selection runs
/// — and selection's output is itself an input to the full predicate.
pub fn authority_verdict<F>(
    input: &ContentAcceptanceInput<'_, F>,
) -> Result<Option<ContentAcceptance>, ContentAcceptanceInputError>
where
    F: Fn(AccountEntryHash, AccountEntryHash) -> AncestryRelation,
{
    // Provenance first: a freshness verdict computed for another account, or for a shorter
    // assertion than the header makes, decides nothing about THIS entry.
    if input.owner_freshness.account_id != input.owner_account_id
        || input.owner_freshness.asserted_auth_len != input.header.owner_auth_len
        || input.author_freshness.account_id != input.header.author_account_id
        || input.author_freshness.asserted_auth_len != input.header.author_auth_len
    {
        return Err(ContentAcceptanceInputError::FreshnessProvenance);
    }
    let owner_freshness = input.owner_freshness.state;
    let author_freshness = input.author_freshness.state;

    let is_owner = input.header.author_account_id == input.owner_account_id;
    if is_owner && (input.header.grant_id.is_some() || input.grant.is_some()) {
        return Ok(Some(ContentAcceptance::Rejected(ContentRejectReason::UnexpectedGrant)));
    }
    if !is_owner && input.header.grant_id.is_none() {
        return Ok(Some(ContentAcceptance::Rejected(ContentRejectReason::GrantRequired)));
    }
    let rejected = |reason| Ok(Some(ContentAcceptance::Rejected(reason)));
    let parked = |reason| Ok(Some(ContentAcceptance::Parked(reason)));

    match input.ownership {
        AuthorityQuery::Effective(fact)
            if fact.owner_account_id == input.owner_account_id
                && fact.stream_id == input.header.stream_id => {},
        AuthorityQuery::Unknown => return parked(ContentParkReason::UnknownOwner),
        // Ownership is minted in the OWNER's log, so the owner's freshness gates its rejection.
        AuthorityQuery::Invalid(reason) =>
            return Ok(Some(invalid_citation(
                reason,
                owner_freshness,
                ContentRejectReason::OwnerReferenceInvalid,
                ContentParkReason::OwnerAuthLenAhead,
            ))),
        AuthorityQuery::Effective(_) =>
            return rejected(ContentRejectReason::OwnerReferenceInvalid),
    }
    let roster = match input.roster {
        AuthorityQuery::Effective(fact)
            if fact.account_id == input.header.author_account_id
                && fact.roster_ref == input.header.roster_ref
                && fact.stream_id == input.header.stream_id
                && fact.authority.device_fingerprint == input.header.device_fingerprint =>
            fact,
        AuthorityQuery::Unknown => return parked(ContentParkReason::UnknownRosterRef),
        // The roster enrollment lives in the AUTHOR's log — the author's freshness gates it.
        AuthorityQuery::Invalid(reason) =>
            return Ok(Some(invalid_citation(
                reason,
                author_freshness,
                ContentRejectReason::RosterReferenceInvalid,
                ContentParkReason::AuthorAuthLenAhead,
            ))),
        AuthorityQuery::Effective(_) =>
            return rejected(ContentRejectReason::RosterReferenceInvalid),
    };
    // A device whose roster role forbids content authoring (a `ReadOnly` device) is rejected here,
    // before the owner/grant coupling — read-only grants read access, never write, and this holds
    // on BOTH paths (the owner account's own stream and a grantee stream). The fold is the ONLY
    // enforcement point today: an entry from any source (local author or a synced push) that
    // reaches the fold fails closed here. A wire-level gate that refuses a read-only peer's
    // pushes BEFORE they are stored as candidates is a later slice; until it lands such content
    // is still ingested and quota-charged, then rejected here — never accepted.
    if !roster.authority.role.can_author_content() {
        return rejected(ContentRejectReason::RoleForbidsAuthoring);
    }
    let mut boundaries = vec![roster.authority.boundary];

    if !is_owner {
        let grant = match input.grant.as_ref() {
            Some(AuthorityQuery::Effective(fact))
                if fact.owner_account_id == input.owner_account_id
                    && Some(fact.grant_id) == input.header.grant_id
                    && fact.authority.grant.stream_id == input.header.stream_id
                    && fact.authority.grant.grantee_account_id
                        == input.header.author_account_id =>
                fact,
            Some(AuthorityQuery::Unknown) | None => return parked(ContentParkReason::UnknownGrant),
            // The grant is minted in the OWNER's log — the owner's freshness gates it.
            Some(AuthorityQuery::Invalid(reason)) =>
                return Ok(Some(invalid_citation(
                    *reason,
                    owner_freshness,
                    ContentRejectReason::GrantReferenceInvalid,
                    ContentParkReason::OwnerAuthLenAhead,
                ))),
            Some(AuthorityQuery::Effective(_)) =>
                return rejected(ContentRejectReason::GrantReferenceInvalid),
        };
        if grant.authority.grant.role != GrantRole::Writer {
            return rejected(ContentRejectReason::GrantDoesNotPermitWrite);
        }
        boundaries.push(match &grant.authority.boundary {
            GrantDeviceBoundary::Open => AuthorityBoundary::Open,
            GrantDeviceBoundary::Cut(cut)
                if cut.device_fingerprint == input.header.device_fingerprint =>
                AuthorityBoundary::Cut { seq: cut.seq, hash: cut.hash },
            GrantDeviceBoundary::Cut(_) =>
                return rejected(ContentRejectReason::GrantReferenceInvalid),
            GrantDeviceBoundary::Closed => AuthorityBoundary::Closed,
        });
    }

    let boundary_decision =
        combine_boundaries(&boundaries, input.header.seq, input.entry_hash, &input.ancestry);
    if matches!(boundary_decision, Some(ContentAcceptance::Condemned(_))) {
        return Ok(boundary_decision);
    }
    match input.subject_hold {
        SubjectAuthorityHold::Contested => return parked(ContentParkReason::ContestedSubject),
        SubjectAuthorityHold::Clear => {},
    }
    // A boundary that could not be decided (a withheld watermark, an incomplete walk) parks here,
    // after the holds — it is the weakest verdict the registers can produce.
    Ok(boundary_decision)
}

/// Lower an `Invalid` citation into a verdict. `WrongSubject` is decided by bytes we already hold,
/// so it rejects outright; `ReferencedEntryNotEffective` is a verdict of OUR fold, and a fold
/// behind the author's may still be missing the `CutExtend` that re-blesses the citation (§11.4) —
/// so while that account's control log is ahead of us, it parks for refetch rather than hardening
/// into a rejection we would have to walk back.
fn invalid_citation(
    reason: AuthorityInvalidReason,
    freshness: AuthorityFreshness,
    reject: ContentRejectReason,
    ahead: ContentParkReason,
) -> ContentAcceptance {
    match (reason, freshness) {
        (AuthorityInvalidReason::WrongSubject, _)
        | (
            AuthorityInvalidReason::ReferencedEntryNotEffective,
            AuthorityFreshness::CurrentOrBehind,
        ) => ContentAcceptance::Rejected(reject),
        (AuthorityInvalidReason::ReferencedEntryNotEffective, AuthorityFreshness::Ahead) =>
            ContentAcceptance::Parked(ahead),
    }
}

/// Combine every applicable revocation register into one verdict, with the account fold's frozen
/// precedence (`register_verdict`): a definite condemnation outranks any park, so incomplete
/// ancestry on one register can never mask a `beyond_cut` (which needs no ancestry at all) on
/// another; a withheld watermark outranks a missing mid-chain link; `Open`/on-branch is clear.
fn combine_boundaries<F>(
    boundaries: &[AuthorityBoundary],
    seq: u64,
    entry_hash: AccountEntryHash,
    ancestry: &F,
) -> Option<ContentAcceptance>
where
    F: Fn(AccountEntryHash, AccountEntryHash) -> AncestryRelation,
{
    let rank = boundaries.iter().fold(0, |rank, boundary| {
        let candidate = match *boundary {
            AuthorityBoundary::Open => 0,
            AuthorityBoundary::Closed => 5,
            AuthorityBoundary::Cut { seq: cut_seq, .. } if seq > cut_seq => 3,
            AuthorityBoundary::Cut { hash, .. } => match ancestry(entry_hash, hash) {
                AncestryRelation::OnBranch => 0,
                AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry) => 1,
                AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget) => 2,
                AncestryRelation::OffBranch => 4,
            },
        };
        rank.max(candidate)
    });
    match rank {
        0 => None,
        1 => Some(ContentAcceptance::Parked(ContentParkReason::IncompleteCutAncestry)),
        2 => Some(ContentAcceptance::Parked(ContentParkReason::UnknownCutTarget)),
        3 => Some(ContentAcceptance::Condemned(ContentCondemnReason::BeyondCut)),
        4 => Some(ContentAcceptance::Condemned(ContentCondemnReason::OffBranch)),
        5 => Some(ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation)),
        _ => unreachable!("boundary ranks are closed"),
    }
}

#[cfg(test)]
#[path = "acceptance/tests.rs"]
mod tests;
