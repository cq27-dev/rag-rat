// Clickable memory affordance: a CodeLens above each line that carries repo
// memories ("◆ N bound memories") -> QuickPick -> full memory in a read-only doc.
import * as vscode from 'vscode';
import type { FileMemory } from './client';
import type { FileStore } from './store';
import { verdictDirectionLabel } from './verdict';

export const MEMORY_DOC_SCHEME = 'rag-rat-memory';

export class MemoryCodeLensProvider implements vscode.CodeLensProvider, vscode.Disposable {
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this.changed.event;

  constructor(private readonly store: FileStore) {}

  refresh(): void {
    this.changed.fire();
  }

  dispose(): void {
    this.changed.dispose();
  }

  async provideCodeLenses(document: vscode.TextDocument): Promise<vscode.CodeLens[]> {
    const loaded = await this.store.dataFor(document);
    const memories = loaded?.data.memories;
    if (!loaded || !memories) {
      return [];
    }
    const path = loaded.path;
    const byLine = new Map<number, FileMemory[]>();
    for (const m of memories) {
      const line = Math.min(Math.max(0, (m.line ?? 1) - 1), document.lineCount - 1);
      const list = byLine.get(line) ?? [];
      list.push(m);
      byLine.set(line, list);
    }
    const lenses: vscode.CodeLens[] = [];
    for (const [line, list] of byLine) {
      lenses.push(
        new vscode.CodeLens(new vscode.Range(line, 0, line, 0), {
          title: `◆ ${list.length} bound ${list.length === 1 ? 'memory' : 'memories'}`,
          command: 'rag-rat-lens.showMemories',
          arguments: [list, path],
        }),
      );
    }
    return lenses;
  }
}

export async function showMemoriesQuickPick(
  memories: FileMemory[],
  path: string,
  documents: MemoryDocProvider,
): Promise<void> {
  const picked = await vscode.window.showQuickPick(
    memories.map((m) => ({
      label: `$(bookmark) ${m.kind}: ${m.title}`,
      description: `${m.confidence} · ${m.anchor_status} · ${m.binding_kind}`,
      memory: m,
    })),
    { placeHolder: 'repo memories on this line' },
  );
  if (!picked) {
    return;
  }
  // Re-read by id rather than rendering the captured object. These items were built when the
  // CodeLens was, and the index can move while the picker sits open — the refresh that would have
  // corrected the document has then already run, so a stale render survives until the NEXT index
  // change. Worse, a memory deleted in the meantime would be put back on screen as though it still
  // existed.
  const outcome = await documents.openPicked(picked.memory.id, path);
  if (outcome.kind !== 'ready') {
    void vscode.window.showInformationMessage(
      outcome.kind === 'absent'
        ? 'That memory could not be opened — it may have been removed, or the lookup for this file failed.'
        : 'The Lens server is not answering, so that memory cannot be opened.',
    );
    return;
  }
  const doc = await vscode.workspace.openTextDocument(outcome.uri);
  await vscode.window.showTextDocument(doc, vscode.ViewColumn.Beside, true);
}

function renderMemoryDoc(m: FileMemory): string {
  const head = [
    `# ${m.title}`,
    '',
    `**kind** ${m.kind}  ·  **confidence** ${m.confidence}  ·  **anchor** ${m.anchor_status}  ·  **binding** ${m.binding_kind}`,
    '',
  ];
  const verdict = verdictSection(m);
  if (m.summary) {
    return [...head, '## Excerpt', '', m.summary, '', '## Verbatim', '', m.body, '', ...verdict].join('\n');
  }
  return [...head, m.body, '', ...verdict].join('\n');
}

function verdictSection(m: FileMemory): string[] {
  if (!m.verdict) {
    return [];
  }
  const direction = m.verdict === 'diverged' || m.verdict_direction
    ? ` (${verdictDirectionLabel(m.verdict_direction)})`
    : '';
  const lines = [
    '## Verdict',
    '',
    `**${m.verdict}**${direction}${m.checked_against_commit ? ` · checked against \`${m.checked_against_commit.slice(0, 8)}\`` : ''}`,
    '',
  ];
  if (m.verdict_evidence) {
    try {
      const checks = JSON.parse(m.verdict_evidence) as string[];
      lines.push(...checks.map((c) => `- ${c}`), '');
    } catch {
      /* evidence_json is informational */
    }
  }
  return lines;
}

/** What opening a picked memory produced — see `MemoryDocProvider.openPicked`. */
export type MemoryPick =
  | { kind: 'ready'; uri: vscode.Uri }
  /** Not in the payload we got back. Deliberately NOT called "deleted" — see `openPicked`. */
  | { kind: 'absent' }
  | { kind: 'unavailable' };

/** Shown in place of a memory document while the server that produced it is unreachable. */
const UNAVAILABLE_DOC = [
  '# Memory unavailable',
  '',
  'The Lens server is not answering, so this memory cannot be shown.',
  'It will reappear when the server is reachable again.',
].join('\n');

export class MemoryDocProvider implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly contents = new Map<string, string>();
  private readonly sourcePaths = new Map<string, string>();
  private readonly changed = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.changed.event;
  /** Bumped by `withdraw()` so a refresh started before it cannot publish after it. */
  private epoch = 0;
  /**
   * Whether the server behind these documents is currently unreachable. Withdrawal has to be a
   * STATE, not a one-off rewrite: a memory can be re-rendered from a `FileMemory` captured before
   * the outage — an open QuickPick, a CodeLens or tree command still clickable — which would put
   * the unreachable server's body back on screen without anyone contacting it.
   */
  private withdrawn = false;

  constructor(private readonly store: FileStore) {}

  /**
   * Open the memory `id` as it stands NOW, re-read from `path` after a picker resolved.
   *
   * `absent` does NOT mean deleted, and the caller must not say that it does. `FileStore.data`
   * merges five independently-settling lanes: when the memory lane alone fails it substitutes the
   * previous value for up to a minute, and an empty list once that expires. So a memory missing
   * here may have been removed from the index, or its lookup may simply have failed — and one
   * present here may be up to a minute old. Only `undefined` from the store is authoritative, and
   * only about the server as a whole. Telling a lane's answer from its fallback needs provenance
   * the store does not yet expose — #1026.
   *
   * What this DOES guarantee is that nothing is rendered from the object the picker captured, so a
   * memory the index no longer holds cannot be put back on screen as though it did.
   */
  async openPicked(id: string, path: string): Promise<MemoryPick> {
    const data = await this.store.data(path);
    if (!data) {
      return { kind: 'unavailable' };
    }
    const memory = data.memories.find((candidate) => candidate.id === id);
    return memory ? { kind: 'ready', uri: this.update(memory, path) } : { kind: 'absent' };
  }

  update(memory: FileMemory, path: string): vscode.Uri {
    const uri = memoryUri(memory.id);
    // Track the source path either way, so a reconnect knows which file to restore this from.
    this.sourcePaths.set(memory.id, path);
    this.contents.set(memory.id, this.withdrawn ? UNAVAILABLE_DOC : renderMemoryDoc(memory));
    this.changed.fire(uri);
    return uri;
  }

  async refresh(): Promise<void> {
    // A refresh in flight when the server drops out would otherwise restore the very content
    // `withdraw()` just replaced. Same discipline as everywhere else here: an async result may
    // only publish if the state it was computed under still holds.
    const epoch = this.epoch;
    const byPath = new Map<string, string[]>();
    for (const [id, path] of this.sourcePaths) {
      const ids = byPath.get(path) ?? [];
      ids.push(id);
      byPath.set(path, ids);
    }
    await Promise.all([...byPath].map(async ([path, ids]) => {
      const data = await this.store.data(path);
      if (!data || this.epoch !== epoch) {
        return;
      }
      const current = new Map(data.memories.map((memory) => [memory.id, memory]));
      for (const id of ids) {
        const memory = current.get(id);
        if (memory) {
          this.update(memory, path);
        } else {
          this.contents.delete(id);
          this.changed.fire(memoryUri(id));
        }
      }
    }));
  }

  /**
   * Withdraw every open memory document's content, keeping its source path so a reconnect can
   * restore it.
   *
   * An open `rag-rat-memory:` document is a rendered claim from one server about one memory —
   * its body, its verdict, the commit it was checked against. When that server stops answering,
   * or the extension is pointed at a different one, the claim is no longer backed by anything,
   * and `refresh()` cannot correct it: with no data to compare against it leaves the previous
   * contents in place. Left alone the document keeps presenting the old server's memory
   * indefinitely, which is the one surface where staleness is invisible — there is no line
   * number to look wrong against.
   */
  /** The server is answering again: render real content once more. Contents follow on refresh. */
  restore(): void {
    this.withdrawn = false;
  }

  withdraw(): void {
    this.epoch += 1;
    this.withdrawn = true;
    for (const id of [...this.contents.keys()]) {
      // REPLACED, not deleted: an absent entry already means "this memory is gone from the index",
      // which `refresh()` produces when a memory really was removed. An outage is a different
      // statement and must not be reported as a deletion.
      this.contents.set(id, UNAVAILABLE_DOC);
      this.changed.fire(memoryUri(id));
    }
  }

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.contents.get(memoryId(uri)) ?? 'memory not found';
  }

  forget(uri: vscode.Uri): void {
    const id = memoryId(uri);
    this.contents.delete(id);
    this.sourcePaths.delete(id);
  }

  dispose(): void {
    this.changed.dispose();
  }
}

function memoryUri(id: string): vscode.Uri {
  return vscode.Uri.parse(`${MEMORY_DOC_SCHEME}:/${encodeURIComponent(id)}.md`);
}

function memoryId(uri: vscode.Uri): string {
  return decodeURIComponent(uri.path.replace(/^\//, '').replace(/\.md$/, ''));
}
