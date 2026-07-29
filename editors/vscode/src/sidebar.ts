// Sidebar tree views: per-file Clone Classes, Memories, and Issues & Decisions.
// All three describe the active editor's file, are filled by one pump off the
// active-editor event and the index-version poll, and read from the shared
// per-file FileStore (no per-view fetching).
import * as vscode from 'vscode';
import type { CloneRegion, DecisionRecord, PapertrailRef } from './client';
import { logError } from './output';
import type { FileData, FileStore } from './store';

class Item extends vscode.TreeItem {
  constructor(
    label: string,
    options?: {
      description?: string;
      tooltip?: string | vscode.MarkdownString;
      command?: vscode.Command;
      icon?: vscode.ThemeIcon;
      children?: Item[];
      contextValue?: string;
    },
  ) {
    super(
      label,
      options?.children?.length
        ? vscode.TreeItemCollapsibleState.Expanded
        : vscode.TreeItemCollapsibleState.None,
    );
    this.description = options?.description;
    this.tooltip = options?.tooltip;
    this.command = options?.command;
    this.iconPath = options?.icon;
    this.contextValue = options?.contextValue;
    if (options?.children?.length) {
      // TreeDataProvider children are carried via getChildren on the item itself.
      (this as { children?: Item[] }).children = options.children;
    }
  }
  children?: Item[];
}

/**
 * What the sidebar is showing: one file's data, or the reason there is none.
 *
 * A state rather than a rendered list, because the pump decides what is TRUE and each view decides
 * how to say it. That split is what lets the three views agree by construction, and it is where a
 * new truth goes — a further reason the data cannot be shown, or a qualification on data that can
 * (which lane of it is a carried-forward fallback, whether the file has moved under the answer) is
 * a member here plus a rendering arm, not a special case threaded through the load path.
 */
type SidebarState =
  | { kind: 'loading' }
  /** No active editor, or a buffer with no indexed path — an unsaved one, or outside the repo. */
  | { kind: 'unindexed' }
  | { kind: 'offline' }
  /** The load itself failed. Distinct from `offline`: nothing was learned about the server. */
  | { kind: 'failed' }
  | { kind: 'file'; path: string; data: FileData };

type SidebarMessage = Exclude<SidebarState, { kind: 'file' }>;

/** The one row a message state renders as. Built per view: a `TreeItem` belongs to one tree. */
function messageItem(state: SidebarMessage): Item {
  switch (state.kind) {
    case 'loading':
      return new Item('loading…', { icon: new vscode.ThemeIcon('loading~spin') });
    case 'unindexed':
      return new Item('open a file in the indexed repo', { icon: new vscode.ThemeIcon('info') });
    case 'offline':
      return new Item('lens server offline', { icon: new vscode.ThemeIcon('cloud-offline') });
    case 'failed':
      return new Item('lens data unavailable', {
        description: 'see the rag-rat Lens log',
        icon: new vscode.ThemeIcon('warning', new vscode.ThemeColor('editorWarning.foreground')),
      });
    default: {
      // Adding a state without a row is a compile error, not a blank tree at runtime.
      const unhandled: never = state;
      throw new Error(`unhandled sidebar state: ${JSON.stringify(unhandled)}`);
    }
  }
}

/**
 * One of the three trees. It holds what it is showing; the pump is the only writer.
 *
 * `getChildren` is deliberately SYNCHRONOUS. `TreeDataProvider.getChildren` has no cancellation
 * token and no way to decline to publish, so an implementation that awaits inside it makes
 * completion order the arbiter of what the sidebar shows: with several invocations in flight a
 * late one overwrites a newer one's result, and every value it can return is a publication — a
 * cached tree can predate an invalidation, an empty one can blank a tree a newer load just filled.
 * There is no safe return value, so the loading moves out instead. Reading state that is already
 * resolved makes a read atomic, and leaves only the load path having to decide whether it is still
 * entitled to write.
 */
abstract class FileView implements vscode.TreeDataProvider<Item> {
  private readonly emitter = new vscode.EventEmitter<Item | undefined>();
  readonly onDidChangeTreeData = this.emitter.event;
  /**
   * `loading`, never `[]`. The views exist before the first load can have answered, and an empty
   * tree is a claim — "this file has nothing" — that nothing has established yet.
   */
  private items: Item[] = [messageItem({ kind: 'loading' })];

  getTreeItem(element: Item): vscode.TreeItem {
    return element;
  }

  getChildren(element?: Item): Item[] {
    return element ? element.children ?? [] : this.items;
  }

  /**
   * This view's rows for `state`. PURE — it shows nothing on its own, so the pump can render all
   * three views before committing any of them.
   */
  render(state: SidebarState): Item[] {
    return state.kind === 'file' ? this.roots(state.path, state.data) : [messageItem(state)];
  }

  /** Show already-rendered rows and make VS Code re-read them. The pump is the only caller. */
  publish(items: Item[]): void {
    this.items = items;
    this.emitter.fire(undefined);
  }

  /**
   * This view's tree for one file's data. The contract is unchanged by the pump — only its caller
   * moved out of `getChildren`, so it now runs once a load is entitled to publish.
   */
  protected abstract roots(path: string, data: FileData): Item[];
}

/**
 * Loads the sidebar's data and publishes it to the three views.
 *
 * Everything the views show describes ONE document, loaded from ONE data source — so the premise a
 * load ran under is part of its answer, and an answer under another premise is wrong however good
 * the data is. This is where that is checked, on both sides of the await:
 *
 * - AFTER it, before publishing: the result is written only if this is still the newest load, still
 *   aimed at the active editor, and still under the index state it started from. Anything else is
 *   dropped rather than published — whatever superseded it publishes its own answer, which is why
 *   every `invalidate`/`reset` in the extension is paired with a `refresh`.
 * - BEFORE it, over what is already on screen: content whose premise no longer holds comes down
 *   immediately rather than standing until a replacement arrives, which can take as long as the
 *   request timeout.
 *
 * One pump for all three views rather than one each: they sit stacked in a single container, so a
 * slow view left describing the previous file beside two describing the current one is the same
 * wrong answer, only visibly. It also puts the decision to publish in one place instead of three.
 */
class SidebarPump {
  private generation = 0;
  /**
   * The premise the views' current CONTENT answers: which document it describes, and which data
   * source it came from. `undefined` while they hold a message — a message describes no file, so
   * no change of premise can falsify it.
   */
  private shown: { document: string; source: number } | undefined;

  constructor(
    private readonly store: FileStore,
    private readonly views: readonly FileView[],
  ) {}

  /** Reload for whatever editor is active now. */
  refresh(): void {
    // The load reports its own failures; this is the backstop for a throw inside that reporting.
    void this.load().catch((error: unknown) => logError('sidebar', error));
  }

  private async load(): Promise<void> {
    const generation = ++this.generation;
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      this.publish({ kind: 'unindexed' });
      return;
    }
    const document = editor.document.uri.toString();
    const source = this.store.sourceEpoch();
    if (this.shown && (this.shown.document !== document || this.shown.source !== source)) {
      // What is on screen answers a question nobody is asking any more — a different document, or a
      // server/repository the store has declared unusable — and leaving it up asserts it about the
      // file now on screen. Both premises are known to have changed BEFORE the load, so the tree
      // comes down now rather than when a replacement happens to arrive.
      //
      // Only a CHANGED premise clears. An ordinary index invalidation reloads the same document
      // from the same source, and emptying its tree on every version event would flicker for no
      // gain.
      this.publish({ kind: 'loading' });
    }
    const epoch = this.store.dataEpoch();
    // Settle the load into a value, the way `probeStatus` does: a rejection has to pass the same
    // entitlement checks as an answer before it may say anything.
    const settled = await this.store.dataFor(editor.document).then(
      (loaded) => ({ ok: true as const, loaded }),
      (error: unknown) => ({ ok: false as const, error }),
    );
    if (
      // A newer load owns the views. Same document and same epoch are not enough to make two loads
      // interchangeable: the store re-points to a replacement endpoint and reloads without moving
      // its epoch, so the earlier request can be carrying an older server's answer.
      generation !== this.generation ||
      // The host updates `activeTextEditor` before it delivers the change event, so a load can
      // settle after the user has moved on but before anything told the pump to reload.
      vscode.window.activeTextEditor?.document.uri.toString() !== document ||
      // The index moved. `dataFor` withholds a payload computed under the old epoch, and reporting
      // that withholding below as an outage would clear the tree over an ordinary reindex.
      this.store.dataEpoch() !== epoch
    ) {
      return;
    }
    if (!settled.ok) {
      // Nothing is known about this file, and a spinner left up forever says the opposite.
      logError('sidebar', settled.error);
      this.publish({ kind: 'failed' });
      return;
    }
    if (!settled.loaded) {
      // Distinguish "nothing to show for this buffer" from "the server did not answer": an
      // unsaved buffer has no indexed path, and reporting that as an outage would be wrong.
      this.publish(
        this.store.pathOf(editor.document) ? { kind: 'offline' } : { kind: 'unindexed' },
      );
      return;
    }
    this.publish({ kind: 'file', path: settled.loaded.path, data: settled.loaded.data }, document);
  }

  /**
   * Show `state` in all three views. `document` is the document a `file` state answers about, and
   * recording it is what lets a later load tell whether the views still describe anything.
   */
  private publish(state: SidebarState, document?: string): void {
    let rendered: readonly (readonly [FileView, Item[]])[];
    try {
      // Render every view BEFORE committing any: a payload one view's builder cannot turn into
      // rows would otherwise leave the other two describing it, which is exactly the disagreement
      // a single pump exists to prevent.
      rendered = this.views.map((view) => [view, view.render(state)] as const);
    } catch (error) {
      logError('sidebar', error);
      if (state.kind !== 'failed') {
        this.publish({ kind: 'failed' });
      }
      return;
    }
    // Read the source HERE, after the load: one that re-points mid-flight answers from the source
    // serving now, not the one it started under.
    this.shown = document === undefined ? undefined : { document, source: this.store.sourceEpoch() };
    for (const [view, items] of rendered) {
      view.publish(items);
    }
  }
}

export class CloneClassesView extends FileView {
  protected roots(path: string, data: FileData): Item[] {
    const out: Item[] = [];
    const unavailable = cloneGraphUnavailableReason(data.cloneGraph);
    if (unavailable) {
      return [
        new Item('clone data unavailable', {
          description: unavailable,
          icon: new vscode.ThemeIcon('warning', new vscode.ThemeColor('editorWarning.foreground')),
        }),
      ];
    }
    if (data.cloneGraph?.stale) {
      const built = data.cloneGraph.finished_at_ms
        ? new Date(data.cloneGraph.finished_at_ms).toLocaleString()
        : 'unknown';
      out.push(
        new Item('clone data stale', {
          description: `graph gen ${data.cloneGraph.generation} built ${built} — recently edited files may show nothing`,
          icon: new vscode.ThemeIcon('warning', new vscode.ThemeColor('editorWarning.foreground')),
        }),
      );
    }
    const regions = data.clones;
    if (!regions.length) {
      out.push(new Item('no actionable clones in this file', { icon: new vscode.ThemeIcon('check') }));
      return out;
    }
    const byClass = new Map<number | null, CloneRegion[]>();
    for (const r of regions) {
      const list = byClass.get(r.class_id) ?? [];
      list.push(r);
      byClass.set(r.class_id, list);
    }
    for (const [gid, members] of [...byClass.entries()].sort((a, b) => (a[0] ?? 1e9) - (b[0] ?? 1e9))) {
      const maxSim = Math.max(...members.map((m) => m.max_similarity ?? 0));
      out.push(
        new Item(gid != null ? `clone class ${gid}` : 'similar code', {
          description: `${members.length} region${members.length === 1 ? '' : 's'} · best ${maxSim.toFixed(2)}`,
          icon: new vscode.ThemeIcon(gid != null ? 'symbol-namespace' : 'symbol-misc'),
          children: members.map((r) => regionItem(path, r)),
        }),
      );
    }
    return out;
  }
}

export function cloneGraphUnavailableReason(meta: FileData['cloneGraph']): string | undefined {
  if (meta && meta.eligible !== false) {
    return undefined;
  }
  switch (meta?.unavailable_reason) {
    case 'missing_generation':
      return 'clone graph is missing; rebuild the index';
    case 'normalizer_mismatch':
      return 'clone graph is incompatible; rebuild the index';
    default:
      return meta?.unavailable_reason
        ? `clone analysis unavailable (${meta.unavailable_reason})`
        : 'clone graph is unavailable; rebuild the index';
  }
}

function regionItem(path: string, r: CloneRegion): Item {
  const siblings = r.partners.map(
    (p) =>
      new Item(`${p.path === path ? 'this file' : p.path}:${p.start_line ?? '?'} · ${p.symbol ?? ''}`, {
        description: p.similarity.toFixed(2),
        icon: new vscode.ThemeIcon('references'),
        command: {
          title: 'open',
          command: 'rag-rat-lens.openLocation',
          arguments: [{ path: p.path, line: p.start_line ?? 1 }],
        },
      }),
  );
  return new Item(`${r.symbol ?? '?'} :${r.start_line}-${r.end_line}`, {
    description: r.refine ? `refactorability ${r.refine.refactorability.toFixed(2)}` : undefined,
    icon: new vscode.ThemeIcon('symbol-method'),
    tooltip: `${r.partners.length} similar regions`,
    command: r.refine
      ? { title: 'extract preview', command: 'rag-rat-lens.showExtract', arguments: [r] }
      : { title: 'open', command: 'rag-rat-lens.openLocation', arguments: [{ path, line: r.start_line ?? 1 }] },
    children: siblings.length ? siblings : undefined,
  });
}

export class MemoriesView extends FileView {
  protected roots(path: string, data: FileData): Item[] {
    const memories = data.memories;
    if (!memories.length) {
      return [new Item('no memories bound to this file', { icon: new vscode.ThemeIcon('check') })];
    }
    return memories.map((m) => {
      const diverged = m.verdict === 'diverged';
      return new Item(m.title, {
        description: `${m.kind}${m.line ? ` :${m.line}` : ''}${m.verdict ? ` · ${m.verdict}` : ''}`,
        icon: new vscode.ThemeIcon(diverged ? 'warning' : 'bookmark', diverged ? new vscode.ThemeColor('errorForeground') : undefined),
        tooltip: `${m.kind} · ${m.confidence} · ${m.anchor_status}${m.verdict_direction ? ` · ${m.verdict_direction}` : ''}`,
        command: { title: 'show', command: 'rag-rat-lens.showMemories', arguments: [[m], path] },
        contextValue: diverged ? 'rag-rat-memory-diverged' : 'rag-rat-memory',
      });
    });
  }
}

export class PapertrailView extends FileView {
  protected roots(_path: string, data: FileData): Item[] {
    const { refs, decisions } = data;
    const out: Item[] = [];
    if (decisions.length) {
      out.push(
        new Item('decision records', {
          icon: new vscode.ThemeIcon('lightbulb'),
          children: decisions.map((d: DecisionRecord) =>
            new Item(`#${d.item_key} ${d.title ?? ''}`, {
              description: d.line ? `:${d.line}` : 'file-level',
              icon: new vscode.ThemeIcon('law'),
              command: { title: 'open', command: 'rag-rat-lens.showDecision', arguments: [d] },
            }),
          ),
        }),
      );
    }
    if (refs.length) {
      const { open, closed, unknown } = groupPapertrailRefs(refs);
      if (open.length) {
        out.push(
          new Item(`open (${open.length})`, {
            icon: new vscode.ThemeIcon('issues'),
            children: open.map((r) => refItem(r)),
          }),
        );
      }
      if (closed.length) {
        out.push(
          new Item(`closed/merged (${closed.length})`, {
            icon: new vscode.ThemeIcon('issue-closed'),
            children: closed.map((r) => refItem(r)),
          }),
        );
      }
      if (unknown.length) {
        out.push(
          new Item(`unknown state (${unknown.length})`, {
            icon: new vscode.ThemeIcon('question'),
            children: unknown.map((r) => refItem(r)),
          }),
        );
      }
    }
    if (!out.length) {
      return [new Item('no tracker items reference this file', { icon: new vscode.ThemeIcon('check') })];
    }
    return out;
  }
}

export function groupPapertrailRefs(refs: PapertrailRef[]): {
  open: PapertrailRef[];
  closed: PapertrailRef[];
  unknown: PapertrailRef[];
} {
  return {
    open: refs.filter((ref) => ref.state_normalized === 'open'),
    closed: refs.filter(
      (ref) => ref.state_normalized === 'closed' || ref.state_normalized === 'merged',
    ),
    unknown: refs.filter(
      (ref) => !['open', 'closed', 'merged'].includes(ref.state_normalized ?? ''),
    ),
  };
}

function refItem(r: PapertrailRef): Item {
  const uri = externalUri(r.url);
  return new Item(`#${r.item_key} ${r.title ?? ''}`, {
    description: `${r.item_kind} · ${r.ref_kind}`,
    tooltip: r.source_text.slice(0, 200),
    icon: new vscode.ThemeIcon('link-external'),
    command: uri ? { title: 'open', command: 'vscode.open', arguments: [uri] } : undefined,
  });
}

function externalUri(raw: string | null): vscode.Uri | undefined {
  if (!raw) {
    return undefined;
  }
  const uri = vscode.Uri.parse(raw);
  return uri.scheme === 'https' || uri.scheme === 'http' ? uri : undefined;
}

export function registerSidebar(
  context: vscode.ExtensionContext,
  store: FileStore,
): { refresh: () => void } {
  const clones = new CloneClassesView();
  const memories = new MemoriesView();
  const papertrail = new PapertrailView();
  const pump = new SidebarPump(store, [clones, memories, papertrail]);
  context.subscriptions.push(
    vscode.window.registerTreeDataProvider('rag-rat-lens.cloneClasses', clones),
    vscode.window.registerTreeDataProvider('rag-rat-lens.memories', memories),
    vscode.window.registerTreeDataProvider('rag-rat-lens.papertrail', papertrail),
    // A switch is a reload, driven through the same pump as the index-version poll so the two
    // cannot publish over each other.
    vscode.window.onDidChangeActiveTextEditor(() => pump.refresh()),
  );
  return { refresh: () => pump.refresh() };
}
