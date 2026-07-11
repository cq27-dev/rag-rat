---
name: configure-rag-rat-dream
description: >
  Use to enable and operate rag-rat's optional AI memory-maintenance passes — the `dream --verify` /
  `--compact` model turns that verify repo memories (flag claims that no longer hold) and compact them
  (shorter summaries). Off by default. This configures `[llm.dream]` / `[llm.dream.remote]` (a chat
  model, over a Connect endpoint or an ephemeral Modal/RunPod cookbook box — vllm/ollama, never
  infinity), and sets up running it on demand or on a schedule (systemd timer). Pairs with the
  `dream-review` skill, which triages the findings these passes produce. Triggers: "configure dream",
  "enable dream", "AI memory verification", "memory compaction", "schedule dream maintenance", "set up
  the dream model".
---

# configure-rag-rat-dream — enable and run the AI memory-maintenance passes

rag-rat's `dream` command has an optional **model turn**: the `--verify` / `--compact` passes that
AI-verify repo **memories** (flag those whose claims no longer hold → `memory_reality`) and compact
them (shorter drive-by summaries). It is rag-rat's **only** generative-model dependency, **off by
default**, and gated by a deterministic layer — `rag-rat dream` stays 100% deterministic unless
`[llm.dream] enabled = true` and you pass `--verify`/`--compact`.

It earns its keep only once a repo has **accumulated memories**, so this is a later, deliberate
setup, not something to do at first index. The `dream-review` skill triages the findings these passes
produce — run this skill to turn the model on, that one to work through what it surfaces.

## When to set it up

- rag-rat has been in use a while and memories have accumulated.
- The user wants AI **verify** (reality-check memories) and/or **compact** (shorter summaries).

Otherwise skip it — the deterministic `dream` is enough.

## Config — mirrors `[llm.embedding.remote]`, but a CHAT backend

Same Connect / Ephemeral split as embeddings, but the model is a small **chat** LLM served over
`/v1/chat/completions`. **`backend` must be `ollama` or `vllm` — `infinity` is embed-only and is
rejected here.** (If the repo already runs a local infinity for embeddings, that is NOT reusable for
dream — dream needs a generation server.)

**Connect** (a chat server you already run):

```toml
[llm.dream]
enabled = true

[llm.dream.remote]
backend  = "ollama"
endpoint = "http://localhost:11434"
model    = "qwen3:4b-instruct"          # server-side (ollama) name
request_timeout_s = 300
```

**Ephemeral** (a Modal/RunPod box — same cookbook as embeddings). CPU inference on a dense evidence
pack is pathologically slow (it can blow past the timeout and never verify), so a GPU is the
practical option:

```toml
[llm.dream]
enabled = true

[llm.dream.remote]
backend  = "vllm"                        # vllm | ollama (NOT infinity)
cookbook = "@rag-rat/cookbook modal"     # or @rag-rat/cookbook runpod
gpu      = "L4"                           # 24 GB fits a 4B in fp16 — same class as the embedding box
model    = "Qwen/Qwen3-4B-Instruct-2507" # HF id for vllm; the memory-compaction eval winner
auth_env = "MODAL_TOKEN"                 # optional: env var holding the box's bearer token
request_timeout_s = 900                  # a dense evidence pack is slow to first token
```

The box is provisioned **only when there is pending work** (a zero-work guard never cold-starts a paid
GPU for a fully churn-skipped repo) and is torn down when the run ends. Temperature 0, no streaming.

Write the config the same way as the init flow: never blind — validate by loading it (`rag-rat doctor`
/ the next `dream` run fails loudly on a bad block), and confirm with the user before enabling a paid
cloud path (`cookbook` spends money and runs downloaded provisioning code with the user's credentials).

## Running it — on demand or on a schedule

The **zero-work guard** makes an unnecessary run cheap: if nothing is pending (memories all
churn-skipped), dream never provisions a box. So either cadence is safe to run often.

`--verify` always runs the deterministic checks; with `enabled = true` it adds the model verdict
pass. `--compact` writes the shorter memory summaries.

**On demand** — run it whenever maintenance is due: after a burst of new memories, before a big
review, or when a plain `rag-rat dream` shows a backlog. An agent can decide this for itself and run:

```bash
rag-rat dream --verify --compact --max-memories 20
```

**Scheduled** — a user-systemd oneshot + timer runs it hands-off (this is the maintainer's setup —
nightly at 04:30):

```ini
# ~/.config/systemd/user/rag-rat-dream.service
[Unit]
Description=rag-rat dream memory maintenance (verify + compact)
[Service]
Type=oneshot
WorkingDirectory=/path/to/the/repo          # the repo whose rag-rat.toml has [llm.dream] enabled
Environment=HOME=/home/<you>                # so ~/.modal.toml (provider creds) resolves
Environment=PATH=/home/<you>/.cargo/bin:/path/to/node/bin:/usr/bin:/bin
ExecStart=%h/.cargo/bin/rag-rat dream --verify --compact --max-memories 200
TimeoutStartSec=60min                       # room for a cold GPU boot + a large first backfill
Nice=10

# ~/.config/systemd/user/rag-rat-dream.timer
[Unit]
Description=Nightly rag-rat dream memory maintenance at 04:30
[Timer]
OnCalendar=*-*-* 04:30:00
Persistent=true                             # run on next boot if the machine was off at 04:30
[Install]
WantedBy=timers.target
```

Enable with `systemctl --user enable --now rag-rat-dream.timer`.

**Bare-service env gotchas** (the two things that bite a systemd/cron dream run): set `HOME` so
`~/.modal.toml` / the provider creds resolve, and a `PATH` that includes both `rag-rat` and
`node`/`npx` — the cookbook recipe shells out to `npx`. The ephemeral box has its own idle +
max-lifetime backstops, so even a wedged local run can't bill the GPU indefinitely.

## Then triage

Once it runs, work through what it surfaced (unverifiable / diverged memories, compaction results)
with the **`dream-review`** skill.
