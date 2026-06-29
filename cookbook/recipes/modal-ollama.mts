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
 *   - Modal Sandbox `command` is passed as entrypoint args for registry images. The
 *     `ollama/ollama` image already has `/bin/ollama` as ENTRYPOINT, so use `["serve"]`; passing
 *     `["/bin/ollama", "serve"]` runs `ollama /bin/ollama serve` and exits with "unknown command".
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

import { randomUUID } from "node:crypto";
import { ModalClient, Probe } from "modal";
import type { Sandbox } from "modal";

import {
  type ProvisionContext,
  type Provisioned,
  type Recipe,
  assertBudgetRemaining,
  errorMessage,
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
  const ollamaNumParallel = String(input.ollama_num_parallel ?? 1);

  ctx.status(
    "provisioning",
    `creating Modal sandbox (model=${input.model}, gpu=${gpu ?? "cpu"}, parallel=${ollamaNumParallel})`,
  );

  // Creds resolve from env (MODAL_TOKEN_ID/MODAL_TOKEN_SECRET) or ~/.modal.toml.
  const modal = new ModalClient();
  const app = await withBudget(deadline, "apps.fromName", () =>
    modal.apps.fromName(APP_NAME, { createIfMissing: true }),
  );

  const image = modal.images.fromRegistry("ollama/ollama:latest");

  // CREATE-TIMEOUT ORPHAN (#330-1, the Modal analog of RunPod's N3 deploy-orphan sweep): the create
  // is `withBudget`-bounded, but Modal can REACH the backend and CREATE the sandbox and then have the
  // SDK call stall past the budget — `withBudget` throws BEFORE `ctx.onBox(sb)`, so `runRecipe` tears
  // down "nothing" while the created sandbox bills until Modal's `timeoutMs` backstop (30 min). We
  // give the sandbox a recoverable UNIQUE name; on ANY create throw, look it up by name and terminate
  // it before rethrowing. (Unlike RunPod, Modal HAS a provider backstop, so this only shortens the
  // worst case — but a leaked GPU box for up to 30 min is still real money.)
  const sandboxName = `${APP_NAME}-${Date.now()}-${randomUUID().slice(0, 8)}`;

  // Modal passes `command` as ENTRYPOINT args for registry images. The ollama image's entrypoint is
  // `/bin/ollama`, so `["serve"]` starts `ollama serve`; including `/bin/ollama` here would become
  // `ollama /bin/ollama serve` and fail before readiness.
  let sb: Sandbox;
  const createRef: { promise?: Promise<Sandbox> } = {};
  try {
    sb = await withBudget(deadline, "sandboxes.create", () => {
      createRef.promise = modal.sandboxes.create(app, image, {
        name: sandboxName,
        command: ["serve"],
        env: {
          OLLAMA_HOST: `0.0.0.0:${OLLAMA_PORT}`,
          OLLAMA_NUM_PARALLEL: ollamaNumParallel,
        },
        encryptedPorts: [OLLAMA_PORT],
        readinessProbe: Probe.withTcp(OLLAMA_PORT, { intervalMs: 1000 }),
        timeoutMs: BOX_MAX_LIFETIME_MS,
        ...(gpu !== null ? { gpu } : {}),
      });
      return createRef.promise;
    });
  } catch (cause) {
    const pendingCreate = createRef.promise;
    if (pendingCreate !== undefined) {
      void pendingCreate.then(
        (lateSandbox: Sandbox) => terminateOrphanSandbox(lateSandbox, sandboxName, "late create"),
        (lateCause: unknown) => {
          log(
            "info",
            `orphan sweep: create promise for "${sandboxName}" later rejected: ${errorMessage(lateCause)}`,
          );
        },
      );
    }
    await sweepOrphanByName(modal, sandboxName);
    throw cause;
  }
  // Report the box NOW so runRecipe tears it down if readiness, pull, or verify throws.
  ctx.onBox(sb);
  log("info", `sandbox created: ${sb.sandboxId ?? "(id unavailable)"}`);

  ctx.status("provisioning", `waiting for ollama serve on port ${OLLAMA_PORT}`);
  const readinessTimeoutMs = assertBudgetRemaining(deadline, "sandbox readiness");
  await raceWithTimeout(
    () => sb.waitUntilReady(readinessTimeoutMs),
    readinessTimeoutMs,
    "sandbox.waitUntilReady",
  );
  log("info", "ollama serve is listening");

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

/**
 * Best-effort orphan sweep for the create-timeout race (#330-1): if `sandboxes.create` threw but
 * Modal actually created the sandbox, that sandbox carries our unique `sandboxName`. Look it up by
 * name within the cookbook App and terminate it. Bounded (`TEARDOWN_TIMEOUT_MS`) and fully swallowed
 * — it must NEVER throw (the caller is about to rethrow the original create error, which is the
 * signal rag-rat acts on); it only logs. A `NotFoundError` from `fromName` is the EXPECTED happy
 * case (create never actually made a box) and is logged as info, not an error.
 */
async function sweepOrphanByName(modal: ModalClient, sandboxName: string): Promise<void> {
  try {
    const orphan = await raceWithTimeout(
      () => modal.sandboxes.fromName(APP_NAME, sandboxName),
      TEARDOWN_TIMEOUT_MS,
      "sandboxes.fromName(orphan sweep)",
    );
    await terminateOrphanSandbox(orphan, sandboxName, "orphan sweep");
  } catch (cause) {
    // fromName raises NotFoundError when no sandbox with that name exists — the common case (create
    // never made one). Distinguish it from a real failure so the log isn't alarming.
    const message = errorMessage(cause);
    if (/not\s*found/i.test(message)) {
      log("info", `orphan sweep: no sandbox named "${sandboxName}" found (create likely never made one)`);
    } else {
      log("error", `orphan sweep: could not reclaim a possible leak "${sandboxName}": ${message}`);
    }
  }
}

async function terminateOrphanSandbox(
  orphan: Sandbox,
  sandboxName: string,
  source: string,
): Promise<void> {
  try {
    await raceWithTimeout(
      () => orphan.terminate(),
      TEARDOWN_TIMEOUT_MS,
      `${source}.terminate`,
    );
    log("warn", `${source}: terminated leaked sandbox "${sandboxName}" (${orphan.sandboxId})`);
  } catch (cause) {
    log("error", `${source}: could not terminate leaked sandbox "${sandboxName}": ${errorMessage(cause)}`);
  }
}

const recipe: Recipe<Sandbox> = {
  provider: "modal",
  defaultProvisionTimeoutS: DEFAULT_PROVISION_TIMEOUT_S,
  provision,
  teardown,
};

await runRecipe(recipe);
