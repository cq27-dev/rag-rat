//! Pure C3 `/3` acceptance predicate (§13).
//!
//! Persistence supplies provenance-bound authority facts and ancestry observations from one
//! snapshot. Late control or ancestry arrival re-evaluates the same candidates; history is never
//! mutated. Freshness is deliberately separate from authority so the frozen order remains
//! authority/registers → branch → freshness.

use super::ContentEntryHeader;
use crate::oplog::account::{
    AccountId, AuthorityBoundary, AuthorityInvalidReason, GrantDeviceAuthority,
    GrantDeviceBoundary, GrantRole, RosterContentAuthority,
};
use crate::oplog::stream::StreamId;

type EntryHash = [u8; 32];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AncestryRelation {
    /// The entry is the watermark itself or an ancestor reached walking backward from it.
    OnBranch,
    OffBranch,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthorityFreshness {
    CurrentOrBehind,
    Ahead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CitedFreshness {
    pub(crate) account_id: AccountId,
    pub(crate) asserted_auth_len: u64,
    pub(crate) state: AuthorityFreshness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubjectAuthorityHold {
    Clear,
    UnknownCutTarget,
    Contested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolvedAuthority<T> {
    Effective(T),
    Unknown,
    Invalid(AuthorityInvalidReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CitedOwnership {
    pub(crate) owner_account_id: AccountId,
    pub(crate) stream_id: StreamId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CitedRosterAuthority {
    pub(crate) account_id: AccountId,
    pub(crate) roster_ref: EntryHash,
    pub(crate) stream_id: StreamId,
    pub(crate) authority: RosterContentAuthority,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CitedGrantAuthority {
    pub(crate) owner_account_id: AccountId,
    pub(crate) grant_id: EntryHash,
    pub(crate) authority: GrantDeviceAuthority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentParkReason {
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
pub(crate) enum ContentCondemnReason {
    BeyondCut,
    OffBranch,
    ClosedIncarnation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentRejectReason {
    OwnerReferenceInvalid,
    RosterReferenceInvalid,
    GrantReferenceInvalid,
    GrantRequired,
    UnexpectedGrant,
    GrantDoesNotPermitWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentAcceptanceInputError {
    FreshnessProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentAcceptance {
    Accepted,
    Forked,
    Parked(ContentParkReason),
    Condemned(ContentCondemnReason),
    Rejected(ContentRejectReason),
}

impl ContentAcceptance {
    pub(crate) fn as_db_str(self) -> &'static str {
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
pub(crate) struct ContentAcceptanceInput<'a, F>
where
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
{
    pub(crate) header: &'a ContentEntryHeader,
    pub(crate) entry_hash: EntryHash,
    pub(crate) owner_account_id: AccountId,
    pub(crate) dense_predecessor_reachable: bool,
    pub(crate) branch_selected: bool,
    pub(crate) ownership: ResolvedAuthority<CitedOwnership>,
    pub(crate) roster: ResolvedAuthority<CitedRosterAuthority>,
    pub(crate) grant: Option<ResolvedAuthority<CitedGrantAuthority>>,
    pub(crate) owner_freshness: CitedFreshness,
    pub(crate) author_freshness: CitedFreshness,
    pub(crate) subject_hold: SubjectAuthorityHold,
    pub(crate) ancestry: F,
}

pub(crate) fn evaluate_content_acceptance<F>(
    input: &ContentAcceptanceInput<'_, F>,
) -> Result<ContentAcceptance, ContentAcceptanceInputError>
where
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
{
    let is_owner = input.header.author_account_id == input.owner_account_id;
    if is_owner && (input.header.grant_id.is_some() || input.grant.is_some()) {
        return Ok(ContentAcceptance::Rejected(ContentRejectReason::UnexpectedGrant));
    }
    if !is_owner && input.header.grant_id.is_none() {
        return Ok(ContentAcceptance::Rejected(ContentRejectReason::GrantRequired));
    }
    match input.ownership {
        ResolvedAuthority::Effective(fact)
            if fact.owner_account_id == input.owner_account_id
                && fact.stream_id == input.header.stream_id => {},
        ResolvedAuthority::Unknown =>
            return Ok(ContentAcceptance::Parked(ContentParkReason::UnknownOwner)),
        ResolvedAuthority::Effective(_) | ResolvedAuthority::Invalid(_) =>
            return Ok(ContentAcceptance::Rejected(ContentRejectReason::OwnerReferenceInvalid)),
    }
    let roster = match input.roster {
        ResolvedAuthority::Effective(fact)
            if fact.account_id == input.header.author_account_id
                && fact.roster_ref == input.header.roster_ref
                && fact.stream_id == input.header.stream_id
                && fact.authority.device_fingerprint == input.header.device_fingerprint =>
            fact,
        ResolvedAuthority::Unknown =>
            return Ok(ContentAcceptance::Parked(ContentParkReason::UnknownRosterRef)),
        ResolvedAuthority::Effective(_) | ResolvedAuthority::Invalid(_) =>
            return Ok(ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid)),
    };
    let mut boundaries = vec![roster.authority.boundary];

    if !is_owner {
        let grant = match input.grant.as_ref() {
            Some(ResolvedAuthority::Effective(fact))
                if fact.owner_account_id == input.owner_account_id
                    && Some(fact.grant_id) == input.header.grant_id
                    && fact.authority.grant.stream_id == input.header.stream_id
                    && fact.authority.grant.grantee_account_id
                        == input.header.author_account_id =>
                fact,
            Some(ResolvedAuthority::Unknown) | None =>
                return Ok(ContentAcceptance::Parked(ContentParkReason::UnknownGrant)),
            Some(ResolvedAuthority::Effective(_)) | Some(ResolvedAuthority::Invalid(_)) =>
                return Ok(ContentAcceptance::Rejected(ContentRejectReason::GrantReferenceInvalid)),
        };
        if grant.authority.grant.role != GrantRole::Writer {
            return Ok(ContentAcceptance::Rejected(ContentRejectReason::GrantDoesNotPermitWrite));
        }
        boundaries.push(match &grant.authority.boundary {
            GrantDeviceBoundary::Open => AuthorityBoundary::Open,
            GrantDeviceBoundary::Cut(cut)
                if cut.device_fingerprint == input.header.device_fingerprint =>
                AuthorityBoundary::Cut { seq: cut.seq, hash: cut.hash },
            GrantDeviceBoundary::Cut(_) =>
                return Ok(ContentAcceptance::Rejected(ContentRejectReason::GrantReferenceInvalid)),
            GrantDeviceBoundary::Closed => AuthorityBoundary::Closed,
        });
    }

    let boundary_decision =
        combine_boundaries(&boundaries, input.header.seq, input.entry_hash, &input.ancestry);
    if matches!(boundary_decision, Some(ContentAcceptance::Condemned(_))) {
        return Ok(boundary_decision.expect("matched Some"));
    }
    match input.subject_hold {
        SubjectAuthorityHold::UnknownCutTarget =>
            return Ok(ContentAcceptance::Parked(ContentParkReason::UnknownCutTarget)),
        SubjectAuthorityHold::Contested =>
            return Ok(ContentAcceptance::Parked(ContentParkReason::ContestedSubject)),
        SubjectAuthorityHold::Clear => {},
    }
    if let Some(decision) = boundary_decision {
        return Ok(decision);
    }
    if !input.dense_predecessor_reachable {
        return Ok(ContentAcceptance::Parked(ContentParkReason::MissingPredecessor));
    }
    if !input.branch_selected {
        return Ok(ContentAcceptance::Forked);
    }
    if input.owner_freshness.account_id != input.owner_account_id
        || input.owner_freshness.asserted_auth_len != input.header.owner_auth_len
        || input.author_freshness.account_id != input.header.author_account_id
        || input.author_freshness.asserted_auth_len != input.header.author_auth_len
    {
        return Err(ContentAcceptanceInputError::FreshnessProvenance);
    }
    if input.owner_freshness.state == AuthorityFreshness::Ahead {
        return Ok(ContentAcceptance::Parked(ContentParkReason::OwnerAuthLenAhead));
    }
    if input.author_freshness.state == AuthorityFreshness::Ahead {
        return Ok(ContentAcceptance::Parked(ContentParkReason::AuthorAuthLenAhead));
    }
    Ok(ContentAcceptance::Accepted)
}

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
            AuthorityBoundary::Closed => 4,
            AuthorityBoundary::Cut { seq: cut_seq, .. } if seq > cut_seq => 2,
            AuthorityBoundary::Cut { hash, .. } => match ancestry(entry_hash, hash) {
                AncestryRelation::OnBranch => 0,
                AncestryRelation::Unknown => 1,
                AncestryRelation::OffBranch => 3,
            },
        };
        rank.max(candidate)
    });
    match rank {
        0 => None,
        1 => Some(ContentAcceptance::Parked(ContentParkReason::IncompleteCutAncestry)),
        2 => Some(ContentAcceptance::Condemned(ContentCondemnReason::BeyondCut)),
        3 => Some(ContentAcceptance::Condemned(ContentCondemnReason::OffBranch)),
        4 => Some(ContentAcceptance::Condemned(ContentCondemnReason::ClosedIncarnation)),
        _ => unreachable!("boundary ranks are closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::account::{DeviceCut, GrantAuthority};
    use crate::oplog::op::DeviceFingerprint;

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

    fn ownership(header: &ContentEntryHeader) -> ResolvedAuthority<CitedOwnership> {
        ResolvedAuthority::Effective(CitedOwnership {
            owner_account_id: owner(),
            stream_id: header.stream_id,
        })
    }

    fn roster(
        header: &ContentEntryHeader,
        boundary: AuthorityBoundary,
    ) -> ResolvedAuthority<CitedRosterAuthority> {
        ResolvedAuthority::Effective(CitedRosterAuthority {
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
    ) -> ResolvedAuthority<CitedGrantAuthority> {
        ResolvedAuthority::Effective(CitedGrantAuthority {
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
        roster: ResolvedAuthority<CitedRosterAuthority>,
        grant: Option<ResolvedAuthority<CitedGrantAuthority>>,
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
            ResolvedAuthority::Effective(fact) => fact,
            _ => unreachable!(),
        };
        wrong_roster.roster_ref = [0xaa; 32];
        assert_eq!(
            evaluate(
                &entry,
                ResolvedAuthority::Effective(wrong_roster),
                Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
                AncestryRelation::OnBranch
            ),
            ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid)
        );
        let mut wrong_grant = match grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open) {
            ResolvedAuthority::Effective(fact) => fact,
            _ => unreachable!(),
        };
        wrong_grant.owner_account_id = AccountId::from_bytes([0xbb; 32]);
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, AuthorityBoundary::Open),
                Some(ResolvedAuthority::Effective(wrong_grant)),
                AncestryRelation::OnBranch
            ),
            ContentAcceptance::Rejected(ContentRejectReason::GrantReferenceInvalid)
        );
        let mut wrong_ownership = match ownership(&entry) {
            ResolvedAuthority::Effective(fact) => fact,
            _ => unreachable!(),
        };
        wrong_ownership.stream_id = StreamId::from_bytes([0xcc; 32]);
        let input = ContentAcceptanceInput {
            header: &entry,
            entry_hash: ENTRY_HASH,
            owner_account_id: owner(),
            dense_predecessor_reachable: true,
            branch_selected: true,
            ownership: ResolvedAuthority::Effective(wrong_ownership),
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
        assert_eq!(
            evaluate(
                &entry,
                roster(&entry, AuthorityBoundary::Cut { seq: 9, hash: [0xaa; 32] }),
                Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Cut(cut))),
                AncestryRelation::Unknown
            ),
            ContentAcceptance::Condemned(ContentCondemnReason::BeyondCut)
        );
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
        assert_eq!(evaluate_content_acceptance(&base), Ok(ContentAcceptance::Forked));
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                roster: ResolvedAuthority::Invalid(
                    AuthorityInvalidReason::ReferencedEntryNotEffective,
                ),
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
                AncestryRelation::Unknown,
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
                roster: ResolvedAuthority::Invalid(AuthorityInvalidReason::WrongSubject),
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
            owner_freshness: freshness(author(), entry.owner_auth_len, AuthorityFreshness::Ahead),
            author_freshness: freshness(owner(), entry.author_auth_len, AuthorityFreshness::Ahead),
            subject_hold: SubjectAuthorityHold::Clear,
            ancestry: |_, _| AncestryRelation::OnBranch,
        };
        assert_eq!(evaluate_content_acceptance(&input), Ok(ContentAcceptance::Forked));
        input.branch_selected = true;
        assert_eq!(
            evaluate_content_acceptance(&input),
            Err(ContentAcceptanceInputError::FreshnessProvenance)
        );
        input.owner_freshness.account_id = owner();
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
                AncestryRelation::Unknown,
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
                ownership: ResolvedAuthority::Unknown,
                ..base.clone()
            }),
            Ok(ContentAcceptance::Parked(ContentParkReason::UnknownOwner))
        );
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                roster: ResolvedAuthority::Invalid(AuthorityInvalidReason::WrongSubject),
                ..base.clone()
            }),
            Ok(ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid))
        );
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                grant: Some(ResolvedAuthority::Unknown),
                ..base.clone()
            }),
            Ok(ContentAcceptance::Parked(ContentParkReason::UnknownGrant))
        );
        assert_eq!(
            evaluate_content_acceptance(&ContentAcceptanceInput {
                grant: Some(ResolvedAuthority::Invalid(
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
                    AncestryRelation::Unknown
                },
            });
            assert_eq!(decision, Ok(expected));
            assert_eq!(calls.get(), 0);
        }
    }

    #[test]
    fn subject_holds_are_fail_closed_and_re_evaluable() {
        let entry = header(owner(), None, 0);
        for (hold, expected) in [
            (
                SubjectAuthorityHold::UnknownCutTarget,
                ContentAcceptance::Parked(ContentParkReason::UnknownCutTarget),
            ),
            (
                SubjectAuthorityHold::Contested,
                ContentAcceptance::Parked(ContentParkReason::ContestedSubject),
            ),
        ] {
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
                    subject_hold: hold,
                    ancestry: |_, _| AncestryRelation::OnBranch,
                }),
                Ok(expected)
            );
        }
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
                ownership: ResolvedAuthority::Effective(fact),
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
            ResolvedAuthority::Effective(fact) => fact,
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
                    ResolvedAuthority::Effective(fact),
                    Some(grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open)),
                    AncestryRelation::OnBranch,
                ),
                ContentAcceptance::Rejected(ContentRejectReason::RosterReferenceInvalid)
            );
        }

        let valid_grant = match grant(&entry, GrantRole::Writer, GrantDeviceBoundary::Open) {
            ResolvedAuthority::Effective(fact) => fact,
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
                    Some(ResolvedAuthority::Effective(fact)),
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
                    AncestryRelation::Unknown
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
                SubjectAuthorityHold::UnknownCutTarget,
                false,
                ContentAcceptance::Condemned(ContentCondemnReason::BeyondCut),
            ),
            (
                AuthorityBoundary::Cut { seq: 1, hash: [1; 32] },
                SubjectAuthorityHold::Contested,
                true,
                ContentAcceptance::Parked(ContentParkReason::ContestedSubject),
            ),
            (
                AuthorityBoundary::Open,
                SubjectAuthorityHold::UnknownCutTarget,
                false,
                ContentAcceptance::Parked(ContentParkReason::UnknownCutTarget),
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
                ancestry: |_, _| AncestryRelation::Unknown,
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
