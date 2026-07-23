# Issue distillation — typed decision records

Distillation turns every **closed issue and merged pull request** **plus its fixing-commit diff**
into one **typed decision record**: what was broken or asked for, the underlying cause, the
approach that *landed* (and the alternatives that didn't), and what actually happened — anchored to
the symbols and files the change touched, and validated mechanically at every step. Records are a
**regenerable derived layer**: they are surfaced where agents already look (retrieval payloads and
drive-by context on the anchored symbols), so the reasoning behind a piece of code arrives in one
call instead of a git-archaeology session.

Nothing shipping elsewhere combines these: existing tools mine the *discussion* without the diff, or
the *diff* without the discussion; none anchor the extracted knowledge to validated symbols, track
`decision → outcome`, or backfill years of closed history on day one.

The model half is **out-of-process, opt-in, and gated by a deterministic layer** — the same posture
as the [dream](config/dream.md) passes. The deterministic extraction runs model-free; the LLM pass
only runs when you enable `[llm.distill]` and drain the queue.

## The record

Each eligible thread yields one record; when a merged PR closes an issue the two **coalesce into a
single issue-owned record** (the PR is exposed as a partner, not its own record). Records are stored
in `papertrail_distill` (with evidence and anchors in side tables) and surfaced to consumers as a
flat `DistilledRecord`. Its content fields:

| field | meaning |
|---|---|
| `root_issue` | what was broken or requested, from the reporter's point of view (null if none stated) |
| `root_cause` / `root_cause_class` | the underlying technical cause + a short failure-class label — **honestly null** when the thread establishes no failure (a feature, not a bug) |
| `decision_chosen` | the core approach that **landed** — not a reviewer's rejected proposal |
| `rejected_alternatives[]` | alternatives explicitly considered and not taken (`{alternative, reason}`) |
| `outcome_status` | the *effective* status: `landed` \| `descoped` \| `superseded` \| `reverted` \| `unclear` (the model's raw call is kept as `outcome_status_model`) |
| `outcome_summary` | what actually happened — **measured results stated as results**, projections never asserted as delivered |
| `fixing_commits` | the commit(s) that resolved the thread |

During the **drain**, the model **cites** every claim to the verbatim `[U#]` thread units that
establish it and **selects** anchors from the changed files' candidate symbols (`extract` only builds
the candidate list, model-free); both are validated (a fabricated citation or an off-list anchor is
rejected) and stored in side tables. The surfaced record does **not** carry the raw citations or
anchor tuples — it exposes the **provenance facets** derived from them (not a fused confidence
score): `fix_edge_source` (`provider` \| `text` \| `none`), `thread_shape` (`investigation` \|
`review_stream` \| `thin`), `anchors_qualified_count`, `outcome_claim_verified`, and
`decision_provenance_verified`. (`epistemic_status_decision` / `epistemic_status_outcome` are reserved
— null in v1.) The *effective* `outcome_status` is computed
in the read layer from status-floor inputs (a revert override, a closing-keyword floor, the presence
of a fix edge) layered over the model's raw status — the model never sees the final label.

## How it works

Two phases, split at the model boundary:

### 1. Extract (deterministic, model-free)

`rag-rat distill extract` walks the papertrail mirror and, for every eligible thread — a closed
issue or a **merged** pull request (a closed-unmerged PR is not yet distilled; see #743) — builds:

- a **skeleton record** on the natural key `(repo_id, tracker, project, item_kind, item_key)`;
- **fixing-commit edges** — provider-attested closing references + merge commits first, a
  text-derived fallback tier where the provider has no native edges;
- **coalesced edges** — an issue and its fixing PR answer as one work-unit (the issue side);
- **anchor candidates** — the `(logical_symbol_id, file, name, kind)` symbols in the changed files,
  offered to the model as an index-selection list (selecting by index removes name-normalization);
- **immutable input snapshots** — the exact thread units, the cross-referenced item titles + opening
  paragraphs, and every coalesced partner are folded into the record's regeneration identity, so the
  drain never reads mutable mirror state. The capped fix diff (per-file unified patches for the paths
  that yielded a symbol anchor) is snapshotted too, but is **best-effort and outside** that identity —
  a shallow or bare-index run that had no diff can fill it in later without re-distilling the record;
- the **work queue** — one row per thread awaiting the model pass.

### 2. Drain (the model pass)

`rag-rat distill drain` runs extraction, then drains the queue through the configured chat model.
Each thread hydrates its snapshot into a rendered prompt + a guided-JSON schema (both hashed into
`model_input_hash`), then runs the **output ladder** — at most two model calls, with an acceptance
rung at each parse:

```
guided decode ──▶ strict serde + validation ──▶ accept (serde)
      │ fail
      ▼
unguided retry ──▶ strict serde + validation ──▶ accept (unguided)
      │ fail
      ▼
one tolerant fence-strip ──▶ accept (tolerant)  │  else fail → re-queue (raw reply persisted)
```

Before validation, the model's output is **normalized** (content-preserving, post-generation only —
it never touches the prompt or the regeneration hash): a reject-worthy inline-code span (a multi-word
backtick phrase, an empty span, an unclosed backtick) is demoted to plain prose while genuine
single-identifier spans are kept, and duplicate citation / anchor ids are deduped order-preserving.
This rescues records the model would otherwise fail on *formatting alone*, with their content intact
— the run stats record a `repaired_serde` counter so the genuinely-clean guided rate stays
`rung_serde − repaired_serde`. Evidence must cite real units; anchors must be visible candidates;
outcome numbers must not assert a projection as a delivered result.

Every completed record is **durable immediately** — a died box loses nothing, and re-running resumes
the queue. The pass is idempotent: a record whose `model_input_hash` matches the current input is not
re-distilled.

## Running it

```bash
# Deterministic substrate only (no model, no cost): skeleton records, fixing edges,
# coalesced pairs, anchor candidates, work queue.
rag-rat distill extract

# Run extraction, then drain up to N queued threads through the model.
rag-rat distill drain --limit 100
```

The drain requires `[llm.distill] enabled = true` and a serving config — see
[`[llm.distill]` configuration](config/distill.md). A zero-work run returns before any capability
check or provisioning, so scheduling it generously costs nothing when the queue is empty.

**Long runs must batch.** On an ephemeral GPU box the provider caps the box's lifetime (~30 min on
the Modal cookbook), so a single drain over a large corpus cannot finish on one box. Size `--limit`
to one box's serving window and loop `distill drain` until the queue empties — the queue is
resumable and records are durable per thread, and a clean teardown warms the model cache so later
boxes boot fast.

## Where records surface

Records ride the surfaces agents already use — there is **no new vector lane** (measurement showed
the record payload beats every no-LLM baseline while embeddings don't beat the existing FTS on
ranking):

- **Retrieval payload** — a `rationale_search` / `papertrail_issue_search` hit that has a distilled
  record returns the record (root issue/cause, decision incl. rejected alternatives, outcome status +
  fixing commits, provenance facets) instead of only raw item/comment matches. Coalesced issue↔PR
  pairs answer as one result.
- **Drive-by attachment** on the symbol surfaces — `semantic_search`, `symbol_lookup`,
  `impact_surface`, and `read_chunk` attach the records for the symbols they touch. This is
  **facet-gated** (requires a provider fix edge *and* a **model-selected** qualified symbol anchor — a
  drain can validly select none even when candidates exist, in which case the record surfaces on no
  symbol), hard-capped (≤2 records per symbol, coalesced, newest-distilled first by `distilled_at_ms`),
  and clearly labeled as distilled + unreviewed
  with the thread + commit as provenance, so a hot symbol's payload never blows the budget.

## Regeneration & durability

Records are a derived layer keyed by `pipeline_version` (extraction shape) and `prompt_version`
(prompt/schema). Bumping either re-queues affected records with model state cleared; a record is
otherwise recomputed only when its input snapshot changes. Because the drain keys on
`model_input_hash`, an unchanged thread is never re-distilled — so a full backfill is a one-time cost
and incremental threads are sub-cent. The fix diff is the one exception: it is deliberately outside
the identity hash, so a diff that only becomes available on a later run (a bare or shallow index had
none) is filled in **without** re-distilling the record.

## Configuration

See [`[llm.distill]` configuration](config/distill.md) for the serving config (connect endpoint vs.
ephemeral cookbook box), the recommended model, and the per-request timeout.
