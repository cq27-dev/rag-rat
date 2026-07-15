# Remote embedding over Ollama (`[llm.embedding.remote]`)

Part of the [config reference](../config.md).

The `model = "..."` selector names the **model**; an optional `[llm.embedding.remote]` block
serves **that same model** over an Ollama server (`POST /api/embed`) instead of running it
in-process — the lever for large repos whose CPU backfill is too slow on the indexing box. Ollama is
a **transport, not a model**: there is no `model = "ollama"` selector. The block's mere **presence**
flips the runtime; absent it, embedding stays local. Same model, same `model_id`, same dimension —
only the runtime changes, so chunk embeddings are keyed by the model regardless of where they were
computed.

There is **no `mode` field** — the mode is INFERRED from which URL field is set. **Exactly one** of
`endpoint` (CONNECT) or `cookbook` (EPHEMERAL) must be present; both or neither is rejected.

**CONNECT** — talk to an already-running Ollama at a fixed URL:

```toml
[llm.embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"   # the MODEL (HF path, 384-dim)

[llm.embedding.remote]       # PRESENCE = "serve that model via Ollama"
endpoint = "http://box:11434"     # CONNECT: the Ollama server URL (required)
model = "all-minilm"              # the Ollama-side model name (the server's own identifier)
# auth_env = "OLLAMA_TOKEN"       # NAME of an env var holding a bearer token (never the token itself)
# batch_size = 256                # texts per /api/embed request
# concurrency = 1                 # 1..=128; connect-safe (cookbook/ephemeral default = 32)
# max_batch_chars = 384000        # max total input chars per /api/embed request
# num_ctx = 4096                  # optional Ollama context window (options.num_ctx)
# request_timeout_s = 60          # per-request HTTP timeout
```

**EPHEMERAL** — provision an on-demand GPU box (e.g. Modal) for the bulk reconcile, then tear it
down. rag-rat spawns the **cookbook** recipe as a subprocess; it provisions a box, prints a handshake
(`ready`, carrying the endpoint) when it's serving, and is torn down (SIGTERM) when the reconcile
finishes. rag-rat auto-tunes the client concurrency against the box itself (see below). Queries embed
against a **local** Ollama (`query_endpoint`) running the same model — so the query vectors share the
same space as the remote-embedded chunks.

```toml
[llm.embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"

[llm.embedding.remote]
cookbook = "@rag-rat/cookbook modal"        # EPHEMERAL: an npm package + a provider subcommand
                                            #   (e.g. `modal`, `runpod`), or a recipe path + args.
                                            #   First token picks the runner: .mjs/.js → node,
                                            #   .ts/.mts → npx tsx, else → npx -y. The value is split
                                            #   on whitespace — paths with spaces are unsupported.
backend = "ollama"                          # "ollama" | "infinity" | "vllm" (default "ollama").
                                            #   Selects the box image/launch/route; the embed wire
                                            #   call is the OpenAI shape for all three. vLLM requires
                                            #   a GPU (its image is CUDA-only).
model = "all-minilm"                        # server-side model NAME: an ollama name for ollama, a
                                            #   HuggingFace id (e.g.
                                            #   sentence-transformers/all-MiniLM-L6-v2) for
                                            #   infinity/vLLM (auto-downloaded on the box).
# query_endpoint = "http://localhost:11434" # LOCAL server for QUERY embedding (the box is torn down
                                            #   after reconcile). Defaults to local Ollama ONLY for
                                            #   backend="ollama"; REQUIRED for infinity/vLLM — point
                                            #   it at a local server of the SAME backend + model.
# num_ctx = 4096                            # ollama-only: baked as OLLAMA_CONTEXT_LENGTH on the box
# auth_env = "OLLAMA_TOKEN"                 # optional bearer-token env var NAME
# gpu = "A10"                               # cookbook-only: the GPU the recipe provisions.
                                            #   Provider-specific, validated at provision time:
                                            #   Modal = a GPU request string (T4 / L4 / A10 / L40S /
                                            #     A100 / H100 / H200 / B200; default CPU — the modal
                                            #     recipe WARNS that GPU there has an exit-137
                                            #     cold-start risk);
                                            #   RunPod = a gpuTypeId (default: NVIDIA RTX A4000).
# batch_size = 256                          # texts per /api/embed request
# concurrency = 32                          # 1..=128 — a CAP; rag-rat auto-tunes the client fan-out within it
# max_batch_chars = 384000                  # max total input chars per /api/embed request
# request_timeout_s = 60
```

`gpu` applies **only** to the EPHEMERAL `cookbook` path; setting it alongside a connect `endpoint` is
a config error. Its value is **not** validated by rag-rat — the provider rejects an unknown GPU when
it tries to provision.

For remote Ollama, `batch_size` and `max_batch_chars` cap one HTTP request; `concurrency` controls
how many of those capped requests reconcile may keep in flight. Candidate chunk order stays
need-first/id-first; rag-rat does not size-sort chunks.

When `concurrency` is omitted, CONNECT configs default to `1` for upgrade safety with ordinary
Ollama servers. Set it explicitly after starting the server with matching parallelism. EPHEMERAL
cookbook configs default to `32`. For ephemeral configs `concurrency` is a **cap**: after the box
boots, **rag-rat** (not the recipe) runs a short micro-sweep with its real `OllamaEmbedder` and
auto-tunes the client fan-out *within* the cap, caching the chosen knee in the index's `index_meta`
per `(runtime, provider, GPU, model, request-shape)` with a TTL. It uses that knee for the live
reconcile but never raises it above — or overwrites — your configured value. Lower `concurrency` to
tighten the cap; the tuner re-tunes within the new limit. `batch_size`/`max_batch_chars` are left
untouched (the sweep does not vary them). Values above `128` are rejected to keep worker threads and
reconcile windows bounded. `RAG_RAT_TUNE_MS` overrides the sweep budget, `RAG_RAT_TUNE_TTL_MS` the
cache TTL (0 = re-sweep every time), and `RAG_RAT_DISABLE_TUNING=1` skips the sweep (uses the cap).

### Init cookbook catalog

`rag-rat init` has a repo-local cookbook catalog for its EPHEMERAL selector. This catalog affects
only wizard choices; runtime still uses the selected `[llm.embedding.remote] cookbook` and `gpu`
strings verbatim. Use it to add custom recipes or override provider GPU lists without waiting for a
new rag-rat release:

```toml
[init.cookbooks.modal]
label = "Modal"
command = "@rag-rat/cookbook modal"
gpus = ["T4", "L4", "A10", "H100"]

[init.cookbooks.my-provider]
label = "My Provider"
command = "./recipes/my-provider.mjs"
gpus = ["small", "large"]
```

Built-in keys are `modal` and `runpod`. A table with the same key overrides the wizard entry; a new
key appends a new choice. `command` is required for new entries. `gpus = []` is valid and means the
wizard shows no GPU choices for that cookbook.

Provisioning happens **only on an explicit `rag-rat reconcile`** (the deliberate bulk pass) — the
background watcher/maintenance pass does **not** cold-start a GPU box for a few changed chunks (it
leaves them pending; an explicit reconcile embeds them). So run, after editing:

```bash
rag-rat models install sentence-transformers/all-MiniLM-L6-v2   # install/activate (probes the box)
rag-rat reconcile                                               # provisions, embeds, tears down
```

The **two `model` keys are different things**: `[llm.embedding] model` is the rag-rat **model
selector** (the HF-path model_id — resolves the dimension + identity); `[remote] model` is the
**Ollama-side model name** sent in the request body. They need not be spelled the same. Only
**transformer** models (`sentence-transformers/all-MiniLM-L6-v2`, `BAAI/bge-small-en-v1.5`,
`jinaai/jina-embeddings-v2-base-code`) can be served remotely — a `[remote]` block on
`minishlab/potion-retrieval-32M` (static), the hash model, or `none` is rejected.

The freshness key is **endpoint-independent** — pointing the `endpoint` (or each ephemeral box's
per-run URL) at a different host does **not** re-embed the repo. A re-embed happens only when the
`[remote] model` changes or you flip between local and remote (those change the vector space; the
endpoint does not). If the (query) endpoint is unreachable at query time, `semantic_search` degrades
to BM25 rather than failing. Embedding is fully offline only when the endpoint is local.
**Credentials go in `auth_env` only** — an `endpoint`/`query_endpoint` URL with embedded
`user:pass@host` is rejected, because that URL is persisted into the index.
