/**
 * Recipe: provision an ephemeral GPU pod on RunPod, serve the model with the selected backend
 * (ollama, infinity, or vLLM), hand rag-rat the proxy endpoint, and terminate the pod on signal.
 *
 * Run (post-build):  RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' node dist/recipes/runpod.mjs
 * Run (dev):         RAG_RAT_COOKBOOK_INPUT='{"model":"sentence-transformers/all-MiniLM-L6-v2","backend":"vllm"}' npx tsx recipes/runpod.mts
 *
 * Auth: RUNPOD_API_KEY in the process env (NOT in RAG_RAT_COOKBOOK_INPUT). rag-rat puts the key in
 * the child env from the user's shell; this recipe errors + exits 1 before any handshake if unset.
 *
 * WHAT VARIES BY BACKEND lives in {@link selectBackendSpec} (image, entrypoint args, env, port, the
 * embeddings route, and whether a post-boot model pull is needed). THIS file owns only the RunPod
 * pod lifecycle, which is backend-independent.
 *
 * Why GraphQL-over-fetch and not the `runpod-sdk` npm package: that SDK only covers SERVERLESS
 * endpoints (run/runSync/status/stream/health) — it has NO pod-management surface. Pod create/
 * terminate lives only in RunPod's GraphQL API, so we call it directly.
 *
 * NO PROVIDER-SIDE BACKSTOP (money-leak note): unlike Modal's `timeoutMs`, RunPod's
 * `podFindAndDeployOnDemand` has NO field that auto-stops or auto-terminates an on-demand GPU pod
 * after a duration or after idle (verified against the RunPod GraphQL spec + reference clients —
 * `idleTimeout` is a serverless/flex-worker concept, not an on-demand-pod control). So there is no
 * net under us: RELIABLE TEARDOWN IS THE ONLY THING THAT STOPS THE BILLING. That is why every
 * fetch here is timeout-bounded (a stalled call must abort, not hang past the Rust grace and get
 * SIGKILLed with the pod still up) and why teardown uses a tight bound to finish inside that grace.
 *
 * Backend gotchas, all load-bearing:
 *   - `dockerArgs` is the container CMD, appended to the image's baked entrypoint. The spec supplies
 *     exactly the args (never the entrypoint) — ollama's entrypoint is `/bin/ollama` so args are
 *     `serve`; infinity's is `infinity_emb` so args start `v2 …`; vLLM's is `vllm serve` so the
 *     first arg is the model id.
 *   - The server must listen on 0.0.0.0 for the RunPod proxy to reach it — ollama needs OLLAMA_HOST
 *     (spec env); infinity binds 0.0.0.0 by default; vLLM gets `--host 0.0.0.0`.
 *   - RunPod exposes an HTTP container port as `https://<podId>-<port>.proxy.runpod.net`. There is
 *     no in-pod exec primitive in podFindAndDeployOnDemand, so for ollama (which boots empty) we PULL
 *     the model CLIENT-SIDE over that proxy URL (`POST /api/pull`) once `serve` is up. infinity/vLLM
 *     auto-download the HF model on boot — no pull step.
 *   - The proxy URL is known the instant the pod id is returned, but the pod takes time to boot;
 *     the pull + embed probes retry until the provisioning budget runs out.
 *
 * The harness ({@link runRecipe}) owns the terminate-on-error wrapper and the provision-timeout
 * clamp, so this recipe is just: deploy, report via `ctx.onBox`, (pull,) verify, return.
 */

import {
  type ProvisionContext,
  type Provisioned,
  type Recipe,
  assertBudgetRemaining,
  endpointPath,
  errorMessage,
  fetchWithTimeout,
  log,
  pollUntil,
  runRecipe,
  verifyEmbed,
} from "../src/contract.js";
import { selectBackendSpec } from "./backends.mjs";

/**
 * Default provisioning budget (boot + pull + first serving response) when input omits it. A GPU
 * pod cold start + image pull + model pull takes minutes; this is generous on purpose.
 */
const DEFAULT_PROVISION_TIMEOUT_S = 900;
/** Cheap, broadly-available on-demand GPU. `input.gpu` overrides this. */
const DEFAULT_GPU_TYPE_ID = "NVIDIA RTX A4000";
/** Pod name prefix (RunPod shows this in the console). */
const POD_NAME = "rag-rat-cookbook";
/**
 * Container scratch disk (GiB). Holds the pulled image's writable layer + downloaded model weights.
 * Generous because the vLLM image alone is ~10–15 GiB; all-minilm weights are tiny, so the image is
 * the driver. Ephemeral, so the marginal disk cost is negligible.
 */
const CONTAINER_DISK_GB = 40;
/** RunPod GraphQL endpoint; the API key rides as a query param (RunPod's documented scheme). */
const RUNPOD_GRAPHQL_URL = "https://api.runpod.io/graphql";
/** Bound on each GraphQL call (deploy/terminate). Generous for deploy; tight enough not to hang. */
const GRAPHQL_TIMEOUT_MS = 30_000;
/**
 * Bound on the teardown GraphQL call. Kept under the Rust side's ~10s teardown grace so terminate
 * either completes or aborts-and-surfaces an error BEFORE rag-rat SIGKILLs us — a hung terminate
 * would otherwise leak a billed pod. See the NO PROVIDER-SIDE BACKSTOP note above.
 */
const TEARDOWN_TIMEOUT_MS = 8_000;
/** Per-attempt timeout for the client-side model pull poll loop. */
const PULL_ATTEMPT_TIMEOUT_MS = 15_000;
/** Delay between pull poll attempts. */
const PULL_POLL_INTERVAL_MS = 3_000;

/** Opaque handle handed to teardown: the pod id + the api key needed to terminate it. */
interface PodHandle {
  readonly podId: string;
  readonly apiKey: string;
}

/** Minimal shape of the pod object podFindAndDeployOnDemand returns. */
interface DeployedPod {
  readonly id: string;
}

/** Provision a RunPod GPU pod serving `input.model` on the selected backend; return its endpoint. */
async function provision(ctx: ProvisionContext<PodHandle>): Promise<Provisioned<PodHandle>> {
  const { input, provisionTimeoutMs } = ctx;
  const spec = selectBackendSpec(input.backend ?? "ollama");
  const port = spec.port;
  // ONE deadline for the WHOLE sequence (deploy + pull + verify). Each step below uses the budget
  // REMAINING until this deadline — never a fresh full budget — so the total stays inside the
  // provisioning budget and we abort (→ teardown) before the Rust provisioner's hard timeout
  // SIGKILLs us with the pod still billing. See `assertBudgetRemaining`.
  const deadline = Date.now() + provisionTimeoutMs;
  const rawKey = process.env["RUNPOD_API_KEY"];
  if (rawKey === undefined || rawKey.trim() === "") {
    throw new Error("RUNPOD_API_KEY is not set; rag-rat must pass the RunPod API key in the env.");
  }
  const apiKey = rawKey.trim();
  const gpuTypeId = input.gpu ?? DEFAULT_GPU_TYPE_ID;

  // Unique pod name so a deploy-timeout orphan sweep can find a pod that RunPod created but whose
  // create-response we lost (see below). The instant suffix makes it unambiguous across runs.
  const podName = `${POD_NAME}-${Date.now()}`;
  ctx.status(
    "provisioning",
    `deploying RunPod pod (backend=${spec.backend}, model=${input.model}, gpu=${gpuTypeId})`,
  );

  // Deploy the pod. The backend spec supplies the image, the dockerArgs (entrypoint args joined),
  // and the container env; `ports "<port>/http"` exposes the proxy. No persistent volume (ephemeral).
  //
  // DEPLOY-TIMEOUT ORPHAN (N3): the call is bounded at GRAPHQL_TIMEOUT_MS, but RunPod can CREATE the
  // pod and then lose/slow the response — `graphql` throws BEFORE `ctx.onBox`, so `runRecipe` would
  // tear down "nothing" while the created pod bills forever (no provider backstop). On ANY deploy
  // throw, sweep for a pod with our unique `podName` and terminate it, THEN rethrow (best-effort,
  // bounded — never masks the original error).
  let deploy: { podFindAndDeployOnDemand: DeployedPod | null };
  try {
    deploy = await graphql<{ podFindAndDeployOnDemand: DeployedPod | null }>(
      apiKey,
      POD_DEPLOY_MUTATION,
      {
        input: {
          cloudType: "ALL",
          gpuCount: 1,
          gpuTypeId,
          name: podName,
          imageName: spec.image(input),
          dockerArgs: spec.entrypointArgs(input).join(" "),
          ports: `${port}/http`,
          containerDiskInGb: CONTAINER_DISK_GB,
          volumeInGb: 0,
          env: Object.entries(spec.env(input)).map(([key, value]) => ({ key, value })),
        },
      },
      GRAPHQL_TIMEOUT_MS,
    );
  } catch (cause) {
    await sweepOrphanByName(apiKey, podName);
    throw cause;
  }
  const deployed = deploy.podFindAndDeployOnDemand;
  if (deployed === null || typeof deployed.id !== "string" || deployed.id === "") {
    throw new Error(
      `podFindAndDeployOnDemand returned no pod (capacity for "${gpuTypeId}" may be unavailable)`,
    );
  }
  const handle: PodHandle = { podId: deployed.id, apiKey };
  // Report the pod NOW so runRecipe terminates it if anything below throws — critical, since
  // RunPod has no idle/lifetime backstop and a leaked pod bills indefinitely.
  ctx.onBox(handle);
  log("info", `pod deployed: ${handle.podId}`);

  // RunPod proxies the HTTP container port at this stable host. Known immediately from the id.
  const endpoint = `https://${handle.podId}-${port}.proxy.runpod.net`;
  log("info", `proxy endpoint: ${endpoint}`);

  // Load the model. ollama boots empty, so pull CLIENT-SIDE over the proxy (no in-pod exec); retries
  // cover pod boot. infinity/vLLM auto-download the HF model on boot — nothing to do here.
  if (spec.modelLoad === "ollama-pull") {
    ctx.status("pulling", `pulling model "${input.model}" over the proxy (covers pod boot)`);
    await pullModel(endpoint, input.model, assertBudgetRemaining(deadline, "model pull"));
    log("info", `model "${input.model}" pulled`);
  }

  // Confirm the server actually embeds before we emit `ready`. Budget = the REMAINING time after the
  // pull, so deploy+pull+verify together stay inside the single provisioning budget.
  ctx.status("verifying", `probing ${spec.embedPath} for a real vector`);
  await verifyEmbed(endpoint, {
    model: input.model,
    embedPath: spec.embedPath,
    budgetMs: assertBudgetRemaining(deadline, "embed verification"),
  });
  log("info", "embed verification passed; pod is serving");

  // RunPod's HTTP proxy is open (no per-request token for proxied ports) → auth_token null.
  return { handle, endpoint, auth_token: null };
}

/**
 * Pull `model` via the Ollama proxy (`POST /api/pull`, non-streaming), retrying until the budget
 * runs out — the pod is still booting for the first several attempts. Each attempt is bounded so a
 * stalled proxy aborts rather than hanging past the budget. ollama-only.
 */
function pullModel(endpoint: string, model: string, budgetMs: number): Promise<void> {
  const url = endpointPath(endpoint, "/api/pull");
  return pollUntil<{ status?: unknown; error?: unknown }>(url, {
    label: `pull "${model}"`,
    budgetMs,
    body: { model, stream: false },
    pollIntervalMs: PULL_POLL_INTERVAL_MS,
    perAttemptTimeoutMs: PULL_ATTEMPT_TIMEOUT_MS,
    isReady: (body) => {
      if (body.status === "success") return { ready: true };
      const reason =
        typeof body.error === "string"
          ? `pull error: ${body.error}`
          : `unexpected /api/pull response: ${JSON.stringify(body).slice(0, 200)}`;
      return { ready: false, reason };
    },
  });
}

/** teardown = permanently terminate (delete) the pod, bounded so it can't hang past the grace. */
async function teardown(handle: PodHandle): Promise<void> {
  await graphql(handle.apiKey, POD_TERMINATE_MUTATION, { input: { podId: handle.podId } }, TEARDOWN_TIMEOUT_MS);
}

/**
 * Best-effort orphan sweep for the deploy-timeout race (N3): if `podFindAndDeployOnDemand` threw
 * but RunPod actually created a pod, that pod carries our unique `podName`. List the account's pods
 * and terminate every match. Bounded and fully swallowed — it must NEVER throw (the caller is about
 * to rethrow the original deploy error, which is the signal rag-rat acts on); it only logs.
 */
async function sweepOrphanByName(apiKey: string, podName: string): Promise<void> {
  try {
    const me = await graphql<{ myself: { pods: ReadonlyArray<{ id?: unknown; name?: unknown }> } }>(
      apiKey,
      LIST_PODS_QUERY,
      {},
      TEARDOWN_TIMEOUT_MS,
    );
    const orphans = me.myself.pods.filter((p) => p.name === podName && typeof p.id === "string");
    if (orphans.length === 0) {
      log("info", `orphan sweep: no pod named "${podName}" found (deploy likely never created one)`);
      return;
    }
    for (const pod of orphans) {
      const podId = pod.id as string;
      try {
        await graphql(apiKey, POD_TERMINATE_MUTATION, { input: { podId } }, TEARDOWN_TIMEOUT_MS);
        log("warn", `orphan sweep: terminated leaked pod ${podId} (named "${podName}")`);
      } catch (cause) {
        log("error", `orphan sweep: FAILED to terminate ${podId} (named "${podName}"): ${errorMessage(cause)}`);
      }
    }
  } catch (cause) {
    log("error", `orphan sweep: could not list pods to check for a leak: ${errorMessage(cause)}`);
  }
}

/** podFindAndDeployOnDemand — deploy an on-demand GPU pod; returns the pod with its id. */
const POD_DEPLOY_MUTATION = `
mutation Deploy($input: PodFindAndDeployOnDemandInput!) {
  podFindAndDeployOnDemand(input: $input) {
    id
    imageName
    machineId
  }
}`;

/** podTerminate — permanently delete a pod; returns Void. */
const POD_TERMINATE_MUTATION = `
mutation Terminate($input: PodTerminateInput!) {
  podTerminate(input: $input)
}`;

/** myself.pods — the account's current pods (id + name), for the deploy-timeout orphan sweep. */
const LIST_PODS_QUERY = `
query Pods {
  myself {
    pods {
      id
      name
    }
  }
}`;

/**
 * Execute a RunPod GraphQL operation with a hard timeout. The API key rides as the `api_key` query
 * param (RunPod's documented auth scheme for this endpoint). Throws on transport/abort, HTTP, or
 * GraphQL-level errors.
 */
async function graphql<T>(
  apiKey: string,
  query: string,
  variables: Record<string, unknown>,
  timeoutMs: number,
): Promise<T> {
  const res = await fetchWithTimeout(
    `${RUNPOD_GRAPHQL_URL}?api_key=${encodeURIComponent(apiKey)}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ query, variables }),
    },
    timeoutMs,
  );
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`RunPod GraphQL HTTP ${res.status} ${res.statusText}: ${text.slice(0, 300)}`);
  }
  let parsed: { data?: T; errors?: ReadonlyArray<{ message?: string }> };
  try {
    parsed = JSON.parse(text) as typeof parsed;
  } catch {
    throw new Error(`RunPod GraphQL returned non-JSON: ${text.slice(0, 300)}`);
  }
  if (parsed.errors !== undefined && parsed.errors.length > 0) {
    const msg = parsed.errors.map((e) => e.message ?? "(no message)").join("; ");
    throw new Error(`RunPod GraphQL error: ${msg}`);
  }
  if (parsed.data === undefined) {
    throw new Error("RunPod GraphQL returned no data");
  }
  return parsed.data;
}

const recipe: Recipe<PodHandle> = {
  provider: "runpod",
  defaultProvisionTimeoutS: DEFAULT_PROVISION_TIMEOUT_S,
  provision,
  teardown,
};

await runRecipe(recipe);
