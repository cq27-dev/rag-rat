# Control v2 replay input prerequisite

`account/control_v2` is isolated from production ingestion and the account fold. It implements
payload grammar and bounded evidence planning, not signature verification, effective authority,
revocation credit, checkpoint installation or activation. Existing control dispatch remains v1.

## Payload

The existing signed account envelope carries control version 2 and the operation tag. Its payload
is canonical CBOR:

```text
["rag-rat/control-op/2", checkpoint_digest, pre_cut_view_digest,
 legacy_operation_bytes, credit_frontier_or_null]
```

Every operation cites the checkpoint and a pre-operation view. DeviceRemove and OwnerDemote carry
a frontier (an empty array is permitted); other known operations carry null. There is no v2
genesis. The inner operation bytes use the unchanged legacy field grammar. This does not give
legacy semantics to the enclosing operation: the future v2 executor must use the cited view and
the checkpoint policy. Unknown tags are outside this prerequisite's decoder; eventual production
dispatch must distinguish unsupported operations from malformed known ones.

Frontiers contain sorted unique `[device_fingerprint, seq, entry_hash]` tuples, at most 256. The
complete signed envelope retains the 64 KiB limit; a payload fitting that limit alone does not
prove its signed envelope fits. The envelope signer/decoder supplies the final size check.

## Exact view inputs

A view manifest is canonical CBOR:

```text
["rag-rat/control-view/2", checkpoint_digest, sorted_unique_v2_entry_hashes]
```

Its digest is SHA-256 of those bytes. The checkpoint's complete authenticated legacy evidence is
implicit in every view. The manifest lists exact additional v2 candidates, including losing forks,
not a caller-asserted effective set. Retained non-control history remains available in the
checkpoint proof. Additional non-control v2-era evidence is not yet represented by this grammar;
activation must settle that dependency before supporting cuts that need such evidence.

The planner takes the consuming operation's envelope, derives its root view citation and hash,
and excludes that consumer from the evidence and every reachable pre-cut view. It decodes bounded
signed envelopes structurally and derives dependencies from each declared candidate's pre-cut
view citation. It returns each reachable view exactly once in
dependency-first order, independent of the supplied evidence order. A future executor can memoize
one result per `(checkpoint, semantics version, view digest)` instead of recursively folding the
same shared view for each cut. A returned plan is not proof of valid signatures, branch ancestry,
authority or a correct citation count; none of its entries may authorize registers or credit yet.

The explicit proof bundle is bounded to 128 manifests, 32 dependency levels, 4096 additional
candidates, 16384 total manifest entry references and 16 MiB of manifest plus envelope bytes
(including the separate consuming operation).
All supplied objects count, including unused objects. Missing reachable manifests or entries
return a missing-evidence error without a partial plan. Duplicates, cross-checkpoint/account
objects and excessive aggregate work reject. These are proposed prerequisite limits; no production
authoring operation emits this grammar yet.

## Remaining execution contract

Activation requires a policy-aware executor that preserves legacy branch winners and losers,
freezes legacy register admission/readiness, and prevents v2 traffic from awakening parked v1
cuts. The checkpoint's current accepted/nonaccepted sets alone do not provide that policy.
The permanent checkpoint finalizes legacy register decisions and tombstones. V2 may close current
baseline authority and continuations, but cannot undo those historical register decisions, revive
excluded history, replace historical branches or replace the permanent pin. Capture legacy
contributor hashes and readiness exclusions from the final fold pass of the final storage branch
closure, together with the authority snapshot; accepted entries alone omit nonaccepted register
contributors and cannot reconstruct that baseline.

The executor must authenticate candidates, validate exact chain and authority dependencies, and
replay each declared view to derive its accepted-authority set. V2 citations and freshness must
use the same accepted-authority count. Credit then counts only eligible observed losses in the
signed frontier, with both direct and transitive paths bounded and the existing cut-local scope,
self-credit exclusions and concurrent-vouch ceilings retained. A missing or invalid view must
install no registers and supply no vouch. Merely planning these inputs implements none of those
authority guarantees and does not resolve #1311.
