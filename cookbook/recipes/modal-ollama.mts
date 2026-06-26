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
 *     SIGKILLs us before teardown runs, Modal self-destructs the box. (RunPod has no equivalent.)
 *
 * The harness ({@link runRecipe}) owns the terminate-on-error wrapper and the provision-timeout
 * clamp, so this recipe is just: create the box, report it via `ctx.onBox`, pull, verify, return.
 * Every Modal SDK await here is wrapped in `withBudget` so a hang aborts into teardown rather than
 * running past the Rust hard timeout and getting SIGKILLed with the box still billing (N2).
 */

import { ModalClient } from "modal";
import type { Sandbox } from "modal";

import {
  type ProvisionContext,
  type Provisioned,
  type Recipe,
  assertBudgetRemaining,
  log,
  raceWithTimeout,
  runRecipe,
  verifyEmbed,
  withBudget,
} from "../src/contract.js";

/** Port ollama serves on inside the box; the one port we tunnel out. */
const OLLAMA_PORT = 11434;
/** Provider-side max-lifetime backstop in ms — a leaked box self-destructs after this. */
const BOX_MAX_LIFETIME_MS = 1_800_000; // 30 minutes
/** Default provisioning budget (boot + pull + first serving response) when input omits it. */
const DEFAULT_PROVISION_TIMEOUT_S = 600;
/** Modal app the sandboxes are grouped under (created on first use). */
const APP_NAME = "rag-rat-cookbook";
/**
 * Bound on the teardown `sb.terminate()` — kept under the Rust side's ~10s teardown grace (like
 * RunPod's `TEARDOWN_TIMEOUT_MS`) so terminate completes-or-aborts BEFORE rag-rat SIGKILLs us; a
 * hung terminate would otherwise leak a billed box until Modal's `timeoutMs` backstop fires.
 */
const TEARDOWN_TIMEOUT_MS = 8_000;

/** Provision an Ollama box serving `input.model` and return its tunnel endpoint. */
async function provision(ctx: ProvisionContext<Sandbox>): Promise<Provisioned<Sandbox>> {
  const { input, provisionTimeoutMs } = ctx;
  // ONE deadline for the WHOLE sequence (create + pull + verify). EVERY SDK await below — none of
  // which has a native timeout — is raced against the time REMAINING until this deadline via
  // `withBudget`, so a single hung Modal call can't run past the Rust provisioner's hard timeout
  // and get SIGKILLed before teardown. A budget-exhaustion throw lands in `runRecipe`'s catch,
  // which tears the box down gracefully. (Modal's `timeoutMs` is only a last-resort backstop; a
  // leaked box bills until then.) See N2.
  const deadline = Date.now() + provisionTimeoutMs;
  const gpu = input.gpu ?? null;

  ctx.status("provisioning", `creating Modal sandbox (model=${input.model}, gpu=${gpu ?? "cpu"})`);

  // Creds resolve from env (MODAL_TOKEN_ID/MODAL_TOKEN_SECRET) or ~/.modal.toml.
  const modal = new ModalClient();
  const app = await withBudget(deadline, "apps.fromName", () =>
    modal.apps.fromName(APP_NAME, { createIfMissing: true }),
  );

  const image = modal.images.fromRegistry("ollama/ollama:latest");

  // `command: ["serve"]` is appended to the image entrypoint (/bin/ollama) → `ollama serve`.
  // OLLAMA_HOST goes in the sandbox env so the runtime process binds all interfaces.
  const sb = await withBudget(deadline, "sandboxes.create", () =>
    modal.sandboxes.create(app, image, {
      command: ["serve"],
      env: { OLLAMA_HOST: `0.0.0.0:${OLLAMA_PORT}` },
      encryptedPorts: [OLLAMA_PORT],
      timeoutMs: BOX_MAX_LIFETIME_MS,
      ...(gpu !== null ? { gpu } : {}),
    }),
  );
  // Report the box NOW so runRecipe tears it down if anything below throws.
  ctx.onBox(sb);
  log("info", `sandbox created: ${sb.sandboxId ?? "(id unavailable)"}`);

  // Pull the model. The server (`serve`) is the box's main process; pull runs as an exec
  // against the same ollama install, populating the model store the server reads. Every step
  // (exec dispatch, wait, stderr read) is budget-bounded.
  ctx.status("pulling", `pulling model "${input.model}" (cold-start cost)`);
  const pull = await withBudget(deadline, "exec(ollama pull)", () =>
    sb.exec(["ollama", "pull", input.model], { mode: "text", stdout: "pipe", stderr: "pipe" }),
  );
  const pullCode = await withBudget(deadline, "pull.wait", () => pull.wait());
  if (pullCode !== 0) {
    const err = await withBudget(deadline, "pull.stderr.readText", () => pull.stderr.readText());
    throw new Error(`"ollama pull ${input.model}" exited ${pullCode}: ${err.trim()}`);
  }
  log("info", `model "${input.model}" pulled`);

  // Resolve the public tunnel URL for the served port.
  const tunnels = await withBudget(deadline, "sb.tunnels", () => sb.tunnels());
  const tunnel = tunnels[OLLAMA_PORT];
  if (tunnel === undefined) {
    throw new Error(`no tunnel for port ${OLLAMA_PORT}; got ports [${Object.keys(tunnels).join(", ")}]`);
  }
  const endpoint = tunnel.url;
  log("info", `tunnel up: ${endpoint}`);

  // Verify the server actually embeds before we emit `ready`. This catches a box that booted but
  // isn't serving (the whole point of waiting before the ready event). Budget = the time REMAINING
  // until the shared deadline (create + pull already consumed some); throws if it's spent.
  ctx.status("verifying", "probing /api/embed for a real vector");
  await verifyEmbed(endpoint, {
    model: input.model,
    budgetMs: assertBudgetRemaining(deadline, "embed verification"),
  });
  log("info", "embed verification passed; box is serving");

  // Open tunnel via encryptedPorts → no per-request token needed. auth_token stays null.
  return { handle: sb, endpoint, auth_token: null };
}

/**
 * Destroy the box, bounded so a hung `terminate()` can't run past the Rust teardown grace and leak
 * a billed box. Idempotent enough — terminate on an already-gone box is a no-op/soft error.
 */
async function teardown(sb: Sandbox): Promise<void> {
  await raceWithTimeout(() => sb.terminate(), TEARDOWN_TIMEOUT_MS, "sb.terminate");
}

const recipe: Recipe<Sandbox> = {
  provider: "modal",
  defaultProvisionTimeoutS: DEFAULT_PROVISION_TIMEOUT_S,
  provision,
  teardown,
};

await runRecipe(recipe);
