# @rag-rat/cookbook

Ephemeral remote-runtime provisioning recipes for [rag-rat](../). A **recipe** is a standalone
Node program that rag-rat spawns as a subprocess: it provisions a remote box (e.g. an Ollama
embedding server), hands rag-rat the endpoint over a one-line stdout handshake, holds the box
while rag-rat works, then tears it down on signal.

This is the JS/TS half of rag-rat's remote-runtime rework (#317/#318). rag-rat (Rust) owns the
lifecycle and the embedding traffic; the cookbook owns provider provisioning.

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
- **Provider-side backstop.** Always set a max box lifetime (the Modal recipe uses `timeoutMs:
  1_800_000`) so a leaked box self-destructs if rag-rat dies before it can signal teardown.

### `CookbookInput` (env var `RAG_RAT_COOKBOOK_INPUT`)

```jsonc
{
  "model": "all-minilm",     // required: embedding model to pull + serve
  "request_timeout_s": 600,  // optional: budget for pull+boot+first response (advisory)
  "gpu": null                // optional: provider GPU spec ("A10G", …) or null for CPU
}
```

### `Handshake` (the one stdout line)

```jsonc
{
  "endpoint": "https://abc123.modal.host",  // base HTTPS endpoint of the model server
  "auth_token": null                         // bearer token, or null/omitted for an open tunnel
}
```

`auth_token` is `null` (or omitted) when the tunnel is open. rag-rat treats a missing field and an
explicit `null` identically.

## Recipes

| Recipe | Provider | Entry (post-build) |
|---|---|---|
| `recipes/modal-ollama.mts` | [Modal](https://modal.com) Sandboxes | `dist/recipes/modal-ollama.mjs` |

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

## Build & run

```bash
npm install
npm run build        # tsc → dist/ (recipes emit .mjs, the contract lib emits .js)
npm run typecheck    # tsc --noEmit

# invoke a recipe the way rag-rat does:
RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' node dist/recipes/modal-ollama.mjs

# dev (no build step), via tsx:
RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' npx tsx recipes/modal-ollama.mts
```

The recipe runs until you `SIGTERM`/`SIGINT` it (Ctrl-C), at which point it terminates the box.

## Writing your own recipe

A recipe is a `.mts` file that calls `runRecipe(provision, teardown)` from `@rag-rat/cookbook`:

```ts
import { type CookbookInput, runRecipe, log } from "@rag-rat/cookbook"; // or "../src/contract.js" in-repo

async function provision(input: CookbookInput) {
  log("provisioning", input.model);
  const box = await myProvider.spawn(/* … */);
  // … wait until the box actually serves embeddings …
  return { handle: box, endpoint: box.url, auth_token: null };
}

async function teardown(box: MyBox) {
  await box.destroy();
}

await runRecipe(provision, teardown);
```

`runRecipe` does all the contract plumbing: it reads `RAG_RAT_COOKBOOK_INPUT`, installs the signal
handlers (so a signal mid-provision still tears down), prints the single handshake line on success,
parks the process, and tears down + exits on signal. Your `provision` throws to fail (the helper
logs to stderr and exits non-zero before any handshake). Keep all of your own logging on stderr.

## Layout

```
cookbook/
  package.json                 @rag-rat/cookbook — deps: modal; dev: typescript, tsx
  tsconfig.json                strict, ESM (NodeNext)
  src/contract.ts              the published contract: types + runRecipe() helper
  recipes/modal-ollama.mts     the Modal + Ollama recipe (compiles to dist/recipes/modal-ollama.mjs)
  README.md                    this file
```
