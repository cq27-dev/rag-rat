// Shared per-file data lane: one cached fetch per endpoint, consumed by the
// CodeLens providers, sidebar views, hover, overlays, and diagnostics —
// instead of each lane fetching its own copy of the same payload.
import type * as vscode from 'vscode';
import type {
  CloneRegion,
  CouplingPartner,
  DecisionRecord,
  FileMemory,
  LensClient,
  PapertrailRef,
  SymbolGraph,
} from './client';

export interface CloneGraphMeta {
  generation?: number;
  eligible?: boolean;
  stale?: boolean;
  finished_at_ms?: number | null;
  hidden_low_value_classes?: number;
  unavailable_reason?: string;
}

export interface FileData {
  at: number;
  symbols: SymbolGraph[];
  clones: CloneRegion[];
  cloneGraph: CloneGraphMeta | null;
  memories: FileMemory[];
  coupling: CouplingPartner[];
  refs: PapertrailRef[];
  decisions: DecisionRecord[];
}

const TTL_MS = 10_000;

/**
 * Files tracked at once. An editing session across a large repository visits far more files than a
 * reader ever looks at again, and every one of them would otherwise keep its payload — and its
 * fallback copy — alive for the lifetime of the window.
 */
const MAX_TRACKED_PATHS = 256;

/**
 * How long a lane's last value may stand in for a request that failed. The fallback exists to
 * survive a dropped request, not to keep a claim alive: memories and clone regions are anchored to
 * line numbers, so a payload re-served over an edited file draws warnings and overlays on lines
 * that have moved. A lane still failing after this goes empty — or, for clones, explicitly
 * unavailable — which is the honest answer once the last real one is old.
 */
const FALLBACK_MAX_AGE_MS = 60_000;

type Lane = 'symbols' | 'clones' | 'memories' | 'coupling' | 'papertrail';

/** Everything known about one file. Kept in ONE entry so the parts cannot drift apart. */
interface PathState {
  /** The most recent load's payload, served until `TTL_MS` elapses or the epoch moves. */
  fresh?: FileData;
  /**
   * The last payload seen for this file, kept across `invalidate` so a lane that fails on the next
   * load falls back to it. Without it a failed memory lane would render as "this file has no
   * memories" — clearing real diagnostics and CodeLenses over one dropped request.
   */
  lastGood?: FileData;
  /** When each lane of `lastGood` last ARRIVED, bounding how long it may be carried forward. */
  laneAt?: Partial<Record<Lane, number>>;
  /** The last load's representative error, whether it failed wholly or in a single lane. */
  failure?: unknown;
  /** The last load produced NOTHING — the only case whose signals must be cleared. */
  unusable?: boolean;
}

function laneValue<T>(lane: PromiseSettledResult<T>, fallback: T): T {
  return lane.status === 'fulfilled' ? lane.value : fallback;
}

export class FileStore {
  /** Least-recently-used last: `state` re-inserts on touch and the oldest entry is evicted. */
  private paths = new Map<string, PathState>();
  private pending = new Map<string, Promise<FileData | undefined>>();
  /** In-flight loads, so an invalidation can stop paying for answers it has already discarded. */
  private inflight = new Set<AbortController>();
  /**
   * Which server the `lastGood` payloads came from. Reuse is only sound while the SAME server is
   * answering, and discovery can re-point silently (a restarted server, a different port), so they
   * are dropped the moment it changes. Keying it here rather than asking callers to reset is what
   * makes that safe: no caller has to know which of its actions can change the endpoint.
   */
  private lastGoodEndpoint: string | undefined;
  private epoch = 0;
  /** See [`sourceEpoch`](FileStore#sourceEpoch) — moves only when the served identity changes. */
  private source = 0;
  private online = false;

  constructor(
    private readonly client: LensClient,
    private readonly theta: () => number,
    private readonly minTokens: () => number,
    private readonly documentPath: (document: vscode.TextDocument) => string | undefined,
    /** Which server is answering right now — see [`lastGoodEndpoint`](FileStore#lastGoodEndpoint). */
    private readonly endpoint: () => Promise<string>,
  ) {}

  /** The index moved: refetch everything, but keep each file's last payload as a fallback. */
  invalidate(): void {
    this.epoch += 1;
    this.pending.clear();
    for (const state of this.paths.values()) {
      state.fresh = undefined;
      state.failure = undefined;
      state.unusable = undefined;
    }
    this.abortInflight();
  }

  /**
   * The server or workspace identity may have changed. Nothing fetched before may be reused —
   * including the per-lane fallbacks, which would otherwise show one repository's data over
   * another's identically named file.
   */
  reset(): void {
    this.invalidate();
    this.forgetServedState();
  }

  /**
   * Which index state the store is serving, as an opaque counter that moves on every
   * `invalidate` / `reset` / reachability change.
   *
   * A caller that awaits data and then writes it to a surface reads this before and after: `data`
   * already REFUSES to return a payload computed under a superseded epoch, but the caller still
   * has to tell that refusal apart from a server that did not answer — both read as `undefined` —
   * and rendering the second over the first turns an ordinary index refresh into a cleared surface.
   */
  dataEpoch(): number {
    return this.epoch;
  }

  /**
   * WHERE the store is serving from, as an opaque counter that moves whenever everything fetched
   * so far stops being attributable to the server now answering: a `reset`, or a discovery
   * re-point observed while loading.
   *
   * Deliberately not `dataEpoch`, which also moves on every ordinary index invalidation. A caller
   * holding a rendered copy of a payload needs the two apart. The index moving means its copy is
   * merely out of date, and the replacement is already on its way — taking it down would flicker
   * every surface on every reindex. The SOURCE moving means its copy describes a different
   * repository or a different server, so it is not out of date but wrong, and it has to come down
   * NOW rather than stand for as long as the replacement request takes.
   */
  sourceEpoch(): number {
    return this.source;
  }

  /** The last load's error, if any — present for a partial load too, so the caller can log it. */
  failure(path: string): unknown {
    return this.paths.get(path)?.failure;
  }

  shouldClearSignals(path: string): boolean {
    return !this.online || this.paths.get(path)?.unusable === true;
  }

  setOnline(online: boolean): void {
    if (this.online !== online) {
      this.online = online;
      this.invalidate();
    }
  }

  pathOf(document: vscode.TextDocument): string | undefined {
    return this.documentPath(document);
  }

  /**
   * Index data for a document, with the path it describes — or `undefined` when the document has
   * no indexed path, the data is unavailable, or the document STOPPED describing that path while
   * the request was in flight.
   *
   * That last case is why this exists. Every consumer runs the same three steps: resolve a path,
   * await the data, draw. Between step two and step three the premise can expire — the user types
   * and the buffer goes dirty, or the workspace folder is replaced — and the answer that arrives
   * describes a file that is no longer what the editor is showing. Drawing it puts line-anchored
   * claims back on a buffer that was deliberately cleared moments earlier.
   *
   * Re-asking `pathOf` after the await is the whole check, but it has to happen in EVERY
   * consumer: overlays, diagnostics, both CodeLens providers, hovers, and the sidebar. Doing it
   * here is what stops that from being five independent chances to forget.
   */
  async dataFor(
    document: vscode.TextDocument,
  ): Promise<{ path: string; data: FileData } | undefined> {
    const path = this.pathOf(document);
    if (!path) {
      return undefined;
    }
    const data = await this.data(path);
    if (!data || this.pathOf(document) !== path) {
      return undefined;
    }
    return { path, data };
  }

  async data(path: string): Promise<FileData | undefined> {
    if (!this.online) {
      return undefined;
    }
    const hit = this.state(path).fresh;
    if (hit && Date.now() - hit.at < TTL_MS) {
      return hit;
    }
    const pending = this.pending.get(path);
    if (pending) {
      return await pending;
    }
    const epoch = this.epoch;
    const request = this.load(path, epoch, 0);
    this.pending.set(path, request);
    try {
      return await request;
    } finally {
      if (this.pending.get(path) === request) {
        this.pending.delete(path);
      }
    }
  }

  private async load(path: string, epoch: number, attempt: number): Promise<FileData | undefined> {
    const attemptControl = new AbortController();
    this.inflight.add(attemptControl);
    try {
      const endpoint = await this.endpoint();
      const signal = attemptControl.signal;
      // The lanes settle independently. `/api/file/clones` is the slow one — it can fall back to a
      // repository-wide scan on a linked worktree — and its timeout must not discard the graph,
      // memory, coupling, and papertrail answers that already arrived, which would clear the
      // file's signals and report the server as offline over a single slow lane.
      const lanes = await Promise.allSettled([
        this.client.fileSymbolGraph(path, signal),
        this.client.fileClonesFull(path, this.theta(), this.minTokens(), signal),
        this.client.fileMemories(path, signal),
        this.client.fileCoupling(path, signal),
        this.client.filePapertrail(path, signal),
      ]);
      if (!this.online || this.epoch !== epoch) {
        return undefined;
      }
      // A lane that fails is retried against the replacement endpoint when discovery re-points
      // mid-load, so this payload can span two servers — mixing one repository's memories with
      // another's clones for an identically named file. The identity is only trustworthy when it
      // is unchanged across the whole load.
      //
      // Resolving it can itself fail — the discovery file is momentarily absent while `rag-rat mcp`
      // restarts, a hosted status probe times out — and that must not turn five successful lanes
      // into a file-level failure that clears the editor's signals. Unconfirmed is not failed: drop
      // the payload, record nothing, and let the next refresh ask again.
      const settled = await this.endpoint().then(
        (value) => value,
        () => undefined,
      );
      // The identity read is another await, and an invalidation landing inside it must not be
      // overwritten by this now-stale payload — which would then be served for a further TTL.
      if (settled === undefined || !this.online || this.epoch !== epoch) {
        return undefined;
      }
      if (settled !== endpoint) {
        // Drop everything the previous server answered and load again, wholly against the settled
        // one. A second re-point is left to the next refresh rather than looping here.
        this.forgetServedState();
        return attempt === 0 ? await this.load(path, epoch, attempt + 1) : undefined;
      }
      const [symbols, clones, memories, coupling, papertrail] = lanes;
      const rejected = lanes.filter(
        (lane): lane is PromiseRejectedResult => lane.status === 'rejected',
      );
      if (rejected.length === lanes.length) {
        // Nothing arrived at all: that is a file-level failure, and the caller clears its signals.
        return this.recordFailure(path, rejected[0].reason);
      }
      this.noteServedEndpoint(endpoint);
      const entry = this.state(path);
      // A rejected lane keeps the last value that arrived for it, for a while. An empty array would
      // be a claim — "no memories on this file" — that the caller acts on by clearing diagnostics
      // and lenses, and a dropped request is not evidence for it. Carrying it forever is the other
      // error: each merged payload becomes the next one's fallback, so a lane that keeps failing
      // would re-assert line-anchored warnings over a file that has since been edited. Where there
      // is nothing left to carry, the clone lane at least has somewhere to say so; the sidebar
      // renders that as unavailable rather than as an empty result.
      const now = Date.now();
      const previous = entry.lastGood;
      const arrivedAt = entry.laneAt ?? {};
      const carried = <T>(lane: Lane, pick: (data: FileData) => T, empty: T): T =>
        previous && now - (arrivedAt[lane] ?? 0) < FALLBACK_MAX_AGE_MS ? pick(previous) : empty;
      const cloneLane = clones.status === 'fulfilled' ? clones.value : undefined;
      const papertrailLane = papertrail.status === 'fulfilled' ? papertrail.value : undefined;
      const data: FileData = {
        at: now,
        symbols: laneValue(symbols, carried('symbols', (d) => d.symbols, [])),
        clones: cloneLane ? cloneLane.clone_regions : carried('clones', (d) => d.clones, []),
        cloneGraph: cloneLane
          ? cloneLane.clone_graph ?? null
          : carried('clones', (d) => d.cloneGraph, null)
            ?? { eligible: false, unavailable_reason: 'lens request failed' },
        memories: laneValue(memories, carried('memories', (d) => d.memories, [])),
        coupling: laneValue(coupling, carried('coupling', (d) => d.coupling, [])),
        refs: papertrailLane ? papertrailLane.refs : carried('papertrail', (d) => d.refs, []),
        decisions: papertrailLane
          ? papertrailLane.decisions
          : carried('papertrail', (d) => d.decisions, []),
      };
      entry.fresh = data;
      entry.lastGood = data;
      entry.laneAt = {
        ...arrivedAt,
        ...(symbols.status === 'fulfilled' ? { symbols: now } : {}),
        ...(cloneLane ? { clones: now } : {}),
        ...(memories.status === 'fulfilled' ? { memories: now } : {}),
        ...(coupling.status === 'fulfilled' ? { coupling: now } : {}),
        ...(papertrailLane ? { papertrail: now } : {}),
      };
      entry.failure = rejected.length ? rejected[0].reason : undefined;
      entry.unusable = undefined;
      return data;
    } catch (error) {
      if (!this.online || this.epoch !== epoch) {
        return undefined;
      }
      return this.recordFailure(path, error);
    } finally {
      this.inflight.delete(attemptControl);
    }
  }

  /** The entry for `path`, created if absent, marked most recently used, evicting the oldest. */
  private state(path: string): PathState {
    const existing = this.paths.get(path);
    if (existing) {
      this.paths.delete(path);
      this.paths.set(path, existing);
      return existing;
    }
    const created: PathState = {};
    this.paths.set(path, created);
    while (this.paths.size > MAX_TRACKED_PATHS) {
      const oldest = this.paths.keys().next().value;
      if (oldest === undefined) {
        break;
      }
      this.paths.delete(oldest);
    }
    return created;
  }

  /**
   * Forget every payload a server answered, leaving the epoch and in-flight bookkeeping alone.
   * Failure state stays: those files still need their signals cleared, and a server changing
   * underneath is no evidence that they recovered.
   */
  private forgetServedState(): void {
    for (const state of this.paths.values()) {
      state.fresh = undefined;
      state.lastGood = undefined;
      state.laneAt = undefined;
    }
    this.lastGoodEndpoint = undefined;
    this.source += 1;
  }

  /**
   * Attribute what is about to be stored to the endpoint that answered it, dropping the fallbacks
   * of any earlier one.
   *
   * Both of the store's own writers of the served identity move `source` — this one and
   * `forgetServedState` — so a consumer holding rendered data learns that it now describes a
   * different server WITHOUT having to know which of the extension's actions can re-point
   * discovery. A stream failure is the case that makes that matter: it re-resolves the endpoint
   * but reloads without an explicit reset, so a server that came back on another port changes the
   * identity with nothing having declared it.
   */
  private noteServedEndpoint(endpoint: string): void {
    if (endpoint === this.lastGoodEndpoint) {
      return;
    }
    for (const state of this.paths.values()) {
      state.lastGood = undefined;
      state.laneAt = undefined;
    }
    this.lastGoodEndpoint = endpoint;
    this.source += 1;
  }

  /**
   * Stop paying for answers already discarded. The clone lane can be a repository-wide scan, and
   * the server frees its worker as soon as the request is aborted.
   */
  private abortInflight(): void {
    for (const control of this.inflight) {
      control.abort();
    }
    this.inflight.clear();
  }

  private recordFailure(path: string, error: unknown): undefined {
    const entry = this.state(path);
    entry.fresh = undefined;
    entry.failure = error;
    entry.unusable = true;
    return undefined;
  }
}
