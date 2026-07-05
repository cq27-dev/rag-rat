---
name: dream-review
description: >
  Use when asked to review, triage, or resolve rag-rat "dream" findings — the memory-maintenance
  worklist `rag-rat dream` produces. Walks the pending (open) findings and, per kind, either fixes
  the underlying memory / coverage gap (which resolves the finding at the root) or records an
  accept / dismiss verdict. Triggers: "dream review", "resolve dream findings", "triage the dream
  worklist", "go through pending dream findings".
---

# dream-review — triage and resolve the rag-rat dream worklist

`rag-rat dream` surfaces findings **about** the repo's memories and load-bearing code. It never
mutates a memory — the deterministic passes and the optional model **propose**; a reviewer
**decides**. This skill is that reviewer loop: for each open finding, investigate with the rag-rat
tools, then resolve it — preferring a **root fix** (repair the memory / close the coverage gap, so
the finding resolves on its own next run) over a bare verdict.

Run this from inside the rag-rat-indexed repo. Use the **rag-rat MCP tools** to investigate
(`semantic_search`, `symbol_lookup`, `impact_surface`, `find_callers`, `git_history_for_symbol`,
`memory_search`) — not `grep`. It is a batch chore meant to run a few times a day, not continuously.

## 1. Preflight

- Confirm the index is fresh: `rag-rat index_status` (MCP) or `rag-rat doctor`. If findings look
  stale/empty, `rag-rat index --discover` then `rag-rat reconcile`.
- (Re)generate findings. Deterministic only:

  ```bash
  rag-rat dream
  ```

  With the model configured (`[llm.dream] enabled = true`) also run the verdict + compaction passes,
  which add `memory_divergence` findings and summaries:

  ```bash
  rag-rat dream --verify --compact --max-memories 20
  ```

## 2. Pull the open worklist

```bash
rag-rat dream --json
```

`findings[]` carries `{ id, kind, subject, evidence, rank, status }`. Work **highest `rank`
first** (rank decays with age). The `id` (a short hex; a unique prefix also works) is what you pass
to the verdict commands. `rag-rat dream --all --json` additionally lists already-`accepted` /
`dismissed` findings (use it to find one to `--reset`).

## 3. Resolve each finding, by kind

For every finding: **investigate first, then act.** Prefer the root fix; fall back to a verdict.

### `coverage_gap` — `subject = "path::symbol"`
A load-bearing symbol (many callers) with no memory binding.
- Investigate: `impact_surface` / `important_symbols` (personalize on the symbol) /
  `git_history_for_symbol` — is there a durable, non-obvious invariant, decision, or footgun here?
- **Root fix:** if yes, `memory_create` (MCP) an `Invariant`/`Decision`/`Risk` **bound to that
  symbol** (`bind.id` = its `sym_…` handle, or `bind.path`). The gap closes automatically on the
  next `rag-rat dream` (the binding now exists) — no verdict needed.
- **Dismiss** if the symbol is self-explanatory / not memory-worthy: `rag-rat dream <id> --dismiss`.
- Use **accept** only to mean "real gap, but I'm not writing the memory in this pass" (keeps it out
  of the open list without claiming it's a non-issue).

### `stale_reference` — `subject = <memory id>`, `evidence` lists the unresolved path(s)
A memory body references a `.rs` path that no longer resolves against the index.
- Read the memory: `rag-rat memory show <id>`. Locate where the path went: `semantic_search` /
  `symbol_lookup` on the moved code.
- **Root fix:** if the code moved, `memory_update` (MCP) the body to the new path, and/or
  `memory_rebind` (MCP or `rag-rat memory rebind`) to re-anchor. The finding resolves once the
  reference is valid again. If the memory is genuinely stale, `memory_mark_obsolete` (MCP).
- **Dismiss** only for a true false positive (e.g. a deliberate historical reference the regex
  can't tell apart): `rag-rat dream <id> --dismiss`.

### `memory_unverifiable` — `subject = <memory id>`
Decided deterministically: the memory's bindings are all gone/absent **and** none of its
identifiers resolve anywhere in the whole-tree index.
- Read it (`rag-rat memory show <id>`). Does the code it describes still exist under a new name?
- **Root fix:** re-anchor with `memory_rebind`; or if it's obsolete, `memory_mark_obsolete` (MCP).
- **Dismiss** if it's still-true prose that simply isn't code-anchorable (accept it as-is):
  `rag-rat dream <id> --dismiss`.

### `memory_divergence` — `subject = <memory id>`, `evidence` = the model's cited pack lines
The model judged the memory to have drifted from current code. **The model is ~71–76% accurate —
verify before acting.**
- Read the memory and the cited lines yourself (`impact_surface`, `read_chunk`). Is the note
  actually wrong about current code?
- **Root fix:** if genuinely stale, `memory_update` to correct it (or `memory_mark_obsolete`). The
  divergence finding drops once the stored verdict flips `current` on a re-check, or the body edit
  self-invalidates the old verdict.
- **Accept** if the divergence is real but intentional — the note is deliberately *ahead* of
  unmerged code (a "note-ahead" case): `rag-rat dream <id> --accept`.
- **Dismiss** if the model is wrong (false positive): `rag-rat dream <id> --dismiss`.

## 4. Verdict semantics (quick reference)

```bash
rag-rat dream <id> --accept    # real, acknowledged (action pending) — leaves the open worklist
rag-rat dream <id> --dismiss   # false positive / won't-fix — hidden from the worklist
rag-rat dream <id> --reset     # undo a verdict, back to open
```

- A verdict **survives future runs**: a re-run that still reports the finding keeps your
  accept/dismiss; a finding the code makes obsolete resolves on its own.
- Fixing the root (memory / coverage) is **better than a verdict** whenever a real fix exists — the
  finding resolves cleanly and the next agent benefits from the repaired memory.
- Only `open`/`accepted`/`dismissed` findings are reviewable; a `resolved`/`superseded` one is not.

## 5. Loop until clear

Re-run `rag-rat dream --json` and repeat until `findings[]` is empty (or only accepted/dismissed
remain under `--all`). If your fixes created new memories, a fresh run may open follow-on findings —
one more pass usually converges.

## Guardrails

- **Never edit, demote, or mark-obsolete a memory without verifying the claim yourself.** The passes
  propose; you decide. This is the invariant that keeps dream from silently corrupting the memory
  layer.
- **Prefer the root fix over a bare `--dismiss`.** Dismissing a real issue just hides it.
- **Capture what you learn.** If investigating a finding taught you a durable, non-obvious fact,
  `memory_create` it (that also closes `coverage_gap` findings).
- Respect the voice/scope rules of whatever repo you're in when writing memory bodies.
