/**
 * Per-backend server specs: the declarative differences between the three embedding servers a
 * recipe can provision (ollama, michaelfeil/infinity, vLLM). The PROVIDER recipes (modal.mts,
 * runpod.mts) own the box lifecycle (create/tunnel/teardown, all provider-specific); a
 * {@link BackendServerSpec} supplies the pieces that vary by SERVER — image, launch args, env, port,
 * the embeddings route, and how the model gets loaded — so one provider recipe serves all backends.
 *
 * All three speak the OpenAI-compatible embeddings API, so the embed CALL and {@link verifyEmbed}
 * are backend-independent; only the ROUTE differs (`embedPath`) — see the per-backend note on it.
 *
 * Launch facts (verified against upstream docs/live probes, 2026-07):
 *   - ollama    `ollama/ollama:latest`         entrypoint `/bin/ollama`  → `serve` (empty server;
 *               model pulled AFTER boot); binds 127.0.0.1 by default so OLLAMA_HOST must open it;
 *               serves `/v1/embeddings`; port 11434; CPU-capable.
 *   - infinity  `michaelf34/infinity:latest[-cpu]`  entrypoint `infinity_emb`
 *               → `v2 --model-id <hf> --port 7997`; auto-downloads the HF model on boot; binds
 *               0.0.0.0 by default; serves `/embeddings`; port 7997; CPU-capable.
 *   - vLLM      `vllm/vllm-openai:latest`      entrypoint `vllm serve`
 *               → `<hf> --runner pooling --host 0.0.0.0 --port 8000`; auto-downloads on boot; serves
 *               `/v1/embeddings`; port 8000; GPU-REQUIRED (no official CPU image).
 */

import type { Backend, CookbookInput } from "../src/contract.js";

/** The declarative, provider-independent description of one embedding backend. */
export interface BackendServerSpec {
  /** Which backend this spec serves — matches {@link CookbookInput.backend}. */
  readonly backend: Backend;
  /**
   * The internal port the server listens on — the one port a provider tunnels/proxies out. Fixed
   * per backend (we also pass it to the server via `entrypointArgs`, so the two never drift).
   */
  readonly port: number;
  /**
   * The OpenAI embeddings route this server answers on. MUST mirror `RemoteBackend::embed_path` on
   * the Rust side (ollama/vLLM → `/v1/embeddings`; infinity → `/embeddings`) — the recipe's
   * verification probe and rag-rat's real embed call have to hit the same URL.
   */
  readonly embedPath: string;
  /**
   * Whether this backend needs a GPU. vLLM's published image is CUDA-only, so a provider MUST attach
   * a GPU for it (the recipe defaults one in when the caller didn't). ollama/infinity are
   * CPU-capable, so a GPU is opt-in there.
   */
  readonly requiresGpu: boolean;
  /**
   * Model-load strategy once the server is listening:
   *   - `"in-launch"` — the launch args already name the model; the server auto-downloads it from
   *     HuggingFace on boot (infinity, vLLM). Nothing to do post-boot; {@link verifyEmbed} covers
   *     the download wait.
   *   - `"ollama-pull"` — the model must be pulled into a running ollama AFTER boot. The mechanism
   *     is provider-specific (Modal in-box exec vs RunPod client-side `/api/pull`), so the provider
   *     recipe owns it; this flag just says a pull step is needed.
   */
  readonly modelLoad: "in-launch" | "ollama-pull";
  /** The container image to run. May differ by CPU vs GPU (infinity), hence a function of `input`. */
  image(input: CookbookInput): string;
  /**
   * The args to pass AFTER the image's baked entrypoint. A provider hands these to the box verbatim
   * (Modal as a `command` array, RunPod joined into `dockerArgs`). Never include the entrypoint
   * itself — that's baked into the image.
   */
  entrypointArgs(input: CookbookInput): readonly string[];
  /** Container env as a plain record; a provider maps it to its own shape. */
  env(input: CookbookInput): Record<string, string>;
}

/** ollama serves on 11434 and binds loopback by default (OLLAMA_HOST opens it to the tunnel). */
const OLLAMA_PORT = 11434;
/** infinity's `v2` server default port. */
const INFINITY_PORT = 7997;
/** vLLM's OpenAI server default port. */
const VLLM_PORT = 8000;

const OLLAMA_SPEC: BackendServerSpec = {
  backend: "ollama",
  port: OLLAMA_PORT,
  embedPath: "/v1/embeddings",
  requiresGpu: false,
  modelLoad: "ollama-pull",
  image: () => "ollama/ollama:latest",
  entrypointArgs: () => ["serve"],
  env: (input) => ({
    // ollama binds 127.0.0.1 by default, unreachable through a tunnel/proxy — open all interfaces.
    OLLAMA_HOST: `0.0.0.0:${OLLAMA_PORT}`,
    // The server's max parallel requests (rag-rat's cap); rag-rat tunes client fan-out within it.
    OLLAMA_NUM_PARALLEL: String(input.server_concurrency ?? 1),
    // Bake the context window for THIS box's models when configured, so chunk vectors match the
    // freshness key that folds `num_ctx` in. Applies to every model this `serve` loads — including
    // one serving queries — which is exactly the chunk↔query consistency we need.
    ...(input.num_ctx != null ? { OLLAMA_CONTEXT_LENGTH: String(input.num_ctx) } : {}),
  }),
};

const INFINITY_SPEC: BackendServerSpec = {
  backend: "infinity",
  port: INFINITY_PORT,
  // infinity's OpenAI shape lives at `/embeddings` (it also mounts `/v1/embeddings`, but `/embeddings`
  // is canonical and is what `RemoteBackend::embed_path` uses).
  embedPath: "/embeddings",
  requiresGpu: false,
  modelLoad: "in-launch",
  // CPU image unless a GPU was explicitly requested (infinity is CPU-capable; the CPU tag is slimmer).
  image: (input) => (input.gpu != null ? "michaelf34/infinity:latest" : "michaelf34/infinity:latest-cpu"),
  // `infinity_emb` is the entrypoint; `v2` is the subcommand. `--model-id` is the HF id. infinity
  // binds 0.0.0.0 by default, so no host flag is needed. server_concurrency is intentionally NOT
  // mapped: infinity's async engine batches dynamically, so there's no concurrent-request knob —
  // rag-rat's client-side fan-out tune still applies.
  entrypointArgs: (input) => ["v2", "--model-id", input.model, "--port", String(INFINITY_PORT)],
  env: () => ({}),
};

const VLLM_SPEC: BackendServerSpec = {
  backend: "vllm",
  port: VLLM_PORT,
  embedPath: "/v1/embeddings",
  // vLLM's published image (`vllm/vllm-openai`) is CUDA-only — a provider must attach a GPU.
  requiresGpu: true,
  modelLoad: "in-launch",
  image: () => "vllm/vllm-openai:latest",
  // Entrypoint is `vllm serve`, so the model id is the first positional. `--runner pooling` puts
  // vLLM in embedding mode (current flag; the old `--task embed` is deprecated as of v0.11). vLLM
  // binds loopback unless `--host 0.0.0.0` is passed — the classic "up but unreachable" trap.
  entrypointArgs: (input) => {
    const args = [input.model, "--runner", "pooling", "--host", "0.0.0.0", "--port", String(VLLM_PORT)];
    // Map the concurrency cap to vLLM's max concurrent sequences when set.
    if (input.server_concurrency != null) {
      args.push("--max-num-seqs", String(input.server_concurrency));
    }
    return args;
  },
  env: () => ({}),
};

/** All backend specs, keyed by backend. The one lookup table {@link selectBackendSpec} resolves. */
const SPECS: Readonly<Record<Backend, BackendServerSpec>> = {
  ollama: OLLAMA_SPEC,
  infinity: INFINITY_SPEC,
  vllm: VLLM_SPEC,
};

/** Resolve the {@link BackendServerSpec} for `backend`. Total over the {@link Backend} union. */
export function selectBackendSpec(backend: Backend): BackendServerSpec {
  return SPECS[backend];
}
