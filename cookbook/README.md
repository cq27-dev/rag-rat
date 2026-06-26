# @rag-rat/cookbook

Ephemeral remote-runtime provisioning recipes for [rag-rat](../). A **recipe** is a standalone
Node program that rag-rat spawns as a subprocess: it provisions a remote box (e.g. an Ollama
embedding server), hands rag-rat the endpoint over a one-line stdout handshake, holds the box
while rag-rat works, then tears it down on signal.

This is the JS/TS half of rag-rat's remote-runtime rework (#317/#318). rag-rat (Rust) owns the
lifecycle and the embedding traffic; the cookbook owns provider provisioning.

> **Security — a cookbook spec must be TRUSTED.** A recipe runs with your full privileges and
> credentials (Modal/RunPod tokens in the env, your shell, your network) and provisions paid cloud
> compute. rag-rat invokes it via `npx`, which **downloads and runs whatever the configured spec
> resolves to** — `npx -y <name>` runs arbitrary downloaded code as you. Pin and trust the cookbook
> spec like any dependency; never point rag-rat at an untrusted package name or a recipe path you
> have not read.

## The process contract

rag-rat and a recipe communicate over a tiny, strict process protocol. Implement it exactly — the
Rust side is built against it.

| Stage | rag-rat | recipe |
|---|---|---|
| spawn | runs `node recipe.mjs` (or `npx tsx recipe.mts`), with `RAG_RAT_COOKBOOK_INPUT` set in env | starts up |
| input | sets `RAG_RAT_COOKBOOK_INPUT` = JSON `CookbookInput`; provider creds already in env / `~/.modal.toml` | reads + parses that env var |
| handshake | reads **one line** from the recipe's **stdout** | once the box is up **and serving**, prints exactly one line to stdout: `{"endpoint":"https://…","auth_token":"…"}` |
| liveness | uses the endpoint | stays running, holding the box |
| teardown | sends `SIGTERM` (or `SIGINT`) | destroys the box, exits `0` |
| failure | sees non-zero exit, no handshake | on provisioning failure: error to **stderr**, exit non-zero, **before** any handshake |

Hard rules:

- **stdout carries exactly one line** — the handshake JSON object. Everything else (logs,
  progress, errors) goes to **stderr**. Use the `log()` helper or `console.error`, never
  `console.log`.
- **Handshake only after serving.** Don't print the endpoint until the box answers a real request
  (the Modal recipe probes `/api/embed` and waits for a vector).
- **Stay alive after the handshake** until signaled.
- **Tear down on `SIGTERM`/`SIGINT`**, then `exit(0)`.
- **Bound every network call.** A stalled fetch that hangs past the Rust-side grace gets SIGKILLed
  with the box still up → a leaked, billed box. Use `pollUntil` / `fetchWithTimeout` (both carry an
  `AbortSignal.timeout`); never call bare `fetch` on a path that could stall.
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
  "gpu": null                  // optional: string or null. For Modal this is "A10G"/"T4" (CPU
                               //   default); for RunPod a gpuTypeId like "NVIDIA RTX A4000".
}
```

The provisioning budget is **`provision_timeout_s`**, never `request_timeout_s` — conflating the
two (using the ~30–60s per-request timeout as the boot budget) is what made the first live e2e time
out before the box finished booting. `readInput` validates each field up front (a string timeout
would otherwise become `NaN` and break the poll loops); a malformed input is a clean `exit(1)` with
no handshake.

### `Handshake` (the one stdout line)

```jsonc
{
  "endpoint": "https://abc123.modal.host",  // base HTTPS endpoint of the model server
  "auth_token": null                         // bearer token, or null/omitted for an open tunnel
}
```

`auth_token` is `null` (or omitted) when the tunnel is open. rag-rat treats a missing field and an
explicit `null` identically.

## Invoking — provider as a subcommand

The published form is **one package, provider as a subcommand**, via the package bin:

```bash
npx @rag-rat/cookbook modal     # provision on Modal
npx @rag-rat/cookbook runpod    # provision on RunPod
```

The bin (`dist/cli.mjs`) reads the provider from `argv[2]` and dynamically imports the matching
recipe (`recipes/<provider>-ollama.mjs`). Recipes self-run on import, so the dispatcher adds only
routing — no contract logic. An unknown or missing provider prints the available providers to
stderr and exits `1` (no handshake). The recipe files stay **directly runnable** too, which is what
rag-rat actually spawns:

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

- The ollama image **entrypoint is `/bin/ollama`**, so the sandbox command is the bare subcommand
  `["serve"]` — `["ollama", "serve"]` would exec `ollama ollama serve`.
- ollama defaults to `127.0.0.1:11434`, unreachable by the tunnel — the recipe sets
  `OLLAMA_HOST=0.0.0.0:11434`.
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
  `OLLAMA_HOST=0.0.0.0:11434` in the pod env.
- There is **no in-pod exec** in `podFindAndDeployOnDemand`, so the model is pulled **client-side**
  over the proxy URL (`POST /api/pull`, non-streaming, retried until the pod finishes booting),
  then `/api/embed` is probed before the handshake.
- **No provider-side backstop.** RunPod's `podFindAndDeployOnDemand` has **no** field that
  auto-stops or auto-terminates an on-demand GPU pod after a duration or after idle (`idleTimeout`
  is a serverless/flex-worker concept, not an on-demand-pod control — verified against the RunPod
  GraphQL spec). So **reliable teardown is the only thing that stops the billing**: every fetch is
  `AbortSignal`-bounded, and `podTerminate` runs under a tight (~8 s) timeout to finish inside the
  Rust-side grace.
- Default GPU is a cheap `NVIDIA RTX A4000`; `input.gpu` is a `gpuTypeId` override.

Auth: `RUNPOD_API_KEY` in the process env — the recipe errors and exits `1` (no handshake) if it's
unset. rag-rat does not pass it through `RAG_RAT_COOKBOOK_INPUT`.

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
request-timeout clamp, so a recipe is just: provision, report the box via `ctx.onBox`, verify, and
return.

```ts
import {
  type ProvisionContext, type Provisioned, type Recipe,
  runRecipe, verifyEmbed, log,
} from "@rag-rat/cookbook"; // or "../src/contract.js" in-repo

async function provision(ctx: ProvisionContext<MyBox>): Promise<Provisioned<MyBox>> {
  const { input, provisionTimeoutMs } = ctx;       // already clamped from provision_timeout_s
  log("provisioning", input.model);
  const box = await myProvider.spawn(/* … */);
  ctx.onBox(box);                                  // report NOW → runRecipe tears down if we throw
  await verifyEmbed(box.url, { model: input.model, budgetMs: provisionTimeoutMs });
  return { handle: box, endpoint: box.url, auth_token: null };
}

async function teardown(box: MyBox) {
  await box.destroy();
}

const recipe: Recipe<MyBox> = {
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
  and a signal arrives later — via an idempotent latch. You no longer write `try { … } catch {
  terminate; throw }` in the recipe;
- prints the single handshake line on success, parks the process, exits `0` on signal.

Call `ctx.onBox(handle)` the instant the box exists (before it's verified serving) — that is what
lets the harness clean up a half-provisioned box.

The contract module also exports the shared network helpers — call these instead of bare `fetch`:

- **`verifyEmbed(endpoint, { model, budgetMs, headers? })`** — poll `<endpoint>/api/embed` until a
  real vector comes back. Call it before returning from `provision` (a reachable box is not a
  serving box).
- **`pollUntil(url, { label, body, isReady, budgetMs, pollIntervalMs?, perAttemptTimeoutMs? })`** —
  the generic retry-until-ready loop (each attempt `AbortSignal`-bounded). `verifyEmbed` and the
  RunPod model pull are both built on it.
- **`fetchWithTimeout(url, init, timeoutMs)`** — a single `fetch` with a hard `AbortSignal.timeout`.
  Use it for one-shot calls (e.g. teardown) so a stall can't hang past the grace and leak the box.

To add a provider, drop a `recipes/<name>-ollama.mts` and register it in the `PROVIDERS` map in
`cli.mts`.

## Layout

```
cookbook/
  package.json                 @rag-rat/cookbook — bin → dist/cli.mjs; deps: modal; dev: typescript, tsx
  tsconfig.json                strict, ESM (NodeNext)
  cli.mts                      the bin dispatcher: `cookbook <provider>` → imports the recipe (→ dist/cli.mjs)
  src/contract.ts              the published contract: types + runRecipe() + pollUntil/verifyEmbed/fetchWithTimeout (→ dist/src/contract.js)
  recipes/modal-ollama.mts     the Modal + Ollama recipe       (→ dist/recipes/modal-ollama.mjs)
  recipes/runpod-ollama.mts    the RunPod + Ollama recipe       (→ dist/recipes/runpod-ollama.mjs)
  README.md                    this file
```
