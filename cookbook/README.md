# @rag-rat/cookbook

Ephemeral remote-runtime provisioning recipes for [rag-rat](https://github.com/cq27-dev/rag-rat). A **recipe** is a standalone
Node program that rag-rat spawns as a subprocess: it provisions a remote box (e.g. an Ollama
embedding server), streams typed JSONL events to stdout (handing rag-rat the endpoint via a
`ready` event), holds the box while rag-rat works, then tears it down on signal.

This is the JS/TS half of rag-rat's remote-runtime rework. rag-rat (Rust) owns the
lifecycle and the embedding traffic; the cookbook owns provider provisioning.

> **Security — a cookbook spec must be TRUSTED.** A recipe runs with your full privileges and
> credentials (Modal/RunPod tokens in the env, your shell, your network) and provisions paid cloud
> compute. rag-rat invokes it via `npx`, which **downloads and runs whatever the configured spec
> resolves to** — `npx -y <name>` runs arbitrary downloaded code as you. Pin and trust the cookbook
> spec like any dependency; never point rag-rat at an untrusted package name or a recipe path you
> have not read.

## The process contract

rag-rat and a recipe communicate over a strict process protocol. **stdout is a typed JSONL event
stream** — one JSON object per line — which a future ratatui `log` view renders live. The
Rust parser is built against the exact event shapes below; implement them precisely.

| Stage | rag-rat | recipe |
|---|---|---|
| spawn | runs `node recipe.mjs` (or `npx tsx recipe.mts`), with `RAG_RAT_COOKBOOK_INPUT` set in env | starts up |
| input | sets `RAG_RAT_COOKBOOK_INPUT` = JSON `CookbookInput`; provider creds already in env / `~/.modal.toml` | reads + parses that env var |
| progress | reads `status` / `log` events from **stdout** | emits `status` (provisioning → pulling → verifying) and `log` events as it works |
| ready | reads the `ready` event → uses `endpoint` | once the box is up **and serving**, emits one `ready` event |
| liveness | uses the endpoint | stays running, holding the box |
| teardown | sends `SIGTERM` (or `SIGINT`) | emits a `tearing_down` status, destroys the box, exits `0` |
| failure | sees an `error` event + non-zero exit, no `ready` | on provisioning failure: emits an `error` event, exits non-zero, **before** any `ready` |

Hard rules:

- **stdout carries ONLY JSONL events** — one `CookbookEvent` per line, nothing else. Use `log(level,
  message)` / `ctx.status(phase, detail)` / the `emit()` helper; never `console.log`. **stderr is
  for genuine crashes only** (an uncaught throw).
- **`ready` only after serving.** Don't emit `ready` until the box answers a real request (the
  recipes probe `/api/embed` and wait for a vector).
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
  "model": "all-minilm",       // required: non-empty string — the embedding model to pull + serve
  "provision_timeout_s": 280,  // optional: positive number — wall-clock budget for the WHOLE
                               //   provisioning sequence (box boot + model pull + first serving
                               //   response). A remote cold start takes MINUTES; rag-rat sends
                               //   ~280s. THIS is the recipes' poll-loop budget. Omitted/null →
                               //   the recipe's default.
  "request_timeout_s": 30,     // optional: positive number — the Rust embedder's per-HTTP-request
                               //   timeout, passed through for completeness. NOT a provisioning
                               //   budget (~30–60s is far too short to boot a box).
  "gpu": null,                 // optional: string or null. For Modal this is a Modal GPU request
                               //   string like "A10", "L40S", "H100", or "B200+" (CPU default);
                               //   for RunPod a gpuTypeId like "NVIDIA RTX A4000".
  "ollama_num_parallel": 32    // optional: positive integer — sets OLLAMA_NUM_PARALLEL in the
                               //   remote Ollama container, normally matching rag-rat's remote
                               //   embedding concurrency.
}
```

The provisioning budget is **`provision_timeout_s`**, never `request_timeout_s` — conflating the
two (using the ~30–60s per-request timeout as the boot budget) is what made the first live e2e time
out before the box finished booting. `readInput` validates each field up front (a string timeout
would otherwise become `NaN` and break the poll loops); a malformed input emits an `error` event and
`exit(1)`, no `ready`.

### The stdout event stream (`CookbookEvent`)

stdout is JSONL — one of these objects per line. Every event carries `ts` (epoch ms).

```jsonc
// progress: lifecycle phase. provider ∈ {"modal","runpod"}; phase ∈
//   {"provisioning","pulling","verifying","tearing_down"}
{"type":"status","phase":"pulling","provider":"runpod","detail":"pulling model …","ts":1782489011404}

// diagnostics: level ∈ {"info","warn","error"}
{"type":"log","level":"info","message":"pod deployed: abc123","ts":1782489011405}

// the box is serving — REPLACES the old bare handshake line
{"type":"ready","endpoint":"https://abc123.modal.host","auth_token":null,"ts":1782489011405}

// provisioning failed before `ready`
{"type":"error","message":"provisioning failed: …","ts":1782489025078}
```

`auth_token` on `ready` is `null` when the tunnel is open (no token needed); a missing token is
normalized to explicit `null`. A run emits zero-or-more `status`/`log` events, then **either** one
`ready` (success — followed later by a `tearing_down` status on signal) **or** one `error` (failure,
with exit 1 and no `ready`).

## Invoking — provider as a subcommand

The published form is **one package, provider as a subcommand**, via the package bin:

```bash
npx @rag-rat/cookbook modal     # provision on Modal
npx @rag-rat/cookbook runpod    # provision on RunPod
```

The bin (`dist/cli.mjs`) reads the provider from `argv[2]` and dynamically imports the matching
recipe (`recipes/<provider>-ollama.mjs`). Recipes self-run on import, so the dispatcher adds only
routing — no contract logic. An unknown or missing provider emits an `error` event listing the
available providers and exits `1` (no `ready`). The recipe files stay **directly runnable** too,
which is what rag-rat actually spawns:

```bash
node dist/recipes/modal-ollama.mjs     # equivalent to `cookbook modal`
node dist/recipes/runpod-ollama.mjs    # equivalent to `cookbook runpod`
```

## Recipes

| Recipe | Provider | Subcommand | Entry (post-build) |
|---|---|---|---|
| `recipes/modal-ollama.mts` | [Modal](https://modal.com) Sandboxes | `modal` | `dist/recipes/modal-ollama.mjs` |
| `recipes/runpod-ollama.mts` | [RunPod](https://runpod.io) GPU pods | `runpod` | `dist/recipes/runpod-ollama.mjs` |

### modal-ollama

Provisions a [Modal Sandbox](https://modal.com/docs/guide/sandboxes) from the `ollama/ollama:latest`
image, serves an embedding model, and tunnels port `11434`.

Provider gotchas baked into the recipe (each one cost real time to find):

- Modal Sandbox `command` is passed as entrypoint args for registry images. The `ollama/ollama`
  image already has `/bin/ollama` as its entrypoint, so the recipe uses `["serve"]`, attaches a TCP
  readiness probe on port `11434`, and explicitly waits for readiness before pulling/verifying the
  model.
- ollama defaults to `127.0.0.1:11434`, unreachable by the tunnel — the recipe sets
  `OLLAMA_HOST=0.0.0.0:11434`.
- Ollama defaults to single-request handling — the recipe sets `OLLAMA_NUM_PARALLEL` from
  `input.ollama_num_parallel` so Modal can serve the client-side remote concurrency window.
- **GPU is off by default.** GPU on Modal must attach before ollama's discovery watchdog times out,
  or the box dies with exit 137 at cold start. CPU `serve` is the safe v1 path; pass `gpu` only if
  you've accepted that.
- `timeoutMs: 1_800_000` (30 min) is the provider-side lifetime backstop.

Auth comes from the ambient env (`MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET`) or `~/.modal.toml` —
rag-rat does not pass Modal creds through `RAG_RAT_COOKBOOK_INPUT`.

### runpod-ollama

Provisions an ephemeral **GPU pod** on [RunPod](https://runpod.io) from the `ollama/ollama:latest`
image and serves an embedding model on port `11434`. RunPod proxies that HTTP port at
`https://<podId>-11434.proxy.runpod.net` — that proxy URL is the endpoint.

It talks to RunPod's **GraphQL API directly over `fetch`** (`podFindAndDeployOnDemand` to deploy,
`podTerminate` to tear down). The `runpod-sdk` npm package is deliberately **not** a dependency: it
only covers serverless endpoints (`run`/`runSync`/`status`/`stream`/`health`) and has no
pod-management surface.

Provider gotchas baked into the recipe:

- The ollama image **entrypoint is `/bin/ollama`**, so `dockerArgs: "serve"` runs `ollama serve`
  (`"ollama serve"` would exec `ollama ollama serve`). `dockerArgs` sets the container CMD,
  appended to the entrypoint.
- ollama defaults to `127.0.0.1:11434`, unreachable through the proxy — the recipe sets
  `OLLAMA_HOST=0.0.0.0:11434` and `OLLAMA_NUM_PARALLEL` in the pod env.
- There is **no in-pod exec** in `podFindAndDeployOnDemand`, so the model is pulled **client-side**
  over the proxy URL (`POST /api/pull`, non-streaming, retried until the pod finishes booting),
  then `/api/embed` is probed before the `ready` event.
- **No provider-side backstop.** RunPod's `podFindAndDeployOnDemand` has **no** field that
  auto-stops or auto-terminates an on-demand GPU pod after a duration or after idle (`idleTimeout`
  is a serverless/flex-worker concept, not an on-demand-pod control — verified against the RunPod
  GraphQL spec). So **reliable teardown is the only thing that stops the billing**: every fetch is
  `AbortSignal`-bounded, and `podTerminate` runs under a tight (~8 s) timeout to finish inside the
  Rust-side grace.
- **Deploy-timeout orphan sweep.** The pod is deployed with a unique name
  (`rag-rat-cookbook-ollama-<ts>`). If the deploy call throws (e.g. RunPod created the pod but the
  response was lost/slow) before the pod handle is reported, the recipe queries `myself { pods }`,
  terminates any pod matching that name, then rethrows — so a created-but-unacknowledged pod can't
  bill forever. Best-effort and bounded; it only logs, never masks the original deploy error.
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

# or a recipe directly (what rag-rat spawns):
RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' node dist/recipes/modal-ollama.mjs

# dev (no build step), via tsx:
RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' npx tsx recipes/runpod-ollama.mts
```

The recipe runs until you `SIGTERM`/`SIGINT` it (Ctrl-C), at which point it tears down the box.

## Writing your own recipe

A recipe is a `.mts` file that hands a `Recipe<H>` to `runRecipe` from `@rag-rat/cookbook`. The
harness owns all the contract plumbing **and** the terminate-on-error wrapper and the
provision-timeout clamp, so a recipe is just: provision, report the box via `ctx.onBox`, emit
progress via `ctx.status`, verify, and return.

```ts
import {
  type ProvisionContext, type Provisioned, type Recipe,
  runRecipe, verifyEmbed, log,
} from "@rag-rat/cookbook"; // or "../src/contract.js" in-repo

async function provision(ctx: ProvisionContext<MyBox>): Promise<Provisioned<MyBox>> {
  const { input, provisionTimeoutMs } = ctx;       // already clamped from provision_timeout_s
  ctx.status("provisioning", "creating box");      // status events tag the recipe's provider
  const box = await myProvider.spawn(/* … */);
  ctx.onBox(box);                                  // report NOW → runRecipe tears down if we throw
  log("info", `box ${box.id} created`);            // log(level, message) → a `log` event
  ctx.status("verifying", "probing /api/embed");
  await verifyEmbed(box.url, { model: input.model, budgetMs: provisionTimeoutMs });
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

- **`verifyEmbed(endpoint, { model, budgetMs, headers? })`** — poll `<endpoint>/api/embed` until a
  real vector comes back. Call it before returning from `provision` (a reachable box is not a
  serving box).
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

To add a provider, drop a `recipes/<name>-ollama.mts` and register it in the `PROVIDERS` map in
`cli.mts`.

## Layout

```
cookbook/
  package.json                 @rag-rat/cookbook — bin → dist/cli.mjs; deps: modal; dev: typescript, tsx
  tsconfig.json                strict, ESM (NodeNext)
  cli.mts                      the bin dispatcher: `cookbook <provider>` → imports the recipe (→ dist/cli.mjs)
  src/contract.ts              the published contract: CookbookEvent + emit/log/status + runRecipe() + pollUntil/verifyEmbed/fetchWithTimeout (→ dist/src/contract.js)
  recipes/modal-ollama.mts     the Modal + Ollama recipe       (→ dist/recipes/modal-ollama.mjs)
  recipes/runpod-ollama.mts    the RunPod + Ollama recipe       (→ dist/recipes/runpod-ollama.mjs)
  README.md                    this file
```
