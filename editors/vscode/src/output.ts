// One output channel for the whole extension: client failures, status
// transitions, poll anomalies. Replaces the silent `catch {}` lanes.
import * as vscode from 'vscode';

const channel = vscode.window.createOutputChannel('rag-rat Lens');

export function logInfo(message: string): void {
  channel.appendLine(`[info] ${message}`);
}

export function logError(context: string, error: unknown): void {
  const detail = error instanceof Error ? `${error.message}` : String(error);
  channel.appendLine(`[error] ${context}: ${detail}`);
}

export function showLog(): void {
  channel.show(true);
}
