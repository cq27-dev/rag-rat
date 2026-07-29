// Symbol hovers: graph trust data at a glance — caller decomposition,
// load-bearing bucket, dispatch variants — without opening a quick pick.
import * as vscode from 'vscode';
import type { FileStore } from './store';

export class GraphHoverProvider implements vscode.HoverProvider {
  constructor(private readonly store: FileStore) {}

  async provideHover(
    document: vscode.TextDocument,
    position: vscode.Position,
  ): Promise<vscode.Hover | undefined> {
    const loaded = await this.store.dataFor(document);
    if (!loaded) {
      return undefined;
    }
    const d = loaded.data;
    const line = position.line + 1;
    const s = d.symbols.find((x) => x.start_line === line);
    if (!s) {
      return undefined;
    }
    const total = s.callers.exact + s.callers.syntactic + s.callers.name_only + s.callers.ambiguous;
    const md = new vscode.MarkdownString('', true);
    md.isTrusted = { enabledCommands: ['rag-rat-lens.showCallers'] };
    const parts = [
      `**${escapeMarkdown(s.kind)}** \`${escapeMarkdown(s.name)}\``,
      s.fan_in_bucket !== 'low' ? `**⚑ ${s.fan_in_bucket} load** (fan-in ${s.fan_in_score})` : null,
      total > 0
        ? `⤴ ${total} callers — ${[
            s.callers.exact ? `${s.callers.exact} exact` : '',
            s.callers.syntactic ? `${s.callers.syntactic} syntactic` : '',
            s.callers.name_only ? `${s.callers.name_only} name-only` : '',
            s.callers.ambiguous ? `${s.callers.ambiguous} ambiguous` : '',
            s.callers.tests ? `${s.callers.tests} test` : '',
            s.callers.dispatch ? `${s.callers.dispatch} dispatch` : '',
          ]
            .filter(Boolean)
            .join(' · ')}`
        : '_no static callers (never proof of dead code)_',
    ].filter(Boolean);
    md.appendMarkdown(parts.join('  \n'));
    if (s.qname) {
      const args = encodeURIComponent(JSON.stringify([s.qname, s.name]));
      md.appendMarkdown(`  \n[show callers](command:rag-rat-lens.showCallers?${args})`);
    }
    if (s.dispatch.length) {
      const variants = [
        ...new Set(s.dispatch.map((x) => x.variant).filter((value): value is string => Boolean(value))),
      ];
      md.appendMarkdown(`  \n⇄ dispatch: ${variants.map(escapeMarkdown).join(', ')}`);
    }
    return new vscode.Hover(md, new vscode.Range(position.line, 0, position.line, 0));
  }
}

function escapeMarkdown(value: string): string {
  return value.replace(/[\\`*_{}[\]()<>#+\-.!|]/g, '\\$&');
}
