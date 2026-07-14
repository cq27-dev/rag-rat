# Config Reference

`rag-rat.toml` has an `[index]` table, optional simple `[target_bindings]`, and optional richer `[[target]]` blocks.

`rag-rat init` writes a fully-commented `rag-rat.toml`: the lines reflecting your repo (bindings, embedding model) are active, and every other table below is emitted as commented defaults so the whole surface is discoverable in the file itself. For a C/C++ repo it detects C++ (any `.cpp`/`.cc`/… present) and binds the header directories as `cpp` so `.h` headers index as C++.

```toml
[index]
root = "."
# database is unset by default: the index + memories live in the machine-global store
# (see "Database location" below). Set it to keep this repo on its own file (deprecated):
# database = ".rag-rat/index.sqlite"

[llm.embedding]
# the MODEL by its full id (the HF path) — no aliases. "none" disables embeddings.
model = "sentence-transformers/all-MiniLM-L6-v2"

[llm.embedding.runtime]
batch_size = 64
ort_threads = 4
omp_threads = 1
max_embedding_chars = 4000
```

## Database location (`[index] database`)

By default rag-rat keeps **one consolidated database per machine** — `$XDG_DATA_HOME/rag-rat/rag-rat.sqlite`
(falling back to `~/.local/share/rag-rat/rag-rat.sqlite`, or `%APPDATA%\rag-rat\rag-rat.sqlite` on
Windows) — holding every registered repo's index and, most importantly, its **repo memories**. This
means an accidental `git clean -fdx` or a deleted checkout no longer takes your authored memories
with it. The location is resolved by cascade:

1. `RAG_RAT_DATA_DIR` — an explicit override, honored verbatim.
2. `$XDG_DATA_HOME/rag-rat`.
3. `$HOME/.local/share/rag-rat` (the XDG default).
4. (Windows) `%APPDATA%\rag-rat`.

Set an explicit `database` key to opt a repo **out** of the consolidated store and keep it on its own
per-repo file:

```toml
[index]
database = ".rag-rat/index.sqlite"   # deprecated: this repo stays un-consolidated
```

An explicit key is honored for backward compatibility but is **deprecated** — an un-consolidated repo
does not share the machine's global store (and, later, cannot participate in memory sync). A relative
path resolves against the **main worktree top**, so every worktree of a repo shares one index; an
absolute path is used as-is.

**Upgrading an existing repo.** A repo indexed before this default flip has a legacy
`.rag-rat/index.sqlite`. rag-rat keeps using it (with a one-line deprecation notice) until you run:

```
rag-rat consolidate
```

which imports the legacy index's memories and content-addressed embedding cache into the global store,
renames the old file to `index.sqlite.imported` (delete it at your leisure), and prints a summary.
Consolidation is idempotent, and everything else in the legacy file is rebuilt on the next
`rag-rat index --full` (the carried embedding cache makes re-embedding a no-op).

If `rag-rat.toml` pins an explicit `database` key, consolidation refuses outright (nothing is
imported): remove the key first, then run `rag-rat consolidate` — the single run imports and renames
atomically, so there is never a window in which memories written to the still-live legacy file could
be lost. A pin at a **custom** path additionally needs its file moved to `.rag-rat/index.sqlite`
first (a keyless config never consults a custom path); the refusal prints the exact commands for
your paths. Once the `.imported` marker exists it acts as a stay-global latch — a stray legacy file that
later reappears beside it is ignored (with a warning), never silently adopted.

The global default also requires a **resolvable repo identity** — a git repository with at least one
commit, or an `[index] repo_id` pin. A root without one (a non-git directory, a fresh `git init`
with no commits yet) keeps its per-root `.rag-rat/index.sqlite` exactly as before the flip, so
unrelated identity-less projects never share scope in the global store; once the repo gains a first
commit (or a pin), the default resolves globally and `rag-rat consolidate` migrates it.

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

## Dream verdict model (`[llm.dream]` / `[llm.dream.remote]`)

`[llm.dream]` configures the dream verify/compaction passes' **model** — rag-rat's only
generative-model dependency. It is **out-of-process, opt-in, and gated by a deterministic layer**:
`rag-rat dream` stays 100% deterministic unless you pass `--verify`/`--compact` *and* set
`enabled = true`. The serving config lives under `[llm.dream.remote]`, mirroring
`[llm.embedding.remote]` — a **connect** endpoint (an already-running chat server) or an
**ephemeral** cookbook-provisioned GPU box.

```toml
[llm.dream]
enabled = false             # off by default — the model turn is skipped

# CONNECT mode (default when [llm.dream.remote] is omitted): a local Ollama.
[llm.dream.remote]
backend  = "ollama"
endpoint = "http://localhost:11434"     # OpenAI-compatible chat server
model    = "qwen3:4b-instruct"          # server-side model name
request_timeout_s = 300
```

For a dense evidence pack, CPU inference is pathologically slow (a large bound file can blow past the
timeout and never verify). Run the model on an **ephemeral remote GPU** instead — the same cookbook
mechanism embeddings use — by setting `cookbook` + `gpu` instead of `endpoint`:

```toml
[llm.dream.remote]
backend  = "vllm"                       # ollama | vllm  (infinity is embed-only — rejected here)
cookbook = "@rag-rat/cookbook modal"    # provision an ephemeral box (mutually exclusive with endpoint)
gpu      = "A10G"                        # provider-specific GPU class (validated at provision time)
model    = "Qwen/Qwen3-8B"              # HF id for vllm (an ollama name for the ollama backend)
auth_env = "MODAL_TOKEN"                # optional: env var holding the box's bearer token
request_timeout_s = 900
```

The box is provisioned only when there is pending work (a zero-work guard never cold-starts a paid
GPU for a fully churn-skipped repo) and is torn down when the run ends. The client speaks the
standard `/v1/chat/completions` route (temperature 0, no streaming). Run it with:

```bash
rag-rat dream --verify --max-memories 20
```

`--verify` always runs the **deterministic** verification findings (`memory_unverifiable`). When
`enabled = true`, it additionally runs the model verdict pass: for each memory in a **churn-skip
queue** (only memories whose body or bound files changed, or that were never checked — capped by
`--max-memories`), it renders a mechanical evidence pack, asks the model for a `current | diverged`
verdict (a fabricated citation is rejected, retried once, then discarded), and records the verdict in
the derived `memory_reality` table. A `diverged` verdict opens a `memory_divergence` finding for the
review flow. The model **proposes**; it never changes a memory's status. Leaving `enabled = false`
skips the model turn entirely — no network calls, deterministic findings only.

`--compact` (independent of `--verify`, and gated the same way on `[llm.dream] enabled = true`)
runs the **compaction pass**, which rewrites each un-summarized memory into a 3–4 sentence,
self-contained summary and stores it in the derived `memory_summaries` table:

```bash
rag-rat dream --compact --max-memories 20
```

It is a churn-skip queue too — a memory is (re)compacted only when it has no summary for its
**current** body (a body edit self-invalidates the old one). The summary is generated from the note
body alone (no code context, no tools — measured to give the highest fidelity), and it must pass
deterministic acceptance guards (3–4 sentences, ≤110 words, no paragraph breaks, and every tracker
reference it cites must resolve in the indexed papertrail); a failing summary is retried once, then
dropped (no row is stored, so the surface falls back to the title). The pass **never** writes a
`repo_memories` column.

### Scheduling the passes (nightly systemd timer / cron)

The verify and compaction passes are a periodic memory-maintenance chore — run them on a schedule so
verdicts and summaries stay fresh as memories churn. Two things shape a good schedule:

- **Run both passes in one invocation** — `rag-rat dream --verify --compact`. A single run provisions
  the ephemeral GPU box *once* and runs both passes against it; two separate jobs pay two cold starts
  (image pull + model load) for the same work. The zero-work guard means an idle run never boots a
  box, so scheduling generously costs nothing when there is nothing to do.
- **`--max-memories` caps *each* model pass over its churn-skip queue — it is per pass, not a shared
  total.** An up-to-date memory is skipped: `--compact` keys that on the body + prompt version, while
  `--verify` *also* re-enqueues when a memory's bound files or cited identifiers change (evidence
  drift), so a nightly run after ordinary code churn still does verify work even if no memory was
  edited. Because the cap is per pass, `--verify --compact --max-memories 200` can drive up to 200
  verifications **and** 200 summaries (~400 model calls) on a full backlog — likely more than one
  box's 30-minute max-lifetime finishes in one run, so a large initial backlog drains over several
  nights (churn-skip resumes where it left off). Size the cap for the per-pass cost you want.

A headless scheduler runs with a **bare environment**, which is where this usually goes wrong. The
run must be able to (a) find the `rag-rat` binary *and* `node`/`npx` (the cookbook recipe shells out
to `npx`), (b) reach the **provider credentials that provision the box** — for Modal, `MODAL_TOKEN_ID`
/ `MODAL_TOKEN_SECRET` in the environment or `~/.modal.toml` (so set `HOME`); these are distinct from
`[llm.dream.remote] auth_env`, which is only the *served endpoint's* bearer token (connect mode, or
the provisioned box's tunnel), not the provisioning credential — and (c) start in the repo whose
`rag-rat.toml` sets `[llm.dream] enabled = true` (the dream config is repo-local; the index store is
machine-global).

A systemd **user** timer, nightly at 04:30:

```ini
# ~/.config/systemd/user/rag-rat-dream.service
[Unit]
Description=rag-rat dream memory maintenance (verify + compact)

[Service]
Type=oneshot
WorkingDirectory=/path/to/your/repo
Environment=HOME=%h
Environment=PATH=%h/.cargo/bin:/path/to/node/bin:/usr/bin:/bin
ExecStart=%h/.cargo/bin/rag-rat dream --verify --compact --max-memories 200
TimeoutStartSec=60min
```

```ini
# ~/.config/systemd/user/rag-rat-dream.timer
[Unit]
Description=Nightly rag-rat dream memory maintenance

[Timer]
OnCalendar=*-*-* 04:30:00
# Catch up on the next boot if the machine was off/asleep at 04:30.
Persistent=true

[Install]
WantedBy=timers.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now rag-rat-dream.timer
systemctl --user list-timers rag-rat-dream.timer   # confirm the next run
loginctl enable-linger "$USER"                      # REQUIRED to fire the timer while logged out
```

`loginctl enable-linger` is easy to forget and load-bearing: without it the user systemd instance is
torn down when your last session ends, and an overnight timer never fires. The **plain-cron
equivalent** is the same command wrapped so `PATH`/`HOME`/cwd are set (cron's environment is even
barer than systemd's).

Either way the run is unattended, so teardown reliability matters. On **Modal**, the cookbook box
carries its own idle + max-lifetime backstops, so a crashed run or a flaked teardown still
self-terminates the box rather than billing a GPU indefinitely. On **RunPod** there is **no
provider-side backstop** — an on-demand pod stops only when the recipe's teardown runs — so a headless
RunPod schedule depends entirely on clean teardown; prefer the Modal cookbook for unattended runs, or
add your own external watchdog.

### Reviewing findings (`rag-rat dream <id> --accept|--dismiss|--reset`)

The worklist `rag-rat dream` prints carries a stable `id` per finding. Dream only *proposes*; a
human (or strong agent) resolves a finding by that id — a full id or an unambiguous prefix
(git-style):

```bash
rag-rat dream                     # print the open worklist (each row has an id + status)
rag-rat dream 3f9a1c22 --accept   # acknowledge it (real, action pending)
rag-rat dream 3f9a1c22 --dismiss  # a false positive / won't-fix — hidden from the worklist
rag-rat dream 3f9a1c22 --reset    # undo a verdict, back to open
rag-rat dream --all               # also list the accepted/dismissed findings (to find one to --reset)
```

A verdict is **preserved across future runs**: a re-run that still reports the finding keeps your
`accepted`/`dismissed` decision, while a finding the code makes obsolete resolves on its own. Only an
`open`/`accepted`/`dismissed` finding is reviewable — a `resolved`/`superseded` one is not (the code
moved on, so there is nothing to act on). Reviewing is repo-scoped and never runs the model.

The same worklist and review actions are available over MCP for a pull-based strong agent: the
`dream` tool returns the ranked worklist (`{ limit?, all? }`; recomputes the deterministic findings
like `rag-rat dream`, but never the opt-in model passes), and `dream_review` applies a verdict
(`{ finding, verdict: "accept" | "dismiss" | "reset" }`).

## Memory surfacing (`[memory] surface`)

`[memory] surface` controls how memory attachments and memory-query results render. The default is
`summary`: memory attachments (and `memory_search` / `memory_for_call_path` / `memory_for_edges`
hits) show the dream-compacted summary plus a plain-text verdict marker (`[verdict: diverged]` /
`[verdict: current @<short-commit>]`) in place of the full body, for each memory that has one, and
fall back to the title alone until one is generated. Set it to `full` to restore whole bodies
everywhere. Either way, `memory show` / `memory_show` always returns the full body — that is the
deliberate "expand on request" path.

```toml
[memory]
surface = "summary"   # default; or "full" — return whole bodies everywhere
```

`summary` applies across every drive-by renderer: `impact_surface`'s compact `repo_memories`, the
`symbol_lookup` / `find_callers` / `trace_callees` attached memories, `read_chunk`'s memories,
`memory_for_symbol` / `memory_for_path` (which keep the full binding/call-path structure but defer
the body), and the grep-augmentation hook context. A memory with no summary for its current body
falls back to title-only. `memory show` / `memory_show` **always** return the full body, regardless
of this setting — it is the expand path.

## Issue trackers (`[[tracker]]`) and papertrail sync (`[papertrail]`)

`[[tracker]]` binds the repo to the issue tracker(s) whose items annotate its papertrail. It is
list-valued: one repo can bind a code host **and** Jira at the same time.

```toml
[[tracker]]
provider = "github"            # github | gitlab | bitbucket | jira
# project = "owner/repo"       # default: derived from the git remote; jira: the project KEY
# remote = "origin"            # which git remote to derive `project` from
# base_url = "https://gitlab.example.com"   # self-hosted / enterprise instance
# auth = { env = "GITLAB_TOKEN" }           # or { token_command = "glab auth token" }
# tags = ["bug", "perf"]       # tracked tags/labels; empty or missing = track ALL
```

**Zero-config default.** With no `[[tracker]]` at all, the binding is auto-detected from the git
`origin` remote URL (ssh and https forms alike): a `github.com` remote binds GitHub, a `gitlab.*`
host (gitlab.com or self-hosted — the base URL is taken from the host) binds GitLab, and
`bitbucket.org` binds Bitbucket. Most repos never need a `[[tracker]]` section. Writing one
**replaces** auto-detection entirely.

Per-key notes:

- `provider` — required, one of `github` / `gitlab` / `bitbucket` / `jira`.
- `project` — optional for the code hosts (derived from the git remote named by `remote`,
  default `origin`); GitLab keeps full subgroup paths (`group/sub/repo`). **Required for Jira**
  (the project key, e.g. `PROJ`): Jira is not a code host, so it is never auto-detected and has
  no remote to derive from.
- `base_url` — self-hosted / enterprise instances (`https://gitlab.example.com`). Must be an
  http(s) URL without inline credentials. Self-hosted GitHub Enterprise and Bitbucket instances
  are configured this way — only the three cloud hosts auto-detect.
- `auth` — exactly one of `env` (the NAME of an env var holding the token — never the token
  itself) or `token_command` (a command printing the token to stdout). Omitted = anonymous /
  provider-native fallback.
- `tags` — the tracked subset, OR-matched case-insensitively against the provider's label-like
  facets (labels on GitHub/GitLab; labels + components on Jira; kind + component on Bitbucket
  issues). Empty or missing tracks everything; item kinds with no tag facet (e.g. Bitbucket pull
  requests) are always tracked. The normalized list is fingerprinted into the sync cursor, so
  widening the list restarts the backfill and narrowing prunes.

Ref grammar per provider (what the annotation layer recognizes in commits, files, and branch
names): GitHub `#N` / `GH-N` / `owner/repo#N` / issue+PR URLs; GitLab `#N` (issues), `!N` (merge
requests), `namespace/path#N` / `!N` with subgroups, and `/-/issues/N` / `/-/merge_requests/N`
URLs; Bitbucket `#N` (issues) and `/issues/N` / `/pull-requests/N` URLs; Jira bare keys
(`PROJ-123`) for the bound project. A bare `#N` resolves against the first code-host binding
only. A self-hosted binding's URL grammar matches its own host, not the cloud host.

All bindings participate in production reference discovery, annotation lookup, and manual mirror
dispatch. GitHub bindings use the native client now; GitLab, Bitbucket, and Jira bindings report a
provider-client-pending result until their provider PRs land, and are never sent through the GitHub
client. The shared runner keeps every result, cursor, auth source, and quota governor scoped to its
binding. A repository may have at most one resolved binding for a given `(provider, project)`:
the mirror rejects duplicates before dispatch because API origin and tag filters are not part of
the persisted item/cache cursor identity. Use one binding and one combined tag set for that
provider project.

The shared transport keeps part of each header-reported quota untouched for the user's own tools.
The reserve defaults to 35% and can be changed independently of provider clients:

```toml
[papertrail]
rate_limit_reserve = 0.35   # finite fraction in 0.0 <= value < 1.0
```

The provider mirror scheduler accepts three cadence keys. Decisions are per binding: attempts are
coalesced by the minimum interval, lightweight probes run on the probe cadence, and a full walk
periodically heals historical drift. Persisted rate-limit horizons and unfinished cursor work take
priority over these ordinary cadences.

```toml
[papertrail]
probe_interval_secs = 900       # default: 15 minutes
sync_min_interval_secs = 900    # default: 15 minutes between attempts
full_sync_interval_secs = 86400 # default: daily healing walk
```
