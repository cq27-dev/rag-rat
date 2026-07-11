# @rag-rat/skills

One-command installer for **rag-rat's agent skills** — reusable instruction sets that teach your
coding agent to get the most out of a [rag-rat](https://github.com/cq27-dev/rag-rat)-indexed repo.

```bash
npx @rag-rat/skills
```

That installs the skills into whichever agents it detects (Claude Code → `.claude/skills/`, Codex →
`.codex/skills/`, Cursor, opencode, and 70+ others).

## What you get

| Skill | What it does |
|---|---|
| **`using-rag-rat`** | The working rule for any rag-rat repo: reach for the MCP tools (`semantic_search`, `symbol_lookup`, `impact_surface`, the call graph, `important_symbols`) to find and understand code before grep, and record durable, non-obvious learnings as cross-agent rag-rat memories before finishing. |
| **`dream-review`** | Triage and resolve the `rag-rat dream` memory-maintenance worklist: per finding kind, fix the underlying memory / coverage gap (resolving the finding at the root) or record an accept / dismiss verdict. |
| **`init-rag-rat`** | Set up rag-rat in a not-yet-indexed repo (a dormant MCP server): scan the repo, guide the embedding-backend choice (local FastEmbed, a local infinity Docker server, or an ephemeral Modal/RunPod GPU worker), preview + write the config after confirmation, run the first index, and prompt an MCP restart. |
| **`configure-rag-rat-dream`** | Enable and operate the optional AI memory-maintenance passes (`dream --verify`/`--compact`): configure the `[llm.dream]` chat model (Connect, or ephemeral Modal/RunPod via cookbook — vllm/ollama), and run it on demand or on a schedule (systemd timer). Pairs with `dream-review`, which triages the findings it produces. |

## Commands

```bash
npx @rag-rat/skills                 # install (default)
npx @rag-rat/skills update          # refresh rag-rat's skills to the latest
npx @rag-rat/skills list            # list installed skills
npx @rag-rat/skills remove          # remove rag-rat's skills
```

`update` and `remove` (plain, or with only `-g`/`-y`) default to rag-rat's own skills
(`using-rag-rat`, `dream-review`, `init-rag-rat`, `configure-rag-rat-dream`) — they won't touch
unrelated skills you've
installed. Pass your
own targets — a skill name, `--agent`, or `--all` — to drive the underlying `skills` CLI directly
instead.

Flags are forwarded to the underlying installer:

```bash
npx @rag-rat/skills -a claude-code       # only Claude Code
npx @rag-rat/skills -s using-rag-rat     # only one skill
npx @rag-rat/skills -g                   # install to your home dir (global), not the project
npx @rag-rat/skills --copy               # copy files instead of symlinking
npx @rag-rat/skills -y                   # non-interactive
```

## How it works

This package is a **thin wrapper** over the [`skills`](https://github.com/vercel-labs/skills) CLI,
pinned to rag-rat's canonical skill directory (`.agents/skills` in the rag-rat repo). It doesn't
reinvent the multi-agent installer — `skills` already knows how to place a `SKILL.md` into every
supported agent, symlink-or-copy, project-or-global. `npx @rag-rat/skills` is just the branded,
single-command entry point; it forwards your flags verbatim.

Equivalent to running:

```bash
npx skills add https://github.com/cq27-dev/rag-rat/tree/main/.agents/skills
```

The skills themselves live in [`.agents/skills/`](https://github.com/cq27-dev/rag-rat/tree/main/.agents/skills)
in the rag-rat repo — that's the single source of truth; this package installs from it.
