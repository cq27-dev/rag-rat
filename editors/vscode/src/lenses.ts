// High-signal CodeLenses over the Lens graph/coupling/papertrail endpoints:
// trust-decomposed callers, load-bearing bucket, actor dispatch, co-change
// coupling, issues/PRs per file, distill decision records, extract-helper
// preview for refined clone classes.
import * as vscode from 'vscode';
import { lensHttpStatus } from './client';
import type {
  CloneRegion,
  CouplingPartner,
  DecisionRecord,
  LensClient,
  PapertrailRef,
  SymbolCallers,
  SymbolGraph,
  SymbolSelector,
} from './client';
import type { FileStore } from './store';

const DOC_SCHEME = 'rag-rat-doc';
const DOC_CACHE_LIMIT = 200;

/** Shown in place of a document whose server is gone — see `LensDocProvider.withdraw`. */
const WITHDRAWN_DOC = [
  '# Unavailable',
  '',
  'The Lens server that produced this is no longer being served.',
  'Re-run the command once the server is reachable again.',
].join('\n');

/**
 * Backing store for the read-only `rag-rat-doc:` documents — decision records and extraction
 * proposals opened from a CodeLens.
 *
 * The contents belong to the PROVIDER rather than the module, so their lifetime is the registered
 * provider's and a test can exercise one without reaching into shared state.
 */
export class LensDocProvider implements vscode.TextDocumentContentProvider {
  private readonly contents = new Map<string, string>();
  private readonly changed = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.changed.event;
  private sequence = 0;

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.contents.get(uri.path) ?? 'not found';
  }

  /** Render `markdown` as a new document and show it beside the editor. */
  open(title: string, markdown: string): Thenable<void> {
    const key = `/${this.sequence++}-${encodeURIComponent(title)}.md`;
    this.contents.set(key, markdown);
    // Evict the oldest entry (Map preserves insertion order) so a long session's quick-pick docs
    // cannot grow the map without bound.
    if (this.contents.size > DOC_CACHE_LIMIT) {
      const oldest = this.contents.keys().next().value;
      if (oldest) {
        this.contents.delete(oldest);
      }
    }
    return vscode.workspace
      .openTextDocument(vscode.Uri.parse(`${DOC_SCHEME}:${key}`))
      .then((doc) => vscode.window.showTextDocument(doc, vscode.ViewColumn.Beside, true))
      .then(() => undefined);
  }

  /**
   * Withdraw every open `rag-rat-doc:` document.
   *
   * These render a decision record or an extraction proposal from ONE server and one checkout, and
   * unlike every other surface they are never refetched — the command that produced them has
   * already returned. An endpoint change or an outage would otherwise leave them presenting the
   * previous server's answer indefinitely, after everything around them has been cleared. Replaced
   * rather than deleted, because an absent key already means "no such document".
   */
  withdraw(): void {
    for (const key of [...this.contents.keys()]) {
      this.contents.set(key, WITHDRAWN_DOC);
      this.changed.fire(vscode.Uri.parse(`${DOC_SCHEME}:${key}`));
    }
  }
}

/** The registered provider, so the extension can withdraw its documents on an endpoint change. */
let lensDocs: LensDocProvider | undefined;

export function withdrawLensDocuments(): void {
  lensDocs?.withdraw();
}

function openDoc(title: string, markdown: string): Thenable<void> {
  return lensDocs ? lensDocs.open(title, markdown) : Promise.resolve();
}

/**
 * Arguments for `rag-rat-lens.showCallers`, built in ONE place for every surface that dispatches
 * it.
 *
 * The CodeLens and the hover link render from the same `SymbolGraph` row and reach the same
 * handler, so the tuple is a shared contract rather than each surface's own detail: a surface that
 * assembles it locally can pass a shape the handler no longer reads, and a surface that sends the
 * row's name instead of its handle answers for every overload sharing that name rather than for
 * the row the reader is looking at. `undefined` = the row names no symbol the server can resolve,
 * so the surface must offer no link at all.
 *
 * Both fields are normalized to `null` because the hover's half of this contract is a JSON
 * `command:` URI, which drops an `undefined` field entirely — a server that omits `id` would
 * otherwise have the two surfaces build tuples that differ in shape while behaving alike, which is
 * exactly the drift an untyped boundary hides.
 */
export function callerCommandArguments(
  symbol: SymbolGraph,
): [SymbolSelector, string] | undefined {
  return symbol.id || symbol.qname
    ? [{ id: symbol.id ?? null, qname: symbol.qname ?? null }, symbol.name]
    : undefined;
}

/**
 * What the caller quick pick says it is showing.
 *
 * `matched_symbols` is the whole answer, and it is read the same way on BOTH lanes: `> 1` means
 * the rows are a union over that many symbols, so `N callers of foo` would state a narrower claim
 * than the server made. Which selector produced the union does not change what a reader is
 * looking at, and both selectors can produce one — a qualified name covers every overload in the
 * file, and a handle covers every overload the grouping key could not tell apart, which is every
 * overload declared on an identical line (`fn new() -> Self {` on two impls). Reading the count
 * only on the fallback lane discards the one signal that is present and correct on the other.
 *
 * `0` is reachable only from the fallback lane — a handle with no symbol behind it is a 404, not
 * an empty answer — and means the name named nothing indexed, so the rows came from unresolved
 * call sites alone. A server that reports no count says nothing, so neither do we.
 */
export function callersPlaceholder(
  name: string,
  rowCount: number,
  answer: Pick<SymbolCallers, 'matched_symbols'>,
): string {
  const matched = answer.matched_symbols;
  if (matched === undefined || matched === 1) {
    return `${rowCount} callers of ${name}`;
  }
  if (matched === 0) {
    return `${rowCount} callers by name — nothing indexed is named ${name}`;
  }
  return `${rowCount} callers of ${matched} symbols named ${name}`;
}

export class SignalLensProvider implements vscode.CodeLensProvider, vscode.Disposable {
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
    if (!loaded) {
      return [];
    }
    const d = loaded.data;
    const lenses: vscode.CodeLens[] = [];
    for (const s of d.symbols) {
      const line = Math.min(Math.max(0, s.start_line - 1), document.lineCount - 1);
      const range = new vscode.Range(line, 0, line, 0);
      const total = s.callers.exact + s.callers.syntactic + s.callers.name_only + s.callers.ambiguous;
      const callers = callerCommandArguments(s);
      if (total > 0 && callers) {
        const tiers = [
          s.callers.exact ? `${s.callers.exact} exact` : '',
          s.callers.syntactic ? `${s.callers.syntactic} syntactic` : '',
          s.callers.name_only ? `${s.callers.name_only} name-only` : '',
          s.callers.ambiguous ? `${s.callers.ambiguous} ambiguous` : '',
          s.callers.tests ? `${s.callers.tests} test` : '',
          s.callers.dispatch ? `${s.callers.dispatch} dispatch` : '',
        ]
          .filter(Boolean)
          .join(' · ');
        lenses.push(
          new vscode.CodeLens(range, {
            title:
              `⤴ ${total} (${tiers})` +
              (s.fan_in_bucket === 'critical' || s.fan_in_bucket === 'high'
                ? `  ⚑${s.fan_in_bucket} load`
                : ''),
            command: 'rag-rat-lens.showCallers',
            // The lens is built from ONE symbol row, so it carries that row's handle: two
            // overloads always share a qualified name, and share a handle only when they declare
            // on an identical line. The count in this title is counted per symbol. Passing the
            // name alone is what made every lens on a name answer alike.
            arguments: callers,
          }),
        );
      } else if (s.fan_in_bucket === 'critical' || s.fan_in_bucket === 'high') {
        lenses.push(new vscode.CodeLens(range, { title: `⚑${s.fan_in_bucket} load`, command: '' }));
      }
      if (s.dispatch.length > 0) {
        const variants = [...new Set(s.dispatch.map((x) => x.variant).filter(Boolean))];
        lenses.push(
          new vscode.CodeLens(range, {
            title: `⇄ ${variants.slice(0, 2).join(', ')}${variants.length > 2 ? ` +${variants.length - 2}` : ''}`,
            command: 'rag-rat-lens.showDispatch',
            arguments: [s.dispatch, s.name],
          }),
        );
      }
    }
    if (d.coupling.length > 0) {
      const top = d.coupling[0];
      lenses.push(
        new vscode.CodeLens(new vscode.Range(0, 0, 0, 0), {
          title: `⇄ usually changes with ${basename(top.path)} (${Math.round(top.confidence * 100)}%)${d.coupling.length > 1 ? ` +${d.coupling.length - 1}` : ''}`,
          command: 'rag-rat-lens.showCoupling',
          arguments: [d.coupling],
        }),
      );
    }
    if (d.refs.length > 0) {
      const open = d.refs.filter((r) => r.state_normalized === 'open').length;
      lenses.push(
        new vscode.CodeLens(new vscode.Range(0, 0, 0, 0), {
          title: `# ${d.refs.length} issue/PR refs${open ? ` (${open} open)` : ''}`,
          command: 'rag-rat-lens.showRefs',
          arguments: [d.refs],
        }),
      );
    }
    for (const dec of d.decisions) {
      const line = Math.min(Math.max(0, (dec.line ?? 1) - 1), document.lineCount - 1);
      const verified = dec.outcome_claim_verified && dec.decision_provenance_verified;
      lenses.push(
        new vscode.CodeLens(new vscode.Range(line, 0, line, 0), {
          title: `§ decision: #${dec.item_key} ${(dec.title ?? '').slice(0, 60)}${verified ? ' ✓' : ''}`,
          command: 'rag-rat-lens.showDecision',
          arguments: [dec],
        }),
      );
    }
    for (const region of d.clones) {
      const refine = region.refine;
      if (!refine || region.start_line == null) {
        continue;
      }
      const line = Math.min(Math.max(0, region.start_line - 1), document.lineCount - 1);
      const params = proposedSignature(refine.proposed_signature).params.length;
      lenses.push(
        new vscode.CodeLens(new vscode.Range(line, 0, line, 0), {
          title: `⚒ extract helper (${params} params · refactorability ${refine.refactorability.toFixed(2)})`,
          command: 'rag-rat-lens.showExtract',
          arguments: [region],
        }),
      );
    }
    return lenses;
  }
}

function basename(p: string): string {
  return p.split('/').pop() ?? p;
}

interface CallerRow {
  name: string;
  qname: string | null;
  path: string;
  source_start_line: number;
  edge_kind: string;
  confidence: string;
}

export function registerSignalCommands(
  context: vscode.ExtensionContext,
  client: LensClient,
): void {
  const openLocation = async (path: string | null, line: number | null) => {
    if (!path) {
      return;
    }
    await vscode.commands.executeCommand('rag-rat-lens.openLocation', { path, line: line ?? 1 });
  };

  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(DOC_SCHEME, (lensDocs = new LensDocProvider())),

    vscode.commands.registerCommand('rag-rat-lens.showCallers', async (selector: SymbolSelector, name: string) => {
      let answer: SymbolCallers;
      try {
        answer = await client.symbolCallers(selector);
      } catch (error) {
        // A handle the server no longer knows is a stale gutter decoration, not something the
        // reader did wrong: the declaration was edited and reindexed under a new handle while
        // this lens kept the old one. `symbolCallers` already retried by name for every row that
        // carried one, so reaching here means this row had only the handle. Say that, rather
        // than raising a "contributed command failed" notification over it. Any other failure is
        // a real one and keeps surfacing as one.
        if (lensHttpStatus(error) !== 404) {
          throw error;
        }
        await vscode.window.showWarningMessage(
          `${name} is no longer in the index under the handle this lens carries — reopen the file to refresh it.`,
        );
        return;
      }
      const rows = answer.callers as CallerRow[];
      const picked = await vscode.window.showQuickPick(
        rows.map((r) => ({
          label: `$(symbol-method) ${r.name}`,
          description: `${r.confidence} · ${r.edge_kind}`,
          detail: `${r.path}:${r.source_start_line}`,
          row: r,
        })),
        { placeHolder: callersPlaceholder(name, rows.length, answer) },
      );
      if (picked) {
        await openLocation(picked.row.path, picked.row.source_start_line);
      }
    }),

    vscode.commands.registerCommand(
      'rag-rat-lens.showDispatch',
      async (items: SymbolGraph['dispatch'], name: string) => {
        const picked = await vscode.window.showQuickPick(
          items.map((d) => ({
            label: `$(radio-tower) ${d.variant ?? '?'} — ${d.direction}`,
            description: d.other_name ?? '',
            detail: `${d.other_path}:${d.other_line ?? '?'}`,
            item: d,
          })),
          { placeHolder: `dispatch edges of ${name}` },
        );
        if (picked) {
          await openLocation(picked.item.other_path, picked.item.other_line);
        }
      },
    ),

    vscode.commands.registerCommand('rag-rat-lens.showCoupling', async (partners: CouplingPartner[]) => {
      const picked = await vscode.window.showQuickPick(
        partners.map((p) => ({
          label: `$(git-compare) ${p.path}`,
          description: `${Math.round(p.confidence * 100)}% · ${p.co_changes}/${p.my_changes} co-changes`,
          partner: p,
        })),
        { placeHolder: 'files that usually change together (lift-curated)' },
      );
      if (picked) {
        await openLocation(picked.partner.path, 1);
      }
    }),

    vscode.commands.registerCommand('rag-rat-lens.showRefs', async (refs: PapertrailRef[]) => {
      const picked = await vscode.window.showQuickPick(
        refs.map((r) => ({
          label: `$(issues) #${r.item_key} ${r.title ?? ''}`,
          description: `${r.item_kind} · ${r.state_normalized ?? '?'} · ${r.ref_kind}`,
          detail: r.source_text.slice(0, 120),
          ref: r,
        })),
        { placeHolder: 'tracker items referencing this file' },
      );
      const uri = externalUri(picked?.ref.url ?? null);
      if (uri) {
        await vscode.env.openExternal(uri);
      }
    }),

    vscode.commands.registerCommand('rag-rat-lens.showDecision', (d: DecisionRecord) => {
      const verified = d.outcome_claim_verified && d.decision_provenance_verified;
      return openDoc(
        `decision-${d.item_key}`,
        [
          `# Decision record: #${d.item_key} ${d.title ?? ''}`,
          '',
          `**status** ${d.outcome_status_model ?? '?'} · **verified facets** ${verified ? 'both' : 'partial/none'} · **fix edge** ${d.fix_edge_source} · ${d.url ?? ''}`,
          '',
          '## Root issue',
          '',
          d.root_issue ?? '_none recorded_',
          '',
          '## Root cause',
          '',
          d.root_cause ?? '_none recorded_',
          '',
          '## Decision',
          '',
          d.decision_chosen ?? '_none recorded_',
          '',
          '## Outcome',
          '',
          d.outcome_summary ?? '_none recorded_',
          '',
        ].join('\n'),
      );
    }),

    vscode.commands.registerCommand('rag-rat-lens.showExtract', (region: CloneRegion) => {
      const r = region.refine;
      if (!r) {
        return undefined;
      }
      const signature = proposedSignature(r.proposed_signature);
      const params = signature.params.map(
        (p, i) => `- \`${p.name ?? `arg${i}`}\`: \`${p.type_text ?? '?'}\` _(${p.confidence ?? '?'})_`,
      );
      const vars = variationPoints(r.variation_points).map(
        (v) =>
          `- \`${v.metavar_id ?? '?'}\` ${v.extraction_role ?? ''} → ${(v.per_member_values ?? [])
            .map((x) => `\`${x}\``)
            .join(', ')}`,
      );
      return openDoc(
        `extract-${region.symbol ?? 'helper'}`,
        [
          `# Extract helper preview — ${region.symbol ?? ''}`,
          '',
          `**refactorability** ${r.refactorability.toFixed(2)} · **coverage** ${r.anti_unify_coverage.toFixed(2)} · **lcs** ${r.lcs_ratio.toFixed(2)} · **confidence** ${r.confidence}`,
          '',
          '## Proposed signature',
          '',
          '```rust',
          signature.text ?? 'fn extracted(…)',
          '```',
          '',
          ...(params.length ? ['## Parameters', '', ...params, ''] : []),
          '## Template (⟨mN⟩ = variation points)',
          '',
          '```rust',
          r.template,
          '```',
          '',
          ...(vars.length ? ['## Variation points', '', ...vars, ''] : []),
          '_Name is a placeholder; closure/type params and low-confidence metavars need human review._',
        ].join('\n'),
      );
    }),
  );
}

interface ProposedSignature {
  text?: string;
  params: { name?: string; type_text?: string; confidence?: string }[];
}

interface VariationPoint {
  metavar_id?: string;
  extraction_role?: string;
  per_member_values?: string[];
}

function proposedSignature(value: unknown): ProposedSignature {
  if (!isRecord(value)) {
    return { params: [] };
  }
  const params = Array.isArray(value.params)
    ? value.params.filter(isRecord).map((param) => ({
        name: typeof param.name === 'string' ? param.name : undefined,
        type_text: typeof param.type_text === 'string' ? param.type_text : undefined,
        confidence: typeof param.confidence === 'string' ? param.confidence : undefined,
      }))
    : [];
  return { text: typeof value.text === 'string' ? value.text : undefined, params };
}

function variationPoints(value: unknown): VariationPoint[] {
  return Array.isArray(value)
    ? value.filter(isRecord).map((point) => ({
        metavar_id: typeof point.metavar_id === 'string' ? point.metavar_id : undefined,
        extraction_role:
          typeof point.extraction_role === 'string' ? point.extraction_role : undefined,
        per_member_values: Array.isArray(point.per_member_values)
          ? point.per_member_values.filter((item): item is string => typeof item === 'string')
          : undefined,
      }))
    : [];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function externalUri(raw: string | null): vscode.Uri | undefined {
  if (!raw) {
    return undefined;
  }
  const uri = vscode.Uri.parse(raw);
  return uri.scheme === 'https' || uri.scheme === 'http' ? uri : undefined;
}
