// Sidebar tree views: per-file Clone Classes, Memories, and Issues & Decisions.
// All three refresh off the active editor and the index-version poll, and all
// read from the shared per-file FileStore (no per-view fetching).
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
 * How many times a tree load may be re-aimed at a newly active editor before giving up. Each
 * attempt awaits a real load, so the bound exists to end an unwinnable race with a user paging
 * through files, not to prevent a hot loop.
 */
const STALE_LOAD_RETRIES = 4;

abstract class FileView implements vscode.TreeDataProvider<Item> {
  private emitter = new vscode.EventEmitter<Item | undefined>();
  readonly onDidChangeTreeData = this.emitter.event;

  constructor(protected readonly store: FileStore) {}

  refresh(): void {
    this.emitter.fire(undefined);
  }

  getTreeItem(element: Item): vscode.TreeItem {
    return element;
  }

  async getChildren(element?: Item): Promise<Item[]> {
    if (element) {
      return element.children ?? [];
    }
    // These views describe the ACTIVE file, so an answer about a document that stopped being
    // active is wrong however good the data is. Rather than discard it — which would let a slow
    // load, resolving after a newer one, blank the tree the newer one just filled — re-ask about
    // wherever the user now is. Whichever request settles last then still describes the current
    // editor, so the outcome no longer depends on completion order.
    for (let attempt = 0; attempt < STALE_LOAD_RETRIES; attempt += 1) {
      const editor = vscode.window.activeTextEditor;
      if (!editor) {
        return [
          new Item('open a file in the indexed repo', { icon: new vscode.ThemeIcon('info') }),
        ];
      }
      const document = editor.document.uri.toString();
      const loaded = await this.store.dataFor(editor.document);
      if (vscode.window.activeTextEditor?.document.uri.toString() !== document) {
        continue;
      }
      if (!loaded) {
        // Distinguish "nothing to show for this buffer" from "the server did not answer": an
        // unsaved buffer has no indexed path, and reporting that as an outage would be wrong.
        return this.store.pathOf(editor.document)
          ? [new Item('lens server offline', { icon: new vscode.ThemeIcon('cloud-offline') })]
          : [new Item('open a file in the indexed repo', { icon: new vscode.ThemeIcon('info') })];
      }
      return this.roots(loaded.path, loaded.data);
    }
    // The active editor changed on every attempt, so the user is still moving and every switch
    // has queued its own refresh. Returning nothing can still blank a tree a newer request just
    // filled, because `getChildren` has no way to decline to publish — that residual is #1022,
    // which replaces this shape with a synchronous read over state a single pump owns.
    return [];
  }

  protected abstract roots(path: string, data: FileData): Item[];
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
  const clones = new CloneClassesView(store);
  const memories = new MemoriesView(store);
  const papertrail = new PapertrailView(store);
  context.subscriptions.push(
    vscode.window.registerTreeDataProvider('rag-rat-lens.cloneClasses', clones),
    vscode.window.registerTreeDataProvider('rag-rat-lens.memories', memories),
    vscode.window.registerTreeDataProvider('rag-rat-lens.papertrail', papertrail),
    vscode.window.onDidChangeActiveTextEditor(() => {
      clones.refresh();
      memories.refresh();
      papertrail.refresh();
    }),
  );
  return {
    refresh: () => {
      clones.refresh();
      memories.refresh();
      papertrail.refresh();
    },
  };
}
