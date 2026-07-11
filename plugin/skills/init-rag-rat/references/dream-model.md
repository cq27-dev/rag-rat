# Dream model passes (optional, later) — `[llm.dream.remote]`

**`init` does not configure this, and you don't need it to index or search.** `[llm.dream]` enables
rag-rat's ONLY generative-model dependency: the `dream --verify` / `--compact` passes that AI-verify
and compact repo **memories**. It's off by default and only earns its keep once a repo has
*accumulated* memories — so treat it as a later, optional step, not first-time setup. (The
`dream-review` skill triages the findings these passes produce.)

## When to set it up

- rag-rat has been in use a while and memories have accumulated.
- The user wants AI **verify** (flag memories whose claims no longer hold → `memory_reality`) and/or
  **compact** (shorter drive-by memory summaries).

Otherwise skip it — `rag-rat dream` stays 100% deterministic without a model.

## Config — mirrors `[llm.embedding.remote]`, but a CHAT backend

Same Connect / Ephemeral split as embeddings, but the model is a small **chat** LLM served over
`/v1/chat/completions`. **`backend` must be `ollama` or `vllm` — `infinity` is embed-only and is
rejected here.**

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

## Run it

```
rag-rat dream --verify --compact --max-memories 20
```

`--verify` always runs the deterministic checks; with `enabled = true` it adds the model verdict
pass. Triage what it surfaces with the `dream-review` skill.
