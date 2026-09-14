# Control-log v2: bounded revocation credit

Status: implementation plan for #1311. V2 is not implemented or enabled. Existing accounts remain
on v1. The compatibility decision is an explicit account upgrade that preserves signed v1 history;
upgraded peers are required to apply subsequent revocations.

## Why the victim's head is insufficient

`revocation_credit` counts entries condemned by a revocation's registers and stale operations
signed under owner incarnations minted by those entries, transitively. A head on the directly
revoked device bounds only the first group. A descendant can append while that head stays fixed.

The characterization tests in `account/fold.rs` cover both `DeviceRemove` and `OwnerDemote`:

- `v1_descendant_entries_can_credit_a_cut_without_advancing_the_victim_chain`: a cut citing four
  operations parks in a three-operation view. A descendant appends, the direct victim's head stays
  fixed, and the cut becomes effective. The descendant's append then folds stale.
- `v1_descendant_credit_can_activate_an_ahead_cuts_concurrent_vouch`: the same activation leaves
  the cut available to vouch for another owner's operation at the cut's citation.

These pin legacy behavior, not desired v2 behavior. V1 replay must remain identifiable as v1;
changing its credit formula retroactively could change existing authority projections.

## Signed bounds for v2 revocations

Each v2 revocation needs a signed credit frontier covering every control chain from which it may
claim credit, including descendant chains. An omitted chain contributes zero. The frontier is
separate from `control_cut`: the cut selects what remains valid, while the frontier bounds what
was observed before revoking it.

The proposed representation is a canonical, sorted, unique array of
`[device_fingerprint, seq, entry_hash]` tuples. The enclosing account and control-log identifier
supply the other coordinates. Both direct and transitive credit must require membership in the
named head's exact predecessor branch. A sequence comparison alone does not exclude a fork.

Required rules:

1. Validate each held head against its account, log, device and sequence. A mismatch rejects the
   whole revocation. Missing heads or necessary predecessor links park it without installing
   registers or supplying concurrent credit.
2. Apply the frontier predicate in both credit loops. A mint outside the frontier must not seed
   descendant credit. A later descendant, including a newly minted key absent from the frontier,
   must never expand the credited set.
3. Retain the current cut-local scope, signer/incarnation checks, self-credit exclusions and
   owner-signed citation ceiling. Frontiers restrict eligibility; they do not authorize entries.
4. Canonicalize and bound the frontier before signing. Enforce the complete account envelope's
   64 KiB limit. Reject an oversized request rather than truncate its frontier; the supported
   maximum and an operational path for larger revocations need explicit wire/API tests.
5. Collect the frontier from the same transactional view as the revocation's `auth_len`. Do not
   infer it from the receiving peer's tail or the final post-revocation projection.

A frontier bounds later traffic, but is not by itself proof of the exact effective pre-cut set:
its branch can contain entries that were already ineffective. Before calling the resulting credit
*exact*, specify whether v2 replays the signed pre-cut view or signs an explicit set of creditable
entry hashes. The latter has a larger payload cost. Both require missing-evidence and size-limit
semantics. The watermark-only proposal does not settle this distinction.

## Explicit account transition

A process configuration flag or a new binary must not silently upgrade an account. An authorized,
signed account transition must identify the legacy history boundary and the new required control
version. Existing signed payloads, hashes, genesis/account identity and v1 decoding stay intact.

The transition needs an account-wide boundary, not merely monotonic versions on each author's
chain. Otherwise another device can append a v1 revocation after the upgrade and bypass signed
credit bounds. Legacy entries within the certified boundary must remain replayable; entries
outside it must not silently extend the v1 authority view. A sequence-only boundary again cannot
identify the retained branch.

Before freezing transition bytes, define and test:

- **Authorization:** which owner incarnation can authorize the transition, against which complete
  historical view, and why a previously revoked key cannot certify an older view to regain power.
- **Concurrency:** how independently authored upgrades with different frontiers converge, and what
  happens to v1 work authored concurrently but delivered after the transition. Arrival order or a
  local database flag cannot decide this.
- **Revocation and forks:** how later revocation of the upgrade signer or a conflicting branch
  affects the upgrade. Recomputing owner authority must not silently downgrade the account.
- **Incomplete delivery:** how the transition parks while its history is missing and how peers
  distinguish an unsupported account version from a successfully current authority projection.
- **Replay:** how a fresh database and an existing one derive the same version and boundary.
  Persisted state is a cache of the signed transition, not a local source of authority.

These are open protocol details, not implemented guarantees. In particular, accepting any
certificate that was authorized at an arbitrary historical frontier would let a formerly valid
owner present a competing transition after revocation. A rollout must resolve that before v2
entries are admitted.

## Wire and persistence integration

V1 revocations have five-field payload arrays. `v1_revocations_reject_an_added_credit_frontier` in
`account/ops.rs` pins rejection of a canonical sixth field for both revocation types. Unknown
control entries also quarantine the rest of their author's accepted chain; adding a tag alone
is not a compatible rollout.

Use control-specific version dispatch. `fold::SUPPORTED_OP_VERSION` is also used by secrets,
annex and snapshot code; globally changing it would widen this change beyond the control log.
Keep those formats unchanged unless a separately demonstrated dependency requires a format bump.

The implementation must cover these paths together:

| Path | Required behavior |
|---|---|
| `account/ops.rs`, envelope validation | Preserve v1 bytes; decode v2 only through its versioned grammar. |
| `account/fold.rs`, `account/candidate.rs` | Derive upgrade policy; gate candidates and registers before side effects; bound both credit paths. |
| `account/authoring.rs` | Explicit upgrade operation; derive version, citation and frontier atomically for later operations. |
| `account/storage.rs` | Version-aware structural validation, key discovery, promotion, full replay and projection writes. |
| `account/snapshot/` and snapshot storage | Validate transition policy from signed evidence; no snapshot may erase or bypass it. |
| CLI/core sync APIs | Expose the explicit account action and required peer upgrade; report unsupported versions clearly. |
| Enrollment/content authorization | Read the same upgraded roster and authority view, including after restart. |

## Delivery and validation

1. Pin the v1 gap and decoder boundary (the tests accompanying this plan).
2. Resolve the transition and exact-credit semantics above; add pure-fold transition tests before
   enabling v2 decoding in production ingestion.
3. Implement versioned wire, signed bounds and fold behavior together. Test direct and multi-level
   descendants, demotion, self-cuts, wrong coordinates, equal-sequence forks, absent chains, missing
   heads/links, duplicate/oversized frontiers, concurrent revocations and vouch ceilings.
4. Add transactional authoring, storage replay and snapshot coverage: interrupted upgrade rollback,
   retry without duplicate transitions, reopen/full refold equivalence, delayed v1 history, downgrade
   attempts, revocation of the upgrade signer, and concurrent conflicting upgrades. Exercise
   delivery permutations at each boundary.
5. Expose the account upgrade only once those checks pass. Existing v1 accounts stay v1 until the
   explicit action; the CLI must not imply old peers can enforce the new revocations.

#1311 remains open until the production path and these transition checks land.
