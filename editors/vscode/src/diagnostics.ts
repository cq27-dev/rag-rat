// Diagnostics: diverged memories as warnings, stale/gone anchors as info.
// Refreshed alongside the overlays on the same file-memories data lane.
import * as vscode from 'vscode';
import type { FileMemory } from './client';
import { verdictDirectionLabel } from './verdict';

/**
 * Diagnostics belong to a DOCUMENT, not to an editor: they sit in the Problems panel whether or
 * not the tab is showing, which is why every entry here is keyed by document URI and why an
 * invalidation has to reach the documents no editor is currently displaying.
 */
export class LensDiagnostics implements vscode.Disposable {
  private collection = vscode.languages.createDiagnosticCollection('rag-rat-lens');

  apply(document: vscode.TextDocument, memories: FileMemory[]): void {
    const diagnostics: vscode.Diagnostic[] = [];
    for (const m of memories) {
      const line = Math.min(Math.max(0, (m.line ?? 1) - 1), document.lineCount - 1);
      const range = new vscode.Range(line, 0, line, document.lineAt(line).text.length);
      if (m.verdict === 'diverged') {
        const direction = verdictDirectionLabel(m.verdict_direction);
        diagnostics.push(
          new vscode.Diagnostic(
            range,
            `rag-rat memory diverged: ${m.title} (${direction})`,
            vscode.DiagnosticSeverity.Warning,
          ),
        );
      } else if (m.anchor_status === 'stale' || m.anchor_status === 'gone') {
        diagnostics.push(
          new vscode.Diagnostic(
            range,
            `rag-rat memory anchor ${m.anchor_status}: ${m.title}`,
            vscode.DiagnosticSeverity.Information,
          ),
        );
      }
    }
    this.collection.set(document.uri, diagnostics);
  }

  clearUri(uri: vscode.Uri): void {
    this.collection.delete(uri);
  }

  /**
   * Drop every document's entries except `keep` — the documents a refresh is about to rewrite.
   * Anything else is a claim about an index state that no longer exists, and it would otherwise sit
   * in the Problems panel until its tab happens to be selected.
   */
  retainOnly(keep: readonly vscode.Uri[]): void {
    const kept = new Set(keep.map((uri) => uri.toString()));
    const stale: vscode.Uri[] = [];
    this.collection.forEach((uri) => {
      if (!kept.has(uri.toString())) {
        stale.push(uri);
      }
    });
    for (const uri of stale) {
      this.collection.delete(uri);
    }
  }

  clear(): void {
    this.collection.clear();
  }

  dispose(): void {
    this.collection.dispose();
  }
}
