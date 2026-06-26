/**
 * Recipe: provision an ephemeral GPU pod on RunPod, serve an embedding model with Ollama, hand
 * rag-rat the proxy endpoint, and terminate the pod on signal.
 *
 * Run (post-build):  RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' node dist/recipes/runpod-ollama.mjs
 * Run (dev):         RAG_RAT_COOKBOOK_INPUT='{"model":"all-minilm"}' npx tsx recipes/runpod-ollama.mts
 *
 * Auth: RUNPOD_API_KEY in the process env (NOT in RAG_RAT_COOKBOOK_INPUT). rag-rat puts the key in
 * the child env from the user's shell; this recipe errors + exits 1 before any handshake if unset.
 *
 * Why GraphQL-over-fetch and not the `runpod-sdk` npm package: that SDK only covers SERVERLESS
 * endpoints (run/runSync/status/stream/health) — it has NO pod-management surface. Pod create/
 * terminate lives only in RunPod's GraphQL API, so we call it directly.
 *
 * Provider gotchas, all load-bearing:
 *   - The ollama image ENTRYPOINT is `/bin/ollama`, so `dockerArgs: "serve"` runs `ollama serve`
 *     (NOT `"ollama serve"`, which would exec `ollama ollama serve`). dockerArgs sets the
 *     container CMD; it is appended to the entrypoint.
 *   - ollama binds 127.0.0.1:11434 by default, unreachable through the RunPod proxy. We set
 *     OLLAMA_HOST=0.0.0.0:11434 via the pod env so it listens on all interfaces.
 *   - RunPod exposes an HTTP container port as `https://<podId>-<port>.proxy.runpod.net`. There is
 *     no in-pod exec primitive in podFindAndDeployOnDemand, so we PULL the model CLIENT-SIDE over
 *     that proxy URL (`POST /api/pull`) once `serve` is up — no shell/exec inside the pod needed.
 *   - The proxy URL is known the instant the pod id is returned, but the pod takes time to boot;
 *     the pull + embed probes retry until the request-timeout budget runs out.
 */

import {
  type CookbookInput,
  log,
  runRecipe,
  sleep,
  verifyEmbed,
} from "../src/contract.js";

/** HTTP port ollama serves on inside the pod; the one port we proxy out. */
const OLLAMA_PORT = 11434;
/** Default per-request budget (boot + pull + first serving response) when input omits it. */
const DEFAULT_REQUEST_TIMEOUT_S = 900;
/** Cheap, broadly-available on-demand GPU. `input.gpu` overrides this. */
const DEFAULT_GPU_TYPE_ID = "NVIDIA RTX A4000";
/** Pod name prefix (RunPod shows this in the console). */
const POD_NAME = "rag-rat-cookbook-ollama";
/** Container scratch disk (GiB). Model weights live here; all-minilm is tiny, keep headroom. */
const CONTAINER_DISK_GB = 20;
/** RunPod GraphQL endpoint; the API key rides as a query param (RunPod's documented scheme). */
const RUNPOD_GRAPHQL_URL = "https://api.runpod.io/graphql";
/**
 * Idle-timeout backstop (seconds): RunPod stops a pod with no proxy traffic for this long, so a
 * leaked pod (rag-rat died before teardown) self-stops instead of billing forever. Belt to the
 * teardown braces.
 */
const IDLE_TIMEOUT_S = 300;

/** Opaque handle handed to teardown: the pod id + the api key needed to terminate it. */
interface PodHandle {
  readonly podId: string;
  readonly apiKey: string;
}

/** Minimal shape of the pod object podFindAndDeployOnDemand returns. */
interface DeployedPod {
  readonly id: string;
}

/** Provision a RunPod GPU pod serving `input.model` via Ollama and return its proxy endpoint. */
async function provision(input: CookbookInput): Promise<{
  handle: PodHandle;
  endpoint: string;
  auth_token: string | null;
}> {
  const apiKey = process.env["RUNPOD_API_KEY"];
  if (apiKey === undefined || apiKey.trim() === "") {
    throw new Error("RUNPOD_API_KEY is not set; rag-rat must pass the RunPod API key in the env.");
  }

  const requestTimeoutS = input.request_timeout_s ?? DEFAULT_REQUEST_TIMEOUT_S;
  const requestTimeoutMs = Math.max(1, requestTimeoutS) * 1000;
  const gpuTypeId = input.gpu ?? DEFAULT_GPU_TYPE_ID;

  log(
    `provisioning runpod pod: model=${input.model} gpu=${gpuTypeId} ` +
      `request_timeout_s=${requestTimeoutS}`,
  );

  // Deploy the pod. dockerArgs="serve" → `ollama serve`; OLLAMA_HOST binds all interfaces;
  // ports "11434/http" exposes the proxy. No persistent volume (ephemeral box).
  const pod = await graphql<{ podFindAndDeployOnDemand: DeployedPod | null }>(
    apiKey.trim(),
    POD_DEPLOY_MUTATION,
    {
      input: {
        cloudType: "ALL",
        gpuCount: 1,
        gpuTypeId,
        name: `${POD_NAME}-${Date.now()}`,
        imageName: "ollama/ollama:latest",
        dockerArgs: "serve",
        ports: `${OLLAMA_PORT}/http`,
        containerDiskInGb: CONTAINER_DISK_GB,
        volumeInGb: 0,
        env: [{ key: "OLLAMA_HOST", value: `0.0.0.0:${OLLAMA_PORT}` }],
      },
    },
  );
  const deployed = pod.podFindAndDeployOnDemand;
  if (deployed === null || typeof deployed.id !== "string" || deployed.id === "") {
    throw new Error(
      `podFindAndDeployOnDemand returned no pod (capacity for "${gpuTypeId}" may be unavailable)`,
    );
  }
  const podId = deployed.id;
  const handle: PodHandle = { podId, apiKey: apiKey.trim() };
  log(`pod deployed: ${podId}`);

  try {
    // RunPod proxies the HTTP container port at this stable host. Known immediately from the id.
    const endpoint = `https://${podId}-${OLLAMA_PORT}.proxy.runpod.net`;
    log(`proxy endpoint: ${endpoint}`);

    // Pull the model CLIENT-SIDE over the proxy (no in-pod exec). Retries cover pod boot time.
    await pullModel(endpoint, input.model, requestTimeoutMs);
    log(`model "${input.model}" pulled`);

    // Confirm the server actually embeds before handing rag-rat the endpoint.
    await verifyEmbed(endpoint, { model: input.model, budgetMs: requestTimeoutMs });
    log("embed verification passed; pod is serving");

    // RunPod's HTTP proxy is open (no per-request token for proxied ports) → auth_token null.
    return { handle, endpoint, auth_token: null };
  } catch (cause) {
    // Provisioning failed after the pod came up — terminate it so we don't lean on the idle backstop.
    log("provision error after pod deploy; terminating pod");
    await terminatePod(handle).catch((e) => log("terminate-on-error failed (idle backstop will reap):", e));
    throw cause;
  }
}

/**
 * Pull `model` via the Ollama proxy (`POST /api/pull`, non-streaming), retrying until the budget
 * runs out — the pod is still booting for the first several attempts.
 */
async function pullModel(endpoint: string, model: string, budgetMs: number): Promise<void> {
  const url = `${endpoint.replace(/\/+$/, "")}/api/pull`;
  const deadline = Date.now() + budgetMs;
  let lastError = "(no attempt made)";
  let attempt = 0;

  while (Date.now() < deadline) {
    attempt += 1;
    try {
      const res = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ model, stream: false }),
      });
      if (!res.ok) {
        lastError = `HTTP ${res.status} ${res.statusText}`;
      } else {
        const body = (await res.json()) as { status?: unknown; error?: unknown };
        if (body.status === "success") return;
        lastError =
          typeof body.error === "string"
            ? `pull error: ${body.error}`
            : `unexpected /api/pull response: ${JSON.stringify(body).slice(0, 200)}`;
      }
    } catch (cause) {
      // Connection refused / DNS not ready yet while the pod boots — expected early, keep retrying.
      lastError = (cause as Error).message;
    }
    log(`pull attempt ${attempt} not done (${lastError}); retrying…`);
    await sleep(3000);
  }
  throw new Error(`"ollama pull ${model}" never succeeded within ${budgetMs}ms; last error: ${lastError}`);
}

/** Terminate (permanently delete) the pod. */
async function terminatePod(handle: PodHandle): Promise<void> {
  await graphql(handle.apiKey, POD_TERMINATE_MUTATION, { input: { podId: handle.podId } });
}

/** teardown = permanently terminate the pod. */
async function teardown(handle: PodHandle): Promise<void> {
  await terminatePod(handle);
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

/**
 * Execute a RunPod GraphQL operation. The API key rides as the `api_key` query param (RunPod's
 * documented auth scheme for this endpoint). Throws on transport, HTTP, or GraphQL-level errors.
 */
async function graphql<T>(
  apiKey: string,
  query: string,
  variables: Record<string, unknown>,
): Promise<T> {
  const res = await fetch(`${RUNPOD_GRAPHQL_URL}?api_key=${encodeURIComponent(apiKey)}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
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

await runRecipe(provision, teardown);
