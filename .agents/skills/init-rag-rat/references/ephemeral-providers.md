# Ephemeral GPU workers — Modal / RunPod (cookbook)

Provision a **paid** cloud GPU worker (running infinity/vLLM/ollama) just for the embedding backfill,
then tear it down. rag-rat spawns a **cookbook** recipe to do this. Use when there is no capable local
machine but there is a cloud budget. Default model: `jinaai/jina-embeddings-v2-base-code` — see
`remote-embeddings.md` for the pairing table and dim rules.

## Warn first — every time

Before configuring or running an ephemeral path, get explicit go-ahead:

- **It costs money.** A GPU box is provisioned on Modal or RunPod for the duration of the backfill.
- **It runs third-party code with the user's credentials.** `cookbook = "@rag-rat/cookbook …"` runs
  downloaded provisioning code (via `npx -y`) with the user's shell, network, and **Modal/RunPod
  tokens**. Only proceed with a cookbook the user trusts.
- Require the provider set up: **Modal** `modal token new`; **RunPod** `RUNPOD_API_KEY` in the env.

## The config block

```toml
[llm.embedding]
model = "jinaai/jina-embeddings-v2-base-code"

[llm.embedding.remote]
backend        = "infinity"
cookbook       = "@rag-rat/cookbook modal"     # or: @rag-rat/cookbook runpod
gpu            = "A100"                          # provider-specific, from the tables below
model          = "jinaai/jina-embeddings-v2-base-code"
query_endpoint = "http://localhost:7997"        # REQUIRED for infinity/vLLM
```

`query_endpoint` is **mandatory** for a non-ollama backend: the ephemeral box does the big backfill,
then queries embed **locally** against the same model. Point it at a local infinity
(`local-infinity.md`) — see the hybrid below.

## Concurrency is auto-tuned — don't hand-set it

You do **not** need to tune `concurrency` for a cookbook box. During the reconcile, rag-rat runs a
measured concurrency sweep against the provisioned box, picks the throughput **knee** (clamped to the
`concurrency` cap — default 32), surfaces it in the provision log, and caches it for later runs. Set
`concurrency` only to *lower* the cap (a rate-limited backend); otherwise leave the default and let
the auto-tuner find the knee.

## GPU classes

Provider-specific, validated by the provider at provision time. Authority:
`crates/rag-rat-cli/src/init/wizard/draft.rs`.

**Modal** (`gpu = "…"`): `T4`, `L4`, `A10`, `L40S`, `A100`, `A100-40GB`, `A100-80GB`, `H100`, `H100!`

**RunPod** (`gpu = "<gpuTypeId>"`): `NVIDIA RTX A4000`, `NVIDIA GeForce RTX 4090`, `NVIDIA L40`,
`NVIDIA L40S`, `NVIDIA A40`, `NVIDIA A100 80GB PCIe`, `NVIDIA A100-SXM4-40GB`, `NVIDIA A100-SXM4-80GB`,
`NVIDIA H100 80GB HBM3`, `NVIDIA H100 NVL`, `NVIDIA H200`, `NVIDIA B200`, `Tesla V100-PCIE-16GB`

## Recommended hybrid (large repo, modest workstation)

The highest-leverage setup: **ephemeral GPU for the one-time big index + local infinity for queries
and incremental reconciles**, *same model/backend* so one shared vector space.

1. Stand up a local infinity (`local-infinity.md`) — this becomes `query_endpoint`.
2. Configure the ephemeral block above with `cookbook` + `gpu` for the initial backfill.
3. `index --discover` runs the backfill on the provisioned box; it tears down afterward. Queries and
   later incremental reconciles use the local infinity.
