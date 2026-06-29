# Config Reference

`rag-rat.toml` has an `[index]` table, optional simple `[target_bindings]`, and optional richer `[[target]]` blocks.

`rag-rat init` writes a fully-commented `rag-rat.toml`: the lines reflecting your repo (bindings, embedding model) are active, and every other table below is emitted as commented defaults so the whole surface is discoverable in the file itself. For a C/C++ repo it detects C++ (any `.cpp`/`.cc`/… present) and binds the header directories as `cpp` so `.h` headers index as C++.

```toml
[index]
root = "."
database = ".rag-rat/index.sqlite"

[llm.embedding]
# the MODEL by its full id (the HF path) — no aliases. "none" disables embeddings.
model = "sentence-transformers/all-MiniLM-L6-v2"

[llm.embedding.runtime]
batch_size = 64
ort_threads = 4
omp_threads = 1
max_embedding_chars = 4000
```

## Embedding model (`[llm.embedding] model`)

Selects how `semantic_search` computes the **vector** half of its hybrid ranking. The selector is the
model's **full id — its HF path** (no aliases). `rag-rat init` recommends a default from repo size;
you can override it here. The registered models:

- `sentence-transformers/all-MiniLM-L6-v2` (default) — MiniLM transformer via FastEmbed (384-dim).
  Best retrieval quality, but the cold backfill is CPU-bound (~10-100 chunks/sec), so it's only
  comfortable for repos that finish in a few minutes.
- `BAAI/bge-small-en-v1.5` — a stronger general-retrieval transformer at the same 384-dim.
- `jinaai/jina-embeddings-v2-base-code` — a code-specific transformer (768-dim).
- `minishlab/potion-retrieval-32M` — static token-vector lookup + mean-pool (512-dim). ~100-500×
  faster on CPU at some retrieval-quality cost: distributional/synonym semantics but no context, word
  order, or polysemy disambiguation. The right choice for large repos that still want vectors. BM25
  (the other half of the hybrid) cushions the quality drop.
- `none` — structural + BM25 only; no dense vectors. `semantic_search` degrades to BM25, and every
  other tool (symbols, graph, impact, git/papertrail, memories) is unaffected. The cheapest option
  for enormous codebases (e.g. the Linux kernel) where any embedding backfill is impractical.

The selector chooses which model `init` installs and activates. The active model is recorded in the
index, so switching the model in the config takes effect after re-running `rag-rat init` or
`rag-rat models install <model-id>` (and a reconcile to re-embed under the new model). Different
models have different vector dimensions, so switching re-embeds from scratch.

## Remote embedding over Ollama (`[llm.embedding.remote]`)

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
# request_timeout_s = 60          # per-request HTTP timeout
```

**EPHEMERAL** — provision an on-demand GPU box (e.g. Modal) for the bulk reconcile, then tear it
down. rag-rat spawns the **cookbook** recipe as a subprocess; it provisions a box, prints a handshake
when it's serving, and is torn down (SIGTERM) when the reconcile finishes. Queries embed against a
**local** Ollama (`query_endpoint`) running the same model — so the query vectors share the same
space as the remote-embedded chunks.

```toml
[llm.embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"

[llm.embedding.remote]
cookbook = "@rag-rat/cookbook modal"        # EPHEMERAL: an npm package + a provider subcommand
                                            #   (e.g. `modal`, `runpod`), or a recipe path + args.
                                            #   First token picks the runner: .mjs/.js → node,
                                            #   .ts/.mts → npx tsx, else → npx -y. The value is split
                                            #   on whitespace — paths with spaces are unsupported.
model = "all-minilm"                        # the Ollama-side model name
# query_endpoint = "http://localhost:11434" # the LOCAL ollama for QUERY embedding
                                            #   (defaults to http://localhost:11434)
# auth_env = "OLLAMA_TOKEN"                 # optional bearer-token env var NAME
# gpu = "A10"                               # cookbook-only: the GPU the recipe provisions.
                                            #   Provider-specific, validated at provision time:
                                            #   Modal = a GPU request string (T4 / L4 / A10 / L40S /
                                            #     A100 / H100 / H200 / B200; default CPU — the modal
                                            #     recipe WARNS that GPU there has an exit-137
                                            #     cold-start risk);
                                            #   RunPod = a gpuTypeId (default: NVIDIA RTX A4000).
```

`gpu` applies **only** to the EPHEMERAL `cookbook` path; setting it alongside a connect `endpoint` is
a config error. Its value is **not** validated by rag-rat — the provider rejects an unknown GPU when
it tries to provision.

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

The database stores explicit schema migrations in `schema_version` with migration id,
`applied_at_ms`, checksum, and description. Opening the index **migrates an older schema forward
automatically** — the migration ladder is additive and idempotent, so a binary upgrade needs no
manual step. Only a *newer* schema (created by a future rag-rat — can't downgrade), a *dirty* or
checksum-mismatched schema (rebuild with `rag-rat index --full`), or a *missing* index (build it
first) is refused. `rag-rat doctor` reports the schema state without changing anything.

Simple bindings map a language to directories:

```toml
[target_bindings]
rust = ["crates/app/src"]
typescript = ["apps/mobile/src"]
kotlin = ["apps/wear-bridge/src"]
cpp = ["include", "src"]
markdown = ["docs"]
```

A simple binding indexes each language's default extensions in the listed directories
(`rust` → `.rs`, `typescript` → `.ts`/`.tsx`, `python` → `.py`/`.pyi`, `c` → `.c`/`.h`, etc.).
The one ambiguous case is the `.h` header: with no binding it is detected as **C** (the safe
default), but an explicit `cpp` binding also claims `.h` in its directories and indexes those headers
as **C++**. This is what lets a C++ library whose API lives in `.h` files (most of them) get header
symbols, so calls resolve to their definitions instead of going unresolved. A `.c` file is never
treated as C++.

Expanded targets add name, kind, include, and exclude metadata:

```toml
[[target]]
name = "generated-bindings"
language = "typescript"
directories = ["packages/app/src/generated"]
kind = "generated"
include = ["**/*.ts"]
exclude = ["**/*.map"]
```

Supported languages are `rust`, `typescript`, `kotlin`, `c`, `cpp`, `python`, and `markdown`. Rust,
TypeScript/TSX, Kotlin, C, C++, and Python source use tree-sitter structural indexing when files are
under the parser size cap.
Markdown uses heading-section chunking and does not use tree-sitter. Supported target kinds are
`source`, `generated`, `docs`, and `tests`; generated targets are indexed with coarse chunks and
still obey `include_generated` filtering.

Parser grammar dependencies are exact-pinned in `Cargo.toml`: `tree-sitter` 0.22.6,
`tree-sitter-rust` 0.21.2, `tree-sitter-typescript` 0.21.2, and `tree-sitter-kotlin` 0.3.8.

`[llm.embedding.runtime]` controls reconcile defaults for local embedding generation. CLI flags
still take precedence: `--batch-size` overrides `batch_size`, and `--max-embedding-chars` overrides
`max_embedding_chars`.

Thread controls:

- `ort_threads` caps the ONNX Runtime **intra-op** thread pool, applied through fastembed's session
  (`with_intra_threads`). **Caveat:** the prebuilt ONNX Runtime binaries fastembed downloads are
  Microsoft's OpenMP builds, where the intra-op setting has no effect — so on the default binaries
  this knob is inert and `omp_threads` is the one that matters.
- `omp_threads` is exported as the `OMP_NUM_THREADS` environment variable (only when not already set
  by the caller). For the OpenMP prebuilt binaries this is **the** effective embedding-thread lever.
  Note the default is `1`, which makes embedding single-threaded; raise it (e.g. to your core count)
  for faster reconciliation on multi-core machines.

(`ort_threads` is no longer exported as `ORT_NUM_THREADS` — ONNX Runtime does not read that
variable.)

`[watch]` controls the background file watcher that keeps the index fresh as files change (new
files, uncommitted edits) so graph/symbol queries reflect the working tree without a commit:

```toml
[watch]
enabled = true        # on by default; false (or RAG_RAT_NO_WATCH=1) disables it
debounce_ms = 400     # quiet window before a reindex pass
max_latency_ms = 2500 # force a pass after this much continuous activity (starvation cap)
periodic_sweep_secs = 300 # backstop pass at least this often (0 disables) — set for NFS/WSL
```

The watcher runs inside `rag-rat mcp` automatically, and on demand via `rag-rat index --watch`. It
watches the configured target directories recursively and runs the discover → reconcile → gc →
memory_validate pipeline on debounced bursts. One watcher per worktree and one writer at a time per
index are enforced with file locks under the index directory; the index DB is shared across a repo's
worktrees (a relative `database` resolves against the main worktree). File locks are unreliable on
NFS and WSL2 `drvfs`/`9p` (`/mnt/...`) mounts — keep the repo on a native filesystem.

`[version_check]` controls the best-effort check for a newer published `rag-rat` on crates.io,
surfaced to agents/operators in the SessionStart digest and the `index_status` MCP tool's `version`
field (current vs latest + the `cargo install rag-rat --force` update command):

```toml
[version_check]
enabled = true   # opted in by default; false makes zero network calls
```

The check is **cached** (refreshed at most once a day, out of band by the long-lived `rag-rat mcp`
server) and **fail-open** — offline, a non-200, or a parse miss simply yields no version info, never
an error, and **never blocks** session start (reads only the cache; `rag-rat version-check` refreshes
it synchronously on demand). The cache lives at `<index-dir>/version-check.json`. Set
`enabled = false` to disable the feature entirely (no crates.io requests).

`[oracle]` controls the background auto-fresh SCIP oracle — compiler-grade ranking that keeps itself
current without a manual `rag-rat oracle run`. **Opt-in; off by default.** When enabled, the
long-lived `rag-rat mcp` server runs the oracle for the active checkout when its index is *stale*
(changed since the last run) and *quiet* (no recent edits), heavily throttled by two gates:

```toml
[oracle]
auto_run = false                 # off by default — opt in explicitly
auto_run_quiet_period_secs = 900   # run only after ~15 min with no index change (debounce)
auto_run_min_interval_secs = 21600 # and at most once every 6 h (floor)
```

Both gates are required, not redundant: producing a `.scip` takes minutes while edits arrive in
seconds, so debouncing a single burst is not enough — the quiet-period keeps a pass from firing
mid-session, and the min-interval floor caps how often it can run regardless of churn. The pass runs
on a **detached thread of the MCP server only** (never short-lived CLI/hook commands), uses the same
lock-free production path as `oracle run` (the slow subprocess runs OUTSIDE the index write lock; only
the brief join/write serializes), and is **fail-open** — any error, or a missing indexer tool, is a
silent no-op, and the thread dies with the server process. While auto-fresh is on, `important-symbols`
reports `heuristic ranking — compiler ranking refreshes in the background` instead of nudging you to
run the oracle by hand.

`rag-rat hooks install` writes generated `post-checkout`, `post-merge`, `post-rewrite`, and
`post-commit` hooks to the current worktree's Git hooks directory. Those hooks call `rag-rat
maintenance --max-seconds 30` in the background so branch switches, merges, rebases, and commits
refresh the current worktree index and advance changed-first embedding reconciliation without
blocking normal Git operations. Each maintenance pass also runs a worktree-safe `gc` that prunes
index rows for commits no longer held by any live worktree (run `rag-rat gc` to prune on demand).
