/**
 * @rag-rat/cookbook — the process contract between rag-rat (Rust) and a recipe.
 *
 * rag-rat spawns a recipe as a subprocess (`node recipe.mjs` or `npx tsx recipe.mts`),
 * passing its request as the JSON env var `RAG_RAT_COOKBOOK_INPUT`. The recipe provisions
 * an ephemeral remote box, prints exactly one handshake line to stdout once the box is up
 * and serving, then stays alive holding the box until it receives SIGTERM/SIGINT, at which
 * point it tears the box down and exits 0.
 *
 * Invariants (the Rust side is built against these — do not change without changing rag-rat):
 *   - stdout carries EXACTLY ONE line: the handshake JSON object. Nothing else, ever.
 *   - all logs/diagnostics go to stderr (use `log()` / console.error), never stdout.
 *   - the handshake is printed only AFTER the box is up and verified serving.
 *   - after the handshake the process STAYS RUNNING until signaled.
 *   - on SIGTERM/SIGINT: teardown the box, then exit 0.
 *   - on provision failure: error to stderr, exit non-zero, BEFORE any handshake.
 */

/** Env var rag-rat sets to the JSON-encoded {@link CookbookInput}. */
export const COOKBOOK_INPUT_ENV = "RAG_RAT_COOKBOOK_INPUT";

/**
 * The request rag-rat passes to a recipe via {@link COOKBOOK_INPUT_ENV}.
 *
 * This is the shared, provider-agnostic shape. A recipe may read additional provider-specific
 * fields off the same object, but every recipe must honor at least `model`.
 */
export interface CookbookInput {
  /** Embedding model to pull and serve, e.g. "all-minilm". */
  readonly model: string;
  /**
   * How long the recipe may wait (seconds) for a single provisioning request — pull, boot,
   * first serving response. Advisory; recipes clamp to their own sane bounds. Defaults vary
   * by recipe when omitted.
   */
  readonly request_timeout_s?: number | null;
  /**
   * GPU spec passed through to the provider (e.g. "A10G", "T4"), or null/omitted for CPU.
   * CPU is the safe default for v1 — see the modal-ollama recipe notes on GPU cold-start.
   */
  readonly gpu?: string | null;
}

/**
 * The single line a recipe prints to stdout once the box is up and serving.
 *
 * `auth_token` is the bearer/connect token a caller must present to reach `endpoint`; it is
 * `null` (or omitted) when the tunnel is open (reachable with no token). rag-rat treats a
 * missing field and an explicit null identically.
 */
export interface Handshake {
  /** Base HTTPS endpoint of the running model server, e.g. "https://abc123.modal.host". */
  readonly endpoint: string;
  /** Bearer token required to reach `endpoint`, or null/omitted for an open tunnel. */
  readonly auth_token?: string | null;
}

/**
 * What a recipe's `provision` returns: an opaque `handle` (passed back to `teardown`), plus
 * the handshake fields. Keeping `handle` separate from the handshake means teardown can carry
 * provider state (the sandbox object) that never crosses the stdout boundary.
 *
 * @typeParam H - the recipe's opaque box handle (e.g. a Modal `Sandbox`).
 */
export interface Provisioned<H> {
  /** Opaque box handle handed back to `teardown`. Never serialized. */
  readonly handle: H;
  /** Base HTTPS endpoint of the running model server. */
  readonly endpoint: string;
  /** Bearer token for `endpoint`, or null for an open tunnel. */
  readonly auth_token?: string | null;
}

/** Provisions the box for `input`. Throws to signal failure (no handshake is printed). */
export type ProvisionFn<H> = (input: CookbookInput) => Promise<Provisioned<H>>;

/** Destroys the box identified by `handle`. Must be idempotent (may be called once). */
export type TeardownFn<H> = (handle: H) => Promise<void>;

/** A recipe = a provision step paired with its teardown step. */
export interface Recipe<H> {
  readonly provision: ProvisionFn<H>;
  readonly teardown: TeardownFn<H>;
}

/** Structured log to stderr. Never writes to stdout (which is reserved for the handshake). */
export function log(...args: unknown[]): void {
  console.error("[cookbook]", ...args);
}

/** Reads and parses {@link COOKBOOK_INPUT_ENV}. Throws a clear error if absent or malformed. */
export function readInput(): CookbookInput {
  const raw = process.env[COOKBOOK_INPUT_ENV];
  if (raw === undefined || raw.trim() === "") {
    throw new Error(
      `${COOKBOOK_INPUT_ENV} is not set. rag-rat must pass the request as a JSON object in this env var.`,
    );
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (cause) {
    throw new Error(`${COOKBOOK_INPUT_ENV} is not valid JSON: ${(cause as Error).message}`);
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error(`${COOKBOOK_INPUT_ENV} must be a JSON object, got ${describe(parsed)}.`);
  }
  const obj = parsed as Record<string, unknown>;
  if (typeof obj["model"] !== "string" || obj["model"].trim() === "") {
    throw new Error(`${COOKBOOK_INPUT_ENV}.model must be a non-empty string.`);
  }
  return obj as unknown as CookbookInput;
}

function describe(v: unknown): string {
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  return typeof v;
}

/** Resolves after `ms` milliseconds. */
export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Options for {@link verifyEmbed}. */
export interface VerifyEmbedOptions {
  /** Embedding model name to probe with (the value the server keys on). */
  readonly model: string;
  /** Wall-clock budget in ms to keep retrying before giving up. */
  readonly budgetMs: number;
  /** Extra headers (e.g. an auth bearer) to send with each probe. Defaults to none. */
  readonly headers?: Record<string, string>;
  /** Delay between retries in ms. Defaults to 2000. */
  readonly pollIntervalMs?: number;
}

/**
 * Polls `<endpoint>/api/embed` until it returns a real embedding vector, or the budget runs out.
 *
 * Every recipe must call this BEFORE handing rag-rat the endpoint: a box can be reachable (tunnel
 * up) while ollama is still loading the model, so "box created" is not "box serving". A non-empty
 * `embeddings[0]` vector is the readiness signal. Throws if the budget elapses with no vector.
 *
 * Shared by every recipe so the readiness contract (Ollama `/api/embed`, retry-until-ready) lives
 * in exactly one place rather than copy-pasted per provider.
 */
export async function verifyEmbed(endpoint: string, options: VerifyEmbedOptions): Promise<void> {
  const url = `${endpoint.replace(/\/+$/, "")}/api/embed`;
  const pollIntervalMs = options.pollIntervalMs ?? 2000;
  const deadline = Date.now() + options.budgetMs;
  let lastError = "(no attempt made)";
  let attempt = 0;

  while (Date.now() < deadline) {
    attempt += 1;
    try {
      const res = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json", ...options.headers },
        body: JSON.stringify({ model: options.model, input: "rag-rat embed readiness probe" }),
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
    await sleep(pollIntervalMs);
  }
  throw new Error(
    `/api/embed never returned a vector within ${options.budgetMs}ms; last error: ${lastError}`,
  );
}

/** Serializes the handshake to its canonical one-line wire form (omitting an undefined token). */
export function encodeHandshake(h: Handshake): string {
  const wire: Handshake =
    h.auth_token === undefined
      ? { endpoint: h.endpoint }
      : { endpoint: h.endpoint, auth_token: h.auth_token };
  return JSON.stringify(wire);
}

/**
 * Runs a recipe end-to-end against the rag-rat process contract.
 *
 * Flow:
 *   1. read+parse {@link COOKBOOK_INPUT_ENV};
 *   2. install SIGTERM/SIGINT handlers (so a signal during provisioning still tears down);
 *   3. `provision(input)` — on throw: log to stderr, exit 1, no handshake;
 *   4. print the handshake JSON as the single stdout line;
 *   5. keep the process alive until a signal arrives;
 *   6. on signal: `teardown(handle)`, then exit 0.
 *
 * Resolves only on a fatal pre-handshake error path that opts to return rather than exit; in
 * normal operation it never resolves (it parks until a signal terminates the process).
 */
export async function runRecipe<H>(
  provision: ProvisionFn<H>,
  teardown: TeardownFn<H>,
): Promise<never> {
  let input: CookbookInput;
  try {
    input = readInput();
  } catch (cause) {
    // Bad/absent input is a pre-handshake failure: clean stderr message, non-zero exit, no stdout.
    log("invalid input:", cause instanceof Error ? cause.message : cause);
    process.exit(1);
  }

  // Handle is captured once provisioned so a signal mid-provision can still tear down.
  let handle: { value: H } | null = null;
  let tearingDown = false;

  const onSignal = (signal: NodeJS.Signals): void => {
    void (async () => {
      if (tearingDown) return;
      tearingDown = true;
      log(`received ${signal}, tearing down`);
      try {
        if (handle !== null) {
          await teardown(handle.value);
          log("teardown complete");
        } else {
          log("no box to tear down (signal arrived before provisioning completed)");
        }
        process.exit(0);
      } catch (cause) {
        log("teardown FAILED:", cause);
        // Exit non-zero so rag-rat knows the box may be leaked (the provider backstop is the net).
        process.exit(1);
      }
    })();
  };

  process.on("SIGTERM", onSignal);
  process.on("SIGINT", onSignal);

  let result: Provisioned<H>;
  try {
    result = await provision(input);
  } catch (cause) {
    log("provisioning FAILED:", cause instanceof Error ? cause.stack ?? cause.message : cause);
    process.exit(1);
  }
  handle = { value: result.handle };

  // THE handshake line — the only thing ever written to stdout.
  process.stdout.write(
    encodeHandshake({ endpoint: result.endpoint, auth_token: result.auth_token ?? null }) + "\n",
  );
  log("handshake emitted; holding box until signaled");

  // Park forever. Node keeps the loop alive on the signal listeners; this timer is belt-and-braces
  // (and gives `unref`-free liveness even if listeners are detached by exotic runtimes).
  await new Promise<never>(() => {
    setInterval(() => {}, 1 << 30);
  });
  // Unreachable: the promise above never resolves; the process exits via onSignal/exit.
  throw new Error("unreachable: runRecipe parked promise resolved");
}
