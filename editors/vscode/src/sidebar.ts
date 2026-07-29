// Sidebar tree views: per-file Clone Classes, Memories, and Issues & Decisions.
// All three describe the active editor's file, are filled by one pump off the
// active-editor event and the index-version poll, and read from the shared
// per-file FileStore (no per-view fetching).
import * as vscode from 'vscode';
import type { CloneRegion, DecisionRecord, PapertrailRef } from './client';
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
  private items: Item[] = [];

  getTreeItem(element: Item): vscode.TreeItem {
    return element;
  }

  getChildren(element?: Item): Item[] {
    return element ? element.children ?? [] : this.items;
  }

  /** Replace what this view shows and make VS Code re-read it. The pump is the only caller. */
  publish(items: Item[]): void {
    this.items = items;
    this.emitter.fire(undefined);
  }

  /**
   * This view's tree for one file's data. The contract is unchanged by the pump — only its caller
   * moved out of `getChildren`, so it now runs once a load is entitled to publish.
   */
  abstract roots(path: string, data: FileData): Item[];
}

const loading = (): Item => new Item('loading…', { icon: new vscode.ThemeIcon('loading~spin') });

const noIndexedFile = (): Item =>
  new Item('open a file in the indexed repo', { icon: new vscode.ThemeIcon('info') });

const serverOffline = (): Item =>
  new Item('lens server offline', { icon: new vscode.ThemeIcon('cloud-offline') });

/**
 * Loads the sidebar's data and publishes it to the three views.
 *
 * Everything the views show describes ONE document — whichever editor is active — so which
 * document a load was aimed at is part of its answer, and an answer about another document is
 * wrong however good the data is. This is where that is checked: the document and the store's data
 * epoch are captured before the await and re-read after it, and the result is written only if this
 * is still the newest load, still aimed at the active editor, and still under the index state it
 * started from. Anything else is dropped rather than published — whatever superseded it publishes
 * its own answer, which is why every `invalidate`/`reset` in the extension is paired with a
 * `refresh`.
 *
 * One pump for all three views rather than one each: they sit stacked in a single container, so a
 * slow view left describing the previous file beside two describing the current one is the same
 * wrong answer, only visibly. It also puts the decision to publish in one place instead of three.
 */
class SidebarPump {
  private generation = 0;
  /** The document the views describe, `undefined` when no editor is active. */
  private document: string | undefined;

  constructor(
    private readonly store: FileStore,
    private readonly views: readonly FileView[],
  ) {}

  /** Reload for whatever editor is active now. */
  refresh(): void {
    void this.load();
  }

  private async load(): Promise<void> {
    const generation = ++this.generation;
    const editor = vscode.window.activeTextEditor;
    const document = editor?.document.uri.toString();
    const switched = document !== this.document;
    this.document = document;
    if (!editor) {
      this.publishMessage(noIndexedFile);
      return;
    }
    if (switched) {
      // The views still hold the previous file's tree, and leaving it up asserts it about the file
      // now on screen. Only a document CHANGE clears: an index refresh reloads the same document,
      // and emptying its tree on every version event would flicker for no gain.
      this.publishMessage(loading);
    }
    const epoch = this.store.dataEpoch();
    const loaded = await this.store.dataFor(editor.document);
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
    if (!loaded) {
      // Distinguish "nothing to show for this buffer" from "the server did not answer": an
      // unsaved buffer has no indexed path, and reporting that as an outage would be wrong.
      this.publishMessage(this.store.pathOf(editor.document) ? serverOffline : noIndexedFile);
      return;
    }
    for (const view of this.views) {
      view.publish(view.roots(loaded.path, loaded.data));
    }
  }

  /** The same message in all three views, built per view: a `TreeItem` belongs to one tree. */
  private publishMessage(item: () => Item): void {
    for (const view of this.views) {
      view.publish([item()]);
    }
  }
}

export class CloneClassesView extends FileView {
  roots(path: string, data: FileData): Item[] {
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
  roots(path: string, data: FileData): Item[] {
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
  roots(_path: string, data: FileData): Item[] {
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
