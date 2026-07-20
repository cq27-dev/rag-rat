# @rag-rat/cookbook

Ephemeral remote-runtime provisioning recipes for [rag-rat](https://github.com/cq27-dev/rag-rat). A **recipe** is a standalone
Node program that rag-rat spawns as a subprocess: it provisions a remote box serving an embedding
model (ollama, [michaelfeil/infinity](https://github.com/michaelfeil/infinity), or
[vLLM](https://docs.vllm.ai) — chosen per run), streams typed JSONL events to stdout (handing rag-rat
the endpoint via a `ready` event), holds the box while rag-rat works, then tears it down on signal.

This is the JS/TS half of rag-rat's remote-runtime rework. rag-rat (Rust) owns the
lifecycle and the embedding traffic; the cookbook owns provider provisioning.

> **Security — a cookbook spec must be TRUSTED.** A recipe runs with your full privileges and
> credentials (Modal/RunPod tokens in the env, your shell, your network) and provisions paid cloud
> compute. rag-rat invokes it via `npx`, which **downloads and runs whatever the configured spec
> resolves to** — `npx -y <name>` runs arbitrary downloaded code as you. Pin and trust the cookbook
> spec like any dependency; never point rag-rat at an untrusted package name or a recipe path you
> have not read.

## Provider × backend

Two axes, kept orthogonal:

- **Provider** (`modal`, `runpod`) — *where* the box runs. Selected by the subcommand
  (`cookbook modal` / `cookbook runpod`). Owns the box lifecycle: create, tunnel/proxy, teardown,
  leak-safety. One recipe file per provider (`recipes/modal.mts`, `recipes/runpod.mts`).
- **Backend** (`ollama`, `infinity`, `vllm`) — *what* serves the model. Selected by the
  `backend` field in `CookbookInput` (NOT a subcommand), so a single provider recipe serves all
  three. The per-backend differences — image, launch command, port, embeddings route, model-load —
  are declared in `recipes/backends.mts`.

All three backends speak the **OpenAI-compatible embeddings API** (`POST <embedPath>` with `{model,
input:[…], encoding_format:"float"}` → `{data:[{embedding,index}]}`), so the embed call and the
readiness probe are backend-independent. Only the **route** differs:

| Backend | Image (pinned, `#689`) | Port | Embeddings route | GPU | Model load |
|---|---|---|---|---|---|
| `ollama` | `ollama/ollama:0.32.1@sha256:6345fbc18bd73a1e16404be681dbc6fd291a027cab43ed541abe78c4c81051b0` | 11434 | `/v1/embeddings` | optional | `ollama pull` after boot |
| `infinity` | `michaelf34/infinity:0.0.77@sha256:11e8b3921b9f1a58965afaad4a844c435c9807cbc82c51e47cb147b7d977fc88` (GPU) / `michaelf34/infinity:latest-cpu@sha256:161ef2dd48ba050ad29a413d671270502acb4ad67759d695eb0d325c033a935d` (CPU) | 7997 | `/embeddings` | optional | auto-download on boot |
| `vllm` | `vllm/vllm-openai:v0.25.1@sha256:e4f88a835143cd22aee2397a26ec6bb80b3a4a6fe0c882bcbc63822904766089` | 8000 | `/v1/embeddings` | **required** | auto-download on boot |

Images are pinned `tag@digest` (see `recipes/backends.mts`) — an unpinned `:latest` let upstream image
bumps change server behavior with no change on our side; bump the pin deliberately, never revert to
`:latest`.

`model` is backend-specific: an **ollama model name** (`all-minilm`) for ollama, a **HuggingFace
model id** (`sentence-transformers/all-MiniLM-L6-v2`) for infinity/vLLM (which download it from HF on
start). The `embedPath` for each backend **must mirror `RemoteBackend::embed_path` on the Rust
side** — the recipe's verification probe and rag-rat's real embed call have to hit the same URL.

## The process contract

rag-rat and a recipe communicate over a strict process protocol. **stdout is a typed JSONL event
stream** — one JSON object per line — which a future ratatui `log` view renders live. The
Rust parser is built against the exact event shapes below; implement them precisely.

| Stage | rag-rat | recipe |
|---|---|---|
| spawn | runs `node recipe.mjs` (or `npx tsx recipe.mts`), with `RAG_RAT_COOKBOOK_INPUT` set in env | starts up |
| input | sets `RAG_RAT_COOKBOOK_INPUT` = JSON `CookbookInput`; provider creds already in env / `~/.modal.toml` | reads + parses that env var |
| progress | reads `status` / `log` events from **stdout** | emits `status` (provisioning → pulling → verifying) and `log` events as it works |
| ready | reads the `ready` event → uses `endpoint` (+ optional `auth_token`) | once the box is up **and serving**, emits one `ready` event |
| liveness | uses the endpoint | stays running, holding the box |
| teardown | sends `SIGTERM` (or `SIGINT`) | emits a `tearing_down` status, destroys the box, exits `0` |
| failure | sees an `error` event + non-zero exit, no `ready` | on provisioning failure: emits an `error` event, exits non-zero, **before** any `ready` |

Hard rules:

- **stdout carries ONLY JSONL events** — one `CookbookEvent` per line, nothing else. Use `log(level,
  message)` / `ctx.status(phase, detail)` / the `emit()` helper; never `console.log`. **stderr is
  for genuine crashes only** (an uncaught throw).
- **`ready` only after serving.** Don't emit `ready` until the box answers a real request — the
  recipes probe the backend's OpenAI embeddings route (`verifyEmbed`) and wait for a vector.
- **Stay alive after `ready`** until signaled.
- **Tear down on `SIGTERM`/`SIGINT`** (a `tearing_down` status precedes it), then `exit(0)`.
- **Bound EVERY await — fetch AND SDK call.** Anything that can stall past the Rust-side grace gets
  SIGKILLed with the box still up → a leaked, billed box. For fetches use `pollUntil` /
  `fetchWithTimeout` (`AbortSignal.timeout`); for an SDK await with no native timeout (Modal's
  `sandboxes.create`/`exec`/`tunnels`, `terminate`) wrap it in `withBudget(deadline, label, work)`
  (races the remaining provisioning budget) or `raceWithTimeout(work, ms, label)` (a fixed wall, for
  teardown). Never leave a bare unbounded `await` on a provisioning or teardown path.
- **Use a provider-side backstop where one exists** (Modal's `timeoutMs: 1_800_000` self-destructs
  a leaked box). Where the provider has none — RunPod on-demand pods have no idle/lifetime auto-stop
  — **reliable teardown is the only thing that stops billing**, which is why teardown is
  timeout-bounded to finish inside the grace.

### `CookbookInput` (env var `RAG_RAT_COOKBOOK_INPUT`)

```jsonc
{
  "model": "all-minilm",       // required: non-empty string — the model to serve. An ollama model
                               //   name for the ollama backend; a HuggingFace model id for
                               //   infinity/vLLM (e.g. "sentence-transformers/all-MiniLM-L6-v2").
  "backend": "ollama",         // optional: "ollama" | "infinity" | "vllm". Omitted/null → "ollama".
                               //   Selects the image/launch/port/route; the embed wire call is
                               //   identical across all three.
  "provision_timeout_s": 280,  // optional: positive number — wall-clock budget for the WHOLE
                               //   provisioning sequence (box boot + model download/pull + first
                               //   serving response). A remote cold start takes MINUTES; rag-rat
                               //   sends ~280s. THIS is the recipes' poll-loop budget. Omitted/null
                               //   → the recipe's default.
  "request_timeout_s": 30,     // optional: positive number — the Rust embedder's per-HTTP-request
                               //   timeout, passed through for completeness. NOT a provisioning
                               //   budget (~30–60s is far too short to boot a box).
  "gpu": null,                 // optional: string or null. For Modal this is a Modal GPU request
                               //   string like "A10G", "L40S", "H100" (CPU default for ollama/
                               //   infinity; a GPU is defaulted in for vLLM); for RunPod a gpuTypeId
                               //   like "NVIDIA RTX A4000".
  "num_ctx": null,             // optional: positive integer — the OLLAMA context window baked into
                               //   the box (OLLAMA_CONTEXT_LENGTH), so chunk vectors match rag-rat's
                               //   freshness key. Ignored by infinity/vLLM. Omitted/null → default.
  "server_concurrency": 32     // optional: positive integer — server-side request parallelism.
                               //   rag-rat sends the user's `[remote] concurrency` CAP; each backend
                               //   maps it (ollama → OLLAMA_NUM_PARALLEL; vLLM → --max-num-seqs;
                               //   infinity ignores it). rag-rat then auto-tunes the actual client
                               //   fan-out (within the cap) itself, against the box, after `ready`.
}
```

The provisioning budget is **`provision_timeout_s`**, never `request_timeout_s` — conflating the
two (using the ~30–60s per-request timeout as the boot budget) is what made the first live e2e time
out before the box finished booting. `readInput` validates each field up front (a string timeout
would otherwise become `NaN` and break the poll loops; an unknown `backend` is a hard error); a
malformed input emits an `error` event and `exit(1)`, no `ready`.

### The stdout event stream (`CookbookEvent`)

stdout is JSONL — one of these objects per line. Every event carries `ts` (epoch ms).

```jsonc
// progress: lifecycle phase. provider ∈ {"modal","runpod"}; phase ∈
//   {"provisioning","pulling","verifying","tearing_down"}
{"type":"status","phase":"pulling","provider":"runpod","detail":"pulling model …","ts":1782489011404}

// diagnostics: level ∈ {"info","warn","error"}
{"type":"log","level":"info","message":"pod deployed: abc123","ts":1782489011405}

// the box is serving. Carries only the endpoint and an optional auth token; rag-rat does the
// throughput tuning itself after this event.
{"type":"ready","endpoint":"https://abc123.modal.host","auth_token":null,"ts":1782489011405}

// provisioning failed before `ready`
{"type":"error","message":"provisioning failed: …","ts":1782489025078}
```

`auth_token` on `ready` is `null` when the tunnel is open (no token needed); a missing token is
normalized to explicit `null`. A run emits zero-or-more `status`/`log` events, then **either** one
`ready` (success — followed later by a `tearing_down` status on signal) **or** one `error` (failure,
with exit 1 and no `ready`).

### Throughput tuning lives in rag-rat, not the recipe

The recipe is a **dumb provisioner**: it boots the box, maps `server_concurrency` to the backend's
own knob, verifies the embeddings route serves, and emits `ready`. It does **no** throughput
sweeping.

rag-rat owns the tuning. After `ready`, rag-rat runs a short client-concurrency micro-sweep against
the box **with its real `OpenAiEmbedder`** — the same request shape (batch size, char budget) that
reconcile will use — so the measured knee can't drift from real load. The tuned knee is a
**hard-clamped-to-cap** client fan-out: rag-rat treats `[remote] concurrency` as a ceiling it
optimizes *within* and never exceeds. The result is cached in the index's `index_meta` (keyed by
backend + provider + GPU + model + request shape) with a TTL, so routine re-provisions reuse it
without re-sweeping. See rag-rat's tuning env knobs, not any cookbook-side ones.

## Invoking — provider as a subcommand, backend in the input

The published form is **one package, provider as a subcommand**, via the package bin:

```bash
npx @rag-rat/cookbook modal     # provision on Modal
npx @rag-rat/cookbook runpod    # provision on RunPod
```

The bin (`dist/cli.mjs`) reads the provider from `argv[2]` and dynamically imports the matching
recipe (`recipes/<provider>.mjs`). The **backend** (ollama/infinity/vLLM) arrives in
`RAG_RAT_COOKBOOK_INPUT.backend`, not the subcommand. Recipes self-run on import, so the dispatcher
adds only routing — no contract logic. An unknown or missing provider emits an `error` event listing
the available providers and exits `1` (no `ready`). The recipe files stay **directly runnable** too,
which is what rag-rat actually spawns:

```bash
node dist/recipes/modal.mjs     # equivalent to `cookbook modal`
node dist/recipes/runpod.mjs    # equivalent to `cookbook runpod`
```

## Recipes

| Recipe | Provider | Subcommand | Entry (post-build) |
|---|---|---|---|
| `recipes/modal.mts` | [Modal](https://modal.com) Sandboxes | `modal` | `dist/recipes/modal.mjs` |
| `recipes/runpod.mts` | [RunPod](https://runpod.io) GPU pods | `runpod` | `dist/recipes/runpod.mjs` |

Each provider recipe owns only the box lifecycle; the backend spec (`recipes/backends.mts`) supplies
image, entrypoint args, env, port, embeddings route, and model-load strategy.

### modal

Provisions a [Modal Sandbox](https://modal.com/docs/guide/sandboxes) from the backend's image, serves
the model, and tunnels the backend's port.

Provider gotchas baked into the recipe (each one cost real time to find):

- Modal Sandbox `command` is passed as **entrypoint args** for registry images — the backend spec
  supplies exactly those (never the entrypoint itself: ollama's is `/bin/ollama` so args are
  `["serve"]`; infinity's is `infinity_emb` so args start `["v2", …]`; vLLM's is `vllm serve` so the
  first arg is the model id). A TCP readiness probe waits for the port before pulling/verifying.
- The box must listen on `0.0.0.0` for the tunnel to reach it — ollama needs `OLLAMA_HOST` (spec
  env); infinity binds `0.0.0.0` by default; vLLM gets an explicit `--host 0.0.0.0`.
- **GPU is off by default for ollama/infinity** (and, for ollama, must attach before its discovery
  watchdog times out or the box dies exit 137 at cold start). vLLM's image is CUDA-only, so when the
  backend `requiresGpu` and the caller passed none, the recipe defaults a GPU in.
- infinity and vLLM mount a persistent, model-scoped Modal Volume at `HF_HOME`. Later Sandboxes for
  the same model reuse downloaded Hugging Face weights instead of paying the full cold-download cost.
  Modal commits Volume changes in the background and on Sandbox shutdown. Because the Modal v1
  Volume API has last-writer-wins semantics, the recipe excludes concurrent mounts of one model Volume.
  infinity CPU boxes explicitly use the torch engine: the image's optimum/OpenVINO default writes
  generated `infinity_onnx` artifacts into the Hub cache that are not safe to reuse across Sandboxes.
- A successful load is published as an empty deterministic marker Volume only after verified serving
  and termination is confirmed through a reattached polling handle, so a marker cannot outrun the
  model Volume's final commit. Markers are advisory: Hugging Face keeps its normal metadata and cache
  recovery behavior, so a moving `main` revision or damaged cache can still refresh.
- vLLM's AOT/Inductor cache is persisted under a namespace keyed by model, pinned image digest, GPU,
  capability, and launch args. A separately published compile marker records a completed cache; vLLM
  still retains its normal recompile fallback so moving model revisions or damaged artifacts self-heal.
  A deterministic, owner-tagged Sandbox name excludes concurrent writable mounts of the same model
  Volume; a concurrent request fails with an explicit retry message instead of risking a v1 lost update.
  Marker names include Modal's opaque Volume ID, so deleting and recreating a cache cannot leave a
  stale marker attached to the replacement Volume.
- The Sandbox main process's stdout/stderr is drained from creation through teardown. A bounded,
  redacted prefix is forwarded as typed `log` events, and readiness failures include a bounded recent
  tail plus the Sandbox exit code when available. Raw container output never bypasses the JSONL event
  contract.
- `timeoutMs: 1_800_000` (30 min) is the provider-side lifetime backstop.

Auth comes from the ambient env (`MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET`) or `~/.modal.toml` —
rag-rat does not pass Modal creds through `RAG_RAT_COOKBOOK_INPUT`.

### runpod

Provisions an ephemeral **GPU pod** on [RunPod](https://runpod.io) from the backend's image and
serves the model on the backend's port. RunPod proxies that HTTP port at
`https://<podId>-<port>.proxy.runpod.net` — that proxy URL is the endpoint.

It talks to RunPod's **GraphQL API directly over `fetch`** (`podFindAndDeployOnDemand` to deploy,
`podTerminate` to tear down). The `runpod-sdk` npm package is deliberately **not** a dependency: it
only covers serverless endpoints (`run`/`runSync`/`status`/`stream`/`health`) and has no
pod-management surface.

Provider gotchas baked into the recipe:

- `dockerArgs` is the container CMD, **appended to the image's baked entrypoint** — the backend spec
  supplies exactly the args (ollama's entrypoint is `/bin/ollama` so args are `serve`; infinity's is
  `infinity_emb` so args start `v2 …`; vLLM's is `vllm serve` so the first arg is the model id).
- The server must listen on `0.0.0.0` for the proxy to reach it — ollama needs `OLLAMA_HOST` (spec
  env); infinity binds `0.0.0.0` by default; vLLM gets `--host 0.0.0.0`.
- There is **no in-pod exec** in `podFindAndDeployOnDemand`. For **ollama** (which boots empty) the
  model is pulled **client-side** over the proxy URL (`POST /api/pull`, non-streaming, retried until
  the pod finishes booting). infinity/vLLM auto-download the HF model on boot — no pull step. Then
  the embeddings route is probed before the `ready` event.
- **No provider-side backstop.** RunPod's `podFindAndDeployOnDemand` has **no** field that
  auto-stops or auto-terminates an on-demand GPU pod after a duration or after idle (`idleTimeout`
  is a serverless/flex-worker concept, not an on-demand-pod control — verified against the RunPod
  GraphQL spec). So **reliable teardown is the only thing that stops the billing**: every fetch is
  `AbortSignal`-bounded, and `podTerminate` runs under a tight (~8 s) timeout to finish inside the
  Rust-side grace.
- **Deploy-timeout orphan sweep.** The pod is deployed with a unique name (`rag-rat-cookbook-<ts>`).
  If the deploy call throws (e.g. RunPod created the pod but the response was lost/slow) before the
  pod handle is reported, the recipe queries `myself { pods }`, terminates any pod matching that
  name, then rethrows — so a created-but-unacknowledged pod can't bill forever. Best-effort and
  bounded; it only logs, never masks the original deploy error.
- Default GPU is a cheap `NVIDIA RTX A4000`; `input.gpu` is a `gpuTypeId` override.

Auth: `RUNPOD_API_KEY` in the process env — the recipe emits an `error` event and exits `1` (no
`ready`) if it's unset. rag-rat does not pass it through `RAG_RAT_COOKBOOK_INPUT`.

## Build & run

```bash
npm install
npm run build        # tsc → dist/ (bin → dist/cli.mjs, recipes → dist/recipes/*.mjs, lib → dist/src/contract.js)
npm run typecheck    # tsc --noEmit

# via the bin dispatcher (the published form):
RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' node dist/cli.mjs modal
RUNPOD_API_KEY=… RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' node dist/cli.mjs runpod

# or a recipe directly (what rag-rat spawns), with a non-default backend:
RAG_RAT_COOKBOOK_INPUT='{"model":"sentence-transformers/all-MiniLM-L6-v2","backend":"infinity"}' node dist/recipes/modal.mjs

# dev (no build step), via tsx:
RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' npx tsx recipes/runpod.mts
```

The recipe runs until you `SIGTERM`/`SIGINT` it (Ctrl-C), at which point it tears down the box.

## Writing your own recipe

A recipe is a `.mts` file that hands a `Recipe<H>` to `runRecipe` from `@rag-rat/cookbook`. The
harness owns all the contract plumbing **and** the terminate-on-error wrapper and the
provision-timeout clamp, so a recipe is just: provision, report the box via `ctx.onBox`, emit
progress via `ctx.status`, verify, and return. Select the backend spec with `selectBackendSpec` so
one recipe serves every backend.

```ts
import {
  type ProvisionContext, type Provisioned, type Recipe,
  runRecipe, verifyEmbed, log,
} from "@rag-rat/cookbook"; // or "../src/contract.js" in-repo
import { selectBackendSpec } from "./backends.mjs";

async function provision(ctx: ProvisionContext<MyBox>): Promise<Provisioned<MyBox>> {
  const { input, provisionTimeoutMs } = ctx;        // already clamped from provision_timeout_s
  const spec = selectBackendSpec(input.backend ?? "ollama");
  ctx.status("provisioning", "creating box");       // status events tag the recipe's provider
  const box = await myProvider.spawn({
    image: spec.image(input),
    command: [...spec.entrypointArgs(input)],
    env: spec.env(input),
    port: spec.port,
  });
  ctx.onBox(box);                                   // report NOW → runRecipe tears down if we throw
  log("info", `box ${box.id} created`);             // log(level, message) → a `log` event
  ctx.status("verifying", `probing ${spec.embedPath}`);
  await verifyEmbed(box.url, {
    model: input.model,
    embedPath: spec.embedPath,
    budgetMs: provisionTimeoutMs,
  });
  return { handle: box, endpoint: box.url, auth_token: null };
}

async function teardown(box: MyBox) {
  await box.destroy();
}

const recipe: Recipe<MyBox> = {
  provider: "modal",              // tags every status event (and the tearing_down one)
  defaultProvisionTimeoutS: 600,  // budget when input.provision_timeout_s is omitted/null
  provision,
  teardown,
};

await runRecipe(recipe);
```

What the harness guarantees so you don't have to:

- reads + validates `RAG_RAT_COOKBOOK_INPUT`, resolves `provisionTimeoutMs` (clamp + default);
- installs `SIGTERM`/`SIGINT` handlers;
- runs **exactly one** teardown on every path — provision throws (after `onBox`), or the box serves
  and a signal arrives later — via an idempotent latch (a `tearing_down` status precedes it). You no
  longer write `try { … } catch { terminate; throw }` in the recipe;
- emits the `ready` event on success and an `error` event on failure; parks the process; exits `0`
  on signal.

Emit your own progress with **`ctx.status(phase, detail)`** (provider pre-bound) and diagnostics
with **`log(level, message)`** — both go out as JSONL events. Never `console.log` (it corrupts the
stdout stream). Call `ctx.onBox(handle)` the instant the box exists (before it's verified serving) —
that is what lets the harness clean up a half-provisioned box.

The contract module also exports the shared bounding helpers — call these instead of a bare `fetch`
or a bare SDK `await`:

- **`verifyEmbed(endpoint, { model, embedPath, budgetMs, headers? })`** — poll
  `<endpoint><embedPath>` with the OpenAI embeddings request until a real vector comes back. Call it
  before returning from `provision` (a reachable box is not a serving box). `embedPath` comes from
  the backend spec.
- **`pollUntil(url, { label, body, isReady, budgetMs, pollIntervalMs?, perAttemptTimeoutMs? })`** —
  the generic retry-until-ready loop. Each attempt is `AbortSignal`-bounded AND clamped to the
  remaining budget, so no attempt runs past the deadline. `verifyEmbed` and the RunPod model pull
  are both built on it.
- **`fetchWithTimeout(url, init, timeoutMs)`** — a single `fetch` with a hard `AbortSignal.timeout`.
- **`withBudget(deadline, label, work)`** — race an SDK await (no native timeout) against the
  provisioning budget remaining until `deadline`; throws (naming the step) if the budget runs out,
  so a hang aborts into teardown instead of being SIGKILLed. Throws immediately if <1s remains.
- **`raceWithTimeout(work, ms, label)`** — race an await against a fixed `ms` wall (used to bound
  teardown's `terminate`); the timer self-clears the instant `work` settles.
- **`assertBudgetRemaining(deadline, label)` / `remainingBudgetMs(deadline)`** — compute/guard the
  one shared provisioning deadline that `verifyEmbed`/`pullModel` budgets derive from.

To add a provider, drop a `recipes/<name>.mts` and register it in the `PROVIDERS` map in `cli.mts`.
To add a backend, add a `BackendServerSpec` to `recipes/backends.mts` and extend the `Backend` union
in `src/contract.ts` (keep it in sync with `RemoteBackend` on the Rust side).

## Layout

```
cookbook/
  package.json                 @rag-rat/cookbook — bin → dist/cli.mjs; deps: modal; dev: typescript, tsx
  tsconfig.json                strict, ESM (NodeNext)
  cli.mts                      the bin dispatcher: `cookbook <provider>` → imports the recipe (→ dist/cli.mjs)
  src/contract.ts              the published contract: CookbookEvent + emit/log/status + runRecipe() + pollUntil/verifyEmbed/fetchWithTimeout (→ dist/src/contract.js)
  recipes/backends.mts         per-backend server specs (image/launch/port/route/model-load)  (→ dist/recipes/backends.mjs)
  recipes/modal.mts            the Modal provider recipe        (→ dist/recipes/modal.mjs)
  recipes/runpod.mts           the RunPod provider recipe       (→ dist/recipes/runpod.mjs)
  README.md                    this file
```
