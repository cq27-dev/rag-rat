// In-file overlays: clone-class ranges — one hue per coherent class of the
// clone graph, so siblings light up in the same color; neutral gray for
// regions with no near-clone group. Memory signals live in the CodeLens,
// sidebar, and diagnostics lanes, not in decorations.
import * as vscode from 'vscode';
import type { CloneRegion } from './client';

// Distinct hues, cycled by class_id; enough that adjacent classes rarely collide.
const CLONE_HUES = [280, 200, 340, 160, 20, 100, 240, 300, 0, 60];

export class LensOverlays implements vscode.Disposable {
  private enabled = false;
  private cloneTypes = new Map<string, vscode.TextEditorDecorationType>();

  get isEnabled(): boolean {
    return this.enabled;
  }

  toggle(): boolean {
    this.enabled = !this.enabled;
    if (!this.enabled) {
      for (const editor of vscode.window.visibleTextEditors) {
        this.clear(editor);
      }
    }
    return this.enabled;
  }

  clear(editor: vscode.TextEditor | undefined): void {
    if (!editor) {
      return;
    }
    for (const t of this.cloneTypes.values()) {
      editor.setDecorations(t, []);
    }
  }

  apply(editor: vscode.TextEditor, clones: CloneRegion[], path: string): void {
    if (!this.enabled) {
      return;
    }
    const cloneBuckets = new Map<string, vscode.DecorationOptions[]>();
    for (const region of mergeRegions(editor, clones)) {
      const cls = region.class_id ?? -1;
      const heading = region.class_id != null
        ? `**clone class ${cls}** · near-clone group`
        : '**similar code** (no near-clone group)';
      const hover = new vscode.MarkdownString('', true);
      hover.isTrusted = { enabledCommands: ['rag-rat-lens.openLocation'] };
      hover.supportThemeIcons = true;
      hover.appendMarkdown(
        `${heading}\n\n${region.partners.length} similar region${region.partners.length === 1 ? '' : 's'} · ` +
          `best similarity ${region.max_similarity.toFixed(2)}\n\n`,
      );
      for (const p of region.partners.slice(0, 8)) {
        const where = p.path === path ? 'this file' : p.path;
        const name = p.symbol ? ` · ${p.symbol}` : '';
        const label = `${where}${spanText(p.start_line, p.end_line)}${name} (${p.similarity.toFixed(2)})`;
        const args = encodeURIComponent(JSON.stringify([{ path: p.path, line: p.start_line ?? 1 }]));
        hover.appendMarkdown(
          `- [${escapeMarkdown(label)}](command:rag-rat-lens.openLocation?${args} "open")\n`,
        );
      }
      if (region.partners.length > 8) {
        hover.appendMarkdown(`- … +${region.partners.length - 8} more`);
      }
      const style = cls < 0 ? -1 : cls % CLONE_HUES.length;
      const key = `${style}:${simTier(region.max_similarity)}`;
      const bucket = cloneBuckets.get(key) ?? [];
      bucket.push({
        range: inclusiveLineRange(editor.document, region.start_line, region.end_line),
        hoverMessage: hover,
      });
      cloneBuckets.set(key, bucket);
    }
    for (const [key, t] of this.cloneTypes) {
      if (!cloneBuckets.has(key)) {
        editor.setDecorations(t, []);
      }
    }
    for (const [key, options] of cloneBuckets) {
      const [styleS, tierS] = key.split(':');
      editor.setDecorations(this.cloneTypeFor(Number(styleS), Number(tierS)), options);
    }
  }

  private cloneTypeFor(style: number, tier: number): vscode.TextEditorDecorationType {
    const key = `${style}:${tier}`;
    let t = this.cloneTypes.get(key);
    if (!t) {
      // Strength by similarity tier: exact/near clones pop, weak matches stay faint.
      const alpha = tier === 2 ? 0.18 : tier === 1 ? 0.11 : 0.06;
      if (style < 0) {
        // No tight (>=0.9) clone class: neutral gray 'similar code elsewhere' tint.
        t = vscode.window.createTextEditorDecorationType({
          isWholeLine: true,
          backgroundColor: `hsla(0, 0%, 55%, ${alpha})`,
          overviewRulerColor: 'hsla(0, 0%, 55%, 0.5)',
          overviewRulerLane: vscode.OverviewRulerLane.Left,
        });
      } else {
        const hue = CLONE_HUES[style];
        t = vscode.window.createTextEditorDecorationType({
          isWholeLine: true,
          backgroundColor: `hsla(${hue}, 70%, 55%, ${alpha})`,
          border: tier === 2 ? `1px solid hsla(${hue}, 70%, 55%, 0.5)` : undefined,
          overviewRulerColor: `hsla(${hue}, 70%, 55%, 0.8)`,
          overviewRulerLane: vscode.OverviewRulerLane.Left,
        });
      }
      this.cloneTypes.set(key, t);
    }
    return t;
  }

  dispose(): void {
    for (const t of this.cloneTypes.values()) {
      t.dispose();
    }
  }
}

export function inclusiveLineRange(
  document: vscode.TextDocument,
  startLine: number,
  endLine: number,
): vscode.Range {
  const lastDocumentLine = Math.max(0, document.lineCount - 1);
  const start = Math.min(Math.max(0, startLine - 1), lastDocumentLine);
  const end = Math.min(Math.max(start, endLine - 1), lastDocumentLine);
  return new vscode.Range(start, 0, end, document.lineAt(end).text.length);
}

interface MergedRegion {
  start_line: number;
  end_line: number;
  class_id: number | null;
  max_similarity: number;
  partners: CloneRegion['partners'];
}

// Sub-block edges produce several overlapping candidate spans per area; for
// display, union overlapping line intervals only within the same coherent class.
function mergeRegions(editor: vscode.TextEditor, regions: CloneRegion[]): MergedRegion[] {
  const normalized = regions
    .map((r) => {
      const start = r.start_line != null ? r.start_line : editor.document.positionAt(r.byte_offset).line + 1;
      const end = r.end_line != null ? r.end_line : start;
      return {
        start_line: start,
        end_line: Math.max(start, end),
        class_id: r.class_id,
        max_similarity: r.max_similarity ?? 0,
        partners: r.partners,
      };
    })
    .sort((a, b) => a.start_line - b.start_line || b.max_similarity - a.max_similarity);
  const merged: MergedRegion[] = [];
  for (const r of normalized) {
    const last = merged[merged.length - 1];
    if (
      last &&
      r.class_id != null &&
      r.class_id === last.class_id &&
      r.start_line <= last.end_line
    ) {
      last.end_line = Math.max(last.end_line, r.end_line);
      if (r.max_similarity > last.max_similarity) {
        last.max_similarity = r.max_similarity;
        last.class_id = r.class_id;
      }
      const seen = new Set(last.partners.map((p) => `${p.path}:${p.start_line}`));
      for (const p of r.partners) {
        const key = `${p.path}:${p.start_line}`;
        if (!seen.has(key)) {
          seen.add(key);
          last.partners.push(p);
        }
      }
      last.partners.sort((a, b) => b.similarity - a.similarity);
      last.partners = last.partners.slice(0, 12);
    } else {
      merged.push({ ...r, partners: [...r.partners] });
    }
  }
  return merged;
}

function simTier(sim: number): number {
  return sim >= 0.95 ? 2 : sim >= 0.85 ? 1 : 0;
}

function spanText(start: number | null, end: number | null): string {
  if (start == null) {
    return '';
  }
  return end != null && end !== start ? `:${start}-${end}` : `:${start}`;
}

function escapeMarkdown(value: string): string {
  return value.replace(/[\\`*_{}[\]()<>#+\-.!|]/g, '\\$&');
}
