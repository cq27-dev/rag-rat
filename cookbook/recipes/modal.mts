/**
 * Recipe: provision an ephemeral embedding box on Modal, serve the model with the selected backend
 * (ollama, infinity, or vLLM), hand rag-rat the tunnel endpoint, and tear the box down on signal.
 *
 * Run (post-build):  RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' node dist/recipes/modal.mjs
 * Run (dev):         RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm","backend":"infinity"}' npx tsx recipes/modal.mts
 *
 * Modal auth comes from the ambient process env (MODAL_TOKEN_ID / MODAL_TOKEN_SECRET) or
 * ~/.modal.toml — rag-rat does not pass creds through RAG_RAT_COOKBOOK_INPUT.
 *
 * WHAT VARIES BY BACKEND lives in {@link selectBackendSpec} (image, entrypoint args, env, port, the
 * embeddings route, and whether a post-boot model pull is needed). THIS file owns only the Modal box
 * lifecycle, which is backend-independent.
 *
 * Modal gotchas, all load-bearing:
 *   - Modal Sandbox `command` is passed as ENTRYPOINT ARGS for registry images (the spec supplies
 *     exactly those, never the entrypoint itself: ollama's is `/bin/ollama` so args are `["serve"]`;
 *     infinity's is `infinity_emb` so args start `["v2", …]`; vLLM's is `vllm serve` so the first
 *     arg is the model id).
 *   - The box must listen on 0.0.0.0 for the tunnel to reach it — ollama needs OLLAMA_HOST (set in
 *     its spec env); infinity binds 0.0.0.0 by default; vLLM gets an explicit `--host 0.0.0.0`.
 *   - GPU: ollama/infinity default to CPU (GPU is opt-in and, for ollama, must attach before its
 *     discovery watchdog times out or the box dies exit 137). vLLM's image is CUDA-only, so when the
 *     backend `requiresGpu` and the caller passed none we default one in ({@link MODAL_DEFAULT_GPU}).
 *   - timeout=1800s (30 min) is the provider-side max-lifetime backstop: if rag-rat dies or
 *     SIGKILLs us before teardown runs, Modal self-destructs the box. (RunPod has no equivalent.)
 *
 * The harness ({@link runRecipe}) owns the terminate-on-error wrapper and the provision-timeout
 * clamp, so this recipe is just: create the box, report it via `ctx.onBox`, (pull,) verify, return.
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
import { selectBackendSpec } from "./backends.mjs";

/** Provider-side max-lifetime backstop in ms — a leaked box self-destructs after this. */
const BOX_MAX_LIFETIME_MS = 1_800_000; // 30 minutes
/** Default provisioning budget (boot + pull + first serving response) when input omits it. */
const DEFAULT_PROVISION_TIMEOUT_S = 600;
/** Modal app the sandboxes are grouped under (created on first use). */
const APP_NAME = "rag-rat-cookbook";
/** GPU attached when a backend REQUIRES one (vLLM) and the caller passed no `gpu`. */
const MODAL_DEFAULT_GPU = "A10G";
/**
 * Bound on the teardown `sb.terminate()` — kept under the Rust side's ~10s teardown grace (like
 * RunPod's `TEARDOWN_TIMEOUT_MS`) so terminate completes-or-aborts BEFORE rag-rat SIGKILLs us; a
 * hung terminate would otherwise leak a billed box until Modal's `timeoutMs` backstop fires.
 */
const TEARDOWN_TIMEOUT_MS = 8_000;

/** Provision an embedding box serving `input.model` on the selected backend; return its endpoint. */
async function provision(ctx: ProvisionContext<Sandbox>): Promise<Provisioned<Sandbox>> {
  const { input, provisionTimeoutMs } = ctx;
  const spec = selectBackendSpec(input.backend ?? "ollama");
  const port = spec.port;
  // ONE deadline for the WHOLE sequence (create + pull + verify). EVERY SDK await below — none of
  // which has a native timeout — is raced against the time REMAINING until this deadline via
  // `withBudget`, so a single hung Modal call can't run past the Rust provisioner's hard timeout
  // and get SIGKILLed before teardown. A budget-exhaustion throw lands in `runRecipe`'s catch,
  // which tears the box down gracefully. (Modal's `timeoutMs` is only a last-resort backstop; a
  // leaked box bills until then.) See N2.
  const deadline = Date.now() + provisionTimeoutMs;
  // ollama/infinity default to CPU; a GPU-required backend (vLLM) gets a default GPU when none given.
  const gpu = input.gpu ?? (spec.requiresGpu ? MODAL_DEFAULT_GPU : null);

  ctx.status(
    "provisioning",
    `creating Modal sandbox (backend=${spec.backend}, model=${input.model}, gpu=${gpu ?? "cpu"})`,
  );

  // Creds resolve from env (MODAL_TOKEN_ID/MODAL_TOKEN_SECRET) or ~/.modal.toml.
  const modal = new ModalClient();
  const app = await withBudget(deadline, "apps.fromName", () =>
    modal.apps.fromName(APP_NAME, { createIfMissing: true }),
  );

  const image = modal.images.fromRegistry(spec.image(input));

  // CREATE-TIMEOUT ORPHAN (#330-1, the Modal analog of RunPod's N3 deploy-orphan sweep): the create
  // is `withBudget`-bounded, but Modal can REACH the backend and CREATE the sandbox and then have the
  // SDK call stall past the budget — `withBudget` throws BEFORE `ctx.onBox(sb)`, so `runRecipe` tears
  // down "nothing" while the created sandbox bills until Modal's `timeoutMs` backstop (30 min). We
  // give the sandbox a recoverable UNIQUE name; on ANY create throw, look it up by name and terminate
  // it before rethrowing. (Unlike RunPod, Modal HAS a provider backstop, so this only shortens the
  // worst case — but a leaked GPU box for up to 30 min is still real money.)
  const sandboxName = `${APP_NAME}-${Date.now()}-${randomUUID().slice(0, 8)}`;

  // The backend spec supplies the entrypoint ARGS (never the entrypoint) + env + the served port.
  let sb: Sandbox;
  const createRef: { promise?: Promise<Sandbox> } = {};
  try {
    sb = await withBudget(deadline, "sandboxes.create", () => {
      createRef.promise = modal.sandboxes.create(app, image, {
        name: sandboxName,
        command: [...spec.entrypointArgs(input)],
        env: spec.env(input),
        encryptedPorts: [port],
        readinessProbe: Probe.withTcp(port, { intervalMs: 1000 }),
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

  ctx.status("provisioning", `waiting for ${spec.backend} to listen on port ${port}`);
  const readinessTimeoutMs = assertBudgetRemaining(deadline, "sandbox readiness");
  await raceWithTimeout(
    () => sb.waitUntilReady(readinessTimeoutMs),
    readinessTimeoutMs,
    "sandbox.waitUntilReady",
  );
  log("info", `${spec.backend} is listening`);

  // Load the model. infinity/vLLM auto-download on boot (nothing to do); ollama boots empty, so pull
  // the model into the running server via an in-box exec.
  if (spec.modelLoad === "ollama-pull") {
    await pullOllamaModel(sb, input.model, deadline, ctx);
  }

  // Resolve the public tunnel URL for the served port.
  const tunnels = await withBudget(deadline, "sb.tunnels", () => sb.tunnels());
  const tunnel = tunnels[port];
  if (tunnel === undefined) {
    throw new Error(`no tunnel for port ${port}; got ports [${Object.keys(tunnels).join(", ")}]`);
  }
  const endpoint = tunnel.url;
  log("info", `tunnel up: ${endpoint}`);

  // Verify the server actually embeds before we emit `ready`. This catches a box that booted but
  // isn't serving (the whole point of waiting before the ready event). Budget = the time REMAINING
  // until the shared deadline (create + pull already consumed some); throws if it's spent.
  ctx.status("verifying", `probing ${spec.embedPath} for a real vector`);
  await verifyEmbed(endpoint, {
    model: input.model,
    embedPath: spec.embedPath,
    budgetMs: assertBudgetRemaining(deadline, "embed verification"),
  });
  log("info", "embed verification passed; box is serving");

  // Open tunnel via encryptedPorts → no per-request token needed. auth_token stays null.
  return { handle: sb, endpoint, auth_token: null };
}

/**
 * Pull `model` into the running ollama via an in-box exec (the server `serve` is the main process;
 * the pull populates the model store it reads). Every step (exec dispatch, wait, stderr read) is
 * budget-bounded. ollama-only — infinity/vLLM auto-download on boot.
 */
async function pullOllamaModel(
  sb: Sandbox,
  model: string,
  deadline: number,
  ctx: ProvisionContext<Sandbox>,
): Promise<void> {
  ctx.status("pulling", `pulling model "${model}" (cold-start cost)`);
  const pull = await withBudget(deadline, "exec(ollama pull)", () =>
    sb.exec(["ollama", "pull", model], { mode: "text", stdout: "pipe", stderr: "pipe" }),
  );
  const pullCode = await withBudget(deadline, "pull.wait", () => pull.wait());
  if (pullCode !== 0) {
    const err = await withBudget(deadline, "pull.stderr.readText", () => pull.stderr.readText());
    throw new Error(`"ollama pull ${model}" exited ${pullCode}: ${err.trim()}`);
  }
  log("info", `model "${model}" pulled`);
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
