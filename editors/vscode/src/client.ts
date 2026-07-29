// Typed client for the rag-rat Lens server contract.
// Runs in both extension hosts: Node 22 desktop and the web worker (fetch is global in both).

import type { LensEndpoint, LensEndpointResolver } from './discovery';

export interface Status {
  repo_id: string;
  repo_root: string;
  indexed_root: string;
  case_insensitive_paths: boolean;
  live_files_generation: number;
  clone_graph_generation: number | null;
  indexed_head: string | null;
  live_file_count: number;
}

/**
 * How a hop request names its symbol. `id` — the server's opaque `sym_<hex>` symbol handle — is
 * the stable identity and the only selector that separates two overloads: they share a qualified
 * name, so `qname` alone reports the union of their callers. Declarations the server groups under
 * one identity share a handle too, and both the row that hands it out and the answer it produces
 * report how far it reaches. `qname` remains as the fallback for a row the server could not hand a
 * handle for, and for a handle it no longer knows.
 */
export interface SymbolSelector {
  id: string | null;
  qname: string | null;
}

/**
 * The `/api/symbol/callers` envelope, kept whole rather than unwrapped to `callers`: the server
 * says which selector it answered by and how many symbols that selector covered, and a reader
 * shown the union of two overloads' callers has to be told that is what it is.
 *
 * Both fields are OPTIONAL because the server and this extension version independently — a server
 * built before the hop routes took a handle sends neither, and absence must read as "this server
 * cannot say", never as an assumed `'id'`.
 */
export interface SymbolCallers {
  callers: unknown[];
  resolved_by?: 'id' | 'ref';
  /** Symbols the selector expanded to; `> 1` on EITHER lane means `callers` is their union. */
  matched_symbols?: number;
}

export interface FileSymbol {
  /** Opaque `sym_<hex>` symbol handle — pass it back verbatim; never parse it as a number. */
  id: string | null;
  /** Declarations `id` covers — see [`SymbolGraph.id_declarations`]. */
  id_declarations?: number;
  name: string;
  qname: string | null;
  kind: string;
  start_line: number;
  end_line: number;
  is_test: boolean;
  signature: string | null;
  fan_in: number;
  fan_out: number;
}

export interface CloneRefine {
  template: string;
  proposed_signature: unknown;
  variation_points: unknown;
  confidence: string;
  anti_unify_coverage: number;
  lcs_ratio: number;
  refactorability: number;
}

export interface ClonePartner {
  path: string;
  start_line: number | null;
  end_line: number | null;
  similarity: number;
  /** Name of the partner symbol (from exact anchor resolution). */
  symbol: string | null;
}

export interface CloneRegion {
  /** Cloned region in the requested file (null lines when outside any symbol). */
  start_line: number | null;
  end_line: number | null;
  byte_offset: number;
  /** Connected-component id over the generation's clone graph; siblings share it. */
  class_id: number | null;
  /** Name of the cloned symbol in the requested file. */
  symbol?: string | null;
  max_similarity: number | null;
  /** Sibling regions in other files, best similarity first. */
  partners: ClonePartner[];
  /** Persisted refine payload, present when the Lens component matches the pipeline class exactly. */
  refine?: CloneRefine;
}

export interface FileMemory {
  id: string;
  kind: string;
  title: string;
  body: string;
  confidence: string;
  binding_kind: string;
  path: string | null;
  /** Resolved display line (1-based) via the binding's symbol/chunk; null = file/dir-scoped. */
  line: number | null;
  anchor_status: string;
  /** LLM-compacted overview (dream --compact), gated on content freshness; null when absent/stale. */
  summary: string | null;
  /** Dream verify verdict ('current' | 'diverged'), freshness-gated; null when unverified. */
  verdict: string | null;
  /** note_ahead | code_ahead | unknown — which side moved since the verdict. */
  verdict_direction: string | null;
  /** JSON array of per-check strings from the dream verify pass. */
  verdict_evidence: string | null;
  checked_against_commit: string | null;
}

export interface SymbolGraph {
  /** Opaque `sym_<hex>` symbol handle — pass it back verbatim; never parse it as a number. */
  id: string | null;
  /**
   * How many declarations `id` covers. `1` for almost every row; `> 1` where the server groups
   * several declarations — a function's cfg variants, say — under one identity, so a hop request
   * carrying this handle is answered for all of them and no selector can narrow it further.
   *
   * OPTIONAL because a server built before this field sends nothing, and absence must read as
   * "this server cannot say" rather than an assumed `1`.
   */
  id_declarations?: number;
  name: string;
  qname: string | null;
  kind: string;
  start_line: number;
  end_line: number;
  is_test: boolean;
  callers: { exact: number; syntactic: number; name_only: number; ambiguous: number; tests: number; dispatch: number };
  fan_in_score: number;
  fan_in_bucket: 'low' | 'medium' | 'high' | 'critical';
  dispatch: { variant: string | null; direction: 'constructs' | 'handled'; other_name: string | null; other_path: string | null; other_line: number | null }[];
}

export interface CouplingPartner {
  path: string;
  co_changes: number;
  my_changes: number;
  confidence: number;
  last_co_change_at_s: number;
}

export interface PapertrailRef {
  tracker: string;
  project: string;
  item_key: string;
  item_kind: string;
  ref_kind: string;
  source_kind: string;
  source_text: string;
  title: string | null;
  url: string | null;
  state_normalized: string | null;
}

export interface DecisionRecord {
  item_key: string;
  title: string | null;
  url: string | null;
  root_issue: string | null;
  root_cause: string | null;
  decision_chosen: string | null;
  outcome_summary: string | null;
  outcome_status_model: string | null;
  outcome_claim_verified: number;
  decision_provenance_verified: number;
  fix_edge_source: string;
  /** Resolved line of the symbol anchor; null = file-level. */
  line: number | null;
}

/**
 * What content a per-file answer was computed from. Three states, because the two ways a payload
 * can carry no hash mean opposite things and a consumer acts differently on each:
 *
 * `sha256` — the `files.sha256` of the file's raw bytes at the revision the answer describes. A
 * consumer that makes line-anchored claims compares it against the file it has open and stays
 * quiet when they disagree.
 *
 * `absent` — the server answered and the ACTIVE SCOPE holds no such file (`content_sha256: null`).
 * That is a fact about the index, not a missing field: whatever another lane is carrying forward
 * for this path describes a file the index no longer holds.
 *
 * `unknown` — no comparison is possible, and it never means the contents disagree. A server
 * predating the field lands here, as does one naming something this client cannot read as a file
 * hash; the client then gates nothing, which is the behaviour it had before this field existed.
 */
export type LaneContent =
  | { kind: 'sha256'; sha256: string }
  | { kind: 'absent' }
  | { kind: 'unknown' };

/** No comparison is possible. Shared because it carries no per-answer state. */
export const UNKNOWN_CONTENT: LaneContent = { kind: 'unknown' };

/** One per-file answer, with the content it was computed from. */
export interface FileAnswer<T> {
  content: LaneContent;
  value: T;
}

export interface VersionToken {
  generation: number;
  max_indexed_at_ms: number;
  git_dirty: string | null;
  revision: string;
}

/**
 * A rejection that carries the status the Lens server answered with.
 *
 * Read it through [`lensHttpStatus`] rather than `instanceof`: the extension ships two bundles
 * (desktop and web worker) and a class identity is per-bundle, so the structural read is the one
 * that holds wherever the error is caught.
 */
export class LensHttpError extends Error {
  constructor(
    readonly lensHttpStatus: number,
    message: string,
  ) {
    super(message);
    this.name = 'LensHttpError';
  }
}

/** The status behind a rejection; `undefined` when nothing answered at all (transport failure). */
export function lensHttpStatus(error: unknown): number | undefined {
  const status = (error as { lensHttpStatus?: unknown } | null | undefined)?.lensHttpStatus;
  return typeof status === 'number' ? status : undefined;
}

/**
 * Whether re-reading discovery could change this outcome.
 *
 * A 4xx is a statement about the REQUEST: the discovered endpoint understood it and rejected it,
 * so re-deriving that endpoint cannot turn a 404 for a stale symbol handle — or a 400 for a
 * malformed one — into a hit. Invalidating on it discards a working endpoint, and the cached
 * identity every other lane shares, to arrive back at the same one. The credential statuses are
 * the exception, because the credential is exactly what discovery carries.
 *
 * A transport failure (no status at all) or a 5xx says nothing about the request, and either can
 * mean this is no longer the server to be talking to — the pre-existing retry stands there.
 */
function rediscoveryCanHelp(error: unknown): boolean {
  const status = lensHttpStatus(error);
  if (status === undefined || status >= 500) {
    return true;
  }
  return status === 401 || status === 403;
}

export class LensClient {
  constructor(private readonly resolver: LensEndpointResolver) {}

  private async get<T>(
    route: string,
    params?: Record<string, string>,
    signal?: AbortSignal,
  ): Promise<T> {
    const endpoint = await this.resolver.resolve();
    try {
      return await this.request<T>(endpoint, route, params, signal);
    } catch (error) {
      if (signal?.aborted || !rediscoveryCanHelp(error)) {
        throw error;
      }
      this.resolver.invalidate();
      const replacement = await this.resolver.resolve();
      if (replacement.baseUrl === endpoint.baseUrl && replacement.token === endpoint.token) {
        throw error;
      }
      return this.request<T>(replacement, route, params, signal);
    }
  }

  private async request<T>(
    endpoint: LensEndpoint,
    route: string,
    params?: Record<string, string>,
    signal?: AbortSignal,
  ): Promise<T> {
    const url = new URL(route, endpoint.baseUrl);
    for (const [k, v] of Object.entries(params ?? {})) {
      url.searchParams.set(k, v);
    }
    // The caller's signal rides alongside the timeout: a request whose answer has already been
    // discarded — an index change while a repository-wide clone scan is running — should stop
    // costing the server, not run to completion for a reader that has gone away.
    const res = await fetch(url.toString(), {
      headers: requestHeaders(endpoint),
      signal: eitherSignal(AbortSignal.timeout(35_000), signal),
    });
    if (!res.ok) {
      const body = (await res.text()).slice(0, 200);
      throw new LensHttpError(res.status, `${route} -> ${res.status}: ${body}`);
    }
    return (await res.json()) as T;
  }

  health(): Promise<{ status: string }> {
    return this.get('/api/health');
  }
  status(): Promise<Status> {
    return this.get('/api/status');
  }
  version(): Promise<VersionToken> {
    return this.get('/api/version');
  }
  async fileSymbols(path: string): Promise<FileAnswer<FileSymbol[]>> {
    const payload = await this.get<{ symbols: FileSymbol[] }>('/api/file/symbols', { path });
    return { content: answerContentOf(payload), value: payload.symbols };
  }
  async fileClones(
    path: string,
    theta?: number,
    minTokens?: number,
  ): Promise<FileAnswer<CloneRegion[]>> {
    const answer = await this.fileClonesFull(path, theta, minTokens);
    return { content: answer.content, value: answer.value.clone_regions };
  }

  async fileClonesFull(
    path: string,
    theta?: number,
    minTokens?: number,
    signal?: AbortSignal,
  ): Promise<FileAnswer<{ clone_regions: CloneRegion[]; clone_graph?: Record<string, unknown> }>> {
    const params: Record<string, string> = { path };
    if (theta != null) {
      params.theta = String(theta);
    }
    if (minTokens != null) {
      params.min_tokens = String(minTokens);
    }
    // The clone payload is the drift-prone boundary (nullable max_similarity, arbitrary refine
    // JSON). Validate and coerce at the edge so a hosted server version can never crash the
    // rendering lanes with `undefined.toFixed`.
    const payload = await this.get<unknown>('/api/file/clones', params, signal);
    return { content: answerContentOf(payload), value: validateFileClones(payload) };
  }
  async fileMemories(path: string, signal?: AbortSignal): Promise<FileAnswer<FileMemory[]>> {
    const payload = await this.get<{ memories: FileMemory[] }>(
      '/api/file/memories',
      { path },
      signal,
    );
    return { content: answerContentOf(payload), value: payload.memories };
  }
  async fileSymbolGraph(path: string, signal?: AbortSignal): Promise<FileAnswer<SymbolGraph[]>> {
    const payload = await this.get<{ symbols: SymbolGraph[] }>('/api/file/graph', { path }, signal);
    return { content: answerContentOf(payload), value: payload.symbols };
  }
  async fileCoupling(path: string, signal?: AbortSignal): Promise<FileAnswer<CouplingPartner[]>> {
    const payload = await this.get<{ coupling: CouplingPartner[] }>(
      '/api/file/coupling',
      { path },
      signal,
    );
    return { content: answerContentOf(payload), value: payload.coupling };
  }
  async filePapertrail(
    path: string,
    signal?: AbortSignal,
  ): Promise<FileAnswer<{ refs: PapertrailRef[]; decisions: DecisionRecord[] }>> {
    const payload = await this.get<{ refs: PapertrailRef[]; decisions: DecisionRecord[] }>(
      '/api/file/papertrail',
      { path },
      signal,
    );
    return {
      content: answerContentOf(payload),
      value: { refs: payload.refs, decisions: payload.decisions },
    };
  }
  /**
   * Callers of ONE symbol. Sends the handle when the row carried one so overloads stay apart, and
   * falls back to the qualified name when it did not — the server then answers with every symbol
   * of that name, which is the older, ambiguous behaviour. The whole envelope is returned so the
   * caller can say which of the two it got.
   *
   * The name is ALSO the fallback for a handle the server no longer knows. A handle is derived
   * from the declaration, so editing a signature mints a new one on the next index pass while the
   * CodeLens already drawn in the gutter still carries the old — a 404 there means the row went
   * stale, not that the symbol is gone, and the same row supplied a name that still resolves.
   * Answering wide (and saying so, through `matched_symbols`) beats failing a click that worked
   * before the handle lane existed. Retried only on 404: a 400 means this client built a
   * malformed handle, and quietly widening the answer would hide the bug that produced it.
   */
  // `async` so a selector-less call REJECTS rather than throwing synchronously: the command
  // handler awaits this, and a synchronous throw would escape its error path.
  async symbolCallers(selector: SymbolSelector, limit = 50): Promise<SymbolCallers> {
    const route = '/api/symbol/callers';
    const limits: Record<string, string> = { limit: String(limit) };
    if (selector.id) {
      try {
        return await this.get<SymbolCallers>(route, { ...limits, id: selector.id });
      } catch (error) {
        if (lensHttpStatus(error) !== 404 || !selector.qname) {
          throw error;
        }
      }
    }
    if (!selector.qname) {
      throw new Error('symbolCallers needs a symbol handle or a qualified name');
    }
    return this.get<SymbolCallers>(route, { ...limits, qname: selector.qname });
  }

  async watchVersions(signal: AbortSignal, onVersion: (version: VersionToken) => void): Promise<void> {
    const attempt = new AbortController();
    const abortAttempt = () => attempt.abort(signal.reason);
    if (signal.aborted) {
      abortAttempt();
    } else {
      signal.addEventListener('abort', abortAttempt, { once: true });
    }
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    try {
      const endpoint = await this.resolver.resolve();
      // `readWithTimeout` below only guards reads, which start AFTER response headers arrive. A
      // reverse proxy that accepts the connection and then never responds would park this `fetch`
      // with no deadline at all: it never rejects, so the reconnect loop never runs another
      // attempt and live refresh stays dead for the rest of the session even once the proxy
      // recovers — while REST requests keep working, so nothing else notices.
      const connectTimer = setTimeout(
        () => attempt.abort(new Error(`/api/events sent no response headers for ${SSE_CONNECT_MS / 1000}s`)),
        SSE_CONNECT_MS,
      );
      let response: Response;
      try {
        response = await fetch(new URL('/api/events', endpoint.baseUrl), {
          headers: requestHeaders(endpoint, 'text/event-stream'),
          signal: attempt.signal,
        });
      } finally {
        // Headers arrived (or the attempt failed): the stall timeout owns the connection now, and
        // leaving this armed would abort a healthy stream mid-flight.
        clearTimeout(connectTimer);
      }
      if (!response.ok) {
        throw new Error(`/api/events -> ${response.status}: ${(await response.text()).slice(0, 200)}`);
      }
      reader = response.body?.getReader();
      if (!reader) {
        throw new Error('/api/events returned no response body');
      }
      const decoder = new TextDecoder();
      let buffered = '';
      while (!signal.aborted) {
        const { done, value } = await readWithTimeout(reader, () => attempt.abort());
        if (done) {
          break;
        }
        buffered += decoder.decode(value, { stream: true });
        let boundary = eventBoundary(buffered);
        while (boundary) {
          const block = buffered.slice(0, boundary.index);
          buffered = buffered.slice(boundary.index + boundary.length);
          const event = parseVersionEvent(block);
          if (event) {
            onVersion(event);
          }
          boundary = eventBoundary(buffered);
        }
      }
      if (!signal.aborted) {
        this.resolver.invalidate();
        throw new Error('/api/events disconnected');
      }
    } finally {
      signal.removeEventListener('abort', abortAttempt);
      if (reader) {
        try {
          await reader.cancel();
        } catch {
          // Fetch aborts can error the body before cancellation observes it.
        }
      }
    }
  }
}

/**
 * Abort when either signal does. `AbortSignal.any` would say this in one line, but it arrived in
 * Node 20 and the extension still supports VS Code 1.85, whose desktop host is Node 18 — calling
 * it there would throw before the request was ever made.
 */
export function eitherSignal(deadline: AbortSignal, caller?: AbortSignal): AbortSignal {
  if (!caller) {
    return deadline;
  }
  const linked = new AbortController();
  const first = [deadline, caller].find((signal) => signal.aborted);
  if (first) {
    linked.abort(first.reason);
    return linked.signal;
  }
  for (const signal of [deadline, caller]) {
    signal.addEventListener('abort', () => linked.abort(signal.reason), { once: true });
  }
  return linked.signal;
}

function requestHeaders(endpoint: LensEndpoint, accept = 'application/json'): HeadersInit {
  return endpoint.token
    ? { Accept: accept, Authorization: `Bearer ${endpoint.token}` }
    : { Accept: accept };
}

/** The one shape `files.sha256` is rendered in: lower-hex over 32 bytes. */
const HEX_SHA256 = /^[0-9a-f]{64}$/;

/**
 * What content a per-file payload names — see [`LaneContent`](LaneContent).
 *
 * The three cases are told apart by the KEY, not by the value alone: a payload without
 * `content_sha256` comes from a server predating the field, while one carrying an explicit `null`
 * is a current server saying the active scope holds no such file. Collapsing them would let a
 * carried-forward lane outlive the index's own statement that the file is gone.
 *
 * Anything else a server might put there is read as `unknown` rather than as a hash. A value this
 * client cannot compare would otherwise fail every comparison and silence the file permanently,
 * where failing open only forgoes the gate.
 */
export function answerContentOf(payload: unknown): LaneContent {
  if (!isRecord(payload) || !('content_sha256' in payload)) {
    return UNKNOWN_CONTENT;
  }
  const named = payload.content_sha256;
  if (named === null) {
    return { kind: 'absent' };
  }
  if (typeof named !== 'string') {
    return UNKNOWN_CONTENT;
  }
  const sha256 = named.toLowerCase();
  return HEX_SHA256.test(sha256) ? { kind: 'sha256', sha256 } : UNKNOWN_CONTENT;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function numberOr(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function stringOrNull(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

export function validateFileClones(value: unknown): {
  clone_regions: CloneRegion[];
  clone_graph?: Record<string, unknown>;
} {
  const root = isRecord(value) ? value : {};
  return {
    clone_regions: Array.isArray(root.clone_regions)
      ? root.clone_regions.filter(isRecord).map(coerceRegion)
      : [],
    clone_graph: isRecord(root.clone_graph) ? root.clone_graph : undefined,
  };
}

function coerceRegion(value: Record<string, unknown>): CloneRegion {
  const refine = isRecord(value.refine) ? value.refine : undefined;
  return {
    start_line: numberOrNull(value.start_line),
    end_line: numberOrNull(value.end_line),
    byte_offset: numberOr(value.byte_offset, 0),
    class_id: numberOrNull(value.class_id),
    symbol: stringOrNull(value.symbol),
    max_similarity: numberOrNull(value.max_similarity),
    partners: Array.isArray(value.partners) ? value.partners.filter(isRecord).map(coercePartner) : [],
    refine: refine
      ? {
          template: typeof refine.template === 'string' ? refine.template : '',
          proposed_signature: refine.proposed_signature,
          variation_points: refine.variation_points,
          confidence: typeof refine.confidence === 'string' ? refine.confidence : '',
          anti_unify_coverage: numberOr(refine.anti_unify_coverage, 0),
          lcs_ratio: numberOr(refine.lcs_ratio, 0),
          refactorability: numberOr(refine.refactorability, 0),
        }
      : undefined,
  };
}

function coercePartner(value: Record<string, unknown>): ClonePartner {
  return {
    path: stringOrNull(value.path) ?? '',
    start_line: numberOrNull(value.start_line),
    end_line: numberOrNull(value.end_line),
    similarity: numberOr(value.similarity, 0),
    symbol: stringOrNull(value.symbol),
  };
}

// > 2× the server's 16s keep-alive interval: a silent half-open connection (sleep/wake, VPN,
// NAT drop) otherwise parks `reader.read()` forever and refresh silently dies.
const SSE_STALL_MS = 40_000;

// Establishing the stream is a normal request — it should answer as fast as `/api/status` does.
// Shorter than SSE_STALL_MS because nothing is streaming yet: this bounds only the wait for
// response headers, after which the stall timeout takes over for the life of the connection.
export const SSE_CONNECT_MS = 15_000;

export function readWithTimeout(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  abortAttempt: () => void,
  timeoutMs = SSE_STALL_MS,
): Promise<ReadableStreamReadResult<Uint8Array>> {
  return new Promise((resolve, reject) => {
    let settled = false;
    const timer = setTimeout(() => {
      settled = true;
      abortAttempt();
      void reader.cancel().catch(() => undefined);
      reject(new Error(`/api/events produced no data for ${timeoutMs / 1000}s`));
    }, timeoutMs);
    reader.read().then(
      (result) => {
        if (settled) {
          return;
        }
        settled = true;
        clearTimeout(timer);
        resolve(result);
      },
      (error) => {
        if (settled) {
          return;
        }
        settled = true;
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

function parseVersionEvent(block: string): VersionToken | undefined {
  const lines = block.split(/\r\n|\r|\n/);
  if (!lines.some((line) => line === 'event: version')) {
    return undefined;
  }
  const data = lines
    .filter((line) => line.startsWith('data:'))
    .map((line) => line.slice(5).trimStart())
    .join('\n');
  return data ? (JSON.parse(data) as VersionToken) : undefined;
}

function eventBoundary(buffered: string): { index: number; length: number } | undefined {
  const match = /\r\n\r\n|\n\n|\r\r/.exec(buffered);
  return match ? { index: match.index, length: match[0].length } : undefined;
}
