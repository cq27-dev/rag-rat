/**
 * Recipe: provision an ephemeral Ollama box on Modal, serve an embedding model, hand rag-rat
 * the tunnel endpoint, and tear the box down on signal.
 *
 * Run (post-build):  RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' node dist/recipes/modal-ollama.mjs
 * Run (dev):         RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' npx tsx recipes/modal-ollama.mts
 *
 * Modal auth comes from the ambient process env (MODAL_TOKEN_ID / MODAL_TOKEN_SECRET) or
 * ~/.modal.toml — rag-rat does not pass creds through RAG_RAT_COOKBOOK_INPUT.
 *
 * Mirrors the proven Python provisioning. Provider gotchas, all load-bearing:
 *   - The ollama image ENTRYPOINT is `/bin/ollama`, so the Sandbox command is the bare
 *     subcommand `serve` — NOT `["ollama", "serve"]`, which would exec `ollama ollama serve`.
 *   - ollama binds 127.0.0.1:11434 by default, which the Modal tunnel cannot reach. We set
 *     OLLAMA_HOST=0.0.0.0:11434 so it listens on all interfaces.
 *   - GPU is optional and OFF by default. GPU on Modal must be attached before ollama's
 *     discovery watchdog times out, or the box dies with exit 137 at cold start; CPU `serve`
 *     is the safe v1 path. Pass input.gpu only when you've accepted that risk.
 *   - timeout=1800s (30 min) is the provider-side max-lifetime backstop: if rag-rat dies or
 *     SIGKILLs us before teardown runs, Modal self-destructs the box.
 */

import { ModalClient } from "modal";
import type { Sandbox } from "modal";

import { type CookbookInput, log, runRecipe } from "../src/contract.js";

/** Port ollama serves on inside the box; the one port we tunnel out. */
const OLLAMA_PORT = 11434;
/** Provider-side max-lifetime backstop in ms — a leaked box self-destructs after this. */
const BOX_MAX_LIFETIME_MS = 1_800_000; // 30 minutes
/** Default per-request budget (pull + boot + first serving response) when input omits it. */
const DEFAULT_REQUEST_TIMEOUT_S = 600;
/** Modal app the sandboxes are grouped under (created on first use). */
const APP_NAME = "rag-rat-cookbook";

/** Provision an Ollama box serving `input.model` and return its tunnel endpoint. */
async function provision(input: CookbookInput): Promise<{
  handle: Sandbox;
  endpoint: string;
  auth_token: string | null;
}> {
  const requestTimeoutS = input.request_timeout_s ?? DEFAULT_REQUEST_TIMEOUT_S;
  const requestTimeoutMs = Math.max(1, requestTimeoutS) * 1000;
  const gpu = input.gpu ?? null;

  log(
    `provisioning ollama box: model=${input.model} gpu=${gpu ?? "cpu"} ` +
      `request_timeout_s=${requestTimeoutS}`,
  );

  // Creds resolve from env (MODAL_TOKEN_ID/MODAL_TOKEN_SECRET) or ~/.modal.toml.
  const modal = new ModalClient();
  const app = await modal.apps.fromName(APP_NAME, { createIfMissing: true });

  const image = modal.images.fromRegistry("ollama/ollama:latest");

  // `command: ["serve"]` is appended to the image entrypoint (/bin/ollama) → `ollama serve`.
  // OLLAMA_HOST goes in the sandbox env so the runtime process binds all interfaces.
  const sb = await modal.sandboxes.create(app, image, {
    command: ["serve"],
    env: { OLLAMA_HOST: `0.0.0.0:${OLLAMA_PORT}` },
    encryptedPorts: [OLLAMA_PORT],
    timeoutMs: BOX_MAX_LIFETIME_MS,
    ...(gpu !== null ? { gpu } : {}),
  });
  log(`sandbox created: ${sb.sandboxId ?? "(id unavailable)"}`);

  try {
    // Pull the model. The server (`serve`) is the box's main process; pull runs as an exec
    // against the same ollama install, populating the model store the server reads.
    log(`pulling model "${input.model}" (this is the cold-start cost)…`);
    const pull = await sb.exec(["ollama", "pull", input.model], {
      mode: "text",
      stdout: "pipe",
      stderr: "pipe",
    });
    const pullCode = await pull.wait();
    if (pullCode !== 0) {
      const err = await pull.stderr.readText();
      throw new Error(`"ollama pull ${input.model}" exited ${pullCode}: ${err.trim()}`);
    }
    log(`model "${input.model}" pulled`);

    // Resolve the public tunnel URL for the served port.
    const tunnels = await sb.tunnels();
    const tunnel = tunnels[OLLAMA_PORT];
    if (tunnel === undefined) {
      throw new Error(`no tunnel for port ${OLLAMA_PORT}; got ports [${Object.keys(tunnels).join(", ")}]`);
    }
    const endpoint = tunnel.url;
    log(`tunnel up: ${endpoint}`);

    // Verify the server actually embeds before we hand rag-rat the endpoint. This catches a
    // box that booted but isn't serving (the whole point of waiting to handshake).
    await verifyEmbed(endpoint, input.model, requestTimeoutMs);
    log("embed verification passed; box is serving");

    // Open tunnel via encryptedPorts → no per-request token needed. auth_token stays null.
    return { handle: sb, endpoint, auth_token: null };
  } catch (cause) {
    // Provisioning failed after the box came up — terminate it so we don't lean on the backstop.
    log("provision error after box creation; terminating box");
    await sb.terminate().catch((e) => log("terminate-on-error failed (backstop will reap):", e));
    throw cause;
  }
}

/** POST /api/embed and confirm a non-empty vector comes back, retrying until the budget runs out. */
async function verifyEmbed(endpoint: string, model: string, budgetMs: number): Promise<void> {
  const url = `${endpoint.replace(/\/+$/, "")}/api/embed`;
  const deadline = Date.now() + budgetMs;
  let lastError = "(no attempt made)";
  let attempt = 0;

  while (Date.now() < deadline) {
    attempt += 1;
    try {
      const res = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ model, input: "rag-rat embed readiness probe" }),
      });
      if (!res.ok) {
        lastError = `HTTP ${res.status} ${res.statusText}`;
      } else {
        const body = (await res.json()) as { embeddings?: unknown };
        const embeddings = body.embeddings;
        if (
          Array.isArray(embeddings) &&
          embeddings.length > 0 &&
          Array.isArray(embeddings[0]) &&
          embeddings[0].length > 0
        ) {
          return;
        }
        lastError = `200 OK but no embedding vector in response: ${JSON.stringify(body).slice(0, 200)}`;
      }
    } catch (cause) {
      lastError = (cause as Error).message;
    }
    log(`embed probe attempt ${attempt} not ready (${lastError}); retrying…`);
    await sleep(2000);
  }
  throw new Error(`/api/embed never returned a vector within ${budgetMs}ms; last error: ${lastError}`);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Destroy the box. Idempotent enough — terminate on an already-gone box is a no-op/soft error. */
async function teardown(sb: Sandbox): Promise<void> {
  await sb.terminate();
}

await runRecipe(provision, teardown);
