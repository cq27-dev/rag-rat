---
name: dream-review
description: >
  Use when asked to review, triage, or resolve rag-rat "dream" findings — the memory-maintenance
  worklist the `dream` tool produces. Walks the open findings and, per kind, either fixes the
  underlying memory / coverage gap (which resolves the finding at the root) or records an
  accept / dismiss verdict with `dream_review`. Triggers: "dream review", "resolve dream findings",
  "triage the dream worklist", "go through pending dream findings".
---

# dream-review — triage and resolve the rag-rat dream worklist

The **`dream`** tool surfaces findings **about** the repo's memories and load-bearing code. It never
mutates a memory — the deterministic passes and the optional model **propose**; a reviewer
**decides**. This skill is that reviewer loop: for each open finding, investigate with the rag-rat
MCP tools, then resolve it — preferring a **root fix** (repair the memory / close the coverage gap,
so the finding resolves on its own next run) over a bare verdict.

Drive the whole loop through the **rag-rat MCP tools** — `dream` and `dream_review` for the worklist,
and `semantic_search` / `symbol_lookup` / `impact_surface` / `find_callers` / `git_history_for_symbol`
/ `memory_search` / `read_chunk` to investigate — not `grep`, and not the `rag-rat` CLI (an agent has
the MCP but may not have the binary on PATH). It is a batch chore meant to run a few times a day, not
continuously.

## 1. Preflight

- Confirm the index is fresh: call **`index_status`**. If it reports drift, call **`heal_index`** to
  repair already-indexed files. (Discovering brand-new files and re-embedding is a CLI/cron job —
  `rag-rat index --discover` then `rag-rat reconcile` — outside this MCP loop; if findings look
  empty, that's the thing to ask a human to run.)
- **The `dream` tool is deterministic-only.** It recomputes `coverage_gap` + `stale_reference` on
  every call and *surfaces* any `memory_divergence` / `memory_unverifiable` findings a prior model
  run persisted — but it does **not** run the model itself. Generating *fresh* model findings is the
  CLI/cron `rag-rat dream --verify --compact` pass (it provisions a GPU and takes minutes, so it
  never runs from a tool call). If the divergence findings look stale, that CLI pass is what refreshes
  them.

## 2. Pull the open worklist

Call **`dream`** — no args needed (`limit` caps the number of `coverage_gap` findings, default 20).
It returns `findings[]`, each `{ id, kind, subject, evidence, rank, status }`. Work **highest `rank`
first** (rank decays with age). The `id` (a short hex; a unique prefix also works) is what you pass
to `dream_review`.

Call `dream` with `{ "all": true }` to *additionally* list already-`accepted` / `dismissed` findings
— use it to find one to reset.

## 3. Resolve each finding, by kind

For every finding: **investigate first, then act.** Prefer the root fix; fall back to a verdict. All
verdicts go through **`dream_review`** with `{ "finding": "<id>", "verdict": "accept" | "dismiss" |
"reset" }`.

### `coverage_gap` — `subject = "path::symbol"`
A load-bearing symbol (many callers) with no memory binding.
- Investigate: `impact_surface` / `important_symbols` (personalize on the symbol) /
  `git_history_for_symbol` — is there a durable, non-obvious invariant, decision, or footgun here?
- **Root fix:** if yes, `memory_create` an `Invariant`/`Decision`/`Risk` **bound to that symbol**
  (`bind.id` = its `sym_…` handle, or `bind.path`). The gap closes automatically on the next `dream`
  call (the binding now exists) — no verdict needed.
- **Dismiss** if the symbol is self-explanatory / not memory-worthy: `dream_review` with
  `verdict: "dismiss"`.
- Use **accept** only to mean "real gap, but I'm not writing the memory in this pass" (keeps it out
  of the open list without claiming it's a non-issue).

### `stale_reference` — `subject = <memory id>`, `evidence` lists the unresolved path(s)
A memory body references a `.rs` path that no longer resolves against the index.
- Read the memory: `memory_show` with its id. Locate where the path went: `semantic_search` /
  `symbol_lookup` on the moved code.
- **Root fix:** if the code moved, `memory_update` the body to the new path, and/or `memory_rebind`
  to re-anchor. The finding resolves once the reference is valid again. If the memory is genuinely
  stale, `memory_mark_obsolete`.
- **Dismiss** only for a true false positive (e.g. a deliberate historical reference the regex can't
  tell apart): `dream_review` with `verdict: "dismiss"`.

### `memory_unverifiable` — `subject = <memory id>`
Decided deterministically: the memory's bindings are all gone/absent **and** none of its identifiers
resolve anywhere in the whole-tree index.
- Read it (`memory_show`). Does the code it describes still exist under a new name?
- **Root fix:** re-anchor with `memory_rebind`; or if it's obsolete, `memory_mark_obsolete`.
- **Dismiss** if it's still-true prose that simply isn't code-anchorable (accept it as-is):
  `dream_review` with `verdict: "dismiss"`.

### `memory_divergence` — `subject = <memory id>`, `evidence` = the model's cited pack lines
The model judged the memory to have drifted from current code. **The model is ~71–76% accurate —
verify before acting.**
- Read the memory and the cited lines yourself (`memory_show`, `impact_surface`, `read_chunk`). Is
  the note actually wrong about current code?
- **Root fix:** if genuinely stale, `memory_update` to correct it (or `memory_mark_obsolete`). The
  divergence finding drops once the stored verdict flips `current` on a re-check, or the body edit
  self-invalidates the old verdict.
- **Accept** if the divergence is real but intentional — the note is deliberately *ahead* of unmerged
  code (a "note-ahead" case): `dream_review` with `verdict: "accept"`.
- **Dismiss** if the model is wrong (false positive): `dream_review` with `verdict: "dismiss"`.

## 4. Verdict semantics (quick reference)

Each is a **`dream_review`** call, `{ "finding": "<id>", "verdict": … }`:

| `verdict` | Meaning |
|---|---|
| `accept` | Real, acknowledged (action pending) — leaves the open worklist. |
| `dismiss` | False positive / won't-fix — hidden from the worklist. |
| `reset` | Undo a prior verdict, back to open. |

- A verdict **survives future runs**: a re-run that still reports the finding keeps your
  accept/dismiss; a finding the code makes obsolete resolves on its own.
- Fixing the root (memory / coverage) is **better than a verdict** whenever a real fix exists — the
  finding resolves cleanly and the next agent benefits from the repaired memory.
- Only `open`/`accepted`/`dismissed` findings are reviewable; a `resolved`/`superseded` one is not.

## 5. Loop until clear

Call `dream` again and repeat until `findings[]` is empty (or only accepted/dismissed remain under
`{ "all": true }`). If your fixes created new memories, a fresh call may open follow-on findings —
one more pass usually converges.

## Guardrails

- **Never edit, demote, or mark-obsolete a memory without verifying the claim yourself.** The passes
  propose; you decide. This is the invariant that keeps dream from silently corrupting the memory
  layer.
- **Prefer the root fix over a bare `dismiss`.** Dismissing a real issue just hides it.
- **Capture what you learn.** If investigating a finding taught you a durable, non-obvious fact,
  `memory_create` it (that also closes `coverage_gap` findings).
- Respect the voice/scope rules of whatever repo you're in when writing memory bodies.
