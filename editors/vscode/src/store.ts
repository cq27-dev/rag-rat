// Shared per-file data lane: one cached fetch per endpoint, consumed by the
// CodeLens providers, sidebar views, hover, overlays, and diagnostics —
// instead of each lane fetching its own copy of the same payload.
import type * as vscode from 'vscode';
import { UNKNOWN_CONTENT } from './client';
import type {
  CloneRegion,
  CouplingPartner,
  DecisionRecord,
  FileAnswer,
  FileMemory,
  LaneContent,
  LensClient,
  PapertrailRef,
  SymbolGraph,
} from './client';
import type { DocumentDigest } from './content';

export interface CloneGraphMeta {
  generation?: number;
  eligible?: boolean;
  stale?: boolean;
  finished_at_ms?: number | null;
  hidden_low_value_classes?: number;
  unavailable_reason?: string;
}

/**
 * Where a lane's value in one payload came from.
 *
 * `current` — the lane's request answered in this load, so the value is what the server said.
 * `carried` — the lane failed and its previous value was substituted for it.
 * `empty` — the lane failed and nothing was left to stand in for it: no value recent enough to
 * carry, or one carried from bytes the file no longer holds. The value is a placeholder, not an
 * answer.
 *
 * Only `current` supports a claim about what the index does or does not hold. Without this a
 * consumer cannot tell a lane's answer from its fallback: an empty memory list means either "the
 * server said so" or "the lookup failed", and a memory present in one means either "it is there"
 * or "it was there up to a minute ago".
 */
export type LaneOrigin = 'current' | 'carried' | 'empty';

/**
 * What one document's load produced — an answer, or the reason there is none.
 *
 * A bare `undefined` collapsed two states a consumer has to act on differently. "Nothing came
 * back" is an outage or an ordinary race, and what is on screen may stand until a replacement
 * arrives; "the answer describes other bytes" is a healthy server on another revision, and what is
 * on screen is anchored to a file the editor no longer holds, so it has to come down NOW — and
 * saying "offline" for it sends the reader after a problem that is not there.
 *
 * The distinction rides the RESULT rather than a follow-up store lookup for the same reason lane
 * provenance rides the payload: a second question, asked after the first was answered, is answered
 * from state that may have moved on. A reindex landing between the two turns a withheld payload
 * back into an agreeing one, and the caller reports an outage for the healthy server this exists to
 * avoid blaming. It also costs a second five-lane load on a server already failing.
 */
export type FileLoad =
  | { kind: 'answer'; path: string; data: FileData }
  /**
   * The answer exists and was computed from bytes this document does not hold — see
   * [`agreeingWithContent`](FileStore#agreeingWithContent).
   */
  | { kind: 'other-content' }
  /** No answer: no indexed path, nothing served, or a premise that expired mid-load. */
  | { kind: 'none' };

export interface FileData {
  at: number;
  /**
   * Where each lane's value came from. Consumers that make claims about presence or absence
   * consult it; consumers that merely render whatever is there ignore it.
   */
  lanes: Record<Lane, LaneOrigin>;
  /**
   * What content each lane's value was computed from — see [`LaneContent`](LaneContent).
   *
   * Line numbers only mean anything relative to the bytes they were computed over, and the index
   * and the editor can sit on different ones: a branch switch leaves `repo_id` and `worktree_id`
   * matching, so the server keeps answering — about the other revision. This is what lets a
   * consumer notice, and it is per lane because the lanes settle independently and a carried-
   * forward one describes whatever it described when it arrived.
   */
  laneContent: Record<Lane, LaneContent>;
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

const LANES: readonly Lane[] = ['symbols', 'clones', 'memories', 'coupling', 'papertrail'];

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

function laneValue<T>(lane: PromiseSettledResult<FileAnswer<T>>, fallback: T): T {
  return lane.status === 'fulfilled' ? lane.value.value : fallback;
}

/**
 * Why the clone lane has nothing when a fallback was dropped for describing other bytes. The
 * sidebar renders it verbatim, so it reads as a state of the answer rather than as a failure.
 */
const OTHER_CONTENT_REASON = 'answer describes other file content';

/**
 * `data` with one lane's value replaced by the placeholder that means "this lane says nothing",
 * and its origin demoted to `empty` so no consumer reads the placeholder as an answer.
 *
 * Every lane's blank is the SAME one `load` uses when a lane fails with nothing left to carry, so
 * the two ways a lane can end up saying nothing are indistinguishable downstream — which is the
 * point: neither supports a claim.
 */
function withoutLane(data: FileData, lane: Lane): FileData {
  const lanes: Record<Lane, LaneOrigin> = { ...data.lanes };
  lanes[lane] = 'empty';
  const laneContent: Record<Lane, LaneContent> = { ...data.laneContent };
  laneContent[lane] = UNKNOWN_CONTENT;
  const blanked: FileData = { ...data, lanes, laneContent };
  switch (lane) {
    case 'symbols':
      return { ...blanked, symbols: [] };
    case 'clones':
      return {
        ...blanked,
        clones: [],
        cloneGraph: { eligible: false, unavailable_reason: OTHER_CONTENT_REASON },
      };
    case 'memories':
      return { ...blanked, memories: [] };
    case 'coupling':
      return { ...blanked, coupling: [] };
    case 'papertrail':
      return { ...blanked, refs: [], decisions: [] };
  }
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
    /**
     * The content hash of what a document holds, with the version it was taken for, or
     * `undefined` when it cannot be produced — see
     * [`agreeingWithContent`](FileStore#agreeingWithContent).
     */
    private readonly documentContentHash: (
      document: vscode.TextDocument,
    ) => Promise<DocumentDigest | undefined>,
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
   * Index data for a document, with the path it describes — or the reason there is none: see
   * [`FileLoad`](FileLoad).
   *
   * The premise-expiry case is why this exists. Every consumer runs the same three steps: resolve a
   * path, await the data, draw. Between step two and step three the premise can expire — the user
   * types and the buffer goes dirty, the workspace folder is replaced, the server stops answering —
   * and what arrives describes a file, or an index, that is no longer the one the editor is looking
   * at. Drawing it puts line-anchored claims back on a buffer that was deliberately cleared moments
   * earlier.
   *
   * Re-checking the premises after each await is the whole check, but it would otherwise have to
   * happen in EVERY consumer: overlays, diagnostics, both CodeLens providers, hovers, and the
   * sidebar. Doing it here is what stops that from being five independent chances to forget.
   */
  async dataFor(document: vscode.TextDocument): Promise<FileLoad> {
    const path = this.pathOf(document);
    if (!path) {
      return { kind: 'none' };
    }
    const data = await this.data(path);
    if (!data || this.pathOf(document) !== path) {
      return { kind: 'none' };
    }
    // The store state this payload is entitled to be released under, read AFTER it arrived: a load
    // that re-points mid-flight answers from the server serving NOW, so sampling before the load
    // would refuse the first payload of every new endpoint.
    const epoch = this.epoch;
    const source = this.source;
    const agreed = await this.agreeingWithContent(document, data);
    // Hashing is another await — a whole-file read, and a network round trip on a remote or
    // virtual workspace — and every premise the payload rests on can expire across it.
    //
    // A document that stopped describing this path says nothing about content agreement, so it is
    // `none` rather than a disagreement: the two are reported separately precisely so neither
    // stands in for the other.
    //
    // The store's OWN state is re-read for the same reason, and it is the one that bites hardest.
    // The push surfaces — overlays and diagnostics — have no second chance to notice: when the
    // server dies mid-hash the reload takes the store offline and `clearSignals()` clears them, so
    // releasing this payload afterwards paints the dead server's clone regions and memory warnings
    // straight back on, with nothing to correct them until a reconnect. Epoch and source are read
    // together because they answer different questions and neither implies the other: the index
    // moving (or reachability changing) versus a re-point to a server whose answers describe a
    // different repository. A dropped payload publishes nothing, which is sound because every
    // invalidation and re-point is paired with a refresh that asks again.
    if (
      this.pathOf(document) !== path
      || !this.online
      || this.epoch !== epoch
      || this.source !== source
    ) {
      return { kind: 'none' };
    }
    return agreed ? { kind: 'answer', path, data: agreed } : { kind: 'other-content' };
  }

  /**
   * `data` narrowed to what was computed from the bytes `document` is showing, or `undefined` when
   * the server's answer describes a different revision altogether.
   *
   * A hosted server and the local window can sit on different revisions of the same checkout: the
   * window switches branch, the server keeps serving what it indexed, and `repo_id` /
   * `worktree_id` both still match — so the association is valid while every line number in the
   * payload belongs to other bytes. Revision equality is not an identity property and must not be
   * bound to the association; disagreement here is normal and transient, so it suppresses rather
   * than errors.
   *
   * Content, not revision, is what is compared. It is strictly stronger — it catches branch skew,
   * an index that has not caught up, and an out-of-band edit with one mechanism — and it needs
   * nothing host-specific, where comparing git heads would need an extension API that virtual and
   * web workspaces do not have.
   *
   * What a disagreement means depends on WHERE the lane's value came from, which is why this reads
   * `lanes` as well as `laneContent`:
   *
   * - A lane that ANSWERED (`current`) and names other bytes is evidence the file moved under the
   *   answer, and that says nothing good about the lanes that agree — they may simply have been
   *   read on the other side of the move. The whole payload is withheld.
   * - A lane that ANSWERED and names NO FILE (`absent`) is the index stating it holds nothing for
   *   this path. Its own value is empty and truthful, so it is served; what it invalidates is
   *   every fallback beside it, which describes a file the index has since stopped holding. Those
   *   are dropped whether or not their hash still matches the buffer — matching bytes say the
   *   editor has not moved, not that the answer is still the index's.
   * - A lane CARRIED forward from a previous load names whatever it named when it arrived. That is
   *   the defined behaviour of the fallback, not a signal about the file, so it withholds only
   *   ITSELF: the lane is blanked and its origin becomes `empty`, which is exactly what a consumer
   *   needs to tell "no answer" from "answered with nothing". Withholding the payload instead
   *   would let one dropped request blank every line-anchored surface for a whole fallback window
   *   after a reindex — the outcome the carry-forward exists to prevent.
   *
   * Either way a carried value can never re-assert line anchors over bytes it was not computed
   * from, which is what bounds the fallback.
   *
   * Fails OPEN when no comparison is possible — no lane named any content (a server predating the
   * field), or this host cannot hash. Treating "cannot compare" as "disagrees" would silence every
   * surface permanently, which is worse than the skew being guarded against. A digest that no
   * longer describes the document is NOT that case: the buffer reloaded under the hash, so the
   * comparison it would support is meaningless and the payload is withheld until the next load.
   */
  private async agreeingWithContent(
    document: vscode.TextDocument,
    data: FileData,
  ): Promise<FileData | undefined> {
    const answered = LANES.filter((lane) => data.lanes[lane] === 'current').map(
      (lane) => data.laneContent[lane],
    );
    const claimed = new Set(
      answered.flatMap((content) => (content.kind === 'sha256' ? [content.sha256] : [])),
    );
    const fallbacks = LANES.flatMap((lane) => {
      const content = data.laneContent[lane];
      return data.lanes[lane] === 'carried' && content.kind === 'sha256'
        ? [{ lane, sha256: content.sha256 }]
        : [];
    });
    if (answered.some((content) => content.kind === 'absent')) {
      // Two lanes cannot both have answered truthfully if one names a file and the other names
      // none, so a hash beside an absence means the index moved mid-load and neither can be
      // trusted for this buffer.
      if (claimed.size > 0) {
        return undefined;
      }
      // EVERY fallback goes, not just the ones naming a hash: the disqualifier is that the index
      // holds no such file, which is true of a carried value whatever it was computed from.
      return LANES.filter((lane) => data.lanes[lane] === 'carried').reduce(withoutLane, data);
    }
    if (claimed.size === 0 && fallbacks.length === 0) {
      return data;
    }
    const digest = await this.documentContentHash(document);
    if (digest === undefined) {
      return data;
    }
    if (digest.version !== document.version) {
      return undefined;
    }
    const current = digest.sha256;
    if (claimed.size > 0 && !(claimed.size === 1 && claimed.has(current))) {
      return undefined;
    }
    const outdated = fallbacks.filter((held) => held.sha256 !== current).map(({ lane }) => lane);
    // A COPY: the store's cached payload keeps every lane, so a buffer that returns to the bytes a
    // fallback describes gets it back rather than having it dropped for good.
    return outdated.length === 0 ? data : outdated.reduce(withoutLane, data);
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
      const canCarry = (lane: Lane): boolean =>
        previous !== undefined && now - (arrivedAt[lane] ?? 0) < FALLBACK_MAX_AGE_MS;
      const carried = <T>(lane: Lane, pick: (data: FileData) => T, empty: T): T =>
        previous && canCarry(lane) ? pick(previous) : empty;
      const origin = (lane: Lane, answered: boolean): LaneOrigin =>
        answered ? 'current' : canCarry(lane) ? 'carried' : 'empty';
      const cloneLane = clones.status === 'fulfilled' ? clones.value.value : undefined;
      const papertrailLane =
        papertrail.status === 'fulfilled' ? papertrail.value.value : undefined;
      // A lane's content travels with its value, fallback included: a carried-forward value still
      // describes the content it was computed from, and saying otherwise would let the fallback
      // pass a content check it never earned.
      const laneContent = <T>(
        lane: Lane,
        settled: PromiseSettledResult<FileAnswer<T>>,
      ): LaneContent =>
        settled.status === 'fulfilled'
          ? settled.value.content
          : carried(lane, (d) => d.laneContent[lane], UNKNOWN_CONTENT);
      const data: FileData = {
        at: now,
        lanes: {
          symbols: origin('symbols', symbols.status === 'fulfilled'),
          clones: origin('clones', cloneLane !== undefined),
          memories: origin('memories', memories.status === 'fulfilled'),
          coupling: origin('coupling', coupling.status === 'fulfilled'),
          papertrail: origin('papertrail', papertrailLane !== undefined),
        },
        laneContent: {
          symbols: laneContent('symbols', symbols),
          clones: laneContent('clones', clones),
          memories: laneContent('memories', memories),
          coupling: laneContent('coupling', coupling),
          papertrail: laneContent('papertrail', papertrail),
        },
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
