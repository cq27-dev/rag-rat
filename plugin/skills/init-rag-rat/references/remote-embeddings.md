# Remote embeddings — overview and model pairings

Read this when the user wants to offload embedding off the local CPU: a **large repo**, a wish for a
**stronger code-specific embedder**, or an available **GPU / cloud budget**. rag-rat speaks the
OpenAI-compatible `/v1/embeddings` API, so every non-CPU backend is a single `[llm.embedding.remote]`
block. Then read the path-specific reference:

- **`local-infinity.md`** — the user has Docker and wants the embedder to run locally (or on a box
  they control). This is *Connect* mode (`endpoint`).
- **`ephemeral-providers.md`** — no capable local machine; provision a paid **Modal/RunPod** GPU
  worker just for the backfill, then tear it down. This is *Ephemeral* mode (`cookbook`).

## The two config keys, and dim parity

- `[llm.embedding] model` is the rag-rat **registry selector** — it fixes the **dimension** and must
  be one of the registry rows below.
- `[llm.embedding.remote] model` is the **server-side** name — *it differs by backend*: the
  HuggingFace id for **infinity/vLLM**, the Ollama model name for **ollama**.

They must agree on dimension and family. A cross-backend name (e.g. the HF id sent to ollama) or a
cross-dim pair fails the parity check when the config loads. Authority for these pairings:
`crates/rag-rat-cli/src/init/wizard/draft.rs`.

## Local ↔ remote model pairings

| `[llm.embedding] model` (registry selector) | dim | `remote.model` for **infinity / vLLM** | `remote.model` for **ollama** |
|---|---|---|---|
| `sentence-transformers/all-MiniLM-L6-v2` | 384 | `sentence-transformers/all-MiniLM-L6-v2` | `all-minilm` |
| `BAAI/bge-small-en-v1.5` | 384 | `BAAI/bge-small-en-v1.5` | `qllama/bge-small-en-v1.5:f16` |
| `jinaai/jina-embeddings-v2-base-code` | 768 | `jinaai/jina-embeddings-v2-base-code` | `ordis/jina-embeddings-v2-base-code` |

**Recommended for a code repo: `jinaai/jina-embeddings-v2-base-code`** — 768-dim, 8192-token context,
so whole code chunks embed without truncation. Use the **same model across local and ephemeral** so
all vectors share one space.

rag-rat's registry currently supplies 384- and 768-dim models. Other Ollama embedders exist
(`nomic-embed-text` 768; `mxbai-embed-large` / `bge-m3` / `snowflake-arctic-embed2` /
`qwen3-embedding:0.6b` 1024) but only pair if a registry model shares their dim — so 1024-dim models
are not usable yet.

## Default local endpoints per backend

| backend | default endpoint | route |
|---|---|---|
| infinity | `http://localhost:7997` | `/embeddings` |
| ollama | `http://localhost:11434` | `/api/embed` |
| vLLM | `http://localhost:8000` | `/v1/embeddings` |

## `[llm.embedding.remote]` fields (reference)

- `backend` — `"ollama"` (default) `| "infinity" | "vllm"`. Routes provisioning + markers; the wire
  call is identical.
- `endpoint` — Connect: base URL of a running server. **Mutually exclusive with `cookbook`.**
- `cookbook` — Ephemeral: the recipe rag-rat spawns to provision a box. **Mutually exclusive with
  `endpoint`.**
- `query_endpoint` — Ephemeral only: the LOCAL server that embeds queries after the box is torn down.
  **Required for a non-ollama backend** (ollama defaults to `localhost:11434`).
- `gpu` — Ephemeral only: provider-specific GPU class. Setting it with `endpoint` is a config error.
- `auth_env` — name of the env var holding the bearer token, if the server needs auth (not the token
  itself).
- Tuning (optional): `batch_size`, `concurrency` (Connect-safe 1; cookbook default 32),
  `max_batch_chars`, `request_timeout_s`, `num_ctx`.
