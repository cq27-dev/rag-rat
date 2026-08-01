//! Pure secrets-log (`log_id = 1`) acceptance predicate for `StreamKeyWrap` (§15, C4.2b).
//!
//! The mirror of the `/3` content predicate ([`super::super::content::acceptance`]), reduced to the
//! secrets authority model: a wrap is authorized only by a **live owner incarnation of the stream's
//! owning account** (the owner-only gate, B3), cited in the header's `authority_ref`, with the
//! account's own `StreamOwn{stream}` effective first. There is a SINGLE account here — the owning
//! account — so ONE freshness axis, not the content predicate's owner+author pair.
//!
//! Every authority fact is resolved against the CURRENT fold (§7) in ONE snapshot, so a refold
//! committing mid-evaluation cannot pair an old incarnation with a new cut. Freshness is applied
//! LAST (a separate axis), with its one reach backward being the citation-invalidity gate: an
//! incarnation our fold reads as ineffective is fold-dependent (a `CutExtend` we have not folded
//! re-blesses it, §11.4), so while we are behind the author that parks rather than hardening into a
//! rejection.

use super::super::{
    AccountId, AuthorityBoundary, AuthorityFreshness, AuthorityInvalidReason, AuthorityQuery,
    OwnerChainAuthority,
};

type EntryHash = [u8; 32];

/// Why an ancestry walk against a cut watermark could not be decided (mirrors the account fold's
/// `UnknownCause`: a withheld watermark parks, and never flips a verdict — I11).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::account) enum UnknownAncestry {
    /// The cut's watermark entry itself is not held.
    UnknownCutTarget,
    /// A link on the walk from the watermark toward the entry is missing.
    IncompleteCutAncestry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::account) enum AncestryRelation {
    OnBranch,
    OffBranch,
    Unknown(UnknownAncestry),
}

/// One freshness observation, bound to the exact query it answers. The pair (account, asserted
/// length) is carried so a result computed for another account, or for a shorter assertion than the
/// header makes, decides nothing about THIS entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::account) struct CitedFreshness {
    pub(in crate::account) account_id: AccountId,
    pub(in crate::account) asserted_auth_len: u64,
    pub(in crate::account) state: AuthorityFreshness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::account) enum SecretsParkReason {
    MissingPredecessor,
    /// The cited owner incarnation is not held yet.
    UnknownOwnerRef,
    /// The account's `StreamOwn{stream}` fact is not folded yet — the owner-only gate cannot
    /// resolve.
    UnknownStreamOwner,
    AuthLenAhead,
    IncompleteCutAncestry,
    UnknownCutTarget,
    ContestedSubject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::account) enum SecretsCondemnReason {
    BeyondCut,
    OffBranch,
    ClosedIncarnation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::account) enum SecretsRejectReason {
    /// The `authority_ref` is null, names a different-subject entry, or an incarnation our fold
    /// reads as ineffective while we are level with the author (the owner-only gate, B3).
    OwnerReferenceInvalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::account) enum SecretsAcceptanceInputError {
    FreshnessProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::account) enum SecretsAcceptance {
    Accepted,
    Forked,
    Parked(SecretsParkReason),
    Condemned(SecretsCondemnReason),
    Rejected(SecretsRejectReason),
}

impl SecretsAcceptance {
    /// The persisted `(status, detail)` PAIR (§16.3) — account rows store status + detail columns,
    /// NOT the content predicate's combined `status{detail}` strings (S-b).
    pub(in crate::account) fn as_db_pair(self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Accepted => ("accepted", None),
            Self::Forked => ("forked", None),
            Self::Parked(SecretsParkReason::MissingPredecessor) =>
                ("parked", Some("missing_predecessor")),
            Self::Parked(SecretsParkReason::UnknownOwnerRef) =>
                ("parked", Some("unknown_owner_ref")),
            Self::Parked(SecretsParkReason::UnknownStreamOwner) =>
                ("parked", Some("unknown_account")),
            Self::Parked(SecretsParkReason::AuthLenAhead) => ("parked", Some("auth_len_ahead")),
            Self::Parked(SecretsParkReason::IncompleteCutAncestry) =>
                ("parked", Some("incomplete_cut_ancestry")),
            Self::Parked(SecretsParkReason::UnknownCutTarget) =>
                ("parked", Some("unknown_cut_target")),
            Self::Parked(SecretsParkReason::ContestedSubject) =>
                ("parked", Some("contested_subject")),
            Self::Condemned(SecretsCondemnReason::BeyondCut) => ("condemned", Some("beyond_cut")),
            Self::Condemned(SecretsCondemnReason::OffBranch) => ("condemned", Some("off_branch")),
            Self::Condemned(SecretsCondemnReason::ClosedIncarnation) =>
                ("condemned", Some("closed_incarnation")),
            Self::Rejected(SecretsRejectReason::OwnerReferenceInvalid) =>
                ("rejected", Some("invalid_owner")),
        }
    }
}

/// The resolved facts for one evaluable `StreamKeyWrap`, all read from ONE fold snapshot.
#[derive(Clone)]
pub(in crate::account) struct SecretsAcceptanceInput<F>
where
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
{
    /// The owning account — the account whose secrets log carries this wrap.
    pub(in crate::account) account_id: AccountId,
    pub(in crate::account) entry_hash: EntryHash,
    pub(in crate::account) seq: u64,
    /// The header's `authority_ref` — the cited owner incarnation id (null ⇒ reject).
    pub(in crate::account) authority_ref: Option<EntryHash>,
    /// The cited owner incarnation resolved via `owner_secrets_authority` (the two secrets
    /// boundaries).
    pub(in crate::account) owner_authority: AuthorityQuery<OwnerChainAuthority>,
    /// The account's ownership of the wrap's stream (`stream_owner_effective`).
    /// Required for key wraps; repository-incarnation artifacts carry no content stream.
    pub(in crate::account) ownership: Option<AuthorityQuery<EntryHash>>,
    /// The header's asserted control-fold length (`auth_len`) — the value `freshness` must have
    /// been computed against (provenance).
    pub(in crate::account) asserted_auth_len: u64,
    pub(in crate::account) dense_predecessor_reachable: bool,
    pub(in crate::account) branch_selected: bool,
    pub(in crate::account) freshness: CitedFreshness,
    pub(in crate::account) contested: bool,
    pub(in crate::account) ancestry: F,
}

/// The full predicate: authority + registers, then the dense predecessor, then branch selection,
/// then freshness LAST (§13/§15).
pub(in crate::account) fn evaluate_secrets_acceptance<F>(
    input: &SecretsAcceptanceInput<F>,
) -> Result<SecretsAcceptance, SecretsAcceptanceInputError>
where
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
{
    if let Some(verdict) = authority_verdict(input)? {
        return Ok(verdict);
    }
    if !input.dense_predecessor_reachable {
        return Ok(SecretsAcceptance::Parked(SecretsParkReason::MissingPredecessor));
    }
    if !input.branch_selected {
        return Ok(SecretsAcceptance::Forked);
    }
    // Freshness last (§13): a wrap citing control ops we have not folded parks for refetch, but
    // only once every verdict the current fold CAN decide — condemnation, fork — has been ruled
    // out.
    if input.freshness.state == AuthorityFreshness::Ahead {
        return Ok(SecretsAcceptance::Parked(SecretsParkReason::AuthLenAhead));
    }
    Ok(SecretsAcceptance::Accepted)
}

/// Everything §15 decides BEFORE the secrets DAG has a say: the owner-only citation, the account's
/// stream ownership, the two secrets-boundary registers, and the contested hold. `None` means the
/// wrap is authorized — it is then eligible to contest a slot in branch selection, and the caller
/// finishes the verdict with [`evaluate_secrets_acceptance`].
///
/// This is the phase split the refold needs: a wrap that is condemned or rejected must NOT compete
/// for its dense seq slot (a small-hash wrap mined beyond a cut would otherwise fork an honest
/// sibling off the accepted branch), so eligibility is decided before selection runs — and
/// selection's output is itself an input to the full predicate.
pub(in crate::account) fn authority_verdict<F>(
    input: &SecretsAcceptanceInput<F>,
) -> Result<Option<SecretsAcceptance>, SecretsAcceptanceInputError>
where
    F: Fn(EntryHash, EntryHash) -> AncestryRelation,
{
    // Provenance first: a freshness verdict computed for another account, or against a different
    // asserted length than this header makes, decides nothing about THIS entry. Both components are
    // enforced (mirrors the content predicate) so a future refactor cannot silently pair the wrong
    // freshness — even though today's single account keeps them equal.
    if input.freshness.account_id != input.account_id
        || input.freshness.asserted_auth_len != input.asserted_auth_len
    {
        return Err(SecretsAcceptanceInputError::FreshnessProvenance);
    }
    let freshness = input.freshness.state;
    let rejected = |reason| Ok(Some(SecretsAcceptance::Rejected(reason)));
    let parked = |reason| Ok(Some(SecretsAcceptance::Parked(reason)));

    // A wrap with no owner citation can never be authorized (the owner-only gate, B3 / freeze #3):
    // decided by bytes we hold, so it rejects outright regardless of freshness.
    if input.authority_ref.is_none() {
        return rejected(SecretsRejectReason::OwnerReferenceInvalid);
    }

    // The owner-only gate: the wrap is admitted only under a live owner incarnation of the OWNING
    // account, and the incarnation's device is the wrap's signer (enforced by
    // `owner_secrets_authority` resolving the queried device — a WrongSubject means the signer
    // is not that owner). A writer-grant or member device therefore rejects here (B3).
    let owner = match input.owner_authority {
        AuthorityQuery::Effective(owner) => owner,
        AuthorityQuery::Unknown => return parked(SecretsParkReason::UnknownOwnerRef),
        AuthorityQuery::Invalid(reason) =>
            return Ok(Some(invalid_owner_citation(reason, freshness))),
    };

    // The stream must be OWNED by this account (a prior effective `StreamOwn{stream}`), or the
    // owner-only authorization has no basis. §14 makes the true owner unprovable-absent locally, so
    // a not-yet-folded ownership fact PARKS (recoverable) — never a rejection.
    if let Some(ownership) = input.ownership {
        match ownership {
            AuthorityQuery::Effective(_) => {},
            AuthorityQuery::Unknown => return parked(SecretsParkReason::UnknownStreamOwner),
            AuthorityQuery::Invalid(_) => return parked(SecretsParkReason::UnknownStreamOwner),
        }
    }

    // The two secrets boundaries (device register + owner-incarnation register) are the conjunction
    // that bounds the wrap's own `(account, log:1, device)` chain. A definite condemnation outranks
    // any park (an incomplete ancestry on one register can never mask a `beyond_cut` on the other).
    let boundaries = [owner.device_boundary, owner.incarnation_boundary];
    let boundary_decision =
        combine_boundaries(&boundaries, input.seq, input.entry_hash, &input.ancestry);
    if matches!(boundary_decision, Some(SecretsAcceptance::Condemned(_))) {
        return Ok(boundary_decision);
    }
    // §12: a contested account halts authority mutation, so a wrap it authorizes fails closed
    // (quota-bounded, reclassified on recovery). Checked after a definite condemnation but before a
    // boundary that could not be decided.
    if input.contested {
        return parked(SecretsParkReason::ContestedSubject);
    }
    // A boundary that could not be decided (a withheld watermark, an incomplete walk) parks here,
    // after the hold — it is the weakest verdict the registers can produce.
    Ok(boundary_decision)
}

/// Lower an `Invalid` owner citation into a verdict. `WrongSubject` is decided by bytes we already
/// hold (the incarnation names a different device than the signer), so it rejects outright;
/// `ReferencedEntryNotEffective` is a verdict of OUR fold, and a fold behind the author's may be
/// missing the `CutExtend` that re-blesses it (§11.4) — so while the account's control log is ahead
/// of us it parks for refetch (S-a #3) rather than hardening into a rejection we would walk back.
fn invalid_owner_citation(
    reason: AuthorityInvalidReason,
    freshness: AuthorityFreshness,
) -> SecretsAcceptance {
    match (reason, freshness) {
        (AuthorityInvalidReason::WrongSubject, _)
        | (
            AuthorityInvalidReason::ReferencedEntryNotEffective,
            AuthorityFreshness::CurrentOrBehind,
        ) => SecretsAcceptance::Rejected(SecretsRejectReason::OwnerReferenceInvalid),
        (AuthorityInvalidReason::ReferencedEntryNotEffective, AuthorityFreshness::Ahead) =>
            SecretsAcceptance::Parked(SecretsParkReason::AuthLenAhead),
    }
}

/// Combine the two secrets registers into one verdict with the account fold's frozen precedence: a
/// definite condemnation outranks any park, so incomplete ancestry on one register can never mask a
/// `beyond_cut` (which needs no ancestry at all) on the other; a withheld watermark outranks a
/// missing mid-chain link; `Open`/on-branch is clear. (Ported from `content::acceptance`.)
fn combine_boundaries<F>(
    boundaries: &[AuthorityBoundary],
    seq: u64,
    entry_hash: EntryHash,
    ancestry: &F,
) -> Option<SecretsAcceptance>
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
        1 => Some(SecretsAcceptance::Parked(SecretsParkReason::IncompleteCutAncestry)),
        2 => Some(SecretsAcceptance::Parked(SecretsParkReason::UnknownCutTarget)),
        3 => Some(SecretsAcceptance::Condemned(SecretsCondemnReason::BeyondCut)),
        4 => Some(SecretsAcceptance::Condemned(SecretsCondemnReason::OffBranch)),
        5 => Some(SecretsAcceptance::Condemned(SecretsCondemnReason::ClosedIncarnation)),
        _ => unreachable!("boundary ranks are closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTRY_HASH: EntryHash = [9; 32];
    const OWNER_ID: EntryHash = [7; 32];

    fn account() -> AccountId {
        AccountId::from_bytes([5; 32])
    }

    fn owner_chain(
        device_boundary: AuthorityBoundary,
        incarnation_boundary: AuthorityBoundary,
    ) -> AuthorityQuery<OwnerChainAuthority> {
        AuthorityQuery::Effective(OwnerChainAuthority {
            owner: super::super::super::OwnerAuthority {
                device_fingerprint: crate::op::DeviceFingerprint::from_bytes([2; 32]),
            },
            device_boundary,
            incarnation_boundary,
        })
    }

    fn base(
        owner_authority: AuthorityQuery<OwnerChainAuthority>,
        relation: AncestryRelation,
    ) -> SecretsAcceptanceInput<impl Fn(EntryHash, EntryHash) -> AncestryRelation> {
        SecretsAcceptanceInput {
            account_id: account(),
            entry_hash: ENTRY_HASH,
            seq: 0,
            authority_ref: Some(OWNER_ID),
            owner_authority,
            ownership: Some(AuthorityQuery::Effective([1; 32])),
            asserted_auth_len: 3,
            dense_predecessor_reachable: true,
            branch_selected: true,
            freshness: CitedFreshness {
                account_id: account(),
                asserted_auth_len: 3,
                state: AuthorityFreshness::CurrentOrBehind,
            },
            contested: false,
            ancestry: move |_, _| relation,
        }
    }

    #[test]
    fn an_owner_wrap_with_clear_authority_accepts() {
        let input = base(
            owner_chain(AuthorityBoundary::Open, AuthorityBoundary::Open),
            AncestryRelation::OnBranch,
        );
        assert_eq!(evaluate_secrets_acceptance(&input), Ok(SecretsAcceptance::Accepted));
    }

    #[test]
    fn a_null_authority_ref_rejects_and_a_non_owner_signer_rejects() {
        let mut null_ref = base(
            owner_chain(AuthorityBoundary::Open, AuthorityBoundary::Open),
            AncestryRelation::OnBranch,
        );
        null_ref.authority_ref = None;
        assert_eq!(
            evaluate_secrets_acceptance(&null_ref),
            Ok(SecretsAcceptance::Rejected(SecretsRejectReason::OwnerReferenceInvalid))
        );
        // A signer that is not the cited incarnation's device (WrongSubject) rejects regardless of
        // freshness — the owner-only gate (B3).
        let mut wrong_subject = base(
            AuthorityQuery::Invalid(AuthorityInvalidReason::WrongSubject),
            AncestryRelation::OnBranch,
        );
        wrong_subject.freshness.state = AuthorityFreshness::Ahead;
        assert_eq!(
            evaluate_secrets_acceptance(&wrong_subject),
            Ok(SecretsAcceptance::Rejected(SecretsRejectReason::OwnerReferenceInvalid))
        );
    }

    #[test]
    fn an_ineffective_incarnation_parks_while_behind_and_rejects_once_level() {
        let mut ahead = base(
            AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
            AncestryRelation::OnBranch,
        );
        ahead.freshness.state = AuthorityFreshness::Ahead;
        assert_eq!(
            evaluate_secrets_acceptance(&ahead),
            Ok(SecretsAcceptance::Parked(SecretsParkReason::AuthLenAhead))
        );
        let level = base(
            AuthorityQuery::Invalid(AuthorityInvalidReason::ReferencedEntryNotEffective),
            AncestryRelation::OnBranch,
        );
        assert_eq!(
            evaluate_secrets_acceptance(&level),
            Ok(SecretsAcceptance::Rejected(SecretsRejectReason::OwnerReferenceInvalid))
        );
    }

    #[test]
    fn a_wrap_before_its_stream_own_parks_recoverably() {
        let mut input = base(
            owner_chain(AuthorityBoundary::Open, AuthorityBoundary::Open),
            AncestryRelation::OnBranch,
        );
        input.ownership = Some(AuthorityQuery::Unknown);
        assert_eq!(
            evaluate_secrets_acceptance(&input),
            Ok(SecretsAcceptance::Parked(SecretsParkReason::UnknownStreamOwner))
        );
    }

    #[test]
    fn an_unheld_incarnation_parks_unknown_owner_ref() {
        let input = base(AuthorityQuery::Unknown, AncestryRelation::OnBranch);
        assert_eq!(
            evaluate_secrets_acceptance(&input),
            Ok(SecretsAcceptance::Parked(SecretsParkReason::UnknownOwnerRef))
        );
    }

    #[test]
    fn a_beyond_cut_wrap_is_condemned_without_consulting_ancestry() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let input = SecretsAcceptanceInput {
            account_id: account(),
            entry_hash: ENTRY_HASH,
            seq: 5,
            authority_ref: Some(OWNER_ID),
            owner_authority: owner_chain(
                AuthorityBoundary::Cut { seq: 2, hash: [1; 32] },
                AuthorityBoundary::Open,
            ),
            ownership: Some(AuthorityQuery::Effective([1; 32])),
            asserted_auth_len: 3,
            dense_predecessor_reachable: true,
            branch_selected: true,
            freshness: CitedFreshness {
                account_id: account(),
                asserted_auth_len: 3,
                state: AuthorityFreshness::CurrentOrBehind,
            },
            contested: false,
            ancestry: |_, _| {
                calls.set(calls.get() + 1);
                AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget)
            },
        };
        assert_eq!(
            evaluate_secrets_acceptance(&input),
            Ok(SecretsAcceptance::Condemned(SecretsCondemnReason::BeyondCut))
        );
        assert_eq!(calls.get(), 0, "beyond-cut is seq-only, never consults ancestry");
    }

    #[test]
    fn an_off_branch_wrap_is_condemned_and_a_within_cut_on_branch_accepts() {
        let off = base(
            owner_chain(AuthorityBoundary::Cut { seq: 5, hash: [1; 32] }, AuthorityBoundary::Open),
            AncestryRelation::OffBranch,
        );
        assert_eq!(
            evaluate_secrets_acceptance(&off),
            Ok(SecretsAcceptance::Condemned(SecretsCondemnReason::OffBranch))
        );
        let on = base(
            owner_chain(AuthorityBoundary::Cut { seq: 5, hash: [1; 32] }, AuthorityBoundary::Open),
            AncestryRelation::OnBranch,
        );
        assert_eq!(evaluate_secrets_acceptance(&on), Ok(SecretsAcceptance::Accepted));
    }

    #[test]
    fn a_definite_condemnation_outranks_a_contested_hold_and_an_incomplete_boundary() {
        // Closed incarnation + contested: the condemnation wins (a hold cannot mask a definite
        // condemnation).
        let input = SecretsAcceptanceInput {
            contested: true,
            ..base(
                owner_chain(AuthorityBoundary::Open, AuthorityBoundary::Closed),
                AncestryRelation::OnBranch,
            )
        };
        assert_eq!(
            evaluate_secrets_acceptance(&input),
            Ok(SecretsAcceptance::Condemned(SecretsCondemnReason::ClosedIncarnation))
        );
        // Contested outranks an undecided boundary park.
        let held = SecretsAcceptanceInput {
            contested: true,
            ..base(
                owner_chain(
                    AuthorityBoundary::Cut { seq: 5, hash: [1; 32] },
                    AuthorityBoundary::Open,
                ),
                AncestryRelation::Unknown(UnknownAncestry::UnknownCutTarget),
            )
        };
        assert_eq!(
            evaluate_secrets_acceptance(&held),
            Ok(SecretsAcceptance::Parked(SecretsParkReason::ContestedSubject))
        );
    }

    #[test]
    fn freshness_provenance_is_a_precondition_and_freshness_runs_last() {
        let mut wrong_account = base(
            owner_chain(AuthorityBoundary::Open, AuthorityBoundary::Open),
            AncestryRelation::OnBranch,
        );
        wrong_account.freshness.account_id = AccountId::from_bytes([0xaa; 32]);
        assert_eq!(
            evaluate_secrets_acceptance(&wrong_account),
            Err(SecretsAcceptanceInputError::FreshnessProvenance)
        );
        // Same account, but the freshness was computed against a DIFFERENT asserted length than the
        // header makes — provenance fails on that component too (mirrors the content predicate).
        let mut wrong_len = base(
            owner_chain(AuthorityBoundary::Open, AuthorityBoundary::Open),
            AncestryRelation::OnBranch,
        );
        wrong_len.freshness.asserted_auth_len = wrong_len.asserted_auth_len + 1;
        assert_eq!(
            evaluate_secrets_acceptance(&wrong_len),
            Err(SecretsAcceptanceInputError::FreshnessProvenance)
        );
        // A fork is decided by the DAG, so an ahead counter cannot mask it (freshness runs after
        // branch selection).
        let mut forked = base(
            owner_chain(AuthorityBoundary::Open, AuthorityBoundary::Open),
            AncestryRelation::OnBranch,
        );
        forked.branch_selected = false;
        forked.freshness.state = AuthorityFreshness::Ahead;
        assert_eq!(evaluate_secrets_acceptance(&forked), Ok(SecretsAcceptance::Forked));
        // Selected + ahead ⇒ park auth_len_ahead (freshness last).
        let mut ahead = base(
            owner_chain(AuthorityBoundary::Open, AuthorityBoundary::Open),
            AncestryRelation::OnBranch,
        );
        ahead.freshness.state = AuthorityFreshness::Ahead;
        assert_eq!(
            evaluate_secrets_acceptance(&ahead),
            Ok(SecretsAcceptance::Parked(SecretsParkReason::AuthLenAhead))
        );
    }

    #[test]
    fn status_pairs_cover_every_frozen_state() {
        for (verdict, pair) in [
            (SecretsAcceptance::Accepted, ("accepted", None)),
            (SecretsAcceptance::Forked, ("forked", None)),
            (
                SecretsAcceptance::Parked(SecretsParkReason::MissingPredecessor),
                ("parked", Some("missing_predecessor")),
            ),
            (
                SecretsAcceptance::Parked(SecretsParkReason::UnknownOwnerRef),
                ("parked", Some("unknown_owner_ref")),
            ),
            (
                SecretsAcceptance::Parked(SecretsParkReason::UnknownStreamOwner),
                ("parked", Some("unknown_account")),
            ),
            (
                SecretsAcceptance::Parked(SecretsParkReason::AuthLenAhead),
                ("parked", Some("auth_len_ahead")),
            ),
            (
                SecretsAcceptance::Parked(SecretsParkReason::IncompleteCutAncestry),
                ("parked", Some("incomplete_cut_ancestry")),
            ),
            (
                SecretsAcceptance::Parked(SecretsParkReason::UnknownCutTarget),
                ("parked", Some("unknown_cut_target")),
            ),
            (
                SecretsAcceptance::Parked(SecretsParkReason::ContestedSubject),
                ("parked", Some("contested_subject")),
            ),
            (
                SecretsAcceptance::Condemned(SecretsCondemnReason::BeyondCut),
                ("condemned", Some("beyond_cut")),
            ),
            (
                SecretsAcceptance::Condemned(SecretsCondemnReason::OffBranch),
                ("condemned", Some("off_branch")),
            ),
            (
                SecretsAcceptance::Condemned(SecretsCondemnReason::ClosedIncarnation),
                ("condemned", Some("closed_incarnation")),
            ),
            (
                SecretsAcceptance::Rejected(SecretsRejectReason::OwnerReferenceInvalid),
                ("rejected", Some("invalid_owner")),
            ),
        ] {
            assert_eq!(verdict.as_db_pair(), pair);
        }
    }
}
