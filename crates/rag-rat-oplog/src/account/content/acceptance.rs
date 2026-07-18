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

use super::ContentEntryHeader;
use crate::account::{
    AccountId, AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason, AuthorityQuery,
    GrantDeviceAuthority, GrantDeviceBoundary, GrantRole, RosterContentAuthority,
};
use crate::stream::StreamId;

type EntryHash = [u8; 32];

/// Why an ancestry walk against a cut watermark could not be decided (mirrors the account fold's
/// `UnknownCause`: a withheld watermark parks, and never flips a verdict — I11).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownAncestry {
    /// The cut's watermark entry itself is not held.
    UnknownCutTarget,
    /// A link on the walk from the watermark toward the entry is missing.
    IncompleteCutAncestry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AncestryRelation {
    /// The entry is the watermark itself or an ancestor reached walking backward from it.
    OnBranch,
    OffBranch,
    Unknown(UnknownAncestry),
}

/// One freshness observation, bound to the exact query it answers. The pair (account, asserted
/// length) is carried so a result computed for the owner cannot be read as the author's, and a
/// result computed for a shorter assertion cannot stand in for the header's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CitedFreshness {
    pub account_id: AccountId,
    pub asserted_auth_len: u64,
    pub state: AuthorityFreshness,
}

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
    pub roster_ref: EntryHash,
    pub stream_id: StreamId,
    pub authority: RosterContentAuthority,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CitedGrantAuthority {
    pub owner_account_id: AccountId,
    pub grant_id: EntryHash,
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
        }
    }
}

#[derive(Clone)]
pub struct ContentAcceptanceInput<'a, F>
where
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
{
    pub header: &'a ContentEntryHeader,
    pub entry_hash: EntryHash,
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
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
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
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
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
    entry_hash: EntryHash,
    ancestry: &F,
) -> Option<ContentAcceptance>
where
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
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
mod tests {
    use super::*;
    use crate::account::{DeviceCut, GrantAuthority};
    use crate::op::DeviceFingerprint;

    const ENTRY_HASH: EntryHash = [9; 32];
    fn owner() -> AccountId {
        AccountId::from_bytes([5; 32])
    }

    fn author() -> AccountId {
        AccountId::from_bytes([6; 32])
    }

    fn header(author: AccountId, grant_id: Option<EntryHash>, seq: u64) -> ContentEntryHeader {
        ContentEntryHeader {
            stream_id: StreamId::from_bytes([1; 32]),
            author_account_id: author,
            device_fingerprint: DeviceFingerprint::from_bytes([2; 32]),
            seq,
            lamport: u64::MAX,
            prev_hash: (seq > 0).then_some([3; 32]),
            grant_id,
            roster_ref: [4; 32],
            owner_auth_len: 1,
            author_auth_len: 1,
            crypto_suite: 0,
            key_id: None,
        }
    }

    fn ownership(header: &ContentEntryHeader) -> AuthorityQuery<CitedOwnership> {
        AuthorityQuery::Effective(CitedOwnership {
            owner_account_id: owner(),
            stream_id: header.stream_id,
        })
    }

    fn roster(
        header: &ContentEntryHeader,
        boundary: AuthorityBoundary,
    ) -> AuthorityQuery<CitedRosterAuthority> {
        AuthorityQuery::Effective(CitedRosterAuthority {
            account_id: header.author_account_id,
            roster_ref: header.roster_ref,
            stream_id: header.stream_id,
            authority: RosterContentAuthority {
                device_fingerprint: header.device_fingerprint,
                boundary,
            },
        })
    }

    fn grant(
        header: &ContentEntryHeader,
        role: GrantRole,
        boundary: GrantDeviceBoundary,
    ) -> AuthorityQuery<CitedGrantAuthority> {
        AuthorityQuery::Effective(CitedGrantAuthority {
            owner_account_id: owner(),
            grant_id: header.grant_id.unwrap(),
            authority: GrantDeviceAuthority {
                grant: GrantAuthority {
                    stream_id: header.stream_id,
                    grantee_account_id: header.author_account_id,
                    role,
                },
                boundary,
            },
        })
    }

    fn freshness(
        account_id: AccountId,
        asserted_auth_len: u64,
        state: AuthorityFreshness,
    ) -> CitedFreshness {
        CitedFreshness { account_id, asserted_auth_len, state }
    }

    fn evaluate(
        header: &ContentEntryHeader,
        roster: AuthorityQuery<CitedRosterAuthority>,
        grant: Option<AuthorityQuery<CitedGrantAuthority>>,
        relation: AncestryRelation,
    ) -> ContentAcceptance {
        evaluate_content_acceptance(&ContentAcceptanceInput {
            header,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: true,
            ownership: ownership(header),
            roster,
            grant,
            owner_freshness: freshness(
                owner(),
                header.owner_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            author_freshness: freshness(
                header.author_account_id,
                header.author_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: move |_, _| relation,
        })
        .expect("test input has consistent freshness provenance")
    }

    #[test]
    fn owner_and_contributor_happy_paths_enforce_grant_shape_and_role() {
        let owner = header(owner(), None, 0);
        assert_eq!(
            evaluate(
                &owner,
                roster(&owner, AuthorityBoundary::Open),
                None,
                AncestryRelation::OnBranch
            ),
            ContentAcceptance::Accepted
        );
        let contributor = header(author(), Some([7; 32]), 0);
        assert_eq!(
            evaluate(
                &contributor,
                roster(&contributor, AuthorityBoundary::Open),
                Some(grant(&contributor, GrantRole::Writer, GrantDeviceBoundary::Open)),
                AncestryRelation::OnBranch
            ),
            ContentAcceptance::Accepted
        );
        assert_eq!(
            evaluate(
                &contributor,
                roster(&contributor, AuthorityBoundary::Open),
                Some(grant(&contributor, GrantRole::Reader, GrantDeviceBoundary::Open)),
                AncestryRelation::OnBranch
            ),
            ContentAcceptance::Rejected(ContentRejectReason::GrantDoesNotPermitWrite)
        );
    }

    #[test]
    fn structural_grant_coupling_precedes_cut_classification() {
        let malformed_owner = header(owner(), Some([7; 32]), 3);
        assert_eq!(
            evaluate(
                &malformed_owner,
                roster(&malformed_owner, AuthorityBoundary::Closed),
                None,
                AncestryRelation::OffBranch
            ),
            ContentAcceptance::Rejected(ContentRejectReason::UnexpectedGrant)
        );
        let malformed_contributor = header(author(), None, 3);
        assert_eq!(
            evaluate(
                &malformed_contributor,
                roster(&malformed_contributor, AuthorityBoundary::Closed),
                None,
                AncestryRelation::OffBranch
            ),
            ContentAcceptance::Rejected(ContentRejectReason::GrantRequired)
        );
    }

    #[test]
    fn exact_query_provenance_prevents_cross_owner_and_readd_laundering() {
        let entry = header(author(), Some([7; 32]), 0);
        let mut wrong_roster = match roster(&entry, AuthorityBoundary::Open) {
            AuthorityQuery::Effective(fact) => fact,
            _ => unreachable!(),
        };
        wrong_roster.roster_ref = [0xaa; 32];
        assert_eq!(
            evaluate(
                &entry,
                AuthorityQuery::Effective(wrong_roster),
                Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
                AncestryRelation::OnBranch
            ),
            ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid)
        );
        let mut wrong_grant = match grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open) {
            AuthorityQuery::Effective(fact) => fact,
            _ => unreachable!(),
        };
        wrong_grant.owner_account_id = AccountId::from_bytes([0xbb; 32]);
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, AuthorityBoundary::Open),
                Some(AuthorityQuery::Effective(wrong_grant)),
                AncestryRelation::OnBranch
            ),
            ContentAcceptance::Rejected(ContentRejectReason::GrantReferenceInvalid)
        );
        let mut wrong_ownership = match ownership(&entry) {
            AuthorityQuery::Effective(fact) => fact,
            _ => unreachable!(),
        };
        wrong_ownership.stream_id = StreamId::from_bytes([0xcc; 32]);
        let input = ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: true,
            ownership: AuthorityQuery::Effective(wrong_ownership),
            roster: roster(&entry, AuthorityBoundary::Open),
            grant: Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
            owner_freshness: freshness(
                owner(),
                entry.owner_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            author_freshness: freshness(
                author(),
                entry.author_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |_, _| AncestryRelation::OnBranch,
        };
        assert_eq!(
            evaluate_content_acceptance(&input),
            Ok(ContentAcceptance::Rejected(ContentRejectReason::OwnerReferenceInvalid))
        );
    }

    #[test]
    fn definite_boundary_outcomes_dominate_incomplete_sibling_boundaries() {
        let entry = header(author(), Some([7; 32]), 3);
        let cut = DeviceCut { device_fingerprint: entry.device_fingerprint, seq: 2, hash: [8; 32] };
        // The grant cut condemns on `seq` alone (no ancestry needed). Neither undecided ancestry
        // cause on the roster's register may mask it — otherwise a peer could park a condemnation
        // indefinitely by withholding one watermark.
        for undecided in [UnknownAncestry::IncompleteCutAncestry, UnknownAncestry::UnknownCutTarget]
        {
            assert_eq!(
                evaluate(
                    &entry,
                    roster(&entry, AuthorityBoundary::Cut { seq: 9, hash: [0xaa; 32] }),
                    Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Cut(cut.clone()))),
                    AncestryRelation::Unknown(undecided)
                ),
                ContentAcceptance::Condemned(ContentCondemnReason::BeyondCut)
            );
        }
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, AuthorityBoundary::Closed),
                Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
                AncestryRelation::OnBranch
            ),
            ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation)
        );
    }

    /// A withheld watermark and a missing mid-chain link are DIFFERENT states of not-knowing, and
    /// the account fold already names them apart. C3 keeps them apart too — a missing cut target
    /// tells a peer to fetch that entry, an incomplete chain tells it to fetch the walk — and it
    /// keeps the fold's precedence: the withheld watermark is the one reported when both apply.
    #[test]
    fn undecided_ancestry_causes_stay_distinct_and_keep_the_folds_precedence() {
        let entry = header(owner(), None, 2);
        let boundary = AuthorityBoundary::Cut { seq: 5, hash: [7; 32] };
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, boundary),
                None,
                AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget)
            ),
            ContentAcceptance::Parked(ContentParkReason::UnknownCutTarget)
        );
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, boundary),
                None,
                AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry)
            ),
            ContentAcceptance::Parked(ContentParkReason::IncompleteCutAncestry)
        );
        // Two registers, each undecided for a different reason: the withheld watermark wins.
        let contributor = header(author(), Some([7; 32]), 2);
        let cut =
            DeviceCut { device_fingerprint: contributor.device_fingerprint, seq: 5, hash: [8; 32] };
        let decision = evaluate_content_acceptance(&ContentAcceptanceInput {
            header: &contributor,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: true,
            ownership: ownership(&contributor),
            roster: roster(&contributor, boundary),
            grant: Some(grant(&contributor, GrantRole::Writer, GrantDeviceBoundary::Cut(cut))),
            owner_freshness: freshness(
                owner(),
                contributor.owner_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            author_freshness: freshness(
                author(),
                contributor.author_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            subject_hold: SubjectAuthorityHold::Clear,
            // The roster's watermark [7; 32] is held but its chain has a gap; the grant's watermark
            // [8; 32] is not held at all.
            ancestry: |_, watermark| {
                if watermark == [8; 32] {
                    AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget)
                } else {
                    AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry)
                }
            },
        });
        assert_eq!(
            decision,
            Ok(ContentAcceptance::Parked(ContentParkReason::UnknownCutTarget)),
            "a withheld watermark outranks a missing mid-chain link, as in the account fold",
        );
    }

    #[test]
    fn branch_precedes_freshness_but_authority_precedes_branch() {
        let entry = header(owner(), None, 0);
        let base = ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: false,
            ownership: ownership(&entry),
            roster: roster(&entry, AuthorityBoundary::Open),
            grant: None,
            owner_freshness: freshness(owner(), entry.owner_auth_len, AuthorityFreshness::Ahead),
            author_freshness: freshness(owner(), entry.author_auth_len, AuthorityFreshness::Ahead),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |_, _| AncestryRelation::OnBranch,
        };
        // A fork is decided by the content DAG alone, so an ahead control log cannot hide it.
        assert_eq!(evaluate_content_acceptance(&base), Ok(ContentAcceptance::Forked));
        // But a citation our own fold reads as ineffective is a verdict OF that fold, and while the
        // author's control log runs ahead of ours the CutExtend that re-blesses it may simply be
        // one of the ops we have not fetched (§11.4). Park for refetch — never harden into a
        // rejection we would have to walk back.
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                roster: AuthorityQuery::Invalid(
                    AuthorityInvalidReason::ReferencedEntryNotEffective,
                ),
                ..base.clone()
            }),
            Ok(ContentAcceptance::Parked(ContentParkReason::AuthorAuthLenAhead))
        );
        // Once we are level with the author, the same citation is a lie and rejects.
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                roster: AuthorityQuery::Invalid(
                    AuthorityInvalidReason::ReferencedEntryNotEffective,
                ),
                author_freshness: freshness(
                    owner(),
                    entry.author_auth_len,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                ..base.clone()
            }),
            Ok(ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid))
        );
        // A wrong-subject citation is contradicted by bytes we already hold, so no depth of control
        // log can rescue it: it rejects even while we are behind.
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                roster: AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject),
                ..base
            }),
            Ok(ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid))
        );
    }

    #[test]
    fn exact_cut_edge_freezes_ancestry_direction_and_missing_link_behavior() {
        use std::cell::Cell;

        let entry = header(owner(), None, 2);
        let boundary = AuthorityBoundary::Cut { seq: 2, hash: [7; 32] };
        for (relation, expected) in [
            (AncestryRelation::OnBranch, ContentAcceptance::Accepted),
            (
                AncestryRelation::OffBranch,
                ContentAcceptance::Condemned(ContentCondemnReason::OffBranch),
            ),
            (
                AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry),
                ContentAcceptance::Parked(ContentParkReason::IncompleteCutAncestry),
            ),
        ] {
            assert_eq!(evaluate(&entry, roster(&entry, boundary), None, relation), expected);
        }
        let arguments = Cell::new(None);
        let decision = evaluate_content_acceptance(&ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: true,
            ownership: ownership(&entry),
            roster: roster(&entry, boundary),
            grant: None,
            owner_freshness: freshness(
                owner(),
                entry.owner_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            author_freshness: freshness(
                owner(),
                entry.author_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |entry_hash, cut_hash| {
                arguments.set(Some((entry_hash, cut_hash)));
                AncestryRelation::OnBranch
            },
        });
        assert_eq!(decision, Ok(ContentAcceptance::Accepted));
        assert_eq!(arguments.get(), Some((ENTRY_HASH, [7; 32])));
    }

    #[test]
    fn authority_and_boundaries_precede_predecessor_branch_and_freshness() {
        let entry = header(owner(), None, 0);
        let base = ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: false,
            branch_selected: false,
            ownership: ownership(&entry),
            roster: roster(&entry, AuthorityBoundary::Open),
            grant: None,
            owner_freshness: freshness(owner(), entry.owner_auth_len, AuthorityFreshness::Ahead),
            author_freshness: freshness(owner(), entry.author_auth_len, AuthorityFreshness::Ahead),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |_, _| AncestryRelation::OnBranch,
        };
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                roster: AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject),
                ..base.clone()
            }),
            Ok(ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid))
        );
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                roster: roster(&entry, AuthorityBoundary::Closed),
                ..base.clone()
            }),
            Ok(ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation))
        );
        assert_eq!(
            evaluate_content_acceptance(&base),
            Ok(ContentAcceptance::Parked(ContentParkReason::MissingPredecessor))
        );
    }

    #[test]
    fn freshness_is_provenance_bound_and_runs_after_branch_selection() {
        let entry = header(owner(), None, 0);
        let mut input = ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: false,
            ownership: ownership(&entry),
            roster: roster(&entry, AuthorityBoundary::Open),
            grant: None,
            // Laundered: the owner's slot carries a verdict computed for the AUTHOR's account.
            owner_freshness: freshness(author(), entry.owner_auth_len, AuthorityFreshness::Ahead),
            author_freshness: freshness(owner(), entry.author_auth_len, AuthorityFreshness::Ahead),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |_, _| AncestryRelation::OnBranch,
        };
        // Provenance is a PRECONDITION, not a lazily-consulted value. The authority phase already
        // gates its rejections on freshness, so a verdict computed for the wrong account has to be
        // refused before ANY decision is derived from it — reaching a fork first does not excuse
        // it.
        assert_eq!(
            evaluate_content_acceptance(&input),
            Err(ContentAcceptanceInputError::FreshnessProvenance)
        );
        input.owner_freshness.account_id = owner();
        // With honest provenance, freshness still runs LAST: the fork the content DAG has already
        // decided is reported, never masked by an ahead counter.
        assert_eq!(evaluate_content_acceptance(&input), Ok(ContentAcceptance::Forked));
        input.branch_selected = true;
        assert_eq!(
            evaluate_content_acceptance(&input),
            Ok(ContentAcceptance::Parked(ContentParkReason::OwnerAuthLenAhead))
        );
        input.owner_freshness.state = AuthorityFreshness::CurrentOrBehind;
        assert_eq!(
            evaluate_content_acceptance(&input),
            Ok(ContentAcceptance::Parked(ContentParkReason::AuthorAuthLenAhead))
        );
    }

    #[test]
    fn grant_cut_requires_same_device_and_obeys_exact_and_closed_boundaries() {
        use std::cell::Cell;

        let entry = header(author(), Some([7; 32]), 2);
        let exact = DeviceCut {
            device_fingerprint: entry.device_fingerprint,
            seq: entry.seq,
            hash: [7; 32],
        };
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, AuthorityBoundary::Open),
                Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Cut(exact.clone()),)),
                AncestryRelation::OnBranch,
            ),
            ContentAcceptance::Accepted
        );
        for (relation, expected) in [
            (
                AncestryRelation::OffBranch,
                ContentAcceptance::Condemned(ContentCondemnReason::OffBranch),
            ),
            (
                AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry),
                ContentAcceptance::Parked(ContentParkReason::IncompleteCutAncestry),
            ),
        ] {
            assert_eq!(
                evaluate(
                    &entry,
                    roster(&entry, AuthorityBoundary::Open),
                    Some(
                        grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Cut(exact.clone()),)
                    ),
                    relation,
                ),
                expected
            );
        }
        let arguments = Cell::new(None);
        let decision = evaluate_content_acceptance(&ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: true,
            ownership: ownership(&entry),
            roster: roster(&entry, AuthorityBoundary::Open),
            grant: Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Cut(exact.clone()))),
            owner_freshness: freshness(
                owner(),
                entry.owner_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            author_freshness: freshness(
                author(),
                entry.author_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |entry_hash, cut_hash| {
                arguments.set(Some((entry_hash, cut_hash)));
                AncestryRelation::OnBranch
            },
        });
        assert_eq!(decision, Ok(ContentAcceptance::Accepted));
        assert_eq!(arguments.get(), Some((ENTRY_HASH, exact.hash)));
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, AuthorityBoundary::Open),
                Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Closed)),
                AncestryRelation::OnBranch,
            ),
            ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation)
        );
        let wrong_device =
            DeviceCut { device_fingerprint: DeviceFingerprint::from_bytes([0xee; 32]), ..exact };
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, AuthorityBoundary::Open),
                Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Cut(wrong_device))),
                AncestryRelation::OnBranch,
            ),
            ContentAcceptance::Rejected(ContentRejectReason::GrantReferenceInvalid)
        );
    }

    #[test]
    fn unresolved_and_invalid_authority_states_are_contextual_and_fail_closed() {
        let entry = header(author(), Some([7; 32]), 0);
        let base = ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: true,
            ownership: ownership(&entry),
            roster: roster(&entry, AuthorityBoundary::Open),
            grant: Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
            owner_freshness: freshness(
                owner(),
                entry.owner_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            author_freshness: freshness(
                author(),
                entry.author_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |_, _| AncestryRelation::OnBranch,
        };
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                ownership: AuthorityQuery::Unknown,
                ..base.clone()
            }),
            Ok(ContentAcceptance::Parked(ContentParkReason::UnknownOwner))
        );
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                roster: AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject),
                ..base.clone()
            }),
            Ok(ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid))
        );
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                grant: Some(AuthorityQuery::Unknown),
                ..base.clone()
            }),
            Ok(ContentAcceptance::Parked(ContentParkReason::UnknownGrant))
        );
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                grant: Some(AuthorityQuery::Invalid(
                    AuthorityInvalidReason::ReferencedEntryNotEffective,
                )),
                ..base
            }),
            Ok(ContentAcceptance::Rejected(ContentRejectReason::GrantReferenceInvalid))
        );
    }

    #[test]
    fn beyond_and_closed_boundaries_do_not_consult_ancestry() {
        use std::cell::Cell;

        let entry = header(owner(), None, u64::MAX);
        for (boundary, expected) in [
            (
                AuthorityBoundary::Cut { seq: u64::MAX - 1, hash: [7; 32] },
                ContentAcceptance::Condemned(ContentCondemnReason::BeyondCut),
            ),
            (
                AuthorityBoundary::Closed,
                ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation),
            ),
        ] {
            let calls = Cell::new(0);
            let decision = evaluate_content_acceptance(&ContentAcceptanceInput {
                header: &entry,
                entry_hash: ENTRY_HASH,
                owner_account_id: owner(),
                dense_predecessor_reachable: true,
                branch_selected: true,
                ownership: ownership(&entry),
                roster: roster(&entry, boundary),
                grant: None,
                owner_freshness: freshness(
                    owner(),
                    entry.owner_auth_len,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                author_freshness: freshness(
                    owner(),
                    entry.author_auth_len,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                subject_hold: SubjectAuthorityHold::Clear,
                ancestry: |_, _| {
                    calls.set(calls.get() + 1);
                    AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget)
                },
            });
            assert_eq!(decision, Ok(expected));
            assert_eq!(calls.get(), 0);
        }
    }

    #[test]
    fn a_contested_subject_hold_is_fail_closed_and_re_evaluable() {
        let entry = header(owner(), None, 0);
        // A contested subject fails content closed even with an Open boundary and clear ancestry —
        // the hold is quota-bounded and reclassifies on recovery, never a flipped verdict.
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                header: &entry,
                entry_hash: ENTRY_HASH,
                owner_account_id: owner(),
                dense_predecessor_reachable: true,
                branch_selected: true,
                ownership: ownership(&entry),
                roster: roster(&entry, AuthorityBoundary::Open),
                grant: None,
                owner_freshness: freshness(
                    owner(),
                    entry.owner_auth_len,
                    AuthorityFreshness::CurrentOrBehind
                ),
                author_freshness: freshness(
                    owner(),
                    entry.author_auth_len,
                    AuthorityFreshness::CurrentOrBehind
                ),
                subject_hold: SubjectAuthorityHold::Contested,
                ancestry: |_, _| AncestryRelation::OnBranch,
            }),
            Ok(ContentAcceptance::Parked(ContentParkReason::ContestedSubject))
        );
    }

    #[test]
    fn every_authority_provenance_component_is_enforced() {
        let entry = header(author(), Some([7; 32]), 0);
        let expected = Ok(ContentAcceptance::Rejected(ContentRejectReason::OwnerReferenceInvalid));
        for fact in [
            CitedOwnership { owner_account_id: author(), stream_id: entry.stream_id },
            CitedOwnership {
                owner_account_id: owner(),
                stream_id: StreamId::from_bytes([0xa1; 32]),
            },
        ] {
            let input = ContentAcceptanceInput {
                header: &entry,
                entry_hash: ENTRY_HASH,
                owner_account_id: owner(),
                dense_predecessor_reachable: true,
                branch_selected: true,
                ownership: AuthorityQuery::Effective(fact),
                roster: roster(&entry, AuthorityBoundary::Open),
                grant: Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
                owner_freshness: freshness(
                    owner(),
                    entry.owner_auth_len,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                author_freshness: freshness(
                    author(),
                    entry.author_auth_len,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                subject_hold: SubjectAuthorityHold::Clear,
                ancestry: |_, _| AncestryRelation::OnBranch,
            };
            assert_eq!(evaluate_content_acceptance(&input), expected);
        }

        let valid_roster = match roster(&entry, AuthorityBoundary::Open) {
            AuthorityQuery::Effective(fact) => fact,
            _ => unreachable!(),
        };
        for fact in [
            CitedRosterAuthority { account_id: owner(), ..valid_roster },
            CitedRosterAuthority { roster_ref: [0xa2; 32], ..valid_roster },
            CitedRosterAuthority { stream_id: StreamId::from_bytes([0xa3; 32]), ..valid_roster },
            CitedRosterAuthority {
                authority: RosterContentAuthority {
                    device_fingerprint: DeviceFingerprint::from_bytes([0xa4; 32]),
                    boundary: AuthorityBoundary::Open,
                },
                ..valid_roster
            },
        ] {
            assert_eq!(
                evaluate(
                    &entry,
                    AuthorityQuery::Effective(fact),
                    Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
                    AncestryRelation::OnBranch,
                ),
                ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid)
            );
        }

        let valid_grant = match grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open) {
            AuthorityQuery::Effective(fact) => fact,
            _ => unreachable!(),
        };
        let mut wrong_grant_id = valid_grant.clone();
        wrong_grant_id.grant_id = [0xa5; 32];
        let mut wrong_stream = valid_grant.clone();
        wrong_stream.authority.grant.stream_id = StreamId::from_bytes([0xa6; 32]);
        let mut wrong_grantee = valid_grant.clone();
        wrong_grantee.authority.grant.grantee_account_id = owner();
        for fact in [
            CitedGrantAuthority { owner_account_id: author(), ..valid_grant.clone() },
            wrong_grant_id,
            wrong_stream,
            wrong_grantee,
        ] {
            assert_eq!(
                evaluate(
                    &entry,
                    roster(&entry, AuthorityBoundary::Open),
                    Some(AuthorityQuery::Effective(fact)),
                    AncestryRelation::OnBranch,
                ),
                ContentAcceptance::Rejected(ContentRejectReason::GrantReferenceInvalid)
            );
        }
    }

    #[test]
    fn every_freshness_provenance_component_is_enforced() {
        let entry = header(author(), Some([7; 32]), 0);
        let base = ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: true,
            ownership: ownership(&entry),
            roster: roster(&entry, AuthorityBoundary::Open),
            grant: Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
            owner_freshness: freshness(
                owner(),
                entry.owner_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            author_freshness: freshness(
                author(),
                entry.author_auth_len,
                AuthorityFreshness::CurrentOrBehind,
            ),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |_, _| AncestryRelation::OnBranch,
        };
        for input in [
            ContentAcceptanceInput {
                owner_freshness: freshness(
                    author(),
                    entry.owner_auth_len,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                ..base.clone()
            },
            ContentAcceptanceInput {
                owner_freshness: freshness(
                    owner(),
                    entry.owner_auth_len + 1,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                ..base.clone()
            },
            ContentAcceptanceInput {
                author_freshness: freshness(
                    owner(),
                    entry.author_auth_len,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                ..base.clone()
            },
            ContentAcceptanceInput {
                author_freshness: freshness(
                    author(),
                    entry.author_auth_len + 1,
                    AuthorityFreshness::CurrentOrBehind,
                ),
                ..base
            },
        ] {
            assert_eq!(
                evaluate_content_acceptance(&input),
                Err(ContentAcceptanceInputError::FreshnessProvenance)
            );
        }
    }

    #[test]
    fn combined_boundaries_and_subject_holds_have_stable_precedence() {
        let boundaries =
            [AuthorityBoundary::Cut { seq: 9, hash: [1; 32] }, AuthorityBoundary::Cut {
                seq: 1,
                hash: [2; 32],
            }];
        assert_eq!(
            combine_boundaries(&boundaries, 2, ENTRY_HASH, &|_, hash| {
                if hash == [1; 32] {
                    AncestryRelation::OffBranch
                } else {
                    AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget)
                }
            }),
            Some(ContentAcceptance::Condemned(ContentCondemnReason::OffBranch))
        );
        assert_eq!(
            combine_boundaries(
                &[AuthorityBoundary::Cut { seq: 9, hash: [1; 32] }, AuthorityBoundary::Closed],
                2,
                ENTRY_HASH,
                &|_, _| AncestryRelation::OffBranch,
            ),
            Some(ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation))
        );

        let entry = header(owner(), None, 1);
        for (boundary, hold, predecessor, expected) in [
            (
                AuthorityBoundary::Closed,
                SubjectAuthorityHold::Contested,
                false,
                ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation),
            ),
            (
                AuthorityBoundary::Cut { seq: 0, hash: [1; 32] },
                SubjectAuthorityHold::Contested,
                false,
                ContentAcceptance::Condemned(ContentCondemnReason::BeyondCut),
            ),
            (
                AuthorityBoundary::Cut { seq: 1, hash: [1; 32] },
                SubjectAuthorityHold::Contested,
                true,
                ContentAcceptance::Parked(ContentParkReason::ContestedSubject),
            ),
        ] {
            let decision = evaluate_content_acceptance(&ContentAcceptanceInput {
                header: &entry,
                entry_hash: ENTRY_HASH,
                owner_account_id: owner(),
                dense_predecessor_reachable: predecessor,
                branch_selected: true,
                ownership: ownership(&entry),
                roster: roster(&entry, boundary),
                grant: None,
                owner_freshness: freshness(owner(), 1, AuthorityFreshness::CurrentOrBehind),
                author_freshness: freshness(owner(), 1, AuthorityFreshness::CurrentOrBehind),
                subject_hold: hold,
                // Incomplete (not withheld) ancestry, so the boundary's own park is the WEAKER
                // token: each expected verdict below is attributable to the hold, not to this.
                ancestry: |_, _| AncestryRelation::Unknown(UnknownAncestry::IncompleteCutAncestry),
            });
            assert_eq!(decision, Ok(expected));
        }
    }

    #[test]
    fn owner_rejects_an_unexpected_supplied_grant_fact() {
        let owner_entry = header(owner(), None, 0);
        let contributor_entry = header(author(), Some([7; 32]), 0);
        assert_eq!(
            evaluate(
                &owner_entry,
                roster(&owner_entry, AuthorityBoundary::Open),
                Some(grant(&contributor_entry, GrantRole::Writer, GrantDeviceBoundary::Open,)),
                AncestryRelation::OnBranch,
            ),
            ContentAcceptance::Rejected(ContentRejectReason::UnexpectedGrant)
        );
    }

    #[test]
    fn lamport_is_irrelevant_and_status_tokens_cover_frozen_states() {
        let mut first = header(owner(), None, 0);
        let mut second = first.clone();
        first.lamport = 0;
        second.lamport = u64::MAX;
        assert_eq!(
            evaluate(
                &first,
                roster(&first, AuthorityBoundary::Open),
                None,
                AncestryRelation::OnBranch
            ),
            evaluate(
                &second,
                roster(&second, AuthorityBoundary::Open),
                None,
                AncestryRelation::OnBranch
            )
        );
        let cases = [
            (ContentAcceptance::Accepted, "accepted"),
            (ContentAcceptance::Forked, "forked"),
            (
                ContentAcceptance::Parked(ContentParkReason::MissingPredecessor),
                "parked{missing_predecessor}",
            ),
            (ContentAcceptance::Parked(ContentParkReason::UnknownOwner), "parked{unknown_account}"),
            (
                ContentAcceptance::Parked(ContentParkReason::UnknownRosterRef),
                "parked{unknown_roster_ref}",
            ),
            (ContentAcceptance::Parked(ContentParkReason::UnknownGrant), "parked{unknown_grant}"),
            (
                ContentAcceptance::Parked(ContentParkReason::OwnerAuthLenAhead),
                "parked{auth_len_ahead}",
            ),
            (
                ContentAcceptance::Parked(ContentParkReason::AuthorAuthLenAhead),
                "parked{auth_len_ahead}",
            ),
            (
                ContentAcceptance::Parked(ContentParkReason::IncompleteCutAncestry),
                "parked{incomplete_cut_ancestry}",
            ),
            (
                ContentAcceptance::Parked(ContentParkReason::UnknownCutTarget),
                "parked{unknown_cut_target}",
            ),
            (
                ContentAcceptance::Parked(ContentParkReason::ContestedSubject),
                "parked{contested_subject}",
            ),
            (
                ContentAcceptance::Condemned(ContentCondemnReason::BeyondCut),
                "condemned{beyond_cut}",
            ),
            (
                ContentAcceptance::Condemned(ContentCondemnReason::OffBranch),
                "condemned{off_branch}",
            ),
            (
                ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation),
                "condemned{closed_incarnation}",
            ),
            (
                ContentAcceptance::Rejected(ContentRejectReason::OwnerReferenceInvalid),
                "rejected{invalid_owner}",
            ),
            (
                ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid),
                "rejected{invalid_roster_ref}",
            ),
            (
                ContentAcceptance::Rejected(ContentRejectReason::GrantReferenceInvalid),
                "rejected{invalid_grant}",
            ),
            (
                ContentAcceptance::Rejected(ContentRejectReason::GrantRequired),
                "rejected{grant_required}",
            ),
            (
                ContentAcceptance::Rejected(ContentRejectReason::UnexpectedGrant),
                "rejected{unexpected_grant}",
            ),
            (
                ContentAcceptance::Rejected(ContentRejectReason::GrantDoesNotPermitWrite),
                "rejected{grant_not_writer}",
            ),
        ];
        for (decision, token) in cases {
            assert_eq!(decision.as_db_str(), token);
        }
    }
}
