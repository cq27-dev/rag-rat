# Control-log v2: bounded revocation credit

Status: implementation plan for #1311. V2 is not implemented or enabled. Existing accounts remain
on v1. The compatibility decision is an explicit account upgrade that preserves signed v1 history;
upgraded peers are required to apply subsequent revocations.

## Checkpoint verification prerequisite

`account/checkpoint.rs` implements proposal creation and verification only. It does not install a
pin, change an account's authority projection, admit control v2, or implement bounded v2 credit.
The public verifier requires an independently obtained `TrustedCheckpointPin` containing the
account, certificate digest, and required control version 2. A certificate authorized at a
historical view is insufficient without that external digest. All existing peers, enrollment,
and recovery must eventually receive the same permanent pin; receiving a certificate from a peer
does not establish that trust.

The standalone certificate is deliberately outside the control chain. Its canonical CBOR body is:

```text
["rag-rat/control-checkpoint/1", account_id, 2, genesis_hash,
 evidence_digest, legacy_projection_hash, signer_ed25519_key, signer_owner_incarnation]
```

Its transport is `["rag-rat/control-checkpoint-signed/1", body_bytes, ed25519_signature]`, where
the signature covers the exact body bytes and the externally pinned digest is SHA-256 of the
complete canonical transport. Certificates are limited to 1024 bytes. The evidence commitment is
SHA-256 of `["rag-rat/control-checkpoint-evidence/1", sorted_entry_hashes]`; each entry hash already
commits to its signed account header and payload. Every supplied signature is independently
verified. Duplicate entry hashes reject, including alternate signatures of one body.

The bundle contains all declared authenticated v1 account candidates, including losing forks and
non-control ancestry evidence. Proposal creation reads every held authenticated candidate in the
caller's transaction, not just accepted rows; unauthenticated pre-verify rows are not evidence.
The protocol limits are 4096 entries and 16 MiB of total signed evidence, separate from the
certificate limit. Requests beyond these bounds fail without truncation. Missing/different
evidence commitments return `MissingEvidence`, never a partially verified projection. These
bounds cover this single legacy proof; future pre-cut view DAGs need their own aggregate limits.

Verification uses the same v1 fold, coherent-branch selection, and authority closure as storage.
It requires a live account, the exact genesis and projection commitment, and an open owner
incarnation bound to the certificate signer. Exact continuation branches must reach their seq-zero
origins with matching account/log/device coordinates and contiguous signed predecessor links.
The opaque `VerifiedCheckpoint` exposes accepted entries, branch/authority-closure losers, all
nonaccepted control entries, and continuation heads separately. These sets are diagnostic facts,
not a sufficient v2 activation policy: nonaccepted legacy entries can still have register effects.

Next prerequisites are immutable pin persistence outside derived projections, explicit unsupported
authority gating, and trusted pin transfer through enrollment/recovery. Activation additionally
requires frozen legacy outcomes, readiness and register-contributor admission: merely restricting
v1 candidates or credit hashes lets later v2 traffic awaken parked v1 cuts. V2 authoring must extend
the checkpoint's retained continuation tip and its admitted v2 descendants, never a held v1 suffix
outside the boundary. Omitted legitimate v1 work must be reconciled before approval or reauthored
under v2. No existing snapshot, refold, losing-fork elimination, or later signer revocation may
replace the pin or reopen a historical branch.

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

The selected exact-credit design, not yet implemented, signs a content-addressed `PreCutViewRef`
bound to the checkpoint and semantics version, plus the canonical branch frontier. The view
commits to its complete declared candidate input, including losing forks. `E(V)` is the effective
accepted set from policy-aware production fold, branch selection, and authority closure over that
complete input. V2 `auth_len` equals this accepted-authority count; it must not be substituted with
the legacy fold's provisional effective count.

Eligible losses are members of `E(V)` on the signed frontier, in the cut-local direct/transitive
invalidation cone, that are no longer effective in the final policy-aware fold. Deduplicate them
and exclude the revocation itself. A descendant mint seeds credit only when it is itself in `E(V)`
and on the frontier. Existing signer/incarnation checks and concurrent-vouch ceilings remain.
This is exact for the declared view, not a claim of complete global account knowledge. Explicit
credit-hash lists are deferred: a verifier would still need to replay the view to validate them.

Evaluate the content-addressed dependency closure iteratively and topologically, memoizing each
unique `(checkpoint, semantics_version, view_digest)` once. Do not recursively fold once per cut
without a shared cache. Protocol limits must bound total reachable unique views, evidence entries,
evidence bytes, dependency depth, and frontier size. Missing dependencies park before registers or
vouches; protocol-limit violations reject. Local scheduling exhaustion stays pending and never
produces partial credit. Honest authoring references only already closed earlier views.

## Explicit account transition

The selected transition is a permanent externally trusted pin to one standalone signed checkpoint.
A binary upgrade or configuration flag cannot upgrade an account. Account/genesis identity and
signed v1 bytes remain unchanged. The checkpoint fixes the exact declared legacy evidence and
projection; every existing peer, enrolling peer, and recovered peer must receive the same trusted
digest. A conflicting pin refuses rather than replacing the installed one. Unsolicited alternate
certificates cannot poison an installed pin.

The certificate signer must be an open owner in the complete certified v1 view. That historical
authorization validates the certificate but does not establish trust in it: the expected digest
comes from explicit operator approval, a trusted transferred ticket, or recovery input. This
external approval prevents an arbitrary formerly authorized owner from selecting an older view.
Omitted v1 work must be reconciled before approval or reauthored under v2; v1 outside the certified
boundary never extends upgraded authority.

The pin is authoritative external trust input, persisted outside derived roster/projection tables.
It is not a cache inferred from historical signatures or a snapshot. Derived proof/projection data
may be recomputed, but full replay must receive and retain the pin. Later revocation of the signer,
purge of derived state, and branch-elimination passes cannot erase it or downgrade the account.
Independent conflicting proposals require operator reconciliation before a common digest is
approved; arrival order never chooses one.

Activation will use full-history replay with frozen legacy policy, not a new state-seeded fold.
Preserve the exact historical winning branches and permanently excluded fork/authority-closure
losers, even after a v2 cut removes a historical winner. Freeze legacy outcomes/readiness,
register-contributor admission, and freshness credit so additional v2 volume cannot activate a
previously parked legacy cut. V2 may revoke historical authority, but cannot select a different
historical branch. Plain `fold(v1 + v2)` is insufficient.

Incomplete proofs must be observable as pending evidence; an installed pin requiring unsupported
control semantics must block operational authority explicitly. Enrollment must bind the expected
pin through ticket, request, redemption, cached replay, and receipt verification. Pin, validated
history, policy-aware enrollment acceptance, and account adoption must commit atomically. Recovery
must export/import the pin with its certificate and full evidence. V130 implements immutable pin and complete certificate/evidence retention outside derived
authority, with snapshot-local policy queries and recovery export that re-verifies the proof.
`pin_checkpoint_in_tx` consumes an externally expected pin and a `VerifiedCheckpoint` in the
caller's IMMEDIATE transaction. Identical installation is idempotent; conflicting pins refuse.
Refold suppresses operational authority while retaining signed evidence and the permanent pin.
Roster, content, table-sync, enrollment and authoring paths return a typed
`UnsupportedAccountControlVersion` under an installed pin, including sessions authenticated
before installation. Unpinned v1 accounts continue using v1 semantics.

This is a fail-closed persistence prerequisite, not v2 activation. There is no CLI pin-install
or upgrade command. Trusted ticket transfer, versioned enrollment and atomic recovery adoption
still require the complete policy-aware evaluator. A backup must retain the pin tables and proof;
restoring older unpinned history requires supplying the trusted pin before operational use.
Ordinary peer traffic never installs or replaces a pin.

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
2. Implement the selected transition and exact-credit semantics above, with pure-fold tests before
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
