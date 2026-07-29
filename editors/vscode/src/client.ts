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
 * name, so `qname` alone reports the union of their callers. `qname` remains as the fallback for
 * a row the server could not hand a handle for.
 */
export interface SymbolSelector {
  id: string | null;
  qname: string | null;
}

export interface FileSymbol {
  /** Opaque `sym_<hex>` symbol handle — pass it back verbatim; never parse it as a number. */
  id: string | null;
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

export interface VersionToken {
  generation: number;
  max_indexed_at_ms: number;
  git_dirty: string | null;
  revision: string;
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
      if (signal?.aborted) {
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
      throw new Error(`${route} -> ${res.status}: ${body}`);
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
  async fileSymbols(path: string): Promise<FileSymbol[]> {
    return (await this.get<{ symbols: FileSymbol[] }>('/api/file/symbols', { path })).symbols;
  }
  async fileClones(path: string, theta?: number, minTokens?: number): Promise<CloneRegion[]> {
    return (await this.fileClonesFull(path, theta, minTokens)).clone_regions;
  }

  async fileClonesFull(
    path: string,
    theta?: number,
    minTokens?: number,
    signal?: AbortSignal,
  ): Promise<{ clone_regions: CloneRegion[]; clone_graph?: Record<string, unknown> }> {
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
    return validateFileClones(await this.get<unknown>('/api/file/clones', params, signal));
  }
  async fileMemories(path: string, signal?: AbortSignal): Promise<FileMemory[]> {
    return (await this.get<{ memories: FileMemory[] }>('/api/file/memories', { path }, signal))
      .memories;
  }
  async fileSymbolGraph(path: string, signal?: AbortSignal): Promise<SymbolGraph[]> {
    return (await this.get<{ symbols: SymbolGraph[] }>('/api/file/graph', { path }, signal)).symbols;
  }
  async fileCoupling(path: string, signal?: AbortSignal): Promise<CouplingPartner[]> {
    return (await this.get<{ coupling: CouplingPartner[] }>('/api/file/coupling', { path }, signal))
      .coupling;
  }
  async filePapertrail(
    path: string,
    signal?: AbortSignal,
  ): Promise<{ refs: PapertrailRef[]; decisions: DecisionRecord[] }> {
    return this.get('/api/file/papertrail', { path }, signal);
  }
  /**
   * Callers of ONE symbol. Sends the handle when the row carried one so overloads stay apart, and
   * falls back to the qualified name only when it did not — the server then answers with every
   * symbol of that name, which is the older, ambiguous behaviour.
   */
  async symbolCallers(selector: SymbolSelector, limit = 50): Promise<unknown[]> {
    const params: Record<string, string> = { limit: String(limit) };
    if (selector.id) {
      params.id = selector.id;
    } else if (selector.qname) {
      params.qname = selector.qname;
    } else {
      throw new Error('symbolCallers needs a symbol handle or a qualified name');
    }
    return (await this.get<{ callers: unknown[] }>('/api/symbol/callers', params)).callers;
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
