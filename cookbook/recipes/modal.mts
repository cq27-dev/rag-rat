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
import { AlreadyExistsError, ModalClient, NotFoundError, Probe } from "modal";
import type { Logger as ModalLogger, Sandbox } from "modal";

import {
  type ProvisionContext,
  type Provisioned,
  type Recipe,
  assertBudgetRemaining,
  errorMessage,
  log,
  raceWithTimeout,
  remainingBudgetMs,
  runRecipe,
  verifyChat,
  verifyEmbed,
  withBudget,
} from "../src/contract.js";
import { assertCapabilitySupported, selectBackendSpec } from "./backends.mjs";
import {
  HF_CACHE_MOUNT_PATH,
  type SandboxOutputCapture,
  modalCacheEnvironment,
  modalCacheSandboxName,
  modalCacheVolumeName,
  modalHfReadyMarkerName,
  safeDiagnostic,
  startSandboxOutputCapture,
  vllmCachePlan,
} from "./modal-support.mjs";

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
 * Bound on the recipe-side teardown path (terminate + shutdown poll + optional marker publication +
 * stream cleanup), kept under Rust's 10s SIGTERM→SIGKILL grace so all of it completes-or-aborts
 * before the process group is killed.
 */
const TEARDOWN_TIMEOUT_MS = 9_000;
/** Briefly allow final startup diagnostics to arrive after readiness rejects. */
const FAILURE_LOG_SETTLE_MS = 250;
/** Keeps the complete JSON-escaped error event below Rust's 16 KiB line cap. */
const READINESS_DIAGNOSTIC_BYTES = 7 * 1024;
/** Bound stream cancellation after the remote box has terminated. */
const OUTPUT_STOP_TIMEOUT_MS = 250;
/** Marker publication is metadata-only and must fit in the post-termination teardown remainder. */
const CACHE_MARKER_TIMEOUT_MS = 500;
const OWNER_TAG = "rag-rat-owner";

interface CachePublication {
  readonly modal: ModalClient;
  readonly markerNames: readonly string[];
  verified: boolean;
}

interface ModalHandle {
  readonly modal: ModalClient;
  readonly sandbox: Sandbox;
  readonly output: SandboxOutputCapture;
  readonly cachePublication: CachePublication | null;
}

/** Provision an embedding box serving `input.model` on the selected backend; return its endpoint. */
async function provision(ctx: ProvisionContext<ModalHandle>): Promise<Provisioned<ModalHandle>> {
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
  // Modal's default logger writes non-JSON text to stdout/stderr and honors ambient config. Route
  // SDK warnings through typed cookbook events instead so the JSONL wire contract cannot be broken.
  const modal = new ModalClient({ logger: modalSdkLogger(), logLevel: "warn" });
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
  // give the sandbox a recoverable name and owner tag; on ANY create throw, look it up by name and
  // terminate it only when that tag proves this invocation created it. (Unlike RunPod, Modal HAS a
  // provider backstop, so this only shortens the worst case — but a leaked GPU box for up to 30 min
  // is still real money.)
  // The backend spec supplies the entrypoint ARGS (never the entrypoint) + env + the served port.
  // Resolve the args first — the vLLM chat path is async (it sizes --max-model-len from the model's
  // HF context).
  const entrypointArgs = await withBudget(deadline, "entrypointArgs", () =>
    Promise.resolve(spec.entrypointArgs(input)),
  );
  const cacheVolumeName = modalCacheVolumeName(input.model);
  const cacheVolume =
    spec.modelLoad === "in-launch"
      ? await withBudget(deadline, "volumes.fromName", () =>
          modal.volumes.fromName(cacheVolumeName, { createIfMissing: true }),
        )
      : null;
  if (cacheVolume !== null) {
    log("info", `using persistent Hugging Face cache volume at ${HF_CACHE_MOUNT_PATH}`);
  }
  const hfMarkerName = modalHfReadyMarkerName(input.model, cacheVolume?.volumeId ?? "uncached");
  const hfReady =
    cacheVolume !== null && (await cacheMarkerExists(modal, hfMarkerName, deadline, "Hugging Face"));
  const compilePlan =
    spec.backend === "vllm"
      ? vllmCachePlan(
          input.model,
          resolvedImage,
          gpu ?? "cpu",
          capability,
          entrypointArgs,
          cacheVolume?.volumeId ?? "uncached",
        )
      : null;
  const compileReady =
    compilePlan !== null &&
    (await cacheMarkerExists(modal, compilePlan.markerName, deadline, "vLLM compile"));
  const cachePublication: CachePublication | null =
    cacheVolume === null
      ? null
      : {
          modal,
          markerNames: [
            ...(!hfReady ? [hfMarkerName] : []),
            ...(compilePlan !== null && !compileReady ? [compilePlan.markerName] : []),
          ],
          verified: false,
        };
  const cacheEnv = cacheVolume === null ? {} : modalCacheEnvironment(compilePlan);
  // Every cache-backed Sandbox can update HF metadata or compiler artifacts. Modal v1 commits are
  // last-writer-wins across the whole Volume, so exclude concurrent mounts for this model even after
  // markers exist. The owner tag makes timeout cleanup safe despite the shared deterministic name.
  const ownerId = randomUUID();
  const sandboxName =
    cacheVolume === null
      ? `${APP_NAME}-${Date.now()}-${randomUUID().slice(0, 8)}`
      : modalCacheSandboxName(cacheVolumeName);
  let sb: Sandbox;
  const createRef: { promise?: Promise<Sandbox> } = {};
  try {
    sb = await withBudget(deadline, "sandboxes.create", () => {
      createRef.promise = modal.sandboxes.create(app, image, {
        name: sandboxName,
        tags: { [OWNER_TAG]: ownerId },
        command: [...entrypointArgs],
        env: {
          ...spec.env(input),
          ...cacheEnv,
        },
        ...(cacheVolume !== null ? { volumes: { [HF_CACHE_MOUNT_PATH]: cacheVolume } } : {}),
        encryptedPorts: [port],
        readinessProbe: Probe.withTcp(port, { intervalMs: 1000 }),
        timeoutMs: BOX_MAX_LIFETIME_MS,
        idleTimeoutMs: idleWindowMs(provisionTimeoutMs),
        ...(gpu !== null ? { gpu } : {}),
      });
      return createRef.promise;
    });
  } catch (cause) {
    if (cacheVolume !== null && cause instanceof AlreadyExistsError) {
      throw new Error(
        `model cache ${cacheVolumeName} is already mounted by another cookbook Sandbox; retry after it finishes`,
        { cause },
      );
    }
    const pendingCreate = createRef.promise;
    if (pendingCreate !== undefined) {
      void pendingCreate.then(
        (lateSandbox: Sandbox) => terminateOrphanSandbox(modal, lateSandbox, sandboxName, "late create"),
        (lateCause: unknown) => {
          log(
            "info",
            `orphan sweep: create promise for "${sandboxName}" later rejected: ${errorMessage(lateCause)}`,
          );
        },
      );
    }
    await sweepOrphanByName(modal, sandboxName, ownerId);
    throw cause;
  }
  // Start draining before readiness: model-download failures are emitted by the main process, and
  // waiting to attach until after readiness would lose the exact diagnostics needed when boot fails.
  const output = startSandboxOutputCapture(sb.stdout, sb.stderr, log);
  const handle = { modal, sandbox: sb, output, cachePublication };
  // Report the box NOW so runRecipe tears it down if readiness, pull, or verify throws.
  ctx.onBox(handle);
  log("info", `sandbox created: ${sb.sandboxId ?? "(id unavailable)"}`);

  ctx.status("provisioning", `waiting for ${spec.backend} to listen on port ${port}`);
  const readinessTimeoutMs = assertBudgetRemaining(deadline, "sandbox readiness");
  try {
    await raceWithTimeout(
      () => sb.waitUntilReady(readinessTimeoutMs),
      readinessTimeoutMs,
      "sandbox.waitUntilReady",
    );
  } catch (cause) {
    const diagnosticMs = Math.min(FAILURE_LOG_SETTLE_MS, Math.max(0, deadline - Date.now()));
    const [, exitCode] = await Promise.all([
      output.settle(diagnosticMs),
      diagnosticMs > 0
        ? raceWithTimeout(() => sb.poll(), diagnosticMs, "sandbox.poll").catch(() => undefined)
        : Promise.resolve(undefined),
    ]);
    const tail = output.failureTail();
    const diagnostic = safeDiagnostic(
      `${errorMessage(cause)}${tail === "" ? "" : `\nrecent sandbox output:\n${tail}`}`,
      READINESS_DIAGNOSTIC_BYTES,
    );
    throw new Error(
      `sandbox readiness failed (exit code ${exitCode === undefined ? "unavailable" : exitCode ?? "not exited"}): ` +
        diagnostic,
      { cause },
    );
  }
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
  if (cachePublication !== null) cachePublication.verified = true;

  // Open tunnel via encryptedPorts → no per-request token needed. auth_token stays null.
  return { handle, endpoint, auth_token: null };
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
  ctx: ProvisionContext<ModalHandle>,
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
async function teardown(handle: ModalHandle): Promise<void> {
  const deadline = Date.now() + TEARDOWN_TIMEOUT_MS;
  try {
    await terminateAndWait(handle.modal, handle.sandbox, "sb.terminate", deadline);
    await publishCacheMarkers(handle.cachePublication, deadline);
  } finally {
    const stopMs = Math.min(OUTPUT_STOP_TIMEOUT_MS, remainingBudgetMs(deadline));
    if (stopMs > 0) {
      await raceWithTimeout(() => handle.output.stop(), stopMs, "sandbox output stop").catch(
        (cause) => log("warn", `sandbox output cleanup did not finish: ${errorMessage(cause)}`),
      );
    }
  }
}

async function cacheMarkerExists(
  modal: ModalClient,
  markerName: string,
  deadline: number,
  label: string,
): Promise<boolean> {
  try {
    await withBudget(deadline, `volumes.fromName(${label} marker)`, () =>
      modal.volumes.fromName(markerName),
    );
    log("info", `${label} cache marker found: ${markerName}`);
    return true;
  } catch (cause) {
    if (cause instanceof NotFoundError || /not\s*found/i.test(errorMessage(cause))) {
      log("info", `${label} cache marker absent; this run will publish it after verified teardown`);
      return false;
    }
    throw cause;
  }
}

async function publishCacheMarkers(
  publication: CachePublication | null,
  deadline: number,
): Promise<void> {
  if (publication === null || !publication.verified || publication.markerNames.length === 0) return;
  const budgetMs = Math.min(CACHE_MARKER_TIMEOUT_MS, remainingBudgetMs(deadline));
  if (budgetMs === 0) {
    log("warn", "cache marker publication skipped: teardown grace exhausted");
    return;
  }
  try {
    await raceWithTimeout(
      () =>
        Promise.all(
          publication.markerNames.map((name) =>
            publication.modal.volumes.fromName(name, { createIfMissing: true }),
          ),
        ),
      budgetMs,
      "cache marker publication",
    );
    log("info", `published cache markers: ${publication.markerNames.join(", ")}`);
  } catch (cause) {
    // The billed box is already gone. A missing marker only costs another conservative warmup;
    // never turn successful teardown into a process failure for optional metadata publication.
    log("warn", `cache marker publication failed; cache remains conservative: ${errorMessage(cause)}`);
  }
}

/**
 * Best-effort orphan sweep for the create-timeout race (#330-1): if `sandboxes.create` threw but
 * Modal actually created the sandbox, that sandbox carries our `sandboxName` and owner tag. Look it
 * up by name within the cookbook App and terminate it only when the tag matches this invocation.
 * This matters for deterministic cache-lock names: an unrelated active owner must survive our
 * failed create. The sweep is bounded (`TEARDOWN_TIMEOUT_MS`) and fully swallowed — it must NEVER
 * throw (the caller is about to rethrow the original create error, which is the signal rag-rat acts
 * on); it only logs. A `NotFoundError` from `fromName` is the EXPECTED happy case (create never
 * actually made a box) and is logged as info, not an error.
 */
async function sweepOrphanByName(
  modal: ModalClient,
  sandboxName: string,
  expectedOwnerId: string,
): Promise<void> {
  try {
    const orphan = await raceWithTimeout(
      () => modal.sandboxes.fromName(APP_NAME, sandboxName),
      TEARDOWN_TIMEOUT_MS,
      "sandboxes.fromName(orphan sweep)",
    );
    const tags = await raceWithTimeout(
      () => orphan.getTags(),
      TEARDOWN_TIMEOUT_MS,
      "orphan.getTags",
    );
    if (tags[OWNER_TAG] !== expectedOwnerId) {
      log("info", `orphan sweep: sandbox "${sandboxName}" belongs to another invocation; leaving it running`);
      return;
    }
    await terminateOrphanSandbox(modal, orphan, sandboxName, "orphan sweep");
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
  modal: ModalClient,
  orphan: Sandbox,
  sandboxName: string,
  source: string,
): Promise<void> {
  try {
    await terminateAndWait(modal, orphan, `${source}.terminate`);
    log("warn", `${source}: terminated leaked sandbox "${sandboxName}" (${orphan.sandboxId})`);
  } catch (cause) {
    log("error", `${source}: could not terminate leaked sandbox "${sandboxName}": ${errorMessage(cause)}`);
  }
}

async function terminateAndWait(
  modal: ModalClient,
  sandbox: Sandbox,
  label: string,
  deadline = Date.now() + TEARDOWN_TIMEOUT_MS,
): Promise<void> {
  const sandboxId = sandbox.sandboxId;
  await withBudget(deadline, label, () => sandbox.terminate());
  const attached = await withBudget(deadline, `${label}.reattach`, () => modal.sandboxes.fromId(sandboxId));
  while ((await withBudget(deadline, `${label}.poll`, () => attached.poll())) === null) {
    await new Promise((resolve) => setTimeout(resolve, Math.min(250, assertBudgetRemaining(deadline, label))));
  }
}

function modalSdkLogger(): ModalLogger {
  const emit = (level: "info" | "warn" | "error", message: string, args: unknown[]): void => {
    const details = args
      .slice(0, 8)
      .map((value) => (value instanceof Error ? value.message : String(value)))
      .join(" ");
    log(level, safeDiagnostic(`Modal SDK: ${message}${details === "" ? "" : ` ${details}`}`));
  };
  return {
    debug: (message, ...args) => emit("info", message, args),
    info: (message, ...args) => emit("info", message, args),
    warn: (message, ...args) => emit("warn", message, args),
    error: (message, ...args) => emit("error", message, args),
  };
}

const recipe: Recipe<ModalHandle> = {
  provider: "modal",
  defaultProvisionTimeoutS: DEFAULT_PROVISION_TIMEOUT_S,
  provision,
  teardown,
};

await runRecipe(recipe);
