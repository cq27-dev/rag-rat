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
 *   - Two provider-side backstops guard against a leaked box when rag-rat dies or SIGKILLs us
 *     before teardown runs. idleTimeoutMs self-destructs the box once it goes idle (Modal counts
 *     only a running exec, an stdin write, or an open tunnel TCP connection as activity — NOT the
 *     serve process); timeout=1800s (30 min) is the unconditional max-lifetime cap. The idle window
 *     is `provisionTimeoutMs + SERVING_IDLE_GRACE_MS`, NOT a bare few minutes: the idle clock runs
 *     from creation and a vLLM/infinity cold start opens no tunnel until it serves, so a short
 *     window would kill a legit slow boot. (RunPod has neither backstop.)
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
  verifyChat,
  verifyEmbed,
  withBudget,
} from "../src/contract.js";
import { assertCapabilitySupported, selectBackendSpec } from "./backends.mjs";

/** Provider-side max-lifetime backstop in ms — a leaked box self-destructs after this. */
const BOX_MAX_LIFETIME_MS = 1_800_000; // 30 minutes
/**
 * Serving-phase margin (ms) added ON TOP OF the provisioning budget to form the box's `idleTimeoutMs`
 * (see {@link idleWindowMs}). Modal terminates a box after `idleTimeoutMs` of NO activity — where
 * "active" means a running `sb.exec`, an `sb.stdin` write, or an OPEN TCP CONNECTION OVER A TUNNEL;
 * the main serve process does NOT count. Once serving, rag-rat's keep-alive HTTP pool holds a tunnel
 * connection open, so a live job keeps the box active; when the owning rag-rat process dies (crash,
 * killed terminal) its connections drop and the box eventually goes idle and self-destructs instead
 * of billing to BOX_MAX_LIFETIME_MS.
 *
 * Why the window is provision-budget + margin, not a bare few minutes: the idle clock runs from
 * CREATION, and a vLLM/infinity cold start (image pull + model load) opens no tunnel until the
 * recipe's verify step, so a short window would self-destruct a legit slow boot before it ever
 * serves. The provision budget already bounds that boot-idle gap (the recipe aborts + tears down if
 * boot overruns it), so a window ≥ it can never fire mid-boot.
 *
 * RECLAIM TIME, stated honestly: Modal restarts the idle countdown on EVERY activity, so a box
 * orphaned mid-serve is reclaimed after the FULL window elapses from its last request — i.e.
 * `provisionTimeoutMs + this margin`, NOT just this margin. That is minutes-to-low-tens-of-minutes
 * (well under the 30-min max-lifetime cap), the price of a single static value: the SDK has no
 * post-create setter to tighten the idle timeout once the box is past boot and serving.
 */
const SERVING_IDLE_GRACE_MS = 180_000; // 3 minutes past the provisioning budget

/**
 * The box's idle window: the provisioning budget (rounded UP to a whole second) plus the
 * serving-phase margin. Rounding keeps it an integer number of seconds — Modal encodes idle timeout
 * as a uint32 seconds field, so a fractional `provision_timeout_s` (the contract allows one) must
 * not produce a sub-second `idleTimeoutMs`. Rounding UP preserves the "≥ provisionTimeoutMs" boot
 * guarantee above.
 */
function idleWindowMs(provisionTimeoutMs: number): number {
  return Math.ceil(provisionTimeoutMs / 1000) * 1000 + SERVING_IDLE_GRACE_MS;
}
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
  // What the box should SERVE (embeddings vs chat). Absent → embed (back-compat). Fail loudly NOW,
  // before any box is created, if this backend can't serve it (infinity + chat).
  const capability = input.capability ?? "embed";
  assertCapabilitySupported(spec, capability);
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

  // #689: name the resolved image in the event stream so an image-related boot failure is
  // attributable from the log view instead of surfacing as an opaque `SandboxWaitUntilReady` error.
  const resolvedImage = spec.image(input);
  log("info", `using ${spec.backend} image: ${resolvedImage}`);
  const image = modal.images.fromRegistry(resolvedImage);

  // CREATE-TIMEOUT ORPHAN (#330-1, the Modal analog of RunPod's N3 deploy-orphan sweep): the create
  // is `withBudget`-bounded, but Modal can REACH the backend and CREATE the sandbox and then have the
  // SDK call stall past the budget — `withBudget` throws BEFORE `ctx.onBox(sb)`, so `runRecipe` tears
  // down "nothing" while the created sandbox bills until Modal's `timeoutMs` backstop (30 min). We
  // give the sandbox a recoverable UNIQUE name; on ANY create throw, look it up by name and terminate
  // it before rethrowing. (Unlike RunPod, Modal HAS a provider backstop, so this only shortens the
  // worst case — but a leaked GPU box for up to 30 min is still real money.)
  const sandboxName = `${APP_NAME}-${Date.now()}-${randomUUID().slice(0, 8)}`;

  // The backend spec supplies the entrypoint ARGS (never the entrypoint) + env + the served port.
  // Resolve the args first — the vLLM chat path is async (it sizes --max-model-len from the model's
  // HF context).
  const entrypointArgs = await spec.entrypointArgs(input);
  let sb: Sandbox;
  const createRef: { promise?: Promise<Sandbox> } = {};
  try {
    sb = await withBudget(deadline, "sandboxes.create", () => {
      createRef.promise = modal.sandboxes.create(app, image, {
        name: sandboxName,
        command: [...entrypointArgs],
        env: spec.env(input),
        encryptedPorts: [port],
        readinessProbe: Probe.withTcp(port, { intervalMs: 1000 }),
        timeoutMs: BOX_MAX_LIFETIME_MS,
        idleTimeoutMs: idleWindowMs(provisionTimeoutMs),
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

  // Verify the server actually serves before we emit `ready`. This catches a box that booted but
  // isn't serving (the whole point of waiting before the ready event). Budget = the time REMAINING
  // until the shared deadline (create + pull already consumed some); throws if it's spent. The probe
  // matches the capability: a chat box gets a chat-completions ping, an embed box an embeddings one.
  const servePath = spec.servePath(capability);
  ctx.status("verifying", `probing ${servePath} for a real ${capability === "chat" ? "completion" : "vector"}`);
  const verifyBudgetMs = assertBudgetRemaining(deadline, `${capability} verification`);
  if (capability === "chat") {
    await verifyChat(endpoint, { model: input.model, chatPath: servePath, budgetMs: verifyBudgetMs });
  } else {
    await verifyEmbed(endpoint, { model: input.model, embedPath: servePath, budgetMs: verifyBudgetMs });
  }
  log("info", `${capability} verification passed; box is serving`);

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
