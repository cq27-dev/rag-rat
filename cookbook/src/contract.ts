/**
 * @rag-rat/cookbook — the process contract between rag-rat (Rust) and a recipe.
 *
 * rag-rat spawns a recipe as a subprocess (`node recipe.mjs` or `npx tsx recipe.mts`),
 * passing its request as the JSON env var `RAG_RAT_COOKBOOK_INPUT`. The recipe provisions
 * an ephemeral remote box, streams TYPED JSONL EVENTS to stdout (status/log/ready/error),
 * then stays alive holding the box until it receives SIGTERM/SIGINT, at which point it tears
 * the box down and exits 0. The `ready` event means the box is serving; an `error` event means
 * provisioning failed before it ever served.
 *
 * Invariants (the Rust side is built against these — do not change without changing rag-rat):
 *   - stdout carries ONLY JSONL events — one `CookbookEvent` JSON object per line, nothing else.
 *     A future ratatui `log` view (#329) renders this stream live, so non-JSON on stdout is a bug.
 *   - all recipe diagnostics go out as `log` EVENTS (via `log()`), not to stderr. stderr is for
 *     genuine crashes only (an uncaught throw, a native abort).
 *   - the `ready` event is emitted only AFTER the box is up and verified serving.
 *   - after `ready` the process STAYS RUNNING until signaled.
 *   - on SIGTERM/SIGINT: emit a `tearing_down` status, teardown the box, then exit 0.
 *   - on provision failure: emit an `error` event, exit non-zero, BEFORE any `ready`.
 *
 * SECURITY: a cookbook recipe runs with the spawning user's full privileges and credentials
 * (Modal/RunPod tokens in the env, the shell, the network). rag-rat invokes it via `npx`, which
 * downloads and executes whatever the configured spec resolves to. Treat the cookbook spec like a
 * dependency you pin and trust — NEVER point rag-rat at an untrusted package name or a recipe path
 * you have not read. `npx -y <name>` runs arbitrary downloaded code as you.
 */

/** Env var rag-rat sets to the JSON-encoded {@link CookbookInput}. */
export const COOKBOOK_INPUT_ENV = "RAG_RAT_COOKBOOK_INPUT";

/** The providers a recipe can target. Tags every `status` event. */
export type Provider = "modal" | "runpod";

/** Provisioning lifecycle phases reported by a `status` event. */
export type Phase = "provisioning" | "pulling" | "verifying" | "tearing_down";

/** Severity for a `log` event. */
export type LogLevel = "info" | "warn" | "error";

/**
 * One line of the stdout JSONL stream. Exactly one of these per line; the `type` discriminant tells
 * rag-rat (and the #329 log view) which it is. Every event carries `ts` (epoch ms).
 *
 * THE WIRE SCHEMA — the Rust parser is built against these exact shapes; do not reorder/rename
 * fields without changing rag-rat:
 *   {"type":"status","phase":<Phase>,"provider":<Provider>,"detail":<string>,"ts":<ms>}
 *   {"type":"log","level":<LogLevel>,"message":<string>,"ts":<ms>}
 *   {"type":"ready","endpoint":<string>,"auth_token":<string|null>,"ts":<ms>}
 *   {"type":"error","message":<string>,"ts":<ms>}
 */
export type CookbookEvent =
  | { readonly type: "status"; readonly phase: Phase; readonly provider: Provider; readonly detail: string; readonly ts: number }
  | { readonly type: "log"; readonly level: LogLevel; readonly message: string; readonly ts: number }
  | { readonly type: "ready"; readonly endpoint: string; readonly auth_token: string | null; readonly ts: number }
  | { readonly type: "error"; readonly message: string; readonly ts: number };

/**
 * Write one {@link CookbookEvent} as a JSONL line to stdout — the ONLY thing that writes to stdout.
 * Stamps nothing itself; callers build the full event (each helper stamps `ts`).
 */
export function emit(event: CookbookEvent): void {
  process.stdout.write(JSON.stringify(event) + "\n");
}

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
   * Wall-clock budget (seconds) for the WHOLE provisioning sequence — box boot + model pull +
   * first serving response. A remote box's cold start takes MINUTES, so this is generous (the Rust
   * side sends ~280s). It is what {@link runRecipe} resolves into `ctx.provisionTimeoutMs`, the
   * `budgetMs` the recipes' poll loops use. Falls back to the recipe's `defaultProvisionTimeoutS`
   * when omitted/null.
   *
   * NOT the same as {@link request_timeout_s} — that is the Rust embedder's per-HTTP-request
   * timeout (~60s), far too short to boot a box. Conflating them is what made the e2e time out.
   */
  readonly provision_timeout_s?: number | null;
  /**
   * The Rust-side embedder's PER-REQUEST timeout (seconds), passed through for completeness. The
   * recipes do NOT use this as a provisioning budget — see {@link provision_timeout_s}.
   */
  readonly request_timeout_s?: number | null;
  /**
   * GPU spec passed through to the provider (e.g. "A10G", "T4", or a RunPod gpuTypeId), or
   * null/omitted for the recipe's default. CPU is the safe default where the provider supports it.
   */
  readonly gpu?: string | null;
  /**
   * Ollama server parallelism to set as `OLLAMA_NUM_PARALLEL` on the box. rag-rat sends the user's
   * `[remote] concurrency` CAP so the server can handle up to that many parallel `/api/embed`
   * requests. rag-rat then tunes the actual client fan-out (within the cap) itself, against the box.
   */
  readonly ollama_num_parallel?: number | null;
}

/**
 * What a recipe's `provision` returns: an opaque `handle` (passed back to `teardown`), plus the
 * serving endpoint. `runRecipe` turns this into the `ready` event. Keeping `handle` separate means
 * teardown can carry provider state (the sandbox object) that never crosses the stdout boundary.
 *
 * `auth_token` is the bearer token a caller must present to reach `endpoint`; `null` (or omitted)
 * when the tunnel is open (reachable with no token). The `ready` event normalizes a missing token
 * to an explicit `null`.
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

/**
 * Context `runRecipe` passes into a recipe's `provision`.
 *
 * @typeParam H - the recipe's opaque box handle.
 */
export interface ProvisionContext<H> {
  /** The validated, parsed request. */
  readonly input: CookbookInput;
  /**
   * The resolved provisioning budget in ms (input.provision_timeout_s clamped to >= 1s, or the
   * recipe's `defaultProvisionTimeoutS`). THIS is the `budgetMs` for the boot/pull/verify poll
   * loops — `runRecipe` owns the clamp so recipes don't each re-derive it. Generous (minutes), not
   * the per-request timeout.
   */
  readonly provisionTimeoutMs: number;
  /**
   * Report the box handle to the harness the INSTANT it exists (before the box is verified
   * serving). This lets `runRecipe` tear the box down if `provision` later throws — so recipes
   * no longer need their own `try { … } catch { terminate; throw }` wrapper. Call it once, right
   * after the provider returns the box id/handle.
   */
  readonly onBox: (handle: H) => void;
  /**
   * Emit a `status` event tagged with this recipe's provider. Call it at each lifecycle phase:
   * `provisioning` when creating the box, `pulling` around the model pull, and `verifying` around
   * the embed probe. (`tearing_down` is emitted by `runRecipe`, not the recipe.)
   */
  readonly status: (phase: Phase, detail: string) => void;
}

/** Provisions the box for `ctx`. Throws to signal failure (an `error` event is emitted). */
export type ProvisionFn<H> = (ctx: ProvisionContext<H>) => Promise<Provisioned<H>>;

/** Destroys the box identified by `handle`. Must be idempotent (may be called once). */
export type TeardownFn<H> = (handle: H) => Promise<void>;

/** A recipe = its provider tag, a provision step, a teardown step, and a default provisioning budget. */
export interface Recipe<H> {
  /** Which provider this recipe targets — tags every `status` event it (and `runRecipe`) emits. */
  readonly provider: Provider;
  /** Fallback provisioning budget (seconds) when the input omits `provision_timeout_s`. */
  readonly defaultProvisionTimeoutS: number;
  readonly provision: ProvisionFn<H>;
  readonly teardown: TeardownFn<H>;
}

/**
 * Emit a `log` event to stdout (the JSONL stream). This is how ALL recipe diagnostics travel — the
 * #329 log view renders them. Never writes to stderr (reserved for genuine crashes).
 */
export function log(level: LogLevel, message: string): void {
  emit({ type: "log", level, message, ts: Date.now() });
}

/** Emit a `status` event for `provider` at `phase`. Recipes use `ctx.status` (provider pre-bound). */
export function status(provider: Provider, phase: Phase, detail: string): void {
  emit({ type: "status", phase, provider, detail, ts: Date.now() });
}

/** Renders an unknown thrown value as a message string without assuming it's an `Error`. */
export function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
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
    throw new Error(`${COOKBOOK_INPUT_ENV} is not valid JSON: ${errorMessage(cause)}`);
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error(`${COOKBOOK_INPUT_ENV} must be a JSON object, got ${describe(parsed)}.`);
  }
  const obj = parsed as Record<string, unknown>;

  if (typeof obj["model"] !== "string" || obj["model"].trim() === "") {
    throw new Error(`${COOKBOOK_INPUT_ENV}.model must be a non-empty string.`);
  }

  // Optional positive-number budgets. If present they must be a finite positive NUMBER — a string
  // like "60" would slip through to budget arithmetic as NaN and break every poll loop.
  const provisionTimeout = optionalPositiveNumber(obj["provision_timeout_s"], "provision_timeout_s");
  const requestTimeout = optionalPositiveNumber(obj["request_timeout_s"], "request_timeout_s");
  const ollamaNumParallel = optionalPositiveInteger(
    obj["ollama_num_parallel"],
    "ollama_num_parallel",
  );

  // gpu: optional; if present must be a string (provider spec) or null.
  const gpu = obj["gpu"];
  if (gpu !== undefined && gpu !== null && typeof gpu !== "string") {
    throw new Error(`${COOKBOOK_INPUT_ENV}.gpu must be a string or null (got ${describe(gpu)}).`);
  }

  const result: CookbookInput = {
    model: obj["model"],
    ...(provisionTimeout !== undefined ? { provision_timeout_s: provisionTimeout } : {}),
    ...(requestTimeout !== undefined ? { request_timeout_s: requestTimeout } : {}),
    ...(typeof gpu === "string" ? { gpu } : gpu === null ? { gpu: null } : {}),
    ...(ollamaNumParallel !== undefined ? { ollama_num_parallel: ollamaNumParallel } : {}),
  };
  return result;
}

/** Validates an optional positive-number field; returns the number, or undefined if absent/null. */
function optionalPositiveNumber(value: unknown, field: string): number | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) {
    throw new Error(`${COOKBOOK_INPUT_ENV}.${field} must be a positive number (got ${describe(value)}).`);
  }
  return value;
}

/** Validates an optional positive integer field; returns the number, or undefined if absent/null. */
function optionalPositiveInteger(value: unknown, field: string): number | undefined {
  const number = optionalPositiveNumber(value, field);
  if (number === undefined) return undefined;
  if (!Number.isInteger(number)) {
    throw new Error(`${COOKBOOK_INPUT_ENV}.${field} must be a positive integer (got ${describe(value)}).`);
  }
  return number;
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

/**
 * The wall-clock budget left until `deadline` (epoch ms), floored at 0. A recipe computes ONE
 * `deadline = Date.now() + provisionTimeoutMs` at the start, then passes `remainingBudgetMs(deadline)`
 * as the `budgetMs` of EACH provisioning step. This keeps the whole sequence (deploy + pull + verify)
 * inside the SINGLE provisioning budget — without it, a step that already burned most of the budget
 * would hand the NEXT step a fresh full budget, and the total could blow past the Rust provisioner's
 * hard timeout, which SIGKILLs the process group before the recipe runs its SIGTERM teardown → a
 * leaked, billed box (RunPod has no provider backstop). Pair with {@link assertBudgetRemaining} to
 * THROW (so `runRecipe` tears the box down) once the budget is effectively exhausted.
 */
export function remainingBudgetMs(deadline: number): number {
  return Math.max(0, deadline - Date.now());
}

/**
 * Throw if fewer than `minMs` of the provisioning budget remain (default 1s). Call it before each
 * step so an exhausted budget aborts into `runRecipe`'s error path — which tears the box down
 * (SIGTERM) — rather than starting a step that can only run past the deadline and get SIGKILLed with
 * the box still up. `label` names the step in the error.
 */
export function assertBudgetRemaining(deadline: number, label: string, minMs = 1_000): number {
  const remaining = remainingBudgetMs(deadline);
  if (remaining < minMs) {
    throw new Error(
      `provisioning budget exhausted before "${label}" (${remaining}ms left); aborting so the box ` +
        `is torn down before the hard timeout kills us`,
    );
  }
  return remaining;
}

/**
 * Race `work` against a hard `ms` timeout: resolves/rejects with `work` if it settles first, else
 * rejects (naming `label`). The timeout timer is CLEARED the moment `work` settles, so it neither
 * leaks nor — critically — holds the event loop open past `work` (and, conversely, while `work` is
 * genuinely pending the live timer keeps the process alive so the timeout can actually fire). Use it
 * to put a wall around any await with no native timeout — Modal's `sandboxes.create`/`exec`/
 * `tunnels`, `sb.terminate()`, RunPod's `podTerminate`. The losing `work` keeps running in the
 * background after a timeout; that's fine here because the caller is about to throw into teardown.
 */
export function raceWithTimeout<T>(work: () => Promise<T>, ms: number, label: string): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`"${label}" exceeded its ${ms}ms timeout`));
    }, ms);
    work().then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (cause: unknown) => {
        clearTimeout(timer);
        reject(cause instanceof Error ? cause : new Error(String(cause)));
      },
    );
  });
}

/**
 * Race an unbounded `work` promise against the budget left until `deadline`: resolves with `work`'s
 * value if it wins, throws (naming `label`) if the budget runs out first. The one wrapper recipes
 * put around every SDK await that lacks its own timeout, so a single hang can't run past the Rust
 * provisioner's hard ceiling and get SIGKILLed before teardown. Throws immediately (does not even
 * start `work`) if the budget is already below `minMs` — see {@link assertBudgetRemaining}.
 */
export function withBudget<T>(
  deadline: number,
  label: string,
  work: () => Promise<T>,
  minMs = 1_000,
): Promise<T> {
  const remaining = assertBudgetRemaining(deadline, label, minMs);
  return raceWithTimeout(work, remaining, label);
}

/**
 * `fetch` with a hard per-call timeout via `AbortSignal.timeout`. Use this instead of a bare
 * `fetch` anywhere a stall would let the box leak: a hung connection must abort and surface as an
 * error, never hang past the caller's budget.
 */
export function fetchWithTimeout(
  url: string,
  init: RequestInit,
  timeoutMs: number,
): Promise<Response> {
  return fetch(url, { ...init, signal: AbortSignal.timeout(timeoutMs) });
}

/** Options for {@link pollUntil}. */
export interface PollUntilOptions<T> {
  /** Short label for log lines, e.g. "embed probe" / "pull". */
  readonly label: string;
  /** Total wall-clock budget in ms to keep retrying before throwing. */
  readonly budgetMs: number;
  /** Request method. Defaults to "POST". */
  readonly method?: string;
  /** JSON-serializable request body sent on every attempt. */
  readonly body: unknown;
  /** Extra headers (e.g. an auth bearer) merged onto `content-type: application/json`. */
  readonly headers?: Record<string, string>;
  /** Delay between attempts in ms. Defaults to 2000. */
  readonly pollIntervalMs?: number;
  /**
   * Per-attempt fetch timeout in ms. Defaults to `pollIntervalMs` (so a stalled attempt aborts
   * before the next would fire). Bounds each fetch so a hung request can't blow the budget.
   */
  readonly perAttemptTimeoutMs?: number;
  /**
   * Inspect a successful (2xx) parsed JSON body. Return `{ ready: true }` to finish, or
   * `{ ready: false, reason }` to keep polling with `reason` as the last-error context.
   */
  readonly isReady: (body: T) => { ready: true } | { ready: false; reason: string };
}

/**
 * Poll `url` with a JSON body until `isReady` is satisfied, or the budget elapses. The single
 * retry-until-ready loop shared by every readiness check ({@link verifyEmbed}, model pulls, …):
 * one place that owns the per-attempt timeout, the poll interval, and the budget accounting.
 *
 * Each attempt is bounded by `perAttemptTimeoutMs` (default = `pollIntervalMs`) so a stalled
 * server aborts rather than hanging past the budget — load-bearing, because a hung fetch on the
 * Rust side means a SIGKILL with no teardown and a leaked (billed) box.
 */
export async function pollUntil<T>(url: string, options: PollUntilOptions<T>): Promise<void> {
  const pollIntervalMs = options.pollIntervalMs ?? 2000;
  const perAttemptTimeoutMs = options.perAttemptTimeoutMs ?? pollIntervalMs;
  const deadline = Date.now() + options.budgetMs;
  // Floor below which it isn't worth starting another attempt — a sub-second fetch can't both
  // dial and respond, and starting it would only push us past `deadline`.
  const attemptFloorMs = 250;
  let lastError = "(no attempt made)";
  let attempt = 0;

  while (Date.now() < deadline) {
    // Clamp THIS attempt's timeout to whatever budget remains, so no single attempt can run past
    // `deadline` (the top-of-loop guard alone lets an attempt entered near the deadline still burn a
    // full `perAttemptTimeoutMs` + sleep, overshooting `budgetMs` by ~30s → past Rust's hard ceiling
    // → SIGKILL before teardown). See N4.
    const attemptTimeout = Math.min(perAttemptTimeoutMs, remainingBudgetMs(deadline));
    if (attemptTimeout < attemptFloorMs) break;
    attempt += 1;
    try {
      const res = await fetchWithTimeout(
        url,
        {
          method: options.method ?? "POST",
          headers: { "content-type": "application/json", ...options.headers },
          body: JSON.stringify(options.body),
        },
        attemptTimeout,
      );
      if (!res.ok) {
        lastError = `HTTP ${res.status} ${res.statusText}`;
      } else {
        const body = (await res.json()) as T;
        const verdict = options.isReady(body);
        if (verdict.ready) return;
        lastError = verdict.reason;
      }
    } catch (cause) {
      // Connection refused / DNS not ready / per-attempt abort while the box boots — expected
      // early; keep retrying until the budget runs out.
      lastError = errorMessage(cause);
    }
    // Skip the trailing sleep if it would itself cross the deadline — no point waiting only to
    // fail the loop guard. (The next attempt would in any case be floored out above.)
    if (Date.now() + pollIntervalMs >= deadline) break;
    log("info", `${options.label} attempt ${attempt} not ready (${lastError}); retrying…`);
    await sleep(pollIntervalMs);
  }
  throw new Error(
    `${options.label} did not become ready within ${options.budgetMs}ms; last error: ${lastError}`,
  );
}

/** Default delay between embed-readiness probes (ms). */
const VERIFY_EMBED_POLL_INTERVAL_MS = 3_000;
/**
 * Default per-attempt timeout for an embed probe (ms). The first successful embed on a
 * freshly-booted box can be slow (model load into memory), so this is generous — but still well
 * under any sane provisioning budget, so a genuinely stalled attempt aborts and retries.
 */
const VERIFY_EMBED_ATTEMPT_TIMEOUT_MS = 30_000;

/** Options for {@link verifyEmbed}. */
export interface VerifyEmbedOptions {
  /** Embedding model name to probe with (the value the server keys on). */
  readonly model: string;
  /** Wall-clock budget in ms to keep retrying before giving up. Use the provisioning budget. */
  readonly budgetMs: number;
  /** Extra headers (e.g. an auth bearer) to send with each probe. Defaults to none. */
  readonly headers?: Record<string, string>;
  /** Delay between retries in ms. Defaults to {@link VERIFY_EMBED_POLL_INTERVAL_MS}. */
  readonly pollIntervalMs?: number;
  /** Per-attempt fetch timeout in ms. Defaults to {@link VERIFY_EMBED_ATTEMPT_TIMEOUT_MS}. */
  readonly perAttemptTimeoutMs?: number;
}

/** Builds an endpoint-relative API URL without a regex over provider/user input. */
export function endpointPath(endpoint: string, path: string): string {
  let endpointEnd = endpoint.length;
  while (endpointEnd > 0 && endpoint.charCodeAt(endpointEnd - 1) === 47) {
    endpointEnd -= 1;
  }
  const normalizedPath = path.startsWith("/") ? path : `/${path}`;
  return `${endpoint.slice(0, endpointEnd)}${normalizedPath}`;
}

/**
 * Polls `<endpoint>/api/embed` until it returns a real embedding vector, or the budget runs out.
 *
 * Every recipe must call this BEFORE handing rag-rat the endpoint: a box can be reachable (tunnel
 * up) while ollama is still loading the model, so "box created" is not "box serving". A non-empty
 * `embeddings[0]` vector is the readiness signal. Throws if the budget elapses with no vector.
 */
export function verifyEmbed(endpoint: string, options: VerifyEmbedOptions): Promise<void> {
  const url = endpointPath(endpoint, "/api/embed");
  return pollUntil<{ embeddings?: unknown }>(url, {
    label: "embed probe",
    budgetMs: options.budgetMs,
    body: {
      model: options.model,
      input: "rag-rat embed readiness probe",
    },
    pollIntervalMs: options.pollIntervalMs ?? VERIFY_EMBED_POLL_INTERVAL_MS,
    perAttemptTimeoutMs: options.perAttemptTimeoutMs ?? VERIFY_EMBED_ATTEMPT_TIMEOUT_MS,
    ...(options.headers !== undefined ? { headers: options.headers } : {}),
    isReady: (body) => {
      const embeddings = body.embeddings;
      if (
        Array.isArray(embeddings) &&
        embeddings.length > 0 &&
        Array.isArray(embeddings[0]) &&
        embeddings[0].length > 0
      ) {
        return { ready: true };
      }
      return {
        ready: false,
        reason: `200 OK but no embedding vector: ${JSON.stringify(body).slice(0, 200)}`,
      };
    },
  });
}

/** Emit an `error` event (provisioning failed before `ready`). */
function emitError(message: string): void {
  emit({ type: "error", message, ts: Date.now() });
}

/**
 * Runs a recipe end-to-end against the rag-rat process contract (typed JSONL event stream).
 *
 * Flow:
 *   1. read+parse+validate {@link COOKBOOK_INPUT_ENV}; resolve the provisioning budget;
 *   2. install SIGTERM/SIGINT handlers (so a signal during provisioning still tears down);
 *   3. `provision(ctx)` — the recipe emits `status`/`log` events as it works; on throw: emit an
 *      `error` event, tear down any box it reported via `onBox`, exit 1, no `ready`;
 *   4. emit the `ready` event (box is serving);
 *   5. keep the process alive until a signal arrives;
 *   6. on signal: emit a `tearing_down` status, `teardown(handle)`, then exit 0.
 *
 * The harness owns the terminate-on-error wrapper and the timeout clamp so recipes stay thin: a
 * recipe reports its box via `ctx.onBox(handle)` the instant it exists, and `runRecipe` guarantees
 * exactly one teardown whether provisioning fails, the box serves, or a signal arrives mid-flight.
 *
 * In normal operation this never resolves (it parks until a signal terminates the process).
 */
export async function runRecipe<H>(recipe: Recipe<H>): Promise<void> {
  let input: CookbookInput;
  try {
    input = readInput();
  } catch (cause) {
    // Bad/absent input is a pre-ready failure: emit an `error` event, exit 1, no `ready`.
    emitError(`invalid input: ${errorMessage(cause)}`);
    process.exit(1);
  }

  // The boot/pull/verify budget — provision_timeout_s (generous, ~minutes), NOT request_timeout_s
  // (the Rust embedder's per-HTTP-request timeout, ~60s, far too short to cold-start a remote box).
  const provisionTimeoutMs =
    Math.max(1, input.provision_timeout_s ?? recipe.defaultProvisionTimeoutS) * 1000;

  // The one box handle, shared by every teardown path. Set by provision via `onBox`, or on the
  // happy path from the provision result.
  let box: { value: H } | null = null;

  // Teardown runs AT MOST ONCE: the first caller starts it and memoizes the in-flight promise;
  // every later caller (a second signal, the provision-failure path) awaits THAT SAME promise
  // instead of starting a new teardown. Crucially this means a second SIGTERM/SIGINT cannot
  // `process.exit` while `recipe.teardown` (a slow `podTerminate`/`sb.terminate`) is still in
  // flight — which would orphan a billed box (RunPod has no provider backstop). See N1.
  let teardownPromise: Promise<void> | null = null;

  const tearDownOnce = (reason: string): Promise<void> => {
    if (teardownPromise !== null) return teardownPromise;
    teardownPromise = (async () => {
      if (box === null) {
        log("info", `nothing to tear down (${reason}; no box was provisioned)`);
        return;
      }
      status(recipe.provider, "tearing_down", reason);
      await recipe.teardown(box.value);
      log("info", "teardown complete");
    })();
    return teardownPromise;
  };

  // Keep `process.on` (NOT `.once`): we must intercept EVERY signal so Node's default "terminate
  // immediately" action never fires mid-teardown. The shared `tearDownOnce` promise makes re-entry
  // safe — a second signal awaits the SAME in-flight teardown and only exits AFTER it settles, so
  // `process.exit` never runs while `recipe.teardown` is in flight.
  const onSignal = (signal: NodeJS.Signals): void => {
    void (async () => {
      try {
        await tearDownOnce(`received ${signal}`);
        process.exit(0);
      } catch (cause) {
        log("error", `teardown FAILED: ${errorMessage(cause)}`);
        // Exit non-zero so rag-rat knows the box may be leaked. RunPod has no provider backstop;
        // reliable teardown is the only net, so a failed teardown is a money-leak signal.
        process.exit(1);
      }
    })();
  };

  process.on("SIGTERM", onSignal);
  process.on("SIGINT", onSignal);

  let result: Provisioned<H>;
  try {
    result = await recipe.provision({
      input,
      provisionTimeoutMs,
      onBox: (handle) => {
        box = { value: handle };
      },
      status: (phase, detail) => {
        status(recipe.provider, phase, detail);
      },
    });
  } catch (cause) {
    emitError(`provisioning failed: ${errorMessage(cause)}`);
    // If a box was created before the failure, tear it down — recipes no longer do this themselves.
    await tearDownOnce("provisioning failed").catch((e) =>
      log("error", `terminate-on-error FAILED (box may be leaked): ${errorMessage(e)}`),
    );
    process.exit(1);
  }
  // Happy path: ensure the handle is captured even if the recipe didn't call onBox.
  box = { value: result.handle };

  // THE `ready` event — the box is serving. Normalizes a missing token to explicit null.
  emit({
    type: "ready",
    endpoint: result.endpoint,
    auth_token: result.auth_token ?? null,
    ts: Date.now(),
  });
  log("info", "box serving; holding until signaled");

  // Park forever. Node keeps the loop alive on the signal listeners; this timer is belt-and-braces
  // (and gives liveness even if listeners are detached by exotic runtimes). The process exits via
  // onSignal/exit, so this promise is intentionally never resolved.
  await new Promise<never>(() => {
    setInterval(() => {}, 1 << 30);
  });
}
