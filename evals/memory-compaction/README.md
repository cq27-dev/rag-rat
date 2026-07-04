# memory-compaction eval

Regression gate for the `rag-rat dream` verify + compact passes. It answers two questions
about any change to the model or prompts those passes use:

1. **Compaction** — does the summarizer ever *invert* a memory's meaning when it compresses it?
2. **Verification** — does the verdict pass correctly label a memory as `current` / `diverged` /
   `unverifiable` against the current checkout?

Both are measured against a **fixed corpus** so results are comparable run-to-run. Re-run this
suite before shipping any change to the dream model or its prompts (see *When to re-run* below).

## What it is

`corpus/eval-corpus.json` is 35 memory notes with hand-authored probes:

- **30 real** rag-rat repo memories (`real_0`..`real_29`) — dense, prose-heavy invariants,
  decisions, bug patterns, and perf notes, 2.5–9 KB each.
- **5 synthetic** notes (`syn_0`..`syn_4`) engineered around the **negation-trap** family —
  conditional negations, exception clauses, asymmetric rules, "necessary but not sufficient".
  They describe deliberately fictional modules; nothing in them is real code.

Each note carries probes. A probe is a claim plus a `gold` label and a `weight`:

- **`gold: true`** — a *coverage* claim: the summary should preserve this fact. Judged `TRUE` =
  kept; judged `FALSE` = **critical false-assert** (the summary states the opposite of a true
  fact); judged `ABSENT` = dropped (a coverage miss, not a correctness failure).
- **`gold: false`** — a **negation trap**: it states the *inverse* of a source fact. The summary
  must NOT support it. Judged `FALSE` = trap correctly resisted; judged `TRUE` = **critical
  trap-flip** (the summary was compressed into asserting the inverse).
- **`weight: "core"`** marks the load-bearing claims; core coverage is tracked separately from
  overall coverage.

There are **79 probes, 39 of them negation traps**. The judge is a **non-candidate** model
(Qwen3-14B) that reads *only the summary* — no source, no outside knowledge — and answers
`TRUE` / `FALSE` / `ABSENT` per claim. Grading it blind to the source is what makes a trap-flip
detectable: the judge can only "know" the inverse fact if the summary itself asserts it.

`HHEM-2.1-open` (Vectara) is computed as a secondary consistency signal, but its absolute scores
run low on dense abstractive compression and its ranking disagrees with the probe metric — it
does **not** gate anything. The probe suite is the metric that matches the real risk.

## The two gates

| gate | applies when | bar |
|---|---|---|
| **compaction** | the compactor model or prompt changes (`dream/compact.rs`) | **zero trap-flips** across all 39 traps, and zero critical false-asserts |
| **verification** | the verdict model or prompt changes (`dream/verdict.rs`) | **≥ 70% verdict accuracy** on the 17-case labeled manifest |

The zero-trap-flip bar is absolute: a single inverted negation is a correctness regression, not a
coverage trade-off. Coverage (how many `gold: true` claims survive) is a *quality* number to
watch, not a hard gate — compression legitimately drops secondary facts.

### The verification manifest

`VERIFY_MANIFEST` in `harness/eval_app.py` is the labeled ground truth for the verdict gate —
17 cases with expected verdicts:

- **8 `current`** — merged-work memories whose claims are visible in the checkout
  (`real_16,17,20,21,22,26,27,29` at `/repo`).
- **5 `diverged / note_ahead`** — memories describing in-flight work not yet in the checkout
  (`real_0,1,3,9,14` at `/repo`). A real class this repo has whenever feature branches are open.
- **2 `unverifiable`** — the fictional synthetics whose named modules resolve nowhere
  (`syn_0,syn_2`).
- **2 `diverged / code_ahead`** — the **doctored-tree** cases (`real_22,27` at `/repo-drift`):
  the note is still accurate but a hypothetical future commit removed the guard it describes.
  These two depend on the drift tree — see below.

## Layout

```
evals/memory-compaction/
  README.md
  corpus/
    eval-corpus.json     35 notes + 79 probes  ← the crown jewels
    memories-full.json   full memory records (binding paths for the verify manifest)
    anchors.json         identifier-anchored code excerpts (anchor-context variants)
    drift-anchors.json   doctored anchors for the drift-detection variant
    verify-packs.json     mechanically-built evidence packs (verify-pack method), keyed id|root
  harness/
    eval_app.py          Modal app: candidates, judge, HHEM, the v2 variants, verify + drift
    score.py             folds judge verdicts + HHEM + format checks into the round-1 scoreboard
    score_v2.py          scores the v2 variants; carries the offline ref-leakage metric
    make-drift-tree.py   regenerates the doctored crates/ copy the manifest's two cases need
  drift-crates/          (generated, gitignored) doctored crates/ copy
  results/               (generated, gitignored) run outputs
```

## How to run

Everything runs on [Modal](https://modal.com) (candidates and judge on L40S, HHEM on T4; ~$2–3
for a full sweep). Entrypoints live in `harness/eval_app.py`. From `evals/memory-compaction/`:

```sh
# round 1 — candidate sweep + probe judge + HHEM, then score
modal run harness/eval_app.py::smoke           # 1 model x 3 items, eyeball output
modal run harness/eval_app.py::summarize_all   # 8 candidates x 35 memories, parallel
modal run harness/eval_app.py::judge_all       # Qwen3-14B probe judge over all summaries
modal run harness/eval_app.py::hhem_all        # HHEM-2.1 consistency scores
python3 harness/score.py                       # -> results/scoreboard.json + printed table

# round 2 — self-containment prompt variants (v2a/b/c) on the two leaders
modal run harness/eval_app.py::summarize_v2
modal run harness/eval_app.py::judge_v2
python3 harness/score_v2.py                    # -> results/scoreboard-v2.json (+ leakage %)

# round 3 — temporal / drift-flag variant (v2d) + the drift-injection probe
python3 harness/make-drift-tree.py             # regenerate drift-crates/ first (mounts /repo-drift)
modal run harness/eval_app.py::summarize_v2d
modal run harness/eval_app.py::judge_v2d
modal run harness/eval_app.py::drift_test

# round 4 — the verification gate
modal run harness/eval_app.py::verify_pack_test   # evidence-pack method (the live design)
python3 harness/make-drift-tree.py                # needed for the agentic comparison arm
modal run harness/eval_app.py::verify_test        # agentic grep/read method (the losing arm)
```

`verify_pack_test` reads the static `corpus/verify-packs.json` (evidence packs pre-built from the
current + doctored trees), so it does not mount the repo. `verify_test` and `drift_test` mount the
live checkout at `/repo` and the doctored copy at `/repo-drift`, so **run
`make-drift-tree.py` first** for those.

### The drift tree

`make-drift-tree.py` copies the repo's `crates/` to `drift-crates/` and applies exactly **two
surgical edits**, both in the write-time clone-check path:

- **`precompute.rs`** — removes the linked-overlay gate
  (`if self.active_scope_is_linked_overlay() { return Ok(None); }`) and its comment. This makes
  `real_22` diverge (`code_ahead`): the note says the postings fast path is disabled under a
  linked overlay; the doctored code no longer disables it.
- **`scoring.rs`** — removes the refined-class self-guard (`if class.refined { return; }`) and its
  comment. This makes `real_27` diverge (`code_ahead`): the note says a refined class is never
  re-dampened; the doctored code drops that guard.

It **fails loudly** if either anchor is missing — the surrounding code drifts over time, and a
missing anchor means the doctored cases must be re-derived by hand. The two `/repo-drift` cases in
the manifest depend on these edits; `corpus/verify-packs.json` encodes the same two edits as a
frozen snapshot for the pack method.

## Measured baseline (2026-07-04, Modal)

### Round 1 — candidate scoreboard (v1 prompt, all 8 models)

| model | trap-flip % ↓ | false-assert % ↓ | core-cov % ↑ | cov % ↑ | HHEM | format % | med. words |
|---|---|---|---|---|---|---|---|
| **Qwen3-4B-Instruct-2507** | **0.0** | **0.0** | **95.5** | 82.5 | 0.49 | 68.6 | 71 |
| phi-4 (14B) | 0.0 | 0.0 | 90.9 | 77.5 | 0.54 | 31.4 | 97 |
| gemma-3-12b-it | 0.0 | 0.0 | 90.9 | 72.5 | 0.48 | **100.0** | 67 |
| Qwen3-8B | 0.0 | 0.0 | 86.4 | 67.5 | 0.70 | 62.9 | 64 |
| Phi-4-mini-instruct (3.8B) | 0.0 | 0.0 | 86.4 | 70.0 | 0.57 | 14.3 | 86 |
| gemma-3-4b-it | 0.0 | 2.5 | 90.9 | 80.0 | 0.56 | 68.6 | 76 |
| SmolLM3-3B | 0.0 | 5.0 | 77.3 | 60.0 | 0.69 | 48.6 | 64 |
| Qwen3.5-4B | 7.7 | 0.0 | 90.9 | 75.0 | 0.51 | 94.3 | 80 |

Six of eight models had zero trap inversions across 39 traps. **Qwen3-4B-Instruct-2507** is the
pick: perfect polarity, best core coverage, 4B (local-servable), fast. Qwen3.5-4B (the newest) is
a regression — it confabulated constraints not in the source on two memories. Newer ≠ safer; pin
per task. The real failure mode is not raw negation but **bug-vs-fix tense**: notes shaped "the
bug WAS x, the fix is y" got summarized by the weakest models with the pre-fix behavior as
current — mitigated by one prompt line ("state the post-fix behavior as current").

### Round 2 — self-containment prompt variants (two leaders)

`v2a` = a self-containment rule (state facts instead of citing issue/PR/phase/round labels;
in-code identifiers like `V042` stay). `v2b` = v2a + an anchor code excerpt. `v2c` = v2a + a
navigation tool loop.

| model | variant | trap-flip % | false-assert % | core-cov % | cov % | format % | ref-leak % | med. words |
|---|---|---|---|---|---|---|---|---|
| Qwen3-4B-2507 | v1 | 0.0 | 0.0 | 95.5 | 82.5 | 68.6 | 60.0 | 71 |
| Qwen3-4B-2507 | v2a | 0.0 | 0.0 | 90.9 | 70.0 | 88.6 | 17.1 | 72 |
| Qwen3-4B-2507 | v2b | 0.0 | 0.0 | **95.5** | 70.0 | 88.6 | **14.3** | 77 |
| Qwen3-4B-2507 | v2c | 0.0 | 0.0 | 95.5 | 70.0 | 88.6 | 17.1 | 77 |
| gemma-3-12b-it | v1 | 0.0 | 0.0 | 90.9 | 72.5 | 100.0 | 60.0 | 67 |
| gemma-3-12b-it | v2a | 0.0 | 0.0 | 90.9 | 67.5 | 94.3 | 25.7 | 70 |
| gemma-3-12b-it | v2b | 0.0 | 2.5 | 90.9 | 67.5 | 100.0 | 17.1 | 68 |
| gemma-3-12b-it | v2c | 5.1 | 5.0 | 81.8 | 67.5 | 91.4 | 14.3 | 72 |

The self-containment prompt drops ref-leakage 60% → 14–26% for a small secondary-coverage cost;
core coverage holds. Anchor context (v2b) is a marginal, model-dependent win not worth the
plumbing. Navigation tools (v2c) **hurt** — gemma made 27 calls and produced the only trap-flips
of the whole variant sweep. The memory body *is* the source of truth for compaction; navigation
is the consumer's job. The shipped compactor uses the self-containment prompt, **no tools, no code
context**.

### Round 4 — verification: evidence packs beat agentic tools

17 manifest cases × 2 models × 2 methods:

| method | Qwen3-4B | gemma-3-12b |
|---|---|---|
| agentic verify (grep/read, ≤5 calls, turn-based) | 6/17 (35%) | 8/17 (47%) |
| **evidence pack + single turn** (whole-tree identifier resolution + bound-file excerpt) | **12/17 (71%)** | **13/17 (76%)** |

These models are good at *comparing* note-vs-code when the right code is in front of them and bad
at *finding* it with a bounded grep budget — negative evidence ("the function this note describes
does not exist") is exactly what a 5-call search cannot establish but a mechanical whole-tree
resolution table states outright. The evidence-pack verdict pass is the shipped design; direction
labeling (`code_ahead` vs `note_ahead`) stays advisory. Verdict accuracy of 71–76% on a hard
corpus is a triage signal for a reviewed worklist, not autonomous action.

## The offline leakage metric

`score_v2.py` carries a fuzzy **ref-leakage** metric — a regex over each summary for issue/PR
numbers, phase labels, and review-round tokens (`#\d+`, `PR`, `phase A5`, `round-6`, bare `A6`).
This is deliberately **offline only**. The runtime compactor does **no shape-regex ref lint**:
in-index references (tracker ids that resolve in the papertrail) are validated by set-membership,
while phase/round/batch labels have no authoritative vocabulary to check against, so they are
handled by the prompt alone and *measured here*. Fuzzy matching is acceptable in the eval because
a human reads the leakage number; a fuzzy lint in the runtime would false-positive on legitimate
identifiers. The eval is the check for the ref classes the runtime deliberately does not lint.

## When to re-run

These prompts are LIVE in rag-rat, versioned so a change is traceable:

- `dream/verdict.rs` — `PROMPT_VERSION = "verify-pack-v1"` (the evidence-pack verdict prompt).
- `dream/compact.rs` — `COMPACT_PROMPT_VERSION = "compact-v1"` (the self-containment compact prompt).

**Bumping either version string, or changing the dream model, means re-running this suite** —
compaction changes against the zero-trap-flip bar, verification changes against the ≥70% manifest
bar. Extend the probe set by mining negation sentences from new memories as they land.
